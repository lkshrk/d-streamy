use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use async_trait::async_trait;
use dstreamy_core::{Fps, Resolution};
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
/// RFC 6716 §3.1 and §3.2.2: TOC config 31 (CELT FB 20 ms), mono, code 0, one frame.
pub const OPUS_SILENCE_FRAME: [u8; 3] = [0xF8, 0xFF, 0xFE];
/// Discord `voice-connections.mdx:502-504`: five silence frames before a break in transmission.
pub const DOCUMENTED_SILENCE_FRAMES: u32 = 5;
const SILENCE_SPACING: Duration = Duration::from_micros(OPUS_FRAME_DURATION_US);
/// One RTP tick rounded up, as in the pump's `Timebase`: 90 kHz video, 48 kHz audio.
const TICK_US: [i64; 2] = [12, 21];
/// A producer that sends only one track still starts within this.
const FIRST_FRAMES_WAIT: Duration = Duration::from_millis(500);
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
}

impl Default for ExternalOptions {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(30),
            stall: Duration::from_secs(2),
            idle: IdleMode::Documented,
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

impl ProducerFrame {
    fn track(&self) -> usize {
        match self {
            Self::Video { .. } => VIDEO,
            Self::Audio { .. } => AUDIO,
        }
    }

    fn pts_us(&self) -> i64 {
        match self {
            Self::Video { pts_us, .. } | Self::Audio { pts_us, .. } => *pts_us,
        }
    }
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

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    #[error("the attachment is closed: {0}")]
    Closed(CloseReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseReason {
    Ended,
    Failed(String),
    Dropped,
    /// The session's capture config, known only at `start`, rules the producer out.
    Incompatible(String),
    GraceExpired,
    SourceGone,
}

impl fmt::Display for CloseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ended => f.write_str("ended"),
            Self::Failed(message) => write!(f, "failed: {message}"),
            Self::Dropped => f.write_str("dropped"),
            Self::Incompatible(message) => write!(f, "incompatible: {message}"),
            Self::GraceExpired => f.write_str("no media within the grace period"),
            Self::SourceGone => f.write_str("the session ended"),
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

/// Frames sent but not delivered: `skipped` before `start` or ahead of a keyframe, `dropped_*` lost after that.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AttachmentStats {
    pub skipped: u64,
    pub dropped_video: u64,
    pub dropped_audio: u64,
}

#[derive(Default)]
struct Link {
    skipped: AtomicU64,
    dropped: [AtomicU64; 2],
    closed: Mutex<Option<CloseReason>>,
}

impl Link {
    fn skip(&self) {
        self.skipped.fetch_add(1, Ordering::Relaxed);
    }

    fn drop_frame(&self, track: usize) {
        self.dropped[track].fetch_add(1, Ordering::Relaxed);
    }

    fn close(&self, reason: CloseReason) {
        let mut closed = self.closed.lock().unwrap_or_else(PoisonError::into_inner);
        closed.get_or_insert(reason);
    }

    fn closed(&self) -> Option<CloseReason> {
        self.closed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn snapshot(&self) -> AttachmentStats {
        AttachmentStats {
            skipped: self.skipped.load(Ordering::Relaxed),
            dropped_video: self.dropped[VIDEO].load(Ordering::Relaxed),
            dropped_audio: self.dropped[AUDIO].load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    Open,
    Resync,
    Overflow,
}

struct Held {
    frame: ProducerFrame,
    at: Instant,
}

struct Current {
    id: u64,
    info: ProducerInfo,
    link: Arc<Link>,
    keyframes: watch::Sender<u64>,
    gate: Gate,
    offset: Option<i64>,
    first_in: [Option<i64>; 2],
    first_at: Option<Instant>,
    held: Vec<Held>,
    active_at: Instant,
    stalled: bool,
}

impl Current {
    fn request_keyframe(&self) {
        self.keyframes.send_modify(|n| *n += 1);
    }

    fn ready(&self) -> bool {
        self.first_in.iter().all(Option::is_some) || self.held.len() >= QUEUE_CAPACITY
    }

    fn drop_held(&mut self) {
        for held in self.held.drain(..) {
            self.link.drop_frame(held.frame.track());
        }
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

    /// The track's last pts carried forward to `now` on the wall clock; `None` before its first frame.
    fn carried(&self, track: usize, now: Instant) -> Option<i64> {
        self.last_media[track]
            .map(|(pts, at)| pts.saturating_add(micros(now.saturating_duration_since(at))))
    }

    /// One offset for both tracks: the smallest that keeps each track at or after its own carried-forward clock.
    fn offset_for(&self, first_in: [Option<i64>; 2], now: Instant) -> i64 {
        let earliest = first_in.iter().flatten().min().copied().unwrap_or(0);
        [VIDEO, AUDIO]
            .into_iter()
            .filter_map(|t| Some(self.carried(t, now)?.saturating_sub(first_in[t]?)))
            .max()
            .unwrap_or_else(|| -earliest)
    }

    fn stamp(&mut self, track: usize, pts: i64) -> u64 {
        let floor = self.last_out[track].map_or(0, |last| last + TICK_US[track]);
        let out = pts.max(floor);
        self.last_out[track] = Some(out);
        out as u64
    }
}

struct Queued {
    frame: MediaFrame,
    owner: Option<Arc<Link>>,
}

impl Queued {
    fn track(&self) -> usize {
        match self.frame {
            MediaFrame::Video(_) => VIDEO,
            MediaFrame::Audio(_) => AUDIO,
        }
    }

    fn is_audio(&self) -> bool {
        self.track() == AUDIO
    }

    fn charge(&self) {
        if let Some(owner) = &self.owner {
            owner.drop_frame(self.track());
        }
    }
}

/// Queues `item` and returns what a full queue lost: video up to a keyframe, else the oldest audio; idle frames never displace video.
fn enqueue(queue: &mut VecDeque<Queued>, item: Queued) -> Vec<Queued> {
    let mut lost = Vec::new();
    if queue.len() >= QUEUE_CAPACITY {
        let oldest_audio = queue.iter().position(Queued::is_audio);
        match &item.frame {
            MediaFrame::Audio(_) if item.owner.is_none() => match oldest_audio {
                Some(i) => lost.extend(queue.remove(i)),
                None => {
                    lost.push(item);
                    return lost;
                }
            },
            MediaFrame::Audio(_) => match oldest_audio {
                Some(i) => lost.extend(queue.remove(i)),
                None => lost.extend(drop_video(queue)),
            },
            MediaFrame::Video(v) => {
                lost.extend(drop_video(queue));
                if !v.keyframe {
                    lost.push(item);
                    return lost;
                }
                if queue.len() >= QUEUE_CAPACITY {
                    lost.extend(queue.pop_front());
                }
            }
        }
    }
    queue.push_back(item);
    lost
}

fn drop_video(queue: &mut VecDeque<Queued>) -> Vec<Queued> {
    let (audio, video) = queue.drain(..).partition(Queued::is_audio);
    *queue = audio;
    video.into()
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
    queue: VecDeque<Queued>,
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
        let cur = self.current.as_ref();
        let stall = cur
            .filter(|c| !c.stalled)
            .map(|c| c.active_at.max(first_start) + options.stall);
        let held = cur
            .filter(|c| c.offset.is_none())
            .and_then(|c| c.first_at)
            .map(|t| t + FIRST_FRAMES_WAIT);
        let expiry = self.absent_since(first_start).map(|t| t + options.grace);
        let idle = self.idle.as_ref().map(|i| i.next);
        [stall, held, expiry, idle].into_iter().flatten().min()
    }

    /// Applies every timer due at `now`; false once the grace period has run out.
    fn advance(&mut self, inner: &Inner, now: Instant) -> bool {
        if self.expired || self.gone {
            return false;
        }
        let (Some(_), Some(first_start)) = (self.running_since, self.first_start) else {
            return true;
        };
        if self
            .current
            .as_ref()
            .and_then(|c| c.first_at.filter(|_| c.offset.is_none()))
            .is_some_and(|t| now >= t + FIRST_FRAMES_WAIT)
        {
            self.release_held();
        }
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
            if let Some(mut cur) = self.current.take() {
                warn!(
                    attachment = cur.id,
                    "producer stalled past the grace period; closing it"
                );
                cur.drop_held();
                cur.link.close(CloseReason::GraceExpired);
            }
            info!("no producer within the grace period; ending the capture");
            self.expired = true;
            self.idle = None;
            self.clear_queue();
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
            let pts = [AUDIO, VIDEO]
                .into_iter()
                .find_map(|t| self.timeline.carried(t, at))
                .unwrap_or(0);
            let pts_us = self.timeline.stamp(AUDIO, pts);
            let silence = MediaFrame::Audio(AudioFrame {
                data: OPUS_SILENCE_FRAME.to_vec(),
                pts_us,
            });
            self.push(silence, None);
        }
        true
    }

    /// Queues a frame and charges every loss to the attachment that sent the lost frame.
    fn push(&mut self, frame: MediaFrame, owner: Option<Arc<Link>>) {
        let queued_keyframe = matches!(&frame, MediaFrame::Video(v) if v.keyframe);
        let lost = enqueue(&mut self.queue, Queued { frame, owner });
        let mut current_lost_video = false;
        for item in &lost {
            item.charge();
            if let (Some(owner), Some(cur)) = (&item.owner, &self.current) {
                current_lost_video |= item.track() == VIDEO && Arc::ptr_eq(owner, &cur.link);
            }
        }
        if current_lost_video
            && !queued_keyframe
            && let Some(cur) = self.current.as_mut()
        {
            cur.gate = Gate::Overflow;
            cur.request_keyframe();
        }
    }

    fn clear_queue(&mut self) {
        for item in self.queue.drain(..) {
            item.charge();
        }
    }

    /// Fixes the attachment's offset from both tracks' first pts, then queues what was held for it.
    fn release_held(&mut self) {
        let Some(cur) = self.current.as_mut() else {
            return;
        };
        let Some(first_at) = cur.first_at else {
            return;
        };
        cur.offset = Some(self.timeline.offset_for(cur.first_in, first_at));
        for Held { frame, at } in std::mem::take(&mut cur.held) {
            self.forward(frame, at);
        }
    }

    /// Stamps and queues a frame of the current attachment, whose offset is fixed.
    fn forward(&mut self, frame: ProducerFrame, at: Instant) {
        let Some(cur) = self.current.as_mut() else {
            return;
        };
        let Some(offset) = cur.offset else {
            return;
        };
        let pts = frame.pts_us().saturating_add(offset);
        let (codec, link) = (cur.info.codec, cur.link.clone());
        let media = self.stamp(frame, pts, at, codec);
        self.push(media, Some(link));
    }

    fn stamp(
        &mut self,
        frame: ProducerFrame,
        pts: i64,
        at: Instant,
        codec: VideoCodec,
    ) -> MediaFrame {
        let track = frame.track();
        let pts_us = self.timeline.stamp(track, pts);
        self.timeline.last_media[track] = Some((pts_us as i64, at));
        match frame {
            ProducerFrame::Video { data, keyframe, .. } => MediaFrame::Video(EncodedVideoFrame {
                codec,
                data,
                pts_us,
                keyframe,
            }),
            ProducerFrame::Audio { data, .. } => MediaFrame::Audio(AudioFrame { data, pts_us }),
        }
    }

    fn detach(&mut self, inner: &Inner, id: u64, reason: CloseReason) {
        let now = Instant::now();
        self.advance(inner, now);
        if self.current.as_ref().is_none_or(|c| c.id != id) {
            return;
        }
        self.release_held();
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
        cur.link.close(reason);
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
    producer: watch::Sender<Option<ProducerInfo>>,
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

/// ADR 0004: H.264 only, a size `Resolution` accepts and an fps `Fps` accepts; the session follows the rest.
fn check_producer(info: &ProducerInfo, config: Option<&CaptureConfig>) -> Result<(), String> {
    if info.codec != VideoCodec::H264 {
        return Err(format!("{:?}: only H.264 is streamed", info.codec));
    }
    if let Some(config) = config
        && config.codec != info.codec
    {
        return Err(format!(
            "{:?}, the session streams {:?}",
            info.codec, config.codec
        ));
    }
    Resolution::new(info.width, info.height).map_err(|e| e.to_string())?;
    Fps::try_from(info.fps).map_err(|e| e.to_string())?;
    Ok(())
}

/// A capture source fed by one producer attachment at a time; its receiver outlives producer swaps.
pub struct ExternalSource {
    inner: Arc<Inner>,
    status: watch::Receiver<SourceState>,
    producer: watch::Receiver<Option<ProducerInfo>>,
    task: Option<JoinHandle<()>>,
}

impl ExternalSource {
    pub fn new(options: ExternalOptions) -> Self {
        let (status_tx, status) = watch::channel(SourceState::Detached);
        let (producer_tx, producer) = watch::channel(None);
        let inner = Arc::new(Inner {
            options,
            state: Mutex::new(State::default()),
            wake: Notify::new(),
            status: status_tx,
            producer: producer_tx,
        });
        Self {
            inner,
            status,
            producer,
            task: None,
        }
    }

    pub fn handle(&self) -> ExternalHandle {
        ExternalHandle {
            inner: Arc::downgrade(&self.inner),
            status: self.status.clone(),
            producer: self.producer.clone(),
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
        if let Some(mut cur) = state.current.take() {
            cur.drop_held();
            cur.link.close(CloseReason::SourceGone);
        }
        state.idle = None;
        state.clear_queue();
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
                check_producer(&c.info, Some(&config))
                    .err()
                    .map(|e| (c.id, e))
            });
            if let Some((id, e)) = incompatible {
                warn!(attachment = id, %e, "closing an incompatible producer");
                state.detach(&self.inner, id, CloseReason::Incompatible(e));
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
        if let Some(cur) = state.current.as_mut() {
            cur.drop_held();
        }
        state.clear_queue();
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
                if let Some(item) = inner.lock().queue.pop_front() {
                    permit.send(item.frame);
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
    producer: watch::Receiver<Option<ProducerInfo>>,
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
        check_producer(&info, state.config.as_ref()).map_err(AttachError::Incompatible)?;
        state.next_id += 1;
        let id = state.next_id;
        let (keyframes, requests) = watch::channel(0);
        let latched = std::mem::take(&mut state.latched_keyframe);
        keyframes.send_modify(|n| *n += 1 + u64::from(latched));
        let link = Arc::new(Link::default());
        state.current = Some(Current {
            id,
            info: info.clone(),
            link: link.clone(),
            keyframes,
            gate: Gate::Resync,
            offset: None,
            first_in: [None; 2],
            first_at: None,
            held: Vec::new(),
            active_at: Instant::now(),
            stalled: false,
        });
        state.idle = None;
        drop(state);
        info!(attachment = id, ?info, "producer attached");
        inner.producer.send_replace(Some(info));
        inner.set_status(SourceState::Producing { attachment: id });
        inner.wake.notify_one();
        Ok(Attachment {
            inner: self.inner.clone(),
            id,
            requests,
            link,
            closed: false,
        })
    }

    pub fn state(&self) -> watch::Receiver<SourceState> {
        self.status.clone()
    }

    /// The latest attached producer's declared video, for announcing it to Discord.
    pub fn producer(&self) -> watch::Receiver<Option<ProducerInfo>> {
        self.producer.clone()
    }
}

pub struct Attachment {
    inner: Weak<Inner>,
    id: u64,
    requests: watch::Receiver<u64>,
    link: Arc<Link>,
    closed: bool,
}

impl Attachment {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Never blocks; frames before `start` or ahead of the first keyframe are skipped.
    pub fn send(&self, frame: impl Into<ProducerFrame>) -> Result<(), SendError> {
        let frame = frame.into();
        let closed = || SendError::Closed(self.link.closed().unwrap_or(CloseReason::SourceGone));
        let inner = self.inner.upgrade().ok_or_else(closed)?;
        let mut guard = inner.lock();
        let now = Instant::now();
        guard.advance(&inner, now);
        let state = &mut *guard;
        let cur = state
            .current
            .as_mut()
            .filter(|c| c.id == self.id)
            .ok_or_else(closed)?;
        cur.active_at = now;
        if cur.stalled {
            cur.stalled = false;
            cur.gate = Gate::Resync;
            cur.request_keyframe();
            info!(attachment = cur.id, "producer resumed");
            inner.set_status(SourceState::Producing { attachment: cur.id });
        }
        if state.running_since.is_none() {
            self.link.skip();
            return Ok(());
        }
        let track = frame.track();
        cur.first_in[track].get_or_insert(frame.pts_us());
        cur.first_at.get_or_insert(now);
        if let ProducerFrame::Video { keyframe, .. } = &frame {
            if *keyframe {
                cur.gate = Gate::Open;
            } else if cur.gate != Gate::Open {
                match cur.gate {
                    Gate::Overflow => self.link.drop_frame(VIDEO),
                    _ => self.link.skip(),
                }
                return Ok(());
            }
        }
        match cur.offset {
            Some(_) => state.forward(frame, now),
            None => {
                cur.held.push(Held { frame, at: now });
                if cur.ready() {
                    state.release_held();
                }
            }
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
        self.link.snapshot()
    }

    pub fn close(mut self, reason: CloseReason) {
        self.detach(reason);
        self.closed = true;
    }

    fn detach(&self, reason: CloseReason) {
        if let Some(inner) = self.inner.upgrade() {
            inner.lock().detach(&inner, self.id, reason);
        }
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        if !self.closed {
            self.detach(CloseReason::Dropped);
        }
    }
}

#[cfg(test)]
mod tests;
