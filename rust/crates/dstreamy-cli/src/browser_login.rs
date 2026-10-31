//! `dstreamy login --browser`: Discord's own login page in a window; the token is read from its API requests.

use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

use crate::CliError;

const LOGIN_URL: &str = "https://discord.com/login";
// A fixed size makes tiling window managers float the window as a dialog.
const WINDOW_SIZE: tao::dpi::LogicalSize<f64> = tao::dpi::LogicalSize::new(520.0, 760.0);
const BACKGROUND: (u8, u8, u8, u8) = (0x31, 0x33, 0x38, 0xff);

enum Msg {
    Close,
    Token(String),
}

const CAPTURE_SCRIPT: &str = r##"
(() => {
  document.documentElement.style.background = "#313338";
  window.addEventListener("keydown", (e) => {
    if (e.key === "Escape") window.ipc.postMessage("close");
  }, true);
  if (!location.hostname.endsWith("discord.com")) return;
  let sent = false;
  const send = (value) => {
    if (sent || typeof value !== "string" || value.length < 20 || value.includes(" ")) return;
    sent = true;
    window.ipc.postMessage("token:" + value);
  };
  const setHeader = XMLHttpRequest.prototype.setRequestHeader;
  XMLHttpRequest.prototype.setRequestHeader = function (name, value) {
    if (String(name).toLowerCase() === "authorization") send(value);
    return setHeader.apply(this, arguments);
  };
  const fetch = window.fetch;
  window.fetch = function (input, init) {
    try {
      const headers = new Headers((init && init.headers) || (input instanceof Request ? input.headers : undefined));
      send(headers.get("authorization"));
    } catch (_) {}
    return fetch.apply(this, arguments);
  };
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
            let body = request.body();
            let msg = match body.strip_prefix("token:") {
                Some(token) => Msg::Token(token.to_owned()),
                None if body == "close" => Msg::Close,
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
            Event::UserEvent(Msg::Token(value)) => {
                token = Some(value);
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
