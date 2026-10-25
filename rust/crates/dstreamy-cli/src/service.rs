//! `dstreamy service`: keep `dstreamy serve` running under the platform's user-level service manager.

#[cfg(target_os = "linux")]
pub mod systemd;

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Output};
use std::time::Duration;

use dstreamy_ipc::{ControlCodec, Endpoint};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_util::codec::Framed;

use crate::CliError;
use crate::output::{Out, table};

const HELLO_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, clap::Args)]
pub struct ServiceArgs {
    #[command(subcommand)]
    pub action: Action,
    #[arg(
        long,
        global = true,
        hide = true,
        value_name = "PATH",
        default_value = "systemctl"
    )]
    pub systemctl: PathBuf,
}

#[derive(Debug, clap::Subcommand)]
pub enum Action {
    /// Write the service definition, then enable and start the service
    Install {
        /// Print the file and the commands instead of running them
        #[arg(long)]
        dry_run: bool,
    },
    /// Stop and disable the service, then remove its definition
    Uninstall {
        /// Print the commands instead of running them
        #[arg(long)]
        dry_run: bool,
    },
    /// Report whether the service is installed and running, and whether its socket answers
    Status {
        /// Socket path; defaults to the one `dstreamy serve` uses
        #[arg(long, value_name = "PATH")]
        endpoint: Option<PathBuf>,
    },
    /// Print the service definition `install` would write
    Print,
}

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("`dstreamy service` is not supported on this platform yet")]
    Unsupported,
    #[error("finding the dstreamy executable: {0}")]
    CurrentExe(io::Error),
    #[error("the dstreamy path {0} is not valid UTF-8")]
    NotUtf8(PathBuf),
    #[error(
        "the dstreamy path {path} contains {found:?}, which a service definition cannot express"
    )]
    UnsupportedPath { path: PathBuf, found: char },
    #[error("no home directory to place the service definition in")]
    NoHome,
    #[error(
        "the user configuration directory {0:?} is not absolute; set XDG_CONFIG_HOME or HOME to an absolute path"
    )]
    RelativeConfigDir(PathBuf),
    #[error("{action} {path}: {source}")]
    File {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    #[error("running {command}: {source}")]
    Spawn { command: String, source: io::Error },
    #[error("{command} failed ({status})")]
    Failed { command: String, status: ExitStatus },
}

pub enum Step {
    Write { path: PathBuf, contents: String },
    Remove(PathBuf),
    Run(Vec<OsString>),
}

/// What the service manager says about the service.
pub struct Supervision {
    pub enabled: bool,
    pub running: bool,
    pub detail: String,
}

pub trait Backend {
    /// The service's name in the manager.
    fn name(&self) -> &str;
    fn definition_path(&self) -> &Path;
    fn definition(&self) -> &str;
    fn install(&self) -> Vec<Step>;
    fn uninstall(&self) -> Vec<Step>;
    fn query(&self) -> Result<Supervision, ServiceError>;
}

pub async fn run(out: &Out, args: ServiceArgs) -> Result<ExitCode, CliError> {
    let backend = backend(args.systemctl)?;
    let backend = backend.as_ref();
    match args.action {
        Action::Print => {
            out.raw(backend.definition());
            Ok(ExitCode::SUCCESS)
        }
        Action::Install { dry_run } => {
            let steps = backend.install();
            if dry_run {
                print_plan(out, &steps);
            } else {
                apply(&steps)?;
                out.line(format_args!(
                    "installed {}; {} is enabled and started",
                    backend.definition_path().display(),
                    backend.name()
                ));
            }
            Ok(ExitCode::SUCCESS)
        }
        Action::Uninstall { dry_run } => {
            if !dry_run && !backend.definition_path().exists() {
                out.line(format_args!(
                    "{} is not installed ({} does not exist)",
                    backend.name(),
                    backend.definition_path().display()
                ));
                return Ok(ExitCode::SUCCESS);
            }
            let steps = backend.uninstall();
            if dry_run {
                print_plan(out, &steps);
            } else {
                apply(&steps)?;
                out.line(format_args!(
                    "uninstalled {}; removed {}",
                    backend.name(),
                    backend.definition_path().display()
                ));
            }
            Ok(ExitCode::SUCCESS)
        }
        Action::Status { endpoint } => {
            let endpoint = match endpoint {
                Some(path) => Endpoint::new(path),
                None => Endpoint::default_for_user()?,
            };
            status(out, backend, &endpoint).await
        }
    }
}

#[cfg(target_os = "linux")]
fn backend(systemctl: PathBuf) -> Result<Box<dyn Backend>, ServiceError> {
    let exe = std::env::current_exe().map_err(ServiceError::CurrentExe)?;
    Ok(Box::new(systemd::Systemd::new(systemctl, &exe)?))
}

#[cfg(not(target_os = "linux"))]
fn backend(_systemctl: PathBuf) -> Result<Box<dyn Backend>, ServiceError> {
    Err(ServiceError::Unsupported)
}

pub fn apply(steps: &[Step]) -> Result<(), ServiceError> {
    for step in steps {
        match step {
            Step::Write { path, contents } => {
                let file_err = |action| {
                    move |source| ServiceError::File {
                        action,
                        path: path.clone(),
                        source,
                    }
                };
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir).map_err(file_err("creating the directory of"))?;
                }
                std::fs::write(path, contents).map_err(file_err("writing"))?;
            }
            Step::Remove(path) => match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(ServiceError::File {
                        action: "removing",
                        path: path.clone(),
                        source,
                    });
                }
            },
            Step::Run(argv) => {
                let output = output(argv)?;
                if !output.status.success() {
                    out_stderr(&output);
                    return Err(ServiceError::Failed {
                        command: shell_words(argv),
                        status: output.status,
                    });
                }
            }
        }
    }
    Ok(())
}

fn out_stderr(output: &Output) {
    let text = String::from_utf8_lossy(&output.stderr);
    if !text.trim().is_empty() {
        Out::new(false).err_raw(&text);
    }
}

/// Runs `argv` to completion, capturing its output.
pub fn output(argv: &[OsString]) -> Result<Output, ServiceError> {
    let (program, args) = argv.split_first().expect("a command has a program");
    Command::new(program)
        .args(args)
        .output()
        .map_err(|source| ServiceError::Spawn {
            command: shell_words(argv),
            source,
        })
}

fn print_plan(out: &Out, steps: &[Step]) {
    if out.json {
        let steps: Vec<Value> = steps
            .iter()
            .map(|step| match step {
                Step::Write { path, contents } => json!({"write": path, "contents": contents}),
                Step::Remove(path) => json!({"remove": path}),
                Step::Run(argv) => {
                    let argv: Vec<_> = argv.iter().map(|a| a.to_string_lossy()).collect();
                    json!({ "run": argv })
                }
            })
            .collect();
        out.json_line(&json!({"dry_run": true, "steps": steps}));
        return;
    }
    let mut text = String::new();
    for step in steps {
        match step {
            Step::Write { path, contents } => {
                text.push_str(&format!("would write {}:\n{contents}", path.display()));
            }
            Step::Remove(path) => text.push_str(&format!("would remove {}\n", path.display())),
            Step::Run(argv) => text.push_str(&format!("would run: {}\n", shell_words(argv))),
        }
    }
    out.raw(&text);
}

/// `argv` as a POSIX shell would need it typed.
pub fn shell_words(argv: &[OsString]) -> String {
    argv.iter()
        .map(|arg| shell_word(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_word(arg: &OsStr) -> String {
    let text = arg.to_string_lossy();
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if plain {
        text.into_owned()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

async fn status(
    out: &Out,
    backend: &dyn Backend,
    endpoint: &Endpoint,
) -> Result<ExitCode, CliError> {
    let installed = backend.definition_path().exists();
    let manager = backend.query().unwrap_or_else(|e| Supervision {
        enabled: false,
        running: false,
        detail: e.to_string(),
    });
    let hello = hello(endpoint).await;
    let healthy = installed && manager.running && hello.is_ok();
    if out.json {
        let (version, error) = match &hello {
            Ok(data) => (data.get("version").cloned(), None),
            Err(e) => (None, Some(e.clone())),
        };
        out.json_line(&json!({
            "name": backend.name(),
            "definition": backend.definition_path(),
            "installed": installed,
            "enabled": manager.enabled,
            "running": manager.running,
            "manager": manager.detail,
            "endpoint": endpoint.as_path(),
            "hello": hello.is_ok(),
            "version": version,
            "error": error,
        }));
    } else {
        let definition = format!(
            "{} ({})",
            backend.definition_path().display(),
            if installed {
                "installed"
            } else {
                "not installed"
            }
        );
        let socket = match &hello {
            Ok(data) => format!(
                "{endpoint} answers hello (dstreamy {})",
                data["version"].as_str().unwrap_or("?")
            ),
            Err(e) => format!("{endpoint} does not answer: {e}"),
        };
        out.raw(&table(&[
            vec!["definition".to_owned(), definition],
            vec![backend.name().to_owned(), manager.detail],
            vec!["socket".to_owned(), socket],
        ]));
    }
    Ok(if healthy {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// The `data` of the service's reply to a control `hello`.
async fn hello(endpoint: &Endpoint) -> Result<Value, String> {
    let exchange = async {
        let stream = dstreamy_ipc::connect(endpoint)
            .await
            .map_err(|e| e.to_string())?;
        let mut framed = Framed::new(stream, ControlCodec::new());
        framed
            .send(json!({"id": "status", "cmd": "hello", "role": "control", "protocol": 2}))
            .await
            .map_err(|e| format!("sending hello: {e}"))?;
        match framed.next().await {
            Some(Ok(Ok(reply))) if reply["type"] == "result" => Ok(reply["data"].clone()),
            Some(Ok(Ok(reply))) => Err(format!("unexpected reply {reply}")),
            Some(Ok(Err(e))) => Err(format!("malformed reply: {e:?}")),
            Some(Err(e)) => Err(format!("reading the reply: {e}")),
            None => Err("the connection closed before the reply".to_owned()),
        }
    };
    tokio::time::timeout(HELLO_TIMEOUT, exchange)
        .await
        .unwrap_or_else(|_| Err(format!("no reply within {HELLO_TIMEOUT:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_words_quotes_only_what_a_shell_would_split_or_expand() {
        let argv: Vec<OsString> = ["/usr/bin/systemctl", "--user", "a b", "it's", ""]
            .into_iter()
            .map(OsString::from)
            .collect();
        assert_eq!(
            shell_words(&argv),
            r"/usr/bin/systemctl --user 'a b' 'it'\''s' ''"
        );
    }
}
