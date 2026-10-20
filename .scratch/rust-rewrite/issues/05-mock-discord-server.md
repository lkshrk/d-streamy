# 05 — mock discord server

Status: ready-for-agent
Depends: 04

## Scope

`dstreamy-mock-discord`: in-process server for tests. Main gateway WS (HELLO/heartbeat/IDENTIFY with user token → READY containing guilds+voice channels; voice state update; op 18 stream create → STREAM_CREATE + STREAM_SERVER_UPDATE; op 19 delete), voice WS (identify, select protocol, SDP answer via webrtc-rs, DAVE MLS peer via `davey`), RTP receiver that decrypts DAVE, depacketizes H.264/H.265/Opus and records frames.
Behaviour must mirror real Discord per protocol-audit.md. Scriptable faults: drop voice WS, server-initiated channel move, resume.

## Gates

- Self-test: mock driven by a minimal scripted client completes handshake
- Fault injection API covered by tests
- Global gates from PRD

## Comments
