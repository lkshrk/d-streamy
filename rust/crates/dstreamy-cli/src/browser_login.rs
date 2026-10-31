//! `dstreamy login --browser`: Discord's own login page in a window; the token is read from its API requests.

use std::time::Instant;

use dstreamy_discord::GatewayError;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::window::{Window, WindowBuilder};
use tokio::runtime::Handle;
use wry::{WebView, WebViewBuilder};

use crate::discord::{self, Persistence};
use crate::login_capture::{
    self, BrowserLoginOutcome, BrowserState, Command, Failure, Job, SeenCandidates, Validation,
    ValidationError, View,
};
use crate::{CliError, Ctx};

const LOGIN_URL: &str = "https://discord.com/login";
// A fixed size makes tiling window managers float the window as a dialog.
const WINDOW_SIZE: tao::dpi::LogicalSize<f64> = tao::dpi::LogicalSize::new(520.0, 760.0);
const BACKGROUND: (u8, u8, u8, u8) = (0x31, 0x33, 0x38, 0xff);

enum Msg {
    Ipc(login_capture::Event),
    Validated {
        generation: u64,
        seen: SeenCandidates,
        result: Validation<String>,
    },
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
  const post = (m) => { try { window.ipc.postMessage(m); } catch (_) {} };
  for (const b of document.querySelectorAll("button[data-action]")) {
    b.addEventListener("click", () => post("action:" + b.dataset.action));
  }
  window.addEventListener("keydown", (e) => { if (e.key === "Escape") post("close"); });
  const first = document.querySelector("button");
  if (first) first.focus();
})();
</script>
</body>
</html>
"##;

const RECOVERY_BUTTONS: &str = r#"<button class="primary" data-action="retry">Retry</button>
    <button data-action="qr">Use QR instead</button>
    <button data-action="token">Paste token</button>
    <button data-action="cancel">Cancel</button>"#;

const CAPTURE_SCRIPT: &str = r##"
(() => {
  document.documentElement.style.background = "#313338";
  window.addEventListener("keydown", (e) => {
    if (e.key === "Escape") window.ipc.postMessage("close");
  }, true);
  if (!location.hostname.endsWith("discord.com")) return;
  const post = (message) => {
    try { window.ipc.postMessage(message); } catch (_) {}
  };
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
})();
"##;

/// Must run on the main thread before any other thread, including the async runtime, starts.
pub fn prepare_environment() {
    #[cfg(target_os = "linux")]
    if std::path::Path::new("/sys/module/nvidia").exists()
        && std::env::var_os("__NV_DISABLE_EXPLICIT_SYNC").is_none()
    {
        // WebKitGTK on Wayland dies with protocol error 71 under NVIDIA explicit sync.
        // SAFETY: the caller guarantees no other thread exists yet.
        unsafe { std::env::set_var("__NV_DISABLE_EXPLICIT_SYNC", "1") };
    }
}

/// Blocks the calling thread, which must be the main thread, until the user logs in or leaves.
///
/// Candidates are validated on `runtime`; only an accepted one is returned as a token.
pub fn run(ctx: &Ctx, runtime: &Handle) -> Result<BrowserLoginOutcome, CliError> {
    let mut event_loop = EventLoopBuilder::<Msg>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let validation_proxy = event_loop.create_proxy();
    #[cfg(target_os = "macos")]
    let _menu = edit_menu()?;
    let window = WindowBuilder::new()
        .with_title("D-Streamy: log in to Discord")
        .with_inner_size(WINDOW_SIZE)
        .with_min_inner_size(WINDOW_SIZE)
        .with_max_inner_size(WINDOW_SIZE)
        .with_resizable(false)
        .build(&event_loop)
        .map_err(|e| CliError::BrowserLogin(e.to_string()))?;
    let builder = WebViewBuilder::new()
        .with_incognito(true)
        .with_clipboard(true)
        .with_background_color(BACKGROUND)
        .with_url(LOGIN_URL)
        .with_initialization_script(CAPTURE_SCRIPT)
        .with_ipc_handler(move |request| {
            if let Some(event) = login_capture::parse_event(request.body()) {
                let _ = proxy.send_event(Msg::Ipc(event));
            }
        });
    #[cfg(target_os = "linux")]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        let vbox = window
            .default_vbox()
            .ok_or_else(|| CliError::BrowserLogin("no GTK container".into()))?;
        builder.build_gtk(vbox)
    };
    #[cfg(not(target_os = "linux"))]
    let webview = builder.build(&window);
    let webview = webview.map_err(|e| CliError::BrowserLogin(e.to_string()))?;

    let mut state = BrowserState::default();
    let mut shown = (View::Discord, false);
    let mut outcome = None;
    event_loop.run_return(|event, _, flow| {
        if outcome.is_some() {
            *flow = ControlFlow::Exit;
            return;
        }
        let now = Instant::now();
        let command = match event {
            Event::UserEvent(Msg::Ipc(event)) => state.event(event, now),
            Event::UserEvent(Msg::Validated {
                generation,
                seen,
                result,
            }) => state.validated(generation, seen, result),
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => state.close(),
            _ => None,
        };
        state.tick(now);
        match command {
            Some(Command::Validate(job)) => {
                spawn_validation(runtime, ctx.clone(), validation_proxy.clone(), job);
            }
            Some(Command::Exit(done)) => {
                outcome = Some(done);
                *flow = ControlFlow::Exit;
                return;
            }
            None => {}
        }
        let current = (state.view(), state.is_validating());
        if current != shown {
            render(&webview, &window, shown.0, current);
            shown = current;
        }
        *flow = match state.deadline() {
            Some(deadline) => ControlFlow::WaitUntil(deadline),
            None => ControlFlow::Wait,
        };
    });
    drop(webview);
    drop(window);
    outcome.ok_or_else(|| CliError::BrowserLogin("the login window closed unexpectedly".into()))
}

fn spawn_validation(runtime: &Handle, ctx: Ctx, proxy: EventLoopProxy<Msg>, job: Job) {
    let Job {
        generation,
        mut seen,
        candidate,
    } = job;
    let check = runtime.spawn(async move {
        let result = login_capture::validate_candidate(&mut seen, candidate, move |token| {
            check_token(ctx, token)
        })
        .await;
        (seen, result)
    });
    runtime.spawn(async move {
        let (seen, result) = check
            .await
            .unwrap_or_else(|_| (SeenCandidates::default(), Validation::Transient));
        let _ = proxy.send_event(Msg::Validated {
            generation,
            seen,
            result,
        });
    });
}

async fn check_token(ctx: Ctx, token: String) -> Result<String, ValidationError<()>> {
    match discord::connect_with(&ctx, token.clone(), Persistence::Brief).await {
        Ok(client) => {
            client.close().await;
            Ok(token)
        }
        Err(CliError::Gateway(GatewayError::Rejected(_))) => Err(ValidationError::Rejected),
        Err(_) => Err(ValidationError::Transient(())),
    }
}

fn render(webview: &WebView, window: &Window, previous: View, (view, validating): (View, bool)) {
    let title = match view {
        View::Discord if validating => "D-Streamy: verifying the login…",
        View::Discord => "D-Streamy: log in to Discord",
        View::Recovery(_) => "D-Streamy: login not completed",
        View::Closing => "D-Streamy: closing…",
    };
    window.set_title(title);
    if view == previous {
        return;
    }
    let loaded = match view {
        View::Discord => webview.load_url(LOGIN_URL),
        View::Recovery(failure) => webview.load_html(&recovery_page(failure)),
        View::Closing => webview.load_html(&page(
            "Closing…",
            "Finishing the current check of the Discord session.",
            "Nothing will be saved.",
            "",
        )),
    };
    if let Err(e) = loaded {
        tracing::warn!("showing the browser login page failed: {e}");
    }
}

fn recovery_page(failure: Failure) -> String {
    let message = match failure {
        Failure::NoCandidate => {
            "Discord shows you as signed in, but D-Streamy could not read the session."
        }
        Failure::Rejected => "Discord did not accept the session D-Streamy read from the page.",
        Failure::Unreachable => "D-Streamy could not reach Discord to check the login.",
    };
    page(
        "Login not completed",
        message,
        "Nothing was saved. Try again, scan a QR code with the Discord app, or paste a token.",
        RECOVERY_BUTTONS,
    )
}

fn page(title: &str, message: &str, hint: &str, actions: &str) -> String {
    RECOVERY_PAGE
        .replace("{title}", title)
        .replace("{message}", message)
        .replace("{hint}", hint)
        .replace("{actions}", actions)
}

/// macOS routes ⌘C/⌘V/⌘X/⌘A through the menu bar; without an Edit menu they do nothing.
#[cfg(target_os = "macos")]
fn edit_menu() -> Result<muda::Menu, CliError> {
    use muda::{Menu, PredefinedMenuItem, Submenu};
    let err = |e: muda::Error| CliError::BrowserLogin(e.to_string());
    let app =
        Submenu::with_items("D-Streamy", true, &[&PredefinedMenuItem::quit(None)]).map_err(err)?;
    let edit = Submenu::with_items(
        "Edit",
        true,
        &[
            &PredefinedMenuItem::undo(None),
            &PredefinedMenuItem::redo(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::cut(None),
            &PredefinedMenuItem::copy(None),
            &PredefinedMenuItem::paste(None),
            &PredefinedMenuItem::select_all(None),
        ],
    )
    .map_err(err)?;
    let menu = Menu::with_items(&[&app, &edit]).map_err(err)?;
    menu.init_for_nsapp();
    Ok(menu)
}
