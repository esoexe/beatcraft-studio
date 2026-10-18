use crate::model::{Note, Pattern, Project, TrackKind, STEPS};
use anyhow::{Context, Result};
use midly::{
    num::{u15, u28, u4, u7},
    Format, Header, MetaMessage, MidiMessage, Smf, Timing, TrackEvent, TrackEventKind,
};
use std::{collections::HashMap, fs, path::Path};

const PPQ: u64 = 480;
const TICKS_PER_STEP: u64 = PPQ / 4;

pub fn export_midi(project: &Project, path: impl AsRef<Path>) -> Result<usize> {
    let mut midi_tracks: Vec<Vec<TrackEvent<'_>>> = Vec::new();
    let mut tempo_track = Vec::new();
    tempo_track.push(TrackEvent {
        delta: u28::new(0),
        kind: TrackEventKind::Meta(MetaMessage::TrackName(b"BeatCraft tempo")),
    });
    let bpm = project.bpm.max(1) as u32;
    let micros = (60_000_000 / bpm).min(0xFF_FFFF);
    tempo_track.push(TrackEvent {
        delta: u28::new(0),
        kind: TrackEventKind::Meta(MetaMessage::Tempo(midly::num::u24::new(micros))),
    });
    tempo_track.push(TrackEvent {
        delta: u28::new(0),
        kind: TrackEventKind::Meta(MetaMessage::TimeSignature(4, 2, 24, 8)),
    });
    midi_tracks.push(tempo_track);
    let mut event_count = 3;
    let pattern_map: HashMap<&str, &Pattern> = project
        .patterns
        .iter()
        .map(|p| (p.id.as_str(), p))
        .collect();
    let any_solo = project.tracks.iter().any(|t| t.solo);
    let mut bars = Vec::new();
    for slot in &project.arrangement {
        bars.push(
            slot.as_ref()
                .and_then(|id| pattern_map.get(id.as_str()).copied()),
        );
    }

    for (track_index, track) in project.tracks.iter().enumerate() {
        let mut events: Vec<(u64, u8, TrackEventKind<'_>)> = Vec::new();
        events.push((
            0,
            0,
            TrackEventKind::Meta(MetaMessage::TrackName(track.name.as_bytes())),
        ));
        if track.mute || (any_solo && !track.solo) || track.volume <= 0.0 {
            midi_tracks.push(vec![TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
            }]);
            continue;
        }
        let channel = if matches!(
            track.kind,
            TrackKind::Kick
                | TrackKind::Snare
                | TrackKind::Hat
                | TrackKind::Cowbell
                | TrackKind::Sample
        ) {
            9
        } else {
            track_index.min(8) as u8
        };
        for (bar_index, pattern) in bars.iter().enumerate() {
            let Some(pattern) = pattern else {
                continue;
            };
            for step in 0..STEPS {
                let swing = if step % 2 == 1 {
                    (TICKS_PER_STEP as f32 * project.swing as f32 / 100.0 * 0.46).round() as u64
                } else {
                    0
                };
                let start = (bar_index as u64 * 16 + step as u64) * TICKS_PER_STEP + swing;
                match track.kind {
                    TrackKind::Kick | TrackKind::Snare | TrackKind::Hat | TrackKind::Cowbell => {
                        if pattern
                            .rows
                            .get(&track.id)
                            .and_then(|r| r.get(step))
                            .copied()
                            .unwrap_or(false)
                        {
                            let note = match track.kind {
                                TrackKind::Kick => 36,
                                TrackKind::Snare => 38,
                                TrackKind::Hat => 42,
                                TrackKind::Cowbell => 56,
                                _ => 60,
                            };
                            let vel = (100.0 * track.volume).round().clamp(1.0, 127.0) as u8;
                            events.push((
                                start,
                                1,
                                TrackEventKind::Midi {
                                    channel: u4::new(9),
                                    message: MidiMessage::NoteOn {
                                        key: u7::new(note),
                                        vel: u7::new(vel),
                                    },
                                },
                            ));
                            events.push((
                                start + TICKS_PER_STEP / 2,
                                0,
                                TrackEventKind::Midi {
                                    channel: u4::new(9),
                                    message: MidiMessage::NoteOff {
                                        key: u7::new(note),
                                        vel: u7::new(0),
                                    },
                                },
                            ));
                            event_count += 2;
                        }
                    }
                    TrackKind::Sub | TrackKind::Bass | TrackKind::Keys => {
                        if let Some(notes) = pattern.notes.get(&track.id) {
                            for note in notes.iter().filter(|n| n.step as usize == step) {
                                let vel = (note.velocity as f32 * track.volume)
                                    .round()
                                    .clamp(1.0, 127.0)
                                    as u8;
                                let pitch = note.pitch.min(127);
                                events.push((
                                    start,
                                    1,
                                    TrackEventKind::Midi {
                                        channel: u4::new(channel),
                                        message: MidiMessage::NoteOn {
                                            key: u7::new(pitch),
                                            vel: u7::new(vel),
                                        },
                                    },
                                ));
                                events.push((
                                    start + note.duration.max(1) as u64 * TICKS_PER_STEP,
                                    0,
                                    TrackEventKind::Midi {
                                        channel: u4::new(channel),
                                        message: MidiMessage::NoteOff {
                                            key: u7::new(pitch),
                                            vel: u7::new(0),
                                        },
                                    },
                                ));
                                event_count += 2;
                            }
                        }
                    }
                    TrackKind::Sample => {
                        if pattern
                            .rows
                            .get(&track.id)
                            .and_then(|r| r.get(step))
                            .copied()
                            .unwrap_or(false)
                        {
                            events.push((
                                start,
                                1,
                                TrackEventKind::Midi {
                                    channel: u4::new(9),
                                    message: MidiMessage::NoteOn {
                                        key: u7::new(60),
                                        vel: u7::new(90),
                                    },
                                },
                            ));
                            events.push((
                                start + TICKS_PER_STEP / 2,
                                0,
                                TrackEventKind::Midi {
                                    channel: u4::new(9),
                                    message: MidiMessage::NoteOff {
                                        key: u7::new(60),
                                        vel: u7::new(0),
                                    },
                                },
                            ));
                            event_count += 2;
                        }
                    }
                }
            }
        }
        events.sort_by_key(|e| (e.0, e.1));
        let mut tick = 0_u64;
        let mut out = Vec::with_capacity(events.len() + 1);
        for (at, _, kind) in events {
            out.push(TrackEvent {
                delta: u28::new(at.saturating_sub(tick).min(0x0FFF_FFFF) as u32),
                kind,
            });
            tick = at;
        }
        out.push(TrackEvent {
            delta: u28::new(0),
            kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
        });
        midi_tracks.push(out);
    }
    let smf = Smf {
        header: Header::new(Format::Parallel, Timing::Metrical(u15::new(PPQ as u16))),
        tracks: midi_tracks,
    };
    let mut bytes = Vec::new();
    smf.write_std(&mut bytes)
        .context("Could not encode MIDI file")?;
    fs::write(path, bytes).context("Could not write MIDI file")?;
    Ok(event_count)
}

pub fn import_midi(path: impl AsRef<Path>, project: &mut Project) -> Result<usize> {
    let bytes = fs::read(path).context("Could not read MIDI file")?;
    let smf = Smf::parse(&bytes).context("File is not a valid Standard MIDI File")?;
    let ticks_per_beat = match smf.header.timing {
        Timing::Metrical(t) => t.as_int() as u64,
        Timing::Timecode(_, _) => anyhow::bail!("SMPTE-timed MIDI files are not supported yet."),
    };
    let ticks_per_step = (ticks_per_beat / 4).max(1);
    let bar_ticks = ticks_per_step * 16;
    let mut bpm = project.bpm;
    let mut collected: Vec<(u64, u64, u8, u8, u8, String)> = Vec::new();
    for track in &smf.tracks {
        let mut tick = 0_u64;
        let mut name = String::new();
        let mut open: HashMap<(u8, u8), Vec<(u64, u8)>> = HashMap::new();
        for event in track {
            tick = tick.saturating_add(event.delta.as_int() as u64);
            match event.kind {
                TrackEventKind::Meta(MetaMessage::Tempo(us)) => {
                    if let Some(tempo_bpm) = 60_000_000_u32.checked_div(us.as_int()) {
                        bpm = tempo_bpm.clamp(50, 200) as u16;
                    }
                }
                TrackEventKind::Meta(MetaMessage::TrackName(bytes)) => {
                    name = String::from_utf8_lossy(bytes).to_string()
                }
                TrackEventKind::Midi { channel, message } => match message {
                    MidiMessage::NoteOn { key, vel } if vel.as_int() > 0 => open
                        .entry((channel.as_int(), key.as_int()))
                        .or_default()
                        .push((tick, vel.as_int())),
                    MidiMessage::NoteOff { key, .. } => {
                        let channel = channel.as_int();
                        let key = key.as_int();
                        if let Some(stack) = open.get_mut(&(channel, key)) {
                            if let Some((start, velocity)) = stack.pop() {
                                collected.push((
                                    start,
                                    tick.max(start + 1),
                                    channel,
                                    key,
                                    velocity,
                                    name.clone(),
                                ));
                            }
                        }
                    }
                    MidiMessage::NoteOn { key, vel } if vel.as_int() == 0 => {
                        let channel = channel.as_int();
                        let key = key.as_int();
                        if let Some(stack) = open.get_mut(&(channel, key)) {
                            if let Some((start, velocity)) = stack.pop() {
                                collected.push((
                                    start,
                                    tick.max(start + 1),
                                    channel,
                                    key,
                                    velocity,
                                    name.clone(),
                                ));
                            }
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        for ((channel, key), stack) in open {
            for (start, velocity) in stack {
                collected.push((
                    start,
                    start + ticks_per_step,
                    channel,
                    key,
                    velocity,
                    name.clone(),
                ));
            }
        }
    }
    if collected.is_empty() {
        anyhow::bail!("The MIDI file has no note events to import.");
    }
    let num_bars = collected
        .iter()
        .map(|e| (e.0 / bar_ticks) as usize)
        .max()
        .unwrap_or(0)
        .saturating_add(1)
        .clamp(1, 64);
    let import_nonce = project.patterns.len();
    let mut imported = Vec::with_capacity(num_bars);
    for bar in 0..num_bars {
        let id = format!(
            "midi-import-{}-{}-{}",
            std::process::id(),
            import_nonce,
            bar + 1
        );
        let mut pattern = crate::model::blank_pattern(&id, bpm);
        pattern.name = format!("MIDI Bar {:02}", bar + 1);
        pattern.genre = "CUSTOM".into();
        pattern.bpm = bpm;
        imported.push(pattern);
    }
    let mut count = 0;
    for (start, end, channel, key, velocity, track_name) in collected {
        let bar = (start / bar_ticks) as usize;
        if bar >= imported.len() {
            continue;
        }
        let step = ((start % bar_ticks) * 16 / bar_ticks) as usize;
        let duration = (end - start).div_ceil(ticks_per_step).clamp(1, 64) as u16;
        let track_id = if channel == 9 {
            match key {
                36 => "kick",
                38 => "snare",
                42 => "hat",
                56 => "cowbell",
                _ => "cowbell",
            }
        } else if track_name.to_lowercase().contains("sub") {
            "sub"
        } else if track_name.to_lowercase().contains("bass") {
            "bass"
        } else {
            "keys"
        };
        if track_id == "kick" || track_id == "snare" || track_id == "hat" || track_id == "cowbell" {
            if let Some(row) = imported[bar].rows.get_mut(track_id) {
                row[step] = true;
            }
        } else {
            imported[bar]
                .notes
                .entry(track_id.into())
                .or_default()
                .push(Note {
                    step: step as u16,
                    pitch: key,
                    duration,
                    velocity,
                });
            if let Some(row) = imported[bar].rows.get_mut(track_id) {
                row[step] = true;
            }
        }
        count += 1;
    }
    project.patterns.extend(imported.iter().cloned());
    project.arrangement = imported.iter().map(|p| Some(p.id.clone())).collect();
    project.selected_pattern_id = imported[0].id.clone();
    project.bpm = bpm;
    project.validate()?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::default_project;
    #[test]
    fn exports_and_reimports_pattern_notes() {
        let mut p = default_project("hermes");
        let dir = std::env::temp_dir().join(format!("beatcraft-midi-{}.mid", std::process::id()));
        let n = export_midi(&p, &dir).unwrap();
        assert!(n > 5);
        let old = p.patterns.len();
        let count = import_midi(&dir, &mut p).unwrap();
        assert!(count > 0);
        assert!(p.patterns.len() > old);
        assert!(import_midi(&dir, &mut p).unwrap() > 0);
        for (index, pattern) in p.patterns.iter().enumerate() {
            assert!(!p.patterns[index + 1..]
                .iter()
                .any(|later| later.id == pattern.id));
        }
        let _ = std::fs::remove_file(dir);
    }
}
