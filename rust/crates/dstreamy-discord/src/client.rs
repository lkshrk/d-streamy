use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::json;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::gateway::{
    CurrentUser, Dispatch, GatewayClient, GatewayError, GatewayEvent, GatewayOptions, Guild,
    VoiceChannel,
};
use crate::transport::{self, PendingConnection, Streamer, StreamerError};

/// The gateway plus a `Streamer` fed with every dispatch, whose payloads go out on the gateway.
pub struct DiscordClient {
    gateway: GatewayClient,
    streamer: Arc<Mutex<Streamer>>,
    bridge: AbortOnDrop,
}

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl DiscordClient {
    pub async fn connect(mut opts: GatewayOptions) -> Result<Self, GatewayError> {
        let (dispatch_tx, dispatch_rx) = mpsc::unbounded_channel();
        opts.dispatch_tx = Some(dispatch_tx);
        let gateway = GatewayClient::connect(opts).await?;
        let (payload_tx, payload_rx) = mpsc::unbounded_channel();
        let streamer = Arc::new(Mutex::new(Streamer::new(
            gateway.user().id.clone(),
            payload_tx,
        )));
        let bridge = tokio::spawn(bridge(
            streamer.clone(),
            dispatch_rx,
            payload_rx,
            gateway.command_sender(),
        ));
        Ok(Self {
            gateway,
            streamer,
            bridge: AbortOnDrop(bridge),
        })
    }

    pub fn gateway(&self) -> &GatewayClient {
        &self.gateway
    }

    pub fn user(&self) -> &CurrentUser {
        self.gateway.user()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<GatewayEvent> {
        self.gateway.subscribe()
    }

    pub fn subscribe_unbounded(&self) -> mpsc::UnboundedReceiver<GatewayEvent> {
        self.gateway.subscribe_unbounded()
    }

    pub fn guilds(&self) -> Vec<Guild> {
        self.gateway.guilds()
    }

    pub fn voice_channels(&self, guild_id: &str) -> Vec<VoiceChannel> {
        self.gateway.voice_channels(guild_id)
    }

    /// Direct access, e.g. for ICE servers or the media connections; never hold it across an await.
    pub fn streamer(&self) -> MutexGuard<'_, Streamer> {
        self.streamer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `guild_id: None` joins a DM call.
    pub fn join_voice(&self, guild_id: Option<String>, channel_id: String) -> PendingConnection {
        self.streamer().join_voice(guild_id, channel_id)
    }

    pub fn create_stream(&self) -> Result<PendingConnection, StreamerError> {
        self.streamer().create_stream()
    }

    pub fn stop_stream(&self) {
        self.streamer().stop_stream();
    }

    pub fn leave_voice(&self) {
        self.streamer().leave_voice();
    }

    /// After a channel move: retarget the next `create_stream()` without rejoining.
    pub fn set_voice_channel(&self, guild_id: Option<String>, channel_id: String) {
        self.streamer().set_voice_channel(guild_id, channel_id);
    }

    pub async fn close(self) {
        drop(self.bridge);
        self.gateway.close().await;
    }
}

async fn bridge(
    streamer: Arc<Mutex<Streamer>>,
    mut dispatches: mpsc::UnboundedReceiver<Dispatch>,
    mut payloads: mpsc::UnboundedReceiver<transport::GatewayPayload>,
    commands: mpsc::UnboundedSender<serde_json::Value>,
) {
    loop {
        tokio::select! {
            dispatch = dispatches.recv() => {
                let Some(Dispatch { name, data, .. }) = dispatch else { break };
                let data = Arc::try_unwrap(data).unwrap_or_else(|shared| (*shared).clone());
                let event = transport::GatewayEvent::from_dispatch(&name, data);
                streamer.lock().unwrap_or_else(PoisonError::into_inner).handle_event(event);
            }
            Some(p) = payloads.recv() => {
                if commands.send(json!({ "op": p.op, "d": p.d })).is_err() {
                    break;
                }
            }
        }
    }
}
