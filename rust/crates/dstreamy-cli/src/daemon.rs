//! `dstreamy daemon`: protocol 1 of ADR 0003, one JSON object per line on stdin and stdout.

use std::collections::HashMap;
use std::io::BufRead;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};

use dstreamy_discord::SessionEvent;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::engine::{Command, Engine, EngineEvent, Failure, invalid_params};
pub use crate::engine::{Options, PROTOCOL};
use crate::output::Out;
use crate::signals::Signals;
use crate::{CliError, Ctx};

pub(crate) const COMMANDS: [&str; 10] = [
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

enum Request {
    Shutdown,
    Engine(Command),
}

pub(crate) fn malformed(id: Option<String>, what: &str) -> (Option<String>, Failure) {
    (
        id,
        Failure::new("malformed", format!("malformed line: {what}")),
    )
}

/// The request's `id` and `cmd`, checked against `known` as protocol 1 checks them.
pub(crate) fn id_and_cmd(
    value: &Value,
    known: &[&str],
) -> Result<(String, String), (Option<String>, Failure)> {
    let Value::Object(fields) = value else {
        return Err(malformed(None, "expected a JSON object"));
    };
    let Some(id) = fields.get("id").and_then(Value::as_str).map(str::to_owned) else {
        return Err(malformed(None, "missing string field \"id\""));
    };
    let Some(cmd) = fields.get("cmd").and_then(Value::as_str) else {
        return Err(malformed(Some(id), "missing string field \"cmd\""));
    };
    if !known.contains(&cmd) {
        let failure = Failure::new("unknown_cmd", format!("unknown command {cmd:?}"));
        return Err((Some(id), failure));
    }
    Ok((id, cmd.to_owned()))
}

pub(crate) fn engine_command(
    id: String,
    value: Value,
) -> Result<(String, Command), (Option<String>, Failure)> {
    match serde_json::from_value(value) {
        Ok(command) => Ok((id, command)),
        Err(e) => Err((Some(id), invalid_params(e))),
    }
}

fn parse(line: &[u8]) -> Result<(String, Request), (Option<String>, Failure)> {
    let value: Value = serde_json::from_slice(line).map_err(|e| malformed(None, &e.to_string()))?;
    let (id, cmd) = id_and_cmd(&value, &COMMANDS)?;
    if cmd == "shutdown" {
        return Ok((id, Request::Shutdown));
    }
    let (id, command) = engine_command(id, value)?;
    Ok((id, Request::Engine(command)))
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

type Reply = (Option<String>, Result<Value, Failure>);

fn line(message: &Message<'_>) -> Option<String> {
    serde_json::to_string(message)
        .inspect_err(|e| tracing::error!("serializing a protocol message: {e}"))
        .ok()
}

fn reply_line(id: Option<&str>, result: Result<Value, Failure>) -> Option<String> {
    let message = match (id, result) {
        (Some(id), Ok(data)) => Message::Result { id, data },
        (id, Ok(_)) => unreachable!("a result always answers a request with id {id:?}"),
        (id, Err(Failure { code, message })) => Message::Error { id, code, message },
    };
    line(&message)
}

fn event_line(event: &EngineEvent) -> Option<String> {
    match event {
        EngineEvent::Session(event) => line(&Message::Event { event }),
        EngineEvent::Source(_) => None,
    }
}

/// Until both channels close; every event sent before a reply is written before it.
async fn write_out(
    mut events: mpsc::UnboundedReceiver<EngineEvent>,
    mut replies: mpsc::UnboundedReceiver<Reply>,
    out: std::sync::mpsc::Sender<String>,
) {
    let send = |line: Option<String>| {
        if let Some(line) = line {
            let _ = out.send(line);
        }
    };
    let mut events_open = true;
    let mut replies_open = true;
    while events_open || replies_open {
        tokio::select! {
            biased;
            event = events.recv(), if events_open => match event {
                Some(event) => send(event_line(&event)),
                None => events_open = false,
            },
            reply = replies.recv(), if replies_open => match reply {
                Some((id, result)) => {
                    while let Ok(event) = events.try_recv() {
                        send(event_line(&event));
                    }
                    send(reply_line(id.as_deref(), result));
                }
                None => replies_open = false,
            },
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
    let engine = Engine::new(ctx, opts);
    let (replies, replies_rx) = mpsc::unbounded_channel::<Reply>();
    let output = tokio::spawn(write_out(engine.subscribe_lossless(), replies_rx, writer));
    let reply = |id: Option<String>, result| {
        let _ = replies.send((id, result));
    };
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
                    Err((id, failure)) => reply(id, Err(failure)),
                    Ok((id, Request::Shutdown)) => {
                        shutdown_id = Some(id);
                        break;
                    }
                    Ok((id, Request::Engine(command))) => {
                        let key = next_key;
                        next_key += 1;
                        lock(&pending).insert(key, id);
                        let engine = engine.clone();
                        let replies = replies.clone();
                        let pending = pending.clone();
                        tasks.spawn(async move {
                            let result = engine.handle(command).await;
                            if let Some(id) = lock(&pending).remove(&key) {
                                let _ = replies.send((Some(id), result));
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
        reply(
            Some(id),
            Err(Failure::new("internal", "daemon shutting down")),
        );
    }
    engine.shutdown().await;
    if let Some(id) = shutdown_id {
        reply(Some(id), Ok(Value::Null));
    }
    drop(replies);
    drop(engine);
    let _ = output.await;
    let _ = tokio::task::spawn_blocking(move || writing.join()).await;
    Ok(ExitCode::SUCCESS)
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
        assert!(matches!(command, Request::Engine(Command::Stop)));
    }

    fn written(
        events: Vec<EngineEvent>,
        replies: Vec<Reply>,
        late: Vec<EngineEvent>,
    ) -> Vec<Value> {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (reply_tx, reply_rx) = mpsc::unbounded_channel();
        let (out, lines) = std::sync::mpsc::channel();
        for event in events {
            event_tx.send(event).unwrap();
        }
        for reply in replies {
            reply_tx.send(reply).unwrap();
        }
        drop(reply_tx);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let writer = tokio::spawn(write_out(event_rx, reply_rx, out));
            tokio::task::yield_now().await;
            for event in late {
                event_tx.send(event).unwrap();
            }
            drop(event_tx);
            writer.await.unwrap();
        });
        lines
            .into_iter()
            .map(|l| serde_json::from_str(&l).unwrap())
            .collect()
    }

    fn resumed() -> EngineEvent {
        EngineEvent::Session(SessionEvent::Resumed)
    }

    #[test]
    fn a_flood_of_events_queued_before_the_writer_runs_is_written_in_full_before_the_reply() {
        let lines = written(
            vec![resumed(); 10_000],
            vec![(Some("1".into()), Ok(Value::Null))],
            vec![],
        );
        assert_eq!(lines.len(), 10_001);
        assert!(lines[..10_000].iter().all(|l| l["type"] == "event"));
        assert_eq!(lines[10_000]["id"], "1");
    }

    #[test]
    fn events_after_the_last_reply_are_written_until_the_engine_closes_them() {
        let lines = written(
            vec![],
            vec![(Some("1".into()), Ok(Value::Null))],
            vec![resumed(); 3],
        );
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0]["id"], "1");
        assert!(lines[1..].iter().all(|l| l["type"] == "event"));
    }

    #[test]
    fn an_event_sent_just_before_a_reply_is_written_first() {
        const N: usize = 20_000;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .build()
            .unwrap();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (reply_tx, reply_rx) = mpsc::unbounded_channel::<Reply>();
        let (out, lines) = std::sync::mpsc::channel();
        let writer = runtime.spawn(write_out(event_rx, reply_rx, out));
        for i in 0..N {
            event_tx.send(resumed()).unwrap();
            reply_tx
                .send((Some(i.to_string()), Ok(Value::Null)))
                .unwrap();
            let event = lines.recv().unwrap();
            assert!(
                event.contains("\"event\""),
                "reply {i} came before its event"
            );
            assert!(lines.recv().unwrap().contains(&format!("\"id\":\"{i}\"")));
        }
        drop((event_tx, reply_tx));
        runtime.block_on(writer).unwrap();
    }
}
