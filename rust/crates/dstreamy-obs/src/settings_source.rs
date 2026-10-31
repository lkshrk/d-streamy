//! `dstreamy_settings`: a private source whose stock properties dialog is D-Streamy's settings window.

use std::ffi::{CStr, c_char, c_void};
use std::mem;
use std::ptr;

use dstreamy_obs_sys as sys;

use crate::guard::guard;

pub const SOURCE_ID: &CStr = c"dstreamy_settings";
pub const SOURCE_NAME: &CStr = c"D-Streamy";

extern "C" fn get_name(_type_data: *mut c_void) -> *const c_char {
    SOURCE_NAME.as_ptr()
}

extern "C" fn create(
    _settings: *mut sys::obs_data_t,
    source: *mut sys::obs_source_t,
) -> *mut c_void {
    // libobs treats a null data pointer as a failed create.
    source.cast()
}

extern "C" fn destroy(_data: *mut c_void) {}

extern "C" fn get_properties(_data: *mut c_void) -> *mut sys::obs_properties_t {
    guard("settings get_properties", ptr::null_mut(), || {
        // SAFETY: the caller owns and destroys the returned properties.
        unsafe { sys::obs_properties_create() }
    })
}

pub fn register() {
    let info = sys::obs_source_info {
        id: SOURCE_ID.as_ptr(),
        type_: sys::OBS_SOURCE_TYPE_INPUT,
        // No video or audio flag: it captures nothing. CAP_DISABLED keeps it out of Add Source.
        output_flags: sys::OBS_SOURCE_CAP_DISABLED,
        get_name: Some(get_name),
        create: Some(create),
        destroy: Some(destroy),
        get_properties: Some(get_properties),
        ..Default::default()
    };
    // SAFETY: libobs copies `info` (`obs-module.c:956-1052`); the strings it points to are 'static.
    unsafe { sys::obs_register_source_s(&info, mem::size_of::<sys::obs_source_info>()) };
}
