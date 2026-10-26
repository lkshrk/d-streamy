//! The producer client against a real `dstreamy serve` from `DSTREAMY_BIN`, with mock Discord in process.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use dstreamy_core::ConfigPaths;
use dstreamy_mock_discord::{
    MOCK_GUILD_ID, MOCK_TOKEN, MOCK_VOICE_CHANNEL_ID, MockDiscord, VoiceKind,
};
use dstreamy_obs_producer::{
    End, Endpoint, Hello, Level, Log, Packet, Ring, StartError, Track, attach, run,
};
use dstreamy_test_producer::sync::H264LumaDecoder;
use dstreamy_test_producer::trace::Trace;
use serde_json::{Value, json};
use tempfile::TempDir;

const T: Duration = Duration::from_secs(20);
const TRACE: &str = "linux-x264-main-bf0-keyint2-30fps";

/// `None` skips the test locally; under `CI=true` a missing binary is a failure.
fn dstreamy_bin() -> Option<PathBuf> {
    match std::env::var_os("DSTREAMY_BIN") {
        Some(bin) => Some(PathBuf::from(bin)),
        None if std::env::var("CI").is_ok_and(|v| v == "true") => {
            panic!("DSTREAMY_BIN is unset; CI must point it at the dstreamy binary")
        }
        None => {
            eprintln!("skipped: set DSTREAMY_BIN to a dstreamy binary to run this test");
            None
        }
    }
}

struct Stderr;

impl Log for Stderr {
    fn log(&self, level: Level, message: &str) {
        eprintln!("[{level:?}] {message}");
    }
}

struct Serve {
    rt: tokio::runtime::Runtime,
    mock: MockDiscord,
    _config: TempDir,
    run: TempDir,
    child: Child,
}

impl Serve {
    fn start(bin: &Path) -> Self {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mock = rt.block_on(MockDiscord::start()).unwrap();
        let config = tempfile::tempdir().unwrap();
        ConfigPaths::in_dir(config.path())
            .save_token(MOCK_TOKEN)
            .unwrap();
        std::fs::write(config.path().join("config.toml"), "").unwrap();
        let run = tempfile::Builder::new().prefix("ds").tempdir().unwrap();
        let endpoint = run.path().join("d").join("s.sock");
        let child = Command::new(bin)
            .env_remove("DSTREAMY_TOKEN")
            .env_remove("DSTREAMY_CONFIG_DIR")
            .arg("--config-dir")
            .arg(config.path())
            .arg("--gateway-url")
            .arg(mock.gateway_url())
            .args(["serve", "--skip-platform", "--endpoint"])
            .arg(&endpoint)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + T;
        while UnixStream::connect(&endpoint).is_err() {
            assert!(Instant::now() < deadline, "serve never listened");
            std::thread::sleep(Duration::from_millis(20));
        }
        Self {
            rt,
            mock,
            _config: config,
            run,
            child,
        }
    }

    fn endpoint(&self) -> Endpoint {
        Endpoint::new(self.run.path().join("d").join("s.sock"))
    }

    fn control(&self) -> Control {
        let mut c = Control::connect(self.endpoint().as_path());
        c.send(json!({"id": "h", "cmd": "hello", "role": "control", "protocol": 2}));
        assert_eq!(c.reply("h")["type"], "result");
        c.send(json!({"id": "sub", "cmd": "subscribe"}));
        c.reply("sub");
        c
    }

    fn external_session(&self, trace: &Trace) -> Control {
        let mut c = self.control();
        c.send(json!({
            "id": "start", "cmd": "start", "guild_id": MOCK_GUILD_ID,
            "channel_id": MOCK_VOICE_CHANNEL_ID, "target": "external",
            "resolution": format!("{}x{}", trace.width().unwrap(), trace.height().unwrap()),
            "fps": trace.fps().unwrap(), "bitrate_kbps": 2500,
        }));
        let reply = c.reply("start");
        assert_eq!(reply["type"], "result", "{reply}");
        c.until("streaming", |m| m["event"]["state"]["state"] == "streaming");
        c
    }

    fn terminate(&mut self) {
        let pid = libc::pid_t::try_from(self.child.id()).unwrap();
        // SAFETY: kill has no memory preconditions; the pid is our unreaped child.
        assert_eq!(
            unsafe { libc::kill(pid, libc::SIGTERM) },
            0,
            "SIGTERM to serve"
        );
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Control {
    stream: UnixStream,
    lines: mpsc::Receiver<Value>,
    seen: Vec<Value>,
}

impl Control {
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

    fn send(&mut self, line: Value) {
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
}

fn trace() -> Trace {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../obs/tests/traces")
        .join(TRACE);
    Trace::load(&dir).unwrap()
}

fn hello(trace: &Trace) -> Hello {
    Hello {
        video_codec: "h264".into(),
        width: trace.width().unwrap(),
        height: trace.height().unwrap(),
        fps: trace.fps().unwrap(),
        audio_codec: "opus".into(),
        sample_rate: 48_000,
        channels: 2,
        frame_ms: 20,
        client_name: "obs-dstreamy-test".into(),
        client_version: env!("CARGO_PKG_VERSION").into(),
        obs_version: "32.2.2".into(),
    }
}

/// Feeds the trace's raw OBS packets at their libobs pace, as `encoded_packet` would; returns how many were pushed.
fn feed(ring: &Ring, trace: &Trace, until: impl Fn(usize) -> bool) -> usize {
    let first = trace.packets[0].dts_usec;
    let started = Instant::now();
    let mut n = 0;
    for p in &trace.packets {
        if until(n) {
            break;
        }
        let due = started + Duration::from_micros((p.dts_usec - first).max(0) as u64);
        std::thread::sleep(due.saturating_duration_since(Instant::now()));
        ring.push(Packet {
            track: if p.video { Track::Video } else { Track::Audio },
            keyframe: p.keyframe,
            pts: p.pts,
            dts: p.dts,
            timebase: p.timebase,
            data: p.data.clone(),
            arrived: Instant::now(),
        });
        n += 1;
    }
    n
}

#[test]
fn a_missing_service_is_not_running() {
    let run = tempfile::Builder::new().prefix("ds").tempdir().unwrap();
    let endpoint = Endpoint::new(run.path().join("d").join("s.sock"));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let err = match rt.block_on(attach(&endpoint, &hello(&trace()))) {
        Err(e) => e,
        Ok(_) => panic!("attached to nothing"),
    };
    assert!(err.is_not_running(), "{err:?}");
    assert!(
        err.to_string()
            .starts_with("The D-Streamy service is not running"),
        "{err}"
    );
}

#[test]
fn without_a_session_the_start_is_refused_with_the_service_message() {
    let Some(bin) = dstreamy_bin() else { return };
    let serve = Serve::start(&bin);
    let err = match serve
        .rt
        .block_on(attach(&serve.endpoint(), &hello(&trace())))
    {
        Err(e) => e,
        Ok(_) => panic!("attached without a session"),
    };
    match &err {
        StartError::Refused { code, message } => {
            assert_eq!(code, "no_session");
            assert!(
                message.contains("no session with target external"),
                "{message}"
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn packets_stream_to_discord_and_a_stop_ends_with_end_of_stream() {
    let Some(bin) = dstreamy_bin() else { return };
    let serve = Serve::start(&bin);
    let trace = trace();
    let mut control = serve.external_session(&trace);
    let connection = serve
        .rt
        .block_on(attach(&serve.endpoint(), &hello(&trace)))
        .unwrap();
    assert!(connection.attachment() > 0);
    let busy = match serve.rt.block_on(attach(&serve.endpoint(), &hello(&trace))) {
        Err(e) => e,
        Ok(_) => panic!("a second producer attached"),
    };
    assert!(
        matches!(&busy, StartError::Refused { code, .. } if code == "busy"),
        "{busy:?}"
    );

    let ring = Ring::default();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<&str>();
    let video_extra = trace.video_extra.clone();
    let mut extra_reads = 0;
    let (end, pushed) = std::thread::scope(|s| {
        let feeder = s.spawn(|| {
            let pushed = feed(&ring, &trace, |n| n >= 300);
            stop_tx.send("user stop").unwrap();
            pushed
        });
        let end = serve.rt.block_on(run(
            connection,
            &ring,
            || {
                extra_reads += 1;
                Some(video_extra.clone())
            },
            async { stop_rx.await.unwrap() },
            &Stderr,
        ));
        (end, feeder.join().unwrap())
    });
    assert_eq!(end, End::Stopped("user stop"));
    assert_eq!(extra_reads, 1);
    assert_eq!(pushed, 300);
    assert_eq!(ring.stats().dropped_video + ring.stats().dropped_audio, 0);

    control.until("the attachment ending after end of stream", |m| {
        m["event"]["event"] == "source" && m["event"]["reason"] == "detached"
    });
    let video: Vec<_> = serve
        .mock
        .recorded()
        .video
        .into_iter()
        .filter(|v| v.kind == VoiceKind::Stream)
        .collect();
    assert!(
        video.len() >= 60,
        "the mock recorded {} video frames",
        video.len()
    );
    assert!(
        has_idr(&video[0].data),
        "the first video at the mock is an IDR"
    );
    let mut decoder = H264LumaDecoder::new().unwrap();
    for (i, v) in video.iter().enumerate() {
        decoder
            .decode(&v.data, i as i64)
            .unwrap_or_else(|e| panic!("video {i}: {e}"));
    }
}

/// H.264 Table 7-1: nal_unit_type 5 is an IDR slice.
fn has_idr(au: &[u8]) -> bool {
    au.windows(4)
        .any(|w| w[..3] == [0, 0, 1] && w[3] & 0x1f == 5)
}

#[test]
fn a_service_shutdown_detaches_the_producer() {
    let Some(bin) = dstreamy_bin() else { return };
    let mut serve = Serve::start(&bin);
    let trace = trace();
    let _control = serve.external_session(&trace);
    let connection = serve
        .rt
        .block_on(attach(&serve.endpoint(), &hello(&trace)))
        .unwrap();
    let ring = Ring::default();
    let video_extra = trace.video_extra.clone();
    let done = std::sync::atomic::AtomicBool::new(false);
    let end = std::thread::scope(|s| {
        s.spawn(|| {
            feed(&ring, &trace, |n| {
                n >= 30 || done.load(std::sync::atomic::Ordering::Relaxed)
            })
        });
        std::thread::sleep(Duration::from_millis(300));
        serve.terminate();
        let end = serve.rt.block_on(run(
            connection,
            &ring,
            || Some(video_extra.clone()),
            std::future::pending::<()>(),
            &Stderr,
        ));
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        end
    });
    match end {
        End::Detached { reason, .. } => assert_eq!(reason, "shutdown"),
        other => panic!("{other:?}"),
    }
}

/// serve refuses an IDR without the keyframe flag; its `detached` must surface even when a write fails first.
#[test]
fn an_ingest_refusal_surfaces_as_detached_even_after_a_failed_write() {
    let Some(bin) = dstreamy_bin() else { return };
    let serve = Serve::start(&bin);
    let trace = trace();
    let _control = serve.external_session(&trace);
    let connection = serve
        .rt
        .block_on(attach(&serve.endpoint(), &hello(&trace)))
        .unwrap();
    let ring = Ring::new(100_000, 1 << 30);
    let idr = trace.video().find(|p| p.keyframe).unwrap();
    let mut extra = trace.video_extra.clone();
    extra.extend_from_slice(&idr.data);
    for i in 0..2_000 {
        ring.push(Packet {
            track: Track::Video,
            keyframe: false,
            pts: i,
            dts: i,
            timebase: (1, 30),
            data: extra.clone(),
            arrived: Instant::now(),
        });
    }
    let end = serve.rt.block_on(run(
        connection,
        &ring,
        || None,
        std::future::pending::<()>(),
        &Stderr,
    ));
    match end {
        End::Detached { reason, message } => {
            assert_eq!(reason, "incompatible", "{message}");
        }
        other => panic!("{other:?}"),
    }
}
