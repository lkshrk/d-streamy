use std::net::IpAddr;
use std::time::Duration;

use dstreamy_core::ResolvedToken;
use dstreamy_discord::{
    CurrentUser, DEFAULT_GATEWAY_URL, DiscordClient, GatewayEvent, GatewayOptions, Guild,
    IdentifyLedger, VoiceChannel,
};
use secrecy::SecretString;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::{Instant, timeout_at};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::ice::network_type::NetworkType;

use crate::{CliError, Ctx};

/// READY can list guilds as unavailable; their GUILD_CREATE usually follows within moments.
const GUILD_WAIT: Duration = Duration::from_secs(5);
const IDENTIFY_DIR: &str = "identify";
const BRIEF_MAX_FAILED_ATTEMPTS: u32 = 3;
/// A long-lived connection retries for minutes once it was READY; its first login gives up after this.
pub const LONG_FIRST_LOGIN_RETRY: Duration = Duration::from_secs(30);
const BRIEF_RECONNECT_DELAY_MAX: Duration = Duration::from_secs(5);
const BRIEF_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const BRIEF_HELLO_TIMEOUT: Duration = Duration::from_secs(20);
const BRIEF_READY_TIMEOUT: Duration = Duration::from_secs(60);
const BRIEF_CONNECTION_BUDGET: Duration = Duration::from_secs(
    (BRIEF_MAX_FAILED_ATTEMPTS as u64
        * (BRIEF_HANDSHAKE_TIMEOUT.as_secs()
            + BRIEF_HELLO_TIMEOUT.as_secs()
            + BRIEF_READY_TIMEOUT.as_secs()))
        + ((BRIEF_MAX_FAILED_ATTEMPTS - 1) as u64 * BRIEF_RECONNECT_DELAY_MAX.as_secs()),
);
pub const BRIEF_VALIDATION_TIMEOUT: Duration =
    BRIEF_CONNECTION_BUDGET.saturating_add(Duration::from_secs(15));

#[derive(Clone, Copy)]
pub enum Persistence {
    /// One-shot commands give up after a few failed connection attempts.
    Brief,
    Long,
    /// As `Long`, with this first-login window instead of [`LONG_FIRST_LOGIN_RETRY`].
    LongWithin(Duration),
}

pub async fn connect(ctx: &Ctx, persistence: Persistence) -> Result<DiscordClient, CliError> {
    let owned = ctx.clone();
    let resolved = tokio::task::spawn_blocking(move || crate::accounts::resolve(&owned))
        .await
        .map_err(|e| CliError::Session(format!("resolving account: {e}")))??;
    connect_resolved(ctx, resolved, persistence).await
}

/// Connects with an already chosen token, and records the account's user if it lacks one.
pub async fn connect_resolved(
    ctx: &Ctx,
    resolved: ResolvedToken,
    persistence: Persistence,
) -> Result<DiscordClient, CliError> {
    let ledger = identify_ledger(ctx, &resolved.token);
    connect_resolved_counted(ctx, resolved, persistence, ledger).await
}

pub async fn connect_resolved_counted(
    ctx: &Ctx,
    resolved: ResolvedToken,
    persistence: Persistence,
    ledger: IdentifyLedger,
) -> Result<DiscordClient, CliError> {
    let client = connect_counted(ctx, resolved.token.clone(), persistence, ledger).await?;
    let owned = ctx.clone();
    let user = client.user().clone();
    tokio::task::spawn_blocking(move || crate::accounts::remember_user(&owned, &resolved, &user))
        .await
        .map_err(|e| CliError::Session(format!("saving account metadata: {e}")))?;
    Ok(client)
}

pub async fn connect_with(
    ctx: &Ctx,
    token: String,
    persistence: Persistence,
) -> Result<DiscordClient, CliError> {
    let ledger = identify_ledger(ctx, &token);
    connect_counted(ctx, token, persistence, ledger).await
}

/// Runs `f` on a brief connection and closes it before returning `f`'s result.
pub async fn brief<T>(ctx: &Ctx, f: impl AsyncFnOnce(&DiscordClient) -> T) -> Result<T, CliError> {
    let client = connect(ctx, Persistence::Brief).await?;
    let result = f(&client).await;
    client.close().await;
    Ok(result)
}

/// The user `token` belongs to, from a brief connection that is closed again.
pub async fn current_user(ctx: &Ctx, token: String) -> Result<CurrentUser, CliError> {
    let client = connect_with(ctx, token, Persistence::Brief).await?;
    let user = client.user().clone();
    client.close().await;
    Ok(user)
}

/// The token's IDENTIFY count, kept under the config dir by token digest so every process shares it.
pub fn identify_ledger(ctx: &Ctx, token: &str) -> IdentifyLedger {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(token.as_bytes());
    let name: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    IdentifyLedger::default().persisted_at(
        ctx.paths
            .dir
            .join(IDENTIFY_DIR)
            .join(format!("{name}.json")),
    )
}

/// Connects with `ledger` counting the IDENTIFYs, so clients for one token share the daily limit.
pub async fn connect_counted(
    ctx: &Ctx,
    token: String,
    persistence: Persistence,
    ledger: IdentifyLedger,
) -> Result<DiscordClient, CliError> {
    let mut opts = GatewayOptions::new(SecretString::from(token)).with_url(&ctx.gateway_url);
    opts.identify_ledger = ledger;
    let opts = match persistence {
        Persistence::Brief => brief_options(opts),
        Persistence::Long => {
            opts.initial_give_up_after = Some(LONG_FIRST_LOGIN_RETRY);
            opts
        }
        Persistence::LongWithin(window) => {
            opts.initial_give_up_after = Some(window);
            opts
        }
    };
    let client = DiscordClient::connect(opts).await?;
    if is_loopback(&ctx.gateway_url) {
        let mut streamer = client.streamer();
        streamer.set_ice_servers(Vec::new());
        streamer.set_setting_engine(loopback_settings());
    }
    Ok(client)
}

fn brief_options(mut opts: GatewayOptions) -> GatewayOptions {
    opts.max_failed_attempts = BRIEF_MAX_FAILED_ATTEMPTS;
    opts.reconnect_delay_max = BRIEF_RECONNECT_DELAY_MAX;
    opts.handshake_timeout = BRIEF_HANDSHAKE_TIMEOUT;
    opts.hello_timeout = BRIEF_HELLO_TIMEOUT;
    opts.ready_timeout = BRIEF_READY_TIMEOUT;
    opts
}

/// Only the Discord gateway or a loopback mock may receive the token.
pub fn gateway_url(url: &str) -> Result<String, String> {
    if url == DEFAULT_GATEWAY_URL || is_loopback(url) {
        Ok(url.to_owned())
    } else {
        Err("only the Discord gateway or a loopback address is allowed".to_owned())
    }
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

/// An unknown guild is settled once no guild is still unavailable, so it fails without waiting.
fn guild_settled(guilds: &[Guild], guild_id: &str) -> bool {
    match guilds.iter().find(|g| g.id == guild_id) {
        Some(g) => g.available,
        None => guilds.iter().all(|g| g.available),
    }
}

pub async fn voice_channels(
    client: &DiscordClient,
    guild_id: &str,
) -> Result<Vec<VoiceChannel>, CliError> {
    let guilds = settle(client, |guilds| guild_settled(guilds, guild_id)).await;
    match guilds.iter().find(|g| g.id == guild_id) {
        Some(g) if g.available => Ok(client.voice_channels(guild_id)),
        Some(_) => Err(CliError::GuildUnavailable(guild_id.to_owned())),
        None => Err(CliError::UnknownGuild(guild_id.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection_budget(opts: &GatewayOptions) -> Duration {
        let attempt = opts.handshake_timeout + opts.hello_timeout + opts.ready_timeout;
        attempt * opts.max_failed_attempts
            + opts.reconnect_delay_max * opts.max_failed_attempts.saturating_sub(1)
    }

    #[test]
    fn brief_options_bound_the_connection_by_the_validation_timeout() {
        let hour = Duration::from_secs(3600);
        let mut lax = GatewayOptions::new(SecretString::from("t"));
        lax.max_failed_attempts = 100;
        lax.reconnect_delay_max = hour;
        lax.handshake_timeout = hour;
        lax.hello_timeout = hour;
        lax.ready_timeout = hour;
        let opts = brief_options(lax);
        assert_eq!(opts.max_failed_attempts, BRIEF_MAX_FAILED_ATTEMPTS);
        assert_eq!(opts.reconnect_delay_max, BRIEF_RECONNECT_DELAY_MAX);
        assert_eq!(opts.handshake_timeout, BRIEF_HANDSHAKE_TIMEOUT);
        assert_eq!(opts.hello_timeout, BRIEF_HELLO_TIMEOUT);
        assert_eq!(opts.ready_timeout, BRIEF_READY_TIMEOUT);
        assert_eq!(connection_budget(&opts), BRIEF_CONNECTION_BUDGET);
        assert!(BRIEF_CONNECTION_BUDGET < BRIEF_VALIDATION_TIMEOUT);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_off_connects_count_in_the_shared_identify_ledger() {
        use dstreamy_mock_discord::{MOCK_TOKEN, MockDiscord};
        let mock = MockDiscord::start().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(
            dstreamy_core::ConfigPaths::in_dir(dir.path()),
            mock.gateway_url(),
            None,
        );
        for _ in 0..2 {
            let client = connect_with(&ctx, MOCK_TOKEN.to_owned(), Persistence::Brief)
                .await
                .unwrap();
            client.close().await;
        }
        assert_eq!(identify_ledger(&ctx, MOCK_TOKEN).recent(), 2);
        assert_eq!(identify_ledger(&ctx, "another-token").recent(), 0);
        for entry in std::fs::read_dir(dir.path().join(IDENTIFY_DIR)).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            assert!(!name.contains(MOCK_TOKEN), "{name}");
            let body = std::fs::read_to_string(&path).unwrap();
            assert!(!body.contains(MOCK_TOKEN), "{body}");
        }
        mock.shutdown().await;
    }

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

    fn guild(id: &str, available: bool) -> Guild {
        Guild {
            id: id.to_owned(),
            name: id.to_owned(),
            available,
        }
    }

    #[test]
    fn guild_lookups_settle_as_soon_as_the_answer_is_known() {
        let ready = [guild("a", true), guild("b", true)];
        assert!(guild_settled(&ready, "a"));
        assert!(guild_settled(&ready, "unknown"));
        let pending = [guild("a", true), guild("b", false)];
        assert!(guild_settled(&pending, "a"));
        assert!(!guild_settled(&pending, "b"));
        assert!(!guild_settled(&pending, "unknown"));
    }

    #[test]
    fn only_discord_or_loopback_gateways_get_the_token() {
        for url in [
            DEFAULT_GATEWAY_URL,
            "ws://127.0.0.1:1234",
            "ws://localhost:1",
        ] {
            assert_eq!(gateway_url(url).as_deref(), Ok(url));
        }
        for url in [
            "ws://gateway.discord.gg",
            "wss://gateway.discord.gg.evil.example",
            "wss://10.0.0.1",
            "not a url",
        ] {
            assert!(gateway_url(url).is_err(), "{url}");
        }
    }
}
