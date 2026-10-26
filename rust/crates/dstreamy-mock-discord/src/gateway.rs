//! Main gateway (v9, JSON) as Discord serves it to a user account.

use std::sync::Arc;

use flate2::{Compress, Compression, FlushCompress};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::{WebSocketStream, accept_hdr_async};
use tracing::debug;

use crate::config::{MockConfig, MockGuild};
use crate::state::{
    Connection, Control, GatewaySession, OpPayload, ReceivedOp, Shared, Socket, State,
    StreamSnapshot, VoiceAuth, VoiceKind, VoiceStateSnapshot,
};
use crate::util::{random_hex, random_snowflake};

pub const OP_DISPATCH: u8 = 0;
pub const OP_HEARTBEAT: u8 = 1;
pub const OP_IDENTIFY: u8 = 2;
pub const OP_VOICE_STATE_UPDATE: u8 = 4;
pub const OP_RESUME: u8 = 6;
pub const OP_RECONNECT: u8 = 7;
pub const OP_INVALID_SESSION: u8 = 9;
pub const OP_HELLO: u8 = 10;
pub const OP_HEARTBEAT_ACK: u8 = 11;
pub const OP_STREAM_CREATE: u8 = 18;
pub const OP_STREAM_DELETE: u8 = 19;
pub const OP_STREAM_WATCH: u8 = 20;
pub const OP_STREAM_SET_PAUSED: u8 = 22;

pub const CLOSE_UNKNOWN_ERROR: u16 = 4000;
pub const CLOSE_DECODE_ERROR: u16 = 4002;
pub const CLOSE_NOT_AUTHENTICATED: u16 = 4003;
pub const CLOSE_AUTHENTICATION_FAILED: u16 = 4004;
pub const CLOSE_ALREADY_AUTHENTICATED: u16 = 4005;

pub(crate) async fn serve(shared: Arc<Shared>, listener: TcpListener) {
    while let Ok((tcp, _)) = listener.accept().await {
        tokio::spawn(handle(shared.clone(), tcp));
    }
}

async fn handle(shared: Arc<Shared>, tcp: TcpStream) {
    let mut zlib = false;
    // tungstenite's handshake `Callback` fixes the error type.
    #[allow(clippy::result_large_err)]
    let callback = |req: &Request, resp: Response| {
        zlib = req
            .uri()
            .query()
            .is_some_and(|q| q.split('&').any(|kv| kv == "compress=zlib-stream"));
        Ok(resp)
    };
    let Ok(ws) = accept_hdr_async(tcp, callback).await else {
        return;
    };
    let compress = zlib.then(|| Compress::new(Compression::default(), true));
    shared.state().gateway_zlib = Some(zlib);
    let id = shared.next_id();
    let (tx, rx) = mpsc::unbounded_channel();
    shared.state().gateway = Some((Connection { id, tx: tx.clone() }, None));
    let hello = json!({
        "op": OP_HELLO, "s": null, "t": null,
        "d": { "heartbeat_interval": shared.config.heartbeat_interval_ms, "_trace": ["mock-gateway"] },
    });
    let _ = tx.send(Control::Send(hello.to_string()));
    drop(tx);
    run(&shared, id, ws, rx, compress).await;
    let mut state = shared.state();
    if state.gateway.as_ref().is_some_and(|(c, _)| c.id == id) {
        state.gateway = None;
    }
}

async fn run(
    shared: &Shared,
    id: u64,
    mut ws: WebSocketStream<TcpStream>,
    mut rx: mpsc::UnboundedReceiver<Control>,
    mut compress: Option<Compress>,
) {
    loop {
        tokio::select! {
            msg = ws.next() => match msg {
                Some(Ok(Message::Text(text))) => {
                    if let Err(code) = on_text(shared, id, &text) {
                        close(&mut ws, code).await;
                        return;
                    }
                }
                Some(Ok(Message::Close(_))) => {
                    let _ = ws.flush().await;
                    return;
                }
                None | Some(Err(_)) => return,
                Some(Ok(_)) => {}
            },
            ctl = rx.recv() => match ctl {
                Some(Control::Send(text)) => {
                    let msg = match compress.as_mut() {
                        Some(c) => Message::Binary(zlib_sync_flush(c, text.as_bytes())),
                        None => Message::Text(text),
                    };
                    if ws.send(msg).await.is_err() {
                        return;
                    }
                }
                Some(Control::Close(Some(code))) => {
                    close(&mut ws, code).await;
                    return;
                }
                Some(Control::Close(None)) | None => return,
            },
        }
    }
}

/// One message on the connection's shared zlib context, ending in the `00 00 ff ff` sync-flush suffix.
fn zlib_sync_flush(c: &mut Compress, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() / 2 + 64);
    let start = c.total_in();
    loop {
        let done = usize::try_from(c.total_in() - start).unwrap_or(data.len());
        c.compress_vec(&data[done..], &mut out, FlushCompress::Sync)
            .expect("deflate into a growable buffer");
        let consumed = usize::try_from(c.total_in() - start).unwrap_or(data.len());
        if consumed == data.len() && out.len() < out.capacity() {
            return out;
        }
        out.reserve(out.capacity().max(1024));
    }
}

pub(crate) async fn close(ws: &mut WebSocketStream<TcpStream>, code: u16) {
    let _ = ws
        .close(Some(CloseFrame {
            code: CloseCode::from(code),
            reason: "".into(),
        }))
        .await;
}

fn on_text(shared: &Shared, id: u64, text: &str) -> Result<(), u16> {
    let msg: Value = serde_json::from_str(text).map_err(|_| CLOSE_DECODE_ERROR)?;
    let op = msg["op"]
        .as_u64()
        .and_then(|o| u8::try_from(o).ok())
        .ok_or(CLOSE_DECODE_ERROR)?;
    let d = msg["d"].clone();
    let mut state = shared.state();
    let Some((tx, identified)) = state
        .gateway
        .as_ref()
        .filter(|(c, _)| c.id == id)
        .map(|(c, bound)| (c.tx.clone(), bound.is_some()))
    else {
        return Ok(());
    };
    state.recorded.ops.push(ReceivedOp {
        socket: Socket::Gateway,
        op,
        payload: OpPayload::Json(d.clone()),
        received_at: std::time::Instant::now(),
    });
    shared.notify();
    match op {
        OP_HEARTBEAT => {
            let _ = tx.send(Control::Send(
                json!({ "op": OP_HEARTBEAT_ACK, "d": null, "s": null, "t": null }).to_string(),
            ));
        }
        OP_IDENTIFY => {
            if identified {
                return Err(CLOSE_ALREADY_AUTHENTICATED);
            }
            if d["token"].as_str() != Some(&shared.config.token) {
                return Err(CLOSE_AUTHENTICATION_FAILED);
            }
            identify(shared, &mut state, id);
        }
        OP_RESUME => {
            if identified {
                return Err(CLOSE_ALREADY_AUTHENTICATED);
            }
            if d["token"].as_str() != Some(&shared.config.token) {
                return Err(CLOSE_AUTHENTICATION_FAILED);
            }
            resume(&mut state, id, &d);
        }
        _ if !identified => return Err(CLOSE_NOT_AUTHENTICATED),
        OP_VOICE_STATE_UPDATE => voice_state_update(shared, &mut state, &d),
        OP_STREAM_CREATE => stream_create(shared, &mut state, &d),
        OP_STREAM_DELETE => stream_delete(shared, &mut state, &d),
        OP_STREAM_WATCH => stream_watch(shared, &mut state, &d),
        OP_STREAM_SET_PAUSED => stream_set_paused(shared, &mut state, &d),
        _ => debug!("mock gateway: unhandled op {op}"),
    }
    drop(state);
    shared.notify();
    Ok(())
}

fn identify(shared: &Shared, state: &mut State, id: u64) {
    let session_id = random_hex(16);
    state.sessions.insert(
        session_id.clone(),
        GatewaySession {
            seq: 0,
            buffer: Vec::new(),
        },
    );
    if let Some(old) = state.session.replace(session_id.clone()) {
        state.sessions.remove(&old);
    }
    if let Some((_, bound)) = state.gateway.as_mut().filter(|(c, _)| c.id == id) {
        *bound = Some(session_id.clone());
    }
    let ready = ready_payload(&shared.config, &session_id, &shared.gateway_url);
    shared.dispatch(state, "READY", ready);
    for guild in shared.config.guilds.iter().filter(|g| !g.in_ready) {
        shared.dispatch(state, "GUILD_CREATE", guild_payload(&shared.config, guild));
    }
}

fn resume(state: &mut State, id: u64, d: &Value) {
    let session_id = d["session_id"].as_str().unwrap_or_default().to_owned();
    let Some((conn, bound)) = state.gateway.as_mut().filter(|(c, _)| c.id == id) else {
        return;
    };
    let tx = conn.tx.clone();
    let live = state.session.as_deref() == Some(session_id.as_str());
    let Some(session) = state.sessions.get_mut(&session_id).filter(|_| live) else {
        let _ = tx.send(Control::Send(
            json!({ "op": OP_INVALID_SESSION, "d": false, "s": null, "t": null }).to_string(),
        ));
        return;
    };
    *bound = Some(session_id);
    let from = d["seq"].as_u64().unwrap_or(0);
    for (_, payload) in session.buffer.iter().filter(|(s, _)| *s > from) {
        let _ = tx.send(Control::Send(payload.clone()));
    }
    session.seq += 1;
    let resumed = json!({ "op": OP_DISPATCH, "t": "RESUMED", "s": session.seq, "d": { "_trace": ["mock-gateway"] } })
        .to_string();
    let _ = tx.send(Control::Send(resumed));
}

pub(crate) fn voice_state_update(shared: &Shared, state: &mut State, d: &Value) {
    if state.defer_join && d["channel_id"].is_string() {
        state.defer_join = false;
        state.deferred_join = Some(d.clone());
        return;
    }
    let user_id = shared.config.user.id.clone();
    let session_id = state.session.clone().unwrap_or_default();
    let (Some(guild_id), Some(channel_id)) = (d["guild_id"].as_str(), d["channel_id"].as_str())
    else {
        leave_voice(shared, state);
        return;
    };
    if shared.config.voice_channel(guild_id, channel_id).is_none() {
        debug!("mock gateway: op 4 for unknown voice channel {guild_id}/{channel_id}");
        return;
    }
    for payload in std::mem::take(&mut state.before_join_answer) {
        shared.dispatch(state, "VOICE_STATE_UPDATE", payload);
    }
    let self_stream = state
        .voice
        .as_ref()
        .is_some_and(|v| v.channel_id == channel_id && v.self_stream);
    let voice = VoiceStateSnapshot {
        guild_id: guild_id.to_owned(),
        channel_id: channel_id.to_owned(),
        session_id,
        self_mute: d["self_mute"].as_bool().unwrap_or(false),
        self_deaf: d["self_deaf"].as_bool().unwrap_or(false),
        self_video: d["self_video"].as_bool().unwrap_or(false),
        self_stream,
    };
    let payload = voice.dispatch(&user_id);
    state.voice = Some(voice);
    shared.dispatch(state, "VOICE_STATE_UPDATE", payload);
    if let Some((guild_id, channel_id)) = state.move_during_join.take() {
        move_to_channel(shared, state, &guild_id, &channel_id);
    }

    let token = random_hex(8);
    state.voice_auth.insert(
        token.clone(),
        VoiceAuth {
            server_id: guild_id.to_owned(),
            kind: VoiceKind::Voice,
            dave_channel_id: channel_id.parse().unwrap_or(0),
        },
    );
    shared.dispatch(
        state,
        "VOICE_SERVER_UPDATE",
        json!({ "token": token, "guild_id": guild_id, "endpoint": shared.voice_endpoint }),
    );
}

fn leave_voice(shared: &Shared, state: &mut State) {
    let Some(mut voice) = state.voice.take() else {
        return;
    };
    let keys: Vec<String> = state.streams.keys().cloned().collect();
    for key in keys {
        end_stream(shared, state, &key);
    }
    let server_ids: Vec<String> = state
        .voice_auth
        .values()
        .map(|a| a.server_id.clone())
        .collect();
    for server_id in server_ids {
        disconnect_voice(state, &server_id);
    }
    state.voice_auth.clear();
    voice.self_stream = false;
    let mut payload = voice.dispatch(&shared.config.user.id);
    payload["channel_id"] = Value::Null;
    shared.dispatch(state, "VOICE_STATE_UPDATE", payload);
}

/// Server-initiated move of the user to another voice channel (someone dragged them).
pub(crate) fn move_to_channel(
    shared: &Shared,
    state: &mut State,
    guild_id: &str,
    channel_id: &str,
) -> bool {
    if shared.config.voice_channel(guild_id, channel_id).is_none() {
        return false;
    }
    let Some(voice) = state.voice.as_mut() else {
        return false;
    };
    voice.guild_id = guild_id.to_owned();
    voice.channel_id = channel_id.to_owned();
    let payload = voice.dispatch(&shared.config.user.id);
    shared.dispatch(state, "VOICE_STATE_UPDATE", payload);
    true
}

fn stream_key(guild_id: &str, channel_id: &str, user_id: &str) -> String {
    format!("guild:{guild_id}:{channel_id}:{user_id}")
}

fn stream_create(shared: &Shared, state: &mut State, d: &Value) {
    if state.close_voice_on_stream_create {
        for (kind, conn) in state.voice_sockets.values() {
            if *kind == VoiceKind::Voice {
                let _ = conn.tx.send(Control::Close(Some(
                    crate::voice::CLOSE_SESSION_NO_LONGER_VALID,
                )));
            }
        }
        return;
    }
    if state.ignore_stream_creates > 0 {
        state.ignore_stream_creates -= 1;
        return;
    }
    let (Some(guild_id), Some(channel_id)) = (d["guild_id"].as_str(), d["channel_id"].as_str())
    else {
        return;
    };
    let in_channel = state
        .voice
        .as_ref()
        .is_some_and(|v| v.guild_id == guild_id && v.channel_id == channel_id);
    if !in_channel {
        debug!("mock gateway: STREAM_CREATE outside the joined voice channel");
        return;
    }
    let key = stream_key(guild_id, channel_id, &shared.config.user.id);
    announce_stream(shared, state, &key);
    if let Some(voice) = state.voice.as_mut() {
        voice.self_stream = true;
        let payload = voice.dispatch(&shared.config.user.id);
        shared.dispatch(state, "VOICE_STATE_UPDATE", payload);
    }
}

fn announce_stream(shared: &Shared, state: &mut State, key: &str) {
    if let Some(old) = state.streams.get(key).map(|s| s.rtc_server_id.clone()) {
        state.voice_auth.retain(|_, a| a.server_id != old);
        disconnect_voice(state, &old);
    }
    let rtc_server_id = random_snowflake();
    let token = random_hex(8);
    let dave_channel_id = rtc_server_id.parse::<u64>().unwrap_or(1) - 1;
    state.voice_auth.insert(
        token.clone(),
        VoiceAuth {
            server_id: rtc_server_id.clone(),
            kind: VoiceKind::Stream,
            dave_channel_id,
        },
    );
    state.streams.insert(
        key.to_owned(),
        StreamSnapshot {
            stream_key: key.to_owned(),
            rtc_server_id: rtc_server_id.clone(),
            paused: false,
        },
    );
    shared.dispatch(
        state,
        "STREAM_CREATE",
        json!({
            "stream_key": key,
            "rtc_server_id": rtc_server_id,
            "region": "mock",
            "viewer_ids": [],
            "paused": false,
        }),
    );
    shared.dispatch(
        state,
        "STREAM_SERVER_UPDATE",
        json!({ "stream_key": key, "endpoint": shared.voice_endpoint, "token": token }),
    );
}

fn disconnect_voice(state: &mut State, server_id: &str) {
    if let Some((_, conn)) = state.voice_sockets.get(server_id) {
        let _ = conn
            .tx
            .send(Control::Close(Some(crate::voice::CLOSE_DISCONNECTED)));
    }
    if let Some(conn) = state.voice_conns.remove(server_id) {
        tokio::spawn(async move { conn.lock().await.close_peer().await });
    }
}

fn end_stream(shared: &Shared, state: &mut State, key: &str) {
    let Some(stream) = state.streams.remove(key) else {
        return;
    };
    state
        .voice_auth
        .retain(|_, a| a.server_id != stream.rtc_server_id);
    disconnect_voice(state, &stream.rtc_server_id);
    shared.dispatch(
        state,
        "STREAM_DELETE",
        json!({ "stream_key": key, "unavailable": false, "reason": "user_requested" }),
    );
}

fn stream_delete(shared: &Shared, state: &mut State, d: &Value) {
    let Some(key) = d["stream_key"].as_str() else {
        return;
    };
    let own = state.streams.contains_key(key);
    end_stream(shared, state, key);
    if own && let Some(voice) = state.voice.as_mut() {
        voice.self_stream = false;
        let payload = voice.dispatch(&shared.config.user.id);
        shared.dispatch(state, "VOICE_STATE_UPDATE", payload);
    }
}

fn stream_watch(shared: &Shared, state: &mut State, d: &Value) {
    let Some(key) = d["stream_key"].as_str() else {
        return;
    };
    let parts: Vec<&str> = key.split(':').collect();
    if let ["guild", guild_id, channel_id, _user] = parts.as_slice()
        && shared.config.voice_channel(guild_id, channel_id).is_some()
    {
        announce_stream(shared, state, key);
    }
}

fn stream_set_paused(shared: &Shared, state: &mut State, d: &Value) {
    let Some(key) = d["stream_key"].as_str() else {
        return;
    };
    let paused = d["paused"].as_bool().unwrap_or(false);
    let Some(stream) = state.streams.get_mut(key) else {
        return;
    };
    stream.paused = paused;
    shared.dispatch(
        state,
        "STREAM_UPDATE",
        json!({ "stream_key": key, "region": "mock", "viewer_ids": [], "paused": paused }),
    );
}

/// Send op 9; a non-resumable invalidation forgets the session so only IDENTIFY works.
pub(crate) fn invalidate_session(state: &mut State, resumable: bool) -> bool {
    let Some((conn, bound)) = state.gateway.as_mut() else {
        return false;
    };
    let _ = conn.tx.send(Control::Send(
        json!({ "op": OP_INVALID_SESSION, "d": resumable, "s": null, "t": null }).to_string(),
    ));
    if !resumable {
        *bound = None;
        if let Some(old) = state.session.take() {
            state.sessions.remove(&old);
        }
    }
    true
}

pub(crate) fn ready_payload(config: &MockConfig, session_id: &str, gateway_url: &str) -> Value {
    let guilds: Vec<Value> = config
        .guilds
        .iter()
        .map(|g| {
            if g.in_ready {
                guild_payload(config, g)
            } else {
                json!({ "id": g.id, "unavailable": true })
            }
        })
        .collect();
    json!({
        "v": 9,
        "user": user_payload(config),
        "session_id": session_id,
        "session_type": "normal",
        "resume_gateway_url": gateway_url,
        "auth_session_id_hash": random_hex(16),
        "guilds": guilds,
        "private_channels": [],
        "relationships": [],
        "users": [],
        "presences": [],
        "sessions": [],
        "notes": {},
        "user_settings": {},
        "user_guild_settings": [],
        "read_state": [],
        "connected_accounts": [],
        "experiments": [],
        "guild_experiments": [],
        "country_code": "US",
        "analytics_token": "",
        "required_action": null,
        "tutorial": null,
        "_trace": ["mock-gateway"],
    })
}

fn user_payload(config: &MockConfig) -> Value {
    json!({
        "id": config.user.id,
        "username": config.user.username,
        "global_name": config.user.global_name,
        "discriminator": "0",
        "avatar": null,
        "bot": false,
        "verified": true,
        "mfa_enabled": false,
        "email": null,
        "flags": 0,
        "premium_type": 0,
    })
}

pub(crate) fn guild_payload(config: &MockConfig, guild: &MockGuild) -> Value {
    let channels: Vec<Value> = guild
        .channels
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut ch = json!({
                "id": c.id,
                "type": c.kind as u8,
                "name": c.name,
                "guild_id": guild.id,
                "position": i,
                "parent_id": null,
                "permission_overwrites": [],
                "nsfw": false,
                "flags": 0,
            });
            if c.is_voice() {
                ch["bitrate"] = json!(64_000);
                ch["user_limit"] = json!(0);
                ch["rtc_region"] = Value::Null;
            }
            ch
        })
        .collect();
    json!({
        "id": guild.id,
        "name": guild.name,
        "icon": null,
        "owner_id": config.user.id,
        "unavailable": false,
        "large": false,
        "member_count": 1,
        "joined_at": "2024-01-01T00:00:00.000000+00:00",
        "features": [],
        "channels": channels,
        "threads": [],
        "roles": [{ "id": guild.id, "name": "@everyone", "permissions": "1071698660929", "position": 0, "color": 0, "hoist": false, "managed": false, "mentionable": false }],
        "emojis": [],
        "stickers": [],
        "members": [{ "user": user_payload(config), "roles": [], "joined_at": "2024-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false }],
        "voice_states": [],
        "presences": [],
        "stage_instances": [],
        "guild_scheduled_events": [],
        "premium_tier": 0,
        "preferred_locale": "en-US",
        "afk_channel_id": null,
        "afk_timeout": 300,
        "system_channel_id": null,
        "verification_level": 0,
        "default_message_notifications": 0,
        "explicit_content_filter": 0,
        "mfa_level": 0,
        "nsfw_level": 0,
        "lazy": true,
    })
}
