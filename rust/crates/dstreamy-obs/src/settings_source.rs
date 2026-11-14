//! `dstreamy_settings`: a private source whose stock properties dialog is D-Streamy's settings window.

use std::cell::Cell;
use std::ffi::{CStr, c_char, c_void};
use std::mem;
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use dstreamy_obs_producer::mixer::{
    AudioView, CONFIRM, NOT_NOW, audio_view, instead_label, mic_guard, notices, release_label,
    track_bit,
};
use dstreamy_obs_producer::model::{
    AUDIO_TRACK_TITLE, AUDIO_TRACKS, Choice, DEFAULT_AUDIO_TRACK, PanelSettings, PanelState,
    TrackStatus, track_name,
};
use dstreamy_obs_producer::panel::{
    AccountRow, CANCEL_LOGIN, NOT_AVAILABLE, account_row, stream_confirm,
};
use dstreamy_obs_producer::panel_config::{ConfigError, ConfigState};
use dstreamy_obs_producer::state_view::Action;
use dstreamy_obs_sys as sys;

use crate::config;
use crate::guard::{guard, lock};
use crate::log;
use crate::output::c_text;
use crate::output::text;
use crate::session;

pub const SOURCE_ID: &CStr = c"dstreamy_settings";
pub const SOURCE_NAME: &CStr = c"D-Streamy";

const AUDIO_TRACK: &CStr = c"audio_track";
const GUILD: &CStr = c"guild_id";
const CHANNEL: &CStr = c"channel_id";
const STATUS: &CStr = c"status";
const LOGIN: &CStr = c"login";

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
    let mut guard = lock(&CONTROLS);
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

fn observe_config_error(error: &ConfigError, track: u8) {
    crate::hooks::observe("config_error", || {
        let (kind, path) = match error {
            ConfigError::NoFolder => ("no_folder", None),
            ConfigError::Corrupt { path, .. } => ("corrupt", Some(path)),
            ConfigError::Unreadable { path, .. } => ("unreadable", Some(path)),
            ConfigError::Symlink { path } => ("symlink", Some(path)),
            ConfigError::Failed(_) => ("write", None),
        };
        serde_json::json!({"kind":kind,"path":path,"track":track})
    });
}

/// Writes save `generation` unless a newer one already reached the disk; only the newest save reports its error.
fn flush((generation, json): (u64, String)) {
    let mut written = lock(&DISK);
    let result = (generation > *written).then(|| config::save(&json));
    *written = (*written).max(generation);
    controls(|c| {
        c.flushed = c.flushed.max(generation);
        if let Some(Err(e)) = result {
            observe_config_error(&e, c.state.model.settings().audio_track);
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
        let _disk = lock(&DISK);
        let read = config::load();
        controls(|c| {
            if c.issued != c.flushed {
                return None;
            }
            let mut pending = Pending::default();
            let before = c.state.error().cloned();
            c.state.open(|| read, pending.write());
            if let Some(e) = c.state.error().filter(|e| Some(*e) != before.as_ref()) {
                observe_config_error(e, c.state.model.settings().audio_track);
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
        observe_config_error(e, c.model.settings().audio_track);
        log::warn(&e.message(c.model.settings().audio_track));
    }
}

struct WeakSource(*mut sys::obs_weak_source_t);

// SAFETY: weak source references are reference-counted by libobs and usable from any thread.
unsafe impl Send for WeakSource {}

impl WeakSource {
    fn references(&self, source: *mut sys::obs_source_t) -> bool {
        // SAFETY: the stored weak reference and callback source are live.
        unsafe { sys::obs_weak_source_references_source(self.0, source) }
    }

    fn upgrade(&self) -> Option<DialogSource> {
        // SAFETY: the weak reference is live and returns a strong reference or NULL.
        let source = unsafe { sys::obs_weak_source_get_source(self.0) };
        (!source.is_null()).then_some(DialogSource(source))
    }
}

impl Drop for WeakSource {
    fn drop(&mut self) {
        // SAFETY: the weak reference this module took.
        unsafe { sys::obs_weak_source_release(self.0) };
    }
}

/// The open dialog's source, so the control thread can ask OBS to rebuild it.
static DIALOG: Mutex<Option<WeakSource>> = Mutex::new(None);

fn watch_dialog(source: *mut sys::obs_source_t) {
    // SAFETY: source is being created; WeakSource releases the reference.
    let weak = WeakSource(unsafe { sys::obs_source_get_weak_source(source) });
    let _old = lock(&DIALOG).replace(weak);
}

/// False when `source` isn't the dialog [`refresh`] rebuilds.
fn forget_dialog(source: *mut sys::obs_source_t) -> bool {
    let mut dialog = lock(&DIALOG);
    let ours = dialog.as_ref().is_some_and(|d| d.references(source));
    if ours {
        dialog.take();
    }
    ours
}

/// The open dialog's source, as a strong reference released on drop.
struct DialogSource(*mut sys::obs_source_t);

impl DialogSource {
    fn get() -> Option<Self> {
        lock(&DIALOG).as_ref().and_then(WeakSource::upgrade)
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
static SUCCESSOR: Mutex<Option<WeakSource>> = Mutex::new(None);

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
    let mut successor = lock(&SUCCESSOR);
    let ours = successor.as_ref().is_some_and(|s| s.references(source));
    if ours {
        successor.take();
    }
    ours
}

fn successor() -> Option<DialogSource> {
    lock(&SUCCESSOR).as_ref().and_then(WeakSource::upgrade)
}

/// `source` becomes the dialog that refreshes, confirms and undoes, with what it shows now.
fn activate(source: *mut sys::obs_source_t) {
    watch_dialog(source);
    let (choice, managed) = with_controls(|c| (c.model.choice(), c.model.managed_mixers()));
    let mixers = Some(crate::audio::mixers());
    CONFIRMING.store(0, Ordering::Release);
    STREAM_CONFIRM.store(0, Ordering::Release);
    lock(&OPENED).push(Opened {
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
    lock(&DIALOG).is_some()
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

/// The status line for a dialog showing `selected`, how OBS should colour it, and whether a mic or a login holds Start back.
fn status(selected: Option<u8>) -> (String, sys::obs_text_info_type, bool) {
    let sources = crate::audio::sources();
    with_controls(|c| {
        let mut preview = c.model.clone();
        if let Some(track) = selected {
            preview.select_audio_track(track);
        }
        let idle = preview.state() == &PanelState::Idle;
        let live = matches!(preview.state(), PanelState::Live { .. });
        let mic = (idle || live)
            .then(|| mic_guard(&preview, &sources))
            .flatten();
        let held = idle && (mic.is_some() || preview.logging_in() || preview.account_changing());
        let login = preview.logging_in() || preview.login_offer().is_some();
        let quiet = idle && preview.error().is_none() && mic.is_none();
        let session = match &mic {
            _ if let Some(problem) = preview.output_problem() => problem.message.clone(),
            Some(guard) if !login => match preview.unended_start() {
                Some(problem) => format!("{} {}", guard.message(), guard.beside(&problem.message)),
                None => guard.message(),
            },
            _ => preview.status_text(),
        };
        if let Some(e) = c.error() {
            let config = e.message(preview.settings().audio_track);
            let text = if quiet {
                config
            } else {
                format!("{}. {config}", session.trim_end_matches('.'))
            };
            return (text, sys::OBS_TEXT_INFO_ERROR, held);
        }
        let kind = match (preview.track_status(), preview.error()) {
            _ if preview.logging_in() => sys::OBS_TEXT_INFO_NORMAL,
            _ if preview.output_problem().is_some() => sys::OBS_TEXT_INFO_ERROR,
            _ if mic.is_some() && !login => sys::OBS_TEXT_INFO_WARNING,
            (TrackStatus::Pending { .. }, _) => sys::OBS_TEXT_INFO_WARNING,
            (TrackStatus::Idle { .. }, Some(_)) => sys::OBS_TEXT_INFO_ERROR,
            _ => sys::OBS_TEXT_INFO_NORMAL,
        };
        (session, kind, held)
    })
}

extern "C" fn get_name(_type_data: *mut c_void) -> *const c_char {
    SOURCE_NAME.as_ptr()
}

extern "C" fn create(
    settings: *mut sys::obs_data_t,
    source: *mut sys::obs_source_t,
) -> *mut c_void {
    guard("settings create", ptr::null_mut(), || {
        open_config();
        let guild = with_controls(|c| c.model.settings().guild_id.clone());
        session::load_lists(guild);
        if OPENING.load(Ordering::Acquire) && dialog_open() {
            // SAFETY: source is the new source libobs is creating; WeakSource releases the weak reference.
            let weak = unsafe { sys::obs_source_get_weak_source(source) };
            *lock(&SUCCESSOR) = Some(WeakSource(weak));
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
        lock(&OPENED).retain(|o| o.source != source);
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
    for o in lock(&OPENED).iter_mut() {
        o.touched |= track_bit(track);
    }
    if let Some(dialog) = DialogSource::get() {
        // SAFETY: the caller passes the dialog's live settings or NULL.
        unsafe { write_changed(dialog.0, settings) };
    }
}

/// Whether a mixer change the dialog of `source` made still stands.
fn changed(source: *mut sys::obs_source_t) -> bool {
    let opened = lock(&OPENED)
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
    for o in lock(&OPENED).iter_mut() {
        o.mixers = None;
        o.touched = 0;
    }
    lock(&REVERTING).take();
    sync_changed();
}

/// OBS leaves the collection and closes the dialog: its mixer changes stay, and closing asks only about picks.
pub fn collection_leaving() {
    let kept = lock(&OPENED).iter().any(|o| o.touched != 0);
    keep_mixer_changes();
    if kept {
        crate::hooks::observe("dialog_mixer_changes_kept", || serde_json::json!({}));
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
    if lock(&OPENED).is_empty() {
        return;
    }
    let (mixers, managed) = (
        Some(crate::audio::mixers()),
        with_controls(|c| c.model.managed_mixers()),
    );
    for o in lock(&OPENED).iter_mut() {
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
        lock(&REVERTING).take();
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
    let pending = lock(&OPENED)
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
    *lock(&REVERTING) = pending;
    let next = successor().filter(|_| OPENING.load(Ordering::Acquire));
    if let Some(next) = next {
        settle_pending();
        // SAFETY: next holds a strong reference; SourceSettings releases its settings reference after the write.
        unsafe { write_picks(SourceSettings::of(next.0).0) };
    } else if !crate::audio::request_settle()
        && let Some(r) = lock(&REVERTING).as_mut()
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
        let mut reverting = lock(&REVERTING);
        let ours = reverting
            .as_ref()
            .is_some_and(|r| r.source == source && r.unsettled);
        ours.then(|| reverting.take()).flatten()
    };
    let mut unrestored = lock(&UNRESTORED);
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
    let snapshot = lock(&UNRESTORED)
        .as_ref()
        .map(|u| (u.mixers.clone(), u.touched, u.managed));
    if let Some((mixers, touched, managed)) = snapshot
        && !crate::audio::changed_since(&mixers, touched, managed)
    {
        forget_unrestored();
    }
}

pub fn forget_unrestored() {
    lock(&UNRESTORED).take();
    change(|c, _| c.model.forget_unrestored());
}

/// On the UI thread once the call that armed it returned: a Cancel puts back the Discord tracks the dialog changed.
pub fn settle() {
    settle_pending();
    crate::hooks::settings_settled();
    log::debug("settled the settings dialog's pending Cancel");
}

fn settle_pending() {
    let Some(pending) = lock(&REVERTING).take() else {
        return;
    };
    let current = lock(&OPENED)
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
        for o in lock(&OPENED)
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
        let opened = lock(&OPENED)
            .iter()
            .find(|o| o.source == source)
            .map(|o| o.choice.clone());
        // SAFETY: as above; Defaults clears every user value, while Cancel and Discard put the opened ones back.
        let defaults = !unsafe { sys::obs_data_has_user_value(settings, AUDIO_TRACK.as_ptr()) };
        if defaults {
            lock(&REVERTING).take();
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
                crate::hooks::observe("audio_track_refused", || serde_json::json!({"track":raw}));
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
                    // Only the dialog's settings hold state; the default matches OBS's copy, so closing asks nothing.
                    if !settings.is_null() {
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
    guard("settings guild_modified", false, || {
        edited();
        true
    })
}

/// The action whose confirm the dialog shows in place of Start and Stop; 0 for none.
static STREAM_CONFIRM: AtomicU8 = AtomicU8::new(0);
const CONFIRM_START: u8 = 1;
const CONFIRM_STOP: u8 = 2;
const STREAM_QUESTION: &CStr = c"stream_question";
const CONFIRM_STREAM: &CStr = c"confirm_stream";
const CANCEL_STREAM: &CStr = c"cancel_stream";

extern "C" fn start_clicked(
    _props: *mut sys::obs_properties_t,
    _property: *mut sys::obs_property_t,
    _data: *mut c_void,
) -> bool {
    guard("settings start", true, || {
        STREAM_CONFIRM.store(CONFIRM_START, Ordering::Release);
        true
    })
}

extern "C" fn confirm_stream_clicked(
    _props: *mut sys::obs_properties_t,
    _property: *mut sys::obs_property_t,
    _data: *mut c_void,
) -> bool {
    guard("settings confirm_stream", true, || {
        match STREAM_CONFIRM.swap(0, Ordering::AcqRel) {
            CONFIRM_START => session::start(),
            CONFIRM_STOP => session::stop(),
            _ => {}
        }
        true
    })
}

extern "C" fn cancel_stream_clicked(
    _props: *mut sys::obs_properties_t,
    _property: *mut sys::obs_property_t,
    _data: *mut c_void,
) -> bool {
    guard("settings cancel_stream", true, || {
        STREAM_CONFIRM.store(0, Ordering::Release);
        true
    })
}

extern "C" fn login_clicked(
    _props: *mut sys::obs_properties_t,
    _property: *mut sys::obs_property_t,
    _data: *mut c_void,
) -> bool {
    guard("settings login", true, || {
        session::login();
        true
    })
}

type LoginClicked =
    unsafe extern "C" fn(*mut sys::obs_properties_t, *mut sys::obs_property_t, *mut c_void) -> bool;

extern "C" fn cancel_login_clicked(
    _props: *mut sys::obs_properties_t,
    _property: *mut sys::obs_property_t,
    _data: *mut c_void,
) -> bool {
    guard("settings cancel_login", true, || {
        session::cancel_login();
        true
    })
}

const ACCOUNT: &CStr = c"account";
const ACCOUNT_USE_PREFIX: &str = "account_use_";
const ADD_ACCOUNT: &CStr = c"add_account";
const LOGOUT_ACCOUNT: &CStr = c"logout_account";
const LOGOUT_QUESTION: &CStr = c"logout_question";
const CONFIRM_LOGOUT: &CStr = c"confirm_logout";
const CANCEL_LOGOUT: &CStr = c"cancel_logout";

/// The account a dialog button names after `prefix`.
fn button_account(property: *mut sys::obs_property_t, prefix: &str) -> Option<String> {
    // SAFETY: the view passes a live property; libobs owns the name.
    let name = unsafe { text(sys::obs_property_name(property)) };
    name.strip_prefix(prefix)
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
}

extern "C" fn use_account_clicked(
    _props: *mut sys::obs_properties_t,
    property: *mut sys::obs_property_t,
    _data: *mut c_void,
) -> bool {
    guard("settings use_account", true, || {
        if let Some(name) = button_account(property, ACCOUNT_USE_PREFIX) {
            session::select_account(&name);
        }
        true
    })
}

extern "C" fn add_account_clicked(
    _props: *mut sys::obs_properties_t,
    _property: *mut sys::obs_property_t,
    _data: *mut c_void,
) -> bool {
    guard("settings add_account", true, || {
        session::add_account();
        true
    })
}

extern "C" fn logout_account_clicked(
    _props: *mut sys::obs_properties_t,
    _property: *mut sys::obs_property_t,
    _data: *mut c_void,
) -> bool {
    guard("settings logout_account", true, || {
        let active = with_controls(|c| c.model.accounts().and_then(|a| a.active.clone()));
        if let Some(name) = active {
            change(|c, _| c.model.request_logout(&name));
        }
        true
    })
}

extern "C" fn confirm_logout_clicked(
    _props: *mut sys::obs_properties_t,
    _property: *mut sys::obs_property_t,
    _data: *mut c_void,
) -> bool {
    guard("settings confirm_logout", true, || {
        if let Some(name) = change(|c, _| c.model.confirm_logout(true)) {
            session::logout_account(&name);
        }
        true
    })
}

extern "C" fn cancel_logout_clicked(
    _props: *mut sys::obs_properties_t,
    _property: *mut sys::obs_property_t,
    _data: *mut c_void,
) -> bool {
    guard("settings cancel_logout", true, || {
        change(|c, _| c.model.confirm_logout(false));
        true
    })
}

/// The dialog's account row: the account in use, a button per other account, Add account… and Log out.
///
/// # Safety
/// `props` is live; libobs copies every string.
unsafe fn add_account_row(props: *mut sys::obs_properties_t, row: &AccountRow) {
    let active = row.items.iter().find(|i| i.selected);
    let line = match active {
        Some(a) if !a.user.is_empty() => format!("{}: {} ({})", row.label, a.name, a.user),
        Some(a) => format!("{}: {}", row.label, a.name),
        None => format!("{}: none picked", row.label),
    };
    // SAFETY: the caller's contract.
    unsafe {
        let line = c_text(&line);
        sys::obs_properties_add_text(props, ACCOUNT.as_ptr(), line.as_ptr(), sys::OBS_TEXT_INFO);
        let button = |key: &CStr, label: &str, clicked: sys::obs_property_clicked_t, tip: &str| {
            let label = c_text(label);
            let b = sys::obs_properties_add_button(props, key.as_ptr(), label.as_ptr(), clicked);
            sys::obs_property_set_enabled(b, row.enabled);
            if !tip.is_empty() {
                let tip = c_text(tip);
                sys::obs_property_set_long_description(b, tip.as_ptr());
            }
        };
        for item in row.items.iter().filter(|i| !i.selected) {
            let key = c_text(&format!("{ACCOUNT_USE_PREFIX}{}", item.name));
            button(
                &key,
                &format!("Use {}", item.name),
                Some(use_account_clicked),
                row.tooltip,
            );
        }
        button(ADD_ACCOUNT, row.add, Some(add_account_clicked), row.tooltip);
        if let Some(confirm) = &row.confirm {
            let text = c_text(&confirm.text);
            let note = sys::obs_properties_add_text(
                props,
                LOGOUT_QUESTION.as_ptr(),
                text.as_ptr(),
                sys::OBS_TEXT_INFO,
            );
            sys::obs_property_text_set_info_type(note, sys::OBS_TEXT_INFO_WARNING);
            button(
                CONFIRM_LOGOUT,
                &confirm.ok,
                Some(confirm_logout_clicked),
                "",
            );
            button(
                CANCEL_LOGOUT,
                &confirm.cancel,
                Some(cancel_logout_clicked),
                "",
            );
        } else if active.is_some() {
            button(
                LOGOUT_ACCOUNT,
                row.logout,
                Some(logout_account_clicked),
                row.logout_tooltip,
            );
        }
    }
}

extern "C" fn stop_clicked(
    _props: *mut sys::obs_properties_t,
    _property: *mut sys::obs_property_t,
    _data: *mut c_void,
) -> bool {
    guard("settings stop", true, || {
        STREAM_CONFIRM.store(CONFIRM_STOP, Ordering::Release);
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
    unavailable: bool,
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
            add(id, if unavailable { NOT_AVAILABLE } else { id });
        }
        list
    }
}

/// What the dialog's lists and buttons show, read under one model lock.
struct DialogSnapshot {
    guilds: Vec<(String, String)>,
    channels: Vec<(String, String)>,
    picked: PanelSettings,
    state: PanelState,
    login: Option<(&'static str, Option<LoginClicked>)>,
    accounts: Option<AccountRow>,
    guild_gone: bool,
    channel_gone: bool,
}

impl DialogSnapshot {
    fn take() -> Self {
        with_controls(|c| {
            let named = |list: &[dstreamy_obs_producer::control::Named]| {
                list.iter()
                    .map(|n| (n.id.clone(), n.name.clone()))
                    .collect::<Vec<_>>()
            };
            Self {
                guilds: named(c.model.guilds()),
                channels: named(c.model.channels()),
                picked: c.model.settings().clone(),
                state: c.model.state().clone(),
                login: c
                    .model
                    .login_offer()
                    .map(|offer| (offer.label(), Some(login_clicked as LoginClicked)))
                    .or_else(|| {
                        c.model
                            .logging_in()
                            .then_some((CANCEL_LOGIN, Some(cancel_login_clicked as LoginClicked)))
                    }),
                accounts: account_row(&c.model),
                guild_gone: c.model.guild_unavailable(),
                channel_gone: c.model.channel_unavailable(),
            }
        })
    }
}

/// # Safety
/// `props` is live; libobs copies every string.
unsafe fn add_track_list(props: *mut sys::obs_properties_t) {
    let title = c_text(AUDIO_TRACK_TITLE);
    // SAFETY: the caller's contract.
    unsafe {
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
    }
}

/// Start and Stop, or the open confirm's question and answers in their place.
///
/// # Safety
/// `props` is live; libobs copies every string.
unsafe fn add_session_buttons(props: *mut sys::obs_properties_t, can_start: bool, can_stop: bool) {
    let confirming = match STREAM_CONFIRM.load(Ordering::Acquire) {
        CONFIRM_START if can_start => Some(Action::Start),
        CONFIRM_STOP if can_stop => Some(Action::Stop),
        _ => {
            STREAM_CONFIRM.store(0, Ordering::Release);
            None
        }
    };
    // SAFETY: the caller's contract.
    unsafe {
        if let Some(action) = confirming {
            let confirm = with_controls(|c| stream_confirm(&c.model, action));
            let text = c_text(&confirm.text);
            let note = sys::obs_properties_add_text(
                props,
                STREAM_QUESTION.as_ptr(),
                text.as_ptr(),
                sys::OBS_TEXT_INFO,
            );
            sys::obs_property_text_set_info_type(note, sys::OBS_TEXT_INFO_WARNING);
            let answers: [(&CStr, &str, sys::obs_property_clicked_t); 2] = [
                (CONFIRM_STREAM, confirm.ok, Some(confirm_stream_clicked)),
                (CANCEL_STREAM, confirm.cancel, Some(cancel_stream_clicked)),
            ];
            for (name, label, clicked) in answers {
                let label = c_text(label);
                sys::obs_properties_add_button(props, name.as_ptr(), label.as_ptr(), clicked);
            }
        } else {
            let buttons: [(&CStr, &CStr, sys::obs_property_clicked_t, bool); 2] = [
                (c"start", c"Start", Some(start_clicked), can_start),
                (c"stop", c"Stop", Some(stop_clicked), can_stop),
            ];
            for (name, label, clicked, enabled) in buttons {
                let button =
                    sys::obs_properties_add_button(props, name.as_ptr(), label.as_ptr(), clicked);
                sys::obs_property_set_enabled(button, enabled);
            }
        }
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
        if !source.is_null() {
            recheck_unrestored();
        }
        let selected = (!settings.is_null())
            .then(|| selected_track(settings))
            .flatten();
        let DialogSnapshot {
            guilds,
            channels,
            picked,
            state,
            login,
            accounts,
            guild_gone,
            channel_gone,
        } = DialogSnapshot::take();
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
            if with_controls(|c| c.model.loading().0) {
                sys::obs_properties_add_text(
                    props,
                    c"lists_loading".as_ptr(),
                    c"Loading Discord servers and voice channels…".as_ptr(),
                    sys::OBS_TEXT_INFO,
                );
            }
            if let Some((label, clicked)) = login {
                let label = c_text(label);
                sys::obs_properties_add_button(props, LOGIN.as_ptr(), label.as_ptr(), clicked);
            }
            if let Some(row) = &accounts {
                add_account_row(props, row);
            }
            let guild = add_list(
                props,
                GUILD,
                c"Discord server",
                c"Pick a server",
                &guilds,
                picked.guild_id.as_deref(),
                guild_gone,
            );
            sys::obs_property_set_modified_callback(guild, Some(guild_modified));
            let channel = add_list(
                props,
                CHANNEL,
                c"Voice channel",
                c"Pick a voice channel",
                &channels,
                picked.channel_id.as_deref(),
                channel_gone,
            );
            sys::obs_property_set_modified_callback(channel, Some(track_modified));
            add_track_list(props);
            add_audio(
                props,
                settings,
                shown_track(selected.unwrap_or(picked.audio_track)),
            );
            drop(owned);
            let picked_both = [&picked.guild_id, &picked.channel_id]
                .iter()
                .all(|id| id.as_deref().is_some_and(|id| !id.is_empty()));
            let can_start = state == PanelState::Idle
                && picked_both
                && !blocked
                && !guild_gone
                && !channel_gone;
            let can_stop = matches!(state, PanelState::Live { .. })
                || with_controls(|c| c.model.cancellable());
            add_session_buttons(props, can_start, can_stop);
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
        lock(&OPENED).clear();
        lock(&REVERTING).take();
        lock(&SUCCESSOR).take();
        CONFIRMING.store(0, Ordering::Release);
        STREAM_CONFIRM.store(0, Ordering::Release);
        lock(&DIALOG).take();
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
            extern "C" fn queued_after_the_destroy(_: *mut c_void) {}
            // SAFETY: the reference taken in `create`; libobs destroys the source on its destruction thread.
            unsafe {
                sys::obs_source_release(self.0);
                // Without video, obs_wait_for_destroy_queue returns at once; a waited task runs after the destroy.
                sys::obs_queue_task(
                    sys::OBS_TASK_DESTROY,
                    Some(queued_after_the_destroy),
                    ptr::null_mut(),
                    true,
                );
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
    fn stopped_output_cleanup_failure_precedes_mic_warning_until_stop_succeeds() {
        use dstreamy_obs_producer::control::Started;
        use dstreamy_obs_producer::model::Canvas;
        let _serial = start();
        let mic = Source::new(crate::ui::tests::MIC, c"Mic");
        let failure = "The output stopped; the Discord session may still be live; press Stop";
        change(|c, _| {
            c.open(|| Ok(None), |_| Ok(()));
            c.model.select_guild("g");
            c.model.select_channel("c");
            c.model
                .start(Canvas {
                    fps_num: 30,
                    fps_den: 1,
                    output_width: 1280,
                    output_height: 720,
                })
                .unwrap();
            c.model.started(Ok(Started {
                ownership: None,
                channel: "c".into(),
            }));
            c.model.output_ended(Err(failure.into()));
        });
        let shown = status(None);
        assert_eq!(shown.0, failure);
        assert_eq!(shown.1, sys::OBS_TEXT_INFO_ERROR);
        assert!(!shown.2, "Stop remains available");
        change(|c, _| {
            c.model.stop().unwrap();
            c.model.stopped(Ok(()));
        });
        drop(mic);
        assert_eq!(status(None).0, "Idle");
        reset();
    }

    #[test]
    fn a_held_back_start_that_left_its_session_running_is_shown_beside_the_mic() {
        use dstreamy_obs_producer::control::{Candidate, Ownership};
        use dstreamy_obs_producer::model::{Canvas, FIX_MIC_TRACK};
        let _serial = start();
        let mic = Source::new(crate::ui::tests::MIC, c"Mic");
        let still_live =
            "The Discord session it started is still live (timed out); press Start to use it again";
        let guard = crate::audio::mic_guard().expect("the mic is on Track 6");
        change(|c, _| {
            c.open(|| Ok(None), |_| Ok(()));
            c.model.select_guild("g");
            c.model.select_channel("c");
            c.model
                .start(Canvas {
                    fps_num: 30,
                    fps_den: 1,
                    output_width: 1280,
                    output_height: 720,
                })
                .unwrap();
            let owned = Ownership {
                connections: vec![Candidate { id: 7, fps: None }],
                guild_id: "g".into(),
                channel_id: "c".into(),
                instance: None,
            };
            assert!(c.model.claim(Some(owned), &[]).is_some());
            c.model.output_failed(
                format!("{} {still_live}", guard.held_back()),
                FIX_MIC_TRACK,
                false,
            );
        });
        let (shown, kind, held) = status(None);
        assert_eq!(shown, format!("{} {still_live}", guard.message()));
        assert_eq!(kind, sys::OBS_TEXT_INFO_WARNING);
        assert!(held, "the mic still holds Start back");
        change(|c, _| c.model.restore_ownership(None));
        assert_eq!(status(None).0, guard.message(), "nothing left running");
        drop(mic);
        reset();
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
            assert_eq!(successor().map(|s| s.0), Some(second.0));
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
        assert!(successor().is_none());

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
        assert!(lock(&OPENED).iter().all(|o| o.source != old));
        let snapshot = lock(&OPENED)
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
    fn the_dialog_offers_the_login_window_with_its_label_only_when_the_model_does() {
        use dstreamy_obs_producer::control::ControlError;

        let _serial = start();
        let mic = Source::new(crate::ui::tests::MIC, c"Mic");
        let dialog = Dialog::create();
        let shown = || {
            // SAFETY: a live source; the properties are destroyed before returning.
            unsafe {
                let props = sys::obs_source_properties(dialog.0);
                let button = sys::obs_properties_get(props, LOGIN.as_ptr());
                let label =
                    (!button.is_null()).then(|| text(sys::obs_property_description(button)));
                let status = sys::obs_properties_get(props, STATUS.as_ptr());
                let shown = (
                    label,
                    text(sys::obs_property_description(status)),
                    sys::obs_property_text_info_type(status),
                    sys::obs_property_enabled(sys::obs_properties_get(props, c"start".as_ptr())),
                );
                sys::obs_properties_destroy(props);
                shown
            }
        };
        assert_eq!(shown().0, None);
        assert!(shown().1.starts_with("Mic is on Track 6"), "{}", shown().1);
        change(|c, _| {
            c.model.select_guild("g");
            c.model.select_channel("c");
            c.model
                .guilds_loaded(Err(ControlError::NotLoggedIn { window: true }));
        });
        let (label, status, _, start) = shown();
        assert_eq!(label.as_deref(), Some("Log in"));
        assert!(
            status.starts_with("Log in first: press Log in"),
            "the login comes before the mic: {status}"
        );
        assert!(!start, "the mic still holds Start back");

        drop(mic);
        change(|c, _| {
            c.open(|| Ok(None), |_| Ok(()));
            assert!(
                c.error().is_none(),
                "a settings file problem would colour the line red"
            );
            assert!(c.model.begin_login());
        });
        let (label, status, kind, start) = shown();
        assert_eq!(label.as_deref(), Some("Cancel login"));
        assert!(
            status.starts_with("Waiting for login in the browser window"),
            "{status}"
        );
        assert_eq!(kind, sys::OBS_TEXT_INFO_NORMAL, "waiting is no error");
        assert!(!start, "no Start while logging in");
        cancel_login_clicked(ptr::null_mut(), ptr::null_mut(), dialog.0.cast());
        let (label, status, _, start) = shown();
        assert_eq!(label.as_deref(), Some("Log in"));
        assert!(status.starts_with("The login window closed"), "{status}");
        assert!(start);

        login_clicked(ptr::null_mut(), ptr::null_mut(), dialog.0.cast());
        let (label, status, _, _) = shown();
        assert_eq!(label, None, "no control thread, so the login ends at once");
        assert!(status.contains("the login can't start"), "{status}");
        drop(dialog);
        reset();
    }

    #[test]
    fn the_dialog_asks_before_going_live_and_cancel_starts_nothing() {
        use dstreamy_obs_producer::control::Named;

        let _serial = start();
        let named = |id: &str, name: &str| Named {
            id: id.into(),
            name: name.into(),
        };
        change(|c, _| {
            c.model.select_guild("g");
            c.model.select_channel("c");
            c.model
                .guilds_loaded(Ok(vec![named("g", "Beste Chatgruppe")]));
            c.model
                .channels_loaded("g", Ok(vec![named("c", "Kids Lounge")]));
        });
        let dialog = Dialog::create();
        let shown = || {
            // SAFETY: a live source; the properties are destroyed before returning.
            unsafe {
                let props = sys::obs_source_properties(dialog.0);
                let described = |key: &CStr| {
                    let p = sys::obs_properties_get(props, key.as_ptr());
                    (!p.is_null()).then(|| text(sys::obs_property_description(p)))
                };
                let start = sys::obs_properties_get(props, c"start".as_ptr());
                let shown = (
                    (!start.is_null()).then(|| sys::obs_property_enabled(start)),
                    described(STREAM_QUESTION),
                    described(CONFIRM_STREAM),
                    described(CANCEL_STREAM),
                    described(c"stop").is_some(),
                );
                sys::obs_properties_destroy(props);
                shown
            }
        };
        let problem = || with_controls(|c| c.model.problem().map(|p| p.headline));
        assert_eq!(shown(), (Some(true), None, None, None, true));
        start_clicked(ptr::null_mut(), ptr::null_mut(), dialog.0.cast());
        assert_eq!(
            shown(),
            (
                None,
                Some("Go live in Kids Lounge on Beste Chatgruppe?".into()),
                Some("Go live".into()),
                Some("Cancel".into()),
                false
            ),
            "the question takes Start's and Stop's place"
        );
        cancel_stream_clicked(ptr::null_mut(), ptr::null_mut(), dialog.0.cast());
        assert_eq!(shown(), (Some(true), None, None, None, true));
        assert_eq!(problem(), None);
        assert_eq!(with_controls(|c| c.model.state().clone()), PanelState::Idle);

        stop_clicked(ptr::null_mut(), ptr::null_mut(), dialog.0.cast());
        assert_eq!(shown().1, None, "nothing to stop, so no question");
        assert_eq!(STREAM_CONFIRM.load(Ordering::Acquire), 0);

        start_clicked(ptr::null_mut(), ptr::null_mut(), dialog.0.cast());
        confirm_stream_clicked(ptr::null_mut(), ptr::null_mut(), dialog.0.cast());
        assert!(
            problem().is_some(),
            "the confirmed start ran and found no video"
        );
        assert_eq!(STREAM_CONFIRM.load(Ordering::Acquire), 0);
        assert_eq!(shown().1, None);
        drop(dialog);
        reset();
    }

    #[test]
    fn the_dialog_keeps_start_disabled_until_a_server_and_a_voice_channel_are_picked() {
        let _serial = start();
        let dialog = Dialog::create();
        let start_enabled = || {
            // SAFETY: a live source; the properties are destroyed before returning.
            unsafe {
                let props = sys::obs_source_properties(dialog.0);
                let enabled =
                    sys::obs_property_enabled(sys::obs_properties_get(props, c"start".as_ptr()));
                sys::obs_properties_destroy(props);
                enabled
            }
        };
        assert!(!start_enabled(), "nothing picked");
        change(|c, _| {
            c.model.select_guild("g");
        });
        assert!(!start_enabled(), "a server but no voice channel");
        change(|c, _| c.model.select_channel(""));
        assert!(!start_enabled(), "an empty channel is no pick");
        change(|c, _| c.model.select_channel("c"));
        assert!(start_enabled(), "both picked");
        change(|c, _| {
            c.model.choose_guild(None);
        });
        assert!(!start_enabled(), "clearing the server clears the channel");
        drop(dialog);
        reset();
    }

    #[test]
    fn the_dialog_asks_before_logging_out_and_flags_a_server_the_account_lacks() {
        use dstreamy_obs_producer::control::{Account, Accounts, Named};

        let _serial = start();
        let dialog = Dialog::create();
        let shown = || {
            // SAFETY: a live source; the properties are destroyed before returning.
            unsafe {
                let props = sys::obs_source_properties(dialog.0);
                let get = |key: &CStr| sys::obs_properties_get(props, key.as_ptr());
                let describe = |key: &CStr| {
                    let p = get(key);
                    (!p.is_null()).then(|| text(sys::obs_property_description(p)))
                };
                let guild = get(GUILD);
                let names: Vec<String> = (0..sys::obs_property_list_item_count(guild))
                    .map(|i| text(sys::obs_property_list_item_name(guild, i)))
                    .collect();
                let shown = (
                    describe(LOGOUT_ACCOUNT),
                    describe(LOGOUT_QUESTION),
                    describe(CONFIRM_LOGOUT),
                    describe(CANCEL_LOGOUT),
                    names,
                    sys::obs_property_enabled(get(c"start")),
                );
                sys::obs_properties_destroy(props);
                shown
            }
        };
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
            c.model.select_guild("g");
            c.model.select_channel("c");
            c.model.guilds_loaded(Ok(vec![Named {
                id: "h".into(),
                name: "Other".into(),
            }]));
        });
        let (logout, question, _, _, names, start) = shown();
        assert_eq!(logout.as_deref(), Some("Log out"));
        assert_eq!(question, None);
        assert_eq!(names, ["Pick a server", "Other", "Not available"]);
        assert!(!start, "a server the account isn't in can't start");
        logout_account_clicked(ptr::null_mut(), ptr::null_mut(), dialog.0.cast());
        let (logout, question, ok, cancel, _, _) = shown();
        assert_eq!(logout, None);
        assert_eq!(question.as_deref(), Some("Log out a?"));
        assert_eq!(
            (ok.as_deref(), cancel.as_deref()),
            (Some("Log out"), Some("Not now"))
        );
        cancel_logout_clicked(ptr::null_mut(), ptr::null_mut(), dialog.0.cast());
        assert_eq!(shown().1, None);
        logout_account_clicked(ptr::null_mut(), ptr::null_mut(), dialog.0.cast());
        confirm_logout_clicked(ptr::null_mut(), ptr::null_mut(), dialog.0.cast());
        assert_eq!(shown().1, None);
        assert!(
            with_controls(|c| c
                .model
                .problem()
                .unwrap()
                .message
                .contains("the account can't change")),
            "no control thread, so the logout ends at once"
        );
        drop(dialog);
        reset();
    }

    #[test]
    fn the_dialog_shows_the_account_row_and_disables_it_while_live() {
        use dstreamy_obs_producer::control::{Account, Accounts, Started};
        use dstreamy_obs_producer::model::Canvas;

        let _serial = start();
        let dialog = Dialog::create();
        let shown = || {
            // SAFETY: a live source; the properties are destroyed before returning.
            unsafe {
                let props = sys::obs_source_properties(dialog.0);
                let get = |key: &CStr| sys::obs_properties_get(props, key.as_ptr());
                let line = get(ACCOUNT);
                let line = (!line.is_null()).then(|| text(sys::obs_property_description(line)));
                let button = |key: &CStr| {
                    let b = get(key);
                    (!b.is_null()).then(|| {
                        (
                            text(sys::obs_property_description(b)),
                            sys::obs_property_enabled(b),
                            text(sys::obs_property_long_description(b)),
                        )
                    })
                };
                let shown = (
                    line,
                    button(c"account_use_b"),
                    button(ADD_ACCOUNT),
                    button(LOGOUT_ACCOUNT),
                    get(c"account_use_a").is_null(),
                );
                sys::obs_properties_destroy(props);
                shown
            }
        };
        assert_eq!(shown().0, None, "an older serve has no account row");
        let account = |name: &str, user: Option<&str>| Account {
            name: name.into(),
            username: user.map(Into::into),
            global_name: None,
            has_token: true,
            active: name == "a",
        };
        change(|c, _| {
            c.model.accounts_loaded(Ok(Some(Accounts {
                active: Some("a".into()),
                accounts: vec![account("a", Some("alpha")), account("b", None)],
            })));
        });
        let (line, use_b, add, logout, no_use_a) = shown();
        assert_eq!(line.as_deref(), Some("Account: a (alpha)"));
        assert_eq!(use_b, Some(("Use b".into(), true, String::new())));
        assert_eq!(add, Some(("Add account…".into(), true, String::new())));
        assert_eq!(logout, Some(("Log out".into(), true, String::new())));
        assert!(no_use_a, "the account in use has no Use button");

        change(|c, _| {
            c.model.select_guild("g");
            c.model.select_channel("c");
            c.model
                .start(Canvas {
                    fps_num: 30,
                    fps_den: 1,
                    output_width: 1280,
                    output_height: 720,
                })
                .unwrap();
            c.model.claim(None, &[]);
            c.model.started(Ok(Started {
                ownership: None,
                channel: "c".into(),
            }));
        });
        let (_, use_b, add, logout, _) = shown();
        let tip = "Stop the stream to switch accounts".to_owned();
        assert_eq!(use_b, Some(("Use b".into(), false, tip.clone())));
        assert_eq!(add, Some(("Add account…".into(), false, tip)));
        assert_eq!(
            logout,
            Some(("Log out".into(), false, "Stop the stream to log out".into()))
        );
        reset();

        change(|c, _| {
            c.model.accounts_loaded(Ok(Some(Accounts {
                active: Some("a".into()),
                accounts: vec![account("a", None), account("b", None)],
            })));
        });
        // SAFETY: a live property list the call frees at once.
        let props = unsafe { sys::obs_source_properties(dialog.0) };
        // SAFETY: as above.
        let button = unsafe { sys::obs_properties_get(props, c"account_use_b".as_ptr()) };
        use_account_clicked(ptr::null_mut(), button, dialog.0.cast());
        // SAFETY: as above.
        unsafe { sys::obs_properties_destroy(props) };
        let status = with_controls(|c| c.model.status_text());
        assert!(status.contains("the account can't change"), "{status}");
        assert!(
            !with_controls(|c| c.model.account_changing()),
            "never stuck"
        );
        drop(dialog);
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
            RELOADING.with(|r| r.set(false));
            let o = lock(&OPENED);
            let o = o.iter().find(|o| o.source == dialog.0).unwrap();
            *lock(&REVERTING) = Some(Reverting {
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
        edited_back.tick(&desktop, true);
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
        cancelled.tick(&desktop, false);
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
