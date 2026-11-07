//! `login`: the browser login window in a child `dstreamy login`, saving the token the next Discord connection reads.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use dstreamy_core::TOKEN_ENV;
use dstreamy_ipc::control::{LOGIN_ENV, LOGIN_WINDOW_LIMIT, login_env_value_ok};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::engine::{Engine, Failure, invalid_params};
use crate::login_capture::CANCELLED_EXIT;
use crate::{CliError, Ctx};

#[derive(Debug, Default, Deserialize)]
pub struct LoginParams {
    /// The client's graphical session; each name of [`LOGIN_ENV`] it lacks is unset for the window.
    #[serde(default)]
    env: Option<HashMap<String, String>>,
    /// The account the login is saved into; without it, the one named as `dstreamy login` names it.
    #[serde(default)]
    account: Option<String>,
}

impl LoginParams {
    pub fn parse(value: Value) -> Result<Self, Failure> {
        let params: Self = serde_json::from_value(value).map_err(invalid_params)?;
        for (name, value) in params.env.iter().flatten() {
            if !LOGIN_ENV.contains(&name.as_str()) {
                return Err(invalid_params(format!(
                    "env may only set {}, not {name}",
                    LOGIN_ENV.join(", ")
                )));
            }
            if !login_env_value_ok(name, value) {
                return Err(invalid_params(format!(
                    "env {name} is not a value a graphical session sets"
                )));
            }
        }
        if let Some(account) = &params.account {
            dstreamy_core::accounts::validate_name(account).map_err(invalid_params)?;
        }
        Ok(params)
    }
}

pub struct Login {
    ctx: Ctx,
    /// The `dstreamy` that opens the window; `None` without browser login.
    program: Option<PathBuf>,
    /// The token comes from the environment, which a saved login can't replace.
    env_token: bool,
    running: AtomicBool,
}

struct Running<'a>(&'a AtomicBool);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Login {
    pub fn new(ctx: &Ctx, program: Option<PathBuf>) -> Self {
        Self {
            ctx: ctx.clone(),
            program: program.or_else(own_program),
            env_token: std::env::var(TOKEN_ENV).is_ok_and(|t| !t.trim().is_empty()),
            running: AtomicBool::new(false),
        }
    }

    pub fn available(&self) -> bool {
        self.program.is_some() && !self.env_token
    }

    pub async fn run(
        &self,
        engine: &Arc<Engine>,
        params: LoginParams,
        slot: Arc<tokio::sync::OwnedSemaphorePermit>,
    ) -> Result<Value, Failure> {
        let Some(program) = &self.program else {
            return Err(Failure::new(
                "login_unavailable",
                CliError::BrowserLoginUnavailable.to_string(),
            ));
        };
        if self.env_token {
            return Err(Failure::new(
                "login_unavailable",
                format!(
                    "the service takes its token from {TOKEN_ENV}, which a login can't replace: unset it for the service and restart it"
                ),
            ));
        }
        if engine.is_live() {
            return Err(Failure::new(
                "streaming",
                "stop the stream before logging in again",
            ));
        }
        if self.running.swap(true, Ordering::AcqRel) {
            return Err(Failure::new(
                "login_running",
                "a login window is already open; finish the login there or close it",
            ));
        }
        let _running = Running(&self.running);
        let mut command = tokio::process::Command::new(program);
        command
            .arg("--json")
            .arg("--gateway-url")
            .arg(&self.ctx.gateway_url)
            .arg("--config-dir")
            .arg(&self.ctx.paths.dir)
            .args(["login", "--browser", "--window-only"])
            .env_remove(TOKEN_ENV)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(account) = &params.account {
            command.arg("--account").arg(account);
        }
        if let Some(env) = &params.env {
            for name in LOGIN_ENV {
                match env.get(name) {
                    Some(value) => command.env(name, value),
                    None => command.env_remove(name),
                };
            }
        }
        let mut child = command.spawn().map_err(|e| {
            Failure::new(
                "login_failed",
                format!(
                    "starting {}: {e}; restart the D-Streamy service",
                    program.display()
                ),
            )
        })?;
        tracing::info!("the login window is open");
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        let mut window = Window(Some(child));
        let finished = async {
            let child = window.0.as_mut().expect("the window is held until it ends");
            let (stdout, stderr, status) = tokio::join!(drain(stdout), drain(stderr), child.wait());
            status.map(|status| Output {
                status,
                stdout,
                stderr,
            })
        };
        let output = tokio::time::timeout(LOGIN_WINDOW_LIMIT, finished)
            .await
            .map_err(|_| {
                Failure::new(
                    "login_failed",
                    format!(
                        "the login window was open for {} minutes without a login, so it was closed",
                        LOGIN_WINDOW_LIMIT.as_secs() / 60
                    ),
                )
            })?
            .map_err(|e| Failure::new("login_failed", format!("waiting for the login: {e}")))?;
        window.0 = None;
        let user = outcome(&output)?;
        let account = user["account"].as_str().map(str::to_owned);
        // Detached, so a client leaving now can't cancel it and leave the old token's connection open.
        let engine = engine.clone();
        let _ = tokio::spawn(async move {
            let _slot = slot;
            engine.logged_in_as(account).await
        })
        .await;
        tracing::info!("logged in from the login window");
        Ok(user)
    }
}

/// The login child; one dropped before it ended is killed and still reaped, so no zombie is left.
struct Window(Option<tokio::process::Child>);

impl Drop for Window {
    fn drop(&mut self) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        let _ = child.start_kill();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = child.wait().await;
            });
        }
    }
}

async fn drain(pipe: Option<impl AsyncRead + Unpin>) -> Vec<u8> {
    let mut bytes = Vec::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_end(&mut bytes).await;
    }
    bytes
}

fn own_program() -> Option<PathBuf> {
    if !cfg!(all(
        feature = "browser-login",
        any(target_os = "linux", target_os = "macos")
    )) {
        return None;
    }
    match std::env::current_exe() {
        Ok(exe) => Some(exe),
        Err(e) => {
            tracing::warn!("browser login is unavailable: finding this program failed: {e}");
            None
        }
    }
}

/// The user a finished `dstreamy --json login` names, or why it failed.
fn outcome(output: &Output) -> Result<Value, Failure> {
    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout);
        let row = text
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str::<Value>(line).ok())
            .unwrap_or_default();
        return Ok(json!({
            "username": row["username"],
            "global_name": row["global_name"],
            "account": row["account"],
        }));
    }
    if output.status.code() == Some(i32::from(CANCELLED_EXIT)) {
        return Err(Failure::new(
            "login_cancelled",
            "the login window was closed before the login finished",
        ));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let reason = stderr
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("error: "))
        .map_or_else(
            || format!("dstreamy login ended with {}", output.status),
            str::to_owned,
        );
    Err(Failure::new("login_failed", reason))
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    use super::*;

    fn output(code: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    #[test]
    fn a_finished_login_names_the_user() {
        let row = r#"{"id":"1","username":"mock","global_name":"Mock","token_file":"/t","account":"mock","default":true}"#;
        assert_eq!(
            outcome(&output(0, &format!("{row}\n"), "")).unwrap(),
            json!({"username": "mock", "global_name": "Mock", "account": "mock"})
        );
    }

    #[test]
    fn a_closed_window_is_cancelled_and_other_ends_carry_the_last_error() {
        let cancelled = outcome(&output(i32::from(CANCELLED_EXIT), "", "")).unwrap_err();
        assert_eq!(cancelled.code, "login_cancelled");
        let failed = outcome(&output(
            1,
            "",
            "noise\nerror: browser login failed: no GTK container\nmore noise\n",
        ))
        .unwrap_err();
        assert_eq!(
            (failed.code, failed.message.as_str()),
            ("login_failed", "browser login failed: no GTK container")
        );
        let bare = outcome(&output(2, "", "")).unwrap_err();
        assert!(
            bare.message.starts_with("dstreamy login ended with "),
            "{}",
            bare.message
        );
    }

    #[test]
    fn unknown_parameters_are_ignored_and_env_must_be_strings() {
        assert!(LoginParams::parse(json!({"id": "1", "cmd": "login"})).is_ok());
        assert!(LoginParams::parse(json!({"cmd": "login", "env": {"DISPLAY": ":0"}})).is_ok());
        let foreign =
            LoginParams::parse(json!({"cmd": "login", "env": {"LD_PRELOAD": "x"}})).unwrap_err();
        assert_eq!(foreign.code, "invalid_params");
        assert!(
            foreign.message.contains("LD_PRELOAD"),
            "{}",
            foreign.message
        );
        let bad = LoginParams::parse(json!({"cmd": "login", "env": {"DISPLAY": 0}})).unwrap_err();
        assert_eq!(bad.code, "invalid_params");
        let exec = LoginParams::parse(json!({
            "cmd": "login",
            "env": {"DBUS_SESSION_BUS_ADDRESS": "unixexec:path=/bin/sh"},
        }))
        .unwrap_err();
        assert_eq!(exec.code, "invalid_params");
        assert!(
            exec.message.contains("DBUS_SESSION_BUS_ADDRESS"),
            "{}",
            exec.message
        );
        let relative =
            LoginParams::parse(json!({"cmd": "login", "env": {"XAUTHORITY": "auth"}})).unwrap_err();
        assert_eq!(relative.code, "invalid_params");
        let named = LoginParams::parse(json!({"cmd": "login", "account": "alt"})).unwrap();
        assert_eq!(named.account.as_deref(), Some("alt"));
        let bad = LoginParams::parse(json!({"cmd": "login", "account": "../x"})).unwrap_err();
        assert_eq!(bad.code, "invalid_params");
    }
}
