# 12 — capture linux

Status: ready-for-agent
Depends: 03

## Scope

`dstreamy-capture-linux`: ScreenCast portal via ashpd (window/monitor, persist restore_token in config), PipeWire fd → GStreamer pipeline `pipewiresrc ! videoconvert ! videocrop ! <vaapih264enc|nvh264enc|x264enc> ! h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au ! appsink` (H265 analogous), encoder auto-select with fallback. Audio: PipeWire capture of system/app node → PCM → Opus.

## Gates

- Container test: same pipeline with `videotestsrc` instead of pipewiresrc yields decodable AUs via x264enc
- Encoder selection logic unit-tested
- Portal + real capture `#[ignore]`, listed in MANUAL-CHECKLIST.md
- Global gates from PRD

## Comments
