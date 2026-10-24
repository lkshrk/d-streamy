//! `serve` + mock Discord + `mock-producer --replay`: real OBS traces converted by `dstreamy-obs-packet`.
#![cfg(unix)]

mod support;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use dstreamy_media::annexb;
use dstreamy_mock_discord::{Recorded, RecordedAudioFrame, RecordedVideoFrame, VoiceKind};
use dstreamy_test_producer::sync::{
    self, H264LumaDecoder, LumaFrame, MarkerPair, OpusMonoDecoder, SAMPLE_RATE, TONE_HZ,
};
use dstreamy_test_producer::trace::{Converted, Trace};
use support::*;

const VIDEO_HZ: i64 = 90_000;
const AUDIO_HZ: i64 = 48_000;
const PACING_SLACK_US: i64 = 20_000;

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

/// `test-hooks` starts every RTP clock at 0, so a timestamp is the pts `serve` stamped on its shared timeline.
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

/// Arrival at the mock follows the trace's libobs dts, so the producer paces in real time.
fn assert_paced<F>(
    recorded: &[&F],
    key: impl Fn(&F) -> Vec<u8>,
    at: impl Fn(&F) -> Instant,
    sent: &[Sent],
    frame_us: i64,
    what: &str,
) {
    let pairs = matched(recorded, key, sent);
    let (f0, s0) = pairs[0];
    let slack = PACING_SLACK_US.max(frame_us);
    let mut lags: Vec<i64> = pairs
        .iter()
        .map(|(f, s)| {
            let arrived = at(f).duration_since(at(f0)).as_micros() as i64;
            arrived - (s.dts_usec - s0.dts_usec)
        })
        .collect();
    let mid = median(lags.clone());
    lags.retain(|l| (l - mid).abs() > slack);
    assert!(
        lags.len() * 50 <= pairs.len(),
        "{what}: {} of {} frames off the trace's dts pace by more than {slack} µs: {lags:?}",
        lags.len(),
        pairs.len()
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

/// Flash/beep pairs on `serve`'s own timeline, every beep - flash offset within the tightened check.
fn markers(
    video: &[&RecordedVideoFrame],
    audio: &[&RecordedAudioFrame],
    fps: i64,
    what: &str,
) -> Vec<MarkerPair> {
    let frames = luma_frames(video);
    let flashes = sync::flash_onsets(&frames);
    let pcm = pcm(audio);
    let start_us = rtp_us(audio[0].rtp_timestamp, AUDIO_HZ);
    let beeps = sync::beep_onsets(&pcm, start_us, SAMPLE_RATE);
    tone_is_440(&pcm, &beeps, start_us, what);
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

/// One whole replay at the mock: frame and packet counts, decoding, RTP steps, pacing, A/V alignment and markers.
fn check_replay(
    video: &[&RecordedVideoFrame],
    audio: &[&RecordedAudioFrame],
    want: &Expected,
    what: &str,
) -> Vec<MarkerPair> {
    assert_eq!(
        video.len(),
        want.video.len(),
        "{what}: video frames at the mock"
    );
    assert_eq!(
        audio.len(),
        want.audio.len(),
        "{what}: audio packets at the mock"
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
        &format!("{what} video"),
    );
    assert_paced(
        audio,
        |f| f.data.clone(),
        |f| f.received_at,
        &want.audio,
        frame_us,
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
    let _s = h.serve();
    let mut c = h.external_session();
    let p = h.replay(dir, &[]);
    let id = p.attached();
    c.source("producing", |e| {
        e["state"] == "producing" && e["attachment"] == id
    });
    let (frames, packets) = (want.video.len(), want.audio.len());
    let mut sent_plis = 0;
    let recorded = loop {
        if plis && sent_plis < 40 && h.rt.block_on(h.mock.send_pli(VoiceKind::Stream)) {
            sent_plis += 1;
        }
        let r = h.mock.recorded();
        if stream_video(&r).len() >= frames && producer_audio(&r).len() >= packets {
            break r;
        }
        std::thread::sleep(Duration::from_millis(if plis { 50 } else { 100 }));
    };
    let lines = p.lines();
    let (status, stderr) = p.finish();
    assert!(status.success(), "{name}: {stderr}");
    assert!(
        recorded.dropped_frames.is_empty(),
        "{name}: {:?}",
        recorded.dropped_frames
    );
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
    std::thread::scope(|s| {
        for (name, dir, trace) in &positives {
            s.spawn(move || replay_through_serve(name, dir, trace, false));
        }
    });
}

#[test]
fn control_events_during_a_replay_lose_no_packet() {
    let (name, dir, trace) = traces()
        .into_iter()
        .find(|(n, _, _)| n == "linux-x264-main-bf0-keyint2-30fps")
        .unwrap();
    replay_through_serve(&name, &dir, &trace, true);
}

#[test]
fn b_frame_traces_are_refused_at_conversion() {
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

/// Splits one track's recorded frames at the restart's RTP jump; `None` until there are two runs.
fn split<'a, T>(
    frames: &[&'a T],
    ts: impl Fn(&T) -> u32,
    hz: i64,
) -> Option<(Vec<&'a T>, Vec<&'a T>)> {
    let rtp: Vec<u32> = frames.iter().map(|f| ts(f)).collect();
    let starts = runs(&rtp, hz, 500_000);
    assert!(starts.len() <= 2, "attachments in {starts:?}");
    let at = *starts.get(1)?;
    Some((frames[..at].to_vec(), frames[at..].to_vec()))
}

fn replay_killed_and_restarted(grace: Duration, gap: Duration) {
    let (_, dir, trace) = traces()
        .into_iter()
        .find(|(n, _, _)| n == "linux-x264-main-bf0-keyint2-30fps")
        .unwrap();
    let want = expected(&trace);
    let h = Harness::new(&format!("[serve]\ngrace_secs = {}\n", grace.as_secs()));
    let _s = h.serve();
    let mut c = h.external_session();

    let mut first = h.replay(&dir, &[]);
    let id = first.attached();
    c.source("producing", |e| {
        e["state"] == "producing" && e["attachment"] == id
    });
    h.stream_media(75, 125);
    first.kill();
    let killed = Instant::now();
    c.source("interrupted", |e| {
        e["state"] == "interrupted" && e["reason"] == "detached"
    });
    std::thread::sleep(gap.saturating_sub(killed.elapsed()));
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
    let recorded = h.wait("the whole second replay", |r| {
        let (_, v2) = split(&stream_video(r), |v| v.rtp_timestamp, VIDEO_HZ)?;
        let (_, a2) = split(&producer_audio(r), |a| a.rtp_timestamp, AUDIO_HZ)?;
        (v2.len() >= frames && a2.len() >= packets).then(|| r.clone())
    });
    assert!(
        recorded.dropped_frames.is_empty(),
        "{:?}",
        recorded.dropped_frames
    );
    assert_eq!(c.status()["session"]["state"], "streaming");
    monotonic(&recorded);

    let (v1, v2) = split(&stream_video(&recorded), |v| v.rtp_timestamp, VIDEO_HZ).unwrap();
    let (a1, a2) = split(&producer_audio(&recorded), |a| a.rtp_timestamp, AUDIO_HZ).unwrap();
    assert!(
        is_keyframe(&v2[0].data),
        "first video after the gap is not an IDR"
    );
    let fps = want.fps;
    let before = markers(&v1, &a1, fps, "before the kill");
    assert!(!before.is_empty(), "{before:?}");
    assert_av_aligned(&v1, &a1, &want, "before the kill");
    check_replay(&v2, &a2, &want, "after the restart");

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
