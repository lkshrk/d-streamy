//! Real OBS through the plugin: `obs-harness` loads `obs-dstreamy` and streams to `serve` + mock Discord; obs-ci only.
#![cfg(target_os = "linux")]

mod support;

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dstreamy_mock_discord::{
    MOCK_GUILD_ID, MOCK_VOICE_CHANNEL_ID, RecordedAudioFrame, RecordedVideoFrame,
};
use dstreamy_test_producer::sync::{self, H264LumaDecoder, OpusMonoDecoder, SAMPLE_RATE};
use serde_json::{Value, json};
use support::*;

const VIDEO_HZ: i64 = 90_000;
const AUDIO_HZ: i64 = 48_000;
const OBS_FPS: i64 = 30;
const MODULES: &str = "image-source,obs-x264,obs-ffmpeg,obs-dstreamy";
const HARNESS_EXIT: Duration = Duration::from_secs(120);

struct ObsEnv {
    harness: PathBuf,
    plugin_bin: String,
    plugin_data: String,
}

fn obs_env() -> ObsEnv {
    let var = |name: &str| {
        std::env::var(name).unwrap_or_else(|_| {
            panic!("{name} is unset: set DSTREAMY_OBS_HARNESS, DSTREAMY_OBS_PLUGIN_BIN and DSTREAMY_OBS_PLUGIN_DATA")
        })
    };
    ObsEnv {
        harness: var("DSTREAMY_OBS_HARNESS").into(),
        plugin_bin: var("DSTREAMY_OBS_PLUGIN_BIN"),
        plugin_data: var("DSTREAMY_OBS_PLUGIN_DATA"),
    }
}

/// An `obs-harness` run whose output lines are collected.
struct HarnessRun {
    child: Child,
    lines: Arc<Mutex<Vec<String>>>,
    readers: Vec<std::thread::JoinHandle<()>>,
}

impl HarnessRun {
    fn start(h: &Harness, settings: Value, args: &[&str]) -> Self {
        let env = obs_env();
        let mut settings = settings;
        if settings.get("endpoint").is_none() {
            settings["endpoint"] = json!(h.endpoint());
        }
        let trace_dir = h.dir.path().join("harness-trace");
        let idle_stream = settings
            .as_object_mut()
            .and_then(|m| m.remove("harness_idle_stream"))
            .is_some();
        let encoders: &[&str] = match (settings["mode"] == "share", idle_stream) {
            (true, false) => &[
                "--video-encoder",
                "obs_x264",
                "--audio-encoder",
                "ffmpeg_opus",
                "--stream-output",
            ],
            (true, true) => &[
                "--video-encoder",
                "obs_x264",
                "--audio-encoder",
                "ffmpeg_opus",
            ],
            (false, _) => &["--no-encoders"],
        };
        let mut child = Command::new(&env.harness)
            .args(["--output", "dstreamy_output", "--modules", MODULES])
            .args(encoders)
            .arg("--plugin-bin")
            .arg(&env.plugin_bin)
            .arg("--plugin-data")
            .arg(&env.plugin_data)
            .arg("--trace-dir")
            .arg(&trace_dir)
            .arg("--output-settings")
            .arg(settings.to_string())
            .args(args)
            .current_dir(h.dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("{}: {e}", env.harness.display()));
        let lines = Arc::new(Mutex::new(Vec::new()));
        let mut readers = Vec::new();
        let out: Box<dyn std::io::Read + Send> = Box::new(child.stdout.take().unwrap());
        let err: Box<dyn std::io::Read + Send> = Box::new(child.stderr.take().unwrap());
        for pipe in [out, err] {
            let sink = lines.clone();
            readers.push(std::thread::spawn(move || {
                for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                    eprintln!("obs-harness: {line}");
                    sink.lock().unwrap().push(line);
                }
            }));
        }
        Self {
            child,
            lines,
            readers,
        }
    }

    fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }

    fn finish(mut self) -> (ExitStatus, Vec<String>) {
        let deadline = Instant::now() + HARNESS_EXIT;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                panic!("obs-harness did not exit; output {:#?}", self.lines());
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
        (status, self.lines())
    }
}

impl Drop for HarnessRun {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn passed(status: ExitStatus, lines: &[String]) {
    assert!(
        status.success() && lines.iter().any(|l| l == "HARNESS result=PASS"),
        "obs-harness failed ({status}): {lines:#?}"
    );
}

fn has_line(lines: &[String], prefix: &str) -> bool {
    lines.iter().any(|l| l.starts_with(prefix))
}

fn number_after(lines: &[String], key: &str) -> u64 {
    lines
        .iter()
        .find_map(|l| l.split(key).nth(1)?.split_whitespace().next()?.parse().ok())
        .unwrap_or_else(|| panic!("no {key} in the harness output"))
}

/// An external session at the harness's canvas, streaming at the mock.
fn obs_session(h: &Harness) -> Client {
    let mut c = h.control();
    c.send(&json!({
        "id": "start", "cmd": "start", "guild_id": MOCK_GUILD_ID,
        "channel_id": MOCK_VOICE_CHANNEL_ID, "target": "external",
        "resolution": "1280x720", "fps": OBS_FPS, "bitrate_kbps": 2500,
    }));
    let reply = c.reply("start");
    assert_eq!(reply["type"], "result", "{reply}");
    c.until("streaming", |m| m["event"]["state"]["state"] == "streaming");
    c
}

/// CLOCK_MONOTONIC in ns, the clock of both std `Instant` on Linux and libobs `os_gettime_ns`.
fn monotonic_ns(at: Instant) -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let now = Instant::now();
    // SAFETY: `ts` is a valid out pointer.
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) },
        0
    );
    let now_ns = ts.tv_sec * 1_000_000_000 + ts.tv_nsec;
    now_ns - i64::try_from(now.duration_since(at).as_nanos()).unwrap()
}

fn luma_frames(video: &[&RecordedVideoFrame]) -> Vec<sync::LumaFrame> {
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

/// Flash/beep pairs on `serve`'s timeline; each beep within a quarter frame of half a frame after its flash.
fn markers(video: &[&RecordedVideoFrame], audio: &[&RecordedAudioFrame]) -> Vec<sync::MarkerPair> {
    let frames = luma_frames(video);
    let flashes = sync::flash_onsets(&frames);
    let pcm = pcm(audio);
    let start_us = rtp_us(audio[0].rtp_timestamp, AUDIO_HZ);
    let beeps = sync::beep_onsets(&pcm, start_us, SAMPLE_RATE);
    let frame_us = 1_000_000 / OBS_FPS;
    let video_span = (frames[0].pts_us, frames.last().unwrap().pts_us);
    let audio_span = (
        start_us,
        start_us + sync::samples_to_us(pcm.len(), SAMPLE_RATE),
    );
    let pairs = sync::align(&flashes, &beeps, video_span, audio_span, frame_us)
        .unwrap_or_else(|e| panic!("{e}; flashes {flashes:?}, beeps {beeps:?}"));
    sync::check_offsets(&pairs, frame_us / 2, frame_us / 4)
        .unwrap_or_else(|e| panic!("{e}; pairs {pairs:?}"));
    pairs
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn obs_streams_through_the_plugin_in_sync_and_within_a_second() {
    let h = Harness::new("");
    let _serve = h.serve();
    let mut c = obs_session(&h);
    let run = HarnessRun::start(
        &h,
        json!({}),
        &["--seconds", "90", "--until-video", "300", "--verbose"],
    );
    let (status, lines) = run.finish();
    passed(status, &lines);
    assert!(
        has_plugin_line(&lines, "from the canvas's video"),
        "an NV12 canvas needs no extra GPU mix: {lines:#?}"
    );
    c.source("the attachment ending at end of stream", |e| {
        e["reason"] == "detached"
    });

    let recorded = h.mock.recorded();
    let video = stream_video(&recorded);
    let audio = producer_audio(&recorded);
    assert!(
        video.len() >= 270,
        "{} of the 300 video frames reached the mock",
        video.len()
    );
    assert!(is_keyframe(&video[0].data), "the first video is an IDR");
    increasing(
        "video",
        &video.iter().map(|v| v.rtp_timestamp).collect::<Vec<_>>(),
    );
    increasing(
        "audio",
        &audio.iter().map(|a| a.rtp_timestamp).collect::<Vec<_>>(),
    );
    decodes(&video, &audio);
    let pairs = markers(&video, &audio);
    assert!(pairs.len() >= 7, "only {} flash/beep pairs", pairs.len());

    let first_us = rtp_us(video[0].rtp_timestamp, VIDEO_HZ);
    let flash_frame = |p: &sync::MarkerPair| {
        video
            .iter()
            .find(|v| rtp_us(v.rtp_timestamp, VIDEO_HZ) == p.flash_us)
            .expect("the flash onset is a recorded frame")
    };

    // The plugin logs when each keyframe reached `encoded_packet`; x264's scenecut makes every flash an IDR.
    let keyframes: Vec<(i64, i64)> = lines
        .iter()
        .filter_map(|l| {
            let rest = l.split("[obs-dstreamy] sent keyframe pts_us=").nth(1)?;
            let (pts, arrived) = rest.split_once(" arrived_ns=")?;
            Some((pts.parse().ok()?, arrived.trim().parse().ok()?))
        })
        .collect();
    assert!(!keyframes.is_empty(), "the plugin logged no keyframes");
    let offset_us = first_us - keyframes[0].0;
    let last_us = rtp_us(video.last().unwrap().rtp_timestamp, VIDEO_HZ);
    for (pts, _) in &keyframes {
        let at = pts + offset_us;
        if !(first_us..=last_us).contains(&at) {
            continue;
        }
        assert!(
            video
                .iter()
                .any(|v| (rtp_us(v.rtp_timestamp, VIDEO_HZ) - at).abs() <= 1_000
                    && is_keyframe(&v.data)),
            "the plugin's keyframe at {pts} µs is no IDR at the mock ({at} µs); the first video there is not its first keyframe"
        );
    }
    let encode_to_mock: Vec<i64> = pairs
        .iter()
        .filter_map(|p| {
            let (_, arrived) = keyframes
                .iter()
                .find(|(pts, _)| (pts + offset_us - p.flash_us).abs() <= 1_000)?;
            Some((monotonic_ns(flash_frame(p).received_at) - arrived) / 1_000_000)
        })
        .collect();

    // Each flash is rendered at base + k * period; its pts since the first frame says which k.
    let base = i64::try_from(number_after(&lines, "base_ns=")).unwrap();
    let period = i64::try_from(number_after(&lines, "period_ns=")).unwrap();
    let capture = i64::try_from(number_after(&lines, "capture_start_ns=")).unwrap();
    let render_to_mock: Vec<i64> = pairs
        .iter()
        .map(|p| {
            let estimate = capture + (p.flash_us - first_us) * 1000;
            let k = (estimate - base + period / 2).div_euclid(period);
            (monotonic_ns(flash_frame(p).received_at) - (base + k * period)) / 1_000_000
        })
        .collect();
    eprintln!(
        "per flash, ms: encode-to-mock {encode_to_mock:?}, render-to-mock {render_to_mock:?}"
    );
    assert!(
        encode_to_mock.len() * 2 >= pairs.len(),
        "only {} of {} flashes matched a logged keyframe",
        encode_to_mock.len(),
        pairs.len()
    );
    assert!(
        encode_to_mock.iter().all(|&ms| (0..1000).contains(&ms)),
        "encode-to-mock latency outside [0, 1000) ms: {encode_to_mock:?}"
    );
    assert!(
        render_to_mock.iter().all(|&ms| ms >= 0),
        "a flash reached the mock before OBS rendered it: {render_to_mock:?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_killed_service_stops_the_output_with_an_error_and_obs_keeps_running() {
    let h = Harness::new("");
    let mut serve = h.serve();
    let _c = obs_session(&h);
    let run = HarnessRun::start(
        &h,
        json!({}),
        &[
            "--seconds",
            "40",
            "--until-video",
            "100000",
            "--expect-error",
            "Lost the D-Streamy service",
        ],
    );
    h.wait("60 video frames from OBS", |r| {
        (stream_video(r).len() >= 60).then_some(())
    });
    serve.kill();
    let (status, lines) = run.finish();
    passed(status, &lines);
    assert!(
        has_line(
            &lines,
            "HARNESS expected_error code=-4: Lost the D-Streamy service"
        ),
        "{lines:#?}"
    );
    assert!(
        has_line(&lines, "HARNESS alive after the stop: "),
        "{lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_panic_in_encoded_packet_stops_the_output_once_with_an_error() {
    let h = Harness::new("");
    let _serve = h.serve();
    let mut c = obs_session(&h);
    let run = HarnessRun::start(
        &h,
        json!({"panic_at_packet": 50}),
        &[
            "--seconds",
            "15",
            "--until-video",
            "100000",
            "--expect-panics",
            "1",
            "--expect-error",
            "panic caught in encoded_packet: test panic at packet 50",
        ],
    );
    let (status, lines) = run.finish();
    passed(status, &lines);
    assert!(
        has_line(
            &lines,
            "HARNESS expected_error code=-4: D-Streamy plugin error (panic caught in encoded_packet"
        ),
        "{lines:#?}"
    );
    assert!(has_line(&lines, "HARNESS alive after the stop: "));
    assert!(
        has_line(&lines, "HARNESS stop_signals=1 error_stop_signals=1"),
        "exactly one stop: {lines:#?}"
    );
    c.source("the attachment ending at end of stream", |e| {
        e["reason"] == "detached"
    });
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_panic_in_stop_still_stops_the_output_with_an_error() {
    let h = Harness::new("");
    let _serve = h.serve();
    let mut c = obs_session(&h);
    let run = HarnessRun::start(
        &h,
        json!({"panic_in": "stop"}),
        &[
            "--seconds",
            "60",
            "--until-video",
            "60",
            "--expect-panics",
            "1",
            "--expect-error",
            "panic caught in stop: test panic in Stop",
        ],
    );
    let (status, lines) = run.finish();
    passed(status, &lines);
    assert!(
        has_line(
            &lines,
            "HARNESS expected_error code=-4: D-Streamy plugin error (panic caught in stop"
        ),
        "{lines:#?}"
    );
    c.source("the attachment ending at end of stream", |e| {
        e["reason"] == "detached"
    });
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_stalled_service_cannot_hold_up_the_stop() {
    let h = Harness::new("");
    let serve = h.serve();
    let _c = obs_session(&h);
    let run = HarnessRun::start(&h, json!({}), &["--seconds", "60", "--until-video", "240"]);
    h.wait("60 video frames from OBS", |r| {
        (stream_video(r).len() >= 60).then_some(())
    });
    let pid = libc::pid_t::try_from(serve.pid()).unwrap();
    // SAFETY: kill has no memory preconditions; serve is our unreaped child.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
    let (status, lines) = run.finish();
    // SAFETY: as above.
    unsafe { libc::kill(pid, libc::SIGCONT) };
    passed(status, &lines);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("[obs-dstreamy] the service read nothing for")),
        "the writer never blocked on the stopped service: {lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn releasing_the_output_while_it_connects_returns_promptly() {
    let h = Harness::new("");
    let dir = h.run.path().join("silent");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let path = dir.join("s.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let held = std::thread::spawn(move || listener.accept().map(|(stream, _)| stream));
    let run = HarnessRun::start(
        &h,
        json!({"endpoint": path}),
        &[
            "--seconds",
            "30",
            "--until-video",
            "60",
            "--release-after-start",
            "500",
        ],
    );
    let (status, lines) = run.finish();
    passed(status, &lines);
    let took = number_after(&lines, "released the output during its start in ");
    assert!(took < 2_000, "release took {took} ms");
    assert!(
        has_line(&lines, "HARNESS stop_signals=0 "),
        "no capture began: {lines:#?}"
    );
    drop(held);
}

/// A service that replies `attached` after `reply_ms`, sends `detached` at `detached_ms` and never reads.
fn late_service(
    h: &Harness,
    reply_ms: u64,
    detached_ms: u64,
) -> (PathBuf, std::thread::JoinHandle<()>) {
    use std::io::{BufRead, Write};
    let dir = h.run.path().join("late");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let path = dir.join("s.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let service = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let accepted = Instant::now();
        let mut hello = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut hello)
            .unwrap();
        std::thread::sleep(Duration::from_millis(reply_ms).saturating_sub(accepted.elapsed()));
        stream.write_all(b"{\"attached\":1}\n").unwrap();
        std::thread::sleep(Duration::from_millis(detached_ms).saturating_sub(accepted.elapsed()));
        let _ = stream.write_all(
            b"{\"message\":\"late test\",\"reason\":\"stopped\",\"type\":\"detached\"}\n",
        );
        std::thread::sleep(Duration::from_secs(10));
    });
    (path, service)
}

/// Capture begins after libobs's active check in destroy (the hook delays our flag by 1.5 s).
fn release_during_a_late_capture(detached_ms: u64) -> Vec<String> {
    let h = Harness::new("");
    let (path, _service) = late_service(&h, 800, detached_ms);
    let run = HarnessRun::start(
        &h,
        json!({"endpoint": path, "test_destroy_delay_ms": 1500}),
        &[
            "--seconds",
            "60",
            "--until-video",
            "100000",
            "--bitrate",
            "8000",
            "--release-after-start",
            "500",
            "--expect-error",
            "late test",
            "--verbose",
        ],
    );
    let (status, lines) = run.finish();
    passed(status, &lines);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("[obs-dstreamy] attached to the D-Streamy service")),
        "capture never began during destroy: {lines:#?}"
    );
    assert!(
        has_line(
            &lines,
            "HARNESS output stopped with code -4: The D-Streamy service ended the stream (stopped): late test"
        ),
        "{lines:#?}"
    );
    assert!(
        has_line(&lines, "HARNESS stop_signals=1 error_stop_signals=1"),
        "{lines:#?}"
    );
    lines
}

/// Path coverage only: the missing join can't be observed without ASan.
#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_late_capture_that_ends_before_destroy_takes_the_join_path() {
    release_during_a_late_capture(1_000);
}

/// The writer takes destroy's Shutdown, then the reader's `detached` wins while the writer is blocked.
#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_late_capture_whose_shutdown_is_lost_still_lets_destroy_return() {
    let lines = release_during_a_late_capture(2_600);
    let at = |needle: &str| lines.iter().position(|l| l.contains(needle));
    let stop = at("[obs-dstreamy] stop requested; sending").expect("the writer took the stop");
    let detached = at("The D-Streamy service ended the stream").expect("the reader saw detached");
    assert!(
        stop < detached,
        "the Shutdown was not taken before the detached: {lines:#?}"
    );
}

fn has_plugin_line(lines: &[String], text: &str) -> bool {
    lines
        .iter()
        .any(|l| l.contains("[obs-dstreamy] ") && l.contains(text))
}

fn first_picture_size(video: &[&RecordedVideoFrame]) -> (usize, usize) {
    use openh264::formats::YUVSource;
    let mut decoder = openh264::decoder::Decoder::new().unwrap();
    video
        .iter()
        .find_map(|v| {
            decoder
                .decode(&v.data)
                .ok()
                .flatten()
                .map(|yuv| yuv.dimensions())
        })
        .expect("a picture decodes")
}

fn rtp_steps(video: &[&RecordedVideoFrame]) -> Vec<u32> {
    video
        .windows(2)
        .map(|w| w[1].rtp_timestamp.wrapping_sub(w[0].rtp_timestamp))
        .collect()
}

fn stream(
    settings: Value,
    args: &[&str],
    video: usize,
) -> (Vec<String>, dstreamy_mock_discord::Recorded) {
    let h = Harness::new("");
    let _serve = h.serve();
    let mut c = obs_session(&h);
    let until = video.to_string();
    let mut all = vec!["--seconds", "90", "--until-video", until.as_str()];
    all.extend_from_slice(args);
    let (status, lines) = HarnessRun::start(&h, settings, &all).finish();
    passed(status, &lines);
    c.source("the attachment ending at end of stream", |e| {
        e["reason"] == "detached"
    });
    let recorded = h.mock.recorded();
    {
        let v = stream_video(&recorded);
        let a = producer_audio(&recorded);
        assert!(
            v.len() * 10 >= video * 9,
            "{} of {video} video frames reached the mock",
            v.len()
        );
        decodes(&v, &a);
    }
    (lines, recorded)
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn share_mode_shares_a_stream_encoder_that_passes() {
    let (lines, _) = stream(
        json!({"mode": "share"}),
        &["--bf", "0", "--keyint-sec", "2"],
        90,
    );
    assert!(
        has_plugin_line(&lines, "] sharing the stream's video encoder obs_x264"),
        "{lines:#?}"
    );
    assert!(!has_plugin_line(&lines, "own video encoder"), "{lines:#?}");
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn share_mode_falls_back_to_the_own_encoder_when_b_frames_are_on() {
    let (lines, _) = stream(
        json!({"mode": "share"}),
        &["--bf", "2", "--keyint-sec", "2"],
        90,
    );
    assert!(
        has_plugin_line(
            &lines,
            "not sharing the stream's video encoder obs_x264 (D-Streamy needs B-frames off"
        ),
        "{lines:#?}"
    );
    assert!(
        has_plugin_line(&lines, "own video encoder obs_x264"),
        "{lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn the_own_encoder_scales_a_1440p_canvas_to_1080p() {
    let (lines, recorded) = stream(json!({}), &["--width", "2560", "--height", "1440"], 60);
    assert!(
        has_plugin_line(&lines, "own video encoder obs_x264 at 1920×1080")
            && has_plugin_line(&lines, "from a GPU mix in NV12"),
        "{lines:#?}"
    );
    assert_eq!(first_picture_size(&stream_video(&recorded)), (1920, 1080));
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn the_own_encoder_divides_a_90_fps_canvas_down_to_30() {
    let (lines, recorded) = stream(
        json!({}),
        &["--fps", "90", "--width", "640", "--height", "360"],
        90,
    );
    assert!(
        has_plugin_line(&lines, "own video encoder obs_x264 at 640×360, 30 FPS"),
        "{lines:#?}"
    );
    let steps = rtp_steps(&stream_video(&recorded));
    let at_30 = steps.iter().filter(|&&s| s == 3000).count();
    assert!(
        at_30 * 10 >= steps.len() * 9,
        "RTP steps at 90 kHz: {steps:?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn the_own_encoder_runs_at_60_fps_on_a_60_fps_canvas() {
    let (lines, recorded) = stream(
        json!({}),
        &["--fps", "60", "--width", "640", "--height", "360"],
        120,
    );
    assert!(
        has_plugin_line(&lines, "own video encoder obs_x264 at 640×360, 60 FPS"),
        "{lines:#?}"
    );
    let steps = rtp_steps(&stream_video(&recorded));
    let at_60 = steps.iter().filter(|&&s| s == 1500).count();
    assert!(
        at_60 * 10 >= steps.len() * 9,
        "RTP steps at 90 kHz: {steps:?}"
    );
}

/// Audio is always D-Streamy's own Opus, even when the stream's AAC encoder is attached.
#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn share_mode_replaces_an_attached_aac_encoder_with_opus() {
    let (lines, _) = stream(
        json!({"mode": "share"}),
        &[
            "--bf",
            "0",
            "--keyint-sec",
            "2",
            "--audio-encoder",
            "ffmpeg_aac",
        ],
        90,
    );
    assert!(
        has_plugin_line(
            &lines,
            "replacing the attached audio encoder ffmpeg_aac with D-Streamy's own Opus encoder"
        ),
        "{lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn share_mode_rechecks_the_stream_encoder_after_a_fallback() {
    let (lines, _) = stream(
        json!({"mode": "share"}),
        &[
            "--bf",
            "2",
            "--keyint-sec",
            "2",
            "--restart-video-settings",
            r#"{"bf": 0}"#,
        ],
        60,
    );
    let at = |text: &str| {
        lines
            .iter()
            .position(|l| l.contains("[obs-dstreamy] ") && l.contains(text))
    };
    let fell_back =
        at("not sharing the stream's video encoder obs_x264").expect("first start falls back");
    let restarted = lines
        .iter()
        .position(|l| l == "HARNESS restarting the output")
        .expect("a restart");
    let shared = at("] sharing the stream's video encoder obs_x264").expect("the restart shares");
    assert!(fell_back < restarted && restarted < shared, "{lines:#?}");
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn share_mode_uses_the_own_encoder_while_the_stream_is_idle() {
    let (lines, _) = stream(
        json!({"mode": "share", "harness_idle_stream": true}),
        &["--bf", "0", "--keyint-sec", "2"],
        60,
    );
    assert!(
        has_plugin_line(&lines, "only while streaming"),
        "{lines:#?}"
    );
    assert!(
        has_plugin_line(&lines, "own video encoder obs_x264"),
        "{lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn the_own_encoder_restarts_on_the_same_output() {
    let (lines, _) = stream(json!({}), &["--restart-video-settings", "{}"], 60);
    let own = lines
        .iter()
        .filter(|l| l.contains("[obs-dstreamy] own video encoder obs_x264"))
        .count();
    assert_eq!(own, 2, "{lines:#?}");
}

fn mixer_reports(lines: &[String]) -> Vec<(String, Option<u64>, u64)> {
    lines
        .iter()
        .filter_map(|l| {
            let rest = l.strip_prefix("HARNESS audio_mixer when=")?;
            let (when, rest) = rest.split_once(' ')?;
            let idx = rest
                .strip_prefix("idx=")
                .and_then(|r| r.split_whitespace().next()?.parse().ok());
            let count = rest
                .split("dstreamy_audio_encoders=")
                .nth(1)?
                .trim()
                .parse()
                .ok()?;
            Some((when.to_owned(), idx, count))
        })
        .collect()
}

fn mixer_for_track(track: i64) -> u64 {
    let (lines, _) = stream(json!({"audio_track": track}), &[], 60);
    let reports = mixer_reports(&lines);
    assert_eq!(reports.len(), 1, "{lines:#?}");
    assert_eq!(reports[0].0, "start");
    assert_eq!(reports[0].2, 1, "{lines:#?}");
    reports[0].1.expect("an audio encoder is attached")
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn audio_track_1_binds_mixer_0() {
    assert_eq!(mixer_for_track(1), 0);
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn audio_track_6_binds_mixer_5() {
    assert_eq!(mixer_for_track(6), 5);
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn an_invalid_audio_track_fails_before_any_audio_encoder_exists() {
    for track in [0, 7] {
        let h = Harness::new("");
        let run = HarnessRun::start(
            &h,
            json!({"audio_track": track}),
            &[
                "--seconds",
                "3",
                "--until-video",
                "100000",
                "--expect-error",
                "audio track must be 1 to 6",
            ],
        );
        let (status, lines) = run.finish();
        passed(status, &lines);
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("HARNESS expected_error start: ")
                    && l.contains(&format!("not {track}"))),
            "{lines:#?}"
        );
        assert_eq!(
            mixer_reports(&lines),
            vec![("start_failed".to_owned(), None, 0)],
            "{lines:#?}"
        );
    }
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn changing_the_audio_track_while_inactive_recreates_the_encoder_on_the_next_start() {
    let (lines, _) = stream(
        json!({"audio_track": 1}),
        &["--restart-output-settings", r#"{"audio_track": 2}"#],
        60,
    );
    let reports = mixer_reports(&lines);
    assert_eq!(
        reports
            .iter()
            .map(|r| (r.0.as_str(), r.1, r.2))
            .collect::<Vec<_>>(),
        vec![("start", Some(0), 1), ("restart", Some(1), 1)],
        "{lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn an_unchanged_audio_track_keeps_the_cached_encoder_across_starts() {
    let (lines, _) = stream(
        json!({"audio_track": 3}),
        &["--restart-output-settings", "{}"],
        60,
    );
    let ptrs: Vec<&str> = lines
        .iter()
        .filter(|l| l.starts_with("HARNESS audio_mixer when="))
        .filter_map(|l| l.split("encoder=").nth(1)?.split_whitespace().next())
        .collect();
    assert_eq!(ptrs.len(), 2, "{lines:#?}");
    assert_eq!(ptrs[0], ptrs[1], "{lines:#?}");
    assert!(
        mixer_reports(&lines).iter().all(|r| r.1 == Some(2)),
        "{lines:#?}"
    );
}

fn report_ui(extra: &[&str]) -> Vec<String> {
    let h = Harness::new("");
    let mut args = vec![
        "--seconds",
        "3",
        "--until-video",
        "100000",
        "--report-ui",
        "--expect-error",
        "The D-Streamy service is not running",
    ];
    args.extend_from_slice(extra);
    let (status, lines) = HarnessRun::start(&h, json!({}), &args).finish();
    assert!(
        status.success() && has_line(&lines, "HARNESS expected_error "),
        "obs-harness failed ({status}): {lines:#?}"
    );
    lines
}

fn harness_lib(name: &str) -> String {
    let lib = obs_env().harness.with_file_name(name);
    assert!(lib.exists(), "{} is not built", lib.display());
    lib.to_string_lossy().into_owned()
}

fn fake_frontend() -> String {
    harness_lib("obs-harness-frontend.so")
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn without_a_frontend_the_plugin_loads_and_offers_no_tools_entry() {
    let lines = report_ui(&[]);
    assert!(
        has_plugin_line(&lines, "no OBS frontend: Tools > D-Streamy is unavailable"),
        "{lines:#?}"
    );
    assert!(
        !has_plugin_line(&lines, "added Tools > D-Streamy"),
        "{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l == "HARNESS settings_source registered=1 listed=0 flags=0x400"),
        "{lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn the_output_has_no_audio_track_property_but_keeps_track_1_as_its_default() {
    let lines = report_ui(&[]);
    let props: Vec<&str> = lines
        .iter()
        .filter_map(|l| l.strip_prefix("HARNESS output_property name="))
        .collect();
    assert!(props.contains(&"mode"), "{lines:#?}");
    assert!(
        props
            .iter()
            .all(|p| !p.contains("audio_track") && !p.contains("label")),
        "{props:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l == "HARNESS output_setting audio_track=1"),
        "{lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn tools_d_streamy_opens_the_private_settings_source_through_the_frontend() {
    let lib = fake_frontend();
    let lines = report_ui(&["--fake-frontend", lib.as_str()]);
    assert!(
        has_plugin_line(&lines, "added Tools > D-Streamy"),
        "{lines:#?}"
    );
    for want in [
        "HARNESS frontend tools_items=1 name=D-Streamy\u{2026}",
        "HARNESS frontend opened id=dstreamy_settings public=0 flags=0x400",
        "HARNESS frontend held expired=0",
        "HARNESS frontend closed expired=1",
        "HARNESS settings_source registered=1 listed=0 flags=0x400",
    ] {
        assert!(lines.iter().any(|l| l == want), "{want}: {lines:#?}");
    }
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_frontend_without_a_properties_dialog_offers_no_tools_entry() {
    let lib = harness_lib("obs-harness-frontend-incomplete.so");
    let lines = report_ui(&["--fake-frontend", lib.as_str()]);
    assert!(
        has_plugin_line(
            &lines,
            "OBS frontend lacks obs_frontend_open_source_properties: Tools > D-Streamy is unavailable"
        ),
        "{lines:#?}"
    );
    assert!(!has_plugin_line(&lines, "no OBS frontend"), "{lines:#?}");
    assert!(
        lines
            .iter()
            .any(|l| l == "HARNESS frontend tools_items=0 name="),
        "{lines:#?}"
    );
}

const IDLE_STATUS: &str = "Idle \u{2014} start controls are unavailable";
const ROUTING_HELP: &str = "D-Streamy sends this OBS mixer track to Discord. New OBS sources feed all tracks \
     until you change them in Edit \u{203a} Advanced Audio Properties \u{203a} Tracks. This does not \
     change OBS's stream or recording. A change applies on the next Go Live.";

fn module_config(dir: &std::path::Path) -> PathBuf {
    dir.join("obs-dstreamy").join("config.json")
}

fn write_module_config(dir: &std::path::Path, config: Value) {
    let path = module_config(dir);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, config.to_string()).unwrap();
}

fn read_module_config(dir: &std::path::Path) -> Value {
    let path = module_config(dir);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap()
}

fn settings_lines<'a>(lines: &'a [String], when: &str) -> Vec<&'a str> {
    let prefix = format!(" when={when} ");
    lines
        .iter()
        .filter(|l| l.starts_with("HARNESS settings_") && l.contains(&prefix))
        .map(String::as_str)
        .collect()
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn the_settings_dialog_offers_tracks_1_to_6_routing_help_and_no_working_start() {
    let config = tempfile::tempdir().unwrap();
    let dir = config.path().to_string_lossy().into_owned();
    let lib = fake_frontend();
    let lines = report_ui(&[
        "--fake-frontend",
        lib.as_str(),
        "--module-config",
        dir.as_str(),
    ]);
    let mut want = vec![
        format!("HARNESS settings_property when=open name=status type=4 enabled=1 desc={IDLE_STATUS}"),
        "HARNESS settings_info when=open name=status kind=0".to_owned(),
        "HARNESS settings_property when=open name=audio_track type=6 enabled=1 desc=Discord audio track"
            .to_owned(),
    ];
    for track in 1..=6 {
        want.push(format!(
            "HARNESS settings_item when=open name=audio_track value={track} text=Track {track}"
        ));
    }
    want.extend([
        format!(
            "HARNESS settings_property when=open name=routing type=4 enabled=1 desc={ROUTING_HELP}"
        ),
        "HARNESS settings_info when=open name=routing kind=0".to_owned(),
        "HARNESS settings_property when=open name=start type=8 enabled=0 desc=Start".to_owned(),
        "HARNESS settings_property when=open name=stop type=8 enabled=0 desc=Stop".to_owned(),
        "HARNESS settings_source when=open audio_track=1".to_owned(),
    ]);
    assert_eq!(settings_lines(&lines, "open"), want, "{lines:#?}");
    assert!(
        !lines.iter().any(|l| l.to_lowercase().contains("label")),
        "{lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_track_chosen_in_the_dialog_is_saved_to_the_module_config_and_reopens() {
    let config = tempfile::tempdir().unwrap();
    let owned = json!({"connections": [{"id": 4, "fps": 30}], "guild_id": "g", "channel_id": "c", "instance": null});
    write_module_config(
        config.path(),
        json!({"audio_track": 3, "guild_id": "g", "bitrate_kbps": 2500, "owned": owned}),
    );
    let dir = config.path().to_string_lossy().into_owned();
    let lib = fake_frontend();
    let lines = report_ui(&[
        "--fake-frontend",
        lib.as_str(),
        "--module-config",
        dir.as_str(),
        "--settings-update",
        r#"{"audio_track": 5}"#,
    ]);
    for want in [
        "HARNESS settings_source when=open audio_track=3",
        "HARNESS settings_source when=updated audio_track=5",
        "HARNESS settings_source when=reopened audio_track=5",
    ] {
        assert!(lines.iter().any(|l| l == want), "{want}: {lines:#?}");
    }
    assert!(
        lines
            .iter()
            .any(|l| *l == format!("HARNESS settings_modified refresh=1 status={IDLE_STATUS}")),
        "{lines:#?}"
    );
    assert_eq!(
        lines
            .iter()
            .filter(|l| *l == "HARNESS frontend closed expired=1")
            .count(),
        2,
        "{lines:#?}"
    );
    let saved = read_module_config(config.path());
    assert_eq!(saved["audio_track"], 5, "{saved}");
    assert_eq!(saved["guild_id"], "g", "{saved}");
    assert_eq!(saved["bitrate_kbps"], 2500, "{saved}");
    assert_eq!(saved["owned"], owned, "{saved}");
    assert!(!saved.to_string().contains("label"), "{saved}");
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn an_out_of_range_track_from_the_dialog_is_refused_and_not_saved() {
    let config = tempfile::tempdir().unwrap();
    write_module_config(config.path(), json!({"audio_track": 3}));
    let dir = config.path().to_string_lossy().into_owned();
    let lib = fake_frontend();
    let lines = report_ui(&[
        "--fake-frontend",
        lib.as_str(),
        "--module-config",
        dir.as_str(),
        "--settings-update",
        r#"{"audio_track": 9}"#,
    ]);
    assert!(
        has_plugin_line(
            &lines,
            "ignored Discord audio track 9: pick Track 1 to Track 6"
        ),
        "{lines:#?}"
    );
    for want in [
        "HARNESS settings_source when=updated audio_track=3",
        "HARNESS settings_source when=reopened audio_track=3",
    ] {
        assert!(lines.iter().any(|l| l == want), "{want}: {lines:#?}");
    }
    assert_eq!(read_module_config(config.path())["audio_track"], 3);
}

fn status_line<'a>(lines: &'a [String], when: &str) -> &'a str {
    let prefix =
        format!("HARNESS settings_property when={when} name=status type=4 enabled=1 desc=");
    lines
        .iter()
        .find_map(|l| l.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("no {when} status: {lines:#?}"))
}

fn temp_files(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok()?.file_name().into_string().ok())
                .filter(|n| n.ends_with(".tmp"))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_corrupt_module_config_is_reported_and_never_overwritten() {
    let bad_json: &[u8] = b"{\"audio_track\": 3, \"owned\": oops";
    let not_utf8: &[u8] = b"{\"audio_track\": 3, \"guild_id\": \"\xff\xfe\"}";
    for garbage in [bad_json, not_utf8] {
        corrupt_module_config_is_kept(garbage);
    }
}

fn corrupt_module_config_is_kept(garbage: &[u8]) {
    let config = tempfile::tempdir().unwrap();
    let path = module_config(config.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, garbage).unwrap();
    let dir = config.path().to_string_lossy().into_owned();
    let lib = fake_frontend();
    let lines = report_ui(&[
        "--fake-frontend",
        lib.as_str(),
        "--module-config",
        dir.as_str(),
        "--settings-update",
        r#"{"audio_track": 5}"#,
    ]);
    let read_error = format!(
        "Couldn't read D-Streamy's settings from {}: not valid JSON (",
        path.display()
    );
    for when in ["open", "updated", "reopened"] {
        let status = status_line(&lines, when);
        assert!(status.starts_with(&read_error), "{when}: {status}");
        assert!(
            lines
                .iter()
                .any(|l| *l == format!("HARNESS settings_info when={when} name=status kind=2")),
            "{when}: {lines:#?}"
        );
    }
    assert!(
        status_line(&lines, "reopened").ends_with(
            "Fix or delete the file; until then Track 5 is used until OBS quits but is not saved"
        ),
        "{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l == "HARNESS settings_source when=reopened audio_track=5"),
        "{lines:#?}"
    );
    assert!(has_plugin_line(&lines, &read_error), "{lines:#?}");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        garbage,
        "the file is untouched"
    );
    assert_eq!(temp_files(path.parent().unwrap()), Vec::<String>::new());
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_track_that_could_not_be_saved_survives_reopening_with_its_error() {
    for saved in [Some(json!({"audio_track": 1, "guild_id": "g"})), None] {
        let config = tempfile::tempdir().unwrap();
        let folder = config.path().join("obs-dstreamy");
        std::fs::create_dir_all(&folder).unwrap();
        if let Some(saved) = &saved {
            write_module_config(config.path(), saved.clone());
        }
        let dir = config.path().to_string_lossy().into_owned();
        let folder_arg = folder.to_string_lossy().into_owned();
        let lib = fake_frontend();
        let lines = report_ui(&[
            "--fake-frontend",
            lib.as_str(),
            "--module-config",
            dir.as_str(),
            "--replace-before-update",
            folder_arg.as_str(),
            "--settings-update",
            r#"{"audio_track": 4}"#,
        ]);
        let save_error = format!(
            "Couldn't save D-Streamy's settings to {}: ",
            module_config(config.path()).display()
        );
        for when in ["updated", "reopened"] {
            let status = status_line(&lines, when);
            assert!(
                status.starts_with(&save_error),
                "{saved:?} {when}: {status}"
            );
            assert!(
                status.ends_with("; Track 4 is used until OBS quits"),
                "{saved:?} {when}: {status}"
            );
            assert!(
                lines
                    .iter()
                    .any(|l| *l == format!("HARNESS settings_info when={when} name=status kind=2")),
                "{when}: {lines:#?}"
            );
        }
        assert!(
            lines
                .iter()
                .any(|l| l == "HARNESS settings_source when=reopened audio_track=4"),
            "{saved:?}: {lines:#?}"
        );
        assert!(has_plugin_line(&lines, &save_error), "{lines:#?}");
        let moved = config.path().join("obs-dstreamy.saved");
        match &saved {
            Some(saved) => assert_eq!(
                &serde_json::from_str::<Value>(
                    &std::fs::read_to_string(moved.join("config.json")).unwrap()
                )
                .unwrap(),
                saved
            ),
            None => assert!(!moved.join("config.json").exists()),
        }
        assert_eq!(temp_files(&moved), Vec::<String>::new());
    }
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn without_a_module_config_folder_the_dialog_says_the_track_is_not_saved() {
    let lib = fake_frontend();
    let lines = report_ui(&[
        "--fake-frontend",
        lib.as_str(),
        "--settings-update",
        r#"{"audio_track": 2}"#,
    ]);
    let error = "OBS gave D-Streamy no settings folder, so Track 2 is used until OBS quits but is not saved";
    assert!(
        lines.iter().any(|l| *l
            == format!(
                "HARNESS settings_property when=updated name=status type=4 enabled=1 desc={error}"
            )),
        "{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l == "HARNESS settings_info when=updated name=status kind=2"),
        "{lines:#?}"
    );
    assert!(has_plugin_line(&lines, error), "{lines:#?}");
    assert!(
        lines
            .iter()
            .any(|l| l == "HARNESS settings_source when=reopened audio_track=2"),
        "{lines:#?}"
    );
    assert!(lines.iter().all(|l| !l.contains("relative")), "{lines:#?}");
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_track_chosen_in_the_dialog_while_live_never_touches_the_active_output() {
    let config = tempfile::tempdir().unwrap();
    let dir = config.path().to_string_lossy().into_owned();
    let lib = fake_frontend();
    let (lines, _) = stream(
        json!({"audio_track": 1}),
        &[
            "--fake-frontend",
            lib.as_str(),
            "--module-config",
            dir.as_str(),
            "--settings-update-while-active",
            r#"{"audio_track": 4}"#,
        ],
        60,
    );
    assert!(
        lines
            .iter()
            .any(|l| l == "HARNESS output_audio_track when=after_settings_update active=1 track=1"),
        "{lines:#?}"
    );
    let encoder = |when: &str| {
        lines
            .iter()
            .find_map(|l| {
                l.strip_prefix(&format!("HARNESS audio_mixer when={when} "))?
                    .split("encoder=")
                    .nth(1)?
                    .split_whitespace()
                    .next()
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| panic!("no {when} mixer report: {lines:#?}"))
    };
    assert_eq!(encoder("start"), encoder("after_settings_update"));
    assert_eq!(
        mixer_reports(&lines)
            .iter()
            .map(|r| (r.0.as_str(), r.1))
            .collect::<Vec<_>>(),
        [("start", Some(0)), ("after_settings_update", Some(0))],
        "{lines:#?}"
    );
    assert_eq!(read_module_config(config.path())["audio_track"], 4);
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_track_changed_on_an_active_output_is_kept_and_applies_on_the_next_start() {
    let (lines, _) = stream(
        json!({"audio_track": 1}),
        &[
            "--update-output-while-active",
            r#"{"audio_track": 4}"#,
            "--restart-output-settings",
            "{}",
        ],
        60,
    );
    assert!(
        lines
            .iter()
            .any(|l| l == "HARNESS output_audio_track when=after_active_update active=1 track=4"),
        "{lines:#?}"
    );
    let ptrs: Vec<(&str, &str)> = lines
        .iter()
        .filter(|l| l.starts_with("HARNESS audio_mixer when="))
        .filter_map(|l| {
            let when = l
                .strip_prefix("HARNESS audio_mixer when=")?
                .split(' ')
                .next()?;
            Some((when, l.split("encoder=").nth(1)?.split_whitespace().next()?))
        })
        .collect();
    assert_eq!(
        ptrs.iter().map(|p| p.0).collect::<Vec<_>>(),
        ["start", "after_active_update", "restart"],
        "{lines:#?}"
    );
    assert_eq!(ptrs[0].1, ptrs[1].1, "the active encoder is untouched");
    assert_eq!(
        mixer_reports(&lines)
            .iter()
            .map(|r| (r.0.as_str(), r.1))
            .collect::<Vec<_>>(),
        [
            ("start", Some(0)),
            ("after_active_update", Some(0)),
            ("restart", Some(3))
        ],
        "{lines:#?}"
    );
}

/// profile_idc, the byte after the SPS NAL header (H.264 7.3.2.1.1).
fn sps_profile(video: &[&RecordedVideoFrame]) -> u8 {
    video
        .iter()
        .find_map(|v| {
            v.data
                .windows(5)
                .find(|w| w[..3] == [0, 0, 1] && w[3] & 0x1f == 7)
                .map(|w| w[4])
        })
        .expect("an SPS reached the mock")
}

/// 90 frames like the share runs: at 60, one loaded run missed the 90% floor by a frame.
#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn the_own_encoder_sends_4_2_0_from_an_i444_canvas() {
    let (lines, recorded) = stream(json!({}), &["--format", "I444"], 90);
    assert!(
        has_plugin_line(&lines, "from a GPU mix in NV12"),
        "{lines:#?}"
    );
    let profile = sps_profile(&stream_video(&recorded));
    assert!(
        [66, 77, 100].contains(&profile),
        "profile_idc {profile}: not 8-bit 4:2:0"
    );
}

/// Plugin logic only: the frontend itself prefers NV12 for its encoder on an I444 canvas.
#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn share_mode_falls_back_from_an_i444_stream_encoder() {
    let (lines, _) = stream(
        json!({"mode": "share"}),
        &["--bf", "0", "--keyint-sec", "2", "--format", "I444"],
        90,
    );
    assert!(
        has_plugin_line(
            &lines,
            "not I444: Settings › Advanced › Color Format), so D-Streamy uses its own"
        ),
        "{lines:#?}"
    );
}

/// A resize from an I420 canvas must still feed NV12, which texture encoders need to stay on the GPU.
#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn the_own_encoder_resizes_an_i420_canvas_in_nv12() {
    let (lines, recorded) = stream(
        json!({}),
        &["--format", "I420", "--width", "2560", "--height", "1440"],
        60,
    );
    assert!(
        has_plugin_line(&lines, "at 1920×1080")
            && has_plugin_line(&lines, "from a GPU mix in NV12"),
        "{lines:#?}"
    );
    assert_eq!(first_picture_size(&stream_video(&recorded)), (1920, 1080));
}
