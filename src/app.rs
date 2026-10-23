use crate::{
    explorer::Explorer,
    insights::{Insights, rss_delta},
    model::{ProcessKey, SessionSnapshot, Snapshot},
    sampling::{DemoSampler, LiveSampler},
    ui,
};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEventKind,
    },
    execute,
};
use ratatui::layout::Rect;
use std::{
    collections::HashSet,
    io,
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

pub struct App {
    pub snapshot: Snapshot,
    pub selected: Option<ProcessKey>,
    pub sort: Sort,
    pub focused: bool,
    pub focus: Focus,
    pub explorer: Explorer,
    pub insights: Insights,
    pub pins: HashSet<ProcessKey>,
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
    pub sampler_failed: bool,
    pub resuming: bool,
}

impl App {
    pub fn new(snapshot: Snapshot) -> Self {
        let mut insights = Insights::default();
        insights.update(&snapshot);
        let requested_all = snapshot.all_processes;
        let mut app = Self {
            snapshot,
            selected: None,
            sort: Sort::Memory,
            focused: false,
            focus: Focus::Sessions,
            explorer: Explorer::default(),
            insights,
            pins: HashSet::new(),
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
            sampler_failed: false,
            resuming: false,
        };
        app.reconcile_selection();
        app
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
    pub fn scope_label(&self) -> &'static str {
        match (self.snapshot.all_processes, self.requested_all) {
            (false, true) => "AGENTS → ALL",
            (true, false) => "ALL → AGENTS",
            (true, true) => "ALL FAMILIES",
            (false, false) => "AGENTS",
        }
    }
    pub fn sample_marker(&self) -> &'static str {
        if self.no_motion || self.paused || self.stale() || self.sample_tick.is_multiple_of(2) {
            "●"
        } else {
            "·"
        }
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
        if self.snapshot.all_processes != snapshot.all_processes {
            self.insights.mark_gap();
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
        if self.focused && self.focus == Focus::Processes {
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
        match target {
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
                    self.explorer.select_key(session, key);
                    self.focus = Focus::Processes;
                }
            }
        }
    }
}

struct TerminalGuard {
    signals: Vec<signal_hook::SigId>,
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableMouseCapture);
        ratatui::restore();
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
) -> io::Result<()> {
    // A bounded channel decouples potentially slow process reads from input/rendering.
    let stop = Arc::new(AtomicBool::new(false));
    let show_all = Arc::new(AtomicBool::new(all));
    let paused = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::sync_channel(1);
    let generation = Arc::new(AtomicU64::new(0));
    let signals = [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ]
    .into_iter()
    .map(|signal| signal_hook::flag::register(signal, stop.clone()))
    .collect::<io::Result<Vec<_>>>()?;
    let worker_stop = stop.clone();
    let worker_all = show_all.clone();
    let worker_pause = paused.clone();
    let worker_generation = generation.clone();
    let worker = thread::spawn(move || {
        let mut demo_sampler = demo.then(DemoSampler::new);
        let mut live_sampler = (!demo).then(LiveSampler::new);
        while !worker_stop.load(Ordering::Relaxed) {
            let start = Instant::now();
            let sample_generation = worker_generation.load(Ordering::Acquire);
            if !worker_pause.load(Ordering::Relaxed) {
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
            for signal in signals {
                signal_hook::low_level::unregister(signal);
            }
            return result.map(|_| ());
        }
    };
    let mut app = App::new(initial);
    app.interval = interval;
    app.tour = tour;
    app.no_motion = no_motion;
    let mut terminal = ratatui::init();
    let guard = TerminalGuard { signals };
    execute!(io::stdout(), EnableMouseCapture)?;
    let outcome = (|| -> io::Result<()> {
        let mut dirty = true;
        let mut last_draw = Instant::now();
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            match rx.try_recv() {
                Ok((epoch, next)) => {
                    if !app.paused && epoch == generation.load(Ordering::Acquire) {
                        app.update(next);
                        dirty = true;
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(io::Error::other("process sampler stopped unexpectedly"));
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            if dirty || last_draw.elapsed() >= Duration::from_secs(1) {
                terminal.draw(|frame| {
                    app.hitboxes = ui::draw(frame, &app);
                })?;
                dirty = false;
                last_draw = Instant::now();
            }
            if event::poll(Duration::from_millis(80))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        dirty = true;
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                            && key.code == KeyCode::Char('c')
                        {
                            break;
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
                        match key.code {
                            KeyCode::Down | KeyCode::Char('j') => app.move_cursor(1),
                            KeyCode::Up | KeyCode::Char('k') => app.move_cursor(-1),
                            KeyCode::PageDown => app.move_cursor(8),
                            KeyCode::PageUp => app.move_cursor(-8),
                            KeyCode::Tab | KeyCode::BackTab => {
                                if app.focused {
                                    app.focus = if app.focus == Focus::Sessions {
                                        Focus::Processes
                                    } else {
                                        Focus::Sessions
                                    };
                                } else {
                                    app.select(if key.code == KeyCode::Tab { 1 } else { -1 });
                                }
                            }
                            KeyCode::Enter if app.focused && app.focus == Focus::Processes => {
                                app.tree_action(key.code)
                            }
                            KeyCode::Enter => {
                                app.focused = true;
                                app.focus = Focus::Processes;
                            }
                            KeyCode::Left
                            | KeyCode::Right
                            | KeyCode::Char('h' | 'l' | 'E' | 'C')
                                if app.focused && app.focus == Focus::Processes =>
                            {
                                app.tree_action(key.code)
                            }
                            KeyCode::Esc => {
                                if app.focused {
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
                            KeyCode::Char('*') => app.toggle_pin(),
                            KeyCode::Char('s') => {
                                app.sort = match app.sort {
                                    Sort::Memory => Sort::Cpu,
                                    Sort::Cpu => Sort::Growth,
                                    Sort::Growth => Sort::Memory,
                                }
                            }
                            KeyCode::Char('a') => {
                                app.requested_all = !app.requested_all;
                                show_all.store(app.requested_all, Ordering::Relaxed);
                                generation.fetch_add(1, Ordering::AcqRel);
                                while rx.try_recv().is_ok() {}
                                if !app.paused {
                                    app.resuming = true;
                                }
                            }
                            KeyCode::Char(' ') | KeyCode::Char('p') => {
                                app.paused = !app.paused;
                                if !app.paused {
                                    app.insights.mark_gap();
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
                        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp if app.help => {
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
                        MouseEventKind::Down(MouseButton::Left) if !app.help && !app.tour => {
                            if let Some((_, key)) = app
                                .hitboxes
                                .iter()
                                .find(|(r, _)| r.contains((mouse.column, mouse.row).into()))
                            {
                                app.click(*key);
                                dirty = true;
                            }
                        }
                        MouseEventKind::ScrollDown if !app.help && !app.tour => {
                            app.move_cursor(1);
                            dirty = true;
                        }
                        MouseEventKind::ScrollUp if !app.help && !app.tour => {
                            app.move_cursor(-1);
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
    })();
    stop.store(true, Ordering::Relaxed);
    drop(guard);
    // Detach if a platform read stalls: terminal restoration must not wait on the sampler.
    if worker.is_finished() {
        let _ = worker.join();
    }
    outcome
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
    fn sample_marker_changes_only_for_received_samples_and_can_be_disabled() {
        let mut sampler = DemoSampler::new();
        let mut app = App::new(sampler.sample(false));
        let initial = app.sample_marker();
        app.select(1);
        assert_eq!(app.sample_marker(), initial);
        app.update(sampler.sample(false));
        assert_ne!(app.sample_marker(), initial);
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
