//! One terminal-cell renderer powers the live UI, tests, and deterministic SVG exports.
use crate::{
    app::{App, Focus, Sort},
    insights::HeadroomLevel,
    model::{ProcessKey, SessionSnapshot},
};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::Widget,
};
use std::{collections::HashMap, fs, io, path::Path};

const BG: Color = Color::Rgb(10, 15, 24);
const PANEL: Color = Color::Rgb(16, 24, 36);
const SELECT: Color = Color::Rgb(24, 38, 48);
const LINE: Color = Color::Rgb(36, 50, 69);
const TEXT: Color = Color::Rgb(226, 233, 242);
const DIM: Color = Color::Rgb(119, 139, 161);
const FAINT: Color = Color::Rgb(49, 65, 85);
const MINT: Color = Color::Rgb(142, 232, 193);
const LILAC: Color = Color::Rgb(183, 165, 247);
const BLUE: Color = Color::Rgb(127, 196, 240);
const PEACH: Color = Color::Rgb(248, 185, 139);
const RED: Color = Color::Rgb(244, 134, 158);
#[derive(Clone, Copy, Debug)]
pub enum HitTarget {
    Session(ProcessKey),
    Process(ProcessKey),
}

const COLORS: [Color; 4] = [MINT, LILAC, BLUE, PEACH];

pub fn bytes(value: u64) -> String {
    if value >= 1 << 30 {
        format!("{:.1} GiB", value as f64 / (1_u64 << 30) as f64)
    } else if value >= 1 << 20 {
        format!("{:.0} MiB", value as f64 / (1_u64 << 20) as f64)
    } else {
        format!("{} KiB", value / 1024)
    }
}
fn cpu(value: Option<f32>) -> String {
    value
        .map(|v| format!("{v:.0}%"))
        .unwrap_or_else(|| "warming".into())
}
fn color(session: &SessionSnapshot) -> Color {
    match session.agent.as_deref() {
        Some("claude") => MINT,
        Some("codex") => LILAC,
        Some("aider") => BLUE,
        Some("opencode") => PEACH,
        _ => COLORS[session.key.pid as usize % COLORS.len()],
    }
}

struct Surface<'a> {
    buf: &'a mut Buffer,
    area: Rect,
}
impl Surface<'_> {
    fn text(&mut self, x: u16, y: u16, text: impl AsRef<str>, fg: Color) {
        self.styled(x, y, text, Style::default().fg(fg));
    }
    fn bold(&mut self, x: u16, y: u16, text: impl AsRef<str>, fg: Color) {
        self.styled(
            x,
            y,
            text,
            Style::default().fg(fg).add_modifier(Modifier::BOLD),
        );
    }
    fn styled(&mut self, x: u16, y: u16, text: impl AsRef<str>, style: Style) {
        if y < self.area.height && x < self.area.width {
            self.buf
                .set_stringn(x, y, text.as_ref(), (self.area.width - x) as usize, style);
        }
    }
    fn fill(&mut self, rect: Rect, bg: Color) {
        for y in rect.y..rect.bottom().min(self.area.height) {
            for x in rect.x..rect.right().min(self.area.width) {
                self.buf[(x, y)]
                    .set_char(' ')
                    .set_style(Style::reset().bg(bg));
            }
        }
    }
    fn rule(&mut self, x: u16, y: u16, width: u16) {
        self.text(x, y, "─".repeat(width as usize), LINE);
    }
    fn spark(&mut self, x: u16, y: u16, width: u16, values: &[f64], max: f64, fg: Color) {
        let glyphs = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        if width == 0 {
            return;
        }
        let history = &values[values.len().saturating_sub(width as usize)..];
        self.text(x, y, "▁".repeat(width as usize), FAINT);
        let start = x + width.saturating_sub(history.len() as u16);
        for (i, value) in history.iter().enumerate() {
            let idx = ((value / max.max(1.0)).clamp(0.0, 1.0) * 7.0).round() as usize;
            self.text(start + i as u16, y, glyphs[idx].to_string(), fg);
        }
    }
    fn bar(&mut self, x: u16, y: u16, width: u16, fraction: f64, fg: Color) {
        let filled = (width as f64 * fraction.clamp(0.0, 1.0)).round() as u16;
        self.text(x, y, "━".repeat(width as usize), FAINT);
        self.text(x, y, "━".repeat(filled as usize), fg);
    }
}

pub fn draw(frame: &mut Frame<'_>, app: &App) -> Vec<(Rect, HitTarget)> {
    let mut hitboxes = Vec::new();
    frame.render_widget(
        Dashboard {
            app,
            hitboxes: &mut hitboxes,
        },
        frame.area(),
    );
    hitboxes
}
struct Dashboard<'a> {
    app: &'a App,
    hitboxes: &'a mut Vec<(Rect, HitTarget)>,
}
impl Widget for Dashboard<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        buf.set_style(area, Style::default().bg(BG).fg(TEXT));
        let mut s = Surface { buf, area };
        if area.width < 72 || area.height < 22 {
            compact(&mut s, self.app, self.hitboxes);
            return;
        }
        let app = self.app;
        let width = area.width;
        let height = area.height;
        s.bold(3, 1, "◈ lattice", MINT);
        if app.editing_search || !app.query.is_empty() {
            let text = format!(
                "/ {}{}",
                app.query,
                if app.editing_search { "▏" } else { "" }
            );
            s.text(
                18,
                1,
                text.chars()
                    .take(width.saturating_sub(47) as usize)
                    .collect::<String>(),
                MINT,
            );
        } else if width >= 96 {
            s.text(
                18,
                1,
                if app.snapshot.all_processes {
                    "YOUR PROCESS FAMILIES"
                } else {
                    "A LITTLE MORE HEADROOM."
                },
                DIM,
            );
        }
        let mode = if app.resuming {
            "◌ SYNCING"
        } else if app.stale() {
            "! STALE"
        } else if app.paused {
            "Ⅱ PAUSED"
        } else if app.snapshot.demo {
            "DEMO"
        } else {
            "LIVE"
        };
        s.bold(
            width.saturating_sub(25),
            1,
            if matches!(mode, "DEMO" | "LIVE") {
                format!("{} {mode}", app.sample_marker())
            } else {
                mode.to_string()
            },
            if app.paused { PEACH } else { MINT },
        );
        s.text(width.saturating_sub(14), 1, app.scope_label(), DIM);
        s.rule(3, 3, width - 6);
        metrics(&mut s, app);
        let footer_y = height - 2;
        let details_h = if height >= 34 { 9 } else { 4 };
        let body_y = if height >= 28 { 11 } else { 9 };
        let body_bottom = height.saturating_sub(details_h + 4);
        let left_w: u16 = if width >= 120 { 32 } else { 26 };
        let map_x = left_w + 6;
        let body_h = body_bottom.saturating_sub(body_y).max(3);
        s.bold(3, body_y - 1, "SESSIONS", DIM);
        s.text(
            13,
            body_y - 1,
            if app.query.is_empty() {
                format!("{:02}", app.sessions().len())
            } else {
                format!("{}/{}", app.sessions().len(), app.snapshot.sessions.len())
            },
            MINT,
        );
        s.text(
            left_w.saturating_sub(7),
            body_y - 1,
            match app.sort {
                Sort::Memory => "↓ RSS",
                Sort::Cpu => "↓ CPU",
                Sort::Growth => "↓ GROW",
            },
            DIM,
        );
        let map_title = if app.focused || width < 120 || body_h < 17 {
            "FOCUS / PROCESS MAP"
        } else {
            "CONSTELLATION / PROCESS MAP"
        };
        s.bold(map_x, body_y - 1, map_title, DIM);
        if width >= 132 {
            s.text(width - 30, body_y - 1, "lines = parent → child", DIM);
        }
        session_list(
            &mut s,
            app,
            Rect::new(3, body_y + 1, left_w, body_h.saturating_sub(1)),
            self.hitboxes,
        );
        let map_area = Rect::new(
            map_x,
            body_y + 1,
            width.saturating_sub(map_x + 3),
            body_h.saturating_sub(1),
        );
        if app.focused {
            focus_view(&mut s, app, map_area, self.hitboxes);
        } else {
            maps(&mut s, app, map_area, self.hitboxes);
        }
        details(&mut s, app, body_bottom + 2, details_h);
        s.rule(3, footer_y - 1, width - 6);
        if width >= 110 {
            s.text(
                3,
                footer_y,
                if app.focused { "↑↓ move  ←→ fold  tab pane  / find  a scope  space pause  esc back  ? help  q quit" } else { "↑↓ select  ↵ inspect  / find  * pin  s sort  a scope  space pause  ? help  q quit" },
                DIM,
            );
            if width >= 132 {
                s.text(width - 23, footer_y, "LOCAL · READ ONLY", MINT);
            }
        } else {
            s.text(
                3,
                footer_y,
                if width < 72 {
                    "↑↓ move  ↵ inspect  a scope  ? help  q quit"
                } else if app.focused {
                    "↑↓ move  ←→ fold  tab pane  a scope  ? help  q quit"
                } else {
                    "↑↓ select  ↵ inspect  / find  a scope  ? help  q quit"
                },
                DIM,
            );
        }
        if app.help || app.tour {
            guide(&mut s, app);
        }
    }
}

fn compact(s: &mut Surface<'_>, app: &App, hitboxes: &mut Vec<(Rect, HitTarget)>) {
    let w = s.area.width;
    let h = s.area.height;
    s.bold(1, 0, "◈ lattice", MINT);
    if w > 23 {
        s.text(
            w - 13,
            0,
            if app.resuming {
                "SYNCING"
            } else if app.stale() {
                "STALE"
            } else if app.paused {
                "PAUSED"
            } else if app.snapshot.demo {
                "DEMO"
            } else {
                "LIVE"
            },
            MINT,
        );
    }
    if h > 2 && (app.editing_search || !app.query.is_empty()) {
        s.text(
            1,
            1,
            format!(
                "/ {}{}",
                app.query,
                if app.editing_search { "▏" } else { "" }
            ),
            MINT,
        );
    }
    if h > 3 {
        s.text(
            1,
            2,
            format!(
                "{} available · CPU {}",
                if app.snapshot.system.memory_total == 0 {
                    "unknown".into()
                } else {
                    bytes(app.snapshot.system.memory_available)
                },
                cpu(app.snapshot.system.cpu_percent)
            ),
            TEXT,
        );
    }
    if h > 4 {
        if app.sampler_failed {
            s.text(1, 3, "Sampler stopped. Values are stale.", RED);
        } else if app.stale() {
            s.text(1, 3, "Waiting for sampler. Data stale.", PEACH);
        } else if app.snapshot.truncated {
            s.text(1, 3, "Scan limit reached. Partial data.", PEACH);
        } else if app.insights.headroom().level == HeadroomLevel::Critical {
            s.text(1, 3, "LOW HEADROOM · review workloads", RED);
        }
    }
    if h > 5 {
        if app.focused {
            let identity = app
                .selected
                .map(|key| {
                    format!(
                        " · {}#{}",
                        if app.pins.contains(&key) { "*" } else { "" },
                        key.pid
                    )
                })
                .unwrap_or_default();
            s.text(1, 4, format!("{}{identity}", app.scope_label()), DIM);
            process_table(
                s,
                app,
                Rect::new(1, 5, w.saturating_sub(2), h.saturating_sub(8)),
                hitboxes,
            );
        } else {
            s.text(1, 4, format!("{} · RSS", app.scope_label()), DIM);
            let list = app.sessions();
            if list.is_empty() {
                s.text(1, 5, empty_title(app), DIM);
            }
            let visible = h.saturating_sub(8).max(1) as usize;
            let selected_index = list
                .iter()
                .position(|s| Some(s.key) == app.selected)
                .unwrap_or(0);
            let start = selected_index.saturating_sub(visible - 1);
            for (i, session) in list.into_iter().skip(start).take(visible).enumerate() {
                let marker = if Some(session.key) == app.selected {
                    "›"
                } else {
                    " "
                };
                s.text(
                    1,
                    5 + i as u16,
                    format!(
                        "{marker}{} {} #{} {}",
                        if app.pins.contains(&session.key) {
                            "*"
                        } else {
                            " "
                        },
                        session
                            .label
                            .chars()
                            .take((w as usize).saturating_sub(
                                bytes(session.rss_bytes()).chars().count()
                                    + session.key.pid.to_string().len()
                                    + 7
                            ))
                            .collect::<String>(),
                        session.key.pid,
                        bytes(session.rss_bytes())
                    ),
                    color(session),
                );
            }
        }
    }
    if h > 1 {
        s.text(1, h - 1, "↑↓ move  a scope  ? help  q quit", DIM);
    }
    if app.help || app.tour {
        guide(s, app);
    }
}

fn metrics(s: &mut Surface<'_>, app: &App) {
    let w = (s.area.width - 8) / 3;
    let y = if s.area.height >= 28 { 5 } else { 4 };
    let system = &app.snapshot.system;
    let used_fraction = system.memory_percent() / 100.0;
    let available_fraction = if system.memory_total == 0 {
        0.0
    } else {
        1.0 - used_fraction
    };
    let level = app.insights.headroom().level;
    let pressure_color = match level {
        HeadroomLevel::Critical => RED,
        HeadroomLevel::Warning => PEACH,
        HeadroomLevel::Unknown => DIM,
        HeadroomLevel::Healthy => MINT,
    };
    let x1 = 3;
    let x2 = 3 + w + 1;
    let x3 = 3 + (w + 1) * 2;
    s.text(x1, y, "MEMORY HEADROOM", DIM);
    s.bold(
        x1,
        y + 1,
        if system.memory_total == 0 {
            "unavailable".into()
        } else {
            bytes(system.memory_available)
        },
        pressure_color,
    );
    if w >= 38 {
        s.text(
            x1 + 15,
            y + 1,
            format!("/ {}", bytes(system.memory_total)),
            DIM,
        );
    }
    s.bar(
        x1,
        y + 2,
        w.saturating_sub(3),
        available_fraction,
        pressure_color,
    );
    s.text(x2, y, "SYSTEM CPU", DIM);
    s.bold(x2, y + 1, cpu(system.cpu_percent), LILAC);
    if w >= 25 {
        s.text(
            x2 + 11,
            y + 1,
            format!("{} logical cores", system.cpu_count),
            DIM,
        );
    }
    let history: Vec<f64> = system
        .cpu_history
        .iter()
        .map(|v| v.unwrap_or(0.0) as f64)
        .collect();
    s.spark(x2, y + 2, w.saturating_sub(3), &history, 100.0, LILAC);
    s.text(
        x3,
        y,
        if w >= 30 {
            "SCOPE TOTAL · RSS"
        } else {
            "SCOPE · RSS"
        },
        DIM,
    );
    s.bold(
        x3,
        y + 1,
        bytes(app.snapshot.sessions.iter().map(|s| s.rss_bytes()).sum()),
        BLUE,
    );
    if w >= 35 {
        s.text(
            x3 + 14,
            y + 1,
            format!(
                "{} processes",
                app.snapshot
                    .sessions
                    .iter()
                    .map(|s| s.process_count())
                    .sum::<usize>()
            ),
            DIM,
        );
    }
    if app.sampler_failed {
        s.text(x3, y + 2, "Sampler stopped · data is stale", RED);
    } else if app.stale() {
        s.text(x3, y + 2, "Waiting for sampler · data stale", PEACH);
    } else if app.snapshot.truncated {
        s.text(x3, y + 2, "Scan limit reached · partial data", PEACH);
    } else if system.memory_total == 0 {
        s.text(x3, y + 2, "Memory data unavailable", PEACH);
    } else if level == HeadroomLevel::Critical {
        s.text(x3, y + 2, "LOW HEADROOM · review workloads", RED);
    } else if level == HeadroomLevel::Warning {
        s.text(x3, y + 2, "LIMITED HEADROOM · stay aware", PEACH);
    } else {
        s.text(x3, y + 2, "room to breathe", MINT);
    }
    if s.area.height >= 34 {
        s.text(x1, y + 3, format!("{} · heuristic", level.label()), DIM);
        s.text(x2, y + 3, "system-wide · history 60 samples", DIM);
        s.text(
            x3,
            y + 3,
            format!("swap {} · total ignores search", bytes(system.swap_used)),
            FAINT,
        );
    }
}

fn session_list(s: &mut Surface<'_>, app: &App, area: Rect, hitboxes: &mut Vec<(Rect, HitTarget)>) {
    let sessions = app.sessions();
    if sessions.is_empty() {
        s.text(area.x, area.y + 1, empty_title(app), TEXT);
        for (i, line) in wrap_line(empty_hint(app), area.width as usize)
            .iter()
            .enumerate()
        {
            if i as u16 + 3 < area.height {
                s.text(area.x, area.y + 3 + i as u16, line, DIM);
            }
        }
        return;
    }
    let row_h = if area.height >= 16 { 4 } else { 3 };
    let visible = (area.height / row_h).max(1) as usize;
    let selected_index = sessions
        .iter()
        .position(|s| Some(s.key) == app.selected)
        .unwrap_or(0);
    let start = selected_index.saturating_sub(visible - 1);
    for (i, session) in sessions.iter().skip(start).take(visible).enumerate() {
        let y = area.y + i as u16 * row_h;
        let active = Some(session.key) == app.selected;
        let c = color(session);
        let rect = Rect::new(area.x, y, area.width, row_h.saturating_sub(1));
        s.fill(rect, if active { SELECT } else { PANEL });
        if active {
            for yy in rect.y..rect.bottom() {
                s.text(area.x, yy, "▎", c);
            }
        }
        s.bold(
            area.x + 2,
            y,
            format!(
                "{} {}",
                if app.pins.contains(&session.key) {
                    "*"
                } else if active {
                    "◉"
                } else {
                    "○"
                },
                session
                    .label
                    .chars()
                    .take(area.width.saturating_sub(15) as usize)
                    .collect::<String>()
            ),
            c,
        );
        s.text(
            area.x + area.width.saturating_sub(10),
            y,
            bytes(session.rss_bytes()),
            TEXT,
        );
        s.text(
            area.x + 4,
            y + 1,
            format!(
                "#{} · {} · {}p",
                session.key.pid,
                cpu(session.cpu_percent()),
                session.process_count()
            )
            .chars()
            .take(area.width.saturating_sub(4) as usize)
            .collect::<String>(),
            DIM,
        );
        if row_h >= 4 {
            let history: Vec<f64> = session.history.iter().map(|v| *v as f64).collect();
            let low = history.iter().copied().fold(f64::INFINITY, f64::min);
            let high = history.iter().copied().fold(0.0, f64::max);
            let trend: Vec<_> = history.iter().map(|v| v - low).collect();
            s.spark(
                area.x + 4,
                y + 2,
                area.width - 7,
                &trend,
                (high - low).max(1.0),
                c,
            );
        }
        hitboxes.push((rect, HitTarget::Session(session.key)));
    }
    if sessions.len() > visible {
        s.text(
            area.x,
            area.bottom().saturating_sub(1),
            format!(
                "{}–{} of {} · scroll to explore",
                start + 1,
                (start + visible).min(sessions.len()),
                sessions.len()
            ),
            FAINT,
        );
    }
}

fn maps(s: &mut Surface<'_>, app: &App, area: Rect, hitboxes: &mut Vec<(Rect, HitTarget)>) {
    if area.height == 0 {
        return;
    }
    // A restrained dot field doubles as a visual reference for the topology.
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if (x - area.x).is_multiple_of(6) && (y - area.y).is_multiple_of(3) {
                s.text(x, y, "·", LINE);
            }
        }
    }
    let sessions = app.sessions();
    if sessions.is_empty() {
        s.bold(
            area.x + 3,
            area.y + area.height / 2,
            if !app.query.is_empty() {
                "No filter matches."
            } else if app.snapshot.all_processes {
                "No accessible process families."
            } else {
                "Your next session starts here."
            },
            MINT,
        );
        if area.height > 6 {
            s.text(
                area.x + 3,
                area.y + area.height / 2 + 2,
                "claude · codex · aider · opencode · goose",
                DIM,
            );
        }
        return;
    }
    let focused = app.focused || s.area.width < 120 || area.height < 16;
    if focused {
        if let Some(session) = app.selected_session() {
            constellation(s, session, area, true, hitboxes);
        }
    } else {
        let mut shown: Vec<_> = sessions.into_iter().take(4).collect();
        if let Some(selected) = app.selected_session()
            && !shown.iter().any(|session| session.key == selected.key)
        {
            shown.pop();
            shown.push(selected);
        }
        let col_w = area.width / 2;
        let row_h = area.height / 2;
        for (i, session) in shown.iter().enumerate() {
            let rect = Rect::new(
                area.x + (i % 2) as u16 * col_w,
                area.y + (i / 2) as u16 * row_h,
                col_w.saturating_sub(2),
                row_h.saturating_sub(1),
            );
            constellation(
                s,
                session,
                rect,
                Some(session.key) == app.selected,
                hitboxes,
            );
        }
    }
}

fn focus_view(s: &mut Surface<'_>, app: &App, area: Rect, hitboxes: &mut Vec<(Rect, HitTarget)>) {
    let Some(session) = app.selected_session() else {
        return;
    };
    if area.height >= 14 {
        let graph_h = (area.height / 2).min(10);
        constellation(
            s,
            session,
            Rect::new(area.x, area.y, area.width, graph_h),
            true,
            hitboxes,
        );
        process_table(
            s,
            app,
            Rect::new(
                area.x,
                area.y + graph_h + 1,
                area.width,
                area.height.saturating_sub(graph_h + 1),
            ),
            hitboxes,
        );
    } else {
        process_table(s, app, area, hitboxes);
    }
}

fn process_table(
    s: &mut Surface<'_>,
    app: &App,
    area: Rect,
    hitboxes: &mut Vec<(Rect, HitTarget)>,
) {
    if area.height < 2 || area.width < 12 {
        return;
    }
    let Some(session) = app.selected_session() else {
        s.text(area.x, area.y, empty_title(app), TEXT);
        for (i, line) in wrap_line(empty_hint(app), area.width as usize)
            .iter()
            .enumerate()
        {
            if i as u16 + 2 < area.height {
                s.text(area.x, area.y + 2 + i as u16, line, DIM);
            }
        }
        return;
    };
    let rows = app.explorer.visible_rows(session);
    let selected = app.explorer.selected();
    let c = color(session);
    let pane = if app.focus == Focus::Processes {
        "◉ PROCESS EXPLORER"
    } else {
        "○ PROCESS EXPLORER"
    };
    s.bold(
        area.x,
        area.y,
        if area.width < 65 {
            format!("{pane} #{}", selected.unwrap_or(session.key).pid)
        } else {
            pane.into()
        },
        if app.focus == Focus::Processes {
            c
        } else {
            DIM
        },
    );
    let wide = area.width >= 65;
    let name_w = area.width.saturating_sub(if wide { 33 } else { 23 });
    let rss_x = area.x + area.width.saturating_sub(if wide { 20 } else { 18 });
    let cpu_x = area.x + area.width.saturating_sub(8);
    if area.height >= 4 {
        s.text(area.x, area.y + 1, "PROCESS", FAINT);
        if wide {
            s.text(area.x + name_w + 1, area.y + 1, "PID", FAINT);
        }
        s.text(rss_x, area.y + 1, "RSS", FAINT);
        s.text(cpu_x, area.y + 1, "CPU", FAINT);
    }
    let header = if area.height >= 4 { 2 } else { 1 };
    let visible = area.height.saturating_sub(header).max(1) as usize;
    let selected_index = rows
        .iter()
        .position(|r| Some(r.key) == selected)
        .unwrap_or(0);
    let start = selected_index.saturating_sub(visible - 1);
    let by_key: HashMap<_, _> = session.processes.iter().map(|p| (p.key, p)).collect();
    for (i, row) in rows.iter().skip(start).take(visible).enumerate() {
        let Some(process) = by_key.get(&row.key) else {
            continue;
        };
        let y = area.y + header + i as u16;
        let active = Some(row.key) == selected;
        if active {
            s.fill(Rect::new(area.x, y, area.width, 1), SELECT);
        }
        let indent = (row.depth as u16).min(name_w.saturating_sub(10) / 2) * 2;
        let marker = if row.has_children {
            if row.expanded { "▾" } else { "▸" }
        } else {
            "·"
        };
        let label = format!("{}{} {}", " ".repeat(indent as usize), marker, process.name);
        s.text(
            area.x,
            y,
            label.chars().take(name_w as usize).collect::<String>(),
            if active { c } else { TEXT },
        );
        if wide {
            s.text(area.x + name_w + 1, y, process.pid().to_string(), DIM);
        }
        s.text(rss_x, y, bytes(process.rss_bytes), TEXT);
        s.text(
            cpu_x,
            y,
            cpu(process.cpu_percent),
            if process.cpu_percent.unwrap_or(0.0) > 50.0 {
                PEACH
            } else {
                DIM
            },
        );
        hitboxes.push((
            Rect::new(area.x, y, area.width, 1),
            HitTarget::Process(row.key),
        ));
    }
}

fn constellation(
    s: &mut Surface<'_>,
    session: &SessionSnapshot,
    area: Rect,
    active: bool,
    hitboxes: &mut Vec<(Rect, HitTarget)>,
) {
    if area.width < 12 || area.height < 3 {
        return;
    }
    let c = color(session);
    let ink = c;
    s.bold(
        area.x + 1,
        area.y,
        format!("{} {}", if active { "◉" } else { "○" }, session.label),
        ink,
    );
    let subtitle = format!("#{} · {}", session.key.pid, bytes(session.rss_bytes()));
    if area.width > 35 {
        s.text(
            area.right() - subtitle.len() as u16 - 1,
            area.y,
            subtitle,
            DIM,
        );
    }
    hitboxes.push((area, HitTarget::Session(session.key)));
    let max_nodes = if area.height >= 14 { 20 } else { 9 };
    let positions = node_positions(
        session,
        Rect::new(
            area.x + 1,
            area.y + 2,
            area.width - 2,
            area.height.saturating_sub(3),
        ),
        max_nodes,
    );
    let by_pid: HashMap<_, _> = positions
        .iter()
        .map(|(pid, x, y)| (*pid, (*x, *y)))
        .collect();
    let mut rails: HashMap<(u16, u16), u8> = HashMap::new();
    for process in &session.processes {
        if let (Some((x, y)), Some(parent)) = (
            by_pid.get(&process.key.pid),
            process.ppid.and_then(|p| by_pid.get(&p)),
        ) {
            for (point, mask) in edge_cells(parent.0, parent.1, *x, *y) {
                *rails.entry(point).or_insert(0) |= mask;
            }
        }
    }
    for ((x, y), mask) in rails {
        s.text(x, y, edge_glyph(mask), if active { LINE } else { FAINT });
    }
    for (pid, x, y) in &positions {
        if let Some(process) = session.processes.iter().find(|p| p.key.pid == *pid) {
            let root = process.key == session.key;
            let hot = process.cpu_percent.unwrap_or(0.0) > 15.0;
            s.bold(
                *x,
                *y,
                if root {
                    "◉"
                } else if hot {
                    "●"
                } else {
                    "○"
                },
                if root || hot { ink } else { DIM },
            );
            let name = if root {
                session.label.as_str()
            } else {
                &process.name
            };
            let next_column = positions
                .iter()
                .map(|(_, xx, _)| *xx)
                .filter(|xx| *xx > *x)
                .min()
                .unwrap_or(area.right());
            let max_label = next_column.saturating_sub(*x + 3).min(12) as usize;
            s.text(
                *x + 2,
                *y,
                name.chars().take(max_label).collect::<String>(),
                if root { ink } else { DIM },
            );
        }
    }
    if session.processes.len() > positions.len() {
        s.text(
            area.x + 1,
            area.bottom() - 1,
            format!(
                "+{} processes not shown",
                session.processes.len() - positions.len()
            ),
            FAINT,
        );
    } else if area.height >= 9 {
        s.text(
            area.x + 1,
            area.bottom() - 1,
            format!(
                "{} CPU · {} connected processes",
                cpu(session.cpu_percent()),
                session.process_count()
            ),
            FAINT,
        );
    }
}

// Bounded tidy tree: sibling subtrees occupy disjoint vertical bands. The map
// never reattaches omitted descendants or lets unrelated parent rails merge.
fn node_positions(session: &SessionSnapshot, area: Rect, limit: usize) -> Vec<(u32, u16, u16)> {
    if area.height == 0 || area.width == 0 || limit == 0 {
        return vec![];
    }
    let root = session.key.pid;
    let max_depth = ((area.width.saturating_sub(12)) / 12).clamp(1, 5) as usize;
    let leaf_budget = area.height as usize;
    let mut all_children: HashMap<u32, Vec<u32>> = HashMap::new();
    for process in &session.processes {
        if process.key.pid != root
            && let Some(parent) = process.ppid
        {
            all_children.entry(parent).or_default().push(process.pid());
        }
    }
    for children in all_children.values_mut() {
        children.sort_unstable();
        children.dedup();
    }
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut seen = std::collections::HashSet::from([root]);
    let mut queue = std::collections::VecDeque::from([(root, 0usize)]);
    let mut leaves = 1usize;
    while let Some((pid, depth)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }
        let room = leaf_budget.saturating_sub(leaves) + 1;
        let selected: Vec<_> = all_children
            .get(&pid)
            .into_iter()
            .flatten()
            .filter(|child| !seen.contains(child))
            .copied()
            .take(room.min(limit.saturating_sub(seen.len())))
            .collect();
        if !selected.is_empty() {
            leaves += selected.len() - 1;
            for child in &selected {
                seen.insert(*child);
                queue.push_back((*child, depth + 1));
            }
            children.insert(pid, selected);
        }
    }
    let mut preorder = Vec::new();
    let mut stack = vec![(root, 0usize)];
    while let Some((pid, depth)) = stack.pop() {
        preorder.push((pid, depth));
        if let Some(list) = children.get(&pid) {
            stack.extend(list.iter().rev().map(|child| (*child, depth + 1)));
        }
    }
    let leaf_ids: Vec<_> = preorder
        .iter()
        .filter(|(pid, _)| !children.contains_key(pid))
        .map(|(pid, _)| *pid)
        .collect();
    let mut bands: HashMap<u32, (u16, u16)> = HashMap::new();
    for (i, pid) in leaf_ids.iter().enumerate() {
        let y = if leaf_ids.len() == 1 {
            area.y + area.height / 2
        } else {
            area.y + (i * (area.height - 1) as usize / (leaf_ids.len() - 1)) as u16
        };
        bands.insert(*pid, (y, y));
    }
    for (pid, _) in preorder.iter().rev() {
        if let Some(list) = children.get(pid) {
            let low = bands[&list[0]].0;
            let high = bands[list.last().unwrap()].1;
            bands.insert(*pid, (low, high));
        }
    }
    let depth = preorder
        .iter()
        .map(|(_, depth)| *depth)
        .max()
        .unwrap_or(1)
        .max(1);
    let step = (area.width.saturating_sub(15) / depth as u16).max(4);
    preorder
        .into_iter()
        .map(|(pid, depth)| {
            let (low, high) = bands[&pid];
            (
                pid,
                area.x + (depth as u16 * step).min(area.width.saturating_sub(2)),
                low + (high - low) / 2,
            )
        })
        .collect()
}

// Up/right/down/left bitmasks preserve junctions when several edges share a
// legitimate parent trunk. Drawing an edge never erases an earlier connection.
fn edge_cells(x1: u16, y1: u16, x2: u16, y2: u16) -> HashMap<(u16, u16), u8> {
    let mut result = HashMap::new();
    if x2 <= x1 {
        return result;
    }
    let mid = x1 + (x2 - x1) / 2;
    let mut path = Vec::new();
    for x in x1..=mid {
        path.push((x, y1));
    }
    if y2 > y1 {
        for y in y1 + 1..=y2 {
            path.push((mid, y));
        }
    } else {
        for y in (y2..y1).rev() {
            path.push((mid, y));
        }
    }
    for x in mid + 1..=x2 {
        path.push((x, y2));
    }
    for pair in path.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let (forward, back) = if a.0 < b.0 {
            (2, 8)
        } else if a.0 > b.0 {
            (8, 2)
        } else if a.1 < b.1 {
            (4, 1)
        } else {
            (1, 4)
        };
        *result.entry(a).or_insert(0) |= forward;
        *result.entry(b).or_insert(0) |= back;
    }
    result
}
fn edge_glyph(mask: u8) -> &'static str {
    match mask {
        5 => "│",
        6 => "╭",
        12 => "╮",
        3 => "╰",
        9 => "╯",
        7 => "├",
        13 => "┤",
        14 => "┬",
        11 => "┴",
        15 => "┼",
        1 | 4 => "│",
        _ => "─",
    }
}

fn details(s: &mut Surface<'_>, app: &App, y: u16, height: u16) {
    let width = s.area.width;
    s.rule(3, y, width - 6);
    let Some(session) = app.selected_session() else {
        s.text(3, y + 1, empty_hint(app), DIM);
        return;
    };
    let c = color(session);
    s.bold(
        3,
        y + 1,
        format!("◉ {} / #{}", session.label, session.pid()),
        c,
    );
    let info = format!(
        "{} RSS   {} CPU   {} processes",
        bytes(session.rss_bytes()),
        cpu(session.cpu_percent()),
        session.process_count()
    );
    s.text(
        width.saturating_sub(info.len() as u16 + 3),
        y + 1,
        info,
        TEXT,
    );
    if height < 7 {
        s.text(
            3,
            y + 2,
            "RSS ≠ unique RAM · CPU 100% = one core · ? limits",
            DIM,
        );
        return;
    }
    if app.focused
        && let Some(process) = session
            .processes
            .iter()
            .find(|p| Some(p.key) == app.explorer.selected())
    {
        s.text(
            3,
            y + 2,
            format!(
                "INSPECT {} #{} · parent {} · {} RSS · {} CPU",
                process.name,
                process.pid(),
                process
                    .ppid
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "none".into()),
                bytes(process.rss_bytes),
                cpu(process.cpu_percent)
            ),
            DIM,
        );
    }
    let half = (width - 11) / 2;
    s.text(3, y + 3, "RAM TRAIL", DIM);
    s.text(half + 8, y + 3, "CPU TRAIL", DIM);
    let rss: Vec<f64> = session.history.iter().map(|v| *v as f64).collect();
    let low = rss.iter().copied().fold(f64::INFINITY, f64::min);
    let high = rss.iter().copied().fold(0.0, f64::max);
    let trend: Vec<_> = rss.iter().map(|v| v - low).collect();
    s.spark(3, y + 4, half, &trend, (high - low).max(1.0), c);
    s.text(
        14,
        y + 3,
        format!("{}–{} · auto", bytes(low as u64), bytes(high as u64)),
        DIM,
    );
    let cpu_values: Vec<f64> = session
        .cpu_history
        .iter()
        .map(|v| v.unwrap_or(0.0) as f64)
        .collect();
    let cpu_max = cpu_values.iter().copied().fold(100.0, f64::max);
    s.spark(half + 8, y + 4, half, &cpu_values, cpu_max, LILAC);
    let growth = app.insights.growth(session.key);
    let delta = app.growth_delta(session);
    let signed = format!(
        "{}{}",
        if delta >= 0 { "+" } else { "−" },
        bytes(delta.unsigned_abs())
    );
    let change = if let Some(growth) = growth.filter(|g| g.sample_count >= 2) {
        format!(
            "{} / {:.0}s · {:.1} MiB/min",
            signed,
            growth.elapsed_seconds.unwrap_or(0.0),
            growth.bytes_per_second.unwrap_or(0.0) * 60.0 / (1_u64 << 20) as f64
        )
    } else {
        format!(
            "{} over {} samples · rate warming",
            signed,
            session.history.len()
        )
    };
    s.text(3, y + 5, change, DIM);
    s.text(
        half + 8,
        y + 5,
        format!("scale 0–{cpu_max:.0}% · 100% = one logical core"),
        DIM,
    );
    s.text(
        3,
        y + 7,
        "Session RSS may overlap shared pages. Headroom is a heuristic, not native OS memory pressure.",
        DIM,
    );
}

fn empty_title(app: &App) -> &'static str {
    if !app.query.is_empty() {
        "No filter matches."
    } else if app.snapshot.all_processes {
        "No accessible families."
    } else {
        "Nothing in orbit. Yet."
    }
}
fn empty_hint(app: &App) -> &'static str {
    if !app.query.is_empty() && app.focused {
        "Esc returns to sessions; Esc again clears the filter. Scope totals stay unfiltered."
    } else if !app.query.is_empty() {
        "Search checks names and PIDs. Esc clears the filter. Scope totals stay unfiltered."
    } else if app.snapshot.all_processes {
        "No owned processes were accessible in this sample. Try --doctor outside the UI."
    } else {
        "Start Claude, Codex, or another coding agent. Press a to explore all your accessible process families."
    }
}
fn wrap_line(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > width {
            rows.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        for ch in word.chars() {
            if current.chars().count() == width {
                rows.push(std::mem::take(&mut current));
            }
            current.push(ch);
        }
    }
    if !current.is_empty() || rows.is_empty() {
        rows.push(current);
    }
    rows
}
const GUIDE: &[(&str, Color)] = &[
    ("MOVE THROUGH YOUR WORK", MINT),
    (
        "j/k or arrows select a session. Enter opens its process explorer. Tab switches the active pane.",
        TEXT,
    ),
    (
        "Left/right or h/l folds a branch. E expands all; C collapses all. Esc leaves the inspector.",
        TEXT,
    ),
    (
        "/ searches agent names, process names, and PIDs. Enter keeps the filter; Esc cancels edits or clears a saved filter.",
        TEXT,
    ),
    (
        "* pins the selected session. A visible * marks it. Pins live only for this run.",
        TEXT,
    ),
    (
        "s sorts RSS, CPU, then observed growth. a switches AGENTS and ALL FAMILIES, only for your OS user.",
        TEXT,
    ),
    (
        "Space freezes sampling and the view. A scope arrow while paused means the change is pending until resume.",
        TEXT,
    ),
    (
        "q quits. Ctrl-C always quits, even while searching. ? or Esc closes this guide.",
        TEXT,
    ),
    ("READ THE CONSTELLATIONS", LILAC),
    (
        "Lines mean parent to child, not network traffic. Each PID belongs to one session. Nested agents form separate sessions.",
        TEXT,
    ),
    (
        "A filled process dot uses over 15% of a core; a hollow dot is quieter. Omitted nodes are counted, not reattached.",
        TEXT,
    ),
    (
        "The header dot changes only when a sample arrives. --no-motion keeps it static. Paused or stale data never animates.",
        TEXT,
    ),
    (
        "SCOPE TOTAL includes all sessions in the current scope, even those hidden by search. System CPU and headroom are machine-wide.",
        TEXT,
    ),
    (
        "Inspector rows are individual processes. Bottom RAM and CPU trails are totals for the selected session.",
        TEXT,
    ),
    (
        "RAM trails auto-scale: use the printed low/high values. Growth uses actual sample time; resume resets the timed window.",
        TEXT,
    ),
    ("KNOW THE LIMITS", PEACH),
    (
        "RSS sums can count shared pages twice. They are not unique physical RAM or macOS memory footprint.",
        TEXT,
    ),
    (
        "Headroom warns at 20% available, alerts at 10%, and clears at 23% / 13%. This is a heuristic, not native macOS pressure or crash prediction.",
        TEXT,
    ),
    (
        "Process CPU 100% means one logical core. System CPU is normalized across all cores. Warming needs another sample.",
        TEXT,
    ),
    (
        "Only your user's process metrics are shown. Detection and permissions can be incomplete. PID/start identity has seconds precision.",
        TEXT,
    ),
    (
        "No process control, argument display, saved activity logs, or telemetry. The OS backend may read argument buffers transiently on macOS.",
        MINT,
    ),
    ("START AGAIN", LILAC),
    (
        "./lattice --tour is a simulated walkthrough. ./lattice is live. ./lattice --doctor checks this machine without a terminal UI.",
        TEXT,
    ),
];
fn guide_rect(width: u16, height: u16) -> Rect {
    let w = width.min(92).saturating_sub(4).max(1);
    let h = height.min(34).saturating_sub(2).max(1);
    Rect::new(
        width.saturating_sub(w) / 2,
        height.saturating_sub(h) / 2,
        w,
        h,
    )
}
fn guide_lines(width: usize) -> Vec<(String, Color)> {
    GUIDE
        .iter()
        .flat_map(|(line, color)| wrap_line(line, width).into_iter().map(|row| (row, *color)))
        .collect()
}
pub fn guide_scroll_limits(width: u16, height: u16) -> (usize, usize) {
    let r = guide_rect(width, height);
    let rows = r.height.saturating_sub(5).max(1) as usize;
    let count = guide_lines(r.width.saturating_sub(4) as usize).len();
    (rows, count.saturating_sub(rows))
}
fn tour_copy(step: usize) -> (&'static str, &'static str) {
    match step {
        0 => (
            "1/3 · YOUR SESSIONS, CONNECTED",
            "This is simulated data, not your laptop. Each constellation groups a coding agent with its child processes. Lines mean parent to child, not network traffic.",
        ),
        1 => (
            "2/3 · FIND THE HEAVY BRANCH",
            "Arrows select; Enter inspects. Tab switches panes. Left/right folds branches. / finds names or PIDs; * pins a session. Space pauses to inspect without chasing changing numbers.",
        ),
        _ => (
            "3/3 · READ THE NUMBERS",
            "RSS may count shared memory twice. CPU 100% means one core. Headroom is a warning heuristic, not crash prevention. Enter explores this demo. Quit with q, then run ./lattice for LIVE data.",
        ),
    }
}

fn guide(s: &mut Surface<'_>, app: &App) {
    let mut r = guide_rect(s.area.width, s.area.height);
    if app.tour {
        let (_, body) = tour_copy(app.tour_step);
        r.height = r
            .height
            .min(wrap_line(body, r.width.saturating_sub(4) as usize).len() as u16 + 6);
        r.y = s.area.height.saturating_sub(r.height) / 2;
    }
    s.fill(r, PANEL);
    if r.width < 8 || r.height < 5 {
        return;
    }
    let width = r.width.saturating_sub(4) as usize;
    let rows = r.height.saturating_sub(5) as usize;
    if app.tour {
        let (title, body) = tour_copy(app.tour_step);
        s.bold(r.x + 2, r.y, "DEMO TOUR · simulated data", MINT);
        s.text(
            r.x + 2,
            r.y + 1,
            title.chars().take(width).collect::<String>(),
            LILAC,
        );
        for (i, line) in wrap_line(body, width).into_iter().take(rows).enumerate() {
            s.text(r.x + 2, r.y + 3 + i as u16, line, TEXT);
        }
        s.text(r.x + 2, r.bottom() - 1, "enter next  esc skip  q quit", DIM);
    } else {
        let lines = guide_lines(width);
        let start = app.help_scroll.min(lines.len().saturating_sub(rows));
        s.bold(
            r.x + 2,
            r.y,
            format!("FIELD GUIDE {}/{}", start + 1, lines.len()),
            MINT,
        );
        s.text(
            r.x + 2,
            r.y + 1,
            format!(
                "{} · {}",
                if app.focused { "INSPECTOR" } else { "SESSIONS" },
                app.scope_label()
            ),
            DIM,
        );
        for (i, (line, color)) in lines.into_iter().skip(start).take(rows).enumerate() {
            s.text(r.x + 2, r.y + 3 + i as u16, line, color);
        }
        s.text(
            r.x + 2,
            r.bottom() - 1,
            "j/k scroll  esc close  q quit",
            DIM,
        );
    }
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
fn hex(color: Color) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        _ => "#e2e9f2".into(),
    }
}
pub fn save_svg(app: &App, width: u16, height: u16, path: &Path) -> io::Result<()> {
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    Dashboard {
        app,
        hitboxes: &mut Vec::new(),
    }
    .render(area, &mut buf);
    let cw = 9.0;
    let ch = 18.0;
    let pad = 24.0;
    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\"><rect width=\"100%\" height=\"100%\" rx=\"16\" fill=\"#0a0f18\"/><g font-family=\"DejaVu Sans Mono, monospace\" font-size=\"14\">",
        width as f64 * cw + 2.0 * pad,
        height as f64 * ch + 2.0 * pad,
        width as f64 * cw + 2.0 * pad,
        height as f64 * ch + 2.0 * pad
    );
    for y in 0..height {
        for x in 0..width {
            let cell = &buf[(x, y)];
            let xx = pad + x as f64 * cw;
            let yy = pad + y as f64 * ch;
            if cell.bg != BG && cell.bg != Color::Reset {
                svg.push_str(&format!(
                    "<rect x=\"{xx}\" y=\"{yy}\" width=\"{cw}\" height=\"{ch}\" fill=\"{}\"/>",
                    hex(cell.bg)
                ));
            }
            if cell.symbol() != " " {
                svg.push_str(&format!(
                    "<text x=\"{xx}\" y=\"{}\" fill=\"{}\"{}>{}</text>",
                    yy + 14.0,
                    hex(cell.fg),
                    if cell.modifier.contains(Modifier::BOLD) {
                        " font-weight=\"bold\""
                    } else {
                        ""
                    },
                    xml(cell.symbol())
                ));
            }
        }
    }
    svg.push_str("</g></svg>");
    fs::write(path, svg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sampling::DemoSampler;
    fn render(width: u16, height: u16, app: &App) -> String {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        Dashboard {
            app,
            hitboxes: &mut Vec::new(),
        }
        .render(area, &mut buf);
        buf.content.iter().map(|c| c.symbol()).collect()
    }
    #[test]
    fn rendering_survives_terminal_sizes_and_help() {
        let mut app = App::new(DemoSampler::new().sample(false));
        for (w, h) in [
            (1, 1),
            (20, 6),
            (40, 16),
            (58, 19),
            (80, 24),
            (100, 30),
            (144, 44),
            (240, 100),
        ] {
            assert!(!render(w, h, &app).is_empty());
            app.help = true;
            assert!(!render(w, h, &app).is_empty());
            app.help = false;
        }
    }
    #[test]
    fn demo_and_live_are_visibly_distinct() {
        let mut app = App::new(DemoSampler::new().sample(false));
        assert!(render(144, 44, &app).contains("DEMO"));
        app.snapshot.demo = false;
        assert!(render(144, 44, &app).contains("LIVE"));
    }
    #[test]
    fn graph_uses_real_processes_and_bounded_positions() {
        let snapshot = DemoSampler::new().sample(false);
        for session in &snapshot.sessions {
            let area = Rect::new(3, 5, 60, 16);
            let nodes = node_positions(session, area, 12);
            assert!(nodes.len() <= 12);
            assert!(nodes.iter().any(|(pid, _, _)| *pid == session.key.pid));
            for (pid, x, y) in nodes {
                assert!(area.contains((x, y).into()));
                assert!(session.processes.iter().any(|p| p.key.pid == pid));
            }
        }
    }
    #[test]
    fn empty_state_and_warning_are_actionable() {
        let mut app = App::new(DemoSampler::new().sample(false));
        app.snapshot.sessions.clear();
        assert!(render(144, 44, &app).contains("Nothing in orbit"));
        app.snapshot.system.memory_available = 1;
        app.insights.update(&app.snapshot);
        assert!(render(144, 44, &app).contains("LOW HEADROOM"));
    }
    #[test]
    fn unknown_memory_and_stale_age_never_reassure() {
        let mut snapshot = DemoSampler::new().sample(false);
        snapshot.system.memory_total = 0;
        snapshot.system.memory_available = 0;
        let mut app = App::new(snapshot);
        let text = render(144, 44, &app);
        assert!(text.contains("unavailable"));
        assert!(!text.contains("room to breathe"));
        app.snapshot.demo = false;
        app.last_sample = std::time::Instant::now() - std::time::Duration::from_secs(600);
        assert!(render(144, 44, &app).contains("STALE"));
        assert!(render(40, 16, &app).contains("STALE"));
    }
    #[test]
    fn inspector_is_safe_at_narrow_and_tiny_sizes() {
        let mut app = App::new(DemoSampler::new().sample(false));
        app.focused = true;
        app.focus = Focus::Processes;
        for (w, h) in [(1, 1), (20, 6), (40, 16), (58, 19), (80, 24), (144, 44)] {
            assert!(!render(w, h, &app).is_empty());
        }
        assert!(render(80, 24, &app).contains("PROCESS EXPLORER"));
    }
    #[test]
    fn svg_escapes_terminal_text() {
        assert_eq!(xml("<&>\""), "&lt;&amp;&gt;&quot;");
    }
}

#[cfg(test)]
mod compact_regressions {
    use super::*;
    use crate::sampling::DemoSampler;
    fn render_text(app: &App, w: u16, h: u16) -> String {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        Dashboard {
            app,
            hitboxes: &mut Vec::new(),
        }
        .render(area, &mut buf);
        buf.content
            .chunks(w as usize)
            .map(|r| r.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn compact_pause_is_truthful() {
        let mut app = App::new(DemoSampler::new().sample(false));
        app.snapshot.demo = false;
        app.paused = true;
        let rendered = render_text(&app, 40, 16);
        println!("{rendered}");
        assert!(
            rendered.contains("PAUSED"),
            "paused compact screen still says LIVE"
        );
    }
    #[test]
    fn compact_selection_stays_visible() {
        let mut app = App::new(DemoSampler::new().sample(false));
        let mut template = app.snapshot.sessions[0].clone();
        for i in 0..20 {
            template.key.pid = 8000 + i;
            template.label = format!("testsession{i}");
            app.snapshot.sessions.push(template.clone());
        }
        app.selected = Some(template.key);
        let rendered = render_text(&app, 40, 16);
        assert!(
            rendered.contains("testsession19"),
            "selected session scrolled entirely out of view"
        );
    }
    #[test]
    fn compact_failure_is_truthful() {
        let mut app = App::new(DemoSampler::new().sample(false));
        app.snapshot.demo = false;
        app.sampler_failed = true;
        let rendered = render_text(&app, 40, 16);
        assert!(
            rendered.contains("stale") || rendered.contains("stopped"),
            "failed compact screen still says LIVE"
        );
    }
}

#[cfg(test)]
mod graph_regressions {
    use super::*;
    use crate::model::ProcessSnapshot;
    #[test]
    fn interleaved_pids_keep_sibling_subtree_rails_disjoint() {
        let process = |pid, ppid| ProcessSnapshot {
            key: ProcessKey { pid, started: 1 },
            ppid: Some(ppid),
            name: format!("p{pid}"),
            ..ProcessSnapshot::default()
        };
        let session = SessionSnapshot {
            key: ProcessKey { pid: 1, started: 1 },
            processes: vec![
                process(1, 0),
                process(2, 1),
                process(3, 2),
                process(4, 1),
                process(5, 2),
                process(6, 4),
                process(7, 4),
            ],
            ..SessionSnapshot::default()
        };
        let positions = node_positions(&session, Rect::new(3, 5, 100, 9), 20);
        let points: HashMap<_, _> = positions
            .iter()
            .map(|(pid, x, y)| (*pid, (*x, *y)))
            .collect();
        assert!(points[&3].1 <= points[&5].1 && points[&5].1 < points[&6].1);
        assert!(points[&2].1 >= points[&3].1 && points[&2].1 <= points[&5].1);
        assert!(points[&4].1 >= points[&6].1 && points[&4].1 <= points[&7].1);
        let rails = |parent, children: &[u32]| {
            children
                .iter()
                .flat_map(|child| {
                    let ((x1, y1), (x2, y2)) = (points[&parent], points[child]);
                    edge_cells(x1, y1, x2, y2).into_keys()
                })
                .collect::<std::collections::HashSet<_>>()
        };
        assert!(rails(2, &[3, 5]).is_disjoint(&rails(4, &[6, 7])));
        for limit in [1, 2, 3, 4] {
            let shown = node_positions(&session, Rect::new(0, 0, 40, 3), limit);
            for (pid, _, _) in &shown {
                if *pid != 1 {
                    let parent = session
                        .processes
                        .iter()
                        .find(|p| p.pid() == *pid)
                        .unwrap()
                        .ppid
                        .unwrap();
                    assert!(shown.iter().any(|(p, _, _)| *p == parent));
                }
            }
        }
    }
    #[test]
    fn shared_parent_junctions_merge_without_erasing_branches() {
        let mut rails = edge_cells(0, 2, 10, 0);
        for (point, mask) in edge_cells(0, 2, 10, 4) {
            *rails.entry(point).or_insert(0) |= mask;
        }
        assert_eq!(edge_glyph(rails[&(5, 2)]), "┤");
    }
}

#[cfg(test)]
mod onboarding_regressions {
    use super::*;
    use crate::sampling::DemoSampler;
    fn render(app: &App, w: u16, h: u16) -> String {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        Dashboard {
            app,
            hitboxes: &mut Vec::new(),
        }
        .render(area, &mut buf);
        buf.content
            .chunks(w as usize)
            .map(|line| line.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn same_name_sessions_keep_visible_pid_and_pin_identity_at_all_sizes() {
        let mut app = App::new(DemoSampler::new().sample(false));
        for session in &mut app.snapshot.sessions {
            session.label = "claude".into();
        }
        app.toggle_pin();
        let selected_pid = app.selected.unwrap().pid.to_string();
        for (w, h) in [(40, 16), (58, 19), (80, 24), (144, 44)] {
            let text = render(&app, w, h);
            assert!(
                text.contains(&format!("#{selected_pid}")),
                "{w}x{h}: {text}"
            );
            assert!(text.contains("* claude"), "{w}x{h}: {text}");
            assert!(text.contains("AGENTS"));
            assert!(text.contains("q quit"));
        }
    }
    #[test]
    fn empty_states_distinguish_agent_detection_access_and_filtering() {
        let mut app = App::new(DemoSampler::new().sample(false));
        app.snapshot.sessions.clear();
        app.selected = None;
        for (w, h) in [(40, 16), (80, 24), (144, 44)] {
            app.snapshot.all_processes = false;
            app.requested_all = false;
            app.query.clear();
            assert!(render(&app, w, h).contains("Nothing in orbit"));
            app.snapshot.all_processes = true;
            app.requested_all = true;
            let all = render(&app, w, h);
            assert!(all.contains("No accessible families"));
            assert!(!all.contains("No agents"));
            app.query = "missing".into();
            let filtered = render(&app, w, h);
            assert!(filtered.contains("No filter matches"));
            assert!(!filtered.contains("No recognized coding"));
        }
    }
    #[test]
    fn all_help_is_reachable_and_inspector_context_is_visible() {
        let mut app = App::new(DemoSampler::new().sample(false));
        app.help = true;
        app.focused = true;
        for (w, h) in [(40, 16), (58, 19), (80, 24), (144, 44)] {
            app.help_scroll = 0;
            let top = render(&app, w, h);
            assert!(top.contains("INSPECTOR"));
            let (rows, maximum) = guide_scroll_limits(w, h);
            assert!(rows > 0 && maximum > 0);
            app.help_scroll = usize::MAX;
            let bottom = render(&app, w, h);
            assert!(bottom.contains("--doctor"), "{w}x{h}: {bottom}");
            assert!(bottom.contains("q quit"));
        }
    }
    #[test]
    fn tour_text_remains_complete_at_small_supported_sizes() {
        let mut app = App::new(DemoSampler::new().sample(false));
        app.tour = true;
        for (w, h) in [(40, 16), (58, 19), (80, 24), (144, 44)] {
            for (step, last) in [
                (0, "network traffic."),
                (1, "changing numbers."),
                (2, "LIVE data."),
            ] {
                app.tour_step = step;
                let text = render(&app, w, h);
                assert!(text.contains("DEMO TOUR"));
                // Ignore terminal line wraps while checking the final text is not clipped.
                let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
                assert!(words.contains(last), "{w}x{h}: {text}");
            }
        }
    }
    #[test]
    fn help_wraps_long_tokens_without_losing_text() {
        assert_eq!(wrap_line("abcdefghij", 3).concat(), "abcdefghij");
        assert!(
            wrap_line("hello world", 4)
                .iter()
                .all(|s| s.chars().count() <= 4)
        );
    }
}

#[cfg(test)]
mod inspector_empty_regression {
    use super::*;
    #[test]
    fn filtered_empty_inspector_explains_escape_in_compact_layouts() {
        let mut app = App::new(crate::sampling::DemoSampler::new().sample(false));
        app.focused = true;
        app.start_search();
        for ch in "zz-no-match".chars() {
            app.search_key(crossterm::event::KeyCode::Char(ch));
        }
        app.search_key(crossterm::event::KeyCode::Enter);
        assert!(app.selected.is_none());
        for (w, h) in [(40, 16), (58, 19), (80, 24), (144, 44)] {
            let area = Rect::new(0, 0, w, h);
            let mut buffer = Buffer::empty(area);
            Dashboard {
                app: &app,
                hitboxes: &mut Vec::new(),
            }
            .render(area, &mut buffer);
            let text = buffer
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(text.contains("No filter matches"), "{w}x{h}: {text}");
            if w < 72 {
                assert!(text.contains("Esc returns to sessions"));
            }
        }
    }
}

#[cfg(test)]
mod narrow_identity_regression {
    use super::*;
    #[test]
    fn narrow_inspector_keeps_session_pin_and_selected_process_pid_visible() {
        let mut app = App::new(crate::sampling::DemoSampler::new().sample(false));
        app.toggle_pin();
        app.focused = true;
        app.focus = Focus::Processes;
        app.move_cursor(1);
        let pid = app.explorer.selected().unwrap().pid;
        for (w, h) in [(40, 16), (58, 19), (80, 24)] {
            let area = Rect::new(0, 0, w, h);
            let mut buffer = Buffer::empty(area);
            Dashboard {
                app: &app,
                hitboxes: &mut Vec::new(),
            }
            .render(area, &mut buffer);
            let text = buffer
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(
                text.contains(&format!("PROCESS EXPLORER #{pid}")),
                "{w}x{h}: {text}"
            );
            assert!(text.contains('*'));
            assert!(text.contains("#4100"));
        }
    }
}
