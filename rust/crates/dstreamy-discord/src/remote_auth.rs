//! QR-code login through Discord's remote auth gateway (`docs.discord.food/remote-authentication/desktop`).

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use rsa::pkcs8::EncodePublicKey;
use rsa::{Oaep, RsaPrivateKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::time::{Instant, interval_at, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

use crate::gateway::{USER_AGENT, rustls_connector};

const GATEWAY_URL: &str = "wss://remote-auth-gateway.discord.gg/?v=2";
const LOGIN_URL: &str = "https://discord.com/api/v9/users/@me/remote-auth/login";
const ORIGIN: &str = "https://discord.com";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(20);
/// Local cap on a QR session, whatever lifetime the gateway announces.
const MAX_SESSION: Duration = Duration::from_secs(300);
/// Nonce, user payload and token: the only ciphertexts a session decrypts (ADR 0011).
const MAX_DECRYPTS: u8 = 3;

#[derive(Debug, Error)]
pub enum RemoteAuthError {
    #[error("remote auth connection failed: {0}")]
    Connect(String),
    #[error("remote auth gateway closed the session ({0})")]
    Closed(String),
    #[error("the QR code expired; run `dstreamy login` again")]
    Expired,
    #[error("the login was cancelled on the phone")]
    Cancelled,
    #[error("remote auth protocol error: {0}")]
    Protocol(String),
    #[error("Discord asked for a captcha; log in with a token instead (`dstreamy login --token`)")]
    Captcha,
    #[error("exchanging the login ticket failed: {0}")]
    Exchange(String),
    #[error(
        "Discord did not answer the login ticket exchange in time; check the network and run `dstreamy login` again"
    )]
    ExchangeTimedOut,
}

/// Progress reported while waiting for the phone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// Show this URL as a QR code.
    Qr(String),
    /// The phone scanned the code; the user still has to confirm there.
    Scanned { username: String },
}

/// Runs the QR flow until the phone confirms, then returns the account token.
pub async fn login(mut progress: impl FnMut(Progress)) -> Result<String, RemoteAuthError> {
    let keys = Keys::generate()?;
    let mut request = GATEWAY_URL
        .into_client_request()
        .map_err(|e| RemoteAuthError::Connect(e.to_string()))?;
    let headers = request.headers_mut();
    headers.insert("Origin", HeaderValue::from_static(ORIGIN));
    headers.insert("User-Agent", HeaderValue::from_static(USER_AGENT));
    let connector = rustls_connector().map_err(|e| RemoteAuthError::Connect(e.to_string()))?;
    let connect =
        tokio_tungstenite::connect_async_tls_with_config(request, None, false, Some(connector));
    let (socket, _) = timeout(CONNECT_TIMEOUT, connect)
        .await
        .map_err(|_| RemoteAuthError::Connect("handshake timed out".into()))?
        .map_err(|e| RemoteAuthError::Connect(e.to_string()))?;
    let (mut sink, mut stream) = socket.split();

    let mut session = Session::new(keys);
    let mut heartbeat = None;
    let mut deadline = Instant::now() + MAX_SESSION;
    let ticket = loop {
        let beat = async {
            match &mut heartbeat {
                Some(h) => {
                    tokio::time::Interval::tick(h).await;
                }
                None => std::future::pending().await,
            }
        };
        let expire = tokio::time::sleep_until(deadline);
        let msg = tokio::select! {
            msg = stream.next() => msg,
            () = beat => {
                send(&mut sink, json!({"op": "heartbeat"})).await?;
                continue;
            }
            () = expire => return Err(RemoteAuthError::Expired),
        };
        let text = match msg {
            Some(Ok(Message::Text(text))) => text,
            Some(Ok(Message::Close(frame))) => {
                return Err(match frame.as_ref().map(|f| u16::from(f.code)) {
                    Some(4003) => RemoteAuthError::Expired,
                    _ => RemoteAuthError::Closed(
                        frame.map_or_else(|| "no close frame".into(), |f| f.code.to_string()),
                    ),
                });
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(RemoteAuthError::Closed(e.to_string())),
            None => return Err(RemoteAuthError::Closed("connection ended".into())),
        };
        let packet: Value = serde_json::from_str(&text)
            .map_err(|e| RemoteAuthError::Protocol(format!("unreadable packet: {e}")))?;
        match session.handle(&packet)? {
            Step::Hello {
                heartbeat: every,
                timeout: lifetime,
                reply,
            } => {
                heartbeat = Some(interval_at(Instant::now() + every, every));
                deadline = deadline.min(Instant::now() + lifetime);
                send(&mut sink, reply).await?;
            }
            Step::Send(reply) => send(&mut sink, reply).await?,
            Step::Progress(p) => progress(p),
            Step::Ticket(ticket) => break ticket,
            Step::Ignore => {}
        }
    };
    let _ = sink.close().await;
    let encrypted = exchange(ticket).await?;
    session.keys.decrypt_text(&encrypted)
}

async fn send<S>(sink: &mut S, packet: Value) -> Result<(), RemoteAuthError>
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    sink.send(Message::Text(packet.to_string()))
        .await
        .map_err(|e| RemoteAuthError::Closed(e.to_string()))
}

async fn exchange(ticket: String) -> Result<String, RemoteAuthError> {
    let body = json!({ "ticket": ticket }).to_string();
    let response = tokio::task::spawn_blocking(move || {
        ureq::post(LOGIN_URL)
            .header("Content-Type", "application/json")
            .header("Origin", ORIGIN)
            .header("User-Agent", USER_AGENT)
            .config()
            .http_status_as_error(false)
            .timeout_global(Some(EXCHANGE_TIMEOUT))
            .build()
            .send(body.as_str())
            .and_then(|mut r| Ok((r.status().as_u16(), r.body_mut().read_to_string()?)))
    })
    .await
    .map_err(|e| RemoteAuthError::Exchange(e.to_string()))?
    .map_err(exchange_error)?;
    parse_exchange(response.0, &response.1)
}

fn exchange_error(error: ureq::Error) -> RemoteAuthError {
    match error {
        ureq::Error::Timeout(_) => RemoteAuthError::ExchangeTimedOut,
        other => RemoteAuthError::Exchange(other.to_string()),
    }
}

fn parse_exchange(status: u16, body: &str) -> Result<String, RemoteAuthError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|_| RemoteAuthError::Exchange(format!("HTTP {status}")))?;
    if value.get("captcha_key").is_some() {
        return Err(RemoteAuthError::Captcha);
    }
    match value["encrypted_token"].as_str() {
        Some(token) if (200..300).contains(&status) => Ok(token.to_owned()),
        _ => Err(RemoteAuthError::Exchange(format!(
            "HTTP {status}: {}",
            value["message"]
                .as_str()
                .unwrap_or("no token in the response")
        ))),
    }
}

struct Keys {
    private: RsaPrivateKey,
    spki: Vec<u8>,
    decrypts: u8,
}

impl Keys {
    fn generate() -> Result<Self, RemoteAuthError> {
        let private = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048)
            .map_err(|e| RemoteAuthError::Protocol(format!("generating the key: {e}")))?;
        let spki = private
            .to_public_key()
            .to_public_key_der()
            .map_err(|e| RemoteAuthError::Protocol(format!("encoding the key: {e}")))?
            .into_vec();
        Ok(Self {
            private,
            spki,
            decrypts: 0,
        })
    }

    fn fingerprint(&self) -> String {
        URL_SAFE_NO_PAD.encode(Sha256::digest(&self.spki))
    }

    // Blinding reduces, but does not remove, the RUSTSEC-2023-0071 timing leak (ADR 0011).
    fn decrypt(&mut self, b64: &str) -> Result<Vec<u8>, RemoteAuthError> {
        if self.decrypts >= MAX_DECRYPTS {
            return Err(RemoteAuthError::Protocol(
                "the gateway sent more ciphertexts than a login needs".into(),
            ));
        }
        self.decrypts += 1;
        let bytes = STANDARD
            .decode(b64)
            .map_err(|e| RemoteAuthError::Protocol(format!("bad base64: {e}")))?;
        self.private
            .decrypt_blinded(&mut rsa::rand_core::OsRng, Oaep::new::<Sha256>(), &bytes)
            .map_err(|e| RemoteAuthError::Protocol(format!("decrypting: {e}")))
    }

    fn decrypt_text(&mut self, b64: &str) -> Result<String, RemoteAuthError> {
        String::from_utf8(self.decrypt(b64)?)
            .map_err(|_| RemoteAuthError::Protocol("decrypted text is not UTF-8".into()))
    }
}

#[derive(Debug)]
enum Step {
    Hello {
        heartbeat: Duration,
        timeout: Duration,
        reply: Value,
    },
    Send(Value),
    Progress(Progress),
    Ticket(String),
    Ignore,
}

struct Session {
    keys: Keys,
    hello_seen: bool,
    nonce_seen: bool,
}

impl Session {
    fn new(keys: Keys) -> Self {
        Self {
            keys,
            hello_seen: false,
            nonce_seen: false,
        }
    }

    fn once(seen: &mut bool, op: &str) -> Result<(), RemoteAuthError> {
        if std::mem::replace(seen, true) {
            return Err(RemoteAuthError::Protocol(format!("repeated {op}")));
        }
        Ok(())
    }

    fn handle(&mut self, packet: &Value) -> Result<Step, RemoteAuthError> {
        let field = |name: &str| {
            packet[name]
                .as_str()
                .ok_or_else(|| RemoteAuthError::Protocol(format!("missing {name}")))
        };
        let millis = |name: &str| {
            packet[name]
                .as_u64()
                .map(Duration::from_millis)
                .ok_or_else(|| RemoteAuthError::Protocol(format!("missing {name}")))
        };
        match packet["op"].as_str().unwrap_or_default() {
            "hello" => {
                Self::once(&mut self.hello_seen, "hello")?;
                Ok(Step::Hello {
                    heartbeat: millis("heartbeat_interval")?,
                    timeout: millis("timeout_ms")?,
                    reply: json!({
                    "op": "init",
                    "encoded_public_key": STANDARD.encode(&self.keys.spki),
                    }),
                })
            }
            "nonce_proof" => {
                Self::once(&mut self.nonce_seen, "nonce_proof")?;
                let nonce = self.keys.decrypt(field("encrypted_nonce")?)?;
                Ok(Step::Send(json!({
                    "op": "nonce_proof",
                    "nonce": URL_SAFE_NO_PAD.encode(nonce),
                })))
            }
            "pending_remote_init" => {
                let fingerprint = field("fingerprint")?;
                if fingerprint != self.keys.fingerprint() {
                    return Err(RemoteAuthError::Protocol("fingerprint mismatch".into()));
                }
                Ok(Step::Progress(Progress::Qr(format!(
                    "https://discord.com/ra/{fingerprint}"
                ))))
            }
            "pending_ticket" => {
                let payload = self.keys.decrypt_text(field("encrypted_user_payload")?)?;
                let username = payload.rsplit(':').next().unwrap_or_default().to_owned();
                Ok(Step::Progress(Progress::Scanned { username }))
            }
            "pending_login" => Ok(Step::Ticket(field("ticket")?.to_owned())),
            "cancel" => Err(RemoteAuthError::Cancelled),
            _ => Ok(Step::Ignore),
        }
    }
}

#[cfg(test)]
mod tests {
    use rsa::RsaPublicKey;
    use rsa::pkcs8::DecodePublicKey;

    use super::*;

    fn encrypt_for(keys: &Keys, plain: &[u8]) -> String {
        let public = RsaPublicKey::from_public_key_der(&keys.spki).unwrap();
        let bytes = public
            .encrypt(&mut rsa::rand_core::OsRng, Oaep::new::<Sha256>(), plain)
            .unwrap();
        STANDARD.encode(bytes)
    }

    fn session() -> Session {
        Session::new(Keys::generate().unwrap())
    }

    #[test]
    fn hello_starts_heartbeats_and_sends_the_public_key() {
        let mut s = session();
        let step = s
            .handle(&json!({"op": "hello", "heartbeat_interval": 41250, "timeout_ms": 142637}))
            .unwrap();
        let Step::Hello {
            heartbeat,
            timeout,
            reply,
        } = step
        else {
            panic!("{step:?}")
        };
        assert_eq!(heartbeat, Duration::from_millis(41250));
        assert_eq!(timeout, Duration::from_millis(142637));
        assert_eq!(reply["op"], "init");
        let key = STANDARD
            .decode(reply["encoded_public_key"].as_str().unwrap())
            .unwrap();
        assert!(RsaPublicKey::from_public_key_der(&key).is_ok());
    }

    #[test]
    fn nonce_proof_returns_the_decrypted_nonce_base64url() {
        let mut s = session();
        let encrypted = encrypt_for(&s.keys, b"\xfa\x01nonce");
        let Step::Send(reply) = s
            .handle(&json!({"op": "nonce_proof", "encrypted_nonce": encrypted}))
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(reply["op"], "nonce_proof");
        assert_eq!(reply["nonce"], URL_SAFE_NO_PAD.encode(b"\xfa\x01nonce"));
    }

    #[test]
    fn pending_remote_init_yields_the_qr_url_for_our_key() {
        let mut s = session();
        let fp = s.keys.fingerprint();
        let step = s
            .handle(&json!({"op": "pending_remote_init", "fingerprint": fp}))
            .unwrap();
        let Step::Progress(Progress::Qr(url)) = step else {
            panic!("{step:?}")
        };
        assert_eq!(url, format!("https://discord.com/ra/{fp}"));
    }

    #[test]
    fn a_foreign_fingerprint_is_rejected() {
        let mut s = session();
        let err = s
            .handle(&json!({"op": "pending_remote_init", "fingerprint": "someone-else"}))
            .unwrap_err();
        assert!(matches!(err, RemoteAuthError::Protocol(_)));
    }

    #[test]
    fn pending_ticket_reports_the_username() {
        let mut s = session();
        let payload = encrypt_for(&s.keys, b"100000000000000001:0:0:example_user");
        let step = s
            .handle(&json!({"op": "pending_ticket", "encrypted_user_payload": payload}))
            .unwrap();
        assert!(matches!(
            step,
            Step::Progress(Progress::Scanned { username }) if username == "example_user"
        ));
    }

    #[test]
    fn pending_login_hands_over_the_ticket_and_cancel_fails() {
        let mut s = session();
        assert!(matches!(
            s.handle(&json!({"op": "pending_login", "ticket": "t1"})).unwrap(),
            Step::Ticket(t) if t == "t1"
        ));
        assert!(matches!(
            s.handle(&json!({"op": "cancel"})),
            Err(RemoteAuthError::Cancelled)
        ));
    }

    #[test]
    fn hello_and_nonce_proof_are_accepted_once() {
        let mut s = session();
        let hello = json!({"op": "hello", "heartbeat_interval": 41250, "timeout_ms": 142637});
        s.handle(&hello).unwrap();
        assert!(
            matches!(s.handle(&hello), Err(RemoteAuthError::Protocol(m)) if m.contains("hello"))
        );
        let nonce = json!({"op": "nonce_proof", "encrypted_nonce": encrypt_for(&s.keys, b"n")});
        s.handle(&nonce).unwrap();
        assert!(
            matches!(s.handle(&nonce), Err(RemoteAuthError::Protocol(m)) if m.contains("nonce"))
        );
    }

    #[test]
    fn decryption_stops_after_the_login_budget() {
        let mut keys = Keys::generate().unwrap();
        let encrypted = encrypt_for(&keys, b"x");
        for _ in 0..MAX_DECRYPTS {
            assert_eq!(keys.decrypt(&encrypted).unwrap(), b"x");
        }
        assert!(matches!(
            keys.decrypt(&encrypted),
            Err(RemoteAuthError::Protocol(_))
        ));
    }

    #[test]
    fn the_exchanged_token_decrypts() {
        let mut keys = Keys::generate().unwrap();
        let encrypted = encrypt_for(&keys, b"my.token.value");
        assert_eq!(keys.decrypt_text(&encrypted).unwrap(), "my.token.value");
    }

    #[test]
    fn exchange_responses_are_classified() {
        assert_eq!(
            parse_exchange(200, r#"{"encrypted_token":"abc"}"#).unwrap(),
            "abc"
        );
        assert!(matches!(
            parse_exchange(400, r#"{"captcha_key":["captcha-required"]}"#),
            Err(RemoteAuthError::Captcha)
        ));
        assert!(matches!(
            parse_exchange(401, r#"{"message":"Unauthorized"}"#),
            Err(RemoteAuthError::Exchange(m)) if m.contains("Unauthorized")
        ));
    }

    #[test]
    fn exchange_timeouts_are_actionable() {
        for timeout in [
            ureq::Timeout::Global,
            ureq::Timeout::Connect,
            ureq::Timeout::Resolve,
        ] {
            let err = exchange_error(ureq::Error::Timeout(timeout));
            assert!(matches!(err, RemoteAuthError::ExchangeTimedOut), "{err:?}");
            assert!(
                err.to_string().contains("run `dstreamy login` again"),
                "{err}"
            );
        }
        assert!(matches!(
            exchange_error(ureq::Error::HostNotFound),
            RemoteAuthError::Exchange(_)
        ));
    }
}
