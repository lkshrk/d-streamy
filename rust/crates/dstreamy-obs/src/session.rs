//! Tools > D-Streamy's Start, Stop and server lists: one thread runs every service call and drives the plugin's output.
// Windows has no service client yet, so only the lists and the stop signal reach the thread there.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use std::cell::Cell;
use std::ffi::{CString, c_void};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dstreamy_obs_producer::dock::{
    Canvas, DockModel, DockState, FIX_AFTER_CLOSING, FIX_LOG, FIX_OUTPUT_SETTING, FIX_PRESS_START,
    FIX_RESTART_OBS, FIX_START_THEN_STOP, FIX_VIDEO,
};
#[cfg(unix)]
use dstreamy_obs_producer::{
    Endpoint,
    control::{ControlError, ObsControl, Ownership, StartRequest},
};
use dstreamy_obs_sys as sys;
use futures_util::FutureExt;
use serde_json::Value;
use tokio::sync::{mpsc, watch};

use crate::log;
use crate::output::{OutputPtr, text};
use crate::settings_source::{change, refresh, with_controls};

/// How long the UI thread waits for a dialog job (Start, Stop, server lists) before it shows the state reached so far.
/// The wait lets the dialog's first paint carry the loaded server lists and a fast `not_running` reply show at click time.
pub const UI_WAIT: Duration = Duration::from_secs(3);
/// How long Stop waits for the output to end before it still stops the service session.
const OUTPUT_STOP_WAIT: Duration = Duration::from_secs(10);
/// How long after OBS starts closing the service calls in flight may still finish, all of them together.
const CLOSING_GRACE: Duration = Duration::from_secs(5);
/// How long after OBS starts closing every control step, cleanup included, must be done.
const CLOSING_LIMIT: Duration = Duration::from_secs(10);
const OUTPUT_NAME: &std::ffi::CStr = c"D-Streamy";

const CLOSING: &str = "OBS is closing, so D-Streamy did not start";
const NO_VIDEO: &str = "OBS has no video running, so D-Streamy can't start. Check OBS's Settings › Video, then press Start again";
const GONE: &str = "D-Streamy's control thread is not running, so Start and Stop can't finish. \
     See the OBS log and restart OBS";

type Done = std_mpsc::SyncSender<()>;

enum Job {
    Lists(Option<String>, Done, ListsFinished),
    #[cfg(unix)]
    Start(StartRequest, Option<Ownership>, Done),
    #[cfg(unix)]
    Stop(Option<Ownership>, Done),
    OutputStopped(i64),
    Shutdown,
}

struct Session {
    jobs: mpsc::UnboundedSender<Job>,
    closing: watch::Sender<Option<Instant>>,
    thread: JoinHandle<()>,
}

/// When OBS began closing, if it has; every wait after that counts against the same moment, so waits never add up.
#[derive(Clone)]
struct Closing(watch::Receiver<Option<Instant>>);

impl Closing {
    fn is_closing(&self) -> bool {
        self.0.borrow().is_some()
    }

    fn deadline(&self, after: Duration) -> Option<Instant> {
        self.0.borrow().map(|began| began + after)
    }

    /// Resolves once OBS has been closing for `after`.
    async fn over(&mut self, after: Duration) {
        let began = match self.0.wait_for(Option::is_some).await {
            Ok(began) => *began,
            Err(_) => None,
        };
        match began {
            Some(began) => tokio::time::sleep_until((began + after).into()).await,
            None => std::future::pending().await,
        }
    }

    /// Runs `call` until OBS has been closing for `after`; `None` once it was dropped.
    async fn within<T>(&mut self, after: Duration, call: impl Future<Output = T>) -> Option<T> {
        tokio::select! {
            biased;
            value = call => Some(value),
            () = self.over(after) => None,
        }
    }

    /// A service call: once OBS starts closing, it may finish within [`CLOSING_GRACE`].
    async fn call<T>(&mut self, call: impl Future<Output = T>) -> Option<T> {
        self.within(CLOSING_GRACE, call).await
    }
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);

fn send(job: Job) -> bool {
    let session = SESSION.lock().unwrap_or_else(PoisonError::into_inner);
    session.as_ref().is_some_and(|s| s.jobs.send(job).is_ok())
}

/// Whether the UI thread waits for a job; the status-bar panel polls instead.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    Dialog,
    No,
}

/// Queues `job`; with [`Wait::Dialog`] waits up to [`UI_WAIT`], and the job finishes on its own if that runs out.
/// False when no control thread took it.
fn queue(job: impl FnOnce(Done) -> Job, wait: Wait) -> bool {
    let (done, finished) = std_mpsc::sync_channel(1);
    if !send(job(done)) {
        log::warn("the D-Streamy control thread is gone");
        return false;
    }
    if wait == Wait::Dialog {
        let _ = finished.recv_timeout(UI_WAIT);
    }
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
fn settle_pending(message: &str, fix: &'static str) {
    change(|c, p| {
        c.session(p.write(), |m| {
            if matches!(m.state(), DockState::Starting | DockState::Stopping) {
                m.abort(message.to_owned(), fix);
            }
        })
    });
}

/// [`settle_pending`] off the UI thread, where the open dialog must be asked to rebuild.
fn abort_pending(message: &str, fix: &'static str) {
    settle_pending(message, fix);
    refresh();
}

/// Starts the control thread; call from `obs_module_load`.
pub fn install() {
    if crate::hooks::no_control_thread() {
        log::warn("test hook: the D-Streamy control thread is not started");
        return;
    }
    let (jobs, receiver) = mpsc::unbounded_channel();
    let (closing, closed) = watch::channel(None);
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
        .spawn(move || run(&runtime, receiver, Closing(closed)))
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

/// Stops and releases the session; only the first call does anything.
pub fn shutdown() {
    let session = SESSION
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    if let Some(session) = session {
        let begun = Instant::now();
        session.closing.send_replace(Some(begun));
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
    send_lists(guild, Wait::Dialog);
}

/// Loads the lists without waiting, for the status-bar panel, which polls; the model shows them loading.
pub fn request_lists(guild: Option<String>) {
    send_lists(guild, Wait::No);
}

/// Ends a list job in the model when dropped: handled, panicked, or never handled.
struct ListsFinished;

impl Drop for ListsFinished {
    fn drop(&mut self) {
        change(|c, _| c.model.lists_finished());
    }
}

fn send_lists(guild: Option<String>, wait: Wait) {
    change(|c, _| c.model.lists_requested());
    queue(|done| Job::Lists(guild, done, ListsFinished), wait);
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

/// Start from the dialog or the status-bar panel, on the UI thread.
#[cfg(unix)]
pub fn start(wait: Wait) {
    let canvas = canvas();
    let begun = change(|c, p| {
        let Some(canvas) = canvas else {
            if c.model.state() == &DockState::Idle {
                c.model.abort(NO_VIDEO.into(), FIX_VIDEO);
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
            if !queue(|done| Job::Start(request, persisted, done), wait) {
                settle_pending(GONE, FIX_RESTART_OBS);
            }
        }
        Err(e) => log::warn(&e),
    }
}

/// Stop from the dialog or the status-bar panel, on the UI thread.
#[cfg(unix)]
pub fn stop(wait: Wait) {
    if let Some(ownership) = change(|c, p| c.session(p.write(), |m| m.stop()))
        && !queue(|done| Job::Stop(ownership, done), wait)
    {
        settle_pending(GONE, FIX_RESTART_OBS);
    }
}

#[cfg(not(unix))]
pub fn start(_wait: Wait) {
    log::warn("D-Streamy for OBS doesn't support Windows yet");
}

#[cfg(not(unix))]
pub fn stop(_wait: Wait) {}

/// The output the dialog starts; the control thread alone creates, starts, stops and releases it.
struct Output {
    ptr: Option<OutputPtr>,
    /// A start libobs accepted whose "stop" hasn't been seen: `active` stays false until the worker connects.
    started: Cell<bool>,
}

/// Counts the output's "stop" signals, so a stop can wait for one it caused.
static STOPS: AtomicU64 = AtomicU64::new(0);

impl Output {
    fn get(&mut self) -> Result<OutputPtr, String> {
        if let Some(output) = self.ptr {
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
        self.ptr = Some(output);
        // SAFETY: a weak reference to the live output, released in `release`.
        let weak = Watched(unsafe { sys::obs_output_get_weak_output(raw) });
        if let Some(old) = WATCHED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(weak)
        {
            // SAFETY: the weak reference this module took.
            unsafe { sys::obs_weak_output_release(old.0) };
        }
        Ok(output)
    }

    /// Applies `settings` to the inactive output and starts it; a failure comes with the panel's fix.
    fn start(&mut self, settings: &Value) -> Result<(), (String, &'static str)> {
        let output = self.get().map_err(|e| (e, FIX_LOG))?;
        // SAFETY: the output is live until `release`, which only this thread calls.
        unsafe {
            if sys::obs_output_active(output.0) {
                return Err((
                    "D-Streamy is still stopping; try again".into(),
                    FIX_PRESS_START,
                ));
            }
            let json = CString::new(settings.to_string()).map_err(|e| (e.to_string(), FIX_LOG))?;
            let data = sys::obs_data_create_from_json(json.as_ptr());
            sys::obs_output_update(output.0, data);
            sys::obs_data_release(data);
            if sys::obs_output_start(output.0) {
                self.started.set(true);
                return Ok(());
            }
            let error = text(sys::obs_output_get_last_error(output.0));
            Err(if error.is_empty() {
                (
                    "D-Streamy's OBS output did not start; see the OBS log".into(),
                    FIX_LOG,
                )
            } else {
                // The output's own errors name the OBS setting to change (encoders.rs).
                (error, FIX_OUTPUT_SETTING)
            })
        }
    }

    fn active(&self) -> bool {
        // SAFETY: as in `start`.
        self.ptr
            .is_some_and(|output| unsafe { sys::obs_output_active(output.0) })
    }

    /// Stops the output, even one still connecting, and waits until it is inactive or OBS's closing leaves no more time.
    fn stop(&self, closing: &Closing) -> Result<(), String> {
        let Some(output) = self.ptr else {
            return Ok(());
        };
        if self.active() {
            // SAFETY: as in `start`.
            unsafe { sys::obs_output_stop(output.0) };
            return self.wait_inactive(closing);
        }
        if !self.started.get() {
            return Ok(());
        }
        let stops = STOPS.load(Ordering::Acquire);
        // obs_output_stop ignores an output whose capture hasn't begun; force_stop still calls the plugin's stop.
        // SAFETY: as in `start`.
        unsafe { sys::obs_output_force_stop(output.0) };
        let stopped = self.wait_until(closing, || {
            STOPS.load(Ordering::Acquire) != stops && !self.active()
        });
        self.started.set(false);
        stopped
    }

    /// Waits until libobs has finished deactivating the output, or OBS's closing leaves no more time.
    fn wait_inactive(&self, closing: &Closing) -> Result<(), String> {
        self.started.set(false);
        self.wait_until(closing, || !self.active())
    }

    fn wait_until(&self, closing: &Closing, done: impl Fn() -> bool) -> Result<(), String> {
        let deadline = Instant::now() + OUTPUT_STOP_WAIT;
        while !done() {
            let now = Instant::now();
            if now >= deadline
                || closing
                    .deadline(CLOSING_LIMIT)
                    .is_some_and(|end| now >= end)
            {
                return Err(format!(
                    "D-Streamy's OBS output did not stop within {} s",
                    OUTPUT_STOP_WAIT.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    fn last_error(&self) -> Option<String> {
        let output = self.ptr?;
        // SAFETY: as in `start`; libobs returns NULL or a string it owns.
        let error = unsafe { text(sys::obs_output_get_last_error(output.0)) };
        (!error.is_empty()).then_some(error)
    }

    fn release(&mut self) {
        if let Some(weak) = WATCHED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            // SAFETY: the weak reference `get` took.
            unsafe { sys::obs_weak_output_release(weak.0) };
        }
        if let Some(output) = self.ptr.take() {
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

/// The control thread's output, for the panel's stats: weak, so reading it never extends the output's life.
struct Watched(*mut sys::obs_weak_output_t);

// SAFETY: weak references are reference-counted by libobs and usable from any thread.
unsafe impl Send for Watched {}

static WATCHED: Mutex<Option<Watched>> = Mutex::new(None);

/// The output's counters while it is active: bytes, frames and dropped frames. Cheap; callable on the UI thread.
pub fn output_counters() -> Option<(u64, u64, u64)> {
    let strong = WATCHED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        // SAFETY: the weak reference is live while it is in WATCHED; this returns a strong reference or NULL.
        .map(|w| unsafe { sys::obs_weak_output_get_output(w.0) })?;
    if strong.is_null() {
        return None;
    }
    // SAFETY: a strong reference taken above, released at once; the getters only read counters.
    unsafe {
        let counters = sys::obs_output_active(strong).then(|| {
            (
                sys::obs_output_get_total_bytes(strong),
                u64::try_from(sys::obs_output_get_total_frames(strong)).unwrap_or(0),
                u64::try_from(sys::obs_output_get_frames_dropped(strong)).unwrap_or(0),
            )
        });
        sys::obs_output_release(strong);
        counters
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
        STOPS.fetch_add(1, Ordering::AcqRel);
    });
}

/// The service session the job in progress brought up: `Some(ownership)` from the service's answer until the job ends.
#[cfg(unix)]
type Brought = Option<Option<Ownership>>;
#[cfg(not(unix))]
type Brought = Option<()>;

fn run(
    runtime: &tokio::runtime::Runtime,
    mut jobs: mpsc::UnboundedReceiver<Job>,
    closing: Closing,
) {
    let mut output = Output {
        ptr: None,
        started: Cell::new(false),
    };
    runtime.block_on(async {
        while let Some(job) = jobs.recv().await {
            if matches!(job, Job::Shutdown) {
                let close = AssertUnwindSafe(stop_on_close(&output, closing.clone()));
                if let Err(payload) = close.catch_unwind().await {
                    let message = crate::guard::record_panic("control close", payload.as_ref());
                    log::warn(&format!(
                        "stopping D-Streamy as OBS closes failed: {message}"
                    ));
                }
                break;
            }
            let mut brought: Brought = None;
            let step = AssertUnwindSafe(handle(&mut output, job, closing.clone(), &mut brought));
            let Err(payload) = step.catch_unwind().await else {
                continue;
            };
            let message = crate::guard::record_panic("control", payload.as_ref());
            log::warn(&format!("a D-Streamy control step failed: {message}"));
            let rollback = AssertUnwindSafe(recover(&output, brought, closing.clone(), &message));
            if let Err(payload) = rollback.catch_unwind().await {
                let again = crate::guard::record_panic("control rollback", payload.as_ref());
                log::warn(&format!(
                    "rolling back the failed D-Streamy control step failed: {again}"
                ));
                abort_pending(
                    &format!(
                        "D-Streamy's control step failed ({message}). See the OBS log, then restart OBS"
                    ),
                    FIX_RESTART_OBS,
                );
            }
        }
    });
    output.release();
}

/// After a panic: a start or stop it left pending never leaves an output or a service session behind unseen.
/// A session already shown live keeps its Stop.
#[cfg(unix)]
async fn recover(output: &Output, brought: Brought, mut closing: Closing, failure: &str) {
    let (state, persisted) =
        with_controls(|c| (c.model.state().clone(), c.model.persisted().cloned()));
    let brought = match state {
        DockState::Starting => brought,
        // A stop that failed half-way is finished: the session it was ending is the saved one.
        DockState::Stopping => Some(persisted),
        _ => {
            refresh();
            return;
        }
    };
    let output_stopped = output.stop(&closing);
    let service = match &brought {
        None => Ok(()),
        Some(ownership) => match control() {
            Ok(control) => end_unused(&control, ownership.as_ref(), &mut closing).await,
            Err(e) => Err(e),
        },
    };
    let ended = brought.is_some() && service.is_ok();
    let message = match (output_stopped, service) {
        (Ok(()), Ok(())) => format!(
            "D-Streamy's control step failed ({failure}), so it stopped what it had started. \
             See the OBS log, then press Start again"
        ),
        (output, service) => {
            let left: Vec<String> = [output.err(), service.err()]
                .into_iter()
                .flatten()
                .collect();
            format!(
                "D-Streamy's control step failed ({failure}) and could not stop what it had started: {}. \
                 Press Start to take the Discord session over, then Stop; see the OBS log",
                left.join("; ")
            )
        }
    };
    log::warn(&message);
    step(|m| {
        if m.state() == &DockState::Starting {
            if let Some(ownership) = brought {
                m.claim(ownership);
            }
            m.output_failed(
                message,
                if ended { FIX_LOG } else { FIX_START_THEN_STOP },
                ended,
            );
        } else {
            m.stopped(Err(message));
            if ended {
                m.restore_ownership(None);
            }
        }
    });
}

#[cfg(not(unix))]
async fn recover(_output: &Output, _brought: Brought, _closing: Closing, failure: &str) {
    abort_pending(
        &format!(
            "D-Streamy's control step failed ({failure}). See the OBS log, then press Start again"
        ),
        FIX_LOG,
    );
}

#[cfg(unix)]
fn control() -> Result<ObsControl, String> {
    Endpoint::default_for_user()
        .map(ObsControl::new)
        .map_err(|e| format!("Cannot find the D-Streamy service: {e}"))
}

const STOPPED_WAITING: &str =
    "OBS is closing: D-Streamy stopped waiting for the Discord session it was starting";

async fn handle(output: &mut Output, job: Job, mut closing: Closing, brought: &mut Brought) {
    match job {
        Job::Lists(guild, done, finished) => {
            if !closing.is_closing() {
                lists(guild, &mut closing).await;
            }
            drop(finished);
            let _ = done.try_send(());
        }
        #[cfg(unix)]
        Job::Start(request, persisted, done) => {
            if closing.is_closing() {
                abort_pending(CLOSING, FIX_AFTER_CLOSING);
            } else {
                start_session(output, &request, persisted.as_ref(), &mut closing, brought).await;
            }
            let _ = done.try_send(());
        }
        #[cfg(unix)]
        Job::Stop(ownership, done) => {
            let _ = stop_session(output, ownership.as_ref(), &mut closing).await;
            let _ = done.try_send(());
        }
        Job::OutputStopped(code) => {
            let message = (code != i64::from(sys::OBS_OUTPUT_SUCCESS)).then(|| {
                let reason = output
                    .last_error()
                    .unwrap_or_else(|| format!("D-Streamy's OBS output stopped (code {code})"));
                format!("D-Streamy stopped: {reason}. Press Start to go live again")
            });
            // libobs signals "stop" before its end-capture thread clears `active`; Start must work once Idle shows.
            if let Err(e) = output.wait_inactive(&closing) {
                log::warn(&e);
            }
            step(|m| m.ended(message));
        }
        Job::Shutdown => {}
    }
    #[cfg(not(unix))]
    let _ = brought;
}

#[cfg(unix)]
async fn lists(guild: Option<String>, closing: &mut Closing) {
    let control = match control() {
        Ok(control) => control,
        Err(e) => {
            log::warn(&e);
            return;
        }
    };
    if let Some(gate) = crate::hooks::lists_gate() {
        let opened = closing.call(async {
            while !gate.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        if opened.await.is_none() {
            return;
        }
    }
    let Some(guilds) = closing.call(control.guilds()).await else {
        return;
    };
    let selected = change(|c, p| c.edit(p.write(), |m| m.guilds_loaded(guilds)));
    refresh();
    if let Some(guild) = guild.or(selected)
        && let Some(channels) = closing.call(control.channels(&guild)).await
    {
        change(|c, p| c.edit(p.write(), |m| m.channels_loaded(&guild, channels)));
        refresh();
    }
}

#[cfg(not(unix))]
async fn lists(_guild: Option<String>, _closing: &mut Closing) {}

/// Ends a session this plugin brought up but won't stream; true once it is gone.
#[cfg(unix)]
async fn end_unused(
    control: &ObsControl,
    ownership: Option<&Ownership>,
    closing: &mut Closing,
) -> Result<(), String> {
    closing
        .within(CLOSING_LIMIT, control.stop(ownership))
        .await
        .ok_or_else(|| "OBS closed first".to_owned())?
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// A session that came up while OBS was closing is stopped, not streamed. When that can't finish in time its
/// ownership stays saved, so the service's grace period ends it and the next Start takes it over or Stop ends it.
#[cfg(unix)]
async fn end_while_closing(
    control: &ObsControl,
    ownership: Option<Ownership>,
    closing: &mut Closing,
) {
    let ended = end_unused(control, ownership.as_ref(), closing).await;
    match &ended {
        Ok(()) => log::info("D-Streamy stopped the Discord session it started, before streaming"),
        Err(e) => log::warn(&format!(
            "the Discord session D-Streamy started may keep running until the D-Streamy service's grace period \
             ends ({e}); it stays saved, so the next Start takes it over and Stop ends it"
        )),
    }
    step(|m| {
        m.claim(ownership);
        m.output_failed(CLOSING.into(), FIX_AFTER_CLOSING, ended.is_ok());
    });
}

#[cfg(unix)]
async fn start_session(
    output: &mut Output,
    request: &StartRequest,
    persisted: Option<&Ownership>,
    closing: &mut Closing,
    brought: &mut Brought,
) {
    let control = match control() {
        Ok(control) => control,
        Err(e) => {
            step(|m| m.output_failed(e, FIX_LOG, false));
            return;
        }
    };
    if let Some(gate) = crate::hooks::start_gate() {
        let opened = closing.call(async {
            while !gate.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        if opened.await.is_none() {
            log::warn(STOPPED_WAITING);
            abort_pending(CLOSING, FIX_AFTER_CLOSING);
            return;
        }
    }
    let mut abandon = closing.clone();
    let started = control
        .start_until(request, persisted, abandon.over(CLOSING_GRACE))
        .await;
    let started = match started {
        Ok(started) => started,
        Err(failed) if failed.error == ControlError::Abandoned => {
            log::warn(STOPPED_WAITING);
            match failed.ownership {
                // The start was sent, so it may still land: its candidate is stopped or kept saved.
                Some(candidate) => end_while_closing(&control, Some(*candidate), closing).await,
                None => abort_pending(CLOSING, FIX_AFTER_CLOSING),
            }
            return;
        }
        Err(failed) => {
            log::warn(&failed.to_string());
            step(|m| m.started(Err(failed)));
            return;
        }
    };
    let ownership = started.ownership.clone();
    *brought = Some(ownership.clone());
    let settings = if closing.is_closing() {
        None
    } else {
        step(|m| m.claim(ownership.clone()))
    };
    // Checked again after the claim's save: OBS may have started closing meanwhile.
    let Some(settings) = settings.filter(|_| !closing.is_closing()) else {
        end_while_closing(&control, ownership, closing).await;
        return;
    };
    match output.start(&settings) {
        Ok(()) => {
            crate::hooks::after_output_start();
            log::info(&format!(
                "started from Tools > D-Streamy on Track {}",
                settings["audio_track"]
            ));
            step(|m| m.started(Ok(started)));
        }
        Err((message, fix)) => {
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
                            fix,
                            false,
                        )
                    });
                }
            };
            step(|m| m.output_failed(message, fix, true));
        }
    }
}

/// OBS closes while live: the output stops first, then the Discord session, within the closing deadlines.
#[cfg(unix)]
async fn stop_on_close(output: &Output, mut closing: Closing) {
    let Some(ownership) = step(DockModel::stop) else {
        return;
    };
    log::info(
        "OBS is closing while D-Streamy is live: it stops its output, then the Discord session",
    );
    match stop_session(output, ownership.as_ref(), &mut closing).await {
        Ok(()) => log::info("OBS is closing: D-Streamy stopped its output and the Discord session"),
        Err(SessionEnded(true)) => {
            log::info("OBS is closing: D-Streamy stopped the Discord session")
        }
        Err(SessionEnded(false)) => log::warn(
            "the Discord session D-Streamy started may keep running until the D-Streamy service's grace period \
             ends; it stays saved, so the next Start takes it over and Stop ends it",
        ),
    }
}

#[cfg(not(unix))]
async fn stop_on_close(_output: &Output, _closing: Closing) {}

/// Why a stop didn't finish: whether the service still ended the Discord session.
struct SessionEnded(bool);

#[cfg(unix)]
async fn stop_session(
    output: &Output,
    ownership: Option<&Ownership>,
    closing: &mut Closing,
) -> Result<(), SessionEnded> {
    let stopped_output = output.stop(closing);
    let service = match control() {
        Ok(control) => match closing.call(control.stop(ownership)).await {
            Some(result) => result.map(|_| ()).map_err(|e| e.to_string()),
            None => Err("OBS closed before the D-Streamy service answered".into()),
        },
        Err(e) => Err(e),
    };
    let ended = SessionEnded(service.is_ok());
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
    let failed = result.is_err();
    step(|m| m.stopped(result));
    if failed { Err(ended) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closing() -> (watch::Sender<Option<Instant>>, Closing) {
        let (tx, rx) = watch::channel(None);
        (tx, Closing(rx))
    }

    #[tokio::test]
    async fn a_list_job_ends_loading_when_dropped_unhandled_or_when_it_panics() {
        let loading = || with_controls(|c| c.model.loading().0);
        let (done, _finished) = std_mpsc::sync_channel(1);

        change(|c, _| c.model.lists_requested());
        let queued = Job::Lists(None, done.clone(), ListsFinished);
        assert!(loading());
        drop(queued);
        assert!(!loading(), "a queued job the control thread never took");

        change(|c, _| c.model.lists_requested());
        let job = Job::Lists(None, done, ListsFinished);
        let run: std::pin::Pin<Box<dyn Future<Output = ()> + Send>> = Box::pin(async move {
            let _job = job;
            tokio::task::yield_now().await;
            panic!("a test panic in a list job");
        });
        assert!(loading());
        assert!(AssertUnwindSafe(run).catch_unwind().await.is_err());
        assert!(!loading(), "a job that panicked");
    }

    #[tokio::test]
    async fn a_call_runs_to_its_end_while_obs_is_open() {
        let (_tx, mut c) = closing();
        let slow = tokio::time::sleep(Duration::from_millis(150));
        assert_eq!(c.within(Duration::ZERO, slow).await, Some(()));
    }

    #[tokio::test]
    async fn every_wait_after_closing_shares_one_deadline() {
        let (tx, mut c) = closing();
        let began = Instant::now();
        tx.send_replace(Some(began));
        let grace = Duration::from_millis(200);
        for _ in 0..3 {
            assert_eq!(c.within(grace, std::future::pending::<()>()).await, None);
        }
        let took = began.elapsed();
        assert!(
            (grace..grace * 2).contains(&took),
            "three waits took {took:?}, not one grace"
        );
        assert_eq!(c.deadline(grace), Some(began + grace));
    }

    #[tokio::test]
    async fn closing_cuts_a_call_already_waiting() {
        let (tx, mut c) = closing();
        let began = Instant::now();
        let close = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            tx.send_replace(Some(Instant::now()));
            std::future::pending::<()>().await
        };
        let call = c.within(Duration::from_millis(100), std::future::pending::<()>());
        tokio::select! {
            dropped = call => assert_eq!(dropped, None),
            () = close => unreachable!(),
        }
        let took = began.elapsed();
        assert!(
            (Duration::from_millis(200)..Duration::from_millis(400)).contains(&took),
            "{took:?}"
        );
    }

    #[tokio::test]
    async fn a_call_done_within_the_grace_still_counts() {
        let (tx, mut c) = closing();
        tx.send_replace(Some(Instant::now()));
        let quick = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            7
        };
        assert_eq!(c.within(Duration::from_millis(300), quick).await, Some(7));
    }
}
