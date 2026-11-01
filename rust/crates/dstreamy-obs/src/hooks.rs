//! Test-only behaviour read from the output's settings and `DSTREAMY_TEST_*` variables; without the `test-hooks`
//! feature every hook is a no-op and none of those names is in the plugin.

#[cfg(feature = "test-hooks")]
mod enabled {
    use std::ffi::CStr;

    use dstreamy_obs_sys as sys;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum PanicIn {
        Nowhere,
        Start,
        Stop,
    }

    pub struct Hooks {
        panic_at: i64,
        panic_in: PanicIn,
        destroy_delay_ms: u64,
    }

    impl Hooks {
        /// # Safety
        /// `settings` is the output's live settings object.
        pub unsafe fn read(settings: *mut sys::obs_data_t) -> Self {
            // SAFETY: the caller's contract; libobs returns "" or a string valid for the call.
            let (panic_at, panic_in, delay) = unsafe {
                let panic_in = sys::obs_data_get_string(settings, c"panic_in".as_ptr());
                (
                    sys::obs_data_get_int(settings, c"panic_at_packet".as_ptr()),
                    if panic_in.is_null() {
                        String::new()
                    } else {
                        CStr::from_ptr(panic_in).to_string_lossy().into_owned()
                    },
                    sys::obs_data_get_int(settings, c"test_destroy_delay_ms".as_ptr()),
                )
            };
            Self {
                panic_at,
                panic_in: match panic_in.as_str() {
                    "start" => PanicIn::Start,
                    "stop" => PanicIn::Stop,
                    _ => PanicIn::Nowhere,
                },
                destroy_delay_ms: u64::try_from(delay).unwrap_or(0),
            }
        }

        pub fn on_start(&self) {
            if self.panic_in == PanicIn::Start {
                panic!("test panic in Start");
            }
        }

        pub fn on_stop(&self) {
            if self.panic_in == PanicIn::Stop {
                panic!("test panic in Stop");
            }
        }

        pub fn on_packet(&self, seen: i64) {
            if self.panic_at > 0 && seen == self.panic_at {
                panic!("test panic at packet {seen}");
            }
        }

        /// Widens the window between libobs's active check in destroy and our destroying flag.
        pub fn before_destroy(&self) {
            if self.destroy_delay_ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(self.destroy_delay_ms));
            }
        }
    }

    /// A file Tools > D-Streamy's Start waits for before it asks the service, so a test decides when it answers.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn start_gate() -> Option<std::path::PathBuf> {
        std::env::var_os("DSTREAMY_TEST_START_GATE").map(Into::into)
    }

    /// Skips starting the control thread, so every Start and Stop finds it gone.
    pub fn no_control_thread() -> bool {
        std::env::var_os("DSTREAMY_TEST_NO_CONTROL_THREAD").is_some()
    }

    /// Panics once Tools > D-Streamy's output has started, before the dialog shows it live.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn after_output_start() {
        if std::env::var_os("DSTREAMY_TEST_PANIC_AFTER_OUTPUT_START").is_some() {
            panic!("test panic after the output started");
        }
    }

    /// A file whose appearance runs Tools > D-Streamy… as a click on it would, in a real OBS.
    pub fn open_dialog_trigger() -> Option<std::path::PathBuf> {
        std::env::var_os("DSTREAMY_TEST_OPEN_DIALOG").map(Into::into)
    }

    /// A file the server list waits for, so a test sees the open dialog rebuilt once it loads.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn lists_gate() -> Option<std::path::PathBuf> {
        std::env::var_os("DSTREAMY_TEST_LISTS_GATE").map(Into::into)
    }

    /// Logs every build of the dialog, which a real OBS does on its own thread.
    pub fn dialog_built(status: &str, servers: usize) {
        if open_dialog_trigger().is_some() {
            crate::log::info(&format!(
                "test hook: dialog built servers={servers} status={status}"
            ));
        }
    }
}

#[cfg(not(feature = "test-hooks"))]
mod enabled {
    use dstreamy_obs_sys as sys;

    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn start_gate() -> Option<std::path::PathBuf> {
        None
    }

    pub fn no_control_thread() -> bool {
        false
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn after_output_start() {}

    pub fn open_dialog_trigger() -> Option<std::path::PathBuf> {
        None
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn lists_gate() -> Option<std::path::PathBuf> {
        None
    }

    pub fn dialog_built(_status: &str, _servers: usize) {}

    pub struct Hooks;

    impl Hooks {
        /// # Safety
        /// Nothing is read; the signature matches the `test-hooks` build.
        pub unsafe fn read(_settings: *mut sys::obs_data_t) -> Self {
            Self
        }

        pub fn on_start(&self) {}

        pub fn on_stop(&self) {}

        pub fn on_packet(&self, _seen: i64) {}

        pub fn before_destroy(&self) {}
    }
}

pub use enabled::{Hooks, dialog_built, no_control_thread, open_dialog_trigger};
#[cfg(unix)]
pub use enabled::{after_output_start, lists_gate, start_gate};
