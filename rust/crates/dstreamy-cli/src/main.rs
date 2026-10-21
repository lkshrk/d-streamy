//! `dstreamy`: stream a window or display to a Discord voice channel from the command line.

mod capture;
mod discord;
mod output;
mod stream;

use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{ArgAction, Parser, Subcommand};
use dstreamy_core::{ConfigError, ConfigPaths, InvalidStreamConfig, TOKEN_ENV};
use dstreamy_discord::{DEFAULT_GATEWAY_URL, GatewayError, VoiceChannelKind};
use dstreamy_media::MediaError;
use serde::Serialize;
use tracing::Level;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::discord::Persistence;
use crate::output::{Out, table};

#[derive(Debug, Parser)]
#[command(
    name = "dstreamy",
    version,
    about = "Stream a window or display to a Discord voice channel"
)]
struct Cli {
    /// Print JSON instead of text (one object per line while streaming)
    #[arg(long, global = true)]
    json: bool,
    /// Directory holding config.toml and the token file
    #[arg(long, global = true, value_name = "DIR", env = "DSTREAMY_CONFIG_DIR")]
    config_dir: Option<PathBuf>,
    /// Log more on stderr (-v info, -vv debug)
    #[arg(short, long, global = true, action = ArgAction::Count)]
    verbose: u8,
    #[arg(long, global = true, hide = true, default_value = DEFAULT_GATEWAY_URL)]
    gateway_url: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Check a Discord user token (read from stdin) and store it
    Login,
    /// List the account's guilds
    Guilds,
    /// List the voice channels of a guild
    Channels {
        /// Guild id, see `dstreamy guilds`
        guild: String,
    },
    /// List the windows and displays that can be captured
    Targets {
        #[arg(long, hide = true)]
        skip_platform: bool,
    },
    /// Go Live in a voice channel until interrupted
    Stream(stream::StreamArgs),
}

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
    #[error("{0}")]
    Session(String),
    #[error("stream stopped: {0}")]
    Stopped(String),
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let out = Out::new(false);
            let text = e.render().to_string();
            if e.use_stderr() {
                out.err_raw(&text);
            } else {
                out.raw(&text);
            }
            return ExitCode::from(u8::try_from(e.exit_code()).unwrap_or(2));
        }
    };
    let out = Out::new(cli.json);
    init_logging(&out, cli.verbose);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            out.error(format_args!("starting the async runtime: {e}"));
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(run(cli, &out));
    // Background tasks (webrtc, the gateway) must not keep the process alive after the command.
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    match result {
        Ok(code) => code,
        Err(e) => {
            out.error(e);
            ExitCode::FAILURE
        }
    }
}

fn init_logging(out: &Out, verbose: u8) {
    let (ours, others) = match verbose {
        0 => (Level::WARN, Level::ERROR),
        1 => (Level::INFO, Level::WARN),
        _ => (Level::DEBUG, Level::INFO),
    };
    let filter = Targets::new()
        .with_target("dstreamy", ours)
        .with_target("discord_stream_rs", ours)
        .with_default(others);
    let stderr = out.stderr_writer();
    let _ = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(move || stderr.clone()),
        )
        .with(filter)
        .try_init();
}

async fn run(cli: Cli, out: &Out) -> Result<ExitCode, CliError> {
    let paths = match &cli.config_dir {
        Some(dir) => ConfigPaths::in_dir(dir),
        None => ConfigPaths::discover()?,
    };
    let ctx = Ctx {
        paths,
        gateway_url: cli.gateway_url,
    };
    match cli.command {
        Command::Login => login(&ctx, out).await,
        Command::Guilds => guilds(&ctx, out).await,
        Command::Channels { guild } => channels(&ctx, out, &guild).await,
        Command::Targets { skip_platform } => targets(out, !skip_platform).await,
        Command::Stream(args) => stream::run(&ctx, out, args).await,
    }
}

fn read_token() -> Result<String, CliError> {
    let stdin = std::io::stdin();
    let text = if stdin.is_terminal() {
        rpassword::prompt_password("Discord token: ").map_err(CliError::ReadToken)?
    } else {
        let mut text = String::new();
        stdin
            .lock()
            .read_to_string(&mut text)
            .map_err(CliError::ReadToken)?;
        text
    };
    let token = text.trim();
    if token.is_empty() {
        return Err(CliError::EmptyToken);
    }
    Ok(token.to_owned())
}

#[derive(Serialize)]
struct LoginRow<'a> {
    id: &'a str,
    username: &'a str,
    global_name: Option<&'a str>,
    token_file: &'a std::path::Path,
}

async fn login(ctx: &Ctx, out: &Out) -> Result<ExitCode, CliError> {
    let token = read_token()?;
    let client = discord::connect_with(ctx, token.clone(), Persistence::Brief).await?;
    let user = client.user().clone();
    client.close().await;
    ctx.paths.save_token(&token)?;
    if out.json {
        out.json_line(&LoginRow {
            id: &user.id,
            username: &user.username,
            global_name: user.global_name.as_deref(),
            token_file: &ctx.paths.token_file,
        });
    } else {
        out.line(format_args!(
            "logged in as {} ({}); token saved to {}",
            user.username,
            user.id,
            ctx.paths.token_file.display()
        ));
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Serialize)]
struct GuildRow {
    id: String,
    name: String,
    available: bool,
}

async fn guilds(ctx: &Ctx, out: &Out) -> Result<ExitCode, CliError> {
    let client = discord::connect(ctx, Persistence::Brief).await?;
    let guilds = discord::guilds(&client).await;
    client.close().await;
    let rows: Vec<GuildRow> = guilds
        .into_iter()
        .map(|g| GuildRow {
            id: g.id,
            name: g.name,
            available: g.available,
        })
        .collect();
    if out.json {
        out.json_line(&rows);
    } else {
        let cells: Vec<Vec<String>> = rows
            .iter()
            .map(|g| {
                let name = if g.available {
                    g.name.clone()
                } else {
                    format!("{} (unavailable)", g.name)
                };
                vec![g.id.clone(), name]
            })
            .collect();
        out.raw(&table(&cells));
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Serialize)]
struct ChannelRow {
    id: String,
    name: String,
    kind: &'static str,
    position: i64,
    parent_id: Option<String>,
}

async fn channels(ctx: &Ctx, out: &Out, guild: &str) -> Result<ExitCode, CliError> {
    let client = discord::connect(ctx, Persistence::Brief).await?;
    let channels = discord::voice_channels(&client, guild).await;
    client.close().await;
    let rows: Vec<ChannelRow> = channels?
        .into_iter()
        .map(|c| ChannelRow {
            id: c.id,
            name: c.name,
            kind: match c.kind {
                VoiceChannelKind::Voice => "voice",
                VoiceChannelKind::Stage => "stage",
            },
            position: c.position,
            parent_id: c.parent_id,
        })
        .collect();
    if out.json {
        out.json_line(&rows);
    } else {
        let cells: Vec<Vec<String>> = rows
            .iter()
            .map(|c| vec![c.id.clone(), c.kind.to_owned(), c.name.clone()])
            .collect();
        out.raw(&table(&cells));
    }
    Ok(ExitCode::SUCCESS)
}

async fn targets(out: &Out, include_platform: bool) -> Result<ExitCode, CliError> {
    let (targets, failure) = capture::targets(include_platform).await;
    if let Some(e) = failure {
        out.warn(format_args!("listing screen capture targets failed: {e}"));
    }
    let rows: Vec<capture::TargetRow> = targets.into_iter().map(Into::into).collect();
    if out.json {
        out.json_line(&rows);
    } else {
        let cells: Vec<Vec<String>> = rows
            .iter()
            .map(|t| {
                let size = if t.width > 0 {
                    format!("{}x{}", t.width, t.height)
                } else {
                    "-".to_owned()
                };
                let title = match &t.app {
                    Some(app) => format!("{} ({app})", t.title),
                    None => t.title.clone(),
                };
                vec![t.id.clone(), t.kind.to_owned(), size, title]
            })
            .collect();
        out.raw(&table(&cells));
    }
    Ok(ExitCode::SUCCESS)
}
