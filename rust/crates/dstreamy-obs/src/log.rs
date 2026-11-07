use std::ffi::{CString, c_int};

#[cfg(unix)]
use dstreamy_obs_producer::{Level, Log};
use dstreamy_obs_sys as sys;

const PREFIX: &str = "[obs-dstreamy] ";

fn blog(level: c_int, msg: &str) {
    let text = CString::new(format!("{PREFIX}{msg}").replace('\0', "\\0"))
        .expect("NUL bytes were replaced");
    // SAFETY: "%s" consumes exactly the one NUL-terminated string passed.
    unsafe { sys::blog(level, c"%s".as_ptr(), text.as_ptr()) };
}

pub fn error(msg: &str) {
    blog(sys::LOG_ERROR, msg);
}

pub fn warn(msg: &str) {
    blog(sys::LOG_WARNING, msg);
}

pub fn info(msg: &str) {
    blog(sys::LOG_INFO, msg);
}

pub fn debug(msg: &str) {
    blog(sys::LOG_DEBUG, msg);
}

#[cfg(unix)]
pub struct Obs;

#[cfg(unix)]
impl Log for Obs {
    fn keyframe_sent(&self, pts_us: i64, arrived_ns: i64) {
        crate::hooks::observe(
            "keyframe_sent",
            || serde_json::json!({"pts_us":pts_us,"arrived_ns":arrived_ns}),
        );
    }
    fn stop_drain_started(&self, queued: usize) {
        crate::hooks::observe(
            "stop_drain_started",
            || serde_json::json!({"queued":queued}),
        );
    }
    fn stop_drain_timed_out(&self, deadline_ms: u64) {
        crate::hooks::observe(
            "stop_drain_timed_out",
            || serde_json::json!({"deadline_ms":deadline_ms}),
        );
    }
    fn log(&self, level: Level, message: &str) {
        match level {
            Level::Debug => debug(message),
            Level::Info => info(message),
            Level::Warn => warn(message),
        }
    }
}
