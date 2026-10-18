use crate::model::{AutomationTarget, Pattern, Project, SampleRef, TrackKind, STEPS};
use anyhow::{Context, Result};
use std::{collections::HashMap, f32::consts::TAU, path::Path};

#[derive(Clone, Debug)]
pub struct SampleClip {
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: Vec<f32>,
}
impl SampleClip {
    pub fn frame_count(&self) -> usize {
        self.frames.len() / self.channels.max(1) as usize
    }
}

pub fn load_wav(path: impl AsRef<Path>) -> Result<SampleClip> {
    let path = path.as_ref();
    let mut reader = hound::WavReader::open(path)
        .with_context(|| format!("Could not open sample {}", path.display()))?;
    let spec = reader.spec();
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .map(|s| s.map(|v| v.clamp(-1.0, 1.0)))
            .collect::<std::result::Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let max = ((1_i64 << (spec.bits_per_sample.saturating_sub(1))) - 1) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| (v as f32 / max).clamp(-1.0, 1.0)))
                .collect::<std::result::Result<_, _>>()?
        }
    };
    anyhow::ensure!(
        spec.channels > 0 && !samples.is_empty(),
        "Sample is empty or has no channels."
    );
    Ok(SampleClip {
        sample_rate: spec.sample_rate,
        channels: spec.channels,
        frames: samples,
    })
}

pub fn write_wav(path: impl AsRef<Path>, samples: &[f32], sample_rate: u32) -> Result<()> {
    write_wav_depth(
        path,
        samples,
        sample_rate,
        crate::model::WavBitDepth::Sixteen,
    )
}

/// Full-resolution WAV export: 16-bit / 24-bit PCM or 32-bit float.
pub fn write_wav_depth(
    path: impl AsRef<Path>,
    samples: &[f32],
    sample_rate: u32,
    depth: crate::model::WavBitDepth,
) -> Result<()> {
    use crate::model::WavBitDepth::{Float32, Sixteen, TwentyFour};
    match depth {
        Sixteen => {
            let spec = hound::WavSpec {
                channels: 2,
                sample_rate,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            };
            let mut writer = hound::WavWriter::create(path, spec)?;
            for value in samples {
                writer.write_sample((value.clamp(-1.0, 1.0) * 32767.0).round() as i16)?;
            }
            writer.finalize()?;
        }
        TwentyFour => {
            let spec = hound::WavSpec {
                channels: 2,
                sample_rate,
                bits_per_sample: 24,
                sample_format: hound::SampleFormat::Int,
            };
            let mut writer = hound::WavWriter::create(path, spec)?;
            for value in samples {
                writer.write_sample((value.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32)?;
            }
            writer.finalize()?;
        }
        Float32 => {
            let spec = hound::WavSpec {
                channels: 2,
                sample_rate,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            };
            let mut writer = hound::WavWriter::create(path, spec)?;
            for value in samples {
                writer.write_sample(value.clamp(-1.5, 1.5))?;
            }
            writer.finalize()?;
        }
    }
    Ok(())
}

pub fn render(project: &Project, sample_rate: u32) -> Result<Vec<f32>> {
    let bpm = project.bpm.clamp(50, 200) as f32;
    let step_seconds = 60.0 / bpm / 4.0;
    let bars = project.arrangement.len().max(1);
    let total_seconds = bars as f32 * 16.0 * step_seconds + 1.0;
    let frames = (total_seconds * sample_rate as f32).ceil() as usize;
    let mut stems: Vec<Vec<f32>> = project
        .tracks
        .iter()
        .map(|_| vec![0.0; frames * 2])
        .collect();
    let pattern_map: HashMap<&str, &Pattern> = project
        .patterns
        .iter()
        .map(|p| (p.id.as_str(), p))
        .collect();
    let any_solo = project.tracks.iter().any(|t| t.solo);
    let mut sample_cache: HashMap<String, SampleClip> = HashMap::new();

    for (bar, slot) in project.arrangement.iter().enumerate() {
        let Some(id) = slot.as_ref() else {
            continue;
        };
        let Some(pattern) = pattern_map.get(id.as_str()) else {
            continue;
        };
        for (track_index, track) in project.tracks.iter().enumerate() {
            if track.mute || (any_solo && !track.solo) {
                continue;
            }
            match track.kind {
                TrackKind::Kick
                | TrackKind::Snare
                | TrackKind::Hat
                | TrackKind::Cowbell
                | TrackKind::Sample => {
                    let row = pattern.rows.get(&track.id);
                    if let Some(row) = row {
                        for step in 0..STEPS {
                            if row.get(step).copied().unwrap_or(false) {
                                let start = onset_frame(
                                    bar,
                                    step,
                                    step_seconds,
                                    project.swing,
                                    sample_rate,
                                );
                                if track.kind == TrackKind::Sample {
                                    if let Some(sample) = track.sample.as_ref() {
                                        let clip = if let Some(c) = sample_cache.get(&sample.path) {
                                            c.clone()
                                        } else {
                                            let decoded = load_wav(&sample.path)?;
                                            sample_cache
                                                .insert(sample.path.clone(), decoded.clone());
                                            decoded
                                        };
                                        let voice = trim_and_resample(&clip, sample, sample_rate);
                                        if sample.looped {
                                            let next_step = ((step + 1)..STEPS)
                                                .find(|next| {
                                                    row.get(*next).copied().unwrap_or(false)
                                                })
                                                .unwrap_or(STEPS);
                                            let end = onset_frame(
                                                bar,
                                                next_step,
                                                step_seconds,
                                                project.swing,
                                                sample_rate,
                                            );
                                            add_looped_voice(
                                                &mut stems[track_index],
                                                start,
                                                &voice,
                                                end,
                                                sample.gain,
                                            );
                                        } else {
                                            add_voice(
                                                &mut stems[track_index],
                                                start,
                                                &voice,
                                                sample.gain,
                                            );
                                        }
                                    }
                                } else {
                                    let voice = synth_one_shot(
                                        track.kind,
                                        pattern.genre.as_str(),
                                        step,
                                        sample_rate,
                                    );
                                    add_voice(&mut stems[track_index], start, &voice, 1.0);
                                }
                            }
                        }
                    }
                }
                TrackKind::Sub | TrackKind::Bass | TrackKind::Keys => {
                    if let Some(notes) = pattern.notes.get(&track.id) {
                        for note in notes {
                            let start = onset_frame(
                                bar,
                                note.step as usize,
                                step_seconds,
                                project.swing,
                                sample_rate,
                            );
                            let duration = (note.duration as f32 * step_seconds).max(0.04);
                            let voice = note_voice(
                                track.kind,
                                note.pitch,
                                duration,
                                note.velocity,
                                sample_rate,
                            );
                            add_voice(&mut stems[track_index], start, &voice, 1.0);
                        }
                    }
                }
            }
        }
    }

    let mut mix = vec![0.0_f32; frames * 2];
    for (track_index, track) in project.tracks.iter().enumerate() {
        if track.mute || (any_solo && !track.solo) {
            continue;
        }
        process_fx(&mut stems[track_index], &track.fx, sample_rate);
        for frame in 0..frames {
            let time = frame as f32 / sample_rate as f32;
            let beat = time * bpm / 60.0;
            let auto_volume =
                automation_value(project, &track.id, AutomationTarget::Volume, beat).unwrap_or(1.0);
            let auto_pan =
                automation_value(project, &track.id, AutomationTarget::Pan, beat).unwrap_or(0.0);
            let pan = (track.pan + auto_pan).clamp(-1.0, 1.0);
            let left_gain = ((1.0 - pan) * 0.5).sqrt();
            let right_gain = ((1.0 + pan) * 0.5).sqrt();
            let gain = track.volume * auto_volume;
            let ix = frame * 2;
            mix[ix] += stems[track_index][ix] * gain * left_gain;
            mix[ix + 1] += stems[track_index][ix + 1] * gain * right_gain;
        }
    }
    let master = project.master.clamp(0.0, 1.0);
    let drive = project.grit.clamp(0.0, 1.0);
    let mut peak = 0.0_f32;
    for sample in &mut mix {
        *sample = (*sample * master * (1.0 + drive * 3.0)).tanh();
        peak = peak.max(sample.abs());
    }
    apply_master_suite(&mut mix, &project.master_suite, sample_rate);
    let ceiling = if project.master_suite.limiter_enabled {
        project.master_suite.limiter_ceiling.clamp(0.3, 1.0)
    } else {
        1.0
    };
    let peak = mix.iter().fold(0.0_f32, |p, s| p.max(s.abs()));
    if peak > ceiling {
        let scale = ceiling / peak;
        for sample in &mut mix {
            *sample *= scale;
        }
    }
    Ok(mix)
}

/* ================= Master Suite: Edit-grade mastering chain =================
   3-band EQ (RBJ biquads) → stereo width (M/S) → soft-knee compressor → tanh limiter.
*/

#[derive(Clone, Copy, Debug, Default)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}
impl Biquad {
    fn low_shelf(sr: u32, freq: f32, db: f32) -> Self {
        let a = 10f32.powf(db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * freq / sr as f32;
        let (cw, sw) = (w0.cos(), w0.sin() / (2.0f32).sqrt());
        let s = a.sqrt();
        let denom = (a + 1.0) + (a - 1.0) * cw + 2.0 * s * sw;
        let b0 = a * ((a + 1.0) - (a - 1.0) * cw + 2.0 * s * sw) / denom;
        let b1 = 2.0 * a * ((a - 1.0) - (a + 1.0) * cw) / denom;
        let b2 = a * ((a + 1.0) - (a - 1.0) * cw - 2.0 * s * sw) / denom;
        let a1 = -2.0 * ((a - 1.0) + (a + 1.0) * cw) / denom;
        let a2 = ((a + 1.0) + (a - 1.0) * cw - 2.0 * s * sw) / denom;
        Self {
            b0,
            b1,
            b2,
            a1,
            a2,
            ..Default::default()
        }
    }
    fn high_shelf(sr: u32, freq: f32, db: f32) -> Self {
        let a = 10f32.powf(db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * freq / sr as f32;
        let (cw, sw) = (w0.cos(), w0.sin() / (2.0f32).sqrt());
        let s = a.sqrt();
        let denom = (a + 1.0) - (a - 1.0) * cw + 2.0 * s * sw;
        let b0 = a * ((a + 1.0) + (a - 1.0) * cw + 2.0 * s * sw) / denom;
        let b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cw) / denom;
        let b2 = a * ((a + 1.0) + (a - 1.0) * cw - 2.0 * s * sw) / denom;
        let a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cw) / denom;
        let a2 = ((a + 1.0) - (a - 1.0) * cw - 2.0 * s * sw) / denom;
        Self {
            b0,
            b1,
            b2,
            a1,
            a2,
            ..Default::default()
        }
    }
    fn peak(sr: u32, freq: f32, db: f32, q: f32) -> Self {
        let a = 10f32.powf(db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * freq / sr as f32;
        let (cw, alpha) = (w0.cos(), w0.sin() / (2.0 * q));
        let denom = 1.0 + alpha / a;
        Self {
            b0: (1.0 + alpha * a) / denom,
            b1: -2.0 * cw / denom,
            b2: (1.0 - alpha * a) / denom,
            a1: -2.0 * cw / denom,
            a2: (1.0 - alpha / a) / denom,
            ..Default::default()
        }
    }
    #[inline]
    fn run(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// Applies project.master_suite in place over interleaved stereo.
pub fn apply_master_suite(mix: &mut [f32], suite: &crate::model::MasterSuite, sr: u32) {
    if mix.len() < 4 {
        return;
    }

    if suite.eq_enabled
        && (suite.eq_low_db.abs() > 0.01
            || suite.eq_mid_db.abs() > 0.01
            || suite.eq_high_db.abs() > 0.01)
    {
        let mut lx = [
            Biquad::low_shelf(sr, 120.0, suite.eq_low_db),
            Biquad::peak(sr, 1200.0, suite.eq_mid_db, 0.9),
            Biquad::high_shelf(sr, 6500.0, suite.eq_high_db),
        ];
        let mut rx = lx;
        for i in (0..mix.len()).step_by(2) {
            let (mut l, mut r) = (mix[i], mix[i + 1]);
            for f in &mut lx {
                l = f.run(l);
            }
            for f in &mut rx {
                r = f.run(r);
            }
            mix[i] = l;
            mix[i + 1] = r;
        }
    }

    if (suite.stereo_width - 1.0).abs() > 0.001 {
        let w = suite.stereo_width.clamp(0.0, 2.0);
        for i in (0..mix.len()).step_by(2) {
            let mid = (mix[i] + mix[i + 1]) * 0.5;
            let side = (mix[i] - mix[i + 1]) * 0.5 * w;
            mix[i] = mid + side;
            mix[i + 1] = mid - side;
        }
    }

    if suite.comp_enabled {
        let threshold = 10f32.powf(suite.comp_threshold_db.clamp(-60.0, 0.0) / 20.0);
        let ratio = suite.comp_ratio.clamp(1.0, 20.0);
        let makeup = 10f32.powf(suite.comp_makeup_db.clamp(0.0, 24.0) / 20.0);
        let (att, rel) = (
            (-1.0 / (0.004 * sr as f32)).exp(),
            (-1.0 / (0.09 * sr as f32)).exp(),
        );
        let mut env = 0.0f32;
        for i in (0..mix.len()).step_by(2) {
            let peak = mix[i].abs().max(mix[i + 1].abs());
            env = if peak > env {
                att * env + (1.0 - att) * peak
            } else {
                rel * env + (1.0 - rel) * peak
            };
            let mut gain = 1.0f32;
            if env > threshold {
                let over_db = 20.0 * (env / threshold.max(1e-9)).log10();
                let gain_db = -over_db * (1.0 - 1.0 / ratio);
                gain = 10f32.powf(gain_db / 20.0);
            }
            mix[i] *= gain * makeup;
            mix[i + 1] *= gain * makeup;
        }
    }

    if suite.limiter_enabled {
        let ceil = suite.limiter_ceiling.clamp(0.3, 1.0);
        for s in mix.iter_mut() {
            *s = ceil * (*s / ceil).tanh();
        }
    }
}

fn onset_frame(bar: usize, step: usize, step_seconds: f32, swing: u8, sample_rate: u32) -> usize {
    let delay = if step % 2 == 1 {
        step_seconds * (swing.min(60) as f32 / 100.0) * 0.46
    } else {
        0.0
    };
    (((bar * STEPS + step) as f32 * step_seconds + delay) * sample_rate as f32) as usize
}

fn add_voice(stem: &mut [f32], start: usize, mono: &[f32], gain: f32) {
    if start >= stem.len() / 2 {
        return;
    }
    let end = (start + mono.len()).min(stem.len() / 2);
    for (frame, sample) in (start..end).zip(mono.iter()) {
        let i = frame * 2;
        let v = *sample * gain;
        stem[i] += v;
        stem[i + 1] += v;
    }
}

fn add_looped_voice(stem: &mut [f32], start: usize, mono: &[f32], end: usize, gain: f32) {
    let end = end.min(stem.len() / 2);
    if mono.is_empty() || start >= end {
        return;
    }
    let mut cursor = start;
    while cursor < end {
        let count = (end - cursor).min(mono.len());
        for (offset, sample) in mono[..count].iter().enumerate() {
            let index = (cursor + offset) * 2;
            let value = *sample * gain;
            stem[index] += value;
            stem[index + 1] += value;
        }
        cursor += count;
    }
}

fn midi_hz(pitch: u8) -> f32 {
    440.0 * 2.0_f32.powf((pitch as f32 - 69.0) / 12.0)
}

fn note_voice(kind: TrackKind, pitch: u8, duration: f32, velocity: u8, sr: u32) -> Vec<f32> {
    let release = 0.12_f32.min((duration * 0.25).max(0.05));
    let len = ((duration + release) * sr as f32) as usize;
    let freq = midi_hz(pitch.min(127));
    let amplitude = velocity as f32 / 127.0
        * match kind {
            TrackKind::Sub => 0.50,
            TrackKind::Bass => 0.22,
            TrackKind::Keys => 0.12,
            _ => 0.1,
        };
    (0..len)
        .map(|i| {
            let t = i as f32 / sr as f32;
            let phase = TAU * freq * t;
            let raw = match kind {
                TrackKind::Bass => {
                    let saw = 2.0 * ((freq * t) % 1.0) - 1.0;
                    (saw + 0.30 * (TAU * freq * 2.0 * t).sin()) / 1.3
                }
                TrackKind::Keys => {
                    0.82 * phase.sin() + 0.14 * (phase * 2.0).sin() + 0.04 * (phase * 3.0).sin()
                }
                _ => phase.sin() + 0.035 * (phase * 2.0).sin(),
            };
            let attack = (t / 0.012).min(1.0);
            let release_gain = if t > duration {
                (1.0 - (t - duration) / release).clamp(0.0, 1.0)
            } else {
                1.0
            };
            let decay = if kind == TrackKind::Bass {
                (-t * 2.6).exp()
            } else {
                1.0
            };
            raw * attack * release_gain * decay * amplitude
        })
        .collect()
}

fn synth_one_shot(kind: TrackKind, genre: &str, step: usize, sr: u32) -> Vec<f32> {
    let duration = match kind {
        TrackKind::Kick => 0.40,
        TrackKind::Snare => 0.23,
        TrackKind::Hat => 0.09,
        TrackKind::Cowbell => 0.20,
        _ => 0.18,
    };
    let len = (duration * sr as f32) as usize;
    let mut noise = step as u32 + 0x9e37_79b9;
    (0..len)
        .map(|i| {
            let t = i as f32 / sr as f32;
            let f = if genre == "FUNK" { 52.0 } else { 43.0 };
            match kind {
                TrackKind::Kick => {
                    let phase = TAU * (f * t + 5.0 * (1.0 - (-t * 26.0).exp()) / 26.0);
                    (phase.sin() * (-t * 8.8).exp()
                        + 0.08 * (TAU * 1600.0 * t).sin() * (-t * 75.0).exp())
                        * 0.82
                }
                TrackKind::Snare => {
                    noise ^= noise << 13;
                    noise ^= noise >> 17;
                    noise ^= noise << 5;
                    let n = (noise as i32 as f32) / (i32::MAX as f32);
                    let tone = (TAU * 190.0 * t).sin() * 0.24;
                    (n * 0.28 + tone) * (-t * 19.0).exp()
                }
                TrackKind::Hat => {
                    noise ^= noise << 13;
                    noise ^= noise >> 17;
                    noise ^= noise << 5;
                    let n = (noise as i32 as f32) / (i32::MAX as f32);
                    n * 0.20 * (-t * 60.0).exp()
                }
                TrackKind::Cowbell => {
                    ((TAU * 565.0 * t).sin().signum() * 0.07
                        + (TAU * 846.0 * t).sin().signum() * 0.05)
                        * (-t * 17.0).exp()
                }
                _ => 0.0,
            }
        })
        .collect()
}

fn trim_and_resample(clip: &SampleClip, reference: &SampleRef, target_rate: u32) -> Vec<f32> {
    let channels = clip.channels.max(1) as usize;
    let total = clip.frame_count();
    let start = (reference.trim_start.clamp(0.0, 0.99) * total as f32) as usize;
    let end = (reference.trim_end.clamp(0.01, 1.0) * total as f32).max(start as f32 + 1.0) as usize;
    let end = end.min(total);
    let source_len = end.saturating_sub(start);
    if source_len == 0 {
        return Vec::new();
    }
    let target_len =
        (source_len as f64 * target_rate as f64 / clip.sample_rate.max(1) as f64) as usize;
    let mut output = Vec::with_capacity(target_len);
    for i in 0..target_len {
        let source_pos = i as f64 * clip.sample_rate as f64 / target_rate as f64;
        let frame = (source_pos as usize).min(source_len - 1) + start;
        let next = (frame + 1).min(end - 1);
        let frac = (source_pos.fract()) as f32;
        let a = mono_frame(clip, frame, channels);
        let b = mono_frame(clip, next, channels);
        output.push((a + (b - a) * frac) * reference.gain.clamp(0.0, 4.0));
    }
    output
}
fn mono_frame(clip: &SampleClip, frame: usize, channels: usize) -> f32 {
    let base = frame * channels;
    if channels == 1 {
        clip.frames.get(base).copied().unwrap_or(0.0)
    } else {
        clip.frames.get(base).copied().unwrap_or(0.0) * 0.5
            + clip.frames.get(base + 1).copied().unwrap_or(0.0) * 0.5
    }
}

fn process_fx(stereo: &mut [f32], fx: &crate::model::FxChain, sr: u32) {
    let cutoff = fx.lowpass_hz.clamp(40.0, 20_000.0);
    let alpha = (1.0 - (-TAU * cutoff / sr as f32).exp()).clamp(0.001, 1.0);
    let delay_len = ((fx.delay_ms.clamp(20.0, 2000.0) / 1000.0 * sr as f32) as usize).max(1);
    let mut delay = vec![0.0_f32; delay_len * 2];
    let mut delay_pos = 0;
    let mut reverb_a = vec![0.0_f32; ((sr as f32 * 0.029).round() as usize).max(1) * 2];
    let mut reverb_b = vec![0.0_f32; ((sr as f32 * 0.037).round() as usize).max(1) * 2];
    let mut ra = 0;
    let mut rb = 0;
    let mut low = [0.0_f32; 2];
    let (frames, remainder) = stereo.as_chunks_mut::<2>();
    debug_assert!(remainder.is_empty());
    for frame in frames {
        let input = [frame[0], frame[1]];
        let mut filtered = [0.0; 2];
        for ch in 0..2 {
            low[ch] += alpha * (input[ch] - low[ch]);
            let driven = (low[ch] * (1.0 + fx.drive * 6.0)).tanh();
            filtered[ch] = driven;
        }
        for ch in 0..2 {
            let delayed = delay[delay_pos * 2 + ch];
            let wet_a = reverb_a[ra * 2 + ch];
            let wet_b = reverb_b[rb * 2 + ch];
            delay[delay_pos * 2 + ch] = filtered[ch] + delayed * fx.delay_feedback;
            reverb_a[ra * 2 + ch] = filtered[ch] * 0.28 + wet_a * 0.72;
            reverb_b[rb * 2 + ch] = filtered[ch] * 0.24 + wet_b * 0.76;
            frame[ch] = filtered[ch] * (1.0 - fx.delay_mix - fx.reverb_mix * 0.5)
                + delayed * fx.delay_mix
                + (wet_a + wet_b) * fx.reverb_mix * 0.5;
        }
        delay_pos = (delay_pos + 1) % delay_len;
        ra = (ra + 1) % reverb_a.len().max(2).div_ceil(2);
        rb = (rb + 1) % reverb_b.len().max(2).div_ceil(2);
    }
}

fn automation_value(
    project: &Project,
    track_id: &str,
    target: AutomationTarget,
    beat: f32,
) -> Option<f32> {
    let lane = project
        .automation
        .iter()
        .find(|l| l.track_id == track_id && l.target == target)?;
    if lane.points.is_empty() {
        return None;
    }
    if beat <= lane.points[0].beat {
        return Some(lane.points[0].value);
    }
    for pair in lane.points.windows(2) {
        if beat <= pair[1].beat {
            let span = (pair[1].beat - pair[0].beat).max(f32::EPSILON);
            let t = ((beat - pair[0].beat) / span).clamp(0.0, 1.0);
            return Some(pair[0].value + (pair[1].value - pair[0].value) * t);
        }
    }
    lane.points.last().map(|p| p.value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{default_project, AutomationLane, AutomationPoint, Note};
    #[test]
    fn renderer_makes_finite_stereo() {
        let p = default_project("nyx");
        let a = render(&p, 8000).unwrap();
        assert_eq!(a.len() % 2, 0);
        assert!(a.iter().all(|v| v.is_finite()));
        assert!(a.iter().any(|v| v.abs() > 0.01));
    }
    #[test]
    fn note_pitch_changes_audio() {
        let mut p = default_project("hermes");
        let pattern = p.selected_pattern_mut().unwrap();
        for r in pattern.rows.values_mut() {
            r.fill(false);
        }
        for n in pattern.notes.values_mut() {
            n.clear();
        }
        pattern.notes.get_mut("keys").unwrap().push(Note {
            step: 0,
            pitch: 70,
            duration: 8,
            velocity: 100,
        });
        let a = render(&p, 8000).unwrap();
        assert!(a.iter().any(|v| v.abs() > 0.01));
    }

    #[test]
    fn automation_interpolates_linearly() {
        let mut project = default_project("nyx");
        project.automation.push(AutomationLane {
            id: "kick-volume".into(),
            track_id: "kick".into(),
            target: AutomationTarget::Volume,
            points: vec![
                AutomationPoint {
                    beat: 0.0,
                    value: 0.0,
                },
                AutomationPoint {
                    beat: 4.0,
                    value: 1.0,
                },
            ],
        });
        assert_eq!(
            automation_value(&project, "kick", AutomationTarget::Volume, 2.0),
            Some(0.5)
        );
    }

    #[test]
    fn looped_voice_repeats_until_its_boundary() {
        let mut stereo = vec![0.0; 12];
        add_looped_voice(&mut stereo, 1, &[0.25, 0.5], 5, 1.0);
        assert_eq!(
            stereo,
            vec![0.0, 0.0, 0.25, 0.25, 0.5, 0.5, 0.25, 0.25, 0.5, 0.5, 0.0, 0.0]
        );
    }
}

#[cfg(test)]
mod mastering_tests {
    use super::*;
    use crate::model::{default_project, MasterSuite, WavBitDepth};

    fn hot_buf(sr: u32, secs: f32) -> Vec<f32> {
        let n = (sr as f32 * secs) as usize;
        let mut v = Vec::with_capacity(n * 2);
        for i in 0..n {
            let s = (i as f32 / sr as f32 * 440.0 * std::f32::consts::TAU).sin() * 0.9;
            v.push(s);
            v.push(s);
        }
        v
    }

    #[test]
    fn compressor_tames_peaks() {
        let sr = 44_100;
        let mut buf = hot_buf(sr, 0.5);
        let peak_before = buf.iter().fold(0.0f32, |p, s| p.max(s.abs()));
        let suite = MasterSuite {
            comp_threshold_db: -30.0,
            comp_ratio: 8.0,
            comp_makeup_db: 0.0,
            limiter_enabled: false,
            eq_enabled: false,
            ..MasterSuite::default()
        };
        apply_master_suite(&mut buf, &suite, sr);
        let peak_after = buf.iter().fold(0.0f32, |p, s| p.max(s.abs()));
        assert!(
            peak_after < peak_before * 0.85,
            "compressor should tame peaks ({peak_after} vs {peak_before})"
        );
    }

    #[test]
    fn limiter_respects_ceiling() {
        let sr = 44_100;
        let mut buf = hot_buf(sr, 0.2);
        let suite = MasterSuite {
            limiter_ceiling: 0.5,
            comp_enabled: false,
            eq_enabled: false,
            ..MasterSuite::default()
        };
        apply_master_suite(&mut buf, &suite, sr);
        let peak = buf.iter().fold(0.0f32, |p, s| p.max(s.abs()));
        assert!(peak <= 0.5 + 1e-3, "limiter peak {peak} above ceiling");
    }

    #[test]
    fn low_shelf_changes_low_band_energy() {
        let sr = 44_100;
        let n = sr as usize / 2;
        let mut buf: Vec<f32> = Vec::with_capacity(n * 2);
        for i in 0..n {
            let s = (i as f32 / sr as f32 * 80.0 * std::f32::consts::TAU).sin() * 0.5;
            buf.push(s);
            buf.push(s);
        }
        let rms = |b: &[f32]| (b.iter().map(|s| s * s).sum::<f32>() / b.len() as f32).sqrt();
        let before = rms(&buf);
        let suite = MasterSuite {
            eq_low_db: 9.0,
            comp_enabled: false,
            limiter_enabled: false,
            ..MasterSuite::default()
        };
        apply_master_suite(&mut buf, &suite, sr);
        assert!(
            rms(&buf) > before * 1.4,
            "low shelf +9dB should boost low energy"
        );
    }

    #[test]
    fn stereo_width_narrows_to_mid_when_zero() {
        let sr = 44_100;
        let mut buf = vec![0.0f32; 8];
        buf[0] = 0.8;
        buf[1] = 0.2;
        buf[2] = -0.6;
        buf[3] = 0.4;
        let suite = MasterSuite {
            stereo_width: 0.0,
            comp_enabled: false,
            limiter_enabled: false,
            eq_enabled: false,
            ..MasterSuite::default()
        };
        apply_master_suite(&mut buf, &suite, sr);
        assert!(
            (buf[0] - buf[1]).abs() < 1e-6,
            "width 0 should collapse to mono"
        );
    }

    #[test]
    fn render_carries_master_suite_safely() {
        let mut project = default_project("nyx");
        project.master_suite.limiter_ceiling = 0.4;
        project.master_suite.comp_enabled = false;
        let out = render(&project, 22_050).expect("render");
        let peak = out.iter().fold(0.0f32, |p, s| p.max(s.abs()));
        assert!(
            out.len() % 2 == 0 && peak <= 0.4 + 1e-3,
            "render peak {peak} over suite ceiling"
        );
    }

    #[test]
    fn wav_depths_roundtrip_lengths() {
        let dir = std::env::temp_dir();
        let samples = vec![0.1f32, -0.2, 0.4, -0.8];
        for (depth, min_bytes) in [
            (WavBitDepth::Sixteen, 8),
            (WavBitDepth::TwentyFour, 12),
            (WavBitDepth::Float32, 16),
        ] {
            let path = dir.join(format!("bc_depth_{min_bytes}.wav"));
            write_wav_depth(&path, &samples, 44_100, depth).expect("write");
            let meta = std::fs::metadata(&path).expect("file");
            assert!(
                meta.len() as usize >= 44 + min_bytes,
                "wav too small for {depth:?}"
            );
            let _ = std::fs::remove_file(&path);
        }
    }
}
