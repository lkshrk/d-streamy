#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

const GOLDEN: &str = include_str!("fixtures/service/dstreamy.service");

fn dstreamy() -> PathBuf {
    assert_cmd::cargo::cargo_bin("dstreamy")
}

/// A temp home whose `systemctl` only records its arguments and answers `show` from a file.
struct Fake {
    dir: TempDir,
}

impl Fake {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("systemctl");
        fs::write(
            &script,
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> \"$(dirname \"$0\")/calls\"\n\
             if [ \"$2\" = show ] && [ -f \"$(dirname \"$0\")/show\" ]; then cat \"$(dirname \"$0\")/show\"; fi\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    fn systemctl(&self) -> PathBuf {
        self.dir.path().join("systemctl")
    }

    fn unit_path(&self) -> PathBuf {
        self.dir.path().join("config/systemd/user/dstreamy.service")
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.dir.path().join("calls")).unwrap_or_default()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_bin(&dstreamy(), args)
    }

    fn run_bin(&self, bin: &Path, args: &[&str]) -> Output {
        Command::new(bin)
            .env("HOME", self.dir.path())
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .args(args)
            .arg("--systemctl")
            .arg(self.systemctl())
            .output()
            .unwrap()
    }
}

fn stdout(output: &Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn golden_for(exe: &Path) -> String {
    GOLDEN.replace("/usr/local/bin/dstreamy", exe.to_str().unwrap())
}

#[test]
fn print_shows_the_unit_for_this_executable() {
    let fake = Fake::new();
    assert_eq!(
        stdout(&fake.run(&["service", "print"])),
        golden_for(&dstreamy())
    );
    assert_eq!(fake.calls(), "");
}

#[test]
fn install_dry_run_prints_the_file_and_the_exact_commands_and_runs_nothing() {
    let fake = Fake::new();
    let systemctl = fake.systemctl();
    let expected = format!(
        "would write {}:\n{}would run: {s} --user daemon-reload\nwould run: {s} --user enable --now dstreamy.service\n",
        fake.unit_path().display(),
        golden_for(&dstreamy()),
        s = systemctl.display(),
    );
    assert_eq!(
        stdout(&fake.run(&["service", "install", "--dry-run"])),
        expected
    );
    assert_eq!(fake.calls(), "");
    assert!(!fake.unit_path().exists());
}

#[test]
fn uninstall_dry_run_prints_the_exact_commands_and_runs_nothing() {
    let fake = Fake::new();
    let systemctl = fake.systemctl();
    let expected = format!(
        "would run: {s} --user disable --now dstreamy.service\nwould remove {}\nwould run: {s} --user daemon-reload\n",
        fake.unit_path().display(),
        s = systemctl.display(),
    );
    assert_eq!(
        stdout(&fake.run(&["service", "uninstall", "--dry-run"])),
        expected
    );
    assert_eq!(fake.calls(), "");
}

#[test]
fn install_writes_the_unit_then_reloads_and_enables_it_and_uninstall_reverses_it() {
    let fake = Fake::new();
    stdout(&fake.run(&["service", "install"]));
    assert_eq!(
        fs::read_to_string(fake.unit_path()).unwrap(),
        golden_for(&dstreamy())
    );
    assert_eq!(
        fake.calls(),
        "--user daemon-reload\n--user enable --now dstreamy.service\n"
    );

    stdout(&fake.run(&["service", "uninstall"]));
    assert!(!fake.unit_path().exists());
    assert_eq!(
        fake.calls(),
        "--user daemon-reload\n--user enable --now dstreamy.service\n\
         --user disable --now dstreamy.service\n--user daemon-reload\n"
    );
}

#[test]
fn a_failing_systemctl_fails_install_and_status_reports_it() {
    let fake = Fake::new();
    fs::write(
        fake.systemctl(),
        "#!/bin/sh\necho 'Failed to connect to bus' >&2\nexit 1\n",
    )
    .unwrap();
    let output = fake.run(&["service", "install"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Failed to connect to bus"), "{stderr}");
    assert!(stderr.contains("daemon-reload"), "{stderr}");

    let endpoint = fake.dir.path().join("none.sock");
    let output = fake.run(&[
        "--json",
        "service",
        "status",
        "--endpoint",
        endpoint.to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    let v: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["running"], false, "{v}");
    assert!(v["manager"].as_str().unwrap().contains("show"), "{v}");
    assert_eq!(v["hello"], false, "{v}");
}

#[test]
fn status_reports_the_manager_state_and_whether_the_socket_answers() {
    let fake = Fake::new();
    fs::write(
        fake.dir.path().join("show"),
        "LoadState=loaded\nActiveState=active\nSubState=running\nUnitFileState=enabled\nMainPID=77\n",
    )
    .unwrap();
    let endpoint = fake.dir.path().join("run/dstreamy/serve.sock");
    let status = |fake: &Fake| {
        let output = fake.run(&[
            "--json",
            "service",
            "status",
            "--endpoint",
            endpoint.to_str().unwrap(),
        ]);
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        (output.status.success(), value)
    };

    let (ok, v) = status(&fake);
    assert!(!ok, "{v}");
    assert_eq!(v["installed"], false, "{v}");
    assert_eq!(v["running"], true, "{v}");
    assert_eq!(v["hello"], false, "{v}");

    stdout(&fake.run(&["service", "install"]));
    let (ok, v) = status(&fake);
    assert!(!ok, "{v}");
    assert_eq!(v["installed"], true, "{v}");
    assert_eq!(v["hello"], false, "{v}");
    let config = fake.dir.path().join("dstreamy");
    let mut serve = Command::new(dstreamy())
        .arg("--config-dir")
        .arg(&config)
        .args(["serve", "--skip-platform", "--endpoint"])
        .arg(&endpoint)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let v = loop {
        let (ok, v) = status(&fake);
        if ok {
            break v;
        }
        assert!(Instant::now() < deadline, "status never healthy: {v}");
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = serve.kill();
    let _ = serve.wait();
    assert_eq!(v["installed"], true, "{v}");
    assert_eq!(v["enabled"], true, "{v}");
    assert_eq!(v["hello"], true, "{v}");
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"), "{v}");
    assert_eq!(
        v["manager"], "loaded, active (running), enabled, pid 77",
        "{v}"
    );
    assert!(
        fake.calls()
            .ends_with("--user show --property=LoadState,ActiveState,SubState,UnitFileState,MainPID dstreamy.service\n"),
        "{}",
        fake.calls()
    );
}

#[test]
fn a_relative_config_directory_is_refused() {
    let fake = Fake::new();
    let output = Command::new(dstreamy())
        .env("HOME", fake.dir.path())
        .env("XDG_CONFIG_HOME", "")
        .args(["service", "install", "--dry-run"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("is not absolute"), "{stderr}");
}

#[test]
fn an_executable_path_with_spaces_and_percent_signs_is_quoted_for_systemd() {
    let fake = Fake::new();
    let dir = fake.dir.path().join("my apps 100%");
    fs::create_dir(&dir).unwrap();
    let exe = dir.join("dstreamy");
    fs::copy(dstreamy(), &exe).unwrap();
    let unit = stdout(&fake.run_bin(&exe, &["service", "print"]));
    let exec_start = format!(
        "ExecStart=\"{}/my apps 100%%/dstreamy\" serve\n",
        fake.dir.path().display()
    );
    assert!(unit.contains(&exec_start), "{unit}");
}

fn require_disposable_host() {
    assert!(
        std::env::var_os("DSTREAMY_DISPOSABLE_HOST").is_some_and(|v| v == "1"),
        "this test installs a real systemd user unit: set DSTREAMY_DISPOSABLE_HOST=1 on a disposable host"
    );
}

fn sh(program: &str, args: &[&str]) -> Output {
    Command::new(program).args(args).output().unwrap()
}

fn main_pid() -> u32 {
    let out = sh(
        "systemctl",
        &[
            "--user",
            "show",
            "-p",
            "MainPID",
            "--value",
            "dstreamy.service",
        ],
    );
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

fn status() -> (bool, Value) {
    let output = Command::new(dstreamy())
        .args(["--json", "service", "status"])
        .output()
        .unwrap();
    let value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    (output.status.success(), value)
}

/// Waits until the service is running under a main PID other than `not` and answers `hello`.
fn wait_healthy(within: Duration, not: u32) -> u32 {
    let deadline = Instant::now() + within;
    loop {
        let pid = main_pid();
        let (ok, v) = status();
        if ok && pid != 0 && pid != not && v["hello"] == true && v["running"] == true {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "not healthy with a new MainPID (old {not}, now {pid}) within {within:?}: {v}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn wait_user_manager() {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let out = sh("systemctl", &["--user", "is-system-running"]);
        let state = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if state == "running" || state == "degraded" {
            return;
        }
        assert!(Instant::now() < deadline, "user manager is {state:?}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

struct Uninstall;

impl Drop for Uninstall {
    fn drop(&mut self) {
        let _ = Command::new(dstreamy())
            .args(["service", "uninstall"])
            .status();
    }
}

#[test]
#[ignore = "installs a real systemd user unit; needs DSTREAMY_DISPOSABLE_HOST=1"]
fn a_real_install_survives_sigkill_and_a_user_manager_restart() {
    require_disposable_host();
    wait_user_manager();
    let _cleanup = Uninstall;

    let unit_dir = std::env::home_dir().unwrap().join(".config/systemd/user");
    let installed = Command::new(dstreamy())
        .args(["service", "install"])
        .output()
        .unwrap();
    stdout(&installed);
    let unit_path = unit_dir.join("dstreamy.service");
    assert_eq!(
        fs::read_to_string(&unit_path).unwrap(),
        golden_for(&dstreamy())
    );
    let verify = sh(
        "systemd-analyze",
        &["--user", "verify", unit_path.to_str().unwrap()],
    );
    assert!(
        verify.status.success(),
        "verify: {}",
        String::from_utf8_lossy(&verify.stderr)
    );

    let first = wait_healthy(Duration::from_secs(15), 0);
    let (_, v) = status();
    assert_eq!(v["installed"], true, "{v}");
    assert_eq!(v["enabled"], true, "{v}");

    assert!(sh("kill", &["-9", &first.to_string()]).status.success());
    let second = wait_healthy(Duration::from_secs(15), first);

    let uid = String::from_utf8(sh("id", &["-u"]).stdout).unwrap();
    let restarted = sh(
        "sudo",
        &[
            "systemctl",
            "restart",
            &format!("user@{}.service", uid.trim()),
        ],
    );
    assert!(
        restarted.status.success(),
        "{}",
        String::from_utf8_lossy(&restarted.stderr)
    );
    wait_user_manager();
    wait_healthy(Duration::from_secs(30), second);

    stdout(
        &Command::new(dstreamy())
            .args(["service", "uninstall"])
            .output()
            .unwrap(),
    );
    assert!(!unit_path.exists());
    let load = sh(
        "systemctl",
        &[
            "--user",
            "show",
            "-p",
            "LoadState",
            "--value",
            "dstreamy.service",
        ],
    );
    assert_eq!(String::from_utf8_lossy(&load.stdout).trim(), "not-found");
    let (ok, v) = status();
    assert!(!ok, "{v}");
    assert_eq!(v["installed"], false, "{v}");
    assert_eq!(v["hello"], false, "{v}");
}

#[test]
#[ignore = "verifies units with systemd-analyze; needs DSTREAMY_DISPOSABLE_HOST=1"]
fn systemd_accepts_the_unit_for_an_executable_path_with_spaces_percent_and_dollar_signs() {
    require_disposable_host();
    let fake = Fake::new();
    let dir = fake.dir.path().join("a b%n$HOME");
    fs::create_dir(&dir).unwrap();
    let exe = dir.join("dstreamy");
    fs::copy(dstreamy(), &exe).unwrap();
    let unit_path = fake.dir.path().join("dstreamy.service");
    fs::write(
        &unit_path,
        stdout(&fake.run_bin(&exe, &["service", "print"])),
    )
    .unwrap();
    let verify = sh(
        "systemd-analyze",
        &["--user", "verify", unit_path.to_str().unwrap()],
    );
    assert!(
        verify.status.success(),
        "{}\n{}",
        fs::read_to_string(&unit_path).unwrap(),
        String::from_utf8_lossy(&verify.stderr)
    );
}
