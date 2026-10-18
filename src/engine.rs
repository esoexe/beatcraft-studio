use anyhow::{Context, Result};
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

enum PlaybackSource {
    Clip(Vec<f32>),
    Stream(Consumer<f32>),
}

struct SharedPlayback {
    // Held behind a short-lived mutex; the callback only ever holds it for one
    // buffer render, producers only swap it at transport boundaries.
    source: std::sync::Mutex<PlaybackSource>,
    cursor: AtomicUsize, // frames (stereo pairs) consumed... in samples (2/frame)
    playing: AtomicBool,
    stream_total: AtomicUsize, // expected total frames in stream mode (0 = clip mode)
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
            source: std::sync::Mutex::new(PlaybackSource::Clip(Vec::new())),
            cursor: AtomicUsize::new(0),
            playing: AtomicBool::new(false),
            stream_total: AtomicUsize::new(0),
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
        *self.shared.source.lock().expect("audio source lock") = PlaybackSource::Clip(audio);
        self.shared.stream_total.store(0, Ordering::Release);
        self.shared.cursor.store(0, Ordering::Release);
        self.shared.playing.store(true, Ordering::Release);
    }

    /// Streamed playback: the engine consumes finished chunks produced
    /// just-ahead-of-playhead by the renderer thread. Underruns render silence
    /// instead of stalling; `total_frames` bounds auto-stop.
    pub fn play_stream(&self, consumer: Consumer<f32>, total_frames: usize) {
        *self.shared.source.lock().expect("audio source lock") = PlaybackSource::Stream(consumer);
        self.shared
            .stream_total
            .store(total_frames, Ordering::Release);
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
    let ch = channels.max(1) as usize;
    let mut cursor = shared.cursor.load(Ordering::Relaxed);
    let playing = shared.playing.load(Ordering::Relaxed);
    let stream_total = shared.stream_total.load(Ordering::Relaxed);
    let mut source = shared.source.lock().expect("audio source lock");
    for frame in output.chunks_mut(ch) {
        let (left, right, advanced) = match &mut *source {
            PlaybackSource::Clip(clip) => {
                let active = playing && cursor + 1 < clip.len();
                if active {
                    (clip[cursor], clip[cursor + 1], true)
                } else {
                    if playing {
                        shared.playing.store(false, Ordering::Release);
                    }
                    (0.0, 0.0, false)
                }
            }
            PlaybackSource::Stream(consumer) => {
                if playing && stream_total > 0 && cursor / 2 >= stream_total {
                    shared.playing.store(false, Ordering::Release);
                }
                let l = consumer.pop().unwrap_or(0.0);
                let r = consumer.pop().unwrap_or(0.0);
                (l, r, true)
            }
        };
        if advanced {
            cursor += 2;
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
