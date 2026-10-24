//! A one-shot identity lock and one observed process family, not process control.
//!
//! Inputs must already pass the live sampler's current-user ownership filter.
//! Descendants require an observed parent chain in this sample; reparented or
//! inaccessible descendants cannot be inferred. Nested agents remain one family.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::model::{
    AttachmentSnapshot, AttachmentStatus, ProcessKey, ProcessSnapshot, SessionSnapshot,
    sanitize_name,
};

#[derive(Clone, Copy, Debug)]
pub(crate) enum RootObservation {
    /// Verified existing process owned by the current user, before any sample cap.
    Present(ProcessKey),
    /// No existing entry in the backend's complete metadata pass. Not an exit cause.
    Missing,
    /// Owner/access is unknown or not the current user. No identity is disclosed.
    Unavailable,
}

pub(crate) struct PidAttachment {
    requested_pid: u32,
    root: Option<ProcessKey>,
    status: Option<AttachmentStatus>,
}

impl PidAttachment {
    pub(crate) fn new(requested_pid: u32) -> Self {
        Self {
            requested_pid,
            root: None,
            status: None,
        }
    }

    pub(crate) fn requested_pid(&self) -> u32 {
        self.requested_pid
    }

    /// Observe the root separately from the capped descendant sample. A missing
    /// sampled child (or a truncated sample) must never terminate this identity.
    pub(crate) fn observe(&mut self, observation: RootObservation) {
        if self
            .status
            .is_some_and(|status| status != AttachmentStatus::Attached)
        {
            return;
        }
        self.status = Some(match observation {
            RootObservation::Present(key) if key.pid != self.requested_pid => {
                AttachmentStatus::Unavailable
            }
            RootObservation::Present(key) => match self.root {
                Some(root) if root != key => AttachmentStatus::IdentityChanged,
                _ => {
                    self.root = Some(key);
                    AttachmentStatus::Attached
                }
            },
            RootObservation::Missing if self.root.is_some() => AttachmentStatus::Ended,
            RootObservation::Missing => AttachmentStatus::Missing,
            RootObservation::Unavailable => AttachmentStatus::Unavailable,
        });
    }

    pub(crate) fn snapshot(&self) -> Option<AttachmentSnapshot> {
        Some(AttachmentSnapshot {
            requested_pid: self.requested_pid,
            root: self.root,
            status: self.status?,
        })
    }

    pub(crate) fn group(&self, processes: Vec<ProcessSnapshot>) -> Vec<SessionSnapshot> {
        if self.status != Some(AttachmentStatus::Attached) {
            return Vec::new();
        }
        let Some(root) = self.root else {
            return Vec::new();
        };
        attached_family(processes, root).into_iter().collect()
    }
}

/// One bounded, nonrecursive walk. An adjacency index avoids walking a deep
/// ancestor chain again for each child. Duplicate PIDs choose the newest identity.
fn attached_family(processes: Vec<ProcessSnapshot>, root: ProcessKey) -> Option<SessionSnapshot> {
    let mut nodes = BTreeMap::<u32, ProcessSnapshot>::new();
    for mut process in processes {
        process.name = sanitize_name(&process.name);
        if let Some(agent) = process.agent.as_mut() {
            *agent = sanitize_name(agent);
        }
        if nodes
            .get(&process.key.pid)
            .is_none_or(|old| old.key.started < process.key.started)
        {
            nodes.insert(process.key.pid, process);
        }
    }
    let root_process = nodes.get(&root.pid).filter(|process| process.key == root)?;
    let label = root_process
        .agent
        .clone()
        .unwrap_or_else(|| root_process.name.clone());
    let agent = root_process.agent.clone();
    let mut children = HashMap::<u32, Vec<u32>>::new();
    for process in nodes.values() {
        // Root ancestry is outside the explicit attachment boundary.
        if process.key == root {
            continue;
        }
        if let Some(parent) = process.ppid.and_then(|pid| nodes.get(&pid))
            && parent.key.pid != process.key.pid
            && parent.key.started <= process.key.started
        {
            children
                .entry(parent.key.pid)
                .or_default()
                .push(process.key.pid);
        }
    }
    let mut seen = HashSet::new();
    let mut stack = vec![root.pid];
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        if let Some(children) = children.get(&pid) {
            stack.extend(children.iter().copied());
        }
    }
    let mut processes: Vec<_> = nodes
        .into_values()
        .filter(|process| seen.contains(&process.key.pid))
        .collect();
    processes.sort_unstable_by_key(|process| (process.key != root, process.key.pid));
    Some(SessionSnapshot {
        key: root,
        label,
        agent,
        processes,
        ..SessionSnapshot::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(pid: u32, started: u64) -> ProcessKey {
        ProcessKey { pid, started }
    }

    fn process(pid: u32, ppid: Option<u32>, started: u64, name: &str) -> ProcessSnapshot {
        ProcessSnapshot {
            key: key(pid, started),
            ppid,
            name: name.to_owned(),
            cpu_percent: Some(10.0),
            rss_bytes: 100,
            ..ProcessSnapshot::default()
        }
    }

    fn attached(pid: u32, started: u64) -> PidAttachment {
        let mut attachment = PidAttachment::new(pid);
        attachment.observe(RootObservation::Present(key(pid, started)));
        attachment
    }

    fn pids(sessions: &[SessionSnapshot]) -> Vec<u32> {
        sessions
            .iter()
            .flat_map(|s| s.processes.iter().map(|p| p.key.pid))
            .collect()
    }

    #[test]
    fn arbitrary_build_and_nested_agents_form_one_family_without_doublecounting() {
        let attachment = attached(50, 10);
        let mut nested = process(52, Some(51), 12, "codex");
        nested.agent = Some("codex".into());
        let mut nested_again = process(53, Some(52), 13, "claude");
        nested_again.agent = Some("claude".into());
        let nodes = vec![
            process(54, Some(53), 14, "rustc"),
            nested,
            process(50, Some(1), 10, "custom-dev-server"),
            process(51, Some(50), 11, "build-worker"),
            nested_again,
            process(99, Some(1), 10, "unrelated"),
            process(51, Some(50), 11, "duplicate"),
        ];
        let sessions = attachment.group(nodes);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].label, "custom-dev-server");
        assert_eq!(pids(&sessions), [50, 51, 52, 53, 54]);
        assert_eq!(sessions[0].rss_bytes(), 500);
        assert_eq!(sessions[0].cpu_percent(), Some(50.0));
    }

    #[test]
    fn root_identity_never_follows_a_reused_pid_or_resurrects() {
        let mut attachment = attached(50, 10);
        attachment.observe(RootObservation::Present(key(50, 11)));
        assert_eq!(
            attachment.snapshot().unwrap().status,
            AttachmentStatus::IdentityChanged
        );
        assert_eq!(attachment.snapshot().unwrap().root, Some(key(50, 10)));
        assert!(
            attachment
                .group(vec![process(50, None, 11, "other")])
                .is_empty()
        );
        attachment.observe(RootObservation::Present(key(50, 10)));
        assert_eq!(
            attachment.snapshot().unwrap().status,
            AttachmentStatus::IdentityChanged
        );
    }

    #[test]
    fn disappearance_is_terminal_even_when_same_identity_returns() {
        let mut attachment = attached(50, 10);
        attachment.observe(RootObservation::Missing);
        assert_eq!(
            attachment.snapshot().unwrap().status,
            AttachmentStatus::Ended
        );
        attachment.observe(RootObservation::Present(key(50, 10)));
        assert_eq!(
            attachment.snapshot().unwrap().status,
            AttachmentStatus::Ended
        );
        assert!(
            attachment
                .group(vec![process(50, None, 10, "build")])
                .is_empty()
        );
    }

    #[test]
    fn missing_foreign_and_unknown_roots_do_not_attach_later() {
        for (observation, expected) in [
            (RootObservation::Missing, AttachmentStatus::Missing),
            (RootObservation::Unavailable, AttachmentStatus::Unavailable),
            (
                RootObservation::Present(key(99, 1)),
                AttachmentStatus::Unavailable,
            ),
        ] {
            let mut attachment = PidAttachment::new(50);
            assert!(attachment.snapshot().is_none());
            assert!(attachment.group(Vec::new()).is_empty());
            attachment.observe(observation);
            attachment.observe(RootObservation::Present(key(50, 10)));
            assert_eq!(attachment.snapshot().unwrap().status, expected);
            assert_eq!(attachment.snapshot().unwrap().root, None);
            assert!(
                attachment
                    .group(vec![process(50, None, 10, "build")])
                    .is_empty()
            );
        }
        let mut attachment = attached(50, 10);
        attachment.observe(RootObservation::Unavailable);
        assert_eq!(
            attachment.snapshot().unwrap().status,
            AttachmentStatus::Unavailable
        );
        assert_eq!(attachment.snapshot().unwrap().root, Some(key(50, 10)));
        attachment.observe(RootObservation::Present(key(50, 10)));
        assert_eq!(
            attachment.snapshot().unwrap().status,
            AttachmentStatus::Unavailable
        );
    }

    #[test]
    fn newer_parent_and_duplicate_pid_identity_do_not_adopt_old_children() {
        let attachment = attached(50, 10);
        let sessions = attachment.group(vec![
            process(50, None, 10, "build"),
            process(51, Some(50), 11, "old-parent"),
            process(51, Some(50), 20, "new-parent"),
            process(52, Some(51), 12, "old-child"),
            process(53, Some(52), 13, "old-grandchild"),
            process(54, Some(51), 21, "new-child"),
        ]);
        assert_eq!(pids(&sessions), [50, 51, 54]);
        assert_eq!(sessions[0].processes[1].name, "new-parent");
    }

    #[test]
    fn cycles_orphans_self_parents_and_reparented_children_are_safe() {
        let attachment = attached(50, 10);
        let sessions = attachment.group(vec![
            process(50, Some(51), 10, "build"),
            process(51, Some(50), 10, "child"),
            process(52, Some(51), 10, "grandchild"),
            process(60, Some(61), 10, "cycle"),
            process(61, Some(60), 10, "cycle"),
            process(70, Some(999), 10, "orphan"),
            process(80, Some(80), 10, "self"),
        ]);
        assert_eq!(pids(&sessions), [50, 51, 52]);
        let sessions = attachment.group(vec![
            process(50, None, 10, "build"),
            process(51, Some(1), 10, "reparented"),
            process(52, Some(51), 10, "grandchild"),
        ]);
        assert_eq!(pids(&sessions), [50]);
    }

    #[test]
    fn incomplete_family_does_not_imply_root_disappearance() {
        let attachment = attached(50, 10);
        assert!(attachment.group(Vec::new()).is_empty());
        assert_eq!(
            attachment.snapshot().unwrap().status,
            AttachmentStatus::Attached
        );
        let sessions = attachment.group(vec![process(50, None, 10, "build")]);
        assert_eq!(pids(&sessions), [50]);
        assert_eq!(
            attachment.snapshot().unwrap().status,
            AttachmentStatus::Attached
        );
        // A malformed/racing duplicate root cannot cause an implicit identity swap.
        assert!(
            attachment
                .group(vec![process(50, None, 11, "new-root")])
                .is_empty()
        );
        assert_eq!(attachment.snapshot().unwrap().root, Some(key(50, 10)));
    }

    #[test]
    fn names_and_labels_are_terminal_safe() {
        let attachment = attached(50, 10);
        let sessions = attachment.group(vec![process(50, None, 10, "/tmp/\x1b[31mname\u{202e}")]);
        assert_eq!(sessions[0].label, "31mname");
        assert_eq!(sessions[0].processes[0].name, "31mname");
    }

    #[test]
    fn sixteen_thousand_deep_family_never_recurses_or_double_counts() {
        let attachment = attached(1, 10);
        let nodes = (1..=16_384)
            .rev()
            .map(|pid| process(pid, (pid > 1).then_some(pid - 1), 10, "worker"))
            .collect();
        let sessions = attachment.group(nodes);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].process_count(), 16_384);
        assert_eq!(sessions[0].rss_bytes(), 1_638_400);
        assert_eq!(
            pids(&sessions).into_iter().collect::<HashSet<_>>().len(),
            16_384
        );
    }
}
