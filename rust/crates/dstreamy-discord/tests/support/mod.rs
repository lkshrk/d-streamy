#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use dstreamy_discord::transport::Streamer;
use dstreamy_discord::{DiscordClient, GatewayOptions, SessionState};
use dstreamy_mock_discord::{
    MOCK_TOKEN, MockDiscord, Recorded, RecordedVideoFrame, VoiceKind, rtc,
};
use secrecy::SecretString;
use tokio::sync::watch;
use tokio::time::timeout;

pub const T: Duration = Duration::from_secs(20);

/// A client on the mock's gateway with loopback-only WebRTC.
pub async fn client_with(
    mock: &MockDiscord,
    gateway: impl FnOnce(&mut GatewayOptions),
    configure: impl FnOnce(&mut Streamer),
) -> Arc<DiscordClient> {
    let mut opts = GatewayOptions::new(SecretString::from(MOCK_TOKEN)).with_url(mock.gateway_url());
    gateway(&mut opts);
    let client = timeout(T, DiscordClient::connect(opts))
        .await
        .unwrap()
        .unwrap();
    {
        let mut streamer = client.streamer();
        streamer.set_ice_servers(vec![]);
        streamer.set_setting_engine(rtc::loopback_settings(false));
        configure(&mut streamer);
    }
    Arc::new(client)
}

pub fn fast_reconnects(opts: &mut GatewayOptions) {
    opts.reconnect_delay_min = Duration::from_millis(10);
    opts.reconnect_delay_max = Duration::from_millis(50);
    opts.invalid_session_delay = Duration::from_millis(50);
}

pub async fn wait_state(
    state: &mut watch::Receiver<SessionState>,
    f: impl Fn(&SessionState) -> bool,
) {
    let waited = timeout(T, state.wait_for(|s| f(s)))
        .await
        .map(|r| r.is_ok());
    assert_eq!(waited, Ok(true), "state stuck at {:?}", *state.borrow());
}

pub fn stream_video(r: &Recorded) -> Vec<&RecordedVideoFrame> {
    r.video
        .iter()
        .filter(|v| v.kind == VoiceKind::Stream)
        .collect()
}
