//! What the plugin and the producer connection share on every platform.

use serde_json::{Value, json};

/// Service text shown in OBS is cut to this many characters.
pub const MAX_SERVICE_TEXT: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Debug,
    Info,
    Warn,
}

pub trait Log {
    fn log(&self, level: Level, message: &str);
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

/// Why a run ended.
#[derive(Debug, PartialEq, Eq)]
pub enum End<T> {
    /// The stop future fired; queued packets and an end-of-stream frame were sent if the connection allowed.
    Stopped(T),
    Detached {
        reason: String,
        message: String,
    },
    Closed(String),
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
}
