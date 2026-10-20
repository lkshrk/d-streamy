//! Capture and encode abstractions shared by every D-Streamy capture backend.

pub mod annexb;
mod encoder;
mod error;
mod frame;
mod h264;
mod opus;
mod source;
mod target;
mod test_pattern;

pub use encoder::{AudioEncoder, I420Frame, VideoEncoder};
pub use error::MediaError;
pub use frame::{
    AudioFrame, EncodedVideoFrame, MediaFrame, OPUS_CHANNELS, OPUS_FRAME_DURATION_US,
    OPUS_FRAME_SAMPLES, OPUS_SAMPLE_RATE, VideoCodec,
};
pub use h264::OpenH264Encoder;
pub use opus::OpusEncoder;
pub use source::{CaptureConfig, CaptureSource, CropRect, FRAME_CHANNEL_CAPACITY, FrameReceiver};
pub use target::{CaptureTarget, TargetKind};
pub use test_pattern::{TEST_PATTERN_TARGET_ID, TestPatternSource};
