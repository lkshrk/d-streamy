use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::{CaptureTarget, MediaError, MediaFrame, VideoCodec};

pub const FRAME_CHANNEL_CAPACITY: usize = 64;

/// Frames in presentation order per kind; the channel closes when the source stops.
pub type FrameReceiver = mpsc::Receiver<MediaFrame>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CropRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

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

    /// Starts capturing; fails with [`MediaError::AlreadyRunning`] if not stopped first.
    async fn start(&mut self, config: CaptureConfig) -> Result<FrameReceiver, MediaError>;

    /// Stops capturing and closes the receiver returned by `start`; a no-op when idle.
    async fn stop(&mut self) -> Result<(), MediaError>;

    /// Asks the encoder to emit a keyframe as soon as possible.
    fn request_keyframe(&self);
}
