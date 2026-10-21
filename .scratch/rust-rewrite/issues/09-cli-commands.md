# 09 — cli commands

Status: done
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
- Done: crate `rust/crates/dstreamy-cli` (binary `dstreamy`). Subcommands `login`, `guilds`, `channels <GUILD>`, `targets`, `stream --guild --channel --target <id|test-pattern> [--crop X,Y,W,H] [--codec h264|h265] [--fps 15|30|60] [--res WxH|720p|900p|1080p] [--bitrate KBPS] [--gain 0-3]`. Global flags: `--json`, `--config-dir DIR` (env `DSTREAMY_CONFIG_DIR`), `-v/-vv`, and hidden `--gateway-url`. `targets` has a hidden `--skip-platform`. `CaptureConfig` gained `audio_gain`, which the macOS pipeline, the Linux GStreamer `volume` element and the test-pattern tone all apply.
- Evidence (macOS): `cd rust && cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` all pass. `dstreamy-cli`: 6 unit and 14 integration tests pass, plus 1 ignored test that needs Screen Recording. The stream/SIGINT tests (`stream_`, `sigint`, 6 tests) ran 10 times in a row, 10/10 green at about 3.4 s each. Linux: `CARGO_BUILD_JOBS=2 rust/ci/linux.sh bash -c "export CARGO_INCREMENTAL=0 && cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace"` exits 0. A reviewer subagent found 2 medium and 5 low issues, all fixed: close the client when the guild check fails, arm the exit-130 watcher for every outcome, register signals once, log a client that is still shared, remove the unreachable outcome, fail fast on an unknown guild, and add UDP6 to loopback ICE. Tests were added for each.
- Decision: `stream` exits 0 on SIGINT/SIGTERM, on a broken stdout, and on a stop that is `requested` or `capture_ended`. It exits 1 on any other stop reason or error, and 2 on usage errors (clap). A second SIGINT/SIGTERM during shutdown exits 130 at once.
- Decision: `login` reads the token from stdin. On a TTY it prompts without echo (rpassword). It checks the token against the gateway before saving it to `token` (0600).
- Decision: `--guild`, `--channel` and `--target` are required. Stream settings default to `config.toml` `[stream]`, and the flags override them. `--crop` falls back to the config crop. The config's `target` is not used yet.
- Decision: `--json` prints `SessionEvent` NDJSON while streaming. The final line is `{"event":"state","state":{"state":"stopped","reason":{"reason":"..."}}}`. List commands print one JSON array of CLI-local rows (guild/channel/target), so the discord and media crates gain no serde derives.
- Decision: a loopback `--gateway-url` (127.0.0.1, ::1, localhost) switches WebRTC to loopback-only ICE without STUN, because the mock's media server only offers loopback candidates. This needs no extra hidden flag.
- Decision: `targets` always lists `test-pattern`. When listing platform targets fails (no Screen Recording permission), it prints a warning on stderr and still exits 0. The mock-free test uses the hidden `--skip-platform` on macOS so that it never triggers a TCC prompt.
- Decision: `guilds` and `channels` wait up to 5 s for guilds that READY marked unavailable (lazy GUILD_CREATE). They stop early once every guild is available. One-shot commands give up after 3 failed gateway attempts, while `stream` keeps the default 10.
- Follow-ups for issue 10 (TUI): reuse `discord::{connect, guilds, voice_channels}` and `capture::{source_for, targets}`. These are private modules of the bin crate, so promote them to a lib target (or into `dstreamy-discord`) before the TUI shares them. Keep `Signals` registered once. Write output through `QuietOnError`. `StreamHealth` already carries the `fps_series`/`audio_series` sparklines. Linux portal `restore_token` from `config.toml` is not wired yet (`LinuxCaptureSource::with_restore_token`), and the config's `target` is ignored. The 08 follow-ups still open: `start_stream` drops its source on failure, channel-move detection matches the user id, a lagged gateway broadcast is only logged, FIR is untested and PLI may be counted twice.
- Note: the shared `dstreamy-linux-target` volume filled the 32 GB Docker VM (`No space left on device`). `debug/incremental` was pruned (6.9 GB). Run the Linux gate with `CARGO_INCREMENTAL=0`. Vault: `projects/dstreamy/pitfalls/cli-signals-and-mock-ice.md`.
- Commit: 5c080f5 (feat(cli)).
- Review round 2 (D), 2026-10-21: the hidden `--gateway-url` now only accepts the default Discord gateway or a loopback host (clap value parser, exit 2), so the token cannot be sent to another host. `login` reads the token from stdin or the prompt before the tokio runtime starts. `stream` and `tui` share `stream::start`/`stream::teardown`, `stream::stopped_cleanly`, `capture::kind_label` and `HealthLevel::label`. The unknown-guild test asserts one gateway IDENTIFY instead of a 4 s wall-clock bound; the early return it guarded is now a unit test of `discord::guild_settled`.
- Decision: `dstreamy tui` exits 1 when connecting to Discord failed or the last stream stopped by itself (any `StopReason` other than `Requested`/`CaptureEnded`), the same rule `stream` uses. A failed start that the user can retry does not count.
- Decision: the second interrupt in `stream` is now a `select!` (biased to the signal) between `Signals::recv` and `stream::teardown`, returning exit 130, instead of a spawned task calling `process::exit`. A mock knob could not widen the window: nothing in the client's shutdown waits on the server. The flaky binary test that accepted 0 or 130 is replaced by an in-process test that drives `stream::run_with` with `Signals::from_channel` and queues both interrupts, so it always takes the 130 path (10/10; one interrupt yields 0). The real SIGINT wiring stays covered by `stream_test_pattern_until_sigint`. `Signals` is still created only once the stream is live, so Ctrl-C while connecting still kills the process.
- Decision: the TUI stays open after a connect failure or a stream that stopped by itself, so the user can read the error; the failure only shows up in the exit status (1) when the user quits.
- `remember_portal_token` now also clears a spent restore token: `PortalToken` records only what the source reports after a successful start (`None` before one), and a reported `None` that differs from the saved token writes `Portal { restore_token: None }`.
