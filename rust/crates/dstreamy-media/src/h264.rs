use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile,
    RateControlMode, UsageType,
};
use openh264::formats::YUVSlices;

use crate::{EncodedVideoFrame, I420Frame, MediaError, VideoCodec, VideoEncoder};

/// Software H.264 Constrained Baseline encoder (bundled OpenH264).
pub struct OpenH264Encoder {
    inner: Encoder,
    width: u32,
    height: u32,
}

impl OpenH264Encoder {
    pub fn new(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
        keyframe_interval: u32,
    ) -> Result<Self, MediaError> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(MediaError::InvalidConfig(format!(
                "H.264 needs non-zero even dimensions, got {width}x{height}"
            )));
        }
        let config = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .profile(Profile::Baseline)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            .intra_frame_period(IntraFramePeriod::from_num_frames(keyframe_interval))
            .skip_frames(false);
        let inner = Encoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(|e| MediaError::Encoder(e.to_string()))?;
        Ok(Self {
            inner,
            width,
            height,
        })
    }
}

impl VideoEncoder for OpenH264Encoder {
    type Frame = I420Frame;

    fn codec(&self) -> VideoCodec {
        VideoCodec::H264
    }

    fn encode(
        &mut self,
        frame: &I420Frame,
        pts_us: u64,
        force_keyframe: bool,
    ) -> Result<Vec<EncodedVideoFrame>, MediaError> {
        if (frame.width, frame.height) != (self.width, self.height) {
            return Err(MediaError::Encoder(format!(
                "frame is {}x{}, encoder expects {}x{}",
                frame.width, frame.height, self.width, self.height
            )));
        }
        let (w, h) = (self.width as usize, self.height as usize);
        let (y, uv) = frame.data.split_at(w * h);
        let (u, v) = uv.split_at((w / 2) * (h / 2));
        let yuv = YUVSlices::new((y, u, v), (w, h), (w, w / 2, w / 2));
        if force_keyframe {
            self.inner.force_intra_frame();
        }
        let bitstream = self
            .inner
            .encode(&yuv)
            .map_err(|e| MediaError::Encoder(e.to_string()))?;
        let keyframe = match bitstream.frame_type() {
            FrameType::IDR => true,
            FrameType::I | FrameType::P | FrameType::IPMixed => false,
            FrameType::Skip | FrameType::Invalid => return Ok(Vec::new()),
        };
        let data = bitstream.to_vec();
        if data.is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![EncodedVideoFrame {
            codec: VideoCodec::H264,
            data,
            pts_us,
            keyframe,
        }])
    }

    fn flush(&mut self) -> Result<Vec<EncodedVideoFrame>, MediaError> {
        Ok(Vec::new())
    }
}
