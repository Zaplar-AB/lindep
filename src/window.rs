//! The cockpit's window model — the sole owner of every window type.
//!
//! Cockpit v3.2 is a tmux-style tiling window manager whose panes ("windows") are
//! live, focusable columns: the permanent **Spine** (issue list / agents roster),
//! N **Coin** windows, and the single chatless **Fleet** overview. A *coin* is one
//! issue with two faces — its live `claude` screen (chat) and its dependency tree
//! (deps) — flipped by `Tab`; the single *unpinned* coin is the transient preview
//! that follows the Spine selection, and pinning it graduates it to a permanent
//! docked tab **in place** (its PTY + [`WindowId`] survive). Every other module
//! consumes the types defined here rather than minting its own.
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

/// Which face of a [`WindowKind::Coin`] is showing: the issue's live agent screen
/// (chat), or its dependency tree (deps). The two sides of one coin, flipped by
/// `Tab`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoinMode {
    /// The issue's live `claude` screen (the "chat-first" default when it has a
    /// live-or-imminent agent).
    Chat,
    /// The issue's upstream/downstream dependency tree (the resting default for an
    /// issue with no live agent).
    Deps,
}

impl CoinMode {
    /// The other face — for the `Tab` flip.
    pub const fn toggled(self) -> Self {
        match self {
            CoinMode::Chat => CoinMode::Deps,
            CoinMode::Deps => CoinMode::Chat,
        }
    }
}

/// The kind of a window. The Spine is permanent and always sits at index 0. A
/// **Coin** is one issue with two faces — its live agent screen (`Chat`) and its
/// dependency tree (`Deps`), flipped by `Tab`; the single *unpinned* coin is the
/// transient preview at index 1 that follows the Spine selection, and pinning it
/// (graduation) makes it a permanent docked tab in place. **Fleet** is the single
/// chatless project-overview tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowKind {
    /// The navigation spine: the issue list or the agents roster (the `r` tab
    /// toggle lives on it). Permanent — never closed, always window 0.
    Spine,
    /// One issue, two faces (`Chat` / `Deps`), flipped by `Tab`. The unpinned coin
    /// is the preview (one, follows the selection); a pinned coin is a docked tab.
    /// It carries a live PTY (in `App.backends`, keyed by issue) on its chat face
    /// and a [`DepsCursor`] (in `Window.deps`) on its deps face.
    Coin { issue: String, mode: CoinMode },
    /// The layered, edge-free project-wide overview (the old graph map). Chatless;
    /// always at most one.
    Fleet,
}

impl WindowKind {
    /// The issue whose live agent screen this window renders, if it currently
    /// shows one — a `Coin` in `Chat` mode. A deps-face coin (no PTY on screen)
    /// returns `None`. Used by the idle-quiet visibility plumbing.
    pub fn agent_issue(&self) -> Option<&str> {
        match self {
            WindowKind::Coin {
                issue,
                mode: CoinMode::Chat,
            } => Some(issue.as_str()),
            _ => None,
        }
    }

    /// The `(issue, mode)` of a coin, if this is one (pinned or the preview).
    pub fn coin(&self) -> Option<(&str, CoinMode)> {
        match self {
            WindowKind::Coin { issue, mode } => Some((issue.as_str(), *mode)),
            _ => None,
        }
    }
}

/// How the strip arranges its windows. Chosen automatically from the docked-coin
/// count (see [`WindowSet::auto_layout`]) unless the user forces one with
/// `Ctrl-a |`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    /// Spine pinned left, one **big pane** (the focused window, or the preview
    /// when the Spine is focused), and a thin right-hand **rail** of compact
    /// status cards for every other docked window. Only the big pane hosts a live
    /// PTY — cards are text, so the rail sidesteps tui-term's horizontal-clipping
    /// limit and preserves the idle-quiet property. The "Open Editors overflow"
    /// layout, used once more than [`MOSAIC_MAX`] coins are docked.
    Rail,
    /// Spine pinned left, every coin tiled near-square in the rest (reuses
    /// `split_grid`). The full-attention layout: every pane is live at once. Used
    /// while the docked coins still fit (≤ [`MOSAIC_MAX`]).
    Mosaic,
}

impl LayoutMode {
    /// The other layout — for the `|` toggle.
    pub const fn toggled(self) -> Self {
        match self {
            LayoutMode::Rail => LayoutMode::Mosaic,
            LayoutMode::Mosaic => LayoutMode::Rail,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            LayoutMode::Rail => "rail",
            LayoutMode::Mosaic => "mosaic",
        }
    }
}

/// How many docked (pinned, non-Spine) windows tile before the rail appears. The
/// preview doesn't count toward this — so up to `MOSAIC_MAX` pinned coins plus the
/// preview tile, and the `MOSAIC_MAX + 1`-th docked coin flips to the rail (which
/// also caps live PTYs to the focused one). Tunable; "more than ~4 chats" per the
/// v3.2 design.
pub const MOSAIC_MAX: usize = 4;

/// Which dependency pane a [`DepsCursor`] currently drives. The v2 lens cycled
/// List → Upstream → Downstream; the List is now the Spine, so a coin's deps face
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

/// The independent navigation state of one coin's deps face: its live root, its
/// back-history, the flattened upstream/downstream trees, the per-tree selection,
/// the per-root collapse set, and which tree is active. Each coin keeps v2's
/// re-root / Back / collapse gestures independently of every other window.
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
    /// Pinned windows persist (docked, in pin order) and are restored on restart.
    /// The Spine is always pinned; the preview coin is never pinned (pinning it
    /// *graduates* it). A coin becomes pinned the moment it graduates from the
    /// preview; the docked-coin count drives the layout.
    pub pinned: bool,
    /// Per-window dependency navigation — `Some` for a coin whose deps face has
    /// been built (Deps mode, or a dormant cursor kept across a flip to chat so a
    /// re-root survives), `None` otherwise.
    pub deps: Option<DepsCursor>,
}

impl Window {
    /// The issue a coin is about — its *identity* (`None` for the Spine / Fleet).
    /// This is the coin's identity, not its deps cursor's current root (which
    /// re-rooting moves for exploration); persistence and merge key off this.
    pub fn issue(&self) -> Option<&str> {
        match &self.kind {
            WindowKind::Coin { issue, .. } => Some(issue.as_str()),
            _ => None,
        }
    }

    /// Whether this is the single transient preview — an *unpinned* coin.
    pub fn is_preview(&self) -> bool {
        matches!(self.kind, WindowKind::Coin { .. }) && !self.pinned
    }
}

/// The result of pinning (graduating) the preview coin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraduateOutcome {
    /// The preview became a new permanent coin (keeping its id).
    Graduated(WindowId),
    /// A pinned coin of the same identity already existed; the preview was dropped
    /// and that coin focused instead (a *merge*).
    Merged(WindowId),
    /// The focused window wasn't the preview — nothing graduated.
    NotPreview,
}

/// The ordered strip of windows plus all view-wide window state: which one is
/// focused, the layout mode, and the zoom toggle. The single source of truth for
/// "what's on screen" — `windows[0]` is always the Spine and `windows[1]` (when
/// present) is always the single transient preview coin.
#[derive(Debug, Clone)]
pub struct WindowSet {
    /// All columns in display order. `windows[0]` is always the Spine; the preview
    /// coin, when present, is always `windows[1]`.
    ///
    /// INVARIANT: every window's `id` is minted by [`Self::next_window_id`] and
    /// never reused — preview_size keys depend on it. Read this field freely, but
    /// add windows only through the typed mutators (`ensure_preview` / `push` /
    /// `open_fleet`); never `windows.push(Window { id, .. })` with a hand-rolled id.
    pub windows: Vec<Window>,
    /// Index into `windows` of the focused column.
    pub focus: usize,
    /// Source of the next [`WindowId`]. Monotonic; never reused.
    next_id: u64,
    /// The *effective* layout, recomputed from the docked-coin count on every
    /// structural change (see [`Self::refresh_layout`]) — unless the user forced a
    /// mode with `Ctrl-a |`, which sets `layout_manual`.
    pub layout: LayoutMode,
    /// `true` once the user forced the layout with `Ctrl-a |`; suppresses the
    /// count-driven auto-recompute for the rest of the session.
    pub layout_manual: bool,
    /// `true` while the big pane is zoomed to fill the whole viewport (hiding the
    /// Spine and the rail).
    pub zoomed: bool,
}

impl Default for WindowSet {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowSet {
    /// A fresh set: just the permanent Spine, focused. The caller seeds the
    /// transient preview coin at index 1 (via [`Self::ensure_preview`]) once it
    /// knows the default selection.
    pub fn new() -> Self {
        let mut set = WindowSet {
            windows: vec![Window {
                id: WindowId(0),
                kind: WindowKind::Spine,
                pinned: true,
                deps: None,
            }],
            focus: 0,
            next_id: 1,
            layout: LayoutMode::Mosaic,
            layout_manual: false,
            zoomed: false,
        };
        set.refresh_layout();
        set
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

    /// Whether any window keeps `issue`'s backend alive — a chat-face coin for it,
    /// or **any pinned coin** for it (a pinned coin can flip back to its chat face,
    /// so its agent must survive even while its deps face is showing). Used by the
    /// backend-reclaim predicate (generalising v2's `attached`).
    pub fn references_agent(&self, issue: &str) -> bool {
        self.windows.iter().any(|w| match &w.kind {
            WindowKind::Coin { issue: i, mode } => {
                i.as_str() == issue && (w.pinned || *mode == CoinMode::Chat)
            }
            _ => false,
        })
    }

    /// Index of the single transient preview coin (always at index 1 when present
    /// — the Spine is index 0).
    pub fn preview_index(&self) -> Option<usize> {
        self.windows.iter().position(Window::is_preview)
    }

    /// Index of a *pinned* coin for `issue`, if one exists (either face).
    pub fn pinned_coin_index(&self, issue: &str) -> Option<usize> {
        self.windows.iter().position(|w| {
            w.pinned && matches!(&w.kind, WindowKind::Coin { issue: i, .. } if i.as_str() == issue)
        })
    }

    /// Whether a *pinned* coin already exists for `issue` — so the preview's
    /// chat-first default can fall back to Deps (no point previewing a coin that's
    /// already a permanent tab).
    pub fn has_pinned_coin(&self, issue: &str) -> bool {
        self.pinned_coin_index(issue).is_some()
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
        self.refresh_layout();
        Some(removed)
    }

    /// Create-or-re-aim the single transient preview coin at index 1 to mirror
    /// `issue` in `mode`. The Spine selection drives this every time it moves; it
    /// never steals focus (an insert bumps an existing focus index so it keeps
    /// pointing at the same window). A `Deps`-mode preview carries a freshly built
    /// [`DepsCursor`] rooted at `issue`; a `Chat`-mode one carries none (a flip to
    /// Deps builds it on demand). Returns the preview's id — stable across re-aims,
    /// so its `preview_size` / live PTY survive.
    pub fn ensure_preview(&mut self, issue: &str, mode: CoinMode, graph: &Graph) -> WindowId {
        let deps = (mode == CoinMode::Deps).then(|| DepsCursor::new(issue.to_string(), graph));
        if let Some(i) = self.preview_index() {
            self.windows[i].kind = WindowKind::Coin {
                issue: issue.to_string(),
                mode,
            };
            self.windows[i].deps = deps;
            return self.windows[i].id;
        }
        let id = self.next_window_id();
        self.windows.insert(
            1,
            Window {
                id,
                kind: WindowKind::Coin {
                    issue: issue.to_string(),
                    mode,
                },
                pinned: false,
                deps,
            },
        );
        // Inserting ahead of the focus keeps it on the same window.
        if self.focus >= 1 {
            self.focus += 1;
        }
        self.refresh_layout();
        id
    }

    /// The preview coin's current `(issue, mode)`, if it exists.
    pub fn preview(&self) -> Option<(String, CoinMode)> {
        self.preview_index().and_then(|i| {
            self.windows[i]
                .kind
                .coin()
                .map(|(issue, mode)| (issue.to_string(), mode))
        })
    }

    /// Focus the preview coin (used by the attach/spawn button).
    pub fn focus_preview(&mut self) {
        if let Some(i) = self.preview_index() {
            self.focus = i;
        }
    }

    /// Drop the transient preview coin if present — e.g. the selection is now a
    /// pinned coin, which *is* the active view, so a duplicate preview would just
    /// clutter. Returns it so the caller can reclaim a dead backend. Focus is fixed
    /// like any removal (and is never the preview here — the caller guards that).
    pub fn clear_preview(&mut self) -> Option<Window> {
        let i = self.preview_index()?;
        self.remove(i)
    }

    /// Flip the coin at `idx` between its chat and deps faces (`Tab`), building its
    /// deps cursor on demand. A dormant cursor is kept across a flip to chat, so a
    /// pinned coin's re-root exploration survives. No-op if `idx` isn't a coin.
    pub fn flip_coin_face(&mut self, idx: usize, graph: &Graph) {
        let Some((issue, mode)) = self
            .windows
            .get(idx)
            .and_then(|w| w.kind.coin())
            .map(|(i, m)| (i.to_string(), m))
        else {
            return;
        };
        let next = mode.toggled();
        if next == CoinMode::Deps && self.windows[idx].deps.is_none() {
            self.windows[idx].deps = Some(DepsCursor::new(issue.clone(), graph));
        }
        self.windows[idx].kind = WindowKind::Coin { issue, mode: next };
    }

    /// Pin = **graduate** the focused preview coin into a permanent docked coin. If
    /// a pinned coin of the same identity already exists, the redundant preview is
    /// dropped and that coin focused (a *merge*) — so one issue is never split
    /// across two pinned coins. Otherwise the preview is pinned **in place**,
    /// keeping its [`WindowId`] (so a live PTY / `preview_size` / deps cursor
    /// survive untouched). The caller re-seeds a fresh preview via
    /// [`Self::ensure_preview`] afterwards.
    pub fn pin_preview(&mut self) -> GraduateOutcome {
        let Some(p) = self.preview_index() else {
            return GraduateOutcome::NotPreview;
        };
        if self.focus != p {
            return GraduateOutcome::NotPreview;
        }
        let Some(issue) = self.windows[p].issue().map(str::to_string) else {
            return GraduateOutcome::NotPreview;
        };
        if let Some(t) = self.pinned_coin_index(&issue) {
            // Already pinned: drop the redundant preview, focus the existing coin.
            let twin_id = self.windows[t].id;
            self.windows.remove(p);
            self.focus = if p < t { t - 1 } else { t };
            self.refresh_layout();
            return GraduateOutcome::Merged(twin_id);
        }
        // Pin in place, preserving the WindowId (and any live PTY / deps cursor).
        self.windows[p].pinned = true;
        let id = self.windows[p].id;
        self.refresh_layout();
        GraduateOutcome::Graduated(id)
    }

    /// Open (or focus) the single Fleet overview window — there's only ever one.
    pub fn open_fleet(&mut self) -> WindowId {
        if let Some(i) = self
            .windows
            .iter()
            .position(|w| matches!(w.kind, WindowKind::Fleet))
        {
            self.focus = i;
            return self.windows[i].id;
        }
        let id = self.next_window_id();
        self.windows.push(Window {
            id,
            kind: WindowKind::Fleet,
            pinned: true,
            deps: None,
        });
        self.focus = self.windows.len() - 1;
        self.refresh_layout();
        id
    }

    /// Close the focused window (undock). The Spine and the preview coin can't be
    /// closed (both are structural) — the caller guards those. Focus falls back to
    /// the previous window. Returns the closed window so the caller can reclaim its
    /// backend handle / drop its geometry.
    pub fn close_focused(&mut self) -> Option<Window> {
        let removed = self.remove(self.focus)?;
        self.zoomed = false;
        Some(removed)
    }

    /// Append a window with the next id, focusing it. Used by persistence restore
    /// and the auto-resume placeholders, where the kind/pin state are already
    /// decided by the caller. `pinned` is honoured as given.
    pub fn push(&mut self, kind: WindowKind, pinned: bool, deps: Option<DepsCursor>) -> WindowId {
        let id = self.next_window_id();
        self.windows.push(Window {
            id,
            kind,
            pinned,
            deps,
        });
        self.focus = self.windows.len() - 1;
        self.refresh_layout();
        id
    }

    /// Move focus one window left/right (no wrap). The focused non-spine window is
    /// always the big pane, so there's nothing to scroll into view.
    pub fn focus_left(&mut self) {
        self.focus = self.focus.saturating_sub(1);
    }

    pub fn focus_right(&mut self) {
        self.focus = (self.focus + 1).min(self.windows.len() - 1);
    }

    /// Jump focus straight home to the Spine in one hop — the dedicated
    /// "back to nav" gesture (so you never step through the deps pane to return).
    pub fn focus_nav(&mut self) {
        self.focus = 0;
    }

    /// Non-destructive zoom toggle: the big pane fills the whole viewport. Zoom
    /// follows the big pane (the renderer reads it), so there's no captured index
    /// to go stale.
    pub fn toggle_zoom(&mut self) {
        self.zoomed = !self.zoomed;
    }

    // ── Count-driven layout ─────────────────────────────────────────────────

    /// Non-Spine *pinned* windows — the docked coins + Fleet. The preview is
    /// unpinned, so it never counts toward the rail threshold.
    pub fn docked_count(&self) -> usize {
        self.windows
            .iter()
            .filter(|w| w.pinned && !matches!(w.kind, WindowKind::Spine))
            .count()
    }

    /// The count-driven layout: tiled (mosaic) while the docked coins fit, rail
    /// beyond [`MOSAIC_MAX`].
    pub fn auto_layout(docked: usize) -> LayoutMode {
        if docked > MOSAIC_MAX {
            LayoutMode::Rail
        } else {
            LayoutMode::Mosaic
        }
    }

    /// Recompute the effective layout from the docked count, unless the user
    /// pinned a choice with `Ctrl-a |`. Called after every structural change.
    fn refresh_layout(&mut self) {
        if self.layout_manual {
            return;
        }
        self.layout = Self::auto_layout(self.docked_count());
    }

    /// Force a layout mode (the `Ctrl-a |` override), suppressing auto-recompute
    /// for the rest of the session.
    pub fn force_layout(&mut self, layout: LayoutMode) {
        self.layout = layout;
        self.layout_manual = true;
    }
}

// ── Dependency-tree construction (lifted verbatim from the v2 App lens) ───────

/// Build the flattened upstream/downstream forest for `root`, honouring the
/// per-root `collapsed` set. A faithful port of the v2 `App::build_forest` /
/// `walk`, now a free function so every coin can build its own.
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
    fn ensure_preview_creates_one_coin_at_index_1_and_reaims_in_place() {
        let graph = demo::graph();
        let mut ws = WindowSet::new();
        let id = ws.ensure_preview("ZAP-204", CoinMode::Deps, &graph);
        assert_eq!(ws.windows.len(), 2, "spine + the single preview coin");
        assert_eq!(ws.preview_index(), Some(1), "the preview lives at index 1");
        assert_eq!(
            ws.focus, 0,
            "creating the preview never steals focus from the spine"
        );
        // Re-aiming mutates the SAME window (no accumulation) and keeps its id,
        // so a live PTY / preview_size survive across nav moves.
        let again = ws.ensure_preview("ZAP-210", CoinMode::Deps, &graph);
        assert_eq!(again, id, "the preview keeps its WindowId across re-aims");
        assert_eq!(ws.windows.len(), 2, "still exactly one preview coin");
        assert_eq!(ws.preview().unwrap().0, "ZAP-210");
    }

    #[test]
    fn flipping_a_coin_toggles_chat_and_deps() {
        let graph = demo::graph();
        let mut ws = WindowSet::new();
        ws.ensure_preview("ZAP-204", CoinMode::Chat, &graph);
        let p = ws.preview_index().unwrap();
        assert_eq!(ws.preview().unwrap().1, CoinMode::Chat);
        assert!(
            ws.windows[p].deps.is_none(),
            "a chat-face coin carries no cursor"
        );
        ws.flip_coin_face(p, &graph);
        assert_eq!(ws.preview().unwrap().1, CoinMode::Deps);
        assert!(
            ws.windows[p].deps.is_some(),
            "flipping to deps builds the cursor on demand"
        );
    }

    #[test]
    fn pin_graduates_the_preview_to_a_permanent_coin_keeping_its_id() {
        let graph = demo::graph();
        let mut ws = WindowSet::new();
        let id = ws.ensure_preview("ZAP-204", CoinMode::Chat, &graph);
        ws.focus_preview();
        let outcome = ws.pin_preview();
        assert_eq!(
            outcome,
            GraduateOutcome::Graduated(id),
            "graduation preserves the WindowId (so a live PTY survives)"
        );
        let g = &ws.windows[ws.focus];
        assert!(g.pinned, "the graduated coin is pinned");
        assert_eq!(g.kind.agent_issue(), Some("ZAP-204"));
        assert!(
            ws.preview_index().is_none(),
            "the preview is consumed by graduation (the caller re-seeds one)"
        );
    }

    #[test]
    fn pin_merges_when_a_coin_of_the_same_identity_already_exists() {
        let graph = demo::graph();
        let mut ws = WindowSet::new();
        let first = ws.ensure_preview("ZAP-204", CoinMode::Chat, &graph);
        ws.focus_preview();
        assert_eq!(ws.pin_preview(), GraduateOutcome::Graduated(first));
        // A fresh preview aimed at the same issue, pinned again → merge, no dupe.
        ws.ensure_preview("ZAP-204", CoinMode::Chat, &graph);
        ws.focus_preview();
        assert_eq!(
            ws.pin_preview(),
            GraduateOutcome::Merged(first),
            "the redundant preview merges into the existing coin"
        );
        assert_eq!(
            ws.windows
                .iter()
                .filter(|w| w.issue() == Some("ZAP-204"))
                .count(),
            1,
            "one issue is never split across two pinned coins"
        );
    }

    #[test]
    fn close_undocks_a_graduated_coin_but_never_the_spine() {
        let graph = demo::graph();
        let mut ws = WindowSet::new();
        ws.ensure_preview("ZAP-204", CoinMode::Chat, &graph);
        ws.focus_preview();
        ws.pin_preview(); // a pinned coin at index 1, focused
        let closed = ws.close_focused().expect("a graduated coin closes");
        assert_eq!(closed.kind.agent_issue(), Some("ZAP-204"));
        assert_eq!(ws.focus, 0);
        // Closing the spine is a no-op.
        assert!(ws.close_focused().is_none());
        assert!(matches!(ws.windows[0].kind, WindowKind::Spine));
    }

    #[test]
    fn layout_auto_switches_to_rail_past_the_threshold() {
        let mut ws = WindowSet::new();
        assert_eq!(ws.layout, LayoutMode::Mosaic, "few coins tile");
        // Dock MOSAIC_MAX + 1 coins → past the threshold, the rail appears.
        for i in 0..=MOSAIC_MAX {
            ws.push(
                WindowKind::Coin {
                    issue: format!("Z-{i}"),
                    mode: CoinMode::Chat,
                },
                true,
                None,
            );
        }
        assert_eq!(
            ws.layout,
            LayoutMode::Rail,
            "more than MOSAIC_MAX docked coins → rail"
        );
        let _ = ws.close_focused();
        assert_eq!(
            ws.layout,
            LayoutMode::Mosaic,
            "dropping back under the threshold re-tiles"
        );
    }

    #[test]
    fn a_forced_layout_survives_a_structural_change() {
        let mut ws = WindowSet::new();
        ws.force_layout(LayoutMode::Rail);
        assert_eq!(ws.layout, LayoutMode::Rail);
        // A structural change must NOT auto-recompute over a manual choice.
        ws.push(
            WindowKind::Coin {
                issue: "Z-1".into(),
                mode: CoinMode::Chat,
            },
            true,
            None,
        );
        assert_eq!(ws.layout, LayoutMode::Rail, "the manual override sticks");
    }

    #[test]
    fn a_pinned_coin_keeps_its_backend_referenced_across_a_face_flip() {
        let graph = demo::graph();
        let mut ws = WindowSet::new();
        ws.ensure_preview("ZAP-204", CoinMode::Chat, &graph);
        ws.focus_preview();
        ws.pin_preview(); // a pinned chat coin
        assert!(ws.references_agent("ZAP-204"));
        // Flip it to its deps face — the agent must still be referenced, since the
        // coin can flip back to chat.
        let f = ws.focus;
        ws.flip_coin_face(f, &graph);
        assert!(
            ws.references_agent("ZAP-204"),
            "a pinned coin keeps its backend alive on its deps face"
        );
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
    fn zoom_toggles() {
        let mut ws = WindowSet::new();
        assert!(!ws.zoomed);
        ws.toggle_zoom();
        assert!(ws.zoomed);
        ws.toggle_zoom();
        assert!(!ws.zoomed);
    }
}
