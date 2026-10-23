//! Shared pieces of the `dstreamy` command line and its terminal UI.

pub mod capture;
pub mod daemon;
pub mod discord;
pub mod lock;
pub mod output;
pub mod signals;
pub mod stream;
pub mod tui;

use dstreamy_core::{ConfigError, ConfigPaths, InvalidStreamConfig, TOKEN_ENV};
use dstreamy_discord::GatewayError;
use dstreamy_media::MediaError;
use lock::LockError;

#[derive(Clone)]
pub struct Ctx {
    pub paths: ConfigPaths,
    pub gateway_url: String,
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
    #[error("unknown guild {0}; see `dstreamy guilds`")]
    UnknownGuild(String),
    #[error("guild {0} is unavailable right now")]
    GuildUnavailable(String),
    #[error("no voice channel {channel} in guild {guild}; see `dstreamy channels {guild}`")]
    UnknownChannel { guild: String, channel: String },
    #[error("screen capture is not supported on this platform; use --target test-pattern")]
    NoPlatformCapture,
    #[error(transparent)]
    Media(#[from] MediaError),
    #[error("a stream is already running")]
    AlreadyStreaming,
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error("{0}")]
    Session(String),
    #[error("stream stopped: {0}")]
    Stopped(String),
    #[error("the TUI needs an interactive terminal on stdin and stdout")]
    NoTerminal,
    #[error("terminal: {0}")]
    Terminal(String),
}
