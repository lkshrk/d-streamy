//! `serve` + mock Discord + `mock-producer` harness shared by the producer integration tests.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, mpsc};
use std::time::{Duration, Instant};

use dstreamy_core::ConfigPaths;
use dstreamy_media::OPUS_SILENCE_FRAME;
use dstreamy_mock_discord::{
    MOCK_GUILD_ID, MOCK_TOKEN, MOCK_VOICE_CHANNEL_ID, MockDiscord, Recorded, RecordedAudioFrame,
    RecordedVideoFrame, VoiceKind,
};
use dstreamy_test_producer::sync::OpusMonoDecoder;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::runtime::Runtime;

pub const T: Duration = Duration::from_secs(20);
pub const FPS: u32 = 30;

/// `DSTREAMY_MOCK_PRODUCER` if set, else built once per test binary next to `dstreamy` (same profile and target).
pub fn mock_producer() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        if let Some(bin) = std::env::var_os("DSTREAMY_MOCK_PRODUCER") {
            let bin = PathBuf::from(bin);
            assert!(
                bin.is_absolute() && bin.is_file(),
                "DSTREAMY_MOCK_PRODUCER={} is not an absolute path to a file",
                bin.display()
            );
            return bin;
        }
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

pub struct Harness {
    pub rt: Runtime,
    pub mock: MockDiscord,
    pub dir: TempDir,
    pub run: TempDir,
    serve_log: Mutex<Option<Arc<Mutex<String>>>>,
}

impl Harness {
    pub fn new(config: &str) -> Self {
        Self::with_mock(config, dstreamy_mock_discord::MockConfig::default())
    }

    pub fn with_mock(config: &str, discord: dstreamy_mock_discord::MockConfig) -> Self {
        mock_producer();
        let rt = Runtime::new().unwrap();
        let mock = rt.block_on(MockDiscord::start_with(discord)).unwrap();
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
            serve_log: Mutex::new(None),
        }
    }

    pub fn endpoint(&self) -> PathBuf {
        self.run.path().join("d").join("s.sock")
    }

    pub fn serve(&self) -> Proc {
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
        *self.serve_log.lock().unwrap() = Some(p.err.clone());
        let deadline = Instant::now() + T;
        while UnixStream::connect(self.endpoint()).is_err() {
            assert!(Instant::now() < deadline, "serve never listened");
            std::thread::sleep(Duration::from_millis(20));
        }
        p
    }

    pub fn producer(&self, args: &[&str]) -> Proc {
        let mut cmd = Command::new(mock_producer());
        cmd.arg("--endpoint")
            .arg(self.endpoint())
            .arg("--pattern")
            .args(args);
        Proc::spawn(&mut cmd)
    }

    pub fn replay(&self, trace: &Path, args: &[&str]) -> Proc {
        let mut cmd = Command::new(mock_producer());
        cmd.arg("--endpoint")
            .arg(self.endpoint())
            .arg("--replay")
            .arg(trace)
            .args(args);
        Proc::spawn(&mut cmd)
    }

    pub fn control(&self) -> Client {
        let mut c = Client::connect(&self.endpoint());
        c.send(&json!({"id": "h", "cmd": "hello", "role": "control", "protocol": 2}));
        assert_eq!(c.reply("h")["type"], "result");
        c.send(&json!({"id": "sub", "cmd": "subscribe"}));
        c.reply("sub");
        c
    }

    /// A control client that started an external session and saw it stream.
    pub fn external_session(&self) -> Client {
        let mut c = self.control();
        c.send(&json!({
            "id": "start", "cmd": "start", "guild_id": MOCK_GUILD_ID,
            "channel_id": MOCK_VOICE_CHANNEL_ID, "target": "external",
            "resolution": "320x180", "fps": FPS, "bitrate_kbps": 1000,
        }));
        let reply = c.reply("start");
        assert_eq!(reply["type"], "result", "{reply}");
        let streaming = c.try_until(|m| m["event"]["state"]["state"] == "streaming");
        assert!(
            streaming.is_some(),
            "no streaming; seen {:#?}\nserve stderr:\n{}",
            c.seen,
            self.serve_log()
        );
        c
    }

    pub fn serve_log(&self) -> String {
        self.serve_log
            .lock()
            .unwrap()
            .as_ref()
            .map(|log| log.lock().unwrap().clone())
            .unwrap_or_default()
    }

    pub fn wait<V>(&self, what: &str, f: impl FnMut(&Recorded) -> Option<V>) -> V {
        self.rt
            .block_on(self.mock.wait_for(T, f))
            .unwrap_or_else(|| panic!("mock never saw {what}"))
    }

    pub fn stream_media(&self, video: usize, audio: usize) -> Recorded {
        self.wait(&format!("{video} video and {audio} audio frames"), |r| {
            (stream_video(r).len() >= video && producer_audio(r).len() >= audio).then(|| r.clone())
        })
    }
}

pub fn stream_video(r: &Recorded) -> Vec<&RecordedVideoFrame> {
    r.video
        .iter()
        .filter(|v| v.kind == VoiceKind::Stream)
        .collect()
}

pub fn producer_audio(r: &Recorded) -> Vec<&RecordedAudioFrame> {
    r.audio
        .iter()
        .filter(|a| a.kind == VoiceKind::Stream && a.data != OPUS_SILENCE_FRAME)
        .collect()
}

pub fn stream_audio(r: &Recorded) -> Vec<&RecordedAudioFrame> {
    r.audio
        .iter()
        .filter(|a| a.kind == VoiceKind::Stream)
        .collect()
}

fn read_pipe(mut pipe: impl Read + AsRawFd, done: &AtomicBool, mut received: impl FnMut(&[u8])) {
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    assert!(flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == 0);
    let mut bytes = [0; 4096];
    let mut deadline = None;
    loop {
        if done.load(Ordering::Acquire) {
            let until = deadline.get_or_insert_with(|| Instant::now() + Duration::from_secs(1));
            if Instant::now() >= *until {
                break;
            }
        }
        match pipe.read(&mut bytes) {
            Ok(0) => break,
            Ok(n) => received(&bytes[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Descendants can retain a reaped child's pipe writers.
                if done.load(Ordering::Acquire) {
                    break;
                }
                std::thread::park_timeout(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
}

/// A process whose stdout lines (JSON from `mock-producer`, anything else kept as text) and stderr are collected.
pub struct Proc {
    child: Mutex<Child>,
    pub lines: Arc<Mutex<Vec<Value>>>,
    pub text: Arc<Mutex<Vec<String>>>,
    pub err: Arc<Mutex<String>>,
    stderr: Mutex<Option<std::thread::JoinHandle<String>>>,
    readers_done: Arc<AtomicBool>,
    stdout: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Proc {
    pub fn spawn(cmd: &mut Command) -> Self {
        Self::spawn_with_stderr_gate(cmd, None)
    }

    pub fn spawn_with_stderr_gate(cmd: &mut Command, gate: Option<mpsc::Receiver<()>>) -> Self {
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let text = Arc::new(Mutex::new(Vec::new()));
        let readers_done = Arc::new(AtomicBool::new(false));
        let stdout = child.stdout.take().unwrap();
        let (sink, other, done) = (lines.clone(), text.clone(), readers_done.clone());
        let stdout = std::thread::spawn(move || {
            let mut pending = Vec::new();
            let record = |line: &[u8]| match serde_json::from_slice(line) {
                Ok(v) => sink.lock().unwrap().push(v),
                Err(_) => other
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(line).into_owned()),
            };
            read_pipe(stdout, &done, |bytes| {
                pending.extend_from_slice(bytes);
                while let Some(end) = pending.iter().position(|&b| b == b'\n') {
                    let line = &pending[..end];
                    record(line.strip_suffix(b"\r").unwrap_or(line));
                    pending.drain(..=end);
                }
            });
            if !pending.is_empty() {
                record(&pending);
            }
        });
        let stderr = child.stderr.take().unwrap();
        let err = Arc::new(Mutex::new(String::new()));
        let (copy, done) = (err.clone(), readers_done.clone());
        let stderr = std::thread::spawn(move || {
            if let Some(gate) = gate {
                let _ = gate.recv();
            }
            let mut captured = Vec::new();
            read_pipe(stderr, &done, |bytes| {
                captured.extend_from_slice(bytes);
                *copy.lock().unwrap() = String::from_utf8_lossy(&captured).into_owned();
            });
            copy.lock().unwrap().clone()
        });
        Self {
            child: Mutex::new(child),
            lines,
            text,
            err,
            stderr: Mutex::new(Some(stderr)),
            readers_done,
            stdout: Mutex::new(Some(stdout)),
        }
    }

    fn child(&self) -> MutexGuard<'_, Child> {
        self.child.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn pid(&self) -> u32 {
        self.child().id()
    }

    pub fn lines(&self) -> Vec<Value> {
        self.lines.lock().unwrap().clone()
    }

    /// Everything the process wrote, for failure messages.
    pub fn output_report(&self) -> String {
        format!(
            "stdout (JSON): {:#?}\nstdout (other): {:#?}\nstderr: {}",
            self.lines(),
            self.text.lock().unwrap(),
            self.err.lock().unwrap()
        )
    }

    /// The first line matching `f`, failing as soon as the process exits without one.
    pub fn wait_line(&self, what: &str, f: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + T;
        loop {
            if let Some(v) = self.lines().into_iter().find(&f) {
                return v;
            }
            let exited = self.child().try_wait().unwrap();
            if let Some(status) = exited {
                self.join_readers();
                if let Some(v) = self.lines().into_iter().find(&f) {
                    return v;
                }
                panic!(
                    "{what} never came: the process {status}\n{}",
                    self.output_report()
                );
            }
            if Instant::now() >= deadline {
                panic!("{what} never came within {T:?}\n{}", self.output_report());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn attached(&self) -> u64 {
        let hello = self.wait_line("hello reply", |v| v.get("hello").is_some());
        hello["hello"]["attached"]
            .as_u64()
            .unwrap_or_else(|| panic!("not attached: {hello}"))
    }

    pub fn exited(&self) -> bool {
        self.child().try_wait().unwrap().is_some()
    }

    /// `exited`, saying how, for failure messages.
    pub fn exit_report(&self) -> String {
        match self.child().try_wait() {
            Ok(Some(status)) => format!("exited with {status}"),
            Ok(None) => "still running".to_owned(),
            Err(e) => format!("unknown: {e}"),
        }
    }

    pub fn kill(&mut self) {
        let mut child = self.child();
        let _ = child.kill();
        let _ = child.wait();
        drop(child);
        self.join_readers();
    }

    fn join_readers(&self) {
        self.readers_done.store(true, Ordering::Release);
        let stdout = self
            .stdout
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(stdout) = stdout {
            stdout.join().expect("stdout reader panicked");
        }
        let stderr = self
            .stderr
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(stderr) = stderr {
            stderr.join().expect("stderr reader panicked");
        }
    }

    /// Exit status and stderr, killing it after `T`.
    pub fn finish(self) -> (std::process::ExitStatus, String) {
        let deadline = Instant::now() + T;
        let status = loop {
            if let Some(status) = self.child().try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = self.child().kill();
                panic!(
                    "process did not exit within {T:?}\n{}",
                    self.output_report()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        self.join_readers();
        (status, self.err.lock().unwrap().clone())
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.kill();
    }
}

pub struct Client {
    pub stream: UnixStream,
    pub lines: mpsc::Receiver<Value>,
    pub seen: Vec<Value>,
    closed: bool,
}

impl Client {
    pub fn connect(path: &Path) -> Self {
        let stream = UnixStream::connect(path).unwrap();
        let read = stream.try_clone().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(read).lines().map_while(Result::ok) {
                let message =
                    serde_json::from_str(&line).unwrap_or_else(|_| json!({ "unparsed": line }));
                if tx.send(message).is_err() {
                    return;
                }
            }
        });
        Self {
            stream,
            lines,
            seen: Vec::new(),
            closed: false,
        }
    }

    pub fn send(&mut self, line: &Value) {
        self.stream
            .write_all(format!("{line}\n").as_bytes())
            .unwrap();
    }

    pub fn until(&mut self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        self.try_until(pred)
            .unwrap_or_else(|| panic!("no {what} {}; seen {:#?}", self.missed(), self.seen))
    }

    fn missed(&self) -> String {
        if self.closed {
            "before serve closed the connection".to_owned()
        } else {
            format!("within {T:?}")
        }
    }

    /// `None` once `T` passes without a match, or once serve closes the connection.
    pub fn try_until(&mut self, pred: impl Fn(&Value) -> bool) -> Option<Value> {
        if let Some(i) = self.seen.iter().position(&pred) {
            return Some(self.seen.remove(i));
        }
        let deadline = Instant::now() + T;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(m) if pred(&m) => return Some(m),
                Ok(m) => self.seen.push(m),
                Err(mpsc::RecvTimeoutError::Timeout) => return None,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.closed = true;
                    return None;
                }
            }
        }
    }

    pub fn reply(&mut self, id: &str) -> Value {
        self.until(&format!("reply {id}"), |m| {
            m["type"] != "event" && m["id"] == id
        })
    }

    pub fn source(&mut self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        self.until(what, |m| {
            m["event"]["event"] == "source" && pred(&m["event"])
        })
    }

    pub fn status(&mut self) -> Value {
        self.send(&json!({"id": "st", "cmd": "status"}));
        self.reply("st")["data"].clone()
    }

    /// Whether serve closed this connection, and what it sent that was not an event, for failure messages.
    pub fn connection_report(&mut self) -> String {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !self.closed {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(m) => self.seen.push(m),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => self.closed = true,
            }
        }
        let replies: Vec<&Value> = self.seen.iter().filter(|m| m["type"] != "event").collect();
        let side = if self.closed {
            "serve closed the connection"
        } else {
            "the connection is open"
        };
        format!("{side}; non-event messages unclaimed {replies:?}")
    }

    /// Like `status`, for failure messages: an error instead of a panic when serve is gone or hung.
    pub fn try_status(&mut self) -> Result<Value, String> {
        self.stream
            .write_all(format!("{}\n", json!({"id": "st", "cmd": "status"})).as_bytes())
            .map_err(|e| format!("status unavailable: {e}"))?;
        self.try_until(|m| m["type"] != "event" && m["id"] == "st")
            .map(|m| m["data"].clone())
            .ok_or_else(|| format!("status unavailable: no reply {}", self.missed()))
    }

    pub fn drain(&mut self) {
        while let Ok(m) = self.lines.try_recv() {
            self.seen.push(m);
        }
    }
}

pub fn increasing(name: &str, ts: &[u32]) {
    for (i, w) in ts.windows(2).enumerate() {
        assert!(
            (w[1].wrapping_sub(w[0]) as i32) > 0,
            "{name} RTP timestamp {i}: {} then {}",
            w[0],
            w[1]
        );
    }
}

pub fn is_keyframe(au: &[u8]) -> bool {
    dstreamy_media::annexb::h264_is_keyframe(au)
}

/// The index of the first video frame after the largest RTP gap.
pub fn after_gap(video: &[&RecordedVideoFrame]) -> usize {
    let (i, _) = video
        .windows(2)
        .map(|w| w[1].rtp_timestamp.wrapping_sub(w[0].rtp_timestamp))
        .enumerate()
        .max_by_key(|&(_, g)| g)
        .unwrap();
    i + 1
}

pub fn decodes(video: &[&RecordedVideoFrame], audio: &[&RecordedAudioFrame]) {
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

/// Start indices of the runs separated by RTP jumps of more than `gap_us`.
pub fn runs(timestamps: &[u32], hz: i64, gap_us: i64) -> Vec<usize> {
    let mut starts = vec![0];
    for (i, w) in timestamps.windows(2).enumerate() {
        if i64::from(w[1].wrapping_sub(w[0])) * 1_000_000 / hz > gap_us {
            starts.push(i + 1);
        }
    }
    starts
}

/// `test-hooks` starts every RTP clock at 0, so a timestamp is the pts `serve` stamped on its shared timeline.
pub fn rtp_us(rtp: u32, hz: i64) -> i64 {
    i64::from(rtp) * 1_000_000 / hz
}

/// The same FNV-1a hash `mock-producer --report-sent` prints: Opus bytes, or an access unit's slice NAL units.
pub fn payload_hash(video: bool, payload: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for &b in bytes {
            h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    };
    if video {
        for n in dstreamy_media::annexb::nal_units(payload)
            .filter(|n| matches!(dstreamy_media::annexb::h264_nal_type(n), Some(1 | 5)))
        {
            feed(n);
        }
    } else {
        feed(payload);
    }
    format!("{h:016x}")
}

/// Median output-minus-input pts of the recorded frames whose payload a producer reported sending once.
pub fn shift_us<'a>(
    recorded: impl Iterator<Item = (&'a [u8], u32)>,
    sent: &[Value],
    video: bool,
    hz: i64,
) -> Option<i64> {
    let kind = if video { "video" } else { "audio" };
    let mut by_hash: std::collections::HashMap<&str, Option<i64>> = Default::default();
    for s in sent
        .iter()
        .filter_map(|v| v.get("sent"))
        .filter(|s| s["kind"] == kind)
    {
        by_hash
            .entry(s["hash"].as_str().unwrap())
            .and_modify(|p| *p = None)
            .or_insert(s["pts_us"].as_i64());
    }
    let mut d: Vec<i64> = recorded
        .filter_map(|(data, rtp)| {
            let pts = (*by_hash.get(payload_hash(video, data).as_str())?)?;
            Some(rtp_us(rtp, hz) - pts)
        })
        .collect();
    if d.len() < 10 {
        return None;
    }
    d.sort_unstable();
    Some(d[d.len() / 2])
}
