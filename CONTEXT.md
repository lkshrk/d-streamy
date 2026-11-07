# D-Streamy: context and glossary

One bounded context. The terms below are the ones the ADRs, the code and the user-facing text use; do not drift to synonyms.

| Term | Meaning | Avoid |
|---|---|---|
| **session** | One Discord Go Live run: from `start` to `stopped`, with its voice connection, stream and stats. Held by whichever of `stream`, `tui`, `daemon` or `serve` runs it; one per account at a time (the session lock). | "stream" for the whole run |
| **source** | What feeds a session with video and audio: a `CaptureSource` implementation (portal or ScreenCaptureKit capture, the test pattern, or the external source fed by producers). | "input", "capture" for the external case |
| **producer** | A process that sends encoded H.264 and Opus to `serve` over the producer role of the IPC, for example the OBS plugin. | "client" for the media sender |
| **attachment** | One producer connection to the external source, from `attached` to `detached`. A producer may attach, drop and attach again within a session. | "producer" when the connection is meant |
| **control client** | A process on the IPC's control role: it sends the protocol 2 commands (`start`, `stop`, `status`, `login`, the account commands) and may subscribe to events. The OBS plugin is one; so is `dstreamy service status`. | "controller" |
| **service** | `dstreamy serve`: the long-running process that control clients and producers connect to. On Linux `dstreamy service` runs it under systemd. | "daemon" |
| **daemon** | `dstreamy daemon`: the protocol 1 child the macOS app spawns on stdin and stdout. Only that. | "daemon" for `serve` |
| **account** | A saved Discord login under `<config-dir>/accounts/<name>/`: a token plus the Discord user it belongs to. | "token", "login" for the saved thing |
| **default account** | The account used when no `--account` is given; `dstreamy account use` or a pick in the OBS panel sets it. `serve` follows it unless pinned with `--account`. | "active account" outside `serve` |
| **server** | A Discord guild, in every user-facing text. The API and the code keep `guild`. | "guild" in UI text |
| **channel** | A voice channel of a server; the place a session streams in. | |
| **target** | What a session captures: `display:<id>`, `window:<id>`, `portal`, `test-pattern` (test builds only) or `external`. | |
| **panel** | The plugin's widget in OBS's status bar (Start, Stop, state, counters) and, by extension, the settings dialog under Tools > D-Streamy…. | "dock" |
| **managed track** | The OBS audio track (Track 6 by default) the plugin controls per scene collection so the mix sent to Discord has no microphone. | "Discord track" for the generic case |
| **grace** | The time (`[serve] grace_secs`) a session survives with no producer attached or a fully stalled producer before it stops. Each explicit source start resets the full period. Video-only interruption with continuing audio does not consume it. | |
| **idle** | What the service sends Discord after a producer detached (`[serve] idle`). | "silence" |
| **end-to-end encryption** | DAVE media encryption, reported as `unknown`, `encrypted` or `unencrypted`. A protocol version or hop-by-hop SRTP alone is not proof. `require_e2ee` stops gracefully when unavailable. | "encrypted" for an unknown state |
| **health** | The per-second stream figures (fps, bitrate, jitter, drops, audio frames) and the levels derived from them. | "stats" for the levels |

Decisions: `docs/adr/`. External behaviour pins: `docs/specs.md`.
