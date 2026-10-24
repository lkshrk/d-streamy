//! Safe wrapper over `obs/src/packet.c`, the OBS packet conversion the plugin uses (ADR 0004, OBS plugin).

use std::ffi::CStr;
use std::os::raw::c_char;
use std::ptr::NonNull;

#[repr(C)]
struct RawConverter {
    _private: [u8; 0],
}

#[repr(C)]
struct RawFrame {
    data: *const u8,
    size: usize,
    pts_us: i64,
    keyframe: bool,
}

unsafe extern "C" {
    fn ds_packet_converter_create() -> *mut RawConverter;
    fn ds_packet_converter_destroy(conv: *mut RawConverter);
    fn ds_packet_set_video_extra_data(conv: *mut RawConverter, data: *const u8, size: usize)
    -> i32;
    fn ds_packet_has_video_extra_data(conv: *const RawConverter) -> bool;
    #[allow(clippy::too_many_arguments)]
    fn ds_packet_convert_video(
        conv: *mut RawConverter,
        data: *const u8,
        size: usize,
        keyframe: bool,
        pts: i64,
        dts: i64,
        timebase_num: i32,
        timebase_den: i32,
        out: *mut RawFrame,
    ) -> i32;
    fn ds_packet_convert_audio(
        pts: i64,
        dts: i64,
        timebase_num: i32,
        timebase_den: i32,
        pts_us: *mut i64,
    ) -> i32;
    fn ds_packet_pts_to_us(pts: i64, timebase_num: i32, timebase_den: i32, pts_us: *mut i64)
    -> i32;
    fn ds_packet_status_message(status: i32) -> *const c_char;
}

/// A refusal from `packet.c`; the numbers are `enum ds_packet_status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketError {
    AvccExtraData,
    NotAnnexB,
    NoParameterSets,
    ExtraDataIncomplete,
    BFrames,
    AudioPtsNeDts,
    BadTimebase,
    PtsOverflow,
    NoMemory,
    Unknown(i32),
}

impl PacketError {
    fn status(self) -> i32 {
        match self {
            Self::AvccExtraData => 1,
            Self::NotAnnexB => 2,
            Self::NoParameterSets => 3,
            Self::ExtraDataIncomplete => 4,
            Self::BFrames => 5,
            Self::AudioPtsNeDts => 6,
            Self::BadTimebase => 7,
            Self::PtsOverflow => 8,
            Self::NoMemory => 9,
            Self::Unknown(s) => s,
        }
    }

    fn check(status: i32) -> Result<(), Self> {
        Err(match status {
            0 => return Ok(()),
            1 => Self::AvccExtraData,
            2 => Self::NotAnnexB,
            3 => Self::NoParameterSets,
            4 => Self::ExtraDataIncomplete,
            5 => Self::BFrames,
            6 => Self::AudioPtsNeDts,
            7 => Self::BadTimebase,
            8 => Self::PtsOverflow,
            9 => Self::NoMemory,
            s => Self::Unknown(s),
        })
    }
}

impl std::fmt::Display for PacketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // SAFETY: returns a pointer to a static NUL-terminated string for any status.
        let message = unsafe { CStr::from_ptr(ds_packet_status_message(self.status())) };
        f.write_str(&message.to_string_lossy())
    }
}

impl std::error::Error for PacketError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoFrame<'a> {
    pub data: &'a [u8],
    pub pts_us: i64,
    pub keyframe: bool,
}

pub struct Converter {
    raw: NonNull<RawConverter>,
}

// The converter is plain heap memory owned exclusively by this value.
unsafe impl Send for Converter {}

impl Converter {
    pub fn new() -> Self {
        // SAFETY: no arguments; aborts below on allocation failure like any Rust allocation.
        let raw = unsafe { ds_packet_converter_create() };
        Self {
            raw: NonNull::new(raw).expect("ds_packet_converter_create: out of memory"),
        }
    }

    pub fn set_video_extra_data(&mut self, extra: &[u8]) -> Result<(), PacketError> {
        // SAFETY: `raw` is live and `extra` is valid for `extra.len()` bytes.
        PacketError::check(unsafe {
            ds_packet_set_video_extra_data(self.raw.as_ptr(), extra.as_ptr(), extra.len())
        })
    }

    pub fn has_video_extra_data(&self) -> bool {
        // SAFETY: `raw` is live.
        unsafe { ds_packet_has_video_extra_data(self.raw.as_ptr()) }
    }

    /// The returned data borrows `data` or this converter, so it lives until the next call.
    pub fn convert_video<'a>(
        &'a mut self,
        data: &'a [u8],
        keyframe: bool,
        pts: i64,
        dts: i64,
        timebase: (i32, i32),
    ) -> Result<VideoFrame<'a>, PacketError> {
        let mut out = RawFrame {
            data: std::ptr::null(),
            size: 0,
            pts_us: 0,
            keyframe: false,
        };
        // SAFETY: `raw` is live, `data` is valid for its length and `out` is writable.
        PacketError::check(unsafe {
            ds_packet_convert_video(
                self.raw.as_ptr(),
                data.as_ptr(),
                data.len(),
                keyframe,
                pts,
                dts,
                timebase.0,
                timebase.1,
                &mut out,
            )
        })?;
        // SAFETY: on success `out.data` points at `out.size` bytes inside `data` or the converter's buffer, both borrowed for 'a.
        let data = unsafe { std::slice::from_raw_parts(out.data, out.size) };
        Ok(VideoFrame {
            data,
            pts_us: out.pts_us,
            keyframe: out.keyframe,
        })
    }
}

impl Default for Converter {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Converter {
    fn drop(&mut self) {
        // SAFETY: created by `ds_packet_converter_create` and destroyed once.
        unsafe { ds_packet_converter_destroy(self.raw.as_ptr()) }
    }
}

pub fn convert_audio(pts: i64, dts: i64, timebase: (i32, i32)) -> Result<i64, PacketError> {
    let mut us = 0;
    // SAFETY: `us` is writable.
    PacketError::check(unsafe {
        ds_packet_convert_audio(pts, dts, timebase.0, timebase.1, &mut us)
    })?;
    Ok(us)
}

pub fn pts_to_us(pts: i64, timebase: (i32, i32)) -> Result<i64, PacketError> {
    let mut us = 0;
    // SAFETY: `us` is writable.
    PacketError::check(unsafe { ds_packet_pts_to_us(pts, timebase.0, timebase.1, &mut us) })?;
    Ok(us)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: [u8; 10] = [0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1f, 0xda, 0x01];
    const PPS: [u8; 8] = [0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80];
    const IDR: [u8; 8] = [0, 0, 0, 1, 0x65, 0x88, 0x84, 0x21];

    #[test]
    fn keyframes_get_the_extra_data_once_it_arrives() {
        let mut conv = Converter::new();
        assert_eq!(
            conv.convert_video(&IDR, true, 0, 0, (1, 30)),
            Err(PacketError::NoParameterSets)
        );
        conv.set_video_extra_data(&[&SPS[..], &PPS].concat())
            .unwrap();
        assert!(conv.has_video_extra_data());
        let frame = conv.convert_video(&IDR, true, 1, 1, (1, 30)).unwrap();
        assert_eq!(frame.data, [&SPS[..], &PPS, &IDR].concat());
        assert_eq!(frame.pts_us, 33_333);
        let with_sps = [&SPS[..], &IDR].concat();
        let frame = conv.convert_video(&with_sps, true, 2, 2, (1, 30)).unwrap();
        assert_eq!(frame.data, [&PPS[..], &SPS, &IDR].concat());
    }

    #[test]
    fn refusals_carry_the_c_message() {
        let mut conv = Converter::new();
        let e = conv.convert_video(&IDR, true, 0, -2, (1, 30)).unwrap_err();
        assert_eq!(e, PacketError::BFrames);
        assert!(e.to_string().starts_with("B-frames are enabled"), "{e}");
        assert_eq!(
            conv.set_video_extra_data(&[1, 0x42, 0xc0, 0x1f, 0xff, 0xe1]),
            Err(PacketError::AvccExtraData)
        );
        assert_eq!(convert_audio(-312, -312, (1, 48_000)), Ok(-6_500));
        assert_eq!(pts_to_us(1, (0, 30)), Err(PacketError::BadTimebase));
    }
}
