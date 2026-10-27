//! The control side of the OBS plugin: listing guilds and channels, and starting or stopping the session it feeds.

#[cfg(unix)]
mod service;

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::types::service_text;

#[cfg(unix)]
pub use service::{ObsControl, STOP_TIMEOUT};

/// What a serve must offer beyond protocol 2 as first released.
pub const STATUS_PLACE: &str = "status_place";
pub const START_GRACE_SECS: &str = "start_grace_secs";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Named {
    pub id: String,
    pub name: String,
}

/// The session this plugin started, kept in the module config so a relaunched OBS still stops it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ownership {
    /// The control connection whose `start` began the session, as serve's `started_by` names it.
    pub connection: u64,
    pub guild_id: String,
    pub channel_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartRequest {
    pub guild_id: String,
    pub channel_id: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub grace_secs: Option<u64>,
}

impl StartRequest {
    pub fn command(&self, id: &str) -> Value {
        let mut command = json!({
            "id": id, "cmd": "start", "guild_id": self.guild_id, "channel_id": self.channel_id,
            "target": "external", "resolution": format!("{}x{}", self.width, self.height),
            "fps": self.fps, "codec": "h264",
        });
        if let Some(secs) = self.grace_secs {
            command["grace_secs"] = secs.into();
        }
        command
    }
}

/// A session serve reports as connecting, streaming or reconnecting, with its place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Live {
    pub guild_id: String,
    pub channel_id: String,
    pub channel_name: Option<String>,
    pub target: String,
    pub started_by: Option<u64>,
}

impl Live {
    /// Reads a `status` result; `Ok(None)` when no session is live.
    pub fn from_status(status: &Value) -> Result<Option<Self>, ControlError> {
        let session = &status["session"];
        if session.is_null() {
            return Ok(None);
        }
        let Some(state) = session["state"].as_str() else {
            return Err(ControlError::Protocol(service_text(&format!(
                "status without a session state: {status}"
            ))));
        };
        if !matches!(state, "connecting" | "streaming" | "reconnecting") {
            return Ok(None);
        }
        let text = |key: &str| session[key].as_str().map(str::to_owned);
        let (Some(guild_id), Some(channel_id), Some(target)) =
            (text("guild_id"), text("channel_id"), text("target"))
        else {
            return Err(ControlError::NoPlace);
        };
        Ok(Some(Self {
            guild_id,
            channel_id,
            channel_name: text("channel_name"),
            target,
            started_by: status["started_by"].as_u64(),
        }))
    }

    pub fn channel(&self) -> String {
        service_text(self.channel_name.as_deref().unwrap_or(&self.channel_id))
    }

    pub fn is_in(&self, guild_id: &str, channel_id: &str) -> bool {
        self.guild_id == guild_id && self.channel_id == channel_id
    }

    /// Connection ids restart with serve, so the guild must match too; the channel may have moved since.
    pub fn owned_by(&self, ownership: &Ownership) -> bool {
        self.started_by == Some(ownership.connection) && self.guild_id == ownership.guild_id
    }
}

/// Where a start left the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    /// `Some` when this plugin started the session, or started it before a relaunch.
    pub ownership: Option<Ownership>,
    pub channel: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    Stopped,
    /// Someone else started the session, so it keeps running.
    LeftRunning,
    NotRunning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Start,
    Attach { owned: Option<Ownership> },
}

/// Starts when nothing is live, attaches to a live external session in the requested channel, and refuses anything else.
pub fn decide(
    request: &StartRequest,
    live: Option<&Live>,
    persisted: Option<&Ownership>,
) -> Result<Decision, ControlError> {
    let Some(live) = live else {
        return Ok(Decision::Start);
    };
    if !live.is_in(&request.guild_id, &request.channel_id) {
        return Err(ControlError::Busy {
            channel: live.channel(),
        });
    }
    if live.target != "external" {
        return Err(ControlError::OtherSource {
            channel: live.channel(),
        });
    }
    Ok(Decision::Attach {
        owned: persisted.filter(|o| live.owned_by(o)).cloned(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlError {
    NotRunning(String),
    Connect(String),
    NotLoggedIn,
    InvalidToken,
    UnknownGuild,
    UnknownChannel,
    GuildUnavailable,
    /// Another process holds the session lock; the message names its PID.
    Locked(String),
    Busy {
        channel: String,
    },
    OtherSource {
        channel: String,
    },
    Outdated {
        feature: &'static str,
    },
    /// A live session without a place, as serve reports after a 2 s catch-up timeout.
    NoPlace,
    Refused {
        code: String,
        message: String,
    },
    Timeout {
        command: &'static str,
        secs: u64,
    },
    Protocol(String),
}

impl ControlError {
    /// Maps an `error` reply by its code.
    pub fn from_reply(reply: &Value) -> Self {
        let code = reply["code"].as_str().unwrap_or_default();
        let message = service_text(reply["message"].as_str().unwrap_or_default());
        match code {
            "not_logged_in" => Self::NotLoggedIn,
            "invalid_token" => Self::InvalidToken,
            "unknown_guild" => Self::UnknownGuild,
            "unknown_channel" => Self::UnknownChannel,
            "guild_unavailable" => Self::GuildUnavailable,
            "already_running" => Self::Locked(message),
            _ => Self::Refused {
                code: service_text(code),
                message,
            },
        }
    }
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRunning(e) => write!(f, "The D-Streamy service is not running ({e})"),
            Self::Connect(e) => write!(f, "Cannot connect to the D-Streamy service: {e}"),
            Self::NotLoggedIn => f.write_str("Log in first: run `dstreamy login`"),
            Self::InvalidToken => {
                f.write_str("Discord rejected the saved login: run `dstreamy login` again")
            }
            Self::UnknownGuild => f.write_str("Discord doesn't list that server: pick it again"),
            Self::UnknownChannel => {
                f.write_str("That server has no such voice channel: pick the channel again")
            }
            Self::GuildUnavailable => {
                f.write_str("Discord reports that server as unavailable: try again later")
            }
            Self::Locked(message) => write!(
                f,
                "Another D-Streamy process is streaming with this account ({message})"
            ),
            Self::Busy { channel } => write!(f, "D-Streamy is already live in {channel}"),
            Self::OtherSource { channel } => write!(
                f,
                "D-Streamy is already live in {channel} from another source: stop it first"
            ),
            Self::Outdated { feature } => write!(
                f,
                "The D-Streamy service is too old for this plugin (it lacks {feature}): update and restart it"
            ),
            Self::NoPlace => f.write_str(
                "The D-Streamy service doesn't know where its session streams: stop it and start again",
            ),
            Self::Refused { code, message } => {
                write!(f, "The D-Streamy service refused ({code}): {message}")
            }
            Self::Timeout { command, secs } => write!(
                f,
                "The D-Streamy service didn't answer {command} within {secs} s"
            ),
            Self::Protocol(m) => write!(f, "The D-Streamy service answered unexpectedly: {m}"),
        }
    }
}

impl std::error::Error for ControlError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> StartRequest {
        StartRequest {
            guild_id: "g".into(),
            channel_id: "c".into(),
            width: 1280,
            height: 720,
            fps: 30,
            grace_secs: None,
        }
    }

    fn live(channel_id: &str, target: &str, started_by: Option<u64>) -> Live {
        Live {
            guild_id: "g".into(),
            channel_id: channel_id.into(),
            channel_name: Some(format!("#{channel_id}")),
            target: target.into(),
            started_by,
        }
    }

    fn owned(connection: u64) -> Ownership {
        Ownership {
            connection,
            guild_id: "g".into(),
            channel_id: "c".into(),
        }
    }

    #[test]
    fn start_command_is_an_external_h264_start() {
        let mut r = request();
        assert_eq!(
            r.command("7"),
            json!({"id": "7", "cmd": "start", "guild_id": "g", "channel_id": "c", "target": "external",
                   "resolution": "1280x720", "fps": 30, "codec": "h264"})
        );
        r.grace_secs = Some(120);
        assert_eq!(r.command("7")["grace_secs"], 120);
    }

    #[test]
    fn nothing_live_starts() {
        assert_eq!(
            decide(&request(), None, Some(&owned(3))),
            Ok(Decision::Start)
        );
    }

    #[test]
    fn same_channel_attaches_and_keeps_only_a_matching_ownership() {
        let r = request();
        let ours = live("c", "external", Some(3));
        assert_eq!(
            decide(&r, Some(&ours), Some(&owned(3))),
            Ok(Decision::Attach {
                owned: Some(owned(3))
            })
        );
        assert_eq!(
            decide(&r, Some(&ours), Some(&owned(4))),
            Ok(Decision::Attach { owned: None })
        );
        assert_eq!(
            decide(&r, Some(&ours), None),
            Ok(Decision::Attach { owned: None })
        );
        let other_guild = Ownership {
            guild_id: "h".into(),
            ..owned(3)
        };
        assert_eq!(
            decide(&r, Some(&ours), Some(&other_guild)),
            Ok(Decision::Attach { owned: None })
        );
    }

    #[test]
    fn a_session_moved_to_another_channel_is_still_owned() {
        let moved = live("d", "external", Some(3));
        assert!(moved.owned_by(&owned(3)));
        assert!(!moved.owned_by(&owned(4)));
    }

    #[test]
    fn another_channel_is_named() {
        let err = decide(&request(), Some(&live("d", "external", None)), None).unwrap_err();
        assert_eq!(err.to_string(), "D-Streamy is already live in #d");
        let mut unnamed = live("d", "external", None);
        unnamed.channel_name = None;
        assert_eq!(
            decide(&request(), Some(&unnamed), None).unwrap_err(),
            ControlError::Busy {
                channel: "d".into()
            }
        );
        let mut other_guild = live("c", "external", None);
        other_guild.guild_id = "h".into();
        assert!(matches!(
            decide(&request(), Some(&other_guild), None),
            Err(ControlError::Busy { .. })
        ));
    }

    #[test]
    fn a_capture_session_in_the_same_channel_is_refused() {
        assert_eq!(
            decide(&request(), Some(&live("c", "test-pattern", None)), None),
            Err(ControlError::OtherSource {
                channel: "#c".into()
            })
        );
    }

    #[test]
    fn status_reads_the_live_place() {
        let status = json!({"session": {"state": "reconnecting", "attempt": 1, "guild_id": "g",
            "channel_id": "c", "channel_name": "General", "target": "external"}, "started_by": 5});
        assert_eq!(
            Live::from_status(&status),
            Ok(Some(Live {
                guild_id: "g".into(),
                channel_id: "c".into(),
                channel_name: Some("General".into()),
                target: "external".into(),
                started_by: Some(5),
            }))
        );
        for state in ["idle", "stopped"] {
            let s = json!({"session": {"state": state}, "started_by": null});
            assert_eq!(Live::from_status(&s), Ok(None), "{state}");
        }
        assert_eq!(Live::from_status(&json!({"session": null})), Ok(None));
        assert_eq!(
            Live::from_status(&json!({"session": {"state": "streaming"}})),
            Err(ControlError::NoPlace)
        );
    }

    #[test]
    fn service_text_in_a_channel_name_is_cleaned() {
        let mut l = live("c", "external", None);
        l.channel_name = Some("a\u{202E}b\nc".into());
        assert_eq!(l.channel(), "a b c");
    }

    #[test]
    fn error_replies_map_to_user_texts() {
        let reply = |code: &str| json!({"type": "error", "code": code, "message": "m"});
        assert_eq!(
            ControlError::from_reply(&reply("not_logged_in")).to_string(),
            "Log in first: run `dstreamy login`"
        );
        assert_eq!(
            ControlError::from_reply(&reply("unknown_channel")),
            ControlError::UnknownChannel
        );
        assert_eq!(
            ControlError::from_reply(&reply("already_running")),
            ControlError::Locked("m".into())
        );
        assert_eq!(
            ControlError::from_reply(&reply("gateway")).to_string(),
            "The D-Streamy service refused (gateway): m"
        );
    }

    #[test]
    fn ownership_round_trips_as_json() {
        let o = owned(9);
        let v = serde_json::to_value(&o).unwrap();
        assert_eq!(
            v,
            json!({"connection": 9, "guild_id": "g", "channel_id": "c"})
        );
        assert_eq!(serde_json::from_value::<Ownership>(v).unwrap(), o);
    }
}
