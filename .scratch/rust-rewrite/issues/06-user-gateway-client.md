# 06 — user gateway client

Status: ready-for-agent
Depends: 04,05

## Scope

`dstreamy-discord::gateway`: user-token main gateway client on tokio-tungstenite (identify with user properties like discord.js-selfbot-v13 sends them, heartbeat/ACK, resume, zlib-stream if needed). Exposes guild + voice channel listing from READY/GUILD_CREATE, voice state + stream ops, and the bridge required by `discord-stream-rs` (`handle_event` / outbound `GatewayPayload`).

## Gates

- Tests vs mock: connect, list guilds/channels, resume after drop, heartbeat timeout reconnect
- Global gates from PRD

## Comments
