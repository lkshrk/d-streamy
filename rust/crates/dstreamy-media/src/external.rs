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

/// The `target` of a session fed by producers (ADR 0004); only `serve` accepts it.
pub const EXTERNAL_TARGET_ID: &str = "external";
/// RFC 6716 §3.1 and §3.2.2: TOC config 31 (CELT FB 20 ms), mono, code 0, one frame.
pub const OPUS_SILENCE_FRAME: [u8; 3] = [0xF8, 0xFF, 0xFE];
/// Discord `voice-connections.mdx:502-504`: five silence frames before a break in transmission.
pub const DOCUMENTED_SILENCE_FRAMES: u32 = 5;
const SILENCE_SPACING: Duration = Duration::from_micros(OPUS_FRAME_DURATION_US);
/// One RTP tick rounded up, as in the pump's `Timebase`: 90 kHz video, 48 kHz audio.
const TICK_US: [i64; 2] = [12, 21];
/// A producer that sends only one track still starts within this.
pub const FIRST_FRAMES_WAIT: Duration = Duration::from_millis(500);
/// Bounds that keep every deadline representable as an `Instant`.
pub const MAX_GRACE: Duration = Duration::from_secs(3600);
/// Upper bound of `[serve] stall_ms` (ADR 0004).
pub const MAX_STALL: Duration = Duration::from_secs(600);
const VIDEO: usize = 0;
const AUDIO: usize = 1;
const CHANNEL_CAPACITY: usize = 1;
const QUEUE_CAPACITY: usize = FRAME_CHANNEL_CAPACITY - CHANNEL_CAPACITY;
/// Queue/first-frame budget; with one 8 MiB codec frame, a producer uses about 72 MiB worst case.
pub const QUEUE_MAX_BYTES: usize = 64 << 20;
/// How far behind the wall clock idle silence catches up after a long sleep; older frames are skipped.
const IDLE_CATCH_UP: Duration = Duration::from_millis(500);
/// A producer must restore video with a keyframe within one maximum keyframe interval after a loss.
pub const RESYNC_BOUND: Duration = Duration::from_secs(2);

/// What is sent after a producer detached (ADR 0004 `[serve] idle`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IdleMode {
    /// Up to five 20 ms Opus silence packets after detach, then no idle media.
    #[default]
    Documented,
    /// No media while detached.
    None,
    /// One Opus silence packet every 20 ms while detached.
    Continuous,
}

/// Grace, stall and idle settings of an external source, from `[serve]` and a `start`'s `grace_secs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalOptions {
    /// Allowed absence after source start, detach or a whole-producer stall.
    pub grace: Duration,
    /// Media inactivity before a producer is stalled; also detects video gaps while audio flows.
    pub stall: Duration,
    pub idle: IdleMode,
}

impl ExternalOptions {
    /// Fails when `stall` is shorter than `FIRST_FRAMES_WAIT`, so a stall never falls while first frames are held, or above its bound.
    pub fn validate(&self) -> Result<(), OptionsError> {
        if self.stall < FIRST_FRAMES_WAIT {
            return Err(OptionsError::StallTooShort(self.stall));
        }
        if self.stall > MAX_STALL {
            return Err(OptionsError::StallTooLong(self.stall));
        }
        if self.grace > MAX_GRACE {
            return Err(OptionsError::GraceTooLong(self.grace));
        }
        Ok(())
    }
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

/// What a producer's `hello` declares: codec, size and frame rate, which the session announces to Discord.
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

    fn len(&self) -> usize {
        match self {
            Self::Video { data, .. } | Self::Audio { data, .. } => data.len(),
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

/// An `ExternalOptions` value outside the ADR 0004 bounds.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OptionsError {
    #[error("stall {0:?} is shorter than the {FIRST_FRAMES_WAIT:?} first-frames wait")]
    StallTooShort(Duration),
    #[error("stall {0:?} is longer than {MAX_STALL:?}")]
    StallTooLong(Duration),
    #[error("grace {0:?} is longer than {MAX_GRACE:?}")]
    GraceTooLong(Duration),
}

/// Why a producer could not attach: no session, or a producer already attached.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttachError {
    #[error("no session holds the external source")]
    NoSession,
    #[error("another producer is attached")]
    Busy,
    #[error("incompatible producer: {0}")]
    Incompatible(String),
}

/// Why a frame was refused by an attachment that is closed or over its limits.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    #[error("the attachment is closed: {0}")]
    Closed(CloseReason),
}

/// Why an attachment ended: the producer stopped, a rule failed, or the session let go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseReason {
    Ended,
    Failed(String),
    /// The producer handle was dropped without an explicit close.
    Dropped,
    /// The session's capture config, known only at `start`, rules the producer out.
    Incompatible(String),
    GraceExpired,
    SourceGone,
    /// A new producer attached while this one was stalled.
    Replaced,
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
            Self::Replaced => f.write_str("replaced by a new producer while stalled"),
        }
    }
}

/// Why the source has no media right now: the producer left, or it stalled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptReason {
    Detached,
    Stalled,
    /// Audio still arrives but video stopped.
    VideoStalled,
}

/// The external source as `status` and `source` events report it (ADR 0004).
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
struct Counters {
    skipped: AtomicU64,
    dropped: [AtomicU64; 2],
}

impl Counters {
    fn snapshot(&self) -> AttachmentStats {
        AttachmentStats {
            skipped: self.skipped.load(Ordering::Relaxed),
            dropped_video: self.dropped[VIDEO].load(Ordering::Relaxed),
            dropped_audio: self.dropped[AUDIO].load(Ordering::Relaxed),
        }
    }
}

#[derive(Default)]
struct Link {
    own: Counters,
    total: Arc<Counters>,
    closed: Mutex<Option<CloseReason>>,
}

impl Link {
    fn new(total: Arc<Counters>) -> Self {
        Self {
            total,
            ..Self::default()
        }
    }

    fn skip(&self) {
        for c in [&self.own, &*self.total] {
            c.skipped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn drop_frame(&self, track: usize) {
        for c in [&self.own, &*self.total] {
            c.dropped[track].fetch_add(1, Ordering::Relaxed);
        }
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
        self.own.snapshot()
    }
}

/// Every attachment's losses so far, and the source queue's length now and at its highest.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SourceStats {
    pub lost: AttachmentStats,
    pub queued: usize,
    pub max_queued: usize,
    /// Keyframes asked for because the queue overflowed and video was lost.
    pub overflow_keyframe_requests: u64,
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
    /// When a loss closed the gate; `None` for the gate an attach or a restart closes, which only a keyframe opens.
    gate_closed_at: Option<Instant>,
    offset: Option<i64>,
    first_in: [Option<i64>; 2],
    first_at: Option<Instant>,
    held: Vec<Held>,
    active_at: Instant,
    video_at: Option<Instant>,
    stalled: bool,
    video_stalled: bool,
    /// False until the producer may send; the stall timer only runs while true.
    timing: bool,
}

impl Current {
    fn request_keyframe(&self) {
        self.keyframes.send_modify(|n| *n += 1);
    }

    fn close_gate_after_loss(&mut self, gate: Gate, now: Instant) {
        self.gate = gate;
        self.gate_closed_at.get_or_insert(now);
        self.request_keyframe();
    }

    fn open_gate(&mut self) {
        self.gate = Gate::Open;
        self.gate_closed_at = None;
    }

    fn holds(&self, len: usize) -> bool {
        self.held.len() < QUEUE_CAPACITY
            && self.held.iter().map(|h| h.frame.len()).sum::<usize>() + len <= QUEUE_MAX_BYTES
    }

    /// True when it dropped anything, which breaks the video continuity the gate must then restore.
    fn drop_held(&mut self) -> bool {
        let dropped = !self.held.is_empty();
        for held in self.held.drain(..) {
            self.link.drop_frame(held.frame.track());
        }
        dropped
    }

    fn status(&self) -> SourceState {
        match self.video_at {
            Some(since) if self.video_stalled => SourceState::Interrupted {
                since,
                reason: InterruptReason::VideoStalled,
            },
            _ => SourceState::Producing {
                attachment: self.id,
            },
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

    /// As `carried`, falling back to the other track's clock for a track that never had a frame.
    fn clock(&self, track: usize, now: Instant) -> Option<i64> {
        self.carried(track, now)
            .or_else(|| self.carried(1 - track, now))
    }

    /// One offset for both tracks: the smallest that keeps each track at or after its own carried-forward clock.
    fn offset_for(&self, first_in: [Option<i64>; 2], now: Instant) -> i64 {
        let earliest = first_in.iter().flatten().min().copied().unwrap_or(0);
        [VIDEO, AUDIO]
            .into_iter()
            .filter_map(|t| Some(self.clock(t, now)?.saturating_sub(first_in[t]?)))
            .max()
            .map(whole_ms)
            .unwrap_or_else(|| earliest.saturating_neg())
    }

    fn stamp(&mut self, track: usize, pts: i64) -> u64 {
        let floor = self.last_out[track].map_or(0, |last| last.saturating_add(TICK_US[track]));
        let out = pts.max(floor);
        self.last_out[track] = Some(out);
        u64::try_from(out).unwrap_or(0)
    }
}

/// Rounds up to a whole millisecond, a whole number of ticks on both RTP clocks, so per-frame tick rounding stays even.
fn whole_ms(us: i64) -> i64 {
    match us.rem_euclid(1000) {
        0 => us,
        r => us.saturating_add(1000 - r),
    }
}

struct Queued {
    frame: MediaFrame,
    owner: Option<Arc<Link>>,
}

impl Queued {
    fn len(&self) -> usize {
        match &self.frame {
            MediaFrame::Video(v) => v.data.len(),
            MediaFrame::Audio(a) => a.data.len(),
        }
    }

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
    let full = |queue: &VecDeque<Queued>| {
        queue.len() >= QUEUE_CAPACITY
            || queue.iter().map(Queued::len).sum::<usize>() + item.len() > QUEUE_MAX_BYTES
    };
    while full(queue) && !queue.is_empty() {
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
                if full(queue) {
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
    max_queued: usize,
    overflow_keyframe_requests: u64,
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
            .filter(|c| c.timing && !c.stalled)
            .map(|c| c.active_at.max(first_start) + options.stall);
        let held = cur
            .filter(|c| c.offset.is_none())
            .and_then(|c| c.first_at)
            .map(|t| t + FIRST_FRAMES_WAIT);
        let gate = cur.and_then(|c| c.gate_closed_at).map(|t| t + RESYNC_BOUND);
        let expiry = self.absent_since(first_start).map(|t| t + options.grace);
        let idle = self.idle.as_ref().map(|i| i.next);
        [stall, held, gate, expiry, idle]
            .into_iter()
            .flatten()
            .min()
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
            && cur.timing
            && !cur.stalled
        {
            let last = cur.active_at.max(first_start);
            if now >= last + inner.options.stall {
                cur.stalled = true;
                if cur.drop_held() {
                    cur.close_gate_after_loss(Gate::Resync, now);
                }
                warn!(attachment = cur.id, "producer stalled");
                inner.set_status(SourceState::Interrupted {
                    since: last,
                    reason: InterruptReason::Stalled,
                });
            }
        }
        if let Some(cur) = self.current.as_mut()
            && cur.gate_closed_at.is_some_and(|t| now >= t + RESYNC_BOUND)
        {
            let id = cur.id;
            cur.drop_held();
            self.clear_queue();
            self.detach_at(
                inner,
                id,
                CloseReason::Incompatible(
                    "no keyframe within 2 s after video loss; restart the producer and set its keyframe interval to at most 2 s".to_owned(),
                ),
                now,
            );
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
        if let Some(idle) = self.idle.as_mut()
            && let Some(floor) = now.checked_sub(IDLE_CATCH_UP)
            && idle.next < floor
        {
            idle.next = floor;
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
            let pts = self.timeline.clock(AUDIO, at).unwrap_or(0);
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
        self.max_queued = self.max_queued.max(self.queue.len());
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
            cur.close_gate_after_loss(Gate::Overflow, Instant::now());
            self.overflow_keyframe_requests += 1;
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
        if let ProducerFrame::Video { keyframe, .. } = &frame {
            if *keyframe {
                cur.open_gate();
            } else if cur.gate != Gate::Open {
                cur.link.drop_frame(VIDEO);
                return;
            }
        }
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
        self.detach_at(inner, id, reason, now);
    }

    fn detach_at(&mut self, inner: &Inner, id: u64, reason: CloseReason, now: Instant) {
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
    totals: Arc<Counters>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Closes the attachment as `SourceGone` and refuses any later one.
    fn end(&self) {
        let mut state = self.lock();
        state.gone = true;
        if let Some(mut cur) = state.current.take() {
            cur.drop_held();
            cur.link.close(CloseReason::SourceGone);
        }
        state.idle = None;
        state.clear_queue();
        self.set_status(SourceState::Detached);
        self.wake.notify_one();
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
    /// Fails as [`ExternalOptions::validate`] does.
    pub fn new(options: ExternalOptions) -> Result<Self, OptionsError> {
        options.validate()?;
        let (status_tx, status) = watch::channel(SourceState::Detached);
        let (producer_tx, producer) = watch::channel(None);
        let inner = Arc::new(Inner {
            options,
            state: Mutex::new(State::default()),
            wake: Notify::new(),
            status: status_tx,
            producer: producer_tx,
            totals: Arc::default(),
        });
        Ok(Self {
            inner,
            status,
            producer,
            task: None,
        })
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
        self.inner.end();
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
            state.first_start = Some(now);
            state.running_since = Some(now);
            if let Some(idle) = state.idle.as_mut() {
                idle.next = idle.next.max(now);
            }
            if let Some(cur) = state.current.as_mut() {
                cur.gate = Gate::Resync;
                cur.gate_closed_at = None;
                cur.stalled = false;
                cur.video_stalled = false;
                cur.request_keyframe();
                self.inner
                    .set_status(SourceState::Producing { attachment: cur.id });
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
    /// Starts a producer's stall timer immediately; a compatible hello may replace a fully stalled attachment.
    pub fn attach(&self, info: ProducerInfo) -> Result<Attachment, AttachError> {
        let attachment = self.attach_paused(info)?;
        attachment.start_stall_timer();
        Ok(attachment)
    }

    /// As [`ExternalHandle::attach`], with the stall timer stopped until [`Attachment::start_stall_timer`].
    pub fn attach_paused(&self, info: ProducerInfo) -> Result<Attachment, AttachError> {
        let inner = self.inner.upgrade().ok_or(AttachError::NoSession)?;
        let mut state = inner.lock();
        if !state.advance(&inner, Instant::now()) {
            return Err(AttachError::NoSession);
        }
        check_producer(&info, state.config.as_ref()).map_err(AttachError::Incompatible)?;
        match state.current.as_ref() {
            Some(cur) if cur.stalled => {
                let id = cur.id;
                state.detach(&inner, id, CloseReason::Replaced);
            }
            Some(_) => return Err(AttachError::Busy),
            None => {}
        }
        state.next_id += 1;
        let id = state.next_id;
        let (keyframes, requests) = watch::channel(0);
        let latched = std::mem::take(&mut state.latched_keyframe);
        keyframes.send_modify(|n| *n += 1 + u64::from(latched));
        let link = Arc::new(Link::new(inner.totals.clone()));
        state.current = Some(Current {
            id,
            info: info.clone(),
            link: link.clone(),
            keyframes,
            gate: Gate::Resync,
            gate_closed_at: None,
            offset: None,
            first_in: [None; 2],
            first_at: None,
            held: Vec::new(),
            active_at: Instant::now(),
            video_at: None,
            stalled: false,
            video_stalled: false,
            timing: false,
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

    /// Ends the source for its session, which stopped but may still hold it.
    pub fn end(&self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.end();
        }
    }

    /// `None` once the source is gone.
    pub fn stats(&self) -> Option<SourceStats> {
        let inner = self.inner.upgrade()?;
        let state = inner.lock();
        Some(SourceStats {
            lost: inner.totals.snapshot(),
            queued: state.queue.len(),
            max_queued: state.max_queued,
            overflow_keyframe_requests: state.overflow_keyframe_requests,
        })
    }
}

/// A producer's handle on the external source: feeds frames and closes; the session never sees it.
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
        let resumed = std::mem::take(&mut cur.stalled);
        // A video stall is told by audio arriving `stall` after the last video frame, so no timer and no flicker when both tracks stop.
        let video_changed = match &frame {
            ProducerFrame::Video { .. } => {
                cur.video_at = Some(now);
                std::mem::take(&mut cur.video_stalled)
            }
            ProducerFrame::Audio { .. } => {
                let stalled = cur
                    .video_at
                    .is_some_and(|at| !cur.video_stalled && now >= at + inner.options.stall);
                if stalled {
                    cur.video_stalled = true;
                    warn!(
                        attachment = cur.id,
                        "producer video stalled while audio flows"
                    );
                }
                stalled
            }
        };
        if resumed {
            if cur.gate != Gate::Open {
                cur.request_keyframe();
            }
            info!(attachment = cur.id, "producer resumed");
        }
        if resumed || video_changed {
            inner.set_status(cur.status());
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
                cur.open_gate();
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
            None if cur.holds(frame.len()) => {
                cur.held.push(Held { frame, at: now });
                if cur.first_in.iter().all(Option::is_some) {
                    state.release_held();
                }
            }
            None => {
                state.release_held();
                state.forward(frame, now);
            }
        }
        drop(guard);
        inner.wake.notify_one();
        Ok(())
    }

    /// Starts the stall timer, from now; the producer may send from here on.
    pub fn start_stall_timer(&self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let mut state = inner.lock();
        if let Some(cur) = state
            .current
            .as_mut()
            .filter(|c| c.id == self.id && !c.timing)
        {
            cur.timing = true;
            cur.active_at = Instant::now();
        }
        drop(state);
        inner.wake.notify_one();
    }

    /// Bumped on every keyframe the session asks for, and once on attach.
    pub fn keyframe_requests(&self) -> watch::Receiver<u64> {
        self.requests.clone()
    }

    pub fn stats(&self) -> AttachmentStats {
        self.link.snapshot()
    }

    /// Why the source closed this attachment, once it has.
    pub fn close_reason(&self) -> Option<CloseReason> {
        self.link.closed()
    }

    /// Detaches this producer while preserving the source timeline and applying absence grace.
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
