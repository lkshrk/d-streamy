use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{ExitCode, Stdio};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use dstreamy_cli::Ctx;
use dstreamy_cli::tui::model::Key;
use dstreamy_cli::tui::{Input, Options, drive};
use dstreamy_core::{Codec, Config, ConfigPaths, Fps, Resolution, StreamConfig};
use dstreamy_mock_discord::opcodes::gateway as gw;
use dstreamy_mock_discord::{MOCK_GUILD_ID, MOCK_TOKEN, MOCK_VOICE_CHANNEL_ID, MockDiscord};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use tempfile::TempDir;
use tokio::sync::{mpsc, watch};
use tokio::time::timeout;

const T: Duration = Duration::from_secs(20);

struct Harness {
    mock: MockDiscord,
    _dir: TempDir,
    ctx: Ctx,
}

impl Harness {
    async fn new() -> Self {
        Self::with(MOCK_TOKEN, Codec::H264).await
    }

    async fn with(token: &str, codec: Codec) -> Self {
        let mock = MockDiscord::start().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::in_dir(dir.path());
        paths.save_token(token).unwrap();
        Config {
            stream: StreamConfig {
                resolution: Resolution::new(320, 180).unwrap(),
                fps: Fps::Fps15,
                bitrate_kbps: 1_000,
                codec,
                ..StreamConfig::default()
            },
            ..Config::default()
        }
        .save(&paths.config_file)
        .unwrap();
        let ctx = Ctx {
            paths,
            gateway_url: mock.gateway_url(),
        };
        Self {
            mock,
            _dir: dir,
            ctx,
        }
    }

    fn config(&self) -> Config {
        Config::load(&self.ctx.paths.config_file).unwrap()
    }

    /// Runs `dstreamy stream` on the same config dir until it streams (`Ok`) or exits (`Err` with stderr).
    async fn external_stream(&self) -> Result<(), String> {
        let dir = self.ctx.paths.config_file.parent().unwrap().to_owned();
        let url = self.ctx.gateway_url.clone();
        tokio::task::spawn_blocking(move || external_stream(&dir, &url))
            .await
            .unwrap()
    }
}

fn external_stream(dir: &Path, gateway_url: &str) -> Result<(), String> {
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("dstreamy"))
        .env_remove("DSTREAMY_TOKEN")
        .env_remove("DSTREAMY_CONFIG_DIR")
        .arg("--config-dir")
        .arg(dir)
        .args([
            "--gateway-url",
            gateway_url,
            "stream",
            "--guild",
            MOCK_GUILD_ID,
        ])
        .args([
            "--channel",
            MOCK_VOICE_CHANNEL_ID,
            "--target",
            "test-pattern",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = std_mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + T;
    loop {
        if let Ok(line) = lines.recv_timeout(Duration::from_millis(50))
            && line == "state: streaming"
        {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(());
        }
        if child.try_wait().unwrap().is_some() {
            let mut stderr = String::new();
            let _ = child.stderr.take().unwrap().read_to_string(&mut stderr);
            return Err(stderr);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the external stream neither streamed nor exited");
        }
    }
}

struct Script {
    keys: mpsc::UnboundedSender<Input>,
    screen: watch::Receiver<String>,
}

impl Script {
    async fn until(&mut self, what: &str) {
        let found = timeout(T, self.screen.wait_for(|s| s.contains(what)))
            .await
            .is_ok_and(|r| r.is_ok());
        if !found {
            panic!(
                "{what:?} never drawn; last screen:\n{}",
                *self.screen.borrow()
            );
        }
    }

    fn press(&self, keys: &[Key]) {
        for key in keys {
            self.keys.send(Input::Key(*key)).unwrap();
        }
    }
}

async fn run_tui<F: Future<Output = ()>>(
    h: &Harness,
    script: impl FnOnce(Script) -> F,
) -> ExitCode {
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let (keys, input) = mpsc::unbounded_channel();
    let (screen_tx, screen) = watch::channel(String::new());
    let opts = Options {
        include_platform: false,
    };
    let tui = drive(&mut terminal, &h.ctx, input, opts, None, |t| {
        screen_tx.send_replace(t.backend().to_string());
    });
    let (result, ()) = timeout(T * 3, async {
        tokio::join!(tui, script(Script { keys, screen }))
    })
    .await
    .expect("the TUI did not finish");
    result.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keys_drive_a_test_pattern_stream_from_picker_to_stop() {
    let h = Harness::new().await;
    let mock = &h.mock;
    let code = run_tui(&h, |mut s| async move {
        s.until("Mock Guild").await;
        s.press(&[Key::Enter]);
        s.until("Voice channels in Mock Guild").await;
        s.until("General").await;
        s.press(&[Key::Enter]);
        s.until("Capture target").await;
        s.until("Test pattern").await;
        s.press(&[Key::Enter]);
        s.until("320x180").await;
        s.press(&[Key::Down, Key::Down, Key::Right]);
        s.until("1500 kbps").await;
        s.press(&[Key::Enter]);
        s.until("FPS, last").await;
        s.until("streaming  ·  Mock Guild › General  ·  Test pattern")
            .await;

        let frames = mock
            .wait_for(T, |r| {
                (r.video.len() >= 5 && !r.audio.is_empty()).then_some(())
            })
            .await;
        assert!(frames.is_some(), "no media reached the mock");
        assert_eq!(mock.streams().len(), 1);

        s.press(&[Key::Char('s')]);
        s.until("stream stopped").await;
        s.until("Stream settings").await;
        let deleted = mock.wait_for_op(None, gw::OP_STREAM_DELETE, 0, T).await;
        assert!(deleted.is_some(), "no STREAM_DELETE");

        s.press(&[Key::Char('q')]);
    })
    .await;
    assert_eq!(code, ExitCode::SUCCESS);

    let config = h.config();
    assert_eq!(config.stream.bitrate_kbps, 1_500);
    assert_eq!(config.stream.resolution, Resolution::new(320, 180).unwrap());
    assert_eq!(config.stream.fps, Fps::Fps15);
    assert_eq!(config.target, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quitting_while_live_stops_the_stream() {
    let h = Harness::new().await;
    let code = run_tui(&h, |mut s| async move {
        s.until("Mock Guild").await;
        s.press(&[Key::Enter]);
        s.until("General").await;
        s.press(&[Key::Enter]);
        s.until("Test pattern").await;
        s.press(&[Key::Enter]);
        s.until("Stream settings").await;
        s.press(&[Key::Enter]);
        s.until("streaming  ·").await;
        s.press(&[Key::Interrupt]);
    })
    .await;
    assert_eq!(code, ExitCode::SUCCESS);
    let deleted = h.mock.wait_for_op(None, gw::OP_STREAM_DELETE, 0, T).await;
    assert!(deleted.is_some(), "no STREAM_DELETE after quitting");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_input_quits() {
    let h = Harness::new().await;
    let code = run_tui(&h, |mut s| async move {
        s.until("Mock Guild").await;
        drop(s.keys);
    })
    .await;
    assert_eq!(code, ExitCode::SUCCESS);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_connection_exits_with_failure() {
    let h = Harness::with("wrong-token", Codec::H264).await;
    let code = run_tui(&h, |mut s| async move {
        s.until("connecting to Discord failed").await;
        s.press(&[Key::Char('q')]);
    })
    .await;
    assert_eq!(code, ExitCode::FAILURE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stream_that_stops_by_itself_exits_with_failure() {
    let h = Harness::with(MOCK_TOKEN, Codec::H265).await;
    let code = run_tui(&h, |mut s| async move {
        s.until("Mock Guild").await;
        s.press(&[Key::Enter]);
        s.until("General").await;
        s.press(&[Key::Enter]);
        s.until("Test pattern").await;
        s.press(&[Key::Enter]);
        s.until("Stream settings").await;
        s.press(&[Key::Enter]);
        s.until("stream stopped:").await;
        s.press(&[Key::Char('q')]);
    })
    .await;
    assert_eq!(code, ExitCode::FAILURE);
}

#[test]
fn the_binary_refuses_to_start_without_a_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let output = assert_cmd::Command::cargo_bin("dstreamy")
        .unwrap()
        .env_remove("DSTREAMY_TOKEN")
        .arg("--config-dir")
        .arg(dir.path())
        .arg("tui")
        .timeout(T)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("interactive terminal"), "{stderr}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_tui_session_holds_the_lock_until_it_stops() {
    let h = Harness::new().await;
    let hr = &h;
    let code = run_tui(&h, |mut s| async move {
        s.until("Mock Guild").await;
        s.press(&[Key::Enter]);
        s.until("General").await;
        s.press(&[Key::Enter]);
        s.until("Test pattern").await;
        s.press(&[Key::Enter]);
        s.until("Stream settings").await;
        s.press(&[Key::Enter]);
        s.until("streaming  ·").await;

        let refused = hr.external_stream().await.unwrap_err();
        assert!(
            refused.contains(&format!(
                "another dstreamy session is live (pid {})",
                std::process::id()
            )),
            "{refused}"
        );

        s.press(&[Key::Char('s')]);
        s.until("stream stopped").await;
        hr.external_stream().await.unwrap();
        s.press(&[Key::Char('q')]);
    })
    .await;
    assert_eq!(code, ExitCode::SUCCESS);
}
