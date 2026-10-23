//! `dstreamy tui`: pickers, settings and a live health panel; `model` holds all state, `view` only draws it.

#[cfg(test)]
mod fixtures;
pub mod model;
pub mod view;

use std::fmt::Display;
use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use dstreamy_core::Config;
use dstreamy_discord::{DiscordClient, Session, SessionEvent};
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::capture::{self, PortalBook};
use crate::discord::{self, Persistence};
use crate::lock::SessionLock;
use crate::signals::Signals;
use crate::stream::{self, FORCED_EXIT};
use crate::{CliError, Ctx};
use model::{App, Effect, Key, Msg, Notice, StartRequest, Tone};

const INPUT_POLL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub include_platform: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Key(Key),
    Redraw,
}

pub async fn run(ctx: &Ctx, opts: Options) -> Result<ExitCode, CliError> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(CliError::NoTerminal);
    }
    ctx.paths.load_token()?.ok_or(CliError::NotLoggedIn)?;
    let signals = Signals::new();
    let restore = RestoreOnDrop;
    let mut terminal = ratatui::try_init().map_err(|e| CliError::Terminal(e.to_string()))?;
    let (tx, input) = mpsc::unbounded_channel();
    let reader = InputReader::spawn(tx);
    let result = drive(&mut terminal, ctx, input, opts, Some(signals), |_| {}).await;
    reader.stop();
    drop(restore);
    result
}

pub fn log_file(ctx: &Ctx) -> std::path::PathBuf {
    ctx.paths.config_file.with_file_name("dstreamy.log")
}

struct RestoreOnDrop;

impl Drop for RestoreOnDrop {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

/// Reads terminal events on a thread; polling lets it notice `stop` instead of blocking forever.
struct InputReader {
    stop: Arc<AtomicBool>,
}

impl InputReader {
    fn spawn(tx: mpsc::UnboundedSender<Input>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        std::thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                match event::poll(INPUT_POLL) {
                    Ok(false) => continue,
                    Ok(true) => {}
                    Err(_) => break,
                }
                let input = match event::read() {
                    Ok(Event::Key(key)) => key_input(key),
                    Ok(Event::Resize(..)) => Some(Input::Redraw),
                    Ok(_) => None,
                    Err(_) => break,
                };
                if let Some(input) = input
                    && tx.send(input).is_err()
                {
                    break;
                }
            }
        });
        Self { stop }
    }

    fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn key_input(key: KeyEvent) -> Option<Input> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let key = match key.code {
        KeyCode::Char('c') if ctrl => Key::Interrupt,
        KeyCode::Char(c) if !ctrl => Key::Char(c),
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Backspace => Key::Backspace,
        _ => return None,
    };
    Some(Input::Key(key))
}

struct Live {
    client: Arc<DiscordClient>,
    session: Session,
    events: broadcast::Receiver<SessionEvent>,
    portal: Option<PortalBook>,
    lock: Option<SessionLock>,
}

/// `on_draw` sees every frame; with `signals`, a second interrupt during shutdown exits at once.
pub async fn drive<B: Backend>(
    terminal: &mut Terminal<B>,
    ctx: &Ctx,
    mut input: mpsc::UnboundedReceiver<Input>,
    opts: Options,
    mut signals: Option<Signals>,
    mut on_draw: impl FnMut(&Terminal<B>),
) -> Result<ExitCode, CliError>
where
    B::Error: Display,
{
    let config = Config::load(&ctx.paths.config_file)?;
    let mut app = App::new(&config);
    let (tx, mut msgs) = mpsc::unbounded_channel();
    let targets = tokio::spawn({
        let tx = tx.clone();
        async move {
            let (targets, error) = capture::targets(opts.include_platform).await;
            let error = error.map(|e| e.to_string());
            let _ = tx.send(Msg::Targets { targets, error });
        }
    });
    let mut connecting: Option<JoinHandle<Result<DiscordClient, CliError>>> = Some(tokio::spawn({
        let ctx = ctx.clone();
        async move { discord::connect(&ctx, Persistence::Long).await }
    }));
    let mut live: Option<Live> = None;
    let mut tasks = vec![targets];

    let result = loop {
        if let Err(e) = render(terminal, &app, &mut on_draw) {
            break Err(e);
        }
        let msg = tokio::select! {
            i = input.recv() => match i {
                Some(Input::Key(key)) => Msg::Key(key),
                Some(Input::Redraw) => continue,
                None => Msg::Key(Key::Interrupt),
            },
            Some(msg) = msgs.recv() => msg,
            joined = async { connecting.as_mut().expect("guarded").await }, if connecting.is_some() => {
                connecting = None;
                match joined {
                    Ok(Ok(client)) => {
                        let client = Arc::new(client);
                        let user = client.user().username.clone();
                        let session = Session::new(client.clone());
                        let events = session.subscribe();
                        tasks.push(tokio::spawn({
                            let client = client.clone();
                            let tx = tx.clone();
                            async move {
                                let _ = tx.send(Msg::Guilds(discord::guilds(&client).await));
                            }
                        }));
                        live = Some(Live { client, session, events, portal: None, lock: None });
                        Msg::Connected { user }
                    }
                    Ok(Err(e)) => Msg::ConnectFailed(e.to_string()),
                    Err(e) => Msg::ConnectFailed(e.to_string()),
                }
            },
            event = next_event(&mut live) => match event {
                Ok(event) => {
                    if matches!(event, SessionEvent::State { .. }) {
                        remember_token(ctx, &mut live, &mut app);
                        release_if_stopped(&mut live);
                    }
                    Msg::Session(event)
                }
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "session events lagged");
                    let Some(l) = &live else { continue };
                    let state = l.session.state().borrow().clone();
                    remember_token(ctx, &mut live, &mut app);
                    release_if_stopped(&mut live);
                    Msg::Session(SessionEvent::State { state })
                }
                Err(RecvError::Closed) => unreachable!("the session holds the event sender"),
            },
            () = next_signal(&mut signals) => Msg::Key(Key::Interrupt),
        };
        let Some(effect) = app.update(msg) else {
            continue;
        };
        match effect {
            Effect::LoadChannels(guild_id) => {
                if let Some(l) = &live {
                    let client = l.client.clone();
                    let tx = tx.clone();
                    tasks.push(tokio::spawn(async move {
                        let result = discord::voice_channels(&client, &guild_id)
                            .await
                            .map_err(|e| e.to_string());
                        let _ = tx.send(Msg::Channels { guild_id, result });
                    }));
                }
            }
            Effect::Start(request) => {
                if let Err(e) = start(ctx, &mut live, &mut app, request).await {
                    app.update(Msg::StartFailed(e));
                }
            }
            Effect::Stop => {
                app.update(Msg::Stopping);
                if let Err(e) = render(terminal, &app, &mut on_draw) {
                    break Err(e);
                }
                let Some(l) = &live else { continue };
                tokio::select! {
                    stopped = l.session.stop() => {
                        if let Err(e) = stopped {
                            app.update(Msg::Notice(Notice {
                                tone: Tone::Bad,
                                text: format!("stopping the stream failed: {e}"),
                            }));
                        }
                    }
                    () = next_interrupt(&mut input) => break Ok(()),
                    () = next_signal(&mut signals) => break Ok(()),
                }
            }
            Effect::Quit => break Ok(()),
        }
    };

    if let Some(mut signals) = signals.take() {
        tokio::spawn(async move {
            tokio::select! {
                () = signals.recv() => {}
                () = next_interrupt(&mut input) => {}
            }
            ratatui::restore();
            std::process::exit(i32::from(FORCED_EXIT));
        });
    }
    if app.streaming() {
        app.update(Msg::Stopping);
        let _ = render(terminal, &app, &mut on_draw);
    }
    if let Some(handle) = connecting {
        handle.abort();
    }
    for task in tasks {
        task.abort();
    }
    if let Some(Live {
        client,
        session,
        portal,
        lock: _lock,
        ..
    }) = live.take()
    {
        stream::teardown(session, client).await;
        if let Some(mut book) = portal
            && let Err(e) = book.save(&ctx.paths)
        {
            tracing::warn!("saving the portal restore token: {e}");
        }
    }
    result.map(|()| {
        if app.failed() {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        }
    })
}

fn render<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &App,
    on_draw: &mut impl FnMut(&Terminal<B>),
) -> Result<(), CliError>
where
    B::Error: Display,
{
    terminal
        .draw(|frame| view::draw(frame, app))
        .map_err(|e| CliError::Terminal(e.to_string()))?;
    on_draw(terminal);
    Ok(())
}

async fn next_event(live: &mut Option<Live>) -> Result<SessionEvent, RecvError> {
    match live {
        Some(l) => l.events.recv().await,
        None => std::future::pending().await,
    }
}

/// Skips other keys; a closed input never interrupts.
async fn next_interrupt(input: &mut mpsc::UnboundedReceiver<Input>) {
    loop {
        match input.recv().await {
            Some(Input::Key(Key::Interrupt)) => return,
            Some(_) => {}
            None => std::future::pending().await,
        }
    }
}

async fn next_signal(signals: &mut Option<Signals>) {
    match signals {
        Some(s) => s.recv().await,
        None => std::future::pending().await,
    }
}

fn release_if_stopped(live: &mut Option<Live>) {
    if let Some(l) = live
        && stream::has_stopped(&l.session)
    {
        l.lock = None;
    }
}

fn remember_token(ctx: &Ctx, live: &mut Option<Live>, app: &mut App) {
    let Some(book) = live.as_mut().and_then(|l| l.portal.as_mut()) else {
        return;
    };
    if let Err(e) = book.save(&ctx.paths) {
        app.update(Msg::Notice(Notice {
            tone: Tone::Warn,
            text: format!("saving the portal restore token: {e}"),
        }));
    }
}

async fn start(
    ctx: &Ctx,
    live: &mut Option<Live>,
    app: &mut App,
    request: StartRequest,
) -> Result<(), String> {
    let Some(l) = live else {
        return Err("not connected to Discord".to_owned());
    };
    let lock = match l.lock.take() {
        Some(lock) => lock,
        None => SessionLock::acquire(&ctx.paths).map_err(|e| e.to_string())?,
    };
    let mut config = Config::load(&ctx.paths.config_file).map_err(|e| e.to_string())?;
    let restore_token = capture::restore_token(&config);
    apply_request(&mut config, &request);
    if let Err(e) = config.save(&ctx.paths.config_file) {
        app.update(Msg::Notice(Notice {
            tone: Tone::Warn,
            text: format!("saving the settings: {e}"),
        }));
    }
    let picked = capture::source_for(&request.target_id, restore_token.clone())
        .map_err(|e| e.to_string())?;
    let capture = stream::capture_config(&request.target_id, &request.stream, request.crop);
    l.portal = Some(PortalBook::new(restore_token, picked.portal_token));
    stream::start(
        &l.session,
        request.guild_id,
        request.channel_id,
        picked.source,
        capture,
    )
    .await
    .map_err(|e| e.to_string())?;
    if !stream::has_stopped(&l.session) {
        l.lock = Some(lock);
    }
    Ok(())
}

/// A saved portal restore token is kept while the portal target stays selected.
fn apply_request(config: &mut Config, request: &StartRequest) {
    config.stream = request.stream;
    config.crop = request.crop;
    let restore_token = capture::restore_token(config);
    if let Some(target) = capture::config_target(&request.target_id, restore_token) {
        config.target = Some(target);
    }
}

#[cfg(test)]
mod tests {
    use dstreamy_core::{CaptureTarget, CropRect, StreamConfig};

    use super::*;

    #[test]
    fn start_requests_update_the_saved_settings() {
        let request = |target_id: &str| StartRequest {
            guild_id: "g".into(),
            channel_id: "c".into(),
            target_id: target_id.into(),
            stream: StreamConfig {
                bitrate_kbps: 2_000,
                ..StreamConfig::default()
            },
            crop: Some(CropRect::new(1, 2, 3, 4)),
        };
        let portal = CaptureTarget::Portal {
            restore_token: Some("tok".into()),
        };
        let mut config = Config {
            target: Some(portal.clone()),
            ..Config::default()
        };
        apply_request(&mut config, &request("portal"));
        assert_eq!(config.target, Some(portal.clone()));
        assert_eq!(config.stream.bitrate_kbps, 2_000);
        assert_eq!(config.crop, Some(CropRect::new(1, 2, 3, 4)));

        apply_request(&mut config, &request("test-pattern"));
        assert_eq!(config.target, Some(portal));
        apply_request(&mut config, &request("display:2"));
        assert_eq!(config.target, Some(CaptureTarget::Display { id: 2 }));
        apply_request(&mut config, &request("portal"));
        assert_eq!(
            config.target,
            Some(CaptureTarget::Portal {
                restore_token: None
            })
        );
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Option<Input> {
        key_input(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn crossterm_keys_map_to_model_keys() {
        assert_eq!(
            key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(Input::Key(Key::Interrupt))
        );
        assert_eq!(
            key(KeyCode::Char('q'), KeyModifiers::NONE),
            Some(Input::Key(Key::Char('q')))
        );
        assert_eq!(key(KeyCode::Char('x'), KeyModifiers::CONTROL), None);
        assert_eq!(
            key(KeyCode::BackTab, KeyModifiers::SHIFT),
            Some(Input::Key(Key::BackTab))
        );
        assert_eq!(key(KeyCode::F(1), KeyModifiers::NONE), None);
        let mut release = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(key_input(release), None);
    }
}
