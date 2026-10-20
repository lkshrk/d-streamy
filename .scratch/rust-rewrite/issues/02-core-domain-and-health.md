# 02 — core domain and health

Status: ready-for-agent
Depends: 01

## Scope

`dstreamy-core`: `StreamConfig` (resolution ≤1080p, fps 15/30/60, bitrate, codec H264|H265, audio gain), `CaptureTarget`, `CropRect`, config file (TOML, `directories` crate path) with token read from `DSTREAMY_TOKEN` env or 0600 file.
Port health scoring + metrics: `capture/Sources/Capture/StreamHealthScoring.swift`, `CaptureMetrics.swift`, `CaptureDimensions.swift`, `CropFilter.swift`, `src/core/{frame-interval-tracker,metrics-sampler,audio-buffer,codec}.ts`. Respect vault pitfall `frame-metrics-off-by-one`.

## Gates

- Every case from the corresponding Swift/TS tests ported as Rust unit tests and passing
- Config round-trip test; token file permission test (unix)
- Global gates from PRD

## Comments
