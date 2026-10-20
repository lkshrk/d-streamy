# 03 — media traits and test pattern

Status: done
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

Evidence (2026-10-20, macOS arm64 and the Linux container):
- `cd rust && cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`: pass. dstreamy-media has 11 unit tests (the 6 ported `AnnexBTests` plus 5 Annex B split/type tests) and 10 integration tests.
- `DSTREAMY_TARGET_VOLUME=dstreamy-target-03 rust/ci/linux.sh`: exit 0, the same 21 tests pass.
- `tests/test_pattern.rs::emits_one_second_of_decodable_keyframe_led_h264`: the first AU is an IDR carrying SPS and PPS; there are ≥30 AUs with pts < 1 s, and each decodes to a 320x180 picture with the openh264 decoder.
- Audio: ≥49 Opus packets in 1 s at 20 ms pts steps, each decodes to 960 stereo samples of a non-silent 440 Hz tone.

Decision: Opus via `unsafe-libopus` 0.2 (libopus 1.3 transpiled to Rust by c2rust). Every bundled-C option (`opus` 0.4 via `opusic-sys`, `audiopus_sys`, `opus-head-sys`) builds libopus with CMake, which the CI image lacks, and stock macOS has no system libopus. The pure-Rust port needs no system library or build tool on either OS. `OpusEncoder` wraps it behind the `AudioEncoder` trait, so it can be swapped later.
Decision: H.264 via `openh264` 0.9 with its default `source` feature (bundled OpenH264 built with `cc`): Constrained Baseline, bitrate rate control, frame skipping off, an IDR every `fps * 2` frames as in the Swift `VideoEncoder`.
Decision: `CaptureSource` is object-safe via `async-trait`, so `Box<dyn CaptureSource>` can pick a backend at runtime. `start` returns a bounded `tokio::sync::mpsc::Receiver<MediaFrame>` (capacity 64) that closes on `stop`, on encoder failure, or when the source is dropped.
Decision: the issue's `Encoder` trait is split into `VideoEncoder` and `AudioEncoder`. `VideoEncoder` has an associated `Frame` input type, so VideoToolbox can take a `CVPixelBuffer` while OpenH264 takes an `I420Frame`, and it returns 0..n AUs. `AudioEncoder` turns one 20 ms 48 kHz stereo i16 frame into an `AudioFrame`.
Decision: `CaptureTarget`, `TargetKind`, `CropRect` and `CaptureConfig` live in `dstreamy-media` for now; `dstreamy-core` can map to them once it lands.
Decision: no SPS VUI rewriting was needed; OpenH264 already prepends SPS and PPS to every IDR.
