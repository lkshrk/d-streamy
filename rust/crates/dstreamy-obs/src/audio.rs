//! The Discord track on OBS's audio sources: listing them, taking the track over, and ticking sources.

use std::cell::Cell;
use std::ffi::{CStr, CString, c_void};
use std::ptr;
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use dstreamy_obs_producer::mixer::{AudioSource, with_track};
use dstreamy_obs_sys as sys;

use crate::guard::guard;
use crate::log;
use crate::output::text;
use crate::settings_source::{self, SOURCE_ID, change, with_controls};

/// OBS's global audio channels: Desktop Audio 1 and 2, Mic/Aux 1 to 3, and one more.
const AUDIO_CHANNELS: std::ops::RangeInclusive<u32> = 1..=6;

/// Lets a mixer change from another thread land before the dialog reads it, and merges bursts.
const REFRESH_DELAY: Duration = Duration::from_millis(50);

thread_local! {
    /// Set while this module changes mixer flags itself, so its own change doesn't rebuild the dialog.
    static APPLYING: Cell<bool> = const { Cell::new(false) };
}

/// # Safety
/// `source` is live for the call.
unsafe fn describe(source: *mut sys::obs_source_t) -> Option<AudioSource> {
    // SAFETY: the caller's contract; libobs returns strings it owns, or NULL.
    unsafe {
        if sys::obs_source_get_output_flags(source) & sys::OBS_SOURCE_AUDIO == 0 {
            return None;
        }
        let id = sys::obs_source_get_id(source);
        if !id.is_null() && CStr::from_ptr(id) == SOURCE_ID {
            return None;
        }
        let uuid = text(sys::obs_source_get_uuid(source));
        (!uuid.is_empty()).then(|| AudioSource {
            uuid,
            name: text(sys::obs_source_get_name(source)),
            mixers: sys::obs_source_get_audio_mixers(source),
        })
    }
}

extern "C" fn collect(param: *mut c_void, source: *mut sys::obs_source_t) -> bool {
    guard("audio sources", true, || {
        // SAFETY: `param` is the Vec `sources` passed; libobs holds the source for the callback.
        let found = unsafe { &mut *param.cast::<Vec<AudioSource>>() };
        if let Some(s) = unsafe { describe(source) } {
            found.push(s);
        }
        true
    })
}

/// Every source with audio, in any scene or a global audio channel, but D-Streamy's own; each once.
pub fn sources() -> Vec<AudioSource> {
    let mut found: Vec<AudioSource> = Vec::new();
    // SAFETY: `collect` only reads the source while libobs holds it, and `found` outlives the call.
    unsafe { sys::obs_enum_sources(Some(collect), (&raw mut found).cast()) };
    for channel in AUDIO_CHANNELS {
        // SAFETY: a strong reference or NULL, released below.
        let source = unsafe { sys::obs_get_output_source(channel) };
        if source.is_null() {
            continue;
        }
        if let Some(s) = unsafe { describe(source) }
            && !found.iter().any(|f| f.uuid == s.uuid)
        {
            found.push(s);
        }
        // SAFETY: the reference taken above.
        unsafe { sys::obs_source_release(source) };
    }
    found
}

/// Runs `f` on the source with `uuid` while holding a reference; false when no such source exists.
fn with_source(uuid: &str, f: impl FnOnce(*mut sys::obs_source_t)) -> bool {
    let Ok(c_uuid) = CString::new(uuid) else {
        return false;
    };
    // SAFETY: a strong reference or NULL.
    let mut source = unsafe { sys::obs_get_source_by_uuid(c_uuid.as_ptr()) };
    if source.is_null() {
        source = AUDIO_CHANNELS
            // SAFETY: strong references or NULL; each one not kept is released at once.
            .map(|channel| unsafe { sys::obs_get_output_source(channel) })
            .find(|&s| {
                let ours = !s.is_null() && unsafe { text(sys::obs_source_get_uuid(s)) } == uuid;
                if !s.is_null() && !ours {
                    unsafe { sys::obs_source_release(s) };
                }
                ours
            })
            .unwrap_or(ptr::null_mut());
    }
    if source.is_null() {
        return false;
    }
    f(source);
    // SAFETY: the reference taken above.
    unsafe { sys::obs_source_release(source) };
    true
}

fn set_track(source: *mut sys::obs_source_t, track: u8, on: bool) {
    // SAFETY: the caller holds a reference; setting the flags is what Advanced Audio Properties does.
    unsafe {
        let mixers = sys::obs_source_get_audio_mixers(source);
        let next = with_track(mixers, track, on);
        if next != mixers {
            APPLYING.with(|a| a.set(true));
            sys::obs_source_set_audio_mixers(source, next);
            APPLYING.with(|a| a.set(false));
        }
    }
}

/// Turns the selected track off on every audio source, then marks it managed. On the UI thread.
pub fn take_over() -> u8 {
    let track = with_controls(|c| c.model.settings().audio_track);
    let audio = sources();
    for source in &audio {
        with_source(&source.uuid, |s| set_track(s, track, false));
    }
    change(|c, p| {
        c.edit(p.write(), |m| {
            if m.settings().audio_track == track {
                m.take_over();
            }
        })
    });
    log::info(&format!(
        "D-Streamy manages Track {track}: turned it off on {} audio sources",
        audio.len()
    ));
    track
}

/// Turns the managed track on or off for one source. On the UI thread.
pub fn set_source(uuid: &str, on: bool) {
    let (track, managed) = with_controls(|c| (c.model.settings().audio_track, c.model.managed()));
    if !managed {
        log::warn(&format!(
            "ignored a tick for Track {track}: D-Streamy doesn't manage it yet"
        ));
        return;
    }
    if !with_source(uuid, |s| set_track(s, track, on)) {
        log::warn(&format!("the audio source {uuid} is gone"));
    }
}

struct Refresher {
    requests: Sender<bool>,
    thread: JoinHandle<()>,
}

static REFRESHER: Mutex<Option<Refresher>> = Mutex::new(None);

/// Rebuilds the open dialog soon, from another thread, so OBS queues the rebuild on its UI thread.
pub fn request_refresh() {
    let refresher = REFRESHER.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(r) = refresher.as_ref() {
        let _ = r.requests.send(true);
    }
}

fn refresh_later() {
    if !APPLYING.with(Cell::get) && settings_source::dialog_open() {
        request_refresh();
    }
}

fn run_refresher(requests: mpsc::Receiver<bool>) {
    while let Ok(true) = requests.recv() {
        std::thread::sleep(REFRESH_DELAY);
        let mut stop = false;
        while let Ok(more) = requests.try_recv() {
            stop |= !more;
        }
        guard("audio refresh", (), settings_source::refresh);
        if stop {
            return;
        }
    }
}

/// A source whose "audio_mixers" signal this module connected to.
struct Watched(*mut sys::obs_weak_source_t);

// SAFETY: weak source references are reference-counted by libobs and usable from any thread.
unsafe impl Send for Watched {}

static WATCHED: Mutex<Vec<Watched>> = Mutex::new(Vec::new());

/// The calldata's "source" if it is an audio source other than D-Streamy's own.
fn audio_source(cd: *mut sys::calldata_t) -> Option<*mut sys::obs_source_t> {
    let mut source: *mut sys::obs_source_t = ptr::null_mut();
    // SAFETY: libobs stores "source" as a pointer (calldata_set_ptr) for these signals.
    let found = unsafe {
        sys::calldata_get_data(
            cd,
            c"source".as_ptr(),
            (&raw mut source).cast(),
            size_of::<*mut sys::obs_source_t>(),
        )
    };
    // SAFETY: the signal's source is live while its handlers run.
    (found && !source.is_null() && unsafe { describe(source) }.is_some()).then_some(source)
}

extern "C" fn on_created(_data: *mut c_void, cd: *mut sys::calldata_t) {
    guard("audio source_create", (), || {
        if let Some(source) = audio_source(cd) {
            // SAFETY: the source is live; the connection is undone on its destroy or in `shutdown`.
            unsafe {
                sys::signal_handler_connect(
                    sys::obs_source_get_signal_handler(source),
                    c"audio_mixers".as_ptr(),
                    Some(on_changed),
                    ptr::null_mut(),
                );
                let weak = Watched(sys::obs_source_get_weak_source(source));
                WATCHED
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(weak);
            }
            // libobs creates every source on all tracks; a scene-collection load restores saved mixers after this.
            let (managed, track) =
                with_controls(|c| (c.model.managed(), c.model.settings().audio_track));
            if managed {
                set_track(source, track, false);
            }
            refresh_later();
        }
    });
}

extern "C" fn on_destroyed(_data: *mut c_void, cd: *mut sys::calldata_t) {
    guard("audio source_destroy", (), || {
        if let Some(source) = audio_source(cd) {
            WATCHED
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .retain(|w| {
                    // SAFETY: both are live; the weak reference is this module's own.
                    let ours = unsafe { sys::obs_weak_source_references_source(w.0, source) };
                    if ours {
                        // SAFETY: as above.
                        unsafe { sys::obs_weak_source_release(w.0) };
                    }
                    !ours
                });
            refresh_later();
        }
    });
}

extern "C" fn on_changed(_data: *mut c_void, cd: *mut sys::calldata_t) {
    guard("audio change", (), || {
        if audio_source(cd).is_some() {
            refresh_later();
        }
    });
}

type Handler = extern "C" fn(*mut c_void, *mut sys::calldata_t);

const GLOBAL_SIGNALS: [(&CStr, Handler); 3] = [
    (c"source_create", on_created),
    (c"source_destroy", on_destroyed),
    (c"source_rename", on_changed),
];

/// Keeps an open dialog's checklist in step with OBS; call from `obs_module_load`.
pub fn install() {
    let (requests, receiver) = mpsc::channel();
    match std::thread::Builder::new()
        .name("dstreamy refresh".into())
        .spawn(move || run_refresher(receiver))
    {
        Ok(thread) => {
            *REFRESHER.lock().unwrap_or_else(PoisonError::into_inner) =
                Some(Refresher { requests, thread });
        }
        Err(e) => log::warn(&format!(
            "cannot start the D-Streamy refresh thread ({e}): the dialog won't follow audio changes"
        )),
    }
    // SAFETY: libobs's global handler outlives the module; `shutdown` disconnects.
    unsafe {
        let handler = sys::obs_get_signal_handler();
        for (signal, callback) in GLOBAL_SIGNALS {
            sys::signal_handler_connect(handler, signal.as_ptr(), Some(callback), ptr::null_mut());
        }
    }
}

/// Disconnects every handler and ends the refresh thread; libobs unloads modules before it frees sources.
pub fn shutdown() {
    // SAFETY: as in `install`.
    unsafe {
        let handler = sys::obs_get_signal_handler();
        for (signal, callback) in GLOBAL_SIGNALS {
            sys::signal_handler_disconnect(
                handler,
                signal.as_ptr(),
                Some(callback),
                ptr::null_mut(),
            );
        }
    }
    let watched = std::mem::take(&mut *WATCHED.lock().unwrap_or_else(PoisonError::into_inner));
    for weak in watched {
        // SAFETY: a strong reference or NULL from this module's weak one; both are released here.
        unsafe {
            let source = sys::obs_weak_source_get_source(weak.0);
            if !source.is_null() {
                sys::signal_handler_disconnect(
                    sys::obs_source_get_signal_handler(source),
                    c"audio_mixers".as_ptr(),
                    Some(on_changed),
                    ptr::null_mut(),
                );
                sys::obs_source_release(source);
            }
            sys::obs_weak_source_release(weak.0);
        }
    }
    let refresher = REFRESHER
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    if let Some(r) = refresher {
        let _ = r.requests.send(false);
        if r.thread.join().is_err() {
            log::warn("the D-Streamy refresh thread panicked");
        }
    }
}
