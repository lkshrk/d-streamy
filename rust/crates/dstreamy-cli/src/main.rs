//! `dstreamy`: stream a window or display to a Discord voice channel from the command line.

use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{ArgAction, Parser, Subcommand};
use dstreamy_cli::discord::{self, Persistence};
use dstreamy_cli::output::{Out, table};
use dstreamy_cli::{CliError, Ctx, capture, daemon, serve, stream, tui};
use dstreamy_core::{ConfigPaths, RotatingFileWriter};
use dstreamy_discord::{DEFAULT_GATEWAY_URL, VoiceChannelKind};
use serde::Serialize;
use tracing::Level;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::fmt::writer::BoxMakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

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
    #[arg(
        long,
        global = true,
        hide = true,
        default_value = DEFAULT_GATEWAY_URL,
        value_parser = discord::gateway_url
    )]
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
    /// Pick a channel and target, adjust settings and watch stream health interactively
    ///
    /// Logs go to dstreamy.log in the config directory while the TUI runs.
    Tui {
        #[arg(long, hide = true)]
        skip_platform: bool,
    },
    /// Serve the JSON protocol of the macOS menu bar app on stdin and stdout
    ///
    /// Logs go to dstreamy.log in the config directory, and to stderr with -v.
    Daemon {
        #[arg(long, hide = true)]
        skip_platform: bool,
    },
    /// Run as a service: control clients and media producers connect over a local socket
    ///
    /// Runs until a client sends `shutdown`, or SIGINT or SIGTERM arrives. Logs go to
    /// dstreamy.log in the config directory, and to stderr with -v.
    Serve {
        /// Socket path; defaults to a private per-user directory
        #[arg(long, value_name = "PATH")]
        endpoint: Option<PathBuf>,
        #[cfg(feature = "test-hooks")]
        #[arg(long, hide = true)]
        skip_platform: bool,
        #[cfg(feature = "test-hooks")]
        #[arg(long, hide = true, default_value_t = 256, value_parser = clap::value_parser!(u64).range(1..=65_536))]
        event_capacity: u64,
        #[cfg(feature = "test-hooks")]
        #[arg(long, hide = true)]
        test_hooks: bool,
    },
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
    let ctx = match context(&cli) {
        Ok(ctx) => ctx,
        Err(e) => {
            out.error(e);
            return ExitCode::FAILURE;
        }
    };
    let (log_file, to_stderr) = match cli.command {
        Command::Tui { .. } => (Some(tui::log_file(&ctx)), false),
        Command::Daemon { .. } | Command::Serve { .. } => {
            (Some(tui::log_file(&ctx)), cli.verbose > 0)
        }
        _ => (None, true),
    };
    init_logging(&out, cli.verbose, log_file, to_stderr);
    let token = match cli.command {
        Command::Login => match read_token() {
            Ok(token) => Some(token),
            Err(e) => {
                out.error(e);
                return ExitCode::FAILURE;
            }
        },
        _ => None,
    };
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
    let result = runtime.block_on(run(cli.command, token, &ctx, &out));
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

fn init_logging(out: &Out, verbose: u8, file: Option<PathBuf>, to_stderr: bool) {
    let (ours, others) = match verbose {
        0 => (Level::WARN, Level::ERROR),
        1 => (Level::INFO, Level::WARN),
        _ => (Level::DEBUG, Level::INFO),
    };
    let filter = Targets::new()
        .with_target("dstreamy", ours)
        .with_target("discord_stream_rs", ours)
        .with_default(others);
    let file = file.map(|path| {
        if let Some(dir) = path.parent() {
            // A missing directory only costs the log file; the writer swallows its own errors.
            let _ = std::fs::create_dir_all(dir);
        }
        let file = RotatingFileWriter::with_defaults(path);
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(BoxMakeWriter::new(move || file.clone()))
    });
    let stderr = to_stderr.then(|| {
        let stderr = out.stderr_writer();
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(BoxMakeWriter::new(move || stderr.clone()))
    });
    let _ = tracing_subscriber::registry()
        .with(file)
        .with(stderr)
        .with(filter)
        .try_init();
}

fn context(cli: &Cli) -> Result<Ctx, CliError> {
    let paths = match &cli.config_dir {
        Some(dir) => ConfigPaths::in_dir(dir),
        None => ConfigPaths::discover()?,
    };
    Ok(Ctx {
        paths,
        gateway_url: cli.gateway_url.clone(),
    })
}

async fn run(
    command: Command,
    token: Option<String>,
    ctx: &Ctx,
    out: &Out,
) -> Result<ExitCode, CliError> {
    match command {
        Command::Login => login(ctx, out, token.ok_or(CliError::EmptyToken)?).await,
        Command::Guilds => guilds(ctx, out).await,
        Command::Channels { guild } => channels(ctx, out, &guild).await,
        Command::Targets { skip_platform } => targets(out, !skip_platform).await,
        Command::Stream(args) => stream::run(ctx, out, args).await,
        Command::Daemon { skip_platform } => {
            let opts = daemon::Options {
                include_platform: !skip_platform,
                ..daemon::Options::default()
            };
            daemon::run(ctx, out, opts).await
        }
        #[cfg(feature = "test-hooks")]
        Command::Serve {
            endpoint,
            skip_platform,
            event_capacity,
            test_hooks,
        } => {
            let opts = serve::Options {
                endpoint,
                include_platform: !skip_platform,
                event_capacity: usize::try_from(event_capacity).unwrap_or(usize::MAX),
                test_hooks,
            };
            serve::run(ctx, out, opts).await
        }
        #[cfg(not(feature = "test-hooks"))]
        Command::Serve { endpoint } => {
            let opts = serve::Options {
                endpoint,
                include_platform: true,
                event_capacity: serve::DEFAULT_EVENT_CAPACITY,
                test_hooks: false,
            };
            serve::run(ctx, out, opts).await
        }
        Command::Tui { skip_platform } => {
            let opts = tui::Options {
                include_platform: !skip_platform,
            };
            tui::run(ctx, opts).await
        }
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

async fn login(ctx: &Ctx, out: &Out, token: String) -> Result<ExitCode, CliError> {
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
