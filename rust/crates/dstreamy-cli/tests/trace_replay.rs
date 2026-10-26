//! `serve` + mock Discord + `mock-producer --replay`: real OBS traces converted by `dstreamy-obs-packet`.
#![cfg(unix)]

mod support;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use dstreamy_media::annexb;
use dstreamy_mock_discord::{
    FIRST_FRAME_DAVE_DROP, Recorded, RecordedAudioFrame, RecordedVideoFrame, VoiceKind,
};
use dstreamy_test_producer::sync::{
    self, H264LumaDecoder, LumaFrame, MarkerPair, OpusMonoDecoder, SAMPLE_RATE, TONE_HZ,
};
use dstreamy_test_producer::trace::{Converted, Trace};
use serde_json::Value;
use support::*;

const VIDEO_HZ: i64 = 90_000;
const AUDIO_HZ: i64 = 48_000;
const PACING_SLACK_US: i64 = 20_000;
const JITTER_MARGIN_US: i64 = 10_000;
const STALL_TICK: Duration = Duration::from_millis(2);
const STALL_MIN: Duration = Duration::from_millis(10);
// The A/V median needs 10 unique payloads; the restart trace repeats frames 9 and 11 later, so 15 frames leave 13.
const MIN_BASELINE_FRAMES: usize = 15;
// GitHub's macOS runners have too few cores to pace six replays, their serves and producers at once.
const CONCURRENT_TRACES: usize = if cfg!(target_os = "macos") {
    2
} else {
    usize::MAX
};

/// On macOS, held for the whole of each pace-checked replay test, so those tests never overlap.
fn exclusive_on_macos() -> Option<MutexGuard<'static, ()>> {
    static PACED: Mutex<()> = Mutex::new(());
    cfg!(target_os = "macos").then(|| PACED.lock().unwrap_or_else(PoisonError::into_inner))
}

fn traces_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../obs/tests/traces")
}

fn uses_b_frames(name: &str) -> bool {
    name.contains("-bf2-") || name.contains("-bfunset-")
}

/// Every committed trace, read at test time so a new trace directory is covered without a code change.
fn traces() -> Vec<(String, PathBuf, Trace)> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(traces_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs.into_iter()
        .map(|d| {
            let name = d.file_name().unwrap().to_string_lossy().into_owned();
            let trace = Trace::load(&d).unwrap();
            (name, d, trace)
        })
        .collect()
}

fn positive(name: &str, trace: &Trace) -> bool {
    !uses_b_frames(name) && trace.audio_codec() == "opus"
}

/// One frame the producer sends, as `dstreamy-obs-packet` converts it.
struct Sent {
    key: Vec<u8>,
    pts_us: i64,
    dts_usec: i64,
}

struct Expected {
    fps: i64,
    video: Vec<Sent>,
    audio: Vec<Sent>,
}

/// The transport rewrites the SPS and start codes, so video frames are matched on their slice NAL units.
fn slices(au: &[u8]) -> Vec<u8> {
    annexb::nal_units(au)
        .filter(|n| matches!(annexb::h264_nal_type(n), Some(1 | 5)))
        .flatten()
        .copied()
        .collect()
}

fn expected(trace: &Trace) -> Expected {
    let (mut video, mut audio) = (Vec::new(), Vec::new());
    for r in trace.clone().replay() {
        let r = r.expect("positive trace converts");
        match r.frame {
            Converted::Video { data, pts_us, .. } => video.push(Sent {
                key: slices(&data),
                pts_us,
                dts_usec: r.dts_usec,
            }),
            Converted::Audio { data, pts_us } => audio.push(Sent {
                key: data,
                pts_us,
                dts_usec: r.dts_usec,
            }),
        }
    }
    Expected {
        fps: i64::from(trace.fps().unwrap()),
        video,
        audio,
    }
}

fn rtp_us(rtp: u32, hz: i64) -> i64 {
    i64::from(rtp) * 1_000_000 / hz
}

/// Recorded frames paired with what was sent; payloads sent more than once are left out.
fn matched<'a, F>(
    recorded: &[&'a F],
    key: impl Fn(&F) -> Vec<u8>,
    sent: &'a [Sent],
) -> Vec<(&'a F, &'a Sent)> {
    let mut by_key: HashMap<&[u8], Option<&Sent>> = HashMap::new();
    for s in sent {
        by_key
            .entry(&s.key)
            .and_modify(|v| *v = None)
            .or_insert(Some(s));
    }
    recorded
        .iter()
        .filter_map(|f| Some((*f, (*by_key.get(key(f).as_slice())?)?)))
        .collect()
}

fn median(mut v: Vec<i64>) -> i64 {
    assert!(
        v.len() >= 10,
        "only {} frames matched what was sent",
        v.len()
    );
    v.sort_unstable();
    v[v.len() / 2]
}

/// Output pts minus input pts per track: equal on both tracks when `serve` kept A/V alignment.
fn shifts(
    video: &[&RecordedVideoFrame],
    audio: &[&RecordedAudioFrame],
    want: &Expected,
) -> (i64, i64) {
    let v = matched(video, |f| slices(&f.data), &want.video);
    let a = matched(audio, |f| f.data.clone(), &want.audio);
    (
        median(
            v.iter()
                .map(|(f, s)| rtp_us(f.rtp_timestamp, VIDEO_HZ) - s.pts_us)
                .collect(),
        ),
        median(
            a.iter()
                .map(|(f, s)| rtp_us(f.rtp_timestamp, AUDIO_HZ) - s.pts_us)
                .collect(),
        ),
    )
}

/// Catches `serve` moving one track against the other; a skew in the conversion itself only the markers can see.
fn assert_av_aligned(
    video: &[&RecordedVideoFrame],
    audio: &[&RecordedAudioFrame],
    want: &Expected,
    what: &str,
) {
    let (v, a) = shifts(video, audio, want);
    let frame_us = 1_000_000 / want.fps;
    assert!(
        (a - v).abs() <= frame_us,
        "{what}: serve moved audio by {a} µs and video by {v} µs; one frame is {frame_us} µs"
    );
}

type Stall = (Instant, Instant);

/// Spans in which a `STALL_TICK` timer task on the mock's runtime, whose workers stamp `received_at`, woke over `STALL_MIN` late.
struct StallCanary(Arc<Mutex<Vec<Stall>>>);

impl StallCanary {
    fn start(rt: &tokio::runtime::Runtime) -> Self {
        let spans = Arc::new(Mutex::new(Vec::new()));
        let sink = spans.clone();
        rt.spawn(async move {
            loop {
                let before = Instant::now();
                tokio::time::sleep(STALL_TICK).await;
                let after = Instant::now();
                if after - before > STALL_TICK + STALL_MIN {
                    sink.lock().unwrap().push((before, after));
                }
            }
        });
        Self(spans)
    }

    fn stalls(&self) -> Vec<Stall> {
        self.0.lock().unwrap().clone()
    }
}

/// Arrival at the mock follows the trace's libobs dts, except during or just after a stall of the mock's runtime.
fn assert_paced<F>(
    recorded: &[&F],
    key: impl Fn(&F) -> Vec<u8>,
    at: impl Fn(&F) -> Instant,
    sent: &[Sent],
    frame_us: i64,
    stalls: &[Stall],
    what: &str,
) {
    let pairs = matched(recorded, key, sent);
    let (f0, s0) = pairs[0];
    let slack = PACING_SLACK_US.max(frame_us);
    let (first, last) = (at(f0), at(pairs[pairs.len() - 1].0));
    let clipped = |(from, to): Stall| (from.max(first), to.min(last));
    let window = |(from, to): Stall| (from, to + (to - from) + Duration::from_micros(slack as u64));
    let stalls: Vec<Stall> = stalls
        .iter()
        .copied()
        .filter(|&s| window(s).1 >= first && s.0 <= last)
        .collect();
    let stalled: Duration = stalls
        .iter()
        .map(|&s| {
            let (from, to) = clipped(s);
            to.saturating_duration_since(from)
        })
        .sum();
    let longest = stalls
        .iter()
        .map(|&(from, to)| to - from)
        .max()
        .unwrap_or_default();
    let mut windows: Vec<Stall> = stalls.iter().copied().map(window).collect();
    windows.sort();
    let mut excused = Duration::ZERO;
    let mut covered_to = first;
    for &w in &windows {
        let (from, to) = clipped(w);
        excused += to.saturating_duration_since(from.max(covered_to));
        covered_to = covered_to.max(to);
    }
    let after_stall = |t: Instant| windows.iter().any(|&(from, to)| t >= from && t <= to);
    // The latest window end and the longest stall among the windows `t` falls in.
    let stall_at = |t: Instant| {
        stalls
            .iter()
            .filter(|&&s| {
                let (from, to) = window(s);
                t >= from && t <= to
            })
            .map(|&(from, to)| (window((from, to)).1, to - from))
            .reduce(|a, b| (a.0.max(b.0), a.1.max(b.1)))
    };
    let lag = |(f, s): &(&F, &Sent)| {
        let arrived = at(f).duration_since(at(f0)).as_micros() as i64;
        arrived - (s.dts_usec - s0.dts_usec)
    };
    let unexcused: Vec<_> = pairs.iter().filter(|(f, _)| !after_stall(at(f))).collect();
    let mid = median(unexcused.iter().map(|p| lag(p)).collect());
    // A backlog can take longer than the stall to drain, so a draining tail within the stall's length after its window goes with it.
    let mut checked = Vec::new();
    let mut tail: Option<(Instant, Duration, i64)> = None;
    let mut previous = first;
    for p in &pairs {
        let t = at(p.0);
        let deviation = lag(p) - mid;
        if let Some((end, len)) = stall_at(t) {
            tail = Some((end, len, len.as_micros() as i64 - JITTER_MARGIN_US));
        } else if let Some((end, len, before)) = tail
            && deviation > slack
            && deviation <= before + JITTER_MARGIN_US
            && t <= end + len
        {
            excused += t.saturating_duration_since(previous);
            tail = Some((end, len, deviation));
        } else {
            tail = None;
            checked.push(p);
        }
        previous = t;
    }
    let span = last - first;
    eprintln!(
        "{what}: pace checked on {} of {} frames, {} left out after {} stalls of {stalled:?} in all, longest {longest:?}, excusing {excused:?} of {span:?}",
        checked.len(),
        pairs.len(),
        pairs.len() - checked.len(),
        stalls.len()
    );
    // A hot loop in serve or the producer also starves the runtime, so it may not hide behind its own stalls.
    assert!(
        excused * 4 <= span,
        "{what}: stalls of the mock's runtime would excuse {excused:?} of the {span:?} trace, over a quarter: {stalls:?}"
    );
    assert!(
        checked.len() * 2 >= pairs.len(),
        "{what}: only {} of {} frames arrived outside a stall of the mock's runtime or its catch-up: {stalls:?}",
        checked.len(),
        pairs.len()
    );
    let off: Vec<i64> = checked
        .iter()
        // The reference frame's lag is 0 by construction, so a late reference would count as its own outlier.
        .filter(|(f, _)| !std::ptr::eq(*f, f0))
        .map(|p| lag(p))
        .filter(|l| (l - mid).abs() > slack)
        .collect();
    let largest = checked
        .iter()
        .filter(|(f, _)| !std::ptr::eq(*f, f0))
        .map(|p| (lag(p) - mid).abs())
        .max()
        .unwrap_or(0);
    eprintln!("{what}: largest deviation from the median lag {largest} µs, slack {slack} µs");
    // Below 50 checked frames the 2% budget allows none, so one frame of scheduling noise past the slack would fail.
    let one_jitter = off.len() == 1 && (off[0] - mid).abs() <= slack + JITTER_MARGIN_US;
    let deviations: Vec<i64> = off.iter().map(|l| l - mid).collect();
    assert!(
        off.len() * 50 <= checked.len() || one_jitter,
        "{what}: {} of {} checked frames off the trace's dts pace by more than {slack} µs from the median lag {mid} µs: lags {off:?}, deviations {deviations:?}",
        off.len(),
        checked.len()
    );
}

fn luma_frames(video: &[&RecordedVideoFrame]) -> Vec<LumaFrame> {
    let mut decoder = H264LumaDecoder::new().unwrap();
    let mut frames = Vec::new();
    for (i, v) in video.iter().enumerate() {
        frames.extend(
            decoder
                .decode(&v.data, rtp_us(v.rtp_timestamp, VIDEO_HZ))
                .unwrap_or_else(|e| panic!("video {i} does not decode: {e}")),
        );
    }
    frames.extend(decoder.finish().unwrap());
    frames
}

fn pcm(audio: &[&RecordedAudioFrame]) -> Vec<f32> {
    let mut opus = OpusMonoDecoder::new().unwrap();
    let mut pcm = Vec::new();
    for (i, a) in audio.iter().enumerate() {
        let samples = opus
            .decode(&a.data)
            .unwrap_or_else(|e| panic!("audio {i} does not decode: {e}"));
        assert_eq!(samples.len(), 960, "audio {i}: 20 ms");
        pcm.extend(samples);
    }
    pcm
}

fn consecutive(name: &str, ts: &[u32], step: u32) {
    for (i, w) in ts.windows(2).enumerate() {
        assert_eq!(
            w[1].wrapping_sub(w[0]),
            step,
            "{name} RTP step at {i}: {} then {}",
            w[0],
            w[1]
        );
    }
}

fn tone_is_440(pcm: &[f32], beeps: &[i64], start_us: i64, what: &str) {
    let quiet = sync::quiet_spans(pcm, start_us, beeps, SAMPLE_RATE);
    let longest = quiet
        .iter()
        .max_by_key(|s| s.len())
        .expect("audio outside the beeps");
    let tone = sync::goertzel_amplitude(longest, TONE_HZ, SAMPLE_RATE);
    let off: Vec<f32> = [220.0, 330.0, 660.0, 880.0]
        .iter()
        .map(|&f| sync::goertzel_amplitude(longest, f, SAMPLE_RATE))
        .collect();
    assert!(
        tone > 0.02 && off.iter().all(|&o| o < tone / 4.0),
        "{what}: 440 Hz amplitude {tone}, off-frequency {off:?}"
    );
}

/// Flash/beep pairs on `serve`'s own timeline, each beep within a quarter frame of its flash.
fn markers(
    video: &[&RecordedVideoFrame],
    audio: &[&RecordedAudioFrame],
    fps: i64,
    what: &str,
) -> Vec<MarkerPair> {
    if video.is_empty() || audio.is_empty() {
        return Vec::new();
    }
    let frames = luma_frames(video);
    let flashes = sync::flash_onsets(&frames);
    let pcm = pcm(audio);
    let start_us = rtp_us(audio[0].rtp_timestamp, AUDIO_HZ);
    let beeps = sync::beep_onsets(&pcm, start_us, SAMPLE_RATE);
    tone_is_440(&pcm, &beeps, start_us, what);
    if flashes.is_empty() || beeps.is_empty() {
        return Vec::new();
    }
    let frame_us = 1_000_000 / fps;
    let video_span = (frames[0].pts_us, frames.last().unwrap().pts_us);
    let audio_span = (
        start_us,
        start_us + sync::samples_to_us(pcm.len(), SAMPLE_RATE),
    );
    let pairs = sync::align(&flashes, &beeps, video_span, audio_span, frame_us)
        .unwrap_or_else(|e| panic!("{what}: {e}; flashes {flashes:?}, beeps {beeps:?}"));
    sync::check_offsets(&pairs, frame_us / 2, frame_us / 4)
        .unwrap_or_else(|e| panic!("{what}: {e}; pairs {pairs:?}"));
    pairs
}

/// Tolerates one first audio packet lost to receive-side track reordering, so only `dave_wire_order` guards the sender's nonce order.
fn start_drops(recorded: &Recorded, what: &str) -> usize {
    let tolerated = format!("{FIRST_FRAME_DAVE_DROP}audio");
    let drops = &recorded.dropped_frames;
    assert!(
        drops.len() <= 1 && drops.iter().all(|d| d.starts_with(&tolerated)),
        "{what}: {drops:?}"
    );
    drops.len()
}

/// One whole replay at the mock: frame and packet counts, decoding, RTP steps, pacing, A/V alignment and markers.
fn check_replay(
    video: &[&RecordedVideoFrame],
    audio: &[&RecordedAudioFrame],
    want: &Expected,
    lost_audio: usize,
    stalls: &[Stall],
    what: &str,
) -> Vec<MarkerPair> {
    assert_eq!(
        video.len(),
        want.video.len(),
        "{what}: video frames at the mock"
    );
    assert_eq!(
        audio.len(),
        want.audio.len() - lost_audio,
        "{what}: audio packets at the mock"
    );
    assert_eq!(
        audio[0].data, want.audio[lost_audio].key,
        "{what}: lost audio other than the first packet"
    );
    let video_ts: Vec<u32> = video.iter().map(|v| v.rtp_timestamp).collect();
    let audio_ts: Vec<u32> = audio.iter().map(|a| a.rtp_timestamp).collect();
    consecutive(
        &format!("{what} video"),
        &video_ts,
        (VIDEO_HZ / want.fps) as u32,
    );
    consecutive(&format!("{what} audio"), &audio_ts, 960);
    assert_eq!(
        luma_frames(video).len(),
        video.len(),
        "{what}: decoded frames"
    );
    let frame_us = 1_000_000 / want.fps;
    assert_paced(
        video,
        |f| slices(&f.data),
        |f| f.received_at,
        &want.video,
        frame_us,
        stalls,
        &format!("{what} video"),
    );
    assert_paced(
        audio,
        |f| f.data.clone(),
        |f| f.received_at,
        &want.audio,
        frame_us,
        stalls,
        &format!("{what} audio"),
    );
    assert_av_aligned(video, audio, want, what);
    let pairs = markers(video, audio, want.fps, what);
    assert!(
        pairs.len() >= 4,
        "{what}: only {} markers: {pairs:?}",
        pairs.len()
    );
    pairs
}

fn monotonic(recorded: &Recorded) {
    increasing(
        "video",
        &stream_video(recorded)
            .iter()
            .map(|v| v.rtp_timestamp)
            .collect::<Vec<_>>(),
    );
    increasing(
        "audio",
        &stream_audio(recorded)
            .iter()
            .map(|a| a.rtp_timestamp)
            .collect::<Vec<_>>(),
    );
}

fn replay_through_serve(name: &str, dir: &Path, trace: &Trace, plis: bool) {
    let want = expected(trace);
    let h = Harness::new("");
    let canary = StallCanary::start(&h.rt);
    let mut serve = h.serve();
    let mut c = h.external_session();
    let p = h.replay(dir, &[]);
    let id = p.attached();
    c.source("producing", |e| {
        e["state"] == "producing" && e["attachment"] == id
    });
    let (frames, packets) = (want.video.len(), want.audio.len());
    let dts = || want.video.iter().chain(&want.audio).map(|s| s.dts_usec);
    let span_us = dts().max().unwrap_or(0) - dts().min().unwrap_or(0);
    let deadline = Instant::now() + Duration::from_micros(span_us.unsigned_abs()) + T;
    let mut sent_plis = 0;
    let recorded = loop {
        if plis && sent_plis < 40 && h.rt.block_on(h.mock.send_pli(VoiceKind::Stream)) {
            sent_plis += 1;
        }
        let r = h.mock.recorded();
        let lost = r
            .dropped_frames
            .iter()
            .filter(|d| d.starts_with(&format!("{FIRST_FRAME_DAVE_DROP}audio")))
            .count();
        if stream_video(&r).len() >= frames && producer_audio(&r).len() + lost >= packets {
            break r;
        }
        if Instant::now() >= deadline {
            let counts = format!(
                "{name}: {} of {frames} video and {} of {packets} audio frames arrived, dropped {:?}",
                stream_video(&r).len(),
                producer_audio(&r).len(),
                r.dropped_frames
            );
            match c.try_status() {
                Ok(s) => match serve_drops(&s, &canary.stalls()) {
                    Some(drops) => panic!("{counts}; {drops}"),
                    None => panic!(
                        "{counts}; session {}; source {}",
                        s["stats"], s["source"]["stats"]
                    ),
                },
                Err(e) => {
                    let connection = c.connection_report();
                    let exit = serve.exit_report();
                    if exit.starts_with("exited") {
                        // The stderr reader may still be copying a dying serve's last lines.
                        std::thread::sleep(Duration::from_millis(500));
                    }
                    let log = h.serve_log();
                    let tail: Vec<&str> = log.lines().rev().take(30).collect();
                    panic!(
                        "{counts}; {e}; {connection}; serve {exit}; the end of serve's log:\n{}",
                        tail.into_iter().rev().collect::<Vec<_>>().join("\n")
                    )
                }
            }
        }
        std::thread::sleep(Duration::from_millis(if plis { 50 } else { 100 }));
    };
    let lines = p.lines.clone();
    let (status, stderr) = p.finish();
    assert!(status.success(), "{name}: {stderr}");
    let lines = lines.lock().unwrap().clone();
    let lost_audio = start_drops(&recorded, name);
    if plis {
        let requests = lines
            .iter()
            .filter(|v| v["event"]["type"] == "keyframe_request")
            .count();
        assert!(
            sent_plis >= 20 && requests >= 3,
            "{sent_plis} PLIs gave {requests} keyframe requests at the producer"
        );
    }
    monotonic(&recorded);
    check_replay(
        &stream_video(&recorded),
        &producer_audio(&recorded),
        &want,
        lost_audio,
        &canary.stalls(),
        name,
    );
}

#[test]
fn every_positive_trace_replays_through_serve() {
    let positives: Vec<_> = traces()
        .into_iter()
        .filter(|(n, _, t)| positive(n, t))
        .collect();
    assert!(positives.len() >= 6, "{} positive traces", positives.len());
    let _paced = exclusive_on_macos();
    for batch in positives.chunks(CONCURRENT_TRACES) {
        std::thread::scope(|s| {
            for (name, dir, trace) in batch {
                s.spawn(move || replay_through_serve(name, dir, trace, false));
            }
        });
    }
}

#[test]
fn control_events_during_a_replay_lose_no_packet() {
    let (name, dir, trace) = traces()
        .into_iter()
        .find(|(n, _, _)| n == "linux-x264-main-bf0-keyint2-30fps")
        .unwrap();
    let _paced = exclusive_on_macos();
    replay_through_serve(&name, &dir, &trace, true);
}

/// 150 frames sent at 30 fps; frames due inside `late_within` arrive `late` behind their dts.
fn paced_arrivals(
    start: Instant,
    late_within: &[Stall],
    late: Duration,
) -> (Vec<Sent>, Vec<(Vec<u8>, Instant)>) {
    let sent: Vec<Sent> = (0..150i64)
        .map(|i| Sent {
            key: i.to_le_bytes().to_vec(),
            pts_us: i * 33_333,
            dts_usec: i * 33_333,
        })
        .collect();
    let arrived = sent
        .iter()
        .map(|s| {
            let due = start + Duration::from_micros(s.dts_usec as u64);
            let delayed = late_within
                .iter()
                .any(|&(from, to)| due >= from && due <= to);
            (s.key.clone(), if delayed { due + late } else { due })
        })
        .collect();
    (sent, arrived)
}

fn check_arrivals(sent: &[Sent], arrived: &[(Vec<u8>, Instant)], stalls: &[Stall]) {
    let recorded: Vec<&(Vec<u8>, Instant)> = arrived.iter().collect();
    assert_paced(
        &recorded,
        |a| a.0.clone(),
        |a| a.1,
        sent,
        33_333,
        stalls,
        "synthetic",
    );
}

#[test]
fn a_few_stalls_excuse_the_frames_they_delay() {
    let start = Instant::now();
    let stalls: Vec<Stall> = [1_000u64, 3_000]
        .iter()
        .map(|&ms| {
            let from = start + Duration::from_millis(ms);
            (from, from + Duration::from_millis(80))
        })
        .collect();
    let (sent, arrived) = paced_arrivals(start, &stalls, Duration::from_millis(70));
    check_arrivals(&sent, &arrived, &stalls);
}

/// 40 unique frames at 60 fps, as many as a short 60 fps trace yields; `late` frames arrive that much behind their dts.
fn check_sixty_fps(late: &[(i64, Duration)]) {
    check_sixty_fps_with(Instant::now(), 40, late, &[]);
}

/// `frames` frames at 60 fps from `start`, with `late` frames behind their dts and the mock's runtime stalled in `stalls`.
fn check_sixty_fps_with(start: Instant, frames: i64, late: &[(i64, Duration)], stalls: &[Stall]) {
    let sent: Vec<Sent> = (0..frames)
        .map(|i| Sent {
            key: i.to_le_bytes().to_vec(),
            pts_us: i * 16_667,
            dts_usec: i * 16_667,
        })
        .collect();
    let arrived: Vec<(Vec<u8>, Instant)> = sent
        .iter()
        .zip(0..)
        .map(|(s, i)| {
            let delay = late
                .iter()
                .find(|(n, _)| *n == i)
                .map_or(Duration::ZERO, |l| l.1);
            let due = start + Duration::from_micros(s.dts_usec as u64);
            (s.key.clone(), due + delay)
        })
        .collect();
    let recorded: Vec<&(Vec<u8>, Instant)> = arrived.iter().collect();
    assert_paced(
        &recorded,
        |a| a.0.clone(),
        |a| a.1,
        &sent,
        16_667,
        stalls,
        "synthetic 60 fps",
    );
}

/// As on CI: a 122.7 ms stall ends 112.9 ms before the first frame, whose catch-up outlasts the stall's window.
fn catch_up_after_a_stall(tail: &[(i64, Duration)]) {
    catch_up_after_a_stall_in(41, tail);
}

fn catch_up_after_a_stall_in(frames: i64, tail: &[(i64, Duration)]) {
    let start = Instant::now() + Duration::from_secs(1);
    let stall_end = start + Duration::from_micros(22_900) - Duration::from_micros(112_900);
    let stalls = [(stall_end - Duration::from_micros(122_700), stall_end)];
    let mut late = vec![
        (0, Duration::from_micros(22_900)),
        (1, Duration::from_micros(13_300)),
        (2, Duration::from_micros(6_700)),
    ];
    late.extend_from_slice(tail);
    check_sixty_fps_with(start, frames, &late, &stalls);
}

fn ms(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

#[test]
#[should_panic(expected = "checked frames off")]
fn drift_right_after_a_stall_still_fails() {
    catch_up_after_a_stall(&[(4, ms(30)), (5, ms(40)), (6, ms(50)), (7, ms(60))]);
}

#[test]
#[should_panic(expected = "checked frames off")]
fn a_tail_growing_faster_than_jitter_inside_the_stall_still_fails() {
    catch_up_after_a_stall(&[(4, ms(30)), (5, ms(45)), (6, ms(60))]);
}

#[test]
#[should_panic(expected = "checked frames off")]
fn a_plateau_after_a_stall_shorter_than_it_still_fails() {
    let start = Instant::now() + Duration::from_secs(1);
    let stall_end = start + ms(50) - ms(5);
    let stalls = [(stall_end - ms(12), stall_end)];
    check_sixty_fps_with(
        start,
        41,
        &[(5, ms(40)), (6, ms(40)), (7, ms(40)), (8, ms(40))],
        &stalls,
    );
}

#[test]
#[should_panic(expected = "checked frames off")]
fn a_tail_longer_than_the_stall_still_fails() {
    let draining: Vec<(i64, Duration)> = (4..14)
        .map(|i| (i, Duration::from_micros(41_700 - 1_500 * (i as u64 - 4))))
        .collect();
    catch_up_after_a_stall_in(121, &draining);
}

#[test]
fn the_catch_up_right_after_a_stall_is_excused_with_it() {
    catch_up_after_a_stall(&[
        (4, Duration::from_micros(41_700)),
        (5, Duration::from_micros(25_200)),
    ]);
}

#[test]
#[should_panic(expected = "checked frames off")]
fn late_frames_after_the_catch_up_has_ended_still_fail() {
    catch_up_after_a_stall(&[
        (4, Duration::from_micros(41_700)),
        (20, Duration::from_micros(41_700)),
        (30, Duration::from_micros(41_700)),
    ]);
}

#[test]
#[should_panic(expected = "checked frames off")]
fn late_frames_without_a_stall_still_fail() {
    check_sixty_fps(&[
        (4, Duration::from_micros(41_700)),
        (5, Duration::from_micros(25_200)),
    ]);
}

#[test]
fn one_frame_of_jitter_past_the_slack_passes_a_short_trace() {
    check_sixty_fps(&[(20, Duration::from_millis(29))]);
}

#[test]
#[should_panic(expected = "checked frames off")]
fn two_late_frames_fail_a_short_trace() {
    check_sixty_fps(&[
        (10, Duration::from_millis(25)),
        (30, Duration::from_millis(25)),
    ]);
}

#[test]
#[should_panic(expected = "checked frames off")]
fn one_frame_late_past_the_jitter_margin_fails_a_short_trace() {
    check_sixty_fps(&[(20, Duration::from_millis(31))]);
}

#[test]
#[should_panic(expected = "over a quarter")]
fn many_short_stalls_cannot_excuse_half_a_trace() {
    let start = Instant::now();
    let stalls: Vec<Stall> = (0..40u64)
        .map(|i| {
            let from = start + Duration::from_millis(60 + i * 125);
            (from, from + Duration::from_millis(12))
        })
        .collect();
    let (sent, arrived) = paced_arrivals(start, &[], Duration::ZERO);
    check_arrivals(&sent, &arrived, &stalls);
}

#[test]
fn overlapping_stalls_excuse_their_union_once() {
    let start = Instant::now();
    let stalls: Vec<Stall> = (0..30u64)
        .map(|i| {
            let from = start + Duration::from_millis(1_000 + i * 30);
            (from, from + Duration::from_millis(12))
        })
        .collect();
    let (sent, arrived) = paced_arrivals(start, &[], Duration::ZERO);
    check_arrivals(&sent, &arrived, &stalls);
}

#[test]
fn a_stall_before_the_first_frame_is_excused_only_inside_the_trace() {
    let start = Instant::now() + Duration::from_secs(1);
    let stalls = [(
        start - Duration::from_millis(800),
        start + Duration::from_millis(50),
    )];
    let (sent, arrived) = paced_arrivals(start, &[], Duration::ZERO);
    check_arrivals(&sent, &arrived, &stalls);
}

#[test]
fn a_stall_that_ends_before_the_first_frame_still_excuses_the_frames_behind_it() {
    let start = Instant::now();
    let stall = (start, start + Duration::from_millis(180));
    let (sent, mut arrived) = paced_arrivals(start, &[], Duration::ZERO);
    for (i, a) in arrived.iter_mut().take(6).enumerate() {
        a.1 = stall.1 + Duration::from_micros(1_000 + 100 * i as u64);
    }
    check_arrivals(&sent, &arrived, &[stall]);
}

#[test]
fn b_frame_traces_are_refused_at_conversion() {
    let _paced = exclusive_on_macos();
    let negatives: Vec<_> = traces()
        .into_iter()
        .filter(|(n, _, _)| uses_b_frames(n))
        .collect();
    assert!(negatives.len() >= 2, "{} B-frame traces", negatives.len());
    let h = Harness::new("[serve]\ngrace_secs = 30\n");
    let _s = h.serve();
    let mut c = h.external_session();
    for (name, dir, _) in &negatives {
        let p = h.replay(dir, &[]);
        p.attached();
        let refused = p.wait_line("refusal", |v| v.get("refused").is_some());
        assert!(
            refused["refused"]
                .as_str()
                .unwrap()
                .starts_with("B-frames are enabled"),
            "{name}: {refused}"
        );
        let (status, stderr) = p.finish();
        assert!(!status.success(), "{name}");
        assert!(stderr.contains("B-frames are enabled"), "{name}: {stderr}");
        c.source(&format!("{name} detached"), |e| {
            e["state"] == "interrupted" && e["reason"] == "detached"
        });
    }
    assert!(
        stream_video(&h.mock.recorded()).is_empty(),
        "B-frame video reached the mock"
    );
}

#[test]
fn non_opus_traces_are_refused_at_hello() {
    let _paced = exclusive_on_macos();
    let refused: Vec<_> = traces()
        .into_iter()
        .filter(|(_, _, t)| t.audio_codec() != "opus")
        .collect();
    assert!(!refused.is_empty());
    let h = Harness::new("");
    let _s = h.serve();
    let _c = h.external_session();
    for (name, dir, _) in &refused {
        let p = h.replay(dir, &[]);
        let reply = p.wait_line("hello reply", |v| v.get("hello").is_some());
        assert_eq!(reply["hello"]["code"], "incompatible", "{name}: {reply}");
        let message = reply["hello"]["message"].as_str().unwrap();
        assert!(
            message.contains("audio encoder must be Opus"),
            "{name}: {reply}"
        );
        let (status, _) = p.finish();
        assert!(!status.success(), "{name}");
    }
}

/// Serve's own drops to a keyframe, which a stall that overfills its video queue causes by design.
fn serve_drops(status: &Value, stalls: &[Stall]) -> Option<String> {
    let dropped = status["stats"]["video_frames_dropped"]
        .as_u64()
        .filter(|&n| n > 0)?;
    let lengths: Vec<Duration> = stalls.iter().map(|&(from, to)| to - from).collect();
    Some(format!(
        "serve dropped {dropped} video frames to a keyframe ({} keyframe requests); stalls: {lengths:?}",
        status["stats"]["keyframe_requests"]
    ))
}

/// `Harness::wait`, failing with serve's drops when they explain the missing frames.
fn wait_or_drops<V>(
    h: &Harness,
    c: &mut Client,
    canary: &StallCanary,
    what: &str,
    f: impl FnMut(&Recorded) -> Option<V>,
) -> V {
    h.rt.block_on(h.mock.wait_for(T, f)).unwrap_or_else(|| {
        let drops = c
            .try_status()
            .ok()
            .and_then(|s| serve_drops(&s, &canary.stalls()));
        match drops {
            Some(drops) => panic!("mock never saw {what}; {drops}"),
            None => panic!("mock never saw {what}"),
        }
    })
}

/// Frames the mock received before `at` and after it.
fn split<'a, T>(
    frames: &[&'a T],
    received: impl Fn(&T) -> Instant,
    at: Instant,
) -> (Vec<&'a T>, Vec<&'a T>) {
    let i = frames.partition_point(|f| received(f) < at);
    (frames[..i].to_vec(), frames[i..].to_vec())
}

fn replay_killed_and_restarted(grace: Duration, gap: Duration) {
    let _paced = exclusive_on_macos();
    let (_, dir, trace) = traces()
        .into_iter()
        .find(|(n, _, _)| n == "linux-x264-main-bf0-keyint2-30fps")
        .unwrap();
    let want = expected(&trace);
    let h = Harness::new(&format!("[serve]\ngrace_secs = {}\n", grace.as_secs()));
    let canary = StallCanary::start(&h.rt);
    let _s = h.serve();
    let mut c = h.external_session();

    let mut first = h.replay(&dir, &[]);
    let id = first.attached();
    c.source("producing", |e| {
        e["state"] == "producing" && e["attachment"] == id
    });
    wait_or_drops(&h, &mut c, &canary, "75 video and 125 audio frames", |r| {
        (stream_video(r).len() >= 75 && producer_audio(r).len() >= 125).then_some(())
    });
    first.kill();
    let killed = Instant::now();
    c.source("interrupted", |e| {
        e["state"] == "interrupted" && e["reason"] == "detached"
    });
    std::thread::sleep(gap.saturating_sub(killed.elapsed()));
    let reattached = Instant::now();
    let second = h.replay(&dir, &[]);
    let id2 = second.attached();
    let detached_for = killed.elapsed();
    assert!(
        detached_for < grace,
        "detached for {detached_for:?}, beyond the {grace:?} grace period"
    );
    c.source("producing again", |e| {
        e["state"] == "producing" && e["attachment"] == id2
    });
    let (status, stderr) = second.finish();
    assert!(status.success(), "{stderr}");
    let (frames, packets) = (want.video.len(), want.audio.len());
    let recorded = wait_or_drops(&h, &mut c, &canary, "the whole second replay", |r| {
        let (_, v2) = split(&stream_video(r), |v| v.received_at, reattached);
        let (_, a2) = split(&producer_audio(r), |a| a.received_at, reattached);
        (v2.len() >= frames && a2.len() >= packets).then(|| r.clone())
    });
    let lost_first = start_drops(&recorded, "a restarted replay");
    assert_eq!(c.status()["session"]["state"], "streaming");
    monotonic(&recorded);

    let (v1, v2) = split(&stream_video(&recorded), |v| v.received_at, reattached);
    let (a1, a2) = split(&producer_audio(&recorded), |a| a.received_at, reattached);
    let drops = serve_drops(&c.status(), &canary.stalls());
    assert!(
        is_keyframe(&v2[0].data),
        "first video after the gap is not an IDR; {drops:?}; stalls {:?}",
        canary.stalls()
    );
    assert_eq!(
        a1[0].data, want.audio[lost_first].key,
        "audio lost before the kill"
    );
    let fps = want.fps;
    let step = (VIDEO_HZ / fps) as u32;
    // Serve dropping video to a keyframe leaves an RTP jump inside the run.
    let undropped = v1
        .windows(2)
        .position(|w| w[1].rtp_timestamp.wrapping_sub(w[0].rtp_timestamp) != step)
        .map_or(v1.len(), |i| i + 1);
    if undropped < v1.len() {
        assert!(
            is_keyframe(&v1[undropped].data),
            "before the kill: video jumps after {undropped} of {} frames to a frame that is not an IDR; {drops:?}",
            v1.len()
        );
    }
    let dropped_in_v1 = drops.as_ref().filter(|_| undropped < v1.len());
    if undropped < MIN_BASELINE_FRAMES {
        let dropped = dropped_in_v1.unwrap_or_else(|| {
            panic!(
                "before the kill: video jumps after {undropped} of {} frames, but serve reports no drops",
                v1.len()
            )
        });
        eprintln!(
            "before the kill: no baseline, serve dropped video after {undropped} frames; {dropped}"
        );
    } else {
        let v1_kept = &v1[..undropped];
        let kept_from = rtp_us(v1_kept[0].rtp_timestamp, VIDEO_HZ);
        let kept_until = rtp_us(v1_kept[v1_kept.len() - 1].rtp_timestamp, VIDEO_HZ);
        let a1_kept: Vec<_> = a1
            .iter()
            .copied()
            .filter(|a| rtp_us(a.rtp_timestamp, AUDIO_HZ) <= kept_until)
            .collect();
        let before = markers(v1_kept, &a1_kept, fps, "before the kill");
        let too_short = kept_until - kept_from < sync::FLASH_PERIOD_US;
        assert!(
            !before.is_empty() || (too_short && dropped_in_v1.is_some()),
            "no markers before the kill in {} of {} frames, {} µs; {drops:?}",
            v1_kept.len(),
            v1.len(),
            kept_until - kept_from
        );
        assert_av_aligned(v1_kept, &a1_kept, &want, "before the kill");
    }
    check_replay(&v2, &a2, &want, 0, &canary.stalls(), "after the restart");

    let last_before = rtp_us(v1.last().unwrap().rtp_timestamp, VIDEO_HZ);
    let resumed = rtp_us(v2[0].rtp_timestamp, VIDEO_HZ) - last_before;
    let frame_us = 1_000_000 / fps;
    assert!(
        resumed >= gap.as_micros() as i64 && resumed <= detached_for.as_micros() as i64 + 1_000_000,
        "video resumed {resumed} µs after the last frame; detached for {detached_for:?}, gap {gap:?}, one frame {frame_us} µs"
    );
}

#[test]
fn a_replay_killed_and_restarted_after_1_s_stays_aligned() {
    replay_killed_and_restarted(Duration::from_secs(30), Duration::from_secs(1));
}

#[test]
fn a_replay_killed_and_restarted_after_grace_minus_1_s_stays_aligned() {
    replay_killed_and_restarted(Duration::from_secs(10), Duration::from_secs(9));
}
