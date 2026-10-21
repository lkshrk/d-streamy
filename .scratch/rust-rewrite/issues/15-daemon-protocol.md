# 15 — `dstreamy daemon`: stdio JSON protocol for the macOS app

Status: ready-for-agent
Depends: 09, 10

## Scope

Implement the `dstreamy daemon` subcommand exactly as specified in `docs/adr/0003-macos-menu-bar-on-rust-daemon.md` (protocol v1). It reuses the existing lib modules (connect, guilds/channels, capture source selection, `stream::start`/`teardown`, config and token storage, `Signals`, `QuietOnError`). Commands are handled concurrently with a running session: `update` and `stop` must not block on `guilds`, and the reverse. Support the hidden `--gateway-url` for tests, as the other commands do.

## Gates

- Integration tests against the mock for every command, the event stream during `start` with the test pattern, `update` taking effect live, `stop`, `shutdown`, EOF on stdin, a malformed line, an unknown cmd, and errors (bad token, unknown guild).
- Golden fixtures in `rust/crates/dstreamy-cli/tests/fixtures/daemon/`: a request file and an expected response/event file per scenario. Fields that vary (ids, timings, versions) are normalised before comparison. Issue 16's Swift tests decode these same files.
- The token never appears on stdout, in logs or in errors (tested).
- Global gates from the PRD.

## Comments
- Decision (wire details the ADR leaves open; the ADR's protocol section now lists them):
  - `id` must be a string. A line without one, or with a non-string one, gets a `malformed` error with `"id":null`, like a line that is not a JSON object or has no string `cmd`.
  - Error codes: `malformed`, `unknown_cmd`, `invalid_params` (missing or invalid fields, including stream settings and crop), `not_logged_in`, `invalid_token`, `gateway` (any other connection failure), `unknown_guild`, `guild_unavailable`, `unknown_channel`, `no_platform_capture`, `capture` (capture backend errors), `already_running`, `not_running`, `streaming` (`set_token` while a stream is live), `config` (config or token file errors), `internal`. Messages are for humans and may change.
  - `codec` is `"h264"` or `"h265"` (case-insensitive), `resolution` is `"WIDTHxHEIGHT"` or `"720p"`/`"900p"`/`"1080p"`, `fps` is 15, 30 or 60, `bitrate_kbps` 1000–15000, `gain` 0.0–3.0, the same rules as the CLI flags.
  - `start` neither reads nor writes `config.toml` `[target]`/`crop`; it only falls back to `[stream]` for missing settings, and never saves the app's choices (the app keeps its own preferences). A Linux portal restore token is still saved, as in the CLI.
  - `stop` and `update` without a live stream fail with `not_running`. `stop` returns the `stats` of the last `health` event, or all-zero `StreamStats` when none arrived yet.
  - `event` carries the `SessionEvent` exactly as `stream --json` prints it, so the inner object has its own `"event"` tag: `{"type":"event","event":{"event":"state","state":{"state":"streaming"}}}`.
  - `hello.platform` is `std::env::consts::OS` (`"macos"` or `"linux"`); `version` is the `dstreamy` crate version.
  - Commands run concurrently: each is handled on its own task and the reply is written when it finishes, so replies can arrive out of request order. `shutdown`, EOF on stdin, SIGINT and SIGTERM all stop any stream, close the gateway and exit 0; `shutdown`'s `null` reply is written after the stream is stopped.
- Decision: `update` calls `Session::reconfigure` (issue 08, 9e6a368) with the running stream's `CaptureConfig` and the update's fields applied, so fps, resolution, bitrate and crop reach the capture source and encoder inside the same Go Live (no STREAM_DELETE, one STREAM_CREATE, op 12 re-announced). Updates are serialised with a mutex so the kept settings match the session's. A codec change cannot be requested (no field); a failed source restart is `capture` and the session stops with `source_failed`.
- Decision: `targets` fails with `capture` when listing the platform targets fails (e.g. no Screen Recording permission), instead of the CLI's warning plus a test-pattern-only list, so the app can tell the user.
- Decision: the daemon logs to `dstreamy.log` in the config directory (the TUI's file), and also to stderr with `-v`.
- Decision: a mock launcher for issue 16's Swift integration test: `cargo build -p dstreamy-mock-discord --bin mock-discord` builds `rust/target/debug/mock-discord`. It prints one JSON line `{"gateway_url","token","guild_id","channel_id"}` and serves until stdin reaches EOF, then exits 0. It is a test tool; `release-rust.yml` builds and packages only `-p dstreamy-cli`'s `dstreamy`. The source is `src/launcher.rs` with an explicit `[[bin]]`, because the repository `.gitignore` ignores every `bin/` directory.
- Evidence (2026-10-21): `cargo fmt --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked` all ok on macOS; `cargo test -p dstreamy-cli --test daemon` 10/10 (6 tests: both fixture scenarios incl. the token never on stdout, stderr or the `-vv` log; EOF; SIGTERM; pipelined commands while streaming; live update); `cargo test -p dstreamy-mock-discord --test launcher` ok.
- Review fixes (2026-10-21):
  - stdin is read as bytes, so a line that is not UTF-8 gets a `malformed` reply instead of ending the daemon.
  - One `lifecycle` lock covers creating the connection, `set_token` (check, validate, replace) and `start` (live check through `Session::start`), so a racing `start` can no longer be torn down by `set_token`.
  - `start` checks `already_running` before building a source, and sets the stats, portal book and running settings only after the session accepted it.
  - The event forwarder lives in the connection and is drained under the same lock when disconnecting.
  - `stop` returning no source and `SessionError::NotRunning` both map to `not_running`; `CodecChange` is `invalid_params`, `Stream(..)` (including `SourceLost`) is `capture`.
  - Commands still running at `shutdown`/EOF get an `internal` "daemon shutting down" reply.
  - All stdout writes go through one writer thread.
  - Tests: the fixture comparison keeps state events (without reconnecting) and the first health event, and the state sequence is checked on its own. The session test waits for op 12 with the updated attributes, the live-update test checks that the RTP step after the update matches 30 fps (so the source really runs with the new config), and the errors test also checks the `-vv` log for both tokens.
- `update` builds the next settings and `CaptureConfig` in `Running::updated`: the update's crop (or the current one when absent) goes into the config passed to `Session::reconfigure`, together with the target and the merged stream settings. A unit test checks that a crop-only update reaches `CaptureConfig.crop` and that a later fps-only update keeps it; the live-update integration test sends a crop with the new size and checks op 12 and 30 fps pacing after the restart.
