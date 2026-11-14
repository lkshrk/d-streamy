//! The pages and script the login window loads, and the text each failure shows.

use super::session::DISCORD_HOSTS;
use crate::login_capture::{Action, Failure, Recovery};

pub(super) fn capture_script(nonce: &str) -> String {
    let hosts = DISCORD_HOSTS.map(|host| format!("\"{host}\"")).join(", ");
    CAPTURE_SCRIPT
        .replace("{hosts}", &hosts)
        .replace("{nonce}", nonce)
}

const RECOVERY_PAGE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>D-Streamy</title>
<style>
  :root { color-scheme: dark; }
  html, body { margin: 0; height: 100%; background: #313338; color: #dbdee1;
    font: 15px/1.45 system-ui, -apple-system, "Segoe UI", sans-serif; }
  main { box-sizing: border-box; height: 100%; padding: 48px 40px;
    display: flex; flex-direction: column; justify-content: center; gap: 14px; }
  h1 { margin: 0; font-size: 22px; color: #f2f3f5; }
  p { margin: 0; }
  .hint { color: #b5bac1; }
  .actions { display: flex; flex-direction: column; gap: 10px; margin-top: 18px; }
  button { font: inherit; padding: 11px 16px; border-radius: 6px; border: 0;
    background: #4e5058; color: #f2f3f5; cursor: pointer; }
  button:hover { background: #6d6f78; }
  button.primary { background: #5865f2; }
  button.primary:hover { background: #4752c4; }
  button:focus-visible { outline: 2px solid #f2f3f5; outline-offset: 2px; }
</style>
</head>
<body>
<main>
  <h1>{title}</h1>
  <p>{message}</p>
  <p class="hint">{hint}</p>
  <div class="actions">{actions}</div>
</main>
<script>
(() => {
  const channel = window.webkit.messageHandlers.ipc;
  const postMessage = channel.postMessage.bind(channel);
  const nonce = "{nonce}";
  const post = (m) => { try { postMessage(nonce + "|" + m); } catch (_) {} };
  for (const b of document.querySelectorAll("button[data-action]")) {
    b.addEventListener("click", () => post("action:" + b.dataset.action));
  }
  window.addEventListener("keydown", (e) => { if (e.key === "Escape") post("close"); });
  const first = document.querySelector("button");
  if (first) first.focus();
  post("ready");
})();
</script>
</body>
</html>
"##;

pub(super) fn recovery_buttons(recovery: Recovery) -> String {
    recovery
        .actions()
        .iter()
        .map(|&action| {
            let (class, label) = match action {
                Action::Retry => (r#" class="primary""#, "Retry"),
                Action::UseQr => ("", "Use QR instead"),
                Action::UseToken => ("", "Paste token"),
                Action::Cancel => ("", "Cancel"),
            };
            format!(
                r#"<button{class} data-action="{}">{label}</button>"#,
                action.name()
            )
        })
        .collect::<Vec<_>>()
        .join("\n    ")
}

#[cfg(target_os = "linux")]
pub(super) const STATUS_PAGE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<style>
  :root { color-scheme: dark; }
  html, body { margin: 0; height: 100%; background: #2b2d31; color: #dbdee1;
    font: 14px/1.4 system-ui, -apple-system, "Segoe UI", sans-serif; }
  main { box-sizing: border-box; height: 100%; padding: 0 16px;
    display: flex; align-items: center; gap: 12px; }
  p { margin: 0; flex: 1; }
  button { font: inherit; padding: 7px 14px; border-radius: 6px; border: 0;
    background: #4e5058; color: #f2f3f5; cursor: pointer; }
  button:hover { background: #6d6f78; }
</style>
</head>
<body>
<main>
  <p role="status">D-Streamy is verifying the login with Discord. Nothing is saved until it succeeds.</p>
  <button>Cancel</button>
</main>
<script>
(() => {
  const channel = window.webkit.messageHandlers.ipc;
  const postMessage = channel.postMessage.bind(channel);
  const nonce = "{nonce}";
  document.querySelector("button").addEventListener("click", () => {
    try { postMessage(nonce + "|action:cancel"); } catch (_) {}
  });
})();
</script>
</body>
</html>
"##;

const CAPTURE_SCRIPT: &str = r##"
(() => {
  document.documentElement.style.background = "#313338";
  if (location.protocol !== "https:" || ![{hosts}].includes(location.host)) return;
  const channel = window.webkit.messageHandlers.ipc;
  const postMessage = channel.postMessage.bind(channel);
  const nonce = "{nonce}";
  const post = (message) => {
    try { postMessage(nonce + "|" + message); } catch (_) {}
  };
  window.addEventListener("keydown", (e) => {
    if (e.isTrusted && e.key === "Escape") post("close");
  }, true);
  const posted = new Set();
  const send = (value) => {
    try {
      if (typeof value !== "string") return;
      let v = value.trim();
      if (v.length > 1 && v.startsWith('"') && v.endsWith('"')) v = v.slice(1, -1);
      const shaped = v.startsWith("mfa.")
        ? /^mfa\.[\w-]{20,}$/.test(v)
        : v.length >= 30 && /^[\w-]+\.[\w-]+\.[\w-]+$/.test(v);
      if (!shaped || posted.has(v)) return;
      posted.add(v);
      post("candidate:" + v);
    } catch (_) {}
  };
  const fromHeaders = (source) => {
    try {
      if (source) send(new Headers(source).get("authorization"));
    } catch (_) {}
  };
  const setHeader = XMLHttpRequest.prototype.setRequestHeader;
  XMLHttpRequest.prototype.setRequestHeader = function (name, value) {
    try {
      if (String(name).toLowerCase() === "authorization") send(value);
    } catch (_) {}
    return setHeader.apply(this, arguments);
  };
  const origFetch = window.fetch;
  window.fetch = function (input, init) {
    try {
      fromHeaders(init && init.headers);
      if (input instanceof Request) fromHeaders(input.headers);
    } catch (_) {}
    return origFetch.apply(this, arguments);
  };
  const isTokenKey = (key) => String(key).toLowerCase().includes("token");
  const getItem = Storage.prototype.getItem;
  Storage.prototype.getItem = function (key) {
    const value = getItem.apply(this, arguments);
    try {
      if (isTokenKey(key)) send(value);
    } catch (_) {}
    return value;
  };
  const setItem = Storage.prototype.setItem;
  Storage.prototype.setItem = function (key, value) {
    try {
      if (isTokenKey(key)) send(value);
    } catch (_) {}
    return setItem.apply(this, arguments);
  };
  let signedIn = false;
  const checkSignedIn = () => {
    if (signedIn) return;
    if (location.pathname.startsWith("/channels") || location.pathname.startsWith("/app")) {
      signedIn = true;
      post("signed_in");
    }
  };
  checkSignedIn();
  setInterval(checkSignedIn, 500);
  let loaded = false;
  const checkLoaded = () => {
    if (loaded) return;
    if (document.readyState !== "loading" && document.body) {
      loaded = true;
      clearInterval(loadTimer);
      post("page_loaded");
    }
  };
  const loadTimer = setInterval(checkLoaded, 250);
  checkLoaded();
})();
"##;

pub(super) fn attention_line(failure: Failure) -> String {
    format!(
        "The login needs your attention in the window: {}",
        recovery_text(failure).1
    )
}

pub(super) fn quiet_line(failure: Failure) -> String {
    format!(
        "{} The window stays on Discord: finish the login there, or close it to cancel.",
        recovery_text(failure).1
    )
}

pub(super) fn recovery_page(failure: Failure, app_nonce: &str, recovery: Recovery) -> String {
    let (title, message) = recovery_text(failure);
    let hint = match recovery {
        Recovery::WithTerminal => {
            "Nothing was saved. Try again, scan a QR code with the Discord app, or paste a token."
        }
        Recovery::WindowOnly => "Nothing was saved. Try again, or cancel and log in later.",
    };
    RECOVERY_PAGE
        .replace("{nonce}", app_nonce)
        .replace("{title}", title)
        .replace("{message}", message)
        .replace("{hint}", hint)
        .replace("{actions}", &recovery_buttons(recovery))
}

pub(super) fn recovery_text(failure: Failure) -> (&'static str, &'static str) {
    match failure {
        Failure::NoCandidate => (
            "Login not completed",
            "Discord shows you as signed in, but D-Streamy could not read the session.",
        ),
        Failure::Rejected => (
            "Login not completed",
            "Discord did not accept the session D-Streamy read from the page.",
        ),
        Failure::Unreachable => (
            "Login not completed",
            "D-Streamy could not reach Discord to check the login.",
        ),
        Failure::PageNotLoaded => (
            "Could not load Discord",
            "Discord's login page did not finish loading. Check the network connection.",
        ),
        Failure::LeftDiscord => (
            "That page isn't part of Discord's login",
            "D-Streamy only shows Discord's own login pages. Retry to go back to the login.",
        ),
    }
}
