//! Capture and encode abstractions shared by every D-Streamy capture backend.

mod activity;
pub mod annexb;
mod audio_clock;
mod encoder;
mod error;
mod external;
mod frame;
#[cfg(feature = "openh264")]
mod h264;
mod ingest;
mod opus;
mod source;
mod target;
mod test_pattern;

pub use activity::{AUDIO_DEVICE_TARGET, AudioActivity};
pub use audio_clock::AudioClock;
pub use encoder::{AudioEncoder, VideoEncoder};
pub use error::MediaError;
pub use external::{
    AttachError, Attachment, AttachmentStats, CloseReason, EXTERNAL_TARGET_ID, ExternalHandle,
    ExternalOptions, ExternalSource, FIRST_FRAMES_WAIT, IdleMode, InterruptReason, MAX_GRACE,
    OPUS_SILENCE_FRAME, OptionsError, ProducerFrame, ProducerInfo, QUEUE_MAX_BYTES, SendError,
    SourceState, SourceStats,
};
pub use frame::{
    AudioFrame, EncodedVideoFrame, MediaFrame, OPUS_BITRATE_BPS, OPUS_CHANNELS,
    OPUS_FRAME_DURATION_US, OPUS_FRAME_SAMPLES, OPUS_SAMPLE_RATE, VideoCodec,
};
pub use ingest::{Ingest, IngestError};
pub use opus::OpusEncoder;
pub use source::{
    CaptureConfig, CaptureDrops, CaptureSource, CropRect, FRAME_CHANNEL_CAPACITY, FrameReceiver,
    send_until_stopped,
};
pub use target::{AudioSource, AudioSourceKind, CaptureTarget, TargetKind};
pub use test_pattern::{TEST_PATTERN_TARGET_ID, TestPatternSource};
