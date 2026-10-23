//! Another OS user against a real listener. Runs only where `DSTREAMY_CROSS_USER_TESTS=1`
//! (disposable CI runners with passwordless sudo and a `nobody` user).

#![cfg(unix)]

use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use dstreamy_ipc::{Endpoint, IpcError, Listener, connect};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const HELPER_ENV: &str = "DSTREAMY_IPC_HELPER_ENDPOINT";
const RESULT_PREFIX: &str = "IPC-HELPER-RESULT:";

fn enabled() -> bool {
    let on = std::env::var_os("DSTREAMY_CROSS_USER_TESTS").is_some_and(|v| v == "1");
    if !on {
        eprintln!("skipped: set DSTREAMY_CROSS_USER_TESTS=1 on a disposable host");
    }
    on
}

struct Sandbox {
    root: tempfile::TempDir,
    helper: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("dstreamy-xuser-")
            .tempdir_in("/tmp")
            .unwrap();
        fs::set_permissions(root.path(), Permissions::from_mode(0o755)).unwrap();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        fs::set_permissions(&bin, Permissions::from_mode(0o755)).unwrap();
        let helper = bin.join("helper");
        fs::copy(std::env::current_exe().unwrap(), &helper).unwrap();
        fs::set_permissions(&helper, Permissions::from_mode(0o755)).unwrap();
        Self { root, helper }
    }

    fn endpoint(&self) -> Endpoint {
        Endpoint::new(self.root.path().join("ipc").join("serve.sock"))
    }

    async fn run_as_nobody(&self, endpoint: &Endpoint) -> String {
        let mut cmd = Command::new("sudo");
        cmd.args(["-n", "-u", "nobody", "/usr/bin/env"])
            .arg(format!("{HELPER_ENV}={}", endpoint.as_path().display()))
            .arg(&self.helper)
            .args(["--exact", "nobody_client", "--ignored", "--nocapture"])
            .args(["--test-threads", "1"]);
        let output = tokio::task::spawn_blocking(move || cmd.output().unwrap())
            .await
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "helper failed: {stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        stdout
            .lines()
            .find_map(|l| l.strip_prefix(RESULT_PREFIX))
            .unwrap_or_else(|| panic!("no helper result in: {stdout}"))
            .trim()
            .to_owned()
    }
}

fn open_to_everyone(path: &Path) {
    fs::set_permissions(path, Permissions::from_mode(0o777)).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn private_directory_keeps_other_users_out() {
    if !enabled() {
        return;
    }
    let sandbox = Sandbox::new();
    let endpoint = sandbox.endpoint();
    let listener = Listener::bind(&endpoint).await.unwrap();
    open_to_everyone(endpoint.as_path());

    let result = sandbox.run_as_nobody(&endpoint).await;
    assert_eq!(result, "denied");
    let accepted = tokio::time::timeout(Duration::from_millis(300), listener.accept()).await;
    assert!(accepted.is_err(), "a connection reached the listener");
    assert_eq!(listener.rejected_peers(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn peer_check_alone_drops_other_users() {
    if !enabled() {
        return;
    }
    let sandbox = Sandbox::new();
    let endpoint = sandbox.endpoint();
    let listener = Listener::bind_permissive_for_tests(&endpoint)
        .await
        .unwrap();

    let result = tokio::select! {
        accepted = listener.accept() => panic!("another user's connection was accepted: {:?}", accepted.map(|(_, peer)| peer)),
        result = sandbox.run_as_nobody(&endpoint) => result,
    };
    assert_eq!(result, "dropped");
    assert_eq!(listener.rejected_peers(), 1);
}

#[tokio::test]
#[ignore = "run by the cross-user tests as another user"]
async fn nobody_client() {
    let Some(path) = std::env::var_os(HELPER_ENV) else {
        return;
    };
    let result = match connect(&Endpoint::new(path)).await {
        Err(IpcError::Io(e)) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            "denied".to_owned()
        }
        Err(e) => format!("connect-error {e}"),
        Ok(mut stream) => {
            let _ = stream
                .write_all(b"{\"id\":\"1\",\"cmd\":\"hello\"}\n")
                .await;
            let mut buf = [0u8; 64];
            match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf)).await {
                Ok(Ok(0)) | Ok(Err(_)) => "dropped".to_owned(),
                Ok(Ok(n)) => format!("reply {n} bytes"),
                Err(_) => "still-open".to_owned(),
            }
        }
    };
    println!("{RESULT_PREFIX} {result}");
}
