use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use dstreamy_media::{EncodedVideoFrame, MediaError, VideoEncoder};
use objc2_core_foundation::{CFRetained, Type};
use objc2_core_video::CVPixelBuffer;

use crate::vt::VtEncoder;

/// ScreenCaptureKit sends no complete frames while the screen is static, so the last one is re-encoded.
pub(crate) const IDLE_KEYFRAME: Duration = Duration::from_secs(2);
/// How long a keyframe request waits for a new frame before the last one is re-encoded.
pub(crate) const REQUEST_GRACE: Duration = Duration::from_millis(100);

struct LastFrame {
    buffer: CFRetained<CVPixelBuffer>,
    pts_us: u64,
    at: Instant,
}

// SAFETY: CoreFoundation reference counting is thread-safe, and the buffer is only read after capture.
unsafe impl Send for LastFrame {}

/// The encoder and the last complete frame, kept to serve keyframes while no new frames arrive.
pub(crate) struct VideoState {
    encoder: VtEncoder,
    last: Option<LastFrame>,
}

impl VideoState {
    pub(crate) fn new(encoder: VtEncoder) -> Self {
        Self {
            encoder,
            last: None,
        }
    }

    pub(crate) fn encode(
        &mut self,
        frame: &CVPixelBuffer,
        pts_us: u64,
        force_keyframe: bool,
        now: Instant,
    ) -> Result<Vec<EncodedVideoFrame>, MediaError> {
        let pts_us = match &self.last {
            Some(last) => pts_us.max(last.pts_us + 1),
            None => pts_us,
        };
        let units = self.encoder.encode(frame, pts_us, force_keyframe)?;
        self.last = Some(LastFrame {
            buffer: frame.retain(),
            pts_us,
            at: now,
        });
        Ok(units)
    }

    /// Re-encodes the last frame as a keyframe if one was requested or the screen has been idle too long.
    pub(crate) fn refresh(
        &mut self,
        keyframe_requested: &AtomicBool,
        now: Instant,
    ) -> Result<Vec<EncodedVideoFrame>, MediaError> {
        let Some(last) = &self.last else {
            return Ok(Vec::new());
        };
        let idle = now.saturating_duration_since(last.at);
        let due = idle >= IDLE_KEYFRAME
            || (idle >= REQUEST_GRACE && keyframe_requested.load(Ordering::Relaxed));
        if !due {
            return Ok(Vec::new());
        }
        keyframe_requested.store(false, Ordering::Relaxed);
        let buffer = last.buffer.clone();
        let pts_us = last
            .pts_us
            .saturating_add(u64::try_from(idle.as_micros()).unwrap_or(u64::MAX));
        self.encode(&buffer, pts_us, true, now)
    }

    pub(crate) fn flush(&mut self) -> Result<Vec<EncodedVideoFrame>, MediaError> {
        self.encoder.flush()
    }
}

#[cfg(test)]
mod tests {
    use std::ptr::{self, NonNull};

    use dstreamy_media::VideoCodec;
    use dstreamy_media::annexb::{H264_NAL_IDR, h264_nal_type, nal_units};
    use objc2_core_video::{CVPixelBufferCreate, kCVPixelFormatType_32BGRA, kCVReturnSuccess};

    use super::*;
    use crate::vt::VtEncoderConfig;

    fn blank_frame() -> CFRetained<CVPixelBuffer> {
        let mut raw: *mut CVPixelBuffer = ptr::null_mut();
        // SAFETY: valid arguments and out-pointer.
        let status = unsafe {
            CVPixelBufferCreate(
                None,
                320,
                240,
                kCVPixelFormatType_32BGRA,
                None,
                NonNull::from(&mut raw),
            )
        };
        assert_eq!(status, kCVReturnSuccess);
        // SAFETY: Create returned a +1 reference.
        unsafe { CFRetained::from_raw(NonNull::new(raw).unwrap()) }
    }

    fn video() -> VideoState {
        VideoState::new(
            VtEncoder::new(VtEncoderConfig {
                width: 320,
                height: 240,
                fps: 30,
                bitrate_bps: 1_000_000,
                codec: VideoCodec::H264,
            })
            .unwrap(),
        )
    }

    fn has_idr(unit: &EncodedVideoFrame) -> bool {
        unit.keyframe && nal_units(&unit.data).any(|n| h264_nal_type(n) == Some(H264_NAL_IDR))
    }

    fn encode_two_frames(video: &mut VideoState, start: Instant) -> Vec<EncodedVideoFrame> {
        let mut units = video.encode(&blank_frame(), 0, true, start).unwrap();
        let next = start + Duration::from_millis(33);
        units.extend(video.encode(&blank_frame(), 33_333, false, next).unwrap());
        units.extend(video.flush().unwrap());
        units
    }

    #[test]
    fn keyframe_request_without_new_frames_yields_an_idr() {
        let mut video = video();
        let start = Instant::now();
        let units = encode_two_frames(&mut video, start);
        assert!(!units.last().unwrap().keyframe);
        let last_pts = units.last().unwrap().pts_us;

        let requested = AtomicBool::new(false);
        let soon = start + Duration::from_millis(500);
        assert!(video.refresh(&requested, soon).unwrap().is_empty());
        video.flush().unwrap();

        requested.store(true, Ordering::Relaxed);
        let mut units = video.refresh(&requested, soon).unwrap();
        units.extend(video.flush().unwrap());
        assert!(!requested.load(Ordering::Relaxed));
        let repeat = units.last().expect("re-encoded frame");
        assert!(has_idr(repeat), "not an IDR: {repeat:?}");
        assert!(repeat.pts_us > last_pts);
    }

    #[test]
    fn idle_screen_gets_a_periodic_keyframe() {
        let mut video = video();
        let start = Instant::now();
        let units = encode_two_frames(&mut video, start);
        let last_pts = units.last().unwrap().pts_us;

        let requested = AtomicBool::new(false);
        let idle = start + Duration::from_millis(33) + IDLE_KEYFRAME;
        let mut units = video.refresh(&requested, idle).unwrap();
        units.extend(video.flush().unwrap());
        let repeat = units.last().expect("re-encoded frame");
        assert!(has_idr(repeat));
        assert_eq!(
            repeat.pts_us,
            last_pts + u64::try_from(IDLE_KEYFRAME.as_micros()).unwrap()
        );
    }

    #[test]
    fn requests_wait_for_a_new_frame_during_the_grace_period() {
        let mut video = video();
        let start = Instant::now();
        let requested = AtomicBool::new(true);
        assert!(video.refresh(&requested, start).unwrap().is_empty());
        encode_two_frames(&mut video, start);
        let within = start + Duration::from_millis(33) + REQUEST_GRACE / 2;
        assert!(video.refresh(&requested, within).unwrap().is_empty());
        assert!(requested.load(Ordering::Relaxed));
    }
}
