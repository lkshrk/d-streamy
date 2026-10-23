//! NDJSON framing for `hello`, the control role and the producer's reverse channel.

use std::io;

use serde::Serialize;
use serde_json::Value;
use tokio_util::bytes::{BufMut, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

/// Longest line accepted, without its `\n` (ADR 0004).
pub const MAX_LINE: usize = 1 << 20;

/// A line that is not one JSON value. The connection stays usable: the codec has already
/// moved past the line.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Malformed {
    #[error("line longer than {MAX_LINE} bytes")]
    Oversized,
    #[error("line is not valid UTF-8")]
    NotUtf8,
    #[error("line is not JSON: {0}")]
    NotJson(String),
}

#[derive(Debug, Default)]
pub struct ControlCodec {
    scanned: usize,
    discarding: bool,
}

impl ControlCodec {
    pub fn new() -> Self {
        Self::default()
    }
}

fn parse(line: &[u8]) -> Result<Value, Malformed> {
    let text = std::str::from_utf8(line).map_err(|_| Malformed::NotUtf8)?;
    serde_json::from_str(text).map_err(|e| Malformed::NotJson(e.to_string()))
}

impl Decoder for ControlCodec {
    type Item = Result<Value, Malformed>;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> io::Result<Option<Self::Item>> {
        loop {
            let limit = src.len().min(MAX_LINE + 1);
            let newline = src[self.scanned.min(limit)..limit]
                .iter()
                .position(|&b| b == b'\n')
                .map(|i| i + self.scanned);
            match (self.discarding, newline) {
                (true, Some(end)) => {
                    let _ = src.split_to(end + 1);
                    self.discarding = false;
                    self.scanned = 0;
                }
                (true, None) => {
                    let _ = src.split_to(limit);
                    self.scanned = 0;
                    if src.is_empty() {
                        return Ok(None);
                    }
                }
                (false, Some(end)) => {
                    let line = src.split_to(end + 1);
                    self.scanned = 0;
                    return Ok(Some(parse(&line[..end])));
                }
                (false, None) if src.len() > MAX_LINE => {
                    self.discarding = true;
                    self.scanned = 0;
                    return Ok(Some(Err(Malformed::Oversized)));
                }
                (false, None) => {
                    self.scanned = src.len();
                    return Ok(None);
                }
            }
        }
    }

    fn decode_eof(&mut self, src: &mut BytesMut) -> io::Result<Option<Self::Item>> {
        if let Some(item) = self.decode(src)? {
            return Ok(Some(item));
        }
        self.scanned = 0;
        if self.discarding {
            self.discarding = false;
            src.clear();
            return Ok(None);
        }
        if src.is_empty() {
            return Ok(None);
        }
        let line = src.split();
        Ok(Some(parse(&line)))
    }
}

impl<T: Serialize> Encoder<T> for ControlCodec {
    type Error = io::Error;

    fn encode(&mut self, item: T, dst: &mut BytesMut) -> io::Result<()> {
        let start = dst.len();
        serde_json::to_writer((&mut *dst).writer(), &item)?;
        if dst.len() - start > MAX_LINE {
            dst.truncate(start);
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                Malformed::Oversized,
            ));
        }
        dst.put_u8(b'\n');
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn drain(codec: &mut ControlCodec, buf: &mut BytesMut) -> Vec<Result<Value, Malformed>> {
        let mut out = Vec::new();
        while let Some(item) = codec.decode(buf).unwrap() {
            out.push(item);
        }
        out
    }

    #[test]
    fn line_at_the_cap_is_accepted() {
        let mut line = Vec::from(&b"\""[..]);
        line.resize(MAX_LINE - 1, b'a');
        line.extend_from_slice(b"\"\n");
        let mut buf = BytesMut::from(&line[..]);
        let items = drain(&mut ControlCodec::new(), &mut buf);
        assert_eq!(items.len(), 1);
        assert!(items[0].is_ok());
    }

    #[test]
    fn oversized_line_reports_once_then_resyncs_at_newline() {
        let mut codec = ControlCodec::new();
        let mut buf = BytesMut::from(&vec![b'x'; MAX_LINE + 1][..]);
        assert_eq!(drain(&mut codec, &mut buf), [Err(Malformed::Oversized)]);
        buf.extend_from_slice(&vec![b'y'; 3 * MAX_LINE]);
        assert!(drain(&mut codec, &mut buf).is_empty());
        buf.extend_from_slice(b"yyy\n{\"id\":\"2\"}\n");
        assert_eq!(drain(&mut codec, &mut buf), [Ok(json!({"id": "2"}))]);
        assert!(buf.is_empty());
    }

    #[test]
    fn bytes_split_across_reads_form_one_line() {
        let mut codec = ControlCodec::new();
        let mut buf = BytesMut::new();
        for chunk in [&b"{\"id\""[..], b":", b"\"1\"}", b"\n"] {
            buf.extend_from_slice(chunk);
            if chunk != b"\n" {
                assert!(drain(&mut codec, &mut buf).is_empty());
            }
        }
        assert_eq!(drain(&mut codec, &mut buf), [Ok(json!({"id": "1"}))]);
    }

    #[test]
    fn final_line_without_newline_is_decoded_at_eof() {
        let mut codec = ControlCodec::new();
        let mut buf = BytesMut::from(&b"{\"id\":\"1\"}"[..]);
        assert_eq!(
            codec.decode_eof(&mut buf).unwrap(),
            Some(Ok(json!({"id": "1"})))
        );
        assert_eq!(codec.decode_eof(&mut buf).unwrap(), None);
    }

    #[test]
    fn encoding_appends_one_newline() {
        let mut buf = BytesMut::new();
        ControlCodec::new()
            .encode(json!({"type": "keyframe_request"}), &mut buf)
            .unwrap();
        assert_eq!(&buf[..], b"{\"type\":\"keyframe_request\"}\n");
    }

    #[test]
    fn encoding_refuses_an_oversized_line() {
        let mut buf = BytesMut::from(&b"kept"[..]);
        let err = ControlCodec::new()
            .encode("a".repeat(MAX_LINE), &mut buf)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(&buf[..], b"kept");
    }
}
