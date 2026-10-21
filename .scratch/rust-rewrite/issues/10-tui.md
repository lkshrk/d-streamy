# 10 — tui

Status: ready-for-agent
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
