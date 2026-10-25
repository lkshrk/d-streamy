//! `serve` + mock Discord + `mock-producer`, all real processes over the real socket.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use dstreamy_cli::engine::ANNOUNCE_INTERVAL;
use dstreamy_core::ConfigPaths;
use dstreamy_media::{OPUS_SILENCE_FRAME, QUEUE_MAX_BYTES};
use dstreamy_mock_discord::opcodes::voice::OP_VIDEO;
use dstreamy_mock_discord::{
    MOCK_GUILD_ID, MOCK_TOKEN, MOCK_VOICE_CHANNEL_ID, MockDiscord, Recorded, RecordedAudioFrame,
    RecordedVideoFrame, VoiceKind,
};
use dstreamy_test_producer::sync::OpusMonoDecoder;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::runtime::Runtime;

const T: Duration = Duration::from_secs(20);
const FPS: u32 = 30;

/// Built once per test binary, next to `dstreamy` with the same profile and target: `cargo test` builds only this package's binaries.
fn mock_producer() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let dstreamy = assert_cmd::cargo::cargo_bin("dstreamy");
        let profile_dir = dstreamy.parent().expect("dstreamy has a directory");
        let profile = match profile_dir.file_name().and_then(|n| n.to_str()) {
            Some("debug") => "dev",
            Some(name) => name,
            None => panic!("no profile directory in {}", dstreamy.display()),
        };
        let mut target_dir = profile_dir
            .parent()
            .expect("a target directory")
            .to_path_buf();
        let mut args: Vec<std::ffi::OsString> = Vec::new();
        // Cargo tags the target directory root; one level more is a `--target` triple.
        if !target_dir.join("CACHEDIR.TAG").exists() {
            let triple = target_dir.file_name().unwrap().to_owned();
            target_dir = target_dir.parent().unwrap().to_path_buf();
            args.extend(["--target".into(), triple]);
        }
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = Command::new(cargo)
            .args([
                "build",
                "--quiet",
                "--locked",
                "-p",
                "dstreamy-test-producer",
            ])
            .args([
                "--bin",
                "mock-producer",
                "--profile",
                profile,
                "--target-dir",
            ])
            .arg(&target_dir)
            .args(args)
            .status()
            .expect("running cargo build for mock-producer");
        assert!(status.success(), "building mock-producer failed");
        let bin = dstreamy.with_file_name(format!("mock-producer{}", std::env::consts::EXE_SUFFIX));
        assert!(bin.exists(), "{} missing", bin.display());
        bin
    })
}

struct Harness {
    rt: Runtime,
    mock: MockDiscord,
    dir: TempDir,
    run: TempDir,
}

impl Harness {
    fn new(config: &str) -> Self {
        mock_producer();
        let rt = Runtime::new().unwrap();
        let mock = rt.block_on(MockDiscord::start()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        ConfigPaths::in_dir(dir.path())
            .save_token(MOCK_TOKEN)
            .unwrap();
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        Self {
            rt,
            mock,
            dir,
            run: tempfile::Builder::new()
                .prefix("ds")
                .tempdir_in("/tmp")
                .unwrap(),
        }
    }

    fn endpoint(&self) -> PathBuf {
        self.run.path().join("d").join("s.sock")
    }

    fn serve(&self) -> Proc {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin("dstreamy"));
        cmd.env_remove("DSTREAMY_TOKEN")
            .env_remove("DSTREAMY_CONFIG_DIR")
            .arg("--config-dir")
            .arg(self.dir.path())
            .arg("--gateway-url")
            .arg(self.mock.gateway_url())
            .args(["-v", "serve", "--skip-platform", "--endpoint"])
            .arg(self.endpoint());
        let p = Proc::spawn(&mut cmd);
        let deadline = Instant::now() + T;
        while UnixStream::connect(self.endpoint()).is_err() {
            assert!(Instant::now() < deadline, "serve never listened");
            std::thread::sleep(Duration::from_millis(20));
        }
        p
    }

    fn producer(&self, args: &[&str]) -> Proc {
        let mut cmd = Command::new(mock_producer());
        cmd.arg("--endpoint")
            .arg(self.endpoint())
            .arg("--pattern")
            .args(args);
        Proc::spawn(&mut cmd)
    }

    fn control(&self) -> Client {
        let mut c = Client::connect(&self.endpoint());
        c.send(&json!({"id": "h", "cmd": "hello", "role": "control", "protocol": 2}));
        assert_eq!(c.reply("h")["type"], "result");
        c.send(&json!({"id": "sub", "cmd": "subscribe"}));
        c.reply("sub");
        c
    }

    /// A control client that started an external session and saw it stream.
    fn external_session(&self) -> Client {
        let mut c = self.control();
        c.send(&json!({
            "id": "start", "cmd": "start", "guild_id": MOCK_GUILD_ID,
            "channel_id": MOCK_VOICE_CHANNEL_ID, "target": "external",
            "resolution": "320x180", "fps": FPS, "bitrate_kbps": 1000,
        }));
        let reply = c.reply("start");
        assert_eq!(reply["type"], "result", "{reply}");
        c.until("streaming", |m| m["event"]["state"]["state"] == "streaming");
        c
    }

    fn wait<V>(&self, what: &str, f: impl FnMut(&Recorded) -> Option<V>) -> V {
        self.rt
            .block_on(self.mock.wait_for(T, f))
            .unwrap_or_else(|| panic!("mock never saw {what}"))
    }

    fn stream_media(&self, video: usize, audio: usize) -> Recorded {
        self.wait(&format!("{video} video and {audio} audio frames"), |r| {
            (stream_video(r).len() >= video && producer_audio(r).len() >= audio).then(|| r.clone())
        })
    }
}

fn stream_video(r: &Recorded) -> Vec<&RecordedVideoFrame> {
    r.video
        .iter()
        .filter(|v| v.kind == VoiceKind::Stream)
        .collect()
}

fn producer_audio(r: &Recorded) -> Vec<&RecordedAudioFrame> {
    r.audio
        .iter()
        .filter(|a| a.kind == VoiceKind::Stream && a.data != OPUS_SILENCE_FRAME)
        .collect()
}

fn stream_audio(r: &Recorded) -> Vec<&RecordedAudioFrame> {
    r.audio
        .iter()
        .filter(|a| a.kind == VoiceKind::Stream)
        .collect()
}

/// A process whose stdout lines (JSON from `mock-producer`) and stderr are collected.
struct Proc {
    child: Child,
    lines: Arc<Mutex<Vec<Value>>>,
    err: Arc<Mutex<String>>,
    stderr: Option<std::thread::JoinHandle<String>>,
}

impl Proc {
    fn spawn(cmd: &mut Command) -> Self {
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let stdout = child.stdout.take().unwrap();
        let sink = lines.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str(&line) {
                    sink.lock().unwrap().push(v);
                }
            }
        });
        let stderr = child.stderr.take().unwrap();
        let err = Arc::new(Mutex::new(String::new()));
        let copy = err.clone();
        let stderr = std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut text = copy.lock().unwrap();
                text.push_str(&line);
                text.push('\n');
            }
            copy.lock().unwrap().clone()
        });
        Self {
            child,
            lines,
            err,
            stderr: Some(stderr),
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn lines(&self) -> Vec<Value> {
        self.lines.lock().unwrap().clone()
    }

    fn wait_line(&self, what: &str, f: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + T;
        loop {
            if let Some(v) = self.lines().into_iter().find(&f) {
                return v;
            }
            if Instant::now() >= deadline {
                panic!(
                    "{what} never came: {:#?}\nstderr: {}",
                    self.lines(),
                    self.err.lock().unwrap()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn attached(&self) -> u64 {
        let hello = self.wait_line("hello reply", |v| v.get("hello").is_some());
        hello["hello"]["attached"]
            .as_u64()
            .unwrap_or_else(|| panic!("not attached: {hello}"))
    }

    fn exited(&mut self) -> bool {
        self.child.try_wait().unwrap().is_some()
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Exit status and stderr, killing it after `T`.
    fn finish(mut self) -> (std::process::ExitStatus, String) {
        let deadline = Instant::now() + T;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                panic!("process did not exit; stdout {:#?}", self.lines());
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        (status, self.stderr.take().unwrap().join().unwrap())
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.kill();
    }
}

struct Client {
    stream: UnixStream,
    lines: mpsc::Receiver<Value>,
    seen: Vec<Value>,
}

impl Client {
    fn connect(path: &Path) -> Self {
        let stream = UnixStream::connect(path).unwrap();
        let read = stream.try_clone().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(read).lines().map_while(Result::ok) {
                if tx.send(serde_json::from_str(&line).unwrap()).is_err() {
                    return;
                }
            }
        });
        Self {
            stream,
            lines,
            seen: Vec::new(),
        }
    }

    fn send(&mut self, line: &Value) {
        self.stream
            .write_all(format!("{line}\n").as_bytes())
            .unwrap();
    }

    fn until(&mut self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        if let Some(i) = self.seen.iter().position(&pred) {
            return self.seen.remove(i);
        }
        let deadline = Instant::now() + T;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(m) if pred(&m) => return m,
                Ok(m) => self.seen.push(m),
                Err(_) => panic!("no {what}; seen {:#?}", self.seen),
            }
        }
    }

    fn reply(&mut self, id: &str) -> Value {
        self.until(&format!("reply {id}"), |m| {
            m["type"] != "event" && m["id"] == id
        })
    }

    fn source(&mut self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        self.until(what, |m| {
            m["event"]["event"] == "source" && pred(&m["event"])
        })
    }

    fn status(&mut self) -> Value {
        self.send(&json!({"id": "st", "cmd": "status"}));
        self.reply("st")["data"].clone()
    }

    fn drain(&mut self) {
        while let Ok(m) = self.lines.try_recv() {
            self.seen.push(m);
        }
    }
}

fn increasing(name: &str, ts: &[u32]) {
    for (i, w) in ts.windows(2).enumerate() {
        assert!(
            (w[1].wrapping_sub(w[0]) as i32) > 0,
            "{name} RTP timestamp {i}: {} then {}",
            w[0],
            w[1]
        );
    }
}

fn is_keyframe(au: &[u8]) -> bool {
    dstreamy_media::annexb::h264_is_keyframe(au)
}

/// The index of the first video frame after the largest RTP gap.
fn after_gap(video: &[&RecordedVideoFrame]) -> usize {
    let (i, _) = video
        .windows(2)
        .map(|w| w[1].rtp_timestamp.wrapping_sub(w[0].rtp_timestamp))
        .enumerate()
        .max_by_key(|&(_, g)| g)
        .unwrap();
    i + 1
}

fn decodes(video: &[&RecordedVideoFrame], audio: &[&RecordedAudioFrame]) {
    let start = video
        .iter()
        .position(|v| is_keyframe(&v.data))
        .expect("no keyframe");
    let mut h264 = openh264::decoder::Decoder::new().unwrap();
    let mut pictures = 0;
    for (i, au) in video.iter().enumerate().skip(start) {
        if h264
            .decode(&au.data)
            .unwrap_or_else(|e| panic!("video {i} does not decode: {e}"))
            .is_some()
        {
            pictures += 1;
        }
    }
    assert!(pictures > 0, "no picture decoded");
    let mut opus = OpusMonoDecoder::new().unwrap();
    for (i, a) in audio.iter().enumerate() {
        let pcm = opus
            .decode(&a.data)
            .unwrap_or_else(|e| panic!("audio {i} does not decode: {e}"));
        assert_eq!(pcm.len(), 960, "audio {i}: 20 ms");
    }
}

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
    let _c = h.external_session();
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
        p.attached();
        // The previous op 12 went out after the previous spawn, so this one is rate limited past it.
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
    let _c = h.external_session();
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

/// Start indices of the runs separated by RTP jumps of more than `gap_us`.
fn runs(timestamps: &[u32], hz: i64, gap_us: i64) -> Vec<usize> {
    let mut starts = vec![0];
    for (i, w) in timestamps.windows(2).enumerate() {
        if i64::from(w[1].wrapping_sub(w[0])) * 1_000_000 / hz > gap_us {
            starts.push(i + 1);
        }
    }
    starts
}

#[test]
fn audio_and_video_stay_aligned_across_a_producer_restart() {
    let h = Harness::new("[serve]\nidle = \"none\"\n");
    let _s = h.serve();
    let mut c = h.external_session();
    let mut recorded = None;
    for n in 0..3 {
        let mut p = h.producer(&[]);
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
    }
    let recorded = recorded.unwrap();
    let video: Vec<&RecordedVideoFrame> = stream_video(&recorded);
    let audio: Vec<u32> = producer_audio(&recorded)
        .iter()
        .map(|a| a.rtp_timestamp)
        .collect();
    let video_ts: Vec<u32> = video.iter().map(|v| v.rtp_timestamp).collect();
    let (vr, ar) = (
        runs(&video_ts, 90_000, 500_000),
        runs(&audio, 48_000, 500_000),
    );
    assert_eq!(
        (vr.len(), ar.len()),
        (3, 3),
        "attachments: video {vr:?}, audio {ar:?}"
    );
    // Both tracks start at pts 0 and RTP clocks at random values (RFC 3550 §5.1), so only offset changes count.
    let rtp_us =
        |t: u32, base: u32, hz: i64| i64::from(t.wrapping_sub(base) as i32) * 1_000_000 / hz;
    let offsets: Vec<i64> = vr
        .iter()
        .zip(&ar)
        .map(|(&v, &a)| {
            rtp_us(video_ts[v], video_ts[0], 90_000) - rtp_us(audio[a], audio[0], 48_000)
        })
        .collect();
    for &v in &vr[1..] {
        assert!(
            is_keyframe(&video[v].data),
            "video {v} after a restart is not an IDR"
        );
    }
    let frame_us = 1_000_000 / i64::from(FPS);
    let first_unreliable =
        !is_keyframe(&video[0].data) || recorded.dropped_frames.iter().any(|d| d.contains("video"));
    let from = usize::from(first_unreliable);
    for pair in offsets[from..].windows(2) {
        assert!(
            (pair[1] - pair[0]).abs() <= frame_us,
            "A/V offset moved by {} µs across a restart (offsets {offsets:?} µs, one frame is {frame_us} µs)",
            pair[1] - pair[0]
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
    let keyframe_requests = p
        .lines()
        .iter()
        .filter(|v| v["event"]["type"] == "keyframe_request")
        .count();
    let lines = p.lines.clone();
    let (status, stderr) = p.finish();
    assert!(status.success(), "{stderr}");
    let sent_bytes = lines
        .lock()
        .unwrap()
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
        flood.sent_bytes / 1024 > bound_kib + bound_kib / 2,
        "the flood sent only {} KiB, too little to tell buffering from the bound",
        flood.sent_bytes / 1024
    );
    assert!(
        flood.peak_rss_kib <= baseline.peak_rss_kib + bound_kib,
        "serve peak RSS {} KiB is more than {bound_kib} KiB over the {} KiB baseline",
        flood.peak_rss_kib,
        baseline.peak_rss_kib
    );
}
