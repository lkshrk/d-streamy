//! The module config file: the dock settings JSON in libobs's per-module config folder.

use std::ffi::CStr;
use std::fs;
use std::io;
use std::path::PathBuf;

use dstreamy_obs_producer::dock::track_name;
use dstreamy_obs_sys as sys;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    NoFolder,
    Failed(String),
}

impl ConfigError {
    pub fn message(&self, track: u8) -> String {
        match self {
            Self::NoFolder => format!(
                "OBS gave D-Streamy no settings folder, so {} is used until OBS quits but is not saved",
                track_name(track)
            ),
            Self::Failed(reason) => {
                format!("{reason}; {} is used until OBS quits", track_name(track))
            }
        }
    }
}

fn path() -> Result<PathBuf, ConfigError> {
    // SAFETY: the module pointer is set before load; libobs returns a bmalloc'd string that is freed below.
    let path = unsafe {
        let raw = sys::obs_module_get_config_path(crate::current_module(), c"config.json".as_ptr());
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

/// The saved settings JSON, or `None` before the first save.
pub fn load() -> Result<Option<String>, ConfigError> {
    let path = path()?;
    match fs::read_to_string(&path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ConfigError::Failed(format!(
            "Couldn't read D-Streamy's settings from {}: {e}",
            path.display()
        ))),
    }
}

pub fn save(json: &str) -> Result<(), ConfigError> {
    let path = path()?;
    let failed = |e: io::Error| {
        ConfigError::Failed(format!(
            "Couldn't save D-Streamy's settings to {}: {e}",
            path.display()
        ))
    };
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(failed)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json).map_err(failed)?;
    fs::rename(&tmp, &path).map_err(failed)
}
