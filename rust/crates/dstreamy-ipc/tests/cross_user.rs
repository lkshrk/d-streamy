//! Another OS user against real sockets; runs only with `DSTREAMY_CROSS_USER_TESTS=1` on disposable CI hosts.

#![cfg(unix)]

use std::fs::{self, Permissions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener as StdListener, UnixStream as StdStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use dstreamy_ipc::{Endpoint, IpcError, Listener, connect};

const HELPER_PATH: &str = "DSTREAMY_IPC_HELPER_PATH";
const HELPER_MODE: &str = "DSTREAMY_IPC_HELPER_MODE";
const MARKER: &str = "IPC-HELPER-RESULT:";

fn enabled() -> bool {
    let on = std::env::var_os("DSTREAMY_CROSS_USER_TESTS").is_some_and(|v| v == "1");
    if !on {
        eprintln!("skipped: set DSTREAMY_CROSS_USER_TESTS=1 on a disposable host");
    }
    on
}

fn mkdir(path: &Path, mode: u32) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, Permissions::from_mode(mode)).unwrap();
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
async fn private_directory_keeps_other_users_out() {
    if !enabled() {
        return;
    }
    let sandbox = Sandbox::new();
    let endpoint = sandbox.endpoint();
    let listener = Listener::bind(&endpoint).await.unwrap();
    fs::set_permissions(endpoint.as_path(), Permissions::from_mode(0o777)).unwrap();

    let result = sandbox.run_as_nobody("connect", endpoint.as_path()).await;
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
        result = sandbox.run_as_nobody("connect", endpoint.as_path()) => result,
    };
    assert_eq!(result, "dropped");
    assert_eq!(listener.rejected_peers(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn client_refuses_a_server_run_by_another_user() {
    if !enabled() {
        return;
    }
    let sandbox = Sandbox::new();
    let open = sandbox.root.path().join("open");
    mkdir(&open, 0o777);
    let planted = open.join("serve.sock");
    let mut child = sandbox.spawn_as_nobody("serve", &planted);
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let ready = lines
        .by_ref()
        .map_while(Result::ok)
        .find_map(|l| result_in(&l));
    assert_eq!(ready.as_deref(), Some("listening"));

    let endpoint = sandbox.endpoint();
    mkdir(endpoint.as_path().parent().unwrap(), 0o700);
    fs::rename(&planted, endpoint.as_path()).unwrap();

    let err = connect(&endpoint).await.err().unwrap();
    assert!(
        matches!(err, IpcError::ImpostorServer { uid, .. } if uid != 0),
        "{err}"
    );
    let rest: Vec<String> = lines.map_while(Result::ok).collect();
    assert!(child.wait().unwrap().success(), "{rest:?}");
    assert_eq!(
        rest.iter().find_map(|l| result_in(l)).as_deref(),
        Some("accepted")
    );
}

#[test]
#[ignore = "run by the cross-user tests as another user"]
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
            let listener = StdListener::bind(&path).unwrap();
            fs::set_permissions(&path, Permissions::from_mode(0o777)).unwrap();
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_secs(20));
                std::process::exit(3);
            });
            report("listening");
            let _ = listener.accept().unwrap();
            report("accepted");
        }
        other => panic!("unknown helper mode {other}"),
    }
}
