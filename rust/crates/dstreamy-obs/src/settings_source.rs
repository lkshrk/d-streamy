//! `dstreamy_settings`: a private source whose stock properties dialog is D-Streamy's settings window.

use std::ffi::{CStr, CString, c_char, c_void};
use std::mem;
use std::ptr;
use std::sync::{Mutex, MutexGuard, PoisonError};

use dstreamy_obs_producer::dock::{
    AUDIO_TRACK_HELP, AUDIO_TRACK_TITLE, AUDIO_TRACKS, DockModel, DockSettings, TrackStatus,
    track_name,
};
use dstreamy_obs_sys as sys;

use crate::config::{self, ConfigError};
use crate::guard::guard;
use crate::log;

pub const SOURCE_ID: &CStr = c"dstreamy_settings";
pub const SOURCE_NAME: &CStr = c"D-Streamy";

const AUDIO_TRACK: &CStr = c"audio_track";
const STATUS: &CStr = c"status";

/// Whether the dialog's Start and Stop drive a real session; until they do they stay disabled.
const START_CONTROLS: bool = false;

/// The producer model behind every dialog, and why its settings could not be read or saved.
struct Controls {
    model: DockModel,
    config_error: Option<ConfigError>,
}

static CONTROLS: Mutex<Option<Controls>> = Mutex::new(None);

fn controls() -> MutexGuard<'static, Option<Controls>> {
    CONTROLS.lock().unwrap_or_else(PoisonError::into_inner)
}

fn with_controls<R>(f: impl FnOnce(&mut Controls) -> R) -> R {
    let mut guard = controls();
    let controls = guard.get_or_insert_with(|| Controls {
        model: DockModel::new(DockSettings::default()),
        config_error: None,
    });
    f(controls)
}

/// Reloads the module config into the model, keeping the in-memory settings when there is none.
fn load(controls: &mut Controls) {
    match config::load() {
        Ok(Some(text)) => {
            controls.model.reload(DockSettings::from_json(&text));
            controls.config_error = None;
        }
        Ok(None) => controls.config_error = None,
        Err(e) => controls.config_error = Some(e),
    }
}

fn save(controls: &mut Controls) {
    match config::save(&controls.model.settings().to_json()) {
        Ok(()) => controls.config_error = None,
        Err(e) => {
            log::warn(&e.message(controls.model.settings().audio_track));
            controls.config_error = Some(e);
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
        if let Some(e) = &c.config_error {
            return (
                e.message(preview.settings().audio_track),
                sys::OBS_TEXT_INFO_ERROR,
            );
        }
        let kind = match (preview.track_status(), preview.error()) {
            (TrackStatus::Pending { .. }, _) => sys::OBS_TEXT_INFO_WARNING,
            (TrackStatus::Idle { .. }, Some(_)) => sys::OBS_TEXT_INFO_ERROR,
            _ => sys::OBS_TEXT_INFO_NORMAL,
        };
        (preview.status_text(START_CONTROLS), kind)
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
        let track = with_controls(|c| {
            load(c);
            c.model.settings().audio_track
        });
        // SAFETY: libobs passes the new source's settings, which it owns.
        unsafe { sys::obs_data_set_int(settings, AUDIO_TRACK.as_ptr(), i64::from(track)) };
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
        with_controls(|c| match selected_track(settings) {
            Some(track) => {
                c.model.select_audio_track(track);
                save(c);
            }
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

extern "C" fn unavailable(
    _props: *mut sys::obs_properties_t,
    _property: *mut sys::obs_property_t,
    _data: *mut c_void,
) -> bool {
    guard("settings unavailable", false, || {
        log::warn("Start and Stop are not available from the D-Streamy dialog yet");
        false
    })
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
            for (name, label) in [(c"start", c"Start"), (c"stop", c"Stop")] {
                let button = sys::obs_properties_add_button(
                    props,
                    name.as_ptr(),
                    label.as_ptr(),
                    Some(unavailable),
                );
                sys::obs_property_set_enabled(button, START_CONTROLS);
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
