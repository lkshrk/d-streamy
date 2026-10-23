#![cfg(unix)]

use std::path::PathBuf;

use dstreamy_ipc::Endpoint;

#[cfg(target_os = "macos")]
const VAR: &str = "TMPDIR";
#[cfg(not(target_os = "macos"))]
const VAR: &str = "XDG_RUNTIME_DIR";

// The only test in this binary, so mutating the environment races with nothing.
#[test]
fn default_endpoint_follows_the_runtime_dir_and_falls_back_when_too_long() {
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let fallback = PathBuf::from(format!("/tmp/dstreamy-{uid}/serve.sock"));

    // SAFETY: no other thread exists that reads the environment.
    unsafe { std::env::set_var(VAR, "/run/user/test") };
    assert_eq!(
        Endpoint::default_for_user().as_path(),
        PathBuf::from("/run/user/test/dstreamy/serve.sock")
    );

    let long = format!("/var/folders/{}/T/", "q".repeat(100));
    // SAFETY: as above.
    unsafe { std::env::set_var(VAR, &long) };
    assert_eq!(Endpoint::default_for_user().as_path(), fallback);

    // SAFETY: as above.
    unsafe { std::env::remove_var(VAR) };
    assert_eq!(Endpoint::default_for_user().as_path(), fallback);
}
