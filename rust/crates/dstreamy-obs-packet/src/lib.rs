//! OBS `encoded_packet` to producer frames for the plugin and `mock-producer`; dependency-free so the `cdylib` links nothing else.

use std::borrow::Cow;
use std::fmt;

const NAL_SLICE: u8 = 1;
const NAL_IDR: u8 = 5;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const NAL_AUD: u8 = 9;
/// Far above any SPS plus PPS (H.264 §7.3.2.1, §7.3.2.2); bounds what a converter retains.
const MAX_EXTRA_DATA: usize = 64 << 10;
const START_CODE: [u8; 4] = [0, 0, 0, 1];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketError {
    AvccExtraData,
    ExtraDataIncomplete,
    ExtraDataTooLarge,
    NotAnnexB,
    NoParameterSets,
    ParameterSetMismatch,
    BFrames,
    AudioPtsNeDts,
    BadTimebase,
    PtsOverflow,
}

impl fmt::Display for PacketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::AvccExtraData => "the video encoder's extra data is avcC, not Annex B",
            Self::ExtraDataIncomplete => "the video encoder's extra data lacks an SPS or a PPS",
            Self::ExtraDataTooLarge => "the video encoder's extra data is larger than 64 KiB",
            Self::NotAnnexB => "video packet is not Annex B: it does not start with a start code",
            Self::NoParameterSets => {
                "keyframe has no SPS/PPS and the encoder has no extra data yet"
            }
            Self::ParameterSetMismatch => {
                "keyframe carries an SPS or PPS that differs from the encoder's extra data"
            }
            Self::BFrames => "B-frames are enabled: video pts differs from dts",
            Self::AudioPtsNeDts => "audio pts differs from dts",
            Self::BadTimebase => "packet timebase is not positive",
            Self::PtsOverflow => "packet pts does not fit in microseconds",
        })
    }
}

impl std::error::Error for PacketError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame<'a> {
    /// Borrows the packet when nothing was inserted.
    pub data: Cow<'a, [u8]>,
    pub pts_us: i64,
    pub keyframe: bool,
}

/// One NAL unit: `code_at` includes a leading `zero_byte`, `payload` excludes trailing zeros.
#[derive(Debug, Clone, Copy)]
struct Nal {
    code_at: usize,
    payload: (usize, usize),
}

impl Nal {
    fn bytes<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        &data[self.payload.0..self.payload.1]
    }

    fn kind(&self, data: &[u8]) -> Option<u8> {
        self.bytes(data).first().map(|b| b & 0x1f)
    }
}

fn start_code_at(data: &[u8], i: usize) -> bool {
    data.get(i..i + 3) == Some(&[0, 0, 1])
}

/// H.264 §B.1.1: any number of leading zero bytes, then `00 00 01`.
fn is_annex_b(data: &[u8]) -> bool {
    let zeros = data.iter().take_while(|&&b| b == 0).count();
    zeros >= 2 && data.get(zeros) == Some(&1)
}

/// Stops at the first VCL NAL unit, whose payload then runs to the end: only the parameter-set head is ever needed.
fn nal_units(data: &[u8]) -> Vec<Nal> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if start_code_at(data, i) {
            starts.push(i);
            if is_vcl(data.get(i + 3).map(|b| b & 0x1f)) {
                break;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::with_capacity(starts.len());
    for (k, &at) in starts.iter().enumerate() {
        let next = starts.get(k + 1).copied().unwrap_or(data.len());
        let mut end = next;
        while end > at + 3 && data[end - 1] == 0 {
            end -= 1;
        }
        let code_at = if at > 0 && data[at - 1] == 0 && (k == 0 || at > starts[k - 1] + 3) {
            at - 1
        } else {
            at
        };
        nals.push(Nal {
            code_at,
            payload: (at + 3, end),
        });
    }
    nals
}

fn is_vcl(kind: Option<u8>) -> bool {
    matches!(kind, Some(NAL_SLICE..=NAL_IDR))
}

/// Converts one video track's packets; holds the SPS and PPS of the encoder's extra data.
#[derive(Debug, Default, Clone)]
pub struct Converter {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl Converter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes the SPS and PPS from Annex B extra data; callable at any time, and a refused update keeps the previous sets.
    pub fn set_video_extra_data(&mut self, extra: &[u8]) -> Result<(), PacketError> {
        if extra.len() > MAX_EXTRA_DATA {
            return Err(PacketError::ExtraDataTooLarge);
        }
        if !is_annex_b(extra) {
            return Err(PacketError::AvccExtraData);
        }
        let (mut sps, mut pps) = (None, None);
        for nal in nal_units(extra) {
            match nal.kind(extra) {
                Some(NAL_SPS) => sps = sps.or_else(|| Some(nal.bytes(extra).to_vec())),
                Some(NAL_PPS) => pps = pps.or_else(|| Some(nal.bytes(extra).to_vec())),
                _ => {}
            }
        }
        match (sps, pps) {
            (Some(sps), Some(pps)) => {
                self.sps = Some(sps);
                self.pps = Some(pps);
                Ok(())
            }
            _ => Err(PacketError::ExtraDataIncomplete),
        }
    }

    pub fn has_video_extra_data(&self) -> bool {
        self.sps.is_some() && self.pps.is_some()
    }

    pub fn convert_video<'a>(
        &self,
        data: &'a [u8],
        keyframe: bool,
        pts: i64,
        dts: i64,
        timebase: (i32, i32),
    ) -> Result<VideoFrame<'a>, PacketError> {
        if pts != dts {
            return Err(PacketError::BFrames);
        }
        if !is_annex_b(data) {
            return Err(PacketError::NotAnnexB);
        }
        let pts_us = pts_to_us(pts, timebase)?;
        let data = if keyframe {
            self.with_parameter_sets(data)?
        } else {
            Cow::Borrowed(data)
        };
        Ok(VideoFrame {
            data,
            pts_us,
            keyframe,
        })
    }

    /// H.264 §7.4.1.2.3: SPS before PPS, both before the first VCL NAL unit and after a leading AUD.
    fn with_parameter_sets<'a>(&self, data: &'a [u8]) -> Result<Cow<'a, [u8]>, PacketError> {
        let nals = nal_units(data);
        let first_vcl = nals
            .iter()
            .position(|n| is_vcl(n.kind(data)))
            .unwrap_or(nals.len());
        let head = &nals[..first_vcl];
        let sps: Vec<&Nal> = head
            .iter()
            .filter(|n| n.kind(data) == Some(NAL_SPS))
            .collect();
        let pps: Vec<&Nal> = head
            .iter()
            .filter(|n| n.kind(data) == Some(NAL_PPS))
            .collect();
        let boundary = |i: usize| nals.get(i).map_or(data.len(), |n| n.code_at);
        let (insert_at, insert): (usize, Vec<&[u8]>) = match (sps.last(), pps.first()) {
            (Some(_), Some(_)) => return Ok(Cow::Borrowed(data)),
            (None, None) => {
                let (Some(s), Some(p)) = (&self.sps, &self.pps) else {
                    return Err(PacketError::NoParameterSets);
                };
                let after_aud =
                    usize::from(head.first().and_then(|n| n.kind(data)) == Some(NAL_AUD));
                (boundary(after_aud), vec![s, p])
            }
            (Some(last_sps), None) => {
                let (Some(s), Some(p)) = (&self.sps, &self.pps) else {
                    return Err(PacketError::NoParameterSets);
                };
                if sps.iter().any(|n| n.bytes(data) != s.as_slice()) {
                    return Err(PacketError::ParameterSetMismatch);
                }
                let i = nals
                    .iter()
                    .position(|n| n.code_at == last_sps.code_at)
                    .expect("last_sps is one of nals");
                (boundary(i + 1), vec![p])
            }
            (None, Some(first_pps)) => {
                let (Some(s), Some(p)) = (&self.sps, &self.pps) else {
                    return Err(PacketError::NoParameterSets);
                };
                if pps.iter().any(|n| n.bytes(data) != p.as_slice()) {
                    return Err(PacketError::ParameterSetMismatch);
                }
                (first_pps.code_at, vec![s])
            }
        };
        let extra: usize = insert.iter().map(|n| START_CODE.len() + n.len()).sum();
        let mut out = Vec::with_capacity(data.len() + extra);
        out.extend_from_slice(&data[..insert_at]);
        for nal in insert {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(nal);
        }
        out.extend_from_slice(&data[insert_at..]);
        Ok(Cow::Owned(out))
    }
}

/// OBS pts to µs, truncated toward zero: libobs counts pts in 1/`timebase_den` and uses `timebase_num` only as a frame's
/// duration (obs-encoder.c:1473-1474,1637; obs-internal.h:55-58), so the numerator is checked but not multiplied in.
pub fn pts_to_us(pts: i64, (num, den): (i32, i32)) -> Result<i64, PacketError> {
    if num <= 0 || den <= 0 {
        return Err(PacketError::BadTimebase);
    }
    let den = i64::from(den);
    // Split so no product overflows; quotient and remainder share the sign of pts, so truncation composes.
    let (q, r) = (pts / den, pts % den);
    q.checked_mul(1_000_000)
        .and_then(|head| head.checked_add(r * 1_000_000 / den))
        .ok_or(PacketError::PtsOverflow)
}

pub fn convert_audio(pts: i64, dts: i64, timebase: (i32, i32)) -> Result<i64, PacketError> {
    if pts != dts {
        return Err(PacketError::AudioPtsNeDts);
    }
    pts_to_us(pts, timebase)
}

#[cfg(test)]
mod tests;
