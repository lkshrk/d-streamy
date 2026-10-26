//! The output's own thread: connect, `hello`, data capture and every stop signal happen here, never on an OBS thread.

use std::ffi::c_int;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use dstreamy_obs_producer::End;
use dstreamy_obs_producer::Hello;
#[cfg(unix)]
use dstreamy_obs_producer::{self as producer, Endpoint, StartError};
use dstreamy_obs_sys as sys;
use futures_util::FutureExt;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::guard;
use crate::log;
use crate::output::{OutputPtr, Shared, set_last_error};

#[cfg(unix)]
/// How long `destroy` waits for a capture that began after libobs decided the output was inactive.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);
/// bindgen types the macro `0` as `u32` and the negative codes as `i32`; `obs_output_signal_stop` takes `int`.
const SUCCESS: c_int = sys::OBS_OUTPUT_SUCCESS as c_int;

pub enum Command {
    Start {
        hello: Hello,
        endpoint: Option<PathBuf>,
    },
    Stop,
    /// `stop` panicked: answered even when idle, since libobs waits for a stop signal.
    StopFailed(String),
    /// A callback panicked mid-stream: ends a running session, ignored when idle.
    Fail(String),
    EncodeError,
    Shutdown,
}

#[cfg(unix)]
enum Reason {
    Stop,
    Fail(String),
    EncodeError,
    Shutdown,
}

enum Outcome {
    #[cfg(unix)]
    Cancelled(Reason),
    StartFailed(String),
    #[cfg(unix)]
    CaptureRefused,
    #[cfg(unix)]
    Ended(End<Reason>),
    Panicked(String),
}

impl Outcome {
    /// destroy cancelled the session, so the worker exits after finishing it.
    fn ends_worker(&self) -> bool {
        #[cfg(unix)]
        return matches!(
            self,
            Self::Cancelled(Reason::Shutdown) | Self::Ended(End::Stopped(Reason::Shutdown))
        );
        #[cfg(not(unix))]
        false
    }
}

pub fn spawn(
    shared: Arc<Shared>,
    commands: UnboundedReceiver<Command>,
) -> std::io::Result<JoinHandle<()>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    std::thread::Builder::new()
        .name("dstreamy output".into())
        .spawn(move || run(&shared, &runtime, commands))
}

fn run(
    shared: &Shared,
    runtime: &tokio::runtime::Runtime,
    mut commands: UnboundedReceiver<Command>,
) {
    #[cfg(target_os = "linux")]
    if let Err(e) = producer::block_sigpipe_on_this_thread() {
        log::warn(&format!(
            "cannot block SIGPIPE on the D-Streamy thread: {e}"
        ));
    }
    runtime.block_on(async {
        while let Some(command) = commands.recv().await {
            match command {
                Command::Start { hello, endpoint } => {
                    let outcome = AssertUnwindSafe(session(shared, hello, endpoint, &mut commands))
                        .catch_unwind()
                        .await
                        .unwrap_or_else(|payload| {
                            Outcome::Panicked(format!(
                                "D-Streamy plugin error ({})",
                                guard::record_panic("worker", payload.as_ref())
                            ))
                        });
                    let shutdown = outcome.ends_worker();
                    guard::guard("worker", (), || finish(shared, outcome));
                    if shutdown {
                        break;
                    }
                }
                // libobs reset its stopping event before calling stop, so an idle output still signals (rtmp-stream.c:222-224).
                Command::Stop => guard::guard("worker", (), || signal_stop(shared.output, SUCCESS)),
                Command::StopFailed(message) => {
                    guard::guard("worker", (), || fail(shared.output, &message))
                }
                Command::Fail(message) => log::debug(&format!(
                    "ignoring an error after the output stopped: {message}"
                )),
                Command::EncodeError => {
                    log::debug("ignoring an encoder error after the output stopped")
                }
                Command::Shutdown => break,
            }
        }
    });
}

#[cfg(not(unix))]
async fn session(
    _shared: &Shared,
    _hello: Hello,
    _endpoint: Option<PathBuf>,
    _commands: &mut UnboundedReceiver<Command>,
) -> Outcome {
    Outcome::StartFailed("D-Streamy for OBS doesn't support Windows yet".into())
}

#[cfg(unix)]
async fn session(
    shared: &Shared,
    hello: Hello,
    endpoint: Option<PathBuf>,
    commands: &mut UnboundedReceiver<Command>,
) -> Outcome {
    let endpoint = match endpoint.map_or_else(Endpoint::default_for_user, |p| Ok(Endpoint::new(p)))
    {
        Ok(endpoint) => endpoint,
        Err(e) => return Outcome::StartFailed(StartError::Connect(e).to_string()),
    };
    log::info(&format!(
        "connecting to the D-Streamy service at {endpoint}"
    ));
    let connection = tokio::select! {
        attached = producer::attach(&endpoint, &hello) => match attached {
            Ok(connection) => connection,
            Err(e) => return Outcome::StartFailed(e.to_string()),
        },
        reason = next_reason(commands) => return Outcome::Cancelled(reason),
    };
    log::info(&format!(
        "attached to the D-Streamy service as producer {}",
        connection.attachment()
    ));
    {
        let destroying = shared.destroying.lock().unwrap_or_else(|e| e.into_inner());
        if *destroying {
            return Outcome::Cancelled(Reason::Shutdown);
        }
        // SAFETY: destroy can't pass the flag while we hold it; rtmp begins capture on its connect thread too.
        if !unsafe { sys::obs_output_begin_data_capture(shared.output.0, 0) } {
            return Outcome::CaptureRefused;
        }
    }
    let output = shared.output;
    let end = producer::run(
        connection,
        &shared.ring,
        || video_extra_data(output),
        next_reason(commands),
        &log::Obs,
    )
    .await;
    Outcome::Ended(end)
}

#[cfg(unix)]
async fn next_reason(commands: &mut UnboundedReceiver<Command>) -> Reason {
    loop {
        match commands.recv().await {
            Some(Command::Stop) => return Reason::Stop,
            Some(Command::Fail(message) | Command::StopFailed(message)) => {
                return Reason::Fail(message);
            }
            Some(Command::EncodeError) => return Reason::EncodeError,
            Some(Command::Shutdown) | None => return Reason::Shutdown,
            Some(Command::Start { .. }) => log::warn("ignoring a start while D-Streamy runs"),
        }
    }
}

#[cfg(unix)]
fn video_extra_data(output: OutputPtr) -> Option<Vec<u8>> {
    // SAFETY: the output holds its encoder during capture; rtmp reads extra data off-thread too (rtmp-stream.c:786-798).
    unsafe {
        let venc = sys::obs_output_get_video_encoder(output.0);
        if venc.is_null() {
            return None;
        }
        let (mut data, mut size) = (std::ptr::null_mut(), 0usize);
        if !sys::obs_encoder_get_extra_data(venc, &mut data, &mut size)
            || data.is_null()
            || size == 0
        {
            return None;
        }
        Some(std::slice::from_raw_parts(data, size).to_vec())
    }
}

fn finish(shared: &Shared, outcome: Outcome) {
    let output = shared.output;
    shared.ring.close();
    // Before the stop signal, so a restart from its handler isn't refused as "already starting".
    shared.busy.store(false, Ordering::Release);
    match outcome {
        #[cfg(unix)]
        Outcome::Cancelled(Reason::Shutdown) => {}
        #[cfg(unix)]
        Outcome::Cancelled(Reason::Fail(message)) => fail(output, &message),
        Outcome::Panicked(message) => fail(output, &message),
        #[cfg(unix)]
        Outcome::Cancelled(Reason::Stop | Reason::EncodeError) => signal_stop(output, SUCCESS),
        Outcome::StartFailed(message) => {
            log::warn(&message);
            set_last_error(output, &message);
            signal_stop(output, sys::OBS_OUTPUT_CONNECT_FAILED);
        }
        #[cfg(unix)]
        Outcome::CaptureRefused => fail(output, "D-Streamy could not begin data capture"),
        #[cfg(unix)]
        Outcome::Ended(end) => ended(shared, end),
    }
}

#[cfg(unix)]
fn ended(shared: &Shared, end: End<Reason>) {
    let output = shared.output;
    match end {
        End::Stopped(Reason::Stop) if !shared.encode_error.load(Ordering::Acquire) => {
            log::info("stopped");
            // SAFETY: the output is live; ending capture from a plugin thread is what rtmp's send thread does.
            unsafe { sys::obs_output_end_data_capture(output.0) };
        }
        End::Stopped(Reason::Stop | Reason::EncodeError) => {
            log::warn("the encoder failed; stopping D-Streamy");
            signal_stop(output, sys::OBS_OUTPUT_ENCODE_ERROR);
        }
        End::Stopped(Reason::Fail(message)) => fail(output, &message),
        End::Stopped(Reason::Shutdown) => {
            // SAFETY: destroy waits in join for this thread, so the output is still allocated.
            unsafe { sys::obs_output_end_data_capture(output.0) };
            let deadline = Instant::now() + SHUTDOWN_WAIT;
            // SAFETY: as above.
            while unsafe { sys::obs_output_active(output.0) } && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            // The end thread still touches the output after clearing `active`; this joins it (obs-output.c:2573-2575).
            // SAFETY: as above.
            unsafe { sys::obs_output_can_begin_data_capture(output.0, 0) };
        }
        End::Detached { reason, message } => fail(
            output,
            &format!("The D-Streamy service ended the stream ({reason}): {message}"),
        ),
        End::Closed(message) => fail(output, &format!("Lost the D-Streamy service: {message}")),
        End::Refused(e) => fail(output, &format!("D-Streamy cannot send this stream: {e}")),
    }
}

/// Not `OBS_OUTPUT_DISCONNECTED`: that code makes libobs reconnect, which restarts the output (obs-output.c:3008-3017).
fn fail(output: OutputPtr, message: &str) {
    log::warn(message);
    set_last_error(output, message);
    signal_stop(output, sys::OBS_OUTPUT_ERROR);
}

fn signal_stop(output: OutputPtr, code: c_int) {
    // SAFETY: the output is live until destroy joins this thread.
    unsafe { sys::obs_output_signal_stop(output.0, code) };
}
