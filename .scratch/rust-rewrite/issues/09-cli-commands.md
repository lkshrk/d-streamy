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
- Follow-ups from 08:
  - Write all stdout/stderr through `dstreamy_core::QuietOnError`, never `println!`, and exit cleanly on EPIPE.
  - Keep `SessionOptions` health_interval at 1 s.
  - `--json` can serialize `SessionEvent`, `SessionState` and `StreamStats` directly.
  - `start_stream` still drops its source on failure.
  - Channel-move detection matches the user id, not the session id.
  - A lagged gateway broadcast is only logged.
  - FIR is untested, and PLI may be counted on both senders.
