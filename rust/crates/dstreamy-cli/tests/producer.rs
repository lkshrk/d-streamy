//! `serve` + mock Discord + `mock-producer`, all real processes over the real socket.
#![cfg(unix)]

mod support;

use std::time::{Duration, Instant};

use dstreamy_cli::engine::ANNOUNCE_INTERVAL;
use dstreamy_media::QUEUE_MAX_BYTES;
use dstreamy_mock_discord::opcodes::voice::OP_VIDEO;
use dstreamy_mock_discord::{
    MOCK_GUILD_ID, MOCK_VOICE_CHANNEL_ID, Recorded, RecordedVideoFrame, VoiceKind,
};
use serde_json::{Value, json};
use support::*;

#[test]
fn a_producer_attaches_to_an_external_session_and_its_media_decodes() {
    let h = Harness::new("");
    let _s = h.serve();
    let mut c = h.external_session();
    let p = h.producer(&[]);
    let id = p.attached();
    c.source("producing", |e| {
        e["state"] == "producing" && e["attachment"] == id
    });
    let recorded = h.stream_media(60, 100);
    decodes(&stream_video(&recorded), &producer_audio(&recorded));
    p.wait_line("source event", |v| v["event"]["type"] == "source");
    p.wait_line("keyframe request", |v| {
        v["event"]["type"] == "keyframe_request"
    });
}

#[test]
fn hello_errors_no_session_busy_incompatible_and_unsupported_protocol() {
    let h = Harness::new("");
    let _s = h.serve();
    let (status, _) = h.producer(&[]).finish();
    assert!(!status.success());

    let hello = |line: Value| {
        let mut c = Client::connect(&h.endpoint());
        c.send(&line);
        c.reply("1")
    };
    let producer = |protocol: u64, fps: u64| {
        json!({
            "id": "1", "cmd": "hello", "role": "producer", "protocol": protocol,
            "video": {"codec": "h264", "width": 320, "height": 180, "fps": fps},
            "audio": {"codec": "opus", "sample_rate": 48000, "channels": 2, "frame_ms": 20},
        })
    };
    assert_eq!(hello(producer(2, 30))["code"], "no_session");

    let _c = h.external_session();
    let first = h.producer(&[]);
    first.attached();
    assert_eq!(hello(producer(2, 30))["code"], "busy");
    let incompatible = hello(producer(2, 25));
    assert_eq!(incompatible["code"], "incompatible", "{incompatible}");
    assert!(
        incompatible["message"]
            .as_str()
            .unwrap()
            .starts_with("video.fps"),
        "{incompatible}"
    );
    assert_eq!(hello(producer(3, 30))["code"], "unsupported_protocol");
    assert_eq!(hello(producer(0, 30))["code"], "unsupported_protocol");

    let second = h.producer(&[]);
    second.wait_line("busy", |v| v["hello"]["code"] == "busy");
    let (status, stderr) = second.finish();
    assert!(!status.success(), "{stderr}");
}

fn video_ops(r: &Recorded) -> Vec<(u64, u64, u64)> {
    r.ops
        .iter()
        .filter(|o| o.is(Some(VoiceKind::Stream), OP_VIDEO))
        .filter_map(|o| o.json())
        .map(|d| {
            let s = &d["streams"][0];
            (
                s["max_resolution"]["width"].as_u64().unwrap_or(0),
                s["max_resolution"]["height"].as_u64().unwrap_or(0),
                s["max_framerate"].as_u64().unwrap_or(0),
            )
        })
        .collect()
}

fn size_args(w: u64, hgt: u64, fps: u64) -> Vec<String> {
    [
        "--width",
        &w.to_string(),
        "--height",
        &hgt.to_string(),
        "--fps",
        &fps.to_string(),
    ]
    .map(str::to_owned)
    .to_vec()
}

#[test]
fn op_12_follows_each_producer_and_comes_before_its_media() {
    let h = Harness::new("");
    let _s = h.serve();
    let mut c = h.external_session();
    let mut seen = h.wait("the session's own op 12", |r| {
        let ops = video_ops(r);
        ops.contains(&(320, 180, u64::from(FPS)))
            .then_some(ops.len())
    });
    let mut previous_spawn: Option<Instant> = None;
    for want in [(640, 360, 15), (160, 90, 30), (640, 360, 15)] {
        let args = size_args(want.0, want.1, want.2);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let spawned = Instant::now();
        let mut p = h.producer(&args);
        let id = p.attached();
        c.source("producing", |e| e["attachment"] == id);
        if let Some(previous) = previous_spawn {
            let waited = previous.elapsed();
            assert!(
                waited >= ANNOUNCE_INTERVAL,
                "attached {waited:?} after the previous producer, before its rate-limited op 12"
            );
        }
        previous_spawn = Some(spawned);
        // The mock records op 12 asynchronously, so it may land after `attached`.
        seen = h.wait(&format!("op 12 {want:?}"), |r| {
            let ops = video_ops(r);
            ops[seen.min(ops.len())..]
                .iter()
                .position(|&op| op == want)
                .map(|at| seen + at + 1)
        });
        p.kill();
        c.source("detached", |e| e["reason"] == "detached");
    }
}

#[test]
fn waiting_for_a_rate_limited_op_12_does_not_count_as_a_stall() {
    let h = Harness::new("[serve]\nstall_ms = 500\n");
    let _s = h.serve();
    let mut c = h.external_session();
    for want in [(640, 360, 15), (160, 90, 30)] {
        let args = size_args(want.0, want.1, want.2);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut p = h.producer(&args);
        let id = p.attached();
        c.source("producing", |e| e["attachment"] == id);
        std::thread::sleep(Duration::from_millis(300));
        p.kill();
        c.source("detached", |e| e["reason"] == "detached");
    }
    c.drain();
    let stalls: Vec<&Value> = c
        .seen
        .iter()
        .filter(|m| m["event"]["event"] == "source" && m["event"]["reason"] == "stalled")
        .collect();
    assert!(stalls.is_empty(), "{stalls:?}");
}

#[test]
fn producers_cycling_sizes_get_at_most_one_op_12_a_second_and_the_last_size_wins() {
    let h = Harness::new("");
    let _s = h.serve();
    let mut c = h.external_session();
    let before = h.wait("the session's own op 12", |r| {
        let ops = video_ops(r);
        ops.contains(&(320, 180, u64::from(FPS)))
            .then_some(ops.len())
    });
    let started = Instant::now();
    let sizes = [(640, 360, 15), (160, 90, 30)];
    for i in 0..10 {
        let (w, hgt, fps) = sizes[i % 2];
        let args = size_args(w, hgt, fps);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut p = h.producer(&args);
        p.attached();
        p.kill();
        c.source("detached", |e| e["reason"] == "detached");
    }
    let elapsed = started.elapsed();
    let last = sizes[9 % 2];
    h.wait("the last size announced", |r| {
        (video_ops(r).last() == Some(&last)).then_some(())
    });
    let sent = video_ops(&h.mock.recorded()).len() - before;
    let allowed = (started.elapsed().as_secs_f64() / 1.0).ceil() as usize + 1;
    assert!(
        sent <= allowed,
        "{sent} op 12 in {elapsed:?} of producer churn; at most {allowed} allowed"
    );
}

#[test]
fn a_producer_restarted_within_grace_continues_the_stream() {
    let h = Harness::new("[serve]\ngrace_secs = 30\n");
    let _s = h.serve();
    let mut c = h.external_session();
    let mut first = h.producer(&[]);
    let id = first.attached();
    c.source("producing", |e| {
        e["state"] == "producing" && e["attachment"] == id
    });
    let before = h.stream_media(45, 75);
    first.kill();
    c.source("interrupted", |e| {
        e["state"] == "interrupted" && e["reason"] == "detached"
    });
    std::thread::sleep(Duration::from_millis(1_000));
    let second = h.producer(&[]);
    let id2 = second.attached();
    assert_ne!(id, id2);
    c.source("producing again", |e| {
        e["state"] == "producing" && e["attachment"] == id2
    });
    let (v0, a0) = (stream_video(&before).len(), producer_audio(&before).len());
    let recorded = h.stream_media(v0 + 45, a0 + 75);
    let video = stream_video(&recorded);
    let audio = stream_audio(&recorded);
    increasing(
        "video",
        &video.iter().map(|v| v.rtp_timestamp).collect::<Vec<_>>(),
    );
    increasing(
        "audio",
        &audio.iter().map(|a| a.rtp_timestamp).collect::<Vec<_>>(),
    );
    let split = after_gap(&video);
    assert!(
        is_keyframe(&video[split].data),
        "first video after the gap is not an IDR"
    );
    decodes(&video[split..], &producer_audio(&recorded)[a0..]);
    assert_eq!(c.status()["session"]["state"], "streaming");
}

#[test]
fn a_producer_that_disconnects_and_reconnects_attaches_again() {
    let h = Harness::new("");
    let _s = h.serve();
    let mut c = h.external_session();
    let p = h.producer(&[
        "--disconnect-after",
        "1500",
        "--reconnect",
        "1",
        "--duration",
        "4000",
    ]);
    let first = p.attached();
    p.wait_line("disconnect", |v| v["fault"] == "disconnect");
    c.source("interrupted", |e| {
        e["state"] == "interrupted" && e["reason"] == "detached"
    });
    let second = c.source("producing again", |e| {
        e["state"] == "producing" && e["attachment"] != first
    });
    assert!(second["event"]["attachment"].as_u64() > Some(first));
    let (status, stderr) = p.finish();
    assert!(status.success(), "{stderr}");
}

#[test]
fn without_a_restart_the_session_stops_with_capture_ended_after_grace() {
    let h = Harness::new("[serve]\ngrace_secs = 2\n");
    let _s = h.serve();
    let mut c = h.external_session();
    let mut p = h.producer(&[]);
    p.attached();
    h.stream_media(15, 25);
    p.kill();
    let killed = Instant::now();
    let stopped = c.until("stopped", |m| m["event"]["state"]["state"] == "stopped");
    assert_eq!(
        stopped["event"]["state"]["reason"],
        json!({"reason": "capture_ended"})
    );
    assert!(
        killed.elapsed() >= Duration::from_secs(2),
        "{:?}",
        killed.elapsed()
    );
}

fn detached_reason(h: &Harness, kind: &str) -> Value {
    let p = h.producer(&["--corrupt", kind, "--corrupt-after", "700"]);
    p.attached();
    let detached = p.wait_line("detached", |v| v["event"]["type"] == "detached");
    let (status, stderr) = p.finish();
    assert!(!status.success(), "{kind}: {stderr}");
    detached["event"].clone()
}

#[test]
fn each_corruption_detaches_with_its_reason_and_the_session_stays_in_grace() {
    let h = Harness::new("[serve]\ngrace_secs = 30\n");
    let _s = h.serve();
    let mut c = h.external_session();
    for (kind, reason, message) in [
        ("no-start-code", "incompatible", "not Annex B"),
        (
            "idr-without-flag",
            "incompatible",
            "not flagged as a keyframe",
        ),
        ("keyframe-without-sps", "incompatible", "no SPS"),
        ("pts-ne-dts", "incompatible", "B-frames are enabled"),
        ("opus-40ms", "incompatible", "not 20 ms"),
        ("oversized", "oversized", "exceeds"),
    ] {
        let detached = detached_reason(&h, kind);
        assert_eq!(detached["reason"], reason, "{kind}: {detached}");
        assert!(
            detached["message"].as_str().unwrap().contains(message),
            "{kind}: {detached}"
        );
        c.source(&format!("interrupted after {kind}"), |e| {
            e["state"] == "interrupted" && e["reason"] == "detached"
        });
        assert_eq!(c.status()["session"]["state"], "streaming", "{kind}");
    }
}

#[test]
fn a_session_stop_detaches_stopped_and_a_shutdown_detaches_shutdown() {
    let h = Harness::new("");
    let s = h.serve();
    let mut c = h.external_session();
    let p = h.producer(&[]);
    p.attached();
    c.send(&json!({"id": "stop", "cmd": "stop"}));
    assert_eq!(c.reply("stop")["type"], "result");
    let detached = p.wait_line("detached", |v| v["event"]["type"] == "detached");
    assert_eq!(detached["event"]["reason"], "stopped", "{detached}");
    drop(p);

    let mut c = h.external_session();
    let p = h.producer(&[]);
    p.attached();
    c.send(&json!({"id": "bye", "cmd": "shutdown"}));
    let detached = p.wait_line("detached", |v| v["event"]["type"] == "detached");
    assert_eq!(detached["event"]["reason"], "shutdown", "{detached}");
    let (status, stderr) = s.finish();
    assert!(status.success(), "{stderr}");
}

#[test]
fn a_session_that_stops_on_its_own_detaches_its_producer_stopped() {
    let h = Harness::new("");
    let _s = h.serve();
    let mut c = h.external_session();
    let p = h.producer(&[]);
    p.attached();
    h.stream_media(10, 15);
    assert!(
        h.mock
            .voice_state_from_other_session(MOCK_GUILD_ID, Some(MOCK_VOICE_CHANNEL_ID))
    );
    c.until("stopped", |m| m["event"]["state"]["state"] == "stopped");
    let detached = p.wait_line("detached", |v| v["event"]["type"] == "detached");
    assert_eq!(detached["event"]["reason"], "stopped", "{detached}");
    let again = h.producer(&[]);
    let reply = again.wait_line("hello reply", |v| v.get("hello").is_some());
    assert_eq!(reply["hello"]["code"], "no_session", "{reply}");
}

#[test]
fn a_stalled_producer_shows_interrupted_stalled_then_producing() {
    let h = Harness::new("[serve]\nstall_ms = 500\n");
    let _s = h.serve();
    let mut c = h.external_session();
    let p = h.producer(&["--stall-after", "1500", "--stall-for", "1500"]);
    let id = p.attached();
    c.source("producing", |e| e["state"] == "producing");
    p.wait_line("stall", |v| v["fault"] == "stall");
    let stalled_at = Instant::now();
    let stalled = c.source("stalled", |e| {
        e["state"] == "interrupted" && e["reason"] == "stalled"
    });
    // The event's `for_ms` is the time from the last frame to the stall.
    let for_ms = stalled["event"]["for_ms"].as_u64().unwrap();
    assert!(
        (400..1_500).contains(&for_ms),
        "stalled {for_ms} ms after the last frame, expected about stall_ms = 500"
    );
    let status = c.status();
    assert_eq!(status["source"]["state"], "interrupted", "{status}");
    assert_eq!(status["source"]["reason"], "stalled", "{status}");
    assert!(stalled_at.elapsed() < Duration::from_millis(1_500));
    c.source("producing after the stall", |e| {
        e["state"] == "producing" && e["attachment"] == id
    });
    p.wait_line("resume", |v| v["fault"] == "resume");
    assert_eq!(c.status()["source"]["state"], "producing");
}

#[test]
fn a_mock_pli_reaches_the_producer_as_a_keyframe_request() {
    let h = Harness::new("");
    let _s = h.serve();
    let _c = h.external_session();
    let p = h.producer(&[]);
    p.attached();
    h.stream_media(30, 50);
    std::thread::sleep(Duration::from_millis(700));
    let count = |p: &Proc| {
        p.lines()
            .iter()
            .filter(|v| v["event"]["type"] == "keyframe_request")
            .count()
    };
    let before = count(&p);
    let sent = Instant::now();
    assert!(h.rt.block_on(h.mock.send_pli(VoiceKind::Stream)));
    let deadline = sent + Duration::from_secs(2);
    while count(&p) == before {
        assert!(
            Instant::now() < deadline,
            "no keyframe_request within 2 s of the PLI"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn audio_and_video_stay_aligned_across_a_producer_restart() {
    let h = Harness::new("[serve]\nidle = \"none\"\n");
    let _s = h.serve();
    let mut c = h.external_session();
    let mut recorded = None;
    let mut sent = Vec::new();
    for n in 0..3 {
        let mut p = h.producer(&["--report-sent"]);
        let id = p.attached();
        c.source("producing", |e| e["attachment"] == id);
        let (v0, a0) = recorded.as_ref().map_or((0, 0), |r: &Recorded| {
            (stream_video(r).len(), producer_audio(r).len())
        });
        recorded = Some(h.stream_media(v0 + 45, a0 + 75));
        if n < 2 {
            p.kill();
            c.source("interrupted", |e| e["state"] == "interrupted");
            std::thread::sleep(Duration::from_millis(1_000));
        }
        sent.push(p.lines());
    }
    let recorded = recorded.unwrap();
    let video: Vec<&RecordedVideoFrame> = stream_video(&recorded);
    let audio = producer_audio(&recorded);
    let video_ts: Vec<u32> = video.iter().map(|v| v.rtp_timestamp).collect();
    let audio_ts: Vec<u32> = audio.iter().map(|a| a.rtp_timestamp).collect();
    let (vr, ar) = (
        runs(&video_ts, 90_000, 500_000),
        runs(&audio_ts, 48_000, 500_000),
    );
    assert_eq!(
        (vr.len(), ar.len()),
        (3, 3),
        "attachments: video {vr:?}, audio {ar:?}"
    );
    for &v in &vr[1..] {
        assert!(
            is_keyframe(&video[v].data),
            "video {v} after a restart is not an IDR"
        );
    }
    let frame_us = 1_000_000 / i64::from(FPS);
    let end = |r: &[usize], i: usize, len: usize| r.get(i + 1).copied().unwrap_or(len);
    for (i, sent) in sent.iter().enumerate() {
        let v = &video[vr[i]..end(&vr, i, video.len())];
        let a = &audio[ar[i]..end(&ar, i, audio.len())];
        let vs = shift_us(
            v.iter().map(|f| (f.data.as_slice(), f.rtp_timestamp)),
            sent,
            true,
            90_000,
        );
        let as_ = shift_us(
            a.iter().map(|f| (f.data.as_slice(), f.rtp_timestamp)),
            sent,
            false,
            48_000,
        );
        let (Some(vs), Some(as_)) = (vs, as_) else {
            panic!("attachment {i}: too few recorded frames matched what was sent");
        };
        assert!(
            (as_ - vs).abs() <= frame_us,
            "attachment {i}: serve moved audio by {as_} µs and video by {vs} µs; one frame is {frame_us} µs"
        );
    }
}

/// Peak resident memory of `pid`, in KiB.
#[cfg(target_os = "linux")]
fn peak_rss_kib(pid: u32) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .expect("VmHWM in /proc/<pid>/status")
}

/// Peak physical footprint of `pid`, in KiB.
#[cfg(target_os = "macos")]
fn peak_rss_kib(pid: u32) -> u64 {
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
    // SAFETY: `info` is a writable `rusage_info_v4`, the flavor asked for.
    let rc = unsafe {
        libc::proc_pid_rusage(
            pid as i32,
            libc::RUSAGE_INFO_V4,
            info.as_mut_ptr().cast::<libc::rusage_info_t>(),
        )
    };
    assert_eq!(rc, 0, "proc_pid_rusage({pid})");
    // SAFETY: filled by the successful call above.
    unsafe { info.assume_init() }.ri_lifetime_max_phys_footprint / 1024
}

struct Overload {
    peak_rss_kib: u64,
    /// The session's `StreamStats`; the pump drops into these when it falls behind.
    session: Value,
    source: Value,
    depths: Vec<u64>,
    keyframe_requests: usize,
    sent_bytes: u64,
}

impl Overload {
    fn dropped(&self) -> u64 {
        let sum =
            |v: &Value, keys: &[&str]| keys.iter().map(|k| v[k].as_u64().unwrap()).sum::<u64>();
        sum(
            &self.session,
            &["video_frames_dropped", "audio_frames_dropped"],
        ) + sum(&self.source, &["dropped_video", "dropped_audio"])
    }
}

/// Runs a producer for at most `secs` and samples the source queue depth every 50 ms.
fn overload_run(burst: Option<u32>, secs: u64) -> Overload {
    overload_run_with(burst, secs, &[])
}

fn overload_run_with(burst: Option<u32>, secs: u64, extra: &[&str]) -> Overload {
    let h = Harness::new("");
    let s = h.serve();
    let mut c = h.external_session();
    let mut args = vec!["--duration".to_owned(), (secs * 1000).to_string()];
    if let Some(factor) = burst {
        args.extend(["--burst".to_owned(), factor.to_string()]);
    }
    let mut args: Vec<&str> = args.iter().map(String::as_str).collect();
    args.extend(extra);
    let mut p = h.producer(&args);
    p.attached();
    let mut depths = Vec::new();
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end && !p.exited() {
        depths.push(c.status()["source"]["stats"]["queued"].as_u64().unwrap());
        std::thread::sleep(Duration::from_millis(50));
    }
    let lines = p.lines.clone();
    let (status, stderr) = p.finish();
    assert!(status.success(), "{stderr}");
    let lines = lines.lock().unwrap().clone();
    let keyframe_requests = lines
        .iter()
        .filter(|v| v["event"]["type"] == "keyframe_request")
        .count();
    let outcome = lines.iter().find_map(|v| v["outcome"].as_str());
    assert_eq!(outcome, Some("Finished"), "{lines:?}");
    let sent_bytes = lines
        .iter()
        .find_map(|v| v["sent_bytes"].as_u64())
        .expect("mock-producer reports the bytes it sent");
    let status = c.status();
    c.drain();
    let run = Overload {
        keyframe_requests,
        sent_bytes,
        peak_rss_kib: peak_rss_kib(s.pid()),
        session: status["stats"].clone(),
        source: status["source"]["stats"].clone(),
        depths,
    };
    eprintln!(
        "burst {burst:?}: sent {} KiB, serve peak RSS {} KiB, session {}, source {}",
        run.sent_bytes / 1024,
        run.peak_rss_kib,
        run.session,
        run.source
    );
    run
}

fn assert_within_capacity(run: &Overload) {
    let capacity = dstreamy_media::FRAME_CHANNEL_CAPACITY as u64;
    assert!(
        run.source["max_queued"].as_u64().unwrap() <= capacity,
        "source queue reached {}",
        run.source
    );
    assert!(
        run.depths.iter().all(|&d| d <= capacity),
        "{:?}",
        run.depths
    );
}

const RSS_MARGIN_KIB: u64 = 8 * 1024;

#[test]
fn a_burst_10_producer_keeps_serve_memory_within_a_margin_of_a_real_time_baseline() {
    let baseline = overload_run(None, 10);
    let burst = overload_run(Some(10), 10);
    assert_within_capacity(&baseline);
    assert_within_capacity(&burst);
    assert!(
        burst.peak_rss_kib <= baseline.peak_rss_kib + RSS_MARGIN_KIB,
        "serve peak RSS {} KiB is more than {RSS_MARGIN_KIB} KiB over the {} KiB baseline",
        burst.peak_rss_kib,
        baseline.peak_rss_kib
    );
    let sent = |r: &Overload| r.session["video_frames_sent"].as_u64().unwrap();
    assert!(
        sent(&burst) > 5 * sent(&baseline),
        "the burst was not faster than real time: {} vs {} video frames",
        sent(&burst),
        sent(&baseline)
    );
}

#[test]
fn a_producer_faster_than_serve_can_send_is_dropped_not_buffered() {
    let baseline = overload_run(None, 5);
    let flood = overload_run(Some(200), 5);
    assert_within_capacity(&flood);
    assert!(
        flood.dropped() > baseline.dropped(),
        "drop counters did not rise: baseline {} {}, flood {} {}",
        baseline.session,
        baseline.source,
        flood.session,
        flood.source
    );
    assert!(
        flood.peak_rss_kib <= baseline.peak_rss_kib + RSS_MARGIN_KIB,
        "serve peak RSS {} KiB is more than {RSS_MARGIN_KIB} KiB over the {} KiB baseline",
        flood.peak_rss_kib,
        baseline.peak_rss_kib
    );
}

#[test]
fn a_flood_of_8_mib_keyframes_keeps_serve_memory_bounded() {
    let baseline = overload_run(None, 5);
    let pad = (8 << 20) - 64;
    // Queues and in-flight copies measured up to ~260 MiB with glibc arenas; keeping what was sent would exceed it.
    let bound_kib = (QUEUE_MAX_BYTES as u64 + 40 * (8 << 20)) / 1024;
    let target = (bound_kib + bound_kib / 2) * 1024 + (16 << 20);
    let flood = overload_run_with(
        Some(50),
        120,
        &[
            "--pad-keyframes",
            &pad.to_string(),
            "--until-bytes",
            &target.to_string(),
        ],
    );
    assert_within_capacity(&flood);
    let source_lost = flood.source["dropped_video"].as_u64().unwrap();
    assert!(
        source_lost > 0,
        "the source queue never overflowed: {}",
        flood.source
    );
    let requested = flood.source["overflow_keyframe_requests"].as_u64().unwrap();
    assert!(
        requested > 0,
        "no keyframe request after overflow: {}",
        flood.source
    );
    assert!(
        flood.keyframe_requests > 0,
        "the producer saw no keyframe request"
    );
    assert!(
        flood.sent_bytes >= target,
        "the flood sent only {} of {} KiB within its time cap",
        flood.sent_bytes / 1024,
        target / 1024
    );
    assert!(
        flood.peak_rss_kib <= baseline.peak_rss_kib + bound_kib,
        "serve peak RSS {} KiB is more than {bound_kib} KiB over the {} KiB baseline",
        flood.peak_rss_kib,
        baseline.peak_rss_kib
    );
}
