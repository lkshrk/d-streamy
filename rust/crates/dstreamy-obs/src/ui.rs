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
use crate::session;
use crate::settings_source::{self, change, with_controls};

const ABI_VERSION: u32 = sys::DSTREAMY_UI_ABI_VERSION;

// libobs scans the module's own folder for modules, never this subfolder; macOS's bundle folder isn't scanned.
#[cfg(not(target_os = "macos"))]
const FOLDER: &str = "dstreamy-ui";
#[cfg(target_os = "macos")]
const FOLDER: &str = "";
#[cfg(target_os = "macos")]
const LIBRARY: &str = "obs-dstreamy-ui.dylib";
#[cfg(windows)]
const LIBRARY: &str = "obs-dstreamy-ui.dll";
#[cfg(not(any(target_os = "macos", windows)))]
const LIBRARY: &str = "obs-dstreamy-ui.so";

type Ctx = *mut c_void;

/// Generated from `obs/ui/dstreamy-ui.h`, so the two sides can't drift.
type Host = sys::dstreamy_ui_host;

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

fn host() -> Host {
    Host {
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
        login: Some(login),
        cancel_login: Some(cancel_login),
        select_account: Some(select_account),
        add_account: Some(add_account),
        logout_account: Some(logout_account),
        confirm_logout: Some(confirm_logout),
    }
}

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
    let host = host();
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
    let shown = {
        let ui = UI.lock().unwrap_or_else(PoisonError::into_inner);
        let ui = ui.as_ref()?;
        // SAFETY: on the UI thread, while the library is loaded.
        unsafe { (ui.open_panel)() }
    };
    opened(shown);
    Some(shown)
}

/// A panel that didn't show, or threw while opening, is closed for the host's state too.
fn opened(shown: bool) {
    if !shown && panel_open() {
        panel_closed(ptr::null_mut());
    }
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
static PANEL_OPEN: AtomicBool = AtomicBool::new(false);

pub fn panel_open() -> bool {
    PANEL_OPEN.load(Ordering::Acquire)
}

/// Samples D-Streamy's output each time the panel polls; `None` while it isn't active.
fn live_stats() -> Option<LiveStats> {
    let counters = session::output_counters();
    let mut sampler = SAMPLER.lock().unwrap_or_else(PoisonError::into_inner);
    let sampler = sampler.get_or_insert_with(Sampler::default);
    let Some((bytes, frames, dropped, dropped_audio)) = counters else {
        sampler.reset();
        return None;
    };
    let at_ms = CLOCK.get_or_init(Instant::now).elapsed().as_millis();
    Some(sampler.observe(
        OutputSample {
            at_ms: u64::try_from(at_ms).unwrap_or(u64::MAX),
            bytes,
            frames,
            dropped,
            dropped_audio,
        },
        session::viewers(),
        session::health(),
    ))
}

extern "C" fn view_json(_ctx: Ctx) -> *mut c_char {
    guard("panel view", ptr::null_mut(), || {
        let live = live_stats();
        settings_source::recheck_unrestored();
        let sources = crate::audio::sources();
        let confirming = CONFIRMING.load(Ordering::Acquire);
        let json =
            with_controls(|c| panel_view(c, true, live.as_ref(), &sources, confirming).to_json());
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
    guard("panel start", (), session::start);
}

extern "C" fn stop(_ctx: Ctx) {
    guard("panel stop", (), session::stop);
}

// Not a dialog callback, so the open dialog can rebuild at once.
extern "C" fn login(_ctx: Ctx) {
    guard("panel login", (), || {
        session::login();
        settings_source::refresh();
    });
}

extern "C" fn cancel_login(_ctx: Ctx) {
    guard("panel cancel_login", (), || {
        session::cancel_login();
        settings_source::refresh();
    });
}

extern "C" fn select_account(_ctx: Ctx, name: *const c_char) {
    guard("panel select_account", (), || {
        if let Some(name) = picked(name) {
            session::select_account(&name);
            settings_source::refresh();
        }
    });
}

extern "C" fn add_account(_ctx: Ctx) {
    guard("panel add_account", (), || {
        session::add_account();
        settings_source::refresh();
    });
}

extern "C" fn logout_account(_ctx: Ctx, name: *const c_char) {
    guard("panel logout_account", (), || {
        if let Some(name) = picked(name)
            && change(|c, _| c.model.request_logout(&name))
        {
            settings_source::refresh();
        }
    });
}

extern "C" fn confirm_logout(_ctx: Ctx, yes: bool) {
    guard("panel confirm_logout", (), || {
        if let Some(name) = change(|c, _| c.model.confirm_logout(yes)) {
            session::logout_account(&name);
        }
        settings_source::refresh();
    });
}

extern "C" fn load_lists(_ctx: Ctx) {
    guard("panel load_lists", (), || {
        PANEL_OPEN.store(true, Ordering::Release);
        CONFIRMING.store(false, Ordering::Release);
        settings_source::open_config();
        session::load_lists(None);
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
            session::load_lists(Some(guild));
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
        if !with_controls(|c| c.model.manages(track)) {
            CONFIRMING.store(true, Ordering::Release);
        }
    });
}

extern "C" fn confirm_take_over(_ctx: Ctx, confirm: bool) {
    guard("panel confirm_take_over", (), || {
        if CONFIRMING.swap(false, Ordering::AcqRel)
            && confirm
            && crate::audio::take_over(shown_track())
        {
            settings_source::mixers_changed();
        }
    });
}

extern "C" fn set_source(_ctx: Ctx, uuid: *const c_char, on: bool) {
    guard("panel set_source", (), || {
        if let Some(uuid) = picked(uuid)
            && crate::audio::set_source(&uuid, shown_track(), on)
        {
            settings_source::mixers_changed();
        }
    });
}

extern "C" fn release_track(_ctx: Ctx, track: c_int) {
    guard("panel release_track", (), || {
        if let Ok(track) = u8::try_from(track)
            && crate::audio::release(track)
        {
            settings_source::mixers_changed();
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
        PANEL_OPEN.store(false, Ordering::Release);
        if !settings_source::dialog_open() {
            change(|c, _| {
                c.model.forget_released();
                c.model.confirm_logout(false);
            });
        }
        settings_source::forget_unrestored();
    });
}

#[cfg(test)]
pub(crate) mod tests {

    use dstreamy_obs_producer::control::Named;
    use dstreamy_obs_producer::dock_config::ConfigState;
    use serde_json::Value;

    use super::*;

    pub(crate) const PLAIN: &CStr = c"dstreamy_test_audio";
    pub(crate) const MIC: &CStr = c"dstreamy_test_input_capture";

    extern "C" fn test_name(_type_data: *mut c_void) -> *const c_char {
        c"test audio".as_ptr()
    }

    extern "C" fn test_create(
        _settings: *mut sys::obs_data_t,
        source: *mut sys::obs_source_t,
    ) -> *mut c_void {
        source.cast()
    }

    extern "C" fn test_destroy(_data: *mut c_void) {}

    /// libobs without video or modules, plus two audio source types: a plain one and a microphone.
    pub(crate) fn libobs() {
        static STARTED: OnceLock<bool> = OnceLock::new();
        let started = *STARTED.get_or_init(|| {
            // SAFETY: started once for the test process; the registered strings are 'static.
            unsafe {
                if !sys::obs_startup(c"en-US".as_ptr(), ptr::null(), ptr::null_mut()) {
                    return false;
                }
                for id in [PLAIN, MIC] {
                    let info = sys::obs_source_info {
                        id: id.as_ptr(),
                        type_: sys::OBS_SOURCE_TYPE_INPUT,
                        output_flags: sys::OBS_SOURCE_AUDIO,
                        get_name: Some(test_name),
                        create: Some(test_create),
                        destroy: Some(test_destroy),
                        ..Default::default()
                    };
                    sys::obs_register_source_s(&info, size_of::<sys::obs_source_info>());
                }
                true
            }
        });
        assert!(
            started,
            "obs_startup failed; on Linux libobs needs an X display, so run the tests under xvfb-run"
        );
    }

    #[cfg(unix)]
    pub(crate) struct Source(pub(crate) *mut sys::obs_source_t);

    #[cfg(unix)]
    impl Source {
        pub(crate) fn new(id: &CStr, name: &CStr) -> Self {
            // SAFETY: libobs is started and `id` is registered; the reference is released on drop.
            let source = unsafe {
                sys::obs_source_create(id.as_ptr(), name.as_ptr(), ptr::null_mut(), ptr::null_mut())
            };
            assert!(!source.is_null());
            Self(source)
        }

        pub(crate) fn uuid(&self) -> CString {
            // SAFETY: a live source.
            CString::new(unsafe { text(sys::obs_source_get_uuid(self.0)) }).unwrap()
        }

        pub(crate) fn on(&self, track: u8) -> bool {
            // SAFETY: a live source.
            let mixers = unsafe { sys::obs_source_get_audio_mixers(self.0) };
            mixers & (1 << (track - 1)) != 0
        }
    }

    #[cfg(unix)]
    impl Drop for Source {
        fn drop(&mut self) {
            // SAFETY: the reference taken in `new`.
            unsafe {
                sys::obs_source_remove(self.0);
                sys::obs_source_release(self.0);
            }
        }
    }

    fn call<R>(f: Option<unsafe extern "C" fn(Ctx) -> R>) -> R {
        // SAFETY: the plugin's own callbacks, which ignore `ctx`.
        unsafe { f.expect("callback set")(ptr::null_mut()) }
    }

    fn view() -> Value {
        let host = host();
        let raw = call(host.view_json);
        assert!(!raw.is_null());
        // SAFETY: a NUL-terminated string from `view_json`, freed below with `free_string`.
        let json = unsafe { text(raw) };
        // SAFETY: as above.
        unsafe { host.free_string.unwrap()(raw) };
        serde_json::from_str(&json).expect("the view is JSON")
    }

    fn confirming() -> bool {
        CONFIRMING.load(Ordering::Acquire)
    }

    pub(crate) fn reset() {
        CONFIRMING.store(false, Ordering::Release);
        change(|c, _| *c = ConfigState::default());
    }

    #[test]
    fn the_host_table_is_the_header_s_and_complete() {
        let host = host();
        assert_eq!(host.abi_version, 6);
        assert!(host.ctx.is_null());
        let set = [
            host.view_json.is_some(),
            host.free_string.is_some(),
            host.start.is_some(),
            host.stop.is_some(),
            host.load_lists.is_some(),
            host.select_guild.is_some(),
            host.select_channel.is_some(),
            host.set_track.is_some(),
            host.request_take_over.is_some(),
            host.confirm_take_over.is_some(),
            host.set_source.is_some(),
            host.panel_closed.is_some(),
            host.release_track.is_some(),
            host.login.is_some(),
            host.cancel_login.is_some(),
            host.select_account.is_some(),
            host.add_account.is_some(),
            host.logout_account.is_some(),
            host.confirm_logout.is_some(),
        ];
        assert!(set.iter().all(|s| *s));
    }

    #[cfg(unix)]
    #[test]
    fn the_audio_callbacks_take_over_tick_and_release_tracks() {
        use dstreamy_obs_producer::dock::DockState;

        let _serial = crate::serial();
        libobs();
        reset();
        let host = host();
        let desktop = Source::new(PLAIN, c"Desktop");
        let mic = Source::new(MIC, c"Mic");
        // SAFETY: the plugin's own callback.
        unsafe { host.set_track.unwrap()(ptr::null_mut(), 1) };
        assert_eq!(view()["audio"]["managed"], false);

        call(host.start);
        assert_eq!(
            with_controls(|c| (c.model.state().clone(), c.model.error().map(str::to_owned))),
            (DockState::Idle, None),
            "a mic on unmanaged Track 1 holds Go Live back before anything else"
        );

        call(host.request_take_over);
        assert!(confirming());
        assert!(view()["audio"]["confirm"].is_object());
        call(host.panel_closed);
        assert!(!confirming());
        call(host.request_take_over);
        // SAFETY: the plugin's own callback.
        unsafe { host.confirm_take_over.unwrap()(ptr::null_mut(), false) };
        assert!(!confirming());
        assert!(!with_controls(|c| c.model.manages(1)));
        assert!(desktop.on(1) && mic.on(1), "Cancel changes no mixer");

        call(host.request_take_over);
        // SAFETY: as above.
        unsafe { host.confirm_take_over.unwrap()(ptr::null_mut(), true) };
        assert!(with_controls(|c| c.model.manages(1)));
        assert!(!desktop.on(1) && !mic.on(1));
        assert!(desktop.on(2), "only the taken track changes");
        call(host.request_take_over);
        assert!(!confirming(), "a managed track needs no confirm");
        let shown = |name: &str| {
            view()["audio"]["sources"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["name"] == name)
                .map(|s| s["on"].as_bool().unwrap())
        };
        assert_eq!((shown("Desktop"), shown("Mic")), (Some(false), Some(false)));

        let set_source = host.set_source.unwrap();
        // SAFETY: as above, with strings valid for each call.
        unsafe {
            set_source(ptr::null_mut(), desktop.uuid().as_ptr(), true);
            assert!(desktop.on(1) && !mic.on(1));
            assert_eq!(shown("Desktop"), Some(true));
            set_source(ptr::null_mut(), desktop.uuid().as_ptr(), false);
            assert!(!desktop.on(1));
            set_source(ptr::null_mut(), c"".as_ptr(), true);
            set_source(ptr::null_mut(), c"no-such-source".as_ptr(), true);
            set_source(ptr::null_mut(), ptr::null(), true);
        }
        assert!(!desktop.on(1) && !mic.on(1));

        call(host.start);
        let (state, error) =
            with_controls(|c| (c.model.state().clone(), c.model.error().map(str::to_owned)));
        assert_eq!(state, DockState::Idle);
        assert!(
            error.is_some_and(|e| e.contains("no video")),
            "with the mic off Discord's track, start reaches the canvas check"
        );

        let set_track = host.set_track.unwrap();
        let release = host.release_track.unwrap();
        // SAFETY: as above.
        unsafe {
            set_track(ptr::null_mut(), 2);
            assert_eq!(with_controls(|c| c.model.settings().audio_track), 2);
            call(host.request_take_over);
            assert!(confirming(), "Track 2 isn't managed");
            set_track(ptr::null_mut(), -1);
            set_track(ptr::null_mut(), 300);
            set_track(ptr::null_mut(), 0);
            assert_eq!(with_controls(|c| c.model.settings().audio_track), 2);
            assert!(confirming(), "an out-of-range track changes nothing");
            set_track(ptr::null_mut(), 3);
            assert!(!confirming(), "another track ends the confirm");

            release(ptr::null_mut(), 1);
            assert!(!with_controls(|c| c.model.manages(1)));
            assert!(!desktop.on(1) && !mic.on(1), "releasing changes no mixer");

            call(host.request_take_over);
            host.confirm_take_over.unwrap()(ptr::null_mut(), true);
            assert!(with_controls(|c| c.model.manages(3)));
            release(ptr::null_mut(), 3);
            assert!(
                with_controls(|c| c.model.manages(3)),
                "the track Discord gets stays managed"
            );
            release(ptr::null_mut(), -1);
        }
        drop((desktop, mic));
        reset();
    }

    #[cfg(unix)]
    #[test]
    fn the_login_callback_opens_the_window_only_where_the_view_offers_it() {
        use dstreamy_obs_producer::control::ControlError;

        let _serial = crate::serial();
        libobs();
        reset();
        let host = host();
        assert_eq!(view()["login"], Value::Null);
        call(host.login);
        assert_eq!(with_controls(|c| c.model.error().map(str::to_owned)), None);

        change(|c, _| {
            c.model
                .guilds_loaded(Err(ControlError::InvalidToken { window: true }));
        });
        assert_eq!(view()["login"]["label"], "Log in again");
        assert_eq!(view()["state_fix"], "", "the button replaces the command");
        call(host.login);
        let (headline, fix) = with_controls(|c| {
            let p = c.model.problem().unwrap();
            (p.headline, p.fix)
        });
        assert_eq!(
            (headline, fix),
            ("Login failed", "Restart OBS"),
            "without a control thread the login can't start"
        );
        assert!(
            with_controls(|c| c.model.error().unwrap().contains("the login can't start")),
            "its own words, not Start and Stop"
        );
        assert!(!with_controls(|c| c.model.logging_in()));
        assert_eq!(view()["login"], Value::Null);

        change(|c, _| {
            c.model
                .guilds_loaded(Err(ControlError::NotLoggedIn { window: true }));
            assert!(c.model.begin_login());
        });
        assert_eq!(view()["login"]["action"], "cancel");
        assert_eq!(view()["login"]["label"], "Cancel login");
        call(host.cancel_login);
        assert!(!with_controls(|c| c.model.logging_in()));
        assert_eq!(view()["state_line"], "Login cancelled");
        assert_eq!(view()["login"]["action"], "login");
        reset();
    }

    #[cfg(unix)]
    #[test]
    fn the_account_callbacks_change_only_a_listed_idle_account() {
        use dstreamy_obs_producer::control::{Account, Accounts};

        let _serial = crate::serial();
        libobs();
        reset();
        let host = host();
        let raw_select = host.select_account.unwrap();
        let raw_logout = host.logout_account.unwrap();
        // SAFETY: the library's calls: NULL or a NUL-terminated string valid for the call.
        let select = |name: *const c_char| unsafe { raw_select(ptr::null_mut(), name) };
        // SAFETY: as above.
        let logout = |name: *const c_char| unsafe { raw_logout(ptr::null_mut(), name) };
        assert_eq!(view()["account"], Value::Null, "no account support listed");
        select(c"b".as_ptr());
        call(host.add_account);
        assert!(!with_controls(
            |c| c.model.account_changing() || c.model.logging_in()
        ));

        let account = |name: &str| Account {
            name: name.into(),
            username: None,
            global_name: None,
            has_token: true,
            active: name == "a",
        };
        change(|c, _| {
            c.model.accounts_loaded(Ok(Some(Accounts {
                active: Some("a".into()),
                accounts: vec![account("a"), account("b")],
            })));
        });
        assert_eq!(view()["account"]["items"][0]["selected"], true);
        assert_eq!(view()["account"]["enabled"], true);
        select(ptr::null());
        assert!(!with_controls(|c| c.model.account_changing()));
        select(c"b".as_ptr());
        let (headline, message) = with_controls(|c| {
            let p = c.model.problem().unwrap();
            (p.headline, p.message.clone())
        });
        assert_eq!(headline, "Can't reach the service");
        assert!(message.contains("the account can't change"), "{message}");
        assert!(
            !with_controls(|c| c.model.account_changing()),
            "never stuck"
        );
        logout(c"zz".as_ptr());
        assert_eq!(
            view()["account"]["confirm"],
            Value::Null,
            "not a saved account"
        );
        logout(c"a".as_ptr());
        assert_eq!(view()["account"]["confirm"]["text"], "Log out a?");
        assert!(
            !with_controls(|c| c.model.account_changing()),
            "it asks first"
        );
        // SAFETY: the library's call.
        unsafe { host.confirm_logout.unwrap()(ptr::null_mut(), false) };
        assert_eq!(view()["account"]["confirm"], Value::Null);
        logout(c"a".as_ptr());
        // SAFETY: the library's call.
        unsafe { host.confirm_logout.unwrap()(ptr::null_mut(), true) };
        assert_eq!(view()["account"]["confirm"], Value::Null);
        assert!(
            with_controls(|c| c
                .model
                .problem()
                .unwrap()
                .message
                .contains("the account can't change")),
            "no control thread, so the logout ends at once"
        );
        assert!(!with_controls(|c| c.model.account_changing()));
        call(host.add_account);
        assert_eq!(
            with_controls(|c| c.model.problem().map(|p| p.headline)),
            Some("Login failed"),
            "without a control thread the login can't start"
        );
        reset();
    }

    #[test]
    fn a_panel_that_fails_to_open_is_closed() {
        let _serial = crate::serial();
        libobs();
        reset();
        call(host().load_lists);
        assert!(panel_open());
        CONFIRMING.store(true, Ordering::Release);
        opened(true);
        assert!(panel_open());
        opened(false);
        assert!(!panel_open() && !confirming());
        change(|c, _| c.model.restore_failed(1 << 5));
        opened(false);
        assert_ne!(with_controls(|c| c.model.unrestored_mixers()), 0);
        reset();
    }

    #[test]
    fn the_list_callbacks_pick_servers_and_channels() {
        let _serial = crate::serial();
        libobs();
        reset();
        let host = host();
        let named = |id: &str, name: &str| Named {
            id: id.into(),
            name: name.into(),
        };
        change(|c, _| {
            c.model.guilds_loaded(Ok(vec![named("g1", "Guild")]));
        });
        let select_guild = host.select_guild.unwrap();
        let select_channel = host.select_channel.unwrap();
        // SAFETY: the plugin's own callbacks, with strings valid for each call.
        unsafe {
            select_guild(ptr::null_mut(), c"g1".as_ptr());
            assert_eq!(
                with_controls(|c| c.model.settings().guild_id.clone()).as_deref(),
                Some("g1")
            );
            assert!(
                !with_controls(|c| c.model.loading().0),
                "without a control thread the channel load ends at once"
            );
            change(|c, _| {
                c.model
                    .channels_loaded("g1", Ok(vec![named("c1", "Voice")]))
            });
            select_channel(ptr::null_mut(), c"c1".as_ptr());
            assert_eq!(
                with_controls(|c| c.model.settings().channel_id.clone()).as_deref(),
                Some("c1")
            );
            select_channel(ptr::null_mut(), c"unlisted".as_ptr());
            assert_eq!(
                with_controls(|c| c.model.settings().channel_id.clone()).as_deref(),
                Some("c1"),
                "an unlisted channel is ignored"
            );
            assert_eq!(view()["channels"]["items"][0]["selected"], true);
            select_channel(ptr::null_mut(), c"".as_ptr());
            assert_eq!(
                with_controls(|c| c.model.settings().channel_id.clone()),
                None
            );
            select_guild(ptr::null_mut(), ptr::null());
            assert_eq!(with_controls(|c| c.model.settings().guild_id.clone()), None);
        }

        CONFIRMING.store(true, Ordering::Release);
        call(host.load_lists);
        assert!(!confirming(), "reloading the lists closes a confirm");
        assert!(!with_controls(|c| c.model.loading().0));
        reset();
    }

    #[cfg(unix)]
    #[test]
    fn stop_without_a_control_thread_ends_the_session_with_an_error() {
        use dstreamy_obs_producer::control::Started;
        use dstreamy_obs_producer::dock::{Canvas, DockState};

        let _serial = crate::serial();
        libobs();
        reset();
        let host = host();
        change(|c, _| {
            c.model.select_guild("g1");
            c.model.select_channel("c1");
            c.model
                .start(Canvas {
                    fps_num: 30,
                    fps_den: 1,
                    output_width: 1280,
                    output_height: 720,
                })
                .expect("a start request");
            c.model.started(Ok(Started {
                ownership: None,
                channel: "c1".into(),
            }));
        });
        assert!(matches!(
            with_controls(|c| c.model.state().clone()),
            DockState::Live { .. }
        ));
        call(host.start);
        assert!(
            matches!(
                with_controls(|c| c.model.state().clone()),
                DockState::Live { .. }
            ),
            "start while live changes nothing"
        );
        call(host.stop);
        let (state, error) =
            with_controls(|c| (c.model.state().clone(), c.model.error().map(str::to_owned)));
        assert_eq!(state, DockState::Idle);
        assert!(error.is_some_and(|e| e.contains("control thread")));
        call(host.stop);
        assert_eq!(with_controls(|c| c.model.state().clone()), DockState::Idle);
        reset();
    }
}
