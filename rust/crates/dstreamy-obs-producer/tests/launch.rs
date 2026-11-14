//! The launcher that starts `dstreamy serve` when a connection finds none.
#![cfg(unix)]

use std::path::PathBuf;
use std::time::Duration;

use dstreamy_core::ConfigPaths;
use dstreamy_ipc::IpcError;
use dstreamy_mock_discord::{MOCK_TOKEN, MockDiscord};
use dstreamy_obs_producer::Endpoint;
use dstreamy_obs_producer::launch::{Ended, Launcher};
use tempfile::TempDir;

fn endpoint(dir: &TempDir) -> Endpoint {
    Endpoint::new(dir.path().join("d").join("s.sock"))
}

fn run_dir() -> TempDir {
    tempfile::Builder::new().prefix("ds").tempdir().unwrap()
}

#[tokio::test]
async fn a_missing_program_says_it_could_not_start() {
    let dir = run_dir();
    let launcher = Launcher::new(
        dir.path().join("no-such-dstreamy"),
        ["serve"],
        endpoint(&dir),
    );
    let err = launcher
        .connect(&endpoint(&dir))
        .await
        .err()
        .expect("connected");
    assert!(matches!(err, IpcError::Io(_)), "{err}");
    assert!(err.to_string().contains("could not start"), "{err}");
    assert!(err.to_string().contains("no-such-dstreamy"), "{err}");
}

#[tokio::test]
async fn a_program_that_exits_is_reported_with_its_status() {
    let dir = run_dir();
    let launcher =
        Launcher::new("sh", ["-c", "exit 3"], endpoint(&dir)).with_wait(Duration::from_millis(800));
    let err = launcher
        .connect(&endpoint(&dir))
        .await
        .err()
        .expect("connected");
    let text = err.to_string();
    assert!(text.contains("exited") && text.contains('3'), "{text}");
    assert!(text.contains("its log has the reason"), "{text}");
    assert_eq!(launcher.started(), None);
}

#[tokio::test]
async fn a_recent_start_is_waited_for_not_repeated() {
    let dir = run_dir();
    let marker = dir.path().join("starts");
    let script = format!("echo x >> '{}'; exit 1", marker.display());
    let launcher = Launcher::new("sh", ["-c", script.as_str()], endpoint(&dir))
        .with_wait(Duration::from_millis(300));
    for _ in 0..3 {
        launcher
            .connect(&endpoint(&dir))
            .await
            .err()
            .expect("connected");
    }
    assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 1);
}

#[tokio::test]
async fn another_endpoint_is_left_alone() {
    let dir = run_dir();
    let other = run_dir();
    let launcher = Launcher::new(
        dir.path().join("no-such-dstreamy"),
        ["serve"],
        endpoint(&dir),
    );
    let err = launcher
        .connect(&endpoint(&other))
        .await
        .err()
        .expect("connected");
    assert!(matches!(err, IpcError::NotRunning(_)), "{err}");
}

/// `None` skips the test locally; under `CI=true` a missing binary is a failure.
fn dstreamy_bin() -> Option<PathBuf> {
    match std::env::var_os("DSTREAMY_BIN") {
        Some(bin) => Some(PathBuf::from(bin)),
        None if std::env::var("CI").is_ok_and(|v| v == "true") => {
            panic!("DSTREAMY_BIN is unset: point it at target/debug/dstreamy")
        }
        None => {
            eprintln!("skipped: DSTREAMY_BIN is unset");
            None
        }
    }
}

struct Kill(Option<u32>);

impl Drop for Kill {
    fn drop(&mut self) {
        if let Some(pid) = self.0.and_then(|p| i32::try_from(p).ok()) {
            // SAFETY: kill(2) takes plain integers.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_serve_is_started_once_and_connected_to() {
    let Some(bin) = dstreamy_bin() else { return };
    let mock = MockDiscord::start().await.unwrap();
    let config = tempfile::tempdir().unwrap();
    ConfigPaths::in_dir(config.path())
        .save_token(MOCK_TOKEN)
        .unwrap();
    std::fs::write(config.path().join("config.toml"), "").unwrap();
    let dir = run_dir();
    let ep = endpoint(&dir);
    let args = [
        "--config-dir".into(),
        config.path().as_os_str().to_owned(),
        "--gateway-url".into(),
        mock.gateway_url().into(),
        "serve".into(),
        "--skip-platform".into(),
        "--endpoint".into(),
        ep.as_path().as_os_str().to_owned(),
    ];
    let launcher = Launcher::new(&bin, args, ep.clone()).with_wait(Duration::from_secs(20));
    let (a, b) = tokio::join!(launcher.connect(&ep), launcher.connect(&ep));
    let _kill = Kill(launcher.started());
    a.unwrap();
    b.unwrap();
    assert!(
        launcher.started().is_some(),
        "the started serve should still run"
    );
    let ended = tokio::task::spawn_blocking(move || {
        let ended = launcher.stop_started(Duration::from_secs(10), true);
        (ended, launcher.started())
    })
    .await
    .unwrap();
    assert!(
        matches!(ended.0, Ended::Exited(status) if status.success()),
        "serve is asked to shut down, not killed: {ended:?}"
    );
    assert_eq!(ended.1, None);
    assert!(dstreamy_ipc::connect(&ep).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_started_program_that_ignores_the_shutdown_request_is_killed_after_the_grace() {
    let dir = run_dir();
    let launcher =
        Launcher::new("sleep", ["30"], endpoint(&dir)).with_wait(Duration::from_millis(300));
    launcher
        .connect(&endpoint(&dir))
        .await
        .err()
        .expect("connected");
    assert!(launcher.started().is_some());
    let began = std::time::Instant::now();
    let ended = tokio::task::spawn_blocking(move || {
        let ended = launcher.stop_started(Duration::from_millis(500), false);
        (ended, launcher.started())
    })
    .await
    .unwrap();
    assert_eq!(ended, (Ended::Killed, None));
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "{:?}",
        began.elapsed()
    );
}

#[test]
fn a_launcher_that_started_nothing_has_nothing_to_stop() {
    let dir = run_dir();
    let launcher = Launcher::new("sleep", ["30"], endpoint(&dir));
    assert_eq!(
        launcher.stop_started(Duration::from_millis(100), true),
        Ended::NotRunning
    );
}

/// A stand-in serve at `ep` whose `hello` reports `state`; returns every line it was sent.
fn hello_service(ep: &Endpoint, state: &'static str) -> std::thread::JoinHandle<Vec<String>> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    let dir = ep.as_path().parent().unwrap();
    std::fs::create_dir_all(dir).unwrap();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(ep.as_path()).unwrap();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut lines = Vec::new();
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            if line.contains("\"hello\"") {
                let reply = format!(
                    "{{\"id\":\"1\",\"type\":\"result\",\"data\":{{\"status\":{{\"session\":{{\"state\":\"{state}\"}}}}}}}}\n"
                );
                writer.write_all(reply.as_bytes()).unwrap();
            }
            lines.push(line);
        }
        lines
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_serve_running_another_clients_session_is_left_running() {
    let dir = run_dir();
    let ep = endpoint(&dir);
    let launcher = Launcher::new("sleep", ["30"], ep.clone()).with_wait(Duration::from_millis(300));
    launcher.connect(&ep).await.err().expect("connected");
    let service = hello_service(&ep, "streaming");
    let ended = tokio::task::spawn_blocking(move || {
        let ended = launcher.stop_started(Duration::from_secs(2), true);
        let running = launcher.started().is_some();
        launcher.stop_started(Duration::from_millis(100), false);
        (ended, running)
    })
    .await
    .unwrap();
    assert_eq!(ended, (Ended::LeftRunning, true));
    let sent = service.join().unwrap();
    assert!(
        !sent.iter().any(|l| l.contains("shutdown")),
        "no shutdown while another client streams: {sent:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_idle_serve_is_asked_to_shut_down() {
    let dir = run_dir();
    let ep = endpoint(&dir);
    let launcher = Launcher::new("sleep", ["30"], ep.clone()).with_wait(Duration::from_millis(300));
    launcher.connect(&ep).await.err().expect("connected");
    let service = hello_service(&ep, "stopped");
    let ended =
        tokio::task::spawn_blocking(move || launcher.stop_started(Duration::from_secs(1), true))
            .await
            .unwrap();
    assert_eq!(ended, Ended::Killed, "sleep ignores the request");
    let sent = service.join().unwrap();
    assert!(sent.iter().any(|l| l.contains("shutdown")), "{sent:?}");
}
