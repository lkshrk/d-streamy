# 11 — capture macos

Status: ready-for-agent
Depends: 03

## Scope

`dstreamy-capture-macos`: enumerate windows/displays (SCShareableContent), capture window or display with crop (sourceRect), per-app/system audio, VideoToolbox H264/H265 CBR Baseline realtime encode (port `VideoEncoder.swift`, `WindowCapture.swift`, `CaptureSession.swift` behaviour).

## Gates

- In-process encoder test with synthetic CVPixelBuffers (no TCC) produces decodable AUs (vault pattern `in-process-encoder-tests`)
- Real capture tests marked `#[ignore]` and listed in MANUAL-CHECKLIST.md
- Crate is empty/no-op on Linux build
- Global gates from PRD

## Comments
