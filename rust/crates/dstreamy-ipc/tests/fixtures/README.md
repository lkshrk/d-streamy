# IPC golden fixtures

Shared by the Rust service and the C plugin (`obs/`). Protocol: `docs/adr/0004-persistent-service-and-external-producers.md` (IPC). Rust checks them in `rust/crates/dstreamy-ipc/tests/golden.rs` and `rust/crates/dstreamy-cli/src/serve/producer.rs`; `DSTREAMY_BLESS=1 cargo test -p dstreamy-ipc --test golden` rewrites the generated ones.

All NDJSON lines are compact JSON with keys sorted, one object per line, `\n`-terminated, as the service writes them.

## `control/`

| File | Content |
|---|---|
| `hello.ndjson` | A control `hello` and a full producer `hello` (protocol 2), as a client sends them. |
| `malformed.ndjson` | Lines the line codec must survive: not JSON, not UTF-8, a bare array, a CRLF line, then a good line. Hand-built. |
| `producer_events.ndjson` | Every line a producer reads from the service: the `{"attached":<id>}` reply, `keyframe_request`, `source` (state as in `status.source`, without `stats`), and `detached` with each `reason` (`stopped`, `shutdown`, `incompatible`, `oversized`) and a `message`. After `detached` the service closes the connection. |
| `producer_hello_replies.ndjson` | One case per line: `{"case", "precondition", "request", "reply"}`. `request` is the producer `hello` sent; `precondition` the service state it needs; `reply` the line the service answers with. Cases: `attached`, `no_session`, `busy`, `incompatible_fps`, `incompatible_audio_codec`, `unsupported_protocol`, `invalid_params`. An error reply's `code` is the start of `case`. |

## `producer/`

Each `<name>.bin` is a byte stream of producer frames (`u32 BE length | u8 kind | i64 BE pts_us | u8 flags | payload`); `<name>.json` describes it:

- `frames`: what decodes, in order: `kind` (`video`, `opus`, `end_of_stream`), `kind_byte`, `pts_us`, `keyframe` (as decoded: always `false` for non-video), `payload_hex`.
- `error` (only `err_*`): the frame-level error the decoder stops with after those frames: `oversized`, `too_short`, `unknown_kind`, `reserved_flags`, `end_of_stream_payload`, `truncated`. The service answers `oversized` with `detached` reason `oversized` and the others with `incompatible`; `truncated` is the connection ending mid-frame and gets no `detached`.
- `detached` (only `ingest_*`): every frame decodes, but ingest validation refuses one. `reason` is the `detached` reason, `clause` the rule's source, `message_contains` a substring of the `detached` message.

| Files | Purpose |
|---|---|
| `av_stream`, `pts_extremes` | Valid streams, including negative and extreme pts. Generated. |
| `opus_keyframe_flag_ignored` | An Opus frame with flag bit 0 set, which decodes with `keyframe: false`. Hand-built. |
| `err_*` | One frame-level error each. |
| `ingest_*` | One ingest rule each: no start code, IDR without the keyframe flag, keyframe without SPS, pts ≠ dts (B-frames), a 40 ms Opus packet. Generated. |
