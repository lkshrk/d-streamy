/// Codec payload type constants — ported from `CodecPayloadType.ts`.
/// These are the fixed RTP payload type numbers Discord uses.

#[derive(Debug, Clone, Copy)]
pub struct AudioCodecInfo {
    pub name: &'static str,
    pub clock_rate: u32,
    pub payload_type: u8,
    /// libdatachannel's default `a=fmtp` for the codec.
    pub fmtp: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct VideoCodecInfo {
    pub name: &'static str,
    pub clock_rate: u32,
    pub payload_type: u8,
    pub rtx_payload_type: u8,
    pub priority: u16,
    /// libdatachannel's default `a=fmtp` for the codec; empty when it has none.
    pub fmtp: &'static str,
}

pub const OPUS: AudioCodecInfo = AudioCodecInfo {
    name: "opus",
    clock_rate: 48_000,
    payload_type: 120,
    fmtp: "minptime=10;maxaveragebitrate=96000;stereo=1;sprop-stereo=1;useinbandfec=1",
};

pub const H264: VideoCodecInfo = VideoCodecInfo {
    name: "H264",
    clock_rate: 90_000,
    priority: 1000,
    payload_type: 101,
    rtx_payload_type: 102,
    fmtp: "profile-level-id=42e01f;packetization-mode=1;level-asymmetry-allowed=1",
};

pub const H265: VideoCodecInfo = VideoCodecInfo {
    name: "H265",
    clock_rate: 90_000,
    priority: 1000,
    payload_type: 103,
    rtx_payload_type: 104,
    fmtp: "",
};

pub const VP8: VideoCodecInfo = VideoCodecInfo {
    name: "VP8",
    clock_rate: 90_000,
    priority: 1000,
    payload_type: 105,
    rtx_payload_type: 106,
    fmtp: "",
};

pub const VP9: VideoCodecInfo = VideoCodecInfo {
    name: "VP9",
    clock_rate: 90_000,
    priority: 1000,
    payload_type: 107,
    rtx_payload_type: 108,
    fmtp: "",
};

pub const AV1: VideoCodecInfo = VideoCodecInfo {
    name: "AV1",
    clock_rate: 90_000,
    priority: 1000,
    payload_type: 109,
    rtx_payload_type: 110,
    fmtp: "",
};

pub const ALL_VIDEO_CODECS: &[VideoCodecInfo] = &[H264, H265, VP8, VP9, AV1];
