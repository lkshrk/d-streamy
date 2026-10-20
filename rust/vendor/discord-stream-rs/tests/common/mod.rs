#![allow(dead_code)]

use dstreamy_mock_discord::{VideoCodec, rtc};
use std::sync::Arc;
use tokio::sync::mpsc;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp::packet::Packet;

pub use dstreamy_mock_discord::depacketize::annexb;
pub use dstreamy_mock_discord::rtc::loopback_settings;

pub async fn fake_discord_peer() -> (Arc<RTCPeerConnection>, mpsc::UnboundedReceiver<Packet>) {
    rtc::answerer_peer(VideoCodec::H264).await.unwrap()
}

pub async fn answer_like_discord(server: &RTCPeerConnection, offer: String) -> String {
    rtc::answer_like_discord(server, offer).await.unwrap()
}
