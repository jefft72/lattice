use crate::{
    changes::Changes,
    explorer::Explorer,
    insights::{Insights, rss_delta},
    model::{AttachmentStatus, ProcessKey, ProcessSnapshot, SessionSnapshot, Snapshot},
    recording::{RecordingOptions, RecordingStatus, RecordingUpdate, RecordingWorker, StopReason},
    sampling::{DemoSampler, LiveSampler},
    ui,
};
use crossterm::{
    cursor::Show,
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, enable_raw_mode},
};
use ratatui::layout::Rect;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Sort {
    Memory,
    Cpu,
    Growth,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Focus {
    Sessions,
    Processes,
}

/// Independent screens share selection, filters, pins and the process explorer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum View {
    Overview,
    #[value(name = "map")]
    ProcessMap,
    History,
    Changes,
}

impl View {
    pub const ALL: [Self; 4] = [
        Self::Overview,
        Self::ProcessMap,
        Self::History,
        Self::Changes,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::ProcessMap => "Process Map",
            Self::History => "History",
            Self::Changes => "Changes",
        }
    }

    pub fn index(self) -> usize {
        match self {
            Self::Overview => 0,
            Self::ProcessMap => 1,
            Self::History => 2,
            Self::Changes => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum GraphMode {
    Constellation,
    Heatmap,
}

impl GraphMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Constellation => "Constellation",
            Self::Heatmap => "Heat map",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Metric {
    Rss,
    Cpu,
}

impl Metric {
    pub fn label(self) -> &'static str {
        match self {
            Self::Rss => "RAM",
            Self::Cpu => "CPU",
        }
    }
}

pub const VIEW_TRANSITION_DURATION: Duration = Duration::from_millis(160);
const TRANSITION_FRAME_INTERVAL: Duration = Duration::from_millis(33);

#[derive(Clone, Copy, Debug)]
pub struct ViewTransition {
    pub from: View,
    pub to: View,
    pub direction: i32,
    pub started: Instant,
}

pub struct App {
    pub view: View,
    pub graph_mode: GraphMode,
    pub metric: Metric,
    pub transition: Option<ViewTransition>,
    pub snapshot: Snapshot,
    pub selected: Option<ProcessKey>,
    pub sort: Sort,
    pub focused: bool,
    pub focus: Focus,
    pub explorer: Explorer,
    pub insights: Insights,
    pub pins: HashSet<ProcessKey>,
    pub nicknames: HashMap<ProcessKey, String>,
    nickname_order: VecDeque<ProcessKey>,
    pub editing_rename: Option<ProcessKey>,
    pub rename_text: String,
    pub changes: Changes,
    pub show_changes: bool,
    pub change_selected: Option<u64>,
    pub query: String,
    pub editing_search: bool,
    search_original: String,
    pub paused: bool,
    pub help: bool,
    pub help_scroll: usize,
    pub tour: bool,
    pub tour_step: usize,
    pub requested_all: bool,
    pub no_motion: bool,
    pub sample_tick: u64,
    pub hitboxes: Vec<(Rect, ui::HitTarget)>,
    pub last_sample: Instant,
    pub interval: Duration,
    pub recording_status: Option<RecordingStatus>,
    pub recording_notice: Option<String>,
    pub editing_recording: bool,
    pub recording_name: String,
    pub sampler_failed: bool,
    pub resuming: bool,
}

impl App {
    pub fn new(snapshot: Snapshot) -> Self {
        let mut insights = Insights::default();
        insights.update(&snapshot);
        let requested_all = snapshot.all_processes;
        let mut changes = Changes::default();
        changes.observe(&snapshot);
        let mut app = Self {
            view: View::Overview,
            graph_mode: GraphMode::Constellation,
            metric: Metric::Rss,
            transition: None,
            snapshot,
            selected: None,
            sort: Sort::Memory,
            focused: false,
            focus: Focus::Sessions,
            explorer: Explorer::default(),
            insights,
            pins: HashSet::new(),
            nicknames: HashMap::new(),
            nickname_order: VecDeque::new(),
            editing_rename: None,
            rename_text: String::new(),
            changes,
            show_changes: false,
            change_selected: None,
            query: String::new(),
            editing_search: false,
            search_original: String::new(),
            paused: false,
            help: false,
            help_scroll: 0,
            tour: false,
            tour_step: 0,
            requested_all,
            no_motion: false,
            sample_tick: 0,
            hitboxes: Vec::new(),
            last_sample: Instant::now(),
            interval: Duration::from_secs(1),
            recording_status: None,
            recording_notice: None,
            editing_recording: false,
            recording_name: String::new(),
            sampler_failed: false,
            resuming: false,
        };
        app.reconcile_selection();
        app
    }
    pub fn set_view(&mut self, view: View) {
        self.show_changes = view == View::Changes;
        if self.view == view {
            if self.no_motion || self.paused || self.stale() {
                self.transition = None;
            }
            return;
        }
        let from = self.view;
        self.view = view;
        self.transition =
            (!self.no_motion && !self.paused && !self.stale()).then(|| ViewTransition {
                from,
                to: view,
                direction: if view.index() > from.index() { 1 } else { -1 },
                started: Instant::now(),
            });
        if view == View::Changes
            && !self
                .changes
                .events()
                .iter()
                .any(|event| Some(event.sequence) == self.change_selected)
        {
            self.change_selected = self.changes.events().back().map(|event| event.sequence);
        }
    }

    pub fn navigate_view(&mut self, delta: i32) {
        let index = (self.view.index() as i64 + i64::from(delta)).rem_euclid(View::ALL.len() as i64)
            as usize;
        let previous = self.view;
        self.set_view(View::ALL[index]);
        if previous != self.view
            && let Some(transition) = &mut self.transition
        {
            transition.direction = delta.signum();
        }
    }

    pub fn navigation_blocked(&self) -> bool {
        self.editing_search
            || self.editing_rename.is_some()
            || self.editing_recording
            || self.help
            || self.tour
    }

    /// Return true only when a navigation/inspector key was handled. Editors and
    /// guides own their keys; process arrows remain tree controls in the inspector.
    pub fn navigation_key(&mut self, key: KeyCode) -> bool {
        if self.navigation_blocked() {
            return false;
        }
        match key {
            KeyCode::Char('1'..='4') => {
                let KeyCode::Char(digit) = key else {
                    unreachable!()
                };
                self.set_view(View::ALL[(digit as u8 - b'1') as usize]);
            }
            KeyCode::Char('[') => self.navigate_view(-1),
            KeyCode::Char(']') => self.navigate_view(1),
            KeyCode::Left | KeyCode::Right
                if self.view == View::ProcessMap
                    && self.focused
                    && self.focus == Focus::Processes =>
            {
                self.tree_action(key);
            }
            KeyCode::Left => self.navigate_view(-1),
            KeyCode::Right => self.navigate_view(1),
            KeyCode::Char('g') if self.view == View::ProcessMap => {
                self.graph_mode = match self.graph_mode {
                    GraphMode::Constellation => GraphMode::Heatmap,
                    GraphMode::Heatmap => GraphMode::Constellation,
                };
                self.focused = false;
                self.focus = if self.graph_mode == GraphMode::Heatmap {
                    Focus::Processes
                } else {
                    Focus::Sessions
                };
            }
            KeyCode::Char('m')
                if self.view == View::History
                    || (self.view == View::ProcessMap && self.graph_mode == GraphMode::Heatmap) =>
            {
                self.metric = match self.metric {
                    Metric::Rss => Metric::Cpu,
                    Metric::Cpu => Metric::Rss,
                };
            }
            KeyCode::Char('i') => {
                if self.view != View::ProcessMap {
                    self.set_view(View::ProcessMap);
                    self.focused = true;
                } else {
                    self.focused = !self.focused;
                }
                self.focus = if self.focused || self.graph_mode == GraphMode::Heatmap {
                    Focus::Processes
                } else {
                    Focus::Sessions
                };
            }
            KeyCode::Tab | KeyCode::BackTab => {
                if self.view == View::ProcessMap
                    && (self.focused || self.graph_mode == GraphMode::Heatmap)
                {
                    self.focus = if self.focus == Focus::Sessions {
                        Focus::Processes
                    } else {
                        Focus::Sessions
                    };
                } else {
                    self.select(if key == KeyCode::Tab { 1 } else { -1 });
                }
            }
            KeyCode::Enter if self.view != View::Changes => {
                if self.view == View::ProcessMap && self.focused && self.focus == Focus::Processes {
                    self.tree_action(key);
                } else {
                    self.set_view(View::ProcessMap);
                    self.focused = true;
                    self.focus = Focus::Processes;
                }
            }
            _ => return false,
        }
        true
    }

    /// Completed/disabled transitions never request further animation frames.
    pub fn transition_progress(&self) -> Option<f32> {
        let transition = self.transition.as_ref()?;
        if self.no_motion
            || self.paused
            || self.stale()
            || transition.to != self.view
            || transition.from == transition.to
        {
            return None;
        }
        let elapsed = transition.started.elapsed();
        (elapsed < VIEW_TRANSITION_DURATION)
            .then(|| elapsed.as_secs_f32() / VIEW_TRANSITION_DURATION.as_secs_f32())
    }

    fn finish_transition(&mut self) -> bool {
        if self.transition.is_some() && self.transition_progress().is_none() {
            self.transition = None;
            true
        } else {
            false
        }
    }

    pub fn sessions(&self) -> Vec<&SessionSnapshot> {
        let query = self.query.to_ascii_lowercase();
        let mut sessions: Vec<_> = self
            .snapshot
            .sessions
            .iter()
            .filter(|s| {
                query.is_empty()
                    || s.label.to_ascii_lowercase().contains(&query)
                    || self.display_label(s).to_ascii_lowercase().contains(&query)
                    || s.key.pid.to_string().contains(&query)
                    || s.processes.iter().any(|p| {
                        p.name.to_ascii_lowercase().contains(&query)
                            || p.pid().to_string().contains(&query)
                    })
            })
            .collect();
        sessions.sort_by(|a, b| {
            let pinned = self.pins.contains(&b.key).cmp(&self.pins.contains(&a.key));
            let order = match self.sort {
                Sort::Memory => b.rss_bytes().cmp(&a.rss_bytes()),
                Sort::Cpu => b
                    .cpu_percent()
                    .unwrap_or(0.0)
                    .total_cmp(&a.cpu_percent().unwrap_or(0.0)),
                Sort::Growth => self.growth_delta(b).cmp(&self.growth_delta(a)),
            };
            pinned.then(order).then_with(|| a.key.pid.cmp(&b.key.pid))
        });
        sessions
    }
    /// The renderer and keyboard use exactly the same all-process heatmap order.
    pub fn heatmap_processes(&self) -> Vec<&ProcessSnapshot> {
        let Some(session) = self.selected_session() else {
            return Vec::new();
        };
        let mut processes: Vec<_> = session.processes.iter().collect();
        processes.sort_by(|a, b| {
            let metric_order = match self.metric {
                Metric::Rss => b.rss_bytes.cmp(&a.rss_bytes),
                Metric::Cpu => {
                    let known = |process: &ProcessSnapshot| {
                        process
                            .cpu_percent
                            .filter(|cpu| cpu.is_finite() && *cpu >= 0.0)
                            .unwrap_or(-1.0)
                    };
                    known(b).total_cmp(&known(a))
                }
            };
            metric_order.then_with(|| a.key.cmp(&b.key))
        });
        processes
    }

    pub fn display_label<'a>(&'a self, session: &'a SessionSnapshot) -> &'a str {
        self.nicknames
            .get(&session.key)
            .map(String::as_str)
            .unwrap_or(&session.label)
    }
    pub fn start_rename(&mut self) {
        if let Some(key) = self.selected {
            self.editing_rename = Some(key);
            self.rename_text = self.nicknames.get(&key).cloned().unwrap_or_default();
        }
    }
    pub fn rename_key(&mut self, key: KeyCode) {
        let Some(target) = self.editing_rename else {
            return;
        };
        match key {
            KeyCode::Esc => {
                self.editing_rename = None;
                self.rename_text.clear();
            }
            KeyCode::Enter => {
                let name = self.rename_text.trim().to_string();
                self.nickname_order.retain(|key| *key != target);
                if name.is_empty() {
                    self.nicknames.remove(&target);
                } else {
                    if self.nicknames.len() >= 128
                        && !self.nicknames.contains_key(&target)
                        && let Some(oldest) = self.nickname_order.pop_front()
                    {
                        self.nicknames.remove(&oldest);
                    }
                    self.nicknames.insert(target, name);
                    self.nickname_order.push_back(target);
                }
                self.editing_rename = None;
                self.rename_text.clear();
                self.reconcile_selection();
            }
            KeyCode::Backspace => {
                self.rename_text.pop();
            }
            KeyCode::Char(c)
                if self.rename_text.len() < 24
                    && (c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' | '+')) =>
            {
                self.rename_text.push(c)
            }
            _ => {}
        }
    }
    pub fn open_changes(&mut self) {
        self.set_view(View::Changes);
        self.change_selected = self.changes.events().back().map(|event| event.sequence);
    }
    pub fn changes_key(&mut self, key: KeyCode) {
        let events = self.changes.events();
        let len = events.len();
        let index = events
            .iter()
            .rev()
            .position(|event| Some(event.sequence) == self.change_selected)
            .unwrap_or(0);
        let new_index = match key {
            KeyCode::Esc | KeyCode::Char('e') => {
                self.set_view(View::Overview);
                return;
            }
            KeyCode::Down | KeyCode::Char('j') => index.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => index.saturating_sub(1),
            KeyCode::PageDown => index.saturating_add(5),
            KeyCode::PageUp => index.saturating_sub(5),
            KeyCode::Home => 0,
            KeyCode::End => len.saturating_sub(1),
            KeyCode::Enter => {
                if let Some(event) = events.iter().rev().nth(index)
                    && self
                        .snapshot
                        .sessions
                        .iter()
                        .any(|session| session.key == event.session_key)
                {
                    self.selected = Some(event.session_key);
                    self.query.clear();
                    self.set_view(View::ProcessMap);
                    self.focused = true;
                    self.focus = Focus::Processes;
                    self.reconcile_explorer();
                }
                return;
            }
            _ => return,
        };
        self.change_selected = events
            .iter()
            .rev()
            .nth(new_index.min(len.saturating_sub(1)))
            .map(|event| event.sequence);
    }
    pub fn growth_delta(&self, session: &SessionSnapshot) -> i64 {
        self.insights
            .growth(session.key)
            .filter(|g| g.sample_count >= 2)
            .map(|g| g.delta_bytes)
            .or_else(|| rss_delta(&session.history))
            .unwrap_or(0)
    }
    pub fn stale(&self) -> bool {
        self.sampler_failed
            || self.resuming
            || (!self.paused
                && self.last_sample.elapsed()
                    > self.interval.saturating_mul(3).max(Duration::from_secs(3)))
    }
    pub fn attachment_notice(&self) -> Option<&'static str> {
        self.snapshot
            .attachment
            .and_then(|attachment| match attachment.status {
                AttachmentStatus::Attached => None,
                AttachmentStatus::Missing => Some("Requested PID not observed"),
                AttachmentStatus::Unavailable => Some("Root access unavailable"),
                AttachmentStatus::Ended => Some("Root no longer observed"),
                AttachmentStatus::IdentityChanged => Some("PID identity changed; stopped"),
            })
    }
    pub fn scope_label(&self) -> &'static str {
        if self.snapshot.attachment.is_some() {
            return if self.attachment_notice().is_some() {
                "PID UNSEEN"
            } else {
                "PID FAMILY"
            };
        }
        match (self.snapshot.all_processes, self.requested_all) {
            (false, true) => "AGENTS → ALL",
            (true, false) => "ALL → AGENTS",
            (true, true) => "ALL FAMILIES",
            (false, false) => "AGENTS",
        }
    }
    pub fn sample_marker(&self) -> &'static str {
        "●"
    }
    pub fn guide_key(&mut self, key: KeyCode, page_rows: usize) {
        if self.tour {
            match key {
                KeyCode::Esc => self.tour = false,
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('n') => {
                    if self.tour_step < 2 {
                        self.tour_step += 1;
                    } else {
                        self.tour = false;
                    }
                }
                KeyCode::Left | KeyCode::Char('b') => {
                    self.tour_step = self.tour_step.saturating_sub(1)
                }
                _ => {}
            }
        } else {
            match key {
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Enter => self.help = false,
                KeyCode::Down | KeyCode::Char('j') => {
                    self.help_scroll = self.help_scroll.saturating_add(1)
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_scroll = self.help_scroll.saturating_sub(1)
                }
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    self.help_scroll = self.help_scroll.saturating_add(page_rows)
                }
                KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(page_rows),
                KeyCode::Home => self.help_scroll = 0,
                KeyCode::End => self.help_scroll = usize::MAX,
                _ => {}
            }
        }
    }
    pub fn selected_session(&self) -> Option<&SessionSnapshot> {
        self.snapshot
            .sessions
            .iter()
            .find(|s| Some(s.key) == self.selected)
    }
    fn reconcile_selection(&mut self) {
        let sessions = self.sessions();
        if !sessions.iter().any(|s| Some(s.key) == self.selected) {
            self.selected = sessions.first().map(|s| s.key);
        }
        self.reconcile_explorer();
    }
    fn reconcile_explorer(&mut self) {
        if let Some(session) = self
            .snapshot
            .sessions
            .iter()
            .find(|s| Some(s.key) == self.selected)
        {
            self.explorer.reconcile(session);
        }
    }
    pub fn update(&mut self, snapshot: Snapshot) {
        if self.stale() || self.snapshot.all_processes != snapshot.all_processes {
            self.insights.mark_gap();
            self.changes.mark_gap();
        }
        self.changes.observe(&snapshot);
        if self.view == View::Changes
            && !self
                .changes
                .events()
                .iter()
                .any(|event| Some(event.sequence) == self.change_selected)
        {
            self.change_selected = self.changes.events().back().map(|event| event.sequence);
        }
        self.insights.update(&snapshot);
        self.snapshot = snapshot;
        self.sample_tick = self.sample_tick.wrapping_add(1);
        self.last_sample = Instant::now();
        self.resuming = false;
        self.pins
            .retain(|key| self.snapshot.sessions.iter().any(|s| s.key == *key));
        self.reconcile_selection();
    }
    pub fn select(&mut self, direction: i32) {
        let sessions = self.sessions();
        if sessions.is_empty() {
            self.selected = None;
            return;
        }
        let old = sessions
            .iter()
            .position(|s| Some(s.key) == self.selected)
            .unwrap_or(0);
        let index = (old as i32 + direction).rem_euclid(sessions.len() as i32) as usize;
        self.selected = Some(sessions[index].key);
        self.reconcile_explorer();
    }
    pub fn move_cursor(&mut self, direction: i32) {
        if self.view == View::ProcessMap
            && self.graph_mode == GraphMode::Heatmap
            && !self.focused
            && self.focus == Focus::Processes
        {
            let target = {
                let processes = self.heatmap_processes();
                let index = processes
                    .iter()
                    .position(|process| Some(process.key) == self.explorer.selected())
                    .unwrap_or(0);
                let next = if direction < 0 {
                    index.saturating_sub(direction.unsigned_abs() as usize)
                } else {
                    index
                        .saturating_add(direction as usize)
                        .min(processes.len().saturating_sub(1))
                };
                processes.get(next).map(|process| process.key)
            };
            if let Some(key) = target
                && let Some(session) = self
                    .snapshot
                    .sessions
                    .iter()
                    .find(|session| Some(session.key) == self.selected)
            {
                self.explorer.reveal_key(session, key);
            }
        } else if self.view == View::ProcessMap
            && self.focus == Focus::Processes
            && (self.focused || self.graph_mode == GraphMode::Heatmap)
        {
            if let Some(session) = self
                .snapshot
                .sessions
                .iter()
                .find(|s| Some(s.key) == self.selected)
            {
                self.explorer.move_selection(session, direction);
            }
        } else {
            self.select(direction);
        }
    }
    pub fn tree_action(&mut self, key: KeyCode) {
        if let Some(session) = self
            .snapshot
            .sessions
            .iter()
            .find(|s| Some(s.key) == self.selected)
        {
            match key {
                KeyCode::Left | KeyCode::Char('h') => self.explorer.collapse_selected(session),
                KeyCode::Right | KeyCode::Char('l') => self.explorer.expand_selected(session),
                KeyCode::Enter => self.explorer.toggle_selected(session),
                KeyCode::Char('E') => self.explorer.expand_all(session),
                KeyCode::Char('C') => self.explorer.collapse_all(session),
                _ => {}
            }
        }
    }
    pub fn start_search(&mut self) {
        self.search_original = self.query.clone();
        self.editing_search = true;
        self.focus = Focus::Sessions;
    }
    pub fn search_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc => {
                self.query = self.search_original.clone();
                self.editing_search = false;
            }
            KeyCode::Enter => self.editing_search = false,
            KeyCode::Backspace => {
                self.query.pop();
            }
            KeyCode::Char(c) if !c.is_control() && self.query.chars().count() < 64 => {
                self.query.push(c)
            }
            _ => {}
        }
        self.reconcile_selection();
    }
    pub fn toggle_pin(&mut self) {
        if let Some(key) = self.selected
            && !self.pins.remove(&key)
        {
            self.pins.insert(key);
        }
    }
    pub fn click(&mut self, target: ui::HitTarget) {
        if self.navigation_blocked() {
            return;
        }
        match target {
            ui::HitTarget::View(view) => self.set_view(view),
            ui::HitTarget::Session(key) => {
                self.selected = Some(key);
                self.focus = Focus::Sessions;
                self.reconcile_explorer();
            }
            ui::HitTarget::Process(key) => {
                if let Some(session) = self
                    .snapshot
                    .sessions
                    .iter()
                    .find(|s| Some(s.key) == self.selected)
                {
                    if self.view == View::ProcessMap && self.graph_mode == GraphMode::Heatmap {
                        self.explorer.reveal_key(session, key);
                    } else {
                        self.explorer.select_key(session, key);
                    }
                    self.focus = Focus::Processes;
                }
            }
        }
    }
}

pub struct RecordingLaunch {
    pub name: Option<String>,
    pub directory: Option<PathBuf>,
    pub interval: Duration,
}

fn recording_error(app: &mut App, error: impl std::fmt::Display) {
    if let Some(status) = &mut app.recording_status {
        status.stopped = true;
    }
    let message: String = error
        .to_string()
        .chars()
        .filter(|c| !c.is_control())
        .take(140)
        .collect();
    app.recording_notice = Some(format!("RECORDING STOPPED · {message}"));
}

fn start_recording(
    app: &mut App,
    recorder: &mut Option<RecordingWorker>,
    options: RecordingOptions,
) {
    match RecordingWorker::start(options, &app.snapshot) {
        Ok(recording) => {
            app.recording_status = None;
            app.recording_notice =
                Some("Recording starting · R stop · monitoring remains live".into());
            *recorder = Some(recording);
        }
        Err(error) => recording_error(app, error),
    }
}

fn stop_recording(app: &mut App, recorder: &mut Option<RecordingWorker>, reason: StopReason) {
    if let Some(recording) = recorder
        && !recording.stopping()
    {
        recording.stop(reason);
        app.recording_notice = Some("Recording stopping · final save pending".into());
    }
}

fn recording_gap(app: &mut App, recorder: &mut Option<RecordingWorker>, reason: &'static str) {
    if let Some(recording) = recorder.as_mut()
        && let Err(error) = recording.mark_gap(reason)
    {
        recording_error(app, error);
    }
}

fn apply_recording_update(
    app: &mut App,
    update: RecordingUpdate,
    stopping: bool,
) -> io::Result<()> {
    if let Some(status) = update.status {
        app.recording_status = Some(status);
    }
    if let Some(error) = update.error {
        recording_error(app, error);
    } else if update.finished {
        app.recording_notice =
            Some("Recording saved · use recordings list / show / export after quitting".into());
    } else if !stopping {
        app.recording_notice = None;
    }
    if update.panicked {
        return Err(io::Error::other("recording worker stopped unexpectedly"));
    }
    Ok(())
}

fn poll_recording(app: &mut App, recorder: &mut Option<RecordingWorker>) -> io::Result<bool> {
    let Some(recording) = recorder else {
        return Ok(false);
    };
    let Some(update) = recording.poll() else {
        return Ok(false);
    };
    let finished = update.finished;
    let result = apply_recording_update(app, update, recording.stopping());
    if finished {
        // The final update is published only after all recording I/O ends. Drop
        // the handle without joining; it can no longer block a restart or input.
        *recorder = None;
    }
    result.map(|()| true)
}

struct TerminalGuard {
    signals: Vec<signal_hook::SigId>,
    active: bool,
}
impl TerminalGuard {
    fn restore(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        // Try both even if writing an escape sequence fails: raw mode must be undone.
        let mouse = execute!(io::stdout(), DisableMouseCapture, Show);
        let terminal = ratatui::try_restore();
        mouse.and(terminal)?;
        self.active = false;
        Ok(())
    }

    fn suspend(
        &mut self,
        terminal: &mut ratatui::DefaultTerminal,
        stop: &AtomicBool,
    ) -> io::Result<()> {
        self.restore()?;
        // Keep terminal I/O out of signal handlers. This stops every thread without
        // unregistering SIGTSTP, so later suspend cycles still take this cleanup path.
        signal_hook::low_level::emulate_default_handler(signal_hook::consts::SIGTSTP)?;
        // Execution continues here after SIGCONT. A termination signal delivered
        // while stopped takes priority over re-entering the UI.
        if !stop.load(Ordering::Relaxed) {
            self.active = true;
            enable_raw_mode()?;
            execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
            terminal.hide_cursor()?;
            // The shell may have changed the screen and size while we were stopped.
            // Invalidate the diff buffer so even an unchanged frame is fully redrawn.
            // Force resize even when dimensions match; unlike clear(), this does
            // not require a cursor-position query/response from the terminal.
            terminal.resize(terminal.size()?.into())?;
        }
        Ok(())
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.restore();
        for signal in &self.signals {
            signal_hook::low_level::unregister(*signal);
        }
    }
}

pub fn run(
    demo: bool,
    all: bool,
    interval: Duration,
    tour: bool,
    no_motion: bool,
    pid: Option<u32>,
    recording_launch: RecordingLaunch,
) -> io::Result<()> {
    // A bounded channel decouples potentially slow process reads from input/rendering.
    let stop = Arc::new(AtomicBool::new(false));
    let show_all = Arc::new(AtomicBool::new(all));
    let paused = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::sync_channel(1);
    let generation = Arc::new(AtomicU64::new(0));
    let mut guard = TerminalGuard {
        signals: Vec::new(),
        active: false,
    };
    for signal in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
        signal_hook::consts::SIGQUIT,
    ] {
        guard
            .signals
            .push(signal_hook::flag::register(signal, stop.clone())?);
    }
    let worker_stop = stop.clone();
    let worker_all = show_all.clone();
    let worker_pause = paused.clone();
    let worker_generation = generation.clone();
    let worker = thread::spawn(move || {
        let mut demo_sampler = demo.then(DemoSampler::new);
        let mut live_sampler = (!demo).then(|| pid.map(LiveSampler::with_pid).unwrap_or_default());
        if let Some(sampler) = &mut live_sampler {
            sampler.set_interval(interval);
        }
        let mut previous_generation = 0;
        while !worker_stop.load(Ordering::Relaxed) {
            let start = Instant::now();
            let sample_generation = worker_generation.load(Ordering::Acquire);
            if !worker_pause.load(Ordering::Relaxed) {
                if sample_generation != previous_generation {
                    if let Some(sampler) = &mut live_sampler {
                        sampler.reset_cpu_baseline();
                    }
                    previous_generation = sample_generation;
                }
                let all = worker_all.load(Ordering::Relaxed);
                let next = match (&mut demo_sampler, &mut live_sampler) {
                    (Some(s), _) => s.sample(all),
                    (_, Some(s)) => s.sample(all),
                    _ => unreachable!(),
                };
                if !worker_pause.load(Ordering::Acquire)
                    && worker_generation.load(Ordering::Acquire) == sample_generation
                    && tx.try_send((sample_generation, next)).is_err()
                    && worker_stop.load(Ordering::Relaxed)
                {
                    break;
                }
            }
            while start.elapsed() < interval && !worker_stop.load(Ordering::Relaxed) {
                if worker_generation.load(Ordering::Acquire) != sample_generation {
                    break;
                }
                thread::sleep(
                    Duration::from_millis(40).min(interval.saturating_sub(start.elapsed())),
                );
            }
        }
    });
    let initial = match wait_initial(&rx, &stop) {
        Ok(Some(snapshot)) => snapshot,
        result => {
            stop.store(true, Ordering::Relaxed);
            return result.map(|_| ());
        }
    };
    if let Some(attachment) = initial.attachment
        && attachment.status != AttachmentStatus::Attached
    {
        stop.store(true, Ordering::Relaxed);
        return Err(io::Error::other(format!(
            "could not attach to PID {}: it is not an accessible current-user process in this sample. Use --all to find a PID, then try again.",
            attachment.requested_pid
        )));
    }
    let mut app = App::new(initial);
    app.interval = interval;
    app.tour = tour;
    app.no_motion = no_motion;
    let mut recorder = None;
    let suspend = Arc::new(AtomicBool::new(false));
    // Before terminal setup SIGTSTP can safely use the OS default action.
    guard.signals.push(signal_hook::flag::register(
        signal_hook::consts::SIGTSTP,
        suspend.clone(),
    )?);
    guard.active = true;
    // ratatui::try_init installs a process-global restoring panic hook. A worker
    // panic must not race terminal state; only this UI thread owns restoration.
    // The active guard covers every partial initialization failure instead.
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let mut terminal =
        ratatui::DefaultTerminal::new(ratatui::backend::CrosstermBackend::new(io::stdout()))?;
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> io::Result<()> {
        execute!(io::stdout(), EnableMouseCapture)?;
        if let Some(name) = recording_launch.name {
            let sampling_interval = app.interval;
            start_recording(
                &mut app,
                &mut recorder,
                RecordingOptions {
                    name,
                    directory: recording_launch.directory.clone(),
                    interval: recording_launch.interval,
                    sampling_interval,
                },
            );
        }
        let mut dirty = true;
        let mut last_draw = Instant::now();
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            if suspend.swap(false, Ordering::Relaxed) {
                recording_gap(&mut app, &mut recorder, "terminal_suspend");
                paused.store(true, Ordering::Release);
                generation.fetch_add(1, Ordering::AcqRel);
                while rx.try_recv().is_ok() {}
                guard.suspend(&mut terminal, &stop)?;
                // Discard any pre-suspend sample and reset CPU/change baselines.
                generation.fetch_add(1, Ordering::AcqRel);
                while rx.try_recv().is_ok() {}
                paused.store(app.paused, Ordering::Release);
                app.insights.mark_gap();
                app.changes.mark_gap();
                app.resuming = !app.paused;
                dirty = true;
                continue;
            }
            dirty |= poll_recording(&mut app, &mut recorder)?;
            match rx.try_recv() {
                Ok((epoch, next)) => {
                    if !app.paused && epoch == generation.load(Ordering::Acquire) {
                        if app.stale() {
                            recording_gap(&mut app, &mut recorder, "sampler_delay");
                        }
                        app.update(next);
                        if let Some(recording) = recorder.as_mut()
                            && let Err(error) = recording.observe(&app.snapshot)
                        {
                            recording_error(&mut app, error);
                        }
                        dirty = true;
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(io::Error::other("process sampler stopped unexpectedly"));
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            dirty |= app.finish_transition();
            let transitioning = app.transition_progress().is_some();
            if dirty
                || last_draw.elapsed() >= Duration::from_secs(1)
                || (transitioning && last_draw.elapsed() >= TRANSITION_FRAME_INTERVAL)
            {
                if let Some(recording) = &recorder
                    && let Some(status) = &mut app.recording_status
                    && !status.stopped
                {
                    status.elapsed_secs = recording.elapsed_secs();
                }
                terminal.draw(|frame| {
                    app.hitboxes = ui::draw(frame, &app);
                })?;
                dirty = false;
                last_draw = Instant::now();
            }
            let poll_interval = if transitioning {
                TRANSITION_FRAME_INTERVAL.saturating_sub(last_draw.elapsed())
            } else {
                Duration::from_millis(80)
            };
            if event::poll(poll_interval)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        dirty = true;
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                            && key.code == KeyCode::Char('c')
                        {
                            break;
                        }
                        // Raw mode disables the terminal driver's Ctrl-Z signal.
                        // Job control takes priority even inside a text editor.
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                            && key.code == KeyCode::Char('z')
                        {
                            suspend.store(true, Ordering::Relaxed);
                            continue;
                        }
                        if app.editing_recording {
                            match key.code {
                                KeyCode::Esc => app.editing_recording = false,
                                KeyCode::Backspace => {
                                    app.recording_name.pop();
                                }
                                KeyCode::Char(c)
                                    if (c.is_ascii_alphanumeric()
                                        || matches!(c, ' ' | '.' | '_' | '-'))
                                        && app.recording_name.len() < 64 =>
                                {
                                    app.recording_name.push(c)
                                }
                                KeyCode::Enter if !app.recording_name.trim().is_empty() => {
                                    app.editing_recording = false;
                                    if app.paused || app.stale() || app.resuming {
                                        app.recording_notice = Some(
                                            "Resume fresh sampling before starting a recording"
                                                .into(),
                                        );
                                    } else {
                                        let sampling_interval = app.interval;
                                        let name = app.recording_name.trim().to_owned();
                                        start_recording(
                                            &mut app,
                                            &mut recorder,
                                            RecordingOptions {
                                                name,
                                                directory: recording_launch.directory.clone(),
                                                interval: recording_launch.interval,
                                                sampling_interval,
                                            },
                                        );
                                    }
                                }
                                _ => {}
                            }
                            continue;
                        }
                        if app.editing_rename.is_some() {
                            app.rename_key(key.code);
                            continue;
                        }
                        if app.editing_search {
                            app.search_key(key.code);
                            continue;
                        }
                        if key.code == KeyCode::Char('q') {
                            break;
                        }
                        if app.help || app.tour {
                            let size = terminal.size()?;
                            let (rows, maximum) = ui::guide_scroll_limits(size.width, size.height);
                            app.help_scroll = app.help_scroll.min(maximum);
                            app.guide_key(key.code, rows);
                            app.help_scroll = app.help_scroll.min(maximum);
                            continue;
                        }
                        if app.navigation_key(key.code) {
                            continue;
                        }
                        if app.view == View::Changes
                            && matches!(
                                key.code,
                                KeyCode::Esc
                                    | KeyCode::Enter
                                    | KeyCode::Char('e' | 'j' | 'k')
                                    | KeyCode::Down
                                    | KeyCode::Up
                                    | KeyCode::PageDown
                                    | KeyCode::PageUp
                                    | KeyCode::Home
                                    | KeyCode::End
                            )
                        {
                            app.changes_key(key.code);
                            continue;
                        }
                        match key.code {
                            KeyCode::Down | KeyCode::Char('j') => app.move_cursor(1),
                            KeyCode::Up | KeyCode::Char('k') => app.move_cursor(-1),
                            KeyCode::PageDown => app.move_cursor(8),
                            KeyCode::PageUp => app.move_cursor(-8),
                            KeyCode::Char('h' | 'l' | 'E' | 'C')
                                if app.view == View::ProcessMap
                                    && app.focused
                                    && app.focus == Focus::Processes =>
                            {
                                app.tree_action(key.code)
                            }
                            KeyCode::Esc => {
                                if app.view == View::ProcessMap && app.focused {
                                    app.focused = false;
                                    app.focus = Focus::Sessions;
                                } else {
                                    app.query.clear();
                                    app.reconcile_selection();
                                }
                            }
                            KeyCode::Char('?') => {
                                app.help = true;
                                app.help_scroll = 0;
                            }
                            KeyCode::Char('/') => app.start_search(),
                            KeyCode::Char('r') => app.start_rename(),
                            KeyCode::Char('R') => {
                                if recorder.is_some() {
                                    stop_recording(&mut app, &mut recorder, StopReason::User);
                                } else if app.paused || app.stale() || app.resuming {
                                    app.recording_notice = Some(
                                        "Resume fresh sampling before starting a recording".into(),
                                    );
                                } else {
                                    app.editing_recording = true;
                                    app.recording_name.clear();
                                }
                            }
                            KeyCode::Char('e') => app.open_changes(),
                            KeyCode::Char('*') => app.toggle_pin(),
                            KeyCode::Char('s') => {
                                app.sort = match app.sort {
                                    Sort::Memory => Sort::Cpu,
                                    Sort::Cpu => Sort::Growth,
                                    Sort::Growth => Sort::Memory,
                                }
                            }
                            KeyCode::Char('a') if app.snapshot.attachment.is_none() => {
                                recording_gap(&mut app, &mut recorder, "scope_change");
                                app.requested_all = !app.requested_all;
                                show_all.store(app.requested_all, Ordering::Relaxed);
                                generation.fetch_add(1, Ordering::AcqRel);
                                while rx.try_recv().is_ok() {}
                                if !app.paused {
                                    app.resuming = true;
                                }
                            }
                            KeyCode::Char(' ') | KeyCode::Char('p') => {
                                let gap_reason = if app.paused { "resume" } else { "pause" };
                                recording_gap(&mut app, &mut recorder, gap_reason);
                                app.paused = !app.paused;
                                if !app.paused {
                                    app.insights.mark_gap();
                                    app.changes.mark_gap();
                                }
                                paused.store(app.paused, Ordering::Release);
                                generation.fetch_add(1, Ordering::AcqRel);
                                while rx.try_recv().is_ok() {}
                                app.resuming = !app.paused;
                            }
                            _ => {}
                        }
                    }
                    Event::Mouse(mouse) => match mouse.kind {
                        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp
                            if app.help && !app.editing_search && app.editing_rename.is_none() =>
                        {
                            let size = terminal.size()?;
                            let (_, maximum) = ui::guide_scroll_limits(size.width, size.height);
                            app.help_scroll = app.help_scroll.min(maximum);
                            app.guide_key(
                                if mouse.kind == MouseEventKind::ScrollDown {
                                    KeyCode::Down
                                } else {
                                    KeyCode::Up
                                },
                                1,
                            );
                            app.help_scroll = app.help_scroll.min(maximum);
                            dirty = true;
                        }
                        MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight
                            if !app.navigation_blocked() =>
                        {
                            app.navigate_view(if mouse.kind == MouseEventKind::ScrollLeft {
                                -1
                            } else {
                                1
                            });
                            dirty = true;
                        }
                        MouseEventKind::Down(MouseButton::Left) if !app.navigation_blocked() => {
                            if let Some((_, target)) = app
                                .hitboxes
                                .iter()
                                .find(|(r, _)| r.contains((mouse.column, mouse.row).into()))
                            {
                                app.click(*target);
                                dirty = true;
                            }
                        }
                        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp
                            if !app.navigation_blocked() =>
                        {
                            let down = mouse.kind == MouseEventKind::ScrollDown;
                            if app.view == View::Changes {
                                app.changes_key(if down { KeyCode::Down } else { KeyCode::Up });
                            } else {
                                app.move_cursor(if down { 1 } else { -1 });
                            }
                            dirty = true;
                        }
                        _ => {}
                    },
                    Event::Resize(_, _) => dirty = true,
                    _ => {}
                }
            }
        }
        Ok(())
    }));
    stop_recording(
        &mut app,
        &mut recorder,
        if matches!(&outcome, Ok(Ok(()))) {
            StopReason::Exit
        } else {
            StopReason::Error
        },
    );
    stop.store(true, Ordering::Relaxed);
    drop(guard);
    // Filesystem stalls may delay process exit, never terminal restoration.
    // This also runs after a UI panic; unwind resumes only after finalization.
    let recording_outcome = if let Some(recording) = recorder.take() {
        apply_recording_update(&mut app, recording.wait(), true)
    } else {
        Ok(())
    };
    if let Some(status) = &app.recording_status {
        println!(
            "Recording {}: {}",
            if status.demo {
                "(SIMULATED)"
            } else {
                "(local)"
            },
            status
                .path
                .to_string_lossy()
                .chars()
                .flat_map(char::escape_default)
                .collect::<String>()
        );
        println!("Review with: lattice recordings show FILE");
    }
    if let Some(notice) = &app.recording_notice {
        println!("{notice}");
    }
    // Detach if a platform read stalls: terminal restoration must not wait on the sampler.
    if worker.is_finished() {
        let _ = worker.join();
    }
    match outcome {
        Ok(result) => result.and(recording_outcome),
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

// Keep cancellation responsive even if the first platform read stalls.
fn wait_initial(
    rx: &mpsc::Receiver<(u64, Snapshot)>,
    stop: &AtomicBool,
) -> io::Result<Option<Snapshot>> {
    let started = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(None);
        }
        if started.elapsed() >= Duration::from_secs(10) {
            return Err(io::Error::other(
                "process sampler did not start within 10 seconds",
            ));
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok((_, snapshot)) => return Ok(Some(snapshot)),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other("process sampler could not start"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_wait_is_cancellable_without_a_first_sample() {
        let (_tx, rx) = mpsc::sync_channel(1);
        let stop = AtomicBool::new(true);
        assert!(wait_initial(&rx, &stop).unwrap().is_none());
    }
    #[test]
    fn stale_age_respects_interval_and_pause() {
        let mut app = App::new(DemoSampler::new().sample(false));
        app.last_sample = Instant::now() - Duration::from_secs(5);
        assert!(app.stale());
        app.interval = Duration::from_secs(30);
        assert!(!app.stale());
        app.interval = Duration::from_secs(1);
        app.paused = true;
        assert!(!app.stale());
        app.sampler_failed = true;
        assert!(app.stale());
    }
    #[test]
    fn selection_survives_resorting_and_updates() {
        let mut sampler = DemoSampler::new();
        let mut app = App::new(sampler.sample(false));
        app.select(1);
        let selected = app.selected;
        app.sort = Sort::Cpu;
        app.update(sampler.sample(false));
        assert_eq!(app.selected, selected);
    }
    #[test]
    fn empty_selection_is_safe() {
        let mut snapshot = DemoSampler::new().sample(false);
        snapshot.sessions.clear();
        let mut app = App::new(snapshot);
        app.select(-1);
        assert!(app.selected.is_none());
    }
    #[test]
    fn disappearing_selection_recovers() {
        let mut sampler = DemoSampler::new();
        let mut app = App::new(sampler.sample(false));
        let selected = app.selected;
        let mut next = sampler.sample(false);
        next.sessions.retain(|s| Some(s.key) != selected);
        app.update(next);
        assert_ne!(app.selected, selected);
        assert!(app.selected_session().is_some());
    }
}

#[cfg(test)]
mod interaction_tests {
    use super::*;
    #[test]
    fn search_preserves_typed_q_and_reconciles_visible_selection() {
        let mut app = App::new(DemoSampler::new().sample(false));
        app.start_search();
        for c in "codex".chars() {
            app.search_key(KeyCode::Char(c));
        }
        assert_eq!(app.sessions().len(), 1);
        assert_eq!(app.selected_session().unwrap().label, "codex");
        app.search_key(KeyCode::Char('q'));
        assert_eq!(app.query, "codexq");
        assert!(app.selected.is_none());
        app.search_key(KeyCode::Esc);
        assert!(app.query.is_empty());
        assert!(app.selected.is_some());
        assert!(!app.editing_search);
    }
    #[test]
    fn search_matches_process_names_and_pid_without_arguments() {
        let mut app = App::new(DemoSampler::new().sample(false));
        app.start_search();
        for c in "4100".chars() {
            app.search_key(KeyCode::Char(c));
        }
        app.search_key(KeyCode::Enter);
        assert_eq!(app.sessions().len(), 1);
        assert_eq!(app.selected.unwrap().pid, 4100);
        app.query = "pytest".into();
        assert!(
            app.sessions()
                .iter()
                .all(|s| s.processes.iter().any(|p| p.name.contains("pytest")))
        );
    }
    #[test]
    fn pins_float_above_sort_but_do_not_resurrect_exited_roots() {
        let mut sampler = DemoSampler::new();
        let mut app = App::new(sampler.sample(false));
        app.select(3);
        let key = app.selected.unwrap();
        app.toggle_pin();
        assert_eq!(app.sessions()[0].key, key);
        app.sort = Sort::Growth;
        assert_eq!(app.sessions()[0].key, key);
        let mut next = sampler.sample(false);
        next.sessions.retain(|s| s.key != key);
        app.update(next);
        assert!(!app.pins.contains(&key));
    }
    #[test]
    fn process_navigation_does_not_switch_the_selected_session() {
        let mut app = App::new(DemoSampler::new().sample(false));
        let key = app.selected;
        app.set_view(View::ProcessMap);
        app.focused = true;
        app.focus = Focus::Processes;
        app.move_cursor(1);
        assert_eq!(app.selected, key);
        assert_ne!(app.explorer.selected(), key);
        app.tree_action(KeyCode::Char('C'));
        assert_eq!(app.explorer.selected(), key);
        app.tree_action(KeyCode::Char('E'));
        assert!(
            app.explorer
                .visible_rows(app.selected_session().unwrap())
                .len()
                > 1
        );
    }
}

#[cfg(test)]
mod onboarding_tests {
    use super::*;
    #[test]
    fn sample_marker_stays_static_across_sampling_and_navigation() {
        let mut sampler = DemoSampler::new();
        let mut app = App::new(sampler.sample(false));
        let initial = app.sample_marker();
        app.select(1);
        assert_eq!(app.sample_marker(), initial);
        app.update(sampler.sample(false));
        assert_eq!(app.sample_marker(), initial);
        app.paused = true;
        assert_eq!(app.sample_marker(), "●");
        app.paused = false;
        app.last_sample = Instant::now() - Duration::from_secs(20);
        assert_eq!(app.sample_marker(), "●");
        app.last_sample = Instant::now();
        app.no_motion = true;
        for _ in 0..4 {
            app.update(sampler.sample(false));
            assert_eq!(app.sample_marker(), "●");
        }
    }
    #[test]
    fn scope_displays_pending_changes_without_relabeling_frozen_data() {
        let mut app = App::new(DemoSampler::new().sample(false));
        assert_eq!(app.scope_label(), "AGENTS");
        app.paused = true;
        app.requested_all = true;
        assert_eq!(app.scope_label(), "AGENTS → ALL");
        assert!(!app.snapshot.all_processes);
        app.snapshot.all_processes = true;
        assert_eq!(app.scope_label(), "ALL FAMILIES");
        app.requested_all = false;
        assert_eq!(app.scope_label(), "ALL → AGENTS");
    }
    #[test]
    fn tour_navigation_never_switches_to_live_data() {
        let mut app = App::new(DemoSampler::new().sample(false));
        app.tour = true;
        app.guide_key(KeyCode::Left, 10);
        assert_eq!(app.tour_step, 0);
        app.guide_key(KeyCode::Enter, 10);
        app.guide_key(KeyCode::Enter, 10);
        assert_eq!(app.tour_step, 2);
        app.guide_key(KeyCode::Enter, 10);
        assert!(!app.tour);
        assert!(app.snapshot.demo);
    }
    #[test]
    fn guide_scroll_and_close_do_not_change_underlying_selection() {
        let mut app = App::new(DemoSampler::new().sample(false));
        let selected = app.selected;
        app.help = true;
        app.guide_key(KeyCode::Down, 8);
        app.guide_key(KeyCode::PageDown, 8);
        assert_eq!(app.help_scroll, 9);
        app.guide_key(KeyCode::Home, 8);
        assert_eq!(app.help_scroll, 0);
        app.guide_key(KeyCode::Esc, 8);
        assert!(!app.help);
        assert_eq!(app.selected, selected);
    }
}

#[cfg(test)]
mod practical_tests {
    use super::*;

    fn rename(app: &mut App, value: &str) {
        app.start_rename();
        app.rename_text.clear();
        for character in value.chars() {
            app.rename_key(KeyCode::Char(character));
        }
        app.rename_key(KeyCode::Enter);
    }

    #[test]
    fn nicknames_are_bounded_sanitized_searchable_and_resettable() {
        let mut app = App::new(DemoSampler::new().sample(false));
        let key = app.selected.unwrap();
        rename(&mut app, "  quick API /\x1b\n🌟  ");
        assert_eq!(app.nicknames[&key], "quick API");
        app.query = "QUICK".into();
        assert_eq!(app.sessions().len(), 1);
        app.query = "claude".into();
        assert_eq!(app.sessions().len(), 1); // Original name remains searchable.
        app.query.clear();
        rename(&mut app, &"a".repeat(100));
        assert_eq!(app.nicknames[&key].len(), 24);
        rename(&mut app, "  ");
        assert!(!app.nicknames.contains_key(&key));
    }

    #[test]
    fn rename_locks_identity_across_selection_exit_and_pid_reuse() {
        let mut sampler = DemoSampler::new();
        let mut app = App::new(sampler.sample(false));
        let target = app.selected.unwrap();
        app.start_rename();
        app.rename_key(KeyCode::Char('q'));
        app.select(1);
        let mut next = sampler.sample(false);
        next.sessions.retain(|s| s.key != target);
        app.update(next);
        app.rename_key(KeyCode::Enter);
        assert_eq!(app.nicknames[&target], "q");
        assert!(!app.nicknames.contains_key(&app.selected.unwrap()));
        let mut replacement = sampler.sample(false);
        let session = replacement
            .sessions
            .iter_mut()
            .find(|s| s.key == target)
            .unwrap();
        session.key.started += 1;
        assert!(!app.nicknames.contains_key(&session.key));
        app.update(replacement);
        app.start_rename();
        app.rename_key(KeyCode::Char('x'));
        app.rename_key(KeyCode::Esc);
        assert!(app.editing_rename.is_none());
        assert!(app.rename_text.is_empty());
    }

    #[test]
    fn nickname_store_evicts_oldest_without_growing_forever() {
        let mut app = App::new(DemoSampler::new().sample(false));
        for pid in 1..=200 {
            app.selected = Some(ProcessKey { pid, started: 1 });
            rename(&mut app, "work");
        }
        assert_eq!(app.nicknames.len(), 128);
        assert_eq!(app.nickname_order.len(), 128);
        assert!(
            !app.nicknames
                .contains_key(&ProcessKey { pid: 1, started: 1 })
        );
        assert!(app.nicknames.contains_key(&ProcessKey {
            pid: 200,
            started: 1
        }));
    }

    #[test]
    fn changes_preserve_cursor_and_inspect_current_family_not_wrong_selection() {
        let mut sampler = DemoSampler::new();
        let mut app = App::new(sampler.sample(false));
        for _ in 0..14 {
            app.update(sampler.sample(false));
        }
        app.open_changes();
        let selected = app.change_selected;
        assert!(selected.is_some());
        for _ in 0..30 {
            app.update(sampler.sample(false));
        }
        assert_eq!(app.change_selected, selected);
        app.query = "no match".into();
        app.changes_key(KeyCode::Enter);
        assert!(!app.show_changes);
        assert!(app.focused);
        assert!(app.query.is_empty());
        assert_eq!(app.selected.unwrap().pid, 4200);
        app.open_changes();
        app.changes_key(KeyCode::End);
        assert_eq!(
            app.change_selected,
            app.changes.events().front().map(|e| e.sequence)
        );
        app.changes_key(KeyCode::Home);
        assert_eq!(
            app.change_selected,
            app.changes.events().back().map(|e| e.sequence)
        );
    }

    #[test]
    fn pause_gap_does_not_create_a_fake_disappearance_or_growth_event() {
        let mut sampler = DemoSampler::new();
        let mut app = App::new(sampler.sample(false));
        for _ in 0..14 {
            app.update(sampler.sample(false));
        }
        let count = app.changes.events().len();
        app.changes.mark_gap();
        for _ in 0..30 {
            sampler.sample(false);
        }
        app.update(sampler.sample(false));
        assert_eq!(app.changes.events().len(), count);
    }

    #[test]
    fn stale_sample_restarts_change_baseline_without_bridging_unobserved_time() {
        let mut sampler = DemoSampler::new();
        let mut app = App::new(sampler.sample(false));
        for _ in 0..14 {
            app.update(sampler.sample(false));
        }
        let count = app.changes.events().len();
        for _ in 0..30 {
            sampler.sample(false);
        }
        app.last_sample = Instant::now() - Duration::from_secs(60);
        app.update(sampler.sample(false));
        assert_eq!(app.changes.events().len(), count);
        assert!(!app.stale());
    }

    #[test]
    fn attached_scope_never_claims_agents_and_end_is_not_a_cause() {
        let mut snapshot = DemoSampler::new().sample(false);
        snapshot.attachment = Some(crate::model::AttachmentSnapshot {
            requested_pid: 4100,
            root: Some(snapshot.sessions[0].key),
            status: AttachmentStatus::Attached,
        });
        let mut app = App::new(snapshot);
        app.requested_all = true;
        assert_eq!(app.scope_label(), "PID FAMILY");
        app.snapshot.attachment.as_mut().unwrap().status = AttachmentStatus::Ended;
        assert_eq!(app.scope_label(), "PID UNSEEN");
        assert_eq!(app.attachment_notice(), Some("Root no longer observed"));
    }
}

#[cfg(test)]
mod navigation_tests {
    use super::*;

    fn demo() -> App {
        App::new(DemoSampler::new().sample(false))
    }

    #[test]
    fn screen_navigation_wraps_and_keeps_the_requested_direction() {
        let mut app = demo();
        assert_eq!(app.view, View::Overview);
        assert_eq!(View::ALL.map(View::index), [0, 1, 2, 3]);
        assert!(app.navigation_key(KeyCode::Left));
        assert_eq!(app.view, View::Changes);
        assert_eq!(app.transition.unwrap().direction, -1);
        assert!(app.show_changes);
        assert!(app.navigation_key(KeyCode::Right));
        assert_eq!(app.view, View::Overview);
        assert_eq!(app.transition.unwrap().direction, 1);
        assert!(!app.show_changes);
        for (digit, view) in ('1'..='4').zip(View::ALL) {
            assert!(app.navigation_key(KeyCode::Char(digit)));
            assert_eq!(app.view, view);
        }
        assert!(app.navigation_key(KeyCode::Char(']')));
        assert_eq!(app.view, View::Overview);
        assert!(app.navigation_key(KeyCode::Char('[')));
        assert_eq!(app.view, View::Changes);
        app.navigate_view(i32::MAX);
        assert_eq!(app.view, View::History);
        app.navigate_view(i32::MIN);
        assert_eq!(app.view, View::History);
    }

    #[test]
    fn switching_screens_preserves_session_work_and_explorer_state() {
        let mut app = demo();
        app.query = "codex".into();
        app.reconcile_selection();
        let selected = app.selected.unwrap();
        app.toggle_pin();
        app.start_rename();
        app.rename_text = "codex project 3".into();
        app.rename_key(KeyCode::Enter);
        app.navigation_key(KeyCode::Enter);
        app.tree_action(KeyCode::Char('C'));
        let rows = app.explorer.visible_rows(app.selected_session().unwrap());
        let process = app.explorer.selected();
        app.paused = true;
        app.sort = Sort::Growth;
        let sample_tick = app.sample_tick;
        for view in [
            View::History,
            View::Changes,
            View::Overview,
            View::ProcessMap,
        ] {
            app.set_view(view);
            assert_eq!(app.selected, Some(selected));
            assert_eq!(app.explorer.selected(), process);
            assert_eq!(
                app.explorer.visible_rows(app.selected_session().unwrap()),
                rows
            );
            assert_eq!(app.query, "codex");
            assert_eq!(app.nicknames[&selected], "codex project 3");
            assert!(app.pins.contains(&selected));
            assert!(app.paused);
            assert_eq!(app.sort, Sort::Growth);
            assert_eq!(app.sample_tick, sample_tick);
            assert!(app.focused);
            assert_eq!(app.focus, Focus::Processes);
        }
        app.set_view(View::Overview);
        app.navigation_key(KeyCode::Right);
        assert_eq!(app.view, View::ProcessMap);
    }

    #[test]
    fn text_editors_capture_shortcuts_and_block_mouse_tabs() {
        let mut app = demo();
        app.start_search();
        for character in "1234[]gmi".chars() {
            assert!(!app.navigation_key(KeyCode::Char(character)));
            app.search_key(KeyCode::Char(character));
        }
        assert_eq!(app.query, "1234[]gmi");
        assert_eq!(app.view, View::Overview);
        app.click(ui::HitTarget::View(View::History));
        assert_eq!(app.view, View::Overview);
        app.search_key(KeyCode::Esc);
        app.start_rename();
        for character in "1234[]gmi".chars() {
            assert!(!app.navigation_key(KeyCode::Char(character)));
            app.rename_key(KeyCode::Char(character));
        }
        assert_eq!(app.rename_text, "1234gmi");
        app.click(ui::HitTarget::View(View::ProcessMap));
        assert_eq!(app.view, View::Overview);
        assert_eq!(app.graph_mode, GraphMode::Constellation);
        assert_eq!(app.metric, Metric::Rss);
    }

    #[test]
    fn guides_own_arrows_and_shortcuts_without_navigation() {
        let mut app = demo();
        for tour in [false, true] {
            app.help = !tour;
            app.tour = tour;
            for key in [
                KeyCode::Left,
                KeyCode::Right,
                KeyCode::Char('2'),
                KeyCode::Char(']'),
                KeyCode::Enter,
            ] {
                assert!(!app.navigation_key(key));
                assert_eq!(app.view, View::Overview);
            }
            app.click(ui::HitTarget::View(View::Changes));
            assert_eq!(app.view, View::Overview);
        }
    }

    #[test]
    fn inspector_arrows_fold_but_brackets_and_digits_still_switch_views() {
        let mut app = demo();
        app.navigation_key(KeyCode::Enter);
        assert_eq!(app.view, View::ProcessMap);
        assert!(app.focused);
        assert_eq!(app.focus, Focus::Processes);
        let expanded = app
            .explorer
            .visible_rows(app.selected_session().unwrap())
            .len();
        assert!(expanded > 1);
        app.navigation_key(KeyCode::Left);
        assert_eq!(app.view, View::ProcessMap);
        assert_eq!(
            app.explorer
                .visible_rows(app.selected_session().unwrap())
                .len(),
            1
        );
        app.navigation_key(KeyCode::Right);
        assert_eq!(app.view, View::ProcessMap);
        assert_eq!(
            app.explorer
                .visible_rows(app.selected_session().unwrap())
                .len(),
            expanded
        );
        app.navigation_key(KeyCode::Char(']'));
        assert_eq!(app.view, View::History);
        app.navigation_key(KeyCode::Char('2'));
        assert_eq!(app.view, View::ProcessMap);
        assert!(app.focused); // Returning to a screen restores its inspector choice.
        app.navigation_key(KeyCode::Char('2'));
        assert!(app.focused);
        app.navigation_key(KeyCode::Char('i'));
        assert!(!app.focused);
        app.navigation_key(KeyCode::Right);
        assert_eq!(app.view, View::History);
    }

    #[test]
    fn graph_only_navigation_moves_sessions_not_hidden_processes() {
        let mut app = demo();
        app.navigation_key(KeyCode::Char('2'));
        let initial = app.selected;
        app.move_cursor(1);
        assert_ne!(app.selected, initial);
        app.navigation_key(KeyCode::Char('i'));
        let selected = app.selected;
        app.move_cursor(1);
        assert_eq!(app.selected, selected);
        assert_ne!(app.explorer.selected(), selected);
        app.navigation_key(KeyCode::Char('i'));
        assert!(!app.focused);
        app.move_cursor(-1);
        assert_eq!(app.selected, initial);
    }

    #[test]
    fn changes_are_a_screen_and_close_to_overview() {
        let mut app = demo();
        app.open_changes();
        assert_eq!(app.view, View::Changes);
        app.changes_key(KeyCode::Esc);
        assert_eq!(app.view, View::Overview);
        assert!(!app.show_changes);
        app.navigation_key(KeyCode::Char('4'));
        assert!(app.show_changes);
        app.changes_key(KeyCode::Char('e'));
        assert_eq!(app.view, View::Overview);
    }

    #[test]
    fn graph_and_metric_modes_persist_and_respect_view_and_editing_context() {
        let mut app = demo();
        assert!(!app.navigation_key(KeyCode::Char('g')));
        assert!(!app.navigation_key(KeyCode::Char('m')));
        app.set_view(View::ProcessMap);
        app.navigation_key(KeyCode::Char('g'));
        app.navigation_key(KeyCode::Char('m'));
        assert_eq!(app.graph_mode, GraphMode::Heatmap);
        assert_eq!(app.metric, Metric::Cpu);
        app.set_view(View::History);
        assert!(!app.navigation_key(KeyCode::Char('g')));
        assert_eq!(app.graph_mode, GraphMode::Heatmap);
        app.start_search();
        assert!(!app.navigation_key(KeyCode::Char('m')));
        assert_eq!(app.metric, Metric::Cpu);
        app.search_key(KeyCode::Esc);
        app.navigation_key(KeyCode::Char('m'));
        assert_eq!(app.metric, Metric::Rss);
        app.set_view(View::Overview);
        app.set_view(View::Changes);
        app.set_view(View::ProcessMap);
        assert_eq!(app.graph_mode, GraphMode::Heatmap);
        assert_eq!(app.metric, Metric::Rss);
        app.navigation_key(KeyCode::Char('g'));
        assert_eq!(app.graph_mode, GraphMode::Constellation);
        assert!(!app.navigation_key(KeyCode::Char('m')));
        assert_eq!(app.metric, Metric::Rss);
    }

    #[test]
    fn heatmap_cursor_and_tab_switch_between_processes_and_families() {
        let mut app = demo();
        app.set_view(View::ProcessMap);
        app.navigation_key(KeyCode::Char('g'));
        assert_eq!(app.graph_mode, GraphMode::Heatmap);
        assert!(!app.focused);
        assert_eq!(app.focus, Focus::Processes);
        let family = app.selected;
        let first = app.heatmap_processes()[0].key;
        app.click(ui::HitTarget::Process(first));
        app.move_cursor(1);
        assert_eq!(app.selected, family);
        assert_ne!(app.explorer.selected(), Some(first));
        app.move_cursor(-1);
        assert_eq!(app.explorer.selected(), Some(first));
        app.navigation_key(KeyCode::Tab);
        assert_eq!(app.focus, Focus::Sessions);
        app.move_cursor(1);
        assert_ne!(app.selected, family);
        let next_family = app.selected;
        app.navigation_key(KeyCode::BackTab);
        assert_eq!(app.focus, Focus::Processes);
        app.move_cursor(i32::MIN);
        let first = app.explorer.selected();
        app.move_cursor(1);
        assert_eq!(app.selected, next_family);
        assert_ne!(app.explorer.selected(), first);
        app.navigation_key(KeyCode::Right);
        assert_eq!(app.view, View::History); // Hidden inspector does not own arrows.
    }

    #[test]
    fn graph_metric_and_inspector_toggles_retain_the_process_identity() {
        let mut app = demo();
        app.navigation_key(KeyCode::Enter);
        app.move_cursor(1);
        let process = app.explorer.selected();
        let family = app.selected;
        app.navigation_key(KeyCode::Char('g'));
        assert!(!app.focused);
        assert_eq!(app.focus, Focus::Processes);
        assert_eq!(app.graph_mode, GraphMode::Heatmap);
        for key in [KeyCode::Char('m'), KeyCode::Char('g'), KeyCode::Char('g')] {
            app.navigation_key(key);
            assert_eq!(app.explorer.selected(), process);
            assert_eq!(app.selected, family);
            assert!(!app.focused);
        }
        app.navigation_key(KeyCode::Char('i'));
        assert!(app.focused);
        app.navigation_key(KeyCode::Char('i'));
        assert!(!app.focused);
        assert_eq!(app.focus, Focus::Processes);
        assert_eq!(app.explorer.selected(), process);
        app.navigation_key(KeyCode::Char('g'));
        assert_eq!(app.focus, Focus::Sessions);
        assert_eq!(app.graph_mode, GraphMode::Constellation);
        assert_eq!(app.explorer.selected(), process);
    }

    #[test]
    fn heatmap_order_is_shared_and_unknown_cpu_is_not_zero() {
        let mut app = demo();
        let selected = app.selected.unwrap();
        let session = app
            .snapshot
            .sessions
            .iter_mut()
            .find(|session| session.key == selected)
            .unwrap();
        for (index, process) in session.processes.iter_mut().enumerate() {
            process.rss_bytes = (1_u64 << 54) + index as u64;
            process.cpu_percent = match index {
                0 => None,
                1 => Some(f32::NAN),
                2 => Some(-3.0),
                3 => Some(0.0),
                _ => Some(25.0),
            };
        }
        assert!(
            app.heatmap_processes()
                .windows(2)
                .all(|pair| pair[0].rss_bytes >= pair[1].rss_bytes)
        );
        app.metric = Metric::Cpu;
        let cpus = app.heatmap_processes();
        assert!(cpus[0].cpu_percent.is_some_and(|value| value == 25.0));
        assert!(cpus.windows(2).all(|pair| {
            let known = |p: &ProcessSnapshot| {
                p.cpu_percent
                    .filter(|v| v.is_finite() && *v >= 0.0)
                    .unwrap_or(-1.0)
            };
            known(pair[0]) > known(pair[1])
                || (known(pair[0]) == known(pair[1]) && pair[0].key.pid < pair[1].key.pid)
        }));
        assert_eq!(
            cpus.iter()
                .filter(|p| p.cpu_percent.is_some_and(|v| v.is_finite() && v >= 0.0))
                .count(),
            cpus.len() - 3
        );
    }

    #[test]
    fn heatmap_can_visit_and_click_every_process_despite_tree_folds() {
        let mut app = demo();
        app.set_view(View::ProcessMap);
        app.tree_action(KeyCode::Char('C'));
        assert_eq!(
            app.explorer
                .visible_rows(app.selected_session().unwrap())
                .len(),
            1
        );
        app.navigation_key(KeyCode::Char('g'));
        let ordered: Vec<_> = app
            .heatmap_processes()
            .iter()
            .map(|process| process.key)
            .collect();
        app.move_cursor(i32::MIN);
        assert_eq!(app.explorer.selected(), ordered.first().copied());
        for key in &ordered {
            assert_eq!(app.explorer.selected(), Some(*key));
            app.move_cursor(1);
        }
        assert_eq!(app.explorer.selected(), ordered.last().copied());
        app.move_cursor(i32::MAX);
        assert_eq!(app.explorer.selected(), ordered.last().copied());
        app.tree_action(KeyCode::Char('C'));
        for key in ordered {
            app.click(ui::HitTarget::Process(key));
            assert_eq!(app.explorer.selected(), Some(key));
        }
    }

    #[test]
    fn paused_stale_and_resuming_data_suppress_and_end_transitions() {
        let mut app = demo();
        for condition in 0..4 {
            app.paused = false;
            app.sampler_failed = false;
            app.resuming = false;
            app.last_sample = Instant::now();
            app.set_view(View::Overview);
            app.set_view(View::History);
            assert!(app.transition_progress().is_some());
            match condition {
                0 => app.paused = true,
                1 => app.last_sample = Instant::now() - Duration::from_secs(60),
                2 => app.resuming = true,
                _ => app.sampler_failed = true,
            }
            assert!(app.transition_progress().is_none());
            assert!(app.finish_transition());
            assert!(app.transition.is_none());
            app.set_view(View::ProcessMap);
            assert!(app.transition.is_none());
            assert_eq!(app.view, View::ProcessMap);
        }
    }

    #[test]
    fn transitions_are_bounded_replaceable_and_reduced_motion_is_instant() {
        let mut app = demo();
        assert!(app.transition_progress().is_none());
        app.set_view(View::History);
        let transition = app.transition.unwrap();
        assert_eq!(transition.from, View::Overview);
        assert_eq!(transition.to, View::History);
        assert!(
            app.transition_progress()
                .is_some_and(|progress| (0.0..1.0).contains(&progress))
        );
        app.transition.as_mut().unwrap().started = Instant::now() - Duration::from_millis(80);
        let progress = app.transition_progress().unwrap();
        assert!((0.5..1.0).contains(&progress));
        app.set_view(View::Changes);
        assert_eq!(app.transition.unwrap().from, View::History);
        app.transition.as_mut().unwrap().started = Instant::now() - VIEW_TRANSITION_DURATION;
        assert!(app.transition_progress().is_none());
        assert!(app.finish_transition());
        assert!(app.transition.is_none());
        assert!(!app.finish_transition());
        app.no_motion = true;
        app.navigate_view(1);
        assert_eq!(app.view, View::Overview);
        assert!(app.transition.is_none());
        app.no_motion = false;
        app.navigate_view(1);
        assert!(app.transition.is_some());
        app.no_motion = true;
        assert!(app.transition_progress().is_none());
        assert!(app.finish_transition());
        assert!(app.transition.is_none());
    }
}
