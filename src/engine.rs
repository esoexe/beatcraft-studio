use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    FromSample, Sample, SampleFormat, SizedSample,
};
use rtrb::{Consumer, Producer, RingBuffer};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

struct SharedPlayback {
    buffer: ArcSwap<Vec<f32>>,
    cursor: AtomicUsize,
    playing: AtomicBool,
}

pub struct AudioEngine {
    _stream: cpal::Stream,
    shared: Arc<SharedPlayback>,
    sample_rate: u32,
    channels: u16,
}
impl AudioEngine {
    pub fn new() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .context("No audio output device is available")?;
        let supported = device
            .default_output_config()
            .context("Could not query the default audio format")?;
        let format = supported.sample_format();
        let config = supported.config();
        anyhow::ensure!(
            config.channels >= 1,
            "Audio device reports no output channels."
        );
        let shared = Arc::new(SharedPlayback {
            buffer: ArcSwap::from_pointee(Vec::new()),
            cursor: AtomicUsize::new(0),
            playing: AtomicBool::new(false),
        });
        let callback_shared = shared.clone();
        let error_fn = |e| eprintln!("BeatCraft audio output: {e}");
        let stream = match format {
            SampleFormat::F32 => device.build_output_stream(
                &config,
                move |data: &mut [f32], _| write_output(data, config.channels, &callback_shared),
                error_fn,
                Some(Duration::from_millis(50)),
            ),
            SampleFormat::I16 => {
                let shared = shared.clone();
                device.build_output_stream(
                    &config,
                    move |data: &mut [i16], _| write_output(data, config.channels, &shared),
                    error_fn,
                    Some(Duration::from_millis(50)),
                )
            }
            SampleFormat::U16 => {
                let shared = shared.clone();
                device.build_output_stream(
                    &config,
                    move |data: &mut [u16], _| write_output(data, config.channels, &shared),
                    error_fn,
                    Some(Duration::from_millis(50)),
                )
            }
            other => anyhow::bail!("Unsupported output sample format: {other:?}"),
        }
        .context("Could not create audio output stream")?;
        stream
            .play()
            .context("Could not start audio output stream")?;
        Ok(Self {
            _stream: stream,
            shared,
            sample_rate: config.sample_rate.0,
            channels: config.channels,
        })
    }
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    pub fn channels(&self) -> u16 {
        self.channels
    }
    pub fn play(&self, audio: Vec<f32>) {
        self.shared.buffer.store(Arc::new(audio));
        self.shared.cursor.store(0, Ordering::Release);
        self.shared.playing.store(true, Ordering::Release);
    }
    pub fn stop(&self) {
        self.shared.playing.store(false, Ordering::Release);
        self.shared.cursor.store(0, Ordering::Release);
    }
    pub fn pause(&self) {
        self.shared.playing.store(false, Ordering::Release);
    }
    pub fn resume(&self) {
        self.shared.playing.store(true, Ordering::Release);
    }
    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Acquire)
    }
    pub fn position_frames(&self) -> usize {
        self.shared.cursor.load(Ordering::Acquire) / 2
    }
}

fn write_output<T>(output: &mut [T], channels: u16, shared: &SharedPlayback)
where
    T: Sample + SizedSample + FromSample<f32>,
{
    let clip = shared.buffer.load_full();
    let ch = channels.max(1) as usize;
    let mut cursor = shared.cursor.load(Ordering::Relaxed);
    let playing = shared.playing.load(Ordering::Relaxed);
    for frame in output.chunks_mut(ch) {
        let active = playing && cursor + 1 < clip.len();
        let left = if active { clip[cursor] } else { 0.0 };
        let right = if active { clip[cursor + 1] } else { 0.0 };
        if active {
            cursor += 2;
        } else if playing {
            shared.playing.store(false, Ordering::Release);
        }
        for (index, sample) in frame.iter_mut().enumerate() {
            *sample = T::from_sample(if index == 0 {
                left
            } else if index == 1 {
                right
            } else {
                (left + right) * 0.5
            });
        }
    }
    shared.cursor.store(cursor, Ordering::Release);
}

pub struct Recorder {
    stream: cpal::Stream,
    consumer: Consumer<f32>,
    pub sample_rate: u32,
    pub channels: u16,
}
impl Recorder {
    pub fn start() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .context("No audio input device is available")?;
        let supported = device
            .default_input_config()
            .context("Could not query the default input format")?;
        let format = supported.sample_format();
        let config = supported.config();
        let channels = config.channels;
        let rate = config.sample_rate.0;
        let capacity = (rate as usize * 2 * 90).max(4096);
        let (producer, consumer) = RingBuffer::<f32>::new(capacity);
        let error_fn = |e| eprintln!("BeatCraft audio input: {e}");
        let stream = match format {
            SampleFormat::F32 => build_input::<f32, _>(&device, &config, producer, error_fn),
            SampleFormat::I16 => build_input::<i16, _>(&device, &config, producer, error_fn),
            SampleFormat::U16 => build_input::<u16, _>(&device, &config, producer, error_fn),
            other => anyhow::bail!("Unsupported input sample format: {other:?}"),
        }
        .context("Could not create microphone/input stream")?;
        stream.play().context("Could not start recording input")?;
        Ok(Self {
            stream,
            consumer,
            sample_rate: rate,
            channels,
        })
    }
    pub fn finish(self) -> Vec<f32> {
        let Self {
            stream,
            mut consumer,
            ..
        } = self;
        drop(stream);
        let mut data = Vec::new();
        while let Ok(value) = consumer.pop() {
            data.push(value);
        }
        data
    }
}

fn build_input<T, F>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut producer: Producer<f32>,
    error_fn: F,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: Sample + SizedSample,
    f32: FromSample<T>,
    F: FnMut(cpal::StreamError) + Send + 'static,
{
    let channels = config.channels as usize;
    device.build_input_stream(
        config,
        move |input: &[T], _| {
            if channels == 1 {
                for sample in input {
                    let v = f32::from_sample(*sample);
                    let _ = producer.push(v);
                    let _ = producer.push(v);
                }
            } else {
                for frame in input.chunks(channels) {
                    let left = frame.first().copied().map(f32::from_sample).unwrap_or(0.0);
                    let right = frame.get(1).copied().map(f32::from_sample).unwrap_or(left);
                    let _ = producer.push(left);
                    let _ = producer.push(right);
                }
            }
        },
        error_fn,
        Some(Duration::from_millis(50)),
    )
}
