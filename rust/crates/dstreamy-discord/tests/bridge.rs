mod common;

use std::time::Duration;

use common::{SESSION_ID, STEP, Server, USER_ID, options};
use dstreamy_discord::DiscordClient;
use dstreamy_discord::transport::StreamerError;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::tungstenite::Message;

const GUILD: &str = "200000000000000001";
const CHANNEL: &str = "300000000000000002";
const MOVED_TO: &str = "300000000000000003";

#[tokio::test]
async fn dispatches_reach_the_streamer_and_its_payloads_go_out() {
    let server = Server::bind().await;
    let client = tokio::spawn(DiscordClient::connect(options(&server.url)));
    let mut conn = server.accept().await;
    conn.handshake("ws://unused").await;
    let client = timeout(STEP, client).await.unwrap().unwrap().unwrap();
    assert_eq!(client.user().id, USER_ID);
    assert_eq!(client.guilds().len(), 2);

    let _voice = client.join_voice(Some(GUILD.into()), CHANNEL.into());
    assert_eq!(
        conn.recv_op(4).await["d"],
        json!({"guild_id": GUILD, "channel_id": CHANNEL, "self_mute": false, "self_deaf": true, "self_video": false})
    );
    assert!(matches!(
        client.create_stream(),
        Err(StreamerError::NoSession)
    ));

    conn.dispatch(
        2,
        "VOICE_STATE_UPDATE",
        json!({"guild_id": GUILD, "channel_id": CHANNEL, "user_id": "100000000000000099", "session_id": "someone-else"}),
    )
    .await;
    conn.dispatch(
        3,
        "VOICE_STATE_UPDATE",
        json!({"guild_id": GUILD, "channel_id": CHANNEL, "user_id": USER_ID, "session_id": SESSION_ID, "self_deaf": true}),
    )
    .await;

    let voice_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}", voice_listener.local_addr().unwrap());
    conn.dispatch(
        4,
        "VOICE_SERVER_UPDATE",
        json!({"guild_id": GUILD, "endpoint": endpoint, "token": "voice-token"}),
    )
    .await;
    let identify = voice_identify(&voice_listener).await;
    assert_eq!(identify["server_id"], GUILD);
    assert_eq!(identify["user_id"], USER_ID);
    assert_eq!(identify["session_id"], SESSION_ID);
    assert_eq!(identify["token"], "voice-token");

    let _stream = timeout(STEP, async {
        loop {
            match client.create_stream() {
                Ok(pending) => return pending,
                Err(_) => sleep(Duration::from_millis(5)).await,
            }
        }
    })
    .await
    .unwrap();
    let key = format!("guild:{GUILD}:{CHANNEL}:{USER_ID}");
    assert_eq!(
        conn.recv_op(18).await["d"],
        json!({"type": "guild", "guild_id": GUILD, "channel_id": CHANNEL, "preferred_region": null})
    );
    assert_eq!(
        conn.recv_op(22).await["d"],
        json!({"stream_key": key, "paused": false})
    );

    client.stop_stream();
    assert_eq!(conn.recv_op(19).await["d"], json!({"stream_key": key}));

    client.set_voice_channel(Some(GUILD.into()), MOVED_TO.into());
    let _moved = client.create_stream().unwrap();
    assert_eq!(conn.recv_op(18).await["d"]["channel_id"], MOVED_TO);
    assert_eq!(
        conn.recv_op(22).await["d"]["stream_key"],
        format!("guild:{GUILD}:{MOVED_TO}:{USER_ID}")
    );

    client.leave_voice();
    assert_eq!(
        conn.recv_op(4).await["d"],
        json!({"guild_id": null, "channel_id": null, "self_mute": true, "self_deaf": false, "self_video": false})
    );
    client.close().await;
}

async fn voice_identify(listener: &TcpListener) -> Value {
    let (tcp, _) = timeout(STEP, listener.accept()).await.unwrap().unwrap();
    let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
    ws.send(Message::Text(
        json!({"op": 8, "d": {"heartbeat_interval": 45_000}}).to_string(),
    ))
    .await
    .unwrap();
    timeout(STEP, async {
        loop {
            if let Some(Ok(Message::Text(t))) = ws.next().await {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["op"] == 0 {
                    return v["d"].clone();
                }
            }
        }
    })
    .await
    .unwrap()
}
