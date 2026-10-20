//! RTP depacketizers for H.264 (RFC 6184) and H.265 (RFC 7798), producing Annex B with 4-byte start codes.

use thiserror::Error;

pub const START_CODE: [u8; 4] = [0, 0, 0, 1];

const H264_STAP_A: u8 = 24;
const H264_FU_A: u8 = 28;
const H265_AP: u8 = 48;
const H265_FU: u8 = 49;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    H265,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DepacketizeError {
    #[error("RTP payload too short")]
    Truncated,
    #[error("fragment without a start fragment")]
    OrphanFragment,
    #[error("unsupported NAL unit type {0}")]
    Unsupported(u8),
}

/// Collects the NAL units of one access unit across RTP packets.
#[derive(Debug)]
pub struct Depacketizer {
    codec: VideoCodec,
    nalus: Vec<Vec<u8>>,
    fragment: Option<Vec<u8>>,
}

impl Depacketizer {
    pub fn new(codec: VideoCodec) -> Self {
        Self {
            codec,
            nalus: Vec::new(),
            fragment: None,
        }
    }

    pub fn push(&mut self, payload: &[u8]) -> Result<(), DepacketizeError> {
        match self.codec {
            VideoCodec::H264 => self.push_h264(payload),
            VideoCodec::H265 => self.push_h265(payload),
        }
    }

    /// The access unit collected so far, as Annex B; resets the depacketizer.
    pub fn take_frame(&mut self) -> Vec<u8> {
        self.fragment = None;
        let nalus = std::mem::take(&mut self.nalus);
        annexb(&nalus.iter().map(Vec::as_slice).collect::<Vec<_>>())
    }

    pub fn reset(&mut self) {
        self.nalus.clear();
        self.fragment = None;
    }

    fn push_h264(&mut self, payload: &[u8]) -> Result<(), DepacketizeError> {
        let header = *payload.first().ok_or(DepacketizeError::Truncated)?;
        match header & 0x1F {
            1..=23 => self.nalus.push(payload.to_vec()),
            H264_STAP_A => self.push_aggregate(&payload[1..])?,
            H264_FU_A => {
                let fu = *payload.get(1).ok_or(DepacketizeError::Truncated)?;
                let nal_header = (header & 0xE0) | (fu & 0x1F);
                self.push_fragment(fu, &[nal_header], &payload[2..])?;
            }
            other => return Err(DepacketizeError::Unsupported(other)),
        }
        Ok(())
    }

    fn push_h265(&mut self, payload: &[u8]) -> Result<(), DepacketizeError> {
        if payload.len() < 2 {
            return Err(DepacketizeError::Truncated);
        }
        match (payload[0] >> 1) & 0x3F {
            0..=47 => self.nalus.push(payload.to_vec()),
            H265_AP => self.push_aggregate(&payload[2..])?,
            H265_FU => {
                let fu = *payload.get(2).ok_or(DepacketizeError::Truncated)?;
                let nal_header = [(payload[0] & 0x81) | ((fu & 0x3F) << 1), payload[1]];
                self.push_fragment(fu, &nal_header, &payload[3..])?;
            }
            other => return Err(DepacketizeError::Unsupported(other)),
        }
        Ok(())
    }

    fn push_aggregate(&mut self, mut body: &[u8]) -> Result<(), DepacketizeError> {
        while !body.is_empty() {
            if body.len() < 2 {
                return Err(DepacketizeError::Truncated);
            }
            let len = usize::from(u16::from_be_bytes([body[0], body[1]]));
            let nalu = body.get(2..2 + len).ok_or(DepacketizeError::Truncated)?;
            self.nalus.push(nalu.to_vec());
            body = &body[2 + len..];
        }
        Ok(())
    }

    fn push_fragment(
        &mut self,
        fu_header: u8,
        nal_header: &[u8],
        body: &[u8],
    ) -> Result<(), DepacketizeError> {
        let (start, end) = (fu_header & 0x80 != 0, fu_header & 0x40 != 0);
        if start {
            if self.fragment.is_some() {
                self.fragment = None;
                return Err(DepacketizeError::OrphanFragment);
            }
            let mut nalu = nal_header.to_vec();
            nalu.extend_from_slice(body);
            self.fragment = Some(nalu);
        } else {
            self.fragment
                .as_mut()
                .ok_or(DepacketizeError::OrphanFragment)?
                .extend_from_slice(body);
        }
        if end && let Some(nalu) = self.fragment.take() {
            self.nalus.push(nalu);
        }
        Ok(())
    }
}

pub fn annexb(nalus: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nalus.iter().map(|n| n.len() + 4).sum());
    for nalu in nalus {
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nalu);
    }
    out
}

/// Split Annex B into NAL units; 3- and 4-byte start codes are both accepted.
pub fn nal_units(stream: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= stream.len() {
        if stream[i..i + 3] == [0, 0, 1] {
            starts.push((i, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &(_, body)) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map_or(stream.len(), |&(sc, _)| sc);
        if k + 1 < starts.len() && end > body && stream[end - 1] == 0 {
            end -= 1;
        }
        if end > body {
            out.push(&stream[body..end]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_single_stap_and_fu_a() {
        let mut d = Depacketizer::new(VideoCodec::H264);
        d.push(&[0x09, 0xF0]).unwrap();
        d.push(&[0x18, 0x00, 0x02, 0x67, 0x42, 0x00, 0x01, 0x68])
            .unwrap();
        d.push(&[0x7C, 0x85, 0xAA, 0xBB]).unwrap();
        d.push(&[0x7C, 0x05, 0xCC]).unwrap();
        d.push(&[0x7C, 0x45, 0xDD]).unwrap();
        assert_eq!(
            d.take_frame(),
            annexb(&[
                &[0x09, 0xF0],
                &[0x67, 0x42],
                &[0x68],
                &[0x65, 0xAA, 0xBB, 0xCC, 0xDD]
            ])
        );
    }

    #[test]
    fn h265_fu_restores_nal_header() {
        let mut d = Depacketizer::new(VideoCodec::H265);
        d.push(&[0x62, 0x01, 0x93, 0x01]).unwrap();
        d.push(&[0x62, 0x01, 0x53, 0x02]).unwrap();
        assert_eq!(d.take_frame(), annexb(&[&[0x26, 0x01, 0x01, 0x02]]));
    }

    #[test]
    fn orphan_fragment_is_an_error() {
        let mut d = Depacketizer::new(VideoCodec::H264);
        assert_eq!(
            d.push(&[0x7C, 0x05, 0xCC]),
            Err(DepacketizeError::OrphanFragment)
        );
    }

    #[test]
    fn splits_mixed_start_codes() {
        let stream = [0, 0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x68, 0, 0, 0, 1, 0x65];
        assert_eq!(
            nal_units(&stream),
            vec![&[0x67, 0x42][..], &[0x68][..], &[0x65][..]]
        );
    }
}
