//! Annex B byte-stream helpers for H.264 and H.265.

pub const START_CODE: [u8; 4] = [0, 0, 0, 1];

pub const H264_NAL_IDR: u8 = 5;
pub const H264_NAL_SPS: u8 = 7;
pub const H264_NAL_PPS: u8 = 8;

/// Appends a start code and `nalu` to `out`.
pub fn append_nalu(out: &mut Vec<u8>, nalu: &[u8]) {
    out.extend_from_slice(&START_CODE);
    out.extend_from_slice(nalu);
}

/// Appends AVCC (4-byte length-prefixed) NAL units to `out` as Annex B, up to the first zero or truncated length.
pub fn append_avcc_nalus(avcc: &[u8], out: &mut Vec<u8>) {
    let mut rest = avcc;
    while let Some((len, tail)) = rest.split_first_chunk::<4>() {
        let n = u32::from_be_bytes(*len) as usize;
        if n == 0 || n > tail.len() {
            break;
        }
        let (nalu, next) = tail.split_at(n);
        append_nalu(out, nalu);
        rest = next;
    }
}

/// Iterates the NAL units of an Annex B stream (3- or 4-byte start codes), without start codes.
pub fn nal_units(stream: &[u8]) -> impl Iterator<Item = &[u8]> {
    let starts = start_code_positions(stream);
    let ends: Vec<usize> = starts
        .iter()
        .skip(1)
        .map(|&(code_at, _)| code_at)
        .chain(std::iter::once(stream.len()))
        .collect();
    starts
        .into_iter()
        .zip(ends)
        .map(move |((_, payload_at), end)| trim_trailing_zeros(&stream[payload_at..end]))
        .filter(|nalu| !nalu.is_empty())
}

fn start_code_positions(stream: &[u8]) -> Vec<(usize, usize)> {
    let mut positions = Vec::new();
    let mut i = 0;
    while i + 3 <= stream.len() {
        if stream[i] == 0 && stream[i + 1] == 0 && stream[i + 2] == 1 {
            positions.push((i, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    positions
}

// Zero bytes before the next start code belong to it (4-byte form) or are trailing_zero_8bits.
fn trim_trailing_zeros(nalu: &[u8]) -> &[u8] {
    let end = nalu.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    &nalu[..end]
}

pub fn h264_nal_type(nalu: &[u8]) -> Option<u8> {
    nalu.first().map(|b| b & 0x1f)
}

pub fn h265_nal_type(nalu: &[u8]) -> Option<u8> {
    nalu.first().map(|b| (b >> 1) & 0x3f)
}

/// True if the H.264 access unit contains an IDR slice.
pub fn h264_is_keyframe(access_unit: &[u8]) -> bool {
    nal_units(access_unit).any(|n| h264_nal_type(n) == Some(H264_NAL_IDR))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(avcc: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        append_avcc_nalus(avcc, &mut out);
        out
    }

    #[test]
    fn single_nalu() {
        assert_eq!(
            convert(&[0, 0, 0, 3, 0x41, 0x42, 0x43]),
            [0, 0, 0, 1, 0x41, 0x42, 0x43]
        );
    }

    #[test]
    fn multiple_nalus() {
        assert_eq!(
            convert(&[0, 0, 0, 3, 0x41, 0x42, 0x43, 0, 0, 0, 2, 0x44, 0x45]),
            [0, 0, 0, 1, 0x41, 0x42, 0x43, 0, 0, 0, 1, 0x44, 0x45]
        );
    }

    #[test]
    fn truncated_length_stops() {
        assert_eq!(convert(&[0, 0, 0, 5, 0x41, 0x42]), [] as [u8; 0]);
    }

    #[test]
    fn partial_second_nalu_keeps_first() {
        assert_eq!(
            convert(&[0, 0, 0, 2, 0x41, 0x42, 0, 0, 0, 9, 0x44]),
            [0, 0, 0, 1, 0x41, 0x42]
        );
    }

    #[test]
    fn empty() {
        assert_eq!(convert(&[]), [] as [u8; 0]);
    }

    #[test]
    fn zero_length_nalu_stops() {
        assert_eq!(convert(&[0, 0, 0, 0, 0x41]), [] as [u8; 0]);
    }

    #[test]
    fn appends_to_existing_output() {
        let mut out = vec![0xaa];
        append_avcc_nalus(&[0, 0, 0, 1, 0x41], &mut out);
        assert_eq!(out, [0xaa, 0, 0, 0, 1, 0x41]);
    }

    #[test]
    fn splits_mixed_start_codes() {
        let stream = [
            0, 0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0x88,
        ];
        let nalus: Vec<&[u8]> = nal_units(&stream).collect();
        assert_eq!(nalus, [&[0x67, 0x42][..], &[0x68, 0xce], &[0x65, 0x88]]);
        let types: Vec<u8> = nalus.iter().filter_map(|n| h264_nal_type(n)).collect();
        assert_eq!(types, [H264_NAL_SPS, H264_NAL_PPS, H264_NAL_IDR]);
        assert!(h264_is_keyframe(&stream));
    }

    #[test]
    fn round_trips_avcc() {
        let avcc = [0, 0, 0, 2, 0x41, 0x9a, 0, 0, 0, 3, 0x01, 0x02, 0x03];
        let annexb = convert(&avcc);
        let nalus: Vec<&[u8]> = nal_units(&annexb).collect();
        assert_eq!(nalus, [&[0x41, 0x9a][..], &[0x01, 0x02, 0x03]]);
        assert!(!h264_is_keyframe(&annexb));
    }

    #[test]
    fn no_start_code_yields_nothing() {
        assert_eq!(nal_units(&[0x65, 0x88, 0x84]).count(), 0);
    }

    #[test]
    fn h265_types() {
        assert_eq!(h265_nal_type(&[0x40, 0x01]), Some(32));
        assert_eq!(h265_nal_type(&[0x26, 0x01]), Some(19));
        assert_eq!(h265_nal_type(&[]), None);
    }
}
