use std::path::Path;
use std::time::Duration;

use assert_cmd::Command;
use dstreamy_mock_discord::{MOCK_TOKEN, MockConfig, MockDiscord, MockUser};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::runtime::Runtime;

const T: Duration = Duration::from_secs(20);
const SECOND_TOKEN: &str = "second.account.token";

struct Harness {
    _rt: Runtime,
    first: MockDiscord,
    second: MockDiscord,
    dir: TempDir,
}

impl Harness {
    fn new() -> Self {
        let rt = Runtime::new().unwrap();
        let first = rt.block_on(MockDiscord::start()).unwrap();
        let second = rt
            .block_on(MockDiscord::start_with(second_account()))
            .unwrap();
        Self {
            _rt: rt,
            first,
            second,
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn dir(&self) -> &Path {
        self.dir.path()
    }

    fn cmd_at(&self, gateway: &str) -> Command {
        let mut cmd = Command::cargo_bin("dstreamy").unwrap();
        cmd.env_remove("DSTREAMY_TOKEN")
            .env_remove("DSTREAMY_CONFIG_DIR")
            .timeout(T)
            .arg("--config-dir")
            .arg(self.dir())
            .arg("--gateway-url")
            .arg(gateway);
        cmd
    }

    fn first(&self) -> Command {
        self.cmd_at(&self.first.gateway_url())
    }

    fn second(&self) -> Command {
        self.cmd_at(&self.second.gateway_url())
    }

    fn list(&self) -> Value {
        json_out(self.first().args(["--json", "account", "list"]))
    }

    fn write_legacy(&self, token: &str) {
        let path = self.dir().join("token");
        std::fs::write(&path, format!("{token}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
}

pub fn second_account() -> MockConfig {
    MockConfig {
        token: SECOND_TOKEN.to_owned(),
        user: MockUser {
            id: "100000000000000222".to_owned(),
            username: "second".to_owned(),
            global_name: Some("Second One".to_owned()),
        },
        ..MockConfig::default()
    }
}

fn ok(cmd: &mut Command) -> (String, String) {
    let out = cmd.output().unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "{stderr}");
    (String::from_utf8(out.stdout).unwrap(), stderr)
}

fn failed(cmd: &mut Command) -> String {
    let out = cmd.output().unwrap();
    assert!(!out.status.success());
    String::from_utf8(out.stderr).unwrap()
}

fn json_out(cmd: &mut Command) -> Value {
    serde_json::from_str(&ok(cmd).0).unwrap()
}

fn names(list: &Value) -> Vec<&str> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect()
}

#[test]
fn logins_are_named_after_the_user_and_each_account_keeps_its_token() {
    let h = Harness::new();
    ok(h.first().arg("login").write_stdin(MOCK_TOKEN));
    let (text, notes) = ok(h.second().arg("login").write_stdin(SECOND_TOKEN));
    assert!(text.contains("saved as account second"), "{text}");
    assert!(notes.contains("dstreamy account use second"), "{notes}");

    assert_eq!(
        h.list(),
        json!([
            {"name": "mockuser", "username": "mockuser", "global_name": "Mock User", "default": true, "has_token": true},
            {"name": "second", "username": "second", "global_name": "Second One", "default": false, "has_token": true},
        ])
    );
    let (table, _) = ok(h.first().args(["account", "list"]));
    assert!(
        table
            .lines()
            .any(|l| l.starts_with('*') && l.contains("mockuser")),
        "{table}"
    );

    ok(h.first().arg("guilds"));
    let rejected = failed(h.second().arg("guilds"));
    assert!(rejected.contains("4004"), "{rejected}");
    ok(h.second().args(["--account", "second", "guilds"]));

    let (text, _) = ok(h.first().args(["account", "use", "second"]));
    assert!(text.contains("second is now the default"), "{text}");
    ok(h.second().arg("guilds"));
    ok(h.first().args(["--account", "mockuser", "guilds"]));
}

#[test]
fn a_named_login_keeps_its_name_and_logging_in_again_reuses_it() {
    let h = Harness::new();
    let row = json_out(
        h.first()
            .args(["--json", "--account", "alt", "login"])
            .write_stdin(MOCK_TOKEN),
    );
    assert_eq!(
        (&row["account"], &row["default"]),
        (&json!("alt"), &json!(true))
    );
    let row = json_out(h.first().args(["--json", "login"]).write_stdin(MOCK_TOKEN));
    assert_eq!(row["account"], "alt");
    assert_eq!(names(&h.list()), ["alt"]);
}

#[test]
fn an_unknown_or_invalid_account_says_what_exists() {
    let h = Harness::new();
    let err = failed(h.first().args(["--account", "nope", "guilds"]));
    assert!(
        err.contains("no account is saved: run `dstreamy login`"),
        "{err}"
    );
    ok(h.first().arg("login").write_stdin(MOCK_TOKEN));
    let err = failed(h.first().args(["--account", "nope", "guilds"]));
    assert!(
        err.contains("unknown account \"nope\"; saved accounts: mockuser"),
        "{err}"
    );
    let out = h
        .first()
        .args(["--account", "Bad/Name", "guilds"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = failed(h.first().args(["account", "use", "nope"]));
    assert!(err.contains("saved accounts: mockuser"), "{err}");
}

#[test]
fn dstreamy_token_wins_and_refuses_a_chosen_account() {
    let h = Harness::new();
    ok(h.second().arg("login").write_stdin(SECOND_TOKEN));
    ok(h.first().arg("guilds").env("DSTREAMY_TOKEN", MOCK_TOKEN));
    let err = failed(
        h.first()
            .args(["--account", "second", "guilds"])
            .env("DSTREAMY_TOKEN", MOCK_TOKEN),
    );
    assert!(err.contains("DSTREAMY_TOKEN"), "{err}");
    let (_, notes) = ok(h
        .first()
        .args(["account", "list"])
        .env("DSTREAMY_TOKEN", MOCK_TOKEN));
    assert!(notes.contains("DSTREAMY_TOKEN is set"), "{notes}");
}

#[test]
fn removing_an_account_that_streams_is_refused() {
    let h = Harness::new();
    ok(h.first().arg("login").write_stdin(MOCK_TOKEN));
    ok(h.second().arg("login").write_stdin(SECOND_TOKEN));
    let held =
        dstreamy_cli::lock::SessionLock::acquire(&h.dir().join("accounts/mockuser")).unwrap();
    let err = failed(h.first().args(["account", "remove", "mockuser"]));
    assert!(
        err.contains(&format!(
            "account mockuser is streaming (pid {})",
            std::process::id()
        )),
        "{err}"
    );
    drop(held);
    let (text, notes) = ok(h.first().args(["account", "remove", "mockuser"]));
    assert!(text.contains("removed account mockuser"), "{text}");
    assert!(notes.contains("the default account is second"), "{notes}");
    let (_, notes) = ok(h.first().args(["account", "remove", "second"]));
    assert!(notes.contains("no account is left"), "{notes}");
    assert_eq!(h.list(), json!([]));
    let err = failed(h.first().arg("guilds"));
    assert!(err.contains("not logged in"), "{err}");
}

#[test]
fn the_legacy_token_moves_into_an_account_named_after_its_user() {
    let h = Harness::new();
    h.write_legacy(MOCK_TOKEN);
    let list = h.list();
    assert_eq!(names(&list), ["mockuser"]);
    assert_eq!(list[0]["default"], true);
    assert!(!h.dir().join("token").exists());
    assert_eq!(
        std::fs::read_to_string(h.dir().join("accounts/mockuser/token"))
            .unwrap()
            .trim(),
        MOCK_TOKEN
    );
    ok(h.first().arg("guilds"));
}

#[test]
fn an_offline_migration_names_the_account_default_and_learns_the_user_later() {
    let h = Harness::new();
    h.write_legacy(MOCK_TOKEN);
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let offline = format!("ws://{}", closed.local_addr().unwrap());
    drop(closed);
    let out = h
        .cmd_at(&offline)
        .args(["--json", "account", "list"])
        .timeout(Duration::from_secs(60))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let list: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(list[0]["name"], "default");
    assert_eq!(list[0]["username"], Value::Null);
    assert!(!h.dir().join("token").exists());
    ok(h.first().arg("guilds"));
    assert_eq!(h.list()[0]["username"], "mockuser");
}

#[test]
fn a_failed_migration_keeps_the_legacy_token_working() {
    let h = Harness::new();
    h.write_legacy(MOCK_TOKEN);
    std::fs::write(h.dir().join("accounts"), "in the way").unwrap();
    let (_, warnings) = ok(h.first().arg("guilds"));
    assert!(warnings.contains("the saved token stays in"), "{warnings}");
    assert_eq!(
        std::fs::read_to_string(h.dir().join("token"))
            .unwrap()
            .trim(),
        MOCK_TOKEN
    );
    std::fs::remove_file(h.dir().join("accounts")).unwrap();
    assert_eq!(names(&h.list()), ["mockuser"]);
    assert!(!h.dir().join("token").exists());
}
