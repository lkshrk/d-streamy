#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

use dstreamy_cli::service::systemd::{exec_path, user_unit_dir};
use serde_json::{Value, json};
use tempfile::TempDir;

const GOLDEN: &str = include_str!("fixtures/service/dstreamy.service");
const NOT_FOUND: &str =
    "LoadState=not-found\nActiveState=inactive\nSubState=dead\nUnitFileState=\nMainPID=0\n";

/// A running unit whose main process is `pid`, started long after any file the test writes.
fn running(pid: u32) -> String {
    running_since(pid, "@9999999999", "no")
}

fn running_since(pid: u32, started: &str, need_reload: &str) -> String {
    format!(
        "LoadState=loaded\nActiveState=active\nSubState=running\nUnitFileState=enabled\nMainPID={pid}\n\
         NeedDaemonReload={need_reload}\nExecMainStartTimestamp={started}\n"
    )
}

fn dstreamy() -> PathBuf {
    assert_cmd::cargo::cargo_bin("dstreamy")
}

const DEFAULT_SCRIPT: &str = "case \"$2\" in\n\
               disable) [ -f \"$XDG_CONFIG_HOME/systemd/user/dstreamy.service\" ] || \
                 { echo 'Failed to disable unit: Unit file dstreamy.service does not exist.' >&2; exit 1; } ;;\n\
               show-environment) cat \"$d/environment\" 2>/dev/null ;;\n\
               show) case \"$3\" in\n\
                 --property=UnitPath) cat \"$d/unitpath\" ;;\n\
                 *) cat \"$d/show\" 2>/dev/null ;;\n\
               esac ;;\n\
             esac\n";

/// A temp home whose `systemctl` records its arguments and answers queries from files.
struct Fake {
    dir: TempDir,
}

impl Fake {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let fake = Self { dir };
        fake.default_script();
        fake.set_unit_path(&[&fake.unit_dir()]);
        fake.answer_show(NOT_FOUND);
        fake
    }

    fn default_script(&self) {
        self.script(DEFAULT_SCRIPT);
    }

    fn script(&self, body: &str) {
        let script = self.systemctl();
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nd=$(dirname \"$0\")\nprintf '%s\\n' \"$*\" >> \"$d/calls\"\n{body}"
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn set_unit_path(&self, dirs: &[&Path]) {
        let dirs: Vec<_> = dirs.iter().map(|d| d.display().to_string()).collect();
        fs::write(
            self.dir.path().join("unitpath"),
            format!(
                "UnitPath=/x/systemd/user.control {} /etc/systemd/user\n",
                dirs.join(" ")
            ),
        )
        .unwrap();
    }

    fn answer_show(&self, show: &str) {
        fs::write(self.dir.path().join("show"), show).unwrap();
    }

    fn systemctl(&self) -> PathBuf {
        self.dir.path().join("systemctl")
    }

    fn unit_dir(&self) -> PathBuf {
        self.dir.path().join("config/systemd/user")
    }

    fn unit_path(&self) -> PathBuf {
        self.unit_dir().join("dstreamy.service")
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.dir.path().join("calls")).unwrap_or_default()
    }

    /// The calls that change something, without the read-only queries.
    fn actions(&self) -> String {
        self.calls()
            .lines()
            .filter(|line| !line.starts_with("--user show"))
            .map(|line| format!("{line}\n"))
            .collect()
    }

    fn command(&self, bin: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new(bin);
        cmd.env("HOME", self.dir.path())
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .args(args)
            .arg("--systemctl")
            .arg(self.systemctl());
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(&dstreamy(), args).output().unwrap()
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

fn failure(output: &Output) -> String {
    assert!(!output.status.success(), "unexpected success");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn golden_for(exe: &Path) -> String {
    GOLDEN.replace("/usr/local/bin/dstreamy", &exec_path(exe).unwrap())
}

#[test]
fn print_shows_the_unit_for_this_executable_as_text_or_json() {
    let fake = Fake::new();
    assert_eq!(
        stdout(&fake.run(&["service", "print"])),
        golden_for(&dstreamy())
    );
    let v: Value =
        serde_json::from_str(&stdout(&fake.run(&["--json", "service", "print"]))).unwrap();
    assert_eq!(
        v,
        json!({"path": fake.unit_path(), "contents": golden_for(&dstreamy())})
    );
    assert_eq!(fake.actions(), "");
}

#[test]
fn install_dry_run_prints_the_file_and_the_exact_commands_and_runs_nothing() {
    let fake = Fake::new();
    let expected = format!(
        "would write {}:\n{}would run: {s} --user daemon-reload\n\
         would run: {s} --user try-restart dstreamy.service\n\
         would run: {s} --user enable --now dstreamy.service\n",
        fake.unit_path().display(),
        golden_for(&dstreamy()),
        s = fake.systemctl().display(),
    );
    assert_eq!(
        stdout(&fake.run(&["service", "install", "--dry-run"])),
        expected
    );
    assert_eq!(fake.actions(), "");
    assert!(!fake.unit_path().exists());
}

#[test]
fn install_dry_run_as_json_lists_the_steps() {
    let fake = Fake::new();
    let v: Value = serde_json::from_str(&stdout(&fake.run(&[
        "--json",
        "service",
        "install",
        "--dry-run",
    ])))
    .unwrap();
    let s = fake.systemctl();
    assert_eq!(
        v,
        json!({"dry_run": true, "steps": [
            {"write": fake.unit_path(), "contents": golden_for(&dstreamy())},
            {"run": [s, "--user", "daemon-reload"]},
            {"run": [s, "--user", "try-restart", "dstreamy.service"]},
            {"run": [s, "--user", "enable", "--now", "dstreamy.service"]},
        ]})
    );
    assert_eq!(fake.actions(), "");
}

#[test]
fn uninstall_dry_run_prints_the_exact_commands_and_runs_nothing() {
    let fake = Fake::new();
    assert_eq!(
        stdout(&fake.run(&["service", "uninstall", "--dry-run"])),
        format!(
            "dstreamy.service is not installed ({} does not exist)\n",
            fake.unit_path().display()
        )
    );
    fs::create_dir_all(fake.unit_dir()).unwrap();
    fs::write(fake.unit_path(), GOLDEN).unwrap();
    let expected = format!(
        "would run: {s} --user disable --now dstreamy.service\nwould remove {}\nwould run: {s} --user daemon-reload\n",
        fake.unit_path().display(),
        s = fake.systemctl().display(),
    );
    assert_eq!(
        stdout(&fake.run(&["service", "uninstall", "--dry-run"])),
        expected
    );
    assert_eq!(fake.actions(), "");
}

#[test]
fn install_writes_the_unit_then_reloads_and_enables_it_and_uninstall_reverses_it() {
    let fake = Fake::new();
    let mut cmd = Command::new("sh");
    cmd.env("HOME", fake.dir.path())
        .env("XDG_CONFIG_HOME", fake.dir.path().join("config"))
        .args(["-c", "umask 077 && exec \"$0\" \"$@\""])
        .arg(dstreamy())
        .args(["service", "install", "--systemctl"])
        .arg(fake.systemctl());
    let out = stdout(&cmd.output().unwrap());
    assert!(out.contains("is enabled and started"), "{out}");
    assert_eq!(
        fs::read_to_string(fake.unit_path()).unwrap(),
        golden_for(&dstreamy())
    );
    let mode = fs::metadata(fake.unit_path()).unwrap().permissions().mode();
    assert_eq!(mode & 0o7777, 0o644);
    assert_eq!(
        fake.actions(),
        "--user daemon-reload\n--user try-restart dstreamy.service\n--user enable --now dstreamy.service\n"
    );

    fake.answer_show(&running(std::process::id()));
    let v: Value =
        serde_json::from_str(&stdout(&fake.run(&["--json", "service", "uninstall"]))).unwrap();
    assert_eq!(v["action"], "uninstalled", "{v}");
    assert!(!fake.unit_path().exists());
    assert_eq!(
        fake.actions(),
        "--user daemon-reload\n--user try-restart dstreamy.service\n--user enable --now dstreamy.service\n\
         --user disable --now dstreamy.service\n--user daemon-reload\n"
    );
}

#[test]
fn an_unchanged_reinstall_over_a_running_service_does_not_restart_it() {
    let fake = Fake::new();
    stdout(&fake.run(&["service", "install"]));
    fake.answer_show(&running(std::process::id()));
    fs::remove_file(fake.dir.path().join("calls")).unwrap();
    let v: Value =
        serde_json::from_str(&stdout(&fake.run(&["--json", "service", "install"]))).unwrap();
    assert_eq!(v["action"], "unchanged", "{v}");
    assert_eq!(fake.actions(), "--user enable --now dstreamy.service\n");
    let text = stdout(&fake.run(&["service", "install"]));
    assert!(text.contains("is unchanged"), "{text}");
}

#[test]
fn a_changed_reinstall_over_a_running_service_restarts_it() {
    let fake = Fake::new();
    fs::create_dir_all(fake.unit_dir()).unwrap();
    fs::write(
        fake.unit_path(),
        "[Service]\nExecStart=/old/dstreamy serve\n",
    )
    .unwrap();
    fake.answer_show(&running(std::process::id()));
    let v: Value =
        serde_json::from_str(&stdout(&fake.run(&["--json", "service", "install"]))).unwrap();
    assert_eq!(v["action"], "restarted", "{v}");
    assert_eq!(
        fake.actions(),
        "--user daemon-reload\n--user try-restart dstreamy.service\n--user enable --now dstreamy.service\n"
    );
    assert_eq!(
        fs::read_to_string(fake.unit_path()).unwrap(),
        golden_for(&dstreamy())
    );
}

#[test]
fn install_refuses_a_symlink_at_the_unit_path() {
    let fake = Fake::new();
    let theirs = fake.dir.path().join("dotfiles.service");
    fs::write(&theirs, "theirs").unwrap();
    fs::create_dir_all(fake.unit_dir()).unwrap();
    std::os::unix::fs::symlink(&theirs, fake.unit_path()).unwrap();
    let stderr = failure(&fake.run(&["service", "install"]));
    assert!(stderr.contains("is a symbolic link"), "{stderr}");
    assert_eq!(fs::read_to_string(&theirs).unwrap(), "theirs");
    assert!(
        fs::symlink_metadata(fake.unit_path())
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fake.actions(), "");
}

#[test]
fn install_refuses_a_unit_directory_the_manager_does_not_search() {
    let fake = Fake::new();
    fake.set_unit_path(&[Path::new("/home/someone/.config/systemd/user")]);
    let stderr = failure(&fake.run(&["service", "install"]));
    assert!(stderr.contains("does not load units from"), "{stderr}");
    assert!(
        stderr.contains("/home/someone/.config/systemd/user"),
        "{stderr}"
    );
    assert!(!fake.unit_path().exists());
    assert_eq!(fake.actions(), "");
    let dry = fake.run(&["service", "install", "--dry-run"]);
    stdout(&dry);
    let stderr = String::from_utf8_lossy(&dry.stderr);
    assert!(
        stderr.contains("a real run would fail") && stderr.contains("does not load units from"),
        "{stderr}"
    );
    let stderr = failure(&fake.run(&["service", "uninstall"]));
    assert!(stderr.contains("does not load units from"), "{stderr}");
}

#[test]
fn uninstall_without_the_file_stops_a_unit_the_manager_knows_and_removes_its_link() {
    let fake = Fake::new();
    let v: Value =
        serde_json::from_str(&stdout(&fake.run(&["--json", "service", "uninstall"]))).unwrap();
    assert_eq!(v["action"], "absent", "{v}");
    assert_eq!(fake.actions(), "");

    let wants = fake.unit_dir().join("default.target.wants");
    fs::create_dir_all(&wants).unwrap();
    std::os::unix::fs::symlink(fake.unit_path(), wants.join("dstreamy.service")).unwrap();
    fake.answer_show(&running(std::process::id()));
    let plan = stdout(&fake.run(&["service", "uninstall", "--dry-run"]));
    assert_eq!(
        plan,
        format!(
            "would run: {s} --user stop dstreamy.service\nwould run: {s} --user reset-failed dstreamy.service\n\
             would remove {}\nwould run: {s} --user daemon-reload\n",
            wants.join("dstreamy.service").display(),
            s = fake.systemctl().display()
        )
    );
    assert_eq!(fake.actions(), "");
    let out = stdout(&fake.run(&["service", "uninstall"]));
    assert!(out.contains("was already gone"), "{out}");
    assert_eq!(
        fake.actions(),
        "--user stop dstreamy.service\n--user reset-failed dstreamy.service\n--user daemon-reload\n"
    );
    assert!(fs::symlink_metadata(wants.join("dstreamy.service")).is_err());
}

#[test]
fn uninstall_removes_a_dangling_enablement_link_left_without_a_loaded_unit() {
    let fake = Fake::new();
    let wants = fake.unit_dir().join("default.target.wants");
    fs::create_dir_all(&wants).unwrap();
    std::os::unix::fs::symlink(fake.unit_path(), wants.join("dstreamy.service")).unwrap();
    stdout(&fake.run(&["service", "uninstall"]));
    assert_eq!(fake.actions(), "--user daemon-reload\n");
    assert!(fs::symlink_metadata(wants.join("dstreamy.service")).is_err());
}

fn reinstall_over(fake: &Fake, show: &str) -> Value {
    stdout(&fake.run(&["service", "install"]));
    fake.answer_show(show);
    fs::remove_file(fake.dir.path().join("calls")).unwrap();
    serde_json::from_str(&stdout(&fake.run(&["--json", "service", "install"]))).unwrap()
}

const RESTART: &str = "--user daemon-reload\n--user try-restart dstreamy.service\n--user enable --now dstreamy.service\n";

#[test]
fn a_rerun_after_a_failed_reload_restarts_the_service_that_needs_one() {
    let fake = Fake::new();
    fake.script("case \"$2\" in daemon-reload) exit 1 ;; show) cat \"$d/unitpath\" ;; esac\n");
    failure(&fake.run(&["service", "install"]));
    fake.default_script();
    fake.answer_show(&running_since(std::process::id(), "@9999999999", "yes"));
    fs::remove_file(fake.dir.path().join("calls")).unwrap();
    let v: Value =
        serde_json::from_str(&stdout(&fake.run(&["--json", "service", "install"]))).unwrap();
    assert_eq!(v["action"], "restarted", "{v}");
    assert_eq!(v["written"], true, "{v}");
    assert_eq!(fake.actions(), RESTART);
    let text = stdout(&fake.run(&["service", "install"]));
    assert!(
        text.contains("is unchanged") && text.contains("outdated definition"),
        "{text}"
    );
}

#[test]
fn a_service_started_before_its_unit_file_was_written_is_restarted() {
    let fake = Fake::new();
    let v = reinstall_over(&fake, &running_since(std::process::id(), "@1", "no"));
    assert_eq!(v["action"], "restarted", "{v}");
    assert_eq!(fake.actions(), RESTART);
}

#[test]
fn a_service_whose_binary_was_replaced_is_restarted() {
    let fake = Fake::new();
    let bin = fake.dir.path().join("old-dstreamy");
    fs::copy("/bin/sleep", &bin).unwrap();
    let old = loop {
        // A concurrent test's fork can briefly hold the copy open for writing (ETXTBSY).
        match Command::new(&bin).arg("30").spawn() {
            Ok(child) => break KillOnDrop(child),
            Err(e) if e.raw_os_error() == Some(26) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => panic!("{e}"),
        }
    };
    fs::remove_file(&bin).unwrap();
    let v = reinstall_over(&fake, &running(old.0.id()));
    assert_eq!(v["action"], "restarted", "{v}");
    assert_eq!(fake.actions(), RESTART);
}

#[test]
fn a_failing_systemctl_fails_install_and_status_reports_it() {
    let fake = Fake::new();
    fake.script("echo 'Failed to connect to bus' >&2\nexit 1\n");
    let stderr = failure(&fake.run(&["service", "install"]));
    assert!(stderr.contains("Failed to connect to bus"), "{stderr}");
    assert!(!fake.unit_path().exists());

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
    let manager = v["manager"].as_str().unwrap();
    assert!(
        manager.contains("show") && manager.contains("Failed to connect to bus"),
        "{v}"
    );
    assert_eq!(v["hello"], false, "{v}");
}

#[test]
fn a_failed_enable_says_the_unit_is_written_and_a_retry_is_safe() {
    let fake = Fake::new();
    fake.script(
        "case \"$2\" in\n\
           show) case \"$3\" in --property=UnitPath) cat \"$d/unitpath\" ;; *) cat \"$d/show\" ;; esac ;;\n\
           enable) echo 'Unit file dstreamy.service does not exist.' >&2; exit 1 ;;\n\
         esac\n",
    );
    let stderr = failure(&fake.run(&["service", "install"]));
    assert!(stderr.contains("does not exist."), "{stderr}");
    assert!(
        stderr.contains("is written, but enabling or starting the service did not finish"),
        "{stderr}"
    );
    assert!(stderr.contains("again is safe"), "{stderr}");
    assert!(fake.unit_path().exists());
}

#[test]
fn status_gives_up_on_a_hung_systemctl() {
    let fake = Fake::new();
    fake.script("exec sleep 60\n");
    let started = Instant::now();
    let output = fake.run(&[
        "--json",
        "service",
        "status",
        "--endpoint",
        "/nonexistent/s.sock",
    ]);
    assert!(started.elapsed() < Duration::from_secs(15));
    assert!(!output.status.success());
    let v: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        v["manager"]
            .as_str()
            .unwrap()
            .contains("did not finish within"),
        "{v}"
    );
}

/// Kills the child when the test ends, however it ends.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn status_finds_the_socket_under_the_services_runtime_directory() {
    let fake = Fake::new();
    fake.answer_show(&running(std::process::id()));
    let runtime = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    fs::write(
        fake.dir.path().join("environment"),
        format!("HOME=/x\nXDG_RUNTIME_DIR={}\n", runtime.path().display()),
    )
    .unwrap();
    let status = |fake: &Fake| {
        let output = fake
            .command(&dstreamy(), &["--json", "service", "status"])
            .env("XDG_RUNTIME_DIR", fake.dir.path())
            .output()
            .unwrap();
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

    let endpoint = runtime.path().join("dstreamy/serve.sock");
    let _serve = KillOnDrop(
        Command::new(dstreamy())
            .arg("--config-dir")
            .arg(fake.dir.path().join("dstreamy"))
            .args(["serve", "--skip-platform", "--endpoint"])
            .arg(&endpoint)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    let v = loop {
        let (ok, v) = status(&fake);
        if ok {
            break v;
        }
        assert!(Instant::now() < deadline, "status never healthy: {v}");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(v["installed"], true, "{v}");
    assert_eq!(v["enabled"], true, "{v}");
    assert_eq!(v["hello"], true, "{v}");
    assert_eq!(v["endpoint"], json!(endpoint), "{v}");
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"), "{v}");
    assert_eq!(
        v["manager"],
        format!(
            "loaded, active (running), enabled, pid {}",
            std::process::id()
        ),
        "{v}"
    );
    let note = v["runtime_dir_note"].as_str().unwrap();
    assert!(note.contains(&runtime.path().display().to_string()), "{v}");
    assert!(
        fake.calls().ends_with(
            "--user show --property=UnitPath\n\
             --user show --timestamp=unix --property=LoadState,ActiveState,SubState,UnitFileState,MainPID,NeedDaemonReload,ExecMainStartTimestamp dstreamy.service\n\
             --user show-environment\n"
        ),
        "{}",
        fake.calls()
    );
}

#[test]
fn status_reports_an_unreadable_service_runtime_dir_and_uses_the_home_fallback_without_one() {
    let fake = Fake::new();
    let shell_runtime = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let status = |fake: &Fake| -> Value {
        let output = fake
            .command(&dstreamy(), &["--json", "service", "status"])
            .env("XDG_RUNTIME_DIR", shell_runtime.path())
            .output()
            .unwrap();
        serde_json::from_slice(&output.stdout).unwrap()
    };
    let environment = fake.dir.path().join("environment");

    fs::write(&environment, "XDG_RUNTIME_DIR=$'/run/user/1\\x20x'\n").unwrap();
    let v = status(&fake);
    let note = v["runtime_dir_note"].as_str().unwrap_or_default();
    assert!(
        note.contains("could not read the service's XDG_RUNTIME_DIR"),
        "{v}"
    );
    assert!(note.contains("it can decode"), "{v}");

    fs::write(&environment, "HOME=/x\n").unwrap();
    let v = status(&fake);
    let endpoint = v["endpoint"].as_str().unwrap();
    assert!(
        !endpoint.starts_with(&shell_runtime.path().display().to_string()),
        "{v}"
    );
    assert!(endpoint.ends_with("/dstreamy/serve.sock"), "{v}");
}

#[test]
fn a_relative_config_directory_is_refused() {
    let fake = Fake::new();
    let output = fake
        .command(&dstreamy(), &["service", "install", "--dry-run"])
        .env("XDG_CONFIG_HOME", "")
        .output()
        .unwrap();
    let stderr = failure(&output);
    assert!(stderr.contains("is not absolute"), "{stderr}");
}

fn copy_of_dstreamy_in(dir: &Path) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let exe = dir.join("dstreamy");
    fs::copy(dstreamy(), &exe).unwrap();
    exe
}

#[test]
fn an_executable_path_with_spaces_and_percent_signs_is_quoted_and_flagged_as_temporary() {
    let fake = Fake::new();
    let exe = copy_of_dstreamy_in(&fake.dir.path().join("my apps 100%"));
    let output = fake.command(&exe, &["service", "print"]).output().unwrap();
    let unit = stdout(&output);
    let exec_start = format!(
        "ExecStart=\"{}/my apps 100%%/dstreamy\" serve\n",
        fake.dir.path().display()
    );
    assert!(unit.contains(&exec_start), "{unit}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("is in a temporary directory"), "{stderr}");
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
            .output();
    }
}

fn install() -> Value {
    let output = Command::new(dstreamy())
        .args(["--json", "service", "install"])
        .output()
        .unwrap();
    serde_json::from_str(&stdout(&output)).unwrap()
}

#[test]
#[ignore = "installs a real systemd user unit; needs DSTREAMY_DISPOSABLE_HOST=1"]
fn a_real_install_survives_sigkill_and_a_user_manager_restart() {
    require_disposable_host();
    wait_user_manager();
    let _cleanup = Uninstall;

    let unit_path = user_unit_dir().unwrap().join("dstreamy.service");
    assert_eq!(install()["action"], "started");
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

    assert_eq!(install()["action"], "unchanged");
    assert_eq!(
        main_pid(),
        first,
        "an unchanged reinstall restarted the service"
    );

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
    let third = wait_healthy(Duration::from_secs(30), second);

    let mut changed = fs::read_to_string(&unit_path).unwrap();
    changed.push_str("# edited\n");
    fs::write(&unit_path, changed).unwrap();
    assert_eq!(install()["action"], "restarted");
    wait_healthy(Duration::from_secs(15), third);

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
fn systemd_accepts_the_unit_for_an_executable_path_with_spaces_and_percent_signs() {
    require_disposable_host();
    let fake = Fake::new();
    let exe = copy_of_dstreamy_in(&fake.dir.path().join("a b%n"));
    let unit_path = fake.dir.path().join("dstreamy.service");
    fs::write(
        &unit_path,
        stdout(&fake.command(&exe, &["service", "print"]).output().unwrap()),
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
