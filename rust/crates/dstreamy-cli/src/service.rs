//! `dstreamy service`: keep `dstreamy serve` running under the platform's user-level service manager.

#[cfg(target_os = "linux")]
pub mod systemd;

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dstreamy_ipc::{ControlCodec, Endpoint};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_util::codec::Framed;

use crate::CliError;
use crate::output::{Out, table};

const HELLO_TIMEOUT: Duration = Duration::from_secs(3);
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const DEFINITION_MODE: u32 = 0o644;

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
    ///
    /// A running service is restarted only when its definition changed, which ends its
    /// Discord session.
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
        /// Socket path; defaults to the one the service's `dstreamy serve` uses
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
    #[error(
        "the running dstreamy binary was replaced or removed ({0}); run the installed dstreamy instead"
    )]
    ExeDeleted(PathBuf),
    #[error("the dstreamy executable {0} does not exist")]
    ExeMissing(PathBuf),
    #[error("no home directory to place the service definition in")]
    NoHome,
    #[error(
        "the user configuration directory {0:?} is not absolute; set XDG_CONFIG_HOME or HOME to an absolute path"
    )]
    RelativeConfigDir(PathBuf),
    #[error(
        "the user service manager does not load units from {dir}; it searches {search_path}. Its XDG_CONFIG_HOME or HOME differs from this shell's: run dstreamy with the same values"
    )]
    NotSearched { dir: PathBuf, search_path: String },
    #[error(
        "{0} is a symbolic link; dstreamy does not replace a link it did not create. Remove it, or install the service by hand"
    )]
    DefinitionIsSymlink(PathBuf),
    #[error("{action} {path}: {source}")]
    File {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    #[error("running {command}: {source}")]
    Spawn { command: String, source: io::Error },
    #[error("{command} failed ({status}){}", stderr_suffix(.stderr))]
    Failed {
        command: String,
        status: ExitStatus,
        stderr: String,
    },
    #[error("{command} did not finish within {after:?}")]
    Timeout { command: String, after: Duration },
    #[error("{command} printed no {what}")]
    Unexpected { command: String, what: String },
    #[error(
        "{source}\n{path} is written, but enabling or starting the service did not finish; running `dstreamy service install` again is safe"
    )]
    NotEnabled {
        path: PathBuf,
        source: Box<ServiceError>,
    },
}

fn stderr_suffix(stderr: &str) -> String {
    let stderr = stderr.trim();
    if stderr.is_empty() {
        String::new()
    } else {
        format!(": {stderr}")
    }
}

impl ServiceError {
    /// The protocol error code this failure maps to.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unsupported => "unsupported_platform",
            Self::NotUtf8(_)
            | Self::UnsupportedPath { .. }
            | Self::ExeDeleted(_)
            | Self::ExeMissing(_)
            | Self::NoHome
            | Self::RelativeConfigDir(_)
            | Self::NotSearched { .. }
            | Self::DefinitionIsSymlink(_) => "config",
            Self::Failed { .. }
            | Self::Timeout { .. }
            | Self::Unexpected { .. }
            | Self::NotEnabled { .. } => "service_manager",
            Self::Spawn { source, .. } if source.kind() == io::ErrorKind::NotFound => {
                "service_manager"
            }
            Self::CurrentExe(_) | Self::File { .. } | Self::Spawn { .. } => "internal",
        }
    }
}

pub enum Step {
    Write { path: PathBuf, contents: String },
    Remove(PathBuf),
    Run(Vec<OsString>),
}

/// The definition already on disk, compared with the one `install` writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Previous {
    Absent,
    Same,
    Different,
}

/// What the service manager says about the service.
pub struct Supervision {
    pub loaded: bool,
    pub enabled: bool,
    pub active: bool,
    pub running: bool,
    /// The running process predates the loaded definition or its binary was replaced.
    pub stale: bool,
    pub detail: String,
}

pub trait Backend: Send + Sync {
    /// The service's name in the manager.
    fn name(&self) -> &str;
    fn definition_path(&self) -> &Path;
    fn definition(&self) -> Result<String, ServiceError>;
    /// With `write`, writes `definition` and restarts a running service; otherwise only enables and starts.
    fn install(&self, definition: &str, write: bool) -> Vec<Step>;
    /// `known`: the manager has the service loaded or active.
    fn uninstall(&self, definition_present: bool, known: bool) -> Vec<Step>;
    /// Whether enablement links are left without a definition.
    fn enablement_left(&self) -> bool;
    fn query(&self) -> Result<Supervision, ServiceError>;
    /// Fails when the manager would not load a definition from [`Backend::definition_path`].
    fn check_location(&self) -> Result<(), ServiceError>;
    /// The runtime directory the manager gives the service, which places its default socket.
    fn service_runtime_dir(&self) -> Result<Option<OsString>, ServiceError>;
}

pub async fn run(out: &Out, args: ServiceArgs) -> Result<ExitCode, CliError> {
    let (backend, exe) = backend(args.systemctl)?;
    match args.action {
        Action::Print => {
            warn_about_exe(out, &exe)?;
            let definition = backend.definition()?;
            if out.json {
                out.json_line(&json!({
                    "path": backend.definition_path(),
                    "contents": definition,
                }));
            } else {
                out.raw(&definition);
            }
            Ok(ExitCode::SUCCESS)
        }
        Action::Install { dry_run } => install(out, backend.as_ref(), &exe, dry_run),
        Action::Uninstall { dry_run } => uninstall(out, backend.as_ref(), dry_run),
        Action::Status { endpoint } => status(out, backend, endpoint).await,
    }
}

#[cfg(target_os = "linux")]
fn backend(systemctl: PathBuf) -> Result<(Arc<dyn Backend>, PathBuf), ServiceError> {
    let exe = std::env::current_exe().map_err(ServiceError::CurrentExe)?;
    let backend = systemd::Systemd::new(systemctl, &exe)?;
    Ok((Arc::new(backend), exe))
}

#[cfg(not(target_os = "linux"))]
fn backend(_systemctl: PathBuf) -> Result<(Arc<dyn Backend>, PathBuf), ServiceError> {
    Err(ServiceError::Unsupported)
}

fn warn_about_exe(out: &Out, exe: &Path) -> Result<(), ServiceError> {
    if let Some(place) = check_exe(exe)? {
        out.warn(format_args!(
            "{} is in {place}; the service stops working when it is removed",
            exe.display()
        ));
    }
    Ok(())
}

/// Refuses an executable the definition cannot point at; names a place that will likely vanish.
pub fn check_exe(exe: &Path) -> Result<Option<&'static str>, ServiceError> {
    if exe.to_string_lossy().ends_with(" (deleted)") {
        return Err(ServiceError::ExeDeleted(exe.to_owned()));
    }
    if !exe.is_file() {
        return Err(ServiceError::ExeMissing(exe.to_owned()));
    }
    let temp = [std::env::temp_dir(), "/tmp".into(), "/var/tmp".into()];
    Ok(vanishing_place(exe, &temp))
}

fn vanishing_place(exe: &Path, temp: &[PathBuf]) -> Option<&'static str> {
    if temp.iter().any(|dir| exe.starts_with(dir)) {
        return Some("a temporary directory");
    }
    let in_target = exe.ancestors().any(|dir| {
        dir.file_name() == Some(OsStr::new("target")) && dir.join("CACHEDIR.TAG").is_file()
    });
    in_target.then_some("a Cargo target directory")
}

pub fn previous(path: &Path, definition: &str) -> Result<Previous, ServiceError> {
    let file_err = |source| ServiceError::File {
        action: "reading",
        path: path.to_owned(),
        source,
    };
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Previous::Absent),
        Err(e) => Err(file_err(e)),
        Ok(meta) if meta.file_type().is_symlink() => {
            Err(ServiceError::DefinitionIsSymlink(path.to_owned()))
        }
        Ok(_) => {
            let current = fs::read(path).map_err(file_err)?;
            Ok(if current == definition.as_bytes() {
                Previous::Same
            } else {
                Previous::Different
            })
        }
    }
}

fn install(
    out: &Out,
    backend: &dyn Backend,
    exe: &Path,
    dry_run: bool,
) -> Result<ExitCode, CliError> {
    warn_about_exe(out, exe)?;
    let definition = backend.definition()?;
    let path = backend.definition_path();
    let previous = previous(path, &definition)?;
    let state = manager_state(out, backend, dry_run)?;
    let running = state.as_ref().is_some_and(|s| s.running);
    let stale = running && state.as_ref().is_some_and(|s| s.stale);
    let write = previous != Previous::Same || stale;
    let steps = backend.install(&definition, write);
    if dry_run {
        print_plan(out, &steps);
        return Ok(ExitCode::SUCCESS);
    }
    apply(&steps)?;
    let outcome = match (running, write) {
        (true, true) => "restarted",
        (true, false) => "unchanged",
        (false, _) => "started",
    };
    if out.json {
        out.json_line(&json!({
            "action": outcome,
            "written": write,
            "name": backend.name(),
            "definition": path,
        }));
    } else {
        let name = backend.name();
        let path = path.display();
        let file = match previous {
            Previous::Absent => format!("installed {path}"),
            Previous::Different => format!("updated {path}"),
            Previous::Same => format!("{path} is unchanged"),
        };
        let service = match outcome {
            "unchanged" => format!("{name} is enabled and was already running"),
            "restarted" if previous == Previous::Same => format!(
                "{name} was restarted because it ran an outdated definition or binary, which ended any Discord session it had"
            ),
            "restarted" => format!(
                "{name} is enabled and was restarted, which ended any Discord session it had"
            ),
            _ => format!("{name} is enabled and started"),
        };
        out.line(format_args!("{file}; {service}"));
    }
    Ok(ExitCode::SUCCESS)
}

/// Checks the location and queries the service; a dry run only warns when either fails.
fn manager_state(
    out: &Out,
    backend: &dyn Backend,
    dry_run: bool,
) -> Result<Option<Supervision>, ServiceError> {
    let checked = backend.check_location();
    let state = backend.query();
    if !dry_run {
        checked?;
        return state.map(Some);
    }
    if let Err(e) = &checked {
        out.warn(format_args!("a real run would fail: {e}"));
    }
    Ok(match state {
        Ok(state) => Some(state),
        Err(e) => {
            out.warn(format_args!("querying the service manager: {e}"));
            None
        }
    })
}

fn uninstall(out: &Out, backend: &dyn Backend, dry_run: bool) -> Result<ExitCode, CliError> {
    let path = backend.definition_path();
    let present = fs::symlink_metadata(path).is_ok();
    let state = manager_state(out, backend, dry_run)?;
    let known = state.as_ref().is_none_or(|s| s.loaded || s.active);
    if !present && !known && !backend.enablement_left() {
        if out.json {
            out.json_line(&json!({
                "action": "absent",
                "name": backend.name(),
                "definition": path,
            }));
        } else {
            out.line(format_args!(
                "{} is not installed ({} does not exist)",
                backend.name(),
                path.display()
            ));
        }
        return Ok(ExitCode::SUCCESS);
    }
    let steps = backend.uninstall(present, known);
    if dry_run {
        print_plan(out, &steps);
        return Ok(ExitCode::SUCCESS);
    }
    apply(&steps)?;
    if out.json {
        out.json_line(&json!({
            "action": "uninstalled",
            "name": backend.name(),
            "definition": path,
        }));
    } else if present {
        out.line(format_args!(
            "uninstalled {}; removed {}",
            backend.name(),
            path.display()
        ));
    } else {
        out.line(format_args!(
            "uninstalled {}; {} was already gone, so the service was stopped and its links removed",
            backend.name(),
            path.display()
        ));
    }
    Ok(ExitCode::SUCCESS)
}

pub fn apply(steps: &[Step]) -> Result<(), ServiceError> {
    let mut written = None;
    for step in steps {
        match step {
            Step::Write { path, contents } => {
                write_atomically(path, contents).map_err(|(renamed, e)| {
                    if renamed {
                        ServiceError::NotEnabled {
                            path: path.clone(),
                            source: Box::new(e),
                        }
                    } else {
                        e
                    }
                })?;
                written = Some(path.clone());
            }
            Step::Remove(path) => match fs::remove_file(path) {
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
                if let Err(e) = checked_output(argv, None) {
                    return Err(match written {
                        Some(path) => ServiceError::NotEnabled {
                            path,
                            source: Box::new(e),
                        },
                        None => e,
                    });
                }
            }
        }
    }
    Ok(())
}

/// Replaces `path` by rename, so a reader never sees half a file and a symlink there is replaced, not followed.
/// An error says whether the rename already happened.
fn write_atomically(path: &Path, contents: &str) -> Result<(), (bool, ServiceError)> {
    let file_err = |action| {
        move |source| ServiceError::File {
            action,
            path: path.to_owned(),
            source,
        }
    };
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(|e| (false, file_err("creating the directory of")(e)))?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let written = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(DEFINITION_MODE)
            .open(&temp)?;
        file.set_permissions(Permissions::from_mode(DEFINITION_MODE))?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&temp);
        return Err((false, file_err("writing")(e)));
    }
    File::open(dir)
        .and_then(|dir| dir.sync_all())
        .map_err(|e| (true, file_err("syncing the directory of")(e)))
}

/// Runs `argv`, capturing its output; with a `timeout` it is killed once that passes.
pub fn output(argv: &[OsString], timeout: Option<Duration>) -> Result<Output, ServiceError> {
    let (program, args) = argv.split_first().expect("a command has a program");
    let spawn_err = |source| ServiceError::Spawn {
        command: shell_words(argv),
        source,
    };
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null());
    let Some(timeout) = timeout else {
        return command.output().map_err(spawn_err);
    };
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(spawn_err)?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        })
    };
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as _));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(spawn_err)? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ServiceError::Timeout {
                command: shell_words(argv),
                after: timeout,
            });
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Ok(Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// [`output`] that fails, with the command's stderr, on a non-zero exit.
pub fn checked_output(
    argv: &[OsString],
    timeout: Option<Duration>,
) -> Result<Output, ServiceError> {
    let output = output(argv, timeout)?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(ServiceError::Failed {
            command: shell_words(argv),
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
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

/// The manager's answers, gathered off the async runtime; after a timeout the rest are skipped.
struct ManagerView {
    location: Result<(), String>,
    supervision: Result<Supervision, String>,
    runtime_dir: Result<Option<OsString>, String>,
}

fn manager_view(backend: &dyn Backend) -> ManagerView {
    let timed_out = |e: &ServiceError| matches!(e, ServiceError::Timeout { .. });
    let location = backend.check_location();
    if let Err(e) = &location
        && timed_out(e)
    {
        let e = e.to_string();
        return ManagerView {
            location: Err(e.clone()),
            supervision: Err(e.clone()),
            runtime_dir: Err(e),
        };
    }
    let supervision = backend.query();
    if let Err(e) = &supervision
        && timed_out(e)
    {
        let e = e.to_string();
        return ManagerView {
            location: location.map_err(|e| e.to_string()),
            supervision: Err(e.clone()),
            runtime_dir: Err(e),
        };
    }
    ManagerView {
        location: location.map_err(|e| e.to_string()),
        supervision: supervision.map_err(|e| e.to_string()),
        runtime_dir: backend.service_runtime_dir().map_err(|e| e.to_string()),
    }
}

async fn status(
    out: &Out,
    backend: Arc<dyn Backend>,
    endpoint: Option<PathBuf>,
) -> Result<ExitCode, CliError> {
    let installed = fs::symlink_metadata(backend.definition_path()).is_ok();
    let view = {
        let backend = backend.clone();
        tokio::task::spawn_blocking(move || manager_view(backend.as_ref()))
            .await
            .map_err(|e| CliError::Session(format!("querying the service manager: {e}")))?
    };
    let own_runtime_dir = std::env::var_os("XDG_RUNTIME_DIR");
    let mut runtime_note = None;
    let endpoint = match endpoint {
        Some(path) => Ok(Endpoint::new(path)),
        None => match &view.runtime_dir {
            Ok(Some(dir)) => {
                if own_runtime_dir.as_deref() != Some(dir.as_os_str()) {
                    runtime_note = Some(format!(
                        "the service's XDG_RUNTIME_DIR is {}, this shell's is {}",
                        dir.to_string_lossy(),
                        own_runtime_dir
                            .as_deref()
                            .map_or("unset".into(), OsStr::to_string_lossy)
                    ));
                }
                Endpoint::default_for_runtime_dir(Some(dir))
            }
            Ok(None) => Endpoint::default_for_runtime_dir(None),
            Err(e) => {
                runtime_note = Some(format!(
                    "could not read the service's XDG_RUNTIME_DIR ({e}); used this shell's"
                ));
                Endpoint::default_for_user()
            }
        },
    }
    .map_err(|e| format!("finding the socket: {e}"));
    let hello = match &endpoint {
        Ok(endpoint) => hello(endpoint).await,
        Err(e) => Err(e.clone()),
    };
    let (running, enabled, manager) = match &view.supervision {
        Ok(s) => (s.running, s.enabled, s.detail.clone()),
        Err(e) => (false, false, e.clone()),
    };
    let healthy = installed && view.location.is_ok() && running && hello.is_ok();
    let endpoint_path = endpoint.as_ref().ok().map(|e| e.as_path().to_owned());
    if out.json {
        let (version, error) = match &hello {
            Ok(data) => (data.get("version").cloned(), None),
            Err(e) => (None, Some(e.clone())),
        };
        out.json_line(&json!({
            "name": backend.name(),
            "definition": backend.definition_path(),
            "installed": installed,
            "location_error": view.location.as_ref().err(),
            "enabled": enabled,
            "running": running,
            "manager": manager,
            "endpoint": endpoint_path,
            "runtime_dir_note": runtime_note,
            "hello": hello.is_ok(),
            "version": version,
            "error": error,
        }));
    } else {
        let mut definition = format!(
            "{} ({})",
            backend.definition_path().display(),
            if installed {
                "installed"
            } else {
                "not installed"
            }
        );
        if let Err(e) = &view.location {
            definition.push_str(&format!("; {e}"));
        }
        let place = endpoint_path
            .as_ref()
            .map_or("the socket".to_owned(), |p| p.display().to_string());
        let mut socket = match &hello {
            Ok(data) => format!(
                "{place} answers hello (dstreamy {})",
                data["version"].as_str().unwrap_or("?")
            ),
            Err(e) => format!("{place} does not answer: {e}"),
        };
        if let Some(note) = &runtime_note {
            socket.push_str(&format!(" ({note})"));
        }
        out.raw(&table(&[
            vec!["definition".to_owned(), definition],
            vec![backend.name().to_owned(), manager],
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

    #[test]
    fn a_replaced_or_missing_executable_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("dstreamy (deleted)");
        std::fs::write(&gone, "").unwrap();
        assert!(matches!(check_exe(&gone), Err(ServiceError::ExeDeleted(_))));
        assert!(matches!(
            check_exe(&dir.path().join("dstreamy")),
            Err(ServiceError::ExeMissing(_))
        ));
    }

    #[test]
    fn an_executable_in_a_temporary_or_cargo_target_directory_is_flagged() {
        let tmp = tempfile::tempdir_in("/tmp").unwrap();
        let exe = tmp.path().join("dstreamy");
        std::fs::write(&exe, "").unwrap();
        assert_eq!(check_exe(&exe).unwrap(), Some("a temporary directory"));

        let target = tmp.path().join("target");
        std::fs::create_dir_all(target.join("debug")).unwrap();
        let exe = target.join("debug/dstreamy");
        assert_eq!(vanishing_place(&exe, &[]), None);
        std::fs::write(target.join("CACHEDIR.TAG"), "").unwrap();
        assert_eq!(vanishing_place(&exe, &[]), Some("a Cargo target directory"));
    }

    #[test]
    fn a_write_replaces_a_symlink_instead_of_following_it_and_sets_the_mode() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, "theirs").unwrap();
        let path = dir.path().join("unit/dstreamy.service");
        std::fs::create_dir(dir.path().join("unit")).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        write_atomically(&path, "ours").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "theirs");
        let meta = std::fs::symlink_metadata(&path).unwrap();
        assert!(meta.file_type().is_file());
        assert_eq!(meta.permissions().mode() & 0o7777, DEFINITION_MODE);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ours");
        assert_eq!(
            std::fs::read_dir(dir.path().join("unit")).unwrap().count(),
            1
        );
    }

    #[test]
    fn previous_refuses_a_symlink_and_compares_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dstreamy.service");
        assert_eq!(previous(&path, "a").unwrap(), Previous::Absent);
        std::fs::write(&path, "a").unwrap();
        assert_eq!(previous(&path, "a").unwrap(), Previous::Same);
        assert_eq!(previous(&path, "b").unwrap(), Previous::Different);
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(dir.path().join("x"), &path).unwrap();
        assert!(matches!(
            previous(&path, "a"),
            Err(ServiceError::DefinitionIsSymlink(_))
        ));
    }

    #[test]
    fn a_query_is_killed_at_its_timeout() {
        let argv: Vec<OsString> = ["sh", "-c", "exec sleep 30"]
            .into_iter()
            .map(OsString::from)
            .collect();
        let started = Instant::now();
        let e = output(&argv, Some(Duration::from_millis(200))).unwrap_err();
        assert!(matches!(e, ServiceError::Timeout { .. }), "{e}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
