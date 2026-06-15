//! Interactive application state and input handling for the cockpit.
//!
//! Cockpit v3 is a tmux-style tiling window manager: a horizontal strip of
//! focusable [`crate::window::Window`]s — the permanent **Spine** (issue list /
//! agents roster), live **Agent** PTYs, and **Deps** trees (per-issue or the
//! Fleet map). The focused window gets your keys; the **prefix** (`Ctrl-a`) is
//! the sole escape to window-manager verbs. [`crate::window::WindowSet`] is the
//! source of truth for what's on screen; this module owns the rest of the view
//! state and routes input. Rendering lives in [`crate::ui`]; this module never
//! touches the terminal.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::layout::Rect;
use ratatui::widgets::ListState;

use crate::backend::{self, AgentBackend, Lifecycle};
use crate::event::AppEvent;
use crate::keymap::{Action, Keymap};
use crate::layout;
use crate::model::{Direction, Graph, Issue};
use crate::session::{AgentStatus, CockpitState, PersistedKind, PersistedWindow};
use crate::supervisor::SupervisorHandle;
use crate::window::{
    CoinMode, DepsCursor, GraduateOutcome, LayoutMode, WindowId, WindowKind, WindowSet,
};

/// How many animation frames a node flash lasts (~400 ms at the 100 ms tick).
const FLASH_FRAMES: u64 = 4;

/// Hard ceiling (~20 s at the 100 ms tick) on how long the "resuming N…" spinner
/// keeps the cockpit animating. A resume that wedges (a stuck `git`, a spawn that
/// never reports) must not pin the loop awake forever; past this the count is
/// force-cleared so an idle cockpit goes quiet.
const RESUME_GRACE_FRAMES: u64 = 200;

/// What fills the Spine: the full issue list (the navigation spine) or the
/// agents roster — every issue that has an agent, sorted by how much it wants
/// your attention. A "tab" you flip with the roster key; selecting a roster row
/// re-aims the spine selection at that agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeftView {
    Issues,
    Agents,
}

/// A brief, self-extinguishing highlight on an issue's node — the "juice" that
/// makes a launch or a finish register. Lives for a few animation frames then
/// expires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flash {
    Launched,
    Finished,
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

pub struct App {
    pub graph: Graph,
    pub order: Vec<String>,
    pub list_state: ListState,

    /// The Spine's current selection — the issue the detail bar describes and the
    /// attach/spawn button acts on. Re-aimed by list/roster navigation and the
    /// cycle / needs-you jumps. (Each Deps window keeps its *own* root.)
    pub root: String,

    /// The window strip — the source of truth for what's on screen.
    pub windows: WindowSet,
    /// Last (rows, cols) each *window's* PTY was resized to, keyed by
    /// [`WindowId`] (not issue) so zoom — which can show one issue at two
    /// geometries across the toggle frame — and any duplicate window stay
    /// unambiguous. We reflow a `claude` only when its window's geometry actually
    /// changes, so browsing/scrolling never churns SIGWINCHes.
    pub preview_size: HashMap<WindowId, (u16, u16)>,
    /// Last known terminal area, set on resize. Lets the input/poll side compute
    /// the post-scroll *visible* window set (which `draw` also derives from the
    /// real frame) without `draw` mutating state.
    pub viewport: Rect,

    pub left_view: LeftView,
    pub filter: Filter,
    pub sort: Sort,
    pub search_query: String,
    pub search_active: bool,
    pub show_help: bool,
    /// Dismissable overlay summarising the selected issue (the `i` button).
    pub show_summary: bool,
    pub status_msg: Option<String>,
    /// True while `status_msg` holds an unacknowledged "needs you" alert. Routine
    /// high-frequency tool chatter (`AgentAction`) must not bury it; it clears the
    /// moment the human touches a Spine/Deps key (acknowledging) or a deliberate
    /// event replaces the footer.
    needs_you_alert: bool,
    /// Issues with a launch command in flight (sent to the supervisor, not yet
    /// acknowledged by an `AgentSpawned` or rejected by a `Notification`). Lets
    /// the cockpit refuse a double-press before the fleet entry materializes.
    pending_launch: HashSet<String>,
    /// Issues the supervisor has fully reaped (`AgentReaped`) this session — a
    /// tombstone. The agent's hook forwarder is a separate, slower path, so a
    /// final `Notification`/`Stop`/`PostToolUse` hook can land *after* the reap;
    /// without this, that late hook would re-insert a live status for an agent
    /// with no backend, inflating the live count and re-arming the sticky alert
    /// with nothing left to clear it. A real relaunch clears the tombstone via
    /// `AgentSpawned`, so it never blocks a fresh agent.
    reaped: HashSet<String>,

    /// Per-issue agent status, driven by the supervisor + notification bus.
    /// Absence of an entry means "no agent" — the fleet view's resting state.
    pub fleet: HashMap<String, AgentStatus>,
    /// Backend handles for agents we launched, keyed by issue. Used to render
    /// and drive an agent's PTY.
    pub backends: HashMap<String, Arc<dyn AgentBackend>>,
    /// An issue whose agent we just launched and are waiting to come up, so the
    /// agent's window opens+focuses the moment it spawns (a user-initiated launch
    /// is the only `AgentSpawned` that steals focus — background/resume spawns
    /// just populate the roster).
    pub pending_attach: Option<String>,
    /// Monotonic animation tick, advanced by the render loop only while something
    /// is animating. The renderer reads it to drive spinners/pulses.
    pub frame: u64,
    /// Transient per-issue node flashes: issue → (kind, frame it expires at).
    pub flash: HashMap<String, (Flash, u64)>,
    /// True after the prefix chord, while the next key is read as a verb (or a
    /// second prefix, forwarded to a focused agent as the literal chord). The v3
    /// generalisation of v2's single `pending_leader` detach gesture.
    pub prefix_armed: bool,
    /// The issue whose agent a `Ctrl-a x` kill is awaiting confirmation for. While
    /// `Some`, the next key confirms (`y`/Enter) or cancels — kill is destructive,
    /// so it's never a single keystroke.
    pub kill_confirm: Option<String>,
    /// Docked agents still pending an auto-resume (Phase 6), each mapped to the
    /// frame at which a wedged resume (one that never reports `AgentSpawned`) is
    /// force-dropped. Per-issue — not a bare count + one shared deadline — so a
    /// stuck resume can't be kept alive indefinitely by a trickle of later ones,
    /// and each spinner self-heals on its own grace. Drives the "resuming N…"
    /// header and keeps the loop animating until it empties.
    resuming: HashMap<String, u64>,
    /// Live-backend ceiling the supervisor enforces (its `max_concurrent`), or 0
    /// when auto-resume is off. A resume that would exceed it is skipped rather
    /// than fired into the supervisor's "at capacity" no-op (which emits only a
    /// `Notification`, never `AgentSpawned`, so the spinner would burn its whole
    /// grace for nothing); the lazy path retries on the next focus once a slot
    /// frees.
    resume_cap: usize,
    /// Whether auto-resume is on (off under `--no-resume`, in `--demo`, tests).
    /// Gates the lazy resume-on-first-focus of docked agents.
    auto_resume: bool,
    /// Handle to the agent supervisor, when running with one (absent in `--demo`,
    /// snapshots and unit tests).
    pub supervisor: Option<SupervisorHandle>,
    /// Active key bindings (defaults, overridden by `config.toml`).
    pub keymap: Keymap,

    /// Where the window layout persists (`.lindep/cockpit.json`), or `None` when
    /// the control plane is off (`--demo`, snapshots, tests) — those never write.
    pub cockpit_path: Option<PathBuf>,
    /// Set when the docked window set / layout / focus changed and the layout
    /// should be re-persisted. The render thread (the sole cockpit writer) checks
    /// it after handling input and saves, so a structural change survives a crash.
    pub cockpit_dirty: bool,

    pub should_quit: bool,
}

impl App {
    pub fn new(graph: Graph) -> Self {
        // Default selection: the most-connected real issue — usually the spine of
        // the dependency web — so the cockpit opens somewhere interesting.
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

        let mut windows = WindowSet::new();
        // Seed the transient context window at index 1 for the default selection,
        // focused on the Spine — so the cockpit opens ready to browse. At startup
        // there are no agents, so chat-first falls back to the dependency lens.
        if !root.is_empty() {
            windows.ensure_preview(&root, CoinMode::Deps, &graph);
            windows.focus = 0;
        }

        let mut app = App {
            graph,
            order: Vec::new(),
            list_state: ListState::default(),
            root,
            windows,
            preview_size: HashMap::new(),
            viewport: Rect::new(0, 0, 80, 24),
            left_view: LeftView::Issues,
            filter: Filter::All,
            sort: Sort::Ready,
            search_query: String::new(),
            search_active: false,
            show_help: false,
            show_summary: false,
            status_msg: None,
            needs_you_alert: false,
            pending_launch: HashSet::new(),
            reaped: HashSet::new(),
            fleet: HashMap::new(),
            backends: HashMap::new(),
            pending_attach: None,
            frame: 0,
            flash: HashMap::new(),
            prefix_armed: false,
            kill_confirm: None,
            resuming: HashMap::new(),
            resume_cap: 0,
            auto_resume: false,
            supervisor: None,
            keymap: Keymap::default(),
            cockpit_path: None,
            cockpit_dirty: false,
            should_quit: false,
        };
        app.rebuild_order();
        app
    }

    pub fn focused_issue(&self) -> Option<&Issue> {
        self.graph.get(&self.root)
    }

    /// The issue the detail bar / summary overlay describes: the focused coin's
    /// issue, else the Spine selection. `None` only when nothing is selected.
    pub fn detail_key(&self) -> Option<&str> {
        match self.windows.focused().issue() {
            Some(i) => Some(i),
            None if !self.root.is_empty() => Some(self.root.as_str()),
            None => None,
        }
    }

    /// The prefix chord's label (e.g. `Ctrl-A`), for hints/help.
    pub fn prefix_label(&self) -> String {
        self.keymap.prefix_label()
    }

    /// Record the terminal size (on resize / startup) so the visible-window set
    /// the poll cadence keys off matches what `draw` will place.
    pub fn set_viewport(&mut self, area: Rect) {
        self.viewport = area;
    }

    // ── Derived list ordering (the Spine's issue list) ─────────────────────────

    fn rebuild_order(&mut self) {
        let needle = self.search_query.to_lowercase();
        let g = &self.graph;
        let (filter, sort) = (self.filter, self.sort);

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
        // If the active filter/search hid the current selection, re-aim it at the
        // first visible issue so the list highlight and the detail bar agree.
        if !self.order.is_empty() && !self.order.contains(&self.root) {
            self.root = self.order[0].clone();
        }
        self.sync_list_selection();
    }

    fn sync_list_selection(&mut self) {
        if let Some(i) = self.order.iter().position(|k| *k == self.root) {
            self.list_state.select(Some(i));
        } else {
            // The selection isn't in the visible list — show NO highlight rather
            // than lighting an unrelated row (a jump can land on a filtered-out
            // issue; the detail bar still describes it honestly).
            self.list_state.select(None);
        }
    }

    /// Whether the selection is absent from the visible list (hidden by the
    /// active filter/search), so the list intentionally shows no highlight.
    fn root_is_hidden(&self) -> bool {
        !self.order.is_empty() && !self.order.contains(&self.root)
    }

    /// Re-aim the Spine selection (and the context window that follows it)
    /// without touching any *pinned* window's deps history.
    fn aim_spine(&mut self, key: String) {
        if key.is_empty() {
            return;
        }
        self.root = key;
        self.sync_list_selection();
        self.reaim_preview();
    }

    // ── Key handling — the window router ───────────────────────────────────────

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }

        // 1. Mid-prefix: the next key is a window-manager verb (or a second
        //    prefix, forwarded to a focused agent as the literal chord).
        if self.prefix_armed {
            self.prefix_armed = false;
            self.on_prefix_key(key);
            return;
        }

        // 2. A pending kill confirmation captures the keyboard: y/Enter confirms,
        //    anything else cancels. Checked before the prefix so the destructive
        //    gesture can't be half-completed by a stray prefix.
        if self.kill_confirm.is_some() {
            self.on_kill_confirm_key(key);
            return;
        }

        // 3. The prefix arms; the next key resolves it.
        if self.keymap.is_prefix(key) {
            self.prefix_armed = true;
            return;
        }

        // 4. Search input captures the keyboard — but only while the Spine (whose
        //    list it filters) is focused. If focus moved to another window while a
        //    search was open, commit it (the filter stays applied) and route the
        //    key to that window, so a key meant for an agent's PTY can never be
        //    swallowed by the search buffer.
        if self.search_active {
            if self.windows.focus == 0 {
                self.on_search_key(key.code);
                return;
            }
            self.search_active = false;
        }

        // 5. The help overlay sits above the keymap so a typo can't trap you: any
        //    key (Esc included) dismisses it.
        if self.show_help || self.show_summary {
            self.show_help = false;
            self.show_summary = false;
            return;
        }

        // 6. Route by the focused window's kind.
        match self.windows.focused_kind().clone() {
            // A coin's chat face owns the keyboard: every key (Esc too) goes to its
            // PTY. The prefix above is the only escape (so `Ctrl-a Tab` flips a
            // focused chat to deps).
            WindowKind::Coin {
                issue,
                mode: CoinMode::Chat,
            } => self.forward_to_agent(&issue, key),
            // A coin's deps face navigates exactly like the old Deps pane.
            WindowKind::Coin {
                mode: CoinMode::Deps,
                ..
            } => {
                self.acknowledge();
                if key.code != KeyCode::Esc
                    && let Some(action) = self.keymap.action_for(key)
                {
                    self.dispatch_deps(action);
                }
            }
            WindowKind::Spine => {
                self.acknowledge();
                if key.code != KeyCode::Esc
                    && let Some(action) = self.keymap.action_for(key)
                {
                    self.dispatch_spine(action);
                }
            }
            // The Fleet overview has no per-node cursor; only window verbs (behind
            // the prefix) and the help toggle apply.
            WindowKind::Fleet => {
                self.acknowledge();
                if key.code != KeyCode::Esc
                    && let Some(action) = self.keymap.action_for(key)
                {
                    match action {
                        Action::ToggleHelp => self.show_help = !self.show_help,
                        Action::ToggleSummary => self.show_summary = !self.show_summary,
                        _ => {}
                    }
                }
            }
        }
    }

    /// A Spine/Deps keypress acknowledges any standing needs-you footer and
    /// clears the transient status line.
    fn acknowledge(&mut self) {
        self.status_msg = None;
        self.needs_you_alert = false;
    }

    /// Resolve a key pressed after the prefix.
    fn on_prefix_key(&mut self, key: KeyEvent) {
        // Double-prefix → send the literal prefix chord through to a focused
        // agent (a chosen prefix is never wholly unreachable by the PTY). Covers
        // the context window in Chat mode too (its `agent_issue()` is `Some`).
        if self.keymap.is_prefix(key) {
            if let Some(issue) = self.windows.focused_kind().agent_issue() {
                let issue = issue.to_string();
                self.forward_to_agent(&issue, self.keymap.prefix_event());
            }
            return;
        }
        let Some(verb) = self.keymap.verb_for(key) else {
            return; // an unbound prefix key is a harmless no-op
        };
        self.dispatch_verb(verb);
    }

    /// Run a window-manager verb (always reached behind the prefix).
    fn dispatch_verb(&mut self, verb: Action) {
        match verb {
            Action::FocusLeft => {
                self.windows.focus_left();
                self.after_focus_change();
            }
            Action::FocusRight => {
                self.windows.focus_right();
                self.after_focus_change();
            }
            Action::FocusNav => self.windows.focus_nav(),
            Action::ZoomToggle => self.windows.toggle_zoom(),
            Action::ContextToggle => self.flip_active_coin(),
            Action::PinWindow => self.pin_window(),
            Action::CloseWindow => self.close_window(),
            Action::KillWindow => self.arm_kill(),
            Action::LayoutToggle => self.toggle_layout(),
            Action::AttachOrSpawn => self.button(),
            Action::Quit => self.should_quit = true,
            Action::StartSearch => {
                self.windows.focus = 0;
                self.start_search();
            }
            Action::ToggleHelp => self.show_help = !self.show_help,
            Action::ToggleSummary => self.show_summary = !self.show_summary,
            Action::ToggleRoster => {
                self.windows.focus = 0;
                self.toggle_roster();
            }
            Action::JumpNeedsYou => self.jump_to_needs_you(),
            // The rest are direct (Spine/Deps) actions, never prefix verbs.
            _ => {}
        }
    }

    /// Direct keys while the Spine is focused.
    fn dispatch_spine(&mut self, action: Action) {
        match action {
            Action::MoveDown => self.move_selection(1),
            Action::MoveUp => self.move_selection(-1),
            // Enter / Space are the attach+spawn button on the Spine.
            Action::Enter | Action::ToggleCollapse => self.button(),
            // Tab flips the active coin chat⇄deps while you browse the nav.
            Action::ContextToggle => self.flip_active_coin(),
            Action::OpenDeps => self.open_deps_for_selection(),
            Action::OpenFleet => self.open_fleet(),
            Action::JumpCycle => self.jump_to_cycle(),
            Action::JumpNeedsYou => self.jump_to_needs_you(),
            Action::ToggleRoster => self.toggle_roster(),
            Action::CycleFilter => {
                self.filter = self.filter.next();
                self.rebuild_order();
            }
            Action::CycleSort => {
                self.sort = self.sort.next();
                self.rebuild_order();
            }
            Action::StartSearch => self.start_search(),
            Action::ToggleHelp => self.show_help = !self.show_help,
            Action::ToggleSummary => self.show_summary = !self.show_summary,
            _ => {}
        }
    }

    /// Direct keys while a coin's deps face is focused (the per-issue tree drives
    /// its own cursor).
    fn dispatch_deps(&mut self, action: Action) {
        // Operations needing the graph are split out so the cursor borrow and the
        // `&self.graph` borrow don't overlap.
        match action {
            Action::MoveDown => self.with_deps(|c| c.move_selection(1)),
            Action::MoveUp => self.with_deps(|c| c.move_selection(-1)),
            Action::SwitchSide => self.with_deps(|c| c.switch_side()),
            Action::Enter => self.deps_enter(),
            Action::ToggleCollapse => self.deps_collapse(),
            Action::Back => self.deps_back(),
            // Tab flips this coin from its deps face to its chat face.
            Action::ContextToggle => self.flip_active_coin(),
            Action::OpenDeps => self.open_deps_for_selection(),
            Action::OpenFleet => self.open_fleet(),
            Action::ToggleHelp => self.show_help = !self.show_help,
            Action::ToggleSummary => self.show_summary = !self.show_summary,
            _ => {}
        }
    }

    /// Mutate the focused window's deps cursor, if it has one.
    fn with_deps(&mut self, f: impl FnOnce(&mut crate::window::DepsCursor)) {
        if let Some(cursor) = self.windows.focused_mut().deps.as_mut() {
            f(cursor);
        }
    }

    fn deps_enter(&mut self) {
        let graph = &self.graph;
        if let Some(cursor) = self.windows.focused_mut().deps.as_mut()
            && let Err(reason) = cursor.enter(graph)
        {
            self.status_msg = Some(reason);
        }
    }

    fn deps_collapse(&mut self) {
        let graph = &self.graph;
        if let Some(cursor) = self.windows.focused_mut().deps.as_mut() {
            cursor.toggle_collapse(graph);
        }
    }

    fn deps_back(&mut self) {
        let graph = &self.graph;
        let popped = self
            .windows
            .focused_mut()
            .deps
            .as_mut()
            .is_some_and(|c| c.back(graph));
        if !popped {
            self.status_msg = Some("nothing to go back to".into());
        }
    }

    fn start_search(&mut self) {
        self.search_active = true;
        // Search filters the issue list, so surface it (you can't fuzzy-find the
        // agents roster).
        self.left_view = LeftView::Issues;
    }

    fn move_selection(&mut self, delta: i32) {
        if self.left_view == LeftView::Agents {
            self.move_roster(delta);
            return;
        }
        move_state(&mut self.list_state, self.order.len(), delta);
        if let Some(i) = self.list_state.selected()
            && let Some(k) = self.order.get(i).cloned()
        {
            self.root = k; // list navigation re-aims the selection
            self.reaim_preview(); // …and the preview coin that follows it
        }
    }

    /// Focus the preview coin in its Deps face — dive into the selection's
    /// dependency tree.
    fn open_deps_for_selection(&mut self) {
        if self.root.is_empty() {
            self.status_msg = Some("no issue selected".into());
            return;
        }
        let root = self.root.clone();
        self.windows
            .ensure_preview(&root, CoinMode::Deps, &self.graph);
        self.windows.focus_preview();
    }

    /// Open (or focus) the single Fleet overview window — a pinned tab on the rail.
    fn open_fleet(&mut self) {
        self.windows.open_fleet();
    }

    // ── The active (context) window ────────────────────────────────────────────

    /// The chat-first default face for previewing `issue`: **Chat** when it has a
    /// live-or-imminent agent (so the preview shows the conversation), **Deps**
    /// otherwise (the honest resting view). If a coin for the issue is already a
    /// pinned tab, fall back to Deps — no point previewing what's already docked.
    /// Pure reads of fleet/backends/launch state, so it can never resurrect a
    /// tombstoned agent by side effect.
    fn default_preview_mode(&self, issue: &str) -> CoinMode {
        if self.windows.has_pinned_coin(issue) {
            return CoinMode::Deps;
        }
        let live = self.fleet.get(issue).is_some_and(AgentStatus::is_live)
            || self.pending_launch.contains(issue)
            || self.resuming.contains_key(issue);
        if live { CoinMode::Chat } else { CoinMode::Deps }
    }

    /// Re-aim the preview coin at the current Spine selection (the chat-first
    /// rule), following arrow nav and deliberate jumps alike — including a jump
    /// fired while the preview itself is focused (so `Ctrl-a n` onto a needy issue
    /// updates the focused pane, not just the selection; in-place deps-tree nav
    /// goes through `dispatch_deps`, never here, so nothing is "blown away").
    /// Reclaims the backend of an agent we re-aimed off, if it's now dead and
    /// unreferenced.
    fn reaim_preview(&mut self) {
        if self.root.is_empty() {
            return;
        }
        // If the selection already has a pinned coin, that coin IS the active view
        // — don't maintain a duplicate preview alongside it (just drop any stale
        // one, reclaiming a dead chat backend it was showing).
        if self.windows.has_pinned_coin(&self.root) {
            if let Some(removed) = self.windows.clear_preview()
                && let Some((issue, CoinMode::Chat)) = removed.kind.coin()
            {
                let issue = issue.to_string();
                self.reclaim_if_dead(&issue);
            }
            return;
        }
        let prev = self.windows.preview();
        let issue = self.root.clone();
        let mode = self.default_preview_mode(&issue);
        self.windows.ensure_preview(&issue, mode, &self.graph);
        if let Some((prev_issue, CoinMode::Chat)) = prev
            && prev_issue != issue
        {
            self.reclaim_if_dead(&prev_issue);
        }
    }

    /// Flip the active coin between its chat and deps faces (`Tab` / `Ctrl-a Tab`).
    /// The target is the focused coin, or — when the Spine/Fleet is focused — the
    /// preview, so you can flip the active window while browsing the nav. Flipping
    /// a docked coin to its chat face may lazy-resume its agent.
    fn flip_active_coin(&mut self) {
        let idx = if matches!(self.windows.focused_kind(), WindowKind::Coin { .. }) {
            self.windows.focus
        } else if let Some(p) = self.windows.preview_index() {
            p
        } else {
            return;
        };
        self.windows.flip_coin_face(idx, &self.graph);
        self.maybe_resume_focused();
    }

    // ── The attach/spawn button ────────────────────────────────────────────────

    /// Open an agent window on the selection — launching (or resuming) if there's
    /// no live one. The v3 merge of v2's `launch_agent` + `attach`: one agent per
    /// issue, never duplicated; a live one is revealed, a terminal one relaunched
    /// (which resumes its conversation).
    fn button(&mut self) {
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

        // A backend already exists (live, or a finished screen still up): reveal
        // its window, no relaunch.
        if self.backends.contains_key(&key) {
            self.open_agent_window(&key);
            return;
        }
        // A launch already in flight (double-press before it spawns): focus the
        // starting card rather than spinning up a second.
        if self.pending_launch.contains(&key) {
            self.open_agent_window(&key);
            self.set_footer(format!("already opening an agent on {key}…"));
            return;
        }
        // Absent, or terminal → (re)launch; the supervisor resumes transparently
        // if a session already exists. Open the window now (a "starting…" card)
        // so the press registers; `AgentSpawned` fills in the backend.
        match self.supervisor.clone() {
            Some(supervisor) => {
                supervisor.launch(key.clone(), title);
                self.pending_launch.insert(key.clone());
                self.pending_attach = Some(key.clone());
                self.open_agent_window(&key);
                self.set_footer(format!("opening agent on {key}…"));
            }
            None => self.status_msg = Some("agent control plane unavailable".into()),
        }
    }

    /// Reveal `issue`'s agent screen and focus it. If the agent is already a
    /// pinned tab, focus that tab; otherwise show it in the active (context)
    /// window in Chat mode and focus that. There's no separate preview to
    /// displace, so nothing to reclaim here.
    /// If `issue` already has a pinned coin, focus it on its chat face and return
    /// true. So "going to" an already-pinned issue lands on the pinned coin rather
    /// than spinning up a duplicate preview — used by the button and the needs-you
    /// jump.
    fn focus_pinned_chat(&mut self, issue: &str) -> bool {
        let Some(i) = self.windows.pinned_coin_index(issue) else {
            return false;
        };
        self.windows.focus = i;
        if !matches!(
            self.windows.windows[i].kind,
            WindowKind::Coin {
                mode: CoinMode::Chat,
                ..
            }
        ) {
            self.windows.flip_coin_face(i, &self.graph);
        }
        self.after_focus_change(); // lazy-resume a docked agent we just revealed
        true
    }

    fn open_agent_window(&mut self, issue: &str) {
        if self.focus_pinned_chat(issue) {
            return;
        }
        self.windows
            .ensure_preview(issue, CoinMode::Chat, &self.graph);
        self.windows.focus_preview();
    }

    // ── Window-manager verbs ───────────────────────────────────────────────────

    fn pin_window(&mut self) {
        if self.windows.focus == 0 {
            self.status_msg = Some("the spine is always pinned".into());
            return;
        }
        // Pinning the preview *graduates* it to a permanent coin, then a fresh
        // preview re-arms for the current selection.
        if self.windows.focused().is_preview() {
            let preview_id = self.windows.focused().id;
            let outcome = self.windows.pin_preview();
            // On a merge the preview is dropped (its identity already had a pinned
            // coin), so reclaim its cached PTY geometry — otherwise the map grows a
            // stale entry per merge. On a graduate the id lives on (kept).
            if matches!(outcome, GraduateOutcome::Merged(_)) {
                self.preview_size.remove(&preview_id);
            }
            let label = self
                .windows
                .focused()
                .issue()
                .unwrap_or("window")
                .to_string();
            self.reaim_preview();
            self.cockpit_dirty = true;
            self.set_footer(match outcome {
                GraduateOutcome::Merged(_) => format!("{label} is already pinned"),
                _ => format!("pinned {label} · stays while you browse"),
            });
            return;
        }
        // An already-pinned coin / Fleet → unpin = undock (close it; a live agent
        // keeps running, refind via the roster).
        self.close_window();
    }

    fn close_window(&mut self) {
        if self.windows.focus == 0 {
            self.status_msg = Some("the spine stays put".into());
            return;
        }
        // The preview is structural — it can't be closed (Tab flips it, pin keeps
        // it).
        if self.windows.focused().is_preview() {
            self.status_msg =
                Some("the preview can't be closed — Tab flips it, pin keeps it".into());
            return;
        }
        let Some(closed) = self.windows.close_focused() else {
            self.status_msg = Some("the spine stays put".into());
            return;
        };
        if closed.pinned {
            self.cockpit_dirty = true; // a docked window left the set
        }
        self.preview_size.remove(&closed.id);
        if let Some(issue) = closed.issue().map(str::to_string) {
            // Close = undock: a *live* agent keeps running (refind via the
            // roster), so only reclaim its backend once it's actually dead.
            let running = self.fleet.get(&issue).is_some_and(AgentStatus::is_live);
            self.reclaim_if_dead(&issue);
            self.set_footer(if running {
                format!("closed {issue} · still running — r to refind")
            } else {
                format!("closed {issue}")
            });
        } else {
            self.status_msg = Some("closed window".into()); // the Fleet overview
        }
    }

    /// Arm a confirmed kill of the focused agent (`Ctrl-a x`). Kill is destructive
    /// and separate from close, so it's never a single keystroke.
    fn arm_kill(&mut self) {
        // A coin carries its agent on either face, so kill works from chat or deps.
        let Some(issue) = self.windows.focused().issue().map(str::to_string) else {
            self.status_msg = Some("no agent here to kill — Ctrl-a w closes a window".into());
            return;
        };
        if !self.fleet.get(&issue).is_some_and(AgentStatus::is_live) {
            self.status_msg = Some(format!("agent on {issue} is not running"));
            return;
        }
        self.status_msg = Some(format!(
            "kill agent on {issue}? y to confirm, any key to cancel"
        ));
        self.kill_confirm = Some(issue);
    }

    fn on_kill_confirm_key(&mut self, key: KeyEvent) {
        let Some(issue) = self.kill_confirm.take() else {
            return;
        };
        let confirm = matches!(
            key.code,
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter
        );
        if !confirm {
            self.status_msg = Some("kill cancelled".into());
            return;
        }
        match self.supervisor.clone() {
            Some(supervisor) => {
                supervisor.cancel(issue.clone());
                self.status_msg = Some(format!("killing agent on {issue}…"));
            }
            None => self.status_msg = Some("agent control plane unavailable".into()),
        }
    }

    fn toggle_layout(&mut self) {
        self.windows.force_layout(self.windows.layout.toggled());
        self.cockpit_dirty = true;
        // Every window's Rect moves under the new layout; forget the cached sizes
        // so the next render reflows each live agent to where it now sits. This
        // (and zoom) are the *only* moments a live PTY reflows — browsing never does.
        self.preview_size.clear();
        self.set_footer(format!("layout: {}", self.windows.layout.label()));
    }

    /// Forward a key to a specific agent's PTY.
    fn forward_to_agent(&mut self, issue: &str, key: KeyEvent) {
        let bytes = backend::key_to_bytes(key);
        if bytes.is_empty() {
            return;
        }
        let Some(backend) = self.backends.get(issue) else {
            return;
        };
        if backend.send_input(&bytes).is_err() {
            // The PTY is gone — the agent exited out from under us. Surface it; the
            // window stays (as an EXITED card) until you close it.
            self.set_footer(format!("agent on {issue} is no longer accepting input"));
        }
    }

    // ── Focus bookkeeping / visibility ─────────────────────────────────────────

    /// Bookkeeping after focus moves between windows: commit an in-progress search
    /// if focus left the Spine (so keys reach the newly-focused window instead of
    /// the search buffer — the search filter stays applied), and lazy-resume a
    /// docked agent that just gained focus.
    fn after_focus_change(&mut self) {
        if self.search_active && self.windows.focus != 0 {
            self.search_active = false; // commit: end input mode, keep the query
        }
        self.maybe_resume_focused();
    }

    /// Whether window `idx` renders as a live PTY right now — so only that window
    /// forces a fast poll / repaint (idle-quiet). In the Rail layout only the big
    /// pane is live; Mosaic shows every window; zoom shows only the big pane.
    /// The window that represents the current selection — its pinned coin if it has
    /// one, else the transient preview. The rail's big pane when the Spine is
    /// focused, so "going to" a pinned issue surfaces that coin (not a duplicate
    /// preview). `None` only when nothing represents the selection.
    pub fn active_index(&self) -> Option<usize> {
        self.windows
            .pinned_coin_index(&self.root)
            .or_else(|| self.windows.preview_index())
    }

    fn is_index_visible(&self, idx: usize) -> bool {
        // A zero-area viewport draws nothing, so nothing is live — never fast-poll
        // into the void.
        if self.viewport.area() == 0 {
            return false;
        }
        let n = self.windows.windows.len();
        let focus = self.windows.focus;
        let active = self.active_index();
        let w = self.viewport.width;
        if self.windows.zoomed {
            // Zoom fills the whole viewport (no Spine reserved), so the big pane is
            // live whenever there's any area.
            return layout::rail_big_index(n, focus, active) == Some(idx);
        }
        // The width-aware predicates mirror the renderer's drops on a cramped
        // terminal, so an off-screen agent never pins the 16 ms loop.
        match self.windows.layout {
            LayoutMode::Mosaic => layout::mosaic_visible(n, idx, w),
            LayoutMode::Rail => layout::rail_visible(n, focus, active, idx, w),
        }
    }

    /// Whether `issue`'s live screen is on screen — gates the AgentOutput repaint
    /// so only visible agents force a redraw (preserving idle-quiet). Allocation-
    /// free: `AgentOutput` fires per PTY read, so this is a hot path.
    pub fn is_agent_visible(&self, issue: &str) -> bool {
        self.windows
            .windows
            .iter()
            .enumerate()
            .any(|(idx, w)| w.kind.agent_issue() == Some(issue) && self.is_index_visible(idx))
    }

    /// Whether any *visible* window hosts a live PTY — so the render loop polls
    /// fast (16 ms) only when an interactive screen is actually on screen, never
    /// for an idle agent scrolled off the strip.
    pub fn has_visible_live_agent(&self) -> bool {
        self.windows.windows.iter().enumerate().any(|(idx, w)| {
            w.kind
                .agent_issue()
                .is_some_and(|i| self.backends.contains_key(i))
                && self.is_index_visible(idx)
        })
    }

    // ── Cockpit layout persistence ──────────────────────────────────────────

    /// Snapshot the docked (pinned) windows, layout mode and focus identity for
    /// `.lindep/cockpit.json`. Unpinned previews and the Spine are not persisted;
    /// focus on either persists as `None` (restore falls back to the Spine).
    pub fn snapshot_cockpit(&self) -> CockpitState {
        let windows = self
            .windows
            .windows
            .iter()
            .filter(|w| w.pinned && !matches!(w.kind, WindowKind::Spine))
            .filter_map(window_to_persisted)
            .collect();
        let focused = self.windows.focused();
        let focus = (focused.pinned && !matches!(focused.kind, WindowKind::Spine))
            .then(|| window_to_persisted(focused))
            .flatten();
        CockpitState {
            layout: self.windows.layout.label().to_string(),
            windows,
            focus,
            ..CockpitState::default()
        }
    }

    /// Rebuild the window strip from a persisted layout, pruning windows whose
    /// subject won't come back: Agent windows whose issue isn't in `resumable`
    /// (the post-reconcile **was-live** set — Done/Failed/Stopped are excluded, so
    /// a terminal agent's window is *not* restored to a permanent "resuming…"
    /// card with nothing ever resuming it), and Deps windows whose root left the
    /// graph. With no docked windows persisted the cockpit's fresh default strip
    /// is kept (so a missing file — or a session where nothing was pinned — opens
    /// like today), while still honouring the saved layout mode. Restored Agent
    /// windows have no backend yet — they render as "resuming…" cards until the
    /// eager/lazy resume fills them.
    pub fn apply_cockpit(&mut self, state: &CockpitState, resumable: &HashSet<String>) {
        // The persisted layout label is advisory in v3.2 — the count-driven auto
        // layout governs after restore (a manual `Ctrl-a |` override is
        // session-only), so we don't force it here.
        //
        // No docked windows → keep the fresh default strip (Spine + preview).
        if state.windows.is_empty() {
            return;
        }
        let mut set = WindowSet::new();
        for pw in &state.windows {
            match pw.kind {
                // A coin persisted on its chat face — restored chatless (a
                // "resuming…" card) until the eager/lazy resume fills its backend.
                PersistedKind::Agent => {
                    if let Some(issue) = &pw.issue
                        && resumable.contains(issue)
                    {
                        set.push(
                            WindowKind::Coin {
                                issue: issue.clone(),
                                mode: CoinMode::Chat,
                            },
                            true,
                            None,
                        );
                    }
                }
                // A coin persisted on its deps face — restored with a fresh cursor.
                // If its root left the graph but its agent is still resumable, don't
                // strand the agent (a deps cursor needs a graph node we no longer
                // have): restore it chat-faced so the eager/lazy resume revives it —
                // matching how a chat-faced coin for the same issue would survive.
                PersistedKind::Deps => {
                    if let Some(root) = &pw.issue {
                        if self.graph.get(root).is_some() {
                            let cursor = DepsCursor::new(root.clone(), &self.graph);
                            set.push(
                                WindowKind::Coin {
                                    issue: root.clone(),
                                    mode: CoinMode::Deps,
                                },
                                true,
                                Some(cursor),
                            );
                        } else if resumable.contains(root) {
                            set.push(
                                WindowKind::Coin {
                                    issue: root.clone(),
                                    mode: CoinMode::Chat,
                                },
                                true,
                                None,
                            );
                        }
                    }
                }
                PersistedKind::Fleet => {
                    set.push(WindowKind::Fleet, true, None);
                }
            }
        }
        // If every persisted window pruned away (none resumable, roots all gone),
        // fall back to the fresh default strip rather than leaving a bare spine.
        if set.windows.len() == 1 {
            return;
        }
        // Restore focus by identity, falling back to the Spine.
        set.focus = state
            .focus
            .as_ref()
            .and_then(|want| {
                set.windows
                    .iter()
                    .position(|w| window_to_persisted(w).as_ref() == Some(want))
            })
            .unwrap_or(0);
        self.windows = set;
        // Re-seed a fresh transient preview at index 1 for the current selection
        // (it's never persisted); this shifts the restored pins right by one and
        // keeps focus on the same window.
        self.reaim_preview();
    }

    // ── Auto-resume (Cockpit v3, Phase 6) ────────────────────────────────────

    /// Bring docked agents back on startup: eager-resume the focused docked agent
    /// plus up to `cap-1` others (so the supervisor's `max_concurrent` isn't
    /// blown), and lazy-resume the rest on first focus (see
    /// [`Self::maybe_resume_focused`]). `resumable` is the post-reconcile
    /// was-live set (never Done/Failed). Enables auto-resume for the session.
    pub fn begin_resume(&mut self, resumable: &HashSet<String>, cap: usize) {
        self.auto_resume = true;
        self.resume_cap = cap;
        if self.supervisor.is_none() {
            return;
        }
        // Docked agent windows that are resumable, the focused one first so it
        // comes back immediately.
        let mut targets: Vec<String> = Vec::new();
        // The focused docked coin first, so it comes back immediately.
        let focused = self.windows.focused();
        if let WindowKind::Coin { issue, .. } = &focused.kind
            && focused.pinned
            && resumable.contains(issue)
        {
            targets.push(issue.clone());
        }
        for w in &self.windows.windows {
            if let WindowKind::Coin { issue, .. } = &w.kind
                && w.pinned
                && resumable.contains(issue)
                && !targets.contains(issue)
            {
                targets.push(issue.clone());
            }
        }
        let eager = cap.max(1).min(targets.len());
        for issue in targets.into_iter().take(eager) {
            self.resume_one(&issue);
        }
    }

    /// Lazy-resume the focused window if it's a docked agent that was live before
    /// the restart but has no backend yet — so a deep docked agent comes back the
    /// moment you focus it, without all of them spawning at once on startup.
    fn maybe_resume_focused(&mut self) {
        if !self.auto_resume {
            return;
        }
        // Covers a focused Agent tab *or* the context window in Chat mode pointed
        // at a docked agent.
        let Some(issue) = self
            .windows
            .focused_kind()
            .agent_issue()
            .map(str::to_string)
        else {
            return;
        };
        if self.backends.contains_key(&issue) || self.pending_launch.contains(&issue) {
            return; // already up, or already resuming
        }
        // "Was-live" survives rehydration as a live fleet status (Idle/NeedsYou).
        if self.fleet.get(&issue).is_some_and(AgentStatus::is_live) {
            self.resume_one(&issue);
        }
    }

    /// Fire a single resume launch (no focus-steal — the window already exists).
    fn resume_one(&mut self, issue: &str) {
        let Some(supervisor) = self.supervisor.clone() else {
            return;
        };
        if self.pending_launch.contains(issue) {
            return;
        }
        // Don't fire a resume the supervisor can only reject as "at capacity": it
        // would emit a bare Notification (never an AgentSpawned), so the spinner
        // would burn its whole grace window for an agent that never comes up.
        // Leave it a docked card; the lazy path retries on the next focus once a
        // live backend frees a slot. (live backends + in-flight resumes ≈ load.)
        if self.resume_cap > 0 && self.backends.len() + self.resuming.len() >= self.resume_cap {
            return;
        }
        let title = self
            .graph
            .get(issue)
            .map(|i| i.title.clone())
            .unwrap_or_else(|| issue.to_string());
        supervisor.launch(issue.to_string(), title);
        self.pending_launch.insert(issue.to_string());
        // Each resume carries its own grace deadline, so a wedged spawn self-
        // clears here (in `tick_frame`) without later resumes pushing it out —
        // the spinner can't pin the loop awake past the grace, eager or lazy.
        self.resuming
            .insert(issue.to_string(), self.frame + RESUME_GRACE_FRAMES);
    }

    // ── Background events ───────────────────────────────────────────────────────

    /// Apply a background [`AppEvent`] to view state, returning whether the
    /// screen must repaint. The render loop is the single writer of `App`.
    pub fn apply_event(&mut self, ev: AppEvent) -> bool {
        match ev {
            AppEvent::Notification(text) => {
                self.pending_launch.clear();
                self.set_footer(text);
                true
            }
            AppEvent::AgentSpawned { issue, backend } => {
                // Clear the double-launch guard (set by the button AND by a resume).
                self.pending_launch.remove(&issue);
                // A real relaunch revives the issue — clear any reaped tombstone.
                self.reaped.remove(&issue);
                self.fleet.insert(issue.clone(), AgentStatus::Spawning);
                self.backends.insert(issue.clone(), backend);
                self.flash
                    .insert(issue.clone(), (Flash::Launched, self.frame + FLASH_FRAMES));
                // This resume settled (Phase 6): drop its entry; the header
                // spinner clears once the map empties.
                self.resuming.remove(&issue);
                // Only an explicit button launch (pending_attach) opens + focuses
                // the window; a background/resume spawn just fills the backend of
                // an already-docked window (or the roster), so a burst of resumes
                // never yanks focus around.
                if self.pending_attach.as_deref() == Some(issue.as_str()) {
                    self.pending_attach = None;
                    self.open_agent_window(&issue);
                    self.set_footer(format!("agent on {issue} ready"));
                }
                true
            }
            // Repaint only when this agent's screen is visible right now.
            AppEvent::AgentOutput { issue } => self.is_agent_visible(&issue),
            AppEvent::AgentExited { issue, code } => {
                self.set_footer(match code {
                    Some(0) | None => format!("agent on {issue} finished"),
                    Some(c) => format!("agent on {issue} exited ({c})"),
                });
                // Its geometry is meaningless once it's dead; drop it so a relaunch
                // reflows from scratch.
                self.drop_preview_sizes_for(&issue);
                // The process is gone (per this event). Keep the backend only if a
                // window still references it (its EXITED card); else reclaim.
                if !self.windows.references_agent(&issue) {
                    self.backends.remove(&issue);
                }
                true
            }
            AppEvent::AgentNeedsYou { issue, reason } => {
                if self.is_terminal(&issue) || self.reaped.contains(&issue) {
                    return false;
                }
                self.fleet.insert(issue.clone(), AgentStatus::NeedsYou);
                self.status_msg = Some(format!("⚑ {issue} needs you — {reason}"));
                self.needs_you_alert = true; // sticky until acknowledged
                true
            }
            AppEvent::AgentStatusChanged { issue, status } => {
                if self.reaped.contains(&issue) {
                    return false;
                }
                if status.is_live() && self.is_terminal(&issue) {
                    return false;
                }
                if !status.needs_you() && self.fleet.get(&issue) == Some(&AgentStatus::NeedsYou) {
                    self.needs_you_alert = false;
                }
                if matches!(status, AgentStatus::Done | AgentStatus::Failed) {
                    self.flash
                        .insert(issue.clone(), (Flash::Finished, self.frame + FLASH_FRAMES));
                }
                self.fleet.insert(issue, status);
                true
            }
            AppEvent::AgentAction { issue, action } => {
                if self.is_terminal(&issue) || self.reaped.contains(&issue) {
                    return false;
                }
                if self.fleet.get(&issue) != Some(&AgentStatus::NeedsYou) {
                    self.fleet.insert(issue.clone(), AgentStatus::Running);
                }
                if !self.needs_you_alert {
                    self.status_msg = Some(format!("{issue}: {action}"));
                }
                true
            }
            AppEvent::AgentReaped { issue } => {
                self.reaped.insert(issue.clone());
                self.fleet.remove(&issue);
                self.drop_preview_sizes_for(&issue);
                // Keep the backend only while a window still shows it.
                if !self.windows.references_agent(&issue) {
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

    /// Drop the cached PTY geometry for every window referencing `issue` —
    /// AgentExited/Reaped only know the issue, so enumerate its windows.
    fn drop_preview_sizes_for(&mut self, issue: &str) {
        let ids: Vec<WindowId> = self
            .windows
            .windows
            .iter()
            .filter(|w| w.kind.agent_issue() == Some(issue))
            .map(|w| w.id)
            .collect();
        for id in ids {
            self.preview_size.remove(&id);
        }
    }

    /// Reclaim a backend whose agent is dead and no longer shown anywhere. Used on
    /// close / preview-displacement, where the agent may still be alive (in which
    /// case its backend is kept for re-open via the roster).
    fn reclaim_if_dead(&mut self, issue: &str) {
        let dead = self
            .backends
            .get(issue)
            .is_some_and(|b| matches!(b.status(), Lifecycle::Exited(_)));
        if dead && !self.windows.references_agent(issue) {
            self.backends.remove(issue);
            self.drop_preview_sizes_for(issue);
        }
    }

    /// `(live-agents, needs-you)` counts for the header summary. "Agents" counts
    /// only *live* nodes, not the terminal Stopped/Done/Failed entries that
    /// linger in `fleet` until reaped — so the number drops the instant you stop
    /// or finish one.
    pub fn fleet_summary(&self) -> (usize, usize) {
        let agents = self.fleet.values().filter(|s| s.is_live()).count();
        let needs_you = self.fleet.values().filter(|s| s.needs_you()).count();
        (agents, needs_you)
    }

    /// Whether `issue`'s agent has reached a terminal state (the process is gone).
    fn is_terminal(&self, issue: &str) -> bool {
        matches!(
            self.fleet.get(issue),
            Some(AgentStatus::Stopped | AgentStatus::Done | AgentStatus::Failed)
        )
    }

    /// Whether anything on screen is animating — a live agent's spinner/pulse, an
    /// unexpired node flash, or an in-flight auto-resume. The render loop arms its
    /// animation tick only when this holds.
    pub fn is_animating(&self) -> bool {
        !self.resuming.is_empty()
            || self.flash.values().any(|&(_, until)| self.frame < until)
            || self.fleet.values().any(AgentStatus::is_animating)
    }

    /// How many docked agents are still mid-resume — drives the "resuming N…"
    /// header span.
    pub fn resuming_count(&self) -> usize {
        self.resuming.len()
    }

    /// Test seam: mark `issue` as mid-resume. Production arms this through
    /// `resume_one`, which needs a live supervisor.
    #[cfg(test)]
    pub fn mark_resuming_for_test(&mut self, issue: &str) {
        self.resuming
            .insert(issue.to_string(), self.frame + RESUME_GRACE_FRAMES);
    }

    /// Advance the animation frame and drop any expired flash. Also hard-clears a
    /// stuck "resuming…" spinner past its grace bound, so a wedged resume can't
    /// pin the cockpit awake forever.
    pub fn tick_frame(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        let now = self.frame;
        self.flash.retain(|_, &mut (_, until)| now < until);
        // Drop each wedged resume on its own grace bound, so a stuck spawn can't
        // pin the loop awake — independently of how many other resumes arrive.
        self.resuming.retain(|_, &mut deadline| now < deadline);
    }

    // ── Agents roster (the Spine's "AGENTS" tab) ────────────────────────────────

    /// The agents roster: every issue with an agent, ordered by salience —
    /// needs-you first, then live work, then idle, then the terminal states that
    /// linger until reaped. Ties break on the natural issue id.
    pub fn agent_order(&self) -> Vec<String> {
        let mut agents: Vec<(&String, AgentStatus)> =
            self.fleet.iter().map(|(k, s)| (k, *s)).collect();
        agents.sort_by(|(ka, sa), (kb, sb)| {
            sa.salience_rank()
                .cmp(&sb.salience_rank())
                .then_with(|| natural_key_cmp(ka, kb))
        });
        agents.into_iter().map(|(k, _)| k.clone()).collect()
    }

    /// Step the roster cursor by `delta` (wrapping) and re-aim the selection at
    /// the agent it lands on, so the detail bar follows. No-op when empty.
    fn move_roster(&mut self, delta: i32) {
        let agents = self.agent_order();
        if agents.is_empty() {
            return;
        }
        let next = match agents.iter().position(|k| *k == self.root) {
            Some(i) => (i as i32 + delta).rem_euclid(agents.len() as i32) as usize,
            None if delta >= 0 => 0,
            None => agents.len() - 1,
        };
        self.aim_spine(agents[next].clone());
    }

    /// Flip the Spine between the issue list and the agents roster.
    fn toggle_roster(&mut self) {
        self.left_view = match self.left_view {
            LeftView::Issues => LeftView::Agents,
            LeftView::Agents => LeftView::Issues,
        };
        if self.left_view == LeftView::Agents && self.fleet.is_empty() {
            self.status_msg = Some("no agents yet — Enter on an issue to open one".into());
        }
    }

    // ── Spine jumps ──────────────────────────────────────────────────────────

    fn jump_to_cycle(&mut self) {
        let members = self.graph.cycle_members();
        if members.is_empty() {
            self.status_msg = Some("no dependency cycles 🎉".into());
            return;
        }
        let next = match members.iter().position(|k| *k == self.root) {
            Some(i) => (i + 1) % members.len(),
            None => 0,
        };
        let (key, n, total) = (members[next].clone(), next + 1, members.len());
        self.aim_spine(key.clone());
        self.status_msg = Some(format!("cycle {n}/{total} — {key}{}", self.hidden_note()));
    }

    /// Jump to the next issue whose agent needs you, in display order, wrapping.
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
        let (key, n, total) = (members[next].clone(), next + 1, members.len());
        self.aim_spine(key.clone());
        // If this needy issue is already a pinned coin, go straight to it (chat
        // face) so you can respond; otherwise the preview follows the selection.
        self.focus_pinned_chat(&key);
        self.status_msg = Some(format!(
            "needs you {n}/{total} — {key}{}",
            self.hidden_note()
        ));
    }

    /// A status suffix flagging that a jump landed on an issue the active
    /// filter/search hides — so the empty list highlight reads as deliberate.
    fn hidden_note(&self) -> &'static str {
        if self.root_is_hidden() {
            " · hidden by filter (clear it to list)"
        } else {
            ""
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

/// Map a window to its persistable identity, or `None` for the Spine (never
/// persisted — it's recreated by `WindowSet::new`).
fn window_to_persisted(w: &crate::window::Window) -> Option<PersistedWindow> {
    match &w.kind {
        // The Spine is recreated by `WindowSet::new`. The preview (an unpinned
        // coin) is transient and re-seeded on restore — neither is persisted.
        WindowKind::Spine => None,
        WindowKind::Coin { issue, mode } => {
            if !w.pinned {
                return None; // the transient preview
            }
            // A coin persists by its current face, so it restores showing the same
            // side; `references_agent` keeps its backend alive across a flip.
            let kind = match mode {
                CoinMode::Chat => PersistedKind::Agent,
                CoinMode::Deps => PersistedKind::Deps,
            };
            Some(PersistedWindow {
                kind,
                issue: Some(issue.clone()),
            })
        }
        WindowKind::Fleet => Some(PersistedWindow {
            kind: PersistedKind::Fleet,
            issue: None,
        }),
    }
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
/// `natural_key_cmp` breaks ties.
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

/// Sort rank that surfaces live work first.
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
    use crate::backend::fake::FakeBackend;
    use crate::demo;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn app() -> App {
        let mut a = App::new(demo::graph());
        // A generous viewport so opened windows are placed/visible in tests.
        a.set_viewport(Rect::new(0, 0, 200, 40));
        a
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    /// Press the prefix (`Ctrl-a`) then `code` — i.e. invoke a window verb.
    fn verb(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        press(app, code);
    }

    fn register(app: &mut App, key: &str) -> Arc<FakeBackend> {
        let fake = FakeBackend::new(key);
        app.backends
            .insert(key.into(), fake.clone() as Arc<dyn AgentBackend>);
        fake
    }

    // ── The spine ──────────────────────────────────────────────────────────

    #[test]
    fn default_selection_is_the_most_connected_issue() {
        assert_eq!(app().root, "ZAP-204");
    }

    #[test]
    fn the_cockpit_opens_with_a_preview_coin_on_the_selection() {
        let app = app();
        // Spine + the transient preview coin at index 1, in deps face (chat-first
        // falls back since the default selection has no agent yet).
        assert_eq!(app.windows.windows.len(), 2);
        assert!(matches!(app.windows.windows[0].kind, WindowKind::Spine));
        assert!(matches!(
            app.windows.windows[1].kind,
            WindowKind::Coin {
                mode: CoinMode::Deps,
                ..
            }
        ));
        assert_eq!(
            app.windows.windows[1].deps.as_ref().unwrap().root,
            "ZAP-204"
        );
        assert!(
            !app.windows.windows[1].pinned,
            "the preview coin is transient (never pinned)"
        );
        assert_eq!(app.windows.focus, 0, "focus opens on the spine");
    }

    #[test]
    fn arrowing_the_spine_reaims_the_preview_coin_live() {
        let mut app = app();
        assert_eq!(app.windows.preview().unwrap().0, "ZAP-204");
        press(&mut app, KeyCode::Down); // move the selection
        let sel = app.root.clone();
        assert_eq!(
            app.windows.preview().unwrap().0,
            sel,
            "the preview coin follows the nav selection live"
        );
    }

    #[test]
    fn jump_needs_you_reaims_even_a_focused_preview() {
        // Regression: `Ctrl-a n` (JumpNeedsYou) fired while the PREVIEW is focused
        // must re-aim the preview to the needy issue — not leave the focused pane
        // stuck on the old one. (reaim_preview used to bail when the preview was
        // focused, which dropped the re-aim for a verb-driven jump.)
        let mut app = app();
        let start = app.root.clone();
        let target = app.order.iter().find(|k| **k != start).unwrap().clone();
        app.fleet.insert(target.clone(), AgentStatus::NeedsYou);
        // Focus the preview (Ctrl-a l from the spine).
        verb(&mut app, KeyCode::Char('l'));
        assert_eq!(app.windows.focus, app.windows.preview_index().unwrap());
        assert_eq!(app.windows.preview().unwrap().0, start);
        // Ctrl-a n jumps to the needy issue.
        verb(&mut app, KeyCode::Char('n'));
        assert_eq!(app.root, target, "the selection jumped to the needy issue");
        assert_eq!(
            app.windows.focused_kind().coin().map(|(i, _)| i),
            Some(target.as_str()),
            "the focused preview re-aimed to the jumped-to issue, not the stale one"
        );
    }

    #[test]
    fn chat_first_picks_chat_for_a_live_agent_else_deps() {
        let mut app = app();
        // A plain issue with no agent → the preview shows its deps.
        assert_eq!(app.default_preview_mode("ZAP-210"), CoinMode::Deps);
        // A live agent → chat-first.
        app.fleet.insert("ZAP-210".into(), AgentStatus::Running);
        assert_eq!(app.default_preview_mode("ZAP-210"), CoinMode::Chat);
        // …unless a coin for it is already a pinned tab, then preview deps instead.
        app.windows.push(
            WindowKind::Coin {
                issue: "ZAP-210".into(),
                mode: CoinMode::Chat,
            },
            true,
            None,
        );
        assert_eq!(app.default_preview_mode("ZAP-210"), CoinMode::Deps);
    }

    #[test]
    fn tab_flips_the_preview_coin_chat_and_deps() {
        let mut app = app();
        assert_eq!(app.windows.preview().unwrap().1, CoinMode::Deps);
        press(&mut app, KeyCode::Tab); // bare Tab on the spine flips the active coin
        assert_eq!(app.windows.preview().unwrap().1, CoinMode::Chat);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.windows.preview().unwrap().1, CoinMode::Deps);
    }

    #[test]
    fn list_navigation_re_aims_the_selection() {
        let mut app = app();
        let before = app.root.clone();
        press(&mut app, KeyCode::Down);
        assert_ne!(app.root, before, "Down re-aims the selection");
        assert_eq!(
            app.order[app.list_state.selected().unwrap()],
            app.root,
            "the highlight tracks the selection"
        );
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
        assert!(app.order.len() > 1);
    }

    #[test]
    fn the_roster_tab_drives_the_selection_through_agents() {
        let mut app = app();
        let k: Vec<String> = app.order.iter().take(2).cloned().collect();
        app.fleet.insert(k[0].clone(), AgentStatus::Idle);
        app.fleet.insert(k[1].clone(), AgentStatus::NeedsYou);
        press(&mut app, KeyCode::Char('r')); // flip to the roster
        assert_eq!(app.left_view, LeftView::Agents);
        press(&mut app, KeyCode::Down);
        let first = app.root.clone();
        assert!(app.fleet.contains_key(&first));
        press(&mut app, KeyCode::Down);
        assert_ne!(first, app.root, "each step lands on a different agent");
    }

    // ── The attach/spawn button + agent windows ─────────────────────────────

    #[test]
    fn the_button_opens_an_agent_window_for_an_existing_backend() {
        let mut app = app();
        register(&mut app, "ZAP-204");
        app.fleet.insert("ZAP-204".into(), AgentStatus::Running);
        // root is ZAP-204; the spine is focused; Enter is the button.
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            app.windows.focused_kind().agent_issue(),
            Some("ZAP-204"),
            "the button opens + focuses the agent window"
        );
    }

    #[test]
    fn opening_the_same_agent_twice_does_not_duplicate() {
        let mut app = app();
        register(&mut app, "ZAP-204");
        press(&mut app, KeyCode::Enter); // opens the agent window, focuses it
        let n = app.windows.windows.len();
        verb(&mut app, KeyCode::Char('h')); // focus back toward the spine
        press(&mut app, KeyCode::Enter); // button again on the same selection
        assert_eq!(app.windows.windows.len(), n, "no duplicate window");
    }

    #[test]
    fn keys_go_to_a_focused_agents_pty_and_the_prefix_escapes() {
        let mut app = app();
        let fake = register(&mut app, "ZAP-204");
        press(&mut app, KeyCode::Enter); // focus the agent window
        assert_eq!(app.windows.focused_kind().agent_issue(), Some("ZAP-204"));
        // A normal key now drives the agent, not the cockpit ('q' would otherwise
        // be nothing on the spine, but here it must reach the PTY).
        press(&mut app, KeyCode::Char('q'));
        assert!(!app.should_quit);
        assert_eq!(fake.inputs.lock().unwrap().last().unwrap(), b"q");
    }

    #[test]
    fn a_double_prefix_sends_the_literal_chord_to_the_agent() {
        let mut app = app();
        let fake = register(&mut app, "ZAP-204");
        press(&mut app, KeyCode::Enter); // focus the agent
        // Ctrl-a Ctrl-a → one literal Ctrl-A (0x01) to the PTY, not a verb.
        app.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(!app.prefix_armed);
        assert_eq!(fake.inputs.lock().unwrap().last().unwrap(), &vec![0x01]);
    }

    #[test]
    fn moving_focus_off_the_spine_during_search_routes_keys_to_the_window() {
        // Regression: an open search must not keep capturing keys once focus moves
        // to an agent window — the key has to reach the PTY, not the search buffer.
        let mut app = app();
        let fake = register(&mut app, "ZAP-204");
        app.root = "ZAP-204".into();
        // Start a search on the spine, then open + focus the agent via the prefix
        // button (reachable mid-search since the prefix arms before the search
        // capture).
        press(&mut app, KeyCode::Char('/'));
        assert!(app.search_active);
        verb(&mut app, KeyCode::Enter); // Ctrl-a Enter = AttachOrSpawn (the button)
        assert_eq!(app.windows.focused_kind().agent_issue(), Some("ZAP-204"));
        // A plain key now reaches the agent's PTY; the search committed itself.
        press(&mut app, KeyCode::Char('q'));
        assert!(
            !app.search_active,
            "the search committed when focus left the spine"
        );
        assert_eq!(
            fake.inputs.lock().unwrap().last().unwrap(),
            b"q",
            "the key reached the agent, not the search buffer"
        );
        assert!(!app.should_quit, "q went to the agent, not Quit");
    }

    // ── Window verbs: focus / close / kill / pin / layout / zoom ─────────────

    #[test]
    fn close_undocks_a_graduated_tab_and_keeps_a_live_agent_running() {
        let mut app = app();
        register(&mut app, "ZAP-204");
        app.fleet.insert("ZAP-204".into(), AgentStatus::Running);
        press(&mut app, KeyCode::Enter); // active window → chat on ZAP-204
        verb(&mut app, KeyCode::Char('p')); // pin = graduate to a permanent tab
        verb(&mut app, KeyCode::Char('w')); // Ctrl-a w = close the tab
        assert!(
            app.windows.pinned_coin_index("ZAP-204").is_none(),
            "the tab is undocked"
        );
        assert!(
            app.backends.contains_key("ZAP-204"),
            "a live agent keeps running (its backend is kept for re-find)"
        );
    }

    #[test]
    fn close_reclaims_a_dead_agents_backend() {
        let mut app = app();
        let fake = register(&mut app, "ZAP-204");
        fake.finish(Some(0)); // the agent exited
        press(&mut app, KeyCode::Enter); // active window → its (EXITED) chat
        verb(&mut app, KeyCode::Char('p')); // graduate to a tab
        verb(&mut app, KeyCode::Char('w')); // close it
        assert!(
            !app.backends.contains_key("ZAP-204"),
            "a dead, unreferenced agent's handle is reclaimed on close"
        );
    }

    #[test]
    fn the_preview_cannot_be_closed() {
        let mut app = app();
        verb(&mut app, KeyCode::Char('l')); // focus the preview coin
        assert!(app.windows.focused().is_preview());
        verb(&mut app, KeyCode::Char('w')); // close is refused — it's structural
        assert!(app.windows.focused().is_preview(), "the preview stays");
        assert!(
            app.status_msg
                .as_deref()
                .unwrap()
                .contains("can't be closed")
        );
    }

    #[test]
    fn closing_the_spine_is_a_no_op() {
        let mut app = app();
        app.windows.focus = 0;
        verb(&mut app, KeyCode::Char('w'));
        assert!(matches!(app.windows.windows[0].kind, WindowKind::Spine));
    }

    #[test]
    fn kill_is_confirmed_and_separate_from_close() {
        let mut app = app();
        register(&mut app, "ZAP-204");
        app.fleet.insert("ZAP-204".into(), AgentStatus::Running);
        press(&mut app, KeyCode::Enter); // focus the live agent
        verb(&mut app, KeyCode::Char('x')); // Ctrl-a x = kill (arms confirm)
        assert_eq!(app.kill_confirm.as_deref(), Some("ZAP-204"));
        // A non-confirming key cancels; the window survives.
        press(&mut app, KeyCode::Char('z'));
        assert!(app.kill_confirm.is_none(), "kill cancelled");
        assert!(app.windows.references_agent("ZAP-204"));
    }

    #[test]
    fn kill_is_refused_when_no_live_agent() {
        let mut app = app();
        // Focus the preview (deps face at startup) for an issue with no agent.
        verb(&mut app, KeyCode::Char('l'));
        assert!(matches!(
            app.windows.focused_kind(),
            WindowKind::Coin {
                mode: CoinMode::Deps,
                ..
            }
        ));
        verb(&mut app, KeyCode::Char('x'));
        assert!(app.kill_confirm.is_none());
        assert!(
            app.status_msg.as_deref().unwrap().contains("not running"),
            "kill is refused when the coin's issue has no live agent"
        );
    }

    #[test]
    fn focus_moves_left_and_right_across_windows() {
        let mut app = app();
        register(&mut app, "ZAP-204");
        press(&mut app, KeyCode::Enter); // active coin → chat on ZAP-204 (the preview)
        verb(&mut app, KeyCode::Char('p')); // graduate → [Spine, Agent], focus=1
        // The selection (ZAP-204) is now itself a pinned coin, so no duplicate
        // preview is seeded — the pinned coin IS the active view.
        assert_eq!(app.windows.windows.len(), 2);
        assert_eq!(app.windows.focus, 1);
        verb(&mut app, KeyCode::Char('h')); // focus left → spine
        assert_eq!(app.windows.focus, 0);
        verb(&mut app, KeyCode::Char('h')); // no wrap past the spine
        assert_eq!(app.windows.focus, 0);
        verb(&mut app, KeyCode::Char('l')); // focus right → the agent
        assert_eq!(app.windows.focus, 1);
    }

    #[test]
    fn pin_graduates_a_tab_that_survives_browsing() {
        let mut app = app();
        register(&mut app, "ZAP-204");
        register(&mut app, "ZAP-205");
        app.aim_spine("ZAP-204".into());
        press(&mut app, KeyCode::Enter); // active window → chat on ZAP-204
        verb(&mut app, KeyCode::Char('p')); // pin = graduate to a permanent tab
        assert!(
            app.windows.pinned_coin_index("ZAP-204").is_some(),
            "ZAP-204 is now a permanent tab"
        );
        app.aim_spine("ZAP-205".into());
        // The prefix button opens ZAP-205 from any focus; pinned ZAP-204 survives.
        verb(&mut app, KeyCode::Enter);
        assert!(app.windows.pinned_coin_index("ZAP-204").is_some());
        assert_eq!(
            app.windows.preview().unwrap(),
            ("ZAP-205".into(), CoinMode::Chat)
        );
    }

    #[test]
    fn layout_toggle_forces_rail_and_mosaic() {
        let mut app = app();
        // A fresh cockpit auto-tiles (mosaic); `Ctrl-a |` forces the other mode.
        assert_eq!(app.windows.layout, LayoutMode::Mosaic);
        verb(&mut app, KeyCode::Char('|'));
        assert_eq!(app.windows.layout, LayoutMode::Rail);
        verb(&mut app, KeyCode::Char('|'));
        assert_eq!(app.windows.layout, LayoutMode::Mosaic);
    }

    #[test]
    fn quit_is_only_reachable_through_the_prefix() {
        let mut app = app();
        press(&mut app, KeyCode::Char('q')); // a bare q does nothing on the spine
        assert!(!app.should_quit);
        verb(&mut app, KeyCode::Char('q')); // Ctrl-a q quits
        assert!(app.should_quit);
    }

    // ── Deps windows (per-window navigation) ─────────────────────────────────

    #[test]
    fn a_focused_deps_window_re_roots_and_back_returns() {
        let mut app = app();
        verb(&mut app, KeyCode::Char('l')); // focus the deps window
        let cursor = app.windows.focused().deps.as_ref().unwrap();
        let target = cursor.up_rows[0].key.clone();
        press(&mut app, KeyCode::Enter); // re-root onto the first blocker
        assert_eq!(app.windows.focused().deps.as_ref().unwrap().root, target);
        press(&mut app, KeyCode::Char('b')); // back
        assert_eq!(app.windows.focused().deps.as_ref().unwrap().root, "ZAP-204");
    }

    #[test]
    fn deps_windows_navigate_independently() {
        let mut app = app();
        // Graduate a pinned deps tab on ZAP-204, then re-aim the context to ZAP-210.
        verb(&mut app, KeyCode::Char('l')); // focus the context window (deps ZAP-204)
        verb(&mut app, KeyCode::Char('p')); // pin = graduate a permanent deps tab
        app.windows.focus = 0; // back to the spine
        app.aim_spine("ZAP-210".into()); // the fresh context re-roots to ZAP-210
        let ctx = app.windows.preview_index().unwrap();
        assert_eq!(
            app.windows.windows[ctx].deps.as_ref().unwrap().root,
            "ZAP-210",
            "the preview coin roots at the new selection"
        );
        // The graduated tab still roots at ZAP-204 — independent navigation.
        let pinned = app
            .windows
            .windows
            .iter()
            .find(|w| {
                matches!(
                    w.kind,
                    WindowKind::Coin {
                        mode: CoinMode::Deps,
                        ..
                    }
                ) && w.pinned
            })
            .unwrap();
        assert_eq!(pinned.deps.as_ref().unwrap().root, "ZAP-204");
    }

    #[test]
    fn open_fleet_opens_a_single_overview_window() {
        let mut app = app();
        press(&mut app, KeyCode::Char('g')); // open the fleet map
        assert!(matches!(app.windows.focused_kind(), WindowKind::Fleet));
        // A second `g` focuses the same one — there's only ever one Fleet window.
        let n = app.windows.windows.len();
        app.windows.focus = 0;
        press(&mut app, KeyCode::Char('g'));
        assert_eq!(app.windows.windows.len(), n);
    }

    // ── Fleet summary + the preserved late-hook / tombstone invariants ───────

    #[test]
    fn fleet_summary_counts_only_live_agents() {
        let mut app = app();
        app.fleet.insert("ZAP-201".into(), AgentStatus::Running);
        app.fleet.insert("ZAP-204".into(), AgentStatus::Idle);
        app.fleet.insert("ZAP-205".into(), AgentStatus::Done);
        app.fleet.insert("ZAP-210".into(), AgentStatus::Failed);
        app.fleet.insert("ZAP-240".into(), AgentStatus::NeedsYou);
        assert_eq!(app.fleet_summary(), (3, 1));
    }

    #[test]
    fn a_dashboard_keypress_acknowledges_the_needs_you_footer() {
        let mut app = app();
        app.apply_event(AppEvent::AgentNeedsYou {
            issue: "ZAP-204".into(),
            reason: "permission".into(),
        });
        assert!(app.needs_you_alert);
        press(&mut app, KeyCode::Char('f')); // a spine key acknowledges
        assert!(!app.needs_you_alert);
        app.apply_event(AppEvent::AgentAction {
            issue: "ZAP-204".into(),
            action: "ran Grep".into(),
        });
        assert_eq!(app.status_msg.as_deref(), Some("ZAP-204: ran Grep"));
    }

    #[test]
    fn post_tool_use_does_not_clear_a_pending_needs_you() {
        let mut app = app();
        app.apply_event(AppEvent::AgentNeedsYou {
            issue: "ZAP-204".into(),
            reason: "permission".into(),
        });
        let alert = app.status_msg.clone();
        app.apply_event(AppEvent::AgentAction {
            issue: "ZAP-204".into(),
            action: "ran Bash".into(),
        });
        assert_eq!(app.fleet.get("ZAP-204"), Some(&AgentStatus::NeedsYou));
        assert_eq!(app.status_msg, alert, "chatter must not bury the alert");
    }

    #[test]
    fn a_late_hook_cannot_resurrect_a_terminated_agent() {
        let mut app = app();
        app.fleet.insert("ZAP-204".into(), AgentStatus::Done);
        assert!(!app.apply_event(AppEvent::AgentAction {
            issue: "ZAP-204".into(),
            action: "ran grep".into(),
        }));
        assert!(!app.apply_event(AppEvent::AgentNeedsYou {
            issue: "ZAP-204".into(),
            reason: "late prompt".into(),
        }));
        assert!(!app.apply_event(AppEvent::AgentStatusChanged {
            issue: "ZAP-204".into(),
            status: AgentStatus::Idle,
        }));
        assert_eq!(app.fleet.get("ZAP-204"), Some(&AgentStatus::Done));
        assert_eq!(app.fleet_summary(), (0, 0));
    }

    #[test]
    fn a_late_hook_cannot_resurrect_a_reaped_agent() {
        let mut app = app();
        let fake = FakeBackend::new("ZAP-204");
        app.apply_event(AppEvent::AgentSpawned {
            issue: "ZAP-204".into(),
            backend: fake as Arc<dyn AgentBackend>,
        });
        app.apply_event(AppEvent::AgentStatusChanged {
            issue: "ZAP-204".into(),
            status: AgentStatus::Done,
        });
        app.apply_event(AppEvent::AgentReaped {
            issue: "ZAP-204".into(),
        });
        assert!(
            !app.fleet.contains_key("ZAP-204"),
            "the reaped agent is gone"
        );
        // All three late hooks are ignored.
        assert!(!app.apply_event(AppEvent::AgentNeedsYou {
            issue: "ZAP-204".into(),
            reason: "late".into(),
        }));
        assert!(!app.apply_event(AppEvent::AgentAction {
            issue: "ZAP-204".into(),
            action: "ran grep".into(),
        }));
        assert!(!app.apply_event(AppEvent::AgentStatusChanged {
            issue: "ZAP-204".into(),
            status: AgentStatus::Idle,
        }));
        assert!(!app.fleet.contains_key("ZAP-204"));
        assert!(!app.needs_you_alert, "no phantom sticky alert");
        // A genuine relaunch clears the tombstone.
        let fake2 = FakeBackend::new("ZAP-204");
        app.apply_event(AppEvent::AgentSpawned {
            issue: "ZAP-204".into(),
            backend: fake2 as Arc<dyn AgentBackend>,
        });
        assert!(app.apply_event(AppEvent::AgentNeedsYou {
            issue: "ZAP-204".into(),
            reason: "real".into(),
        }));
        assert_eq!(app.fleet.get("ZAP-204"), Some(&AgentStatus::NeedsYou));
    }

    #[test]
    fn agent_exited_reclaims_an_unreferenced_backend_but_keeps_a_windowed_one() {
        // No window references it → AgentExited reclaims the dead handle.
        {
            let mut app = app();
            let fake = FakeBackend::new("ZAP-201");
            app.backends
                .insert("ZAP-201".into(), fake as Arc<dyn AgentBackend>);
            app.fleet.insert("ZAP-201".into(), AgentStatus::Running);
            app.apply_event(AppEvent::AgentExited {
                issue: "ZAP-201".into(),
                code: Some(1),
            });
            assert!(
                !app.backends.contains_key("ZAP-201"),
                "an unreferenced dead PTY handle is reclaimed"
            );
            // Status stays the supervisor's authority (unchanged by AgentExited).
            assert_eq!(app.fleet.get("ZAP-201"), Some(&AgentStatus::Running));
        }

        // A windowed agent keeps its handle (its EXITED card) until the window closes.
        let mut app = app();
        register(&mut app, "ZAP-204");
        press(&mut app, KeyCode::Enter); // open a window referencing ZAP-204
        app.apply_event(AppEvent::AgentExited {
            issue: "ZAP-204".into(),
            code: Some(0),
        });
        assert!(
            app.backends.contains_key("ZAP-204"),
            "a windowed agent keeps its final screen"
        );
    }

    #[test]
    fn agent_output_repaints_only_when_its_window_is_visible() {
        let mut app = app();
        register(&mut app, "ZAP-205");
        app.root = "ZAP-205".into();
        press(&mut app, KeyCode::Enter); // open + focus ZAP-205's window
        assert!(
            app.apply_event(AppEvent::AgentOutput {
                issue: "ZAP-205".into()
            }),
            "a visible agent's output forces a redraw"
        );
        // An agent with no window changes nothing visible.
        assert!(!app.apply_event(AppEvent::AgentOutput {
            issue: "ZAP-999".into()
        }));
    }

    #[test]
    fn spawning_for_a_pending_button_opens_and_focuses_the_window() {
        let mut app = app();
        app.pending_attach = Some("ZAP-205".into());
        let fake = FakeBackend::new("ZAP-205");
        app.apply_event(AppEvent::AgentSpawned {
            issue: "ZAP-205".into(),
            backend: fake as Arc<dyn AgentBackend>,
        });
        assert!(app.pending_attach.is_none());
        assert_eq!(app.windows.focused_kind().agent_issue(), Some("ZAP-205"));
        assert!(app.flash.contains_key("ZAP-205"), "a launch flash is set");
    }

    #[test]
    fn a_background_spawn_does_not_steal_focus() {
        let mut app = app();
        let focus_before = app.windows.focus;
        let fake = FakeBackend::new("ZAP-205");
        app.apply_event(AppEvent::AgentSpawned {
            issue: "ZAP-205".into(),
            backend: fake as Arc<dyn AgentBackend>,
        });
        // No pending_attach / pending_launch → the roster gains it, focus stays put.
        assert_eq!(app.windows.focus, focus_before);
        assert!(!app.windows.references_agent("ZAP-205"));
        assert_eq!(app.fleet.get("ZAP-205"), Some(&AgentStatus::Spawning));
    }

    #[test]
    fn agent_order_sorts_by_salience_not_id() {
        let mut app = app();
        let k: Vec<String> = app.order.iter().take(4).cloned().collect();
        app.fleet.insert(k[0].clone(), AgentStatus::Done);
        app.fleet.insert(k[1].clone(), AgentStatus::NeedsYou);
        app.fleet.insert(k[2].clone(), AgentStatus::Running);
        app.fleet.insert(k[3].clone(), AgentStatus::Idle);
        assert_eq!(
            app.agent_order(),
            vec![k[1].clone(), k[2].clone(), k[3].clone(), k[0].clone()]
        );
    }

    #[test]
    fn is_animating_is_false_for_only_resting_agents() {
        let mut app = app();
        assert!(!app.is_animating());
        app.fleet.insert("ZAP-204".into(), AgentStatus::Idle);
        app.fleet.insert("ZAP-205".into(), AgentStatus::Done);
        assert!(!app.is_animating());
        app.fleet.insert("ZAP-240".into(), AgentStatus::Running);
        assert!(app.is_animating(), "a working agent drives the tick");
    }

    #[test]
    fn resuming_keeps_the_cockpit_animating_then_the_grace_clears_it() {
        let mut app = app();
        // An in-flight resume keeps the loop awake…
        app.mark_resuming_for_test("ZAP-1");
        app.mark_resuming_for_test("ZAP-2");
        assert!(app.is_animating());
        // …but a stuck resume can't pin it forever: past the grace it self-clears.
        for _ in 0..=RESUME_GRACE_FRAMES {
            app.tick_frame();
        }
        assert_eq!(
            app.resuming_count(),
            0,
            "the grace bound cleared a stuck resume"
        );
        assert!(!app.is_animating());
    }

    #[test]
    fn a_wedged_resume_clears_on_its_own_grace_despite_a_trickle_of_new_ones() {
        // Regression for the single-shared-deadline bug: a stuck resume must time
        // out on its OWN grace, not be kept alive by a steady trickle of later
        // resumes pushing one shared deadline forever forward.
        let mut app = app();
        app.mark_resuming_for_test("WEDGED"); // armed at frame 0 → deadline = GRACE
        for f in 1..=RESUME_GRACE_FRAMES {
            app.tick_frame();
            if f == RESUME_GRACE_FRAMES / 2 {
                app.mark_resuming_for_test("FRESH"); // a later, still-valid deadline
            }
        }
        assert!(
            !app.resuming.contains_key("WEDGED"),
            "the wedged resume cleared on its own grace"
        );
        assert!(
            app.resuming.contains_key("FRESH"),
            "a newer resume keeps its own (later) grace and the spinner"
        );
        assert!(
            app.is_animating(),
            "the loop stays awake for the live resume"
        );
    }

    #[test]
    fn an_agent_spawn_settles_its_own_resume() {
        let mut app = app();
        app.mark_resuming_for_test("ZAP-205");
        app.mark_resuming_for_test("ZAP-206");
        let fake = FakeBackend::new("ZAP-205");
        app.apply_event(AppEvent::AgentSpawned {
            issue: "ZAP-205".into(),
            backend: fake as Arc<dyn AgentBackend>,
        });
        assert_eq!(
            app.resuming_count(),
            1,
            "the spawned agent settles its own resume"
        );
        assert!(
            app.resuming.contains_key("ZAP-206"),
            "the other in-flight resume is untouched"
        );
    }

    #[test]
    fn tick_frame_advances_and_expires_flashes() {
        let mut app = app();
        app.flash.insert("ZAP-204".into(), (Flash::Launched, 3));
        assert!(app.is_animating());
        for _ in 0..3 {
            app.tick_frame();
        }
        assert!(!app.is_animating());
        assert!(app.flash.is_empty());
    }

    // ── Rail: the focused window is always the big pane ──────────────────────

    /// Graduate several pinned agent tabs (carded in the rail) so there's more
    /// than just the Spine, the context window and one tab.
    fn railed_strip() -> App {
        let mut app = app();
        for k in ["ZAP-1", "ZAP-2", "ZAP-3"] {
            register(&mut app, k);
            app.windows.push(
                WindowKind::Coin {
                    issue: k.into(),
                    mode: CoinMode::Chat,
                },
                true,
                None,
            );
        }
        // Force the rail so these tests exercise big-pane/card semantics even
        // though three docked coins would otherwise auto-tile (mosaic).
        app.windows.force_layout(LayoutMode::Rail);
        app.windows.focus = 0;
        app
    }

    #[test]
    fn opening_fleet_makes_it_the_big_pane() {
        let mut app = railed_strip();
        press(&mut app, KeyCode::Char('g')); // OpenFleet from the spine
        assert!(
            matches!(app.windows.focused_kind(), WindowKind::Fleet),
            "the Fleet window is focused"
        );
        // The focused non-spine window is always the rail's big pane → visible.
        assert!(
            app.is_index_visible(app.windows.focus),
            "the just-opened Fleet window is the big pane"
        );
    }

    #[test]
    fn the_focused_window_is_the_big_pane_after_a_zoom_round_trip() {
        let mut app = railed_strip();
        app.windows.focus = 2;
        verb(&mut app, KeyCode::Char('z')); // zoom the big pane
        verb(&mut app, KeyCode::Right); // move focus while zoomed
        verb(&mut app, KeyCode::Char('z')); // un-zoom
        assert!(!app.windows.zoomed);
        assert!(
            app.is_index_visible(app.windows.focus),
            "the focused window is the big pane after un-zoom"
        );
    }

    #[test]
    fn a_carded_agent_is_not_visible_for_polling() {
        // A pinned agent shown only as a rail card (not the big pane) must NOT
        // force a fast poll / repaint — the idle-quiet property.
        let mut app = railed_strip(); // focus on the Spine → context is the big pane
        app.fleet.insert("ZAP-1".into(), AgentStatus::Running);
        // ZAP-1 is a carded tab, not the big pane.
        assert!(
            !app.is_agent_visible("ZAP-1"),
            "a carded agent isn't a live PTY"
        );
        assert!(
            !app.has_visible_live_agent(),
            "no carded agent pins the 16ms loop"
        );
        // Focus it → it becomes the big pane and is now visible.
        app.windows.focus = app.windows.pinned_coin_index("ZAP-1").unwrap();
        assert!(app.is_agent_visible("ZAP-1"));
    }

    // ── Persistence (Phase 5) ────────────────────────────────────────────────

    #[test]
    fn cockpit_snapshot_persists_only_docked_windows() {
        let mut app = app(); // [Spine, Context(ZAP-204)]
        register(&mut app, "ZAP-204");
        press(&mut app, KeyCode::Enter); // active window → chat on ZAP-204
        verb(&mut app, KeyCode::Char('p')); // pin = graduate to a permanent tab
        let state = app.snapshot_cockpit();
        // The graduated agent is docked; the transient context window is never
        // persisted (even though a fresh one now sits at index 1).
        assert_eq!(state.windows.len(), 1);
        assert_eq!(state.windows[0].kind, PersistedKind::Agent);
        assert_eq!(state.windows[0].issue.as_deref(), Some("ZAP-204"));
        assert_eq!(state.layout, "mosaic", "one docked coin still tiles");
        assert_eq!(state.focus.unwrap().issue.as_deref(), Some("ZAP-204"));
    }

    #[test]
    fn apply_cockpit_restores_docked_windows_and_prunes_dead_ones() {
        let state = CockpitState {
            layout: "mosaic".into(),
            windows: vec![
                PersistedWindow {
                    kind: PersistedKind::Agent,
                    issue: Some("ZAP-204".into()),
                },
                PersistedWindow {
                    kind: PersistedKind::Agent,
                    issue: Some("GONE-1".into()),
                },
                PersistedWindow {
                    kind: PersistedKind::Deps,
                    issue: Some("ZAP-205".into()),
                },
                PersistedWindow {
                    kind: PersistedKind::Fleet,
                    issue: None,
                },
            ],
            focus: Some(PersistedWindow {
                kind: PersistedKind::Deps,
                issue: Some("ZAP-205".into()),
            }),
            ..CockpitState::default()
        };
        let mut app = app();
        // Only ZAP-204 was live at save time; GONE-1 isn't resumable (it left, or
        // finished Done/Failed) so its Agent window must not be restored to a
        // permanent "resuming…" card.
        let resumable: HashSet<String> = ["ZAP-204"].into_iter().map(String::from).collect();
        app.apply_cockpit(&state, &resumable);
        // Spine + 3 restored windows; the non-resumable agent (GONE-1) is pruned.
        // The default selection (ZAP-204) is itself a restored pinned coin, so no
        // duplicate preview is seeded — that coin is the active view.
        assert_eq!(app.windows.windows.len(), 4);
        assert!(
            app.windows.preview_index().is_none(),
            "no duplicate preview when the selection is already a pinned coin"
        );
        assert!(app.windows.references_agent("ZAP-204"));
        assert!(
            !app.windows.references_agent("GONE-1"),
            "a non-resumable agent window is pruned on restore"
        );
        assert_eq!(app.windows.layout, LayoutMode::Mosaic);
        assert!(
            matches!(
                app.windows.focused_kind(),
                WindowKind::Coin {
                    mode: CoinMode::Deps,
                    ..
                }
            ),
            "focus is restored by identity to the deps coin"
        );
    }

    #[test]
    fn apply_cockpit_prunes_a_deps_window_whose_root_left_the_graph() {
        let state = CockpitState {
            layout: "filmstrip".into(),
            windows: vec![
                PersistedWindow {
                    kind: PersistedKind::Deps,
                    issue: Some("ZAP-205".into()), // a real graph node — kept
                },
                PersistedWindow {
                    kind: PersistedKind::Deps,
                    issue: Some("GONE-DEPS".into()), // not in the graph — pruned
                },
            ],
            focus: None,
            ..CockpitState::default()
        };
        let mut app = app();
        app.apply_cockpit(&state, &HashSet::new());
        // Spine + the re-seeded context window + the one Deps tab whose root exists.
        assert_eq!(app.windows.windows.len(), 3);
        assert!(
            app.windows
                .windows
                .iter()
                .filter(|w| w.pinned)
                .all(|w| w.issue() != Some("GONE-DEPS")),
            "a Deps window whose root left the graph is pruned on restore"
        );
    }

    #[test]
    fn a_resumable_deps_coin_whose_root_left_the_graph_restores_chat_faced() {
        // Asymmetry guard: a coin pinned on its DEPS face must not strand a still-
        // resumable agent merely because its issue left the graph between sessions.
        // It restores on the CHAT face (a deps cursor needs a graph node we no
        // longer have) so the eager/lazy resume can revive it — exactly as a
        // chat-faced coin for the same issue would have survived.
        let state = CockpitState {
            layout: "mosaic".into(),
            windows: vec![PersistedWindow {
                kind: PersistedKind::Deps,
                issue: Some("GHOST-1".into()), // not in the graph any more…
            }],
            focus: None,
            ..CockpitState::default()
        };
        let mut app = app();
        // …but its agent session is still live (post-reconcile was-live set).
        let resumable: HashSet<String> = ["GHOST-1"].into_iter().map(String::from).collect();
        app.apply_cockpit(&state, &resumable);
        assert!(
            app.windows.references_agent("GHOST-1"),
            "the resumable agent is kept, not stranded"
        );
        let coin = app
            .windows
            .windows
            .iter()
            .find(|w| w.issue() == Some("GHOST-1"))
            .expect("the coin is restored");
        assert!(
            matches!(
                coin.kind,
                WindowKind::Coin {
                    mode: CoinMode::Chat,
                    ..
                }
            ),
            "restored on the chat face so it can resume"
        );
    }

    #[test]
    fn a_fully_pruned_layout_falls_back_to_the_default_strip() {
        // A persisted layout whose every window prunes (nothing resumable, roots
        // gone) should open like a pinless session — keeping the default context
        // window — not collapse to a bare spine.
        let state = CockpitState {
            layout: "filmstrip".into(),
            windows: vec![PersistedWindow {
                kind: PersistedKind::Agent,
                issue: Some("GONE".into()),
            }],
            focus: None,
            ..CockpitState::default()
        };
        let mut app = app();
        let before = app.windows.windows.len();
        app.apply_cockpit(&state, &HashSet::new()); // GONE isn't resumable → pruned
        assert_eq!(
            app.windows.windows.len(),
            before,
            "a fully-pruned layout keeps the fresh default strip"
        );
        assert!(
            app.windows.windows.len() > 1,
            "the default strip still has its context window, not just the spine"
        );
    }

    #[test]
    fn an_empty_persisted_layout_keeps_the_default_strip() {
        let mut app = app();
        let before = app.windows.windows.len();
        app.apply_cockpit(&CockpitState::default(), &HashSet::new());
        assert_eq!(
            app.windows.windows.len(),
            before,
            "a missing/empty file leaves the fresh default untouched"
        );
    }

    #[test]
    fn a_saved_layout_with_no_docked_windows_still_keeps_the_default_strip() {
        // The save path ALWAYS writes a layout label (e.g. "filmstrip"), so a
        // session that pinned nothing round-trips to {layout:"filmstrip",
        // windows:[]}. Reloading that must keep the default deps window — not
        // rebuild to a bare spine — while still adopting the layout mode.
        let mut app = app();
        let before = app.windows.windows.len();
        let state = CockpitState {
            layout: "mosaic".into(),
            windows: vec![],
            focus: None,
            ..CockpitState::default()
        };
        app.apply_cockpit(&state, &HashSet::new());
        assert_eq!(
            app.windows.windows.len(),
            before,
            "no docked windows → the default strip survives"
        );
        assert_eq!(
            app.windows.layout,
            LayoutMode::Mosaic,
            "but the saved layout mode is adopted"
        );
    }
}
