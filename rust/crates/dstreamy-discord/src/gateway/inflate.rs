use flate2::{Decompress, FlushDecompress, Status};

const ZLIB_SUFFIX: [u8; 4] = [0x00, 0x00, 0xff, 0xff];
const CHUNK: usize = 64 * 1024;
const MAX_MESSAGE: usize = 64 * 1024 * 1024;

/// One zlib context per connection; a message ends with a sync flush (`00 00 ff ff`).
pub(crate) struct ZlibStream {
    inflate: Decompress,
    buffer: Vec<u8>,
}

impl ZlibStream {
    pub(crate) fn new() -> Self {
        Self {
            inflate: Decompress::new(true),
            buffer: Vec::new(),
        }
    }

    /// Returns the decompressed message once its last frame has arrived.
    pub(crate) fn push(&mut self, frame: &[u8]) -> Result<Option<Vec<u8>>, String> {
        self.buffer.extend_from_slice(frame);
        if !self.buffer.ends_with(&ZLIB_SUFFIX) {
            return Ok(None);
        }
        let input = std::mem::take(&mut self.buffer);
        let mut out = Vec::with_capacity(input.len().saturating_mul(4).clamp(CHUNK, MAX_MESSAGE));
        let mut offset = 0;
        loop {
            if out.len() > MAX_MESSAGE {
                return Err("decompressed message exceeds 64 MiB".into());
            }
            if out.capacity() - out.len() < CHUNK {
                out.reserve(CHUNK);
            }
            let in_before = self.inflate.total_in();
            let out_before = self.inflate.total_out();
            let status = self
                .inflate
                .decompress_vec(&input[offset..], &mut out, FlushDecompress::Sync)
                .map_err(|e| e.to_string())?;
            offset += (self.inflate.total_in() - in_before) as usize;
            let produced = self.inflate.total_out() - out_before;
            let output_full = out.len() == out.capacity();
            if status == Status::StreamEnd
                || (offset >= input.len() && !output_full)
                || (produced == 0 && self.inflate.total_in() == in_before)
            {
                break;
            }
        }
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compress, Compression, FlushCompress};

    fn compress(c: &mut Compress, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len() + 1024);
        let start = c.total_in();
        loop {
            let done = (c.total_in() - start) as usize;
            c.compress_vec(&data[done..], &mut out, FlushCompress::Sync)
                .unwrap();
            if (c.total_in() - start) as usize == data.len() && out.len() < out.capacity() {
                break;
            }
            out.reserve(1024);
        }
        out
    }

    #[test]
    fn decodes_split_messages_on_one_context() {
        let mut c = Compress::new(Compression::default(), true);
        let mut z = ZlibStream::new();
        let first = compress(&mut c, b"{\"op\":10}");
        assert_eq!(z.push(&first).unwrap().unwrap(), b"{\"op\":10}");

        let big: Vec<u8> = (0..300_000u32)
            .flat_map(|i| (i % 251).to_le_bytes())
            .collect();
        let second = compress(&mut c, &big);
        let (a, b) = second.split_at(second.len() / 2);
        assert!(z.push(a).unwrap().is_none());
        assert_eq!(z.push(b).unwrap().unwrap(), big);
    }
}
