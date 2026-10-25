use std::ffi::{CString, c_int};

use dstreamy_obs_sys as sys;

const PREFIX: &str = "[obs-dstreamy] ";

fn blog(level: u32, msg: &str) {
    let text = CString::new(format!("{PREFIX}{msg}").replace('\0', "\\0"))
        .expect("NUL bytes were replaced");
    // SAFETY: "%s" consumes exactly the one NUL-terminated string passed.
    unsafe { sys::blog(level as c_int, c"%s".as_ptr(), text.as_ptr()) };
}

pub fn error(msg: &str) {
    blog(sys::LOG_ERROR, msg);
}

pub fn info(msg: &str) {
    blog(sys::LOG_INFO, msg);
}
