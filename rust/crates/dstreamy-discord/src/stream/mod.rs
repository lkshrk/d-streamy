mod queue;
mod timebase;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use dstreamy_media::{CaptureConfig, CaptureSource, FrameReceiver, MediaError, MediaFrame};
use tokio::sync::{Notify, RwLock, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

use crate::DiscordClient;
use crate::transport::{
    ConnectionError, MediaConnection, StreamerError, VideoAttributes, VoiceEvent,
};
use queue::{Effects, FrameQueue};
use timebase::{Timebase, Track};

const POLL: Duration = Duration::from_millis(10);
const TASK_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
const FPS_WINDOW: Duration = Duration::from_secs(1);

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

#[derive(Debug, Clone, Default, PartialEq)]
pub struct StreamStats {
    pub video_frames_sent: u64,
    pub video_frames_dropped: u64,
    pub audio_frames_sent: u64,
    pub audio_frames_dropped: u64,
    pub bytes_sent: u64,
    pub keyframe_requests: u64,
    /// Voice or stream connection resumes after a voice WS drop.
    pub resumes: u64,
    /// Video frames sent during the last second.
    pub fps: f64,
    pub last_error: Option<String>,
    pub capture_ended: bool,
    /// The voice or stream connection gave up; the stream needs restarting.
    pub connection_closed: bool,
}

#[derive(Default)]
struct StatsState {
    stats: StreamStats,
    video_sent_at: VecDeque<Instant>,
}

impl StatsState {
    fn apply(&mut self, effects: Effects) {
        self.stats.video_frames_dropped += effects.dropped_video;
        self.stats.audio_frames_dropped += effects.dropped_audio;
    }

    fn video_sent(&mut self, bytes: usize, now: Instant) {
        self.stats.video_frames_sent += 1;
        self.stats.bytes_sent += bytes as u64;
        self.video_sent_at.push_back(now);
        self.prune(now);
    }

    fn prune(&mut self, now: Instant) {
        while self
            .video_sent_at
            .front()
            .is_some_and(|t| now.duration_since(*t) >= FPS_WINDOW)
        {
            self.video_sent_at.pop_front();
        }
    }
}

struct Shared {
    queue: Mutex<FrameQueue>,
    frame_ready: Notify,
    stats: Mutex<StatsState>,
    configured_fps: AtomicU32,
    source: RwLock<Box<dyn CaptureSource>>,
}

impl Shared {
    fn queue(&self) -> MutexGuard<'_, FrameQueue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn stats(&self) -> MutexGuard<'_, StatsState> {
        self.stats.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn handle(&self, effects: Effects) {
        self.stats().apply(effects);
        if effects.request_keyframe {
            self.request_keyframe();
        }
    }

    fn request_keyframe(&self) {
        // Only `stop` holds the write lock, and then a keyframe is moot.
        if let Ok(source) = self.source.try_read() {
            source.request_keyframe();
            self.stats().stats.keyframe_requests += 1;
        }
    }

    fn error(&self, error: impl ToString) {
        self.stats().stats.last_error = Some(error.to_string());
    }
}

pub struct StreamHandle {
    client: Arc<DiscordClient>,
    conn: MediaConnection,
    shared: Arc<Shared>,
    shutdown: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
    attrs: Mutex<VideoAttributes>,
    stopped: bool,
}

pub async fn start_stream(
    client: Arc<DiscordClient>,
    guild_id: Option<String>,
    channel_id: String,
    mut source: Box<dyn CaptureSource>,
    options: StreamOptions,
) -> Result<StreamHandle, StreamError> {
    let capture = options.capture.clone();
    let attrs = VideoAttributes {
        width: capture.width,
        height: capture.height,
        fps: capture.fps,
    };
    let teardown = Teardown(Some(&client));
    let (conn, events) = connect(&client, guild_id, channel_id, &options, attrs).await?;
    let frames = source.start(capture).await?;
    teardown.defuse();

    let shared = Arc::new(Shared {
        queue: Mutex::new(FrameQueue::new(
            options.max_queued_video,
            options.max_queued_audio,
        )),
        frame_ready: Notify::new(),
        stats: Mutex::new(StatsState::default()),
        configured_fps: AtomicU32::new(attrs.fps),
        source: RwLock::new(source),
    });
    let (shutdown, shutdown_rx) = watch::channel(false);
    let tasks = vec![
        tokio::spawn(read_frames(frames, shared.clone(), shutdown_rx.clone())),
        tokio::spawn(send_frames(conn.clone(), shared.clone())),
        tokio::spawn(watch_connections(events, shared.clone(), shutdown_rx)),
    ];
    Ok(StreamHandle {
        client,
        conn,
        shared,
        shutdown,
        tasks,
        attrs: Mutex::new(attrs),
        stopped: false,
    })
}

type ConnectionEvents = [mpsc::UnboundedReceiver<VoiceEvent>; 2];

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

async fn connect(
    client: &DiscordClient,
    guild_id: Option<String>,
    channel_id: String,
    options: &StreamOptions,
    attrs: VideoAttributes,
) -> Result<(MediaConnection, ConnectionEvents), StreamError> {
    let step = options.step_timeout;
    let mut voice = client.join_voice(guild_id, channel_id);
    timeout(step, voice.ready())
        .await
        .map_err(|_| StreamError::Timeout("joining voice"))??;
    let mut stream = client.create_stream()?;
    let conn = timeout(step, stream.ready())
        .await
        .map_err(|_| StreamError::Timeout("creating the stream"))??;
    timeout(step, async {
        while !conn.is_ready().await {
            sleep(POLL).await;
        }
    })
    .await
    .map_err(|_| StreamError::Timeout("connecting the peer connection"))?;
    let mut dave_version = None;
    while let Ok(event) = stream.events.try_recv() {
        if let VoiceEvent::SelectProtocolAck {
            dave_version: v, ..
        } = event
        {
            dave_version = Some(v);
        }
    }
    if dave_version != Some(0) {
        // A failed DAVE handshake should not block the stream; frames then go out unencrypted.
        let _ = timeout(options.dave_timeout, async {
            while !conn.dave_ready() {
                sleep(POLL).await;
            }
        })
        .await;
    }

    conn.set_packetizer(codec_name(options.capture.codec))
        .await?;
    conn.set_speaking(true)?;
    conn.set_video_attributes(Some(attrs))?;
    Ok((conn, [voice.events, stream.events]))
}

fn codec_name(codec: dstreamy_media::VideoCodec) -> &'static str {
    match codec {
        dstreamy_media::VideoCodec::H264 => "H264",
        dstreamy_media::VideoCodec::H265 => "H265",
    }
}

impl StreamHandle {
    pub fn stats(&self) -> StreamStats {
        let mut state = self.shared.stats();
        state.prune(Instant::now());
        let mut stats = state.stats.clone();
        stats.fps = state.video_sent_at.len() as f64 / FPS_WINDOW.as_secs_f64();
        stats
    }

    /// Announces new video attributes (op 12) after a resolution or frame-rate change.
    pub fn update_video(&self, width: u32, height: u32, fps: u32) -> Result<(), StreamError> {
        let attrs = VideoAttributes { width, height, fps };
        self.conn.set_video_attributes(Some(attrs))?;
        self.shared.configured_fps.store(fps, Ordering::Relaxed);
        *self.attrs.lock().unwrap_or_else(PoisonError::into_inner) = attrs;
        Ok(())
    }

    pub fn video_attributes(&self) -> VideoAttributes {
        *self.attrs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn request_keyframe(&self) {
        self.shared.request_keyframe();
    }

    /// Dropping the handle instead aborts the pump and ends Go Live, but leaves the source to its own `Drop`.
    pub async fn stop(mut self) -> Result<StreamStats, StreamError> {
        let _ = self.shutdown.send(true);
        {
            let mut queue = self.shared.queue();
            queue.close();
            queue.clear();
        }
        self.shared.frame_ready.notify_one();
        for task in &mut self.tasks {
            if timeout(TASK_JOIN_TIMEOUT, &mut *task).await.is_err() {
                task.abort();
                let _ = task.await;
            }
        }
        self.client.stop_stream();
        self.client.leave_voice();
        self.stopped = true;
        let stopped = timeout(TASK_JOIN_TIMEOUT, async {
            self.shared.source.write().await.stop().await
        })
        .await
        .map_err(|_| StreamError::Timeout("stopping the capture source"))?;
        stopped?;
        Ok(self.stats())
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        if self.stopped {
            return;
        }
        let _ = self.shutdown.send(true);
        for task in &self.tasks {
            task.abort();
        }
        self.client.stop_stream();
        self.client.leave_voice();
    }
}

async fn read_frames(
    mut frames: FrameReceiver,
    shared: Arc<Shared>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            frame = frames.recv() => match frame {
                Some(frame) => {
                    let effects = shared.queue().push(frame, Instant::now());
                    shared.handle(effects);
                    shared.frame_ready.notify_one();
                }
                None => {
                    let mut state = shared.stats();
                    state.stats.capture_ended = true;
                    state.stats.last_error = Some("capture source ended".to_owned());
                    break;
                }
            },
        }
    }
    shared.queue().close();
    shared.frame_ready.notify_one();
}

async fn send_frames(conn: MediaConnection, shared: Arc<Shared>) {
    let mut fps = shared.configured_fps.load(Ordering::Relaxed);
    let mut timebase = Timebase::new(fps);
    loop {
        let next = {
            let mut queue = shared.queue();
            match queue.pop() {
                Some(frame) => Some(frame),
                None if queue.is_closed() => return,
                None => None,
            }
        };
        let Some(frame) = next else {
            shared.frame_ready.notified().await;
            continue;
        };
        match frame {
            MediaFrame::Video(video) => {
                let current = shared.configured_fps.load(Ordering::Relaxed);
                if current != fps {
                    fps = current;
                    timebase.set_fps(fps);
                }
                let pts = timebase.map(Track::Video, video.pts_us);
                match conn.send_video_frame_at(&video.data, pts).await {
                    Ok(()) => shared.stats().video_sent(video.data.len(), Instant::now()),
                    Err(e) => {
                        let effects = shared.queue().video_lost(Instant::now());
                        shared.stats().stats.video_frames_dropped += 1;
                        shared.error(e);
                        shared.handle(effects);
                    }
                }
            }
            MediaFrame::Audio(audio) => {
                let pts = timebase.map(Track::Audio, audio.pts_us);
                match conn.send_audio_frame_at(&audio.data, pts).await {
                    Ok(()) => {
                        let mut state = shared.stats();
                        state.stats.audio_frames_sent += 1;
                        state.stats.bytes_sent += audio.data.len() as u64;
                    }
                    Err(e) => {
                        shared.stats().stats.audio_frames_dropped += 1;
                        shared.error(e);
                    }
                }
            }
        }
    }
}

async fn watch_connections(
    [mut voice, mut stream]: ConnectionEvents,
    shared: Arc<Shared>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let (name, event) = tokio::select! {
            biased;
            _ = shutdown.changed() => return,
            event = voice.recv() => ("voice", event),
            event = stream.recv() => ("stream", event),
        };
        let mut state = shared.stats();
        match event {
            Some(VoiceEvent::Resumed) => state.stats.resumes += 1,
            Some(VoiceEvent::Closed { code }) => {
                state.stats.connection_closed = true;
                state.stats.last_error = Some(format!("{name} connection closed (code {code:?})"));
            }
            Some(_) => {}
            None => {
                state.stats.connection_closed = true;
                state.stats.last_error = Some(format!("{name} connection ended"));
                return;
            }
        }
    }
}
