use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use clap::Parser;
use dstreamy_cli::Ctx;
use dstreamy_cli::output::Out;
use dstreamy_cli::signals::Signals;
use dstreamy_cli::stream::{self, StreamArgs};
use dstreamy_core::ConfigPaths;
use dstreamy_mock_discord::opcodes::gateway as gw;
use dstreamy_mock_discord::{
    MOCK_GUILD_ID, MOCK_TEXT_CHANNEL_ID, MOCK_TOKEN, MOCK_USER_ID, MOCK_VOICE_CHANNEL_ID,
    MockChannel, MockConfig, MockDiscord, MockGuild,
};
use serde_json::Value;
use tempfile::TempDir;
use tokio::runtime::Runtime;

const T: Duration = Duration::from_secs(20);
const LATE_GUILD_ID: &str = "200000000000000002";

struct Harness {
    rt: Runtime,
    mock: MockDiscord,
    dir: TempDir,
}

impl Harness {
    fn new() -> Self {
        Self::with(MockConfig::default())
    }

    fn with(config: MockConfig) -> Self {
        let rt = Runtime::new().unwrap();
        let mock = rt.block_on(MockDiscord::start_with(config)).unwrap();
        Self {
            rt,
            mock,
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn config_dir(&self) -> &Path {
        self.dir.path()
    }

    fn cmd(&self) -> Command {
        let mut cmd = bare_cmd();
        cmd.arg("--config-dir")
            .arg(self.config_dir())
            .arg("--gateway-url")
            .arg(self.mock.gateway_url());
        cmd
    }

    fn logged_in(self) -> Self {
        self.cmd()
            .arg("login")
            .write_stdin(format!("{MOCK_TOKEN}\n"))
            .assert()
            .success();
        self
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.cmd().arg("--json").args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

fn bare_cmd() -> Command {
    let mut cmd = Command::cargo_bin("dstreamy").unwrap();
    cmd.env_remove("DSTREAMY_TOKEN")
        .env_remove("DSTREAMY_CONFIG_DIR")
        .timeout(T);
    cmd
}

fn stdout(cmd: &mut Command) -> String {
    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn stderr_of_failure(cmd: &mut Command) -> String {
    let output = cmd.output().unwrap();
    assert!(!output.status.success());
    String::from_utf8(output.stderr).unwrap()
}

#[test]
fn a_remote_gateway_url_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let output = bare_cmd()
        .env("DSTREAMY_TOKEN", MOCK_TOKEN)
        .arg("--config-dir")
        .arg(dir.path())
        .args(["--gateway-url", "ws://10.0.0.1:9", "guilds"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("loopback"), "{stderr}");
}

#[test]
fn help_runs_from_the_built_binary() {
    let text = stdout(bare_cmd().arg("--help"));
    for command in ["login", "guilds", "channels", "targets", "stream"] {
        assert!(text.contains(command), "{text}");
    }
    assert!(!text.contains("gateway-url"));
}

#[test]
fn closed_stdout_is_not_an_error() {
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("dstreamy"))
        .arg("--help")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
}

#[test]
fn login_verifies_and_stores_the_token() {
    let h = Harness::new();
    let out = h
        .cmd()
        .args(["--json", "login"])
        .write_stdin(format!("  {MOCK_TOKEN}\n"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let row: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(row["id"], MOCK_USER_ID);
    assert_eq!(row["username"], "mockuser");
    let token_file = h.config_dir().join("token");
    assert_eq!(
        std::fs::read_to_string(&token_file).unwrap().trim(),
        MOCK_TOKEN
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&token_file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    let text = stdout(h.cmd().arg("login").write_stdin(MOCK_TOKEN));
    assert!(text.contains("logged in as mockuser"), "{text}");
}

#[test]
fn login_rejects_a_bad_or_missing_token() {
    let h = Harness::new();
    let err = stderr_of_failure(h.cmd().arg("login").write_stdin("wrong-token\n"));
    assert!(err.contains("4004"), "{err}");
    let err = stderr_of_failure(h.cmd().arg("login").write_stdin("\n"));
    assert!(err.contains("no token"), "{err}");
    assert!(!h.config_dir().join("token").exists());
}

#[test]
fn commands_need_a_token() {
    let h = Harness::new();
    let err = stderr_of_failure(h.cmd().arg("guilds"));
    assert!(err.contains("dstreamy login"), "{err}");

    let text = stdout(h.cmd().arg("guilds").env("DSTREAMY_TOKEN", MOCK_TOKEN));
    assert!(text.contains("Mock Guild"), "{text}");
}

#[test]
fn guilds_include_ones_announced_after_ready() {
    let mut config = MockConfig::default();
    config.guilds.push(MockGuild {
        id: LATE_GUILD_ID.to_owned(),
        name: "Late Guild".to_owned(),
        channels: vec![MockChannel::voice("300000000000000009", "Lobby")],
        in_ready: false,
    });
    let h = Harness::with(config).logged_in();

    let guilds = h.json(&["guilds"]);
    let rows = guilds.as_array().unwrap();
    assert_eq!(rows.len(), 2, "{guilds}");
    assert_eq!(rows[0]["id"], MOCK_GUILD_ID);
    assert_eq!(rows[0]["name"], "Mock Guild");
    assert_eq!(rows[1]["name"], "Late Guild");
    assert!(rows.iter().all(|g| g["available"] == true), "{guilds}");

    let text = stdout(h.cmd().arg("guilds"));
    assert!(
        text.starts_with(&format!("{MOCK_GUILD_ID}  Mock Guild\n")),
        "{text}"
    );

    let late = h.json(&["channels", LATE_GUILD_ID]);
    assert_eq!(late[0]["name"], "Lobby");
}

#[test]
fn channels_lists_voice_channels_only() {
    let h = Harness::new().logged_in();
    let channels = h.json(&["channels", MOCK_GUILD_ID]);
    let rows = channels.as_array().unwrap();
    let names: Vec<&str> = rows.iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["General", "Gaming"]);
    assert!(rows.iter().all(|c| c["kind"] == "voice"));
    assert!(rows.iter().all(|c| c["id"] != MOCK_TEXT_CHANNEL_ID));

    let text = stdout(h.cmd().args(["channels", MOCK_GUILD_ID]));
    assert!(
        text.starts_with(&format!("{MOCK_VOICE_CHANNEL_ID}  voice  General\n")),
        "{text}"
    );

    let err = stderr_of_failure(h.cmd().args(["channels", "999"]));
    assert!(err.contains("unknown guild 999"), "{err}");
}

#[test]
fn targets_always_offer_the_test_pattern() {
    let mut cmd = bare_cmd();
    cmd.args(["--json", "targets"]);
    // ScreenCaptureKit would ask for the Screen Recording permission.
    #[cfg(target_os = "macos")]
    cmd.arg("--skip-platform");
    let targets: Value = serde_json::from_str(&stdout(&mut cmd)).unwrap();
    let rows = targets.as_array().unwrap();
    let pattern = rows.iter().find(|t| t["id"] == "test-pattern").unwrap();
    assert_eq!(pattern["kind"], "display");
    #[cfg(target_os = "linux")]
    assert!(rows.iter().any(|t| t["id"] == "portal"), "{targets}");

    let text = stdout(bare_cmd().args(["targets", "--skip-platform"]));
    assert_eq!(text, "test-pattern  display  1280x720  Test pattern\n");
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "needs Screen Recording permission"]
fn targets_list_macos_displays() {
    let targets: Value =
        serde_json::from_str(&stdout(bare_cmd().args(["--json", "targets"]))).unwrap();
    let rows = targets.as_array().unwrap();
    assert!(
        rows.iter()
            .any(|t| t["id"].as_str().unwrap().starts_with("display:")),
        "{targets}"
    );
}

fn stream_args(channel: &str) -> Vec<&str> {
    vec![
        "stream",
        "--guild",
        MOCK_GUILD_ID,
        "--channel",
        channel,
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
fn stream_rejects_bad_settings_and_unknown_channels() {
    let h = Harness::new().logged_in();
    let mut args = stream_args(MOCK_VOICE_CHANNEL_ID);
    args.extend(["--gain", "5"]);
    let err = stderr_of_failure(h.cmd().args(&args));
    assert!(err.contains("audio gain"), "{err}");

    let mut args = stream_args(MOCK_VOICE_CHANNEL_ID);
    let fps = args.iter().position(|a| *a == "15").unwrap();
    args[fps] = "24";
    h.cmd().args(&args).assert().code(2);

    let err = stderr_of_failure(h.cmd().args(stream_args(MOCK_TEXT_CHANNEL_ID)));
    assert!(err.contains("no voice channel"), "{err}");
    assert!(h.mock.streams().is_empty());

    std::fs::write(
        h.config_dir().join("config.toml"),
        "[stream]\nbitrate_kbps = 50000\n",
    )
    .unwrap();
    let err = stderr_of_failure(h.cmd().args(stream_args(MOCK_VOICE_CHANNEL_ID)));
    assert!(err.contains("config.toml"), "{err}");
}

struct Lines {
    rx: mpsc::Receiver<String>,
    seen: Vec<String>,
}

impl Lines {
    fn spawn(pipe: impl std::io::Read + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines() {
                let Ok(line) = line else { return };
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        Self {
            rx,
            seen: Vec::new(),
        }
    }

    fn until(&mut self, what: &str, mut f: impl FnMut(&str) -> bool) -> String {
        let deadline = Instant::now() + T;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self
                .rx
                .recv_timeout(left)
                .unwrap_or_else(|e| panic!("no {what} ({e}); seen {:?}", self.seen));
            self.seen.push(line.clone());
            if f(&line) {
                return line;
            }
        }
    }

    fn until_json(&mut self, what: &str, mut f: impl FnMut(&Value) -> bool) -> Value {
        let line = self.until(what, |l| {
            let v: Value =
                serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}"));
            f(&v)
        });
        serde_json::from_str(&line).unwrap()
    }

    fn rest(self) -> Vec<String> {
        self.rx.iter().collect()
    }
}

fn is_state(v: &Value, state: &str) -> bool {
    v["event"] == "state" && v["state"]["state"] == state
}

#[cfg(unix)]
struct Streaming {
    child: Child,
    lines: Lines,
    stderr: Lines,
}

#[cfg(unix)]
impl Streaming {
    fn spawn(h: &Harness, extra: &[&str]) -> Self {
        let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("dstreamy"))
            .env_remove("DSTREAMY_TOKEN")
            .env_remove("DSTREAMY_CONFIG_DIR")
            .arg("--config-dir")
            .arg(h.config_dir())
            .arg("--gateway-url")
            .arg(h.mock.gateway_url())
            .args(stream_args(MOCK_VOICE_CHANNEL_ID))
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let lines = Lines::spawn(child.stdout.take().unwrap());
        let stderr = Lines::spawn(child.stderr.take().unwrap());
        Self {
            child,
            lines,
            stderr,
        }
    }

    fn sigint(&self) {
        let killed = std::process::Command::new("kill")
            .args(["-INT", &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(killed.success());
    }

    /// Sends SIGINT and returns the exit status, stderr and the remaining stdout lines.
    fn interrupt(self) -> (std::process::ExitStatus, String, Vec<String>) {
        self.sigint();
        self.wait()
    }

    fn wait(mut self) -> (std::process::ExitStatus, String, Vec<String>) {
        let deadline = Instant::now() + T;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                panic!("dstreamy did not exit after SIGINT");
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let mut stderr = self.stderr.seen;
        stderr.extend(self.stderr.rx.iter());
        (status, stderr.join("\n"), self.lines.rest())
    }
}

#[cfg(unix)]
#[test]
fn stream_test_pattern_until_sigint() {
    let h = Harness::new().logged_in();
    let mut s = Streaming::spawn(&h, &["--json", "--gain", "0.5"]);

    s.lines
        .until_json("streaming state", |v| is_state(v, "streaming"));
    let streaming_since = Instant::now();
    let health = s
        .lines
        .until_json("health event", |v| v["event"] == "health");
    assert!(health["stats"]["video_frames_sent"].as_u64().unwrap() > 0);
    std::thread::sleep(Duration::from_secs(3).saturating_sub(streaming_since.elapsed()));

    let video = h.mock.recorded_video().len();
    assert!(video >= 15, "only {video} video frames reached the mock");
    assert!(!h.mock.recorded_audio().is_empty());
    assert_eq!(h.mock.streams().len(), 1);

    let (status, stderr, rest) = s.interrupt();
    assert!(status.success(), "{status}: {stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
    let last: Value = serde_json::from_str(rest.last().expect("no final state")).unwrap();
    assert!(is_state(&last, "stopped"), "{rest:?}");
    assert_eq!(last["state"]["reason"]["reason"], "requested");

    let deleted =
        h.rt.block_on(h.mock.wait_for_op(None, gw::OP_STREAM_DELETE, 0, T));
    assert!(deleted.is_some(), "no STREAM_DELETE");
}

#[cfg(unix)]
#[test]
fn stream_prints_text_by_default() {
    let h = Harness::new().logged_in();
    let mut s = Streaming::spawn(&h, &[]);
    s.lines
        .until("streaming state", |l| l == "state: streaming");
    s.lines.until("health line", |l| {
        l.contains(" fps  ") && l.contains("video ")
    });
    let (status, stderr, rest) = s.interrupt();
    assert!(status.success(), "{status}: {stderr}");
    assert!(stderr.contains("stopping"), "{stderr}");
    assert_eq!(
        rest.last().map(String::as_str),
        Some("state: stopped: requested")
    );
}

#[test]
fn a_second_interrupt_cuts_the_shutdown_short() {
    #[derive(clap::Parser)]
    struct Cmd {
        #[command(flatten)]
        args: StreamArgs,
    }
    let h = Harness::new().logged_in();
    let ctx = Ctx {
        paths: ConfigPaths::in_dir(h.config_dir()),
        gateway_url: h.mock.gateway_url(),
    };
    let args = Cmd::parse_from(stream_args(MOCK_VOICE_CHANNEL_ID)).args;
    let (interrupts, rx) = tokio::sync::mpsc::unbounded_channel();
    let code = h.rt.block_on(async {
        let run = tokio::spawn(async move {
            stream::run_with(&ctx, &Out::new(true), args, || Signals::from_channel(rx)).await
        });
        let video = h
            .mock
            .wait_for(T, |r| (!r.video.is_empty()).then_some(()))
            .await;
        assert!(video.is_some(), "no video reached the mock");
        interrupts.send(()).unwrap();
        interrupts.send(()).unwrap();
        tokio::time::timeout(T, run).await
    });
    let code = code.expect("the stream did not end").unwrap().unwrap();
    assert_eq!(code, std::process::ExitCode::from(stream::FORCED_EXIT));
}

#[test]
fn stream_exits_with_failure_when_the_session_stops_by_itself() {
    let h = Harness::new().logged_in();
    let mut args = stream_args(MOCK_VOICE_CHANNEL_ID);
    args.extend(["--codec", "h265"]);
    let output = h.cmd().arg("--json").args(&args).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("stream stopped"), "{stderr}");
    assert!(
        stderr.contains("warning: H.265 is experimental"),
        "{stderr}"
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let last: Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
    assert!(is_state(&last, "stopped"), "{stdout}");
    assert_eq!(last["state"]["reason"]["reason"], "source_failed");
}

#[test]
fn stream_rejects_an_unknown_guild_after_one_identify() {
    let h = Harness::new().logged_in();
    let mut args = stream_args(MOCK_VOICE_CHANNEL_ID);
    args[2] = "999";
    let identifies = || {
        h.mock
            .received_ops()
            .iter()
            .filter(|op| op.is(None, gw::OP_IDENTIFY))
            .count()
    };
    let before = identifies();
    let err = stderr_of_failure(h.cmd().args(&args));
    assert!(err.contains("unknown guild 999"), "{err}");
    assert_eq!(identifies(), before + 1);
    assert!(h.mock.streams().is_empty());
}
