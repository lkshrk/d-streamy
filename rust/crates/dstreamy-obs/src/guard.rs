use std::any::Any;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicI64, Ordering};

use crate::log;

static PANICS: AtomicI64 = AtomicI64::new(0);

/// Runs `body` for a callback from libobs; a panic is logged and turned into `fallback`
/// because unwinding into C is undefined behaviour.
pub fn guard<R>(callback: &str, fallback: R, body: impl FnOnce() -> R) -> R {
    match panic::catch_unwind(AssertUnwindSafe(body)) {
        Ok(value) => value,
        Err(payload) => {
            PANICS.fetch_add(1, Ordering::Relaxed);
            log::error(&format!(
                "panic caught in {callback}: {}",
                payload_text(payload.as_ref())
            ));
            fallback
        }
    }
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
