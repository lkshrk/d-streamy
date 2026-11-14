//! Real OBS through the plugin: `obs-harness` loads `obs-dstreamy` and streams to `serve` + mock Discord; obs-ci only.
#![cfg(target_os = "linux")]

#[path = "support/obs_events.rs"]
mod obs_events;
mod support;
use obs_events::{event, event_matches, field, has_event, line_starts};

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
    /// Plugin observations with the time the test read them.
    observed: Arc<Mutex<Vec<(Instant, String)>>>,
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
        let observed = Arc::new(Mutex::new(Vec::new()));
        let mut readers = Vec::new();
        let out: Box<dyn std::io::Read + Send> = Box::new(child.stdout.take().unwrap());
        let err: Box<dyn std::io::Read + Send> = Box::new(child.stderr.take().unwrap());
        for pipe in [out, err] {
            let sink = lines.clone();
            let stamps = observed.clone();
            readers.push(std::thread::spawn(move || {
                for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                    eprintln!("obs-harness: {line}");
                    if let Some(name) =
                        event(&line).and_then(|row| row["event"].as_str().map(str::to_owned))
                    {
                        stamps.lock().unwrap().push((Instant::now(), name));
                    }
                    if !line.starts_with("HARNESS ") {
                        sink.lock().unwrap().push(line);
                    }
                }
            }));
        }
        Self {
            child,
            lines,
            observed,
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
    dir.canonicalize().unwrap()
}

/// Runs `f` with `serve` stopped by SIGSTOP, then lets it continue.
fn frozen<R>(serve: &Proc, f: impl FnOnce() -> R) -> R {
    let pid = libc::pid_t::try_from(serve.pid()).unwrap();
    // SAFETY: kill has no memory preconditions; serve is the test's unreaped child.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
    let result = f();
    // SAFETY: as above.
    unsafe { libc::kill(pid, libc::SIGCONT) };
    result
}

fn serve_endpoint(h: &Harness) -> PathBuf {
    runtime_dir(h).join("dstreamy").join("serve.sock")
}

fn passed(status: ExitStatus, lines: &[String]) {
    assert!(
        status.success()
            && lines
                .iter()
                .any(|l| event_matches(l, "HARNESS result=PASS", false)),
        "obs-harness failed ({status}): {lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn concurrent_c_and_rust_observations_preserve_complete_records() {
    let h = Harness::new("");
    let (status, lines) = HarnessRun::start(&h, json!({}), &["--observation-framing"]).finish();
    passed(status, &lines);
    let payload = "Grüße 🧃".repeat(1024);
    assert!(payload.len() > 4096);
    for name in ["c_framing_probe", "plugin_framing_probe"] {
        let rows: Vec<_> = lines
            .iter()
            .filter_map(|line| event(line))
            .filter(|row| row["event"] == name)
            .collect();
        assert_eq!(rows.len(), 64, "every {name} record must survive");
        let mut sequences = std::collections::BTreeSet::new();
        for row in rows {
            sequences.insert(row["sequence"].as_u64().unwrap());
            assert_eq!(row["payload"].as_str(), Some(payload.as_str()));
            assert_eq!(row["note"], "first\nsecond");
            if name == "plugin_framing_probe" {
                assert_eq!(row["nul"], "\0");
            }
        }
        assert_eq!(sequences, (0..64).collect());
    }
    assert!(has_event(&lines, "HARNESS framing_complete records=64"));
}

/// The dialog's `name` (start or stop) asked a question starting with `question` and changed nothing itself.
fn asked_first(lines: &[String], name: &str, output_active: u8, question: &str) {
    assert!(
        lines.iter().filter_map(|line| event(line)).any(|row| {
            row["event"] == "settings_asked"
                && row["name"] == name
                && row["output_active"].as_u64() == Some(u64::from(output_active))
                && row["question"]
                    .as_str()
                    .is_some_and(|text| text.starts_with(question))
        }),
        "no matching confirm question: {lines:#?}"
    );
    let clicked = lines
        .iter()
        .find(|l| {
            line_starts(
                l,
                &format!("HARNESS settings_clicked name={name} enabled=1 "),
            )
        })
        .unwrap_or_else(|| panic!("no {name} click: {lines:#?}"));
    assert!(
        field(clicked, "status").is_none_or(|status| !status.starts_with("Starting") && !status.starts_with("Stopping")),
        "{name} alone acts on nothing: {clicked}"
    );
}

fn has_line(lines: &[String], prefix: &str) -> bool {
    lines.iter().any(|l| line_starts(l, prefix))
}

fn event_number(lines: &[String], name: &str, key: &str) -> u64 {
    lines
        .iter()
        .filter_map(|line| event(line))
        .find_map(|row| (row["event"] == name).then(|| row[key].as_u64()).flatten())
        .unwrap_or_else(|| panic!("no {name}.{key} in the harness output"))
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
        plugin_observed(&lines, "own_video_encoder", json!({"gpu_mix": false})),
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
            let row = event(l)?;
            (row["event"] == "plugin_keyframe_sent").then_some(())?;
            Some((row["pts_us"].as_i64()?, row["arrived_ns"].as_i64()?))
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
    let base = i64::try_from(event_number(&lines, "markers", "base_ns")).unwrap();
    let period = i64::try_from(event_number(&lines, "markers", "period_ns")).unwrap();
    let capture = i64::try_from(event_number(&lines, "capture_start", "ns")).unwrap();
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
    let (status, lines) = frozen(&serve, || run.finish());
    passed(status, &lines);
    assert!(
        plugin_observed(&lines, "stop_drain_timed_out", json!({"deadline_ms":1500})),
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
    let took = event_number(&lines, "release_during_start", "ms");
    assert!(took < 2_000, "release took {took} ms");
    assert!(
        has_line(&lines, "HARNESS stop_signals=0 "),
        "no capture began: {lines:#?}"
    );
    drop(held);
}

struct LateService {
    complete: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for LateService {
    fn drop(&mut self) {
        self.complete.take();
        let joined = self.thread.take().unwrap().join();
        if !std::thread::panicking() {
            joined.unwrap();
        }
    }
}

/// A service that replies `attached` after `reply_ms`, sends `detached` at `detached_ms` and never reads.
fn late_service(h: &Harness, reply_ms: u64, detached_ms: u64) -> (PathBuf, LateService) {
    use std::io::{BufRead, Write};
    let dir = h.run.path().join("late");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let path = dir.join("s.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let (complete, completed) = std::sync::mpsc::channel();
    let service = std::thread::spawn(move || {
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if completed.recv_timeout(Duration::from_millis(20))
                        != Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    {
                        return;
                    }
                }
                Err(e) => panic!("accept: {e}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let accepted = Instant::now();
        let mut read = BufReader::new(stream.try_clone().unwrap());
        let mut hello = String::new();
        while !hello.ends_with('\n') {
            if completed.try_recv() != Err(std::sync::mpsc::TryRecvError::Empty) {
                return;
            }
            match read.read_line(&mut hello) {
                Ok(0) => return,
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => panic!("hello: {e}"),
            }
        }
        if completed
            .recv_timeout(Duration::from_millis(reply_ms).saturating_sub(accepted.elapsed()))
            != Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        {
            return;
        }
        stream.write_all(b"{\"attached\":1}\n").unwrap();
        if completed
            .recv_timeout(Duration::from_millis(detached_ms).saturating_sub(accepted.elapsed()))
            != Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        {
            return;
        }
        let _ = stream.write_all(
            b"{\"message\":\"late test\",\"reason\":\"incompatible\",\"type\":\"detached\"}\n",
        );
        let _ = completed.recv();
    });
    (
        path,
        LateService {
            complete: Some(complete),
            thread: Some(service),
        },
    )
}

#[test]
fn late_service_teardown_cancels_without_a_connection_or_complete_hello() {
    use std::io::Write;
    for connect in [false, true] {
        let h = Harness::new("");
        let (path, service) = late_service(&h, 800, 2600);
        let _peer = connect.then(|| {
            let mut stream = std::os::unix::net::UnixStream::connect(path).unwrap();
            stream.write_all(b"{partial").unwrap();
            stream
        });
        let started = Instant::now();
        drop(service);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "cancellation must join before hello too"
        );
    }
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_native_capture_reattaches_with_decodable_video_and_audio() {
    use std::io::{Read, Write};
    let h = Harness::new("");
    let directory = h.run.path().join("reattach");
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(
        &directory,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let path = directory.join("s.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let service = std::thread::spawn(move || {
        let mut second_video = Vec::new();
        let mut second_audio = 0;
        let mut first_last_pts = 0;
        for attempt in 0..2 {
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    other => panic!("producer did not attach: {other:?}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(15)))
                .unwrap();
            let mut read = BufReader::new(stream.try_clone().unwrap());
            let mut hello = String::new();
            read.read_line(&mut hello).unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&hello).unwrap()["role"],
                "producer"
            );
            stream
                .write_all(format!("{{\"attached\":{attempt}}}\n").as_bytes())
                .unwrap();
            let mut video_count = 0;
            loop {
                let mut length = [0; 4];
                read.read_exact(&mut length).unwrap();
                let length = u32::from_be_bytes(length) as usize;
                assert!((10..=(8 << 20)).contains(&length));
                let mut frame = vec![0; length];
                read.read_exact(&mut frame).unwrap();
                let pts = i64::from_be_bytes(frame[1..9].try_into().unwrap());
                match frame[0] {
                    1 => {
                        video_count += 1;
                        if attempt == 0 {
                            first_last_pts = pts;
                        } else {
                            second_video.push((pts, frame[9] & 1 != 0, frame[10..].to_vec()));
                        }
                    }
                    2 if attempt == 1 => second_audio += 1,
                    3 => break,
                    _ => {}
                }
                if attempt == 0 && video_count == 30 {
                    break;
                }
            }
        }
        (first_last_pts, second_video, second_audio)
    });
    let (status, lines) = HarnessRun::start(
        &h,
        json!({"endpoint":path}),
        &["--seconds", "30", "--until-video", "180"],
    )
    .finish();
    passed(status, &lines);
    let (before, video, audio) = service.join().unwrap();
    assert!(
        video.len() >= 60 && audio >= 60,
        "capture must survive reattachment: {} video, {audio} audio",
        video.len()
    );
    assert!(
        video[0].1 && video[0].0 > before,
        "reattach starts at a fresh IDR, never replaying the old one"
    );
    assert!(video.windows(2).all(|pair| pair[0].0 < pair[1].0));
    let mut decoder = openh264::decoder::Decoder::new().unwrap();
    let pictures = video
        .iter()
        .filter(|(_, _, bytes)| decoder.decode(bytes).unwrap().is_some())
        .count();
    assert!(
        pictures >= 60,
        "post-reattach frames decode independently: {pictures}"
    );
}

/// Capture begins after libobs's active check in destroy (the hook delays our flag by 1.5 s).
fn release_during_a_late_capture(detached_ms: u64) -> Vec<String> {
    let h = Harness::new("");
    let (path, _service) = late_service(&h, 800, detached_ms);
    let clean_stop = detached_ms > 2000;
    let mut args = vec![
        "--seconds",
        "60",
        "--until-video",
        "100000",
        "--bitrate",
        "8000",
        "--release-after-start",
        "500",
        "--verbose",
    ];
    if !clean_stop {
        args.extend(["--expect-error", "late test"]);
    }
    let run = HarnessRun::start(
        &h,
        json!({"endpoint": path, "test_destroy_delay_ms": 1500}),
        &args,
    );
    let (status, lines) = run.finish();
    passed(status, &lines);
    assert!(
        plugin_observed(&lines, "attached", json!({"attachment":1})),
        "capture never began during destroy: {lines:#?}"
    );
    assert!(
        has_line(
            &lines,
            if clean_stop {
                "HARNESS output stopped with code 0: "
            } else {
                "HARNESS output stopped with code -4: The D-Streamy service ended the stream (incompatible): late test"
            }
        ),
        "{lines:#?}"
    );
    assert!(
        has_line(
            &lines,
            if clean_stop {
                "HARNESS stop_signals=1 error_stop_signals=0"
            } else {
                "HARNESS stop_signals=1 error_stop_signals=1"
            }
        ),
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
fn a_late_capture_preserves_consumed_shutdown_when_the_peer_ends_during_drain() {
    let lines = release_during_a_late_capture(2_600);
    let stop = lines
        .iter()
        .position(|line| event(line).is_some_and(|row| row["event"] == "plugin_stop_drain_started"))
        .expect("the writer took the stop");
    let detached = lines
        .iter()
        .position(|line| {
            event(line).is_some_and(|row| row["event"] == "output_stop" && row["code"] == 0)
        })
        .expect("the requested Shutdown ended cleanly");
    assert!(
        stop < detached,
        "the Shutdown was not taken before the detached: {lines:#?}"
    );
}

fn plugin_observed(lines: &[String], name: &str, fields: Value) -> bool {
    lines.iter().filter_map(|line| event(line)).any(|row| {
        row["event"] == format!("plugin_{name}")
            && fields
                .as_object()
                .unwrap()
                .iter()
                .all(|(key, value)| row.get(key) == Some(value))
    })
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
    let (recorded, dropped) = settled_video(&h, &mut c);
    {
        let v = stream_video(&recorded);
        let a = producer_audio(&recorded);
        let minimum = video * 9 / 10;
        assert!(
            !v.is_empty() && v.len() as u64 + dropped >= minimum as u64,
            "{} video frames reached the mock and serve dropped {dropped}, under {minimum}",
            v.len()
        );
        decodes(&v, &a);
    }
    (lines, recorded)
}

/// The mock's recording once serve's health stats cover every frame it sent, and the video serve dropped to stay live.
fn settled_video(h: &Harness, c: &mut Client) -> (dstreamy_mock_discord::Recorded, u64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let stats = c.status()["stats"].clone();
        let recorded = h.mock.recorded();
        let sent = stats["video_frames_sent"].as_u64();
        if sent == Some(stream_video(&recorded).len() as u64) || Instant::now() >= deadline {
            let dropped = ["video_frames_dropped", "source_video_frames_dropped"]
                .iter()
                .map(|key| stats[*key].as_u64().unwrap_or(0))
                .sum();
            return (recorded, dropped);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
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
        plugin_observed(&lines, "shared_video_encoder", json!({"id": "obs_x264"})),
        "{lines:#?}"
    );
    assert!(
        !plugin_observed(&lines, "own_video_encoder", json!({})),
        "{lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn custom_x264_profile_override_uses_a_safe_own_encoder() {
    let (lines, _) = stream(
        json!({"mode":"share"}),
        &[
            "--preset",
            "veryfast",
            "--profile",
            "baseline",
            "--keyint-sec",
            "2",
            "--x264opts",
            "profile=main",
        ],
        90,
    );
    assert!(plugin_observed(
        &lines,
        "share_refused",
        json!({"custom_options":true})
    ));
    assert!(plugin_observed(
        &lines,
        "own_video_encoder",
        json!({"id":"obs_x264"})
    ));
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
        plugin_observed(
            &lines,
            "share_refused",
            json!({"id": "obs_x264", "b_frames": true})
        ),
        "{lines:#?}"
    );
    assert!(
        plugin_observed(&lines, "own_video_encoder", json!({"id": "obs_x264"})),
        "{lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn the_own_encoder_scales_a_1440p_canvas_to_1080p() {
    let (lines, recorded) = stream(json!({}), &["--width", "2560", "--height", "1440"], 60);
    assert!(
        plugin_observed(
            &lines,
            "own_video_encoder",
            json!({"id": "obs_x264", "width": 1920, "height": 1080})
        ) && plugin_observed(
            &lines,
            "own_video_encoder",
            json!({"gpu_mix": true, "format": "NV12"})
        ),
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
        plugin_observed(
            &lines,
            "own_video_encoder",
            json!({"width": 640, "height": 360, "fps": 30})
        ),
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
        plugin_observed(
            &lines,
            "own_video_encoder",
            json!({"width": 640, "height": 360, "fps": 60})
        ),
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
        plugin_observed(
            &lines,
            "audio_encoder_replaced",
            json!({"previous": "ffmpeg_aac", "current": "ffmpeg_opus"})
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
    let at = |name: &str| {
        lines
            .iter()
            .position(|line| event(line).is_some_and(|row| row["event"] == name))
    };
    let fell_back = at("plugin_share_refused").expect("first start falls back");
    let restarted = lines
        .iter()
        .position(|l| event_matches(l, "HARNESS restarting the output", false))
        .expect("a restart");
    let shared = at("plugin_shared_video_encoder").expect("the restart shares");
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
        plugin_observed(&lines, "share_refused", json!({"inactive": true})),
        "{lines:#?}"
    );
    assert!(
        plugin_observed(&lines, "own_video_encoder", json!({"id": "obs_x264"})),
        "{lines:#?}"
    );
}

fn share_config(fallback: bool) -> tempfile::TempDir {
    picked_config_with(1, json!({"mode": "share", "share_fallback": fallback}))
}

/// Tools > D-Streamy in share mode, next to the fake frontend's main stream on the harness's x264 with `bf`.
fn share_session(h: &Harness, fallback: bool, bf: &str, extra: &[&str]) -> Vec<String> {
    share_session_env(h, fallback, bf, extra, &[]).0
}

/// [`share_session`] with plugin test variables; also when the test read each plugin observation.
fn share_session_env(
    h: &Harness,
    fallback: bool,
    bf: &str,
    extra: &[&str],
    vars: &[(&str, &std::path::Path)],
) -> (Vec<String>, Vec<(Instant, String)>) {
    let config = share_config(fallback);
    let mut args = vec![
        "--video-encoder",
        "obs_x264",
        "--audio-encoder",
        "ffmpeg_aac",
        "--keyint-sec",
        "2",
        "--bf",
        bf,
        "--seconds",
        "60",
        "--until-video",
        "30",
    ];
    args.extend_from_slice(extra);
    let run = tools_run_env(h, config.path(), &args, vars);
    let observed = run.observed.clone();
    let (status, lines) = run.finish();
    passed(status, &lines);
    let observed = observed.lock().unwrap().clone();
    (lines, observed)
}

fn read_at(observed: &[(Instant, String)], name: &str) -> Instant {
    let event_name = format!("plugin_{name}");
    observed
        .iter()
        .find_map(|(at, n)| (*n == event_name).then_some(*at))
        .unwrap_or_else(|| panic!("no {event_name}: {observed:?}"))
}

fn is_idr(frame: &RecordedVideoFrame) -> bool {
    frame
        .data
        .windows(4)
        .any(|w| w[..3] == [0, 0, 1] && w[3] & 0x1f == 5)
}

/// Video the mock received after `at`, from its first keyframe on.
fn video_after(h: &Harness, at: Instant) -> usize {
    let recorded = h.mock.recorded();
    let after: Vec<_> = stream_video(&recorded)
        .into_iter()
        .filter(|v| v.received_at > at)
        .skip_while(|v| !is_idr(v))
        .collect();
    after.len()
}

/// The checks every move from the shared to the own encoder passes, with the session kept live.
fn assert_moved_to_own_encoder(h: &Harness, lines: &[String], observed: &[(Instant, String)]) {
    let order = [
        "main_stream_starting",
        "shared_video_encoder",
        "main_stream_ending",
        "leaving_shared_encoder",
        "own_video_encoder",
        "switched_to_own_encoder",
    ]
    .map(|name| observed_at(lines, name));
    assert!(order.is_sorted(), "{order:?}: {lines:#?}");
    assert!(
        has_event(lines, "HARNESS settings_status when=stopped status=Idle"),
        "{lines:#?}"
    );
    let session = serve_session(h);
    assert_eq!(
        session["reason"]["reason"], "requested",
        "only Stop ended the session: {session}"
    );
    let after = video_after(h, read_at(observed, "switched_to_own_encoder"));
    assert!(
        after >= 20,
        "{after} frames reached Discord after the switch"
    );
}

fn observed_at(lines: &[String], name: &str) -> usize {
    let event_name = format!("plugin_{name}");
    lines
        .iter()
        .position(|l| event(l).is_some_and(|row| row["event"] == event_name))
        .unwrap_or_else(|| panic!("no {event_name}: {lines:#?}"))
}

fn shown_status(lines: &[String], prefix: &str) -> String {
    lines
        .iter()
        .find_map(|l| line_starts(l, prefix).then(|| field(l, "status")).flatten())
        .unwrap_or_else(|| panic!("no {prefix}: {lines:#?}"))
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn share_mode_shares_the_frontends_streaming_encoder_without_a_second_one() {
    let h = Harness::new("");
    let _serve = default_serve(&h);
    let lines = share_session(&h, false, "0", &["--frontend-stream"]);
    assert!(
        plugin_observed(
            &lines,
            "main_stream_starting",
            json!({"encoder": "obs_x264", "multitrack": false})
        ),
        "{lines:#?}"
    );
    assert!(
        plugin_observed(&lines, "shared_video_encoder", json!({"id": "obs_x264"})),
        "{lines:#?}"
    );
    assert!(
        !plugin_observed(&lines, "own_video_encoder", json!({})),
        "{lines:#?}"
    );
    assert!(
        has_event(&lines, "HARNESS video_encoders when=tools_live count=1"),
        "only the main stream's video encoder exists: {lines:#?}"
    );
    assert!(
        stream_video(&h.mock.recorded()).len() >= 20,
        "the shared encoder reached Discord"
    );
    let session = serve_session(&h);
    assert_eq!(session["reason"]["reason"], "requested", "{session}");
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn when_the_main_stream_stops_share_mode_moves_to_its_own_encoder_and_stays_live() {
    let h = Harness::new("");
    let _serve = default_serve(&h);
    let (lines, observed) = share_session_env(
        &h,
        true,
        "0",
        &["--frontend-stream", "--frontend-stream-stop"],
        &[(
            "DSTREAMY_TEST_STOP_SIGNAL_DELAY_MS",
            std::path::Path::new("300"),
        )],
    );
    assert_moved_to_own_encoder(&h, &lines, &observed);
    assert!(
        has_event(&lines, "HARNESS video_encoders when=tools_live count=1"),
        "{lines:#?}"
    );
    assert!(
        has_event(&lines, "HARNESS video_encoders when=restarted count=2"),
        "{lines:#?}"
    );
    assert!(
        status_line(&lines, "restarted").starts_with("Live on"),
        "the Discord session stays live across the switch: {lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_lost_main_stream_moves_share_mode_to_its_own_encoder_too() {
    let h = Harness::new("");
    let _serve = default_serve(&h);
    let (lines, observed) = share_session_env(
        &h,
        true,
        "0",
        &["--frontend-stream", "--frontend-stream-lost"],
        &[],
    );
    assert_moved_to_own_encoder(&h, &lines, &observed);
    assert!(
        status_line(&lines, "restarted").starts_with("Live on"),
        "{lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_main_stream_that_stops_before_capture_begins_still_moves_share_mode_to_its_own_encoder() {
    let h = Harness::new("");
    let _serve = default_serve(&h);
    let gate = h.dir.path().join("capture-gate");
    let gate_arg = gate.to_string_lossy().into_owned();
    let (lines, observed) = share_session_env(
        &h,
        true,
        "0",
        &[
            "--frontend-stream",
            "--release-capture-gate",
            gate_arg.as_str(),
        ],
        &[("DSTREAMY_TEST_CAPTURE_GATE", gate.as_path())],
    );
    assert!(
        has_event(&lines, "HARNESS capture_gate released=1"),
        "{lines:#?}"
    );
    assert_moved_to_own_encoder(&h, &lines, &observed);
    assert!(
        status_line(&lines, "live").starts_with("Live on"),
        "{lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn without_fallback_share_mode_ends_the_stream_when_the_main_stream_stops() {
    let h = Harness::new("");
    let _serve = default_serve(&h);
    let lines = share_session(
        &h,
        false,
        "0",
        &[
            "--frontend-stream",
            "--frontend-stream-stop",
            "--tools-wait-end",
            "--tools-expect",
            "only while streaming",
        ],
    );
    let ended = shown_status(&lines, "HARNESS settings_status when=ended ");
    assert!(ended.contains("only while streaming"), "{ended}");
    assert!(
        !plugin_observed(&lines, "own_video_encoder", json!({})),
        "{lines:#?}"
    );
    let session = serve_session(&h);
    assert_eq!(session["state"], "stopped", "{session}");
    assert_eq!(session["reason"]["reason"], "requested", "{session}");
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn without_fallback_share_mode_refuses_when_no_main_stream_runs() {
    let h = Harness::new("");
    let _serve = default_serve(&h);
    let fix = ". Fix it, then press Start again";
    let lines = share_session(&h, false, "0", &["--tools-expect", fix]);
    let shown = shown_status(&lines, "HARNESS settings_status when=live ");
    assert!(
        shown.contains("only while streaming") && shown.ends_with(fix),
        "{shown}"
    );
    assert!(
        !plugin_observed(&lines, "own_video_encoder", json!({})),
        "{lines:#?}"
    );
    assert!(
        !plugin_observed(&lines, "tools_output_started", json!({})),
        "{lines:#?}"
    );
    let session = serve_session(&h);
    assert_eq!(session["state"], "stopped", "rolled back: {session}");
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn with_fallback_share_mode_uses_its_own_encoder_when_no_main_stream_runs() {
    let h = Harness::new("");
    let _serve = default_serve(&h);
    let lines = share_session(&h, true, "0", &[]);
    assert!(
        plugin_observed(
            &lines,
            "share_refused",
            json!({"inactive": true, "stream": false})
        ),
        "{lines:#?}"
    );
    assert!(
        plugin_observed(&lines, "own_video_encoder", json!({"id": "obs_x264"})),
        "{lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn share_mode_falls_back_from_a_multitrack_main_stream() {
    let h = Harness::new("");
    let _serve = default_serve(&h);
    let lines = share_session(
        &h,
        true,
        "0",
        &["--frontend-stream", "--frontend-stream-multitrack"],
    );
    assert!(
        plugin_observed(
            &lines,
            "main_stream_starting",
            json!({"encoder": "obs_x264", "multitrack": true})
        ),
        "{lines:#?}"
    );
    assert!(
        plugin_observed(
            &lines,
            "share_refused",
            json!({"multitrack": true, "stream": true})
        ),
        "{lines:#?}"
    );
    assert!(
        plugin_observed(&lines, "own_video_encoder", json!({"id": "obs_x264"})),
        "{lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn share_mode_falls_back_from_enhanced_broadcasting_with_one_video_track() {
    let h = Harness::new("");
    let _serve = default_serve(&h);
    let lines = share_session(
        &h,
        true,
        "0",
        &["--frontend-stream", "--frontend-stream-multitrack-single"],
    );
    assert!(
        plugin_observed(
            &lines,
            "main_stream_starting",
            json!({"encoder": "obs_x264", "multitrack": true})
        ),
        "{lines:#?}"
    );
    assert!(
        plugin_observed(
            &lines,
            "share_refused",
            json!({"multitrack": true, "stream": true})
        ),
        "{lines:#?}"
    );
    assert!(
        plugin_observed(&lines, "own_video_encoder", json!({"id": "obs_x264"})),
        "{lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn without_fallback_share_mode_refuses_a_multitrack_main_stream() {
    let h = Harness::new("");
    let _serve = default_serve(&h);
    let fix = ". Fix it, then press Start again";
    let lines = share_session(
        &h,
        false,
        "0",
        &[
            "--frontend-stream",
            "--frontend-stream-multitrack",
            "--tools-expect",
            fix,
        ],
    );
    let shown = shown_status(&lines, "HARNESS settings_status when=live ");
    assert!(
        shown.contains("can't share the encoders of a multitrack stream") && shown.ends_with(fix),
        "{shown}"
    );
    assert!(
        !plugin_observed(&lines, "own_video_encoder", json!({})),
        "{lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn share_mode_falls_back_from_a_main_stream_with_b_frames() {
    let h = Harness::new("");
    let _serve = default_serve(&h);
    let lines = share_session(&h, true, "2", &["--frontend-stream"]);
    assert!(
        plugin_observed(
            &lines,
            "share_refused",
            json!({"id": "obs_x264", "b_frames": true})
        ),
        "{lines:#?}"
    );
    assert!(
        plugin_observed(&lines, "own_video_encoder", json!({"id": "obs_x264"})),
        "{lines:#?}"
    );
    assert!(
        has_event(&lines, "HARNESS video_encoders when=tools_live count=2"),
        "{lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn without_fallback_share_mode_refuses_a_main_stream_with_b_frames() {
    let h = Harness::new("");
    let _serve = default_serve(&h);
    let fix = ". Fix it, then press Start again";
    let lines = share_session(
        &h,
        false,
        "2",
        &["--frontend-stream", "--tools-expect", fix],
    );
    let shown = shown_status(&lines, "HARNESS settings_status when=live ");
    assert!(
        shown.contains("D-Streamy needs B-frames off") && shown.ends_with(fix),
        "{shown}"
    );
    assert!(
        !plugin_observed(&lines, "own_video_encoder", json!({})),
        "{lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn the_own_encoder_restarts_on_the_same_output() {
    let (lines, _) = stream(json!({}), &["--restart-video-settings", "{}"], 60);
    let own = lines
        .iter()
        .filter(|line| {
            event(line).is_some_and(|row| {
                row["event"] == "plugin_own_video_encoder" && row["id"] == "obs_x264"
            })
        })
        .count();
    assert_eq!(own, 2, "{lines:#?}");
}

fn mixer_reports(lines: &[String]) -> Vec<(String, Option<u64>, u64)> {
    lines
        .iter()
        .filter_map(|line| {
            let row = event(line)?;
            (row["event"] == "audio_mixer").then(|| {
                Some((
                    row["when"].as_str()?.to_owned(),
                    row["idx"].as_u64(),
                    row["dstreamy_audio_encoders"].as_u64()?,
                ))
            })?
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
                .any(|l| line_starts(l, "HARNESS expected_error start: ")
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
    let ptrs: Vec<String> = lines
        .iter()
        .filter(|l| line_starts(l, "HARNESS audio_mixer when="))
        .filter_map(|l| field(l, "encoder"))
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

/// The fake frontend and `config` as the module config folder, then `extra`.
fn frontend_args(config: &std::path::Path, extra: &[&str]) -> Vec<String> {
    let mut args = vec![
        "--fake-frontend".to_owned(),
        fake_frontend(),
        "--module-config".to_owned(),
        config.to_string_lossy().into_owned(),
    ];
    args.extend(extra.iter().map(|&a| a.to_owned()));
    args
}

fn as_strs(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

/// [`report_ui`] with [`frontend_args`].
fn frontend_session(config: &std::path::Path, extra: &[&str]) -> Vec<String> {
    report_ui(&as_strs(&frontend_args(config, extra)))
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn without_a_frontend_the_plugin_loads_and_offers_no_tools_entry() {
    let lines = report_ui(&[]);
    assert!(
        plugin_observed(
            &lines,
            "frontend_unavailable",
            json!({"reason": "frontend"})
        ),
        "{lines:#?}"
    );
    assert!(
        !plugin_observed(&lines, "tools_entry_added", json!({})),
        "{lines:#?}"
    );
    assert!(
        lines.iter().any(|l| event_matches(
            l,
            "HARNESS settings_source registered=1 listed=0 flags=0x400",
            false
        )),
        "{lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn the_output_has_no_audio_track_property_but_keeps_track_1_as_its_default() {
    let lines = report_ui(&[]);
    let props: Vec<String> = lines
        .iter()
        .filter_map(|l| {
            line_starts(l, "HARNESS output_property ")
                .then(|| field(l, "name"))
                .flatten()
        })
        .collect();
    assert!(props.iter().any(|name| name == "mode"), "{lines:#?}");
    assert!(
        props
            .iter()
            .all(|p| !p.contains("audio_track") && !p.contains("label")),
        "{props:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| event_matches(l, "HARNESS output_setting audio_track=1", false)),
        "{lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn tools_d_streamy_opens_the_private_settings_source_through_the_frontend() {
    let lib = fake_frontend();
    let lines = report_ui(&["--fake-frontend", lib.as_str()]);
    assert!(
        plugin_observed(&lines, "tools_entry_added", json!({})),
        "{lines:#?}"
    );
    for want in [
        "HARNESS frontend tools_items=1 name=D-Streamy\u{2026}",
        "HARNESS frontend opened id=dstreamy_settings public=0 flags=0x400",
        "HARNESS frontend held expired=0",
        "HARNESS frontend closed expired=1",
        "HARNESS settings_source registered=1 listed=0 flags=0x400",
    ] {
        assert!(
            lines.iter().any(|l| event_matches(l, want, false)),
            "{want}: {lines:#?}"
        );
    }
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_frontend_without_a_properties_dialog_offers_no_tools_entry() {
    let lib = harness_lib("obs-harness-frontend-incomplete.so");
    let lines = report_ui(&["--fake-frontend", lib.as_str()]);
    assert!(
        plugin_observed(
            &lines,
            "frontend_unavailable",
            json!({"reason": "symbol", "symbol": "obs_frontend_open_source_properties"})
        ),
        "{lines:#?}"
    );
    assert!(
        !plugin_observed(
            &lines,
            "frontend_unavailable",
            json!({"reason": "frontend"})
        ),
        "{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| event_matches(l, "HARNESS frontend tools_items=0 name=", false)),
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
    lines
        .iter()
        .filter(|line| {
            event(line).is_some_and(|row| {
                row["event"]
                    .as_str()
                    .is_some_and(|name| name.starts_with("settings_"))
                    && row["when"] == when
            })
        })
        .map(String::as_str)
        .collect()
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn the_settings_dialog_offers_a_server_a_channel_tracks_1_to_6_and_start_while_idle() {
    let config = tempfile::tempdir().unwrap();
    let lines = frontend_session(config.path(), &[]);
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
        "HARNESS settings_property when=open name=start type=8 enabled=0 desc=Start".to_owned(),
        "HARNESS settings_property when=open name=stop type=8 enabled=0 desc=Stop".to_owned(),
        "HARNESS settings_source when=open audio_track=6".to_owned(),
    ]);
    let actual = settings_lines(&lines, "open");
    assert_eq!(actual.len(), want.len(), "{lines:#?}");
    assert!(
        actual
            .iter()
            .zip(&want)
            .all(|(line, want)| event_matches(line, want, false)),
        "{lines:#?}"
    );
    assert!(
        has_event(&lines, "HARNESS check_settings when=close differ=0"),
        "an unchanged close asks nothing: {lines:#?}"
    );
    assert!(
        !lines.iter().any(|l| l.to_lowercase().contains("label")),
        "{lines:#?}"
    );
}

/// Runs the harness's mixer session: sources Music and Voice on all six tracks, takeover, tick and untick Music.
fn mixer_session(saved: Option<Value>) -> (Vec<String>, Value) {
    let (lines, saved) = dialog_session_with("--mixer-session", saved);
    (lines, saved.expect("the mixer session saves its config"))
}

fn assert_lines(lines: &[String], want: &[String]) {
    for want in want {
        assert!(
            lines.iter().any(|l| event_matches(l, want, false)),
            "{want}: {lines:#?}"
        );
    }
}

/// The lines between a `kind` marker (`mixer_step` or `collection_step`) named `step` and the next one.
fn step_lines<'a>(lines: &'a [String], kind: &str, step: &str) -> &'a [String] {
    let marker = format!("HARNESS {kind} name={step}");
    let start = lines
        .iter()
        .position(|l| event_matches(l, &marker, false))
        .unwrap_or_else(|| panic!("no {marker}: {lines:#?}"))
        + 1;
    let end = lines[start..]
        .iter()
        .position(|l| line_starts(l, &format!("HARNESS {kind} ")))
        .map_or(lines.len(), |n| start + n);
    &lines[start..end]
}

/// The dialog never rebuilds inside its own callbacks, and an outside mixer change shows once libobs stored it.
fn assert_dialog_follows(lines: &[String]) {
    assert!(
        has_event(lines, "HARNESS mixer_sync_reloads count=0"),
        "no rebuild inside a click or a pick: {lines:#?}"
    );
    assert!(
        has_event(
            step_lines(lines, "mixer_step", "track5"),
            "HARNESS settings_property when=view name=take_over type=8 enabled=1 desc=Manage Track 5"
        ),
        "a picked track shows its own takeover: {lines:#?}"
    );
    let voice = step_lines(lines, "mixer_step", "external")
        .iter()
        .rfind(|l| line_starts(l, "HARNESS settings_checkbox when=view desc=Voice "));
    assert_eq!(
        voice.map(|line| event_matches(
            line,
            "HARNESS settings_checkbox when=view desc=Voice on=1",
            false
        )),
        Some(true),
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
        lines.iter().any(|l| line_starts(
            l,
            "HARNESS settings_clicked name=take_over enabled=1 refresh=0 "
        )),
        "{lines:#?}"
    );
    assert!(
        has_event(
            &lines,
            &format!(
                "HARNESS settings_property when=confirming name=take_over_effect type=4 enabled=1 desc={TAKE_OVER_EFFECT}"
            )
        ),
        "the takeover waits for its confirm: {lines:#?}"
    );
    assert!(
        !lines.iter().any(|l| line_starts(
            l,
            "HARNESS settings_property when=confirming name=take_over "
        )),
        "{lines:#?}"
    );
    assert!(
        !lines
            .iter()
            .any(|l| line_starts(l, "HARNESS settings_checkbox when=unmanaged")),
        "no checklist before the takeover: {lines:#?}"
    );
    assert_dialog_follows(&lines);
    assert!(
        has_event(&lines, "HARNESS mixers when=external name=Voice mixers=63"),
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
    dialog_session_with(mode, None)
}

fn dialog_session_with(mode: &str, saved: Option<Value>) -> (Vec<String>, Option<Value>) {
    let config = tempfile::tempdir().unwrap();
    if let Some(saved) = saved {
        write_module_config(config.path(), saved);
    }
    let lines = frontend_session(config.path(), &[mode]);
    let saved = module_config(config.path())
        .exists()
        .then(|| read_module_config(config.path()));
    (lines, saved)
}

fn start_button(lines: &[String], when: &str) -> Option<bool> {
    lines
        .iter()
        .rfind(|line| {
            line_starts(
                line,
                &format!("HARNESS settings_property when={when} name=start "),
            )
        })
        .and_then(|line| event(line)?["enabled"].as_u64().map(|enabled| enabled == 1))
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn the_managed_track_belongs_to_its_scene_collection_and_loads_never_touch_mixers() {
    let picks = json!({"guild_id": "g", "channel_id": "c", "audio_track": 6, "config_version": 2});
    let (lines, saved) = dialog_session_with("--collection-session", Some(picks));
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
        has_event(&lines, &guard),
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
            !lines.iter().any(|l| line_starts(
                l,
                &format!("HARNESS settings_checkbox when={when} ")
            ) || line_starts(
                l,
                &format!("HARNESS settings_property when={when} name=confirm_take_over ")
            )),
            "{when} is unmanaged with no confirm open: {lines:#?}"
        );
    }
    let saved = saved.expect("the picks stay saved");
    assert_eq!(
        saved.get("managed_track"),
        None,
        "a takeover is kept in the scene collection, not the module config: {saved}"
    );
    assert_eq!(
        (&saved["guild_id"], &saved["channel_id"]),
        (&json!("g"), &json!("c")),
        "{saved}"
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
        plugin_observed(&lines, "tracks_restored", json!({"sources": 3})),
        "Music, Voice and Late: {lines:#?}"
    );
    assert!(
        step_lines(&lines, "mixer_step", "managed")
            .iter()
            .any(|l| line_starts(l, "HARNESS picked track=4 ")),
        "Defaults has a pick to put back: {lines:#?}"
    );
    assert!(
        !lines
            .iter()
            .any(|l| line_starts(l, "HARNESS settings_checkbox when=cancelled ")),
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
    let mixers = mic_mixers.to_string();
    let mut args = vec!["--collection-adopt", "--adopt-mic-mixers", mixers.as_str()];
    args.extend_from_slice(extra);
    let lines = frontend_session(config.path(), &args);
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
    let (lines, saved) = adopt(
        json!({"audio_track": 3, "managed_track": 3, "guild_id": "g", "channel_id": "c"}),
        0x1f,
    );
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
        plugin_observed(
            &lines,
            "track_adoption_refused",
            json!({"track": 3, "microphones": ["Mic"]})
        ),
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
            .any(|l| line_starts(l, &prefix) && l.contains("Couldn't read D-Streamy's settings"))
    };
    assert!(unreadable("corrupt"), "{lines:#?}");
    assert!(
        !unreadable("repaired"),
        "the repaired file, still naming managed_track, was loaded again: {lines:#?}"
    );
    assert!(
        !lines
            .iter()
            .any(|l| line_starts(l, "HARNESS settings_checkbox when=b_adopt ")),
        "B has no mic on Track 6, yet the reloaded file's managed_track is not adopted again: {lines:#?}"
    );
    assert_eq!(
        saved["managed_track"], 6,
        "the read-only file was never written: {saved}"
    );
    assert!(
        plugin_observed(&lines, "config_error", json!({"kind": "write"})),
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
        lines.iter().any(|l| line_starts(l,
            "HARNESS settings_property when=adopted name=audio_note_0 type=4 enabled=1 desc=Track 1 is OBS's \
             streaming and recording track, and D-Streamy manages it for Discord"
        )),
        "{lines:#?}"
    );
    assert!(
        !lines
            .iter()
            .any(|l| line_starts(l, "HARNESS settings_property when=adopted name=release_")),
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
        lines.iter().any(|l| line_starts(l,
            "HARNESS settings_property when=track1_confirming name=take_over_effect type=4 enabled=1 \
             desc=Track 1 is OBS's streaming and recording track: managing it turns it off on every audio source"
        )),
        "{lines:#?}"
    );
    assert!(
        has_event(
            &lines,
            "HARNESS settings_property when=three name=release_6 type=8 enabled=1 desc=Stop managing Track 6"
        ),
        "a Cancel would manage the opened pick again, so letting it go is offered at once: {lines:#?}"
    );
    assert!(
        !lines
            .iter()
            .any(|l| line_starts(l, "HARNESS settings_property when=released name=release_")),
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
    for (step, closes) in [
        ("duplicate", "none"),
        ("rename", "none"),
        ("switch_discard", "discard"),
    ] {
        let lines = step_lines(&lines, "collection_step", step);
        let close = format!(
            "HARNESS close_dialog differ={} choice={closes}",
            u8::from(closes != "none")
        );
        assert!(has_event(lines, &close), "{step}: {lines:#?}");
    }
    assert!(
        plugin_observed(
            step_lines(&lines, "collection_step", "switch_discard"),
            "dialog_mixer_changes_kept",
            json!({})
        ),
        "the switch's changing event keeps the dialog's changes before the Discard: {lines:#?}"
    );
    let exit = step_lines(&lines, "collection_step", "exit");
    let at = |want: &str| exit.iter().position(|l| event_matches(l, want, false));
    assert!(
        at("HARNESS close_dialog differ=0 choice=none")
            > at("HARNESS check_settings when=exit_before differ=1"),
        "OBS saved the takeover at exit, so closing asks nothing: {exit:#?}"
    );
    assert!(
        plugin_observed(exit, "dialog_mixer_changes_kept", json!({})),
        "{exit:#?}"
    );
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
    let lines = frontend_session(config.path(), &[]);
    assert!(
        has_event(&lines, "HARNESS settings_source when=open audio_track=6"),
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
    let lines = frontend_session(
        config.path(),
        &["--settings-update", r#"{"audio_track": 5}"#],
    );
    for want in [
        "HARNESS settings_source when=open audio_track=3",
        "HARNESS settings_source when=updated audio_track=5",
        "HARNESS settings_source when=reopened audio_track=5",
    ] {
        assert!(
            lines.iter().any(|l| event_matches(l, want, false)),
            "{want}: {lines:#?}"
        );
    }
    assert!(
        lines
            .iter()
            .any(|l| line_starts(l, "HARNESS settings_modified refresh=1 ")
                && field(l, "status").is_some_and(|status| status.starts_with(NO_SERVICE))),
        "{lines:#?}"
    );
    assert_eq!(
        lines
            .iter()
            .filter(|l| event_matches(l, "HARNESS frontend closed expired=1", false))
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
    let lines = frontend_session(
        config.path(),
        &["--settings-update", r#"{"audio_track": 9}"#],
    );
    assert!(
        plugin_observed(&lines, "audio_track_refused", json!({"track": 9})),
        "{lines:#?}"
    );
    for want in [
        "HARNESS settings_source when=updated audio_track=3",
        "HARNESS settings_source when=reopened audio_track=3",
    ] {
        assert!(
            lines.iter().any(|l| event_matches(l, want, false)),
            "{want}: {lines:#?}"
        );
    }
    assert_eq!(read_module_config(config.path())["audio_track"], 3);
}

fn status_line(lines: &[String], when: &str) -> String {
    lines
        .iter()
        .find_map(|line| {
            line_starts(
                line,
                &format!("HARNESS settings_property when={when} name=status "),
            )
            .then(|| field(line, "desc"))
            .flatten()
        })
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
    let lines = frontend_session(
        config.path(),
        &["--settings-update", r#"{"audio_track": 5}"#],
    );
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
            lines.iter().any(|l| event_matches(
                l,
                &format!("HARNESS settings_info when={when} name=status kind=2"),
                false
            )),
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
        lines.iter().any(|l| event_matches(
            l,
            "HARNESS settings_source when=reopened audio_track=5",
            false
        )),
        "{lines:#?}"
    );
    assert!(
        plugin_observed(&lines, "config_error", json!({"kind":"corrupt"})),
        "{lines:#?}"
    );
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
        let folder_arg = folder.to_string_lossy().into_owned();
        let lines = frontend_session(
            config.path(),
            &[
                "--replace-before-update",
                folder_arg.as_str(),
                "--settings-update",
                r#"{"audio_track": 4}"#,
            ],
        );
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
                lines.iter().any(|l| event_matches(
                    l,
                    &format!("HARNESS settings_info when={when} name=status kind=2"),
                    false
                )),
                "{when}: {lines:#?}"
            );
        }
        assert!(
            lines.iter().any(|l| event_matches(
                l,
                "HARNESS settings_source when=reopened audio_track=4",
                false
            )),
            "{saved:?}: {lines:#?}"
        );
        assert!(
            plugin_observed(&lines, "config_error", json!({"kind":"write"})),
            "{lines:#?}"
        );
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
        lines.iter().any(|l| event_matches(
            l,
            "HARNESS settings_info when=updated name=status kind=2",
            false
        )),
        "{lines:#?}"
    );
    assert!(
        plugin_observed(
            &lines,
            "config_error",
            json!({"kind":"no_folder","track":2})
        ),
        "{lines:#?}"
    );
    assert!(
        lines.iter().any(|l| event_matches(
            l,
            "HARNESS settings_source when=reopened audio_track=2",
            false
        )),
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
    let lines = frontend_session(
        config.path(),
        &["--settings-update", r#"{"audio_track": 5}"#],
    );
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
            lines.iter().any(|l| event_matches(
                l,
                &format!("HARNESS settings_info when={when} name=status kind=2"),
                false
            )),
            "{when}: {lines:#?}"
        );
    }
    assert!(
        lines.iter().any(|l| event_matches(
            l,
            "HARNESS settings_source when=reopened audio_track=5",
            false
        )),
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
    default_serve_env(h, &[])
}

fn default_serve_env(h: &Harness, vars: &[(&str, &str)]) -> Proc {
    let endpoint = serve_endpoint(h);
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin("dstreamy"));
    cmd.envs(vars.iter().copied())
        .env_remove("DSTREAMY_TOKEN")
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
    let pick =
        json!({"guild_id": MOCK_GUILD_ID, "channel_id": MOCK_VOICE_CHANNEL_ID, "audio_track": 3})
            .to_string();
    let args = frontend_args(
        config.path(),
        &[
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
    let run = HarnessRun::start(&h, json!({}), &as_strs(&args));
    let (status, lines) = run.finish();
    passed(status, &lines);

    assert!(
        lines.iter().any(|l| event_matches(
            l,
            "HARNESS settings_item when=open name=guild_id value=0 text=Mock Guild",
            false
        )),
        "the dialog lists the service's servers: {lines:#?}"
    );
    assert!(
        lines.iter().any(|l| event_matches(
            l,
            "HARNESS settings_item when=updated name=channel_id value=0 text=General",
            false
        )),
        "{lines:#?}"
    );
    asked_first(&lines, "start", 0, "Go live in General on Mock Guild");
    assert!(
        lines.iter().any(|l| event_matches(l, "HARNESS settings_clicked name=confirm_stream enabled=1 refresh=1 status=Live on Track 3", false)
            || line_starts(l,
                "HARNESS settings_clicked name=confirm_stream enabled=1 refresh=1 status=Starting on Track 3"
            )),
        "the confirm starts: {lines:#?}"
    );
    asked_first(&lines, "stop", 1, "Stop the Discord stream?");
    assert!(
        lines.iter().any(|l| line_starts(
            l,
            "HARNESS settings_clicked name=confirm_stream enabled=1 refresh=1 status=Stopping"
        ) || event_matches(
            l,
            "HARNESS settings_clicked name=confirm_stream enabled=1 refresh=1 status=Idle",
            false
        )),
        "the second confirm stops: {lines:#?}"
    );
    assert!(
        lines.iter().any(|l| event_matches(
            l,
            "HARNESS settings_status when=live status=Live on Track 3",
            false
        )),
        "{lines:#?}"
    );
    assert!(
        lines.iter().any(|l| event_matches(
            l,
            "HARNESS output_audio_track when=tools_live active=1 track=3",
            false
        )),
        "{lines:#?}"
    );
    assert_eq!(
        status_line(&lines, "live_updated"),
        "Live on Track 3 \u{2014} Discord hears Track 5 from the next Go Live"
    );
    assert!(
        lines.iter().any(|l| event_matches(
            l,
            "HARNESS output_audio_track when=after_settings_update active=1 track=3",
            false
        )),
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
        assert!(
            lines.iter().any(|l| event_matches(l, want, false)),
            "{want}: {lines:#?}"
        );
    }
    assert!(
        plugin_observed(&lines, "tools_output_started", json!({"track": 3})),
        "{lines:#?}"
    );

    let recorded = h.mock.recorded();
    assert!(
        stream_video(&recorded).len() >= 30,
        "the dialog's session reached Discord"
    );
    let mut control = Client::connect(&serve_endpoint(&h));
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
        serve_endpoint(&h).display(),
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
        lines.iter().any(|line| {
            event_matches(line, &format!("HARNESS settings_clicked name=confirm_stream enabled=1 refresh=1 status={not_running}"), false)
                || event_matches(line, "HARNESS settings_clicked name=confirm_stream enabled=1 refresh=1 status=Starting on Track 2…", false)
        }),
        "{lines:#?}"
    );
    assert!(
        has_event(
            &lines,
            &format!("HARNESS settings_status when=live status={not_running}")
        ),
        "{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| event_matches(l, "HARNESS tools output=0 active=0", false)),
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
            assert!(has_event(&lines, &want), "{want}: {lines:#?}");
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
        lines.iter().any(|l| line_starts(
            l,
            "HARNESS settings_clicked name=cancel_stream enabled=1 refresh=1 "
        )),
        "{lines:#?}"
    );
    assert!(
        !lines
            .iter()
            .any(|l| line_starts(l, "HARNESS settings_clicked name=confirm_stream ")),
        "{lines:#?}"
    );
    assert!(
        has_event(&lines, "HARNESS tools output=0 active=0"),
        "{lines:#?}"
    );
    for when in ["cancelled", "reopened"] {
        for want in [
            format!("HARNESS settings_property when={when} name=start type=8 enabled=1 desc=Start"),
            format!("HARNESS settings_property when={when} name=stop type=8 enabled=0 desc=Stop"),
        ] {
            assert!(has_event(&lines, &want), "{want}: {lines:#?}");
        }
        assert!(
            !lines.iter().any(|l| line_starts(
                l,
                &format!("HARNESS settings_property when={when} name=stream_question ")
            )),
            "the question is gone: {lines:#?}"
        );
    }
    assert!(
        !plugin_observed(&lines, "tools_output_started", json!({})),
        "{lines:#?}"
    );
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
        has_event(
            &lines,
            &format!(
                "HARNESS settings_clicked name=confirm_stream enabled=1 refresh=1 status={gone}"
            )
        ),
        "the click's own rebuild shows the error: {lines:#?}"
    );
    assert!(
        has_event(
            &lines,
            "HARNESS settings_click_sync_refreshes name=confirm_stream count=0"
        ),
        "the click never asked OBS to rebuild the dialog it is running in: {lines:#?}"
    );
    assert!(has_event(&lines, "HARNESS tools output=0 active=0"));
    for want in [
        "HARNESS settings_property when=after_stop name=start type=8 enabled=1 desc=Start",
        "HARNESS settings_property when=after_stop name=stop type=8 enabled=0 desc=Stop",
        "HARNESS settings_info when=after_stop name=status kind=2",
    ] {
        assert!(has_event(&lines, want), "{want}: {lines:#?}");
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
    let mut args = vec!["--tools-session"];
    args.extend(extra);
    HarnessRun::start_with_env(h, json!({}), &as_strs(&frontend_args(config, &args)), vars)
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
    let mut control = Client::connect(&serve_endpoint(h));
    control.send(&json!({"id": "h", "cmd": "hello", "role": "control", "protocol": 2}));
    control.reply("h")["data"]["status"]["session"].clone()
}

fn picked_config(track: u8) -> tempfile::TempDir {
    picked_config_with(track, json!({}))
}

/// A module config with the mock server and voice channel picked, `track` and the fields of `extra`.
fn picked_config_with(track: u8, extra: Value) -> tempfile::TempDir {
    let mut saved = json!({"guild_id": MOCK_GUILD_ID, "channel_id": MOCK_VOICE_CHANNEL_ID, "audio_track": track,
        "config_version": 2});
    if let (Some(saved), Value::Object(extra)) = (saved.as_object_mut(), extra) {
        saved.extend(extra);
    }
    let config = tempfile::tempdir().unwrap();
    write_module_config(config.path(), saved);
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
    let (status, lines) = tools_run(
        &h,
        config.path(),
        &[
            "--settings-update",
            &first,
            "--then-update",
            &other,
            "--tools-expect",
            "Idle",
            "--seconds",
            "10",
            "--until-video",
            "1",
        ],
    )
    .finish();
    passed(status, &lines);
    assert!(
        has_event(
            &lines,
            &format!(
                "HARNESS pick when=updated guild_id={MOCK_GUILD_ID} channel_id={MOCK_VOICE_CHANNEL_ID}"
            )
        ),
        "{lines:#?}"
    );
    assert!(
        has_event(
            &lines,
            &format!("HARNESS pick when=updated_again guild_id={OTHER_GUILD_ID} channel_id=")
        ),
        "the view's old channel is cleared with the server: {lines:#?}"
    );
    let channels: Vec<String> = settings_lines(&lines, "updated_again")
        .into_iter()
        .filter(|line| {
            line_starts(
                line,
                "HARNESS settings_item when=updated_again name=channel_id ",
            )
        })
        .filter_map(|line| field(line, "text"))
        .collect();
    assert_eq!(channels, ["Pick a voice channel", "Lobby"], "{lines:#?}");
    assert_lines(
        &lines,
        &[
            "HARNESS settings_property when=updated_again name=start type=8 enabled=0 desc=Start",
            "HARNESS settings_clicked name=start enabled=0 refresh=0 status=Idle",
            "HARNESS settings_asked name=start output_active=0 question=",
            "HARNESS settings_status when=live status=Idle",
            "HARNESS tools output=0 active=0",
        ]
        .map(str::to_owned),
    );
    assert!(
        !lines
            .iter()
            .any(|l| line_starts(l, "HARNESS settings_clicked name=confirm_stream ")),
        "Start never uses the old server's channel: {lines:#?}"
    );
    assert!(
        has_event(
            &lines,
            &format!("HARNESS pick when=reopened guild_id={OTHER_GUILD_ID} channel_id=")
        ),
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
        lines.iter().any(|l| line_starts(
            l,
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
            line_starts(l, "HARNESS settings_clicked name=confirm_stream ")
        });
        assert_eq!(
            field(&clicked, "status"),
            Some("Starting on Track 4…".to_owned()),
            "the click's wait ran out first"
        );
        asked_first(&run.lines(), "start", 0, "Go live in ");
        if !answered {
            serve.kill();
        }
        wait_for_line(&run, "refresh baseline", |line| {
            event(line).is_some_and(|row| {
                row["event"] == "refresh_baseline_ready" && row["when"] == "live"
            })
        });
        std::fs::write(&gate, "").unwrap();
        let (status, lines) = run.finish();
        passed(status, &lines);
        for name in ["start", "stop", "confirm_stream"] {
            assert!(
                !lines.iter().any(|l| line_starts(
                    l,
                    &format!("HARNESS settings_click_sync_refreshes name={name} count=")
                ) && field(l, "count").as_deref() != Some("0")),
                "no refresh inside a click: {lines:#?}"
            );
        }
        let want = if answered {
            "Live on Track 4"
        } else {
            late_error
        };
        assert!(
            lines.iter().any(|l| line_starts(
                l,
                &format!("HARNESS settings_refreshed when=live status={want}")
            )),
            "{answered}: the dialog rebuilt itself on the control thread's change: {lines:#?}"
        );
        assert!(
            lines.iter().any(|l| line_starts(
                l,
                &format!("HARNESS settings_status when=live status={want}")
            )),
            "{lines:#?}"
        );
        if answered {
            assert!(
                has_event(&lines, "HARNESS settings_status when=stopped status=Idle"),
                "{lines:#?}"
            );
            assert!(plugin_observed(
                &lines,
                "tools_output_started",
                json!({"track": 4})
            ));
        } else {
            assert!(has_event(&lines, "HARNESS tools output=0 active=0"));
            assert!(
                has_event(
                    &lines,
                    "HARNESS settings_property when=reopened name=start type=8 enabled=1 desc=Start"
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
        .find_map(|l| {
            line_starts(l, "HARNESS settings_status when=live ")
                .then(|| field(l, "status"))
                .flatten()
        })
        .unwrap_or_else(|| panic!("{lines:#?}"));
    assert!(
        shown.contains("no_such_encoder") && shown.ends_with(fix),
        "{shown}"
    );
    assert!(
        has_event(&lines, "HARNESS tools output=1 active=0"),
        "{lines:#?}"
    );
    assert!(
        !plugin_observed(&lines, "tools_output_started", json!({})),
        "{lines:#?}"
    );
    let session = serve_session(&h);
    assert_eq!(session["state"], "stopped", "rolled back: {session}");
    assert_eq!(session["reason"]["reason"], "requested", "{session}");
    let saved = read_module_config(config.path());
    assert_eq!(saved["owned"], Value::Null, "{saved}");
    assert!(
        has_event(
            &lines,
            "HARNESS settings_property when=reopened name=start type=8 enabled=1 desc=Start"
        ),
        "{lines:#?}"
    );
}

/// Runs a held Start that `mode` makes unsafe while it shows "Starting…", then lets the service answer.
fn mic_while_starting(
    h: &Harness,
    config: &std::path::Path,
    mode: &str,
    expect: &str,
    vars: &[(&str, &std::path::Path)],
) -> HarnessRun {
    let gate = h.dir.path().join("start-gate");
    let mut vars = vars.to_vec();
    vars.push(("DSTREAMY_TEST_START_GATE", &gate));
    let run = tools_run_env(
        h,
        config,
        &[
            "--tools-mic-while-starting",
            mode,
            "--tools-expect",
            expect,
            "--seconds",
            "60",
            "--until-video",
            "1",
        ],
        &vars,
    );
    let changed = wait_for_line(&run, "the change while starting", |l| {
        event(l).is_some_and(|row| row["event"] == "tools_mic_while_starting")
    });
    assert!(
        field(&changed, "status").is_some_and(|s| s.starts_with("Starting on Track")),
        "{changed}"
    );
    std::fs::write(&gate, "").unwrap();
    run
}

const HEARS_MIC: &str = "so Discord would hear it";

/// The dialog shows the mic guard while the mic is there, and the refusal once it is gone.
fn assert_held_back(lines: &[String], track: u8) {
    let after = status_line(lines, "after_stop");
    assert!(
        after.starts_with(&format!(
            "Go Live held back: Mic is on Track {track}, {HEARS_MIC}."
        )),
        "{after}"
    );
    let shown = lines
        .iter()
        .find_map(|l| {
            line_starts(l, "HARNESS settings_status when=live ")
                .then(|| field(l, "status"))
                .flatten()
        })
        .unwrap_or_else(|| panic!("{lines:#?}"));
    assert!(
        shown.starts_with(&format!("Mic is on Track {track}, {HEARS_MIC}")),
        "{shown}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_mic_reaching_the_track_while_starting_stops_the_session_and_never_streams() {
    for (mode, track) in [("track", 1), ("route", 6)] {
        let h = Harness::new("");
        let _serve = default_serve(&h);
        let config = picked_config(6);
        let (status, lines) = mic_while_starting(&h, config.path(), mode, HEARS_MIC, &[]).finish();
        passed(status, &lines);
        assert_held_back(&lines, track);
        assert!(
            plugin_observed(
                &lines,
                "start_held_back_by_mic",
                json!({"track": track, "ended": true})
            ),
            "{mode}: {lines:#?}"
        );
        assert!(
            !plugin_observed(&lines, "tools_output_started", json!({})),
            "{mode}: {lines:#?}"
        );
        let session = serve_session(&h);
        assert_eq!(session["state"], "stopped", "{mode}: {session}");
        assert_eq!(
            session["reason"]["reason"], "requested",
            "{mode}: {session}"
        );
        assert_eq!(read_module_config(config.path())["owned"], Value::Null);
        let reopened = status_line(&lines, "reopened");
        assert!(reopened.ends_with("Then press Start again"), "{reopened}");
        assert!(
            has_event(
                &lines,
                "HARNESS settings_property when=reopened name=start type=8 enabled=1 desc=Start"
            ),
            "{mode}: the mic is gone, so Start works again: {lines:#?}"
        );
    }
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_mic_refusal_whose_cleanup_fails_keeps_the_ownership_and_says_the_session_is_live() {
    let h = Harness::new("");
    let serve = default_serve(&h);
    let config = picked_config(6);
    let refusal = h.dir.path().join("refusal-gate");
    let run = mic_while_starting(
        &h,
        config.path(),
        "route",
        HEARS_MIC,
        &[("DSTREAMY_TEST_MIC_REFUSAL_GATE", &refusal)],
    );
    wait_for_line(&run, "the held refusal", |l| {
        event(l).is_some_and(|row| row["event"] == "plugin_mic_refusal_gated")
    });
    let (status, lines) = frozen(&serve, || {
        std::fs::write(&refusal, "").unwrap();
        run.finish()
    });
    passed(status, &lines);
    assert_held_back(&lines, 6);
    let after = status_line(&lines, "after_stop");
    assert!(
        after.contains("The Discord session it started is still live")
            && after.ends_with("press Start to use it again"),
        "{after}"
    );
    assert!(
        plugin_observed(
            &lines,
            "start_held_back_by_mic",
            json!({"track": 6, "ended": false})
        ),
        "{lines:#?}"
    );
    assert!(
        !plugin_observed(&lines, "tools_output_started", json!({})),
        "{lines:#?}"
    );
    assert!(
        read_module_config(config.path())["owned"].is_object(),
        "a failed cleanup keeps the ownership for the next Start or Stop"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_mic_refusal_leaves_an_attached_external_session_running() {
    let h = Harness::new("");
    let _serve = default_serve(&h);
    let mut external = Client::connect(&serve_endpoint(&h));
    external.send(&json!({"id": "h", "cmd": "hello", "role": "control", "protocol": 2}));
    external.reply("h");
    external.send(&json!({
        "id": "start", "cmd": "start", "guild_id": MOCK_GUILD_ID,
        "channel_id": MOCK_VOICE_CHANNEL_ID, "target": "external",
        "resolution": "1280x720", "fps": OBS_FPS, "bitrate_kbps": 2500,
    }));
    let reply = external.reply("start");
    assert_eq!(reply["type"], "result", "{reply}");
    let config = picked_config(6);
    let (status, lines) = mic_while_starting(&h, config.path(), "route", HEARS_MIC, &[]).finish();
    passed(status, &lines);
    assert_held_back(&lines, 6);
    assert!(
        plugin_observed(
            &lines,
            "start_held_back_by_mic",
            json!({"track": 6, "ended": true})
        ),
        "{lines:#?}"
    );
    assert!(
        !plugin_observed(&lines, "tools_output_started", json!({})),
        "{lines:#?}"
    );
    let session = serve_session(&h);
    assert_ne!(session["state"], "stopped", "{session}");
    assert!(
        !session.is_null() && session["state"] != "idle",
        "{session}"
    );
    assert_eq!(read_module_config(config.path())["owned"], Value::Null);
}

/// Runs a Tools session until Live, calls `end` while the dialog waits for the end, and returns the shown status.
fn ended_from_outside(
    h: &Harness,
    config: &std::path::Path,
    extra: &[&str],
    end: impl FnOnce(),
) -> String {
    let mut args = vec![
        "--tools-wait-end",
        "--tools-expect",
        "Start",
        "--seconds",
        "60",
        "--until-video",
        "30",
    ];
    args.extend(extra);
    let run = tools_run(h, config, &args);
    wait_for_line(&run, "the live wait", |l| {
        event_matches(l, "HARNESS tools waiting for the end", false)
    });
    end();
    let (status, lines) = run.finish();
    passed(status, &lines);
    let ended = lines
        .iter()
        .find_map(|l| {
            line_starts(l, "HARNESS settings_refreshed when=ended ")
                .then(|| field(l, "status"))
                .flatten()
        })
        .unwrap_or_else(|| panic!("the dialog was never asked to rebuild: {lines:#?}"));
    assert!(
        has_event(&lines, "HARNESS tools stopped active=0"),
        "{lines:#?}"
    );
    assert!(
        has_event(
            &lines,
            "HARNESS settings_property when=after_stop name=start type=8 enabled=1 desc=Start"
        ),
        "{lines:#?}"
    );
    ended
}

fn control_command(h: &Harness, cmd: &str) -> Value {
    let mut control = Client::connect(&serve_endpoint(h));
    control.send(&json!({"id": "h", "cmd": "hello", "role": "control", "protocol": 2}));
    control.reply("h");
    control.send(&json!({"id": "c", "cmd": cmd}));
    control.reply("c")
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_session_the_service_ends_shows_in_the_open_dialog() {
    let h = Harness::new("");
    let _serve = default_serve(&h);
    let config = picked_config(1);
    let ended = ended_from_outside(&h, config.path(), &[], || {
        let stopped = control_command(&h, "stop");
        assert_eq!(stopped["type"], "result", "{stopped}");
    });
    assert!(
        ended.starts_with("The Discord session ended: a D-Streamy client stopped it")
            && ended.contains("another OBS")
            && !ended.contains("dstreamy stop"),
        "{ended}"
    );
    assert_eq!(read_module_config(config.path())["owned"], Value::Null);
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_session_another_device_takes_over_shows_why_in_the_open_dialog() {
    let h = Harness::new("");
    let _serve = default_serve(&h);
    let config = picked_config(1);
    let ended = ended_from_outside(&h, config.path(), &[], || {
        assert!(
            h.mock
                .voice_state_from_other_session(MOCK_GUILD_ID, Some(MOCK_VOICE_CHANNEL_ID))
        );
    });
    assert_eq!(
        ended,
        "The Discord session ended: another device of this account joined voice. \
         Leave voice on the other device, then press Start"
    );
    assert_eq!(read_module_config(config.path())["owned"], Value::Null);
    assert_eq!(serve_session(&h)["reason"]["reason"], "voice_taken_over");
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_service_shutdown_while_live_shows_in_the_open_dialog() {
    let h = Harness::new("");
    let _serve = default_serve(&h);
    let config = picked_config(1);
    let ended = ended_from_outside(&h, config.path(), &[], || {
        let mut control = Client::connect(&serve_endpoint(&h));
        control.send(&json!({"id": "h", "cmd": "hello", "role": "control", "protocol": 2}));
        control.reply("h");
        control.send(&json!({"id": "c", "cmd": "shutdown"}));
    });
    assert!(
        ended.starts_with("The D-Streamy service stopped, so the Discord session ended"),
        "{ended}"
    );
    assert_eq!(read_module_config(config.path())["owned"], Value::Null);
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_slow_service_shutdown_while_live_still_clears_the_panel() {
    let h = Harness::new("");
    let teardown = h.dir.path().join("teardown");
    let teardown_arg = teardown.to_string_lossy().into_owned();
    let _serve = default_serve_env(&h, &[("DSTREAMY_TEST_SHUTDOWN_GATE", &teardown_arg)]);
    let config = picked_config(1);
    let ended = ended_from_outside(&h, config.path(), &[], || {
        let mut control = Client::connect(&serve_endpoint(&h));
        control.send(&json!({"id": "h", "cmd": "hello", "role": "control", "protocol": 2}));
        control.reply("h");
        control.send(&json!({"id": "c", "cmd": "shutdown"}));
    });
    assert_eq!(
        ended,
        "The D-Streamy service stopped, so the Discord session ended. Press Start to go live again"
    );
    assert_eq!(read_module_config(config.path())["owned"], Value::Null);
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_stop_pressed_as_the_service_shuts_down_settles_without_asking_it() {
    let h = Harness::new("");
    let teardown = h.dir.path().join("teardown");
    let teardown_arg = teardown.to_string_lossy().into_owned();
    let _serve = default_serve_env(&h, &[("DSTREAMY_TEST_SHUTDOWN_GATE", &teardown_arg)]);
    let config = picked_config(1);
    let go = h.dir.path().join("stop-now");
    let gate = h.dir.path().join("stop-read");
    std::fs::write(&go, "").unwrap();
    let go_arg = go.to_string_lossy().into_owned();
    let run = tools_run_env(
        &h,
        config.path(),
        &[
            "--tools-stop-after",
            &go_arg,
            "--seconds",
            "60",
            "--until-video",
            "30",
        ],
        &[("DSTREAMY_TEST_STOP_READ_GATE", gate.as_path())],
    );
    wait_for_line(&run, "the gated stop", |l| {
        event(l).is_some_and(|row| row["event"] == "plugin_stop_read_gated")
    });
    let mut control = Client::connect(&serve_endpoint(&h));
    control.send(&json!({"id": "h", "cmd": "hello", "role": "control", "protocol": 2}));
    control.reply("h");
    control.send(&json!({"id": "c", "cmd": "shutdown"}));
    wait_for_line(&run, "the service's stop signal", |l| {
        event(l).is_some_and(|row| row["event"] == "plugin_stop_signal" && row["service"] == true)
    });
    std::fs::write(&gate, "").unwrap();
    let (status, lines) = run.finish();
    passed(status, &lines);
    assert_eq!(
        shown_status(&lines, "HARNESS settings_status when=stopped "),
        "Idle"
    );
    assert_eq!(read_module_config(config.path())["owned"], Value::Null);
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn an_output_stopped_outside_d_streamy_ends_the_discord_session() {
    let h = Harness::new("");
    let _serve = default_serve(&h);
    let config = picked_config(1);
    let ended = ended_from_outside(&h, config.path(), &["--tools-external-stop"], || {});
    assert!(
        ended.contains("stopped outside D-Streamy")
            && ended.ends_with("Press Start to go live again"),
        "{ended}"
    );
    let session = serve_session(&h);
    assert_eq!(session["state"], "stopped", "{session}");
    assert_eq!(session["reason"]["reason"], "requested", "{session}");
    assert_eq!(read_module_config(config.path())["owned"], Value::Null);
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_stop_pressed_as_the_service_ends_the_session_finishes_at_once() {
    let h = Harness::new("");
    let _serve = default_serve(&h);
    let config = picked_config(1);
    let go = h.dir.path().join("stop-now");
    let gate = h.dir.path().join("stop-read");
    std::fs::write(&go, "").unwrap();
    let go_arg = go.to_string_lossy().into_owned();
    let run = tools_run_env(
        &h,
        config.path(),
        &[
            "--tools-stop-after",
            &go_arg,
            "--seconds",
            "60",
            "--until-video",
            "30",
        ],
        &[("DSTREAMY_TEST_STOP_READ_GATE", gate.as_path())],
    );
    wait_for_line(&run, "the gated stop", |l| {
        event(l).is_some_and(|row| row["event"] == "plugin_stop_read_gated")
    });
    let stopped = control_command(&h, "stop");
    assert_eq!(stopped["type"], "result", "{stopped}");
    wait_for_line(&run, "the service's stop signal", |l| {
        event(l).is_some_and(|row| row["event"] == "plugin_stop_signal" && row["service"] == true)
    });
    let released = Instant::now();
    std::fs::write(&gate, "").unwrap();
    let (status, lines) = run.finish();
    passed(status, &lines);
    assert_eq!(
        shown_status(&lines, "HARNESS settings_status when=stopped "),
        "Idle"
    );
    assert!(
        released.elapsed() < Duration::from_secs(8),
        "Stop waited for a stop signal that had already come: {lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn a_start_cancelled_while_starting_leaves_nothing_running() {
    let h = Harness::new("");
    let _serve = default_serve(&h);
    let config = picked_config(1);
    let gate = h.dir.path().join("start-gate");
    let run = tools_run_gated(
        &h,
        config.path(),
        &[
            "--tools-cancel-while-starting",
            "--tools-expect",
            "Idle",
            "--seconds",
            "30",
            "--until-video",
            "1",
        ],
        &gate,
    );
    wait_for_line(&run, "the cancel click", |l| {
        event(l).is_some_and(|row| row["event"] == "tools_cancel_while_starting")
    });
    std::fs::write(&gate, "").unwrap();
    let (status, lines) = run.finish();
    passed(status, &lines);
    assert!(
        plugin_observed(&lines, "start_cancelled", json!({})),
        "{lines:#?}"
    );
    assert!(
        !plugin_observed(&lines, "tools_output_started", json!({})),
        "{lines:#?}"
    );
    assert_eq!(
        shown_status(&lines, "HARNESS settings_status when=live "),
        "Idle"
    );
    let session = serve_session(&h);
    assert!(
        session.is_null() || session["state"] == "idle" || session["state"] == "stopped",
        "{session}"
    );
    assert_eq!(read_module_config(config.path())["owned"], Value::Null);
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
        line_starts(l, "HARNESS tools video=")
    });
    let (status, lines) = frozen(&serve, || {
        std::fs::write(&go, "").unwrap();
        run.finish()
    });
    passed(status, &lines);
    let stopped = lines
        .iter()
        .find_map(|l| {
            line_starts(l, "HARNESS settings_status when=stopped ")
                .then(|| field(l, "status"))
                .flatten()
        })
        .unwrap_or_else(|| panic!("{lines:#?}"));
    assert_eq!(
        stopped,
        format!(
            "Stop didn't finish: The D-Streamy service didn't answer hello within 10 s. {hint}"
        )
    );
    assert!(
        lines.iter().any(|l| line_starts(
            l,
            "HARNESS settings_refreshed when=stopped status=Stop didn't finish"
        )),
        "{lines:#?}"
    );
    assert!(
        has_event(&lines, "HARNESS tools stopped active=0"),
        "{lines:#?}"
    );
    assert!(
        has_event(
            &lines,
            "HARNESS settings_info when=after_stop name=status kind=2"
        ),
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
        .filter_map(|line| event(line))
        .find_map(|row| {
            (row["event"] == "frontend_exit")
                .then(|| row["ms"].as_u64())
                .flatten()
        })
        .unwrap_or_else(|| panic!("no frontend exit: {lines:#?}"));
    exit + event_number(lines, "obs_shutdown", "ms")
}

/// The plugin shut down once, on the frontend's exit event, and left no output for `obs_shutdown` to stop after audio.
fn assert_closed_on_frontend_exit(lines: &[String]) {
    let position = |name: &str| {
        lines
            .iter()
            .position(|line| event(line).is_some_and(|row| row["event"] == name))
    };
    let exiting = position("plugin_frontend_exit");
    let ended = position("plugin_control_closed");
    let shutdown = lines.iter().position(|line| {
        line_starts(
            line,
            "HARNESS obs_shutdown begins dstreamy_output=0 active=0",
        )
    });
    assert!(
        matches!((exiting, ended, shutdown), (Some(a), Some(b), Some(c)) if a < b && b < c),
        "the control thread ended on frontend exit, before obs_shutdown: {lines:#?}"
    );
    assert_eq!(
        lines
            .iter()
            .filter_map(|line| event(line))
            .filter(|row| row["event"] == "plugin_control_closing")
            .count(),
        1,
        "unload after exit shuts down nothing again: {lines:#?}"
    );
    assert!(
        has_line(lines, "HARNESS frontend_exit callbacks=1 "),
        "the plugin watched the frontend's exit: {lines:#?}"
    );
}

#[test]
#[ignore = "needs obs-harness and the built plugin; obs-ci runs it"]
fn an_error_ended_output_retains_stop_and_ownership_when_service_cleanup_fails() {
    let h = Harness::new("");
    let serve = default_serve(&h);
    let config = picked_config(1);
    let fail = h.dir.path().join("fail-output");
    let run = tools_run_env(
        &h,
        config.path(),
        &[
            "--tools-wait-end",
            "--tools-expect",
            "press Stop to end it",
            "--seconds",
            "60",
            "--until-video",
            "30",
        ],
        &[("DSTREAMY_TEST_OUTPUT_ERROR_GATE", &fail)],
    );
    wait_for_line(&run, "live video", |line| {
        line_starts(line, "HARNESS tools video=")
    });
    let (status, lines) = frozen(&serve, || {
        std::fs::write(&fail, "").unwrap();
        run.finish()
    });
    passed(status, &lines);
    assert!(has_event(&lines, "HARNESS tools stopped active=0"));
    assert!(
        has_event(
            &lines,
            "HARNESS settings_property when=after_stop name=stop type=8 enabled=1 desc=Stop"
        ),
        "{lines:#?}"
    );
    let stopped = status_line(&lines, "after_stop");
    assert!(
        stopped.contains("test output failure") && stopped.contains("press Stop to end it"),
        "{stopped}"
    );
    assert!(
        read_module_config(config.path())["owned"].is_object(),
        "cleanup failure keeps ownership for retry"
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
            event(l).is_some_and(|row| row["event"] == "plugin_control_closing")
        });
        if answered {
            std::fs::write(&gate, "").unwrap();
        }
        let (status, lines) = run.finish();
        passed(status, &lines);
        asked_first(&lines, "start", 0, "Go live in ");
        assert!(
            has_event(
                &lines,
                "HARNESS tools exiting status=Starting on Track 1\u{2026}"
            ),
            "{lines:#?}"
        );
        assert!(
            !plugin_observed(&lines, "tools_output_started", json!({})),
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
                plugin_observed(&lines, "closing_unstreamed_session", json!({"ended": true})),
                "{lines:#?}"
            );
            let session = serve_session(&h);
            assert_eq!(session["state"], "stopped", "{session}");
            assert_eq!(session["reason"]["reason"], "requested", "{session}");
            assert_eq!(read_module_config(config.path())["owned"], Value::Null);
        } else {
            assert!(
                plugin_observed(&lines, "closing_start_abandoned", json!({})),
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
            line_starts(l, "HARNESS tools video=")
        });
        let finish = || {
            std::fs::write(&go, "").unwrap();
            run.finish()
        };
        let (status, lines) = if answered {
            finish()
        } else {
            frozen(&serve, finish)
        };
        passed(status, &lines);
        assert!(
            lines
                .iter()
                .any(|l| line_starts(l, "HARNESS tools exiting status=Live on Track 1")),
            "{answered}: {lines:#?}"
        );
        assert!(
            plugin_observed(&lines, "closing_live_session", json!({})),
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
        let stopped = position(&|l| line_starts(l, "HARNESS tools output_stopped code=0"));
        let shutdown = position(&|l| line_starts(l, "HARNESS obs_shutdown begins"));
        assert!(
            matches!((stopped, shutdown), (Some(a), Some(b)) if a < b),
            "{answered}: the output stopped before obs_shutdown stopped audio: {lines:#?}"
        );
        let begins = position(&|line| {
            event(line).is_some_and(|row| row["event"] == "plugin_closing_live_session")
        });
        let output_stopped = position(&|l| line_starts(l, "HARNESS tools output_stopped code=0"));
        let ended = position(&|line| {
            event(line).is_some_and(|row| row["event"] == "plugin_closing_live_session_finished")
        });
        assert!(
            matches!((begins, output_stopped, ended), (Some(a), Some(b), Some(c)) if a < b && b < c),
            "{answered}: output stop must happen between the plugin's start and end lines: {lines:#?}"
        );
        let owned = read_module_config(config.path())["owned"].clone();
        if answered {
            assert!(
                plugin_observed(
                    &lines,
                    "closing_live_session_finished",
                    json!({"output_stopped": true, "session_ended": true})
                ),
                "{lines:#?}"
            );
            let session = serve_session(&h);
            assert_eq!(session["state"], "stopped", "{session}");
            assert_eq!(session["reason"]["reason"], "requested", "{session}");
            assert_eq!(owned, Value::Null);
        } else {
            assert!(
                plugin_observed(
                    &lines,
                    "closing_live_session_finished",
                    json!({"session_ended": false})
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
    let args = frontend_args(
        config.path(),
        &["--settings-update-while-active", r#"{"audio_track": 4}"#],
    );
    let (lines, _) = stream(json!({"audio_track": 1}), &as_strs(&args), 60);
    assert!(
        lines.iter().any(|l| event_matches(
            l,
            "HARNESS output_audio_track when=after_settings_update active=1 track=1",
            false
        )),
        "{lines:#?}"
    );
    let encoder = |when: &str| {
        lines
            .iter()
            .find_map(|l| {
                line_starts(l, &format!("HARNESS audio_mixer when={when} "))
                    .then(|| field(l, "encoder"))
                    .flatten()
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
        lines.iter().any(|l| event_matches(
            l,
            "HARNESS output_audio_track when=after_active_update active=1 track=4",
            false
        )),
        "{lines:#?}"
    );
    let ptrs: Vec<(String, String)> = lines
        .iter()
        .filter(|line| line_starts(line, "HARNESS audio_mixer "))
        .filter_map(|line| Some((field(line, "when")?, field(line, "encoder")?)))
        .collect();
    assert_eq!(
        ptrs.iter().map(|p| p.0.as_str()).collect::<Vec<_>>(),
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
        plugin_observed(
            &lines,
            "own_video_encoder",
            json!({"gpu_mix": true, "format": "NV12"})
        ),
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
        plugin_observed(&lines, "share_refused", json!({"color_format": true})),
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
        plugin_observed(
            &lines,
            "own_video_encoder",
            json!({"width": 1920, "height": 1080})
        ) && plugin_observed(
            &lines,
            "own_video_encoder",
            json!({"gpu_mix": true, "format": "NV12"})
        ),
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
            has_event(&lines, "HARNESS audio_source mixers=0x2"),
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
    let changes = json!({"guild_id": OTHER_GUILD_ID, "audio_track": 5}).to_string();
    let lines = frontend_session(
        config.path(),
        &["--settings-update", &changes, "--settings-cancel"],
    );
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
        assert!(has_event(&lines, &want), "{want}: {lines:#?}");
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
    let lines = frontend_session(config.path(), &["--settings-update", r#"{"guild_id": ""}"#]);
    for want in [
        format!(
            "HARNESS pick when=open guild_id={MOCK_GUILD_ID} channel_id={MOCK_VOICE_CHANNEL_ID}"
        ),
        "HARNESS pick when=updated guild_id= channel_id=".to_owned(),
        "HARNESS pick when=reopened guild_id= channel_id=".to_owned(),
        "HARNESS settings_source when=reopened audio_track=3".to_owned(),
    ] {
        assert!(has_event(&lines, &want), "{want}: {lines:#?}");
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
        .find_map(|l| {
            line_starts(l, "HARNESS settings_status when=live ")
                .then(|| field(l, "status"))
                .flatten()
        })
        .unwrap_or_else(|| panic!("{lines:#?}"));
    assert_eq!(
        shown,
        format!(
            "D-Streamy's control step failed (panic caught in control: test panic after the output started), {cleaned}"
        )
    );
    assert!(
        has_event(&lines, "HARNESS tools output=1 active=0"),
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
        assert!(has_event(&lines, &want), "{want}: {lines:#?}");
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
        let mut text = newest
            .and_then(|e| std::fs::read_to_string(e.path()).ok())
            .unwrap_or_default();
        text.push_str(
            &std::fs::read_to_string(h.dir.path().join("obs-stdout.log")).unwrap_or_default(),
        );
        text
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
        l.lines()
            .filter_map(event)
            .any(|row| row["event"] == "plugin_tools_entry_added")
            && l.contains("==== Startup complete")
    });
    std::fs::write(&open, "").unwrap();
    let built = |text: &str, servers: u64| {
        text.lines().position(|line| {
            event(line).is_some_and(|row| {
                row["event"] == "plugin_dialog_built" && row["servers"] == servers
            })
        })
    };
    wait("the dialog", &|text| built(text, 0).is_some());
    std::fs::write(&gate, "").unwrap();
    let text = wait("the rebuilt dialog", &|text| built(text, 1).is_some());
    assert!(
        built(&text, 0) < built(&text, 1),
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
    assert!(
        text.lines()
            .filter_map(event)
            .any(|row| row["event"] == "plugin_unloaded"),
        "{text}"
    );
    assert!(!text.contains("[obs-dstreamy] panic caught"), "{text}");
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
