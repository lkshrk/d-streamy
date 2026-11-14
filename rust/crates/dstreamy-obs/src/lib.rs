//! D-Streamy OBS plugin module: the entry points libobs looks up (`libobs/obs-module.c:40-50`).

mod audio;
mod config;
mod encoders;
mod frontend;
mod guard;
mod hooks;
mod log;
mod output;
mod session;
mod settings_source;
mod ui;
mod worker;

use std::ffi::CStr;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

use dstreamy_obs_sys as sys;

use guard::guard;

static MODULE: AtomicPtr<sys::obs_module_t> = AtomicPtr::new(ptr::null_mut());

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_set_pointer(module: *mut sys::obs_module_t) {
    guard("obs_module_set_pointer", (), || {
        MODULE.store(module, Ordering::Release);
    });
}

fn current_module() -> *mut sys::obs_module_t {
    MODULE.load(Ordering::Acquire)
}

/// Where libobs loaded this plugin's own library from.
fn module_binary() -> Option<PathBuf> {
    // SAFETY: the module pointer is set before load; libobs returns a string it owns, or NULL.
    unsafe {
        let raw = sys::obs_get_module_binary_path(current_module());
        if raw.is_null() {
            return None;
        }
        Some(PathBuf::from(
            CStr::from_ptr(raw).to_string_lossy().into_owned(),
        ))
    }
}

/// Lets the plugin start the `dstreamy.exe` it ships beside its DLL when no serve runs.
#[cfg(windows)]
fn install_launcher() {
    use dstreamy_obs_producer::{Endpoint, launch};
    let Some(exe) = module_binary().and_then(|dll| Some(dll.parent()?.join("dstreamy.exe"))) else {
        return;
    };
    if !exe.is_file() {
        log::warn(&format!(
            "{} is missing, so the plugin cannot start the D-Streamy service; run `dstreamy serve` yourself",
            exe.display()
        ));
        return;
    }
    let endpoint = match Endpoint::default_for_user() {
        Ok(endpoint) => endpoint,
        Err(e) => {
            log::warn(&format!(
                "the plugin cannot start the D-Streamy service: {e}"
            ));
            return;
        }
    };
    log::info(&format!(
        "starts {} when the D-Streamy service is not running",
        exe.display()
    ));
    launch::install(launch::Launcher::new(exe, ["serve"], endpoint));
}

/// Shuts down the serve this plugin started, unless another client's session is live on it.
#[cfg(windows)]
fn stop_started_service() {
    use dstreamy_obs_producer::launch::{self, Ended, SHUTDOWN_GRACE};
    let Some(launcher) = launch::installed() else {
        return;
    };
    let ours_left = settings_source::with_controls(|c| c.model.persisted().is_some());
    match launcher.stop_started(SHUTDOWN_GRACE, !ours_left) {
        Ended::NotRunning => {}
        Ended::Exited(status) => log::info(&format!(
            "the D-Streamy service D-Streamy started shut down ({status})"
        )),
        Ended::Killed => log::warn(&format!(
            "the D-Streamy service D-Streamy started didn't shut down within {} s, so D-Streamy ended it; \
             Discord may show its stream until Discord notices it is gone",
            SHUTDOWN_GRACE.as_secs()
        )),
        Ended::LeftRunning => log::info(
            "the D-Streamy service D-Streamy started keeps running: another client's Discord session is live on it",
        ),
    }
}

/// Serializes unit tests that share the plugin's global model.
#[cfg(test)]
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    guard::lock(&LOCK)
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_ver() -> u32 {
    guard("obs_module_ver", sys::LIBOBS_API_VER, || {
        sys::LIBOBS_API_VER
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_load() -> bool {
    guard("obs_module_load", false, || {
        if current_module().is_null() {
            log::error("obs_module_set_pointer was not called before obs_module_load");
            return false;
        }
        hooks::install_observations();
        output::register();
        settings_source::register();
        audio::install();
        #[cfg(windows)]
        install_launcher();
        session::install();
        settings_source::open_config();
        frontend::install();
        if frontend::available() {
            ui::install();
            frontend::add_tools_entry();
        }
        log::info(&format!("loaded (version {})", env!("CARGO_PKG_VERSION")));
        true
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_unload() {
    guard("obs_module_unload", (), || {
        ui::shutdown();
        frontend::shutdown();
        audio::shutdown();
        session::shutdown();
        #[cfg(windows)]
        stop_started_service();
        crate::hooks::observe("unloaded", || serde_json::json!({}));
        log::info("unloaded");
    });
}
