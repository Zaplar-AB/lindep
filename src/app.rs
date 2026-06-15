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

/// How many animation frames a node flash lasts (~400 ms at the 100 ms tick).
const FLASH_FRAMES: u64 = 4;

/// Most chat panes shown at once on the chat wall — below ~a handful of rows a
/// `claude` PTY preview is unreadable, so we cap rather than slice it thinner.
const MAX_CHAT_PANES: usize = 4;

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

/// What fills the right half of the lens: the dependency trees, or the live
/// agent chats. Toggled with the `chat` key; the left issue list (the
/// navigation spine) stays put either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RightView {
    Deps,
    Chat,
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
    /// Issues the supervisor has fully reaped (`AgentReaped`) this session — a
    /// tombstone. The agent's hook forwarder is a separate, slower path, so a
    /// final `Notification`/`Stop`/`PostToolUse` hook can land *after* the reap;
    /// without this, that late hook would re-insert a live status for an agent
    /// with no backend, inflating the live count and re-arming the sticky alert
    /// with nothing left to clear it. A real relaunch clears the tombstone via
    /// `AgentSpawned`, so it never blocks a fresh agent. (Rehydration at startup
    /// seeds the fleet *before* anything is reaped, so it is unaffected.)
    reaped: HashSet<String>,

    /// Per-issue agent status, driven by the supervisor + notification bus.
    /// Absence of an entry means "no agent" — the fleet view's resting state.
    pub fleet: HashMap<String, AgentStatus>,
    /// Backend handles for agents we launched, keyed by issue. Used to render
    /// and drive an agent's PTY when attached.
    pub backends: HashMap<String, Arc<dyn AgentBackend>>,
    /// The issue whose PTY the cockpit is currently attached to (`None` = the
    /// dashboard). While attached, all input is forwarded to that agent.
    pub attached: Option<String>,
    /// Last (rows, cols) each agent's PTY was resized to, keyed by issue — for
    /// both the full-screen attach pane and the smaller chat-preview panes. We
    /// reflow a `claude` only when *its* geometry actually changes, so browsing
    /// the chat wall doesn't churn SIGWINCHes.
    pub preview_size: HashMap<String, (u16, u16)>,
    /// Which widget fills the right half of the lens (deps trees vs agent chats).
    pub right_view: RightView,
    /// Issues whose chat is pinned to the chat wall — kept visible while you
    /// browse other issues. Ordered (a `Vec`, not a set) so pins stay put.
    pub pinned: Vec<String>,
    /// An issue whose agent we just launched and are waiting to come up, so the
    /// footer can nudge "ready · t to attach" the moment it spawns. Never an
    /// auto-takeover — launch is async and a surprise full-screen would jar.
    pub pending_attach: Option<String>,
    /// Monotonic animation tick, advanced by the render loop only while
    /// something is animating. The renderer reads it to drive spinners/pulses;
    /// it never reads a clock, so `ui::draw` stays a pure function of state.
    pub frame: u64,
    /// Transient per-issue node flashes: issue → (kind, frame it expires at).
    /// Written in `apply_event`, pruned each animation tick ([`App::tick_frame`])
    /// once expired; read by the renderer (which also gates on the expiry frame).
    pub flash: HashMap<String, (Flash, u64)>,
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
            reaped: HashSet::new(),
            fleet: HashMap::new(),
            backends: HashMap::new(),
            attached: None,
            preview_size: HashMap::new(),
            right_view: RightView::Deps,
            pinned: Vec::new(),
            pending_attach: None,
            frame: 0,
            flash: HashMap::new(),
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
        if let Some(i) = self.order.iter().position(|k| *k == self.root) {
            self.list_state.select(Some(i));
        } else {
            // The root isn't in the visible list — either the list is empty, or a
            // jump (n / c / ] / enter / back) landed on an issue the active
            // filter/search hides. Show NO highlight rather than lighting up an
            // unrelated row: the lens describes the root, the list honestly shows
            // nothing selected. (A filter/search *change* re-aims the root into
            // the list first, in `rebuild_order`, so it never reaches here.)
            self.list_state.select(None);
        }
    }

    /// Whether the focused root is absent from the visible list (hidden by the
    /// active filter/search), so the list intentionally shows no highlight. Lets
    /// a jump explain itself instead of leaving the user staring at a blank
    /// selection.
    fn root_is_hidden(&self) -> bool {
        !self.order.is_empty() && !self.order.contains(&self.root)
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
        let (key, n, total) = (members[next].clone(), next + 1, members.len());
        // Re-root first, then describe where we landed — so the "hidden by
        // filter" note reflects the new root's visibility, not the old one's.
        self.set_root(key.clone(), true);
        self.status_msg = Some(format!("cycle {n}/{total} — {key}{}", self.hidden_note()));
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
        let (key, n, total) = (members[next].clone(), next + 1, members.len());
        self.set_root(key.clone(), true);
        self.status_msg = Some(format!(
            "needs you {n}/{total} — {key}{}",
            self.hidden_note()
        ));
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
            Action::ToggleChat => {
                self.right_view = match self.right_view {
                    RightView::Deps => RightView::Chat,
                    RightView::Chat => RightView::Deps,
                };
            }
            Action::TogglePin => self.toggle_pin_current(),
            Action::CycleChat => self.cycle_chat(1),
            Action::CycleChatBack => self.cycle_chat(-1),
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
                // A real relaunch revives the issue — clear any reaped tombstone
                // so this generation's hooks are honoured again.
                self.reaped.remove(&issue);
                self.fleet.insert(issue.clone(), AgentStatus::Spawning);
                self.backends.insert(issue.clone(), backend);
                self.flash
                    .insert(issue.clone(), (Flash::Launched, self.frame + FLASH_FRAMES));
                // If `a` was waiting on this launch, surface the attach nudge now
                // its terminal is live, and clear the wait.
                let t = self.keymap.label_for(Action::Attach);
                if self.pending_attach.as_deref() == Some(issue.as_str()) {
                    self.pending_attach = None;
                    self.set_footer(format!("agent on {issue} ready · {t} to attach"));
                } else {
                    self.set_footer(format!("agent launched on {issue} · {t} to attach"));
                }
                true
            }
            // Repaint when the output belongs to an agent whose screen is on
            // screen right now — attached, or showing as a chat pane. Off-screen
            // output changes nothing visible, so an idle/closed chat never
            // busy-repaints.
            AppEvent::AgentOutput { issue } => self.is_chat_visible(&issue),
            AppEvent::AgentExited { issue, code } => {
                // The supervisor's agent task is authoritative for fleet status
                // (via AgentStatusChanged) — a cancel reads as Stopped, a
                // self-exit as Done/Failed. Here we only surface a footer line and
                // reclaim the render handle now the PTY is gone, unless the user
                // is still attached and looking at its final screen.
                self.set_footer(match code {
                    Some(0) | None => format!("agent on {issue} finished"),
                    Some(c) => format!("agent on {issue} exited ({c})"),
                });
                // Its geometry is meaningless once it's dead; drop the bookkeeping
                // so a relaunch reflows from scratch and the map stays bounded.
                self.preview_size.remove(&issue);
                // Reclaim the dead render handle — unless we're attached (reading
                // its final screen) or it's pinned to the wall (so its final
                // screen stays as an EXITED card until you unpin it).
                let keep = self.attached.as_deref() == Some(issue.as_str())
                    || self.pinned.contains(&issue);
                if !keep {
                    self.backends.remove(&issue);
                }
                true
            }
            AppEvent::AgentNeedsYou { issue, reason } => {
                // Ignore a hook that arrives after the agent was torn down — it
                // must not resurrect a terminated node and re-inflate the count.
                // `is_terminal` covers the pre-reap window (a terminal entry still
                // lingers); `reaped` covers the post-reap window (the entry is
                // already gone). Together they span the whole post-mortem.
                if self.is_terminal(&issue) || self.reaped.contains(&issue) {
                    return false;
                }
                self.fleet.insert(issue.clone(), AgentStatus::NeedsYou);
                self.status_msg = Some(format!("⚑ {issue} needs you — {reason}"));
                self.needs_you_alert = true; // sticky until acknowledged
                true
            }
            AppEvent::AgentStatusChanged { issue, status } => {
                // A reaped agent is gone for good — the supervisor sends its
                // terminal verdict *before* the reap, so any AgentStatusChanged
                // arriving after the tombstone is a late hook (e.g. a Stop's Idle)
                // and must not revive the node. A relaunch clears the tombstone
                // via AgentSpawned first, so this never blocks a fresh agent.
                if self.reaped.contains(&issue) {
                    return false;
                }
                // The supervisor's terminal verdict (Stopped/Done/Failed) is
                // final: a late *live* status from a racing hook (e.g. a Stop
                // hook's Idle) must not un-terminate the node. A fresh launch
                // revives it through AgentSpawned, not here.
                if status.is_live() && self.is_terminal(&issue) {
                    return false;
                }
                // An explicit transition is authoritative — it's the one event
                // allowed to clear a NeedsYou. Drop the sticky alert if the node
                // it referred to is no longer waiting on the human.
                if !status.needs_you() && self.fleet.get(&issue) == Some(&AgentStatus::NeedsYou) {
                    self.needs_you_alert = false;
                }
                // A clean or crashed finish gets a brief node flash so it
                // registers even if you weren't watching that node.
                if matches!(status, AgentStatus::Done | AgentStatus::Failed) {
                    self.flash
                        .insert(issue.clone(), (Flash::Finished, self.frame + FLASH_FRAMES));
                }
                self.fleet.insert(issue, status);
                true
            }
            AppEvent::AgentAction { issue, action } => {
                // Stale tool-use hook after teardown — drop it whether the
                // terminal entry still lingers (is_terminal) or the agent has
                // already been reaped away entirely (reaped tombstone).
                if self.is_terminal(&issue) || self.reaped.contains(&issue) {
                    return false;
                }
                // Hook-bus PostToolUse chatter must not clobber a pending NeedsYou
                // the human still has to act on (a queued PostToolUse can arrive
                // just after a permission prompt), nor let the routine footer bury
                // an unacknowledged needs-you alert.
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
                // complete). Drop it from the fleet so the view stays bounded and
                // mirrors the supervisor. Keep the backend handle while attached
                // (reading its final screen) or pinned (so its EXITED card stays
                // on the wall until you unpin it); otherwise reclaim it.
                //
                // Tombstone the issue so a hook that raced in behind the reap
                // (the forwarder is a separate, slower path) can't re-create a
                // live entry for an agent that no longer exists.
                self.reaped.insert(issue.clone());
                self.fleet.remove(&issue);
                self.preview_size.remove(&issue);
                let keep = self.attached.as_deref() == Some(issue.as_str())
                    || self.pinned.contains(&issue);
                if !keep {
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

    /// Open an agent on the focused issue (the `a` key) — the first half of the
    /// two-step open→attach flow. One agent per issue: a *live* agent (incl. an
    /// idle one) is never duplicated — we reveal its chat and point at attach; a
    /// stopped/finished one is relaunched, which resumes its conversation. Always
    /// switches the right pane to the chat so you see what you opened.
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
        let t = self.keymap.label_for(Action::Attach);

        // A live agent already exists (spawning/running/needs-you/idle — all
        // alive): don't spin up a second. Reveal its chat and point at attach,
        // rather than print an optimistic "launching…" the supervisor's own
        // running/capacity guards would contradict a tick later.
        if let Some(s) = self.fleet.get(&key).copied()
            && s.is_live()
        {
            self.right_view = RightView::Chat;
            self.status_msg = Some(if s.needs_you() {
                format!("⚑ {key} needs you · {t} to attach")
            } else {
                format!("{key} already has an agent · {t} to attach")
            });
            return;
        }
        // A launch already in flight (double-press before it spawns).
        if self.pending_launch.contains(&key) {
            self.right_view = RightView::Chat;
            self.set_footer(format!("already opening an agent on {key}…"));
            return;
        }
        // Absent, or terminal (stopped/done/failed) → (re)launch; the supervisor
        // resumes the conversation transparently if a session already exists.
        // Clone the handle out so we can also touch `status_msg` without a
        // borrow conflict; the clone is cheap (an mpsc sender).
        match self.supervisor.clone() {
            Some(supervisor) => {
                supervisor.launch(key.clone(), title);
                self.pending_launch.insert(key.clone());
                self.right_view = RightView::Chat;
                self.pending_attach = Some(key.clone());
                self.set_footer(format!("opening agent on {key} · {t} to attach when ready"));
            }
            None => self.status_msg = Some("agent control plane unavailable".into()),
        }
    }

    /// Stop the agent on the focused issue (the `x` key), leaving others running.
    fn cancel_agent(&mut self) {
        let Some(issue) = self.focused_issue().map(|i| i.key.clone()) else {
            return;
        };
        // Only a *live* agent can be stopped: a stopped/done/failed entry lingers
        // briefly for its glyph but the supervisor has already reaped it, so a
        // Cancel would only earn a contradicting "no agent running" a tick later.
        if !self.fleet.get(&issue).is_some_and(AgentStatus::is_live) {
            self.status_msg = Some(format!("agent on {issue} is not running"));
            return;
        }
        match self.supervisor.clone() {
            Some(supervisor) => {
                supervisor.cancel(issue.clone());
                self.status_msg = Some(format!("stopping agent on {issue}…"));
            }
            None => self.status_msg = Some("agent control plane unavailable".into()),
        }
    }

    /// `(live-agents, needs-you)` counts for the header summary. "Agents" counts
    /// only *live* nodes ([`AgentStatus::is_live`] — spawning/running/needs-you/
    /// idle-but-alive), not the terminal Stopped/Done/Failed entries that linger
    /// in `fleet` until reaped — so the number drops the instant you stop or
    /// finish one, reflecting what's actually running.
    pub fn fleet_summary(&self) -> (usize, usize) {
        let agents = self.fleet.values().filter(|s| s.is_live()).count();
        let needs_you = self.fleet.values().filter(|s| s.needs_you()).count();
        (agents, needs_you)
    }

    /// Whether `issue`'s agent has reached a terminal state (the process is
    /// gone). Used to make terminal status sticky against a racing late hook.
    fn is_terminal(&self, issue: &str) -> bool {
        matches!(
            self.fleet.get(issue),
            Some(AgentStatus::Stopped | AgentStatus::Done | AgentStatus::Failed)
        )
    }

    /// The issues whose chat is on the chat wall, in render order: pinned first
    /// (in pin order), then the current selection. Filtered on `backends` — a
    /// handle exists only while there's a screen to show. The live current
    /// selection is *always* given a slot (the "follows the cursor" chat is the
    /// primary feature), dropping the oldest pin only if the wall is already
    /// full — so a selected agent's chat is never silently hidden.
    pub fn chat_panes(&self) -> Vec<String> {
        let mut panes: Vec<String> = self
            .pinned
            .iter()
            .filter(|k| self.backends.contains_key(*k))
            .cloned()
            .collect();
        match self.focused_issue().map(|i| i.key.clone()) {
            Some(sel) if self.backends.contains_key(&sel) && !panes.contains(&sel) => {
                if panes.len() >= MAX_CHAT_PANES {
                    panes.truncate(MAX_CHAT_PANES - 1); // reserve the selection's slot
                }
                panes.push(sel);
            }
            _ => panes.truncate(MAX_CHAT_PANES),
        }
        panes
    }

    /// Allocation-free predicate for "is there any chat pane to show" — mirrors
    /// [`App::chat_panes`] membership (a pinned issue with a backend, or the live
    /// current selection) without materialising the `Vec`. Hot path: the render
    /// loop calls it every poll tick to pick the chat-wall poll cadence.
    pub fn has_chat_panes(&self) -> bool {
        self.pinned.iter().any(|k| self.backends.contains_key(k))
            || self
                .focused_issue()
                .is_some_and(|i| self.backends.contains_key(&i.key))
    }

    /// Whether `issue`'s live screen is on screen right now — attached, or shown
    /// as a chat pane. Gates the AgentOutput repaint so only visible agents'
    /// output forces a redraw (preserving the idle-quiet property). Kept
    /// allocation-free: `AgentOutput` fires per PTY read, so this is a hot path.
    pub fn is_chat_visible(&self, issue: &str) -> bool {
        if self.attached.as_deref() == Some(issue) {
            return true;
        }
        if self.right_view != RightView::Chat || self.mode != Mode::Lens {
            return false;
        }
        // Mirrors `chat_panes` membership without materialising it: a pinned
        // agent (pins are capped below the wall size, so none is truncated), or
        // the live current selection (always given a slot).
        self.pinned.iter().any(|k| k == issue)
            || (self.backends.contains_key(issue)
                && self.focused_issue().is_some_and(|i| i.key == issue))
    }

    /// Whether anything on screen is animating — a live agent's spinner/pulse,
    /// or an unexpired node flash. The render loop arms its animation tick only
    /// when this holds, so a cockpit of only resting/terminal agents (or none)
    /// never busy-repaints.
    pub fn is_animating(&self) -> bool {
        self.flash.values().any(|&(_, until)| self.frame < until)
            || self.fleet.values().any(AgentStatus::is_animating)
    }

    /// Advance the animation frame and drop any flash that has now expired (so
    /// the `flash` map stays bounded and `is_animating` settles back to false).
    /// Called by the render loop on its wall-clock animation cadence.
    pub fn tick_frame(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        let now = self.frame;
        self.flash.retain(|_, &mut (_, until)| now < until);
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
        // Drop any cached geometry so the first render resizes the agent to the
        // full-screen attach pane.
        self.preview_size.remove(&issue);
        self.pending_leader = None;
        let detach = self.keymap.label_for(Action::Detach);
        self.status_msg = Some(format!("attached to {issue} · {detach} to detach"));
    }

    /// Pin or unpin the focused issue's chat (the `p` key). A pinned chat stays
    /// on the wall while you browse other issues — the way to keep several agents
    /// visible at once. Refuses to overflow the wall rather than silently
    /// evicting a pin.
    fn toggle_pin_current(&mut self) {
        let Some(key) = self.focused_issue().map(|i| i.key.clone()) else {
            return;
        };
        if let Some(pos) = self.pinned.iter().position(|k| *k == key) {
            self.pinned.remove(pos);
            // If we were keeping a dead agent's screen alive only because it was
            // pinned (see AgentExited), reclaim its handle now.
            if self
                .backends
                .get(&key)
                .is_some_and(|b| matches!(b.status(), Lifecycle::Exited(_)))
            {
                self.backends.remove(&key);
                self.preview_size.remove(&key);
            }
            self.status_msg = Some(format!("unpinned {key}"));
            return;
        }
        if !self.backends.contains_key(&key) {
            self.status_msg = Some(format!("no agent on {key} to pin — a to open one"));
            return;
        }
        // Cap pins one below the wall size so the current selection always keeps
        // a slot — pinning never hides the chat that follows your cursor.
        const PIN_CAP: usize = MAX_CHAT_PANES - 1;
        if self.pinned.len() >= PIN_CAP {
            self.status_msg = Some(format!("chat wall full ({PIN_CAP} pins) — unpin one first"));
            return;
        }
        self.pinned.push(key.clone());
        self.right_view = RightView::Chat;
        self.status_msg = Some(format!("pinned {key} · stays while you browse"));
    }

    /// Step the lens to the next/prev issue that has a live agent screen,
    /// wrapping, and switch to the chat view so you see it — a quick tour of the
    /// running agents (`]` / `[`). Walks the graph's display order so it visits
    /// agents even on issues the current filter hides.
    fn cycle_chat(&mut self, delta: i32) {
        let agents: Vec<String> = self
            .graph
            .keys()
            .iter()
            .filter(|k| self.backends.contains_key(*k))
            .cloned()
            .collect();
        if agents.is_empty() {
            self.status_msg = Some("no agents to switch between — a to open one".into());
            return;
        }
        self.right_view = RightView::Chat;
        let next = match agents.iter().position(|k| *k == self.root) {
            Some(i) => (i as i32 + delta).rem_euclid(agents.len() as i32) as usize,
            None if delta >= 0 => 0,
            None => agents.len() - 1,
        };
        let (key, n, total) = (agents[next].clone(), next + 1, agents.len());
        self.set_root(key.clone(), true);
        self.status_msg = Some(format!("chat {n}/{total} · {key}{}", self.hidden_note()));
    }

    /// A status suffix flagging that the lens jumped to an issue the active
    /// filter/search hides — so the empty list highlight reads as deliberate.
    fn hidden_note(&self) -> &'static str {
        if self.root_is_hidden() {
            " · hidden by filter (clear it to list)"
        } else {
            ""
        }
    }

    /// Detach back to the dashboard, leaving the agent running. If the agent
    /// exited while we were attached, reclaim its render handle on the way out.
    fn detach(&mut self) {
        self.pending_leader = None;
        if let Some(issue) = self.attached.take() {
            // Force the next render (chat pane or re-attach) to re-resize this
            // agent to whatever Rect it lands in.
            self.preview_size.remove(&issue);
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
            } else {
                // The leader was a no-op — including a double-tap, where `key` is
                // the leader itself, so forwarding it sends a single leader chord
                // through (a chosen leader is never wholly unreachable). The
                // consumed leader keypress is intentionally not re-sent.
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
    fn stopping_or_finishing_an_agent_drops_it_from_the_live_count() {
        // The reported bug: the header count never fell when you closed an agent.
        let mut app = app();
        app.fleet.insert("ZAP-204".into(), AgentStatus::Running);
        app.fleet.insert("ZAP-205".into(), AgentStatus::NeedsYou);
        assert_eq!(app.fleet_summary(), (2, 1));

        // A stop (Stopped) and a finish (Done) both leave the node in the fleet
        // — so you can still see it ran — but neither counts as a live agent.
        app.apply_event(AppEvent::AgentStatusChanged {
            issue: "ZAP-204".into(),
            status: AgentStatus::Stopped,
        });
        app.apply_event(AppEvent::AgentStatusChanged {
            issue: "ZAP-205".into(),
            status: AgentStatus::Done,
        });
        assert_eq!(app.fleet_summary(), (0, 0), "the live count drops to zero");
        assert!(
            app.fleet.contains_key("ZAP-204") && app.fleet.contains_key("ZAP-205"),
            "the nodes still record that an agent ran there"
        );
    }

    #[test]
    fn chat_panes_show_pins_first_then_selection_and_cap() {
        let mut app = app();
        for key in ["ZAP-150", "ZAP-188", "ZAP-198", "ZAP-201", "ZAP-205"] {
            let fake = crate::backend::fake::FakeBackend::new(key);
            app.backends
                .insert(key.into(), fake as Arc<dyn AgentBackend>);
        }
        app.pinned = vec!["ZAP-150".into(), "ZAP-188".into()];
        app.root = "ZAP-205".into();
        assert_eq!(
            app.chat_panes(),
            vec!["ZAP-150", "ZAP-188", "ZAP-205"],
            "pins in order, then the current selection"
        );

        // Even with the wall full of pins (set directly, bypassing the UI cap),
        // a live unpinned selection is never silently hidden — it keeps a slot,
        // dropping the oldest pin instead (the M1 fix).
        app.pinned = vec![
            "ZAP-150".into(),
            "ZAP-188".into(),
            "ZAP-198".into(),
            "ZAP-201".into(),
        ];
        let panes = app.chat_panes();
        assert_eq!(panes.len(), 4);
        assert!(
            panes.contains(&"ZAP-205".to_string()),
            "the selected agent's chat always gets a slot"
        );
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
    fn pinning_caps_at_one_below_the_wall_and_keeps_the_selection_visible() {
        let mut app = app();
        for key in ["ZAP-150", "ZAP-188", "ZAP-198", "ZAP-201"] {
            let fake = crate::backend::fake::FakeBackend::new(key);
            app.backends
                .insert(key.into(), fake as Arc<dyn AgentBackend>);
        }
        // Pin three (the cap, reserving the 4th slot for the selection).
        for key in ["ZAP-150", "ZAP-188", "ZAP-198"] {
            app.root = key.into();
            press(&mut app, KeyCode::Char('p'));
        }
        assert_eq!(app.pinned.len(), 3);
        // The fourth pin is refused.
        app.root = "ZAP-201".into();
        press(&mut app, KeyCode::Char('p'));
        assert_eq!(app.pinned.len(), 3, "the cap holds");
        assert!(app.status_msg.as_deref().unwrap().contains("full"));
        // …and ZAP-201's chat still shows as the selection's reserved pane.
        let panes = app.chat_panes();
        assert_eq!(panes.len(), 4);
        assert!(panes.contains(&"ZAP-201".to_string()));
    }

    #[test]
    fn a_late_hook_cannot_resurrect_a_terminated_agent() {
        // Guards the headline count fix against a hook racing the teardown.
        let mut app = app();
        app.fleet.insert("ZAP-204".into(), AgentStatus::Done);

        // Stray PostToolUse / Notification / Stop hooks after teardown are ignored.
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
        assert_eq!(app.fleet_summary(), (0, 0), "the count stays at zero");

        // But a fresh launch legitimately revives it through AgentSpawned.
        let fake = crate::backend::fake::FakeBackend::new("ZAP-204");
        app.apply_event(AppEvent::AgentSpawned {
            issue: "ZAP-204".into(),
            backend: fake as Arc<dyn AgentBackend>,
        });
        assert_eq!(app.fleet.get("ZAP-204"), Some(&AgentStatus::Spawning));
    }

    #[test]
    fn a_late_hook_cannot_resurrect_a_reaped_agent() {
        // The post-reap window the terminal guard alone misses: once AgentReaped
        // has removed the fleet entry, `is_terminal` is false (no entry), so a
        // final hook forwarded by the dying agent must be stopped by the reaped
        // tombstone instead — or it would re-create a live, backend-less node,
        // inflating the count and re-arming a sticky alert nothing can clear.
        let mut app = app();
        let fake = crate::backend::fake::FakeBackend::new("ZAP-204");
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
        assert_eq!(app.fleet_summary(), (0, 0));

        // All three late hook events for the reaped agent are ignored: no
        // repaint, no resurrected entry, no sticky alert.
        assert!(!app.apply_event(AppEvent::AgentNeedsYou {
            issue: "ZAP-204".into(),
            reason: "late prompt".into(),
        }));
        assert!(!app.apply_event(AppEvent::AgentAction {
            issue: "ZAP-204".into(),
            action: "ran grep".into(),
        }));
        assert!(!app.apply_event(AppEvent::AgentStatusChanged {
            issue: "ZAP-204".into(),
            status: AgentStatus::Idle,
        }));
        assert!(
            !app.fleet.contains_key("ZAP-204"),
            "no late hook revived the reaped agent"
        );
        assert_eq!(app.fleet_summary(), (0, 0), "live count stays zero");
        assert!(!app.needs_you_alert, "no phantom sticky needs-you alert");

        // A genuine relaunch clears the tombstone, so the new generation's hooks
        // work again.
        let fake2 = crate::backend::fake::FakeBackend::new("ZAP-204");
        app.apply_event(AppEvent::AgentSpawned {
            issue: "ZAP-204".into(),
            backend: fake2 as Arc<dyn AgentBackend>,
        });
        assert!(app.apply_event(AppEvent::AgentNeedsYou {
            issue: "ZAP-204".into(),
            reason: "real prompt".into(),
        }));
        assert_eq!(app.fleet.get("ZAP-204"), Some(&AgentStatus::NeedsYou));
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
                .contains("already has an agent"),
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
                .contains("already opening"),
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
    fn a_pinned_agents_screen_survives_its_exit_until_unpinned() {
        let mut app = app();
        let fake = crate::backend::fake::FakeBackend::new("ZAP-205");
        app.backends
            .insert("ZAP-205".into(), fake.clone() as Arc<dyn AgentBackend>);
        app.pinned = vec!["ZAP-205".into()];

        // It exits — but because it's pinned, its final screen stays on the wall.
        fake.finish(Some(0));
        app.apply_event(AppEvent::AgentExited {
            issue: "ZAP-205".into(),
            code: Some(0),
        });
        assert!(
            app.backends.contains_key("ZAP-205"),
            "kept for the EXITED card while pinned"
        );

        // Unpinning the dead agent reclaims its handle.
        app.root = "ZAP-205".into();
        press(&mut app, KeyCode::Char('p'));
        assert!(app.pinned.is_empty());
        assert!(
            !app.backends.contains_key("ZAP-205"),
            "reclaimed on unpin once it's dead"
        );
    }

    #[test]
    fn a_jump_to_a_filtered_out_agent_clears_the_highlight_and_says_so() {
        let mut app = app();
        // ZAP-198 is unblocked, so filtering to Blocked hides it from the list.
        app.fleet.insert("ZAP-198".into(), AgentStatus::NeedsYou);
        app.filter = Filter::Blocked;
        app.rebuild_order();
        assert!(
            !app.order.contains(&"ZAP-198".to_string()),
            "precondition: the filter hides the agent's issue"
        );

        press(&mut app, KeyCode::Char('n')); // jump to the needs-you agent
        assert_eq!(app.root, "ZAP-198", "the lens jumps to the agent");
        assert_eq!(
            app.list_state.selected(),
            None,
            "the list shows no highlight rather than lighting an unrelated row"
        );
        assert!(
            app.status_msg
                .as_deref()
                .unwrap()
                .contains("hidden by filter"),
            "and the jump explains why the list looks empty"
        );

        // Clearing the filter brings it back into the list, highlighted in sync.
        app.filter = Filter::All;
        app.rebuild_order();
        assert_eq!(
            app.list_state.selected(),
            app.order.iter().position(|k| *k == "ZAP-198"),
            "revealed and highlighted once the filter no longer hides it"
        );
    }

    #[test]
    fn tick_frame_advances_and_expires_flashes() {
        let mut app = app();
        // A launch flash makes the cockpit animate until the flash expires.
        app.flash.insert("ZAP-204".into(), (Flash::Launched, 3));
        assert!(app.is_animating());
        for _ in 0..3 {
            app.tick_frame();
        }
        assert!(
            !app.is_animating(),
            "the flash expired and nothing else animates → the loop goes quiet"
        );
        assert!(app.flash.is_empty(), "the expired flash was pruned");
    }

    #[test]
    fn agent_output_repaints_only_when_its_chat_is_on_screen() {
        let mut app = app();
        let fake = crate::backend::fake::FakeBackend::new("ZAP-205");
        app.backends
            .insert("ZAP-205".into(), fake as Arc<dyn AgentBackend>);
        app.root = "ZAP-205".into();

        // Deps view: the screen isn't shown, so output changes nothing.
        app.right_view = RightView::Deps;
        assert!(!app.apply_event(AppEvent::AgentOutput {
            issue: "ZAP-205".into()
        }));

        // Chat view with it selected: its pane is on screen → repaint.
        app.right_view = RightView::Chat;
        assert!(app.apply_event(AppEvent::AgentOutput {
            issue: "ZAP-205".into()
        }));

        // An off-screen agent's output still changes nothing visible.
        assert!(!app.apply_event(AppEvent::AgentOutput {
            issue: "ZAP-999".into()
        }));
    }

    #[test]
    fn is_animating_is_false_for_a_fleet_of_only_resting_agents() {
        let mut app = app();
        assert!(!app.is_animating(), "no agents → nothing animates");
        app.fleet.insert("ZAP-204".into(), AgentStatus::Idle);
        app.fleet.insert("ZAP-205".into(), AgentStatus::Done);
        app.fleet.insert("ZAP-201".into(), AgentStatus::Stopped);
        assert!(
            !app.is_animating(),
            "idle/done/stopped rest quietly — the cockpit doesn't busy-repaint"
        );
        app.fleet.insert("ZAP-240".into(), AgentStatus::Running);
        assert!(app.is_animating(), "a working agent drives the tick");
    }

    #[test]
    fn opening_an_already_live_agent_shows_its_chat_instead_of_duplicating() {
        let mut app = app();
        app.root = "ZAP-205".into();
        app.fleet.insert("ZAP-205".into(), AgentStatus::Running);
        // The live-guard returns before any supervisor command, so no duplicate.
        press(&mut app, KeyCode::Char('a'));
        assert_eq!(app.right_view, RightView::Chat, "a reveals the chat");
        assert!(
            app.status_msg
                .as_deref()
                .unwrap()
                .contains("already has an agent")
        );
    }

    #[test]
    fn pending_attach_is_cleared_and_a_flash_set_when_the_agent_spawns() {
        let mut app = app();
        app.pending_attach = Some("ZAP-205".into());
        let fake = crate::backend::fake::FakeBackend::new("ZAP-205");
        app.apply_event(AppEvent::AgentSpawned {
            issue: "ZAP-205".into(),
            backend: fake as Arc<dyn AgentBackend>,
        });
        assert!(app.pending_attach.is_none(), "the wait clears once it's up");
        assert!(app.status_msg.as_deref().unwrap().contains("ready"));
        assert!(app.flash.contains_key("ZAP-205"), "a launch flash is set");
    }

    #[test]
    fn pin_toggles_and_refuses_without_an_agent() {
        let mut app = app();
        app.root = "ZAP-205".into();
        // No agent yet → can't pin.
        press(&mut app, KeyCode::Char('p'));
        assert!(app.pinned.is_empty());
        assert!(app.status_msg.as_deref().unwrap().contains("no agent"));

        // With a live screen, p pins then unpins.
        let fake = crate::backend::fake::FakeBackend::new("ZAP-205");
        app.backends
            .insert("ZAP-205".into(), fake as Arc<dyn AgentBackend>);
        press(&mut app, KeyCode::Char('p'));
        assert_eq!(app.pinned, vec!["ZAP-205".to_string()]);
        press(&mut app, KeyCode::Char('p'));
        assert!(app.pinned.is_empty(), "a second p unpins");
    }

    #[test]
    fn v_toggles_the_right_pane_and_stop_needs_a_live_agent() {
        let mut app = app();
        assert_eq!(app.right_view, RightView::Deps);
        press(&mut app, KeyCode::Char('v'));
        assert_eq!(app.right_view, RightView::Chat);
        press(&mut app, KeyCode::Char('v'));
        assert_eq!(app.right_view, RightView::Deps);

        // x on a stopped (not live) node reports rather than firing a no-op stop.
        app.root = "ZAP-205".into();
        app.fleet.insert("ZAP-205".into(), AgentStatus::Stopped);
        press(&mut app, KeyCode::Char('x'));
        assert!(app.status_msg.as_deref().unwrap().contains("not running"));
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
