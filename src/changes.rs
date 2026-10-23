//! Bounded, ephemeral observations, never a process audit log or a diagnosis.
//!
//! Compare consecutive complete samples only. Emit a family change whenever its
//! observed membership changes, or its summed RSS grows by at least the larger
//! of 32 MiB and 5% of its previous RSS. Small increments do not accumulate here;
//! longer-window growth belongs in `insights`. A decrease alone is not an alert.
//! Counts and contributors describe two observations, not actual spawn/exit
//! events, allocation ownership, leaks, or causes of disappearance. Shared pages
//! can be counted in more than one process's RSS.

use crate::model::{ProcessKey, Snapshot, sanitize_name};
use std::collections::{HashMap, VecDeque, hash_map::Entry};

pub const EVENT_LIMIT: usize = 128;
pub const PROCESS_STATE_LIMIT: usize = 16_384;
pub const SESSION_STATE_LIMIT: usize = 16_384;
pub const MIN_GROWTH_BYTES: u64 = 32 * 1024 * 1024;
pub const CONTRIBUTOR_LIMIT: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Appeared,
    Disappeared,
    Changed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContributorStatus {
    /// The same PID and creation time were observed in both samples.
    Present,
    Appeared,
    Disappeared,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RssContributor {
    pub key: ProcessKey,
    /// Terminal-safe basename, at most 40 ASCII characters.
    pub name: String,
    /// Observed change, saturating to i64. An appearance is measured from zero;
    /// a disappearance is measured to zero, not a claim of memory being freed.
    pub delta_bytes: i64,
    pub status: ContributorStatus,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChangeEvent {
    /// Increasing event order, independent of sampler clock restarts.
    pub sequence: u64,
    /// Finite, nonnegative sampler time; only comparable within a clock epoch.
    pub monotonic: f64,
    pub session_key: ProcessKey,
    /// Sanitized label, at most 40 ASCII characters.
    pub session_name: String,
    pub kind: ChangeKind,
    /// Difference of saturating, deduplicated summed RSS, saturating to i64.
    pub rss_delta_bytes: i64,
    pub appeared: usize,
    pub disappeared: usize,
    /// True only for comparable samples meeting the documented growth threshold.
    pub significant_growth: bool,
    /// Up to three largest absolute signed RSS changes, with ProcessKey ties.
    /// This is not a full decomposition of the aggregate change.
    pub contributors: Vec<RssContributor>,
}

#[derive(Debug)]
struct ProcessState {
    name: String,
    rss_bytes: u64,
}

#[derive(Debug)]
struct SessionState {
    name: String,
    processes: HashMap<ProcessKey, ProcessState>,
    rss_bytes: u64,
}

#[derive(Debug)]
struct Baseline {
    monotonic: f64,
    scope: (bool, bool),
    sessions: HashMap<ProcessKey, SessionState>,
    // Preserve input order without sorting all sessions for each sample.
    order: Vec<ProcessKey>,
}

/// A bounded 128-event ring and at most 16,384 previous process/session states.
/// No persistence, process arguments, paths, environment, or network access.
#[derive(Debug, Default)]
pub struct Changes {
    baseline: Option<Baseline>,
    events: VecDeque<ChangeEvent>,
    next_sequence: u64,
    dropped_events: u64,
}

impl Changes {
    /// Oldest first. Use `.iter().rev()` for a newest-first activity panel.
    pub fn events(&self) -> &VecDeque<ChangeEvent> {
        &self.events
    }

    /// Events removed only because the history ring filled, not sample gaps.
    pub fn dropped_events(&self) -> u64 {
        self.dropped_events
    }

    /// Preserve history but discard continuity across a pause, scope switch,
    /// sampler failure, or any other unobserved interval. Next sample primes.
    pub fn mark_gap(&mut self) {
        self.baseline = None;
    }

    /// First samples prime without an appearance flood. Equal/reversed times
    /// replace the baseline without events; invalid times, truncated or oversized
    /// samples break continuity. Scope/source changes automatically re-prime too.
    /// Identical duplicate process keys in one family are counted once; conflicting
    /// RSS or duplicate ownership across families invalidates that sample.
    pub fn observe(&mut self, snapshot: &Snapshot) {
        let Some(current) = Baseline::from_snapshot(snapshot) else {
            self.mark_gap();
            return;
        };
        let Some(mut previous) = self.baseline.take() else {
            self.baseline = Some(current);
            return;
        };
        if current.scope != previous.scope || current.monotonic <= previous.monotonic {
            self.baseline = Some(current);
            return;
        }

        for key in &current.order {
            let session = &current.sessions[key];
            match previous.sessions.remove(key) {
                Some(old) => {
                    if let Some(event) = compare(*key, &old, session, current.monotonic) {
                        self.push(event);
                    }
                }
                None => self.push(lifecycle_event(
                    *key,
                    session,
                    current.monotonic,
                    ChangeKind::Appeared,
                )),
            }
        }
        for key in previous.order {
            if let Some(old) = previous.sessions.remove(&key) {
                self.push(lifecycle_event(
                    key,
                    &old,
                    current.monotonic,
                    ChangeKind::Disappeared,
                ));
            }
        }
        // Replacing, rather than extending, evicts every absent session/process.
        self.baseline = Some(current);
    }

    fn push(&mut self, mut event: ChangeEvent) {
        self.next_sequence = self.next_sequence.saturating_add(1);
        event.sequence = self.next_sequence;
        if self.events.len() == EVENT_LIMIT {
            self.events.pop_front();
            self.dropped_events = self.dropped_events.saturating_add(1);
        }
        self.events.push_back(event);
    }
}

impl Baseline {
    fn from_snapshot(snapshot: &Snapshot) -> Option<Self> {
        if !snapshot.monotonic.is_finite()
            || snapshot.monotonic < 0.0
            || snapshot.truncated
            || snapshot.sessions.len() > SESSION_STATE_LIMIT
        {
            return None;
        }
        // Check before allocating. Input entries, including duplicates, are capped.
        let process_entries = snapshot
            .sessions
            .iter()
            .try_fold(0_usize, |total, s| total.checked_add(s.processes.len()))?;
        if process_entries > PROCESS_STATE_LIMIT {
            return None;
        }

        let mut baseline = Self {
            monotonic: snapshot.monotonic,
            scope: (snapshot.all_processes, snapshot.demo),
            sessions: HashMap::new(),
            order: Vec::new(),
        };
        let mut owners = HashMap::new();
        for session in &snapshot.sessions {
            let state = match baseline.sessions.entry(session.key) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    baseline.order.push(session.key);
                    entry.insert(SessionState {
                        name: sanitize_name(&session.label),
                        processes: HashMap::new(),
                        rss_bytes: 0,
                    })
                }
            };
            for process in &session.processes {
                if let Some(owner) = owners.insert(process.key, session.key)
                    && owner != session.key
                {
                    return None;
                }
                match state.processes.entry(process.key) {
                    Entry::Occupied(entry) => {
                        if entry.get().rss_bytes != process.rss_bytes {
                            return None;
                        }
                    }
                    Entry::Vacant(entry) => {
                        state.rss_bytes = state.rss_bytes.saturating_add(process.rss_bytes);
                        entry.insert(ProcessState {
                            name: sanitize_name(&process.name),
                            rss_bytes: process.rss_bytes,
                        });
                    }
                }
            }
        }
        Some(baseline)
    }
}

fn compare(
    key: ProcessKey,
    previous: &SessionState,
    current: &SessionState,
    monotonic: f64,
) -> Option<ChangeEvent> {
    let delta = i128::from(current.rss_bytes) - i128::from(previous.rss_bytes);
    // Integer ceiling avoids float error at the inclusive 5% boundary.
    let threshold = MIN_GROWTH_BYTES.max(previous.rss_bytes.div_ceil(20));
    let significant_growth = delta >= i128::from(threshold);
    let mut appeared = 0;
    let mut disappeared = 0;
    let mut contributors = TopContributors::default();
    for (key, process) in &current.processes {
        let (delta, status) = match previous.processes.get(key) {
            Some(old) => (
                i128::from(process.rss_bytes) - i128::from(old.rss_bytes),
                ContributorStatus::Present,
            ),
            None => {
                appeared += 1;
                (i128::from(process.rss_bytes), ContributorStatus::Appeared)
            }
        };
        contributors.insert(*key, &process.name, delta, status);
    }
    for (key, old) in &previous.processes {
        if !current.processes.contains_key(key) {
            disappeared += 1;
            contributors.insert(
                *key,
                &old.name,
                -i128::from(old.rss_bytes),
                ContributorStatus::Disappeared,
            );
        }
    }
    (significant_growth || appeared > 0 || disappeared > 0).then(|| ChangeEvent {
        sequence: 0,
        monotonic,
        session_key: key,
        session_name: current.name.clone(),
        kind: ChangeKind::Changed,
        rss_delta_bytes: clamp_delta(delta),
        appeared,
        disappeared,
        significant_growth,
        contributors: contributors.finish(),
    })
}

fn lifecycle_event(
    key: ProcessKey,
    session: &SessionState,
    monotonic: f64,
    kind: ChangeKind,
) -> ChangeEvent {
    let appeared = kind == ChangeKind::Appeared;
    let sign = if appeared { 1 } else { -1 };
    let status = if appeared {
        ContributorStatus::Appeared
    } else {
        ContributorStatus::Disappeared
    };
    let mut contributors = TopContributors::default();
    for (key, process) in &session.processes {
        contributors.insert(
            *key,
            &process.name,
            sign * i128::from(process.rss_bytes),
            status,
        );
    }
    ChangeEvent {
        sequence: 0,
        monotonic,
        session_key: key,
        session_name: session.name.clone(),
        kind,
        rss_delta_bytes: clamp_delta(sign * i128::from(session.rss_bytes)),
        appeared: if appeared { session.processes.len() } else { 0 },
        disappeared: if appeared { 0 } else { session.processes.len() },
        significant_growth: false,
        contributors: contributors.finish(),
    }
}

#[derive(Default)]
struct TopContributors<'a>(Vec<(ProcessKey, &'a str, i128, ContributorStatus)>);

impl<'a> TopContributors<'a> {
    fn insert(&mut self, key: ProcessKey, name: &'a str, delta: i128, status: ContributorStatus) {
        if delta == 0 && status == ContributorStatus::Present {
            return;
        }
        let position = self
            .0
            .iter()
            .position(|(old_key, _, old_delta, _)| {
                delta.abs() > old_delta.abs() || (delta.abs() == old_delta.abs() && key < *old_key)
            })
            .unwrap_or(self.0.len());
        if position < CONTRIBUTOR_LIMIT {
            if self.0.len() == CONTRIBUTOR_LIMIT {
                self.0.pop();
            }
            self.0.insert(position, (key, name, delta, status));
        }
    }

    fn finish(self) -> Vec<RssContributor> {
        self.0
            .into_iter()
            .map(|(key, name, delta, status)| RssContributor {
                key,
                name: name.to_owned(),
                delta_bytes: clamp_delta(delta),
                status,
            })
            .collect()
    }
}

fn clamp_delta(delta: i128) -> i64 {
    delta.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProcessSnapshot, SessionSnapshot};

    const MIB: u64 = 1024 * 1024;

    fn key(pid: u32) -> ProcessKey {
        ProcessKey { pid, started: 10 }
    }

    fn process(pid: u32, rss_bytes: u64) -> ProcessSnapshot {
        ProcessSnapshot {
            key: key(pid),
            name: format!("worker-{pid}"),
            rss_bytes,
            ..Default::default()
        }
    }

    fn session(pid: u32, processes: Vec<ProcessSnapshot>) -> SessionSnapshot {
        SessionSnapshot {
            key: key(pid),
            label: "codex".into(),
            processes,
            ..Default::default()
        }
    }

    fn snapshot(monotonic: f64, sessions: Vec<SessionSnapshot>) -> Snapshot {
        Snapshot {
            monotonic,
            sessions,
            ..Default::default()
        }
    }

    fn one(monotonic: f64, rss: u64) -> Snapshot {
        snapshot(monotonic, vec![session(1, vec![process(1, rss)])])
    }

    #[test]
    fn first_sample_primes_and_repeated_state_never_spams() {
        let mut changes = Changes::default();
        changes.observe(&one(0.0, 100 * MIB));
        changes.observe(&one(1.0, 100 * MIB));
        assert!(changes.events().is_empty());
        changes.observe(&one(2.0, 132 * MIB));
        let event = changes.events().back().unwrap();
        assert_eq!(event.sequence, 1);
        assert_eq!(event.kind, ChangeKind::Changed);
        assert_eq!(event.rss_delta_bytes, 32 * MIB as i64);
        assert!(event.significant_growth);
        assert_eq!((event.appeared, event.disappeared), (0, 0));
        changes.observe(&one(3.0, 132 * MIB));
        assert_eq!(changes.events().len(), 1);
    }

    #[test]
    fn absolute_growth_threshold_is_inclusive_and_per_sample() {
        let mut changes = Changes::default();
        changes.observe(&one(0.0, 0));
        changes.observe(&one(1.0, MIN_GROWTH_BYTES - 1));
        changes.observe(&one(2.0, 2 * (MIN_GROWTH_BYTES - 1)));
        assert!(changes.events().is_empty());
        changes.observe(&one(3.0, 3 * MIN_GROWTH_BYTES - 2));
        assert_eq!(changes.events().len(), 1);
    }

    #[test]
    fn relative_threshold_uses_exact_integer_ceiling() {
        let previous = 1000 * MIB + 1;
        let threshold = previous.div_ceil(20);
        for (growth, expected) in [(threshold - 1, 0), (threshold, 1), (threshold + 1, 1)] {
            let mut changes = Changes::default();
            changes.observe(&one(0.0, previous));
            changes.observe(&one(1.0, previous + growth));
            assert_eq!(changes.events().len(), expected, "growth={growth}");
        }
    }

    #[test]
    fn decreases_update_baseline_without_alerts() {
        let mut changes = Changes::default();
        changes.observe(&one(0.0, 900 * MIB));
        changes.observe(&one(1.0, 100 * MIB));
        assert!(changes.events().is_empty());
        changes.observe(&one(2.0, 132 * MIB));
        assert_eq!(
            changes.events().back().unwrap().rss_delta_bytes,
            32 * MIB as i64
        );
    }

    #[test]
    fn family_membership_reports_net_observed_keys_even_when_rss_decreases() {
        let mut changes = Changes::default();
        changes.observe(&snapshot(
            0.0,
            vec![session(1, vec![process(1, 100), process(2, 500)])],
        ));
        let changed = snapshot(1.0, vec![session(1, vec![process(1, 200), process(3, 50)])]);
        changes.observe(&changed);
        let event = changes.events().back().unwrap();
        assert_eq!((event.appeared, event.disappeared), (1, 1));
        assert_eq!(event.rss_delta_bytes, -350);
        assert!(!event.significant_growth);
        assert_eq!(event.contributors[0].key, key(2));
        assert_eq!(event.contributors[0].status, ContributorStatus::Disappeared);
        assert_eq!(event.contributors[0].delta_bytes, -500);
        assert_eq!(event.contributors[1].status, ContributorStatus::Present);
        assert_eq!(event.contributors[2].status, ContributorStatus::Appeared);
        changes.observe(&Snapshot {
            monotonic: 2.0,
            ..changed
        });
        assert_eq!(changes.events().len(), 1);
    }

    #[test]
    fn same_pid_with_new_start_is_not_the_same_process() {
        let mut changes = Changes::default();
        let initial = snapshot(0.0, vec![session(1, vec![process(2, 100)])]);
        changes.observe(&initial);
        let mut replacement = initial;
        replacement.monotonic = 1.0;
        replacement.sessions[0].processes[0].key.started += 1;
        changes.observe(&replacement);
        let event = changes.events().back().unwrap();
        assert_eq!((event.appeared, event.disappeared), (1, 1));
        assert_eq!(event.rss_delta_bytes, 0);
        assert_eq!(event.contributors.len(), 2);
        assert_eq!(event.contributors[0].status, ContributorStatus::Disappeared);
        assert_eq!(event.contributors[1].status, ContributorStatus::Appeared);
    }

    #[test]
    fn sessions_appear_disappear_and_are_evicted_without_initial_flood() {
        let mut changes = Changes::default();
        changes.observe(&snapshot(0.0, vec![]));
        changes.observe(&one(1.0, 100));
        assert_eq!(changes.events()[0].kind, ChangeKind::Appeared);
        assert_eq!(changes.events()[0].appeared, 1);
        assert!(!changes.events()[0].significant_growth);
        changes.observe(&snapshot(2.0, vec![]));
        assert_eq!(changes.events()[1].kind, ChangeKind::Disappeared);
        assert_eq!(changes.events()[1].rss_delta_bytes, -100);
        assert_eq!(changes.events()[1].disappeared, 1);
        assert!(changes.baseline.as_ref().unwrap().sessions.is_empty());
        changes.observe(&snapshot(3.0, vec![]));
        assert_eq!(changes.events().len(), 2);
        changes.observe(&one(4.0, 200));
        assert_eq!(changes.events()[2].kind, ChangeKind::Appeared);
    }

    #[test]
    fn reused_root_is_an_appearance_and_disappearance_not_a_growth() {
        let mut changes = Changes::default();
        changes.observe(&one(0.0, 100));
        let mut replacement = one(1.0, 900 * MIB);
        replacement.sessions[0].key.started += 1;
        replacement.sessions[0].processes[0].key.started += 1;
        changes.observe(&replacement);
        assert_eq!(changes.events().len(), 2);
        assert_eq!(changes.events()[0].kind, ChangeKind::Appeared);
        assert_eq!(changes.events()[1].kind, ChangeKind::Disappeared);
        assert!(
            changes
                .events()
                .iter()
                .all(|event| !event.significant_growth)
        );
        assert_eq!(changes.baseline.as_ref().unwrap().sessions.len(), 1);
    }

    #[test]
    fn explicit_gap_preserves_history_but_restarts_comparison() {
        let mut changes = Changes::default();
        changes.observe(&one(0.0, 0));
        changes.observe(&one(1.0, 32 * MIB));
        changes.mark_gap();
        assert!(changes.baseline.is_none());
        assert_eq!(changes.events().len(), 1);
        changes.observe(&snapshot(500.0, vec![]));
        assert_eq!(changes.events().len(), 1);
        changes.observe(&one(501.0, 900 * MIB));
        assert_eq!(changes.events().back().unwrap().kind, ChangeKind::Appeared);
    }

    #[test]
    fn source_and_scope_switches_prime_without_topology_floods() {
        let mut changes = Changes::default();
        changes.observe(&one(0.0, 0));
        let mut changed = one(1.0, 1000 * MIB);
        changed.all_processes = true;
        changes.observe(&changed);
        changed.monotonic = 2.0;
        changed.demo = true;
        changed.sessions.clear();
        changes.observe(&changed);
        assert!(changes.events().is_empty());
    }

    #[test]
    fn reversed_and_equal_clocks_replace_baseline_without_events() {
        for replacement_time in [0.0, 10.0] {
            let mut changes = Changes::default();
            changes.observe(&one(10.0, 0));
            changes.observe(&one(replacement_time, 900 * MIB));
            assert!(changes.events().is_empty());
            changes.observe(&one(replacement_time + 1.0, 1000 * MIB));
            assert_eq!(changes.events().len(), 1);
            assert_eq!(changes.events()[0].rss_delta_bytes, 100 * MIB as i64);
        }
    }

    #[test]
    fn invalid_clocks_and_truncated_samples_break_continuity() {
        for bad_time in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            let mut changes = Changes::default();
            changes.observe(&one(0.0, 0));
            changes.observe(&one(bad_time, 900 * MIB));
            assert!(changes.baseline.is_none());
            changes.observe(&one(2.0, 1000 * MIB));
            assert!(changes.events().is_empty());
        }
        let mut changes = Changes::default();
        changes.observe(&one(0.0, 100));
        changes.observe(&Snapshot {
            truncated: true,
            ..snapshot(1.0, vec![])
        });
        assert!(changes.baseline.is_none());
        changes.observe(&one(2.0, 1000 * MIB));
        assert!(changes.events().is_empty());
    }

    #[test]
    fn duplicate_roots_merge_and_identical_process_keys_are_counted_once() {
        let mut changes = Changes::default();
        let family = session(1, vec![process(1, 100), process(1, 100)]);
        changes.observe(&snapshot(0.0, vec![family.clone(), family]));
        let baseline = changes.baseline.as_ref().unwrap();
        assert_eq!(baseline.sessions.len(), 1);
        assert_eq!(baseline.sessions[&key(1)].processes.len(), 1);
        assert_eq!(baseline.sessions[&key(1)].rss_bytes, 100);
        changes.observe(&one(1.0, 100));
        assert!(changes.events().is_empty());
    }

    #[test]
    fn conflicting_duplicates_and_multiple_owners_break_continuity() {
        for invalid in [
            vec![session(1, vec![process(1, 100), process(1, 200)])],
            vec![
                session(1, vec![process(1, 100)]),
                session(2, vec![process(1, 100)]),
            ],
        ] {
            let mut changes = Changes::default();
            changes.observe(&one(0.0, 0));
            changes.observe(&snapshot(1.0, invalid));
            assert!(changes.baseline.is_none());
            changes.observe(&one(2.0, 900 * MIB));
            assert!(changes.events().is_empty());
        }
    }

    #[test]
    fn history_is_capped_with_visible_drop_count_and_stable_sequence() {
        let mut changes = Changes::default();
        changes.observe(&snapshot(0.0, vec![]));
        for index in 1..=150 {
            let sessions = if index % 2 == 1 {
                one(0.0, 100).sessions
            } else {
                vec![]
            };
            changes.observe(&snapshot(f64::from(index), sessions));
        }
        assert_eq!(changes.events().len(), EVENT_LIMIT);
        assert_eq!(changes.dropped_events(), 22);
        assert_eq!(changes.events().front().unwrap().sequence, 23);
        assert_eq!(changes.events().back().unwrap().sequence, 150);
        changes.mark_gap();
        assert_eq!(changes.dropped_events(), 22);
    }

    #[test]
    fn top_three_rank_signed_magnitude_and_break_ties_by_full_key() {
        let mut changes = Changes::default();
        changes.observe(&snapshot(
            0.0,
            vec![session(
                1,
                vec![
                    process(1, 100),
                    process(2, 500),
                    process(3, 100),
                    process(4, 800),
                ],
            )],
        ));
        changes.observe(&snapshot(
            1.0,
            vec![session(
                1,
                vec![
                    process(1, 600),
                    process(2, 0),
                    process(3, 200),
                    process(5, 900),
                ],
            )],
        ));
        let top = &changes.events().back().unwrap().contributors;
        assert_eq!(top.len(), 3);
        assert_eq!(top.iter().map(|c| c.key.pid).collect::<Vec<_>>(), [5, 4, 1]);
        assert_eq!(
            top.iter().map(|c| c.delta_bytes).collect::<Vec<_>>(),
            [900, -800, 500]
        );
        assert_eq!(top[0].status, ContributorStatus::Appeared);
        assert_eq!(top[1].status, ContributorStatus::Disappeared);
        assert_eq!(top[2].status, ContributorStatus::Present);
    }

    #[test]
    fn unchanged_processes_do_not_fill_the_top_contributor_list() {
        let mut changes = Changes::default();
        changes.observe(&one(0.0, 100));
        changes.observe(&snapshot(
            1.0,
            vec![session(1, vec![process(1, 100), process(2, 0)])],
        ));
        let top = &changes.events().back().unwrap().contributors;
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].key, key(2));
        assert_eq!(top[0].status, ContributorStatus::Appeared);
    }

    #[test]
    fn stored_strings_are_bounded_terminal_safe_basenames() {
        let mut changes = Changes::default();
        changes.observe(&snapshot(0.0, vec![]));
        let mut sample = one(1.0, 100);
        sample.sessions[0].label = format!("/private/secret/\u{202e}{}\n", "a".repeat(1000));
        sample.sessions[0].processes[0].name = "C:\\secret\\\x1b[31mworker\n".into();
        changes.observe(&sample);
        let event = changes.events().back().unwrap();
        assert_eq!(event.session_name, "a".repeat(40));
        assert_eq!(event.contributors[0].name, "31mworker");
        let saved = &changes.baseline.as_ref().unwrap().sessions[&key(1)];
        assert_eq!(saved.name.len(), 40);
        assert_eq!(saved.processes[&key(1)].name, "31mworker");
    }

    #[test]
    fn huge_rss_deltas_saturate_without_overflow_or_wrong_ranking() {
        let mut changes = Changes::default();
        changes.observe(&snapshot(0.0, vec![]));
        changes.observe(&snapshot(
            1.0,
            vec![session(
                1,
                vec![
                    process(1, i64::MAX as u64 + 1),
                    process(2, u64::MAX),
                    process(3, u64::MAX - 1),
                ],
            )],
        ));
        let event = changes.events().back().unwrap();
        assert_eq!(event.rss_delta_bytes, i64::MAX);
        assert_eq!(
            event
                .contributors
                .iter()
                .map(|c| c.key.pid)
                .collect::<Vec<_>>(),
            [2, 3, 1]
        );
        assert!(event.contributors.iter().all(|c| c.delta_bytes == i64::MAX));
        changes.observe(&snapshot(2.0, vec![]));
        let event = changes.events().back().unwrap();
        assert_eq!(event.rss_delta_bytes, i64::MIN);
        assert!(event.contributors.iter().all(|c| c.delta_bytes == i64::MIN));
    }

    #[test]
    fn bounded_inputs_and_empty_families_cannot_accumulate_state() {
        let mut changes = Changes::default();
        let processes = (0..PROCESS_STATE_LIMIT as u32)
            .map(|pid| process(pid, 1))
            .collect();
        changes.observe(&snapshot(0.0, vec![session(1, processes)]));
        assert_eq!(
            changes.baseline.as_ref().unwrap().sessions[&key(1)]
                .processes
                .len(),
            PROCESS_STATE_LIMIT
        );
        let oversized = (0..=PROCESS_STATE_LIMIT as u32)
            .map(|pid| process(pid, 1))
            .collect();
        changes.observe(&snapshot(1.0, vec![session(1, oversized)]));
        assert!(changes.baseline.is_none());
        let sessions = (0..SESSION_STATE_LIMIT as u32)
            .map(|pid| session(pid, vec![]))
            .collect();
        changes.observe(&snapshot(2.0, sessions));
        assert_eq!(
            changes.baseline.as_ref().unwrap().sessions.len(),
            SESSION_STATE_LIMIT
        );
        let oversized = (0..=SESSION_STATE_LIMIT as u32)
            .map(|pid| session(pid, vec![]))
            .collect();
        changes.observe(&snapshot(3.0, oversized));
        assert!(changes.baseline.is_none());
        assert!(changes.events().is_empty());
    }
}
