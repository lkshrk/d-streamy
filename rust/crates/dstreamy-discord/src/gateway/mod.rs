mod cache;
mod connection;
mod event;
mod inflate;
mod options;

use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use discord_stream_rs::{GatewayOpCode, GatewayPayload};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::Connector;

pub use cache::{Guild, VoiceChannel, VoiceChannelKind};
pub use event::{CurrentUser, Dispatch, GatewayEvent, VoiceState};
pub use options::{DEFAULT_GATEWAY_URL, GATEWAY_VERSION, GatewayOptions, IdentifyProperties};

const EVENT_CAPACITY: usize = 256;
const CLOSE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GatewayError {
    /// Close codes 4004 (authentication failed) and 4010–4014 cannot be retried.
    #[error("gateway rejected the session with close code {0}")]
    Rejected(u16),
    #[error("gave up after {0} failed gateway connection attempts")]
    Exhausted(u32),
    #[error("gateway client is closed")]
    Closed,
    #[error("TLS setup failed: {0}")]
    Tls(String),
}

#[derive(Default)]
pub(crate) struct Shared {
    cache: RwLock<cache::Cache>,
    user: RwLock<Option<CurrentUser>>,
    latency: Mutex<Option<Duration>>,
    lossless: Mutex<Vec<mpsc::UnboundedSender<GatewayEvent>>>,
}

/// User-account main gateway connection; reconnects and resumes on its own until closed.
pub struct GatewayClient {
    commands: mpsc::UnboundedSender<Value>,
    events: broadcast::Sender<GatewayEvent>,
    shared: Arc<Shared>,
    user: CurrentUser,
    shutdown: watch::Sender<()>,
    task: JoinHandle<()>,
}

impl GatewayClient {
    /// Connects, identifies and waits for READY.
    pub async fn connect(opts: GatewayOptions) -> Result<Self, GatewayError> {
        let connector = rustls_connector()?;
        let shared = Arc::new(Shared::default());
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let (commands, commands_rx) = mpsc::unbounded_channel();
        let (shutdown, shutdown_rx) = watch::channel(());
        let (ready_tx, ready_rx) = oneshot::channel();
        let runner = connection::Runner::new(
            opts,
            connector,
            shared.clone(),
            events.clone(),
            commands_rx,
            shutdown_rx,
            ready_tx,
        );
        let task = tokio::spawn(runner.run());
        let user = ready_rx.await.unwrap_or(Err(GatewayError::Closed))?;
        Ok(Self {
            commands,
            events,
            shared,
            user,
            shutdown,
            task,
        })
    }

    /// The account from the first READY.
    pub fn user(&self) -> &CurrentUser {
        &self.user
    }

    pub fn subscribe(&self) -> broadcast::Receiver<GatewayEvent> {
        self.events.subscribe()
    }

    /// Like `subscribe`, but never lags: for consumers that must see every voice state and READY.
    pub fn subscribe_unbounded(&self) -> mpsc::UnboundedReceiver<GatewayEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.shared
            .lossless
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(tx);
        rx
    }

    pub fn guilds(&self) -> Vec<Guild> {
        self.shared
            .cache
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .guilds()
    }

    /// Voice and stage channels of a guild, in Discord's display order.
    pub fn voice_channels(&self, guild_id: &str) -> Vec<VoiceChannel> {
        self.shared
            .cache
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .voice_channels(guild_id)
    }

    /// Round trip of the last acknowledged heartbeat.
    pub fn latency(&self) -> Option<Duration> {
        *self
            .shared
            .latency
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub fn is_closed(&self) -> bool {
        self.task.is_finished()
    }

    /// Queues a payload; it is written once the session is READY or RESUMED.
    pub fn send(&self, op: u8, d: Value) -> Result<(), GatewayError> {
        self.commands
            .send(json!({ "op": op, "d": d }))
            .map_err(|_| GatewayError::Closed)
    }

    pub fn send_payload(&self, payload: GatewayPayload) -> Result<(), GatewayError> {
        self.send(payload.op, payload.d)
    }

    /// Op 4; `channel_id: None` leaves voice.
    pub fn update_voice_state(
        &self,
        guild_id: Option<&str>,
        channel_id: Option<&str>,
        self_mute: bool,
        self_deaf: bool,
        self_video: bool,
    ) -> Result<(), GatewayError> {
        self.send(
            GatewayOpCode::VoiceStateUpdate as u8,
            json!({
                "guild_id": guild_id,
                "channel_id": channel_id,
                "self_mute": self_mute,
                "self_deaf": self_deaf,
                "self_video": self_video,
            }),
        )
    }

    /// Op 18; `guild_id: None` streams into a DM call.
    pub fn stream_create(
        &self,
        guild_id: Option<&str>,
        channel_id: &str,
    ) -> Result<(), GatewayError> {
        self.send(
            GatewayOpCode::StreamCreate as u8,
            json!({
                "type": if guild_id.is_some() { "guild" } else { "call" },
                "guild_id": guild_id,
                "channel_id": channel_id,
                "preferred_region": null,
            }),
        )
    }

    /// Op 22.
    pub fn stream_set_paused(&self, stream_key: &str, paused: bool) -> Result<(), GatewayError> {
        self.send(
            GatewayOpCode::StreamSetPaused as u8,
            json!({ "stream_key": stream_key, "paused": paused }),
        )
    }

    /// Op 19.
    pub fn stream_delete(&self, stream_key: &str) -> Result<(), GatewayError> {
        self.send(
            GatewayOpCode::StreamDelete as u8,
            json!({ "stream_key": stream_key }),
        )
    }

    /// Closes the socket with 1000 (ending the session) and waits for the task.
    pub async fn close(mut self) {
        let _ = self.shutdown.send(());
        if tokio::time::timeout(CLOSE_TIMEOUT, &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
        }
    }

    pub(crate) fn command_sender(&self) -> mpsc::UnboundedSender<Value> {
        self.commands.clone()
    }
}

fn rustls_connector() -> Result<Connector, GatewayError> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| GatewayError::Tls(e.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Connector::Rustls(Arc::new(config)))
}
