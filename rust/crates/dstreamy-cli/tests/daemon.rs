use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use dstreamy_core::ConfigPaths;
use dstreamy_mock_discord::opcodes::gateway as gw;
use dstreamy_mock_discord::opcodes::voice as vo;
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
}

impl Harness {
    fn new() -> Self {
        let rt = Runtime::new().unwrap();
        let mock = rt.block_on(MockDiscord::start()).unwrap();
        Self {
            rt,
            mock,
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn logged_in(self) -> Self {
        ConfigPaths::in_dir(self.dir.path())
            .save_token(MOCK_TOKEN)
            .unwrap();
        self
    }

    fn log_file(&self) -> PathBuf {
        self.dir.path().join("dstreamy.log")
    }

    fn spawn(&self, verbose: bool) -> Daemon {
        let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("dstreamy"));
        cmd.env_remove("DSTREAMY_TOKEN")
            .env_remove("DSTREAMY_CONFIG_DIR")
            .arg("--config-dir")
            .arg(self.dir.path())
            .arg("--gateway-url")
            .arg(self.mock.gateway_url());
        if verbose {
            cmd.arg("-vv");
        }
        let mut child = cmd
            .args(["daemon", "--skip-platform"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut stderr = child.stderr.take().unwrap();
        let stderr = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        });
        Daemon {
            child,
            stdin,
            lines,
            stderr: Some(stderr),
            raw: Vec::new(),
            seen: Vec::new(),
            taken: Vec::new(),
        }
    }

    fn stream_deleted(&self) -> bool {
        self.rt
            .block_on(self.mock.wait_for_op(None, gw::OP_STREAM_DELETE, 0, T))
            .is_some()
    }
}

struct Daemon {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
    stderr: Option<std::thread::JoinHandle<String>>,
    raw: Vec<String>,
    seen: Vec<Value>,
    taken: Vec<bool>,
}

struct Finished {
    status: ExitStatus,
    stdout: String,
    stderr: String,
    messages: Vec<Value>,
}

impl Daemon {
    fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
    }

    /// The first message matching `pred` that no earlier call returned, waiting for it if needed.
    fn until(&mut self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        if let Some(i) = (0..self.seen.len()).find(|&i| !self.taken[i] && pred(&self.seen[i])) {
            self.taken[i] = true;
            return self.seen[i].clone();
        }
        let deadline = Instant::now() + T;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Ok(line) = self.lines.recv_timeout(left) else {
                panic!("no {what}; seen {:#?}", self.seen);
            };
            let message: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("stdout line is not JSON ({e}): {line}"));
            self.raw.push(line);
            self.seen.push(message.clone());
            let matched = pred(&message);
            self.taken.push(matched);
            if matched {
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

    fn request(&mut self, line: &str) -> Value {
        let id = serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|v| v["id"].as_str().map(str::to_owned));
        self.send(line);
        self.reply(id.as_deref())
    }

    fn state(&mut self, state: &str) {
        self.until(&format!("{state} state"), |m| {
            m["event"]["event"] == "state" && m["event"]["state"]["state"] == state
        });
    }

    fn close_stdin(&mut self) {
        self.stdin = None;
    }

    fn finish(mut self) -> Finished {
        let deadline = Instant::now() + T;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                panic!("the daemon did not exit; seen {:#?}", self.seen);
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        for line in self.lines.iter() {
            self.seen.push(serde_json::from_str(&line).unwrap());
            self.raw.push(line);
        }
        Finished {
            status,
            stdout: self.raw.join("\n"),
            stderr: self.stderr.take().unwrap().join().unwrap(),
            messages: self.seen,
        }
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/daemon")
        .join(name)
}

/// Replies in request order, then state events and the first health event with its values zeroed.
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
                    scrub(&mut m["event"]);
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
            }
            if data.get("video_frames_sent").is_some() {
                scrub(data);
            }
        }
        replies.push(m);
    }
    replies.extend(events);
    replies
}

fn scrub(value: &mut Value) {
    let Value::Object(fields) = value else { return };
    for (key, v) in fields.iter_mut() {
        if key.ends_with("_level") {
            *v = json!("good");
        } else if key.ends_with("_series") {
            *v = json!([]);
        } else if key == "last_error" {
            *v = Value::Null;
        } else {
            match v {
                Value::Number(_) => *v = json!(0),
                Value::Bool(_) => *v = json!(false),
                Value::Object(_) => scrub(v),
                _ => {}
            }
        }
    }
}

fn check_fixture(name: &str, messages: &[Value]) {
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

fn requests(name: &str) -> Vec<String> {
    std::fs::read_to_string(fixture(&format!("{name}.in.ndjson")))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn session_scenario_matches_the_fixture_and_never_shows_the_token() {
    let h = Harness::new();
    let mut d = h.spawn(true);
    for line in requests("session") {
        let reply = d.request(&line);
        assert_eq!(reply["type"], "result", "{line} -> {reply}");
        if line.contains("\"cmd\":\"start\"") {
            d.state("streaming");
            d.until("health event", |m| m["event"]["event"] == "health");
        }
        if line.contains("\"cmd\":\"update\"") {
            let announced =
                h.rt.block_on(h.mock.wait_for(T, |r| announced(r, 640, 30).then_some(())));
            assert!(announced.is_some(), "no op 12 for 640x360 at 30 fps");
        }
    }
    let done = d.finish();
    assert!(done.status.success(), "{}: {}", done.status, done.stderr);
    check_fixture("session", &done.messages);
    assert_eq!(
        states(&done.messages),
        ["connecting", "streaming", "stopped"]
    );
    assert!(h.stream_deleted(), "no STREAM_DELETE");

    let log = std::fs::read_to_string(h.log_file()).unwrap();
    assert!(log.contains("DEBUG"), "-vv did not reach the log file");
    assert!(!done.stderr.is_empty(), "-v did not reach stderr");
    for (what, text) in [
        ("stdout", &done.stdout),
        ("stderr", &done.stderr),
        ("log", &log),
    ] {
        assert!(!text.contains(MOCK_TOKEN), "the token is in {what}");
    }
}

#[test]
fn errors_scenario_matches_the_fixture_and_keeps_running() {
    let h = Harness::new();
    let mut d = h.spawn(true);
    for line in requests("errors") {
        d.request(&line);
    }
    let done = d.finish();
    assert!(done.status.success(), "{}: {}", done.status, done.stderr);
    check_fixture("errors", &done.messages);
    assert!(h.mock.streams().is_empty());
    let log = std::fs::read_to_string(h.log_file()).unwrap();
    for (what, text) in [
        ("stdout", &done.stdout),
        ("stderr", &done.stderr),
        ("log", &log),
    ] {
        assert!(
            !text.contains("wrong-token"),
            "the wrong token is in {what}"
        );
        assert!(!text.contains(MOCK_TOKEN), "the token is in {what}");
    }
}

fn start_line() -> String {
    json!({
        "id": "start",
        "cmd": "start",
        "guild_id": MOCK_GUILD_ID,
        "channel_id": MOCK_VOICE_CHANNEL_ID,
        "target": "test-pattern",
        "resolution": "320x180",
        "fps": 15,
        "bitrate_kbps": 1000,
    })
    .to_string()
}

#[test]
fn eof_on_stdin_stops_the_stream_and_exits() {
    let h = Harness::new().logged_in();
    let mut d = h.spawn(false);
    assert_eq!(d.request(&start_line())["type"], "result");
    d.state("streaming");
    d.close_stdin();
    let done = d.finish();
    assert!(done.status.success(), "{}: {}", done.status, done.stderr);
    assert!(h.stream_deleted(), "no STREAM_DELETE after EOF");
    let last = done.messages.last().unwrap();
    assert_eq!(
        last["event"]["state"]["reason"]["reason"], "requested",
        "{last}"
    );
}

#[cfg(unix)]
#[test]
fn sigterm_stops_the_stream_and_exits() {
    let h = Harness::new().logged_in();
    let mut d = h.spawn(false);
    d.request(&start_line());
    d.state("streaming");
    let killed = std::process::Command::new("kill")
        .args(["-TERM", &d.child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let done = d.finish();
    assert!(done.status.success(), "{}: {}", done.status, done.stderr);
    assert!(h.stream_deleted(), "no STREAM_DELETE after SIGTERM");
}

#[test]
fn pipelined_commands_are_all_answered_while_streaming() {
    let h = Harness::new().logged_in();
    let mut d = h.spawn(false);
    d.request(&start_line());
    d.state("streaming");
    for (id, cmd) in [
        ("g", "guilds"),
        ("t", "targets"),
        ("l", "logged_in"),
        ("s", "stop"),
    ] {
        d.send(&json!({ "id": id, "cmd": cmd }).to_string());
    }
    let mut pending: Vec<&str> = vec!["g", "t", "l", "s"];
    while !pending.is_empty() {
        let reply = d.until("a reply", |m| m["type"] != "event");
        assert_eq!(reply["type"], "result", "{reply}");
        pending.retain(|id| reply["id"] != *id);
    }
    d.state("stopped");
    d.request(&json!({ "id": "again", "cmd": "stop" }).to_string());
    d.close_stdin();
    let done = d.finish();
    assert!(done.status.success());
    let again = done.messages.iter().find(|m| m["id"] == "again").unwrap();
    assert_eq!(again["code"], "not_running", "{again}");
}

fn stream_video(r: &Recorded) -> usize {
    r.video
        .iter()
        .filter(|v| v.kind == VoiceKind::Stream)
        .count()
}

fn announced(r: &Recorded, width: u64, fps: u64) -> bool {
    r.ops
        .iter()
        .rev()
        .filter(|o| o.is(Some(VoiceKind::Stream), vo::OP_VIDEO))
        .find_map(|o| o.json().map(|d| d["streams"][0].clone()))
        .is_some_and(|s| s["max_resolution"]["width"] == width && s["max_framerate"] == fps)
}

#[test]
fn update_restarts_capture_inside_the_running_go_live() {
    let h = Harness::new().logged_in();
    let mut d = h.spawn(false);
    d.request(&start_line());
    d.state("streaming");
    let before = h.rt.block_on(
        h.mock
            .wait_for(T, |r| (stream_video(r) >= 10).then(|| stream_video(r))),
    );
    let before = before.expect("video before the update");

    let update = json!({
        "id": "u",
        "cmd": "update",
        "resolution": "640x360",
        "fps": 30,
        "bitrate_kbps": 1500,
        "crop": {"x": 0, "y": 0, "width": 640, "height": 360},
    });
    let reply = d.request(&update.to_string());
    assert_eq!(reply["type"], "result", "{reply}");
    let after = h.rt.block_on(h.mock.wait_for(T, |r| {
        (announced(r, 640, 30) && stream_video(r) >= before + 40).then(|| r.clone())
    }));
    let after = after.expect("no op 12 for 640x360 at 30 fps, or no video after it");
    let timestamps: Vec<u32> = after
        .video
        .iter()
        .filter(|v| v.kind == VoiceKind::Stream)
        .skip(before + 10)
        .map(|v| v.rtp_timestamp)
        .collect();
    let mut steps: Vec<u32> = timestamps
        .windows(2)
        .map(|w| w[1].wrapping_sub(w[0]))
        .collect();
    steps.sort_unstable();
    let median = steps[steps.len() / 2];
    assert!(
        median.abs_diff(3000) <= 300,
        "median RTP step {median}: the source is not capturing at 30 fps"
    );
    let ops = h.mock.received_ops();
    let count = |op| ops.iter().filter(|o| o.is(None, op)).count();
    assert_eq!(count(gw::OP_STREAM_CREATE), 1);
    assert_eq!(count(gw::OP_STREAM_DELETE), 0);

    let bad = json!({"id": "bad", "cmd": "update", "fps": 24});
    assert_eq!(d.request(&bad.to_string())["code"], "invalid_params");
    d.close_stdin();
    let done = d.finish();
    assert!(done.status.success(), "{}: {}", done.status, done.stderr);
    assert_eq!(
        states(&done.messages),
        ["connecting", "streaming", "stopped"]
    );
}

fn states(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .filter(|m| m["event"]["event"] == "state")
        .filter_map(|m| m["event"]["state"]["state"].as_str())
        .filter(|s| *s != "reconnecting")
        .map(str::to_owned)
        .collect()
}
