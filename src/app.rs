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
use std::sync::{Arc, Mutex};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::widgets::ListState;

use crate::backend::{self, AgentBackend, Lifecycle};
use crate::event::{AppEvent, AppEventTx, PushOutcome};
use crate::keymap::{Action, Keymap};
use crate::layout;
use crate::ledger::Ledger;
use crate::linear::{Client, ProjectRef};
use crate::model::{Direction, Graph, Issue, Priority, Status};
use crate::picker::{Picker, ReclaimPrompt, RepoChoice, RepoPicker};
use crate::session::{AgentStatus, CockpitState, PersistedKind, PersistedWindow};
use crate::window::{
    CoinMode, DepsCursor, GraduateOutcome, LayoutMode, WindowId, WindowKind, WindowSet, move_state,
};
use crate::workspace::WorkspaceHandle;

/// An open up-front repo multi-select (ENG-536), stashing the launch it gates.
/// While `Some` it is a full modal: keys toggle/move/confirm/cancel and never reach
/// the cockpit or a focused PTY. On confirm it fires the stashed launch with the
/// checked repo set; on cancel the launch is abandoned (nothing was sent yet).
pub(crate) struct RepoSelect {
    pub picker: RepoPicker,
    pub issue: String,
    pub title: String,
    pub size: Option<(u16, u16)>,
    pub adhoc: bool,
}

/// The open global all-agents screen (ENG-406): every live agent across the whole
/// workspace as `project · ISSUE · status`, with a cursor. A third top-level surface
/// (toggled from any graph or the project list); a snapshot taken when opened. Full
/// modal while `Some` — ↑↓ move, Enter re-roots onto the row (switching projects if
/// needed) attach-ready, Esc backs out.
pub(crate) struct GlobalView {
    pub rows: Vec<(String, String, AgentStatus)>,
    pub state: ListState,
}

/// A pending fenced-lazy-pull confirmation (ENG-542): a running agent asked for an
/// extra (in-candidate) repo, and the human must confirm before it's materialised.
/// While `Some`, the next key resolves it (`y`/Enter pulls, anything else denies) —
/// captured below the kill-confirm band but above the focused agent's PTY, so a
/// mid-turn agent's keystrokes don't leak into the answer and a kill's `y` still wins.
pub(crate) struct RepoConfirm {
    pub project_id: String,
    pub issue: String,
    pub handle: String,
}

/// How many animation frames a node flash lasts (~400 ms at the 100 ms tick).
const FLASH_FRAMES: u64 = 4;

/// Hard ceiling (~20 s at the 100 ms tick) on how long the "resuming N…" spinner
/// keeps the cockpit animating. A resume that wedges (a stuck `git`, a spawn that
/// never reports) must not pin the loop awake forever; past this the count is
/// force-cleared so an idle cockpit goes quiet.
const RESUME_GRACE_FRAMES: u64 = 200;
/// How long a freshly-quiet agent still renders as WORKING after its last PTY output.
/// Claude can emit its Stop/Idle hook before the last buffered terminal bytes paint; a
/// short settle window prevents a visible pane from flipping to IDLE while text is still
/// arriving.
const OUTPUT_SETTLE_FRAMES: u64 = 20;

/// A brief, self-extinguishing highlight on an issue's node — the "juice" that
/// makes a launch or a finish register. Lives for a few animation frames then
/// expires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flash {
    Launched,
    Finished,
    /// A crashed agent — flashes RED so a failure is never painted with the green
    /// "Finished" success flash it used to share (H5).
    Failed,
    /// A user-killed agent — a brief graphite pulse so a deliberate stop gets the same
    /// ~400 ms confirmation as a finish/crash (it used to flash nothing).
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    HasDeps,
}

impl Filter {
    const fn next(self) -> Self {
        match self {
            Filter::All => Filter::HasDeps,
            Filter::HasDeps => Filter::All,
        }
    }
    pub const fn label(self) -> &'static str {
        match self {
            Filter::All => "all",
            Filter::HasDeps => "has-deps",
        }
    }
}

/// The single fused per-issue *state* — the type the cockpit was missing.
///
/// lindep has one noun (the Issue) but, until v1.7, no one place that said what
/// state an issue is *in*: the graph truth (blocked / done) lives in
/// [`Graph`](crate::model::Graph), the agent truth (running / needs-you) lives
/// in [`AgentStatus`], and the two never met. `Readiness` is where they meet —
/// [`App::readiness`] is its only producer, and the spine bands, the dispatch
/// gate, and the Fleet tints are all facets of it rather than each re-deriving
/// "blocked" / "ready" locally.
///
/// Variants are declared in band / salience order, top→bottom, so the derived
/// `Ord` *is* the schedule order (`NeedsYou < Working < Idle < Ready < Blocked <
/// Done`): the spine can sort on it directly, with no separate comparator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Readiness {
    /// A live agent is waiting on you — the one thing you must act on.
    NeedsYou,
    /// A live agent is actively churning — spawning, or producing output /
    /// running tools. Work in motion.
    Working,
    /// A live agent is alive but resting (it finished a turn, process still up),
    /// waiting on nothing. Split out from `Working` so an agent that has genuinely
    /// gone quiet is visibly distinct from one still churning — a stuck-"working"
    /// agent that actually settled now drops to its own band instead of hiding.
    Idle,
    /// Unblocked, unresolved, no live agent — the launchpad. The one genuinely
    /// new state; every other band already had a home in the codebase.
    Ready,
    /// At least one upstream blocker is unresolved, or it sits in a dependency
    /// cycle (`↺`, an un-runnable sub-state of Blocked, not a band of its own).
    Blocked,
    /// Finished or abandoned — resolved in Linear.
    Done,
}

pub struct App {
    pub graph: Graph,
    pub order: Vec<String>,
    pub list_state: ListState,
    /// Render-space selection for the readiness-banded spine: its index counts
    /// the section dividers, so it can't share `list_state` (which indexes pure
    /// issue keys for navigation). Persisted on `App` only so its scroll *offset*
    /// survives across frames — a fresh `ListState` each draw would pin the
    /// selection to the viewport bottom on a long list.
    pub banded_list_state: ListState,

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

    pub filter: Filter,
    pub search_query: String,
    pub search_active: bool,
    pub show_help: bool,
    /// Scroll offset (top row) of the `?` help overlay. The overlay is taller than
    /// most split-pane terminals, so it scrolls instead of clipping its bottom rows
    /// (H3); reset to 0 each time help opens.
    pub help_scroll: u16,
    /// Dismissable overlay summarising the selected issue (the `i` button).
    pub show_summary: bool,
    /// Scroll offset of the `i` summary overlay — it wraps a long title and scrolls a
    /// long dependency list instead of clipping (A5); reset to 0 each time it opens.
    pub summary_scroll: u16,
    pub status_msg: Option<String>,
    /// True while `status_msg` holds an unacknowledged "needs you" alert. Routine
    /// high-frequency tool chatter (`AgentAction`) must not bury it; it clears the
    /// moment the human touches a Spine/Deps key (acknowledging) or a deliberate
    /// event replaces the footer.
    needs_you_alert: bool,
    /// Issues with a launch command in flight (sent to the supervisor, not yet
    /// acknowledged by an `AgentSpawned` or rejected) mapped to the frame at which a
    /// *wedged* launch — one that never reports back (e.g. a hung `git worktree add`
    /// before `AgentSpawned`) — is force-dropped, so its "starting…" card self-heals
    /// instead of stranding forever (M9). Lets the cockpit refuse a double-press
    /// before the fleet entry materializes.
    pending_launch: HashMap<String, u64>,
    /// Issues the supervisor has fully reaped (`AgentReaped`) this session — a
    /// tombstone. The agent's hook forwarder is a separate, slower path, so a
    /// final `Notification`/`Stop`/`PostToolUse` hook can land *after* the reap;
    /// without this, that late hook would re-insert a live status for an agent
    /// with no backend, inflating the live count and re-arming the sticky alert
    /// with nothing left to clear it. A real relaunch clears the tombstone via
    /// `AgentSpawned`, so it never blocks a fresh agent.
    reaped: HashSet<String>,

    /// The repo handles each live agent materialised, keyed by issue (primary
    /// first), as reported by the supervisor on `AgentSpawned` (ENG-536). Lets the
    /// agent window/coin headers show *which* repos/worktrees an agent spans — a
    /// multi-repo agent is otherwise indistinguishable from a single-repo one.
    /// Cleared on `AgentReaped` (full teardown); an EXITED card keeps it so you can
    /// still see what the finished agent owned.
    pub agent_repos: HashMap<String, Vec<String>>,
    /// Per-issue agent status, driven by the supervisor + notification bus.
    /// Absence of an entry means "no agent" — the fleet view's resting state.
    pub fleet: HashMap<String, AgentStatus>,
    /// Backend handles for agents we launched, keyed by issue. Used to render
    /// and drive an agent's PTY.
    pub backends: HashMap<String, Arc<dyn AgentBackend>>,
    /// Last frame at which each agent produced PTY output. Used only as a short
    /// visual settle window so an otherwise-idle agent does not read as quiet while
    /// buffered output is still painting.
    last_output: HashMap<String, u64>,
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
    /// The issue whose workspace a `Ctrl-a d` discard (push branches + remove
    /// worktrees, ENG-541) is awaiting confirmation for. Like `kill_confirm`, the
    /// next key confirms (`y`/Enter) or cancels — it reclaims checkout disk, so it's
    /// never a single keystroke.
    pub discard_confirm: Option<String>,
    /// A pending fenced-lazy-pull confirmation (ENG-542) raised by an agent's
    /// `request-repo`. While `Some`, the next key pulls (`y`/Enter) or denies.
    pub repo_confirm: Option<RepoConfirm>,
    /// A pending quit confirmation. Quitting can strand running agents off-screen,
    /// so the prefix quit verb asks once before tearing the cockpit down.
    pub quit_confirm: bool,
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
    /// The session's standing resume *policy* (set once by [`Self::begin_resume`],
    /// false under `--no-resume`). `auto_resume` is the live toggle; this is what a
    /// project switch restores it to — without it, [`Self::activate_project`] would
    /// re-enable resume after a switch and silently defeat `--no-resume`.
    auto_resume_enabled: bool,
    /// True under `--demo`: a read-only graph viewer with no control plane, so the
    /// launch refusal can name the real reason ("drop --demo") instead of jargon and
    /// the banner advertises no agent actions (H6). Distinct from a *degraded* run (a
    /// real project whose control plane failed to arm), which also has
    /// `workspace == None` but is not demo.
    pub demo: bool,
    /// Why the control plane didn't arm on a *non-demo* run (unregistered project,
    /// declined onboarding, hook-port clash). Persisted so a standing "⚠ agents off"
    /// header chip and the launch refusal can name the real reason, instead of the
    /// reason flashing once as a footer the next keystroke wipes (M13).
    pub degrade_reason: Option<String>,
    /// Set once a `Ctrl-a o` re-config wrote registry.toml this session. The live
    /// workspace keeps its old binding (re-rooting a running worktree manager mid-flight
    /// isn't safe), so a standing "⟳ restart to apply config" chip keeps the divergence
    /// visible until restart — not just the one-shot footer the next keystroke wipes.
    pub config_restart_pending: bool,
    /// Issues whose discard KEPT the worktree on disk (a rejected push left unpushed
    /// commits) — a standing header chip flags them and the issue stays re-discardable
    /// once the push can land, so the work is never silently stranded (D-HIGH).
    pub kept_worktrees: HashSet<String>,
    /// Per project, the keys (`issue`, or `issue/repo` for a multi-repo issue) whose
    /// v1.6 auto-push was REJECTED — the commit is stranded on the local clone. A
    /// standing header chip, and a CROSS-PROJECT surface (keyed by project, like
    /// [`Self::other_needs_you`]) because a rejected push strands commits whichever
    /// project owns the committing agent — so the scope guard must not drop it. A
    /// failed push is thus never papered over by the next "pushed" footer (the
    /// data-integrity contract). A key clears when a later push of it succeeds (or it
    /// turns out local-only) or when its issue's workspace is discarded; unlike the
    /// per-project fleet, it is NOT wiped on a project switch.
    pub unpushed: HashMap<String, HashSet<String>>,
    /// Synthetic ad-hoc agent ids (`ask-*`) grafted into the graph for this project.
    /// They reuse the normal session/worktree path, but their terminal teardown skips
    /// the issue-branch push because the branch is intentionally throwaway.
    ask_agents: HashSet<String>,
    /// Count of agents that finished cleanly (Done) this session — a "shipped today"
    /// header tally so the cockpit occasionally says you're winning, not just what's
    /// left (CF-14). Session-volatile; not persisted.
    pub shipped_today: u32,
    /// Handle to the workspace (every project's fleet), when running with one
    /// (absent in `--demo`, snapshots and unit tests). Launch/cancel route through
    /// it by `(active_project, issue)`.
    pub workspace: Option<WorkspaceHandle>,
    /// The Linear project the cockpit is currently inside — the project `a`/`x`
    /// and resume act on. Set when the control plane arms; empty in `--demo` /
    /// snapshots / tests (which never launch agents).
    pub active_project: String,
    /// Every Linear project, for the in-cockpit switcher overlay. Empty in
    /// `--demo` / when the control plane is off (switching is then unavailable).
    pub project_list: Vec<ProjectRef>,
    /// The configured (mapped) project ids — the set the switcher offers, since
    /// only a mapped project can run agents. Populated when the control plane arms.
    pub mapped_projects: HashSet<String>,
    /// The open project-switcher overlay (`Ctrl-a s`), if any. While `Some` it is a
    /// full modal: it captures every key (typing filters; Esc cancels).
    pub project_switcher: Option<Picker>,
    /// The up-front repo multi-select modal (ENG-536), if open. Full modal: while
    /// `Some` it captures every key (space toggles, ↑↓ move, ⏎ launches, Esc cancels).
    pub repo_select: Option<RepoSelect>,
    /// Per-project candidate repo choices for the up-front select, surfaced from the
    /// registry when the control plane arms (the registry itself lives off-`App`, in
    /// the workspace). Keyed by `project_id`. A project with ≤1 candidate launches
    /// straight away (no modal); >1 opens the select.
    pub project_candidates: HashMap<String, Vec<RepoChoice>>,
    /// Every registered repo (handle + local-only), sorted, snapshotted from the
    /// registry when the control plane arms. The at-launch repo picker's "add another
    /// repo" (CF-20) offers any of these the active project doesn't yet list as a
    /// candidate; confirming persists the pick into `registry.toml` and into
    /// `project_candidates` above (so the next launch offers it — no restart).
    pub registered_repos: Vec<RepoChoice>,
    /// The open disk-reclaim prompt (ENG-540, `Ctrl-a m`), if any. Full modal.
    pub reclaim: Option<ReclaimPrompt>,
    /// True while a reclaim scan/delete is running on the blocking pool, so a
    /// second Enter can't fire a duplicate delete before the rescan lands and
    /// clears it. Set only on the off-thread path; the inline fallback never arms.
    pub reclaim_busy: bool,
    /// The `~/.lindep` layout, surfaced when the control plane arms so the reclaim
    /// prompt can scan mirrors/clones (a quick filesystem walk) on a deliberate user
    /// action. `None` with the control plane off (`--demo`, snapshots, tests).
    pub layout: Option<crate::registry::Layout>,
    /// Every registered project's id → on-disk handle, snapshotted at boot so a switch
    /// can re-point the durable ledger to the target project's own file (H3) without
    /// re-reading the registry on the render thread.
    pub project_handles: HashMap<String, String>,
    /// Live agent backends for projects you've switched *away* from, keyed by
    /// `project_id` then issue. Stashed on switch (the `Arc`s stay valid while
    /// their agents run) so switching back re-attaches to the real PTYs; the
    /// active project's backends live in [`Self::backends`].
    stashed_backends: HashMap<String, HashMap<String, Arc<dyn AgentBackend>>>,
    /// Issues that need you in projects you're *not* currently inside, keyed by
    /// `project_id`. The scoping guard drops a backgrounded project's agent events
    /// from the on-screen fleet, but a "needs you" there must still surface — so we
    /// tally it here (and clear it when that agent resumes/exits, or when you switch
    /// into the project). Drives the header's "⚑N elsewhere" badge, the switcher's
    /// per-project flag, and a one-time toast. Never holds the active project.
    other_needs_you: HashMap<String, HashSet<String>>,
    /// Cross-project agent status, fed by EVERY agent event (before the scoping
    /// guard) so backgrounded projects' agents are visible too — the source for the
    /// workspace roll-up header, the global all-agents screen, and the cross-project
    /// `n` jump (ENG-406). Keyed `project_id` → `issue` → status. The active project's
    /// authoritative view stays in [`fleet`](Self::fleet); the roll-up reads `fleet`
    /// for the active project and `world` for the rest, so a late post-reap hook in
    /// the active project can't double-count a just-removed agent.
    pub world: HashMap<String, HashMap<String, AgentStatus>>,
    /// The open global all-agents screen (ENG-406, `Ctrl-a a`), if any. Full modal.
    pub global_view: Option<GlobalView>,
    /// A pending cross-project landing (ENG-406): `(project_id, issue, attach, gen)`.
    /// After a switch completes, re-root onto `issue` and (when `attach`) attach to its
    /// agent. Stamped with the target project AND the switch generation it belongs to,
    /// so a superseded/raced switch (the user fires another switch before this one's
    /// graph lands) DROPS the land in [`activate_project`](Self::activate_project)
    /// instead of mis-applying it to the wrong project (issue keys collide across
    /// projects). Set by the global screen's Enter and the cross-project needs-you jump.
    pending_land: Option<(String, String, bool, u64)>,
    /// Hand-off slot for a switch's freshly-loaded graph: the off-thread fetch
    /// drops `(gen, target, graph)` here and wakes the loop with
    /// [`AppEvent::ProjectActivated`] (`Graph` is neither `Clone` nor `Debug`, so
    /// it can't ride the event itself). `gen` is the switch generation, so a slow
    /// fetch for a superseded switch is dropped rather than applied late.
    switch_inbox: Arc<Mutex<Option<(u64, ProjectRef, Graph)>>>,
    /// Monotonic switch generation: bumped per [`Self::request_switch`] (and per
    /// cancel) so the most recently *selected* project wins regardless of which
    /// fetch *completes* first.
    switch_seq: u64,
    /// The project a switch is currently fetching, if any — so re-selecting the
    /// current project can cancel it and the footer can reflect "loading…".
    pending_switch: Option<String>,
    /// Set by the `configure-project` verb (`Ctrl-a o`); drained by the event loop,
    /// which suspends the cockpit, runs the onboarding wizard, then resumes.
    pending_configure: bool,
    /// Switcher plumbing, wired by [`Self::enable_project_switching`] when the
    /// control plane arms: the Linear client + runtime for the off-thread graph
    /// fetch, and the event sender to wake the render loop.
    linear: Option<Arc<Client>>,
    runtime: Option<tokio::runtime::Handle>,
    events: Option<AppEventTx>,
    /// Active key bindings (defaults, overridden by `config.toml`).
    pub keymap: Keymap,

    /// Where the window layout persists (`.lindep/cockpit.json`), or `None` when
    /// the control plane is off (`--demo`, snapshots, tests) — those never write.
    pub cockpit_path: Option<PathBuf>,
    /// Set when the docked window set / layout / focus changed and the layout
    /// should be re-persisted. The render thread (the sole cockpit writer) checks
    /// it after handling input and saves, so a structural change survives a crash.
    pub cockpit_dirty: bool,

    /// The per-issue agent ledger — a durable history of which sessions ran for an
    /// issue. Recorded from lifecycle events for *every* project (the render thread
    /// sees them before the scoping guard), so a backgrounded project's runs are
    /// logged too. Rendered in the `Ctrl-a t` overlay and the `i` summary panel.
    pub ledger: Ledger,
    /// Where the ledger persists (`.lindep/ledger.json`), or `None` with the
    /// control plane off. Unlike [`cockpit_path`](Self::cockpit_path) this is
    /// *not* cleared on a project switch: the ledger spans every project, so it
    /// keeps persisting to the booted repo's file across switches.
    pub ledger_path: Option<PathBuf>,
    /// Set when a ledger run started/ended so the render thread re-persists it
    /// (alongside the cockpit layout). Like `cockpit_dirty`, this fires only on a
    /// lifecycle change, never per keystroke.
    pub ledger_dirty: bool,
    /// Dismissable overlay listing the selected issue's agent run history (`Ctrl-a t`).
    pub show_ledger: bool,
    /// Scroll offset of the `Ctrl-a t` ledger overlay — like help/summary it scrolls a
    /// long history and dismisses only on Esc / `t`, so the three info overlays share
    /// one convention; reset to 0 each time it opens.
    pub ledger_scroll: u16,
    /// Inner height of the *focused* deps coin's active tree, captured at render so
    /// PageUp/PageDown move one visible screenful of THAT pane (a tiled coin), not the
    /// whole-terminal `spine_page`. 0 until a deps window has rendered.
    pub deps_view_h: u16,

    pub should_quit: bool,
}

impl App {
    pub fn new(graph: Graph) -> Self {
        // Default selection: the most-connected real issue — usually the spine of
        // the dependency web — so the cockpit opens somewhere interesting. Shares the
        // one helper with the project-switch path so the two picks can't drift.
        let root = most_connected_root(&graph);

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
            banded_list_state: ListState::default(),
            root,
            windows,
            preview_size: HashMap::new(),
            viewport: Rect::new(0, 0, 80, 24),
            filter: Filter::All,
            search_query: String::new(),
            search_active: false,
            show_help: false,
            help_scroll: 0,
            show_summary: false,
            summary_scroll: 0,
            status_msg: None,
            needs_you_alert: false,
            pending_launch: HashMap::new(),
            reaped: HashSet::new(),
            agent_repos: HashMap::new(),
            fleet: HashMap::new(),
            backends: HashMap::new(),
            last_output: HashMap::new(),
            pending_attach: None,
            frame: 0,
            flash: HashMap::new(),
            prefix_armed: false,
            kill_confirm: None,
            discard_confirm: None,
            repo_confirm: None,
            quit_confirm: false,
            resuming: HashMap::new(),
            resume_cap: 0,
            auto_resume: false,
            auto_resume_enabled: false,
            demo: false,
            degrade_reason: None,
            config_restart_pending: false,
            kept_worktrees: HashSet::new(),
            unpushed: HashMap::new(),
            ask_agents: HashSet::new(),
            shipped_today: 0,
            workspace: None,
            active_project: String::new(),
            project_list: Vec::new(),
            mapped_projects: HashSet::new(),
            project_switcher: None,
            repo_select: None,
            project_candidates: HashMap::new(),
            registered_repos: Vec::new(),
            reclaim: None,
            reclaim_busy: false,
            layout: None,
            project_handles: HashMap::new(),
            stashed_backends: HashMap::new(),
            other_needs_you: HashMap::new(),
            world: HashMap::new(),
            global_view: None,
            pending_land: None,
            switch_inbox: Arc::new(Mutex::new(None)),
            switch_seq: 0,
            pending_switch: None,
            pending_configure: false,
            linear: None,
            runtime: None,
            events: None,
            keymap: Keymap::default(),
            cockpit_path: None,
            cockpit_dirty: false,
            ledger: Ledger::default(),
            ledger_path: None,
            ledger_dirty: false,
            show_ledger: false,
            ledger_scroll: 0,
            deps_view_h: 0,
            should_quit: false,
        };
        app.rebuild_order();
        app
    }

    /// The issue every on-screen surface and action shares: the detail bar, the `i`
    /// summary, AND the dispatch / kill / editor verbs all resolve through this, so
    /// what you see is always what a verb acts on (H6). A coin on its deps face → its
    /// roving cursor root; a chat coin → its identity; the Spine/Fleet → the selection.
    /// `None` only when nothing is selected.
    pub fn detail_key(&self) -> Option<&str> {
        let w = self.windows.focused();
        // A coin on its deps face describes the issue its cursor is *currently*
        // rooted at — re-rooting explores other issues — not the coin's fixed
        // identity, so the detail bar follows deps navigation. Chat coins describe
        // their own identity; the Spine/Fleet fall back to the selection.
        if let WindowKind::Coin {
            mode: CoinMode::Deps,
            ..
        } = &w.kind
            && let Some(cursor) = w.deps.as_ref()
        {
            return Some(cursor.root.as_str());
        }
        match w.issue() {
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

    /// Rebuild the Spine's visible `order` from the active filter/search/sort.
    /// `pub(crate)` so `render_spine` can re-band a stale Readiness order (the
    /// only sort whose ordering depends on live fleet state) just before drawing.
    pub(crate) fn rebuild_order(&mut self) {
        let needle = self.search_query.to_lowercase();
        let filter = self.filter;

        // Own the key list so the per-key sort key can borrow `&self` — the
        // Readiness band reads the fleet, not just the graph — without aliasing
        // `self.graph`. `rebuild_order` runs on sort/filter/search changes (and,
        // for the fleet-dependent Readiness sort, when `render_spine` finds the
        // order stale), never per render, so the clone is not on a hot path.
        let keys = self.graph.keys().to_vec();
        let mut decorated: Vec<((u8, u64), String)> = keys
            .into_iter()
            .filter_map(|k| {
                let issue = self.graph.get(&k)?;
                if issue.external {
                    return None; // externals show in trees, not the project list
                }
                let pass_filter = match filter {
                    Filter::All => true,
                    Filter::HasDeps => {
                        self.graph.direct_count(&k, Direction::Upstream) > 0
                            || self.graph.direct_count(&k, Direction::Downstream) > 0
                    }
                };
                let pass_search = needle.is_empty()
                    || issue.key.to_lowercase().contains(&needle)
                    || issue.title.to_lowercase().contains(&needle);
                (pass_filter && pass_search).then(|| (self.sort_key(&k), k.clone()))
            })
            .collect();

        // Within a band+impact tie, the Readiness schedule prefers higher
        // priority (the folded-in priority sort), then a stable natural id.
        decorated.sort_by(|(ka, a), (kb, b)| {
            ka.cmp(kb)
                .then_with(|| self.priority_rank(a).cmp(&self.priority_rank(b)))
                .then_with(|| natural_key_cmp(a, b))
        });
        self.order = decorated.into_iter().map(|(_, k)| k).collect();
        // If the active filter/search hid the current selection, re-aim it at the
        // first visible issue so the list highlight and the detail bar agree — and
        // re-aim the follower preview too, so its tree/chat doesn't keep showing the
        // now-hidden issue while you type a search (M4). reaim_preview self-guards
        // against pinned coins, and this fires only when root actually moved.
        if !self.order.is_empty() && !self.order.contains(&self.root) {
            self.root = self.order[0].clone();
            self.reaim_preview();
        }
        self.sync_list_selection();
    }

    /// Fuse an issue's graph truth (blocked / done) and agent truth (running /
    /// needs-you) into the one [`Readiness`] band it belongs to. This is the
    /// keystone of v1.7: the spine bands, the dispatch gate, and the Fleet
    /// tints all read from here, so "what state is this issue in" has exactly
    /// one answer instead of five partial re-derivations.
    ///
    /// Precedence is salience, top band first:
    /// 1. **A live agent outranks the graph.** A `NeedsYou` agent → `NeedsYou`;
    ///    a churning agent (spawning / running) → `Working`; a resting-but-alive
    ///    agent (idle) → `Idle`. An agent on an issue Linear already marks
    ///    resolved still reads as live — the process is the actionable thing,
    ///    not the label.
    /// 2. **A terminal agent (stopped / done / failed) pins no band** — the
    ///    issue reverts to its graph truth, so a failed launch shows `Ready`
    ///    again (re-dispatchable) and a clean finish shows `Done`.
    /// 3. **Graph truth for an agent-less issue:** `Done` if resolved, else
    ///    `Blocked` if it has an unresolved blocker or sits in a cycle, else
    ///    `Ready` — the one line the codebase didn't already compute somewhere.
    ///
    /// `key` may be a fleet member that has left the graph (an archived issue
    /// whose session survived reconcile); the agent-truth branch classifies it
    /// without needing a graph node.
    pub fn readiness(&self, key: &str) -> Readiness {
        if let Some(status) = self.fleet.get(key) {
            if status.needs_you() {
                return Readiness::NeedsYou;
            }
            if status.is_working() {
                return Readiness::Working;
            }
            if status.is_idle() {
                // FEAT-B: a settled (Idle) agent whose child is still streaming PTY
                // output is busy, not resting — band and sort it under WORKING so the
                // header matches the live spinner `display_agent_status` already shows
                // in the gutter (no band/row stutter). Self-expires once output stops.
                if self.recently_active(key) {
                    return Readiness::Working;
                }
                return Readiness::Idle;
            }
            // Terminal: fall through to graph truth below.
        }
        if self.graph.get(key).is_some_and(|i| i.status.is_resolved()) {
            return Readiness::Done;
        }
        if self.graph.is_blocked(key) || self.graph.in_cycle(key) {
            return Readiness::Blocked;
        }
        Readiness::Ready
    }

    /// The once-per-node sort key for the readiness schedule — band (top→bottom),
    /// then highest downstream impact within the band; priority then id break the
    /// remaining ties in `rebuild_order`. Lower sorts first. A method (not a free
    /// fn) because the band reads the fleet via [`App::readiness`], not just the graph.
    fn sort_key(&self, key: &str) -> (u8, u64) {
        (
            self.readiness(key) as u8,
            u64::MAX - self.graph.transitive(key, Direction::Downstream) as u64,
        )
    }

    /// Priority rank (Urgent first … None last) — the within-band tiebreak the
    /// Readiness schedule folds in, in place of the deleted standalone priority sort.
    fn priority_rank(&self, key: &str) -> u8 {
        self.graph.get(key).map_or(u8::MAX, |i| i.priority.rank())
    }

    /// Whether `order` is still sorted exactly as `rebuild_order` would sort it —
    /// by (band, downstream impact), then priority, then natural id. Agent events
    /// (and a graph refetch) move the fleet/graph under `order` without re-sorting,
    /// so `render_spine` re-bands when this returns false — keeping the section
    /// dividers contiguous AND the within-band order fresh, not just the band
    /// sequence. Must mirror `rebuild_order`'s comparator.
    pub fn order_is_banded(&self) -> bool {
        self.order.windows(2).all(|w| {
            self.sort_key(&w[0])
                .cmp(&self.sort_key(&w[1]))
                .then_with(|| self.priority_rank(&w[0]).cmp(&self.priority_rank(&w[1])))
                .then_with(|| natural_key_cmp(&w[0], &w[1]))
                != Ordering::Greater
        })
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

        // 1b. The project switcher is a full modal: every key filters/navigates it
        //     (Esc cancels), so it sits above the prefix and all window routing.
        if self.project_switcher.is_some() {
            self.on_switcher_key(key);
            return;
        }

        // 1c. The global all-agents screen (ENG-406) — a third top-level surface — is
        //     a full modal: ↑↓ move, Enter re-roots onto the row, Esc backs out.
        if self.global_view.is_some() {
            self.on_global_key(key);
            return;
        }

        // 1d. The up-front repo multi-select (ENG-536) is a full modal too: while
        //     open it owns the keyboard (space toggles, ↑↓ move, ⏎ launches, Esc
        //     cancels), above the prefix and all window routing — like the switcher.
        if self.repo_select.is_some() {
            self.on_repo_select_key(key);
            return;
        }

        // 1e. The disk-reclaim prompt (ENG-540) is a full modal too.
        if self.reclaim.is_some() {
            self.on_reclaim_key(key);
            return;
        }

        // 2. A pending kill confirmation captures the keyboard: y/Enter confirms,
        //    anything else cancels. Checked before the prefix so the destructive
        //    gesture can't be half-completed by a stray prefix.
        if self.kill_confirm.is_some() {
            self.on_kill_confirm_key(key);
            return;
        }

        // 2b. A pending discard confirmation (ENG-541) likewise captures the
        //     keyboard: y/Enter confirms, anything else cancels.
        if self.discard_confirm.is_some() {
            self.on_discard_confirm_key(key);
            return;
        }

        // 2c. A pending lazy-pull confirmation (ENG-542) — raised by an agent's
        //     `request-repo`. Captured here: below kill/discard (so those win), but
        //     ABOVE the focused agent's PTY (band 6), so a mid-turn agent's keystrokes
        //     can't answer the prompt for you.
        if self.repo_confirm.is_some() {
            self.on_repo_confirm_key(key);
            return;
        }

        // 2d. Quit is also confirmed: a stray prefixed `q` should not tear down a
        //     cockpit that still has live agents and pending context.
        if self.quit_confirm {
            self.on_quit_confirm_key(key);
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
                self.on_search_key(key);
                return;
            }
            self.search_active = false;
        }

        // 5. The three info overlays share one convention: arrows / page keys scroll,
        //    the toggle key or Esc closes, and any other key is ignored so a stray key
        //    can't lose your place mid-read (H3 / A5). Help, summary and ledger all
        //    scroll — no info overlay closes on an arbitrary key anymore.
        if self.show_help {
            if key.code == KeyCode::Esc || self.keymap.action_for(key) == Some(Action::ToggleHelp) {
                self.show_help = false;
            } else {
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.help_scroll = self.help_scroll.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.help_scroll = self.help_scroll.saturating_add(1);
                    }
                    KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(10),
                    KeyCode::PageDown | KeyCode::Char(' ') => {
                        self.help_scroll = self.help_scroll.saturating_add(10);
                    }
                    KeyCode::Home => self.help_scroll = 0,
                    KeyCode::End => self.help_scroll = u16::MAX,
                    _ => {}
                }
            }
            return;
        }
        // The summary scrolls a long dependency list and dismisses only on Esc / `i`,
        // so a stray key while reading can't lose your place (A5).
        if self.show_summary {
            if key.code == KeyCode::Esc
                || self.keymap.action_for(key) == Some(Action::ToggleSummary)
            {
                self.show_summary = false;
            } else {
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.summary_scroll = self.summary_scroll.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.summary_scroll = self.summary_scroll.saturating_add(1);
                    }
                    KeyCode::PageUp => self.summary_scroll = self.summary_scroll.saturating_sub(10),
                    KeyCode::PageDown | KeyCode::Char(' ') => {
                        self.summary_scroll = self.summary_scroll.saturating_add(10);
                    }
                    KeyCode::Home => self.summary_scroll = 0,
                    KeyCode::End => self.summary_scroll = u16::MAX,
                    _ => {}
                }
            }
            return;
        }
        // The ledger scrolls a long run-history and dismisses only on Esc / `t`, matching
        // help and summary so the first arrow reflex never slams an info overlay shut (A5).
        if self.show_ledger {
            if key.code == KeyCode::Esc || self.keymap.action_for(key) == Some(Action::ToggleLedger)
            {
                self.show_ledger = false;
            } else {
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.ledger_scroll = self.ledger_scroll.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.ledger_scroll = self.ledger_scroll.saturating_add(1);
                    }
                    KeyCode::PageUp => self.ledger_scroll = self.ledger_scroll.saturating_sub(10),
                    KeyCode::PageDown | KeyCode::Char(' ') => {
                        self.ledger_scroll = self.ledger_scroll.saturating_add(10);
                    }
                    KeyCode::Home => self.ledger_scroll = 0,
                    KeyCode::End => self.ledger_scroll = u16::MAX,
                    _ => {}
                }
            }
            return;
        }

        // 5b. Global window switch (Alt-←/→): a one-step, wrapping focus move that
        //     fires from ANY focus — including a focused chat, so it must sit ABOVE
        //     the PTY-forwarding match below. Like the prefix, these chords are taken
        //     from the agent's PTY by design; Alt-arrows aren't claude editing keys,
        //     so nothing useful is lost. All modals/overlays/confirms returned above,
        //     so this only ever runs against a real window.
        if let Some(action) = self.keymap.global_for(key) {
            match action {
                Action::CycleNext => self.cycle_window(true),
                Action::CyclePrev => self.cycle_window(false),
                _ => {}
            }
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
            } => {
                // Typing into the very agent that needs you is the strongest "I'm
                // handling it" signal: drop the sticky footer alert now. The
                // per-issue NeedsYou status clears the moment the agent resumes
                // (the UserPromptSubmit / PostToolUse hooks), so the roster/header
                // stay honest until then.
                self.needs_you_alert = false;
                self.forward_to_agent(&issue, key);
            }
            // A coin's deps face navigates exactly like the old Deps pane. Esc pops
            // the re-root history (the one window where Esc conventionally pops a
            // stack), so a deep dive-in has a back-out key, not just Backspace/`b`.
            WindowKind::Coin {
                mode: CoinMode::Deps,
                ..
            } => {
                self.acknowledge();
                if key.code == KeyCode::Esc {
                    // Route through the same path as Backspace/`b` (not a bare
                    // `deps_back`), so a preview coin's Spine highlight re-syncs to the
                    // popped root instead of stranding on the dived-into issue.
                    self.dispatch_deps(Action::Back);
                } else if let Some(action) = self.keymap.action_for(key) {
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
                        Action::ToggleHelp => self.toggle_help(),
                        Action::ToggleSummary => self.toggle_summary(),
                        Action::ToggleLedger => self.toggle_ledger(),
                        _ => {}
                    }
                }
            }
        }
    }

    /// A Spine/Deps keypress clears the transient status line — but does NOT blanket-drop
    /// the sticky needs-you guard. The guard is re-derived: it survives navigation and
    /// the `n` triage jump, and clears only once the agent actually resolves/exits
    /// (CF-11). The one hard "I'm handling it" signal is typing into the needy agent's
    /// own chat (handled separately at the Chat-focus path), not browsing the nav.
    fn acknowledge(&mut self) {
        self.status_msg = None;
        self.clear_needs_you_alert_if_resolved();
    }

    /// Toggle the `?` help overlay, rewinding it to the top each time it opens so a
    /// previous scroll position never hides the first rows (H3).
    fn toggle_help(&mut self) {
        self.show_help = !self.show_help;
        if self.show_help {
            self.help_scroll = 0;
        }
    }

    /// Toggle the `i` summary overlay, rewinding its scroll on open (A5).
    fn toggle_summary(&mut self) {
        if !self.show_summary {
            // Don't open onto nothing (an empty project — `detail_key` is None) or onto
            // a node that has left the graph (archived/moved): render_summary would paint
            // nothing, and since the summary now dismisses only on Esc/`i` (A5) the empty
            // overlay would silently trap every key. Refuse with a footer instead.
            let target = self.detail_key().map(str::to_string);
            match target {
                None => {
                    self.set_footer("no summary — nothing selected".into());
                    return;
                }
                Some(key) if self.graph.get(&key).is_none() => {
                    self.set_footer(format!(
                        "no summary — {key} isn't in this project's graph (archived/moved)"
                    ));
                    return;
                }
                _ => {}
            }
            self.summary_scroll = 0;
        }
        self.show_summary = !self.show_summary;
    }

    /// Toggle the `Ctrl-a t` ledger overlay, rewinding its scroll on open (A5).
    fn toggle_ledger(&mut self) {
        self.show_ledger = !self.show_ledger;
        if self.show_ledger {
            self.ledger_scroll = 0;
        }
    }

    /// Resolve a key pressed after the prefix.
    fn on_prefix_key(&mut self, key: KeyEvent) {
        // A window verb must never fire *underneath* a still-painted info overlay.
        // The prefix is consumed above the band-5 overlay-dismiss, so without this a
        // `Ctrl-a x`/`Ctrl-a d` armed its red confirm behind a ~40-row help card and
        // a reflexive `y` could confirm a kill blind; `Ctrl-a s` floated the switcher
        // over a still-rendered help card (H1). Dismiss any open overlay first, in
        // the same gesture — honouring the documented "any key dismisses it" contract.
        // Capture help's pre-clear state so the M6 `Ctrl-a ?` branch below can *toggle*
        // it (open AND close) rather than only ever re-opening it.
        let help_was_open = self.show_help;
        self.show_help = false;
        self.show_summary = false;
        self.show_ledger = false;

        // Double-prefix → send the literal prefix chord through to a focused
        // agent (a chosen prefix is never wholly unreachable by the PTY). Covers
        // the context window in Chat mode too (its `agent_issue()` is `Some`).
        if self.keymap.is_prefix(key) {
            let agent = self
                .windows
                .focused_kind()
                .agent_issue()
                .map(str::to_string);
            if let Some(issue) = agent {
                self.forward_to_agent(&issue, self.keymap.prefix_event());
            } else {
                // Double-prefix only means anything in a chat (forward the literal
                // chord to its PTY). Elsewhere, acknowledge it instead of eating both
                // keystrokes silently and leaving the next key as a raw direct action.
                self.status_msg = Some(format!(
                    "{p} {p}: no agent here — {p} then a verb",
                    p = self.prefix_label()
                ));
            }
            return;
        }
        // M6: `Ctrl-a ?` toggles the help overlay from inside a Chat coin (or the
        // Fleet), where the direct `?` is swallowed by the PTY — so someone driving an
        // agent can still reach the binding reference. Help isn't a prefix VERB (that
        // would re-introduce the prefix/direct duplication ENG-562 removed), so it
        // wouldn't resolve below; while it's open, `Ctrl-a ?` again — or `?`/Esc, which
        // band 5 handles before focus routing — closes it.
        if self.keymap.action_for(key) == Some(Action::ToggleHelp)
            && matches!(
                self.windows.focused_kind(),
                WindowKind::Coin {
                    mode: CoinMode::Chat,
                    ..
                } | WindowKind::Fleet
            )
        {
            // True toggle: the clear above already set show_help=false, so flip the
            // captured pre-clear state.
            self.show_help = !help_was_open;
            if self.show_help {
                self.help_scroll = 0;
            }
            return;
        }
        let Some(verb) = self.keymap.verb_for(key) else {
            // M5: acknowledge an unbound prefix key instead of silently eating it and
            // leaving the next key to land as a raw direct action.
            self.status_msg = Some(format!(
                "{} {}: no window command — {} for the list",
                self.prefix_label(),
                self.keymap.key_label(key),
                self.keymap.label_for(Action::ToggleHelp),
            ));
            return;
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
            Action::Quit => self.request_quit(),
            Action::StartSearch => {
                self.windows.focus_nav(); // reveal the Spine (clears zoom) before searching (M1)
                self.start_search();
            }
            Action::ToggleHelp => self.toggle_help(),
            Action::ToggleSummary => self.toggle_summary(),
            Action::ToggleLedger => self.toggle_ledger(),
            Action::JumpNeedsYou => self.jump_to_needs_you(),
            Action::SwitchProject => self.open_project_switcher(),
            Action::OpenInEditor => self.open_in_editor(),
            Action::ReclaimMirrors => self.open_reclaim(),
            Action::DiscardWorkspace => self.arm_discard(),
            Action::GlobalView => self.open_global(),
            Action::ConfigureProject => self.request_configure(),
            Action::RestartAgent => self.restart_agent(),
            Action::NextAgent => self.next_agent(),
            Action::DispatchReady => self.dispatch_ready(),
            Action::ChooseRepos => self.dispatch_selection(true),
            Action::AskAgent => self.ask_spawn(),
            // The rest are direct (Spine/Deps) actions, never prefix verbs.
            _ => {}
        }
    }

    /// One-step, wrapping window switch (Alt-←/→). Shares `after_focus_change` with
    /// the prefixed FocusLeft/Right so landing on the Spine reveals it (clears zoom)
    /// and a docked agent that just gained focus lazy-resumes — the move is identical,
    /// only the reach (any focus, no prefix) and the wrap differ.
    fn cycle_window(&mut self, forward: bool) {
        self.windows.cycle_focus(forward);
        self.after_focus_change();
    }

    /// Ask the event loop to re-open the onboarding wizard for the active project
    /// (`Ctrl-a o`): the wizard owns the terminal, so the loop must suspend the
    /// cockpit's alternate screen around it (see [`Self::take_configure_request`]).
    /// Only meaningful for a connected project — in the read-only viewer there's no
    /// active project id to edit, so we footer instead.
    fn request_configure(&mut self) {
        if self.active_project.is_empty() {
            self.set_footer("no connected project to configure — relaunch to set one up".into());
            return;
        }
        self.pending_configure = true;
    }

    /// Taken by the event loop once per request: the project to re-configure, or
    /// `None`. Returning a [`ProjectRef`] (id + display name) is all the wizard needs;
    /// it reloads the registry itself.
    pub fn take_configure_request(&mut self) -> Option<ProjectRef> {
        std::mem::take(&mut self.pending_configure).then(|| ProjectRef {
            id: self.active_project.clone(),
            name: self.graph.project.clone(),
        })
    }

    /// Set the footer/status line — used by the event loop to report the wizard's
    /// outcome after it resumes the cockpit.
    pub fn note_status(&mut self, msg: String) {
        self.set_footer(msg);
    }

    /// Open the focused agent's workspace directory in an external editor (v1.6,
    /// `Ctrl-a e`). Detached and env-inheriting (the user's own handoff tool), so
    /// it survives cockpit teardown. A no-op with a footer when the focused issue
    /// has no agent on disk yet — there's nothing to open until an agent has run.
    fn open_in_editor(&mut self) {
        // Open the workspace of the agent the pane shows (H6: matches dispatch/kill).
        let Some(issue) = self.detail_key().map(str::to_string) else {
            self.set_footer("no agent selected to open".into());
            return;
        };
        let Some(backend) = self.backends.get(&issue) else {
            self.set_footer(format!(
                "{issue}: open an agent first — no workspace on disk yet"
            ));
            return;
        };
        let dir = backend.cwd().to_path_buf();
        if dir.as_os_str().is_empty() || !dir.exists() {
            self.set_footer(format!("{issue}: workspace not on disk yet"));
            return;
        }
        match crate::backend::open_in_editor(&dir) {
            Ok(editor) => self.set_footer(format!("opened {issue} in {editor}")),
            Err(e) => self.set_footer(format!("couldn't open editor: {e}")),
        }
    }

    /// Wire the in-cockpit project switcher when the control plane arms: the
    /// Linear client + runtime that run the off-thread graph fetch, the event
    /// sender that wakes the render loop, and the list of projects to offer.
    pub fn enable_project_switching(
        &mut self,
        linear: Arc<Client>,
        runtime: tokio::runtime::Handle,
        events: AppEventTx,
        projects: Vec<ProjectRef>,
    ) {
        self.linear = Some(linear);
        self.runtime = Some(runtime);
        self.events = Some(events);
        self.project_list = projects;
    }

    /// Open the project switcher overlay, or explain why it can't open. Offers only
    /// *registered* projects (those with a `[[project]]` in `~/.lindep/registry.toml`)
    /// — switching to an unregistered project would swap the graph but never be able
    /// to run agents.
    fn open_project_switcher(&mut self) {
        if self.linear.is_none() || self.runtime.is_none() || self.events.is_none() {
            self.set_footer("project switching needs the agent control plane".into());
            return;
        }
        let choices: Vec<ProjectRef> = self
            .project_list
            .iter()
            .filter(|p| self.mapped_projects.contains(&p.id))
            .cloned()
            .collect();
        if choices.len() < 2 {
            self.set_footer(
                "no other connected project — open one to set it up, or edit ~/.lindep/registry.toml"
                    .into(),
            );
            return;
        }
        self.project_switcher = Some(Picker::new(choices));
    }

    /// Drive the open switcher overlay: type to filter, ↑↓ move, Enter switch,
    /// Esc cancel.
    fn on_switcher_key(&mut self, key: KeyEvent) {
        let Some(picker) = self.project_switcher.as_mut() else {
            return;
        };
        match key.code {
            // Esc and Ctrl-C both cancel — matching the startup picker so the exits
            // are consistent (without Ctrl-C the chord would leak in as filter text).
            KeyCode::Esc => self.project_switcher = None,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.project_switcher = None;
            }
            KeyCode::Enter => {
                let selected = picker.selected();
                self.project_switcher = None;
                if let Some(target) = selected {
                    self.request_switch(target);
                }
            }
            KeyCode::Down => picker.move_by(1),
            KeyCode::Up => picker.move_by(-1),
            KeyCode::Backspace => {
                picker.query.pop();
                picker.refilter();
            }
            // Only unmodified chars filter — a stray Ctrl-/Alt-chord never leaks in.
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                picker.query.push(c);
                picker.refilter();
            }
            _ => {}
        }
    }

    /// Begin a switch to `target`: fetch its issue graph off the render thread
    /// (the network call must not freeze the UI), then wake the loop with
    /// [`AppEvent::ProjectActivated`] to swap it in. A no-op for the current
    /// project.
    fn request_switch(&mut self, target: ProjectRef) {
        if target.id == self.active_project {
            // Re-selecting the project you're on cancels any in-flight switch
            // (bumping the generation makes its result stale) and stays put.
            if self.pending_switch.take().is_some() {
                self.switch_seq += 1;
                self.set_footer(format!("staying on {}", target.name));
            } else {
                self.set_footer(format!("already on {}", target.name));
            }
            return;
        }
        let (Some(linear), Some(runtime), Some(events)) = (
            self.linear.clone(),
            self.runtime.clone(),
            self.events.clone(),
        ) else {
            return;
        };
        // Stamp this switch so the most recently *selected* project wins regardless
        // of which fetch *completes* first (a slow fetch for a superseded switch is
        // dropped at apply time, and never overwrites a newer one in the slot).
        self.switch_seq += 1;
        let generation = self.switch_seq;
        self.pending_switch = Some(target.id.clone());
        self.set_footer(format!("loading {}…", target.name));
        let inbox = Arc::clone(&self.switch_inbox);
        runtime.spawn_blocking(move || match linear.fetch_graph(&target) {
            Ok(graph) => {
                if let Ok(mut slot) = inbox.lock() {
                    // Keep only the highest generation seen, so an older fetch
                    // landing late can't clobber a newer one.
                    if slot.as_ref().is_none_or(|(g, _, _)| generation >= *g) {
                        *slot = Some((generation, target, graph));
                    }
                }
                let _ = events.send(AppEvent::ProjectActivated);
            }
            Err(e) => {
                let _ = events.send(AppEvent::Notification(format!(
                    "switch to {} failed: {e}",
                    target.name
                )));
            }
        });
    }

    /// Swap the cockpit to `project` and its freshly-loaded `graph`. The project we
    /// leave keeps its agents running in the supervisor; we stash its live backends
    /// so a later switch back re-attaches to their real screens. The target's fleet
    /// is repopulated by asking the workspace to (build and) re-emit its statuses.
    fn activate_project(&mut self, project: ProjectRef, graph: Graph) {
        if project.id == self.active_project {
            return;
        }
        self.pending_switch = None;
        // Stash the leaving project's live backends — the `Arc`s stay valid while
        // their agents run, so switching back reveals their real PTYs.
        let leaving = std::mem::take(&mut self.backends);
        if !self.active_project.is_empty() {
            self.stashed_backends
                .insert(self.active_project.clone(), leaving);
        }

        self.active_project = project.id.clone();
        self.graph = graph;
        // Its needs-you now shows in the on-screen fleet (re-emitted below), so drop
        // the backgrounded "elsewhere" tally for the project we're entering.
        self.other_needs_you.remove(&project.id);

        // Restore the target's stashed backends, dropping any whose agent exited
        // while backgrounded (its exit/reap events were filtered out, so the only
        // way to know is to ask the backend).
        let restored = self
            .stashed_backends
            .remove(&project.id)
            .unwrap_or_default();
        self.backends = restored
            .into_iter()
            .filter(|(_, b)| !matches!(b.status(), Lifecycle::Exited(_)))
            .collect();

        // Clean view for the new project — its statuses arrive via the workspace
        // re-emit below; everything else starts fresh.
        self.fleet.clear();
        self.reaped.clear();
        self.kept_worktrees.clear();
        // `unpushed` is intentionally NOT cleared here — it is a cross-project surface
        // (a strand stays a strand whichever project you're viewing), maintained
        // per-project by `note_push_outcome` from both the scope-guard arm (a
        // backgrounded commit) and the main match (the active one), and cleared only by
        // a later clean push or a discard.
        self.ask_agents.clear();
        self.pending_launch.clear();
        self.pending_attach = None;
        self.resuming.clear();
        self.flash.clear();
        self.preview_size.clear();
        self.search_active = false;
        self.search_query.clear();
        // Reset the filter too (its own comment promises "everything else starts
        // fresh"): a has-deps filter carried into a flat-backlog project would hide
        // most of it on arrival while the footer reported the full count.
        self.filter = Filter::All;
        self.needs_you_alert = false;

        // Seed the fleet from the restored live backends BEFORE the async re-emit
        // lands, so a discard/kill on the very next keystroke sees a live agent (and
        // refuses) instead of an empty fleet for ≥1 iteration — the switch-back
        // data-loss window (H2). The workspace re-emit overwrites these placeholders
        // with each issue's true per-issue status moments later.
        let live_issues: Vec<String> = self.backends.keys().cloned().collect();
        for issue in live_issues {
            self.fleet.insert(issue, AgentStatus::Running);
        }

        self.windows = WindowSet::new();
        self.root = most_connected_root(&self.graph);
        if !self.root.is_empty() {
            self.windows
                .ensure_preview(&self.root, CoinMode::Deps, &self.graph);
            self.windows.focus = 0;
        }
        self.rebuild_order();

        // Apply a pending cross-project land (ENG-406): the global screen's Enter or
        // the cross-project needs-you jump asked to land on a specific issue here.
        // Apply it ONLY when it's for THIS project and THIS switch generation (a
        // superseded land — the user fired a later switch — is taken and discarded, so
        // a same-keyed issue in the wrong project is never landed on). Re-root onto it
        // (overriding most_connected_root) and, if requested, attach to its agent.
        let landed = if let Some((pid, issue, attach, land_gen)) = self.pending_land.take()
            && pid == project.id
            && land_gen == self.switch_seq
            && self.graph.get(&issue).is_some()
        {
            self.aim_spine(issue.clone());
            if attach {
                self.open_agent_window(&issue);
            }
            true
        } else {
            false
        };

        // On a plain switch-back (no explicit cross-project land), aim the cursor at
        // a live agent so the "⏎ to open" reassurance actually opens a running agent
        // (M15/NEW-26). `most_connected_root` is pure topology and almost never a
        // running issue, so without this Enter would target a dead root — and launch
        // a *brand-new* agent on it, the opposite of "open". Pick deterministically
        // (lowest key) and only if it's a real node in this graph.
        if !landed {
            let live_issue = self.world.get(&project.id).and_then(|m| {
                m.iter()
                    .filter(|(k, s)| s.is_live() && self.graph.get(k.as_str()).is_some())
                    .map(|(k, _)| k.clone())
                    .min()
            });
            if let Some(issue) = live_issue {
                self.aim_spine(issue);
            }
        }

        // The saved cockpit layout belongs to the project we booted into; once you
        // switch, stop persisting so we don't overwrite it with another project's
        // windows. (Per-project layout persistence is future work.)
        self.cockpit_path = None;
        self.cockpit_dirty = false;

        let mut ledger_note = String::new();
        // Re-point the durable ledger to the target project's own file (H3) while
        // keeping the live in-memory ledger workspace-wide. Save dirty project slices,
        // then merge the target file so its history appears in the overlay without
        // throwing away background events recorded while another project was active.
        let target_handle = self.project_handles.get(&project.id).cloned();
        if let (Some(layout), Some(handle)) = (self.layout.clone(), target_handle) {
            if self.ledger_dirty {
                self.save_ledgers();
            }
            let path = layout.ledger_path(&handle);
            match crate::ledger::Ledger::load(&path) {
                Ok(ledger) => self.ledger.merge_project(&project.id, ledger),
                Err(crate::session::StateError::Version { .. }) => {
                    // Do not overwrite a newer file on the next lifecycle event.
                    self.project_handles.remove(&project.id);
                    ledger_note = " · ledger newer, not writing".into();
                }
                Err(_) => {}
            }
            self.ledger_dirty = false;
            self.ledger_path = Some(path);
        }

        // Bring the target online: build its plane if needed (which reconciles +
        // rehydrates) and re-emit its current fleet statuses. Resume-on-focus
        // reuses the restored live backends; a dead docked agent relaunches.
        if let Some(workspace) = &self.workspace {
            workspace.activate(project.id.clone());
        }
        // Restore resume to the session's standing policy — NOT unconditionally on,
        // which would silently re-enable auto-resume after a switch even under
        // `--no-resume` (where the policy is off and resume_cap is 0).
        self.auto_resume = self.auto_resume_enabled;

        // The switch discarded every docked coin (WindowSet::new above), but the
        // agents themselves keep running — nudge that they're here and one Enter
        // re-opens each, so a switch-back isn't a bare Spine with hidden live work
        // (M15). Count from `world` (the continuously-maintained cross-project tally),
        // since the on-screen fleet re-emit is asynchronous and hasn't landed yet.
        let running_here = self
            .world
            .get(&project.id)
            .map(|m| m.values().filter(|s| s.is_live()).count())
            .unwrap_or(0);
        self.set_footer(if running_here > 0 {
            format!(
                "switched to {} · {} issues · {running_here} agent{} running here — ⏎ to open{}",
                project.name,
                self.graph.len(),
                if running_here == 1 { "" } else { "s" },
                ledger_note,
            )
        } else {
            format!(
                "switched to {} · {} issues{}",
                project.name,
                self.graph.len(),
                ledger_note
            )
        });
    }

    /// Direct keys while the Spine is focused.
    fn dispatch_spine(&mut self, action: Action) {
        match action {
            Action::MoveDown => self.move_selection(1),
            Action::MoveUp => self.move_selection(-1),
            Action::MoveTop => self.aim_index(0),
            Action::MoveBottom => self.aim_index(self.order.len().saturating_sub(1)),
            Action::PageUp => {
                let cur = self.list_state.selected().unwrap_or(0);
                self.aim_index(cur.saturating_sub(self.spine_page()));
            }
            Action::PageDown => {
                let cur = self.list_state.selected().unwrap_or(0);
                self.aim_index(cur + self.spine_page());
            }
            // Enter / Space are the attach+spawn button on the Spine.
            Action::Enter | Action::ToggleCollapse => self.button(),
            // Tab flips the active coin chat⇄deps while you browse the nav.
            Action::ContextToggle => self.flip_active_coin(),
            Action::OpenDeps => self.open_deps_for_selection(),
            Action::OpenFleet => self.open_fleet(),
            Action::PinWindow => self.pin_window(),
            Action::JumpCycle => self.jump_to_cycle(),
            Action::JumpNeedsYou => self.jump_to_needs_you(),
            Action::CycleFilter => {
                self.filter = self.filter.next();
                self.rebuild_order();
                // Every state-changing direct action leaves a one-line trace, so the
                // footer is a reliable "that did something" signal (B0d) — and an empty
                // result self-explains rather than reading as a lost project.
                self.set_footer(if self.order.is_empty() {
                    format!(
                        "filter:{} · 0 of {} — clear to list",
                        self.filter.label(),
                        self.graph.len()
                    )
                } else {
                    format!("filter:{}", self.filter.label())
                });
            }
            Action::StartSearch => self.start_search(),
            Action::ToggleHelp => self.toggle_help(),
            Action::ToggleSummary => self.toggle_summary(),
            Action::ToggleLedger => self.toggle_ledger(),
            _ => {}
        }
    }

    /// Direct keys while a coin's deps face is focused (the per-issue tree drives
    /// its own cursor).
    fn dispatch_deps(&mut self, action: Action) {
        // Operations needing the graph are split out so the cursor borrow and the
        // `&self.graph` borrow don't overlap.
        let page = self.deps_page() as i32;
        match action {
            Action::MoveDown => self.with_deps(|c| c.move_selection(1)),
            Action::MoveUp => self.with_deps(|c| c.move_selection(-1)),
            Action::MoveTop => self.with_deps(|c| c.move_to_edge(false)),
            Action::MoveBottom => self.with_deps(|c| c.move_to_edge(true)),
            Action::PageUp => self.with_deps(move |c| c.page(-page)),
            Action::PageDown => self.with_deps(move |c| c.page(page)),
            Action::SwitchSide => self.with_deps(|c| c.switch_side()),
            Action::Enter => self.deps_enter(),
            Action::ToggleCollapse => self.deps_collapse(),
            Action::Back => self.deps_back(),
            // Tab flips this coin from its deps face to its chat face.
            Action::ContextToggle => self.flip_active_coin(),
            Action::OpenDeps => self.open_deps_for_selection(),
            Action::OpenFleet => self.open_fleet(),
            Action::PinWindow => self.pin_window(),
            Action::ToggleHelp => self.toggle_help(),
            Action::ToggleSummary => self.toggle_summary(),
            Action::ToggleLedger => self.toggle_ledger(),
            _ => {}
        }
        // The transient preview follows the selection both ways: re-rooting its
        // deps tree (Enter dives in, Back pops out) re-aims the Spine so the nav
        // list tracks where you are — fixing "the nav bar stays on the previous
        // issue". A pinned coin is an independent explorer; it never moves the
        // Spine. The cursor (and its Back history) is left intact either way.
        if matches!(action, Action::Enter | Action::Back) && self.windows.focused().is_preview() {
            self.sync_spine_to_focused_deps();
        }
    }

    /// Re-aim the Spine selection onto the focused coin's current deps-cursor root,
    /// so navigating the preview's dependency tree drags the nav-list highlight
    /// with it. Only the selection moves — the cursor and its history stay put, so
    /// in-place re-root / Back still work.
    fn sync_spine_to_focused_deps(&mut self) {
        if let Some(root) = self.windows.focused().deps.as_ref().map(|c| c.root.clone()) {
            if root != self.root {
                self.root = root.clone();
            }
            self.sync_list_selection();
            // If the deps cursor landed on an issue that already has a pinned coin,
            // focus that real coin rather than leaving the preview aimed at a hidden
            // duplicate. This keeps switch-back / re-root flows attached to the
            // running agent the user can actually type into.
            if self.windows.focused().is_preview() && self.windows.has_pinned_coin(&root) {
                if let Some(removed) = self.windows.clear_preview()
                    && let Some((issue, CoinMode::Chat)) = removed.kind.coin()
                {
                    let issue = issue.to_string();
                    self.reclaim_if_dead(&issue);
                }
                if let Some(i) = self.windows.pinned_coin_index(&root) {
                    self.windows.focus = i;
                    self.after_focus_change();
                }
            }
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
    }

    fn move_selection(&mut self, delta: i32) {
        let len = self.order.len();
        if len == 0 {
            return;
        }
        let prev = self.list_state.selected();
        let next = match prev {
            // From a filter-hidden selection (no highlight), step into the list at the
            // near end rather than skipping row 0 (down) or flinging to the last row (up).
            None => {
                if delta >= 0 {
                    0
                } else {
                    len - 1
                }
            }
            Some(cur) => (cur as i32 + delta).rem_euclid(len as i32) as usize,
        };
        self.list_state.select(Some(next));
        // The wrap is the only fast cross-list traversal, so keep it — but flag the
        // top↔bottom teleport (there's no scrollbar; a silent full-viewport jump reads
        // as an accident). Only when the selection actually crossed the boundary —
        // `cur != next` suppresses a spurious "wrapped" footer on a single-row Spine,
        // where rem_euclid keeps `next == cur == 0 == len-1`.
        if let Some(cur) = prev
            && cur != next
        {
            if cur == 0 && next == len - 1 {
                self.set_footer("top of schedule — wrapped to the bottom".into());
            } else if cur == len - 1 && next == 0 {
                self.set_footer("bottom of schedule — wrapped to the top".into());
            }
        }
        if let Some(k) = self.order.get(next).cloned() {
            self.root = k; // list navigation re-aims the selection
            self.reaim_preview(); // …and the preview coin that follows it
        }
    }

    /// Aim the Spine at an absolute index (clamped), re-aiming the root and the
    /// follower preview. Unlike [`move_selection`] this never wraps — the top/bottom
    /// jumps and paging want a hard edge, not a teleport to the far end (M3).
    fn aim_index(&mut self, i: usize) {
        if self.order.is_empty() {
            return;
        }
        let i = i.min(self.order.len() - 1);
        self.list_state.select(Some(i));
        self.root = self.order[i].clone();
        self.reaim_preview();
    }

    /// One screenful of Spine rows, for paging — derived from the last known
    /// terminal height, with a floor so a tiny pane still advances.
    fn spine_page(&self) -> usize {
        (self.viewport.height as usize).saturating_sub(5).max(1)
    }

    /// One screenful of *deps-tree* rows — the focused deps coin can be a small tiled
    /// pane, so paging it by the whole-terminal `spine_page` would overshoot. Uses the
    /// tree height captured at render, falling back to `spine_page` before the first
    /// deps render; floored so a tiny pane still advances (M3).
    fn deps_page(&self) -> usize {
        if self.deps_view_h > 0 {
            (self.deps_view_h as usize).saturating_sub(1).max(1)
        } else {
            self.spine_page()
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
        // Flip an existing pinned coin for this issue to its Deps face rather than
        // minting a second coin for the same identity (the one-coin-per-issue
        // invariant; without this, `d` on a pinned issue duplicated it).
        if let Some(i) = self.windows.pinned_coin_index(&root) {
            self.windows.focus = i;
            if !matches!(
                self.windows.windows[i].kind,
                WindowKind::Coin {
                    mode: CoinMode::Deps,
                    ..
                }
            ) {
                self.windows.flip_coin_face(i, &self.graph);
            }
            self.after_focus_change();
            return;
        }
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
            || self.pending_launch.contains_key(issue)
            || self.resuming.contains_key(issue);
        if live { CoinMode::Chat } else { CoinMode::Deps }
    }

    fn is_starting(&self, issue: &str) -> bool {
        self.pending_launch.contains_key(issue) || self.resuming.contains_key(issue)
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
        // From the Spine, flip the coin that's actually the big pane: the selection's
        // pinned coin, else its preview. (`preview_index` alone was None for a pinned
        // selection — `reaim_preview` clears the preview there — so Tab was a dead key
        // in exactly the case where a coin is provably on screen.)
        } else if let Some(p) = self.active_index() {
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
    /// The first unresolved upstream blocker of `key`, if any — the issue a
    /// blocked dispatch is waiting on (mirrors `Graph::is_blocked`'s predicate),
    /// used to name it in the refusal footer.
    fn first_unresolved_blocker(&self, key: &str) -> Option<String> {
        self.graph
            .neighbours(key, Direction::Upstream)
            .iter()
            .find(|b| self.graph.get(b).is_none_or(|i| !i.status.is_resolved()))
            .cloned()
    }

    /// The workspace-wide capacity-gate load: agents that hold a genuinely-live
    /// process across the active project and any backgrounded project. A terminal
    /// agent (Done/Failed/Stopped) whose EXITED card is still windowed keeps a
    /// backend but is NOT live, so counting `backends.len()` would falsely refuse a
    /// launch when nothing is actually running.
    fn live_agent_count(&self) -> usize {
        self.workspace_summary().0
    }

    fn ensure_ask_issue(&mut self, key: &str, title: &str) -> bool {
        let added = self.graph.get(key).is_none();
        if added {
            self.graph.add_issue(Issue {
                key: key.to_string(),
                title: title.to_string(),
                status: Status::Started,
                priority: Priority::None,
                assignee: None,
                external: false,
            });
        }
        self.ask_agents.insert(key.to_string());
        added
    }

    fn reveal_ask_issue(&mut self, key: &str) {
        self.filter = Filter::All;
        self.search_active = false;
        self.search_query.clear();
        self.root = key.to_string();
        self.rebuild_order();
    }

    /// Re-arm a synthetic ad-hoc agent's `ask_agents` membership and Spine node when
    /// a live hook arrives for it and the node was pruned (e.g. by a reconcile on a
    /// project switch-back). Called at the top of the live status handlers, BEFORE
    /// their reaped/terminal guards — so it must enforce the same tombstone itself:
    /// a late in-flight hook for a *killed* (reaped) or already-*terminal* ask agent
    /// must NOT resurrect it into the Spine. `ensure_ask_issue` is unconditional, so
    /// without this guard a stray post-cancel `AgentNeedsYou`/`AgentAction` would
    /// re-inject a dead row. The legitimate restore path is a still-live ask agent
    /// (neither reaped nor terminal), which both guards leave untouched.
    fn restore_ask_issue_if_needed(&mut self, key: &str) {
        if self.reaped.contains(key) || self.is_terminal(key) {
            return;
        }
        if crate::worktree::is_synthetic_ask_id(key) && self.ensure_ask_issue(key, "Ad-hoc agent") {
            self.rebuild_order();
        }
    }

    fn ask_spawn(&mut self) {
        let load = self.live_agent_count() + self.resuming.len();
        if self.resume_cap > 0 && load >= self.resume_cap {
            self.set_footer(format!(
                "fleet at capacity ({load}/{}) — finish or stop an agent first",
                self.resume_cap
            ));
            return;
        }

        let Some(workspace) = self.workspace.clone() else {
            self.status_msg = Some(if self.demo {
                "read-only demo — ask agents need a real project; drop --demo".into()
            } else {
                self.degrade_reason
                    .as_ref()
                    .map(|r| format!("agent control plane unavailable: {r}"))
                    .unwrap_or_else(|| "agent control plane unavailable".into())
            });
            return;
        };

        let key = crate::worktree::synthetic_ask_id();
        let title = "Ad-hoc agent".to_string();
        let size = self.agent_spawn_size();
        self.ensure_ask_issue(&key, &title);
        self.reveal_ask_issue(&key);

        let choices = self
            .project_candidates
            .get(&self.active_project)
            .cloned()
            .unwrap_or_default();
        if choices.len() > 1 {
            self.repo_select = Some(RepoSelect {
                picker: RepoPicker::new(choices),
                issue: key.clone(),
                title,
                size,
                adhoc: true,
            });
            self.open_agent_window(&key);
            self.set_footer(
                "select repos for ad-hoc agent · space toggles · ⏎ launch · esc cancel".into(),
            );
            return;
        }

        workspace.launch_ask(
            self.active_project.clone(),
            key.clone(),
            title,
            size,
            Vec::new(),
        );
        self.pending_launch
            .insert(key.clone(), self.frame + RESUME_GRACE_FRAMES);
        self.pending_attach = Some(key.clone());
        self.open_agent_window(&key);
        self.set_footer(format!("opening ad-hoc agent {key}…"));
    }

    fn button(&mut self) {
        self.dispatch_selection(false);
    }

    /// Dispatch the on-screen selection (the `Enter` / `Ctrl-a Enter` button, and the
    /// `Ctrl-a c` "choose repos" verb). `force_repo_select` opens the repo multi-select
    /// even on a single-candidate project — the on-demand entry that lets you give one
    /// agent more than one repo (and add a registered repo the project doesn't yet
    /// list); the plain button leaves it `false`, keeping the fast single-repo path.
    fn dispatch_selection(&mut self, force_repo_select: bool) {
        // Ctrl-a Enter / Enter dispatches the SELECTION from any focus — but in a
        // re-rooted deps coin the perceived selection is the cursor root the pane shows
        // (H6), not the stale Spine root. A chat coin (or the Spine/Fleet) is not a
        // selection move, so it dispatches the Spine selection exactly as before — which
        // is what lets you browse the nav while focused in a chat and launch the row.
        let target = match self.windows.focused_kind() {
            WindowKind::Coin { .. } => self.detail_key().map(str::to_string),
            _ => (!self.root.is_empty()).then(|| self.root.clone()),
        };
        let Some(key) = target else {
            self.status_msg = Some("no issue selected".into());
            return;
        };
        let Some(issue) = self.graph.get(&key) else {
            self.status_msg = Some(format!("{key} isn't a dispatchable issue here"));
            return;
        };
        if issue.external {
            let k = issue.key.clone();
            self.status_msg = Some(format!("{k} is external — launch it in its own project"));
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
        if self.pending_launch.contains_key(&key) {
            self.open_agent_window(&key);
            self.set_footer(format!("already opening an agent on {key}…"));
            return;
        }
        // Readiness gate (ENG-559): manual dispatch *is* this button. Refuse a
        // blocked issue with a deps-aware footer instead of launching an agent
        // that can't make progress (today it launches one silently). A live or
        // needs-you agent has a backend and was handled above, so only
        // Ready / Blocked / Done reach here.
        if self.readiness(&key) == Readiness::Blocked {
            let msg = match self.first_unresolved_blocker(&key) {
                Some(blocker) => format!("{key} blocked by {blocker} — dispatch refused"),
                None => format!("{key} is in a dependency cycle — dispatch refused"),
            };
            self.set_footer(msg);
            return;
        }
        // Capacity gate: the workspace caps concurrent agents (`max_concurrent`).
        // A refusal here is NOT a dependency block — say so plainly, or a full
        // fleet reads as a bogus dep block (RFC §6).
        let load = self.live_agent_count() + self.resuming.len();
        if self.resume_cap > 0 && load >= self.resume_cap {
            self.set_footer(format!(
                "fleet at capacity ({load}/{}) — finish or stop an agent first",
                self.resume_cap
            ));
            return;
        }
        // Absent, or terminal → (re)launch; the supervisor resumes transparently
        // if a session already exists. Open the window now (a "starting…" card)
        // so the press registers; `AgentSpawned` fills in the backend.
        match self.workspace.clone() {
            Some(workspace) => {
                // Spawn the PTY at the tile we'll render it in, so claude's first
                // paint already fits (no full-width reflow flash beside a pin).
                let size = self.agent_spawn_size();
                // Up-front repo select (ENG-536): a project with more than one
                // candidate repo lets the user pick which the issue needs before
                // launch. One candidate (the common single-repo case) launches
                // straight away — no modal — so the default path is unchanged. The
                // `Ctrl-a c` verb forces the modal open even on a single candidate
                // (CF-20), the on-demand way to add a second repo to one agent.
                let choices = self
                    .project_candidates
                    .get(&self.active_project)
                    .cloned()
                    .unwrap_or_default();
                if choices.len() > 1 || (force_repo_select && !choices.is_empty()) {
                    self.repo_select = Some(RepoSelect {
                        picker: RepoPicker::new(choices),
                        issue: key.clone(),
                        title,
                        size,
                        adhoc: false,
                    });
                    self.set_footer(format!(
                        "select repos for {key} · space toggles · a add repo · ⏎ launch · esc cancel"
                    ));
                    return;
                }
                workspace.launch(self.active_project.clone(), key.clone(), title, size);
                self.pending_launch
                    .insert(key.clone(), self.frame + RESUME_GRACE_FRAMES);
                self.pending_attach = Some(key.clone());
                self.open_agent_window(&key);
                // Single-candidate fast path: the multi-repo selector never appeared,
                // so name the verb that summons it — otherwise giving one agent several
                // repos is undiscoverable on a single-repo project (CF-20).
                let choose = self.keymap.verb_label(Action::ChooseRepos);
                self.set_footer(format!("opening agent on {key}…  ({choose} to add repos)"));
            }
            None => {
                self.status_msg = Some(if self.demo {
                    "read-only demo — agents need a real project; drop --demo".into()
                } else {
                    // Reuse the real degrade reason instead of opaque jargon (M13).
                    self.degrade_reason
                        .clone()
                        .unwrap_or_else(|| "agent control plane unavailable".into())
                });
            }
        }
    }

    /// Restart the on-screen issue's agent in one press (`Ctrl-a r`): reclaim a dead
    /// backend, then relaunch — collapsing the kill→reselect→Enter ritual that made
    /// re-dispatching a crash harder than causing it (CF-14). A still-live agent is
    /// refused (its worktree is in use), so this never silently kills running work.
    fn restart_agent(&mut self) {
        let Some(issue) = self.detail_key().map(str::to_string) else {
            self.status_msg = Some("no agent here to restart".into());
            return;
        };
        if self.fleet.get(&issue).is_some_and(AgentStatus::is_live) || self.has_live_backend(&issue)
        {
            self.set_footer(format!(
                "{issue} is still live — stop it ({}) before restarting",
                self.keymap.verb_label(Action::KillWindow)
            ));
            return;
        }
        // The agent is dead (a live one was refused above): close its window + drop the
        // backend (and any kill tombstone) so `button` relaunches (resuming the session)
        // instead of re-opening the corpse card. `reclaim_if_dead` won't free a *windowed*
        // backend, so undock first; aim the Spine so the relaunch targets this issue.
        self.undock_issue(&issue);
        self.backends.remove(&issue);
        self.reaped.remove(&issue);
        self.aim_spine(issue);
        self.button();
    }

    /// Walk to the next live agent (any state), wrapping, focusing its chat (`Ctrl-a j`)
    /// — "how are my agents doing" without hunting the Spine. Mirrors `jump_to_needs_you`
    /// but over the whole live fleet, not just the needy ones (CF-14).
    fn next_agent(&mut self) {
        let mut members: Vec<String> = self
            .fleet
            .iter()
            .filter(|(_, s)| s.is_live())
            .map(|(k, _)| k.clone())
            .collect();
        members.sort_by(|a, b| natural_key_cmp(a, b));
        if members.is_empty() {
            self.status_msg = Some("no live agents to walk".into());
            return;
        }
        let next = members
            .iter()
            .position(|k| *k == self.root)
            .map_or(0, |i| (i + 1) % members.len());
        let (key, n, total) = (members[next].clone(), next + 1, members.len());
        self.set_jump_status("agent", &key, n, total);
        // Land in the agent's conversation when it's docked; otherwise the follower
        // preview (re-aimed by set_jump_status) already shows it.
        if !self.focus_pinned_chat(&key) {
            self.windows.focus_preview();
            self.after_focus_change();
        }
    }

    /// Launch every READY issue up to the concurrency cap in one press (`Ctrl-a g`) —
    /// turning the "I cleared the blockers, go" moment into one intentional action with
    /// a partial-success trace, instead of a row of identical Enters that abort silently
    /// at capacity (CF-14). (The over-cap remainder stays READY to re-press; a persistent
    /// auto-draining queue is a noted follow-up.)
    fn dispatch_ready(&mut self) {
        if self.workspace.is_none() {
            self.status_msg = Some(
                self.degrade_reason
                    .clone()
                    .unwrap_or_else(|| "agents need a real project".into()),
            );
            return;
        }
        let mut ready: Vec<String> = self
            .graph
            .keys()
            .iter()
            .filter(|k| {
                self.graph.get(k).is_some_and(|i| !i.external)
                    && self.readiness(k) == Readiness::Ready
                    && !self.backends.contains_key(*k)
                    && !self.pending_launch.contains_key(*k)
            })
            .cloned()
            .collect();
        ready.sort_by(|a, b| {
            self.sort_key(a)
                .cmp(&self.sort_key(b))
                .then_with(|| self.priority_rank(a).cmp(&self.priority_rank(b)))
                .then_with(|| natural_key_cmp(a, b))
        });
        if ready.is_empty() {
            self.set_footer("nothing READY to dispatch".into());
            return;
        }
        let budget = if self.resume_cap == 0 {
            ready.len()
        } else {
            self.resume_cap
                .saturating_sub(self.live_agent_count() + self.resuming.len())
        };
        let take = budget.min(ready.len());
        for key in ready.iter().take(take) {
            self.launch_issue(key);
        }
        let left = ready.len() - take;
        self.set_footer(if left == 0 {
            format!(
                "dispatched {take} READY {}",
                if take == 1 { "issue" } else { "issues" }
            )
        } else {
            format!("launched {take} · {left} still ready — fleet at capacity, finish some to launch more")
        });
    }

    /// The bare launch path `button` runs after its readiness/capacity gates, minus the
    /// repo-select modal and the focus-grabbing window open — so batch dispatch can fire
    /// many at once silently (each issue's row shows the spawning marker). A multi-repo
    /// issue launches its primary repo only here.
    fn launch_issue(&mut self, key: &str) {
        let Some(workspace) = self.workspace.clone() else {
            return;
        };
        let title = self
            .graph
            .get(key)
            .map(|i| i.title.clone())
            .unwrap_or_default();
        let size = self.agent_spawn_size();
        workspace.launch(self.active_project.clone(), key.to_string(), title, size);
        self.pending_launch
            .insert(key.to_string(), self.frame + RESUME_GRACE_FRAMES);
    }

    /// Drive the open repo multi-select (ENG-536): space toggles the cursor's repo,
    /// ↑↓ move, Enter launches with the checked set, Esc/Ctrl-C cancels (no launch
    /// was sent yet, so cancelling is clean). `a` opens the "add another repo"
    /// sub-list (CF-20), which then captures movement/confirm until it closes.
    fn on_repo_select_key(&mut self, key: KeyEvent) {
        if self.repo_select.is_none() {
            return;
        }
        // While the add-list is open, keys drive IT: ↑↓ move, Enter pulls the repo into
        // the checklist, Esc backs out to the main checklist (not the whole modal).
        if self
            .repo_select
            .as_ref()
            .is_some_and(|s| s.picker.is_adding())
        {
            let picker = &mut self.repo_select.as_mut().expect("Some").picker;
            match key.code {
                KeyCode::Esc => picker.cancel_add(),
                KeyCode::Down => picker.move_by(1),
                KeyCode::Up => picker.move_by(-1),
                KeyCode::Enter => picker.confirm_add(),
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc => {
                let issue = self.repo_select.take().map(|s| s.issue).unwrap_or_default();
                self.set_footer(format!("launch cancelled for {issue}"));
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.repo_select = None;
                self.set_footer("launch cancelled".into());
            }
            KeyCode::Char(' ') => self.repo_select.as_mut().expect("Some").picker.toggle(),
            KeyCode::Char('a') => {
                // Open the add-list over registered repos this project doesn't yet list.
                // Disjoint fields (`repo_select` mut, `registered_repos` shared), so this
                // borrow pair is fine; footer the empty case after releasing the borrow.
                let opened = self
                    .repo_select
                    .as_mut()
                    .expect("Some")
                    .picker
                    .open_add(&self.registered_repos);
                if !opened {
                    self.set_footer("no other registered repos to add".into());
                }
            }
            KeyCode::Down => self.repo_select.as_mut().expect("Some").picker.move_by(1),
            KeyCode::Up => self.repo_select.as_mut().expect("Some").picker.move_by(-1),
            KeyCode::Enter => {
                let RepoSelect {
                    picker,
                    issue,
                    title,
                    size,
                    adhoc,
                } = self.repo_select.take().expect("repo_select is Some here");
                let repos = picker.selected_handles();
                // Persist any repo the human added beyond the project's candidates — a
                // deliberate widening of the trust boundary that sticks (CF-20).
                self.persist_added_candidates(&repos);
                let Some(workspace) = self.workspace.clone() else {
                    self.status_msg = Some("agent control plane unavailable".into());
                    return;
                };
                if adhoc {
                    workspace.launch_ask(
                        self.active_project.clone(),
                        issue.clone(),
                        title,
                        size,
                        repos,
                    );
                } else {
                    workspace.launch_with_repos(
                        self.active_project.clone(),
                        issue.clone(),
                        title,
                        size,
                        repos,
                    );
                }
                self.pending_launch
                    .insert(issue.clone(), self.frame + RESUME_GRACE_FRAMES);
                self.pending_attach = Some(issue.clone());
                self.open_agent_window(&issue);
                let label = if adhoc { "ad-hoc agent" } else { "agent" };
                self.set_footer(format!("opening {label} on {issue}…"));
            }
            _ => {}
        }
    }

    /// Persist every handle in `chosen` that the active project doesn't yet list as a
    /// candidate — the repos the human added via the picker's "add another repo" (CF-20).
    /// Two effects, both so the next launch needs no restart: append it to the in-session
    /// `project_candidates` (so a plain `Enter` next time offers it), and write it into
    /// `registry.toml` (so it survives, and the agent may `request-repo` it later). A
    /// registry write failure footers but never blocks the launch already in flight.
    fn persist_added_candidates(&mut self, chosen: &[String]) {
        let active = self.active_project.clone();
        let existing: std::collections::HashSet<String> = self
            .project_candidates
            .get(&active)
            .map(|c| c.iter().map(|r| r.handle.clone()).collect())
            .unwrap_or_default();
        for handle in chosen {
            if existing.contains(handle) {
                continue;
            }
            // In-session: make it a candidate now (carry the local-only flag the picker
            // shows). Skipped if it isn't a known registered repo — nothing to surface.
            if let Some(meta) = self
                .registered_repos
                .iter()
                .find(|r| &r.handle == handle)
                .cloned()
            {
                let candidates = self.project_candidates.entry(active.clone()).or_default();
                if !candidates.iter().any(|c| &c.handle == handle) {
                    candidates.push(RepoChoice {
                        handle: meta.handle,
                        local: meta.local,
                        primary: false,
                    });
                }
            }
            // Durable: widen the project's candidate set in registry.toml.
            if let Some(layout) = self.layout.clone()
                && let Err(e) = crate::registry::add_candidate(&layout, &active, handle)
            {
                self.set_footer(format!("couldn't persist repo {handle}: {e}"));
            }
        }
    }

    /// Open the disk-reclaim prompt (ENG-540, `Ctrl-a m`): scan for unreferenced
    /// mirrors so their object DBs can be freed. The scan recurses each bare
    /// object DB to size it (GB-scale on a large repo), so it runs on the blocking
    /// pool and the result rides back as a [`AppEvent::ReclaimScanned`] — never on
    /// the synchronous render loop. A footer no-op when the control plane is off
    /// (no layout); falls back to an inline scan only when there's no runtime to
    /// offload to (headless tests), which is identical to the pre-offload path.
    fn open_reclaim(&mut self) {
        let Some(layout) = self.layout.clone() else {
            self.set_footer("disk reclaim needs the agent control plane".into());
            return;
        };
        match (self.runtime.clone(), self.events.clone()) {
            (Some(rt), Some(tx)) => {
                self.set_footer("scanning mirrors…".into());
                rt.spawn_blocking(move || {
                    let mirrors = crate::mirror::reclaimable_mirrors(&layout);
                    let _ = tx.send(AppEvent::ReclaimScanned {
                        mirrors,
                        opening: true,
                        note: None,
                    });
                });
            }
            _ => {
                let mirrors = crate::mirror::reclaimable_mirrors(&layout);
                self.apply_reclaim_scan(mirrors, true, None);
            }
        }
    }

    /// Fold a (possibly off-thread) reclaim scan result into the prompt. `opening`
    /// distinguishes the initial `Ctrl-a m` scan — which opens the modal, or
    /// footers when nothing is reclaimable — from a post-delete rescan, which
    /// reports `note` (the delete outcome) and then refreshes the modal *only if
    /// it's still open* (the user may have pressed Esc while the rescan was in
    /// flight, and a late result must not resurrect a closed prompt). Clears the
    /// busy latch either way.
    fn apply_reclaim_scan(
        &mut self,
        mirrors: Vec<crate::mirror::ReclaimableMirror>,
        opening: bool,
        note: Option<String>,
    ) {
        self.reclaim_busy = false;
        if let Some(note) = note {
            self.set_footer(note);
        }
        if opening {
            if mirrors.is_empty() {
                self.set_footer("nothing to reclaim — every mirror is in use".into());
                self.reclaim = None;
            } else {
                self.reclaim = Some(ReclaimPrompt::new(mirrors));
            }
        } else if self.reclaim.is_some() {
            self.reclaim = (!mirrors.is_empty()).then(|| ReclaimPrompt::new(mirrors));
        }
    }

    /// Drive the disk-reclaim prompt: ↑↓ move, Enter / `d` reclaims the selected
    /// mirror (refused with a footer if it's somehow become referenced since the
    /// scan — the alternates guard), Esc closes. The delete (`delete_mirror` takes a
    /// blocking cross-process flock and `remove_dir_all`s a GB-scale object DB) runs
    /// on the blocking pool; the prompt rescans and closes from the resulting
    /// [`AppEvent::ReclaimScanned`]. A `reclaim_busy` latch swallows a second Enter
    /// until that result lands so one keypress can't fire two deletes.
    fn on_reclaim_key(&mut self, key: KeyEvent) {
        let Some(prompt) = self.reclaim.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.reclaim = None,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.reclaim = None;
            }
            KeyCode::Down => prompt.move_by(1),
            KeyCode::Up => prompt.move_by(-1),
            KeyCode::Enter | KeyCode::Char('d') => {
                if self.reclaim_busy {
                    return;
                }
                let target = prompt.selected();
                // Drop the modal borrow before touching other fields / rescanning.
                let (Some(layout), Some(target)) = (self.layout.clone(), target) else {
                    self.reclaim = None;
                    return;
                };
                let handle = target.handle;
                match (self.runtime.clone(), self.events.clone()) {
                    (Some(rt), Some(tx)) => {
                        self.reclaim_busy = true;
                        self.set_footer(format!("reclaiming mirror {handle}…"));
                        rt.spawn_blocking(move || {
                            let note = reclaim_note(
                                crate::mirror::delete_mirror(&layout, &handle),
                                &handle,
                            );
                            let mirrors = crate::mirror::reclaimable_mirrors(&layout);
                            let _ = tx.send(AppEvent::ReclaimScanned {
                                mirrors,
                                opening: false,
                                note: Some(note),
                            });
                        });
                    }
                    _ => {
                        let note =
                            reclaim_note(crate::mirror::delete_mirror(&layout, &handle), &handle);
                        let mirrors = crate::mirror::reclaimable_mirrors(&layout);
                        self.apply_reclaim_scan(mirrors, false, Some(note));
                    }
                }
            }
            _ => {}
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
        // From the Spine, pin graduates the previewed coin for the current
        // selection — pin *that issue's view* without Tab-ing into it first (A2);
        // focus then returns to the Spine so you keep browsing.
        let from_spine = self.windows.focus == 0;
        if from_spine {
            if self.root.is_empty() {
                self.status_msg = Some("nothing selected to pin".into());
                return;
            }
            if self.windows.has_pinned_coin(&self.root) {
                // `p` from the nav is a toggle: a second press on an
                // already-pinned selection *unpins* it (undock by identity).
                // undock_issue reaims the preview so the issue stays on screen
                // and a live agent keeps running, and it never touches focus, so
                // you stay on the Spine and keep browsing.
                let issue = self.root.clone();
                let running = self.fleet.get(&issue).is_some_and(AgentStatus::is_live);
                self.undock_issue(&issue);
                self.cockpit_dirty = true;
                self.set_footer(if running {
                    format!("unpinned {issue} · still running — ⏎ to re-open")
                } else {
                    format!("unpinned {issue}")
                });
                return;
            }
            // Aim the preview at the selection, then focus it so the graduate path
            // below treats it exactly like pinning from inside the issue's window.
            self.reaim_preview();
            self.windows.focus_preview();
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
            if from_spine {
                self.windows.focus = 0; // back to the Spine to keep browsing
            }
            self.cockpit_dirty = true;
            self.set_footer(match outcome {
                GraduateOutcome::Merged(_) => format!("{label} is already pinned"),
                _ => format!("pinned {label} · stays while you browse"),
            });
            return;
        }
        // An already-pinned coin / Fleet → unpin = undock (close it; a live agent
        // keeps running — re-open it by selecting its Spine row and pressing Enter).
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
            // Close = undock: a *live* agent keeps running, so only reclaim its
            // backend once it's actually dead. Re-open it later by selecting its
            // Spine row and pressing Enter — Enter re-attaches the retained backend.
            let running = self.fleet.get(&issue).is_some_and(AgentStatus::is_live);
            self.reclaim_if_dead(&issue);
            self.set_footer(if running {
                format!("closed {issue} · still running — select it & ⏎ to re-open")
            } else {
                format!("closed {issue}")
            });
        } else {
            self.status_msg = Some("closed window".into()); // the Fleet overview
        }
        // Re-seed the follower preview for the current selection so the thing you're
        // looking at never vanishes: unpinning the *selected* issue's coin demotes it
        // back to the preview (it stays on screen) instead of closing to an empty big
        // pane. Mirrors `undock_issue`, which already reaims after a close-by-identity.
        self.reaim_preview();
    }

    /// Arm a confirmed kill of the focused agent (`Ctrl-a x`). Kill is destructive
    /// and separate from close, so it's never a single keystroke.
    /// True when `issue` has a restored/live backend `Arc` whose process hasn't
    /// exited — ground truth even in the switch-back window where the fleet entry
    /// hasn't been re-emitted yet, so a discard/kill can never act on a live agent
    /// through an empty-fleet gap (H2).
    fn has_live_backend(&self, issue: &str) -> bool {
        self.backends
            .get(issue)
            .is_some_and(|b| !matches!(b.status(), Lifecycle::Exited(_)))
    }

    fn arm_kill(&mut self) {
        // A coin carries its agent on either face, so kill works from chat or deps.
        // From the Spine/roster (no agent window focused) it targets the selected
        // issue, so you can stop an agent straight from the navbar.
        // Kill the agent the pane shows (a re-rooted deps coin's cursor root, a chat
        // coin's identity, else the selection) — not the coin's stale identity (H6).
        let Some(issue) = self.detail_key().map(str::to_string) else {
            self.status_msg = Some(format!(
                "no agent here to kill — {} closes a window",
                self.keymap.verb_label(Action::CloseWindow)
            ));
            return;
        };
        if !(self.fleet.get(&issue).is_some_and(AgentStatus::is_live)
            || self.has_live_backend(&issue))
        {
            // A wedged fresh launch has no fleet entry yet (AgentSpawned never came),
            // so the normal kill is refused — but the user must still be able to abort
            // it. Cancel the in-flight launch directly so a hung start is recoverable
            // without restarting the cockpit (M9).
            if self.pending_launch.contains_key(&issue) {
                if let Some(workspace) = self.workspace.clone() {
                    workspace.cancel(self.active_project.clone(), issue.clone());
                }
                self.pending_launch.remove(&issue);
                if self.pending_attach.as_deref() == Some(issue.as_str()) {
                    self.pending_attach = None;
                }
                // Tear down the "◌ starting agent…" card the button launch opened, so the
                // view matches the "cancelled" footer instead of still claiming the agent
                // is starting (the card paints purely on no-backend + no-fleet) (M9).
                self.undock_issue(&issue);
                self.set_footer(format!("cancelled the pending launch on {issue}"));
                return;
            }
            self.status_msg = Some(format!("agent on {issue} is not running"));
            return;
        }
        self.status_msg = Some(format!(
            "kill agent on {issue}? y to confirm, any key to cancel"
        ));
        self.kill_confirm = Some(issue);
    }

    /// Arm a confirmed discard of the selected issue's workspace (`Ctrl-a d`,
    /// ENG-541): push each repo's branch then remove its worktrees. Refused while the
    /// agent is still live — its checkout is in use, so stop it first.
    fn arm_discard(&mut self) {
        // Target the issue the pane is *showing* (CF-9 / H6): a re-rooted deps coin
        // describes its cursor root, not its fixed identity. `detail_key()` is the single
        // on-screen-target source the detail bar, dispatch and kill already resolve
        // through — discard (the one destructive verb CF-9 missed) must agree, or
        // confirming pushes + tears down the *wrong* issue's worktree (NEW-04).
        let Some(issue) = self.detail_key().map(str::to_string) else {
            self.status_msg = Some("no issue selected to discard".into());
            return;
        };
        if self.fleet.get(&issue).is_some_and(AgentStatus::is_live) || self.has_live_backend(&issue)
        {
            self.status_msg = Some(format!(
                "{issue} is still running — stop it ({}) before discarding",
                self.keymap.verb_label(Action::KillWindow)
            ));
            return;
        }
        let action = if crate::worktree::is_synthetic_ask_id(&issue) {
            "remove throwaway worktrees"
        } else {
            "push branches + remove worktrees"
        };
        self.status_msg = Some(format!(
            "discard {issue}'s workspace — {action}? y to confirm, any key to cancel"
        ));
        self.discard_confirm = Some(issue);
    }

    fn on_discard_confirm_key(&mut self, key: KeyEvent) {
        let Some(issue) = self.discard_confirm.take() else {
            return;
        };
        let confirm = matches!(
            key.code,
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter
        );
        if !confirm {
            self.status_msg = Some("discard cancelled".into());
            return;
        }
        match self.workspace.clone() {
            Some(workspace) => {
                if crate::worktree::is_synthetic_ask_id(&issue) {
                    workspace.teardown_ask(self.active_project.clone(), issue.clone());
                } else {
                    workspace.teardown(self.active_project.clone(), issue.clone());
                }
                // Don't tear down the UI yet — wait for teardown to report whether the
                // worktree was actually removed (`WorkspaceDiscarded`) or KEPT after a
                // push/remove/scratch failure (`DiscardKeptWorktree`). Dropping the UI
                // now would strand local work with no on-screen trace.
                self.set_footer(format!("discarding {issue}'s workspace…"));
            }
            None => self.status_msg = Some("agent control plane unavailable".into()),
        }
    }

    /// Resolve a pending lazy-pull confirmation (ENG-542): `y`/Enter materialises the
    /// requested repo (off the render thread, via the workspace); anything else denies
    /// it. The agent's `request-repo` already returned, so a deny just means the repo
    /// isn't added — the agent isn't blocked.
    fn on_repo_confirm_key(&mut self, key: KeyEvent) {
        let Some(req) = self.repo_confirm.take() else {
            return;
        };
        let confirm = matches!(
            key.code,
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter
        );
        if !confirm {
            self.set_footer(format!("denied repo pull: {}", req.handle));
            return;
        }
        match self.workspace.clone() {
            Some(workspace) => {
                workspace.materialize_repo(req.project_id, req.issue.clone(), req.handle.clone());
                self.set_footer(format!("pulling {} into {}…", req.handle, req.issue));
            }
            None => self.status_msg = Some("agent control plane unavailable".into()),
        }
    }

    fn request_quit(&mut self) {
        let (live, needs) = self.workspace_summary();
        if live == 0 {
            // Nothing is at stake — quit immediately, no confirmation friction
            // (the kill-confirm shape is only there to guard a live fleet teardown).
            self.should_quit = true;
            return;
        }
        self.quit_confirm = true;
        self.status_msg = Some(format!(
            "quit with {live} live agent{}{}? y to confirm, esc to cancel",
            if live == 1 { "" } else { "s" },
            if needs > 0 {
                format!(" ({needs} needs you)")
            } else {
                String::new()
            }
        ));
    }

    fn on_quit_confirm_key(&mut self, key: KeyEvent) {
        let confirm = matches!(
            key.code,
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter
        );
        self.quit_confirm = false;
        if confirm {
            self.should_quit = true;
        } else {
            self.status_msg = Some("quit cancelled".into());
        }
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
        match self.workspace.clone() {
            Some(workspace) => {
                workspace.cancel(self.active_project.clone(), issue.clone());
                // Eagerly tombstone the killed issue (CF-4): a NeedsYou / working
                // AgentAction / live AgentStatusChanged hook already in flight before the
                // cancel must be dropped by the resurrection guards — not re-promote the
                // dying agent to WORKING or re-raise "needs you" until the graded Stopped
                // lands. Reuses the shared `reaped` set discard already seeds; a real
                // relaunch (AgentSpawned) clears it.
                self.reaped.insert(issue.clone());
                // Kill also undocks the agent's window — pinned coins included.
                // Close is "undock, agent keeps running"; kill is "stop it and put
                // it away", so a killed agent never lingers as a dead EXITED card
                // you have to dismiss by hand.
                let closed = self.undock_issue(&issue);
                self.status_msg = Some(if closed {
                    format!("killing agent on {issue} · window closed")
                } else {
                    format!("killing agent on {issue}…")
                });
            }
            None => self.status_msg = Some("agent control plane unavailable".into()),
        }
    }

    /// Close every window for `issue` (pinned coins + the preview if aimed there)
    /// and tidy up after them — the kill verb's window teardown. Returns whether a
    /// *docked* (pinned) window was closed, so the caller knows the layout changed.
    /// A fresh preview is re-aimed at the current selection so the strip is never
    /// left previewless. The backend is reclaimed on the agent's reap (no window
    /// references it now); a still-live one is left to the supervisor's teardown.
    fn undock_issue(&mut self, issue: &str) -> bool {
        let removed = self.windows.close_issue(issue);
        if removed.is_empty() {
            return false;
        }
        let closed_pinned = removed.iter().any(|w| w.pinned);
        for w in &removed {
            self.preview_size.remove(&w.id);
        }
        if closed_pinned {
            self.cockpit_dirty = true; // a docked window left the set
        }
        self.reaim_preview();
        closed_pinned
    }

    fn toggle_layout(&mut self) {
        // While zoomed the renderer always draws the single big pane, so a layout flip
        // is invisible (and used to silently latch manual mode) — refuse with a hint
        // instead of changing state behind a curtain.
        if self.windows.zoomed {
            self.set_footer(format!(
                "un-zoom ({}) to change layout",
                self.keymap.verb_label(Action::ZoomToggle)
            ));
            return;
        }
        // Tri-state cycle (auto → rail → mosaic → auto) so a peek at the other layout
        // isn't a one-way door out of the adaptive layout (M2).
        self.windows.cycle_layout();
        self.cockpit_dirty = true;
        // Every window's Rect moves under the new layout; forget the cached sizes
        // so the next render reflows each live agent to where it now sits. This
        // (and zoom) are the *only* moments a live PTY reflows — browsing never does.
        self.preview_size.clear();
        self.set_footer(format!("layout: {}", self.windows.layout_label()));
    }

    /// Forward a key to a specific agent's PTY.
    fn forward_to_agent(&mut self, issue: &str, key: KeyEvent) {
        let bytes = backend::key_to_bytes(key);
        if bytes.is_empty() {
            return;
        }
        let Some(backend) = self.backends.get(issue) else {
            // The backend was already reclaimed (the agent exited and its card was
            // closed/pruned) — say so once instead of silently swallowing the keystroke
            // into the void, which read as a frozen prompt (D-MED).
            self.set_footer(if self.is_starting(issue) {
                format!("agent on {issue} is still starting — input will work when it is ready")
            } else {
                format!("agent on {issue} is no longer accepting input")
            });
            return;
        };
        if matches!(backend.status(), Lifecycle::Exited(_)) {
            self.set_footer(format!("agent on {issue} is no longer accepting input"));
            return;
        }
        if backend.send_input(&bytes).is_err() {
            // The PTY is gone — the agent exited out from under us. Surface it; the
            // window stays (as an EXITED card) until you close it.
            self.set_footer(format!("agent on {issue} is no longer accepting input"));
        }
    }

    /// Forward a bracketed paste to a focused chat agent verbatim, re-wrapped in the
    /// terminal's paste markers (`ESC[200~`…`ESC[201~`) so the child `claude`
    /// reassembles it as one block rather than submitting at every newline (the
    /// line-by-line paste bug). Returns true when a chat coin consumed it (so the
    /// caller repaints); a paste with no chat focused is dropped, like a keystroke.
    pub fn forward_paste(&mut self, text: &str) -> bool {
        let WindowKind::Coin {
            issue,
            mode: CoinMode::Chat,
        } = self.windows.focused_kind().clone()
        else {
            return false;
        };
        let clean = text.replace("\x1b[200~", "").replace("\x1b[201~", "");
        let mut bytes = Vec::with_capacity(text.len() + 12);
        bytes.extend_from_slice(b"\x1b[200~");
        bytes.extend_from_slice(clean.as_bytes());
        bytes.extend_from_slice(b"\x1b[201~");
        match self.backends.get(&issue) {
            Some(backend) if matches!(backend.status(), Lifecycle::Exited(_)) => {
                self.set_footer(format!("agent on {issue} is no longer accepting input"));
            }
            Some(backend) if backend.send_input(&bytes).is_ok() => {}
            _ => self.set_footer(if self.is_starting(&issue) {
                format!("agent on {issue} is still starting — paste when it is ready")
            } else {
                format!("agent on {issue} is no longer accepting input")
            }),
        }
        true
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
        // Any path that lands focus on the Spine must reveal it. A zoomed coin hides the
        // Spine entirely, so FocusLeft/Right stepping onto index 0 (like the dedicated
        // FocusNav) has to clear zoom — otherwise focused_kind()==Spine routes j/k/⏎ to
        // an off-screen Spine and Enter could spawn an agent the user can't see (M1).
        if self.windows.focus == 0 {
            self.windows.zoomed = false;
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

    /// The inner PTY size window `idx` occupies in the current strip layout, so a
    /// (re)spawned agent's PTY can be created at its real tile size and claude's
    /// first paint already fits — instead of painting at the full-terminal width
    /// and only reflowing once it processes the SIGWINCH (the stale, wrong-sized
    /// frame seen when a chat is opened beside a pinned coin). `None` when the
    /// window isn't placed (off-screen, or no viewport yet). Mirrors the layout
    /// branches in [`Self::is_index_visible`].
    fn window_pane_size(&self, idx: usize) -> Option<(u16, u16)> {
        let area = self.viewport;
        if area.area() == 0 || idx >= self.windows.windows.len() {
            return None;
        }
        let n = self.windows.windows.len();
        let focus = self.windows.focus;
        let active = self.active_index();
        let rect = if self.windows.zoomed {
            // Zoom fills the viewport with the single big pane; only it is placed.
            (layout::rail_big_index(n, focus, active) == Some(idx)).then_some(area)
        } else {
            match self.windows.layout {
                LayoutMode::Mosaic => layout::mosaic(area, n)
                    .into_iter()
                    .find(|p| p.index == idx)
                    .map(|p| p.rect),
                LayoutMode::Rail => {
                    let (full, _, _) =
                        layout::rail(area, n, focus, active, self.windows.preview_index());
                    full.into_iter().find(|p| p.index == idx).map(|p| p.rect)
                }
            }
        }?;
        // Subtract the window block's 1-cell border to get the PTY's inner size.
        Some((
            rect.height.saturating_sub(2).max(1),
            rect.width.saturating_sub(2).max(1),
        ))
    }

    /// The size to spawn a freshly-opened agent's PTY at: the pane its host window
    /// (the selection's pinned coin, else the preview) currently occupies, or —
    /// before any such window exists — the lone tile right of the Spine. See
    /// [`Self::window_pane_size`] for why this matters.
    fn agent_spawn_size(&self) -> Option<(u16, u16)> {
        if let Some(size) = self.active_index().and_then(|i| self.window_pane_size(i)) {
            return Some(size);
        }
        let area = self.viewport;
        if area.area() == 0 {
            return None;
        }
        let cols = area.width.saturating_sub(layout::SPINE_WIDTH);
        Some((
            area.height.saturating_sub(2).max(1),
            cols.saturating_sub(2).max(1),
        ))
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

    /// Persist ledger history to the right per-project files. The in-memory ledger
    /// sees events from every project before the active-project scoping guard; saving
    /// the whole thing to `ledger_path` would leak background project history into
    /// whichever project happened to be active when the app booted.
    pub fn save_ledgers(&self) {
        if let Some(layout) = &self.layout {
            for project_id in self.ledger.project_ids() {
                if let Some(handle) = self.project_handles.get(&project_id) {
                    let path = layout.ledger_path(handle);
                    let _ = self.ledger.save_project(&path, &project_id);
                }
            }
        } else if let Some(path) = self.ledger_path.as_deref() {
            let _ = self.ledger.save(path);
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
        // Record the policy so a later project switch restores resume to ON rather
        // than fabricating it; under `--no-resume` begin_resume is never called, so
        // this stays false and a switch leaves resume OFF.
        self.auto_resume_enabled = true;
        self.resume_cap = cap;
        if self.workspace.is_none() {
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
        if self.backends.contains_key(&issue) || self.pending_launch.contains_key(&issue) {
            return; // already up, or already resuming
        }
        // "Was-live" survives rehydration as a live fleet status (Idle/NeedsYou).
        if self.fleet.get(&issue).is_some_and(AgentStatus::is_live) {
            self.resume_one(&issue);
        }
    }

    /// Fire a single resume launch (no focus-steal — the window already exists).
    fn resume_one(&mut self, issue: &str) {
        let Some(workspace) = self.workspace.clone() else {
            return;
        };
        if self.pending_launch.contains_key(issue) {
            return;
        }
        // Don't fire a resume the supervisor can only reject as "at capacity": it
        // would emit a bare Notification (never an AgentSpawned), so the spinner
        // would burn its whole grace window for an agent that never comes up.
        // Leave it a docked card; the lazy path retries on the next focus once a
        // live agent frees a slot. (live agents + in-flight resumes ≈ load — a
        // terminal EXITED card holds a backend but no live process, so it must not
        // count, or a project full of finished cards would never resume.)
        if self.resume_cap > 0 && self.live_agent_count() + self.resuming.len() >= self.resume_cap {
            return;
        }
        let title = self
            .graph
            .get(issue)
            .map(|i| i.title.clone())
            .unwrap_or_else(|| issue.to_string());
        // Resume into the docked window's current tile, so the resumed screen
        // fits on its first paint rather than reflowing from the full width.
        let size = self
            .windows
            .pinned_coin_index(issue)
            .and_then(|i| self.window_pane_size(i));
        workspace.launch(self.active_project.clone(), issue.to_string(), title, size);
        self.pending_launch
            .insert(issue.to_string(), self.frame + RESUME_GRACE_FRAMES);
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
        // Record the durable per-issue ledger for EVERY project — before the
        // scoping guard below drops a backgrounded project's events from the
        // on-screen fleet — so an agent's run history is captured no matter which
        // project you're inside when it runs.
        self.record_ledger(&ev);
        // Fold the event into the cross-project `world` map (ENG-406) for EVERY
        // project — before the scoping guard below drops a backgrounded project's
        // events — so the workspace roll-up, the global screen and the cross-project
        // jump see agents in projects you aren't currently inside.
        self.update_world(&ev);
        // FEAT-B step 3: stamp the quiet-running overlay for a LIVE issue's PTY burst.
        // Gating on `is_live()` keeps a terminal/absent issue's late output out of the
        // map (the read-side `recently_active` already ignores it, but this keeps the map
        // bounded and honest). A *fresh* activation — an Idle agent that wasn't
        // recently-active until this burst — forces one repaint even off-screen, so its
        // band/marker flips to WORKING at once instead of on the next animation frame.
        let mut newly_active = false;
        if let AppEvent::AgentOutput { project_id, issue } = &ev
            && (self.active_project.is_empty()
                || project_id.as_ref() == self.active_project.as_str())
        {
            let key: &str = issue;
            if self.fleet.get(key).is_some_and(|s| s.is_live()) {
                newly_active =
                    self.fleet.get(key) == Some(&AgentStatus::Idle) && !self.recently_active(key);
                self.last_output.insert(key.to_string(), self.frame);
            }
        }
        // While the cockpit is inside one project, an agent event from another
        // (backgrounded) project isn't for the fleet on screen — drop it. ENG-401
        // shards the fleet by project and files these instead of dropping. An
        // empty `active_project` (demo / snapshots / unit tests, which never arm a
        // workspace) disables the guard so every event still applies.
        if !self.active_project.is_empty()
            && ev
                .project_id()
                .is_some_and(|pid| pid != self.active_project)
        {
            // A backgrounded project's event isn't for the on-screen fleet — but a
            // backend that *spawns* while backgrounded must not be lost (its
            // AgentSpawned would otherwise be dropped and the agent orphaned on a
            // switch back, since the supervisor won't re-hand the backend). File it
            // into that project's stash instead, and drop it again on reap. And a
            // "needs you" over there must still surface, so we tally it (cleared
            // when that agent resumes/exits) and toast it once.
            let mut repaint = false;
            match ev {
                AppEvent::AgentSpawned {
                    project_id,
                    issue,
                    backend,
                    ..
                } => {
                    self.stashed_backends
                        .entry(project_id)
                        .or_default()
                        .insert(issue, backend);
                }
                AppEvent::AgentReaped { project_id, issue } => {
                    if let Some(m) = self.stashed_backends.get_mut(&project_id) {
                        m.remove(&issue);
                    }
                    repaint = self.forget_elsewhere_needs_you(&project_id, &issue);
                }
                AppEvent::AgentNeedsYou {
                    project_id, issue, ..
                } => {
                    let newly = self
                        .other_needs_you
                        .entry(project_id.clone())
                        .or_default()
                        .insert(issue.clone());
                    if newly {
                        // Toast once, but never bury a *local* standing alert or an armed
                        // confirmation's prompt (H4).
                        if !self.needs_you_alert && !self.confirm_pending() {
                            let name = self.project_name(&project_id);
                            let switch = self.keymap.verb_label(Action::SwitchProject);
                            self.status_msg = Some(format!(
                                "⚑ {issue} in {name} needs you — {switch} to switch"
                            ));
                        }
                        repaint = true; // refresh the header "elsewhere" badge
                    }
                }
                // Any sign the backgrounded agent is no longer blocked clears it.
                AppEvent::AgentStatusChanged {
                    project_id,
                    issue,
                    status,
                } if !status.needs_you() => {
                    repaint = self.forget_elsewhere_needs_you(&project_id, &issue);
                }
                AppEvent::AgentAction {
                    project_id,
                    issue,
                    working: true,
                    ..
                } => {
                    repaint = self.forget_elsewhere_needs_you(&project_id, &issue);
                }
                AppEvent::AgentExited {
                    project_id, issue, ..
                } => {
                    repaint = self.forget_elsewhere_needs_you(&project_id, &issue);
                }
                // A repo-pull request from a backgrounded project can't raise the modal
                // (you're not looking at it), but it must not vanish silently — surface
                // a footer so you know to switch in and confirm. The agent isn't blocked
                // (request-repo returns immediately), so it can re-request on its own.
                AppEvent::RepoRequested {
                    project_id,
                    issue,
                    repo_handle,
                } => {
                    if !self.needs_you_alert && !self.confirm_pending() {
                        let name = self.project_name(&project_id);
                        self.status_msg = Some(format!(
                            "agent on {issue} in {name} requests repo `{repo_handle}` — switch in to confirm"
                        ));
                    }
                    repaint = true;
                }
                // A commit in a backgrounded project: maintain the CROSS-PROJECT
                // `unpushed` chip here (its active footer never runs), so a rejected
                // push there is never silently dropped — the data-integrity contract
                // can't be evaded by the scope guard. A newly stranded one toasts once.
                AppEvent::AgentCommitted {
                    project_id,
                    issue,
                    repo_handle,
                    outcome,
                    ..
                } => {
                    let newly = self.note_push_outcome(&project_id, &issue, &repo_handle, &outcome);
                    if newly && !self.needs_you_alert && !self.confirm_pending() {
                        let name = self.project_name(&project_id);
                        self.status_msg = Some(format!(
                            "⚠ {issue} in {name}: auto-push failed — commits stranded"
                        ));
                    }
                    repaint = true;
                }
                // A discard finishing in a backgrounded project clears its strand chip
                // too, so a removed issue can't keep a cross-project chip alive.
                AppEvent::WorkspaceDiscarded { project_id, issue } => {
                    self.forget_unpushed(&project_id, &issue);
                    repaint = true;
                }
                _ => {}
            }
            return repaint;
        }
        match ev {
            AppEvent::Notification(text) => {
                self.set_footer(text);
                true
            }
            // A launch the supervisor refused (capacity / already-running / still-
            // stopping): drop ONLY the rejected issue's double-press guard — a bare
            // Notification used to clear every issue's `pending_launch`, so an
            // unrelated toast removed a different issue's in-flight guard (M10). A
            // wedged guard with no rejection still self-heals on its deadline (M9).
            AppEvent::LaunchRejected { issue, reason } => {
                self.pending_launch.remove(&issue);
                self.set_footer(reason);
                true
            }
            // First-materialisation clone progress for the project being entered (the
            // scope guard above already dropped a backgrounded clone's ticks). Set the
            // footer directly — unlike a `Notification` this is a high-frequency tick,
            // so it must NOT clear `pending_launch` or disturb a needs-you alert.
            AppEvent::MaterializeProgress {
                project_id,
                phase,
                percent,
            } => {
                if !self.confirm_pending() {
                    let name = self.project_name(&project_id);
                    self.status_msg = Some(format!("materialising {name} · {phase} {percent}%"));
                }
                true
            }
            // The first clone finished — replace the terminal "… 100%" tick with a
            // settled line. Like the progress arm, this must NOT clear `pending_launch`
            // (a launch queued during the clone stays pending) or touch a needs-you
            // alert, so it sets the footer directly rather than via `set_footer`.
            AppEvent::MaterializeDone { project_id } => {
                if !self.confirm_pending() {
                    let name = self.project_name(&project_id);
                    self.status_msg = Some(format!("materialised {name}"));
                }
                true
            }
            // A disk-reclaim scan/delete finished on the blocking pool: open,
            // refresh, or close the prompt off the result (see `apply_reclaim_scan`).
            AppEvent::ReclaimScanned {
                mirrors,
                opening,
                note,
            } => {
                self.apply_reclaim_scan(mirrors, opening, note);
                true
            }
            // A switch's graph finished loading off-thread: take it from the inbox
            // and swap the cockpit over to it — unless a newer switch (or a cancel)
            // has since bumped the generation, in which case this result is stale.
            AppEvent::ProjectActivated => {
                let taken = self
                    .switch_inbox
                    .lock()
                    .ok()
                    .and_then(|mut slot| slot.take());
                if let Some((generation, project, graph)) = taken
                    && generation == self.switch_seq
                {
                    self.activate_project(project, graph);
                }
                true
            }
            AppEvent::WorkspaceDiscarded { project_id, issue } => {
                // Teardown confirmed the worktrees are gone — now it's safe to drop the
                // fleet entry + window and tombstone late hooks (the cleanup the confirm
                // used to do synchronously, before knowing the discard actually finished).
                self.kept_worktrees.remove(&issue);
                self.forget_unpushed(&project_id, &issue);
                self.ask_agents.remove(&issue);
                self.fleet.remove(&issue);
                self.reaped.insert(issue.clone());
                self.undock_issue(&issue);
                self.set_footer(format!("discarded {issue}'s workspace"));
                true
            }
            AppEvent::DiscardKeptWorktree { issue, reason, .. } => {
                // The worktree is still on disk. Keep a standing chip + leave the issue
                // in place so a re-discard after fixing cleanup finishes the job.
                if crate::worktree::is_synthetic_ask_id(&issue) {
                    self.ask_agents.insert(issue.clone());
                }
                self.kept_worktrees.insert(issue.clone());
                self.set_footer(format!("{issue}: {reason}"));
                true
            }
            // `project_id` is ignored while the cockpit is single-project; ENG-401
            // shards the fleet by project and binds it here.
            AppEvent::AgentSpawned {
                issue,
                backend,
                repos,
                ..
            } => {
                self.restore_ask_issue_if_needed(&issue);
                // Clear the double-launch guard (set by the button AND by a resume).
                self.pending_launch.remove(&issue);
                // A real relaunch revives the issue — clear any reaped tombstone.
                self.reaped.remove(&issue);
                self.fleet.insert(issue.clone(), AgentStatus::Spawning);
                // Record the repo set the supervisor checked out so the agent's
                // header can name its repos/worktrees. A resume reports its full
                // rehydrated set here too, so this stays accurate across restarts.
                if !repos.is_empty() {
                    self.agent_repos.insert(issue.clone(), repos);
                }
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
            AppEvent::AgentOutput { issue, .. } => self.is_agent_visible(&issue) || newly_active,
            AppEvent::AgentExited { issue, code, .. } => {
                let note = match code {
                    Some(0) | None => format!("agent on {issue} finished"),
                    Some(c) => format!("agent on {issue} exited ({c})"),
                };
                // An unrelated agent's autonomous exit must drop the sticky needs-you
                // alert only when *no* agent still needs you — mirror AgentReaped, not
                // set_footer's blanket clear, so a sibling's exit can't strip another
                // agent's standing prompt guard (M11). Then show the exit note only if
                // it won't bury a still-standing alert (or an armed confirm, H4).
                self.clear_needs_you_alert_if_resolved();
                if !self.needs_you_alert && !self.confirm_pending() {
                    self.status_msg = Some(note);
                }
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
            AppEvent::AgentNeedsYou { issue, reason, .. } => {
                self.restore_ask_issue_if_needed(&issue);
                if self.is_terminal(&issue) || self.reaped.contains(&issue) {
                    return false;
                }
                self.fleet.insert(issue.clone(), AgentStatus::NeedsYou);
                self.needs_you_alert = true; // sticky until acknowledged
                // Never overwrite an armed kill/discard/repo prompt: a "⚑ needs you" line
                // under the red "confirm kill" hint would make a reflexive `y` destroy the
                // agent under a misleading message (H4). The alert stays sticky (the header
                // badge + flag still show it), so it isn't lost — it just doesn't seize the
                // prompt slot while a confirmation owns `status_msg` and its `y`.
                if !self.confirm_pending() {
                    self.status_msg = Some(format!("⚑ {issue} needs you — {reason}"));
                }
                true
            }
            AppEvent::AgentStatusChanged { issue, status, .. } => {
                self.restore_ask_issue_if_needed(&issue);
                // A reaped (killed/discarded) agent drops a *live* re-emit — a NeedsYou /
                // working hook in flight before the cancel must not re-promote a dying
                // agent (CF-4). But a *terminal* status (the supervisor's graded `Stopped`
                // for a kill) must still land, so the deliberate kill gets its
                // `Flash::Stopped` pulse (CF-14) and the fleet bands the row terminal
                // instead of leaving a stale live status until `AgentReaped`. Resurrection
                // stays guarded by the `is_live()` check below and the durable
                // `set_status` terminal guard.
                if self.reaped.contains(&issue) && status.is_live() {
                    return false;
                }
                if status.is_live() && self.is_terminal(&issue) {
                    return false;
                }
                // A clean finish flashes green (Finished); a crash flashes RED
                // (Failed) so a failed run never reads as a success (H5).
                let outcome_flash = match status {
                    AgentStatus::Done => Some(Flash::Finished),
                    AgentStatus::Failed => Some(Flash::Failed),
                    AgentStatus::Stopped => Some(Flash::Stopped),
                    _ => None,
                };
                if let Some(kind) = outcome_flash {
                    self.flash
                        .insert(issue.clone(), (kind, self.frame + FLASH_FRAMES));
                }
                // CF-14: tally a genuine clean finish (Done over a non-terminal status,
                // not a re-emit of an already-terminal one) for the "shipped" chip.
                if status == AgentStatus::Done && !self.is_terminal(&issue) {
                    self.shipped_today += 1;
                }
                let teardown_ask = status.is_terminal() && self.ask_agents.remove(&issue);
                self.fleet.insert(issue.clone(), status);
                if teardown_ask && let Some(workspace) = self.workspace.clone() {
                    workspace.teardown_ask(self.active_project.clone(), issue.clone());
                }
                // Clear the sticky alert only when *no* agent needs you anymore —
                // resolving one of several needy agents must not silence the rest
                // (the old per-event clear dropped the global flag on the first to
                // resolve).
                self.clear_needs_you_alert_if_resolved();
                true
            }
            AppEvent::AgentAction {
                issue,
                action,
                working,
                ..
            } => {
                self.restore_ask_issue_if_needed(&issue);
                if self.is_terminal(&issue) || self.reaped.contains(&issue) {
                    return false;
                }
                let prev = self.fleet.get(&issue).copied();
                let was_needs_you = prev == Some(AgentStatus::NeedsYou);
                // A *working* action promotes a live, non-idle agent to Running (a
                // tool ran, or a prompt was answered → it's churning), including a
                // NeedsYou agent — that's what resolves a prompt once you answer and
                // it resumes. It must NOT revive an *Idle* agent: only a genuine new
                // turn does (UserPromptSubmit, routed as a status change), so a late
                // or out-of-order mid-turn PostToolUse can't flip a settled agent back
                // to WORKING (A4). A non-working action (the ~60 s idle nudge) never
                // promotes, so it can neither silence a NeedsYou prompt nor un-idle a
                // resting agent.
                let promotable = prev.is_some_and(|s| s.is_live() && !s.is_idle());
                if working && promotable {
                    self.fleet.insert(issue.clone(), AgentStatus::Running);
                }
                // The needy agent itself just resumed: drop the sticky footer alert
                // unless *another* agent still needs you.
                let resumed = working && was_needs_you;
                if resumed {
                    self.clear_needs_you_alert_if_resolved();
                }
                // Don't let routine chatter bury a standing alert — but the agent
                // that just resumed may speak (its alert is the one we cleared). And
                // never overwrite an armed confirmation's prompt (H4).
                if (resumed || !self.needs_you_alert) && !self.confirm_pending() {
                    self.status_msg = Some(format!("{issue}: {action}"));
                }
                true
            }
            AppEvent::AgentReaped { issue, .. } => {
                let teardown_ask = self.ask_agents.remove(&issue);
                self.reaped.insert(issue.clone());
                self.fleet.remove(&issue);
                // The agent's worktrees are torn down with it — drop its repo set so a
                // stale badge can't linger past the agent that owned it.
                self.agent_repos.remove(&issue);
                // FEAT-B step 9: drop the quiet-running overlay stamp with the fleet
                // entry so a reaped issue can never resurface as "recently active"
                // (tick_frame also ages it out, but the reap clears it immediately).
                self.last_output.remove(&issue);
                self.drop_preview_sizes_for(&issue);
                // Keep the backend only while a window still shows it.
                if !self.windows.references_agent(&issue) {
                    self.backends.remove(&issue);
                }
                if teardown_ask && let Some(workspace) = self.workspace.clone() {
                    workspace.teardown_ask(self.active_project.clone(), issue.clone());
                }
                // A needy agent that's now gone leaves nothing to act on — drop the
                // sticky alert (unless another agent still needs you). Without this
                // a kill/exit of the flagged agent left the footer yelling forever.
                self.clear_needs_you_alert_if_resolved();
                true
            }
            // v1.6 auto-push: an agent committed and its branch was pushed — or the
            // push was REJECTED. This is the ACTIVE project's path (a backgrounded one
            // is handled by the scope-guard arm above); `note_push_outcome` maintains
            // the standing cross-project `unpushed` chip, and here we additionally
            // paint THIS project's transient footer from the `outcome`,
            // so a rejected push reads as a failure, never a blanket "pushed". A commit
            // is never "needs you" and must never bury a standing alert or a pending
            // confirmation's prompt, so the footer defers to them (the chip, set above,
            // is the durable surface). The repo handle scopes the line for a multi-repo
            // issue (where several repos can push independently).
            AppEvent::AgentCommitted {
                project_id,
                issue,
                repo_handle,
                branch,
                outcome,
            } => {
                self.note_push_outcome(&project_id, &issue, &repo_handle, &outcome);
                if !self.needs_you_alert && !self.confirm_pending() {
                    let where_ = if repo_handle.is_empty() {
                        issue
                    } else {
                        format!("{issue}/{repo_handle}")
                    };
                    self.status_msg = Some(match &outcome {
                        PushOutcome::Pushed if branch.is_empty() => format!("{where_}: pushed"),
                        PushOutcome::Pushed => format!("{where_}: pushed {branch}"),
                        PushOutcome::LocalOnly if branch.is_empty() => {
                            format!("{where_}: committed (local-only)")
                        }
                        PushOutcome::LocalOnly => {
                            format!("{where_}: committed {branch} (local-only)")
                        }
                        PushOutcome::Rejected(reason) => {
                            format!("{where_}: auto-push failed — {reason}")
                        }
                    });
                }
                true
            }
            // A running agent (in the active project — the scoping guard already
            // filtered a backgrounded one) asked for an in-candidate repo. Raise the
            // confirmation modal; the human's `y` materialises it (ENG-542).
            AppEvent::RepoRequested {
                project_id,
                issue,
                repo_handle,
            } => {
                // Never raise the repo-pull prompt over a *destructive* confirmation
                // whose `y` it doesn't own: the on_key band resolves kill/discard
                // before repo_confirm, so a "y to pull" footer shadowing a pending
                // kill would make `y` destroy the agent under a misleading message.
                // Likewise don't clobber an already-pending repo_confirm. The agent's
                // `request-repo` returns immediately, so a dropped request can be
                // re-issued — surface a passive footer and bail.
                if self.confirm_pending() || self.full_modal_open() {
                    // A confirmation is armed (owns `status_msg` + its `y`), or a full
                    // modal owns the keyboard (so a "y to pull" would never reach this
                    // confirm and would leak to a later key). The agent's `request-repo`
                    // returns immediately and is re-issued next turn, so drop it rather
                    // than raise a prompt no one can answer here (H4 / M7).
                    return true;
                }
                self.set_footer(format!(
                    "agent on {issue} requests repo `{repo_handle}` — y to pull, any key to deny"
                ));
                self.repo_confirm = Some(RepoConfirm {
                    project_id,
                    issue,
                    handle: repo_handle,
                });
                true
            }
        }
    }

    /// Whether a keyboard-capturing confirmation is armed (kill / discard / repo
    /// pull). While true its prompt owns `status_msg` and its `y`, so a background
    /// footer writer must not overwrite the visible text — that would make a
    /// reflexive `y` act under a misleading message (H4).
    fn confirm_pending(&self) -> bool {
        self.kill_confirm.is_some()
            || self.discard_confirm.is_some()
            || self.repo_confirm.is_some()
            || self.quit_confirm
    }

    /// Whether a full-screen modal currently owns the keyboard (project switcher,
    /// global all-agents, repo multi-select, or disk-reclaim). These resolve in
    /// `on_key` ahead of the prefix and `repo_confirm`, so a confirmation raised while
    /// one is open would never see its `y` — it would leak to a later keystroke (M7).
    fn full_modal_open(&self) -> bool {
        self.project_switcher.is_some()
            || self.global_view.is_some()
            || self.repo_select.is_some()
            || self.reclaim.is_some()
    }

    /// Set the transient footer line, superseding (and acknowledging) any
    /// standing needs-you alert — used by deliberate, low-frequency events. A no-op
    /// while a confirmation is armed, so routine chatter can't shadow its prompt (H4).
    /// Fold one auto-push [`PushOutcome`] into the cross-project `unpushed` chip,
    /// keyed by `project_id` then `issue` (or `issue/<repo>` for a multi-repo issue).
    /// A reject raises the key (commits stranded); a clean push or a local-only commit
    /// clears it. Returns `true` only when a NEW strand was recorded — the caller
    /// toasts a backgrounded one, since its footer never runs. A late reject for an
    /// already-reaped issue in the *active* project is ignored, so a torn-down issue's
    /// in-flight push can't resurrect a chip (the tombstone discipline every sibling
    /// re-emit handler follows).
    fn note_push_outcome(
        &mut self,
        project_id: &str,
        issue: &str,
        repo_handle: &str,
        outcome: &PushOutcome,
    ) -> bool {
        let key = if repo_handle.is_empty() {
            issue.to_string()
        } else {
            format!("{issue}/{repo_handle}")
        };
        match outcome {
            PushOutcome::Rejected(_) => {
                if project_id == self.active_project.as_str() && self.reaped.contains(issue) {
                    return false;
                }
                self.unpushed
                    .entry(project_id.to_string())
                    .or_default()
                    .insert(key)
            }
            PushOutcome::Pushed | PushOutcome::LocalOnly => {
                if let Some(set) = self.unpushed.get_mut(project_id) {
                    set.remove(&key);
                    if set.is_empty() {
                        self.unpushed.remove(project_id);
                    }
                }
                false
            }
        }
    }

    /// Drop any standing "unpushed" chip entries for `(project_id, issue)` — both the
    /// bare `issue` key (single-repo) and every `issue/<repo>` key (multi-repo) — when
    /// its workspace is discarded, so a removed issue can't keep a stranded-commit chip
    /// alive. The trailing-`/` guard keeps `ZAP-9` from also clearing `ZAP-90`.
    fn forget_unpushed(&mut self, project_id: &str, issue: &str) {
        if let Some(set) = self.unpushed.get_mut(project_id) {
            set.retain(|k| {
                k != issue && !k.strip_prefix(issue).is_some_and(|rest| rest.starts_with('/'))
            });
            if set.is_empty() {
                self.unpushed.remove(project_id);
            }
        }
    }

    /// Total stranded commit-sets across all projects — behind the header's
    /// "⇡N unpushed" chip. Mirrors [`Self::elsewhere_needs_you`].
    pub fn unpushed_count(&self) -> usize {
        self.unpushed.values().map(HashSet::len).sum()
    }

    fn set_footer(&mut self, text: String) {
        if self.confirm_pending() {
            return;
        }
        self.status_msg = Some(text);
        // A deliberate footer no longer blanket-drops the sticky needs-you guard — it
        // re-derives it (CF-11), so a nav action's trace (e.g. a filter cycle) can't
        // bury an un-answered "needs you" alert while an agent still needs you.
        self.clear_needs_you_alert_if_resolved();
    }

    /// Drop the sticky needs-you footer alert iff no agent in the on-screen fleet
    /// still needs you. The alert summarises the per-issue [`AgentStatus::NeedsYou`]
    /// set, so it must outlive resolving *one* of several needy agents and must
    /// not survive the last one's resolution/exit. Only ever clears (never
    /// re-arms), so an explicit acknowledge stays acknowledged.
    fn clear_needs_you_alert_if_resolved(&mut self) {
        if self.needs_you_alert && !self.fleet.values().any(AgentStatus::needs_you) {
            self.needs_you_alert = false;
        }
    }

    /// Drop `issue` from a backgrounded `project_id`'s needs-you tally (it resumed,
    /// exited, or was reaped), pruning the project's entry once empty. Returns
    /// whether anything was removed, so the caller can repaint the header badge.
    fn forget_elsewhere_needs_you(&mut self, project_id: &str, issue: &str) -> bool {
        let Some(set) = self.other_needs_you.get_mut(project_id) else {
            return false;
        };
        let removed = set.remove(issue);
        if set.is_empty() {
            self.other_needs_you.remove(project_id);
        }
        removed
    }

    /// How many agents in *other* (backgrounded) projects need you — the count
    /// behind the header's "⚑N elsewhere" badge.
    pub fn elsewhere_needs_you(&self) -> usize {
        self.other_needs_you.values().map(HashSet::len).sum()
    }

    /// The set of backgrounded `project_id`s with at least one agent needing you —
    /// the switcher flags these.
    pub fn projects_needing_you(&self) -> HashSet<String> {
        self.other_needs_you.keys().cloned().collect()
    }

    /// Fold one lifecycle event into the durable per-issue ledger: a launch opens a
    /// run, a `NeedsYou` bumps its prompt count, and a terminal status / reap closes
    /// it. Called for every project (before the scoping guard), so backgrounded runs
    /// are logged too; flips [`ledger_dirty`](Self::ledger_dirty) so the render
    /// thread persists the change. Surface-only chatter (`AgentAction`, `Output`)
    /// never touches the ledger.
    fn record_ledger(&mut self, ev: &AppEvent) {
        let now = crate::ledger::now_unix();
        match ev {
            AppEvent::AgentSpawned {
                project_id, issue, ..
            } => {
                let sid = crate::session::SessionStore::session_id_for(project_id, issue);
                self.ledger.begin(project_id, issue, sid, now);
                self.ledger_dirty = true;
            }
            AppEvent::AgentNeedsYou {
                project_id, issue, ..
            } => {
                self.ledger.note_needs_you(project_id, issue);
                self.ledger_dirty = true;
            }
            AppEvent::AgentStatusChanged {
                project_id,
                issue,
                status,
            } if status.is_terminal() => {
                self.ledger.note_terminal(project_id, issue, *status, now);
                self.ledger_dirty = true;
            }
            AppEvent::AgentReaped { project_id, issue } => {
                // A fallback close: the terminal AgentStatusChanged the supervisor
                // emits first usually closes the run; this catches a reap with no
                // verdict (e.g. a setup-failure teardown) without clobbering one.
                self.ledger.note_closed(project_id, issue, now);
                self.ledger_dirty = true;
            }
            _ => {}
        }
    }

    /// A project's display name from the switcher list, falling back to its id when
    /// the list is unavailable (demo / control plane off).
    fn project_name(&self, project_id: &str) -> String {
        self.project_list
            .iter()
            .find(|p| p.id == project_id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| project_id.to_string())
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
    /// Fold an agent event into the cross-project [`world`](Self::world) map (ENG-406),
    /// mirroring the same status transitions [`apply_event`](Self::apply_event) applies
    /// to the active [`fleet`](Self::fleet) — but for ALL projects, so a backgrounded
    /// project's agents stay visible to the workspace roll-up / global screen / jump.
    fn update_world(&mut self, ev: &AppEvent) {
        // Mirror apply_event's resurrection guards for the active project: a late hook
        // (a `NeedsYou`/`Running` POST that lands after the agent was reaped, or for an
        // issue already terminal in the fleet) must NOT re-insert a live status, or it
        // leaks a phantom live/needs-you agent into world[active] that the roll-up only
        // surfaces — inflating the count — once you switch away. A genuine relaunch
        // arrives as AgentSpawned (which clears the reaped tombstone in the main match),
        // so that variant is never blocked.
        let blocked = |this: &Self, pid: &str, issue: &str| -> bool {
            pid == this.active_project && (this.reaped.contains(issue) || this.is_terminal(issue))
        };
        match ev {
            AppEvent::AgentSpawned {
                project_id, issue, ..
            } => {
                self.world
                    .entry(project_id.clone())
                    .or_default()
                    .insert(issue.clone(), AgentStatus::Spawning);
            }
            AppEvent::AgentNeedsYou {
                project_id, issue, ..
            } => {
                if blocked(self, project_id, issue) {
                    return;
                }
                self.world
                    .entry(project_id.clone())
                    .or_default()
                    .insert(issue.clone(), AgentStatus::NeedsYou);
            }
            AppEvent::AgentStatusChanged {
                project_id,
                issue,
                status,
            } => {
                if status.is_live() && blocked(self, project_id, issue) {
                    return;
                }
                self.world
                    .entry(project_id.clone())
                    .or_default()
                    .insert(issue.clone(), *status);
            }
            AppEvent::AgentAction {
                project_id,
                issue,
                working: true,
                ..
            } => {
                if blocked(self, project_id, issue) {
                    return;
                }
                // Mirror apply_event's A4 promotion rule: a working action promotes only
                // a pre-existing live, non-idle entry — it must NOT create one (an action
                // before AgentSpawned) nor un-idle a settled agent. Only a genuine new
                // turn (UserPromptSubmit, routed as AgentStatusChanged{Running}) revives
                // Idle. Without this, `world` would diverge from `fleet` and inflate the
                // cross-project roll-up with a phantom live agent once you switch away.
                if let Some(s) = self
                    .world
                    .get_mut(project_id)
                    .and_then(|m| m.get_mut(issue))
                    && s.is_live()
                    && !s.is_idle()
                {
                    *s = AgentStatus::Running;
                }
            }
            AppEvent::AgentReaped { project_id, issue }
            | AppEvent::AgentExited {
                project_id, issue, ..
            } => {
                if let Some(m) = self.world.get_mut(project_id) {
                    m.remove(issue);
                    if m.is_empty() {
                        self.world.remove(project_id);
                    }
                }
            }
            _ => {}
        }
    }

    /// Live-agent + needs-you counts across the WHOLE workspace (ENG-406): the active
    /// project from the authoritative [`fleet`](Self::fleet), every other project from
    /// the event-fed [`world`](Self::world). The header roll-up renders this on every
    /// screen — the locked workspace-level needs-you indicator.
    pub fn workspace_summary(&self) -> (usize, usize) {
        let (mut live, mut needs) = (0, 0);
        let mut tally = |s: &AgentStatus| {
            if s.is_live() {
                live += 1;
            }
            if s.needs_you() {
                needs += 1;
            }
        };
        for s in self.fleet.values() {
            tally(s);
        }
        for (pid, issues) in &self.world {
            if *pid == self.active_project {
                continue;
            }
            for s in issues.values() {
                tally(s);
            }
        }
        (live, needs)
    }

    /// FEAT-B quiet-running overlay: an *Idle* agent counts as "recently active"
    /// while its child kept streaming PTY output within the settle window. Gated to
    /// Idle on purpose — Running/NeedsYou already render live, and a terminal/absent
    /// agent must never read as active, so a late `AgentOutput` (the stamp at
    /// [`Self::record_ledger`]'s sibling arm fires for any issue) can't resurrect a
    /// dead row's band. The window is measured in animation frames, so the
    /// [`Self::is_animating`] clause that keeps the clock alive lets it expire.
    pub fn recently_active(&self, issue: &str) -> bool {
        self.fleet.get(issue) == Some(&AgentStatus::Idle)
            && self
                .last_output
                .get(issue)
                .is_some_and(|&frame| self.frame.wrapping_sub(frame) < OUTPUT_SETTLE_FRAMES)
    }

    pub fn display_agent_status(&self, issue: &str, status: AgentStatus) -> AgentStatus {
        if status == AgentStatus::Idle && self.recently_active(issue) {
            AgentStatus::Running
        } else {
            status
        }
    }

    /// Per-project `(live, needs-you)` counts for the switcher rows (ENG-406): active
    /// from `fleet`, the rest from `world`.
    pub fn project_agent_counts(&self) -> HashMap<String, (usize, usize)> {
        let count = |issues: &HashMap<String, AgentStatus>| {
            (
                issues.values().filter(|s| s.is_live()).count(),
                issues.values().filter(|s| s.needs_you()).count(),
            )
        };
        let mut out: HashMap<String, (usize, usize)> = self
            .world
            .iter()
            .filter(|(pid, _)| **pid != self.active_project)
            .map(|(pid, issues)| (pid.clone(), count(issues)))
            .collect();
        if !self.active_project.is_empty() {
            out.insert(self.active_project.clone(), count(&self.fleet));
        }
        out
    }

    /// Every agent across the workspace as `(project_id, issue, status)` rows — the
    /// global all-agents screen's source (ENG-406). Active project from `fleet`, the
    /// rest from `world`; sorted by project then natural issue id.
    pub fn all_agents(&self) -> Vec<(String, String, AgentStatus)> {
        let mut rows: Vec<(String, String, AgentStatus)> = self
            .fleet
            .iter()
            .map(|(issue, status)| (self.active_project.clone(), issue.clone(), *status))
            .collect();
        for (pid, issues) in &self.world {
            if *pid == self.active_project {
                continue;
            }
            rows.extend(
                issues
                    .iter()
                    .map(|(issue, status)| (pid.clone(), issue.clone(), *status)),
            );
        }
        // Sort by the *displayed* project name (not the invisible UUID), so clusters
        // appear in a legible, stable order rather than arbitrary id order.
        rows.sort_by(|a, b| {
            self.project_name(&a.0)
                .cmp(&self.project_name(&b.0))
                // Two projects can share a display name — keep their clusters distinct and
                // stable by project id rather than interleaving them by issue key.
                .then_with(|| a.0.cmp(&b.0))
                .then_with(|| natural_key_cmp(&a.1, &b.1))
        });
        rows
    }

    /// Whether `issue`'s agent has reached a terminal state (the process is gone).
    fn is_terminal(&self, issue: &str) -> bool {
        matches!(
            self.fleet.get(issue),
            Some(AgentStatus::Stopped | AgentStatus::Done | AgentStatus::Failed)
        )
    }

    /// Whether anything on screen is animating — a live agent's spinner/pulse, an
    /// unexpired node flash, an in-flight auto-resume, or a launch still coming up.
    /// The render loop arms its animation tick only when this holds; a pending launch
    /// must keep it ticking so a wedged one reaches its grace deadline (M9).
    pub fn is_animating(&self) -> bool {
        !self.resuming.is_empty()
            || !self.pending_launch.is_empty()
            || self.flash.values().any(|&(_, until)| self.frame < until)
            || self
                .fleet
                .iter()
                .any(|(issue, status)| *status == AgentStatus::Idle && self.recently_active(issue))
            || self.fleet.values().any(AgentStatus::is_animating)
    }

    /// How many docked agents are still mid-resume — drives the "resuming N…"
    /// header span.
    pub fn resuming_count(&self) -> usize {
        self.resuming.len()
    }

    /// Test seam: mark `issue` as mid-resume. Production arms this through
    /// `resume_one`, which needs a live supervisor. Mirrors `resume_one`
    /// faithfully by arming BOTH the `pending_launch` guard and the `resuming`
    /// spinner deadline — earlier this set only `resuming`, which masked the
    /// wedged-resume bug where the guard was never released.
    #[cfg(test)]
    pub fn mark_resuming_for_test(&mut self, issue: &str) {
        self.pending_launch
            .insert(issue.to_string(), self.frame + RESUME_GRACE_FRAMES);
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
        self.last_output
            .retain(|_, frame| now.wrapping_sub(*frame) < OUTPUT_SETTLE_FRAMES);
        // Drop each wedged resume on its own grace bound, so a stuck spawn can't
        // pin the loop awake — independently of how many other resumes arrive.
        // Also release the matching `pending_launch` guard `resume_one` armed: a
        // wedged spawn (a hung worktree add that never emits AgentSpawned or a
        // setup-failure Notification) would otherwise strand the docked card
        // behind a guard that `maybe_resume_focused`/`resume_one` early-return on
        // forever — it could never be revived once a slot frees. A normal spawn
        // still clears both via AgentSpawned; this only fires for entries that
        // outlived their grace.
        let expired: Vec<String> = self
            .resuming
            .iter()
            .filter(|&(_, &deadline)| now >= deadline)
            .map(|(issue, _)| issue.clone())
            .collect();
        // A wedged *button* launch arms no `resuming` entry, so it self-heals on its
        // own `pending_launch` deadline: drop the stuck "starting…" card and invite a
        // retry instead of stranding it forever (M9). Computed before the resuming
        // retain so a resume-wedge (handled silently just below) isn't double-reported.
        let timed_out: Vec<String> = self
            .pending_launch
            .iter()
            .filter(|&(issue, &deadline)| now >= deadline && !self.resuming.contains_key(issue))
            .map(|(issue, _)| issue.clone())
            .collect();
        self.resuming.retain(|_, &mut deadline| now < deadline);
        for issue in expired {
            self.pending_launch.remove(&issue);
        }
        for issue in timed_out {
            self.pending_launch.remove(&issue);
            if self.pending_attach.as_deref() == Some(issue.as_str()) {
                self.pending_attach = None;
            }
            // Drop the stranded "◌ starting agent…" card so it stops claiming a launch
            // that never arrived; a fresh preview re-aims at the selection (M9).
            self.undock_issue(&issue);
            // This fires autonomously from the animation timer, so use the same guard as
            // the other background writers (M11/H4): never clobber a standing needs-you
            // alert or an armed confirmation's prompt. Word the retry as the gesture that
            // works once the card is closed (select the row + ⏎), not "Enter here" — the
            // focused empty card would have swallowed Enter to a non-existent PTY.
            if !self.needs_you_alert && !self.confirm_pending() {
                self.status_msg = Some(format!(
                    "launch on {issue} timed out — select it & ⏎ to retry"
                ));
            }
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
        self.set_jump_status("cycle", &key, n, total);
    }

    /// Jump to the next issue whose agent needs you, in display order, wrapping.
    fn jump_to_needs_you(&mut self) {
        // Source targets from the fleet — the same set the header counts and the
        // roster shows — not from graph.keys(). An agent can be in the fleet yet
        // absent from the graph (e.g. a worktree-backed session for an issue
        // archived/moved out of Linear between runs survives reconcile but isn't
        // in the freshly fetched graph); graph-bounding here made the header
        // advertise "N needs you" for an agent this jump could never reach. Stable
        // natural-id order so the wrapping cursor is deterministic (the roster
        // sorts the same way; raw HashMap order isn't stable).
        let mut members: Vec<String> = self
            .fleet
            .iter()
            .filter(|(_, s)| s.needs_you())
            .map(|(k, _)| k.clone())
            .collect();
        members.sort_by(|a, b| natural_key_cmp(a, b));
        if members.is_empty() {
            // Nothing needs you here — but a backgrounded project might (ENG-406).
            // Jump cross-project: switch to it, land on the needy issue, and attach.
            return self.jump_to_needs_you_elsewhere();
        }
        let next = match members.iter().position(|k| *k == self.root) {
            Some(i) => (i + 1) % members.len(),
            None => 0,
        };
        let (key, n, total) = (members[next].clone(), next + 1, members.len());
        self.set_jump_status("needs you", &key, n, total);
        // If this needy issue is already a pinned coin, go straight to it (chat
        // face) so you can respond; otherwise the preview follows the selection.
        // Runs after `set_jump_status` (which aims the spine) so this focus wins.
        if !self.focus_pinned_chat(&key) {
            self.windows.focus_preview();
            self.after_focus_change();
        }
    }

    /// Cross-project leg of the needs-you jump (ENG-406): when no agent in the active
    /// project needs you, reach the next one in ANY backgrounded project — switching
    /// to it and (on arrival) attaching to its PTY (the user's chosen jump behaviour).
    /// Sources the backgrounded needs-you tally (`other_needs_you`), in stable order.
    fn jump_to_needs_you_elsewhere(&mut self) {
        let mut targets: Vec<(String, String)> = self
            .other_needs_you
            .iter()
            .flat_map(|(pid, issues)| issues.iter().map(move |i| (pid.clone(), i.clone())))
            .collect();
        targets.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| natural_key_cmp(&a.1, &b.1)));
        let Some((project_id, issue)) = targets.into_iter().next() else {
            self.status_msg = Some("no agents need you right now".into());
            return;
        };
        let name = self.project_name(&project_id);
        self.set_footer(format!("→ {issue} in {name} needs you"));
        self.land_on(&project_id, &issue, true);
    }

    /// Open the global all-agents screen (ENG-406, `Ctrl-a a`): a snapshot of every
    /// agent across the workspace. A footer no-op when nothing is running anywhere.
    fn open_global(&mut self) {
        let rows = self.all_agents();
        if rows.is_empty() {
            self.set_footer("no agents anywhere — open one with the button".into());
            return;
        }
        let mut state = ListState::default();
        state.select(Some(0));
        self.global_view = Some(GlobalView { rows, state });
    }

    /// Drive the global all-agents screen: ↑↓ move, Enter re-roots onto the selected
    /// `(project, issue)` (attach-ready — switching projects if it's elsewhere), Esc
    /// backs out.
    fn on_global_key(&mut self, key: KeyEvent) {
        let Some(view) = self.global_view.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.global_view = None,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.global_view = None;
            }
            KeyCode::Down => move_state(&mut view.state, view.rows.len(), 1),
            KeyCode::Up => move_state(&mut view.state, view.rows.len(), -1),
            KeyCode::Enter => {
                let target = view
                    .state
                    .selected()
                    .and_then(|i| view.rows.get(i))
                    .map(|(p, i, _)| (p.clone(), i.clone()));
                self.global_view = None;
                if let Some((project_id, issue)) = target {
                    // Attach-ready (re-root + select), not auto-attached — Enter on a
                    // row lands you on the issue; press the button / `t` to take over.
                    self.land_on(&project_id, &issue, false);
                }
            }
            _ => {}
        }
    }

    /// Re-root onto `(project_id, issue)`, switching projects first if it's elsewhere
    /// (ENG-406). `attach` opens + focuses the agent's chat (the cross-project `n`
    /// jump's auto-attach); otherwise it just aims the spine at the issue
    /// (attach-ready). A cross-project land stashes the target in `pending_land` and
    /// applies it in [`activate_project`](Self::activate_project) once the graph is live.
    fn land_on(&mut self, project_id: &str, issue: &str, attach: bool) {
        if project_id == self.active_project || self.active_project.is_empty() {
            self.aim_spine(issue.to_string());
            if attach {
                self.open_agent_window(issue);
            }
            return;
        }
        let Some(target) = self
            .project_list
            .iter()
            .find(|p| p.id == project_id)
            .cloned()
        else {
            self.set_footer(format!("can't switch to the project for {issue}"));
            return;
        };
        self.request_switch(target);
        // Stamp the land with the switch generation request_switch just used, so a
        // later switch that supersedes this one drops the land instead of applying it
        // to the wrong project.
        self.pending_land = Some((
            project_id.to_string(),
            issue.to_string(),
            attach,
            self.switch_seq,
        ));
    }

    /// Aim the spine at `key` and set the footer to "<prefix> n/total — key",
    /// flagging when the landing row is hidden by the active filter. Shared by
    /// the cycle and needs-you spine jumps.
    fn set_jump_status(&mut self, prefix: &str, key: &str, n: usize, total: usize) {
        self.aim_spine(key.to_string());
        self.status_msg = Some(format!(
            "{prefix} {n}/{total} — {key}{}",
            self.hidden_note()
        ));
    }

    /// A status suffix flagging that a jump landed on a selection with no Spine row
    /// — so the empty list highlight reads as deliberate, not a glitch.
    fn hidden_note(&self) -> String {
        // An agent can sit on an issue that has left this project's graph (archived /
        // moved, its session surviving reconcile): it's reachable via `n` and counted,
        // yet has no Spine row. Point at the global screen that *does* list it (M14).
        if !self.root.is_empty() && self.graph.get(&self.root).is_none() {
            let global = self.keymap.verb_label(Action::GlobalView);
            return format!(" · agent on an issue not in this project ({global} for ALL AGENTS)");
        }
        if self.root_is_hidden() {
            " · hidden by filter (clear it to list)".to_string()
        } else {
            String::new()
        }
    }

    fn on_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Ctrl-C cancels like Esc: drop the in-progress query rather than
                // committing it as a sticky filter.
                self.search_active = false;
                self.search_query.clear();
                self.rebuild_order();
            }
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
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && self.search_query.len() < 64 =>
            {
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
/// Render a `delete_mirror` outcome as the reclaim prompt's footer line — a
/// success names the freed handle, a refusal surfaces the error (e.g. a clone
/// raced in and re-referenced the mirror, the alternates guard).
fn reclaim_note(res: Result<(), crate::mirror::MirrorError>, handle: &str) -> String {
    match res {
        Ok(()) => format!("reclaimed mirror {handle}"),
        Err(e) => format!("reclaim refused: {e}"),
    }
}

/// The most-connected non-external node — the cockpit's default root/selection
/// for a freshly-loaded graph (mirrors the same pick in [`App::new`]; reused when
/// switching projects).
fn most_connected_root(graph: &Graph) -> String {
    graph
        .keys()
        .iter()
        .filter(|k| graph.get(k).is_some_and(|i| !i.external))
        .max_by_key(|k| {
            graph.direct_count(k, Direction::Upstream)
                + graph.direct_count(k, Direction::Downstream)
        })
        .cloned()
        .unwrap_or_default()
}

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

/// Sort rank that surfaces live work first.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::FakeBackend;
    use crate::demo;
    use crate::model::Issue;
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

    // ── Project switching (Ctrl-a s) ─────────────────────────────────────────

    fn project(id: &str, name: &str) -> ProjectRef {
        ProjectRef {
            id: id.into(),
            name: name.into(),
        }
    }

    // ── Up-front repo multi-select (ENG-536) ─────────────────────────────────

    fn repo(handle: &str, primary: bool) -> RepoChoice {
        RepoChoice {
            handle: handle.into(),
            local: false,
            primary,
        }
    }

    #[test]
    fn launching_a_multi_repo_project_opens_the_repo_select_first() {
        let mut a = app();
        a.workspace = Some(crate::workspace::WorkspaceHandle::detached());
        a.active_project = "proj".into();
        // Aim at a Ready issue — ENG-559's gate refuses a blocked dispatch, and
        // the demo's default selection (ZAP-204) is blocked.
        a.aim_spine("ZAP-188".into());
        let key = a.root.clone();
        a.project_candidates
            .insert("proj".into(), vec![repo("api", true), repo("web", false)]);

        a.button();
        assert!(
            a.repo_select.is_some(),
            "more than one candidate opens the select"
        );
        assert!(
            a.pending_launch.is_empty(),
            "nothing launched yet — the selection pends"
        );

        // Confirm with Enter: the launch fires (pending) and the modal closes.
        a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(a.repo_select.is_none(), "confirming closes the modal");
        assert!(
            a.pending_launch.contains_key(&key),
            "the agent launch is now in flight"
        );
    }

    #[test]
    fn ask_agent_mints_a_synthetic_row_and_launches() {
        let mut a = app();
        a.workspace = Some(crate::workspace::WorkspaceHandle::detached());
        a.active_project = "proj".into();
        a.filter = Filter::HasDeps;
        a.search_active = true;
        a.search_query = "nothing".into();
        a.rebuild_order();

        verb(&mut a, KeyCode::Char('?'));

        let key = a
            .pending_launch
            .keys()
            .next()
            .expect("ask launch is in flight")
            .clone();
        assert!(crate::worktree::is_synthetic_ask_id(&key));
        assert!(a.graph.get(&key).is_some(), "ask agent has a graph row");
        assert!(a.ask_agents.contains(&key), "ask id is tracked");
        assert_eq!(a.root, key, "the synthetic row becomes the selection");
        assert_eq!(a.filter, Filter::All, "ask launch reveals the row");
        assert!(a.search_query.is_empty(), "ask launch clears hiding search");
    }

    #[test]
    fn ask_agent_uses_the_repo_select_for_multi_repo_projects() {
        let mut a = app();
        a.workspace = Some(crate::workspace::WorkspaceHandle::detached());
        a.active_project = "proj".into();
        a.project_candidates
            .insert("proj".into(), vec![repo("api", true), repo("web", false)]);

        verb(&mut a, KeyCode::Char('?'));

        let select = a
            .repo_select
            .as_ref()
            .expect("ask launch opens repo select");
        assert!(select.adhoc, "repo select remembers this is an ask launch");
        let key = select.issue.clone();
        assert!(crate::worktree::is_synthetic_ask_id(&key));
        assert!(
            a.pending_launch.is_empty(),
            "ask launch waits for repo confirmation"
        );

        a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(a.repo_select.is_none(), "confirming closes the modal");
        assert!(
            a.pending_launch.contains_key(&key),
            "confirmed ask launch is now in flight"
        );
    }

    #[test]
    fn a_bare_notification_keeps_launch_guards_and_a_rejection_is_scoped() {
        // M10: a bare Notification (cross-issue chatter) must not clear in-flight
        // launch guards; only a typed LaunchRejected for a specific issue drops that
        // one issue's guard, leaving siblings' double-press protection intact.
        let mut a = app();
        a.pending_launch.insert("ENG-1".into(), 999);
        a.pending_launch.insert("ENG-2".into(), 999);

        a.apply_event(AppEvent::Notification("ENG-9: scratch skipped".into()));
        assert!(
            a.pending_launch.contains_key("ENG-1") && a.pending_launch.contains_key("ENG-2"),
            "a cross-issue toast keeps every launch guard"
        );

        a.apply_event(AppEvent::LaunchRejected {
            issue: "ENG-1".into(),
            reason: "ENG-1 already has a running agent".into(),
        });
        assert!(
            !a.pending_launch.contains_key("ENG-1"),
            "the rejection drops only its own issue's guard"
        );
        assert!(
            a.pending_launch.contains_key("ENG-2"),
            "an unrelated issue's guard is untouched"
        );
    }

    #[test]
    fn launching_a_single_repo_project_skips_the_select() {
        let mut a = app();
        a.workspace = Some(crate::workspace::WorkspaceHandle::detached());
        a.active_project = "proj".into();
        a.aim_spine("ZAP-188".into()); // a Ready issue (the ENG-559 gate)
        let key = a.root.clone();
        a.project_candidates
            .insert("proj".into(), vec![repo("only", true)]);

        a.button();
        assert!(
            a.repo_select.is_none(),
            "a single candidate launches straight away — no modal"
        );
        assert!(a.pending_launch.contains_key(&key));
    }

    #[test]
    fn choose_repos_opens_the_select_even_for_a_single_candidate_project() {
        // CF-20: `Ctrl-a c` forces the modal open where plain Enter would fast-launch —
        // the on-demand entry to give one agent more than one repo.
        let mut a = app();
        a.workspace = Some(crate::workspace::WorkspaceHandle::detached());
        a.active_project = "proj".into();
        a.aim_spine("ZAP-188".into()); // a Ready issue (the ENG-559 gate)
        a.project_candidates
            .insert("proj".into(), vec![repo("only", true)]);

        verb(&mut a, KeyCode::Char('c'));
        assert!(
            a.repo_select.is_some(),
            "choose-repos opens the select on a single candidate"
        );
        assert!(a.pending_launch.is_empty(), "nothing launched yet");
    }

    #[test]
    fn adding_a_repo_in_the_picker_persists_it_as_a_candidate_and_launches_with_both() {
        // CF-20 end-to-end: open the select via `Ctrl-a c` on a single-candidate
        // project, press `a` to add a registered repo the project didn't list, confirm
        // it in, then launch — the new repo rides this launch AND becomes a durable
        // candidate (here: the in-session `project_candidates`; the registry.toml write
        // is exercised by registry::add_candidate's own tests since this App has no layout).
        let mut a = app();
        a.workspace = Some(crate::workspace::WorkspaceHandle::detached());
        a.active_project = "proj".into();
        a.aim_spine("ZAP-188".into());
        let key = a.root.clone();
        a.project_candidates
            .insert("proj".into(), vec![repo("api", true)]);
        a.registered_repos = vec![repo("api", false), repo("web", false)];

        verb(&mut a, KeyCode::Char('c'));
        assert!(a.repo_select.is_some());

        press(&mut a, KeyCode::Char('a')); // open the "add another repo" sub-list
        assert!(
            a.repo_select.as_ref().unwrap().picker.is_adding(),
            "`a` opens the add-list"
        );
        press(&mut a, KeyCode::Enter); // add the only addable repo (web)
        assert!(
            !a.repo_select.as_ref().unwrap().picker.is_adding(),
            "adding closes the sub-list"
        );
        assert_eq!(
            a.repo_select.as_ref().unwrap().picker.selected_handles(),
            vec!["api", "web"],
            "both repos are now checked"
        );

        press(&mut a, KeyCode::Enter); // confirm the launch
        assert!(a.repo_select.is_none(), "confirming closes the modal");
        assert!(a.pending_launch.contains_key(&key), "the launch is in flight");

        let cands: Vec<&str> = a.project_candidates["proj"]
            .iter()
            .map(|c| c.handle.as_str())
            .collect();
        assert_eq!(
            cands,
            vec!["api", "web"],
            "the added repo is now a durable candidate (no restart)"
        );
    }

    #[test]
    fn cancelling_the_repo_select_launches_nothing() {
        let mut a = app();
        a.workspace = Some(crate::workspace::WorkspaceHandle::detached());
        a.active_project = "proj".into();
        a.aim_spine("ZAP-188".into()); // a Ready issue (the ENG-559 gate)
        a.project_candidates
            .insert("proj".into(), vec![repo("api", true), repo("web", false)]);

        a.button();
        assert!(a.repo_select.is_some());
        a.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(a.repo_select.is_none(), "Esc closes the modal");
        assert!(a.pending_launch.is_empty(), "cancel launches nothing");
    }

    // ── Disk reclaim (ENG-540, Ctrl-a m) ─────────────────────────────────────

    /// A temp `~/.lindep` with one unreferenced mirror (no project clones it).
    fn layout_with_unreferenced_mirror(tag: &str) -> (crate::registry::Layout, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("lindep-app-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mirror = root.join("mirrors").join("core.git");
        std::fs::create_dir_all(&mirror).unwrap();
        std::fs::write(mirror.join("data"), b"objects").unwrap();
        (crate::registry::Layout::new(&root), mirror)
    }

    #[test]
    fn the_reclaim_prompt_surfaces_unreferenced_mirrors_and_closes_on_esc() {
        let (layout, _mirror) = layout_with_unreferenced_mirror("reclaim-esc");
        let mut a = app();
        a.layout = Some(layout);
        a.open_reclaim();
        assert!(a.reclaim.is_some(), "an unreferenced mirror is surfaced");
        a.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(a.reclaim.is_none(), "Esc closes the prompt");
    }

    #[test]
    fn reclaiming_the_last_mirror_deletes_it_and_closes() {
        let (layout, mirror) = layout_with_unreferenced_mirror("reclaim-del");
        let mut a = app();
        a.layout = Some(layout);
        a.open_reclaim();
        assert!(a.reclaim.is_some());
        // Enter reclaims the only mirror → it's deleted and the prompt closes.
        a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!mirror.exists(), "the mirror was reclaimed from disk");
        assert!(
            a.reclaim.is_none(),
            "the prompt closes when nothing remains"
        );
        let _ = std::fs::remove_dir_all(mirror.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn reclaim_with_no_control_plane_is_a_footer_no_op() {
        let mut a = app(); // layout is None (no control plane)
        a.open_reclaim();
        assert!(a.reclaim.is_none());
    }

    #[test]
    fn an_initial_reclaim_scan_opens_the_prompt_when_a_mirror_is_free() {
        let mut a = app();
        a.apply_reclaim_scan(
            vec![crate::mirror::ReclaimableMirror {
                handle: "core".into(),
                size_bytes: 1,
            }],
            true,
            None,
        );
        assert!(a.reclaim.is_some(), "a free mirror opens the prompt");
    }

    #[test]
    fn a_late_reclaim_rescan_never_reopens_a_closed_prompt() {
        // The user pressed Esc (reclaim is None) while a post-delete rescan was
        // still in flight; the late result must not resurrect the modal, and it
        // must clear the busy latch.
        let mut a = app();
        a.reclaim_busy = true;
        a.apply_reclaim_scan(
            vec![crate::mirror::ReclaimableMirror {
                handle: "core".into(),
                size_bytes: 1,
            }],
            false,
            Some("reclaimed mirror web".into()),
        );
        assert!(
            a.reclaim.is_none(),
            "a rescan never reopens a closed prompt"
        );
        assert!(!a.reclaim_busy, "the busy latch is cleared by the rescan");
    }

    // ── Discard workspace (ENG-541, Ctrl-a d) ────────────────────────────────

    #[test]
    fn discard_confirms_then_drops_the_issue_on_the_terminal_signal() {
        // CF-12: confirming no longer drops the UI synchronously — it waits for teardown
        // to report. `WorkspaceDiscarded` (worktrees actually removed) drops the fleet;
        // a `DiscardKeptWorktree` would instead flag it and keep it, so local work is
        // never silently stranded.
        let mut a = app();
        a.workspace = Some(crate::workspace::WorkspaceHandle::detached());
        a.active_project = "proj".into();
        let key = a.root.clone();
        a.fleet.insert(key.clone(), AgentStatus::Done); // terminal — not live
        a.arm_discard();
        assert_eq!(a.discard_confirm.as_deref(), Some(key.as_str()));
        a.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(a.discard_confirm.is_none(), "the confirm resolves");
        assert!(
            a.fleet.contains_key(&key),
            "the fleet entry is NOT dropped until teardown confirms the removal"
        );
        a.apply_event(AppEvent::WorkspaceDiscarded {
            project_id: "proj".into(),
            issue: key.clone(),
        });
        assert!(
            !a.fleet.contains_key(&key),
            "WorkspaceDiscarded drops the discarded issue's fleet entry"
        );
    }

    #[test]
    fn confirming_a_kill_tombstones_so_a_late_hook_is_dropped() {
        // CF-4: a hook in flight before the cancel must be dropped by the resurrection
        // guards, not re-raise "needs you" / re-promote the dying agent.
        let mut a = app();
        a.workspace = Some(crate::workspace::WorkspaceHandle::detached());
        a.active_project = "proj".into();
        register(&mut a, "ZAP-204");
        a.fleet.insert("ZAP-204".into(), AgentStatus::Running);
        a.aim_spine("ZAP-204".into());
        a.windows.focus = 0;
        verb(&mut a, KeyCode::Char('x')); // arm kill
        a.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)); // confirm
        assert!(
            a.reaped.contains("ZAP-204"),
            "the killed issue is tombstoned"
        );
        a.needs_you_alert = false;
        a.apply_event(AppEvent::AgentNeedsYou {
            project_id: "proj".into(),
            issue: "ZAP-204".into(),
            reason: "permission".into(),
        });
        assert!(
            !a.needs_you_alert,
            "a late needs-you hook for a killed agent is dropped"
        );
    }

    #[test]
    fn discard_is_refused_while_the_agent_is_live() {
        let mut a = app();
        a.workspace = Some(crate::workspace::WorkspaceHandle::detached());
        let key = a.root.clone();
        a.fleet.insert(key.clone(), AgentStatus::Running); // live — checkout in use
        a.arm_discard();
        assert!(
            a.discard_confirm.is_none(),
            "a live agent's workspace can't be discarded"
        );
    }

    // ── Fenced lazy-pull confirm (ENG-542) ───────────────────────────────────

    #[test]
    fn a_repo_request_raises_the_confirm_and_y_pulls() {
        let mut a = app();
        a.workspace = Some(crate::workspace::WorkspaceHandle::detached());
        a.active_project = "proj".into();
        a.apply_event(AppEvent::RepoRequested {
            project_id: "proj".into(),
            issue: "ENG-1".into(),
            repo_handle: "web".into(),
        });
        assert!(
            a.repo_confirm.is_some(),
            "a request raises the confirm modal"
        );
        // y confirms → fires the (detached) materialize command and closes.
        a.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(a.repo_confirm.is_none(), "confirming resolves the modal");
    }

    #[test]
    fn a_repo_request_is_denied_by_any_other_key() {
        let mut a = app();
        a.workspace = Some(crate::workspace::WorkspaceHandle::detached());
        a.active_project = "proj".into();
        a.apply_event(AppEvent::RepoRequested {
            project_id: "proj".into(),
            issue: "ENG-1".into(),
            repo_handle: "web".into(),
        });
        a.on_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(a.repo_confirm.is_none(), "any non-y key denies the pull");
    }

    #[test]
    fn a_pending_kill_confirm_blocks_a_repo_request_from_raising_its_prompt() {
        // A repo-pull request landing while a kill confirmation is armed must NOT
        // raise repo_confirm: the on_key band resolves kill before repo, so a
        // "y to pull" footer over a pending kill would let `y` destroy the agent.
        let mut a = app();
        a.active_project = "proj".into();
        a.kill_confirm = Some("ENG-1".into());
        a.apply_event(AppEvent::RepoRequested {
            project_id: "proj".into(),
            issue: "ENG-1".into(),
            repo_handle: "web".into(),
        });
        assert!(
            a.repo_confirm.is_none(),
            "a repo request can't shadow a pending kill confirmation"
        );
        assert!(
            a.kill_confirm.is_some(),
            "the kill confirmation is untouched"
        );
    }

    #[test]
    fn background_chatter_never_overwrites_an_armed_confirmation_prompt() {
        // H4: a kill is armed — its red prompt and `y` are live against the visible
        // text. Routine background events (a tool ran, a push, a materialise tick, a
        // deliberate footer) must not overwrite that prompt, or a reflexive `y` would
        // confirm a kill under a misleading message.
        let mut a = app();
        a.active_project = "proj".into();
        let prompt = "kill agent on ENG-1? y to confirm, any key to cancel".to_string();
        a.kill_confirm = Some("ENG-1".into());
        a.status_msg = Some(prompt.clone());

        // An unrelated agent's working AgentAction.
        a.fleet.insert("ENG-2".into(), AgentStatus::Running);
        a.apply_event(AppEvent::AgentAction {
            project_id: "proj".into(),
            issue: "ENG-2".into(),
            action: "ran Bash".into(),
            working: true,
        });
        assert_eq!(
            a.status_msg.as_deref(),
            Some(prompt.as_str()),
            "AgentAction must not shadow the armed kill prompt"
        );

        // A push and a materialise tick.
        a.apply_event(AppEvent::MaterializeProgress {
            project_id: "proj".into(),
            phase: "clone".into(),
            percent: 40,
        });
        // A deliberate set_footer (e.g. AgentExited / Notification) is suppressed too.
        a.set_footer("ENG-3 exited".into());
        assert_eq!(
            a.status_msg.as_deref(),
            Some(prompt.as_str()),
            "neither a progress tick nor set_footer may shadow the kill prompt"
        );
        assert!(a.kill_confirm.is_some(), "the kill stays armed throughout");
    }

    #[test]
    fn a_full_modal_defers_a_repo_request() {
        // M7: a repo-pull request arriving while a full modal owns the keyboard must
        // NOT raise repo_confirm — its "y to pull" would never reach the confirm and
        // would leak to a later key. The agent re-issues request-repo, so the deferral
        // is safe.
        let mut a = app();
        a.active_project = "proj".into();
        a.global_view = Some(GlobalView {
            rows: Vec::new(),
            state: ListState::default(),
        });
        assert!(a.full_modal_open());
        a.apply_event(AppEvent::RepoRequested {
            project_id: "proj".into(),
            issue: "ENG-1".into(),
            repo_handle: "web".into(),
        });
        assert!(
            a.repo_confirm.is_none(),
            "a full modal defers the repo request rather than leaking its y"
        );
    }

    #[test]
    fn ctrl_a_question_opens_help_from_the_fleet() {
        // M6: a focused Chat coin forwards `?` to its PTY, so the prefix form must be
        // able to summon help from a pane that can't reach the direct key. The Fleet
        // is the simplest such focus to drive in a test.
        let mut a = app();
        a.open_fleet();
        assert!(matches!(a.windows.focused_kind(), WindowKind::Fleet));
        a.prefix_armed = true;
        a.on_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        assert!(a.show_help, "Ctrl-a ? opens help from the Fleet");
    }

    #[test]
    fn ctrl_a_question_also_closes_help_from_the_fleet() {
        // M6 follow-up: the prefix help gesture must TOGGLE — pressing it again from the
        // same pane closes help, not re-open it (the top-of-on_prefix_key overlay clear
        // would otherwise make it open-only).
        let mut a = app();
        a.open_fleet();
        a.show_help = true; // already open
        a.prefix_armed = true;
        a.on_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        assert!(!a.show_help, "Ctrl-a ? again closes help from the Fleet");
    }

    #[test]
    fn a_single_row_spine_does_not_emit_a_spurious_wrapped_footer() {
        // A 1-row Spine can't move, so j/k must stay silent — rem_euclid keeps
        // next == cur == 0 == len-1, which used to trip the top↔bottom wrap message.
        let mut a = app();
        a.order = vec!["ZAP-201".into()];
        a.root = "ZAP-201".into();
        a.list_state.select(Some(0));
        a.status_msg = None;
        a.dispatch_spine(Action::MoveDown);
        assert!(
            a.status_msg.is_none(),
            "j on a 1-row spine is silent, got {:?}",
            a.status_msg
        );
        a.dispatch_spine(Action::MoveUp);
        assert!(
            a.status_msg.is_none(),
            "k on a 1-row spine is silent, got {:?}",
            a.status_msg
        );
    }

    #[test]
    fn a_background_needs_you_does_not_overwrite_an_armed_kill_prompt() {
        // H4 (extended): with a kill armed, an UNRELATED agent firing needs-you must not
        // overwrite the red "confirm kill" prompt — a reflexive `y` would then kill the
        // armed issue under a message naming a different one. The needs-you is still
        // recorded (fleet + sticky flag), it just doesn't seize the prompt slot.
        let mut a = app();
        a.active_project = "proj".into();
        let prompt = "kill agent on ZAP-1? y to confirm, any key to cancel".to_string();
        a.kill_confirm = Some("ZAP-1".into());
        a.status_msg = Some(prompt.clone());
        a.apply_event(AppEvent::AgentNeedsYou {
            project_id: "proj".into(),
            issue: "ZAP-9".into(),
            reason: "permission?".into(),
        });
        assert_eq!(
            a.status_msg.as_deref(),
            Some(prompt.as_str()),
            "the armed kill prompt must survive a background needs-you"
        );
        assert_eq!(
            a.fleet.get("ZAP-9"),
            Some(&AgentStatus::NeedsYou),
            "the needs-you is still recorded"
        );
        assert!(a.kill_confirm.is_some(), "the kill stays armed");
    }

    #[test]
    fn focus_left_onto_the_spine_clears_zoom() {
        // M1: FocusLeft/Right landing on the Spine (index 0) must clear zoom, mirroring
        // FocusNav — else direct keys route to an off-screen Spine (blind dispatch).
        let mut a = app();
        a.open_fleet(); // a non-spine window, now focused
        assert!(a.windows.focus != 0, "the fleet is focused, not the spine");
        a.windows.zoomed = true;
        while a.windows.focus != 0 {
            a.dispatch_verb(Action::FocusLeft);
        }
        assert!(!a.windows.zoomed, "landing focus on the Spine cleared zoom");
    }

    #[test]
    fn a_second_repo_request_does_not_clobber_a_pending_one() {
        let mut a = app();
        a.active_project = "proj".into();
        a.apply_event(AppEvent::RepoRequested {
            project_id: "proj".into(),
            issue: "ENG-1".into(),
            repo_handle: "web".into(),
        });
        a.apply_event(AppEvent::RepoRequested {
            project_id: "proj".into(),
            issue: "ENG-1".into(),
            repo_handle: "docs".into(),
        });
        let rc = a
            .repo_confirm
            .as_ref()
            .expect("the first request still stands");
        assert_eq!(rc.handle, "web", "the first pending request is preserved");
    }

    // ── Global fleet view + cross-project state (ENG-406) ────────────────────

    #[test]
    fn the_workspace_summary_sums_agents_across_projects() {
        let mut a = app();
        a.active_project = "proj-a".into();
        // Active project (from the authoritative fleet): 2 live, 1 needing you.
        a.fleet.insert("ENG-1".into(), AgentStatus::Running);
        a.fleet.insert("ENG-2".into(), AgentStatus::NeedsYou);
        // A backgrounded project (from the event-fed world map): 1 live, 1 terminal.
        let bg = a.world.entry("proj-b".into()).or_default();
        bg.insert("ENG-9".into(), AgentStatus::Idle);
        bg.insert("ENG-8".into(), AgentStatus::Done);

        assert_eq!(
            a.workspace_summary(),
            (3, 1),
            "2 active + 1 background live; one needs you"
        );
        let counts = a.project_agent_counts();
        assert_eq!(counts.get("proj-a"), Some(&(2, 1)));
        assert_eq!(counts.get("proj-b"), Some(&(1, 0)));
        let rows = a.all_agents();
        assert_eq!(rows.len(), 4, "every agent across both projects: {rows:?}");
    }

    #[test]
    fn apply_event_tracks_a_backgrounded_projects_agents_in_world() {
        let mut a = app();
        a.active_project = "proj-a".into();
        // A backgrounded project's status event updates `world` even though the
        // scoping guard drops it from the on-screen fleet.
        a.apply_event(AppEvent::AgentStatusChanged {
            project_id: "proj-b".into(),
            issue: "ENG-9".into(),
            status: AgentStatus::Running,
        });
        assert_eq!(
            a.world.get("proj-b").and_then(|m| m.get("ENG-9")),
            Some(&AgentStatus::Running)
        );
        assert!(
            !a.fleet.contains_key("ENG-9"),
            "the backgrounded agent isn't in the active fleet"
        );
        a.apply_event(AppEvent::AgentReaped {
            project_id: "proj-b".into(),
            issue: "ENG-9".into(),
        });
        assert!(
            !a.world.contains_key("proj-b"),
            "reaping the last agent drops the project from world"
        );
    }

    #[test]
    fn the_global_view_opens_and_enter_re_roots_within_the_project() {
        let mut a = app();
        a.active_project = "proj-a".into();
        let target = a
            .order
            .iter()
            .find(|k| **k != a.root)
            .cloned()
            .expect("the demo graph has more than one issue");
        a.fleet.insert(target.clone(), AgentStatus::Running);

        a.open_global();
        assert!(a.global_view.is_some(), "the global screen opens");
        a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(a.global_view.is_none(), "Enter closes the screen");
        assert_eq!(a.root, target, "it re-rooted onto the selected agent");
    }

    #[test]
    fn a_late_hook_after_reap_does_not_leak_a_phantom_into_the_cross_project_roll_up() {
        let mut a = app();
        a.active_project = "proj-a".into();
        a.reaped.insert("ENG-1".into()); // ENG-1 was reaped in the active project
        // A late NeedsYou hook for the reaped issue must NOT re-enter the world map…
        a.apply_event(AppEvent::AgentNeedsYou {
            project_id: "proj-a".into(),
            issue: "ENG-1".into(),
            reason: "late".into(),
        });
        assert!(
            !a.world
                .get("proj-a")
                .is_some_and(|m| m.contains_key("ENG-1")),
            "no phantom in world[active]"
        );
        // …so it can't inflate the workspace roll-up once you switch away (the bug).
        a.activate_project(project("proj-b", "Beta"), demo::graph());
        assert_eq!(
            a.workspace_summary(),
            (0, 0),
            "no phantom agent inflates the cross-project roll-up after a switch"
        );
    }

    #[test]
    fn the_needs_you_jump_reaches_a_backgrounded_project() {
        let mut a = app();
        a.workspace = Some(crate::workspace::WorkspaceHandle::detached());
        a.active_project = "proj-a".into();
        a.project_list = vec![project("proj-a", "Alpha"), project("proj-b", "Beta")];
        // Nothing needs you here, but proj-b does.
        a.other_needs_you
            .entry("proj-b".into())
            .or_default()
            .insert("ENG-9".into());

        a.jump_to_needs_you();
        // A cross-project landing is stashed (project + issue + attach=true) for the
        // switch to apply; the switch itself no-ops here (no Linear client wired).
        let pl = a.pending_land.clone().expect("a land was stashed");
        assert_eq!(
            (pl.0.as_str(), pl.1.as_str(), pl.2),
            ("proj-b", "ENG-9", true)
        );
    }

    #[test]
    fn a_kill_confirm_outranks_a_repo_confirm_for_the_y_key() {
        // The repo-confirm band sits BELOW kill-confirm, so when both are somehow
        // pending a `y` resolves the kill first (its band is checked earlier).
        let mut a = app();
        a.workspace = Some(crate::workspace::WorkspaceHandle::detached());
        a.kill_confirm = Some("ENG-1".into());
        a.repo_confirm = Some(RepoConfirm {
            project_id: "proj".into(),
            issue: "ENG-1".into(),
            handle: "web".into(),
        });
        a.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(a.kill_confirm.is_none(), "the kill confirm consumed the y");
        assert!(a.repo_confirm.is_some(), "the repo confirm is untouched");
    }

    #[test]
    fn switching_projects_swaps_the_graph_and_stashes_then_restores_live_backends() {
        let mut a = app();
        a.active_project = "proj-a".into();
        let agent = register(&mut a, "ENG-1"); // a live backend in proj-a
        a.fleet.insert("ENG-1".into(), AgentStatus::Running);

        // Switch to proj-b: the active view is clean, proj-a's backend is stashed
        // (not killed — its Arc lives on for a switch back).
        a.activate_project(project("proj-b", "Beta"), demo::graph());
        assert_eq!(a.active_project, "proj-b");
        assert!(
            a.backends.is_empty(),
            "the new project starts with no backends"
        );
        assert!(a.fleet.is_empty(), "the new project's fleet starts empty");
        assert!(
            a.stashed_backends
                .get("proj-a")
                .is_some_and(|m| m.contains_key("ENG-1")),
            "the left project's live backend is stashed"
        );
        drop(agent);

        // Switch back: the still-live backend is re-attached, no relaunch.
        a.activate_project(project("proj-a", "Alpha"), demo::graph());
        assert_eq!(a.active_project, "proj-a");
        assert!(
            a.backends.contains_key("ENG-1"),
            "switching back re-attaches the still-live backend"
        );
    }

    #[test]
    fn a_discard_right_after_switch_back_is_refused_for_a_live_agent() {
        // H2: the switch-back window must not let `Ctrl-a d` discard a live agent's
        // worktree. The fleet is seeded from the restored live Arc, so the discard is
        // refused on the very next keystroke instead of acting on an empty fleet.
        let mut a = app();
        a.active_project = "proj-a".into();
        let fake = FakeBackend::new("ZAP-9") as Arc<dyn AgentBackend>;
        a.stashed_backends
            .entry("proj-b".into())
            .or_default()
            .insert("ZAP-9".into(), fake);
        a.activate_project(project("proj-b", "Beta"), demo::graph());
        assert!(
            a.backends.contains_key("ZAP-9"),
            "the live backend is restored"
        );
        a.root = "ZAP-9".into();
        a.arm_discard();
        assert!(
            a.discard_confirm.is_none(),
            "discard is refused for a live agent right after switch-back"
        );
        assert!(
            a.status_msg
                .as_deref()
                .unwrap_or("")
                .contains("still running"),
            "and it says why: {:?}",
            a.status_msg
        );
    }

    #[test]
    fn a_kill_right_after_switch_back_arms_instead_of_mis_answering_not_running() {
        // The same empty-fleet window made `Ctrl-a x` wrongly report "not running" for
        // a live agent — now the seeded fleet (and the backend ground truth) arm it.
        let mut a = app();
        a.active_project = "proj-a".into();
        let fake = FakeBackend::new("ZAP-9") as Arc<dyn AgentBackend>;
        a.stashed_backends
            .entry("proj-b".into())
            .or_default()
            .insert("ZAP-9".into(), fake);
        a.activate_project(project("proj-b", "Beta"), demo::graph());
        a.root = "ZAP-9".into();
        a.arm_kill();
        assert_eq!(
            a.kill_confirm.as_deref(),
            Some("ZAP-9"),
            "kill arms on the live agent"
        );
        assert!(
            !a.status_msg
                .as_deref()
                .unwrap_or("")
                .contains("not running"),
            "it must not mis-answer 'not running': {:?}",
            a.status_msg
        );
    }

    #[test]
    fn switching_projects_repoints_the_ledger_to_the_target_file() {
        // H3: the ledger must follow the active project, not stay pinned to the boot
        // project's file (which would silently collect every project's run history).
        let mut a = app();
        let layout = crate::registry::Layout::new("/tmp/lindep-ledger-repoint-test");
        a.layout = Some(layout.clone());
        a.active_project = "proj-a".into();
        a.project_handles.insert("proj-a".into(), "handle-a".into());
        a.project_handles.insert("proj-b".into(), "handle-b".into());
        a.ledger_path = Some(layout.ledger_path("handle-a"));
        a.activate_project(project("proj-b", "Beta"), demo::graph());
        assert_eq!(
            a.ledger_path,
            Some(layout.ledger_path("handle-b")),
            "the ledger re-points to the target project's own file"
        );
    }

    #[test]
    fn switching_into_a_newer_format_ledger_never_clobbers_it() {
        // NEW-24: the version guard is protected at BOOT but was unprotected on SWITCH —
        // switching into a project whose ledger.json is a future format, then running an
        // agent, would overwrite it with an empty downgraded v1 file. The fix drops the
        // project's handle when its ledger fails the version load, so the per-project
        // `save_ledgers` (handle-gated) skips it forever after. This guards that no
        // subsequent lifecycle event downgrades the newer file.
        let dir = "/tmp/lindep-ledger-new24-clobber-test";
        let _ = std::fs::remove_dir_all(dir);
        let layout = crate::registry::Layout::new(dir);
        let newer = layout.ledger_path("handle-b");
        std::fs::create_dir_all(newer.parent().unwrap()).unwrap();
        let original = br#"{"version":999,"issues":[]}"#;
        std::fs::write(&newer, original).unwrap();

        let mut a = app();
        a.layout = Some(layout.clone());
        a.active_project = "proj-a".into();
        a.project_handles.insert("proj-a".into(), "handle-a".into());
        a.project_handles.insert("proj-b".into(), "handle-b".into());
        a.ledger_path = Some(layout.ledger_path("handle-a"));
        a.ledger_dirty = false; // a clean switch — no outgoing slice to flush first

        a.activate_project(project("proj-b", "Beta"), demo::graph());
        assert!(
            !a.project_handles.contains_key("proj-b"),
            "the newer-format project is de-handled so save_ledgers can't write it"
        );

        // The next agent run records a proj-b episode and flips the dirty flag…
        a.ledger.begin("proj-b", "ENG-1", "sid".into(), 100);
        a.ledger_dirty = true;
        a.save_ledgers();

        // …but proj-b's file is untouched: still the future format, not a downgraded v1.
        assert!(
            matches!(
                crate::ledger::Ledger::load(&newer),
                Err(crate::session::StateError::Version { found: 999, .. })
            ),
            "the newer-format ledger.json is preserved, not clobbered to v1"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_kept_worktree_discard_flags_it_and_does_not_drop_the_issue() {
        // D-HIGH: a rejected-push discard keeps the worktree — the cockpit must flag it,
        // not silently drop the fleet entry (which would strand the unpushed work).
        let mut a = app();
        a.fleet.insert("ZAP-9".into(), AgentStatus::Done);
        a.apply_event(AppEvent::DiscardKeptWorktree {
            project_id: String::new(),
            issue: "ZAP-9".into(),
            reason: "unpushed work kept".into(),
        });
        assert!(
            a.kept_worktrees.contains("ZAP-9"),
            "the kept worktree is flagged"
        );
        assert!(
            a.fleet.contains_key("ZAP-9"),
            "the issue is NOT dropped — still re-discardable after pushing"
        );
    }

    #[test]
    fn a_clean_discard_drops_the_fleet_entry_and_clears_the_flag() {
        let mut a = app();
        a.fleet.insert("ZAP-9".into(), AgentStatus::Done);
        a.kept_worktrees.insert("ZAP-9".into()); // a prior kept worktree, now removed
        a.apply_event(AppEvent::WorkspaceDiscarded {
            project_id: String::new(),
            issue: "ZAP-9".into(),
        });
        assert!(
            !a.fleet.contains_key("ZAP-9"),
            "a confirmed removal drops the fleet entry"
        );
        assert!(
            !a.kept_worktrees.contains("ZAP-9"),
            "and clears any kept-worktree flag"
        );
        assert!(
            a.reaped.contains("ZAP-9"),
            "and tombstones late hooks for the discarded issue"
        );
    }

    /// Whether `key` (an `issue` or `issue/repo`) is flagged unpushed in ANY project.
    fn is_unpushed(a: &App, key: &str) -> bool {
        a.unpushed.values().any(|s| s.contains(key))
    }

    fn commit_event(project_id: &str, issue: &str, outcome: PushOutcome) -> AppEvent {
        AppEvent::AgentCommitted {
            project_id: project_id.into(),
            issue: issue.into(),
            repo_handle: "api".into(),
            branch: "feat".into(),
            outcome,
        }
    }

    #[test]
    fn a_rejected_auto_push_raises_the_unpushed_chip_and_never_reports_pushed() {
        // HIGH data-integrity: the reject and a blanket "pushed" used to drain in one
        // render tick with "pushed" winning, so a stranded commit read as a clean
        // success. The outcome now rides the single event: a reject footers the
        // failure AND raises a standing chip that no later overwrite can hide.
        let mut a = app();
        a.apply_event(commit_event(
            "",
            "ZAP-9",
            PushOutcome::Rejected("remote rejected (non-fast-forward)".into()),
        ));
        assert!(
            is_unpushed(&a, "ZAP-9/api"),
            "a rejected push raises the standing unpushed chip"
        );
        let footer = a.status_msg.clone().unwrap_or_default();
        assert!(
            footer.contains("auto-push failed") && footer.contains("non-fast-forward"),
            "the footer reports the failure, not a clean push: {footer:?}"
        );
        assert!(
            !footer.contains("pushed feat"),
            "a rejected push never reports as pushed: {footer:?}"
        );
    }

    #[test]
    fn a_later_clean_push_clears_the_unpushed_chip_for_that_key() {
        let mut a = app();
        a.apply_event(commit_event("", "ZAP-9", PushOutcome::Rejected("x".into())));
        assert!(is_unpushed(&a, "ZAP-9/api"));
        a.apply_event(commit_event("", "ZAP-9", PushOutcome::Pushed));
        assert!(
            !is_unpushed(&a, "ZAP-9/api"),
            "a clean push of the same key clears its stranded-commit chip"
        );
        assert_eq!(a.status_msg.as_deref(), Some("ZAP-9/api: pushed feat"));
    }

    #[test]
    fn a_clean_push_of_one_repo_leaves_another_repos_strand_intact() {
        // The masking must not return through over-broad clearing: a Pushed for one
        // key must clear ONLY that key, never a sibling repo's standing strand.
        let mut a = app();
        a.apply_event(commit_event("", "ZAP-9", PushOutcome::Rejected("x".into())));
        a.apply_event(AppEvent::AgentCommitted {
            project_id: String::new(),
            issue: "ZAP-9".into(),
            repo_handle: "web".into(),
            branch: "feat".into(),
            outcome: PushOutcome::Pushed,
        });
        assert!(
            is_unpushed(&a, "ZAP-9/api"),
            "ZAP-9/api stays stranded after a clean push of ZAP-9/web"
        );
        assert_eq!(a.unpushed_count(), 1);
    }

    #[test]
    fn a_local_only_commit_reads_as_committed_not_pushed_and_raises_no_chip() {
        let mut a = app();
        a.apply_event(commit_event("", "ZAP-9", PushOutcome::LocalOnly));
        assert_eq!(a.unpushed_count(), 0, "local-only is a clean state, no chip");
        assert_eq!(
            a.status_msg.as_deref(),
            Some("ZAP-9/api: committed feat (local-only)"),
            "a local-only commit is reported as committed, never pushed"
        );
    }

    #[test]
    fn the_unpushed_chip_survives_a_needs_you_alert_even_when_the_footer_defers() {
        // The footer must not bury a needs-you alert, but the chip is the durable
        // surface and must be raised regardless — else the failure would be invisible
        // while an unrelated alert is up.
        let mut a = app();
        a.needs_you_alert = true;
        a.status_msg = Some("2 agents need you".into());
        a.apply_event(commit_event(
            "",
            "ZAP-9",
            PushOutcome::Rejected("auth failed".into()),
        ));
        assert!(
            is_unpushed(&a, "ZAP-9/api"),
            "the chip is raised even under a needs-you alert"
        );
        assert_eq!(
            a.status_msg.as_deref(),
            Some("2 agents need you"),
            "the transient footer still defers to the standing alert"
        );
    }

    #[test]
    fn a_rejected_push_in_a_backgrounded_project_still_raises_the_chip_and_toasts() {
        // HIGH regression guard: the scope guard drops a backgrounded project's
        // AgentCommitted, so the chip mutation must run AHEAD of it (cross-project) —
        // else a stranded commit in another project is invisible (the masking, moved).
        let mut a = app();
        a.active_project = "proj-a".into();
        a.apply_event(commit_event(
            "proj-b",
            "ZAP-9",
            PushOutcome::Rejected("non-fast-forward".into()),
        ));
        assert!(
            is_unpushed(&a, "ZAP-9/api"),
            "a backgrounded project's strand is still flagged"
        );
        assert_eq!(a.unpushed_count(), 1);
        let footer = a.status_msg.clone().unwrap_or_default();
        assert!(
            footer.contains("auto-push failed") && footer.contains("ZAP-9"),
            "a backgrounded strand toasts once: {footer:?}"
        );
    }

    #[test]
    fn a_late_rejected_push_does_not_resurrect_a_reaped_issues_chip() {
        // A discarded/killed issue's in-flight push that lands late must not re-raise a
        // chip for a torn-down issue (the tombstone discipline siblings enforce).
        let mut a = app();
        a.reaped.insert("ZAP-9".into());
        a.apply_event(commit_event(
            "",
            "ZAP-9",
            PushOutcome::Rejected("late".into()),
        ));
        assert_eq!(
            a.unpushed_count(),
            0,
            "a reaped issue's late reject is ignored, no leaked chip"
        );
    }

    #[test]
    fn a_single_repo_commit_with_no_handle_keys_the_chip_by_bare_issue() {
        let mut a = app();
        a.apply_event(AppEvent::AgentCommitted {
            project_id: String::new(),
            issue: "ZAP-9".into(),
            repo_handle: String::new(),
            branch: String::new(),
            outcome: PushOutcome::Rejected("x".into()),
        });
        assert!(
            is_unpushed(&a, "ZAP-9"),
            "an empty repo handle keys the chip by the bare issue"
        );
        assert_eq!(a.status_msg.as_deref(), Some("ZAP-9: auto-push failed — x"));
    }

    #[test]
    fn discarding_an_issue_clears_its_unpushed_chip_without_touching_a_sibling() {
        let mut a = app();
        a.unpushed.entry(String::new()).or_default().extend([
            "ZAP-9".to_string(),
            "ZAP-9/api".to_string(),
            "ZAP-90/api".to_string(), // a prefix sibling that must survive
        ]);
        a.apply_event(AppEvent::WorkspaceDiscarded {
            project_id: String::new(),
            issue: "ZAP-9".into(),
        });
        assert!(
            !is_unpushed(&a, "ZAP-9") && !is_unpushed(&a, "ZAP-9/api"),
            "discard clears the issue's bare and per-repo chip keys"
        );
        assert!(
            is_unpushed(&a, "ZAP-90/api"),
            "the trailing-slash guard spares the ZAP-90 sibling"
        );
    }

    #[test]
    fn switching_back_drops_a_backend_that_exited_while_backgrounded() {
        let mut a = app();
        a.active_project = "proj-a".into();
        let agent = register(&mut a, "ENG-1");
        a.activate_project(project("proj-b", "Beta"), demo::graph());
        // The agent exits while proj-a is backgrounded (its reap event was filtered).
        agent.finish(Some(0));
        a.activate_project(project("proj-a", "Alpha"), demo::graph());
        assert!(
            !a.backends.contains_key("ENG-1"),
            "a backend whose agent exited while backgrounded is not re-attached"
        );
    }

    #[test]
    fn switching_back_seeds_the_fleet_from_a_live_backend_so_a_discard_is_refused() {
        // CF-5 / H2: on switch the fleet is seeded Running from the restored live backends
        // BEFORE the async status re-emit, so a discard/kill on the very next keystroke
        // sees a live agent (and the destructive-verb guard refuses) instead of an empty
        // fleet for ≥1 iteration — the switch-back data-loss window.
        let mut a = app();
        a.active_project = "proj-a".into();
        let _agent = register(&mut a, "ENG-1"); // a still-live FakeBackend on proj-a
        a.activate_project(project("proj-b", "Beta"), demo::graph()); // stashes proj-a
        a.activate_project(project("proj-a", "Alpha"), demo::graph()); // restores it live
        assert_eq!(
            a.fleet.get("ENG-1"),
            Some(&AgentStatus::Running),
            "the restored live backend seeds a Running fleet entry immediately on switch-back"
        );
        assert!(
            a.has_live_backend("ENG-1"),
            "and the destructive-verb guard sees it as live, so a discard/kill is refused"
        );
    }

    #[test]
    fn the_open_switcher_captures_typing_and_esc_closes_it() {
        let mut a = app();
        a.project_switcher = Some(Picker::new(vec![
            project("1", "Billing"),
            project("2", "Infra"),
        ]));
        // A bare letter filters the overlay — it is NOT routed to a window verb.
        press(&mut a, KeyCode::Char('i'));
        assert_eq!(a.project_switcher.as_ref().unwrap().query, "i");
        // Esc cancels without switching.
        press(&mut a, KeyCode::Esc);
        assert!(a.project_switcher.is_none());
        assert_eq!(a.active_project, "");
    }

    #[test]
    fn the_switcher_is_unavailable_without_the_control_plane() {
        let mut a = app(); // no client/runtime/events wired
        a.open_project_switcher();
        assert!(a.project_switcher.is_none(), "no overlay opens");
        assert!(
            a.status_msg
                .as_deref()
                .unwrap_or("")
                .contains("control plane"),
            "an actionable footer explains why"
        );
    }

    #[test]
    fn a_superseded_switch_result_is_ignored_so_the_last_selection_wins() {
        let mut a = app();
        a.active_project = "proj-a".into();
        a.switch_seq = 2; // the user's latest selection is generation 2
        // A slow fetch for an EARLIER selection (gen 1) lands first → dropped.
        *a.switch_inbox.lock().unwrap() = Some((1, project("proj-b", "B"), demo::graph()));
        a.apply_event(AppEvent::ProjectActivated);
        assert_eq!(
            a.active_project, "proj-a",
            "a superseded (older-generation) switch result is ignored"
        );
        // The latest selection's fetch (gen 2) lands → applied.
        *a.switch_inbox.lock().unwrap() = Some((2, project("proj-c", "C"), demo::graph()));
        a.apply_event(AppEvent::ProjectActivated);
        assert_eq!(
            a.active_project, "proj-c",
            "the most recently selected project wins regardless of fetch order"
        );
    }

    #[test]
    fn a_backend_spawning_while_its_project_is_backgrounded_is_stashed_not_dropped() {
        let mut a = app();
        a.active_project = "proj-a".into();
        // An AgentSpawned for a DIFFERENT, backgrounded project must be stashed —
        // not dropped — or the agent is orphaned (no backend) on a switch back.
        let applied = a.apply_event(AppEvent::AgentSpawned {
            project_id: "proj-b".into(),
            issue: "ENG-9".into(),
            backend: FakeBackend::new("ENG-9") as Arc<dyn AgentBackend>,
            repos: Vec::new(),
});
        assert!(
            !applied,
            "a backgrounded project's event doesn't repaint the active view"
        );
        assert!(
            a.stashed_backends
                .get("proj-b")
                .is_some_and(|m| m.contains_key("ENG-9")),
            "the backgrounded backend is stashed for a later switch-back"
        );
        // Its reap (also filtered) drops it from the stash so it can't be restored.
        a.apply_event(AppEvent::AgentReaped {
            project_id: "proj-b".into(),
            issue: "ENG-9".into(),
        });
        assert!(
            a.stashed_backends
                .get("proj-b")
                .is_none_or(|m| !m.contains_key("ENG-9")),
            "a reaped backgrounded agent is removed from the stash"
        );
    }

    #[test]
    fn a_backgrounded_projects_needs_you_surfaces_as_an_elsewhere_tally_and_clears() {
        // A "needs you" in a project you switched away from must not vanish: it's
        // dropped from the on-screen fleet but tallied for the header/switcher and
        // toasted once — then cleared the moment that agent resumes work.
        let mut a = app();
        a.active_project = "proj-a".into();
        a.project_list = vec![project("proj-b", "Beta")];

        let repaint = a.apply_event(AppEvent::AgentNeedsYou {
            project_id: "proj-b".into(),
            issue: "ENG-9".into(),
            reason: "permission".into(),
        });
        assert!(repaint, "the header 'elsewhere' badge repaints");
        assert_eq!(a.elsewhere_needs_you(), 1);
        assert!(a.projects_needing_you().contains("proj-b"));
        assert!(
            !a.fleet.contains_key("ENG-9"),
            "a backgrounded needs-you never enters the active fleet"
        );
        assert!(
            a.status_msg.as_deref().unwrap_or_default().contains("Beta"),
            "the one-time toast names the project"
        );

        // The agent resumes (a working tool-run) → the elsewhere tally clears.
        a.apply_event(AppEvent::AgentAction {
            project_id: "proj-b".into(),
            issue: "ENG-9".into(),
            action: "ran Edit".into(),
            working: true,
        });
        assert_eq!(a.elsewhere_needs_you(), 0, "resumed work clears the tally");
    }

    #[test]
    fn switching_back_aims_the_cursor_at_a_running_agent() {
        // NEW-26: the "N agents running here — ⏎ to open" reassurance must land Enter
        // on a real running agent. most_connected_root is pure topology and almost
        // never a running issue, so activate_project re-aims onto a live agent it
        // finds in `world` — otherwise Enter would target a dead root and launch a
        // brand-new agent on it (the opposite of "open").
        let mut a = app();
        a.active_project = "proj-a".into();
        let topo_root = most_connected_root(&demo::graph());
        assert_ne!(
            topo_root, "ZAP-210",
            "precondition: the live issue is not already the topology root"
        );
        a.world
            .entry("proj-b".into())
            .or_default()
            .insert("ZAP-210".into(), AgentStatus::Running);
        a.activate_project(project("proj-b", "Beta"), demo::graph());
        assert_eq!(
            a.root, "ZAP-210",
            "the cursor lands on the running agent, not the topology root"
        );
    }

    #[test]
    fn switching_into_a_project_clears_its_elsewhere_tally() {
        let mut a = app();
        a.active_project = "proj-a".into();
        a.apply_event(AppEvent::AgentNeedsYou {
            project_id: "proj-b".into(),
            issue: "ENG-9".into(),
            reason: "permission".into(),
        });
        assert_eq!(a.elsewhere_needs_you(), 1);
        a.activate_project(project("proj-b", "Beta"), demo::graph());
        assert_eq!(
            a.elsewhere_needs_you(),
            0,
            "entering the project moves its needs-you into the on-screen fleet"
        );
    }

    // ── Spawn sizing (point F: no full-width reflow flash) ───────────────────

    #[test]
    fn a_lone_agent_spawns_at_the_full_pane_width() {
        // One non-spine window in a 200×40 viewport: its tile is the 156 cols right
        // of the 44-col Spine, less the 1-cell window border → 154×38 inner.
        assert_eq!(app().agent_spawn_size(), Some((38, 154)));
    }

    #[test]
    fn an_agent_beside_a_pin_spawns_at_the_two_up_tile() {
        // A pinned coin + the live preview tile side by side (mosaic), so a chat
        // opened here must spawn at the 78-col half-tile (inner 76×38), not the
        // full terminal width — otherwise claude paints wide and only reflows once
        // it processes the SIGWINCH (the "doesn't resize beside a pin" flash).
        let mut a = app();
        a.windows.focus_preview();
        a.windows.pin_preview(); // graduate the preview → a permanent coin
        a.windows
            .ensure_preview("ZAP-205", CoinMode::Chat, &a.graph);
        a.aim_spine("ZAP-205".into());
        assert_eq!(a.agent_spawn_size(), Some((38, 76)));
    }

    #[test]
    fn alt_arrows_switch_windows_even_when_a_chat_owns_the_keyboard() {
        let mut a = app();
        // Pin the opening preview into a permanent coin, then focus a fresh chat
        // coin — the band that forwards every key to the agent's PTY.
        a.windows.focus_preview();
        a.windows.pin_preview();
        a.windows
            .ensure_preview("ZAP-205", CoinMode::Chat, &a.graph);
        a.windows.focus_preview();
        assert!(
            matches!(
                a.windows.focused_kind(),
                WindowKind::Coin {
                    mode: CoinMode::Chat,
                    ..
                }
            ),
            "precondition: a chat is focused, so a bare key would go to the PTY"
        );
        let before = a.windows.focus;
        // Alt-← resolves as the global switch ABOVE PTY forwarding — it moves focus
        // rather than being typed into claude.
        a.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        assert_ne!(
            a.windows.focus, before,
            "Alt-arrow switched the window from inside a focused chat"
        );
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
        // focused, which dropped the re-aim for a verb-driven jump.) This is why
        // JumpNeedsYou keeps its prefix binding (ENG-562): a focused pane consults
        // only its own nav keys, so the from-anywhere jump must be a verb.
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
    fn search_ctrl_c_cancels_without_committing_the_query() {
        // NEW-06: Ctrl-C cancels like Esc — it must drop the in-progress needle and
        // restore the full order, never leave the typed query applied as a filter.
        let mut app = app();
        press(&mut app, KeyCode::Char('/'));
        for c in "210".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert_eq!(app.order, vec!["ZAP-210".to_string()]);
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.search_active, "Ctrl-C closes search");
        assert!(app.search_query.is_empty(), "Ctrl-C drops the query");
        assert!(
            app.order.len() > 1,
            "the full order is restored, not left filtered to the typed needle"
        );
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
    fn a_paste_into_a_chat_is_wrapped_so_claude_does_not_submit_each_line() {
        // CF-8: a multi-line paste must reach claude as one bracketed-paste block, not
        // as N newline-terminated submits.
        let mut app = app();
        let fake = register(&mut app, "ZAP-204");
        press(&mut app, KeyCode::Enter); // focus the agent's chat
        assert!(
            app.forward_paste("line one\nline two"),
            "a focused chat consumes the paste"
        );
        assert_eq!(
            fake.inputs.lock().unwrap().last().unwrap(),
            b"\x1b[200~line one\nline two\x1b[201~",
            "the paste is wrapped in bracketed-paste markers, not split on the newline"
        );
    }

    #[test]
    fn a_paste_with_no_chat_focused_is_dropped() {
        // Off a chat (here the Spine), a paste is dropped like a stray keystroke.
        let mut app = app();
        register(&mut app, "ZAP-204");
        assert!(!app.forward_paste("hello"), "a paste off a chat is dropped");
    }

    #[test]
    fn typing_into_a_reclaimed_chat_says_so_instead_of_swallowing_it() {
        // D-MED: a dead chat whose backend was reclaimed used to swallow keystrokes
        // silently (a frozen prompt); now it says it's not accepting input.
        let mut app = app();
        register(&mut app, "ZAP-204");
        press(&mut app, KeyCode::Enter); // focus the chat
        app.backends.remove("ZAP-204"); // its backend was reclaimed
        app.forward_to_agent(
            "ZAP-204",
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        );
        assert!(
            app.status_msg
                .as_deref()
                .unwrap_or("")
                .contains("no longer accepting input"),
            "a reclaimed chat says so: {:?}",
            app.status_msg
        );
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
        // …and the issue you were looking at stays on screen: close demotes the
        // selected issue's coin back to the follower preview (never an empty pane).
        assert_eq!(
            app.windows.preview().map(|(issue, _)| issue),
            Some("ZAP-204".to_string()),
            "unpinning the selected issue re-seeds its preview"
        );
    }

    #[test]
    fn unpinning_the_selected_issues_coin_demotes_it_to_a_preview() {
        // The thing you're looking at must never vanish (Felix / B0b): unpinning the
        // pinned coin of the *currently selected* issue demotes it back to the
        // follower preview, not an empty big pane.
        let mut app = app();
        register(&mut app, "ZAP-204");
        press(&mut app, KeyCode::Enter); // active window → chat coin on the selection
        verb(&mut app, KeyCode::Char('p')); // pin = graduate to a permanent tab
        assert!(
            app.windows.pinned_coin_index("ZAP-204").is_some(),
            "precondition: the selected issue has a pinned coin"
        );
        verb(&mut app, KeyCode::Char('w')); // Ctrl-a w = unpin / close
        assert!(
            app.windows.pinned_coin_index("ZAP-204").is_none(),
            "the coin is undocked"
        );
        assert_eq!(
            app.windows.preview().map(|(issue, _)| issue),
            Some("ZAP-204".to_string()),
            "the issue you were looking at is demoted to the preview, not gone"
        );
        assert!(
            app.active_index().is_some(),
            "something represents the selection, so the big pane isn't empty"
        );
    }

    #[test]
    fn direct_p_on_the_spine_toggles_pin_then_unpin() {
        // R6 + Felix: a bare `p` from the nav is a *toggle*. The first press pins
        // the previewed selection; a second press on the now-pinned selection
        // unpins it (demoted back to a follower preview so it never vanishes), and
        // either way focus stays on the Spine (0) so you keep browsing. This drives
        // the DIRECT `p` key (press), not the `Ctrl-a p` prefix (verb).
        let mut app = app();
        register(&mut app, "ZAP-204");
        assert_eq!(app.windows.focus, 0, "precondition: focused on the Spine");
        // First direct `p`: pin the previewed selection (no Tab/prefix needed).
        press(&mut app, KeyCode::Char('p'));
        assert!(
            app.windows.pinned_coin_index("ZAP-204").is_some(),
            "direct p from the nav pins the previewed issue"
        );
        assert_eq!(app.windows.focus, 0, "pin from the nav keeps you on the Spine");
        // Second direct `p` on the same, now-pinned selection: toggle → unpin.
        press(&mut app, KeyCode::Char('p'));
        assert!(
            app.windows.pinned_coin_index("ZAP-204").is_none(),
            "a second direct p unpins (undocks) the issue"
        );
        assert_eq!(
            app.windows.preview().map(|(issue, _)| issue),
            Some("ZAP-204".to_string()),
            "the unpinned issue is demoted to the preview, not gone"
        );
        assert_eq!(app.windows.focus, 0, "unpin from the nav keeps you on the Spine");
    }

    #[test]
    fn apply_event_records_a_run_in_the_ledger_for_any_project() {
        // The ledger is fed from lifecycle events for EVERY project — even one you
        // switched away from — recorded before the scoping guard. A spawn opens a
        // run, a needs-you bumps its prompt count, a terminal status closes it.
        let mut app = app();
        app.active_project = "proj-a".into();
        // A backgrounded project's agent: its events are dropped from the fleet…
        let spawn = AppEvent::AgentSpawned {
            project_id: "proj-b".into(),
            issue: "ENG-9".into(),
            backend: FakeBackend::new("ENG-9") as Arc<dyn AgentBackend>,
            repos: Vec::new(),
};
        app.apply_event(spawn);
        app.apply_event(AppEvent::AgentNeedsYou {
            project_id: "proj-b".into(),
            issue: "ENG-9".into(),
            reason: "permission".into(),
        });
        app.apply_event(AppEvent::AgentStatusChanged {
            project_id: "proj-b".into(),
            issue: "ENG-9".into(),
            status: AgentStatus::Done,
        });

        // …but the run is still in the ledger, fully closed.
        let eps = app.ledger.episodes("proj-b", "ENG-9");
        assert_eq!(
            eps.len(),
            1,
            "the run was recorded for the backgrounded project"
        );
        assert_eq!(eps[0].outcome, Some(AgentStatus::Done));
        assert_eq!(eps[0].needs_you, 1);
        assert!(!eps[0].is_open(), "a terminal status closed the run");
        assert!(
            app.ledger_dirty,
            "a recorded run asks the render thread to persist"
        );
    }

    #[test]
    fn kill_undocks_a_pinned_agent_window() {
        // Bug: killing an agent left its pinned coin on screen as a dead card.
        // Kill must now undock the issue's window (pinned included) — `undock_issue`
        // is the window half of the confirmed-kill path.
        let mut app = app();
        register(&mut app, "ZAP-204");
        app.fleet.insert("ZAP-204".into(), AgentStatus::Running);
        press(&mut app, KeyCode::Enter); // active window → chat on ZAP-204
        verb(&mut app, KeyCode::Char('p')); // pin = graduate to a permanent tab
        assert!(
            app.windows.pinned_coin_index("ZAP-204").is_some(),
            "precondition: the agent has a pinned coin"
        );

        let closed_pinned = app.undock_issue("ZAP-204");
        assert!(closed_pinned, "a docked window was closed");
        assert!(
            app.windows.pinned_coin_index("ZAP-204").is_none(),
            "killing the agent undocked its pinned window"
        );
        assert!(
            app.cockpit_dirty,
            "closing a docked window re-persists the layout"
        );
        assert!(
            app.windows.preview_index().is_some(),
            "a fresh preview is re-aimed so the strip isn't left previewless"
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
        // The preview follows the browsed row while the pinned ZAP-204 coin survives.
        verb(&mut app, KeyCode::Enter);
        assert!(app.windows.pinned_coin_index("ZAP-204").is_some());
        assert_eq!(
            app.windows.preview().unwrap(),
            ("ZAP-205".into(), CoinMode::Deps)
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
        assert!(!app.quit_confirm);
    }

    #[test]
    fn an_idle_cockpit_quits_immediately_but_a_live_fleet_asks_first() {
        // R7: no friction when nothing is at stake — Ctrl-a q on an agent-less
        // cockpit quits straight away. With a live agent it arms a confirmation
        // (Q sits right above A, so a fat-fingered prefix-then-q shouldn't tear
        // down a running fleet); y/⏎ confirms.
        let mut idle = app();
        verb(&mut idle, KeyCode::Char('q'));
        assert!(idle.should_quit, "an idle cockpit quits without a prompt");
        assert!(!idle.quit_confirm);

        let mut busy = app();
        register(&mut busy, "ZAP-204");
        busy.fleet.insert("ZAP-204".into(), AgentStatus::Running);
        verb(&mut busy, KeyCode::Char('q'));
        assert!(busy.quit_confirm, "a live fleet arms a confirmation");
        assert!(!busy.should_quit, "the first Ctrl-a q does not quit");
        press(&mut busy, KeyCode::Char('y'));
        assert!(busy.should_quit, "y confirms the quit");
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
    fn entering_a_dep_in_the_preview_moves_the_nav_selection() {
        let mut app = app();
        verb(&mut app, KeyCode::Char('l')); // focus the deps preview (roots at ZAP-204)
        assert_eq!(app.root, "ZAP-204");
        let target = app.windows.focused().deps.as_ref().unwrap().up_rows[0]
            .key
            .clone();
        press(&mut app, KeyCode::Enter); // dive into the first blocker
        assert_eq!(
            app.root, target,
            "the nav selection follows the entered dep"
        );
    }

    #[test]
    fn the_detail_bar_follows_the_focused_deps_cursor() {
        let mut app = app();
        verb(&mut app, KeyCode::Char('l')); // focus the deps preview
        let target = app.windows.focused().deps.as_ref().unwrap().up_rows[0]
            .key
            .clone();
        press(&mut app, KeyCode::Enter); // re-root the cursor onto the blocker
        assert_eq!(app.detail_key(), Some(target.as_str()));
    }

    #[test]
    fn a_pinned_deps_coin_explores_without_moving_the_nav() {
        let mut app = app();
        verb(&mut app, KeyCode::Char('l')); // focus the preview deps
        verb(&mut app, KeyCode::Char('p')); // pin it → an independent coin, still focused
        let before = app.root.clone();
        let target = app.windows.focused().deps.as_ref().unwrap().up_rows[0]
            .key
            .clone();
        assert_ne!(target, before, "the blocker differs from the selection");
        press(&mut app, KeyCode::Enter); // re-root the pinned coin's tree
        assert_eq!(
            app.windows.focused().deps.as_ref().unwrap().root,
            target,
            "the pinned coin re-roots in place"
        );
        assert_eq!(
            app.root, before,
            "but the Spine stays put — pinned coins are independent"
        );
    }

    #[test]
    fn verbs_target_the_re_rooted_deps_node_the_pane_shows_not_the_stale_selection() {
        // H6: in a pinned deps coin re-rooted onto a blocker, the shared on-screen
        // target (detail bar / dispatch / kill / editor) must follow the cursor root,
        // not the Spine selection three screens back.
        let mut app = app();
        verb(&mut app, KeyCode::Char('l')); // focus the preview deps
        verb(&mut app, KeyCode::Char('p')); // pin it
        let before = app.root.clone();
        let target = app.windows.focused().deps.as_ref().unwrap().up_rows[0]
            .key
            .clone();
        assert_ne!(target, before);
        press(&mut app, KeyCode::Enter); // re-root onto the blocker
        assert_eq!(app.windows.focused().deps.as_ref().unwrap().root, target);
        assert_eq!(
            app.detail_key(),
            Some(target.as_str()),
            "the detail/dispatch/kill target follows the pane, not the stale selection"
        );
        assert_eq!(app.root, before, "the Spine itself stays put");
    }

    #[test]
    fn tab_on_the_spine_flips_the_selections_pinned_coin() {
        // Tab was a dead key when the selection had a pinned coin (its preview is
        // cleared) — now it flips the pinned coin that IS the big pane.
        let mut app = app();
        register(&mut app, "ZAP-204");
        press(&mut app, KeyCode::Enter); // chat coin on ZAP-204
        verb(&mut app, KeyCode::Char('p')); // pin it
        verb(&mut app, KeyCode::Char('0')); // FocusNav home to the Spine
        assert_eq!(app.windows.focus, 0, "back on the Spine");
        let pinned = app.windows.pinned_coin_index("ZAP-204").unwrap();
        assert!(
            matches!(
                app.windows.windows[pinned].kind,
                WindowKind::Coin {
                    mode: CoinMode::Chat,
                    ..
                }
            ),
            "the pinned coin starts on its chat face"
        );
        press(&mut app, KeyCode::Tab); // ContextToggle on the Spine
        assert!(
            matches!(
                app.windows.windows[pinned].kind,
                WindowKind::Coin {
                    mode: CoinMode::Deps,
                    ..
                }
            ),
            "Tab flipped the pinned coin to deps instead of doing nothing"
        );
    }

    #[test]
    fn d_on_a_pinned_issue_flips_it_to_deps_without_duplicating() {
        let mut app = app();
        register(&mut app, "ZAP-204");
        press(&mut app, KeyCode::Enter); // chat coin
        verb(&mut app, KeyCode::Char('p')); // pin it
        verb(&mut app, KeyCode::Char('0')); // home to the Spine; selection still ZAP-204
        let n = app.windows.windows.len();
        press(&mut app, KeyCode::Char('d')); // OpenDeps on the pinned selection
        assert_eq!(
            app.windows.windows.len(),
            n,
            "no duplicate coin minted for ZAP-204"
        );
        assert!(
            matches!(
                app.windows.focused_kind(),
                WindowKind::Coin {
                    mode: CoinMode::Deps,
                    ..
                }
            ),
            "the existing pinned coin is flipped to its deps face and focused"
        );
    }

    #[test]
    fn the_one_shot_prefix_fires_a_single_verb() {
        // `Ctrl-a l` fires exactly one window verb (focus right). v1.7 (ENG-562)
        // removed command mode, so the prefix is now purely one-shot.
        let mut app = app();
        let before = app.windows.focus;
        verb(&mut app, KeyCode::Char('l'));
        assert!(app.windows.focus > before, "the one-shot verb fires");
    }

    #[test]
    fn kill_targets_the_selected_agent() {
        let mut app = app();
        app.fleet.insert("ZAP-210".into(), AgentStatus::Running);
        // The roster fold (ENG-563) removed the AGENTS tab; select the agent's
        // issue directly on the nav, then Ctrl-a x arms a kill of that agent.
        app.aim_spine("ZAP-210".into());
        app.windows.focus = 0; // the nav (what flipping to the roster used to do)
        verb(&mut app, KeyCode::Char('x'));
        assert_eq!(app.kill_confirm.as_deref(), Some("ZAP-210"));
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
    fn workspace_summary_counts_only_live_agents() {
        let mut app = app();
        app.fleet.insert("ZAP-201".into(), AgentStatus::Running);
        app.fleet.insert("ZAP-204".into(), AgentStatus::Idle);
        app.fleet.insert("ZAP-205".into(), AgentStatus::Done);
        app.fleet.insert("ZAP-210".into(), AgentStatus::Failed);
        app.fleet.insert("ZAP-240".into(), AgentStatus::NeedsYou);
        assert_eq!(app.workspace_summary(), (3, 1));
    }

    #[test]
    fn a_nav_keypress_keeps_the_sticky_needs_you_guard_armed() {
        // CF-11: a stray nav key must NOT drop the sticky needs-you guard while an agent
        // still needs you — it's the one alert designed to be un-buryable. It clears only
        // once the agent actually resolves/exits (re-derived from the fleet).
        let mut app = app();
        app.fleet.insert("ZAP-204".into(), AgentStatus::NeedsYou);
        app.needs_you_alert = true;
        press(&mut app, KeyCode::Char('f')); // a spine key no longer disarms it
        assert!(
            app.needs_you_alert,
            "the guard survives navigation while the agent still needs you"
        );
        // Resolve it (NeedsYou → Running); now the next nav key clears the guard.
        app.fleet.insert("ZAP-204".into(), AgentStatus::Running);
        press(&mut app, KeyCode::Char('k'));
        assert!(
            !app.needs_you_alert,
            "once the agent resolves, nav clears the guard"
        );
    }

    #[test]
    fn the_needs_you_jump_leaves_the_guard_armed() {
        // Pressing `n` to triage must not disarm the burial guard mid-triage.
        let mut app = app();
        app.fleet.insert("ZAP-204".into(), AgentStatus::NeedsYou);
        app.needs_you_alert = true;
        press(&mut app, KeyCode::Char('n')); // JumpNeedsYou
        assert!(
            app.needs_you_alert,
            "the dedicated triage key keeps the guard armed"
        );
    }

    #[test]
    fn restart_refuses_a_live_agent_then_relaunches_a_dead_one() {
        // CF-14: one-press restart — refuse a live agent (never silently kill running
        // work), reclaim + relaunch a dead one.
        let mut a = app();
        a.workspace = Some(crate::workspace::WorkspaceHandle::detached());
        a.active_project = "proj".into();
        let fake = register(&mut a, "ZAP-204");
        a.aim_spine("ZAP-204".into());
        a.windows.focus = 0;
        a.fleet.insert("ZAP-204".into(), AgentStatus::Running);
        verb(&mut a, KeyCode::Char('r')); // Ctrl-a r
        assert!(
            a.status_msg.as_deref().unwrap_or("").contains("still live"),
            "restart refuses a live agent: {:?}",
            a.status_msg
        );
        assert!(
            !a.pending_launch.contains_key("ZAP-204"),
            "no relaunch while live"
        );
        // The agent dies; restart reclaims its (windowed) dead backend so button
        // relaunches it — instead of just re-opening the corpse card (the reclaim is
        // restart's distinctive move; the relaunch then follows button's normal gates).
        fake.finish(Some(1));
        a.fleet.insert("ZAP-204".into(), AgentStatus::Failed);
        assert!(
            a.backends.contains_key("ZAP-204"),
            "precondition: the dead backend is still up"
        );
        verb(&mut a, KeyCode::Char('r'));
        assert!(
            !a.backends.contains_key("ZAP-204"),
            "restart reclaims the dead backend in one press, so it relaunches not re-opens"
        );
    }

    #[test]
    fn next_agent_walks_the_live_fleet_wrapping() {
        let mut a = app();
        register(&mut a, "ZAP-201");
        register(&mut a, "ZAP-205");
        a.fleet.insert("ZAP-201".into(), AgentStatus::Running);
        a.fleet.insert("ZAP-205".into(), AgentStatus::Idle);
        a.windows.focus = 0;
        a.aim_spine("ZAP-201".into());
        verb(&mut a, KeyCode::Char('j')); // next-agent
        assert_eq!(a.root, "ZAP-205", "walks to the next live agent");
        verb(&mut a, KeyCode::Char('j'));
        assert_eq!(a.root, "ZAP-201", "wraps back to the first");
    }

    #[test]
    fn dispatch_ready_launches_the_ready_lane_in_one_press() {
        let mut a = app();
        a.workspace = Some(crate::workspace::WorkspaceHandle::detached());
        a.active_project = "proj".into();
        a.resume_cap = 0; // uncapped → launch every READY issue
        let ready: Vec<String> = a
            .order
            .iter()
            .filter(|k| a.readiness(k) == Readiness::Ready)
            .cloned()
            .collect();
        assert!(
            !ready.is_empty(),
            "precondition: the graph has READY issues"
        );
        verb(&mut a, KeyCode::Char('g')); // dispatch-ready
        let launched = ready
            .iter()
            .filter(|k| a.pending_launch.contains_key(*k))
            .count();
        assert_eq!(
            launched,
            ready.len(),
            "every READY issue launched in one press"
        );
    }

    #[test]
    fn a_clean_finish_increments_the_shipped_tally_but_a_crash_does_not() {
        let mut a = app();
        a.active_project = "proj".into();
        a.fleet.insert("ZAP-204".into(), AgentStatus::Running);
        a.apply_event(AppEvent::AgentStatusChanged {
            project_id: "proj".into(),
            issue: "ZAP-204".into(),
            status: AgentStatus::Done,
        });
        assert_eq!(a.shipped_today, 1, "a clean finish ships");
        // A re-emit of the same terminal status must not double-count.
        a.apply_event(AppEvent::AgentStatusChanged {
            project_id: "proj".into(),
            issue: "ZAP-204".into(),
            status: AgentStatus::Done,
        });
        assert_eq!(a.shipped_today, 1, "a re-emit doesn't double-count");
        // A crash isn't a ship.
        a.fleet.insert("ZAP-205".into(), AgentStatus::Running);
        a.apply_event(AppEvent::AgentStatusChanged {
            project_id: "proj".into(),
            issue: "ZAP-205".into(),
            status: AgentStatus::Failed,
        });
        assert_eq!(a.shipped_today, 1, "a crash doesn't ship");
    }

    #[test]
    fn a_working_action_resolves_a_pending_needs_you() {
        // The bug: after a permission prompt set NeedsYou and the user answered,
        // the agent ran tools (working actions) but the flag never cleared until
        // the next Stop. A working action must promote NeedsYou → Running and drop
        // the sticky alert — that's "the agent resumed, you're no longer needed".
        let mut app = app();
        app.apply_event(AppEvent::AgentNeedsYou {
            project_id: String::new(),
            issue: "ZAP-204".into(),
            reason: "permission".into(),
        });
        assert!(app.needs_you_alert);
        app.apply_event(AppEvent::AgentAction {
            project_id: String::new(),
            issue: "ZAP-204".into(),
            action: "ran Bash".into(),
            working: true,
        });
        assert_eq!(
            app.fleet.get("ZAP-204"),
            Some(&AgentStatus::Running),
            "a working tool-run after the user answered clears NeedsYou → Running"
        );
        assert!(!app.needs_you_alert, "the resolved agent drops its alert");
        assert_eq!(app.status_msg.as_deref(), Some("ZAP-204: ran Bash"));
    }

    #[test]
    fn an_ambient_action_does_not_clear_a_pending_needs_you() {
        // The other half: a NON-working action (the ~60 s idle nudge) must NOT
        // promote a needs-you agent or bury its alert — only genuine work resolves
        // a prompt, so routine chatter can't silence one.
        let mut app = app();
        app.apply_event(AppEvent::AgentNeedsYou {
            project_id: String::new(),
            issue: "ZAP-204".into(),
            reason: "permission".into(),
        });
        let alert = app.status_msg.clone();
        app.apply_event(AppEvent::AgentAction {
            project_id: String::new(),
            issue: "ZAP-204".into(),
            action: "agent idle".into(),
            working: false,
        });
        assert_eq!(app.fleet.get("ZAP-204"), Some(&AgentStatus::NeedsYou));
        assert_eq!(
            app.status_msg, alert,
            "ambient chatter must not bury the alert"
        );
    }

    #[test]
    fn a_settled_idle_agent_is_not_revived_by_mid_turn_or_ambient_chatter() {
        // A4: once an agent goes Idle (its Stop hook fired), a late or out-of-order
        // mid-turn PostToolUse — or the ~60 s idle nudge — must NOT flip it back to
        // WORKING. Only a genuine new turn (a Running status change, which is how
        // UserPromptSubmit is routed) revives it.
        let mut app = app();
        app.fleet.insert("ZAP-204".into(), AgentStatus::Idle);

        // A mid-turn working tool-run lands late.
        app.apply_event(AppEvent::AgentAction {
            project_id: String::new(),
            issue: "ZAP-204".into(),
            action: "ran Bash".into(),
            working: true,
        });
        assert_eq!(
            app.fleet.get("ZAP-204"),
            Some(&AgentStatus::Idle),
            "a mid-turn PostToolUse must not un-idle a settled agent"
        );

        // The ~60 s ambient idle nudge.
        app.apply_event(AppEvent::AgentAction {
            project_id: String::new(),
            issue: "ZAP-204".into(),
            action: "agent idle".into(),
            working: false,
        });
        assert_eq!(
            app.fleet.get("ZAP-204"),
            Some(&AgentStatus::Idle),
            "the idle nudge must not un-idle a settled agent"
        );

        // A genuine new turn revives it.
        app.apply_event(AppEvent::AgentStatusChanged {
            project_id: String::new(),
            issue: "ZAP-204".into(),
            status: AgentStatus::Running,
        });
        assert_eq!(
            app.fleet.get("ZAP-204"),
            Some(&AgentStatus::Running),
            "a new turn revives an Idle agent to Running"
        );
    }

    #[test]
    fn the_world_roll_up_mirrors_the_idle_fix() {
        // The cross-project roll-up (`world`) must apply the same A4 rule as `fleet`, or
        // a switched-away project would report a phantom *live* agent that is really Idle.
        let mut app = app();
        app.active_project = "proj".into();
        // Idle via a real status change populates both fleet and world.
        app.apply_event(AppEvent::AgentStatusChanged {
            project_id: "proj".into(),
            issue: "ENG-1".into(),
            status: AgentStatus::Idle,
        });
        // A mid-turn working action must not un-idle it in EITHER map.
        app.apply_event(AppEvent::AgentAction {
            project_id: "proj".into(),
            issue: "ENG-1".into(),
            action: "ran Bash".into(),
            working: true,
        });
        assert_eq!(app.fleet.get("ENG-1"), Some(&AgentStatus::Idle));
        assert_eq!(
            app.world.get("proj").and_then(|m| m.get("ENG-1")),
            Some(&AgentStatus::Idle),
            "world must not un-idle a settled agent on mid-turn chatter"
        );
        // An action with no prior entry creates nothing in world (no phantom agent).
        app.apply_event(AppEvent::AgentAction {
            project_id: "proj".into(),
            issue: "GHOST".into(),
            action: "ran Bash".into(),
            working: true,
        });
        assert!(
            app.world.get("proj").and_then(|m| m.get("GHOST")).is_none(),
            "a working action before AgentSpawned must not conjure a world entry"
        );
    }

    #[test]
    fn a_late_hook_cannot_resurrect_a_terminated_agent() {
        let mut app = app();
        app.fleet.insert("ZAP-204".into(), AgentStatus::Done);
        assert!(!app.apply_event(AppEvent::AgentAction {
            project_id: String::new(),
            issue: "ZAP-204".into(),
            action: "ran grep".into(),
            working: true,
        }));
        assert!(!app.apply_event(AppEvent::AgentNeedsYou {
            project_id: String::new(),
            issue: "ZAP-204".into(),
            reason: "late prompt".into(),
        }));
        assert!(!app.apply_event(AppEvent::AgentStatusChanged {
            project_id: String::new(),
            issue: "ZAP-204".into(),
            status: AgentStatus::Idle,
        }));
        assert_eq!(app.fleet.get("ZAP-204"), Some(&AgentStatus::Done));
        assert_eq!(app.workspace_summary(), (0, 0));
    }

    #[test]
    fn agent_spawned_records_repo_set_and_reap_clears_it() {
        let mut app = app();
        let fake = FakeBackend::new("ENG-7");
        app.apply_event(AppEvent::AgentSpawned {
            project_id: String::new(),
            issue: "ENG-7".into(),
            backend: fake as Arc<dyn AgentBackend>,
            repos: vec!["pulse".into(), "cortex".into(), "lindep".into()],
        });
        assert_eq!(
            app.agent_repos.get("ENG-7").map(Vec::as_slice),
            Some(["pulse".to_string(), "cortex".to_string(), "lindep".to_string()].as_slice()),
            "the multi-repo set the supervisor reported is recorded so the header can name it"
        );
        // Reaping the agent tears down its worktrees — the repo set goes with it so no
        // stale badge can outlive the agent that owned it.
        app.apply_event(AppEvent::AgentReaped {
            project_id: String::new(),
            issue: "ENG-7".into(),
        });
        assert!(
            !app.agent_repos.contains_key("ENG-7"),
            "reap drops the repo set"
        );
    }

    #[test]
    fn a_late_hook_cannot_resurrect_a_reaped_agent() {
        let mut app = app();
        let fake = FakeBackend::new("ZAP-204");
        app.apply_event(AppEvent::AgentSpawned {
            project_id: String::new(),
            issue: "ZAP-204".into(),
            backend: fake as Arc<dyn AgentBackend>,
            repos: Vec::new(),
        });
        app.apply_event(AppEvent::AgentStatusChanged {
            project_id: String::new(),
            issue: "ZAP-204".into(),
            status: AgentStatus::Done,
        });
        app.apply_event(AppEvent::AgentReaped {
            project_id: String::new(),
            issue: "ZAP-204".into(),
        });
        assert!(
            !app.fleet.contains_key("ZAP-204"),
            "the reaped agent is gone"
        );
        // All three late hooks are ignored.
        assert!(!app.apply_event(AppEvent::AgentNeedsYou {
            project_id: String::new(),
            issue: "ZAP-204".into(),
            reason: "late".into(),
        }));
        assert!(!app.apply_event(AppEvent::AgentAction {
            project_id: String::new(),
            issue: "ZAP-204".into(),
            action: "ran grep".into(),
            working: true,
        }));
        assert!(!app.apply_event(AppEvent::AgentStatusChanged {
            project_id: String::new(),
            issue: "ZAP-204".into(),
            status: AgentStatus::Idle,
        }));
        assert!(!app.fleet.contains_key("ZAP-204"));
        assert!(!app.needs_you_alert, "no phantom sticky alert");
        // A genuine relaunch clears the tombstone.
        let fake2 = FakeBackend::new("ZAP-204");
        app.apply_event(AppEvent::AgentSpawned {
            project_id: String::new(),
            issue: "ZAP-204".into(),
            backend: fake2 as Arc<dyn AgentBackend>,
            repos: Vec::new(),
        });
        assert!(app.apply_event(AppEvent::AgentNeedsYou {
            project_id: String::new(),
            issue: "ZAP-204".into(),
            reason: "real".into(),
        }));
        assert_eq!(app.fleet.get("ZAP-204"), Some(&AgentStatus::NeedsYou));
    }

    #[test]
    fn a_late_hook_cannot_resurrect_a_reaped_ask_agent() {
        // FEAT-A: `restore_ask_issue_if_needed` runs at the TOP of the live status
        // handlers — before their reaped/terminal guards — and `ensure_ask_issue` is
        // unconditional. So a stray post-cancel hook for a *killed* ad-hoc agent must
        // not re-arm `ask_agents` or re-inject its synthetic Spine node. (The existing
        // reaped-resurrection test uses a non-synthetic key, so it never exercises the
        // `is_synthetic_ask_id` restore path that owns this bug.)
        let ask = crate::worktree::synthetic_ask_id();
        let mut app = app();
        app.reaped.insert(ask.clone()); // the ad-hoc agent was killed/discarded
        assert!(app.graph.get(&ask).is_none(), "no synthetic node before the hook");

        for ev in [
            AppEvent::AgentNeedsYou {
                project_id: String::new(),
                issue: ask.clone(),
                reason: "late prompt".into(),
            },
            AppEvent::AgentAction {
                project_id: String::new(),
                issue: ask.clone(),
                action: "ran grep".into(),
                working: true,
            },
            AppEvent::AgentStatusChanged {
                project_id: String::new(),
                issue: ask.clone(),
                status: AgentStatus::Running,
            },
        ] {
            assert!(
                !app.apply_event(ev),
                "a reaped ask agent's late live hook is dropped"
            );
            assert!(
                !app.ask_agents.contains(&ask),
                "the reaped ad-hoc agent is not re-armed"
            );
            assert!(
                app.graph.get(&ask).is_none(),
                "the reaped ad-hoc agent's synthetic node is not resurrected into the Spine"
            );
            assert!(!app.fleet.contains_key(&ask), "no phantom fleet entry");
        }
    }

    #[test]
    fn a_live_ask_agents_pruned_node_is_restored_on_its_next_hook() {
        // The legitimate path the guard must leave intact: a still-live ad-hoc agent
        // whose synthetic node was pruned (e.g. a reconcile on a project switch-back)
        // gets re-injected into the Spine when its next live hook arrives.
        let ask = crate::worktree::synthetic_ask_id();
        let mut app = app();
        assert!(app.graph.get(&ask).is_none());
        assert!(app.apply_event(AppEvent::AgentNeedsYou {
            project_id: String::new(),
            issue: ask.clone(),
            reason: "needs you".into(),
        }));
        assert!(
            app.ask_agents.contains(&ask),
            "a live ad-hoc agent is (re-)armed"
        );
        assert!(
            app.graph.get(&ask).is_some(),
            "its synthetic Spine node is restored"
        );
        assert_eq!(app.fleet.get(&ask), Some(&AgentStatus::NeedsYou));
    }

    #[test]
    fn idle_with_recent_pty_output_reads_as_working() {
        // FEAT-B: a settled Idle agent whose child is still streaming PTY output is
        // busy, not resting — its readiness band upgrades to WORKING, matching the
        // live gutter spinner `display_agent_status` shows (so no band/row stutter).
        let mut app = app();
        app.fleet.insert("ZAP-204".into(), AgentStatus::Idle);
        assert_eq!(
            app.readiness("ZAP-204"),
            Readiness::Idle,
            "no output yet → honestly resting"
        );
        app.apply_event(AppEvent::AgentOutput {
            project_id: "".into(),
            issue: "ZAP-204".into(),
        });
        assert!(app.recently_active("ZAP-204"), "the PTY stamp is fresh");
        assert_eq!(
            app.readiness("ZAP-204"),
            Readiness::Working,
            "a quiet-but-producing Idle agent bands under WORKING"
        );
        assert_eq!(
            app.display_agent_status("ZAP-204", AgentStatus::Idle),
            AgentStatus::Running,
            "and the gutter glyph shows the live spinner — band + row agree"
        );
    }

    #[test]
    fn quiet_window_expires_back_to_idle() {
        // The overlay self-expires: once PTY output stops for the settle window the
        // agent falls back to the honest resting IDLE (proves the frame clock + the
        // window length cooperate).
        let mut app = app();
        app.fleet.insert("ZAP-204".into(), AgentStatus::Idle);
        app.apply_event(AppEvent::AgentOutput {
            project_id: "".into(),
            issue: "ZAP-204".into(),
        });
        assert_eq!(app.readiness("ZAP-204"), Readiness::Working);
        for _ in 0..OUTPUT_SETTLE_FRAMES {
            app.tick_frame();
        }
        assert!(!app.recently_active("ZAP-204"), "the window expired");
        assert_eq!(
            app.readiness("ZAP-204"),
            Readiness::Idle,
            "with no fresh output the band settles back to IDLE"
        );
    }

    #[test]
    fn recently_active_idle_keeps_the_cockpit_animating_then_settles() {
        // The frame clock must keep ticking while an Idle agent is recently-active —
        // otherwise the window could never expire on an otherwise-quiet fleet — and
        // then settle so a quiet child doesn't pin the loop awake forever.
        let mut app = app();
        app.fleet.insert("ZAP-204".into(), AgentStatus::Idle);
        assert!(
            !app.is_animating(),
            "a resting Idle agent alone doesn't animate"
        );
        app.apply_event(AppEvent::AgentOutput {
            project_id: "".into(),
            issue: "ZAP-204".into(),
        });
        assert!(app.is_animating(), "recent PTY output keeps the loop awake");
        for _ in 0..OUTPUT_SETTLE_FRAMES {
            app.tick_frame();
        }
        assert!(
            !app.is_animating(),
            "once the window expires the cockpit settles — no perpetual wake-up"
        );
    }

    #[test]
    fn pty_output_never_revives_a_terminal_or_absent_agent() {
        // The overlay only upgrades Idle: a terminal (or never-seen) agent that emits
        // a late PTY burst must not read as active, and its band stays graph-truth.
        for status in [AgentStatus::Done, AgentStatus::Stopped, AgentStatus::Failed] {
            let mut app = app();
            app.fleet.insert("ZAP-204".into(), status);
            app.apply_event(AppEvent::AgentOutput {
                project_id: "".into(),
                issue: "ZAP-204".into(),
            });
            assert_eq!(
                app.fleet.get("ZAP-204"),
                Some(&status),
                "{status:?} fleet entry unchanged"
            );
            assert!(
                !app.recently_active("ZAP-204"),
                "{status:?} is terminal — never recently-active"
            );
            assert_ne!(
                app.readiness("ZAP-204"),
                Readiness::Working,
                "{status:?} never bands under WORKING from PTY output"
            );
        }
        // An issue with no fleet entry conjures nothing from a stray output event.
        let mut app = app();
        app.apply_event(AppEvent::AgentOutput {
            project_id: "".into(),
            issue: "GHOST".into(),
        });
        assert!(!app.recently_active("GHOST"));
        assert!(!app.fleet.contains_key("GHOST"));
    }

    #[test]
    fn pty_output_does_not_touch_a_non_idle_live_agent() {
        // Spawning/Running/NeedsYou already render live — the Idle-only overlay must not
        // perturb their band or status.
        for status in [
            AgentStatus::Spawning,
            AgentStatus::Running,
            AgentStatus::NeedsYou,
        ] {
            let mut app = app();
            app.fleet.insert("ZAP-204".into(), status);
            let band_before = app.readiness("ZAP-204");
            app.apply_event(AppEvent::AgentOutput {
                project_id: "".into(),
                issue: "ZAP-204".into(),
            });
            assert!(!app.recently_active("ZAP-204"), "the overlay is Idle-only");
            assert_eq!(
                app.readiness("ZAP-204"),
                band_before,
                "{status:?} band is unchanged by PTY output"
            );
            assert_eq!(app.fleet.get("ZAP-204"), Some(&status), "{status:?} unchanged");
        }
    }

    #[test]
    fn a_fresh_pty_burst_repaints_an_off_screen_idle_agent_then_quiets() {
        // FEAT-B step 3: the FIRST PTY burst for an off-screen Idle agent forces a
        // repaint so its band flips to WORKING immediately; a steady-state burst while
        // it is already active does not (preserving the off-screen-quiet contract). A
        // terminal issue's late output is never stamped at all (the is_live stamp gate).
        let mut app = app();
        app.fleet.insert("ZAP-9".into(), AgentStatus::Idle); // no window → off-screen
        assert!(
            app.apply_event(AppEvent::AgentOutput {
                project_id: "".into(),
                issue: "ZAP-9".into(),
            }),
            "the first burst flips the band, so it must repaint even off-screen"
        );
        assert!(
            !app.apply_event(AppEvent::AgentOutput {
                project_id: "".into(),
                issue: "ZAP-9".into(),
            }),
            "steady-state output for an already-active off-screen agent stays quiet"
        );
        // A terminal issue's late PTY burst is not stamped into the overlay map.
        app.fleet.insert("ZAP-9".into(), AgentStatus::Done);
        app.last_output.remove("ZAP-9");
        app.apply_event(AppEvent::AgentOutput {
            project_id: "".into(),
            issue: "ZAP-9".into(),
        });
        assert!(
            !app.last_output.contains_key("ZAP-9"),
            "a terminal issue's output is not stamped (the is_live stamp gate)"
        );
    }

    #[test]
    fn overlay_is_pruned_on_reap_and_age() {
        // The `last_output` map must stay bounded: a reap drops the stamp with the
        // fleet entry, and `tick_frame` ages out a stale stamp on its own.
        {
            let mut app = app();
            app.fleet.insert("ZAP-204".into(), AgentStatus::Idle);
            app.apply_event(AppEvent::AgentOutput {
                project_id: "".into(),
                issue: "ZAP-204".into(),
            });
            assert!(app.last_output.contains_key("ZAP-204"));
            app.apply_event(AppEvent::AgentReaped {
                project_id: String::new(),
                issue: "ZAP-204".into(),
            });
            assert!(
                !app.last_output.contains_key("ZAP-204"),
                "AgentReaped drops the overlay stamp with the fleet entry"
            );
        }

        // Age-prune: a stale stamp is dropped by tick_frame even without a reap.
        let mut app = app();
        app.fleet.insert("ZAP-9".into(), AgentStatus::Idle);
        app.apply_event(AppEvent::AgentOutput {
            project_id: "".into(),
            issue: "ZAP-9".into(),
        });
        for _ in 0..OUTPUT_SETTLE_FRAMES {
            app.tick_frame();
        }
        assert!(
            !app.last_output.contains_key("ZAP-9"),
            "tick_frame ages out a stale stamp so the map stays bounded"
        );
    }

    #[test]
    fn a_killed_agent_flashes_stopped_and_bands_terminal() {
        // R2 (CF-4 / CF-14): a confirmed kill seeds `reaped` synchronously, THEN the
        // supervisor's graded terminal `Stopped` arrives. The reaped guard must block
        // only LIVE re-emits — the terminal Stopped must still land so the graphite
        // kill pulse (Flash::Stopped, its sole producer) fires and the Spine bands the
        // row terminal immediately, instead of stale-live until AgentReaped. No test
        // previously asserted the pulse fires on a kill.
        let mut app = app();
        app.fleet.insert("ZAP-204".into(), AgentStatus::Running);
        app.reaped.insert("ZAP-204".into()); // kill confirmed → tombstoned up front

        assert!(app.apply_event(AppEvent::AgentStatusChanged {
            project_id: String::new(),
            issue: "ZAP-204".into(),
            status: AgentStatus::Stopped,
        }));
        assert!(
            matches!(app.flash.get("ZAP-204"), Some((Flash::Stopped, _))),
            "the deliberate kill fires the graphite Stopped pulse"
        );
        assert_eq!(
            app.fleet.get("ZAP-204"),
            Some(&AgentStatus::Stopped),
            "the killed row bands terminal at once, not stale-live until reap"
        );

        // …but a LIVE re-emit for the same reaped issue is still dropped — the kill
        // must not be resurrected.
        assert!(!app.apply_event(AppEvent::AgentStatusChanged {
            project_id: String::new(),
            issue: "ZAP-204".into(),
            status: AgentStatus::Running,
        }));
        assert_eq!(app.fleet.get("ZAP-204"), Some(&AgentStatus::Stopped));
    }

    #[test]
    fn a_reaped_synthetic_ask_agents_terminal_stop_still_flashes_and_bands() {
        // FEAT-A guard harmlessness: restore_ask_issue_if_needed early-returns for a
        // reaped SYNTHETIC ask id (skipping node injection), but it must NOT suppress the
        // deliberate-kill terminal flash/banding — that path runs after and independent of
        // the restore helper. (The R2 test uses a non-synthetic id; this exercises the
        // synthetic-id branch of the guard.)
        let ask = crate::worktree::synthetic_ask_id();
        let mut app = app();
        app.ask_agents.insert(ask.clone());
        app.fleet.insert(ask.clone(), AgentStatus::Running);
        app.reaped.insert(ask.clone()); // the ad-hoc agent was killed

        assert!(app.apply_event(AppEvent::AgentStatusChanged {
            project_id: String::new(),
            issue: ask.clone(),
            status: AgentStatus::Stopped,
        }));
        assert!(
            matches!(app.flash.get(&ask), Some((Flash::Stopped, _))),
            "the killed ad-hoc agent still gets its graphite Stopped pulse"
        );
        assert_eq!(
            app.fleet.get(&ask),
            Some(&AgentStatus::Stopped),
            "and bands terminal — the restore guard didn't swallow the terminal status"
        );
        assert!(
            app.graph.get(&ask).is_none(),
            "yet the guard still held — no synthetic node was resurrected by the late hook"
        );
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
                project_id: String::new(),
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
            project_id: String::new(),
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
                project_id: "".into(),
                issue: "ZAP-205".into()
            }),
            "a visible agent's output forces a redraw"
        );
        // An agent with no window changes nothing visible.
        assert!(!app.apply_event(AppEvent::AgentOutput {
            project_id: "".into(),
            issue: "ZAP-999".into()
        }));
    }

    #[test]
    fn spawning_for_a_pending_button_opens_and_focuses_the_window() {
        let mut app = app();
        app.pending_attach = Some("ZAP-205".into());
        let fake = FakeBackend::new("ZAP-205");
        app.apply_event(AppEvent::AgentSpawned {
            project_id: String::new(),
            issue: "ZAP-205".into(),
            backend: fake as Arc<dyn AgentBackend>,
            repos: Vec::new(),
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
            project_id: String::new(),
            issue: "ZAP-205".into(),
            backend: fake as Arc<dyn AgentBackend>,
            repos: Vec::new(),
        });
        // No pending_attach / pending_launch → the roster gains it, focus stays put.
        assert_eq!(app.windows.focus, focus_before);
        assert!(!app.windows.references_agent("ZAP-205"));
        assert_eq!(app.fleet.get("ZAP-205"), Some(&AgentStatus::Spawning));
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
    fn a_wedged_resume_releases_its_pending_launch_guard_on_grace() {
        // A resume that never spawns (a hung worktree add: no AgentSpawned and no
        // setup-failure Notification) self-clears its spinner on grace but used to
        // leave `pending_launch` set forever — so `maybe_resume_focused` and
        // `resume_one` early-returned on it and the docked card could never be
        // revived once a slot freed. The grace expiry must release the guard in
        // lockstep with the spinner.
        let mut app = app();
        app.mark_resuming_for_test("WEDGED");
        assert!(
            app.pending_launch.contains_key("WEDGED"),
            "resume arms the double-launch guard"
        );
        for _ in 0..=RESUME_GRACE_FRAMES {
            app.tick_frame();
        }
        assert_eq!(app.resuming_count(), 0, "the spinner cleared on its grace");
        assert!(
            !app.pending_launch.contains_key("WEDGED"),
            "the pending_launch guard was released, so the card can resume again"
        );
    }

    #[test]
    fn an_agent_spawn_settles_its_own_resume() {
        let mut app = app();
        app.mark_resuming_for_test("ZAP-205");
        app.mark_resuming_for_test("ZAP-206");
        let fake = FakeBackend::new("ZAP-205");
        app.apply_event(AppEvent::AgentSpawned {
            project_id: String::new(),
            issue: "ZAP-205".into(),
            backend: fake as Arc<dyn AgentBackend>,
            repos: Vec::new(),
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

    // ── Readiness classifier (ENG-557) ───────────────────────────────────────

    /// A graph exercising every graph-truth branch of [`App::readiness`]:
    /// `ready` (unblocked, unstarted), `blocked` (held by the unresolved
    /// `gate`), `done` (completed), and a `cyc1`↔`cyc2` cycle whose `cyc2` is
    /// resolved — so `cyc1` is *in a cycle yet not `is_blocked`*, isolating the
    /// in_cycle arm.
    fn readiness_graph() -> Graph {
        use crate::model::{Priority, Status};
        let node = |key: &str, status: Status| Issue {
            key: key.into(),
            title: key.into(),
            status,
            priority: Priority::None,
            assignee: None,
            external: false,
        };
        let mut g = Graph::new("t");
        g.add_issue(node("gate", Status::Started)); // an unresolved blocker
        g.add_issue(node("blocked", Status::Unstarted));
        g.add_issue(node("ready", Status::Unstarted));
        g.add_issue(node("done", Status::Completed));
        g.add_issue(node("cyc1", Status::Unstarted));
        g.add_issue(node("cyc2", Status::Completed));
        g.add_edge("gate", "blocked");
        g.add_edge("cyc1", "cyc2"); // cyc1 ↔ cyc2 — a structural cycle
        g.add_edge("cyc2", "cyc1");
        g.finalize();
        g
    }

    fn readiness_app() -> App {
        let mut a = App::new(readiness_graph());
        a.set_viewport(Rect::new(0, 0, 200, 40));
        a
    }

    #[test]
    fn readiness_classifies_graph_truth_for_agentless_issues() {
        let a = readiness_app();
        assert_eq!(a.readiness("ready"), Readiness::Ready);
        assert_eq!(a.readiness("blocked"), Readiness::Blocked);
        assert_eq!(a.readiness("done"), Readiness::Done);
        // `gate` holds `blocked` but is itself unblocked → Ready.
        assert_eq!(a.readiness("gate"), Readiness::Ready);
    }

    #[test]
    fn readiness_treats_a_cycle_member_as_blocked_even_when_unblocked() {
        let a = readiness_app();
        // cyc1's only blocker (cyc2) is resolved, so `is_blocked` is false — but
        // it sits in a cycle, an un-runnable sub-state of Blocked. Without the
        // in_cycle arm this would wrongly read as Ready.
        assert_eq!(a.readiness("cyc1"), Readiness::Blocked);
        // A *resolved* cycle member is Done — resolution outranks the cycle.
        assert_eq!(a.readiness("cyc2"), Readiness::Done);
    }

    #[test]
    fn readiness_lets_a_live_agent_outrank_graph_truth() {
        let mut a = readiness_app();
        // A needs-you agent is the top band, even on a blocked issue.
        a.fleet.insert("blocked".into(), AgentStatus::NeedsYou);
        assert_eq!(a.readiness("blocked"), Readiness::NeedsYou);
        // Spawning / Running churn → Working; Idle rests → Idle.
        for s in [AgentStatus::Spawning, AgentStatus::Running] {
            a.fleet.insert("ready".into(), s);
            assert_eq!(a.readiness("ready"), Readiness::Working, "{s:?} is working");
        }
        a.fleet.insert("ready".into(), AgentStatus::Idle);
        assert_eq!(a.readiness("ready"), Readiness::Idle, "idle agent rests");
        // A live agent even outranks a resolved issue.
        a.fleet.insert("done".into(), AgentStatus::Running);
        assert_eq!(a.readiness("done"), Readiness::Working);
    }

    #[test]
    fn readiness_reverts_to_graph_truth_when_the_agent_is_terminal() {
        let mut a = readiness_app();
        // Stopped / Done / Failed pin no band — the issue is its graph state, so
        // a failed launch is re-dispatchable (Ready) and a blocked one stays Blocked.
        for s in [AgentStatus::Stopped, AgentStatus::Done, AgentStatus::Failed] {
            a.fleet.insert("ready".into(), s);
            assert_eq!(a.readiness("ready"), Readiness::Ready, "{s:?} → Ready");
            a.fleet.insert("blocked".into(), s);
            assert_eq!(
                a.readiness("blocked"),
                Readiness::Blocked,
                "{s:?} → Blocked"
            );
        }
    }

    #[test]
    fn readiness_classifies_a_fleet_member_absent_from_the_graph() {
        // An archived issue whose session survived reconcile isn't in the graph,
        // but a needs-you agent on it must still classify (the header counts it).
        let mut a = readiness_app();
        a.fleet.insert("ENG-archived".into(), AgentStatus::NeedsYou);
        assert_eq!(a.readiness("ENG-archived"), Readiness::NeedsYou);
    }

    #[test]
    fn readiness_band_order_is_the_schedule_order() {
        // The derived Ord is the top→bottom band order ENG-558 will sort on.
        assert!(Readiness::NeedsYou < Readiness::Working);
        assert!(Readiness::Working < Readiness::Idle);
        assert!(Readiness::Idle < Readiness::Ready);
        assert!(Readiness::Ready < Readiness::Blocked);
        assert!(Readiness::Blocked < Readiness::Done);
    }

    #[test]
    fn order_is_banded_detects_within_band_staleness() {
        // The self-heal oracle must catch a WITHIN-band drift, not just band
        // monotonicity — else a same-band transition (NeedsYou→Running) leaves
        // the band mis-sorted by impact until an unrelated rebuild. `gate`
        // (downstream impact 1, blocks `blocked`) must out-rank `ready` (impact 0)
        // inside the READY band; the reversed order must read as un-banded.
        let mut a = readiness_app();
        a.order = vec!["ready".into(), "gate".into()];
        assert!(
            !a.order_is_banded(),
            "within-band impact drift is detected, not just band order"
        );
        a.rebuild_order();
        assert!(a.order_is_banded(), "a rebuild restores the full ordering");
    }

    // ── Readiness-gated dispatch (ENG-559) ───────────────────────────────────

    #[test]
    fn dispatching_a_blocked_issue_is_refused_with_a_deps_footer() {
        let mut a = app();
        a.workspace = Some(crate::workspace::WorkspaceHandle::detached());
        a.active_project = "proj".into();
        // ZAP-205 is blocked by ZAP-204 (Started) in the demo graph.
        a.aim_spine("ZAP-205".into());
        a.button();
        assert!(
            a.pending_launch.is_empty(),
            "a blocked dispatch launches nothing"
        );
        let msg = a.status_msg.clone().unwrap_or_default();
        assert!(
            msg.contains("ZAP-205") && msg.contains("blocked by") && msg.contains("refused"),
            "deps-aware refusal footer, got: {msg:?}"
        );
    }

    #[test]
    fn dispatching_a_ready_issue_launches() {
        let mut a = app();
        a.workspace = Some(crate::workspace::WorkspaceHandle::detached());
        a.active_project = "proj".into();
        // ZAP-188 is ready — its one blocker (ZAP-150) is Completed.
        a.aim_spine("ZAP-188".into());
        a.button();
        assert!(
            a.pending_launch.contains_key("ZAP-188"),
            "a ready dispatch launches an agent"
        );
    }

    #[test]
    fn a_capacity_refusal_reads_differently_from_a_dep_block() {
        let mut a = app();
        a.workspace = Some(crate::workspace::WorkspaceHandle::detached());
        a.active_project = "proj".into();
        a.resume_cap = 1; // a fleet ceiling of one…
        register(&mut a, "ZAP-201"); // …already filled by one genuinely-live agent
        a.fleet.insert("ZAP-201".into(), AgentStatus::Running); // (backend + live status)
        a.aim_spine("ZAP-188".into()); // a READY issue, so this is NOT a dep block
        a.button();
        assert!(
            a.pending_launch.is_empty(),
            "a full fleet refuses the launch"
        );
        let msg = a.status_msg.clone().unwrap_or_default();
        assert!(
            msg.contains("capacity"),
            "capacity refusal names capacity, got: {msg:?}"
        );
        assert!(
            !msg.contains("blocked by"),
            "a capacity refusal must not read as a dependency block, got: {msg:?}"
        );
    }

    #[test]
    fn a_terminal_exited_card_does_not_consume_fleet_capacity() {
        // The capacity gate counts genuinely-live agents, not backend-map size. A
        // finished agent whose EXITED card is still open keeps a `backends` entry but
        // a terminal fleet status, so it must NOT fill the ceiling — otherwise a
        // project full of finished cards could never launch again.
        let mut a = app();
        a.workspace = Some(crate::workspace::WorkspaceHandle::detached());
        a.active_project = "proj".into();
        a.resume_cap = 1;
        register(&mut a, "ZAP-201"); // a backend whose card is still up…
        a.fleet.insert("ZAP-201".into(), AgentStatus::Done); // …but the agent finished
        a.aim_spine("ZAP-188".into()); // a READY issue
        a.button();
        assert!(
            a.pending_launch.contains_key("ZAP-188"),
            "a terminal EXITED card must not count toward capacity"
        );
    }

    // ── Subtraction acceptance gate (ENG-563) ────────────────────────────────

    #[test]
    fn the_filter_cycle_is_the_collapsed_v17_residual() {
        // v1.7 is net-smaller by construction: the readiness schedule replaced the
        // old 5 sorts outright (the flat id-sort and its `r` binding are gone) and
        // collapsed the filters. The filter cycle must return to start in exactly
        // the residual number of stops — a guard against re-growing them.
        let mut f = Filter::All;
        let mut filters = vec![f];
        loop {
            f = f.next();
            if f == Filter::All {
                break;
            }
            filters.push(f);
        }
        assert_eq!(
            filters,
            vec![Filter::All, Filter::HasDeps],
            "filters collapsed to {{all, has-deps}}"
        );
    }

    #[test]
    fn spine_paging_clamps_to_the_edges_without_wrapping() {
        // M3: top/bottom + paging are hard-edged, never the wrapping teleport that
        // single-step j/k uses, so PageDown at the bottom can't fling you to the top.
        let mut a = app();
        assert!(a.order.len() >= 2, "need a multi-row order to page through");
        let first = a.order.first().unwrap().clone();
        let last = a.order.last().unwrap().clone();

        a.dispatch_spine(Action::MoveBottom);
        assert_eq!(a.root, last, "MoveBottom selects the last row");
        a.dispatch_spine(Action::PageDown);
        assert_eq!(a.root, last, "PageDown at the bottom does not wrap");

        a.dispatch_spine(Action::MoveTop);
        assert_eq!(a.root, first, "MoveTop selects the first row");
        a.dispatch_spine(Action::PageUp);
        assert_eq!(a.root, first, "PageUp at the top does not wrap");
    }
}
