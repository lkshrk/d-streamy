use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use dstreamy_media::{EncodedVideoFrame, MediaFrame};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

/// Keyframes forced by back-pressure come at most this often, so a near-full channel is not flooded with IDRs.
pub(crate) const MIN_FORCED_KEYFRAME_GAP: Duration = Duration::from_millis(500);

pub(crate) type SharedSender = Arc<Mutex<Option<mpsc::Sender<MediaFrame>>>>;

pub(crate) fn send(sender: &SharedSender, frame: MediaFrame, on_full: impl FnOnce()) {
    let guard = sender.lock().unwrap_or_else(PoisonError::into_inner);
    match guard.as_ref().map(|tx| tx.try_send(frame)) {
        Some(Err(TrySendError::Full(_))) => on_full(),
        Some(Ok(())) | Some(Err(TrySendError::Closed(_))) | None => {}
    }
}

/// The encoder's output: after a lost access unit, later deltas reference it, so all wait for a keyframe.
pub(crate) fn video_sink(
    sender: SharedSender,
    frame_dropped: Arc<AtomicBool>,
    dropped_video: Arc<AtomicU64>,
) -> impl FnMut(EncodedVideoFrame) + Send + 'static {
    let mut awaiting_keyframe = false;
    move |frame| {
        if frame.keyframe {
            awaiting_keyframe = false;
        }
        if awaiting_keyframe {
            dropped_video.fetch_add(1, Ordering::Relaxed);
            return;
        }
        send(&sender, MediaFrame::Video(frame), || {
            awaiting_keyframe = true;
            frame_dropped.store(true, Ordering::Relaxed);
            dropped_video.fetch_add(1, Ordering::Relaxed);
        });
    }
}

/// Spaces keyframes forced after a loss by [`MIN_FORCED_KEYFRAME_GAP`].
#[derive(Debug, Default)]
pub(crate) struct KeyframePacer {
    last_forced: Option<Instant>,
}

impl KeyframePacer {
    /// Whether the next frame must be a keyframe; serves a pending loss only once the gap has passed.
    pub(crate) fn force(
        &mut self,
        requested: bool,
        frame_dropped: &AtomicBool,
        now: Instant,
    ) -> bool {
        let ready = self
            .last_forced
            .is_none_or(|last| now.saturating_duration_since(last) >= MIN_FORCED_KEYFRAME_GAP);
        let lost = (ready || requested) && frame_dropped.swap(false, Ordering::Relaxed);
        if requested || lost {
            self.last_forced = Some(now);
        }
        requested || lost
    }
}

#[cfg(test)]
mod tests {
    use dstreamy_media::VideoCodec;

    use super::*;

    fn unit(keyframe: bool, tag: u8) -> EncodedVideoFrame {
        EncodedVideoFrame {
            codec: VideoCodec::H264,
            data: vec![tag],
            pts_us: u64::from(tag),
            keyframe,
        }
    }

    fn tags(rx: &mut mpsc::Receiver<MediaFrame>) -> Vec<u8> {
        let mut out = Vec::new();
        while let Ok(MediaFrame::Video(v)) = rx.try_recv() {
            out.push(v.data[0]);
        }
        out
    }

    #[test]
    fn deltas_after_a_lost_unit_wait_for_the_next_keyframe() {
        let (tx, mut rx) = mpsc::channel(1);
        let dropped = Arc::new(AtomicBool::new(false));
        let count = Arc::new(AtomicU64::new(0));
        let mut sink = video_sink(
            Arc::new(Mutex::new(Some(tx))),
            Arc::clone(&dropped),
            Arc::clone(&count),
        );
        sink(unit(true, 0));
        sink(unit(false, 1));
        assert!(dropped.load(Ordering::Relaxed));
        assert_eq!(tags(&mut rx), [0]);
        sink(unit(false, 2));
        assert_eq!(tags(&mut rx), Vec::<u8>::new());
        sink(unit(false, 3));
        sink(unit(true, 4));
        assert_eq!(tags(&mut rx), [4]);
        sink(unit(false, 5));
        assert_eq!(tags(&mut rx), [5]);
        assert_eq!(count.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn forced_keyframes_after_losses_are_spaced() {
        let mut pacer = KeyframePacer::default();
        let dropped = AtomicBool::new(true);
        let start = Instant::now();
        assert!(pacer.force(false, &dropped, start));
        dropped.store(true, Ordering::Relaxed);
        let soon = start + MIN_FORCED_KEYFRAME_GAP / 2;
        assert!(!pacer.force(false, &dropped, soon));
        assert!(dropped.load(Ordering::Relaxed), "the loss stays pending");
        assert!(
            pacer.force(true, &dropped, soon),
            "requests are not delayed"
        );
        assert!(
            !dropped.load(Ordering::Relaxed),
            "a requested keyframe serves the loss"
        );
        dropped.store(true, Ordering::Relaxed);
        assert!(!pacer.force(false, &dropped, soon + MIN_FORCED_KEYFRAME_GAP / 4));
        let later = soon + MIN_FORCED_KEYFRAME_GAP;
        assert!(pacer.force(false, &dropped, later));
        assert!(!dropped.load(Ordering::Relaxed));
        assert!(!pacer.force(false, &dropped, later + MIN_FORCED_KEYFRAME_GAP));
    }
}
