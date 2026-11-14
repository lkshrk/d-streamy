use std::os::fd::{AsRawFd, OwnedFd};

use async_trait::async_trait;
use dstreamy_media::{
    AudioActivity, AudioSource, CaptureConfig, CaptureSource, CaptureTarget, FrameReceiver,
    MediaError, TargetKind,
};
use tokio::task::JoinHandle;

use crate::audio;
use crate::encoder;
use crate::pipeline::{self, AudioInput, AudioOptions, Capture, VideoInput};
use crate::portal::PortalStream;

/// The single target Linux offers: the ScreenCast portal dialog picks the actual window or monitor.
pub const PORTAL_TARGET_ID: &str = "portal";

/// Wayland/X11 capture through the ScreenCast portal, PipeWire and GStreamer.
pub struct LinuxCaptureSource {
    restore_token: Option<String>,
    running: Option<Running>,
    drops: Option<pipeline::Ender>,
    activity: AudioActivity,
}

/// Fields drop in order: the pipeline stops before its PipeWire fd closes and the portal session ends.
struct Running {
    closed_watch: Option<ClosedWatch>,
    capture: Capture,
    _pipewire_fd: OwnedFd,
    portal: Option<PortalStream>,
}

/// Ends the capture when the portal session closes under it; aborted before a requested stop.
struct ClosedWatch(JoinHandle<()>);

impl ClosedWatch {
    async fn stop(mut self) {
        self.0.abort();
        let _ = (&mut self.0).await;
    }
}

impl Drop for ClosedWatch {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Default for LinuxCaptureSource {
    fn default() -> Self {
        Self::new()
    }
}

impl LinuxCaptureSource {
    pub fn new() -> Self {
        Self {
            restore_token: None,
            running: None,
            drops: None,
            activity: AudioActivity::default(),
        }
    }

    /// Reuses an earlier portal selection without showing the picker, if the token is still valid.
    pub fn with_restore_token(mut self, token: Option<String>) -> Self {
        self.restore_token = token;
        self
    }

    /// The token to persist so the next session skips the picker; updated by each `start`, cleared when the share went away.
    pub fn restore_token(&self) -> Option<&str> {
        self.restore_token.as_deref()
    }

    /// The user ended the share, so the next start shows the picker instead of restoring the same source.
    fn forget_token_if_gone(&mut self, error: &MediaError) {
        if matches!(error, MediaError::Backend(message) if message == pipeline::SOURCE_GONE) {
            self.restore_token = None;
        }
    }

    pub(crate) fn retain_drops(&mut self, drops: pipeline::Ender) {
        self.drops = Some(drops);
    }

    /// The GStreamer encoder element of the running capture.
    pub fn encoder(&self) -> Option<&'static str> {
        self.running.as_ref().map(|r| r.capture.encoder())
    }

    /// Frames lost before encoding since the latest successful start, retained after stop.
    pub(crate) fn dropped_frames(&self) -> u64 {
        self.drops
            .as_ref()
            .map_or(0, pipeline::Ender::dropped_frames)
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
            kind: TargetKind::Display,
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

    fn capture_drops(&self) -> dstreamy_media::CaptureDrops {
        dstreamy_media::CaptureDrops {
            video_frames: self.dropped_frames(),
            audio_frames: 0,
        }
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
        pipeline::init_gstreamer()?;
        let candidates = encoder::installed_candidates(&config);
        if candidates.is_empty() {
            return Err(pipeline::no_encoder(config.codec));
        }
        let audio = if config.audio {
            if let Some(id) = config.audio_source.as_deref() {
                audio::check_not_alias(id)?;
                audio::check_known(id, &self.list_audio_sources().await.unwrap_or_default())?;
            }
            let input = AudioInput::select(&encoder::available_factories()).ok_or_else(|| {
                MediaError::Unsupported("neither pulsesrc nor pipewiresrc installed".into())
            })?;
            if let (AudioInput::PulseMonitor, Some(id)) = (input, config.audio_source.as_deref()) {
                audio::check_pulse_name(id)?;
            }
            Some(input)
        } else {
            None
        };

        let (portal, fd) = PortalStream::open(self.restore_token.as_deref()).await?;
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
                Capture::start_with(
                    video,
                    audio,
                    &config,
                    &candidates,
                    activity.as_ref(),
                    &AudioOptions::default(),
                ),
                fd,
                portal,
            )
        })
        .await
        .map_err(|e| MediaError::Backend(e.to_string()))?;
        self.adopt(started, pipewire_fd, Some(portal)).await
    }

    /// Returns the pipeline failure, if the capture ended on its own rather than by this call.
    async fn stop(&mut self) -> Result<(), MediaError> {
        self.activity.stopped();
        let Some(Running {
            closed_watch,
            capture,
            _pipewire_fd: pipewire_fd,
            portal,
        }) = self.running.take()
        else {
            return Ok(());
        };
        if let Some(watch) = closed_watch {
            watch.stop().await;
        }
        let (result, portal) = tokio::task::spawn_blocking(move || {
            let error = capture.error();
            let stopped = capture.stop();
            drop(pipewire_fd);
            (
                error.map_or(stopped, |message| Err(MediaError::Backend(message))),
                portal,
            )
        })
        .await
        .map_err(|e| MediaError::Backend(e.to_string()))?;
        if let Some(portal) = portal {
            portal.close().await;
        }
        if let Err(e) = &result {
            self.forget_token_if_gone(e);
        }
        result
    }

    fn request_keyframe(&self) {
        if let Some(running) = &self.running {
            running.capture.request_keyframe();
        }
    }
}

impl LinuxCaptureSource {
    /// Takes over a started pipeline, or cleans up after one that failed to start.
    async fn adopt(
        &mut self,
        started: Result<(Capture, FrameReceiver), MediaError>,
        pipewire_fd: OwnedFd,
        mut portal: Option<PortalStream>,
    ) -> Result<FrameReceiver, MediaError> {
        match started {
            Ok((capture, rx)) => {
                let closed_watch = portal
                    .as_mut()
                    .and_then(PortalStream::closed)
                    .map(|closed| {
                        let ender = capture.ender();
                        ClosedWatch(tokio::spawn(async move {
                            if closed.await.is_ok() {
                                ender.fail(pipeline::SOURCE_GONE);
                            }
                        }))
                    });
                self.retain_drops(capture.ender());
                self.running = Some(Running {
                    closed_watch,
                    capture,
                    _pipewire_fd: pipewire_fd,
                    portal,
                });
                Ok(rx)
            }
            Err(e) => {
                self.activity.stopped();
                if let Some(portal) = portal {
                    portal.close().await;
                }
                self.forget_token_if_gone(&e);
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> LinuxCaptureSource {
        LinuxCaptureSource::new().with_restore_token(Some("monitor".into()))
    }

    fn fd() -> OwnedFd {
        std::fs::File::open("/dev/null").unwrap().into()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_share_that_went_away_while_streaming_forgets_the_restore_token() {
        let mut source = source();
        let (capture, rx) = pipeline::tests::live_video_capture();
        let ender = capture.ender();
        source.adopt(Ok((capture, rx)), fd(), None).await.unwrap();
        ender.fail(pipeline::SOURCE_GONE);
        let stopped = source.stop().await.unwrap_err().to_string();
        assert!(stopped.contains(pipeline::SOURCE_GONE), "{stopped}");
        assert_eq!(source.restore_token(), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_requested_stop_or_another_failure_keeps_the_restore_token() {
        let mut source = source();
        let (capture, rx) = pipeline::tests::live_video_capture();
        source.adopt(Ok((capture, rx)), fd(), None).await.unwrap();
        source.stop().await.unwrap();
        assert_eq!(source.restore_token(), Some("monitor"));

        let (capture, rx) = pipeline::tests::live_video_capture();
        let ender = capture.ender();
        source.adopt(Ok((capture, rx)), fd(), None).await.unwrap();
        ender.fail("encoder failed");
        assert!(source.stop().await.is_err());
        assert_eq!(source.restore_token(), Some("monitor"));
    }

    #[tokio::test]
    async fn a_start_that_found_the_share_gone_forgets_the_restore_token() {
        let mut source = source();
        let other = MediaError::Backend("no encoder works".into());
        assert!(source.adopt(Err(other), fd(), None).await.is_err());
        assert_eq!(source.restore_token(), Some("monitor"));
        let gone = MediaError::Backend(pipeline::SOURCE_GONE.into());
        assert!(source.adopt(Err(gone), fd(), None).await.is_err());
        assert_eq!(source.restore_token(), None);
    }
}
