# Manual checklist (human-only)

Agents append items here; each needs exact commands and expected result.

- [ ] Live Go Live on real Discord with burner account: `DSTREAMY_TOKEN=… dstreamy stream --guild … --channel … --target test-pattern` → viewer sees pattern + hears tone, H264
- [ ] macOS real capture (Screen Recording permission): `cargo test -p dstreamy-capture-macos -- --ignored`
- [ ] Linux Wayland portal + PipeWire capture (needs `gstreamer1.0-pipewire`, `gstreamer1.0-pulseaudio`, `gstreamer1.0-plugins-ugly`, a running desktop session and pipewire-pulse): `cd rust && cargo test -p dstreamy-capture-linux --test portal -- --ignored --nocapture` → pick a screen in the dialog, play some audio; the test passes (≥30 decodable AUs, first a keyframe, Opus frames present, restore token returned) and the second start reuses the token without showing the dialog. The printed `encoder:` line names the selected encoder.
- [ ] Hardware encoders: VA-API, NVENC selected and streaming
- [ ] Sign off removal of Swift/Bun code (issue 14)
