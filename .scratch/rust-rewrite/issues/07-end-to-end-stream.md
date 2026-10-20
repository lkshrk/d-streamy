# 07 — end to end stream

Status: ready-for-agent
Depends: 03,05,06

## Scope

Wire gateway + vendored streamer + `CaptureSource`: join voice, create Go Live, pump frames with correct pacing/pts, codec H264 and H265, update video attributes on resolution change (parity with `updateVideo` IPC).

## Gates

- E2E test: TestPatternSource → mock; mock decodes ≥ 95% of sent video AUs bit-identical and audio frames, for H264 and H265
- DAVE path exercised (mock verifies E2EE decrypt)
- Global gates from PRD

## Comments
