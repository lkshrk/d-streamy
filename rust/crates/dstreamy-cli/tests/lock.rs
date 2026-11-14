mod common;

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use dstreamy_cli::lock::SessionLock;
use dstreamy_core::ConfigPaths;
use dstreamy_mock_discord::{MOCK_GUILD_ID, MOCK_TOKEN, MOCK_VOICE_CHANNEL_ID, MockDiscord};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::runtime::Runtime;

const T: Duration = Duration::from_secs(20);

struct Harness {
    _rt: Runtime,
    mock: MockDiscord,
    dir: TempDir,
}

impl Harness {
    fn new() -> Self {
        let rt = Runtime::new().unwrap();
        let mock = rt.block_on(MockDiscord::start()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        ConfigPaths::in_dir(dir.path())
            .save_token(MOCK_TOKEN)
            .unwrap();
        Self { _rt: rt, mock, dir }
    }

    fn cmd(&self) -> Command {
        common::spawnable(self.dir.path(), &self.mock.gateway_url())
    }

    fn spawn(&self, args: &[&str]) -> Proc {
        let mut child = self
            .cmd()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let lines = read_lines(child.stdout.take().unwrap());
        let stderr = read_lines(child.stderr.take().unwrap());
        Proc {
            child,
            stdin,
            lines,
            stderr,
            seen: Vec::new(),
            taken: Vec::new(),
        }
    }

    fn stream(&self) -> Proc {
        let mut s = self.spawn(&common::stream_args(MOCK_VOICE_CHANNEL_ID));
        s.until("streaming state", |l| l == "state: streaming");
        s
    }

    fn stream_is_refused_by(&self, pid: u32) {
        let (status, stderr) = self
            .spawn(&common::stream_args(MOCK_VOICE_CHANNEL_ID))
            .wait();
        assert!(!status.success(), "{status}: {stderr}");
        assert!(
            stderr.contains(&format!("another dstreamy session is live (pid {pid})")),
            "{stderr}"
        );
    }

    fn daemon(&self) -> Proc {
        self.spawn(&["daemon", "--skip-platform"])
    }

    fn lock_is_free(&self) -> bool {
        SessionLock::acquire(&self.dir.path().join("accounts/default")).is_ok()
    }
}

fn read_lines(pipe: impl std::io::Read + Send + 'static) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(pipe).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

struct Proc {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
    stderr: mpsc::Receiver<String>,
    seen: Vec<String>,
    taken: Vec<bool>,
}

impl Proc {
    /// The first line matching `f` that no earlier call returned, waiting for it if needed.
    fn until(&mut self, what: &str, f: impl FnMut(&str) -> bool) -> String {
        self.until_before(what, Instant::now() + T, f)
    }

    fn until_before(
        &mut self,
        what: &str,
        deadline: Instant,
        mut f: impl FnMut(&str) -> bool,
    ) -> String {
        if let Some(i) = (0..self.seen.len()).find(|&i| !self.taken[i] && f(&self.seen[i])) {
            self.taken[i] = true;
            return self.seen[i].clone();
        }
        loop {
            let received = match deadline.checked_duration_since(Instant::now()) {
                Some(left) => self.lines.recv_timeout(left),
                None => Err(mpsc::RecvTimeoutError::Timeout),
            };
            let Ok(line) = received else {
                let stderr: Vec<String> = self.stderr.try_iter().collect();
                panic!("no {what}; seen {:?}; stderr {stderr:?}", self.seen);
            };
            self.seen.push(line.clone());
            let matched = f(&line);
            self.taken.push(matched);
            if matched {
                return line;
            }
        }
    }

    fn send(&mut self, request: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        writeln!(stdin, "{request}").unwrap();
        stdin.flush().unwrap();
    }

    fn reply(&mut self, id: &str) -> Value {
        self.reply_before(id, Instant::now() + T)
    }

    fn reply_before(&mut self, id: &str, deadline: Instant) -> Value {
        let line = self.until_before(&format!("reply to {id}"), deadline, |l| {
            let v: Value = serde_json::from_str(l).unwrap();
            v["type"] != "event" && v["id"] == id
        });
        serde_json::from_str(&line).unwrap()
    }

    fn request(&mut self, request: &Value) -> Value {
        self.send(request);
        self.reply(request["id"].as_str().unwrap())
    }

    fn daemon_state(&mut self, state: &str) -> Value {
        let line = self.until(&format!("{state} state"), |l| {
            let v: Value = serde_json::from_str(l).unwrap();
            v["event"]["event"] == "state" && v["event"]["state"]["state"] == state
        });
        serde_json::from_str(&line).unwrap()
    }

    fn alive(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    fn wait(mut self) -> (std::process::ExitStatus, String) {
        drop(self.stdin.take());
        let deadline = Instant::now() + T;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                panic!("dstreamy did not exit");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        (status, self.stderr.iter().collect::<Vec<_>>().join("\n"))
    }

    fn kill(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_request() -> Value {
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
}

#[test]
fn a_second_stream_is_refused_until_the_first_is_killed() {
    let h = Harness::new();
    let first = h.stream();
    h.stream_is_refused_by(first.child.id());
    first.kill();
    h.stream().kill();
}

#[test]
fn daemon_start_is_already_running_while_a_stream_is_live() {
    let h = Harness::new();
    let s = h.stream();
    let pid = s.child.id();

    let mut d = h.daemon();
    let reply = d.request(&start_request());
    assert_eq!(reply["type"], "error", "{reply}");
    assert_eq!(reply["code"], "already_running", "{reply}");
    assert_eq!(
        reply["message"],
        format!("another dstreamy session is live (pid {pid})")
    );
    let (status, stderr) = d.wait();
    assert!(status.success(), "{status}: {stderr}");
    s.kill();
}

#[test]
fn a_daemon_session_that_ends_by_itself_releases_the_lock() {
    let h = Harness::new();
    let mut d = h.daemon();
    let pid = d.child.id();
    assert_eq!(d.request(&start_request())["type"], "result");
    d.daemon_state("streaming");
    h.stream_is_refused_by(pid);

    assert!(
        h.mock
            .voice_state_from_other_session(MOCK_GUILD_ID, Some(MOCK_VOICE_CHANNEL_ID))
    );
    let stopped = d.daemon_state("stopped");
    assert_eq!(
        stopped["event"]["state"]["reason"]["reason"], "voice_taken_over",
        "{stopped}"
    );

    h.stream().kill();

    assert_eq!(d.request(&start_request())["type"], "result");
    d.daemon_state("streaming");
    h.stream_is_refused_by(pid);
    let (status, stderr) = d.wait();
    assert!(status.success(), "{status}: {stderr}");
}

#[test]
fn a_daemon_stop_releases_the_lock_before_its_reply() {
    let h = Harness::new();
    let mut d = h.daemon();
    assert_eq!(d.request(&start_request())["type"], "result");
    d.daemon_state("streaming");
    let reply = d.request(&json!({ "id": "stop", "cmd": "stop" }));
    assert_eq!(reply["type"], "result", "{reply}");
    assert!(h.lock_is_free(), "the lock outlived the stop reply");

    h.stream().kill();
    assert!(d.alive(), "the daemon exited after stop");
    let (status, stderr) = d.wait();
    assert!(status.success(), "{status}: {stderr}");
}

#[test]
fn read_only_commands_run_while_a_stream_holds_the_lock() {
    let h = Harness::new();
    let s = h.stream();
    let (status, stderr) = h.spawn(&["channels", MOCK_GUILD_ID]).wait();
    assert!(status.success(), "{status}: {stderr}");
    s.kill();
}

#[test]
fn a_start_right_after_a_takeover_keeps_its_lock() {
    let h = Harness::new();
    let mut d = h.daemon();
    assert_eq!(d.request(&start_request())["type"], "result");
    d.daemon_state("streaming");
    assert!(
        h.mock
            .voice_state_from_other_session(MOCK_GUILD_ID, Some(MOCK_VOICE_CHANNEL_ID))
    );
    let deadline = Instant::now() + T;
    let mut takeover_stop_seen = false;
    for n in 0.. {
        assert!(
            Instant::now() < deadline,
            "the taken-over session never stopped"
        );
        let mut restart = start_request();
        restart["id"] = json!(format!("restart-{n}"));
        d.send(&restart);
        let reply = d.reply_before(restart["id"].as_str().unwrap(), deadline);
        if reply["type"] == "result" {
            break;
        }
        assert_eq!(reply["type"], "error", "{reply}");
        match reply["code"].as_str() {
            Some("already_running") => {}
            Some("busy") => {
                if !takeover_stop_seen {
                    let line = d.until_before("takeover stopped state", deadline, |l| {
                        let v: Value = serde_json::from_str(l).unwrap();
                        v["event"]["event"] == "state" && v["event"]["state"]["state"] == "stopped"
                    });
                    let stopped: Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(
                        stopped["event"]["state"]["reason"]["reason"], "voice_taken_over",
                        "{stopped}"
                    );
                    takeover_stop_seen = true;
                }
            }
            _ => panic!("unexpected restart response: {reply}"),
        }
    }
    d.daemon_state("streaming");
    assert!(
        !h.lock_is_free(),
        "the earlier stop released the new run's lock"
    );
    h.stream_is_refused_by(d.child.id());
}
