//! The producer test harness itself: what `Proc` keeps and when `wait_line` gives up.
#![cfg(unix)]

mod support;

use std::process::Command;
use std::time::{Duration, Instant};

use support::Proc;

fn shell(script: &str) -> Proc {
    Proc::spawn(Command::new("sh").args(["-c", script]))
}

#[test]
fn non_json_stdout_lines_are_kept_next_to_the_json_ones() {
    let p = shell("echo '{\"hello\":1}'; echo 'thread panicked at x'; echo '{\"done\":true}'");
    let done = p.wait_line("done", |v| v.get("done").is_some());
    assert_eq!(done["done"], true);
    assert_eq!(p.lines().len(), 2);
    assert_eq!(*p.text.lock().unwrap(), ["thread panicked at x"]);
}

#[test]
fn wait_line_fails_as_soon_as_the_process_exits_and_shows_its_output() {
    let p = shell("echo '{\"hello\":1}'; echo 'dying'; echo 'on stderr' >&2; exit 3");
    let started = Instant::now();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        p.wait_line("attached", |v| v.get("attached").is_some())
    }))
    .unwrap_err();
    let message = panic
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()).unwrap());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "waited {:?} for a process that had exited",
        started.elapsed()
    );
    for expected in [
        "attached never came: the process exit status: 3",
        "\"hello\"",
        "\"dying\"",
        "stderr: on stderr",
    ] {
        assert!(
            message.contains(expected),
            "{expected:?} missing from:\n{message}"
        );
    }
}

fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    panic
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_else(|| panic.downcast_ref::<&str>().unwrap().to_string())
}

fn exited(p: &Proc) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !p.exited() {
        assert!(Instant::now() < deadline, "child never exited");
        std::thread::park_timeout(Duration::from_millis(1));
    }
}

#[test]
fn an_exited_process_waits_for_its_delayed_stderr_reader() {
    use std::sync::{Arc, mpsc};
    let (release, gate) = mpsc::channel();
    let p = Arc::new(Proc::spawn_with_stderr_gate(
        Command::new("sh").args(["-c", "printf 'late stderr' >&2; exit 3"]),
        Some(gate),
    ));
    exited(&p);
    let (ready, started) = mpsc::channel();
    let (done, result) = mpsc::channel();
    let other = p.clone();
    let waiter = std::thread::spawn(move || {
        ready.send(()).unwrap();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            other.wait_line("missing", |_| false)
        }))
        .unwrap_err();
        done.send(panic_message(panic)).unwrap();
    });
    started.recv().unwrap();
    let early = result.recv_timeout(Duration::from_millis(100));
    release.send(()).unwrap();
    let was_early = early.is_ok();
    let message = early.unwrap_or_else(|_| result.recv_timeout(Duration::from_secs(3)).unwrap());
    waiter.join().unwrap();
    assert!(
        !was_early,
        "exit diagnostics escaped before stderr drained: {message}"
    );
    assert!(message.contains("stderr: late stderr"), "{message}");
}

struct HeldPipes {
    dir: tempfile::TempDir,
    gate: std::fs::File,
}

impl HeldPipes {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            Command::new("mkfifo")
                .arg(dir.path().join("gate"))
                .status()
                .unwrap()
                .success()
        );
        let gate = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.path().join("gate"))
            .unwrap();
        Self { dir, gate }
    }

    fn spawn(&self) -> Proc {
        Proc::spawn(Command::new("sh").args(["-c", "(read -r done < \"$1\"; : > \"$2\") & printf '{\"holder\":%s}\\n' \"$!\"; printf 'partial stdout'; printf 'partial stderr' >&2; exit 3", "holder"]).arg(self.dir.path().join("gate")).arg(self.dir.path().join("done")))
    }
}

impl Drop for HeldPipes {
    fn drop(&mut self) {
        use std::io::Write;
        let _ = self.gate.write_all(b"done\n");
        let deadline = Instant::now() + Duration::from_secs(3);
        while !self.dir.path().join("done").exists() && Instant::now() < deadline {
            std::thread::park_timeout(Duration::from_millis(1));
        }
    }
}

#[test]
fn inherited_open_pipes_do_not_hold_exit_diagnostics_or_partial_lines() {
    use std::sync::{Arc, mpsc};
    let held = HeldPipes::new();
    let p = Arc::new(held.spawn());
    p.wait_line("holder", |v| v.get("holder").is_some());
    exited(&p);
    let (done, result) = mpsc::channel();
    let other = p.clone();
    let waiter = std::thread::spawn(move || {
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            other.wait_line("missing", |_| false)
        }))
        .unwrap_err();
        done.send(panic_message(panic)).unwrap();
    });
    let before_release = result.recv_timeout(Duration::from_secs(1));
    drop(held);
    let bounded = before_release.is_ok();
    let message =
        before_release.unwrap_or_else(|_| result.recv_timeout(Duration::from_secs(3)).unwrap());
    waiter.join().unwrap();
    assert!(
        bounded,
        "inherited descriptors held exit diagnostics: {message}"
    );
    assert_eq!(*p.text.lock().unwrap(), ["partial stdout"]);
    assert_eq!(*p.err.lock().unwrap(), "partial stderr");
    assert!(
        message.contains("partial stdout") && message.contains("partial stderr"),
        "{message}"
    );
}

#[test]
fn eof_keeps_a_partial_carriage_return_and_multibyte_stderr_exactly() {
    let stderr = format!("{}€", "x".repeat(4095));
    let p = Proc::spawn(Command::new("sh").args([
        "-c",
        "printf 'partial\\r'; printf '%s' \"$1\" >&2",
        "fixture",
        &stderr,
    ]));
    let text = p.text.clone();
    let (status, captured) = p.finish();
    assert!(status.success());
    assert_eq!(captured, stderr);
    assert_eq!(*text.lock().unwrap(), ["partial\r"]);
}
