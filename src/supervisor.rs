//! Multi-agent supervisor + launch flow.
//!
//! One supervisor task owns the fleet. The cockpit drives it with fire-and-
//! forget [`SupervisorHandle`] commands (launch / cancel / shutdown); the
//! supervisor does the blocking setup (git worktree, session record, hook
//! settings), spawns the agent through an injected [`SpawnFn`] (real `claude`
//! in production, a fake in tests), and tracks each agent as a task under a
//! [`TaskTracker`] with its own child [`CancellationToken`].
//!
//! * **Launch** on an issue → worktree + branch → session record (a fresh
//!   `--session-id`, or `--resume` if we've launched it before) → spawn.
//! * **Cancel** one agent → cancel its child token → its task tears the backend
//!   down, leaving the others running.
//! * **Shutdown** (cockpit quit) → cancel the parent token, then await every
//!   agent task so all process groups are killed before we restore the terminal.

use std::collections::HashMap;
use std::collections::HashMap as StdHashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::backend::{AgentBackend, Lifecycle, SpawnConfig, SpawnFn};
use crate::event::{AppEvent, AppEventTx};
use crate::registry::{Layout, RepoEntry, ScratchSpec};
use crate::session::{AgentStatus, SessionStore};
use crate::worktree::WorktreeManager;

/// Everything `supervise()` needs to materialise a launch's **secondary** repos
/// (the up-front multi-select beyond the primary, ENG-536) on the blocking pool:
/// the `~/.lindep` [`Layout`], this project's handle + branch namespace, the always-
/// materialised `primary`, and the resolved candidate [`RepoEntry`]s keyed by handle
/// (the trust boundary — only a candidate can ever be checked out). The primary's
/// own L2 clone + worktree manager already exist on [`SupervisorConfig::worktree`];
/// this is what lets a per-issue launch clone the *rest* of its chosen set.
#[derive(Clone)]
pub struct RepoProvision {
    pub layout: Layout,
    pub project_handle: String,
    pub branch_prefix: String,
    /// Git ref a brand-new per-issue branch forks from (`HEAD` unless the project
    /// set a `base_branch`). Resolved to a fresh `origin/<base>` at create time.
    pub base: String,
    pub primary: String,
    pub candidates: StdHashMap<String, RepoEntry>,
    /// The project's declared scratch datastores (ENG-561), provisioned per issue at
    /// launch and recorded for teardown. Empty for a project with no `[[scratch]]`.
    pub scratch: Vec<ScratchSpec>,
}

impl RepoProvision {
    /// Normalise a launch's selected handle set: the `primary` is always first
    /// (it's always materialised), declared order is otherwise preserved, duplicates
    /// are dropped, and any handle that isn't a candidate is fenced out — so even a
    /// stale or forged selection can only ever check out repos in the trust boundary.
    /// An empty selection (every legacy / single-repo launch) collapses to just the
    /// primary, so the single-repo path is unchanged.
    fn normalize(&self, selected: &[String]) -> Vec<String> {
        let mut out = vec![self.primary.clone()];
        for handle in selected {
            if handle != &self.primary
                && self.candidates.contains_key(handle)
                && !out.contains(handle)
            {
                out.push(handle.clone());
            }
        }
        out
    }
}

/// Everything the supervisor needs to launch and host agents.
pub struct SupervisorConfig {
    /// The Linear project this supervisor owns the fleet for. Tags every event
    /// it emits so the cockpit files them under the right project.
    pub project_id: String,
    pub worktree: WorktreeManager,
    /// How to materialise a launch's secondary repos beyond the primary (ENG-536).
    pub provision: RepoProvision,
    pub store: Arc<Mutex<SessionStore>>,
    pub events: AppEventTx,
    /// How to spawn a backend — injected so tests can use a fake.
    pub spawn: Arc<SpawnFn>,
    /// Absolute path to this binary, for the hook-forwarder command.
    pub exe: PathBuf,
    /// Loopback port the hook endpoint listens on.
    pub hook_port: u16,
    /// Per-run bearer token agents echo back so the endpoint can reject forged
    /// hooks from other local processes.
    pub hook_token: String,
    /// Directory for per-issue hook settings files (`.lindep/hooks`).
    pub hooks_dir: PathBuf,
    /// Git ref each worktree forks from (e.g. `HEAD`).
    pub base: String,
    /// Initial PTY size; attach resizes to the real pane later.
    pub rows: u16,
    pub cols: u16,
    /// Most agents allowed at once, enforced **workspace-wide**: every project's
    /// supervisor shares the same cap and the same `live_count`, so the ceiling
    /// is N agents across all projects, not N per project.
    pub max_concurrent: usize,
    /// Shared count of live agents across the whole workspace (every supervisor
    /// increments on a launch it accepts and decrements on reap). Checked against
    /// `max_concurrent` before each launch so the cap spans all projects.
    pub live_count: Arc<AtomicUsize>,
    /// Extra `claude` args applied to every launch (e.g. `--permission-mode`).
    pub guardrails: Vec<String>,
}

/// Grace period to let a cancelled agent exit on SIGTERM before we SIGKILL it.
#[cfg(not(test))]
const KILL_GRACE: Duration = Duration::from_secs(3);
/// Shorter under test so the SIGKILL-escalation path runs fast instead of
/// stalling the suite for the full production grace.
#[cfg(test)]
const KILL_GRACE: Duration = Duration::from_millis(300);

/// How often to poll an agent's lifecycle while waiting for it to exit.
const EXIT_POLL: Duration = Duration::from_millis(25);

/// Commands the supervisor processes. `Reaped` is internal — an agent task
/// sends it once teardown is complete so the supervisor can drop the record.
enum Command {
    Launch {
        issue: String,
        title: String,
        /// The `(rows, cols)` the cockpit will render this agent's pane at, so the
        /// PTY is created at its real tile size and claude's first paint already
        /// fits — `None` falls back to the supervisor's configured size. Avoids the
        /// stale full-width frame that lingers until claude processes the SIGWINCH
        /// (worst when a chat is opened beside an already-pinned coin).
        size: Option<(u16, u16)>,
        /// The up-front-selected repo handles to materialise for this issue beyond
        /// the always-checked-out primary (ENG-536). Empty for a single-repo launch
        /// (or any caller that doesn't multi-select), which collapses to the primary.
        repos: Vec<String>,
    },
    Cancel {
        issue: String,
    },
    /// An agent task finished tearing down; remove it if it's still the current
    /// generation for that issue (guards a reap from racing a relaunch).
    Reaped {
        issue: String,
        generation: u64,
    },
    Shutdown,
}

/// Cheap, cloneable handle the cockpit holds to drive the supervisor.
#[derive(Clone)]
pub struct SupervisorHandle {
    cmd_tx: mpsc::UnboundedSender<Command>,
}

impl SupervisorHandle {
    /// Launch an agent on `issue` (no-op if already running or at capacity). `size`
    /// = `(rows, cols)` is the tile the cockpit will render the agent in, so the
    /// PTY starts at its real size and claude's first paint already fits — no
    /// reflow flash. `None` falls back to the supervisor's configured size.
    ///
    /// Production launches always carry a repo selection (the workspace forwards
    /// [`launch_with_repos`](Self::launch_with_repos)); this no-repos convenience is
    /// the single-repo shorthand the supervisor's own tests drive it with.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "production goes through launch_with_repos; this is the tests' single-repo shorthand"
        )
    )]
    pub fn launch(&self, issue: String, title: String, size: Option<(u16, u16)>) {
        self.launch_with_repos(issue, title, size, Vec::new());
    }
    /// Launch an agent with an explicit up-front repo selection (ENG-536). The
    /// primary is always materialised; `repos` adds the rest of the chosen set.
    pub fn launch_with_repos(
        &self,
        issue: String,
        title: String,
        size: Option<(u16, u16)>,
        repos: Vec<String>,
    ) {
        let _ = self.cmd_tx.send(Command::Launch {
            issue,
            title,
            size,
            repos,
        });
    }

    /// Launch a synthetic ad-hoc agent. The supervisor's spawn path is identical
    /// to a normal issue launch; callers own the synthetic id and cleanup policy.
    pub fn launch_ask(
        &self,
        issue: String,
        title: String,
        size: Option<(u16, u16)>,
        repos: Vec<String>,
    ) {
        self.launch_with_repos(issue, title, size, repos);
    }
    /// Stop a single agent, leaving the others running.
    pub fn cancel(&self, issue: String) {
        let _ = self.cmd_tx.send(Command::Cancel { issue });
    }
    /// Begin a clean shutdown of all agents. Pair with awaiting the supervisor's
    /// `JoinHandle` so the process waits for every agent to die.
    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(Command::Shutdown);
    }
}

/// Per-agent bookkeeping kept by the supervisor. `generation` distinguishes
/// successive launches of the same issue so a late reap can't drop a fresh one.
struct AgentRecord {
    generation: u64,
    token: CancellationToken,
    /// Set once a cancel is requested. The record lingers through the whole
    /// SIGTERM→grace→SIGKILL teardown (so a relaunch can't race the still-dying
    /// process for the same worktree + session id); this flag lets `launch`
    /// distinguish "still stopping" from "already running" in its message.
    cancelling: bool,
}

/// Everything one agent task needs, cloned out of the config at launch so the
/// command loop never blocks on a launch's setup.
struct AgentTask {
    project_id: String,
    issue: String,
    title: String,
    generation: u64,
    token: CancellationToken,
    /// A handle to the supervisor's *parent* token (the one a shutdown cancels).
    /// Lets a tearing-down agent tell a whole-cockpit shutdown (parent cancelled,
    /// cascading to this child) from a per-agent kill (only its own child token
    /// cancelled), so it can preserve a waiting agent's `NeedsYou` across the
    /// former — see `grade_teardown`.
    parent: CancellationToken,
    worktree: WorktreeManager,
    /// The up-front-selected repo set for this launch (ENG-536). Empty → primary only.
    repos: Vec<String>,
    /// How to materialise the secondary repos beyond the primary.
    provision: RepoProvision,
    store: Arc<Mutex<SessionStore>>,
    events: AppEventTx,
    spawn: Arc<SpawnFn>,
    exe: PathBuf,
    hook_port: u16,
    hook_token: String,
    hooks_dir: PathBuf,
    base: String,
    rows: u16,
    cols: u16,
    guardrails: Vec<String>,
    reap_tx: mpsc::UnboundedSender<Command>,
}

/// The fleet owner. Lives entirely inside its own task; state is never shared.
pub struct Supervisor {
    cfg: SupervisorConfig,
    agents: HashMap<String, AgentRecord>,
    parent: CancellationToken,
    tracker: TaskTracker,
    /// Clone of the command sender, handed to each agent task so it can report
    /// its own completion (`Reaped`) back to the loop.
    self_tx: mpsc::UnboundedSender<Command>,
    next_generation: u64,
}

impl Supervisor {
    /// Spawn the supervisor task on `handle` and return a handle to drive it
    /// plus the task's `JoinHandle` (await it after `shutdown()` to block until
    /// every agent's process has actually been torn down).
    pub fn start(cfg: SupervisorConfig, handle: &Handle) -> (SupervisorHandle, JoinHandle<()>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let supervisor = Supervisor {
            cfg,
            agents: HashMap::new(),
            parent: CancellationToken::new(),
            tracker: TaskTracker::new(),
            self_tx: cmd_tx.clone(),
            next_generation: 0,
        };
        let join = handle.spawn(supervisor.run(cmd_rx));
        (SupervisorHandle { cmd_tx }, join)
    }

    async fn run(mut self, mut cmd_rx: mpsc::UnboundedReceiver<Command>) {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                Command::Launch {
                    issue,
                    title,
                    size,
                    repos,
                } => self.launch(issue, title, size, repos),
                Command::Cancel { issue } => self.cancel(&issue),
                Command::Reaped { issue, generation } => self.reap(&issue, generation),
                // Stop receiving and fall through to teardown. Reaping is
                // intentionally skipped from here on: any `Reaped` an agent task
                // still sends is dropped (nothing drains `cmd_rx` after the break),
                // and the `agents` map is never cleared. That's by design —
                // `tracker.wait()` below is the sole teardown authority, awaiting
                // each agent task (which kills its backend) so no process leaks.
                // Do NOT add map-cleanup-on-reap assumptions to this path.
                Command::Shutdown => break,
            }
        }
        // Cancel every agent and wait for each task to finish tearing its backend
        // down (SIGTERM, then SIGKILL if needed) so no process group outlives the
        // cockpit. `tracker.wait()` returns only after every task has exited.
        self.parent.cancel();
        self.tracker.close();
        self.tracker.wait().await;
    }

    /// Record an agent and hand its whole lifecycle to a tracked task. This is
    /// synchronous: the blocking worktree/spawn work happens inside the task, so
    /// a slow `git` never stalls cancel/shutdown of other agents.
    fn launch(
        &mut self,
        issue: String,
        title: String,
        size: Option<(u16, u16)>,
        repos: Vec<String>,
    ) {
        if let Some(record) = self.agents.get(&issue) {
            // A cancelled record lingers until its task confirms teardown; tell
            // the user it's still stopping rather than the misleading "already
            // running" — the relaunch will take once the matching `Reaped` lands.
            if record.cancelling {
                self.reject_launch(
                    &issue,
                    format!("still stopping {issue}, try again in a moment"),
                );
            } else {
                self.reject_launch(&issue, format!("{issue} already has a running agent"));
            }
            return;
        }
        // Workspace-wide capacity: reserve a slot in the shared counter with a
        // single atomic compare-and-increment, so the cap holds even when two
        // projects' supervisors launch concurrently. A plain load-then-add would
        // race across the independent supervisor tasks and overshoot the ceiling;
        // `fetch_update` commits the increment only when the value was still under
        // the cap, closing that window. Released in `reap` when the record drops.
        if self
            .cfg
            .live_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.cfg.max_concurrent).then_some(n + 1)
            })
            .is_err()
        {
            self.reject_launch(
                &issue,
                format!(
                    "at capacity ({} agents across the workspace) — cancel one first",
                    self.cfg.max_concurrent
                ),
            );
            return;
        }

        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        let token = self.parent.child_token();
        // The reserved slot is released in `reap` when this record is dropped (a
        // cancelling record lingers until teardown, so it keeps its slot).
        self.agents.insert(
            issue.clone(),
            AgentRecord {
                generation,
                token: token.clone(),
                cancelling: false,
            },
        );

        let task = AgentTask {
            project_id: self.cfg.project_id.clone(),
            issue,
            title,
            generation,
            token,
            parent: self.parent.clone(),
            worktree: self.cfg.worktree.clone(),
            repos,
            provision: self.cfg.provision.clone(),
            store: Arc::clone(&self.cfg.store),
            events: self.cfg.events.clone(),
            spawn: Arc::clone(&self.cfg.spawn),
            exe: self.cfg.exe.clone(),
            hook_port: self.cfg.hook_port,
            hook_token: self.cfg.hook_token.clone(),
            hooks_dir: self.cfg.hooks_dir.clone(),
            base: self.cfg.base.clone(),
            // The cockpit's pane size when it knows it, else the configured default.
            rows: size.map_or(self.cfg.rows, |(r, _)| r.max(1)),
            cols: size.map_or(self.cfg.cols, |(_, c)| c.max(1)),
            guardrails: self.cfg.guardrails.clone(),
            reap_tx: self.self_tx.clone(),
        };
        self.tracker.spawn(run_agent(task));
    }

    /// Signal one agent to stop. The record stays until its task reports back via
    /// `Reaped`, so a relaunch can't race the still-dying process for the same
    /// worktree + session id.
    fn cancel(&mut self, issue: &str) {
        if let Some(record) = self.agents.get_mut(issue) {
            record.token.cancel();
            record.cancelling = true;
            self.notify(format!("cancelling agent on {issue}"));
        } else {
            self.notify(format!("no agent running on {issue}"));
        }
    }

    /// Drop a finished agent's record, but only if it's still the generation that
    /// reported in (a relaunch bumps the generation, so a stale reap is ignored).
    fn reap(&mut self, issue: &str, generation: u64) {
        if self
            .agents
            .get(issue)
            .is_some_and(|r| r.generation == generation)
        {
            self.agents.remove(issue);
            // Release the workspace slot this record held.
            self.cfg.live_count.fetch_sub(1, Ordering::Relaxed);
            // Tell the cockpit the agent is fully gone so it can drop the fleet
            // entry (bounds the overview; keeps it in step with our live map).
            let _ = self.cfg.events.send(AppEvent::AgentReaped {
                project_id: self.cfg.project_id.clone(),
                issue: issue.to_string(),
            });
        }
    }

    fn notify(&self, message: String) {
        let _ = self.cfg.events.send(AppEvent::Notification(message));
    }

    /// Refuse a launch for a specific `issue`, carrying the id so the cockpit drops
    /// only that issue's double-press guard rather than everyone's (M10).
    fn reject_launch(&self, issue: &str, reason: String) {
        let _ = self.cfg.events.send(AppEvent::LaunchRejected {
            issue: issue.to_string(),
            reason,
        });
    }
}

/// Post-mortem status for a torn-down agent, graded off the pre-teardown
/// lifecycle snapshot (so our own SIGTERM/SIGKILL can't recolour the verdict).
///
/// A self-exit is graded by its code: `Done` (0 / signalled) or `Failed`. A
/// *cancel* of a still-running process is `Stopped` — dead but resumable —
/// EXCEPT when the whole cockpit is shutting down (`parent_cancelled`) and the
/// agent was waiting on you (`prior == NeedsYou`): then `NeedsYou` is preserved
/// on disk so the next launch's startup `⚑` flags the project that was waiting.
/// A per-agent kill (only the agent's own child token cancelled, parent not)
/// is always `Stopped` — you deliberately stopped it.
fn grade_teardown(
    pre_shutdown: Lifecycle,
    parent_cancelled: bool,
    prior: Option<AgentStatus>,
) -> AgentStatus {
    match pre_shutdown {
        Lifecycle::Running if parent_cancelled && prior == Some(AgentStatus::NeedsYou) => {
            AgentStatus::NeedsYou
        }
        Lifecycle::Running => AgentStatus::Stopped,
        Lifecycle::Exited(Some(0)) | Lifecycle::Exited(None) => AgentStatus::Done,
        Lifecycle::Exited(Some(_)) => AgentStatus::Failed,
    }
}

/// One agent's whole lifecycle: set up its worktree/session/hooks, spawn the
/// backend, supervise it until cancel-or-exit, tear it down, report status, and
/// always reap its record so the issue can be launched again.
async fn run_agent(task: AgentTask) {
    supervise(&task).await;
    // Whatever happened — success, setup failure, cancel, or crash — free the
    // slot so this issue can be relaunched (and resumed).
    let _ = task.reap_tx.send(Command::Reaped {
        issue: task.issue.clone(),
        generation: task.generation,
    });
}

/// What the materialise pass returns: the worktrees that came up (handle → worktree),
/// paired with per-secondary failure messages (skipped, not fatal — see the loop).
type MaterializeOutcome = Result<(Vec<(String, crate::worktree::Worktree)>, Vec<String>), String>;

/// Best-effort teardown of freshly-provisioned scratch records on the blocking pool —
/// rolls back what a launch created when it then aborts (cancel, or a `required`
/// failure), so an external resource isn't orphaned before it's ever recorded on the
/// session. Reused (pre-existing persisted) resources are filtered out by the caller.
async fn rollback_scratch(records: Vec<crate::session::ScratchRecord>) {
    if records.is_empty() {
        return;
    }
    let _ = tokio::task::spawn_blocking(move || {
        for record in &records {
            let _ = crate::scratch::teardown(record);
        }
    })
    .await;
}

async fn supervise(task: &AgentTask) {
    let notify = |msg: String| {
        let _ = task.events.send(AppEvent::Notification(msg));
    };
    let reject = |reason: String| {
        let _ = task.events.send(AppEvent::LaunchRejected {
            issue: task.issue.clone(),
            reason,
        });
    };

    // Materialise every selected repo (primary + the up-front multi-select,
    // ENG-536) on the blocking pool: each repo's L2 reference clone is ensured
    // (idempotent, fsck self-healing) and its per-issue worktree created, serially —
    // which sidesteps the mirror-lock race entirely. The primary's clone + manager
    // already exist (build_plane provisioned them); secondary repos are cloned and
    // re-rooted here, each getting this run's post-commit hook. git is slow and
    // un-abortable, so a cancel/shutdown mid-clone stops awaiting and returns (the
    // detached thread finishes on its own) — keeping teardown responsive exactly as
    // the single-repo path did.
    // The issue's prior durable repo set, read once up front — unioned into the
    // materialise selection (so a resume brings back lazily-pulled repos), and unioned
    // again into the persisted set below (so a repo dropped from the candidate list by
    // a registry edit is RETAINED in the record, never silently orphaned).
    let existing_repos: Vec<String> = task
        .store
        .lock()
        .ok()
        .and_then(|s| s.get(&task.issue).map(|sess| sess.repos.clone()))
        .unwrap_or_default();
    // The issue's prior scratch records (ENG-561), read once: lets a `persist`ed
    // resource reuse its recorded port across a resume, and lets us retain a record
    // whose spec was removed from the registry so discard still tears it down.
    let existing_scratch: Vec<crate::session::ScratchRecord> = task
        .store
        .lock()
        .ok()
        .and_then(|s| s.get(&task.issue).map(|sess| sess.scratch.clone()))
        .unwrap_or_default();
    let materialize = {
        let provision = task.provision.clone();
        let primary_mgr = task.worktree.clone();
        // The set to materialise is the launch-time selection UNIONED with the issue's
        // durable repo set — so a resume brings back every repo a mid-session lazy-pull
        // (ENG-542) added, not just the original up-front pick. normalize() dedups,
        // pins the primary first, and fences to candidates.
        let selected = {
            let mut requested = task.repos.clone();
            requested.extend(existing_repos.iter().cloned());
            provision.normalize(&requested)
        };
        let issue = task.issue.clone();
        let title = task.title.clone();
        let base = task.base.clone();
        let exe = task.exe.to_string_lossy().into_owned();
        let port = task.hook_port;
        let token = task.hook_token.clone();
        tokio::task::spawn_blocking(move || -> MaterializeOutcome {
            let mut out = Vec::with_capacity(selected.len());
            // Secondary-repo failures are collected, not fatal: a launch must not
            // be lost because ONE secondary repo can't be materialised (offline,
            // its mirror reclaimed, …). Only the primary is required — it anchors
            // the agent's cwd and branch. A skipped repo stays in the durable set
            // recorded below, so the next resume retries it.
            let mut failures: Vec<String> = Vec::new();
            for handle in &selected {
                let is_primary = handle == &provision.primary;
                let step = (|| -> Result<crate::worktree::Worktree, String> {
                    let mgr = if is_primary {
                        primary_mgr.clone()
                    } else {
                        let entry = provision.candidates.get(handle).ok_or_else(|| {
                            format!("repo `{handle}` is not a candidate of this project")
                        })?;
                        // Best-effort, throttled mirror refresh so a (re)built clone
                        // forks off current upstream rather than the mirror's state at
                        // first create (the stale-base concern).
                        let _ = crate::mirror::refresh_mirror(&provision.layout, entry);
                        let clone = crate::mirror::ensure_clone(
                            &provision.layout,
                            &provision.project_handle,
                            entry,
                        )
                        .map_err(|e| e.to_string())?;
                        // Install/refresh this repo's auto-push hook with THIS run's
                        // port + token (the stale-port trap), like the primary's.
                        let _ = crate::notify::write_post_commit_hook(
                            &clone, &exe, port, &token, handle,
                        );
                        let worktrees_root =
                            provision.layout.worktrees_dir(&provision.project_handle);
                        WorktreeManager::with_layout(
                            &clone,
                            &provision.branch_prefix,
                            &worktrees_root,
                            handle,
                        )
                        .map_err(|e| e.to_string())?
                    };
                    mgr.create(&issue, &title, &base).map_err(|e| e.to_string())
                })();
                match step {
                    Ok(wt) => out.push((handle.clone(), wt)),
                    // The primary is required — propagate its failure as fatal.
                    Err(e) if is_primary => return Err(e),
                    Err(e) => failures.push(format!("repo `{handle}` skipped — {e}")),
                }
            }
            Ok((out, failures))
        })
    };
    let (materialized, repo_failures) = tokio::select! {
        // A cancel/shutdown during the (blocking, un-abortable) git op must not
        // pin teardown behind it: stop awaiting and return so `tracker.wait()`
        // stays responsive. The detached blocking thread finishes git on its own
        // and the runtime reclaims it — we just don't gate shutdown on it.
        () = task.token.cancelled() => return,
        res = materialize => match res {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return reject(format!("workspace for {} failed: {e}", task.issue)),
            Err(e) => return reject(format!("workspace task for {} panicked: {e}", task.issue)),
        },
    };
    // A skipped secondary isn't fatal, but the human should know its tools won't be
    // mounted until a later resume retries it.
    for failure in &repo_failures {
        notify(format!("{}: {failure}", task.issue));
    }

    // Cancelled just as git finished? Bail before spawning a process.
    if task.token.is_cancelled() {
        return;
    }

    // The primary worktree anchors the session record (its branch, and — for a
    // single-repo issue — its path as the agent cwd). A multi-repo issue runs in
    // the shared per-issue workspace parent (`worktrees/<ISSUE>`) with each repo a
    // sibling subdir, told apart by a generated `WORKSPACE.md` and reachable via one
    // `--add-dir` per repo so claude has tool access to each.
    let primary_wt = materialized
        .iter()
        .find(|(h, _)| h == &task.provision.primary)
        .map(|(_, w)| w.clone())
        .unwrap_or_else(|| materialized[0].1.clone());
    let multi = materialized.len() > 1;
    let workspace_dir = task
        .provision
        .layout
        .issue_workspace_dir(&task.provision.project_handle, &task.issue);
    let cwd = if multi {
        workspace_dir.clone()
    } else {
        primary_wt.path.clone()
    };
    let materialized_handles: Vec<String> = materialized.iter().map(|(h, _)| h.clone()).collect();
    let add_dirs: Vec<String> = if multi {
        materialized
            .iter()
            .map(|(_, w)| w.path.to_string_lossy().into_owned())
            .collect()
    } else {
        Vec::new()
    };
    if multi {
        let repos: Vec<(String, String)> = materialized
            .iter()
            .map(|(h, w)| (h.clone(), w.branch.clone()))
            .collect();
        if let Err(e) = crate::worktree::write_workspace_md(&workspace_dir, &task.issue, &repos) {
            notify(format!("WORKSPACE.md for {} not written: {e}", task.issue));
        }
    }

    // ── Scratch datastores (ENG-561) ──────────────────────────────────────────
    // Provision the project's [[scratch]] resources for this issue on the blocking
    // pool (they shell out) so the agent's app/tests get an isolated DB / ports. The
    // resolved env is injected into the spawn below; the records are persisted so
    // teardown/sweep can drop them. Best-effort per resource unless `required`; a
    // `persist`ed resource reuses its recorded port across a resume.
    let mut scratch_env: Vec<(String, String)> = Vec::new();
    let mut scratch_records: Vec<crate::session::ScratchRecord> = Vec::new();
    if !task.provision.scratch.is_empty() {
        let specs = task.provision.scratch.clone();
        let prior = existing_scratch.clone();
        let ctx = crate::scratch::Context {
            issue: task.issue.clone(),
            project: task.provision.project_handle.clone(),
            workspace: workspace_dir.clone(),
        };
        // Await the provision to completion rather than racing the cancel token: a
        // scratch resource is EXTERNAL (a DB/container/port), not on-disk under the
        // worktree, so unlike the git materialize there's no reconcile sweep to mop it
        // up — abandoning a mid-flight provision would orphan whatever the detached
        // thread then creates. The wait is bounded by one provision; on cancel we roll
        // back what THIS pass freshly created (a reused persisted resource pre-dates the
        // launch and is left alone) and bail before spawning.
        let provisioned = match tokio::task::spawn_blocking(move || {
            crate::scratch::provision_all(&specs, &ctx, &prior)
        })
        .await
        {
            Ok(v) => v,
            Err(e) => return reject(format!("scratch task for {} panicked: {e}", task.issue)),
        };
        if task.token.is_cancelled() {
            rollback_scratch(
                provisioned
                    .into_iter()
                    .filter_map(Result::ok)
                    .filter(|p| !p.reused)
                    .map(|p| p.record)
                    .collect(),
            )
            .await;
            return;
        }
        let mut ok: Vec<crate::scratch::Provisioned> = Vec::new();
        let mut fatal: Option<String> = None;
        for outcome in provisioned {
            match outcome {
                Ok(p) => ok.push(p),
                // `required` → the agent can't run without it; abort the launch.
                Err(e) if e.required => {
                    if fatal.is_none() {
                        fatal = Some(format!(
                            "{}: scratch `{}` failed: {}",
                            task.issue, e.name, e.message
                        ));
                    }
                }
                // Best-effort: footer it and launch anyway.
                Err(e) => notify(format!(
                    "{}: scratch `{}` skipped — {}",
                    task.issue, e.name, e.message
                )),
            }
        }
        if let Some(msg) = fatal {
            // Roll back only what this pass freshly CREATED — a reused persisted
            // resource pre-dates the launch (the resume reconnected to it) and the
            // persist contract keeps it across an abort. Freshly-created records aren't
            // persisted yet, so without this they'd orphan.
            rollback_scratch(
                ok.iter()
                    .filter(|p| !p.reused)
                    .map(|p| p.record.clone())
                    .collect(),
            )
            .await;
            return reject(msg);
        }
        for p in ok {
            scratch_env.extend(p.env);
            scratch_records.push(p.record);
        }
    }

    // Session record: deterministic id, resume if we've launched this before.
    let (session_id, resume, snapshot) = {
        let Ok(mut store) = task.store.lock() else {
            return reject("session store lock poisoned".to_string());
        };
        let resume = store.get(&task.issue).is_some();
        let session = store.ensure(&task.issue, cwd.clone(), primary_wt.branch.clone());
        let session_id = session.session_id.clone();
        store.set_status(&task.issue, AgentStatus::Spawning);
        // Record the materialised repo set (ENG-536) so a restart rehydrates exactly
        // these repos (ENG-540) and the lazy-pull (ENG-542) extends a known set. Union
        // with the prior durable set so a repo that fell out of the candidate list
        // (a registry edit) keeps its place in the record (its worktree stays tracked
        // for teardown) rather than being silently dropped.
        let mut durable = existing_repos; // last use — move, don't clone
        for h in &materialized_handles {
            if !durable.contains(h) {
                durable.push(h.clone());
            }
        }
        store.set_repos(&task.issue, durable);
        // Record the scratch resources (ENG-561), unioned with any prior record whose
        // spec was removed from the registry — so discard still tears that resource
        // down rather than orphaning it (same retention discipline as repos).
        let mut durable_scratch = scratch_records;
        for prior in existing_scratch {
            if !durable_scratch.iter().any(|r| r.name == prior.name) {
                durable_scratch.push(prior);
            }
        }
        store.set_scratch(&task.issue, durable_scratch);
        // Snapshot (+ ordering seq) under the lock; persist after dropping it so
        // blocking fs I/O never runs under the store mutex (a hook waiting on the
        // same lock must not block behind a disk rename).
        let snapshot = store
            .snapshot_with_seq()
            .ok()
            .map(|(b, seq)| (store.path().to_path_buf(), b, seq));
        (session_id, resume, snapshot)
    };
    if let Some((path, bytes, seq)) = snapshot {
        crate::session::persist_snapshot(&task.events, path, bytes, seq).await;
    }

    // Hook settings so this agent's notifications find their way back to us.
    // Written on the blocking pool: create_dir_all + open + write + chmod are
    // blocking fs syscalls, and `.lindep` can be NFS-backed/contended/near-full.
    // Parking one of the 2 async workers (see event.rs) on that would stall the
    // supervisor command loop and the hook accept loop — the same reason the
    // worktree create and the state persist above run off the workers.
    let settings = task.hooks_dir.join(format!("{}.settings.json", task.issue));
    let write = {
        let settings = settings.clone();
        let exe = task.exe.to_string_lossy().to_string();
        let port = task.hook_port;
        let hook_token = task.hook_token.clone();
        tokio::task::spawn_blocking(move || {
            crate::notify::write_settings(&settings, &exe, port, &hook_token)
        })
        .await
    };
    match write {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return reject(format!("hook settings for {} failed: {e}", task.issue)),
        Err(e) => {
            return reject(format!(
                "hook settings task for {} panicked: {e}",
                task.issue
            ));
        }
    }

    // A `--resume` whose saved conversation has vanished ("No conversation found")
    // exits fast and non-zero; retry ONCE with a fresh `--session-id` (the
    // deterministic id recreates cleanly) rather than looping on the dead session.
    let mut resume = resume;
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        // A cancel that landed during the async setup (state persist, hook
        // settings write) or during a prior attempt's brief life must not fork a
        // process. The bail above only covers the git window; this re-check
        // covers every await since — and the missing-conversation retry's loop
        // re-entry. run_agent still sends `Reaped` on this early return, so the
        // issue stays relaunchable, exactly like the git-window bail.
        if task.token.is_cancelled() {
            return;
        }
        let mut spawn_cfg = SpawnConfig::claude(
            &task.project_id,
            &task.issue,
            cwd.clone(),
            &session_id,
            resume,
            task.rows,
            task.cols,
        )
        .arg("--settings")
        .arg(settings.to_string_lossy().to_string());
        // One `--add-dir` per repo so a multi-repo agent has full tool access to
        // each sibling worktree, not just its cwd (the workspace parent). Argv-baked,
        // so a repo added mid-session needs a resume for full access (ENG-542).
        for dir in &add_dirs {
            spawn_cfg = spawn_cfg.arg("--add-dir").arg(dir);
        }
        for guardrail in &task.guardrails {
            spawn_cfg = spawn_cfg.arg(guardrail);
        }
        // Hand the agent the cockpit's hook endpoint + its own identity so it can run
        // `lindep request-repo <handle>` (ENG-542 fenced lazy-pull) from inside its
        // workspace. These ride the agent's env (the spawn `env_clear`s, so they reach
        // the child only because we add them); the token only authorises loopback hook
        // POSTs the agent can already make, so exposing it here is no new capability.
        spawn_cfg
            .env
            .push(("LINDEP_HOOK_PORT".to_string(), task.hook_port.to_string()));
        spawn_cfg
            .env
            .push(("LINDEP_HOOK_TOKEN".to_string(), task.hook_token.clone()));
        spawn_cfg
            .env
            .push(("LINDEP_PROJECT".to_string(), task.project_id.clone()));
        spawn_cfg
            .env
            .push(("LINDEP_ISSUE".to_string(), task.issue.clone()));
        // Inject each scratch resource's resolved env (ENG-561) so the agent's app /
        // tests reach their isolated DB / ports with no app changes. Re-pushed every
        // attempt (the spawn consumes `spawn_cfg`), and re-resolved every launch — the
        // stale-port-trap rule, the same reason the hook port/token are re-pushed here.
        for (key, value) in &scratch_env {
            spawn_cfg.env.push((key.clone(), value.clone()));
        }

        let backend = match (task.spawn)(spawn_cfg, task.events.clone()) {
            Ok(backend) => backend,
            Err(e) => return reject(format!("spawning agent for {} failed: {e}", task.issue)),
        };
        let _ = task.events.send(AppEvent::AgentSpawned {
            project_id: task.project_id.clone(),
            issue: task.issue.clone(),
            backend: Arc::clone(&backend),
            repos: materialized_handles.clone(),
        });

        // The process is up but hasn't *done* anything yet, so it stays `Spawning`
        // — rendered "starting…" (a steady ◌, not the working spinner). We used to
        // flip to `Running` here, which made every fresh agent read as "working"
        // before it produced a thing. The hook bus now promotes it on real
        // activity: the first PostToolUse → Running ("working"), a Stop → Idle, a
        // permission prompt → NeedsYou. A genuinely wedged spawn simply stays
        // "starting…", which is honest — it never did anything.

        // Run until the user cancels or the agent exits on its own.
        let exit = backend.exit_notify();
        let cancelled = tokio::select! {
            () = task.token.cancelled() => true,
            () = exit.notified() => false,
        };

        // Snapshot the ground-truth lifecycle *before* we signal anything, so the
        // verdict below reflects what the process actually did rather than what our
        // teardown left behind. On a self-exit the wait thread has already recorded
        // `Exited(code)` before waking us; on a cancel this is `Running` unless the
        // process beat us to it (both select! arms ready) — in which case it carries
        // the real exit code, so a crash racing the cancel isn't laundered into Idle.
        //
        // One sub-microsecond window is knowingly left open: the wait thread sets
        // `reaped` *before* it writes `Exited` (so the killpg gate is conservative —
        // see backend::signal_group), so a cancel that reads `status()` in between
        // sees `Running` and grades a just-crashed agent `Stopped` rather than
        // `Failed`. Closing it would require writing `Exited` before `reaped`, which
        // trades a cosmetic mis-grade for a real recycled-pid-signal hazard — not a
        // good trade, so the window stays.
        let pre_shutdown = backend.status();

        if cancelled {
            backend.shutdown(); // SIGTERM the process group
            // Confirm death; escalate to SIGKILL if the agent ignores SIGTERM, so a
            // process group never outlives the cockpit. We poll the *monotonic*
            // lifecycle rather than awaiting `exit.notified()` again: the wait thread
            // fires its one permit only once, and the select! above may already have
            // consumed it (a self-exit racing the cancel), which would leave a second
            // `notified()` to hang for the whole grace window.
            if !await_exit(backend.as_ref()).await {
                backend.force_kill();
                await_exit(backend.as_ref()).await;
            }
        } else {
            backend.shutdown(); // already exited; idempotent
        }

        // A `--resume` that found no saved conversation self-exits non-zero with
        // claude's "No conversation found" banner. Relaunch once, fresh, before this
        // gets graded Failed (which would tombstone the issue and strand it in a loop
        // that keeps re-resuming the same missing session).
        if !cancelled
            && resume
            && attempt == 1
            && matches!(pre_shutdown, Lifecycle::Exited(Some(code)) if code != 0)
            && missing_conversation(backend.as_ref()).await
        {
            resume = false;
            notify(format!(
                "{}: saved conversation gone — starting a fresh session",
                task.issue
            ));
            continue;
        }

        // The agent task is the sole authority on post-mortem status, graded off the
        // pre-teardown snapshot so our own SIGTERM/SIGKILL can't recolour the verdict.
        // A self-exit is graded by its exit code (Done/Failed). A deliberate cancel of
        // a still-running process is Stopped — dead but resumable — but a cancel that
        // lost the race to the process's own exit honours the real exit code, so a
        // non-zero crash reads Failed, never a laundered Stopped. Stopped is distinct
        // from Idle (resting *but still up*), so the header stops counting a cancelled
        // agent as live the instant you stop it. (The backend's own AgentExited event
        // only drives the footer + frees the render handle.)
        // The store's last-known status, read before grading so a whole-cockpit
        // shutdown can preserve a waiting agent's NeedsYou (see `grade_teardown`).
        let prior = task
            .store
            .lock()
            .ok()
            .and_then(|s| s.get(&task.issue).map(|sess| sess.status));
        let status = grade_teardown(pre_shutdown, task.parent.is_cancelled(), prior);
        crate::session::mutate_and_persist(&task.store, &task.events, |store| {
            store.set_status(&task.issue, status);
        })
        .await;
        let _ = task.events.send(AppEvent::AgentStatusChanged {
            project_id: task.project_id.clone(),
            issue: task.issue.clone(),
            status,
        });
        break;
    }
}

/// Whether the agent's screen shows claude's "No conversation found" banner — the
/// failure mode of `--resume <id>` when that conversation has been deleted. Polled
/// for up to ~1 s because the wait thread can fire the exit notify before the read
/// pump has drained the final banner into the parser; the generous window keeps the
/// retry-fresh decision robust even under load (a false negative just degrades to
/// the pre-fix behaviour — graded Failed, relaunchable). Returns early the moment
/// the banner appears. Cheap: only called once, on a resume launch's own non-zero
/// exit. A false positive is implausible (the exact phrase only appears on this
/// failure), so a fresh session is never started over a live conversation.
async fn missing_conversation(backend: &dyn AgentBackend) -> bool {
    for _ in 0..20 {
        let seen = backend
            .parser()
            .read()
            .map(|p| p.screen().contents().contains("No conversation found"))
            .unwrap_or(false);
        if seen {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    false
}

/// Wait up to [`KILL_GRACE`] for the agent's process to be confirmed gone,
/// polling its monotonic lifecycle (which only ever moves Running → Exited).
/// Returns whether it exited within the window. Polling, not the exit Notify,
/// is what makes this immune to a lost wakeup.
async fn await_exit(backend: &dyn AgentBackend) -> bool {
    tokio::time::timeout(KILL_GRACE, async {
        while !matches!(backend.status(), Lifecycle::Exited(_)) {
            tokio::time::sleep(EXIT_POLL).await;
        }
    })
    .await
    .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::AgentBackend;
    use crate::backend::fake::FakeBackend;
    use std::path::Path;
    use std::process::Command as Git;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    #[test]
    fn teardown_preserves_needs_you_only_on_whole_cockpit_shutdown() {
        use AgentStatus::*;
        // A per-agent kill of a waiting agent → Stopped (you stopped it).
        assert_eq!(
            grade_teardown(Lifecycle::Running, false, Some(NeedsYou)),
            Stopped
        );
        // A whole-cockpit shutdown of a waiting agent → NeedsYou preserved on disk,
        // so the next launch's startup ⚑ flags the project (ENG-562 review #8).
        assert_eq!(
            grade_teardown(Lifecycle::Running, true, Some(NeedsYou)),
            NeedsYou
        );
        // Shutdown of a live but non-waiting agent → Stopped (it wasn't waiting).
        assert_eq!(
            grade_teardown(Lifecycle::Running, true, Some(Running)),
            Stopped
        );
        assert_eq!(grade_teardown(Lifecycle::Running, true, None), Stopped);
        // Self-exit grading is unaffected by the shutdown flag.
        assert_eq!(
            grade_teardown(Lifecycle::Exited(Some(0)), true, Some(NeedsYou)),
            Done
        );
        assert_eq!(grade_teardown(Lifecycle::Exited(None), false, None), Done);
        assert_eq!(
            grade_teardown(Lifecycle::Exited(Some(2)), true, Some(NeedsYou)),
            Failed
        );
    }

    /// A throwaway git repo + manager, like the worktree tests use.
    fn temp_repo() -> (PathBuf, WorktreeManager) {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("lindep-sup-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            assert!(
                Git::new("git")
                    .arg("-C")
                    .arg(&dir)
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success(),
                "git {args:?}"
            );
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "Test"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        let mgr = WorktreeManager::with_prefix(&dir, "felix").unwrap();
        (dir, mgr)
    }

    /// Spawn fn that records every fake it creates (and the args it was given).
    type Registry = Arc<Mutex<Vec<(Arc<FakeBackend>, Vec<String>)>>>;
    fn recording_spawn(registry: Registry) -> Arc<SpawnFn> {
        Arc::new(move |cfg: SpawnConfig, _events: AppEventTx| {
            let fake = FakeBackend::new(&cfg.issue);
            registry
                .lock()
                .unwrap()
                .push((Arc::clone(&fake), cfg.args.clone()));
            Ok(fake as Arc<dyn AgentBackend>)
        })
    }

    /// Spawn fn whose fakes ignore SIGTERM, forcing the supervisor to escalate to
    /// `force_kill()` — used to exercise the SIGKILL path end to end.
    fn ignoring_spawn(registry: Registry) -> Arc<SpawnFn> {
        Arc::new(move |cfg: SpawnConfig, _events: AppEventTx| {
            let fake = FakeBackend::new_ignoring_sigterm(&cfg.issue);
            registry
                .lock()
                .unwrap()
                .push((Arc::clone(&fake), cfg.args.clone()));
            Ok(fake as Arc<dyn AgentBackend>)
        })
    }

    /// Spawn fn that always fails, to exercise the supervisor's spawn-failure path
    /// (and prove a reserved workspace slot is released when a launch can't spawn).
    fn failing_spawn() -> Arc<SpawnFn> {
        Arc::new(|_cfg: SpawnConfig, _events: AppEventTx| {
            Err(crate::backend::AgentError::Spawn {
                program: "claude".to_string(),
                detail: "synthetic spawn failure".to_string(),
            })
        })
    }

    /// How many times an issue has been spawned so far — a queried fact, not the
    /// positional ordering assumption the older tests baked into `registry[1]`.
    fn spawn_count(registry: &Registry, issue: &str) -> usize {
        registry
            .lock()
            .unwrap()
            .iter()
            .filter(|(b, _)| b.issue() == issue)
            .count()
    }

    /// The `nth` (0-based) spawn recorded for `issue`: its fake and the args it
    /// was launched with. Looks the record up by identity rather than by a global
    /// position, so it's robust to other issues interleaving in the registry.
    fn nth_spawn(registry: &Registry, issue: &str, nth: usize) -> (Arc<FakeBackend>, Vec<String>) {
        registry
            .lock()
            .unwrap()
            .iter()
            .filter(|(b, _)| b.issue() == issue)
            .nth(nth)
            .map(|(b, args)| (Arc::clone(b), args.clone()))
            .expect("a spawn for this issue at this index")
    }

    fn config(
        dir: &Path,
        wt: WorktreeManager,
        tx: AppEventTx,
        spawn: Arc<SpawnFn>,
        cap: usize,
    ) -> SupervisorConfig {
        SupervisorConfig {
            project_id: String::new(),
            worktree: wt,
            // Single-repo provisioning: `normalize` collapses an empty selection to
            // just the primary, which reuses `worktree` above — so the candidates
            // map is never consulted and the layout/handle only matter for multi-repo.
            provision: RepoProvision {
                layout: Layout::new(dir),
                project_handle: "test".to_string(),
                branch_prefix: "test".to_string(),
                base: "HEAD".to_string(),
                primary: "test".to_string(),
                candidates: StdHashMap::new(),
                scratch: Vec::new(),
            },
            store: Arc::new(Mutex::new(
                SessionStore::load(dir.join(".lindep").join("state.json")).unwrap(),
            )),
            events: tx,
            spawn,
            exe: PathBuf::from("lindep"),
            hook_port: 1,
            hook_token: String::new(),
            hooks_dir: dir.join(".lindep").join("hooks"),
            base: "HEAD".to_string(),
            rows: 24,
            cols: 80,
            max_concurrent: cap,
            live_count: Arc::new(AtomicUsize::new(0)),
            guardrails: vec![],
        }
    }

    async fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
        for _ in 0..200 {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        cond()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_spawn_releases_its_reserved_workspace_slot() {
        // launch() reserves a slot in the shared live_count BEFORE the agent task
        // runs. If the spawn fails, run_agent must still reap so the slot is freed —
        // otherwise every failed launch would permanently shrink the workspace cap.
        let (dir, wt) = temp_repo();
        let (tx, mut rx) = crate::event::channel();
        let cfg = config(&dir, wt, tx, failing_spawn(), 4);
        let live = Arc::clone(&cfg.live_count);
        let (handle, join) = Supervisor::start(cfg, &Handle::current());

        handle.launch("ENG-1".into(), "One".into(), None);

        // The spawn failure is typed to the rejected launch, so the cockpit can drop
        // only ENG-1's pending guard.
        let saw_failure = wait_for(|| {
            while let Ok(ev) = rx.try_recv() {
                if let AppEvent::LaunchRejected { issue, reason } = ev
                    && issue == "ENG-1"
                    && reason.contains("spawning agent for ENG-1 failed")
                {
                    return true;
                }
            }
            false
        })
        .await;
        assert!(saw_failure, "a failing spawn surfaces an error");

        // …and the reserved slot is handed back, so the cap isn't leaked.
        assert!(
            wait_for(|| live.load(Ordering::Relaxed) == 0).await,
            "the reserved workspace slot is released after the spawn fails"
        );

        handle.shutdown();
        join.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_agent_that_ignores_sigterm_is_escalated_to_force_kill() {
        let (dir, wt) = temp_repo();
        let (tx, mut rx) = crate::event::channel();
        let registry: Registry = Arc::new(Mutex::new(Vec::new()));
        let cfg = config(&dir, wt, tx, ignoring_spawn(Arc::clone(&registry)), 4);
        let store = Arc::clone(&cfg.store);
        let (handle, join) = Supervisor::start(cfg, &Handle::current());

        handle.launch("ENG-1".into(), "One".into(), None);
        assert!(
            wait_for(|| spawn_count(&registry, "ENG-1") == 1).await,
            "agent spawned"
        );
        let fake = nth_spawn(&registry, "ENG-1", 0).0;

        // The agent refuses SIGTERM, so `shutdown()` alone never ends it: the
        // supervisor must time out `await_exit` and escalate to SIGKILL. This is
        // the load-bearing "no process group outlives the cockpit" guarantee, and
        // the only test that actually drives `force_kill`.
        handle.cancel("ENG-1".into());
        assert!(
            wait_for(|| fake.force_kill_count() == 1).await,
            "supervisor escalated an unresponsive agent to force_kill"
        );
        assert!(
            matches!(fake.status(), Lifecycle::Exited(_)),
            "force_kill confirmed the process is gone"
        );

        // A cancel of a still-running process is graded a clean, resumable Stopped
        // (off the pre-teardown snapshot) and persisted as such — distinct from
        // Idle (resting but alive), so the cockpit stops counting it as live.
        assert!(
            wait_for(|| {
                while let Ok(ev) = rx.try_recv() {
                    if let AppEvent::AgentStatusChanged { issue, status, .. } = ev
                        && issue == "ENG-1"
                        && status == AgentStatus::Stopped
                    {
                        return true;
                    }
                }
                false
            })
            .await,
            "cancel of an unresponsive agent resolves to Stopped"
        );
        assert_eq!(
            store.lock().unwrap().get("ENG-1").map(|s| s.status),
            Some(AgentStatus::Stopped),
            "durable store records the resumable Stopped"
        );

        handle.shutdown();
        let _ = join.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn launches_several_cancels_one_and_shuts_the_rest_down() {
        let (dir, wt) = temp_repo();
        let (tx, mut rx) = crate::event::channel();
        let registry: Registry = Arc::new(Mutex::new(Vec::new()));
        let cfg = config(&dir, wt, tx, recording_spawn(Arc::clone(&registry)), 4);
        let store = Arc::clone(&cfg.store);
        let (handle, join) = Supervisor::start(cfg, &Handle::current());

        handle.launch("ENG-1".into(), "One".into(), None);
        handle.launch("ENG-2".into(), "Two".into(), None);

        let mut spawned = std::collections::HashSet::new();
        assert!(
            wait_for(|| {
                while let Ok(ev) = rx.try_recv() {
                    if let AppEvent::AgentSpawned { issue, .. } = ev {
                        spawned.insert(issue);
                    }
                }
                spawned.len() >= 2
            })
            .await,
            "both agents emitted AgentSpawned within the budget"
        );
        assert_eq!(spawned.len(), 2, "both agents launched");
        {
            let store = store.lock().unwrap();
            assert!(
                store.get("ENG-1").is_some() && store.get("ENG-2").is_some(),
                "both sessions recorded"
            );
        }
        assert!(dir.join(".lindep").join("worktrees").join("ENG-1").is_dir());

        // Each issue's fake is findable in the registry.
        let fake = |issue: &str| -> Arc<FakeBackend> {
            registry
                .lock()
                .unwrap()
                .iter()
                .find(|(b, _)| b.issue() == issue)
                .map(|(b, _)| Arc::clone(b))
                .unwrap()
        };
        let (fake1, fake2) = (fake("ENG-1"), fake("ENG-2"));

        handle.cancel("ENG-1".into());
        assert!(
            wait_for(|| fake1.shutdown_count() >= 1).await,
            "cancelled agent shut down"
        );
        assert_eq!(fake2.shutdown_count(), 0, "the other agent is untouched");

        handle.shutdown();
        join.await.unwrap();
        assert!(
            fake2.shutdown_count() >= 1,
            "shutdown stops the remaining agent"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relaunching_an_issue_resumes_its_session() {
        let (dir, wt) = temp_repo();
        let (tx, mut rx) = crate::event::channel();
        let registry: Registry = Arc::new(Mutex::new(Vec::new()));
        let cfg = config(&dir, wt, tx, recording_spawn(Arc::clone(&registry)), 4);
        let (handle, join) = Supervisor::start(cfg, &Handle::current());

        let count_spawns = |rx: &mut crate::event::AppEventRx| {
            let mut n = 0;
            while let Ok(ev) = rx.try_recv() {
                if matches!(ev, AppEvent::AgentSpawned { .. }) {
                    n += 1;
                }
            }
            n
        };

        handle.launch("ENG-1".into(), "One".into(), None);
        let mut spawns = 0;
        assert!(
            wait_for(|| {
                spawns += count_spawns(&mut rx);
                spawns >= 1
            })
            .await,
            "the first launch spawned"
        );
        // First launch is a fresh conversation.
        let (fake1, first_args) = nth_spawn(&registry, "ENG-1", 0);
        assert!(first_args.starts_with(&["--session-id".to_string()]));

        handle.cancel("ENG-1".into());
        assert!(
            wait_for(|| fake1.shutdown_count() >= 1).await,
            "the first agent tore down on cancel"
        );

        // Relaunch, re-sending until it takes: the cancelled record lingers until
        // its `Reaped` is processed, so an early relaunch is a no-op ("still
        // stopping") rather than a second spawn — retrying removes that timing
        // dependency instead of assuming the reap already landed.
        assert!(
            wait_for(|| {
                spawns += count_spawns(&mut rx);
                if spawns < 2 {
                    handle.launch("ENG-1".into(), "One".into(), None);
                }
                spawns >= 2
            })
            .await,
            "the relaunch spawned a second agent"
        );

        // The relaunch resumes the same session id rather than starting fresh.
        // Found by issue + index (the second ENG-1 spawn), not a global position.
        let (_fake2, args) = nth_spawn(&registry, "ENG-1", 1);
        assert_eq!(args.first().map(String::as_str), Some("--resume"));
        assert_eq!(
            args.get(1),
            Some(&SessionStore::session_id_for("", "ENG-1"))
        );

        handle.shutdown();
        join.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_agent_that_finishes_on_its_own_is_reaped_as_done() {
        let (dir, wt) = temp_repo();
        let (tx, mut rx) = crate::event::channel();
        let registry: Registry = Arc::new(Mutex::new(Vec::new()));
        let cfg = config(&dir, wt, tx, recording_spawn(Arc::clone(&registry)), 4);
        let store = Arc::clone(&cfg.store);
        let (handle, join) = Supervisor::start(cfg, &Handle::current());

        handle.launch("ENG-1".into(), "One".into(), None);
        let mut seen = false;
        assert!(
            wait_for(|| {
                while let Ok(ev) = rx.try_recv() {
                    if matches!(ev, AppEvent::AgentSpawned { .. }) {
                        seen = true;
                    }
                }
                seen
            })
            .await,
            "the agent spawned"
        );

        // The agent exits cleanly on its own; the supervisor's reaper records it.
        nth_spawn(&registry, "ENG-1", 0).0.finish(Some(0));
        let done = wait_for(|| {
            store
                .lock()
                .ok()
                .and_then(|s| s.get("ENG-1").map(|r| r.status == AgentStatus::Done))
                .unwrap_or(false)
        })
        .await;
        assert!(done, "a clean exit is reaped as Done");

        handle.shutdown();
        join.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_issue_can_be_relaunched_after_its_agent_self_exits() {
        // The reaper regression: a finished agent must free its slot so the
        // issue can be launched again (and `--resume`d), not be stuck forever.
        let (dir, wt) = temp_repo();
        let (tx, mut rx) = crate::event::channel();
        let registry: Registry = Arc::new(Mutex::new(Vec::new()));
        let cfg = config(&dir, wt, tx, recording_spawn(Arc::clone(&registry)), 4);
        let (handle, join) = Supervisor::start(cfg, &Handle::current());

        let mut spawns = 0usize;
        handle.launch("ENG-1".into(), "One".into(), None);
        assert!(
            wait_for(|| {
                while let Ok(ev) = rx.try_recv() {
                    if matches!(ev, AppEvent::AgentSpawned { .. }) {
                        spawns += 1;
                    }
                }
                spawns >= 1
            })
            .await,
            "the first agent spawned"
        );
        assert_eq!(spawns, 1);

        // The agent finishes on its own.
        nth_spawn(&registry, "ENG-1", 0).0.finish(Some(0));

        // Once the supervisor reaps it, a relaunch of the SAME issue takes. We
        // re-send until it does, since the reap is processed asynchronously.
        let relaunched = wait_for(|| {
            while let Ok(ev) = rx.try_recv() {
                if matches!(ev, AppEvent::AgentSpawned { .. }) {
                    spawns += 1;
                }
            }
            if spawns < 2 {
                handle.launch("ENG-1".into(), "One".into(), None);
            }
            spawns >= 2
        })
        .await;
        assert!(relaunched, "the issue was relaunchable after a self-exit");
        // The relaunch resumed the same session — found by issue + index, not a
        // global registry position.
        let (_fake, args) = nth_spawn(&registry, "ENG-1", 1);
        assert_eq!(args.first().map(String::as_str), Some("--resume"));

        handle.shutdown();
        join.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_self_exit_racing_a_cancel_keeps_the_real_failed_verdict() {
        // The race the `pre_shutdown` snapshot guards: the agent crashes non-zero
        // at almost the same instant the user presses `x`. Both select! arms can
        // be ready, so even if it resolves as `cancelled`, the status must be the
        // real Failed (from the exit code), never a laundered, resumable Idle.
        let (dir, wt) = temp_repo();
        let (tx, mut rx) = crate::event::channel();
        let registry: Registry = Arc::new(Mutex::new(Vec::new()));
        let cfg = config(&dir, wt, tx, recording_spawn(Arc::clone(&registry)), 4);
        let store = Arc::clone(&cfg.store);
        let (handle, join) = Supervisor::start(cfg, &Handle::current());

        handle.launch("ENG-1".into(), "One".into(), None);
        assert!(
            wait_for(|| {
                while let Ok(ev) = rx.try_recv() {
                    if matches!(ev, AppEvent::AgentSpawned { .. }) {
                        return true;
                    }
                }
                false
            })
            .await,
            "the agent spawned"
        );

        // The process self-exits non-zero (a crash). Whether the supervisor's
        // select! then takes the exit arm or the cancel arm, the pre-shutdown
        // snapshot carries the real Exited(1), so the verdict is Failed either way.
        let fake = nth_spawn(&registry, "ENG-1", 0).0;
        fake.finish(Some(1));
        handle.cancel("ENG-1".into());

        let failed = wait_for(|| {
            store
                .lock()
                .ok()
                .and_then(|s| s.get("ENG-1").map(|r| r.status == AgentStatus::Failed))
                .unwrap_or(false)
        })
        .await;
        assert!(
            failed,
            "a non-zero self-exit racing a cancel is recorded Failed, not Idle"
        );

        handle.shutdown();
        join.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_fresh_agent_stays_starting_and_never_fakes_running() {
        // We no longer flip to Running at spawn (that made every fresh agent read
        // as "working" before it did anything). The backend comes up — AgentSpawned
        // opens the window — but the node stays Spawning ("starting…"); only a real
        // hook (the first PostToolUse) promotes it. With no hooks, it must never
        // fake Running on its own.
        let (dir, wt) = temp_repo();
        let (tx, mut rx) = crate::event::channel();
        let registry: Registry = Arc::new(Mutex::new(Vec::new()));
        let cfg = config(&dir, wt, tx, recording_spawn(Arc::clone(&registry)), 4);
        let store = Arc::clone(&cfg.store);
        let (handle, join) = Supervisor::start(cfg, &Handle::current());

        handle.launch("ENG-1".into(), "One".into(), None);

        // Collect events over a window long enough that an eager flip would show.
        let (mut spawned, mut running) = (false, false);
        for _ in 0..15 {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    AppEvent::AgentSpawned { issue, .. } if issue == "ENG-1" => spawned = true,
                    AppEvent::AgentStatusChanged { issue, status, .. }
                        if issue == "ENG-1" && status == AgentStatus::Running =>
                    {
                        running = true;
                    }
                    _ => {}
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(spawned, "the backend came up and opened a window");
        assert!(
            !running,
            "a fresh agent must not fake Running before it works"
        );

        // The durable status is still Spawning, so the cockpit renders "starting…".
        let status = store.lock().unwrap().get("ENG-1").map(|r| r.status);
        assert_eq!(status, Some(AgentStatus::Spawning), "it stays 'starting…'");

        handle.shutdown();
        join.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relaunching_while_still_stopping_reports_a_distinct_message() {
        // A cancelled record lingers through teardown; a relaunch during that
        // window must say "still stopping", not the misleading "already running".
        let (dir, wt) = temp_repo();
        let (tx, mut rx) = crate::event::channel();
        let registry: Registry = Arc::new(Mutex::new(Vec::new()));
        let cfg = config(&dir, wt, tx, recording_spawn(Arc::clone(&registry)), 4);
        let (handle, join) = Supervisor::start(cfg, &Handle::current());

        handle.launch("ENG-1".into(), "One".into(), None);
        assert!(
            wait_for(|| spawn_count(&registry, "ENG-1") >= 1).await,
            "the agent spawned"
        );

        // Cancel then relaunch back-to-back, with no await between the two sends:
        // both commands are queued before the agent task can finish its teardown
        // (its `state.json` save fsyncs to disk first), so the relaunch is
        // processed while the record is still present with `cancelling` set, and
        // must yield the distinct "still stopping" message — never the misleading
        // "already has a running agent", which we assert against throughout.
        handle.cancel("ENG-1".into());
        handle.launch("ENG-1".into(), "One".into(), None);

        let saw_still_stopping = wait_for(|| {
            while let Ok(ev) = rx.try_recv() {
                if let AppEvent::LaunchRejected { reason, .. } = &ev {
                    assert!(
                        !reason.contains("already has a running agent"),
                        "a cancelling record must not report 'already running'"
                    );
                    if reason.contains("still stopping ENG-1") {
                        return true;
                    }
                }
            }
            false
        })
        .await;
        assert!(
            saw_still_stopping,
            "a relaunch during teardown reports 'still stopping'"
        );

        handle.shutdown();
        join.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_during_setup_bails_without_spawning_and_is_relaunchable() {
        // ARCHITECTURE.md's "worktree.create —(cancelled during git? bail)" branch:
        // a cancel that lands while setup is still in flight must NOT spawn a
        // process (no backend, no AgentSpawned) yet must still reap the record so
        // the issue stays relaunchable. We cancel in the same synchronous burst as
        // the launch — both commands are queued and processed before the spawned
        // run_agent's off-thread `git worktree add` can finish, so the
        // `is_cancelled()` early-bail fires before any backend is created.
        let (dir, wt) = temp_repo();
        let (tx, mut rx) = crate::event::channel();
        let registry: Registry = Arc::new(Mutex::new(Vec::new()));
        let cfg = config(&dir, wt, tx, recording_spawn(Arc::clone(&registry)), 4);
        let (handle, join) = Supervisor::start(cfg, &Handle::current());

        handle.launch("ENG-1".into(), "One".into(), None);
        handle.cancel("ENG-1".into()); // races into the setup window, before spawn

        // No process is ever spawned for this aborted launch: no fake recorded and
        // no AgentSpawned on the channel, even after a generous settle window.
        assert!(
            !wait_for(|| {
                while let Ok(ev) = rx.try_recv() {
                    if matches!(ev, AppEvent::AgentSpawned { .. }) {
                        return true;
                    }
                }
                false
            })
            .await,
            "a cancel during setup must not spawn the process"
        );
        assert_eq!(
            spawn_count(&registry, "ENG-1"),
            0,
            "the spawn fn was never invoked — no backend leaked"
        );

        // The record was still reaped, so the issue relaunches cleanly. We re-send
        // until it takes, since the reap from the aborted launch is async.
        let relaunched = wait_for(|| {
            if spawn_count(&registry, "ENG-1") == 0 {
                handle.launch("ENG-1".into(), "One".into(), None);
            }
            spawn_count(&registry, "ENG-1") >= 1
        })
        .await;
        assert!(
            relaunched,
            "the record was reaped, so the issue is relaunchable after a setup cancel"
        );

        handle.shutdown();
        join.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
