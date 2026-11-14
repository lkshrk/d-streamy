# 0003 — Keep the macOS menu bar app as a frontend to `dstreamy daemon`

Status: accepted (2026-10-21). Amends 0001 ("the menu bar app is dropped").

## Context

ADR 0001 made the product a CLI/TUI on both platforms. On macOS the user wants to keep the SwiftUI menu bar app. Today that app captures in-process (CaptureLib: ScreenCaptureKit and VideoToolbox) and pipes frames to a Bun daemon over a JSON stdin/stdout protocol. The Rust rewrite now does capture, encode and Discord, so the app only needs to be a UI.

## Decision

- macOS: the SwiftUI menu bar app stays and becomes a thin frontend. It spawns the bundled `dstreamy daemon` and talks to it over a line-delimited JSON protocol on stdin/stdout (below). Capture, encoding and Discord all live in Rust. CaptureLib and the Bun daemon are gone (removed 2026-11-06, amending ADR 0001).
- Linux: CLI and TUI only, unchanged.
- The app bundles a universal `dstreamy` binary (lipo of aarch64 and x86_64) in `Contents/MacOS/`, signed together with the app. The Screen Recording permission is attributed to the app as the responsible process (to be verified on the manual checklist).
- Target picking uses the daemon's `targets` list (`display:<id>`, `window:<id>`). A crop drawn with the crop overlay is sent as source pixels.

## Protocol (version 1)

Each message is one JSON object per line, UTF-8. stdout carries protocol messages only. Logs go to the log file, plus stderr with `-v`. Daemon stderr is best-effort: a worker owns blocking writes behind a 64-record queue, each record capped at 8 KiB. Oversized writes are truncated, and a full or closed queue discards diagnostics; enqueue, flush and drop do not wait for the parent to read. Other CLI commands keep direct stderr, and file logging is unchanged. Field names are snake_case.

Commands (app → daemon). Every command has a client-chosen `"id"` (string), echoed back in its reply.

| `cmd` | fields | reply `data` |
|---|---|---|
| `hello` | — | `{"protocol":1,"version":"x.y.z","platform":"macos"}` |
| `set_token` | `token` | `null`. The daemon validates the token against the gateway, then stores it (0600). The token is never echoed or logged. |
| `logged_in` | — | `{"logged_in":bool}` |
| `guilds` | — | `[{"id","name"}]` |
| `channels` | `guild_id` | `[{"id","name"}]` (voice channels) |
| `targets` | — | `[{"id","kind":"display"\|"window","title","app","width","height"}]` |
| `start` | `guild_id`, `channel_id`, `target`; optional `crop {x,y,width,height}`, `codec`, `fps`, `resolution`, `bitrate_kbps`, `gain`, `require_e2ee`, `audio_source` (a device from `dstreamy audio-sources`) | `null`. Optional settings fall back to config.toml `[stream]` and `audio_source`. Effective `require_e2ee` is configured OR requested: `false` cannot weaken a configured requirement. `grace_secs` (ADR 0004) is accepted and ignored, as any unknown field is. |
| `update` | optional `fps`, `resolution`, `bitrate_kbps`, `crop` | `null` (applied live via `Session::update_video`) |
| `stop` | — | the final `StreamStats` |
| `shutdown` | — | `null`, then the process exits 0 if shutdown and output draining succeed |

Replies: `{"type":"result","id":…,"data":…}` or `{"type":"error","id":…,"code":"<snake_case>","message":"…"}`.

Error codes: `malformed`, `unknown_cmd`, `invalid_params`, `not_logged_in`, `invalid_token`, `account_mismatch`, `unknown_account` (the `--account` the daemon was started with does not exist, or was removed), `accounts_unavailable` (`--account` with `DSTREAMY_TOKEN` set), `gateway`, `identify_limit`, `unknown_guild`, `guild_unavailable`, `unknown_channel`, `no_platform_capture`, `capture`, `already_running`, `not_running`, `streaming`, `config`, `internal`, `overloaded`, `busy`, `cancelled` (a `start` that a later `stop` cancelled before its session began), `shutting_down` (a command cut off by shutdown). `dstreamy service` failures also use `service_manager` and `unsupported_platform`; `daemon` never sends them. `id` must be a string; a line without one (or that is not a JSON object, or not UTF-8) is `malformed` with `"id":null`, while a line with a string `id` but no string `cmd` is `malformed` with that `id`. Commands still running at `shutdown` or EOF get a `shutting_down` error reply when output capacity and the shutdown deadline permit delivery. Replies can arrive out of request order. `stop` and `update` without a live stream fail with `not_running`. Setting values follow the CLI flags (`codec` `"h264"`/`"h265"`, `resolution` `"WIDTHxHEIGHT"` or `"720p"`/`"900p"`/`"1080p"`, `fps` 15/30/60, `bitrate_kbps` 1000–15000, `gain` 0.0–3.0).

`start` returns `busy` if previous session-event publication cannot be confirmed within 2 s or session events are published during its preparation; no new capture or Go Live run starts in that case. A `stop` reply of `busy` means terminal-event publication is unconfirmed: media may already be stopped, but this is not a successful acknowledgement and terminal statistics are not consumed. Wait for the stopped event before retrying Start. An idle or repeated Stop still returns `not_running`.

The macOS app keeps Stop available and shows a fatal error while an operation or stream may still be active, until an actual stopped state or confirmed process termination settles it. A successful Stop reply alone does not enable another Start if its terminal callback has not been applied; an explicit repeat Stop can reconcile by terminating the daemon. Actual stopped states remain authoritative, including during Start. Any `DaemonError` rejecting Start reconciles that same daemon instance before retry is allowed, even if a terminal callback already set cancellation. A local Start timeout retains unresolved control until state or process reconciliation; it is not remote cancellation.

Limits: input lines are capped at 1 MiB, with at most 64 queued lines, 16 live commands, 64 queued replies and 64 serialized stdout messages. The engine event subscription holds 256 events by default. Exceeding the live-command limit produces `overloaded` when output permits; input, reply, stdout or event overflow starts failure shutdown with an actionable diagnostic and a nonzero exit. Events queued before a reply are written before it. The parent must read stdout continuously and wait for replies before exceeding the command limits; shutdown may abandon messages it cannot drain.

Events (daemon → app, unsolicited): `{"type":"event","event":<SessionEvent>}`, using the existing `SessionEvent` serde form (state, health with metrics, channel_moved, resumed, e2ee, error). The encryption event is `{"event":"e2ee","state":"unknown"|"encrypted"|"unencrypted","dave_protocol_version":<u16>}`. `StreamStats` carries `e2ee` and the same numeric version; unknown state must not be inferred encrypted from that number. Producer health in ADR 0004 instead uses a nullable version. DAVE end-to-end encryption is distinct from hop-by-hop SRTP. When required encryption is unavailable the session stops gracefully with `{"reason":"e2ee_unavailable"}`.

EOF on stdin means the parent is gone: stop any stream and exit. EOF, `shutdown` and the first signal start a daemon-work and stdout-drain phase bounded by 5 s. Another signal abandons that phase immediately with exit 130; reaching its bound also exits 130. After the command returns, runtime teardown has its own 1 s bound. A non-forced exit then attempts up to 100 ms of best-effort stderr draining; forced exit 130 skips that diagnostic drain. An unknown `cmd` or malformed line returns an error reply (with id `null` when there was none) and keeps the daemon running.

`StreamStats` also exposes `source_video_frames_dropped` and `source_audio_frames_dropped`, separate from pump and transport losses. Capture counters use cumulative high-water samples and survive pump restarts without double counting. A completed source stop is sampled again, including losses recorded during teardown. Backends retain the latest successful capture's counters while idle and reset them only after another successful start. If teardown times out or fails to establish its counter barrier, the observed totals remain a lower bound alongside the stop error. Health includes source video losses in video-drop metrics and source audio losses in the encoded total, not in network-not-ready audio.

The contract is pinned by golden NDJSON fixtures in `rust/crates/dstreamy-cli/tests/fixtures/daemon/`. Rust tests produce and check them, and the Swift tests decode the same files.

## Consequences

- One capture and encode implementation (Rust) instead of two.
- The app needs neither Bun, `sharp` nor the Node daemon; the release workflow bundles the Rust binary (`scripts/build.sh`).
- The app loses SCContentSharingPicker unless a picked filter can be mapped to a `window:`/`display:` id. The app's own picker over `targets` is the default.
- Protocol changes need a `protocol` bump and fixture updates on both sides.
