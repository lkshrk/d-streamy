# 05 — mock discord server

Status: done
Depends: 04

## Scope

`dstreamy-mock-discord`: in-process server for tests. Main gateway WS (HELLO/heartbeat/IDENTIFY with user token → READY containing guilds+voice channels; voice state update; op 18 stream create → STREAM_CREATE + STREAM_SERVER_UPDATE; op 19 delete), voice WS (identify, select protocol, SDP answer via webrtc-rs, DAVE MLS peer via `davey`), RTP receiver that decrypts DAVE, depacketizes H.264/H.265/Opus and records frames.
Behaviour must mirror real Discord per protocol-audit.md. Scriptable faults: drop voice WS, server-initiated channel move, resume.

## Gates

- Self-test: mock driven by a minimal scripted client completes handshake
- Fault injection API covered by tests
- Global gates from PRD

## Comments
- Evidence (2026-10-21, coordinator): merged 44e7aba; macOS gates 174 passed / 4 ignored; Linux `rust/ci/linux.sh` ok. Self-tests stream test-pattern H264+Opus through Streamer and real DiscordClient with DAVE active; frames bit-identical.
- Decision: mock accepts only the announced video codec (webrtc-rs H264 partial-match caveat, see protocol-audit.md Verification).
- Known test-only gaps left: `wait_for` runs closure under state lock; voice handler sends under connection lock; `pump` helper has no stop handle.
