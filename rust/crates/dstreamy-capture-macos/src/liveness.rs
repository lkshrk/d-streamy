use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Told when the captured window or display is gone: closed, quit or disconnected.
pub(crate) const SOURCE_GONE: &str =
    "the shared window or display went away; pick another one and start again";

/// How long without a complete frame before the target is looked up again.
pub(crate) const PRESENCE_CHECK_IDLE: Duration = Duration::from_secs(2);
/// The least time between two lookups while frames stay away.
pub(crate) const PRESENCE_CHECK_INTERVAL: Duration = Duration::from_secs(2);

/// Decides when to check that the captured target still exists while ScreenCaptureKit sends no frames.
#[derive(Debug)]
pub(crate) struct PresenceWatch {
    last_frame: Instant,
    last_check: Option<Instant>,
    suspect: bool,
}

impl PresenceWatch {
    pub(crate) fn new(started: Instant) -> Self {
        Self {
            last_frame: started,
            last_check: None,
            suspect: false,
        }
    }

    pub(crate) fn frame(&mut self, now: Instant) {
        self.last_frame = now;
        self.last_check = None;
        self.suspect = false;
    }

    /// Makes the next lookup due at once, as after a frame with status `Stopped`.
    pub(crate) fn suspect(&mut self) {
        self.suspect = true;
    }

    /// True when a lookup is due; records it as done.
    pub(crate) fn check_due(&mut self, now: Instant) -> bool {
        let idle = now.saturating_duration_since(self.last_frame) >= PRESENCE_CHECK_IDLE;
        let spaced = self
            .last_check
            .is_none_or(|last| now.saturating_duration_since(last) >= PRESENCE_CHECK_INTERVAL);
        let due = std::mem::take(&mut self.suspect) || (idle && spaced);
        if due {
            self.last_check = Some(now);
        }
        due
    }
}

/// Whether the captured target is still listed; `None` when the lookup failed.
pub(crate) type Lookup = Box<dyn Fn() -> Option<bool> + Send + Sync>;

/// Runs target lookups on a thread of their own, so a slow one never holds up keyframes.
pub(crate) struct Presence {
    watch: Mutex<PresenceWatch>,
    lookup: Lookup,
    in_flight: AtomicBool,
    stopping: AtomicBool,
    /// Held while a confirmed absence is checked against `stopping` and reported.
    reporting: Mutex<()>,
}

impl Presence {
    pub(crate) fn new(lookup: Lookup, started: Instant) -> Arc<Self> {
        Arc::new(Self {
            watch: Mutex::new(PresenceWatch::new(started)),
            lookup,
            in_flight: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            reporting: Mutex::new(()),
        })
    }

    pub(crate) fn frame(&self, now: Instant) {
        self.watch().frame(now);
    }

    pub(crate) fn suspect(&self) {
        self.watch().suspect();
    }

    /// Called before a requested stop; waits for a report in progress, and no lookup reports anything afterwards.
    pub(crate) fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        drop(
            self.reporting
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
    }

    /// Starts a due lookup unless one is running, from one thread only; `gone` hears [`SOURCE_GONE`] once the target is confirmed missing.
    pub(crate) fn poll(self: &Arc<Self>, now: Instant, gone: impl FnOnce(String) + Send + 'static) {
        if self.stopping.load(Ordering::SeqCst)
            || self.in_flight.load(Ordering::Relaxed)
            || !self.watch().check_due(now)
        {
            return;
        }
        self.in_flight.store(true, Ordering::Relaxed);
        let presence = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("sck-presence".to_owned())
            .spawn(move || {
                // A panicking lookup counts as failed; the panic hook has already reported it.
                let present =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (presence.lookup)()))
                        .unwrap_or(None);
                presence.in_flight.store(false, Ordering::Relaxed);
                let _reporting = presence
                    .reporting
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if present == Some(false) && !presence.stopping.load(Ordering::SeqCst) {
                    gone(SOURCE_GONE.to_owned());
                }
            });
        if spawned.is_err() {
            self.in_flight.store(false, Ordering::Relaxed);
        }
    }

    fn watch(&self) -> std::sync::MutexGuard<'_, PresenceWatch> {
        self.watch.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookups_start_after_idle_and_are_spaced() {
        let start = Instant::now();
        let mut watch = PresenceWatch::new(start);
        assert!(!watch.check_due(start + PRESENCE_CHECK_IDLE / 2));
        let idle = start + PRESENCE_CHECK_IDLE;
        assert!(watch.check_due(idle));
        assert!(!watch.check_due(idle + PRESENCE_CHECK_INTERVAL / 2));
        assert!(watch.check_due(idle + PRESENCE_CHECK_INTERVAL));
    }

    #[test]
    fn a_new_frame_resets_the_watch() {
        let start = Instant::now();
        let mut watch = PresenceWatch::new(start);
        let idle = start + PRESENCE_CHECK_IDLE;
        assert!(watch.check_due(idle));
        watch.frame(idle);
        assert!(!watch.check_due(idle + PRESENCE_CHECK_IDLE / 2));
        assert!(watch.check_due(idle + PRESENCE_CHECK_IDLE));
    }

    #[test]
    fn a_suspect_frame_makes_a_lookup_due_at_once() {
        let start = Instant::now();
        let mut watch = PresenceWatch::new(start);
        watch.suspect();
        assert!(watch.check_due(start));
        assert!(!watch.check_due(start));
    }

    struct Probe {
        presence: Arc<Presence>,
        lookups: Arc<std::sync::atomic::AtomicUsize>,
        gone: std::sync::mpsc::Receiver<String>,
        tx: std::sync::mpsc::Sender<String>,
    }

    fn probe(answer: Option<bool>, hold: Option<std::sync::mpsc::Receiver<()>>) -> Probe {
        let lookups = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&lookups);
        let hold = Mutex::new(hold);
        let presence = Presence::new(
            Box::new(move || {
                counted.fetch_add(1, Ordering::Relaxed);
                if let Some(hold) = hold.lock().unwrap().as_ref() {
                    hold.recv().unwrap();
                }
                answer
            }),
            Instant::now()
                .checked_sub(PRESENCE_CHECK_IDLE)
                .unwrap_or_else(Instant::now),
        );
        let (tx, gone) = std::sync::mpsc::channel();
        Probe {
            presence,
            lookups,
            gone,
            tx,
        }
    }

    impl Probe {
        fn poll(&self, now: Instant) {
            let tx = self.tx.clone();
            self.presence
                .poll(now, move |message| tx.send(message).unwrap());
        }

        fn wait_lookups(&self, n: usize) {
            let deadline = Instant::now() + Duration::from_secs(2);
            while self.lookups.load(Ordering::Relaxed) < n && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(self.lookups.load(Ordering::Relaxed), n);
        }
    }

    #[test]
    fn a_missing_target_ends_the_capture_saying_it_went_away() {
        let probe = probe(Some(false), None);
        probe.poll(Instant::now());
        let message = probe.gone.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(message, SOURCE_GONE);
    }

    #[test]
    fn a_listed_target_or_a_failed_lookup_keeps_the_capture() {
        for answer in [Some(true), None] {
            let probe = probe(answer, None);
            probe.poll(Instant::now());
            probe.wait_lookups(1);
            assert!(probe.gone.recv_timeout(Duration::from_millis(100)).is_err());
        }
    }

    #[test]
    fn a_requested_stop_waits_for_a_report_in_progress_and_wins_after() {
        let (entered, in_gone) = std::sync::mpsc::channel();
        let (finish, finished) = std::sync::mpsc::channel::<()>();
        let probe = probe(Some(false), None);
        probe.presence.poll(Instant::now(), move |_| {
            entered.send(()).unwrap();
            finished.recv().unwrap();
        });
        in_gone.recv_timeout(Duration::from_secs(2)).unwrap();
        let (stopped_tx, stopped) = std::sync::mpsc::channel();
        let presence = Arc::clone(&probe.presence);
        std::thread::spawn(move || {
            presence.stop();
            stopped_tx.send(()).unwrap();
        });
        assert!(
            stopped.recv_timeout(Duration::from_millis(100)).is_err(),
            "stop returned while the report was still running"
        );
        finish.send(()).unwrap();
        stopped.recv_timeout(Duration::from_secs(2)).unwrap();
        probe.presence.suspect();
        probe.poll(Instant::now());
        assert!(probe.gone.recv_timeout(Duration::from_millis(100)).is_err());
        assert_eq!(probe.lookups.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn nothing_is_reported_while_stopping() {
        let probe = probe(Some(false), None);
        probe.presence.stop();
        probe.poll(Instant::now());
        assert!(probe.gone.recv_timeout(Duration::from_millis(100)).is_err());
        assert_eq!(probe.lookups.load(Ordering::Relaxed), 0);

        let (release, hold) = std::sync::mpsc::channel();
        let probe = self::probe(Some(false), Some(hold));
        probe.poll(Instant::now());
        probe.wait_lookups(1);
        probe.presence.stop();
        release.send(()).unwrap();
        assert!(probe.gone.recv_timeout(Duration::from_millis(100)).is_err());
    }

    #[test]
    fn a_slow_lookup_never_blocks_the_caller_and_runs_alone() {
        let (release, hold) = std::sync::mpsc::channel();
        let probe = probe(Some(false), Some(hold));
        let start = Instant::now();
        let (returned, poll_returned) = std::sync::mpsc::channel();
        let presence = Arc::clone(&probe.presence);
        let tx = probe.tx.clone();
        std::thread::spawn(move || {
            presence.poll(start, move |message| tx.send(message).unwrap());
            returned.send(()).unwrap();
        });
        poll_returned
            .recv_timeout(Duration::from_secs(10))
            .expect("poll waited for the lookup");
        probe.wait_lookups(1);
        probe.presence.suspect();
        probe.poll(start + PRESENCE_CHECK_INTERVAL);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(probe.lookups.load(Ordering::Relaxed), 1);
        release.send(()).unwrap();
        assert_eq!(
            probe.gone.recv_timeout(Duration::from_secs(2)).unwrap(),
            SOURCE_GONE
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while probe.presence.in_flight.load(Ordering::Relaxed) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        release.send(()).unwrap();
        probe.poll(start + PRESENCE_CHECK_INTERVAL);
        probe.wait_lookups(2);
    }

    #[test]
    fn a_panicking_lookup_does_not_stop_later_checks() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let presence = Presence::new(
            Box::new(move || {
                if counted.fetch_add(1, Ordering::Relaxed) == 0 {
                    panic!("lookup failed");
                }
                Some(false)
            }),
            Instant::now()
                .checked_sub(PRESENCE_CHECK_IDLE)
                .unwrap_or_else(Instant::now),
        );
        let (tx, gone) = std::sync::mpsc::channel();
        let start = Instant::now();
        let first = tx.clone();
        presence.poll(start, move |m| first.send(m).unwrap());
        let deadline = Instant::now() + Duration::from_secs(2);
        while (calls.load(Ordering::Relaxed) == 0 || presence.in_flight.load(Ordering::Relaxed))
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        presence.poll(start + PRESENCE_CHECK_INTERVAL, move |m| {
            tx.send(m).unwrap()
        });
        assert_eq!(
            gone.recv_timeout(Duration::from_secs(2)).unwrap(),
            SOURCE_GONE
        );
    }
}
