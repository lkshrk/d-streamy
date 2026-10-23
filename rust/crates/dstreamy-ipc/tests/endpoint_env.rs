#![cfg(unix)]

use std::path::PathBuf;

use dstreamy_ipc::Endpoint;

#[cfg(target_os = "macos")]
const VAR: &str = "TMPDIR";
#[cfg(not(target_os = "macos"))]
const VAR: &str = "XDG_RUNTIME_DIR";

#[cfg(target_os = "macos")]
const HOME_FALLBACK: &str = "Library/Application Support/dstreamy/serve.sock";
#[cfg(not(target_os = "macos"))]
const HOME_FALLBACK: &str = ".local/state/dstreamy/serve.sock";

fn default_path() -> PathBuf {
    Endpoint::default_for_user().unwrap().as_path().to_owned()
}

// The only test in this binary, so mutating the environment races with nothing.
#[test]
fn default_endpoint_follows_the_runtime_dir_and_falls_back_to_home() {
    let fallback = PathBuf::from("/home/ipc-test").join(HOME_FALLBACK);
    // SAFETY: no other thread exists that reads the environment.
    unsafe { std::env::set_var("HOME", "/home/ipc-test") };

    // SAFETY: as above.
    unsafe { std::env::set_var(VAR, "/run/user/test") };
    assert_eq!(
        default_path(),
        PathBuf::from("/run/user/test/dstreamy/serve.sock")
    );

    let long = format!("/var/folders/{}/T/", "q".repeat(100));
    // SAFETY: as above.
    unsafe { std::env::set_var(VAR, &long) };
    assert_eq!(default_path(), fallback);

    // SAFETY: as above.
    unsafe { std::env::remove_var(VAR) };
    assert_eq!(default_path(), fallback);

    let long_home = format!("/home/{}", "h".repeat(100));
    // SAFETY: as above.
    unsafe { std::env::set_var("HOME", &long_home) };
    let err = Endpoint::default_for_user().unwrap_err();
    assert!(
        matches!(err, dstreamy_ipc::IpcError::PathTooLong(ref p) if p.starts_with(&long_home)),
        "{err}"
    );
}
