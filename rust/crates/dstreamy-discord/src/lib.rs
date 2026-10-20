pub use discord_stream_rs as transport;

mod client;
pub mod gateway;
pub mod stream;

pub use client::DiscordClient;
pub use gateway::{
    CurrentUser, DEFAULT_GATEWAY_URL, Dispatch, GatewayClient, GatewayError, GatewayEvent,
    GatewayOptions, Guild, IdentifyProperties, VoiceChannel, VoiceChannelKind, VoiceState,
};
pub use stream::{StreamError, StreamHandle, StreamOptions, StreamStats, start_stream};
