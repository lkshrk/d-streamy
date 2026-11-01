//! `dstreamy login --browser`: Discord's own login page in a window; the token is read from its API requests.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use dstreamy_discord::GatewayError;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::window::{Window, WindowBuilder};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use wry::{NewWindowResponse, PageLoadEvent, WebView, WebViewBuilder};

use crate::discord::{self, Persistence};
use crate::login_capture::{
    self, Action, BrowserLoginOutcome, BrowserState, Command, Failure, Job, RECOVERY_ACTIONS,
    SeenCandidates, Validation, ValidationError, View,
};
use crate::{CliError, Ctx};

const LOGIN_URL: &str = "https://discord.com/login";
// A fixed size makes tiling window managers float the window as a dialog.
const WINDOW_SIZE: tao::dpi::LogicalSize<f64> = tao::dpi::LogicalSize::new(520.0, 760.0);
#[cfg(feature = "test-hooks")]
fn smoke() -> bool {
    std::env::var_os("DSTREAMY_LOGIN_SMOKE").is_some()
}

fn login_url() -> String {
    #[cfg(feature = "test-hooks")]
    if let Ok(url) = std::env::var("DSTREAMY_LOGIN_URL") {
        return url;
    }
    LOGIN_URL.to_owned()
}

const BACKGROUND: (u8, u8, u8, u8) = (0x31, 0x33, 0x38, 0xff);

fn abort_unless_validating(validation: &mut Option<JoinHandle<()>>, validating: bool) {
    if !validating && let Some(task) = validation.take() {
        task.abort();
    }
}

fn start_validation(validation: &mut Option<JoinHandle<()>>, task: JoinHandle<()>) {
    if let Some(stale) = validation.replace(task) {
        stale.abort();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Screen {
    Discord { verifying: bool },
    Recovery(Failure),
}

impl Screen {
    fn of(state: &BrowserState) -> Self {
        match state.view() {
            View::Discord => Self::Discord {
                verifying: state.verifying(),
            },
            View::Recovery(failure) => Self::Recovery(failure),
        }
    }

    fn same_page(self, other: Self) -> bool {
        matches!((self, other), (Self::Discord { .. }, Self::Discord { .. })) || self == other
    }

    #[cfg(any(target_os = "linux", test))]
    fn verifying(self) -> bool {
        matches!(self, Self::Discord { verifying: true })
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Origin {
    Discord,
    App,
    Other,
}

/// Other subdomains such as support.discord.com are hosted by third parties.
const DISCORD_HOSTS: [&str; 3] = ["discord.com", "ptb.discord.com", "canary.discord.com"];

fn origin(uri: &str) -> Origin {
    if uri == "about:blank" {
        return Origin::App;
    }
    match url::Url::parse(uri) {
        Ok(url)
            if url.scheme() == "https"
                && url.port().is_none()
                && url
                    .host_str()
                    .is_some_and(|host| DISCORD_HOSTS.contains(&host)) =>
        {
            Origin::Discord
        }
        _ => Origin::Other,
    }
}

struct Nonces {
    discord: String,
    app: String,
}

impl Nonces {
    fn new() -> Result<Self, CliError> {
        Ok(Self {
            discord: new_nonce()?,
            app: new_nonce()?,
        })
    }
}

fn signed<'a>(nonce: &str, body: &'a str) -> Option<&'a str> {
    body.strip_prefix(nonce)?.strip_prefix('|')
}

/// A frame URI is forgeable (Linux reports the top page's, about:blank is anyone's), so every page signs.
fn accepted_ipc<'a>(nonces: &Nonces, shown: Screen, uri: &str, body: &'a str) -> Option<&'a str> {
    match (origin(uri), shown) {
        (Origin::Discord, Screen::Discord { .. }) => {
            signed(&nonces.discord, body).filter(|message| !message.starts_with("action:"))
        }
        (Origin::App, Screen::Recovery(_)) => signed(&nonces.app, body).filter(|message| {
            matches!(*message, "close" | "ready") || message.starts_with("action:")
        }),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn status_cancels(nonce: &str, shown: Screen, uri: &str, body: &str) -> bool {
    shown.verifying() && origin(uri) == Origin::App && signed(nonce, body) == Some("action:cancel")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trouble {
    SentAway,
    NotLoading,
}

impl Trouble {
    fn error(self) -> CliError {
        let cause = match self {
            Self::SentAway => "the login window kept being sent away from Discord's login",
            Self::NotLoading => "D-Streamy's page in the login window never finished loading",
        };
        CliError::BrowserLogin(format!(
            "{cause}, so it was closed. Run the login again, or use the QR code or a token instead"
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PageDecision {
    Expected,
    LeftDiscord,
    Reload(Trouble),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PageAction {
    Expected,
    Recover,
    Reload,
    GiveUp(Trouble),
}

const APP_COMMIT_TIMEOUT: Duration = Duration::from_secs(10);
const APP_READY_TIMEOUT: Duration = Duration::from_secs(20);
const RELOAD_WINDOW: Duration = Duration::from_secs(30);
const MAX_RELOADS: usize = 5;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum AppPage {
    #[default]
    None,
    Loading {
        deadline: Instant,
        committed: bool,
    },
    Ready {
        committed: bool,
    },
    /// Retry replaced a recovery page that had not committed yet; its late commit is harmless.
    Superseded,
}

#[derive(Default)]
struct PageLoads {
    app: AppPage,
    reloads: VecDeque<Instant>,
}

impl PageLoads {
    fn showing(&mut self, screen: Screen, now: Instant) {
        self.app = match (screen, self.app) {
            (
                Screen::Discord { .. },
                AppPage::Loading {
                    committed: false, ..
                }
                | AppPage::Ready { committed: false },
            ) => AppPage::Superseded,
            (Screen::Discord { .. }, _) => AppPage::None,
            (Screen::Recovery(_), _) => AppPage::Loading {
                deadline: now + APP_COMMIT_TIMEOUT,
                committed: false,
            },
        };
    }

    fn ready(&mut self) {
        if let AppPage::Loading { committed, .. } = self.app {
            self.app = AppPage::Ready { committed };
        }
    }

    fn deadline(&self) -> Option<Instant> {
        match self.app {
            AppPage::Loading { deadline, .. } => Some(deadline),
            _ => None,
        }
    }

    fn page_load(
        &mut self,
        shown: Screen,
        url: &str,
        start_url: &str,
        now: Instant,
    ) -> PageDecision {
        match (shown, self.app) {
            (
                Screen::Recovery(_),
                AppPage::Loading {
                    committed: false, ..
                },
            ) if url == "about:blank" => {
                self.app = AppPage::Loading {
                    deadline: now + APP_READY_TIMEOUT,
                    committed: true,
                };
                PageDecision::Expected
            }
            (Screen::Recovery(_), AppPage::Ready { committed: false }) if url == "about:blank" => {
                self.app = AppPage::Ready { committed: true };
                PageDecision::Expected
            }
            (Screen::Recovery(_), _) => PageDecision::Reload(Trouble::SentAway),
            (Screen::Discord { .. }, app) => {
                self.app = AppPage::None;
                if (url == "about:blank" && app == AppPage::Superseded)
                    || origin(url) == Origin::Discord
                    || url == start_url
                {
                    PageDecision::Expected
                } else {
                    PageDecision::LeftDiscord
                }
            }
        }
    }

    fn expire(&mut self, now: Instant) -> Option<Trouble> {
        match self.app {
            AppPage::Loading { deadline, .. } if now >= deadline => {
                self.app = AppPage::None;
                Some(Trouble::NotLoading)
            }
            _ => None,
        }
    }

    fn reload(&mut self, now: Instant, trouble: Trouble) -> PageAction {
        while self
            .reloads
            .front()
            .is_some_and(|&at| now.duration_since(at) >= RELOAD_WINDOW)
        {
            self.reloads.pop_front();
        }
        if self.reloads.len() >= MAX_RELOADS {
            return PageAction::GiveUp(trouble);
        }
        self.reloads.push_back(now);
        PageAction::Reload
    }
}

const LINK_INTERVAL: Duration = Duration::from_secs(1);
const MAX_LINKS: u32 = 20;

#[derive(Default)]
struct LinkLimiter {
    opened: u32,
    last: Option<Instant>,
}

impl LinkLimiter {
    fn allow(&mut self, now: Instant) -> bool {
        if self.opened >= MAX_LINKS || self.last.is_some_and(|last| now < last + LINK_INTERVAL) {
            return false;
        }
        self.opened += 1;
        self.last = Some(now);
        true
    }
}

fn external_link(url: &str) -> Option<String> {
    let url = url::Url::parse(url).ok()?;
    (url.scheme() == "https" && url.host_str().is_some()).then(|| url.to_string())
}

fn open_in_browser(url: &str) {
    #[cfg(target_os = "linux")]
    let opener = "xdg-open";
    #[cfg(target_os = "macos")]
    let opener = "open";
    let spawned = std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match spawned {
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(e) => tracing::warn!("opening the link in the default browser failed: {e}"),
    }
}

fn new_nonce() -> Result<String, CliError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| CliError::BrowserLogin(e.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn capture_script(nonce: &str) -> String {
    let hosts = DISCORD_HOSTS.map(|host| format!("\"{host}\"")).join(", ");
    CAPTURE_SCRIPT
        .replace("{hosts}", &hosts)
        .replace("{nonce}", nonce)
}

enum Msg {
    Ipc {
        uri: String,
        body: String,
    },
    PageLoad(String),
    #[cfg(target_os = "linux")]
    Status {
        uri: String,
        body: String,
    },
    #[cfg(target_os = "linux")]
    LoadFailed,
    Validated {
        generation: u64,
        seen: SeenCandidates,
        result: Validation<String>,
    },
}

enum LoopEvent {
    Msg(Msg),
    CloseRequested,
    Wake,
}

struct Step {
    command: Option<Command>,
    load: Option<Screen>,
    retitle: bool,
}

/// Everything the login window decides, apart from the window and the webviews themselves.
struct Session {
    state: BrowserState,
    shown: Screen,
    loads: PageLoads,
    nonces: Nonces,
    start_url: String,
    trouble: Option<Trouble>,
}

impl Session {
    fn new(nonces: Nonces, start_url: String, now: Instant) -> Self {
        Self {
            state: BrowserState::new(now),
            shown: Screen::Discord { verifying: false },
            loads: PageLoads::default(),
            nonces,
            start_url,
            trouble: None,
        }
    }

    fn deadline(&self) -> Option<Instant> {
        [self.state.deadline(), self.loads.deadline()]
            .into_iter()
            .flatten()
            .min()
    }

    fn step(&mut self, event: LoopEvent, now: Instant) -> Step {
        let mut page = PageAction::Expected;
        let command = match event {
            LoopEvent::Msg(Msg::Ipc { uri, body }) => {
                match accepted_ipc(&self.nonces, self.shown, &uri, &body) {
                    Some("ready") => {
                        self.loads.ready();
                        None
                    }
                    Some(message) => self.state.ipc(message, now),
                    None => None,
                }
            }
            LoopEvent::Msg(Msg::PageLoad(url)) => {
                page = self.page_load(&url, now);
                None
            }
            #[cfg(target_os = "linux")]
            LoopEvent::Msg(Msg::Status { uri, body }) => {
                status_cancels(&self.nonces.app, self.shown, &uri, &body)
                    .then(|| self.state.close())
                    .flatten()
            }
            #[cfg(target_os = "linux")]
            LoopEvent::Msg(Msg::LoadFailed) => {
                self.state.event(login_capture::Event::LoadFailed, now)
            }
            LoopEvent::Msg(Msg::Validated {
                generation,
                seen,
                result,
            }) => self.state.validated(generation, seen, result, now),
            LoopEvent::CloseRequested => self.state.close(),
            LoopEvent::Wake => None,
        };
        if page == PageAction::Expected
            && let Some(trouble) = self.loads.expire(now)
        {
            page = self.loads.reload(now, trouble);
        }
        let command = self.give_up(command, page);
        self.state.tick(now);
        let current = Screen::of(&self.state);
        let load =
            (!current.same_page(self.shown) || page == PageAction::Reload).then_some(current);
        if let Some(screen) = load {
            self.loads.showing(screen, now);
        }
        let retitle = current != self.shown;
        self.shown = current;
        Step {
            command,
            load,
            retitle,
        }
    }

    fn page_load(&mut self, url: &str, now: Instant) -> PageAction {
        match self.loads.page_load(self.shown, url, &self.start_url, now) {
            PageDecision::Expected => PageAction::Expected,
            PageDecision::Reload(trouble) => self.loads.reload(now, trouble),
            PageDecision::LeftDiscord => {
                let _ = self.state.event(login_capture::Event::LeftDiscord, now);
                if Screen::of(&self.state).same_page(self.shown) {
                    self.loads.reload(now, Trouble::SentAway)
                } else {
                    PageAction::Recover
                }
            }
        }
    }

    /// An exit decided by the same event must win, or the window would never get its outcome.
    fn give_up(&mut self, command: Option<Command>, page: PageAction) -> Option<Command> {
        match (page, command) {
            (PageAction::GiveUp(trouble), None | Some(Command::Validate(_))) => {
                self.trouble = Some(trouble);
                self.state.close()
            }
            (_, command) => command,
        }
    }
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
  <p class="hint">Nothing was saved. Try again, scan a QR code with the Discord app, or paste a token.</p>
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

fn recovery_buttons() -> String {
    RECOVERY_ACTIONS
        .map(|action| {
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
        .join("\n    ")
}

#[cfg(target_os = "linux")]
const STATUS_HEIGHT: i32 = 56;

#[cfg(target_os = "linux")]
const STATUS_PAGE: &str = r##"<!doctype html>
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

#[cfg(target_os = "linux")]
fn genuine_load_failure(error: &webkit2gtk::glib::Error) -> bool {
    use webkit2gtk::{NetworkError, PolicyError};
    !(error.matches(NetworkError::Cancelled)
        || error.matches(PolicyError::FrameLoadInterruptedByPolicyChange))
}

/// Must run on the main thread before any other thread, including the async runtime, starts.
pub fn prepare_environment() {
    #[cfg(target_os = "linux")]
    if std::path::Path::new("/sys/module/nvidia").exists()
        && std::env::var_os("__NV_DISABLE_EXPLICIT_SYNC").is_none()
    {
        // SAFETY: no other thread exists yet; WebKitGTK on Wayland dies under NVIDIA explicit sync.
        unsafe { std::env::set_var("__NV_DISABLE_EXPLICIT_SYNC", "1") };
    }
}

struct Views {
    main: WebView,
    #[cfg(target_os = "linux")]
    status: WebView,
}

/// Blocks the main thread until the user logs in or leaves; only a validated token is returned.
pub fn run(ctx: &Ctx, runtime: &Handle) -> Result<BrowserLoginOutcome, CliError> {
    let mut event_loop = EventLoopBuilder::<Msg>::with_user_event().build();
    let nonces = Nonces::new()?;
    let start_url = login_url();
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
    let views = build_views(&event_loop, &window, &nonces, &start_url)?;
    let mut session = Session::new(nonces, start_url, Instant::now());
    let outcome = drive(&mut event_loop, &mut session, &views, &window, ctx, runtime);
    drop(views);
    drop(window);
    if let Some(trouble) = session.trouble {
        return Err(trouble.error());
    }
    outcome.ok_or_else(|| CliError::BrowserLogin("the login window closed unexpectedly".into()))
}

fn main_view<'a>(
    event_loop: &EventLoop<Msg>,
    nonces: &Nonces,
    start_url: &str,
) -> WebViewBuilder<'a> {
    let ipc = event_loop.create_proxy();
    let pages = event_loop.create_proxy();
    let links = RefCell::new(LinkLimiter::default());
    WebViewBuilder::new()
        .with_incognito(true)
        .with_clipboard(true)
        .with_background_color(BACKGROUND)
        .with_url(start_url)
        .with_initialization_script_for_main_only(capture_script(&nonces.discord), true)
        .with_ipc_handler(move |request| {
            let _ = ipc.send_event(Msg::Ipc {
                uri: request.uri().to_string(),
                body: request.body().clone(),
            });
        })
        .with_on_page_load_handler(move |event, url| {
            if let PageLoadEvent::Started = event {
                let _ = pages.send_event(Msg::PageLoad(url));
            }
        })
        .with_new_window_req_handler(move |url, _| {
            if let Some(url) = external_link(&url) {
                if links.borrow_mut().allow(Instant::now()) {
                    open_in_browser(&url);
                } else {
                    tracing::warn!(
                        "ignored a link: links open at most once a second, 20 per window"
                    );
                }
            }
            NewWindowResponse::Deny
        })
}

#[cfg(target_os = "linux")]
fn build_views(
    event_loop: &EventLoop<Msg>,
    window: &Window,
    nonces: &Nonces,
    start_url: &str,
) -> Result<Views, CliError> {
    use gtk::prelude::{BoxExt, WidgetExt};
    use tao::platform::unix::WindowExtUnix;
    use webkit2gtk::WebViewExt;
    use wry::{WebViewBuilderExtUnix, WebViewExtUnix};
    let err = |e: wry::Error| CliError::BrowserLogin(e.to_string());
    let vbox = window
        .default_vbox()
        .ok_or_else(|| CliError::BrowserLogin("no GTK container".into()))?;
    let status_proxy = event_loop.create_proxy();
    let status = WebViewBuilder::new()
        .with_incognito(true)
        .with_visible(false)
        .with_focused(false)
        .with_background_color(BACKGROUND)
        .with_html(STATUS_PAGE.replace("{nonce}", &nonces.app))
        .with_ipc_handler(move |request| {
            let _ = status_proxy.send_event(Msg::Status {
                uri: request.uri().to_string(),
                body: request.body().clone(),
            });
        })
        .build_gtk(vbox)
        .map_err(err)?;
    let banner = status.webview();
    vbox.set_child_packing(&banner, false, false, 0, gtk::PackType::Start);
    banner.set_size_request(-1, STATUS_HEIGHT);
    let main = main_view(event_loop, nonces, start_url)
        .build_gtk(vbox)
        .map_err(err)?;
    let load_proxy = event_loop.create_proxy();
    main.webview().connect_load_failed(move |_, _, _, error| {
        if !genuine_load_failure(error) {
            return false;
        }
        let _ = load_proxy.send_event(Msg::LoadFailed);
        true
    });
    Ok(Views { main, status })
}

#[cfg(not(target_os = "linux"))]
fn build_views(
    event_loop: &EventLoop<Msg>,
    window: &Window,
    nonces: &Nonces,
    start_url: &str,
) -> Result<Views, CliError> {
    let main = main_view(event_loop, nonces, start_url)
        .build(window)
        .map_err(|e| CliError::BrowserLogin(e.to_string()))?;
    Ok(Views { main })
}

fn drive(
    event_loop: &mut EventLoop<Msg>,
    session: &mut Session,
    views: &Views,
    window: &Window,
    ctx: &Ctx,
    runtime: &Handle,
) -> Option<BrowserLoginOutcome> {
    #[cfg(feature = "test-hooks")]
    let smoke = smoke();
    #[cfg(feature = "test-hooks")]
    if smoke {
        let size = window.inner_size().to_logical::<f64>(window.scale_factor());
        eprintln!(
            "smoke: size={}x{} background={:?}",
            size.width, size.height, BACKGROUND
        );
    }
    let validation_proxy = event_loop.create_proxy();
    let mut validation = None;
    let mut outcome = None;
    event_loop.run_return(|event, _, flow| {
        if outcome.is_some() {
            *flow = ControlFlow::Exit;
            return;
        }
        let event = match event {
            Event::UserEvent(msg) => LoopEvent::Msg(msg),
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => LoopEvent::CloseRequested,
            _ => LoopEvent::Wake,
        };
        let now = Instant::now();
        let step = session.step(event, now);
        abort_unless_validating(&mut validation, session.state.is_validating());
        match step.command {
            Some(Command::Validate(job)) => start_validation(
                &mut validation,
                spawn_validation(runtime, ctx.clone(), validation_proxy.clone(), job),
            ),
            Some(Command::Exit(done)) => {
                outcome = Some(done);
                *flow = ControlFlow::Exit;
                return;
            }
            None => {}
        }
        if let Some(screen) = step.load {
            show(&views.main, screen, &session.start_url, &session.nonces.app);
        }
        if step.retitle {
            set_title(window, session.shown);
            #[cfg(target_os = "linux")]
            if let Err(e) = views.status.set_visible(session.shown.verifying()) {
                tracing::warn!("showing the verification status failed: {e}");
            }
            #[cfg(feature = "test-hooks")]
            if smoke {
                eprintln!("smoke: view={:?}", session.state.view());
                if matches!(session.shown, Screen::Recovery(_)) {
                    outcome = Some(BrowserLoginOutcome::Cancelled);
                    *flow = ControlFlow::Exit;
                    return;
                }
            }
        }
        *flow = match session.deadline() {
            Some(deadline) => ControlFlow::WaitUntil(deadline),
            None => ControlFlow::Wait,
        };
    });
    abort_unless_validating(&mut validation, false);
    outcome
}

fn spawn_validation(
    runtime: &Handle,
    ctx: Ctx,
    proxy: EventLoopProxy<Msg>,
    job: Job,
) -> JoinHandle<()> {
    let Job {
        generation,
        seen,
        candidate,
    } = job;
    runtime.spawn(async move {
        let (seen, result) =
            login_capture::validate_job(seen, candidate, move |token| check_token(ctx, token))
                .await;
        let _ = proxy.send_event(Msg::Validated {
            generation,
            seen,
            result,
        });
    })
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

fn set_title(window: &Window, screen: Screen) {
    let title = match screen {
        Screen::Discord { verifying: true } => "D-Streamy: verifying the login…",
        Screen::Discord { verifying: false } => "D-Streamy: log in to Discord",
        Screen::Recovery(_) => "D-Streamy: login not completed",
    };
    window.set_title(title);
}

fn show(webview: &WebView, screen: Screen, start_url: &str, app_nonce: &str) {
    let loaded = match screen {
        Screen::Discord { .. } => webview.load_url(start_url),
        Screen::Recovery(failure) => webview.load_html(&recovery_page(failure, app_nonce)),
    };
    if let Err(e) = loaded {
        tracing::warn!("showing the browser login page failed: {e}");
    }
}

fn recovery_page(failure: Failure, app_nonce: &str) -> String {
    let (title, message) = match failure {
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
    };
    RECOVERY_PAGE
        .replace("{nonce}", app_nonce)
        .replace("{title}", title)
        .replace("{message}", message)
        .replace("{actions}", &recovery_buttons())
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

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::time::Duration;

    use super::*;
    use crate::discord::BRIEF_VALIDATION_TIMEOUT;
    use crate::login_capture::{Candidate, parse_candidate};

    const WAIT: Duration = Duration::from_secs(5);
    const QUIET: Duration = Duration::from_millis(200);

    struct Cancelled(Sender<()>);

    impl Drop for Cancelled {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    fn candidate() -> Candidate {
        parse_candidate("candidate:aaa.bbb.ccc").unwrap()
    }

    /// A check that never finishes and reports through `Cancelled` when its future is dropped.
    fn running(runtime: &tokio::runtime::Runtime) -> (JoinHandle<()>, Receiver<()>) {
        let (started_tx, started) = channel();
        let (cancelled_tx, cancelled) = channel();
        let task = runtime.spawn(async move {
            let _ = login_capture::validate_job(SeenCandidates::default(), candidate(), |_| {
                let guard = Cancelled(cancelled_tx);
                started_tx.send(()).unwrap();
                async move {
                    let _guard = guard;
                    std::future::pending::<Result<String, ValidationError<()>>>().await
                }
            })
            .await;
        });
        started.recv_timeout(WAIT).unwrap();
        (task, cancelled)
    }

    fn aborted(cancelled: &Receiver<()>) -> bool {
        cancelled.recv_timeout(WAIT).is_ok()
    }

    fn still_running(cancelled: &Receiver<()>) -> bool {
        cancelled.recv_timeout(QUIET).is_err()
    }

    fn checking(now: Instant) -> (BrowserState, Job) {
        let mut state = BrowserState::new(now);
        state.page_loaded();
        state.ipc("signed_in", now);
        match state.ipc("candidate:aaa.bbb.ccc", now) {
            Some(Command::Validate(job)) => (state, job),
            _ => panic!("expected a check"),
        }
    }

    type Path = fn(&mut BrowserState, Job, Instant);

    #[test]
    fn every_way_a_check_ends_aborts_its_task() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let paths: [(&str, Path); 4] = [
            ("close", |state, _, _| {
                let _ = state.close();
            }),
            ("escape", |state, _, now| {
                let _ = state.ipc("close", now);
            }),
            ("timeout", |state, _, now| {
                state.tick(now + BRIEF_VALIDATION_TIMEOUT);
            }),
            ("result", |state, job, now| {
                let _ = state.validated(job.generation, job.seen, Validation::Rejected, now);
            }),
        ];
        for (name, path) in paths {
            let now = Instant::now();
            let (mut state, job) = checking(now);
            let (task, cancelled) = running(&runtime);
            let mut validation = Some(task);
            abort_unless_validating(&mut validation, state.is_validating());
            assert!(validation.is_some(), "{name}");
            path(&mut state, job, now);
            abort_unless_validating(&mut validation, state.is_validating());
            assert!(validation.is_none(), "{name}");
            assert!(aborted(&cancelled), "{name}");
        }
    }

    #[test]
    fn unrelated_and_stale_events_leave_the_running_check_alone() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let now = Instant::now();
        let (mut state, stale) = checking(now);
        let later = now + BRIEF_VALIDATION_TIMEOUT;
        state.tick(later);
        let _ = state.ipc("action:retry", later);
        state.page_loaded();
        let _current = match state.ipc("candidate:ddd.eee.fff", later) {
            Some(Command::Validate(job)) => job,
            _ => panic!("expected a check"),
        };
        let (task, cancelled) = running(&runtime);
        let mut validation = Some(task);
        let _ = state.ipc("page_loaded", now);
        let _ = state.validated(
            stale.generation,
            stale.seen,
            Validation::Accepted("aaa.bbb.ccc".to_owned()),
            now,
        );
        abort_unless_validating(&mut validation, state.is_validating());
        assert!(validation.is_some());
        assert!(still_running(&cancelled));
    }

    #[test]
    fn starting_a_check_aborts_a_leftover_one() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (old, old_cancelled) = running(&runtime);
        let (new, new_cancelled) = running(&runtime);
        let mut validation = Some(old);
        start_validation(&mut validation, new);
        assert!(aborted(&old_cancelled));
        assert!(validation.is_some());
        assert!(still_running(&new_cancelled));
    }

    #[test]
    fn sign_in_during_a_check_keeps_discord_loaded_and_shows_the_status() {
        let now = Instant::now();
        let mut state = BrowserState::new(now);
        state.page_loaded();
        assert!(state.ipc("candidate:aaa.bbb.ccc", now).is_some());
        let checking = Screen::of(&state);
        assert!(checking.verifying());
        assert!(state.ipc("signed_in", now).is_none());
        assert!(Screen::of(&state) == checking);
        let idle = Screen::Discord { verifying: false };
        assert!(checking.same_page(idle) && !idle.verifying());
    }

    const DISCORD_NONCE: &str = "0123456789abcdef0123456789abcdef";
    const APP_NONCE: &str = "fedcba9876543210fedcba9876543210";
    const ON_DISCORD: Screen = Screen::Discord { verifying: false };
    const ON_RECOVERY: Screen = Screen::Recovery(Failure::Rejected);
    const START: &str = "https://discord.com/login";

    fn nonces() -> Nonces {
        Nonces {
            discord: DISCORD_NONCE.to_owned(),
            app: APP_NONCE.to_owned(),
        }
    }

    fn by(nonce: &str, message: &str) -> String {
        format!("{nonce}|{message}")
    }

    fn accepts(screen: Screen, uri: &str, body: &str) -> Option<String> {
        accepted_ipc(&nonces(), screen, uri, body).map(str::to_owned)
    }

    const SESSION: [&str; 3] = ["candidate:a.b.c", "signed_in", "page_loaded"];
    const ACTIONS: [&str; 4] = ["action:retry", "action:qr", "action:token", "action:cancel"];

    #[test]
    fn discord_pages_report_sessions_signed_with_the_discord_nonce() {
        for uri in [
            "https://discord.com/login",
            "https://ptb.discord.com/channels/@me",
            "https://CANARY.discord.com/app",
        ] {
            for message in SESSION.into_iter().chain(["close"]) {
                let body = by(DISCORD_NONCE, message);
                assert_eq!(accepts(ON_DISCORD, uri, &body).as_deref(), Some(message));
                assert_eq!(accepts(ON_RECOVERY, uri, &body), None, "{uri} {message}");
            }
            for message in ACTIONS {
                assert_eq!(accepts(ON_DISCORD, uri, &by(DISCORD_NONCE, message)), None);
            }
        }
    }

    #[test]
    fn discord_messages_without_the_discord_nonce_are_dropped() {
        let uri = "https://discord.com/login";
        for body in [
            "candidate:a.b.c".to_owned(),
            format!("{DISCORD_NONCE}candidate:a.b.c"),
            by(APP_NONCE, "candidate:a.b.c"),
            format!("|{DISCORD_NONCE}|candidate:a.b.c"),
            by(&DISCORD_NONCE[1..], "candidate:a.b.c"),
            String::new(),
        ] {
            assert_eq!(accepts(ON_DISCORD, uri, &body), None, "{body}");
        }
    }

    #[test]
    fn the_recovery_page_chooses_actions_signed_with_the_app_nonce() {
        for message in ACTIONS.into_iter().chain(["close"]) {
            let body = by(APP_NONCE, message);
            assert_eq!(
                accepts(ON_RECOVERY, "about:blank", &body).as_deref(),
                Some(message)
            );
        }
        for message in SESSION {
            assert_eq!(
                accepts(ON_RECOVERY, "about:blank", &by(APP_NONCE, message)),
                None
            );
        }
    }

    #[test]
    fn the_recovery_page_confirms_itself_with_a_signed_ready() {
        let body = by(APP_NONCE, "ready");
        assert_eq!(
            accepts(ON_RECOVERY, "about:blank", &body).as_deref(),
            Some("ready")
        );
        assert_eq!(accepts(ON_RECOVERY, "about:blank", "ready"), None);
        assert_eq!(accepts(ON_DISCORD, "about:blank", &body), None);
        assert!(recovery_page(Failure::Rejected, APP_NONCE).contains(r#"post("ready")"#));
    }

    #[test]
    fn forged_about_blank_messages_are_dropped() {
        for message in ACTIONS.into_iter().chain(["close"]) {
            for body in [message.to_owned(), by(DISCORD_NONCE, message)] {
                assert_eq!(accepts(ON_RECOVERY, "about:blank", &body), None, "{body}");
            }
        }
    }

    #[test]
    fn app_actions_outside_the_recovery_screen_are_dropped() {
        for screen in [ON_DISCORD, Screen::Discord { verifying: true }] {
            for message in ACTIONS.into_iter().chain(["close"]) {
                assert_eq!(
                    accepts(screen, "about:blank", &by(APP_NONCE, message)),
                    None
                );
            }
        }
    }

    const FOREIGN: [&str; 11] = [
        "http://discord.com/login",
        "https://discord.com.evil.example/login",
        "https://discord.com@evil.example/login",
        "https://discord.com./login",
        "https://discord.com:8443/login",
        "https://support.discord.com/hc",
        "https://evildiscord.com/login",
        "https://hcaptcha.com/",
        "data:text/html,x",
        "about:srcdoc",
        "",
    ];

    #[test]
    fn other_origins_are_ignored() {
        for uri in FOREIGN {
            for screen in [ON_DISCORD, ON_RECOVERY] {
                for message in ["candidate:a.b.c", "signed_in", "close", "action:cancel"] {
                    for nonce in [DISCORD_NONCE, APP_NONCE] {
                        assert_eq!(accepts(screen, uri, &by(nonce, message)), None, "{uri}");
                    }
                }
            }
        }
    }

    fn session(now: Instant) -> Session {
        let mut session = Session::new(nonces(), START.to_owned(), now);
        session.state.page_loaded();
        session
    }

    fn commit(session: &mut Session, url: &str, now: Instant) -> Step {
        session.step(LoopEvent::Msg(Msg::PageLoad(url.to_owned())), now)
    }

    fn app(session: &mut Session, body: String, now: Instant) -> Step {
        session.step(
            LoopEvent::Msg(Msg::Ipc {
                uri: "about:blank".to_owned(),
                body,
            }),
            now,
        )
    }

    fn wake(session: &mut Session, now: Instant) -> Step {
        session.step(LoopEvent::Wake, now)
    }

    fn on_recovery(now: Instant) -> Session {
        let mut session = session(now);
        let step = commit(&mut session, "https://discordstatus.com/", now);
        assert_eq!(step.load, Some(Screen::Recovery(Failure::LeftDiscord)));
        session
    }

    #[test]
    fn the_recovery_page_counts_as_loaded_only_after_its_signed_ready() {
        let now = Instant::now();
        let mut session = on_recovery(now);
        assert_eq!(commit(&mut session, "about:blank", now).load, None);
        assert_eq!(session.deadline(), Some(now + APP_READY_TIMEOUT));
        assert_eq!(app(&mut session, "ready".to_owned(), now).load, None);
        assert_eq!(session.deadline(), Some(now + APP_READY_TIMEOUT));
        assert_eq!(app(&mut session, by(APP_NONCE, "ready"), now).load, None);
        assert_eq!(session.deadline(), None);
        assert_eq!(wake(&mut session, now + RELOAD_WINDOW).load, None);
    }

    #[test]
    fn a_blank_page_that_never_sends_ready_is_replaced() {
        let now = Instant::now();
        let mut session = on_recovery(now);
        assert_eq!(commit(&mut session, "about:blank", now).load, None);
        let late = now + APP_READY_TIMEOUT;
        assert_eq!(
            wake(&mut session, late - Duration::from_millis(1)).load,
            None
        );
        assert_eq!(wake(&mut session, late).load, Some(session.shown));
    }

    #[test]
    fn a_ready_before_the_commit_still_confirms_the_page() {
        let now = Instant::now();
        let mut session = on_recovery(now);
        app(&mut session, by(APP_NONCE, "ready"), now);
        assert_eq!(session.deadline(), None);
        assert_eq!(commit(&mut session, "about:blank", now).load, None);
        let shown = session.shown;
        assert_eq!(commit(&mut session, "about:blank", now).load, Some(shown));
    }

    #[test]
    fn a_ready_from_a_superseded_page_is_ignored() {
        let now = Instant::now();
        let mut loads = PageLoads::default();
        loads.showing(Screen::Recovery(Failure::Rejected), now);
        loads.showing(Screen::Discord { verifying: false }, now);
        loads.ready();
        assert_eq!(loads.app, AppPage::Superseded);
    }

    #[test]
    fn a_slow_app_page_keeps_its_load_once_it_has_committed() {
        let now = Instant::now();
        let mut session = on_recovery(now);
        let committed = now + APP_COMMIT_TIMEOUT - Duration::from_millis(1);
        assert_eq!(commit(&mut session, "about:blank", committed).load, None);
        assert_eq!(wake(&mut session, now + APP_COMMIT_TIMEOUT).load, None);
        let almost = committed + APP_READY_TIMEOUT - Duration::from_millis(1);
        assert_eq!(wake(&mut session, almost).load, None);
        assert_eq!(app(&mut session, by(APP_NONCE, "ready"), almost).load, None);
        assert_eq!(session.deadline(), None);
    }

    #[test]
    fn an_app_page_load_that_never_commits_is_reissued() {
        let now = Instant::now();
        let mut session = on_recovery(now);
        let due = now + APP_COMMIT_TIMEOUT;
        assert_eq!(
            wake(&mut session, due - Duration::from_millis(1)).load,
            None
        );
        assert_eq!(wake(&mut session, due).load, Some(session.shown));
    }

    #[test]
    fn any_other_top_page_on_the_recovery_screen_reloads_it() {
        let now = Instant::now();
        for url in [
            "https://discord.com/login",
            "https://evil.example/",
            "about:blank#x",
            "about:blank?x",
        ] {
            let mut session = on_recovery(now);
            let shown = session.shown;
            assert_eq!(commit(&mut session, url, now).load, Some(shown), "{url}");
        }
        let mut session = on_recovery(now);
        commit(&mut session, "about:blank", now);
        app(&mut session, by(APP_NONCE, "ready"), now);
        let shown = session.shown;
        assert_eq!(commit(&mut session, "about:blank", now).load, Some(shown));
    }

    #[test]
    fn about_blank_not_following_our_own_load_reloads_the_recovery_page() {
        let now = Instant::now();
        let mut session = on_recovery(now);
        assert_eq!(commit(&mut session, "about:blank", now).load, None);
        let shown = session.shown;
        assert_eq!(commit(&mut session, "about:blank", now).load, Some(shown));
    }

    #[test]
    fn reloads_are_capped_within_a_window() {
        let now = Instant::now();
        let mut loads = PageLoads::default();
        for _ in 0..MAX_RELOADS {
            assert_eq!(loads.reload(now, Trouble::SentAway), PageAction::Reload);
        }
        assert_eq!(
            loads.reload(now, Trouble::NotLoading),
            PageAction::GiveUp(Trouble::NotLoading)
        );
        assert_eq!(
            loads.reload(now + RELOAD_WINDOW, Trouble::SentAway),
            PageAction::Reload
        );
    }

    #[test]
    fn being_sent_away_too_often_closes_the_window_with_that_reason() {
        let now = Instant::now();
        let mut session = on_recovery(now);
        for _ in 0..MAX_RELOADS {
            assert!(
                commit(&mut session, "https://evil.example/", now)
                    .load
                    .is_some()
            );
        }
        let step = commit(&mut session, "https://evil.example/", now);
        assert!(matches!(
            step.command,
            Some(Command::Exit(BrowserLoginOutcome::Cancelled))
        ));
        assert_eq!(session.trouble, Some(Trouble::SentAway));
        assert!(!session.state.is_open());
    }

    fn worn_out(now: Instant) -> Session {
        let mut session = on_recovery(now);
        session.loads.reloads = std::iter::repeat_n(now, MAX_RELOADS).collect();
        session
    }

    #[test]
    fn a_page_that_never_loads_closes_the_window_with_that_reason() {
        let now = Instant::now();
        let mut session = worn_out(now);
        let step = wake(&mut session, now + APP_COMMIT_TIMEOUT);
        assert!(matches!(
            step.command,
            Some(Command::Exit(BrowserLoginOutcome::Cancelled))
        ));
        assert_eq!(session.trouble, Some(Trouble::NotLoading));
        assert!(!session.state.is_open());
        let sent = Trouble::SentAway.error().to_string();
        let stuck = Trouble::NotLoading.error().to_string();
        assert!(sent.contains("sent away") && stuck.contains("never finished loading"));
    }

    #[test]
    fn giving_up_never_replaces_an_exit_from_the_same_event() {
        let now = Instant::now();
        let mut session = worn_out(now);
        let step = app(
            &mut session,
            by(APP_NONCE, "action:qr"),
            now + APP_COMMIT_TIMEOUT,
        );
        assert!(matches!(
            step.command,
            Some(Command::Exit(BrowserLoginOutcome::UseQr))
        ));
        assert_eq!(session.trouble, None);
        assert!(!session.state.is_open());
    }

    #[test]
    fn a_late_app_page_commit_after_retry_is_ignored_once() {
        let now = Instant::now();
        let mut session = on_recovery(now);
        let step = app(&mut session, by(APP_NONCE, "action:retry"), now);
        assert_eq!(step.load, Some(Screen::Discord { verifying: false }));
        assert_eq!(commit(&mut session, "about:blank", now).load, None);
        assert_eq!(session.state.view(), View::Discord);
        commit(&mut session, "about:blank", now);
        assert_eq!(session.state.view(), View::Recovery(Failure::LeftDiscord));
    }

    #[test]
    fn a_late_commit_after_an_early_ready_and_retry_is_ignored_once() {
        let now = Instant::now();
        let mut session = on_recovery(now);
        app(&mut session, by(APP_NONCE, "ready"), now);
        let step = app(&mut session, by(APP_NONCE, "action:retry"), now);
        assert_eq!(step.load, Some(Screen::Discord { verifying: false }));
        assert_eq!(commit(&mut session, "about:blank", now).load, None);
        assert_eq!(session.state.view(), View::Discord);
        commit(&mut session, "about:blank", now);
        assert_eq!(session.state.view(), View::Recovery(Failure::LeftDiscord));
    }

    #[test]
    fn leaving_discord_without_a_check_shows_its_own_recovery() {
        let now = Instant::now();
        for url in ["https://discordstatus.com/", "about:blank", "about:blank#x"] {
            let mut session = session(now);
            let step = commit(&mut session, url, now);
            assert_eq!(
                step.load,
                Some(Screen::Recovery(Failure::LeftDiscord)),
                "{url}"
            );
        }
    }

    #[test]
    fn leaving_discord_during_a_check_reloads_discord() {
        let now = Instant::now();
        let mut session = session(now);
        let step = session.step(
            LoopEvent::Msg(Msg::Ipc {
                uri: START.to_owned(),
                body: by(DISCORD_NONCE, "candidate:aaa.bbb.ccc"),
            }),
            now,
        );
        assert!(matches!(step.command, Some(Command::Validate(_))));
        let checking = Screen::Discord { verifying: true };
        assert_eq!(
            commit(&mut session, "https://evil.example/", now).load,
            Some(checking)
        );
        assert_eq!(session.state.view(), View::Discord);
    }

    #[test]
    fn discord_pages_are_expected_on_the_discord_screen() {
        let now = Instant::now();
        let mut session = session(now);
        for url in [START, "https://ptb.discord.com/channels/@me"] {
            assert_eq!(commit(&mut session, url, now).load, None, "{url}");
        }
        assert_eq!(session.state.view(), View::Discord);
    }

    #[test]
    fn recovery_buttons_offer_every_recovery_action() {
        let buttons = recovery_buttons();
        for action in RECOVERY_ACTIONS {
            assert!(buttons.contains(&format!("data-action=\"{}\"", action.name())));
        }
    }

    #[test]
    fn links_open_at_most_once_a_second_and_twenty_times_in_all() {
        let now = Instant::now();
        let mut links = LinkLimiter::default();
        assert!(links.allow(now));
        assert!(!links.allow(now + Duration::from_millis(999)));
        let mut at = now;
        for _ in 1..MAX_LINKS {
            at += LINK_INTERVAL;
            assert!(links.allow(at));
        }
        assert!(!links.allow(at + Duration::from_secs(60)));
    }

    #[test]
    fn only_https_links_open_in_the_browser() {
        assert_eq!(
            external_link("https://discord.com/terms").as_deref(),
            Some("https://discord.com/terms")
        );
        for url in [
            "http://discord.com/terms",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "data:text/html,x",
            "mailto:a@b.c",
            "about:blank",
            "https:",
            "",
        ] {
            assert_eq!(external_link(url), None, "{url}");
        }
    }

    #[test]
    fn nonces_are_fresh_and_each_page_gets_only_its_own() {
        let nonces = Nonces::new().unwrap();
        assert_eq!(nonces.discord.len(), 32);
        assert!(nonces.discord.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(nonces.discord, nonces.app);
        assert_ne!(nonces.discord, Nonces::new().unwrap().discord);
        let script = capture_script(&nonces.discord);
        let guard = script.find("includes(location.host)) return;").unwrap();
        assert_eq!(script.matches(nonces.discord.as_str()).count(), 1);
        assert!(script.find(nonces.discord.as_str()).unwrap() > guard);
        assert!(!script.contains(nonces.app.as_str()));
        let recovery = recovery_page(Failure::Rejected, &nonces.app);
        assert!(recovery.contains(nonces.app.as_str()));
        assert!(!recovery.contains(nonces.discord.as_str()));
        assert!(!script.contains("window.ipc") && !recovery.contains("window.ipc"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_status_view_only_cancels_while_a_check_runs() {
        let checking = Screen::Discord { verifying: true };
        let cancel = by(APP_NONCE, "action:cancel");
        assert!(status_cancels(APP_NONCE, checking, "about:blank", &cancel));
        assert!(!status_cancels(
            APP_NONCE,
            ON_DISCORD,
            "about:blank",
            &cancel
        ));
        assert!(!status_cancels(
            APP_NONCE,
            checking,
            "about:blank",
            "action:cancel"
        ));
        assert!(!status_cancels(
            APP_NONCE,
            checking,
            "https://discord.com/",
            &cancel
        ));
        for message in ["close", "candidate:a.b.c", "action:retry"] {
            assert!(!status_cancels(
                APP_NONCE,
                checking,
                "about:blank",
                &by(APP_NONCE, message)
            ));
        }
        assert!(STATUS_PAGE.contains("postMessage(nonce + \"|action:cancel\")"));
    }

    #[test]
    fn capture_script_only_runs_on_the_discord_login_hosts() {
        let script = capture_script(DISCORD_NONCE);
        assert!(script.contains(
            r#"if (location.protocol !== "https:" || !["discord.com", "ptb.discord.com", "canary.discord.com"].includes(location.host)) return;"#
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn only_genuine_load_failures_are_recovered() {
        use webkit2gtk::glib::Error;
        use webkit2gtk::{NetworkError, PolicyError};
        assert!(!genuine_load_failure(&Error::new(
            NetworkError::Cancelled,
            "x"
        )));
        assert!(!genuine_load_failure(&Error::new(
            PolicyError::FrameLoadInterruptedByPolicyChange,
            "x"
        )));
        assert!(genuine_load_failure(&Error::new(NetworkError::Failed, "x")));
        assert!(genuine_load_failure(&Error::new(
            NetworkError::Transport,
            "x"
        )));
        assert!(genuine_load_failure(&Error::new(PolicyError::Failed, "x")));
    }
}
