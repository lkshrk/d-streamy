//! `dstreamy`: stream a window or display to a Discord voice channel from the command line.

use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{ArgAction, Parser, Subcommand};
use dstreamy_cli::discord::{self, Persistence};
use dstreamy_cli::login_capture::{LoginMode, login_mode};
use dstreamy_cli::output::{Out, table};
use dstreamy_cli::{CliError, Ctx, capture, daemon, serve, service, stream, tui};
use dstreamy_core::{ConfigPaths, RotatingFileWriter};
use dstreamy_discord::{CurrentUser, DEFAULT_GATEWAY_URL, VoiceChannelKind, remote_auth};
use dstreamy_media::AUDIO_DEVICE_TARGET;
use serde::Serialize;
use tracing::Level;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::{Targets, filter_fn};
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
    /// Log in by scanning a QR code with the Discord mobile app and store the token
    ///
    /// A token piped on stdin is used instead of the QR code.
    Login {
        /// Type or paste a token instead of scanning a QR code
        #[arg(long, conflicts_with = "browser")]
        token: bool,
        /// Log in on Discord's login page in a window instead of scanning a QR code
        #[arg(long)]
        browser: bool,
        /// Offer no QR or token fallback, for a login started without a terminal
        #[arg(long, hide = true, requires = "browser")]
        window_only: bool,
    },
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
    /// List the audio devices `stream --audio-source` can capture
    ///
    /// Without --audio-source, a stream captures what plays on the default output.
    AudioSources,
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
        #[cfg(feature = "test-hooks")]
        #[arg(long, hide = true, value_name = "PATH")]
        login_program: Option<PathBuf>,
    },
    /// Keep `dstreamy serve` running under the user's service manager
    ///
    /// Linux: a systemd user unit in ~/.config/systemd/user. A restart of the service ends
    /// the Discord session; nothing is carried over.
    Service(service::ServiceArgs),
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
    // `stream` prints its own warning and note for these, so stderr would show them twice.
    let own_reports = matches!(cli.command, Command::Stream(_)).then_some(AUDIO_DEVICE_TARGET);
    init_logging(&out, cli.verbose, log_file, to_stderr, own_reports);
    #[cfg(all(
        feature = "browser-login",
        any(target_os = "linux", target_os = "macos")
    ))]
    if let Command::Login { browser: true, .. } = cli.command {
        dstreamy_cli::browser_login::prepare_environment();
    }
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
    let credentials = match cli.command {
        Command::Login {
            token,
            browser,
            window_only,
        } => {
            let mode = login_mode(token, browser, std::io::stdin().is_terminal());
            let read = match mode {
                #[cfg(all(
                    feature = "browser-login",
                    any(target_os = "linux", target_os = "macos")
                ))]
                LoginMode::Browser => browser_token(&ctx, runtime.handle(), &out, window_only),
                #[cfg(not(all(
                    feature = "browser-login",
                    any(target_os = "linux", target_os = "macos")
                )))]
                LoginMode::Browser => Err(CliError::BrowserLoginUnavailable),
                LoginMode::Token => read_token().map(Credentials::unchecked).map(Some),
                LoginMode::Qr => Ok(None),
            };
            match read {
                Ok(credentials) => credentials,
                Err(e) => {
                    out.error(&e);
                    return match e {
                        CliError::BrowserLoginCancelled if window_only => {
                            ExitCode::from(dstreamy_cli::login_capture::CANCELLED_EXIT)
                        }
                        _ => ExitCode::FAILURE,
                    };
                }
            }
        }
        _ => None,
    };
    let result = runtime.block_on(run(cli.command, credentials, &ctx, &out));
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

fn init_logging(
    out: &Out,
    verbose: u8,
    file: Option<PathBuf>,
    to_stderr: bool,
    hidden_on_stderr: Option<&'static str>,
) {
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
            .with_filter(filter_fn(move |m| Some(m.target()) != hidden_on_stderr))
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
    credentials: Option<Credentials>,
    ctx: &Ctx,
    out: &Out,
) -> Result<ExitCode, CliError> {
    match command {
        Command::Login { .. } => {
            let credentials = match credentials {
                Some(credentials) => credentials,
                None => Credentials::unchecked(qr_token(out).await?),
            };
            login(ctx, out, credentials).await
        }
        Command::Guilds => guilds(ctx, out).await,
        Command::Channels { guild } => channels(ctx, out, &guild).await,
        Command::Targets { skip_platform } => targets(out, !skip_platform).await,
        Command::AudioSources => audio_sources(out).await,
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
            login_program,
        } => {
            let opts = serve::Options {
                endpoint,
                include_platform: !skip_platform,
                event_capacity: usize::try_from(event_capacity).unwrap_or(usize::MAX),
                test_hooks,
                login_program,
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
                login_program: None,
            };
            serve::run(ctx, out, opts).await
        }
        Command::Service(args) => service::run(out, args).await,
        Command::Tui { skip_platform } => {
            let opts = tui::Options {
                include_platform: !skip_platform,
            };
            tui::run(ctx, opts).await
        }
    }
}

/// `None` means the QR login runs next.
#[cfg(all(
    feature = "browser-login",
    any(target_os = "linux", target_os = "macos")
))]
fn browser_token(
    ctx: &Ctx,
    runtime: &tokio::runtime::Handle,
    out: &Out,
    window_only: bool,
) -> Result<Option<Credentials>, CliError> {
    use dstreamy_cli::login_capture::{FollowUp, Recovery, follow_up};
    let recovery = if window_only {
        Recovery::WindowOnly
    } else {
        Recovery::WithTerminal
    };
    match follow_up(dstreamy_cli::browser_login::run(
        ctx, runtime, out, recovery,
    )?)? {
        FollowUp::Login(verified) => Ok(Some(Credentials {
            token: verified.token,
            user: Some(verified.user),
        })),
        FollowUp::Qr | FollowUp::ReadToken if window_only => Err(CliError::BrowserLogin(
            "the QR code and token logins need a terminal: run `dstreamy login` in one".into(),
        )),
        FollowUp::Qr => Ok(None),
        FollowUp::ReadToken => read_token().map(Credentials::unchecked).map(Some),
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
#[serde(tag = "event", rename_all = "snake_case")]
enum QrRow<'a> {
    Qr { url: &'a str },
    Scanned { username: &'a str },
}

async fn qr_token(out: &Out) -> Result<String, CliError> {
    let token = remote_auth::login(|progress| match progress {
        remote_auth::Progress::Qr(url) if out.json => {
            out.json_line(&QrRow::Qr { url: &url });
        }
        remote_auth::Progress::Qr(url) => {
            out.line(format_args!(
                "Scan with the Discord mobile app (Settings → Scan QR Code).\n\
                 The login is for the account that is active in the app.\n\n{}",
                qr_text(&url)
            ));
        }
        remote_auth::Progress::Scanned { username } if out.json => {
            out.json_line(&QrRow::Scanned {
                username: &username,
            });
        }
        remote_auth::Progress::Scanned { username } => {
            out.line(format_args!(
                "scanned by {username}; confirm the login on the phone"
            ));
        }
    })
    .await?;
    Ok(token)
}

/// Two characters per module, so the code stays square in a terminal.
fn qr_text(url: &str) -> String {
    match qrcode::QrCode::new(url) {
        Ok(code) => code
            .render::<char>()
            .dark_color(' ')
            .light_color('█')
            .module_dimensions(2, 1)
            .quiet_zone(true)
            .build(),
        Err(_) => String::new(),
    }
}

#[derive(Serialize)]
struct LoginRow<'a> {
    id: &'a str,
    username: &'a str,
    global_name: Option<&'a str>,
    token_file: &'a std::path::Path,
}

/// A login token; `user` is set once the gateway has accepted it.
struct Credentials {
    token: String,
    user: Option<CurrentUser>,
}

impl Credentials {
    fn unchecked(token: String) -> Self {
        Self { token, user: None }
    }
}

async fn login(ctx: &Ctx, out: &Out, credentials: Credentials) -> Result<ExitCode, CliError> {
    let Credentials { token, user } = credentials;
    let user = match user {
        Some(user) => user,
        None => {
            let client = discord::connect_with(ctx, token.clone(), Persistence::Brief).await?;
            let user = client.user().clone();
            client.close().await;
            user
        }
    };
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

async fn audio_sources(out: &Out) -> Result<ExitCode, CliError> {
    let rows: Vec<capture::AudioSourceRow> = capture::audio_sources()
        .await?
        .into_iter()
        .map(Into::into)
        .collect();
    if out.json {
        out.json_line(&rows);
    } else if rows.is_empty() {
        out.note(
            "no audio sources to choose from; streams capture what plays on the default output",
        );
    } else {
        let cells: Vec<Vec<String>> = rows
            .iter()
            .map(|s| vec![s.id.clone(), s.kind.to_owned(), s.name.clone()])
            .collect();
        out.raw(&table(&cells));
    }
    Ok(ExitCode::SUCCESS)
}
