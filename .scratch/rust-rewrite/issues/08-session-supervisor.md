# 08 — session supervisor

Status: ready-for-agent
Depends: 07

## Scope

`dstreamy-core::Session` state machine (Idle → Connecting → Streaming → Reconnecting → Stopped) driving streamer: auto-reconnect with backoff, channel move follow (patch channelId, no rejoin — vault pitfall `discord-channel-move`), graceful shutdown (vault `daemon-epipe-log-loop`: logging must never loop on broken output), health metrics events stream. File logger with size cap.

## Gates

- Mock fault tests: voice WS drop → resumes streaming; server channel move → continues without rejoin; stop → clean teardown, no leaked tasks (tokio test with timeout)
- Global gates from PRD

## Comments
