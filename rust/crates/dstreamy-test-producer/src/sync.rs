//! A/V sync markers: one white video frame per second, paired with a 20 ms 1 kHz beep over a quiet 440 Hz tone.

use std::collections::VecDeque;
use std::f32::consts::PI;
use std::ptr::NonNull;

use openh264::decoder::Decoder;
use openh264::formats::YUVSource;

pub const SAMPLE_RATE: u32 = 48_000;
pub const BEEP_HZ: f32 = 1000.0;
pub const TONE_HZ: f32 = 440.0;
pub const BEEP_MS: u32 = 20;
pub const FLASH_PERIOD_US: i64 = 1_000_000;

/// Mean 8-bit luma between the harness's dark (about 40) and white (235) frames.
pub const FLASH_LUMA_THRESHOLD: f32 = 128.0;
/// The beep is 0.5 full scale and the tone 0.05, so half the beep amplitude separates them.
pub const BEEP_AMPLITUDE_THRESHOLD: f32 = 0.25;

const BEEP_WINDOW: usize = 240;
const BEEP_HOP: usize = 24;
const MAX_OPUS_FRAME: usize = 5760;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum SyncError {
    #[error("H.264 decoder: {0}")]
    H264(String),
    #[error("Opus decoder: {0}")]
    Opus(String),
    #[error("no flash markers in the video")]
    NoFlashes,
    #[error("no beep markers in the audio")]
    NoBeeps,
    #[error(
        "flash at {flash_us} µs has no beep within {tolerance_us} µs (nearest beep {nearest_us:?})"
    )]
    UnmatchedFlash {
        flash_us: i64,
        nearest_us: Option<i64>,
        tolerance_us: i64,
    },
    #[error("beep at {beep_us} µs has no flash within {tolerance_us} µs")]
    UnmatchedBeep { beep_us: i64, tolerance_us: i64 },
    #[error(
        "beep - flash offset {offset_us} µs at flash {flash_us} µs is not {expected_us} ± {tolerance_us} µs"
    )]
    OffsetOutOfRange {
        flash_us: i64,
        offset_us: i64,
        expected_us: i64,
        tolerance_us: i64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LumaFrame {
    pub pts_us: i64,
    pub mean_luma: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkerPair {
    pub flash_us: i64,
    pub beep_us: i64,
}

impl MarkerPair {
    pub fn offset_us(&self) -> i64 {
        self.beep_us - self.flash_us
    }
}

/// Presentation times of the first white frame of every run of white frames.
pub fn flash_onsets(frames: &[LumaFrame]) -> Vec<i64> {
    let mut onsets = Vec::new();
    let mut was_white = false;
    for frame in frames {
        let white = frame.mean_luma >= FLASH_LUMA_THRESHOLD;
        if white && !was_white {
            onsets.push(frame.pts_us);
        }
        was_white = white;
    }
    onsets
}

/// Amplitude of the `freq` component of `samples`, scaled so a full-window sine of amplitude `a` gives `a`.
pub fn goertzel_amplitude(samples: &[f32], freq: f32, sample_rate: u32) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let coeff = 2.0 * (2.0 * PI * freq / sample_rate as f32).cos();
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for &x in samples {
        let s0 = x + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0);
    2.0 * power.sqrt() / samples.len() as f32
}

/// Onset times of the 1 kHz beeps in mono `samples` whose first sample is at `start_us`.
pub fn beep_onsets(samples: &[f32], start_us: i64, sample_rate: u32) -> Vec<i64> {
    let mut onsets = Vec::new();
    let refractory = sample_rate as usize / 2;
    let mut next_allowed = 0usize;
    let mut i = 0usize;
    while i + BEEP_WINDOW <= samples.len() {
        if i >= next_allowed
            && goertzel_amplitude(&samples[i..i + BEEP_WINDOW], BEEP_HZ, sample_rate)
                >= BEEP_AMPLITUDE_THRESHOLD
        {
            // The threshold is half the beep amplitude, crossed when the beep fills half the window.
            let onset = i + BEEP_WINDOW / 2;
            onsets.push(start_us + samples_to_us(onset, sample_rate));
            next_allowed = i + refractory;
        }
        i += BEEP_HOP;
    }
    onsets
}

/// The samples of `samples` that lie outside every beep (with a margin), for tone checks.
pub fn quiet_spans<'a>(
    samples: &'a [f32],
    start_us: i64,
    beeps: &[i64],
    sample_rate: u32,
) -> Vec<&'a [f32]> {
    let margin_us = 20_000;
    let beep_us = i64::from(BEEP_MS) * 1000;
    let mut spans = Vec::new();
    let mut from = 0usize;
    for &b in beeps {
        let to = us_to_samples(b - margin_us - start_us, sample_rate).min(samples.len());
        if to > from {
            spans.push(&samples[from..to]);
        }
        from = us_to_samples(b + beep_us + margin_us - start_us, sample_rate).min(samples.len());
    }
    if from < samples.len() {
        spans.push(&samples[from..]);
    }
    spans
}

/// Pairs flashes with beeps within `tolerance_us`; only markers inside the other track's span need a partner.
pub fn align(
    flashes: &[i64],
    beeps: &[i64],
    video_span_us: (i64, i64),
    audio_span_us: (i64, i64),
    tolerance_us: i64,
) -> Result<Vec<MarkerPair>, SyncError> {
    if flashes.is_empty() {
        return Err(SyncError::NoFlashes);
    }
    if beeps.is_empty() {
        return Err(SyncError::NoBeeps);
    }
    let inside =
        |t: i64, (first, last): (i64, i64)| t >= first + tolerance_us && t <= last - tolerance_us;
    let mut pairs = Vec::with_capacity(flashes.len());
    for &flash_us in flashes {
        let nearest = beeps.iter().copied().min_by_key(|b| (b - flash_us).abs());
        match nearest {
            Some(beep_us) if (beep_us - flash_us).abs() <= tolerance_us => {
                pairs.push(MarkerPair { flash_us, beep_us })
            }
            _ if !inside(flash_us, audio_span_us) => {}
            _ => {
                return Err(SyncError::UnmatchedFlash {
                    flash_us,
                    nearest_us: nearest,
                    tolerance_us,
                });
            }
        }
    }
    for &beep_us in beeps {
        if inside(beep_us, video_span_us) && !pairs.iter().any(|p| p.beep_us == beep_us) {
            return Err(SyncError::UnmatchedBeep {
                beep_us,
                tolerance_us,
            });
        }
    }
    if pairs.is_empty() {
        return Err(SyncError::NoFlashes);
    }
    Ok(pairs)
}

/// Checks every beep - flash offset is within `tolerance_us` of `expected_us` (half a frame for the harness).
pub fn check_offsets(
    pairs: &[MarkerPair],
    expected_us: i64,
    tolerance_us: i64,
) -> Result<(), SyncError> {
    match pairs
        .iter()
        .find(|p| (p.offset_us() - expected_us).abs() > tolerance_us)
    {
        Some(p) => Err(SyncError::OffsetOutOfRange {
            flash_us: p.flash_us,
            offset_us: p.offset_us(),
            expected_us,
            tolerance_us,
        }),
        None => Ok(()),
    }
}

/// Decodes H.264 Annex B access units to their mean luma.
pub struct H264LumaDecoder {
    inner: Decoder,
    pending_pts: VecDeque<i64>,
}

impl H264LumaDecoder {
    pub fn new() -> Result<Self, SyncError> {
        let inner = Decoder::new().map_err(|e| SyncError::H264(e.to_string()))?;
        Ok(Self {
            inner,
            pending_pts: VecDeque::new(),
        })
    }

    /// Feeds parameter sets (extra data) that produce no picture.
    pub fn prime(&mut self, parameter_sets: &[u8]) -> Result<(), SyncError> {
        match self.inner.decode(parameter_sets) {
            Ok(None) => Ok(()),
            Ok(Some(_)) => Err(SyncError::H264("parameter sets produced a picture".into())),
            Err(e) => Err(SyncError::H264(e.to_string())),
        }
    }

    /// Decodes one access unit; returns the frame it completes, if any (decoders may lag a few frames).
    pub fn decode(
        &mut self,
        access_unit: &[u8],
        pts_us: i64,
    ) -> Result<Option<LumaFrame>, SyncError> {
        self.pending_pts.push_back(pts_us);
        let decoded = self
            .inner
            .decode(access_unit)
            .map_err(|e| SyncError::H264(e.to_string()))?;
        Ok(decoded.map(|yuv| {
            let mean_luma = mean_luma(&yuv);
            let pts_us = self.pending_pts.pop_front().unwrap_or(pts_us);
            LumaFrame { pts_us, mean_luma }
        }))
    }

    pub fn finish(mut self) -> Result<Vec<LumaFrame>, SyncError> {
        let frames = self
            .inner
            .flush_remaining()
            .map_err(|e| SyncError::H264(e.to_string()))?;
        Ok(frames
            .iter()
            .filter_map(|yuv| {
                self.pending_pts.pop_front().map(|pts_us| LumaFrame {
                    pts_us,
                    mean_luma: mean_luma(yuv),
                })
            })
            .collect())
    }
}

fn mean_luma(yuv: &impl YUVSource) -> f32 {
    let (w, h) = yuv.dimensions();
    let stride = yuv.strides().0;
    let y = yuv.y();
    let (mut sum, mut n) = (0u64, 0u64);
    for row in (0..h).step_by(4) {
        let line = &y[row * stride..row * stride + w];
        for &px in line.iter().step_by(4) {
            sum += u64::from(px);
            n += 1;
        }
    }
    if n == 0 { 0.0 } else { sum as f32 / n as f32 }
}

/// Decodes Opus packets to the left channel of 48 kHz stereo PCM.
pub struct OpusMonoDecoder {
    state: NonNull<unsafe_libopus::OpusDecoder>,
    pcm: Vec<f32>,
}

// The decoder state is plain heap memory owned exclusively by this value.
unsafe impl Send for OpusMonoDecoder {}

impl OpusMonoDecoder {
    pub fn new() -> Result<Self, SyncError> {
        let mut err = 0;
        // SAFETY: valid constant arguments; `err` outlives the call.
        let raw = unsafe { unsafe_libopus::opus_decoder_create(SAMPLE_RATE as i32, 2, &mut err) };
        let state = NonNull::new(raw)
            .filter(|_| err == unsafe_libopus::OPUS_OK)
            .ok_or_else(|| SyncError::Opus(format!("opus_decoder_create: {err}")))?;
        Ok(Self {
            state,
            pcm: vec![0.0; MAX_OPUS_FRAME * 2],
        })
    }

    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<f32>, SyncError> {
        let len =
            i32::try_from(packet.len()).map_err(|_| SyncError::Opus("packet too large".into()))?;
        // SAFETY: `pcm` holds `MAX_OPUS_FRAME` stereo samples and `packet` is `len` bytes.
        let n = unsafe {
            unsafe_libopus::opus_decode_float(
                self.state.as_ptr(),
                packet.as_ptr(),
                len,
                self.pcm.as_mut_ptr(),
                MAX_OPUS_FRAME as i32,
                0,
            )
        };
        let n =
            usize::try_from(n).map_err(|_| SyncError::Opus(format!("opus_decode_float: {n}")))?;
        Ok(self.pcm[..n * 2].iter().step_by(2).copied().collect())
    }
}

impl Drop for OpusMonoDecoder {
    fn drop(&mut self) {
        // SAFETY: created by `opus_decoder_create` and destroyed once.
        unsafe { unsafe_libopus::opus_decoder_destroy(self.state.as_ptr()) }
    }
}

pub fn samples_to_us(samples: usize, sample_rate: u32) -> i64 {
    (samples as i64 * 1_000_000) / i64::from(sample_rate)
}

fn us_to_samples(us: i64, sample_rate: u32) -> usize {
    usize::try_from(us.max(0) * i64::from(sample_rate) / 1_000_000).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_US: i64 = 33_333;

    fn luma(values: &[f32]) -> Vec<LumaFrame> {
        values
            .iter()
            .enumerate()
            .map(|(i, &mean_luma)| LumaFrame {
                pts_us: i as i64 * FRAME_US,
                mean_luma,
            })
            .collect()
    }

    fn marker_audio(seconds: usize, beeps_at_us: &[i64]) -> Vec<f32> {
        let n = seconds * SAMPLE_RATE as usize;
        let beep_len = (SAMPLE_RATE * BEEP_MS / 1000) as usize;
        let mut out: Vec<f32> = (0..n)
            .map(|i| (2.0 * PI * TONE_HZ * i as f32 / SAMPLE_RATE as f32).sin() * 0.05)
            .collect();
        for &b in beeps_at_us {
            let start = us_to_samples(b, SAMPLE_RATE);
            for (j, s) in out.iter_mut().skip(start).take(beep_len).enumerate() {
                *s = (2.0 * PI * BEEP_HZ * j as f32 / SAMPLE_RATE as f32).sin() * 0.5;
            }
        }
        out
    }

    #[test]
    fn flash_onsets_take_the_first_frame_of_each_white_run() {
        let frames = luma(&[40.0, 40.0, 235.0, 40.0, 40.0, 235.0, 235.0, 40.0]);
        assert_eq!(flash_onsets(&frames), [2 * FRAME_US, 5 * FRAME_US]);
    }

    #[test]
    fn flash_onsets_ignore_mid_grey_and_count_a_leading_flash() {
        let frames = luma(&[235.0, 100.0, 120.0, 40.0]);
        assert_eq!(flash_onsets(&frames), [0]);
        assert!(flash_onsets(&luma(&[40.0, 127.0, 60.0])).is_empty());
    }

    #[test]
    fn goertzel_measures_the_tone_amplitude() {
        let tone = marker_audio(1, &[]);
        let at_440 = goertzel_amplitude(&tone, TONE_HZ, SAMPLE_RATE);
        assert!((at_440 - 0.05).abs() < 0.002, "{at_440}");
        for off in [300.0, 600.0, BEEP_HZ] {
            assert!(
                goertzel_amplitude(&tone, off, SAMPLE_RATE) < 0.005,
                "{off} Hz"
            );
        }
        assert_eq!(goertzel_amplitude(&[], TONE_HZ, SAMPLE_RATE), 0.0);
    }

    #[test]
    fn beep_onsets_are_found_within_a_millisecond() {
        let beeps = [250_000, 1_250_000, 2_250_000];
        let audio = marker_audio(3, &beeps);
        let start_us = -6_500;
        let found = beep_onsets(&audio, start_us, SAMPLE_RATE);
        assert_eq!(found.len(), beeps.len(), "{found:?}");
        for (f, b) in found.iter().zip(beeps) {
            assert!((f - (b + start_us)).abs() <= 1_000, "found {f}, beep {b}");
        }
    }

    #[test]
    fn the_tone_alone_has_no_beeps() {
        assert!(beep_onsets(&marker_audio(2, &[]), 0, SAMPLE_RATE).is_empty());
    }

    #[test]
    fn quiet_spans_skip_the_beeps() {
        let beeps = [500_000, 1_500_000];
        let audio = marker_audio(2, &beeps);
        let spans = quiet_spans(&audio, 0, &beeps, SAMPLE_RATE);
        assert_eq!(spans.len(), 3);
        for span in spans {
            assert!(goertzel_amplitude(span, BEEP_HZ, SAMPLE_RATE) < 0.01);
            assert!(goertzel_amplitude(span, TONE_HZ, SAMPLE_RATE) > 0.04);
        }
    }

    #[test]
    fn align_pairs_flashes_with_beeps_inside_one_frame() {
        let flashes = [1_000_000, 2_000_000];
        let beeps = [1_010_000, 2_030_000];
        let pairs = align(&flashes, &beeps, (0, 3_000_000), (0, 3_000_000), FRAME_US).unwrap();
        assert_eq!(
            pairs.iter().map(MarkerPair::offset_us).collect::<Vec<_>>(),
            [10_000, 30_000]
        );
    }

    #[test]
    fn align_rejects_a_late_beep() {
        let span = (0, 2_000_000);
        let err = align(&[1_000_000], &[1_040_000], span, span, FRAME_US).unwrap_err();
        assert_eq!(
            err,
            SyncError::UnmatchedFlash {
                flash_us: 1_000_000,
                nearest_us: Some(1_040_000),
                tolerance_us: FRAME_US
            }
        );
    }

    #[test]
    fn align_rejects_a_beep_without_a_flash_inside_the_video() {
        let flashes = [1_000_000];
        let beeps = [1_000_000, 2_000_000];
        let audio = (0, 3_000_000);
        let err = align(&flashes, &beeps, (0, 3_000_000), audio, FRAME_US).unwrap_err();
        assert!(matches!(
            err,
            SyncError::UnmatchedBeep {
                beep_us: 2_000_000,
                ..
            }
        ));
        assert!(align(&flashes, &beeps, (0, 2_000_000), audio, FRAME_US).is_ok());
    }

    #[test]
    fn align_skips_a_flash_past_the_end_of_the_audio() {
        let flashes = [1_000_000, 2_990_000];
        let beeps = [1_005_000];
        let pairs = align(&flashes, &beeps, (0, 3_000_000), (0, 2_980_000), FRAME_US).unwrap();
        assert_eq!(pairs.len(), 1);
        let err = align(&flashes, &beeps, (0, 3_000_000), (0, 3_100_000), FRAME_US).unwrap_err();
        assert!(matches!(
            err,
            SyncError::UnmatchedFlash {
                flash_us: 2_990_000,
                ..
            }
        ));
    }

    fn pair(flash_us: i64, offset_us: i64) -> MarkerPair {
        MarkerPair {
            flash_us,
            beep_us: flash_us + offset_us,
        }
    }

    #[test]
    fn offsets_near_half_a_frame_pass() {
        let pairs = [
            pair(1_000_000, FRAME_US / 2),
            pair(2_000_000, FRAME_US / 2 + 2_000),
        ];
        assert_eq!(check_offsets(&pairs, FRAME_US / 2, FRAME_US / 4), Ok(()));
    }

    #[test]
    fn a_one_frame_regression_fails_although_align_accepts_it() {
        let flashes = [1_000_000];
        let beeps = [1_000_000 - FRAME_US / 2];
        let pairs = align(&flashes, &beeps, (0, 2_000_000), (0, 2_000_000), FRAME_US).unwrap();
        let err = check_offsets(&pairs, FRAME_US / 2, FRAME_US / 4).unwrap_err();
        assert!(
            matches!(err, SyncError::OffsetOutOfRange { offset_us, .. } if offset_us == -FRAME_US / 2),
            "{err:?}"
        );
        let late = [pair(1_000_000, FRAME_US / 2 + FRAME_US / 4 + 1)];
        assert!(check_offsets(&late, FRAME_US / 2, FRAME_US / 4).is_err());
    }

    #[test]
    fn align_needs_markers_on_both_tracks() {
        assert_eq!(
            align(&[], &[1], (0, 1), (0, 1), 1),
            Err(SyncError::NoFlashes)
        );
        assert_eq!(align(&[1], &[], (0, 1), (0, 1), 1), Err(SyncError::NoBeeps));
    }

    #[test]
    fn h264_round_trip_keeps_flashes_and_order() {
        use openh264::OpenH264API;
        use openh264::encoder::{Encoder, EncoderConfig};
        use openh264::formats::YUVBuffer;

        let (w, h) = (64usize, 48usize);
        let mut encoder =
            Encoder::with_api_config(OpenH264API::from_source(), EncoderConfig::new()).unwrap();
        let mut decoder = H264LumaDecoder::new().unwrap();
        let pattern = [false, false, true, false, false, false, true, false];
        let mut frames = Vec::new();
        for (i, &white) in pattern.iter().enumerate() {
            let mut raw = vec![128u8; w * h * 3 / 2];
            raw[..w * h].fill(if white { 235 } else { 40 });
            let yuv = YUVBuffer::from_vec(raw, w, h);
            let bitstream = encoder.encode(&yuv).unwrap().to_vec();
            frames.extend(decoder.decode(&bitstream, i as i64 * FRAME_US).unwrap());
        }
        frames.extend(decoder.finish().unwrap());
        assert_eq!(frames.len(), pattern.len());
        assert_eq!(flash_onsets(&frames), [2 * FRAME_US, 6 * FRAME_US]);
    }

    #[test]
    fn opus_round_trip_keeps_the_beep_and_tone() {
        let beeps = [300_000];
        let audio = marker_audio(1, &beeps);
        let mut err = 0;
        // SAFETY: valid constants; destroyed below.
        let enc = unsafe {
            unsafe_libopus::opus_encoder_create(
                SAMPLE_RATE as i32,
                2,
                unsafe_libopus::OPUS_APPLICATION_AUDIO,
                &mut err,
            )
        };
        assert_eq!(err, unsafe_libopus::OPUS_OK);
        let mut decoder = OpusMonoDecoder::new().unwrap();
        let mut decoded = Vec::new();
        let mut packet = vec![0u8; 4000];
        for chunk in audio.as_chunks::<960>().0 {
            let stereo: Vec<f32> = chunk.iter().flat_map(|&s| [s, s]).collect();
            // SAFETY: `stereo` holds 960 stereo samples and `packet` 4000 bytes.
            let n = unsafe {
                unsafe_libopus::opus_encode_float(
                    enc,
                    stereo.as_ptr(),
                    960,
                    packet.as_mut_ptr(),
                    4000,
                )
            };
            assert!(n > 0);
            decoded.extend(decoder.decode(&packet[..n as usize]).unwrap());
        }
        // SAFETY: created above and not used afterwards.
        unsafe { unsafe_libopus::opus_encoder_destroy(enc) };
        assert_eq!(decoded.len(), audio.len());

        let found = beep_onsets(&decoded, 0, SAMPLE_RATE);
        assert_eq!(found.len(), 1, "{found:?}");
        // Opus adds 6.5 ms of algorithmic delay that OBS removes through the negative first pts.
        assert!((found[0] - beeps[0] - 6_500).abs() <= 1_500, "{found:?}");
        let quiet = quiet_spans(&decoded, 0, &found, SAMPLE_RATE);
        let tone = goertzel_amplitude(quiet[1], TONE_HZ, SAMPLE_RATE);
        assert!(tone > 0.03, "{tone}");
    }

    #[test]
    fn opus_decoder_rejects_garbage_length() {
        let mut decoder = OpusMonoDecoder::new().unwrap();
        assert!(decoder.decode(&[0xff; 2]).is_err());
    }
}
