use std::hash::Hash;

use iced::advanced::subscription::{from_recipe, EventStream, Hasher, Recipe};
use iced::futures::stream::BoxStream;
use iced::futures::StreamExt;
use iced::alignment::{Horizontal, Vertical};
use iced::task::Task;
use iced::widget::image::Handle as ImageHandle;
use iced::widget::{button, column, container, row, text, text_input, Image};
use iced::{Element, Length, Subscription, Theme};
use image::ImageFormat;
use qrcode::{Color, QrCode};
use tokio_stream::wrappers::BroadcastStream;

use voice_core::{CallCommand, CallEvent, CallState, CoreConfig, CoreHandle};

#[derive(Debug, Clone)]
pub enum Message {
    CopyTicket,
    RemoteTicketChanged(String),
    ToggleListening,
    Call,
    Accept,
    Reject,
    HangUp,
    ToggleMute,
    CoreEvent(CallEvent),
}

pub struct VoiceApp {
    core: Option<CoreHandle>,
    node_id: String,
    local_ticket: String,
    remote_ticket: String,
    call_state: CallState,
    last_error: Option<String>,
    muted: bool,
    audio_info: Option<String>,
    qr_handle: Option<ImageHandle>,
}

impl VoiceApp {
    pub fn run() -> iced::Result {
        iced::application(VoiceApp::new, VoiceApp::update, VoiceApp::view)
            .title(VoiceApp::title)
            .subscription(VoiceApp::subscription)
            .theme(Theme::Light)
            .run()
    }

    fn new() -> (Self, Task<Message>) {
        let mut last_error = None;
        let mut node_id = String::new();
        let mut local_ticket = String::new();
        let mut core = None;

        match CoreHandle::start(CoreConfig::default()) {
            Ok(handle) => {
                node_id = handle.node_id.clone();
                local_ticket = handle.ticket.clone();
                core = Some(handle);
            }
            Err(err) => {
                last_error = Some(err.to_string());
            }
        }

        let qr_handle = Self::build_qr(&local_ticket);

        (
            Self {
                core,
                node_id,
                local_ticket,
                remote_ticket: String::new(),
                call_state: CallState::Idle,
                last_error,
                muted: false,
                audio_info: None,
                qr_handle,
            },
            Task::none(),
        )
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::CopyTicket => {
                return iced::clipboard::write(self.local_ticket.clone());
            }
            Message::RemoteTicketChanged(value) => {
                self.remote_ticket = value;
            }
            Message::ToggleListening => {
                if matches!(self.call_state, CallState::Listening) {
                    self.send_command(CallCommand::StopListening);
                } else {
                    self.send_command(CallCommand::StartListening);
                }
            }
            Message::Call => {
                if !self.remote_ticket.trim().is_empty() {
                    self.send_command(CallCommand::Dial {
                        ticket: self.remote_ticket.trim().to_string(),
                    });
                }
            }
            Message::Accept => {
                self.send_command(CallCommand::Accept);
            }
            Message::Reject => {
                self.send_command(CallCommand::Reject);
            }
            Message::HangUp => {
                self.send_command(CallCommand::HangUp);
            }
            Message::ToggleMute => {
                self.muted = !self.muted;
                self.send_command(CallCommand::SetMute(self.muted));
            }
            Message::CoreEvent(event) => {
                self.apply_event(event);
            }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let header = column![
            text("Accord Voice").size(32),
            text(format!(
                "Node ID: {}",
                if self.node_id.is_empty() {
                    "Unavailable"
                } else {
                    &self.node_id
                }
            ))
            .size(14),
        ]
        .spacing(6);

        let ticket_row = row![
            text("Local ticket:").size(14),
            text(self.local_ticket.clone()).size(14).width(Length::Fill),
            button("Copy ticket").on_press(Message::CopyTicket),
        ]
        .spacing(10)
        .align_y(Vertical::Center);

        let remote_input = text_input("Paste remote ticket", &self.remote_ticket)
            .on_input(Message::RemoteTicketChanged)
            .padding(8)
            .size(14);

        let call_button = if self.remote_ticket.trim().is_empty() {
            button("Call")
        } else {
            button("Call").on_press(Message::Call)
        };

        let listening_button = if matches!(self.call_state, CallState::Listening) {
            button("Stop listening").on_press(Message::ToggleListening)
        } else {
            button("Start listening").on_press(Message::ToggleListening)
        };

        let accept_button = if matches!(self.call_state, CallState::IncomingCall { .. }) {
            button("Accept").on_press(Message::Accept)
        } else {
            button("Accept")
        };

        let reject_button = if matches!(self.call_state, CallState::IncomingCall { .. }) {
            button("Reject").on_press(Message::Reject)
        } else {
            button("Reject")
        };

        let hangup_button = if matches!(self.call_state, CallState::InCall | CallState::Dialing) {
            button("Hang up").on_press(Message::HangUp)
        } else {
            button("Hang up")
        };

        let mute_label = if self.muted { "Unmute" } else { "Mute" };
        let mute_button = if matches!(self.call_state, CallState::InCall) {
            button(mute_label).on_press(Message::ToggleMute)
        } else {
            button(mute_label)
        };

        let actions = row![
            listening_button,
            call_button,
            accept_button,
            reject_button,
            hangup_button,
            mute_button,
        ]
        .spacing(10)
        .align_y(Vertical::Center);

        let status = column![
            text(format!("State: {}", self.call_state_label())),
            text(
                self.audio_info
                    .clone()
                    .unwrap_or_else(|| "Audio: default devices".to_string())
            )
            .size(14),
        ]
        .spacing(6);

        let error_text = self
            .last_error
            .as_ref()
            .map(|err| text(format!("Last error: {err}")).size(14))
            .unwrap_or_else(|| text("Last error: none").size(14));

        let qr_block = if let Some(handle) = &self.qr_handle {
            column![
                text("Ticket QR"),
                Image::new(handle.clone()).width(Length::Fixed(180.0))
            ]
            .spacing(6)
            .align_x(Horizontal::Center)
        } else {
            column![text("Ticket QR unavailable")].spacing(6)
        };

        let content = column![
            header,
            ticket_row,
            text("Remote ticket:").size(14),
            remote_input,
            actions,
            status,
            error_text,
            qr_block,
        ]
        .spacing(16)
        .padding(20)
        .max_width(900);

        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
    }

    fn subscription(&self) -> Subscription<Message> {
        match &self.core {
            Some(core) => from_recipe(CoreEventSubscription {
                sender: core.event_tx.clone(),
            })
            .map(Message::CoreEvent),
            None => Subscription::none(),
        }
    }

    fn title(&self) -> String {
        "Accord Voice".to_string()
    }

    fn send_command(&self, command: CallCommand) {
        if let Some(core) = &self.core {
            let _ = core.command_tx.send(command);
        }
    }

    fn apply_event(&mut self, event: CallEvent) {
        match event {
            CallEvent::CallState(state) => {
                self.call_state = state;
                if !matches!(self.call_state, CallState::InCall) {
                    self.muted = false;
                }
            }
            CallEvent::Error(error) => {
                self.last_error = Some(error);
            }
            CallEvent::AudioDevices(info) => {
                self.audio_info = Some(format!(
                    "Input: {} | Output: {} | {} Hz",
                    info.input_name, info.output_name, info.sample_rate
                ));
            }
        }
    }

    fn call_state_label(&self) -> String {
        match &self.call_state {
            CallState::Idle => "Idle".to_string(),
            CallState::Listening => "Listening for calls".to_string(),
            CallState::IncomingCall { from } => format!("Incoming call from {from}"),
            CallState::Dialing => "Dialing".to_string(),
            CallState::InCall => "In call".to_string(),
            CallState::Error => "Error".to_string(),
        }
    }

    fn build_qr(ticket: &str) -> Option<ImageHandle> {
        if ticket.trim().is_empty() {
            return None;
        }

        let code = QrCode::new(ticket.as_bytes()).ok()?;
        let colors = code.to_colors();
        let side = code.width() as u32;
        let quiet = 4u32;
        let target = 240u32;
        let mut scale = (target / (side + quiet * 2)).max(1);
        if (side + quiet * 2) * scale < target {
            scale += 1;
        }
        let size = (side + quiet * 2) * scale;

        let mut image = image::ImageBuffer::from_pixel(size, size, image::Luma([255u8]));

        for y in 0..side {
            for x in 0..side {
                let idx = (y as usize * side as usize) + x as usize;
                if colors[idx] == Color::Dark {
                    let start_x = (x + quiet) * scale;
                    let start_y = (y + quiet) * scale;
                    for dy in 0..scale {
                        for dx in 0..scale {
                            image.put_pixel(start_x + dx, start_y + dy, image::Luma([0u8]));
                        }
                    }
                }
            }
        }

        let mut png = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut png);
        let dyn_image = image::DynamicImage::ImageLuma8(image);
        dyn_image.write_to(&mut cursor, ImageFormat::Png).ok()?;
        Some(ImageHandle::from_bytes(png))
    }
}

struct CoreEventSubscription {
    sender: tokio::sync::broadcast::Sender<CallEvent>,
}

impl Recipe for CoreEventSubscription {
    type Output = CallEvent;

    fn hash(&self, state: &mut Hasher) {
        "core-events".hash(state);
    }

    fn stream(self: Box<Self>, _input: EventStream) -> BoxStream<'static, Self::Output> {
        let receiver = self.sender.subscribe();
        BroadcastStream::new(receiver)
            .filter_map(|event| async move { event.ok() })
            .boxed()
    }
}
