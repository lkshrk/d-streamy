use std::time::Duration;

use dstreamy_media::OPUS_FRAME_DURATION_US;

/// Backward pts jumps beyond this are a source restart and rebase the timeline.
const REBASE_THRESHOLD_US: i64 = 1_000_000;
/// One RTP tick rounded up, so bumped frames never share a timestamp: 90 kHz video, 48 kHz audio.
const VIDEO_TICK_US: i64 = 12;
const AUDIO_TICK_US: i64 = 21;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Track {
    Video,
    Audio,
}

/// Maps source pts to one shared, per-track strictly increasing timeline starting at zero.
#[derive(Debug)]
pub struct Timebase {
    origin: Option<i64>,
    offset: [i64; 2],
    last: [Option<i64>; 2],
    /// The latest rebase offset, adopted by the other track when it restarts too.
    rebase: Option<i64>,
    video_step_us: i64,
}

impl Timebase {
    pub fn new(fps: u32) -> Self {
        Self {
            origin: None,
            offset: [0; 2],
            last: [None; 2],
            rebase: None,
            video_step_us: step_for(fps),
        }
    }

    /// Continues a timeline whose tracks last mapped `last_us`, `gap_us` (at least a frame) later.
    pub fn continuing(fps: u32, last_us: [Option<u64>; 2], gap_us: u64) -> Self {
        let mut timebase = Self::new(fps);
        let last = last_us.map(|l| l.map(|us| i64::try_from(us).unwrap_or(i64::MAX)));
        let latest = last.iter().flatten().max().copied().unwrap_or(0);
        let gap = i64::try_from(gap_us)
            .unwrap_or(i64::MAX)
            .max(timebase.video_step_us);
        timebase.offset = [latest.saturating_add(gap); 2];
        timebase.last = last;
        timebase
    }

    pub fn set_fps(&mut self, fps: u32) {
        self.video_step_us = step_for(fps);
    }

    pub fn map(&mut self, track: Track, pts_us: u64) -> Duration {
        let pts = i64::try_from(pts_us).unwrap_or(i64::MAX);
        let origin = *self.origin.get_or_insert(pts);
        let t = track as usize;
        let mut rel = pts.saturating_add(self.offset[t]).saturating_sub(origin);
        let (step, tick) = match track {
            Track::Video => (self.video_step_us, VIDEO_TICK_US),
            Track::Audio => (OPUS_FRAME_DURATION_US as i64, AUDIO_TICK_US),
        };
        match self.last[t] {
            Some(last) if rel < last + tick && last - rel > REBASE_THRESHOLD_US => {
                let shared = self
                    .rebase
                    .filter(|&o| o != self.offset[t])
                    .map(|o| (o, pts.saturating_add(o).saturating_sub(origin)))
                    .filter(|&(_, r)| r >= last + tick);
                if let Some((offset, r)) = shared {
                    self.offset[t] = offset;
                    rel = r;
                } else {
                    self.offset[t] += last + step - rel;
                    self.rebase = Some(self.offset[t]);
                    rel = last + step;
                }
            }
            Some(last) if rel < last + tick => rel = last + tick,
            _ => rel = rel.max(0),
        }
        self.last[t] = Some(rel);
        Duration::from_micros(rel as u64)
    }
}

fn step_for(fps: u32) -> i64 {
    1_000_000 / i64::from(fps.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn us(d: Duration) -> u64 {
        d.as_micros() as u64
    }

    #[test]
    fn first_frame_of_either_track_is_the_origin() {
        let mut tb = Timebase::new(30);
        assert_eq!(us(tb.map(Track::Audio, 5_000_000)), 0);
        assert_eq!(us(tb.map(Track::Video, 5_010_000)), 10_000);
        assert_eq!(us(tb.map(Track::Audio, 5_020_000)), 20_000);
        assert_eq!(us(tb.map(Track::Video, 5_043_333)), 43_333);
    }

    #[test]
    fn a_track_starting_before_the_origin_clamps_to_zero() {
        let mut tb = Timebase::new(30);
        assert_eq!(us(tb.map(Track::Video, 1_000)), 0);
        assert_eq!(us(tb.map(Track::Audio, 990)), 0);
        assert_eq!(us(tb.map(Track::Audio, 20_990)), 19_990);
    }

    #[test]
    fn repeated_or_jittered_pts_stay_strictly_increasing() {
        let mut tb = Timebase::new(30);
        assert_eq!(us(tb.map(Track::Video, 0)), 0);
        assert_eq!(us(tb.map(Track::Video, 0)), 12);
        assert_eq!(us(tb.map(Track::Video, 33_333)), 33_333);
        assert_eq!(us(tb.map(Track::Video, 33_000)), 33_345);
        assert_eq!(us(tb.map(Track::Audio, 0)), 0);
        assert_eq!(us(tb.map(Track::Audio, 10)), 21);
    }

    #[test]
    fn source_restart_rebases_both_tracks() {
        let mut tb = Timebase::new(30);
        tb.map(Track::Video, 0);
        tb.map(Track::Audio, 0);
        assert_eq!(us(tb.map(Track::Video, 5_000_000)), 5_000_000);
        assert_eq!(us(tb.map(Track::Audio, 5_000_000)), 5_000_000);
        assert_eq!(us(tb.map(Track::Video, 0)), 5_033_333);
        assert_eq!(us(tb.map(Track::Audio, 0)), 5_033_333);
        assert_eq!(us(tb.map(Track::Audio, 20_000)), 5_053_333);
        assert_eq!(us(tb.map(Track::Video, 33_333)), 5_066_666);
    }

    #[test]
    fn a_single_track_restart_leaves_the_other_track_alone() {
        let mut tb = Timebase::new(30);
        tb.map(Track::Video, 0);
        tb.map(Track::Audio, 0);
        assert_eq!(us(tb.map(Track::Audio, 5_000_000)), 5_000_000);
        assert_eq!(us(tb.map(Track::Video, 5_000_000)), 5_000_000);
        assert_eq!(us(tb.map(Track::Video, 0)), 5_033_333);
        assert_eq!(us(tb.map(Track::Audio, 5_020_000)), 5_020_000);
        assert_eq!(us(tb.map(Track::Video, 33_333)), 5_066_666);
        assert_eq!(us(tb.map(Track::Audio, 5_040_000)), 5_040_000);
    }

    #[test]
    fn a_continued_timeline_starts_after_the_last_frame_and_the_gap() {
        let mut tb = Timebase::continuing(30, [Some(9_000_000), Some(9_010_000)], 0);
        assert_eq!(us(tb.map(Track::Video, 123_000)), 9_043_333);
        assert_eq!(us(tb.map(Track::Audio, 123_000)), 9_043_333);
        assert_eq!(us(tb.map(Track::Video, 156_333)), 9_076_666);

        let mut tb = Timebase::continuing(30, [Some(9_000_000), None], 500_000);
        assert_eq!(us(tb.map(Track::Audio, 0)), 9_500_000);
    }

    #[test]
    fn a_continued_track_stays_strictly_increasing() {
        let mut tb = Timebase::continuing(30, [Some(1_000_000), Some(9_000_000)], 0);
        assert_eq!(us(tb.map(Track::Video, 0)), 9_033_333);
        assert_eq!(us(tb.map(Track::Audio, 0)), 9_033_333);
        assert!(us(tb.map(Track::Audio, 0)) > 9_033_333);
    }

    #[test]
    fn fps_change_sets_the_rebase_step() {
        let mut tb = Timebase::new(30);
        tb.set_fps(60);
        tb.map(Track::Video, 3_000_000);
        tb.map(Track::Video, 6_000_000);
        assert_eq!(us(tb.map(Track::Video, 0)), 3_016_666);
        assert_eq!(us(tb.map(Track::Video, 16_666)), 3_033_332);
    }
}
