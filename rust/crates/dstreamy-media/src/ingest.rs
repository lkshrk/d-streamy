//! Checks on producer media before it reaches the session (ADR 0004, "Ingest is validated against the specs").

use crate::annexb::{self, H264_NAL_IDR, H264_NAL_PPS, H264_NAL_SPS};

const OPUS_PACKET_US: u32 = 20_000;

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
    #[error("Opus packet is empty")]
    OpusEmpty,
    #[error("Opus code 3 packet has no frame count byte")]
    OpusNoFrameCount,
    #[error("Opus packet lasts {0} µs, not 20 ms")]
    OpusDuration(u32),
}

/// One attachment's checks; video pts must strictly increase within it.
#[derive(Debug, Default)]
pub struct Ingest {
    last_video_pts: Option<i64>,
}

impl Ingest {
    pub fn video(&mut self, data: &[u8], pts_us: i64, keyframe: bool) -> Result<(), IngestError> {
        if !starts_with_start_code(data) {
            return Err(IngestError::NotAnnexB);
        }
        let types: Vec<u8> = annexb::nal_units(data)
            .filter_map(annexb::h264_nal_type)
            .collect();
        let idr = types.iter().position(|&t| t == H264_NAL_IDR);
        match (keyframe, idr) {
            (true, None) => return Err(IngestError::KeyframeWithoutIdr),
            (true, Some(idr)) if !types[..idr].contains(&H264_NAL_SPS) => {
                return Err(IngestError::KeyframeWithoutSps);
            }
            (true, Some(idr)) if !types[..idr].contains(&H264_NAL_PPS) => {
                return Err(IngestError::KeyframeWithoutPps);
            }
            (false, Some(_)) => return Err(IngestError::IdrWithoutKeyframeFlag),
            _ => {}
        }
        if let Some(last_us) = self.last_video_pts
            && pts_us <= last_us
        {
            return Err(IngestError::Reordered { pts_us, last_us });
        }
        self.last_video_pts = Some(pts_us);
        Ok(())
    }

    pub fn audio(&self, data: &[u8]) -> Result<(), IngestError> {
        match opus_duration_us(data)? {
            OPUS_PACKET_US => Ok(()),
            other => Err(IngestError::OpusDuration(other)),
        }
    }
}

/// H.264 §B.1.1: optional leading zero bytes, then `00 00 01`.
fn starts_with_start_code(data: &[u8]) -> bool {
    let zeros = data.iter().take_while(|&&b| b == 0).count();
    zeros >= 2 && data.get(zeros) == Some(&1)
}

/// RFC 6716 §3.1 (TOC config and code) and §3.2.5 (code 3 frame count `M`).
pub fn opus_duration_us(packet: &[u8]) -> Result<u32, IngestError> {
    let toc = *packet.first().ok_or(IngestError::OpusEmpty)?;
    let config = toc >> 3;
    let frame_us = match config {
        0..=11 => [10_000, 20_000, 40_000, 60_000][usize::from(config & 3)],
        12..=15 => [10_000, 20_000][usize::from(config & 1)],
        _ => [2_500, 5_000, 10_000, 20_000][usize::from(config & 3)],
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => u32::from(*packet.get(1).ok_or(IngestError::OpusNoFrameCount)? & 0x3f),
    };
    Ok(frame_us * frames)
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
            annexb::append_nalu(&mut out, n);
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
    fn h264_7_4_1_2_a_keyframe_carries_an_idr_slice() {
        let mut ingest = Ingest::default();
        assert_eq!(
            ingest.video(&au(&[SPS, PPS, P]), 0, true),
            Err(IngestError::KeyframeWithoutIdr)
        );
    }

    #[test]
    fn h264_7_4_1_2_a_keyframe_has_sps_and_pps_before_its_idr() {
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

    #[test]
    fn rfc6716_3_1_opus_packets_total_20ms_with_any_code() {
        let ingest = Ingest::default();
        // config 31 CELT FB 20 ms, code 0
        assert_eq!(ingest.audio(&[0xF8, 0xFF, 0xFE]), Ok(()));
        // config 30 CELT FB 10 ms, code 1 (two equal frames)
        assert_eq!(ingest.audio(&[0xF1, 0x00, 0x00]), Ok(()));
        // config 30, code 2 (two frames of different sizes)
        assert_eq!(ingest.audio(&[0xF2, 0x01, 0x00, 0x00]), Ok(()));
        // config 29 CELT FB 5 ms, code 3 with M = 4
        assert_eq!(ingest.audio(&[0xEB, 0x04, 0x00]), Ok(()));
        // config 1 SILK NB 20 ms and config 13 Hybrid SWB 20 ms
        assert_eq!(ingest.audio(&[0x08]), Ok(()));
        assert_eq!(ingest.audio(&[0x68]), Ok(()));
    }

    #[test]
    fn rfc6716_3_1_opus_packets_of_other_durations_are_refused() {
        let ingest = Ingest::default();
        assert_eq!(
            ingest.audio(&[0xF9, 0, 0]),
            Err(IngestError::OpusDuration(40_000))
        );
        assert_eq!(
            ingest.audio(&[0xF0]),
            Err(IngestError::OpusDuration(10_000))
        );
        assert_eq!(
            ingest.audio(&[0x18]),
            Err(IngestError::OpusDuration(60_000))
        );
        assert_eq!(
            ingest.audio(&[0xFB, 0x03]),
            Err(IngestError::OpusDuration(60_000))
        );
        assert_eq!(
            ingest.audio(&[0xFB, 0x00]),
            Err(IngestError::OpusDuration(0))
        );
    }

    #[test]
    fn rfc6716_3_2_5_opus_code_3_needs_its_frame_count_byte() {
        let ingest = Ingest::default();
        assert_eq!(ingest.audio(&[0xEB]), Err(IngestError::OpusNoFrameCount));
        assert_eq!(ingest.audio(&[]), Err(IngestError::OpusEmpty));
    }

    #[test]
    fn rfc6716_3_1_the_stereo_bit_is_per_packet_and_not_checked() {
        let ingest = Ingest::default();
        assert_eq!(ingest.audio(&[0xF8, 0xFF, 0xFE]), Ok(()));
        assert_eq!(ingest.audio(&[0xFC, 0xFF, 0xFE]), Ok(()));
    }
}
