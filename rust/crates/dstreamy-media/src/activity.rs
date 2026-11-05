use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Samples at or below this magnitude count as silence: about -60 dBFS.
pub const SILENCE_PEAK: u16 = 32;

const IDLE: u64 = u64::MAX;

/// When captured audio last carried sound; every clone observes the same capture.
#[derive(Debug, Clone)]
pub struct AudioActivity(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    epoch: Instant,
    /// Milliseconds after `epoch` since which the audio has been silent; `IDLE` while not capturing.
    silent_since_ms: AtomicU64,
}

impl Default for AudioActivity {
    fn default() -> Self {
        Self(Arc::new(Inner {
            epoch: Instant::now(),
            silent_since_ms: AtomicU64::new(IDLE),
        }))
    }
}

impl AudioActivity {
    pub fn started(&self) {
        self.started_at(Instant::now());
    }

    pub fn stopped(&self) {
        self.0.silent_since_ms.store(IDLE, Ordering::Relaxed);
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
        self.0
            .silent_since_ms
            .store(self.ms(now), Ordering::Relaxed);
    }

    fn samples_at(&self, samples: impl IntoIterator<Item = i16>, now: Instant) {
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
    fn clones_share_the_record() {
        let a = AudioActivity::default();
        let b = a.clone();
        a.started();
        assert!(b.silent_for().is_some());
    }
}
