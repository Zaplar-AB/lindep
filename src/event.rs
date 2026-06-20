//! The async event backbone.
//!
//! The cockpit keeps a **synchronous** ratatui render loop (see
//! [`crate::main`]'s `event_loop`), but agents, the hook endpoint and the PTY
//! pumps all run on a background `tokio` runtime. They communicate state changes
//! to the loop by sending an [`AppEvent`] down an mpsc channel; the loop drains
//! it with a non-blocking `try_recv` each tick and repaints **only** when an
//! event actually changed something. A fully idle cockpit therefore still never
//! busy-repaints — the property the original loop was written to preserve.
//!
//! This module owns the channel and the runtime handle; the variants of
//! [`AppEvent`] grow as later subsystems (supervisor, notification bus) land.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::backend::AgentBackend;
use crate::session::AgentStatus;

/// A message from a background task to the render loop. Anything that mutates
/// [`crate::app::App`] state from off the render thread travels as one of these,
/// so the loop is the single writer of view state.
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// Show a transient line in the footer. Carries its own text so any
    /// subsystem can surface a message without reaching into `App`. The startup
    /// readiness banner is the first producer; the notification bus (ENG-397)
    /// and supervisor add more.
    Notification(String),
    /// A new agent was launched for `(project_id, issue)`. Carries the backend
    /// handle so the cockpit can render and attach to it.
    AgentSpawned {
        project_id: String,
        issue: String,
        backend: Arc<dyn AgentBackend>,
        /// The repo handles the supervisor materialised for this agent, primary
        /// first (ENG-536). One entry is the common single-repo case; more than
        /// one is a multi-repo agent spanning sibling worktrees. The cockpit keeps
        /// this so a launched agent can show *which* repos/worktrees it owns — the
        /// supervisor is the only authority here (it folds in the primary and any
        /// lazily-pulled repos a resume rehydrates, which the picker never sees).
        repos: Vec<String>,
    },
    /// An agent's PTY produced output and its pane should repaint. Coalesced by
    /// the loop into a single redraw per tick. The ids are `Arc<str>` (not `String`)
    /// because the pump emits one of these per PTY read — cloning an `Arc` is a
    /// refcount bump, where two fresh `String`s would heap-allocate on every chunk.
    AgentOutput {
        project_id: Arc<str>,
        issue: Arc<str>,
    },
    /// An agent process exited (cleanly or not).
    AgentExited {
        project_id: String,
        issue: String,
        code: Option<i32>,
    },
    /// The agent on `(project_id, issue)` is waiting on the human — a permission
    /// prompt or an idle nudge (from a `Notification` hook).
    AgentNeedsYou {
        project_id: String,
        issue: String,
        reason: String,
    },
    /// The agent on `(project_id, issue)` changed conversational status (e.g.
    /// `Stop` → idle).
    AgentStatusChanged {
        project_id: String,
        issue: String,
        status: AgentStatus,
    },
    /// The agent on `(project_id, issue)` took an action (e.g. ran a tool) — a
    /// per-issue activity line (from a `PostToolUse` hook, an idle nudge, …).
    AgentAction {
        project_id: String,
        issue: String,
        action: String,
        /// Whether this action is an unambiguous sign the agent is *actively
        /// working* — a tool ran, or an MCP elicitation was answered. A working
        /// action promotes even a `NeedsYou` agent back to `Running` and clears
        /// its attention flag (answering a prompt then resuming work is exactly
        /// this). An ambient action (the ~60 s idle nudge) leaves a needs-you
        /// agent untouched, so routine chatter never silences a real prompt.
        working: bool,
    },
    /// The supervisor finished tearing an agent down and dropped it from its live
    /// map. The cockpit drops it from the fleet view too, so the overview stays
    /// bounded and mirrors the supervisor instead of accreting dead agents.
    AgentReaped { project_id: String, issue: String },
    /// An agent committed in `repo_handle`'s worktree (from a `post-commit` hook).
    /// Drives v1.6 auto-push: the work is pushed to the repo's true remote off the
    /// status machinery (a commit is never "needs you"), and `outcome` carries the
    /// push's true fate so the cockpit reports it faithfully — a *rejected* push
    /// raises a standing "unpushed" chip instead of being papered over by a blanket
    /// "pushed" (the v1.6 "a rejected push is never papered over" contract). `branch`
    /// is the committed branch.
    AgentCommitted {
        project_id: String,
        issue: String,
        repo_handle: String,
        branch: String,
        outcome: PushOutcome,
    },
    /// The agent on `(project_id, issue)` requested an extra repo be pulled into its
    /// workspace (from `lindep request-repo <handle>` over the hook endpoint, ENG-542).
    /// Already fenced to the project's candidate set by the CLI; the cockpit raises a
    /// confirmation modal and, on confirm, materialises it (L1→L2→L3). `repo_handle`
    /// is the requested repo.
    RepoRequested {
        project_id: String,
        issue: String,
        repo_handle: String,
    },
    /// The supervisor refused a specific launch (at capacity, already running, or
    /// still stopping). Carries the rejected `issue` so the cockpit drops *only* that
    /// issue's double-press guard — unlike a bare `Notification`, which used to clear
    /// every issue's `pending_launch` (M10). NOT agent-scoped — launches only target
    /// the active project, so `project_id()` is `None`.
    LaunchRejected { issue: String, reason: String },
    /// A background disk-reclaim scan finished (`Ctrl-a m`, ENG-540). Carries the
    /// unreferenced mirrors safe to offer, whether this scan should *open* the
    /// prompt (the initial scan) or merely *refresh* an already-open one (the
    /// post-delete rescan), and an optional footer note (a delete's outcome). Both
    /// the scan (`reclaimable_mirrors`, a recursive object-DB walk) and the delete
    /// (`delete_mirror`, which takes a blocking cross-process flock) run on the
    /// blocking pool so they can't freeze the render loop; this event carries the
    /// result back. NOT agent-scoped — `project_id()` is `None`.
    ReclaimScanned {
        mirrors: Vec<crate::mirror::ReclaimableMirror>,
        opening: bool,
        note: Option<String>,
    },
    /// First-materialisation clone progress for `project_id` — a slow
    /// `git clone --mirror` (hundreds of MB) is streaming, so the footer shows
    /// e.g. "materialising core · Receiving objects 45%" instead of looking frozen
    /// (the v1.6 "surface progress" gap). `phase` is git's phase label and
    /// `percent` its 0–100 reading. Project-scoped so switching away while a
    /// backgrounded project still clones drops its ticks (the cross-project guard).
    MaterializeProgress {
        project_id: String,
        phase: String,
        percent: u8,
    },
    /// First materialisation of `project_id` finished cloning — sent only when a
    /// progress meter was actually drawn (a real clone, not the mirror-already-there
    /// fast path). Lets the footer replace the lingering "materialising … 100%" tick
    /// with a terminal "materialised …" line, so it doesn't read as still-running.
    /// Project-scoped, like [`AppEvent::MaterializeProgress`].
    MaterializeDone { project_id: String },
    /// A project switch's graph finished loading off the render thread (see
    /// `App::request_switch`). A pure wake signal: the loaded `(ProjectRef, Graph)`
    /// rides a side mailbox because `Graph` isn't `Clone`/`Debug` and so can't live
    /// in this enum (latest switch wins). NOT agent-scoped — it *changes* the
    /// active project — so `project_id()` is `None` and the guard never drops it.
    ProjectActivated,
    /// A confirmed discard finished. `WorkspaceDiscarded` = the worktrees were actually
    /// removed (the cockpit may now drop the fleet entry + window). `DiscardKeptWorktree`
    /// = teardown KEPT the worktree (a rejected push left unpushed commits on disk), so
    /// the cockpit must NOT silently drop it — it raises a standing "unpushed work kept"
    /// chip and leaves the issue re-discardable (D-HIGH). Project-scoped because a
    /// backgrounded teardown result must not mutate the active project if issue keys
    /// collide across projects.
    WorkspaceDiscarded { project_id: String, issue: String },
    DiscardKeptWorktree {
        project_id: String,
        issue: String,
        reason: String,
    },
}

/// The fate of a v1.6 auto-push, carried on [`AppEvent::AgentCommitted`]. A commit
/// always happened — only the *push* of it can fail or be a no-op — so the cockpit
/// must distinguish "reached the true remote" from "stranded on the local clone",
/// never collapsing both to "pushed" (a rejected push masquerading as a clean
/// success is the exact data-integrity bug this enum closes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    /// The committed branch reached the repo's true remote.
    Pushed,
    /// The push to the true remote was rejected (or the push task panicked): the
    /// commit is stranded on the local clone and has NOT reached the remote. The
    /// string is git's clamped reason, shown in the footer and the standing chip.
    Rejected(String),
    /// The repo is local-only (no true remote): the branch was pushed to the
    /// synthesised bare mirror — the durability backstop a clone rebuild recovers
    /// from — but there is nowhere to publish it. A clean state, reported as
    /// "committed" (never "pushed", which would imply a remote), and never the
    /// "unpushed" chip.
    LocalOnly,
}

impl AppEvent {
    /// The project an agent-lifecycle event belongs to, or `None` for an event
    /// that isn't agent-scoped ([`AppEvent::Notification`], and
    /// [`AppEvent::ProjectActivated`] which *changes* the active project). The
    /// render loop uses this to file each event under the right project's fleet.
    pub fn project_id(&self) -> Option<&str> {
        match self {
            AppEvent::Notification(_)
            | AppEvent::LaunchRejected { .. }
            | AppEvent::ReclaimScanned { .. }
            | AppEvent::ProjectActivated => None,
            AppEvent::AgentSpawned { project_id, .. }
            | AppEvent::AgentExited { project_id, .. }
            | AppEvent::AgentNeedsYou { project_id, .. }
            | AppEvent::AgentStatusChanged { project_id, .. }
            | AppEvent::AgentAction { project_id, .. }
            | AppEvent::AgentReaped { project_id, .. }
            | AppEvent::AgentCommitted { project_id, .. }
            | AppEvent::MaterializeProgress { project_id, .. }
            | AppEvent::MaterializeDone { project_id, .. }
            | AppEvent::RepoRequested { project_id, .. }
            | AppEvent::WorkspaceDiscarded { project_id, .. }
            | AppEvent::DiscardKeptWorktree { project_id, .. } => Some(project_id),
            // Separate arm: its `project_id` is `Arc<str>`, not `String`, so it can't
            // share the or-pattern above (both still deref-coerce to `&str` here).
            AppEvent::AgentOutput { project_id, .. } => Some(project_id),
        }
    }
}

/// Sender half — cloned into every background subsystem.
pub type AppEventTx = mpsc::UnboundedSender<AppEvent>;
/// Receiver half — owned solely by the render loop.
pub type AppEventRx = mpsc::UnboundedReceiver<AppEvent>;

/// Create the app-event channel.
///
/// Unbounded because the loop drains the whole queue every tick and the senders
/// must never block the runtime; high-frequency producers (PTY output) are
/// expected to coalesce their own signals before sending rather than relying on
/// backpressure here.
pub fn channel() -> (AppEventTx, AppEventRx) {
    mpsc::unbounded_channel()
}

/// Build the multi-threaded runtime that carries all background work.
///
/// Kept separate from the channel so `main` can stand the runtime up, hand
/// `Handle`s and an [`AppEventTx`] to the subsystems it spawns, and still own
/// the synchronous render loop on the main thread.
pub fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        // The async side is light and await-heavy: one supervisor task, one
        // accept loop, short hook handlers, and per-agent supervise tasks that
        // mostly sit in `select!`. Concurrency is bounded by `max_concurrent`
        // agents, and the genuinely blocking work (PTY read/wait, git) runs on
        // dedicated/blocking threads — not these workers. So a small explicit
        // cap serves the whole workload; the num_cpus default would spin a dozen
        // idle workers on a workstation for no benefit. The blocking pool keeps
        // its default for `git`/`spawn_blocking`.
        .worker_threads(2)
        .enable_all()
        .thread_name("lindep-rt")
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_background_task_can_wake_the_loop_through_the_channel() {
        // The whole point of the backbone: a task that holds only a `Sender`
        // delivers an event the loop can pick up, with no terminal input.
        let (tx, mut rx) = channel();
        tokio::spawn(async move {
            let _ = tx.send(AppEvent::Notification("agent online".into()));
        });
        let ev = rx.recv().await.expect("event delivered");
        let AppEvent::Notification(text) = ev else {
            panic!("expected a Notification, got {ev:?}");
        };
        assert_eq!(text, "agent online");
    }

    #[test]
    fn try_recv_is_empty_until_something_is_sent() {
        // The loop relies on `try_recv` being non-blocking and reporting Empty
        // so an idle tick falls through without repainting.
        let (tx, mut rx) = channel();
        assert!(rx.try_recv().is_err());
        tx.send(AppEvent::Notification("hi".into())).expect("send");
        assert!(matches!(rx.try_recv(), Ok(AppEvent::Notification(_))));
    }
}
