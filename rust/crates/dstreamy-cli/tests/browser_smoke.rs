#![cfg(all(feature = "browser-login", target_os = "linux"))]

use std::time::Duration;

use assert_cmd::Command;

fn has_display() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some()
}

#[test]
fn unresolvable_host_shows_recovery_in_a_fixed_dark_popup() {
    if !has_display() {
        eprintln!("skipped: no display");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let output = Command::cargo_bin("dstreamy")
        .unwrap()
        .arg("--config-dir")
        .arg(dir.path())
        .args(["login", "--browser"])
        .env("DSTREAMY_LOGIN_SMOKE", "1")
        .env("DSTREAMY_LOGIN_URL", "http://login-smoke.invalid/")
        .timeout(Duration::from_secs(60))
        .output()
        .unwrap();
    let err = String::from_utf8(output.stderr).unwrap();
    assert!(
        err.contains("smoke: size=520x760 background=(49, 51, 56, 255)"),
        "{err}"
    );
    assert!(err.contains("smoke: view=Recovery(PageNotLoaded)"), "{err}");
    assert!(!dir.path().join("token").exists());
    eprintln!("browser smoke: ran with a display");
}
