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
    },
    /// An agent's PTY produced output and its pane should repaint. Coalesced by
    /// the loop into a single redraw per tick.
    AgentOutput { project_id: String, issue: String },
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
    /// per-issue activity line (from a `PostToolUse` hook).
    AgentAction {
        project_id: String,
        issue: String,
        action: String,
    },
    /// The supervisor finished tearing an agent down and dropped it from its live
    /// map. The cockpit drops it from the fleet view too, so the overview stays
    /// bounded and mirrors the supervisor instead of accreting dead agents.
    AgentReaped { project_id: String, issue: String },
}

impl AppEvent {
    /// The project an agent-lifecycle event belongs to, or `None` for a bare
    /// [`AppEvent::Notification`] (which isn't project-scoped). The render loop
    /// uses this to file each event under the right project's fleet.
    pub fn project_id(&self) -> Option<&str> {
        match self {
            AppEvent::Notification(_) => None,
            AppEvent::AgentSpawned { project_id, .. }
            | AppEvent::AgentOutput { project_id, .. }
            | AppEvent::AgentExited { project_id, .. }
            | AppEvent::AgentNeedsYou { project_id, .. }
            | AppEvent::AgentStatusChanged { project_id, .. }
            | AppEvent::AgentAction { project_id, .. }
            | AppEvent::AgentReaped { project_id, .. } => Some(project_id),
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
