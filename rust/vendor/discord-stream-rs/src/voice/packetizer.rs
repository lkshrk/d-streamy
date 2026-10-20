//! RTP packetization equivalent to the libdatachannel packetizers of the JS reference.

use crate::processing::split_nalu;
use crate::utils::VideoCodec;
use bytes::Bytes;
use std::time::Duration;
use webrtc::rtp::header::{EXTENSION_PROFILE_ONE_BYTE, Extension, Header};
use webrtc::rtp::packet::Packet;

/// Fits RTP header, extension and SRTP tag into a 1280-byte IPv6 minimum MTU.
pub const MAX_PAYLOAD_SIZE: usize = 1200;

/// RTP header extension id for `playout-delay` used by the JS reference.
pub const PLAYOUT_DELAY_EXT_ID: u8 = 5;

const H264_FU_A: u8 = 28;
const H265_FU: u8 = 49;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadFormat {
    Opus,
    Video(VideoCodec),
}

/// Per-SSRC RTP state: sequence number, timestamp and payload format.
#[derive(Debug, Clone)]
pub struct RtpPacketizer {
    ssrc: u32,
    payload_type: u8,
    clock_rate: u32,
    format: PayloadFormat,
    sequence_number: u16,
    start_timestamp: u32,
    timestamp: u32,
    playout_delay: (u16, u16),
}

impl RtpPacketizer {
    pub fn new(
        ssrc: u32,
        payload_type: u8,
        clock_rate: u32,
        format: PayloadFormat,
        playout_delay: (u16, u16),
    ) -> Self {
        let start_timestamp = rand::random();
        Self {
            ssrc,
            payload_type,
            clock_rate,
            format,
            sequence_number: rand::random(),
            start_timestamp,
            timestamp: start_timestamp,
            playout_delay,
        }
    }

    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }

    pub fn payload_type(&self) -> u8 {
        self.payload_type
    }

    pub fn clock_rate(&self) -> u32 {
        self.clock_rate
    }

    pub fn format(&self) -> PayloadFormat {
        self.format
    }

    pub fn timestamp(&self) -> u32 {
        self.timestamp
    }

    pub fn sequence_number(&self) -> u16 {
        self.sequence_number
    }

    /// `rtpConfig.timestamp += round(frametime * clockRate / 1000)`.
    pub fn advance(&mut self, frametime_ms: f64) {
        let ticks = (frametime_ms * f64::from(self.clock_rate) / 1000.0).round() as i64;
        self.timestamp = self.timestamp.wrapping_add(ticks as u32);
    }

    /// Set the RTP timestamp from a presentation time relative to the first frame.
    pub fn set_pts(&mut self, pts: Duration) {
        let ticks = (pts.as_secs_f64() * f64::from(self.clock_rate)).round() as u64;
        self.timestamp = self.start_timestamp.wrapping_add(ticks as u32);
    }

    /// Split one frame into RTP packets at the current timestamp.
    pub fn packetize(&mut self, frame: &[u8]) -> Vec<Packet> {
        let payloads = match self.format {
            PayloadFormat::Opus => vec![Bytes::copy_from_slice(frame)],
            PayloadFormat::Video(VideoCodec::H264) => annexb_payloads(frame, 1, fu_a),
            PayloadFormat::Video(VideoCodec::H265) => annexb_payloads(frame, 2, fu_h265),
            PayloadFormat::Video(_) => fixed_size_payloads(frame),
        };
        let last = payloads.len().saturating_sub(1);
        payloads
            .into_iter()
            .enumerate()
            .map(|(i, payload)| self.packet(payload, i == last))
            .collect()
    }

    fn packet(&mut self, payload: Bytes, marker: bool) -> Packet {
        let header = Header {
            version: 2,
            marker,
            payload_type: self.payload_type,
            sequence_number: self.sequence_number,
            timestamp: self.timestamp,
            ssrc: self.ssrc,
            extension: true,
            extension_profile: EXTENSION_PROFILE_ONE_BYTE,
            extensions: vec![Extension {
                id: PLAYOUT_DELAY_EXT_ID,
                payload: playout_delay_payload(self.playout_delay.0, self.playout_delay.1),
            }],
            ..Default::default()
        };
        self.sequence_number = self.sequence_number.wrapping_add(1);
        Packet { header, payload }
    }
}

/// `playout-delay` extension body: 12-bit min and max delay in 10 ms units.
pub fn playout_delay_payload(min: u16, max: u16) -> Bytes {
    let (min, max) = (min.min(0xFFF), max.min(0xFFF));
    Bytes::from(vec![
        (min >> 4) as u8,
        (((min & 0xF) << 4) | (max >> 8)) as u8,
        (max & 0xFF) as u8,
    ])
}

fn annexb_payloads(
    frame: &[u8],
    header_len: usize,
    fragment: fn(&[u8], &mut Vec<Bytes>),
) -> Vec<Bytes> {
    let mut out = Vec::new();
    for nalu in split_nalu(frame) {
        if nalu.len() <= MAX_PAYLOAD_SIZE || nalu.len() <= header_len {
            out.push(Bytes::copy_from_slice(nalu));
        } else {
            fragment(nalu, &mut out);
        }
    }
    out
}

fn fu_a(nalu: &[u8], out: &mut Vec<Bytes>) {
    let indicator = (nalu[0] & 0xE0) | H264_FU_A;
    let nal_type = nalu[0] & 0x1F;
    fragment_with(&nalu[1..], &[indicator], nal_type, out);
}

fn fu_h265(nalu: &[u8], out: &mut Vec<Bytes>) {
    let payload_header = [(nalu[0] & 0x81) | (H265_FU << 1), nalu[1]];
    let nal_type = (nalu[0] >> 1) & 0x3F;
    fragment_with(&nalu[2..], &payload_header, nal_type, out);
}

fn fragment_with(body: &[u8], prefix: &[u8], nal_type: u8, out: &mut Vec<Bytes>) {
    let chunk = MAX_PAYLOAD_SIZE - prefix.len() - 1;
    let count = body.len().div_ceil(chunk);
    for (i, part) in body.chunks(chunk).enumerate() {
        let mut fu_header = nal_type;
        if i == 0 {
            fu_header |= 0x80;
        }
        if i + 1 == count {
            fu_header |= 0x40;
        }
        let mut p = Vec::with_capacity(prefix.len() + 1 + part.len());
        p.extend_from_slice(prefix);
        p.push(fu_header);
        p.extend_from_slice(part);
        out.push(Bytes::from(p));
    }
}

fn fixed_size_payloads(frame: &[u8]) -> Vec<Bytes> {
    frame
        .chunks(MAX_PAYLOAD_SIZE)
        .map(Bytes::copy_from_slice)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h264_packetizer() -> RtpPacketizer {
        RtpPacketizer::new(
            2222,
            101,
            90_000,
            PayloadFormat::Video(VideoCodec::H264),
            (0, 10),
        )
    }

    fn depacketize_h264(packets: &[Packet]) -> Vec<u8> {
        let mut out = Vec::new();
        for p in packets {
            let b = &p.payload;
            if b[0] & 0x1F == H264_FU_A {
                if b[1] & 0x80 != 0 {
                    out.extend_from_slice(&[0, 0, 0, 1, (b[0] & 0xE0) | (b[1] & 0x1F)]);
                }
                out.extend_from_slice(&b[2..]);
            } else {
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(b);
            }
        }
        out
    }

    fn depacketize_h265(packets: &[Packet]) -> Vec<u8> {
        let mut out = Vec::new();
        for p in packets {
            let b = &p.payload;
            if (b[0] >> 1) & 0x3F == H265_FU {
                if b[2] & 0x80 != 0 {
                    out.extend_from_slice(&[
                        0,
                        0,
                        0,
                        1,
                        (b[0] & 0x81) | ((b[2] & 0x3F) << 1),
                        b[1],
                    ]);
                }
                out.extend_from_slice(&b[3..]);
            } else {
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(b);
            }
        }
        out
    }

    fn nal(header: &[u8], len: usize) -> Vec<u8> {
        let mut v = header.to_vec();
        v.extend((0..len).map(|i| (i % 251) as u8 | 0x10));
        v
    }

    fn annexb(nalus: &[Vec<u8>]) -> Vec<u8> {
        nalus
            .iter()
            .flat_map(|n| [0, 0, 0, 1].into_iter().chain(n.iter().copied()))
            .collect()
    }

    #[test]
    fn h264_keeps_every_nal_and_fragments_large_ones() {
        let frame = annexb(&[
            nal(&[0x09], 1),
            nal(&[0x67], 10),
            nal(&[0x68], 4),
            nal(&[0x0C], 20),
            nal(&[0x65], 5000),
        ]);
        let mut p = h264_packetizer();
        let packets = p.packetize(&frame);
        assert_eq!(depacketize_h264(&packets), frame);
        assert!(packets.iter().all(|p| p.payload.len() <= MAX_PAYLOAD_SIZE));
        assert!(packets.len() > 5);
        let markers: Vec<bool> = packets.iter().map(|p| p.header.marker).collect();
        assert_eq!(markers.iter().filter(|m| **m).count(), 1);
        assert!(markers.last().unwrap());
    }

    #[test]
    fn h265_fragments_with_fu_payload_header() {
        let frame = annexb(&[nal(&[0x40, 0x01], 30), nal(&[0x26, 0x01], 4000)]);
        let mut p = RtpPacketizer::new(
            2222,
            103,
            90_000,
            PayloadFormat::Video(VideoCodec::H265),
            (0, 10),
        );
        let packets = p.packetize(&frame);
        assert_eq!(depacketize_h265(&packets), frame);
        assert_eq!(packets.iter().filter(|p| p.header.marker).count(), 1);
        assert!(packets.iter().all(|p| p.payload.len() <= MAX_PAYLOAD_SIZE));
    }

    #[test]
    fn header_fields_sequence_and_extension() {
        let mut p = h264_packetizer();
        let seq0 = p.sequence_number();
        let ts0 = p.timestamp();
        let packets = p.packetize(&annexb(&[nal(&[0x65], 3000)]));
        for (i, pkt) in packets.iter().enumerate() {
            assert_eq!(pkt.header.ssrc, 2222);
            assert_eq!(pkt.header.payload_type, 101);
            assert_eq!(pkt.header.timestamp, ts0);
            assert_eq!(pkt.header.sequence_number, seq0.wrapping_add(i as u16));
            assert_eq!(
                pkt.header.get_extension(PLAYOUT_DELAY_EXT_ID).unwrap()[..],
                [0, 0, 10]
            );
        }
    }

    #[test]
    fn timestamps_follow_frametime_or_pts() {
        let mut p = RtpPacketizer::new(1, 120, 48_000, PayloadFormat::Opus, (0, 1));
        let start = p.timestamp();
        p.advance(20.0);
        assert_eq!(p.timestamp(), start.wrapping_add(960));
        p.set_pts(Duration::from_millis(1000));
        assert_eq!(p.timestamp(), start.wrapping_add(48_000));
        let pkts = p.packetize(&[0xFC, 0xFF, 0xFE]);
        assert_eq!(pkts.len(), 1);
        assert!(pkts[0].header.marker);
        assert_eq!(pkts[0].payload[..], [0xFC, 0xFF, 0xFE]);
    }

    #[test]
    fn playout_delay_encoding() {
        assert_eq!(playout_delay_payload(0, 1)[..], [0, 0, 1]);
        assert_eq!(playout_delay_payload(0x123, 0x456)[..], [0x12, 0x34, 0x56]);
    }
}
