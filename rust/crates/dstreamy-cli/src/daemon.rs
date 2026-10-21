//! `dstreamy daemon`: protocol 1 of ADR 0003, one JSON object per line on stdin and stdout.

use std::collections::HashMap;
use std::io::BufRead;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use dstreamy_core::{CropRect, Fps, Resolution, StreamConfig};
use dstreamy_discord::{
    DiscordClient, GatewayError, Session, SessionError, SessionEvent, SessionState, StreamStats,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::timeout;

use crate::capture::{self, PortalBook, TargetRow};
use crate::discord::{self, Persistence};
use crate::output::Out;
use crate::signals::Signals;
use crate::stream::{self, parse_codec};
use crate::{CliError, Ctx};

pub const PROTOCOL: u32 = 1;
/// Discord closes the gateway with 4004 when the token is not valid.
const AUTH_FAILED: u16 = 4004;
const FORWARDER_DRAIN: Duration = Duration::from_secs(2);
const COMMANDS: [&str; 10] = [
    "hello",
    "set_token",
    "logged_in",
    "guilds",
    "channels",
    "targets",
    "start",
    "update",
    "stop",
    "shutdown",
];

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub include_platform: bool,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Message<'a> {
    Result {
        id: &'a str,
        data: Value,
    },
    Error {
        id: Option<&'a str>,
        code: &'static str,
        message: String,
    },
    Event {
        event: &'a SessionEvent,
    },
}

#[derive(Debug)]
struct Failure {
    code: &'static str,
    message: String,
}

impl Failure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<CliError> for Failure {
    fn from(e: CliError) -> Self {
        let code = match &e {
            CliError::Config(_) => "config",
            CliError::InvalidStreamConfig(_) | CliError::EmptyToken => "invalid_params",
            CliError::NotLoggedIn => {
                return Self::new("not_logged_in", "not logged in; send set_token first");
            }
            CliError::Gateway(GatewayError::Rejected(AUTH_FAILED)) => {
                return Self::new("invalid_token", "Discord rejected the token");
            }
            CliError::Gateway(_) => "gateway",
            CliError::UnknownGuild(guild) => {
                return Self::new("unknown_guild", format!("unknown guild {guild}"));
            }
            CliError::GuildUnavailable(_) => "guild_unavailable",
            CliError::UnknownChannel { guild, channel } => {
                return Self::new(
                    "unknown_channel",
                    format!("no voice channel {channel} in guild {guild}"),
                );
            }
            CliError::NoPlatformCapture => {
                return Self::new(
                    "no_platform_capture",
                    "screen capture is not supported on this platform",
                );
            }
            CliError::Media(_) => "capture",
            CliError::AlreadyStreaming => "already_running",
            CliError::ReadToken(_)
            | CliError::Session(_)
            | CliError::Stopped(_)
            | CliError::NoTerminal
            | CliError::Terminal(_) => "internal",
        };
        Self::new(code, e.to_string())
    }
}

fn invalid_params(e: impl std::fmt::Display) -> Failure {
    Failure::new("invalid_params", e.to_string())
}

fn not_running() -> Failure {
    Failure::new("not_running", "no stream is running")
}

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Command {
    Hello,
    SetToken { token: String },
    LoggedIn,
    Guilds,
    Channels { guild_id: String },
    Targets,
    Start(StartParams),
    Update(UpdateParams),
    Stop,
    Shutdown,
}

#[derive(Deserialize)]
struct StartParams {
    guild_id: String,
    channel_id: String,
    target: String,
    crop: Option<CropRect>,
    codec: Option<String>,
    fps: Option<Fps>,
    resolution: Option<Resolution>,
    bitrate_kbps: Option<u32>,
    gain: Option<f32>,
}

#[derive(Deserialize)]
struct UpdateParams {
    crop: Option<CropRect>,
    fps: Option<Fps>,
    resolution: Option<Resolution>,
    bitrate_kbps: Option<u32>,
}

fn parse(line: &[u8]) -> Result<(String, Command), (Option<String>, Failure)> {
    let malformed = |id: Option<String>, what: &str| {
        (
            id,
            Failure::new("malformed", format!("malformed line: {what}")),
        )
    };
    let value: Value = serde_json::from_slice(line).map_err(|e| malformed(None, &e.to_string()))?;
    let Value::Object(fields) = &value else {
        return Err(malformed(None, "expected a JSON object"));
    };
    let Some(id) = fields.get("id").and_then(Value::as_str).map(str::to_owned) else {
        return Err(malformed(None, "missing string field \"id\""));
    };
    let Some(cmd) = fields.get("cmd").and_then(Value::as_str) else {
        return Err(malformed(Some(id), "missing string field \"cmd\""));
    };
    if !COMMANDS.contains(&cmd) {
        let failure = Failure::new("unknown_cmd", format!("unknown command {cmd:?}"));
        return Err((Some(id), failure));
    }
    match serde_json::from_value(value) {
        Ok(command) => Ok((id, command)),
        Err(e) => Err((Some(id), invalid_params(e))),
    }
}

struct Conn {
    client: Arc<DiscordClient>,
    session: Session,
    forwarder: JoinHandle<()>,
}

impl Conn {
    fn live(&self) -> bool {
        matches!(
            *self.session.state().borrow(),
            SessionState::Connecting | SessionState::Streaming | SessionState::Reconnecting { .. }
        )
    }
}

struct Running {
    target: String,
    stream: StreamConfig,
    crop: Option<CropRect>,
}

struct Daemon {
    ctx: Ctx,
    opts: Options,
    writer: Mutex<Option<std::sync::mpsc::Sender<String>>>,
    conn: Mutex<Option<Arc<Conn>>>,
    /// Held while the connection is created, replaced or torn down, and while a stream starts.
    lifecycle: tokio::sync::Mutex<()>,
    updating: tokio::sync::Mutex<()>,
    pending: Mutex<HashMap<u64, String>>,
    last_stats: Mutex<Option<StreamStats>>,
    running: Mutex<Option<Running>>,
    portal: Mutex<Option<PortalBook>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Lines from stdin until EOF or a read error; invalid UTF-8 still arrives as a line.
fn read_stdin(lines: mpsc::UnboundedSender<Vec<u8>>) {
    let mut stdin = std::io::stdin().lock();
    loop {
        let mut line = Vec::new();
        match stdin.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if line.last() == Some(&b'\n') {
                    line.pop();
                }
                if lines.send(line).is_err() {
                    break;
                }
            }
        }
    }
}

pub async fn run(ctx: &Ctx, out: &Out, opts: Options) -> Result<ExitCode, CliError> {
    let (lines_tx, mut lines) = mpsc::unbounded_channel();
    // A detached thread: a blocked stdin read must not hold up the runtime's shutdown.
    std::thread::spawn(move || read_stdin(lines_tx));
    let (writer, written) = std::sync::mpsc::channel::<String>();
    let writer_out = out.clone();
    let writing = std::thread::spawn(move || {
        for line in written {
            writer_out.line(line);
        }
    });
    let daemon = Arc::new(Daemon {
        ctx: ctx.clone(),
        opts,
        writer: Mutex::new(Some(writer)),
        conn: Mutex::new(None),
        lifecycle: tokio::sync::Mutex::new(()),
        updating: tokio::sync::Mutex::new(()),
        pending: Mutex::new(HashMap::new()),
        last_stats: Mutex::new(None),
        running: Mutex::new(None),
        portal: Mutex::new(None),
    });
    let mut signals = Signals::new();
    let mut tasks = JoinSet::new();
    let mut next_key = 0u64;
    let mut shutdown_id = None;
    loop {
        tokio::select! {
            line = lines.recv() => {
                let Some(line) = line else { break };
                match parse(&line) {
                    Err((id, failure)) => daemon.reply(id.as_deref(), Err(failure)),
                    Ok((id, Command::Shutdown)) => {
                        shutdown_id = Some(id);
                        break;
                    }
                    Ok((id, command)) => {
                        let key = next_key;
                        next_key += 1;
                        lock(&daemon.pending).insert(key, id);
                        let daemon = daemon.clone();
                        tasks.spawn(async move {
                            let result = daemon.clone().handle(command).await;
                            if let Some(id) = lock(&daemon.pending).remove(&key) {
                                daemon.reply(Some(&id), result);
                            }
                        });
                    }
                }
            }
            () = signals.recv() => break,
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
        }
        if out.stdout_broken() {
            break;
        }
    }
    tasks.shutdown().await;
    let unanswered: Vec<String> = lock(&daemon.pending).drain().map(|(_, id)| id).collect();
    for id in unanswered {
        daemon.reply(
            Some(&id),
            Err(Failure::new("internal", "daemon shutting down")),
        );
    }
    daemon.disconnect().await;
    if let Some(id) = shutdown_id {
        daemon.reply(Some(&id), Ok(Value::Null));
    }
    drop(lock(&daemon.writer).take());
    let _ = tokio::task::spawn_blocking(move || writing.join()).await;
    Ok(ExitCode::SUCCESS)
}

impl Daemon {
    fn write(&self, message: &Message<'_>) {
        let line = match serde_json::to_string(message) {
            Ok(line) => line,
            Err(e) => {
                tracing::error!("serializing a protocol message: {e}");
                return;
            }
        };
        if let Some(writer) = lock(&self.writer).as_ref() {
            let _ = writer.send(line);
        }
    }

    fn reply(&self, id: Option<&str>, result: Result<Value, Failure>) {
        let message = match (id, result) {
            (Some(id), Ok(data)) => Message::Result { id, data },
            (id, Ok(_)) => unreachable!("a result always answers a request with id {id:?}"),
            (id, Err(Failure { code, message })) => Message::Error { id, code, message },
        };
        self.write(&message);
    }

    async fn handle(self: Arc<Self>, command: Command) -> Result<Value, Failure> {
        match command {
            Command::Hello => Ok(json!({
                "protocol": PROTOCOL,
                "version": env!("CARGO_PKG_VERSION"),
                "platform": std::env::consts::OS,
            })),
            Command::SetToken { token } => self.set_token(token).await,
            Command::LoggedIn => {
                let logged_in = self
                    .ctx
                    .paths
                    .load_token()
                    .map_err(CliError::from)?
                    .is_some();
                Ok(json!({ "logged_in": logged_in }))
            }
            Command::Guilds => {
                let conn = self.conn().await?;
                let guilds = discord::guilds(&conn.client).await;
                Ok(guilds
                    .into_iter()
                    .map(|g| json!({ "id": g.id, "name": g.name }))
                    .collect())
            }
            Command::Channels { guild_id } => {
                let conn = self.conn().await?;
                let channels = discord::voice_channels(&conn.client, &guild_id).await?;
                Ok(channels
                    .into_iter()
                    .map(|c| json!({ "id": c.id, "name": c.name }))
                    .collect())
            }
            Command::Targets => {
                let (targets, failure) = capture::targets(self.opts.include_platform).await;
                if let Some(e) = failure {
                    return Err(CliError::from(e).into());
                }
                let rows: Vec<TargetRow> = targets.into_iter().map(Into::into).collect();
                serde_json::to_value(rows).map_err(|e| Failure::new("internal", e.to_string()))
            }
            Command::Start(params) => self.start(params).await,
            Command::Update(params) => self.update(params).await,
            Command::Stop => self.stop().await,
            Command::Shutdown => unreachable!("shutdown is handled by the read loop"),
        }
    }

    fn current(&self) -> Option<Arc<Conn>> {
        lock(&self.conn).clone()
    }

    async fn conn(self: &Arc<Self>) -> Result<Arc<Conn>, CliError> {
        if let Some(conn) = self.current() {
            return Ok(conn);
        }
        let _lifecycle = self.lifecycle.lock().await;
        self.ensure_conn().await
    }

    /// Needs `lifecycle` held.
    async fn ensure_conn(self: &Arc<Self>) -> Result<Arc<Conn>, CliError> {
        if let Some(conn) = self.current() {
            return Ok(conn);
        }
        let client = Arc::new(discord::connect(&self.ctx, Persistence::Long).await?);
        let session = Session::new(client.clone());
        let forwarder = tokio::spawn(self.clone().forward(session.subscribe()));
        let conn = Arc::new(Conn {
            client,
            session,
            forwarder,
        });
        *lock(&self.conn) = Some(conn.clone());
        Ok(conn)
    }

    async fn forward(self: Arc<Self>, mut events: broadcast::Receiver<SessionEvent>) {
        loop {
            match events.recv().await {
                Ok(event) => {
                    match &event {
                        SessionEvent::Health(h) => *lock(&self.last_stats) = Some(h.stats.clone()),
                        SessionEvent::State { .. } => self.remember_portal_token(),
                        _ => {}
                    }
                    self.write(&Message::Event { event: &event });
                }
                Err(RecvError::Lagged(n)) => tracing::warn!(skipped = n, "session events lagged"),
                Err(RecvError::Closed) => break,
            }
        }
    }

    fn remember_portal_token(&self) {
        if let Some(book) = lock(&self.portal).as_mut()
            && let Err(e) = book.save(&self.ctx.paths)
        {
            tracing::warn!("saving the portal restore token: {e}");
        }
    }

    async fn set_token(self: &Arc<Self>, token: String) -> Result<Value, Failure> {
        let token = token.trim().to_owned();
        if token.is_empty() {
            return Err(CliError::EmptyToken.into());
        }
        let _lifecycle = self.lifecycle.lock().await;
        if self.current().is_some_and(|c| c.live()) {
            return Err(Failure::new(
                "streaming",
                "stop the stream before changing the token",
            ));
        }
        let client = discord::connect_with(&self.ctx, token.clone(), Persistence::Brief).await?;
        client.close().await;
        self.ctx.paths.save_token(&token).map_err(CliError::from)?;
        self.disconnect_locked().await;
        Ok(Value::Null)
    }

    async fn start(self: &Arc<Self>, p: StartParams) -> Result<Value, Failure> {
        let config =
            dstreamy_core::Config::load(&self.ctx.paths.config_file).map_err(CliError::from)?;
        let codec = match &p.codec {
            Some(codec) => parse_codec(codec)
                .map_err(|e| invalid_params(format!("invalid codec {codec:?}: {e}")))?,
            None => config.stream.codec,
        };
        let settings = StreamConfig {
            resolution: p.resolution.unwrap_or(config.stream.resolution),
            fps: p.fps.unwrap_or(config.stream.fps),
            bitrate_kbps: p.bitrate_kbps.unwrap_or(config.stream.bitrate_kbps),
            codec,
            audio_gain: p.gain.unwrap_or(config.stream.audio_gain),
        };
        settings.validate().map_err(invalid_params)?;
        if let Some(crop) = p.crop {
            crop.validate().map_err(invalid_params)?;
        }
        let _lifecycle = self.lifecycle.lock().await;
        let conn = self.ensure_conn().await?;
        if conn.live() {
            return Err(CliError::AlreadyStreaming.into());
        }
        let channels = discord::voice_channels(&conn.client, &p.guild_id).await?;
        if !channels.iter().any(|c| c.id == p.channel_id) {
            return Err(CliError::UnknownChannel {
                guild: p.guild_id,
                channel: p.channel_id,
            }
            .into());
        }
        let restore_token = capture::restore_token(&config);
        let picked = capture::source_for(&p.target, restore_token.clone())?;
        let capture = stream::capture_config(&p.target, &settings, p.crop);
        stream::start(
            &conn.session,
            p.guild_id,
            p.channel_id,
            picked.source,
            capture,
        )
        .await?;
        *lock(&self.last_stats) = None;
        *lock(&self.portal) = Some(PortalBook::new(restore_token, picked.portal_token));
        *lock(&self.running) = Some(Running {
            target: p.target,
            stream: settings,
            crop: p.crop,
        });
        Ok(Value::Null)
    }

    async fn update(self: &Arc<Self>, p: UpdateParams) -> Result<Value, Failure> {
        if let Some(crop) = p.crop {
            crop.validate().map_err(invalid_params)?;
        }
        let conn = self
            .current()
            .filter(|c| c.live())
            .ok_or_else(not_running)?;
        let _updating = self.updating.lock().await;
        let (target, settings, crop) = {
            let running = lock(&self.running);
            let running = running.as_ref().ok_or_else(not_running)?;
            let settings = StreamConfig {
                resolution: p.resolution.unwrap_or(running.stream.resolution),
                fps: p.fps.unwrap_or(running.stream.fps),
                bitrate_kbps: p.bitrate_kbps.unwrap_or(running.stream.bitrate_kbps),
                ..running.stream
            };
            (running.target.clone(), settings, p.crop.or(running.crop))
        };
        settings.validate().map_err(invalid_params)?;
        let capture = stream::capture_config(&target, &settings, crop);
        conn.session
            .reconfigure(capture)
            .await
            .map_err(session_failure)?;
        if let Some(running) = lock(&self.running).as_mut() {
            running.stream = settings;
            running.crop = crop;
        }
        Ok(Value::Null)
    }

    async fn stop(self: &Arc<Self>) -> Result<Value, Failure> {
        let conn = self
            .current()
            .filter(|c| c.live())
            .ok_or_else(not_running)?;
        let stopped = conn.session.stop().await.map_err(session_failure)?;
        if stopped.is_none() {
            return Err(not_running());
        }
        *lock(&self.running) = None;
        let stats = lock(&self.last_stats).take().unwrap_or_default();
        serde_json::to_value(stats).map_err(|e| Failure::new("internal", e.to_string()))
    }

    async fn disconnect(&self) {
        let _lifecycle = self.lifecycle.lock().await;
        self.disconnect_locked().await;
    }

    /// Needs `lifecycle` held; the session's last events are written before it returns.
    async fn disconnect_locked(&self) {
        let Some(conn) = lock(&self.conn).take() else {
            return;
        };
        match Arc::try_unwrap(conn) {
            Ok(Conn {
                client,
                session,
                forwarder,
            }) => {
                stream::teardown(session, client).await;
                if timeout(FORWARDER_DRAIN, forwarder).await.is_err() {
                    tracing::debug!("session events still open; not waiting for the rest");
                }
            }
            Err(shared) => {
                let _ = shared.session.stop().await;
            }
        }
        *lock(&self.running) = None;
        self.remember_portal_token();
    }
}

fn session_failure(e: SessionError) -> Failure {
    match e {
        SessionError::NotRunning => not_running(),
        SessionError::CodecChange => invalid_params(e),
        SessionError::Stream(_) => Failure::new("capture", e.to_string()),
        _ => Failure::new("internal", e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(line: &str) -> (Option<String>, &'static str, String) {
        match parse(line.as_bytes()) {
            Err((id, f)) => (id, f.code, f.message),
            Ok(_) => panic!("{line} parsed"),
        }
    }

    #[test]
    fn lines_without_a_string_id_are_malformed_with_a_null_id() {
        for line in [
            "",
            "[]",
            "{\"cmd\":\"hello\"}",
            "{\"id\":1,\"cmd\":\"hello\"}",
        ] {
            let (id, code, _) = error(line);
            assert_eq!((id, code), (None, "malformed"), "{line}");
        }
        let (id, code, _) = error("{\"id\":\"a\"}");
        assert_eq!((id.as_deref(), code), (Some("a"), "malformed"));
    }

    #[test]
    fn invalid_utf8_is_malformed() {
        match parse(b"{\"id\":\"\xff\"}") {
            Err((None, f)) => assert_eq!(f.code, "malformed"),
            _ => panic!("invalid UTF-8 parsed"),
        }
    }

    #[test]
    fn unknown_commands_and_bad_fields_keep_the_id() {
        let (id, code, message) = error("{\"id\":\"a\",\"cmd\":\"bogus\"}");
        assert_eq!((id.as_deref(), code), (Some("a"), "unknown_cmd"));
        assert_eq!(message, "unknown command \"bogus\"");
        let (_, code, message) = error("{\"id\":\"b\",\"cmd\":\"update\",\"fps\":24}");
        assert_eq!(code, "invalid_params");
        assert_eq!(message, "invalid fps 24: expected 15, 30 or 60");
    }

    #[test]
    fn extra_fields_are_ignored() {
        let (id, command) = parse(b"{\"id\":\"x\",\"cmd\":\"stop\",\"later\":true}").unwrap();
        assert_eq!(id, "x");
        assert!(matches!(command, Command::Stop));
    }

    #[test]
    fn a_rejected_token_maps_to_invalid_token() {
        let f = Failure::from(CliError::Gateway(GatewayError::Rejected(AUTH_FAILED)));
        assert_eq!(f.code, "invalid_token");
        let f = Failure::from(CliError::Gateway(GatewayError::Exhausted(3)));
        assert_eq!(f.code, "gateway");
    }
}
