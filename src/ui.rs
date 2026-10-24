//! One terminal-cell renderer powers the live UI, tests, and deterministic SVG exports.
use crate::{
    app::{App, Focus, GraphMode, Metric, Sort, View},
    changes::{ChangeKind, ContributorStatus},
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
const CYAN: Color = Color::Rgb(105, 219, 244);
const LILAC: Color = Color::Rgb(183, 165, 247);
const BLUE: Color = Color::Rgb(127, 196, 240);
const PEACH: Color = Color::Rgb(248, 185, 139);
const RED: Color = Color::Rgb(244, 134, 158);
#[derive(Clone, Copy, Debug)]
pub enum HitTarget {
    Session(ProcessKey),
    Process(ProcessKey),
    View(View),
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
        let app = self.app;
        let mut s = Surface { buf, area };
        let small = area.width < 72 || area.height < 22;
        let x = if area.width < 3 {
            0
        } else if small {
            1
        } else {
            3
        };
        let top = if small { 0 } else { 1 };
        s.bold(x, top, "◈ lattice", CYAN);
        let mode = if app.resuming {
            "SYNCING"
        } else if app.stale() {
            "STALE"
        } else if app.paused {
            "PAUSED"
        } else if app.snapshot.demo {
            if app.sample_marker() == "●" {
                "● DEMO"
            } else {
                "DEMO"
            }
        } else {
            if app.sample_marker() == "●" {
                "● LIVE"
            } else {
                "LIVE"
            }
        };
        if area.width >= 24 {
            s.bold(
                area.width.saturating_sub(if small { 11 } else { 27 }),
                top,
                mode,
                if app.stale() || app.paused {
                    PEACH
                } else {
                    MINT
                },
            );
        }
        if small {
            s.text(x, top + 1, app.scope_label(), DIM);
        } else {
            s.text(area.width.saturating_sub(14), top, app.scope_label(), DIM);
        }
        let tabs_y = 3;
        let mut tab_x = x;
        for view in View::ALL {
            let label = if area.width < 72 {
                match view {
                    View::Overview => "Home",
                    View::ProcessMap => "Map",
                    View::History => "History",
                    View::Changes => "Changes",
                }
            } else {
                view.label()
            };
            let text = format!("{} {label}", view.index() + 1);
            let width = text.chars().count() as u16;
            if view == app.view {
                s.fill(
                    Rect::new(tab_x.saturating_sub(1), tabs_y, width + 2, 1),
                    SELECT,
                );
                s.bold(tab_x, tabs_y, &text, CYAN);
            } else {
                s.text(tab_x, tabs_y, &text, DIM);
            }
            if tabs_y < area.height && tab_x < area.width {
                self.hitboxes.push((
                    Rect::new(
                        tab_x.saturating_sub(1),
                        tabs_y,
                        (width + 2).min(area.width.saturating_sub(tab_x.saturating_sub(1))),
                        1,
                    ),
                    HitTarget::View(view),
                ));
            }
            tab_x += width + if small { 2 } else { 4 };
        }
        let body_y = if small { 5 } else { 7 };
        if !small {
            s.rule(x, 5, area.width.saturating_sub(x * 2));
        }
        let body = Rect::new(
            x,
            body_y,
            area.width.saturating_sub(x * 2),
            area.height.saturating_sub(body_y + 4),
        );
        if body.height > 0 && body.width > 0 {
            match app.view {
                View::Overview => overview(&mut s, app, body, self.hitboxes),
                View::ProcessMap => process_map(&mut s, app, body, self.hitboxes),
                View::History => history_view(&mut s, app, body),
                View::Changes => changes_view(&mut s, app, body),
            }
        }
        // Motion is a short, input-driven arrival of the content only. Chrome,
        // status, sample values and the sampler clock remain independent.
        if let Some(progress) = app.transition_progress() {
            transition_content(
                &mut s,
                body,
                progress,
                app.transition.as_ref().map(|t| t.direction).unwrap_or(1),
            );
            // Never let a partially shifted frame expose stale content targets.
            self.hitboxes
                .retain(|(_, target)| matches!(target, HitTarget::View(_)));
        }
        if app.editing_search && body.height > 0 {
            s.fill(Rect::new(body.x, body.y, body.width, 1), BG);
            s.text(body.x, body.y, format!("/ {}▏", app.query), MINT);
        }
        if area.height >= 5 {
            let status_y = area.height - 4;
            let notice = if app.sampler_failed {
                Some(("Sampler stopped · values are stale", RED))
            } else if app.stale() {
                Some(("Waiting for sampler · data stale", PEACH))
            } else if let Some(notice) = app.attachment_notice() {
                Some((notice, PEACH))
            } else if app.snapshot.truncated {
                Some(("Scan limit reached · partial data", PEACH))
            } else if app.insights.headroom().level == HeadroomLevel::Critical {
                Some(("LOW HEADROOM · review workloads", RED))
            } else {
                None
            };
            if let Some((text, color)) = notice {
                s.text(x, status_y, text, color);
            } else if !app.editing_search && !app.query.is_empty() {
                s.text(
                    x,
                    status_y,
                    format!(
                        "/ {}{}",
                        app.query,
                        if app.editing_search { "▏" } else { "" }
                    ),
                    MINT,
                );
            } else if area.width >= 72 {
                s.text(
                    x,
                    status_y,
                    if app.snapshot.demo {
                        "SIMULATED DATA · nothing on your machine is being measured"
                    } else {
                        "LOCAL · NO PROCESS CONTROL"
                    },
                    FAINT,
                );
            }
        }
        if area.height >= 4 {
            let text = if let Some(notice) = &app.recording_notice {
                notice.clone()
            } else if let Some(recording) = &app.recording_status {
                let state = if recording.stopped {
                    "SAVED"
                } else if app.paused || app.stale() || app.resuming {
                    "REC GAP"
                } else {
                    "REC"
                };
                let mode = if recording.demo { "SIM " } else { "" };
                let action = if recording.stopped { "new" } else { "stop" };
                let duration = crate::recording::format_duration(recording.elapsed_secs);
                // Keep the stop control before an optional long name at every width.
                if area.width < 80 {
                    format!("{mode}{state} {duration} · R {action} · {}", recording.name)
                } else {
                    format!(
                        "{mode}{state} · {duration} · R {action} · {} · {} samples",
                        recording.name, recording.samples
                    )
                }
            } else {
                "R start local recording".into()
            };
            s.text(
                x,
                area.height - 3,
                text,
                if app.recording_notice.is_some() {
                    PEACH
                } else {
                    BLUE
                },
            );
        }
        if area.height > 2 {
            let hint = if app.editing_search {
                "type to filter · enter keep · esc cancel"
            } else {
                match app.view {
                    View::Overview => "↑↓ select  ↵ inspect  / find  * pin  r name  s sort",
                    View::ProcessMap if app.focused => {
                        "↑↓ move  ←→ fold  tab pane  i hide  g graph"
                    }
                    View::ProcessMap
                        if app.graph_mode == GraphMode::Heatmap
                            && app.focus == Focus::Processes =>
                    {
                        "↑↓ process  tab family  ↵ inspect  g graph  m metric"
                    }
                    View::ProcessMap if app.graph_mode == GraphMode::Heatmap => {
                        "↑↓ family  tab process  ↵ inspect  g graph  m metric"
                    }
                    View::ProcessMap => "↑↓ family  ↵ inspect  g heat map  / find",
                    View::History => "↑↓ family  m metric  / find  * pin  r name",
                    View::Changes => "↑↓ browse  ↵ inspect family  esc overview",
                }
            };
            let hint = if area.width < 72 && !app.editing_search {
                match app.view {
                    View::ProcessMap if app.focused => "↑↓ move  ←→ fold  i hide  tab pane",
                    View::ProcessMap
                        if app.graph_mode == GraphMode::Heatmap
                            && app.focus == Focus::Processes =>
                    {
                        "↑↓ process  tab family  g graph  m lens"
                    }
                    View::ProcessMap if app.graph_mode == GraphMode::Heatmap => {
                        "↑↓ family  tab process  g graph  m lens"
                    }
                    View::ProcessMap => "↑↓ family  ↵ inspect  g graph",
                    View::History => "↑↓ family  m RAM/CPU  / find",
                    _ => hint,
                }
            } else {
                hint
            };
            s.text(x, area.height - 2, hint, DIM);
        }
        if area.height > 1 {
            s.text(
                x,
                area.height - 1,
                if area.width < 14 {
                    "q quit"
                } else if area.width < 36 {
                    "? help  q quit"
                } else if area.width < 72 {
                    "[ ] views  1–4 jump  ? help  q quit"
                } else {
                    "[ ] previous / next   1–4 jump   space pause   ? help   q quit"
                },
                DIM,
            );
        }
        if app.help || app.tour {
            guide(&mut s, app);
        }
        if app.editing_rename.is_some() {
            rename_view(&mut s, app);
        }
        if app.editing_recording {
            recording_view(&mut s, app);
        }
    }
}

fn transition_content(s: &mut Surface<'_>, area: Rect, progress: f32, direction: i32) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let shift = ((1.0 - progress).powi(2) * 4.0).ceil() as i32 * direction;
    let saved = s.buf.clone();
    s.fill(area, BG);
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let source = x as i32 - shift;
            if source >= area.x as i32 && source < area.right() as i32 {
                let mut cell = saved[(source as u16, y)].clone();
                if let Color::Rgb(r, g, b) = cell.fg {
                    let alpha = 0.65 + 0.35 * progress;
                    cell.fg = Color::Rgb(
                        (r as f32 * alpha) as u8,
                        (g as f32 * alpha) as u8,
                        (b as f32 * alpha) as u8,
                    );
                }
                s.buf[(x, y)] = cell;
            }
        }
    }
}

fn title(s: &mut Surface<'_>, area: Rect, name: &str, subtitle: &str) {
    s.bold(area.x, area.y, name, TEXT);
    if area.width >= 84 {
        s.text(area.x + 22, area.y, subtitle, DIM);
    }
}

fn empty_view(s: &mut Surface<'_>, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    s.bold(area.x, area.y, empty_title(app), MINT);
    for (i, line) in wrap_line(empty_hint(app), area.width as usize)
        .into_iter()
        .enumerate()
    {
        if i as u16 + 2 < area.height {
            s.text(area.x, area.y + 2 + i as u16, line, DIM);
        }
    }
}

fn session_identity(s: &mut Surface<'_>, app: &App, area: Rect) {
    if let Some(session) = app.selected_session() {
        let suffix = format!(" · #{}", session.pid());
        let name = app
            .display_label(session)
            .chars()
            .take(area.width.saturating_sub(suffix.chars().count() as u16 + 2) as usize)
            .collect::<String>();
        s.bold(
            area.x,
            area.y,
            format!(
                "{} {name}{suffix}",
                if app.pins.contains(&session.key) {
                    "*"
                } else {
                    "◉"
                }
            ),
            color(session),
        );
    }
}

// A sample-driven sweep is a receipt for an observed sample, not a second clock.
// It never schedules frames or advances when readings are held or unavailable.
fn core_sweep(app: &App) -> Option<usize> {
    (!app.no_motion && !app.paused && !app.stale() && !app.resuming)
        .then_some(app.sample_tick as usize % 72)
}

fn centered(s: &mut Surface<'_>, center: u16, y: u16, value: &str, tint: Color) {
    s.bold(
        center.saturating_sub(value.chars().count() as u16 / 2),
        y,
        value,
        tint,
    );
}

fn meter(s: &mut Surface<'_>, x: u16, y: u16, width: u16, value: Option<f64>, tint: Color) {
    let filled = value.map(|value| (value.clamp(0.0, 100.0) / 100.0 * width as f64).round() as u16);
    for column in 0..width {
        let active = filled.is_some_and(|filled| column < filled);
        s.text(
            x + column,
            y,
            if active { "━" } else { "─" },
            if active { tint } else { LINE },
        );
    }
}

fn headroom_tint(app: &App) -> Color {
    if app.paused || app.stale() || app.resuming {
        return DIM;
    }
    match app.insights.headroom().level {
        HeadroomLevel::Critical => RED,
        HeadroomLevel::Warning => PEACH,
        HeadroomLevel::Unknown => DIM,
        HeadroomLevel::Healthy => CYAN,
    }
}

fn headroom_core(s: &mut Surface<'_>, app: &App, center: u16, y: u16) {
    let tint = headroom_tint(app);
    let available = app.insights.headroom().available_percent;
    let sweep = core_sweep(app);
    centered(s, center, y, "MEMORY HEADROOM", DIM);
    let cy = y + 6;
    // A fixed 72-segment arc encodes available memory. The inner contour is
    // static; only the small outer sample marker moves with an accepted sample.
    for segment in 0..72 {
        let angle = segment as f64 / 72.0 * std::f64::consts::TAU - std::f64::consts::FRAC_PI_2;
        let x = (center as f64 + angle.cos() * 17.0).round() as u16;
        let yy = (cy as f64 + angle.sin() * 5.0).round() as u16;
        let active = available.is_some_and(|percent| segment as f64 / 72.0 < percent / 100.0);
        s.text(x, yy, "•", if active { tint } else { LINE });
        if sweep == Some(segment) {
            let x = (center as f64 + angle.cos() * 19.0).round() as u16;
            let yy = (cy as f64 + angle.sin() * 5.5).round() as u16;
            s.text(x, yy, "◆", CYAN);
        }
    }
    s.text(center - 8, cy - 3, "╭───────────────╮", LINE);
    s.text(center - 8, cy + 3, "╰───────────────╯", LINE);
    if let Some(percent) = available {
        const DIGITS: [[&str; 3]; 10] = [
            ["█▀█", "█ █", "█▄█"],
            [" ▄█", "  █", "  █"],
            ["▀▀█", "█▀▀", "█▄▄"],
            ["▀▀█", " ▀█", "▄▄█"],
            ["█ █", "▀▀█", "  █"],
            ["█▀▀", "▀▀█", "▄▄█"],
            ["█▀▀", "█▀█", "█▄█"],
            ["▀▀█", "  █", "  █"],
            ["█▀█", "█▀█", "█▄█"],
            ["█▀█", "▀▀█", "▄▄█"],
        ];
        let number = format!("{percent:.0}");
        let x = center.saturating_sub((number.len() as u16 * 4 - 1) / 2);
        for (column, digit) in number.bytes().enumerate() {
            for row in 0..3 {
                s.text(
                    x + column as u16 * 4,
                    cy - 1 + row,
                    DIGITS[(digit - b'0') as usize][row as usize],
                    tint,
                );
            }
        }
        centered(s, center, cy + 4, &format!("{percent:.0}% AVAILABLE"), DIM);
    } else {
        centered(s, center, cy, "unavailable", DIM);
    }
    let state = if app.resuming {
        "SYNCING · last reading"
    } else if app.stale() {
        "STALE · last reading"
    } else if app.paused {
        "PAUSED · held reading"
    } else {
        match app.insights.headroom().level {
            HeadroomLevel::Critical => "VERY LOW HEADROOM",
            HeadroomLevel::Warning => "LOW HEADROOM",
            HeadroomLevel::Unknown => "availability unknown",
            HeadroomLevel::Healthy => "HEADROOM AVAILABLE",
        }
    };
    centered(s, center, y + 13, state, tint);
}

fn overview_telemetry(s: &mut Surface<'_>, app: &App, area: Rect) {
    let system = &app.snapshot.system;
    let side = (area.width / 3).saturating_sub(6).min(32);
    let left = area.x + 1;
    let right = area.right() - side - 1;
    let compact = area.height < 30;
    let y = area.y + if compact { 3 } else { 4 };
    let available = (system.memory_total > 0).then_some(100.0 - system.memory_percent());
    let tint = headroom_tint(app);
    headroom_core(
        s,
        app,
        area.x + area.width / 2,
        area.y + if compact { 1 } else { 2 },
    );

    s.text(left, y, "MEMORY AVAILABLE", DIM);
    s.bold(
        left,
        y + 2,
        if system.memory_total == 0 {
            "unavailable".into()
        } else {
            bytes(system.memory_available)
        },
        tint,
    );
    meter(s, left, y + 4, side, available, tint);
    s.text(
        left,
        y + 6,
        format!(
            "{} total · {} used",
            bytes(system.memory_total),
            bytes(system.memory_used())
        )
        .chars()
        .take(side as usize)
        .collect::<String>(),
        DIM,
    );
    s.text(left, y + 9, "SCOPE TOTAL · RSS", DIM);
    let rss = app
        .snapshot
        .sessions
        .iter()
        .fold(0u64, |sum, session| sum.saturating_add(session.rss_bytes()));
    s.bold(left, y + 10, bytes(rss), TEXT);
    s.text(
        left,
        y + 11,
        format!("{} families · unfiltered", app.snapshot.sessions.len()),
        DIM,
    );

    s.text(right, y, "SYSTEM CPU", DIM);
    s.bold(right, y + 2, cpu(system.cpu_percent), CYAN);
    meter(
        s,
        right,
        y + 4,
        side,
        system.cpu_percent.map(f64::from),
        CYAN,
    );
    s.text(
        right,
        y + 6,
        format!("{} logical cores · 0–100%", system.cpu_count),
        DIM,
    );
    s.text(right, y + 9, "SWAP IN USE", DIM);
    s.bold(right, y + 10, bytes(system.swap_used), TEXT);
    s.text(
        right,
        y + 11,
        format!("{} total", bytes(system.swap_total)),
        DIM,
    );
    let note = "Available-memory heuristic · not native OS pressure";
    centered(
        s,
        area.x + area.width / 2,
        area.y + if compact { 15 } else { 17 },
        note,
        DIM,
    );
    s.rule(area.x, area.y + if compact { 16 } else { 18 }, area.width);
}

fn overview(s: &mut Surface<'_>, app: &App, area: Rect, hitboxes: &mut Vec<(Rect, HitTarget)>) {
    title(
        s,
        area,
        "Overview",
        "SYSTEM TELEMETRY  /  LOCAL OBSERVATIONS",
    );
    let roomy = area.width >= 66 && area.height >= 12;
    let system = &app.snapshot.system;
    let core_layout = area.width >= 104 && area.height >= 25;
    let list_y = if core_layout {
        overview_telemetry(s, app, area);
        area.y + if area.height < 30 { 17 } else { 19 }
    } else if roomy {
        let col = area.width / 3;
        let level = app.insights.headroom().level;
        let headroom = match level {
            HeadroomLevel::Critical => RED,
            HeadroomLevel::Warning => PEACH,
            HeadroomLevel::Unknown => DIM,
            HeadroomLevel::Healthy => CYAN,
        };
        for (i, label, value, tint, note) in [
            (
                0,
                "MEMORY AVAILABLE",
                if system.memory_total == 0 {
                    "unavailable".into()
                } else {
                    bytes(system.memory_available)
                },
                headroom,
                if system.memory_total == 0 {
                    "availability unknown".into()
                } else {
                    format!(
                        "{} · {:.0}% available",
                        level.label(),
                        100.0 - system.memory_percent()
                    )
                },
            ),
            (
                1,
                "SYSTEM CPU",
                cpu(system.cpu_percent),
                CYAN,
                format!("{} logical cores", system.cpu_count),
            ),
            (
                2,
                "SCOPE TOTAL · RSS",
                bytes(app.snapshot.sessions.iter().map(|s| s.rss_bytes()).sum()),
                BLUE,
                format!("{} families · unfiltered", app.snapshot.sessions.len()),
            ),
        ] {
            let x = area.x + i * col;
            s.text(x, area.y + 3, label, DIM);
            s.bold(x, area.y + 5, value, tint);
            s.text(
                x,
                area.y + 7,
                note.chars()
                    .take(col.saturating_sub(2) as usize)
                    .collect::<String>(),
                DIM,
            );
        }
        area.y + 10
    } else {
        if area.height > 2 {
            s.text(
                area.x,
                area.y + 1,
                format!(
                    "{} available · CPU {}",
                    if system.memory_total == 0 {
                        "unavailable".into()
                    } else {
                        bytes(system.memory_available)
                    },
                    cpu(system.cpu_percent)
                ),
                DIM,
            );
        }
        area.y + 3
    };
    if list_y >= area.bottom() {
        return;
    }
    if app.sessions().is_empty() {
        empty_view(
            s,
            app,
            Rect::new(
                area.x,
                list_y,
                area.width,
                area.bottom().saturating_sub(list_y),
            ),
        );
        return;
    }
    let wide = area.width >= 66;
    if wide {
        s.text(
            area.x,
            list_y,
            format!("SESSIONS  {:02}", app.sessions().len()),
            DIM,
        );
        s.text(
            area.right() - 34,
            list_y,
            match app.sort {
                Sort::Memory => "RSS ↓",
                Sort::Cpu => "RSS",
                Sort::Growth => "RSS",
            },
            DIM,
        );
        s.text(
            area.right() - 20,
            list_y,
            if app.sort == Sort::Cpu {
                "CPU ↓"
            } else {
                "CPU"
            },
            DIM,
        );
        s.text(
            area.right() - 9,
            list_y,
            if app.sort == Sort::Growth {
                "GROWTH ↓"
            } else {
                "PROCS"
            },
            DIM,
        );
    }
    let start_y = list_y + u16::from(wide) * 2;
    let row_h = if core_layout && area.height < 30 {
        2
    } else if roomy && area.height >= 24 {
        3
    } else if roomy {
        2
    } else {
        1
    };
    let available = area.bottom().saturating_sub(start_y);
    if available == 0 {
        return;
    }
    let visible = (available / row_h).max(1) as usize;
    let sessions = app.sessions();
    let selected = sessions
        .iter()
        .position(|s| Some(s.key) == app.selected)
        .unwrap_or(0);
    let start = selected.saturating_sub(visible.saturating_sub(1));
    for (i, session) in sessions.iter().skip(start).take(visible).enumerate() {
        let y = start_y + i as u16 * row_h;
        let active = Some(session.key) == app.selected;
        if active {
            // Highlight the actual text baseline, not the spacing reserved
            // between families. Keep the larger row hitbox for easy selection.
            s.fill(Rect::new(area.x, y, area.width, 1), SELECT);
        }
        let identity = format!(" #{}", session.key.pid);
        let tail = if wide {
            38
        } else {
            bytes(session.rss_bytes()).chars().count() as u16 + 2
        };
        let name_w =
            area.width
                .saturating_sub(tail + identity.chars().count() as u16 + 4) as usize;
        s.text(
            area.x + 1,
            y,
            format!(
                "{}{} {}{}",
                if active { "›" } else { " " },
                if app.pins.contains(&session.key) {
                    "*"
                } else {
                    " "
                },
                app.display_label(session)
                    .chars()
                    .take(name_w)
                    .collect::<String>(),
                identity
            ),
            if active { color(session) } else { TEXT },
        );
        if wide {
            s.text(area.right() - 34, y, bytes(session.rss_bytes()), TEXT);
            s.text(area.right() - 20, y, cpu(session.cpu_percent()), DIM);
            s.text(
                area.right() - 9,
                y,
                if app.sort == Sort::Growth {
                    signed_bytes(app.growth_delta(session))
                } else {
                    session.process_count().to_string()
                },
                DIM,
            );
        } else {
            let rss = bytes(session.rss_bytes());
            s.text(area.right().saturating_sub(rss.len() as u16), y, rss, DIM);
        }
        hitboxes.push((
            Rect::new(area.x, y, area.width, row_h.min(area.bottom() - y)),
            HitTarget::Session(session.key),
        ));
    }
}

fn process_map(s: &mut Surface<'_>, app: &App, area: Rect, hitboxes: &mut Vec<(Rect, HitTarget)>) {
    title(
        s,
        area,
        "Process Map",
        &format!(
            "{} · g switch graph{}",
            app.graph_mode.label(),
            if app.graph_mode == GraphMode::Heatmap {
                " · m RAM/CPU"
            } else {
                ""
            }
        ),
    );
    if area.height <= 2 {
        return;
    }
    let gap = if area.height < 12 { 1 } else { 2 };
    let content = Rect::new(area.x, area.y + gap, area.width, area.height - gap);
    let Some(session) = app.selected_session() else {
        empty_view(s, app, content);
        return;
    };
    session_identity(s, app, content);
    if content.height <= 2 {
        return;
    }
    let body = Rect::new(
        content.x,
        content.y + gap,
        content.width,
        content.height - gap,
    );
    if app.focused {
        if body.width >= 100 && body.height >= 12 {
            let map_w = body.width * 52 / 100;
            graph(
                s,
                app,
                session,
                Rect::new(body.x, body.y, map_w - 3, body.height),
                hitboxes,
            );
            process_table(
                s,
                app,
                Rect::new(body.x + map_w, body.y, body.width - map_w, body.height),
                hitboxes,
            );
        } else {
            process_table(s, app, body, hitboxes);
        }
    } else {
        graph(s, app, session, body, hitboxes);
    }
}

fn graph(
    s: &mut Surface<'_>,
    app: &App,
    session: &SessionSnapshot,
    area: Rect,
    hitboxes: &mut Vec<(Rect, HitTarget)>,
) {
    match app.graph_mode {
        GraphMode::Constellation => {
            constellation(s, session, area, true, hitboxes, app.display_label(session))
        }
        GraphMode::Heatmap => heatmap(s, app, area, hitboxes),
    }
}

fn resource_value(process: &crate::model::ProcessSnapshot, metric: Metric) -> Option<f64> {
    match metric {
        Metric::Rss => Some(process.rss_bytes as f64),
        Metric::Cpu => process
            .cpu_percent
            .filter(|v| v.is_finite() && *v >= 0.0)
            .map(f64::from),
    }
}

fn heat_color(value: Option<f64>, maximum: f64) -> Color {
    let Some(value) = value else {
        return PANEL;
    };
    let fraction = if maximum > 0.0 {
        (value / maximum).clamp(0.0, 1.0)
    } else {
        0.0
    };
    Color::Rgb(
        (21.0 + 32.0 * fraction) as u8,
        (32.0 + 65.0 * fraction) as u8,
        (39.0 + 43.0 * fraction) as u8,
    )
}

fn heatmap(s: &mut Surface<'_>, app: &App, area: Rect, hitboxes: &mut Vec<(Rect, HitTarget)>) {
    if area.height == 0 || area.width < 12 {
        return;
    }
    let processes = app.heatmap_processes();
    let maximum = processes
        .iter()
        .filter_map(|p| resource_value(p, app.metric))
        .fold(0.0, f64::max);
    let max_label = match app.metric {
        Metric::Rss => bytes(maximum as u64),
        Metric::Cpu => format!("{maximum:.0}%"),
    };
    s.bold(
        area.x,
        area.y,
        format!("HEAT MAP · {}", app.metric.label()),
        MINT,
    );
    if area.height > 1 {
        s.text(
            area.x,
            area.y + 1,
            format!("0 → {max_label} · relative to family max"),
            DIM,
        );
    }
    if area.height < 4 {
        return;
    }
    s.text(area.x, area.y + 2, "low", DIM);
    for step in 0..5 {
        s.fill(
            Rect::new(area.x + 4 + step * 2, area.y + 2, 2, 1),
            heat_color(Some(step as f64), 4.0),
        );
    }
    s.text(area.x + 15, area.y + 2, "high · relative", DIM);
    let compact = area.height < 10 || area.width < 50;
    let row_h = if compact { 1 } else { 4 };
    let columns = if compact {
        1
    } else {
        (area.width / 27).clamp(1, 4)
    };
    let card_w = area.width / columns;
    let grid_y = area.y + 3;
    let capacity = (area.bottom().saturating_sub(grid_y + 1) / row_h * columns).max(1) as usize;
    let selected_index = processes
        .iter()
        .position(|p| Some(p.key) == app.explorer.selected())
        .unwrap_or(0);
    let start = if selected_index >= capacity {
        selected_index / capacity * capacity
    } else {
        0
    };
    for (i, process) in processes.iter().skip(start).take(capacity).enumerate() {
        let x = area.x + i as u16 % columns * card_w;
        let y = grid_y + i as u16 / columns * row_h;
        if y >= area.bottom().saturating_sub(1) {
            break;
        }
        let rect = Rect::new(
            x,
            y,
            card_w.saturating_sub(u16::from(columns > 1)),
            row_h.saturating_sub(u16::from(!compact)),
        );
        let value = resource_value(process, app.metric);
        let value_label = value
            .map(|v| match app.metric {
                Metric::Rss => bytes(v as u64),
                Metric::Cpu => format!("{v:.0}%"),
            })
            .unwrap_or_else(|| "unknown".into());
        s.fill(rect, heat_color(value, maximum));
        let selected = Some(process.key) == app.explorer.selected();
        if compact {
            let suffix = format!(" #{} {value_label}", process.key.pid);
            let name_w = rect.width.saturating_sub(suffix.chars().count() as u16 + 2) as usize;
            s.text(
                x,
                y,
                format!(
                    "{} {}{suffix}",
                    if selected { "›" } else { " " },
                    process.name.chars().take(name_w).collect::<String>()
                ),
                TEXT,
            );
        } else {
            s.bold(
                x + 1,
                y,
                format!(
                    "{} {}",
                    if selected { "›" } else { " " },
                    process
                        .name
                        .chars()
                        .take(rect.width.saturating_sub(4) as usize)
                        .collect::<String>()
                ),
                TEXT,
            );
            s.text(x + 2, y + 1, format!("#{}", process.key.pid), TEXT);
            s.text(
                x + 2,
                y + 2,
                value_label,
                if value.is_none() { DIM } else { MINT },
            );
        }
        hitboxes.push((rect, HitTarget::Process(process.key)));
    }
    let count = processes.len().saturating_sub(start).min(capacity);
    if area.height >= 4 {
        s.text(
            area.x,
            area.bottom() - 1,
            if processes.len() > count {
                format!("{count}/{} shown · ↑↓ browse", processes.len())
            } else if app.metric == Metric::Cpu {
                "CPU 100% = one core · unknown ≠ zero".into()
            } else {
                "Equal tiles · color = RSS, not OS pressure".into()
            },
            DIM,
        );
    }
}

fn history_view(s: &mut Surface<'_>, app: &App, area: Rect) {
    title(
        s,
        area,
        "History",
        "Recent samples, not a recording or replay.",
    );
    if area.height <= 2 {
        return;
    }
    let gap = if area.height < 12 { 1 } else { 2 };
    let content = Rect::new(area.x, area.y + gap, area.width, area.height - gap);
    let Some(session) = app.selected_session() else {
        empty_view(s, app, content);
        return;
    };
    session_identity(s, app, content);
    if content.height < 4 {
        return;
    }
    let (values, low, high, label, tint) = match app.metric {
        Metric::Rss => {
            let values: Vec<_> = session.history.iter().map(|v| Some(*v as f64)).collect();
            let low = values
                .iter()
                .flatten()
                .copied()
                .reduce(f64::min)
                .unwrap_or(0.0);
            let high = values
                .iter()
                .flatten()
                .copied()
                .reduce(f64::max)
                .unwrap_or(0.0);
            (values, low, high, "RAM TRAIL · RSS", color(session))
        }
        Metric::Cpu => {
            let values: Vec<_> = session
                .cpu_history
                .iter()
                .map(|v| v.filter(|v| v.is_finite() && *v >= 0.0).map(f64::from))
                .collect();
            let high = values.iter().flatten().copied().fold(100.0, f64::max);
            (values, 0.0, high, "CPU TRAIL", LILAC)
        }
    };
    let roomy = content.height >= 10;
    let chart_top = content.y + if roomy { 2 } else { 1 };
    let chart_h = content
        .height
        .saturating_sub(if roomy { 8 } else { 4 })
        .max(1);
    trace_chart(
        s,
        Rect::new(area.x, chart_top, area.width, chart_h),
        label,
        &values,
        low,
        high,
        tint,
    );
    if roomy {
        let scale = if app.metric == Metric::Rss {
            format!(
                "{} – {} · auto scale",
                bytes(low as u64),
                bytes(high as u64)
            )
        } else {
            format!("0–{high:.0}% · 100% = one logical core · blank = unknown")
        };
        s.text(area.x, chart_top + chart_h, scale, DIM);
        let y = area.bottom() - 3;
        let growth = app.insights.growth(session.key);
        let change = if let Some(growth) = growth.filter(|g| g.sample_count >= 2) {
            format!(
                "RSS {} in {:.0}s · {:.1} MiB/min",
                signed_bytes(growth.delta_bytes),
                growth.elapsed_seconds.unwrap_or(0.0),
                growth.bytes_per_second.unwrap_or(0.0) * 60.0 / (1_u64 << 20) as f64
            )
        } else {
            "Timed growth warming · needs comparable samples".into()
        };
        s.text(area.x, y, change, DIM);
        s.text(
            area.x,
            y + 1,
            format!(
                "{} of 60 samples · RSS may overlap shared pages",
                values.len()
            ),
            DIM,
        );
        s.text(
            area.x,
            y + 2,
            "Sample order, not a time axis. Pauses may leave gaps; no disk log.",
            FAINT,
        );
    } else {
        if content.height >= 6 {
            s.text(
                area.x,
                area.bottom() - 3,
                if app.metric == Metric::Rss {
                    format!("{}–{} · auto", bytes(low as u64), bytes(high as u64))
                } else {
                    format!("0–{high:.0}% · blank = unknown")
                },
                DIM,
            );
            s.text(
                area.x,
                area.bottom() - 2,
                format!("{} / 60 samples · sample order", values.len()),
                DIM,
            );
            s.text(
                area.x,
                area.bottom() - 1,
                "Gaps possible · not a recording",
                DIM,
            );
        } else {
            s.text(
                area.x,
                area.bottom() - 1,
                format!("{} / 60 · sample order", values.len()),
                DIM,
            );
        }
    }
}

fn trace_chart(
    s: &mut Surface<'_>,
    area: Rect,
    label: &str,
    values: &[Option<f64>],
    low: f64,
    high: f64,
    tint: Color,
) {
    if area.height == 0 {
        return;
    }
    s.text(area.x, area.y, label, DIM);
    if area.height < 2 || area.width == 0 {
        return;
    }
    let plot_y = area.y + if area.height >= 5 { 2 } else { 1 };
    let plot_h = area.bottom() - plot_y;
    if values.is_empty() {
        return;
    }
    // Expand sample steps to the available width without interpolating values
    // or implying a wall-clock time axis. Only measured values get a glyph:
    // unknown samples stay blank, while measured zero still gets a minimum bar
    // so the distinction survives terminals with color disabled.
    for column in 0..area.width {
        let index = column as usize * values.len() / area.width as usize;
        let Some(value) = values[index] else {
            continue;
        };
        let x = area.x + column;
        let units = (((value - low) / (high - low).max(1.0)).clamp(0.0, 1.0)
            * plot_h.saturating_mul(8).saturating_sub(1) as f64)
            .round() as u16
            + 1;
        for row in 0..plot_h {
            let n = units.saturating_sub(row * 8).min(8);
            if n > 0 {
                s.text(
                    x,
                    area.bottom() - 1 - row,
                    ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'][(n - 1) as usize].to_string(),
                    tint,
                );
            }
        }
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
        format!("{pane} #{}", selected.unwrap_or(session.key).pid),
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
    label: &str,
) {
    if area.width < 12 || area.height < 3 {
        return;
    }
    let c = color(session);
    let ink = c;
    s.bold(area.x + 1, area.y, "CONSTELLATION", ink);
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
            let name = if root { label } else { &process.name };
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

fn empty_title(app: &App) -> &'static str {
    if app.attachment_notice().is_some() {
        "Attached root not observed."
    } else if !app.query.is_empty() {
        "No filter matches."
    } else if app.snapshot.all_processes {
        "No accessible families."
    } else {
        "Nothing in orbit. Yet."
    }
}
fn empty_hint(app: &App) -> &'static str {
    if app.attachment_notice().is_some() {
        "Identity is locked; no automatic reattach. The cause is unknown. Quit and choose a current --pid to start again."
    } else if !app.query.is_empty() && app.focused {
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
        "1–4 jumps to Overview, Process Map, History, and Changes. [ and ] move between views. Click a tab; horizontal wheel events work when your terminal sends them.",
        TEXT,
    ),
    (
        "j/k selects a family. Enter opens its inspector; i hides it. Tab switches family/process focus. Inspector left/right or h/l folds branches. E expands all; C collapses all.",
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
        "q quits except while typing a search or nickname. Ctrl-C always quits. ? or Esc closes this guide.",
        TEXT,
    ),
    ("LOCAL HACKATHON RECORDING", BLUE),
    (
        "Start with ./lattice --record CalHacks, or Shift-R and a name. Shift-R stops and saves. Keep the app open: it is not a daemon and cannot record while closed. Space pauses both sampling and recording, leaving an explicit gap.",
        TEXT,
    ),
    (
        "By default one observed CPU/RAM sample is saved every 5 seconds, with up to 32 family contributors. Review recorded-sample peaks/means and gaps using recordings list, recordings show FILE, or recordings export FILE --output report.csv. Restart creates a separate file, never resumes silently.",
        TEXT,
    ),
    (
        "Recording files are local, bounded, and recover complete records after an interrupted write. Demo recordings are labeled simulated and kept separately. Recorded values are snapshots, not total work or energy. Disk errors stop recording visibly, not monitoring.",
        TEXT,
    ),
    ("MAKE IT YOUR WORKSPACE", MINT),
    (
        "r names the selected session for this run. Names are bounded, terminal-safe, and tied to PID/start identity, not inherited when the start identity changes. Empty name restores the original. At most 128 names are retained.",
        TEXT,
    ),
    (
        "e opens What Changed: observed appearances, disappearances, and RSS growth between comparable samples. It is memory-only, keeps 128 events, and does not infer causes or leaks. Enter inspects a family still present.",
        TEXT,
    ),
    (
        "Growth is significant at max(32 MiB, 5% of previous family RSS) in one sample interval. Largest signed process RSS changes are contributors to the observed total, not evidence of a cause.",
        TEXT,
    ),
    (
        "./lattice --pid 12345 attaches to an existing owned process and its observed descendants, including builds or dev servers. Find its PID with --all. No commands are launched; no process is controlled.",
        TEXT,
    ),
    (
        "PID mode locks the first observed start identity and disables scope switching. If the root is no longer accessible or its identity changes, it never silently reattaches. Start times have one-second precision.",
        TEXT,
    ),
    ("CHOOSE YOUR LENS", LILAC),
    (
        "In Process Map, g switches constellation and heat map. m switches RSS/CPU in the heat map and History. Graph and metric choices stay set as you navigate. Heat-map up/down keys select processes; Tab switches process/family focus. Selecting a folded-away process reveals only its ancestors.",
        TEXT,
    ),
    (
        "Heat-map tiles have equal size. Color is linear from zero to the maximum observed process value in this family, not temperature or OS memory pressure. Unknown CPU is labeled, never treated as zero. Inspector browses every process.",
        TEXT,
    ),
    ("READ THE CONSTELLATIONS", LILAC),
    (
        "Lines mean parent to child, not network traffic. Each PID belongs to one session. Nested agents form separate sessions, except inside an explicit PID family.",
        TEXT,
    ),
    (
        "A filled process dot uses over 15% of a core; a hollow dot is quieter. Omitted nodes are counted, not reattached.",
        TEXT,
    ),
    (
        "View transitions last 160ms and stop. --no-motion switches instantly and disables the Overview sample marker. The status dot stays still. Sampling does not speed up during navigation. Trackpad gestures depend on terminal support.",
        TEXT,
    ),
    (
        "The Overview arc shows available-memory percentage. Its outer marker steps with an accepted sample, not clock time. Paused, stale, and syncing readings are held and labeled. Headroom is a heuristic, not native OS pressure.",
        TEXT,
    ),
    (
        "SCOPE TOTAL includes all sessions in the current scope, even those hidden by search. System CPU and headroom are machine-wide.",
        TEXT,
    ),
    (
        "Inspector rows are individual processes. History holds up to 60 resource samples for the selected family, in sample order, not a full recorder or replay. Pauses may leave gaps.",
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
        "No process control, argument display, or telemetry. Recording is opt-in and writes local metric files. The OS backend may read argument buffers transiently on macOS.",
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
    s.fill(s.area, BG);
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

fn signed_bytes(delta: i64) -> String {
    format!(
        "{}{}",
        if delta >= 0 { "+" } else { "−" },
        bytes(delta.unsigned_abs())
    )
}
fn recording_view(s: &mut Surface<'_>, app: &App) {
    s.fill(s.area, BG);
    let width = s.area.width.saturating_sub(4).clamp(1, 78);
    let height = s.area.height.saturating_sub(2).clamp(1, 16);
    let r = Rect::new(
        s.area.width.saturating_sub(width) / 2,
        s.area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    s.fill(r, PANEL);
    if width < 10 || height < 5 {
        s.text(0, 0, "^C quit", DIM);
        return;
    }
    s.bold(
        r.x + 2,
        r.y,
        if app.snapshot.demo {
            "START SIMULATED RECORDING"
        } else {
            "START LOCAL RECORDING"
        },
        BLUE,
    );
    s.text(r.x + 2, r.y + 1, "Name this run (e.g. CalHacks)", DIM);
    s.bold(r.x + 2, r.y + 2, format!("{}▏", app.recording_name), TEXT);
    let copy = "Local CPU/RAM samples + safe family names. Keep lattice open; no daemon or recording while closed. Space leaves a gap. R stops and saves. No args, paths, or telemetry. Review later: lattice recordings list.";
    for (i, line) in wrap_line(copy, width.saturating_sub(4) as usize)
        .into_iter()
        .enumerate()
    {
        if 4 + (i as u16) < height.saturating_sub(2) {
            s.text(r.x + 2, r.y + 4 + i as u16, line, DIM);
        }
    }
    s.text(
        r.x + 2,
        r.bottom() - 1,
        "enter start  esc cancel  ^C quit",
        DIM,
    );
}

fn rename_view(s: &mut Surface<'_>, app: &App) {
    s.fill(s.area, BG);
    let Some(key) = app.editing_rename else {
        return;
    };
    let width = s.area.width.saturating_sub(4).clamp(1, 66);
    let height = s.area.height.saturating_sub(2).clamp(1, 10);
    let r = Rect::new(
        s.area.width.saturating_sub(width) / 2,
        s.area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    s.fill(r, PANEL);
    if width < 10 || height < 5 {
        return;
    }
    let inner = width.saturating_sub(4) as usize;
    s.bold(r.x + 2, r.y, format!("NAME SESSION · #{}", key.pid), MINT);
    s.text(r.x + 2, r.y + 1, "This run only · 24 safe characters", DIM);
    s.bold(r.x + 2, r.y + 3, format!("{}▏", app.rename_text), TEXT);
    let hint = "Letters, digits, spaces, . _ - +. Empty restores the original name.";
    for (i, line) in wrap_line(hint, inner).into_iter().enumerate() {
        if 5 + (i as u16) < height.saturating_sub(1) {
            s.text(r.x + 2, r.y + 5 + i as u16, line, DIM);
        }
    }
    s.text(
        r.x + 2,
        r.bottom() - 1,
        "enter save  esc cancel  ^C quit",
        DIM,
    );
}
fn changes_view(s: &mut Surface<'_>, app: &App, area: Rect) {
    if area.height < 2 || area.width < 12 {
        return;
    }
    let entries: Vec<_> = app.changes.events().iter().rev().collect();
    s.bold(
        area.x,
        area.y,
        format!("WHAT CHANGED · {} kept", entries.len()),
        TEXT,
    );
    s.text(
        area.x,
        area.y + 1,
        "Observed, not causes · memory only",
        DIM,
    );
    if entries.is_empty() {
        let message = "No changes observed yet. The first sample primes a baseline. Growth means at least 32 MiB and 5% of previous family RSS in one sample interval. Appearances and disappearances are observations, not exit causes.";
        for (i, line) in wrap_line(message, area.width as usize)
            .into_iter()
            .enumerate()
        {
            if i as u16 + 3 < area.height {
                s.text(area.x, area.y + 3 + i as u16, line, TEXT);
            }
        }
        return;
    }
    let selected = entries
        .iter()
        .position(|event| Some(event.sequence) == app.change_selected)
        .unwrap_or(0);
    let event = entries[selected];
    let wide = area.width >= 100;
    let list_width = if wide { area.width / 2 } else { area.width };
    let compact = area.height < 16;
    let row_height = if compact { 1 } else { 3 };
    let details_h = if compact { 3 } else { 7 };
    let list_height = if wide {
        area.height.saturating_sub(5)
    } else {
        area.height.saturating_sub(details_h + 4)
    };
    let visible = (list_height / row_height).max(1) as usize;
    let start = selected.saturating_sub(visible - 1);
    let list_y = area.y + if compact { 2 } else { 4 };
    for (offset, event) in entries.iter().skip(start).take(visible).enumerate() {
        let y = list_y + offset as u16 * row_height;
        if y >= area.bottom() {
            break;
        }
        let active = offset + start == selected;
        if active {
            s.fill(
                Rect::new(area.x, y, list_width, row_height.min(area.bottom() - y)),
                SELECT,
            );
        }
        let label = app
            .nicknames
            .get(&event.session_key)
            .map(String::as_str)
            .unwrap_or(&event.session_name);
        let tail = format!(
            " #{} {}",
            event.session_key.pid,
            signed_bytes(event.rss_delta_bytes)
        );
        let name_width = (list_width as usize).saturating_sub(tail.chars().count() + 2);
        s.text(
            area.x,
            y,
            format!(
                "{} {}{}",
                if active { "›" } else { " " },
                label.chars().take(name_width).collect::<String>(),
                tail
            ),
            if active { MINT } else { TEXT },
        );
        if row_height > 1 {
            let kind = match event.kind {
                ChangeKind::Appeared => "family first seen",
                ChangeKind::Disappeared => "family not seen",
                ChangeKind::Changed if event.significant_growth => "RSS growth",
                ChangeKind::Changed => "membership changed",
            };
            s.text(
                area.x + 2,
                y + 1,
                format!("t+{:.0}s · {kind}", event.monotonic)
                    .chars()
                    .take(list_width.saturating_sub(2) as usize)
                    .collect::<String>(),
                DIM,
            );
        }
    }
    let detail_x = if wide {
        area.x + list_width + 5
    } else {
        area.x
    };
    let detail_width = if wide {
        area.width - list_width - 5
    } else {
        area.width
    };
    let detail_y = if wide {
        list_y
    } else if compact {
        list_y + visible as u16
    } else {
        area.bottom().saturating_sub(details_h)
    };
    if !compact && detail_y < area.bottom() {
        s.bold(detail_x, detail_y, "SELECTED OBSERVATION", LILAC);
        s.text(
            detail_x,
            detail_y + 2,
            format!(
                "+{} seen / −{} not seen · RSS {}",
                event.appeared,
                event.disappeared,
                signed_bytes(event.rss_delta_bytes)
            )
            .chars()
            .take(detail_width as usize)
            .collect::<String>(),
            DIM,
        );
    }
    let contributor_y = detail_y + if compact { 0 } else { 4 };
    for (i, item) in event.contributors.iter().take(3).enumerate() {
        let y = contributor_y + i as u16;
        if y >= area.bottom() {
            break;
        }
        let status = match item.status {
            ContributorStatus::Present => "delta",
            ContributorStatus::Appeared => "new",
            ContributorStatus::Disappeared => "not seen",
        };
        let prefix = format!("{} ", signed_bytes(item.delta_bytes));
        let suffix = format!(" #{} {status}", item.key.pid);
        let name_width =
            (detail_width as usize).saturating_sub(prefix.chars().count() + suffix.chars().count());
        s.text(
            detail_x,
            y,
            format!(
                "{}{}{}",
                prefix,
                item.name.chars().take(name_width).collect::<String>(),
                suffix
            ),
            TEXT,
        );
    }
    if wide {
        s.text(
            detail_x,
            contributor_y + 5,
            "Top 3 signed RSS contributors",
            DIM,
        );
        let live = app
            .snapshot
            .sessions
            .iter()
            .any(|session| session.key == event.session_key);
        s.text(
            detail_x,
            contributor_y + 7,
            if live {
                "Enter inspects this family"
            } else {
                "Family not in latest sample"
            },
            DIM,
        );
    }
    if area.height >= 16 {
        s.text(
            area.x,
            area.bottom() - 1,
            format!(
                "{}/{} · {} older events discarded · no disk log",
                selected + 1,
                entries.len(),
                app.changes.dropped_events()
            )
            .chars()
            .take(area.width as usize)
            .collect::<String>(),
            FAINT,
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
        assert!(!text.contains("100% available"));
        app.snapshot.demo = false;
        app.last_sample = std::time::Instant::now() - std::time::Duration::from_secs(600);
        assert!(render(144, 44, &app).contains("STALE"));
        assert!(render(40, 16, &app).contains("STALE"));
    }
    #[test]
    fn inspector_is_safe_at_narrow_and_tiny_sizes() {
        let mut app = App::new(DemoSampler::new().sample(false));
        app.view = View::ProcessMap;
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
        app.view = View::ProcessMap;
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
        app.view = View::ProcessMap;
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
        app.view = View::ProcessMap;
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

#[cfg(test)]
mod practical_ui_tests {
    use super::*;
    use crate::{
        model::{AttachmentSnapshot, AttachmentStatus},
        sampling::DemoSampler,
    };
    fn render(w: u16, h: u16, app: &App) -> String {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        Dashboard {
            app,
            hitboxes: &mut Vec::new(),
        }
        .render(area, &mut buf);
        buf.content
            .chunks(w as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn practical_modals_preserve_identity_and_quit_cues_at_supported_sizes() {
        let mut sampler = DemoSampler::new();
        let mut app = App::new(sampler.sample(false));
        for _ in 0..59 {
            app.update(sampler.sample(false));
        }
        for (w, h) in [(40, 16), (58, 19), (72, 22), (80, 24), (144, 44)] {
            app.start_rename();
            app.rename_text = "CalHacks API".into();
            let text = render(w, h, &app);
            assert!(text.contains("NAME SESSION · #4100"), "{w}x{h}\n{text}");
            assert!(text.contains("CalHacks API"));
            assert!(text.contains("^C quit"));
            app.rename_key(crossterm::event::KeyCode::Esc);
            app.open_changes();
            let text = render(w, h, &app);
            for expected in [
                "WHAT CHANGED",
                "Observed, not causes",
                "vite",
                "not seen",
                "q quit",
            ] {
                assert!(text.contains(expected), "{w}x{h}: {expected}\n{text}");
            }
            app.show_changes = false;
        }
    }
    #[test]
    fn event_empty_state_is_actionable_and_tiny_modals_do_not_panic() {
        let mut app = App::new(DemoSampler::new().sample(false));
        app.open_changes();
        let text = render(40, 16, &app);
        assert!(text.contains("No changes observed yet."));
        assert!(text.contains("q quit"));
        for w in 1..80 {
            for h in 1..24 {
                app.editing_rename = None;
                render(w, h, &app);
                app.start_rename();
                render(w, h, &app);
            }
        }
    }
    #[test]
    fn attached_root_loss_is_not_presented_as_generic_agent_empty_state() {
        let mut snapshot = DemoSampler::new().sample(false);
        snapshot.demo = false;
        snapshot.attachment = Some(AttachmentSnapshot {
            requested_pid: 4100,
            root: Some(snapshot.sessions[0].key),
            status: AttachmentStatus::Ended,
        });
        snapshot.sessions.clear();
        let app = App::new(snapshot);
        for (w, h) in [(40, 16), (80, 24), (144, 44)] {
            let text = render(w, h, &app);
            assert!(text.contains("PID UNSEEN"), "{text}");
            assert!(
                text.contains("Root no longer observed")
                    || text.contains("Attached root not observed"),
                "{text}"
            );
            assert!(!text.contains("No agents found"), "{text}");
        }
    }
}

#[cfg(test)]
mod modular_view_tests {
    use super::*;
    use crate::{model::ProcessSnapshot, sampling::DemoSampler};

    fn render(app: &App, width: u16, height: u16) -> (String, Vec<(Rect, HitTarget)>) {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        let mut hits = Vec::new();
        Dashboard {
            app,
            hitboxes: &mut hits,
        }
        .render(area, &mut buffer);
        (
            buffer
                .content
                .chunks(width as usize)
                .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n"),
            hits,
        )
    }

    #[test]
    fn views_are_focused_and_tabs_keep_their_own_hit_targets() {
        let mut app = App::new(DemoSampler::new().sample(false));
        app.no_motion = true;
        for view in View::ALL {
            app.set_view(view);
            let (text, hits) = render(&app, 144, 44);
            assert_eq!(
                hits.iter()
                    .filter(|(_, target)| matches!(target, HitTarget::View(_)))
                    .count(),
                4
            );
            for (rect, _) in hits {
                assert!(Rect::new(0, 0, 144, 44).contains((rect.x, rect.y).into()));
            }
            assert!(text.contains("q quit"));
            assert_eq!(text.contains("SCOPE TOTAL"), view == View::Overview);
            assert_eq!(text.contains("RAM TRAIL"), view == View::History);
            assert_eq!(text.contains("WHAT CHANGED"), view == View::Changes);
            assert!(!text.contains("PROCESS EXPLORER"));
        }
    }

    #[test]
    fn heatmap_values_and_color_scale_distinguish_zero_unknown_and_extremes() {
        let mut process = ProcessSnapshot::default();
        assert_eq!(resource_value(&process, Metric::Rss), Some(0.0));
        assert_eq!(resource_value(&process, Metric::Cpu), None);
        process.cpu_percent = Some(0.0);
        assert_eq!(resource_value(&process, Metric::Cpu), Some(0.0));
        for value in [f32::NAN, f32::INFINITY, -1.0] {
            process.cpu_percent = Some(value);
            assert_eq!(resource_value(&process, Metric::Cpu), None);
        }
        process.rss_bytes = u64::MAX;
        process.cpu_percent = Some(f32::MAX);
        assert!(resource_value(&process, Metric::Rss).unwrap().is_finite());
        assert!(resource_value(&process, Metric::Cpu).unwrap().is_finite());
        assert_ne!(heat_color(None, 0.0), heat_color(Some(0.0), 0.0));
        assert_eq!(heat_color(Some(0.0), 0.0), heat_color(Some(0.0), 100.0));
        assert_eq!(
            heat_color(Some(100.0), 100.0),
            heat_color(Some(1000.0), 100.0)
        );
        assert_ne!(
            heat_color(Some(1.0), 1000.0),
            heat_color(Some(1000.0), 1000.0)
        );
    }

    #[test]
    fn supported_compact_graphs_show_selected_process_and_scale() {
        let mut app = App::new(DemoSampler::new().sample(false));
        app.no_motion = true;
        app.set_view(View::ProcessMap);
        for (width, height) in [(40, 16), (58, 19), (80, 24), (144, 44)] {
            app.graph_mode = GraphMode::Heatmap;
            let (text, _) = render(&app, width, height);
            let pid = app.explorer.selected().unwrap().pid;
            assert!(text.contains(&format!("#{pid}")), "{text}");
            assert!(text.contains("MiB"), "{text}");
            assert!(text.contains("low") && text.contains("high"), "{text}");
            assert!(
                text.contains('›'),
                "selected tile must not rely on color: {text}"
            );
            app.graph_mode = GraphMode::Constellation;
            let (text, _) = render(&app, width, height);
            assert!(text.contains("claude"));
            assert!(
                !text.contains("+7 processes not shown"),
                "root must fit: {text}"
            );
            app.set_view(View::History);
            let (text, _) = render(&app, width, height);
            assert!(text.contains("auto"), "scale missing: {text}");
            assert!(
                text.contains("sample order") || text.contains("Sample order"),
                "{text}"
            );
            app.set_view(View::ProcessMap);
        }
    }

    #[test]
    fn heatmap_uniform_zero_unknown_and_skewed_fixtures_remain_labeled() {
        let mut app = App::new(DemoSampler::new().sample(false));
        app.no_motion = true;
        app.set_view(View::ProcessMap);
        app.graph_mode = GraphMode::Heatmap;
        app.metric = Metric::Cpu;
        let key = app.selected.unwrap();
        for case in 0..4 {
            let session = app
                .snapshot
                .sessions
                .iter_mut()
                .find(|s| s.key == key)
                .unwrap();
            for (index, process) in session.processes.iter_mut().enumerate() {
                process.cpu_percent = match case {
                    0 => Some(0.0),
                    1 => Some(10.0),
                    2 => None,
                    _ => Some(if index == 0 { 1600.0 } else { 0.1 }),
                };
            }
            let (text, _) = render(&app, 144, 44);
            assert!(text.contains("HEAT MAP · CPU"));
            assert!(text.contains("relative to family max"));
            if case == 2 {
                assert!(text.contains("unknown"));
            }
            if case == 3 {
                assert!(text.contains("1600%"));
            }
        }
    }

    #[test]
    fn every_view_and_lens_survives_tiny_resize_with_quit_cue_when_it_fits() {
        let mut app = App::new(DemoSampler::new().sample(false));
        app.no_motion = true;
        for view in View::ALL {
            app.set_view(view);
            for graph in [GraphMode::Constellation, GraphMode::Heatmap] {
                app.graph_mode = graph;
                for (width, height) in [
                    (1, 1),
                    (12, 4),
                    (20, 6),
                    (30, 12),
                    (40, 16),
                    (58, 19),
                    (80, 24),
                    (144, 44),
                    (240, 100),
                ] {
                    let (text, _) = render(&app, width, height);
                    assert!(!text.is_empty());
                    if width >= 12 && height >= 4 {
                        assert!(text.contains("q quit"), "{width}x{height}: {text}");
                    }
                }
            }
        }
    }

    #[test]
    fn trace_chart_gaps_and_zero_have_distinct_glyphs_without_color() {
        for (width, height) in [(4, 2), (12, 4), (8, 5), (16, 8)] {
            let bounds = Rect::new(0, 0, width + 4, height + 2);
            let chart = Rect::new(2, 1, width, height);
            let mut buffer = Buffer::empty(bounds);
            trace_chart(
                &mut Surface {
                    buf: &mut buffer,
                    area: bounds,
                },
                chart,
                "CPU",
                &[None, Some(0.0), Some(100.0), None],
                0.0,
                100.0,
                LILAC,
            );
            let plot_y = chart.y + if height >= 5 { 2 } else { 1 };
            for column in 0..width {
                let sample = column * 4 / width;
                for y in plot_y..chart.bottom() {
                    let expected = match sample {
                        1 if y == chart.bottom() - 1 => "▁",
                        2 => "█",
                        _ => " ",
                    };
                    // Assert symbols, not styles: NO_COLOR removes the tint.
                    assert_eq!(
                        buffer[(chart.x + column, y)].symbol(),
                        expected,
                        "{width}x{height}, sample {sample}, column {column}, row {y}"
                    );
                }
            }
        }
    }

    #[test]
    fn trace_chart_without_measurements_has_no_zero_baseline() {
        for values in [vec![], vec![None], vec![None; 4]] {
            for height in [2, 4, 5, 8] {
                let area = Rect::new(0, 0, 12, height);
                let mut buffer = Buffer::empty(area);
                trace_chart(
                    &mut Surface {
                        buf: &mut buffer,
                        area,
                    },
                    area,
                    "CPU",
                    &values,
                    0.0,
                    100.0,
                    LILAC,
                );
                for y in 1..height {
                    for x in 0..area.width {
                        assert_eq!(buffer[(x, y)].symbol(), " ");
                    }
                }
            }
        }
    }

    #[test]
    fn history_cpu_unknowns_are_not_drawn_as_measured_zero() {
        let mut app = App::new(DemoSampler::new().sample(false));
        app.no_motion = true;
        app.set_view(View::History);
        app.metric = Metric::Cpu;
        let key = app.selected.unwrap();
        let session = app
            .snapshot
            .sessions
            .iter_mut()
            .find(|s| s.key == key)
            .unwrap();
        session.cpu_history = vec![None, Some(0.0), Some(100.0), None];
        let (text, _) = render(&app, 144, 44);
        assert!(text.contains("blank = unknown"));
        assert!(text.contains("Sample order, not a time axis"));
        assert!(text.contains("no disk log"));

        for (width, height) in [(40, 16), (58, 19), (80, 24), (144, 44)] {
            let mut render_history = |values| {
                app.snapshot
                    .sessions
                    .iter_mut()
                    .find(|session| session.key == key)
                    .unwrap()
                    .cpu_history = values;
                render(&app, width, height).0
            };
            let unknown = render_history(vec![None; 4]);
            let zero = render_history(vec![Some(0.0); 4]);
            // The helper returns only glyphs, so color cannot hide a regression.
            assert_ne!(
                unknown, zero,
                "{width}x{height}: unknown must not look idle"
            );
            let invalid = render_history(
                [f32::NAN, f32::INFINITY, -1.0, f32::NEG_INFINITY]
                    .map(Some)
                    .to_vec(),
            );
            assert_eq!(invalid, unknown, "invalid readings must remain gaps");
            assert!(unknown.contains("blank = unknown"));
            assert!(!unknown.contains('▁'));
            assert!(zero.contains('▁'));
        }
    }
}

#[cfg(test)]
mod command_dashboard_tests {
    use super::*;
    use crate::sampling::DemoSampler;
    use std::time::{Duration, Instant};

    fn core(app: &App) -> Buffer {
        let area = Rect::new(0, 0, 48, 18);
        let mut buffer = Buffer::empty(area);
        let mut surface = Surface {
            buf: &mut buffer,
            area,
        };
        headroom_core(&mut surface, app, 24, 1);
        buffer
    }

    #[test]
    fn sample_sweep_is_observation_driven_and_never_a_second_clock() {
        let mut app = App::new(DemoSampler::new().sample(false));
        app.sample_tick = 0;
        assert_eq!(core_sweep(&app), Some(0));
        let first = core(&app);
        assert_eq!(
            first,
            core(&app),
            "repainting the same snapshot must not advance the core"
        );
        app.sample_tick = 18;
        assert_eq!(core_sweep(&app), Some(18));
        assert_ne!(first, core(&app));
    }

    #[test]
    fn reduced_motion_pause_sync_and_stale_disable_the_sweep() {
        for mode in 0..4 {
            let mut app = App::new(DemoSampler::new().sample(false));
            match mode {
                0 => app.no_motion = true,
                1 => app.paused = true,
                2 => app.resuming = true,
                _ => app.last_sample = Instant::now() - Duration::from_secs(30),
            }
            app.sample_tick = 3;
            assert_eq!(core_sweep(&app), None);
            let first = core(&app);
            app.sample_tick = 49;
            assert_eq!(first, core(&app));
        }
    }

    #[test]
    fn held_core_replaces_reassurance_with_explicit_status() {
        for (mode, expected) in [
            (0, "PAUSED · held reading"),
            (1, "STALE · last reading"),
            (2, "SYNCING · last reading"),
        ] {
            let mut app = App::new(DemoSampler::new().sample(false));
            match mode {
                0 => app.paused = true,
                1 => app.last_sample = Instant::now() - Duration::from_secs(30),
                _ => app.resuming = true,
            }
            let buffer = core(&app);
            let text = buffer
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(text.contains(expected));
            assert!(!text.contains("HEADROOM AVAILABLE"));
        }
    }

    #[test]
    fn unavailable_or_stale_headroom_never_looks_healthy() {
        let mut snapshot = DemoSampler::new().sample(false);
        snapshot.system.memory_total = 0;
        let mut app = App::new(snapshot);
        let text = |buffer: Buffer| {
            buffer
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };
        let rendered = text(core(&app));
        assert!(rendered.contains("unavailable"));
        assert!(!rendered.contains("HEADROOM AVAILABLE"));
        app.last_sample = Instant::now() - Duration::from_secs(30);
        assert!(text(core(&app)).contains("STALE · last reading"));
        assert_eq!(headroom_tint(&app), DIM);
    }

    #[test]
    fn shorter_wide_terminal_keeps_core_and_three_families_visible() {
        let app = App::new(DemoSampler::new().sample(false));
        let area = Rect::new(0, 0, 120, 36);
        let mut buffer = Buffer::empty(area);
        let mut hits = Vec::new();
        Dashboard {
            app: &app,
            hitboxes: &mut hits,
        }
        .render(area, &mut buffer);
        assert_eq!(
            hits.iter()
                .filter(|(_, target)| matches!(target, HitTarget::Session(_)))
                .count(),
            3
        );
        let text = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("MEMORY HEADROOM"));
        assert!(text.contains("not native OS pressure"));
    }

    #[test]
    fn standard_dashboard_keeps_all_four_demo_families_visible() {
        let app = App::new(DemoSampler::new().sample(false));
        let area = Rect::new(0, 0, 144, 44);
        let mut buffer = Buffer::empty(area);
        let mut hits = Vec::new();
        Dashboard {
            app: &app,
            hitboxes: &mut hits,
        }
        .render(area, &mut buffer);
        assert_eq!(
            hits.iter()
                .filter(|(_, target)| matches!(target, HitTarget::Session(_)))
                .count(),
            4
        );
        let text = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        for label in [
            "MEMORY HEADROOM",
            "SCOPE TOTAL",
            "SYSTEM CPU",
            "SWAP IN USE",
            "not native OS pressure",
        ] {
            assert!(text.contains(label), "{label}");
        }
    }
}

#[cfg(test)]
mod recording_dashboard_tests {
    use super::*;
    use crate::{recording::RecordingStatus, sampling::DemoSampler};

    fn render(app: &App, width: u16, height: u16) -> String {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        Dashboard {
            app,
            hitboxes: &mut Vec::new(),
        }
        .render(area, &mut buffer);
        buffer
            .content
            .chunks(width as usize)
            .map(|line| line.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn long_recording_name_cannot_hide_stop_or_simulation_label() {
        let mut app = App::new(DemoSampler::new().sample(false));
        app.recording_status = Some(RecordingStatus {
            name: "A long hackathon recording name for this run".into(),
            path: "/unused".into(),
            samples: 123456,
            elapsed_secs: 172800.0,
            bytes_written: 1024,
            demo: true,
            stopped: false,
        });
        for (width, height) in [(40, 16), (58, 19), (80, 24), (144, 44)] {
            let text = render(&app, width, height);
            assert!(text.contains("SIM REC"), "{width}x{height}: {text}");
            assert!(text.contains("48:00:00"), "{width}x{height}: {text}");
            assert!(text.contains("R stop"), "{width}x{height}: {text}");
            assert!(!text.contains("/unused"));
            app.paused = true;
            assert!(render(&app, width, height).contains("REC GAP"));
            app.paused = false;
        }
    }

    #[test]
    fn recording_modal_explains_local_scope_and_limitations_at_compact_size() {
        let mut app = App::new(DemoSampler::new().sample(false));
        app.editing_recording = true;
        app.recording_name = "CalHacks".into();
        for (width, height) in [(40, 16), (58, 19), (80, 24), (144, 44)] {
            let text = render(&app, width, height);
            let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
            for expected in [
                "START SIMULATED RECORDING",
                "CalHacks",
                "Local CPU/RAM",
                "no daemon",
                "recording while closed",
                "Space leaves a gap",
                "R stops and saves",
                "No args, paths, or telemetry",
                "lattice recordings list",
                "enter start",
                "esc cancel",
                "^C quit",
            ] {
                assert!(
                    words.contains(expected),
                    "{width}x{height}: {expected}\n{text}"
                );
            }
        }
        for (width, height) in [(1, 1), (7, 3), (20, 6), (30, 12)] {
            let text = render(&app, width, height);
            if width >= 7 && height < 7 {
                assert!(text.contains("^C quit"));
            }
        }
    }
}

#[cfg(test)]
mod overview_selection_alignment_tests {
    use super::*;
    use crate::sampling::DemoSampler;

    fn text_from(buffer: &Buffer, x: u16, y: u16, width: u16) -> String {
        (x..x + width)
            .map(|column| buffer[(column, y)].symbol())
            .collect()
    }

    #[test]
    fn selected_overview_highlight_shares_the_name_and_metric_baseline() {
        for (width, height, expected_hit_height) in [
            (144, 44, 3),
            (120, 36, 2),
            (80, 28, 2),
            (80, 24, 1),
            (58, 19, 1),
            (40, 16, 1),
        ] {
            let mut app = App::new(DemoSampler::new().sample(false));
            for _ in 0..4 {
                let area = Rect::new(0, 0, width, height);
                let mut buffer = Buffer::empty(area);
                let mut hits = Vec::new();
                Dashboard {
                    app: &app,
                    hitboxes: &mut hits,
                }
                .render(area, &mut buffer);
                let session = app.selected_session().unwrap();
                let row = hits
                    .iter()
                    .find_map(|(rect, target)| match target {
                        HitTarget::Session(key) if *key == session.key => Some(*rect),
                        _ => None,
                    })
                    .expect("selected family must remain visible");
                assert_eq!(
                    row.height, expected_hit_height,
                    "hitbox spacing changed at {width}x{height}"
                );
                let text = text_from(&buffer, row.x, row.y, row.width);
                assert!(
                    text.contains(app.display_label(session)),
                    "session name not on highlight: {text}"
                );
                assert!(
                    text.contains(&format!("#{}", session.key.pid)),
                    "name/PID not on highlight: {text}"
                );
                assert!(
                    text.contains(&bytes(session.rss_bytes())),
                    "RAM not on highlight: {text}"
                );
                if row.width >= 66 {
                    assert!(
                        text_from(&buffer, row.right() - 34, row.y, 14)
                            .starts_with(&bytes(session.rss_bytes()))
                    );
                    assert!(
                        text_from(&buffer, row.right() - 20, row.y, 11)
                            .starts_with(&cpu(session.cpu_percent()))
                    );
                    assert!(
                        text_from(&buffer, row.right() - 9, row.y, 9)
                            .starts_with(&session.process_count().to_string())
                    );
                }
                for y in row.y..row.bottom() {
                    for x in row.x..row.right() {
                        assert_eq!(
                            buffer[(x, y)].bg,
                            if y == row.y { SELECT } else { BG },
                            "selection must cover exactly the text line at {width}x{height}, cell {x},{y}"
                        );
                    }
                }
                // The padded click target still selects the same family; keyboard
                // selection and scrolling must preserve the baseline invariant.
                assert!(row.contains((row.x, row.bottom() - 1).into()));
                app.select(1);
            }
        }
    }
}
