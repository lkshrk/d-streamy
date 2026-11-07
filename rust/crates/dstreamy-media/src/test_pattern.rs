use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use async_trait::async_trait;

use crate::{
    CaptureConfig, CaptureSource, CaptureTarget, FrameReceiver, MediaError, TargetKind, VideoCodec,
};

#[cfg(feature = "openh264")]
mod generator;

/// The `target` that streams the built-in test pattern without capturing.
pub const TEST_PATTERN_TARGET_ID: &str = "test-pattern";

/// Whether this build can stream the test pattern; release builds ship no H.264 software encoder.
pub const TEST_PATTERN_AVAILABLE: bool = cfg!(feature = "openh264");

/// Synthetic source: a moving pattern encoded as H.264 Baseline plus a 440 Hz tone as Opus.
pub struct TestPatternSource {
    #[cfg_attr(not(feature = "openh264"), allow(dead_code))]
    realtime: bool,
    running: Option<Running>,
    keyframe_requested: Arc<AtomicBool>,
}

struct Running {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl Default for TestPatternSource {
    fn default() -> Self {
        Self::new()
    }
}

impl TestPatternSource {
    /// Paces frames in real time, like a capture device.
    pub fn new() -> Self {
        Self {
            realtime: true,
            running: None,
            keyframe_requested: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Generates frames as fast as the receiver consumes them.
    pub fn unpaced() -> Self {
        let mut source = Self::new();
        source.realtime = false;
        source
    }
}

#[async_trait]
impl CaptureSource for TestPatternSource {
    async fn list_targets(&self) -> Result<Vec<CaptureTarget>, MediaError> {
        if !TEST_PATTERN_AVAILABLE {
            return Ok(Vec::new());
        }
        Ok(vec![CaptureTarget {
            id: TEST_PATTERN_TARGET_ID.to_owned(),
            kind: TargetKind::Display,
            title: "Test pattern".to_owned(),
            app: None,
            width: 1280,
            height: 720,
        }])
    }

    async fn start(&mut self, config: CaptureConfig) -> Result<FrameReceiver, MediaError> {
        if self.running.is_some() {
            return Err(MediaError::AlreadyRunning);
        }
        if config.codec != VideoCodec::H264 {
            return Err(MediaError::Unsupported(format!(
                "test pattern encodes H.264 only, not {:?}",
                config.codec
            )));
        }
        if config.fps == 0 {
            return Err(MediaError::InvalidConfig("fps must be positive".to_owned()));
        }
        #[cfg(not(feature = "openh264"))]
        return Err(MediaError::Unsupported(
            "this build has no H.264 software encoder, so the test pattern is unavailable; \
             stream a display, window or portal target instead"
                .to_owned(),
        ));
        #[cfg(feature = "openh264")]
        {
            let (rx, stop, thread) =
                generator::spawn(config, self.realtime, self.keyframe_requested.clone())?;
            self.running = Some(Running { stop, thread });
            Ok(rx)
        }
    }

    async fn stop(&mut self) -> Result<(), MediaError> {
        let Some(running) = self.running.take() else {
            return Ok(());
        };
        running.stop.store(true, Ordering::Relaxed);
        tokio::task::spawn_blocking(move || running.thread.join())
            .await
            .map_err(|e| MediaError::Backend(e.to_string()))?
            .map_err(|_| MediaError::Backend("test pattern thread panicked".to_owned()))
    }

    fn request_keyframe(&self) {
        self.keyframe_requested.store(true, Ordering::Relaxed);
    }
}

impl Drop for TestPatternSource {
    fn drop(&mut self) {
        if let Some(running) = &self.running {
            running.stop.store(true, Ordering::Relaxed);
        }
    }
}
