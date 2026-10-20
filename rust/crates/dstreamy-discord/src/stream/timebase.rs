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
    offset: i64,
    last: [Option<i64>; 2],
    video_step_us: i64,
}

impl Timebase {
    pub fn new(fps: u32) -> Self {
        Self {
            origin: None,
            offset: 0,
            last: [None; 2],
            video_step_us: step_for(fps),
        }
    }

    pub fn set_fps(&mut self, fps: u32) {
        self.video_step_us = step_for(fps);
    }

    pub fn map(&mut self, track: Track, pts_us: u64) -> Duration {
        let pts = i64::try_from(pts_us).unwrap_or(i64::MAX);
        let origin = *self.origin.get_or_insert(pts);
        let mut rel = pts.saturating_add(self.offset).saturating_sub(origin);
        let slot = &mut self.last[track as usize];
        let (step, tick) = match track {
            Track::Video => (self.video_step_us, VIDEO_TICK_US),
            Track::Audio => (OPUS_FRAME_DURATION_US as i64, AUDIO_TICK_US),
        };
        match *slot {
            Some(last) if rel < last + tick && last - rel > REBASE_THRESHOLD_US => {
                self.offset += last + step - rel;
                rel = last + step;
            }
            Some(last) if rel < last + tick => rel = last + tick,
            _ => rel = rel.max(0),
        }
        *slot = Some(rel);
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
    fn fps_change_sets_the_rebase_step() {
        let mut tb = Timebase::new(30);
        tb.set_fps(60);
        tb.map(Track::Video, 3_000_000);
        tb.map(Track::Video, 6_000_000);
        assert_eq!(us(tb.map(Track::Video, 0)), 3_016_666);
        assert_eq!(us(tb.map(Track::Video, 16_666)), 3_033_332);
    }
}
