//! The status-bar item and its panel: an optional Qt library next to this module, fed by these callbacks.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Instant;

use dstreamy_obs_producer::live::{LiveStats, OutputSample, Sampler};
use dstreamy_obs_producer::panel::panel_view;
use dstreamy_obs_sys as sys;
use libloading::Library;

use crate::guard::guard;
use crate::log;
use crate::output::text;
use crate::session::{self, Wait};
use crate::settings_source::{self, change, with_controls};

/// `DSTREAMY_UI_ABI_VERSION` in `obs/ui/dstreamy-ui.h`.
const ABI_VERSION: u32 = 4;

// libobs scans the module's own folder for modules, never this subfolder; macOS's bundle folder isn't scanned.
#[cfg(not(target_os = "macos"))]
const FOLDER: &str = "obs-dstreamy-ui";
#[cfg(target_os = "macos")]
const FOLDER: &str = "";
#[cfg(target_os = "macos")]
const LIBRARY: &str = "obs-dstreamy-ui.dylib";
#[cfg(windows)]
const LIBRARY: &str = "obs-dstreamy-ui.dll";
#[cfg(not(any(target_os = "macos", windows)))]
const LIBRARY: &str = "obs-dstreamy-ui.so";

type Ctx = *mut c_void;

/// `struct dstreamy_ui_host` in `obs/ui/dstreamy-ui.h`.
#[repr(C)]
struct Host {
    abi_version: u32,
    ctx: Ctx,
    view_json: Option<extern "C" fn(Ctx) -> *mut c_char>,
    free_string: Option<extern "C" fn(*mut c_char)>,
    start: Option<extern "C" fn(Ctx)>,
    stop: Option<extern "C" fn(Ctx)>,
    load_lists: Option<extern "C" fn(Ctx)>,
    select_guild: Option<extern "C" fn(Ctx, *const c_char)>,
    select_channel: Option<extern "C" fn(Ctx, *const c_char)>,
    set_track: Option<extern "C" fn(Ctx, c_int)>,
    request_take_over: Option<extern "C" fn(Ctx)>,
    confirm_take_over: Option<extern "C" fn(Ctx, bool)>,
    set_source: Option<extern "C" fn(Ctx, *const c_char, bool)>,
    panel_closed: Option<extern "C" fn(Ctx)>,
    release_track: Option<extern "C" fn(Ctx, c_int)>,
}

type Version = unsafe extern "C" fn() -> u32;
type Load = unsafe extern "C" fn(*const Host) -> bool;
type Unload = unsafe extern "C" fn();
type OpenPanel = unsafe extern "C" fn() -> bool;

struct Ui {
    unload: Unload,
    open_panel: OpenPanel,
    library: Library,
}

static UI: Mutex<Option<Ui>> = Mutex::new(None);

const MAIN_WINDOW: &str = "obs_frontend_get_main_window";

#[cfg(unix)]
fn has_main_window() -> bool {
    let host = libloading::os::unix::Library::this();
    // SAFETY: only the symbol's presence is checked; it is never called through this pointer.
    unsafe { host.get::<*const c_void>(MAIN_WINDOW.as_bytes()) }.is_ok()
}

#[cfg(windows)]
fn has_main_window() -> bool {
    let Ok(host) = libloading::os::windows::Library::open_already_loaded("obs-frontend-api.dll")
    else {
        return false;
    };
    // SAFETY: as on unix.
    unsafe { host.get::<*const c_void>(MAIN_WINDOW.as_bytes()) }.is_ok()
}

fn library_path() -> Option<PathBuf> {
    // SAFETY: the module pointer is set before load; libobs returns a string it owns, or NULL.
    let binary = unsafe {
        let raw = sys::obs_get_module_binary_path(crate::current_module());
        if raw.is_null() {
            return None;
        }
        PathBuf::from(CStr::from_ptr(raw).to_string_lossy().into_owned())
    };
    Some(binary.parent()?.join(FOLDER).join(LIBRARY))
}

const FALLBACK: &str = "Tools > D-Streamy opens the settings dialog instead";

/// Adds the status-bar item and panel; false when only the settings dialog is left.
pub fn install() -> bool {
    if !has_main_window() {
        log::info(&format!(
            "OBS frontend lacks {MAIN_WINDOW}: the D-Streamy status-bar panel is unavailable"
        ));
        return false;
    }
    let Some(path) = library_path() else {
        log::warn(&format!(
            "libobs gave no path for this module: the status-bar panel is unavailable; {FALLBACK}"
        ));
        return false;
    };
    if !path.exists() {
        log::info(&format!(
            "{} is missing: the status-bar panel is unavailable; {FALLBACK}",
            path.display()
        ));
        return false;
    }
    // SAFETY: the library is ours and has no load-time side effects beyond Qt's static init.
    let library = match unsafe { Library::new(&path) } {
        Ok(library) => library,
        Err(e) => {
            log::warn(&format!(
                "could not load {}: {e}; the status-bar panel is unavailable; {FALLBACK}",
                path.display()
            ));
            return false;
        }
    };
    // SAFETY: the types match dstreamy-ui.h; `library` stays loaded for the life of the process.
    let symbols = unsafe {
        let version = library
            .get::<Version>(b"dstreamy_ui_abi_version")
            .map(|s| *s);
        let load = library.get::<Load>(b"dstreamy_ui_load").map(|s| *s);
        let unload = library.get::<Unload>(b"dstreamy_ui_unload").map(|s| *s);
        let open_panel = library
            .get::<OpenPanel>(b"dstreamy_ui_open_panel")
            .map(|s| *s);
        (version, load, unload, open_panel)
    };
    let (Ok(version), Ok(load), Ok(unload), Ok(open_panel)) = symbols else {
        log::warn(&format!(
            "{} predates UI ABI {ABI_VERSION} or is not the D-Streamy UI library: {FALLBACK}",
            path.display()
        ));
        std::mem::forget(library);
        return false;
    };
    // SAFETY: a plain getter of the library.
    let theirs = unsafe { version() };
    if theirs != ABI_VERSION {
        log::warn(&format!(
            "{} speaks UI ABI {theirs}, this plugin needs {ABI_VERSION}: {FALLBACK}",
            path.display()
        ));
        std::mem::forget(library);
        return false;
    }
    let host = Host {
        abi_version: ABI_VERSION,
        ctx: ptr::null_mut(),
        view_json: Some(view_json),
        free_string: Some(free_string),
        start: Some(start),
        stop: Some(stop),
        load_lists: Some(load_lists),
        select_guild: Some(select_guild),
        select_channel: Some(select_channel),
        set_track: Some(set_track),
        request_take_over: Some(request_take_over),
        confirm_take_over: Some(confirm_take_over),
        set_source: Some(set_source),
        panel_closed: Some(panel_closed),
        release_track: Some(release_track),
    };
    // SAFETY: on the UI thread, with a host the library copies.
    if unsafe { load(&host) } {
        log::info("added the D-Streamy status-bar panel");
        *UI.lock().unwrap_or_else(PoisonError::into_inner) = Some(Ui {
            unload,
            open_panel,
            library,
        });
        true
    } else {
        log::warn(&format!(
            "the D-Streamy UI library refused to load: {FALLBACK}"
        ));
        std::mem::forget(library);
        false
    }
}

/// Opens the panel like a status-bar click: whether it shows, or `None` when it isn't loaded. UI thread.
pub fn open_panel() -> Option<bool> {
    let ui = UI.lock().unwrap_or_else(PoisonError::into_inner);
    let ui = ui.as_ref()?;
    // SAFETY: on the UI thread, while the library is loaded.
    Some(unsafe { (ui.open_panel)() })
}

/// Ends every call into the host; call from `obs_module_unload` before the session shuts down.
pub fn shutdown() {
    let ui = UI.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let Some(ui) = ui {
        // SAFETY: on the UI thread; after it returns the library never calls the host again.
        unsafe { (ui.unload)() };
        // Qt may still hold objects whose code lives in the library, so it is never unmapped.
        std::mem::forget(ui.library);
    }
}

static SAMPLER: Mutex<Option<Sampler>> = Mutex::new(None);
static CLOCK: OnceLock<Instant> = OnceLock::new();
/// The takeover's confirm row is open in the panel.
static CONFIRMING: AtomicBool = AtomicBool::new(false);

/// Samples D-Streamy's output each time the panel polls; `None` while it isn't active.
fn live_stats() -> Option<LiveStats> {
    let counters = session::output_counters();
    let mut sampler = SAMPLER.lock().unwrap_or_else(PoisonError::into_inner);
    let sampler = sampler.get_or_insert_with(Sampler::default);
    let Some((bytes, frames, dropped)) = counters else {
        sampler.reset();
        return None;
    };
    let at_ms = CLOCK.get_or_init(Instant::now).elapsed().as_millis();
    Some(sampler.push(OutputSample {
        at_ms: u64::try_from(at_ms).unwrap_or(u64::MAX),
        bytes,
        frames,
        dropped,
    }))
}

extern "C" fn view_json(_ctx: Ctx) -> *mut c_char {
    guard("panel view", ptr::null_mut(), || {
        let live = live_stats();
        let sources = if cfg!(unix) {
            crate::audio::sources()
        } else {
            Vec::new()
        };
        let confirming = CONFIRMING.load(Ordering::Acquire);
        let json = with_controls(|c| {
            panel_view(c, cfg!(unix), live.as_ref(), &sources, confirming).to_json()
        });
        CString::new(json.replace('\0', " "))
            .expect("NUL bytes were replaced")
            .into_raw()
    })
}

extern "C" fn free_string(s: *mut c_char) {
    guard("panel free_string", (), || {
        if !s.is_null() {
            // SAFETY: `s` came from `view_json`'s `CString::into_raw`.
            drop(unsafe { CString::from_raw(s) });
        }
    });
}

extern "C" fn start(_ctx: Ctx) {
    guard("panel start", (), || session::start(Wait::No));
}

extern "C" fn stop(_ctx: Ctx) {
    guard("panel stop", (), || session::stop(Wait::No));
}

extern "C" fn load_lists(_ctx: Ctx) {
    guard("panel load_lists", (), || {
        CONFIRMING.store(false, Ordering::Release);
        settings_source::open_config();
        session::request_lists(None);
    });
}

/// An id from the panel; "" is its "Pick a …" entry.
fn picked(id: *const c_char) -> Option<String> {
    // SAFETY: the library passes a NUL-terminated string or NULL, valid for the call.
    let id = unsafe { text(id) };
    (!id.is_empty()).then_some(id)
}

extern "C" fn select_guild(_ctx: Ctx, id: *const c_char) {
    guard("panel select_guild", (), || {
        let id = picked(id);
        let load = change(|c, p| c.edit(p.write(), |m| m.choose_guild(id.as_deref())));
        if let Some(guild) = load {
            session::request_lists(Some(guild));
        }
    });
}

extern "C" fn select_channel(_ctx: Ctx, id: *const c_char) {
    guard("panel select_channel", (), || {
        let id = picked(id);
        change(|c, p| c.edit(p.write(), |m| m.choose_channel(id.as_deref())));
    });
}

extern "C" fn set_track(_ctx: Ctx, track: c_int) {
    guard("panel set_track", (), || {
        let Ok(track) = u8::try_from(track) else {
            return;
        };
        if change(|c, p| c.select_audio_track(track, p.write())) {
            CONFIRMING.store(false, Ordering::Release);
        }
    });
}

/// The track the panel's audio part shows: the one Discord hears now while live, else the next start's.
fn shown_track() -> u8 {
    with_controls(|c| c.model.heard_track())
}

extern "C" fn request_take_over(_ctx: Ctx) {
    guard("panel request_take_over", (), || {
        let track = shown_track();
        if cfg!(unix) && !with_controls(|c| c.model.manages(track)) {
            CONFIRMING.store(true, Ordering::Release);
        }
    });
}

extern "C" fn confirm_take_over(_ctx: Ctx, confirm: bool) {
    guard("panel confirm_take_over", (), || {
        if CONFIRMING.swap(false, Ordering::AcqRel) && confirm {
            crate::audio::take_over(shown_track());
        }
    });
}

extern "C" fn set_source(_ctx: Ctx, uuid: *const c_char, on: bool) {
    guard("panel set_source", (), || {
        if let Some(uuid) = picked(uuid) {
            crate::audio::set_source(&uuid, shown_track(), on);
        }
    });
}

extern "C" fn release_track(_ctx: Ctx, track: c_int) {
    guard("panel release_track", (), || {
        if let Ok(track) = u8::try_from(track) {
            crate::audio::release(track);
        }
    });
}

/// Another scene collection: a takeover confirm opened for the last one no longer applies.
pub fn collection_changed() {
    CONFIRMING.store(false, Ordering::Release);
}

extern "C" fn panel_closed(_ctx: Ctx) {
    guard("panel closed", (), || {
        CONFIRMING.store(false, Ordering::Release);
    });
}
