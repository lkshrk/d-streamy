#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use dstreamy_core::ConfigPaths;
use dstreamy_mock_discord::opcodes::gateway as gw;
use dstreamy_mock_discord::{
    MOCK_GUILD_ID, MOCK_TOKEN, MOCK_VOICE_CHANNEL_ID, MockDiscord, Recorded, VoiceKind,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::runtime::Runtime;

const T: Duration = Duration::from_secs(20);

struct Harness {
    rt: Runtime,
    mock: MockDiscord,
    dir: TempDir,
    run: TempDir,
}

impl Harness {
    fn new() -> Self {
        let rt = Runtime::new().unwrap();
        let mock = rt.block_on(MockDiscord::start()).unwrap();
        Self {
            rt,
            mock,
            dir: tempfile::tempdir().unwrap(),
            run: tempfile::Builder::new()
                .prefix("ds")
                .tempdir_in("/tmp")
                .unwrap(),
        }
    }

    fn logged_in(self) -> Self {
        ConfigPaths::in_dir(self.dir.path())
            .save_token(MOCK_TOKEN)
            .unwrap();
        self
    }

    fn with_config(self, text: &str) -> Self {
        std::fs::write(self.dir.path().join("config.toml"), text).unwrap();
        self
    }

    fn endpoint(&self) -> PathBuf {
        self.run.path().join("d").join("s.sock")
    }

    fn cmd(&self) -> std::process::Command {
        let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("dstreamy"));
        cmd.env_remove("DSTREAMY_TOKEN")
            .env_remove("DSTREAMY_CONFIG_DIR")
            .arg("--config-dir")
            .arg(self.dir.path())
            .arg("--gateway-url")
            .arg(self.mock.gateway_url());
        cmd
    }

    fn spawn_serve(&self, extra: &[&str]) -> Service {
        Service::spawn(
            self.cmd()
                .args([
                    "-v",
                    "serve",
                    "--skip-platform",
                    "--test-hooks",
                    "--endpoint",
                ])
                .arg(self.endpoint())
                .args(extra)
                .stdout(Stdio::null()),
        )
    }

    fn serve(&self, extra: &[&str]) -> Service {
        let s = self.spawn_serve(extra);
        let deadline = Instant::now() + T;
        while UnixStream::connect(self.endpoint()).is_err() {
            assert!(Instant::now() < deadline, "serve never listened");
            std::thread::sleep(Duration::from_millis(20));
        }
        s
    }

    fn client(&self) -> Client {
        Client::connect(&self.endpoint())
    }

    fn control(&self) -> (Client, Value) {
        let mut c = self.client();
        let hello = c.request(&hello("control", 2));
        assert_eq!(hello["type"], "result", "{hello}");
        (c, hello)
    }

    /// Waits until the service holds exactly `n` connections, counting the one that asks.
    fn wait_for_connections(&self, n: u64) {
        let (mut probe, _) = self.control();
        wait_for_live(&mut probe, n + 1);
    }

    fn voice_left(&self) -> bool {
        self.rt
            .block_on(self.mock.wait_for(T, |r| {
                r.ops
                    .iter()
                    .filter(|o| o.is(None, gw::OP_VOICE_STATE_UPDATE))
                    .any(|o| o.json().is_some_and(|d| d["channel_id"].is_null()))
                    .then_some(())
            }))
            .is_some()
    }

    fn stream_video(&self, at_least: usize) -> bool {
        self.rt
            .block_on(
                self.mock
                    .wait_for(T, |r| (stream_video(r) >= at_least).then_some(())),
            )
            .is_some()
    }
}

fn stream_video(r: &Recorded) -> usize {
    r.video
        .iter()
        .filter(|v| v.kind == VoiceKind::Stream)
        .count()
}

struct Service {
    child: Child,
    stderr: Option<std::thread::JoinHandle<String>>,
}

impl Service {
    fn spawn(cmd: &mut std::process::Command) -> Self {
        let mut child = cmd
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let stderr = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        });
        Self {
            child,
            stderr: Some(stderr),
        }
    }

    fn finish(mut self) -> (ExitStatus, String) {
        let deadline = Instant::now() + T;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                panic!("serve did not exit");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        (status, self.stderr.take().unwrap().join().unwrap())
    }

    fn signal(&self, name: &str) {
        let sent = std::process::Command::new("kill")
            .args([&format!("-{name}"), &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(sent.success());
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Client {
    stream: UnixStream,
    lines: mpsc::Receiver<Option<String>>,
    seen: Vec<Value>,
    taken: Vec<bool>,
    closed: bool,
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

impl Client {
    fn connect(path: &Path) -> Self {
        let stream = UnixStream::connect(path).unwrap();
        let read = stream.try_clone().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(read).lines().map_while(Result::ok) {
                if tx.send(Some(line)).is_err() {
                    return;
                }
            }
            let _ = tx.send(None);
        });
        Self {
            stream,
            lines,
            seen: Vec::new(),
            taken: Vec::new(),
            closed: false,
        }
    }

    fn send_raw(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).unwrap();
    }

    fn send(&mut self, line: &Value) {
        self.send_raw(format!("{line}\n").as_bytes());
    }

    fn next_line(&mut self, deadline: Instant) -> Option<Value> {
        let left = deadline.saturating_duration_since(Instant::now());
        match self.lines.recv_timeout(left) {
            Ok(Some(line)) => {
                let message: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("not JSON ({e}): {line}"));
                self.seen.push(message.clone());
                self.taken.push(false);
                Some(message)
            }
            Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.closed = true;
                None
            }
            Err(mpsc::RecvTimeoutError::Timeout) => panic!("timed out; seen {:#?}", self.seen),
        }
    }

    fn until(&mut self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        if let Some(i) = (0..self.seen.len()).find(|&i| !self.taken[i] && pred(&self.seen[i])) {
            self.taken[i] = true;
            return self.seen[i].clone();
        }
        let deadline = Instant::now() + T;
        loop {
            assert!(!self.closed, "closed before {what}; seen {:#?}", self.seen);
            if let Some(message) = self.next_line(deadline)
                && pred(&message)
            {
                *self.taken.last_mut().unwrap() = true;
                return message;
            }
        }
    }

    fn reply(&mut self, id: Option<&str>) -> Value {
        let id = id.map_or(Value::Null, |id| json!(id));
        self.until(&format!("reply to {id}"), |m| {
            m["type"] != "event" && m["id"] == id
        })
    }

    fn request(&mut self, line: &Value) -> Value {
        self.send(line);
        self.reply(line["id"].as_str())
    }

    fn state(&mut self, state: &str) -> Value {
        self.until(&format!("{state} state"), |m| {
            m["event"]["event"] == "state" && m["event"]["state"]["state"] == state
        })
    }

    /// Everything until the service closes the connection.
    fn drain(&mut self) -> Vec<Value> {
        let deadline = Instant::now() + T;
        while !self.closed {
            self.next_line(deadline);
        }
        self.seen.clone()
    }

    /// Whatever arrives within `window`, without waiting for anything in particular.
    fn settle(&mut self, window: Duration) {
        let deadline = Instant::now() + window;
        while let Ok(Some(line)) = self
            .lines
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            self.seen.push(serde_json::from_str(&line).unwrap());
            self.taken.push(false);
        }
    }

    fn is_closed_within(&mut self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while !self.closed {
            if Instant::now() >= deadline {
                return false;
            }
            match self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(Some(line)) => {
                    self.seen.push(serde_json::from_str(&line).unwrap());
                    self.taken.push(false);
                }
                Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => self.closed = true,
                Err(mpsc::RecvTimeoutError::Timeout) => return false,
            }
        }
        true
    }
}

fn hello(role: &str, protocol: u64) -> Value {
    json!({"id": "h", "cmd": "hello", "role": role, "protocol": protocol})
}

fn start(target: &str) -> Value {
    json!({
        "id": "start",
        "cmd": "start",
        "guild_id": MOCK_GUILD_ID,
        "channel_id": MOCK_VOICE_CHANNEL_ID,
        "target": target,
        "resolution": "320x180",
        "fps": 15,
        "bitrate_kbps": 1000,
    })
}

/// Waits until the service holds exactly `n` connections, `probe` included.
fn wait_for_live(probe: &mut Client, n: u64) {
    let deadline = Instant::now() + T;
    loop {
        let live = probe.request(&json!({"id": "n", "cmd": "test_connections"}));
        if live["data"]["live"] == n {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "connections never reached {n}: {live}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn shutdown_and_exit(h: &Harness, s: Service) {
    let (mut c, _) = h.control();
    assert_eq!(
        c.request(&json!({"id": "bye", "cmd": "shutdown"}))["type"],
        "result"
    );
    let (status, stderr) = s.finish();
    assert!(status.success(), "{status}: {stderr}");
}

#[test]
fn hello_negotiates_the_protocol_and_rejects_bad_roles() {
    let h = Harness::new();
    let s = h.serve(&[]);

    let (_, reply) = h.control();
    let data = &reply["data"];
    assert_eq!(data["protocol"], 2);
    assert_eq!(data["platform"], std::env::consts::OS);
    assert!(data["connection"].is_u64(), "{reply}");
    assert_eq!(
        data["status"],
        json!({"session": null, "source": null, "stats": null, "started_by": null})
    );
    assert!(data.get("session").is_none(), "{reply}");

    let mut v1 = h.client();
    let reply = v1.request(&hello("control", 1));
    assert_eq!(reply["data"]["protocol"], 2, "{reply}");
    let status = v1.request(&json!({"id": "st", "cmd": "status"}));
    assert_eq!(status["type"], "result", "{status}");

    let mut c = h.client();
    let reply = c.request(&hello("control", 3));
    assert_eq!(reply["code"], "unsupported_protocol", "{reply}");
    assert!(
        c.is_closed_within(T),
        "still open after unsupported_protocol"
    );

    for line in [
        json!({"id": "h", "cmd": "hello", "protocol": 2}),
        hello("viewer", 2),
        json!({"id": "h", "cmd": "status"}),
    ] {
        let mut c = h.client();
        let reply = c.request(&line);
        assert_eq!(reply["code"], "malformed", "{line} -> {reply}");
        assert!(c.is_closed_within(T), "{line}: still open");
    }

    let mut c = h.client();
    let reply = c.request(&hello("producer", 2));
    assert_eq!(reply["type"], "error", "{reply}");
    assert!(c.is_closed_within(T), "producer: still open");

    shutdown_and_exit(&h, s);
}

#[test]
fn two_controls_share_one_session_and_a_disconnect_keeps_it() {
    let h = Harness::new().logged_in();
    let s = h.serve(&[]);
    let (mut a, hello_a) = h.control();
    let a_id = hello_a["data"]["connection"].clone();
    let (mut b, _) = h.control();
    assert_eq!(
        b.request(&json!({"id": "sub", "cmd": "subscribe"}))["type"],
        "result"
    );

    let started = a.request(&start("test-pattern"));
    assert_eq!(started["type"], "result", "{started}");
    b.state("streaming");
    b.until("health event", |m| m["event"]["event"] == "health");
    a.settle(Duration::from_millis(1500));
    assert!(
        a.seen.iter().all(|m| m["type"] != "event"),
        "A got events without subscribing: {:?}",
        a.seen
    );
    b.settle(Duration::from_millis(200));
    assert!(b.seen.iter().all(|m| m["id"] != "start"), "B got A's reply");

    drop(a);
    h.wait_for_connections(1);
    let before = stream_video(&h.mock.recorded());
    assert!(h.stream_video(before + 15), "the stream stopped with A");
    b.until("health after A left", |m| m["event"]["event"] == "health");

    let (mut a, _) = h.control();
    let status = a.request(&json!({"id": "st", "cmd": "status"}));
    let data = &status["data"];
    assert_eq!(
        data["session"],
        json!({"state": "streaming", "guild_id": MOCK_GUILD_ID, "channel_id": MOCK_VOICE_CHANNEL_ID,
            "channel_name": "General", "target": "test-pattern"}),
        "{status}"
    );
    assert_eq!(data["started_by"], a_id, "{status}");
    assert_eq!(data["source"], Value::Null);
    assert!(data["stats"].is_object(), "{status}");

    assert_eq!(
        a.request(&json!({"id": "stop", "cmd": "stop"}))["type"],
        "result"
    );
    b.state("stopped");
    shutdown_and_exit(&h, s);
}

#[test]
fn a_second_serve_on_the_same_endpoint_is_refused() {
    let h = Harness::new();
    let first = h.serve(&[]);
    let (status, stderr) = h.spawn_serve(&[]).finish();
    assert!(!status.success(), "{status}");
    assert!(stderr.contains("already running"), "{stderr}");
    shutdown_and_exit(&h, first);
}

#[test]
fn a_stale_socket_file_is_replaced() {
    let h = Harness::new();
    let dir = h.endpoint().parent().unwrap().to_owned();
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    bind_stale_socket(&h.endpoint());
    let meta = std::fs::symlink_metadata(h.endpoint()).unwrap();
    assert!(std::os::unix::fs::FileTypeExt::is_socket(&meta.file_type()));
    let refused = UnixStream::connect(h.endpoint()).unwrap_err();
    assert_eq!(refused.kind(), std::io::ErrorKind::ConnectionRefused);
    let s = h.serve(&[]);
    h.control();
    shutdown_and_exit(&h, s);
}

/// In a child: on macOS std marks sockets close-on-exec after creation, so a parallel spawn could inherit it.
fn bind_stale_socket(path: &Path) {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "stale_socket_helper", "--nocapture"])
        .env(STALE_SOCKET_ENV, path)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "{status}");
}

const STALE_SOCKET_ENV: &str = "DSTREAMY_TEST_STALE_SOCKET";

#[test]
fn stale_socket_helper() {
    if let Some(path) = std::env::var_os(STALE_SOCKET_ENV) {
        drop(std::os::unix::net::UnixListener::bind(path).unwrap());
    }
}

#[test]
fn an_oversized_line_is_malformed_and_the_connection_stays_usable() {
    let h = Harness::new();
    let s = h.serve(&[]);
    let (mut c, _) = h.control();
    let mut big = vec![b'x'; (1 << 20) + 10];
    big.push(b'\n');
    c.send_raw(&big);
    let reply = c.reply(None);
    assert_eq!(reply["code"], "malformed", "{reply}");
    c.send_raw(b"\r\n\n  \n{\"id\":\"l\",\"cmd\":\"logged_in\"}\r\n");
    let reply = c.reply(Some("l"));
    assert_eq!(reply["data"], json!({"logged_in": false}), "{reply}");
    shutdown_and_exit(&h, s);
}

#[test]
fn a_subscriber_that_never_reads_is_dropped_with_lagged_and_others_continue() {
    const N: usize = 200;
    let h = Harness::new();
    let s = h.serve(&["--event-capacity", "4"]);
    let mut slow = UnixStream::connect(h.endpoint()).unwrap();
    writeln!(slow, "{}", hello("control", 2)).unwrap();
    writeln!(slow, "{}", json!({"id": "sub", "cmd": "subscribe"})).unwrap();
    let (mut fast, _) = h.control();
    assert_eq!(
        fast.request(&json!({"id": "sub", "cmd": "subscribe"}))["type"],
        "result"
    );
    let (mut driver, _) = h.control();
    let events =
        json!({"id": "ev", "cmd": "test_events", "count": N, "bytes": 8192, "interval_ms": 10});
    assert_eq!(driver.request(&events)["type"], "result");
    let injected = (0..N)
        .map(|n| {
            fast.until("an injected event", |m| {
                m["event"]["message"]
                    .as_str()
                    .is_some_and(|t| t.starts_with(&format!("test event {n} ")))
            })
        })
        .count();
    assert_eq!(injected, N);

    slow.set_read_timeout(Some(T)).unwrap();
    let lines: Vec<Value> = BufReader::new(slow)
        .lines()
        .map(|l| serde_json::from_str(&l.unwrap()).unwrap())
        .collect();
    let last = lines.last().unwrap();
    assert_eq!(last["code"], "lagged", "{last}");
    assert_eq!(last["id"], Value::Null);
    assert!(lines.len() < N, "the slow client got every event");
    let status = fast.request(&json!({"id": "st", "cmd": "status"}));
    assert_eq!(status["type"], "result", "{status}");
    shutdown_and_exit(&h, s);
}

#[test]
fn shutdown_stops_the_session_closes_every_connection_and_exits_zero() {
    let h = Harness::new().logged_in();
    let s = h.serve(&[]);
    let (mut a, _) = h.control();
    let (mut other, _) = h.control();
    assert_eq!(
        a.request(&json!({"id": "sub", "cmd": "subscribe"}))["type"],
        "result"
    );
    assert_eq!(a.request(&start("test-pattern"))["type"], "result");
    a.state("streaming");
    assert_eq!(
        a.request(&json!({"id": "bye", "cmd": "shutdown"}))["type"],
        "result"
    );
    let stopped = a.state("stopped");
    assert_eq!(stopped["event"]["state"]["reason"]["reason"], "requested");
    let order: Vec<&Value> = a.seen.iter().collect();
    let stop_at = order
        .iter()
        .position(|m| m["event"]["state"]["state"] == "stopped")
        .unwrap();
    let reply_at = order.iter().position(|m| m["id"] == "bye").unwrap();
    assert!(
        stop_at < reply_at,
        "the reply came before the session stopped"
    );
    assert!(a.is_closed_within(T));
    assert!(other.is_closed_within(T), "an idle connection stayed open");
    let (status, stderr) = s.finish();
    assert!(status.success(), "{status}: {stderr}");
    assert!(h.voice_left(), "the mock never saw the voice leave");
    assert!(!h.endpoint().exists() || UnixStream::connect(h.endpoint()).is_err());
}

#[test]
fn sigterm_stops_the_session_and_exits_zero() {
    let h = Harness::new().logged_in();
    let s = h.serve(&[]);
    let (mut a, _) = h.control();
    a.request(&json!({"id": "sub", "cmd": "subscribe"}));
    assert_eq!(a.request(&start("test-pattern"))["type"], "result");
    a.state("streaming");
    s.signal("TERM");
    let (status, stderr) = s.finish();
    assert!(status.success(), "{status}: {stderr}");
    assert!(
        h.voice_left(),
        "no op 4 leaving voice; serve stderr:\n{stderr}"
    );
}

#[test]
fn update_on_an_external_session_is_invalid_params() {
    let h = Harness::new().logged_in();
    let s = h.serve(&[]);
    let (mut a, _) = h.control();
    a.request(&json!({"id": "sub", "cmd": "subscribe"}));
    let reply = a.request(&start("external"));
    assert_eq!(reply["type"], "result", "{reply}");
    a.state("streaming");
    let status = a.request(&json!({"id": "st", "cmd": "status"}));
    assert_eq!(
        status["data"]["source"],
        json!({"state": "detached", "stats": {"skipped": 0, "dropped_video": 0,
            "dropped_audio": 0, "queued": 0, "max_queued": 0, "overflow_keyframe_requests": 0}}),
        "{status}"
    );
    let source = a.until("source event", |m| m["event"]["event"] == "source");
    assert_eq!(
        source,
        json!({"type": "event", "event": {"event": "source", "state": "detached"}})
    );

    let update = json!({"id": "u", "cmd": "update", "fps": 30});
    let reply = a.request(&update);
    assert_eq!(reply["code"], "invalid_params", "{reply}");
    let streams = h.mock.streams();
    assert_eq!(streams.len(), 1, "{streams:?}");

    let h264 = json!({"id": "s2", "cmd": "start", "guild_id": MOCK_GUILD_ID,
        "channel_id": MOCK_VOICE_CHANNEL_ID, "target": "external", "codec": "h265"});
    let reply = a.request(&h264);
    assert_eq!(reply["code"], "invalid_params", "{reply}");

    assert_eq!(
        a.request(&json!({"id": "stop", "cmd": "stop"}))["type"],
        "result"
    );
    let status = a.request(&json!({"id": "st2", "cmd": "status"}));
    assert_eq!(status["data"]["source"], Value::Null, "{status}");
    shutdown_and_exit(&h, s);
}

#[test]
fn a_stall_shorter_than_the_first_frames_wait_is_refused() {
    let h = Harness::new().with_config("[serve]\nstall_ms = 499\n");
    let (status, stderr) = h.spawn_serve(&[]).finish();
    assert!(!status.success(), "{status}");
    assert!(stderr.contains("[serve]"), "{stderr}");
    assert!(stderr.contains("first-frames wait"), "{stderr}");

    let h = Harness::new().with_config("[serve]\nstall_ms = 500\nidle = \"none\"\n");
    let s = h.serve(&[]);
    shutdown_and_exit(&h, s);
}

fn stream_args() -> Vec<&'static str> {
    vec![
        "stream",
        "--guild",
        MOCK_GUILD_ID,
        "--channel",
        MOCK_VOICE_CHANNEL_ID,
        "--target",
        "test-pattern",
        "--res",
        "320x180",
        "--fps",
        "15",
        "--bitrate",
        "1000",
    ]
}

#[test]
fn serve_holds_the_session_lock_against_stream_and_the_reverse() {
    let h = Harness::new().logged_in();
    let s = h.serve(&[]);
    let pid = s.child.id();
    let (mut a, _) = h.control();
    a.request(&json!({"id": "sub", "cmd": "subscribe"}));
    assert_eq!(a.request(&start("test-pattern"))["type"], "result");
    a.state("streaming");
    let refused = Service::spawn(h.cmd().args(stream_args()).stdout(Stdio::null()));
    let (status, stderr) = refused.finish();
    assert!(!status.success(), "{status}");
    assert!(
        stderr.contains(&format!("another dstreamy session is live (pid {pid})")),
        "{stderr}"
    );
    assert_eq!(
        a.request(&json!({"id": "stop", "cmd": "stop"}))["type"],
        "result"
    );

    let mut stream = Service::spawn(h.cmd().args(stream_args()).stdout(Stdio::piped()));
    let (tx, streaming) = mpsc::channel();
    let stdout = stream.child.stdout.take().unwrap();
    std::thread::spawn(move || {
        let seen = BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .any(|l| l == "state: streaming");
        let _ = tx.send(seen);
    });
    assert_eq!(
        streaming.recv_timeout(T),
        Ok(true),
        "the stream never streamed"
    );
    let reply = a.request(&json!({"id": "again", "cmd": "start",
        "guild_id": MOCK_GUILD_ID, "channel_id": MOCK_VOICE_CHANNEL_ID, "target": "test-pattern"}));
    assert_eq!(reply["code"], "already_running", "{reply}");
    assert_eq!(
        reply["message"],
        format!(
            "another dstreamy session is live (pid {})",
            stream.child.id()
        )
    );
    drop(stream);
    shutdown_and_exit(&h, s);
}

#[test]
fn a_client_that_floods_requests_without_reading_is_bounded() {
    let h = Harness::new();
    let s = h.serve(&[]);
    let mut flood = UnixStream::connect(h.endpoint()).unwrap();
    writeln!(flood, "{}", hello("control", 2)).unwrap();
    flood
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let line = format!("{}\n", json!({"id": "s", "cmd": "status"}));
    let mut written = 0usize;
    let blocked = loop {
        match flood.write_all(line.as_bytes()) {
            Ok(()) => written += 1,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break true,
            Err(e) => panic!("{e}"),
        }
        if written > 2_000_000 {
            break false;
        }
    };
    assert!(
        blocked,
        "the service kept reading {written} requests nobody read the replies of"
    );
    let rss = rss_kib(s.child.id());
    assert!(rss < 200 * 1024, "service RSS {rss} KiB");

    let (mut other, _) = h.control();
    let reply = other.request(&json!({"id": "l", "cmd": "logged_in"}));
    assert_eq!(reply["type"], "result", "{reply}");
    drop(flood);
    shutdown_and_exit(&h, s);
}

fn rss_kib(pid: u32) -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

#[test]
fn a_connection_that_never_says_hello_is_closed() {
    let h = Harness::new();
    let s = h.serve(&[]);
    let mut silent = h.client();
    let opened = Instant::now();
    assert!(
        silent.is_closed_within(T),
        "the silent connection stayed open"
    );
    let waited = opened.elapsed();
    assert!(waited >= Duration::from_secs(9), "closed after {waited:?}");
    assert!(silent.seen.is_empty(), "{:?}", silent.seen);
    shutdown_and_exit(&h, s);
}

#[test]
fn connections_over_the_cap_are_refused_with_busy() {
    let h = Harness::new();
    let s = h.serve(&[]);
    let (mut probe, _) = h.control();
    // serve's readiness probe may still count until its close is processed.
    wait_for_live(&mut probe, 1);
    let mut held: Vec<Client> = vec![probe];
    held.extend((0..63).map(|_| h.client()));
    for c in &mut held[1..63] {
        assert_eq!(c.request(&hello("control", 2))["type"], "result");
    }
    let mut extra = h.client();
    let reply = extra.reply(None);
    assert_eq!(reply["code"], "busy", "{reply}");
    assert!(extra.is_closed_within(T));
    let last = held.last_mut().unwrap();
    assert_eq!(last.request(&hello("control", 2))["type"], "result");
    held.truncate(1);
    // A fresh probe could be refused while the dropped connections still hold slots.
    wait_for_live(&mut held[0], 1);
    shutdown_and_exit(&h, s);
}

#[test]
fn status_forgets_who_started_a_session_once_it_stops() {
    let h = Harness::new().logged_in();
    let s = h.serve(&[]);
    let (mut a, hello_a) = h.control();
    a.request(&json!({"id": "sub", "cmd": "subscribe"}));
    assert_eq!(a.request(&start("external"))["type"], "result");
    a.state("streaming");
    let status = a.request(&json!({"id": "st1", "cmd": "status"}));
    assert_eq!(status["data"]["started_by"], hello_a["data"]["connection"]);
    assert!(
        h.mock
            .voice_state_from_other_session(MOCK_GUILD_ID, Some(MOCK_VOICE_CHANNEL_ID))
    );
    a.state("stopped");
    let status = a.request(&json!({"id": "st2", "cmd": "status"}));
    assert_eq!(status["data"]["started_by"], Value::Null, "{status}");
    assert_eq!(status["data"]["source"], Value::Null, "{status}");

    assert_eq!(a.request(&start("test-pattern"))["type"], "result");
    a.state("streaming");
    assert_eq!(
        a.request(&json!({"id": "stop", "cmd": "stop"}))["type"],
        "result"
    );
    let status = a.request(&json!({"id": "st3", "cmd": "status"}));
    assert_eq!(status["data"]["started_by"], Value::Null, "{status}");
    shutdown_and_exit(&h, s);
}

#[test]
fn shutdown_waits_for_commands_still_holding_the_connection() {
    let h = Harness::new().logged_in();
    let s = h.serve(&[]);
    let (mut a, _) = h.control();
    a.request(&json!({"id": "sub", "cmd": "subscribe"}));
    assert_eq!(a.request(&start("test-pattern"))["type"], "result");
    a.state("streaming");
    a.send(&json!({"id": "hold", "cmd": "test_hold", "ms": 1500}));
    a.until("hold event", |m| m["event"]["message"] == "test hold true");
    let (mut b, _) = h.control();
    assert_eq!(
        b.request(&json!({"id": "bye", "cmd": "shutdown"}))["type"],
        "result"
    );
    let (status, stderr) = s.finish();
    assert!(status.success(), "{status}: {stderr}");
    assert!(
        !stderr.contains("still in use"),
        "shutdown ran while a command held the connection: {stderr}"
    );
    assert!(
        h.voice_left(),
        "no op 4 leaving voice; serve stderr:\n{stderr}"
    );
}

#[test]
fn shutdown_waits_for_commands_of_clients_that_already_left() {
    let h = Harness::new().logged_in();
    let s = h.serve(&[]);
    let (mut a, _) = h.control();
    a.request(&json!({"id": "sub", "cmd": "subscribe"}));
    assert_eq!(a.request(&start("test-pattern"))["type"], "result");
    a.state("streaming");
    a.send(&json!({"id": "hold", "cmd": "test_hold", "ms": 1500}));
    a.until("hold event", |m| m["event"]["message"] == "test hold true");
    drop(a);
    h.wait_for_connections(0);
    let (mut b, _) = h.control();
    assert_eq!(
        b.request(&json!({"id": "bye", "cmd": "shutdown"}))["type"],
        "result"
    );
    let (status, stderr) = s.finish();
    assert!(status.success(), "{status}: {stderr}");
    assert!(
        !stderr.contains("still in use"),
        "shutdown ran while a departed client's command held the connection: {stderr}"
    );
    assert!(
        h.voice_left(),
        "no op 4 leaving voice; serve stderr:\n{stderr}"
    );
}

#[test]
fn a_second_shutdown_during_shutdown_also_succeeds() {
    let h = Harness::new().logged_in();
    let s = h.serve(&[]);
    let (mut a, _) = h.control();
    let (mut b, _) = h.control();
    a.request(&json!({"id": "sub", "cmd": "subscribe"}));
    assert_eq!(a.request(&start("test-pattern"))["type"], "result");
    a.state("streaming");
    a.send(&json!({"id": "bye-a", "cmd": "shutdown"}));
    b.send(&json!({"id": "bye-b", "cmd": "shutdown"}));
    assert_eq!(b.reply(Some("bye-b"))["type"], "result");
    assert_eq!(a.reply(Some("bye-a"))["type"], "result");
    let (status, stderr) = s.finish();
    assert!(status.success(), "{status}: {stderr}");
}

#[test]
fn lines_still_unread_when_the_service_closes_are_answered() {
    let h = Harness::new();
    let s = h.serve(&[]);
    let (mut a, _) = h.control();
    let (mut b, _) = h.control();
    assert_eq!(
        b.request(&json!({"id": "mute", "cmd": "test_mute"}))["type"],
        "result"
    );
    b.send(&json!({"id": "st", "cmd": "status"}));
    b.send(&json!({"id": "bye-b", "cmd": "shutdown"}));
    assert_eq!(
        a.request(&json!({"id": "bye-a", "cmd": "shutdown"}))["type"],
        "result"
    );
    let status = b.reply(Some("st"));
    assert_eq!(status["code"], "internal", "{status}");
    assert_eq!(b.reply(Some("bye-b"))["type"], "result");
    assert!(b.is_closed_within(T));
    let (status, stderr) = s.finish();
    assert!(status.success(), "{status}: {stderr}");
}

#[test]
fn the_endpoint_stays_bound_until_the_session_is_torn_down_and_is_removed_after() {
    let h = Harness::new().logged_in();
    let s = h.serve(&[]);
    let (mut a, _) = h.control();
    a.request(&json!({"id": "sub", "cmd": "subscribe"}));
    assert_eq!(a.request(&start("test-pattern"))["type"], "result");
    a.state("streaming");
    a.send(&json!({"id": "bye", "cmd": "shutdown"}));
    let lock_free =
        || dstreamy_cli::lock::SessionLock::acquire(&ConfigPaths::in_dir(h.dir.path())).is_ok();
    let deadline = Instant::now() + T;
    while h.endpoint().exists() {
        assert!(
            Instant::now() < deadline,
            "the socket file was never removed"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        lock_free(),
        "the endpoint was given up while the session still held the lock"
    );
    let (status, stderr) = s.finish();
    assert!(status.success(), "{status}: {stderr}");
    assert!(
        h.voice_left(),
        "no op 4 leaving voice; serve stderr:\n{stderr}"
    );
}

/// One scenario per fixture pair, as the daemon fixtures: replies in request order, then kept events.
mod golden {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/serve")
            .join(name)
    }

    fn requests(name: &str) -> Vec<String> {
        std::fs::read_to_string(fixture(&format!("{name}.in.ndjson")))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn zero(value: &mut Value) {
        let Value::Object(fields) = value else { return };
        for (key, v) in fields.iter_mut() {
            if key.ends_with("_level") && v.is_string() {
                *v = json!("good");
            } else if key.ends_with("_series") && v.is_array() {
                *v = json!([]);
            } else if key == "last_error" {
                *v = Value::Null;
            } else {
                match v {
                    Value::Number(_) => *v = json!(0),
                    Value::Bool(_) => *v = json!(false),
                    Value::Object(_) => zero(v),
                    _ => {}
                }
            }
        }
    }

    fn status(value: &mut Value) {
        if !value["started_by"].is_null() {
            value["started_by"] = json!("<connection>");
        }
        if value["stats"].is_object() {
            zero(&mut value["stats"]);
        }
        if value["source"].get("for_ms").is_some() {
            value["source"]["for_ms"] = json!(0);
        }
        if value["source"]["stats"].is_object() {
            zero(&mut value["source"]["stats"]);
        }
    }

    /// Replies in request order, then state events and the first health event, with run-specific values replaced.
    fn normalize(messages: &[Value]) -> Vec<Value> {
        let mut replies = Vec::new();
        let mut events = Vec::new();
        let mut health = false;
        for m in messages {
            let mut m = m.clone();
            if m["type"] == "event" {
                match m["event"]["event"].as_str() {
                    Some("health") if !health => {
                        health = true;
                        zero(&mut m["event"]);
                    }
                    Some("state") if m["event"]["state"]["state"] != "reconnecting" => {}
                    _ => continue,
                }
                events.push(m);
                continue;
            }
            if let Some(data) = m.get_mut("data") {
                if data.get("protocol").is_some() {
                    data["version"] = json!("<version>");
                    data["platform"] = json!("<platform>");
                    data["connection"] = json!("<connection>");
                    status(&mut data["status"]);
                } else if data.get("started_by").is_some() {
                    status(data);
                } else if data.get("video_frames_sent").is_some() {
                    zero(data);
                }
            }
            replies.push(m);
        }
        replies.extend(events);
        replies
    }

    fn check(name: &str, messages: &[Value]) {
        let actual = normalize(messages);
        let path = fixture(&format!("{name}.out.ndjson"));
        if std::env::var_os("DSTREAMY_UPDATE_FIXTURES").is_some() {
            let text: String = actual.iter().map(|v| format!("{v}\n")).collect();
            std::fs::write(&path, text).unwrap();
            return;
        }
        let expected: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let show = |vs: &[Value]| vs.iter().map(|v| format!("{v}\n")).collect::<String>();
        assert!(
            actual == expected,
            "{name}: expected\n{}\ngot\n{}",
            show(&expected),
            show(&actual)
        );
    }

    fn run(h: &Harness, name: &str) -> Vec<Value> {
        let mut c = h.client();
        for line in requests(name) {
            let request: Option<Value> = serde_json::from_str(&line).ok();
            c.send_raw(format!("{line}\n").as_bytes());
            let id = request
                .as_ref()
                .and_then(|r| r["id"].as_str())
                .map(str::to_owned);
            let ok = c.reply(id.as_deref())["type"] == "result";
            let cmd = request.as_ref().map(|r| r["cmd"].clone()).filter(|_| ok);
            if cmd == Some(json!("start")) {
                c.state("streaming");
                c.until("health event", |m| m["event"]["event"] == "health");
            }
            if cmd == Some(json!("stop")) {
                c.state("stopped");
            }
        }
        c.drain()
    }

    #[test]
    fn control_session_matches_the_fixture() {
        let h = Harness::new();
        let s = h.serve(&[]);
        let messages = run(&h, "control");
        let (status, stderr) = s.finish();
        assert!(status.success(), "{status}: {stderr}");
        check("control", &messages);
    }

    #[test]
    fn handshakes_match_the_fixture() {
        let h = Harness::new();
        let s = h.serve(&[]);
        let mut all = Vec::new();
        for line in requests("handshake") {
            let mut c = h.client();
            c.send_raw(format!("{line}\n").as_bytes());
            c.stream.shutdown(std::net::Shutdown::Write).unwrap();
            all.extend(c.drain());
        }
        check("handshake", &all);
        shutdown_and_exit(&h, s);
    }
}
