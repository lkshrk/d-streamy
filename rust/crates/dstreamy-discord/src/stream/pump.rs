use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use dstreamy_core::{FrameIntervalSnapshot, FrameIntervalTracker};
use dstreamy_media::{CaptureSource, FrameReceiver, MediaFrame};
use tokio::sync::{Notify, RwLock, watch};
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval, timeout};

use super::queue::{Effects, FrameQueue};
use super::timebase::{Timebase, Track};
use super::{StreamError, StreamOptions, StreamStats};
use crate::transport::MediaConnection;

pub(crate) const TASK_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
const FPS_WINDOW: Duration = Duration::from_secs(1);
const DAVE_POLL: Duration = Duration::from_millis(100);
/// davey can reject the first keyframe after DAVE comes up (audio overtakes video), so ask again.
const DAVE_KEYFRAME_DELAY: Duration = Duration::from_millis(500);
/// Viewer PLIs arrive per viewer; one keyframe answers them all.
const VIEWER_KEYFRAME_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Default)]
struct StatsState {
    stats: StreamStats,
    video_sent_at: VecDeque<Instant>,
    intervals: FrameIntervalTracker,
    last_frame_at: Option<Instant>,
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
        self.intervals.record(now);
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
    /// Latest mapped pts per track (video, audio) in µs plus one, 0 before the first frame.
    last_pts: [AtomicU64; 2],
    source: RwLock<Option<Box<dyn CaptureSource>>>,
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
        if let Ok(source) = self.source.try_read()
            && let Some(source) = source.as_ref()
        {
            source.request_keyframe();
            self.stats().stats.keyframe_requests += 1;
        }
    }

    fn note_pts(&self, track: Track, pts: Duration) {
        let us = u64::try_from(pts.as_micros()).unwrap_or(u64::MAX - 1);
        self.last_pts[track as usize].fetch_max(us + 1, Ordering::Relaxed);
        self.stats().last_frame_at = Some(Instant::now());
    }

    fn error(&self, error: impl ToString) {
        self.stats().stats.last_error = Some(error.to_string());
    }

    // `attach` asks for a keyframe, so a detached pump only counts what it drops.
    fn discard(&self, frame: MediaFrame) {
        let mut state = self.stats();
        match frame {
            MediaFrame::Video(_) => state.stats.video_frames_dropped += 1,
            MediaFrame::Audio(_) => state.stats.audio_frames_dropped += 1,
        }
    }
}

pub(crate) struct StatsUpdater(Arc<Shared>);

impl StatsUpdater {
    pub(crate) fn update(&self, f: impl FnOnce(&mut StreamStats)) {
        f(&mut self.0.stats().stats);
    }
}

/// Where a restarted pump picks up on the same connection: after the old timeline, with its counters.
pub(crate) struct Resume {
    last_pts_us: [Option<u64>; 2],
    last_frame_at: Option<Instant>,
    stats: StreamStats,
    video_sent_at: VecDeque<Instant>,
    /// DAVE already came up on this connection, so its keyframe request is not repeated.
    pub(crate) same_connection: bool,
}

/// Capture source plus the tasks that move its frames to whichever stream connection is attached.
pub(crate) struct MediaPump {
    shared: Arc<Shared>,
    conn: watch::Sender<Option<MediaConnection>>,
    shutdown: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
    stopped: bool,
}

impl MediaPump {
    /// Starts the source; on failure the source is handed back for a retry.
    pub(crate) async fn start(
        source: Box<dyn CaptureSource>,
        options: &StreamOptions,
        conn: Option<MediaConnection>,
    ) -> Result<Self, (Box<dyn CaptureSource>, StreamError)> {
        Self::start_resumed(source, options, conn, None).await
    }

    pub(crate) async fn start_resumed(
        mut source: Box<dyn CaptureSource>,
        options: &StreamOptions,
        conn: Option<MediaConnection>,
        resume: Option<Resume>,
    ) -> Result<Self, (Box<dyn CaptureSource>, StreamError)> {
        let frames = match source.start(options.capture.clone()).await {
            Ok(frames) => frames,
            Err(e) => return Err((source, e.into())),
        };
        let fps = options.capture.fps;
        let (timebase, stats, dave_seen) = match resume {
            Some(resume) => {
                let timebase = if resume.last_pts_us.iter().any(Option::is_some) {
                    let gap = resume.last_frame_at.map_or(0, |t| t.elapsed().as_micros());
                    Timebase::continuing(
                        fps,
                        resume.last_pts_us,
                        u64::try_from(gap).unwrap_or(u64::MAX),
                    )
                } else {
                    Timebase::new(fps)
                };
                let mut stats = resume.stats;
                stats.capture_ended = false;
                stats.last_error = None;
                let state = StatsState {
                    stats,
                    video_sent_at: resume.video_sent_at,
                    last_frame_at: resume.last_frame_at,
                    ..StatsState::default()
                };
                (timebase, state, resume.same_connection)
            }
            None => (Timebase::new(fps), StatsState::default(), false),
        };
        let shared = Arc::new(Shared {
            queue: Mutex::new(FrameQueue::new(
                options.max_queued_video,
                options.max_queued_audio,
            )),
            frame_ready: Notify::new(),
            stats: Mutex::new(stats),
            configured_fps: AtomicU32::new(options.capture.fps),
            last_pts: [AtomicU64::new(0), AtomicU64::new(0)],
            source: RwLock::new(Some(source)),
        });
        let (conn, conn_rx) = watch::channel(conn);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let tasks = vec![
            tokio::spawn(read_frames(frames, shared.clone(), shutdown_rx.clone())),
            tokio::spawn(send_frames(conn_rx.clone(), shared.clone(), timebase)),
            tokio::spawn(watch_keyframes(
                conn_rx,
                shared.clone(),
                shutdown_rx,
                dave_seen,
            )),
        ];
        Ok(Self {
            shared,
            conn,
            shutdown,
            tasks,
            stopped: false,
        })
    }

    pub(crate) fn stats(&self) -> StreamStats {
        let mut state = self.shared.stats();
        state.prune(Instant::now());
        let mut stats = state.stats.clone();
        stats.fps = state.video_sent_at.len() as f64 / FPS_WINDOW.as_secs_f64();
        stats
    }

    pub(crate) fn update_stats(&self, f: impl FnOnce(&mut StreamStats)) {
        f(&mut self.shared.stats().stats);
    }

    pub(crate) fn stats_updater(&self) -> StatsUpdater {
        StatsUpdater(self.shared.clone())
    }

    /// Send times of video frames since the last call.
    pub(crate) fn take_frame_intervals(&self) -> FrameIntervalSnapshot {
        self.shared.stats().intervals.drain()
    }

    pub(crate) fn capture_ended(&self) -> bool {
        self.shared.stats().stats.capture_ended
    }

    pub(crate) fn set_fps(&self, fps: u32) {
        self.shared.configured_fps.store(fps, Ordering::Relaxed);
    }

    pub(crate) fn request_keyframe(&self) {
        self.shared.request_keyframe();
    }

    /// Switches to a new stream connection; video resumes at the next keyframe, which is requested now.
    pub(crate) fn attach(&self, conn: MediaConnection) {
        let effects = {
            // Swapped under the queue lock so no frame popped before the swap reaches the new connection.
            let mut queue = self.shared.queue();
            let effects = queue.require_keyframe(Instant::now());
            self.conn.send_replace(Some(conn));
            effects
        };
        self.shared.handle(effects);
    }

    /// Frames are dropped until the next `attach`.
    pub(crate) fn detach(&self) {
        self.conn.send_replace(None);
    }

    /// Like `stop`, plus what a pump restarted on the same connection needs to continue.
    pub(crate) async fn stop_resumable(
        self,
    ) -> (
        Option<Box<dyn CaptureSource>>,
        Resume,
        Result<StreamStats, StreamError>,
    ) {
        let shared = self.shared.clone();
        let (source, result) = self.stop().await;
        let state = shared.stats();
        let resume = Resume {
            last_pts_us: shared
                .last_pts
                .each_ref()
                .map(|p| p.load(Ordering::Relaxed).checked_sub(1)),
            last_frame_at: state.last_frame_at,
            stats: state.stats.clone(),
            video_sent_at: state.video_sent_at.clone(),
            same_connection: false,
        };
        drop(state);
        (source, resume, result)
    }

    pub(crate) async fn stop(
        mut self,
    ) -> (
        Option<Box<dyn CaptureSource>>,
        Result<StreamStats, StreamError>,
    ) {
        self.stopped = true;
        self.detach();
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
        let source = self.shared.source.write().await.take();
        let Some(mut source) = source else {
            return (None, Ok(self.stats()));
        };
        let result = match timeout(TASK_JOIN_TIMEOUT, source.stop()).await {
            Ok(Ok(())) => Ok(self.stats()),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => Err(StreamError::Timeout("stopping the capture source")),
        };
        (Some(source), result)
    }
}

impl Drop for MediaPump {
    fn drop(&mut self) {
        if self.stopped {
            return;
        }
        let _ = self.shutdown.send(true);
        for task in &self.tasks {
            task.abort();
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let shared = self.shared.clone();
            runtime.spawn(async move {
                if let Some(source) = shared.source.write().await.as_mut() {
                    let _ = source.stop().await;
                }
            });
        }
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

async fn send_frames(
    mut conn: watch::Receiver<Option<MediaConnection>>,
    shared: Arc<Shared>,
    mut timebase: Timebase,
) {
    let mut fps = shared.configured_fps.load(Ordering::Relaxed);
    loop {
        let next = {
            let mut queue = shared.queue();
            match queue.pop() {
                Some(frame) => Some((frame, conn.borrow_and_update().clone())),
                None if queue.is_closed() => return,
                None => None,
            }
        };
        let Some((frame, current)) = next else {
            shared.frame_ready.notified().await;
            continue;
        };
        let Some(current) = current else {
            shared.discard(frame);
            continue;
        };
        match frame {
            MediaFrame::Video(video) => {
                let configured = shared.configured_fps.load(Ordering::Relaxed);
                if configured != fps {
                    fps = configured;
                    timebase.set_fps(fps);
                }
                let pts = timebase.map(Track::Video, video.pts_us);
                shared.note_pts(Track::Video, pts);
                match current.send_video_frame_at(&video.data, pts).await {
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
                shared.note_pts(Track::Audio, pts);
                match current.send_audio_frame_at(&audio.data, pts).await {
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

/// Turns viewer PLI/FIR and DAVE coming up on the attached connection into keyframe requests.
async fn watch_keyframes(
    mut conn: watch::Receiver<Option<MediaConnection>>,
    shared: Arc<Shared>,
    mut shutdown: watch::Receiver<bool>,
    mut dave_seen: bool,
) {
    let mut poll = interval(DAVE_POLL);
    poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        let current = conn.borrow_and_update().clone();
        let mut viewer_requests = current.as_ref().map(|c| {
            let mut requests = c.keyframe_requests();
            requests.borrow_and_update();
            requests
        });
        let mut dave_ready = std::mem::take(&mut dave_seen)
            && current.as_ref().is_some_and(MediaConnection::dave_ready);
        let mut due: Option<Instant> = None;
        let mut last_viewer_request: Option<Instant> = None;
        loop {
            tokio::select! {
                biased;
                _ = shutdown.changed() => return,
                changed = conn.changed() => match changed {
                    Ok(()) => break,
                    Err(_) => return,
                },
                () = next_request(&mut viewer_requests) => {
                    shared.stats().stats.viewer_keyframe_requests += 1;
                    let now = Instant::now();
                    if last_viewer_request.is_none_or(|t| now.duration_since(t) >= VIEWER_KEYFRAME_INTERVAL) {
                        last_viewer_request = Some(now);
                        shared.request_keyframe();
                    }
                }
                _ = poll.tick() => {
                    let now = Instant::now();
                    let ready = current.as_ref().is_some_and(MediaConnection::dave_ready);
                    if ready && !dave_ready {
                        due = Some(now + DAVE_KEYFRAME_DELAY);
                    }
                    dave_ready = ready;
                    if due.is_some_and(|at| now >= at) {
                        due = None;
                        shared.stats().stats.dave_keyframe_requests += 1;
                        shared.request_keyframe();
                    }
                }
            }
        }
    }
}

async fn next_request(requests: &mut Option<watch::Receiver<u64>>) {
    if let Some(rx) = requests.as_mut()
        && rx.changed().await.is_ok()
    {
        return;
    }
    *requests = None;
    std::future::pending().await
}
