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
