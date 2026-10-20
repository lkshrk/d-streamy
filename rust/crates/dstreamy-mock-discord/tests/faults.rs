mod common;

use common::{Client, Frame, GatewayDriver, T};
use discord_stream_rs::{GatewayEvent, VoiceEvent};
use dstreamy_mock_discord::opcodes::{gateway as gw, voice as vo};
use dstreamy_mock_discord::{
    MOCK_GUILD_ID, MOCK_OTHER_VOICE_CHANNEL_ID, MOCK_TOKEN, MOCK_VOICE_CHANNEL_ID, MockDiscord,
    VoiceKind,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::time::Duration;
use tokio::time::timeout;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

async fn join(driver: &mut GatewayDriver, channel: &str) {
    driver
        .send(
            gw::OP_VOICE_STATE_UPDATE,
            json!({ "guild_id": MOCK_GUILD_ID, "channel_id": channel, "self_mute": false, "self_deaf": true, "self_video": false }),
        )
        .await;
    driver.dispatch("VOICE_SERVER_UPDATE").await;
}

#[tokio::test]
async fn identify_with_wrong_token_closes_4004() {
    let mock = MockDiscord::start().await.unwrap();
    let mut driver = GatewayDriver::connect(&mock.gateway_url()).await;
    assert_eq!(driver.hello["d"]["heartbeat_interval"], 41_250);
    driver
        .send(gw::OP_IDENTIFY, json!({ "token": "wrong" }))
        .await;
    assert_eq!(
        driver.next().await,
        Frame::Closed(Some(gw::CLOSE_AUTHENTICATION_FAILED))
    );
}

#[tokio::test]
async fn ops_before_identify_close_4003_and_heartbeats_are_acked() {
    let mock = MockDiscord::start().await.unwrap();
    let mut driver = GatewayDriver::connect(&mock.gateway_url()).await;
    driver.send(gw::OP_HEARTBEAT, json!(null)).await;
    assert_eq!(driver.next().await, Frame::Op(11, json!(null)));
    driver.send(gw::OP_STREAM_CREATE, json!({})).await;
    assert_eq!(
        driver.next().await,
        Frame::Closed(Some(gw::CLOSE_NOT_AUTHENTICATED))
    );
}

#[tokio::test]
async fn dropped_gateway_resumes_and_replays_missed_dispatches() {
    let mock = MockDiscord::start().await.unwrap();
    let url = mock.gateway_url();
    let (mut driver, ready) = GatewayDriver::identify(&url, MOCK_TOKEN).await;
    assert_eq!(ready["resume_gateway_url"], url.as_str());
    join(&mut driver, MOCK_VOICE_CHANNEL_ID).await;

    assert!(mock.drop_gateway(Some(gw::CLOSE_UNKNOWN_ERROR)));
    assert_eq!(
        driver.next().await,
        Frame::Closed(Some(gw::CLOSE_UNKNOWN_ERROR))
    );
    assert!(mock.move_to_channel(MOCK_GUILD_ID, MOCK_OTHER_VOICE_CHANNEL_ID));

    let (session_id, seq) = (driver.session_id.clone(), driver.seq);
    let mut resumed = GatewayDriver::connect(&url).await;
    resumed.session_id = session_id;
    resumed.seq = seq;
    resumed.send_resume(MOCK_TOKEN).await;
    let replayed = resumed.dispatch("VOICE_STATE_UPDATE").await;
    assert_eq!(replayed["channel_id"], MOCK_OTHER_VOICE_CHANNEL_ID);
    resumed.dispatch("RESUMED").await;
    assert_eq!(resumed.seq, seq + 2);
    assert!(mock.wait_for_op(None, gw::OP_RESUME, 0, T).await.is_some());
}

#[tokio::test]
async fn invalid_session_requires_identify() {
    let mock = MockDiscord::start().await.unwrap();
    let url = mock.gateway_url();
    let (mut driver, ready) = GatewayDriver::identify(&url, MOCK_TOKEN).await;
    assert!(mock.invalidate_session(false));
    assert_eq!(driver.next().await, Frame::Op(9, json!(false)));
    assert!(mock.session_id().is_none());

    driver.send_resume(MOCK_TOKEN).await;
    assert_eq!(driver.next().await, Frame::Op(9, json!(false)));
    let again = driver.send_identify(MOCK_TOKEN).await;
    assert_ne!(again["session_id"], ready["session_id"]);
    assert_eq!(mock.session_id().as_deref(), again["session_id"].as_str());

    assert!(mock.invalidate_session(true));
    assert_eq!(driver.next().await, Frame::Op(9, json!(true)));
    assert!(mock.request_reconnect());
    assert_eq!(driver.next().await, Frame::Op(7, json!(null)));
}

#[tokio::test]
async fn voice_identify_with_unknown_token_closes_4004() {
    let mock = MockDiscord::start().await.unwrap();
    let (mut ws, _) = connect_async(format!("{}/?v=8", mock.voice_endpoint()))
        .await
        .unwrap();
    let hello = ws.next().await.unwrap().unwrap();
    assert!(hello.to_text().unwrap().contains("\"op\":8"));
    let identify = json!({ "op": 0, "d": { "server_id": MOCK_GUILD_ID, "user_id": "1", "session_id": "x", "token": "nope" } });
    ws.send(Message::Text(identify.to_string())).await.unwrap();
    match timeout(T, ws.next()).await.unwrap() {
        Some(Ok(Message::Close(Some(f)))) => {
            assert_eq!(u16::from(f.code), vo::CLOSE_AUTHENTICATION_FAILED)
        }
        other => panic!("expected close, got {other:?}"),
    }
}

async fn drop_voice_and_resume(close_code: Option<u16>) {
    let mock = MockDiscord::start().await.unwrap();
    let mut client = Client::connect(&mock).await;
    let mut voice = client
        .streamer
        .join_voice(Some(MOCK_GUILD_ID.into()), MOCK_VOICE_CHANNEL_ID.into());
    let conn = client.ready(&mut voice).await;
    conn.set_packetizer("H264").await.unwrap();

    assert!(mock.drop_voice(VoiceKind::Voice, close_code));
    let resume = mock
        .wait_for_op(Some(VoiceKind::Voice), vo::OP_RESUME, 0, T)
        .await
        .expect("voice RESUME");
    assert_eq!(resume.json().unwrap()["server_id"], MOCK_GUILD_ID);
    timeout(T, async {
        while let Some(ev) = voice.events.recv().await {
            if matches!(ev, VoiceEvent::Resumed) {
                return;
            }
        }
        panic!("voice events closed");
    })
    .await
    .expect("Resumed");

    conn.send_audio_frame_at(&[0xFC, 0xFF, 0xFE], Duration::ZERO)
        .await
        .unwrap();
    let audio = mock
        .wait_for(T, |r| r.audio.first().cloned())
        .await
        .expect("audio after resume");
    assert!(audio.e2ee);
    assert_eq!(audio.data, [0xFC, 0xFF, 0xFE]);
    mock.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn voice_ws_drop_without_close_frame_is_resumed() {
    drop_voice_and_resume(None).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn voice_ws_close_4015_is_resumed() {
    drop_voice_and_resume(Some(vo::CLOSE_SERVER_CRASHED)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn channel_move_retargets_the_next_stream() {
    let mock = MockDiscord::start().await.unwrap();
    let mut client = Client::connect(&mock).await;
    let mut voice = client
        .streamer
        .join_voice(Some(MOCK_GUILD_ID.into()), MOCK_VOICE_CHANNEL_ID.into());
    client.ready(&mut voice).await;

    assert!(!mock.move_to_channel(MOCK_GUILD_ID, "999"));
    assert!(mock.move_to_channel(MOCK_GUILD_ID, MOCK_OTHER_VOICE_CHANNEL_ID));
    let moved = timeout(T, async {
        loop {
            match client.events.recv().await {
                Some(GatewayEvent::VoiceStateUpdate(_)) => break,
                Some(_) => {}
                None => panic!("gateway closed"),
            }
        }
    });
    moved.await.unwrap();
    let state = mock.voice_state().unwrap();
    assert_eq!(state.channel_id, MOCK_OTHER_VOICE_CHANNEL_ID);
    assert!(
        mock.received_ops()
            .iter()
            .filter(|o| o.is(None, gw::OP_VOICE_STATE_UPDATE))
            .count()
            == 1
    );

    client.streamer.set_voice_channel(
        Some(MOCK_GUILD_ID.into()),
        MOCK_OTHER_VOICE_CHANNEL_ID.into(),
    );
    let mut stream = client.streamer.create_stream().unwrap();
    client.ready(&mut stream).await;
    let key = &mock.streams()[0].stream_key;
    assert!(key.contains(MOCK_OTHER_VOICE_CHANNEL_ID), "{key}");
    mock.shutdown().await;
}
