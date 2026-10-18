# Roadmap

BeatCraft Studio's aim is to become a capable, open-source workspace for funk, phonk, and hybrid production. Surpassing mature DAWs is a long-term aspiration—not a claim of feature parity or a promise that parity can be delivered quickly.

## V1 source milestone

- Rust native desktop launcher and direct-launch DRIFT, BOOGIE, and RHYTHM apps.
- 16-step sequencing, genre starter grooves, tempo/swing, pattern editing, and multi-bar arrangement.
- Piano roll with MIDI pitch, note duration, velocity, and deletion.
- Native synthesis and offline stereo WAV rendering/playback.
- Track level/pan/mute/solo, master/tape grit, low-pass, drive, delay, and reverb.
- Volume/pan automation interpolated across a project arrangement.
- WAV sample import/trim/loop, default-input recording, MIDI file import/export, portable local projects, and basic undo/redo.
- Automated checks and Linux launch smoke testing.

## Next engineering stages

1. **Audio engine:** sample-accurate transport, low-latency streaming/mixing, device selection, metering, and real-device timing tests.
2. **Editing workflow:** clip-level sample placement, richer piano-roll tools, key/scale helpers, grid/snap options, and scalable arrangement editing.
3. **MIDI workflow:** hardware input, device selection, stronger event/tempo-map handling, and compatibility fixtures from common DAWs.
4. **Project safety:** autosave, crash recovery, migration/versioning, and robust handling of missing or moved samples.
5. **Platform packaging:** validate Windows/macOS builds and audio devices, then add signed installers/bundles where practical.
6. **Plugin architecture:** investigate a cross-platform plugin format and crash-isolation model only after transport and routing are stable.

Features count as implemented only when present in the application and covered by tests. Physical audio-device behavior and Windows/macOS builds still need verification on real systems. Plugin hosting and mature-DAW parity are not part of the current source milestone.
