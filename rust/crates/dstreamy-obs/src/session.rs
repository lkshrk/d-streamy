//! Tools > D-Streamy's Start, Stop and server lists: one thread runs every service call and drives the plugin's output.
// Windows has no service client yet, so only the lists and the stop signal reach the thread there.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use std::ffi::{CString, c_void};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::ptr;
use std::sync::mpsc as std_mpsc;
use std::sync::{Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dstreamy_obs_producer::dock::{Canvas, DockModel, DockState};
#[cfg(unix)]
use dstreamy_obs_producer::{
    Endpoint,
    control::{ObsControl, Ownership, StartRequest},
};
use dstreamy_obs_sys as sys;
use futures_util::FutureExt;
use serde_json::Value;
use tokio::sync::{mpsc, watch};

use crate::log;
use crate::output::{OutputPtr, text};
use crate::settings_source::{change, refresh};

/// How long a dialog click waits for the service before it shows the state reached so far.
pub const UI_WAIT: Duration = Duration::from_secs(3);
/// How long Stop waits for the output to end before it still stops the service session.
const OUTPUT_STOP_WAIT: Duration = Duration::from_secs(10);
/// How long a service call in flight may still finish once OBS starts closing.
const CLOSING_GRACE: Duration = Duration::from_secs(5);
/// How long stopping a session that came up while OBS was closing may take.
const CLOSING_STOP_WAIT: Duration = Duration::from_secs(5);
const OUTPUT_NAME: &std::ffi::CStr = c"D-Streamy";

const CLOSING: &str = "OBS is closing, so D-Streamy did not start";
const NO_VIDEO: &str = "OBS has no video running, so D-Streamy can't start. Check OBS's Settings › Video, then press Start again";
const GONE: &str = "D-Streamy's control thread is not running, so Start and Stop can't finish. \
     See the OBS log and restart OBS";

type Done = std_mpsc::SyncSender<()>;

enum Job {
    Lists(Option<String>, Done),
    #[cfg(unix)]
    Start(StartRequest, Option<Ownership>, Done),
    #[cfg(unix)]
    Stop(Option<Ownership>, Done),
    OutputStopped(i64),
    Shutdown,
}

struct Session {
    jobs: mpsc::UnboundedSender<Job>,
    closing: watch::Sender<bool>,
    thread: JoinHandle<()>,
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);

fn send(job: Job) -> bool {
    let session = SESSION.lock().unwrap_or_else(PoisonError::into_inner);
    session.as_ref().is_some_and(|s| s.jobs.send(job).is_ok())
}

/// Queues `job` and waits up to [`UI_WAIT`] for it; the job finishes on its own if the wait runs out.
/// False when no control thread took it.
fn send_and_wait(job: impl FnOnce(Done) -> Job) -> bool {
    let (done, finished) = std_mpsc::sync_channel(1);
    if !send(job(done)) {
        log::warn("the D-Streamy control thread is gone");
        return false;
    }
    let _ = finished.recv_timeout(UI_WAIT);
    true
}

/// A model step from the control thread: saved off the model lock, then shown in an open dialog.
fn step<R>(f: impl FnOnce(&mut DockModel) -> R) -> R {
    let result = change(|c, p| c.session(p.write(), f));
    refresh();
    result
}

/// Ends a start or stop that can't finish with `message`; a settled state is left alone.
/// Safe in a button callback: it never refreshes, and the callback's `true` rebuilds the dialog.
fn settle_pending(message: &str) {
    change(|c, p| {
        c.session(p.write(), |m| {
            if matches!(m.state(), DockState::Starting | DockState::Stopping) {
                m.abort(message.to_owned());
            }
        })
    });
}

/// [`settle_pending`] off the UI thread, where the open dialog must be asked to rebuild.
fn abort_pending(message: &str) {
    settle_pending(message);
    refresh();
}

/// Starts the control thread; call from `obs_module_load`.
pub fn install() {
    if crate::hooks::no_control_thread() {
        log::warn("test hook: the D-Streamy control thread is not started");
        return;
    }
    let (jobs, receiver) = mpsc::unbounded_channel();
    let (closing, closed) = watch::channel(false);
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            log::error(&format!("cannot start the D-Streamy control thread: {e}"));
            return;
        }
    };
    match std::thread::Builder::new()
        .name("dstreamy control".into())
        .spawn(move || run(&runtime, receiver, closed))
    {
        Ok(thread) => {
            *SESSION.lock().unwrap_or_else(PoisonError::into_inner) = Some(Session {
                jobs,
                closing,
                thread,
            });
        }
        Err(e) => log::error(&format!("cannot start the D-Streamy control thread: {e}")),
    }
}

/// Ends the control thread and releases the output; call from `obs_module_unload`, before libobs unmaps the plugin.
/// A service call in flight gets [`CLOSING_GRACE`] to finish, and a session it brought up is stopped, not streamed.
pub fn shutdown() {
    let session = SESSION
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    if let Some(session) = session {
        let begun = Instant::now();
        session.closing.send_replace(true);
        log::info("OBS is closing: D-Streamy finishes its control steps");
        let _ = session.jobs.send(Job::Shutdown);
        if session.thread.join().is_err() {
            log::warn("the D-Streamy control thread panicked");
        }
        log::info(&format!(
            "the D-Streamy control thread ended in {} ms",
            begun.elapsed().as_millis()
        ));
    }
}

/// Loads the server list, and the selected server's channels, for a dialog about to show them.
pub fn load_lists(guild: Option<String>) {
    send_and_wait(|done| Job::Lists(guild, done));
}

fn canvas() -> Option<Canvas> {
    let mut ovi = sys::obs_video_info::default();
    // SAFETY: a valid out pointer; false when video isn't initialized.
    unsafe { sys::obs_get_video_info(&mut ovi) }.then_some(Canvas {
        fps_num: ovi.fps_num,
        fps_den: ovi.fps_den,
        output_width: ovi.output_width,
        output_height: ovi.output_height,
    })
}

/// The dialog's Start, on the UI thread.
#[cfg(unix)]
pub fn start() {
    let canvas = canvas();
    let begun = change(|c, p| {
        let Some(canvas) = canvas else {
            if c.model.state() == &DockState::Idle {
                c.model.abort(NO_VIDEO.into());
            }
            return Err(NO_VIDEO.to_owned());
        };
        c.session(p.write(), |m| {
            m.start(canvas)
                .map(|request| (request, m.persisted().cloned()))
        })
    });
    match begun {
        Ok((request, persisted)) => {
            if !send_and_wait(|done| Job::Start(request, persisted, done)) {
                settle_pending(GONE);
            }
        }
        Err(e) => log::warn(&e),
    }
}

/// The dialog's Stop, on the UI thread.
#[cfg(unix)]
pub fn stop() {
    if let Some(ownership) = change(|c, p| c.session(p.write(), |m| m.stop()))
        && !send_and_wait(|done| Job::Stop(ownership, done))
    {
        settle_pending(GONE);
    }
}

#[cfg(not(unix))]
pub fn start() {
    log::warn("D-Streamy for OBS doesn't support Windows yet");
}

#[cfg(not(unix))]
pub fn stop() {}

/// The output the dialog starts; the control thread alone creates, starts, stops and releases it.
struct Output(Option<OutputPtr>);

impl Output {
    fn get(&mut self) -> Result<OutputPtr, String> {
        if let Some(output) = self.0 {
            return Ok(output);
        }
        // SAFETY: the type is registered at module load; a null settings object is allowed.
        let raw = unsafe {
            sys::obs_output_create(
                c"dstreamy_output".as_ptr(),
                OUTPUT_NAME.as_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if raw.is_null() {
            return Err("D-Streamy could not create its OBS output".into());
        }
        // SAFETY: the handler is disconnected in `release` before the output goes away.
        unsafe {
            sys::signal_handler_connect(
                sys::obs_output_get_signal_handler(raw),
                c"stop".as_ptr(),
                Some(on_stop),
                ptr::null_mut(),
            );
        }
        let output = OutputPtr(raw);
        self.0 = Some(output);
        Ok(output)
    }

    /// Applies `settings` to the inactive output and starts it; never updates an active output.
    fn start(&mut self, settings: &Value) -> Result<(), String> {
        let output = self.get()?;
        // SAFETY: the output is live until `release`, which only this thread calls.
        unsafe {
            if sys::obs_output_active(output.0) {
                return Err("D-Streamy is still stopping; try again".into());
            }
            let json = CString::new(settings.to_string()).map_err(|e| e.to_string())?;
            let data = sys::obs_data_create_from_json(json.as_ptr());
            sys::obs_output_update(output.0, data);
            sys::obs_data_release(data);
            if sys::obs_output_start(output.0) {
                return Ok(());
            }
            let error = text(sys::obs_output_get_last_error(output.0));
            Err(if error.is_empty() {
                "D-Streamy's OBS output did not start; see the OBS log".into()
            } else {
                error
            })
        }
    }

    /// Stops the output through libobs and waits until it is inactive.
    fn stop(&self) -> Result<(), String> {
        let Some(output) = self.0 else {
            return Ok(());
        };
        // SAFETY: as in `start`.
        unsafe {
            if !sys::obs_output_active(output.0) {
                return Ok(());
            }
            sys::obs_output_stop(output.0);
            let deadline = Instant::now() + OUTPUT_STOP_WAIT;
            while sys::obs_output_active(output.0) {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "D-Streamy's OBS output did not stop within {} s",
                        OUTPUT_STOP_WAIT.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        Ok(())
    }

    fn last_error(&self) -> Option<String> {
        let output = self.0?;
        // SAFETY: as in `start`; libobs returns NULL or a string it owns.
        let error = unsafe { text(sys::obs_output_get_last_error(output.0)) };
        (!error.is_empty()).then_some(error)
    }

    fn release(&mut self) {
        if let Some(output) = self.0.take() {
            // SAFETY: release stops an active output first and joins its worker (output.rs `destroy`).
            unsafe {
                sys::signal_handler_disconnect(
                    sys::obs_output_get_signal_handler(output.0),
                    c"stop".as_ptr(),
                    Some(on_stop),
                    ptr::null_mut(),
                );
                sys::obs_output_release(output.0);
            }
        }
    }
}

/// Runs on whichever thread libobs signals from; the control thread reads the result.
extern "C" fn on_stop(_data: *mut c_void, cd: *mut sys::calldata_t) {
    crate::guard::guard("on_stop", (), || {
        let mut code: i64 = 0;
        // SAFETY: libobs stores "code" as a long long (obs-output.c signal_stop).
        unsafe {
            sys::calldata_get_data(
                cd,
                c"code".as_ptr(),
                (&raw mut code).cast(),
                size_of::<i64>(),
            )
        };
        send(Job::OutputStopped(code));
    });
}

fn run(
    runtime: &tokio::runtime::Runtime,
    mut jobs: mpsc::UnboundedReceiver<Job>,
    closing: watch::Receiver<bool>,
) {
    let mut output = Output(None);
    runtime.block_on(async {
        while let Some(job) = jobs.recv().await {
            if matches!(job, Job::Shutdown) {
                break;
            }
            if let Err(payload) = AssertUnwindSafe(handle(&mut output, job, closing.clone()))
                .catch_unwind()
                .await
            {
                let message = crate::guard::record_panic("control", payload.as_ref());
                log::error(&format!("a D-Streamy control step failed: {message}"));
                abort_pending(&format!(
                    "D-Streamy's control step failed ({message}). See the OBS log, then press Start again"
                ));
            }
        }
    });
    output.release();
}

fn is_closing(closing: &watch::Receiver<bool>) -> bool {
    *closing.borrow()
}

/// Runs a service call; once OBS starts closing it gets [`CLOSING_GRACE`] more, then is dropped (`None`).
async fn until_closing<T>(
    closing: &mut watch::Receiver<bool>,
    call: impl Future<Output = T>,
) -> Option<T> {
    let mut call = std::pin::pin!(call);
    tokio::select! {
        biased;
        value = &mut call => return Some(value),
        _ = closing.wait_for(|c| *c) => {}
    }
    tokio::time::timeout(CLOSING_GRACE, call).await.ok()
}

#[cfg(unix)]
fn control() -> Result<ObsControl, String> {
    Endpoint::default_for_user()
        .map(ObsControl::new)
        .map_err(|e| format!("Cannot find the D-Streamy service: {e}"))
}

async fn handle(output: &mut Output, job: Job, mut closing: watch::Receiver<bool>) {
    match job {
        Job::Lists(guild, done) => {
            if !is_closing(&closing) {
                lists(guild, &mut closing).await;
            }
            let _ = done.try_send(());
        }
        #[cfg(unix)]
        Job::Start(request, persisted, done) => {
            if is_closing(&closing) {
                abort_pending(CLOSING);
            } else {
                start_session(output, &request, persisted.as_ref(), &mut closing).await;
            }
            let _ = done.try_send(());
        }
        #[cfg(unix)]
        Job::Stop(ownership, done) => {
            stop_session(output, ownership.as_ref(), &mut closing).await;
            let _ = done.try_send(());
        }
        Job::OutputStopped(code) => {
            let message = (code != i64::from(sys::OBS_OUTPUT_SUCCESS)).then(|| {
                let reason = output
                    .last_error()
                    .unwrap_or_else(|| format!("D-Streamy's OBS output stopped (code {code})"));
                format!("D-Streamy stopped: {reason}. Press Start to go live again")
            });
            step(|m| m.ended(message));
        }
        Job::Shutdown => {}
    }
}

#[cfg(unix)]
async fn lists(guild: Option<String>, closing: &mut watch::Receiver<bool>) {
    let control = match control() {
        Ok(control) => control,
        Err(e) => {
            log::warn(&e);
            return;
        }
    };
    let Some(guilds) = until_closing(closing, control.guilds()).await else {
        return;
    };
    let selected = change(|c, p| c.edit(p.write(), |m| m.guilds_loaded(guilds)));
    refresh();
    if let Some(guild) = guild.or(selected)
        && let Some(channels) = until_closing(closing, control.channels(&guild)).await
    {
        change(|c, p| c.edit(p.write(), |m| m.channels_loaded(&guild, channels)));
        refresh();
    }
}

#[cfg(not(unix))]
async fn lists(_guild: Option<String>, _closing: &mut watch::Receiver<bool>) {}

/// Ends a session this plugin brought up but won't stream; true once it is gone.
#[cfg(unix)]
async fn end_unused(
    control: &ObsControl,
    ownership: Option<&Ownership>,
    closing: &mut watch::Receiver<bool>,
) -> Result<(), String> {
    let stop = control.stop(ownership);
    let result = if is_closing(closing) {
        tokio::time::timeout(CLOSING_STOP_WAIT, stop)
            .await
            .map_err(|_| format!("no reply within {} s", CLOSING_STOP_WAIT.as_secs()))?
    } else {
        until_closing(closing, stop)
            .await
            .ok_or_else(|| "OBS closed first".to_owned())?
    };
    result.map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(unix)]
async fn start_session(
    output: &mut Output,
    request: &StartRequest,
    persisted: Option<&Ownership>,
    closing: &mut watch::Receiver<bool>,
) {
    let control = match control() {
        Ok(control) => control,
        Err(e) => {
            step(|m| m.output_failed(e, false));
            return;
        }
    };
    let call = async {
        if let Some(gate) = crate::hooks::start_gate() {
            while !gate.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        control.start(request, persisted).await
    };
    let started = match until_closing(closing, call).await {
        Some(Ok(started)) => started,
        Some(Err(failed)) => {
            log::warn(&failed.to_string());
            step(|m| m.started(Err(failed)));
            return;
        }
        None => {
            log::warn(
                "OBS is closing: D-Streamy stopped waiting for the Discord session it was starting",
            );
            abort_pending(CLOSING);
            return;
        }
    };
    let ownership = started.ownership.clone();
    let settings = if is_closing(closing) {
        None
    } else {
        step(|m| m.claim(ownership.clone()))
    };
    // Checked again after the claim's save: OBS may have started closing meanwhile.
    let Some(settings) = settings.filter(|_| !is_closing(closing)) else {
        let ended = end_unused(&control, ownership.as_ref(), closing).await;
        match &ended {
            Ok(()) => {
                log::info("D-Streamy stopped the Discord session it started, before streaming")
            }
            Err(e) => log::warn(&format!(
                "the Discord session D-Streamy started keeps running: {e}"
            )),
        }
        step(|m| {
            m.claim(ownership);
            m.output_failed(CLOSING.into(), ended.is_ok());
        });
        return;
    };
    match output.start(&settings) {
        Ok(()) => {
            log::info(&format!(
                "started from Tools > D-Streamy on Track {}",
                settings["audio_track"]
            ));
            step(|m| m.started(Ok(started)));
        }
        Err(message) => {
            log::warn(&message);
            let message = match end_unused(&control, ownership.as_ref(), closing).await {
                Ok(()) => format!("{message}. Fix it, then press Start again"),
                Err(e) => {
                    log::warn(&format!(
                        "the Discord session D-Streamy started keeps running: {e}"
                    ));
                    return step(|m| {
                        m.output_failed(
                            format!(
                                "{message}. The Discord session it started is still live ({e}); \
                                 press Start to use it again"
                            ),
                            false,
                        )
                    });
                }
            };
            step(|m| m.output_failed(message, true));
        }
    }
}

#[cfg(unix)]
async fn stop_session(
    output: &Output,
    ownership: Option<&Ownership>,
    closing: &mut watch::Receiver<bool>,
) {
    let stopped_output = output.stop();
    let service = match control() {
        Ok(control) => match until_closing(closing, control.stop(ownership)).await {
            Some(result) => result.map(|_| ()).map_err(|e| e.to_string()),
            None => Err("OBS closed before the D-Streamy service answered".into()),
        },
        Err(e) => Err(e),
    };
    let result = match (stopped_output, service) {
        (Ok(()), Ok(())) => Ok(()),
        (output, Err(service)) => {
            let output = output.err().map(|e| format!("{e}; ")).unwrap_or_default();
            Err(format!(
                "Stop didn't finish: {output}{service}. The Discord session may still be live: \
                 press Start to take it over, then Stop again"
            ))
        }
        (Err(output), Ok(())) => Err(format!(
            "Stop didn't finish: {output}. Start works again once it has stopped; see the OBS log"
        )),
    };
    if let Err(e) = &result {
        log::warn(e);
    }
    step(|m| m.stopped(result));
}
