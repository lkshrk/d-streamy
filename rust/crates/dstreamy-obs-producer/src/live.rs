//! The status bar's live figures: rates from D-Streamy's OBS output counters, sampled each time the panel polls, and the service's health.

use std::collections::VecDeque;

use serde_json::Value;

/// Rates span about this long, so they don't jump with every poll.
pub const WINDOW_MS: u64 = 2500;
/// Before this much time has passed since the first sample, only the uptime is shown.
pub const MIN_SPAN_MS: u64 = 1000;
/// Missed frames this recent tint the status bar's counter.
pub const MISSED_WINDOW_MS: u64 = 10_000;
/// Reconnects this recent tint the status bar's counter.
pub const RECONNECT_WINDOW_MS: u64 = 60_000;
/// Audio dropouts this recent tint the status bar's counter.
pub const AUDIO_DROPOUT_WINDOW_MS: u64 = 10_000;

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

/// serve's rating of the last ten seconds of video or audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceLevel {
    Good,
    Warn,
    Bad,
}

impl ServiceLevel {
    fn parse(value: &Value) -> Option<Self> {
        match value.as_str()? {
            "good" => Some(Self::Good),
            "warn" => Some(Self::Warn),
            "bad" => Some(Self::Bad),
            _ => None,
        }
    }
}

/// serve's `health` event (ADR 0004); the last interval's figures are `None` until serve has one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServiceHealth {
    pub reconnecting: bool,
    pub reconnects: u64,
    /// Video frames serve lost after the plugin sent them.
    pub dropped_video: u64,
    pub audio_per_sec: Option<u64>,
    /// serve's intervals of this stream with too few audio frames; `None` before the first, or from an older serve.
    pub audio_dropouts: Option<u64>,
    pub audio_level: Option<ServiceLevel>,
    pub video_level: Option<ServiceLevel>,
    pub fps: Option<u64>,
    pub jitter_p95_ms: Option<u64>,
    pub jitter_max_ms: Option<u64>,
    pub bitrate_kbps: Option<u64>,
}

impl ServiceHealth {
    /// `None` for an event without the counters, which serve sends while no stream runs.
    pub fn from_event(event: &Value) -> Option<Self> {
        Some(Self {
            reconnecting: event["reconnecting"].as_bool()?,
            reconnects: event["reconnects"].as_u64()?,
            dropped_video: event["dropped_video"].as_u64().unwrap_or(0),
            audio_per_sec: event["audio_per_sec"].as_u64(),
            audio_dropouts: event["audio_dropouts"].as_u64(),
            audio_level: ServiceLevel::parse(&event["audio_level"]),
            video_level: ServiceLevel::parse(&event["video_level"]),
            fps: event["fps"].as_u64(),
            jitter_p95_ms: event["jitter_p95_ms"].as_u64(),
            jitter_max_ms: event["jitter_max_ms"].as_u64(),
            bitrate_kbps: event["bitrate_kbps"].as_u64(),
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LiveStats {
    pub uptime_ms: u64,
    pub rates: Option<Rates>,
    /// Video packets D-Streamy's output dropped because the service fell behind.
    pub dropped: u64,
    /// From the service; `None` until Discord reports the viewers.
    pub viewers: Option<u32>,
    /// `None` until the service reports health; a service from before the event never does.
    pub service: Option<ServiceHealth>,
    /// [`LiveStats::missed`] gained this many in the last [`MISSED_WINDOW_MS`].
    pub missed_recent: u64,
    /// Reconnects in the last [`RECONNECT_WINDOW_MS`].
    pub reconnects_recent: u64,
    /// Audio dropouts in the last [`AUDIO_DROPOUT_WINDOW_MS`].
    pub audio_dropouts_recent: u64,
    /// The session's frame rate, filled in by the panel.
    pub target_fps: Option<u32>,
    /// Whether any audio source feeds the Discord track, filled in by the panel; `None` where unknown.
    pub audio_fed: Option<bool>,
    /// The one-based track Discord hears, filled in by the panel.
    pub heard_track: Option<u8>,
}

impl LiveStats {
    /// Frames viewers miss: dropped by the output in OBS plus lost in the service.
    pub fn missed(&self) -> u64 {
        self.dropped + self.service.map_or(0, |s| s.dropped_video)
    }
}

/// How much a running total grew within the last `WINDOW` ms.
#[derive(Debug, Default)]
struct Recent<const WINDOW: u64> {
    points: VecDeque<(u64, u64)>,
}

impl<const WINDOW: u64> Recent<WINDOW> {
    fn push(&mut self, at_ms: u64, total: u64) -> u64 {
        match self.points.back() {
            Some(&(_, last)) if total >= last => {}
            _ => {
                self.points.clear();
                self.points.push_back((at_ms, total));
            }
        }
        self.points.push_back((at_ms, total));
        while self.points.len() > 2 && at_ms.saturating_sub(self.points[1].0) >= WINDOW {
            self.points.pop_front();
        }
        total - self.points[0].1
    }
}

#[derive(Debug, Default)]
pub struct Sampler {
    started_ms: Option<u64>,
    samples: VecDeque<OutputSample>,
    missed: Recent<MISSED_WINDOW_MS>,
    reconnects: Recent<RECONNECT_WINDOW_MS>,
    audio_dropouts: Recent<AUDIO_DROPOUT_WINDOW_MS>,
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
            ..LiveStats::default()
        }
    }

    /// [`Sampler::push`] plus what the service reported, with the recent missed frames, reconnects and audio dropouts.
    pub fn observe(
        &mut self,
        sample: OutputSample,
        viewers: Option<u32>,
        service: Option<ServiceHealth>,
    ) -> LiveStats {
        let mut stats = LiveStats {
            viewers,
            service,
            ..self.push(sample)
        };
        stats.missed_recent = self.missed.push(sample.at_ms, stats.missed());
        stats.reconnects_recent =
            service.map_or(0, |s| self.reconnects.push(sample.at_ms, s.reconnects));
        stats.audio_dropouts_recent = service
            .and_then(|s| s.audio_dropouts)
            .map_or(0, |n| self.audio_dropouts.push(sample.at_ms, n));
        stats
    }
}

pub(crate) fn bitrate(bits_per_sec: f64) -> String {
    let kbps = (bits_per_sec / 1000.0).round();
    if kbps >= 1000.0 {
        format!("{:.1} Mbit/s", bits_per_sec / 1_000_000.0)
    } else {
        format!("{kbps:.0} kbit/s")
    }
}

pub(crate) fn clock(ms: u64) -> String {
    let secs = ms / 1000;
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// "6.0 Mbit/s · 59/60 fps · 12:34" for the panel, or just the uptime until rates exist.
pub fn stats_line(stats: &LiveStats) -> String {
    let uptime = clock(stats.uptime_ms);
    let Some(r) = stats.rates else {
        return uptime;
    };
    let fps = match stats.target_fps {
        Some(target) => format!("{:.0}/{target} fps", r.fps),
        None => format!("{:.0} fps", r.fps),
    };
    format!("{} · {fps} · {uptime}", bitrate(r.bits_per_sec))
}

/// The status bar's figures in words, one per line, for its tooltip and screen readers.
pub fn stats_tooltip(stats: &LiveStats) -> String {
    crate::status_bar::status_lines(stats).join("\n")
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
        assert_eq!(stats.dropped, 2);
        assert_eq!(stats_line(&stats), "6.0 Mbit/s · 60 fps · 0:04");
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
            ..LiveStats::default()
        };
        assert_eq!(stats_line(&stats), "2.5 Mbit/s · 30 fps · 1:02:05");
        let targeted = LiveStats {
            target_fps: Some(30),
            ..stats
        };
        assert_eq!(stats_line(&targeted), "2.5 Mbit/s · 30/30 fps · 1:02:05");
        assert_eq!(
            stats_tooltip(&targeted),
            "Missed frames: 0\nAudio dropouts: unknown\nReconnects: unknown\nViewers: unknown\n\
             Duration: 1:02:05\nVideo: 2.5 Mbit/s, 30 of 30 fps"
        );
    }

    #[test]
    fn service_health_comes_from_its_event() {
        let event = serde_json::json!({
            "type": "health", "reconnecting": true, "reconnects": 2, "dropped_video": 4,
            "audio_per_sec": 49, "audio_dropouts": 3, "audio_level": "warn", "video_level": "bad", "fps": 59,
            "jitter_p95_ms": 18, "jitter_max_ms": 41, "bitrate_kbps": 5800
        });
        let h = ServiceHealth::from_event(&event).unwrap();
        assert!(h.reconnecting);
        assert_eq!(
            (
                h.reconnects,
                h.dropped_video,
                h.audio_per_sec,
                h.audio_dropouts
            ),
            (2, 4, Some(49), Some(3))
        );
        assert_eq!(
            (h.audio_level, h.video_level),
            (Some(ServiceLevel::Warn), Some(ServiceLevel::Bad))
        );
        assert_eq!(
            (h.fps, h.jitter_p95_ms, h.jitter_max_ms, h.bitrate_kbps),
            (Some(59), Some(18), Some(41), Some(5800))
        );
        let early = serde_json::json!({
            "type": "health", "reconnecting": false, "reconnects": 0, "dropped_video": 0,
            "audio_per_sec": null, "audio_level": null, "level": "new"
        });
        let h = ServiceHealth::from_event(&early).unwrap();
        assert_eq!(
            (h.audio_per_sec, h.audio_dropouts, h.audio_level, h.fps),
            (None, None, None, None)
        );
        assert_eq!(
            ServiceHealth::from_event(&serde_json::json!({"type": "health"})),
            None
        );
    }

    #[test]
    fn observe_counts_missed_frames_and_reconnects_within_their_windows() {
        let mut s = Sampler::default();
        let health = |dropped_video, reconnects| {
            Some(ServiceHealth {
                dropped_video,
                reconnects,
                ..ServiceHealth::default()
            })
        };
        let first = s.observe(at(0, 0, 0, 0), Some(2), health(0, 0));
        assert_eq!(
            (first.missed(), first.missed_recent, first.reconnects_recent),
            (0, 0, 0)
        );
        assert_eq!(first.viewers, Some(2));
        let dropped = s.observe(at(1000, 1, 1, 1), None, health(2, 1));
        assert_eq!((dropped.missed(), dropped.missed_recent), (3, 3));
        assert_eq!(dropped.reconnects_recent, 1);
        let mut later = dropped;
        for t in (1500..=11_500).step_by(500) {
            later = s.observe(at(t, t, t, 1), None, health(2, 1));
        }
        assert_eq!(
            (later.missed(), later.missed_recent),
            (3, 0),
            "older than 10 s"
        );
        assert_eq!(later.reconnects_recent, 1, "within the minute");
        for t in (12_000..=62_000).step_by(500) {
            later = s.observe(at(t, t, t, 1), None, health(2, 1));
        }
        assert_eq!(later.reconnects_recent, 0, "a minute later");
        let old = s.observe(at(62_500, 62_500, 62_500, 1), None, None);
        assert_eq!(
            (old.missed(), old.reconnects_recent),
            (1, 0),
            "an older service"
        );
        let back = s.observe(at(63_000, 63_000, 63_000, 1), None, health(0, 0));
        assert_eq!(back.missed_recent, 0, "a lower total starts over");
    }

    #[test]
    fn observe_counts_audio_dropouts_within_ten_seconds() {
        let mut s = Sampler::default();
        let health = |audio_dropouts| {
            Some(ServiceHealth {
                audio_dropouts,
                ..ServiceHealth::default()
            })
        };
        assert_eq!(
            s.observe(at(0, 0, 0, 0), None, health(None))
                .audio_dropouts_recent,
            0
        );
        assert_eq!(
            s.observe(at(500, 1, 1, 0), None, health(Some(0)))
                .audio_dropouts_recent,
            0
        );
        let dropped = s.observe(at(1000, 2, 2, 0), None, health(Some(2)));
        assert_eq!(dropped.audio_dropouts_recent, 2);
        let mut later = dropped;
        for t in (1500..=11_500).step_by(500) {
            later = s.observe(at(t, t, t, 0), None, health(Some(2)));
        }
        assert_eq!(later.audio_dropouts_recent, 0, "older than 10 s");
        let old = s.observe(at(12_000, 12_000, 12_000, 0), None, health(None));
        assert_eq!(old.audio_dropouts_recent, 0, "an older serve");
    }

    #[test]
    fn totals_from_before_a_restart_are_not_recent() {
        let mut s = Sampler::default();
        let health = |reconnects, audio_dropouts| {
            Some(ServiceHealth {
                reconnects,
                audio_dropouts: Some(audio_dropouts),
                ..ServiceHealth::default()
            })
        };
        s.observe(at(0, 0, 0, 0), None, health(1, 1));
        s.observe(at(60_000, 9_000_000, 3600, 0), None, health(3, 4));
        let restarted = s.observe(at(61_000, 1000, 1, 0), None, health(3, 4));
        assert_eq!(
            (restarted.reconnects_recent, restarted.audio_dropouts_recent),
            (0, 0)
        );
        let fresh = Sampler::default().observe(at(5, 0, 0, 0), None, health(3, 4));
        assert_eq!(
            (fresh.reconnects_recent, fresh.audio_dropouts_recent),
            (0, 0)
        );
        let later = s.observe(at(62_000, 2000, 2, 0), None, health(4, 6));
        assert_eq!(
            (later.reconnects_recent, later.audio_dropouts_recent),
            (1, 2)
        );
    }

    #[test]
    fn a_restarted_output_forgets_the_recent_counts() {
        let mut s = Sampler::default();
        s.observe(at(0, 0, 0, 0), None, None);
        assert_eq!(s.observe(at(500, 10, 10, 5), None, None).missed_recent, 5);
        let restarted = s.observe(at(1000, 1, 1, 0), None, None);
        assert_eq!((restarted.uptime_ms, restarted.missed_recent), (0, 0));
    }
}
