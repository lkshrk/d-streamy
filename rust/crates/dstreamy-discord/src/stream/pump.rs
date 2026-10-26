use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use dstreamy_core::{FrameIntervalSnapshot, FrameIntervalTracker};
use dstreamy_media::{CaptureSource, EncodedVideoFrame, FrameReceiver, MediaFrame};
use tokio::sync::{Notify, RwLock, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval, timeout};

use super::queue::{Effects, FrameQueue};
use super::timebase::{Timebase, Track};
use super::{StreamError, StreamOptions, StreamStats};
use crate::transport::{MediaConnection, VideoGate, VideoProgress, WebRtcError};

pub(crate) const TASK_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
/// How long `stop` waits for the frame the pacer is sending; a frame at the size cap takes about 1 s.
const PACER_SETTLE_TIMEOUT: Duration = Duration::from_millis(1500);
type Carried = Vec<(Arc<VideoGate>, VideoProgress)>;
const FPS_WINDOW: Duration = Duration::from_secs(1);
const DAVE_POLL: Duration = Duration::from_millis(100);
/// A receiver may still lose the first keyframe after DAVE comes up (it demuxes tracks), so ask again.
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

    // Frames written between two watch reads share one time; the fps window and intervals blur only then.
    fn video_sent(&mut self, frames: u64, bytes: u64, now: Instant) {
        self.stats.video_frames_sent += frames;
        self.stats.bytes_sent += bytes;
        for _ in 0..frames {
            self.video_sent_at.push_back(now);
            self.intervals.record(now);
        }
        self.prune(now);
    }

    fn video_progress(&mut self, delta: VideoProgress, now: Instant) {
        self.video_sent(delta.sent_frames, delta.sent_bytes, now);
        self.stats.video_frames_dropped += delta.dropped_frames + delta.flushed_frames;
        if delta.dropped_frames > 0
            && let Some(e) = delta.last_error
        {
            self.stats.last_error = Some(e);
        }
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
    /// Every followed connection's progress as last applied, for a pump restarted on one of them.
    carried: Mutex<Carried>,
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

    /// Drops video up to the next keyframe here and in `conn`'s pacer, in one step against `send_frames`' pops.
    fn video_lost(
        &self,
        conn: &MediaConnection,
        attached: &watch::Receiver<Option<MediaConnection>>,
        keyframe_lost: bool,
        now: Instant,
    ) {
        let effects = {
            let mut queue = self.queue();
            // attach swaps under this lock and already dropped queued video for the new connection.
            if !attached.borrow().as_ref().is_some_and(|c| c.ptr_eq(conn)) {
                return;
            }
            let flushed = conn.flush_video();
            queue.video_lost(now, keyframe_lost || flushed.keyframe)
        };
        self.handle(effects);
    }

    /// A keyframe the pacer skipped as stale: valid later video is already queued, so only ask again if still waiting.
    fn stale_keyframe(&self, now: Instant) {
        if self.queue().keyframe_lost(now) {
            self.request_keyframe();
        }
    }

    /// The next frame with the connection and flush generation current at its pop; `None` once closed and empty.
    #[allow(clippy::type_complexity)]
    fn take_next(
        &self,
        conn: &mut watch::Receiver<Option<MediaConnection>>,
    ) -> Option<(Option<MediaFrame>, Option<MediaConnection>, Option<u64>)> {
        let mut queue = self.queue();
        let frame = queue.pop();
        if frame.is_none() && queue.is_closed() {
            return None;
        }
        // attach and video_lost hold the queue lock too, so these match the moment of the pop.
        let current = conn.borrow_and_update().clone();
        let generation = current.as_ref().map(MediaConnection::video_generation);
        Some((frame, current, generation))
    }

    fn send_failed(
        &self,
        conn: &MediaConnection,
        attached: &watch::Receiver<Option<MediaConnection>>,
        error: &WebRtcError,
        frame: &EncodedVideoFrame,
        now: Instant,
    ) {
        match send_loss(error, frame.keyframe) {
            SendLoss::Transport { keyframe } => self.video_lost(conn, attached, keyframe, now),
            SendLoss::Refused => {
                let effects = self.queue().video_lost(now, false);
                self.handle(effects);
            }
        }
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
    carried: Carried,
    /// DAVE already came up on this connection, so its keyframe request is not repeated.
    pub(crate) same_connection: bool,
}

/// Capture source plus the tasks that move its frames to whichever stream connection is attached.
pub(crate) struct MediaPump {
    shared: Arc<Shared>,
    conn: watch::Sender<Option<MediaConnection>>,
    shutdown: watch::Sender<bool>,
    settle: Option<oneshot::Sender<oneshot::Sender<()>>>,
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
        let carried = resume.as_ref().map_or_else(Vec::new, |r| r.carried.clone());
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
            carried: Mutex::new(carried),
        });
        let (conn, conn_rx) = watch::channel(conn);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (settle, settle_rx) = oneshot::channel();
        let tasks = vec![
            tokio::spawn(read_frames(frames, shared.clone(), shutdown_rx.clone())),
            tokio::spawn(send_frames(conn_rx.clone(), shared.clone(), timebase)),
            tokio::spawn(watch_keyframes(
                conn_rx,
                shared.clone(),
                shutdown_rx,
                settle_rx,
                dave_seen,
            )),
        ];
        Ok(Self {
            shared,
            conn,
            shutdown,
            settle: Some(settle),
            tasks,
            stopped: false,
        })
    }

    // The watcher settles, as the only task that applies progress; the reply wait bounds the whole of it.
    async fn settle_transport(&mut self) {
        let Some(settle) = self.settle.take() else {
            return;
        };
        let (done, settled) = oneshot::channel();
        if settle.send(done).is_ok() {
            let _ = timeout(PACER_SETTLE_TIMEOUT, settled).await;
        }
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
            carried: shared
                .carried
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
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
        {
            let mut queue = self.shared.queue();
            queue.close();
            queue.clear();
        }
        self.shared.frame_ready.notify_one();
        self.settle_transport().await;
        let _ = self.shutdown.send(true);
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
        if let Some(conn) = self.conn.borrow().as_ref() {
            conn.flush_video();
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

#[derive(Debug, PartialEq, Eq)]
enum SendLoss {
    /// The transport lost it: flush and skip to a keyframe; `keyframe` asks again soon instead of after the retry.
    Transport { keyframe: bool },
    /// Refused as oversized: the transport is fine, but later frames reference it, so skip to a keyframe at the normal rate.
    Refused,
}

/// Only a keyframe refused by a full pacer is lost for good; a keyframe failing because the connection is down would ask again every frame.
fn send_loss(e: &WebRtcError, keyframe: bool) -> SendLoss {
    match e {
        WebRtcError::FrameTooLarge(_) => SendLoss::Refused,
        WebRtcError::PacerQueueFull => SendLoss::Transport { keyframe },
        _ => SendLoss::Transport { keyframe: false },
    }
}

#[derive(Debug, PartialEq, Eq)]
enum LossReaction {
    /// Frames were lost on the wire: the full loss path; `keyframe` when a stale keyframe went with them.
    Lost {
        keyframe: bool,
    },
    StaleKeyframe,
}

fn loss_reaction(delta: &VideoProgress, attached: bool) -> Option<LossReaction> {
    if !attached {
        None
    } else if delta.dropped_frames > 0 {
        Some(LossReaction::Lost {
            keyframe: delta.stale_keyframes > 0,
        })
    } else if delta.stale_keyframes > 0 {
        Some(LossReaction::StaleKeyframe)
    } else {
        None
    }
}

async fn send_frames(
    mut conn: watch::Receiver<Option<MediaConnection>>,
    shared: Arc<Shared>,
    mut timebase: Timebase,
) {
    let mut fps = shared.configured_fps.load(Ordering::Relaxed);
    loop {
        let notified = shared.frame_ready.notified();
        let Some((next, current, generation)) = shared.take_next(&mut conn) else {
            return;
        };
        let Some(frame) = next else {
            notified.await;
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
                let sent = current
                    .send_video_frame_at_generation(&video.data, pts, generation.unwrap_or(0))
                    .await;
                if let Err(e) = sent {
                    shared.stats().stats.video_frames_dropped += 1;
                    shared.send_failed(&current, &conn, &e, &video, Instant::now());
                    shared.error(e);
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

/// The only task that applies pacer progress, so each connection's applied baseline lives in one place.
async fn watch_keyframes(
    mut conn: watch::Receiver<Option<MediaConnection>>,
    shared: Arc<Shared>,
    mut shutdown: watch::Receiver<bool>,
    mut settle: oneshot::Receiver<oneshot::Sender<()>>,
    mut dave_seen: bool,
) {
    let mut poll = interval(DAVE_POLL);
    poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let carried = std::mem::take(&mut *lock_carried(&shared));
    let mut progress = ProgressWatch::<Arc<VideoGate>>::resumed(carried);
    loop {
        let current = conn.borrow_and_update().clone();
        progress.attach(current.as_ref().map(MediaConnection::video_gate));
        *lock_carried(&shared) = progress.carried();
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
                done = &mut settle => {
                    if let Ok(done) = done {
                        let delta = progress.settle_all(PACER_SETTLE_TIMEOUT).await;
                        shared.stats().video_progress(delta, Instant::now());
                        *lock_carried(&shared) = progress.carried();
                        let _ = done.send(());
                    }
                    return;
                }
                changed = conn.changed() => match changed {
                    Ok(()) => break,
                    Err(_) => return,
                },
                (delta, attached) = progress.next() => {
                    *lock_carried(&shared) = progress.carried();
                    let reaction = loss_reaction(&delta, attached);
                    shared.stats().video_progress(delta, Instant::now());
                    match (reaction, &current) {
                        (Some(LossReaction::Lost { keyframe }), Some(c)) => {
                            shared.video_lost(c, &conn, keyframe, Instant::now());
                            shared.frame_ready.notify_one();
                        }
                        (Some(LossReaction::StaleKeyframe), Some(_)) => {
                            shared.stale_keyframe(Instant::now())
                        }
                        _ => {}
                    }
                }
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

trait Gate: Clone {
    fn same(&self, other: &Self) -> bool;
    fn video_progress(&self) -> watch::Receiver<VideoProgress>;
    fn flush(&self);
}

impl Gate for Arc<VideoGate> {
    fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(self, other)
    }

    fn video_progress(&self) -> watch::Receiver<VideoProgress> {
        VideoGate::video_progress(self)
    }

    fn flush(&self) {
        VideoGate::flush(self);
    }
}

struct Followed<G> {
    gate: G,
    rx: watch::Receiver<VideoProgress>,
    seen: VideoProgress,
}

impl<G> Followed<G> {
    fn delta(&mut self) -> VideoProgress {
        let now = self.rx.borrow_and_update().clone();
        let delta = VideoProgress {
            accepted_frames: now.accepted_frames - self.seen.accepted_frames,
            sent_frames: now.sent_frames - self.seen.sent_frames,
            sent_bytes: now.sent_bytes - self.seen.sent_bytes,
            dropped_frames: now.dropped_frames - self.seen.dropped_frames,
            flushed_frames: now.flushed_frames - self.seen.flushed_frames,
            stale_keyframes: now.stale_keyframes - self.seen.stale_keyframes,
            last_error: now.last_error.clone(),
        };
        self.seen = now;
        delta
    }
}

fn add(total: &mut VideoProgress, d: VideoProgress) {
    total.accepted_frames += d.accepted_frames;
    total.sent_frames += d.sent_frames;
    total.sent_bytes += d.sent_bytes;
    total.dropped_frames += d.dropped_frames;
    total.flushed_frames += d.flushed_frames;
    total.stale_keyframes += d.stale_keyframes;
    if d.dropped_frames > 0 {
        total.last_error = d.last_error.or(total.last_error.take());
    }
}

/// Every connection this pump sent video on; the attached one is first.
struct ProgressWatch<G> {
    followed: Vec<Followed<G>>,
    attached: bool,
}

impl<G: Gate> ProgressWatch<G> {
    /// Starts from where a stopped pump left each connection.
    fn resumed(carried: Vec<(G, VideoProgress)>) -> Self {
        let followed = carried
            .into_iter()
            .map(|(gate, seen)| {
                let mut rx = gate.video_progress();
                rx.mark_changed();
                Followed { gate, rx, seen }
            })
            .collect();
        Self {
            followed,
            attached: false,
        }
    }

    fn attach(&mut self, gate: Option<G>) {
        let Some(gate) = gate else {
            self.attached = false;
            return;
        };
        let known = self.followed.iter().position(|f| f.gate.same(&gate));
        let entry = match known {
            Some(i) => self.followed.remove(i),
            None => {
                let mut rx = gate.video_progress();
                let seen = rx.borrow_and_update().clone();
                Followed { gate, rx, seen }
            }
        };
        self.followed.insert(0, entry);
        self.attached = true;
    }

    fn carried(&self) -> Vec<(G, VideoProgress)> {
        self.followed
            .iter()
            .map(|f| (f.gate.clone(), f.seen.clone()))
            .collect()
    }

    /// The next change on any followed connection, and whether it is the attached one.
    async fn next(&mut self) -> (VideoProgress, bool) {
        if self.followed.is_empty() {
            return std::future::pending().await;
        }
        let index = {
            let mut changes: Vec<_> = self
                .followed
                .iter_mut()
                .map(|f| Box::pin(f.rx.changed()))
                .collect();
            futures_util::future::select_all(changes.iter_mut()).await.1
        };
        (self.followed[index].delta(), self.attached && index == 0)
    }

    async fn settle_all(&mut self, bound: Duration) -> VideoProgress {
        for f in &self.followed {
            f.gate.flush();
        }
        let deadline = tokio::time::Instant::now() + bound;
        let mut total = VideoProgress::default();
        for f in &mut self.followed {
            let _ = tokio::time::timeout_at(deadline, f.rx.wait_for(VideoProgress::settled)).await;
            add(&mut total, f.delta());
        }
        total
    }
}

fn lock_carried(shared: &Shared) -> MutexGuard<'_, Carried> {
    shared
        .carried
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A connection whose pacer the test drives by hand; `in_flight` frames survive a flush.
    #[derive(Clone)]
    struct FakeGate(u32, Arc<watch::Sender<VideoProgress>>, Arc<AtomicU64>);

    impl FakeGate {
        fn new(id: u32) -> Self {
            Self(
                id,
                Arc::new(watch::channel(VideoProgress::default()).0),
                Arc::default(),
            )
        }

        fn publish(&self, f: impl FnOnce(&mut VideoProgress)) {
            self.1.send_modify(f);
        }

        fn accept(&self, n: u64) {
            self.publish(|p| p.accepted_frames += n);
        }

        fn send(&self, n: u64) {
            self.publish(|p| {
                p.sent_frames += n;
                p.sent_bytes += 100 * n;
            });
        }
    }

    impl Gate for FakeGate {
        fn same(&self, other: &Self) -> bool {
            self.0 == other.0
        }

        fn video_progress(&self) -> watch::Receiver<VideoProgress> {
            self.1.subscribe()
        }

        fn flush(&self) {
            let in_flight = self.2.load(Ordering::Relaxed);
            self.publish(|p| {
                let unsettled =
                    p.accepted_frames - p.sent_frames - p.dropped_frames - p.flushed_frames;
                p.flushed_frames += unsettled.saturating_sub(in_flight);
            });
        }
    }

    async fn apply(watch: &mut ProgressWatch<FakeGate>, total: &mut VideoProgress) {
        while let Ok((d, _)) = timeout(Duration::from_millis(50), watch.next()).await {
            add(total, d);
        }
    }

    #[test]
    fn video_counts_as_sent_and_in_the_fps_window_only_once_written() {
        let mut state = StatsState::default();
        let now = Instant::now();
        state.video_progress(
            VideoProgress {
                sent_frames: 2,
                sent_bytes: 300,
                last_error: Some("old".into()),
                ..VideoProgress::default()
            },
            now,
        );
        assert_eq!(
            (state.stats.video_frames_sent, state.stats.bytes_sent),
            (2, 300)
        );
        assert_eq!(state.video_sent_at.len(), 2);
        assert_eq!(state.stats.last_error, None);
        state.video_progress(
            VideoProgress {
                dropped_frames: 1,
                last_error: Some("write failed".into()),
                ..VideoProgress::default()
            },
            now,
        );
        assert_eq!(state.stats.video_frames_sent, 2);
        assert_eq!(state.stats.video_frames_dropped, 1);
        assert_eq!(state.video_sent_at.len(), 2);
        assert_eq!(state.stats.last_error.as_deref(), Some("write failed"));
        state.video_progress(
            VideoProgress {
                flushed_frames: 3,
                last_error: Some("old".into()),
                ..VideoProgress::default()
            },
            now,
        );
        assert_eq!(state.stats.video_frames_dropped, 4);
        assert_eq!(state.stats.last_error.as_deref(), Some("write failed"));
    }

    fn offline_connection() -> MediaConnection {
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        crate::transport::VoiceConnection::new(None, "1".into(), "2".into(), events)
            .unwrap()
            .start()
            .0
    }

    async fn pump(conn: &MediaConnection) -> MediaPump {
        let options = StreamOptions::new(dstreamy_media::CaptureConfig::default());
        let source = Box::new(dstreamy_media::TestPatternSource::unpaced());
        MediaPump::start(source, &options, Some(conn.clone()))
            .await
            .ok()
            .unwrap()
    }

    /// A source whose frames the test pushes into the pump by hand; counts keyframe requests.
    #[derive(Default)]
    struct SilentSource {
        frames: Option<tokio::sync::mpsc::Sender<MediaFrame>>,
        requests: Arc<AtomicU64>,
    }

    #[async_trait::async_trait]
    impl CaptureSource for SilentSource {
        async fn list_targets(
            &self,
        ) -> Result<Vec<dstreamy_media::CaptureTarget>, dstreamy_media::MediaError> {
            Ok(Vec::new())
        }

        async fn start(
            &mut self,
            _: dstreamy_media::CaptureConfig,
        ) -> Result<FrameReceiver, dstreamy_media::MediaError> {
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            self.frames = Some(tx);
            Ok(rx)
        }

        async fn stop(&mut self) -> Result<(), dstreamy_media::MediaError> {
            self.frames = None;
            Ok(())
        }

        fn request_keyframe(&self) {
            self.requests.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn oversized(pts_us: u64, keyframe: bool) -> MediaFrame {
        let mut data = vec![0, 0, 0, 1, if keyframe { 0x65 } else { 0x41 }];
        data.resize(
            crate::transport::voice::webrtc::MAX_VIDEO_FRAME_BYTES + 1,
            7,
        );
        MediaFrame::Video(dstreamy_media::EncodedVideoFrame {
            codec: dstreamy_media::VideoCodec::H264,
            data,
            pts_us,
            keyframe,
        })
    }

    #[tokio::test]
    async fn oversized_keyframes_are_dropped_and_asked_for_at_most_once_per_retry() {
        let conn = offline_connection();
        let source = SilentSource::default();
        let requests = source.requests.clone();
        let options = StreamOptions::new(dstreamy_media::CaptureConfig::default());
        let pump = MediaPump::start(Box::new(source), &options, Some(conn.clone()))
            .await
            .ok()
            .unwrap();
        let started = Instant::now();
        let attached = requests.load(Ordering::Relaxed);
        let generation = conn.video_generation();
        for i in 0..4 {
            pump.shared
                .queue()
                .push(oversized(i * 33_333, true), Instant::now());
            pump.shared.frame_ready.notify_one();
            timeout(Duration::from_secs(5), async {
                while pump.stats().video_frames_dropped < i + 1 {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("the oversized frame was handled");
        }
        let stats = pump.stats();
        assert_eq!(stats.video_frames_dropped, 4);
        assert_eq!(stats.video_frames_sent, 0);
        assert!(
            stats
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("exceeds")),
            "{:?}",
            stats.last_error
        );
        let asked = requests.load(Ordering::Relaxed) - attached;
        let allowed = 1 + started.elapsed().as_secs();
        assert!(
            (1..=allowed).contains(&asked),
            "{asked} requests in {:?}: at least one, and at most one per retry",
            started.elapsed()
        );
        assert_eq!(
            conn.video_generation(),
            generation,
            "the transport is not flushed"
        );
        let _ = pump.stop().await;
    }

    #[tokio::test]
    async fn take_next_returns_the_latest_attached_connection() {
        let (old, new) = (offline_connection(), offline_connection());
        let pump = pump(&old).await;
        pump.detach();
        let (tx, mut rx) = watch::channel(Some(old.clone()));
        rx.borrow_and_update();
        {
            let mut queue = pump.shared.queue();
            queue.clear();
            queue.push(
                MediaFrame::Video(dstreamy_media::EncodedVideoFrame {
                    codec: dstreamy_media::VideoCodec::H264,
                    data: vec![0, 0, 0, 1, 0x65],
                    pts_us: 0,
                    keyframe: true,
                }),
                Instant::now(),
            );
            tx.send_replace(Some(new.clone()));
        }
        let (frame, current, _) = pump.shared.take_next(&mut rx).unwrap();
        assert!(frame.is_some());
        assert!(current.unwrap().ptr_eq(&new));
        let _ = pump.stop().await;
    }

    #[tokio::test]
    async fn dropping_a_running_pump_bumps_the_flush_generation() {
        let conn = offline_connection();
        let pump = pump(&conn).await;
        let before = conn.video_generation();
        drop(pump);
        assert!(conn.video_generation() > before);
    }

    fn idr() -> EncodedVideoFrame {
        EncodedVideoFrame {
            codec: dstreamy_media::VideoCodec::H264,
            data: vec![0, 0, 0, 1, 0x65],
            pts_us: 0,
            keyframe: true,
        }
    }

    /// A pump on an offline connection that last asked for a keyframe at `asked`, waiting for it.
    async fn waiting_pump(conn: &MediaConnection) -> (MediaPump, Instant) {
        let pump = pump(conn).await;
        let asked = Instant::now();
        pump.shared.queue().clear();
        pump.shared.queue().video_lost(asked, false);
        (pump, asked)
    }

    fn requests(pump: &MediaPump) -> u64 {
        pump.stats().keyframe_requests
    }

    #[tokio::test]
    async fn a_keyframe_refused_by_a_full_pacer_is_asked_for_again_soon() {
        let conn = offline_connection();
        let (pump, asked) = waiting_pump(&conn).await;
        let attached = pump.conn.subscribe();
        let before = requests(&pump);
        let soon = asked + super::super::queue::KEYFRAME_RETRY_SOON;
        pump.shared
            .send_failed(&conn, &attached, &WebRtcError::PacerQueueFull, &idr(), soon);
        assert_eq!(
            requests(&pump) - before,
            1,
            "not held back by the full retry"
        );
        pump.shared
            .send_failed(&conn, &attached, &WebRtcError::PacerQueueFull, &idr(), soon);
        assert_eq!(requests(&pump) - before, 1, "the floor holds");
        let _ = pump.stop().await;
    }

    #[tokio::test]
    async fn a_keyframe_failing_on_a_down_connection_asks_at_most_once_per_retry() {
        let conn = offline_connection();
        let (pump, asked) = waiting_pump(&conn).await;
        let attached = pump.conn.subscribe();
        let before = requests(&pump);
        for i in 1..30u32 {
            let at = asked + Duration::from_millis(33) * i;
            pump.shared
                .send_failed(&conn, &attached, &WebRtcError::NotConnected, &idr(), at);
        }
        assert_eq!(
            requests(&pump) - before,
            0,
            "under a second since the last request"
        );
        let at = asked + super::super::queue::KEYFRAME_RETRY;
        pump.shared
            .send_failed(&conn, &attached, &WebRtcError::NotConnected, &idr(), at);
        assert_eq!(requests(&pump) - before, 1);
        let _ = pump.stop().await;
    }

    #[tokio::test]
    async fn an_oversized_keyframe_asks_at_most_once_per_retry() {
        let conn = offline_connection();
        let (pump, asked) = waiting_pump(&conn).await;
        let attached = pump.conn.subscribe();
        let generation = conn.video_generation();
        let too_large = WebRtcError::FrameTooLarge(4_000_000);
        let before = requests(&pump);
        for i in 1..30u32 {
            let at = asked + Duration::from_millis(33) * i;
            pump.shared
                .send_failed(&conn, &attached, &too_large, &idr(), at);
        }
        assert_eq!(requests(&pump) - before, 0, "a broken encoder cannot spin");
        let at = asked + super::super::queue::KEYFRAME_RETRY;
        pump.shared
            .send_failed(&conn, &attached, &too_large, &idr(), at);
        assert_eq!(
            requests(&pump) - before,
            1,
            "the broken reference chain is repaired"
        );
        assert_eq!(
            conn.video_generation(),
            generation,
            "the transport is not flushed"
        );
        let _ = pump.stop().await;
    }

    #[tokio::test]
    async fn a_stale_keyframe_neither_flushes_nor_cuts_and_asks_only_while_waiting() {
        let conn = offline_connection();
        let (pump, asked) = waiting_pump(&conn).await;
        let generation = conn.video_generation();
        let soon = asked + super::super::queue::KEYFRAME_RETRY_SOON;
        let before = requests(&pump);
        pump.shared.stale_keyframe(soon);
        assert_eq!(requests(&pump) - before, 1);
        assert_eq!(conn.video_generation(), generation, "no second flush");
        pump.shared.queue().push(MediaFrame::Video(idr()), soon);
        pump.shared
            .stale_keyframe(soon + super::super::queue::KEYFRAME_RETRY);
        assert_eq!(requests(&pump) - before, 1, "a keyframe is queued again");
        assert_eq!(pump.shared.queue().len(), 1, "queued video is kept");
        let _ = pump.stop().await;
    }

    #[tokio::test]
    async fn a_loss_on_a_connection_no_longer_attached_is_ignored() {
        let (old, new) = (offline_connection(), offline_connection());
        let pump = pump(&old).await;
        pump.attach(new.clone());
        let attached = pump.conn.subscribe();
        let (old_generation, new_generation) = (old.video_generation(), new.video_generation());
        let before = requests(&pump);
        let later = Instant::now() + super::super::queue::KEYFRAME_RETRY;
        pump.shared.video_lost(&old, &attached, true, later);
        assert_eq!(requests(&pump), before);
        assert_eq!(old.video_generation(), old_generation);
        assert_eq!(new.video_generation(), new_generation);
        pump.shared.video_lost(&new, &attached, true, later);
        assert!(new.video_generation() > new_generation);
        let _ = pump.stop().await;
    }

    #[test]
    fn only_the_attached_connection_drives_the_loss_path() {
        let dropped = VideoProgress {
            dropped_frames: 2,
            ..VideoProgress::default()
        };
        let stale_keyframe = VideoProgress {
            flushed_frames: 1,
            stale_keyframes: 1,
            ..VideoProgress::default()
        };
        let both = VideoProgress {
            dropped_frames: 1,
            stale_keyframes: 1,
            ..VideoProgress::default()
        };
        assert_eq!(
            loss_reaction(&dropped, true),
            Some(LossReaction::Lost { keyframe: false })
        );
        assert_eq!(
            loss_reaction(&both, true),
            Some(LossReaction::Lost { keyframe: true })
        );
        assert_eq!(
            loss_reaction(&stale_keyframe, true),
            Some(LossReaction::StaleKeyframe)
        );
        assert_eq!(
            loss_reaction(&dropped, false),
            None,
            "a retired pacer's drops"
        );
        assert_eq!(loss_reaction(&stale_keyframe, false), None);
        let sent = VideoProgress {
            sent_frames: 3,
            flushed_frames: 2,
            ..VideoProgress::default()
        };
        assert_eq!(
            loss_reaction(&sent, true),
            None,
            "flushes the pump asked for"
        );
    }

    #[tokio::test]
    async fn a_retired_connections_delta_is_not_the_attached_ones() {
        let (a, b) = (FakeGate::new(1), FakeGate::new(2));
        let mut watch = ProgressWatch::resumed(Vec::new());
        watch.attach(Some(a.clone()));
        timeout(Duration::from_millis(50), watch.next()).await.ok();
        watch.attach(Some(b.clone()));
        a.publish(|p| p.dropped_frames += 1);
        let (delta, attached) = timeout(Duration::from_secs(1), watch.next()).await.unwrap();
        assert_eq!((delta.dropped_frames, attached), (1, false));
        b.publish(|p| p.dropped_frames += 1);
        let (delta, attached) = timeout(Duration::from_secs(1), watch.next()).await.unwrap();
        assert_eq!((delta.dropped_frames, attached), (1, true));
    }

    #[tokio::test]
    async fn progress_survives_a_swap_and_is_applied_once_per_connection() {
        let (a, b) = (FakeGate::new(1), FakeGate::new(2));
        a.accept(5);
        a.send(5);
        let mut watch = ProgressWatch::resumed(Vec::new());
        watch.attach(Some(a.clone()));
        let mut total = VideoProgress::default();
        apply(&mut watch, &mut total).await;
        assert_eq!(
            total.sent_frames, 0,
            "progress before the attach is not ours"
        );

        a.accept(3);
        a.send(2);
        a.publish(|p| p.dropped_frames += 1);
        watch.attach(Some(b.clone()));
        b.accept(1);
        b.send(1);
        a.accept(1);
        a.send(1);
        apply(&mut watch, &mut total).await;
        assert_eq!((total.sent_frames, total.dropped_frames), (4, 1));

        total = VideoProgress::default();
        total.sent_frames += watch
            .settle_all(Duration::from_millis(50))
            .await
            .sent_frames;
        assert_eq!(total.sent_frames, 0, "nothing applied twice at stop");
    }

    #[tokio::test]
    async fn progress_after_a_detach_counts_once_at_stop() {
        let a = FakeGate::new(1);
        let mut watch = ProgressWatch::resumed(Vec::new());
        watch.attach(Some(a.clone()));
        a.accept(6);
        a.send(2);
        let mut total = VideoProgress::default();
        apply(&mut watch, &mut total).await;
        assert_eq!(total.sent_frames, 2);
        watch.attach(None);
        a.send(3);
        apply(&mut watch, &mut total).await;
        add(
            &mut total,
            watch.settle_all(Duration::from_millis(50)).await,
        );
        assert_eq!(
            total.sent_frames, 5,
            "each frame once, before and after the detach"
        );
        assert_eq!(total.flushed_frames, 1);
    }

    #[tokio::test]
    async fn reattaching_the_same_connection_keeps_its_baseline() {
        let (a, b) = (FakeGate::new(1), FakeGate::new(2));
        let mut watch = ProgressWatch::resumed(Vec::new());
        watch.attach(Some(a.clone()));
        a.accept(2);
        a.send(2);
        let mut total = VideoProgress::default();
        apply(&mut watch, &mut total).await;
        watch.attach(Some(b));
        a.accept(1);
        a.send(1);
        watch.attach(Some(a.clone()));
        apply(&mut watch, &mut total).await;
        add(
            &mut total,
            watch.settle_all(Duration::from_millis(50)).await,
        );
        assert_eq!(total.sent_frames, 3);
    }

    #[tokio::test]
    async fn settling_waits_for_the_frame_in_flight_on_every_connection() {
        let (a, b) = (FakeGate::new(1), FakeGate::new(2));
        let mut watch = ProgressWatch::resumed(Vec::new());
        watch.attach(Some(a.clone()));
        watch.attach(Some(b.clone()));
        a.accept(4);
        b.accept(2);
        a.2.store(1, Ordering::Relaxed);
        b.2.store(1, Ordering::Relaxed);
        let (a_done, b_done) = (a.clone(), b.clone());
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            a_done.send(1);
            tokio::time::sleep(Duration::from_millis(30)).await;
            b_done.send(1);
        });
        let total = watch.settle_all(Duration::from_secs(1)).await;
        assert_eq!(
            (total.sent_frames, total.flushed_frames),
            (2, 4),
            "both frames in flight"
        );
    }

    #[tokio::test]
    async fn settling_is_bounded_when_a_frame_never_finishes() {
        let a = FakeGate::new(1);
        let mut watch = ProgressWatch::resumed(Vec::new());
        watch.attach(Some(a.clone()));
        a.accept(1);
        a.2.store(1, Ordering::Relaxed);
        timeout(
            Duration::from_millis(500),
            watch.settle_all(Duration::from_millis(100)),
        )
        .await
        .expect("settling is bounded");
    }

    #[tokio::test]
    async fn a_pump_resumed_on_a_followed_connection_applies_what_happened_in_between() {
        let (a, other) = (FakeGate::new(1), FakeGate::new(2));
        let mut first = ProgressWatch::resumed(Vec::new());
        first.attach(Some(a.clone()));
        a.accept(3);
        a.send(3);
        let mut total = VideoProgress::default();
        apply(&mut first, &mut total).await;
        let carried = first.carried();
        drop(first);

        a.accept(2);
        a.send(1);
        a.publish(|p| p.dropped_frames += 1);
        other.accept(9);
        other.send(9);
        let mut resumed = ProgressWatch::resumed(carried);
        resumed.attach(Some(a));
        let mut later = VideoProgress::default();
        apply(&mut resumed, &mut later).await;
        assert_eq!((later.sent_frames, later.dropped_frames), (1, 1));
        resumed.attach(Some(other));
        apply(&mut resumed, &mut later).await;
        assert_eq!(
            later.sent_frames, 1,
            "an unfollowed connection starts from now"
        );
    }
}
