//! Starting, watching and ending a Go Live session, shared by `stream`, the TUI and the engine.

use std::sync::Arc;
use std::time::Duration;

use dstreamy_core::{Codec, CropRect, StreamConfig};
use dstreamy_discord::{
    DiscordClient, Session, SessionError, SessionEvent, SessionOptions, SessionState, StopReason,
    StreamOptions,
};
use dstreamy_media::{CaptureConfig, CaptureSource, VideoCodec};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, watch};
use tokio::time::timeout;

use crate::CliError;

const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

pub fn capture_config(
    target: &str,
    stream: &StreamConfig,
    crop: Option<CropRect>,
    audio_source: Option<String>,
) -> CaptureConfig {
    CaptureConfig {
        target_id: Some(target.to_owned()),
        crop,
        width: stream.resolution.width(),
        height: stream.resolution.height(),
        fps: stream.fps.get(),
        bitrate_bps: stream.bitrate_kbps * 1000,
        codec: match stream.codec {
            Codec::H264 => VideoCodec::H264,
            Codec::H265 => VideoCodec::H265,
        },
        audio: true,
        audio_source,
        audio_gain: stream.audio_gain,
    }
}

/// Returns once the run has left the previous state, so a `Stopped` seen afterwards is this run's.
pub async fn start(
    session: &Session,
    guild: String,
    channel: String,
    source: Box<dyn CaptureSource>,
    capture: CaptureConfig,
    require_e2ee: bool,
) -> Result<(), CliError> {
    let options = SessionOptions::new(StreamOptions {
        require_e2ee,
        ..StreamOptions::new(capture)
    });
    let mut state = session.state();
    state.borrow_and_update();
    session
        .start(Some(guild), channel, source, options)
        .await
        .map_err(|e| match e.error {
            SessionError::AlreadyRunning => CliError::AlreadyStreaming,
            error => CliError::Session(error.to_string()),
        })?;
    let _ = state.changed().await;
    Ok(())
}

/// A session's events, ending with a `TaskEnded` stop once its task died without reporting one, as after a panic.
pub struct SessionEvents {
    events: broadcast::Receiver<SessionEvent>,
    task: watch::Receiver<SessionState>,
    task_ended: bool,
    reported: bool,
}

impl SessionEvents {
    pub fn new(session: &Session) -> Self {
        Self {
            events: session.subscribe(),
            task: session.state(),
            task_ended: false,
            reported: false,
        }
    }

    pub async fn recv(&mut self) -> Result<SessionEvent, RecvError> {
        loop {
            if self.task_ended && !self.reported && self.events.is_empty() {
                self.reported = true;
                if !matches!(*self.task.borrow(), SessionState::Stopped { .. }) {
                    return Ok(SessionEvent::State {
                        state: SessionState::Stopped {
                            reason: StopReason::TaskEnded,
                        },
                    });
                }
            }
            tokio::select! {
                biased;
                event = self.events.recv() => return event,
                changed = self.task.changed(), if !self.task_ended => {
                    self.task_ended = changed.is_err();
                }
            }
        }
    }

    pub fn try_recv(&mut self) -> Result<SessionEvent, broadcast::error::TryRecvError> {
        self.events.try_recv()
    }
}

/// Also true once the session task is gone, whatever state it last showed.
pub fn has_stopped(session: &Session) -> bool {
    let state = session.state();
    state.has_changed().is_err() || matches!(*state.borrow(), SessionState::Stopped { .. })
}

pub async fn teardown(session: Session, client: Arc<DiscordClient>) {
    session.shutdown().await;
    close(client).await;
}

pub(crate) async fn close(client: Arc<DiscordClient>) {
    match Arc::try_unwrap(client) {
        Ok(client) => {
            let _ = timeout(CLOSE_TIMEOUT, client.close()).await;
        }
        Err(_) => {
            tracing::warn!("the Discord client is still shared; dropping it without a close frame")
        }
    }
}

pub fn stopped_cleanly(reason: &StopReason) -> bool {
    matches!(reason, StopReason::Requested | StopReason::CaptureEnded)
}
