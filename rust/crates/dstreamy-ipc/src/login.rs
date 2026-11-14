//! Policy for graphical-session variables passed to a control login window.

/// The variables a control `login` may pass for its window: the client's graphical session, which serve may lack.
pub const LOGIN_ENV: [&str; 6] = [
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "XDG_SESSION_TYPE",
    "XDG_CURRENT_DESKTOP",
    "DBUS_SESSION_BUS_ADDRESS",
];

/// D-Bus unixexec and autolaunch addresses can run programs, unlike these graphical-session values.
pub fn login_env_value_ok(name: &str, value: &str) -> bool {
    const MAX_VALUE: usize = 4096;
    if value.len() > MAX_VALUE || value.chars().any(char::is_control) {
        return false;
    }
    match name {
        "XAUTHORITY" => value.starts_with('/'),
        "WAYLAND_DISPLAY" => !value.is_empty() && (value.starts_with('/') || !value.contains('/')),
        "DISPLAY" => {
            value.contains(':')
                && value
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '-' | '_' | '/'))
        }
        "XDG_SESSION_TYPE" => value.chars().all(|c| c.is_ascii_alphanumeric()),
        "DBUS_SESSION_BUS_ADDRESS" => value.split(';').all(|entry| {
            ["unix:", "tcp:", "nonce-tcp:", "systemd:", "launchd:"]
                .iter()
                .any(|transport| entry.starts_with(transport))
        }),
        _ => true,
    }
}

/// How long serve keeps a login window open before it closes it.
pub const LOGIN_WINDOW_LIMIT: std::time::Duration = std::time::Duration::from_secs(600);
