# 03 — media traits and test pattern

Status: ready-for-agent
Depends: 01

## Scope

`dstreamy-media`: `CaptureSource` trait (async stream of `MediaFrame::{Video(EncodedVideoFrame), Audio(AudioFrame)}`, `start/stop`, `list_targets`), `Encoder` trait for backends that encode separately, Annex B helpers (port `AnnexB.swift` + tests), SPS VUI handling only if needed.
`TestPatternSource`: generates moving pattern + sine audio, encodes H.264 Baseline via `openh264` crate (bundled build, no system deps), Opus via chosen opus crate — record choice as Decision.

## Gates

- Annex B golden tests ported
- Test: TestPatternSource emits ≥30 decodable keyframe-led H.264 AUs in 1s (decode back with openh264 decoder)
- Builds in Linux container
- Global gates from PRD

## Comments
