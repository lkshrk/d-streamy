use crate::{AudioFrame, EncodedVideoFrame, MediaError, VideoCodec};

/// Planar 4:2:0 frame with tightly packed Y, U and V planes.
#[cfg(feature = "openh264")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct I420Frame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

#[cfg(feature = "openh264")]
impl I420Frame {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            data: vec![0; Self::expected_len(width, height)],
        }
    }

    /// Bytes in a tightly packed `width`x`height` frame.
    pub fn expected_len(width: u32, height: u32) -> usize {
        let (luma, chroma) = Self::plane_lens(width, height);
        luma + 2 * chroma
    }

    fn plane_lens(width: u32, height: u32) -> (usize, usize) {
        let (w, h) = (width as usize, height as usize);
        (w * h, (w / 2) * (h / 2))
    }

    pub fn planes(&self) -> (&[u8], &[u8], &[u8]) {
        let (luma, chroma) = Self::plane_lens(self.width, self.height);
        let (y, uv) = self.data.split_at(luma);
        let (u, v) = uv.split_at(chroma);
        (y, u, v)
    }

    pub fn planes_mut(&mut self) -> (&mut [u8], &mut [u8], &mut [u8]) {
        let (luma, chroma) = Self::plane_lens(self.width, self.height);
        let (y, uv) = self.data.split_at_mut(luma);
        let (u, v) = uv.split_at_mut(chroma);
        (y, u, v)
    }
}

/// Encoder for backends whose capture yields raw frames (e.g. ScreenCaptureKit into VideoToolbox).
pub trait VideoEncoder: Send {
    type Frame;

    fn codec(&self) -> VideoCodec;

    /// Encodes one frame; asynchronous encoders may return zero or several access units.
    fn encode(
        &mut self,
        frame: &Self::Frame,
        pts_us: u64,
        force_keyframe: bool,
    ) -> Result<Vec<EncodedVideoFrame>, MediaError>;

    fn flush(&mut self) -> Result<Vec<EncodedVideoFrame>, MediaError>;
}

/// PCM to Opus encoder; input is one 20 ms frame of interleaved 48 kHz stereo `i16`.
pub trait AudioEncoder: Send {
    fn encode(&mut self, pcm: &[i16], pts_us: u64) -> Result<AudioFrame, MediaError>;
}
