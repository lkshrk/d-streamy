//! Test-only hooks from output settings and `DSTREAMY_TEST_*` variables; no-ops without the `test-hooks` feature.

#[cfg(feature = "test-hooks")]
mod enabled {

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
        output_error_gate: Option<std::path::PathBuf>,
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
                    crate::output::text(panic_in),
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
                output_error_gate: std::env::var_os("DSTREAMY_TEST_OUTPUT_ERROR_GATE")
                    .map(Into::into),
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

        pub fn output_error(&self) -> bool {
            self.output_error_gate
                .as_ref()
                .is_some_and(|path| path.exists())
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

    /// A file the output waits for before it attaches to the service, so a test holds it started but not capturing.
    pub fn capture_gate() -> Option<std::path::PathBuf> {
        std::env::var_os("DSTREAMY_TEST_CAPTURE_GATE").map(Into::into)
    }

    /// Delays counting the output's stop signal, which libobs may send after `active` is clear.
    pub fn delay_stop_signal() {
        if let Some(ms) = std::env::var("DSTREAMY_TEST_STOP_SIGNAL_DELAY_MS")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
    }

    /// Holds a stop between its two state reads until the gate file appears, then waits, bounded, for deactivation.
    pub fn between_stop_reads(inactive: impl Fn() -> bool) {
        let Some(gate) =
            std::env::var_os("DSTREAMY_TEST_STOP_READ_GATE").map(std::path::PathBuf::from)
        else {
            return;
        };
        super::observe("stop_read_gated", || serde_json::json!({}));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !gate.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !inactive() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// A file Tools > D-Streamy's Start waits for before it asks the service, so a test decides when it answers.
    pub fn start_gate() -> Option<std::path::PathBuf> {
        std::env::var_os("DSTREAMY_TEST_START_GATE").map(Into::into)
    }

    /// A file a start held back by a microphone waits for before it ends its Discord session.
    pub fn mic_refusal_gate() -> Option<std::path::PathBuf> {
        std::env::var_os("DSTREAMY_TEST_MIC_REFUSAL_GATE").map(Into::into)
    }

    /// Skips starting the control thread, so every Start and Stop finds it gone.
    pub fn no_control_thread() -> bool {
        std::env::var_os("DSTREAMY_TEST_NO_CONTROL_THREAD").is_some()
    }

    /// Panics once Tools > D-Streamy's output has started, before the dialog shows it live.
    pub fn after_output_start() {
        if std::env::var_os("DSTREAMY_TEST_PANIC_AFTER_OUTPUT_START").is_some() {
            panic!("test panic after the output started");
        }
    }

    /// A file whose appearance runs Tools > D-Streamy… as a click on it would, in a real OBS.
    pub fn open_dialog_trigger() -> Option<std::path::PathBuf> {
        std::env::var_os("DSTREAMY_TEST_OPEN_DIALOG").map(Into::into)
    }

    /// A file whose appearance presses the panel's Start, in a real OBS.
    pub fn press_start_trigger() -> Option<std::path::PathBuf> {
        std::env::var_os("DSTREAMY_TEST_PRESS_START").map(Into::into)
    }

    /// A file whose appearance presses the panel's Stop, in a real OBS.
    pub fn press_stop_trigger() -> Option<std::path::PathBuf> {
        std::env::var_os("DSTREAMY_TEST_PRESS_STOP").map(Into::into)
    }

    /// A file the server list waits for, so a test sees the open dialog rebuilt once it loads.
    pub fn lists_gate() -> Option<std::path::PathBuf> {
        std::env::var_os("DSTREAMY_TEST_LISTS_GATE").map(Into::into)
    }

    /// Logs every build of the dialog, which a real OBS does on its own thread.
    pub fn dialog_built(status: &str, servers: usize) {
        if open_dialog_trigger().is_some() {
            super::observe(
                "dialog_built",
                || serde_json::json!({"servers":servers,"status":status}),
            );
            crate::log::info(&format!(
                "test hook: dialog built servers={servers} status={status}"
            ));
        }
    }
}

#[cfg(not(feature = "test-hooks"))]
mod enabled {
    use dstreamy_obs_sys as sys;

    pub fn start_gate() -> Option<std::path::PathBuf> {
        None
    }

    pub fn mic_refusal_gate() -> Option<std::path::PathBuf> {
        None
    }

    pub fn no_control_thread() -> bool {
        false
    }

    pub fn capture_gate() -> Option<std::path::PathBuf> {
        None
    }

    pub fn delay_stop_signal() {}

    pub fn between_stop_reads(_inactive: impl Fn() -> bool) {}

    pub fn after_output_start() {}

    pub fn open_dialog_trigger() -> Option<std::path::PathBuf> {
        None
    }

    pub fn press_start_trigger() -> Option<std::path::PathBuf> {
        None
    }

    pub fn press_stop_trigger() -> Option<std::path::PathBuf> {
        None
    }

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

        pub fn output_error(&self) -> bool {
            false
        }

        pub fn on_packet(&self, _seen: i64) {}

        pub fn before_destroy(&self) {}
    }
}

pub use enabled::{
    Hooks, after_output_start, between_stop_reads, capture_gate, delay_stop_signal, dialog_built,
    lists_gate, mic_refusal_gate, no_control_thread, open_dialog_trigger, press_start_trigger,
    press_stop_trigger, start_gate,
};

/// Structured observations of decisions in test builds, independent of human log wording.
pub fn observe(event: &str, fields: impl FnOnce() -> serde_json::Value) {
    #[cfg(feature = "test-hooks")]
    {
        use std::ffi::{CString, c_char, c_int, c_void};
        #[cfg_attr(
            all(windows, target_env = "msvc"),
            link(name = "legacy_stdio_definitions")
        )]
        unsafe extern "C" {
            fn printf(format: *const c_char, ...) -> c_int;
            fn fflush(stream: *mut c_void) -> c_int;
        }
        let mut row = fields();
        row["event"] = serde_json::Value::String(format!("plugin_{event}"));
        let row = CString::new(row.to_string()).expect("JSON escapes NUL bytes");
        // SAFETY: the fixed format reads this live CString under the harness's C stdout lock.
        unsafe {
            printf(c"%s\n".as_ptr(), row.as_ptr());
            fflush(std::ptr::null_mut());
        }
    }
    #[cfg(not(feature = "test-hooks"))]
    let _ = (event, fields);
}

pub fn install_observations() {
    #[cfg(feature = "test-hooks")]
    // SAFETY: module load owns the live global signal handler; libobs copies the declaration.
    unsafe {
        dstreamy_obs_sys::signal_handler_add(
            dstreamy_obs_sys::obs_get_signal_handler(),
            c"void dstreamy_test_settings_settled()".as_ptr(),
        );
    }
}

pub fn settings_settled() {
    #[cfg(feature = "test-hooks")]
    // SAFETY: the registered signal has no parameters and its handler is live during plugin callbacks.
    unsafe {
        let mut data = dstreamy_obs_sys::calldata_t::default();
        dstreamy_obs_sys::signal_handler_signal(
            dstreamy_obs_sys::obs_get_signal_handler(),
            c"dstreamy_test_settings_settled".as_ptr(),
            &mut data,
        );
    }
}

#[cfg(feature = "test-hooks")]
#[unsafe(export_name = "DSTREAMY_TEST_OBSERVATION_EMIT")]
pub extern "C" fn observation_probe(sequence: u32) {
    crate::guard::guard("observation probe", (), || {
        observe("framing_probe", || {
            serde_json::json!({
                "sequence": sequence,
                "payload": "Grüße 🧃".repeat(1024),
                "note": "first\nsecond",
                "nul": "\0",
            })
        });
    });
}
