use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const STEPS: usize = 16;
pub const SAMPLE_RATE: u32 = 44_100;
pub const PROJECT_FORMAT: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    Kick,
    Snare,
    Hat,
    Cowbell,
    Sub,
    Bass,
    Keys,
    Sample,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Track {
    pub id: String,
    pub name: String,
    pub kind: TrackKind,
    pub color: String,
    pub volume: f32,
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    pub fx: FxChain,
    pub sample: Option<SampleRef>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FxChain {
    pub lowpass_hz: f32,
    pub drive: f32,
    pub delay_mix: f32,
    pub delay_feedback: f32,
    pub delay_ms: f32,
    pub reverb_mix: f32,
}
impl Default for FxChain {
    fn default() -> Self {
        Self {
            lowpass_hz: 20_000.0,
            drive: 0.0,
            delay_mix: 0.0,
            delay_feedback: 0.28,
            delay_ms: 280.0,
            reverb_mix: 0.0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SampleRef {
    /// Absolute while editing; made project-relative when the project is saved.
    pub path: String,
    pub trim_start: f32,
    pub trim_end: f32,
    pub gain: f32,
    pub looped: bool,
    /// Varispeed "warp": squeeze this take into exactly `fit_steps` 16ths
    /// (0 = natural length). MPC-style length fit — pitch follows speed.
    #[serde(default)]
    pub fit_steps: u8,
}
impl SampleRef {
    pub fn new(path: String) -> Self {
        Self {
            path,
            trim_start: 0.0,
            trim_end: 1.0,
            fit_steps: 0,
            gain: 1.0,
            looped: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Note {
    pub step: u16,
    pub pitch: u8,
    pub duration: u16,
    pub velocity: u8,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pattern {
    pub id: String,
    pub name: String,
    pub genre: String,
    pub bpm: u16,
    pub rows: BTreeMap<String, Vec<bool>>,
    pub notes: BTreeMap<String, Vec<Note>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationTarget {
    Volume,
    Pan,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AutomationPoint {
    pub beat: f32,
    pub value: f32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AutomationLane {
    pub id: String,
    pub track_id: String,
    pub target: AutomationTarget,
    pub points: Vec<AutomationPoint>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WavBitDepth {
    #[default]
    Sixteen,
    TwentyFour,
    Float32,
}

/// Master-bus mastering chain: 3-band EQ → stereo width → compressor → limiter.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MasterSuite {
    pub eq_enabled: bool,
    pub eq_low_db: f32,
    pub eq_mid_db: f32,
    pub eq_high_db: f32,
    pub comp_enabled: bool,
    pub comp_threshold_db: f32,
    pub comp_ratio: f32,
    pub comp_makeup_db: f32,
    pub limiter_enabled: bool,
    pub limiter_ceiling: f32,
    pub stereo_width: f32,
}
impl Default for MasterSuite {
    fn default() -> Self {
        Self {
            eq_enabled: true,
            eq_low_db: 0.0,
            eq_mid_db: 0.0,
            eq_high_db: 0.0,
            comp_enabled: true,
            comp_threshold_db: -14.0,
            comp_ratio: 3.0,
            comp_makeup_db: 3.0,
            limiter_enabled: true,
            limiter_ceiling: 0.98,
            stereo_width: 1.0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    pub format_version: u32,
    pub studio_id: String,
    pub name: String,
    pub bpm: u16,
    /// Swing percentage, 0 through 60.
    pub swing: u8,
    pub master: f32,
    pub grit: f32,
    pub tracks: Vec<Track>,
    pub patterns: Vec<Pattern>,
    pub arrangement: Vec<Option<String>>,
    pub selected_pattern_id: String,
    pub automation: Vec<AutomationLane>,
    #[serde(default)]
    pub master_suite: MasterSuite,
    #[serde(default)]
    pub export_bit_depth: WavBitDepth,
    /// 44100 or 48000; 0/invalid falls back to 44100 at use time.
    #[serde(default)]
    pub export_sample_rate: u32,
}

#[derive(Clone, Copy)]
pub struct StudioSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub title: &'static str,
    pub genre: &'static str,
    pub color: &'static str,
    pub portrait: &'static str,
    pub tagline: &'static str,
    pub description: &'static str,
}
pub const STUDIOS: [StudioSpec; 3] = [
    StudioSpec {
        id: "nyx",
        name: "DRIFT",
        title: "Phonk Studio",
        genre: "PHONK",
        color: "#B58CFF",
        portrait: "drift-portrait.png",
        tagline: "After dark, the low end leads.",
        description: "Cowbell-led grooves, Memphis drums, tape grit, and heavy sub.",
    },
    StudioSpec {
        id: "hermes",
        name: "BOOGIE",
        title: "Funk Studio",
        genre: "FUNK",
        color: "#E7C779",
        portrait: "boogie-portrait.png",
        tagline: "Find the pocket. Keep it moving.",
        description: "Syncopated bass, pocket drums, and warm chord stabs.",
    },
    StudioSpec {
        id: "alexander",
        name: "RHYTHM",
        title: "Beat Lab",
        genre: "HYBRID",
        color: "#82C7C5",
        portrait: "rhythm-portrait.png",
        tagline: "Build a sound that marches forward.",
        description: "An open canvas for hybrid, half-time, and club patterns.",
    },
];

fn track(id: &str, name: &str, kind: TrackKind, color: &str) -> Track {
    Track {
        id: id.into(),
        name: name.into(),
        kind,
        color: color.into(),
        volume: 0.82,
        pan: 0.0,
        mute: false,
        solo: false,
        fx: FxChain::default(),
        sample: None,
    }
}
pub fn standard_tracks() -> Vec<Track> {
    vec![
        track("kick", "Kick", TrackKind::Kick, "#B8E58A"),
        track("snare", "Snare", TrackKind::Snare, "#F08D79"),
        track("hat", "Closed hat", TrackKind::Hat, "#79B7FF"),
        track("cowbell", "Cowbell", TrackKind::Cowbell, "#B58CFF"),
        track("sub", "Sub 808", TrackKind::Sub, "#72D6C7"),
        track("bass", "Funk bass", TrackKind::Bass, "#E7C779"),
        track("keys", "Keys", TrackKind::Keys, "#EAA1C7"),
    ]
}

const PHONK_DEGREES: [i8; STEPS] = [0, 0, 3, 0, 7, 3, 0, -2, 0, 3, 7, 3, 0, -2, 3, 0];
const FUNK_DEGREES: [i8; STEPS] = [0, 7, 0, 3, 0, 5, 7, 3, 0, 7, 10, 7, 0, 3, 5, 7];

fn pattern(id: &str, name: &str, genre: &str, bpm: u16, rows: &[(&str, &[usize])]) -> Pattern {
    let mut steps: BTreeMap<String, Vec<bool>> = standard_tracks()
        .into_iter()
        .map(|t| (t.id, vec![false; STEPS]))
        .collect();
    for (track_id, positions) in rows {
        let row = steps
            .entry((*track_id).into())
            .or_insert_with(|| vec![false; STEPS]);
        for &step in *positions {
            if step < STEPS {
                row[step] = true;
            }
        }
    }
    let mut notes: BTreeMap<String, Vec<Note>> = ["sub", "bass", "keys"]
        .into_iter()
        .map(|id| (id.into(), Vec::new()))
        .collect();
    let degrees = if genre == "FUNK" {
        &FUNK_DEGREES
    } else {
        &PHONK_DEGREES
    };
    for id in ["sub", "bass", "keys"] {
        let root: i16 = match (id, genre) {
            ("sub", "FUNK") => 33,
            ("sub", "HYBRID") => 41,
            ("sub", _) => 38,
            ("bass", "FUNK") => 45,
            ("bass", "HYBRID") => 48,
            ("bass", _) => 38,
            ("keys", "FUNK") => 57,
            ("keys", "HYBRID") => 53,
            _ => 50,
        };
        let duration = if id == "keys" { 3 } else { 2 };
        for (step, degree) in degrees.iter().enumerate() {
            if steps
                .get(id)
                .and_then(|r| r.get(step))
                .copied()
                .unwrap_or(false)
            {
                let pitch = (root + *degree as i16).clamp(0, 127) as u8;
                let intervals: &[u8] = if id == "keys" { &[0, 3, 7] } else { &[0] };
                for interval in intervals {
                    notes.get_mut(id).unwrap().push(Note {
                        step: step as u16,
                        pitch: pitch.saturating_add(*interval),
                        duration,
                        velocity: if id == "sub" { 100 } else { 92 },
                    });
                }
            }
        }
    }
    Pattern {
        id: id.into(),
        name: name.into(),
        genre: genre.into(),
        bpm,
        rows: steps,
        notes,
    }
}

pub fn default_project(studio_id: &str) -> Project {
    let spec = STUDIOS
        .iter()
        .find(|s| s.id == studio_id)
        .unwrap_or(&STUDIOS[0]);
    let patterns = match spec.id {
        "hermes" => vec![
            pattern(
                "hermes-pocket",
                "Pocket Funk",
                "FUNK",
                106,
                &[
                    ("kick", &[0, 5, 8, 11, 14]),
                    ("snare", &[4, 12]),
                    ("hat", &[0, 2, 4, 6, 8, 10, 12, 14]),
                    ("sub", &[0, 6, 8, 14]),
                    ("bass", &[0, 3, 5, 7, 8, 11, 14]),
                    ("keys", &[0, 6, 8, 14]),
                ],
            ),
            pattern(
                "hermes-brass",
                "Brass Stomp",
                "FUNK",
                112,
                &[
                    ("kick", &[0, 3, 8, 10, 14]),
                    ("snare", &[4, 12]),
                    ("hat", &[0, 2, 4, 6, 8, 10, 12, 14]),
                    ("sub", &[0, 5, 8, 13]),
                    ("bass", &[0, 3, 6, 7, 8, 11, 13, 15]),
                    ("keys", &[0, 5, 8, 13]),
                ],
            ),
            pattern(
                "hermes-boogie",
                "After Hours",
                "FUNK",
                98,
                &[
                    ("kick", &[0, 6, 8, 11]),
                    ("snare", &[4, 12]),
                    ("hat", &[0, 2, 4, 6, 8, 10, 12, 14]),
                    ("sub", &[0, 7, 8, 14]),
                    ("bass", &[0, 2, 5, 7, 8, 10, 13, 15]),
                    ("keys", &[0, 4, 8, 12]),
                ],
            ),
        ],
        "alexander" => vec![
            pattern(
                "alex-vanguard",
                "Vanguard",
                "HYBRID",
                120,
                &[
                    ("kick", &[0, 4, 8, 12]),
                    ("snare", &[4, 12]),
                    ("hat", &[0, 2, 4, 6, 8, 10, 12, 14]),
                    ("cowbell", &[2, 6, 10, 14]),
                    ("sub", &[0, 7, 8, 11]),
                    ("bass", &[0, 3, 5, 8, 11, 14]),
                    ("keys", &[0, 6, 8, 14]),
                ],
            ),
            pattern(
                "alex-march",
                "Half-time March",
                "HYBRID",
                96,
                &[
                    ("kick", &[0, 6, 8, 14]),
                    ("snare", &[4, 12]),
                    ("hat", &[0, 2, 4, 6, 8, 10, 12, 14]),
                    ("cowbell", &[0, 8]),
                    ("sub", &[0, 6, 8, 14]),
                    ("bass", &[0, 5, 8, 13]),
                    ("keys", &[0, 8]),
                ],
            ),
            pattern(
                "alex-signal",
                "Signal Fire",
                "HYBRID",
                128,
                &[
                    ("kick", &[0, 3, 8, 10, 12]),
                    ("snare", &[4, 12]),
                    ("hat", &[0, 2, 4, 6, 8, 10, 12, 14]),
                    ("cowbell", &[3, 7, 11, 15]),
                    ("sub", &[0, 5, 8, 11, 14]),
                    ("bass", &[0, 3, 8, 11]),
                    ("keys", &[0, 7, 8, 15]),
                ],
            ),
        ],
        _ => vec![
            pattern(
                "nyx-drift",
                "Drift Phonk",
                "PHONK",
                144,
                &[
                    ("kick", &[0, 3, 8, 11]),
                    ("snare", &[4, 12]),
                    ("hat", &[0, 2, 4, 6, 8, 10, 12, 14]),
                    ("cowbell", &[0, 3, 6, 8, 10, 13, 15]),
                    ("sub", &[0, 6, 8, 11, 14]),
                ],
            ),
            pattern(
                "nyx-memphis",
                "Memphis Tape",
                "PHONK",
                132,
                &[
                    ("kick", &[0, 6, 8, 10, 14]),
                    ("snare", &[4, 12]),
                    ("hat", &[0, 2, 3, 4, 6, 8, 10, 12, 14]),
                    ("cowbell", &[1, 4, 7, 9, 12, 15]),
                    ("sub", &[0, 3, 8, 11, 14]),
                    ("keys", &[0, 8]),
                ],
            ),
            pattern(
                "nyx-slowburn",
                "Slow Burn",
                "PHONK",
                88,
                &[
                    ("kick", &[0, 7, 8, 14]),
                    ("snare", &[4, 12]),
                    ("hat", &[0, 3, 6, 8, 11, 14]),
                    ("cowbell", &[0, 4, 7, 11, 15]),
                    ("sub", &[0, 7, 8, 15]),
                ],
            ),
        ],
    };
    let selected = patterns[0].id.clone();
    let bpm = patterns[0].bpm;
    Project {
        format_version: PROJECT_FORMAT,
        studio_id: spec.id.into(),
        name: format!("{} Session 01", title_case(spec.name)),
        bpm,
        swing: 12,
        master: 0.78,
        grit: 0.18,
        tracks: standard_tracks(),
        patterns,
        arrangement: vec![Some(selected.clone()); 8],
        selected_pattern_id: selected,
        automation: Vec::new(),
        master_suite: MasterSuite::default(),
        export_bit_depth: WavBitDepth::default(),
        export_sample_rate: 44_100,
    }
}

fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str().to_lowercase().as_str(),
    }
}

impl Project {
    pub fn selected_pattern(&self) -> Option<&Pattern> {
        self.patterns
            .iter()
            .find(|p| p.id == self.selected_pattern_id)
    }
    pub fn selected_pattern_mut(&mut self) -> Option<&mut Pattern> {
        self.patterns
            .iter_mut()
            .find(|p| p.id == self.selected_pattern_id)
    }
    pub fn pattern(&self, id: &str) -> Option<&Pattern> {
        self.patterns.iter().find(|p| p.id == id)
    }
    pub fn validate(&mut self) -> anyhow::Result<()> {
        if self.patterns.is_empty() {
            anyhow::bail!("Project has no patterns.");
        }
        if self.tracks.is_empty() {
            self.tracks = standard_tracks();
        }
        self.bpm = self.bpm.clamp(50, 200);
        self.swing = self.swing.min(60);
        self.master = self.master.clamp(0.0, 1.0);
        self.grit = self.grit.clamp(0.0, 1.0);
        let ids: Vec<String> = self.patterns.iter().map(|p| p.id.clone()).collect();
        for pattern in &mut self.patterns {
            if pattern.id.is_empty() {
                pattern.id = format!("pattern-{}", ids.len());
            }
            pattern.bpm = pattern.bpm.clamp(50, 200);
            for track in &self.tracks {
                let row = pattern
                    .rows
                    .entry(track.id.clone())
                    .or_insert_with(|| vec![false; STEPS]);
                row.resize(STEPS, false);
                row.truncate(STEPS);
                if matches!(
                    track.kind,
                    TrackKind::Sub | TrackKind::Bass | TrackKind::Keys
                ) {
                    pattern.notes.entry(track.id.clone()).or_default();
                }
            }
            for events in pattern.notes.values_mut() {
                events.retain(|n| n.step < STEPS as u16);
                for note in events {
                    note.pitch = note.pitch.min(127);
                    note.duration = note.duration.clamp(1, 64);
                    note.velocity = note.velocity.clamp(1, 127);
                }
            }
            for (track_id, events) in &pattern.notes {
                if let Some(row) = pattern.rows.get_mut(track_id) {
                    for value in row.iter_mut() {
                        *value = false;
                    }
                    for note in events {
                        if let Some(v) = row.get_mut(note.step as usize) {
                            *v = true;
                        }
                    }
                }
            }
        }
        if !ids.contains(&self.selected_pattern_id) {
            self.selected_pattern_id = ids[0].clone();
        }
        let valid: Vec<String> = self.patterns.iter().map(|p| p.id.clone()).collect();
        for id in &mut self.arrangement {
            if id.as_ref().is_some_and(|v| !valid.contains(v)) {
                *id = None;
            }
        }
        if self.arrangement.len() < 4 {
            self.arrangement
                .resize(4, Some(self.selected_pattern_id.clone()));
        }
        if self.arrangement.len() > 64 {
            self.arrangement.truncate(64);
        }
        for track in &mut self.tracks {
            track.volume = track.volume.clamp(0.0, 1.0);
            track.pan = track.pan.clamp(-1.0, 1.0);
            track.fx.lowpass_hz = track.fx.lowpass_hz.clamp(40.0, 20_000.0);
            track.fx.drive = track.fx.drive.clamp(0.0, 1.0);
            track.fx.delay_mix = track.fx.delay_mix.clamp(0.0, 0.8);
            track.fx.delay_feedback = track.fx.delay_feedback.clamp(0.0, 0.9);
            track.fx.delay_ms = track.fx.delay_ms.clamp(20.0, 2000.0);
            track.fx.reverb_mix = track.fx.reverb_mix.clamp(0.0, 0.8);
            if let Some(sample) = track.sample.as_mut() {
                sample.trim_start = sample.trim_start.clamp(0.0, 0.99);
                sample.trim_end = sample
                    .trim_end
                    .clamp(0.01, 1.0)
                    .max(sample.trim_start + 0.01)
                    .min(1.0);
                sample.gain = sample.gain.clamp(0.0, 4.0);
            }
        }
        self.automation
            .retain(|lane| self.tracks.iter().any(|t| t.id == lane.track_id));
        for lane in &mut self.automation {
            lane.points
                .retain(|p| p.beat.is_finite() && p.value.is_finite());
            lane.points.sort_by(|a, b| a.beat.total_cmp(&b.beat));
            for p in &mut lane.points {
                p.beat = p.beat.max(0.0);
                p.value = p.value.clamp(-1.0, 1.0);
            }
        }
        Ok(())
    }

    pub fn new_pattern(&mut self, duplicate: bool) {
        let source = self.selected_pattern().cloned();
        let num = self.patterns.len() + 1;
        let id = format!("{}-pattern-{}", self.studio_id, num);
        let mut p = if duplicate {
            source
                .clone()
                .unwrap_or_else(|| blank_pattern(&id, self.bpm))
        } else {
            blank_pattern(&id, self.bpm)
        };
        p.id = id.clone();
        p.name = format!("Pattern {num}");
        if !duplicate {
            p.genre = "CUSTOM".into();
            p.bpm = self.bpm;
        }
        self.patterns.push(p);
        self.selected_pattern_id = id;
        for slot in &mut self.arrangement {
            if slot.is_none() {
                *slot = Some(self.selected_pattern_id.clone());
            }
        }
    }
}

pub fn blank_pattern(id: &str, bpm: u16) -> Pattern {
    let rows = standard_tracks()
        .into_iter()
        .map(|t| (t.id, vec![false; STEPS]))
        .collect();
    let notes = ["sub", "bass", "keys"]
        .into_iter()
        .map(|id| (id.into(), Vec::new()))
        .collect();
    Pattern {
        id: id.into(),
        name: "Untitled Pattern".into(),
        genre: "CUSTOM".into(),
        bpm,
        rows,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn three_studios_have_three_patterns_and_eight_bars() {
        for s in STUDIOS {
            let p = default_project(s.id);
            assert_eq!(p.patterns.len(), 3);
            assert_eq!(p.arrangement.len(), 8);
        }
    }
    #[test]
    fn validation_clamps_and_syncs_piano_rows() {
        let mut p = default_project("nyx");
        p.bpm = 400;
        p.patterns[0].notes.get_mut("sub").unwrap().push(Note {
            step: 15,
            pitch: 250,
            duration: 0,
            velocity: 0,
        });
        p.validate().unwrap();
        assert_eq!(p.bpm, 200);
        let n = p.patterns[0].notes["sub"].last().unwrap();
        assert_eq!((n.pitch, n.duration, n.velocity), (127, 1, 1));
        assert!(p.patterns[0].rows["sub"][15]);
    }
}
