//! `dstreamy_settings`: a private source whose stock properties dialog is D-Streamy's settings window.

use std::ffi::{CStr, CString, c_char, c_void};
use std::mem;
use std::ptr;
use std::sync::{Mutex, PoisonError};

use dstreamy_obs_producer::dock::{
    AUDIO_TRACK_HELP, AUDIO_TRACK_TITLE, AUDIO_TRACKS, DockState, TrackStatus, track_name,
};
use dstreamy_obs_producer::dock_config::ConfigState;
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
static CONTROLS: Mutex<Option<ConfigState>> = Mutex::new(None);

pub fn with_controls<R>(f: impl FnOnce(&mut ConfigState) -> R) -> R {
    let mut guard = CONTROLS.lock().unwrap_or_else(PoisonError::into_inner);
    f(guard.get_or_insert_with(ConfigState::default))
}

/// Runs a config step and logs its config error: every time for a change, only when new on open.
fn config_step(c: &mut ConfigState, change: bool, step: impl FnOnce(&mut ConfigState)) {
    let before = c.error().cloned();
    step(c);
    if let Some(e) = c.error().filter(|e| change || Some(*e) != before.as_ref()) {
        log::warn(&e.message(c.model.settings().audio_track));
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
        let session = preview.status_text(true);
        if let Some(e) = c.error() {
            let config = e.message(preview.settings().audio_track);
            let text = if preview.state() == &DockState::Idle {
                config
            } else {
                format!("{session}. {config}")
            };
            return (text, sys::OBS_TEXT_INFO_ERROR);
        }
        let kind = match (preview.track_status(), preview.error()) {
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
        let guild = with_controls(|c| {
            config_step(c, false, |c| c.open(config::load, config::save));
            c.model.settings().guild_id.clone()
        });
        session::load_lists(guild);
        let picked = with_controls(|c| c.model.settings().clone());
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

extern "C" fn destroy(_data: *mut c_void) {}

extern "C" fn get_defaults(settings: *mut sys::obs_data_t) {
    guard("settings get_defaults", (), || {
        // SAFETY: libobs passes the defaults object it owns.
        unsafe { sys::obs_data_set_default_int(settings, AUDIO_TRACK.as_ptr(), 1) };
    });
}

/// Runs on the UI thread whenever the dialog changes a value, and on Cancel with the old values.
extern "C" fn update(_data: *mut c_void, settings: *mut sys::obs_data_t) {
    guard("settings update", (), || {
        // SAFETY: libobs passes the source's own settings.
        let (guild, channel) = unsafe { (picked(settings, GUILD), picked(settings, CHANNEL)) };
        let load = with_controls(|c| {
            let guild = guild.filter(|g| c.model.settings().guild_id.as_ref() != Some(g))?;
            config_step(c, true, |c| {
                c.edit(config::save, |m| m.select_guild(&guild));
            });
            Some(guild)
        });
        if let Some(guild) = load {
            session::load_lists(Some(guild));
        }
        with_controls(|c| {
            if let Some(channel) =
                channel.filter(|ch| c.model.settings().channel_id.as_ref() != Some(ch))
            {
                config_step(c, true, |c| {
                    c.edit(config::save, |m| m.select_channel(&channel))
                });
            }
        });
        with_controls(|c| match selected_track(settings) {
            Some(track) => config_step(c, true, |c| {
                c.select_audio_track(track, config::save);
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
            set_status(status_prop, status(selected));
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
                    state == DockState::Idle,
                ),
                (
                    c"stop",
                    c"Stop",
                    Some(stop_clicked),
                    matches!(state, DockState::Live { .. }),
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
