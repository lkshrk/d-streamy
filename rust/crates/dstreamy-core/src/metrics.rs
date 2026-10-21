use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameIntervalSnapshot {
    /// Frames recorded in the interval (not gaps between them).
    pub samples: u64,
    pub p95_ms: u64,
    pub max_ms: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct FrameIntervalTracker {
    gaps: Vec<Duration>,
    frames: u64,
    last: Option<Instant>,
    truncated: bool,
    max_samples: usize,
}

impl Default for FrameIntervalTracker {
    fn default() -> Self {
        Self::new(Self::DEFAULT_MAX_SAMPLES)
    }
}

impl FrameIntervalTracker {
    pub const DEFAULT_MAX_SAMPLES: usize = 120;

    pub fn new(max_samples: usize) -> Self {
        Self {
            gaps: Vec::with_capacity(max_samples),
            frames: 0,
            last: None,
            truncated: false,
            max_samples,
        }
    }

    pub fn record(&mut self, ts: Instant) {
        self.frames += 1;
        if let Some(last) = self.last {
            if self.gaps.len() < self.max_samples {
                self.gaps.push(ts.saturating_duration_since(last));
            } else {
                self.truncated = true;
            }
        }
        self.last = Some(ts);
    }

    /// Returns the interval's statistics and starts a new interval; no gap spans a drain.
    pub fn drain(&mut self) -> FrameIntervalSnapshot {
        let snapshot = self.compute();
        self.gaps.clear();
        self.frames = 0;
        self.last = None;
        self.truncated = false;
        snapshot
    }

    fn compute(&mut self) -> FrameIntervalSnapshot {
        let mut snapshot = FrameIntervalSnapshot {
            samples: self.frames,
            truncated: self.truncated,
            ..FrameIntervalSnapshot::default()
        };
        if self.gaps.is_empty() {
            return snapshot;
        }
        self.gaps.sort_unstable();
        let n = self.gaps.len();
        let idx = ((0.95 * n as f64).ceil() as usize)
            .saturating_sub(1)
            .min(n - 1);
        snapshot.p95_ms = round_ms(self.gaps[idx]);
        snapshot.max_ms = round_ms(self.gaps[n - 1]);
        snapshot
    }
}

fn round_ms(d: Duration) -> u64 {
    (d.as_secs_f64() * 1000.0).round() as u64
}

/// Cumulative counters reported by the media pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PipelineStats {
    pub frame_count: u64,
    pub bytes_sent: u64,
    pub audio_frames_encoded: u64,
    pub audio_frames_sent: u64,
    pub audio_frames_dropped_not_ready: u64,
    pub audio_encode_errors: u64,
}

/// Per-interval stream metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct StreamMetrics {
    pub fps: u64,
    pub gap_p95_ms: u64,
    pub gap_max_ms: u64,
    pub gap_trunc: bool,
    pub bitrate_kbps: u64,
    pub v_drop: u64,
    pub a_enc: u64,
    pub a_sent: u64,
    pub a_drop_not_ready: u64,
    pub a_enc_err: u64,
}

/// Turns cumulative [`PipelineStats`] into per-interval [`StreamMetrics`].
#[derive(Debug, Clone, Default)]
pub struct MetricsSampler {
    prev: Option<(PipelineStats, Instant)>,
}

impl MetricsSampler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.prev = None;
    }

    /// The first call after construction or [`reset`](Self::reset) only seeds the baseline.
    pub fn sample(
        &mut self,
        now: Instant,
        cur: PipelineStats,
        jitter: FrameIntervalSnapshot,
    ) -> Option<StreamMetrics> {
        let (prev, prev_at) = self.prev.replace((cur, now))?;

        let dt_sec = now
            .saturating_duration_since(prev_at)
            .as_secs_f64()
            .max(0.001);
        let d_frames = cur.frame_count.saturating_sub(prev.frame_count);
        let d_bytes = cur.bytes_sent.saturating_sub(prev.bytes_sent);
        let sent = jitter.samples;

        Some(StreamMetrics {
            fps: (sent as f64 / dt_sec).round() as u64,
            gap_p95_ms: jitter.p95_ms,
            gap_max_ms: jitter.max_ms,
            gap_trunc: jitter.truncated,
            bitrate_kbps: (d_bytes as f64 * 8.0 / dt_sec / 1000.0).round() as u64,
            v_drop: d_frames.saturating_sub(sent),
            a_enc: cur
                .audio_frames_encoded
                .saturating_sub(prev.audio_frames_encoded),
            a_sent: cur.audio_frames_sent.saturating_sub(prev.audio_frames_sent),
            a_drop_not_ready: cur
                .audio_frames_dropped_not_ready
                .saturating_sub(prev.audio_frames_dropped_not_ready),
            a_enc_err: cur
                .audio_encode_errors
                .saturating_sub(prev.audio_encode_errors),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn samples_counts_frames_not_gaps() {
        let base = Instant::now();
        let mut t = FrameIntervalTracker::new(120);
        t.record(at(base, 1000));
        t.record(at(base, 1020));
        t.record(at(base, 1050));
        let s = t.drain();
        assert_eq!(s.samples, 3);
        assert_eq!(s.max_ms, 30);
        assert!(!s.truncated);
    }

    #[test]
    fn single_frame_counts_as_one_sample_with_no_gap() {
        let mut t = FrameIntervalTracker::new(120);
        t.record(Instant::now());
        let s = t.drain();
        assert_eq!(s.samples, 1);
        assert_eq!(s.p95_ms, 0);
        assert_eq!(s.max_ms, 0);
    }

    #[test]
    fn p95_picks_the_high_gap() {
        let base = Instant::now();
        let mut t = FrameIntervalTracker::default();
        let mut ts = 0;
        for _ in 0..100 {
            ts += 10;
            t.record(at(base, ts));
        }
        ts += 200;
        t.record(at(base, ts));
        let s = t.drain();
        assert_eq!(s.p95_ms, 10);
        assert_eq!(s.max_ms, 200);
    }

    #[test]
    fn drain_resets_state_and_gap_does_not_span_drains() {
        let base = Instant::now();
        let mut t = FrameIntervalTracker::new(120);
        t.record(at(base, 1000));
        t.record(at(base, 1010));
        t.drain();
        t.record(at(base, 5000));
        t.record(at(base, 5010));
        let s = t.drain();
        assert_eq!(s.samples, 2);
        assert_eq!(s.max_ms, 10);
    }

    #[test]
    fn exceeding_cap_truncates_gaps_but_counts_all_frames() {
        let base = Instant::now();
        let mut t = FrameIntervalTracker::new(3);
        for i in 1..=10 {
            t.record(at(base, i * 10));
        }
        let s = t.drain();
        assert!(s.truncated);
        assert_eq!(s.samples, 10);
    }

    #[test]
    fn perfect_stream_reports_full_fps_and_no_drops() {
        let base = Instant::now();
        let mut tracker = FrameIntervalTracker::default();
        let mut sampler = MetricsSampler::new();
        assert!(
            sampler
                .sample(base, PipelineStats::default(), tracker.drain())
                .is_none()
        );
        for i in 0..30 {
            tracker.record(at(base, i * 33));
        }
        let stats = PipelineStats {
            frame_count: 30,
            ..PipelineStats::default()
        };
        let m = sampler
            .sample(at(base, 1000), stats, tracker.drain())
            .unwrap();
        assert_eq!(m.fps, 30);
        assert_eq!(m.v_drop, 0);
    }

    fn jitter(samples: u64, p95_ms: u64, max_ms: u64) -> FrameIntervalSnapshot {
        FrameIntervalSnapshot {
            samples,
            p95_ms,
            max_ms,
            truncated: false,
        }
    }

    #[test]
    fn first_sample_returns_none() {
        let mut sampler = MetricsSampler::new();
        let r = sampler.sample(
            Instant::now(),
            PipelineStats::default(),
            FrameIntervalSnapshot::default(),
        );
        assert!(r.is_none());
    }

    #[test]
    fn second_sample_computes_per_interval_deltas_over_elapsed_time() {
        let base = Instant::now();
        let mut sampler = MetricsSampler::new();
        sampler.sample(base, PipelineStats::default(), jitter(28, 34, 120));
        let stats = PipelineStats {
            frame_count: 30,
            bytes_sent: 250_000,
            audio_frames_encoded: 50,
            audio_frames_sent: 49,
            audio_frames_dropped_not_ready: 1,
            ..PipelineStats::default()
        };

        let m = sampler
            .sample(at(base, 1000), stats, jitter(28, 34, 120))
            .unwrap();
        assert_eq!(m.fps, 28);
        assert_eq!(m.v_drop, 2);
        assert_eq!(m.bitrate_kbps, 2000);
        assert_eq!(m.gap_p95_ms, 34);
        assert_eq!(m.gap_max_ms, 120);
        assert_eq!(m.a_enc, 50);
        assert_eq!(m.a_sent, 49);
        assert_eq!(m.a_drop_not_ready, 1);
    }

    #[test]
    fn reset_reseeds_baseline() {
        let base = Instant::now();
        let mut sampler = MetricsSampler::new();
        let stats = PipelineStats::default();
        let j = FrameIntervalSnapshot::default();
        sampler.sample(at(base, 1000), stats, j);
        assert!(sampler.sample(at(base, 2000), stats, j).is_some());
        sampler.reset();
        assert!(sampler.sample(at(base, 3000), stats, j).is_none());
    }

    #[test]
    fn zero_elapsed_time_is_clamped() {
        let base = Instant::now();
        let mut sampler = MetricsSampler::new();
        let j = jitter(1, 0, 0);
        sampler.sample(base, PipelineStats::default(), j);
        let m = sampler.sample(base, PipelineStats::default(), j).unwrap();
        assert_eq!(m.fps, 1000);
    }
}
