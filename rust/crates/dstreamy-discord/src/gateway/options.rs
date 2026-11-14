use std::time::Duration;

use secrecy::SecretString;
use serde::Serialize;
use tokio::sync::mpsc;

use super::event::Dispatch;
pub use super::ledger::IdentifyLedger;

pub const DEFAULT_GATEWAY_URL: &str = "wss://gateway.discord.gg";
pub const GATEWAY_VERSION: u8 = 9;

pub(crate) const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) discord/1.0.9210 Chrome/134.0.6998.205 Electron/35.3.0 Safari/537.36";

#[derive(Debug, Clone)]
pub struct GatewayOptions {
    pub url: String,
    pub token: SecretString,
    pub properties: IdentifyProperties,
    pub capabilities: u64,
    /// Transport compression (`compress=zlib-stream`).
    pub compress: bool,
    pub handshake_timeout: Duration,
    pub hello_timeout: Duration,
    /// Until READY or RESUMED, from connect or from losing READY; a run of invalid sessions shares it, extended once by the first IDENTIFY wait.
    pub ready_timeout: Duration,
    /// Re-identify after a non-resumable INVALID_SESSION waits a random time in `[max / 5, max]`.
    pub invalid_session_delay: Duration,
    pub reconnect_delay_min: Duration,
    pub reconnect_delay_max: Duration,
    /// Consecutive failed connections before the client gives up.
    pub max_failed_attempts: u32,
    /// Before the first READY, give up at the first failure this long after connecting began; `None` only counts attempts.
    pub initial_give_up_after: Option<Duration>,
    /// A READY or RESUMED stretch this long is healthy: it resets the failure count and re-allows re-identify and resume-in-place.
    /// Without one, a second non-resumable invalid session closes the connection.
    pub healthy_session_min: Duration,
    /// IDENTIFYs allowed in any 24 h; one more stops the client with `GatewayError::IdentifyLimit`. RESUMEs do not count.
    pub max_identifies_per_day: u32,
    /// Where IDENTIFYs are counted; give replacement clients for one token the same ledger.
    pub identify_ledger: IdentifyLedger,
    /// Receives every dispatch in order, without the lag a broadcast subscriber can hit.
    pub dispatch_tx: Option<mpsc::UnboundedSender<Dispatch>>,
}

impl GatewayOptions {
    pub fn new(token: SecretString) -> Self {
        Self {
            url: DEFAULT_GATEWAY_URL.to_owned(),
            token,
            properties: IdentifyProperties::default(),
            capabilities: 0,
            compress: true,
            handshake_timeout: Duration::from_secs(30),
            hello_timeout: Duration::from_secs(20),
            ready_timeout: Duration::from_secs(60),
            invalid_session_delay: Duration::from_secs(5),
            reconnect_delay_min: Duration::from_secs(1),
            reconnect_delay_max: Duration::from_secs(60),
            max_failed_attempts: 10,
            initial_give_up_after: None,
            healthy_session_min: Duration::from_secs(30),
            max_identifies_per_day: 100,
            identify_ledger: IdentifyLedger::default(),
            dispatch_tx: None,
        }
    }

    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }
}

/// IDENTIFY `properties` of the Discord desktop client, as discord.js-selfbot-v13 3.7 sends them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IdentifyProperties {
    pub os: String,
    pub browser: String,
    pub release_channel: String,
    pub client_version: String,
    pub os_version: String,
    pub os_arch: String,
    pub app_arch: String,
    pub system_locale: String,
    pub has_client_mods: bool,
    pub client_launch_id: String,
    pub browser_user_agent: String,
    pub browser_version: String,
    pub os_sdk_version: String,
    pub client_build_number: u64,
    pub native_build_number: u64,
    pub client_event_source: Option<String>,
    pub launch_signature: String,
    pub client_heartbeat_session_id: String,
    pub client_app_state: String,
}

impl Default for IdentifyProperties {
    fn default() -> Self {
        let uuid = || uuid::Uuid::new_v4().to_string();
        Self {
            os: "Windows".into(),
            browser: "Discord Client".into(),
            release_channel: "stable".into(),
            client_version: "1.0.9210".into(),
            os_version: "10.0.19044".into(),
            os_arch: "x64".into(),
            app_arch: "x64".into(),
            system_locale: "en-US".into(),
            has_client_mods: false,
            client_launch_id: uuid(),
            browser_user_agent: USER_AGENT.into(),
            browser_version: "35.3.0".into(),
            os_sdk_version: "19044".into(),
            client_build_number: 455_964,
            native_build_number: 69_976,
            client_event_source: None,
            launch_signature: uuid(),
            client_heartbeat_session_id: uuid(),
            client_app_state: "focused".into(),
        }
    }
}

pub(crate) fn connect_url(base: &str, compress: bool) -> String {
    let (base, query) = base.split_once('?').unwrap_or((base, ""));
    let after_scheme = base.split_once("://").map_or(base, |(_, rest)| rest);
    let slash = if after_scheme.contains('/') { "" } else { "/" };
    let mut url = format!("{base}{slash}?v={GATEWAY_VERSION}&encoding=json");
    if compress {
        url.push_str("&compress=zlib-stream");
    }
    if !query.is_empty() {
        url.push('&');
        url.push_str(query);
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_url_adds_path_and_query() {
        assert_eq!(
            connect_url("wss://gateway.discord.gg", true),
            "wss://gateway.discord.gg/?v=9&encoding=json&compress=zlib-stream"
        );
        assert_eq!(
            connect_url("ws://127.0.0.1:1/resume", false),
            "ws://127.0.0.1:1/resume?v=9&encoding=json"
        );
        assert_eq!(
            connect_url("ws://h/?x=1", false),
            "ws://h/?v=9&encoding=json&x=1"
        );
    }

    #[test]
    fn debug_redacts_token() {
        let opts = GatewayOptions::new(SecretString::from("super-secret-value"));
        assert!(!format!("{opts:?}").contains("super-secret-value"));
    }
}
