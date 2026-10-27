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

/// One run of `dstreamy serve`, told apart by its socket file, which every serve binds anew.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instance {
    pub dev: u64,
    pub ino: u64,
    pub mtime: i64,
    pub mtime_nsec: i64,
}

/// How many lost starts of one serve run an ownership remembers.
pub const MAX_CANDIDATES: usize = 8;

/// A start that may have begun the session: serve's `started_by` id and the fps it announced, which status lacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub id: u64,
    #[serde(default)]
    pub fps: Option<u32>,
}

/// Whether the ownership journal still waits on a start whose outcome is unknown; a missing record reads as pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartCertainty {
    #[default]
    Pending,
    Confirmed,
}

/// The session this plugin started, kept in the module config so a relaunched OBS still stops it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "StoredOwnership")]
pub struct Ownership {
    /// One once confirmed; more while starts whose replies were lost may still land.
    pub connections: Vec<Candidate>,
    /// Where the newest start asked to stream; the session may have moved since.
    pub guild_id: String,
    pub channel_id: String,
    #[serde(default)]
    pub instance: Option<Instance>,
}

/// The persisted form, which also reads the single `connection` and `fps` of earlier builds.
#[derive(Deserialize)]
struct StoredOwnership {
    #[serde(default)]
    connection: Option<u64>,
    #[serde(default)]
    fps: Option<u32>,
    #[serde(default)]
    connections: Vec<Candidate>,
    guild_id: String,
    channel_id: String,
    #[serde(default)]
    instance: Option<Instance>,
}

impl From<StoredOwnership> for Ownership {
    fn from(s: StoredOwnership) -> Self {
        let mut connections = s.connections;
        if let Some(id) = s
            .connection
            .filter(|id| connections.iter().all(|c| c.id != *id))
        {
            connections.push(Candidate { id, fps: s.fps });
        }
        Self {
            connections,
            guild_id: s.guild_id,
            channel_id: s.channel_id,
            instance: s.instance,
        }
    }
}

impl Ownership {
    /// Connection ids restart with serve, so only the same serve run's ids name the same start.
    pub fn matches(&self, started_by: Option<u64>, instance: Option<Instance>) -> bool {
        started_by.is_some_and(|id| self.connections.iter().any(|c| c.id == id))
            && instance.is_some()
            && instance == self.instance
    }

    /// The session's fps, known once a single start is left.
    pub fn fps(&self) -> Option<u32> {
        match self.connections.as_slice() {
            [only] => only.fps,
            _ => None,
        }
    }

    /// A lost start joins the candidates of the same serve run; on another run it starts over.
    pub fn with_candidate(persisted: Option<&Ownership>, lost: Ownership) -> Ownership {
        match persisted {
            Some(p) if p.instance.is_some() && p.instance == lost.instance => {
                let mut connections: Vec<Candidate> = p
                    .connections
                    .iter()
                    .filter(|c| lost.connections.iter().all(|l| l.id != c.id))
                    .chain(&lost.connections)
                    .copied()
                    .collect();
                // serve queues starts in arrival order, so the oldest lost start is the one most likely to land.
                let excess = connections.len().saturating_sub(MAX_CANDIDATES);
                if excess > 0 {
                    connections.drain(1..=excess);
                }
                Ownership {
                    connections,
                    ..lost
                }
            }
            _ => lost,
        }
    }

    /// The ownership once serve names the start that won, with that start's fps.
    pub fn confirmed(&self, started_by: u64) -> Ownership {
        let fps = self
            .connections
            .iter()
            .find(|c| c.id == started_by)
            .and_then(|c| c.fps);
        Ownership {
            connections: vec![Candidate {
                id: started_by,
                fps,
            }],
            ..self.clone()
        }
    }
}

/// Whether `status` shows a live session; its place is not needed.
fn is_live(status: &Value) -> Result<bool, ControlError> {
    let session = &status["session"];
    if session.is_null() {
        return Ok(false);
    }
    match session["state"].as_str() {
        Some("connecting" | "streaming" | "reconnecting") => Ok(true),
        Some("idle" | "stopped") => Ok(false),
        Some("stopping") => Err(ControlError::Stopping),
        Some(other) => Err(ControlError::Protocol(service_text(&format!(
            "unknown session state {other}"
        )))),
        None => Err(ControlError::Protocol(service_text(&format!(
            "status without a session state: {status}"
        )))),
    }
}

/// A live session without `started_by` is a start still in flight (serve sets it once `start` returns), possibly ours.
pub fn start_in_flight(
    status: &Value,
    persisted: Option<&Ownership>,
    instance: Instance,
) -> Result<bool, ControlError> {
    let ours = persisted.is_some_and(|o| o.instance == Some(instance));
    Ok(ours && status["started_by"].is_null() && is_live(status)?)
}

/// `Some(true)` when `status` shows a live session that `ownership` started, even one without a place.
pub fn owns(
    status: &Value,
    ownership: &Ownership,
    instance: Instance,
) -> Result<Option<bool>, ControlError> {
    if !is_live(status)? {
        return Ok(None);
    }
    Ok(Some(
        ownership.matches(status["started_by"].as_u64(), Some(instance)),
    ))
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
    /// The serve that reported this session; `from_status` leaves it to the caller.
    pub instance: Option<Instance>,
}

impl Live {
    /// Reads a `status` result; `Ok(None)` when no session is live.
    pub fn from_status(status: &Value) -> Result<Option<Self>, ControlError> {
        if !is_live(status)? {
            return Ok(None);
        }
        let session = &status["session"];
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
            instance: None,
        }))
    }

    pub fn channel(&self) -> String {
        service_text(self.channel_name.as_deref().unwrap_or(&self.channel_id))
    }

    pub fn is_in(&self, guild_id: &str, channel_id: &str) -> bool {
        self.guild_id == guild_id && self.channel_id == channel_id
    }

    pub fn owned_by(&self, ownership: &Ownership) -> bool {
        ownership.matches(self.started_by, self.instance)
    }
}

/// Where a start left the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    /// `Some` when this plugin started the session, or started it before a relaunch.
    pub ownership: Option<Ownership>,
    pub channel: String,
}

impl Started {
    /// Known only for a session this plugin started; see [`crate::settings::check_share_fps`].
    pub fn session_fps(&self) -> Option<u32> {
        self.ownership.as_ref().and_then(Ownership::fps)
    }
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

/// Starts when nothing is live, attaches to our session wherever it is or to an external one in the requested channel.
pub fn decide(
    request: &StartRequest,
    live: Option<&Live>,
    persisted: Option<&Ownership>,
) -> Result<Decision, ControlError> {
    let Some(live) = live else {
        return Ok(Decision::Start);
    };
    if let (Some(owned), Some(started_by)) =
        (persisted.filter(|o| live.owned_by(o)), live.started_by)
    {
        return Ok(Decision::Attach {
            owned: Some(owned.confirmed(started_by)),
        });
    }
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
    Ok(Decision::Attach { owned: None })
}

/// What a start does after reading serve's `status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Start,
    Attach(Started),
}

/// Our own session wins wherever it is and even without a place; otherwise [`decide`] on the live session.
pub fn resolve(
    request: &StartRequest,
    status: &Value,
    persisted: Option<&Ownership>,
    instance: Instance,
) -> Result<Resolution, ControlError> {
    let placed = || {
        Live::from_status(status).map(|live| {
            live.map(|live| Live {
                instance: Some(instance),
                ..live
            })
        })
    };
    if let Some(ours) = persisted
        && owns(status, ours, instance)? == Some(true)
    {
        let channel = match placed() {
            Ok(Some(live)) => live.channel(),
            _ => ours.channel_id.clone(),
        };
        let started_by = status["started_by"].as_u64().unwrap_or_default();
        return Ok(Resolution::Attach(Started {
            ownership: Some(ours.confirmed(started_by)),
            channel,
        }));
    }
    let live = placed()?;
    Ok(match decide(request, live.as_ref(), persisted)? {
        Decision::Start => Resolution::Start,
        Decision::Attach { owned } => Resolution::Attach(Started {
            ownership: owned,
            channel: live.map(|l| l.channel()).unwrap_or_default(),
        }),
    })
}

/// [`resolve`] for a journal with a certainty; a pending one never lets a new start through.
pub fn resolve_reconciled(
    request: &StartRequest,
    status: &Value,
    persisted: Option<&Ownership>,
    certainty: StartCertainty,
    instance: Instance,
) -> Result<Resolution, ControlError> {
    let Some(pending) = persisted.filter(|_| certainty == StartCertainty::Pending) else {
        return resolve(request, status, persisted, instance);
    };
    if unknown_pending(pending, certainty) {
        return Err(ControlError::UnresolvedStart);
    }
    if pending.instance != Some(instance) {
        return resolve(request, status, persisted, instance);
    }
    match resolve(request, status, persisted, instance)? {
        Resolution::Attach(started)
            if started.ownership.is_some() && pending.connections.len() == 1 =>
        {
            Ok(Resolution::Attach(started))
        }
        _ => Err(ControlError::UnresolvedStart),
    }
}

/// A pending start of an unknown service run cannot be told from a later run's, so no status resolves it.
fn unknown_pending(ownership: &Ownership, certainty: StartCertainty) -> bool {
    certainty == StartCertainty::Pending && ownership.instance.is_none()
}

#[derive(Debug, PartialEq, Eq)]
enum StopStep {
    Done(Stopped),
    Send,
}

/// What a stop does after reading `status`; only a confirmed ownership may read an idle service as over.
fn stop_step(
    ownership: &Ownership,
    certainty: StartCertainty,
    status: &Value,
    instance: Instance,
) -> Result<StopStep, ControlError> {
    if unknown_pending(ownership, certainty) {
        return Err(ControlError::UnresolvedStart);
    }
    let may_still_land =
        certainty == StartCertainty::Pending && ownership.instance == Some(instance);
    Ok(match owns(status, ownership, instance)? {
        Some(true) => StopStep::Send,
        _ if may_still_land => return Err(ControlError::UnresolvedStart),
        None => StopStep::Done(Stopped::NotRunning),
        Some(false) => StopStep::Done(Stopped::LeftRunning),
    })
}

/// The outcome of a stop that was sent; stopping one of several pending starts proves nothing about the others.
fn settle_stop(
    ownership: &Ownership,
    certainty: StartCertainty,
    stopped: Stopped,
) -> Result<Stopped, ControlError> {
    if certainty == StartCertainty::Pending && ownership.connections.len() != 1 {
        return Err(ControlError::UnresolvedStart);
    }
    Ok(stopped)
}

/// A start that failed; `ownership` is set once the start was sent, since serve may still carry it out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartFailed {
    pub error: ControlError,
    pub ownership: Option<Box<Ownership>>,
}

impl From<ControlError> for StartFailed {
    fn from(error: ControlError) -> Self {
        Self {
            error,
            ownership: None,
        }
    }
}

impl fmt::Display for StartFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for StartFailed {}

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
    Stopping,
    StillStarting,
    /// The service restarted between two looks at its socket.
    Restarted,
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
    /// The caller could not save the ownership a start must record before it is sent.
    Persistence(String),
    /// A start whose outcome is unknown may still land, so nothing may start or clear the ownership yet.
    UnresolvedStart,
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
            "unsupported_protocol" => Self::Outdated {
                feature: "control protocol 2",
            },
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
            Self::Stopping => f.write_str("D-Streamy is still stopping; try again"),
            Self::StillStarting => f.write_str("D-Streamy is still starting; try again"),
            Self::Restarted => {
                f.write_str("The D-Streamy service restarted while connecting; try again")
            }
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
            Self::Persistence(e) => write!(f, "Cannot save the D-Streamy session record: {e}"),
            Self::UnresolvedStart => f.write_str(
                "An earlier D-Streamy start may still be in progress: wait for it to settle, or restart the service",
            ),
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
            instance: Some(instance(1)),
        }
    }

    fn instance(ino: u64) -> Instance {
        Instance {
            dev: 1,
            ino,
            mtime: 5,
            mtime_nsec: 6,
        }
    }

    fn owned(connection: u64) -> Ownership {
        Ownership {
            connections: vec![Candidate {
                id: connection,
                fps: Some(30),
            }],
            guild_id: "g".into(),
            channel_id: "c".into(),
            instance: Some(instance(1)),
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
    }

    #[test]
    fn our_session_is_attached_wherever_it_moved() {
        let r = request();
        let mut moved = live("d", "external", Some(3));
        moved.guild_id = "h".into();
        assert!(moved.owned_by(&owned(3)));
        assert!(!moved.owned_by(&owned(4)));
        assert_eq!(
            decide(&r, Some(&moved), Some(&owned(3))),
            Ok(Decision::Attach {
                owned: Some(owned(3))
            })
        );
        assert!(matches!(
            decide(&r, Some(&moved), Some(&owned(4))),
            Err(ControlError::Busy { .. })
        ));
    }

    #[test]
    fn our_session_without_a_place_is_attached_and_kept() {
        let r = request();
        let placeless = json!({"session": {"state": "streaming"}, "started_by": 3});
        assert_eq!(
            resolve(&r, &placeless, Some(&owned(3)), instance(1)),
            Ok(Resolution::Attach(Started {
                ownership: Some(owned(3)),
                channel: "c".into(),
            }))
        );
        assert_eq!(
            resolve(&r, &placeless, Some(&owned(4)), instance(1)),
            Err(ControlError::NoPlace)
        );
        assert_eq!(
            resolve(&r, &placeless, Some(&owned(3)), instance(2)),
            Err(ControlError::NoPlace)
        );
        let placed = json!({"session": {"state": "streaming", "guild_id": "g", "channel_id": "d",
            "channel_name": "Gaming", "target": "external"}, "started_by": 3});
        assert_eq!(
            resolve(&r, &placed, Some(&owned(3)), instance(1)),
            Ok(Resolution::Attach(Started {
                ownership: Some(owned(3)),
                channel: "Gaming".into(),
            }))
        );
        assert_eq!(
            resolve(&r, &json!({"session": null}), Some(&owned(3)), instance(1)),
            Ok(Resolution::Start)
        );
    }

    #[test]
    fn a_live_session_without_a_starter_is_in_flight_for_the_same_serve_only() {
        let in_flight = json!({"session": {"state": "connecting"}, "started_by": null});
        assert_eq!(
            start_in_flight(&in_flight, Some(&owned(3)), instance(1)),
            Ok(true)
        );
        assert_eq!(
            start_in_flight(&in_flight, Some(&owned(3)), instance(2)),
            Ok(false)
        );
        assert_eq!(start_in_flight(&in_flight, None, instance(1)), Ok(false));
        let started = json!({"session": {"state": "connecting"}, "started_by": 3});
        assert_eq!(
            start_in_flight(&started, Some(&owned(3)), instance(1)),
            Ok(false)
        );
        let idle = json!({"session": {"state": "idle"}, "started_by": null});
        assert_eq!(
            start_in_flight(&idle, Some(&owned(3)), instance(1)),
            Ok(false)
        );
    }

    #[test]
    fn ownership_is_read_without_a_place() {
        let status = |state: &str, started_by: u64| json!({"session": {"state": state}, "started_by": started_by});
        let here = instance(1);
        assert_eq!(
            owns(&status("streaming", 3), &owned(3), here),
            Ok(Some(true))
        );
        assert_eq!(
            owns(&status("streaming", 4), &owned(3), here),
            Ok(Some(false))
        );
        assert_eq!(
            owns(&status("streaming", 3), &owned(3), instance(2)),
            Ok(Some(false))
        );
        assert_eq!(owns(&status("stopped", 3), &owned(3), here), Ok(None));
        assert_eq!(owns(&json!({"session": null}), &owned(3), here), Ok(None));
        assert_eq!(
            Live::from_status(&status("streaming", 3)),
            Err(ControlError::NoPlace)
        );
    }

    #[test]
    fn a_restarted_service_reusing_the_connection_id_is_not_owned() {
        let r = request();
        let mut restarted = live("c", "external", Some(3));
        restarted.instance = Some(instance(2));
        assert_eq!(
            decide(&r, Some(&restarted), Some(&owned(3))),
            Ok(Decision::Attach { owned: None })
        );
        let unknown = Ownership {
            instance: None,
            ..owned(3)
        };
        assert!(!live("c", "external", Some(3)).owned_by(&unknown));
        let mut unseen = live("c", "external", Some(3));
        unseen.instance = None;
        assert!(!unseen.owned_by(&unknown));
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
                instance: None,
            }))
        );
        for state in ["idle", "stopped"] {
            let s = json!({"session": {"state": state}, "started_by": null});
            assert_eq!(Live::from_status(&s), Ok(None), "{state}");
        }
        assert_eq!(Live::from_status(&json!({"session": null})), Ok(None));
        assert_eq!(
            Live::from_status(&json!({"session": {"state": "stopping"}})),
            Err(ControlError::Stopping)
        );
        assert!(matches!(
            Live::from_status(&json!({"session": {"state": "paused"}})),
            Err(ControlError::Protocol(_))
        ));
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
            ControlError::from_reply(&reply("unsupported_protocol")).to_string(),
            "The D-Streamy service is too old for this plugin (it lacks control protocol 2): update and restart it"
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
            json!({"connections": [{"id": 9, "fps": 30}], "guild_id": "g", "channel_id": "c",
                   "instance": {"dev": 1, "ino": 1, "mtime": 5, "mtime_nsec": 6}})
        );
        assert_eq!(serde_json::from_value::<Ownership>(v).unwrap(), o);
        let older: Ownership = serde_json::from_value(
            json!({"connection": 9, "fps": 60, "guild_id": "g", "channel_id": "c"}),
        )
        .unwrap();
        assert_eq!(
            older.connections,
            [Candidate {
                id: 9,
                fps: Some(60)
            }]
        );
        assert_eq!(older.instance, None);
    }

    fn candidates(ids: &[u64], ino: u64) -> Ownership {
        Ownership {
            connections: ids
                .iter()
                .map(|&id| Candidate {
                    id,
                    fps: Some(u32::try_from(id).unwrap()),
                })
                .collect(),
            instance: Some(instance(ino)),
            ..owned(0)
        }
    }

    fn ids(o: &Ownership) -> Vec<u64> {
        o.connections.iter().map(|c| c.id).collect()
    }

    #[test]
    fn lost_starts_of_one_serve_run_all_stay_candidates() {
        let merged = Ownership::with_candidate(Some(&candidates(&[3], 1)), candidates(&[5], 1));
        assert_eq!(ids(&merged), [3, 5]);
        assert!(merged.matches(Some(3), Some(instance(1))));
        assert!(merged.matches(Some(5), Some(instance(1))));
        assert!(!merged.matches(Some(4), Some(instance(1))));
        assert!(!merged.matches(Some(3), Some(instance(2))));
        let again = Ownership::with_candidate(Some(&merged), candidates(&[3], 1));
        assert_eq!(ids(&again), [5, 3]);
        let other_run = Ownership::with_candidate(Some(&merged), candidates(&[7], 2));
        assert_eq!(ids(&other_run), [7]);
        assert_eq!(
            ids(&Ownership::with_candidate(None, candidates(&[7], 1))),
            [7]
        );
        let unknown_run = Ownership {
            instance: None,
            ..candidates(&[3], 1)
        };
        assert_eq!(
            ids(&Ownership::with_candidate(
                Some(&unknown_run),
                candidates(&[7], 1)
            )),
            [7]
        );
    }

    #[test]
    fn the_candidates_are_bounded_and_keep_the_first_and_the_newest() {
        let mut o = candidates(&[1], 1);
        for id in 2..=12 {
            o = Ownership::with_candidate(Some(&o), candidates(&[id], 1));
        }
        assert_eq!(ids(&o), [1, 6, 7, 8, 9, 10, 11, 12]);
        assert_eq!(o.connections.len(), MAX_CANDIDATES);
        let nine = (2..=9).fold(candidates(&[1], 1), |o, id| {
            Ownership::with_candidate(Some(&o), candidates(&[id], 1))
        });
        assert_eq!(ids(&nine), [1, 3, 4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn empty_candidate_lists_merge_without_panicking() {
        let merged = Ownership::with_candidate(Some(&candidates(&[], 1)), candidates(&[], 1));
        assert!(merged.connections.is_empty());
        assert!(!merged.matches(Some(1), Some(instance(1))));
        let one = Ownership::with_candidate(Some(&candidates(&[], 1)), candidates(&[4], 1));
        assert_eq!(ids(&one), [4]);
    }

    #[test]
    fn the_winning_start_keeps_its_own_fps() {
        let first = Ownership {
            connections: vec![Candidate {
                id: 3,
                fps: Some(60),
            }],
            ..candidates(&[], 1)
        };
        let second = Ownership {
            connections: vec![Candidate {
                id: 5,
                fps: Some(30),
            }],
            ..candidates(&[], 1)
        };
        let merged = Ownership::with_candidate(Some(&first), second);
        assert_eq!(merged.fps(), None, "unknown until one start is confirmed");
        assert_eq!(merged.confirmed(3).fps(), Some(60));
        assert_eq!(merged.confirmed(5).fps(), Some(30));
        assert_eq!(merged.confirmed(4).fps(), None);
        let started = Started {
            ownership: Some(merged.confirmed(3)),
            channel: "c".into(),
        };
        assert_eq!(started.session_fps(), Some(60));
    }

    #[test]
    fn an_owned_attach_collapses_to_the_winning_start() {
        let set = candidates(&[3, 5], 1);
        let mut live = live("c", "external", Some(5));
        live.instance = Some(instance(1));
        assert_eq!(
            decide(&request(), Some(&live), Some(&set)),
            Ok(Decision::Attach {
                owned: Some(candidates(&[5], 1))
            })
        );
        let placeless = json!({"session": {"state": "streaming"}, "started_by": 3});
        assert_eq!(
            resolve(&request(), &placeless, Some(&set), instance(1)),
            Ok(Resolution::Attach(Started {
                ownership: Some(candidates(&[3], 1)),
                channel: "c".into(),
            }))
        );
    }

    fn serving(started_by: Option<u64>) -> Value {
        json!({"session": {"state": "streaming", "guild_id": "g", "channel_id": "c",
            "channel_name": "General", "target": "external"}, "started_by": started_by})
    }

    fn nothing_live() -> Value {
        json!({"session": null, "started_by": null})
    }

    fn legacy(ids: &[u64]) -> Ownership {
        Ownership {
            instance: None,
            ..candidates(ids, 1)
        }
    }

    const PENDING: StartCertainty = StartCertainty::Pending;
    const CONFIRMED: StartCertainty = StartCertainty::Confirmed;

    #[test]
    fn a_missing_certainty_reads_as_pending() {
        assert_eq!(StartCertainty::default(), PENDING);
        assert_eq!(serde_json::to_value(CONFIRMED).unwrap(), json!("confirmed"));
        assert_eq!(
            serde_json::from_value::<StartCertainty>(json!("pending")).unwrap(),
            PENDING
        );
    }

    #[test]
    fn a_pending_start_with_nothing_live_is_not_over() {
        let known = candidates(&[3], 1);
        for status in [nothing_live(), json!({"session": {"state": "idle"}})] {
            assert_eq!(
                stop_step(&known, PENDING, &status, instance(1)),
                Err(ControlError::UnresolvedStart)
            );
            assert_eq!(
                stop_step(&known, CONFIRMED, &status, instance(1)),
                Ok(StopStep::Done(Stopped::NotRunning))
            );
        }
        assert_eq!(
            stop_step(&known, PENDING, &serving(Some(4)), instance(1)),
            Err(ControlError::UnresolvedStart)
        );
        assert_eq!(
            stop_step(&known, CONFIRMED, &serving(Some(4)), instance(1)),
            Ok(StopStep::Done(Stopped::LeftRunning))
        );
        for certainty in [PENDING, CONFIRMED] {
            assert_eq!(
                stop_step(&known, certainty, &serving(Some(3)), instance(1)),
                Ok(StopStep::Send)
            );
        }
    }

    #[test]
    fn a_replaced_service_retires_a_pending_identity() {
        let known = candidates(&[3, 5], 1);
        assert_eq!(
            stop_step(&known, PENDING, &nothing_live(), instance(2)),
            Ok(StopStep::Done(Stopped::NotRunning))
        );
        assert_eq!(
            stop_step(&known, PENDING, &serving(Some(3)), instance(2)),
            Ok(StopStep::Done(Stopped::LeftRunning))
        );
    }

    #[test]
    fn a_pending_start_of_an_unknown_service_is_never_resolved_by_status() {
        let unknown = legacy(&[3]);
        for status in [nothing_live(), serving(Some(3)), serving(Some(4))] {
            assert_eq!(
                stop_step(&unknown, PENDING, &status, instance(1)),
                Err(ControlError::UnresolvedStart)
            );
        }
        assert_eq!(
            stop_step(&unknown, CONFIRMED, &nothing_live(), instance(1)),
            Ok(StopStep::Done(Stopped::NotRunning))
        );
    }

    #[test]
    fn stopping_one_of_several_pending_starts_leaves_the_rest_unresolved() {
        let several = candidates(&[3, 5], 1);
        assert_eq!(
            stop_step(&several, PENDING, &serving(Some(5)), instance(1)),
            Ok(StopStep::Send)
        );
        assert_eq!(
            settle_stop(&several, PENDING, Stopped::Stopped),
            Err(ControlError::UnresolvedStart)
        );
        assert_eq!(
            settle_stop(&several, PENDING, Stopped::NotRunning),
            Err(ControlError::UnresolvedStart)
        );
        assert_eq!(
            settle_stop(&candidates(&[3], 1), PENDING, Stopped::Stopped),
            Ok(Stopped::Stopped)
        );
        assert_eq!(
            settle_stop(&several, CONFIRMED, Stopped::Stopped),
            Ok(Stopped::Stopped)
        );
    }

    #[test]
    fn a_pending_start_never_lets_a_new_remote_start_through() {
        let r = request();
        let one = candidates(&[3], 1);
        assert_eq!(
            resolve_reconciled(&r, &nothing_live(), Some(&one), PENDING, instance(1)),
            Err(ControlError::UnresolvedStart)
        );
        assert_eq!(
            resolve_reconciled(&r, &nothing_live(), Some(&one), CONFIRMED, instance(1)),
            Ok(Resolution::Start)
        );
        assert_eq!(
            resolve_reconciled(
                &r,
                &nothing_live(),
                Some(&legacy(&[3])),
                PENDING,
                instance(1)
            ),
            Err(ControlError::UnresolvedStart)
        );
        assert_eq!(
            resolve_reconciled(&r, &nothing_live(), Some(&one), PENDING, instance(2)),
            Ok(Resolution::Start),
            "another service run cannot land the old start"
        );
        assert_eq!(
            resolve_reconciled(&r, &nothing_live(), None, PENDING, instance(1)),
            Ok(Resolution::Start)
        );
    }

    #[test]
    fn a_pending_start_attaches_only_to_its_single_late_candidate() {
        let r = request();
        let one = candidates(&[3], 1);
        assert_eq!(
            resolve_reconciled(&r, &serving(Some(3)), Some(&one), PENDING, instance(1)),
            Ok(Resolution::Attach(Started {
                ownership: Some(one.confirmed(3)),
                channel: "General".into(),
            }))
        );
        assert_eq!(
            resolve_reconciled(&r, &serving(Some(4)), Some(&one), PENDING, instance(1)),
            Err(ControlError::UnresolvedStart),
            "a foreign session does not resolve it nor replace the record"
        );
        let several = candidates(&[3, 5], 1);
        assert_eq!(
            resolve_reconciled(&r, &serving(Some(3)), Some(&several), PENDING, instance(1)),
            Err(ControlError::UnresolvedStart),
            "attaching must not collapse the other candidates"
        );
        assert_eq!(
            resolve_reconciled(
                &r,
                &serving(Some(3)),
                Some(&several),
                CONFIRMED,
                instance(1)
            ),
            resolve(&r, &serving(Some(3)), Some(&several), instance(1))
        );
    }

    #[test]
    fn the_new_error_texts_say_what_to_do() {
        assert_eq!(
            ControlError::Persistence("disk full".into()).to_string(),
            "Cannot save the D-Streamy session record: disk full"
        );
        assert!(
            ControlError::UnresolvedStart
                .to_string()
                .starts_with("An earlier D-Streamy start may still be in progress")
        );
    }

    #[test]
    fn a_stopping_session_blocks_a_start_with_a_retryable_error() {
        let status = json!({"session": {"state": "stopping"}, "started_by": null});
        let err = Live::from_status(&status).unwrap_err();
        assert_eq!(err.to_string(), "D-Streamy is still stopping; try again");
    }
}
