//! `dstreamy login --browser`: Discord's own login page in a window; the token is read from its API requests.

use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

use crate::CliError;
use crate::login_capture::{self, Candidate};

const LOGIN_URL: &str = "https://discord.com/login";
// A fixed size makes tiling window managers float the window as a dialog.
const WINDOW_SIZE: tao::dpi::LogicalSize<f64> = tao::dpi::LogicalSize::new(520.0, 760.0);
const BACKGROUND: (u8, u8, u8, u8) = (0x31, 0x33, 0x38, 0xff);

enum Msg {
    Close,
    Candidate(Candidate),
    SignedIn,
}

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

/// Blocks the calling thread, which must be the main thread, until the user logs in or closes the window.
pub fn run() -> Result<String, CliError> {
    #[cfg(target_os = "linux")]
    if std::path::Path::new("/sys/module/nvidia").exists()
        && std::env::var_os("__NV_DISABLE_EXPLICIT_SYNC").is_none()
    {
        // WebKitGTK on Wayland dies with protocol error 71 under NVIDIA explicit sync.
        // SAFETY: runs on the main thread before the async runtime or any webview thread exists.
        unsafe { std::env::set_var("__NV_DISABLE_EXPLICIT_SYNC", "1") };
    }
    let mut event_loop = EventLoopBuilder::<Msg>::with_user_event().build();
    let proxy = event_loop.create_proxy();
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
            let msg = match login_capture::parse_event(request.body()) {
                Some(login_capture::Event::Candidate(c)) => Msg::Candidate(c),
                Some(login_capture::Event::SignedIn) => Msg::SignedIn,
                Some(login_capture::Event::Close) => Msg::Close,
                None => return,
            };
            let _ = proxy.send_event(msg);
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

    let mut token = None;
    event_loop.run_return(|event, _, flow| {
        *flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(Msg::Candidate(candidate)) => {
                token = Some(candidate.into_string());
                *flow = ControlFlow::Exit;
            }
            Event::UserEvent(Msg::Close)
            | Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => *flow = ControlFlow::Exit,
            _ => {}
        }
    });
    drop(webview);
    drop(window);
    token.ok_or_else(|| CliError::BrowserLogin("the login window was closed".into()))
}

/// macOS routes ⌘C/⌘V/⌘X/⌘A through the menu bar; without an Edit menu they do nothing.
#[cfg(target_os = "macos")]
fn edit_menu() -> Result<muda::Menu, CliError> {
    use muda::{Menu, PredefinedMenuItem, Submenu};
    let err = |e: muda::Error| CliError::BrowserLogin(e.to_string());
    let app = Submenu::with_items("D-Streamy", true, &[&PredefinedMenuItem::quit(None)])
        .map_err(err)?;
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
