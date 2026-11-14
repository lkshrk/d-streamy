//! The tao, wry and GTK side of the login window.

use std::cell::RefCell;
#[cfg(target_os = "linux")]
use std::time::Duration;
use std::time::Instant;

use tao::event_loop::EventLoop;
use tao::window::Window;
use wry::{NewWindowResponse, PageLoadEvent, WebView, WebViewBuilder};

use super::login_error;
#[cfg(target_os = "linux")]
use super::pages::STATUS_PAGE;
use super::pages::{capture_script, recovery_page};
use super::session::{
    LinkLimiter, Msg, Nonces, Screen, Session, allowed_navigation, external_link,
};
use crate::CliError;

pub(super) const BACKGROUND: (u8, u8, u8, u8) = (0x31, 0x33, 0x38, 0xff);

#[cfg(target_os = "linux")]
const STATUS_HEIGHT: i32 = 56;

fn open_in_browser(url: &str) {
    #[cfg(target_os = "linux")]
    let opener = "xdg-open";
    #[cfg(target_os = "macos")]
    let opener = "open";
    let spawned = std::process::Command::new(opener)
        .arg(url)
        .env_remove(dstreamy_core::TOKEN_ENV)
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

#[cfg(target_os = "linux")]
pub(super) fn genuine_load_failure(error: &webkit2gtk::glib::Error) -> bool {
    use webkit2gtk::{NetworkError, PolicyError};
    !(error.matches(NetworkError::Cancelled)
        || error.matches(PolicyError::FrameLoadInterruptedByPolicyChange))
}

pub(super) struct Views {
    pub(super) main: WebView,
    #[cfg(target_os = "linux")]
    _status: WebView,
    #[cfg(target_os = "linux")]
    pub(super) status_bar: gtk::ScrolledWindow,
}

#[cfg(target_os = "linux")]
const GTK_FINISH_LIMIT: Duration = Duration::from_millis(500);

/// GTK destroys the window only on later main-loop turns; unpumped, the compositor keeps it mapped.
#[cfg(target_os = "linux")]
pub(super) fn finish_gtk_work() {
    let limit = Instant::now() + GTK_FINISH_LIMIT;
    while gtk::events_pending() && Instant::now() < limit {
        gtk::main_iteration_do(false);
    }
}

fn main_view<'a>(
    event_loop: &EventLoop<Msg>,
    nonces: &Nonces,
    start_url: &str,
) -> WebViewBuilder<'a> {
    let ipc = event_loop.create_proxy();
    let pages = event_loop.create_proxy();
    let links = RefCell::new(LinkLimiter::default());
    let start = start_url.to_owned();
    WebViewBuilder::new()
        .with_incognito(true)
        .with_background_color(BACKGROUND)
        .with_url(start_url)
        .with_navigation_handler(move |url| {
            let allowed = allowed_navigation(&url, &start);
            if !allowed {
                let host = url::Url::parse(&url)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_owned));
                tracing::warn!(
                    host = host.as_deref().unwrap_or("-"),
                    "blocked a navigation away from Discord"
                );
            }
            allowed
        })
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
pub(super) fn build_views(
    event_loop: &EventLoop<Msg>,
    window: &Window,
    nonces: &Nonces,
    start_url: &str,
) -> Result<Views, CliError> {
    use gtk::prelude::{BoxExt, OverlayExt, ScrolledWindowExt, WidgetExt};
    use tao::platform::unix::WindowExtUnix;
    use webkit2gtk::WebViewExt;
    use wry::{WebViewBuilderExtUnix, WebViewExtUnix};
    let vbox = window
        .default_vbox()
        .ok_or_else(|| CliError::BrowserLogin("no GTK container".into()))?;
    // An overlay keeps the bar out of the window's size request; WebKit asks for the full window height.
    let overlay = gtk::Overlay::new();
    vbox.pack_start(&overlay, true, true, 0);
    overlay.show();
    let status_bar = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    status_bar.set_policy(gtk::PolicyType::Never, gtk::PolicyType::External);
    status_bar.set_propagate_natural_height(false);
    status_bar.set_min_content_height(STATUS_HEIGHT);
    status_bar.set_max_content_height(STATUS_HEIGHT);
    status_bar.set_valign(gtk::Align::Start);
    status_bar.set_no_show_all(true);
    overlay.add_overlay(&status_bar);
    let status_proxy = event_loop.create_proxy();
    let status = WebViewBuilder::new()
        .with_incognito(true)
        .with_focused(false)
        .with_background_color(BACKGROUND)
        .with_html(STATUS_PAGE.replace("{nonce}", &nonces.app))
        .with_ipc_handler(move |request| {
            let _ = status_proxy.send_event(Msg::Status {
                uri: request.uri().to_string(),
                body: request.body().clone(),
            });
        })
        .build_gtk(&status_bar)
        .map_err(login_error)?;
    let main = main_view(event_loop, nonces, start_url)
        .build_gtk(&overlay)
        .map_err(login_error)?;
    let load_proxy = event_loop.create_proxy();
    main.webview().connect_load_failed(move |_, _, _, error| {
        if !genuine_load_failure(error) {
            return false;
        }
        let _ = load_proxy.send_event(Msg::LoadFailed);
        true
    });
    Ok(Views {
        main,
        _status: status,
        status_bar,
    })
}

#[cfg(not(target_os = "linux"))]
pub(super) fn build_views(
    event_loop: &EventLoop<Msg>,
    window: &Window,
    nonces: &Nonces,
    start_url: &str,
) -> Result<Views, CliError> {
    let main = main_view(event_loop, nonces, start_url)
        .build(window)
        .map_err(login_error)?;
    Ok(Views { main })
}

pub(super) fn set_window_visible(window: &Window, visible: bool) {
    #[cfg(target_os = "linux")]
    {
        use gtk::prelude::{GtkWindowExt, WidgetExt};
        use tao::platform::unix::WindowExtUnix;
        if visible {
            window.gtk_window().present();
        } else {
            window.gtk_window().hide();
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        window.set_visible(visible);
        // Ordering the window front does not activate the app, so it could stay behind the terminal.
        if visible {
            window.set_focus();
        }
    }
}

pub(super) const LOGIN_TITLE: &str = "D-Streamy: log in to Discord";

pub(super) fn set_title(window: &Window, screen: Screen) {
    let title = match screen {
        Screen::Discord { verifying: true } => "D-Streamy: verifying the login…",
        Screen::Discord { verifying: false } => LOGIN_TITLE,
        Screen::Recovery(_) => "D-Streamy: login not completed",
    };
    window.set_title(title);
}

pub(super) fn show(webview: &WebView, screen: Screen, session: &Session) {
    let loaded = match screen {
        Screen::Discord { .. } => webview.load_url(&session.start_url),
        Screen::Recovery(failure) => webview.load_html(&recovery_page(
            failure,
            &session.nonces.app,
            session.recovery,
        )),
    };
    if let Err(e) = loaded {
        tracing::warn!("showing the browser login page failed: {e}");
    }
}

/// macOS routes ⌘C/⌘V/⌘X/⌘A through the menu bar; without an Edit menu they do nothing.
#[cfg(target_os = "macos")]
pub(super) fn edit_menu() -> Result<muda::Menu, CliError> {
    use muda::{Menu, PredefinedMenuItem, Submenu};
    let app = Submenu::with_items("D-Streamy", true, &[&PredefinedMenuItem::quit(None)])
        .map_err(login_error)?;
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
    .map_err(login_error)?;
    let menu = Menu::with_items(&[&app, &edit]).map_err(login_error)?;
    menu.init_for_nsapp();
    Ok(menu)
}
