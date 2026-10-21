mod common;

use common::{annexb, answer_like_discord, fake_discord_peer, loopback_settings};
use discord_stream_rs::voice::VideoAttributes;
use discord_stream_rs::voice::connection::ConnectionError;
use discord_stream_rs::{GatewayEvent, GatewayPayload, Streamer, StreamerError, VoiceEvent};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::{WebSocketStream, accept_async};

type Ws = WebSocketStream<TcpStream>;

const T: Duration = Duration::from_secs(10);

async fn accept(listener: &TcpListener) -> Ws {
    accept_with_heartbeat(listener, 1000).await
}

async fn accept_with_heartbeat(listener: &TcpListener, interval_ms: u64) -> Ws {
    let (tcp, _) = timeout(T, listener.accept()).await.unwrap().unwrap();
    let mut ws = accept_async(tcp).await.unwrap();
    send(
        &mut ws,
        json!({ "op": 8, "d": { "heartbeat_interval": interval_ms } }),
    )
    .await;
    ws
}

async fn send(ws: &mut Ws, v: Value) {
    ws.send(Message::Text(v.to_string())).await.unwrap();
}

/// Next JSON message with opcode `op`; other messages are skipped.
async fn recv_op(ws: &mut Ws, op: u64) -> Value {
    timeout(T, async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    if v["op"] == op {
                        return v["d"].clone();
                    }
                }
                Some(Ok(_)) => {}
                other => panic!("voice WS ended while waiting for op {op}: {other:?}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for op {op}"))
}

async fn gateway_op(rx: &mut mpsc::UnboundedReceiver<GatewayPayload>) -> GatewayPayload {
    timeout(T, rx.recv()).await.unwrap().unwrap()
}

fn dispatch(name: &str, d: Value) -> GatewayEvent {
    GatewayEvent::from_dispatch(name, d)
}

#[tokio::test(flavor = "multi_thread")]
async fn voice_and_stream_lifecycle_against_fake_gateway() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    let (gw_tx, mut gw_rx) = mpsc::unbounded_channel();
    let mut streamer = Streamer::new("42".into(), gw_tx);
    streamer.set_ice_servers(vec![]);
    streamer.set_setting_engine(loopback_settings(false));

    let mut voice = streamer.join_voice(Some("100".into()), "200".into());
    let join = gateway_op(&mut gw_rx).await;
    assert_eq!(join.op, 4);
    assert_eq!(join.d["channel_id"], "200");
    assert_eq!(join.d["self_deaf"], true);

    streamer.handle_event(dispatch(
        "VOICE_STATE_UPDATE",
        json!({ "user_id": "42", "session_id": "sess" }),
    ));
    streamer.handle_event(dispatch(
        "VOICE_SERVER_UPDATE",
        json!({ "guild_id": "999", "endpoint": "ws://127.0.0.1:1", "token": "other" }),
    ));
    streamer.handle_event(dispatch(
        "VOICE_SERVER_UPDATE",
        json!({ "guild_id": "100", "endpoint": endpoint, "token": "tok" }),
    ));

    let mut ws = accept(&listener).await;
    let identify = recv_op(&mut ws, 0).await;
    assert_eq!(identify["server_id"], "100");
    assert_eq!(identify["session_id"], "sess");
    assert_eq!(identify["token"], "tok");
    assert_eq!(identify["video"], true);
    assert_eq!(identify["max_dave_protocol_version"], 1);
    assert_eq!(identify["streams"][0]["type"], "screen");

    send(
        &mut ws,
        json!({ "op": 2, "seq": 1, "d": {
            "ssrc": 1111, "ip": "127.0.0.1", "port": 1234,
            "modes": ["aead_aes256_gcm_rtpsize", "aead_xchacha20_poly1305_rtpsize"],
            "streams": [{ "ssrc": 2222, "rtx_ssrc": 2223, "rid": "100", "quality": 100, "active": false }]
        }}),
    )
    .await;
    let select = recv_op(&mut ws, 1).await;
    assert_eq!(select["protocol"], "webrtc");
    assert_eq!(select["sdp"], select["data"]);
    assert_eq!(select["codecs"].as_array().unwrap().len(), 6);
    assert_eq!(select["rtc_connection_id"].as_str().unwrap().len(), 36);
    let video_off = recv_op(&mut ws, 12).await;
    assert_eq!(video_off["audio_ssrc"], 1111);
    assert_eq!(video_off["video_ssrc"], 0);

    let (peer, mut rtp_rx) = fake_discord_peer().await;
    let sdp = answer_like_discord(&peer, select["sdp"].as_str().unwrap().to_owned()).await;
    send(
        &mut ws,
        json!({ "op": 4, "seq": 2, "d": {
            "sdp": sdp, "dave_protocol_version": 0, "audio_codec": "opus", "video_codec": "H264"
        }}),
    )
    .await;
    let conn = timeout(T, voice.ready()).await.unwrap().unwrap();
    assert_eq!(recv_op(&mut ws, 3).await["seq_ack"], 2);

    timeout(T, async {
        while !conn.is_ready().await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("peer connection did not connect");
    conn.set_packetizer("H264").await.unwrap();
    conn.set_speaking(true).unwrap();
    let speaking = recv_op(&mut ws, 5).await;
    assert_eq!(
        (speaking["speaking"].clone(), speaking["ssrc"].clone()),
        (json!(1), json!(1111))
    );
    conn.set_video_attributes(Some(VideoAttributes {
        width: 1280,
        height: 720,
        fps: 30,
    }))
    .unwrap();
    let video_on = recv_op(&mut ws, 12).await;
    assert_eq!(video_on["streams"][0]["max_resolution"]["height"], 720);

    let frame = annexb(&[&[0x65, 0x88, 0x84, 0x21]]);
    timeout(T, async {
        let mut pts = Duration::ZERO;
        loop {
            conn.send_video_frame_at(&frame, pts).await.unwrap();
            pts += Duration::from_millis(33);
            tokio::time::sleep(Duration::from_millis(20)).await;
            if let Ok(p) = rtp_rx.try_recv() {
                assert_eq!(p.header.ssrc, 2222);
                break;
            }
        }
    })
    .await
    .expect("no RTP received");

    ws.close(Some(CloseFrame {
        code: CloseCode::from(4015),
        reason: "".into(),
    }))
    .await
    .unwrap();
    let mut ws = accept(&listener).await;
    let resume = recv_op(&mut ws, 7).await;
    assert_eq!(resume["server_id"], "100");
    assert_eq!(resume["session_id"], "sess");
    assert_eq!(resume["seq_ack"], 2);
    send(&mut ws, json!({ "op": 9, "d": null })).await;
    timeout(T, async {
        while let Some(ev) = voice.events.recv().await {
            if matches!(ev, VoiceEvent::Resumed) {
                return;
            }
        }
        panic!("event channel closed");
    })
    .await
    .expect("no Resumed event");

    let _stream = streamer.create_stream().unwrap();
    let create = gateway_op(&mut gw_rx).await;
    assert_eq!(create.op, 18);
    assert_eq!(
        create.d,
        json!({ "type": "guild", "guild_id": "100", "channel_id": "200", "preferred_region": null })
    );
    let paused = gateway_op(&mut gw_rx).await;
    assert_eq!(paused.op, 22);
    assert_eq!(
        paused.d,
        json!({ "stream_key": "guild:100:200:42", "paused": false })
    );

    streamer.handle_event(dispatch(
        "STREAM_CREATE",
        json!({ "stream_key": "guild:100:200:42", "rtc_server_id": "555" }),
    ));
    streamer.handle_event(dispatch(
        "STREAM_SERVER_UPDATE",
        json!({ "stream_key": "guild:100:200:42", "endpoint": endpoint, "token": "stok" }),
    ));
    let mut stream_ws = accept(&listener).await;
    let stream_identify = recv_op(&mut stream_ws, 0).await;
    assert_eq!(stream_identify["server_id"], "555");
    assert_eq!(stream_identify["session_id"], "sess");
    assert_eq!(stream_identify["token"], "stok");

    streamer.stop_stream();
    let delete = gateway_op(&mut gw_rx).await;
    assert_eq!(
        (delete.op, delete.d["stream_key"].clone()),
        (19, json!("guild:100:200:42"))
    );
    timeout(T, async {
        loop {
            match stream_ws.next().await {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            }
        }
    })
    .await
    .expect("stream WS was not closed");

    streamer.leave_voice();
    let leave = gateway_op(&mut gw_rx).await;
    assert_eq!(leave.op, 4);
    assert_eq!(leave.d["channel_id"], Value::Null);
    peer.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn channel_move_retargets_next_stream_without_rejoining() {
    let (gw_tx, mut gw_rx) = mpsc::unbounded_channel();
    let mut streamer = Streamer::new("42".into(), gw_tx);
    let _voice = streamer.join_voice(Some("100".into()), "200".into());
    gateway_op(&mut gw_rx).await;
    streamer.handle_event(dispatch(
        "VOICE_STATE_UPDATE",
        json!({ "user_id": "42", "session_id": "sess" }),
    ));
    streamer.set_voice_channel(Some("100".into()), "300".into());
    let _stream = streamer.create_stream().unwrap();
    let create = gateway_op(&mut gw_rx).await;
    assert_eq!(create.op, 18);
    assert_eq!(create.d["channel_id"], "300");
    assert_eq!(
        gateway_op(&mut gw_rx).await.d["stream_key"],
        "guild:100:300:42"
    );
    assert!(gw_rx.try_recv().is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_heartbeat_acks_trigger_resume() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    let (gw_tx, _gw_rx) = mpsc::unbounded_channel();
    let mut streamer = Streamer::new("42".into(), gw_tx);
    let _voice = streamer.join_voice(Some("100".into()), "200".into());
    streamer.handle_event(dispatch(
        "VOICE_STATE_UPDATE",
        json!({ "user_id": "42", "session_id": "sess" }),
    ));
    streamer.handle_event(dispatch(
        "VOICE_SERVER_UPDATE",
        json!({ "guild_id": "100", "endpoint": endpoint, "token": "tok" }),
    ));
    let mut ws = accept_with_heartbeat(&listener, 30).await;
    recv_op(&mut ws, 0).await;
    let silent = tokio::spawn(async move { while ws.next().await.is_some() {} });
    let mut ws = accept(&listener).await;
    assert_eq!(recv_op(&mut ws, 7).await["session_id"], "sess");
    silent.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn unusable_dave_channel_ids_fail_the_connection() {
    let (gw_tx, mut gw_rx) = mpsc::unbounded_channel();
    let mut streamer = Streamer::new("42".into(), gw_tx);
    let mut voice = streamer.join_voice(Some("100".into()), "general".into());
    assert!(matches!(
        voice.ready().await,
        Err(StreamerError::Connection(
            ConnectionError::InvalidChannelId(_)
        ))
    ));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    let _voice = streamer.join_voice(Some("100".into()), "200".into());
    gateway_op(&mut gw_rx).await;
    streamer.handle_event(dispatch(
        "VOICE_STATE_UPDATE",
        json!({ "user_id": "42", "session_id": "sess" }),
    ));
    let mut stream = streamer.create_stream().unwrap();
    for rtc_server_id in ["0", "555"] {
        streamer.handle_event(dispatch(
            "STREAM_CREATE",
            json!({ "stream_key": "guild:100:200:42", "rtc_server_id": rtc_server_id }),
        ));
    }
    streamer.handle_event(dispatch(
        "STREAM_SERVER_UPDATE",
        json!({ "stream_key": "guild:100:200:42", "endpoint": endpoint, "token": "stok" }),
    ));
    assert!(matches!(
        stream.ready().await,
        Err(StreamerError::Connection(
            ConnectionError::InvalidChannelId(_)
        ))
    ));
    assert!(
        timeout(Duration::from_millis(300), listener.accept())
            .await
            .is_err()
    );
}
