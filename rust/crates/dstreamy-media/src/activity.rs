use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Samples at or below this magnitude count as silence: about -60 dBFS.
pub const SILENCE_PEAK: u16 = 32;

const IDLE: u64 = u64::MAX;

/// The tracing target of a capture's audio device loss and return, which `dstreamy stream` reports itself.
pub const AUDIO_DEVICE_TARGET: &str = "dstreamy::audio_device";

/// When captured audio last carried sound; every clone observes the same capture.
#[derive(Debug, Clone)]
pub struct AudioActivity(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    epoch: Instant,
    /// Milliseconds after `epoch` since which the audio has been silent; `IDLE` while not capturing.
    silent_since_ms: AtomicU64,
    /// The capture's audio device went away and has not delivered samples since.
    lost: AtomicBool,
    /// Losses so far, so a reader polling now and then sees a loss that already ended.
    losses: AtomicU64,
    /// Returns of a lost device, counted like `losses`.
    recoveries: AtomicU64,
}

impl Default for AudioActivity {
    fn default() -> Self {
        Self(Arc::new(Inner {
            epoch: Instant::now(),
            silent_since_ms: AtomicU64::new(IDLE),
            lost: AtomicBool::new(false),
            losses: AtomicU64::new(0),
            recoveries: AtomicU64::new(0),
        }))
    }
}

impl AudioActivity {
    pub fn started(&self) {
        self.started_at(Instant::now());
    }

    pub fn stopped(&self) {
        self.0.silent_since_ms.store(IDLE, Ordering::Relaxed);
        self.0.lost.store(false, Ordering::Relaxed);
    }

    /// The audio device went away; cleared by the next samples, from the same device once it is back.
    pub fn lost(&self) {
        self.0.losses.fetch_add(1, Ordering::Relaxed);
        self.0.lost.store(true, Ordering::Relaxed);
    }

    /// How many times the device was lost since this record was made; it never goes down.
    pub fn losses(&self) -> u64 {
        self.0.losses.load(Ordering::Relaxed)
    }

    /// How many times a lost device delivered samples again.
    pub fn recoveries(&self) -> u64 {
        self.0.recoveries.load(Ordering::Relaxed)
    }

    pub fn is_lost(&self) -> bool {
        self.0.lost.load(Ordering::Relaxed)
    }

    /// Records a block of interleaved samples; sound restarts the silence clock.
    pub fn samples(&self, samples: impl IntoIterator<Item = i16>) {
        self.samples_at(samples, Instant::now());
    }

    /// How long the audio has been silent; `None` while not capturing.
    pub fn silent_for(&self) -> Option<Duration> {
        self.silent_for_at(Instant::now())
    }

    fn ms(&self, now: Instant) -> u64 {
        u64::try_from(now.saturating_duration_since(self.0.epoch).as_millis()).unwrap_or(IDLE - 1)
    }

    fn started_at(&self, now: Instant) {
        self.0.lost.store(false, Ordering::Relaxed);
        self.0
            .silent_since_ms
            .store(self.ms(now), Ordering::Relaxed);
    }

    fn samples_at(&self, samples: impl IntoIterator<Item = i16>, now: Instant) {
        if self.0.lost.swap(false, Ordering::Relaxed) {
            self.0.recoveries.fetch_add(1, Ordering::Relaxed);
        }
        if samples.into_iter().any(|s| s.unsigned_abs() > SILENCE_PEAK) {
            self.heard_at(now);
        }
    }

    fn heard_at(&self, now: Instant) {
        let now = self.ms(now);
        let _ = self
            .0
            .silent_since_ms
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |since| {
                (since != IDLE).then_some(now)
            });
    }

    fn silent_for_at(&self, now: Instant) -> Option<Duration> {
        match self.0.silent_since_ms.load(Ordering::Relaxed) {
            IDLE => None,
            since => Some(Duration::from_millis(self.ms(now).saturating_sub(since))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn unknown_until_capture_starts_and_after_it_stops() {
        let a = AudioActivity::default();
        assert_eq!(a.silent_for(), None);
        a.samples([10_000]);
        assert_eq!(a.silent_for(), None);
        a.started();
        assert!(a.silent_for().is_some());
        a.stopped();
        assert_eq!(a.silent_for(), None);
    }

    #[test]
    fn silence_counts_from_the_start_and_sound_resets_it() {
        let a = AudioActivity::default();
        let t0 = a.0.epoch;
        a.started_at(t0);
        assert_eq!(a.silent_for_at(t0 + 5 * S), Some(5 * S));
        a.heard_at(t0 + 5 * S);
        assert_eq!(a.silent_for_at(t0 + 7 * S), Some(2 * S));
    }

    #[test]
    fn only_samples_above_the_threshold_count_as_sound() {
        let a = AudioActivity::default();
        let t0 = a.0.epoch;
        a.started_at(t0);
        a.samples_at([0, 32, -32], t0 + S);
        assert_eq!(a.silent_for_at(t0 + 2 * S), Some(2 * S));
        a.samples_at([0, -33], t0 + 2 * S);
        assert_eq!(a.silent_for_at(t0 + 3 * S), Some(S));
        a.samples_at([i16::MIN], t0 + 3 * S);
        assert_eq!(a.silent_for_at(t0 + 3 * S), Some(Duration::ZERO));
    }

    #[test]
    fn a_lost_device_stays_lost_until_samples_arrive_even_silent_ones() {
        let a = AudioActivity::default();
        a.started();
        assert!(!a.is_lost());
        a.lost();
        assert!(a.is_lost());
        a.samples([0, 0]);
        assert!(!a.is_lost());
        a.lost();
        a.stopped();
        assert!(!a.is_lost());
        assert_eq!(
            a.losses(),
            2,
            "a loss stays counted after the device is back"
        );
        assert_eq!(a.recoveries(), 1, "stopping is not a return");
    }

    #[test]
    fn clones_share_the_record() {
        let a = AudioActivity::default();
        let b = a.clone();
        a.started();
        assert!(b.silent_for().is_some());
    }
}
