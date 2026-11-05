use std::os::fd::{AsRawFd, OwnedFd};

use async_trait::async_trait;
use dstreamy_media::{
    AudioActivity, AudioSource, CaptureConfig, CaptureSource, CaptureTarget, FrameReceiver,
    MediaError, TargetKind,
};
use gstreamer as gst;

use crate::audio;
use crate::pipeline::{self, AudioInput, Capture, VideoInput};
use crate::portal::{PortalSources, PortalStream};

/// The single target Linux offers: the ScreenCast portal dialog picks the actual window or monitor.
pub const PORTAL_TARGET_ID: &str = "portal";

/// Wayland/X11 capture through the ScreenCast portal, PipeWire and GStreamer.
pub struct LinuxCaptureSource {
    sources: PortalSources,
    restore_token: Option<String>,
    running: Option<Running>,
    activity: AudioActivity,
}

/// Fields drop in order: the pipeline stops before its PipeWire fd closes and the portal session ends.
struct Running {
    capture: Capture,
    _pipewire_fd: OwnedFd,
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
            activity: AudioActivity::default(),
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

    async fn list_audio_sources(&self) -> Result<Vec<AudioSource>, MediaError> {
        tokio::task::spawn_blocking(audio::audio_sources)
            .await
            .map_err(|e| MediaError::Backend(e.to_string()))?
    }

    fn audio_activity(&self) -> Option<AudioActivity> {
        Some(self.activity.clone())
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
            if let Some(id) = config.audio_source.as_deref() {
                audio::check_not_alias(id)?;
                audio::check_known(id, &self.list_audio_sources().await.unwrap_or_default())?;
            }
            let input = AudioInput::select(&pipeline::available_factories()).ok_or_else(|| {
                MediaError::Unsupported("neither pulsesrc nor pipewiresrc installed".into())
            })?;
            if let (AudioInput::PulseMonitor, Some(id)) = (input, config.audio_source.as_deref()) {
                audio::check_pulse_name(id)?;
            }
            Some(input)
        } else {
            None
        };

        let (portal, fd) = PortalStream::open(self.sources, self.restore_token.as_deref()).await?;
        // Replaced even by none and even if the pipeline fails: the portal spends the token it was given.
        self.restore_token.clone_from(&portal.restore_token);
        let (node_id, size) = (portal.node_id, portal.size);
        let activity = config.audio.then(|| self.activity.clone());
        if let Some(activity) = &activity {
            activity.started();
        }
        // The portal moves into the task so a cancelled start cannot end the session under a starting pipeline.
        let (started, pipewire_fd, portal) = tokio::task::spawn_blocking(move || {
            let video = VideoInput::PipeWire {
                fd: fd.as_raw_fd(),
                node_id,
                size,
            };
            (
                Capture::start_with(video, audio, &config, &candidates, activity.as_ref()),
                fd,
                portal,
            )
        })
        .await
        .map_err(|e| MediaError::Backend(e.to_string()))?;
        match started {
            Ok((capture, rx)) => {
                self.running = Some(Running {
                    capture,
                    _pipewire_fd: pipewire_fd,
                    portal,
                });
                Ok(rx)
            }
            Err(e) => {
                self.activity.stopped();
                portal.close().await;
                Err(e)
            }
        }
    }

    async fn stop(&mut self) -> Result<(), MediaError> {
        self.activity.stopped();
        let Some(Running {
            capture,
            _pipewire_fd: pipewire_fd,
            portal,
        }) = self.running.take()
        else {
            return Ok(());
        };
        let portal = tokio::task::spawn_blocking(move || {
            capture.stop();
            drop(pipewire_fd);
            portal
        })
        .await
        .map_err(|e| MediaError::Backend(e.to_string()))?;
        portal.close().await;
        Ok(())
    }

    fn request_keyframe(&self) {
        if let Some(running) = &self.running {
            running.capture.request_keyframe();
        }
    }
}
