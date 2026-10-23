//! Producer frames per ADR 0004: `u32 BE len | u8 kind | i64 BE pts_us | u8 flags | payload`.

use std::io;

use tokio_util::bytes::{Buf, BufMut, Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

pub const MAX_FRAME_LEN: u32 = 8 << 20;
pub const HEADER_LEN: u32 = 1 + 8 + 1;
pub const FLAG_KEYFRAME: u8 = 1;

const LEN_PREFIX: usize = 4;
/// Most a header may make the decoder reserve before the payload bytes arrive.
pub const MAX_RESERVE: usize = 64 << 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Kind {
    /// One H.264 access unit in Annex B form.
    Video = 1,
    Opus = 2,
    EndOfStream = 3,
}

impl TryFrom<u8> for Kind {
    type Error = ProducerError;

    fn try_from(value: u8) -> Result<Self, ProducerError> {
        match value {
            1 => Ok(Self::Video),
            2 => Ok(Self::Opus),
            3 => Ok(Self::EndOfStream),
            other => Err(ProducerError::UnknownKind(other)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: Kind,
    pub pts_us: i64,
    pub keyframe: bool,
    pub payload: Bytes,
}

impl Frame {
    pub fn end_of_stream(pts_us: i64) -> Self {
        Self {
            kind: Kind::EndOfStream,
            pts_us,
            keyframe: false,
            payload: Bytes::new(),
        }
    }

    pub fn encoded_len(&self) -> usize {
        LEN_PREFIX + HEADER_LEN as usize + self.payload.len()
    }
}

/// Every variant except `Io` is a protocol violation; the stream cannot be resynchronised.
#[derive(Debug, thiserror::Error)]
pub enum ProducerError {
    #[error("frame length {0} exceeds the {MAX_FRAME_LEN}-byte limit")]
    Oversized(u64),
    #[error("frame length {0} is shorter than the {HEADER_LEN}-byte header")]
    TooShort(u32),
    #[error("unknown frame kind {0}")]
    UnknownKind(u8),
    #[error("reserved flag bits set: {0:#04x}")]
    ReservedFlags(u8),
    #[error("end-of-stream frame carries {0} payload bytes")]
    EndOfStreamPayload(usize),
    #[error("stream ended inside a frame")]
    Truncated,
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ProducerCodec;

impl ProducerCodec {
    pub fn new() -> Self {
        Self
    }
}

fn check(kind: Kind, flags: u8, payload_len: usize) -> Result<(), ProducerError> {
    if flags & !FLAG_KEYFRAME != 0 {
        return Err(ProducerError::ReservedFlags(flags & !FLAG_KEYFRAME));
    }
    if kind == Kind::EndOfStream && payload_len != 0 {
        return Err(ProducerError::EndOfStreamPayload(payload_len));
    }
    Ok(())
}

impl Decoder for ProducerCodec {
    type Item = Frame;
    type Error = ProducerError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Frame>, ProducerError> {
        let Some(len) = src.get(..LEN_PREFIX) else {
            return Ok(None);
        };
        let len = u32::from_be_bytes(len.try_into().expect("four bytes"));
        if len > MAX_FRAME_LEN {
            return Err(ProducerError::Oversized(len.into()));
        }
        if len < HEADER_LEN {
            return Err(ProducerError::TooShort(len));
        }
        if let Some(&kind) = src.get(LEN_PREFIX) {
            Kind::try_from(kind)?;
        }
        let total = LEN_PREFIX + len as usize;
        if src.len() < total {
            src.reserve((total - src.len()).min(MAX_RESERVE));
            return Ok(None);
        }
        src.advance(LEN_PREFIX);
        let kind = Kind::try_from(src.get_u8())?;
        let pts_us = src.get_i64();
        let flags = src.get_u8();
        let payload_len = (len - HEADER_LEN) as usize;
        check(kind, flags, payload_len)?;
        let payload = src.split_to(payload_len).freeze();
        Ok(Some(Frame {
            kind,
            pts_us,
            keyframe: kind == Kind::Video && flags & FLAG_KEYFRAME != 0,
            payload,
        }))
    }

    fn decode_eof(&mut self, src: &mut BytesMut) -> Result<Option<Frame>, ProducerError> {
        match self.decode(src)? {
            Some(frame) => Ok(Some(frame)),
            None if src.is_empty() => Ok(None),
            None => Err(ProducerError::Truncated),
        }
    }
}

impl Encoder<Frame> for ProducerCodec {
    type Error = ProducerError;

    fn encode(&mut self, frame: Frame, dst: &mut BytesMut) -> Result<(), ProducerError> {
        self.encode(&frame, dst)
    }
}

impl Encoder<&Frame> for ProducerCodec {
    type Error = ProducerError;

    fn encode(&mut self, frame: &Frame, dst: &mut BytesMut) -> Result<(), ProducerError> {
        let len = HEADER_LEN as u64 + frame.payload.len() as u64;
        if len > MAX_FRAME_LEN.into() {
            return Err(ProducerError::Oversized(len));
        }
        let flags = if frame.keyframe && frame.kind == Kind::Video {
            FLAG_KEYFRAME
        } else {
            0
        };
        check(frame.kind, flags, frame.payload.len())?;
        dst.reserve(frame.encoded_len());
        dst.put_u32(len as u32);
        dst.put_u8(frame.kind as u8);
        dst.put_i64(frame.pts_us);
        dst.put_u8(flags);
        dst.put_slice(&frame.payload);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(len: u32, kind: u8, pts: i64, flags: u8) -> BytesMut {
        let mut buf = BytesMut::new();
        buf.put_u32(len);
        buf.put_u8(kind);
        buf.put_i64(pts);
        buf.put_u8(flags);
        buf
    }

    #[test]
    fn oversized_length_is_refused_before_buffering() {
        let mut buf = BytesMut::new();
        buf.put_u32(MAX_FRAME_LEN + 1);
        let err = ProducerCodec.decode(&mut buf).unwrap_err();
        assert!(matches!(err, ProducerError::Oversized(n) if n == u64::from(MAX_FRAME_LEN) + 1));
    }

    #[test]
    fn largest_frame_is_accepted() {
        let mut buf = header(MAX_FRAME_LEN, 1, 0, 0);
        buf.resize(LEN_PREFIX + MAX_FRAME_LEN as usize, 0);
        let frame = ProducerCodec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(frame.payload.len(), (MAX_FRAME_LEN - HEADER_LEN) as usize);
    }

    #[test]
    fn unknown_kind_fails_before_the_payload_arrives() {
        let mut buf = BytesMut::new();
        buf.put_u32(1000);
        buf.put_u8(9);
        assert!(matches!(
            ProducerCodec.decode(&mut buf),
            Err(ProducerError::UnknownKind(9))
        ));
    }

    #[test]
    fn short_length_is_refused() {
        let mut buf = BytesMut::new();
        buf.put_u32(HEADER_LEN - 1);
        assert!(matches!(
            ProducerCodec.decode(&mut buf),
            Err(ProducerError::TooShort(9))
        ));
    }

    #[test]
    fn reserved_flags_are_refused() {
        let mut buf = header(HEADER_LEN, 1, 0, 0b10);
        assert!(matches!(
            ProducerCodec.decode(&mut buf),
            Err(ProducerError::ReservedFlags(2))
        ));
    }

    // OBS ffmpeg_opus marks every packet a keyframe (obs-ffmpeg-audio-encoders.c:394 @ 32.2.2).
    #[test]
    fn keyframe_flag_is_ignored_on_non_video() {
        let mut buf = header(HEADER_LEN + 3, 2, 960, FLAG_KEYFRAME);
        buf.put_slice(&[0xF8, 0xFF, 0xFE]);
        let frame = ProducerCodec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(frame.kind, Kind::Opus);
        assert!(!frame.keyframe);
        let mut buf = header(HEADER_LEN, 3, 0, FLAG_KEYFRAME);
        assert!(!ProducerCodec.decode(&mut buf).unwrap().unwrap().keyframe);
    }

    #[test]
    fn a_header_alone_reserves_at_most_the_cap() {
        let mut buf = BytesMut::new();
        buf.put_u32(MAX_FRAME_LEN);
        buf.put_u8(1);
        assert!(ProducerCodec.decode(&mut buf).unwrap().is_none());
        assert!(
            buf.capacity() <= LEN_PREFIX + 1 + MAX_RESERVE,
            "{}",
            buf.capacity()
        );
    }

    #[test]
    fn end_of_stream_has_no_payload() {
        let mut buf = header(HEADER_LEN + 1, 3, 0, 0);
        buf.put_u8(0);
        assert!(matches!(
            ProducerCodec.decode(&mut buf),
            Err(ProducerError::EndOfStreamPayload(1))
        ));
    }

    #[test]
    fn eof_inside_a_frame_is_truncated() {
        let mut buf = header(HEADER_LEN + 4, 1, 0, 0);
        buf.put_slice(&[0, 0]);
        assert!(ProducerCodec.decode(&mut buf).unwrap().is_none());
        assert!(matches!(
            ProducerCodec.decode_eof(&mut buf),
            Err(ProducerError::Truncated)
        ));
    }

    #[test]
    fn negative_pts_round_trips() {
        let frame = Frame {
            kind: Kind::Opus,
            pts_us: -6_500,
            keyframe: false,
            payload: Bytes::from_static(&[0xF8, 0xFF, 0xFE]),
        };
        let mut buf = BytesMut::new();
        ProducerCodec.encode(&frame, &mut buf).unwrap();
        assert_eq!(buf.len(), frame.encoded_len());
        assert_eq!(ProducerCodec.decode(&mut buf).unwrap(), Some(frame));
        assert!(buf.is_empty());
    }
}
