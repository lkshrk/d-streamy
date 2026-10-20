//! # discord-stream-rs
//!
//! A Rust library for streaming audio/video to Discord voice channels,
//! ported from [`@dank074/discord-video-stream`](https://github.com/dank074/discord-video-stream).
//!
//! ## Implemented so far
//! - Voice gateway OpCodes (JSON + binary)          — `VoiceOpCodes.ts`
//! - Voice gateway message types (serde)             — `VoiceMessageTypes.ts`
//! - DAVE / E2EE session                             — `@snazzah/davey`
//! - `VoiceConnection` state machine + DAVE handshake — `BaseMediaConnection.ts`
//! - `StreamConnection` (Go Live)                    — `StreamConnection.ts`
//! - Gateway events + opcodes                        — `GatewayEvents.ts` / `GatewayOpCodes.ts`
//! - `Streamer` controller                           — `Streamer.ts`
//! - Stream key helpers + codec utils                — `utils.ts`

pub mod dave;
pub mod gateway;
#[cfg(feature = "media")]
pub mod media;
pub mod processing;
pub mod utils;
pub mod voice;

pub use dave::{DaveError, DaveHandler};
pub use gateway::{
    GatewayEvent, GatewayOpCode, GatewayPayload, PendingConnection, Streamer, StreamerError,
};
pub use processing::{
    H264Helpers, H264NalUnitType, H265Helpers, H265NalUnitType, rewrite_sps_vui, split_nalu,
};
pub use utils::{
    StreamKey, StreamType, VideoCodec, generate_stream_key, normalize_video_codec, parse_stream_key,
};
pub use voice::{
    ConnectionError, ConnectionKind, MediaConnection, VideoAttributes, VoiceConnection, VoiceEvent,
    WebRtcError, WebRtcParams, WebRtcWrapper,
};
