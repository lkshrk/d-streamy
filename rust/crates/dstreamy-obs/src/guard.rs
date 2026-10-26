use std::any::Any;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicI64, Ordering};

use crate::log;

static PANICS: AtomicI64 = AtomicI64::new(0);

/// Runs a libobs callback body; a panic is logged and returns `fallback` instead of unwinding into C.
pub fn guard<R>(callback: &str, fallback: R, body: impl FnOnce() -> R) -> R {
    guard_with(callback, fallback, body, |_| {})
}

/// Like [`guard`], and hands the panic message to `on_panic` before returning `fallback`.
pub fn guard_with<R>(
    callback: &str,
    fallback: R,
    body: impl FnOnce() -> R,
    on_panic: impl FnOnce(&str),
) -> R {
    match panic::catch_unwind(AssertUnwindSafe(body)) {
        Ok(value) => value,
        Err(payload) => {
            let message = record_panic(callback, payload.as_ref());
            if panic::catch_unwind(AssertUnwindSafe(|| on_panic(&message))).is_err() {
                log::warn(&format!("handling the panic in {callback} panicked too"));
            }
            fallback
        }
    }
}

/// Counts and logs a caught panic; returns the message.
pub fn record_panic(callback: &str, payload: &(dyn Any + Send)) -> String {
    PANICS.fetch_add(1, Ordering::Relaxed);
    let message = format!("panic caught in {callback}: {}", payload_text(payload));
    log::error(&message);
    message
}

pub fn panics() -> i64 {
    PANICS.load(Ordering::Relaxed)
}

fn payload_text(payload: &(dyn Any + Send)) -> &str {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s
    } else {
        "non-string panic payload"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_text_reads_str_and_string() {
        let s: Box<dyn Any + Send> = Box::new("static");
        let owned: Box<dyn Any + Send> = Box::new(String::from("owned"));
        let other: Box<dyn Any + Send> = Box::new(7_u8);
        assert_eq!(payload_text(s.as_ref()), "static");
        assert_eq!(payload_text(owned.as_ref()), "owned");
        assert_eq!(payload_text(other.as_ref()), "non-string panic payload");
    }
}
