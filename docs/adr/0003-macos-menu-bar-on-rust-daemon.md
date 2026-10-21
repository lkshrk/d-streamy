# 0003 — Keep the macOS menu bar app as a frontend to `dstreamy daemon`

Status: accepted (2026-10-21). Amends 0001 ("the menu bar app is dropped").

## Context

ADR 0001 made the product a CLI/TUI on both platforms. On macOS the user wants to keep the SwiftUI menu bar app. Today that app captures in-process (CaptureLib: ScreenCaptureKit and VideoToolbox) and pipes frames to a Bun daemon over a JSON stdin/stdout protocol. The Rust rewrite now does capture, encode and Discord, so the app only needs to be a UI.

## Decision

- macOS: the SwiftUI menu bar app stays and becomes a thin frontend. It spawns the bundled `dstreamy daemon` and talks to it over a line-delimited JSON protocol on stdin/stdout (below). Capture, encoding and Discord all live in Rust. CaptureLib and the Bun daemon go away.
- Linux: CLI and TUI only, unchanged.
- The app bundles a universal `dstreamy` binary (lipo of aarch64 and x86_64) in `Contents/MacOS/`, signed together with the app. The Screen Recording permission is attributed to the app as the responsible process (to be verified on the manual checklist).
- Target picking uses the daemon's `targets` list (`display:<id>`, `window:<id>`). A crop drawn with the crop overlay is sent as source pixels.

## Protocol (version 1)

Each message is one JSON object per line, UTF-8. stdout carries protocol messages only. Logs go to the log file, plus stderr with `-v`. Field names are snake_case.

Commands (app → daemon). Every command has a client-chosen `"id"` (string), echoed back in its reply.

| `cmd` | fields | reply `data` |
|---|---|---|
| `hello` | — | `{"protocol":1,"version":"x.y.z","platform":"macos"}` |
| `set_token` | `token` | `null`. The daemon validates the token against the gateway, then stores it (0600). The token is never echoed or logged. |
| `logged_in` | — | `{"logged_in":bool}` |
| `guilds` | — | `[{"id","name"}]` |
| `channels` | `guild_id` | `[{"id","name"}]` (voice channels) |
| `targets` | — | `[{"id","kind":"display"\|"window","title","app","width","height"}]` |
| `start` | `guild_id`, `channel_id`, `target`; optional `crop {x,y,width,height}`, `codec`, `fps`, `resolution`, `bitrate_kbps`, `gain` | `null`. Optional settings fall back to config.toml `[stream]`. |
| `update` | optional `fps`, `resolution`, `bitrate_kbps`, `crop` | `null` (applied live via `Session::update_video`) |
| `stop` | — | the final `StreamStats` |
| `shutdown` | — | `null`, then the process exits 0 |

Replies: `{"type":"result","id":…,"data":…}` or `{"type":"error","id":…,"code":"<snake_case>","message":"…"}`.

Error codes: `malformed`, `unknown_cmd`, `invalid_params`, `not_logged_in`, `invalid_token`, `gateway`, `unknown_guild`, `guild_unavailable`, `unknown_channel`, `no_platform_capture`, `capture`, `already_running`, `not_running`, `streaming`, `config`, `internal`. `id` must be a string; a line without one (or that is not a JSON object, or not UTF-8) is `malformed` with `"id":null`, while a line with a string `id` but no string `cmd` is `malformed` with that `id`. Commands still running at `shutdown` or EOF get an `internal` error reply. Replies can arrive out of request order. `stop` and `update` without a live stream fail with `not_running`. Setting values follow the CLI flags (`codec` `"h264"`/`"h265"`, `resolution` `"WIDTHxHEIGHT"` or `"720p"`/`"900p"`/`"1080p"`, `fps` 15/30/60, `bitrate_kbps` 1000–15000, `gain` 0.0–3.0).

Events (daemon → app, unsolicited): `{"type":"event","event":<SessionEvent>}`, using the existing `SessionEvent` serde form (state, health with metrics, channel_moved, resumed, error).

EOF on stdin means the parent is gone: stop any stream and exit. An unknown `cmd` or malformed line returns an error reply (with id `null` when there was none) and keeps the daemon running.

The contract is pinned by golden NDJSON fixtures in `rust/crates/dstreamy-cli/tests/fixtures/daemon/`. Rust tests produce and check them, and the Swift tests decode the same files.

## Consequences

- One capture and encode implementation (Rust) instead of two.
- The app no longer needs Bun, `sharp` or the Node daemon. The release workflow bundles the Rust binary instead.
- The app loses SCContentSharingPicker unless a picked filter can be mapped to a `window:`/`display:` id. The app's own picker over `targets` is the default.
- Protocol changes need a `protocol` bump and fixture updates on both sides.
