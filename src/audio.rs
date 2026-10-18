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
    render_with_progress(project, sample_rate, None)
}

/// The complete mix signal chain up to and including the Master Suite and any
/// enabled master CLAP insert, **before** the global ceiling normalize. This is
/// the reference the streaming engine must reproduce sample-for-sample.
pub fn render_core(project: &Project, sample_rate: u32) -> Result<Vec<f32>> {
    render_core_impl(project, sample_rate, None)
}

/// Same renderer as [`render`], optionally invoking `progress(0.0..=1.0)` as each
/// arrangement bar is synthesized. Parallel/deterministic — progress is a pure
/// side-channel and never changes the audio.
pub fn render_with_progress(
    project: &Project,
    sample_rate: u32,
    progress: Option<&mut dyn FnMut(f32)>,
) -> Result<Vec<f32>> {
    let mut mix = render_core_impl(project, sample_rate, progress)?;
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

fn render_core_impl(
    project: &Project,
    sample_rate: u32,
    mut progress: Option<&mut dyn FnMut(f32)>,
) -> Result<Vec<f32>> {
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
    // Plugin tracks: absolute-frame note events collected during the bar loop,
    // rendered in one continuous pass afterwards (see below).
    let mut plugin_events: HashMap<usize, Vec<crate::clap::NoteEvent>> = HashMap::new();

    for (bar, slot) in project.arrangement.iter().enumerate() {
        if let Some(cb) = progress.as_deref_mut() {
            cb(bar as f32 / bars.max(1) as f32);
        }
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
                                        let voice = trim_and_resample(
                                            &clip,
                                            sample,
                                            sample_rate,
                                            step_seconds as f64 * sample_rate as f64,
                                        );
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
                TrackKind::Plugin => {
                    if let Some(notes) = pattern.notes.get(&track.id) {
                        let evs = plugin_events.entry(track_index).or_default();
                        for note in notes {
                            let on = onset_frame(
                                bar,
                                note.step as usize,
                                step_seconds,
                                project.swing,
                                sample_rate,
                            );
                            let off_step = note.step as usize + note.duration.max(1) as usize;
                            let off = onset_frame(
                                bar,
                                off_step,
                                step_seconds,
                                project.swing,
                                sample_rate,
                            );
                            let key = note.pitch.min(127);
                            evs.push(crate::clap::NoteEvent {
                                abs_frame: on,
                                on: true,
                                key,
                                velocity: note.velocity as f32 / 127.0,
                            });
                            evs.push(crate::clap::NoteEvent {
                                abs_frame: off,
                                on: false,
                                key,
                                velocity: 0.0,
                            });
                        }
                    }
                }
            }
        }
    }

    // CLAP instruments: one continuous pass per track after the source loop
    // and before per-track FX. The persistent instance plus absolute event
    // filtering make this tile-identical to the streaming engine.
    let mut track_latencies: Vec<(usize, usize)> = Vec::new();
    for (track_index, track) in project.tracks.iter().enumerate() {
        if track.kind != TrackKind::Plugin {
            continue;
        }
        if track.mute || (any_solo && !track.solo) {
            continue;
        }
        let Some(insert) = track.clap_instrument.as_ref().filter(|i| i.enabled) else {
            continue;
        };
        let events = plugin_events.get(&track_index).cloned().unwrap_or_default();
        match crate::clap::ClapSynth::load(
            std::path::Path::new(&insert.path),
            &insert.plugin_id,
            sample_rate,
        ) {
            Ok(mut synth) => {
                synth.apply_initial(&insert.params);
                track_latencies.push((track_index, synth.latency()));
                synth.render_window(&mut stems[track_index], 0, &events);
            }
            Err(e) => eprintln!("CLAP instrument '{}' skipped: {e}", insert.name),
        }
    }
    // PDC: shift every stem so all tracks ride the slowest plugin's time base.
    let latency_max = track_latencies.iter().map(|l| l.1).max().unwrap_or(0);
    if latency_max > 0 {
        for (track_index, stem) in stems.iter_mut().enumerate() {
            let lt = track_latencies
                .iter()
                .find(|l| l.0 == track_index)
                .map(|l| l.1)
                .unwrap_or(0);
            let shift = latency_max.saturating_sub(lt);
            if shift > 0 {
                let mut pending = vec![0.0_f32; shift * 2];
                apply_latency_shift(stem, &mut pending);
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
    MasterSuiteState::new(&project.master_suite, sample_rate).process(&mut mix);
    if let Some(mut fx) = master_fx_instance(project, sample_rate) {
        fx.process_chunk(&mut mix);
    }
    Ok(mix)
}

/// Instantiate the project's master CLAP insert, if one is set and enabled.
/// Failures degrade to None so a bounce never fails over a missing plugin.
fn master_fx_instance(project: &Project, sample_rate: u32) -> Option<crate::clap::ClapFx> {
    let insert = project.master_clap.as_ref().filter(|i| i.enabled)?;
    match crate::clap::ClapFx::load(
        std::path::Path::new(&insert.path),
        &insert.plugin_id,
        sample_rate,
    ) {
        Ok(mut fx) => {
            fx.apply_initial(&insert.params);
            Some(fx)
        }
        Err(e) => {
            eprintln!("CLAP insert '{}' skipped: {e}", insert.name);
            None
        }
    }
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

/// Persistent mastering-chain state (EQ biquads + compressor envelope survive
/// chunk boundaries). Emitted in the fixed stage order: EQ → width → comp → limiter.
struct MasterSuiteState {
    eq: Option<([Biquad; 3], [Biquad; 3])>,
    width: Option<f32>,
    comp: Option<(f32, f32, f32, f32, f32, f32)>, // att, rel, threshold, ratio, makeup, env
    limiter: Option<f32>,
}
impl MasterSuiteState {
    fn new(suite: &crate::model::MasterSuite, sr: u32) -> Self {
        let eq = if suite.eq_enabled
            && (suite.eq_low_db.abs() > 0.01
                || suite.eq_mid_db.abs() > 0.01
                || suite.eq_high_db.abs() > 0.01)
        {
            let lx = [
                Biquad::low_shelf(sr, 120.0, suite.eq_low_db),
                Biquad::peak(sr, 1200.0, suite.eq_mid_db, 0.9),
                Biquad::high_shelf(sr, 6500.0, suite.eq_high_db),
            ];
            Some((lx, lx))
        } else {
            None
        };
        let width =
            ((suite.stereo_width - 1.0).abs() > 0.001).then(|| suite.stereo_width.clamp(0.0, 2.0));
        let comp = suite.comp_enabled.then(|| {
            (
                (-1.0 / (0.004 * sr as f32)).exp(),
                (-1.0 / (0.09 * sr as f32)).exp(),
                10f32.powf(suite.comp_threshold_db.clamp(-60.0, 0.0) / 20.0),
                suite.comp_ratio.clamp(1.0, 20.0),
                10f32.powf(suite.comp_makeup_db.clamp(0.0, 24.0) / 20.0),
                0.0f32,
            )
        });
        let limiter = suite
            .limiter_enabled
            .then(|| suite.limiter_ceiling.clamp(0.3, 1.0));
        Self {
            eq,
            width,
            comp,
            limiter,
        }
    }

    fn process(&mut self, mix: &mut [f32]) {
        if let Some((lx, rx)) = &mut self.eq {
            for i in (0..mix.len()).step_by(2) {
                let (mut l, mut r) = (mix[i], mix[i + 1]);
                for f in lx.iter_mut() {
                    l = f.run(l);
                }
                for f in rx.iter_mut() {
                    r = f.run(r);
                }
                mix[i] = l;
                mix[i + 1] = r;
            }
        }
        if let Some(w) = self.width {
            for i in (0..mix.len()).step_by(2) {
                let mid = (mix[i] + mix[i + 1]) * 0.5;
                let side = (mix[i] - mix[i + 1]) * 0.5 * w;
                mix[i] = mid + side;
                mix[i + 1] = mid - side;
            }
        }
        if let Some((att, rel, threshold, ratio, makeup, env)) = &mut self.comp {
            for i in (0..mix.len()).step_by(2) {
                let peak = mix[i].abs().max(mix[i + 1].abs());
                *env = if peak > *env {
                    *att * *env + (1.0 - *att) * peak
                } else {
                    *rel * *env + (1.0 - *rel) * peak
                };
                let mut gain = 1.0f32;
                if *env > *threshold {
                    let over_db = 20.0 * (*env / (*threshold).max(1e-9)).log10();
                    let gain_db = -over_db * (1.0 - 1.0 / *ratio);
                    gain = 10f32.powf(gain_db / 20.0);
                }
                mix[i] *= gain * *makeup;
                mix[i + 1] *= gain * *makeup;
            }
        }
        if let Some(ceil) = self.limiter {
            for s in mix.iter_mut() {
                *s = ceil * (*s / ceil).tanh();
            }
        }
    }
}

/// Applies project.master_suite in place over interleaved stereo.
pub fn apply_master_suite(mix: &mut [f32], suite: &crate::model::MasterSuite, sr: u32) {
    if mix.len() < 4 {
        return;
    }
    MasterSuiteState::new(suite, sr).process(mix);
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

fn trim_and_resample(
    clip: &SampleClip,
    reference: &SampleRef,
    target_rate: u32,
    step_frames: f64,
) -> Vec<f32> {
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
    // Varispeed fit: squeeze the take into exactly fit_steps 16ths (0 = natural).
    let fit = reference.fit_steps.clamp(0, 64) as f64;
    if fit > 0.0 && !output.is_empty() && step_frames > 0.0 {
        let target = ((fit * step_frames) as usize).max(1);
        let mut fitted = Vec::with_capacity(target);
        for i in 0..target {
            let pos = i as f64 * output.len() as f64 / target as f64;
            let base = (pos as usize).min(output.len() - 1);
            let next = (base + 1).min(output.len() - 1);
            let frac = (pos.fract()) as f32;
            fitted.push(output[base] + (output[next] - output[base]) * frac);
        }
        fitted
    } else {
        output
    }
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

/// Per-track FX processor with persistent causal state. Identical per-frame math
/// to the original monolithic pass — the streaming engine holds one per track.
struct FxState {
    fx: crate::model::FxChain,
    alpha: f32,
    delay: Vec<f32>,
    delay_pos: usize,
    reverb_a: Vec<f32>,
    reverb_b: Vec<f32>,
    ra: usize,
    rb: usize,
    low: [f32; 2],
}
impl FxState {
    fn new(fx: &crate::model::FxChain, sr: u32) -> Self {
        let cutoff = fx.lowpass_hz.clamp(40.0, 20_000.0);
        let alpha = (1.0 - (-TAU * cutoff / sr as f32).exp()).clamp(0.001, 1.0);
        let delay_len = ((fx.delay_ms.clamp(20.0, 2000.0) / 1000.0 * sr as f32) as usize).max(1);
        Self {
            fx: fx.clone(),
            alpha,
            delay: vec![0.0_f32; delay_len * 2],
            delay_pos: 0,
            reverb_a: vec![0.0_f32; ((sr as f32 * 0.029).round() as usize).max(1) * 2],
            reverb_b: vec![0.0_f32; ((sr as f32 * 0.037).round() as usize).max(1) * 2],
            ra: 0,
            rb: 0,
            low: [0.0_f32; 2],
        }
    }
    fn process(&mut self, stereo: &mut [f32]) {
        let fx = &self.fx;
        let alpha = self.alpha;
        let delay_len = self.delay.len() / 2;
        let (frames, remainder) = stereo.as_chunks_mut::<2>();
        debug_assert!(remainder.is_empty());
        for frame in frames {
            let input = [frame[0], frame[1]];
            let mut filtered = [0.0; 2];
            for ch in 0..2 {
                self.low[ch] += alpha * (input[ch] - self.low[ch]);
                let driven = (self.low[ch] * (1.0 + fx.drive * 6.0)).tanh();
                filtered[ch] = driven;
            }
            for ch in 0..2 {
                let delayed = self.delay[self.delay_pos * 2 + ch];
                let wet_a = self.reverb_a[self.ra * 2 + ch];
                let wet_b = self.reverb_b[self.rb * 2 + ch];
                self.delay[self.delay_pos * 2 + ch] = filtered[ch] + delayed * fx.delay_feedback;
                self.reverb_a[self.ra * 2 + ch] = filtered[ch] * 0.28 + wet_a * 0.72;
                self.reverb_b[self.rb * 2 + ch] = filtered[ch] * 0.24 + wet_b * 0.76;
                frame[ch] = filtered[ch] * (1.0 - fx.delay_mix - fx.reverb_mix * 0.5)
                    + delayed * fx.delay_mix
                    + (wet_a + wet_b) * fx.reverb_mix * 0.5;
            }
            self.delay_pos = (self.delay_pos + 1) % delay_len;
            self.ra = (self.ra + 1) % self.reverb_a.len().max(2).div_ceil(2);
            self.rb = (self.rb + 1) % self.reverb_b.len().max(2).div_ceil(2);
        }
    }
}

fn process_fx(stereo: &mut [f32], fx: &crate::model::FxChain, sr: u32) {
    FxState::new(fx, sr).process(stereo);
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

/// Render job executed on a worker thread; the UI (or any caller) stays fully
/// responsive while bouncing. Progress messages stream in as bars complete.
pub enum RenderMsg {
    Progress { bar: usize, bars: usize },
    Done(Result<Vec<f32>, String>),
}

pub fn spawn_render(
    project: Project,
    sample_rate: u32,
) -> (
    std::sync::mpsc::Receiver<RenderMsg>,
    std::thread::JoinHandle<()>,
) {
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let bars = project.arrangement.len().max(1);
        let result = render_with_progress(
            &project,
            sample_rate,
            Some(&mut |p: f32| {
                let bar = (p * bars as f32) as usize;
                let _ = tx.send(RenderMsg::Progress { bar, bars });
            }),
        )
        .map_err(|e| e.to_string());
        let _ = tx.send(RenderMsg::Done(result));
    });
    (rx, handle)
}

/* ================= True streaming renderer (phase 2) =================
   Produces the arrangement bar-by-bar with persistent per-track FX and master
   states. The reference contract: concatenated chunks == render_core() exactly.
*/

struct StemStream {
    fx: FxState,
    pending: Vec<f32>, // interleaved stereo source overflow into later window
    /// Per-track latency-alignment carry: `(Lmax − track latency) * 2` samples
    /// of leading silence on the first window, then carry afterward.
    shift_pending: Vec<f32>,
}

/// Shift `window` right by `pending.len()/2` frames, using `pending` as the
/// persistent carry. This is the track-latency alignment primitive: every
/// stream prepends `Lmax − latency` frames of silence so all tracks land on
/// the slowest plugin's time base (standard PDC). Content shifted past the
/// window end is carried into the next call, so the stream is seamless.
fn apply_latency_shift(window: &mut [f32], pending: &mut Vec<f32>) {
    if pending.is_empty() {
        return;
    }
    let mut combined = std::mem::take(pending);
    combined.extend_from_slice(window);
    let w = window.len();
    window.copy_from_slice(&combined[..w]);
    combined.drain(..w);
    *pending = combined;
}

pub struct StreamingRender<'p> {
    project: &'p Project,
    sr: u32,
    bpm: f32,
    step_seconds: f32,
    bars: usize,
    total_frames: usize,
    bar: usize,
    next_frame: usize,
    done: bool,
    err: Option<String>,
    included: Vec<usize>,
    tracks: Vec<StemStream>,
    suite: MasterSuiteState,
    /// Persistent master CLAP instance; tiled on the global 512-frame grid so
    /// chunked output stays bit-identical to the single-shot offline renderer.
    master_fx: Option<crate::clap::ClapFx>,
    /// Per-track CLAP instruments (same index order as `tracks`/`included`).
    synths: Vec<Option<crate::clap::ClapSynth>>,
    sample_cache: HashMap<String, SampleClip>,
    pattern_map: HashMap<&'p str, &'p Pattern>,
}

impl<'p> StreamingRender<'p> {
    pub fn new(project: &'p Project, sample_rate: u32) -> Self {
        let bpm = project.bpm.clamp(50, 200) as f32;
        let step_seconds = 60.0 / bpm / 4.0;
        let bars = project.arrangement.len().max(1);
        let total_frames =
            ((bars as f32 * 16.0 * step_seconds + 1.0) * sample_rate as f32).ceil() as usize;
        let _bar_frames = (16.0 * step_seconds * sample_rate as f32).round().max(1.0) as usize;
        let any_solo = project.tracks.iter().any(|t| t.solo);
        let included: Vec<usize> = project
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| !(t.mute || (any_solo && !t.solo)))
            .map(|(i, _)| i)
            .collect();
        // CLAP instruments come online once and stream across the whole bounce.
        let synths: Vec<Option<crate::clap::ClapSynth>> = included
            .iter()
            .map(|&ti| {
                let track = &project.tracks[ti];
                if track.kind != TrackKind::Plugin {
                    return None;
                }
                let insert = track.clap_instrument.as_ref().filter(|i| i.enabled)?;
                match crate::clap::ClapSynth::load(
                    std::path::Path::new(&insert.path),
                    &insert.plugin_id,
                    sample_rate,
                ) {
                    Ok(mut synth) => {
                        synth.apply_initial(&insert.params);
                        Some(synth)
                    }
                    Err(e) => {
                        eprintln!("CLAP instrument '{}' skipped: {e}", insert.name);
                        None
                    }
                }
            })
            .collect();
        // PDC: align every stream to the slowest plugin track.
        let latency_max = synths
            .iter()
            .filter_map(|s| s.as_ref().map(crate::clap::ClapSynth::latency))
            .max()
            .unwrap_or(0);
        let shift_seeds: Vec<Vec<f32>> = included
            .iter()
            .map(|&ti| {
                let lt = project.tracks[ti]
                    .clap_instrument
                    .as_ref()
                    .filter(|i| i.enabled)
                    .and_then(|_| {
                        included
                            .iter()
                            .position(|x| *x == ti)
                            .and_then(|pos| synths[pos].as_ref().map(|s| s.latency()))
                    })
                    .unwrap_or(0);
                vec![0.0_f32; (latency_max.saturating_sub(lt)) * 2]
            })
            .collect();
        let tracks = included
            .iter()
            .enumerate()
            .map(|(pos, i)| StemStream {
                fx: FxState::new(&project.tracks[*i].fx, sample_rate),
                pending: Vec::new(),
                shift_pending: shift_seeds[pos].clone(),
            })
            .collect();
        Self {
            project,
            sr: sample_rate,
            bpm,
            step_seconds,
            bars,
            total_frames,
            bar: 0,
            next_frame: 0,
            done: false,
            err: None,
            included,
            tracks,
            suite: MasterSuiteState::new(&project.master_suite, sample_rate),
            master_fx: master_fx_instance(project, sample_rate),
            synths,
            sample_cache: HashMap::new(),
            pattern_map: project
                .patterns
                .iter()
                .map(|p| (p.id.as_str(), p))
                .collect(),
        }
    }

    pub fn total_frames(&self) -> usize {
        self.total_frames
    }

    /// Up to `bar_frames` frames of finished master-chain stereo per chunk.
    /// None when everything (incl. the 1 s FX tail) has been emitted.
    pub fn next_chunk(&mut self) -> Option<Vec<f32>> {
        if self.done || self.err.is_some() {
            return None;
        }
        // Chunk boundaries track the *continuous* time lattice exactly (the same
        // formula onset_frame uses) — integer-accumulated windows would drift a
        // few samples per bar and drop hits that fall across the seam.
        let bar_start =
            |k: usize| ((k * STEPS) as f32 * self.step_seconds * self.sr as f32) as usize;
        if self.bar > self.bars {
            self.done = true;
            return None;
        }
        let start_frame = bar_start(self.bar);
        if start_frame >= self.total_frames {
            self.done = true;
            return None;
        }
        let w = if self.bar < self.bars {
            bar_start(self.bar + 1) - start_frame
        } else {
            self.total_frames - start_frame // tail chunk (FX reverb/delay decay)
        };
        let mut mix = vec![0.0_f32; w * 2];
        let bar = self.bar;

        // Per-track: drain pending overflow, add this bar's triggers, run FX.
        let mut processed: Vec<Vec<f32>> = Vec::with_capacity(self.tracks.len());
        let available = self.bars.saturating_sub(0); // bars * 16 steps of content
        let included = self.included.clone();
        for (track_pos, &track_index) in included.iter().enumerate() {
            let track = &self.project.tracks[track_index];
            let mut window = vec![0.0_f32; w * 2];
            {
                let stream = &mut self.tracks[track_pos];
                let take = (stream.pending.len() / 2).min(w);
                window[..take * 2].copy_from_slice(&stream.pending[..take * 2]);
                stream.pending.drain(..take * 2);
            }
            if bar < available {
                if let Err(e) =
                    self.add_bar_sources(track, bar, start_frame, &mut window, track_pos)
                {
                    self.err = Some(e);
                    return None;
                }
            }
            if self.synths[track_pos].is_some() {
                let events = self.collect_synth_events(track_index, start_frame, start_frame + w);
                if let Some(synth) = self.synths[track_pos].as_mut() {
                    synth.render_window(&mut window, start_frame, &events);
                }
            }
            apply_latency_shift(&mut window, &mut self.tracks[track_pos].shift_pending);
            self.tracks[track_pos].fx.process(&mut window);
            processed.push(window);
        }

        // Mix with automation — same per-frame math as the offline renderer.
        for (track_pos, window) in processed.iter().enumerate() {
            let track = &self.project.tracks[self.included[track_pos]];
            for f in 0..w {
                let abs = start_frame + f;
                let time = abs as f32 / self.sr as f32;
                let beat = time * self.bpm / 60.0;
                let auto_volume =
                    automation_value(self.project, &track.id, AutomationTarget::Volume, beat)
                        .unwrap_or(1.0);
                let auto_pan =
                    automation_value(self.project, &track.id, AutomationTarget::Pan, beat)
                        .unwrap_or(0.0);
                let pan = (track.pan + auto_pan).clamp(-1.0, 1.0);
                let left_gain = ((1.0 - pan) * 0.5).sqrt();
                let right_gain = ((1.0 + pan) * 0.5).sqrt();
                let gain = track.volume * auto_volume;
                let ix = f * 2;
                mix[ix] += window[ix] * gain * left_gain;
                mix[ix + 1] += window[ix + 1] * gain * right_gain;
            }
        }
        let master = self.project.master.clamp(0.0, 1.0);
        let drive = self.project.grit.clamp(0.0, 1.0);
        for sample in &mut mix {
            *sample = (*sample * master * (1.0 + drive * 3.0)).tanh();
        }
        self.suite.process(&mut mix);
        if let Some(fx) = self.master_fx.as_mut() {
            fx.process_chunk(&mut mix);
        }
        self.bar += 1;
        self.next_frame = start_frame + w;
        Some(mix)
    }

    /// Absolute-frame note events for a plugin track inside `[start, end)`.
    /// NOTE_ONs that began before the window were already delivered to the
    /// persistent instance by earlier chunks, so only events *landing* in the
    /// window are emitted — exactly what the offline renderer delivers in its
    /// single continuous pass.
    fn collect_synth_events(
        &self,
        track_index: usize,
        start: usize,
        end: usize,
    ) -> Vec<crate::clap::NoteEvent> {
        let track = &self.project.tracks[track_index];
        let bar_start =
            |k: usize| ((k * STEPS) as f32 * self.step_seconds * self.sr as f32) as usize;
        let mut evs = Vec::new();
        for (bar, slot) in self.project.arrangement.iter().enumerate() {
            if bar_start(bar) >= end {
                break; // nothing past this bar can start inside the window
            }
            let Some(id) = slot.as_ref() else {
                continue;
            };
            let Some(pattern) = self.pattern_map.get(id.as_str()) else {
                continue;
            };
            let Some(notes) = pattern.notes.get(&track.id) else {
                continue;
            };
            for note in notes {
                let on = onset_frame(
                    bar,
                    note.step as usize,
                    self.step_seconds,
                    self.project.swing,
                    self.sr,
                );
                if on >= end {
                    continue;
                }
                let off = onset_frame(
                    bar,
                    note.step as usize + note.duration.max(1) as usize,
                    self.step_seconds,
                    self.project.swing,
                    self.sr,
                );
                let key = note.pitch.min(127);
                if on >= start {
                    evs.push(crate::clap::NoteEvent {
                        abs_frame: on,
                        on: true,
                        key,
                        velocity: note.velocity as f32 / 127.0,
                    });
                }
                if off >= start && off < end {
                    evs.push(crate::clap::NoteEvent {
                        abs_frame: off,
                        on: false,
                        key,
                        velocity: 0.0,
                    });
                }
            }
        }
        evs
    }

    /// Synthesize every trigger belonging to `bar` into `window`, pushing any
    /// voice content beyond the window into that track's pending stream.
    fn add_bar_sources(
        &mut self,
        track: &crate::model::Track,
        bar: usize,
        window_start: usize,
        window: &mut [f32],
        track_pos: usize,
    ) -> std::result::Result<(), String> {
        let w = window.len() / 2;
        let Some(slot) = self.project.arrangement.get(bar) else {
            return Ok(());
        };
        let Some(id) = slot.as_ref() else {
            return Ok(());
        };
        let Some(pattern) = self.pattern_map.get(id.as_str()) else {
            return Ok(());
        };
        match track.kind {
            TrackKind::Kick
            | TrackKind::Snare
            | TrackKind::Hat
            | TrackKind::Cowbell
            | TrackKind::Sample => {
                let Some(row) = pattern.rows.get(&track.id) else {
                    return Ok(());
                };
                for step in 0..STEPS {
                    if !row.get(step).copied().unwrap_or(false) {
                        continue;
                    }
                    let start =
                        onset_frame(bar, step, self.step_seconds, self.project.swing, self.sr);
                    if track.kind == TrackKind::Sample {
                        let Some(sample) = track.sample.as_ref() else {
                            continue;
                        };
                        let clip = if let Some(c) = self.sample_cache.get(&sample.path) {
                            c.clone()
                        } else {
                            let decoded = load_wav(&sample.path).map_err(|e| e.to_string())?;
                            self.sample_cache
                                .insert(sample.path.clone(), decoded.clone());
                            decoded
                        };
                        let voice = trim_and_resample(
                            &clip,
                            sample,
                            self.sr,
                            self.step_seconds as f64 * self.sr as f64,
                        );
                        if sample.looped {
                            let next_step = ((step + 1)..STEPS)
                                .find(|next| row.get(*next).copied().unwrap_or(false))
                                .unwrap_or(STEPS);
                            let end = onset_frame(
                                bar,
                                next_step,
                                self.step_seconds,
                                self.project.swing,
                                self.sr,
                            );
                            add_voice_windowed(
                                &mut self.tracks[track_pos].pending,
                                self.total_frames,
                                window,
                                window_start,
                                start,
                                &voice,
                                Some(end),
                                sample.gain,
                            );
                        } else {
                            add_voice_windowed(
                                &mut self.tracks[track_pos].pending,
                                self.total_frames,
                                window,
                                window_start,
                                start,
                                &voice,
                                None,
                                sample.gain,
                            );
                        }
                    } else {
                        let voice =
                            synth_one_shot(track.kind, pattern.genre.as_str(), step, self.sr);
                        add_voice_windowed(
                            &mut self.tracks[track_pos].pending,
                            self.total_frames,
                            window,
                            window_start,
                            start,
                            &voice,
                            None,
                            1.0,
                        );
                    }
                }
            }
            TrackKind::Plugin => {
                // Events are delivered straight into next_chunk's persistent
                // ClapSynth instances (see collect_synth_events); nothing is
                // synthesized here.
            }
            TrackKind::Sub | TrackKind::Bass | TrackKind::Keys => {
                if let Some(notes) = pattern.notes.get(&track.id) {
                    for note in notes {
                        let start = onset_frame(
                            bar,
                            note.step as usize,
                            self.step_seconds,
                            self.project.swing,
                            self.sr,
                        );
                        let duration = (note.duration as f32 * self.step_seconds).max(0.04);
                        let voice =
                            note_voice(track.kind, note.pitch, duration, note.velocity, self.sr);
                        add_voice_windowed(
                            &mut self.tracks[track_pos].pending,
                            self.total_frames,
                            window,
                            window_start,
                            start,
                            &voice,
                            None,
                            1.0,
                        );
                    }
                }
            }
        }
        let _ = w;
        Ok(())
    }
}

/// Add a mono voice into this window; content beyond the window end is routed
/// into `pending` (same per-frame addition order as the offline build).
/// When `loop_end` is Some, the voice repeats from 0 until that absolute frame
/// (bar-local looped samples, mirroring add_looped_voice).
#[allow(clippy::too_many_arguments)]
fn add_voice_windowed(
    pending: &mut Vec<f32>,
    total_frames: usize,
    window: &mut [f32],
    window_start: usize,
    start_abs: usize,
    mono: &[f32],
    loop_end: Option<usize>,
    gain: f32,
) {
    let w = window.len() / 2;
    if start_abs < window_start {
        return;
    }
    let rel = start_abs - window_start;
    match loop_end {
        None => {
            for (i, sample) in mono.iter().enumerate() {
                let frame = rel + i;
                if frame >= total_frames {
                    break;
                }
                let v = *sample * gain;
                if frame < w {
                    window[frame * 2] += v;
                    window[frame * 2 + 1] += v;
                } else {
                    let over = (frame - w) * 2;
                    if pending.len() < over + 2 {
                        pending.resize(over + 2, 0.0);
                    }
                    pending[over] += v;
                    pending[over + 1] += v;
                }
            }
        }
        Some(end_abs) => {
            if mono.is_empty() {
                return;
            }
            let end_rel = end_abs.saturating_sub(window_start);
            let mut cursor = rel;
            while cursor < end_rel {
                let count = (end_rel - cursor).min(mono.len());
                for (offset, sample) in mono[..count].iter().enumerate() {
                    let frame = cursor + offset;
                    if frame >= total_frames {
                        return;
                    }
                    let v = *sample * gain;
                    if frame < w {
                        window[frame * 2] += v;
                        window[frame * 2 + 1] += v;
                    } else {
                        let over = (frame - w) * 2;
                        if pending.len() < over + 2 {
                            pending.resize(over + 2, 0.0);
                        }
                        pending[over] += v;
                        pending[over + 1] += v;
                    }
                }
                cursor += count;
            }
        }
    }
}

/// Background streaming producer: renders just-ahead chunks into a ring while
/// reporting progress. Memory stays flat regardless of arrangement length.
pub fn spawn_stream(
    project: Project,
    sample_rate: u32,
) -> (
    std::sync::mpsc::Receiver<RenderMsg>,
    rtrb::Consumer<f32>,
    usize,
    std::thread::JoinHandle<()>,
) {
    let ring_capacity = 1 << 18; // ~2.97 s of stereo at 44.1 kHz look-ahead
    let (mut producer, consumer) = rtrb::RingBuffer::<f32>::new(ring_capacity);
    let total = StreamingRender::new(&project, sample_rate).total_frames();
    let (tx, rx) = std::sync::mpsc::channel();
    let bars = project.arrangement.len().max(1) + 1; // bars + final tail chunk
    let handle = std::thread::spawn(move || {
        let mut engine = StreamingRender::new(&project, sample_rate);
        let mut sent_bars = 0usize;
        while let Some(chunk) = engine.next_chunk() {
            let mut offset = 0usize;
            while offset < chunk.len() {
                match producer.push(chunk[offset]) {
                    Ok(()) => offset += 1,
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(1)),
                }
            }
            sent_bars += 1;
            let _ = tx.send(RenderMsg::Progress {
                bar: sent_bars,
                bars,
            });
        }
        let _ = tx.send(RenderMsg::Done(Ok(Vec::new())));
    });
    (rx, consumer, total, handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn latency_shift_aligns_with_persistent_carry() {
        // 2-frame shift (4 interleaved samples of carry)
        let mut a: Vec<f32> = vec![0.0; 8];
        a[0] = 1.0;
        let mut pending = vec![0.0_f32; 4];
        apply_latency_shift(&mut a, &mut pending);
        assert_eq!(&a[..4], &[0.0; 4]);
        assert_eq!(a[4], 1.0);
        // A marker near the window end overflows into the carry, then spills
        // onto the next window's head — the seam stays seamless.
        a = vec![0.0; 8];
        a[6] = 1.0;
        apply_latency_shift(&mut a, &mut pending);
        assert!(a.iter().all(|v| *v == 0.0));
        assert_eq!(pending, vec![0.0, 0.0, 1.0, 0.0]);
        let mut b = vec![2.0_f32; 8];
        apply_latency_shift(&mut b, &mut pending);
        assert_eq!(b[2], 1.0);
        assert_eq!(&b[4..], &[2.0; 4]);
        // An empty carry is an identity shift (no-op fast path).
        let mut none: Vec<f32> = Vec::new();
        let before = b.clone();
        apply_latency_shift(&mut b, &mut none);
        assert_eq!(b, before);
    }

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
            out.len().is_multiple_of(2) && peak <= 0.4 + 1e-3,
            "render peak {peak} over suite ceiling"
        );
    }

    #[test]
    fn progressive_render_is_bit_identical_and_reports_monotonic_progress() {
        // THE streaming-correctness proof: the progress-reporting variant must
        // produce the exact same bytes as plain render(), with monotonic progress.
        let project = default_project("iftar");
        let plain = render(&project, 44_100).unwrap();
        let mut marks = Vec::new();
        let progressive =
            render_with_progress(&project, 44_100, Some(&mut |p| marks.push(p))).unwrap();
        assert_eq!(
            progressive.len(),
            plain.len(),
            "streamed render diverged in length"
        );
        assert_eq!(progressive, plain, "streamed render is not bit-identical");
        assert!(
            marks.len() >= project.arrangement.len(),
            "progress not reported per bar"
        );
        assert!(
            marks.windows(2).all(|w| w[0] <= w[1]),
            "progress not monotonic"
        );
        assert!(marks.iter().all(|m| (0.0..=1.0).contains(m)));
    }

    #[test]
    fn streaming_render_is_bit_identical_to_offline_core() {
        // THE streaming correctness contract, proven across every studio floor
        // and every arrangement prefix: chunk concatenation == whole render,
        // sample for sample. Covers drum rows, pitched lanes, loops, tails.
        for studio in ["iftar", "drift", "boogie", "rhythm"] {
            let full = default_project(studio);
            let full_ref = render_core(&full, 44_100).unwrap();
            for keep in 1..=full.arrangement.len() {
                let mut project = full.clone();
                project.arrangement.truncate(keep);
                let reference = render_core(&project, 44_100).unwrap();
                let mut engine = StreamingRender::new(&project, 44_100);
                let mut streamed = Vec::new();
                while let Some(chunk) = engine.next_chunk() {
                    streamed.extend_from_slice(&chunk);
                }
                assert_eq!(engine.err, None, "{studio}/{keep}: stream error");
                assert_eq!(
                    streamed.len(),
                    reference.len(),
                    "{studio}/{keep}: streamed length diverged"
                );
                assert_eq!(streamed, reference, "{studio}/{keep}: NOT bit-identical");
            }
            // determinism cross-checks on the same instance
            assert_eq!(
                render_core(&full, 44_100).unwrap(),
                full_ref,
                "{studio}: core not deterministic"
            );
        }
    }

    #[test]
    fn sample_fit_steps_warps_length_to_grid() {
        let clip = SampleClip {
            sample_rate: 44_100,
            channels: 1,
            frames: vec![0.5; 8_820],
        }; // 0.2 s
        let mut reference = crate::model::SampleRef::new("/tmp/x.wav".into());
        reference.gain = 1.0;
        reference.fit_steps = 2;
        let step_seconds = 60.0 / 120.0 / 4.0; // 0.125 s
        let step_frames = step_seconds * 44_100.0; // 5512.5
        let voice = trim_and_resample(&clip, &reference, 44_100, step_frames);
        let expect = (2.0 * step_frames) as usize;
        assert_eq!(
            voice.len(),
            expect,
            "fit_steps did not match the grid exactly"
        );
        reference.fit_steps = 0;
        let natural = trim_and_resample(&clip, &reference, 44_100, step_frames);
        assert_eq!(
            natural.len(),
            8_820,
            "fit_steps=0 must leave the take untouched"
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
