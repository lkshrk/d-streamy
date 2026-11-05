use async_trait::async_trait;
pub use dstreamy_core::CropRect;
use tokio::sync::mpsc;

use crate::{AudioActivity, AudioSource, CaptureTarget, MediaError, MediaFrame, VideoCodec};

pub const FRAME_CHANNEL_CAPACITY: usize = 64;

/// Frames in presentation order per kind; the channel closes when the source stops.
pub type FrameReceiver = mpsc::Receiver<MediaFrame>;

#[derive(Debug, Clone, PartialEq)]
pub struct CaptureConfig {
    /// `None` lets the backend choose (main display, or the portal picker on Wayland).
    pub target_id: Option<String>,
    pub crop: Option<CropRect>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub codec: VideoCodec,
    pub audio: bool,
    /// An [`AudioSource`] id; `None` captures what plays on the default output.
    pub audio_source: Option<String>,
    /// Linear gain applied to captured audio before encoding.
    pub audio_gain: f32,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            target_id: None,
            crop: None,
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_bps: 2_500_000,
            codec: VideoCodec::H264,
            audio: true,
            audio_source: None,
            audio_gain: 1.0,
        }
    }
}

impl CaptureConfig {
    pub fn keyframe_interval_frames(&self) -> u32 {
        self.fps * 2
    }
}

/// A capture backend that yields encoded video and Opus audio.
#[async_trait]
pub trait CaptureSource: Send + Sync {
    async fn list_targets(&self) -> Result<Vec<CaptureTarget>, MediaError>;

    /// Audio devices `CaptureConfig::audio_source` may name; empty when the backend picks its own.
    async fn list_audio_sources(&self) -> Result<Vec<AudioSource>, MediaError> {
        Ok(Vec::new())
    }

    /// A handle that reports silence in the captured audio; `None` if the backend cannot tell.
    fn audio_activity(&self) -> Option<AudioActivity> {
        None
    }

    /// Starts capturing; fails with [`MediaError::AlreadyRunning`] if not stopped first.
    async fn start(&mut self, config: CaptureConfig) -> Result<FrameReceiver, MediaError>;

    /// Stops capturing and closes the receiver returned by `start`; a no-op when idle.
    async fn stop(&mut self) -> Result<(), MediaError>;

    /// Asks the encoder to emit a keyframe as soon as possible.
    fn request_keyframe(&self);
}
