//! Voice gateway (v8) for voice and Go Live connections, with the WebRTC answerer and DAVE peer.

use std::num::NonZeroU16;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{WebSocketStream, accept_async};
use tracing::{debug, warn};
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtp::packet::Packet;

use crate::dave::DaveServer;
use crate::depacketize::VideoCodec;
use crate::gateway::close;
use crate::rtc;
use crate::sink::{Negotiated, SharedDave, Sink, lock_dave};
use crate::state::{Connection, Control, OpPayload, ReceivedOp, Shared, Socket, VoiceKind};
use crate::util::random_hex;

pub const OP_IDENTIFY: u8 = 0;
pub const OP_SELECT_PROTOCOL: u8 = 1;
pub const OP_READY: u8 = 2;
pub const OP_HEARTBEAT: u8 = 3;
pub const OP_SELECT_PROTOCOL_ACK: u8 = 4;
pub const OP_SPEAKING: u8 = 5;
pub const OP_HEARTBEAT_ACK: u8 = 6;
pub const OP_RESUME: u8 = 7;
pub const OP_HELLO: u8 = 8;
pub const OP_RESUMED: u8 = 9;
pub const OP_CLIENTS_CONNECT: u8 = 11;
pub const OP_VIDEO: u8 = 12;
pub const OP_DAVE_EXECUTE_TRANSITION: u8 = 22;
pub const OP_DAVE_TRANSITION_READY: u8 = 23;
pub const OP_MLS_EXTERNAL_SENDER: u8 = 25;
pub const OP_MLS_KEY_PACKAGE: u8 = 26;
pub const OP_MLS_PROPOSALS: u8 = 27;
pub const OP_MLS_COMMIT_WELCOME: u8 = 28;
pub const OP_MLS_ANNOUNCE_COMMIT_TRANSITION: u8 = 29;
pub const OP_MLS_INVALID_COMMIT_WELCOME: u8 = 31;

pub const CLOSE_DECODE_ERROR: u16 = 4002;
pub const CLOSE_NOT_AUTHENTICATED: u16 = 4003;
pub const CLOSE_AUTHENTICATION_FAILED: u16 = 4004;
pub const CLOSE_SESSION_NO_LONGER_VALID: u16 = 4006;
pub const CLOSE_SESSION_TIMEOUT: u16 = 4009;
pub const CLOSE_UNKNOWN_PROTOCOL: u16 = 4012;
pub const CLOSE_DISCONNECTED: u16 = 4014;
pub const CLOSE_SERVER_CRASHED: u16 = 4015;
pub const CLOSE_BAD_REQUEST: u16 = 4020;

const ENCRYPTION_MODES: [&str; 2] = ["aead_aes256_gcm_rtpsize", "aead_xchacha20_poly1305_rtpsize"];

pub(crate) struct VoiceConn {
    pub kind: VoiceKind,
    pub server_id: String,
    pub session_id: String,
    pub token: String,
    pub user_id: u64,
    pub dave_channel_id: u64,
    pub dave_version: u16,
    pub seq: u16,
    pub audio_ssrc: u32,
    pub video_ssrc: u32,
    pub dave: SharedDave,
    peer: Option<(Arc<RTCPeerConnection>, JoinHandle<()>)>,
}

impl VoiceConn {
    pub fn dave_active(&self) -> bool {
        lock_dave(&self.dave)
            .as_ref()
            .is_some_and(DaveServer::is_active)
    }

    pub fn privacy_code(&self) -> Option<String> {
        lock_dave(&self.dave)
            .as_ref()
            .and_then(DaveServer::privacy_code)
    }

    pub(crate) async fn send_pli(&self) -> bool {
        let Some((pc, _)) = &self.peer else {
            return false;
        };
        let pli = PictureLossIndication {
            sender_ssrc: 0,
            media_ssrc: self.video_ssrc,
        };
        pc.write_rtcp(&[Box::new(pli)]).await.is_ok()
    }

    pub(crate) async fn close_peer(&mut self) {
        if let Some((pc, sink)) = self.peer.take() {
            let _ = pc.close().await;
            sink.abort();
        }
    }
}

type Ws = WebSocketStream<TcpStream>;
type SharedConn = Arc<tokio::sync::Mutex<VoiceConn>>;

pub(crate) async fn serve(shared: Arc<Shared>, listener: TcpListener) {
    while let Ok((tcp, _)) = listener.accept().await {
        tokio::spawn(handle(shared.clone(), tcp));
    }
}

pub(crate) async fn close_all(shared: &Shared) {
    let conns: Vec<SharedConn> = shared.state().voice_conns.drain().map(|(_, c)| c).collect();
    for conn in conns {
        conn.lock().await.close_peer().await;
    }
}

async fn handle(shared: Arc<Shared>, tcp: TcpStream) {
    let Ok(mut ws) = accept_async(tcp).await else {
        return;
    };
    let hello = json!({
        "op": OP_HELLO,
        "d": { "heartbeat_interval": shared.config.voice_heartbeat_interval_ms, "v": 8 },
    });
    if ws.send(Message::Text(hello.to_string())).await.is_err() {
        return;
    }
    let conn = match open(&shared, &mut ws).await {
        Ok(conn) => conn,
        Err(code) => {
            close(&mut ws, code).await;
            return;
        }
    };
    let (kind, server_id) = {
        let c = conn.lock().await;
        (c.kind, c.server_id.clone())
    };
    let id = shared.next_id();
    let (tx, rx) = mpsc::unbounded_channel();
    let replaced = shared
        .state()
        .voice_sockets
        .insert(server_id.clone(), (kind, Connection { id, tx }));
    if let Some((_, old)) = replaced {
        let _ = old.tx.send(Control::Close(None));
    }
    shared.notify();
    let socket = match kind {
        VoiceKind::Voice => Socket::Voice(server_id.clone()),
        VoiceKind::Stream => Socket::Stream(server_id.clone()),
    };
    run(&shared, &conn, socket, ws, rx).await;
    let mut state = shared.state();
    if state
        .voice_sockets
        .get(&server_id)
        .is_some_and(|(_, c)| c.id == id)
    {
        state.voice_sockets.remove(&server_id);
    }
}

async fn open(shared: &Arc<Shared>, ws: &mut Ws) -> Result<SharedConn, u16> {
    let msg = loop {
        match ws.next().await {
            Some(Ok(Message::Text(text))) => break text,
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            _ => return Err(CLOSE_NOT_AUTHENTICATED),
        }
    };
    let msg: Value = serde_json::from_str(&msg).map_err(|_| CLOSE_NOT_AUTHENTICATED)?;
    let op = msg["op"].as_u64().unwrap_or(u64::MAX);
    let d = msg["d"].clone();
    let server_id = d["server_id"].as_str().unwrap_or_default().to_owned();
    let token = d["token"].as_str().unwrap_or_default();
    let session_id = d["session_id"].as_str().unwrap_or_default();
    let auth = shared.state().voice_auth.get(token).cloned();
    let socket = match auth.as_ref().map(|a| a.kind) {
        Some(VoiceKind::Stream) => Socket::Stream(server_id.clone()),
        _ => Socket::Voice(server_id.clone()),
    };
    if let Ok(op) = u8::try_from(op) {
        shared.record(|r| {
            r.ops.push(ReceivedOp {
                socket,
                op,
                payload: OpPayload::Json(d.clone()),
            })
        });
    }
    match u8::try_from(op) {
        Ok(OP_IDENTIFY) => {
            let auth = auth
                .filter(|a| a.server_id == server_id)
                .ok_or(CLOSE_AUTHENTICATION_FAILED)?;
            let valid_session = shared
                .state()
                .voice
                .as_ref()
                .is_some_and(|v| v.session_id == session_id);
            let user_id = d["user_id"].as_str().unwrap_or_default();
            if !valid_session || user_id != shared.config.user.id {
                return Err(CLOSE_AUTHENTICATION_FAILED);
            }
            let max_dave = d["max_dave_protocol_version"].as_u64().unwrap_or(0);
            let dave_version = u16::try_from(max_dave)
                .unwrap_or(u16::MAX)
                .min(shared.config.dave_protocol_version);
            let base = 10_000 + (shared.next_id() as u32 % 100_000) * 10;
            let mut conn = VoiceConn {
                kind: auth.kind,
                server_id: server_id.clone(),
                session_id: session_id.to_owned(),
                token: token.to_owned(),
                user_id: user_id.parse().unwrap_or(0),
                dave_channel_id: auth.dave_channel_id,
                dave_version,
                seq: 0,
                audio_ssrc: base,
                video_ssrc: base + 1,
                dave: Arc::new(Mutex::new(None)),
                peer: None,
            };
            let ready = json!({
                "ssrc": conn.audio_ssrc,
                "ip": "127.0.0.1",
                "port": shared.voice_port,
                "modes": ENCRYPTION_MODES,
                "streams": [{
                    "type": "video", "ssrc": conn.video_ssrc, "rtx_ssrc": conn.video_ssrc + 1,
                    "rid": "100", "quality": 100, "active": false,
                }],
                "experiments": [],
            });
            send_json(ws, &mut conn, OP_READY, ready).await;
            let conn = Arc::new(tokio::sync::Mutex::new(conn));
            let old = shared.state().voice_conns.insert(server_id, conn.clone());
            if let Some(old) = old {
                old.lock().await.close_peer().await;
            }
            Ok(conn)
        }
        Ok(OP_RESUME) => {
            let conn = shared
                .state()
                .voice_conns
                .get(&server_id)
                .cloned()
                .ok_or(CLOSE_SESSION_NO_LONGER_VALID)?;
            {
                let mut c = conn.lock().await;
                if c.session_id != session_id || c.token != token {
                    return Err(CLOSE_SESSION_NO_LONGER_VALID);
                }
                send_json(ws, &mut c, OP_RESUMED, Value::Null).await;
            }
            Ok(conn)
        }
        _ => Err(CLOSE_NOT_AUTHENTICATED),
    }
}

async fn run(
    shared: &Arc<Shared>,
    conn: &SharedConn,
    socket: Socket,
    mut ws: Ws,
    mut rx: mpsc::UnboundedReceiver<Control>,
) {
    let heartbeat_timeout = Duration::from_millis(shared.config.voice_heartbeat_interval_ms * 2);
    let mut heartbeat_deadline = tokio::time::Instant::now() + heartbeat_timeout;
    loop {
        tokio::select! {
            msg = ws.next() => match msg {
                Some(Ok(Message::Text(text))) => {
                    let parsed = serde_json::from_str::<Value>(&text).ok().and_then(|m| {
                        let op = m["op"].as_u64().and_then(|o| u8::try_from(o).ok())?;
                        Some((op, m))
                    });
                    let Some((op, msg)) = parsed else {
                        close(&mut ws, CLOSE_DECODE_ERROR).await;
                        return;
                    };
                    let d = msg["d"].clone();
                    shared.record(|r| r.ops.push(ReceivedOp {
                        socket: socket.clone(),
                        op,
                        payload: OpPayload::Json(d.clone()),
                    }));
                    if op == OP_HEARTBEAT {
                        heartbeat_deadline = tokio::time::Instant::now() + heartbeat_timeout;
                    }
                    if let Err(code) = on_text(shared, conn, &mut ws, op, &d).await {
                        close(&mut ws, code).await;
                        return;
                    }
                }
                Some(Ok(Message::Binary(data))) => {
                    let Some((&op, payload)) = data.split_first() else {
                        continue;
                    };
                    shared.record(|r| r.ops.push(ReceivedOp {
                        socket: socket.clone(),
                        op,
                        payload: OpPayload::Binary(payload.to_vec()),
                    }));
                    on_binary(conn, &mut ws, op, payload).await;
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => {}
            },
            ctl = rx.recv() => match ctl {
                Some(Control::Send(text)) => {
                    if ws.send(Message::Text(text)).await.is_err() {
                        return;
                    }
                }
                Some(Control::Close(Some(code))) => {
                    close(&mut ws, code).await;
                    return;
                }
                Some(Control::Close(None)) | None => return,
            },
            () = tokio::time::sleep_until(heartbeat_deadline) => {
                close(&mut ws, CLOSE_SESSION_TIMEOUT).await;
                return;
            }
        }
    }
}

async fn on_text(
    shared: &Arc<Shared>,
    conn: &SharedConn,
    ws: &mut Ws,
    op: u8,
    d: &Value,
) -> Result<(), u16> {
    let mut c = conn.lock().await;
    match op {
        OP_HEARTBEAT => {
            if !acks_sent_seq(c.seq, &d["seq_ack"]) {
                return Err(CLOSE_BAD_REQUEST);
            }
            send_json(ws, &mut c, OP_HEARTBEAT_ACK, json!({ "t": d["t"] })).await;
        }
        OP_SELECT_PROTOCOL => select_protocol(shared, &mut c, ws, d).await?,
        OP_SPEAKING => {
            if d["ssrc"].as_u64() != Some(u64::from(c.audio_ssrc)) {
                return Err(CLOSE_BAD_REQUEST);
            }
        }
        OP_VIDEO => check_video_ssrcs(&c, d)?,
        OP_DAVE_TRANSITION_READY => {
            if let Some(server) = lock_dave(&c.dave).as_mut() {
                server.execute_transition();
            }
            let transition_id = d["transition_id"].clone();
            send_json(
                ws,
                &mut c,
                OP_DAVE_EXECUTE_TRANSITION,
                json!({ "transition_id": transition_id }),
            )
            .await;
        }
        OP_MLS_INVALID_COMMIT_WELCOME => {}
        _ => debug!("mock voice: unhandled op {op}"),
    }
    drop(c);
    shared.notify();
    Ok(())
}

/// `-1` before any sequenced message, else a seq at or (wrapping) behind the last one sent.
fn acks_sent_seq(sent: u16, seq_ack: &Value) -> bool {
    match seq_ack.as_i64() {
        Some(-1) => true,
        Some(acked) => u16::try_from(acked).is_ok_and(|a| sent.wrapping_sub(a) < 0x8000),
        None => false,
    }
}

fn check_video_ssrcs(c: &VoiceConn, d: &Value) -> Result<(), u16> {
    let is = |v: &Value, ssrc: u32| v.as_u64() == Some(u64::from(ssrc));
    let rtx_ssrc = c.video_ssrc + 1;
    let streams_ok = d["streams"].as_array().is_some_and(|streams| {
        streams
            .iter()
            .all(|s| is(&s["ssrc"], c.video_ssrc) && is(&s["rtx_ssrc"], rtx_ssrc))
    });
    let ok = is(&d["audio_ssrc"], c.audio_ssrc)
        && (is(&d["video_ssrc"], 0) || is(&d["video_ssrc"], c.video_ssrc))
        && (is(&d["rtx_ssrc"], 0) || is(&d["rtx_ssrc"], rtx_ssrc))
        && streams_ok;
    if ok { Ok(()) } else { Err(CLOSE_BAD_REQUEST) }
}

fn announced_payload_type(d: &Value, kind: &str, name: &str) -> Option<u8> {
    d["codecs"]
        .as_array()?
        .iter()
        .find(|c| {
            c["type"] == kind
                && c["name"]
                    .as_str()
                    .is_some_and(|n| n.eq_ignore_ascii_case(name))
        })
        .and_then(|c| c["payload_type"].as_u64())
        .and_then(|pt| u8::try_from(pt).ok())
}

async fn select_protocol(
    shared: &Arc<Shared>,
    c: &mut VoiceConn,
    ws: &mut Ws,
    d: &Value,
) -> Result<(), u16> {
    if d["protocol"] != "webrtc" {
        return Err(CLOSE_UNKNOWN_PROTOCOL);
    }
    let video_codec = shared.config.video_codec;
    let video_name = match video_codec {
        VideoCodec::H264 => "H264",
        VideoCodec::H265 => "H265",
    };
    let (Some(audio_payload_type), Some(video_payload_type)) = (
        announced_payload_type(d, "audio", "opus"),
        announced_payload_type(d, "video", video_name),
    ) else {
        return Err(CLOSE_BAD_REQUEST);
    };
    let offer = d["sdp"]
        .as_str()
        .or_else(|| d["data"].as_str())
        .unwrap_or_default()
        .to_owned();
    c.close_peer().await;
    let answered = answer(video_codec, audio_payload_type, video_payload_type, offer).await;
    let (pc, rtp_rx, sdp) = match answered {
        Ok(answered) => answered,
        Err(e) => {
            warn!("mock voice: SELECT_PROTOCOL failed: {e}");
            return Err(CLOSE_BAD_REQUEST);
        }
    };
    *lock_dave(&c.dave) = None;
    let negotiated = Negotiated {
        audio_ssrc: c.audio_ssrc,
        video_ssrc: c.video_ssrc,
        audio_payload_type,
        video_payload_type,
        video_codec,
    };
    let sink = Sink::spawn(
        shared.clone(),
        c.kind,
        c.server_id.clone(),
        c.dave.clone(),
        negotiated,
        rtp_rx,
    );
    c.peer = Some((pc, sink));
    let ack = json!({
        "audio_codec": "opus",
        "video_codec": video_name,
        "media_session_id": random_hex(16),
        "sdp": sdp,
        "dave_protocol_version": c.dave_version,
    });
    send_json(ws, c, OP_SELECT_PROTOCOL_ACK, ack).await;
    if let Some(version) = NonZeroU16::new(c.dave_version) {
        match DaveServer::new(
            version,
            c.user_id,
            c.dave_channel_id,
            shared.config.dave_participant_id,
        ) {
            Ok(server) => {
                let package = server.external_sender_package().to_vec();
                *lock_dave(&c.dave) = Some(server);
                send_binary(ws, c, OP_MLS_EXTERNAL_SENDER, &package).await;
            }
            Err(e) => warn!("mock voice: DAVE setup failed: {e}"),
        }
    }
    Ok(())
}

async fn answer(
    video_codec: VideoCodec,
    audio_payload_type: u8,
    video_payload_type: u8,
    offer: String,
) -> Result<
    (
        Arc<RTCPeerConnection>,
        mpsc::UnboundedReceiver<Packet>,
        String,
    ),
    rtc::RtcError,
> {
    let (pc, rtp_rx) =
        rtc::answerer_peer(video_codec, audio_payload_type, video_payload_type).await?;
    match rtc::answer_like_discord(&pc, offer).await {
        Ok(sdp) => Ok((pc, rtp_rx, sdp)),
        Err(e) => {
            let _ = pc.close().await;
            Err(e)
        }
    }
}

async fn on_binary(conn: &SharedConn, ws: &mut Ws, op: u8, payload: &[u8]) {
    let mut c = conn.lock().await;
    match op {
        OP_MLS_KEY_PACKAGE => {
            let result = lock_dave(&c.dave)
                .as_mut()
                .map(|s| (s.participant_id(), s.on_key_package(payload)));
            match result {
                Some((participant, Ok(proposals))) => {
                    let connect = json!({ "user_ids": [participant.to_string()] });
                    send_json(ws, &mut c, OP_CLIENTS_CONNECT, connect).await;
                    send_binary(ws, &mut c, OP_MLS_PROPOSALS, &proposals).await;
                }
                Some((_, Err(e))) => warn!("mock voice: bad key package: {e}"),
                None => debug!("mock voice: key package without DAVE"),
            }
        }
        OP_MLS_COMMIT_WELCOME => {
            let result = lock_dave(&c.dave)
                .as_mut()
                .map(|s| s.on_commit_welcome(payload));
            match result {
                Some(Ok((_, announce))) => {
                    send_binary(ws, &mut c, OP_MLS_ANNOUNCE_COMMIT_TRANSITION, &announce).await;
                }
                Some(Err(e)) => warn!("mock voice: bad commit/welcome: {e}"),
                None => debug!("mock voice: commit without DAVE"),
            }
        }
        _ => debug!("mock voice: unhandled binary op {op}"),
    }
}

async fn send_json(ws: &mut Ws, c: &mut VoiceConn, op: u8, d: Value) {
    c.seq = c.seq.wrapping_add(1);
    let msg = json!({ "op": op, "seq": c.seq, "d": d });
    let _ = ws.send(Message::Text(msg.to_string())).await;
}

async fn send_binary(ws: &mut Ws, c: &mut VoiceConn, op: u8, payload: &[u8]) {
    c.seq = c.seq.wrapping_add(1);
    let mut frame = c.seq.to_be_bytes().to_vec();
    frame.push(op);
    frame.extend_from_slice(payload);
    let _ = ws.send(Message::Binary(frame)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_ack_may_trail_the_sent_seq_across_the_wrap() {
        assert!(acks_sent_seq(3, &json!(65_530)));
        assert!(acks_sent_seq(3, &json!(3)));
        assert!(acks_sent_seq(3, &json!(-1)));
        assert!(!acks_sent_seq(3, &json!(4)));
        assert!(!acks_sent_seq(3, &json!(70_000)));
    }
}
