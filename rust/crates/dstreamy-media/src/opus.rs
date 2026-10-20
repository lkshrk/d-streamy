use std::ptr::NonNull;

use unsafe_libopus::{
    OPUS_APPLICATION_AUDIO, OPUS_OK, OPUS_SET_BITRATE_REQUEST, opus_encode, opus_encoder_create,
    opus_encoder_ctl, opus_encoder_destroy,
};

use crate::{
    AudioEncoder, AudioFrame, MediaError, OPUS_CHANNELS, OPUS_FRAME_SAMPLES, OPUS_SAMPLE_RATE,
};

const MAX_PACKET_BYTES: usize = 4000;

/// Opus encoder for 20 ms frames of 48 kHz interleaved stereo, the format Discord expects.
pub struct OpusEncoder {
    state: NonNull<unsafe_libopus::OpusEncoder>,
}

// The encoder state is plain heap memory owned exclusively by this value.
unsafe impl Send for OpusEncoder {}

impl OpusEncoder {
    pub fn new(bitrate_bps: u32) -> Result<Self, MediaError> {
        let bitrate = i32::try_from(bitrate_bps)
            .map_err(|_| MediaError::InvalidConfig(format!("Opus bitrate {bitrate_bps}")))?;
        let mut err = 0;
        // SAFETY: arguments are valid constants and `err` outlives the call.
        let raw = unsafe {
            opus_encoder_create(
                OPUS_SAMPLE_RATE as i32,
                OPUS_CHANNELS as i32,
                OPUS_APPLICATION_AUDIO,
                &mut err,
            )
        };
        let state = NonNull::new(raw)
            .filter(|_| err == OPUS_OK)
            .ok_or_else(|| MediaError::Encoder(format!("opus_encoder_create failed: {err}")))?;
        let encoder = Self { state };
        // SAFETY: `state` is a live encoder created above.
        let rc =
            unsafe { opus_encoder_ctl!(encoder.state.as_ptr(), OPUS_SET_BITRATE_REQUEST, bitrate) };
        if rc != OPUS_OK {
            return Err(MediaError::Encoder(format!(
                "OPUS_SET_BITRATE failed: {rc}"
            )));
        }
        Ok(encoder)
    }
}

impl AudioEncoder for OpusEncoder {
    fn encode(&mut self, pcm: &[i16], pts_us: u64) -> Result<AudioFrame, MediaError> {
        if pcm.len() != OPUS_FRAME_SAMPLES * OPUS_CHANNELS {
            return Err(MediaError::Encoder(format!(
                "Opus frame needs {} samples, got {}",
                OPUS_FRAME_SAMPLES * OPUS_CHANNELS,
                pcm.len()
            )));
        }
        let mut packet = vec![0u8; MAX_PACKET_BYTES];
        // SAFETY: `pcm` holds exactly one frame and `packet` has `MAX_PACKET_BYTES` of space.
        let len = unsafe {
            opus_encode(
                self.state.as_ptr(),
                pcm.as_ptr(),
                OPUS_FRAME_SAMPLES as i32,
                packet.as_mut_ptr(),
                MAX_PACKET_BYTES as i32,
            )
        };
        let len =
            usize::try_from(len).map_err(|_| MediaError::Encoder(format!("opus_encode: {len}")))?;
        packet.truncate(len);
        Ok(AudioFrame {
            data: packet,
            pts_us,
        })
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        // SAFETY: `state` was created by `opus_encoder_create` and is destroyed once.
        unsafe { opus_encoder_destroy(self.state.as_ptr()) }
    }
}
