//! The dependency graph: issues as nodes and directed `blocks` edges
//! (blocker → blocked). Pure data + graph algorithms, no I/O and no rendering,
//! so it can be unit-tested in isolation.

use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Triage,
    Backlog,
    Unstarted,
    Started,
    Completed,
    Canceled,
    Duplicate,
    Unknown,
}

impl Status {
    /// Parse Linear's `WorkflowState.type` string. Matched defensively: the
    /// field is a plain `String!`, so unrecognised values become `Unknown`
    /// rather than breaking the build of the graph.
    pub fn from_type(kind: &str) -> Self {
        match kind {
            "triage" => Status::Triage,
            "backlog" => Status::Backlog,
            "unstarted" => Status::Unstarted,
            "started" => Status::Started,
            "completed" => Status::Completed,
            "canceled" => Status::Canceled,
            "duplicate" => Status::Duplicate,
            _ => Status::Unknown,
        }
    }

    /// Work that is finished or abandoned — it can never block anything.
    pub const fn is_resolved(self) -> bool {
        matches!(
            self,
            Status::Completed | Status::Canceled | Status::Duplicate
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    None,
    Urgent,
    High,
    Medium,
    Low,
}

impl Priority {
    /// Linear sends `priority` as a `Float!`: 0 None, 1 Urgent … 4 Low.
    pub fn from_value(v: f64) -> Self {
        match v.round() as i64 {
            1 => Priority::Urgent,
            2 => Priority::High,
            3 => Priority::Medium,
            4 => Priority::Low,
            _ => Priority::None,
        }
    }

    /// Sort rank with the most pressing first (Urgent → … → None).
    pub const fn rank(self) -> u8 {
        match self {
            Priority::Urgent => 0,
            Priority::High => 1,
            Priority::Medium => 2,
            Priority::Low => 3,
            Priority::None => 4,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Issue {
    pub key: String, // human identifier, e.g. "ENG-123"
    pub title: String,
    pub status: Status,
    pub priority: Priority,
    pub assignee: Option<String>,
    /// True when the issue lives outside the queried project (a cross-project
    /// blocker/blocked endpoint we only know about through a relation).
    pub external: bool,
}

impl Issue {
    /// The team prefix of the identifier ("ENG" from "ENG-123").
    pub fn team(&self) -> &str {
        self.key.split('-').next().unwrap_or(&self.key)
    }
}

/// Which way to walk the graph from a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Toward blockers — issues that must finish first.
    Upstream,
    /// Toward blocked work — issues this one is holding up.
    Downstream,
}

pub struct Graph {
    pub project: String,
    nodes: HashMap<String, Issue>,
    /// Stable display order of every node key (insertion order).
    order: Vec<String>,
    /// blocker → issues it blocks (downstream neighbours).
    blocks: HashMap<String, Vec<String>>,
    /// blocked → issues that block it (upstream neighbours).
    blocked_by: HashMap<String, Vec<String>>,
    edges: HashSet<(String, String)>, // (blocker, blocked), de-duplicated
    cycles: Vec<Vec<String>>,
    in_cycle: HashSet<String>,
    back_edges: HashSet<(String, String)>,
    /// Per-node transitive closure sizes `(upstream, downstream)`, precomputed in
    /// [`finalize`](Self::finalize). The detail bar calls [`transitive`](Self::transitive)
    /// for the focused issue on every repaint; without this each call re-runs a BFS
    /// that allocates a set + work-stack and clones every visited key. The graph is
    /// read-only after `finalize`, so the cache never goes stale; `transitive` falls
    /// back to computing when it's absent (a not-yet-finalized graph, e.g. in tests).
    transitive_counts: HashMap<String, (usize, usize)>,
}

impl Graph {
    pub fn new(project: impl Into<String>) -> Self {
        Graph {
            project: project.into(),
            nodes: HashMap::new(),
            order: Vec::new(),
            blocks: HashMap::new(),
            blocked_by: HashMap::new(),
            edges: HashSet::new(),
            cycles: Vec::new(),
            in_cycle: HashSet::new(),
            back_edges: HashSet::new(),
            transitive_counts: HashMap::new(),
        }
    }

    /// Insert (or replace) an issue node, preserving first-seen order.
    pub fn add_issue(&mut self, issue: Issue) {
        if !self.nodes.contains_key(&issue.key) {
            self.order.push(issue.key.clone());
        }
        self.nodes.insert(issue.key.clone(), issue);
    }

    /// Ensure a referenced endpoint exists, materialising it as an external
    /// node if we never fetched it directly.
    pub fn ensure_external(&mut self, key: &str, title: &str) {
        if !self.nodes.contains_key(key) {
            self.add_issue(Issue {
                key: key.to_string(),
                title: title.to_string(),
                status: Status::Unknown,
                priority: Priority::None,
                assignee: None,
                external: true,
            });
        }
    }

    /// Record a directed `blocks` edge (blocker → blocked), de-duplicated. The
    /// same relation arrives twice (once per direction from Linear), so the
    /// edge set collapses them.
    pub fn add_edge(&mut self, blocker: &str, blocked: &str) {
        if blocker == blocked {
            return; // a self-block carries no information
        }
        let edge = (blocker.to_string(), blocked.to_string());
        if !self.edges.insert(edge) {
            return; // already known
        }
        self.blocks
            .entry(blocker.to_string())
            .or_default()
            .push(blocked.to_string());
        self.blocked_by
            .entry(blocked.to_string())
            .or_default()
            .push(blocker.to_string());
    }

    /// Run cycle detection and sort neighbour lists into a stable display
    /// order. Call once after all issues and edges are loaded.
    pub fn finalize(&mut self) {
        let order_index: HashMap<&str, usize> = self
            .order
            .iter()
            .enumerate()
            .map(|(i, k)| (k.as_str(), i))
            .collect();
        let by_order = |a: &String, b: &String| {
            let ia = order_index.get(a.as_str()).copied().unwrap_or(usize::MAX);
            let ib = order_index.get(b.as_str()).copied().unwrap_or(usize::MAX);
            ia.cmp(&ib)
        };
        for v in self.blocks.values_mut() {
            v.sort_by(by_order);
        }
        for v in self.blocked_by.values_mut() {
            v.sort_by(by_order);
        }
        self.detect_cycles();
        self.mark_cycle_members();
        // Precompute transitive closure sizes so the render hot path reads them O(1).
        // Computed into a local first (immutable borrows of `self`), then stored.
        let counts: HashMap<String, (usize, usize)> = self
            .order
            .iter()
            .map(|k| {
                let up = self.compute_transitive(k, Direction::Upstream);
                let down = self.compute_transitive(k, Direction::Downstream);
                (k.clone(), (up, down))
            })
            .collect();
        self.transitive_counts = counts;
    }

    /// Classic three-colour DFS over downstream edges, recording back-edges and
    /// a representative cycle path for each one. These drive the overview's
    /// "CYCLES" list and the back-edge skip in [`Graph::levels`]. Cycle
    /// *membership* (`in_cycle`) is computed separately in [`Graph::mark_cycle_members`]
    /// from strongly-connected components, because a single DFS's gray back-edges
    /// do not enumerate every node on an overlapping cycle.
    fn detect_cycles(&mut self) {
        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            White,
            Gray,
            Black,
        }
        let mut color: HashMap<String, Mark> = self
            .order
            .iter()
            .map(|k| (k.clone(), Mark::White))
            .collect();

        // Iterative DFS so deep graphs cannot overflow the stack.
        for ri in 0..self.order.len() {
            let root = self.order[ri].clone();
            if color[&root] != Mark::White {
                continue;
            }
            let mut path: Vec<String> = Vec::new();
            // (node, index-into-its-children)
            let mut stack: Vec<(String, usize)> = vec![(root.clone(), 0)];
            color.insert(root, Mark::Gray);
            while let Some((node, idx)) = stack.last().map(|t| (t.0.clone(), t.1)) {
                if idx == 0 {
                    path.push(node.clone());
                }
                let n_children = self.blocks.get(&node).map_or(0, Vec::len);
                if idx < n_children {
                    stack.last_mut().unwrap().1 += 1;
                    let child = self.blocks[&node][idx].clone();
                    match color.get(&child).copied().unwrap_or(Mark::White) {
                        Mark::White => {
                            color.insert(child.clone(), Mark::Gray);
                            stack.push((child, 0));
                        }
                        Mark::Gray => {
                            // Back-edge: child is an ancestor on the stack.
                            self.back_edges.insert((node.clone(), child.clone()));
                            if let Some(start) = path.iter().position(|n| *n == child) {
                                let mut cycle = path[start..].to_vec();
                                cycle.push(child);
                                self.cycles.push(cycle);
                            }
                        }
                        Mark::Black => {}
                    }
                } else {
                    color.insert(node.clone(), Mark::Black);
                    path.pop();
                    stack.pop();
                }
            }
        }
    }

    /// Mark every node that lies on a directed cycle, via Tarjan's
    /// strongly-connected-components over the downstream `blocks` edges. A node
    /// is on a cycle iff its SCC has more than one member (self-edges are
    /// rejected by [`Graph::add_edge`], so a singleton SCC is never cyclic).
    ///
    /// This supersedes deriving membership from [`Graph::detect_cycles`]'s
    /// back-edges: gray-back-edge detection proves a cycle *exists* but does not
    /// enumerate every node on it when cycles overlap (an edge into an already
    /// finished node still closes a cycle, yet records no back-edge). Kept
    /// iterative — like `detect_cycles` — so deep graphs cannot overflow the stack.
    fn mark_cycle_members(&mut self) {
        let n = self.order.len();
        let index: HashMap<&str, usize> = self
            .order
            .iter()
            .enumerate()
            .map(|(i, k)| (k.as_str(), i))
            .collect();
        // Downstream adjacency as index lists, so the SCC pass needs no string work.
        let adj: Vec<Vec<usize>> = self
            .order
            .iter()
            .map(|k| {
                self.blocks
                    .get(k)
                    .map(|v| {
                        v.iter()
                            .filter_map(|c| index.get(c.as_str()).copied())
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect();
        drop(index);

        const UNVISITED: usize = usize::MAX;
        let mut disc = vec![UNVISITED; n]; // discovery order
        let mut low = vec![0usize; n]; // lowest discovery reachable
        let mut on_stack = vec![false; n];
        let mut comp_stack: Vec<usize> = Vec::new();
        let mut cyclic = vec![false; n];
        let mut counter = 0usize;
        let mut component: Vec<usize> = Vec::new();

        for s in 0..n {
            if disc[s] != UNVISITED {
                continue;
            }
            // Explicit work stack of (node, next-child-index) — iterative Tarjan.
            let mut work: Vec<(usize, usize)> = vec![(s, 0)];
            while let Some(&(v, ci)) = work.last() {
                if ci == 0 {
                    disc[v] = counter;
                    low[v] = counter;
                    counter += 1;
                    comp_stack.push(v);
                    on_stack[v] = true;
                }
                if ci < adj[v].len() {
                    let w = adj[v][ci];
                    work.last_mut().unwrap().1 += 1;
                    if disc[w] == UNVISITED {
                        work.push((w, 0));
                    } else if on_stack[w] {
                        low[v] = low[v].min(disc[w]);
                    }
                } else {
                    // Finished v: if it roots an SCC, pop the whole component.
                    if low[v] == disc[v] {
                        loop {
                            let w = comp_stack.pop().unwrap();
                            on_stack[w] = false;
                            component.push(w);
                            if w == v {
                                break;
                            }
                        }
                        if component.len() > 1 {
                            for &w in &component {
                                cyclic[w] = true;
                            }
                        }
                        component.clear();
                    }
                    work.pop();
                    if let Some(&(parent, _)) = work.last() {
                        low[parent] = low[parent].min(low[v]);
                    }
                }
            }
        }

        for (i, &is_cyclic) in cyclic.iter().enumerate() {
            if is_cyclic {
                self.in_cycle.insert(self.order[i].clone());
            }
        }
    }

    // ── Queries ─────────────────────────────────────────────────────────────

    pub fn get(&self, key: &str) -> Option<&Issue> {
        self.nodes.get(key)
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    pub fn cycle_count(&self) -> usize {
        self.cycles.len()
    }

    pub fn cycles(&self) -> &[Vec<String>] {
        &self.cycles
    }

    pub fn in_cycle(&self, key: &str) -> bool {
        self.in_cycle.contains(key)
    }

    /// Every key in stable display order.
    pub fn keys(&self) -> &[String] {
        &self.order
    }

    /// Neighbours of `key` in the given direction.
    pub fn neighbours(&self, key: &str, dir: Direction) -> &[String] {
        let map = match dir {
            Direction::Upstream => &self.blocked_by,
            Direction::Downstream => &self.blocks,
        };
        map.get(key).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn direct_count(&self, key: &str, dir: Direction) -> usize {
        self.neighbours(key, dir).len()
    }

    /// An issue is *blocked* when at least one upstream blocker is unresolved.
    pub fn is_blocked(&self, key: &str) -> bool {
        self.neighbours(key, Direction::Upstream)
            .iter()
            .any(|b| self.get(b).is_none_or(|i| !i.status.is_resolved()))
    }

    /// Distinct issues reachable in `dir` (transitive closure, excluding self).
    /// O(1) after [`finalize`](Self::finalize) (reads the precomputed cache); falls
    /// back to a live BFS for a not-yet-finalized graph.
    pub fn transitive(&self, key: &str, dir: Direction) -> usize {
        if let Some(&(up, down)) = self.transitive_counts.get(key) {
            return match dir {
                Direction::Upstream => up,
                Direction::Downstream => down,
            };
        }
        self.compute_transitive(key, dir)
    }

    /// The BFS behind [`transitive`](Self::transitive), run live (the cache is built
    /// by calling this for every node in `finalize`).
    fn compute_transitive(&self, key: &str, dir: Direction) -> usize {
        // Seed with `key` so a cycle that walks back to it isn't counted; the
        // seed is subtracted at the end to honour the "excluding self" contract.
        let mut seen = HashSet::new();
        seen.insert(key.to_string());
        let mut stack: Vec<String> = self.neighbours(key, dir).to_vec();
        while let Some(n) = stack.pop() {
            if seen.insert(n.clone()) {
                for next in self.neighbours(&n, dir) {
                    if !seen.contains(next) {
                        stack.push(next.clone());
                    }
                }
            }
        }
        seen.len() - 1
    }

    /// Issues that participate in any cycle, in display order.
    pub fn cycle_members(&self) -> Vec<String> {
        self.order
            .iter()
            .filter(|k| self.in_cycle.contains(*k))
            .cloned()
            .collect()
    }

    /// External (cross-project) endpoints, in display order.
    pub fn externals(&self) -> Vec<&Issue> {
        self.order
            .iter()
            .filter_map(|k| self.nodes.get(k))
            .filter(|i| i.external)
            .collect()
    }

    /// How many nodes are materialized external blockers — the count the header
    /// subtracts from [`Self::len`] to match the Spine's issue count, without the
    /// per-render `Vec` allocation [`Self::externals`] would cost on the hot path.
    pub fn external_count(&self) -> usize {
        self.order
            .iter()
            .filter_map(|k| self.nodes.get(k))
            .filter(|i| i.external)
            .count()
    }

    /// Longest-path layering over the acyclic part of the graph (back-edges
    /// removed). Level 0 holds roots with no real blockers; each subsequent
    /// level sits below its deepest blocker. Returns bands in level order.
    pub fn levels(&self) -> Vec<Vec<String>> {
        let level = self.compute_levels();
        let max = level.values().copied().max().unwrap_or(0);
        let mut bands = vec![Vec::new(); max + 1];
        for key in &self.order {
            bands[level[key]].push(key.clone());
        }
        bands
    }

    /// A node's longest-path level: `1 + max(level of its real upstream
    /// blockers)`, or 0 if it has none. Iterative post-order memoisation over an
    /// explicit work-stack (like `detect_cycles`/`mark_cycle_members`) so depth
    /// lives on the heap, not the call stack, and a deep blocker chain cannot
    /// overflow it. Back-edges and the on-path guard prune cyclic re-entry, so
    /// the relaxation always terminates.
    fn compute_levels(&self) -> HashMap<String, usize> {
        let mut level: HashMap<String, usize> = HashMap::new();
        // True for any blocker we must never traverse: a back-edge (would re-enter
        // the DFS path) or a blocker already on the current path (a cycle). A node
        // skipped this way contributes nothing to a level, exactly as the old
        // recursive `level_of` skipped it.
        let skip = |blocker: &str, node: &str, on_path: &HashSet<&str>| {
            on_path.contains(blocker)
                || self
                    .back_edges
                    .contains(&(blocker.to_string(), node.to_string()))
        };
        for root in &self.order {
            if level.contains_key(root) {
                continue;
            }
            // Nodes currently descended-into; a blocker already on this path is a
            // cycle edge and is skipped, mirroring the old recursive guard.
            let mut on_path: HashSet<&str> = HashSet::new();
            // (node, next-blocker-index). `idx == 0` is the pre-visit.
            let mut stack: Vec<(&str, usize)> = vec![(root.as_str(), 0)];
            on_path.insert(root.as_str());
            while let Some(&(node, idx)) = stack.last() {
                let blockers = self.neighbours(node, Direction::Upstream);
                // Walk forward to the next unresolved, traversable blocker. Already
                // resolved blockers (memoised in `level`) need no descent; we fold
                // them in during the post-order relax below.
                let mut i = idx;
                let mut pending: Option<&str> = None;
                while i < blockers.len() {
                    let blocker = blockers[i].as_str();
                    i += 1;
                    if skip(blocker, node, &on_path) || level.contains_key(blocker) {
                        continue;
                    }
                    pending = Some(blocker);
                    break;
                }
                if let Some(top) = stack.last_mut() {
                    top.1 = i;
                }
                if let Some(blocker) = pending {
                    on_path.insert(blocker);
                    stack.push((blocker, 0));
                    continue;
                }
                // All blockers visited: relax `node` from its resolved blockers.
                let best = blockers
                    .iter()
                    .filter(|b| !skip(b.as_str(), node, &on_path))
                    .filter_map(|b| level.get(b.as_str()).map(|&l| l + 1))
                    .max()
                    .unwrap_or(0);
                level.insert(node.to_string(), best);
                on_path.remove(node);
                stack.pop();
            }
        }
        level
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(key: &str, status: Status) -> Issue {
        Issue {
            key: key.into(),
            title: key.into(),
            status,
            priority: Priority::None,
            assignee: None,
            external: false,
        }
    }

    /// A → B → C, plus an external X that blocks A. No cycles.
    fn chain() -> Graph {
        let mut g = Graph::new("t");
        g.add_issue(node("A", Status::Started));
        g.add_issue(node("B", Status::Unstarted));
        g.add_issue(node("C", Status::Backlog));
        g.ensure_external("X-1", "ext");
        g.add_edge("X-1", "A");
        g.add_edge("A", "B");
        g.add_edge("B", "C");
        g.finalize();
        g
    }

    #[test]
    fn edges_are_directional_and_deduped() {
        let g = chain();
        assert_eq!(g.edge_count(), 3);
        assert_eq!(g.neighbours("A", Direction::Downstream), &["B"]);
        assert_eq!(g.neighbours("B", Direction::Upstream), &["A"]);
    }

    #[test]
    fn duplicate_edges_collapse() {
        let mut g = Graph::new("t");
        g.add_issue(node("A", Status::Started));
        g.add_issue(node("B", Status::Started));
        g.add_edge("A", "B");
        g.add_edge("A", "B"); // same relation seen from the other side
        g.finalize();
        assert_eq!(g.edge_count(), 1);
        assert_eq!(g.neighbours("A", Direction::Downstream).len(), 1);
    }

    #[test]
    fn transitive_closure_counts_distinct_reachable() {
        let g = chain();
        assert_eq!(g.transitive("A", Direction::Downstream), 2); // B, C
        assert_eq!(g.transitive("C", Direction::Upstream), 3); // B, A, X-1
    }

    #[test]
    fn blocked_only_when_blocker_unresolved() {
        let g = chain();
        // B is blocked by A (Started → unresolved).
        assert!(g.is_blocked("B"));

        let mut g2 = Graph::new("t");
        g2.add_issue(node("A", Status::Completed));
        g2.add_issue(node("B", Status::Unstarted));
        g2.add_edge("A", "B");
        g2.finalize();
        // A is done, so B is not actually blocked.
        assert!(!g2.is_blocked("B"));
    }

    #[test]
    fn cycle_is_detected_without_recursing_forever() {
        let mut g = Graph::new("t");
        g.add_issue(node("A", Status::Started));
        g.add_issue(node("B", Status::Started));
        g.add_issue(node("C", Status::Started));
        g.add_edge("A", "B");
        g.add_edge("B", "C");
        g.add_edge("C", "A"); // closes the loop
        g.finalize();
        assert_eq!(g.cycle_count(), 1);
        assert!(g.in_cycle("A") && g.in_cycle("B") && g.in_cycle("C"));
        assert_eq!(g.cycle_members().len(), 3);

        // A node on a cycle must not count itself in its transitive closure.
        assert_eq!(g.transitive("A", Direction::Downstream), 2); // B, C — not A
        assert_eq!(g.transitive("A", Direction::Upstream), 2); // C, B — not A
    }

    #[test]
    fn overlapping_cycles_mark_every_member() {
        // Two cycles share nodes: A→B→A and A→C→B→A. Gray-back-edge detection
        // alone records only one back-edge and, walking A→B (back-edge B→A) then
        // A→C, finds C's only child B already finished — so the cycle through C
        // goes unrecorded and C would be wrongly reported as not-in-a-cycle. SCC
        // membership must still flag all three.
        let mut g = Graph::new("t");
        for k in ["A", "B", "C"] {
            g.add_issue(node(k, Status::Started));
        }
        g.add_edge("A", "B");
        g.add_edge("A", "C");
        g.add_edge("B", "A");
        g.add_edge("C", "B");
        g.finalize();

        assert!(g.in_cycle("A"));
        assert!(g.in_cycle("B"));
        assert!(g.in_cycle("C"), "C is on cycle A→C→B→A but was not flagged");
        assert_eq!(g.cycle_members(), vec!["A", "B", "C"]);
    }

    #[test]
    fn acyclic_graph_marks_no_members() {
        let g = chain(); // A→B→C with external X-1→A, no cycles
        assert!(g.cycle_members().is_empty());
        for k in ["A", "B", "C", "X-1"] {
            assert!(!g.in_cycle(k));
        }
    }

    #[test]
    fn levels_terminate_and_are_finite_on_a_cycle() {
        // The back-edge skip in level_of only matters on a cyclic graph; assert it
        // terminates and yields a clean layering instead of hanging or panicking.
        let mut g = Graph::new("t");
        for k in ["A", "B", "C"] {
            g.add_issue(node(k, Status::Started));
        }
        g.add_edge("A", "B");
        g.add_edge("B", "C");
        g.add_edge("C", "A"); // closes the loop
        g.finalize();

        let bands = g.levels();
        let placed: usize = bands.iter().map(Vec::len).sum();
        assert_eq!(placed, 3, "every node lands in exactly one band");
        assert!(!bands.is_empty());
    }

    #[test]
    fn levels_layer_by_longest_path() {
        let g = chain();
        let bands = g.levels();
        // X-1 at L0, A at L1, B at L2, C at L3.
        assert_eq!(bands.len(), 4);
        assert!(bands[0].contains(&"X-1".to_string()));
        assert!(bands[3].contains(&"C".to_string()));
    }

    #[test]
    fn levels_take_the_longest_path_through_a_diamond() {
        // A → B → D and A → C, C → D. D's level must follow the longer A→B→D
        // path (L2), not the shorter A→C→D… here both are length 2, so add a
        // detour: A→B1→B2→D (len 3) vs A→C→D (len 2). D lands at L3.
        let mut g = Graph::new("t");
        for k in ["A", "B1", "B2", "C", "D"] {
            g.add_issue(node(k, Status::Started));
        }
        g.add_edge("A", "B1");
        g.add_edge("B1", "B2");
        g.add_edge("B2", "D");
        g.add_edge("A", "C");
        g.add_edge("C", "D");
        g.finalize();

        let bands = g.levels();
        let level_of = |key: &str| bands.iter().position(|b| b.contains(&key.to_string()));
        assert_eq!(level_of("A"), Some(0));
        assert_eq!(level_of("C"), Some(1));
        assert_eq!(level_of("B2"), Some(2));
        // D sits below its DEEPEST blocker (B2 at L2), not the shallower C.
        assert_eq!(level_of("D"), Some(3));
    }

    #[test]
    fn levels_do_not_overflow_the_stack_on_a_deep_chain() {
        // The recursive layering would consume one stack frame per node of the
        // deepest blocker chain; a release-train-length chain (thousands of
        // issues) overflows and aborts mid-render. The iterative relaxation must
        // place every node with depth bounded by the heap, not the call stack.
        const N: usize = 20_000;
        let mut g = Graph::new("t");
        for i in 0..N {
            g.add_issue(node(&format!("I-{i}"), Status::Started));
        }
        for i in 0..N - 1 {
            // I-i blocks I-(i+1): a single chain N deep.
            g.add_edge(&format!("I-{i}"), &format!("I-{}", i + 1));
        }
        g.finalize();

        let bands = g.levels();
        assert_eq!(bands.len(), N, "one band per link in the chain");
        let placed: usize = bands.iter().map(Vec::len).sum();
        assert_eq!(placed, N, "every node lands in exactly one band");
        // The tail of the chain is at the deepest level.
        assert!(bands[N - 1].contains(&format!("I-{}", N - 1)));
    }

    #[test]
    fn external_endpoints_are_materialized() {
        let g = chain();
        let ext = g.externals();
        assert_eq!(ext.len(), 1);
        assert_eq!(ext[0].key, "X-1");
        assert!(ext[0].external);
    }
}
