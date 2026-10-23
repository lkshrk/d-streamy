//! Another OS user against real sockets; CI runs these with `--ignored` on disposable hosts.

#![cfg(unix)]

mod common;

use common::within;
use std::fs::{self, Permissions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener as StdListener, UnixStream as StdStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use dstreamy_ipc::{Endpoint, IpcError, Listener, connect};

const HELPER_PATH: &str = "DSTREAMY_IPC_HELPER_PATH";
const HELPER_MODE: &str = "DSTREAMY_IPC_HELPER_MODE";
const MARKER: &str = "IPC-HELPER-RESULT:";

fn require_disposable_host() {
    assert!(
        std::env::var_os("DSTREAMY_CROSS_USER_TESTS").is_some_and(|v| v == "1"),
        "cross-user tests need sudo and a nobody user: set DSTREAMY_CROSS_USER_TESTS=1 on a disposable host"
    );
}

fn mkdir(path: &Path, mode: u32) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, Permissions::from_mode(mode)).unwrap();
}

/// Kills the helper (and the sudo wrapping it) if the test fails or times out, so its pipe closes.
struct KillOnDrop {
    child: Option<Child>,
    helper_pid: Arc<OnceLock<String>>,
}

impl KillOnDrop {
    fn wait(mut self) -> std::process::ExitStatus {
        self.child.take().unwrap().wait().unwrap()
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        if let Some(pid) = self.helper_pid.get() {
            let _ = Command::new("sudo")
                .args(["-n", "-u", "nobody", "kill", "-9", pid])
                .status();
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn nobody_uid() -> u32 {
    let out = Command::new("id").args(["-u", "nobody"]).output().unwrap();
    let uid: i64 = String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // macOS prints nobody as -2, the two's-complement spelling of uid 4294967294.
    uid as u32
}

fn result_in(line: &str) -> Option<String> {
    line.find(MARKER)
        .map(|i| line[i + MARKER.len()..].trim().to_owned())
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
        mkdir(&bin, 0o755);
        let helper = bin.join("helper");
        fs::copy(std::env::current_exe().unwrap(), &helper).unwrap();
        fs::set_permissions(&helper, Permissions::from_mode(0o755)).unwrap();
        Self { root, helper }
    }

    fn endpoint(&self) -> Endpoint {
        Endpoint::new(self.root.path().join("ipc").join("serve.sock"))
    }

    fn spawn_as_nobody(&self, mode: &str, path: &Path) -> Child {
        Command::new("sudo")
            .args(["-n", "-u", "nobody", "/usr/bin/env"])
            .arg(format!("{HELPER_PATH}={}", path.display()))
            .arg(format!("{HELPER_MODE}={mode}"))
            .arg(&self.helper)
            .args(["--exact", "nobody_helper", "--ignored", "--nocapture"])
            .args(["--test-threads", "1"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap()
    }

    async fn run_as_nobody(&self, mode: &'static str, path: &Path) -> String {
        let child = self.spawn_as_nobody(mode, path);
        let output = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap())
            .await
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "helper failed: {stdout}");
        stdout
            .lines()
            .find_map(result_in)
            .unwrap_or_else(|| panic!("no helper result in: {stdout}"))
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "cross-user: needs sudo and a nobody user, run in CI"]
async fn private_directory_keeps_other_users_out() {
    within(async {
        require_disposable_host();
        let sandbox = Sandbox::new();
        let endpoint = sandbox.endpoint();
        let listener = Listener::bind(&endpoint).await.unwrap();
        fs::set_permissions(endpoint.as_path(), Permissions::from_mode(0o777)).unwrap();

        let result = sandbox.run_as_nobody("connect", endpoint.as_path()).await;
        assert_eq!(result, "denied");
        let accepted = tokio::time::timeout(Duration::from_millis(300), listener.accept()).await;
        assert!(accepted.is_err(), "a connection reached the listener");
        assert_eq!(listener.rejected_peers(), 0);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "cross-user: needs sudo and a nobody user, run in CI"]
async fn peer_check_alone_drops_other_users() {
    within(async {
        require_disposable_host();
        let sandbox = Sandbox::new();
        let endpoint = sandbox.endpoint();
        let listener = Listener::bind_permissive_for_tests(&endpoint)
            .await
            .unwrap();

        let result = tokio::select! {
            accepted = listener.accept() => panic!("another user's connection was accepted: {:?}", accepted.map(|(_, peer)| peer)),
            result = sandbox.run_as_nobody("connect", endpoint.as_path()) => result,
        };
        assert_eq!(result, "dropped");
        assert_eq!(listener.rejected_peers(), 1);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "cross-user: needs sudo and a nobody user, run in CI"]
async fn client_refuses_a_server_run_by_another_user() {
    within(async {
        require_disposable_host();
        let sandbox = Sandbox::new();
        let open = sandbox.root.path().join("open");
        mkdir(&open, 0o777);
        let planted = open.join("serve.sock");
        let mut child = sandbox.spawn_as_nobody("serve", &planted);
        let stdout = child.stdout.take().unwrap();
        let helper_pid = Arc::new(OnceLock::new());
        let child = KillOnDrop {
            child: Some(child),
            helper_pid: helper_pid.clone(),
        };
        let (tx, mut results) = tokio::sync::mpsc::unbounded_channel();
        let reader = tokio::task::spawn_blocking(move || {
            let mut lines = Vec::new();
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(result) = result_in(&line) {
                    match result.strip_prefix("pid ") {
                        Some(pid) => {
                            let _ = helper_pid.set(pid.to_owned());
                        }
                        None => {
                            let _ = tx.send(result);
                        }
                    }
                }
                lines.push(line);
            }
            lines
        });
        assert_eq!(results.recv().await.as_deref(), Some("listening"));

        let endpoint = sandbox.endpoint();
        mkdir(endpoint.as_path().parent().unwrap(), 0o700);
        fs::rename(&planted, endpoint.as_path()).unwrap();

        let err = connect(&endpoint).await.err().unwrap();
        let nobody = nobody_uid();
        assert!(
            matches!(err, IpcError::ImpostorServer { uid, .. } if uid == nobody),
            "expected ImpostorServer with uid {nobody}: {err}"
        );
        assert_eq!(results.recv().await.as_deref(), Some("accepted"));
        fs::write(open.join("release"), b"").unwrap();
        let status = tokio::task::spawn_blocking(move || child.wait())
            .await
            .unwrap();
        let lines = reader.await.unwrap();
        assert!(status.success(), "{lines:?}");
    })
    .await;
}

#[test]
#[ignore = "helper run by the cross-user tests as another user"]
fn nobody_helper() {
    let (Some(path), Some(mode)) = (
        std::env::var_os(HELPER_PATH),
        std::env::var(HELPER_MODE).ok(),
    ) else {
        return;
    };
    let report = |result: &str| {
        println!("\n{MARKER} {result}");
        std::io::stdout().flush().unwrap();
    };
    match mode.as_str() {
        "connect" => report(&match StdStream::connect(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => "denied".to_owned(),
            Err(e) => format!("connect-error {e}"),
            Ok(mut stream) => {
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let _ = stream.write_all(b"{\"id\":\"1\",\"cmd\":\"hello\"}\n");
                let mut buf = [0u8; 64];
                match stream.read(&mut buf) {
                    Ok(0) => "dropped".to_owned(),
                    Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {
                        "dropped".to_owned()
                    }
                    Ok(n) => format!("reply {n} bytes"),
                    Err(e) => format!("read-error {e}"),
                }
            }
        }),
        "serve" => {
            report(&format!("pid {}", std::process::id()));
            let listener = StdListener::bind(&path).unwrap();
            fs::set_permissions(&path, Permissions::from_mode(0o777)).unwrap();
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_secs(20));
                std::process::exit(3);
            });
            report("listening");
            let (held, _) = listener.accept().unwrap();
            report("accepted");
            let release = Path::new(&path).with_file_name("release");
            while !release.exists() {
                std::thread::sleep(Duration::from_millis(20));
            }
            drop(held);
        }
        other => panic!("unknown helper mode {other}"),
    }
}
