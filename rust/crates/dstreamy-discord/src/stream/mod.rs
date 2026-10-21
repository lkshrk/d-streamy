mod link;
pub(crate) mod pump;
mod queue;
mod timebase;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use dstreamy_media::{CaptureConfig, CaptureSource, MediaError};
use tokio::task::JoinHandle;

use crate::DiscordClient;
use crate::transport::{
    ConnectionError, MediaConnection, StreamerError, VideoAttributes, VoiceEvent,
};
pub(crate) use link::{Events, initial_attributes, join_voice, open_stream};
use pump::MediaPump;

#[derive(Debug, Clone)]
pub struct StreamOptions {
    /// Passed to the source; also gives the codec and the initial video attributes.
    pub capture: CaptureConfig,
    /// Limit for each of: joining voice, creating the stream, the peer connection.
    pub step_timeout: Duration,
    /// How long to hold media back for DAVE before sending anyway.
    pub dave_timeout: Duration,
    pub max_queued_video: usize,
    pub max_queued_audio: usize,
}

impl Default for StreamOptions {
    fn default() -> Self {
        Self::new(CaptureConfig::default())
    }
}

impl StreamOptions {
    pub fn new(capture: CaptureConfig) -> Self {
        Self {
            capture,
            step_timeout: Duration::from_secs(30),
            dave_timeout: Duration::from_secs(10),
            max_queued_video: 8,
            max_queued_audio: 25,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("timed out {0}")]
    Timeout(&'static str),
    #[error(transparent)]
    Streamer(#[from] StreamerError),
    #[error(transparent)]
    Connection(#[from] ConnectionError),
    #[error(transparent)]
    Media(#[from] MediaError),
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct StreamStats {
    pub video_frames_sent: u64,
    pub video_frames_dropped: u64,
    pub audio_frames_sent: u64,
    pub audio_frames_dropped: u64,
    pub bytes_sent: u64,
    pub keyframe_requests: u64,
    /// PLI or FIR packets from viewers, each forwarded as a keyframe request.
    pub viewer_keyframe_requests: u64,
    /// Keyframes requested because DAVE became ready on the attached connection.
    pub dave_keyframe_requests: u64,
    /// Voice or stream connection resumes after a voice WS drop.
    pub resumes: u64,
    /// Video frames sent during the last second.
    pub fps: f64,
    pub last_error: Option<String>,
    pub capture_ended: bool,
    /// The voice or stream connection gave up; the stream needs restarting.
    pub connection_closed: bool,
}

pub struct StreamHandle {
    client: Arc<DiscordClient>,
    conn: MediaConnection,
    voice: MediaConnection,
    pump: Option<MediaPump>,
    watcher: JoinHandle<()>,
    attrs: Mutex<VideoAttributes>,
}

/// A failed `start_stream`, with the source handed back for a retry.
#[derive(thiserror::Error)]
#[error("starting the stream failed")]
pub struct StreamStartError {
    pub capture: Box<dyn CaptureSource>,
    #[source]
    pub error: StreamError,
}

impl std::fmt::Debug for StreamStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamStartError")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

pub async fn start_stream(
    client: Arc<DiscordClient>,
    guild_id: Option<String>,
    channel_id: String,
    source: Box<dyn CaptureSource>,
    options: StreamOptions,
) -> Result<StreamHandle, StreamStartError> {
    let attrs = initial_attributes(&options);
    let teardown = Teardown(Some(&client));
    let connected = async {
        let voice = join_voice(&client, guild_id, channel_id, options.step_timeout).await?;
        let stream = open_stream(&client, &options, attrs).await?;
        Ok::<_, StreamError>((voice, stream))
    };
    let ((voice, voice_events), (conn, stream_events)) = match connected.await {
        Ok(connected) => connected,
        Err(error) => {
            return Err(StreamStartError {
                capture: source,
                error,
            });
        }
    };
    let pump = MediaPump::start(source, &options, Some(conn.clone()))
        .await
        .map_err(|(capture, error)| StreamStartError { capture, error })?;
    teardown.defuse();

    let watcher = tokio::spawn(watch_connections(
        [voice_events, stream_events],
        pump.stats_updater(),
    ));
    Ok(StreamHandle {
        client,
        conn,
        voice,
        pump: Some(pump),
        watcher,
        attrs: Mutex::new(attrs),
    })
}

/// Ends Go Live and leaves voice unless defused, also when `start_stream` is cancelled.
struct Teardown<'a>(Option<&'a DiscordClient>);

impl Teardown<'_> {
    fn defuse(mut self) {
        self.0 = None;
    }
}

impl Drop for Teardown<'_> {
    fn drop(&mut self) {
        if let Some(client) = self.0 {
            client.stop_stream();
            client.leave_voice();
        }
    }
}

impl StreamHandle {
    fn pump(&self) -> &MediaPump {
        self.pump.as_ref().expect("pump lives until stop or drop")
    }

    pub fn stats(&self) -> StreamStats {
        self.pump().stats()
    }

    /// Announces new video attributes (op 12) after a resolution or frame-rate change.
    pub fn update_video(&self, width: u32, height: u32, fps: u32) -> Result<(), StreamError> {
        let attrs = VideoAttributes { width, height, fps };
        self.conn.set_video_attributes(Some(attrs))?;
        self.pump().set_fps(fps);
        *self.attrs.lock().unwrap_or_else(PoisonError::into_inner) = attrs;
        Ok(())
    }

    pub fn video_attributes(&self) -> VideoAttributes {
        *self.attrs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn request_keyframe(&self) {
        self.pump().request_keyframe();
    }

    // The Streamer holds one session; a newer one (reconnect, channel move) must survive this handle.
    fn end_session(&self) {
        let mut streamer = self.client.streamer();
        if streamer
            .stream_connection()
            .is_some_and(|c| c.ptr_eq(&self.conn))
        {
            streamer.stop_stream();
        }
        if streamer
            .voice_connection()
            .is_some_and(|c| c.ptr_eq(&self.voice))
        {
            streamer.leave_voice();
        }
    }

    pub async fn stop(self) -> Result<StreamStats, StreamError> {
        let (_, result) = self.stop_and_take_source().await;
        result
    }

    /// Like `stop`, and hands the stopped source back for reuse.
    pub async fn stop_and_take_source(
        mut self,
    ) -> (
        Option<Box<dyn CaptureSource>>,
        Result<StreamStats, StreamError>,
    ) {
        self.watcher.abort();
        let pump = self.pump.take().expect("pump lives until stop or drop");
        pump.detach();
        self.end_session();
        pump.stop().await
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.watcher.abort();
        if self.pump.take().is_some() {
            self.end_session();
        }
    }
}

type ConnectionEvents = [Events; 2];

async fn watch_connections([mut voice, mut stream]: ConnectionEvents, stats: pump::StatsUpdater) {
    loop {
        let (name, event) = tokio::select! {
            event = voice.recv() => ("voice", event),
            event = stream.recv() => ("stream", event),
        };
        match event {
            Some(VoiceEvent::Resumed) => stats.update(|s| s.resumes += 1),
            Some(VoiceEvent::Closed { code }) => stats.update(|s| {
                s.connection_closed = true;
                s.last_error = Some(format!("{name} connection closed (code {code:?})"));
            }),
            Some(_) => {}
            None => {
                stats.update(|s| {
                    s.connection_closed = true;
                    s.last_error = Some(format!("{name} connection ended"));
                });
                return;
            }
        }
    }
}
