use std::ffi::{CString, c_int};

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

pub struct Obs;

impl Log for Obs {
    fn log(&self, level: Level, message: &str) {
        match level {
            Level::Debug => debug(message),
            Level::Info => info(message),
            Level::Warn => warn(message),
        }
    }
}
