//! Tools > D-Streamy… and the exit event through obs-frontend-api, looked up at runtime so a libobs-only host still loads the plugin.

use std::ffi::{CStr, c_int, c_void};
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread::JoinHandle;

use dstreamy_obs_sys as sys;
use libloading::Library;

use crate::guard::guard;
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
fn host() -> Result<Library, Missing> {
    Ok(libloading::os::unix::Library::this().into())
}

#[cfg(windows)]
fn host() -> Result<Library, Missing> {
    libloading::os::windows::Library::open_already_loaded("obs-frontend-api.dll")
        .map(Into::into)
        .map_err(|_| Missing::Library)
}

/// Why the Tools entry is unavailable.
enum Missing {
    #[cfg_attr(unix, allow(dead_code))]
    Library,
    Frontend,
    Symbol(&'static str),
}

const ADD_TOOLS_MENU_ITEM: &str = "obs_frontend_add_tools_menu_item";
const ADD_EVENT_CALLBACK: &str = "obs_frontend_add_event_callback";
const REMOVE_EVENT_CALLBACK: &str = "obs_frontend_remove_event_callback";
const OPEN_SOURCE_PROPERTIES: &str = "obs_frontend_open_source_properties";

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

struct ExitWatch {
    remove: sys::dstreamy_frontend_event_callback,
    _host: Library,
}

static EXIT_WATCH: Mutex<Option<ExitWatch>> = Mutex::new(None);
/// Set once the frontend sent its exit event, after which its API is gone.
static EXITED: AtomicBool = AtomicBool::new(false);

/// Shuts the session down on the frontend's exit event, which OBS sends before `obs_shutdown` stops audio and video.
fn watch_exit() {
    let Ok(host) = host() else {
        return;
    };
    // SAFETY: wrapper.h asserts these types against obs-frontend-api.h; `_host` keeps the library loaded.
    let (add, remove) = unsafe {
        (
            host.get::<sys::dstreamy_frontend_event_callback>(ADD_EVENT_CALLBACK.as_bytes())
                .ok()
                .and_then(|s| *s),
            host.get::<sys::dstreamy_frontend_event_callback>(REMOVE_EVENT_CALLBACK.as_bytes())
                .ok()
                .and_then(|s| *s),
        )
    };
    let (Some(add), Some(remove)) = (add, remove) else {
        if add.is_some() || remove.is_some() {
            log::warn(&format!(
                "OBS frontend lacks {ADD_EVENT_CALLBACK} or {REMOVE_EVENT_CALLBACK}: \
                 D-Streamy stops its output only when OBS unloads it"
            ));
        }
        return;
    };
    EXITED.store(false, Ordering::Release);
    // SAFETY: module load runs on the UI thread; the callback lives as long as the process.
    unsafe { add(Some(on_event), ptr::null_mut()) };
    *EXIT_WATCH.lock().unwrap_or_else(PoisonError::into_inner) = Some(ExitWatch {
        remove: Some(remove),
        _host: host,
    });
}

extern "C" fn on_event(event: c_int, _private_data: *mut c_void) {
    guard("frontend event", (), || {
        if u32::try_from(event) != Ok(sys::DSTREAMY_FRONTEND_EVENT_EXIT) {
            return;
        }
        EXITED.store(true, Ordering::Release);
        log::info("OBS's frontend is exiting: D-Streamy stops before OBS stops audio and video");
        crate::session::shutdown();
    });
}

/// Finds OBS's frontend; call from `obs_module_load`, on the UI thread.
pub fn install() {
    watch_exit();
    let frontend = match Frontend::resolve() {
        Ok(frontend) => frontend,
        Err(Missing::Library) => {
            log::info(
                "no OBS frontend (obs-frontend-api.dll is not loaded): Tools > D-Streamy is unavailable",
            );
            return;
        }
        Err(Missing::Frontend) => {
            log::info("no OBS frontend: Tools > D-Streamy is unavailable");
            return;
        }
        Err(Missing::Symbol(name)) => {
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
        log::info("added Tools > D-Streamy");
        if let Some(trigger) = crate::hooks::open_dialog_trigger() {
            click_when(trigger);
        }
    }
}

/// Whether [`install`] found OBS's frontend.
pub fn available() -> bool {
    FRONTEND.get().is_some()
}

static CLICKER: Mutex<Option<(Arc<AtomicBool>, JoinHandle<()>)>> = Mutex::new(None);

/// Test hook: once `trigger` exists, runs the Tools item's callback on OBS's UI thread, as a click does.
fn click_when(trigger: PathBuf) {
    let unloading = Arc::new(AtomicBool::new(false));
    let stop = unloading.clone();
    let thread = std::thread::spawn(move || {
        while !trigger.exists() {
            if stop.load(Ordering::Acquire) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        log::info("test hook: clicking Tools > D-Streamy");
        // SAFETY: the task runs on the UI thread, where OBS runs a menu click's callback.
        unsafe {
            sys::obs_queue_task(
                sys::OBS_TASK_UI,
                Some(open_settings),
                ptr::null_mut(),
                false,
            )
        };
    });
    *CLICKER.lock().unwrap_or_else(PoisonError::into_inner) = Some((unloading, thread));
}

/// Stops watching the frontend and ends the test hook's thread; call from `obs_module_unload`.
pub fn shutdown() {
    let watch = EXIT_WATCH
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    let remove = watch.and_then(|w| w.remove);
    if let Some(remove) = remove.filter(|_| !EXITED.load(Ordering::Acquire)) {
        // SAFETY: the frontend is still up: it never sent its exit event.
        unsafe { remove(Some(on_event), ptr::null_mut()) };
    }
    let clicker = CLICKER
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    if let Some((unloading, thread)) = clicker {
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
        // SAFETY: the properties dialog takes its own reference, so this one is released at once.
        unsafe {
            open(source);
            sys::obs_source_release(source);
        }
    });
}
