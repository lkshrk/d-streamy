mod common;

use std::time::Duration;

use common::{SESSION_ID, STEP, Server, TOKEN, USER_ID, options};
use dstreamy_discord::{
    GatewayClient, GatewayError, GatewayEvent, VoiceChannelKind, gateway::GatewayOptions,
};
use serde_json::json;
use tokio::sync::broadcast;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::tungstenite::Message;

async fn connect(server: &Server, opts: GatewayOptions) -> (GatewayClient, common::Conn) {
    let resume = format!("{}/resume", server.url);
    let client = tokio::spawn(GatewayClient::connect(opts));
    let mut conn = server.accept().await;
    conn.handshake(&resume).await;
    let client = timeout(STEP, client).await.unwrap().unwrap().unwrap();
    (client, conn)
}

async fn wait_for<F: Fn(&GatewayEvent) -> bool>(
    rx: &mut broadcast::Receiver<GatewayEvent>,
    pred: F,
) -> GatewayEvent {
    timeout(STEP, async {
        loop {
            let event = rx.recv().await.unwrap();
            if pred(&event) {
                return event;
            }
        }
    })
    .await
    .expect("event not received")
}

async fn eventually<F: Fn() -> bool>(cond: F) {
    timeout(STEP, async {
        while !cond() {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("condition not reached");
}

#[tokio::test]
async fn identify_matches_the_user_client() {
    let server = Server::bind().await;
    let client = tokio::spawn(GatewayClient::connect(options(&server.url)));
    let mut conn = server.accept().await;
    assert_eq!(conn.path, "/?v=9&encoding=json");
    conn.hello(45_000).await;
    let identify = conn.recv_op(2).await;
    let d = &identify["d"];
    assert_eq!(d["token"], TOKEN);
    assert_eq!(d["capabilities"], 0);
    assert_eq!(d["compress"], false);
    assert_eq!(d["client_state"], json!({"guild_versions": {}}));
    assert_eq!(
        d["presence"],
        json!({"status": "online", "since": 0, "activities": [], "afk": true})
    );
    assert!(d.get("intents").is_none());
    let p = &d["properties"];
    assert_eq!(p["os"], "Windows");
    assert_eq!(p["browser"], "Discord Client");
    assert_eq!(p["release_channel"], "stable");
    assert_eq!(p["client_version"], "1.0.9210");
    assert_eq!(p["client_build_number"], 455_964);
    assert_eq!(p["native_build_number"], 69_976);
    assert_eq!(p["has_client_mods"], false);
    assert_eq!(p["client_event_source"], json!(null));
    assert_eq!(p["client_app_state"], "focused");
    assert!(
        p["browser_user_agent"]
            .as_str()
            .unwrap()
            .contains("Electron/35.3.0")
    );
    assert_eq!(p["client_launch_id"].as_str().unwrap().len(), 36);

    conn.ready(1, "ws://unused").await;
    let client = timeout(STEP, client).await.unwrap().unwrap().unwrap();
    assert_eq!(client.user().id, USER_ID);
    assert_eq!(client.user().username, "streamer");
    client.close().await;
    assert_eq!(conn.closed().await, Some(1000));
}

#[tokio::test(flavor = "multi_thread")]
async fn subscribers_made_after_connect_never_see_the_first_ready() {
    for _ in 0..20 {
        let server = Server::bind().await;
        let (client, mut conn) = connect(&server, options(&server.url)).await;
        let mut events = client.subscribe();
        conn.dispatch(2, "RESUMED", json!({})).await;
        let first = wait_for(&mut events, |_| true).await;
        assert!(
            !matches!(first, GatewayEvent::Ready { .. }),
            "first event after connect: {first:?}"
        );
        client.close().await;
    }
}

#[tokio::test]
async fn close_waits_for_the_servers_close_before_dropping_the_socket() {
    let server = Server::bind().await;
    let (client, mut conn) = connect(&server, options(&server.url)).await;
    conn.dispatch(2, "SESSIONS_REPLACE", json!([])).await;
    let closing = tokio::spawn(client.close());
    sleep(Duration::from_millis(300)).await;
    assert!(
        !closing.is_finished(),
        "the client dropped the socket before the server's Close"
    );
    assert_eq!(conn.closed().await, Some(1000));
    timeout(Duration::from_secs(1), closing)
        .await
        .expect("the client missed the server's Close and waited out its timeout")
        .unwrap();
}

#[tokio::test]
async fn ready_and_guild_dispatches_fill_the_cache() {
    let server = Server::bind().await;
    let (client, mut conn) = connect(&server, options(&server.url)).await;

    let guilds = client.guilds();
    assert_eq!(guilds.len(), 2);
    assert_eq!(
        (guilds[0].id.as_str(), guilds[0].name.as_str()),
        ("200000000000000001", "Gaming")
    );
    assert!(guilds[0].available);
    assert!(!guilds[1].available);

    let channels = client.voice_channels("200000000000000001");
    let summary: Vec<_> = channels
        .iter()
        .map(|c| (c.name.as_str(), c.position, c.kind))
        .collect();
    assert_eq!(
        summary,
        [
            ("Lounge 1", 1, VoiceChannelKind::Voice),
            ("Lounge 2", 2, VoiceChannelKind::Voice),
            ("Stage", 3, VoiceChannelKind::Stage),
        ]
    );
    assert_eq!(channels[0].id, "300000000000000002");
    assert_eq!(channels[0].parent_id.as_deref(), Some("300000000000000010"));
    assert_eq!(channels[0].guild_id, "200000000000000001");

    conn.dispatch(
        2,
        "GUILD_CREATE",
        json!({
            "id": "200000000000000002", "name": "Friends", "member_count": 3,
            "channels": [{"id": "310000000000000001", "type": 2, "name": "Hangout", "position": 0}],
        }),
    )
    .await;
    conn.dispatch(
        3,
        "CHANNEL_CREATE",
        json!({"id": "300000000000000006", "type": 2, "guild_id": "200000000000000001", "name": "Lounge 0", "position": 0}),
    )
    .await;
    conn.dispatch(
        4,
        "CHANNEL_DELETE",
        json!({"id": "300000000000000004", "type": 13, "guild_id": "200000000000000001", "name": "Stage", "position": 3}),
    )
    .await;
    conn.dispatch(5, "GUILD_DELETE", json!({"id": "200000000000000002"}))
        .await;

    eventually(|| client.guilds().len() == 1).await;
    let names: Vec<_> = client
        .voice_channels("200000000000000001")
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(names, ["Lounge 0", "Lounge 1", "Lounge 2"]);
    assert!(client.voice_channels("200000000000000002").is_empty());
    client.close().await;
}

#[tokio::test]
async fn heartbeats_carry_the_sequence_and_acks_keep_the_session() {
    let server = Server::bind().await;
    let client = tokio::spawn(GatewayClient::connect(options(&server.url)));
    let mut conn = server.accept().await;
    conn.hello(100).await;
    conn.recv_op(2).await;
    conn.ready(1, &format!("{}/resume", server.url)).await;
    conn.dispatch(7, "SESSIONS_REPLACE", json!([])).await;

    loop {
        let hb = conn.recv_op(1).await;
        if hb["d"] == 7 {
            break;
        }
    }
    for _ in 0..4 {
        conn.recv_op(1).await;
    }
    let client = timeout(STEP, client).await.unwrap().unwrap().unwrap();
    assert!(client.latency().is_some());

    let until = tokio::time::Instant::now() + Duration::from_millis(400);
    while tokio::time::Instant::now() < until {
        conn.recv_op(1).await;
    }
    assert!(!client.is_closed());
    client.close().await;
}

#[tokio::test]
async fn missing_heartbeat_ack_resumes_on_the_resume_url() {
    let server = Server::bind().await;
    let client = tokio::spawn(GatewayClient::connect(options(&server.url)));
    let mut conn = server.accept().await;
    conn.auto_ack = false;
    conn.hello(100).await;
    conn.recv_op(2).await;
    conn.ready(1, &format!("{}/resume", server.url)).await;
    let client = timeout(STEP, client).await.unwrap().unwrap().unwrap();
    let mut events = client.subscribe();
    conn.dispatch(2, "SESSIONS_REPLACE", json!([])).await;

    assert_eq!(conn.closed().await, Some(4000));
    let mut conn = server.accept().await;
    assert_eq!(conn.path, "/resume?v=9&encoding=json");
    conn.hello(45_000).await;
    let resume = conn.recv_op(6).await;
    assert_eq!(
        resume["d"],
        json!({"token": TOKEN, "session_id": SESSION_ID, "seq": 2})
    );
    conn.dispatch(3, "RESUMED", json!({})).await;
    wait_for(&mut events, |e| matches!(e, GatewayEvent::Resumed)).await;
    client.close().await;
}

#[tokio::test]
async fn reconnect_opcode_resumes() {
    let server = Server::bind().await;
    let (client, mut conn) = connect(&server, options(&server.url)).await;
    let mut events = client.subscribe();
    conn.dispatch(5, "SESSIONS_REPLACE", json!([])).await;
    conn.send(json!({"op": 7, "d": null})).await;
    assert_eq!(conn.closed().await, Some(4000));
    wait_for(&mut events, |e| {
        matches!(e, GatewayEvent::Disconnected { resume: true, .. })
    })
    .await;

    let mut conn = server.accept().await;
    assert!(conn.path.starts_with("/resume?"));
    conn.hello(45_000).await;
    let resume = conn.recv_op(6).await;
    assert_eq!(resume["d"]["session_id"], SESSION_ID);
    assert_eq!(resume["d"]["seq"], 5);
    client.close().await;
}

#[tokio::test]
async fn invalid_session_reidentifies_or_resumes() {
    let server = Server::bind().await;
    let (client, mut conn) = connect(&server, options(&server.url)).await;
    let mut events = client.subscribe();

    conn.send(json!({"op": 9, "d": true})).await;
    let resume = conn.recv().await;
    assert_eq!(resume["op"], 6);
    assert_eq!(resume["d"]["session_id"], SESSION_ID);
    conn.dispatch(2, "RESUMED", json!({})).await;
    wait_for(&mut events, |e| matches!(e, GatewayEvent::Resumed)).await;

    conn.send(json!({"op": 9, "d": false})).await;
    wait_for(&mut events, |e| {
        matches!(e, GatewayEvent::InvalidSession { resumable: false })
    })
    .await;
    let identify = conn.recv_op(2).await;
    assert_eq!(identify["d"]["token"], TOKEN);
    conn.ready(1, &format!("{}/resume", server.url)).await;
    wait_for(&mut events, |e| matches!(e, GatewayEvent::Ready { .. })).await;
    assert!(
        server
            .no_connection_within(Duration::from_millis(100))
            .await
    );
    client.close().await;
}

#[tokio::test]
async fn session_timeout_close_reidentifies() {
    let server = Server::bind().await;
    let (client, mut conn) = connect(&server, options(&server.url)).await;
    conn.close(4009).await;
    let mut conn = server.accept().await;
    assert_eq!(conn.path, "/?v=9&encoding=json");
    conn.hello(45_000).await;
    let packet = conn.recv().await;
    assert_eq!(packet["op"], 2);
    client.close().await;
}

#[tokio::test]
async fn authentication_failure_is_fatal() {
    let server = Server::bind().await;
    let client = tokio::spawn(GatewayClient::connect(options(&server.url)));
    let mut conn = server.accept().await;
    conn.hello(45_000).await;
    conn.recv_op(2).await;
    conn.close(4004).await;
    let result = timeout(STEP, client).await.unwrap().unwrap();
    assert_eq!(result.err(), Some(GatewayError::Rejected(4004)));
    assert!(
        server
            .no_connection_within(Duration::from_millis(100))
            .await
    );
}

#[tokio::test]
async fn unreachable_gateway_gives_up_after_the_attempt_limit() {
    // Port 1 is privileged and unused, so the dial is refused without racing other tests' binds.
    let mut opts = options("ws://127.0.0.1:1");
    opts.max_failed_attempts = 3;
    let result = timeout(STEP, GatewayClient::connect(opts)).await.unwrap();
    assert_eq!(result.err(), Some(GatewayError::Exhausted(3)));
}

#[tokio::test]
async fn zlib_stream_messages_split_across_frames() {
    let server = Server::bind().await;
    let mut opts = options(&server.url);
    opts.compress = true;
    let client = tokio::spawn(GatewayClient::connect(opts));
    let mut conn = server.accept().await;
    assert_eq!(conn.path, "/?v=9&encoding=json&compress=zlib-stream");
    conn.enable_zlib();
    conn.hello(45_000).await;
    conn.recv_op(2).await;

    let ready = json!({"op": 0, "s": 1, "t": "READY", "d": common::ready_fixture("ws://unused")});
    let frame = conn.zlib_frame(&ready);
    let (head, tail) = frame.split_at(frame.len() / 3);
    conn.send_raw(Message::Binary(head.to_vec())).await;
    conn.send_raw(Message::Binary(tail.to_vec())).await;

    let client = timeout(STEP, client).await.unwrap().unwrap().unwrap();
    assert_eq!(client.voice_channels("200000000000000001").len(), 3);
    conn.dispatch(2, "GUILD_DELETE", json!({"id": "200000000000000002"}))
        .await;
    eventually(|| client.guilds().len() == 1).await;
    client.close().await;
}

#[tokio::test]
async fn voice_and_stream_ops_wait_for_ready_and_go_out() {
    let server = Server::bind().await;
    let (client, mut conn) = connect(&server, options(&server.url)).await;
    client
        .update_voice_state(
            Some("200000000000000001"),
            Some("300000000000000002"),
            false,
            true,
            false,
        )
        .unwrap();
    client
        .stream_create(Some("200000000000000001"), "300000000000000002")
        .unwrap();
    client.stream_set_paused("guild:1:2:3", false).unwrap();
    client.stream_delete("guild:1:2:3").unwrap();

    assert_eq!(
        conn.recv_op(4).await["d"],
        json!({"guild_id": "200000000000000001", "channel_id": "300000000000000002", "self_mute": false, "self_deaf": true, "self_video": false})
    );
    assert_eq!(
        conn.recv_op(18).await["d"],
        json!({"type": "guild", "guild_id": "200000000000000001", "channel_id": "300000000000000002", "preferred_region": null})
    );
    assert_eq!(
        conn.recv_op(22).await["d"],
        json!({"stream_key": "guild:1:2:3", "paused": false})
    );
    assert_eq!(
        conn.recv_op(19).await["d"],
        json!({"stream_key": "guild:1:2:3"})
    );

    conn.close(4000).await;
    let mut conn = server.accept().await;
    conn.hello(45_000).await;
    client
        .update_voice_state(None, None, true, false, false)
        .unwrap();
    conn.recv_op(6).await;
    assert!(
        timeout(Duration::from_millis(100), conn.recv_op(4))
            .await
            .is_err(),
        "op 4 must wait for RESUMED"
    );
    conn.dispatch(2, "RESUMED", json!({})).await;
    assert_eq!(conn.recv_op(4).await["d"]["channel_id"], json!(null));
    client.close().await;
}

#[tokio::test]
async fn heartbeat_request_is_answered_at_once() {
    let server = Server::bind().await;
    let client = tokio::spawn(GatewayClient::connect(options(&server.url)));
    let mut conn = server.accept().await;
    conn.hello(600_000).await;
    conn.recv_op(2).await;
    conn.ready(3, "ws://unused").await;
    let client = timeout(STEP, client).await.unwrap().unwrap().unwrap();
    conn.send(json!({"op": 1, "d": null})).await;
    let hb = timeout(Duration::from_secs(1), conn.recv_op(1))
        .await
        .expect("no heartbeat");
    assert_eq!(hb["d"], 3);
    client.close().await;
}

#[tokio::test]
async fn missing_ready_counts_as_a_failed_attempt() {
    let server = Server::bind().await;
    let mut opts = options(&server.url);
    opts.ready_timeout = Duration::from_millis(100);
    opts.max_failed_attempts = 2;
    let client = tokio::spawn(GatewayClient::connect(opts));
    for _ in 0..2 {
        let mut conn = server.accept().await;
        conn.hello(45_000).await;
        conn.recv_op(2).await;
        assert_eq!(conn.closed().await, Some(4000));
    }
    let result = timeout(STEP, client).await.unwrap().unwrap();
    assert_eq!(result.err(), Some(GatewayError::Exhausted(2)));
}

#[tokio::test]
async fn reconnect_before_ready_counts_as_a_failed_attempt() {
    let server = Server::bind().await;
    let mut opts = options(&server.url);
    opts.max_failed_attempts = 2;
    let client = tokio::spawn(GatewayClient::connect(opts));
    for _ in 0..2 {
        let mut conn = server.accept().await;
        conn.hello(45_000).await;
        conn.recv_op(2).await;
        conn.send(json!({"op": 7, "d": null})).await;
        assert_eq!(conn.closed().await, Some(4000));
    }
    let result = timeout(STEP, client).await.unwrap().unwrap();
    assert_eq!(result.err(), Some(GatewayError::Exhausted(2)));
}

#[tokio::test]
async fn a_huge_heartbeat_interval_is_clamped() {
    let server = Server::bind().await;
    let client = tokio::spawn(GatewayClient::connect(options(&server.url)));
    let mut conn = server.accept().await;
    conn.hello(u64::MAX).await;
    conn.recv_op(2).await;
    conn.ready(1, &format!("{}/resume", server.url)).await;
    let client = timeout(STEP, client).await.unwrap().unwrap().unwrap();
    assert!(!client.is_closed());
    client.close().await;
}

#[tokio::test]
async fn unbounded_subscribers_never_lag() {
    let server = Server::bind().await;
    let (client, mut conn) = connect(&server, options(&server.url)).await;
    let mut lossless = client.subscribe_unbounded();
    for seq in 2..602 {
        conn.dispatch(seq, "SESSIONS_REPLACE", json!([])).await;
    }
    conn.dispatch(602, "RESUMED", json!({})).await;
    let mut seen = 0;
    timeout(STEP, async {
        while let Some(event) = lossless.recv().await {
            match event {
                GatewayEvent::Dispatch(d) if d.name == "SESSIONS_REPLACE" => seen += 1,
                GatewayEvent::Resumed => break,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(seen, 600);
    client.close().await;
}
