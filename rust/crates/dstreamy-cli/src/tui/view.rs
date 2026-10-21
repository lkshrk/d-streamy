use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Sparkline};

use super::model::{
    App, Connection, Field, HealthView, Live, Picker, Screen, Tone, channel_label, state_tone,
    target_label,
};

fn color(tone: Tone) -> Color {
    match tone {
        Tone::Good => Color::Green,
        Tone::Warn => Color::Yellow,
        Tone::Bad => Color::Red,
        Tone::Info => Color::Cyan,
    }
}

fn toned(text: impl Into<String>, tone: Tone) -> Span<'static> {
    Span::styled(text.into(), Style::new().fg(color(tone)))
}

pub fn draw(frame: &mut Frame, app: &App) {
    let [header, body, notice, hints] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    frame.render_widget(header_line(app), header);
    match app.screen {
        Screen::Guilds => picker(
            frame,
            body,
            "Guilds",
            &app.guilds,
            |g| {
                if g.available {
                    g.name.clone()
                } else {
                    format!("{} (unavailable)", g.name)
                }
            },
            loading_text(app),
        ),
        Screen::Channels => {
            let guild = app.guilds.current().map_or("", |g| g.name.as_str());
            picker(
                frame,
                body,
                &format!("Voice channels in {guild}"),
                &app.channels,
                channel_label,
                "loading channels…",
            );
        }
        Screen::Targets => picker(
            frame,
            body,
            "Capture target",
            &app.targets,
            target_label,
            "listing windows and displays…",
        ),
        Screen::Settings => settings(frame, body, app),
        Screen::Live => {
            if let Some(live) = &app.live {
                live_panel(frame, body, live);
            }
        }
    }
    if let Some(n) = &app.notice {
        frame.render_widget(Line::from(toned(n.text.clone(), n.tone)), notice);
    }
    frame.render_widget(
        Line::from(hints_text(app)).style(Style::new().add_modifier(Modifier::DIM)),
        hints,
    );
}

fn header_line(app: &App) -> Line<'static> {
    let account = match &app.connection {
        Connection::Connecting => toned("connecting to Discord…", Tone::Warn),
        Connection::Connected { user } => Span::raw(format!("logged in as {user}")),
        Connection::Failed(_) => toned("not connected", Tone::Bad),
    };
    Line::from(vec![
        Span::styled("dstreamy", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw("  ·  "),
        account,
    ])
}

fn loading_text(app: &App) -> &'static str {
    match app.connection {
        Connection::Failed(_) => "no guilds: not connected",
        _ => "loading guilds…",
    }
}

fn hints_text(app: &App) -> &'static str {
    match app.screen {
        Screen::Guilds => "↑↓ select · Enter open · q quit",
        Screen::Channels | Screen::Targets => "↑↓ select · Enter next · Esc back · q quit",
        Screen::Settings => "↑↓ field · ←→ change · 0-9 , crop · Enter go live · Esc back · q quit",
        Screen::Live => match &app.live {
            Some(live) if live.stopping => "stopping…",
            Some(live) if live.active() => "s stop · q stop and quit",
            _ => "Enter settings · q quit",
        },
    }
}

fn picker<T>(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    picker: &Picker<T>,
    label: impl Fn(&T) -> String,
    loading: &str,
) {
    let block = Block::bordered().title(format!(" {title} "));
    if picker.loading || picker.items.is_empty() {
        let text = if picker.loading {
            loading
        } else {
            "nothing to pick"
        };
        frame.render_widget(Paragraph::new(text).block(block), area);
        return;
    }
    let items: Vec<ListItem> = picker
        .items
        .iter()
        .map(|i| ListItem::new(label(i)))
        .collect();
    let list = List::new(items)
        .block(block)
        .highlight_symbol("› ")
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default().with_selected(Some(picker.selected));
    frame.render_stateful_widget(list, area, &mut state);
}

fn settings(frame: &mut Frame, area: Rect, app: &App) {
    let s = &app.settings;
    let mut lines: Vec<Line> = Field::ALL
        .iter()
        .map(|&field| {
            let focused = field == s.field;
            let value = s.value(field);
            let value = match (focused, field) {
                (true, Field::Crop) => format!("{}▏", s.crop_text),
                (true, _) => format!("◂ {value} ▸"),
                (false, _) => value,
            };
            let marker = if focused { "› " } else { "  " };
            let line = Line::from(format!("{marker}{:<14}{value}", field.label()));
            if focused {
                line.style(Style::new().add_modifier(Modifier::BOLD))
            } else {
                line
            }
        })
        .collect();
    let destination = match (app.channels.current(), app.targets.current()) {
        (Some(c), Some(t)) => format!("Enter goes live in {} with {}", c.name, t.title),
        _ => String::new(),
    };
    lines.push(Line::default());
    lines.push(Line::from(destination));
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Stream settings ")),
        area,
    );
}

fn live_panel(frame: &mut Frame, area: Rect, live: &Live) {
    let [info, health, log] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(12),
        Constraint::Length(8),
    ])
    .areas(area);
    frame.render_widget(
        Line::from(vec![
            toned(live.state.to_string(), state_tone(&live.state)),
            Span::raw(format!(
                "  ·  {} › {}  ·  {}",
                live.guild, live.channel, live.target
            )),
        ]),
        info,
    );
    health_panel(frame, health, live);
    let lines: Vec<Line> = live
        .log
        .iter()
        .map(|n| Line::from(toned(n.text.clone(), n.tone)))
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Events ")),
        log,
    );
}

fn health_panel(frame: &mut Frame, area: Rect, live: &Live) {
    let block = Block::bordered().title(" Health ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(snapshot) = &live.health else {
        frame.render_widget(
            Paragraph::new("waiting for the first health sample…"),
            inner,
        );
        return;
    };
    let view = HealthView::new(snapshot, live.target_fps);
    let [scores, labels, sparks, rows] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(0),
    ])
    .areas(inner);
    let mut spans = Vec::new();
    for score in [&view.video, &view.audio] {
        spans.push(Span::raw(format!("{} ", score.label)));
        spans.push(toned(
            format!("{:>3}% {:<4}", score.percent, score.level),
            score.tone,
        ));
        spans.push(Span::raw("    "));
    }
    frame.render_widget(Line::from(spans), scores);

    let halves = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]);
    let [fps_label, audio_label] = halves.areas(labels);
    let [fps_spark, audio_spark] = halves.areas(sparks);
    frame.render_widget(
        Line::from(format!("FPS, last {}s", view.fps_series.len())),
        fps_label,
    );
    frame.render_widget(Line::from("Audio frames/s"), audio_label);
    frame.render_widget(
        Sparkline::default()
            .data(view.fps_series.iter().copied())
            .max(view.fps_max)
            .style(Style::new().fg(color(view.video.tone))),
        fps_spark,
    );
    frame.render_widget(
        Sparkline::default()
            .data(view.audio_series.iter().copied())
            .max(view.audio_max)
            .style(Style::new().fg(color(view.audio.tone))),
        audio_spark,
    );
    let lines: Vec<Line> = view
        .rows
        .iter()
        .map(|(name, value)| Line::from(format!("{name:<10}{value}")))
        .collect();
    frame.render_widget(Paragraph::new(lines), rows);
}

#[cfg(test)]
mod tests {
    use dstreamy_core::{Config, StreamHealth, StreamMetrics};
    use dstreamy_discord::{HealthSnapshot, SessionEvent, SessionState, StopReason, StreamStats};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::tui::fixtures::{channel, guild, target};
    use crate::tui::model::{Key, Msg};

    fn render(app: &App) -> TestBackend {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        terminal.backend().clone()
    }

    fn press(app: &mut App, keys: &[Key]) {
        for key in keys {
            app.update(Msg::Key(*key));
        }
    }

    fn connected() -> App {
        let mut app = App::new(&Config::default());
        app.update(Msg::Connected {
            user: "mockuser".into(),
        });
        app.update(Msg::Guilds(vec![
            guild("g1", "Mock Guild", true),
            guild("g2", "Outage", false),
        ]));
        app.update(Msg::Targets {
            targets: vec![
                target("display:1", "Display 1"),
                target("test-pattern", "Test pattern"),
            ],
            error: None,
        });
        app
    }

    fn at_settings() -> App {
        let mut app = connected();
        press(&mut app, &[Key::Enter]);
        app.update(Msg::Channels {
            guild_id: "g1".into(),
            result: Ok(vec![channel("c1", "General"), channel("c2", "Gaming")]),
        });
        press(&mut app, &[Key::Enter, Key::Down, Key::Enter]);
        app
    }

    fn healthy(fps: u64, a_sent: u64) -> StreamMetrics {
        StreamMetrics {
            fps,
            gap_p95_ms: 34,
            gap_max_ms: 41,
            bitrate_kbps: 5_800,
            a_enc: 50,
            a_sent,
            ..StreamMetrics::default()
        }
    }

    fn streaming() -> App {
        let mut app = at_settings();
        press(&mut app, &[Key::Enter]);
        app.update(Msg::Session(SessionEvent::State {
            state: SessionState::Streaming,
        }));
        let history: Vec<StreamMetrics> = (0..30)
            .map(|i| healthy(if i % 7 == 3 { 20 } else { 30 }, 50 - i % 2))
            .collect();
        app.update(Msg::Session(SessionEvent::Health(HealthSnapshot {
            metrics: history[history.len() - 1],
            health: StreamHealth::compute(&history, 30),
            stats: StreamStats {
                video_frames_sent: 900,
                video_frames_dropped: 3,
                viewer_keyframe_requests: 1,
                ..StreamStats::default()
            },
        })));
        app
    }

    #[test]
    fn connecting_screen() {
        insta::assert_snapshot!(render(&App::new(&Config::default())));
    }

    #[test]
    fn guilds_screen() {
        let mut app = connected();
        press(&mut app, &[Key::Down, Key::Enter]);
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn channels_screen() {
        let mut app = connected();
        press(&mut app, &[Key::Enter]);
        app.update(Msg::Channels {
            guild_id: "g1".into(),
            result: Ok(vec![channel("c1", "General"), channel("c2", "Gaming")]),
        });
        press(&mut app, &[Key::Down]);
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn targets_screen() {
        let mut app = at_settings();
        press(&mut app, &[Key::Esc]);
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn settings_screen() {
        let mut app = at_settings();
        press(&mut app, &[Key::Down, Key::Down, Key::Right]);
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn settings_screen_editing_crop() {
        let mut app = at_settings();
        press(&mut app, &[Key::BackTab, Key::Char('1'), Key::Char(',')]);
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn live_screen_waiting_for_health() {
        let mut app = at_settings();
        press(&mut app, &[Key::Enter]);
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn live_screen_with_health() {
        insta::assert_snapshot!(render(&streaming()));
    }

    #[test]
    fn live_screen_after_failure() {
        let mut app = streaming();
        app.update(Msg::Session(SessionEvent::Error {
            message: "voice connection closed".into(),
            fatal: false,
        }));
        app.update(Msg::Session(SessionEvent::State {
            state: SessionState::Stopped {
                reason: StopReason::GaveUp { attempts: 10 },
            },
        }));
        insta::assert_snapshot!(render(&app));
    }

    #[test]
    fn states_and_levels_are_colored() {
        fn fg(backend: &TestBackend, needle: &str) -> Color {
            let buffer = backend.buffer();
            let width = buffer.area.width as usize;
            let text: String = buffer.content.iter().map(|c| c.symbol()).collect();
            let at = text
                .find(needle)
                .unwrap_or_else(|| panic!("{needle} not on screen"));
            let index = text[..at].chars().count();
            buffer[((index % width) as u16, (index / width) as u16)].fg
        }
        let mut app = streaming();
        let backend = render(&app);
        assert_eq!(fg(&backend, "streaming"), Color::Green);
        assert_eq!(fg(&backend, " 87% good"), Color::Green);
        app.update(Msg::Session(SessionEvent::State {
            state: SessionState::Stopped {
                reason: StopReason::GatewayClosed,
            },
        }));
        assert_eq!(fg(&render(&app), "stopped:"), Color::Red);
    }
}
