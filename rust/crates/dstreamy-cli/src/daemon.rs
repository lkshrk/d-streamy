//! `dstreamy daemon`: protocol 1 of ADR 0003, one JSON object per line on stdin and stdout.

use std::collections::HashMap;
use std::io::BufRead;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};

use dstreamy_discord::SessionEvent;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::engine::{Command, Engine, EngineEvent, Failure, invalid_params};
pub use crate::engine::{Options, PROTOCOL};
use crate::output::Out;
use crate::signals::Signals;
use crate::{CliError, Ctx};

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

struct Writer {
    lines: Mutex<Option<std::sync::mpsc::Sender<String>>>,
}

impl Writer {
    fn write(&self, message: &Message<'_>) {
        let line = match serde_json::to_string(message) {
            Ok(line) => line,
            Err(e) => {
                tracing::error!("serializing a protocol message: {e}");
                return;
            }
        };
        if let Some(writer) = lock(&self.lines).as_ref() {
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
    let writer = Arc::new(Writer {
        lines: Mutex::new(Some(writer)),
    });
    let engine = Engine::new(ctx, opts);
    let (drain, drained) = tokio::sync::oneshot::channel();
    let events = tokio::spawn(write_events(engine.events(), writer.clone(), drained));
    let pending: Arc<Mutex<HashMap<u64, String>>> = Arc::default();
    let mut signals = Signals::new();
    let mut tasks = JoinSet::new();
    let mut next_key = 0u64;
    let mut shutdown_id = None;
    loop {
        tokio::select! {
            line = lines.recv() => {
                let Some(line) = line else { break };
                match parse(&line) {
                    Err((id, failure)) => writer.reply(id.as_deref(), Err(failure)),
                    Ok((id, Command::Shutdown)) => {
                        shutdown_id = Some(id);
                        break;
                    }
                    Ok((id, command)) => {
                        let key = next_key;
                        next_key += 1;
                        lock(&pending).insert(key, id);
                        let engine = engine.clone();
                        let writer = writer.clone();
                        let pending = pending.clone();
                        tasks.spawn(async move {
                            let result = engine.handle(command).await;
                            if let Some(id) = lock(&pending).remove(&key) {
                                writer.reply(Some(&id), result);
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
    let unanswered: Vec<String> = lock(&pending).drain().map(|(_, id)| id).collect();
    for id in unanswered {
        writer.reply(
            Some(&id),
            Err(Failure::new("internal", "daemon shutting down")),
        );
    }
    engine.shutdown().await;
    let _ = drain.send(());
    let _ = events.await;
    if let Some(id) = shutdown_id {
        writer.reply(Some(&id), Ok(Value::Null));
    }
    drop(lock(&writer.lines).take());
    let _ = tokio::task::spawn_blocking(move || writing.join()).await;
    Ok(ExitCode::SUCCESS)
}

/// Writes engine events until `drain` fires, then the ones already queued.
async fn write_events(
    mut events: tokio::sync::broadcast::Receiver<EngineEvent>,
    writer: Arc<Writer>,
    mut drain: tokio::sync::oneshot::Receiver<()>,
) {
    let write = |event: EngineEvent| match event {
        EngineEvent::Session(event) => writer.write(&Message::Event { event: &event }),
    };
    loop {
        tokio::select! {
            biased;
            event = events.recv() => match event {
                Ok(event) => write(event),
                Err(RecvError::Lagged(n)) => tracing::warn!(skipped = n, "engine events lagged"),
                Err(RecvError::Closed) => return,
            },
            _ = &mut drain => break,
        }
    }
    loop {
        match events.try_recv() {
            Ok(event) => write(event),
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                tracing::warn!(skipped = n, "engine events lagged");
            }
            Err(_) => return,
        }
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
}
