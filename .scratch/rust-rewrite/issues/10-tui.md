# 10 — tui

Status: done
Depends: 09

## Scope

`dstreamy tui` with ratatui: guild/channel picker, target picker, settings form, live health panel (scores + sparklines + FPS/jitter/drops/audio cadence, green/yellow/red), start/stop. Pure view-model layer separate from rendering.

## Gates

- View-model unit tests
- Snapshot tests via ratatui `TestBackend` + `insta` for each screen
- Scripted key-event test driving full flow against mock with test-pattern
- Global gates from PRD

## Comments
- Follow-ups from 09:
  - `src/discord.rs` and `src/capture.rs` (connect, guild/channel helpers, capture source selection) are private modules of the bin crate. Promote them to a lib target in `dstreamy-cli` (or a shared module) before reuse; keep the CLI tests green.
  - Register signals once (see `Signals` in `stream.rs`), and write any non-TUI output through `QuietOnError`.
  - The Linux portal `restore_token` and the config `target` are not wired into the CLI yet. Wire them here: persist the restore token after a portal pick, and default the target picker to the config `target`.
  - Still open from 08: `start_stream` drops its source on failure, channel-move detection matches the user id, a lagged gateway broadcast is only logged, and FIR is untested.
- Linux gate: `rust/ci/linux.sh` now runs with `CARGO_INCREMENTAL=0` by default.
- Done: `dstreamy tui` (ratatui 0.30, crossterm) in the same binary. It has a guild picker, a voice channel picker, a target picker (it defaults to the config `target`) and a settings form: resolution 720p/900p/1080p, fps 15/30/60, bitrate in 500 kbps steps, codec, gain in 0.1 steps, and typed crop X,Y,W,H. The live panel shows the session state, Video/Audio scores colored green/yellow/red, FPS and audio-cadence sparklines, FPS/target, jitter p95/max, drops, audio frames/s, bitrate, keyframe requests and resumes, plus an event log. `s` stops, `q`/Ctrl-C quits. `dstreamy-cli` now has a lib target (`dstreamy_cli`: `capture`, `discord`, `output`, `signals`, `stream`, `tui`); `main.rs` is just the clap front end. `tui::model` is a pure view-model (keys and messages in, `Effect`s out), `tui::view` only draws it, and `tui::drive` is the async driver, generic over the ratatui `Backend`.
- Evidence (macOS): `cd rust && cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` all pass. dstreamy-cli: 34 lib tests, among them view-model tests and 9 `insta` snapshots via `TestBackend` (connecting, guilds, channels, targets, settings, settings with crop editing, live waiting, live with health, live after failure). `tests/tui.rs` has 4 tests: the scripted key flow against the mock with the test pattern (picker → settings → live → stop → quit, checking media, STREAM_DELETE and the saved config), quit while live, closed input, and the binary refusing to start without a TTY. The `tests/tui.rs` suite ran 10/10 green, about 2.1 s per run, both before and after the review fixes. Linux: `CARGO_BUILD_JOBS=2 rust/ci/linux.sh` exited 0 on the tree before the review fixes. The re-run after the fixes hit a read-only Docker Desktop VM disk (`meta.db: read-only file system`), which is an infrastructure fault, so its evidence is the pre-fix run; the fixes are platform-agnostic. GitHub CI `rust-linux` confirms Linux after the team lead pushes. A reviewer subagent found 0 high, 2 medium and 13 low issues, all fixed. A second Ctrl-C during shutdown arrives as a key in raw mode, so it now also triggers the exit-130 watcher. A stop no longer blocks interrupts. A lagged event stream resyncs from `session.state()`. The restore guard is created before `try_init`. The config update is extracted and tested, the portal token bookkeeping (`PortalBook`) is tested, and doc comments that restated the code were removed.
- Decision: the TUI does not log in. Without a token it exits with the `dstreamy login` hint, and it refuses to start when stdin or stdout is not a terminal. While it runs, tracing goes to `dstreamy.log` (rotating) in the config directory instead of stderr.
- Decision: pressing Enter in the settings form saves `[stream]`, `crop` and the target to `config.toml`, then goes live. The test pattern is not a config target, so choosing it keeps the previous target. Choosing `portal` keeps the saved restore token.
- Decision: the portal restore token is persisted when the capture source reports a new one (Linux-only `TokenReporting` wrapper around `LinuxCaptureSource`), on every session state change and at exit. `dstreamy stream` persists it too, at exit. Config target ids map to `display:N`, `window:N` and `portal`.
- Decision: a stop the user requested returns to the settings form. Any other stop reason stays on the live panel in red until Enter/Esc. Health is shown as the session sends it; the TUI never recomputes it.
- Decision: SIGINT/SIGTERM (registered once) and Ctrl-C quit gracefully. A second one during shutdown restores the terminal and exits 130. ratatui's panic hook plus a drop guard restore the terminal on panic and on error.
- Commit: 8f08e5c (feat(tui)).
- Follow-up: there is no driver-level test for a rejected `Session::start`, because the mock offers no clean way to make it fail; the model test covers `StartFailed`. Still open from 08: `start_stream` drops its source on failure, channel-move detection matches the user id, FIR is untested.

