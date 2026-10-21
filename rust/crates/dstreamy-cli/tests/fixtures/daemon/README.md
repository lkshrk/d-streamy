# `dstreamy daemon` golden fixtures (protocol 1)

Protocol: `docs/adr/0003-macos-menu-bar-on-rust-daemon.md`. Each scenario has two NDJSON files:

- `<scenario>.in.ndjson`: the lines written to the daemon's stdin, in order. `errors.in.ndjson` starts with a line that is deliberately not JSON.
- `<scenario>.out.ndjson`: what the daemon wrote to stdout after normalisation. The `result`/`error` replies come first, in request order, followed by the `event` messages in the order they arrived. On the wire, events interleave with replies.

The Rust test (`tests/daemon.rs`) sends one request at a time and waits for its reply. During `session` it also waits for the `streaming` state and one `health` event before sending `update`. It runs against the mock Discord server with `--skip-platform`, so `targets` lists only the test pattern.

Normalisation, applied to the real output before comparing:

- `hello`: `data.version` becomes `"<version>"` and `data.platform` becomes `"<platform>"` (really `"macos"` or `"linux"`).
- Only `state` events (without `reconnecting`) and the first `health` event are kept; other events are dropped. Inside it, and inside the `stop` result (`StreamStats`), every number becomes `0`, every boolean `false`, `last_error` `null`, `*_level` `"good"` and `*_series` `[]`.

Regenerate with `DSTREAMY_UPDATE_FIXTURES=1 cargo test -p dstreamy-cli --test daemon`.
