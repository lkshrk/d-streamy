mod common;

use std::time::Duration;

use common::{SESSION_ID, STEP, Server, TOKEN, USER_ID, options};
use dstreamy_discord::{
    GatewayClient, GatewayError, GatewayEvent, IdentifyLedger, VoiceChannelKind,
    gateway::GatewayOptions,
};
use serde_json::json;
use tokio::sync::broadcast;
use tokio::time::{Instant, sleep, timeout};
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

/// Asserts the resumable close of an authentication timeout, no sooner than `min` after `since`.
async fn assert_auth_timeout(conn: &mut common::Conn, since: Instant, min: Duration) {
    let code = timeout(Duration::from_secs(1), conn.closed()).await;
    let waited = since.elapsed();
    assert_eq!(code.ok().flatten(), Some(4000));
    assert!(waited >= min, "closed after {waited:?}");
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
    assert_eq!(conn.close_frame().await, Some(1000));
    assert!(
        !closing.is_finished(),
        "the client finished closing before the server replied to its Close"
    );
    conn.flush().await;
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
    // Windows retries a refused loopback connect for about two seconds per attempt.
    let limit = if cfg!(windows) { STEP * 6 } else { STEP };
    let result = timeout(limit, GatewayClient::connect(opts)).await.unwrap();
    match result.err() {
        Some(GatewayError::Unreachable { attempts: 3, cause }) => {
            assert!(!cause.is_empty());
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn the_first_login_retries_for_its_window_then_gives_up_with_the_cause() {
    // Accepting and closing at once fails each attempt fast on every platform; a refused connect takes 2 s on Windows.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let accepted = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counter = accepted.clone();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            drop(socket);
        }
    });
    let window = Duration::from_millis(600);
    let mut opts = options(&url);
    opts.max_failed_attempts = 1000;
    opts.reconnect_delay_min = Duration::from_millis(50);
    opts.reconnect_delay_max = Duration::from_millis(100);
    opts.initial_give_up_after = Some(window);
    let began = std::time::Instant::now();
    let result = timeout(STEP, GatewayClient::connect(opts)).await.unwrap();
    assert!(
        began.elapsed() + Duration::from_millis(100) >= window,
        "gave up while another attempt still fit in its window"
    );
    let attempts = match result.err() {
        Some(GatewayError::Unreachable { attempts, cause }) => {
            assert!(!cause.is_empty());
            attempts
        }
        other => panic!("the first login used the reconnect limit: {other:?}"),
    };
    assert!(
        attempts >= 3,
        "it stopped retrying inside its window: {attempts}"
    );
    assert!(attempts < 1000, "the window never ended the retries");
    let all_accepted = timeout(STEP, async {
        while accepted.load(std::sync::atomic::Ordering::SeqCst) < attempts {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(all_accepted.is_ok(), "an attempt never reached the gateway");
    assert_eq!(
        attempts,
        accepted.load(std::sync::atomic::Ordering::SeqCst),
        "every attempt reached the gateway"
    );
}

#[tokio::test]
async fn the_first_login_window_bounds_an_attempt_in_flight() {
    let server = Server::bind().await;
    let mut opts = options(&server.url);
    opts.hello_timeout = Duration::from_secs(20);
    opts.initial_give_up_after = Some(Duration::from_millis(500));
    let client = tokio::spawn(GatewayClient::connect(opts));
    let _silent = server.accept().await;
    let result = timeout(Duration::from_secs(3), client)
        .await
        .expect("the attempt ran past the window")
        .unwrap();
    match result.err() {
        Some(GatewayError::Unreachable { cause, .. }) => {
            assert!(cause.contains("no HELLO"), "{cause}");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn the_first_login_window_stops_applying_after_ready() {
    let server = Server::bind().await;
    let mut opts = options(&server.url);
    opts.max_failed_attempts = 3;
    let window = Duration::from_millis(300);
    opts.initial_give_up_after = Some(window);
    let (client, mut conn) = connect(&server, opts).await;
    let mut events = client.subscribe();
    tokio::time::sleep(window * 2).await;
    for attempt in 0..3 {
        if attempt > 0 {
            conn = server.accept().await;
            conn.handshake(&format!("{}/resume", server.url)).await;
        }
        conn.send(json!({"op": 9, "d": false})).await;
        conn.recv_op(2).await;
        conn.ready(1, &format!("{}/resume", server.url)).await;
        conn.send(json!({"op": 9, "d": false})).await;
        assert_eq!(conn.recv_op_or_close(2).await.err(), Some(Some(4000)));
    }
    let closed = wait_for(&mut events, |e| matches!(e, GatewayEvent::Closed(_))).await;
    assert!(
        matches!(closed, GatewayEvent::Closed(GatewayError::Exhausted(3))),
        "{closed:?}"
    );
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
    let voice = json!({"guild_id": "200000000000000001", "channel_id": "300000000000000002", "self_mute": false, "self_deaf": true, "self_video": false});
    let create = json!({"type": "guild", "guild_id": "200000000000000001", "channel_id": "300000000000000002", "preferred_region": null});
    let paused = json!({"stream_key": "guild:1:2:3", "paused": false});
    let delete = json!({"stream_key": "guild:1:2:3"});
    client.send(4, voice.clone()).unwrap();
    client.send(18, create.clone()).unwrap();
    client.send(22, paused.clone()).unwrap();
    client.send(19, delete.clone()).unwrap();

    assert_eq!(conn.recv_op(4).await["d"], voice);
    assert_eq!(conn.recv_op(18).await["d"], create);
    assert_eq!(conn.recv_op(22).await["d"], paused);
    assert_eq!(conn.recv_op(19).await["d"], delete);

    conn.close(4000).await;
    let mut conn = server.accept().await;
    conn.hello(45_000).await;
    client
        .send(4, json!({"guild_id": null, "channel_id": null}))
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
    match result.err() {
        Some(GatewayError::Unreachable { attempts: 2, cause }) => {
            assert!(cause.contains("no READY"), "{cause}");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn missing_ready_after_invalid_session_reconnects() {
    let server = Server::bind().await;
    let mut opts = options(&server.url);
    opts.ready_timeout = Duration::from_millis(100);
    opts.healthy_session_min = Duration::from_millis(200);
    let (client, mut conn) = connect(&server, opts).await;

    conn.send(json!({"op": 9, "d": false})).await;
    conn.recv_op(2).await;
    conn.ready(1, &format!("{}/resume", server.url)).await;
    assert!(
        server
            .no_connection_within(Duration::from_millis(300))
            .await
    );

    conn.send(json!({"op": 9, "d": false})).await;
    conn.recv_op(2).await;
    assert_auth_timeout(&mut conn, Instant::now(), Duration::from_millis(80)).await;
    server.accept().await;
    client.close().await;
}

#[tokio::test]
async fn a_second_invalid_session_before_ready_closes() {
    let server = Server::bind().await;
    let client = tokio::spawn(GatewayClient::connect(options(&server.url)));
    let mut conn = server.accept().await;
    conn.hello(45_000).await;
    conn.recv_op(2).await;

    conn.send(json!({"op": 9, "d": false})).await;
    conn.recv_op(2).await;
    conn.send(json!({"op": 9, "d": false})).await;
    assert_eq!(conn.recv_op_or_close(2).await.err(), Some(Some(4000)));

    let mut conn = server.accept().await;
    conn.hello(45_000).await;
    conn.recv_op(2).await;
    conn.ready(1, &format!("{}/resume", server.url)).await;
    let client = timeout(STEP, client).await.unwrap().unwrap().unwrap();
    client.close().await;
}

#[tokio::test]
async fn ready_then_invalid_session_loops_close_and_exhaust() {
    let server = Server::bind().await;
    let mut opts = options(&server.url);
    opts.max_failed_attempts = 3;
    let (client, mut conn) = connect(&server, opts).await;
    let mut events = client.subscribe();
    for attempt in 0..3 {
        if attempt > 0 {
            conn = server.accept().await;
            conn.handshake(&format!("{}/resume", server.url)).await;
        }
        conn.send(json!({"op": 9, "d": false})).await;
        conn.recv_op(2).await;
        conn.ready(1, &format!("{}/resume", server.url)).await;
        conn.send(json!({"op": 9, "d": false})).await;
        assert_eq!(conn.recv_op_or_close(2).await.err(), Some(Some(4000)));
    }
    let closed = wait_for(&mut events, |e| matches!(e, GatewayEvent::Closed(_))).await;
    assert!(
        matches!(closed, GatewayEvent::Closed(GatewayError::Exhausted(3))),
        "{closed:?}"
    );
    assert!(
        server
            .no_connection_within(Duration::from_millis(100))
            .await
    );
}

#[tokio::test]
async fn the_daily_identify_cap_stops_reconnecting() {
    let server = Server::bind().await;
    let mut opts = options(&server.url);
    opts.max_identifies_per_day = 2;
    let (client, mut conn) = connect(&server, opts).await;
    let mut events = client.subscribe();

    conn.close(4000).await;
    let mut conn = server.accept().await;
    conn.hello(45_000).await;
    conn.recv_op(6).await;
    conn.dispatch(2, "RESUMED", json!({})).await;
    conn.close(4007).await;
    let mut conn = server.accept().await;
    conn.handshake(&format!("{}/resume", server.url)).await;
    conn.close(4007).await;

    let closed = wait_for(&mut events, |e| matches!(e, GatewayEvent::Closed(_))).await;
    assert!(
        matches!(
            closed,
            GatewayEvent::Closed(GatewayError::IdentifyLimit { limit: 2, .. })
        ),
        "{closed:?}"
    );
    assert!(
        server
            .no_connection_within(Duration::from_millis(100))
            .await
    );
}

#[tokio::test]
async fn a_repeated_hello_sends_no_second_identify() {
    let server = Server::bind().await;
    let client = tokio::spawn(GatewayClient::connect(options(&server.url)));
    let mut conn = server.accept().await;
    conn.hello(45_000).await;
    conn.recv_op(2).await;
    conn.hello(45_000).await;
    conn.hello(45_000).await;
    assert!(
        timeout(Duration::from_millis(200), conn.recv_op(2))
            .await
            .is_err()
    );
    conn.ready(1, &format!("{}/resume", server.url)).await;
    let client = timeout(STEP, client).await.unwrap().unwrap().unwrap();
    client.close().await;
}

#[tokio::test]
async fn a_replacement_client_keeps_counting_identifies() {
    let server = Server::bind().await;
    let ledger = IdentifyLedger::new(Duration::from_millis(500));
    let opts = || {
        let mut opts = options(&server.url);
        opts.max_identifies_per_day = 1;
        opts.identify_ledger = ledger.clone();
        opts
    };
    let started = Instant::now();
    let (client, mut conn) = connect(&server, opts()).await;
    let mut events = client.subscribe();
    conn.close(4007).await;
    let closed = wait_for(&mut events, |e| matches!(e, GatewayEvent::Closed(_))).await;
    assert!(
        matches!(
            closed,
            GatewayEvent::Closed(GatewayError::IdentifyLimit { limit: 1, .. })
        ),
        "{closed:?}"
    );

    let refused = timeout(STEP, GatewayClient::connect(opts())).await.unwrap();
    let refused = refused.err();
    assert!(
        matches!(refused, Some(GatewayError::IdentifyLimit { limit: 1, .. })),
        "{refused:?}"
    );
    let message = refused.unwrap().to_string();
    assert!(
        message.contains("Try again in less than a minute."),
        "{message}"
    );
    assert!(server.no_connection_within(Duration::from_millis(50)).await);
    assert!(started.elapsed() < Duration::from_millis(500));

    sleep(Duration::from_millis(600).saturating_sub(started.elapsed())).await;
    let (client, _conn) = connect(&server, opts()).await;
    assert_eq!(ledger.recent(), 1);
    client.close().await;
}

#[tokio::test]
async fn the_daily_identify_cap_stops_a_delayed_identify() {
    let server = Server::bind().await;
    let mut opts = options(&server.url);
    opts.max_identifies_per_day = 1;
    let (client, mut conn) = connect(&server, opts).await;
    let mut events = client.subscribe();

    conn.send(json!({"op": 9, "d": false})).await;
    assert_eq!(conn.recv_op_or_close(2).await.err(), Some(Some(1000)));
    let closed = wait_for(&mut events, |e| matches!(e, GatewayEvent::Closed(_))).await;
    assert!(
        matches!(
            closed,
            GatewayEvent::Closed(GatewayError::IdentifyLimit { limit: 1, .. })
        ),
        "{closed:?}"
    );
    assert!(
        server
            .no_connection_within(Duration::from_millis(100))
            .await
    );
}

#[tokio::test]
async fn an_invalid_session_during_login_does_not_eat_the_identify_deadline() {
    let server = Server::bind().await;
    let mut opts = options(&server.url);
    opts.ready_timeout = Duration::from_millis(300);
    opts.invalid_session_delay = Duration::from_secs(1);
    let client = tokio::spawn(GatewayClient::connect(opts));
    let mut conn = server.accept().await;
    conn.hello(45_000).await;
    conn.recv_op(2).await;

    conn.send(json!({"op": 9, "d": false})).await;
    conn.recv_op(2).await;
    assert_auth_timeout(&mut conn, Instant::now(), Duration::from_millis(200)).await;
    let mut conn = server.accept().await;
    conn.handshake(&format!("{}/resume", server.url)).await;
    let client = timeout(STEP, client).await.unwrap().unwrap().unwrap();
    client.close().await;
}

#[tokio::test]
async fn a_resume_does_not_restart_a_running_deadline() {
    let server = Server::bind().await;
    let mut opts = options(&server.url);
    opts.ready_timeout = Duration::from_millis(200);
    let (client, mut conn) = connect(&server, opts).await;

    conn.send(json!({"op": 9, "d": true})).await;
    conn.recv_op(6).await;
    let first = Instant::now();
    sleep(Duration::from_millis(120)).await;
    conn.send(json!({"op": 9, "d": true})).await;
    conn.recv_op(6).await;
    assert_auth_timeout(&mut conn, first, Duration::from_millis(160)).await;
    let waited = first.elapsed();
    assert!(
        waited < Duration::from_millis(290),
        "closed after {waited:?}"
    );
    server.accept().await;
    client.close().await;
}

#[tokio::test]
async fn connections_invalidated_right_after_ready_count_as_failed() {
    let server = Server::bind().await;
    let mut opts = options(&server.url);
    opts.ready_timeout = Duration::from_millis(300);
    opts.healthy_session_min = Duration::from_millis(200);
    opts.max_failed_attempts = 3;
    let (client, mut conn) = connect(&server, opts).await;
    let mut events = client.subscribe();
    for attempt in 0..3 {
        if attempt > 0 {
            conn = server.accept().await;
            conn.handshake(&format!("{}/resume", server.url)).await;
        }
        conn.send(json!({"op": 9, "d": false})).await;
        conn.recv_op(2).await;
        assert_auth_timeout(&mut conn, Instant::now(), Duration::from_millis(200)).await;
    }
    let closed = wait_for(&mut events, |e| matches!(e, GatewayEvent::Closed(_))).await;
    assert!(
        matches!(closed, GatewayEvent::Closed(GatewayError::Exhausted(3))),
        "{closed:?}"
    );
    assert!(
        server
            .no_connection_within(Duration::from_millis(100))
            .await
    );
}

#[tokio::test]
async fn an_authenticated_connection_resets_the_failure_count() {
    let server = Server::bind().await;
    let mut opts = options(&server.url);
    opts.ready_timeout = Duration::from_millis(100);
    opts.max_failed_attempts = 3;
    opts.healthy_session_min = Duration::from_millis(50);
    let client = tokio::spawn(GatewayClient::connect(opts));
    for _ in 0..2 {
        let mut conn = server.accept().await;
        conn.hello(45_000).await;
        conn.recv_op(2).await;
        conn.close(4000).await;
    }
    let mut conn = server.accept().await;
    conn.handshake(&format!("{}/resume", server.url)).await;
    let client = timeout(STEP, client).await.unwrap().unwrap().unwrap();
    sleep(Duration::from_millis(100)).await;

    conn.send(json!({"op": 9, "d": false})).await;
    conn.recv_op(2).await;
    assert_auth_timeout(&mut conn, Instant::now(), Duration::from_millis(80)).await;
    let mut conn = server.accept().await;
    conn.hello(45_000).await;
    conn.recv_op(2).await;
    assert!(!client.is_closed());
    client.close().await;
}

#[tokio::test]
async fn missing_resumed_after_resumable_invalid_session_reconnects() {
    let server = Server::bind().await;
    let mut opts = options(&server.url);
    opts.ready_timeout = Duration::from_millis(100);
    opts.healthy_session_min = Duration::from_millis(200);
    let (client, mut conn) = connect(&server, opts).await;

    conn.send(json!({"op": 9, "d": true})).await;
    conn.recv_op(6).await;
    sleep(Duration::from_millis(60)).await;
    conn.dispatch(2, "RESUMED", json!({})).await;
    assert!(
        server
            .no_connection_within(Duration::from_millis(300))
            .await
    );

    conn.send(json!({"op": 9, "d": true})).await;
    conn.recv_op(6).await;
    assert_auth_timeout(&mut conn, Instant::now(), Duration::from_millis(80)).await;
    server.accept().await;
    client.close().await;
}

#[tokio::test]
async fn resumed_then_resumable_invalid_session_loops_close_and_exhaust() {
    let server = Server::bind().await;
    let mut opts = options(&server.url);
    opts.max_failed_attempts = 3;
    let (client, mut conn) = connect(&server, opts).await;
    let mut events = client.subscribe();
    let mut resumes = 0;
    for attempt in 0..3 {
        if attempt > 0 {
            conn = server.accept().await;
            conn.hello(45_000).await;
            conn.recv_op(6).await;
            resumes += 1;
            conn.dispatch(2, "RESUMED", json!({})).await;
        }
        let closed = timeout(STEP, async {
            loop {
                conn.send(json!({"op": 9, "d": true})).await;
                match conn.recv_op_or_close(6).await {
                    Ok(_) => resumes += 1,
                    Err(code) => return code,
                }
                assert!(resumes < 20, "RESUME loop on one socket");
                conn.dispatch(2, "RESUMED", json!({})).await;
            }
        })
        .await
        .expect("the client kept resuming on one socket");
        assert_eq!(closed, Some(4000));
    }
    let closed = wait_for(&mut events, |e| matches!(e, GatewayEvent::Closed(_))).await;
    assert!(
        matches!(closed, GatewayEvent::Closed(GatewayError::Exhausted(3))),
        "{closed:?}"
    );
    assert!(resumes <= 6, "{resumes} RESUMEs");
    client.close().await;
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
    assert!(
        matches!(
            result.err(),
            Some(GatewayError::Unreachable { attempts: 2, ref cause }) if !cause.is_empty()
        ),
        "a failure before READY keeps a cause"
    );
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

#[tokio::test]
async fn a_gateway_that_closes_right_after_ready_counts_as_a_failed_attempt() {
    let server = Server::bind().await;
    let mut opts = options(&server.url);
    opts.max_failed_attempts = 2;
    let (client, mut conn) = connect(&server, opts).await;
    let mut events = client.subscribe();
    conn.close(4000).await;
    let mut conn = server.accept().await;
    conn.hello(45_000).await;
    conn.recv_op(6).await;
    conn.close(4000).await;
    let closed = wait_for(&mut events, |e| matches!(e, GatewayEvent::Closed(_))).await;
    assert!(
        matches!(closed, GatewayEvent::Closed(GatewayError::Exhausted(2))),
        "{closed:?}"
    );
    assert!(
        server
            .no_connection_within(Duration::from_millis(100))
            .await
    );
    client.close().await;
}
