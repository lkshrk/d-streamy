#![cfg(unix)]

use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dstreamy_ipc::Endpoint;

#[cfg(not(target_os = "macos"))]
const HOME_FALLBACK: &str = ".local/state/dstreamy/serve.sock";

/// Marks the child that resolves the default endpoint under one environment; the parent never calls `set_var`.
const CHILD_ENV: &str = "DSTREAMY_TEST_ENDPOINT_CHILD";

fn passwd_home() -> PathBuf {
    // SAFETY: the returned entry is copied out before any other passwd call.
    let dir = unsafe { CStr::from_ptr((*libc::getpwuid(libc::geteuid())).pw_dir) };
    PathBuf::from(dir.to_str().unwrap())
}

/// `path` with its longest existing prefix canonicalized, the form the endpoint resolver returns.
#[cfg(not(target_os = "macos"))]
fn resolved(path: &Path) -> PathBuf {
    let existing = path.ancestors().find(|p| p.exists()).unwrap();
    let rest = path.strip_prefix(existing).unwrap();
    std::fs::canonicalize(existing).unwrap().join(rest)
}

/// The default endpoint as `endpoint_env_helper` resolves it in a child with `HOME` wrong and `env` applied.
fn resolve_with(env: &[(&str, Option<&Path>)]) -> PathBuf {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "endpoint_env_helper", "--nocapture"])
        .env("HOME", "/home/not-the-user")
        .env(CHILD_ENV, "1")
        .stderr(Stdio::inherit());
    for (name, value) in env {
        match value {
            Some(value) => cmd.env(name, value),
            None => cmd.env_remove(name),
        };
    }
    let output = cmd.output().unwrap();
    assert!(output.status.success(), "{env:?}: {}", output.status);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let line = stdout
        .lines()
        .find_map(|l| l.strip_prefix("endpoint="))
        .unwrap_or_else(|| panic!("no endpoint line in {stdout:?}"));
    PathBuf::from(line)
}

#[test]
fn endpoint_env_helper() {
    if std::env::var_os(CHILD_ENV).is_some() {
        let endpoint = Endpoint::default_for_user().unwrap();
        println!("endpoint={}", endpoint.as_path().display());
    }
}

#[cfg(not(target_os = "macos"))]
#[test]
fn default_endpoint_uses_a_trusted_runtime_dir_or_the_passwd_home() {
    const VAR: &str = "XDG_RUNTIME_DIR";
    let fallback = resolved(&passwd_home().join(HOME_FALLBACK));
    let runtime = tempfile::Builder::new()
        .prefix(".ipc-env")
        .tempdir_in(passwd_home())
        .unwrap();
    std::fs::set_permissions(
        runtime.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    assert_eq!(
        resolve_with(&[(VAR, Some(runtime.path()))]),
        resolved(&runtime.path().join("dstreamy/serve.sock"))
    );

    let missing = runtime.path().join("not-created");
    assert_eq!(resolve_with(&[(VAR, Some(&missing))]), fallback);

    for untrusted in ["/tmp", "/tmp/", "/tmp/dstreamy-base"] {
        assert_eq!(
            resolve_with(&[(VAR, Some(Path::new(untrusted)))]),
            fallback,
            "{VAR}={untrusted}"
        );
    }

    let escape = passwd_home().join("no-such-dir-xyz/../../../tmp");
    assert_eq!(resolve_with(&[(VAR, Some(&escape))]), fallback);

    let long = format!("/var/folders/{}/T/", "q".repeat(100));
    assert_eq!(resolve_with(&[(VAR, Some(Path::new(&long)))]), fallback);

    assert_eq!(resolve_with(&[(VAR, None)]), fallback);
}

/// OBS.app launched by launchd and a `serve` from a terminal have different `TMPDIR`s and must still meet.
#[test]
fn default_endpoint_does_not_depend_on_tmpdir() {
    let other = tempfile::Builder::new()
        .prefix(".ipc-tmp")
        .tempdir_in(passwd_home())
        .unwrap();
    std::fs::set_permissions(
        other.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let unset = resolve_with(&[("TMPDIR", None)]);
    assert_eq!(resolve_with(&[("TMPDIR", Some(other.path()))]), unset);
    assert_eq!(resolve_with(&[("TMPDIR", Some(Path::new("/tmp")))]), unset);
    assert!(!unset.starts_with(other.path()), "{}", unset.display());
    assert!(unset.is_absolute());
    assert!(
        unset.ends_with("dstreamy/serve.sock"),
        "{}",
        unset.display()
    );
}
