# Changelog

All notable changes to BeatCraft Studio are documented here.

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
