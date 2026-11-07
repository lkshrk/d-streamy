//! `dstreamy daemon`: protocol 1 of ADR 0003, one JSON object per line on stdin and stdout.

use std::collections::HashMap;
use std::future::Future;
use std::io::BufRead;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use dstreamy_core::lock;
use dstreamy_discord::SessionEvent;
use dstreamy_ipc::control::{MAX_LINE, Malformed};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::engine::{Command, Engine, EngineEvent, Failure, invalid_params};
pub use crate::engine::{Options, PROTOCOL};
use crate::output::Out;
use crate::signals::Signals;
use crate::{CliError, Ctx};

/// How long the daemon waits for its stream to end after stdin closes or the first signal; the app kills it after.
pub const SHUTDOWN_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

const MAX_COMMANDS: usize = 64;
const MAX_TASKS: usize = 16;
const MAX_OUTPUT: usize = 64;

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

enum Line {
    Ok(Vec<u8>),
    Oversized,
}

fn read_lines(mut input: impl BufRead, lines: &mpsc::Sender<Line>) -> Result<(), &'static str> {
    loop {
        let mut line = Vec::new();
        let mut oversized = false;
        let ended = loop {
            let buf = match input.fill_buf() {
                Ok(buf) => buf,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break true,
            };
            if buf.is_empty() {
                break true;
            }
            let newline = buf.iter().position(|&b| b == b'\n');
            let chunk = &buf[..newline.unwrap_or(buf.len())];
            if !oversized && line.len() + chunk.len() > MAX_LINE {
                oversized = true;
                line = Vec::new();
            }
            if !oversized {
                line.extend_from_slice(chunk);
            }
            let consumed = chunk.len() + usize::from(newline.is_some());
            input.consume(consumed);
            if newline.is_some() {
                break false;
            }
        };
        if ended && line.is_empty() && !oversized {
            return Ok(());
        }
        let line = if oversized {
            Line::Oversized
        } else {
            Line::Ok(line)
        };
        lines.try_send(line).map_err(|_| "daemon command queue full or disconnected; send fewer outstanding commands and restart the daemon")?;
        if ended {
            return Ok(());
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
    mut events: mpsc::Receiver<EngineEvent>,
    mut replies: mpsc::Receiver<Reply>,
    out: std::sync::mpsc::SyncSender<String>,
) -> Result<(), &'static str> {
    let send = |line: Option<String>| -> Result<(), &'static str> {
        if let Some(line) = line {
            out.try_send(line).map_err(|_| "daemon stdout queue full or disconnected; read stdout continuously and restart the daemon")?;
        }
        Ok(())
    };
    let mut events_open = true;
    let mut replies_open = true;
    while events_open || replies_open {
        tokio::select! {
            biased;
            event = events.recv(), if events_open => match event {
                Some(event) => send(event_line(&event))?,
                None => {
                    if !replies.is_closed() {
                        return Err("daemon event subscription overflowed or disconnected; read stdout continuously and restart the daemon");
                    }
                    events_open = false;
                }
            },
            reply = replies.recv(), if replies_open => match reply {
                Some((id, result)) => {
                    while let Ok(event) = events.try_recv() {
                        send(event_line(&event))?;
                    }
                    send(reply_line(id.as_deref(), result))?;
                }
                None => replies_open = false,
            },
        }
    }
    Ok(())
}

fn spawn_command(
    tasks: &mut JoinSet<()>,
    command: impl Future<Output = ()> + Send + 'static,
) -> Result<(), &'static str> {
    if tasks.len() >= MAX_TASKS {
        return Err(
            "daemon concurrent command limit reached; wait for replies before sending more commands",
        );
    }
    tasks.spawn(command);
    Ok(())
}

pub async fn run(ctx: &Ctx, out: &Out, opts: Options) -> Result<ExitCode, CliError> {
    let (fault, mut fault_rx) = tokio::sync::watch::channel(None);
    let (lines_tx, mut lines) = mpsc::channel(MAX_COMMANDS);
    let input_fault = fault.clone();
    std::thread::spawn(move || {
        if let Err(message) = read_lines(std::io::stdin().lock(), &lines_tx) {
            input_fault.send_replace(Some(message));
        }
    });
    let (writer, written) = std::sync::mpsc::sync_channel::<String>(MAX_OUTPUT);
    let writer_out = out.clone();
    let writer_fault = fault.clone();
    let (writing_done, written_all) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        for line in written {
            writer_out.line(line);
            if writer_out.stdout_broken() {
                writer_fault.send_replace(Some(
                    "daemon stdout disconnected; keep stdout open and restart the daemon",
                ));
                break;
            }
        }
        let _ = writing_done.send(());
    });
    let engine = Engine::new(ctx, opts);
    let (replies, replies_rx) = mpsc::channel::<Reply>(MAX_OUTPUT);
    let mut output = tokio::spawn(write_out(engine.subscribe_lossless(), replies_rx, writer));
    let reply = |id: Option<String>, result| {
        if replies.try_send((id, result)).is_err() {
            fault.send_replace(Some("daemon reply queue full or disconnected; read stdout and send fewer outstanding commands"));
        }
    };
    let pending: Arc<Mutex<HashMap<u64, String>>> = Arc::default();
    let mut signals = Signals::new();
    let mut tasks = JoinSet::new();
    let mut next_key = 0u64;
    let mut shutdown_id = None;
    let mut failed = None;
    let mut output_done = false;
    loop {
        tokio::select! {
            biased;
            () = signals.recv() => break,
            changed = fault_rx.changed() => {
                if changed.is_ok() {
                    failed = *fault_rx.borrow_and_update();
                    break;
                }
            }
            result = &mut output => {
                output_done = true;
                failed = Some(match result {
                    Ok(Err(message)) => message,
                    _ => "daemon event delivery disconnected; restart the daemon",
                });
                break;
            }
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            line = lines.recv() => {
                let line = match line {
                    Some(Line::Ok(line)) => line,
                    Some(Line::Oversized) => {
                        let (id, failure) = malformed(None, &Malformed::Oversized.to_string());
                        reply(id, Err(failure));
                        continue;
                    }
                    None => break,
                };
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
                        let task_pending = pending.clone();
                        let fault = fault.clone();
                        if let Err(message) = spawn_command(&mut tasks, async move {
                            let result = engine.handle(command).await;
                            if let Some(id) = lock(&task_pending).remove(&key)
                                && replies.try_send((Some(id), result)).is_err()
                            {
                                fault.send_replace(Some("daemon reply queue full or disconnected; read stdout and send fewer outstanding commands"));
                            }
                        }) {
                            reply(lock(&pending).remove(&key), Err(Failure::new("overloaded", message)));
                            failed = Some(message);
                            break;
                        }
                    }
                }
            }
        }
        if out.stdout_broken() {
            break;
        }
    }
    if let Some(message) = failed {
        tracing::error!("{message}");
        out.error(message);
    }
    drop(lines);
    let shutdown_failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutdown_fault = shutdown_failed.clone();
    let stopped = async {
        tasks.shutdown().await;
        let unanswered: Vec<String> = lock(&pending).drain().map(|(_, id)| id).collect();
        for id in unanswered {
            if replies
                .try_send((
                    Some(id),
                    Err(Failure::new("internal", "daemon shutting down")),
                ))
                .is_err()
            {
                shutdown_fault.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        engine.shutdown().await;
        if let Some(id) = shutdown_id
            && replies.try_send((Some(id), Ok(Value::Null))).is_err()
        {
            shutdown_fault.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        drop(replies);
        drop(engine);
        if !output_done {
            match output.await {
                Ok(Ok(())) => {}
                result => {
                    tracing::error!(
                        ?result,
                        "daemon output failed during shutdown; read stdout continuously and restart the daemon"
                    );
                    shutdown_fault.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
        let _ = written_all.await;
    };
    if cut_short(&mut signals, stopped, SHUTDOWN_LIMIT).await {
        tracing::warn!("exiting without a finished shutdown");
        return Ok(ExitCode::from(crate::stream::FORCED_EXIT));
    }
    Ok(
        if failed.is_some() || shutdown_failed.load(std::sync::atomic::Ordering::Relaxed) {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        },
    )
}

/// Whether `stopped` had to be abandoned: another signal arrived, or `limit` passed.
async fn cut_short(
    signals: &mut Signals,
    stopped: impl Future<Output = ()>,
    limit: std::time::Duration,
) -> bool {
    let stopped = tokio::time::timeout(limit, stopped);
    tokio::select! {
        biased;
        () = signals.recv() => true,
        finished = stopped => finished.is_err(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn live_command_tasks_have_a_fixed_bound() {
        let mut tasks = JoinSet::new();
        for index in 0..1_000 {
            let admitted = spawn_command(&mut tasks, std::future::pending::<()>());
            assert_eq!(admitted.is_ok(), index < MAX_TASKS);
        }
        assert!(
            tasks.len() <= MAX_TASKS,
            "{} live command tasks",
            tasks.len()
        );
    }

    #[test]
    fn stdin_burst_has_a_fixed_resident_queue_bound() {
        let (tx, rx) = mpsc::channel(MAX_COMMANDS);
        assert!(read_lines(std::io::Cursor::new(b"hello\n".repeat(1_000)), &tx).is_err());
        assert!(rx.len() <= 64, "queued {} commands", rx.len());
    }

    #[tokio::test]
    async fn unread_stdout_has_a_fixed_resident_queue_bound() {
        let (events, event_rx) = mpsc::channel(MAX_OUTPUT);
        let (replies, reply_rx) = mpsc::channel(1_000);
        let (out, lines) = std::sync::mpsc::sync_channel(MAX_OUTPUT);
        for i in 0..1_000 {
            replies
                .try_send((Some(i.to_string()), Ok(Value::Null)))
                .unwrap();
        }
        drop((events, replies));
        assert!(
            write_out(event_rx, reply_rx, out)
                .await
                .unwrap_err()
                .contains("read stdout")
        );
        assert!(lines.try_iter().count() <= 64, "unbounded stdout backlog");
    }

    #[tokio::test]
    async fn a_shutdown_is_cut_short_by_a_signal_or_the_limit() {
        let limit = std::time::Duration::from_millis(100);
        let (tx, rx) = mpsc::unbounded_channel();
        let mut signals = Signals::from_channel(rx);
        assert!(!cut_short(&mut signals, async {}, limit).await);
        let started = std::time::Instant::now();
        assert!(cut_short(&mut signals, std::future::pending(), limit).await);
        assert!(started.elapsed() >= limit);
        tx.send(()).unwrap();
        assert!(cut_short(&mut signals, std::future::pending(), limit).await);
    }

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

    fn lines_of(input: &[u8]) -> Vec<Option<Vec<u8>>> {
        let (tx, mut rx) = mpsc::channel(MAX_COMMANDS);
        read_lines(std::io::Cursor::new(input), &tx).unwrap();
        drop(tx);
        let mut lines = Vec::new();
        while let Ok(line) = rx.try_recv() {
            lines.push(match line {
                Line::Ok(line) => Some(line),
                Line::Oversized => None,
            });
        }
        lines
    }

    #[test]
    fn lines_are_split_at_newlines_and_the_last_one_needs_none() {
        assert_eq!(
            lines_of(b"a\n\nbc\r\nlast"),
            [
                Some(b"a".to_vec()),
                Some(Vec::new()),
                Some(b"bc\r".to_vec()),
                Some(b"last".to_vec()),
            ]
        );
        assert_eq!(lines_of(b""), []);
        assert_eq!(lines_of(b"\n"), [Some(Vec::new())]);
    }

    #[test]
    fn a_line_past_the_limit_is_dropped_while_read_and_the_next_one_arrives() {
        let mut input = vec![b'x'; MAX_LINE + 1];
        input.extend_from_slice(b"\nok\n");
        assert_eq!(lines_of(&input), [None, Some(b"ok".to_vec())]);
        let exact = vec![b'y'; MAX_LINE];
        assert_eq!(lines_of(&exact), [Some(exact.clone())]);
        let unterminated = vec![b'z'; MAX_LINE + 1];
        assert_eq!(lines_of(&unterminated), [None]);
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
        let (event_tx, event_rx) = mpsc::channel(20_000);
        let (reply_tx, reply_rx) = mpsc::channel(20_000);
        let (out, lines) = std::sync::mpsc::sync_channel(20_001);
        for event in events {
            event_tx.try_send(event).unwrap();
        }
        for reply in replies {
            reply_tx.try_send(reply).unwrap();
        }
        drop(reply_tx);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let writer = tokio::spawn(write_out(event_rx, reply_rx, out));
            tokio::task::yield_now().await;
            for event in late {
                event_tx.try_send(event).unwrap();
            }
            drop(event_tx);
            writer.await.unwrap().unwrap();
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
        let (event_tx, event_rx) = mpsc::channel(20_000);
        let (reply_tx, reply_rx) = mpsc::channel::<Reply>(20_000);
        let (out, lines) = std::sync::mpsc::sync_channel(20_001);
        let writer = runtime.spawn(write_out(event_rx, reply_rx, out));
        for i in 0..N {
            event_tx.try_send(resumed()).unwrap();
            reply_tx
                .try_send((Some(i.to_string()), Ok(Value::Null)))
                .unwrap();
            let event = lines.recv().unwrap();
            assert!(
                event.contains("\"event\""),
                "reply {i} came before its event"
            );
            assert!(lines.recv().unwrap().contains(&format!("\"id\":\"{i}\"")));
        }
        drop(reply_tx);
        drop(event_tx);
        runtime.block_on(writer).unwrap().unwrap();
    }
}
