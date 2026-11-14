//! Tools > D-Streamy… and frontend events through obs-frontend-api, looked up at runtime so libobs-only hosts load.

use std::ffi::{CStr, c_int, c_void};
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

use dstreamy_obs_sys as sys;
use libloading::Library;

use crate::guard::{guard, lock};
use crate::log;
use crate::settings_source;

const TOOLS_ITEM: &CStr = c"D-Streamy\u{2026}";

struct Frontend {
    add_tools_menu_item: sys::dstreamy_frontend_add_tools_menu_item,
    open_source_properties: sys::dstreamy_frontend_open_source_properties,
    _host: Library,
}

static FRONTEND: OnceLock<Frontend> = OnceLock::new();

#[cfg(unix)]
pub(crate) fn host() -> Result<Library, Missing> {
    Ok(libloading::os::unix::Library::this().into())
}

#[cfg(windows)]
pub(crate) fn host() -> Result<Library, Missing> {
    libloading::os::windows::Library::open_already_loaded("obs-frontend-api.dll")
        .map(Into::into)
        .map_err(|_| Missing::Library)
}

/// Why the Tools entry is unavailable.
pub(crate) enum Missing {
    #[cfg_attr(unix, allow(dead_code))]
    Library,
    Frontend,
    Symbol(&'static str),
}

const ADD_TOOLS_MENU_ITEM: &str = "obs_frontend_add_tools_menu_item";
const ADD_EVENT_CALLBACK: &str = "obs_frontend_add_event_callback";
const REMOVE_EVENT_CALLBACK: &str = "obs_frontend_remove_event_callback";
const ADD_SAVE_CALLBACK: &str = "obs_frontend_add_save_callback";
const REMOVE_SAVE_CALLBACK: &str = "obs_frontend_remove_save_callback";
const ADD_PRELOAD_CALLBACK: &str = "obs_frontend_add_preload_callback";
const REMOVE_PRELOAD_CALLBACK: &str = "obs_frontend_remove_preload_callback";
const OPEN_SOURCE_PROPERTIES: &str = "obs_frontend_open_source_properties";
const GET_STREAMING_OUTPUT: &str = "obs_frontend_get_streaming_output";

impl Frontend {
    fn resolve() -> Result<Self, Missing> {
        let host = host()?;
        // SAFETY: wrapper.h asserts these types against obs-frontend-api.h; `_host` keeps the library loaded.
        let (add, open) = unsafe {
            (
                host.get::<sys::dstreamy_frontend_add_tools_menu_item>(
                    ADD_TOOLS_MENU_ITEM.as_bytes(),
                )
                .ok()
                .and_then(|s| *s),
                host.get::<sys::dstreamy_frontend_open_source_properties>(
                    OPEN_SOURCE_PROPERTIES.as_bytes(),
                )
                .ok()
                .and_then(|s| *s),
            )
        };
        match (add, open) {
            (Some(add), Some(open)) => Ok(Self {
                add_tools_menu_item: Some(add),
                open_source_properties: Some(open),
                _host: host,
            }),
            (None, None) => Err(Missing::Frontend),
            (None, Some(_)) => Err(Missing::Symbol(ADD_TOOLS_MENU_ITEM)),
            (Some(_), None) => Err(Missing::Symbol(OPEN_SOURCE_PROPERTIES)),
        }
    }
}

struct Watch {
    remove_event: sys::dstreamy_frontend_event_callback,
    /// Set only while the event callback is registered, so share mode hears every start and stop of the main stream.
    streaming_output: sys::dstreamy_frontend_get_output,
    remove_save: sys::dstreamy_frontend_save_callback,
    remove_preload: sys::dstreamy_frontend_save_callback,
    _host: Library,
}

static WATCH: Mutex<Option<Watch>> = Mutex::new(None);
/// Set once the frontend sent its exit event, after which its API is gone.
static EXITED: AtomicBool = AtomicBool::new(false);

/// # Safety
/// `T` is the type of the symbol `name`.
unsafe fn symbol<T: Copy>(host: &Library, name: &str) -> Option<T> {
    // SAFETY: the caller's contract.
    unsafe { host.get::<T>(name.as_bytes()) }.ok().map(|s| *s)
}

/// Follows the frontend's exit, which comes before `obs_shutdown` stops audio and video, and its scene collections.
fn watch() {
    let Ok(host) = host() else {
        return;
    };
    // SAFETY: wrapper.h asserts these types against obs-frontend-api.h; `_host` keeps the library loaded.
    let (add, remove, add_save, remove_save, add_preload, remove_preload, streaming_output) = unsafe {
        (
            symbol::<sys::dstreamy_frontend_event_callback>(&host, ADD_EVENT_CALLBACK).flatten(),
            symbol::<sys::dstreamy_frontend_event_callback>(&host, REMOVE_EVENT_CALLBACK).flatten(),
            symbol::<sys::dstreamy_frontend_save_callback>(&host, ADD_SAVE_CALLBACK).flatten(),
            symbol::<sys::dstreamy_frontend_save_callback>(&host, REMOVE_SAVE_CALLBACK).flatten(),
            symbol::<sys::dstreamy_frontend_save_callback>(&host, ADD_PRELOAD_CALLBACK).flatten(),
            symbol::<sys::dstreamy_frontend_save_callback>(&host, REMOVE_PRELOAD_CALLBACK)
                .flatten(),
            symbol::<sys::dstreamy_frontend_get_output>(&host, GET_STREAMING_OUTPUT).flatten(),
        )
    };
    EXITED.store(false, Ordering::Release);
    let events = match (add, remove) {
        (Some(add), Some(remove)) => {
            // SAFETY: module load runs on the UI thread; the callback lives as long as the process.
            unsafe { add(Some(on_event), ptr::null_mut()) };
            Some(remove)
        }
        (None, None) => None,
        _ => {
            log::warn(&format!(
                "OBS frontend lacks {ADD_EVENT_CALLBACK} or {REMOVE_EVENT_CALLBACK}: \
                 D-Streamy stops its output only when OBS unloads it"
            ));
            None
        }
    };
    let collections = match (add_save, remove_save, add_preload, remove_preload) {
        (Some(add_save), Some(remove_save), Some(add_preload), Some(remove_preload)) => {
            // SAFETY: as above.
            unsafe {
                add_save(Some(on_save), ptr::null_mut());
                add_preload(Some(on_preload), ptr::null_mut());
            }
            Some((remove_save, remove_preload))
        }
        (None, None, None, None) => None,
        _ => {
            log::warn(
                "OBS frontend lacks its scene-collection save callbacks: \
                 the Discord tracks D-Streamy manages are forgotten when OBS quits",
            );
            None
        }
    };
    if events.is_none() && collections.is_none() {
        return;
    }
    if events.is_some() && streaming_output.is_none() {
        log::warn(&format!(
            "OBS frontend lacks {GET_STREAMING_OUTPUT}: share mode can't find the stream's encoder"
        ));
    }
    *lock(&WATCH) = Some(Watch {
        remove_event: events,
        streaming_output: events.and(streaming_output),
        remove_save: collections.map(|c| c.0),
        remove_preload: collections.map(|c| c.1),
        _host: host,
    });
}

extern "C" fn on_event(event: c_int, _private_data: *mut c_void) {
    guard("frontend event", (), || match u32::try_from(event) {
        Ok(sys::DSTREAMY_FRONTEND_EVENT_EXIT) => {
            EXITED.store(true, Ordering::Release);
            drop(crate::encoders::main_stream_ending());
            crate::hooks::observe("frontend_exit", || serde_json::json!({}));
            log::info(
                "OBS's frontend is exiting: D-Streamy stops before OBS stops audio and video",
            );
            crate::session::shutdown();
        }
        Ok(sys::DSTREAMY_FRONTEND_EVENT_STREAMING_STARTING) => main_stream_starting(),
        Ok(
            sys::DSTREAMY_FRONTEND_EVENT_STREAMING_STOPPING
            | sys::DSTREAMY_FRONTEND_EVENT_STREAMING_STOPPED,
        ) => main_stream_ending(),
        Ok(sys::DSTREAMY_FRONTEND_EVENT_SCENE_COLLECTION_CLEANUP) => {
            crate::audio::collection_cleared();
        }
        // Both come before OBS leaves the collection or quits and closes every dialog.
        Ok(
            sys::DSTREAMY_FRONTEND_EVENT_SCENE_COLLECTION_CHANGING
            | sys::DSTREAMY_FRONTEND_EVENT_SCRIPTING_SHUTDOWN,
        ) => settings_source::collection_leaving(),
        _ => {}
    });
}

fn streaming_output() -> sys::dstreamy_frontend_get_output {
    lock(&WATCH)
        .as_ref()
        .and_then(|w| w.streaming_output)
        .filter(|_| !EXITED.load(Ordering::Acquire))
}

/// Whether share mode learns the stream's encoder from the frontend's streaming events; else from the output.
pub fn follows_streaming() -> bool {
    streaming_output().is_some()
}

/// STREAMING_STARTING, on the UI thread: the frontend has set up its output and encoders, which start next.
fn main_stream_starting() {
    let Some(get) = streaming_output() else {
        return;
    };
    // SAFETY: frontend events run on the UI thread, where the output handler may be read; only weak refs are kept.
    let found = unsafe {
        let output = get();
        if output.is_null() {
            None
        } else {
            let found = crate::encoders::MainStream::of(output);
            sys::obs_output_release(output);
            found
        }
    };
    let (stream, id) = found.unzip();
    let multitrack = stream
        .as_ref()
        .is_some_and(crate::encoders::MainStream::multitrack);
    crate::hooks::observe(
        "main_stream_starting",
        || serde_json::json!({"encoder": id, "multitrack": multitrack}),
    );
    match &id {
        Some(id) => log::info(&format!(
            "OBS's stream is starting with the video encoder {id}"
        )),
        None => log::info("OBS's stream is starting without a video encoder D-Streamy can see"),
    }
    crate::encoders::main_stream_starting(stream);
}

/// STREAMING_STOPPING or STOPPED (a failed or lost stream sends only STOPPED), on the UI thread.
fn main_stream_ending() {
    if let Some(stream) = crate::encoders::main_stream_ending() {
        crate::hooks::observe("main_stream_ending", || serde_json::json!({}));
        log::info("OBS's stream is stopping: D-Streamy stops sharing its encoder");
        crate::session::main_stream_ended(stream);
    }
}

extern "C" fn on_save(data: *mut sys::obs_data_t, saving: bool, _private_data: *mut c_void) {
    guard("frontend save", (), || {
        if !saving {
            crate::audio::collection_loaded();
        } else if !data.is_null() {
            // SAFETY: the frontend passes its live "modules" data.
            unsafe { crate::audio::collection_saving(data) };
        }
    });
}

extern "C" fn on_preload(data: *mut sys::obs_data_t, _saving: bool, _private_data: *mut c_void) {
    // SAFETY: the frontend passes the collection's "modules" data, or NULL when it has none.
    guard("frontend preload", (), || unsafe {
        crate::audio::collection_loading(data)
    });
}

/// Finds OBS's frontend; call from `obs_module_load`, on the UI thread.
pub fn install() {
    watch();
    let frontend = match Frontend::resolve() {
        Ok(frontend) => frontend,
        Err(Missing::Library) => {
            log::info(
                "no OBS frontend (obs-frontend-api.dll is not loaded): Tools > D-Streamy is unavailable",
            );
            return;
        }
        Err(Missing::Frontend) => {
            crate::hooks::observe(
                "frontend_unavailable",
                || serde_json::json!({"reason":"frontend"}),
            );
            log::info("no OBS frontend: Tools > D-Streamy is unavailable");
            return;
        }
        Err(Missing::Symbol(name)) => {
            crate::hooks::observe(
                "frontend_unavailable",
                || serde_json::json!({"reason":"symbol","symbol":name}),
            );
            log::warn(&format!(
                "OBS frontend lacks {name}: Tools > D-Streamy is unavailable"
            ));
            return;
        }
    };
    FRONTEND.get_or_init(|| frontend);
}

/// Adds Tools > D-Streamy…: it opens the status-bar panel when that loaded, else the settings dialog. UI thread.
pub fn add_tools_entry() {
    let Some(frontend) = FRONTEND.get() else {
        return;
    };
    if let Some(add) = frontend.add_tools_menu_item {
        // SAFETY: module load runs on the UI thread; the frontend copies the name.
        unsafe { add(TOOLS_ITEM.as_ptr(), Some(open_settings), ptr::null_mut()) };
        crate::hooks::observe("tools_entry_added", || serde_json::json!({}));
        log::info("added Tools > D-Streamy");
        if let Some(trigger) = crate::hooks::open_dialog_trigger() {
            click_when(trigger, "clicking Tools > D-Streamy", open_settings);
        }
        if let Some(trigger) = crate::hooks::press_start_trigger() {
            click_when(trigger, "pressing Start", press_start);
        }
        if let Some(trigger) = crate::hooks::press_stop_trigger() {
            click_when(trigger, "pressing Stop", press_stop);
        }
    }
}

/// Whether [`install`] found OBS's frontend.
pub fn available() -> bool {
    FRONTEND.get().is_some()
}

static CLICKERS: Mutex<Vec<(Arc<AtomicBool>, JoinHandle<()>)>> = Mutex::new(Vec::new());

/// Test hook: once `trigger` exists, runs `task` on OBS's UI thread, as a click does.
fn click_when(trigger: PathBuf, what: &'static str, task: extern "C" fn(*mut c_void)) {
    let unloading = Arc::new(AtomicBool::new(false));
    let stop = unloading.clone();
    let thread = std::thread::spawn(move || {
        while !trigger.exists() {
            if stop.load(Ordering::Acquire) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        log::info(&format!("test hook: {what}"));
        // SAFETY: the task runs on the UI thread, where OBS runs a menu click's callback.
        unsafe { sys::obs_queue_task(sys::OBS_TASK_UI, Some(task), ptr::null_mut(), false) };
    });
    lock(&CLICKERS).push((unloading, thread));
}

extern "C" fn press_start(_private_data: *mut c_void) {
    guard("test press start", (), crate::session::start);
}

extern "C" fn press_stop(_private_data: *mut c_void) {
    guard("test press stop", (), crate::session::stop);
}

/// Stops watching the frontend and ends the test hook's thread; call from `obs_module_unload`.
pub fn shutdown() {
    drop(crate::encoders::main_stream_ending());
    let watch = lock(&WATCH).take();
    if let Some(w) = watch.filter(|_| !EXITED.load(Ordering::Acquire)) {
        // SAFETY: the frontend is still up: it never sent its exit event.
        unsafe {
            if let Some(remove) = w.remove_event {
                remove(Some(on_event), ptr::null_mut());
            }
            if let Some(remove) = w.remove_save {
                remove(Some(on_save), ptr::null_mut());
            }
            if let Some(remove) = w.remove_preload {
                remove(Some(on_preload), ptr::null_mut());
            }
        }
    }
    let clickers = std::mem::take(&mut *lock(&CLICKERS));
    for (unloading, thread) in clickers {
        unloading.store(true, Ordering::Release);
        let _ = thread.join();
    }
}

extern "C" fn open_settings(_private_data: *mut c_void) {
    guard("open_settings", (), || {
        match crate::ui::open_panel() {
            Some(true) => return log::debug("Tools > D-Streamy opened the status-bar panel"),
            Some(false) => log::warn(
                "Tools > D-Streamy could not show the status-bar panel: it opens the settings dialog",
            ),
            None => {}
        }
        let Some(open) = FRONTEND.get().and_then(|f| f.open_source_properties) else {
            return;
        };
        settings_source::opening(|| {
            // SAFETY: the type is registered at module load; a null settings object is allowed.
            let source = unsafe {
                sys::obs_source_create_private(
                    settings_source::SOURCE_ID.as_ptr(),
                    settings_source::SOURCE_NAME.as_ptr(),
                    ptr::null_mut(),
                )
            };
            if source.is_null() {
                log::warn("could not create the D-Streamy settings source");
                return;
            }
            // SAFETY: a dialog OBS opens takes its own reference, so this one is released at once.
            let opened = unsafe {
                let weak = sys::obs_source_get_weak_source(source);
                open(source);
                sys::obs_source_release(source);
                let opened = !sys::obs_weak_source_expired(weak);
                sys::obs_weak_source_release(weak);
                opened
            };
            if !opened {
                settings_source::not_opened(source);
            }
        });
    });
}
