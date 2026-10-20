use std::os::fd::AsRawFd;

use async_trait::async_trait;
use dstreamy_media::{
    CaptureConfig, CaptureSource, CaptureTarget, FrameReceiver, MediaError, TargetKind,
};
use gstreamer as gst;

use crate::pipeline::{self, AudioInput, Capture, VideoInput};
use crate::portal::{PortalSources, PortalStream};

/// The single target Linux offers: the ScreenCast portal dialog picks the actual window or monitor.
pub const PORTAL_TARGET_ID: &str = "portal";

/// Wayland/X11 capture through the ScreenCast portal, PipeWire and GStreamer.
pub struct LinuxCaptureSource {
    sources: PortalSources,
    restore_token: Option<String>,
    running: Option<Running>,
}

struct Running {
    capture: Capture,
    portal: PortalStream,
}

impl Default for LinuxCaptureSource {
    fn default() -> Self {
        Self::new()
    }
}

impl LinuxCaptureSource {
    pub fn new() -> Self {
        Self {
            sources: PortalSources::default(),
            restore_token: None,
            running: None,
        }
    }

    pub fn with_sources(mut self, sources: PortalSources) -> Self {
        self.sources = sources;
        self
    }

    /// Reuses an earlier portal selection without showing the picker, if the token is still valid.
    pub fn with_restore_token(mut self, token: Option<String>) -> Self {
        self.restore_token = token;
        self
    }

    /// The token to persist so the next session skips the picker; updated by each `start`.
    pub fn restore_token(&self) -> Option<&str> {
        self.restore_token.as_deref()
    }

    /// The GStreamer encoder element of the running capture.
    pub fn encoder(&self) -> Option<&'static str> {
        self.running.as_ref().map(|r| r.capture.encoder())
    }

    /// Why the frame channel closed early, if the pipeline failed rather than being stopped.
    pub fn last_error(&self) -> Option<String> {
        self.running.as_ref().and_then(|r| r.capture.error())
    }
}

impl Drop for LinuxCaptureSource {
    fn drop(&mut self) {
        if let Some(running) = self.running.take() {
            // Pipeline teardown blocks, so keep it off a possibly async caller's thread.
            std::thread::spawn(move || drop(running));
        }
    }
}

#[async_trait]
impl CaptureSource for LinuxCaptureSource {
    async fn list_targets(&self) -> Result<Vec<CaptureTarget>, MediaError> {
        Ok(vec![CaptureTarget {
            id: PORTAL_TARGET_ID.to_owned(),
            kind: match self.sources {
                PortalSources::Window => TargetKind::Window,
                _ => TargetKind::Display,
            },
            title: "Chosen in the screen sharing dialog".to_owned(),
            app: None,
            width: 0,
            height: 0,
        }])
    }

    async fn start(&mut self, config: CaptureConfig) -> Result<FrameReceiver, MediaError> {
        if self.running.is_some() {
            return Err(MediaError::AlreadyRunning);
        }
        if let Some(id) = config.target_id.as_deref()
            && id != PORTAL_TARGET_ID
        {
            return Err(MediaError::InvalidConfig(format!(
                "unknown target {id:?}; Linux captures through the portal only"
            )));
        }
        pipeline::validate(&config)?;
        gst::init().map_err(|e| MediaError::Backend(format!("GStreamer init: {e}")))?;
        let candidates = pipeline::installed_candidates(&config);
        if candidates.is_empty() {
            return Err(MediaError::Unsupported(format!(
                "no GStreamer encoder for {:?}",
                config.codec
            )));
        }
        let audio = if config.audio {
            Some(
                AudioInput::select(&pipeline::available_factories()).ok_or_else(|| {
                    MediaError::Unsupported("neither pulsesrc nor pipewiresrc installed".into())
                })?,
            )
        } else {
            None
        };

        let portal = PortalStream::open(self.sources, self.restore_token.as_deref()).await?;
        // Kept even if the pipeline fails: the portal invalidates the token it was given.
        if portal.restore_token.is_some() {
            self.restore_token.clone_from(&portal.restore_token);
        }
        let video = VideoInput::PipeWire {
            fd: portal.fd.as_raw_fd(),
            node_id: portal.node_id,
        };
        let started =
            tokio::task::spawn_blocking(move || Capture::start(video, audio, &config, &candidates))
                .await
                .map_err(|e| MediaError::Backend(e.to_string()))
                .and_then(|r| r);
        match started {
            Ok((capture, rx)) => {
                self.running = Some(Running { capture, portal });
                Ok(rx)
            }
            Err(e) => {
                portal.close().await;
                Err(e)
            }
        }
    }

    async fn stop(&mut self) -> Result<(), MediaError> {
        let Some(Running { capture, portal }) = self.running.take() else {
            return Ok(());
        };
        let stopped = tokio::task::spawn_blocking(move || capture.stop()).await;
        portal.close().await;
        stopped.map_err(|e| MediaError::Backend(e.to_string()))
    }

    fn request_keyframe(&self) {
        if let Some(running) = &self.running {
            running.capture.request_keyframe();
        }
    }
}
