//! The cockpit's window model — the sole owner of every window type.
//!
//! Cockpit v3 is a tmux-style tiling window manager whose panes ("windows") are
//! live, focusable columns: the permanent **Spine** (issue list / agents
//! roster), N live **Agent** PTYs, and **Deps** trees (a per-issue dependency
//! lens, or the project-wide Fleet map). Every other module consumes the types
//! defined here rather than minting its own — there is exactly one `WindowKind`,
//! one `WindowId`, one `WindowSet`. (The v2 split that invented three
//! incompatible `Window` types is what this charter exists to prevent.)
//!
//! The control plane (supervisor / backends / sessions) is untouched by any of
//! this: v3 is a UI/keymap reshape over a proven process layer.

use std::collections::HashSet;

use ratatui::widgets::ListState;

use crate::model::{Direction, Graph};

/// A monotonic, never-reused window identifier. Used as the key for per-window
/// PTY resize bookkeeping (`preview_size`) so that zoom — which can show the
/// same issue at two geometries across the toggle frame — and any future
/// duplicate-window feature stay unambiguous. A `u64` counter never wraps in a
/// human-driven session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowId(pub u64);

/// What a [`WindowKind::Deps`] window is rooted on: a single issue's dependency
/// tree, or the project-wide Fleet overview (the old graph map). Kept a distinct
/// enum (rather than two `WindowKind` variants) so persistence and the renderer
/// can pattern-match the deps *family* in one arm. An `Issue` window's live root
/// (which re-rooting moves) lives in its [`DepsCursor`] — the single source of
/// truth, doubling as the window's persistence identity — so this carries no
/// payload and never drifts from the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepsRoot {
    /// A per-issue upstream/downstream lens (root in the window's `deps` cursor).
    Issue,
    /// The layered, edge-free overview of the whole project.
    Fleet,
}

/// The kind of a window. The Spine is permanent and always sits at index 0;
/// Agent and Deps windows are opened, pinned, closed and killed by the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowKind {
    /// The navigation spine: the issue list or the agents roster (the `r` tab
    /// toggle lives on it). Permanent — never closed, always window 0.
    Spine,
    /// A live `claude` PTY, keyed by its issue. The old single "attached" pane,
    /// now one of N simultaneous columns.
    Agent(String),
    /// A dependency view — a per-issue tree, or the Fleet map.
    Deps(DepsRoot),
}

impl WindowKind {
    /// The issue an Agent window renders, if this is one.
    pub fn agent_issue(&self) -> Option<&str> {
        match self {
            WindowKind::Agent(issue) => Some(issue.as_str()),
            _ => None,
        }
    }
}

/// How the strip tiles its windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    /// Spine pinned left; non-spine windows are fixed-width columns, only the
    /// ones that fully fit are drawn, the strip scrolls horizontally. The
    /// headline v3 layout.
    Filmstrip,
    /// Every window tiled near-square to fill the viewport (reuses the chat
    /// wall's `split_grid`). Proven first because it sidesteps the tui-term
    /// horizontal-clipping problem entirely.
    Mosaic,
}

impl LayoutMode {
    /// The other layout — for the `|` toggle.
    pub const fn toggled(self) -> Self {
        match self {
            LayoutMode::Filmstrip => LayoutMode::Mosaic,
            LayoutMode::Mosaic => LayoutMode::Filmstrip,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            LayoutMode::Filmstrip => "filmstrip",
            LayoutMode::Mosaic => "mosaic",
        }
    }
}

/// Which dependency pane a [`DepsCursor`] currently drives. The v2 lens cycled
/// List → Upstream → Downstream; the List is now the Spine, so a Deps window
/// only toggles between its two trees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepsSide {
    Up,
    Down,
}

impl DepsSide {
    const fn toggled(self) -> Self {
        match self {
            DepsSide::Up => DepsSide::Down,
            DepsSide::Down => DepsSide::Up,
        }
    }
}

/// How a node renders inside a dependency tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Normal,
    Cycle,    // back-edge to an ancestor on the current path
    Ref,      // already drawn elsewhere in this tree (a join, not a cycle)
    External, // lives outside the project — a terminal leaf
}

/// One rendered line of a dependency tree.
#[derive(Debug, Clone)]
pub struct TreeRow {
    pub key: String,
    pub prefix: String,
    pub kind: NodeKind,
    pub has_children: bool,
    pub collapsed: bool,
}

/// The independent navigation state of one Deps(Issue) window: its live root,
/// its back-history, the flattened upstream/downstream trees, the per-tree
/// selection, the per-root collapse set, and which tree is active. Extended from
/// the parallel-design draft's `{side, up, down}` to the full
/// `{side, up, down, root, history, collapsed}` so every Deps window keeps v2's
/// re-root / Back / collapse gestures — independently of every other window.
#[derive(Debug, Clone)]
pub struct DepsCursor {
    /// The issue currently at the root of this lens. `Enter` on a node moves it;
    /// `Back` pops `history`.
    pub root: String,
    history: Vec<String>,
    pub up_rows: Vec<TreeRow>,
    pub down_rows: Vec<TreeRow>,
    pub up_state: ListState,
    pub down_state: ListState,
    collapsed: HashSet<String>, // "U:KEY" / "D:KEY"
    collapsed_for: String,      // the root `collapsed` belongs to; reset on re-root
    pub side: DepsSide,
}

impl DepsCursor {
    /// A fresh cursor rooted at `root`, with its trees built from `graph`.
    pub fn new(root: String, graph: &Graph) -> Self {
        let mut cursor = DepsCursor {
            root,
            history: Vec::new(),
            up_rows: Vec::new(),
            down_rows: Vec::new(),
            up_state: ListState::default(),
            down_state: ListState::default(),
            collapsed: HashSet::new(),
            collapsed_for: String::new(),
            side: DepsSide::Up,
        };
        cursor.rebuild(graph);
        cursor
    }

    /// Rebuild both trees for the current root, clearing collapse state when the
    /// root moved (so each issue opens fully expanded; a collapse toggle leaves
    /// the root unchanged and keeps its state).
    pub fn rebuild(&mut self, graph: &Graph) {
        if self.collapsed_for != self.root {
            self.collapsed.clear();
            self.collapsed_for = self.root.clone();
        }
        self.up_rows = build_forest(graph, &self.root, Direction::Upstream, &self.collapsed);
        self.down_rows = build_forest(graph, &self.root, Direction::Downstream, &self.collapsed);
        clamp_selection(&mut self.up_state, self.up_rows.len());
        clamp_selection(&mut self.down_state, self.down_rows.len());
    }

    /// The active tree's rows + selection, for movement and re-root.
    fn active(&self) -> (&[TreeRow], &ListState) {
        match self.side {
            DepsSide::Up => (&self.up_rows, &self.up_state),
            DepsSide::Down => (&self.down_rows, &self.down_state),
        }
    }

    /// The row under the cursor in the active tree.
    pub fn selected_row(&self) -> Option<&TreeRow> {
        let (rows, state) = self.active();
        state.selected().and_then(|i| rows.get(i))
    }

    /// Move the active tree's selection by `delta`, wrapping.
    pub fn move_selection(&mut self, delta: i32) {
        let (rows, state) = match self.side {
            DepsSide::Up => (&self.up_rows, &mut self.up_state),
            DepsSide::Down => (&self.down_rows, &mut self.down_state),
        };
        move_state(state, rows.len(), delta);
    }

    /// Flip the active tree (upstream ↔ downstream).
    pub fn switch_side(&mut self) {
        self.side = self.side.toggled();
    }

    /// Re-root onto the selected node, pushing the old root onto history. Returns
    /// `Err(reason)` for an external node (which can't be re-rooted), so the
    /// caller can surface why. A no-op (and `Ok`) when nothing is selected.
    pub fn enter(&mut self, graph: &Graph) -> Result<(), String> {
        let Some(row) = self.selected_row() else {
            return Ok(());
        };
        if row.kind == NodeKind::External {
            let team = graph.get(&row.key).map(|i| i.team()).unwrap_or("?");
            return Err(format!(
                "{} is external (team {team}) — open it in Linear to follow its chain",
                row.key
            ));
        }
        let key = row.key.clone();
        if key == self.root {
            return Ok(());
        }
        self.history.push(std::mem::replace(&mut self.root, key));
        self.rebuild(graph);
        Ok(())
    }

    /// Pop back to the previously-rooted issue. Returns whether there was one.
    pub fn back(&mut self, graph: &Graph) -> bool {
        match self.history.pop() {
            Some(prev) => {
                self.root = prev;
                self.rebuild(graph);
                true
            }
            None => false,
        }
    }

    /// Collapse or expand the selected subtree (only when it has children).
    pub fn toggle_collapse(&mut self, graph: &Graph) {
        let tag = match self.side {
            DepsSide::Up => "U",
            DepsSide::Down => "D",
        };
        if let Some(row) = self.selected_row()
            && row.has_children
        {
            let id = format!("{tag}:{}", row.key);
            if !self.collapsed.remove(&id) {
                self.collapsed.insert(id);
            }
            self.rebuild(graph);
        }
    }
}

/// One window in the strip.
#[derive(Debug, Clone)]
pub struct Window {
    pub id: WindowId,
    pub kind: WindowKind,
    /// Pinned windows persist (docked, in pin order) and are restored on
    /// restart; an unpinned window is a transient preview, dropped on `close`
    /// and never persisted.
    pub pinned: bool,
    /// Per-window dependency navigation — `Some` exactly for `Deps(Issue)`
    /// windows, `None` for Spine / Agent / Deps(Fleet).
    pub deps: Option<DepsCursor>,
}

impl Window {
    /// A short, human label for the window's status line / persistence — not the
    /// rendered title (which the renderer composes with live agent state). For a
    /// Deps(Issue) window this is the cursor's live root.
    pub fn issue(&self) -> Option<&str> {
        match &self.kind {
            WindowKind::Agent(issue) => Some(issue.as_str()),
            WindowKind::Deps(DepsRoot::Issue) => self.deps.as_ref().map(|c| c.root.as_str()),
            _ => None,
        }
    }
}

/// The ordered strip of windows plus all view-wide window state: which one is
/// focused, the horizontal scroll (filmstrip), the layout mode, and the zoom
/// toggle. The single source of truth for "what's on screen" once v3 lands —
/// replacing v2's `attached` / `mode` / `right_view` / `chat_split` / `pinned`.
#[derive(Debug, Clone)]
pub struct WindowSet {
    /// All columns in display order. `windows[0]` is always the Spine.
    pub windows: Vec<Window>,
    /// Index into `windows` of the focused column.
    pub focus: usize,
    /// Source of the next [`WindowId`]. Monotonic; never reused.
    next_id: u64,
    pub layout: LayoutMode,
    /// Horizontal scroll offset, in *columns* (filmstrip): how many non-spine
    /// windows are scrolled off the left. Only ever mutated by focus-move /
    /// resize handlers, never by `draw` — preserving the render-mutation
    /// contract.
    pub scroll_x: usize,
    /// `true` while a single window is zoomed to fill the viewport.
    pub zoomed: bool,
    /// The `scroll_x` saved when zoom was entered, restored on unzoom so the
    /// strip returns to exactly where it was.
    pre_zoom_scroll: usize,
}

impl Default for WindowSet {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowSet {
    /// A fresh set: just the permanent Spine, focused.
    pub fn new() -> Self {
        WindowSet {
            windows: vec![Window {
                id: WindowId(0),
                kind: WindowKind::Spine,
                pinned: true,
                deps: None,
            }],
            focus: 0,
            next_id: 1,
            layout: LayoutMode::Filmstrip,
            scroll_x: 0,
            zoomed: false,
            pre_zoom_scroll: 0,
        }
    }

    fn next_window_id(&mut self) -> WindowId {
        let id = WindowId(self.next_id);
        self.next_id += 1;
        id
    }

    /// The focused window (always valid: the Spine can never be removed).
    pub fn focused(&self) -> &Window {
        &self.windows[self.focus]
    }

    pub fn focused_mut(&mut self) -> &mut Window {
        &mut self.windows[self.focus]
    }

    pub fn focused_kind(&self) -> &WindowKind {
        &self.windows[self.focus].kind
    }

    /// Index of the Agent window for `issue`, if one is open.
    pub fn agent_window(&self, issue: &str) -> Option<usize> {
        self.windows
            .iter()
            .position(|w| w.kind.agent_issue() == Some(issue))
    }

    /// Whether any window references `issue`'s backend — an Agent window for it.
    /// Used by the backend-reclaim predicate (generalising v2's `attached`).
    pub fn references_agent(&self, issue: &str) -> bool {
        self.windows
            .iter()
            .any(|w| w.kind.agent_issue() == Some(issue))
    }

    /// The lone unpinned window of a given family (the transient "preview"), if
    /// one is open. Used to keep at most one unpinned Agent and one unpinned Deps
    /// preview at a time: opening another replaces it (the agent keeps running —
    /// the roster is the refind net), so unpinned windows never accumulate.
    fn unpinned_agent(&self) -> Option<usize> {
        self.windows
            .iter()
            .position(|w| !w.pinned && matches!(w.kind, WindowKind::Agent(_)))
    }

    fn unpinned_deps(&self) -> Option<usize> {
        self.windows
            .iter()
            .position(|w| !w.pinned && matches!(w.kind, WindowKind::Deps(DepsRoot::Issue)))
    }

    /// Remove the window at `idx` (never the Spine) and fix focus. Returns it so
    /// the caller can reclaim a backend / drop geometry.
    fn remove(&mut self, idx: usize) -> Option<Window> {
        if idx == 0 || idx >= self.windows.len() {
            return None;
        }
        let removed = self.windows.remove(idx);
        if self.focus >= self.windows.len() {
            self.focus = self.windows.len() - 1;
        } else if self.focus > idx {
            self.focus -= 1;
        }
        Some(removed)
    }

    /// Open (or focus, if already open) an Agent window for `issue`. A new window
    /// is the unpinned Agent *preview*: it replaces any prior unpinned Agent
    /// window (whose agent keeps running). Returns `(focused id, displaced
    /// preview)` so the caller can reclaim the displaced agent's backend.
    pub fn open_or_focus_agent(&mut self, issue: &str) -> (WindowId, Option<Window>) {
        if let Some(i) = self.agent_window(issue) {
            self.focus = i;
            return (self.windows[i].id, None);
        }
        let displaced = self.unpinned_agent().and_then(|i| self.remove(i));
        let id = self.next_window_id();
        self.windows.push(Window {
            id,
            kind: WindowKind::Agent(issue.to_string()),
            pinned: false,
            deps: None,
        });
        self.focus = self.windows.len() - 1;
        (id, displaced)
    }

    /// Open a per-issue Deps window rooted at `root`, or re-root the existing
    /// unpinned Deps preview onto it — so browsing deps doesn't spawn a window
    /// per issue. Returns the focused window's id.
    pub fn open_or_reroot_deps(&mut self, root: String, graph: &Graph) -> WindowId {
        if let Some(i) = self.unpinned_deps() {
            let cursor = DepsCursor::new(root, graph);
            self.windows[i].deps = Some(cursor);
            self.focus = i;
            return self.windows[i].id;
        }
        let id = self.next_window_id();
        self.windows.push(Window {
            id,
            kind: WindowKind::Deps(DepsRoot::Issue),
            pinned: false,
            deps: Some(DepsCursor::new(root, graph)),
        });
        self.focus = self.windows.len() - 1;
        id
    }

    /// Open (or focus) the single Fleet overview window — there's only ever one.
    pub fn open_fleet(&mut self) -> WindowId {
        if let Some(i) = self
            .windows
            .iter()
            .position(|w| matches!(w.kind, WindowKind::Deps(DepsRoot::Fleet)))
        {
            self.focus = i;
            return self.windows[i].id;
        }
        let id = self.next_window_id();
        self.windows.push(Window {
            id,
            kind: WindowKind::Deps(DepsRoot::Fleet),
            pinned: false,
            deps: None,
        });
        self.focus = self.windows.len() - 1;
        id
    }

    /// Close the focused window (undock). The Spine can't be closed — closing it
    /// is a no-op. Focus falls back to the previous window. Returns the closed
    /// window so the caller can reclaim its backend handle / drop its geometry.
    pub fn close_focused(&mut self) -> Option<Window> {
        let removed = self.remove(self.focus)?;
        if self.zoomed {
            self.zoomed = false;
            self.scroll_x = self.pre_zoom_scroll;
        }
        Some(removed)
    }

    /// Append a window with the next id, focusing it. Used by persistence
    /// restore and the auto-resume placeholders, where the kind/pin state are
    /// already decided by the caller. `pinned` is honoured as given.
    pub fn push(&mut self, kind: WindowKind, pinned: bool, deps: Option<DepsCursor>) -> WindowId {
        let id = self.next_window_id();
        self.windows.push(Window {
            id,
            kind,
            pinned,
            deps,
        });
        self.focus = self.windows.len() - 1;
        id
    }

    /// The focused window's 0-based position among the *non-spine* windows, or
    /// `None` when the Spine is focused — for keeping it in view while scrolling.
    pub fn focus_column(&self) -> Option<usize> {
        (self.focus > 0).then(|| self.focus - 1)
    }

    /// Count of non-spine windows.
    pub fn non_spine_count(&self) -> usize {
        self.windows.len().saturating_sub(1)
    }

    /// Toggle the focused window's pin (persistence). The Spine is always pinned.
    pub fn toggle_pin_focused(&mut self) -> bool {
        if self.focus == 0 {
            return true;
        }
        let w = &mut self.windows[self.focus];
        w.pinned = !w.pinned;
        w.pinned
    }

    /// Move focus one window left/right (no wrap), keeping focus on a real
    /// window. Scroll follows in [`crate::layout`]; this only moves the index.
    pub fn focus_left(&mut self) {
        self.focus = self.focus.saturating_sub(1);
    }

    pub fn focus_right(&mut self) {
        self.focus = (self.focus + 1).min(self.windows.len() - 1);
    }

    /// Non-destructive zoom toggle: remember/restore the scroll so the strip
    /// snaps back to where it was. Zoom follows focus (the renderer reads the
    /// focused window), so there's no captured index to go stale.
    pub fn toggle_zoom(&mut self) {
        if self.zoomed {
            self.zoomed = false;
            self.scroll_x = self.pre_zoom_scroll;
        } else {
            self.zoomed = true;
            self.pre_zoom_scroll = self.scroll_x;
        }
    }
}

// ── Dependency-tree construction (lifted verbatim from the v2 App lens) ───────

/// Build the flattened upstream/downstream forest for `root`, honouring the
/// per-root `collapsed` set. A faithful port of the v2 `App::build_forest` /
/// `walk`, now a free function so every Deps window can build its own.
fn build_forest(
    graph: &Graph,
    root: &str,
    dir: Direction,
    collapsed: &HashSet<String>,
) -> Vec<TreeRow> {
    let tag = match dir {
        Direction::Upstream => "U",
        Direction::Downstream => "D",
    };
    let mut rows = Vec::new();
    let mut drawn = HashSet::new();
    let mut path = HashSet::new();
    path.insert(root.to_string());

    let children = graph.neighbours(root, dir);
    for (i, child) in children.iter().enumerate() {
        walk(
            graph,
            child,
            dir,
            tag,
            &mut Vec::new(),
            i + 1 == children.len(),
            collapsed,
            &mut path,
            &mut drawn,
            &mut rows,
        );
    }
    rows
}

#[allow(clippy::too_many_arguments)]
fn walk(
    graph: &Graph,
    key: &str,
    dir: Direction,
    tag: &str,
    ancestors: &mut Vec<bool>,
    is_last: bool,
    collapsed: &HashSet<String>,
    path: &mut HashSet<String>,
    drawn: &mut HashSet<String>,
    rows: &mut Vec<TreeRow>,
) {
    let mut prefix = String::new();
    for &last in ancestors.iter() {
        prefix.push_str(if last { "   " } else { "│  " });
    }
    prefix.push_str(if is_last { "└─ " } else { "├─ " });

    // Classify before deciding whether to recurse.
    let kind = if path.contains(key) {
        NodeKind::Cycle
    } else if drawn.contains(key) {
        NodeKind::Ref
    } else if graph.get(key).is_some_and(|i| i.external) {
        NodeKind::External
    } else {
        NodeKind::Normal
    };

    let children = graph.neighbours(key, dir);
    let has_children = kind == NodeKind::Normal && !children.is_empty();
    let collapsed_here = has_children && collapsed.contains(&format!("{tag}:{key}"));

    rows.push(TreeRow {
        key: key.to_string(),
        prefix,
        kind,
        has_children,
        collapsed: collapsed_here,
    });

    if kind != NodeKind::Normal {
        return; // Cycle / Ref / External are terminal
    }
    drawn.insert(key.to_string());
    if !has_children || collapsed_here {
        return;
    }

    path.insert(key.to_string());
    ancestors.push(is_last);
    for (i, child) in children.iter().enumerate() {
        walk(
            graph,
            child,
            dir,
            tag,
            ancestors,
            i + 1 == children.len(),
            collapsed,
            path,
            drawn,
            rows,
        );
    }
    ancestors.pop();
    path.remove(key);
}

/// Step a `ListState` selection by `delta`, wrapping; empties select nothing.
fn move_state(state: &mut ListState, len: usize, delta: i32) {
    if len == 0 {
        state.select(None);
        return;
    }
    let cur = state.selected().unwrap_or(0) as i32;
    let next = (cur + delta).rem_euclid(len as i32) as usize;
    state.select(Some(next));
}

/// Clamp a selection into a (possibly shrunken) list, selecting nothing when
/// it's empty.
fn clamp_selection(state: &mut ListState, len: usize) {
    if len == 0 {
        state.select(None);
    } else {
        let i = state.selected().unwrap_or(0).min(len - 1);
        state.select(Some(i));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo;

    #[test]
    fn a_fresh_window_set_is_just_a_focused_spine() {
        let ws = WindowSet::new();
        assert_eq!(ws.windows.len(), 1);
        assert!(matches!(ws.focused_kind(), WindowKind::Spine));
        assert_eq!(ws.focus, 0);
        assert!(ws.focused().pinned, "the spine is always pinned");
    }

    #[test]
    fn opening_an_agent_twice_focuses_the_same_window() {
        let mut ws = WindowSet::new();
        let (first, _) = ws.open_or_focus_agent("ZAP-204");
        assert_eq!(ws.windows.len(), 2);
        // A second open of the same issue focuses the existing window, no dupe.
        ws.focus = 0;
        let (again, displaced) = ws.open_or_focus_agent("ZAP-204");
        assert_eq!(ws.windows.len(), 2, "no duplicate window for one issue");
        assert_eq!(first, again);
        assert!(
            displaced.is_none(),
            "focusing an existing agent displaces nothing"
        );
        assert_eq!(ws.focus, 1, "focus returns to the existing window");
    }

    #[test]
    fn a_new_agent_preview_displaces_the_old_unpinned_one() {
        let mut ws = WindowSet::new();
        ws.open_or_focus_agent("ZAP-1");
        // Opening a second, different agent replaces the unpinned preview.
        let (_, displaced) = ws.open_or_focus_agent("ZAP-2");
        assert_eq!(ws.windows.len(), 2, "the spine + one preview");
        assert_eq!(
            displaced.and_then(|w| w.kind.agent_issue().map(str::to_string)),
            Some("ZAP-1".to_string()),
            "the old preview is displaced (its agent keeps running)"
        );
        // …but a pinned agent window is never displaced.
        ws.toggle_pin_focused(); // pin ZAP-2
        ws.open_or_focus_agent("ZAP-3");
        assert_eq!(ws.windows.len(), 3, "pinned ZAP-2 survives, ZAP-3 added");
    }

    #[test]
    fn close_undocks_a_window_but_never_the_spine() {
        let mut ws = WindowSet::new();
        ws.open_or_focus_agent("ZAP-204");
        let closed = ws.close_focused().expect("an agent window closes");
        assert_eq!(closed.kind.agent_issue(), Some("ZAP-204"));
        assert_eq!(ws.windows.len(), 1);
        assert_eq!(ws.focus, 0);
        // Closing the spine is a no-op.
        assert!(ws.close_focused().is_none());
        assert_eq!(ws.windows.len(), 1);
    }

    #[test]
    fn deps_cursor_re_roots_and_back_returns() {
        let graph = demo::graph();
        let mut cursor = DepsCursor::new("ZAP-204".into(), &graph);
        assert!(!cursor.up_rows.is_empty(), "ZAP-204 has blockers");
        let target = cursor.up_rows[0].key.clone();
        cursor.enter(&graph).unwrap();
        assert_eq!(cursor.root, target);
        assert!(cursor.back(&graph));
        assert_eq!(cursor.root, "ZAP-204");
        assert!(!cursor.back(&graph), "nothing left to go back to");
    }

    #[test]
    fn deps_cursor_collapse_resets_on_re_root() {
        let graph = demo::graph();
        let mut cursor = DepsCursor::new("ZAP-204".into(), &graph);
        let before = cursor.up_rows.len();
        cursor.toggle_collapse(&graph); // collapse the first subtree
        assert!(cursor.up_rows.len() < before);
        let _ = cursor.enter(&graph); // re-root onto the collapsed node
        let _ = cursor.back(&graph); // back to the original root
        assert_eq!(
            cursor.up_rows.len(),
            before,
            "the lens re-opens fully expanded after re-rooting"
        );
    }

    #[test]
    fn zoom_round_trips_the_scroll_offset() {
        let mut ws = WindowSet::new();
        ws.scroll_x = 3;
        ws.toggle_zoom();
        assert!(ws.zoomed);
        ws.scroll_x = 9; // some scrolling happened while zoomed
        ws.toggle_zoom();
        assert!(!ws.zoomed);
        assert_eq!(ws.scroll_x, 3, "unzoom restores the pre-zoom scroll");
    }
}
