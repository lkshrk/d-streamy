//! `serve` + mock Discord + `mock-producer --replay`: real OBS traces converted by `obs/src/packet.c`.
#![cfg(unix)]

mod support;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dstreamy_media::annexb;
use dstreamy_mock_discord::{RecordedAudioFrame, RecordedVideoFrame};
use dstreamy_test_producer::sync::{
    self, H264LumaDecoder, LumaFrame, MarkerPair, OpusMonoDecoder, SAMPLE_RATE, TONE_HZ,
};
use dstreamy_test_producer::trace::{Converted, Trace};
use support::*;

const VIDEO_HZ: i64 = 90_000;
const AUDIO_HZ: i64 = 48_000;

fn traces_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../obs/tests/traces")
}

fn trace_dir(name: &str) -> PathBuf {
    traces_dir().join(name)
}

fn uses_b_frames(name: &str) -> bool {
    name.contains("-bf2-") || name.contains("-bfunset-")
}

const POSITIVE: [&str; 6] = [
    "linux-ffmpeg_opus-defaults",
    "linux-x264-baseline-bf0-keyint2-30fps",
    "linux-x264-baseline-bf0-keyint2-60fps",
    "linux-x264-main-bf0-keyint0-30fps",
    "linux-x264-main-bf0-keyint2-30fps",
    "linux-x264-main-bf0-keyint2-60fps",
];

/// What the producer sends for a trace, as `packet.c` converts it: payload and pts in µs per track.
struct Expected {
    fps: i64,
    video: Vec<(Vec<u8>, i64)>,
    audio: Vec<(Vec<u8>, i64)>,
}

fn expected(trace: &Trace) -> Expected {
    let (mut video, mut audio) = (Vec::new(), Vec::new());
    for r in trace.clone().replay() {
        match r.expect("positive trace converts").frame {
            Converted::Video { data, pts_us, .. } => video.push((data, pts_us)),
            Converted::Audio { data, pts_us } => audio.push((data, pts_us)),
        }
    }
    Expected {
        fps: i64::from(trace.fps().unwrap()),
        video,
        audio,
    }
}

#[derive(Debug, Clone, Copy)]
struct Clock {
    base: u32,
    hz: i64,
}

impl Clock {
    /// RTP bases are random (RFC 3550 §5.1), so each track is anchored on the pts of the frames whose payload was sent once.
    fn anchored<'a>(
        recorded: impl Iterator<Item = (u32, &'a [u8])>,
        sent: &[(Vec<u8>, i64)],
        hz: i64,
    ) -> Self {
        let mut by_payload: HashMap<&[u8], Option<i64>> = HashMap::new();
        for (data, pts) in sent {
            by_payload
                .entry(data)
                .and_modify(|p| *p = None)
                .or_insert(Some(*pts));
        }
        let bases: Vec<u32> = recorded
            .filter_map(|(rtp, data)| {
                let pts = (*by_payload.get(data)?)?;
                Some(rtp.wrapping_sub((pts * hz).div_euclid(1_000_000) as u32))
            })
            .collect();
        assert!(
            bases.len() >= 10,
            "only {} frames matched what was sent",
            bases.len()
        );
        let first = bases[0];
        let mut deltas: Vec<i32> = bases.iter().map(|b| b.wrapping_sub(first) as i32).collect();
        deltas.sort_unstable();
        Self {
            base: first.wrapping_add(deltas[deltas.len() / 2] as u32),
            hz,
        }
    }

    /// The transport rewrites the SPS and start codes, so video frames are matched on their slice NAL units.
    fn video(frames: &[&RecordedVideoFrame], sent: &[(Vec<u8>, i64)]) -> Self {
        let sent: Vec<(Vec<u8>, i64)> = sent.iter().map(|(d, p)| (slices(d), *p)).collect();
        let recorded: Vec<(u32, Vec<u8>)> = frames
            .iter()
            .map(|f| (f.rtp_timestamp, slices(&f.data)))
            .collect();
        Self::anchored(
            recorded.iter().map(|(t, d)| (*t, d.as_slice())),
            &sent,
            VIDEO_HZ,
        )
    }

    fn audio(frames: &[&RecordedAudioFrame], sent: &[(Vec<u8>, i64)]) -> Self {
        Self::anchored(
            frames.iter().map(|f| (f.rtp_timestamp, f.data.as_slice())),
            sent,
            AUDIO_HZ,
        )
    }

    fn us(&self, rtp: u32) -> i64 {
        i64::from(rtp.wrapping_sub(self.base) as i32) * 1_000_000 / self.hz
    }
}

fn slices(au: &[u8]) -> Vec<u8> {
    annexb::nal_units(au)
        .filter(|n| matches!(annexb::h264_nal_type(n), Some(1 | 5)))
        .flatten()
        .copied()
        .collect()
}

fn luma_frames(video: &[&RecordedVideoFrame], clock: Clock) -> Vec<LumaFrame> {
    let mut decoder = H264LumaDecoder::new().unwrap();
    let mut frames = Vec::new();
    for (i, v) in video.iter().enumerate() {
        frames.extend(
            decoder
                .decode(&v.data, clock.us(v.rtp_timestamp))
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

/// Flash/beep pairs of one attachment's media, every beep - flash offset within the tightened check.
fn markers(
    video: &[&RecordedVideoFrame],
    audio: &[&RecordedAudioFrame],
    clocks: (Clock, Clock),
    fps: i64,
    what: &str,
) -> Vec<MarkerPair> {
    let frames = luma_frames(video, clocks.0);
    let flashes = sync::flash_onsets(&frames);
    let pcm = pcm(audio);
    let start_us = clocks.1.us(audio[0].rtp_timestamp);
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

fn replay_trace_through_serve(name: &str) {
    let dir = trace_dir(name);
    let trace = Trace::load(&dir).unwrap();
    let want = expected(&trace);
    let h = Harness::new("");
    let _s = h.serve();
    let mut c = h.external_session();
    let p = h.replay(&dir, &[]);
    let id = p.attached();
    c.source("producing", |e| {
        e["state"] == "producing" && e["attachment"] == id
    });
    let (frames, packets) = (want.video.len(), want.audio.len());
    let recorded = h.wait(
        &format!("{name}: {frames} video and {packets} audio"),
        |r| {
            (stream_video(r).len() >= frames && producer_audio(r).len() >= packets)
                .then(|| r.clone())
        },
    );
    let (status, stderr) = p.finish();
    assert!(status.success(), "{name}: {stderr}");
    assert!(
        recorded.dropped_frames.is_empty(),
        "{name}: {:?}",
        recorded.dropped_frames
    );

    let video = stream_video(&recorded);
    let audio = producer_audio(&recorded);
    assert_eq!(video.len(), frames, "{name}: video frames at the mock");
    assert_eq!(audio.len(), packets, "{name}: audio packets at the mock");
    let video_ts: Vec<u32> = video.iter().map(|v| v.rtp_timestamp).collect();
    let audio_ts: Vec<u32> = audio.iter().map(|a| a.rtp_timestamp).collect();
    increasing("video", &video_ts);
    increasing(
        "audio",
        &stream_audio(&recorded)
            .iter()
            .map(|a| a.rtp_timestamp)
            .collect::<Vec<_>>(),
    );
    consecutive("video", &video_ts, (VIDEO_HZ / want.fps) as u32);
    consecutive("audio", &audio_ts, 960);

    let clocks = (
        Clock::video(&video, &want.video),
        Clock::audio(&audio, &want.audio),
    );
    let decoded = luma_frames(&video, clocks.0);
    assert_eq!(decoded.len(), frames, "{name}: decoded frames");
    let pairs = markers(&video, &audio, clocks, want.fps, name);
    assert!(
        pairs.len() >= 4,
        "{name}: only {} markers: {pairs:?}",
        pairs.len()
    );
}

#[test]
fn every_positive_trace_is_covered() {
    let mut names: Vec<String> = std::fs::read_dir(traces_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| traces_dir().join(n).is_dir())
        .filter(|n| !uses_b_frames(n))
        .filter(|n| Trace::load(&trace_dir(n)).unwrap().audio_codec() == "opus")
        .collect();
    names.sort();
    assert_eq!(names, POSITIVE);
}

#[test]
fn replays_ffmpeg_opus_defaults() {
    replay_trace_through_serve(POSITIVE[0]);
}

#[test]
fn replays_x264_baseline_bf0_keyint2_30fps() {
    replay_trace_through_serve(POSITIVE[1]);
}

#[test]
fn replays_x264_baseline_bf0_keyint2_60fps() {
    replay_trace_through_serve(POSITIVE[2]);
}

#[test]
fn replays_x264_main_bf0_keyint0_30fps() {
    replay_trace_through_serve(POSITIVE[3]);
}

#[test]
fn replays_x264_main_bf0_keyint2_30fps() {
    replay_trace_through_serve(POSITIVE[4]);
}

#[test]
fn replays_x264_main_bf0_keyint2_60fps() {
    replay_trace_through_serve(POSITIVE[5]);
}

#[test]
fn b_frame_traces_are_refused_at_conversion() {
    let h = Harness::new("[serve]\ngrace_secs = 30\n");
    let _s = h.serve();
    let mut c = h.external_session();
    for name in [
        "linux-x264-main-bf2-keyint2-30fps",
        "linux-x264-main-bfunset-keyint2-30fps",
    ] {
        let p = h.replay(&trace_dir(name), &[]);
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
    let recorded = h.mock.recorded();
    assert!(
        stream_video(&recorded).is_empty(),
        "B-frame video reached the mock"
    );
}

#[test]
fn the_aac_trace_is_refused_at_hello() {
    let h = Harness::new("");
    let _s = h.serve();
    let _c = h.external_session();
    let p = h.replay(&trace_dir("linux-ffmpeg_aac-defaults"), &[]);
    let reply = p.wait_line("hello reply", |v| v.get("hello").is_some());
    assert_eq!(reply["hello"]["code"], "incompatible", "{reply}");
    let message = reply["hello"]["message"].as_str().unwrap();
    assert!(message.contains("audio encoder must be Opus"), "{reply}");
    let (status, _) = p.finish();
    assert!(!status.success());
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

fn replay_killed_and_restarted(grace_secs: u64, gap: Duration) {
    let name = "linux-x264-main-bf0-keyint2-30fps";
    let dir = trace_dir(name);
    let want = expected(&Trace::load(&dir).unwrap());
    let h = Harness::new(&format!("[serve]\ngrace_secs = {grace_secs}\n"));
    let _s = h.serve();
    let mut c = h.external_session();

    let mut first = h.replay(&dir, &[]);
    let id = first.attached();
    c.source("producing", |e| {
        e["state"] == "producing" && e["attachment"] == id
    });
    h.stream_media(75, 125);
    first.kill();
    c.source("interrupted", |e| {
        e["state"] == "interrupted" && e["reason"] == "detached"
    });
    std::thread::sleep(gap);
    let second = h.replay(&dir, &[]);
    let id2 = second.attached();
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

    increasing(
        "video",
        &stream_video(&recorded)
            .iter()
            .map(|v| v.rtp_timestamp)
            .collect::<Vec<_>>(),
    );
    increasing(
        "audio",
        &stream_audio(&recorded)
            .iter()
            .map(|a| a.rtp_timestamp)
            .collect::<Vec<_>>(),
    );
    let (v1, v2) = split(&stream_video(&recorded), |v| v.rtp_timestamp, VIDEO_HZ).unwrap();
    let (a1, a2) = split(&producer_audio(&recorded), |a| a.rtp_timestamp, AUDIO_HZ).unwrap();
    assert_eq!(
        (v2.len(), a2.len()),
        (frames, packets),
        "second replay at the mock"
    );
    assert!(
        is_keyframe(&v2[0].data),
        "first video after the gap is not an IDR"
    );

    // Both attachments are read on the first one's clocks, so any shift at the restart moves the markers.
    let clocks = (
        Clock::video(&v1, &want.video),
        Clock::audio(&a1, &want.audio),
    );
    let fps = want.fps;
    let before = markers(&v1, &a1, clocks, fps, "before the kill");
    let after = markers(&v2, &a2, clocks, fps, "after the restart");
    assert!(
        !before.is_empty() && after.len() >= 4,
        "{before:?} {after:?}"
    );
    let resumed_us =
        clocks.0.us(v2[0].rtp_timestamp) - clocks.0.us(v1.last().unwrap().rtp_timestamp);
    assert!(
        resumed_us >= gap.as_micros() as i64,
        "the second attachment resumed {resumed_us} µs after the first, within the {gap:?} gap"
    );
}

#[test]
fn a_replay_killed_and_restarted_after_1_s_stays_aligned() {
    replay_killed_and_restarted(30, Duration::from_secs(1));
}

#[test]
fn a_replay_killed_and_restarted_after_grace_minus_1_s_stays_aligned() {
    replay_killed_and_restarted(3, Duration::from_secs(2));
}
