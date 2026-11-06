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
        Self::start_with_env(h, settings, args, &[])
    }

    fn start_with_env(
        h: &Harness,
        settings: Value,
        args: &[&str],
        vars: &[(&str, &std::path::Path)],
    ) -> Self {
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
            .env("XDG_RUNTIME_DIR", runtime_dir(h))
            .envs(vars.iter().copied())
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

/// The harness's runtime directory, so the plugin's default service endpoint is known and private to the test.
/// Not under /tmp: the endpoint's folders must be writable by no one else.
fn runtime_dir(h: &Harness) -> PathBuf {
    use std::os::unix::fs::DirBuilderExt;
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(h.run.path().file_name().unwrap());
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&dir)
            .unwrap();
    }
    dir
}

fn passed(status: ExitStatus, lines: &[String]) {
    assert!(
        status.success() && lines.iter().any(|l| l == "HARNESS result=PASS"),
        "obs-harness failed ({status}): {lines:#?}"
    );
}

/// The dialog's `name` (start or stop) asked a question starting with `question` and changed nothing itself.
fn asked_first(lines: &[String], name: &str, output_active: u8, question: &str) {
    let prefix = format!(
        "HARNESS settings_asked name={name} output_active={output_active} question={question}"
    );
    assert!(
        lines.iter().any(|l| l.starts_with(&prefix)),
        "{prefix}: {lines:#?}"
    );
    let clicked = lines
        .iter()
        .find(|l| l.starts_with(&format!("HARNESS settings_clicked name={name} enabled=1 ")))
        .unwrap_or_else(|| panic!("no {name} click: {lines:#?}"));
    assert!(
        !clicked.contains(" status=Starting") && !clicked.contains(" status=Stopping"),
        "{name} alone acts on nothing: {clicked}"
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
    stream_with_minimum_mock_video(settings, args, video, video * 9 / 10)
}

/// A GPU rescale can lag OBS's render loop, so only half of the plugin's frames must reach the mock.
fn rescaled_stream(
    settings: Value,
    args: &[&str],
    video: usize,
) -> (Vec<String>, dstreamy_mock_discord::Recorded) {
    stream_with_minimum_mock_video(settings, args, video, video.div_ceil(2))
}

fn stream_with_minimum_mock_video(
    settings: Value,
    args: &[&str],
    video: usize,
    minimum_mock_video: usize,
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
            v.len() >= minimum_mock_video,
            "{} of at least {minimum_mock_video} video frames reached the mock",
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
    let (lines, recorded) =
        rescaled_stream(json!({}), &["--width", "2560", "--height", "1440"], 60);
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

/// The status while no service runs at the harness's default endpoint.
const NO_SERVICE: &str = "The D-Streamy service is not running (no service is listening at ";
const TAKE_OVER_HELP: &str =
    "Discord would hear every source on Track 6. Manage Track 6 to pick sources here.";
const TAKE_OVER_EFFECT: &str =
    "Turns Track 6 off on every audio source, also for recordings and streams that use it.";
/// The dialog's status while the unmanaged Track 6 carries the harness's Mic.
const MIC_GUARD: &str = "Mic is on Track 6, so Discord would hear it. Manage Track 6, \
     or turn Track 6 off for Mic in Edit \u{203a} Advanced Audio Properties.";

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
fn the_settings_dialog_offers_a_server_a_channel_tracks_1_to_6_and_start_while_idle() {
    let config = tempfile::tempdir().unwrap();
    let dir = config.path().to_string_lossy().into_owned();
    let lib = fake_frontend();
    let lines = report_ui(&[
        "--fake-frontend",
        lib.as_str(),
        "--module-config",
        dir.as_str(),
    ]);
    let status = status_line(&lines, "open");
    assert!(status.starts_with(NO_SERVICE), "{status}");
    let mut want = vec![
        format!("HARNESS settings_property when=open name=status type=4 enabled=1 desc={status}"),
        "HARNESS settings_info when=open name=status kind=2".to_owned(),
        "HARNESS settings_property when=open name=guild_id type=6 enabled=1 desc=Discord server"
            .to_owned(),
        "HARNESS settings_item when=open name=guild_id value=0 text=Pick a server".to_owned(),
        "HARNESS settings_property when=open name=channel_id type=6 enabled=1 desc=Voice channel"
            .to_owned(),
        "HARNESS settings_item when=open name=channel_id value=0 text=Pick a voice channel"
            .to_owned(),
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
            "HARNESS settings_property when=open name=audio_help type=4 enabled=1 desc={TAKE_OVER_HELP}"
        ),
        "HARNESS settings_info when=open name=audio_help kind=0".to_owned(),
        "HARNESS settings_property when=open name=take_over type=8 enabled=1 desc=Manage Track 6"
            .to_owned(),
        "HARNESS settings_property when=open name=start type=8 enabled=1 desc=Start".to_owned(),
        "HARNESS settings_property when=open name=stop type=8 enabled=0 desc=Stop".to_owned(),
        "HARNESS settings_source when=open audio_track=6".to_owned(),
    ]);
    assert_eq!(settings_lines(&lines, "open"), want, "{lines:#?}");
    assert!(
        lines.contains(&"HARNESS check_settings when=close differ=0".to_owned()),
        "an unchanged close asks nothing: {lines:#?}"
    );
    assert!(
        !lines.iter().any(|l| l.to_lowercase().contains("label")),
        "{lines:#?}"
    );
}

/// Runs the harness's mixer session: sources Music and Voice on all six tracks, takeover, tick and untick Music.
fn mixer_session(saved: Option<Value>) -> (Vec<String>, Value) {
    let config = tempfile::tempdir().unwrap();
    if let Some(saved) = saved {
        write_module_config(config.path(), saved);
    }
    let dir = config.path().to_string_lossy().into_owned();
    let lib = fake_frontend();
    let lines = report_ui(&[
        "--fake-frontend",
        lib.as_str(),
        "--module-config",
        dir.as_str(),
        "--mixer-session",
    ]);
    (lines, read_module_config(config.path()))
}

fn assert_lines(lines: &[String], want: &[String]) {
    for want in want {
        assert!(lines.iter().any(|l| l == want), "{want}: {lines:#?}");
    }
}

/// The lines between the mixer session's `step` marker and the next marker.
fn mixer_step<'a>(lines: &'a [String], step: &str) -> &'a [String] {
    let marker = format!("HARNESS mixer_step name={step}");
    let start = lines
        .iter()
        .position(|l| *l == marker)
        .unwrap_or_else(|| panic!("no {marker}: {lines:#?}"))
        + 1;
    let end = lines[start..]
        .iter()
        .position(|l| l.starts_with("HARNESS mixer_step "))
        .map_or(lines.len(), |n| start + n);
    &lines[start..end]
}

/// The dialog never rebuilds inside its own callbacks, and an outside mixer change shows once libobs stored it.
fn assert_dialog_follows(lines: &[String]) {
    assert!(
        lines.contains(&"HARNESS mixer_sync_reloads count=0".to_owned()),
        "no rebuild inside a click or a pick: {lines:#?}"
    );
    assert!(
        mixer_step(lines, "track5").contains(
            &"HARNESS settings_property when=view name=take_over type=8 enabled=1 desc=Manage Track 5"
                .to_owned()
        ),
        "a picked track shows its own takeover: {lines:#?}"
    );
    let voice = mixer_step(lines, "external")
        .iter()
        .rfind(|l| l.starts_with("HARNESS settings_checkbox when=view desc=Voice "));
    assert_eq!(
        voice.map(String::as_str),
        Some("HARNESS settings_checkbox when=view desc=Voice on=1"),
        "the last rebuild shows the stored flags: {lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn taking_over_track_6_clears_it_on_every_source_and_a_tick_sets_only_that_bit() {
    let (lines, saved) = mixer_session(None);
    assert_lines(
        &lines,
        &[
            "HARNESS mixers when=start name=Music mixers=63",
            "HARNESS mixers when=start name=Voice mixers=63",
            "HARNESS settings_property when=unmanaged name=take_over type=8 enabled=1 desc=Manage Track 6",
            "HARNESS settings_click_sync_refreshes name=take_over count=0",
            "HARNESS mixers when=confirming name=Music mixers=63",
            "HARNESS mixers when=confirming name=Voice mixers=63",
            "HARNESS settings_property when=confirming name=confirm_take_over type=8 enabled=1 desc=Manage",
            "HARNESS settings_property when=confirming name=cancel_take_over type=8 enabled=1 desc=Not now",
            "HARNESS settings_click_sync_refreshes name=confirm_take_over count=0",
            "HARNESS mixers when=taken name=Music mixers=31",
            "HARNESS mixers when=taken name=Voice mixers=31",
            "HARNESS settings_checkbox when=managed desc=Music on=0",
            "HARNESS settings_checkbox when=managed desc=Voice on=0",
            "HARNESS mixers when=ticked name=Music mixers=63",
            "HARNESS mixers when=ticked name=Voice mixers=31",
            "HARNESS settings_checkbox when=ticked desc=Music on=1",
            "HARNESS settings_checkbox when=ticked desc=Voice on=0",
            "HARNESS mixers when=unticked name=Music mixers=31",
            "HARNESS mixers when=unticked name=Voice mixers=31",
            "HARNESS mixers when=type_level name=Music mixers=63",
            "HARNESS mixers when=created name=Late mixers=223",
            "HARNESS mixers when=loaded name=Saved mixers=63",
        ]
        .map(str::to_owned),
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("HARNESS settings_clicked name=take_over enabled=1 refresh=0 ")),
        "{lines:#?}"
    );
    assert!(
        lines.contains(&format!(
            "HARNESS settings_property when=confirming name=take_over_effect type=4 enabled=1 desc={TAKE_OVER_EFFECT}"
        )),
        "the takeover waits for its confirm: {lines:#?}"
    );
    assert!(
        !lines
            .iter()
            .any(|l| l.starts_with("HARNESS settings_property when=confirming name=take_over ")),
        "{lines:#?}"
    );
    assert!(
        !lines
            .iter()
            .any(|l| l.starts_with("HARNESS settings_checkbox when=unmanaged")),
        "no checklist before the takeover: {lines:#?}"
    );
    assert_dialog_follows(&lines);
    assert!(
        lines.contains(&"HARNESS mixers when=external name=Voice mixers=63".to_owned()),
        "{lines:#?}"
    );
    assert_eq!(
        saved.get("managed_track"),
        None,
        "kept per scene collection: {saved}"
    );
    assert_eq!(saved["audio_track"], 6, "{saved}");
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_saved_track_3_stays_and_is_taken_over_on_its_own_bit() {
    let (lines, saved) = mixer_session(Some(json!({"audio_track": 3, "guild_id": "g"})));
    assert_lines(
        &lines,
        &[
            "HARNESS settings_source when=unmanaged audio_track=3",
            "HARNESS settings_property when=unmanaged name=take_over type=8 enabled=1 desc=Manage Track 3",
            "HARNESS mixers when=taken name=Music mixers=59",
            "HARNESS mixers when=taken name=Voice mixers=59",
            "HARNESS mixers when=ticked name=Music mixers=63",
            "HARNESS mixers when=ticked name=Voice mixers=59",
            "HARNESS mixers when=type_level name=Music mixers=63",
            "HARNESS mixers when=created name=Late mixers=251",
            "HARNESS mixers when=loaded name=Saved mixers=63",
        ]
        .map(str::to_owned),
    );
    assert_dialog_follows(&lines);
    assert_eq!(saved["audio_track"], 3, "{saved}");
    assert_eq!(saved.get("managed_track"), None, "{saved}");
    assert_eq!(saved["guild_id"], "g", "{saved}");
}

/// Runs a harness mode on the settings dialog with the fake frontend; also returns the module config, if saved.
fn dialog_session(mode: &str) -> (Vec<String>, Option<Value>) {
    let config = tempfile::tempdir().unwrap();
    let dir = config.path().to_string_lossy().into_owned();
    let lib = fake_frontend();
    let lines = report_ui(&[
        "--fake-frontend",
        lib.as_str(),
        "--module-config",
        dir.as_str(),
        mode,
    ]);
    let saved = module_config(config.path())
        .exists()
        .then(|| read_module_config(config.path()));
    (lines, saved)
}

fn start_button(lines: &[String], when: &str) -> Option<bool> {
    let prefix = format!("HARNESS settings_property when={when} name=start type=8 enabled=");
    lines
        .iter()
        .rfind(|l| l.starts_with(&prefix))
        .map(|l| l[prefix.len()..].starts_with('1'))
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn the_managed_track_belongs_to_its_scene_collection_and_loads_never_touch_mixers() {
    let (lines, saved) = dialog_session("--collection-session");
    assert_lines(
        &lines,
        &[
            "HARNESS mixers when=a_loaded name=Mic mixers=63",
            "HARNESS mixers when=a_loaded name=Music mixers=63",
            "HARNESS settings_property when=a_unmanaged name=take_over type=8 enabled=1 desc=Manage Track 6",
            "HARNESS settings_info when=a_unmanaged name=status kind=1",
            "HARNESS mixers when=a_confirming name=Mic mixers=63",
            "HARNESS settings_property when=a_confirming name=cancel_take_over type=8 enabled=1 desc=Not now",
            "HARNESS mixers when=a_not_now name=Mic mixers=63",
            "HARNESS mixers when=a_not_now name=Music mixers=63",
            "HARNESS settings_property when=a_not_now name=take_over type=8 enabled=1 desc=Manage Track 6",
            "HARNESS mixers when=a_taken name=Mic mixers=31",
            "HARNESS mixers when=a_taken name=Music mixers=31",
            "HARNESS settings_checkbox when=a_managed desc=Mic on=0",
            "HARNESS mixers when=a_ticked name=Mic mixers=31",
            "HARNESS mixers when=a_ticked name=Music mixers=63",
            "HARNESS collection_saved name=A managed_mixers=32",
            "HARNESS collection_loaded name=B",
            "HARNESS mixers when=b_loaded name=Mic mixers=63",
            "HARNESS mixers when=b_loaded name=Music mixers=63",
            "HARNESS settings_property when=b name=take_over type=8 enabled=1 desc=Manage Track 6",
            "HARNESS check_settings when=b differ=0",
            "HARNESS settings_property when=b_confirming name=confirm_take_over type=8 enabled=1 desc=Manage",
            "HARNESS collection_saved name=B managed_mixers=-1",
            "HARNESS collection_loaded name=C",
            "HARNESS mixers when=c_loaded name=Mic mixers=63",
            "HARNESS settings_property when=c name=take_over type=8 enabled=1 desc=Manage Track 6",
            "HARNESS mixers when=c_after name=Mic mixers=63",
            "HARNESS mixers when=c_after name=Music mixers=63",
            "HARNESS collection_saved name=C managed_mixers=-1",
            "HARNESS mixers when=a_again name=Mic mixers=31",
            "HARNESS mixers when=a_again name=Music mixers=63",
            "HARNESS settings_checkbox when=a_again desc=Mic on=0",
            "HARNESS settings_checkbox when=a_again desc=Music on=1",
            "HARNESS check_settings when=a_again differ=0",
            "HARNESS mixers when=new name=Mic mixers=255",
            "HARNESS settings_property when=new name=take_over type=8 enabled=1 desc=Manage Track 6",
            "HARNESS check_settings when=new differ=0",
            "HARNESS collection_saved name=new managed_mixers=-1",
        ]
        .map(str::to_owned),
    );
    let guard = format!(
        "HARNESS settings_property when=a_unmanaged name=status type=4 enabled=1 desc={MIC_GUARD}"
    );
    assert!(
        lines.contains(&guard),
        "the dialog says why Start waits: {lines:#?}"
    );
    for (when, enabled) in [
        ("a_unmanaged", false),
        ("a_managed", true),
        ("b", false),
        ("a_again", true),
        ("new", false),
    ] {
        assert_eq!(
            start_button(&lines, when),
            Some(enabled),
            "{when}: {lines:#?}"
        );
    }
    for when in ["a_not_now", "b", "c"] {
        assert!(
            !lines.iter().any(|l| l
                .starts_with(&format!("HARNESS settings_checkbox when={when} "))
                || l.starts_with(&format!(
                    "HARNESS settings_property when={when} name=confirm_take_over "
                ))),
            "{when} is unmanaged with no confirm open: {lines:#?}"
        );
    }
    assert_eq!(
        saved, None,
        "a takeover is kept in the scene collection, not the module config"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn cancel_puts_back_the_discord_track_the_dialog_changed_on_every_source() {
    let (lines, _) = dialog_session("--mixer-cancel");
    assert_lines(
        &lines,
        &[
            "HARNESS mixers when=open name=Music mixers=63",
            "HARNESS mixers when=open name=Voice mixers=31",
            "HARNESS mixers when=changed name=Music mixers=31",
            "HARNESS mixers when=changed name=Voice mixers=63",
            "HARNESS mixers when=changed name=Late mixers=223",
            "HARNESS check_settings when=cancelled differ=0",
            "HARNESS mixers when=cancelled name=Music mixers=63",
            "HARNESS mixers when=cancelled name=Voice mixers=31",
            "HARNESS mixers when=cancelled name=Late mixers=255",
            "HARNESS settings_property when=cancelled name=take_over type=8 enabled=1 desc=Manage Track 6",
            "HARNESS check_settings when=confirming differ=0",
            "HARNESS check_settings when=taken differ=1",
            "HARNESS check_settings when=managed_ok differ=1",
            "HARNESS mixers when=defaults name=Music mixers=63",
            "HARNESS settings_source when=defaults audio_track=6",
            "HARNESS pick when=defaults guild_id= channel_id=",
            "HARNESS settings_checkbox when=defaults desc=Music on=1",
            "HARNESS mixers when=kept name=Music mixers=63",
            "HARNESS settings_checkbox when=managed_open desc=Music on=1",
            "HARNESS check_settings when=ticked_back differ=0",
            "HARNESS mixers when=managed_ticked name=Music mixers=31",
            "HARNESS check_settings when=managed_ticked differ=1",
            "HARNESS check_settings when=managed_cancelled differ=0",
            "HARNESS mixers when=managed_cancelled name=Music mixers=63",
            "HARNESS settings_checkbox when=managed_cancelled desc=Music on=1",
        ]
        .map(str::to_owned),
    );
    assert!(
        has_plugin_line(
            &lines,
            "D-Streamy put back the Discord tracks on 3 audio sources"
        ),
        "Music, Voice and Late: {lines:#?}"
    );
    assert!(
        mixer_step(&lines, "managed")
            .iter()
            .any(|l| l.starts_with("HARNESS picked track=4 ")),
        "Defaults has a pick to put back: {lines:#?}"
    );
    assert!(
        !lines
            .iter()
            .any(|l| l.starts_with("HARNESS settings_checkbox when=cancelled ")),
        "unmanaged again: {lines:#?}"
    );
}

/// A read-only folder, writable again when dropped, so a failing test doesn't leave it read-only.
struct ReadOnly(PathBuf);

impl ReadOnly {
    /// `None` when this user (root) can write it anyway.
    fn new(folder: &std::path::Path) -> Option<Self> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(folder, std::fs::Permissions::from_mode(0o555)).unwrap();
        let guard = Self(folder.to_owned());
        std::fs::write(folder.join("probe"), "")
            .is_err()
            .then_some(guard)
    }
}

impl Drop for ReadOnly {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
}

/// The adopt mode, A's Mic on `mic_mixers`; `None` when `read_only` can't stop this user (root) writing.
fn adopt_with(
    saved: Value,
    mic_mixers: u32,
    read_only: bool,
    extra: &[&str],
) -> Option<(Vec<String>, Value)> {
    let config = tempfile::tempdir().unwrap();
    write_module_config(config.path(), saved);
    let folder = module_config(config.path()).parent().unwrap().to_owned();
    let _read_only = if read_only {
        Some(ReadOnly::new(&folder)?)
    } else {
        None
    };
    let dir = config.path().to_string_lossy().into_owned();
    let lib = fake_frontend();
    let mixers = mic_mixers.to_string();
    let mut args = vec![
        "--fake-frontend",
        lib.as_str(),
        "--module-config",
        dir.as_str(),
        "--collection-adopt",
        "--adopt-mic-mixers",
        mixers.as_str(),
    ];
    args.extend_from_slice(extra);
    let lines = report_ui(&args);
    Some((lines, read_module_config(config.path())))
}

fn adopt(saved: Value, mic_mixers: u32) -> (Vec<String>, Value) {
    adopt_with(saved, mic_mixers, false, &[]).expect("a writable config")
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_track_managed_before_the_upgrade_is_adopted_by_the_first_collection_without_touching_mixers() {
    let (lines, saved) = adopt(
        json!({"audio_track": 6, "managed_track": 6, "guild_id": "g"}),
        0x1f,
    );
    assert_lines(
        &lines,
        &[
            "HARNESS mixers when=a_loaded name=Mic mixers=31",
            "HARNESS mixers when=a_loaded name=Music mixers=63",
            "HARNESS settings_checkbox when=adopted desc=Mic on=0",
            "HARNESS settings_checkbox when=adopted desc=Music on=1",
            "HARNESS check_settings when=adopted differ=0",
            "HARNESS collection_saved name=A managed_mixers=32",
            "HARNESS mixers when=b_adopt name=Mic mixers=63",
            "HARNESS settings_property when=b_adopt name=take_over type=8 enabled=1 desc=Manage Track 6",
            "HARNESS collection_saved name=B managed_mixers=-1",
        ]
        .map(str::to_owned),
    );
    assert_eq!(saved["config_version"], 2, "{saved}");
    assert_eq!(saved.get("managed_track"), None, "{saved}");
    assert_eq!(
        saved.get("adopt_managed_track"),
        None,
        "adopted once: {saved}"
    );
    assert_eq!(saved["guild_id"], "g", "{saved}");
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_legacy_managed_track_with_a_mic_on_it_stays_unmanaged_so_go_live_waits() {
    let (lines, saved) = adopt(json!({"audio_track": 3, "managed_track": 3}), 0x1f);
    assert_lines(
        &lines,
        &[
            "HARNESS mixers when=a_loaded name=Mic mixers=31",
            "HARNESS settings_property when=adopted name=take_over type=8 enabled=1 desc=Manage Track 3",
            "HARNESS collection_saved name=A managed_mixers=-1",
        ]
        .map(str::to_owned),
    );
    assert_eq!(start_button(&lines, "adopted"), Some(false), "{lines:#?}");
    assert!(
        has_plugin_line(&lines, "but Mic is on it here, so Track 3 stays unmanaged"),
        "{lines:#?}"
    );
    assert_eq!(
        saved.get("adopt_managed_track"),
        None,
        "decided once: {saved}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn an_unsaved_adoption_is_not_repeated_in_the_next_collection() {
    let Some((lines, saved)) = adopt_with(
        json!({"audio_track": 6, "managed_track": 6}),
        0x1f,
        true,
        &["--adopt-stale-reload"],
    ) else {
        eprintln!("skipped: this user can write a read-only folder");
        return;
    };
    assert_lines(
        &lines,
        &[
            "HARNESS collection_saved name=A managed_mixers=32",
            "HARNESS check_settings when=corrupt differ=0",
            "HARNESS check_settings when=repaired differ=0",
            "HARNESS mixers when=b_adopt name=Mic mixers=31",
            "HARNESS settings_property when=b_adopt name=take_over type=8 enabled=1 desc=Manage Track 6",
            "HARNESS collection_saved name=B managed_mixers=-1",
        ]
        .map(str::to_owned),
    );
    let unreadable = |when: &str| {
        let prefix = format!("HARNESS settings_property when={when} name=status ");
        lines
            .iter()
            .any(|l| l.starts_with(&prefix) && l.contains("Couldn't read D-Streamy's settings"))
    };
    assert!(unreadable("corrupt"), "{lines:#?}");
    assert!(
        !unreadable("repaired"),
        "the repaired file, still naming managed_track, was loaded again: {lines:#?}"
    );
    assert!(
        !lines
            .iter()
            .any(|l| l.starts_with("HARNESS settings_checkbox when=b_adopt ")),
        "B has no mic on Track 6, yet the reloaded file's managed_track is not adopted again: {lines:#?}"
    );
    assert_eq!(
        saved["managed_track"], 6,
        "the read-only file was never written: {saved}"
    );
    assert!(
        has_plugin_line(&lines, "Couldn't save D-Streamy's settings"),
        "{lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_deliberate_track_1_takeover_stays_on_track_1_and_says_what_it_costs() {
    let (lines, saved) = adopt(json!({"audio_track": 1, "managed_track": 1}), 0x3e);
    assert_lines(
        &lines,
        &[
            "HARNESS settings_source when=adopted audio_track=1",
            "HARNESS settings_info when=adopted name=audio_note_0 kind=1",
            "HARNESS collection_saved name=A managed_mixers=1",
        ]
        .map(str::to_owned),
    );
    assert!(
        lines.iter().any(|l| l.starts_with(
            "HARNESS settings_property when=adopted name=audio_note_0 type=4 enabled=1 desc=Track 1 is OBS's \
             streaming and recording track, and D-Streamy manages it for Discord"
        )),
        "{lines:#?}"
    );
    assert!(
        !lines
            .iter()
            .any(|l| l.starts_with("HARNESS settings_property when=adopted name=release_")),
        "Discord gets Track 1, so it can't be let go here: {lines:#?}"
    );
    assert_eq!(saved["audio_track"], 1, "{saved}");
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn track_1_confirms_its_cost_and_another_managed_track_can_be_let_go() {
    let (lines, _) = dialog_session("--release-session");
    assert_lines(
        &lines,
        &[
            "HARNESS settings_info when=track1_confirming name=take_over_effect kind=1",
            "HARNESS settings_property when=track1_confirming name=instead_6 type=8 enabled=1 desc=Use Track 6 instead",
            "HARNESS settings_source when=instead audio_track=6",
            "HARNESS settings_checkbox when=instead desc=Music on=0",
            "HARNESS mixers when=three name=Mic mixers=27",
            "HARNESS mixers when=three name=Music mixers=27",
            "HARNESS settings_source when=reopened audio_track=3",
            "HARNESS settings_property when=reopened name=release_6 type=8 enabled=1 desc=Stop managing Track 6",
            "HARNESS settings_info when=reopened name=audio_note_0 kind=1",
            "HARNESS mixers when=released name=Mic mixers=27",
            "HARNESS mixers when=released name=Music mixers=27",
            "HARNESS settings_property when=released name=audio_note_0 type=4 enabled=1 desc=D-Streamy no longer \
             manages Track 6. It is still off on Mic and Music: turn it back on in Edit \u{203a} Advanced Audio \
             Properties for those that need it.",
            "HARNESS mixers when=released name=Late mixers=251",
            "HARNESS settings_property when=six_again name=take_over type=8 enabled=1 desc=Manage Track 6",
            "HARNESS settings_source when=cancelled audio_track=3",
            "HARNESS settings_property when=cancelled name=release_6 type=8 enabled=1 desc=Stop managing Track 6",
            "HARNESS mixers when=cancelled name=Mic mixers=27",
            "HARNESS mixers when=cancelled name=Late mixers=219",
        ]
        .map(str::to_owned),
    );
    assert!(
        lines.iter().any(|l| l.starts_with(
            "HARNESS settings_property when=track1_confirming name=take_over_effect type=4 enabled=1 \
             desc=Track 1 is OBS's streaming and recording track: managing it turns it off on every audio source"
        )),
        "{lines:#?}"
    );
    assert!(
        lines.contains(
            &"HARNESS settings_property when=three name=release_6 type=8 enabled=1 desc=Stop managing Track 6"
                .to_owned()
        ),
        "a Cancel would manage the opened pick again, so letting it go is offered at once: {lines:#?}"
    );
    assert!(
        !lines
            .iter()
            .any(|l| l.starts_with("HARNESS settings_property when=released name=release_")),
        "nothing left to let go: {lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn mixer_changes_obs_saves_before_closing_the_dialog_stay_and_only_picks_are_asked_about() {
    let (lines, _) = dialog_session("--collection-close");
    assert_lines(
        &lines,
        &[
            "HARNESS check_settings when=kept_before differ=1",
            "HARNESS collection_saved name=A managed_mixers=32",
            "HARNESS close_dialog differ=0 choice=none",
            "HARNESS mixers when=a_kept name=Mic mixers=31",
            "HARNESS mixers when=a_kept name=Music mixers=63",
            "HARNESS settings_checkbox when=a_kept desc=Music on=1",
            "HARNESS check_settings when=a_kept differ=0",
            "HARNESS check_settings when=autosaved differ=0",
            "HARNESS check_settings when=autosave_rebuilt differ=1",
            "HARNESS collection_loaded name=D",
            "HARNESS mixers when=d_loaded name=Music mixers=31",
            "HARNESS collection_loaded name=R",
            "HARNESS mixers when=r_loaded name=Music mixers=63",
            "HARNESS close_dialog differ=1 choice=discard",
            "HARNESS settings_source when=b_discarded audio_track=6",
            "HARNESS mixers when=a_discarded name=Mic mixers=63",
            "HARNESS close_dialog differ=1 choice=cancel",
            "HARNESS check_settings when=b_cancel_back differ=0",
            "HARNESS mixers when=b_taken name=Mic mixers=31",
            "HARNESS check_settings when=b_taken differ=1",
            "HARNESS mixers when=b_cancelled name=Mic mixers=63",
            "HARNESS mixers when=b_cancelled name=Music mixers=63",
            "HARNESS settings_property when=b_cancelled name=take_over type=8 enabled=1 desc=Manage Track 6",
            "HARNESS check_settings when=exit_before differ=1",
        ]
        .map(str::to_owned),
    );
    let leaving = "the Discord track changes made in the open D-Streamy dialog stay";
    for (step, closes) in [
        ("duplicate", "none"),
        ("rename", "none"),
        ("switch_discard", "discard"),
    ] {
        let lines = collection_step(&lines, step);
        let close = format!(
            "HARNESS close_dialog differ={} choice={closes}",
            u8::from(closes != "none")
        );
        assert!(lines.contains(&close), "{step}: {lines:#?}");
    }
    assert!(
        has_plugin_line(collection_step(&lines, "switch_discard"), leaving),
        "the switch's changing event keeps the dialog's changes before the Discard: {lines:#?}"
    );
    let exit = collection_step(&lines, "exit");
    let at = |want: &str| exit.iter().position(|l| l == want);
    assert!(
        at("HARNESS close_dialog differ=0 choice=none")
            > at("HARNESS check_settings when=exit_before differ=1"),
        "OBS saved the takeover at exit, so closing asks nothing: {exit:#?}"
    );
    assert!(has_plugin_line(exit, leaving), "{exit:#?}");
}

/// The lines between a collection run's `step` marker and the next marker.
fn collection_step<'a>(lines: &'a [String], step: &str) -> &'a [String] {
    let marker = format!("HARNESS collection_step name={step}");
    let start = lines
        .iter()
        .position(|l| *l == marker)
        .unwrap_or_else(|| panic!("no {marker}: {lines:#?}"))
        + 1;
    let end = lines[start..]
        .iter()
        .position(|l| l.starts_with("HARNESS collection_step "))
        .map_or(lines.len(), |n| start + n);
    &lines[start..end]
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn reopening_the_dialog_keeps_the_open_one_on_cancel_and_starts_fresh_on_discard() {
    let (lines, _) = dialog_session("--reopen-session");
    assert_lines(
        &lines,
        &[
            "HARNESS settings_property when=first_confirming name=confirm_take_over type=8 enabled=1 desc=Manage",
            "HARNESS close_dialog differ=1 choice=cancel",
            "HARNESS reopened opened=0",
            "HARNESS mixers when=cancelled name=Music mixers=27",
            "HARNESS settings_checkbox when=first_after desc=Music on=0",
            "HARNESS close_dialog differ=1 choice=discard",
            "HARNESS reopened opened=1",
            "HARNESS mixers when=discarded name=Music mixers=63",
            "HARNESS settings_source when=second audio_track=6",
            "HARNESS settings_property when=second name=take_over type=8 enabled=1 desc=Manage Track 6",
            "HARNESS check_settings when=second differ=0",
            "HARNESS settings_property when=second_confirming name=confirm_take_over type=8 enabled=1 desc=Manage",
            "HARNESS mixers when=second_cancelled name=Music mixers=63",
            "HARNESS settings_source when=second_cancelled audio_track=6",
        ]
        .map(str::to_owned),
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_config_saved_on_the_old_default_track_1_opens_on_track_6() {
    let config = tempfile::tempdir().unwrap();
    write_module_config(config.path(), json!({"audio_track": 1, "guild_id": "g"}));
    let dir = config.path().to_string_lossy().into_owned();
    let lib = fake_frontend();
    let lines = report_ui(&[
        "--fake-frontend",
        lib.as_str(),
        "--module-config",
        dir.as_str(),
    ]);
    assert!(
        lines.contains(&"HARNESS settings_source when=open audio_track=6".to_owned()),
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
        lines.iter().any(|l| l
            .strip_prefix("HARNESS settings_modified refresh=1 status=")
            .is_some_and(|s| s.starts_with(NO_SERVICE))),
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
        assert!(status.starts_with(NO_SERVICE), "{when}: {status}");
        assert!(
            status.contains(&format!("). {read_error}")),
            "{when}: the session and config errors both show: {status}"
        );
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
        let status = status_line(&lines, "updated");
        assert!(status.starts_with(NO_SERVICE), "{saved:?}: {status}");
        assert!(
            status.contains(&format!("). {save_error}")),
            "{saved:?}: {status}"
        );
        assert!(
            status.ends_with("; Track 4 is used until OBS quits"),
            "{saved:?}: {status}"
        );
        let reopened = status_line(&lines, "reopened");
        let read_error = format!(
            "Couldn't read D-Streamy's settings from {}: Not a directory (os error 20). \
             Fix or delete the file; until then Track 4 is used until OBS quits but is not saved",
            module_config(config.path()).display()
        );
        assert!(
            reopened.starts_with(NO_SERVICE) && reopened.ends_with(&format!("). {read_error}")),
            "{saved:?}: the retry re-reads first and is blocked: {reopened}"
        );
        for when in ["updated", "reopened"] {
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
    let status = status_line(&lines, "updated");
    assert!(
        status.starts_with(NO_SERVICE) && status.ends_with(&format!("). {error}")),
        "{status}"
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
fn a_folder_in_place_of_the_module_config_is_reported_and_never_replaced() {
    let config = tempfile::tempdir().unwrap();
    let path = module_config(config.path());
    std::fs::create_dir_all(path.join("keep")).unwrap();
    std::fs::write(path.join("keep").join("note"), "user data").unwrap();
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
        "Couldn't read D-Streamy's settings from {}: Is a directory (os error 21). Fix or delete the file; \
         until then Track",
        path.display()
    );
    for (when, track) in [("open", 6), ("updated", 5), ("reopened", 5)] {
        let status = status_line(&lines, when);
        assert!(status.starts_with(NO_SERVICE), "{when}: {status}");
        assert!(
            status.ends_with(&format!(
                "). {read_error} {track} is used until OBS quits but is not saved"
            )),
            "{when}: {status}"
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
            .any(|l| l == "HARNESS settings_source when=reopened audio_track=5"),
        "{lines:#?}"
    );
    assert!(path.is_dir(), "the folder is kept");
    assert_eq!(
        std::fs::read_to_string(path.join("keep").join("note")).unwrap(),
        "user data"
    );
    assert_eq!(temp_files(path.parent().unwrap()), Vec::<String>::new());
}

/// `dstreamy serve` at the plugin's default endpoint under [`runtime_dir`].
fn default_serve(h: &Harness) -> Proc {
    let endpoint = runtime_dir(h).join("dstreamy").join("serve.sock");
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin("dstreamy"));
    cmd.env_remove("DSTREAMY_TOKEN")
        .env_remove("DSTREAMY_CONFIG_DIR")
        .arg("--config-dir")
        .arg(h.dir.path())
        .arg("--gateway-url")
        .arg(h.mock.gateway_url())
        .args(["-v", "serve", "--skip-platform", "--endpoint"])
        .arg(&endpoint);
    let p = Proc::spawn(&mut cmd);
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::os::unix::net::UnixStream::connect(&endpoint).is_err() {
        assert!(Instant::now() < deadline, "serve never listened");
        std::thread::sleep(Duration::from_millis(20));
    }
    p
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn tools_start_binds_the_chosen_track_and_stop_ends_the_session() {
    let h = Harness::new("");
    let _serve = default_serve(&h);
    let config = tempfile::tempdir().unwrap();
    let stale = json!({"connections": [{"id": 3, "fps": 30}], "guild_id": "g", "channel_id": "c",
        "instance": {"dev": 1, "ino": 1, "mtime": 1, "mtime_nsec": 1}});
    write_module_config(
        config.path(),
        json!({"audio_track": 1, "bitrate_kbps": 2500, "owned": stale}),
    );
    let dir = config.path().to_string_lossy().into_owned();
    let lib = fake_frontend();
    let pick =
        json!({"guild_id": MOCK_GUILD_ID, "channel_id": MOCK_VOICE_CHANNEL_ID, "audio_track": 3})
            .to_string();
    let run = HarnessRun::start(
        &h,
        json!({}),
        &[
            "--fake-frontend",
            lib.as_str(),
            "--module-config",
            dir.as_str(),
            "--tools-session",
            "--settings-update",
            pick.as_str(),
            "--settings-update-while-active",
            r#"{"audio_track": 5}"#,
            "--seconds",
            "60",
            "--until-video",
            "60",
        ],
    );
    let (status, lines) = run.finish();
    passed(status, &lines);

    assert!(
        lines
            .iter()
            .any(|l| l == "HARNESS settings_item when=open name=guild_id value=0 text=Mock Guild"),
        "the dialog lists the service's servers: {lines:#?}"
    );
    assert!(
        lines.iter().any(|l| l == "HARNESS settings_item when=updated name=channel_id value=0 text=General"),
        "{lines:#?}"
    );
    asked_first(&lines, "start", 0, "Go live in General on Mock Guild");
    assert!(
        lines.iter().any(|l| l
            == "HARNESS settings_clicked name=confirm_stream enabled=1 refresh=1 status=Live on Track 3"
            || l.starts_with(
                "HARNESS settings_clicked name=confirm_stream enabled=1 refresh=1 status=Starting on Track 3"
            )),
        "the confirm starts: {lines:#?}"
    );
    asked_first(&lines, "stop", 1, "Stop the Discord stream?");
    assert!(
        lines.iter().any(|l| l.starts_with(
            "HARNESS settings_clicked name=confirm_stream enabled=1 refresh=1 status=Stopping"
        ) || l
            == "HARNESS settings_clicked name=confirm_stream enabled=1 refresh=1 status=Idle"),
        "the second confirm stops: {lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l == "HARNESS settings_status when=live status=Live on Track 3"),
        "{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l == "HARNESS output_audio_track when=tools_live active=1 track=3"),
        "{lines:#?}"
    );
    assert_eq!(
        status_line(&lines, "live_updated"),
        "Live on Track 3 \u{2014} Discord hears Track 5 from the next Go Live"
    );
    assert!(
        lines
            .iter()
            .any(|l| l == "HARNESS output_audio_track when=after_settings_update active=1 track=3"),
        "a live change never touches the output: {lines:#?}"
    );
    assert_eq!(
        mixer_reports(&lines)
            .iter()
            .map(|r| (r.0.as_str(), r.1))
            .collect::<Vec<_>>(),
        [("tools_live", Some(2)), ("after_settings_update", Some(2))],
        "{lines:#?}"
    );
    for want in [
        "HARNESS settings_status when=stopped status=Idle",
        "HARNESS tools stopped active=0",
        "HARNESS settings_source when=reopened audio_track=5",
        "HARNESS settings_property when=reopened name=start type=8 enabled=1 desc=Start",
        "HARNESS settings_property when=reopened name=stop type=8 enabled=0 desc=Stop",
    ] {
        assert!(lines.iter().any(|l| l == want), "{want}: {lines:#?}");
    }
    assert!(
        has_plugin_line(&lines, "started from Tools > D-Streamy on Track 3"),
        "{lines:#?}"
    );

    let recorded = h.mock.recorded();
    assert!(
        stream_video(&recorded).len() >= 30,
        "the dialog's session reached Discord"
    );
    let mut control = Client::connect(&runtime_dir(&h).join("dstreamy").join("serve.sock"));
    control.send(&json!({"id": "h", "cmd": "hello", "role": "control", "protocol": 2}));
    let hello = control.reply("h");
    let session = &hello["data"]["status"]["session"];
    assert_eq!(
        session["state"], "stopped",
        "Stop ended the service session: {hello}"
    );
    assert_eq!(session["reason"]["reason"], "requested", "{hello}");

    let saved = read_module_config(config.path());
    assert_eq!(
        saved["owned"],
        Value::Null,
        "the stopped session is no longer owned: {saved}"
    );
    assert_eq!(saved["audio_track"], 5, "{saved}");
    assert_eq!(saved["guild_id"], MOCK_GUILD_ID, "{saved}");
    assert_eq!(saved["channel_id"], MOCK_VOICE_CHANNEL_ID, "{saved}");
    assert_eq!(saved["bitrate_kbps"], 2500, "{saved}");
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn tools_start_without_the_service_says_so_and_stays_idle() {
    let h = Harness::new("");
    let config = tempfile::tempdir().unwrap();
    write_module_config(
        config.path(),
        json!({"guild_id": MOCK_GUILD_ID, "channel_id": MOCK_VOICE_CHANNEL_ID, "audio_track": 2}),
    );
    let not_running = format!(
        "The D-Streamy service is not running (no service is listening at {}; \
         start it with `dstreamy service install`, or run `dstreamy serve`)",
        runtime_dir(&h)
            .join("dstreamy")
            .join("serve.sock")
            .display(),
    );
    let (status, lines) = tools_run(
        &h,
        config.path(),
        &[
            "--tools-expect",
            &not_running,
            "--seconds",
            "5",
            "--until-video",
            "1",
        ],
    )
    .finish();
    passed(status, &lines);
    asked_first(&lines, "start", 0, "Go live in ");
    assert!(
        lines.contains(&format!(
            "HARNESS settings_clicked name=confirm_stream enabled=1 refresh=1 status={not_running}"
        )),
        "{lines:#?}"
    );
    assert!(
        lines.contains(&format!(
            "HARNESS settings_status when=live status={not_running}"
        )),
        "{lines:#?}"
    );
    assert!(
        lines.iter().any(|l| l == "HARNESS tools output=0 active=0"),
        "no output is created without a session: {lines:#?}"
    );
    for when in ["after_stop", "reopened"] {
        for want in [
            format!(
                "HARNESS settings_property when={when} name=status type=4 enabled=1 desc={not_running}"
            ),
            format!("HARNESS settings_info when={when} name=status kind=2"),
            format!("HARNESS settings_property when={when} name=start type=8 enabled=1 desc=Start"),
            format!("HARNESS settings_property when={when} name=stop type=8 enabled=0 desc=Stop"),
        ] {
            assert!(lines.contains(&want), "{want}: {lines:#?}");
        }
    }
    assert!(lines.iter().all(|l| !l.contains("panicked")), "{lines:#?}");
    let saved = read_module_config(config.path());
    assert_eq!(saved["audio_track"], 2, "{saved}");
    assert_eq!(saved.get("owned"), None, "never owned: {saved}");
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn tools_start_then_cancel_leaves_the_session_idle_and_the_service_untouched() {
    let h = Harness::new("");
    let _serve = default_serve(&h);
    let config = picked_config(3);
    let (status, lines) = tools_run(
        &h,
        config.path(),
        &[
            "--tools-cancel-start",
            "--seconds",
            "10",
            "--until-video",
            "1",
        ],
    )
    .finish();
    passed(status, &lines);
    asked_first(&lines, "start", 0, "Go live in ");
    assert!(
        lines
            .iter()
            .any(|l| l
                .starts_with("HARNESS settings_clicked name=cancel_stream enabled=1 refresh=1 ")),
        "{lines:#?}"
    );
    assert!(
        !lines
            .iter()
            .any(|l| l.starts_with("HARNESS settings_clicked name=confirm_stream ")),
        "{lines:#?}"
    );
    assert!(
        lines.contains(&"HARNESS tools output=0 active=0".to_owned()),
        "{lines:#?}"
    );
    for when in ["cancelled", "reopened"] {
        for want in [
            format!("HARNESS settings_property when={when} name=start type=8 enabled=1 desc=Start"),
            format!("HARNESS settings_property when={when} name=stop type=8 enabled=0 desc=Stop"),
        ] {
            assert!(lines.contains(&want), "{want}: {lines:#?}");
        }
        assert!(
            !lines.iter().any(|l| l.starts_with(&format!(
                "HARNESS settings_property when={when} name=stream_question "
            ))),
            "the question is gone: {lines:#?}"
        );
    }
    assert!(!has_plugin_line(&lines, "started from Tools"), "{lines:#?}");
    assert!(h.mock.voice_state().is_none(), "nothing joined: {lines:#?}");
    assert!(
        stream_video(&h.mock.recorded()).is_empty(),
        "nothing reached Discord"
    );
    assert_eq!(
        serve_session(&h),
        json!({"state": "idle"}),
        "serve never started a session"
    );
    let saved = read_module_config(config.path());
    assert!(
        saved.get("owned").is_none_or(Value::is_null),
        "never owned: {saved}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_start_without_the_control_thread_says_so_without_rebuilding_inside_the_click() {
    let h = Harness::new("");
    let _serve = default_serve(&h);
    let config = picked_config(3);
    let gone = "D-Streamy's control thread is not running, so Start and Stop can't finish. \
                See the OBS log and restart OBS";
    let (status, lines) = tools_run_env(
        &h,
        config.path(),
        &[
            "--tools-expect",
            gone,
            "--seconds",
            "5",
            "--until-video",
            "1",
        ],
        &[("DSTREAMY_TEST_NO_CONTROL_THREAD", std::path::Path::new("1"))],
    )
    .finish();
    passed(status, &lines);
    asked_first(&lines, "start", 0, "Go live in ");
    assert!(
        lines.contains(&format!(
            "HARNESS settings_clicked name=confirm_stream enabled=1 refresh=1 status={gone}"
        )),
        "the click's own rebuild shows the error: {lines:#?}"
    );
    assert!(
        lines.contains(
            &"HARNESS settings_click_sync_refreshes name=confirm_stream count=0".to_owned()
        ),
        "the click never asked OBS to rebuild the dialog it is running in: {lines:#?}"
    );
    assert!(lines.contains(&"HARNESS tools output=0 active=0".to_owned()));
    for want in [
        "HARNESS settings_property when=after_stop name=start type=8 enabled=1 desc=Start",
        "HARNESS settings_property when=after_stop name=stop type=8 enabled=0 desc=Stop",
        "HARNESS settings_info when=after_stop name=status kind=2",
    ] {
        assert!(lines.contains(&want.to_owned()), "{want}: {lines:#?}");
    }
    assert!(lines.iter().all(|l| !l.contains("panicked")), "{lines:#?}");
    assert!(h.mock.voice_state().is_none(), "nothing joined");
    let session = serve_session(&h);
    assert!(
        session.is_null() || session == json!({"state": "idle"}),
        "serve never started a session: {session}"
    );
}

fn tools_run(h: &Harness, config: &std::path::Path, extra: &[&str]) -> HarnessRun {
    tools_run_env(h, config, extra, &[])
}

/// With `gate`, the plugin's Start asks the service only once that file exists.
fn tools_run_gated(
    h: &Harness,
    config: &std::path::Path,
    extra: &[&str],
    gate: &std::path::Path,
) -> HarnessRun {
    tools_run_env(h, config, extra, &[("DSTREAMY_TEST_START_GATE", gate)])
}

fn tools_run_env(
    h: &Harness,
    config: &std::path::Path,
    extra: &[&str],
    vars: &[(&str, &std::path::Path)],
) -> HarnessRun {
    let dir = config.to_string_lossy().into_owned();
    let lib = fake_frontend();
    let mut args = vec![
        "--fake-frontend",
        lib.as_str(),
        "--module-config",
        dir.as_str(),
        "--tools-session",
    ];
    args.extend(extra);
    HarnessRun::start_with_env(h, json!({}), &args, vars)
}

fn wait_for_line(run: &HarnessRun, what: &str, f: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(line) = run.lines().into_iter().find(|l| f(l)) {
            return line;
        }
        assert!(Instant::now() < deadline, "no {what}: {:#?}", run.lines());
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn serve_session(h: &Harness) -> Value {
    let mut control = Client::connect(&runtime_dir(h).join("dstreamy").join("serve.sock"));
    control.send(&json!({"id": "h", "cmd": "hello", "role": "control", "protocol": 2}));
    control.reply("h")["data"]["status"]["session"].clone()
}

fn picked_config(track: u8) -> tempfile::TempDir {
    let config = tempfile::tempdir().unwrap();
    write_module_config(
        config.path(),
        json!({"guild_id": MOCK_GUILD_ID, "channel_id": MOCK_VOICE_CHANNEL_ID, "audio_track": track,
            "config_version": 2}),
    );
    config
}

const OTHER_GUILD_ID: &str = "200000000000000002";
const OTHER_GUILD_CHANNEL_ID: &str = "300000000000000010";

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_server_change_in_the_dialog_never_keeps_the_old_servers_channel() {
    let mut discord = dstreamy_mock_discord::MockConfig::default();
    discord.guilds.push(dstreamy_mock_discord::MockGuild {
        id: OTHER_GUILD_ID.into(),
        name: "Other Guild".into(),
        channels: vec![dstreamy_mock_discord::MockChannel::voice(
            OTHER_GUILD_CHANNEL_ID,
            "Lobby",
        )],
        in_ready: true,
    });
    let h = Harness::with_mock("", discord);
    let _serve = default_serve(&h);
    let config = tempfile::tempdir().unwrap();
    let first =
        json!({"guild_id": MOCK_GUILD_ID, "channel_id": MOCK_VOICE_CHANNEL_ID, "audio_track": 2})
            .to_string();
    let other = json!({"guild_id": OTHER_GUILD_ID}).to_string();
    let pick_first = "Pick a server and a voice channel first";
    let (status, lines) = tools_run(
        &h,
        config.path(),
        &[
            "--settings-update",
            &first,
            "--then-update",
            &other,
            "--tools-expect",
            pick_first,
            "--seconds",
            "10",
            "--until-video",
            "1",
        ],
    )
    .finish();
    passed(status, &lines);
    assert!(
        lines.contains(&format!(
            "HARNESS pick when=updated guild_id={MOCK_GUILD_ID} channel_id={MOCK_VOICE_CHANNEL_ID}"
        )),
        "{lines:#?}"
    );
    assert!(
        lines.contains(&format!(
            "HARNESS pick when=updated_again guild_id={OTHER_GUILD_ID} channel_id="
        )),
        "the view's old channel is cleared with the server: {lines:#?}"
    );
    let channels: Vec<&str> = settings_lines(&lines, "updated_again")
        .into_iter()
        .filter_map(|l| l.strip_prefix("HARNESS settings_item when=updated_again name=channel_id "))
        .collect();
    assert_eq!(
        channels,
        ["value=0 text=Pick a voice channel", "value=0 text=Lobby"],
        "only the new server's channels: {lines:#?}"
    );
    asked_first(&lines, "start", 0, "Go live in ");
    assert!(
        lines.contains(&format!(
            "HARNESS settings_clicked name=confirm_stream enabled=1 refresh=1 status={pick_first}"
        )),
        "Start never uses the old server's channel: {lines:#?}"
    );
    assert!(lines.contains(&"HARNESS tools output=0 active=0".to_owned()));
    assert!(
        lines.contains(&format!(
            "HARNESS pick when=reopened guild_id={OTHER_GUILD_ID} channel_id="
        )),
        "{lines:#?}"
    );
    assert!(h.mock.voice_state().is_none(), "nothing joined: {lines:#?}");
    assert_eq!(
        serve_session(&h),
        json!({"state": "idle"}),
        "serve never started a session"
    );
    let saved = read_module_config(config.path());
    assert_eq!(saved["guild_id"], OTHER_GUILD_ID, "{saved}");
    assert_eq!(saved["channel_id"], Value::Null, "{saved}");
    assert_eq!(saved["audio_track"], 2, "{saved}");
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn while_live_the_dialog_shows_the_heard_track_and_warns_about_a_mic_on_it() {
    let h = Harness::new("");
    let _serve = default_serve(&h);
    let config = picked_config(4);
    let (status, lines) = tools_run(
        &h,
        config.path(),
        &[
            "--live-mic",
            "--settings-update-while-active",
            r#"{"audio_track": 2}"#,
            "--tools-expect",
            "Mic is on Track 2, so Discord would hear it.",
            "--seconds",
            "30",
            "--until-video",
            "30",
        ],
    )
    .finish();
    passed(status, &lines);
    let leak = "Mic is on Track 4, so Discord hears it now. Manage Track 4, \
                or turn Track 4 off for Mic in Edit \u{203a} Advanced Audio Properties.";
    assert_lines(
        &lines,
        &[
            format!("HARNESS settings_property when=live_mic name=status type=4 enabled=1 desc={leak}"),
            "HARNESS settings_info when=live_mic name=status kind=1".to_owned(),
            "HARNESS settings_property when=live_mic name=take_over type=8 enabled=1 desc=Manage Track 4"
                .to_owned(),
            "HARNESS settings_property when=live_updated name=take_over type=8 enabled=1 desc=Manage Track 4"
                .to_owned(),
            "HARNESS live_pick_refreshes count=0".to_owned(),
        ],
    );
    assert!(
        lines.iter().any(|l| l.starts_with(
            "HARNESS settings_property when=live_mic name=audio_help type=4 enabled=1 \
             desc=Discord hears every source on Track 4 (live), including Mic."
        )),
        "{lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_start_slower_than_the_click_shows_live_or_its_error_without_reopening() {
    for answered in [true, false] {
        let h = Harness::new("");
        let mut serve = default_serve(&h);
        let config = picked_config(4);
        let gate = h.dir.path().join("start-gate");
        let late_error = "The D-Streamy service is not running";
        let mut extra = vec!["--seconds", "60", "--until-video", "30"];
        if !answered {
            extra.extend(["--tools-expect", late_error]);
        }
        let run = tools_run_gated(&h, config.path(), &extra, &gate);
        let clicked = wait_for_line(&run, "start confirm", |l| {
            l.starts_with("HARNESS settings_clicked name=confirm_stream ")
        });
        assert_eq!(
            clicked,
            "HARNESS settings_clicked name=confirm_stream enabled=1 refresh=1 status=Starting on Track 4\u{2026}",
            "the click's wait ran out first"
        );
        asked_first(&run.lines(), "start", 0, "Go live in ");
        if !answered {
            serve.kill();
        }
        std::thread::sleep(Duration::from_millis(500));
        std::fs::write(&gate, "").unwrap();
        let (status, lines) = run.finish();
        passed(status, &lines);
        for name in ["start", "stop", "confirm_stream"] {
            assert!(
                !lines.iter().any(|l| l.starts_with(&format!(
                    "HARNESS settings_click_sync_refreshes name={name} count="
                )) && !l.ends_with(" count=0")),
                "no refresh inside a click: {lines:#?}"
            );
        }
        let want = if answered {
            "Live on Track 4"
        } else {
            late_error
        };
        assert!(
            lines.iter().any(|l| l.starts_with(&format!(
                "HARNESS settings_refreshed when=live status={want}"
            ))),
            "{answered}: the dialog rebuilt itself on the control thread's change: {lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with(&format!("HARNESS settings_status when=live status={want}"))),
            "{lines:#?}"
        );
        if answered {
            assert!(
                lines.contains(&"HARNESS settings_status when=stopped status=Idle".to_owned()),
                "{lines:#?}"
            );
            assert!(has_plugin_line(
                &lines,
                "started from Tools > D-Streamy on Track 4"
            ));
        } else {
            assert!(lines.contains(&"HARNESS tools output=0 active=0".to_owned()));
            assert!(
                lines.contains(
                    &"HARNESS settings_property when=reopened name=start type=8 enabled=1 desc=Start"
                        .to_owned()
                ),
                "{lines:#?}"
            );
        }
    }
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn an_output_that_refuses_to_start_ends_the_service_session_it_started() {
    let h = Harness::new("");
    let _serve = default_serve(&h);
    let config = tempfile::tempdir().unwrap();
    write_module_config(
        config.path(),
        json!({"guild_id": MOCK_GUILD_ID, "channel_id": MOCK_VOICE_CHANNEL_ID,
            "video_encoder": "no_such_encoder"}),
    );
    let fix = ". Fix it, then press Start again";
    let (status, lines) = tools_run(
        &h,
        config.path(),
        &[
            "--tools-expect",
            fix,
            "--seconds",
            "20",
            "--until-video",
            "1",
        ],
    )
    .finish();
    passed(status, &lines);
    let shown = lines
        .iter()
        .find_map(|l| l.strip_prefix("HARNESS settings_status when=live status="))
        .unwrap_or_else(|| panic!("{lines:#?}"));
    assert!(
        shown.contains("no_such_encoder") && shown.ends_with(fix),
        "{shown}"
    );
    assert!(
        lines.contains(&"HARNESS tools output=1 active=0".to_owned()),
        "{lines:#?}"
    );
    assert!(!has_plugin_line(&lines, "started from Tools"), "{lines:#?}");
    let session = serve_session(&h);
    assert_eq!(session["state"], "stopped", "rolled back: {session}");
    assert_eq!(session["reason"]["reason"], "requested", "{session}");
    let saved = read_module_config(config.path());
    assert_eq!(saved["owned"], Value::Null, "{saved}");
    assert!(
        lines.contains(
            &"HARNESS settings_property when=reopened name=start type=8 enabled=1 desc=Start"
                .to_owned()
        ),
        "{lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_session_the_service_ends_shows_in_the_open_dialog() {
    let h = Harness::new("");
    let _serve = default_serve(&h);
    let config = picked_config(1);
    let run = tools_run(
        &h,
        config.path(),
        &[
            "--tools-wait-end",
            "--tools-expect",
            "Press Start to go live again",
            "--seconds",
            "60",
            "--until-video",
            "30",
        ],
    );
    wait_for_line(&run, "the live wait", |l| {
        l == "HARNESS tools waiting for the end"
    });
    let mut control = Client::connect(&runtime_dir(&h).join("dstreamy").join("serve.sock"));
    control.send(&json!({"id": "h", "cmd": "hello", "role": "control", "protocol": 2}));
    control.reply("h");
    control.send(&json!({"id": "s", "cmd": "stop"}));
    let stopped = control.reply("s");
    assert_eq!(stopped["type"], "result", "{stopped}");
    let (status, lines) = run.finish();
    passed(status, &lines);
    let ended = lines
        .iter()
        .find_map(|l| l.strip_prefix("HARNESS settings_refreshed when=ended status="))
        .unwrap_or_else(|| panic!("the dialog was never asked to rebuild: {lines:#?}"));
    assert!(
        ended.starts_with("D-Streamy stopped: ")
            && ended.ends_with(". Press Start to go live again"),
        "{ended}"
    );
    assert!(
        lines.contains(&"HARNESS tools stopped active=0".to_owned()),
        "{lines:#?}"
    );
    assert!(
        lines.contains(
            &"HARNESS settings_property when=after_stop name=start type=8 enabled=1 desc=Start"
                .to_owned()
        ),
        "{lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_stop_the_service_cannot_answer_says_what_to_do_next() {
    let h = Harness::new("");
    let serve = default_serve(&h);
    let config = picked_config(1);
    let go = h.dir.path().join("stop-now");
    let go_arg = go.to_string_lossy().into_owned();
    let hint =
        "The Discord session may still be live: press Start to take it over, then Stop again";
    let run = tools_run(
        &h,
        config.path(),
        &[
            "--tools-stop-after",
            &go_arg,
            "--tools-expect",
            hint,
            "--seconds",
            "60",
            "--until-video",
            "30",
        ],
    );
    wait_for_line(&run, "live video", |l| {
        l.starts_with("HARNESS tools video=")
    });
    let pid = i32::try_from(serve.pid()).unwrap();
    // SAFETY: signals the serve child this test spawned.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
    std::fs::write(&go, "").unwrap();
    let (status, lines) = run.finish();
    // SAFETY: as above.
    unsafe { libc::kill(pid, libc::SIGCONT) };
    passed(status, &lines);
    let stopped = lines
        .iter()
        .find_map(|l| l.strip_prefix("HARNESS settings_status when=stopped status="))
        .unwrap_or_else(|| panic!("{lines:#?}"));
    assert_eq!(
        stopped,
        format!(
            "Stop didn't finish: The D-Streamy service didn't answer hello within 10 s. {hint}"
        )
    );
    assert!(
        lines
            .iter()
            .any(|l| l
                .starts_with("HARNESS settings_refreshed when=stopped status=Stop didn't finish")),
        "{lines:#?}"
    );
    assert!(
        lines.contains(&"HARNESS tools stopped active=0".to_owned()),
        "{lines:#?}"
    );
    assert!(
        lines.contains(&"HARNESS settings_info when=after_stop name=status kind=2".to_owned()),
        "{lines:#?}"
    );
    assert!(
        read_module_config(config.path())["owned"].is_object(),
        "kept, so a later Start can take the session over"
    );
}

/// Milliseconds from the frontend's exit event to the end of `obs_shutdown`.
fn closing_ms(lines: &[String]) -> u64 {
    let exit = lines
        .iter()
        .find_map(|l| {
            l.strip_prefix("HARNESS frontend_exit ")?
                .split(" ms=")
                .nth(1)?
                .parse::<u64>()
                .ok()
        })
        .unwrap_or_else(|| panic!("no frontend exit in the harness output: {lines:#?}"));
    exit + number_after(lines, "HARNESS obs_shutdown ms=")
}

/// The plugin shut down once, on the frontend's exit event, and left no output for `obs_shutdown` to stop after audio.
fn assert_closed_on_frontend_exit(lines: &[String]) {
    let position = |text: &str| lines.iter().position(|l| l.contains(text));
    let exiting =
        position("OBS's frontend is exiting: D-Streamy stops before OBS stops audio and video");
    let shutdown = position("HARNESS obs_shutdown begins dstreamy_output=0 active=0");
    let closing = "OBS is closing: D-Streamy finishes its control steps";
    let ended = position("the D-Streamy control thread ended in ");
    assert!(
        matches!((exiting, ended, shutdown), (Some(a), Some(b), Some(c)) if a < b && b < c),
        "the control thread ended on the exit event, before obs_shutdown: {lines:#?}"
    );
    assert_eq!(
        lines.iter().filter(|l| l.contains(closing)).count(),
        1,
        "unload after the exit event shuts down nothing again: {lines:#?}"
    );
    assert!(
        has_line(lines, "HARNESS frontend_exit callbacks=1 "),
        "the plugin watched the frontend's exit: {lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn closing_obs_during_a_start_never_streams_and_unloads_promptly() {
    for answered in [true, false] {
        let h = Harness::new("");
        let _serve = default_serve(&h);
        let config = picked_config(1);
        let gate = h.dir.path().join("start-gate");
        let run = tools_run_gated(
            &h,
            config.path(),
            &[
                "--tools-exit-while-starting",
                "--seconds",
                "20",
                "--until-video",
                "1",
            ],
            &gate,
        );
        wait_for_line(&run, "the closing log", |l| {
            l.contains("OBS is closing: D-Streamy finishes its control steps")
        });
        if answered {
            std::fs::write(&gate, "").unwrap();
        }
        let (status, lines) = run.finish();
        passed(status, &lines);
        asked_first(&lines, "start", 0, "Go live in ");
        assert!(
            lines.contains(&"HARNESS tools exiting status=Starting on Track 1\u{2026}".to_owned()),
            "{lines:#?}"
        );
        assert!(
            !has_plugin_line(&lines, "started from Tools"),
            "{answered}: {lines:#?}"
        );
        let closing_ms = closing_ms(&lines);
        assert!(
            closing_ms < 12_000,
            "{answered}: closing took {closing_ms} ms"
        );
        assert_closed_on_frontend_exit(&lines);
        if answered {
            assert!(
                has_plugin_line(
                    &lines,
                    "D-Streamy stopped the Discord session it started, before streaming"
                ),
                "{lines:#?}"
            );
            let session = serve_session(&h);
            assert_eq!(session["state"], "stopped", "{session}");
            assert_eq!(session["reason"]["reason"], "requested", "{session}");
            assert_eq!(read_module_config(config.path())["owned"], Value::Null);
        } else {
            assert!(
                has_plugin_line(
                    &lines,
                    "OBS is closing: D-Streamy stopped waiting for the Discord session it was starting"
                ),
                "{lines:#?}"
            );
        }
    }
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn closing_obs_while_live_stops_the_output_then_the_session_promptly() {
    for answered in [true, false] {
        let h = Harness::new("");
        let serve = default_serve(&h);
        let config = picked_config(1);
        let go = h.dir.path().join("exit-now");
        let go_arg = go.to_string_lossy().into_owned();
        let run = tools_run(
            &h,
            config.path(),
            &[
                "--tools-exit-while-live",
                "--tools-stop-after",
                &go_arg,
                "--seconds",
                "60",
                "--until-video",
                "30",
            ],
        );
        wait_for_line(&run, "live video", |l| {
            l.starts_with("HARNESS tools video=")
        });
        let pid = i32::try_from(serve.pid()).unwrap();
        if !answered {
            // SAFETY: signals the serve child this test spawned.
            assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
        }
        std::fs::write(&go, "").unwrap();
        let (status, lines) = run.finish();
        if !answered {
            // SAFETY: as above.
            unsafe { libc::kill(pid, libc::SIGCONT) };
        }
        passed(status, &lines);
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("HARNESS tools exiting status=Live on Track 1")),
            "{answered}: {lines:#?}"
        );
        assert!(
            has_plugin_line(
                &lines,
                "OBS is closing while D-Streamy is live: it stops its output, then the Discord session"
            ),
            "{answered}: {lines:#?}"
        );
        let position = |pred: &dyn Fn(&str) -> bool| lines.iter().position(|l| pred(l));
        let closing_ms = closing_ms(&lines);
        let bound = if answered { 4_000 } else { 8_000 };
        assert!(
            closing_ms < bound,
            "{answered}: closing took {closing_ms} ms"
        );
        if !answered {
            assert!(
                closing_ms >= 4_000,
                "the frozen service was waited on for its 5 s budget: {closing_ms} ms"
            );
        }
        assert_closed_on_frontend_exit(&lines);
        let stopped = position(&|l| l.starts_with("HARNESS tools output_stopped code=0"));
        let shutdown = position(&|l| l.starts_with("HARNESS obs_shutdown begins"));
        assert!(
            matches!((stopped, shutdown), (Some(a), Some(b)) if a < b),
            "{answered}: the output stopped before obs_shutdown stopped audio: {lines:#?}"
        );
        let begins = position(&|l| l.contains("it stops its output, then the Discord session"));
        let output_stopped = position(&|l| l.starts_with("HARNESS tools output_stopped code=0"));
        let ended = position(&|l| {
            l.contains("[obs-dstreamy] ")
                && (l.contains("stopped its output and the Discord session")
                    || l.contains("may keep running until the D-Streamy service's grace period"))
        });
        assert!(
            matches!((begins, output_stopped, ended), (Some(a), Some(b), Some(c)) if a < b && b < c),
            "{answered}: output stop must happen between the plugin's start and end lines: {lines:#?}"
        );
        let owned = read_module_config(config.path())["owned"].clone();
        if answered {
            assert!(
                has_plugin_line(
                    &lines,
                    "OBS is closing: D-Streamy stopped its output and the Discord session"
                ),
                "{lines:#?}"
            );
            let session = serve_session(&h);
            assert_eq!(session["state"], "stopped", "{session}");
            assert_eq!(session["reason"]["reason"], "requested", "{session}");
            assert_eq!(owned, Value::Null);
        } else {
            assert!(
                has_plugin_line(
                    &lines,
                    "the Discord session D-Streamy started may keep running until the D-Streamy service's grace period ends"
                ),
                "{lines:#?}"
            );
            assert!(owned.is_object(), "kept for the next Start: {owned}");
        }
    }
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
    let (lines, recorded) = rescaled_stream(
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

/// Advanced Audio Properties › Tracks routes the harness tone to Track 2 only. Track 2 carries it to Discord;
/// Track 1 sends silence at the same pace, and the video is the same either way.
#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn only_the_chosen_mixer_track_carries_a_source_routed_to_it() {
    let peak = |track: u8| {
        let (lines, recorded) = stream(json!({"audio_track": track}), &["--audio-mixers", "2"], 90);
        assert!(
            lines.contains(&"HARNESS audio_source mixers=0x2".to_owned()),
            "{lines:#?}"
        );
        let video = stream_video(&recorded);
        let audio = producer_audio(&recorded);
        let video_us = rtp_us(video.last().unwrap().rtp_timestamp, VIDEO_HZ)
            - rtp_us(video[0].rtp_timestamp, VIDEO_HZ);
        let audio_us = i64::try_from(audio.len()).unwrap() * 20_000;
        assert!(
            audio_us * 10 >= video_us * 8,
            "Track {track}: {audio_us} µs of audio for {video_us} µs of video"
        );
        increasing(
            "audio",
            &audio.iter().map(|a| a.rtp_timestamp).collect::<Vec<_>>(),
        );
        pcm(&audio).iter().fold(0f32, |m, s| m.max(s.abs()))
    };
    let routed = peak(2);
    let other = peak(1);
    assert!(routed > 0.02, "Track 2 carries the tone: peak {routed}");
    assert!(other < 0.001, "Track 1 stays silent: peak {other}");
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn cancel_in_the_dialog_puts_back_and_saves_what_it_opened_with() {
    let config = picked_config(2);
    let dir = config.path().to_string_lossy().into_owned();
    let lib = fake_frontend();
    let changes = json!({"guild_id": OTHER_GUILD_ID, "audio_track": 5}).to_string();
    let lines = report_ui(&[
        "--fake-frontend",
        lib.as_str(),
        "--module-config",
        dir.as_str(),
        "--settings-update",
        &changes,
        "--settings-cancel",
    ]);
    let opened = format!("guild_id={MOCK_GUILD_ID} channel_id={MOCK_VOICE_CHANNEL_ID}");
    for want in [
        format!("HARNESS pick when=updated guild_id={OTHER_GUILD_ID} channel_id="),
        "HARNESS settings_source when=updated audio_track=5".to_owned(),
        "HARNESS check_settings when=cancelled differ=0".to_owned(),
        format!("HARNESS pick when=cancelled {opened}"),
        "HARNESS settings_source when=cancelled audio_track=2".to_owned(),
        format!("HARNESS pick when=reopened {opened}"),
        "HARNESS settings_source when=reopened audio_track=2".to_owned(),
    ] {
        assert!(lines.contains(&want), "{want}: {lines:#?}");
    }
    let saved = read_module_config(config.path());
    assert_eq!(saved["guild_id"], MOCK_GUILD_ID, "{saved}");
    assert_eq!(saved["channel_id"], MOCK_VOICE_CHANNEL_ID, "{saved}");
    assert_eq!(saved["audio_track"], 2, "{saved}");
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn pick_a_server_in_the_dialog_clears_the_server_and_its_channel() {
    let config = picked_config(3);
    let dir = config.path().to_string_lossy().into_owned();
    let lib = fake_frontend();
    let lines = report_ui(&[
        "--fake-frontend",
        lib.as_str(),
        "--module-config",
        dir.as_str(),
        "--settings-update",
        r#"{"guild_id": ""}"#,
    ]);
    for want in [
        format!(
            "HARNESS pick when=open guild_id={MOCK_GUILD_ID} channel_id={MOCK_VOICE_CHANNEL_ID}"
        ),
        "HARNESS pick when=updated guild_id= channel_id=".to_owned(),
        "HARNESS pick when=reopened guild_id= channel_id=".to_owned(),
        "HARNESS settings_source when=reopened audio_track=3".to_owned(),
    ] {
        assert!(lines.contains(&want), "{want}: {lines:#?}");
    }
    let saved = read_module_config(config.path());
    assert_eq!(saved["guild_id"], Value::Null, "{saved}");
    assert_eq!(saved["channel_id"], Value::Null, "{saved}");
    assert_eq!(saved["audio_track"], 3, "{saved}");
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_panic_after_the_output_started_stops_it_and_its_session() {
    let h = Harness::new("");
    let _serve = default_serve(&h);
    let config = picked_config(1);
    let cleaned = "so it stopped what it had started. See the OBS log, then press Start again";
    let (status, lines) = tools_run_env(
        &h,
        config.path(),
        &[
            "--tools-expect",
            cleaned,
            "--expect-panics",
            "1",
            "--seconds",
            "30",
            "--until-video",
            "1",
        ],
        &[(
            "DSTREAMY_TEST_PANIC_AFTER_OUTPUT_START",
            std::path::Path::new("1"),
        )],
    )
    .finish();
    passed(status, &lines);
    let shown = lines
        .iter()
        .find_map(|l| l.strip_prefix("HARNESS settings_status when=live status="))
        .unwrap_or_else(|| panic!("{lines:#?}"));
    assert_eq!(
        shown,
        format!(
            "D-Streamy's control step failed (panic caught in control: test panic after the output started), {cleaned}"
        )
    );
    assert!(
        lines.contains(&"HARNESS tools output=1 active=0".to_owned()),
        "the output it started is stopped: {lines:#?}"
    );
    let session = serve_session(&h);
    assert_eq!(session["state"], "stopped", "{session}");
    assert_eq!(session["reason"]["reason"], "requested", "{session}");
    assert_eq!(read_module_config(config.path())["owned"], Value::Null);
    for (name, label, enabled) in [("start", "Start", 1), ("stop", "Stop", 0)] {
        let want = format!(
            "HARNESS settings_property when=after_stop name={name} type=8 enabled={enabled} desc={label}"
        );
        assert!(lines.contains(&want), "{want}: {lines:#?}");
    }
}

/// A real OBS frontend, not the harness: OBS loads the plugin from the user's plugin folder like any third-party
/// plugin, Tools > D-Streamy… opens OBS's own Qt properties dialog, and that dialog rebuilds itself when the
/// server list arrives from the service after it opened.
#[test]
#[ignore = "needs OBS with its frontend, the built plugin and an X server; obs-ci runs it"]
fn real_obs_shows_tools_d_streamy_and_rebuilds_its_dialog_from_the_service() {
    use std::os::unix::fs::symlink;
    let obs = PathBuf::from(
        std::env::var("DSTREAMY_OBS_FRONTEND")
            .expect("DSTREAMY_OBS_FRONTEND is unset: point it at OBS's obs binary"),
    );
    let env = obs_env();
    let h = Harness::new("");
    let _serve = default_serve(&h);
    let home = h.dir.path().join("obs-home");
    let cfg = home.join("obs-studio");
    // As obs/tests/frontend/frontend-smoke.sh: no first-run wizard, no update check.
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("global.ini"),
        "[General]\nLastVersion=537001986\nPre31Migrated=true\nEnableAutoUpdates=false\nInfoIncrement=99999\n",
    )
    .unwrap();
    std::fs::write(
        cfg.join("user.ini"),
        "[General]\nFirstRun=true\nPre19Defaults=false\nPre21Defaults=false\nPre23Defaults=false\nPre24.1Defaults=false\n",
    )
    .unwrap();
    let plugin = cfg.join("plugins").join("obs-dstreamy");
    std::fs::create_dir_all(plugin.join("bin").join("64bit")).unwrap();
    symlink(
        PathBuf::from(&env.plugin_bin).join("obs-dstreamy.so"),
        plugin.join("bin").join("64bit").join("obs-dstreamy.so"),
    )
    .unwrap();
    std::fs::create_dir_all(plugin.join("data")).unwrap();
    write_module_config(
        &cfg.join("plugin_config"),
        json!({"guild_id": MOCK_GUILD_ID, "channel_id": MOCK_VOICE_CHANNEL_ID, "audio_track": 2}),
    );
    let open = h.dir.path().join("open-dialog");
    let gate = h.dir.path().join("lists-gate");
    let out = std::fs::File::create(h.dir.path().join("obs-stdout.log")).unwrap();
    let child = Command::new(&obs)
        .args(["--multi", "--disable-updater", "--verbose"])
        .env("XDG_CONFIG_HOME", &home)
        .env("XDG_RUNTIME_DIR", runtime_dir(&h))
        .env("DSTREAMY_TEST_OPEN_DIALOG", &open)
        .env("DSTREAMY_TEST_LISTS_GATE", &gate)
        .env("LIBGL_ALWAYS_SOFTWARE", "1")
        .current_dir(obs.parent().unwrap())
        .stdin(Stdio::null())
        .stdout(out.try_clone().unwrap())
        .stderr(out)
        .spawn()
        .unwrap_or_else(|e| panic!("{}: {e}", obs.display()));
    let mut obs_run = KillOnDrop(child);
    let logs = cfg.join("logs");
    let log = || -> String {
        let newest = std::fs::read_dir(&logs).ok().and_then(|entries| {
            entries
                .filter_map(Result::ok)
                .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        });
        newest
            .and_then(|e| std::fs::read_to_string(e.path()).ok())
            .unwrap_or_default()
    };
    let wait = |what: &str, done: &dyn Fn(&str) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let text = log();
            if done(&text) {
                return text;
            }
            assert!(
                Instant::now() < deadline,
                "OBS never logged {what}:\n{text}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    wait("its startup", &|l| {
        l.contains("[obs-dstreamy] added Tools > D-Streamy") && l.contains("==== Startup complete")
    });
    std::fs::write(&open, "").unwrap();
    let first = "[obs-dstreamy] test hook: dialog built servers=0 ";
    wait("the dialog", &|l| l.contains(first));
    std::fs::write(&gate, "").unwrap();
    let rebuilt = "[obs-dstreamy] test hook: dialog built servers=1 ";
    let text = wait("the rebuilt dialog", &|l| l.contains(rebuilt));
    assert!(
        text.find(first) < text.find(rebuilt),
        "OBS built the dialog with the servers only after they loaded:\n{text}"
    );
    // SAFETY: signals the OBS child this test spawned; OBS closes its window on SIGINT.
    unsafe { libc::kill(i32::try_from(obs_run.0.id()).unwrap(), libc::SIGINT) };
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = obs_run.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "OBS did not exit:\n{}", log());
        std::thread::sleep(Duration::from_millis(100));
    };
    let text = log();
    assert!(status.success(), "OBS exited with {status}:\n{text}");
    assert!(text.contains("[obs-dstreamy] unloaded"), "{text}");
    assert!(!text.contains("[obs-dstreamy] panic caught"), "{text}");
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
