use std::time::Duration;

use dstreamy_ipc::{ControlCodec, Endpoint, IpcError};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{ReadHalf, WriteHalf};
use tokio_util::codec::{FramedRead, FramedWrite};

use super::{
    ControlError, Decision, Live, Named, Ownership, START_GRACE_SECS, STATUS_PLACE, StartRequest,
    Started, Stopped, decide,
};
use crate::client::REPLY_TIMEOUT;
use crate::types::service_text;

/// How long a `stop` this plugin sends waits for its reply.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Talks to `dstreamy serve` over a fresh control connection per call, so nothing outlives a call.
#[derive(Debug, Clone)]
pub struct ObsControl {
    endpoint: Endpoint,
}

struct Connection {
    lines: FramedRead<ReadHalf<dstreamy_ipc::Stream>, ControlCodec>,
    writer: FramedWrite<WriteHalf<dstreamy_ipc::Stream>, ControlCodec>,
    hello: Value,
    next_id: u64,
}

impl ObsControl {
    pub fn new(endpoint: Endpoint) -> Self {
        Self { endpoint }
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
        Live::from_status(&c.hello["status"])
    }

    /// Starts the session, or attaches to it when it is already live in the requested channel.
    pub async fn start(
        &self,
        request: &StartRequest,
        persisted: Option<&Ownership>,
    ) -> Result<Started, ControlError> {
        let mut c = self.open().await?;
        if request.grace_secs.is_some() {
            c.require(START_GRACE_SECS)?;
        }
        let live = Live::from_status(&c.hello["status"])?;
        match decide(request, live.as_ref(), persisted)? {
            Decision::Attach { owned } => Ok(Started {
                ownership: owned,
                channel: live.map(|l| l.channel()).unwrap_or_default(),
            }),
            Decision::Start => {
                let connection = c.connection()?;
                let id = c.id();
                c.send(request.command(&id)).await?;
                match c.reply(&id, "start", REPLY_TIMEOUT).await? {
                    Ok(_) => {}
                    Err(e @ ControlError::Locked(_)) => {
                        return c.attach_after_race(request, e).await;
                    }
                    Err(e) => return Err(e),
                }
                // The session is ours now, so a status without a place must not fail the start.
                let channel = match c.live().await {
                    Ok(Some(live)) => live.channel(),
                    _ => request.channel_id.clone(),
                };
                Ok(Started {
                    ownership: Some(Ownership {
                        connection,
                        guild_id: request.guild_id.clone(),
                        channel_id: request.channel_id.clone(),
                    }),
                    channel,
                })
            }
        }
    }

    /// Sends `stop` only while the live session is still the one `ownership` names.
    pub async fn stop(&self, ownership: Option<&Ownership>) -> Result<Stopped, ControlError> {
        let Some(ownership) = ownership else {
            return Ok(Stopped::LeftRunning);
        };
        let mut c = self.open().await?;
        match Live::from_status(&c.hello["status"])? {
            None => return Ok(Stopped::NotRunning),
            Some(live) if !live.owned_by(ownership) => return Ok(Stopped::LeftRunning),
            Some(_) => {}
        }
        match c.call(json!({"cmd": "stop"}), "stop", STOP_TIMEOUT).await {
            Ok(_) => Ok(Stopped::Stopped),
            Err(ControlError::Refused { code, .. }) if code == "not_running" => {
                Ok(Stopped::NotRunning)
            }
            Err(e) => Err(e),
        }
    }

    async fn open(&self) -> Result<Connection, ControlError> {
        let stream = dstreamy_ipc::connect(&self.endpoint)
            .await
            .map_err(|e| match e {
                IpcError::NotRunning(_) => ControlError::NotRunning(service_text(&e.to_string())),
                e => ControlError::Connect(service_text(&e.to_string())),
            })?;
        let (read, write) = tokio::io::split(stream);
        let mut c = Connection {
            lines: FramedRead::new(read, ControlCodec::new()),
            writer: FramedWrite::new(write, ControlCodec::new()),
            hello: Value::Null,
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
        Ok(c)
    }
}

impl Connection {
    fn id(&mut self) -> String {
        self.next_id += 1;
        self.next_id.to_string()
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

    async fn live(&mut self) -> Result<Option<Live>, ControlError> {
        let status = self
            .call(json!({"cmd": "status"}), "status", REPLY_TIMEOUT)
            .await?;
        Live::from_status(&status)
    }

    /// Another client's start won the race, or another process holds the session lock.
    async fn attach_after_race(
        &mut self,
        request: &StartRequest,
        refused: ControlError,
    ) -> Result<Started, ControlError> {
        let Some(live) = self.live().await? else {
            return Err(refused);
        };
        match decide(request, Some(&live), None)? {
            Decision::Attach { .. } => Ok(Started {
                ownership: None,
                channel: live.channel(),
            }),
            Decision::Start => Err(refused),
        }
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
