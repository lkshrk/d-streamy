use std::net::IpAddr;
use std::time::Duration;

use dstreamy_discord::{DiscordClient, GatewayEvent, GatewayOptions, Guild, VoiceChannel};
use secrecy::SecretString;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::{Instant, timeout_at};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::ice::network_type::NetworkType;

use crate::{CliError, Ctx};

/// READY can list guilds as unavailable; their GUILD_CREATE usually follows within moments.
const GUILD_WAIT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
pub enum Persistence {
    /// One-shot commands give up after a few failed connection attempts.
    Brief,
    Long,
}

pub async fn connect(ctx: &Ctx, persistence: Persistence) -> Result<DiscordClient, CliError> {
    let token = ctx.paths.load_token()?.ok_or(CliError::NotLoggedIn)?;
    connect_with(ctx, token, persistence).await
}

pub async fn connect_with(
    ctx: &Ctx,
    token: String,
    persistence: Persistence,
) -> Result<DiscordClient, CliError> {
    let mut opts = GatewayOptions::new(SecretString::from(token)).with_url(&ctx.gateway_url);
    if let Persistence::Brief = persistence {
        opts.max_failed_attempts = 3;
        opts.reconnect_delay_max = Duration::from_secs(5);
    }
    let client = DiscordClient::connect(opts).await?;
    if is_loopback(&ctx.gateway_url) {
        let mut streamer = client.streamer();
        streamer.set_ice_servers(Vec::new());
        streamer.set_setting_engine(loopback_settings());
    }
    Ok(client)
}

/// A loopback gateway is a local mock, whose media server only offers loopback candidates.
fn is_loopback(gateway_url: &str) -> bool {
    let Ok(url) = url::Url::parse(gateway_url) else {
        return false;
    };
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

fn loopback_settings() -> SettingEngine {
    let mut s = SettingEngine::default();
    s.set_include_loopback_candidate(true);
    s.set_ip_filter(Box::new(|ip: IpAddr| ip.is_loopback()));
    s.set_network_types(vec![NetworkType::Udp4, NetworkType::Udp6]);
    s
}

/// Waits until `done` holds for the guild list or `GUILD_WAIT` passes, then returns the list.
async fn settle(client: &DiscordClient, mut done: impl FnMut(&[Guild]) -> bool) -> Vec<Guild> {
    let mut events = client.subscribe();
    let deadline = Instant::now() + GUILD_WAIT;
    loop {
        let guilds = client.guilds();
        if done(&guilds) {
            return guilds;
        }
        match timeout_at(deadline, events.recv()).await {
            Err(_) | Ok(Err(RecvError::Closed)) | Ok(Ok(GatewayEvent::Closed(_))) => {
                return guilds;
            }
            Ok(_) => {}
        }
    }
}

pub async fn guilds(client: &DiscordClient) -> Vec<Guild> {
    settle(client, |guilds| guilds.iter().all(|g| g.available)).await
}

pub async fn voice_channels(
    client: &DiscordClient,
    guild_id: &str,
) -> Result<Vec<VoiceChannel>, CliError> {
    let guilds = settle(client, |guilds| {
        match guilds.iter().find(|g| g.id == guild_id) {
            Some(g) => g.available,
            None => guilds.iter().all(|g| g.available),
        }
    })
    .await;
    match guilds.iter().find(|g| g.id == guild_id) {
        Some(g) if g.available => Ok(client.voice_channels(guild_id)),
        Some(_) => Err(CliError::GuildUnavailable(guild_id.to_owned())),
        None => Err(CliError::UnknownGuild(guild_id.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_urls_are_detected() {
        for url in [
            "ws://127.0.0.1:1234",
            "ws://localhost:1/?x=1",
            "ws://[::1]:9",
        ] {
            assert!(is_loopback(url), "{url}");
        }
        for url in ["wss://gateway.discord.gg", "ws://10.0.0.1:1", "not a url"] {
            assert!(!is_loopback(url), "{url}");
        }
    }
}
