use super::app::App;
use super::theme::Theme;
use crate::event::{Level, LogEvent, fmt_duration};
use crate::state::Status;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

pub const SMALL: u16 = 5;
pub const BIG: u16 = 14;
const MAX_WIDTH: u16 = 92;
const LEFT_WIDTH: u16 = 50;
const LABEL: usize = 9;

pub fn draw(f: &mut Frame, app: &App, theme: &Theme) {
    let mut area = f.area();
    area.width = area.width.min(MAX_WIDTH);
    f.render_widget(Clear, area);
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme.dim()),
        area,
    );
    draw_title(f, area, app, theme);
    let [_, body, _] =
        Layout::vertical([Constraint::Length(1), Constraint::Fill(1), Constraint::Length(1)])
            .horizontal_margin(2)
            .areas(area);
    let [panel, more] = Layout::vertical([Constraint::Length(3), Constraint::Fill(1)]).areas(body);
    let (left, right) = if panel.width > 70 {
        let [l, r] = Layout::horizontal([Constraint::Length(LEFT_WIDTH), Constraint::Fill(1)])
            .spacing(2)
            .areas(panel);
        (l, Some(r))
    } else {
        (panel, None)
    };
    draw_left(f, left, app, theme);
    if let Some(r) = right {
        draw_right(f, r, app, theme);
    }
    if app.more {
        draw_more(f, more, app, theme);
    }
}

fn draw_title(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let version = format!(" yawn {} ", app.info.env.version);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(version, theme.dim()))),
        Rect { x: area.x + 2, width: area.width.saturating_sub(4), height: 1, ..area },
    );
    if area.width < 50 {
        return;
    }
    let more = if app.more { theme.warn() } else { theme.dim() };
    let line = Line::from(vec![
        Span::styled(" Ctrl-C:quit  r:reload  ", theme.dim()),
        Span::styled("/:more", more),
        Span::styled(" ", theme.dim()),
    ]);
    f.render_widget(
        Paragraph::new(line).right_aligned(),
        Rect { x: area.x + 1, width: area.width.saturating_sub(3), height: 1, ..area },
    );
}

fn label(text: &str, theme: &Theme) -> Span<'static> {
    Span::styled(format!("{text:<LABEL$}"), theme.dim())
}

fn clip(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let mut out = Vec::with_capacity(spans.len());
    let mut used = 0;
    for s in spans {
        let w = s.width();
        if used + w <= width {
            used += w;
            out.push(s);
        } else {
            let keep = width.saturating_sub(used + 1);
            let text: String = s.content.chars().take(keep).collect();
            out.push(Span::styled(format!("{text}…"), s.style));
            break;
        }
    }
    Line::from(out)
}

fn gauge(
    name: &str,
    lane: Option<(u32, u32)>,
    active: bool,
    app: &App,
    theme: &Theme,
    width: usize,
) -> Line<'static> {
    let s = &app.state;
    let (ratio, style, suffix) = match s.status() {
        Status::Building if !active => (1.0, theme.dim(), String::new()),
        Status::Building => match lane {
            Some((done, total)) if total > 0 => {
                (done as f64 / total as f64, theme.accent(), format!(" {done}/{total}"))
            }
            _ => (0.0, theme.accent(), String::new()),
        },
        Status::Ok => (1.0, theme.ok(), String::new()),
        Status::Failed => (1.0, theme.error(), String::new()),
        Status::Idle => (0.0, theme.dim(), String::new()),
    };
    let bar = width.saturating_sub(LABEL + suffix.chars().count()).max(4);
    let filled = ((ratio.clamp(0.0, 1.0) * bar as f64).round() as usize).min(bar);
    Line::from(vec![
        label(name, theme),
        Span::styled("━".repeat(filled), style),
        Span::styled("─".repeat(bar - filled), theme.dim()),
        Span::styled(suffix, theme.dim()),
    ])
}

fn draw_left(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let rows = Layout::vertical([Constraint::Length(1); 3]).split(area);
    let s = &app.state;
    let w = area.width as usize;

    for (row, (name, key)) in rows.iter().zip([("Client:", "client"), ("Server:", "server")]) {
        let running = s.running();
        let active = running.is_none_or(|r| r.target.contains(key));
        let lane = running.and_then(|r| r.lane(key));
        f.render_widget(Paragraph::new(gauge(name, lane, active, app, theme, w)), *row);
    }

    let mut status = vec![label("Status:", theme)];
    match (s.running(), s.status()) {
        (Some(r), _) if r.total > 0 => {
            status.push(Span::styled("Compiling ", theme.warn()));
            status.push(Span::raw(format!("{}/{} ", r.done, r.total)));
            status.push(Span::styled(r.label.clone(), theme.dim()));
        }
        (Some(r), _) => {
            let text =
                if r.label.is_empty() { "Starting build".to_string() } else { r.label.clone() };
            status.push(Span::styled(text, theme.warn()));
        }
        (None, Status::Ok) => status.push(Span::styled("Ready", theme.ok())),
        (None, Status::Failed) => status.push(Span::styled("Failed", theme.error())),
        (None, _) if s.ended() => status.push(Span::styled("Session ended", theme.dim())),
        (None, _) => status.push(Span::styled("Waiting for the first build", theme.dim())),
    }
    if s.running().is_none()
        && let Some(l) = s.last_build()
    {
        status.push(Span::styled(format!(" {}", fmt_duration(l.duration)), theme.dim()));
        if l.errors > 0 {
            status.push(Span::styled(
                format!(" · {}", count(l.errors as u64, "error")),
                theme.error(),
            ));
        }
        if l.warnings > 0 {
            status.push(Span::styled(
                format!(" · {}", count(l.warnings as u64, "warning")),
                theme.warn(),
            ));
        }
    }
    f.render_widget(Paragraph::new(clip(status, w)), rows[2]);
}

fn count(n: u64, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

fn project(app: &App) -> String {
    app.info.title.strip_prefix("dev · ").unwrap_or(&app.info.title).to_string()
}

fn draw_right(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let rows = Layout::vertical([Constraint::Length(1); 3]).split(area);
    let w = area.width as usize;
    let s = &app.state;
    f.render_widget(
        Paragraph::new(clip(
            vec![label("Project:", theme), Span::styled(project(app), theme.warn())],
            w,
        )),
        rows[0],
    );
    let address = match app.address() {
        Some(url) => Span::styled(url, theme.accent()),
        None => {
            Span::styled(s.chip("server").unwrap_or("no server address").to_string(), theme.dim())
        }
    };
    f.render_widget(Paragraph::new(clip(vec![label("Address:", theme), address], w)), rows[1]);
    let client = Span::styled(s.chip("client").unwrap_or("-").to_string(), theme.base());
    f.render_widget(Paragraph::new(clip(vec![label("Wasm:", theme), client], w)), rows[2]);
}

fn draw_more(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let [left, right] = Layout::horizontal([Constraint::Length(LEFT_WIDTH), Constraint::Fill(1)])
        .spacing(2)
        .areas(area);
    let env = &app.info.env;
    let info = [
        ("yawn version:", env.version.to_string()),
        ("rustc:", app.rustc.clone().unwrap_or_else(|| "unknown".into())),
        ("Cargo project:", env.cargo_project.clone().unwrap_or_else(|| "none found".into())),
        ("Directory:", env.cwd.clone()),
    ];
    for (i, (k, v)) in info.iter().enumerate() {
        let row = Rect { y: left.y + 1 + i as u16, height: 1, ..left };
        if row.y >= left.y + left.height {
            break;
        }
        let spans =
            vec![Span::styled(format!("{k} "), theme.dim()), Span::styled(v.clone(), theme.warn())];
        f.render_widget(Paragraph::new(clip(spans, left.width as usize)), row);
    }
    let commands = [
        ("r", "rebuild and reload the page"),
        ("o", "open the app in the browser"),
        ("v", "toggle debug logs"),
        ("t", "toggle trace logs"),
        ("c", "clear the screen"),
        ("/", "toggle more commands"),
        ("Ctrl-C", "quit"),
    ];
    for (i, (k, d)) in commands.iter().enumerate() {
        let row = Rect { y: right.y + 1 + i as u16, height: 1, ..right };
        if row.y >= right.y + right.height {
            break;
        }
        let line = Line::from(vec![Span::raw(format!("{k}: ")), Span::styled(*d, theme.dim())]);
        f.render_widget(Paragraph::new(line), row);
    }
}

/// The lines printed above the panel for one log entry, wrapped to `width`
pub fn log_lines(e: &LogEvent, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let style = if e.level == Level::Error { theme.strong() } else { theme.base() };
    let mut spans = vec![
        Span::styled(format!("{:>8.3}s ", e.at.as_secs_f64()), theme.dim()),
        Span::styled(format!("[{}] ", e.source), theme.level(e.level)),
        Span::styled(e.message.clone(), style),
    ];
    if let Some(loc) = &e.location {
        spans.push(Span::styled(format!("  {loc}"), theme.dim()));
    }
    if let Some(c) = &e.code {
        spans.push(Span::styled(format!(" [{c}]"), theme.dim()));
    }
    wrap(Line::from(spans), width)
}

/// Breaks a line at `width` columns, the way a terminal wraps a plain `println!`
pub fn wrap(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut col = 0;
    for span in line.spans {
        let mut buf = String::new();
        for ch in span.content.chars() {
            let w = Span::raw(ch.to_string()).width();
            if col + w > width && col > 0 {
                if !buf.is_empty() {
                    current.push(Span::styled(std::mem::take(&mut buf), span.style));
                }
                out.push(Line::from(std::mem::take(&mut current)));
                col = 0;
            }
            buf.push(ch);
            col += w;
        }
        if !buf.is_empty() {
            current.push(Span::styled(buf, span.style));
        }
    }
    out.push(Line::from(current));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{BuildEvent, Event, Level, Location};
    use crate::tui::app::tests::{app, key};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyCode;
    use std::time::Duration;

    fn render(app: &App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        let theme = Theme { color: false };
        t.draw(|f| draw(f, app, &theme)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn build(a: &mut App, ev: BuildEvent) {
        a.on_event(Event::Build(ev));
    }

    #[test]
    fn idle_panel_waits_for_the_first_build() {
        let a = app();
        let s = render(&a, 100, SMALL);
        assert!(s.contains("yawn 0.0.0"), "{s}");
        assert!(s.contains("Waiting for the first build"), "{s}");
        assert!(s.contains("Ctrl-C:quit") && s.contains("/:more"));
        assert!(s.contains("╭") && s.contains("╯"));
    }

    #[test]
    fn building_shows_progress_and_the_crate() {
        let mut a = app();
        build(&mut a, BuildEvent::Started { at: Duration::ZERO, id: 1, target: "app".into() });
        build(
            &mut a,
            BuildEvent::Progress {
                at: Duration::ZERO,
                id: 1,
                done: 12,
                total: 40,
                label: "yawn-web".into(),
            },
        );
        let s = render(&a, 100, SMALL);
        assert!(s.contains("Compiling 12/40 yawn-web"), "{s}");
        assert!(s.contains("Client:") && s.contains("Server:"), "{s}");
        assert!(s.contains("━") && s.contains("─"));
    }

    #[test]
    fn each_lane_has_its_own_bar() {
        let mut a = app();
        build(
            &mut a,
            BuildEvent::Started { at: Duration::ZERO, id: 1, target: "client + server".into() },
        );
        for (lane, done) in [("client", 10), ("server", 2)] {
            build(
                &mut a,
                BuildEvent::Lane { at: Duration::ZERO, id: 1, lane: lane.into(), done, total: 10 },
            );
        }
        let s = render(&a, 100, SMALL);
        let row = |name: &str| s.lines().find(|l| l.contains(name)).unwrap().to_string();
        assert!(row("Client:").contains("10/10") && row("Server:").contains("2/10"), "{s}");
    }

    #[test]
    fn finished_build_shows_ready_or_the_issues() {
        let mut a = app();
        let finish = |ok, errors| BuildEvent::Finished {
            at: Duration::from_secs(2),
            id: 1,
            target: "app".into(),
            ok,
            duration: Duration::from_millis(1500),
            warnings: 1,
            errors,
        };
        build(&mut a, finish(true, 0));
        let s = render(&a, 100, SMALL);
        assert!(s.contains("Ready"), "{s}");
        assert!(s.contains("1 warning"), "{s}");
        build(&mut a, finish(false, 2));
        let s = render(&a, 100, SMALL);
        assert!(s.contains("Failed") && s.contains("2 errors"), "{s}");
    }

    #[test]
    fn right_column_shows_project_and_address() {
        let mut a = app();
        a.on_event(Event::Info {
            at: Duration::ZERO,
            key: "server".into(),
            value: "127.0.0.1:3000".into(),
        });
        let s = render(&a, 100, SMALL);
        assert!(s.contains("Project:") && s.contains("demo"), "{s}");
        assert!(s.contains("http://127.0.0.1:3000"), "{s}");
        assert!(!render(&a, 60, SMALL).contains("Project:"), "right column needs room");
    }

    #[test]
    fn command_list_opens_under_the_panel() {
        let mut a = app();
        assert!(!render(&a, 100, BIG).contains("toggle debug logs"));
        a.on_key(key(KeyCode::Char('/')));
        a.rustc = Some("rustc 1.90.0".into());
        let s = render(&a, 100, BIG);
        for want in [
            "rebuild and reload",
            "open the app",
            "toggle debug logs",
            "clear the screen",
            "rustc 1.90.0",
            "yawn version:",
        ] {
            assert!(s.contains(want), "{want}: {s}");
        }
    }

    #[test]
    fn log_line_has_time_source_message_and_location() {
        let e = LogEvent::new(Duration::from_millis(1234), Level::Error, "rsx", "type mismatch")
            .at_location(Location::new("src/a.rsx", 12, 5))
            .with_code("E0308");
        let text = |lines: Vec<Line>| lines.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        assert_eq!(
            text(log_lines(&e, &Theme { color: false }, 100)),
            ["   1.234s [rsx] type mismatch  src/a.rsx:12:5 [E0308]"]
        );
    }

    #[test]
    fn long_lines_wrap() {
        let theme = Theme { color: false };
        let e = LogEvent::new(Duration::ZERO, Level::Info, "s", "x".repeat(100));
        assert_eq!(log_lines(&e, &theme, 80).len(), 2);
        let wrapped = wrap(Line::from("abcdefghij"), 4);
        assert_eq!(
            wrapped.iter().map(|l| l.to_string()).collect::<Vec<_>>(),
            ["abcd", "efgh", "ij"]
        );
        assert_eq!(wrap(Line::from(""), 4).len(), 1);
    }
}
