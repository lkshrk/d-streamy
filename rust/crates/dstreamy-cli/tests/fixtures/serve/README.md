# `dstreamy serve` golden fixtures (control role, protocol 2)

Protocol: `docs/adr/0004-persistent-service-and-external-producers.md` (IPC), with the protocol 1 commands of `docs/adr/0003-macos-menu-bar-on-rust-daemon.md`. Each scenario has two NDJSON files:

- `<scenario>.in.ndjson`: the lines a client writes to the socket, in order.
- `<scenario>.out.ndjson`: what the service wrote back after normalisation. The `result`/`error` replies come first, in request order, followed by the `event` messages in the order they arrived.

Scenarios:

- `control`: one control connection, from `hello` to `shutdown`, against the mock Discord server with `--skip-platform`. The test sends one request at a time and waits for its reply; after a `start` it also waits for the `streaming` state and one `health` event, and after a `stop` for the `stopped` state. It ends when the service closes the connection.
- `handshake`: each line on its own new connection; every one is refused and the connection closed.

Normalisation, applied to the real output before comparing:

- `hello`: `data.version` becomes `"<version>"`, `data.platform` `"<platform>"` and `data.connection` `"<connection>"`; `data.session` is normalised like a `status` reply.
- `status`: a non-null `started_by` becomes `"<connection>"`, and `source.for_ms` `0`.
- Only `state` events (without `reconnecting`) and the first `health` event are kept. `source` events race with `state` events, so the scenario pins the source through `status` instead. Inside the health event, `status.stats` and the `stop` result (`StreamStats`), every number becomes `0`, every boolean `false`, `last_error` `null`, `*_level` `"good"` and `*_series` `[]`.

Regenerate with `DSTREAMY_UPDATE_FIXTURES=1 cargo test -p dstreamy-cli --test serve golden`.
