use std::cell::Cell;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use proptest::prelude::*;
use tokio::time::{Instant, sleep, sleep_until};

use super::*;

const FPS: u32 = 30;
const FRAME_US: i64 = 1_000_000 / FPS as i64;
const AUDIO_US: i64 = OPUS_FRAME_DURATION_US as i64;
const KEYFRAME_EVERY: i64 = 60;
const GRACE: Duration = Duration::from_secs(30);
const STALL: Duration = Duration::from_secs(2);
const MS: Duration = Duration::from_millis(1);
const IDLE_MODES: [IdleMode; 3] = [IdleMode::Documented, IdleMode::None, IdleMode::Continuous];

fn options(idle: IdleMode) -> ExternalOptions {
    ExternalOptions {
        grace: GRACE,
        stall: STALL,
        idle,
    }
}

fn info() -> ProducerInfo {
    ProducerInfo {
        codec: VideoCodec::H264,
        fps: FPS,
        width: 1280,
        height: 720,
    }
}

fn us(d: Duration) -> i64 {
    micros(d)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Marker {
    attachment: u8,
    pts_in: i64,
    sent_us: i64,
    skew_us: i64,
}

impl Marker {
    fn encode(&self) -> Vec<u8> {
        let mut data = vec![self.attachment];
        for v in [self.pts_in, self.sent_us, self.skew_us] {
            data.extend_from_slice(&v.to_be_bytes());
        }
        data
    }

    fn decode(data: &[u8]) -> Option<Self> {
        let field = |i: usize| i64::from_be_bytes(data[1 + 8 * i..9 + 8 * i].try_into().unwrap());
        (data.len() == 25).then(|| Self {
            attachment: data[0],
            pts_in: field(0),
            sent_us: field(1),
            skew_us: field(2),
        })
    }
}

#[derive(Debug, Clone)]
struct Received {
    frame: MediaFrame,
    at_us: i64,
}

impl Received {
    fn pts(&self) -> i64 {
        self.frame.pts_us() as i64
    }

    fn track(&self) -> usize {
        match self.frame {
            MediaFrame::Video(_) => VIDEO,
            MediaFrame::Audio(_) => AUDIO,
        }
    }

    fn data(&self) -> &[u8] {
        match &self.frame {
            MediaFrame::Video(v) => &v.data,
            MediaFrame::Audio(a) => &a.data,
        }
    }

    fn marker(&self) -> Option<Marker> {
        Marker::decode(self.data())
    }

    fn keyframe(&self) -> bool {
        matches!(&self.frame, MediaFrame::Video(v) if v.keyframe)
    }

    fn sent_us(&self) -> i64 {
        self.marker().map_or(self.at_us, |m| m.sent_us)
    }

    /// The wall time this frame's pts stands for: a skewed track is ahead or behind its send time.
    fn wall_us(&self) -> i64 {
        self.marker().map_or(self.at_us, |m| m.sent_us + m.skew_us)
    }
}

#[derive(Default)]
struct Output {
    frames: Vec<Received>,
    closed_at: Option<i64>,
}

/// Records every frame of every receiver of one source, in arrival order.
#[derive(Clone)]
struct Collector {
    epoch: Instant,
    all: Arc<Mutex<Vec<Received>>>,
}

impl Collector {
    fn spawn(&self, mut rx: FrameReceiver) -> Arc<Mutex<Output>> {
        let out = Arc::new(Mutex::new(Output::default()));
        let (all, mine, epoch) = (self.all.clone(), out.clone(), self.epoch);
        tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                let received = Received {
                    frame,
                    at_us: us(Instant::now() - epoch),
                };
                all.lock().unwrap().push(received.clone());
                mine.lock().unwrap().frames.push(received);
            }
            mine.lock().unwrap().closed_at = Some(us(Instant::now() - epoch));
        });
        out
    }

    fn frames(&self) -> Vec<Received> {
        self.all.lock().unwrap().clone()
    }
}

/// Sends 30 fps video and 20 ms audio on its own clock with pts `start_pts + elapsed`; the payload is a marker.
struct Producer {
    attachment: Attachment,
    tag: u8,
    epoch: Instant,
    origin: Instant,
    start_pts: i64,
    audio_skew: i64,
    next: [i64; 2],
    first_keyframe: bool,
    last_sent: Cell<Option<Instant>>,
    refused_at: Cell<Option<Instant>>,
}

impl Producer {
    fn new(attachment: Attachment, tag: u8, epoch: Instant, start_pts: i64) -> Self {
        Self {
            attachment,
            tag,
            epoch,
            origin: Instant::now(),
            start_pts,
            audio_skew: 0,
            next: [0, 0],
            first_keyframe: true,
            last_sent: Cell::new(None),
            refused_at: Cell::new(None),
        }
    }

    fn with_audio_skew(mut self, skew_us: i64) -> Self {
        self.audio_skew = skew_us;
        self
    }

    fn starting_mid_gop(mut self) -> Self {
        self.first_keyframe = false;
        self
    }

    fn due(&self, track: usize) -> i64 {
        match track {
            VIDEO => self.next[VIDEO] * 1_000_000 / i64::from(FPS),
            _ => self.next[AUDIO] * AUDIO_US,
        }
    }

    /// Sends what falls due within `dur`; the grid continues across calls, skipping times already past.
    async fn run(&mut self, dur: Duration) {
        let end = Instant::now() + dur;
        loop {
            let elapsed = us(Instant::now() - self.origin);
            for track in [VIDEO, AUDIO] {
                while self.due(track) < elapsed {
                    self.next[track] += 1;
                }
            }
            let track = if self.due(VIDEO) <= self.due(AUDIO) {
                VIDEO
            } else {
                AUDIO
            };
            let t = self.due(track);
            let at = self.origin + Duration::from_micros(t as u64);
            if at >= end {
                sleep_until(end).await;
                return;
            }
            sleep_until(at).await;
            self.send(track, t);
            self.next[track] += 1;
        }
    }

    fn send(&self, track: usize, t: i64) {
        let skew = if track == AUDIO { self.audio_skew } else { 0 };
        let marker = Marker {
            attachment: self.tag,
            pts_in: self.start_pts + t + skew,
            sent_us: us(Instant::now() - self.epoch),
            skew_us: skew,
        };
        let index = self.next[VIDEO];
        let frame = match track {
            VIDEO => ProducerFrame::Video {
                data: marker.encode(),
                pts_us: marker.pts_in,
                keyframe: index % KEYFRAME_EVERY == 0 && (index > 0 || self.first_keyframe),
            },
            _ => ProducerFrame::Audio {
                data: marker.encode(),
                pts_us: marker.pts_in,
            },
        };
        match self.attachment.send(frame) {
            Ok(()) => self.last_sent.set(Some(Instant::now())),
            Err(SendError::Closed(_)) => {
                self.refused_at
                    .set(self.refused_at.get().or(Some(Instant::now())));
            }
        }
    }
}

struct Rig {
    source: ExternalSource,
    handle: ExternalHandle,
    epoch: Instant,
    collector: Collector,
    receiver: Arc<Mutex<Output>>,
}

impl Rig {
    fn new(idle: IdleMode) -> Self {
        let source = ExternalSource::new(options(idle));
        let handle = source.handle();
        let epoch = Instant::now();
        Self {
            source,
            handle,
            epoch,
            collector: Collector {
                epoch,
                all: Arc::default(),
            },
            receiver: Arc::default(),
        }
    }

    async fn started(idle: IdleMode) -> Self {
        let mut rig = Self::new(idle);
        rig.start().await;
        rig
    }

    async fn start(&mut self) {
        let rx = self.source.start(CaptureConfig::default()).await.unwrap();
        self.receiver = self.collector.spawn(rx);
    }

    async fn restart(&mut self) {
        self.source.stop().await.unwrap();
        self.start().await;
    }

    fn producer(&self, tag: u8, start_pts: i64) -> Producer {
        Producer::new(
            self.handle.attach(info()).unwrap(),
            tag,
            self.epoch,
            start_pts,
        )
    }

    fn now_us(&self) -> i64 {
        us(Instant::now() - self.epoch)
    }

    fn closed(&self) -> Option<i64> {
        self.receiver.lock().unwrap().closed_at
    }
}

async fn settle() {
    sleep(MS).await;
}

fn audio(pts_us: i64) -> ProducerFrame {
    ProducerFrame::Audio {
        data: vec![1],
        pts_us,
    }
}

fn video(pts_us: i64, keyframe: bool) -> ProducerFrame {
    ProducerFrame::Video {
        data: vec![0],
        pts_us,
        keyframe,
    }
}

fn strictly_increasing(frames: &[Received]) {
    for track in [VIDEO, AUDIO] {
        let pts: Vec<i64> = frames
            .iter()
            .filter(|f| f.track() == track)
            .map(Received::pts)
            .collect();
        for w in pts.windows(2) {
            assert!(w[1] > w[0], "track {track} pts not increasing: {w:?}");
        }
    }
}

/// `pts_out - (sent - t0)` stays within one video frame; a gap counted twice breaks it.
fn on_wall_clock(frames: &[Received], t0_us: i64) {
    for f in frames {
        let wall = f.wall_us() - t0_us;
        let drift = f.pts() - wall;
        assert!(
            drift.abs() <= FRAME_US,
            "pts {} at wall {wall} drifts {drift} µs",
            f.pts()
        );
    }
}

/// Per attachment, `out - in` of video and audio agree within one video frame.
fn av_aligned(frames: &[Received]) {
    let mut tags: Vec<u8> = frames
        .iter()
        .filter_map(|f| f.marker().map(|m| m.attachment))
        .collect();
    tags.sort_unstable();
    tags.dedup();
    for tag in tags {
        let shifts = |track: usize| -> Vec<i64> {
            frames
                .iter()
                .filter(|f| f.track() == track)
                .filter_map(|f| {
                    f.marker()
                        .filter(|m| m.attachment == tag)
                        .map(|m| f.pts() - m.pts_in)
                })
                .collect()
        };
        let (v, a) = (shifts(VIDEO), shifts(AUDIO));
        let (Some(vmin), Some(vmax)) = (v.iter().min(), v.iter().max()) else {
            continue;
        };
        let (Some(amin), Some(amax)) = (a.iter().min(), a.iter().max()) else {
            continue;
        };
        assert!(
            (vmax - amin).abs() <= FRAME_US && (amax - vmin).abs() <= FRAME_US,
            "attachment {tag}: video shift {vmin}..{vmax}, audio shift {amin}..{amax}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn receiver_stays_open_when_a_producer_reattaches_just_inside_grace() {
    let rig = Rig::started(IdleMode::Documented).await;
    let mut p = rig.producer(1, 0);
    p.run(Duration::from_secs(1)).await;
    drop(p);
    sleep(GRACE - MS).await;
    let mut p = rig.producer(2, 0);
    p.run(Duration::from_secs(1)).await;
    assert_eq!(rig.closed(), None);
    let frames = rig.collector.frames();
    assert!(
        frames
            .iter()
            .any(|f| f.marker().is_some_and(|m| m.attachment == 2))
    );
}

#[tokio::test(start_paused = true)]
async fn receiver_closes_when_nothing_reattaches_by_grace() {
    let rig = Rig::started(IdleMode::Documented).await;
    let mut p = rig.producer(1, 0);
    p.run(Duration::from_secs(1)).await;
    drop(p);
    let detached = rig.now_us();
    sleep(GRACE).await;
    assert_eq!(
        rig.handle.attach(info()).err(),
        Some(AttachError::NoSession)
    );
    settle().await;
    assert_eq!(rig.closed(), Some(detached + us(GRACE)));
    assert_eq!(*rig.handle.state().borrow(), SourceState::Detached);
}

#[tokio::test(start_paused = true)]
async fn receiver_closes_at_grace_after_start_when_never_attached() {
    let rig = Rig::started(IdleMode::Documented).await;
    sleep(GRACE - MS).await;
    assert_eq!(rig.closed(), None);
    sleep(2 * MS).await;
    assert_eq!(rig.closed(), Some(us(GRACE)));
}

#[tokio::test(start_paused = true)]
async fn grace_counts_from_capture_start_not_from_handle_creation() {
    let mut rig = Rig::new(IdleMode::Documented);
    let early = rig.handle.attach(info()).unwrap();
    early.send(audio(0)).unwrap();
    drop(early);
    sleep(GRACE * 3).await;
    rig.start().await;
    let started = rig.now_us();
    sleep(GRACE - MS).await;
    assert_eq!(rig.closed(), None);
    sleep(2 * MS).await;
    assert_eq!(rig.closed(), Some(started + us(GRACE)));
}

#[tokio::test(start_paused = true)]
async fn stop_closes_the_receiver_at_once() {
    let mut rig = Rig::started(IdleMode::Documented).await;
    let _p = rig.producer(1, 0);
    rig.source.stop().await.unwrap();
    settle().await;
    assert_eq!(rig.closed(), Some(0));
}

#[tokio::test(start_paused = true)]
async fn a_second_attach_is_busy_and_the_first_keeps_working() {
    let rig = Rig::started(IdleMode::Documented).await;
    let mut p = rig.producer(1, 0);
    assert_eq!(rig.handle.attach(info()).err(), Some(AttachError::Busy));
    p.run(Duration::from_millis(500)).await;
    assert_eq!(rig.collector.frames().len(), 15 + 25);
    assert_eq!(
        *rig.handle.state().borrow(),
        SourceState::Producing { attachment: 1 }
    );
}

// ADR 0004: refuse what `Resolution` or `Fps` reject and anything but H.264.
#[tokio::test(start_paused = true)]
async fn invalid_producer_video_is_incompatible() {
    let rig = Rig::started(IdleMode::Documented).await;
    let cases = [
        ProducerInfo {
            codec: VideoCodec::H265,
            ..info()
        },
        ProducerInfo { fps: 25, ..info() },
        ProducerInfo { fps: 0, ..info() },
        ProducerInfo {
            width: 1921,
            ..info()
        },
        ProducerInfo {
            width: 1280,
            height: 1082,
            ..info()
        },
        ProducerInfo {
            width: 641,
            height: 360,
            ..info()
        },
    ];
    for case in cases {
        assert!(
            matches!(
                rig.handle.attach(case.clone()),
                Err(AttachError::Incompatible(_))
            ),
            "{case:?}"
        );
    }
    assert_eq!(*rig.handle.producer().borrow(), None);
}

#[tokio::test(start_paused = true)]
async fn a_reattach_with_another_valid_size_and_fps_is_accepted_and_published() {
    let rig = Rig::started(IdleMode::Documented).await;
    let mut producer = rig.handle.producer();
    let first = rig.handle.attach(info()).unwrap();
    assert_eq!(*producer.borrow_and_update(), Some(info()));
    drop(first);
    let canvas = ProducerInfo {
        codec: VideoCodec::H264,
        fps: 60,
        width: 1920,
        height: 1080,
    };
    let second = rig.handle.attach(canvas.clone()).unwrap();
    assert!(producer.has_changed().unwrap());
    assert_eq!(*producer.borrow_and_update(), Some(canvas));
    drop(second);
    let small = ProducerInfo {
        fps: 15,
        width: 640,
        height: 360,
        ..info()
    };
    let _third = rig.handle.attach(small.clone()).unwrap();
    assert_eq!(*producer.borrow(), Some(small));
}

#[tokio::test(start_paused = true)]
async fn an_early_producer_ruled_out_at_start_is_closed_as_incompatible() {
    let mut rig = Rig::new(IdleMode::Documented);
    let early = rig.handle.attach(info()).unwrap();
    let h265 = CaptureConfig {
        codec: VideoCodec::H265,
        ..CaptureConfig::default()
    };
    let _rx = rig.source.start(h265).await.unwrap();
    assert!(matches!(
        early.send(audio(0)),
        Err(SendError::Closed(CloseReason::Incompatible(_)))
    ));
    assert!(matches!(
        rig.handle.attach(info()),
        Err(AttachError::Incompatible(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn pts_continue_across_producers_restarting_at_zero_huge_or_negative() {
    let rig = Rig::started(IdleMode::Documented).await;
    let gaps = [
        Duration::from_millis(100),
        Duration::from_secs(5),
        Duration::from_secs(1),
    ];
    let starts = [5_000_000, 0, 1_000_000_000_000, -50_000];
    for (i, start) in starts.into_iter().enumerate() {
        let mut p = rig.producer(i as u8 + 1, start);
        p.run(Duration::from_secs(2)).await;
        drop(p);
        if let Some(gap) = gaps.get(i) {
            sleep(*gap).await;
        }
    }
    let frames = rig.collector.frames();
    assert_eq!(frames[0].pts(), 0);
    strictly_increasing(&frames);
    on_wall_clock(&frames, 0);
}

#[tokio::test(start_paused = true)]
async fn negative_first_pts_map_to_zero_and_never_below() {
    let rig = Rig::started(IdleMode::None).await;
    let a = rig.handle.attach(info()).unwrap();
    a.send(audio(-40_000)).unwrap();
    a.send(video(-30_000, true)).unwrap();
    a.send(audio(-60_000)).unwrap();
    a.send(audio(-20_000)).unwrap();
    settle().await;
    let pts: Vec<(usize, i64)> = rig
        .collector
        .frames()
        .iter()
        .map(|f| (f.track(), f.pts()))
        .collect();
    assert_eq!(
        pts,
        [(AUDIO, 0), (VIDEO, 10_000), (AUDIO, 21), (AUDIO, 20_000)]
    );
}

#[tokio::test(start_paused = true)]
async fn a_non_monotonic_input_pts_is_clamped_to_one_tick_after_the_last() {
    let rig = Rig::started(IdleMode::None).await;
    let a = rig.handle.attach(info()).unwrap();
    for frame in [
        video(0, true),
        video(33_333, false),
        video(33_000, false),
        audio(0),
        audio(0),
    ] {
        a.send(frame).unwrap();
    }
    settle().await;
    let pts: Vec<i64> = rig.collector.frames().iter().map(Received::pts).collect();
    assert_eq!(pts, [0, 33_333, 33_345, 0, 21]);
}

#[tokio::test(start_paused = true)]
async fn pts_restarting_at_zero_reach_the_pump_without_a_backward_jump_and_keep_the_gap() {
    let rig = Rig::started(IdleMode::None).await;
    let mut p = rig.producer(1, 0);
    p.run(Duration::from_secs(10)).await;
    drop(p);
    let last = rig.collector.frames().iter().map(Received::pts).max();
    let last = last.unwrap();
    sleep(Duration::from_secs(3)).await;
    let mut p = rig.producer(2, 0);
    p.run(Duration::from_secs(1)).await;
    let frames = rig.collector.frames();
    strictly_increasing(&frames);
    let resumed = frames
        .iter()
        .find(|f| f.marker().is_some_and(|m| m.attachment == 2))
        .unwrap();
    let gap = resumed.pts() - last;
    assert!((gap - 3_000_000).abs() <= FRAME_US, "gap {gap} µs");
}

async fn reattach_scenario(idle: IdleMode, gap: Duration, skews: [i64; 4]) -> Vec<Received> {
    let rig = Rig::started(idle).await;
    let starts = [0, 0, 1_000_000_000_000, -50_000];
    for (i, start) in starts.into_iter().enumerate() {
        let mut p = rig.producer(i as u8 + 1, start).with_audio_skew(skews[i]);
        p.run(Duration::from_millis(2_500)).await;
        drop(p);
        if i + 1 < starts.len() {
            sleep(gap).await;
        }
    }
    assert_eq!(rig.closed(), None, "{idle:?} {gap:?}");
    rig.collector.frames()
}

const GAPS: [Duration; 3] = [
    Duration::from_millis(100),
    Duration::from_secs(5),
    Duration::from_millis(29_999),
];

#[tokio::test(start_paused = true)]
async fn outgoing_pts_follow_the_wall_clock_across_reattaches_in_every_idle_mode() {
    for idle in IDLE_MODES {
        for gap in GAPS {
            let frames = reattach_scenario(idle, gap, [0; 4]).await;
            strictly_increasing(&frames);
            on_wall_clock(&frames, 0);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn video_and_audio_keep_their_offset_across_reattaches_in_every_idle_mode() {
    for idle in IDLE_MODES {
        for gap in GAPS {
            av_aligned(&reattach_scenario(idle, gap, [0, 100_000, 0, 50_000]).await);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_producer_attached_before_start_delivers_nothing_until_start_then_a_keyframe() {
    let mut rig = Rig::new(IdleMode::Documented);
    let mut p = rig.producer(1, 0).starting_mid_gop();
    p.run(Duration::from_millis(500)).await;
    assert_eq!(p.attachment.stats().skipped, 15 + 25);
    rig.start().await;
    let started = rig.now_us();
    p.run(Duration::from_secs(3)).await;
    let frames = rig.collector.frames();
    assert!(frames.iter().all(|f| f.sent_us() >= started));
    let first_video = frames.iter().find(|f| f.track() == VIDEO).unwrap();
    assert!(first_video.keyframe());
    assert_eq!(first_video.marker().unwrap().pts_in, 2_000_000);
    strictly_increasing(&frames);
}

#[tokio::test(start_paused = true)]
async fn stop_and_start_keep_the_attachment_and_continue_pts() {
    let mut rig = Rig::started(IdleMode::Documented).await;
    let mut p = rig.producer(1, 0);
    p.run(Duration::from_secs(1)).await;
    let first = rig.receiver.clone();
    let requests = *p.attachment.keyframe_requests().borrow();
    rig.restart().await;
    settle().await;
    assert!(first.lock().unwrap().closed_at.is_some());
    assert!(*p.attachment.keyframe_requests().borrow() > requests);
    let restarted = rig.now_us();
    p.run(Duration::from_secs(3)).await;
    assert_eq!(
        *rig.handle.state().borrow(),
        SourceState::Producing { attachment: 1 }
    );
    let after = rig.receiver.lock().unwrap().frames.clone();
    assert!(after.iter().all(|f| f.sent_us() >= restarted));
    assert!(
        after
            .iter()
            .find(|f| f.track() == VIDEO)
            .unwrap()
            .keyframe()
    );
    let frames = rig.collector.frames();
    strictly_increasing(&frames);
    on_wall_clock(&frames, 0);
}

#[tokio::test(start_paused = true)]
async fn a_silent_producer_is_stalled_then_producing_again_from_a_keyframe() {
    let rig = Rig::started(IdleMode::Documented).await;
    let mut state = rig.handle.state();
    let mut p = rig.producer(1, 0);
    p.run(Duration::from_millis(500)).await;
    let last = p.last_sent.get().unwrap();
    sleep_until(last + STALL - MS).await;
    assert_eq!(*state.borrow(), SourceState::Producing { attachment: 1 });
    sleep(MS).await;
    settle().await;
    assert_eq!(
        *state.borrow_and_update(),
        SourceState::Interrupted {
            since: last,
            reason: InterruptReason::Stalled
        }
    );
    let requests = *p.attachment.keyframe_requests().borrow();
    let resumed = rig.now_us();
    p.run(Duration::from_secs(3)).await;
    assert_eq!(*state.borrow(), SourceState::Producing { attachment: 1 });
    assert!(*p.attachment.keyframe_requests().borrow() > requests);
    let frames = rig.collector.frames();
    let first_video = frames
        .iter()
        .find(|f| f.track() == VIDEO && f.sent_us() >= resumed)
        .unwrap();
    assert!(first_video.keyframe());
    strictly_increasing(&frames);
    on_wall_clock(&frames, 0);
}

#[tokio::test(start_paused = true)]
async fn a_producer_stalled_past_grace_is_closed_and_the_receiver_closes() {
    let rig = Rig::started(IdleMode::Documented).await;
    let mut p = rig.producer(1, 0);
    p.run(Duration::from_millis(500)).await;
    let last = us(p.last_sent.get().unwrap() - rig.epoch);
    sleep(GRACE).await;
    settle().await;
    assert_eq!(rig.closed(), Some(last + us(GRACE)));
    assert_eq!(
        p.attachment.send(audio(0)),
        Err(SendError::Closed(CloseReason::GraceExpired))
    );
}

#[tokio::test(start_paused = true)]
async fn video_waits_for_a_keyframe_after_attach_while_audio_flows() {
    let rig = Rig::started(IdleMode::Documented).await;
    let mut p = rig.producer(1, 0).starting_mid_gop();
    p.run(Duration::from_millis(1_500)).await;
    let frames = rig.collector.frames();
    assert!(frames.iter().all(|f| f.track() == AUDIO));
    assert_eq!(frames.len(), 75);
    assert_eq!(p.attachment.stats().skipped, 45);
    p.run(Duration::from_secs(1)).await;
    let frames = rig.collector.frames();
    let first_video = frames.iter().find(|f| f.track() == VIDEO).unwrap();
    assert!(first_video.keyframe());
    assert_eq!(first_video.marker().unwrap().pts_in, 2_000_000);
}

#[tokio::test(start_paused = true)]
async fn keyframe_requests_reach_the_attachment_or_are_latched_for_the_next() {
    let rig = Rig::started(IdleMode::Documented).await;
    let first = rig.handle.attach(info()).unwrap();
    assert_eq!(*first.keyframe_requests().borrow(), 1);
    rig.source.request_keyframe();
    assert_eq!(*first.keyframe_requests().borrow(), 2);
    drop(first);
    rig.source.request_keyframe();
    rig.source.request_keyframe();
    let second = rig.handle.attach(info()).unwrap();
    let mut requests = second.keyframe_requests();
    assert!(requests.has_changed().unwrap());
    assert_eq!(*requests.borrow_and_update(), 2);
    drop(second);
    let third = rig.handle.attach(info()).unwrap();
    assert_eq!(*third.keyframe_requests().borrow(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_full_queue_stays_bounded_and_counts_every_loss_exactly() {
    let mut source = ExternalSource::new(options(IdleMode::None));
    let handle = source.handle();
    let mut rx = source.start(CaptureConfig::default()).await.unwrap();
    let a = handle.attach(info()).unwrap();
    let requests = *a.keyframe_requests().borrow();
    let mut sent = 0u64;
    for i in 0..1_000i64 {
        a.send(video(i * FRAME_US, i % 50 == 10)).unwrap();
        a.send(audio(i * FRAME_US)).unwrap();
        a.send(audio(i * FRAME_US + AUDIO_US)).unwrap();
        sent += 3;
        assert!(source.queued() <= QUEUE_CAPACITY);
    }
    let stats = a.stats();
    assert_eq!(stats.skipped, 10);
    assert!(
        stats.dropped_video > 0 && stats.dropped_audio > 0,
        "{stats:?}"
    );
    assert!(*a.keyframe_requests().borrow() > requests);
    let queued = source.queued() as u64;
    let delivered = std::iter::from_fn(|| rx.try_recv().ok()).count() as u64;
    let lost = stats.skipped + stats.dropped_video + stats.dropped_audio;
    assert_eq!(sent, queued + delivered + lost);
    source.stop().await.unwrap();
}

fn delivered(rx: &mut FrameReceiver) -> u64 {
    std::iter::from_fn(|| rx.try_recv().ok()).count() as u64
}

fn lost(stats: AttachmentStats) -> u64 {
    stats.skipped + stats.dropped_video + stats.dropped_audio
}

fn burst(a: &Attachment, from: i64, n: i64) -> u64 {
    for i in from..from + n {
        a.send(video(i * FRAME_US, i % 50 == 0)).unwrap();
        a.send(audio(i * FRAME_US)).unwrap();
    }
    2 * n as u64
}

#[tokio::test(start_paused = true)]
async fn frames_cleared_by_stop_and_restart_are_counted() {
    let mut source = ExternalSource::new(options(IdleMode::None));
    let handle = source.handle();
    let mut rx = source.start(CaptureConfig::default()).await.unwrap();
    let a = handle.attach(info()).unwrap();
    let mut sent = burst(&a, 0, 40);
    let mut got = delivered(&mut rx);
    source.stop().await.unwrap();
    got += delivered(&mut rx);
    assert_eq!(source.queued(), 0);
    assert_eq!(sent, got + lost(a.stats()), "{:?}", a.stats());
    let mut rx = source.start(CaptureConfig::default()).await.unwrap();
    sent += burst(&a, 40, 100);
    got += delivered(&mut rx);
    source.stop().await.unwrap();
    got += delivered(&mut rx);
    assert_eq!(sent, got + lost(a.stats()), "{:?}", a.stats());
}

#[tokio::test(start_paused = true)]
async fn frames_cleared_when_grace_expires_are_counted() {
    let mut source = ExternalSource::new(options(IdleMode::Continuous));
    let handle = source.handle();
    let mut rx = source.start(CaptureConfig::default()).await.unwrap();
    let a = handle.attach(info()).unwrap();
    let sent = burst(&a, 0, 100);
    sleep(GRACE + MS).await;
    let mut got = 0;
    while let Some(frame) = rx.recv().await {
        if !matches!(&frame, MediaFrame::Audio(f) if f.data == OPUS_SILENCE_FRAME) {
            got += 1;
        }
    }
    assert!(lost(a.stats()) > 0, "{:?}", a.stats());
    assert_eq!(sent, got + lost(a.stats()), "{:?}", a.stats());
    assert_eq!(
        a.send(audio(0)),
        Err(SendError::Closed(CloseReason::GraceExpired))
    );
}

#[tokio::test(start_paused = true)]
async fn losses_are_charged_to_the_attachment_that_sent_the_frame() {
    let mut source = ExternalSource::new(options(IdleMode::None));
    let handle = source.handle();
    let mut rx = source.start(CaptureConfig::default()).await.unwrap();
    let first = handle.attach(info()).unwrap();
    let half = QUEUE_CAPACITY as i64 / 2;
    let first_sent = burst(&first, 0, half);
    assert_eq!(source.queued(), QUEUE_CAPACITY - 1);
    let first_link = first.link.clone();
    first.close(CloseReason::Ended);
    let second = handle.attach(info()).unwrap();
    second.send(video(0, true)).unwrap();
    for i in 0..10 {
        second.send(audio(i * AUDIO_US)).unwrap();
    }
    let first_stats = first_link.snapshot();
    assert_eq!(first_stats.dropped_audio, 10, "{first_stats:?}");
    assert_eq!(first_stats.dropped_video, 0);
    assert_eq!(second.stats(), AttachmentStats::default());
    let got = delivered(&mut rx) + source.queued() as u64;
    assert_eq!(
        first_sent + 11,
        got + lost(first_stats) + lost(second.stats())
    );
}

#[tokio::test(start_paused = true)]
async fn the_offset_uses_the_earlier_first_pts_of_both_tracks() {
    let rig = Rig::started(IdleMode::None).await;
    let a = rig.handle.attach(info()).unwrap();
    a.send(video(0, true)).unwrap();
    a.send(audio(-21_333)).unwrap();
    a.send(audio(-1_333)).unwrap();
    a.send(audio(18_667)).unwrap();
    settle().await;
    let pts: Vec<(usize, i64)> = rig
        .collector
        .frames()
        .iter()
        .map(|f| (f.track(), f.pts()))
        .collect();
    assert_eq!(
        pts,
        [
            (VIDEO, 21_333),
            (AUDIO, 0),
            (AUDIO, 20_000),
            (AUDIO, 40_000)
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_single_track_producer_starts_after_the_first_frames_wait() {
    let rig = Rig::started(IdleMode::None).await;
    let a = rig.handle.attach(info()).unwrap();
    a.send(audio(5_000)).unwrap();
    sleep(FIRST_FRAMES_WAIT - MS).await;
    assert!(rig.collector.frames().is_empty());
    sleep(2 * MS).await;
    let frames = rig.collector.frames();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].pts(), 0);
    a.send(audio(25_000)).unwrap();
    settle().await;
    assert_eq!(rig.collector.frames()[1].pts(), 20_000);
}

#[tokio::test(start_paused = true)]
async fn audio_lagging_video_on_reattach_keeps_its_spacing_in_continuous_mode() {
    let rig = Rig::started(IdleMode::Continuous).await;
    let mut p = rig.producer(1, 0);
    p.run(Duration::from_secs(1)).await;
    drop(p);
    sleep(Duration::from_millis(500)).await;
    let mut p = rig.producer(2, 0).with_audio_skew(-80_000);
    p.run(Duration::from_secs(1)).await;
    let audio: Vec<i64> = rig
        .collector
        .frames()
        .iter()
        .filter(|f| f.track() == AUDIO && f.marker().is_some_and(|m| m.attachment == 2))
        .map(Received::pts)
        .collect();
    for w in audio.windows(2) {
        assert!(
            w[1] - w[0] >= AUDIO_US - TICK_US[AUDIO],
            "audio pts bunched: {w:?}"
        );
    }
    av_aligned(&rig.collector.frames());
}

#[tokio::test(start_paused = true)]
async fn a_constant_av_skew_does_not_accumulate_drift_over_reattaches() {
    let rig = Rig::started(IdleMode::Continuous).await;
    for tag in 1..=6 {
        let mut p = rig.producer(tag, 0).with_audio_skew(-21_333);
        p.run(Duration::from_secs(1)).await;
        drop(p);
        sleep(Duration::from_secs(1)).await;
    }
    let frames = rig.collector.frames();
    strictly_increasing(&frames);
    av_aligned(&frames);
    on_wall_clock(&frames, 0);
}

#[tokio::test(start_paused = true)]
async fn a_lagging_audio_track_then_an_unskewed_producer_stays_aligned() {
    let rig = Rig::started(IdleMode::Continuous).await;
    let mut p = rig.producer(1, 0).with_audio_skew(-51_655);
    p.run(Duration::from_millis(201)).await;
    drop(p);
    let mut p = rig.producer(2, 0);
    p.run(Duration::from_millis(500)).await;
    let frames = rig.collector.frames();
    strictly_increasing(&frames);
    av_aligned(&frames);
}

fn tracks(lost: &[Queued]) -> (usize, usize) {
    let audio = lost.iter().filter(|q| q.is_audio()).count();
    (lost.len() - audio, audio)
}

#[test]
fn queue_overflow_drops_video_to_the_next_keyframe_and_the_oldest_audio() {
    let link = Arc::new(Link::default());
    let item = |frame: MediaFrame| Queued {
        frame,
        owner: Some(link.clone()),
    };
    let v = |pts: u64, keyframe: bool| {
        MediaFrame::Video(EncodedVideoFrame {
            codec: VideoCodec::H264,
            data: vec![],
            pts_us: pts,
            keyframe,
        })
    };
    let a = |pts: u64| {
        MediaFrame::Audio(AudioFrame {
            data: vec![],
            pts_us: pts,
        })
    };
    let mut queue = VecDeque::new();
    for i in 0..QUEUE_CAPACITY as u64 {
        let frame = if i % 2 == 0 { v(i, i == 0) } else { a(i) };
        assert!(enqueue(&mut queue, item(frame)).is_empty());
    }
    let lost = enqueue(&mut queue, item(a(1_000)));
    assert_eq!(tracks(&lost), (0, 1));
    assert_eq!(lost[0].frame, a(1));
    assert_eq!(queue.back().map(|q| &q.frame), Some(&a(1_000)));
    let videos = queue.iter().filter(|q| !q.is_audio()).count();
    let lost = enqueue(&mut queue, item(v(2_000, false)));
    assert_eq!(tracks(&lost), (videos + 1, 0));
    assert!(queue.iter().all(Queued::is_audio));
    let mut n = 3_000;
    while queue.len() < QUEUE_CAPACITY {
        queue.push_back(item(a(n)));
        n += 1;
    }
    let oldest = queue[0].frame.clone();
    let lost = enqueue(&mut queue, item(v(4_000, true)));
    assert_eq!(tracks(&lost), (0, 1));
    assert_eq!(lost[0].frame, oldest);
    assert_eq!(queue.back().map(|q| &q.frame), Some(&v(4_000, true)));
    assert_eq!(queue.len(), QUEUE_CAPACITY);
}

#[test]
fn an_idle_frame_on_a_full_queue_never_displaces_video() {
    let link = Arc::new(Link::default());
    let video = |pts: u64| Queued {
        frame: MediaFrame::Video(EncodedVideoFrame {
            codec: VideoCodec::H264,
            data: vec![],
            pts_us: pts,
            keyframe: pts == 0,
        }),
        owner: Some(link.clone()),
    };
    let silence = || Queued {
        frame: MediaFrame::Audio(AudioFrame {
            data: OPUS_SILENCE_FRAME.to_vec(),
            pts_us: 0,
        }),
        owner: None,
    };
    let mut queue: VecDeque<Queued> = (0..QUEUE_CAPACITY as u64).map(video).collect();
    let lost = enqueue(&mut queue, silence());
    assert_eq!(lost.len(), 1);
    assert!(lost[0].owner.is_none());
    assert!(queue.iter().all(|q| !q.is_audio()));
    assert_eq!(queue.len(), QUEUE_CAPACITY);
}

#[tokio::test(start_paused = true)]
async fn idle_output_over_one_second_matches_each_mode() {
    for (idle, expected) in [
        (IdleMode::Documented, 5),
        (IdleMode::Continuous, 50),
        (IdleMode::None, 0),
    ] {
        let rig = Rig::started(idle).await;
        let mut p = rig.producer(1, 0);
        p.run(Duration::from_millis(500)).await;
        drop(p);
        let detached = rig.now_us();
        sleep(Duration::from_secs(1)).await;
        settle().await;
        let frames = rig.collector.frames();
        let idle_frames: Vec<&Received> = frames.iter().filter(|f| f.marker().is_none()).collect();
        assert_eq!(idle_frames.len(), expected, "{idle:?}");
        for (i, f) in idle_frames.iter().enumerate() {
            assert_eq!(f.data(), [0xF8, 0xFF, 0xFE]);
            assert_eq!(f.track(), AUDIO);
            assert_eq!(f.at_us, detached + (i as i64 + 1) * AUDIO_US);
        }
        strictly_increasing(&frames);
        on_wall_clock(&frames, 0);
    }
}

#[tokio::test(start_paused = true)]
async fn lists_one_external_target() {
    let source = ExternalSource::new(options(IdleMode::None));
    let targets = source.list_targets().await.unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].id, EXTERNAL_TARGET_ID);
}

#[derive(Debug, Clone)]
enum Op {
    Attach {
        start: i64,
        skew: i64,
        mid_gop: bool,
    },
    Detach,
    Produce(u64),
    Wait(u64),
    Restart,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (
            prop_oneof![
                Just(0i64),
                Just(1_000_000_000_000i64),
                Just(-50_000i64),
                -10_000_000i64..10_000_000,
            ],
            -200_000i64..200_000,
            any::<bool>(),
        )
            .prop_map(|(start, skew, mid_gop)| Op::Attach { start, skew, mid_gop }),
        2 => Just(Op::Detach),
        4 => (20u64..4_000).prop_map(Op::Produce),
        2 => prop_oneof![1u64..3_000, 25_000u64..35_000].prop_map(Op::Wait),
        1 => Just(Op::Restart),
    ]
}

/// The grace rule: media absent since the detach, or since the last frame once stalled.
struct Model {
    started: Instant,
    last_activity: Option<Instant>,
    detached_at: Instant,
    expired: bool,
}

impl Model {
    fn stalled(&self, now: Instant) -> bool {
        self.last_activity
            .is_some_and(|t| now >= t.max(self.started) + STALL)
    }

    fn absent_since(&self, now: Instant) -> Option<Instant> {
        match self.last_activity {
            Some(t) if self.stalled(now) => Some(t.max(self.started)),
            Some(_) => None,
            None => Some(self.detached_at.max(self.started)),
        }
    }

    fn update(&mut self, now: Instant) {
        if self.absent_since(now).is_some_and(|t| now >= t + GRACE) {
            self.expired = true;
        }
    }
}

async fn run_ops(ops: Vec<Op>) {
    let mut rig = Rig::started(IdleMode::Continuous).await;
    let mut model = Model {
        started: Instant::now(),
        last_activity: None,
        detached_at: Instant::now(),
        expired: false,
    };
    let mut producer: Option<Producer> = None;
    let mut tag = 0u8;
    let mut restarts: Vec<i64> = Vec::new();
    for op in ops {
        model.update(Instant::now());
        match op {
            Op::Attach {
                start,
                skew,
                mid_gop,
            } => match rig.handle.attach(info()) {
                Ok(attachment) => {
                    assert!(!model.expired && producer.is_none());
                    tag += 1;
                    let p = Producer::new(attachment, tag, rig.epoch, start).with_audio_skew(skew);
                    producer = Some(if mid_gop { p.starting_mid_gop() } else { p });
                    model.last_activity = Some(Instant::now());
                }
                Err(AttachError::Busy) => assert!(!model.expired && producer.is_some()),
                Err(AttachError::NoSession) => {
                    assert!(model.expired);
                    producer = None;
                }
                Err(e) => panic!("{e}"),
            },
            Op::Detach => {
                if producer.take().is_some() && !model.expired {
                    let now = Instant::now();
                    let last = model.last_activity.take().unwrap();
                    model.detached_at = if model.stalled_at(last, now) {
                        last
                    } else {
                        now
                    };
                }
            }
            Op::Produce(ms) => {
                let p = producer.as_mut().filter(|_| !model.expired);
                match p {
                    Some(p) => {
                        let deadline = model.absent_since(Instant::now()).map(|t| t + GRACE);
                        p.run(Duration::from_millis(ms)).await;
                        if let Some(refused) = p.refused_at.get() {
                            assert!(
                                deadline.is_some_and(|d| refused >= d),
                                "refused at {refused:?}, grace ends {deadline:?}"
                            );
                            model.expired = true;
                        } else if let Some(t) = p.last_sent.get() {
                            model.last_activity = model.last_activity.map(|l| l.max(t));
                        }
                    }
                    None => sleep(Duration::from_millis(ms)).await,
                }
            }
            Op::Wait(ms) => sleep(Duration::from_millis(ms)).await,
            Op::Restart => {
                if !model.expired {
                    rig.restart().await;
                    restarts.push(rig.now_us());
                }
            }
        }
    }
    settle().await;
    model.update(Instant::now());
    assert_eq!(
        rig.closed().is_some(),
        model.expired,
        "closed at {:?}",
        rig.closed()
    );

    let frames = rig.collector.frames();
    strictly_increasing(&frames);
    av_aligned(&frames);
    let mut shift: Option<(u8, i64)> = None;
    for f in frames.iter().filter(|f| f.marker().is_some()) {
        let m = f.marker().unwrap();
        let d = f.pts() - f.wall_us();
        match shift {
            Some((tag, base)) if tag == m.attachment => assert!(
                (d - base).abs() <= FRAME_US,
                "attachment {tag}: pts drift {} µs from the wall clock",
                d - base
            ),
            Some((_, base)) => assert!(
                (d - base).abs() <= 2 * 200_000 + FRAME_US,
                "reattach shifted the timeline by {} µs",
                d - base
            ),
            None => {}
        }
        if shift.is_none_or(|(tag, _)| tag != m.attachment) {
            shift = Some((m.attachment, d));
        }
    }
    let mut previous: Option<Marker> = None;
    let mut rearmed = true;
    for f in &frames {
        let Some(m) = f.marker() else { continue };
        let restarted =
            previous.is_some_and(|p| restarts.iter().any(|&r| p.sent_us < r && r <= m.sent_us));
        if restarted
            || previous
                .is_none_or(|p| p.attachment != m.attachment || m.sent_us - p.sent_us >= us(STALL))
        {
            rearmed = true;
        }
        previous = Some(m);
        if f.track() == VIDEO {
            assert!(
                !rearmed || f.keyframe(),
                "video at {} µs before a keyframe",
                m.sent_us
            );
            rearmed = false;
        }
    }
}

impl Model {
    fn stalled_at(&self, last: Instant, now: Instant) -> bool {
        now >= last.max(self.started) + STALL
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    #[test]
    fn invariants_hold_for_random_attach_detach_stall_restart_sequences(ops in prop::collection::vec(op(), 1..14)) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        runtime.block_on(run_ops(ops));
    }
}
