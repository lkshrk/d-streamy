//! Tools > D-Streamy… through obs-frontend-api, looked up at runtime so a libobs-only host still loads the plugin.

use std::ffi::{CStr, c_void};
use std::ptr;
use std::sync::OnceLock;

use dstreamy_obs_sys as sys;
use libloading::Library;

use crate::guard::guard;
use crate::log;
use crate::settings_source;

const TOOLS_ITEM: &CStr = c"D-Streamy\u{2026}";

struct Frontend {
    add_tools_menu_item: sys::dstreamy_frontend_add_tools_menu_item,
    open_source_properties: sys::dstreamy_frontend_open_source_properties,
    _host: Library,
}

static FRONTEND: OnceLock<Frontend> = OnceLock::new();

#[cfg(unix)]
fn host() -> Result<Library, Missing> {
    Ok(libloading::os::unix::Library::this().into())
}

#[cfg(windows)]
fn host() -> Result<Library, Missing> {
    libloading::os::windows::Library::open_already_loaded("obs-frontend-api.dll")
        .map(Into::into)
        .map_err(|_| Missing::Library)
}

/// Why the Tools entry is unavailable.
enum Missing {
    #[cfg_attr(unix, allow(dead_code))]
    Library,
    Frontend,
    Symbol(&'static str),
}

const ADD_TOOLS_MENU_ITEM: &str = "obs_frontend_add_tools_menu_item";
const OPEN_SOURCE_PROPERTIES: &str = "obs_frontend_open_source_properties";

impl Frontend {
    fn resolve() -> Result<Self, Missing> {
        let host = host()?;
        // SAFETY: wrapper.h asserts these types against obs-frontend-api.h; `_host` keeps the library loaded.
        let (add, open) = unsafe {
            (
                host.get::<sys::dstreamy_frontend_add_tools_menu_item>(
                    ADD_TOOLS_MENU_ITEM.as_bytes(),
                )
                .ok()
                .and_then(|s| *s),
                host.get::<sys::dstreamy_frontend_open_source_properties>(
                    OPEN_SOURCE_PROPERTIES.as_bytes(),
                )
                .ok()
                .and_then(|s| *s),
            )
        };
        match (add, open) {
            (Some(add), Some(open)) => Ok(Self {
                add_tools_menu_item: Some(add),
                open_source_properties: Some(open),
                _host: host,
            }),
            (None, None) => Err(Missing::Frontend),
            (None, Some(_)) => Err(Missing::Symbol(ADD_TOOLS_MENU_ITEM)),
            (Some(_), None) => Err(Missing::Symbol(OPEN_SOURCE_PROPERTIES)),
        }
    }
}

/// Adds Tools > D-Streamy… when OBS's frontend is present; call from `obs_module_load`, on the UI thread.
pub fn install() {
    let frontend = match Frontend::resolve() {
        Ok(frontend) => frontend,
        Err(Missing::Library) => {
            log::info(
                "no OBS frontend (obs-frontend-api.dll is not loaded): Tools > D-Streamy is unavailable",
            );
            return;
        }
        Err(Missing::Frontend) => {
            log::info("no OBS frontend: Tools > D-Streamy is unavailable");
            return;
        }
        Err(Missing::Symbol(name)) => {
            log::warn(&format!(
                "OBS frontend lacks {name}: Tools > D-Streamy is unavailable"
            ));
            return;
        }
    };
    let frontend = FRONTEND.get_or_init(|| frontend);
    if let Some(add) = frontend.add_tools_menu_item {
        // SAFETY: module load runs on the UI thread; the frontend copies the name.
        unsafe { add(TOOLS_ITEM.as_ptr(), Some(open_settings), ptr::null_mut()) };
        log::info("added Tools > D-Streamy");
    }
}

extern "C" fn open_settings(_private_data: *mut c_void) {
    guard("open_settings", (), || {
        let Some(open) = FRONTEND.get().and_then(|f| f.open_source_properties) else {
            return;
        };
        // SAFETY: the type is registered at module load; a null settings object is allowed.
        let source = unsafe {
            sys::obs_source_create_private(
                settings_source::SOURCE_ID.as_ptr(),
                settings_source::SOURCE_NAME.as_ptr(),
                ptr::null_mut(),
            )
        };
        if source.is_null() {
            log::warn("could not create the D-Streamy settings source");
            return;
        }
        // SAFETY: the properties dialog takes its own reference, so this one is released at once.
        unsafe {
            open(source);
            sys::obs_source_release(source);
        }
    });
}
