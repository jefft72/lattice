//! Small, presentation-neutral snapshots. No arguments, paths or environment data.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

pub const HISTORY_LIMIT: usize = 60;

/// Best-effort identity: the operating system can reuse a PID.
/// Creation times have one-second precision, so same-second reuse can still collide.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProcessKey {
    pub pid: u32,
    /// Process creation time, in Unix seconds, as reported by the OS backend.
    pub started: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProcessSnapshot {
    pub key: ProcessKey,
    pub ppid: Option<u32>,
    /// Sanitized basename only. Never a command line or executable path.
    pub name: String,
    /// Core-percent: 100 means one fully occupied logical CPU. None is priming.
    pub cpu_percent: Option<f32>,
    pub rss_bytes: u64,
    pub agent: Option<String>,
}

impl ProcessSnapshot {
    pub fn pid(&self) -> u32 {
        self.key.pid
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionSnapshot {
    pub key: ProcessKey,
    pub label: String,
    pub agent: Option<String>,
    pub processes: Vec<ProcessSnapshot>,
    /// Aggregate RSS bytes, oldest to newest, at most HISTORY_LIMIT points.
    pub history: Vec<u64>,
    pub cpu_history: Vec<Option<f32>>,
}

impl SessionSnapshot {
    pub fn pid(&self) -> u32 {
        self.key.pid
    }

    pub fn cpu_percent(&self) -> Option<f32> {
        self.processes.iter().try_fold(0.0, |total, process| {
            process.cpu_percent.map(|cpu| total + cpu)
        })
    }

    /// RSS is summed, not deduplicated physical memory; shared pages may overlap.
    pub fn rss_bytes(&self) -> u64 {
        self.processes.iter().fold(0_u64, |total, process| {
            total.saturating_add(process.rss_bytes)
        })
    }

    pub fn process_count(&self) -> usize {
        self.processes.len()
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SystemSnapshot {
    pub cpu_percent: Option<f32>,
    pub cpu_count: usize,
    pub memory_total: u64,
    pub memory_available: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub memory_history: Vec<u64>,
    pub cpu_history: Vec<Option<f32>>,
}

impl SystemSnapshot {
    pub fn memory_used(&self) -> u64 {
        self.memory_total.saturating_sub(self.memory_available)
    }

    pub fn memory_percent(&self) -> f64 {
        if self.memory_total == 0 {
            0.0
        } else {
            self.memory_used() as f64 / self.memory_total as f64 * 100.0
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    /// Unix seconds, for human-readable timestamps only.
    pub timestamp: f64,
    /// Seconds since this sampler was constructed; safe for elapsed-time displays.
    pub monotonic: f64,
    pub sessions: Vec<SessionSnapshot>,
    pub system: SystemSnapshot,
    /// Accessible, current-user processes sampled, including hidden non-agents.
    pub process_count: usize,
    /// Processes visible to the backend whose ownership could not be established.
    /// OS-hidden processes cannot be counted and are not included here.
    pub inaccessible_count: usize,
    pub all_processes: bool,
    pub demo: bool,
    /// True when a safety limit prevented some owned processes being sampled.
    pub truncated: bool,
}

/// Convert untrusted OS names to short, terminal-safe basenames. Restricting the
/// alphabet also removes ANSI escapes, bidi controls, zero-width text and markup.
pub fn sanitize_name(raw: &str) -> String {
    let basename = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let name: String = basename
        .chars()
        .take(256)
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
        .take(40)
        .collect();
    if name.is_empty() {
        "process".to_owned()
    } else {
        name
    }
}

/// Each process belongs to its nearest recognized agent ancestor, or its top
/// accessible ancestor in all-processes mode. Nested agents are separate roots.
/// Walks are iterative and memoized, so very deep trees cannot overflow the stack.
pub fn group_processes(
    processes: Vec<ProcessSnapshot>,
    all_processes: bool,
) -> Vec<SessionSnapshot> {
    let mut nodes: BTreeMap<u32, ProcessSnapshot> = BTreeMap::new();
    for mut process in processes {
        process.name = sanitize_name(&process.name);
        if let Some(agent) = process.agent.as_mut() {
            *agent = sanitize_name(agent);
        }
        let replace = nodes
            .get(&process.key.pid)
            .is_none_or(|old| old.key.started < process.key.started);
        if replace {
            nodes.insert(process.pid(), process);
        }
    }

    let mut roots: HashMap<u32, Option<u32>> = HashMap::new();
    for &pid in nodes.keys() {
        if roots.contains_key(&pid) {
            continue;
        }
        let mut path = Vec::new();
        let mut seen = HashMap::new();
        let mut cursor = pid;
        let root = loop {
            if let Some(root) = roots.get(&cursor) {
                break *root;
            }
            let node = &nodes[&cursor];
            if node.agent.is_some() {
                roots.insert(cursor, Some(cursor));
                break Some(cursor);
            }
            if let Some(&cycle_start) = seen.get(&cursor) {
                // Stable fallback for a corrupt/racing ancestry graph.
                break if all_processes {
                    path[cycle_start..].iter().copied().min()
                } else {
                    None
                };
            }
            seen.insert(cursor, path.len());
            path.push(cursor);
            let parent = node.ppid.and_then(|ppid| nodes.get(&ppid));
            match parent {
                // A newer parent cannot be the original parent: its PID was reused.
                Some(parent) if parent.key.started <= node.key.started => {
                    cursor = parent.key.pid;
                }
                _ => break all_processes.then_some(cursor),
            }
        };
        for visited in path {
            roots.insert(visited, root);
        }
    }

    let mut grouped: BTreeMap<u32, Vec<ProcessSnapshot>> = BTreeMap::new();
    for (pid, node) in &nodes {
        if let Some(Some(root)) = roots.get(pid) {
            grouped.entry(*root).or_default().push(node.clone());
        }
    }
    grouped
        .into_iter()
        .map(|(pid, mut processes)| {
            let root = &nodes[&pid];
            processes.sort_by_key(|process| (process.key.pid != pid, process.key.pid));
            SessionSnapshot {
                key: root.key,
                label: root.agent.clone().unwrap_or_else(|| root.name.clone()),
                agent: root.agent.clone(),
                processes,
                history: Vec::new(),
                cpu_history: Vec::new(),
            }
        })
        .collect()
}

#[derive(Default)]
struct Trace {
    memory: VecDeque<u64>,
    cpu: VecDeque<Option<f32>>,
}

impl Trace {
    fn push(&mut self, memory: u64, cpu: Option<f32>) {
        if self.memory.len() == HISTORY_LIMIT {
            self.memory.pop_front();
            self.cpu.pop_front();
        }
        self.memory.push_back(memory);
        self.cpu.push_back(cpu);
    }
}

/// Histories only retain live roots; dead sessions cannot accumulate in memory.
#[derive(Default)]
pub struct HistoryStore {
    sessions: HashMap<ProcessKey, Trace>,
    system: Trace,
}

impl HistoryStore {
    pub fn record(&mut self, sessions: &mut [SessionSnapshot], system: &mut SystemSnapshot) {
        let active: HashSet<ProcessKey> = sessions.iter().map(|session| session.key).collect();
        self.sessions.retain(|key, _| active.contains(key));
        for session in sessions {
            let memory = session.rss_bytes();
            let cpu = session.cpu_percent();
            let trace = self.sessions.entry(session.key).or_default();
            trace.push(memory, cpu);
            session.history = trace.memory.iter().copied().collect();
            session.cpu_history = trace.cpu.iter().copied().collect();
        }
        self.system.push(system.memory_used(), system.cpu_percent);
        system.memory_history = self.system.memory.iter().copied().collect();
        system.cpu_history = self.system.cpu.iter().copied().collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, ppid: u32, agent: Option<&str>) -> ProcessSnapshot {
        ProcessSnapshot {
            key: ProcessKey { pid, started: 100 },
            ppid: Some(ppid),
            name: agent.unwrap_or("worker").to_owned(),
            agent: agent.map(str::to_owned),
            cpu_percent: Some(10.0),
            rss_bytes: 1024,
        }
    }

    #[test]
    fn nested_agents_are_separate_and_never_double_counted() {
        let groups = group_processes(
            vec![
                process(1, 0, None),
                process(10, 1, Some("claude")),
                process(11, 10, None),
                process(12, 11, Some("codex")),
                process(13, 12, None),
                process(20, 1, Some("aider")),
            ],
            false,
        );
        assert_eq!(
            groups.iter().map(SessionSnapshot::pid).collect::<Vec<_>>(),
            [10, 12, 20]
        );
        assert_eq!(groups[0].process_count(), 2);
        assert_eq!(groups[1].process_count(), 2);
        assert_eq!(groups[0].rss_bytes(), 2048);
        let pids: Vec<_> = groups
            .iter()
            .flat_map(|g| g.processes.iter().map(|p| p.key.pid))
            .collect();
        assert_eq!(
            pids.iter().copied().collect::<HashSet<_>>().len(),
            pids.len()
        );
    }

    #[test]
    fn all_mode_keeps_ordinary_roots_separate_from_agent_roots() {
        let groups = group_processes(
            vec![
                process(1, 0, None),
                process(2, 1, None),
                process(3, 2, Some("goose")),
            ],
            true,
        );
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].process_count(), 2);
        assert_eq!(groups[1].pid(), 3);
    }

    #[test]
    fn missing_parents_and_empty_agent_view_are_valid() {
        assert!(group_processes(vec![process(2, 88, None)], false).is_empty());
        assert_eq!(
            group_processes(vec![process(2, 88, None)], true)[0].pid(),
            2
        );
        assert!(group_processes(vec![], true).is_empty());
    }

    #[test]
    fn cycles_terminate_and_have_a_deterministic_owner() {
        let nodes = vec![
            process(5, 6, None),
            process(6, 5, None),
            process(7, 6, None),
        ];
        let groups = group_processes(nodes.clone(), true);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].pid(), 5);
        assert_eq!(groups[0].process_count(), 3);
        assert!(group_processes(nodes, false).is_empty());
        assert_eq!(group_processes(vec![process(9, 9, None)], true).len(), 1);
    }

    #[test]
    fn reused_parent_pid_is_not_an_ancestor() {
        let child = process(5, 2, None);
        let mut parent = process(2, 0, Some("codex"));
        parent.key.started = 200;
        let groups = group_processes(vec![parent, child], false);
        assert_eq!(groups[0].process_count(), 1);
    }

    #[test]
    fn duplicate_pids_keep_the_newest_identity() {
        let old = process(1, 0, Some("aider"));
        let mut new = process(1, 0, Some("codex"));
        new.key.started = 101;
        let groups = group_processes(vec![new, old], true);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].label, "codex");
    }

    #[test]
    fn deep_trees_are_iterative() {
        let mut nodes = vec![process(1, 0, Some("codex"))];
        nodes.extend((2..20_000).map(|pid| process(pid, pid - 1, None)));
        let groups = group_processes(nodes, false);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].process_count(), 19_999);
    }

    #[test]
    fn names_cannot_inject_terminal_controls_or_paths() {
        assert_eq!(sanitize_name("/private/home/secret/codex"), "codex");
        assert_eq!(sanitize_name("C:\\private\\aider.exe"), "aider.exe");
        let safe = sanitize_name("\x1b[31m\u{202e}node\n");
        assert!(
            safe.chars()
                .all(|c| c.is_ascii_alphanumeric() || ".-_+".contains(c))
        );
        assert_eq!(sanitize_name("🧙"), "process");
        assert_eq!(sanitize_name(&"x".repeat(1000)).len(), 40);
    }

    #[test]
    fn histories_are_capped_and_dead_or_reused_roots_are_evicted() {
        let mut history = HistoryStore::default();
        let mut groups = group_processes(vec![process(10, 0, Some("codex"))], false);
        let mut system = SystemSnapshot::default();
        for _ in 0..100 {
            history.record(&mut groups, &mut system);
        }
        assert_eq!(groups[0].history.len(), HISTORY_LIMIT);
        assert_eq!(groups[0].cpu_history.len(), HISTORY_LIMIT);
        assert_eq!(system.memory_history.len(), HISTORY_LIMIT);
        groups[0].key.started += 1;
        history.record(&mut groups, &mut system);
        assert_eq!(groups[0].history.len(), 1);
        assert_eq!(history.sessions.len(), 1);
        history.record(&mut [], &mut system);
        assert!(history.sessions.is_empty());
    }

    #[test]
    fn system_estimates_handle_boundaries_and_unknown_memory() {
        let mut system = SystemSnapshot::default();
        assert_eq!(system.memory_percent(), 0.0);
        system.memory_total = 100;
        system.memory_available = 21;
        assert!(system.memory_percent() < 80.0);
        system.memory_available = 20;
        assert!(system.memory_percent() >= 80.0);
        system.memory_available = 10;
        assert!(system.memory_percent() >= 90.0);
        system.memory_available = 101;
        assert_eq!(system.memory_used(), 0);
    }

    #[test]
    fn cpu_is_unknown_until_every_member_is_primed() {
        let mut groups = group_processes(
            vec![process(1, 0, Some("codex")), process(2, 1, None)],
            false,
        );
        assert_eq!(groups[0].cpu_percent(), Some(20.0));
        groups[0].processes[1].cpu_percent = None;
        assert_eq!(groups[0].cpu_percent(), None);
        groups[0].processes[0].rss_bytes = u64::MAX;
        assert_eq!(groups[0].rss_bytes(), u64::MAX);
    }
}

#[cfg(test)]
mod grouping_regressions {
    use super::*;
    fn process(pid: u32, ppid: u32, agent: bool) -> ProcessSnapshot {
        ProcessSnapshot {
            key: ProcessKey { pid, started: 100 },
            ppid: Some(ppid),
            name: "worker".into(),
            cpu_percent: Some(1.0),
            rss_bytes: 1024,
            agent: agent.then(|| "codex".into()),
        }
    }
    #[test]
    fn synthetic_16k_chain_and_many_root_history_are_bounded() {
        let now = std::time::Instant::now();
        let nodes = (1..=16_384)
            .map(|pid| process(pid, if pid == 16_384 { 0 } else { pid + 1 }, pid == 16_384))
            .collect();
        let groups = group_processes(nodes, false);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].process_count(), 16_384);
        let mut roots =
            group_processes((1..=2000).map(|pid| process(pid, 0, true)).collect(), false);
        let mut h = HistoryStore::default();
        let mut system = SystemSnapshot::default();
        for _ in 0..70 {
            h.record(&mut roots, &mut system);
        }
        assert_eq!(h.sessions.len(), 2000);
        assert!(
            roots
                .iter()
                .all(|s| s.history.len() == 60 && s.cpu_history.len() == 60)
        );
        h.record(&mut [], &mut system);
        assert_eq!(h.sessions.len(), 0);
        println!(
            "16K reverse ancestry + 2K roots × 70 history records: {:?}",
            now.elapsed()
        );
    }
    #[test]
    fn grouping_matches_independent_walk_for_small_graphs() {
        let mut random = 1u64;
        for case in 0..200 {
            let mut nodes = Vec::new();
            for pid in 1..=70 {
                random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                nodes.push(process(
                    pid,
                    (random >> 32) as u32 % 76,
                    random.is_multiple_of(13),
                ));
            }
            for all in [false, true] {
                let groups = group_processes(nodes.clone(), all);
                let got: HashMap<_, _> = groups
                    .iter()
                    .flat_map(|s| s.processes.iter().map(move |p| (p.pid(), s.pid())))
                    .collect();
                for node in &nodes {
                    let mut path = Vec::new();
                    let mut cur = node.pid();
                    let expected = loop {
                        let n = &nodes[(cur - 1) as usize];
                        if n.agent.is_some() {
                            break Some(cur);
                        }
                        if let Some(start) = path.iter().position(|p| *p == cur) {
                            break if all {
                                path[start..].iter().copied().min()
                            } else {
                                None
                            };
                        }
                        path.push(cur);
                        let p = n.ppid.unwrap_or(0);
                        if p == 0 || p > 70 {
                            break all.then_some(cur);
                        }
                        cur = p;
                    };
                    assert_eq!(
                        got.get(&node.pid()).copied(),
                        expected,
                        "case={case}, pid={}, all={all}",
                        node.pid()
                    );
                }
            }
        }
    }
}
