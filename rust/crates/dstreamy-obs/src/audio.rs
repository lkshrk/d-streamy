//! The Discord track on OBS's audio sources: listing them, taking the track over, and ticking sources.

use std::cell::Cell;
use std::ffi::{CStr, CString, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use dstreamy_obs_producer::mixer::{
    AudioSource, MIC_AUX_CHANNELS, is_input_device, restored_managed, restored_mixers, track_bit,
    with_track,
};
#[cfg(unix)]
use dstreamy_obs_producer::mixer::{MicGuard, mic_guard as guard_for};
use dstreamy_obs_sys as sys;

use crate::guard::guard;
use crate::log;
use crate::output::text;
use crate::settings_source::{self, SOURCE_ID, change, with_controls};

/// Where a scene collection's "modules" data keeps D-Streamy's state.
const COLLECTION_KEY: &CStr = c"dstreamy";
const MANAGED_MIXERS: &CStr = c"managed_mixers";

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
            mic: is_input_device(&text(sys::obs_source_get_unversioned_id(source))),
            active: sys::obs_source_active(source),
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
        if let Some(mut s) = unsafe { describe(source) } {
            s.mic |= MIC_AUX_CHANNELS.contains(&channel);
            match found.iter_mut().find(|f| f.uuid == s.uuid) {
                Some(f) => f.mic |= s.mic,
                None => found.push(s),
            }
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

/// Turns `track` off on every audio source, then marks it managed in the loaded scene collection. On the UI thread.
pub fn take_over(track: u8) -> bool {
    if !cfg!(unix) {
        log::warn(&format!(
            "ignored taking over Track {track}: D-Streamy can't change Discord audio on Windows yet"
        ));
        return false;
    }
    let audio = sources();
    for source in &audio {
        with_source(&source.uuid, |s| set_track(s, track, false));
    }
    if !change(|c, _| c.model.take_over(track)) {
        return false;
    }
    log::info(&format!(
        "D-Streamy manages Track {track} in this scene collection: turned it off on {} audio sources",
        audio.len()
    ));
    true
}

/// Turns managed `track` on or off for one source. On the UI thread.
pub fn set_source(uuid: &str, track: u8, on: bool) -> bool {
    if !cfg!(unix) {
        log::warn("ignored a tick: D-Streamy can't change Discord audio on Windows yet");
        return false;
    }
    if !with_controls(|c| c.model.manages(track)) {
        log::warn(&format!(
            "ignored a tick for Track {track}: D-Streamy doesn't manage it in this scene collection"
        ));
        return false;
    }
    let found = with_source(uuid, |s| set_track(s, track, on));
    if !found {
        log::warn(&format!("the audio source {uuid} is gone"));
    }
    found
}

/// Why Go Live must wait: the next start's track is unmanaged and carries a microphone.
#[cfg(unix)]
pub fn mic_guard() -> Option<MicGuard> {
    let audio = sources();
    with_controls(|c| guard_for(&c.model, &audio))
}

/// Each audio source's mixer flags, by UUID.
pub type Mixers = Vec<(String, u32)>;

pub fn mixers() -> Mixers {
    sources().into_iter().map(|s| (s.uuid, s.mixers)).collect()
}

/// What a Cancel would set: the managed tracks, and each source whose flags change.
fn revert_plan(
    before: &[(String, u32)],
    touched: u32,
    managed_at_open: u32,
) -> (u32, Vec<(String, u32)>) {
    let managed =
        with_controls(|c| restored_managed(c.model.managed_mixers(), managed_at_open, touched));
    let sources = sources()
        .into_iter()
        .filter_map(|source| {
            let old = before
                .iter()
                .find(|(uuid, _)| *uuid == source.uuid)
                .map(|(_, mixers)| *mixers);
            let next = restored_mixers(source.mixers, old, touched, managed);
            (next != source.mixers).then_some((source.uuid, next))
        })
        .collect();
    (managed, sources)
}

/// Whether the `touched` tracks or the managed tracks differ from when a dialog opened.
pub fn changed_since(before: &[(String, u32)], touched: u32, managed_at_open: u32) -> bool {
    let current = with_controls(|c| c.model.managed_mixers());
    let (managed, sources) = revert_plan(before, touched, managed_at_open);
    managed != current || !sources.is_empty()
}

/// Puts the `touched` tracks back as they were when a dialog opened; the managed tracks too.
pub fn restore(before: &[(String, u32)], touched: u32, managed_at_open: u32) {
    let (managed, plan) = revert_plan(before, touched, managed_at_open);
    change(|c, _| c.model.restore_managed(managed));
    for (uuid, next) in &plan {
        with_source(uuid, |s| {
            APPLYING.with(|a| a.set(true));
            // SAFETY: `with_source` holds a reference.
            unsafe { sys::obs_source_set_audio_mixers(s, *next) };
            APPLYING.with(|a| a.set(false));
        });
    }
    crate::hooks::observe(
        "tracks_restored",
        || serde_json::json!({"sources":plan.len()}),
    );
    log::info(&format!(
        "D-Streamy put back the Discord tracks on {} audio sources",
        plan.len()
    ));
}

/// Lets go of a managed track Discord doesn't get; mixer flags stay as they are. On the UI thread.
pub fn release(track: u8) -> bool {
    let released = change(|c, _| c.model.release(track));
    if released {
        log::info(&format!(
            "D-Streamy stopped managing Track {track}: new sources keep it, and no source changed"
        ));
        request_refresh();
    }
    released
}

/// The loaded collection had no D-Streamy data; the legacy managed track waits for its sources.
static ADOPTION_CANDIDATE: AtomicBool = AtomicBool::new(false);

/// The frontend loads a scene collection's "modules" data before its sources (`OBSBasic::LoadData`).
///
/// # Safety
/// `modules` is live or NULL.
pub unsafe fn collection_loading(modules: *mut sys::obs_data_t) {
    let saved = if modules.is_null() {
        None
    } else {
        // SAFETY: the caller's contract; the object reference is released below.
        unsafe {
            let ours = sys::obs_data_get_obj(modules, COLLECTION_KEY.as_ptr());
            let saved =
                (!ours.is_null()).then(|| sys::obs_data_get_int(ours, MANAGED_MIXERS.as_ptr()));
            sys::obs_data_release(ours);
            saved
        }
    };
    let managed = saved.map_or(0, |v| u32::try_from(v).unwrap_or(0));
    let dropped = change(|c, p| {
        c.model.collection_loaded(managed);
        let pending = c.model.settings().adopt_managed_track.is_some();
        (saved.is_some() && pending)
            .then(|| c.edit(p.write(), |m| m.take_adoption()))
            .flatten()
    });
    ADOPTION_CANDIDATE.store(saved.is_none(), Ordering::Release);
    if let Some(track) = dropped {
        log::info(&format!(
            "this scene collection has its own Discord tracks, so Track {track}, managed before \
             D-Streamy kept that per scene collection, is not adopted"
        ));
    }
    if managed != 0 {
        log::info(&format!(
            "this scene collection's Discord tracks managed by D-Streamy: {managed:#08b}"
        ));
    }
    left_collection();
}

/// The frontend loaded the collection's sources: the legacy managed track is adopted unless a mic is on it.
pub fn collection_loaded() {
    if ADOPTION_CANDIDATE.swap(false, Ordering::AcqRel)
        && with_controls(|c| c.model.settings().adopt_managed_track.is_some())
    {
        let audio = sources();
        let (track, mics) = change(|c, p| {
            let track = c.edit(p.write(), |m| m.take_adoption());
            let mics: Vec<String> = track
                .map(|t| {
                    audio
                        .iter()
                        .filter(|s| s.mic && s.mixers & track_bit(t) != 0)
                        .map(|s| s.name.clone())
                        .collect()
                })
                .unwrap_or_default();
            if let Some(t) = track.filter(|_| mics.is_empty()) {
                c.model.take_over(t);
            }
            (track, mics)
        });
        if let Some(track) = track.filter(|_| !mics.is_empty()) {
            crate::hooks::observe(
                "track_adoption_refused",
                || serde_json::json!({"track":track,"microphones":mics}),
            );
        }
        match (track, mics.is_empty()) {
            (Some(t), true) => log::info(&format!(
                "D-Streamy managed Track {t} before it kept this per scene collection: this scene \
                 collection keeps it, its mixer flags unchanged"
            )),
            (Some(t), false) => log::warn(&format!(
                "D-Streamy managed Track {t} before it kept this per scene collection, but {} is on it \
                 here, so Track {t} stays unmanaged and Go Live waits until it is sorted out",
                mics.join(", ")
            )),
            (None, _) => {}
        }
    }
    settings_source::collection_loaded();
    request_refresh();
}

/// The frontend saves the scene collection; its "modules" data keeps the managed tracks next to the mixer flags.
///
/// # Safety
/// `modules` is live.
pub unsafe fn collection_saving(modules: *mut sys::obs_data_t) {
    settings_source::collection_saved();
    let managed = with_controls(|c| c.model.managed_mixers());
    // SAFETY: the caller's contract; libobs copies the key and takes its own reference to the object.
    unsafe {
        if managed == 0 {
            sys::obs_data_erase(modules, COLLECTION_KEY.as_ptr());
            return;
        }
        let ours = sys::obs_data_create();
        sys::obs_data_set_int(ours, MANAGED_MIXERS.as_ptr(), i64::from(managed));
        sys::obs_data_set_obj(modules, COLLECTION_KEY.as_ptr(), ours);
        sys::obs_data_release(ours);
    }
}

/// The frontend cleared the scene collection; until another loads, D-Streamy manages no track.
pub fn collection_cleared() {
    ADOPTION_CANDIDATE.store(false, Ordering::Release);
    change(|c, _| c.model.collection_loaded(0));
    left_collection();
}

fn left_collection() {
    settings_source::collection_changed();
    crate::ui::collection_changed();
    request_refresh();
}

enum Request {
    Refresh,
    /// Decide a possible Cancel once the UI thread finished the call that raised it.
    Settle,
    Stop,
}

struct Refresher {
    requests: Sender<Request>,
    thread: JoinHandle<()>,
}

static REFRESHER: Mutex<Option<Refresher>> = Mutex::new(None);

/// Rebuilds the open dialog soon, from another thread, so OBS queues the rebuild on its UI thread.
pub fn request_refresh() {
    let refresher = REFRESHER.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(r) = refresher.as_ref() {
        let _ = r.requests.send(Request::Refresh);
    }
}

/// Runs `settings_source::settle` on the UI thread after the current UI call returns; false when it can't.
pub fn request_settle() -> bool {
    let sent = REFRESHER
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .is_some_and(|r| r.requests.send(Request::Settle).is_ok());
    if sent {
        return true;
    }
    let queued = std::thread::Builder::new()
        .name("dstreamy settle".into())
        // SAFETY: queued from another thread, so OBS runs it later on its UI thread.
        .spawn(|| unsafe {
            sys::obs_queue_task(sys::OBS_TASK_UI, Some(settle_task), ptr::null_mut(), false)
        });
    if let Err(e) = &queued {
        log::warn(&format!(
            "cannot queue the settings dialog's Cancel ({e}): its Discord track changes stay; \
             undo them in Edit › Advanced Audio Properties"
        ));
    }
    queued.is_ok()
}

extern "C" fn settle_task(_param: *mut c_void) {
    guard("settings settle", (), settings_source::settle);
}

fn refresh_later() {
    if !APPLYING.with(Cell::get) && settings_source::dialog_open() {
        request_refresh();
    }
}

/// Queues a settle at once, and notes a refresh or stop for the caller.
fn take(request: Request, refresh: &mut bool, stop: &mut bool) {
    match request {
        // SAFETY: queued from this thread, so OBS runs it later on its UI thread.
        Request::Settle => unsafe {
            sys::obs_queue_task(sys::OBS_TASK_UI, Some(settle_task), ptr::null_mut(), false)
        },
        Request::Refresh => *refresh = true,
        Request::Stop => *stop = true,
    }
}

fn run_refresher(requests: mpsc::Receiver<Request>) {
    while let Ok(first) = requests.recv() {
        let (mut refresh, mut stop) = (false, false);
        take(first, &mut refresh, &mut stop);
        while let Ok(more) = requests.try_recv() {
            take(more, &mut refresh, &mut stop);
        }
        if refresh {
            std::thread::sleep(REFRESH_DELAY);
            while let Ok(more) = requests.try_recv() {
                take(more, &mut refresh, &mut stop);
            }
            guard("audio refresh", (), settings_source::refresh);
        }
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
            let managed = with_controls(|c| c.model.managed_mixers());
            if managed != 0 {
                // SAFETY: the source is live for the signal.
                unsafe {
                    let mixers = sys::obs_source_get_audio_mixers(source);
                    if mixers & managed != 0 {
                        APPLYING.with(|a| a.set(true));
                        sys::obs_source_set_audio_mixers(source, mixers & !managed);
                        APPLYING.with(|a| a.set(false));
                    }
                }
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
        let _ = r.requests.send(Request::Stop);
        if r.thread.join().is_err() {
            log::warn("the D-Streamy refresh thread panicked");
        }
    }
}
