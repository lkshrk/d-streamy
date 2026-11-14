use crate::{OPUS_FRAME_DURATION_US, OPUS_SAMPLE_RATE};

/// Drift between sample-count pts and buffer timestamps beyond which audio pts are realigned.
pub(crate) const AUDIO_RESYNC_US: u64 = 2 * OPUS_FRAME_DURATION_US;
/// Buffer timestamps further behind than this were reset, not drifting; dropping that much would mute.
pub(crate) const AUDIO_REANCHOR_US: u64 = 1_000_000;

/// Sample-count pts for captured audio, realigned to buffer timestamps without ever stepping back.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AudioClock {
    /// Pts of the first queued sample.
    next_pts_us: Option<u64>,
    /// Added to buffer timestamps after their clock was reset backwards.
    offset_us: u64,
}

impl AudioClock {
    /// Aligns `frames` stamped `actual` after `pending` queued ones; returns how many leading frames to drop.
    pub fn admit(&mut self, pending: usize, frames: usize, actual: Option<u64>) -> usize {
        let pending_us = frames_to_us(pending);
        let Some(next) = self.next_pts_us else {
            self.next_pts_us = Some(actual.unwrap_or(0).saturating_sub(pending_us));
            return 0;
        };
        let Some(raw) = actual else {
            return 0;
        };
        let expected = next + pending_us;
        if self.offset_us > 0 && raw.abs_diff(expected) <= AUDIO_RESYNC_US {
            // A single outlier caused the offset; stamps are back on the original timeline.
            self.offset_us = 0;
            return 0;
        }
        let actual = raw.saturating_add(self.offset_us);
        if actual > expected + AUDIO_RESYNC_US {
            self.next_pts_us = Some(actual - pending_us);
            0
        } else if expected > actual + AUDIO_REANCHOR_US {
            self.offset_us += expected - actual;
            0
        } else if expected > actual + AUDIO_RESYNC_US {
            us_to_frames(expected - actual).min(frames)
        } else {
            0
        }
    }

    /// The pts of the next whole Opus frame; advances the clock past it.
    pub fn advance(&mut self) -> u64 {
        let pts = self.next_pts_us.unwrap_or(0);
        self.next_pts_us = Some(pts + OPUS_FRAME_DURATION_US);
        pts
    }
}

fn frames_to_us(frames: usize) -> u64 {
    frames as u64 * 1_000_000 / u64::from(OPUS_SAMPLE_RATE)
}

fn us_to_frames(us: u64) -> usize {
    usize::try_from(us * u64::from(OPUS_SAMPLE_RATE) / 1_000_000).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::OPUS_FRAME_SAMPLES;

    const FRAME: usize = OPUS_FRAME_SAMPLES;

    #[test]
    fn the_first_buffer_sets_the_clock() {
        let mut clock = AudioClock::default();
        assert_eq!(clock.admit(0, FRAME, Some(5_000)), 0);
        assert_eq!(clock.advance(), 5_000);
        assert_eq!(clock.advance(), 25_000);

        let mut unstamped = AudioClock::default();
        assert_eq!(unstamped.admit(0, FRAME, None), 0);
        assert_eq!(unstamped.advance(), 0);
    }

    #[test]
    fn small_jitter_keeps_sample_count_timing() {
        let mut clock = AudioClock::default();
        clock.admit(0, FRAME, Some(100_000));
        clock.advance();
        assert_eq!(clock.admit(0, FRAME, Some(100_000)), 0);
        assert_eq!(clock.advance(), 120_000);
        assert_eq!(clock.admit(0, FRAME, Some(170_000)), 0);
        assert_eq!(clock.advance(), 140_000);
    }

    #[test]
    fn a_gap_moves_the_clock_forward_past_queued_samples() {
        let mut clock = AudioClock::default();
        clock.admit(0, FRAME, Some(0));
        clock.advance();
        assert_eq!(clock.admit(480, FRAME, Some(500_000)), 0);
        assert_eq!(clock.advance(), 490_000);
    }

    #[test]
    fn a_late_buffer_drops_its_overlap_instead_of_stepping_back() {
        let mut clock = AudioClock::default();
        clock.admit(0, FRAME, Some(100_000));
        clock.advance();
        clock.advance();
        assert_eq!(clock.admit(0, FRAME, Some(90_000)), FRAME);
        assert_eq!(clock.admit(240, 3 * FRAME, Some(95_000)), 2_400);
        assert_eq!(clock.advance(), 140_000);
        assert_eq!(clock.admit(0, FRAME, None), 0);
        assert_eq!(clock.advance(), 160_000);
    }

    #[test]
    fn a_backward_reset_reanchors_instead_of_muting() {
        let mut clock = AudioClock::default();
        clock.admit(0, FRAME, Some(5_000_000));
        clock.advance();
        assert_eq!(clock.admit(0, FRAME, Some(0)), 0);
        assert_eq!(clock.advance(), 5_020_000);
        assert_eq!(clock.admit(0, FRAME, Some(20_000)), 0);
        assert_eq!(clock.advance(), 5_040_000);
        assert_eq!(clock.admit(0, FRAME, Some(500_000)), 0);
        assert_eq!(clock.advance(), 5_520_000);
    }

    #[test]
    fn a_single_backward_outlier_leaves_no_lasting_jump() {
        let mut clock = AudioClock::default();
        clock.admit(0, FRAME, Some(5_000_000));
        assert_eq!(clock.advance(), 5_000_000);
        assert_eq!(clock.admit(0, FRAME, Some(0)), 0);
        assert_eq!(clock.advance(), 5_020_000);
        assert_eq!(clock.admit(0, FRAME, Some(5_040_000)), 0);
        assert_eq!(clock.advance(), 5_040_000);
        assert_eq!(clock.admit(0, FRAME, Some(5_060_000)), 0);
        assert_eq!(clock.advance(), 5_060_000);
    }

    #[test]
    fn a_late_clock_mutes_at_most_the_reanchor_bound() {
        for lag_us in [500_000, 3_000_000] {
            let mut clock = AudioClock::default();
            clock.admit(0, FRAME, Some(lag_us + 20_000));
            clock.advance();
            let (mut pending, mut dropped) = (0, 0);
            for i in 0..300 {
                let stamp = 20_000 + i * OPUS_FRAME_DURATION_US;
                let drop = clock.admit(pending, FRAME, Some(stamp));
                dropped += drop;
                pending += FRAME - drop;
                while pending >= FRAME {
                    clock.advance();
                    pending -= FRAME;
                }
            }
            assert!(
                dropped <= us_to_frames(AUDIO_REANCHOR_US),
                "{lag_us}: {dropped}"
            );
        }
    }

    #[test]
    fn emitted_pts_never_go_backwards_whatever_the_stamps() {
        let mut clock = AudioClock::default();
        let mut pending = 0;
        let mut last = None;
        for (i, actual) in [
            0, 20_000, 400_000, 10_000, 30_000, 900_000, 0, 5_000, 950_000,
        ]
        .into_iter()
        .enumerate()
        {
            let frames = FRAME / 2 + i * 97;
            let drop = clock.admit(pending, frames, Some(actual));
            pending += frames - drop;
            while pending >= FRAME {
                let pts = clock.advance();
                assert!(last.is_none_or(|l| pts >= l + OPUS_FRAME_DURATION_US));
                last = Some(pts);
                pending -= FRAME;
            }
        }
    }
}
