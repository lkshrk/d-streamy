use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};
use tracing::{debug, info, warn};

use crate::{
    AudioFrame, CaptureConfig, CaptureSource, CaptureTarget, EncodedVideoFrame,
    FRAME_CHANNEL_CAPACITY, FrameReceiver, MediaError, MediaFrame, OPUS_FRAME_DURATION_US,
    TargetKind, VideoCodec,
};

pub const EXTERNAL_TARGET_ID: &str = "external";
/// RFC 6716 §3.1: TOC config 31 (CELT FB 20 ms), mono, code 0; Discord `voice-connections.mdx:502-504`.
pub const OPUS_SILENCE_FRAME: [u8; 3] = [0xF8, 0xFF, 0xFE];
/// Discord `voice-connections.mdx:502-504`: five silence frames before a break in transmission.
pub const DOCUMENTED_SILENCE_FRAMES: u32 = 5;
const SILENCE_SPACING: Duration = Duration::from_micros(OPUS_FRAME_DURATION_US);
/// One RTP tick rounded up, as in the pump's `Timebase`: 90 kHz video, 48 kHz audio.
const TICK_US: [i64; 2] = [12, 21];
const VIDEO: usize = 0;
const AUDIO: usize = 1;
const CHANNEL_CAPACITY: usize = 1;
const QUEUE_CAPACITY: usize = FRAME_CHANNEL_CAPACITY - CHANNEL_CAPACITY;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IdleMode {
    #[default]
    Documented,
    None,
    Continuous,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalOptions {
    pub grace: Duration,
    pub stall: Duration,
    pub idle: IdleMode,
    pub fps: u32,
}

impl Default for ExternalOptions {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(30),
            stall: Duration::from_secs(2),
            idle: IdleMode::Documented,
            fps: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProducerInfo {
    pub codec: VideoCodec,
    pub fps: u32,
    pub width: u32,
    pub height: u32,
}

/// A frame as a producer sends it; pts are signed because encoders may start below zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProducerFrame {
    Video {
        data: Vec<u8>,
        pts_us: i64,
        keyframe: bool,
    },
    Audio {
        data: Vec<u8>,
        pts_us: i64,
    },
}

impl From<MediaFrame> for ProducerFrame {
    fn from(frame: MediaFrame) -> Self {
        let signed = |pts: u64| i64::try_from(pts).unwrap_or(i64::MAX);
        match frame {
            MediaFrame::Video(v) => Self::Video {
                data: v.data,
                pts_us: signed(v.pts_us),
                keyframe: v.keyframe,
            },
            MediaFrame::Audio(a) => Self::Audio {
                data: a.data,
                pts_us: signed(a.pts_us),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttachError {
    #[error("no session holds the external source")]
    NoSession,
    #[error("another producer is attached")]
    Busy,
    #[error("incompatible producer: {0}")]
    Incompatible(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    #[error("the attachment is closed")]
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseReason {
    Ended,
    Failed(String),
    Dropped,
}

impl fmt::Display for CloseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ended => f.write_str("ended"),
            Self::Failed(message) => write!(f, "failed: {message}"),
            Self::Dropped => f.write_str("dropped"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptReason {
    Detached,
    Stalled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceState {
    Detached,
    Producing {
        attachment: u64,
    },
    /// `since` is the detach, or for a stall the last frame.
    Interrupted {
        since: Instant,
        reason: InterruptReason,
    },
}

/// Frames sent but not delivered: `skipped` before `start` or ahead of a keyframe, `dropped_*` to a full queue.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AttachmentStats {
    pub skipped: u64,
    pub dropped_video: u64,
    pub dropped_audio: u64,
}

#[derive(Default)]
struct Counters {
    skipped: AtomicU64,
    dropped_video: AtomicU64,
    dropped_audio: AtomicU64,
}

impl Counters {
    fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    fn snapshot(&self) -> AttachmentStats {
        AttachmentStats {
            skipped: self.skipped.load(Ordering::Relaxed),
            dropped_video: self.dropped_video.load(Ordering::Relaxed),
            dropped_audio: self.dropped_audio.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    Open,
    Resync,
    Overflow,
}

struct Current {
    id: u64,
    info: ProducerInfo,
    keyframes: watch::Sender<u64>,
    gate: Gate,
    offset: Option<i64>,
    active_at: Instant,
    stalled: bool,
}

impl Current {
    fn request_keyframe(&self) {
        self.keyframes.send_modify(|n| *n += 1);
    }
}

struct Idle {
    next: Instant,
    remaining: Option<u32>,
}

/// One outgoing timeline for both tracks, continued across attachments on wall-clock time.
#[derive(Default)]
struct Timeline {
    last_out: [Option<i64>; 2],
    last_media: [Option<(i64, Instant)>; 2],
}

impl Timeline {
    fn has_media(&self) -> bool {
        self.last_media.iter().any(Option::is_some)
    }

    /// The larger last producer pts plus the time since the latest producer frame, as `Timebase::continuing`.
    fn base_at(&self, now: Instant) -> i64 {
        let media = self.last_media.iter().flatten();
        let Some(pts) = media.clone().map(|&(pts, _)| pts).max() else {
            return 0;
        };
        let at = media.map(|&(_, at)| at).max().unwrap_or(now);
        pts.saturating_add(micros(now.saturating_duration_since(at)))
    }

    fn stamp(&mut self, track: usize, pts: i64) -> u64 {
        let floor = self.last_out[track].map_or(0, |last| last + TICK_US[track]);
        let out = pts.max(floor);
        self.last_out[track] = Some(out);
        out as u64
    }
}

#[derive(Default)]
struct Lost {
    video: u64,
    audio: u64,
    needs_keyframe: bool,
}

/// Queues `frame`; when full, video is dropped up to the next keyframe and the oldest audio makes room.
fn enqueue(queue: &mut VecDeque<MediaFrame>, frame: MediaFrame) -> Lost {
    let mut lost = Lost::default();
    if queue.len() >= QUEUE_CAPACITY {
        match &frame {
            MediaFrame::Audio(_) => {
                if let Some(i) = queue.iter().position(|f| matches!(f, MediaFrame::Audio(_))) {
                    queue.remove(i);
                    lost.audio = 1;
                } else {
                    lost.video = drop_video(queue);
                    lost.needs_keyframe = true;
                }
            }
            MediaFrame::Video(v) => {
                lost.video = drop_video(queue);
                if !v.keyframe {
                    lost.video += 1;
                    lost.needs_keyframe = true;
                    return lost;
                }
                if queue.len() >= QUEUE_CAPACITY {
                    queue.pop_front();
                    lost.audio = 1;
                }
            }
        }
    }
    queue.push_back(frame);
    lost
}

fn drop_video(queue: &mut VecDeque<MediaFrame>) -> u64 {
    let before = queue.len();
    queue.retain(|f| matches!(f, MediaFrame::Audio(_)));
    (before - queue.len()) as u64
}

fn micros(d: Duration) -> i64 {
    i64::try_from(d.as_micros()).unwrap_or(i64::MAX)
}

#[derive(Default)]
struct State {
    gone: bool,
    expired: bool,
    first_start: Option<Instant>,
    running_since: Option<Instant>,
    config: Option<CaptureConfig>,
    current: Option<Current>,
    next_id: u64,
    latched_keyframe: bool,
    detached_at: Option<Instant>,
    idle: Option<Idle>,
    queue: VecDeque<MediaFrame>,
    timeline: Timeline,
    logged_ignored: bool,
}

impl State {
    /// Since when no media has arrived while the grace period runs; `None` while producing.
    fn absent_since(&self, first_start: Instant) -> Option<Instant> {
        match &self.current {
            Some(c) if c.stalled => Some(c.active_at.max(first_start)),
            Some(_) => None,
            None => Some(self.detached_at.map_or(first_start, |t| t.max(first_start))),
        }
    }

    fn deadline(&self, options: &ExternalOptions) -> Option<Instant> {
        self.running_since?;
        let first_start = self.first_start?;
        let stall = self
            .current
            .as_ref()
            .filter(|c| !c.stalled)
            .map(|c| c.active_at.max(first_start) + options.stall);
        let expiry = self.absent_since(first_start).map(|t| t + options.grace);
        let idle = self.idle.as_ref().map(|i| i.next);
        [stall, expiry, idle].into_iter().flatten().min()
    }

    /// Applies every timer due at `now`; false once the grace period has run out.
    fn advance(&mut self, inner: &Inner, now: Instant) -> bool {
        if self.expired || self.gone {
            return false;
        }
        let (Some(_), Some(first_start)) = (self.running_since, self.first_start) else {
            return true;
        };
        if let Some(cur) = self.current.as_mut()
            && !cur.stalled
        {
            let last = cur.active_at.max(first_start);
            if now >= last + inner.options.stall {
                cur.stalled = true;
                warn!(attachment = cur.id, "producer stalled");
                inner.set_status(SourceState::Interrupted {
                    since: last,
                    reason: InterruptReason::Stalled,
                });
            }
        }
        if self
            .absent_since(first_start)
            .is_some_and(|since| now >= since + inner.options.grace)
        {
            if let Some(cur) = self.current.take() {
                warn!(
                    attachment = cur.id,
                    "producer stalled past the grace period; closing it"
                );
            }
            info!("no producer within the grace period; ending the capture");
            self.expired = true;
            self.idle = None;
            self.queue.clear();
            inner.set_status(SourceState::Detached);
            inner.wake.notify_one();
            return false;
        }
        while let Some(idle) = self.idle.as_mut()
            && idle.next <= now
        {
            let at = idle.next;
            idle.next += SILENCE_SPACING;
            if let Some(remaining) = idle.remaining.as_mut() {
                *remaining -= 1;
                if *remaining == 0 {
                    self.idle = None;
                }
            }
            let pts = self.timeline.base_at(at);
            let pts_us = self.timeline.stamp(AUDIO, pts);
            let silence = MediaFrame::Audio(AudioFrame {
                data: OPUS_SILENCE_FRAME.to_vec(),
                pts_us,
            });
            enqueue(&mut self.queue, silence);
        }
        true
    }

    fn detach(&mut self, inner: &Inner, id: u64, reason: &CloseReason) {
        let now = Instant::now();
        self.advance(inner, now);
        if self.current.as_ref().is_none_or(|c| c.id != id) {
            return;
        }
        let Some(cur) = self.current.take() else {
            return;
        };
        let since = if cur.stalled { cur.active_at } else { now };
        self.detached_at = Some(since);
        self.idle = match inner.options.idle {
            _ if !self.timeline.has_media() => None,
            IdleMode::None => None,
            IdleMode::Documented => Some(Idle {
                next: now + SILENCE_SPACING,
                remaining: Some(DOCUMENTED_SILENCE_FRAMES),
            }),
            IdleMode::Continuous => Some(Idle {
                next: now + SILENCE_SPACING,
                remaining: None,
            }),
        };
        info!(attachment = id, %reason, "producer detached");
        inner.set_status(SourceState::Interrupted {
            since,
            reason: InterruptReason::Detached,
        });
        inner.wake.notify_one();
    }
}

struct Inner {
    options: ExternalOptions,
    state: Mutex<State>,
    wake: Notify,
    status: watch::Sender<SourceState>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set_status(&self, status: SourceState) {
        self.status.send_if_modified(|current| {
            let changed = *current != status;
            *current = status;
            changed
        });
    }
}

fn check_compatible(
    info: &ProducerInfo,
    config: Option<&CaptureConfig>,
    fps: u32,
) -> Result<(), String> {
    let fps = config.map_or(fps, |c| c.fps);
    if info.fps != fps {
        return Err(format!("{} fps, the session runs at {fps}", info.fps));
    }
    let Some(config) = config else {
        return Ok(());
    };
    if info.codec != config.codec {
        return Err(format!(
            "{:?}, the session streams {:?}",
            info.codec, config.codec
        ));
    }
    if (info.width, info.height) != (config.width, config.height) {
        return Err(format!(
            "{}x{}, the session streams {}x{}",
            info.width, info.height, config.width, config.height
        ));
    }
    Ok(())
}

/// A capture source fed by one producer attachment at a time; its receiver outlives producer swaps.
pub struct ExternalSource {
    inner: Arc<Inner>,
    status: watch::Receiver<SourceState>,
    task: Option<JoinHandle<()>>,
}

impl ExternalSource {
    pub fn new(options: ExternalOptions) -> Self {
        let (status_tx, status) = watch::channel(SourceState::Detached);
        let inner = Arc::new(Inner {
            options,
            state: Mutex::new(State::default()),
            wake: Notify::new(),
            status: status_tx,
        });
        Self {
            inner,
            status,
            task: None,
        }
    }

    pub fn handle(&self) -> ExternalHandle {
        ExternalHandle {
            inner: Arc::downgrade(&self.inner),
            status: self.status.clone(),
        }
    }

    #[cfg(test)]
    fn queued(&self) -> usize {
        self.inner.lock().queue.len()
    }
}

impl Drop for ExternalSource {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        let mut state = self.inner.lock();
        state.gone = true;
        state.current = None;
        state.idle = None;
        state.queue.clear();
        self.inner.set_status(SourceState::Detached);
    }
}

#[async_trait]
impl CaptureSource for ExternalSource {
    async fn list_targets(&self) -> Result<Vec<CaptureTarget>, MediaError> {
        let state = self.inner.lock();
        let (width, height) = state
            .config
            .as_ref()
            .map_or((0, 0), |c| (c.width, c.height));
        Ok(vec![CaptureTarget {
            id: EXTERNAL_TARGET_ID.to_owned(),
            kind: TargetKind::Display,
            title: "External producer".to_owned(),
            app: None,
            width,
            height,
        }])
    }

    async fn start(&mut self, config: CaptureConfig) -> Result<FrameReceiver, MediaError> {
        if self.task.is_some() {
            return Err(MediaError::AlreadyRunning);
        }
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        {
            let mut state = self.inner.lock();
            let ignored = config.crop.is_some()
                || config.audio_gain != 1.0
                || config
                    .target_id
                    .as_deref()
                    .is_some_and(|t| t != EXTERNAL_TARGET_ID);
            if ignored && !state.logged_ignored {
                state.logged_ignored = true;
                debug!(
                    target_id = ?config.target_id,
                    crop = ?config.crop,
                    audio_gain = config.audio_gain,
                    "crop, audio gain and target are ignored for an external source"
                );
            }
            let incompatible = state.current.as_ref().and_then(|c| {
                check_compatible(&c.info, Some(&config), self.inner.options.fps)
                    .err()
                    .map(|e| (c.id, e))
            });
            if let Some((id, e)) = incompatible {
                warn!(attachment = id, %e, "closing an incompatible producer");
                state.detach(&self.inner, id, &CloseReason::Failed(e));
            }
            let now = Instant::now();
            state.config = Some(config);
            state.first_start.get_or_insert(now);
            state.running_since = Some(now);
            if let Some(idle) = state.idle.as_mut() {
                idle.next = idle.next.max(now);
            }
            if let Some(cur) = state.current.as_mut() {
                cur.gate = Gate::Resync;
                cur.request_keyframe();
            }
        }
        self.task = Some(tokio::spawn(drive(self.inner.clone(), tx)));
        Ok(rx)
    }

    async fn stop(&mut self) -> Result<(), MediaError> {
        let Some(task) = self.task.take() else {
            return Ok(());
        };
        task.abort();
        let _ = task.await;
        let mut state = self.inner.lock();
        state.running_since = None;
        state.queue.clear();
        Ok(())
    }

    fn request_keyframe(&self) {
        let mut state = self.inner.lock();
        match &state.current {
            Some(cur) => cur.request_keyframe(),
            None => state.latched_keyframe = true,
        }
    }
}

async fn drive(inner: Arc<Inner>, tx: mpsc::Sender<MediaFrame>) {
    loop {
        let (deadline, queued) = {
            let mut state = inner.lock();
            if !state.advance(&inner, Instant::now()) {
                return;
            }
            (state.deadline(&inner.options), !state.queue.is_empty())
        };
        tokio::select! {
            biased;
            permit = tx.reserve(), if queued => {
                let Ok(permit) = permit else { return };
                if let Some(frame) = inner.lock().queue.pop_front() {
                    permit.send(frame);
                }
            }
            () = inner.wake.notified() => {}
            () = sleep_opt(deadline) => {}
        }
    }
}

async fn sleep_opt(at: Option<Instant>) {
    match at {
        Some(at) => sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// What the IPC layer holds to attach producers to the session's external source.
#[derive(Clone)]
pub struct ExternalHandle {
    inner: Weak<Inner>,
    status: watch::Receiver<SourceState>,
}

impl ExternalHandle {
    pub fn attach(&self, info: ProducerInfo) -> Result<Attachment, AttachError> {
        let inner = self.inner.upgrade().ok_or(AttachError::NoSession)?;
        let mut state = inner.lock();
        if !state.advance(&inner, Instant::now()) {
            return Err(AttachError::NoSession);
        }
        if state.current.is_some() {
            return Err(AttachError::Busy);
        }
        check_compatible(&info, state.config.as_ref(), inner.options.fps)
            .map_err(AttachError::Incompatible)?;
        state.next_id += 1;
        let id = state.next_id;
        let (keyframes, requests) = watch::channel(0);
        let latched = std::mem::take(&mut state.latched_keyframe);
        keyframes.send_modify(|n| *n += 1 + u64::from(latched));
        let counters = Arc::new(Counters::default());
        state.current = Some(Current {
            id,
            info,
            keyframes,
            gate: Gate::Resync,
            offset: None,
            active_at: Instant::now(),
            stalled: false,
        });
        state.idle = None;
        drop(state);
        info!(attachment = id, "producer attached");
        inner.set_status(SourceState::Producing { attachment: id });
        inner.wake.notify_one();
        Ok(Attachment {
            inner: self.inner.clone(),
            id,
            requests,
            counters,
            closed: false,
        })
    }

    pub fn state(&self) -> watch::Receiver<SourceState> {
        self.status.clone()
    }
}

pub struct Attachment {
    inner: Weak<Inner>,
    id: u64,
    requests: watch::Receiver<u64>,
    counters: Arc<Counters>,
    closed: bool,
}

impl Attachment {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Never blocks; frames before `start` or ahead of the first keyframe are skipped.
    pub fn send(&self, frame: impl Into<ProducerFrame>) -> Result<(), SendError> {
        let frame = frame.into();
        let inner = self.inner.upgrade().ok_or(SendError::Closed)?;
        let mut guard = inner.lock();
        let now = Instant::now();
        guard.advance(&inner, now);
        let state = &mut *guard;
        let cur = state
            .current
            .as_mut()
            .filter(|c| c.id == self.id)
            .ok_or(SendError::Closed)?;
        cur.active_at = now;
        if cur.stalled {
            cur.stalled = false;
            cur.gate = Gate::Resync;
            cur.request_keyframe();
            info!(attachment = cur.id, "producer resumed");
            inner.set_status(SourceState::Producing { attachment: cur.id });
        }
        let counters = &self.counters;
        if state.running_since.is_none() {
            Counters::add(&counters.skipped, 1);
            return Ok(());
        }
        let (track, pts_in) = match &frame {
            ProducerFrame::Video { pts_us, .. } => (VIDEO, *pts_us),
            ProducerFrame::Audio { pts_us, .. } => (AUDIO, *pts_us),
        };
        let offset = *cur
            .offset
            .get_or_insert_with(|| state.timeline.base_at(now).saturating_sub(pts_in));
        if let ProducerFrame::Video { keyframe, .. } = &frame {
            if *keyframe {
                cur.gate = Gate::Open;
            } else if cur.gate != Gate::Open {
                let counter = match cur.gate {
                    Gate::Overflow => &counters.dropped_video,
                    _ => &counters.skipped,
                };
                Counters::add(counter, 1);
                return Ok(());
            }
        }
        let pts_us = state.timeline.stamp(track, pts_in.saturating_add(offset));
        state.timeline.last_media[track] = Some((pts_us as i64, now));
        let media = match frame {
            ProducerFrame::Video { data, keyframe, .. } => MediaFrame::Video(EncodedVideoFrame {
                codec: cur.info.codec,
                data,
                pts_us,
                keyframe,
            }),
            ProducerFrame::Audio { data, .. } => MediaFrame::Audio(AudioFrame { data, pts_us }),
        };
        let lost = enqueue(&mut state.queue, media);
        Counters::add(&counters.dropped_video, lost.video);
        Counters::add(&counters.dropped_audio, lost.audio);
        if lost.needs_keyframe {
            cur.gate = Gate::Overflow;
            cur.request_keyframe();
        }
        drop(guard);
        inner.wake.notify_one();
        Ok(())
    }

    /// Bumped on every keyframe the session asks for, and once on attach.
    pub fn keyframe_requests(&self) -> watch::Receiver<u64> {
        self.requests.clone()
    }

    pub fn stats(&self) -> AttachmentStats {
        self.counters.snapshot()
    }

    pub fn close(mut self, reason: CloseReason) {
        self.detach(&reason);
        self.closed = true;
    }

    fn detach(&self, reason: &CloseReason) {
        if let Some(inner) = self.inner.upgrade() {
            inner.lock().detach(&inner, self.id, reason);
        }
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        if !self.closed {
            self.detach(&CloseReason::Dropped);
        }
    }
}

#[cfg(test)]
mod tests;
