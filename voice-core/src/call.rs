use std::path::PathBuf;
use std::sync::{Arc, atomic::AtomicBool};

use anyhow::Context;
use iroh::Endpoint;
use iroh::endpoint::Connection;
use tokio::sync::{broadcast, mpsc};

use crate::audio::{
    AudioDeviceInfo, AudioDeviceList, AudioDeviceSelection, AudioPipeline, EncodedFrame,
    default_device_selection, list_devices,
};
use crate::endpoint::{default_config_dir, init_endpoint};
use crate::error::{Result, VoiceError};
use crate::ticket::{CALL_ALPN, create_ticket, parse_ticket};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallState {
    Idle,
    Listening,
    IncomingCall { from: String },
    Dialing,
    InCall,
    Error,
}

#[derive(Debug, Clone)]
pub enum CallCommand {
    StartListening,
    StopListening,
    Dial { ticket: String },
    RefreshDevices,
    SetInputDevice(String),
    SetOutputDevice(String),
    Accept,
    Reject,
    HangUp,
    SetMute(bool),
}

#[derive(Debug, Clone)]
pub enum CallEvent {
    CallState(CallState),
    Error(String),
    AudioDevices(AudioDeviceInfo),
    DeviceList {
        inputs: Vec<String>,
        outputs: Vec<String>,
        selected_input: Option<String>,
        selected_output: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct CoreConfig {
    pub config_dir: Option<PathBuf>,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self { config_dir: None }
    }
}

#[derive(Debug, Clone)]
pub struct CoreHandle {
    pub command_tx: mpsc::UnboundedSender<CallCommand>,
    pub event_tx: broadcast::Sender<CallEvent>,
    pub node_id: String,
    pub ticket: String,
}

impl CoreHandle {
    pub fn start(config: CoreConfig) -> Result<Self> {
        let config_dir = match config.config_dir {
            Some(path) => path,
            None => default_config_dir()?,
        };

        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, _) = broadcast::channel(64);
        let (init_tx, init_rx) = std::sync::mpsc::channel();

        let event_tx_clone = event_tx.clone();

        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build();

            let runtime = match runtime {
                Ok(runtime) => runtime,
                Err(err) => {
                    let _ = init_tx.send(Err(VoiceError::Other(anyhow::Error::from(err))));
                    return;
                }
            };

            let init_tx_async = init_tx.clone();
            let init_result = runtime.block_on(async move {
                let endpoint_info = init_endpoint(&config_dir).await?;
                let ticket = create_ticket(&endpoint_info.endpoint).await?;

                let _ = init_tx_async.send(Ok((endpoint_info.node_id.clone(), ticket.clone())));

                let mut actor = CallActor::new(
                    endpoint_info.endpoint,
                    endpoint_info.node_id,
                    command_rx,
                    event_tx_clone,
                );

                actor.run().await;
                Ok::<(), VoiceError>(())
            });

            if let Err(err) = init_result {
                let _ = init_tx.send(Err(err));
            }
        });

        let (node_id, ticket) = init_rx.recv().map_err(|_| VoiceError::ChannelClosed)??;

        Ok(Self {
            command_tx,
            event_tx,
            node_id,
            ticket,
        })
    }
}

struct CallActor {
    endpoint: Endpoint,
    node_id: String,
    command_rx: mpsc::UnboundedReceiver<CallCommand>,
    event_tx: broadcast::Sender<CallEvent>,
    state: CallState,
    listen_task: Option<tokio::task::JoinHandle<()>>,
    listening_requested: bool,
    pending_incoming: Option<Connection>,
    active_call: Option<ActiveCall>,
    mute_handle: Option<Arc<AtomicBool>>,
    internal_tx: Option<mpsc::UnboundedSender<InternalEvent>>,
    device_list: AudioDeviceList,
    audio_selection: AudioDeviceSelection,
}

struct ActiveCall {
    _connection: Connection,
    _audio: AudioPipeline,
    sender_task: tokio::task::JoinHandle<()>,
    receiver_task: tokio::task::JoinHandle<()>,
}

enum InternalEvent {
    Incoming(Connection),
    DialResult(Result<Connection>),
    CallEnded(String),
}

impl CallActor {
    fn new(
        endpoint: Endpoint,
        node_id: String,
        command_rx: mpsc::UnboundedReceiver<CallCommand>,
        event_tx: broadcast::Sender<CallEvent>,
    ) -> Self {
        Self {
            endpoint,
            node_id,
            command_rx,
            event_tx,
            state: CallState::Idle,
            listen_task: None,
            listening_requested: false,
            pending_incoming: None,
            active_call: None,
            mute_handle: None,
            internal_tx: None,
            device_list: AudioDeviceList::default(),
            audio_selection: default_device_selection(),
        }
    }

    async fn run(&mut self) {
        tracing::info!(node_id = %self.node_id, "voice core started");
        let (internal_tx, mut internal_rx) = mpsc::unbounded_channel();
        self.internal_tx = Some(internal_tx.clone());

        self.emit_state();
        if let Err(err) = self.refresh_devices() {
            self.emit_error(err);
        }

        loop {
            tokio::select! {
                Some(command) = self.command_rx.recv() => {
                    if let Err(err) = self.handle_command(command).await {
                        self.emit_error(err);
                    }
                }
                Some(event) = internal_rx.recv() => {
                    if let Err(err) = self.handle_internal(event).await {
                        self.emit_error(err);
                    }
                }
                else => {
                    break;
                }
            }
        }
    }

    async fn handle_command(&mut self, command: CallCommand) -> Result<()> {
        match command {
            CallCommand::StartListening => {
                self.listening_requested = true;
                if self.active_call.is_none() {
                    self.start_listening();
                }
            }
            CallCommand::StopListening => {
                self.listening_requested = false;
                self.stop_listening();
                if self.active_call.is_none() {
                    self.set_state(CallState::Idle);
                }
            }
            CallCommand::Dial { ticket } => {
                if self.active_call.is_some() || matches!(self.state, CallState::Dialing) {
                    return Err(VoiceError::InvalidState("Already in a call".to_string()));
                }
                let node_addr = parse_ticket(&ticket)?;
                self.set_state(CallState::Dialing);
                let endpoint = self.endpoint.clone();
                let internal_tx = self.internal_tx.clone().ok_or(VoiceError::ChannelClosed)?;
                tokio::spawn(async move {
                    let result = endpoint
                        .connect(node_addr, CALL_ALPN)
                        .await
                        .context("Dial failed")
                        .map_err(VoiceError::Other);
                    let _ = internal_tx.send(InternalEvent::DialResult(result));
                });
            }
            CallCommand::RefreshDevices => {
                self.refresh_devices()?;
            }
            CallCommand::SetInputDevice(name) => {
                self.audio_selection.input = Some(name);
                self.emit_device_list();
            }
            CallCommand::SetOutputDevice(name) => {
                self.audio_selection.output = Some(name);
                self.emit_device_list();
            }
            CallCommand::Accept => {
                if let Some(connection) = self.pending_incoming.take() {
                    self.start_call(connection).await?;
                } else {
                    return Err(VoiceError::InvalidState(
                        "No incoming call to accept".to_string(),
                    ));
                }
            }
            CallCommand::Reject => {
                if let Some(connection) = self.pending_incoming.take() {
                    drop(connection);
                    if self.listening_requested {
                        self.start_listening();
                    }
                    self.set_state(CallState::Idle);
                }
            }
            CallCommand::HangUp => {
                self.end_call("Call ended by user".to_string()).await;
            }
            CallCommand::SetMute(enabled) => {
                if let Some(mute) = &self.mute_handle {
                    mute.store(enabled, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
        Ok(())
    }

    async fn handle_internal(&mut self, event: InternalEvent) -> Result<()> {
        match event {
            InternalEvent::Incoming(connection) => {
                if self.active_call.is_some() || self.pending_incoming.is_some() {
                    drop(connection);
                    return Ok(());
                }
                let remote = connection
                    .remote_node_id()
                    .map(|id| id.to_string())
                    .unwrap_or_else(|_| "Unknown".to_string());
                self.pending_incoming = Some(connection);
                self.set_state(CallState::IncomingCall { from: remote });
            }
            InternalEvent::DialResult(result) => match result {
                Ok(connection) => {
                    self.start_call(connection).await?;
                }
                Err(err) => {
                    self.emit_error(err);
                    self.set_state(CallState::Idle);
                }
            },
            InternalEvent::CallEnded(reason) => {
                self.end_call(reason).await;
            }
        }
        Ok(())
    }

    fn start_listening(&mut self) {
        if self.listen_task.is_some() {
            return;
        }

        let endpoint = self.endpoint.clone();
        let internal_tx = match self.internal_tx.clone() {
            Some(tx) => tx,
            None => return,
        };
        let handle = tokio::spawn(async move {
            loop {
                let incoming = match endpoint.accept().await {
                    Some(incoming) => incoming,
                    None => break,
                };

                let connection = match incoming.await {
                    Ok(connection) => connection,
                    Err(err) => {
                        tracing::warn!("accept failed: {err}");
                        continue;
                    }
                };

                let _ = internal_tx.send(InternalEvent::Incoming(connection));
            }
        });

        self.listen_task = Some(handle);
        self.set_state(CallState::Listening);
    }

    fn stop_listening(&mut self) {
        if let Some(handle) = &self.listen_task {
            handle.abort();
        }
        self.listen_task = None;
    }

    async fn start_call(&mut self, connection: Connection) -> Result<()> {
        if let Some(handle) = &self.listen_task {
            handle.abort();
        }
        self.listen_task = None;

        let mut audio = AudioPipeline::start_with_devices(&self.audio_selection)?;
        let device_info = audio.device_info();
        let outgoing = audio.take_outgoing()?;
        let incoming = audio.incoming_sender();
        let mute_handle = audio.mute_handle();

        let (mut roq_sender, mut roq_receiver) =
            roq_transport::AudioTransport::new(connection.clone()).await?;

        let sender_internal = self.internal_tx.clone().ok_or(VoiceError::ChannelClosed)?;
        let sender_task = tokio::spawn(async move {
            if let Err(err) = roq_sender.send_loop(outgoing).await {
                let _ = sender_internal.send(InternalEvent::CallEnded(err.to_string()));
            }
        });

        let receiver_internal = self.internal_tx.clone().ok_or(VoiceError::ChannelClosed)?;
        let receiver_task = tokio::spawn(async move {
            if let Err(err) = roq_receiver.recv_loop(incoming).await {
                let _ = receiver_internal.send(InternalEvent::CallEnded(err.to_string()));
            }
        });

        self.active_call = Some(ActiveCall {
            _connection: connection,
            _audio: audio,
            sender_task,
            receiver_task,
        });
        self.mute_handle = Some(mute_handle);
        self.audio_selection.input = Some(device_info.input_name.clone());
        self.audio_selection.output = Some(device_info.output_name.clone());
        self.emit_device_list();
        let _ = self.event_tx.send(CallEvent::AudioDevices(device_info));
        self.set_state(CallState::InCall);
        Ok(())
    }

    async fn end_call(&mut self, reason: String) {
        if let Some(active) = self.active_call.take() {
            active.sender_task.abort();
            active.receiver_task.abort();
            drop(active);
        }
        self.mute_handle = None;
        tracing::info!("call ended: {reason}");
        if self.listening_requested {
            self.start_listening();
        }
        self.set_state(if self.listening_requested {
            CallState::Listening
        } else {
            CallState::Idle
        });
    }

    fn set_state(&mut self, state: CallState) {
        self.state = state.clone();
        self.emit_state();
    }

    fn emit_state(&self) {
        let _ = self.event_tx.send(CallEvent::CallState(self.state.clone()));
    }

    fn emit_error(&mut self, err: VoiceError) {
        tracing::error!("core error: {err}");
        let _ = self.event_tx.send(CallEvent::Error(err.to_string()));
        self.state = CallState::Error;
        let _ = self.event_tx.send(CallEvent::CallState(CallState::Error));
    }

    fn emit_device_list(&self) {
        let _ = self.event_tx.send(CallEvent::DeviceList {
            inputs: self.device_list.inputs.clone(),
            outputs: self.device_list.outputs.clone(),
            selected_input: self.audio_selection.input.clone(),
            selected_output: self.audio_selection.output.clone(),
        });
    }

    fn refresh_devices(&mut self) -> Result<()> {
        let devices = list_devices()?;
        let defaults = default_device_selection();

        let input_valid = self
            .audio_selection
            .input
            .as_ref()
            .map(|name| devices.inputs.iter().any(|item| item == name))
            .unwrap_or(false);
        let output_valid = self
            .audio_selection
            .output
            .as_ref()
            .map(|name| devices.outputs.iter().any(|item| item == name))
            .unwrap_or(false);

        if !input_valid {
            self.audio_selection.input = defaults.input;
        }
        if !output_valid {
            self.audio_selection.output = defaults.output;
        }

        self.device_list = devices;
        self.emit_device_list();
        Ok(())
    }
}

mod roq_transport {
    use super::*;
    use bytes::Bytes;
    use iroh_roq::rtp::header::Header;
    use iroh_roq::{RtpPacket, Session, VarInt};
    use rand_core::RngCore;

    const AUDIO_FLOW_ID: u32 = 1;
    const RTP_PAYLOAD_TYPE_OPUS: u8 = 111;

    pub struct AudioSender {
        flow: iroh_roq::SendFlow,
        sequence: u16,
        ssrc: u32,
    }

    pub struct AudioReceiver {
        flow: iroh_roq::ReceiveFlow,
    }

    pub struct AudioTransport;

    impl AudioTransport {
        pub async fn new(connection: Connection) -> Result<(AudioSender, AudioReceiver)> {
            let session = Session::new(connection);
            let flow_id = VarInt::from_u32(AUDIO_FLOW_ID);

            let send_flow = session
                .new_send_flow(flow_id)
                .await
                .context("Failed to create RoQ send flow")
                .map_err(VoiceError::Other)?;
            let recv_flow = session
                .new_receive_flow(flow_id)
                .await
                .context("Failed to create RoQ receive flow")
                .map_err(VoiceError::Other)?;

            let mut rng = rand_core::OsRng;
            let ssrc = rng.next_u32();
            let sequence = (rng.next_u32() & 0xFFFF) as u16;

            Ok((
                AudioSender {
                    flow: send_flow,
                    sequence,
                    ssrc,
                },
                AudioReceiver { flow: recv_flow },
            ))
        }
    }

    impl AudioSender {
        pub async fn send_loop(
            &mut self,
            mut outgoing: mpsc::Receiver<EncodedFrame>,
        ) -> Result<()> {
            while let Some(frame) = outgoing.recv().await {
                let packet = RtpPacket {
                    header: Header {
                        version: 2,
                        payload_type: RTP_PAYLOAD_TYPE_OPUS,
                        sequence_number: self.sequence,
                        timestamp: frame.timestamp,
                        ssrc: self.ssrc,
                        ..Header::default()
                    },
                    payload: Bytes::from(frame.payload),
                };
                self.sequence = self.sequence.wrapping_add(1);
                self.flow
                    .send_rtp(&packet)
                    .context("Failed to send RTP")
                    .map_err(VoiceError::Other)?;
            }
            Ok(())
        }
    }

    impl AudioReceiver {
        pub async fn recv_loop(&mut self, incoming: mpsc::Sender<EncodedFrame>) -> Result<()> {
            loop {
                let packet = self
                    .flow
                    .read_rtp()
                    .await
                    .context("Failed to receive RTP")
                    .map_err(VoiceError::Other)?;
                let encoded = EncodedFrame {
                    payload: packet.payload.to_vec(),
                    timestamp: packet.header.timestamp,
                };
                if incoming.send(encoded).await.is_err() {
                    break;
                }
            }
            Ok(())
        }
    }
}
