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
        has_plugin_line(&lines, "own video encoder obs_x264 at 1920×1080"),
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

/// PRD decision 9: audio is always D-Streamy's own Opus, even when the stream's AAC encoder is attached.
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
