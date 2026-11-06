//! The status-bar item: a state dot, four counters with icons and the stream's duration.

use serde::Serialize;

use crate::dock_view::Tone;
use crate::live::{LiveStats, bitrate, clock};

/// Missed frames within [`crate::live::MISSED_WINDOW_MS`] from which the counter turns red.
pub const MISSED_ERROR: u64 = 30;
/// Reconnects within [`crate::live::RECONNECT_WINDOW_MS`] from which the counter turns red.
pub const RECONNECTS_ERROR: u64 = 3;
/// What Discord gets from an Opus track of 20 ms frames.
pub const AUDIO_PER_SEC: u64 = 50;
/// Fewer audio frames a second than this reach Discord: audio counts as lost.
pub const AUDIO_LOST_BELOW: u64 = AUDIO_PER_SEC / 2;
/// Shown for a figure nobody reported yet.
pub const UNKNOWN: &str = "–";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Idle,
    Busy,
    Live,
    /// Live, but a counter shows a problem or the service is reconnecting.
    Degraded,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Normal,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Counter {
    pub id: &'static str,
    /// The icon's name in the panel library's resources.
    pub icon: &'static str,
    pub text: String,
    pub level: Level,
    /// The counter in words, for screen readers.
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusBar {
    pub state: State,
    pub counters: Vec<Counter>,
    /// Empty while not live.
    pub duration: String,
    pub tooltip: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audio {
    Unknown,
    Ok,
    /// Frames reach Discord, but no audio source feeds the track.
    Silent,
    /// Too few frames reach Discord.
    Lost,
}

pub fn audio(stats: &LiveStats) -> Audio {
    let service = stats.service.filter(|s| !s.reconnecting);
    match (service.and_then(|s| s.audio_per_sec), stats.audio_fed) {
        (Some(n), _) if n < AUDIO_LOST_BELOW => Audio::Lost,
        (_, Some(false)) => Audio::Silent,
        (Some(_), _) => Audio::Ok,
        (None, _) => Audio::Unknown,
    }
}

fn missed_level(stats: &LiveStats) -> Level {
    match stats.missed_recent {
        0 => Level::Normal,
        n if n < MISSED_ERROR => Level::Warn,
        _ => Level::Error,
    }
}

fn reconnects_level(stats: &LiveStats) -> Level {
    match stats.service {
        None => Level::Normal,
        Some(_) if stats.reconnects_recent >= RECONNECTS_ERROR => Level::Error,
        Some(s) if s.reconnecting || stats.reconnects_recent > 0 => Level::Warn,
        Some(_) => Level::Normal,
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn counter(
    id: &'static str,
    icon: &'static str,
    text: String,
    level: Level,
    label: String,
) -> Counter {
    Counter {
        id,
        icon,
        text,
        level,
        label,
    }
}

/// Missed frames, audio, reconnects and viewers; dashes while not live.
pub fn counters(live: Option<&LiveStats>) -> Vec<Counter> {
    let Some(stats) = live else {
        let unknown = |id, icon, label: &str| {
            counter(
                id,
                icon,
                UNKNOWN.to_owned(),
                Level::Normal,
                format!("{label} unknown"),
            )
        };
        return vec![
            unknown("missed", "missed", "Missed frames"),
            unknown("audio", "audio", "Audio"),
            unknown("reconnects", "reconnects", "Reconnects"),
            unknown("viewers", "viewers", "Viewers"),
        ];
    };
    let missed = stats.missed();
    let (icon, text, level) = match audio(stats) {
        Audio::Unknown => ("audio", UNKNOWN, Level::Normal),
        Audio::Ok => ("audio", "ok", Level::Normal),
        Audio::Silent => ("audio-silent", "silent", Level::Warn),
        Audio::Lost => ("audio-lost", "lost", Level::Error),
    };
    let audio_label = match text {
        UNKNOWN => "Audio unknown".to_owned(),
        _ => format!("Audio {text}"),
    };
    let reconnects = stats.service.map(|s| s.reconnects);
    vec![
        counter(
            "missed",
            "missed",
            missed.to_string(),
            missed_level(stats),
            plural(missed, "missed frame", "missed frames"),
        ),
        counter("audio", icon, text.to_owned(), level, audio_label),
        counter(
            "reconnects",
            "reconnects",
            reconnects.map_or_else(|| UNKNOWN.to_owned(), |n| n.to_string()),
            reconnects_level(stats),
            reconnects.map_or_else(
                || "Reconnects unknown".to_owned(),
                |n| plural(n, "reconnect", "reconnects"),
            ),
        ),
        counter(
            "viewers",
            "viewers",
            stats
                .viewers
                .map_or_else(|| UNKNOWN.to_owned(), |n| n.to_string()),
            Level::Normal,
            stats.viewers.map_or_else(
                || "Viewers unknown".to_owned(),
                |n| plural(u64::from(n), "viewer", "viewers"),
            ),
        ),
    ]
}

/// Each figure in words, with what the bar leaves out: bitrate, frame rate against the target, frame gaps.
pub fn status_lines(stats: &LiveStats) -> Vec<String> {
    let mut lines = Vec::new();
    let mut missed = format!("Missed frames: {}", stats.missed());
    if let Some(s) = stats.service {
        missed.push_str(&format!(
            " ({} in OBS, {} in the service)",
            stats.dropped, s.dropped_video
        ));
    }
    if stats.missed_recent > 0 {
        missed.push_str(&format!(", {} in the last 10 seconds", stats.missed_recent));
    }
    lines.push(missed);
    let per_sec = stats.service.and_then(|s| s.audio_per_sec);
    lines.push(match (audio(stats), per_sec) {
        (Audio::Ok, Some(n)) => format!("Audio: ok, {n} of {AUDIO_PER_SEC} frames a second"),
        (Audio::Lost, Some(n)) => {
            format!("Audio: lost, only {n} of {AUDIO_PER_SEC} frames a second reach Discord")
        }
        (Audio::Silent, _) => "Audio: silent, no audio source feeds the Discord track".to_owned(),
        _ if stats.service.is_some_and(|s| s.reconnecting) => {
            "Audio: unknown while reconnecting".to_owned()
        }
        _ => "Audio: unknown".to_owned(),
    });
    lines.push(match stats.service {
        Some(s) if s.reconnecting => format!("Reconnects: {}, reconnecting now", s.reconnects),
        Some(s) => format!("Reconnects: {}", s.reconnects),
        None => "Reconnects: unknown".to_owned(),
    });
    lines.push(match stats.viewers {
        Some(n) => format!("Viewers: {n}"),
        None => "Viewers: unknown".to_owned(),
    });
    lines.push(format!("Duration: {}", clock(stats.uptime_ms)));
    let mut video = Vec::new();
    if let Some(r) = stats.rates {
        video.push(bitrate(r.bits_per_sec));
        video.push(match stats.target_fps {
            Some(target) => format!("{:.0} of {target} fps", r.fps),
            None => format!("{:.0} fps", r.fps),
        });
    }
    if let Some((p95, max)) = stats
        .service
        .and_then(|s| Some((s.jitter_p95_ms?, s.jitter_max_ms?)))
    {
        video.push(format!(
            "frame gaps {p95} ms (95th percentile), {max} ms at most"
        ));
    }
    if !video.is_empty() {
        lines.push(format!("Video: {}", video.join(", ")));
    }
    lines
}

/// The bar for `tone`; `live` while D-Streamy's output runs, `tooltip` as the panel's state tooltip.
pub fn status_bar(tone: Tone, live: Option<&LiveStats>, tooltip: String) -> StatusBar {
    let counters = counters(live);
    let troubled = counters.iter().any(|c| c.level > Level::Normal)
        || live.and_then(|s| s.service).is_some_and(|s| s.reconnecting);
    StatusBar {
        state: match tone {
            Tone::Idle => State::Idle,
            Tone::Busy => State::Busy,
            Tone::Attention => State::Error,
            Tone::Live if troubled => State::Degraded,
            Tone::Live => State::Live,
        },
        counters,
        duration: live.map(|s| clock(s.uptime_ms)).unwrap_or_default(),
        tooltip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::{Rates, ServiceHealth};

    fn healthy() -> LiveStats {
        LiveStats {
            uptime_ms: 754_000,
            rates: Some(Rates {
                bits_per_sec: 6_000_000.0,
                fps: 59.6,
            }),
            dropped: 0,
            viewers: Some(3),
            service: Some(ServiceHealth {
                reconnects: 0,
                audio_per_sec: Some(50),
                fps: Some(60),
                jitter_p95_ms: Some(18),
                jitter_max_ms: Some(41),
                bitrate_kbps: Some(5800),
                ..ServiceHealth::default()
            }),
            target_fps: Some(60),
            audio_fed: Some(true),
            ..LiveStats::default()
        }
    }

    fn texts(bar: &StatusBar) -> Vec<(&str, &str, Level)> {
        bar.counters
            .iter()
            .map(|c| (c.id, c.text.as_str(), c.level))
            .collect()
    }

    #[test]
    fn a_healthy_stream_shows_zeros_in_a_green_bar() {
        let stats = healthy();
        let bar = status_bar(Tone::Live, Some(&stats), String::new());
        assert_eq!(bar.state, State::Live);
        assert_eq!(
            texts(&bar),
            [
                ("missed", "0", Level::Normal),
                ("audio", "ok", Level::Normal),
                ("reconnects", "0", Level::Normal),
                ("viewers", "3", Level::Normal),
            ]
        );
        assert_eq!(bar.duration, "12:34");
        let icons: Vec<&str> = bar.counters.iter().map(|c| c.icon).collect();
        assert_eq!(icons, ["missed", "audio", "reconnects", "viewers"]);
        let labels: Vec<&str> = bar.counters.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(
            labels,
            ["0 missed frames", "Audio ok", "0 reconnects", "3 viewers"]
        );
    }

    #[test]
    fn not_live_shows_every_counter_as_a_dash_without_a_duration() {
        for (tone, state) in [
            (Tone::Idle, State::Idle),
            (Tone::Busy, State::Busy),
            (Tone::Attention, State::Error),
        ] {
            let bar = status_bar(tone, None, "Idle".into());
            assert_eq!(bar.state, state);
            assert!(
                bar.counters
                    .iter()
                    .all(|c| c.text == UNKNOWN && c.level == Level::Normal)
            );
            assert_eq!(bar.counters.len(), 4);
            assert_eq!(bar.duration, "");
            assert_eq!(bar.tooltip, "Idle");
        }
    }

    #[test]
    fn recent_missed_frames_turn_amber_then_red() {
        let mut stats = healthy();
        stats.dropped = 2;
        stats.service.as_mut().unwrap().dropped_video = 3;
        let bar = status_bar(Tone::Live, Some(&stats), String::new());
        assert_eq!(
            texts(&bar)[0],
            ("missed", "5", Level::Normal),
            "all of them are old"
        );
        assert_eq!(bar.state, State::Live);
        stats.missed_recent = 1;
        let bar = status_bar(Tone::Live, Some(&stats), String::new());
        assert_eq!(texts(&bar)[0].2, Level::Warn);
        assert_eq!(bar.state, State::Degraded);
        stats.missed_recent = MISSED_ERROR - 1;
        assert_eq!(missed_level(&stats), Level::Warn);
        stats.missed_recent = MISSED_ERROR;
        assert_eq!(missed_level(&stats), Level::Error);
        assert_eq!(
            status_bar(Tone::Live, Some(&stats), String::new()).state,
            State::Degraded,
            "only a session problem makes the dot red"
        );
    }

    #[test]
    fn audio_is_lost_below_half_the_frames_and_silent_without_a_source() {
        let mut stats = healthy();
        let set = |stats: &mut LiveStats, n: Option<u64>| {
            stats.service.as_mut().unwrap().audio_per_sec = n;
        };
        set(&mut stats, Some(AUDIO_LOST_BELOW));
        assert_eq!(audio(&stats), Audio::Ok);
        set(&mut stats, Some(AUDIO_LOST_BELOW - 1));
        assert_eq!(audio(&stats), Audio::Lost);
        let bar = status_bar(Tone::Live, Some(&stats), String::new());
        assert_eq!(texts(&bar)[1], ("audio", "lost", Level::Error));
        assert_eq!(bar.counters[1].icon, "audio-lost");
        set(&mut stats, Some(50));
        stats.audio_fed = Some(false);
        assert_eq!(audio(&stats), Audio::Silent);
        let bar = status_bar(Tone::Live, Some(&stats), String::new());
        assert_eq!(texts(&bar)[1], ("audio", "silent", Level::Warn));
        assert_eq!(bar.counters[1].icon, "audio-silent");
        set(&mut stats, Some(0));
        assert_eq!(audio(&stats), Audio::Lost, "lost outranks silent");
        set(&mut stats, None);
        stats.audio_fed = Some(true);
        assert_eq!(
            audio(&stats),
            Audio::Unknown,
            "before serve's first interval"
        );
        stats.audio_fed = None;
        stats.service = None;
        assert_eq!(audio(&stats), Audio::Unknown);
        stats.audio_fed = Some(false);
        assert_eq!(audio(&stats), Audio::Silent, "the mixer needs no service");
    }

    #[test]
    fn reconnecting_turns_the_dot_and_counter_amber() {
        let mut stats = healthy();
        {
            let s = stats.service.as_mut().unwrap();
            s.reconnecting = true;
            s.reconnects = 1;
        }
        stats.reconnects_recent = 1;
        let bar = status_bar(Tone::Live, Some(&stats), String::new());
        assert_eq!(bar.state, State::Degraded);
        assert_eq!(texts(&bar)[2], ("reconnects", "1", Level::Warn));
        assert_eq!(
            texts(&bar)[1],
            ("audio", "–", Level::Normal),
            "stale while reconnecting"
        );
        stats.service.as_mut().unwrap().reconnecting = false;
        assert_eq!(reconnects_level(&stats), Level::Warn, "a minute after it");
        stats.reconnects_recent = 0;
        assert_eq!(reconnects_level(&stats), Level::Normal);
        stats.reconnects_recent = RECONNECTS_ERROR;
        assert_eq!(reconnects_level(&stats), Level::Error);
    }

    #[test]
    fn an_old_service_leaves_its_figures_unknown() {
        let mut stats = healthy();
        stats.service = None;
        stats.viewers = None;
        stats.dropped = 4;
        let bar = status_bar(Tone::Live, Some(&stats), String::new());
        assert_eq!(
            texts(&bar),
            [
                ("missed", "4", Level::Normal),
                ("audio", "–", Level::Normal),
                ("reconnects", "–", Level::Normal),
                ("viewers", "–", Level::Normal),
            ]
        );
        assert_eq!(bar.state, State::Live);
        assert_eq!(
            status_lines(&stats),
            [
                "Missed frames: 4",
                "Audio: unknown",
                "Reconnects: unknown",
                "Viewers: unknown",
                "Duration: 12:34",
                "Video: 6.0 Mbit/s, 60 of 60 fps",
            ]
        );
    }

    #[test]
    fn the_tooltip_has_every_figure_in_words() {
        let mut stats = healthy();
        stats.dropped = 1;
        stats.missed_recent = 3;
        {
            let s = stats.service.as_mut().unwrap();
            s.dropped_video = 2;
            s.reconnects = 2;
        }
        assert_eq!(
            status_lines(&stats),
            [
                "Missed frames: 3 (1 in OBS, 2 in the service), 3 in the last 10 seconds",
                "Audio: ok, 50 of 50 frames a second",
                "Reconnects: 2",
                "Viewers: 3",
                "Duration: 12:34",
                "Video: 6.0 Mbit/s, 60 of 60 fps, frame gaps 18 ms (95th percentile), 41 ms at most",
            ]
        );
        stats.service.as_mut().unwrap().audio_per_sec = Some(10);
        stats.service.as_mut().unwrap().reconnecting = true;
        let lines = status_lines(&stats);
        assert_eq!(lines[1], "Audio: unknown while reconnecting");
        assert_eq!(lines[2], "Reconnects: 2, reconnecting now");
        stats.service.as_mut().unwrap().reconnecting = false;
        assert_eq!(
            status_lines(&stats)[1],
            "Audio: lost, only 10 of 50 frames a second reach Discord"
        );
        stats.audio_fed = Some(false);
        stats.service.as_mut().unwrap().audio_per_sec = Some(50);
        assert_eq!(
            status_lines(&stats)[1],
            "Audio: silent, no audio source feeds the Discord track"
        );
        stats.rates = None;
        stats.service = None;
        assert!(!status_lines(&stats).iter().any(|l| l.starts_with("Video")));
    }

    #[test]
    fn the_view_serializes_for_the_panel_library() {
        let bar = status_bar(Tone::Live, Some(&healthy()), "Live".into());
        let json = serde_json::to_value(&bar).unwrap();
        assert_eq!(json["state"], "live");
        assert_eq!(json["duration"], "12:34");
        assert_eq!(json["counters"][1]["level"], "normal");
        assert_eq!(
            json["counters"][0],
            serde_json::json!({
                "id": "missed", "icon": "missed", "text": "0", "level": "normal",
                "label": "0 missed frames"
            })
        );
    }
}
