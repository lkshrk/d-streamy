#![allow(dead_code, clippy::result_large_err)]

use std::time::Duration;

use dstreamy_discord::GatewayOptions;
use flate2::{Compress, Compression, FlushCompress};
use futures_util::{SinkExt, StreamExt};
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

pub const TOKEN: &str = "test-token-not-a-real-one";
pub const USER_ID: &str = "100000000000000001";
pub const SESSION_ID: &str = "6f1c0b1a2d3e4f5a6b7c8d9e0f1a2b3c";
pub const STEP: Duration = Duration::from_secs(5);

pub fn options(url: &str) -> GatewayOptions {
    let mut opts = GatewayOptions::new(SecretString::from(TOKEN)).with_url(url);
    opts.compress = false;
    opts.reconnect_delay_min = Duration::from_millis(10);
    opts.reconnect_delay_max = Duration::from_millis(50);
    opts.invalid_session_delay = Duration::from_millis(50);
    opts
}

pub fn ready_fixture(resume_url: &str) -> Value {
    let text = include_str!("../fixtures/ready.json").replace("RESUME_URL", resume_url);
    serde_json::from_str(&text).unwrap()
}

/// Loopback gateway: one listener, one `Conn` per accepted socket.
pub struct Server {
    listener: TcpListener,
    pub url: String,
}

impl Server {
    pub async fn bind() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        Self { listener, url }
    }

    pub async fn accept(&self) -> Conn {
        let (tcp, _) = timeout(STEP, self.listener.accept())
            .await
            .expect("no connection")
            .unwrap();
        let mut path = String::new();
        let ws = tokio_tungstenite::accept_hdr_async(tcp, |req: &Request, resp: Response| {
            path = req.uri().to_string();
            Ok(resp)
        })
        .await
        .unwrap();
        Conn {
            ws,
            path,
            compress: None,
            auto_ack: true,
            heartbeats: Vec::new(),
        }
    }

    pub async fn no_connection_within(&self, wait: Duration) -> bool {
        timeout(wait, self.listener.accept()).await.is_err()
    }
}

pub struct Conn {
    ws: WebSocketStream<TcpStream>,
    pub path: String,
    compress: Option<Compress>,
    pub auto_ack: bool,
    pub heartbeats: Vec<Value>,
}

impl Conn {
    pub fn enable_zlib(&mut self) {
        self.compress = Some(Compress::new(Compression::default(), true));
    }

    pub async fn send(&mut self, packet: Value) {
        let text = packet.to_string();
        let msg = match self.compress.as_mut() {
            Some(c) => Message::Binary(zlib(c, text.as_bytes())),
            None => Message::Text(text),
        };
        self.ws.send(msg).await.unwrap();
    }

    pub async fn send_raw(&mut self, msg: Message) {
        self.ws.send(msg).await.unwrap();
    }

    pub fn zlib_frame(&mut self, packet: &Value) -> Vec<u8> {
        zlib(
            self.compress.as_mut().unwrap(),
            packet.to_string().as_bytes(),
        )
    }

    pub async fn hello(&mut self, interval_ms: u64) {
        self.send(
            json!({"op": 10, "d": {"heartbeat_interval": interval_ms}, "s": null, "t": null}),
        )
        .await;
    }

    pub async fn dispatch(&mut self, seq: u64, name: &str, d: Value) {
        self.send(json!({"op": 0, "s": seq, "t": name, "d": d}))
            .await;
    }

    pub async fn ready(&mut self, seq: u64, resume_url: &str) {
        self.dispatch(seq, "READY", ready_fixture(resume_url)).await;
    }

    /// Next client packet; heartbeats are recorded and, with `auto_ack`, acknowledged.
    pub async fn recv(&mut self) -> Value {
        timeout(STEP, async {
            loop {
                match self.ws.next().await {
                    Some(Ok(Message::Text(t))) => return serde_json::from_str(&t).unwrap(),
                    Some(Ok(Message::Close(_))) | None => panic!("client closed the socket"),
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => panic!("socket error: {e}"),
                }
            }
        })
        .await
        .expect("no packet from client")
    }

    pub async fn recv_op(&mut self, op: u64) -> Value {
        loop {
            let packet = self.recv().await;
            let got = packet["op"].as_u64().unwrap();
            if got == 1 {
                self.heartbeats.push(packet.clone());
                if self.auto_ack {
                    self.send(json!({"op": 11})).await;
                }
            }
            if got == op {
                return packet;
            }
        }
    }

    /// Waits for the client to close the socket, returning the close code.
    pub async fn closed(&mut self) -> Option<u16> {
        timeout(STEP, async {
            loop {
                match self.ws.next().await {
                    Some(Ok(Message::Close(frame))) => return frame.map(|f| u16::from(f.code)),
                    Some(Ok(_)) => continue,
                    _ => return None,
                }
            }
        })
        .await
        .expect("client did not close")
    }

    pub async fn close(&mut self, code: u16) {
        let frame = CloseFrame {
            code: CloseCode::from(code),
            reason: "".into(),
        };
        let _ = self.ws.send(Message::Close(Some(frame))).await;
    }

    /// HELLO, IDENTIFY, READY; returns the IDENTIFY packet.
    pub async fn handshake(&mut self, resume_url: &str) -> Value {
        self.hello(45_000).await;
        let identify = self.recv_op(2).await;
        self.ready(1, resume_url).await;
        identify
    }
}

fn zlib(c: &mut Compress, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 1024);
    let start = c.total_in();
    loop {
        let done = (c.total_in() - start) as usize;
        c.compress_vec(&data[done..], &mut out, FlushCompress::Sync)
            .unwrap();
        if (c.total_in() - start) as usize == data.len() && out.len() < out.capacity() {
            break;
        }
        out.reserve(1024);
    }
    out
}
