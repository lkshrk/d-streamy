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
use crate::transport::{MediaConnection, VideoProgress};

pub(crate) const TASK_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
/// How long `stop` waits for the frame the pacer is sending; a frame at the size cap takes about 1 s.
const PACER_SETTLE_TIMEOUT: Duration = Duration::from_millis(1500);
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
    /// The attached connection's video progress last applied to the stats.
    progress_seen: Mutex<Option<(MediaConnection, VideoProgress)>>,
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
    progress_seen: Option<(MediaConnection, VideoProgress)>,
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
        let progress_seen = resume.as_ref().and_then(|r| r.progress_seen.clone());
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
            progress_seen: Mutex::new(progress_seen),
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

    async fn settle_transport(&self) {
        let last = lock_seen(&self.shared).clone();
        if let Some(seen) = settle(last, &self.shared.stats).await {
            *lock_seen(&self.shared) = Some(seen);
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
            progress_seen: lock_seen(&shared).clone(),
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
        self.settle_transport().await;
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
        let notified = shared.frame_ready.notified();
        let (next, flush) = {
            let mut queue = shared.queue();
            let flush = queue.take_flush();
            match queue.pop() {
                Some(frame) => (Some(frame), flush),
                None if queue.is_closed() => return,
                None => (None, flush),
            }
        };
        let current = conn.borrow_and_update().clone();
        // Video popped after a loss is newer than everything in the transport, so the transport is flushed first.
        if flush && let Some(c) = &current {
            c.flush_video().await;
        }
        let Some(frame) = next else {
            if !flush {
                notified.await;
            }
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
                    Ok(()) => {}
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

/// Turns viewer PLI/FIR and DAVE coming up on the attached connection into keyframe requests, and pacer progress into sent or lost video.
async fn watch_keyframes(
    mut conn: watch::Receiver<Option<MediaConnection>>,
    shared: Arc<Shared>,
    mut shutdown: watch::Receiver<bool>,
    mut dave_seen: bool,
) {
    let mut poll = interval(DAVE_POLL);
    poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut progress = ProgressWatch::<MediaConnection>::default();
    loop {
        let current = conn.borrow_and_update().clone();
        let carried = lock_seen(&shared).clone();
        progress.attach(current.as_ref(), carried);
        if let Some(seen) = progress.seen() {
            *lock_seen(&shared) = Some(seen);
        }
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
                delta = progress.next() => {
                    if let Some(seen) = progress.seen() {
                        *lock_seen(&shared) = Some(seen);
                    }
                    let now = Instant::now();
                    if shared.stats().video_progress(delta, now) {
                        let effects = shared.queue().video_lost(now);
                        shared.frame_ready.notify_one();
                        shared.handle(effects);
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

trait Conn: Clone {
    fn same(&self, other: &Self) -> bool;
    fn video_progress(&self) -> watch::Receiver<VideoProgress>;
    async fn flush_video(&self);
}

impl Conn for MediaConnection {
    fn same(&self, other: &Self) -> bool {
        self.ptr_eq(other)
    }

    fn video_progress(&self) -> watch::Receiver<VideoProgress> {
        MediaConnection::video_progress(self)
    }

    async fn flush_video(&self) {
        MediaConnection::flush_video(self).await;
    }
}

/// Applies what the last connection's pacer did with the pump's video, after dropping what it has not started.
async fn settle<C: Conn>(
    last: Option<(C, VideoProgress)>,
    stats: &Mutex<StatsState>,
) -> Option<(C, VideoProgress)> {
    let (conn, seen) = last?;
    conn.flush_video().await;
    let mut rx = conn.video_progress();
    let _ = timeout(PACER_SETTLE_TIMEOUT, rx.wait_for(VideoProgress::settled)).await;
    let mut tracked = Tracked { rx, seen };
    let delta = tracked.delta();
    stats
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .video_progress(delta, Instant::now());
    Some((conn, tracked.seen))
}

/// One connection's cumulative video progress and the part already applied.
struct Tracked {
    rx: watch::Receiver<VideoProgress>,
    seen: VideoProgress,
}

impl Tracked {
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

/// Follows the attached connection's pacer and, until they close, the connections it replaced.
struct ProgressWatch<C> {
    current: Option<(C, Tracked)>,
    // Only receivers: holding a retired connection would keep its sender, and so this entry, alive.
    retired: Vec<Tracked>,
}

impl<C> Default for ProgressWatch<C> {
    fn default() -> Self {
        Self {
            current: None,
            retired: Vec::new(),
        }
    }
}

impl<C: Conn> ProgressWatch<C> {
    /// `carried` is where a previous pump on the same connection stopped applying progress.
    fn attach(&mut self, conn: Option<&C>, carried: Option<(C, VideoProgress)>) {
        if let (Some((current, _)), Some(c)) = (&self.current, conn)
            && current.same(c)
        {
            return;
        }
        if let Some((_, old)) = self.current.take() {
            self.retired.push(old);
        }
        self.current = conn.map(|c| {
            let mut rx = c.video_progress();
            let now = rx.borrow_and_update().clone();
            let seen = carried
                .filter(|(prev, _)| prev.same(c))
                .map_or(now, |(_, seen)| seen);
            rx.mark_changed();
            (c.clone(), Tracked { rx, seen })
        });
    }

    fn seen(&self) -> Option<(C, VideoProgress)> {
        self.current
            .as_ref()
            .map(|(c, t)| (c.clone(), t.seen.clone()))
    }

    /// The next change on any followed connection; a closed retired one is let go.
    async fn next(&mut self) -> VideoProgress {
        loop {
            if let Some(i) = self
                .retired
                .iter()
                .position(|t| t.rx.has_changed().is_err())
            {
                let delta = self.retired.remove(i).delta();
                if delta != VideoProgress::default() {
                    return delta;
                }
                continue;
            }
            if self.current.is_none() && self.retired.is_empty() {
                return std::future::pending().await;
            }
            let (index, open) = {
                let mut changes: Vec<_> = self
                    .current
                    .iter_mut()
                    .map(|(_, t)| t)
                    .chain(self.retired.iter_mut())
                    .map(|t| Box::pin(t.rx.changed()))
                    .collect();
                let (result, index, _) = futures_util::future::select_all(changes.iter_mut()).await;
                (index, result.is_ok())
            };
            let current = self.current.is_some();
            let tracked = match (current, index) {
                (true, 0) => &mut self.current.as_mut().unwrap().1,
                (true, i) => &mut self.retired[i - 1],
                (false, i) => &mut self.retired[i],
            };
            let delta = tracked.delta();
            if !open {
                if current && index == 0 {
                    self.current = None;
                } else {
                    self.retired.remove(index - usize::from(current));
                }
            }
            if open || delta != VideoProgress::default() {
                return delta;
            }
        }
    }
}

fn lock_seen(shared: &Shared) -> MutexGuard<'_, Option<(MediaConnection, VideoProgress)>> {
    shared
        .progress_seen
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

    #[derive(Clone)]
    struct FakeConn(u32, Arc<watch::Sender<VideoProgress>>);

    impl FakeConn {
        fn new(id: u32) -> Self {
            Self(id, Arc::new(watch::channel(VideoProgress::default()).0))
        }

        fn publish(&self, f: impl FnOnce(&mut VideoProgress)) {
            self.1.send_modify(f);
        }
    }

    impl Conn for FakeConn {
        fn same(&self, other: &Self) -> bool {
            self.0 == other.0
        }

        fn video_progress(&self) -> watch::Receiver<VideoProgress> {
            self.1.subscribe()
        }

        async fn flush_video(&self) {
            self.publish(|p| {
                let unstarted =
                    p.accepted_frames - p.sent_frames - p.dropped_frames - p.flushed_frames;
                p.flushed_frames += unstarted.saturating_sub(1);
            });
        }
    }

    #[tokio::test]
    async fn stop_applies_what_the_pacer_did_after_the_pump_stopped_watching() {
        let a = FakeConn::new(1);
        a.publish(|p| {
            p.accepted_frames = 10;
            p.sent_frames = 2;
        });
        let stats = Mutex::new(StatsState::default());
        let seen = VideoProgress {
            accepted_frames: 10,
            sent_frames: 2,
            ..VideoProgress::default()
        };
        let finisher = a.clone();
        tokio::spawn(async move {
            sleep_ms(30).await;
            finisher.publish(|p| p.sent_frames += 1);
        });
        let (_, seen) = settle(Some((a, seen)), &stats).await.unwrap();
        let state = stats.lock().unwrap();
        assert_eq!(state.stats.video_frames_sent, 1, "the frame in flight");
        assert_eq!(state.stats.video_frames_dropped, 7, "the unstarted ones");
        assert!(seen.settled());
    }

    async fn sleep_ms(ms: u64) {
        tokio::time::sleep(Duration::from_millis(ms)).await;
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

    async fn drain(watch: &mut ProgressWatch<FakeConn>) -> VideoProgress {
        let mut total = VideoProgress::default();
        while let Ok(d) = timeout(Duration::from_millis(50), watch.next()).await {
            total.sent_frames += d.sent_frames;
            total.sent_bytes += d.sent_bytes;
            total.dropped_frames += d.dropped_frames;
            total.last_error = d.last_error.or(total.last_error);
        }
        total
    }

    #[tokio::test]
    async fn progress_arrives_as_deltas_and_survives_a_connection_swap() {
        let (a, b) = (FakeConn::new(1), FakeConn::new(2));
        a.publish(|p| p.sent_frames = 5);
        let mut watch = ProgressWatch::default();
        watch.attach(Some(&a), None);
        assert_eq!(drain(&mut watch).await, VideoProgress::default());

        a.publish(|p| {
            p.sent_frames = 7;
            p.sent_bytes = 70;
            p.dropped_frames = 1;
            p.last_error = Some("write failed".into());
        });
        let d = drain(&mut watch).await;
        assert_eq!((d.sent_frames, d.sent_bytes, d.dropped_frames), (2, 70, 1));
        assert_eq!(d.last_error.as_deref(), Some("write failed"));

        a.publish(|p| p.dropped_frames = 2);
        watch.attach(Some(&b), None);
        assert_eq!(
            drain(&mut watch).await.dropped_frames,
            1,
            "loss not yet applied when the connection was swapped"
        );
        a.publish(|p| p.dropped_frames = 5);
        b.publish(|p| p.sent_frames = 1);
        let d = drain(&mut watch).await;
        assert_eq!(
            (d.sent_frames, d.dropped_frames),
            (1, 3),
            "loss on the old connection after the swap"
        );

        a.publish(|p| p.dropped_frames = 6);
        drop(a);
        assert_eq!(
            drain(&mut watch).await.dropped_frames,
            1,
            "last loss before close"
        );
        assert!(watch.retired.is_empty(), "a closed connection is let go");
    }

    #[tokio::test]
    async fn a_pump_resumed_on_the_same_connection_applies_what_happened_in_between() {
        let a = FakeConn::new(1);
        let mut first = ProgressWatch::default();
        first.attach(Some(&a), None);
        a.publish(|p| p.sent_frames = 3);
        assert_eq!(drain(&mut first).await.sent_frames, 3);
        let carried = first.seen();
        drop(first);

        a.publish(|p| {
            p.sent_frames = 4;
            p.dropped_frames = 1;
        });
        let mut resumed = ProgressWatch::default();
        resumed.attach(Some(&a), carried.clone());
        let d = drain(&mut resumed).await;
        assert_eq!((d.sent_frames, d.dropped_frames), (1, 1));

        let other = FakeConn::new(2);
        other.publish(|p| p.sent_frames = 9);
        let mut elsewhere = ProgressWatch::default();
        elsewhere.attach(Some(&other), carried);
        assert_eq!(drain(&mut elsewhere).await, VideoProgress::default());
    }
}
