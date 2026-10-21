# 08 — session supervisor

Status: ready-for-agent
Depends: 07

## Scope

`dstreamy-core::Session` state machine (Idle → Connecting → Streaming → Reconnecting → Stopped) driving streamer: auto-reconnect with backoff, channel move follow (patch channelId, no rejoin — vault pitfall `discord-channel-move`), graceful shutdown (vault `daemon-epipe-log-loop`: logging must never loop on broken output), health metrics events stream. File logger with size cap.

## Gates

- Mock fault tests: voice WS drop → resumes streaming; server channel move → continues without rejoin; stop → clean teardown, no leaked tasks (tokio test with timeout)
- Global gates from PRD

## Comments
- Carried from 07 (coordinator, 2026-10-21):
  - Health is poll-only (`StreamHandle::stats`). Add a status watch / `closed()` future with typed reason.
  - Split frame pump from Discord session so reconnect/channel move doesn't restart the source.
  - Return the source from `stop()` and on errors so retries can reuse it.
  - Route viewer PLI to `request_keyframe` (vendored transport ignores PLI today).
  - Timebase offset shared across tracks; a single-track restart shifts the other.
  - Streamer holds one Go Live session; supervisor must own exactly one live handle.
  - Vault pitfall `davey-first-frame-nonce`: first video keyframe can be lost at start; request a keyframe shortly after DAVE becomes active.
