use dstreamy_media::{AudioClock, AudioEncoder, AudioFrame, OPUS_CHANNELS, OPUS_FRAME_SAMPLES};

/// One Core Audio buffer of 32-bit float samples, interleaved when `channels` > 1.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SourceBuffer<'a> {
    pub channels: usize,
    pub samples: &'a [f32],
}

/// Converts interleaved or per-channel float buffers to interleaved stereo `i16`, duplicating mono.
pub(crate) fn stereo_i16_from_f32(
    frames: usize,
    buffers: &[SourceBuffer<'_>],
    gain: f32,
) -> Option<Vec<i16>> {
    let first = buffers.first()?;
    if frames == 0 {
        return None;
    }
    let interleaved = buffers.len() == 1 && first.channels > 1;
    let mut out = Vec::with_capacity(frames * OPUS_CHANNELS);
    for frame in 0..frames {
        for channel in 0..OPUS_CHANNELS {
            let buffer = if interleaved {
                first
            } else {
                &buffers[channel.min(buffers.len() - 1)]
            };
            let channels = buffer.channels.max(1);
            let index = frame * channels + channel.min(channels - 1);
            let sample = buffer.samples.get(index).copied().unwrap_or(0.0);
            out.push(pcm16(sample * gain));
        }
    }
    Some(out)
}

fn pcm16(sample: f32) -> i16 {
    if !sample.is_finite() {
        return 0;
    }
    (sample.clamp(-1.0, 1.0) * 32767.0) as i16
}

/// Re-chunks stereo PCM of any buffer size into 20 ms Opus packets with continuous timestamps.
pub(crate) struct AudioPacketizer<E> {
    encoder: E,
    pending: Vec<i16>,
    clock: AudioClock,
}

impl<E: AudioEncoder> AudioPacketizer<E> {
    pub fn new(encoder: E) -> Self {
        Self {
            encoder,
            pending: Vec::with_capacity(OPUS_FRAME_SAMPLES * OPUS_CHANNELS * 2),
            clock: AudioClock::default(),
        }
    }

    /// Appends interleaved stereo samples starting at `pts_us`; frames Opus fails to encode are dropped.
    pub fn push(&mut self, pcm: &[i16], pts_us: u64) -> Vec<AudioFrame> {
        let drop = self.clock.admit(
            self.pending.len() / OPUS_CHANNELS,
            pcm.len() / OPUS_CHANNELS,
            Some(pts_us),
        );
        self.pending.extend_from_slice(&pcm[drop * OPUS_CHANNELS..]);
        let chunk = OPUS_FRAME_SAMPLES * OPUS_CHANNELS;
        let mut packets = Vec::new();
        let mut consumed = 0;
        while self.pending.len() - consumed >= chunk {
            let frame = &self.pending[consumed..consumed + chunk];
            if let Ok(packet) = self.encoder.encode(frame, self.clock.advance()) {
                packets.push(packet);
            }
            consumed += chunk;
        }
        self.pending.drain(..consumed);
        packets
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dstreamy_media::{OPUS_BITRATE_BPS, OpusEncoder};

    fn buffer(channels: usize, samples: &[f32]) -> SourceBuffer<'_> {
        SourceBuffer { channels, samples }
    }

    #[test]
    fn interleaved_stereo_float_becomes_stereo_pcm16() {
        let samples = [0.25, -0.25, 0.50, -0.50];
        let pcm = stereo_i16_from_f32(2, &[buffer(2, &samples)], 1.0);
        assert_eq!(pcm, Some(vec![8191, -8191, 16383, -16383]));
    }

    #[test]
    fn planar_mono_float_duplicates_to_stereo_pcm16() {
        let samples = [0.25, -0.50];
        let pcm = stereo_i16_from_f32(2, &[buffer(1, &samples)], 1.0);
        assert_eq!(pcm, Some(vec![8191, 8191, -16383, -16383]));
    }

    #[test]
    fn planar_stereo_float_becomes_stereo_pcm16() {
        let (left, right) = ([1.0, -1.0], [0.0, 0.5]);
        let pcm = stereo_i16_from_f32(2, &[buffer(1, &left), buffer(1, &right)], 1.0);
        assert_eq!(pcm, Some(vec![32767, 0, -32767, 16383]));
    }

    #[test]
    fn gain_clamps_and_non_finite_samples_are_silenced() {
        let samples = [0.75, f32::NAN];
        let pcm = stereo_i16_from_f32(1, &[buffer(2, &samples)], 2.0);
        assert_eq!(pcm, Some(vec![32767, 0]));
    }

    #[test]
    fn short_buffers_read_as_silence_and_empty_input_is_rejected() {
        assert_eq!(
            stereo_i16_from_f32(2, &[buffer(2, &[0.5])], 1.0),
            Some(vec![16383, 0, 0, 0])
        );
        assert_eq!(stereo_i16_from_f32(0, &[buffer(2, &[])], 1.0), None);
        assert_eq!(stereo_i16_from_f32(4, &[], 1.0), None);
    }

    #[test]
    fn packetizer_emits_20ms_opus_frames_with_continuous_pts() {
        let mut packetizer = AudioPacketizer::new(OpusEncoder::new(OPUS_BITRATE_BPS).unwrap());
        let block = vec![0i16; 1024 * OPUS_CHANNELS];
        let first = packetizer.push(&block, 1_000_000);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].pts_us, 1_000_000);
        let second = packetizer.push(&block, 1_021_333);
        let pts: Vec<u64> = second.iter().map(|p| p.pts_us).collect();
        assert_eq!(pts, [1_020_000]);
        let third = packetizer.push(&block, 1_042_666);
        assert_eq!(
            third.iter().map(|p| p.pts_us).collect::<Vec<_>>(),
            [1_040_000]
        );
        assert!(third.iter().all(|p| !p.data.is_empty()));
    }

    #[test]
    fn packetizer_resyncs_after_a_capture_gap() {
        let mut packetizer = AudioPacketizer::new(OpusEncoder::new(OPUS_BITRATE_BPS).unwrap());
        let block = vec![0i16; 1024 * OPUS_CHANNELS];
        packetizer.push(&block, 0);
        let after_gap = packetizer.push(&block, 500_000);
        assert_eq!(
            after_gap.iter().map(|p| p.pts_us).collect::<Vec<_>>(),
            [498_667]
        );
    }

    #[test]
    fn packetizer_never_steps_back_when_the_clock_falls_behind() {
        let mut packetizer = AudioPacketizer::new(OpusEncoder::new(OPUS_BITRATE_BPS).unwrap());
        let frame = vec![0i16; OPUS_FRAME_SAMPLES * OPUS_CHANNELS];
        let pts = |p: Vec<AudioFrame>| p.iter().map(|f| f.pts_us).collect::<Vec<_>>();
        assert_eq!(pts(packetizer.push(&frame, 100_000)), [100_000]);
        assert_eq!(pts(packetizer.push(&frame, 110_000)), [120_000]);
        assert!(pts(packetizer.push(&frame, 90_000)).is_empty());
        assert_eq!(pts(packetizer.push(&frame, 125_000)), [140_000]);
    }
}
