//! `dstreamy_settings`: a private source whose stock properties dialog is D-Streamy's settings window.

use std::cell::Cell;
use std::ffi::{CStr, CString, c_char, c_void};
use std::mem;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Mutex, PoisonError};

use dstreamy_obs_producer::dock::{
    AUDIO_TRACK_TITLE, AUDIO_TRACKS, Choice, DEFAULT_AUDIO_TRACK, DockState, TrackStatus,
    track_name,
};
use dstreamy_obs_producer::dock_config::{ConfigError, ConfigState};
use dstreamy_obs_producer::dock_view::UNSUPPORTED;
use dstreamy_obs_producer::mixer::{
    AudioView, CONFIRM, NOT_NOW, audio_view, instead_label, mic_guard, notices, release_label,
    track_bit,
};
use dstreamy_obs_producer::panel::NO_AUDIO_CONTROLS;
use dstreamy_obs_sys as sys;

use crate::config;
use crate::guard::guard;
use crate::log;
use crate::output::text;
use crate::session;

pub const SOURCE_ID: &CStr = c"dstreamy_settings";
pub const SOURCE_NAME: &CStr = c"D-Streamy";

const AUDIO_TRACK: &CStr = c"audio_track";
const GUILD: &CStr = c"guild_id";
const CHANNEL: &CStr = c"channel_id";
const STATUS: &CStr = c"status";

/// The producer model behind every dialog; it outlives each dialog's source.
struct Controls {
    state: ConfigState,
    /// Saves handed out, and saves written or superseded; they differ while a write is on its way to disk.
    issued: u64,
    flushed: u64,
}

static CONTROLS: Mutex<Option<Controls>> = Mutex::new(None);
/// Serializes the config file's reads and writes; taken before `CONTROLS`, never while holding it.
static DISK: Mutex<u64> = Mutex::new(0);

fn controls<R>(f: impl FnOnce(&mut Controls) -> R) -> R {
    let mut guard = CONTROLS.lock().unwrap_or_else(PoisonError::into_inner);
    f(guard.get_or_insert_with(|| Controls {
        state: ConfigState::default(),
        issued: 0,
        flushed: 0,
    }))
}

/// Reads the model; never use it to change settings, since nothing it does is saved.
pub fn with_controls<R>(f: impl FnOnce(&ConfigState) -> R) -> R {
    controls(|c| f(&c.state))
}

/// The JSON a step asked to save, written once the model lock is released.
#[derive(Default)]
pub struct Pending(Option<String>);

impl Pending {
    pub fn write(&mut self) -> impl FnOnce(&str) -> Result<(), ConfigError> + '_ {
        |json| {
            self.0 = Some(json.to_owned());
            Ok(())
        }
    }
}

/// Changes the model; a save it asks for is written to disk after the lock is released.
pub fn change<R>(f: impl FnOnce(&mut ConfigState, &mut Pending) -> R) -> R {
    let (result, save) = controls(|c| {
        let mut pending = Pending::default();
        let result = f(&mut c.state, &mut pending);
        (result, issue(c, pending))
    });
    if let Some(save) = save {
        flush(save);
    }
    result
}

fn issue(c: &mut Controls, pending: Pending) -> Option<(u64, String)> {
    let json = pending.0?;
    c.issued += 1;
    Some((c.issued, json))
}

/// Writes save `generation` unless a newer one already reached the disk; only the newest save reports its error.
fn flush((generation, json): (u64, String)) {
    let mut written = DISK.lock().unwrap_or_else(PoisonError::into_inner);
    let result = (generation > *written).then(|| config::save(&json));
    *written = (*written).max(generation);
    controls(|c| {
        c.flushed = c.flushed.max(generation);
        if let Some(Err(e)) = result {
            log::warn(&e.message(c.state.model.settings().audio_track));
            if generation == c.issued {
                c.state.write_failed(e);
            }
        }
    });
}

/// A dialog opens: the file is read outside the model lock, and not at all while a save is still being written.
pub fn open_config() {
    let save = {
        let _disk = DISK.lock().unwrap_or_else(PoisonError::into_inner);
        let read = config::load();
        controls(|c| {
            if c.issued != c.flushed {
                return None;
            }
            let mut pending = Pending::default();
            let before = c.state.error().cloned();
            c.state.open(|| read, pending.write());
            if let Some(e) = c.state.error().filter(|e| Some(*e) != before.as_ref()) {
                log::warn(&e.message(c.state.model.settings().audio_track));
            }
            issue(c, pending)
        })
    };
    if let Some(save) = save {
        flush(save);
    }
}

/// Runs a dialog change and logs the config error it leaves.
fn config_step(c: &mut ConfigState, step: impl FnOnce(&mut ConfigState)) {
    step(c);
    if let Some(e) = c.error().filter(|e| !matches!(e, ConfigError::Failed(_))) {
        log::warn(&e.message(c.model.settings().audio_track));
    }
}

/// The open dialog's source, so the control thread can ask OBS to rebuild it.
struct Dialog(*mut sys::obs_weak_source_t);

// SAFETY: weak source references are reference-counted by libobs and usable from any thread.
unsafe impl Send for Dialog {}

static DIALOG: Mutex<Option<Dialog>> = Mutex::new(None);

fn watch_dialog(source: *mut sys::obs_source_t) {
    // SAFETY: `source` is the source being created; the weak reference is released in `forget_dialog` or here.
    let weak = Dialog(unsafe { sys::obs_source_get_weak_source(source) });
    let old = DIALOG
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .replace(weak);
    if let Some(old) = old {
        // SAFETY: the weak reference this module took.
        unsafe { sys::obs_weak_source_release(old.0) };
    }
}

/// False when `source` isn't the dialog [`refresh`] rebuilds.
fn forget_dialog(source: *mut sys::obs_source_t) -> bool {
    let mut dialog = DIALOG.lock().unwrap_or_else(PoisonError::into_inner);
    // SAFETY: both pointers are live; the weak reference is this module's own.
    let ours = dialog
        .as_ref()
        .is_some_and(|d| unsafe { sys::obs_weak_source_references_source(d.0, source) });
    if ours && let Some(old) = dialog.take() {
        // SAFETY: as above.
        unsafe { sys::obs_weak_source_release(old.0) };
    }
    ours
}

/// The open dialog's source, as a strong reference released on drop.
struct DialogSource(*mut sys::obs_source_t);

impl DialogSource {
    fn get() -> Option<Self> {
        let source = DIALOG
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            // SAFETY: the weak reference is live while it is in DIALOG; this returns a strong reference or NULL.
            .map(|d| unsafe { sys::obs_weak_source_get_source(d.0) });
        source.filter(|s| !s.is_null()).map(Self)
    }
}

impl Drop for DialogSource {
    fn drop(&mut self) {
        // SAFETY: the strong reference taken in `get`.
        unsafe { sys::obs_source_release(self.0) };
    }
}

/// Set while Tools > D-Streamy opens a dialog, which OBS shows only once the open one closes.
static OPENING: AtomicBool = AtomicBool::new(false);

/// A dialog created while another was open; it takes over at its first build, which OBS queues after the open.
struct Successor(*mut sys::obs_source_t);

// SAFETY: the pointer is only compared, and dereferenced on the UI thread while Tools > D-Streamy holds it.
unsafe impl Send for Successor {}

static SUCCESSOR: Mutex<Option<Successor>> = Mutex::new(None);

struct Opening;

impl Drop for Opening {
    fn drop(&mut self) {
        OPENING.store(false, Ordering::Release);
    }
}

/// Runs Tools > D-Streamy's create and open: a dialog already open stays in charge until OBS closes it.
pub fn opening<R>(f: impl FnOnce() -> R) -> R {
    OPENING.store(true, Ordering::Release);
    let _done = Opening;
    f()
}

/// OBS opened no dialog for `source`: the open one's close was cancelled.
pub fn not_opened(source: *mut sys::obs_source_t) {
    take_successor(source);
}

fn take_successor(source: *mut sys::obs_source_t) -> bool {
    let mut successor = SUCCESSOR.lock().unwrap_or_else(PoisonError::into_inner);
    let ours = successor.as_ref().is_some_and(|s| s.0 == source);
    if ours {
        successor.take();
    }
    ours
}

fn successor() -> Option<*mut sys::obs_source_t> {
    SUCCESSOR
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|s| s.0)
}

/// `source` becomes the dialog that refreshes, confirms and undoes, with what it shows now.
fn activate(source: *mut sys::obs_source_t) {
    watch_dialog(source);
    let (choice, managed) = with_controls(|c| (c.model.choice(), c.model.managed_mixers()));
    let mixers = cfg!(unix).then(crate::audio::mixers);
    CONFIRMING.store(0, Ordering::Release);
    OPENED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(Opened {
            source,
            choice,
            mixers,
            managed,
            touched: 0,
        });
}

/// Writes the model's picks into a dialog's live settings.
unsafe fn write_picks(settings: *mut sys::obs_data_t) {
    let picked = with_controls(|c| c.model.settings().clone());
    // SAFETY: the caller's contract; libobs copies the strings.
    unsafe {
        sys::obs_data_set_int(
            settings,
            AUDIO_TRACK.as_ptr(),
            i64::from(picked.audio_track),
        );
        for (key, value) in [(GUILD, picked.guild_id), (CHANNEL, picked.channel_id)] {
            let value = c_text(&value.unwrap_or_default());
            sys::obs_data_set_string(settings, key.as_ptr(), value.as_ptr());
        }
    }
}

pub fn dialog_open() -> bool {
    DIALOG
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_some()
}

/// Asks the open dialog, if any, to rebuild itself from the model; callable from any thread, without the model lock.
pub fn refresh() {
    if let Some(source) = DialogSource::get() {
        // SAFETY: a strong reference; OBS's properties view reloads on its own thread.
        unsafe { sys::obs_source_update_properties(source.0) };
    }
}

fn selected_track(settings: *mut sys::obs_data_t) -> Option<u8> {
    // SAFETY: the caller passes live settings.
    let raw = unsafe { sys::obs_data_get_int(settings, AUDIO_TRACK.as_ptr()) };
    u8::try_from(raw).ok().filter(|t| AUDIO_TRACKS.contains(t))
}

/// The status line for a dialog showing `selected`, how OBS should colour it, and whether a mic holds Start back.
fn status(selected: Option<u8>) -> (String, sys::obs_text_info_type, bool) {
    let sources = if cfg!(unix) {
        crate::audio::sources()
    } else {
        Vec::new()
    };
    with_controls(|c| {
        let mut preview = c.model.clone();
        if let Some(track) = selected {
            preview.select_audio_track(track);
        }
        let idle = preview.state() == &DockState::Idle;
        let live = matches!(preview.state(), DockState::Live { .. });
        let mic = (cfg!(unix) && (idle || live))
            .then(|| mic_guard(&preview, &sources))
            .flatten();
        let held = idle && mic.is_some();
        let quiet = idle && preview.error().is_none() && mic.is_none();
        let session = match &mic {
            _ if !cfg!(unix) => UNSUPPORTED.to_owned(),
            Some(guard) => guard.message(),
            None => preview.status_text(true),
        };
        if let Some(e) = c.error() {
            let config = e.message(preview.settings().audio_track);
            let text = if quiet && cfg!(unix) {
                config
            } else {
                format!("{}. {config}", session.trim_end_matches('.'))
            };
            return (text, sys::OBS_TEXT_INFO_ERROR, held);
        }
        let kind = match (preview.track_status(), preview.error()) {
            _ if !cfg!(unix) || mic.is_some() => sys::OBS_TEXT_INFO_WARNING,
            (TrackStatus::Pending { .. }, _) => sys::OBS_TEXT_INFO_WARNING,
            (TrackStatus::Idle { .. }, Some(_)) => sys::OBS_TEXT_INFO_ERROR,
            _ => sys::OBS_TEXT_INFO_NORMAL,
        };
        (session, kind, held)
    })
}

fn c_text(text: &str) -> CString {
    CString::new(text.replace('\0', " ")).expect("NUL bytes were replaced")
}

extern "C" fn get_name(_type_data: *mut c_void) -> *const c_char {
    SOURCE_NAME.as_ptr()
}

extern "C" fn create(
    settings: *mut sys::obs_data_t,
    source: *mut sys::obs_source_t,
) -> *mut c_void {
    guard("settings create", source.cast(), || {
        open_config();
        let guild = with_controls(|c| c.model.settings().guild_id.clone());
        session::load_lists(guild);
        if OPENING.load(Ordering::Acquire) && dialog_open() {
            *SUCCESSOR.lock().unwrap_or_else(PoisonError::into_inner) = Some(Successor(source));
        } else {
            activate(source);
        }
        // SAFETY: libobs passes the new source's settings, which it owns.
        unsafe { write_picks(settings) };
        // libobs treats a null data pointer as a failed create.
        source.cast()
    })
}

extern "C" fn destroy(data: *mut c_void) {
    guard("settings destroy", (), || {
        let source: *mut sys::obs_source_t = data.cast();
        take_successor(source);
        OPENED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|o| o.source != source);
        if forget_dialog(source) && !crate::ui::panel_open() {
            change(|c, _| c.model.forget_released());
        }
        closed(source);
    });
}

extern "C" fn get_defaults(settings: *mut sys::obs_data_t) {
    guard("settings get_defaults", (), || {
        // SAFETY: libobs passes the defaults object it owns.
        unsafe {
            sys::obs_data_set_default_int(
                settings,
                AUDIO_TRACK.as_ptr(),
                i64::from(DEFAULT_AUDIO_TRACK),
            )
        };
    });
}

/// What each open dialog showed when it opened, by source, so its Cancel can put exactly that back.
struct Opened {
    source: *mut sys::obs_source_t,
    choice: Choice,
    /// Each audio source's mixer flags; `None` once another scene collection replaced those sources.
    mixers: Option<crate::audio::Mixers>,
    managed: u32,
    /// The tracks this dialog took over or ticked, as mixer bits.
    touched: u32,
}

// SAFETY: the pointer is only compared, never dereferenced.
unsafe impl Send for Opened {}

static OPENED: Mutex<Vec<Opened>> = Mutex::new(Vec::new());

/// The open dialogs changed `track`; `settings` records whether any change still stands.
fn touch(track: u8, settings: *mut sys::obs_data_t) {
    for o in OPENED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter_mut()
    {
        o.touched |= track_bit(track);
    }
    if let Some(dialog) = DialogSource::get() {
        // SAFETY: the caller passes the dialog's live settings or NULL.
        unsafe { write_changed(dialog.0, settings) };
    }
}

/// Whether a mixer change the dialog of `source` made still stands.
fn changed(source: *mut sys::obs_source_t) -> bool {
    let opened = OPENED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .find(|o| o.source == source)
        .map(|o| (o.mixers.clone(), o.touched, o.managed));
    opened.is_some_and(|(mixers, touched, managed)| {
        touched != 0
            && mixers
                .as_ref()
                .is_some_and(|m| crate::audio::changed_since(m, touched, managed))
    })
}

/// Safety: `settings` is the live settings of `source`'s dialog or NULL, used on the UI thread.
unsafe fn write_changed(source: *mut sys::obs_source_t, settings: *mut sys::obs_data_t) {
    if settings.is_null() {
        return;
    }
    let changed = changed(source);
    // SAFETY: the caller's contract.
    unsafe {
        if changed {
            sys::obs_data_set_int(settings, CHANGED.as_ptr(), 1);
        } else if sys::obs_data_has_user_value(settings, CHANGED.as_ptr()) {
            sys::obs_data_unset_user_value(settings, CHANGED.as_ptr());
        }
    }
}

/// Brings the open dialog's Save or Discard question in line with its mixer changes. On the UI thread.
fn sync_changed() {
    if let Some(source) = DialogSource::get() {
        // SAFETY: a strong reference to a live source.
        let settings = unsafe { SourceSettings::of(source.0) };
        // SAFETY: the dialog's live settings, on the UI thread.
        unsafe { write_changed(source.0, settings.0) };
    }
}

/// The panel changed the Discord tracks: the open dialog's checkboxes and close question follow. On the UI thread.
pub fn mixers_changed() {
    if dialog_open() {
        sync_changed();
        crate::audio::request_refresh();
    }
}

/// Runs `f` with a clicked dialog's settings, or NULL; `data` is the source `create` returned.
fn with_dialog_settings<R>(data: *mut c_void, f: impl FnOnce(*mut sys::obs_data_t) -> R) -> R {
    // SAFETY: OBS passes the clicked source's data, which is the source itself or NULL.
    let settings = unsafe { SourceSettings::of(data.cast()) };
    f(settings.0)
}

/// A source's settings reference, released on drop so a panic doesn't leak it.
struct SourceSettings(*mut sys::obs_data_t);

impl SourceSettings {
    /// Safety: `source` is NULL or a live source.
    unsafe fn of(source: *mut sys::obs_source_t) -> Self {
        if source.is_null() {
            Self(ptr::null_mut())
        } else {
            // SAFETY: the caller's contract; released in drop.
            Self(unsafe { sys::obs_source_get_settings(source) })
        }
    }
}

impl Drop for SourceSettings {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the reference taken in `of`.
            unsafe { sys::obs_data_release(self.0) };
        }
    }
}

/// Properties being built, destroyed on drop unless handed to the caller.
struct Properties(*mut sys::obs_properties_t);

impl Properties {
    fn into_raw(self) -> *mut sys::obs_properties_t {
        let props = self.0;
        mem::forget(self);
        props
    }
}

impl Drop for Properties {
    fn drop(&mut self) {
        // SAFETY: created by obs_properties_create and not handed out.
        unsafe { sys::obs_properties_destroy(self.0) };
    }
}

fn button_track(property: *mut sys::obs_property_t, prefix: &str) -> Option<u8> {
    // SAFETY: the view passes a live property; libobs owns the name.
    let name = unsafe { text(sys::obs_property_name(property)) };
    name.strip_prefix(prefix)?.parse().ok()
}

/// Another scene collection: the mixer flags the open dialogs saw are gone, and so is a takeover confirm.
pub fn collection_changed() {
    CONFIRMING.store(0, Ordering::Release);
    keep_mixer_changes();
}

/// The open dialogs' mixer changes stay as they are: no Cancel or Discard puts them back.
fn keep_mixer_changes() {
    for o in OPENED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter_mut()
    {
        o.mixers = None;
        o.touched = 0;
    }
    REVERTING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    sync_changed();
}

/// OBS leaves the collection and closes the dialog: its mixer changes stay, and closing asks only about picks.
pub fn collection_leaving() {
    let kept = OPENED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .any(|o| o.touched != 0);
    keep_mixer_changes();
    if kept {
        log::info(
            "OBS is leaving the scene collection: the Discord track changes made in the open D-Streamy dialog \
             stay, whatever its close asks",
        );
    }
}

/// Manages a dialog's opened Discord pick again if it was let go, so putting the pick back finds it managed.
fn reclaim_opened_pick(track: u8) {
    if change(|c, _| !c.model.manages(track) && c.model.take_over(track)) {
        log::info(&format!(
            "D-Streamy manages Track {track} again: it was the Discord track when the dialog opened"
        ));
    }
}

/// OBS saved the collection: a close right after (Duplicate, Rename) asks nothing until the next rebuild.
pub fn collection_saved() {
    let Some(source) = DialogSource::get() else {
        return;
    };
    // SAFETY: a strong reference to a live source.
    let settings = unsafe { SourceSettings::of(source.0) };
    // SAFETY: the dialog's live settings, on the UI thread, where OBS saves.
    let asked = unsafe { sys::obs_data_has_user_value(settings.0, CHANGED.as_ptr()) };
    if asked {
        // SAFETY: as above.
        unsafe { sys::obs_data_unset_user_value(settings.0, CHANGED.as_ptr()) };
        crate::audio::request_refresh();
    }
}

/// Another scene collection's sources loaded: an open dialog's Cancel can put back what changes from now on.
pub fn collection_loaded() {
    if OPENED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_empty()
    {
        return;
    }
    let (mixers, managed) = (
        cfg!(unix).then(crate::audio::mixers),
        with_controls(|c| c.model.managed_mixers()),
    );
    for o in OPENED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter_mut()
    {
        o.mixers = mixers.clone();
        o.managed = managed;
        o.touched = 0;
    }
}

thread_local! {
    /// Set while OBS rebuilds a dialog, which runs every modified callback; cleared by the next update.
    static RELOADING: Cell<bool> = const { Cell::new(false) };
}

/// A dialog whose picks went back to what it opened with: a Cancel or Discard, unless a modified callback follows.
struct Reverting {
    source: *mut sys::obs_source_t,
    pick: u8,
    mixers: Option<crate::audio::Mixers>,
    managed: u32,
    touched: u32,
    /// No settle could be queued; the dialog closing with this still armed makes it a Cancel that undid nothing.
    unsettled: bool,
}

// SAFETY: the pointer is only compared, never dereferenced.
unsafe impl Send for Reverting {}

static REVERTING: Mutex<Option<Reverting>> = Mutex::new(None);

/// A property's modified callback: the update before it was the user's own edit, not a Cancel.
fn edited() {
    if !RELOADING.with(Cell::get) {
        REVERTING
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }
}

/// Puts back the opened picks, listed or not, and arms the mixer part for [`settle`].
fn restore_picks(source: *mut sys::obs_source_t, opened: &Choice) {
    let load = change(|c, p| {
        let mut load = None;
        if c.model.choice() != *opened {
            config_step(c, |c| {
                load = c.edit(p.write(), |m| m.restore_choice(opened))
            });
        }
        load
    });
    if let Some(guild) = load {
        session::load_lists(Some(guild));
    }
    let pending = OPENED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .find(|o| o.source == source)
        .map(|o| Reverting {
            source,
            pick: o.choice.audio_track,
            mixers: o.mixers.clone(),
            managed: o.managed,
            touched: o.touched,
            unsettled: false,
        });
    *REVERTING.lock().unwrap_or_else(PoisonError::into_inner) = pending;
    let next = successor().filter(|_| OPENING.load(Ordering::Acquire));
    if let Some(next) = next {
        settle_pending();
        // SAFETY: Tools > D-Streamy holds the successor while OBS closes this dialog; OBS copies its settings later.
        unsafe { write_picks(SourceSettings::of(next).0) };
    } else if !crate::audio::request_settle()
        && let Some(r) = REVERTING
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
    {
        r.unsettled = true;
    }
}

/// What a Cancel couldn't put back, kept to tell when it no longer differs.
struct Unrestored {
    source: *mut sys::obs_source_t,
    mixers: crate::audio::Mixers,
    touched: u32,
    managed: u32,
}

// SAFETY: the pointer is only compared, never dereferenced.
unsafe impl Send for Unrestored {}

static UNRESTORED: Mutex<Option<Unrestored>> = Mutex::new(None);

/// A dialog is gone: a Cancel it armed without a settle undid nothing, and an older such notice has been seen.
fn closed(source: *mut sys::obs_source_t) {
    let lost = {
        let mut reverting = REVERTING.lock().unwrap_or_else(PoisonError::into_inner);
        let ours = reverting
            .as_ref()
            .is_some_and(|r| r.source == source && r.unsettled);
        ours.then(|| reverting.take()).flatten()
    };
    let mut unrestored = UNRESTORED.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(r) = lost.filter(|r| r.touched != 0)
        && let Some(mixers) = r.mixers
    {
        *unrestored = Some(Unrestored {
            source,
            mixers,
            touched: r.touched,
            managed: r.managed,
        });
        drop(unrestored);
        change(|c, _| c.model.restore_failed(r.touched));
    } else if unrestored.as_ref().is_some_and(|u| u.source != source) {
        unrestored.take();
        drop(unrestored);
        change(|c, _| c.model.forget_unrestored());
    }
}

/// Drops the failed-undo notice once the tracks match what the dialog opened with again.
pub fn recheck_unrestored() {
    let snapshot = UNRESTORED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|u| (u.mixers.clone(), u.touched, u.managed));
    if let Some((mixers, touched, managed)) = snapshot
        && !crate::audio::changed_since(&mixers, touched, managed)
    {
        forget_unrestored();
    }
}

pub fn forget_unrestored() {
    UNRESTORED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    change(|c, _| c.model.forget_unrestored());
}

/// On the UI thread once the call that armed it returned: a Cancel puts back the Discord tracks the dialog changed.
pub fn settle() {
    settle_pending();
    log::debug("settled the settings dialog's pending Cancel");
}

fn settle_pending() {
    let Some(pending) = REVERTING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
    else {
        return;
    };
    let current = OPENED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter_mut()
        .find(|o| o.source == pending.source)
        .map(|o| (std::mem::take(&mut o.touched), o.mixers.clone(), o.managed));
    let (touched, mixers, managed) =
        current.unwrap_or((pending.touched, pending.mixers, pending.managed));
    if mixers.is_some() && managed & track_bit(pending.pick) != 0 {
        reclaim_opened_pick(pending.pick);
    }
    if let Some(mixers) = mixers.filter(|_| touched != 0) {
        crate::audio::restore(&mixers, touched, managed);
        let (now, managed) = (
            crate::audio::mixers(),
            with_controls(|c| c.model.managed_mixers()),
        );
        // A dialog opened while this one closed saw the flags before they were put back.
        for o in OPENED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter_mut()
            .filter(|o| o.source != pending.source && o.touched == 0)
        {
            o.mixers = Some(now.clone());
            o.managed = managed;
        }
        crate::audio::request_refresh();
    }
}

/// Runs on the UI thread whenever the dialog changes a value, and on Cancel or Discard with every value it opened with.
extern "C" fn update(data: *mut c_void, settings: *mut sys::obs_data_t) {
    guard("settings update", (), || {
        RELOADING.with(|r| r.set(false));
        let source: *mut sys::obs_source_t = data.cast();
        // SAFETY: libobs passes the source's own settings.
        let (guild, channel) = unsafe { (picked(settings, GUILD), picked(settings, CHANNEL)) };
        let shown = selected_track(settings).map(|audio_track| Choice {
            guild_id: guild.clone(),
            channel_id: channel.clone(),
            audio_track,
        });
        let opened = OPENED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|o| o.source == source)
            .map(|o| o.choice.clone());
        // SAFETY: as above; Defaults clears every user value, while Cancel and Discard put the opened ones back.
        let defaults = !unsafe { sys::obs_data_has_user_value(settings, AUDIO_TRACK.as_ptr()) };
        if defaults {
            REVERTING
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
        } else if let Some(opened) = opened.filter(|o| Some(o) == shown.as_ref()) {
            restore_picks(source, &opened);
            return;
        }
        let load = change(|c, p| {
            let mut load = None;
            if c.model.settings().guild_id != guild {
                config_step(c, |c| {
                    load = c.edit(p.write(), |m| m.choose_guild(guild.as_deref()));
                });
            }
            load
        });
        if let Some(guild) = load {
            session::load_lists(Some(guild));
        }
        // The view keeps the old server's channel after a server change; only a channel the new server lists counts.
        let kept = change(|c, p| {
            if c.model.settings().channel_id != channel {
                config_step(c, |c| {
                    c.edit(p.write(), |m| m.choose_channel(channel.as_deref()));
                });
            }
            c.model.settings().channel_id.clone()
        });
        if kept != channel {
            let kept = c_text(&kept.unwrap_or_default());
            // SAFETY: libobs passes the source's own settings; it copies the string.
            unsafe { sys::obs_data_set_string(settings, CHANNEL.as_ptr(), kept.as_ptr()) };
        }
        change(|c, p| match selected_track(settings) {
            Some(track) => config_step(c, |c| {
                c.select_audio_track(track, p.write());
            }),
            None => {
                // SAFETY: libobs passes the source's own settings.
                let raw = unsafe { sys::obs_data_get_int(settings, AUDIO_TRACK.as_ptr()) };
                log::warn(&format!(
                    "ignored Discord audio track {raw}: pick Track 1 to Track 6"
                ));
                let kept = i64::from(c.model.settings().audio_track);
                // SAFETY: as above.
                unsafe { sys::obs_data_set_int(settings, AUDIO_TRACK.as_ptr(), kept) };
            }
        });
    });
}

fn set_status(
    status_prop: *mut sys::obs_property_t,
    (text, kind, _): (String, sys::obs_text_info_type, bool),
) {
    let text = c_text(&text);
    // SAFETY: the property belongs to properties the caller holds; libobs copies the text.
    unsafe {
        sys::obs_property_set_description(status_prop, text.as_ptr());
        sys::obs_property_text_set_info_type(status_prop, kind);
    }
}

extern "C" fn track_modified(
    props: *mut sys::obs_properties_t,
    _property: *mut sys::obs_property_t,
    settings: *mut sys::obs_data_t,
) -> bool {
    guard("settings track_modified", false, || {
        edited();
        // SAFETY: the view passes the properties it shows and their settings.
        let status_prop = unsafe { sys::obs_properties_get(props, STATUS.as_ptr()) };
        if status_prop.is_null() {
            return false;
        }
        let selected = selected_track(settings);
        set_status(status_prop, status(selected));
        if selected
            .map(shown_track)
            .is_some_and(|t| t != AUDIO_BUILT_FOR.load(Ordering::Acquire))
        {
            crate::audio::request_refresh();
        }
        true
    })
}

/// The track the open dialog's audio part shows; a different pick rebuilds it.
static AUDIO_BUILT_FOR: AtomicU8 = AtomicU8::new(0);

/// The audio part shows the track Discord hears while live, like the panel, else the dialog's pick.
fn shown_track(selected: u8) -> u8 {
    with_controls(|c| c.model.live_track()).unwrap_or(selected)
}
/// The track whose takeover waits for its confirm in the dialog; 0 for none.
static CONFIRMING: AtomicU8 = AtomicU8::new(0);

const AUDIO_HELP: &CStr = c"audio_help";
const TAKE_OVER: &CStr = c"take_over";
const TAKE_OVER_EFFECT: &CStr = c"take_over_effect";
const CONFIRM_TAKE_OVER: &CStr = c"confirm_take_over";
const CANCEL_TAKE_OVER: &CStr = c"cancel_take_over";
const AUDIO_EMPTY: &CStr = c"audio_empty";
const RELEASE_PREFIX: &str = "release_";
const INSTEAD_PREFIX: &str = "instead_";
/// A user value only while the dialog's mixer changes stand, so closing with X asks Save or Discard like OBS does.
const CHANGED: &CStr = c"dstreamy_mixers_changed";
const SOURCE_PREFIX: &str = "source_";

// Each button returns false and rebuilds from another thread: a rebuild inside the click frees the button.
extern "C" fn take_over_clicked(
    _props: *mut sys::obs_properties_t,
    _property: *mut sys::obs_property_t,
    _data: *mut c_void,
) -> bool {
    guard("settings take_over", false, || {
        CONFIRMING.store(AUDIO_BUILT_FOR.load(Ordering::Acquire), Ordering::Release);
        crate::audio::request_refresh();
        false
    })
}

extern "C" fn confirm_take_over_clicked(
    _props: *mut sys::obs_properties_t,
    _property: *mut sys::obs_property_t,
    data: *mut c_void,
) -> bool {
    guard("settings confirm_take_over", false, || {
        let track = CONFIRMING.swap(0, Ordering::AcqRel);
        if track != 0 && crate::audio::take_over(track) {
            with_dialog_settings(data, |settings| touch(track, settings));
        }
        crate::audio::request_refresh();
        false
    })
}

extern "C" fn release_clicked(
    _props: *mut sys::obs_properties_t,
    property: *mut sys::obs_property_t,
    data: *mut c_void,
) -> bool {
    guard("settings release", false, || {
        if let Some(track) = button_track(property, RELEASE_PREFIX)
            && crate::audio::release(track)
        {
            with_dialog_settings(data, |settings| touch(track, settings));
        }
        false
    })
}

/// Picks another track for Discord instead of a takeover, as the track list would.
extern "C" fn instead_clicked(
    _props: *mut sys::obs_properties_t,
    property: *mut sys::obs_property_t,
    data: *mut c_void,
) -> bool {
    guard("settings instead", false, || {
        let Some(track) = button_track(property, INSTEAD_PREFIX) else {
            return false;
        };
        CONFIRMING.store(0, Ordering::Release);
        with_dialog_settings(data, |settings| {
            if !settings.is_null() {
                // SAFETY: the dialog's live settings.
                unsafe { sys::obs_data_set_int(settings, AUDIO_TRACK.as_ptr(), i64::from(track)) };
            }
        });
        change(|c, p| {
            config_step(c, |c| {
                c.select_audio_track(track, p.write());
            })
        });
        crate::audio::request_refresh();
        false
    })
}

extern "C" fn cancel_take_over_clicked(
    _props: *mut sys::obs_properties_t,
    _property: *mut sys::obs_property_t,
    _data: *mut c_void,
) -> bool {
    guard("settings cancel_take_over", false, || {
        CONFIRMING.store(0, Ordering::Release);
        crate::audio::request_refresh();
        false
    })
}

extern "C" fn source_toggled(
    _props: *mut sys::obs_properties_t,
    property: *mut sys::obs_property_t,
    settings: *mut sys::obs_data_t,
) -> bool {
    guard("settings source_toggled", false, || {
        edited();
        // SAFETY: the view passes a live property and its settings; libobs owns the name.
        let (name, on, written) = unsafe {
            let name = sys::obs_property_name(property);
            (
                text(name),
                sys::obs_data_get_bool(settings, name),
                sys::obs_data_has_user_value(settings, name),
            )
        };
        if !written {
            return false;
        }
        let track = AUDIO_BUILT_FOR.load(Ordering::Acquire);
        if let Some(uuid) = name.strip_prefix(SOURCE_PREFIX)
            && crate::audio::set_source(uuid, track, on)
        {
            let key = c_text(&name);
            // SAFETY: the view's live settings; a default holds the tick, so only CHANGED says it changed.
            unsafe {
                sys::obs_data_unset_user_value(settings, key.as_ptr());
                sys::obs_data_set_default_bool(settings, key.as_ptr(), on);
            }
            touch(track, settings);
        }
        false
    })
}

/// Adds the audio part for `track` and writes each checkbox's real state into `settings`.
///
/// # Safety
/// `props` is live and `settings` is live or NULL; libobs copies every string.
unsafe fn add_audio(props: *mut sys::obs_properties_t, settings: *mut sys::obs_data_t, track: u8) {
    AUDIO_BUILT_FOR.store(track, Ordering::Release);
    // SAFETY: the caller's contract.
    unsafe {
        let note = |key: &CStr, text: &str, kind: sys::obs_text_info_type| {
            let text = c_text(text);
            let p = sys::obs_properties_add_text(
                props,
                key.as_ptr(),
                text.as_ptr(),
                sys::OBS_TEXT_INFO,
            );
            sys::obs_property_text_set_info_type(p, kind);
        };
        let info = |key: &CStr, text: &str| note(key, text, sys::OBS_TEXT_INFO_NORMAL);
        let button = |key: &CStr, label: &str, clicked: sys::obs_property_clicked_t| {
            let label = c_text(label);
            sys::obs_properties_add_button(props, key.as_ptr(), label.as_ptr(), clicked);
        };
        if !cfg!(unix) {
            info(AUDIO_HELP, NO_AUDIO_CONTROLS);
            return;
        }
        let sources = crate::audio::sources();
        let (view, warnings) = with_controls(|c| {
            (
                audio_view(&c.model, track, &sources),
                notices(&c.model, &sources),
            )
        });
        for (i, warning) in warnings.into_iter().enumerate() {
            note(
                &c_text(&format!("audio_note_{i}")),
                &warning.text,
                sys::OBS_TEXT_INFO_WARNING,
            );
            if let Some(release) = warning.release {
                let key = c_text(&format!("{RELEASE_PREFIX}{release}"));
                button(&key, &release_label(release), Some(release_clicked));
            }
        }
        match view {
            AudioView::Unmanaged {
                help,
                take_over,
                effect,
                instead,
                ..
            } => {
                info(AUDIO_HELP, &help);
                if CONFIRMING.load(Ordering::Acquire) == track {
                    note(TAKE_OVER_EFFECT, &effect, sys::OBS_TEXT_INFO_WARNING);
                    button(CONFIRM_TAKE_OVER, CONFIRM, Some(confirm_take_over_clicked));
                    if let Some(other) = instead {
                        let key = c_text(&format!("{INSTEAD_PREFIX}{other}"));
                        button(&key, &instead_label(other), Some(instead_clicked));
                    }
                    button(CANCEL_TAKE_OVER, NOT_NOW, Some(cancel_take_over_clicked));
                } else {
                    button(TAKE_OVER, &take_over, Some(take_over_clicked));
                }
            }
            AudioView::Managed {
                help,
                sources,
                empty,
                ..
            } => {
                info(AUDIO_HELP, &help);
                for row in sources {
                    let key = c_text(&format!("{SOURCE_PREFIX}{}", row.uuid));
                    let name = c_text(&row.name);
                    let checkbox = sys::obs_properties_add_bool(props, key.as_ptr(), name.as_ptr());
                    // Without the dialog's own settings there is no real state, and defaults would untick all.
                    if !settings.is_null() {
                        // A default keeps the settings equal to OBS's copy, so closing asks nothing.
                        sys::obs_data_set_default_bool(settings, key.as_ptr(), row.on);
                        if sys::obs_data_has_user_value(settings, key.as_ptr()) {
                            sys::obs_data_set_bool(settings, key.as_ptr(), row.on);
                        }
                        sys::obs_property_set_modified_callback(checkbox, Some(source_toggled));
                    }
                }
                if let Some(empty) = empty {
                    info(AUDIO_EMPTY, &empty);
                }
            }
        }
    }
}

/// A non-empty string setting.
///
/// # Safety
/// `settings` is live.
unsafe fn picked(settings: *mut sys::obs_data_t, key: &CStr) -> Option<String> {
    // SAFETY: the caller's contract; libobs returns a string it owns or "".
    let value = unsafe { text(sys::obs_data_get_string(settings, key.as_ptr())) };
    (!value.is_empty()).then_some(value)
}

extern "C" fn guild_modified(
    _props: *mut sys::obs_properties_t,
    _property: *mut sys::obs_property_t,
    _settings: *mut sys::obs_data_t,
) -> bool {
    edited();
    // Rebuilds the dialog, so the channel list shows the new server's channels.
    true
}

extern "C" fn start_clicked(
    _props: *mut sys::obs_properties_t,
    _property: *mut sys::obs_property_t,
    _data: *mut c_void,
) -> bool {
    guard("settings start", true, || {
        session::start(session::Wait::Dialog);
        true
    })
}

extern "C" fn stop_clicked(
    _props: *mut sys::obs_properties_t,
    _property: *mut sys::obs_property_t,
    _data: *mut c_void,
) -> bool {
    guard("settings stop", true, || {
        session::stop(session::Wait::Dialog);
        true
    })
}

/// # Safety
/// `props` is live; libobs copies every string.
unsafe fn add_list(
    props: *mut sys::obs_properties_t,
    key: &CStr,
    title: &CStr,
    none: &CStr,
    items: &[(String, String)],
    selected: Option<&str>,
) -> *mut sys::obs_property_t {
    // SAFETY: the caller's contract.
    unsafe {
        let list = sys::obs_properties_add_list(
            props,
            key.as_ptr(),
            title.as_ptr(),
            sys::OBS_COMBO_TYPE_LIST,
            sys::OBS_COMBO_FORMAT_STRING,
        );
        sys::obs_property_list_add_string(list, none.as_ptr(), c"".as_ptr());
        let add = |id: &str, name: &str| {
            let (id, name) = (c_text(id), c_text(name));
            sys::obs_property_list_add_string(list, name.as_ptr(), id.as_ptr());
        };
        for (id, name) in items {
            add(id, name);
        }
        // Keeps a saved choice visible while the service can't list it.
        if let Some(id) = selected.filter(|id| items.iter().all(|(i, _)| i != id)) {
            add(id, id);
        }
        list
    }
}

extern "C" fn get_properties(data: *mut c_void) -> *mut sys::obs_properties_t {
    guard("settings get_properties", ptr::null_mut(), || {
        let source: *mut sys::obs_source_t = data.cast();
        if !source.is_null() && take_successor(source) {
            activate(source);
        }
        // SAFETY: `data` is the source `create` returned.
        let owned = unsafe { SourceSettings::of(source) };
        let settings = owned.0;
        // SAFETY: OBS builds a dialog's view on the UI thread, from the settings it shows.
        unsafe { write_changed(source, settings) };
        RELOADING.with(|r| r.set(true));
        if cfg!(unix) && !source.is_null() {
            recheck_unrestored();
        }
        let selected = (!settings.is_null())
            .then(|| selected_track(settings))
            .flatten();
        let title = c_text(AUDIO_TRACK_TITLE);
        let (guilds, channels, picked, state) = with_controls(|c| {
            let named = |list: &[dstreamy_obs_producer::control::Named]| {
                list.iter()
                    .map(|n| (n.id.clone(), n.name.clone()))
                    .collect::<Vec<_>>()
            };
            (
                named(c.model.guilds()),
                named(c.model.channels()),
                c.model.settings().clone(),
                c.model.state().clone(),
            )
        });
        // SAFETY: the caller owns and destroys the returned properties; libobs copies every string.
        unsafe {
            let owned_props = Properties(sys::obs_properties_create());
            let props = owned_props.0;
            let status_prop = sys::obs_properties_add_text(
                props,
                STATUS.as_ptr(),
                c"".as_ptr(),
                sys::OBS_TEXT_INFO,
            );
            let shown = status(selected);
            let blocked = shown.2;
            crate::hooks::dialog_built(&shown.0, guilds.len());
            set_status(status_prop, shown);
            let guild = add_list(
                props,
                GUILD,
                c"Discord server",
                c"Pick a server",
                &guilds,
                picked.guild_id.as_deref(),
            );
            sys::obs_property_set_modified_callback(guild, Some(guild_modified));
            let channel = add_list(
                props,
                CHANNEL,
                c"Voice channel",
                c"Pick a voice channel",
                &channels,
                picked.channel_id.as_deref(),
            );
            sys::obs_property_set_modified_callback(channel, Some(track_modified));
            let tracks = sys::obs_properties_add_list(
                props,
                AUDIO_TRACK.as_ptr(),
                title.as_ptr(),
                sys::OBS_COMBO_TYPE_LIST,
                sys::OBS_COMBO_FORMAT_INT,
            );
            for track in AUDIO_TRACKS {
                let name = c_text(&track_name(track));
                sys::obs_property_list_add_int(tracks, name.as_ptr(), i64::from(track));
            }
            sys::obs_property_set_modified_callback(tracks, Some(track_modified));
            add_audio(
                props,
                settings,
                shown_track(selected.unwrap_or(picked.audio_track)),
            );
            drop(owned);
            let buttons: [(&CStr, &CStr, sys::obs_property_clicked_t, bool); 2] = [
                (
                    c"start",
                    c"Start",
                    Some(start_clicked),
                    cfg!(unix) && state == DockState::Idle && !blocked,
                ),
                (
                    c"stop",
                    c"Stop",
                    Some(stop_clicked),
                    cfg!(unix) && matches!(state, DockState::Live { .. }),
                ),
            ];
            for (name, label, clicked, enabled) in buttons {
                let button =
                    sys::obs_properties_add_button(props, name.as_ptr(), label.as_ptr(), clicked);
                sys::obs_property_set_enabled(button, enabled);
            }
            owned_props.into_raw()
        }
    })
}

pub fn register() {
    let info = sys::obs_source_info {
        id: SOURCE_ID.as_ptr(),
        type_: sys::OBS_SOURCE_TYPE_INPUT,
        // No video or audio flag: it captures nothing, and update runs at once on the calling thread.
        output_flags: sys::OBS_SOURCE_CAP_DISABLED,
        get_name: Some(get_name),
        create: Some(create),
        destroy: Some(destroy),
        get_defaults: Some(get_defaults),
        get_properties: Some(get_properties),
        update: Some(update),
        ..Default::default()
    };
    // SAFETY: libobs copies `info` (`obs-module.c:956-1052`); the strings it points to are 'static.
    unsafe { sys::obs_register_source_s(&info, mem::size_of::<sys::obs_source_info>()) };
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::Once;

    use super::*;
    use crate::ui::tests::{PLAIN, Source, libobs, reset};

    fn registered() {
        libobs();
        static REGISTERED: Once = Once::new();
        REGISTERED.call_once(register);
    }

    fn forget_dialogs() {
        OPENED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        REVERTING
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        SUCCESSOR
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        CONFIRMING.store(0, Ordering::Release);
        let old = DIALOG.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(old) = old {
            // SAFETY: the weak reference this module took.
            unsafe { sys::obs_weak_source_release(old.0) };
        }
    }

    /// A settings source as Tools > D-Streamy creates it, released like OBS's dialog drops it.
    struct Dialog(*mut sys::obs_source_t);

    impl Dialog {
        fn create() -> Self {
            // SAFETY: the type is registered; libobs runs `create` before returning.
            let source = unsafe {
                sys::obs_source_create_private(
                    SOURCE_ID.as_ptr(),
                    SOURCE_NAME.as_ptr(),
                    ptr::null_mut(),
                )
            };
            assert!(!source.is_null());
            Self(source)
        }

        fn settings(&self) -> SourceSettings {
            // SAFETY: a live source.
            unsafe { SourceSettings::of(self.0) }
        }

        /// What OBS's properties dialog does while it is built and on Cancel or Discard: a copy, then the view.
        fn copy(&self) -> *mut sys::obs_data_t {
            // SAFETY: a fresh object and the source's live settings.
            unsafe {
                let copy = sys::obs_data_create();
                sys::obs_data_apply(copy, self.settings().0);
                copy
            }
        }

        fn build(&self) {
            // SAFETY: a live source; the properties are destroyed at once.
            unsafe { sys::obs_properties_destroy(sys::obs_source_properties(self.0)) };
        }

        fn changed(&self) -> bool {
            // SAFETY: the source's live settings.
            unsafe { sys::obs_data_has_user_value(self.settings().0, CHANGED.as_ptr()) }
        }

        fn audio_track(&self) -> i64 {
            // SAFETY: as above.
            unsafe { sys::obs_data_get_int(self.settings().0, AUDIO_TRACK.as_ptr()) }
        }

        fn is_the_dialog(&self) -> bool {
            DialogSource::get().is_some_and(|d| d.0 == self.0)
        }

        /// Ticks the checkbox of `source` as OBS's view does: the value, then its modified callback.
        fn tick(&self, source: &Source, on: bool) {
            let key = c_text(&format!(
                "{SOURCE_PREFIX}{}",
                source.uuid().to_str().unwrap()
            ));
            // SAFETY: live properties and settings; the properties are destroyed after the callback.
            unsafe {
                let props = sys::obs_source_properties(self.0);
                let checkbox = sys::obs_properties_get(props, key.as_ptr());
                assert!(!checkbox.is_null(), "no checkbox for the source");
                let settings = self.settings();
                sys::obs_data_set_bool(settings.0, key.as_ptr(), on);
                source_toggled(props, checkbox, settings.0);
                sys::obs_properties_destroy(props);
            }
        }

        fn take_over(&self) {
            self.build();
            take_over_clicked(ptr::null_mut(), ptr::null_mut(), self.0.cast());
            confirm_take_over_clicked(ptr::null_mut(), ptr::null_mut(), self.0.cast());
        }
    }

    impl Drop for Dialog {
        fn drop(&mut self) {
            // SAFETY: the reference taken in `create`; libobs destroys the source on its destruction thread.
            unsafe {
                sys::obs_source_release(self.0);
                while sys::obs_wait_for_destroy_queue() {}
            }
        }
    }

    fn start() -> std::sync::MutexGuard<'static, ()> {
        let serial = crate::serial();
        registered();
        reset();
        forget_dialogs();
        change(|c, _| {
            c.model.select_audio_track(6);
        });
        serial
    }

    #[test]
    fn the_close_question_follows_the_dialog_s_mixer_changes_whoever_undoes_them() {
        let _serial = start();
        change(|c, _| c.model.take_over(6));
        let desktop = Source::new(PLAIN, c"Desktop");
        let dialog = Dialog::create();
        dialog.build();
        assert!(!dialog.changed());
        dialog.tick(&desktop, false);
        assert!(!desktop.on(6));
        assert!(dialog.changed(), "a tick that stands makes X ask");

        assert!(crate::audio::set_source(
            desktop.uuid().to_str().unwrap(),
            6,
            true
        ));
        mixers_changed();
        assert!(
            !dialog.changed(),
            "the panel put the tick back, so X closes quietly"
        );

        dialog.tick(&desktop, false);
        assert!(dialog.changed());
        collection_leaving();
        assert!(
            !dialog.changed(),
            "OBS saves the collection with the change before it closes the dialog"
        );
        assert!(!desktop.on(6), "the change stays");
        dialog.tick(&desktop, true);
        assert!(
            !dialog.changed(),
            "no Cancel could put back a change already saved"
        );

        collection_loaded();
        dialog.tick(&desktop, false);
        assert!(
            dialog.changed(),
            "changes after another collection loaded can be undone again"
        );
        collection_changed();
        assert!(!dialog.changed());
        drop((dialog, desktop));
        reset();
    }

    #[test]
    fn reopening_while_a_dialog_is_open_leaves_it_in_charge_until_obs_closes_it() {
        let _serial = start();
        let desktop = Source::new(PLAIN, c"Desktop");
        let first = Dialog::create();
        let opened = first.copy();
        first.build();
        assert!(first.is_the_dialog());
        first.take_over();
        assert!(!desktop.on(6) && with_controls(|c| c.model.manages(6)));
        // SAFETY: the dialog's live settings, as OBS's view edits them.
        unsafe {
            sys::obs_data_set_int(first.settings().0, AUDIO_TRACK.as_ptr(), 3);
            sys::obs_source_update(first.0, first.settings().0);
        }
        assert_eq!(with_controls(|c| c.model.settings().audio_track), 3);
        CONFIRMING.store(3, Ordering::Release);

        let refused = opening(|| {
            let second = Dialog::create();
            assert_eq!(successor(), Some(second.0));
            assert!(first.is_the_dialog());
            second.0
        });
        not_opened(refused);
        assert!(
            first.is_the_dialog(),
            "the close was cancelled, so the open dialog keeps refreshing"
        );
        assert_eq!(
            CONFIRMING.load(Ordering::Acquire),
            3,
            "and keeps its confirm row"
        );
        assert_eq!(successor(), None);

        let second = opening(|| {
            let second = Dialog::create();
            assert_eq!(second.audio_track(), 3);
            // SAFETY: OBS's Discard: an update with the copy taken when the dialog opened.
            unsafe { sys::obs_source_update(first.0, opened) };
            assert!(
                desktop.on(6),
                "the Discard put the track back before the new dialog looked"
            );
            assert!(!with_controls(|c| c.model.manages(6)));
            assert_eq!(
                second.audio_track(),
                6,
                "the new dialog opens with the picks put back"
            );
            second
        });
        let old = first.0;
        drop(first);
        for _ in 0..200 {
            if !dialog_open() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            !dialog_open(),
            "OBS deleted the closed dialog before the new view's queued build"
        );
        second.build();
        assert!(
            second.is_the_dialog(),
            "the new dialog takes over at its first build"
        );
        assert!(
            OPENED
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .all(|o| o.source != old)
        );
        let snapshot = OPENED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|o| o.source == second.0)
            .map(|o| (o.choice.audio_track, o.touched));
        assert_eq!(snapshot, Some((6, 0)));
        assert_eq!(CONFIRMING.load(Ordering::Acquire), 0);
        second.take_over();
        assert!(second.changed(), "the new dialog tracks its own changes");
        // SAFETY: the copy made above.
        unsafe { sys::obs_data_release(opened) };
        drop((second, desktop));
        reset();
    }

    #[test]
    fn a_reopened_dialog_built_before_the_old_one_is_destroyed_stays_in_charge() {
        let _serial = start();
        let first = Dialog::create();
        first.build();
        let second = opening(Dialog::create);
        second.build();
        assert!(second.is_the_dialog());
        drop(first);
        assert!(
            second.is_the_dialog(),
            "the old dialog's destroy leaves the new one watched"
        );
        drop(second);
        reset();
    }

    #[test]
    fn cancel_manages_the_opened_pick_again_after_the_panel_let_it_go() {
        let _serial = start();
        change(|c, _| c.model.take_over(6));
        let dialog = Dialog::create();
        let opened = dialog.copy();
        dialog.build();
        // SAFETY: the dialog's live settings, as OBS's view edits them.
        unsafe {
            sys::obs_data_set_int(dialog.settings().0, AUDIO_TRACK.as_ptr(), 3);
            sys::obs_source_update(dialog.0, dialog.settings().0);
        }
        assert!(
            crate::audio::release(6),
            "the panel offers Track 6 once the pick moved"
        );
        // SAFETY: OBS's Cancel: clear, then update with the copy.
        unsafe {
            sys::obs_data_clear(dialog.settings().0);
            sys::obs_source_update(dialog.0, opened);
        }
        settle();
        assert_eq!(with_controls(|c| c.model.settings().audio_track), 6);
        assert!(
            with_controls(|c| c.model.manages(6)),
            "the pick put back is managed again"
        );

        // SAFETY: as above.
        unsafe {
            sys::obs_data_set_int(dialog.settings().0, AUDIO_TRACK.as_ptr(), 3);
            sys::obs_source_update(dialog.0, dialog.settings().0);
        }
        assert!(crate::audio::release(6));
        collection_leaving();
        assert!(
            !with_controls(|c| c.model.manages(6)),
            "a close OBS starts keeps the release, like every other mixer change it saves"
        );
        // SAFETY: OBS's Discard after its own save: an update with the copy.
        unsafe { sys::obs_source_update(dialog.0, opened) };
        settle();
        let view = with_controls(|c| {
            dstreamy_obs_producer::mixer::audio_view(&c.model, c.model.settings().audio_track, &[])
        });
        assert!(
            matches!(view, AudioView::Unmanaged { track: 6, ref take_over, .. } if take_over == "Manage Track 6"),
            "the pick put back shows as unmanaged, with its takeover: {view:?}"
        );
        // SAFETY: the copy made above.
        unsafe { sys::obs_data_release(opened) };
        drop(dialog);
        reset();
    }

    #[test]
    fn a_cancel_without_a_settle_is_reported_only_once_the_dialog_closes_without_an_edit() {
        let _serial = start();
        change(|c, _| c.model.take_over(6));
        let desktop = Source::new(PLAIN, c"Desktop");
        let arm = |dialog: &Dialog| {
            let o = OPENED.lock().unwrap_or_else(PoisonError::into_inner);
            let o = o.iter().find(|o| o.source == dialog.0).unwrap();
            *REVERTING.lock().unwrap_or_else(PoisonError::into_inner) = Some(Reverting {
                source: dialog.0,
                pick: 6,
                mixers: o.mixers.clone(),
                managed: o.managed,
                touched: track_bit(6),
                unsettled: true,
            });
        };
        let edited_back = Dialog::create();
        edited_back.build();
        edited_back.tick(&desktop, false);
        arm(&edited_back);
        edited();
        drop(edited_back);
        assert_eq!(
            with_controls(|c| c.model.unrestored_mixers()),
            0,
            "an edit back, not a Cancel"
        );

        let cancelled = Dialog::create();
        cancelled.build();
        arm(&cancelled);
        drop(cancelled);
        assert_eq!(with_controls(|c| c.model.unrestored_mixers()), track_bit(6));
        assert!(crate::audio::set_source(
            desktop.uuid().to_str().unwrap(),
            6,
            true
        ));
        recheck_unrestored();
        assert_eq!(
            with_controls(|c| c.model.unrestored_mixers()),
            0,
            "put right by hand"
        );
        drop(desktop);
        reset();
    }
}
