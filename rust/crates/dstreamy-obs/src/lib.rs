//! D-Streamy OBS plugin module: the entry points libobs looks up (`libobs/obs-module.c:40-50`).

mod config;
mod controller;
mod encoders;
mod guard;
mod hooks;
mod log;
mod output;
mod proc;
#[cfg(test)]
mod testdir;
mod worker;

use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, Ordering};

use dstreamy_obs_sys as sys;

use controller::{ControllerHandle, Parts, Remote, Unwired};
use guard::guard;

static MODULE: AtomicPtr<sys::obs_module_t> = AtomicPtr::new(ptr::null_mut());
static GATE: proc::Gate = proc::Gate::new(proc::OBS_REFS);
static CONTROLLER: Mutex<Option<ControllerHandle>> = Mutex::new(None);

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_set_pointer(module: *mut sys::obs_module_t) {
    guard("obs_module_set_pointer", (), || {
        MODULE.store(module, Ordering::Release);
    });
}

fn current_module() -> *mut sys::obs_module_t {
    MODULE.load(Ordering::Acquire)
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
        output::register();
        start_controller();
        log::info(&format!("loaded (version {})", env!("CARGO_PKG_VERSION")));
        true
    })
}

fn start_controller() {
    // SAFETY: libobs's global handlers live from obs_startup until obs_shutdown, which unloads modules first.
    let (procs, signals) = unsafe { (sys::obs_get_proc_handler(), sys::obs_get_signal_handler()) };
    if !GATE.register(procs, signals) {
        log::warn("the D-Streamy procs were already registered");
        return;
    }
    let parts = Parts::<Remote, Unwired> {
        store: config::ConfigStore::for_module(current_module()),
        service: Remote::for_user(),
        lifecycle: None,
        signals,
    };
    match controller::spawn(&GATE, parts) {
        Ok(handle) => *CONTROLLER.lock().unwrap_or_else(|p| p.into_inner()) = Some(handle),
        Err(e) => {
            log::error(&format!("cannot start the D-Streamy controller: {e}"));
            GATE.close();
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_unload() {
    guard("obs_module_unload", (), || {
        GATE.close();
        let handle = CONTROLLER.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(handle) = handle {
            handle.cancel_unload();
            handle.join();
        }
        log::info("unloaded");
    });
}
