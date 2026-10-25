use std::collections::VecDeque;
use std::time::{Duration, Instant};

use dstreamy_media::MediaFrame;

/// How often a still-missing keyframe is asked for again.
pub const KEYFRAME_RETRY: Duration = Duration::from_secs(1);

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Effects {
    pub dropped_video: u64,
    pub dropped_audio: u64,
    pub request_keyframe: bool,
}

/// Non-blocking capture-to-transport buffer; late video is dropped up to the next keyframe.
#[derive(Debug)]
pub struct FrameQueue {
    frames: VecDeque<MediaFrame>,
    video_len: usize,
    audio_len: usize,
    max_video: usize,
    max_audio: usize,
    awaiting_keyframe: bool,
    last_request: Option<Instant>,
    closed: bool,
    flush_transport: bool,
}

impl FrameQueue {
    pub fn new(max_video: usize, max_audio: usize) -> Self {
        Self {
            frames: VecDeque::new(),
            video_len: 0,
            audio_len: 0,
            max_video: max_video.max(1),
            max_audio: max_audio.max(1),
            awaiting_keyframe: true,
            last_request: None,
            closed: false,
            flush_transport: false,
        }
    }

    pub fn push(&mut self, frame: MediaFrame, now: Instant) -> Effects {
        let mut effects = Effects::default();
        if self.closed {
            return effects;
        }
        match frame {
            MediaFrame::Audio(_) => {
                if self.audio_len >= self.max_audio {
                    self.drop_oldest_audio();
                    effects.dropped_audio = 1;
                }
                self.audio_len += 1;
                self.frames.push_back(frame);
            }
            MediaFrame::Video(ref v) if v.keyframe => {
                self.awaiting_keyframe = false;
                if self.video_len >= self.max_video {
                    effects.dropped_video = self.drop_video_before(self.frames.len());
                }
                self.video_len += 1;
                self.frames.push_back(frame);
            }
            MediaFrame::Video(_) => {
                if !self.awaiting_keyframe && self.video_len >= self.max_video {
                    effects.dropped_video = self.drop_video_before(self.frames.len());
                    self.awaiting_keyframe = true;
                }
                if self.awaiting_keyframe {
                    effects.dropped_video += 1;
                    effects.request_keyframe = self.should_request(now);
                } else {
                    self.video_len += 1;
                    self.frames.push_back(frame);
                }
            }
        }
        effects
    }

    pub fn pop(&mut self) -> Option<MediaFrame> {
        let frame = self.frames.pop_front()?;
        match frame {
            MediaFrame::Video(_) => self.video_len -= 1,
            MediaFrame::Audio(_) => self.audio_len -= 1,
        }
        Some(frame)
    }

    /// Whether video queued in the transport must be dropped before the next send; clears it.
    pub fn take_flush(&mut self) -> bool {
        std::mem::take(&mut self.flush_transport)
    }

    /// A video frame was lost in transport: skip queued video up to the next keyframe, here and in the transport.
    pub fn video_lost(&mut self, now: Instant) -> Effects {
        self.flush_transport = true;
        let next_key = self
            .frames
            .iter()
            .position(|f| matches!(f, MediaFrame::Video(v) if v.keyframe));
        let mut effects = Effects {
            dropped_video: self.drop_video_before(next_key.unwrap_or(self.frames.len())),
            ..Effects::default()
        };
        if next_key.is_none() {
            self.awaiting_keyframe = true;
            effects.request_keyframe = self.should_request(now);
        }
        effects
    }

    /// A new connection needs a keyframe: drop queued video and ask for one now.
    pub fn require_keyframe(&mut self, now: Instant) -> Effects {
        let dropped_video = self.drop_video_before(self.frames.len());
        self.awaiting_keyframe = true;
        self.last_request = Some(now);
        Effects {
            dropped_video,
            dropped_audio: 0,
            request_keyframe: true,
        }
    }

    /// Refuses further frames; queued ones can still be popped.
    pub fn close(&mut self) {
        self.closed = true;
    }

    pub fn clear(&mut self) {
        self.frames.clear();
        self.video_len = 0;
        self.audio_len = 0;
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    #[cfg(test)]
    pub fn awaiting_keyframe(&self) -> bool {
        self.awaiting_keyframe
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    fn should_request(&mut self, now: Instant) -> bool {
        let due = self
            .last_request
            .is_none_or(|t| now.duration_since(t) >= KEYFRAME_RETRY);
        if due {
            self.last_request = Some(now);
        }
        due
    }

    fn drop_oldest_audio(&mut self) {
        if let Some(i) = self
            .frames
            .iter()
            .position(|f| matches!(f, MediaFrame::Audio(_)))
        {
            self.frames.remove(i);
            self.audio_len -= 1;
        }
    }

    fn drop_video_before(&mut self, end: usize) -> u64 {
        let mut index = 0;
        let mut dropped = 0;
        self.frames.retain(|f| {
            let keep = index >= end || !matches!(f, MediaFrame::Video(_));
            index += 1;
            if !keep {
                dropped += 1;
            }
            keep
        });
        self.video_len -= dropped;
        dropped as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dstreamy_media::{AudioFrame, EncodedVideoFrame, VideoCodec};

    fn video(pts_us: u64, keyframe: bool) -> MediaFrame {
        MediaFrame::Video(EncodedVideoFrame {
            codec: VideoCodec::H264,
            data: vec![0; 4],
            pts_us,
            keyframe,
        })
    }

    fn audio(pts_us: u64) -> MediaFrame {
        MediaFrame::Audio(AudioFrame {
            data: vec![1],
            pts_us,
        })
    }

    fn drain(q: &mut FrameQueue) -> Vec<(bool, u64)> {
        std::iter::from_fn(|| q.pop())
            .map(|f| (matches!(f, MediaFrame::Video(_)), f.pts_us()))
            .collect()
    }

    #[test]
    fn require_keyframe_drops_queued_video_and_waits_for_the_next_keyframe() {
        let now = Instant::now();
        let mut q = FrameQueue::new(4, 4);
        q.push(video(0, true), now);
        q.push(audio(0), now);
        q.push(video(33, false), now);
        let e = q.require_keyframe(now);
        assert_eq!((e.dropped_video, e.request_keyframe), (2, true));
        let e = q.push(video(66, false), now);
        assert_eq!((e.dropped_video, e.request_keyframe), (1, false));
        q.push(video(100, true), now);
        assert_eq!(drain(&mut q), [(false, 0), (true, 100)]);
    }

    #[test]
    fn passes_frames_through_in_order() {
        let now = Instant::now();
        let mut q = FrameQueue::new(4, 4);
        for f in [video(0, true), audio(0), video(33, false), audio(20)] {
            assert_eq!(q.push(f, now), Effects::default());
        }
        assert_eq!(
            drain(&mut q),
            [(true, 0), (false, 0), (true, 33), (false, 20)]
        );
    }

    #[test]
    fn video_before_the_first_keyframe_is_dropped_and_a_keyframe_requested() {
        let now = Instant::now();
        let mut q = FrameQueue::new(4, 4);
        let e = q.push(video(0, false), now);
        assert_eq!((e.dropped_video, e.request_keyframe), (1, true));
        let e = q.push(video(33, false), now);
        assert_eq!((e.dropped_video, e.request_keyframe), (1, false));
        assert_eq!(q.push(video(66, true), now), Effects::default());
        assert_eq!(drain(&mut q), [(true, 66)]);
    }

    #[test]
    fn full_video_queue_drops_until_next_keyframe() {
        let now = Instant::now();
        let mut q = FrameQueue::new(2, 8);
        q.push(video(0, true), now);
        q.push(audio(0), now);
        q.push(video(33, false), now);
        let e = q.push(video(66, false), now);
        assert_eq!((e.dropped_video, e.request_keyframe), (3, true));
        assert!(q.awaiting_keyframe());
        let e = q.push(video(100, false), now);
        assert_eq!((e.dropped_video, e.request_keyframe), (1, false));
        q.push(audio(20), now);
        q.push(video(133, true), now);
        q.push(video(166, false), now);
        assert_eq!(
            drain(&mut q),
            [(false, 0), (false, 20), (true, 133), (true, 166)]
        );
    }

    #[test]
    fn keyframe_into_a_full_queue_replaces_queued_video() {
        let now = Instant::now();
        let mut q = FrameQueue::new(2, 8);
        q.push(video(0, true), now);
        q.push(video(33, false), now);
        let e = q.push(video(66, true), now);
        assert_eq!((e.dropped_video, e.request_keyframe), (2, false));
        assert_eq!(drain(&mut q), [(true, 66)]);
    }

    #[test]
    fn audio_overflow_drops_the_oldest_audio_only() {
        let now = Instant::now();
        let mut q = FrameQueue::new(4, 2);
        q.push(audio(0), now);
        q.push(video(0, true), now);
        q.push(audio(20), now);
        let e = q.push(audio(40), now);
        assert_eq!(e.dropped_audio, 1);
        assert_eq!(drain(&mut q), [(true, 0), (false, 20), (false, 40)]);
    }

    #[test]
    fn transport_loss_skips_to_a_queued_keyframe() {
        let now = Instant::now();
        let mut q = FrameQueue::new(8, 8);
        q.push(video(0, true), now);
        q.pop();
        q.push(video(33, false), now);
        q.push(audio(20), now);
        q.push(video(66, true), now);
        q.push(video(100, false), now);
        let e = q.video_lost(now);
        assert_eq!((e.dropped_video, e.request_keyframe), (1, false));
        assert!(!q.awaiting_keyframe());
        assert_eq!(drain(&mut q), [(false, 20), (true, 66), (true, 100)]);
    }

    #[test]
    fn transport_loss_without_queued_keyframe_waits_for_one() {
        let now = Instant::now();
        let mut q = FrameQueue::new(8, 8);
        q.push(video(0, true), now);
        q.pop();
        q.push(video(33, false), now);
        let e = q.video_lost(now);
        assert_eq!((e.dropped_video, e.request_keyframe), (1, true));
        let e = q.push(video(66, false), now);
        assert_eq!((e.dropped_video, e.request_keyframe), (1, false));
        let e = q.push(video(100, false), now + KEYFRAME_RETRY);
        assert_eq!((e.dropped_video, e.request_keyframe), (1, true));
        q.push(video(133, true), now);
        assert_eq!(drain(&mut q), [(true, 133)]);
    }

    #[test]
    fn closed_queue_drains_but_refuses_new_frames() {
        let now = Instant::now();
        let mut q = FrameQueue::new(4, 4);
        q.push(video(0, true), now);
        q.push(audio(0), now);
        q.close();
        assert!(q.is_closed());
        assert_eq!(q.push(audio(20), now), Effects::default());
        assert_eq!(q.len(), 2);
        assert!(q.pop().is_some());
        q.clear();
        assert!(q.pop().is_none());
    }
}
