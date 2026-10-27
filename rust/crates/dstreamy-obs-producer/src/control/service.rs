use std::os::unix::fs::MetadataExt;
use std::time::Duration;

use dstreamy_ipc::{ControlCodec, Endpoint, IpcError};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{ReadHalf, WriteHalf};
use tokio_util::codec::{FramedRead, FramedWrite};

use super::{
    Candidate, ControlError, Instance, Live, Named, Ownership, Resolution, START_GRACE_SECS,
    STATUS_PLACE, StartCertainty, StartFailed, StartRequest, Started, StopStep, Stopped, resolve,
    resolve_reconciled, settle_stop, start_in_flight, stop_step, unknown_pending,
};
use crate::client::REPLY_TIMEOUT;
use crate::types::service_text;

/// How often a start in flight is looked at again until serve names its starter.
const SETTLE_POLL: Duration = Duration::from_millis(50);

/// How long a `stop` this plugin sends waits for its reply.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Talks to `dstreamy serve` over a fresh control connection per call, so nothing outlives a call.
#[derive(Debug, Clone)]
pub struct ObsControl {
    endpoint: Endpoint,
    start_timeout: Duration,
    settle_timeout: Duration,
}

struct Connection {
    lines: FramedRead<ReadHalf<dstreamy_ipc::Stream>, ControlCodec>,
    writer: FramedWrite<WriteHalf<dstreamy_ipc::Stream>, ControlCodec>,
    hello: Value,
    instance: Instance,
    next_id: u64,
}

impl ObsControl {
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            start_timeout: REPLY_TIMEOUT,
            settle_timeout: REPLY_TIMEOUT,
        }
    }

    /// How long a start or stop waits for serve to name the starter of a start in flight; [`REPLY_TIMEOUT`] by default.
    pub fn with_settle_timeout(self, settle_timeout: Duration) -> Self {
        Self {
            settle_timeout,
            ..self
        }
    }

    /// How long a `start` waits for its reply; [`REPLY_TIMEOUT`] by default.
    pub fn with_start_timeout(self, start_timeout: Duration) -> Self {
        Self {
            start_timeout,
            ..self
        }
    }

    pub async fn guilds(&self) -> Result<Vec<Named>, ControlError> {
        let mut c = self.open().await?;
        named(
            c.call(json!({"cmd": "guilds"}), "guilds", REPLY_TIMEOUT)
                .await?,
        )
    }

    pub async fn channels(&self, guild_id: &str) -> Result<Vec<Named>, ControlError> {
        let mut c = self.open().await?;
        named(
            c.call(
                json!({"cmd": "channels", "guild_id": guild_id}),
                "channels",
                REPLY_TIMEOUT,
            )
            .await?,
        )
    }

    pub async fn status(&self) -> Result<Option<Live>, ControlError> {
        let c = self.open().await?;
        c.hello_live()
    }

    /// Starts the session, or attaches to it when it is already live in the requested channel or was ours.
    pub async fn start(
        &self,
        request: &StartRequest,
        persisted: Option<&Ownership>,
    ) -> Result<Started, StartFailed> {
        self.start_persistent(request, persisted, |_| Ok(())).await
    }

    /// [`Self::start`], calling `checkpoint` with the ownership to record just before `start` is sent.
    pub async fn start_persistent<F>(
        &self,
        request: &StartRequest,
        persisted: Option<&Ownership>,
        checkpoint: F,
    ) -> Result<Started, StartFailed>
    where
        F: FnMut(&Ownership) -> Result<(), ControlError>,
    {
        self.start_checkpointed(
            request,
            persisted,
            StartCertainty::Confirmed,
            true,
            checkpoint,
        )
        .await
    }

    /// [`Self::start_persistent`] for a journal with a certainty; a pending one never sends a new `start`, and the checkpoint names only the new start.
    pub async fn start_reconciled<F>(
        &self,
        request: &StartRequest,
        persisted: Option<&Ownership>,
        certainty: StartCertainty,
        checkpoint: F,
    ) -> Result<Started, StartFailed>
    where
        F: FnMut(&Ownership) -> Result<(), ControlError>,
    {
        self.start_checkpointed(request, persisted, certainty, false, checkpoint)
            .await
    }

    async fn start_checkpointed<F>(
        &self,
        request: &StartRequest,
        persisted: Option<&Ownership>,
        certainty: StartCertainty,
        merge_previous: bool,
        mut checkpoint: F,
    ) -> Result<Started, StartFailed>
    where
        F: FnMut(&Ownership) -> Result<(), ControlError>,
    {
        let mut c = self.open().await?;
        if request.grace_secs.is_some() {
            c.require(START_GRACE_SECS)?;
        }
        let status = c
            .settled(c.hello["status"].clone(), persisted, self.settle_timeout)
            .await?;
        match resolve_reconciled(request, &status, persisted, certainty, c.instance)? {
            Resolution::Attach(started) => Ok(started),
            Resolution::Start => {
                let provisional = Ownership {
                    connections: vec![Candidate {
                        id: c.connection()?,
                        fps: Some(request.fps),
                    }],
                    guild_id: request.guild_id.clone(),
                    channel_id: request.channel_id.clone(),
                    instance: Some(c.instance),
                };
                let pending = if merge_previous {
                    Ownership::with_candidate(persisted, provisional.clone())
                } else {
                    provisional.clone()
                };
                checkpoint(&pending)?;
                // serve finishes a start whose connection went away, so from here on the start may still land.
                let unconfirmed = |error| StartFailed {
                    error,
                    ownership: Some(Box::new(pending.clone())),
                };
                let id = c.id();
                c.send(request.command(&id)).await.map_err(unconfirmed)?;
                match c
                    .reply(&id, "start", self.start_timeout)
                    .await
                    .map_err(unconfirmed)?
                {
                    Ok(_) => {}
                    Err(e @ ControlError::Locked(_)) => {
                        return Ok(c
                            .attach_after_race(request, persisted, e, self.settle_timeout)
                            .await?);
                    }
                    Err(e) => return Err(e.into()),
                }
                // The session is ours now, so a status without a place must not fail the start.
                let channel = match c.live().await {
                    Ok(Some(live)) => live.channel(),
                    _ => request.channel_id.clone(),
                };
                Ok(Started {
                    ownership: Some(provisional),
                    channel,
                })
            }
        }
    }

    /// Sends `stop` only while the live session is still the one `ownership` names.
    pub async fn stop(&self, ownership: Option<&Ownership>) -> Result<Stopped, ControlError> {
        self.stop_reconciled(ownership, StartCertainty::Confirmed)
            .await
    }

    /// [`Self::stop`] for a journal with a certainty; it never reports a pending start as over while one may still land.
    pub async fn stop_reconciled(
        &self,
        ownership: Option<&Ownership>,
        certainty: StartCertainty,
    ) -> Result<Stopped, ControlError> {
        let Some(ownership) = ownership else {
            return Ok(Stopped::LeftRunning);
        };
        if unknown_pending(ownership, certainty) {
            return Err(ControlError::UnresolvedStart);
        }
        let mut c = match self.open().await {
            Ok(c) => c,
            Err(e) => return stop_without_connection(e),
        };
        let status = c
            .settled(
                c.hello["status"].clone(),
                Some(ownership),
                self.settle_timeout,
            )
            .await?;
        if let StopStep::Done(stopped) = stop_step(ownership, certainty, &status, c.instance)? {
            return Ok(stopped);
        }
        let stopped = match c.call(json!({"cmd": "stop"}), "stop", STOP_TIMEOUT).await {
            Ok(_) => Stopped::Stopped,
            Err(ControlError::Refused { code, .. }) if code == "not_running" => Stopped::NotRunning,
            Err(e) => return Err(e),
        };
        settle_stop(ownership, certainty, stopped)
    }

    async fn open(&self) -> Result<Connection, ControlError> {
        let before = self.instance();
        let stream = dstreamy_ipc::connect(&self.endpoint)
            .await
            .map_err(|e| match e {
                IpcError::NotRunning(_) => ControlError::NotRunning(service_text(&e.to_string())),
                e => ControlError::Connect(service_text(&e.to_string())),
            })?;
        let before = before.ok_or(ControlError::Restarted)?;
        let (read, write) = tokio::io::split(stream);
        let mut c = Connection {
            lines: FramedRead::new(read, ControlCodec::new()),
            writer: FramedWrite::new(write, ControlCodec::new()),
            hello: Value::Null,
            instance: before,
            next_id: 0,
        };
        c.hello = c
            .call(
                json!({"cmd": "hello", "role": "control", "protocol": 2}),
                "hello",
                REPLY_TIMEOUT,
            )
            .await?;
        c.require(STATUS_PLACE)?;
        // The path names whichever serve bound it last, so it must be the same file on both sides of the hello.
        if self.instance() != Some(before) {
            return Err(ControlError::Restarted);
        }
        Ok(c)
    }

    fn instance(&self) -> Option<Instance> {
        let meta = std::fs::metadata(self.endpoint.as_path()).ok()?;
        Some(Instance {
            dev: meta.dev(),
            ino: meta.ino(),
            mtime: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
        })
    }
}

impl Connection {
    fn id(&mut self) -> String {
        self.next_id += 1;
        self.next_id.to_string()
    }

    fn hello_live(&self) -> Result<Option<Live>, ControlError> {
        Ok(self.with_instance(Live::from_status(&self.hello["status"])?))
    }

    fn with_instance(&self, live: Option<Live>) -> Option<Live> {
        live.map(|live| Live {
            instance: Some(self.instance),
            ..live
        })
    }

    fn require(&self, feature: &'static str) -> Result<(), ControlError> {
        let offered = self.hello["features"]
            .as_array()
            .is_some_and(|f| f.iter().any(|v| v == feature));
        if offered {
            Ok(())
        } else {
            Err(ControlError::Outdated { feature })
        }
    }

    fn connection(&self) -> Result<u64, ControlError> {
        self.hello["connection"].as_u64().ok_or_else(|| {
            ControlError::Protocol(service_text(&format!(
                "hello reply without a connection id: {}",
                self.hello
            )))
        })
    }

    async fn send(&mut self, line: Value) -> Result<(), ControlError> {
        self.writer
            .send(line)
            .await
            .map_err(|e| ControlError::Protocol(service_text(&format!("sending: {e}"))))
    }

    /// Sends `command` with a fresh id and returns its result.
    async fn call(
        &mut self,
        mut command: Value,
        name: &'static str,
        timeout: Duration,
    ) -> Result<Value, ControlError> {
        let id = self.id();
        command["id"] = id.clone().into();
        self.send(command).await?;
        self.reply(&id, name, timeout).await?
    }

    /// The outer error is the connection failing; the inner one is serve's `error` reply.
    async fn reply(
        &mut self,
        id: &str,
        name: &'static str,
        timeout: Duration,
    ) -> Result<Result<Value, ControlError>, ControlError> {
        let wait = async {
            loop {
                let line = match self.lines.next().await {
                    Some(Ok(Ok(line))) => line,
                    Some(Ok(Err(e))) => {
                        return Err(ControlError::Protocol(service_text(&format!(
                            "{name} reply: {e}"
                        ))));
                    }
                    Some(Err(e)) => {
                        return Err(ControlError::Protocol(service_text(&format!(
                            "reading the {name} reply: {e}"
                        ))));
                    }
                    None => {
                        return Err(ControlError::Protocol(format!(
                            "connection closed before the {name} reply"
                        )));
                    }
                };
                if line["id"] != id {
                    if line["type"] == "error" && line["id"].is_null() {
                        return Err(ControlError::from_reply(&line));
                    }
                    continue;
                }
                return Ok(match line["type"].as_str() {
                    Some("result") => Ok(line["data"].clone()),
                    Some("error") => Err(ControlError::from_reply(&line)),
                    _ => Err(ControlError::Protocol(service_text(&format!(
                        "{name} reply {line}"
                    )))),
                });
            }
        };
        tokio::time::timeout(timeout, wait)
            .await
            .map_err(|_| ControlError::Timeout {
                command: name,
                secs: timeout.as_secs(),
            })?
    }

    /// Waits out a start in flight, which may be our own whose reply was lost, until serve names its starter.
    async fn settled(
        &mut self,
        mut status: Value,
        persisted: Option<&Ownership>,
        timeout: Duration,
    ) -> Result<Value, ControlError> {
        let deadline = tokio::time::Instant::now() + timeout;
        while start_in_flight(&status, persisted, self.instance)? {
            tokio::time::sleep(
                SETTLE_POLL.min(deadline.saturating_duration_since(tokio::time::Instant::now())),
            )
            .await;
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return Err(ControlError::StillStarting);
            }
            status = match self.call(json!({"cmd": "status"}), "status", left).await {
                Err(ControlError::Timeout { .. }) => return Err(ControlError::StillStarting),
                other => other?,
            };
        }
        Ok(status)
    }

    async fn live(&mut self) -> Result<Option<Live>, ControlError> {
        let status = self
            .call(json!({"cmd": "status"}), "status", REPLY_TIMEOUT)
            .await?;
        Ok(self.with_instance(Live::from_status(&status)?))
    }

    /// Another start won the race, maybe our own earlier one, or another process holds the session lock.
    async fn attach_after_race(
        &mut self,
        request: &StartRequest,
        persisted: Option<&Ownership>,
        refused: ControlError,
        settle_timeout: Duration,
    ) -> Result<Started, ControlError> {
        let status = self
            .call(json!({"cmd": "status"}), "status", REPLY_TIMEOUT)
            .await?;
        let status = self.settled(status, persisted, settle_timeout).await?;
        match resolve(request, &status, persisted, self.instance)? {
            Resolution::Attach(started) => Ok(started),
            Resolution::Start => Err(refused),
        }
    }
}

/// Only a missing or replaced serve proves our session is gone; any other failure keeps the ownership for a retry.
fn stop_without_connection(e: ControlError) -> Result<Stopped, ControlError> {
    match e {
        ControlError::NotRunning(_) | ControlError::Restarted => Ok(Stopped::NotRunning),
        e => Err(e),
    }
}

fn named(data: Value) -> Result<Vec<Named>, ControlError> {
    serde_json::from_value::<Vec<Named>>(data.clone())
        .map(|list| {
            list.into_iter()
                .map(|n| Named {
                    id: n.id,
                    name: service_text(&n.name),
                })
                .collect()
        })
        .map_err(|e| ControlError::Protocol(service_text(&format!("{e}: {data}"))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_missing_or_restarted_serve_ends_the_session() {
        assert_eq!(
            stop_without_connection(ControlError::NotRunning("gone".into())),
            Ok(Stopped::NotRunning)
        );
        assert_eq!(
            stop_without_connection(ControlError::Restarted),
            Ok(Stopped::NotRunning)
        );
        let impostor = ControlError::Connect("the service runs as uid 7".into());
        assert_eq!(stop_without_connection(impostor.clone()), Err(impostor));
        assert_eq!(
            stop_without_connection(ControlError::Stopping),
            Err(ControlError::Stopping)
        );
    }
}
