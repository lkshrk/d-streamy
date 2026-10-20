#![allow(dead_code)]

use discord_stream_rs::{
    GatewayEvent, GatewayPayload, MediaConnection, PendingConnection, Streamer,
};
use dstreamy_mock_discord::{MOCK_TOKEN, MockDiscord, rtc};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::future::Future;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

pub const T: Duration = Duration::from_secs(15);

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Dispatch(String, Value),
    Op(u64, Value),
    Closed(Option<u16>),
}

/// A minimal scripted main-gateway client: HELLO, IDENTIFY/RESUME, sequence tracking.
pub struct GatewayDriver {
    ws: Ws,
    pub seq: u64,
    pub session_id: Option<String>,
    pub hello: Value,
}

impl GatewayDriver {
    pub async fn connect(url: &str) -> Self {
        let (mut ws, _) = timeout(T, connect_async(format!("{url}/?v=9&encoding=json")))
            .await
            .unwrap()
            .unwrap();
        let hello = match timeout(T, ws.next()).await.unwrap() {
            Some(Ok(Message::Text(t))) => serde_json::from_str::<Value>(&t).unwrap(),
            other => panic!("expected HELLO, got {other:?}"),
        };
        assert_eq!(hello["op"], 10);
        Self {
            ws,
            seq: 0,
            session_id: None,
            hello,
        }
    }

    /// Connect, IDENTIFY and return the READY payload.
    pub async fn identify(url: &str, token: &str) -> (Self, Value) {
        let mut driver = Self::connect(url).await;
        let ready = driver.send_identify(token).await;
        (driver, ready)
    }

    pub async fn send_identify(&mut self, token: &str) -> Value {
        self.send(
            2,
            json!({
                "token": token,
                "capabilities": 0,
                "properties": { "os": "Mac OS X", "browser": "Discord Client", "device": "" },
                "presence": { "status": "online", "since": 0, "activities": [], "afk": false },
                "compress": false,
                "client_state": { "guild_versions": {} },
            }),
        )
        .await;
        let ready = self.dispatch("READY").await;
        self.session_id = ready["session_id"].as_str().map(str::to_owned);
        ready
    }

    pub async fn send_resume(&mut self, token: &str) {
        let (session_id, seq) = (self.session_id.clone(), self.seq);
        self.send(
            6,
            json!({ "token": token, "session_id": session_id, "seq": seq }),
        )
        .await;
    }

    pub async fn send(&mut self, op: u8, d: Value) {
        self.ws
            .send(Message::Text(json!({ "op": op, "d": d }).to_string()))
            .await
            .unwrap();
    }

    pub async fn next(&mut self) -> Frame {
        loop {
            match timeout(T, self.ws.next()).await.expect("gateway frame") {
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    if let Some(s) = v["s"].as_u64() {
                        self.seq = self.seq.max(s);
                    }
                    return match v["op"].as_u64() {
                        Some(0) => {
                            Frame::Dispatch(v["t"].as_str().unwrap().to_owned(), v["d"].clone())
                        }
                        Some(op) => Frame::Op(op, v["d"].clone()),
                        None => panic!("no op in {v}"),
                    };
                }
                Some(Ok(Message::Close(frame))) => {
                    return Frame::Closed(frame.map(|f| u16::from(f.code)));
                }
                None | Some(Err(_)) => return Frame::Closed(None),
                Some(Ok(_)) => {}
            }
        }
    }

    /// Skip frames until dispatch `t`.
    pub async fn dispatch(&mut self, t: &str) -> Value {
        loop {
            match self.next().await {
                Frame::Dispatch(name, d) if name == t => return d,
                Frame::Closed(code) => panic!("gateway closed ({code:?}) waiting for {t}"),
                _ => {}
            }
        }
    }

    /// Runs until the socket closes or `outbound` is dropped.
    pub fn pump(
        mut self,
        mut outbound: mpsc::UnboundedReceiver<GatewayPayload>,
    ) -> mpsc::UnboundedReceiver<GatewayEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    msg = self.ws.next() => match msg {
                        Some(Ok(Message::Text(t))) => {
                            let v: Value = serde_json::from_str(&t).unwrap();
                            if v["op"] == 0 {
                                let name = v["t"].as_str().unwrap_or_default().to_owned();
                                let _ = tx.send(GatewayEvent::from_dispatch(&name, v["d"].clone()));
                            }
                        }
                        Some(Ok(_)) => {}
                        _ => return,
                    },
                    payload = outbound.recv() => match payload {
                        Some(p) => self.send(p.op, p.d).await,
                        None => return,
                    },
                }
            }
        });
        rx
    }
}

/// A Streamer wired to the mock through a scripted gateway connection.
pub struct Client {
    pub streamer: Streamer,
    pub events: mpsc::UnboundedReceiver<GatewayEvent>,
    pub ready: Value,
}

impl Client {
    pub async fn connect(mock: &MockDiscord) -> Self {
        let (driver, ready) = GatewayDriver::identify(&mock.gateway_url(), MOCK_TOKEN).await;
        let (gw_tx, gw_rx) = mpsc::unbounded_channel();
        let user_id = ready["user"]["id"].as_str().unwrap().to_owned();
        let mut streamer = Streamer::new(user_id, gw_tx);
        streamer.set_ice_servers(vec![]);
        streamer.set_setting_engine(rtc::loopback_settings(false));
        let events = driver.pump(gw_rx);
        Self {
            streamer,
            events,
            ready,
        }
    }

    /// Drive `fut` while feeding gateway dispatches to the Streamer.
    pub async fn pump_until<F: Future>(&mut self, fut: F) -> F::Output {
        tokio::pin!(fut);
        timeout(T, async {
            loop {
                tokio::select! {
                    out = &mut fut => return out,
                    Some(ev) = self.events.recv() => self.streamer.handle_event(ev),
                }
            }
        })
        .await
        .expect("timed out")
    }

    pub fn drain_events(&mut self) -> Vec<GatewayEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.events.try_recv() {
            out.push(ev.clone());
            self.streamer.handle_event(ev);
        }
        out
    }

    pub async fn ready(&mut self, pending: &mut PendingConnection) -> MediaConnection {
        let conn = self
            .pump_until(pending.ready())
            .await
            .expect("connection ready");
        wait_connected(&conn).await;
        conn
    }
}

pub async fn wait_connected(conn: &MediaConnection) {
    timeout(T, async {
        while !(conn.is_ready().await && conn.dave_ready()) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("peer connection and DAVE ready");
}
