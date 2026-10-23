#![cfg(unix)]

use std::ffi::CStr;
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

fn passwd_home() -> PathBuf {
    // SAFETY: single-threaded test; the returned entry is copied out before any other passwd call.
    let dir = unsafe { CStr::from_ptr((*libc::getpwuid(libc::geteuid())).pw_dir) };
    PathBuf::from(dir.to_str().unwrap())
}

fn default_path() -> PathBuf {
    Endpoint::default_for_user().unwrap().as_path().to_owned()
}

// The only test in this binary, so mutating the environment races with nothing.
#[test]
fn default_endpoint_uses_a_trusted_runtime_dir_or_the_passwd_home() {
    let fallback = passwd_home().join(HOME_FALLBACK);
    // SAFETY: no other thread exists that reads the environment.
    unsafe { std::env::set_var("HOME", "/home/not-the-user") };

    // SAFETY: as above.
    unsafe { std::env::set_var(VAR, "/run/user/test") };
    assert_eq!(
        default_path(),
        PathBuf::from("/run/user/test/dstreamy/serve.sock")
    );

    for untrusted in ["/tmp", "/tmp/", "/tmp/dstreamy-base"] {
        // SAFETY: as above.
        unsafe { std::env::set_var(VAR, untrusted) };
        assert_eq!(default_path(), fallback, "{VAR}={untrusted}");
    }

    let escape = passwd_home().join("no-such-dir-xyz/../../../tmp");
    // SAFETY: as above.
    unsafe { std::env::set_var(VAR, &escape) };
    assert_eq!(default_path(), fallback, "{VAR}={}", escape.display());

    let long = format!("/var/folders/{}/T/", "q".repeat(100));
    // SAFETY: as above.
    unsafe { std::env::set_var(VAR, &long) };
    assert_eq!(default_path(), fallback);

    // SAFETY: as above.
    unsafe { std::env::remove_var(VAR) };
    assert_eq!(default_path(), fallback);
}
