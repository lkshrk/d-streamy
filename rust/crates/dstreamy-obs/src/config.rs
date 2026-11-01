//! The module config file: the dock settings JSON in libobs's per-module config folder.

use std::ffi::CStr;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use dstreamy_obs_producer::dock_config::{ConfigError, Saved};
use dstreamy_obs_sys as sys;

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

/// The saved bytes, or `None` only when no file exists yet.
pub fn load() -> Result<Option<Saved>, ConfigError> {
    let path = path()?;
    match fs::read(&path) {
        Ok(bytes) => Ok(Some(Saved {
            path: path.display().to_string(),
            bytes,
        })),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ConfigError::Unreadable {
            path: path.display().to_string(),
            detail: e.to_string(),
        }),
    }
}

fn write_synced(tmp: &Path, json: &str) -> io::Result<()> {
    let mut file = File::create(tmp)?;
    file.write_all(json.as_bytes())?;
    file.sync_all()
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
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    // Unique per process and call, so two OBS instances never share a temp file.
    let tmp = path.with_extension(format!("json.{}.{nanos}.tmp", std::process::id()));
    let result = write_synced(&tmp, json).and_then(|()| fs::rename(&tmp, &path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(failed)
}
