# 09 — cli commands

Status: ready-for-agent
Depends: 08

## Scope

`dstreamy` clap CLI: `login` (store token), `guilds`, `channels <guild>`, `targets`, `stream --guild --channel --target <id|test-pattern> [--crop x,y,w,h] [--codec] [--fps] [--res] [--bitrate] [--gain]`, `--json` output for machine use. Hidden `--gateway-url` override used by tests to hit mock.

## Gates

- `assert_cmd` tests vs mock for every subcommand, incl. `stream --target test-pattern` running 3s and exiting cleanly on SIGINT
- Global gates from PRD

## Comments
- Note from 11: any binary linking `dstreamy-capture-macos` must emit `cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift` (copy `crates/dstreamy-capture-macos/build.rs`), else it fails at load with `libswift_Concurrency.dylib` not found. Add a test that runs the built `dstreamy --help` on macOS.
- Note from 11: `dstreamy_media::CaptureConfig` has no audio gain field; backends apply gain 1.0. Wire `StreamConfig` gain through (add field to `CaptureConfig`).
