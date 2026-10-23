//! `dstreamy serve`: the command engine behind a local socket, for any number of control clients (ADR 0004).

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use dstreamy_core::Config;
use dstreamy_discord::SessionEvent;
use dstreamy_ipc::{ControlCodec, Endpoint, Listener, Stream};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{ReadHalf, WriteHalf};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio_util::codec::{Encoder, FramedRead, FramedWrite};

use crate::daemon::{COMMANDS, engine_command, id_and_cmd, malformed};
use crate::engine::{self, Engine, EngineEvent, Failure, SourceView};
use crate::output::Out;
use crate::signals::Signals;
use crate::stream::FORCED_EXIT;
use crate::{CliError, Ctx};

pub const PROTOCOL: u64 = 2;
/// How long a lagged subscriber gets to read its `lagged` error before the connection closes.
const LAG_FLUSH: Duration = Duration::from_secs(10);
const CLOSE_FLUSH: Duration = Duration::from_secs(2);
const ACCEPT_RETRY: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct Options {
    pub endpoint: Option<PathBuf>,
    pub include_platform: bool,
    pub event_capacity: usize,
    /// Enables the hidden `test_events` command.
    pub test_hooks: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Running,
    Stopping,
    Closed,
}

pub async fn run(ctx: &Ctx, out: &Out, opts: Options) -> Result<ExitCode, CliError> {
    let config = Config::load(&ctx.paths.config_file)?;
    engine::external_options(&config.serve)?;
    let endpoint = match &opts.endpoint {
        Some(path) => Endpoint::new(path),
        None => Endpoint::default_for_user()?,
    };
    let listener = Listener::bind(&endpoint).await?;
    out.note(format_args!("listening on {endpoint}"));
    let engine = Engine::new(
        ctx,
        engine::Options {
            include_platform: opts.include_platform,
            event_capacity: opts.event_capacity,
            allow_external: true,
        },
    );
    let (phase_tx, phase) = watch::channel(Phase::Running);
    let (shutdown_tx, mut shutdown_rx) = mpsc::unbounded_channel::<oneshot::Sender<()>>();
    let mut signals = Signals::new();
    let mut connections = JoinSet::new();
    let mut next_id = 0u64;
    let caller = loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    next_id += 1;
                    let conn = Connection {
                        id: next_id,
                        engine: engine.clone(),
                        phase: phase.clone(),
                        shutdown: shutdown_tx.clone(),
                        capacity: opts.event_capacity,
                        test_hooks: opts.test_hooks,
                    };
                    connections.spawn(conn.run(stream));
                }
                Err(e) => {
                    tracing::warn!("accepting a connection: {e}");
                    tokio::time::sleep(ACCEPT_RETRY).await;
                }
            },
            Some(done) = shutdown_rx.recv() => break Some(done),
            () = signals.recv() => break None,
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    };
    drop(listener);
    phase_tx.send_replace(Phase::Stopping);
    let forced = tokio::select! {
        biased;
        () = signals.recv() => true,
        () = engine.shutdown() => false,
    };
    if forced {
        return Ok(ExitCode::from(FORCED_EXIT));
    }
    if let Some(done) = caller {
        let _ = done.send(());
    }
    phase_tx.send_replace(Phase::Closed);
    let drained = timeout(CLOSE_FLUSH * 2, async {
        while connections.join_next().await.is_some() {}
    });
    if drained.await.is_err() {
        tracing::debug!("connections still open at exit");
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Message {
    Result {
        id: String,
        data: Value,
    },
    Error {
        id: Option<String>,
        code: &'static str,
        message: String,
    },
    Event {
        event: Value,
    },
}

impl Message {
    fn reply(id: Option<String>, result: Result<Value, Failure>) -> Self {
        match (id, result) {
            (Some(id), Ok(data)) => Self::Result { id, data },
            (id, Ok(_)) => unreachable!("a result always answers a request with id {id:?}"),
            (id, Err(Failure { code, message })) => Self::Error { id, code, message },
        }
    }

    fn event(event: &EngineEvent) -> Self {
        let event = match event {
            EngineEvent::Session(event) => serde_json::to_value(event),
            EngineEvent::Source(state) => {
                serde_json::to_value(SourceView::from(state)).map(|mut v| {
                    v["event"] = json!("source");
                    v
                })
            }
        };
        Self::Event {
            event: event.unwrap_or_else(|e| json!({ "event": "error", "message": e.to_string() })),
        }
    }
}

enum Role {
    Control,
}

fn hello_data(engine: &Engine, connection: u64) -> Value {
    json!({
        "protocol": PROTOCOL,
        "version": env!("CARGO_PKG_VERSION"),
        "platform": std::env::consts::OS,
        "connection": connection,
        "session": engine.status(),
    })
}

fn handshake(value: &Value) -> Result<(String, Role), (Option<String>, Failure)> {
    let Value::Object(fields) = value else {
        return Err(malformed(None, "expected a JSON object"));
    };
    let Some(id) = fields.get("id").and_then(Value::as_str).map(str::to_owned) else {
        return Err(malformed(None, "missing string field \"id\""));
    };
    if fields.get("cmd").and_then(Value::as_str) != Some("hello") {
        return Err(malformed(Some(id), "the first line must be hello"));
    }
    let Some(protocol) = fields.get("protocol").and_then(Value::as_u64) else {
        return Err(malformed(Some(id), "missing integer field \"protocol\""));
    };
    if protocol > PROTOCOL {
        let message =
            format!("protocol {protocol} is not supported; this service speaks {PROTOCOL}");
        return Err((Some(id), Failure::new("unsupported_protocol", message)));
    }
    match fields.get("role").and_then(Value::as_str) {
        Some("control") => Ok((id, Role::Control)),
        Some("producer") => Err((
            Some(id),
            Failure::new(
                "invalid_params",
                "role \"producer\" is not available in this version",
            ),
        )),
        Some(role) => Err(malformed(Some(id), &format!("unknown role {role:?}"))),
        None => Err(malformed(Some(id), "missing string field \"role\"")),
    }
}

type Reader = FramedRead<ReadHalf<Stream>, ControlCodec>;
type Writer = FramedWrite<WriteHalf<Stream>, ControlCodec>;

enum End {
    Eof,
    Gone,
    Lagged,
    Closed,
}

struct Connection {
    id: u64,
    engine: Arc<Engine>,
    phase: watch::Receiver<Phase>,
    shutdown: mpsc::UnboundedSender<oneshot::Sender<()>>,
    capacity: usize,
    test_hooks: bool,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Encodes `message` into the writer's buffer, or an `internal` error when it can't be encoded.
fn queue(writer: &mut Writer, message: Message) {
    let id = match &message {
        Message::Result { id, .. } => Some(id.clone()),
        Message::Error { id, .. } => id.clone(),
        Message::Event { .. } => None,
    };
    if let Err(e) = ControlCodec::new().encode(&message, writer.write_buffer_mut()) {
        tracing::error!("encoding a control message: {e}");
        let fallback = Message::Error {
            id,
            code: "internal",
            message: format!("the reply could not be encoded: {e}"),
        };
        let _ = ControlCodec::new().encode(&fallback, writer.write_buffer_mut());
    }
}

async fn flush_within(writer: &mut Writer, limit: Duration) {
    let _ = timeout(limit, SinkExt::<&Message>::flush(writer)).await;
}

impl Connection {
    async fn run(mut self, stream: Stream) {
        let (read, write) = tokio::io::split(stream);
        let mut reader = FramedRead::new(read, ControlCodec::new());
        let mut writer = FramedWrite::new(write, ControlCodec::new());
        let first = tokio::select! {
            first = reader.next() => first,
            _ = self.phase.wait_for(|p| *p != Phase::Running) => return,
        };
        let Some(Ok(first)) = first else { return };
        let hello = first
            .map_err(|e| malformed(None, &e.to_string()))
            .and_then(|value| handshake(&value));
        match hello {
            Ok((id, Role::Control)) => {
                queue(
                    &mut writer,
                    Message::reply(Some(id), Ok(hello_data(&self.engine, self.id))),
                );
            }
            Err((id, failure)) => {
                queue(&mut writer, Message::reply(id, Err(failure)));
                flush_within(&mut writer, CLOSE_FLUSH).await;
                return;
            }
        }
        self.control(reader, writer).await;
    }

    async fn control(&mut self, mut reader: Reader, mut writer: Writer) {
        let mut events: Option<broadcast::Receiver<EngineEvent>> = None;
        let mut pending: VecDeque<Message> = VecDeque::new();
        let mut queued_events = 0usize;
        let (replies_tx, mut replies) =
            mpsc::unbounded_channel::<(String, Result<Value, Failure>)>();
        let unanswered: Arc<Mutex<HashMap<u64, String>>> = Arc::default();
        let mut tasks = JoinSet::new();
        let mut next_key = 0u64;
        let mut reading = true;
        let mut stopping = *self.phase.borrow() != Phase::Running;
        let mut shutdown: Option<(String, oneshot::Receiver<()>)> = None;
        let end = loop {
            if writer.write_buffer().is_empty()
                && let Some(message) = pending.pop_front()
            {
                if matches!(message, Message::Event { .. }) {
                    queued_events -= 1;
                }
                queue(&mut writer, message);
            }
            let unflushed = !writer.write_buffer().is_empty();
            if !reading
                && tasks.is_empty()
                && shutdown.is_none()
                && pending.is_empty()
                && !unflushed
            {
                break End::Eof;
            }
            tokio::select! {
                biased;
                changed = self.phase.changed() => {
                    let phase = if changed.is_ok() { *self.phase.borrow() } else { Phase::Closed };
                    if phase == Phase::Closed {
                        break End::Closed;
                    }
                    if !stopping {
                        stopping = true;
                        tasks.abort_all();
                        for (_, id) in lock(&unanswered).drain() {
                            let failure = Failure::new("internal", "service shutting down");
                            pending.push_back(Message::reply(Some(id), Err(failure)));
                        }
                    }
                }
                flushed = SinkExt::<&Message>::flush(&mut writer), if unflushed => {
                    if flushed.is_err() {
                        break End::Gone;
                    }
                }
                event = recv(&mut events), if events.is_some() => match event {
                    Ok(event) => {
                        if queued_events >= self.capacity {
                            break End::Lagged;
                        }
                        pending.push_back(Message::event(&event));
                        queued_events += 1;
                    }
                    Err(RecvError::Lagged(_)) => break End::Lagged,
                    Err(RecvError::Closed) => events = None,
                },
                Some((id, result)) = replies.recv() => {
                    pending.push_back(Message::reply(Some(id), result));
                }
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
                _ = wait(&mut shutdown), if shutdown.is_some() => {
                    if let Some((id, _)) = shutdown.take() {
                        pending.push_back(Message::reply(Some(id), Ok(Value::Null)));
                    }
                }
                line = reader.next(), if reading => match line {
                    None | Some(Err(_)) => reading = false,
                    Some(Ok(Err(e))) => {
                        let (id, failure) = malformed(None, &e.to_string());
                        pending.push_back(Message::reply(id, Err(failure)));
                    }
                    Some(Ok(Ok(value))) => {
                        let request = Request::parse(value, self.test_hooks);
                        match request {
                            Err((id, failure)) => pending.push_back(Message::reply(id, Err(failure))),
                            Ok((id, _)) if stopping => {
                                let failure = Failure::new("internal", "service shutting down");
                                pending.push_back(Message::reply(Some(id), Err(failure)));
                            }
                            Ok((id, Request::Hello)) => {
                                pending.push_back(Message::reply(Some(id), Ok(hello_data(&self.engine, self.id))));
                            }
                            Ok((id, Request::Status)) => {
                                let status = serde_json::to_value(self.engine.status())
                                    .map_err(|e| Failure::new("internal", e.to_string()));
                                pending.push_back(Message::reply(Some(id), status));
                            }
                            Ok((id, Request::Subscribe)) => {
                                events.get_or_insert_with(|| self.engine.events());
                                pending.push_back(Message::reply(Some(id), Ok(Value::Null)));
                            }
                            Ok((id, Request::Shutdown)) => {
                                let (done, rx) = oneshot::channel();
                                let _ = self.shutdown.send(done);
                                if let Some((earlier, _)) = shutdown.replace((id, rx)) {
                                    pending.push_back(Message::reply(Some(earlier), Ok(Value::Null)));
                                }
                            }
                            Ok((id, Request::TestEvents(spec))) => {
                                let key = next_key;
                                next_key += 1;
                                lock(&unanswered).insert(key, id);
                                let engine = self.engine.clone();
                                let replies = replies_tx.clone();
                                let unanswered = unanswered.clone();
                                tasks.spawn(async move {
                                    spec.emit(&engine).await;
                                    if let Some(id) = lock(&unanswered).remove(&key) {
                                        let _ = replies.send((id, Ok(Value::Null)));
                                    }
                                });
                            }
                            Ok((id, Request::Engine(command))) => {
                                let key = next_key;
                                next_key += 1;
                                lock(&unanswered).insert(key, id);
                                let engine = self.engine.clone();
                                let replies = replies_tx.clone();
                                let unanswered = unanswered.clone();
                                let caller = self.id;
                                tasks.spawn(async move {
                                    let result = engine.handle_as(command, Some(caller)).await;
                                    if let Some(id) = lock(&unanswered).remove(&key) {
                                        let _ = replies.send((id, result));
                                    }
                                });
                            }
                        }
                    }
                },
            }
        };
        match end {
            End::Eof | End::Gone => tasks.detach_all(),
            End::Lagged => {
                tasks.detach_all();
                let failure = Failure::new("lagged", "events were not read in time; disconnecting");
                queue(&mut writer, Message::reply(None, Err(failure)));
                flush_within(&mut writer, LAG_FLUSH).await;
            }
            End::Closed => {
                if let Some(events) = events.as_mut() {
                    while let Ok(event) = events.try_recv() {
                        pending.push_back(Message::event(&event));
                    }
                }
                tasks.abort_all();
                while let Ok((id, result)) = replies.try_recv() {
                    pending.push_back(Message::reply(Some(id), result));
                }
                for (_, id) in lock(&unanswered).drain() {
                    let failure = Failure::new("internal", "service shutting down");
                    pending.push_back(Message::reply(Some(id), Err(failure)));
                }
                if let Some((id, _)) = shutdown.take() {
                    pending.push_back(Message::reply(Some(id), Ok(Value::Null)));
                }
                for message in pending {
                    queue(&mut writer, message);
                }
                flush_within(&mut writer, CLOSE_FLUSH).await;
            }
        }
    }
}

async fn recv(
    events: &mut Option<broadcast::Receiver<EngineEvent>>,
) -> Result<EngineEvent, RecvError> {
    match events {
        Some(events) => events.recv().await,
        None => std::future::pending().await,
    }
}

async fn wait(shutdown: &mut Option<(String, oneshot::Receiver<()>)>) {
    match shutdown {
        Some((_, rx)) => {
            let _ = rx.await;
        }
        None => std::future::pending().await,
    }
}

enum Request {
    Hello,
    Status,
    Subscribe,
    Shutdown,
    TestEvents(TestEvents),
    Engine(engine::Command),
}

impl Request {
    fn parse(value: Value, test_hooks: bool) -> Result<(String, Self), (Option<String>, Failure)> {
        let mut known: Vec<&str> = COMMANDS.to_vec();
        known.extend(["status", "subscribe"]);
        if test_hooks {
            known.push("test_events");
        }
        let (id, cmd) = id_and_cmd(&value, &known)?;
        let request = match cmd.as_str() {
            "hello" => Self::Hello,
            "status" => Self::Status,
            "subscribe" => Self::Subscribe,
            "shutdown" => Self::Shutdown,
            "test_events" => match serde_json::from_value(value) {
                Ok(spec) => Self::TestEvents(spec),
                Err(e) => return Err((Some(id), engine::invalid_params(e))),
            },
            _ => {
                let (id, command) = engine_command(id, value)?;
                return Ok((id, Self::Engine(command)));
            }
        };
        Ok((id, request))
    }
}

/// `count` non-fatal error events of about `bytes` each, `interval_ms` apart.
#[derive(serde::Deserialize)]
struct TestEvents {
    count: u32,
    #[serde(default)]
    bytes: usize,
    #[serde(default)]
    interval_ms: u64,
}

impl TestEvents {
    async fn emit(&self, engine: &Engine) {
        let padding = "x".repeat(self.bytes);
        for n in 0..self.count {
            engine.inject_for_tests(SessionEvent::Error {
                message: format!("test event {n} {padding}"),
                fatal: false,
            });
            tokio::time::sleep(Duration::from_millis(self.interval_ms)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejected(line: &str) -> (Option<String>, &'static str) {
        match handshake(&serde_json::from_str(line).unwrap()) {
            Err((id, f)) => (id, f.code),
            Ok(_) => panic!("{line} was accepted"),
        }
    }

    #[test]
    fn the_handshake_checks_the_protocol_before_the_role() {
        let ok = r#"{"id":"1","cmd":"hello","role":"control","protocol":2}"#;
        assert!(handshake(&serde_json::from_str(ok).unwrap()).is_ok());
        let older = r#"{"id":"1","cmd":"hello","role":"control","protocol":1}"#;
        assert!(handshake(&serde_json::from_str(older).unwrap()).is_ok());
        for (line, id, code) in [
            (
                r#"{"id":"1","cmd":"hello","role":"bogus","protocol":3}"#,
                Some("1"),
                "unsupported_protocol",
            ),
            (
                r#"{"id":"1","cmd":"hello","protocol":2}"#,
                Some("1"),
                "malformed",
            ),
            (
                r#"{"id":"1","cmd":"hello","role":"viewer","protocol":2}"#,
                Some("1"),
                "malformed",
            ),
            (
                r#"{"id":"1","cmd":"hello","role":"control"}"#,
                Some("1"),
                "malformed",
            ),
            (
                r#"{"id":"1","cmd":"status","role":"control","protocol":2}"#,
                Some("1"),
                "malformed",
            ),
            (
                r#"{"cmd":"hello","role":"control","protocol":2}"#,
                None,
                "malformed",
            ),
            (r#"[]"#, None, "malformed"),
            (
                r#"{"id":"1","cmd":"hello","role":"producer","protocol":2}"#,
                Some("1"),
                "invalid_params",
            ),
        ] {
            let (got_id, got_code) = rejected(line);
            assert_eq!((got_id.as_deref(), got_code), (id, code), "{line}");
        }
    }

    #[test]
    fn test_events_exist_only_with_the_hook() {
        let line = json!({"id": "t", "cmd": "test_events", "count": 1});
        assert!(matches!(
            Request::parse(line.clone(), false),
            Err((
                Some(_),
                Failure {
                    code: "unknown_cmd",
                    ..
                }
            ))
        ));
        assert!(matches!(
            Request::parse(line, true),
            Ok((_, Request::TestEvents(_)))
        ));
    }

    #[test]
    fn a_source_event_carries_its_state() {
        let Message::Event { event } = Message::event(&EngineEvent::Source(
            dstreamy_media::SourceState::Producing { attachment: 3 },
        )) else {
            panic!("not an event");
        };
        assert_eq!(
            event,
            json!({"event": "source", "state": "producing", "attachment": 3})
        );
    }
}
