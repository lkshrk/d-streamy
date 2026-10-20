pub const OPUS_SAMPLE_RATE: u32 = 48_000;
pub const OPUS_CHANNELS: usize = 2;
/// Samples per channel in one 20 ms Opus frame.
pub const OPUS_FRAME_SAMPLES: usize = 960;
pub const OPUS_FRAME_DURATION_US: u64 = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    H264,
    H265,
}

/// One encoded access unit in Annex B form; keyframes carry their parameter sets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedVideoFrame {
    pub codec: VideoCodec,
    pub data: Vec<u8>,
    pub pts_us: u64,
    pub keyframe: bool,
}

/// One Opus packet of 20 ms, 48 kHz stereo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFrame {
    pub data: Vec<u8>,
    pub pts_us: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaFrame {
    Video(EncodedVideoFrame),
    Audio(AudioFrame),
}

impl MediaFrame {
    pub fn pts_us(&self) -> u64 {
        match self {
            Self::Video(v) => v.pts_us,
            Self::Audio(a) => a.pts_us,
        }
    }
}
