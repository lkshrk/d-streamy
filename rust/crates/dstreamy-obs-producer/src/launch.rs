//! Starts `dstreamy serve` when a connection finds none, for plugins that ship the CLI beside themselves.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use dstreamy_ipc::{Endpoint, IpcError, Stream};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

/// How long a connection waits for a serve it started to listen.
pub const START_WAIT: Duration = Duration::from_secs(10);
/// A serve started this recently is waited for instead of started again.
pub const RELAUNCH_AFTER: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(100);
/// How long OBS's unload waits for a serve it started to shut down before it kills it.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
const HELLO: &[u8] = b"{\"id\":\"1\",\"cmd\":\"hello\",\"role\":\"control\",\"protocol\":2}\n";
const SHUTDOWN: &[u8] = b"{\"id\":\"2\",\"cmd\":\"shutdown\"}\n";

/// How [`Launcher::stop_started`] ended the serve it started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// It had not started one, or that one already exited.
    NotRunning,
    /// It shut down on request, ending its Discord session first.
    Exited(ExitStatus),
    /// It didn't shut down within the grace, so it was killed.
    Killed,
    /// It runs a live session this plugin doesn't own, so it was left running.
    LeftRunning,
}

static INSTALLED: OnceLock<Launcher> = OnceLock::new();

#[derive(Debug)]
pub struct Launcher {
    program: PathBuf,
    args: Vec<OsString>,
    endpoint: Endpoint,
    wait: Duration,
    last: Mutex<Option<Launch>>,
}

#[derive(Debug)]
struct Launch {
    at: Instant,
    child: Option<Child>,
    exited: Option<ExitStatus>,
}

impl Launcher {
    /// Runs `program args` when nothing listens at `endpoint`; other endpoints are left alone.
    pub fn new(
        program: impl Into<PathBuf>,
        args: impl IntoIterator<Item = impl Into<OsString>>,
        endpoint: Endpoint,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            endpoint,
            wait: START_WAIT,
            last: Mutex::new(None),
        }
    }

    /// How long a connection waits for the started serve; [`START_WAIT`] by default.
    pub fn with_wait(self, wait: Duration) -> Self {
        Self { wait, ..self }
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Shuts down the serve this launcher started, killing it after `grace`; `keep_live_session` spares a live one.
    pub fn stop_started(&self, grace: Duration, keep_live_session: bool) -> Ended {
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(launch) = last.as_mut() else {
            return Ended::NotRunning;
        };
        launch.reap();
        if launch.child.is_none() {
            return Ended::NotRunning;
        }
        let deadline = Instant::now() + grace;
        if request_shutdown(&self.endpoint, grace, keep_live_session) == Asked::LeftRunning {
            return Ended::LeftRunning;
        }
        loop {
            launch.reap();
            if let (None, Some(status)) = (&launch.child, launch.exited) {
                return Ended::Exited(status);
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
        }
        if let Some(mut child) = launch.child.take() {
            let _ = child.kill();
            launch.exited = child.wait().ok();
        }
        Ended::Killed
    }

    /// The process id of the serve this launcher started, while it runs.
    pub fn started(&self) -> Option<u32> {
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        let l = last.as_mut()?;
        l.reap();
        l.child.as_ref().map(Child::id)
    }

    /// Connects to `endpoint`, starting the program first when nothing listens there.
    pub async fn connect(&self, endpoint: &Endpoint) -> Result<Stream, IpcError> {
        match dstreamy_ipc::connect(endpoint).await {
            Err(IpcError::NotRunning(_)) if *endpoint == self.endpoint => {}
            other => return other,
        }
        self.launch()?;
        let deadline = Instant::now() + self.wait;
        loop {
            match dstreamy_ipc::connect(endpoint).await {
                Err(IpcError::NotRunning(path)) => {
                    if Instant::now() >= deadline || self.started().is_none() {
                        return Err(self.gave_up(path));
                    }
                    tokio::time::sleep(POLL).await;
                }
                other => return other,
            }
        }
    }

    fn launch(&self) -> Result<(), IpcError> {
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(l) = last.as_mut() {
            l.reap();
            let running = l.child.is_some();
            if running || l.at.elapsed() < RELAUNCH_AFTER {
                return Ok(());
            }
        }
        let child = spawn(&self.program, &self.args).map_err(|e| {
            IpcError::Io(io::Error::new(
                e.kind(),
                format!("could not start {}: {e}", self.program.display()),
            ))
        })?;
        *last = Some(Launch {
            at: Instant::now(),
            child: Some(child),
            exited: None,
        });
        Ok(())
    }

    fn gave_up(&self, path: PathBuf) -> IpcError {
        let exited = self
            .last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
            .and_then(|l| {
                l.reap();
                l.exited
            });
        let program = self.program.display();
        match exited {
            Some(status) => IpcError::Io(io::Error::other(format!(
                "{program} exited ({status}) and nothing listens at {}; its log has the reason",
                path.display()
            ))),
            None => IpcError::Io(io::Error::other(format!(
                "{program} did not listen at {} within {} s",
                path.display(),
                self.wait.as_secs()
            ))),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Asked {
    Shutdown,
    LeftRunning,
}

/// Sends serve `shutdown` and waits for it to close the connection; `keep_live_session` spares a live session.
fn request_shutdown(endpoint: &Endpoint, within: Duration, keep_live_session: bool) -> Asked {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return Asked::Shutdown;
    };
    runtime.block_on(async {
        let asked = tokio::time::timeout(within, async {
            let stream = dstreamy_ipc::connect(endpoint).await.ok()?;
            let (read, mut write) = tokio::io::split(stream);
            let mut read = tokio::io::BufReader::new(read);
            write.write_all(HELLO).await.ok()?;
            let mut hello = String::new();
            read.read_line(&mut hello).await.ok()?;
            if keep_live_session && live_session(&hello) {
                return Some(Asked::LeftRunning);
            }
            write.write_all(SHUTDOWN).await.ok()?;
            let mut rest = Vec::new();
            let _ = read.read_to_end(&mut rest).await;
            Some(Asked::Shutdown)
        })
        .await;
        asked.ok().flatten().unwrap_or(Asked::Shutdown)
    })
}

/// Whether a `hello` reply reports a session that is still connecting, streaming or reconnecting.
fn live_session(hello: &str) -> bool {
    let reply: serde_json::Value = serde_json::from_str(hello).unwrap_or_default();
    let state = &reply["data"]["status"]["session"]["state"];
    state
        .as_str()
        .is_some_and(|s| !matches!(s, "idle" | "stopped"))
}

impl Launch {
    fn reap(&mut self) {
        if let Some(child) = self.child.as_mut()
            && let Ok(Some(status)) = child.try_wait()
        {
            self.exited = Some(status);
            self.child = None;
        }
    }
}

#[cfg(windows)]
fn spawn(program: &Path, args: &[OsString]) -> io::Result<Child> {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const ERROR_ACCESS_DENIED: i32 = 5;
    let run = |flags| command(program, args).creation_flags(flags).spawn();
    let flags = CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
    // A job that forbids breakaway refuses the flag; serve then ends with OBS.
    match run(flags | CREATE_BREAKAWAY_FROM_JOB) {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => run(flags),
        other => other,
    }
}

#[cfg(not(windows))]
fn spawn(program: &Path, args: &[OsString]) -> io::Result<Child> {
    command(program, args).spawn()
}

fn command(program: &Path, args: &[OsString]) -> Command {
    let mut c = Command::new(program);
    c.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    c
}

/// Makes every later connection start `launcher` when its endpoint has no serve; false when one was installed already.
pub fn install(launcher: Launcher) -> bool {
    INSTALLED.set(launcher).is_ok()
}

pub fn installed() -> Option<&'static Launcher> {
    INSTALLED.get()
}

/// [`dstreamy_ipc::connect`], through the installed [`Launcher`] when there is one.
pub async fn connect(endpoint: &Endpoint) -> Result<Stream, IpcError> {
    match INSTALLED.get() {
        Some(launcher) => launcher.connect(endpoint).await,
        None => dstreamy_ipc::connect(endpoint).await,
    }
}
