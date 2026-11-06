//! Shared pieces of the `dstreamy` command line and its terminal UI.

pub mod accounts;
#[cfg(all(
    feature = "browser-login",
    any(target_os = "linux", target_os = "macos")
))]
pub mod browser_login;
pub mod capture;
pub mod daemon;
pub mod discord;
pub mod engine;
pub mod lock;
pub mod login_capture;
pub mod output;
pub mod serve;
pub mod service;
pub mod signals;
pub mod stream;
pub mod tui;

use dstreamy_core::{ConfigError, ConfigPaths, InvalidStreamConfig, TOKEN_ENV};
use dstreamy_discord::GatewayError;
use dstreamy_ipc::IpcError;
use dstreamy_media::{MediaError, OptionsError};
use lock::LockError;

#[derive(Clone)]
pub struct Ctx {
    pub paths: ConfigPaths,
    pub gateway_url: String,
    /// `--account`; `None` uses the default account.
    pub account: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    InvalidStreamConfig(#[from] InvalidStreamConfig),
    #[error("not logged in: run `dstreamy login` or set {TOKEN_ENV}")]
    NotLoggedIn,
    #[error("no token given on stdin")]
    EmptyToken,
    #[error("reading the token: {0}")]
    ReadToken(std::io::Error),
    #[error(transparent)]
    Gateway(#[from] GatewayError),
    #[error(transparent)]
    RemoteAuth(#[from] dstreamy_discord::remote_auth::RemoteAuthError),
    #[error("browser login failed: {0}")]
    BrowserLogin(String),
    #[error("browser login failed: cancelled")]
    BrowserLoginCancelled,
    #[error(
        "browser login is not included in this build of dstreamy; run `dstreamy login` to scan a QR code, or `dstreamy login --token` to paste a token"
    )]
    BrowserLoginUnavailable,
    #[error("unknown guild {0}; see `dstreamy guilds`")]
    UnknownGuild(String),
    #[error("guild {0} is unavailable right now")]
    GuildUnavailable(String),
    #[error("no voice channel {channel} in guild {guild}; see `dstreamy channels {guild}`")]
    UnknownChannel { guild: String, channel: String },
    #[error("screen capture is not supported on this platform; use --target test-pattern")]
    NoPlatformCapture,
    #[error("unknown audio source {0:?}; see `dstreamy audio-sources`")]
    UnknownAudioSource(String),
    #[error(transparent)]
    Media(#[from] MediaError),
    #[error("a stream is already running")]
    AlreadyStreaming,
    #[error("account {name} is streaming{}; stop that stream first", pid.map(|p| format!(" (pid {p})")).unwrap_or_default())]
    AccountStreaming { name: String, pid: Option<u32> },
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error("config [serve]: {0}")]
    ServeConfig(#[from] OptionsError),
    #[error(transparent)]
    Ipc(#[from] IpcError),
    #[error("{0}")]
    Session(String),
    #[error("stream stopped: {0}")]
    Stopped(String),
    #[error("the TUI needs an interactive terminal on stdin and stdout")]
    NoTerminal,
    #[error("terminal: {0}")]
    Terminal(String),
    #[error(transparent)]
    Service(#[from] service::ServiceError),
}
