//! Opens a DAVE frame statelessly with a sender's media key, independent of davey's per-user nonce state.

use davey::aead::consts::{U8, U12};
use davey::aes::Aes128;
use davey::{AeadInPlace, AesGcm, KeyInit};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::dave::DAVE_MAGIC;

const TAG_BYTES: usize = 8;
const KEY_BYTES: usize = 16;
const NONCE_BYTES: usize = 12;
const GENERATION_SHIFT_BITS: u32 = 24;

/// The label davey exports each member's media key under (`src/session.rs:31,782-797`).
pub const MEDIA_KEY_LABEL: &str = "Discord Secure Frames v0";

/// A DAVE frame split as davey's `InboundFrameProcessor::parse_frame` does (`cryptor/frame_processors.rs:182-275`).
struct Sealed<'a> {
    nonce: u32,
    tag: &'a [u8],
    authenticated: Vec<u8>,
    ciphertext: Vec<u8>,
}

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

fn parse(frame: &[u8]) -> Option<Sealed<'_>> {
    if !frame.ends_with(&DAVE_MAGIC) {
        return None;
    }
    let footer = frame.len().checked_sub(DAVE_MAGIC.len() + 1)?;
    let size = usize::from(frame[footer]);
    if size < TAG_BYTES + 1 + DAVE_MAGIC.len() || size > frame.len() {
        return None;
    }
    let body_len = frame.len() - size;
    let supplemental = &frame[body_len..footer];
    let tag = &supplemental[..TAG_BYTES];
    let (nonce, mut read) = leb128(&supplemental[TAG_BYTES..])?;
    read += TAG_BYTES;
    let mut ranges = Vec::new();
    while read < supplemental.len() {
        let (offset, n) = leb128(&supplemental[read..])?;
        read += n;
        let (len, n) = leb128(supplemental.get(read..)?)?;
        read += n;
        ranges.push((usize::try_from(offset).ok()?, usize::try_from(len).ok()?));
    }
    let mut authenticated = Vec::new();
    let mut ciphertext = Vec::new();
    let mut at = 0;
    for (offset, len) in ranges {
        let end = offset
            .checked_add(len)
            .filter(|&e| offset >= at && e <= body_len)?;
        ciphertext.extend_from_slice(&frame[at..offset]);
        authenticated.extend_from_slice(&frame[offset..end]);
        at = end;
    }
    ciphertext.extend_from_slice(&frame[at..body_len]);
    Some(Sealed {
        nonce: u32::try_from(nonce).ok()?,
        tag,
        authenticated,
        ciphertext,
    })
}

/// MLS++'s HKDF-Expand, which accepts davey's 16-byte secrets (`cryptor/mlspp_crypto.rs`).
fn expand_with_label(secret: &[u8], label: &str, generation: u32, length: usize) -> Vec<u8> {
    let label = format!("MLS 1.0 {label}");
    let mut info = Vec::new();
    info.extend_from_slice(&u16::try_from(length).unwrap_or(u16::MAX).to_be_bytes());
    for part in [label.as_bytes(), &generation.to_be_bytes()[..]] {
        info.push(u8::try_from(part.len()).unwrap_or(u8::MAX));
        info.extend_from_slice(part);
    }
    let mut okm = Vec::new();
    let mut block = Vec::new();
    for i in 1u8.. {
        if okm.len() >= length {
            break;
        }
        let mut mac =
            <Hmac<Sha256> as Mac>::new_from_slice(secret).expect("HMAC takes any key length");
        mac.update(&block);
        mac.update(&info);
        mac.update(&[i]);
        block = mac.finalize().into_bytes().to_vec();
        okm.extend_from_slice(&block);
    }
    okm.truncate(length);
    okm
}

/// The AES key davey's `HashRatchet` derives for `generation` (`cryptor/hash_ratchet.rs:24-80`).
fn ratchet_key(media_key: &[u8], generation: u32) -> Vec<u8> {
    let mut secret = media_key.to_vec();
    for g in 0..generation {
        secret = expand_with_label(&secret, "secret", g, 32);
    }
    expand_with_label(&secret, "key", generation, KEY_BYTES)
}

/// The frame's truncated nonce, when it authenticates under `media_key`.
pub fn open(media_key: &[u8], frame: &[u8]) -> Option<u32> {
    let mut sealed = parse(frame)?;
    let key = ratchet_key(media_key, sealed.nonce >> GENERATION_SHIFT_BITS);
    let cipher = AesGcm::<Aes128, U12, U8>::new_from_slice(&key).ok()?;
    let mut nonce = [0u8; NONCE_BYTES];
    nonce[NONCE_BYTES - 4..].copy_from_slice(&sealed.nonce.to_le_bytes());
    cipher
        .decrypt_in_place_detached(
            (&nonce).into(),
            &sealed.authenticated,
            &mut sealed.ciphertext,
            sealed.tag.into(),
        )
        .ok()
        .map(|()| sealed.nonce)
}
