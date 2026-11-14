//! Real OBS on Windows with the plugin installed as a user would: it starts serve, streams and ends serve on exit.
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use dstreamy_core::ConfigPaths;
use dstreamy_ipc::{Endpoint, IpcError};
use dstreamy_mock_discord::{MOCK_GUILD_ID, MOCK_TOKEN, MOCK_VOICE_CHANNEL_ID, MockDiscord};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const WAIT: Duration = Duration::from_secs(120);

fn var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is unset; obs-ci sets it"))
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

struct Obs {
    child: Child,
    rt: tokio::runtime::Runtime,
    endpoint: Endpoint,
    config: PathBuf,
    serve_log: PathBuf,
    press_start: PathBuf,
    press_stop: PathBuf,
    _dir: tempfile::TempDir,
}

impl Obs {
    /// Installs the staged plugin and a test `dstreamy.exe` for this user and starts OBS with them.
    fn launch(token: &str, guild: &str, channel: &str, gateway: Option<String>) -> Self {
        let obs_exe = PathBuf::from(var("DSTREAMY_OBS_FRONTEND"));
        let stage = PathBuf::from(var("DSTREAMY_OBS_PLUGIN_STAGE"));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let endpoint = Endpoint::default_for_user().unwrap();
        assert!(
            matches!(
                rt.block_on(dstreamy_ipc::connect(&endpoint)).map(drop),
                Err(IpcError::NotRunning(_))
            ),
            "a serve already runs for this user"
        );

        let dir = tempfile::tempdir().unwrap();
        let dstreamy_config = dir.path().join("dstreamy");
        std::fs::create_dir_all(&dstreamy_config).unwrap();
        ConfigPaths::in_dir(&dstreamy_config)
            .save_token(token)
            .unwrap();
        std::fs::write(dstreamy_config.join("config.toml"), "").unwrap();

        let plugin = PathBuf::from(std::env::var_os("ProgramData").unwrap())
            .join("obs-studio")
            .join("plugins")
            .join("obs-dstreamy");
        let _ = std::fs::remove_dir_all(&plugin);
        copy_dir(&stage, &plugin);
        std::fs::copy(
            assert_cmd::cargo::cargo_bin("dstreamy"),
            plugin.join("bin").join("64bit").join("dstreamy.exe"),
        )
        .unwrap();

        let config = PathBuf::from(std::env::var_os("APPDATA").unwrap()).join("obs-studio");
        let _ = std::fs::remove_dir_all(&config);
        std::fs::create_dir_all(&config).unwrap();
        // As the Linux frontend test: no first-run wizard, no update check.
        std::fs::write(
            config.join("global.ini"),
            "[General]\r\nLastVersion=537001986\r\nPre31Migrated=true\r\nEnableAutoUpdates=false\r\nInfoIncrement=99999\r\n",
        )
        .unwrap();
        std::fs::write(
            config.join("user.ini"),
            "[General]\r\nFirstRun=true\r\nPre19Defaults=false\r\nPre21Defaults=false\r\nPre23Defaults=false\r\nPre24.1Defaults=false\r\n",
        )
        .unwrap();
        let module_config = config.join("plugin_config").join("obs-dstreamy");
        std::fs::create_dir_all(&module_config).unwrap();
        std::fs::write(
            module_config.join("config.json"),
            json!({"guild_id": guild, "channel_id": channel, "audio_track": 2}).to_string(),
        )
        .unwrap();

        let press_start = dir.path().join("press-start");
        let press_stop = dir.path().join("press-stop");
        let mut command = Command::new(&obs_exe);
        command
            .args(["--multi", "--disable-updater", "--verbose"])
            .env("DSTREAMY_CONFIG_DIR", &dstreamy_config)
            .env("DSTREAMY_TEST_PRESS_START", &press_start)
            .env("DSTREAMY_TEST_PRESS_STOP", &press_stop)
            .env_remove("DSTREAMY_TOKEN")
            .env_remove("DSTREAMY_GATEWAY_URL")
            .current_dir(obs_exe.parent().unwrap())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(gateway) = gateway {
            command.env("DSTREAMY_GATEWAY_URL", gateway);
        }
        let child = command
            .spawn()
            .unwrap_or_else(|e| panic!("{}: {e}", obs_exe.display()));
        let obs = Self {
            child,
            rt,
            endpoint,
            config,
            serve_log: dstreamy_config.join("dstreamy.log"),
            press_start,
            press_stop,
            _dir: dir,
        };
        obs.wait_for("its startup with the launcher", |l| {
            l.contains("==== Startup complete")
                && l.contains("dstreamy.exe when the D-Streamy service is not running")
        });
        obs
    }

    fn log(&self) -> String {
        let newest = std::fs::read_dir(self.config.join("logs"))
            .ok()
            .and_then(|entries| {
                entries
                    .filter_map(Result::ok)
                    .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
            });
        let mut text = newest
            .and_then(|e| std::fs::read_to_string(e.path()).ok())
            .unwrap_or_default();
        text.push_str("\n---- dstreamy.log ----\n");
        text.push_str(&std::fs::read_to_string(&self.serve_log).unwrap_or_default());
        text
    }

    fn wait_for(&self, what: &str, done: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + WAIT;
        loop {
            let text = self.log();
            if done(&text) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "OBS never logged {what}:\n{text}"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn press_start(&self) {
        std::fs::write(&self.press_start, "").unwrap();
    }

    fn press_stop(&self) {
        std::fs::write(&self.press_stop, "").unwrap();
    }

    /// The session part of serve's status, from a control `hello`.
    fn session(&self) -> Result<Value, IpcError> {
        self.rt.block_on(async {
            let stream = dstreamy_ipc::connect(&self.endpoint).await?;
            let (read, mut write) = tokio::io::split(stream);
            write
                .write_all(
                    b"{\"id\":\"h\",\"cmd\":\"hello\",\"role\":\"control\",\"protocol\":2}\n",
                )
                .await?;
            let mut lines = BufReader::new(read).lines();
            while let Some(line) = lines.next_line().await? {
                let reply: Value = serde_json::from_str(&line).unwrap();
                if reply["id"] == "h" {
                    return Ok(reply["data"]["status"]["session"].clone());
                }
            }
            Err(IpcError::Io(std::io::Error::other(
                "serve closed before its hello reply",
            )))
        })
    }

    /// Closes OBS as its window's close button does and checks that the serve it started went with it.
    fn close(mut self) {
        let pid = self.child.id().to_string();
        let closed = Command::new("taskkill")
            .args(["/PID", &pid])
            .status()
            .unwrap();
        assert!(closed.success(), "taskkill could not ask OBS to close");
        let deadline = Instant::now() + Duration::from_secs(60);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "OBS did not close:\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(200));
        };
        let text = self.log();
        assert!(status.success(), "OBS exited with {status}:\n{text}");
        assert!(!text.contains("[obs-dstreamy] panic caught"), "{text}");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !matches!(self.session(), Err(IpcError::NotRunning(_))) {
            assert!(
                Instant::now() < deadline,
                "the serve the plugin started outlived OBS:\n{text}"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for Obs {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(None)) {
            return;
        }
        // A failed run still leaves Discord cleanly: Stop, then the close button, and only then a kill.
        let _ = std::fs::write(&self.press_stop, "");
        std::thread::sleep(Duration::from_secs(5));
        let _ = Command::new("taskkill")
            .args(["/PID", &self.child.id().to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(30);
        while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "installs into this user's OBS and needs OBS's frontend and the staged plugin; obs-ci runs it"]
fn real_obs_starts_the_bundled_serve_streams_and_ends_it_on_exit() {
    let mock_rt = tokio::runtime::Runtime::new().unwrap();
    let mock = mock_rt.block_on(MockDiscord::start()).unwrap();
    let obs = Obs::launch(
        MOCK_TOKEN,
        MOCK_GUILD_ID,
        MOCK_VOICE_CHANNEL_ID,
        Some(mock.gateway_url()),
    );
    obs.press_start();
    let deadline = Instant::now() + WAIT;
    while mock.recorded_video().len() < 30 {
        assert!(
            Instant::now() < deadline,
            "mock Discord got {} video frames:\n{}",
            mock.recorded_video().len(),
            obs.log()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        obs.session().is_ok(),
        "the plugin's serve stopped answering:\n{}",
        obs.log()
    );
    obs.close();
}
