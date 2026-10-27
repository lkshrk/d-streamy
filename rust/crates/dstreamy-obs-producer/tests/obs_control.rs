//! The dock's control logic against a real `dstreamy serve` from `DSTREAMY_BIN`, with mock Discord in process.
#![cfg(unix)]

use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use dstreamy_core::ConfigPaths;
use dstreamy_mock_discord::{
    MOCK_GUILD_ID, MOCK_OTHER_VOICE_CHANNEL_ID, MOCK_TOKEN, MOCK_VOICE_CHANNEL_ID, MockChannel,
    MockConfig, MockDiscord, MockGuild,
};
use dstreamy_obs_producer::Endpoint;
use dstreamy_obs_producer::control::{
    ControlError, Instance, Named, ObsControl, Ownership, StartFailed, StartRequest, Started,
    Stopped,
};
use dstreamy_obs_producer::dock::{DockModel, DockSettings};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

const T: Duration = Duration::from_secs(20);
const SECOND_GUILD_ID: &str = "200000000000000002";
const SECOND_VOICE_CHANNEL_ID: &str = "300000000000000011";
const BIN_HINT: &str = "point DSTREAMY_BIN at a test-hooks build of serve: `cargo build -p dstreamy-cli --features test-hooks` (target/debug/dstreamy); a plain build exits 2 on --skip-platform";

/// `None` skips the test locally; under `CI=true` a missing binary is a failure.
fn dstreamy_bin() -> Option<PathBuf> {
    match std::env::var_os("DSTREAMY_BIN") {
        Some(bin) => Some(PathBuf::from(bin)),
        None if std::env::var("CI").is_ok_and(|v| v == "true") => {
            panic!("DSTREAMY_BIN is unset: {BIN_HINT}")
        }
        None => {
            eprintln!("skipped: {BIN_HINT}");
            None
        }
    }
}

fn two_guilds() -> MockConfig {
    let mut config = MockConfig::default();
    config.guilds.push(MockGuild {
        id: SECOND_GUILD_ID.into(),
        name: "Second Guild".into(),
        channels: vec![
            MockChannel::text("300000000000000012", "chat"),
            MockChannel::voice(SECOND_VOICE_CHANNEL_ID, "Lounge"),
        ],
        in_ready: true,
    });
    config
}

async fn spawn(bin: &Path, config: &Path, mock: &MockDiscord, run: &Path) -> Child {
    let endpoint = run.join("d").join("s.sock");
    let mut child = Command::new(bin)
        .env_remove("DSTREAMY_TOKEN")
        .env_remove("DSTREAMY_CONFIG_DIR")
        .arg("--config-dir")
        .arg(config)
        .arg("--gateway-url")
        .arg(mock.gateway_url())
        .args(["serve", "--skip-platform", "--endpoint"])
        .arg(&endpoint)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + T;
    while StdUnixStream::connect(&endpoint).is_err() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("serve exited with {status} before listening; {BIN_HINT}");
        }
        assert!(Instant::now() < deadline, "serve never listened");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    child
}

struct Serve {
    bin: PathBuf,
    mock: MockDiscord,
    config: TempDir,
    run: TempDir,
    child: Child,
}

impl Serve {
    async fn start(bin: &Path, logged_in: bool) -> Self {
        let mock = MockDiscord::start_with(two_guilds()).await.unwrap();
        let config = tempfile::tempdir().unwrap();
        if logged_in {
            ConfigPaths::in_dir(config.path())
                .save_token(MOCK_TOKEN)
                .unwrap();
        }
        std::fs::write(config.path().join("config.toml"), "").unwrap();
        let run = tempfile::Builder::new().prefix("ds").tempdir().unwrap();
        let child = spawn(bin, config.path(), &mock, run.path()).await;
        Self {
            bin: bin.to_owned(),
            mock,
            config,
            run,
            child,
        }
    }

    /// Kills serve and starts another on the same endpoint and config.
    async fn restart(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        self.child = spawn(&self.bin, self.config.path(), &self.mock, self.run.path()).await;
    }

    fn endpoint(&self) -> Endpoint {
        Endpoint::new(self.run.path().join("d").join("s.sock"))
    }

    fn control(&self) -> ObsControl {
        ObsControl::new(self.endpoint())
    }

    /// Another control client's connection, kept open like a CLI or app would.
    async fn client(&self) -> Client {
        let stream = UnixStream::connect(self.endpoint().as_path())
            .await
            .unwrap();
        let (read, write) = stream.into_split();
        let mut c = Client {
            lines: BufReader::new(read),
            write,
        };
        let hello = c
            .call(json!({"id": "h", "cmd": "hello", "role": "control", "protocol": 2}))
            .await;
        assert_eq!(hello["type"], "result", "{hello}");
        c
    }

    async fn voice_channel(&self, channel_id: Option<&str>) {
        let deadline = Instant::now() + T;
        loop {
            let now = self.mock.voice_state().map(|v| v.channel_id);
            if now.as_deref() == channel_id {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "voice stayed in {now:?}, not {channel_id:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Client {
    lines: BufReader<tokio::net::unix::OwnedReadHalf>,
    write: tokio::net::unix::OwnedWriteHalf,
}

impl Client {
    async fn call(&mut self, command: Value) -> Value {
        let id = command["id"].clone();
        self.write
            .write_all(format!("{command}\n").as_bytes())
            .await
            .unwrap();
        tokio::time::timeout(T, async {
            loop {
                let mut line = String::new();
                assert_ne!(self.lines.read_line(&mut line).await.unwrap(), 0, "closed");
                let reply: Value = serde_json::from_str(&line).unwrap();
                if reply["id"] == id {
                    return reply;
                }
            }
        })
        .await
        .expect("a reply")
    }

    async fn start_external(&mut self, channel_id: &str) {
        let reply = self
            .call(json!({
                "id": "s", "cmd": "start", "guild_id": MOCK_GUILD_ID, "channel_id": channel_id,
                "target": "external", "resolution": "1280x720", "fps": 30,
            }))
            .await;
        assert_eq!(reply["type"], "result", "{reply}");
    }

    async fn start_test_pattern(&mut self, channel_id: &str) {
        let reply = self
            .call(json!({
                "id": "s", "cmd": "start", "guild_id": MOCK_GUILD_ID, "channel_id": channel_id,
                "target": "test-pattern", "resolution": "320x180", "fps": 15,
            }))
            .await;
        assert_eq!(reply["type"], "result", "{reply}");
    }

    async fn stop(&mut self) {
        let reply = self.call(json!({"id": "x", "cmd": "stop"})).await;
        assert_eq!(reply["type"], "result", "{reply}");
    }
}

fn request(channel_id: &str) -> StartRequest {
    StartRequest {
        guild_id: MOCK_GUILD_ID.into(),
        channel_id: channel_id.into(),
        width: 1280,
        height: 720,
        fps: 30,
        grace_secs: None,
    }
}

fn named(id: &str, name: &str) -> Named {
    Named {
        id: id.into(),
        name: name.into(),
    }
}

macro_rules! serve {
    ($logged_in:expr) => {{
        let Some(bin) = dstreamy_bin() else { return };
        Serve::start(&bin, $logged_in).await
    }};
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_dock_lists_guilds_and_follows_a_guild_change() {
    let serve = serve!(true);
    let control = serve.control();
    let mut dock = DockModel::new(DockSettings::default());
    assert_eq!(dock.guilds_loaded(control.guilds().await), None);
    assert_eq!(
        dock.guilds(),
        [
            named(MOCK_GUILD_ID, "Mock Guild"),
            named(SECOND_GUILD_ID, "Second Guild")
        ]
    );

    let guild = dock.select_guild(MOCK_GUILD_ID).unwrap();
    dock.channels_loaded(&guild, control.channels(&guild).await);
    assert_eq!(
        dock.channels(),
        [
            named(MOCK_VOICE_CHANNEL_ID, "General"),
            named(MOCK_OTHER_VOICE_CHANNEL_ID, "Gaming")
        ]
    );
    dock.select_channel(MOCK_OTHER_VOICE_CHANNEL_ID);

    let guild = dock.select_guild(SECOND_GUILD_ID).unwrap();
    assert_eq!(dock.settings().channel_id, None);
    dock.channels_loaded(&guild, control.channels(&guild).await);
    assert_eq!(dock.channels(), [named(SECOND_VOICE_CHANNEL_ID, "Lounge")]);

    assert_eq!(
        control.channels("299999999999999999").await,
        Err(ControlError::UnknownGuild)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_from_obs_owns_the_session_and_its_stop_ends_it() {
    let serve = serve!(true);
    let control = serve.control();
    let started = control
        .start(&request(MOCK_VOICE_CHANNEL_ID), None)
        .await
        .unwrap();
    assert_eq!(started.channel, "General");
    assert_eq!(started.session_fps(), Some(30));
    let ownership = started.ownership.clone().expect("owned");
    assert_eq!(ownership.channel_id, MOCK_VOICE_CHANNEL_ID);
    serve.voice_channel(Some(MOCK_VOICE_CHANNEL_ID)).await;
    let live = control.status().await.unwrap().expect("live");
    assert_eq!(live.started_by, Some(ownership.connections[0]));
    assert_eq!(live.target, "external");

    assert_eq!(control.stop(Some(&ownership)).await, Ok(Stopped::Stopped));
    assert_eq!(control.status().await, Ok(None));
    serve.voice_channel(None).await;
    assert_eq!(
        control.stop(Some(&ownership)).await,
        Ok(Stopped::NotRunning)
    );
}

async fn moved_to(serve: &Serve, control: &ObsControl, guild_id: &str, channel_id: &str) {
    assert!(serve.mock.move_to_channel(guild_id, channel_id));
    let deadline = Instant::now() + T;
    loop {
        let live = control.status().await.unwrap().unwrap();
        if live.guild_id == guild_id && live.channel_id == channel_id {
            return;
        }
        assert!(Instant::now() < deadline, "status never showed the move");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_moved_in_discord_is_still_ours_after_a_relaunch() {
    let serve = serve!(true);
    let control = serve.control();
    let ownership = control
        .start(&request(MOCK_VOICE_CHANNEL_ID), None)
        .await
        .unwrap()
        .ownership
        .unwrap();
    serve.voice_channel(Some(MOCK_VOICE_CHANNEL_ID)).await;
    moved_to(&serve, &control, MOCK_GUILD_ID, MOCK_OTHER_VOICE_CHANNEL_ID).await;
    let err = control
        .start(&request(MOCK_VOICE_CHANNEL_ID), None)
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "D-Streamy is already live in Gaming");
    let started = control
        .start(&request(MOCK_VOICE_CHANNEL_ID), Some(&ownership))
        .await
        .unwrap();
    assert_eq!(
        started,
        Started {
            ownership: Some(ownership.clone()),
            channel: "Gaming".into()
        }
    );
    assert_eq!(control.stop(Some(&ownership)).await, Ok(Stopped::Stopped));
    serve.voice_channel(None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_moved_to_another_guild_is_still_stopped_by_its_starter() {
    let serve = serve!(true);
    let control = serve.control();
    let ownership = control
        .start(&request(MOCK_VOICE_CHANNEL_ID), None)
        .await
        .unwrap()
        .ownership
        .unwrap();
    serve.voice_channel(Some(MOCK_VOICE_CHANNEL_ID)).await;
    moved_to(&serve, &control, SECOND_GUILD_ID, SECOND_VOICE_CHANNEL_ID).await;
    assert_eq!(control.stop(Some(&ownership)).await, Ok(Stopped::Stopped));
    serve.voice_channel(None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_whose_reply_is_lost_is_still_stopped_by_its_starter() {
    let mut serve = serve!(true);
    let provisional = lost_start(&mut serve).await;
    let control = serve.control();
    let deadline = Instant::now() + T;
    loop {
        let live = control.status().await.unwrap();
        if live.as_ref().is_some_and(|l| l.started_by.is_some()) {
            assert_eq!(live.unwrap().started_by, Some(provisional.connections[0]));
            break;
        }
        assert!(Instant::now() < deadline, "the start never landed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(control.stop(Some(&provisional)).await, Ok(Stopped::Stopped));
    serve.voice_channel(None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retry_after_a_lost_start_reply_keeps_the_ownership() {
    let mut serve = serve!(true);
    // The retry meets the first start queued, in flight or done, depending on timing; each round takes one.
    for round in 0..5 {
        let provisional = lost_start(&mut serve).await;
        let control = serve.control();
        let started = control
            .start(&request(MOCK_VOICE_CHANNEL_ID), Some(&provisional))
            .await
            .unwrap();
        assert_eq!(
            started.ownership,
            Some(provisional.clone()),
            "round {round}"
        );
        assert_eq!(
            control.stop(Some(&provisional)).await,
            Ok(Stopped::Stopped),
            "round {round}"
        );
        serve.voice_channel(None).await;
        serve.restart().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_lost_starts_in_a_row_still_own_the_session() {
    let mut serve = serve!(true);
    let mut second_lost = 0;
    for round in 0..8 {
        let first = lost_start_without_reading(&serve).await;
        let second = serve
            .control()
            .with_start_timeout(Duration::from_millis(1))
            .start(&request(MOCK_VOICE_CHANNEL_ID), Some(&first))
            .await;
        let ownership = match second {
            Err(failed) => {
                second_lost += 1;
                *failed.ownership.expect("a provisional ownership")
            }
            Ok(started) => started.ownership.expect("our own session"),
        };
        // A stop now could still run before either start reaches serve, so the dock's next step is another start.
        let third = serve
            .control()
            .start(&request(MOCK_VOICE_CHANNEL_ID), Some(&ownership))
            .await
            .unwrap();
        let confirmed = third.ownership.expect("still ours");
        assert_eq!(confirmed.connections.len(), 1, "round {round}");
        assert_eq!(
            serve.control().stop(Some(&confirmed)).await,
            Ok(Stopped::Stopped),
            "round {round}"
        );
        serve.voice_channel(None).await;
        serve.restart().await;
    }
    assert!(
        second_lost > 0,
        "the second start was never lost behind the first"
    );
}

/// Sends a start and hangs up without its reply, as a timed-out start does, and returns the ownership it left.
async fn lost_start_without_reading(serve: &Serve) -> Ownership {
    let mut client = serve.client().await;
    let hello = client
        .call(json!({"id": "h2", "cmd": "hello", "role": "control", "protocol": 2}))
        .await;
    let meta = std::fs::metadata(serve.endpoint().as_path()).unwrap();
    client
        .write
        .write_all(format!("{}\n", request(MOCK_VOICE_CHANNEL_ID).command("lost")).as_bytes())
        .await
        .unwrap();
    drop(client);
    Ownership {
        connections: vec![hello["data"]["connection"].as_u64().unwrap()],
        guild_id: MOCK_GUILD_ID.into(),
        channel_id: MOCK_VOICE_CHANNEL_ID.into(),
        instance: Some(Instance {
            dev: meta.dev(),
            ino: meta.ino(),
            mtime: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
        }),
        fps: Some(30),
    }
}

/// A start whose reply times out after it was sent; a serve fast enough to answer within 1 ms is restarted and asked again.
async fn lost_start(serve: &mut Serve) -> Ownership {
    for _ in 0..5 {
        let control = serve.control().with_start_timeout(Duration::from_millis(1));
        match control.start(&request(MOCK_VOICE_CHANNEL_ID), None).await {
            Err(failed) => {
                assert!(
                    matches!(
                        failed.error,
                        ControlError::Timeout {
                            command: "start",
                            ..
                        }
                    ),
                    "{failed:?}"
                );
                return *failed.ownership.expect("a provisional ownership");
            }
            Ok(started) => {
                let stopped = serve.control().stop(started.ownership.as_ref()).await;
                assert_eq!(stopped, Ok(Stopped::Stopped));
                serve.voice_channel(None).await;
                serve.restart().await;
            }
        }
    }
    panic!("serve answered every start within 1 ms");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_after_serve_died_clears_the_session() {
    let mut serve = serve!(true);
    let control = serve.control();
    let ownership = control
        .start(&request(MOCK_VOICE_CHANNEL_ID), None)
        .await
        .unwrap()
        .ownership
        .unwrap();
    serve.child.kill().unwrap();
    serve.child.wait().unwrap();
    assert_eq!(
        control.stop(Some(&ownership)).await,
        Ok(Stopped::NotRunning)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_another_client_started_is_attached_and_left_running() {
    let serve = serve!(true);
    let mut client = serve.client().await;
    client.start_external(MOCK_VOICE_CHANNEL_ID).await;
    let control = serve.control();
    let started = control
        .start(&request(MOCK_VOICE_CHANNEL_ID), None)
        .await
        .unwrap();
    assert_eq!(
        started,
        Started {
            ownership: None,
            channel: "General".into()
        }
    );
    assert_eq!(control.stop(None).await, Ok(Stopped::LeftRunning));
    let foreign = Ownership {
        connections: vec![999],
        guild_id: MOCK_GUILD_ID.into(),
        channel_id: MOCK_VOICE_CHANNEL_ID.into(),
        instance: None,
        fps: None,
    };
    assert_eq!(control.stop(Some(&foreign)).await, Ok(Stopped::LeftRunning));
    assert!(control.status().await.unwrap().is_some());
    client.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_in_another_channel_is_named() {
    let serve = serve!(true);
    let mut client = serve.client().await;
    client.start_external(MOCK_OTHER_VOICE_CHANNEL_ID).await;
    let err = serve
        .control()
        .start(&request(MOCK_VOICE_CHANNEL_ID), None)
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "D-Streamy is already live in Gaming");
    client.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_capture_session_in_the_same_channel_is_refused() {
    let serve = serve!(true);
    let mut client = serve.client().await;
    client.start_test_pattern(MOCK_VOICE_CHANNEL_ID).await;
    let err = serve
        .control()
        .start(&request(MOCK_VOICE_CHANNEL_ID), None)
        .await
        .unwrap_err();
    assert_eq!(
        err.error,
        ControlError::OtherSource {
            channel: "General".into()
        }
    );
    client.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ownership_persisted_across_a_relaunch_still_stops_the_session() {
    let serve = serve!(true);
    let ownership = serve
        .control()
        .start(&request(MOCK_VOICE_CHANNEL_ID), None)
        .await
        .unwrap()
        .ownership
        .unwrap();
    let saved = DockSettings {
        owned: Some(ownership.clone()),
        ..DockSettings::default()
    }
    .to_json();

    let relaunched = DockSettings::from_json(&saved);
    let control = serve.control();
    let started = control
        .start(&request(MOCK_VOICE_CHANNEL_ID), relaunched.owned.as_ref())
        .await
        .unwrap();
    assert_eq!(started.ownership, Some(ownership.clone()));
    assert_eq!(
        started.session_fps(),
        Some(30),
        "the fps survives the relaunch"
    );
    assert_eq!(
        control.stop(started.ownership.as_ref()).await,
        Ok(Stopped::Stopped)
    );
    serve.voice_channel(None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_ownership_never_stops_a_newer_session() {
    let serve = serve!(true);
    let control = serve.control();
    let old = control
        .start(&request(MOCK_VOICE_CHANNEL_ID), None)
        .await
        .unwrap()
        .ownership
        .unwrap();
    let mut client = serve.client().await;
    client.stop().await;
    client.start_external(MOCK_VOICE_CHANNEL_ID).await;

    let started = control
        .start(&request(MOCK_VOICE_CHANNEL_ID), Some(&old))
        .await
        .unwrap();
    assert_eq!(started.ownership, None);
    assert_eq!(control.stop(Some(&old)).await, Ok(Stopped::LeftRunning));
    assert!(control.status().await.unwrap().is_some());
    client.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ownership_from_before_a_serve_restart_is_ignored() {
    let mut serve = serve!(true);
    let old = serve
        .control()
        .start(&request(MOCK_VOICE_CHANNEL_ID), None)
        .await
        .unwrap()
        .ownership
        .unwrap();
    serve.restart().await;
    let mut client = serve.client().await;
    client.start_external(MOCK_VOICE_CHANNEL_ID).await;
    let control = serve.control();
    let live = control.status().await.unwrap().expect("live");
    assert_eq!(
        live.started_by,
        Some(old.connections[0]),
        "the new serve reuses the connection id, which is what the instance guards against"
    );
    let started = control
        .start(&request(MOCK_VOICE_CHANNEL_ID), Some(&old))
        .await
        .unwrap();
    assert_eq!(started.ownership, None);
    assert_eq!(control.stop(Some(&old)).await, Ok(Stopped::LeftRunning));
    assert!(control.status().await.unwrap().is_some());
    client.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_grace_override_is_sent_with_the_start() {
    let serve = serve!(true);
    let control = serve.control();
    let mut r = request(MOCK_VOICE_CHANNEL_ID);
    r.grace_secs = Some(3601);
    assert!(matches!(
        control.start(&r, None).await,
        Err(StartFailed { error: ControlError::Refused { code, .. }, .. }) if code == "invalid_params"
    ));
    r.grace_secs = Some(120);
    let started = control.start(&r, None).await.unwrap();
    assert!(started.ownership.is_some());
    assert_eq!(
        control.stop(started.ownership.as_ref()).await,
        Ok(Stopped::Stopped)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn not_logged_in_tells_the_user_to_log_in() {
    let serve = serve!(false);
    let control = serve.control();
    let err = control.guilds().await.unwrap_err();
    assert_eq!(err.to_string(), "Log in first: run `dstreamy login`");
    let err = control
        .start(&request(MOCK_VOICE_CHANNEL_ID), None)
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "Log in first: run `dstreamy login`");
}

#[tokio::test]
async fn no_service_is_reported_as_not_running() {
    let run = tempfile::Builder::new().prefix("ds").tempdir().unwrap();
    let control = ObsControl::new(Endpoint::new(run.path().join("d").join("s.sock")));
    let err = control.guilds().await.unwrap_err();
    assert!(matches!(err, ControlError::NotRunning(_)), "{err:?}");
    assert!(
        err.to_string()
            .starts_with("The D-Streamy service is not running"),
        "{err}"
    );
}
