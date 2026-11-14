//! `dstreamy login --browser`: Discord's own login page in a window; the token is read from its API requests.

use std::time::Instant;

use dstreamy_discord::GatewayError;
use serde::Serialize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::window::{Window, WindowBuilder};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use crate::discord;
use crate::login_capture::{
    self, BrowserLoginOutcome, Command, Failure, Job, Recovery, ValidationError, VerifiedLogin,
};
use crate::output::Out;
use crate::{CliError, Ctx};

mod pages;
mod session;
mod window;

use pages::{attention_line, quiet_line};
#[cfg(feature = "test-hooks")]
use session::Screen;
use session::{LoopEvent, Msg, Nonces, Notice, Session};
#[cfg(feature = "test-hooks")]
use window::BACKGROUND;
#[cfg(target_os = "macos")]
use window::edit_menu;
#[cfg(target_os = "linux")]
use window::finish_gtk_work;
use window::{LOGIN_TITLE, Views, build_views, set_title, set_window_visible, show};

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

fn login_error(e: impl std::fmt::Display) -> CliError {
    CliError::BrowserLogin(e.to_string())
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

/// Blocks the main thread until the user logs in or leaves; only a validated token is returned.
pub fn run(
    ctx: &Ctx,
    runtime: &Handle,
    out: &Out,
    recovery: Recovery,
) -> Result<BrowserLoginOutcome, CliError> {
    let mut event_loop = EventLoopBuilder::<Msg>::with_user_event().build();
    let nonces = Nonces::new()?;
    let start_url = login_url();
    #[cfg(target_os = "macos")]
    let _menu = edit_menu()?;
    let window = WindowBuilder::new()
        .with_title(LOGIN_TITLE)
        .with_inner_size(WINDOW_SIZE)
        .with_min_inner_size(WINDOW_SIZE)
        .with_max_inner_size(WINDOW_SIZE)
        .with_resizable(false)
        .build(&event_loop)
        .map_err(login_error)?;
    let views = build_views(&event_loop, &window, &nonces, &start_url)?;
    let mut session = Session::new(nonces, start_url, recovery, Instant::now());
    let outcome = drive(
        &mut event_loop,
        &mut session,
        &views,
        &window,
        ctx,
        runtime,
        out,
    );
    drop(views);
    drop(window);
    #[cfg(target_os = "linux")]
    finish_gtk_work();
    if let Some(trouble) = session.trouble {
        return Err(trouble.error(session.recovery));
    }
    outcome.ok_or_else(|| CliError::BrowserLogin("the login window closed unexpectedly".into()))
}

fn drive(
    event_loop: &mut EventLoop<Msg>,
    session: &mut Session,
    views: &Views,
    window: &Window,
    ctx: &Ctx,
    runtime: &Handle,
    out: &Out,
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
        if step.visible == Some(false) {
            set_window_visible(window, false);
        }
        match step.notice {
            Some(Notice::Verifying) => report_verifying(out),
            Some(Notice::Attention(failure)) => report_attention(out, failure),
            Some(Notice::Quiet(failure)) => report_quiet(out, failure),
            None => {}
        }
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
            show(&views.main, screen, session);
        }
        if step.visible == Some(true) {
            set_window_visible(window, true);
        }
        if step.retitle {
            set_title(window, session.shown);
            #[cfg(target_os = "linux")]
            {
                use gtk::prelude::WidgetExt;
                views.status_bar.set_visible(session.shown.verifying());
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

async fn check_token(ctx: Ctx, token: String) -> Result<VerifiedLogin, ValidationError<()>> {
    match discord::current_user(&ctx, token.clone()).await {
        Ok(user) => Ok(VerifiedLogin { token, user }),
        Err(CliError::Gateway(GatewayError::Rejected(_))) => Err(ValidationError::Rejected),
        Err(_) => Err(ValidationError::Transient(())),
    }
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum ProgressRow {
    Verifying,
    Attention,
}

fn report_verifying(out: &Out) {
    if out.json {
        out.json_line(&ProgressRow::Verifying);
    } else {
        out.line("Verifying the Discord login… (Ctrl+C cancels)");
    }
}

fn report_attention(out: &Out, failure: Failure) {
    if out.json {
        out.json_line(&ProgressRow::Attention);
    } else {
        out.line(attention_line(failure));
    }
}

/// Only the terminal hears of it: the page is still Discord's, and the JSON parent has no terminal.
fn report_quiet(out: &Out, failure: Failure) {
    if !out.json {
        out.line(quiet_line(failure));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::time::Duration;

    use super::pages::*;
    use super::session::*;
    #[cfg(target_os = "linux")]
    use super::window::*;
    use super::*;
    use crate::discord::BRIEF_VALIDATION_TIMEOUT;
    use crate::login_capture::{
        Action, BrowserState, Candidate, RECOVERY_ACTIONS, SeenCandidates, Validation, View,
        parse_candidate,
    };

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
            Validation::Accepted(VerifiedLogin {
                token: "aaa.bbb.ccc".to_owned(),
                user: dstreamy_discord::CurrentUser {
                    id: "1".to_owned(),
                    username: "user".to_owned(),
                    global_name: None,
                },
            }),
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
        assert!(
            recovery_page(Failure::Rejected, APP_NONCE, Recovery::WithTerminal)
                .contains(r#"post("ready")"#)
        );
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
        let mut session = Session::new(nonces(), START.to_owned(), Recovery::WithTerminal, now);
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
        let sent = Trouble::SentAway.error(Recovery::WithTerminal).to_string();
        let stuck = Trouble::NotLoading
            .error(Recovery::WithTerminal)
            .to_string();
        assert!(sent.contains("sent away") && stuck.contains("never finished loading"));
        assert!(sent.contains("QR code"));
        let window = Trouble::NotLoading.error(Recovery::WindowOnly).to_string();
        assert!(
            window.ends_with("so it was closed. Try the login again"),
            "{window}"
        );
        assert!(
            !window.contains("QR") && !window.contains("token"),
            "{window}"
        );
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

    fn discord(session: &mut Session, message: &str, now: Instant) -> Step {
        session.step(
            LoopEvent::Msg(Msg::Ipc {
                uri: "https://discord.com/app".to_owned(),
                body: by(DISCORD_NONCE, message),
            }),
            now,
        )
    }

    #[test]
    fn the_window_hides_once_on_sign_in_and_returns_for_recovery() {
        let now = Instant::now();
        let mut session = session(now);
        let step = discord(&mut session, "signed_in", now);
        assert_eq!(step.visible, Some(false));
        assert_eq!(step.notice, Some(Notice::Verifying));
        let step = wake(&mut session, now);
        assert_eq!((step.visible, step.notice), (None, None));
        let step = discord(&mut session, "candidate:aaa.bbb.ccc", now);
        assert!(matches!(step.command, Some(Command::Validate(_))));
        assert_eq!((step.visible, step.notice), (None, None));
        let late = now + BRIEF_VALIDATION_TIMEOUT;
        let step = wake(&mut session, late);
        assert_eq!(step.load, Some(Screen::Recovery(Failure::Unreachable)));
        assert_eq!(step.visible, Some(true));
        assert_eq!(step.notice, Some(Notice::Attention(Failure::Unreachable)));
        let step = wake(&mut session, late);
        assert_eq!((step.visible, step.notice), (None, None));
    }

    #[test]
    fn the_attention_line_gives_the_recovery_reason() {
        let line = attention_line(Failure::Rejected);
        assert!(line.starts_with("The login needs your attention in the window: "));
        assert!(line.ends_with(recovery_text(Failure::Rejected).1));
        assert!(
            recovery_page(Failure::Rejected, APP_NONCE, Recovery::WithTerminal)
                .contains(recovery_text(Failure::Rejected).1)
        );
    }

    #[test]
    fn a_recovery_before_sign_in_needs_no_notice() {
        let now = Instant::now();
        let mut session = session(now);
        let step = commit(&mut session, "https://discordstatus.com/", now);
        assert_eq!(step.load, Some(Screen::Recovery(Failure::LeftDiscord)));
        assert_eq!((step.visible, step.notice), (None, None));
    }

    #[test]
    fn a_rejection_before_sign_in_is_reported_on_the_terminal_only() {
        let now = Instant::now();
        let mut session = session(now);
        let step = discord(&mut session, "candidate:aaa.bbb.ccc", now);
        let Some(Command::Validate(job)) = step.command else {
            panic!("expected a validation job");
        };
        let step = session.step(
            LoopEvent::Msg(Msg::Validated {
                generation: job.generation,
                seen: job.seen,
                result: Validation::Rejected,
            }),
            now,
        );
        assert_eq!(step.notice, Some(Notice::Quiet(Failure::Rejected)));
        assert_eq!((step.load, step.visible), (None, None));
        assert_eq!(session.state.view(), View::Discord);
        assert_eq!(wake(&mut session, now).notice, None);
        assert!(quiet_line(Failure::Rejected).contains(recovery_text(Failure::Rejected).1));
    }

    #[test]
    fn the_window_hides_when_the_login_ends() {
        let now = Instant::now();
        let mut session = session(now);
        let step = session.step(LoopEvent::CloseRequested, now);
        assert!(matches!(step.command, Some(Command::Exit(_))));
        assert_eq!(step.visible, Some(false));
        assert_eq!(step.notice, None);
    }

    #[test]
    fn recovery_buttons_offer_every_recovery_action() {
        let buttons = recovery_buttons(Recovery::WithTerminal);
        for action in RECOVERY_ACTIONS {
            assert!(buttons.contains(&format!("data-action=\"{}\"", action.name())));
        }
    }

    #[test]
    fn a_window_only_login_offers_no_terminal_fallback() {
        let page = recovery_page(Failure::Rejected, APP_NONCE, Recovery::WindowOnly);
        for (action, offered) in [
            (Action::Retry, true),
            (Action::Cancel, true),
            (Action::UseQr, false),
            (Action::UseToken, false),
        ] {
            let button = format!("data-action=\"{}\"", action.name());
            assert_eq!(page.contains(&button), offered, "{action:?}");
        }
        assert!(!page.contains("QR") && !page.contains("{hint}"));
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
    fn navigation_stays_on_discord_and_its_captcha_frames() {
        let start = "http://login-smoke.invalid/";
        for url in [
            "https://discord.com/login",
            "https://ptb.discord.com/app",
            "https://canary.discord.com/channels/@me",
            "about:blank",
            "about:srcdoc",
            start,
            "blob:https://discord.com/0b8e2c1a",
            "https://newassets.hcaptcha.com/captcha/v1/abc/static/hcaptcha.html#frame=checkbox",
            "https://hcaptcha.com/checkcaptcha",
            "https://challenges.cloudflare.com/cdn-cgi/challenge-platform/h/b/turnstile/if/ov2",
            "https://www.google.com/recaptcha/api2/anchor?k=x",
            "https://www.recaptcha.net/recaptcha/api2/bframe",
        ] {
            assert!(allowed_navigation(url, start), "{url}");
        }
        for url in [
            "https://example.org/",
            "http://discord.com/login",
            "https://discord.com:8443/login",
            "https://support.discord.com/hc",
            "https://discord.com.evil.example/",
            "https://evilhcaptcha.com/",
            "https://hcaptcha.com.evil.example/",
            "http://newassets.hcaptcha.com/captcha",
            "https://www.google.com/search?q=x",
            "https://www.google.com/recaptchas/",
            "http://login-smoke.invalid/elsewhere",
            "blob:https://example.org/0b8e2c1a",
            "data:text/html,x",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "",
        ] {
            assert!(!allowed_navigation(url, start), "{url}");
        }
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
        let recovery = recovery_page(Failure::Rejected, &nonces.app, Recovery::WithTerminal);
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
