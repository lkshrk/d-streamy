//! `dstreamy_settings`: a private source whose stock properties dialog is D-Streamy's settings window.

use std::ffi::{CStr, CString, c_char, c_void};
use std::mem;
use std::ptr;
use std::sync::{Mutex, PoisonError};

use dstreamy_obs_producer::dock::{
    AUDIO_TRACK_HELP, AUDIO_TRACK_TITLE, AUDIO_TRACKS, Choice, DockState, TrackStatus, track_name,
};
use dstreamy_obs_producer::dock_config::{ConfigError, ConfigState};
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

/// Shown instead of the session status where the plugin has no service client yet.
const UNSUPPORTED: &str =
    "D-Streamy for OBS is not supported on Windows yet, so Start and Stop are disabled";

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
fn open_config() {
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

fn forget_dialog(source: *mut sys::obs_source_t) {
    let mut dialog = DIALOG.lock().unwrap_or_else(PoisonError::into_inner);
    // SAFETY: both pointers are live; the weak reference is this module's own.
    let ours = dialog
        .as_ref()
        .is_some_and(|d| unsafe { sys::obs_weak_source_references_source(d.0, source) });
    if ours && let Some(old) = dialog.take() {
        // SAFETY: as above.
        unsafe { sys::obs_weak_source_release(old.0) };
    }
}

/// Asks the open dialog, if any, to rebuild itself from the model; callable from any thread, without the model lock.
pub fn refresh() {
    let source = DIALOG
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        // SAFETY: the weak reference is live while it is in DIALOG; this returns a strong reference or NULL.
        .map(|d| unsafe { sys::obs_weak_source_get_source(d.0) });
    if let Some(source) = source.filter(|s| !s.is_null()) {
        // SAFETY: a strong reference taken above; OBS's properties view reloads on its own thread.
        unsafe {
            sys::obs_source_update_properties(source);
            sys::obs_source_release(source);
        }
    }
}

fn selected_track(settings: *mut sys::obs_data_t) -> Option<u8> {
    // SAFETY: the caller passes live settings.
    let raw = unsafe { sys::obs_data_get_int(settings, AUDIO_TRACK.as_ptr()) };
    u8::try_from(raw).ok().filter(|t| AUDIO_TRACKS.contains(t))
}

/// The status line for a dialog showing `selected`, and how OBS should colour it.
fn status(selected: Option<u8>) -> (String, sys::obs_text_info_type) {
    with_controls(|c| {
        let mut preview = c.model.clone();
        if let Some(track) = selected {
            preview.select_audio_track(track);
        }
        let quiet = preview.state() == &DockState::Idle && preview.error().is_none();
        let session = if cfg!(unix) {
            preview.status_text(true)
        } else {
            UNSUPPORTED.to_owned()
        };
        if let Some(e) = c.error() {
            let config = e.message(preview.settings().audio_track);
            let text = if quiet && cfg!(unix) {
                config
            } else {
                format!("{session}. {config}")
            };
            return (text, sys::OBS_TEXT_INFO_ERROR);
        }
        let kind = match (preview.track_status(), preview.error()) {
            _ if !cfg!(unix) => sys::OBS_TEXT_INFO_WARNING,
            (TrackStatus::Pending { .. }, _) => sys::OBS_TEXT_INFO_WARNING,
            (TrackStatus::Idle { .. }, Some(_)) => sys::OBS_TEXT_INFO_ERROR,
            _ => sys::OBS_TEXT_INFO_NORMAL,
        };
        (session, kind)
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
        watch_dialog(source);
        open_config();
        let guild = with_controls(|c| c.model.settings().guild_id.clone());
        session::load_lists(guild);
        let picked = with_controls(|c| c.model.settings().clone());
        let choice = with_controls(|c| c.model.choice());
        OPENED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Opened(source, choice));
        // SAFETY: libobs passes the new source's settings, which it owns; it copies the strings.
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
        // libobs treats a null data pointer as a failed create.
        source.cast()
    })
}

extern "C" fn destroy(data: *mut c_void) {
    guard("settings destroy", (), || {
        let source: *mut sys::obs_source_t = data.cast();
        OPENED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|o| o.0 != source);
        forget_dialog(source);
    });
}

extern "C" fn get_defaults(settings: *mut sys::obs_data_t) {
    guard("settings get_defaults", (), || {
        // SAFETY: libobs passes the defaults object it owns.
        unsafe { sys::obs_data_set_default_int(settings, AUDIO_TRACK.as_ptr(), 1) };
    });
}

/// What each open dialog showed when it opened, by source, so its Cancel can put exactly that back.
struct Opened(*mut sys::obs_source_t, Choice);

// SAFETY: the pointer is only compared, never dereferenced.
unsafe impl Send for Opened {}

static OPENED: Mutex<Vec<Opened>> = Mutex::new(Vec::new());

fn opened(source: *mut sys::obs_source_t) -> Option<Choice> {
    let opened = OPENED.lock().unwrap_or_else(PoisonError::into_inner);
    opened.iter().find(|o| o.0 == source).map(|o| o.1.clone())
}

/// Runs on the UI thread whenever the dialog changes a value. Cancel calls it with every value the dialog opened
/// with; those are put back as they were, even ones the service doesn't list (yet).
extern "C" fn update(data: *mut c_void, settings: *mut sys::obs_data_t) {
    guard("settings update", (), || {
        // SAFETY: libobs passes the source's own settings.
        let (guild, channel) = unsafe { (picked(settings, GUILD), picked(settings, CHANNEL)) };
        let shown = selected_track(settings).map(|audio_track| Choice {
            guild_id: guild.clone(),
            channel_id: channel.clone(),
            audio_track,
        });
        if let Some(opened) = opened(data.cast()).filter(|o| Some(o) == shown.as_ref()) {
            let load = change(|c, p| {
                let mut load = None;
                if c.model.choice() != opened {
                    config_step(c, |c| {
                        load = c.edit(p.write(), |m| m.restore_choice(&opened))
                    });
                }
                load
            });
            if let Some(guild) = load {
                session::load_lists(Some(guild));
            }
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
    (text, kind): (String, sys::obs_text_info_type),
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
        // SAFETY: the view passes the properties it shows and their settings.
        let status_prop = unsafe { sys::obs_properties_get(props, STATUS.as_ptr()) };
        if status_prop.is_null() {
            return false;
        }
        set_status(status_prop, status(selected_track(settings)));
        true
    })
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
    // Rebuilds the dialog, so the channel list shows the new server's channels.
    true
}

extern "C" fn start_clicked(
    _props: *mut sys::obs_properties_t,
    _property: *mut sys::obs_property_t,
    _data: *mut c_void,
) -> bool {
    guard("settings start", true, || {
        session::start();
        true
    })
}

extern "C" fn stop_clicked(
    _props: *mut sys::obs_properties_t,
    _property: *mut sys::obs_property_t,
    _data: *mut c_void,
) -> bool {
    guard("settings stop", true, || {
        session::stop();
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
        let selected = if source.is_null() {
            None
        } else {
            // SAFETY: `data` is the source `create` returned; the settings reference is released at once.
            unsafe {
                let settings = sys::obs_source_get_settings(source);
                let track = selected_track(settings);
                sys::obs_data_release(settings);
                track
            }
        };
        let title = c_text(AUDIO_TRACK_TITLE);
        let help = c_text(AUDIO_TRACK_HELP);
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
            let props = sys::obs_properties_create();
            let status_prop = sys::obs_properties_add_text(
                props,
                STATUS.as_ptr(),
                c"".as_ptr(),
                sys::OBS_TEXT_INFO,
            );
            let shown = status(selected);
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
            sys::obs_properties_add_text(
                props,
                c"routing".as_ptr(),
                help.as_ptr(),
                sys::OBS_TEXT_INFO,
            );
            let buttons: [(&CStr, &CStr, sys::obs_property_clicked_t, bool); 2] = [
                (
                    c"start",
                    c"Start",
                    Some(start_clicked),
                    cfg!(unix) && state == DockState::Idle,
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
            props
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
