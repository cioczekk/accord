use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use anyhow::Context;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SampleFormat, StreamConfig, SupportedStreamConfigRange};
use dasp_sample::{FromSample, ToSample};
use ringbuf::{HeapConsumer, HeapProducer, HeapRb};
use tokio::sync::mpsc;

use crate::error::{Result, VoiceError};

const OPUS_SAMPLE_RATES: [u32; 5] = [48_000, 24_000, 16_000, 12_000, 8_000];
const FRAME_TIME_MS: u32 = 20;
const OUTPUT_JITTER_MS: u32 = 120;
const CHANNELS_MONO: u16 = 1;

#[derive(Clone, Debug, Default)]
pub struct AudioDeviceSelection {
    pub input: Option<String>,
    pub output: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct AudioDeviceList {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct AudioDeviceInfo {
    pub input_name: String,
    pub output_name: String,
    pub sample_rate: u32,
    pub channels: u16,
}

#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub payload: Vec<u8>,
    pub timestamp: u32,
}

pub struct AudioPipeline {
    outgoing_rx: Option<mpsc::Receiver<EncodedFrame>>,
    incoming_tx: mpsc::Sender<EncodedFrame>,
    mute: Arc<AtomicBool>,
    _input_stream: cpal::Stream,
    _output_stream: cpal::Stream,
    device_info: AudioDeviceInfo,
    encode_task: tokio::task::JoinHandle<()>,
    decode_task: tokio::task::JoinHandle<()>,
}

impl AudioPipeline {
    pub fn start() -> Result<Self> {
        Self::start_with_devices(&AudioDeviceSelection::default())
    }

    pub fn start_with_devices(selection: &AudioDeviceSelection) -> Result<Self> {
        let host = cpal::default_host();
        let input_device = resolve_input_device(&host, selection.input.as_deref())?;
        let output_device = resolve_output_device(&host, selection.output.as_deref())?;

        let (input_config, input_format) = select_input_stream_config(&input_device)?;
        let (output_config, output_format) = select_output_stream_config(&output_device)?;

        if input_config.sample_rate.0 != output_config.sample_rate.0 {
            return Err(VoiceError::Audio(
                "Input/output sample rates must match in this build".to_string(),
            ));
        }

        let sample_rate = input_config.sample_rate.0;
        if !OPUS_SAMPLE_RATES.contains(&sample_rate) {
            return Err(VoiceError::Audio(format!(
                "Unsupported sample rate for Opus: {sample_rate}"
            )));
        }

        let input_name = input_device.name().unwrap_or_else(|_| "Input".to_string());
        let output_name = output_device
            .name()
            .unwrap_or_else(|_| "Output".to_string());

        let (input_stream, input_consumer) =
            build_input_stream(&input_device, &input_config, input_format, CHANNELS_MONO)?;

        let (output_stream, output_producer, output_level) =
            build_output_stream(&output_device, &output_config, output_format, CHANNELS_MONO)?;

        let frame_size = (sample_rate / (1000 / FRAME_TIME_MS)) as usize;
        let (outgoing_tx, outgoing_rx) = mpsc::channel(128);
        let (incoming_tx, incoming_rx) = mpsc::channel(128);
        let mute = Arc::new(AtomicBool::new(false));

        let encode_mute = Arc::clone(&mute);
        let encode_task = tokio::spawn(async move {
            if let Err(err) = capture_and_encode(
                input_consumer,
                outgoing_tx,
                encode_mute,
                sample_rate,
                frame_size,
            )
            .await
            {
                tracing::error!("audio encode task ended: {err}");
            }
        });

        let decode_task = tokio::spawn(async move {
            if let Err(err) =
                decode_and_play(incoming_rx, output_producer, output_level, sample_rate).await
            {
                tracing::error!("audio decode task ended: {err}");
            }
        });

        input_stream
            .play()
            .context("Failed to start input stream")
            .map_err(VoiceError::Other)?;
        output_stream
            .play()
            .context("Failed to start output stream")
            .map_err(VoiceError::Other)?;

        Ok(Self {
            outgoing_rx: Some(outgoing_rx),
            incoming_tx,
            mute,
            _input_stream: input_stream,
            _output_stream: output_stream,
            device_info: AudioDeviceInfo {
                input_name,
                output_name,
                sample_rate,
                channels: CHANNELS_MONO,
            },
            encode_task,
            decode_task,
        })
    }

    pub fn take_outgoing(&mut self) -> Result<mpsc::Receiver<EncodedFrame>> {
        self.outgoing_rx.take().ok_or(VoiceError::InvalidState(
            "Outgoing audio already taken".to_string(),
        ))
    }

    pub fn incoming_sender(&self) -> mpsc::Sender<EncodedFrame> {
        self.incoming_tx.clone()
    }

    pub fn mute_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.mute)
    }

    pub fn device_info(&self) -> AudioDeviceInfo {
        self.device_info.clone()
    }
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        self.encode_task.abort();
        self.decode_task.abort();
    }
}

fn select_input_stream_config(device: &cpal::Device) -> Result<(StreamConfig, SampleFormat)> {
    let configs = device
        .supported_input_configs()
        .context("No supported input configs")
        .map_err(VoiceError::Other)?
        .collect::<Vec<_>>();
    select_best_config(configs)
}

fn select_output_stream_config(device: &cpal::Device) -> Result<(StreamConfig, SampleFormat)> {
    let configs = device
        .supported_output_configs()
        .context("No supported output configs")
        .map_err(VoiceError::Other)?
        .collect::<Vec<_>>();
    select_best_config(configs)
}

fn select_best_config(
    configs: Vec<SupportedStreamConfigRange>,
) -> Result<(StreamConfig, SampleFormat)> {
    let mut best: Option<(StreamConfig, SampleFormat, u32)> = None;
    for config_range in configs {
        let channels = config_range.channels();
        if channels == 0 {
            continue;
        }
        for rate in OPUS_SAMPLE_RATES {
            if rate < config_range.min_sample_rate().0 || rate > config_range.max_sample_rate().0 {
                continue;
            }
            let config = config_range.with_sample_rate(cpal::SampleRate(rate));
            let format = config.sample_format();
            let candidate = (config.config(), format, rate);
            let replace = match &best {
                None => true,
                Some((_, best_format, best_rate)) => {
                    rate > *best_rate
                        || (rate == *best_rate
                            && format == SampleFormat::F32
                            && *best_format != SampleFormat::F32)
                }
            };
            if replace {
                best = Some(candidate);
            }
        }
    }

    best.map(|(config, format, _)| (config, format))
        .ok_or_else(|| VoiceError::Audio("No compatible audio configuration".to_string()))
}

fn build_input_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    sample_format: SampleFormat,
    target_channels: u16,
) -> Result<(cpal::Stream, HeapConsumer<i16>)> {
    let capacity = config.sample_rate.0 as usize * 2;
    let (mut producer, consumer) = HeapRb::<i16>::new(capacity).split();
    let channels = config.channels as usize;

    let err_fn = |err| {
        tracing::error!("input stream error: {err}");
    };

    let stream = match sample_format {
        SampleFormat::F32 => device.build_input_stream(
            config,
            move |data: &[f32], _| {
                push_input_samples(data, channels, target_channels, &mut producer)
            },
            err_fn,
            None,
        ),
        SampleFormat::I16 => device.build_input_stream(
            config,
            move |data: &[i16], _| {
                push_input_samples(data, channels, target_channels, &mut producer)
            },
            err_fn,
            None,
        ),
        SampleFormat::U16 => device.build_input_stream(
            config,
            move |data: &[u16], _| {
                push_input_samples(data, channels, target_channels, &mut producer)
            },
            err_fn,
            None,
        ),
        _ => {
            return Err(VoiceError::Audio(
                "Unsupported input sample format".to_string(),
            ));
        }
    }
    .context("Failed to build input stream")
    .map_err(VoiceError::Other)?;

    Ok((stream, consumer))
}

fn push_input_samples<T>(
    data: &[T],
    input_channels: usize,
    target_channels: u16,
    producer: &mut HeapProducer<i16>,
) where
    T: Sample + ToSample<f32>,
{
    let target_channels = target_channels as usize;
    if input_channels == 0 || target_channels == 0 {
        return;
    }

    for frame in data.chunks(input_channels) {
        let mut sum = 0.0f32;
        for sample in frame {
            let sample_f32: f32 = (*sample).to_sample::<f32>();
            sum += sample_f32;
        }
        let mono = (sum / input_channels as f32).clamp(-1.0, 1.0);
        let sample_i16 = (mono * i16::MAX as f32) as i16;

        for _ in 0..target_channels {
            let _ = producer.push(sample_i16);
        }
    }
}

fn build_output_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    sample_format: SampleFormat,
    target_channels: u16,
) -> Result<(cpal::Stream, HeapProducer<i16>, Arc<AtomicUsize>)> {
    let capacity = config.sample_rate.0 as usize * 2;
    let (producer, mut consumer) = HeapRb::<i16>::new(capacity).split();
    let available = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(AtomicBool::new(false));
    let channels = config.channels as usize;
    let min_samples = ((config.sample_rate.0 as u64 * OUTPUT_JITTER_MS as u64) / 1000) as usize;

    let err_fn = |err| {
        tracing::error!("output stream error: {err}");
    };

    let stream = match sample_format {
        SampleFormat::F32 => {
            let available = Arc::clone(&available);
            let started = Arc::clone(&started);
            device.build_output_stream(
                config,
                move |data: &mut [f32], _| {
                    render_output_samples(
                        data,
                        channels,
                        target_channels,
                        &mut consumer,
                        &available,
                        &started,
                        min_samples,
                    )
                },
                err_fn,
                None,
            )
        }
        SampleFormat::I16 => {
            let available = Arc::clone(&available);
            let started = Arc::clone(&started);
            device.build_output_stream(
                config,
                move |data: &mut [i16], _| {
                    render_output_samples(
                        data,
                        channels,
                        target_channels,
                        &mut consumer,
                        &available,
                        &started,
                        min_samples,
                    )
                },
                err_fn,
                None,
            )
        }
        SampleFormat::U16 => {
            let available = Arc::clone(&available);
            let started = Arc::clone(&started);
            device.build_output_stream(
                config,
                move |data: &mut [u16], _| {
                    render_output_samples(
                        data,
                        channels,
                        target_channels,
                        &mut consumer,
                        &available,
                        &started,
                        min_samples,
                    )
                },
                err_fn,
                None,
            )
        }
        _ => {
            return Err(VoiceError::Audio(
                "Unsupported output sample format".to_string(),
            ));
        }
    }
    .context("Failed to build output stream")
    .map_err(VoiceError::Other)?;

    Ok((stream, producer, available))
}

fn render_output_samples<T>(
    data: &mut [T],
    output_channels: usize,
    target_channels: u16,
    consumer: &mut HeapConsumer<i16>,
    available: &Arc<AtomicUsize>,
    started: &Arc<AtomicBool>,
    min_samples: usize,
) where
    T: Sample + FromSample<f32>,
{
    let target_channels = target_channels as usize;
    if output_channels == 0 || target_channels == 0 {
        return;
    }

    if !started.load(Ordering::Relaxed) && available.load(Ordering::Relaxed) >= min_samples {
        started.store(true, Ordering::Relaxed);
    }

    let playing = started.load(Ordering::Relaxed);

    for frame in data.chunks_mut(output_channels) {
        let sample_i16 = if playing {
            if let Some(sample) = consumer.pop() {
                available.fetch_sub(1, Ordering::Relaxed);
                sample
            } else {
                0
            }
        } else {
            0
        };

        let sample_f32 = sample_i16 as f32 / i16::MAX as f32;
        let converted = T::from_sample(sample_f32);
        for out in frame.iter_mut() {
            *out = converted;
        }
    }
}

pub fn list_devices() -> Result<AudioDeviceList> {
    let host = cpal::default_host();
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();

    let input_devices = host
        .input_devices()
        .context("Failed to enumerate input devices")
        .map_err(VoiceError::Other)?;
    for device in input_devices {
        if let Ok(name) = device.name() {
            inputs.push(name);
        }
    }

    let output_devices = host
        .output_devices()
        .context("Failed to enumerate output devices")
        .map_err(VoiceError::Other)?;
    for device in output_devices {
        if let Ok(name) = device.name() {
            outputs.push(name);
        }
    }

    Ok(AudioDeviceList { inputs, outputs })
}

pub fn default_device_selection() -> AudioDeviceSelection {
    let host = cpal::default_host();
    let input = host
        .default_input_device()
        .and_then(|device| device.name().ok());
    let output = host
        .default_output_device()
        .and_then(|device| device.name().ok());

    AudioDeviceSelection { input, output }
}

fn resolve_input_device(host: &cpal::Host, name: Option<&str>) -> Result<cpal::Device> {
    if let Some(name) = name {
        if let Ok(devices) = host.input_devices() {
            for device in devices {
                if device.name().ok().as_deref() == Some(name) {
                    return Ok(device);
                }
            }
        }
        tracing::warn!(
            requested = name,
            "input device not found, falling back to default"
        );
    }

    host.default_input_device()
        .ok_or_else(|| VoiceError::Audio("No input device available".to_string()))
}

fn resolve_output_device(host: &cpal::Host, name: Option<&str>) -> Result<cpal::Device> {
    if let Some(name) = name {
        if let Ok(devices) = host.output_devices() {
            for device in devices {
                if device.name().ok().as_deref() == Some(name) {
                    return Ok(device);
                }
            }
        }
        tracing::warn!(
            requested = name,
            "output device not found, falling back to default"
        );
    }

    host.default_output_device()
        .ok_or_else(|| VoiceError::Audio("No output device available".to_string()))
}

async fn capture_and_encode(
    mut consumer: HeapConsumer<i16>,
    outgoing: mpsc::Sender<EncodedFrame>,
    mute: Arc<AtomicBool>,
    sample_rate: u32,
    frame_size: usize,
) -> Result<()> {
    let mut encoder =
        opus::Encoder::new(sample_rate, opus::Channels::Mono, opus::Application::Voip)?;
    let mut pcm = vec![0i16; frame_size];
    let mut timestamp: u32 = 0;

    loop {
        let mut filled = 0;
        while filled < frame_size {
            if let Some(sample) = consumer.pop() {
                pcm[filled] = sample;
                filled += 1;
            } else {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }

        if mute.load(Ordering::Relaxed) {
            pcm.fill(0);
        }

        let mut encoded = vec![0u8; 4000];
        let len = encoder.encode(&pcm, &mut encoded)?;
        encoded.truncate(len);

        if outgoing
            .send(EncodedFrame {
                payload: encoded,
                timestamp,
            })
            .await
            .is_err()
        {
            break;
        }

        timestamp = timestamp.wrapping_add(frame_size as u32);
    }

    Ok(())
}

async fn decode_and_play(
    mut incoming: mpsc::Receiver<EncodedFrame>,
    mut producer: HeapProducer<i16>,
    available: Arc<AtomicUsize>,
    sample_rate: u32,
) -> Result<()> {
    let mut decoder = opus::Decoder::new(sample_rate, opus::Channels::Mono)?;
    let max_frame = (sample_rate / (1000 / FRAME_TIME_MS)) as usize;
    let mut pcm = vec![0i16; max_frame * 2];

    while let Some(frame) = incoming.recv().await {
        let len = decoder.decode(&frame.payload, &mut pcm, false)?;
        for &sample in &pcm[..len] {
            if producer.push(sample).is_ok() {
                available.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    Ok(())
}
