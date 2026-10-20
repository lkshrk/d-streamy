# Manual checklist (human-only)

Agents append items here; each needs exact commands and expected result.

- [ ] Live Go Live on real Discord with burner account: `DSTREAMY_TOKEN=… dstreamy stream --guild … --channel … --target test-pattern` → viewer sees pattern + hears tone, H264
- [ ] macOS real capture (Screen Recording permission): grant Screen Recording to the terminal, then `cd rust && cargo test -p dstreamy-capture-macos --test screencapture -- --ignored --nocapture --test-threads=1` → 4 tests pass: `lists_displays_and_windows` prints `display:<id>` and `window:<id>` targets, `captures_main_display_as_h264_with_audio` decodes the H264 AUs and prints a non-zero audio count while sound plays, `captures_cropped_display_as_h265` and `captures_first_user_window` start with a keyframe. Without the permission SCShareableContent returned windows but no displays and no frames (seen 2026-10-21).
- [ ] Linux Wayland portal + PipeWire capture: `cargo test -p dstreamy-capture-linux -- --ignored`
- [ ] Hardware encoders: VA-API, NVENC selected and streaming
- [ ] Sign off removal of Swift/Bun code (issue 14)
