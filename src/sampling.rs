//! Read-only OS sampling. Live and demo data have deliberately separate paths.
//!
//! No environment, working directory, executable path, network or disk refreshes
//! are requested. We inspect only whitelisted interpreter command positions, in
//! a temporary System discarded immediately after launcher identification.
//! On macOS, sysinfo may transiently read the OS argument/environment block even
//! when those refreshes are disabled. We never retain, display or log that block,
//! and never inspect environment values.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, Uid, UpdateKind};

use crate::attachment::{PidAttachment, RootObservation};
use crate::model::{
    HistoryStore, ProcessKey, ProcessSnapshot, Snapshot, SystemSnapshot, group_processes,
    sanitize_name,
};

const MAX_PROCESSES: usize = 16_384;
const MIN_DELTA: Duration = Duration::from_millis(100);

#[derive(Default)]
struct CpuDeltas {
    previous: HashMap<ProcessKey, (Duration, u64)>,
}

impl CpuDeltas {
    fn sample(&mut self, key: ProcessKey, at: Duration, cpu_ms: u64) -> Option<f32> {
        let old = self.previous.insert(key, (at, cpu_ms))?;
        let elapsed = at.checked_sub(old.0)?;
        let delta = cpu_ms.checked_sub(old.1)?;
        if elapsed < MIN_DELTA {
            return None;
        }
        let percent = delta as f64 / elapsed.as_secs_f64() / 10.0;
        percent.is_finite().then_some(percent as f32)
    }

    fn retain(&mut self, alive: &HashSet<ProcessKey>) {
        self.previous.retain(|key, _| alive.contains(key));
    }
}

fn basename(value: &str) -> &str {
    value.rsplit(['/', '\\']).next().unwrap_or(value)
}

fn known_agent(value: &str) -> Option<&'static str> {
    let name = basename(value).to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    let name = name.strip_suffix(".js").unwrap_or(name);
    let name = name.strip_suffix(".mjs").unwrap_or(name);
    match name {
        "claude" | "claude-code" => Some("claude"),
        "codex" => Some("codex"),
        "aider" => Some("aider"),
        "goose" => Some("goose"),
        "opencode" => Some("opencode"),
        _ => None,
    }
}

fn is_interpreter(name: &str) -> bool {
    let lower = basename(name).to_ascii_lowercase();
    let lower = lower.strip_suffix(".exe").unwrap_or(&lower);
    matches!(lower, "node" | "nodejs" | "bun" | "deno")
        || ["python", "pypy"].iter().any(|prefix| {
            lower
                .strip_prefix(prefix)
                .is_some_and(|version| version.chars().all(|c| c.is_ascii_digit() || c == '.'))
        })
}

fn launcher_agent(value: &str) -> Option<&'static str> {
    if let Some(agent) = known_agent(value) {
        return Some(agent);
    }
    // Anthropic's npm launcher is generically named cli.js. Only its exact
    // package suffix is sufficient evidence; a random cli.js is not an agent.
    let normalized = value.replace('\\', "/");
    normalized
        .ends_with("/@anthropic-ai/claude-code/cli.js")
        .then_some("claude")
}

/// Only executable/script/module positions are considered. Prompts, shell source,
/// arbitrary arguments and substrings such as "my-codex-project" cannot match.
fn identify_agent(name: &str, command: &[OsString]) -> Option<&'static str> {
    if let Some(agent) = known_agent(name) {
        return Some(agent);
    }
    if !is_interpreter(name) {
        return None;
    }
    if let Some(first) = command.first().and_then(|arg| arg.to_str())
        && let Some(agent) = launcher_agent(first)
    {
        return Some(agent);
    }
    let mut tokens = command.iter().skip(1).take(7);
    while let Some(token) = tokens.next().and_then(|arg| arg.to_str()) {
        match token {
            "-c" | "-e" | "--eval" | "-p" | "--print" => return None,
            "-m" => {
                let module = tokens.next()?.to_str()?;
                return match module {
                    "aider" | "aider.main" => Some("aider"),
                    "goose" => Some("goose"),
                    _ => None,
                };
            }
            "--" => return launcher_agent(tokens.next()?.to_str()?),
            "-u"
            | "-B"
            | "-E"
            | "-I"
            | "-s"
            | "-S"
            | "-O"
            | "-OO"
            | "--no-warnings"
            | "--enable-source-maps"
            | "--inspect"
            | "--inspect-brk" => {
                continue;
            }
            option if option.starts_with("--inspect=") || option.starts_with("--inspect-brk=") => {
                continue;
            }
            option if option.starts_with('-') => return None,
            script => return launcher_agent(script),
        }
    }
    None
}

fn belongs_to_user(owner: Option<&Uid>, current: Option<&Uid>) -> bool {
    matches!((owner, current), (Some(owner), Some(current)) if owner == current)
}

fn root_observation(system: &System, current: Option<&Uid>, pid: u32) -> RootObservation {
    match system
        .process(Pid::from_u32(pid))
        .filter(|process| process.exists())
    {
        Some(process) if belongs_to_user(process.user_id(), current) => {
            RootObservation::Present(ProcessKey {
                pid,
                started: process.start_time(),
            })
        }
        Some(_) => RootObservation::Unavailable,
        None => RootObservation::Missing,
    }
}

/// Preserve the explicit root even if it has a high PID. A family can still be
/// incomplete, but this cap cannot be mistaken for root disappearance.
fn limit_process_ids(pids: &mut Vec<Pid>, requested_pid: Option<u32>) -> bool {
    pids.sort_unstable_by_key(|pid| (Some(pid.as_u32()) != requested_pid, pid.as_u32()));
    let truncated = pids.len() > MAX_PROCESSES;
    pids.truncate(MAX_PROCESSES);
    truncated
}

pub struct LiveSampler {
    system: System,
    current_user: Option<Uid>,
    started: Instant,
    last_system_cpu: Option<Instant>,
    cpu: CpuDeltas,
    history: HistoryStore,
    attachment: Option<PidAttachment>,
}

impl Default for LiveSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveSampler {
    pub fn new() -> Self {
        let mut system = System::new();
        let own_pid = Pid::from_u32(std::process::id());
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[own_pid]),
            true,
            ProcessRefreshKind::nothing()
                .without_tasks()
                .with_user(UpdateKind::Always),
        );
        let current_user = system.process(own_pid).and_then(|p| p.user_id()).cloned();
        Self {
            system,
            current_user,
            started: Instant::now(),
            last_system_cpu: None,
            cpu: CpuDeltas::default(),
            history: HistoryStore::default(),
            attachment: None,
        }
    }

    /// Observe one existing current-user PID and its currently visible descendants.
    /// The first sample locks PID + start time. Loss of the root ends attachment;
    /// it never launches a process, sends a signal, or follows a reused PID.
    pub fn with_pid(pid: u32) -> Self {
        Self {
            attachment: Some(PidAttachment::new(pid)),
            ..Self::new()
        }
    }

    pub fn sample(&mut self, all_processes: bool) -> Snapshot {
        // Metadata first, so CPU, memory and launcher arguments are only requested
        // for our user. Threads are not separate nodes or duplicate RSS totals.
        // sysinfo's macOS metadata implementation may itself transiently obtain
        // the OS argument/environment block, despite these disabled refreshes.
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .without_tasks()
                .with_user(UpdateKind::Always),
        );
        if self.current_user.is_none() {
            self.current_user = self
                .system
                .process(Pid::from_u32(std::process::id()))
                .and_then(|p| p.user_id())
                .cloned();
        }
        if let Some(attachment) = self.attachment.as_mut() {
            attachment.observe(root_observation(
                &self.system,
                self.current_user.as_ref(),
                attachment.requested_pid(),
            ));
        }
        let inaccessible_count = self
            .system
            .processes()
            .values()
            .filter(|p| p.user_id().is_none())
            .count();
        let mut pids: Vec<Pid> = self
            .system
            .processes()
            .iter()
            .filter(|(_, p)| belongs_to_user(p.user_id(), self.current_user.as_ref()))
            .map(|(&pid, _)| pid)
            .collect();
        let requested_pid = self.attachment.as_ref().map(PidAttachment::requested_pid);
        let truncated = limit_process_ids(&mut pids, requested_pid);
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            true,
            ProcessRefreshKind::nothing()
                .without_tasks()
                .with_user(UpdateKind::Always)
                .with_cpu()
                .with_memory(),
        );
        // Revalidate after the resource refresh as the OS table can race. The
        // root was observed and prioritized before the cap, never inferred from
        // a possibly incomplete family vector.
        if let Some(attachment) = self.attachment.as_mut() {
            attachment.observe(root_observation(
                &self.system,
                self.current_user.as_ref(),
                attachment.requested_pid(),
            ));
        }
        let at = self.started.elapsed();

        let runtime_pids: Vec<Pid> = pids
            .iter()
            .copied()
            .filter(|pid| {
                self.system
                    .process(*pid)
                    .is_some_and(|p| is_interpreter(&p.name().to_string_lossy()))
            })
            .collect();
        let launchers = self.classify_launchers(&runtime_pids);

        let mut alive = HashSet::new();
        let mut processes = Vec::with_capacity(pids.len());
        for pid in pids {
            let Some(process) = self.system.process(pid) else {
                continue;
            };
            if !process.exists() || !belongs_to_user(process.user_id(), self.current_user.as_ref())
            {
                continue;
            }
            let key = ProcessKey {
                pid: pid.as_u32(),
                started: process.start_time(),
            };
            alive.insert(key);
            let raw_name = process.name().to_string_lossy();
            let agent = known_agent(&raw_name).or_else(|| launchers.get(&key).copied());
            processes.push(ProcessSnapshot {
                key,
                ppid: process.parent().map(|parent| parent.as_u32()),
                name: sanitize_name(&raw_name),
                cpu_percent: self.cpu.sample(key, at, process.accumulated_cpu_time()),
                rss_bytes: process.memory(),
                agent: agent.map(str::to_owned),
            });
        }
        self.cpu.retain(&alive);
        let process_count = processes.len();

        let now = Instant::now();
        let cpu_ready = self
            .last_system_cpu
            .is_some_and(|last| now.duration_since(last) >= sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        self.system.refresh_cpu_usage();
        if self.last_system_cpu.is_none() || cpu_ready {
            self.last_system_cpu = Some(now);
        }
        self.system.refresh_memory();
        let mut system = SystemSnapshot {
            cpu_percent: cpu_ready
                .then(|| self.system.global_cpu_usage().clamp(0.0, 100.0))
                .filter(|value| value.is_finite()),
            cpu_count: self.system.cpus().len(),
            memory_total: self.system.total_memory(),
            memory_available: self.system.available_memory(),
            swap_total: self.system.total_swap(),
            swap_used: self.system.used_swap(),
            ..SystemSnapshot::default()
        };
        let mut sessions = match self.attachment.as_ref() {
            Some(attachment) => attachment.group(processes),
            None => group_processes(processes, all_processes),
        };
        self.history.record(&mut sessions, &mut system);
        Snapshot {
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64(),
            monotonic: at.as_secs_f64(),
            sessions,
            system,
            process_count,
            inaccessible_count,
            all_processes: all_processes && self.attachment.is_none(),
            demo: false,
            truncated,
            attachment: self.attachment.as_ref().and_then(PidAttachment::snapshot),
        }
    }

    fn classify_launchers(&self, pids: &[Pid]) -> HashMap<ProcessKey, &'static str> {
        if pids.is_empty() {
            return HashMap::new();
        }
        // Raw OS command lines are scoped to one process at a time. Even with
        // many interpreter processes, argument buffers cannot accumulate here.
        // They are never retained in snapshots, labels, logs, histories or exports.
        // Only known launcher positions are inspected, never environment values;
        // sysinfo may transiently fetch the combined block internally on macOS.
        pids.iter()
            .filter_map(|&pid| {
                let mut temporary = System::new();
                temporary.refresh_processes_specifics(
                    ProcessesToUpdate::Some(&[pid]),
                    true,
                    ProcessRefreshKind::nothing()
                        .without_tasks()
                        .with_user(UpdateKind::Always)
                        .with_cmd(UpdateKind::Always),
                );
                let process = temporary.process(pid)?;
                let key = ProcessKey {
                    pid: pid.as_u32(),
                    started: process.start_time(),
                };
                let expected = self.system.process(pid)?;
                if expected.start_time() != key.started
                    || !belongs_to_user(process.user_id(), self.current_user.as_ref())
                {
                    return None;
                }
                identify_agent(&process.name().to_string_lossy(), process.cmd())
                    .map(|agent| (key, agent))
            })
            .collect()
    }
}

/// Repeatable animation data. It never mixes with live measurements.
#[derive(Default)]
pub struct DemoSampler {
    frame: u64,
    history: HistoryStore,
}

impl DemoSampler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sample(&mut self, all_processes: bool) -> Snapshot {
        let frame = self.frame;
        self.frame = self.frame.saturating_add(1);
        let t = frame as f64 / 5.0;
        let mut processes = Vec::new();
        let agents = ["claude", "codex", "aider", "opencode"];
        let names = [
            "node",
            "typescript",
            "cargo",
            "rustc",
            "git",
            "python",
            "pytest",
        ];
        for (index, &agent) in agents.iter().enumerate() {
            let root = 4100 + index as u32 * 100;
            let count = [7, 6, 4, 5][index];
            for child in 0..count {
                let wave = ((t + index as f64 * 1.7 + child as f64 * 0.6).sin() + 1.0) / 2.0;
                let base_memory = [270.0, 180.0, 120.0, 92.0][index];
                let memory = (base_memory + wave * 45.0 + child as f64 * 12.0) * 1024.0 * 1024.0;
                let cpu = [21.0, 14.0, 7.0, 4.0][index] * (0.18 + wave * 1.2);
                let ppid = if child == 0 {
                    Some(3000 + index as u32)
                } else if child < 3 {
                    Some(root)
                } else {
                    Some(root + 1 + ((child - 3) % 2) as u32)
                };
                processes.push(ProcessSnapshot {
                    key: ProcessKey {
                        pid: root + child as u32,
                        started: 1_780_000_000 + index as u64,
                    },
                    ppid,
                    name: if child == 0 {
                        agent.to_owned()
                    } else {
                        names[(index + child) % names.len()].to_owned()
                    },
                    cpu_percent: Some(cpu as f32),
                    rss_bytes: memory as u64,
                    agent: (child == 0).then(|| agent.to_owned()),
                });
            }
        }
        // Simulated build activity makes the changes view explorable without
        // observing or launching real work. The child identity changes each cycle.
        let phase = frame % 60;
        if (12..40).contains(&phase) {
            processes.push(ProcessSnapshot {
                key: ProcessKey {
                    pid: 4290,
                    started: 1_780_000_012 + (frame / 60) * 60,
                },
                ppid: Some(4200),
                name: "vite".into(),
                cpu_percent: Some(38.0),
                rss_bytes: (if phase < 20 { 192 } else { 320 }) * 1024 * 1024,
                agent: None,
            });
        }
        let process_count = processes.len();
        let mut sessions = group_processes(processes, all_processes);
        let total = 16 * 1024_u64.pow(3);
        let available = ((4.7 + t.sin() * 0.55) * 1024.0_f64.powi(3)) as u64;
        let mut system = SystemSnapshot {
            cpu_percent: Some((24.0 + (t * 0.7).sin() * 8.0) as f32),
            cpu_count: 10,
            memory_total: total,
            memory_available: available,
            swap_total: 4 * 1024_u64.pow(3),
            swap_used: 384 * 1024_u64.pow(2),
            ..SystemSnapshot::default()
        };
        self.history.record(&mut sessions, &mut system);
        Snapshot {
            timestamp: 1_780_000_000.0 + frame as f64,
            monotonic: frame as f64,
            sessions,
            system,
            process_count,
            inaccessible_count: 0,
            all_processes,
            demo: true,
            truncated: false,
            attachment: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn recognizes_agents_and_interpreter_entrypoints() {
        for agent in ["claude", "codex", "aider", "goose", "opencode"] {
            assert_eq!(identify_agent(agent, &[]), Some(agent));
        }
        assert_eq!(identify_agent("codex.exe", &[]), Some("codex"));
        assert_eq!(
            identify_agent(
                "node",
                &args(&["node", "/bin/codex.js", "--token", "private"])
            ),
            Some("codex")
        );
        assert_eq!(
            identify_agent("python3.14", &args(&["python", "-u", "-m", "aider.main"])),
            Some("aider")
        );
        assert_eq!(
            identify_agent(
                "node",
                &args(&["node", "/p/node_modules/@anthropic-ai/claude-code/cli.js"])
            ),
            Some("claude")
        );
    }

    #[test]
    fn never_classifies_prompts_or_arbitrary_command_arguments() {
        for (name, command) in [
            ("node", vec!["node", "ordinary.js", "codex"]),
            ("python", vec!["python", "-c", "codex"]),
            ("node", vec!["node", "--eval", "claude"]),
            ("bash", vec!["bash", "-c", "aider"]),
            ("my-codex-project", vec![]),
            ("node", vec!["node", "/somewhere/cli.js"]),
            ("python", vec!["python", "-m", "aider_fake"]),
            ("node", vec!["node", "--unknown", "codex"]),
        ] {
            assert_eq!(identify_agent(name, &args(&command)), None);
        }
    }

    #[test]
    fn cpu_priming_reuse_counter_reset_and_eviction() {
        let mut tracker = CpuDeltas::default();
        let key = ProcessKey {
            pid: 7,
            started: 10,
        };
        assert_eq!(tracker.sample(key, Duration::ZERO, 1000), None);
        assert_eq!(
            tracker.sample(key, Duration::from_secs(1), 2500),
            Some(150.0)
        );
        assert_eq!(tracker.sample(key, Duration::from_secs(2), 5), None);
        let reused = ProcessKey {
            pid: 7,
            started: 20,
        };
        assert_eq!(tracker.sample(reused, Duration::from_secs(3), 1000), None);
        tracker.retain(&HashSet::from([reused]));
        assert_eq!(tracker.previous.len(), 1);
        tracker.retain(&HashSet::new());
        assert!(tracker.previous.is_empty());
    }

    #[test]
    fn cpu_rejects_backwards_or_too_short_intervals() {
        let mut tracker = CpuDeltas::default();
        let key = ProcessKey { pid: 1, started: 1 };
        tracker.sample(key, Duration::from_secs(1), 100);
        assert_eq!(tracker.sample(key, Duration::from_millis(1050), 200), None);
        assert_eq!(tracker.sample(key, Duration::ZERO, 300), None);
    }

    #[test]
    fn unknown_ownership_never_defaults_to_the_current_user() {
        assert!(!belongs_to_user(None, None));
    }

    #[test]
    fn process_cap_prioritizes_explicit_root_without_changing_default_order() {
        let root = MAX_PROCESSES as u32 + 100;
        let all: Vec<_> = (1..=root).rev().map(Pid::from_u32).collect();
        let mut default = all.clone();
        assert!(limit_process_ids(&mut default, None));
        assert_eq!(default.len(), MAX_PROCESSES);
        assert_eq!(default.first().unwrap().as_u32(), 1);
        assert!(!default.contains(&Pid::from_u32(root)));
        let mut attached = all;
        assert!(limit_process_ids(&mut attached, Some(root)));
        assert_eq!(attached.len(), MAX_PROCESSES);
        assert_eq!(attached.first().unwrap().as_u32(), root);
        assert_eq!(attached.iter().collect::<HashSet<_>>().len(), MAX_PROCESSES);
        assert!(!limit_process_ids(&mut Vec::new(), Some(root)));
    }

    #[test]
    fn root_observation_does_not_disclose_unknown_owner_identity() {
        let sampler = LiveSampler::new();
        let pid = std::process::id();
        assert!(matches!(
            root_observation(&sampler.system, None, pid),
            RootObservation::Unavailable
        ));
        assert!(matches!(
            root_observation(&System::new(), sampler.current_user.as_ref(), pid),
            RootObservation::Missing
        ));
    }

    #[test]
    #[cfg(unix)]
    fn denied_and_different_owners_are_excluded() {
        let user: Uid = "1000".parse().unwrap();
        let other: Uid = "1001".parse().unwrap();
        assert!(!belongs_to_user(None, Some(&user)));
        assert!(!belongs_to_user(Some(&user), None));
        assert!(!belongs_to_user(Some(&other), Some(&user)));
        assert!(belongs_to_user(Some(&user), Some(&user)));
    }

    #[test]
    fn deterministic_demo_has_four_unique_dependency_families_and_bounded_history() {
        let mut a = DemoSampler::new();
        let mut b = DemoSampler::new();
        for _ in 0..80 {
            assert_eq!(a.sample(false), b.sample(false));
        }
        let snapshot = a.sample(false);
        assert!(snapshot.demo);
        assert_eq!(snapshot.sessions.len(), 4);
        assert!(snapshot.sessions.iter().all(|s| s.history.len() == 60));
        let processes: Vec<_> = snapshot
            .sessions
            .iter()
            .flat_map(|s| &s.processes)
            .collect();
        let pids: HashSet<_> = processes.iter().map(|p| p.key.pid).collect();
        assert_eq!(pids.len(), snapshot.process_count);
        assert!(
            processes
                .iter()
                .any(|p| p.ppid.is_some_and(|pid| pids.contains(&pid)))
        );
    }

    #[test]
    fn live_sampling_is_read_only_and_starts_with_unprimed_cpu() {
        let mut sampler = LiveSampler::new();
        let snapshot = sampler.sample(true);
        assert!(!snapshot.demo);
        assert!(snapshot.system.cpu_percent.is_none());
        assert!(
            snapshot
                .sessions
                .iter()
                .flat_map(|s| &s.processes)
                .all(|p| p.cpu_percent.is_none())
        );
        assert!(
            snapshot
                .sessions
                .iter()
                .flat_map(|s| &s.processes)
                .all(|p| !p.name.contains(['/', '\\', '\x1b']))
        );
        assert!(sampler.system.processes().values().all(|p| {
            p.cmd().is_empty() && p.environ().is_empty() && p.cwd().is_none() && p.exe().is_none()
        }));
    }

    #[test]
    fn live_attachment_locks_current_owned_process_and_ignores_all_flag() {
        use crate::model::AttachmentStatus;

        let pid = std::process::id();
        let mut sampler = LiveSampler::with_pid(pid);
        let snapshot = sampler.sample(true);
        let attachment = snapshot.attachment.unwrap();
        assert_eq!(attachment.requested_pid, pid);
        assert_eq!(attachment.status, AttachmentStatus::Attached);
        assert!(!snapshot.all_processes);
        assert!(!snapshot.demo);
        assert_eq!(snapshot.sessions.len(), 1);
        assert_eq!(snapshot.sessions[0].key.pid, pid);
        assert_eq!(Some(snapshot.sessions[0].key), attachment.root);
        assert_eq!(snapshot.sessions[0].processes[0].key.pid, pid);
        assert!(
            snapshot.sessions[0]
                .processes
                .iter()
                .all(|p| p.cpu_percent.is_none())
        );
        let next = sampler.sample(false);
        assert_eq!(next.attachment, snapshot.attachment);
        assert_eq!(next.sessions[0].history.len(), 2);
        assert!(sampler.system.processes().values().all(|p| {
            p.cmd().is_empty() && p.environ().is_empty() && p.cwd().is_none() && p.exe().is_none()
        }));
    }
}
