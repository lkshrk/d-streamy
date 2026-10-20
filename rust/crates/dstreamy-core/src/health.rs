use crate::metrics::StreamMetrics;

pub const AUDIO_FRAMES_PER_SEC: f64 = 50.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HealthLevel {
    Bad,
    Warn,
    Good,
}

impl HealthLevel {
    pub fn from_score(score: f64) -> Self {
        if score >= 0.9 {
            Self::Good
        } else if score >= 0.6 {
            Self::Warn
        } else {
            Self::Bad
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HealthSample {
    pub fps: u64,
    pub gap_p95_ms: u64,
    pub v_drop: u64,
    pub a_sent: u64,
    pub a_drop_not_ready: u64,
    pub a_enc_err: u64,
}

impl HealthSample {
    /// Video stability for one interval: 1.0 good, 0.5 warn, 0.0 bad.
    pub fn video_score(&self, target_fps: u32) -> f64 {
        let target = f64::from(target_fps.max(1));
        let frame_interval = 1000.0 / target;
        // WebRTC freeze detection: gap >= max(3*frame_dur, frame_dur+150ms)
        let freeze_ms = (3.0 * frame_interval).max(frame_interval + 150.0);

        let fps = self.fps as f64;
        let jitter = self.gap_p95_ms as f64;
        let drop_pct = self.v_drop as f64 / target * 100.0;

        let good_fps = fps >= 0.95 * target;
        let warn_fps = fps >= 0.85 * target;
        let good_jit = jitter <= 2.0 * frame_interval;
        let warn_jit = jitter < freeze_ms;

        if good_fps && good_jit && drop_pct <= 2.0 {
            1.0
        } else if warn_fps && warn_jit && drop_pct <= 5.0 {
            0.5
        } else {
            0.0
        }
    }

    /// Audio stability for one interval: 1.0 good, 0.5 warn, 0.0 bad.
    pub fn audio_score(&self) -> f64 {
        let sent_pct = self.a_sent as f64 / AUDIO_FRAMES_PER_SEC * 100.0;
        if self.a_enc_err == 0 && self.a_drop_not_ready <= 1 && sent_pct >= 96.0 {
            1.0
        } else if self.a_enc_err == 0 && self.a_drop_not_ready <= 3 && sent_pct >= 90.0 {
            0.5
        } else {
            0.0
        }
    }
}

impl From<&StreamMetrics> for HealthSample {
    fn from(m: &StreamMetrics) -> Self {
        Self {
            fps: m.fps,
            gap_p95_ms: m.gap_p95_ms,
            v_drop: m.v_drop,
            a_sent: m.a_sent,
            a_drop_not_ready: m.a_drop_not_ready,
            a_enc_err: m.a_enc_err,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StreamHealth {
    /// Session-average score, 0–100.
    pub video_score: u32,
    pub audio_score: u32,
    /// Level over the most recent [`StreamHealth::RECENT_WINDOW`] intervals.
    pub video_level: HealthLevel,
    pub audio_level: HealthLevel,
    pub fps_series: Vec<f64>,
    pub audio_series: Vec<f64>,
}

impl StreamHealth {
    pub const SPARK_COUNT: usize = 40;
    pub const RECENT_WINDOW: usize = 10;

    pub fn compute(history: &[StreamMetrics], target_fps: u32) -> Self {
        let samples: Vec<HealthSample> = history.iter().map(HealthSample::from).collect();
        let video: Vec<f64> = samples.iter().map(|s| s.video_score(target_fps)).collect();
        let audio: Vec<f64> = samples.iter().map(HealthSample::audio_score).collect();
        let spark = &history[history.len().saturating_sub(Self::SPARK_COUNT)..];

        Self {
            video_score: percent(avg(&video)),
            audio_score: percent(avg(&audio)),
            video_level: HealthLevel::from_score(avg(tail(&video, Self::RECENT_WINDOW))),
            audio_level: HealthLevel::from_score(avg(tail(&audio, Self::RECENT_WINDOW))),
            fps_series: spark.iter().map(|m| m.fps as f64).collect(),
            audio_series: spark.iter().map(|m| m.a_sent as f64).collect(),
        }
    }

    /// Worst of video and audio.
    pub fn overall_level(&self) -> HealthLevel {
        self.video_level.min(self.audio_level)
    }
}

fn tail(xs: &[f64], n: usize) -> &[f64] {
    &xs[xs.len().saturating_sub(n)..]
}

fn avg(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        1.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

fn percent(score: f64) -> u32 {
    (score * 100.0).round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> HealthSample {
        HealthSample {
            fps: 30,
            gap_p95_ms: 33,
            v_drop: 0,
            a_sent: 50,
            a_drop_not_ready: 0,
            a_enc_err: 0,
        }
    }

    fn video(s: HealthSample, target_fps: u32) -> f64 {
        s.video_score(target_fps)
    }

    #[test]
    fn video_healthy() {
        assert_eq!(video(sample(), 30), 1.0);
    }

    #[test]
    fn video_fps_boundaries() {
        let at = |fps| video(HealthSample { fps, ..sample() }, 30);
        assert_eq!(at(29), 1.0);
        assert_eq!(at(28), 0.5);
        assert_eq!(at(26), 0.5);
        assert_eq!(at(25), 0.0);
    }

    #[test]
    fn video_jitter_freeze_threshold() {
        let at = |gap_p95_ms| {
            video(
                HealthSample {
                    gap_p95_ms,
                    ..sample()
                },
                30,
            )
        };
        assert_eq!(at(66), 1.0);
        assert_eq!(at(120), 0.5);
        assert_eq!(at(183), 0.5);
        assert_eq!(at(184), 0.0);
    }

    #[test]
    fn video_drop_percent() {
        let at = |v_drop| video(HealthSample { v_drop, ..sample() }, 30);
        assert_eq!(at(0), 1.0);
        assert_eq!(at(1), 0.5);
        assert_eq!(at(2), 0.0);
    }

    #[test]
    fn video_high_fps_target_scales_thresholds() {
        let at = |gap_p95_ms| {
            video(
                HealthSample {
                    fps: 60,
                    gap_p95_ms,
                    ..sample()
                },
                60,
            )
        };
        assert_eq!(at(33), 1.0);
        assert_eq!(at(50), 0.5);
    }

    #[test]
    fn video_zero_target_fps_is_clamped() {
        assert_eq!(video(HealthSample { fps: 0, ..sample() }, 0), 0.0);
        let s = HealthSample {
            fps: 1,
            gap_p95_ms: 1000,
            ..sample()
        };
        assert_eq!(video(s, 0), 1.0);
    }

    #[test]
    fn audio_healthy() {
        assert_eq!(sample().audio_score(), 1.0);
    }

    #[test]
    fn audio_sent_boundaries() {
        let at = |a_sent| HealthSample { a_sent, ..sample() }.audio_score();
        assert_eq!(at(48), 1.0);
        assert_eq!(at(47), 0.5);
        assert_eq!(at(45), 0.5);
        assert_eq!(at(44), 0.0);
    }

    #[test]
    fn audio_encode_error_forces_bad() {
        let s = HealthSample {
            a_enc_err: 1,
            ..sample()
        };
        assert_eq!(s.audio_score(), 0.0);
    }

    #[test]
    fn audio_drop_not_ready_boundaries() {
        let at = |a_drop_not_ready| {
            HealthSample {
                a_drop_not_ready,
                ..sample()
            }
            .audio_score()
        };
        assert_eq!(at(1), 1.0);
        assert_eq!(at(2), 0.5);
        assert_eq!(at(4), 0.0);
    }

    #[test]
    fn level_thresholds() {
        assert_eq!(HealthLevel::from_score(1.0), HealthLevel::Good);
        assert_eq!(HealthLevel::from_score(0.9), HealthLevel::Good);
        assert_eq!(HealthLevel::from_score(0.89), HealthLevel::Warn);
        assert_eq!(HealthLevel::from_score(0.6), HealthLevel::Warn);
        assert_eq!(HealthLevel::from_score(0.59), HealthLevel::Bad);
        assert_eq!(HealthLevel::from_score(0.0), HealthLevel::Bad);
    }

    fn metrics(fps: u64, a_sent: u64) -> StreamMetrics {
        StreamMetrics {
            fps,
            gap_p95_ms: 33,
            a_sent,
            ..StreamMetrics::default()
        }
    }

    #[test]
    fn empty_history_is_fully_healthy() {
        let h = StreamHealth::compute(&[], 30);
        assert_eq!(h.video_score, 100);
        assert_eq!(h.audio_score, 100);
        assert_eq!(h.overall_level(), HealthLevel::Good);
        assert!(h.fps_series.is_empty());
        assert!(h.audio_series.is_empty());
    }

    #[test]
    fn session_score_averages_whole_history_and_level_uses_recent_window() {
        let mut history = vec![metrics(0, 50); 10];
        history.extend(vec![metrics(30, 50); 10]);
        let h = StreamHealth::compute(&history, 30);
        assert_eq!(h.video_score, 50);
        assert_eq!(h.video_level, HealthLevel::Good);
        assert_eq!(h.audio_score, 100);
        assert_eq!(h.audio_level, HealthLevel::Good);
    }

    #[test]
    fn overall_level_is_worst_of_video_and_audio() {
        let history = vec![metrics(30, 47); 5];
        let h = StreamHealth::compute(&history, 30);
        assert_eq!(h.video_level, HealthLevel::Good);
        assert_eq!(h.audio_level, HealthLevel::Bad);
        assert_eq!(h.overall_level(), HealthLevel::Bad);
    }

    #[test]
    fn series_keep_last_spark_count_entries() {
        let history: Vec<_> = (0..50).map(|i| metrics(i, 100 + i)).collect();
        let h = StreamHealth::compute(&history, 30);
        assert_eq!(h.fps_series.len(), StreamHealth::SPARK_COUNT);
        assert_eq!(h.fps_series.first(), Some(&10.0));
        assert_eq!(h.fps_series.last(), Some(&49.0));
        assert_eq!(h.audio_series.first(), Some(&110.0));
    }
}
