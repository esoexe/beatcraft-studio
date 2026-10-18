# Changelog

All notable changes to BeatCraft Studio are documented here.

## [2.7.0] — 2026-10-18

### Added — PDC, parameter hosting, MIDI bridging

- **Plugin-delay compensation (PDC) on tracks.** CLAP instruments report
  `clap.latency`; every track is shifted onto the slowest plugin track's time
  base with a persistent carry, so a 64-sample-latency synth lands its notes
  sample-aligned with the built-in voices. The direction is spec-standard
  (delay everyone to the slowest), the carry is seamless across chunk borders
  (unit-tested), and the master insert deliberately stays uniform-shifted —
  whole-mix delay changes nothing audible. Master-insert latency is reported
  but intentionally uncompensated (documented).
- **Full parameter hosting.** Plugins are queried for `clap.params`
  (hidden/read-only/bypass params filtered), and every parameter is editable
  in a generic slider panel: "CLAP parameters" under the master FX picker,
  "Instrument parameters" in the piano roll for plugin tracks. Values persist
  in the project (`ClapInsert.params`) and are delivered as sample-accurate
  `clap_event_param_value` events at sample 0 of the first processed tile —
  verified against a parameterized fixture: exact `(id, time, value)` receipt
  and effect applied from the first sample. Plugin default state is playback
  until you touch a slider.
- **MIDI-dialect bridging.** Instruments whose note ports only speak the MIDI
  dialect now work: note lanes are bridged to `clap_event_midi` 0x90/0x80
  frames with velocity mapped to 1..127, negotiated per plugin at load
  (CLAP dialect preferred). Tested with a MIDI-only fixture.
- The input event pipeline was generalized into a unified, spec-sorted event
  set (notes + param values + bridged MIDI share one list per processing
  tile). 31/31 tests, clippy/fmt clean.

### Not yet (honesty ledger)

- No plugin GUI embedding — native window parenting across three OSes is
  being held back until it can be validated on real systems, per the
  no-claims-without-proof rule.
- No per-note expressions/MPE, no lane-based parameter automation
  (parameters are set-and-stored, not automated over time), no sidechain yet.

## [2.6.0] — 2026-10-18

### Added — CLAP instruments

- **Instrument tracks.** "+ Add plugin track" in the mixer enumerates every
  CLAP `instrument` on your system (same spec scan paths as the FX host) and
  adds it as a purple track driven by a pitched lane: notes from the tracker /
  piano roll / MIDI-in play the plugin. Swing, velocity (mapped from 0-127 to
  the spec's 0..1), and sample-accurate gates all carry through.
- **Sample-accurate, streaming-true note delivery.** Notes become
  `clap_event_note` NOTE_ON/OFF pairs pinned to the absolute frame timeline;
  the streaming engine keeps one persistent instance per track and only emits
  events *landing* in each chunk, so a note that opens in bar 1 and closes in
  bar 5 plays exactly once. Both render paths tile onto the same global
  512-frame grid and see identical `steady_time`/`process()` call sequences —
  offline and streamed bounces remain bit-identical with instruments active
  (proven against an in-process instrument fixture: event offsets exact to the
  sample, whole-timeline vs. four-window renders bit-equal). 26/26 tests.
- Instrument output flows through the track's normal chain afterwards
  (transient/drive/delay/reverb sends, mixer volume/pan/automation, mute/solo),
  and MIDI file export includes plugin notes as pitched events.
- Loading validates the note-port dialect (CLAP) and fails loudly with a
  dry-render fallback on machines lacking the plugin — same no-fail guarantee
  as the FX insert.

### Not yet (honesty ledger)

- Still headless: no plugin GUI, no parameter automation.
- No note expressions / MPE, no tempo or transport events, no MIDI-dialect
  bridging — CLAP-dialect instruments only.
- Plugin latency (PDC) and sidechain routing remain on the list.

## [2.5.0] — 2026-10-18

### Added — The Plugin Host release

- **CLAP effect hosting on the master buss.** BeatCraft Studio now scans the
  spec-standard CLAP locations (`~/.clap`, `/usr/lib/clap` on Linux,
  `%COMMONPROGRAMFILES%\CLAP` and the `%LOCALAPPDATA%` variant on Windows,
  `/Library/Audio/Plug-Ins/CLAP` and the per-user one on macOS — plus
  `CLAP_PATH` everywhere), enumerates every `audio-effect` plugin it finds, and
  lets you insert one at the very end of the master chain: after the Master
  Suite, before the ceiling normalize. The host is a hand-built implementation
  of the official CLAP 1.x C ABI (entry → plugin factory → activate →
  `process()`), with exact-spec search paths and port negotiation — no wrapper
  crates, just `libloading` and the spec. 24/24 tests.
- **Streaming-true plugin rendering.** The insert is a persistent instance
  shared by the offline renderer and the streaming engine, and every audio
  buffer is retiled onto a global, absolute 512-frame grid before reaching the
  plugin. A streamed bounce therefore makes the same `process()` calls with the
  same `steady_time` values as an offline render — the v2.0 bit-parity contract
  survives a plugin insert (chunk-invariance is unit-tested through the real
  ABI structs; the plugin's own determinism is the only external factor).
- **Mixer UI: CLAP FX picker** in the Master Suite group — scans once on first
  open, "None" clears the insert, per-insert enable toggle. The insert
  serializes with the project (`master_clap`); rendering on a machine that
  lacks the plugin degrades to a dry bounce with a stderr note — renders never
  fail over a missing plugin.

### Not yet (honesty ledger)

- **Effects only** — instruments and note-event delivery are the next milestone.
- **Headless** — no plugin GUI embedding; parameters stay at factory defaults.
- **No plugin-delay compensation** and no sidechain/multi-bus routing yet.

## [2.0.0] — 2026-10-18

### Added — The Streaming Mastering release

- **True streaming render engine (phase 2)** — the arrangement is synthesized
  bar-by-bar by a dedicated producer thread, just ahead of the playhead, into a
  lock-free ring the audio callback drains directly. Playback starts with the
  first bar, memory stays flat no matter how long the project is, and underruns
  render silence instead of stalling. Per-track FX states (lowpass, drive,
  feedback delay, dual reverbs) and the complete Master Suite state (EQ biquads,
  compressor envelope) cross every chunk boundary seamlessly; chunk borders ride
  the continuous musical time lattice, so no hit can ever fall between bars.
- **Bit-parity contract, machine-verified** — for all four studio floors and for
  every arrangement prefix of each, the concatenated stream is proven to be the
  offline render, sample for sample (drums, pitched lanes, loops, FX tails, and
  mastering included). 21/21 tests.
- **Varispeed warp (Fit)** — sample tracks gain a "Fit" control that forces any
  take into exactly N sixteenths (0 = natural). MPC-style: speed follows length,
  pitch follows speed. Grid-exact by unit test.
- Record-into-pattern affirmed: mic takes keep landing on the selected pattern
  at step 1 as a new sample track, undoable like everything else.

## [1.2.0] — 2026-10-18

### Added — Realtime & Performance pass

- **Hardware MIDI-in** (`midir`, all desktop OSes) — pick any connected
  keyboard/controller from the transport "MIDI in" menu, arm **MIDI REC**, press
  Play: note-ons are quantized onto the selected pattern's 16-step grid at the
  playhead. GM-mapped rows on drum floors (kick 35/36, snare 38–40, hats 42/44/46,
  cowbell/cr/… open sounds, everything else → sample row); pitched note insertion
  on Drift/Boogie into the current piano-roll lane. Undo-free of spam: one snapshot
  per ~half-second of performance. Live note feedback (note name + velocity) in the
  status line when not recording.
- **Streaming render pipeline (phase 1)** — playback bounces now run on a dedicated
  worker thread with live per-bar progress in the status bar; the entire UI —
  transport, mixer, and the new MIDI-in — stays fully responsive during renders.
  Playback starts automatically the instant production finishes.
- **Verified by the gate**: 19/19 tests. New proofs: raw MIDI wire parsing incl.
  velocity-0-as-off / channel masks / garbage-safety, GM mapping table, and the
  streaming-parity test asserting the progressive renderer is **bit-identical** to
  the standard one with monotonic per-bar progress (the guarantee that progress
  reporting can never change your audio). Clippy zero-warnings, rustfmt clean.

## [1.1.0] — 2026-10-18

### Added — Mastering & Delivery pass ("beat the DAWs" sprint)

- **Master Suite (final buss DSP)** — a dedicated mastering chain appended after the
  master mixdown, always in the same physically-correct order:
  - *3-band EQ* — RBJ biquads (120 Hz low shelf / 1.2 kHz mid peak / 6.5 kHz high shelf), ±12 dB
  - *Stereo width* — mid/side processing, 0.0 (mono) … 2.0 (wide)
  - *Feedforward compressor* — peak-follower with 4 ms attack / 90 ms release
    exponential envelopes, threshold −40…0 dB, ratio 1…12:1, up to +18 dB makeup
  - *Tanh limiter* — variable ceiling 0.5…1.0, always last, cannot be bypassed
    downstream of it (safety normalize respects the ceiling)
  - Mixer UI: gold-tinted "MASTER SUITE" group with toggles + sliders, all undoable
- **Professional WAV export** — 16-bit PCM, **24-bit PCM**, or **32-bit float** WAV,
  selectable from a toolbar dropdown next to a **44.1 / 48 kHz** sample-rate choice.
- **Autosave + crash recovery** — every ~25 s of edits, a JSON sidecar
  (`beatcraft-studio-autosave.json` in the system temp dir) is written; on the next
  launch after an interrupted session a banner offers one-click recovery. Saving
  clears the dirty flag and dismisses the offer. Fully local — nothing leaves the machine.

### Verified by the gate

- Test suite: **14/14 green**, including 6 new DSP-level tests proving compressor peak
  reduction, limiter ceiling conformance, low-shelf band energy boost, mono collapse at
  width=0, end-to-end render respecting the suite ceiling, and correct byte sizes for
  all three WAV depths (incl. 24-bit byte-accurate round-trip).
- `cargo clippy --all-targets`: **0 warnings**. `cargo fmt --check`: clean.

## [1.0.0]

- Initial public release: four floors (BeatForge / Drift / Boogie / Rhythm), tracker
  sequencer, piano-roll editor, mixer with per-track FX, automation lanes, MIDI
  import/export, WAV export, handcrafted phonk/funk sound engines.
