//! Interactive application state and input handling. Holds the graph, the
//! currently focused issue ("root" of the lens), the flattened upstream and
//! downstream trees, and all view state (search, filter, sort, mode). Rendering
//! lives in [`crate::ui`]; this module never touches the terminal.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::widgets::ListState;

use crate::backend::{self, AgentBackend, Lifecycle};
use crate::event::AppEvent;
use crate::keymap::{Action, Keymap};
use crate::model::{Direction, Graph, Issue};
use crate::session::AgentStatus;
use crate::supervisor::SupervisorHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    List,
    Upstream,
    Downstream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Lens,
    Overview,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    Blocked,
    HasDeps,
}

impl Filter {
    const fn next(self) -> Self {
        match self {
            Filter::All => Filter::Blocked,
            Filter::Blocked => Filter::HasDeps,
            Filter::HasDeps => Filter::All,
        }
    }
    pub const fn label(self) -> &'static str {
        match self {
            Filter::All => "all",
            Filter::Blocked => "blocked",
            Filter::HasDeps => "has-deps",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    /// Ready-to-start work first: unblocked issues on top, highest downstream
    /// impact within each group.
    Ready,
    Blocked,
    Status,
    Priority,
    Key,
}

impl Sort {
    const fn next(self) -> Self {
        match self {
            Sort::Ready => Sort::Blocked,
            Sort::Blocked => Sort::Status,
            Sort::Status => Sort::Priority,
            Sort::Priority => Sort::Key,
            Sort::Key => Sort::Ready,
        }
    }
    pub const fn label(self) -> &'static str {
        match self {
            Sort::Ready => "ready",
            Sort::Blocked => "blocked",
            Sort::Status => "status",
            Sort::Priority => "priority",
            Sort::Key => "id",
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
pub struct TreeRow {
    pub key: String,
    pub prefix: String,
    pub kind: NodeKind,
    pub has_children: bool,
    pub collapsed: bool,
}

pub struct App {
    pub graph: Graph,
    pub order: Vec<String>,
    pub list_state: ListState,

    pub root: String,
    history: Vec<String>,

    pub up_rows: Vec<TreeRow>,
    pub down_rows: Vec<TreeRow>,
    pub up_state: ListState,
    pub down_state: ListState,
    collapsed: HashSet<String>, // "U:KEY" / "D:KEY"
    collapsed_for: String,      // the root `collapsed` belongs to; reset on re-root

    pub focus: Pane,
    pub mode: Mode,
    pub filter: Filter,
    pub sort: Sort,
    pub search_query: String,
    pub search_active: bool,
    pub show_help: bool,
    pub status_msg: Option<String>,
    /// True while `status_msg` holds an unacknowledged "needs you" alert. Routine
    /// high-frequency tool chatter (`AgentAction`) must not bury it; it clears the
    /// moment the human touches a dashboard key (acknowledging) or a deliberate
    /// event replaces the footer.
    needs_you_alert: bool,
    /// Issues with a launch command in flight (sent to the supervisor, not yet
    /// acknowledged by an `AgentSpawned` or rejected by a `Notification`). Lets
    /// the cockpit refuse a double-press before the fleet entry materializes.
    pending_launch: HashSet<String>,

    /// Per-issue agent status, driven by the supervisor + notification bus.
    /// Absence of an entry means "no agent" — the fleet view's resting state.
    pub fleet: HashMap<String, AgentStatus>,
    /// Backend handles for agents we launched, keyed by issue. Used to render
    /// and drive an agent's PTY when attached.
    pub backends: HashMap<String, Arc<dyn AgentBackend>>,
    /// The issue whose PTY the cockpit is currently attached to (`None` = the
    /// dashboard). While attached, all input is forwarded to that agent.
    pub attached: Option<String>,
    /// Size (rows, cols) the attached agent was last resized to, so we only
    /// push a resize when the pane actually changes.
    pub attached_size: Option<(u16, u16)>,
    /// While attached with a leader-sequence detach key, the leader chord that's
    /// been pressed and is awaiting its completion (e.g. `Ctrl-A`, waiting for
    /// `d`). `None` the rest of the time.
    pub pending_leader: Option<KeyEvent>,
    /// Handle to the agent supervisor, when the cockpit is running with one
    /// (absent in `--demo`, snapshots and unit tests).
    pub supervisor: Option<SupervisorHandle>,
    /// Active key bindings (defaults, overridden by `config.toml`).
    pub keymap: Keymap,

    pub should_quit: bool,
}

impl App {
    pub fn new(graph: Graph) -> Self {
        // Default focus: the most-connected real issue — usually the spine of
        // the dependency web — so the lens opens somewhere interesting.
        let root = graph
            .keys()
            .iter()
            .filter(|k| graph.get(k).is_some_and(|i| !i.external))
            .max_by_key(|k| {
                graph.direct_count(k, Direction::Upstream)
                    + graph.direct_count(k, Direction::Downstream)
            })
            .cloned()
            .unwrap_or_default();

        let mut app = App {
            graph,
            order: Vec::new(),
            list_state: ListState::default(),
            root,
            history: Vec::new(),
            up_rows: Vec::new(),
            down_rows: Vec::new(),
            up_state: ListState::default(),
            down_state: ListState::default(),
            collapsed: HashSet::new(),
            collapsed_for: String::new(),
            focus: Pane::List,
            mode: Mode::Lens,
            filter: Filter::All,
            sort: Sort::Ready,
            search_query: String::new(),
            search_active: false,
            show_help: false,
            status_msg: None,
            needs_you_alert: false,
            pending_launch: HashSet::new(),
            fleet: HashMap::new(),
            backends: HashMap::new(),
            attached: None,
            attached_size: None,
            pending_leader: None,
            supervisor: None,
            keymap: Keymap::default(),
            should_quit: false,
        };
        app.rebuild_order();
        app.rebuild_trees();
        app
    }

    pub fn focused_issue(&self) -> Option<&Issue> {
        self.graph.get(&self.root)
    }

    // ── Derived list ordering ────────────────────────────────────────────────

    fn rebuild_order(&mut self) {
        let needle = self.search_query.to_lowercase();
        let g = &self.graph;
        let (filter, sort) = (self.filter, self.sort);

        // Decorate each surviving key with its sort key once (so transitive()
        // isn't recomputed O(log n) times inside the comparator), then sort.
        let mut decorated: Vec<((u8, u64), String)> = g
            .keys()
            .iter()
            .filter_map(|k| {
                let issue = g.get(k)?;
                if issue.external {
                    return None; // externals show in trees, not the project list
                }
                let pass_filter = match filter {
                    Filter::All => true,
                    Filter::Blocked => g.is_blocked(k),
                    Filter::HasDeps => {
                        g.direct_count(k, Direction::Upstream) > 0
                            || g.direct_count(k, Direction::Downstream) > 0
                    }
                };
                let pass_search = needle.is_empty()
                    || issue.key.to_lowercase().contains(&needle)
                    || issue.title.to_lowercase().contains(&needle);
                (pass_filter && pass_search).then(|| (sort_key(g, k, sort), k.clone()))
            })
            .collect();

        decorated.sort_by(|(ka, a), (kb, b)| ka.cmp(kb).then_with(|| natural_key_cmp(a, b)));
        self.order = decorated.into_iter().map(|(_, k)| k).collect();
        // If the active filter/search hid the current root, re-aim the lens at the
        // first visible issue. Otherwise the left list would highlight order[0]
        // while the right-hand lens kept describing the now-invisible root, and
        // the first Down would skip order[0]. An empty list keeps the last root.
        if !self.order.is_empty() && !self.order.contains(&self.root) {
            self.root = self.order[0].clone();
            self.rebuild_trees();
        }
        self.sync_list_selection();
    }

    fn sync_list_selection(&mut self) {
        if self.order.is_empty() {
            self.list_state.select(None);
        } else if let Some(i) = self.order.iter().position(|k| *k == self.root) {
            self.list_state.select(Some(i));
        } else {
            self.list_state.select(Some(0));
        }
    }

    // ── Tree (lens) construction ───────────────────────────────────────────

    fn rebuild_trees(&mut self) {
        // Collapse state is per-root, so each issue opens fully expanded. Re-rooting
        // changes `root` and clears it here; a collapse toggle leaves `root`
        // unchanged (so its state survives its own rebuild).
        if self.collapsed_for != self.root {
            self.collapsed.clear();
            self.collapsed_for = self.root.clone();
        }
        self.up_rows = self.build_forest(Direction::Upstream);
        self.down_rows = self.build_forest(Direction::Downstream);
        clamp_selection(&mut self.up_state, self.up_rows.len());
        clamp_selection(&mut self.down_state, self.down_rows.len());
    }

    fn build_forest(&self, dir: Direction) -> Vec<TreeRow> {
        let tag = match dir {
            Direction::Upstream => "U",
            Direction::Downstream => "D",
        };
        let mut rows = Vec::new();
        let mut drawn = HashSet::new();
        let mut path = HashSet::new();
        path.insert(self.root.clone());

        let children = self.graph.neighbours(&self.root, dir);
        for (i, child) in children.iter().enumerate() {
            self.walk(
                child,
                dir,
                tag,
                &mut Vec::new(),
                i + 1 == children.len(),
                &mut path,
                &mut drawn,
                &mut rows,
            );
        }
        rows
    }

    #[allow(clippy::too_many_arguments)]
    fn walk(
        &self,
        key: &str,
        dir: Direction,
        tag: &str,
        ancestors: &mut Vec<bool>,
        is_last: bool,
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
        } else if self.graph.get(key).is_some_and(|i| i.external) {
            NodeKind::External
        } else {
            NodeKind::Normal
        };

        let children = self.graph.neighbours(key, dir);
        let has_children = kind == NodeKind::Normal && !children.is_empty();
        let collapsed = has_children && self.collapsed.contains(&format!("{tag}:{key}"));

        rows.push(TreeRow {
            key: key.to_string(),
            prefix,
            kind,
            has_children,
            collapsed,
        });

        if kind != NodeKind::Normal {
            return; // Cycle / Ref / External are terminal
        }
        drawn.insert(key.to_string());
        if !has_children || collapsed {
            return;
        }

        path.insert(key.to_string());
        ancestors.push(is_last);
        for (i, child) in children.iter().enumerate() {
            self.walk(
                child,
                dir,
                tag,
                ancestors,
                i + 1 == children.len(),
                path,
                drawn,
                rows,
            );
        }
        ancestors.pop();
        path.remove(key);
    }

    // ── Focus / navigation ───────────────────────────────────────────────────

    fn set_root(&mut self, key: String, push_history: bool) {
        if key.is_empty() || key == self.root {
            return;
        }
        if push_history {
            self.history.push(self.root.clone());
        }
        self.root = key;
        self.sync_list_selection();
        self.rebuild_trees();
    }

    fn move_selection(&mut self, delta: i32) {
        // In the overview the only visible cursor is the highlighted root chip,
        // so arrows always drive the list there regardless of the last pane.
        let pane = if self.mode == Mode::Overview {
            Pane::List
        } else {
            self.focus
        };
        match pane {
            Pane::List => {
                move_state(&mut self.list_state, self.order.len(), delta);
                if let Some(i) = self.list_state.selected()
                    && let Some(k) = self.order.get(i).cloned()
                {
                    // List navigation re-aims the lens without touching history.
                    self.root = k;
                    self.rebuild_trees();
                }
            }
            Pane::Upstream => move_state(&mut self.up_state, self.up_rows.len(), delta),
            Pane::Downstream => move_state(&mut self.down_state, self.down_rows.len(), delta),
        }
    }

    fn selected_tree_row(&self) -> Option<&TreeRow> {
        let (rows, state) = match self.focus {
            Pane::Upstream => (&self.up_rows, &self.up_state),
            Pane::Downstream => (&self.down_rows, &self.down_state),
            Pane::List => return None,
        };
        state.selected().and_then(|i| rows.get(i))
    }

    fn enter(&mut self) {
        match self.focus {
            Pane::List => {
                if !self.up_rows.is_empty() {
                    self.focus = Pane::Upstream;
                } else if !self.down_rows.is_empty() {
                    self.focus = Pane::Downstream;
                }
            }
            Pane::Upstream | Pane::Downstream => {
                let Some(row) = self.selected_tree_row() else {
                    return;
                };
                if row.kind == NodeKind::External {
                    self.status_msg = Some(format!(
                        "{} is external (team {}) — open it in Linear to follow its chain",
                        row.key,
                        self.graph.get(&row.key).map(|i| i.team()).unwrap_or("?")
                    ));
                    return;
                }
                let key = row.key.clone();
                self.set_root(key, true);
            }
        }
    }

    fn toggle_collapse(&mut self) {
        let tag = match self.focus {
            Pane::Upstream => "U",
            Pane::Downstream => "D",
            Pane::List => return,
        };
        if let Some(row) = self.selected_tree_row()
            && row.has_children
        {
            let id = format!("{tag}:{}", row.key);
            if !self.collapsed.remove(&id) {
                self.collapsed.insert(id);
            }
            self.rebuild_trees();
        }
    }

    fn go_back(&mut self) {
        if let Some(prev) = self.history.pop() {
            self.root = prev;
            self.sync_list_selection();
            self.rebuild_trees();
        } else {
            self.status_msg = Some("nothing to go back to".into());
        }
    }

    fn jump_to_cycle(&mut self) {
        let members = self.graph.cycle_members();
        if members.is_empty() {
            self.status_msg = Some("no dependency cycles 🎉".into());
            return;
        }
        // Advance to the next cycle member after wherever we're standing, derived
        // from the current root rather than a counter that drifts when you re-root
        // by other means. Cyclic SCCs always have ≥2 members (self-blocks are
        // rejected), so this always moves — the status line never claims a jump
        // that didn't happen.
        let next = match members.iter().position(|k| *k == self.root) {
            Some(i) => (i + 1) % members.len(),
            None => 0,
        };
        let key = members[next].clone();
        self.status_msg = Some(format!("cycle {}/{} — {}", next + 1, members.len(), key));
        self.set_root(key, true);
    }

    /// Jump to the next issue whose agent needs you, in display order, wrapping
    /// from wherever the lens currently sits — the fleet-view analogue of the
    /// cycle jump (`c`).
    fn jump_to_needs_you(&mut self) {
        let members: Vec<String> = self
            .graph
            .keys()
            .iter()
            .filter(|k| self.fleet.get(*k).is_some_and(AgentStatus::needs_you))
            .cloned()
            .collect();
        if members.is_empty() {
            self.status_msg = Some("no agents need you right now".into());
            return;
        }
        let next = match members.iter().position(|k| *k == self.root) {
            Some(i) => (i + 1) % members.len(),
            None => 0,
        };
        let key = members[next].clone();
        self.status_msg = Some(format!("needs you {}/{} — {key}", next + 1, members.len()));
        self.set_root(key, true);
    }

    // ── Key handling ─────────────────────────────────────────────────────────

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }

        // While attached, the agent owns the keyboard — every key is forwarded
        // to its PTY except the detach chord. Handled before anything else so
        // the cockpit's own bindings (q, /, …) don't shadow the agent's input.
        if self.attached.is_some() {
            self.on_attached_key(key);
            return;
        }

        // A dashboard keypress acknowledges any standing needs-you footer and
        // clears the transient status line.
        self.status_msg = None;
        self.needs_you_alert = false;

        if self.search_active {
            self.on_search_key(key.code);
            return;
        }

        // Any key dismisses the help overlay (except a Quit binding, which still
        // quits). The overlay sits above the keymap so a typo can't trap you.
        if self.show_help {
            if self.keymap.action_for(key) == Some(Action::Quit) {
                self.should_quit = true;
            } else {
                self.show_help = false;
            }
            return;
        }

        // Esc is a fixed, context-sensitive key (close overview / clear search /
        // quit) — deliberately not remappable.
        if key.code == KeyCode::Esc {
            self.on_escape();
            return;
        }

        if let Some(action) = self.keymap.action_for(key) {
            self.dispatch(action);
        }
    }

    /// Run a cockpit action (resolved from the keymap, or a direct call).
    fn dispatch(&mut self, action: Action) {
        match action {
            Action::Quit => self.should_quit = true,
            Action::MoveDown => self.move_selection(1),
            Action::MoveUp => self.move_selection(-1),
            Action::FocusList => self.focus = Pane::List,
            Action::CyclePane => {
                self.focus = match self.focus {
                    Pane::List | Pane::Downstream => Pane::Upstream,
                    Pane::Upstream => Pane::Downstream,
                }
            }
            Action::CycleFocus => {
                self.focus = match self.focus {
                    Pane::List => Pane::Upstream,
                    Pane::Upstream => Pane::Downstream,
                    Pane::Downstream => Pane::List,
                }
            }
            Action::Enter => self.enter(),
            Action::ToggleCollapse => self.toggle_collapse(),
            Action::Back => self.go_back(),
            Action::JumpCycle => self.jump_to_cycle(),
            Action::JumpNeedsYou => self.jump_to_needs_you(),
            Action::LaunchAgent => self.launch_agent(),
            Action::CancelAgent => self.cancel_agent(),
            Action::Attach => self.attach(),
            // Only meaningful while attached (handled in on_attached_key).
            Action::Detach => {}
            Action::CycleFilter => {
                self.filter = self.filter.next();
                self.rebuild_order();
            }
            Action::CycleSort => {
                self.sort = self.sort.next();
                self.rebuild_order();
            }
            Action::ToggleGraph => {
                self.mode = match self.mode {
                    Mode::Lens => Mode::Overview,
                    Mode::Overview => Mode::Lens,
                }
            }
            Action::StartSearch => {
                self.search_active = true;
                self.focus = Pane::List;
            }
            Action::ToggleHelp => self.show_help = !self.show_help,
        }
    }

    /// Apply a background [`AppEvent`] to view state, returning whether the
    /// screen must repaint as a result. The render loop is the single writer of
    /// `App`, so every off-thread update funnels through here — keeping
    /// rendering a pure function of state.
    pub fn apply_event(&mut self, ev: AppEvent) -> bool {
        match ev {
            AppEvent::Notification(text) => {
                // A rejected launch (e.g. "already has a running agent" / "at
                // capacity") arrives as a Notification; clear any optimistic
                // pending-launch marks so a later retry isn't wrongly refused.
                self.pending_launch.clear();
                self.set_footer(text);
                true
            }
            AppEvent::AgentSpawned { issue, backend } => {
                self.pending_launch.remove(&issue);
                self.fleet.insert(issue.clone(), AgentStatus::Spawning);
                self.backends.insert(issue.clone(), backend);
                self.set_footer(format!("agent launched on {issue} — t to attach"));
                true
            }
            // Repaint only when the output belongs to the agent we're attached
            // to; off-screen agents' output changes nothing visible.
            AppEvent::AgentOutput { issue } => self.attached.as_deref() == Some(issue.as_str()),
            AppEvent::AgentExited { issue, code } => {
                // The supervisor's agent task is authoritative for fleet status
                // (via AgentStatusChanged) — a cancel reads as Idle, a self-exit
                // as Done/Failed. Here we only surface a footer line and reclaim
                // the render handle now the PTY is gone, unless the user is still
                // attached and looking at its final screen.
                self.set_footer(match code {
                    Some(0) | None => format!("agent on {issue} finished"),
                    Some(c) => format!("agent on {issue} exited ({c})"),
                });
                if self.attached.as_deref() != Some(issue.as_str()) {
                    self.backends.remove(&issue);
                }
                true
            }
            AppEvent::AgentNeedsYou { issue, reason } => {
                self.fleet.insert(issue.clone(), AgentStatus::NeedsYou);
                self.status_msg = Some(format!("⚑ {issue} needs you — {reason}"));
                self.needs_you_alert = true; // sticky until acknowledged
                true
            }
            AppEvent::AgentStatusChanged { issue, status } => {
                // An explicit status transition is authoritative — it's the one
                // event allowed to clear a NeedsYou (the supervisor resolving the
                // agent's lifecycle). Drop the sticky footer if the node it
                // referred to is no longer waiting on the human.
                if !status.needs_you() && self.fleet.get(&issue) == Some(&AgentStatus::NeedsYou) {
                    self.needs_you_alert = false;
                }
                self.fleet.insert(issue, status);
                true
            }
            AppEvent::AgentAction { issue, action } => {
                // Hook-bus PostToolUse chatter. It must not clobber a pending
                // NeedsYou the human still has to act on: a queued PostToolUse can
                // arrive just after a permission prompt. Leave a NeedsYou node
                // alone (only a resolving AgentStatusChanged/AgentNeedsYou moves
                // it), and don't let the routine footer bury an unacknowledged
                // needs-you alert.
                if self.fleet.get(&issue) != Some(&AgentStatus::NeedsYou) {
                    self.fleet.insert(issue.clone(), AgentStatus::Running);
                }
                if !self.needs_you_alert {
                    self.status_msg = Some(format!("{issue}: {action}"));
                }
                true
            }
            AppEvent::AgentReaped { issue } => {
                // The supervisor dropped this agent from its live map (teardown
                // complete). Drop it from the fleet view so the overview stays
                // bounded and mirrors the supervisor. Keep the backend handle only
                // while the user is still attached to its final screen — detaching
                // then clears it.
                self.fleet.remove(&issue);
                if self.attached.as_deref() != Some(issue.as_str()) {
                    self.backends.remove(&issue);
                }
                true
            }
        }
    }

    /// Set the transient footer line, superseding (and acknowledging) any
    /// standing needs-you alert — used by deliberate, low-frequency events.
    fn set_footer(&mut self, text: String) {
        self.status_msg = Some(text);
        self.needs_you_alert = false;
    }

    /// Launch an agent on the focused issue (the `a` key). Surfaces a reason in
    /// the footer when it can't.
    fn launch_agent(&mut self) {
        let Some(issue) = self.focused_issue() else {
            self.status_msg = Some("no issue selected".into());
            return;
        };
        if issue.external {
            let key = issue.key.clone();
            self.status_msg = Some(format!("{key} is external — launch it in its own project"));
            return;
        }
        let (key, title) = (issue.key.clone(), issue.title.clone());
        // Pre-validate against what the cockpit already knows, so we don't print
        // an optimistic "launching…" the supervisor contradicts a tick later. The
        // supervisor's running/capacity guards remain authoritative; this just
        // closes the obvious double-press window (a live fleet entry, or a launch
        // already in flight) the cockpit can see locally.
        if self.fleet.get(&key).is_some_and(is_live) {
            self.status_msg = Some(format!("{key} already has a running agent"));
            return;
        }
        if self.pending_launch.contains(&key) {
            self.status_msg = Some(format!("already launching an agent on {key}…"));
            return;
        }
        // Clone the handle out so we can also touch `status_msg` without a
        // borrow conflict; the clone is cheap (an mpsc sender).
        match self.supervisor.clone() {
            Some(supervisor) => {
                supervisor.launch(key.clone(), title);
                self.pending_launch.insert(key.clone());
                self.status_msg = Some(format!("launching agent on {key}…"));
            }
            None => self.status_msg = Some("agent control plane unavailable".into()),
        }
    }

    /// Stop the agent on the focused issue (the `x` key), leaving others running.
    fn cancel_agent(&mut self) {
        let Some(issue) = self.focused_issue().map(|i| i.key.clone()) else {
            return;
        };
        // Gate on a *live* status, not mere presence: a Done/Failed entry lingers
        // in `fleet` for its glyph but the supervisor has already reaped it, so
        // sending Cancel would only earn a contradicting "no agent running" a tick
        // later. This keeps the cockpit's claim aligned with what it can do.
        if !self.fleet.get(&issue).is_some_and(is_live) {
            self.status_msg = Some(format!("agent on {issue} is not running"));
            return;
        }
        match self.supervisor.clone() {
            Some(supervisor) => {
                supervisor.cancel(issue.clone());
                self.status_msg = Some(format!("cancelling agent on {issue}…"));
            }
            None => self.status_msg = Some("agent control plane unavailable".into()),
        }
    }

    /// `(agents, needs-you)` counts for the header summary. "Agents" counts only
    /// *live* nodes (spawning / running / needs-you / idle-but-alive), not the
    /// terminal Done/Failed entries that linger in `fleet` until relaunched — so
    /// the header reflects what's actually running, not every issue that ever ran
    /// an agent.
    pub fn fleet_summary(&self) -> (usize, usize) {
        let agents = self.fleet.values().filter(|&s| is_live(s)).count();
        let needs_you = self.fleet.values().filter(|s| s.needs_you()).count();
        (agents, needs_you)
    }

    /// The label of the currently-bound detach key (e.g. `F10`), so the attach
    /// pane always shows the real key even after a rebind.
    pub fn detach_key_label(&self) -> String {
        self.keymap.label_for(Action::Detach)
    }

    /// Attach to the focused issue's agent, taking over its PTY (the `t` key).
    fn attach(&mut self) {
        let Some(issue) = self.focused_issue().map(|i| i.key.clone()) else {
            return;
        };
        if !self.backends.contains_key(&issue) {
            self.status_msg = Some(format!("no agent on {issue} to attach to"));
            return;
        }
        self.attached = Some(issue.clone());
        self.attached_size = None; // force a resize to the pane on first render
        self.pending_leader = None;
        let detach = self.keymap.label_for(Action::Detach);
        self.status_msg = Some(format!("attached to {issue} · {detach} to detach"));
    }

    /// Detach back to the dashboard, leaving the agent running. If the agent
    /// exited while we were attached, reclaim its render handle on the way out.
    fn detach(&mut self) {
        self.pending_leader = None;
        if let Some(issue) = self.attached.take() {
            self.attached_size = None;
            let still_running = self
                .backends
                .get(&issue)
                .is_some_and(|b| matches!(b.status(), Lifecycle::Running));
            if still_running {
                self.status_msg = Some(format!("detached from {issue} (still running)"));
            } else {
                self.backends.remove(&issue);
                self.status_msg = Some(format!("detached from {issue}"));
            }
        }
    }

    /// Handle a key while attached: the detach key returns to the dashboard;
    /// everything else is encoded and written to the agent's PTY.
    fn on_attached_key(&mut self, key: KeyEvent) {
        // The detach gesture returns to the dashboard; everything else is
        // forwarded to the agent. Detach is F10 by default (a function key works
        // on every layout and never collides with claude's line editing), but can
        // be rebound — including to a tmux-style leader sequence like `Ctrl-A d`,
        // which works even on keyboards/terminals without usable function keys.

        // Mid-sequence: a leader was pressed and we're waiting for the rest.
        if let Some(leader) = self.pending_leader.take() {
            if self.keymap.detach_completes(leader, key) {
                self.detach();
            } else if self.keymap.same_chord(leader, key) {
                // Leader pressed twice — send a single leader chord to the agent,
                // so a chosen leader is never wholly unreachable.
                self.forward_to_agent(key);
            } else {
                // Not a completion: the leader was a no-op; forward this key.
                self.forward_to_agent(key);
            }
            return;
        }

        // A single-key detach (e.g. the default F10).
        if self.keymap.action_for(key) == Some(Action::Detach) {
            self.detach();
            return;
        }
        // Arm a leader sequence; the next key completes or cancels it.
        if self.keymap.is_detach_leader(key) {
            self.pending_leader = Some(key);
            return;
        }
        self.forward_to_agent(key);
    }

    /// Encode a key and write it to the attached agent's PTY.
    fn forward_to_agent(&mut self, key: KeyEvent) {
        let bytes = backend::key_to_bytes(key);
        if bytes.is_empty() {
            return;
        }
        let Some(issue) = self.attached.clone() else {
            return;
        };
        let Some(backend) = self.backends.get(&issue) else {
            return;
        };
        if backend.send_input(&bytes).is_err() {
            // The PTY is gone — the agent exited out from under us. Surface it and
            // detach so keystrokes don't silently vanish into a dead terminal.
            self.set_footer(format!("agent on {issue} is no longer accepting input"));
            self.attached = None;
        }
    }

    /// Whether a detach leader has been pressed and we're awaiting completion.
    pub fn detach_armed(&self) -> bool {
        self.pending_leader.is_some()
    }

    fn on_escape(&mut self) {
        if self.show_help {
            self.show_help = false;
        } else if self.search_active {
            self.search_active = false;
            self.search_query.clear();
            self.rebuild_order();
        } else if self.mode == Mode::Overview {
            self.mode = Mode::Lens;
        } else {
            self.should_quit = true;
        }
    }

    fn on_search_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.search_active = false;
                self.search_query.clear();
                self.rebuild_order();
            }
            KeyCode::Enter => self.search_active = false,
            KeyCode::Backspace => {
                self.search_query.pop();
                self.rebuild_order();
            }
            KeyCode::Char(c) => {
                self.search_query.push(c);
                self.rebuild_order();
            }
            _ => {}
        }
    }
}

// ── Free helpers ────────────────────────────────────────────────────────────

/// Whether an agent status means the node is *alive* (so it should count as a
/// running agent and be a valid stop/attach target). Done/Failed are terminal:
/// the entry lingers in `fleet` for the glyph but is no longer a live agent.
/// Idle counts as live — the conversation is quiet but the process is up.
fn is_live(status: &AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Spawning | AgentStatus::Running | AgentStatus::NeedsYou | AgentStatus::Idle
    )
}

fn move_state(state: &mut ListState, len: usize, delta: i32) {
    if len == 0 {
        state.select(None);
        return;
    }
    let cur = state.selected().unwrap_or(0) as i32;
    let next = (cur + delta).rem_euclid(len as i32) as usize;
    state.select(Some(next));
}

fn clamp_selection(state: &mut ListState, len: usize) {
    if len == 0 {
        state.select(None);
    } else {
        let i = state.selected().unwrap_or(0).min(len - 1);
        state.select(Some(i));
    }
}

/// Compare identifiers naturally: same prefix sorts by numeric suffix.
fn natural_key_cmp(a: &str, b: &str) -> Ordering {
    fn split(k: &str) -> (&str, u64) {
        match k.split_once('-') {
            Some((p, n)) => (p, n.parse().unwrap_or(0)),
            None => (k, 0),
        }
    }
    let (pa, na) = split(a);
    let (pb, nb) = split(b);
    pa.cmp(pb).then(na.cmp(&nb))
}

/// The once-per-node sort key for the active sort mode. Lower sorts first;
/// `natural_key_cmp` breaks ties. `Ready` and `Blocked` are mirror images:
/// both group by blocked-vs-unblocked, then by higher downstream impact (hence
/// the inversion) — `Ready` surfaces unblocked work, `Blocked` surfaces blocked.
fn sort_key(graph: &Graph, key: &str, sort: Sort) -> (u8, u64) {
    let by_impact = || u64::MAX - graph.transitive(key, Direction::Downstream) as u64;
    match sort {
        Sort::Ready => (u8::from(graph.is_blocked(key)), by_impact()),
        Sort::Blocked => (u8::from(!graph.is_blocked(key)), by_impact()),
        Sort::Status => (status_rank(graph, key), 0),
        Sort::Priority => (graph.get(key).map_or(u8::MAX, |i| i.priority.rank()), 0),
        Sort::Key => (0, 0),
    }
}

/// Sort rank that surfaces live work first: in-progress → unstarted → backlog →
/// done.
fn status_rank(graph: &Graph, key: &str) -> u8 {
    use crate::model::Status::*;
    match graph.get(key).map(|i| i.status) {
        Some(Started) => 0,
        Some(Triage) => 1,
        Some(Unstarted) => 2,
        Some(Backlog) => 3,
        Some(Completed) => 4,
        Some(Duplicate) => 5,
        Some(Canceled) => 6,
        _ => 7,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn app() -> App {
        App::new(demo::graph())
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn default_focus_is_the_most_connected_issue() {
        // ZAP-204 has the most direct edges in the demo graph.
        assert_eq!(app().root, "ZAP-204");
    }

    #[test]
    fn enter_on_a_blocker_re_roots_and_back_returns() {
        let mut app = app();
        press(&mut app, KeyCode::Char('l')); // focus upstream tree
        assert_eq!(app.focus, Pane::Upstream);

        let target = app.up_rows[0].key.clone(); // first blocker of ZAP-204
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.root, target);
        assert_ne!(app.root, "ZAP-204");

        press(&mut app, KeyCode::Char('b')); // back
        assert_eq!(app.root, "ZAP-204");
    }

    #[test]
    fn list_navigation_re_aims_the_lens() {
        let mut app = app();
        let idx0 = app.list_state.selected().unwrap();
        assert_eq!(app.order[idx0], app.root); // selection tracks the root
        press(&mut app, KeyCode::Down);
        let idx1 = app.list_state.selected().unwrap();
        assert_ne!(idx1, idx0);
        assert_eq!(app.root, app.order[idx1]); // root re-aimed to new selection
    }

    #[test]
    fn collapsing_a_subtree_hides_its_children() {
        let mut app = app();
        press(&mut app, KeyCode::Char('l')); // upstream; row 0 = ZAP-188 (has child ZAP-150)
        let before = app.up_rows.len();
        press(&mut app, KeyCode::Char(' '));
        assert!(app.up_rows.len() < before);
        press(&mut app, KeyCode::Char(' ')); // expand again
        assert_eq!(app.up_rows.len(), before);
    }

    #[test]
    fn collapse_state_resets_when_the_lens_re_roots() {
        let mut app = app();
        press(&mut app, KeyCode::Char('l')); // upstream
        let before = app.up_rows.len();
        press(&mut app, KeyCode::Char(' ')); // collapse a subtree
        assert!(app.up_rows.len() < before);
        press(&mut app, KeyCode::Enter); // re-root onto the collapsed node
        press(&mut app, KeyCode::Char('b')); // back to the original root
        assert_eq!(
            app.up_rows.len(),
            before,
            "the lens re-opens fully expanded after re-rooting"
        );
    }

    #[test]
    fn cycle_jump_moves_to_a_distinct_member_each_press() {
        let mut app = app();
        assert!(app.graph.cycle_members().len() >= 2);
        press(&mut app, KeyCode::Char('c'));
        let first = app.root.clone();
        assert!(app.graph.in_cycle(&first));
        press(&mut app, KeyCode::Char('c'));
        assert!(app.graph.in_cycle(&app.root));
        assert_ne!(
            first, app.root,
            "each press lands on a different cycle member"
        );
        assert!(app.status_msg.as_deref().unwrap().contains(&app.root));
    }

    #[test]
    fn ready_sort_puts_unblocked_issues_first() {
        let app = app(); // Sort::Ready is the default
        assert_eq!(app.sort, Sort::Ready);
        // The list is partitioned: every unblocked issue precedes every blocked one.
        let mut seen_blocked = false;
        for k in &app.order {
            if app.graph.is_blocked(k) {
                seen_blocked = true;
            } else {
                assert!(!seen_blocked, "unblocked {k} appears after a blocked issue");
            }
        }
    }

    #[test]
    fn search_filters_then_clears() {
        let mut app = app();
        press(&mut app, KeyCode::Char('/'));
        assert!(app.search_active);
        for c in "210".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert_eq!(app.order, vec!["ZAP-210".to_string()]);
        press(&mut app, KeyCode::Esc);
        assert!(!app.search_active);
        assert!(app.search_query.is_empty());
        assert!(app.order.len() > 1);
    }

    #[test]
    fn natural_key_cmp_orders_by_numeric_suffix() {
        assert_eq!(natural_key_cmp("ZAP-9", "ZAP-188"), Ordering::Less);
        assert_eq!(natural_key_cmp("ZAP-188", "ZAP-9"), Ordering::Greater);
        // A differing prefix orders lexically by prefix, ignoring the number.
        assert_eq!(natural_key_cmp("ABC-500", "ZAP-1"), Ordering::Less);
    }

    #[test]
    fn filtering_out_the_root_re_aims_the_lens_to_the_visible_selection() {
        // Searching for an issue other than the default root (ZAP-204) must keep
        // the list highlight and the lens describing the SAME issue.
        let mut app = app();
        press(&mut app, KeyCode::Char('/'));
        for c in "210".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert_eq!(app.order, vec!["ZAP-210".to_string()]);
        let sel = app.list_state.selected().expect("a row stays selected");
        assert_eq!(app.order[sel], app.root, "highlight and root agree");
        assert_eq!(app.focused_issue().unwrap().key, "ZAP-210");
    }

    #[test]
    fn external_node_cannot_be_re_rooted() {
        let mut app = app();
        press(&mut app, KeyCode::Char('l'));
        // Find the external INFRA-77 row and select it.
        let idx = app
            .up_rows
            .iter()
            .position(|r| r.kind == NodeKind::External)
            .expect("demo has an external blocker");
        app.up_state.select(Some(idx));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.root, "ZAP-204"); // unchanged
        assert!(app.status_msg.is_some()); // explained instead
    }

    #[test]
    fn cycle_jump_and_mode_and_quit_never_panic() {
        let mut app = app();
        press(&mut app, KeyCode::Char('c')); // jump to a cycle member
        assert!(app.graph.in_cycle(&app.root));
        for k in ['f', 's', 'g', 'g', '?', '?'] {
            press(&mut app, KeyCode::Char(k));
        }
        press(&mut app, KeyCode::Char('q'));
        assert!(app.should_quit);
    }

    #[test]
    fn jump_to_needs_you_visits_each_flagged_issue_then_reports_when_none() {
        let mut app = app();
        // No agents yet → the jump is a no-op that says so.
        press(&mut app, KeyCode::Char('n'));
        assert!(app.status_msg.as_deref().unwrap().contains("no agents"));

        app.fleet.insert("ZAP-205".into(), AgentStatus::NeedsYou);
        app.fleet.insert("ZAP-240".into(), AgentStatus::NeedsYou);
        app.fleet.insert("ZAP-201".into(), AgentStatus::Running); // not "needs you"

        press(&mut app, KeyCode::Char('n'));
        let first = app.root.clone();
        assert!(app.fleet.get(&first).is_some_and(AgentStatus::needs_you));
        press(&mut app, KeyCode::Char('n'));
        assert_ne!(
            app.root, first,
            "each press advances to a different flagged issue"
        );
        assert!(app.fleet.get(&app.root).is_some_and(AgentStatus::needs_you));
        // Running agents are never visited.
        assert_ne!(app.root, "ZAP-201");
    }

    #[test]
    fn attach_forwards_keys_to_the_agent_then_detaches() {
        let mut app = app();
        let fake = crate::backend::fake::FakeBackend::new("ZAP-204");
        app.backends
            .insert("ZAP-204".into(), fake.clone() as Arc<dyn AgentBackend>);
        app.fleet.insert("ZAP-204".into(), AgentStatus::Running);
        app.root = "ZAP-204".into();

        press(&mut app, KeyCode::Char('t')); // attach
        assert_eq!(app.attached.as_deref(), Some("ZAP-204"));

        // A normal key now drives the agent, not the cockpit (note 'q' would
        // otherwise quit — proof the agent owns the keyboard).
        press(&mut app, KeyCode::Char('q'));
        assert!(!app.should_quit, "q goes to the agent while attached");
        assert_eq!(fake.inputs.lock().unwrap().last().unwrap(), b"q");

        // F10 detaches, leaving the agent running.
        press(&mut app, KeyCode::F(10));
        assert!(app.attached.is_none());
        // The detach key itself is not sent to the agent.
        assert_eq!(fake.inputs.lock().unwrap().len(), 1);
    }

    #[test]
    fn agent_exited_frees_the_backend_but_leaves_fleet_status_to_the_supervisor() {
        let mut app = app();
        let fake = crate::backend::fake::FakeBackend::new("ZAP-204");
        app.backends
            .insert("ZAP-204".into(), fake as Arc<dyn AgentBackend>);
        app.fleet.insert("ZAP-204".into(), AgentStatus::Running);

        // A nonzero exit must NOT itself flip the node to Failed — the
        // supervisor's AgentStatusChanged is the authority (so a cancel reads
        // as Idle, not Failed). AgentExited only reclaims the dead PTY handle.
        app.apply_event(AppEvent::AgentExited {
            issue: "ZAP-204".into(),
            code: Some(1),
        });
        assert_eq!(app.fleet.get("ZAP-204"), Some(&AgentStatus::Running));
        assert!(
            !app.backends.contains_key("ZAP-204"),
            "dead PTY handle reclaimed"
        );

        // The supervisor's status event is what actually moves the node.
        app.apply_event(AppEvent::AgentStatusChanged {
            issue: "ZAP-204".into(),
            status: AgentStatus::Failed,
        });
        assert_eq!(app.fleet.get("ZAP-204"), Some(&AgentStatus::Failed));
    }

    #[test]
    fn agent_exited_keeps_the_backend_while_attached() {
        let mut app = app();
        let fake = crate::backend::fake::FakeBackend::new("ZAP-204");
        app.backends
            .insert("ZAP-204".into(), fake as Arc<dyn AgentBackend>);
        app.attached = Some("ZAP-204".into());
        app.apply_event(AppEvent::AgentExited {
            issue: "ZAP-204".into(),
            code: Some(0),
        });
        assert!(
            app.backends.contains_key("ZAP-204"),
            "kept so the attached user can read its final screen; freed on detach"
        );
    }

    #[test]
    fn leader_sequence_detach_arms_completes_and_passes_through() {
        let mut app = app();
        app.keymap
            .apply(&[("detach".to_string(), vec!["ctrl-a d".to_string()])]);
        let fake = crate::backend::fake::FakeBackend::new("ZAP-204");
        app.backends
            .insert("ZAP-204".into(), fake.clone() as Arc<dyn AgentBackend>);
        app.root = "ZAP-204".into();
        let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);

        // Attach, then press the leader: it arms without detaching or forwarding.
        press(&mut app, KeyCode::Char('t'));
        assert_eq!(app.attached.as_deref(), Some("ZAP-204"));
        app.on_key(ctrl_a);
        assert!(app.detach_armed(), "leader arms the sequence");
        assert!(app.attached.is_some());
        assert!(
            fake.inputs.lock().unwrap().is_empty(),
            "leader isn't forwarded"
        );

        // Completion detaches.
        press(&mut app, KeyCode::Char('d'));
        assert!(app.attached.is_none());
        assert!(!app.detach_armed());

        // Re-attach (the agent is still running, so its backend was kept).
        press(&mut app, KeyCode::Char('t'));
        assert_eq!(app.attached.as_deref(), Some("ZAP-204"));

        // Double-tapping the leader sends one Ctrl-A (0x01) through, not detach.
        app.on_key(ctrl_a);
        app.on_key(ctrl_a);
        assert!(app.attached.is_some(), "double-tap doesn't detach");
        assert_eq!(fake.inputs.lock().unwrap().last().unwrap(), &vec![0x01]);

        // Arm, then a non-completion key cancels and is forwarded.
        app.on_key(ctrl_a);
        assert!(app.detach_armed());
        press(&mut app, KeyCode::Char('z'));
        assert!(!app.detach_armed());
        assert!(app.attached.is_some());
        assert_eq!(fake.inputs.lock().unwrap().last().unwrap(), b"z");
    }

    #[test]
    fn attach_without_an_agent_is_a_no_op_with_a_reason() {
        let mut app = app(); // no backends registered
        press(&mut app, KeyCode::Char('t'));
        assert!(app.attached.is_none());
        assert!(app.status_msg.as_deref().unwrap().contains("no agent"));
    }

    #[test]
    fn fleet_summary_counts_agents_and_attention() {
        let mut app = app();
        assert_eq!(app.fleet_summary(), (0, 0));
        app.fleet.insert("ZAP-204".into(), AgentStatus::Running);
        app.fleet.insert("ZAP-205".into(), AgentStatus::NeedsYou);
        assert_eq!(app.fleet_summary(), (2, 1));
    }

    #[test]
    fn fleet_summary_excludes_terminal_agents() {
        // Terminal nodes linger in `fleet` for their glyph but must not inflate
        // the header's live-agent count. Done/Failed are terminal; Idle is alive.
        let mut app = app();
        app.fleet.insert("ZAP-201".into(), AgentStatus::Running);
        app.fleet.insert("ZAP-204".into(), AgentStatus::Idle);
        app.fleet.insert("ZAP-205".into(), AgentStatus::Done);
        app.fleet.insert("ZAP-210".into(), AgentStatus::Failed);
        app.fleet.insert("ZAP-240".into(), AgentStatus::NeedsYou);
        // Running + Idle + NeedsYou are live (3); Done/Failed are not. One needs you.
        assert_eq!(app.fleet_summary(), (3, 1));
    }

    #[test]
    fn post_tool_use_does_not_clear_a_pending_needs_you() {
        // A queued PostToolUse (→Running) arriving just after a permission prompt
        // (→NeedsYou) must not silently downgrade the node the human has to act
        // on, nor bury the footer alert.
        let mut app = app();
        app.apply_event(AppEvent::AgentNeedsYou {
            issue: "ZAP-204".into(),
            reason: "permission".into(),
        });
        assert_eq!(app.fleet.get("ZAP-204"), Some(&AgentStatus::NeedsYou));
        let alert = app.status_msg.clone();

        app.apply_event(AppEvent::AgentAction {
            issue: "ZAP-204".into(),
            action: "ran Bash".into(),
        });
        assert_eq!(
            app.fleet.get("ZAP-204"),
            Some(&AgentStatus::NeedsYou),
            "tool chatter must not downgrade a NeedsYou node"
        );
        assert_eq!(
            app.status_msg, alert,
            "tool chatter must not bury the needs-you footer"
        );
        // A different agent's action still lands normally.
        app.apply_event(AppEvent::AgentAction {
            issue: "ZAP-201".into(),
            action: "ran Read".into(),
        });
        assert_eq!(app.fleet.get("ZAP-201"), Some(&AgentStatus::Running));
    }

    #[test]
    fn an_explicit_status_change_resolves_a_needs_you() {
        // Only a resolving AgentStatusChanged (the supervisor's authority) clears
        // a NeedsYou — then routine chatter is free to move the node again.
        let mut app = app();
        app.apply_event(AppEvent::AgentNeedsYou {
            issue: "ZAP-204".into(),
            reason: "permission".into(),
        });
        app.apply_event(AppEvent::AgentStatusChanged {
            issue: "ZAP-204".into(),
            status: AgentStatus::Running,
        });
        assert_eq!(app.fleet.get("ZAP-204"), Some(&AgentStatus::Running));
        // The sticky footer alert is lifted, so chatter shows again.
        app.apply_event(AppEvent::AgentAction {
            issue: "ZAP-204".into(),
            action: "ran Edit".into(),
        });
        assert_eq!(
            app.status_msg.as_deref(),
            Some("ZAP-204: ran Edit"),
            "after resolution, the activity line is restored"
        );
    }

    #[test]
    fn a_dashboard_keypress_acknowledges_the_needs_you_footer() {
        let mut app = app();
        app.apply_event(AppEvent::AgentNeedsYou {
            issue: "ZAP-204".into(),
            reason: "permission".into(),
        });
        assert!(app.needs_you_alert);
        // Any dashboard key acknowledges (here: cycle filter); chatter is no
        // longer suppressed afterwards.
        press(&mut app, KeyCode::Char('f'));
        assert!(!app.needs_you_alert);
        app.apply_event(AppEvent::AgentAction {
            issue: "ZAP-204".into(),
            action: "ran Grep".into(),
        });
        assert_eq!(app.status_msg.as_deref(), Some("ZAP-204: ran Grep"));
    }

    #[test]
    fn cancel_is_refused_on_a_terminal_or_absent_agent() {
        let mut app = app();
        app.root = "ZAP-204".into();
        // No agent at all.
        app.dispatch(Action::CancelAgent);
        assert!(
            app.status_msg.as_deref().unwrap().contains("not running"),
            "{:?}",
            app.status_msg
        );
        // A reaped Done entry lingers in fleet but is not a live cancel target.
        app.fleet.insert("ZAP-204".into(), AgentStatus::Done);
        app.dispatch(Action::CancelAgent);
        assert!(app.status_msg.as_deref().unwrap().contains("not running"));
        // A live agent is a valid target (no supervisor here, so it reports the
        // missing control plane rather than the "not running" guard).
        app.fleet.insert("ZAP-204".into(), AgentStatus::Running);
        app.dispatch(Action::CancelAgent);
        assert!(
            !app.status_msg.as_deref().unwrap().contains("not running"),
            "a live agent passes the guard"
        );
    }

    #[test]
    fn launch_is_refused_while_an_agent_is_already_live() {
        let mut app = app();
        app.root = "ZAP-204".into();
        app.fleet.insert("ZAP-204".into(), AgentStatus::Running);
        app.dispatch(Action::LaunchAgent);
        assert!(
            app.status_msg
                .as_deref()
                .unwrap()
                .contains("already has a running agent"),
            "{:?}",
            app.status_msg
        );
    }

    #[test]
    fn a_second_launch_is_refused_while_the_first_is_in_flight() {
        // The double-press window: a pending launch (sent, not yet AgentSpawned)
        // blocks a second optimistic "launching…". AgentSpawned then clears it.
        let mut app = app();
        app.root = "ZAP-204".into();
        // Simulate the in-flight mark the supervisor path would set.
        app.pending_launch.insert("ZAP-204".into());
        app.dispatch(Action::LaunchAgent);
        assert!(
            app.status_msg
                .as_deref()
                .unwrap()
                .contains("already launching"),
            "{:?}",
            app.status_msg
        );
        // The spawn acknowledgement clears the pending mark.
        let fake = crate::backend::fake::FakeBackend::new("ZAP-204");
        app.apply_event(AppEvent::AgentSpawned {
            issue: "ZAP-204".into(),
            backend: fake as Arc<dyn AgentBackend>,
        });
        assert!(!app.pending_launch.contains("ZAP-204"));
    }

    #[test]
    fn empty_list_navigation_keeps_root_and_never_panics() {
        // A search that matches nothing empties the list; navigation must be a
        // safe no-op and the lens must keep its last valid root + trees.
        let mut app = app();
        let root = app.root.clone();
        press(&mut app, KeyCode::Char('/'));
        for c in "zzzznomatch".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert!(app.order.is_empty());
        for code in [
            KeyCode::Down,
            KeyCode::Up,
            KeyCode::Enter,
            KeyCode::Char(' '),
            KeyCode::Tab,
        ] {
            press(&mut app, code);
        }
        assert_eq!(app.root, root);
        assert!(!app.up_rows.is_empty());
    }
}
