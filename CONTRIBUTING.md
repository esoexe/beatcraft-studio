# Contributing

Thanks for helping improve BeatCraft Studio. The application is written in Rust and uses a native `eframe`/`egui` interface. It is local-first and GPL-3.0-or-later.

## Development setup

Install Rust 1.88 or newer and Cargo. On Debian/Ubuntu, the ALSA development headers are needed to build CPAL:

```bash
sudo apt install build-essential pkg-config libasound2-dev
```

Build and test before submitting changes:

```bash
cargo fmt --all -- --check
cargo check --all-targets --locked
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
cargo build --release --locked --bins
```

Run the launcher with `cargo run --bin beatcraft-studio`, or start a studio directly with `cargo run --bin beatcraft-drift`, `cargo run --bin beatcraft-boogie`, or `cargo run --bin beatcraft-rhythm`.

## Code and feature guidelines

- Keep project data and studio defaults in `src/model.rs`.
- Keep synthesis, sample decoding, mixing, rendering, and WAV handling in `src/audio.rs`.
- Keep CPAL output/input stream ownership in `src/engine.rs`; avoid blocking work in real-time audio callbacks.
- Keep MIDI parsing/writing in `src/midi.rs`, portable project persistence in `src/project.rs`, and native UI in `src/app.rs`.
- Add tests for project formats, audio behavior, MIDI, or other core logic when changing it.
- Describe platform/audio limitations accurately. Do not claim plugin hosting, low-latency playback, or hardware support without implementation and tests.
- Do not add proprietary samples, leaked code, unlicensed artwork, or required hosted services.
- Keep projects usable offline; network access should not be required for core features.

Contributions are offered under GPL-3.0-or-later, consistent with the project license.
