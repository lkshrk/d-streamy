use std::time::Duration;

use dstreamy_discord::{DiscordClient, GatewayEvent, GatewayOptions};
use dstreamy_mock_discord::depacketize::annexb;
use dstreamy_mock_discord::opcodes::gateway as gw;
use dstreamy_mock_discord::{
    MOCK_GUILD_ID, MOCK_TOKEN, MOCK_USER_ID, MOCK_VOICE_CHANNEL_ID, MockDiscord, VoiceKind, rtc,
};
use secrecy::SecretString;
use tokio::time::timeout;

const T: Duration = Duration::from_secs(15);

#[tokio::test(flavor = "multi_thread")]
async fn real_client_over_zlib_streams_and_resumes() {
    let mock = MockDiscord::start().await.unwrap();
    let mut opts = GatewayOptions::new(SecretString::from(MOCK_TOKEN)).with_url(mock.gateway_url());
    assert!(opts.compress);
    opts.reconnect_delay_min = Duration::from_millis(10);
    opts.reconnect_delay_max = Duration::from_millis(50);
    let client = timeout(T, DiscordClient::connect(opts))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(client.user().id, MOCK_USER_ID);
    assert_eq!(mock.gateway_zlib(), Some(true));
    assert!(
        client
            .voice_channels(MOCK_GUILD_ID)
            .iter()
            .any(|c| c.id == MOCK_VOICE_CHANNEL_ID)
    );
    {
        let mut streamer = client.streamer();
        streamer.set_ice_servers(vec![]);
        streamer.set_setting_engine(rtc::loopback_settings(false));
    }

    let mut voice = client.join_voice(Some(MOCK_GUILD_ID.into()), MOCK_VOICE_CHANNEL_ID.into());
    timeout(T, voice.ready()).await.unwrap().unwrap();
    let mut stream = client.create_stream().unwrap();
    let conn = timeout(T, stream.ready()).await.unwrap().unwrap();
    timeout(T, async {
        while !(conn.is_ready().await && conn.dave_ready()) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    conn.set_packetizer("H264").await.unwrap();
    let au = annexb(&[&[0x09, 0xF0], &[0x65, 0x88, 0x84, 0x21, 0x10]]);
    for i in 0..5u64 {
        conn.send_video_frame_at(&au, Duration::from_millis(33 * i))
            .await
            .unwrap();
    }
    let frames = mock
        .wait_for(T, |r| (r.video.len() >= 5).then(|| r.video.clone()))
        .await
        .unwrap();
    assert!(
        frames
            .iter()
            .all(|f| f.kind == VoiceKind::Stream && f.e2ee && f.data == au)
    );

    let mut events = client.subscribe();
    let session = mock.session_id().unwrap();
    assert!(mock.drop_gateway(Some(gw::CLOSE_UNKNOWN_ERROR)));
    let resume = mock
        .wait_for_op(None, gw::OP_RESUME, 0, T)
        .await
        .expect("client resumed");
    assert_eq!(resume.json().unwrap()["session_id"], session.as_str());
    timeout(T, async {
        loop {
            if let Ok(GatewayEvent::Resumed) = events.recv().await {
                return;
            }
        }
    })
    .await
    .expect("Resumed event");

    client.stop_stream();
    mock.wait_for_op(None, gw::OP_STREAM_DELETE, 0, T)
        .await
        .unwrap();
    client.close().await;
    mock.shutdown().await;
}
