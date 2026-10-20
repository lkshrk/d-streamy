# 02 — core domain and health

Status: done
Depends: 01

## Scope

`dstreamy-core`: `StreamConfig` (resolution ≤1080p, fps 15/30/60, bitrate, codec H264|H265, audio gain), `CaptureTarget`, `CropRect`, config file (TOML, `directories` crate path) with token read from `DSTREAMY_TOKEN` env or 0600 file.
Port health scoring + metrics: `capture/Sources/Capture/StreamHealthScoring.swift`, `CaptureMetrics.swift`, `CaptureDimensions.swift`, `CropFilter.swift`, `src/core/{frame-interval-tracker,metrics-sampler,audio-buffer,codec}.ts`. Respect vault pitfall `frame-metrics-off-by-one`.

## Gates

- Every case from the corresponding Swift/TS tests ported as Rust unit tests and passing
- Config round-trip test; token file permission test (unix)
- Global gates from PRD

## Comments

Evidence (2026-10-20):
- `cd rust && cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` on macOS: clean, `dstreamy-core` 67 passed, 0 failed.
- `DSTREAMY_TARGET_VOLUME=dstreamy-target-02 rust/ci/linux.sh` (Docker, Debian): exit 0, 67 passed.
- Ported cases: StreamHealthScoringTests (10), CaptureMetricsTests (2), CaptureDimensionsTests (3), VideoCodecTests raw values + parameter set count, frame-interval-tracker (5), metrics-sampler (3), audio-buffer (2), codec (3). Added a perfect-stream test for the `frame-metrics-off-by-one` pitfall, a config round-trip test, tests that config and token files are written 0600, and insecure-token-file rejection (unix).
- Self-review by a reviewer subagent. Fixed: config file now written 0600 and atomically (temp + rename) since it may hold a portal restore token; `save` validates first; token read via one open fd (no metadata/read race); crop checks source length before allocating.

Decision: `VideoCodecTests.testCodecTypes` (VideoToolbox `CMVideoCodecType`) is not ported; it belongs to the macOS capture backend (issue 11).
Decision: `normalizeCodec(undefined|null|123)` maps to `Codec::normalize(None | Some("123"))`; non-strings cannot reach a typed API. The config file parses codecs strictly (`"h265"` is an error), so typos are not silently streamed as H.264.
Decision: `StreamConfig` uses `bitrate_kbps: u32` (1000..=15000, default 6000) instead of the app's `bitrateMbps` slider (1–15, step 0.5); gain 0.0..=3.0, default 1.0; fps enum 15/30/60, default 30; resolution is any even WxH up to 1920x1080 or the presets 720p/900p/1080p, default 1080p, serialized as `"WxH"`.
Decision: `MetricsSampler::sample` takes `now`, the cumulative `PipelineStats` and the drained `FrameIntervalSnapshot` as arguments instead of callbacks; the caller drains the tracker on every tick, including the seeding one.
Decision: counter deltas use `saturating_sub` (u64), so a counter going backwards reads as 0 instead of the TS negative; counters are monotonic within one pipeline and the sampler is `reset` per Session.
Decision: `CaptureMetrics` uses independent atomics; a snapshot is not cross-counter consistent like the Swift lock version, but no count is ever lost or double-counted.
Decision: `CropFilter.apply` became `CropRect::apply_bgra` over a strided byte slice (no CVPixelBuffer); new tests cover it since Swift had none. `StreamHealth.compute` from the app popover is ported as `StreamHealth::compute`.
Decision: a token file with any group/other permission bit is a hard error (like ssh), not a warning. `DSTREAMY_TOKEN` takes precedence over the file. Config lives at `directories::ProjectDirs::from("me", "harke", "dstreamy")` `config_dir()/config.toml`, token at `.../token`.
Decision: worktree branch was created from `main`; reset it to `rust-rewrite` (2ff2ce6) before starting, since issue 01's scaffold lives there.
