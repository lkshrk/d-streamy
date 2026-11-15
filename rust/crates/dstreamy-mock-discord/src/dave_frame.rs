//! Reads a DAVE frame's truncated nonce, which its footer carries in the clear.

use crate::dave::DAVE_MAGIC;

const TAG_BYTES: usize = 8;

fn leb128(bytes: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (i, byte) in bytes.iter().enumerate().take(10) {
        value |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// The nonce as davey's `InboundFrameProcessor::parse_frame` reads it (`cryptor/frame_processors.rs:182-229`).
pub fn nonce(frame: &[u8]) -> Option<u32> {
    if !frame.ends_with(&DAVE_MAGIC) {
        return None;
    }
    let footer = frame.len().checked_sub(DAVE_MAGIC.len() + 1)?;
    let size = usize::from(frame[footer]);
    if size < TAG_BYTES + 1 + DAVE_MAGIC.len() || size > frame.len() {
        return None;
    }
    let supplemental = &frame[frame.len() - size..footer];
    let (nonce, _) = leb128(supplemental.get(TAG_BYTES..)?)?;
    u32::try_from(nonce).ok()
}
