//! What the login window decides from page messages, page loads and timers, apart from the window itself.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::login_error;
use crate::CliError;
use crate::login_capture::{
    self, BrowserState, Command, Failure, Recovery, SeenCandidates, Validation, VerifiedLogin, View,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Screen {
    Discord { verifying: bool },
    Recovery(Failure),
}

impl Screen {
    pub(super) fn of(state: &BrowserState) -> Self {
        match state.view() {
            View::Discord => Self::Discord {
                verifying: state.verifying(),
            },
            View::Recovery(failure) => Self::Recovery(failure),
        }
    }

    pub(super) fn same_page(self, other: Self) -> bool {
        matches!((self, other), (Self::Discord { .. }, Self::Discord { .. })) || self == other
    }

    #[cfg(any(target_os = "linux", test))]
    pub(super) fn verifying(self) -> bool {
        matches!(self, Self::Discord { verifying: true })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Origin {
    Discord,
    App,
    Other,
}

/// Other subdomains such as support.discord.com are hosted by third parties.
pub(super) const DISCORD_HOSTS: [&str; 3] =
    ["discord.com", "ptb.discord.com", "canary.discord.com"];

pub(super) fn origin(uri: &str) -> Origin {
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

/// Hosts Discord's login page frames for its captcha (hCaptcha, Cloudflare Turnstile, reCAPTCHA).
pub(super) const CAPTCHA_DOMAINS: [&str; 2] = ["hcaptcha.com", "challenges.cloudflare.com"];
pub(super) const CAPTCHA_PREFIXES: [&str; 2] = [
    "https://www.google.com/recaptcha/",
    "https://www.recaptcha.net/recaptcha/",
];

/// wry applies this to every frame without saying which, so it also admits the captcha frames.
pub(super) fn allowed_navigation(url: &str, start_url: &str) -> bool {
    if url == start_url || url == "about:srcdoc" || origin(url) != Origin::Other {
        return true;
    }
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    match parsed.scheme() {
        "blob" => origin(&parsed.origin().ascii_serialization()) == Origin::Discord,
        "https" if parsed.port().is_none() => {
            let host = parsed.host_str().unwrap_or_default();
            CAPTCHA_DOMAINS
                .iter()
                .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
                || CAPTCHA_PREFIXES
                    .iter()
                    .any(|prefix| url.starts_with(prefix))
        }
        _ => false,
    }
}

pub(super) struct Nonces {
    pub(super) discord: String,
    pub(super) app: String,
}

impl Nonces {
    pub(super) fn new() -> Result<Self, CliError> {
        Ok(Self {
            discord: new_nonce()?,
            app: new_nonce()?,
        })
    }
}

pub(super) fn signed<'a>(nonce: &str, body: &'a str) -> Option<&'a str> {
    body.strip_prefix(nonce)?.strip_prefix('|')
}

/// A frame URI is forgeable (Linux reports the top page's, about:blank is anyone's), so every page signs.
pub(super) fn accepted_ipc<'a>(
    nonces: &Nonces,
    shown: Screen,
    uri: &str,
    body: &'a str,
) -> Option<&'a str> {
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
pub(super) fn status_cancels(nonce: &str, shown: Screen, uri: &str, body: &str) -> bool {
    shown.verifying() && origin(uri) == Origin::App && signed(nonce, body) == Some("action:cancel")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Trouble {
    SentAway,
    NotLoading,
}

impl Trouble {
    pub(super) fn error(self, recovery: Recovery) -> CliError {
        let cause = match self {
            Self::SentAway => "the login window kept being sent away from Discord's login",
            Self::NotLoading => "D-Streamy's page in the login window never finished loading",
        };
        let next = match recovery {
            Recovery::WithTerminal => "Run the login again, or use the QR code or a token instead",
            Recovery::WindowOnly => "Try the login again",
        };
        CliError::BrowserLogin(format!("{cause}, so it was closed. {next}"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PageDecision {
    Expected,
    LeftDiscord,
    Reload(Trouble),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PageAction {
    Expected,
    Recover,
    Reload,
    GiveUp(Trouble),
}

pub(super) const APP_COMMIT_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) const APP_READY_TIMEOUT: Duration = Duration::from_secs(20);
pub(super) const RELOAD_WINDOW: Duration = Duration::from_secs(30);
pub(super) const MAX_RELOADS: usize = 5;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum AppPage {
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
pub(super) struct PageLoads {
    pub(super) app: AppPage,
    pub(super) reloads: VecDeque<Instant>,
}

impl PageLoads {
    pub(super) fn showing(&mut self, screen: Screen, now: Instant) {
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

    pub(super) fn ready(&mut self) {
        if let AppPage::Loading { committed, .. } = self.app {
            self.app = AppPage::Ready { committed };
        }
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        match self.app {
            AppPage::Loading { deadline, .. } => Some(deadline),
            _ => None,
        }
    }

    pub(super) fn page_load(
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

    pub(super) fn expire(&mut self, now: Instant) -> Option<Trouble> {
        match self.app {
            AppPage::Loading { deadline, .. } if now >= deadline => {
                self.app = AppPage::None;
                Some(Trouble::NotLoading)
            }
            _ => None,
        }
    }

    pub(super) fn reload(&mut self, now: Instant, trouble: Trouble) -> PageAction {
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

pub(super) const LINK_INTERVAL: Duration = Duration::from_secs(1);
pub(super) const MAX_LINKS: u32 = 20;

#[derive(Default)]
pub(super) struct LinkLimiter {
    pub(super) opened: u32,
    pub(super) last: Option<Instant>,
}

impl LinkLimiter {
    pub(super) fn allow(&mut self, now: Instant) -> bool {
        if self.opened >= MAX_LINKS || self.last.is_some_and(|last| now < last + LINK_INTERVAL) {
            return false;
        }
        self.opened += 1;
        self.last = Some(now);
        true
    }
}

pub(super) fn external_link(url: &str) -> Option<String> {
    let url = url::Url::parse(url).ok()?;
    (url.scheme() == "https" && url.host_str().is_some()).then(|| url.to_string())
}

pub(super) fn new_nonce() -> Result<String, CliError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(login_error)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

pub(super) enum Msg {
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
        result: Validation<VerifiedLogin>,
    },
}

pub(super) enum LoopEvent {
    Msg(Msg),
    CloseRequested,
    Wake,
}

pub(super) struct Step {
    pub(super) command: Option<Command>,
    pub(super) load: Option<Screen>,
    pub(super) retitle: bool,
    pub(super) visible: Option<bool>,
    pub(super) notice: Option<Notice>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Notice {
    Verifying,
    Attention(Failure),
    /// A check failed while the Discord page stays up, so only the terminal can say so.
    Quiet(Failure),
}

/// Everything the login window decides, apart from the window and the webviews themselves.
pub(super) struct Session {
    pub(super) state: BrowserState,
    pub(super) shown: Screen,
    pub(super) visible: bool,
    pub(super) loads: PageLoads,
    pub(super) nonces: Nonces,
    pub(super) start_url: String,
    pub(super) recovery: Recovery,
    pub(super) trouble: Option<Trouble>,
}

impl Session {
    pub(super) fn new(nonces: Nonces, start_url: String, recovery: Recovery, now: Instant) -> Self {
        Self {
            state: BrowserState::new(now),
            shown: Screen::Discord { verifying: false },
            visible: true,
            loads: PageLoads::default(),
            nonces,
            start_url,
            recovery,
            trouble: None,
        }
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        [self.state.deadline(), self.loads.deadline()]
            .into_iter()
            .flatten()
            .min()
    }

    pub(super) fn step(&mut self, event: LoopEvent, now: Instant) -> Step {
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
        let visible = self.state.window_visible();
        let toggle = (visible != self.visible).then_some(visible);
        self.visible = visible;
        let quiet = self.state.take_quiet_failure();
        let notice = match (toggle, current) {
            _ if !self.state.is_open() => None,
            (Some(false), _) => Some(Notice::Verifying),
            (Some(true), Screen::Recovery(failure)) => Some(Notice::Attention(failure)),
            _ => quiet.map(Notice::Quiet),
        };
        Step {
            command,
            load,
            retitle,
            visible: toggle,
            notice,
        }
    }

    pub(super) fn page_load(&mut self, url: &str, now: Instant) -> PageAction {
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
    pub(super) fn give_up(
        &mut self,
        command: Option<Command>,
        page: PageAction,
    ) -> Option<Command> {
        match (page, command) {
            (PageAction::GiveUp(trouble), None | Some(Command::Validate(_))) => {
                self.trouble = Some(trouble);
                self.state.close()
            }
            (_, command) => command,
        }
    }
}
