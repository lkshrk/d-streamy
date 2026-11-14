//! The module config file: the D-Streamy settings JSON in libobs's per-module config folder.

use std::ffi::CStr;
use std::path::PathBuf;

use dstreamy_obs_producer::panel_config::{self, ConfigError, Saved};
use dstreamy_obs_sys as sys;

fn path() -> Result<PathBuf, ConfigError> {
    let module = crate::current_module();
    if module.is_null() {
        return Err(ConfigError::NoFolder);
    }
    // SAFETY: the module pointer is set before load; libobs returns a bmalloc'd string that is freed below.
    let path = unsafe {
        let raw = sys::obs_module_get_config_path(module, c"config.json".as_ptr());
        if raw.is_null() {
            return Err(ConfigError::NoFolder);
        }
        let path = PathBuf::from(CStr::from_ptr(raw).to_string_lossy().into_owned());
        sys::bfree(raw.cast());
        path
    };
    // Without a module config path from obs_startup, libobs returns a path relative to the working directory.
    if path.is_relative() {
        return Err(ConfigError::NoFolder);
    }
    Ok(path)
}

/// The saved bytes, or `None` only when no file exists yet.
pub fn load() -> Result<Option<Saved>, ConfigError> {
    panel_config::load_file(&path()?)
}

pub fn save(json: &str) -> Result<(), ConfigError> {
    panel_config::save_file(&path()?, json)
}
