# 11 — capture macos

Status: done
Depends: 03

## Scope

`dstreamy-capture-macos`: enumerate windows/displays (SCShareableContent), capture window or display with crop (sourceRect), per-app/system audio, VideoToolbox H264/H265 CBR Baseline realtime encode (port `VideoEncoder.swift`, `WindowCapture.swift`, `CaptureSession.swift` behaviour).

## Gates

- In-process encoder test with synthetic CVPixelBuffers (no TCC) produces decodable AUs (vault pattern `in-process-encoder-tests`)
- Real capture tests marked `#[ignore]` and listed in MANUAL-CHECKLIST.md
- Crate is empty/no-op on Linux build
- Global gates from PRD

## Comments

Evidence (2026-10-21, macOS 27.0.1 arm64 and the Linux container):
- `cd rust && cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`: pass, 110 tests, 4 ignored. dstreamy-capture-macos adds 17 unit tests (ported `CaptureDimensionsTests`, `AudioPCMConverterTests`, the `filterUserWindows` rules, target ids, Opus packetizing) and 5 in-process VideoToolbox tests.
- `tests/videotoolbox.rs`: synthetic 320x240 BGRA CVPixelBuffers, no TCC needed. H264: the first AU is a keyframe with SPS, PPS and IDR; pts increase strictly (no reordering); every AU decodes to 320x240 with the openh264 decoder. H265: the first AU carries VPS, SPS, PPS and an IRAP slice. A keyframe forced mid-stream carries SPS. The sink mode delivers without polling.
- `DSTREAMY_TARGET_VOLUME=dstreamy-target-11 rust/ci/linux.sh`: exit 0; the crate builds as an empty lib on Linux.
- `tests/screencapture.rs` (4 tests) is `#[ignore]` and listed in MANUAL-CHECKLIST.md. A run without Screen Recording permission listed windows but no displays and got no frames.
- A reviewer subagent ran on the diff. Fixed from its findings: the refcon is now leaked with `Box::leak` and freed after invalidation; dropped AUs or AUs with an unexpected NAL length force a keyframe; audio pts resync; an error stop is reported by `stop()`; the first-sample clock clamps to 0. Comments were trimmed.

Decision: bindings are `screencapturekit` 11 (Swift bridge; feature `macos_14_0` for `SCContentFilter::content_rect`/`point_pixel_scale`) for capture, and `objc2-video-toolbox`/`objc2-core-media`/`objc2-core-video`/`objc2-core-foundation` 0.3.2 for encoding. The CVPixelBuffer from a capture sample is borrowed and reinterpreted as the objc2 type, without a copy. The minimum runtime is macOS 14.
Decision: `VtEncoder` implements `VideoEncoder<Frame = CVPixelBuffer>`. Default mode queues AUs, which `encode` and `flush` return. `with_sink` hands each AU to a closure on the VideoToolbox thread; capture uses this so a frame does not wait for the next one, since ScreenCaptureKit sends no frames for a static screen.
Decision: the encoder settings are H264 Baseline AutoLevel or HEVC Main AutoLevel, AverageBitRate, RealTime, AllowFrameReordering=false and MaxKeyFrameInterval=fps*2; a failure to set any of these is an error. DataRateLimits (1.5x average, in bytes per second, while the Swift code passed bits) and ExpectedFrameRate are best-effort hints. Parameter sets come from the format description on every sync sample, with the count taken from the API. An AU whose NAL length size is not 4, or that VideoToolbox drops, forces a keyframe on the next frame.
Decision: crop uses `sourceRect` (crop pixels / point-pixel scale) with `scalesToFit`, not the Swift CPU crop. The output size fits the crop, or the whole source, into the configured width/height, keeping aspect, never upscaling, at even sizes, so `CaptureDimensions` keeps no separate stream size. A crop outside the source is `InvalidConfig`.
Decision: target ids are `display:<CGDirectDisplayID>` / `window:<CGWindowID>`; `None` means the first display. Display titles are `Display <id>`, because NSScreen names would need AppKit. Window filtering ports `filterUserWindows`. Width and height are in pixels.
Decision: audio is SCK float32 (any buffer size, planar or interleaved) → stereo i16 (gain 1.0; `CaptureConfig` has no gain yet) → 20 ms Opus frames via `OpusEncoder` at 128 kbps. Timestamps come from the sample count and resync to the capture clock when they drift more than 40 ms. Both kinds share one µs timebase, starting at the first sample of either kind. The current process's audio is excluded and the cursor hidden, as in Swift.
Decision: a stream that stops with an error closes the frame channel. The next `stop()` returns that error, and `start()` replaces the dead session instead of returning `AlreadyRunning`. Dropping a running source stops it synchronously (a brief block); call `stop()` from async code.
Decision: `build.rs` adds `-rpath /usr/lib/swift`, because screencapturekit's own rpath does not propagate. Any binary that links this crate (dstreamy-cli, issue 09/13) must emit the same link arg, or it will fail at load with `libswift_Concurrency.dylib` not found.
Decision: no changes to `dstreamy-media` were needed.
