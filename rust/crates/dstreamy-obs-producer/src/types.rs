//! What the plugin and the producer connection share on every platform.

use serde_json::{Value, json};

use crate::control::Relogin;

/// Service text shown in OBS is cut to this many characters.
pub const MAX_SERVICE_TEXT: usize = 300;

/// Log level mapped onto libobs's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Debug,
    Info,
    Warn,
}

/// Where the libobs-free half logs to; the plugin forwards to `blog`, tests collect.
pub trait Log {
    fn log(&self, level: Level, message: &str);
    fn keyframe_sent(&self, _pts_us: i64, _arrived_ns: i64) {}
    fn stop_drain_started(&self, _queued: usize) {}
    fn stop_drain_timed_out(&self, _deadline_ms: u64) {}
}

/// What the producer declares in its `hello`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub video_codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub audio_codec: String,
    pub sample_rate: u32,
    pub channels: u32,
    pub frame_ms: u32,
    pub client_name: String,
    pub client_version: String,
    pub obs_version: String,
}

impl Hello {
    pub fn to_json(&self) -> Value {
        json!({
            "id": "1", "cmd": "hello", "role": "producer", "protocol": 2,
            "video": {"codec": self.video_codec, "width": self.width, "height": self.height, "fps": self.fps},
            "audio": {
                "codec": self.audio_codec, "sample_rate": self.sample_rate,
                "channels": self.channels, "frame_ms": self.frame_ms,
            },
            "client": {"name": self.client_name, "version": self.client_version, "obs_version": self.obs_version},
        })
    }
}

/// The service's attachment-end reason, preserving sanitized unknown values for future versions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetachReason {
    Stopped,
    Shutdown,
    Incompatible,
    Oversized,
    Unknown(String),
}

impl DetachReason {
    pub fn from_service(value: &str) -> Self {
        match value {
            "stopped" => Self::Stopped,
            "shutdown" => Self::Shutdown,
            "incompatible" => Self::Incompatible,
            "oversized" => Self::Oversized,
            other => Self::Unknown(service_text(other)),
        }
    }

    /// Only an explicit stop or service shutdown is a clean service detach.
    pub fn is_clean(&self) -> bool {
        matches!(self, Self::Stopped | Self::Shutdown)
    }

    /// What OBS shows for a clean detach: never this plugin's own Stop, which ends the output before the session.
    pub fn session_end(&self, message: &str, stop: Option<&StopCode>) -> SessionEnd {
        let said = message.trim().trim_end_matches('.');
        let ended = if said.is_empty() || said == SESSION_ENDED {
            "The Discord session ended".to_owned()
        } else {
            format!("The Discord session ended: {said}")
        };
        let code = stop.map_or("", |s| s.code.as_str());
        let end = |headline, fix, text: String, then: &str| SessionEnd {
            headline,
            fix,
            message: format!("{text}. {then}"),
            login: None,
        };
        if *self == Self::Shutdown {
            return end(
                "Service stopped",
                START_AGAIN,
                "The D-Streamy service stopped, so the Discord session ended".into(),
                "Press Start to go live again",
            );
        }
        match code {
            "invalid_token" => SessionEnd {
                login: Some(Relogin::Again),
                ..end(
                    "Discord login expired",
                    "Log in again, then press Start",
                    "The Discord session ended: Discord rejected the saved login".into(),
                    "Log in again, then press Start",
                )
            },
            "identify_limit" => {
                let wait = stop
                    .and_then(|s| s.retry_after_secs)
                    .map_or("Try again later".to_owned(), |secs| {
                        format!("Try again in {}", human_wait(secs))
                    });
                end(
                    crate::control::IDENTIFY_PAUSED,
                    "Try again later",
                    "The Discord session ended: Discord kept ending it, so D-Streamy stopped logging in \
                     to protect the account"
                        .into(),
                    &wait,
                )
            }
            "e2ee_unavailable" => end(
                "No end-to-end encryption",
                "Turn off require_e2ee to stream",
                ended,
                "Turn off `require_e2ee` in the D-Streamy config to stream without it, or try again later",
            ),
            "voice_taken_over" => end(
                "Voice used elsewhere",
                "Leave voice there, then Start",
                ended,
                "Leave voice on the other device, then press Start",
            ),
            "gave_up" => end(
                "Connection lost",
                "Check the network, then Start",
                ended,
                "Check the network, then press Start",
            ),
            "gateway_closed" => end(
                "Discord disconnected",
                "Check the network, then Start",
                ended,
                "Check the network, then press Start",
            ),
            "capture_ended" => end(
                "No media from OBS",
                "Check OBS's output, then Start",
                ended,
                "Check that OBS still feeds D-Streamy's output, then press Start",
            ),
            "source_failed" => end(
                "Capture failed",
                "See the D-Streamy log",
                ended,
                "See the D-Streamy log, then press Start",
            ),
            "task_ended" => end(
                "Session failed",
                "See the D-Streamy log",
                ended,
                "See the D-Streamy log, then press Start",
            ),
            "requested" => end(
                "Stopped elsewhere",
                START_AGAIN,
                ended,
                "Press Start to go live again",
            ),
            "grace_expired" => end(
                "No media from OBS",
                START_AGAIN,
                ended,
                "Press Start to go live again",
            ),
            _ => end(
                "Stream stopped",
                START_AGAIN,
                ended,
                "Press Start to go live again",
            ),
        }
    }
}

const SESSION_ENDED: &str = "the session ended";
const START_AGAIN: &str = "Press Start to go live again";

/// A retry delay in words: "a minute", "12 min", "about 3 h".
fn human_wait(secs: u64) -> String {
    match secs {
        0..=90 => "a minute".into(),
        91..=5_399 => format!("{} min", secs.div_ceil(60)),
        _ => format!("about {} h", (secs + 1_800) / 3_600),
    }
}

/// A `stopped` detach's machine-readable stop reason, as serve's `status` names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopCode {
    pub code: String,
    pub retry_after_secs: Option<u64>,
}

/// How the panel shows a session the service ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEnd {
    pub headline: &'static str,
    pub fix: &'static str,
    pub message: String,
    pub login: Option<Relogin>,
}

impl std::fmt::Display for DetachReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Stopped => "stopped",
            Self::Shutdown => "shutdown",
            Self::Incompatible => "incompatible",
            Self::Oversized => "oversized",
            Self::Unknown(reason) => reason,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum End<T> {
    /// The stop future fired; queued packets and an end-of-stream frame were sent if the connection allowed.
    Stopped(T),
    Detached {
        reason: DetachReason,
        message: String,
        stop: Option<StopCode>,
    },
    /// IPC ended without a service detach reason; reconnection may still be attempted.
    Closed(String),
    /// A packet could not satisfy the producer's format or size requirements.
    Refused(String),
}

/// Service text as OBS may show it: no control or bidi-override characters, at most `MAX_SERVICE_TEXT` characters.
pub(crate) fn service_text(text: &str) -> String {
    let hidden = |c: char| {
        c.is_control()
            || matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{2028}' | '\u{2029}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
    };
    let long = text.chars().nth(MAX_SERVICE_TEXT).is_some();
    let keep = if long {
        MAX_SERVICE_TEXT - 1
    } else {
        MAX_SERVICE_TEXT
    };
    let mut out: String = text
        .chars()
        .take(keep)
        .map(|c| if hidden(c) { ' ' } else { c })
        .collect();
    if long {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_matches_the_golden_producer_hello() {
        let hello = Hello {
            video_codec: "h264".into(),
            width: 1280,
            height: 720,
            fps: 30,
            audio_codec: "opus".into(),
            sample_rate: 48_000,
            channels: 2,
            frame_ms: 20,
            client_name: "obs-dstreamy".into(),
            client_version: "0.1.0".into(),
            obs_version: "32.2.2".into(),
        };
        let fixture = include_str!("../../dstreamy-ipc/tests/fixtures/control/hello.ndjson");
        let golden: Value = fixture
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .find(|v| v["role"] == "producer")
            .unwrap();
        assert_eq!(hello.to_json(), golden);
    }

    fn stop(code: &str, retry_after_secs: Option<u64>) -> StopCode {
        StopCode {
            code: code.into(),
            retry_after_secs,
        }
    }

    #[test]
    fn each_stop_code_gets_its_own_headline_fix_and_next_step() {
        let stopped = DetachReason::Stopped;
        let token = stopped.session_end(
            "Discord rejected the login token; log in again",
            Some(&stop("invalid_token", None)),
        );
        assert_eq!(token.headline, "Discord login expired");
        assert_eq!(token.login, Some(Relogin::Again));
        assert!(
            token.message.ends_with("Log in again, then press Start"),
            "{token:?}"
        );
        assert!(!token.message.contains("go live again"), "{token:?}");

        let limit = stopped.session_end(
            "Try again later.",
            Some(&stop("identify_limit", Some(3 * 3_600 - 100))),
        );
        assert_eq!(limit.fix, "Try again later");
        assert!(
            limit.message.ends_with("Try again in about 3 h"),
            "{limit:?}"
        );
        assert!(!limit.message.contains("Press Start"), "{limit:?}");
        assert!(
            stopped
                .session_end("", Some(&stop("identify_limit", None)))
                .message
                .ends_with("Try again later")
        );

        let e2ee = stopped.session_end("no encryption", Some(&stop("e2ee_unavailable", None)));
        assert!(e2ee.message.contains("require_e2ee"), "{e2ee:?}");
        for code in ["gateway_closed", "capture_ended", "source_failed"] {
            let end = stopped.session_end("x", Some(&stop(code, None)));
            assert_ne!(
                end.headline, "Stream stopped",
                "{code} gets its own headline"
            );
            assert_ne!(end.fix, START_AGAIN, "{code} says what to check");
        }
        let unknown = stopped.session_end("something new", Some(&stop("from_the_future", None)));
        assert_eq!(unknown.headline, "Stream stopped");
        assert_eq!(
            unknown.message,
            "The Discord session ended: something new. Press Start to go live again"
        );
        assert_eq!(
            stopped.session_end("the session ended", None).message,
            "The Discord session ended. Press Start to go live again"
        );
        for (code, fix) in [
            ("voice_taken_over", "Leave voice there, then Start"),
            ("gave_up", "Check the network, then Start"),
            ("task_ended", "See the D-Streamy log"),
        ] {
            let end = stopped.session_end("x", Some(&stop(code, None)));
            assert_eq!(end.fix, fix, "{code}");
            assert!(
                end.fix.chars().count() <= 35 && end.headline.len() <= 28,
                "{code}"
            );
        }
    }

    #[test]
    fn a_retry_delay_reads_as_words() {
        assert_eq!(human_wait(30), "a minute");
        assert_eq!(human_wait(600), "10 min");
        assert_eq!(human_wait(5_400), "about 2 h");
        assert_eq!(human_wait(86_000), "about 24 h");
    }

    #[test]
    fn a_clean_detach_says_why_the_session_ended() {
        let stopped = DetachReason::from_service("stopped");
        assert_eq!(
            stopped
                .session_end("another device of this account joined voice", None)
                .message,
            "The Discord session ended: another device of this account joined voice. \
             Press Start to go live again"
        );
        assert_eq!(
            stopped.session_end(" ", None).message,
            "The Discord session ended. Press Start to go live again"
        );
        assert_eq!(
            DetachReason::from_service("shutdown")
                .session_end("the service is shutting down", None)
                .message,
            "The D-Streamy service stopped, so the Discord session ended. Press Start to go live again"
        );
    }
}
