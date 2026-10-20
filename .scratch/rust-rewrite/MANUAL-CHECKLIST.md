# Manual checklist (human-only)

Agents append items here; each needs exact commands and expected result.

- [ ] Live Go Live on real Discord with burner account: `DSTREAMY_TOKEN=… dstreamy stream --guild … --channel … --target test-pattern` → viewer sees pattern + hears tone, H264
- [ ] macOS real capture (Screen Recording permission): `cargo test -p dstreamy-capture-macos -- --ignored`
- [ ] Linux Wayland portal + PipeWire capture: `cargo test -p dstreamy-capture-linux -- --ignored`
- [ ] Hardware encoders: VA-API, NVENC selected and streaming
- [ ] Sign off removal of Swift/Bun code (issue 14)
