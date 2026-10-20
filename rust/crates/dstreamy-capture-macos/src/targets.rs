use std::fmt;
use std::str::FromStr;

use dstreamy_media::MediaError;

/// Target ids are `display:<CGDirectDisplayID>` or `window:<CGWindowID>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetId {
    Display(u32),
    Window(u32),
}

impl fmt::Display for TargetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Display(id) => write!(f, "display:{id}"),
            Self::Window(id) => write!(f, "window:{id}"),
        }
    }
}

impl FromStr for TargetId {
    type Err = MediaError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || MediaError::InvalidConfig(format!("target id {s:?}"));
        let (kind, id) = s.split_once(':').ok_or_else(invalid)?;
        let id = id.parse().map_err(|_| invalid())?;
        match kind {
            "display" => Ok(Self::Display(id)),
            "window" => Ok(Self::Window(id)),
            _ => Err(invalid()),
        }
    }
}

const EXCLUDED_BUNDLE_IDS: &[&str] = &[
    "com.apple.dock",
    "com.apple.controlcenter",
    "com.apple.notificationcenterui",
    "com.apple.WindowManager",
    "com.apple.systemuiserver",
    "com.apple.loginwindow",
    "com.apple.Spotlight",
];

const EXCLUDED_APP_NAMES: &[&str] = &[
    "Dock",
    "Control Center",
    "Notification Center",
    "Window Server",
    "Wallpaper",
    "StatusIndicator",
    "SystemUIServer",
];

const MIN_WINDOW_POINTS: f64 = 50.0;

/// True for titled application windows a user would pick, excluding system chrome and tiny windows.
pub fn is_user_window(
    app_name: &str,
    bundle_id: &str,
    title: &str,
    width: f64,
    height: f64,
) -> bool {
    !app_name.is_empty()
        && !title.is_empty()
        && !EXCLUDED_BUNDLE_IDS.contains(&bundle_id)
        && !EXCLUDED_APP_NAMES.contains(&app_name)
        && !title.contains("Backstop")
        && width >= MIN_WINDOW_POINTS
        && height >= MIN_WINDOW_POINTS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_ids_round_trip() {
        for id in [TargetId::Display(1), TargetId::Window(4242)] {
            assert_eq!(id.to_string().parse::<TargetId>().unwrap(), id);
        }
        assert_eq!("window:7".parse::<TargetId>().unwrap(), TargetId::Window(7));
    }

    #[test]
    fn malformed_target_ids_are_rejected() {
        for bad in [
            "",
            "display",
            "display:",
            "screen:1",
            "window:x",
            "window:-1",
        ] {
            assert!(bad.parse::<TargetId>().is_err(), "{bad}");
        }
    }

    #[test]
    fn user_windows_are_kept_and_system_windows_dropped() {
        let window = |app, bundle, title, width| is_user_window(app, bundle, title, width, 600.0);
        assert!(window("Safari", "com.apple.Safari", "Start", 800.0));
        assert!(!window("Dock", "com.apple.dock", "Dock", 800.0));
        assert!(!window("Foo", "com.apple.controlcenter", "x", 800.0));
        assert!(!window("Safari", "com.apple.Safari", "", 800.0));
        assert!(!window("", "x", "Title", 800.0));
        assert!(!window(
            "Finder",
            "com.apple.finder",
            "Backstop Menubar",
            800.0
        ));
        assert!(!window("Safari", "com.apple.Safari", "Tiny", 40.0));
    }
}
