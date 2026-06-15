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
use crate::window::{DepsCursor, DepsRoot, LayoutMode, WindowId, WindowKind, WindowSet};

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
    /// How many docked agents are still pending an auto-resume (Phase 6). Drives
    /// the "resuming N…" header and keeps the loop animating until it settles.
    pub resuming_count: usize,
    /// Frame at which a stuck resume spinner is force-cleared (see
    /// [`RESUME_GRACE_FRAMES`]).
    resume_deadline: u64,
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
        // Open showing the dependency lens for the default selection, focused on
        // the Spine — so the cockpit opens like the v2 lens did, ready to browse.
        if !root.is_empty() {
            windows.open_or_reroot_deps(root.clone(), &graph);
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
            resuming_count: 0,
            resume_deadline: 0,
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

    /// The prefix chord's label (e.g. `Ctrl-A`), for hints/help.
    pub fn prefix_label(&self) -> String {
        self.keymap.prefix_label()
    }

    /// Record the terminal size (on resize / startup) so the visible-window set
    /// the poll cadence keys off matches what `draw` will place.
    pub fn set_viewport(&mut self, area: Rect) {
        self.viewport = area;
        self.keep_focus_in_view();
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

    /// Re-aim the Spine selection without touching any window's deps history.
    fn aim_spine(&mut self, key: String) {
        if key.is_empty() {
            return;
        }
        self.root = key;
        self.sync_list_selection();
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
        if self.show_help {
            self.show_help = false;
            return;
        }

        // 6. Route by the focused window's kind.
        match self.windows.focused_kind().clone() {
            // An Agent owns the keyboard: every key (Esc too) goes to its PTY.
            // The prefix above is the only escape.
            WindowKind::Agent(issue) => self.forward_to_agent(&issue, key),
            WindowKind::Spine => {
                self.acknowledge();
                if key.code != KeyCode::Esc
                    && let Some(action) = self.keymap.action_for(key)
                {
                    self.dispatch_spine(action);
                }
            }
            WindowKind::Deps(root) => {
                self.acknowledge();
                if key.code != KeyCode::Esc
                    && let Some(action) = self.keymap.action_for(key)
                {
                    self.dispatch_deps(action, root);
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
        // agent (a chosen prefix is never wholly unreachable by the PTY).
        if self.keymap.is_prefix(key) {
            if let WindowKind::Agent(issue) = self.windows.focused_kind().clone() {
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
            Action::ZoomToggle => self.windows.toggle_zoom(),
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
            Action::OpenDeps => self.open_deps_for_selection(),
            Action::OpenFleet => {
                self.windows.open_fleet();
            }
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
            _ => {}
        }
    }

    /// Direct keys while a Deps window is focused.
    fn dispatch_deps(&mut self, action: Action, root: DepsRoot) {
        // The Fleet map has no per-node cursor; only window verbs (behind the
        // prefix) and help apply. A per-issue tree drives its own cursor.
        if root == DepsRoot::Fleet {
            if action == Action::ToggleHelp {
                self.show_help = !self.show_help;
            }
            return;
        }
        // Operations needing the graph are split out so the cursor borrow and the
        // `&self.graph` borrow don't overlap.
        match action {
            Action::MoveDown => self.with_deps(|c| c.move_selection(1)),
            Action::MoveUp => self.with_deps(|c| c.move_selection(-1)),
            Action::SwitchSide => self.with_deps(|c| c.switch_side()),
            Action::Enter => self.deps_enter(),
            Action::ToggleCollapse => self.deps_collapse(),
            Action::Back => self.deps_back(),
            Action::OpenDeps => self.open_deps_for_selection(),
            Action::OpenFleet => {
                self.windows.open_fleet();
            }
            Action::ToggleHelp => self.show_help = !self.show_help,
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
        }
    }

    /// Open (or re-root the preview onto) a dependency window for the selection.
    fn open_deps_for_selection(&mut self) {
        if self.root.is_empty() {
            self.status_msg = Some("no issue selected".into());
            return;
        }
        let root = self.root.clone();
        self.windows.open_or_reroot_deps(root, &self.graph);
        self.keep_focus_in_view();
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

    /// Open (or focus) the Agent window for `issue`, reclaiming the backend of any
    /// displaced unpinned preview that's already dead.
    fn open_agent_window(&mut self, issue: &str) {
        let (_, displaced) = self.windows.open_or_focus_agent(issue);
        if let Some(w) = displaced {
            self.preview_size.remove(&w.id);
            if let Some(di) = w.kind.agent_issue() {
                self.reclaim_if_dead(di);
            }
        }
        self.keep_focus_in_view();
    }

    // ── Window-manager verbs ───────────────────────────────────────────────────

    fn pin_window(&mut self) {
        if self.windows.focus == 0 {
            self.status_msg = Some("the spine is always pinned".into());
            return;
        }
        let pinned = self.windows.toggle_pin_focused();
        self.cockpit_dirty = true; // the docked set changed
        let label = self
            .windows
            .focused()
            .issue()
            .unwrap_or("window")
            .to_string();
        self.status_msg = Some(if pinned {
            format!("pinned {label} · stays while you browse")
        } else {
            format!("unpinned {label}")
        });
    }

    fn close_window(&mut self) {
        let Some(closed) = self.windows.close_focused() else {
            self.status_msg = Some("the spine stays put".into());
            return;
        };
        if closed.pinned {
            self.cockpit_dirty = true; // a docked window left the set
        }
        self.preview_size.remove(&closed.id);
        if let Some(issue) = closed.kind.agent_issue() {
            // Close = undock: a *live* agent keeps running (refind via the
            // roster), so only reclaim its backend once it's actually dead.
            self.reclaim_if_dead(issue);
            self.set_footer(format!("closed {issue} · still running — r to refind"));
        } else {
            self.status_msg = Some("closed window".into());
        }
        self.keep_focus_in_view();
    }

    /// Arm a confirmed kill of the focused agent (`Ctrl-a x`). Kill is destructive
    /// and separate from close, so it's never a single keystroke.
    fn arm_kill(&mut self) {
        let WindowKind::Agent(issue) = self.windows.focused_kind().clone() else {
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
        self.windows.layout = self.windows.layout.toggled();
        self.cockpit_dirty = true;
        // Every window's Rect moves under the new layout; forget the cached sizes
        // so the next render reflows each live agent to where it now sits. This
        // (and zoom) are the *only* moments width reflows — browsing never does.
        self.preview_size.clear();
        self.keep_focus_in_view();
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

    // ── Scroll / visibility (filmstrip) ────────────────────────────────────────

    /// Bookkeeping after focus moves between windows: keep the focused column in
    /// view, commit an in-progress search if focus left the Spine (so keys reach
    /// the newly-focused window instead of the search buffer — the search filter
    /// stays applied), and lazy-resume a docked agent that just gained focus.
    fn after_focus_change(&mut self) {
        self.keep_focus_in_view();
        if self.search_active && self.windows.focus != 0 {
            self.search_active = false; // commit: end input mode, keep the query
        }
        self.maybe_resume_focused();
    }

    /// Keep the focused column in view (filmstrip horizontal scroll). Called from
    /// focus-move / open / close / layout / resize — never from `draw`.
    fn keep_focus_in_view(&mut self) {
        if self.windows.layout != LayoutMode::Filmstrip {
            return;
        }
        self.windows.scroll_x = layout::scroll_offset(
            self.windows.scroll_x,
            self.windows.focus_column(),
            self.windows.non_spine_count(),
            self.viewport.width,
        );
    }

    /// Whether window `idx` is on screen right now (post-scroll). Mosaic shows
    /// every window; zoom shows only the focused one; filmstrip shows the whole
    /// columns that fit.
    fn is_index_visible(&self, idx: usize) -> bool {
        if self.windows.zoomed {
            return idx == self.windows.focus;
        }
        match self.windows.layout {
            LayoutMode::Mosaic => idx < self.windows.windows.len(),
            LayoutMode::Filmstrip => layout::filmstrip_visible(
                self.windows.windows.len(),
                self.windows.scroll_x,
                self.viewport.width,
                idx,
            ),
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
    /// subject didn't survive: Agent windows whose issue isn't in `survivors`
    /// (the reconcile survivor set), and Deps windows whose root left the graph.
    /// With no docked windows persisted the cockpit's fresh default strip is kept
    /// (so a missing file — or a session where nothing was pinned — opens like
    /// today), while still honouring the saved layout mode. Restored Agent
    /// windows have no backend yet — they render as "resuming…" cards.
    pub fn apply_cockpit(&mut self, state: &CockpitState, survivors: &HashSet<String>) {
        let layout = match state.layout.as_str() {
            "mosaic" => LayoutMode::Mosaic,
            _ => LayoutMode::Filmstrip,
        };
        // No docked windows → keep the default strip (the save path always writes
        // a layout label, so we can't gate on that being empty), just adopt the
        // layout mode so a `mosaic` preference survives a pinless session.
        if state.windows.is_empty() {
            self.windows.layout = layout;
            self.keep_focus_in_view();
            return;
        }
        let mut set = WindowSet::new();
        set.layout = layout;
        for pw in &state.windows {
            match pw.kind {
                PersistedKind::Agent => {
                    if let Some(issue) = &pw.issue
                        && survivors.contains(issue)
                    {
                        set.push(WindowKind::Agent(issue.clone()), true, None);
                    }
                }
                PersistedKind::Deps => {
                    if let Some(root) = &pw.issue
                        && self.graph.get(root).is_some()
                    {
                        let cursor = DepsCursor::new(root.clone(), &self.graph);
                        set.push(WindowKind::Deps(DepsRoot::Issue), true, Some(cursor));
                    }
                }
                PersistedKind::Fleet => {
                    set.push(WindowKind::Deps(DepsRoot::Fleet), true, None);
                }
            }
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
        self.keep_focus_in_view();
    }

    // ── Auto-resume (Cockpit v3, Phase 6) ────────────────────────────────────

    /// Bring docked agents back on startup: eager-resume the focused docked agent
    /// plus up to `cap-1` others (so the supervisor's `max_concurrent` isn't
    /// blown), and lazy-resume the rest on first focus (see
    /// [`Self::maybe_resume_focused`]). `resumable` is the post-reconcile
    /// was-live set (never Done/Failed). Enables auto-resume for the session.
    pub fn begin_resume(&mut self, resumable: &HashSet<String>, cap: usize) {
        self.auto_resume = true;
        if self.supervisor.is_none() {
            return;
        }
        // Docked agent windows that are resumable, the focused one first so it
        // comes back immediately.
        let mut targets: Vec<String> = Vec::new();
        if let WindowKind::Agent(issue) = self.windows.focused_kind()
            && resumable.contains(issue)
        {
            targets.push(issue.clone());
        }
        for w in &self.windows.windows {
            if let WindowKind::Agent(issue) = &w.kind
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
        let WindowKind::Agent(issue) = self.windows.focused_kind().clone() else {
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
        let title = self
            .graph
            .get(issue)
            .map(|i| i.title.clone())
            .unwrap_or_else(|| issue.to_string());
        supervisor.launch(issue.to_string(), title);
        self.pending_launch.insert(issue.to_string());
        self.resuming_count += 1;
        // (Re)arm the grace from this resume, so a lazily-resumed agent (focused
        // long after the eager batch) shows its spinner and is still bounded —
        // the spinner can't pin the loop awake past the grace, eager or lazy.
        self.resume_deadline = self.frame + RESUME_GRACE_FRAMES;
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
                // One resume settled (Phase 6): drop the spinner once they're all in.
                self.resuming_count = self.resuming_count.saturating_sub(1);
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
        self.resuming_count > 0
            || self.flash.values().any(|&(_, until)| self.frame < until)
            || self.fleet.values().any(AgentStatus::is_animating)
    }

    /// Advance the animation frame and drop any expired flash. Also hard-clears a
    /// stuck "resuming…" spinner past its grace bound, so a wedged resume can't
    /// pin the cockpit awake forever.
    pub fn tick_frame(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        let now = self.frame;
        self.flash.retain(|_, &mut (_, until)| now < until);
        if self.resuming_count > 0 && now >= self.resume_deadline {
            self.resuming_count = 0;
        }
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
        WindowKind::Spine => None,
        WindowKind::Agent(issue) => Some(PersistedWindow {
            kind: PersistedKind::Agent,
            issue: Some(issue.clone()),
        }),
        WindowKind::Deps(DepsRoot::Issue) => Some(PersistedWindow {
            kind: PersistedKind::Deps,
            issue: w.deps.as_ref().map(|c| c.root.clone()),
        }),
        WindowKind::Deps(DepsRoot::Fleet) => Some(PersistedWindow {
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
    fn the_cockpit_opens_with_a_deps_window_on_the_selection() {
        let app = app();
        // Spine + a dependency window rooted at the default selection.
        assert_eq!(app.windows.windows.len(), 2);
        assert!(matches!(app.windows.windows[0].kind, WindowKind::Spine));
        assert!(matches!(
            app.windows.windows[1].kind,
            WindowKind::Deps(DepsRoot::Issue)
        ));
        assert_eq!(
            app.windows.windows[1].deps.as_ref().unwrap().root,
            "ZAP-204"
        );
        assert_eq!(app.windows.focus, 0, "focus opens on the spine");
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
    fn close_undocks_a_window_and_keeps_a_live_agent_running() {
        let mut app = app();
        register(&mut app, "ZAP-204");
        app.fleet.insert("ZAP-204".into(), AgentStatus::Running);
        press(&mut app, KeyCode::Enter); // open + focus the agent window
        verb(&mut app, KeyCode::Char('w')); // Ctrl-a w = close
        assert!(
            app.windows.agent_window("ZAP-204").is_none(),
            "the window is undocked"
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
        press(&mut app, KeyCode::Enter); // open its (EXITED) window
        verb(&mut app, KeyCode::Char('w')); // close it
        assert!(
            !app.backends.contains_key("ZAP-204"),
            "a dead, unreferenced agent's handle is reclaimed on close"
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
        assert!(app.windows.agent_window("ZAP-204").is_some());
    }

    #[test]
    fn kill_is_refused_on_a_non_agent_window() {
        let mut app = app();
        // Focus the deps window (index 1).
        verb(&mut app, KeyCode::Char('l'));
        assert!(matches!(
            app.windows.focused_kind(),
            WindowKind::Deps(DepsRoot::Issue)
        ));
        verb(&mut app, KeyCode::Char('x'));
        assert!(app.kill_confirm.is_none());
        assert!(app.status_msg.as_deref().unwrap().contains("no agent"));
    }

    #[test]
    fn focus_moves_left_and_right_across_windows() {
        let mut app = app();
        register(&mut app, "ZAP-204");
        press(&mut app, KeyCode::Enter); // windows: [Spine, Deps, Agent], focus=2
        verb(&mut app, KeyCode::Char('h')); // focus left
        assert_eq!(app.windows.focus, 1);
        verb(&mut app, KeyCode::Char('h'));
        assert_eq!(app.windows.focus, 0);
        verb(&mut app, KeyCode::Char('h')); // no wrap past the spine
        assert_eq!(app.windows.focus, 0);
        verb(&mut app, KeyCode::Char('l'));
        assert_eq!(app.windows.focus, 1);
    }

    #[test]
    fn pin_keeps_a_window_off_the_preview_slot() {
        let mut app = app();
        register(&mut app, "ZAP-204");
        register(&mut app, "ZAP-205");
        app.root = "ZAP-204".into();
        press(&mut app, KeyCode::Enter); // open ZAP-204 (unpinned preview)
        verb(&mut app, KeyCode::Char('p')); // pin it (focus stays on the agent)
        assert!(app.windows.focused().pinned);
        app.root = "ZAP-205".into();
        // The prefix button opens ZAP-205 from any focus; pinned ZAP-204 survives.
        verb(&mut app, KeyCode::Enter);
        assert!(app.windows.agent_window("ZAP-204").is_some());
        assert!(app.windows.agent_window("ZAP-205").is_some());
    }

    #[test]
    fn layout_toggle_flips_filmstrip_and_mosaic() {
        let mut app = app();
        assert_eq!(app.windows.layout, LayoutMode::Filmstrip);
        verb(&mut app, KeyCode::Char('|'));
        assert_eq!(app.windows.layout, LayoutMode::Mosaic);
        verb(&mut app, KeyCode::Char('|'));
        assert_eq!(app.windows.layout, LayoutMode::Filmstrip);
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
        // Open a second deps window on a different selection (pin the first so the
        // preview slot doesn't re-root it).
        verb(&mut app, KeyCode::Char('l')); // focus deps #1 (ZAP-204)
        verb(&mut app, KeyCode::Char('p')); // pin it
        app.root = "ZAP-210".into();
        press(&mut app, KeyCode::Char('d')); // open deps #2 on ZAP-210
        assert_eq!(app.windows.focused().deps.as_ref().unwrap().root, "ZAP-210");
        // The first window still roots at ZAP-204 — independent navigation.
        let first = app
            .windows
            .windows
            .iter()
            .find(|w| matches!(w.kind, WindowKind::Deps(DepsRoot::Issue)) && w.pinned)
            .unwrap();
        assert_eq!(first.deps.as_ref().unwrap().root, "ZAP-204");
    }

    #[test]
    fn open_fleet_opens_a_single_overview_window() {
        let mut app = app();
        press(&mut app, KeyCode::Char('g')); // open the fleet map
        assert!(matches!(
            app.windows.focused_kind(),
            WindowKind::Deps(DepsRoot::Fleet)
        ));
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
        assert!(app.windows.agent_window("ZAP-205").is_none());
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
        app.resuming_count = 2;
        app.resume_deadline = app.frame + RESUME_GRACE_FRAMES;
        assert!(app.is_animating());
        // …but a stuck resume can't pin it forever: past the grace it hard-clears.
        for _ in 0..=RESUME_GRACE_FRAMES {
            app.tick_frame();
        }
        assert_eq!(
            app.resuming_count, 0,
            "the grace bound cleared a stuck resume"
        );
        assert!(!app.is_animating());
    }

    #[test]
    fn an_agent_spawn_decrements_the_resume_count() {
        let mut app = app();
        app.resuming_count = 2;
        let fake = FakeBackend::new("ZAP-205");
        app.apply_event(AppEvent::AgentSpawned {
            issue: "ZAP-205".into(),
            backend: fake as Arc<dyn AgentBackend>,
        });
        assert_eq!(
            app.resuming_count, 1,
            "each resumed agent that comes up settles one"
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

    // ── Persistence (Phase 5) ────────────────────────────────────────────────

    #[test]
    fn cockpit_snapshot_persists_only_docked_windows() {
        let mut app = app(); // [Spine, Deps(ZAP-204) unpinned]
        register(&mut app, "ZAP-204");
        press(&mut app, KeyCode::Enter); // open Agent(ZAP-204) unpinned, focus it
        verb(&mut app, KeyCode::Char('p')); // pin the agent
        let state = app.snapshot_cockpit();
        // The pinned agent is docked; the unpinned startup deps preview is not.
        assert_eq!(state.windows.len(), 1);
        assert_eq!(state.windows[0].kind, PersistedKind::Agent);
        assert_eq!(state.windows[0].issue.as_deref(), Some("ZAP-204"));
        assert_eq!(state.layout, "filmstrip");
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
        let survivors: HashSet<String> = ["ZAP-204"].into_iter().map(String::from).collect();
        app.apply_cockpit(&state, &survivors);
        // Spine + 3 restored windows; the non-survivor agent (GONE-1) is pruned.
        assert_eq!(app.windows.windows.len(), 4);
        assert!(app.windows.agent_window("ZAP-204").is_some());
        assert!(
            app.windows.agent_window("GONE-1").is_none(),
            "a non-survivor agent window is pruned on restore"
        );
        assert_eq!(app.windows.layout, LayoutMode::Mosaic);
        assert!(
            matches!(
                app.windows.focused_kind(),
                WindowKind::Deps(DepsRoot::Issue)
            ),
            "focus is restored by identity to the deps window"
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
