//! Checks on producer media before it reaches the session (ADR 0004, "Ingest is validated against the specs").

use crate::annexb::{H264_NAL_IDR, H264_NAL_PPS, H264_NAL_SPS};

const OPUS_PACKET_US: u32 = 20_000;
/// RFC 6716 §3.2.1: no frame is longer than 1275 bytes.
const OPUS_MAX_FRAME: usize = 1275;
/// One Opus packet per RTP payload (RFC 7587 §4.2) must fit a 1500-byte MTU.
pub const OPUS_MAX_PACKET: usize = 1500;
/// Largest pts step between consecutive frames of one attachment, in either direction.
pub const MAX_PTS_JUMP_US: i64 = 10_000_000;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IngestError {
    #[error("video is not Annex B: it does not start with a start code")]
    NotAnnexB,
    #[error("keyframe has no IDR slice")]
    KeyframeWithoutIdr,
    #[error("keyframe has no SPS before its IDR slice")]
    KeyframeWithoutSps,
    #[error("keyframe has no PPS before its IDR slice")]
    KeyframeWithoutPps,
    #[error("IDR slice in a frame not flagged as a keyframe")]
    IdrWithoutKeyframeFlag,
    #[error("B-frames are enabled: video pts {pts_us} µs does not follow {last_us} µs")]
    Reordered { pts_us: i64, last_us: i64 },
    #[error("audio pts {pts_us} µs does not follow {last_us} µs")]
    AudioReordered { pts_us: i64, last_us: i64 },
    #[error("{track} pts {pts_us} µs jumps more than 10 s from the previous frame at {last_us} µs")]
    PtsJump {
        track: &'static str,
        pts_us: i64,
        last_us: i64,
    },
    #[error("Opus packet is empty")]
    OpusEmpty,
    #[error("Opus packet of {0} bytes exceeds {OPUS_MAX_PACKET}")]
    OpusTooLong(usize),
    #[error("Opus packet is malformed: {0}")]
    OpusMalformed(&'static str),
    #[error("Opus packet lasts {0} µs, not 20 ms")]
    OpusDuration(u32),
}

/// One attachment's checks: pts strictly increase per track and never jump more than 10 s from the last frame.
#[derive(Debug, Default)]
pub struct Ingest {
    last_video_pts: Option<i64>,
    last_audio_pts: Option<i64>,
}

impl Ingest {
    pub fn video(&mut self, data: &[u8], pts_us: i64, keyframe: bool) -> Result<(), IngestError> {
        if !starts_with_start_code(data) {
            return Err(IngestError::NotAnnexB);
        }
        let (mut sps, mut pps, mut idr) = (false, false, None);
        for t in nal_types(data) {
            match t {
                H264_NAL_SPS => sps = true,
                H264_NAL_PPS => pps = true,
                H264_NAL_IDR if idr.is_none() => idr = Some((sps, pps)),
                _ => {}
            }
        }
        match (keyframe, idr) {
            (true, None) => return Err(IngestError::KeyframeWithoutIdr),
            (true, Some((false, _))) => return Err(IngestError::KeyframeWithoutSps),
            (true, Some((_, false))) => return Err(IngestError::KeyframeWithoutPps),
            (false, Some(_)) => return Err(IngestError::IdrWithoutKeyframeFlag),
            _ => {}
        }
        if let Some(last_us) = self.last_video_pts
            && pts_us <= last_us
        {
            return Err(IngestError::Reordered { pts_us, last_us });
        }
        self.check_jump("video", pts_us)?;
        self.last_video_pts = Some(pts_us);
        Ok(())
    }

    pub fn audio(&mut self, data: &[u8], pts_us: i64) -> Result<(), IngestError> {
        match opus_duration_us(data)? {
            OPUS_PACKET_US => {}
            other => return Err(IngestError::OpusDuration(other)),
        }
        if let Some(last_us) = self.last_audio_pts
            && pts_us <= last_us
        {
            return Err(IngestError::AudioReordered { pts_us, last_us });
        }
        self.check_jump("audio", pts_us)?;
        self.last_audio_pts = Some(pts_us);
        Ok(())
    }

    /// The previous frame of either track bounds the next one, so neither track can drag the timeline far.
    fn check_jump(&self, track: &'static str, pts_us: i64) -> Result<(), IngestError> {
        let last = match (self.last_video_pts, self.last_audio_pts) {
            (Some(v), Some(a)) => Some(if track == "video" { v } else { a }),
            (v, a) => v.or(a),
        };
        if let Some(last_us) = last
            && pts_us.abs_diff(last_us) > MAX_PTS_JUMP_US.unsigned_abs()
        {
            return Err(IngestError::PtsJump {
                track,
                pts_us,
                last_us,
            });
        }
        Ok(())
    }
}

/// H.264 §B.1.1: optional leading zero bytes, then `00 00 01`.
fn starts_with_start_code(data: &[u8]) -> bool {
    let zeros = data.iter().take_while(|&&b| b == 0).count();
    zeros >= 2 && data.get(zeros) == Some(&1)
}

/// The type (H.264 §7.3.1) of every NAL unit after a `00 00 01`, in one pass without allocating.
fn nal_types(data: &[u8]) -> impl Iterator<Item = u8> + '_ {
    data.windows(4)
        .filter(|w| w[..3] == [0, 0, 1])
        .map(|w| w[3] & 0x1f)
}

/// RFC 6716 §3.1 (TOC) and §3.2 (framing rules R1–R7); returns the packet's total duration.
pub fn opus_duration_us(packet: &[u8]) -> Result<u32, IngestError> {
    let malformed = IngestError::OpusMalformed;
    let (&toc, rest) = packet.split_first().ok_or(IngestError::OpusEmpty)?;
    if packet.len() > OPUS_MAX_PACKET {
        return Err(IngestError::OpusTooLong(packet.len()));
    }
    let config = toc >> 3;
    let frame_us: u32 = match config {
        0..=11 => [10_000, 20_000, 40_000, 60_000][usize::from(config & 3)],
        12..=15 => [10_000, 20_000][usize::from(config & 1)],
        _ => [2_500, 5_000, 10_000, 20_000][usize::from(config & 3)],
    };
    let frames = match toc & 3 {
        0 => {
            if rest.len() > OPUS_MAX_FRAME {
                return Err(malformed("R2: code 0 frame longer than 1275 bytes"));
            }
            1
        }
        1 => {
            if rest.len() % 2 != 0 {
                return Err(malformed("R3: code 1 payload length is odd"));
            }
            if rest.len() / 2 > OPUS_MAX_FRAME {
                return Err(malformed("R2: code 1 frame longer than 1275 bytes"));
            }
            2
        }
        2 => {
            let (n1, used) =
                frame_length(rest).ok_or(malformed("R4: code 2 packet has no frame length"))?;
            if n1 > rest.len() - used {
                return Err(malformed("R4: code 2 first frame runs past the packet"));
            }
            if rest.len() - used - n1 > OPUS_MAX_FRAME {
                return Err(malformed("R2: code 2 second frame longer than 1275 bytes"));
            }
            2
        }
        _ => code3_frames(rest, frame_us)?,
    };
    Ok(frame_us * frames)
}

/// RFC 6716 §3.2.1: one byte below 252, otherwise two bytes `b0 + 4 * b1`.
fn frame_length(data: &[u8]) -> Option<(usize, usize)> {
    match *data.first()? {
        b @ 0..=251 => Some((usize::from(b), 1)),
        b => Some((usize::from(b) + 4 * usize::from(*data.get(1)?), 2)),
    }
}

/// RFC 6716 §3.2.5: frame count byte `v p M`, optional padding, then CBR or VBR frames (R5–R7).
fn code3_frames(rest: &[u8], frame_us: u32) -> Result<u32, IngestError> {
    let malformed = IngestError::OpusMalformed;
    let (&fc, mut data) = rest
        .split_first()
        .ok_or(malformed("R6/R7: code 3 packet has no frame count byte"))?;
    let (vbr, padded, m) = (fc & 0x80 != 0, fc & 0x40 != 0, usize::from(fc & 0x3f));
    if m == 0 {
        return Err(malformed("R5: code 3 frame count is 0"));
    }
    if m as u32 * frame_us > 120_000 {
        return Err(malformed("R5: code 3 packet longer than 120 ms"));
    }
    let mut padding = 0usize;
    if padded {
        loop {
            let (&b, tail) = data.split_first().ok_or(malformed(
                "R6/R7: code 3 padding length runs past the packet",
            ))?;
            data = tail;
            padding += usize::from(b.min(254));
            if b != 255 {
                break;
            }
        }
    }
    let body = data
        .len()
        .checked_sub(padding)
        .ok_or(malformed("R6/R7: code 3 padding runs past the packet"))?;
    if vbr {
        let mut used = 0usize;
        let mut total = 0usize;
        for _ in 0..m - 1 {
            let (n, len) = frame_length(&data[used..body.max(used)]).ok_or(malformed(
                "R7: code 3 VBR frame length runs past the packet",
            ))?;
            used += len;
            total += n;
        }
        let last = body
            .checked_sub(used + total)
            .ok_or(malformed("R7: code 3 VBR frames run past the packet"))?;
        if last > OPUS_MAX_FRAME {
            return Err(malformed(
                "R2: code 3 last VBR frame longer than 1275 bytes",
            ));
        }
    } else {
        if body % m != 0 {
            return Err(malformed(
                "R6: code 3 CBR payload does not divide into M frames",
            ));
        }
        if body / m > OPUS_MAX_FRAME {
            return Err(malformed("R2: code 3 CBR frame longer than 1275 bytes"));
        }
    }
    Ok(m as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0x67, 0x42, 0xC0, 0x1F];
    const PPS: &[u8] = &[0x68, 0xCE, 0x3C, 0x80];
    const IDR: &[u8] = &[0x65, 0x88, 0x84, 0x00];
    const P: &[u8] = &[0x41, 0x9A, 0x02];
    const SEI: &[u8] = &[0x06, 0x05, 0x01];
    const AUD: &[u8] = &[0x09, 0xF0];

    fn au(nalus: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for n in nalus {
            crate::annexb::append_nalu(&mut out, n);
        }
        out
    }

    #[test]
    fn h264_b_1_1_video_must_start_with_a_start_code() {
        let mut ingest = Ingest::default();
        let keyframe = au(&[SPS, PPS, IDR]);
        assert_eq!(
            ingest.video(&keyframe[4..], 0, true),
            Err(IngestError::NotAnnexB)
        );
        let mut avcc = Vec::new();
        for n in [SPS, PPS, IDR] {
            avcc.extend_from_slice(&(n.len() as u32).to_be_bytes());
            avcc.extend_from_slice(n);
        }
        assert_eq!(ingest.video(&avcc, 0, true), Err(IngestError::NotAnnexB));
        assert_eq!(ingest.video(&[], 0, false), Err(IngestError::NotAnnexB));
        assert_eq!(
            ingest.video(&[0, 1, 0x41], 0, false),
            Err(IngestError::NotAnnexB)
        );
    }

    #[test]
    fn h264_b_1_2_three_and_four_byte_start_codes_are_accepted() {
        let mut ingest = Ingest::default();
        let mut three = vec![0, 0, 1];
        three.extend_from_slice(P);
        assert_eq!(ingest.video(&three, 0, false), Ok(()));
        assert_eq!(
            ingest.video(&au(&[AUD, SPS, PPS, SEI, IDR]), 1, true),
            Ok(())
        );
    }

    #[test]
    fn adr0004_a_keyframe_carries_an_idr_slice() {
        let mut ingest = Ingest::default();
        assert_eq!(
            ingest.video(&au(&[SPS, PPS, P]), 0, true),
            Err(IngestError::KeyframeWithoutIdr)
        );
    }

    #[test]
    fn adr0004_a_keyframe_has_sps_and_pps_before_its_idr() {
        let mut ingest = Ingest::default();
        assert_eq!(
            ingest.video(&au(&[PPS, IDR]), 0, true),
            Err(IngestError::KeyframeWithoutSps)
        );
        assert_eq!(
            ingest.video(&au(&[SPS, IDR]), 0, true),
            Err(IngestError::KeyframeWithoutPps)
        );
        assert_eq!(
            ingest.video(&au(&[IDR, SPS, PPS]), 0, true),
            Err(IngestError::KeyframeWithoutSps)
        );
        assert_eq!(ingest.video(&au(&[SPS, PPS, IDR, IDR]), 0, true), Ok(()));
    }

    #[test]
    fn h264_table_7_1_an_idr_without_the_keyframe_flag_is_mislabelled() {
        let mut ingest = Ingest::default();
        assert_eq!(
            ingest.video(&au(&[SPS, PPS, IDR]), 0, false),
            Err(IngestError::IdrWithoutKeyframeFlag)
        );
        assert_eq!(ingest.video(&au(&[P]), 0, false), Ok(()));
    }

    #[test]
    fn specs_obs_bf0_pts_equals_dts_so_video_pts_strictly_increase() {
        let mut ingest = Ingest::default();
        let p = au(&[P]);
        assert_eq!(ingest.video(&au(&[SPS, PPS, IDR]), -66_666, true), Ok(()));
        assert_eq!(ingest.video(&p, -33_333, false), Ok(()));
        let err = ingest.video(&p, -66_666, false).unwrap_err();
        assert_eq!(
            err,
            IngestError::Reordered {
                pts_us: -66_666,
                last_us: -33_333
            }
        );
        assert!(err.to_string().starts_with("B-frames are enabled"), "{err}");
        assert!(matches!(
            ingest.video(&p, -33_333, false),
            Err(IngestError::Reordered { .. })
        ));
        assert_eq!(ingest.video(&p, 0, false), Ok(()));
    }

    #[test]
    fn a_rejected_frame_does_not_advance_the_pts() {
        let mut ingest = Ingest::default();
        assert_eq!(ingest.video(&au(&[P]), 100, false), Ok(()));
        assert!(ingest.video(&au(&[SPS, PPS, IDR]), 200, false).is_err());
        assert_eq!(ingest.video(&au(&[P]), 150, false), Ok(()));
    }

    fn opus(packet: &[u8]) -> Result<(), IngestError> {
        Ingest::default().audio(packet, 0)
    }

    fn malformed(packet: &[u8], rule: &str) {
        match opus(packet) {
            Err(IngestError::OpusMalformed(m)) => assert!(m.starts_with(rule), "{m}"),
            other => panic!("{packet:02x?}: {other:?}, expected {rule}"),
        }
    }

    #[test]
    fn rfc6716_3_1_opus_packets_total_20ms_with_any_code() {
        // config 31 CELT FB 20 ms, code 0
        assert_eq!(opus(&[0xF8, 0xFF, 0xFE]), Ok(()));
        // config 30 CELT FB 10 ms, code 1 (two equal frames)
        assert_eq!(opus(&[0xF1, 0x00, 0x00]), Ok(()));
        // config 30, code 2 (two frames of different sizes)
        assert_eq!(opus(&[0xF2, 0x01, 0x00, 0x00]), Ok(()));
        // config 29 CELT FB 5 ms, code 3 CBR with M = 4
        assert_eq!(opus(&[0xEB, 0x04, 0x00, 0x00, 0x00, 0x00]), Ok(()));
        // config 1 SILK NB 20 ms and config 13 Hybrid SWB 20 ms
        assert_eq!(opus(&[0x08]), Ok(()));
        assert_eq!(opus(&[0x68]), Ok(()));
    }

    #[test]
    fn rfc6716_table_2_every_config_has_its_frame_size() {
        let sizes = |configs: std::ops::RangeInclusive<u8>, us: &[u32]| {
            for (c, &want) in configs.zip(us.iter().cycle()) {
                assert_eq!(opus_duration_us(&[c << 3]), Ok(want), "config {c}");
            }
        };
        sizes(0..=11, &[10_000, 20_000, 40_000, 60_000]);
        sizes(12..=15, &[10_000, 20_000]);
        sizes(16..=31, &[2_500, 5_000, 10_000, 20_000]);
    }

    #[test]
    fn rfc6716_3_1_opus_packets_of_other_durations_are_refused() {
        assert_eq!(opus(&[0xF9, 0, 0]), Err(IngestError::OpusDuration(40_000)));
        assert_eq!(opus(&[0xF0]), Err(IngestError::OpusDuration(10_000)));
        assert_eq!(opus(&[0x18]), Err(IngestError::OpusDuration(60_000)));
        assert_eq!(
            opus(&[0xFB, 0x03, 0, 0, 0]),
            Err(IngestError::OpusDuration(60_000))
        );
    }

    #[test]
    fn rfc6716_r1_a_packet_has_at_least_one_byte() {
        assert_eq!(opus(&[]), Err(IngestError::OpusEmpty));
    }

    #[test]
    fn rfc6716_r2_no_frame_is_longer_than_1275_bytes() {
        let mut code0 = vec![0xF8];
        code0.resize(1 + 1276, 0);
        malformed(&code0, "R2");
        code0.truncate(1 + 1275);
        assert_eq!(opus(&code0), Ok(()));
        let mut code1 = vec![0xF1];
        code1.resize(1 + 2 * 1276, 0);
        assert_eq!(opus(&code1), Err(IngestError::OpusTooLong(code1.len())));
    }

    #[test]
    fn opus_packets_over_1500_bytes_are_refused() {
        let mut packet = vec![0xF8];
        packet.resize(8 << 20, 0);
        assert_eq!(opus(&packet), Err(IngestError::OpusTooLong(8 << 20)));
    }

    #[test]
    fn rfc6716_r3_code_1_payload_splits_evenly() {
        malformed(&[0xF1, 0x00, 0x00, 0x00], "R3");
        assert_eq!(opus(&[0xF1]), Ok(()));
    }

    #[test]
    fn rfc6716_r4_code_2_first_frame_length_fits_the_packet() {
        malformed(&[0xF2], "R4");
        malformed(&[0xF2, 0x05, 0x00], "R4");
        malformed(&[0xF2, 0xFC], "R4");
        assert_eq!(opus(&[0xF2, 0x00]), Ok(()));
        let mut two_byte = vec![0xF2, 0xFC, 0x01];
        two_byte.resize(3 + 256, 0);
        assert_eq!(opus(&two_byte), Ok(()));
    }

    #[test]
    fn rfc6716_r5_code_3_has_at_least_one_frame_and_at_most_120ms() {
        malformed(&[0xFB, 0x00], "R5");
        malformed(&[0xEB, 0x31], "R5");
    }

    #[test]
    fn rfc6716_r6_code_3_cbr_payload_divides_into_m_frames_after_padding() {
        malformed(&[0xEB], "R6");
        malformed(&[0xEB, 0x04, 0x00, 0x00, 0x00], "R6");
        malformed(&[0xEB, 0x44, 0x05, 0x00], "R6");
        malformed(&[0xEB, 0x44, 0xFF], "R6");
        assert_eq!(opus(&[0xEB, 0x44, 0x01, 0, 0, 0, 0, 0]), Ok(()));
        let mut long_pad = vec![0xEB, 0x44, 0xFF, 0x00];
        long_pad.resize(4 + 254, 0);
        assert_eq!(opus(&long_pad), Ok(()));
    }

    #[test]
    fn rfc6716_r7_code_3_vbr_lengths_fit_the_packet() {
        malformed(&[0xEB, 0x84, 0x01, 0x01], "R7");
        malformed(&[0xEB, 0x84, 0x05, 0x05, 0x05, 0x00], "R7");
        malformed(&[0xEB, 0xC4, 0x02, 0x01, 0x01, 0x01, 0x00], "R7");
        assert_eq!(
            opus(&[0xEB, 0x84, 0x01, 0x00, 0x02, 0xAA, 0xBB, 0xBB, 0xCC]),
            Ok(())
        );
    }

    #[test]
    fn rfc6716_3_1_the_stereo_bit_is_per_packet_and_not_checked() {
        assert_eq!(opus(&[0xF8, 0xFF, 0xFE]), Ok(()));
        assert_eq!(opus(&[0xFC, 0xFF, 0xFE]), Ok(()));
    }

    #[test]
    fn audio_pts_strictly_increase() {
        let mut ingest = Ingest::default();
        let packet = [0xF8, 0xFF, 0xFE];
        assert_eq!(ingest.audio(&packet, 0), Ok(()));
        assert_eq!(ingest.audio(&packet, 20_000), Ok(()));
        assert_eq!(
            ingest.audio(&packet, 20_000),
            Err(IngestError::AudioReordered {
                pts_us: 20_000,
                last_us: 20_000
            })
        );
    }

    #[test]
    fn a_pts_jump_beyond_10s_is_refused_on_either_track() {
        let mut ingest = Ingest::default();
        let packet = [0xF8, 0xFF, 0xFE];
        assert_eq!(ingest.audio(&packet, 0), Ok(()));
        assert_eq!(ingest.audio(&packet, 20_000), Ok(()));
        let err = ingest.audio(&packet, i64::MAX).unwrap_err();
        assert!(
            matches!(err, IngestError::PtsJump { track: "audio", .. }),
            "{err}"
        );
        assert!(err.to_string().starts_with("audio pts"), "{err}");
        assert_eq!(ingest.audio(&packet, 40_000), Ok(()));

        let mut ingest = Ingest::default();
        let keyframe = au(&[SPS, PPS, IDR]);
        assert_eq!(ingest.video(&keyframe, i64::MIN, true), Ok(()));
        let err = ingest.audio(&packet, 0).unwrap_err();
        assert!(matches!(err, IngestError::PtsJump { .. }), "{err}");
        assert!(matches!(
            ingest.video(&au(&[P]), 0, false),
            Err(IngestError::PtsJump { track: "video", .. })
        ));
        assert_eq!(ingest.video(&au(&[P]), i64::MIN + 33_333, false), Ok(()));
        assert_eq!(ingest.audio(&packet, i64::MIN + 20_000), Ok(()));
    }
}
