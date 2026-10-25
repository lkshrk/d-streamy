use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use dstreamy_core::{FrameIntervalSnapshot, FrameIntervalTracker};
use dstreamy_media::{CaptureSource, FrameReceiver, MediaFrame};
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

    /// Applies what the pacer did with accepted video; true when frames were lost in transport.
    fn video_progress(&mut self, delta: VideoProgress, now: Instant) -> bool {
        self.video_sent(delta.sent_frames, delta.sent_bytes, now);
        self.stats.video_frames_dropped += delta.dropped_frames + delta.flushed_frames;
        if delta.dropped_frames > 0
            && let Some(e) = delta.last_error
        {
            self.stats.last_error = Some(e);
        }
        delta.dropped_frames > 0
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
    fn video_lost(&self, conn: &MediaConnection) {
        let effects = {
            let mut queue = self.queue();
            let flushed = conn.flush_video();
            queue.video_lost(Instant::now(), flushed.keyframe)
        };
        self.handle(effects);
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

    /// Has the progress watcher flush and settle every followed pacer, all within one bound.
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

/// A refused oversized frame says nothing about the transport; skipping to a keyframe would refuse every oversized one.
fn is_transport_loss(e: &WebRtcError) -> bool {
    !matches!(e, WebRtcError::FrameTooLarge(_))
}

async fn send_frames(
    mut conn: watch::Receiver<Option<MediaConnection>>,
    shared: Arc<Shared>,
    mut timebase: Timebase,
) {
    let mut fps = shared.configured_fps.load(Ordering::Relaxed);
    loop {
        let notified = shared.frame_ready.notified();
        let current = conn.borrow_and_update().clone();
        let (next, generation) = {
            let mut queue = shared.queue();
            let frame = queue.pop();
            if frame.is_none() && queue.is_closed() {
                return;
            }
            // Read under the queue lock, which a loss also holds: a flush after this pop drops the frame.
            (
                frame,
                current.as_ref().map(MediaConnection::video_generation),
            )
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
                    let lost = is_transport_loss(&e);
                    shared.error(e);
                    if lost {
                        shared.video_lost(&current);
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
                delta = progress.next() => {
                    *lock_carried(&shared) = progress.carried();
                    if shared.stats().video_progress(delta, Instant::now())
                        && let Some(c) = &current
                    {
                        shared.video_lost(c);
                        shared.frame_ready.notify_one();
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

    async fn next(&mut self) -> VideoProgress {
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
        self.followed[index].delta()
    }

    /// Flushes every followed pacer, waits until each settles or `bound` passes, and returns the rest.
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

    /// Applies every delta that arrives within 50 ms, as the watcher would.
    async fn apply(watch: &mut ProgressWatch<FakeGate>, total: &mut VideoProgress) {
        while let Ok(d) = timeout(Duration::from_millis(50), watch.next()).await {
            add(total, d);
        }
    }

    #[test]
    fn video_counts_as_sent_and_in_the_fps_window_only_once_written() {
        let mut state = StatsState::default();
        let now = Instant::now();
        let lost = state.video_progress(
            VideoProgress {
                sent_frames: 2,
                sent_bytes: 300,
                last_error: Some("old".into()),
                ..VideoProgress::default()
            },
            now,
        );
        assert!(!lost);
        assert_eq!(
            (state.stats.video_frames_sent, state.stats.bytes_sent),
            (2, 300)
        );
        assert_eq!(state.video_sent_at.len(), 2);
        assert_eq!(state.stats.last_error, None);
        let lost = state.video_progress(
            VideoProgress {
                dropped_frames: 1,
                last_error: Some("write failed".into()),
                ..VideoProgress::default()
            },
            now,
        );
        assert!(lost);
        assert_eq!(state.stats.video_frames_sent, 2);
        assert_eq!(state.stats.video_frames_dropped, 1);
        assert_eq!(state.video_sent_at.len(), 2);
        assert_eq!(state.stats.last_error.as_deref(), Some("write failed"));
        let lost = state.video_progress(
            VideoProgress {
                flushed_frames: 3,
                last_error: Some("old".into()),
                ..VideoProgress::default()
            },
            now,
        );
        assert!(
            !lost,
            "a flush the pump asked for is not a new transport loss"
        );
        assert_eq!(state.stats.video_frames_dropped, 4);
        assert_eq!(state.stats.last_error.as_deref(), Some("write failed"));
    }

    #[tokio::test]
    async fn dropping_a_running_pump_flushes_the_attached_pacer() {
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (conn, _ready, _task) =
            crate::transport::VoiceConnection::new(None, "1".into(), "2".into(), events)
                .unwrap()
                .start();
        let options = StreamOptions::new(dstreamy_media::CaptureConfig::default());
        let source = Box::new(dstreamy_media::TestPatternSource::unpaced());
        let pump = MediaPump::start(source, &options, Some(conn.clone()))
            .await
            .ok()
            .unwrap();
        let before = conn.video_generation();
        drop(pump);
        assert!(conn.video_generation() > before);
    }

    #[test]
    fn an_oversized_frame_is_dropped_without_skipping_to_a_keyframe() {
        assert!(!is_transport_loss(&WebRtcError::FrameTooLarge(4_000_000)));
        assert!(is_transport_loss(&WebRtcError::PacerQueueFull));
        assert!(is_transport_loss(&WebRtcError::PacerStopped));
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
        let late = a.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            late.send(1);
        });
        let total = watch.settle_all(Duration::from_secs(1)).await;
        assert_eq!((total.sent_frames, total.flushed_frames), (1, 5));
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
