//! The global procs and the `dstreamy_state` signal; global procs cannot be removed, so their userdata is a static [`Gate`].

use std::collections::HashSet;
use std::ffi::{CStr, c_char, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use dstreamy_obs_sys as sys;
use tokio::sync::{Notify, mpsc};

use crate::guard::guard;

pub const QUEUE: usize = 64;
pub const MAX_SETTINGS_BYTES: usize = 64 * 1024;
pub const MAX_ID_BYTES: usize = 1024;
pub const SHUTTING_DOWN: &str = "D-Streamy is shutting down";
pub const LOADING: &str = "D-Streamy is still loading";
pub const BUSY: &str = "D-Streamy is busy; try again shortly";
pub const SIGNAL: &CStr = c"dstreamy_state";
const SIGNAL_DECL: &CStr = c"void dstreamy_state(string json)";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Start,
    Stop,
    Status,
    Guilds,
    Channels { guild_id: String },
    GetSettings,
    SetSettings { json: String },
    Shutdown,
}

impl Request {
    pub fn operation(&self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Status => "status",
            Self::Guilds => "guilds",
            Self::Channels { .. } => "channels",
            Self::GetSettings => "get_settings",
            Self::SetSettings { .. } => "set_settings",
            Self::Shutdown => "shutdown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admitted {
    pub id: i64,
    pub request: Request,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    Unknown,
    Conventional,
    Enhanced,
}

impl Classification {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "unknown" => Some(Self::Unknown),
            "conventional" => Some(Self::Conventional),
            "enhanced" => Some(Self::Enhanced),
            _ => None,
        }
    }
}

/// How the gate takes and drops its own strong reference on a frontend output.
#[derive(Clone, Copy)]
pub struct Refs {
    pub acquire: fn(*mut sys::obs_output_t) -> bool,
    pub release: fn(*mut sys::obs_output_t),
}

pub const OBS_REFS: Refs = Refs {
    // SAFETY (both): the caller holds its own reference for the duration of the proc call.
    acquire: |output| !unsafe { sys::obs_output_get_ref(output) }.is_null(),
    release: |output| unsafe { sys::obs_output_release(output) },
};

/// A strong reference on an output, released on drop.
pub struct OutputRef {
    output: *mut sys::obs_output_t,
    release: fn(*mut sys::obs_output_t),
}

// SAFETY: obs_output_release may be called from any thread.
unsafe impl Send for OutputRef {}

impl Drop for OutputRef {
    fn drop(&mut self) {
        (self.release)(self.output);
    }
}

/// The latest stream observation, as the controller sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observed {
    pub generation: i64,
    pub revision: u64,
    pub classification: Classification,
    pub output: bool,
    pub stopped: bool,
}

struct Attempt {
    generation: i64,
    revision: u64,
    classification: Classification,
    output: Option<OutputRef>,
    stopped: bool,
}

#[derive(Default)]
struct MailState {
    current: Option<Attempt>,
    changed: Option<&'static str>,
    closed: bool,
}

/// One slot for the stream attempt the frontend last reported, apart from the request queue.
#[derive(Default)]
pub struct Mailbox {
    state: Mutex<MailState>,
    wake: Notify,
}

impl Mailbox {
    pub fn view(&self) -> Option<Observed> {
        lock(&self.state).current.as_ref().map(|a| Observed {
            generation: a.generation,
            revision: a.revision,
            classification: a.classification,
            output: a.output.is_some(),
            stopped: a.stopped,
        })
    }

    /// The operation of the latest change not yet taken.
    pub fn take_change(&self) -> Option<&'static str> {
        lock(&self.state).changed.take()
    }

    pub async fn changed(&self) {
        self.wake.notified().await;
    }

    /// Drops the held reference and refuses later observations.
    pub fn close(&self) {
        let released = {
            let mut s = lock(&self.state);
            s.closed = true;
            s.current.take()
        };
        drop(released);
    }

    /// `start` is `None` for a stop; displaced references are released after the lock is dropped.
    fn apply(
        &self,
        operation: &'static str,
        generation: i64,
        start: Option<(Option<OutputRef>, Classification)>,
    ) -> Result<(), String> {
        let mut released = None;
        let result = {
            let mut s = lock(&self.state);
            let current = s.current.as_ref().map(|a| (a.generation, a.stopped));
            match (start, current) {
                (start, _) if s.closed => {
                    released = start.and_then(|(output, _)| output);
                    Err(SHUTTING_DOWN.to_owned())
                }
                (Some((output, _)), Some((g, _))) if generation < g => {
                    released = output;
                    Err(format!("generation {generation} is older than {g}"))
                }
                (Some((output, _)), Some((g, true))) if generation == g => {
                    released = output;
                    Err(format!("stream attempt {generation} already stopped"))
                }
                (Some((output, classification)), Some((g, false))) if generation == g => {
                    let a = s.current.as_mut().expect("current attempt");
                    let same = a.classification == classification
                        && a.output.as_ref().map(|o| o.output) == output.as_ref().map(|o| o.output);
                    if same {
                        released = output;
                    } else {
                        a.revision += 1;
                        a.classification = classification;
                        released = std::mem::replace(&mut a.output, output);
                        s.changed = Some(operation);
                    }
                    Ok(())
                }
                (Some((output, classification)), _) => {
                    released = s
                        .current
                        .replace(Attempt {
                            generation,
                            revision: 0,
                            classification,
                            output,
                            stopped: false,
                        })
                        .and_then(|a| a.output);
                    s.changed = Some(operation);
                    Ok(())
                }
                (None, Some((g, _))) if generation < g => Ok(()),
                (None, Some((g, true))) if generation == g => Ok(()),
                (None, Some((g, false))) if generation == g => {
                    let a = s.current.as_mut().expect("current attempt");
                    a.stopped = true;
                    released = a.output.take();
                    s.changed = Some(operation);
                    Ok(())
                }
                (None, _) => {
                    released = s
                        .current
                        .replace(Attempt {
                            generation,
                            revision: 0,
                            classification: Classification::Unknown,
                            output: None,
                            stopped: true,
                        })
                        .and_then(|a| a.output);
                    s.changed = Some(operation);
                    Ok(())
                }
            }
        };
        drop(released);
        if result.is_ok() {
            self.wake.notify_one();
        }
        result
    }
}

/// What the procs tell the controller outside the queue: a stop or shutdown wish, and unload.
#[derive(Default)]
pub struct Priority {
    stop: AtomicBool,
    shutdown: AtomicI64,
    unload: AtomicBool,
    wake: Notify,
}

impl Priority {
    pub fn take_stop(&self) -> bool {
        self.stop.swap(false, Ordering::AcqRel)
    }

    /// The request id of an admitted shutdown, once.
    pub fn take_shutdown(&self) -> Option<i64> {
        Some(self.shutdown.swap(0, Ordering::AcqRel)).filter(|&id| id != 0)
    }

    pub fn shutting_down(&self) -> bool {
        self.shutdown.load(Ordering::Acquire) != 0 || self.unloading()
    }

    pub fn unloading(&self) -> bool {
        self.unload.load(Ordering::Acquire)
    }

    pub fn cancel_unload(&self) {
        self.unload.store(true, Ordering::Release);
        self.wake.notify_waiters();
    }

    fn request_shutdown(&self, id: i64) {
        self.shutdown.store(id, Ordering::Release);
        self.wake.notify_waiters();
    }

    fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// Returns once `done` holds; safe when it already held before the call.
    pub async fn until(&self, done: impl Fn(&Self) -> bool) {
        loop {
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if done(self) {
                return;
            }
            notified.await;
        }
    }
}

/// State the gate shares with the controller it admits work to.
#[derive(Default)]
pub struct Shared {
    pub mailbox: Mailbox,
    pub priority: Priority,
    outstanding: Mutex<HashSet<i64>>,
}

impl Shared {
    /// Frees a request id for reuse; called before its completion is emitted.
    pub fn finish(&self, id: i64) {
        lock(&self.outstanding).remove(&id);
    }

    pub fn clear(&self) {
        lock(&self.outstanding).clear();
    }
}

enum GateState {
    Loading,
    Open {
        requests: mpsc::Sender<Admitted>,
        shared: Arc<Shared>,
    },
    Closed,
}

/// The process-lifetime userdata of every global proc.
pub struct Gate {
    state: Mutex<GateState>,
    registered: AtomicBool,
    refs: Refs,
}

impl Gate {
    pub const fn new(refs: Refs) -> Self {
        Self {
            state: Mutex::new(GateState::Loading),
            registered: AtomicBool::new(false),
            refs,
        }
    }

    pub fn open(&self, requests: mpsc::Sender<Admitted>, shared: Arc<Shared>) {
        let mut state = lock(&self.state);
        if matches!(*state, GateState::Loading) {
            *state = GateState::Open { requests, shared };
        }
    }

    /// Refuses every later call for good.
    pub fn close(&self) {
        let previous = std::mem::replace(&mut *lock(&self.state), GateState::Closed);
        drop(previous);
    }

    /// Adds the signal and the procs once; a second call adds nothing and returns false.
    pub fn register(
        &'static self,
        procs: *mut sys::proc_handler_t,
        signals: *mut sys::signal_handler_t,
    ) -> bool {
        if self.registered.swap(true, Ordering::AcqRel) {
            return false;
        }
        let data = ptr::from_ref(self).cast_mut().cast::<c_void>();
        // SAFETY: valid handlers; the declarations are NUL-terminated and `data` lives for the process.
        unsafe {
            sys::signal_handler_add(signals, SIGNAL_DECL.as_ptr());
            for ((_, decl), callback) in PROCS.iter().zip(CALLBACKS) {
                sys::proc_handler_add(procs, decl.as_ptr(), Some(callback), data);
            }
        }
        true
    }

    /// # Safety
    /// `cd` is the calldata of a live proc call.
    unsafe fn admit(&self, which: Proc, cd: *mut sys::calldata_t) -> Result<(), String> {
        // SAFETY (all reads): per the caller.
        let request = match which {
            Proc::StreamStarting | Proc::StreamStarted => {
                let generation = unsafe { generation(cd) }?;
                let mut output: *mut sys::obs_output_t = ptr::null_mut();
                let found = unsafe {
                    sys::calldata_get_data(
                        cd,
                        c"output".as_ptr(),
                        (&raw mut output).cast(),
                        size_of::<*mut sys::obs_output_t>(),
                    )
                };
                if !found {
                    return Err("output must be an output pointer or NULL".into());
                }
                let text = unsafe { string(cd, c"classification", MAX_ID_BYTES) }?;
                let classification = Classification::parse(&text).ok_or_else(|| {
                    "classification must be \"unknown\", \"conventional\" or \"enhanced\""
                        .to_owned()
                })?;
                let operation = if which == Proc::StreamStarting {
                    "stream_starting"
                } else {
                    "stream_started"
                };
                let held = (!output.is_null() && (self.refs.acquire)(output)).then(|| OutputRef {
                    output,
                    release: self.refs.release,
                });
                let shared = self.shared()?;
                return shared
                    .mailbox
                    .apply(operation, generation, Some((held, classification)));
            }
            Proc::StreamStopped => {
                let generation = unsafe { generation(cd) }?;
                return self
                    .shared()?
                    .mailbox
                    .apply("stream_stopped", generation, None);
            }
            Proc::Start => Request::Start,
            Proc::Stop => Request::Stop,
            Proc::Status => Request::Status,
            Proc::Guilds => Request::Guilds,
            Proc::Channels => {
                let guild_id = unsafe { string(cd, c"guild_id", MAX_ID_BYTES) }?;
                if guild_id.is_empty() {
                    return Err("guild_id must not be empty".into());
                }
                Request::Channels { guild_id }
            }
            Proc::GetSettings => Request::GetSettings,
            Proc::SetSettings => Request::SetSettings {
                json: unsafe { string(cd, c"json", MAX_SETTINGS_BYTES) }?,
            },
            Proc::Shutdown => Request::Shutdown,
        };
        let id = unsafe { get_int(cd, c"request_id") }
            .filter(|&id| id > 0)
            .ok_or_else(|| "request_id must be a positive integer".to_owned())?;
        self.submit(id, request)
    }

    fn shared(&self) -> Result<Arc<Shared>, String> {
        match &*lock(&self.state) {
            GateState::Open { shared, .. } => Ok(shared.clone()),
            GateState::Loading => Err(LOADING.into()),
            GateState::Closed => Err(SHUTTING_DOWN.into()),
        }
    }

    fn submit(&self, id: i64, request: Request) -> Result<(), String> {
        let (requests, shared) = {
            let mut state = lock(&self.state);
            let (requests, shared) = match &*state {
                GateState::Open { requests, shared } => (requests.clone(), shared.clone()),
                GateState::Loading => return Err(LOADING.into()),
                GateState::Closed => return Err(SHUTTING_DOWN.into()),
            };
            if !lock(&shared.outstanding).insert(id) {
                return Err(format!("request_id {id} is still outstanding"));
            }
            if request == Request::Shutdown {
                *state = GateState::Closed;
                shared.priority.request_shutdown(id);
                return Ok(());
            }
            (requests, shared)
        };
        let stop = request == Request::Stop;
        match requests.try_send(Admitted { id, request }) {
            Ok(()) => {
                if stop {
                    shared.priority.request_stop();
                }
                Ok(())
            }
            Err(e) => {
                shared.finish(id);
                Err(match e {
                    mpsc::error::TrySendError::Full(_) => BUSY,
                    mpsc::error::TrySendError::Closed(_) => SHUTTING_DOWN,
                }
                .to_owned())
            }
        }
    }
}

/// # Safety
/// `cd` is a valid calldata.
unsafe fn generation(cd: *const sys::calldata_t) -> Result<i64, String> {
    // SAFETY: per the caller.
    unsafe { get_int(cd, c"generation") }
        .filter(|&g| g > 0)
        .ok_or_else(|| "generation must be a positive integer".to_owned())
}

/// Copies a string argument after checking its size and encoding.
///
/// # Safety
/// `cd` is a valid calldata.
unsafe fn string(cd: *const sys::calldata_t, name: &CStr, limit: usize) -> Result<String, String> {
    let shown = name.to_string_lossy();
    // SAFETY: per the caller.
    let Some(text) = (unsafe { get_str(cd, name) }) else {
        return Err(format!("{shown} must be a string"));
    };
    let bytes = text.to_bytes();
    if bytes.len() > limit {
        return Err(format!("{shown} is longer than {limit} bytes"));
    }
    String::from_utf8(bytes.to_vec()).map_err(|_| format!("{shown} must be UTF-8"))
}

unsafe extern "C" fn on_proc<const P: usize>(data: *mut c_void, cd: *mut sys::calldata_t) {
    guard("dstreamy proc", (), || {
        // SAFETY: `data` is the static gate `register` passed with this proc.
        let gate = unsafe { &*data.cast_const().cast::<Gate>() };
        // SAFETY: libobs passes the call's live calldata.
        let result = unsafe { gate.admit(PROCS[P].0, cd) };
        // SAFETY: as above.
        unsafe {
            set_bool(cd, c"accepted", result.is_ok());
            if let Err(e) = result {
                let e = std::ffi::CString::new(e.replace('\0', " ")).expect("NULs replaced");
                set_str(cd, c"error", &e);
            }
        }
    });
}

type Callback = unsafe extern "C" fn(*mut c_void, *mut sys::calldata_t);

const CALLBACKS: [Callback; 11] = [
    on_proc::<0>,
    on_proc::<1>,
    on_proc::<2>,
    on_proc::<3>,
    on_proc::<4>,
    on_proc::<5>,
    on_proc::<6>,
    on_proc::<7>,
    on_proc::<8>,
    on_proc::<9>,
    on_proc::<10>,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Proc {
    Start,
    Stop,
    Status,
    Guilds,
    Channels,
    GetSettings,
    SetSettings,
    StreamStarting,
    StreamStarted,
    StreamStopped,
    Shutdown,
}

const PROCS: [(Proc, &CStr); 11] = [
    (Proc::Start, c"void dstreamy_start(in int request_id, out bool accepted, out string error)"),
    (Proc::Stop, c"void dstreamy_stop(in int request_id, out bool accepted, out string error)"),
    (Proc::Status, c"void dstreamy_status(in int request_id, out bool accepted, out string error)"),
    (Proc::Guilds, c"void dstreamy_guilds(in int request_id, out bool accepted, out string error)"),
    (
        Proc::Channels,
        c"void dstreamy_channels(in int request_id, in string guild_id, out bool accepted, out string error)",
    ),
    (
        Proc::GetSettings,
        c"void dstreamy_get_settings(in int request_id, out bool accepted, out string error)",
    ),
    (
        Proc::SetSettings,
        c"void dstreamy_set_settings(in int request_id, in string json, out bool accepted, out string error)",
    ),
    (
        Proc::StreamStarting,
        c"void dstreamy_stream_starting(in int generation, in ptr output, in string classification, out bool accepted, out string error)",
    ),
    (
        Proc::StreamStarted,
        c"void dstreamy_stream_started(in int generation, in ptr output, in string classification, out bool accepted, out string error)",
    ),
    (
        Proc::StreamStopped,
        c"void dstreamy_stream_stopped(in int generation, out bool accepted, out string error)",
    ),
    (Proc::Shutdown, c"void dstreamy_shutdown(in int request_id, out bool accepted, out string error)"),
];

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// # Safety
/// `cd` is a valid calldata.
pub unsafe fn get_int(cd: *const sys::calldata_t, name: &CStr) -> Option<i64> {
    let mut v: i64 = 0;
    // SAFETY: `v` has the exact size libobs stores for `int`.
    unsafe { sys::calldata_get_data(cd, name.as_ptr(), (&raw mut v).cast(), size_of::<i64>()) }
        .then_some(v)
}

/// # Safety
/// `cd` is a valid calldata.
#[cfg(test)]
pub unsafe fn get_bool(cd: *const sys::calldata_t, name: &CStr) -> Option<bool> {
    let mut v = false;
    // SAFETY: `v` has the exact size libobs stores for `bool`.
    unsafe { sys::calldata_get_data(cd, name.as_ptr(), (&raw mut v).cast(), size_of::<bool>()) }
        .then_some(v)
}

/// # Safety
/// `cd` is a valid calldata; a string it holds is NUL-terminated.
pub unsafe fn get_str<'a>(cd: *const sys::calldata_t, name: &CStr) -> Option<&'a CStr> {
    let mut p: *const c_char = ptr::null();
    // SAFETY: per the caller.
    unsafe { sys::calldata_get_string(cd, name.as_ptr(), &mut p) && !p.is_null() }
        .then(|| unsafe { CStr::from_ptr(p) })
}

/// # Safety
/// `cd` is a valid calldata.
#[cfg(test)]
pub unsafe fn set_int(cd: *mut sys::calldata_t, name: &CStr, v: i64) {
    // SAFETY: libobs copies the value.
    unsafe { sys::calldata_set_data(cd, name.as_ptr(), (&raw const v).cast(), size_of::<i64>()) }
}

/// # Safety
/// `cd` is a valid calldata.
pub unsafe fn set_bool(cd: *mut sys::calldata_t, name: &CStr, v: bool) {
    // SAFETY: libobs copies the value.
    unsafe { sys::calldata_set_data(cd, name.as_ptr(), (&raw const v).cast(), size_of::<bool>()) }
}

/// # Safety
/// `cd` is a valid calldata.
#[cfg(test)]
pub unsafe fn set_ptr(cd: *mut sys::calldata_t, name: &CStr, v: *mut c_void) {
    // SAFETY: libobs copies the value.
    unsafe {
        sys::calldata_set_data(
            cd,
            name.as_ptr(),
            (&raw const v).cast(),
            size_of::<*mut c_void>(),
        )
    }
}

/// # Safety
/// `cd` is a valid calldata.
pub unsafe fn set_str(cd: *mut sys::calldata_t, name: &CStr, v: &CStr) {
    let bytes = v.to_bytes_with_nul();
    // SAFETY: libobs copies the bytes, NUL included.
    unsafe { sys::calldata_set_data(cd, name.as_ptr(), bytes.as_ptr().cast(), bytes.len()) }
}

/// A heap calldata, freed on drop.
pub struct Calldata(sys::calldata_t);

impl Calldata {
    pub fn new() -> Self {
        Self(sys::calldata_t {
            stack: ptr::null_mut(),
            size: 0,
            capacity: 0,
            fixed: false,
        })
    }

    pub fn as_ptr(&mut self) -> *mut sys::calldata_t {
        &raw mut self.0
    }
}

impl Drop for Calldata {
    fn drop(&mut self) {
        if !self.0.fixed {
            // SAFETY: the stack was allocated by calldata_set_data with bmalloc, or is NULL.
            unsafe { sys::bfree(self.0.stack.cast()) };
        }
    }
}

#[cfg(test)]
pub mod tests {
    use std::collections::HashMap;
    use std::ffi::CString;
    use std::sync::LazyLock;
    use std::thread;

    use super::*;

    static HELD: LazyLock<Mutex<HashMap<usize, i64>>> = LazyLock::new(Default::default);

    /// An output pointer the fake refs treat as already being destroyed.
    const DYING: usize = 0xdead_0000;

    pub const FAKE_REFS: Refs = Refs {
        acquire: |p| {
            if p as usize == DYING {
                return false;
            }
            *lock(&HELD).entry(p as usize).or_default() += 1;
            true
        },
        release: |p| *lock(&HELD).entry(p as usize).or_default() -= 1,
    };

    pub fn held(base: usize) -> i64 {
        lock(&HELD)
            .iter()
            .filter(|(p, _)| (base..base + 0x1000).contains(p))
            .map(|(_, n)| n)
            .sum()
    }

    pub fn fake_output(base: usize, n: usize) -> *mut c_void {
        (base + n * 8) as *mut c_void
    }

    pub struct Handlers {
        pub procs: *mut sys::proc_handler_t,
        pub signals: *mut sys::signal_handler_t,
    }

    impl Handlers {
        pub fn new() -> Self {
            // SAFETY: plain constructors.
            unsafe {
                Self {
                    procs: sys::proc_handler_create(),
                    signals: sys::signal_handler_create(),
                }
            }
        }
    }

    impl Drop for Handlers {
        fn drop(&mut self) {
            // SAFETY: created in `new`; no thread calls them anymore.
            unsafe {
                sys::proc_handler_destroy(self.procs);
                sys::signal_handler_destroy(self.signals);
            }
        }
    }

    #[derive(Clone, Copy)]
    pub enum Arg<'a> {
        Int(&'a str, i64),
        Bool(&'a str, bool),
        Str(&'a str, &'a [u8]),
        Ptr(&'a str, *mut c_void),
    }

    #[derive(Debug, PartialEq, Eq)]
    pub struct Reply {
        pub found: bool,
        pub accepted: Option<bool>,
        pub error: Option<String>,
    }

    pub fn call(procs: *mut sys::proc_handler_t, name: &str, args: &[Arg]) -> Reply {
        let mut cd = Calldata::new();
        let name = CString::new(name).unwrap();
        for arg in args {
            // SAFETY: cd is a fresh calldata.
            unsafe {
                match *arg {
                    Arg::Int(n, v) => set_int(cd.as_ptr(), &CString::new(n).unwrap(), v),
                    Arg::Bool(n, v) => set_bool(cd.as_ptr(), &CString::new(n).unwrap(), v),
                    Arg::Str(n, v) => set_str(
                        cd.as_ptr(),
                        &CString::new(n).unwrap(),
                        &CString::new(v).unwrap(),
                    ),
                    Arg::Ptr(n, v) => set_ptr(cd.as_ptr(), &CString::new(n).unwrap(), v),
                }
            }
        }
        // SAFETY: valid handler and calldata.
        let found = unsafe { sys::proc_handler_call(procs, name.as_ptr(), cd.as_ptr()) };
        // SAFETY: cd is valid until dropped below.
        let (accepted, error) = unsafe {
            (
                get_bool(cd.as_ptr(), c"accepted"),
                get_str(cd.as_ptr(), c"error").map(|s| s.to_string_lossy().into_owned()),
            )
        };
        Reply {
            found,
            accepted,
            error,
        }
    }

    pub fn accepted() -> Reply {
        Reply {
            found: true,
            accepted: Some(true),
            error: None,
        }
    }

    pub fn refused(error: &str) -> Reply {
        Reply {
            found: true,
            accepted: Some(false),
            error: Some(error.to_owned()),
        }
    }

    pub struct Fixture {
        pub gate: &'static Gate,
        pub handlers: Handlers,
        pub requests: mpsc::Receiver<Admitted>,
        pub shared: Arc<Shared>,
    }

    pub fn fixture() -> Fixture {
        let gate: &'static Gate = Box::leak(Box::new(Gate::new(FAKE_REFS)));
        let handlers = Handlers::new();
        assert!(gate.register(handlers.procs, handlers.signals));
        let (tx, requests) = mpsc::channel(QUEUE);
        let shared = Arc::new(Shared::default());
        gate.open(tx, shared.clone());
        Fixture {
            gate,
            handlers,
            requests,
            shared,
        }
    }

    impl Fixture {
        pub fn call(&self, name: &str, args: &[Arg]) -> Reply {
            call(self.handlers.procs, name, args)
        }

        fn received(&mut self) -> Vec<Admitted> {
            let mut v = Vec::new();
            while let Ok(a) = self.requests.try_recv() {
                v.push(a);
            }
            v
        }
    }

    fn id(n: i64) -> Arg<'static> {
        Arg::Int("request_id", n)
    }

    const ALL: [&str; 11] = [
        "dstreamy_start",
        "dstreamy_stop",
        "dstreamy_status",
        "dstreamy_guilds",
        "dstreamy_channels",
        "dstreamy_get_settings",
        "dstreamy_set_settings",
        "dstreamy_stream_starting",
        "dstreamy_stream_started",
        "dstreamy_stream_stopped",
        "dstreamy_shutdown",
    ];

    fn valid_args(name: &str, n: i64, output: *mut c_void) -> Vec<Arg<'static>> {
        match name {
            "dstreamy_channels" => vec![id(n), Arg::Str("guild_id", b"g")],
            "dstreamy_set_settings" => vec![id(n), Arg::Str("json", b"{}")],
            "dstreamy_stream_starting" | "dstreamy_stream_started" => vec![
                Arg::Int("generation", 1),
                Arg::Ptr("output", output),
                Arg::Str("classification", b"conventional"),
            ],
            "dstreamy_stream_stopped" => vec![Arg::Int("generation", 1)],
            _ => vec![id(n)],
        }
    }

    #[test]
    fn every_declaration_is_registered_and_admits() {
        let base = 0x10_0000;
        let mut f = fixture();
        for (n, name) in ALL.iter().enumerate() {
            let reply = f.call(name, &valid_args(name, n as i64 + 1, fake_output(base, 0)));
            assert_eq!(reply, accepted(), "{name}");
        }
        let got = f.received();
        assert_eq!(
            got,
            vec![
                Admitted {
                    id: 1,
                    request: Request::Start
                },
                Admitted {
                    id: 2,
                    request: Request::Stop
                },
                Admitted {
                    id: 3,
                    request: Request::Status
                },
                Admitted {
                    id: 4,
                    request: Request::Guilds
                },
                Admitted {
                    id: 5,
                    request: Request::Channels {
                        guild_id: "g".into()
                    }
                },
                Admitted {
                    id: 6,
                    request: Request::GetSettings
                },
                Admitted {
                    id: 7,
                    request: Request::SetSettings { json: "{}".into() }
                },
            ]
        );
        assert!(f.shared.priority.take_stop());
        assert_eq!(f.shared.priority.take_shutdown(), Some(11));
        let seen = f.shared.mailbox.view().unwrap();
        assert_eq!(
            (seen.generation, seen.stopped, seen.output),
            (1, true, false)
        );
        assert_eq!(held(base), 0);
    }

    #[test]
    fn a_second_registration_adds_nothing() {
        let f = fixture();
        assert!(!f.gate.register(f.handlers.procs, f.handlers.signals));
        let other = Handlers::new();
        assert!(!f.gate.register(other.procs, other.signals));
        assert!(!call(other.procs, "dstreamy_status", &[id(1)]).found);
    }

    #[test]
    fn an_unknown_proc_is_not_found() {
        let f = fixture();
        assert!(!f.call("dstreamy_launch", &[id(1)]).found);
    }

    #[test]
    fn the_state_signal_is_declared() {
        let f = fixture();
        let mut cd = Calldata::new();
        // SAFETY: valid calldata.
        unsafe { set_str(cd.as_ptr(), c"json", c"{}") };
        // SAFETY: a declared signal with no subscribers is a no-op; an undeclared one only logs.
        unsafe { sys::signal_handler_signal(f.handlers.signals, SIGNAL.as_ptr(), cd.as_ptr()) };
        assert!(!unsafe { sys::signal_handler_add(f.handlers.signals, SIGNAL_DECL.as_ptr()) });
    }

    #[test]
    fn bad_request_ids_are_refused() {
        let mut f = fixture();
        let bad: [&[Arg]; 5] = [
            &[],
            &[id(0)],
            &[id(-3)],
            &[Arg::Bool("request_id", true)],
            &[Arg::Str("request_id", b"1")],
        ];
        for args in bad {
            let reply = f.call("dstreamy_status", args);
            assert_eq!(reply.accepted, Some(false), "{reply:?}");
            assert!(reply.error.unwrap().contains("request_id"));
        }
        assert!(f.received().is_empty());
    }

    #[test]
    fn string_inputs_are_checked_and_copied() {
        let mut f = fixture();
        for guild in [&b""[..], &[0xff, 0xfe], &[b'x'; MAX_ID_BYTES + 1]] {
            let reply = f.call("dstreamy_channels", &[id(1), Arg::Str("guild_id", guild)]);
            assert_eq!(reply.accepted, Some(false), "{guild:?}");
            assert!(reply.error.unwrap().contains("guild_id"));
        }
        assert_eq!(f.call("dstreamy_channels", &[id(1)]).accepted, Some(false));
        let json = vec![b' '; MAX_SETTINGS_BYTES + 1];
        let reply = f.call("dstreamy_set_settings", &[id(2), Arg::Str("json", &json)]);
        assert_eq!(reply.accepted, Some(false));
        assert!(reply.error.unwrap().contains("json"));
        assert_eq!(
            f.call("dstreamy_set_settings", &[id(2)]).accepted,
            Some(false)
        );
        assert!(f.received().is_empty());

        let longest = "ギ".repeat(MAX_ID_BYTES / 3);
        assert_eq!(
            f.call(
                "dstreamy_channels",
                &[id(3), Arg::Str("guild_id", longest.as_bytes())]
            ),
            accepted()
        );
        let mut biggest = vec![b' '; MAX_SETTINGS_BYTES - 2];
        biggest.splice(0..0, *b"{}");
        assert_eq!(
            f.call(
                "dstreamy_set_settings",
                &[id(4), Arg::Str("json", &biggest)]
            ),
            accepted()
        );
        let got = f.received();
        assert_eq!(got[0].request, Request::Channels { guild_id: longest });
        assert_eq!(
            got[1].request,
            Request::SetSettings {
                json: String::from_utf8(biggest).unwrap()
            }
        );
    }

    #[test]
    fn observation_inputs_are_checked() {
        let base = 0x20_0000;
        let mut f = fixture();
        let out = Arg::Ptr("output", fake_output(base, 0));
        let class = Arg::Str("classification", b"conventional");
        let bad: [&[Arg]; 7] = [
            &[out, class],
            &[Arg::Int("generation", 0), out, class],
            &[Arg::Int("generation", -1), out, class],
            &[Arg::Int("generation", 1), class],
            &[Arg::Int("generation", 1), out],
            &[
                Arg::Int("generation", 1),
                out,
                Arg::Str("classification", b"Conventional"),
            ],
            &[
                Arg::Int("generation", 1),
                out,
                Arg::Str("classification", b"enhanced "),
            ],
        ];
        for args in bad {
            assert_eq!(
                f.call("dstreamy_stream_starting", args).accepted,
                Some(false)
            );
        }
        assert_eq!(f.call("dstreamy_stream_stopped", &[]).accepted, Some(false));
        assert_eq!(
            f.call("dstreamy_stream_stopped", &[Arg::Int("generation", 0)])
                .accepted,
            Some(false)
        );
        assert!(f.shared.mailbox.view().is_none());
        assert_eq!(held(base), 0);
        assert!(f.received().is_empty());
    }

    #[test]
    fn a_full_queue_is_busy_not_blocking() {
        let mut f = fixture();
        for n in 1..=QUEUE as i64 {
            assert_eq!(f.call("dstreamy_status", &[id(n)]), accepted());
        }
        assert_eq!(f.call("dstreamy_status", &[id(100)]), refused(BUSY));
        let first = f.requests.try_recv().unwrap();
        f.shared.finish(first.id);
        assert_eq!(f.call("dstreamy_status", &[id(100)]), accepted());
    }

    #[test]
    fn priority_paths_ignore_a_full_queue() {
        let base = 0x80_0000;
        let mut f = fixture();
        for n in 1..=QUEUE as i64 {
            assert_eq!(f.call("dstreamy_status", &[id(n)]), accepted());
        }
        assert_eq!(f.call("dstreamy_stop", &[id(100)]), refused(BUSY));
        assert!(!f.shared.priority.take_stop());

        let observe = |name: &str, out: *mut c_void| {
            f.call(
                name,
                &[
                    Arg::Int("generation", 1),
                    Arg::Ptr("output", out),
                    Arg::Str("classification", b"conventional"),
                ],
            )
        };
        let (a, b) = (fake_output(base, 0), fake_output(base, 1));
        assert_eq!(observe("dstreamy_stream_starting", a), accepted());
        assert_eq!(observe("dstreamy_stream_started", b), accepted());
        let seen = f.shared.mailbox.view().unwrap();
        assert_eq!((seen.generation, seen.revision, seen.output), (1, 1, true));
        assert_eq!(held(base), 1);
        assert_eq!(
            f.call("dstreamy_stream_stopped", &[Arg::Int("generation", 1)]),
            accepted()
        );
        assert!(f.shared.mailbox.view().unwrap().stopped);
        assert_eq!(held(base), 0);

        assert_eq!(f.call("dstreamy_shutdown", &[id(101)]), accepted());
        assert_eq!(f.shared.priority.take_shutdown(), Some(101));
        assert!(!f.shared.priority.take_stop());
        assert_eq!(f.received().len(), QUEUE);
        assert_eq!(held(base), 0);
    }

    #[test]
    fn an_outstanding_id_is_refused_until_finished() {
        let mut f = fixture();
        assert_eq!(f.call("dstreamy_status", &[id(7)]), accepted());
        let reply = f.call("dstreamy_guilds", &[id(7)]);
        assert_eq!(reply.accepted, Some(false));
        assert!(reply.error.unwrap().contains('7'));
        f.shared.finish(7);
        assert_eq!(f.call("dstreamy_guilds", &[id(7)]), accepted());
        assert_eq!(f.received().len(), 2);
    }

    #[test]
    fn observations_follow_generation_and_revision() {
        let base = 0x30_0000;
        let f = fixture();
        let starting = |g: i64, out: *mut c_void, class: &'static [u8]| {
            f.call(
                "dstreamy_stream_starting",
                &[
                    Arg::Int("generation", g),
                    Arg::Ptr("output", out),
                    Arg::Str("classification", class),
                ],
            )
        };
        let (a, b) = (fake_output(base, 0), fake_output(base, 1));
        assert_eq!(starting(3, a, b"unknown"), accepted());
        assert_eq!(f.shared.mailbox.take_change(), Some("stream_starting"));
        assert_eq!(held(base), 1);

        assert_eq!(starting(3, a, b"unknown"), accepted());
        let same = f.call(
            "dstreamy_stream_started",
            &[
                Arg::Int("generation", 3),
                Arg::Ptr("output", a),
                Arg::Str("classification", b"unknown"),
            ],
        );
        assert_eq!(same, accepted());
        assert_eq!(f.shared.mailbox.take_change(), None);
        assert_eq!(f.shared.mailbox.view().unwrap().revision, 0);
        assert_eq!(held(base), 1);

        assert_eq!(starting(3, b, b"enhanced"), accepted());
        let seen = f.shared.mailbox.view().unwrap();
        assert_eq!(
            (seen.generation, seen.revision, seen.classification),
            (3, 1, Classification::Enhanced)
        );
        assert_eq!(held(base), 1);

        let stale = starting(2, a, b"unknown");
        assert_eq!(stale.accepted, Some(false));
        assert_eq!(held(base), 1);

        assert_eq!(starting(4, ptr::null_mut(), b"enhanced"), accepted());
        let seen = f.shared.mailbox.view().unwrap();
        assert_eq!((seen.generation, seen.revision, seen.output), (4, 0, false));
        assert_eq!(held(base), 0);

        assert_eq!(
            starting(5, DYING as *mut c_void, b"conventional"),
            accepted()
        );
        assert!(!f.shared.mailbox.view().unwrap().output);

        assert_eq!(starting(6, a, b"conventional"), accepted());
        assert_eq!(held(base), 1);
        let _ = f.shared.mailbox.take_change();
        assert_eq!(
            f.call("dstreamy_stream_stopped", &[Arg::Int("generation", 6)]),
            accepted()
        );
        assert_eq!(f.shared.mailbox.take_change(), Some("stream_stopped"));
        assert_eq!(held(base), 0);
        assert_eq!(
            f.call("dstreamy_stream_stopped", &[Arg::Int("generation", 6)]),
            accepted()
        );
        assert_eq!(
            f.call("dstreamy_stream_stopped", &[Arg::Int("generation", 5)]),
            accepted()
        );
        assert_eq!(f.shared.mailbox.take_change(), None);
        let late = f.call(
            "dstreamy_stream_started",
            &[
                Arg::Int("generation", 6),
                Arg::Ptr("output", a),
                Arg::Str("classification", b"conventional"),
            ],
        );
        assert_eq!(late.accepted, Some(false));
        assert!(f.shared.mailbox.view().unwrap().stopped);
        assert_eq!(held(base), 0);

        assert_eq!(starting(7, a, b"conventional"), accepted());
        f.shared.mailbox.close();
        assert_eq!(held(base), 0);
        assert_eq!(starting(8, a, b"conventional").accepted, Some(false));
        assert_eq!(held(base), 0);
    }

    #[test]
    fn shutdown_closes_admission_at_once() {
        let mut f = fixture();
        assert_eq!(f.call("dstreamy_shutdown", &[id(9)]), accepted());
        for name in ALL {
            assert_eq!(
                f.call(name, &valid_args(name, 20, ptr::null_mut())),
                refused(SHUTTING_DOWN),
                "{name}"
            );
        }
        assert_eq!(f.shared.priority.take_shutdown(), Some(9));
        assert!(f.received().is_empty());
    }

    #[test]
    fn after_close_every_call_is_refused_and_releases_its_ref() {
        let base = 0x40_0000;
        let f = fixture();
        f.gate.close();
        for _ in 0..3 {
            for name in ALL {
                assert_eq!(
                    f.call(name, &valid_args(name, 1, fake_output(base, 0))),
                    refused(SHUTTING_DOWN),
                    "{name}"
                );
            }
        }
        assert_eq!(held(base), 0);
    }

    #[test]
    fn before_open_calls_are_refused_as_loading() {
        let gate: &'static Gate = Box::leak(Box::new(Gate::new(FAKE_REFS)));
        let handlers = Handlers::new();
        assert!(gate.register(handlers.procs, handlers.signals));
        assert_eq!(
            call(handlers.procs, "dstreamy_status", &[id(1)]),
            refused(LOADING)
        );
    }

    #[test]
    fn concurrent_calls_and_close_either_enter_or_refuse() {
        let base = 0x50_0000;
        let mut f = fixture();
        let procs = f.handlers.procs as usize;
        let accepted_requests = Arc::new(AtomicI64::new(0));
        let workers: Vec<_> = (0..4)
            .map(|t| {
                let accepted_requests = accepted_requests.clone();
                thread::spawn(move || {
                    let procs = procs as *mut sys::proc_handler_t;
                    for n in 0..200i64 {
                        let rid = t * 1000 + n + 1;
                        let r = call(procs, "dstreamy_get_settings", &[id(rid)]);
                        match r.accepted {
                            Some(true) => {
                                accepted_requests.fetch_add(1, Ordering::Relaxed);
                            }
                            _ => assert!(
                                matches!(r.error.as_deref(), Some(BUSY | SHUTTING_DOWN)),
                                "{r:?}"
                            ),
                        }
                        let o = call(
                            procs,
                            "dstreamy_stream_starting",
                            &[
                                Arg::Int("generation", rid),
                                Arg::Ptr("output", fake_output(base, (t * 8) as usize)),
                                Arg::Str("classification", b"unknown"),
                            ],
                        );
                        assert!(
                            o.found && (o.accepted == Some(true) || o.error.is_some()),
                            "{o:?}"
                        );
                    }
                })
            })
            .collect();
        let mut drained = 0;
        for _ in 0..50 {
            drained += f.received().len();
            f.shared.clear();
            thread::yield_now();
        }
        f.gate.close();
        for w in workers {
            w.join().unwrap();
        }
        drained += f.received().len();
        assert_eq!(drained as i64, accepted_requests.load(Ordering::Relaxed));
        f.shared.mailbox.close();
        assert_eq!(held(base), 0);
        for name in ALL {
            assert_eq!(
                f.call(name, &valid_args(name, 1, fake_output(base, 0))),
                refused(SHUTTING_DOWN)
            );
        }
        assert_eq!(held(base), 0);
    }
}
