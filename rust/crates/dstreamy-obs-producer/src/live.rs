//! The dock's live line: rates from D-Streamy's OBS output counters, sampled each time the dock polls.

use std::collections::VecDeque;

/// Rates span about this long, so they don't jump with every poll.
pub const WINDOW_MS: u64 = 2500;
/// Before this much time has passed since the first sample, only the uptime is shown.
pub const MIN_SPAN_MS: u64 = 1000;

/// The output's counters at one poll; `at_ms` is any monotonic clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputSample {
    pub at_ms: u64,
    pub bytes: u64,
    pub frames: u64,
    pub dropped: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rates {
    pub bits_per_sec: f64,
    pub fps: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LiveStats {
    pub uptime_ms: u64,
    pub rates: Option<Rates>,
    pub dropped: u64,
    /// From the service; `None` until Discord reports the viewers.
    pub viewers: Option<u32>,
}

#[derive(Debug, Default)]
pub struct Sampler {
    started_ms: Option<u64>,
    samples: VecDeque<OutputSample>,
}

impl Sampler {
    /// The output stopped: the next sample starts a new uptime.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn push(&mut self, sample: OutputSample) -> LiveStats {
        let restarted = self.samples.back().is_some_and(|last| {
            sample.at_ms < last.at_ms || sample.bytes < last.bytes || sample.frames < last.frames
        });
        if restarted {
            self.reset();
        }
        let started = *self.started_ms.get_or_insert(sample.at_ms);
        self.samples.push_back(sample);
        while self.samples.len() > 2 && sample.at_ms - self.samples[1].at_ms >= WINDOW_MS {
            self.samples.pop_front();
        }
        let oldest = self.samples[0];
        let span = sample.at_ms - oldest.at_ms;
        let rates = (span >= MIN_SPAN_MS).then(|| Rates {
            bits_per_sec: (sample.bytes - oldest.bytes) as f64 * 8000.0 / span as f64,
            fps: (sample.frames - oldest.frames) as f64 * 1000.0 / span as f64,
        });
        LiveStats {
            uptime_ms: sample.at_ms - started,
            rates,
            dropped: sample.dropped,
            viewers: None,
        }
    }
}

fn bitrate(bits_per_sec: f64) -> String {
    let kbps = (bits_per_sec / 1000.0).round();
    if kbps >= 1000.0 {
        format!("{:.1} Mbit/s", bits_per_sec / 1_000_000.0)
    } else {
        format!("{kbps:.0} kbit/s")
    }
}

fn clock(ms: u64) -> String {
    let secs = ms / 1000;
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// "6.0 Mbit/s · 60 fps · 12:34 · 👁 3", "… · ⚠ 3" once frames drop, or just the uptime until rates exist.
pub fn stats_line(stats: &LiveStats) -> String {
    line(stats, stats.dropped > 0, Words::Symbols)
}

/// Like [`stats_line`] in words, with the dropped count even when it is zero.
pub fn stats_tooltip(stats: &LiveStats) -> String {
    line(stats, true, Words::Full)
}

#[derive(Clone, Copy, PartialEq)]
enum Words {
    Symbols,
    Full,
}

fn line(stats: &LiveStats, dropped: bool, words: Words) -> String {
    let mut text = clock(stats.uptime_ms);
    if let Some(r) = stats.rates {
        text = format!("{} · {:.0} fps · {text}", bitrate(r.bits_per_sec), r.fps);
    }
    if let Some(n) = stats.viewers {
        text.push_str(&match words {
            Words::Symbols => format!(" · 👁 {n}"),
            Words::Full => format!(" · {n} watching"),
        });
    }
    if dropped && stats.rates.is_some() {
        text.push_str(&match words {
            Words::Symbols => format!(" · ⚠ {}", stats.dropped),
            Words::Full => format!(" · {} dropped", stats.dropped),
        });
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(at_ms: u64, bytes: u64, frames: u64, dropped: u64) -> OutputSample {
        OutputSample {
            at_ms,
            bytes,
            frames,
            dropped,
        }
    }

    #[test]
    fn the_first_samples_show_only_the_uptime() {
        let mut s = Sampler::default();
        let first = s.push(at(10_000, 0, 0, 0));
        assert_eq!(first.rates, None);
        assert_eq!(stats_line(&first), "0:00");
        let early = s.push(at(10_500, 375_000, 30, 0));
        assert_eq!(early.rates, None, "half a second is too short");
        assert_eq!(stats_line(&early), "0:00");
    }

    #[test]
    fn rates_come_from_deltas_over_the_window() {
        let mut s = Sampler::default();
        let mut stats = None;
        for i in 0..=8u64 {
            stats = Some(s.push(at(i * 500, i * 375_000, i * 30, 2)));
        }
        let stats = stats.unwrap();
        let r = stats.rates.unwrap();
        assert!((r.bits_per_sec - 6_000_000.0).abs() < 1.0, "{r:?}");
        assert!((r.fps - 60.0).abs() < 0.01, "{r:?}");
        assert_eq!(stats.uptime_ms, 4000);
        assert_eq!(stats_line(&stats), "6.0 Mbit/s · 60 fps · 0:04 · ⚠ 2");
        assert_eq!(
            stats_tooltip(&stats),
            "6.0 Mbit/s · 60 fps · 0:04 · 2 dropped"
        );
    }

    #[test]
    fn the_window_forgets_older_samples() {
        let mut s = Sampler::default();
        s.push(at(0, 0, 0, 0));
        s.push(at(500, 10_000_000, 600, 0));
        for i in 2..=10u64 {
            s.push(at(
                i * 500,
                10_000_000 + (i - 1) * 62_500,
                600 + (i - 1) * 15,
                0,
            ));
        }
        let r = s
            .push(at(5500, 10_000_000 + 10 * 62_500, 600 + 150, 0))
            .rates
            .unwrap();
        assert!(
            (r.bits_per_sec - 1_000_000.0).abs() < 1.0,
            "the early burst is gone: {r:?}"
        );
        assert!((r.fps - 30.0).abs() < 0.01, "{r:?}");
    }

    #[test]
    fn a_restarted_output_starts_a_new_uptime() {
        let mut s = Sampler::default();
        s.push(at(0, 0, 0, 0));
        s.push(at(60_000, 9_000_000, 3600, 5));
        let after = s.push(at(61_000, 1000, 1, 0));
        assert_eq!(after.uptime_ms, 0);
        assert_eq!(after.rates, None);
        s.reset();
        assert_eq!(s.push(at(70_000, 0, 0, 0)).uptime_ms, 0);
    }

    #[test]
    fn units_and_clock_formats() {
        assert_eq!(bitrate(6_000_000.0), "6.0 Mbit/s");
        assert_eq!(bitrate(12_345_678.0), "12.3 Mbit/s");
        assert_eq!(bitrate(999_400.0), "999 kbit/s");
        assert_eq!(bitrate(999_600.0), "1.0 Mbit/s");
        assert_eq!(bitrate(640_000.0), "640 kbit/s");
        assert_eq!(bitrate(0.0), "0 kbit/s");
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(754_000), "12:34");
        assert_eq!(clock(3_599_999), "59:59");
        assert_eq!(clock(3_600_000), "1:00:00");
        assert_eq!(clock(37_384_000), "10:23:04");
        let stats = LiveStats {
            uptime_ms: 3_725_000,
            rates: Some(Rates {
                bits_per_sec: 2_500_000.0,
                fps: 29.97,
            }),
            dropped: 0,
            viewers: None,
        };
        assert_eq!(stats_line(&stats), "2.5 Mbit/s · 30 fps · 1:02:05");
        assert_eq!(
            stats_tooltip(&stats),
            "2.5 Mbit/s · 30 fps · 1:02:05 · 0 dropped"
        );
    }

    #[test]
    fn viewers_show_once_known() {
        let mut stats = LiveStats {
            uptime_ms: 754_000,
            rates: Some(Rates {
                bits_per_sec: 6_000_000.0,
                fps: 60.0,
            }),
            dropped: 2,
            viewers: Some(3),
        };
        assert_eq!(
            stats_line(&stats),
            "6.0 Mbit/s · 60 fps · 12:34 · 👁 3 · ⚠ 2"
        );
        stats.viewers = Some(0);
        stats.dropped = 0;
        assert_eq!(stats_line(&stats), "6.0 Mbit/s · 60 fps · 12:34 · 👁 0");
        assert_eq!(
            stats_tooltip(&stats),
            "6.0 Mbit/s · 60 fps · 12:34 · 0 watching · 0 dropped"
        );
        stats.rates = None;
        stats.viewers = Some(1);
        assert_eq!(stats_line(&stats), "12:34 · 👁 1");
        stats.viewers = None;
        assert_eq!(stats_line(&stats), "12:34");
    }
}
