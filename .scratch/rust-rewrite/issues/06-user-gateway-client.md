# 06 — user gateway client

Status: done
Depends: 04,05

## Scope

`dstreamy-discord::gateway`: user-token main gateway client on tokio-tungstenite (identify with user properties like discord.js-selfbot-v13 sends them, heartbeat/ACK, resume, zlib-stream if needed). Exposes guild + voice channel listing from READY/GUILD_CREATE, voice state + stream ops, and the bridge required by `discord-stream-rs` (`handle_event` / outbound `GatewayPayload`).

## Gates

- Tests vs mock: connect, list guilds/channels, resume after drop, heartbeat timeout reconnect
- Global gates from PRD

## Comments

Evidence (2026-10-21, commit `5082944` on top of `rust-rewrite` `3cdbbe8`):

- `cd rust && cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` on macOS: pass. `dstreamy-discord`: 7 unit, `tests/gateway.rs` 13, `tests/bridge.rs` 1 (21 total); the gateway suite was run 15+ times in a row without a failure.
- `DSTREAMY_TARGET_VOLUME=dstreamy-target-06 rust/ci/linux.sh`: exit 0, same counts.
- Tests run against a loopback WS server inside `crates/dstreamy-discord/tests/common`; no test touches the default gateway URL.
- Covered: IDENTIFY shape, READY fixture (`tests/fixtures/ready.json`) into guilds/voice+stage channels, GUILD_CREATE/CHANNEL_CREATE/CHANNEL_DELETE/GUILD_DELETE cache updates, heartbeat seq + ACK, server op 1, missed ACK → close 4000 → RESUME on `resume_gateway_url`, op 7 → RESUME, op 9 true → RESUME / false → re-IDENTIFY, close 4009 → re-IDENTIFY, 4004 fatal, attempt limit, READY timeout, zlib-stream split across frames, ops queued until READY/RESUMED, and the Streamer bridge (VOICE_STATE_UPDATE/VOICE_SERVER_UPDATE reach the Streamer, which starts a voice WS IDENTIFY with the session and token; its op 4/18/22/19 go out on the gateway; channel-move retarget).
- Self-review: reviewer subagent on the diff; fixed: zero-delay reconnect loop, READY/RESUMED deadline, commands lost on write failure, write/close timeouts, heartbeat catch-up after stalls, inflate size cap, non-`wss` resume URL accepted for a `wss` gateway, bounded `close()`, a test that could not detect a reconnect.
- Tests against `dstreamy-mock-discord` (connect, list guilds/channels, resume after drop, heartbeat timeout reconnect) are deferred to issue 07, since the mock is being built in parallel.

Public API (`dstreamy_discord`):
- `GatewayClient::connect(GatewayOptions) -> Result<GatewayClient, GatewayError>` (waits for READY); `user()`, `subscribe() -> broadcast::Receiver<GatewayEvent>`, `guilds()`, `voice_channels(guild_id)`, `latency()`, `is_closed()`, `send(op, d)`, `send_payload(GatewayPayload)`, `update_voice_state(guild, channel, self_mute, self_deaf, self_video)`, `stream_create(guild, channel)`, `stream_set_paused(key, paused)`, `stream_delete(key)`, `close()`.
- `DiscordClient::connect(GatewayOptions)`: owns the gateway and a `transport::Streamer`; `join_voice`, `create_stream`, `stop_stream`, `leave_voice`, `set_voice_channel` (channel move), `streamer()` guard, `guilds()`, `voice_channels()`, `subscribe()`, `close()`.
- `GatewayOptions::new(SecretString)` (default URL `wss://gateway.discord.gg`, `.with_url()`), `GatewayEvent::{Ready, Resumed, VoiceStateUpdate, GuildsChanged, Dispatch, InvalidSession, Disconnected, Closed}`.

Decision: identify exactly like discord.js-selfbot-v13 3.7.1 (`capabilities: 0`, desktop-client `properties`, presence `online`/`afk: true`, `compress: false`, `client_state.guild_versions`), gateway v9, JSON encoding, no intents.
Decision: zlib-stream transport compression on by default (`GatewayOptions::compress`); the reference enables it whenever `zlib-sync` is installed, and it keeps large READY payloads small. dstreamy's Bun build had no `zlib-sync`, so it ran uncompressed; tests cover both.
Decision: heartbeat with Discord's jitter (first beat at `interval * rand`); a heartbeat due while the previous one is unacknowledged closes with 4000 and resumes (the reference resets the session instead; resuming keeps voice state).
Decision: op 9 is handled on the same socket like the reference: resumable → RESUME at once, otherwise re-IDENTIFY after a random 1–5 s.
Decision: close codes 4004 and 4010–4014 are fatal; 4007/4009 drop the session and re-identify; anything else resumes. Every reconnect waits a jittered exponential backoff (min 1 s, max 60 s); after 10 consecutive attempts that never reach READY/RESUMED the client stops with `GatewayError::Exhausted`. Higher-level retry belongs to the session supervisor (issue 08).
Decision: outbound ops are queued until READY/RESUMED and rate-limited to 110 per 60 s (10 reserved for heartbeat/identify), matching the reference's 120/60 s bucket.
Decision: no GUILD_SUBSCRIPTIONS_BULK (op 37) after READY; voice and Go Live don't need it and it only adds traffic.
Decision: the Streamer is created after READY (it needs the user id) and fed through a dedicated ordered mpsc (`GatewayOptions::dispatch_tx`) rather than the broadcast channel, so a lagging subscriber can't drop a VOICE_SERVER_UPDATE.
Decision: rustls with webpki roots and the ring provider, passed explicitly as the connector, because the vendored crate turns on tokio-tungstenite's `native-tls` and that would otherwise win for `wss://`. The token is a `secrecy::SecretString` and only exposed when building IDENTIFY/RESUME.
Decision: a `resume_gateway_url` that is not `wss://` is ignored when the configured gateway is `wss://`, so the token is never sent in cleartext.
Decision: voice channel cache = guild channel types 2 (voice) and 13 (stage), ordered by `(position, id)`; guilds keep Discord's order; unavailable guilds stay listed with `available: false`.

