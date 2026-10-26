//! D-Streamy OBS plugin module: the entry points libobs looks up (`libobs/obs-module.c:40-50`).

mod guard;
mod hooks;
mod log;
mod output;
mod worker;

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
        log::info(&format!("loaded (version {})", env!("CARGO_PKG_VERSION")));
        true
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_unload() {
    guard("obs_module_unload", (), || log::info("unloaded"));
}
