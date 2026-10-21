mod common;

use common::{GatewayDriver, T};
use dstreamy_mock_discord::opcodes::{gateway as gw, voice as vo};
use dstreamy_mock_discord::{
    MOCK_GUILD_ID, MOCK_TOKEN, MOCK_USER_ID, MOCK_VOICE_CHANNEL_ID, MockConfig, MockDiscord,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// An identified raw voice socket, its READY payload and the gateway that must stay open.
async fn voice(mock: &MockDiscord) -> (GatewayDriver, Ws, Value) {
    let (mut driver, _) = GatewayDriver::identify(&mock.gateway_url(), MOCK_TOKEN).await;
    driver
        .send(
            gw::OP_VOICE_STATE_UPDATE,
            json!({ "guild_id": MOCK_GUILD_ID, "channel_id": MOCK_VOICE_CHANNEL_ID, "self_mute": false, "self_deaf": false, "self_video": false }),
        )
        .await;
    let server = driver.dispatch("VOICE_SERVER_UPDATE").await;
    let session_id = mock.voice_state().unwrap().session_id;
    let (mut ws, _) = connect_async(format!("{}/?v=8", mock.voice_endpoint()))
        .await
        .unwrap();
    ws.next().await.unwrap().unwrap();
    let identify = json!({ "op": vo::OP_IDENTIFY, "d": {
        "server_id": MOCK_GUILD_ID, "user_id": MOCK_USER_ID, "session_id": session_id,
        "token": server["token"], "max_dave_protocol_version": 0,
    } });
    ws.send(Message::Text(identify.to_string())).await.unwrap();
    let ready = next_op(&mut ws).await;
    assert_eq!(ready["op"], vo::OP_READY);
    (driver, ws, ready["d"].clone())
}

async fn next_op(ws: &mut Ws) -> Value {
    match timeout(T, ws.next()).await.unwrap() {
        Some(Ok(Message::Text(t))) => serde_json::from_str(&t).unwrap(),
        other => panic!("expected a text op, got {other:?}"),
    }
}

async fn send(ws: &mut Ws, op: u8, d: Value) {
    let msg = json!({ "op": op, "d": d });
    ws.send(Message::Text(msg.to_string())).await.unwrap();
}

async fn close_code(ws: &mut Ws) -> Option<u16> {
    loop {
        match timeout(T, ws.next()).await.unwrap() {
            Some(Ok(Message::Close(f))) => return f.map(|f| u16::from(f.code)),
            Some(Ok(Message::Text(_))) => {}
            other => panic!("expected close, got {other:?}"),
        }
    }
}

fn codecs(video: &str) -> Value {
    json!([
        { "name": "opus", "type": "audio", "priority": 1000, "payload_type": 120 },
        { "name": video, "type": "video", "priority": 1000, "payload_type": 105, "rtx_payload_type": 106 },
    ])
}

/// Send `op` then a valid heartbeat: `None` when the heartbeat is acked, else the close code.
async fn verdict(op: u8, d: impl FnOnce(&Value) -> Value) -> Option<u16> {
    let mock = MockDiscord::start().await.unwrap();
    let (_gw, mut ws, ready) = voice(&mock).await;
    send(&mut ws, op, d(&ready)).await;
    send(&mut ws, vo::OP_HEARTBEAT, json!({ "t": 9, "seq_ack": 1 })).await;
    loop {
        match timeout(T, ws.next()).await.unwrap() {
            Some(Ok(Message::Text(t))) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["op"] == vo::OP_HEARTBEAT_ACK {
                    return None;
                }
            }
            Some(Ok(Message::Close(f))) => return f.map(|f| u16::from(f.code)),
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn video(ready: &Value, video: u64, rtx: u64, streams: Value) -> Value {
    json!({ "audio_ssrc": ready["ssrc"], "video_ssrc": video, "rtx_ssrc": rtx, "streams": streams })
}

fn stream(ssrc: u64, rtx: u64) -> Value {
    json!([{ "type": "video", "rid": "100", "ssrc": ssrc, "rtx_ssrc": rtx, "active": true, "quality": 100 }])
}

fn video_ssrc(ready: &Value) -> u64 {
    ready["streams"][0]["ssrc"].as_u64().unwrap()
}

#[tokio::test]
async fn speaking_must_use_the_ready_audio_ssrc() {
    let ok = |r: &Value| json!({ "speaking": 1, "delay": 0, "ssrc": r["ssrc"] });
    assert_eq!(verdict(vo::OP_SPEAKING, ok).await, None);
    let foreign =
        |r: &Value| json!({ "speaking": 1, "delay": 0, "ssrc": r["ssrc"].as_u64().unwrap() + 100 });
    assert_eq!(
        verdict(vo::OP_SPEAKING, foreign).await,
        Some(vo::CLOSE_BAD_REQUEST)
    );
}

#[tokio::test]
async fn video_must_use_the_ready_ssrcs() {
    let off = |r: &Value| video(r, 0, 0, json!([]));
    assert_eq!(verdict(vo::OP_VIDEO, off).await, None);
    let on = |r: &Value| {
        let v = video_ssrc(r);
        video(r, v, v + 1, stream(v, v + 1))
    };
    assert_eq!(verdict(vo::OP_VIDEO, on).await, None);
    let foreign_video = |r: &Value| video(r, 7, video_ssrc(r) + 1, json!([]));
    let foreign_rtx = |r: &Value| video(r, video_ssrc(r), 8, json!([]));
    let foreign_stream = |r: &Value| {
        let v = video_ssrc(r);
        video(r, v, v + 1, stream(7, v + 1))
    };
    let foreign_stream_rtx = |r: &Value| {
        let v = video_ssrc(r);
        video(r, v, v + 1, stream(v, 8))
    };
    for bad in [
        foreign_video,
        foreign_rtx,
        foreign_stream,
        foreign_stream_rtx,
    ] {
        assert_eq!(
            verdict(vo::OP_VIDEO, bad).await,
            Some(vo::CLOSE_BAD_REQUEST)
        );
    }
}

#[tokio::test]
async fn heartbeat_must_ack_a_sent_sequence() {
    for (seq_ack, expected) in [
        (json!(-1), None),
        (json!(1), None),
        (json!(1000), Some(vo::CLOSE_BAD_REQUEST)),
        (json!("1"), Some(vo::CLOSE_BAD_REQUEST)),
        (json!(1.5), Some(vo::CLOSE_BAD_REQUEST)),
    ] {
        let d = |_: &Value| json!({ "t": 5, "seq_ack": seq_ack });
        assert_eq!(verdict(vo::OP_HEARTBEAT, d).await, expected, "{seq_ack}");
    }
}

#[tokio::test]
async fn missing_heartbeats_close_4009() {
    let mock = MockDiscord::start_with(MockConfig {
        voice_heartbeat_interval_ms: 100,
        ..MockConfig::default()
    })
    .await
    .unwrap();
    let (_gw, mut ws, _) = voice(&mock).await;
    assert_eq!(close_code(&mut ws).await, Some(vo::CLOSE_SESSION_TIMEOUT));
}

#[tokio::test]
async fn select_protocol_other_than_webrtc_closes_4012() {
    let mock = MockDiscord::start().await.unwrap();
    let (_gw, mut ws, _) = voice(&mock).await;
    let select = json!({ "protocol": "udp", "codecs": codecs("H264"), "data": {} });
    send(&mut ws, vo::OP_SELECT_PROTOCOL, select).await;
    assert_eq!(close_code(&mut ws).await, Some(vo::CLOSE_UNKNOWN_PROTOCOL));
}

#[tokio::test]
async fn select_protocol_without_the_servers_codec_closes_4020() {
    let mock = MockDiscord::start().await.unwrap();
    let (_gw, mut ws, _) = voice(&mock).await;
    let select = json!({ "protocol": "webrtc", "codecs": codecs("VP8"), "sdp": "" });
    send(&mut ws, vo::OP_SELECT_PROTOCOL, select).await;
    assert_eq!(close_code(&mut ws).await, Some(vo::CLOSE_BAD_REQUEST));
}
