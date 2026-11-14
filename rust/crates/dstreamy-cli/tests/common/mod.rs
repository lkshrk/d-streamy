#![allow(dead_code)]

use std::path::Path;
use std::time::Duration;

use dstreamy_mock_discord::MOCK_GUILD_ID;

const T: Duration = Duration::from_secs(20);

pub fn bare() -> assert_cmd::Command {
    let mut cmd = assert_cmd::Command::cargo_bin("dstreamy").unwrap();
    cmd.env_remove("DSTREAMY_TOKEN")
        .env_remove("DSTREAMY_CONFIG_DIR")
        .timeout(T);
    cmd
}

pub fn dstreamy(config_dir: &Path, gateway: &str) -> assert_cmd::Command {
    let mut cmd = bare();
    cmd.arg("--config-dir")
        .arg(config_dir)
        .arg("--gateway-url")
        .arg(gateway);
    cmd
}

pub fn spawnable(config_dir: &Path, gateway: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("dstreamy"));
    cmd.env_remove("DSTREAMY_TOKEN")
        .env_remove("DSTREAMY_CONFIG_DIR")
        .arg("--config-dir")
        .arg(config_dir)
        .arg("--gateway-url")
        .arg(gateway);
    cmd
}

pub fn stream_args(channel: &str) -> Vec<&str> {
    vec![
        "stream",
        "--guild",
        MOCK_GUILD_ID,
        "--channel",
        channel,
        "--target",
        "test-pattern",
        "--res",
        "320x180",
        "--fps",
        "15",
        "--bitrate",
        "1000",
    ]
}

pub fn ok(cmd: &mut assert_cmd::Command) -> (String, String) {
    let out = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{stderr}");
    (String::from_utf8(out.stdout).unwrap(), stderr)
}

pub fn failed(cmd: &mut assert_cmd::Command) -> String {
    let out = cmd.output().unwrap();
    assert!(!out.status.success());
    String::from_utf8(out.stderr).unwrap()
}
