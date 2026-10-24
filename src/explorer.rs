//! Presentation-neutral process-tree navigation. All ancestry walks are iterative.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::model::{ProcessKey, SessionSnapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub key: ProcessKey,
    /// The real ancestry depth. The renderer can cap indentation independently.
    pub depth: usize,
    pub has_children: bool,
    /// False for leaves, which have nothing to expand.
    pub expanded: bool,
}

/// New branches are expanded by default. Identity includes process start time,
/// so a reused PID does not inherit an old process's selection or folded state.
#[derive(Clone, Debug, Default)]
pub struct Explorer {
    session: Option<ProcessKey>,
    selected: Option<ProcessKey>,
    collapsed: HashSet<ProcessKey>,
    previous_parents: HashMap<ProcessKey, Option<ProcessKey>>,
}

impl Explorer {
    pub fn selected(&self) -> Option<ProcessKey> {
        self.selected
    }

    /// Preserve selection across refreshes, falling back to a visible ancestor
    /// (including the last known ancestors of an exited process), then a root.
    pub fn reconcile(&mut self, session: &SessionSnapshot) {
        self.reconcile_with(session.key, &Forest::new(session));
    }

    pub fn visible_rows(&self, session: &SessionSnapshot) -> Vec<Row> {
        let forest = Forest::new(session);
        // A caller can render a newly selected session before reconciling it.
        if self.session.is_some_and(|key| key != session.key) {
            forest.visible_rows(&HashSet::new())
        } else {
            forest.visible_rows(&self.collapsed)
        }
    }

    /// Select a current process. Hidden targets resolve to their nearest visible
    /// ancestor; unknown targets do not disturb the current selection.
    pub fn select_key(&mut self, session: &SessionSnapshot, key: ProcessKey) {
        let forest = Forest::new(session);
        self.reconcile_with(session.key, &forest);
        if forest.by_key.contains_key(&key) {
            self.selected = Some(key);
            self.reconcile_with(session.key, &forest);
        }
    }

    /// Select an exact process from a flat view, opening only its ancestors.
    /// Unrelated folded branches and the target's own folded state are preserved.
    pub fn reveal_key(&mut self, session: &SessionSnapshot, key: ProcessKey) {
        let forest = Forest::new(session);
        self.reconcile_with(session.key, &forest);
        let Some(&index) = forest.by_key.get(&key) else {
            return;
        };
        let mut ancestor = forest.parents[index];
        while let Some(index) = ancestor {
            self.collapsed.remove(&forest.keys[index]);
            ancestor = forest.parents[index];
        }
        self.selected = Some(key);
        self.reconcile_with(session.key, &forest);
    }

    pub fn move_selection(&mut self, session: &SessionSnapshot, delta: i32) {
        let forest = Forest::new(session);
        self.reconcile_with(session.key, &forest);
        let rows = forest.visible_rows(&self.collapsed);
        if rows.is_empty() {
            return;
        }
        let index = rows
            .iter()
            .position(|row| Some(row.key) == self.selected)
            .unwrap_or(0);
        let next = if delta < 0 {
            index.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            index.saturating_add(delta as usize).min(rows.len() - 1)
        };
        self.selected = Some(rows[next].key);
    }

    /// Left-arrow behavior: fold an expanded branch, otherwise select its parent.
    pub fn collapse_selected(&mut self, session: &SessionSnapshot) {
        let forest = Forest::new(session);
        self.reconcile_with(session.key, &forest);
        let Some(index) = self
            .selected
            .and_then(|key| forest.by_key.get(&key).copied())
        else {
            return;
        };
        let key = forest.keys[index];
        if !forest.children[index].is_empty() && !self.collapsed.contains(&key) {
            self.collapsed.insert(key);
        } else if let Some(parent) = forest.parents[index] {
            self.selected = Some(forest.keys[parent]);
        }
        self.reconcile_with(session.key, &forest);
    }

    /// Right-arrow behavior: unfold a branch, or enter its first visible child.
    pub fn expand_selected(&mut self, session: &SessionSnapshot) {
        let forest = Forest::new(session);
        self.reconcile_with(session.key, &forest);
        let Some(index) = self
            .selected
            .and_then(|key| forest.by_key.get(&key).copied())
        else {
            return;
        };
        if let Some(&child) = forest.children[index].first()
            && !self.collapsed.remove(&forest.keys[index])
        {
            self.selected = Some(forest.keys[child]);
        }
    }

    pub fn toggle_selected(&mut self, session: &SessionSnapshot) {
        let forest = Forest::new(session);
        self.reconcile_with(session.key, &forest);
        let Some(index) = self
            .selected
            .and_then(|key| forest.by_key.get(&key).copied())
        else {
            return;
        };
        if !forest.children[index].is_empty() {
            let key = forest.keys[index];
            if !self.collapsed.remove(&key) {
                self.collapsed.insert(key);
            }
            self.reconcile_with(session.key, &forest);
        }
    }

    pub fn expand_all(&mut self, session: &SessionSnapshot) {
        let forest = Forest::new(session);
        self.reconcile_with(session.key, &forest);
        self.collapsed.clear();
    }

    pub fn collapse_all(&mut self, session: &SessionSnapshot) {
        let forest = Forest::new(session);
        self.reconcile_with(session.key, &forest);
        self.collapsed = forest
            .keys
            .iter()
            .enumerate()
            .filter(|(index, _)| !forest.children[*index].is_empty())
            .map(|(_, &key)| key)
            .collect();
        self.reconcile_with(session.key, &forest);
    }

    fn reconcile_with(&mut self, session: ProcessKey, forest: &Forest) {
        if self.session != Some(session) {
            self.session = Some(session);
            self.selected = None;
            self.collapsed.clear();
            self.previous_parents.clear();
        }
        self.collapsed.retain(|key| forest.by_key.contains_key(key));
        let rows = forest.visible_rows(&self.collapsed);
        let visible: HashSet<_> = rows.iter().map(|row| row.key).collect();
        let mut selected = self
            .selected
            .and_then(|key| forest.visible_ancestor(key, &visible));
        if selected.is_none() {
            // The old forest is acyclic, so even an exited chain can be walked
            // without a recursive call or a per-ancestor full-tree scan.
            let mut cursor = self.selected;
            while let Some(key) = cursor {
                cursor = self.previous_parents.get(&key).copied().flatten();
                if let Some(ancestor) = cursor {
                    selected = forest.visible_ancestor(ancestor, &visible);
                    if selected.is_some() {
                        break;
                    }
                }
            }
        }
        self.selected = selected.or_else(|| rows.first().map(|row| row.key));
        self.previous_parents = forest
            .keys
            .iter()
            .enumerate()
            .map(|(index, &key)| (key, forest.parents[index].map(|parent| forest.keys[parent])))
            .collect();
    }
}

/// A deterministic forest with one entry per live PID. Constructed in O(n log n),
/// with linear cycle repair and traversal, including arbitrarily deep chains.
struct Forest {
    keys: Vec<ProcessKey>,
    by_key: HashMap<ProcessKey, usize>,
    parents: Vec<Option<usize>>,
    children: Vec<Vec<usize>>,
    roots: Vec<usize>,
}

impl Forest {
    fn new(session: &SessionSnapshot) -> Self {
        let mut processes = BTreeMap::new();
        for process in &session.processes {
            let existing = processes.entry(process.key.pid).or_insert(process);
            if process.key.started > existing.key.started {
                *existing = process;
            }
        }
        let keys: Vec<_> = processes.values().map(|process| process.key).collect();
        let by_pid: HashMap<_, _> = keys
            .iter()
            .enumerate()
            .map(|(i, key)| (key.pid, i))
            .collect();
        let by_key: HashMap<_, _> = keys.iter().enumerate().map(|(i, &key)| (key, i)).collect();
        let mut parents: Vec<_> = processes
            .values()
            .enumerate()
            .map(|(index, process)| {
                if process.key == session.key {
                    return None; // The session boundary is always a root.
                }
                process
                    .ppid
                    .and_then(|pid| by_pid.get(&pid).copied())
                    .filter(|&parent| {
                        parent != index && keys[parent].started <= process.key.started
                    })
            })
            .collect();

        let mut state = vec![0_u8; keys.len()];
        for start in 0..keys.len() {
            if state[start] != 0 {
                continue;
            }
            let mut path = Vec::new();
            let mut cursor = Some(start);
            while let Some(index) = cursor {
                if state[index] == 2 {
                    break;
                }
                if state[index] == 1 {
                    // Cut the lowest-PID member of this cycle. Every other
                    // parent edge is preserved and each process remains visible.
                    let mut cut = index;
                    let mut member = parents[index];
                    while let Some(next) = member {
                        if next == index {
                            break;
                        }
                        cut = cut.min(next);
                        member = parents[next];
                    }
                    parents[cut] = None;
                    break;
                }
                state[index] = 1;
                path.push(index);
                cursor = parents[index];
            }
            for index in path {
                state[index] = 2;
            }
        }

        let mut children = vec![Vec::new(); keys.len()];
        let mut roots = Vec::new();
        for (index, parent) in parents.iter().enumerate() {
            if let Some(parent) = parent {
                children[*parent].push(index);
            } else {
                roots.push(index);
            }
        }
        if let Some(&root) = by_key.get(&session.key) {
            roots.retain(|&index| index != root);
            roots.insert(0, root);
        }
        Self {
            keys,
            by_key,
            parents,
            children,
            roots,
        }
    }

    fn visible_rows(&self, collapsed: &HashSet<ProcessKey>) -> Vec<Row> {
        let mut rows = Vec::with_capacity(self.keys.len());
        let mut stack: Vec<_> = self.roots.iter().rev().map(|&index| (index, 0)).collect();
        while let Some((index, depth)) = stack.pop() {
            let key = self.keys[index];
            let has_children = !self.children[index].is_empty();
            let expanded = has_children && !collapsed.contains(&key);
            rows.push(Row {
                key,
                depth,
                has_children,
                expanded,
            });
            if expanded {
                stack.extend(
                    self.children[index]
                        .iter()
                        .rev()
                        .map(|&child| (child, depth + 1)),
                );
            }
        }
        rows
    }

    fn visible_ancestor(
        &self,
        key: ProcessKey,
        visible: &HashSet<ProcessKey>,
    ) -> Option<ProcessKey> {
        let mut cursor = self.by_key.get(&key).copied();
        while let Some(index) = cursor {
            let key = self.keys[index];
            if visible.contains(&key) {
                return Some(key);
            }
            cursor = self.parents[index];
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProcessSnapshot;

    fn key(pid: u32) -> ProcessKey {
        ProcessKey { pid, started: 10 }
    }

    fn process(pid: u32, ppid: Option<u32>) -> ProcessSnapshot {
        ProcessSnapshot {
            key: key(pid),
            ppid,
            ..ProcessSnapshot::default()
        }
    }

    fn session() -> SessionSnapshot {
        SessionSnapshot {
            key: key(10),
            // Deliberately unsorted: ancestry is not the input list order.
            processes: vec![
                process(31, Some(30)),
                process(30, Some(10)),
                process(10, None),
                process(20, Some(10)),
                process(21, Some(20)),
            ],
            ..SessionSnapshot::default()
        }
    }

    fn pids(explorer: &Explorer, session: &SessionSnapshot) -> Vec<u32> {
        explorer
            .visible_rows(session)
            .iter()
            .map(|row| row.key.pid)
            .collect()
    }

    #[test]
    fn true_ancestry_has_stable_pid_siblings() {
        let mut session = session();
        let mut explorer = Explorer::default();
        explorer.reconcile(&session);
        assert_eq!(explorer.selected(), Some(key(10)));
        assert_eq!(pids(&explorer, &session), [10, 20, 21, 30, 31]);
        assert_eq!(
            explorer
                .visible_rows(&session)
                .iter()
                .map(|row| row.depth)
                .collect::<Vec<_>>(),
            [0, 1, 2, 1, 2]
        );
        session.processes.reverse();
        assert_eq!(pids(&explorer, &session), [10, 20, 21, 30, 31]);
    }

    #[test]
    fn left_right_and_toggle_follow_tree_navigation() {
        let session = session();
        let mut explorer = Explorer::default();
        explorer.expand_selected(&session);
        assert_eq!(explorer.selected(), Some(key(20)));
        explorer.collapse_selected(&session);
        assert_eq!(pids(&explorer, &session), [10, 20, 30, 31]);
        assert_eq!(explorer.selected(), Some(key(20)));
        explorer.expand_selected(&session);
        assert_eq!(explorer.selected(), Some(key(20)));
        explorer.expand_selected(&session);
        assert_eq!(explorer.selected(), Some(key(21)));
        explorer.expand_selected(&session);
        explorer.toggle_selected(&session);
        assert_eq!(explorer.selected(), Some(key(21)));
        explorer.collapse_selected(&session);
        assert_eq!(explorer.selected(), Some(key(20)));
        explorer.toggle_selected(&session);
        explorer.collapse_selected(&session);
        assert_eq!(explorer.selected(), Some(key(10)));
        explorer.collapse_selected(&session);
        explorer.collapse_selected(&session);
        assert_eq!(pids(&explorer, &session), [10]);
        assert_eq!(explorer.selected(), Some(key(10)));
    }

    #[test]
    fn reveal_key_opens_only_ancestors_and_selects_exact_identity() {
        let session = session();
        let mut explorer = Explorer::default();
        explorer.collapse_all(&session);
        explorer.reveal_key(&session, key(21));
        assert_eq!(explorer.selected(), Some(key(21)));
        assert_eq!(pids(&explorer, &session), [10, 20, 21, 30]);
        assert!(
            !explorer
                .visible_rows(&session)
                .iter()
                .find(|row| row.key == key(30))
                .unwrap()
                .expanded
        );
        explorer.collapse_all(&session);
        explorer.reveal_key(&session, key(20));
        assert_eq!(explorer.selected(), Some(key(20)));
        assert_eq!(pids(&explorer, &session), [10, 20, 30]);
        explorer.reveal_key(&session, key(999));
        assert_eq!(explorer.selected(), Some(key(20)));
        assert_eq!(pids(&explorer, &session), [10, 20, 30]);
    }

    #[test]
    fn selection_is_clamped_without_signed_overflow() {
        let session = session();
        let mut explorer = Explorer::default();
        explorer.move_selection(&session, i32::MAX);
        assert_eq!(explorer.selected(), Some(key(31)));
        explorer.move_selection(&session, i32::MIN);
        assert_eq!(explorer.selected(), Some(key(10)));
        explorer.move_selection(&session, -1);
        assert_eq!(explorer.selected(), Some(key(10)));
    }

    #[test]
    fn refresh_preserves_identity_then_last_known_ancestor() {
        let mut session = session();
        let mut explorer = Explorer::default();
        explorer.select_key(&session, key(21));
        session.processes.reverse();
        explorer.reconcile(&session);
        assert_eq!(explorer.selected(), Some(key(21)));
        session.processes.retain(|process| process.key.pid != 21);
        explorer.reconcile(&session);
        assert_eq!(explorer.selected(), Some(key(20)));
        session.processes.retain(|process| process.key.pid != 20);
        explorer.reconcile(&session);
        assert_eq!(explorer.selected(), Some(key(10)));
    }

    #[test]
    fn collapsed_state_survives_refresh_and_new_branches_are_expanded() {
        let mut session = session();
        let mut explorer = Explorer::default();
        explorer.select_key(&session, key(20));
        explorer.collapse_selected(&session);
        session
            .processes
            .extend([process(40, Some(10)), process(41, Some(40))]);
        explorer.reconcile(&session);
        assert_eq!(pids(&explorer, &session), [10, 20, 30, 31, 40, 41]);
        explorer.select_key(&session, key(21));
        assert_eq!(explorer.selected(), Some(key(20)));
        explorer.select_key(&session, key(999));
        assert_eq!(explorer.selected(), Some(key(20)));
    }

    #[test]
    fn collapsing_all_moves_hidden_selection_to_root() {
        let session = session();
        let mut explorer = Explorer::default();
        explorer.select_key(&session, key(21));
        explorer.collapse_all(&session);
        assert_eq!(pids(&explorer, &session), [10]);
        assert_eq!(explorer.selected(), Some(key(10)));
        explorer.expand_all(&session);
        assert_eq!(pids(&explorer, &session), [10, 20, 21, 30, 31]);
    }

    #[test]
    fn reparenting_under_folded_branch_reconciles_selection() {
        let mut session = session();
        let mut explorer = Explorer::default();
        explorer.select_key(&session, key(20));
        explorer.collapse_selected(&session);
        explorer.select_key(&session, key(31));
        session
            .processes
            .iter_mut()
            .find(|process| process.key.pid == 31)
            .unwrap()
            .ppid = Some(20);
        explorer.reconcile(&session);
        assert_eq!(explorer.selected(), Some(key(20)));
    }

    #[test]
    fn reused_pid_does_not_inherit_selection_or_folded_state() {
        let mut session = session();
        let mut explorer = Explorer::default();
        explorer.select_key(&session, key(20));
        explorer.collapse_selected(&session);
        for process in &mut session.processes {
            if process.key.pid == 20 || process.key.pid == 21 {
                process.key.started = 20;
            }
        }
        explorer.reconcile(&session);
        assert_eq!(explorer.selected(), Some(key(10)));
        assert_eq!(pids(&explorer, &session), [10, 20, 21, 30, 31]);
    }

    #[test]
    fn broken_ancestry_and_duplicates_are_visible_once() {
        let session = SessionSnapshot {
            key: key(10),
            processes: vec![
                process(10, Some(20)),
                process(20, Some(10)),
                process(40, Some(50)),
                process(50, Some(40)),
                process(60, Some(50)),
                process(70, Some(999)),
                process(80, Some(80)),
                process(50, Some(40)),
            ],
            ..SessionSnapshot::default()
        };
        let mut explorer = Explorer::default();
        explorer.reconcile(&session);
        assert_eq!(pids(&explorer, &session), [10, 20, 40, 50, 60, 70, 80]);
        assert_eq!(
            explorer
                .visible_rows(&session)
                .iter()
                .map(|row| row.depth)
                .collect::<Vec<_>>(),
            [0, 1, 0, 1, 2, 0, 0]
        );
        explorer.select_key(&session, key(60));
        explorer.collapse_all(&session);
        assert_eq!(explorer.selected(), Some(key(40)));
    }

    #[test]
    fn newer_parent_pid_is_not_treated_as_real_ancestry() {
        let mut session = session();
        session
            .processes
            .iter_mut()
            .find(|process| process.key.pid == 20)
            .unwrap()
            .key
            .started = 11;
        let explorer = Explorer::default();
        let rows = explorer.visible_rows(&session);
        assert_eq!(rows.iter().find(|row| row.key.pid == 21).unwrap().depth, 0);
    }

    #[test]
    fn switching_session_resets_tree_and_empty_snapshots_are_safe() {
        let mut session = session();
        let mut explorer = Explorer::default();
        explorer.collapse_all(&session);
        session.key = key(20);
        assert!(explorer.visible_rows(&session).len() > 1);
        explorer.reconcile(&session);
        assert_eq!(explorer.selected(), Some(key(20)));
        session.processes.clear();
        explorer.move_selection(&session, i32::MIN);
        explorer.collapse_selected(&session);
        explorer.expand_selected(&session);
        explorer.toggle_selected(&session);
        explorer.expand_all(&session);
        explorer.collapse_all(&session);
        assert_eq!(explorer.selected(), None);
        assert!(explorer.visible_rows(&session).is_empty());
    }

    #[test]
    fn sixteen_thousand_deep_nodes_do_not_recurse_or_duplicate() {
        let session = SessionSnapshot {
            key: key(1),
            processes: (1..=16_000)
                .map(|pid| process(pid, (pid > 1).then_some(pid - 1)))
                .collect(),
            ..SessionSnapshot::default()
        };
        let mut explorer = Explorer::default();
        explorer.reconcile(&session);
        let rows = explorer.visible_rows(&session);
        assert_eq!(rows.len(), 16_000);
        assert_eq!(rows.last().unwrap().depth, 15_999);
        assert_eq!(
            rows.iter().map(|row| row.key).collect::<HashSet<_>>().len(),
            16_000
        );
        explorer.move_selection(&session, i32::MAX);
        assert_eq!(explorer.selected(), Some(key(16_000)));
        explorer.collapse_all(&session);
        assert_eq!(explorer.selected(), Some(key(1)));
        assert_eq!(pids(&explorer, &session), [1]);
        explorer.reveal_key(&session, key(16_000));
        assert_eq!(explorer.selected(), Some(key(16_000)));
        assert_eq!(explorer.visible_rows(&session).len(), 16_000);
        explorer.expand_all(&session);
        assert_eq!(explorer.visible_rows(&session).len(), 16_000);
    }
}
