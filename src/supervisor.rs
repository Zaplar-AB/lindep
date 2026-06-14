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
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::backend::{AgentBackend, Lifecycle, SpawnConfig, SpawnFn};
use crate::event::{AppEvent, AppEventTx};
use crate::session::{AgentStatus, SessionStore};
use crate::worktree::WorktreeManager;

/// Everything the supervisor needs to launch and host agents.
pub struct SupervisorConfig {
    pub worktree: WorktreeManager,
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
    /// Most agents allowed at once.
    pub max_concurrent: usize,
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
    /// Launch an agent on `issue` (no-op if already running or at capacity).
    pub fn launch(&self, issue: String, title: String) {
        let _ = self.cmd_tx.send(Command::Launch { issue, title });
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
    issue: String,
    title: String,
    generation: u64,
    token: CancellationToken,
    worktree: WorktreeManager,
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
                Command::Launch { issue, title } => self.launch(issue, title),
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
    fn launch(&mut self, issue: String, title: String) {
        if let Some(record) = self.agents.get(&issue) {
            // A cancelled record lingers until its task confirms teardown; tell
            // the user it's still stopping rather than the misleading "already
            // running" — the relaunch will take once the matching `Reaped` lands.
            if record.cancelling {
                self.notify(format!("still stopping {issue}, try again in a moment"));
            } else {
                self.notify(format!("{issue} already has a running agent"));
            }
            return;
        }
        if self.agents.len() >= self.cfg.max_concurrent {
            self.notify(format!(
                "at capacity ({} agents) — cancel one first",
                self.cfg.max_concurrent
            ));
            return;
        }

        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        let token = self.parent.child_token();
        self.agents.insert(
            issue.clone(),
            AgentRecord {
                generation,
                token: token.clone(),
                cancelling: false,
            },
        );

        let task = AgentTask {
            issue,
            title,
            generation,
            token,
            worktree: self.cfg.worktree.clone(),
            store: Arc::clone(&self.cfg.store),
            events: self.cfg.events.clone(),
            spawn: Arc::clone(&self.cfg.spawn),
            exe: self.cfg.exe.clone(),
            hook_port: self.cfg.hook_port,
            hook_token: self.cfg.hook_token.clone(),
            hooks_dir: self.cfg.hooks_dir.clone(),
            base: self.cfg.base.clone(),
            rows: self.cfg.rows,
            cols: self.cfg.cols,
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
            // Tell the cockpit the agent is fully gone so it can drop the fleet
            // entry (bounds the overview; keeps it in step with our live map).
            let _ = self.cfg.events.send(AppEvent::AgentReaped {
                issue: issue.to_string(),
            });
        }
    }

    fn notify(&self, message: String) {
        let _ = self.cfg.events.send(AppEvent::Notification(message));
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

async fn supervise(task: &AgentTask) {
    let notify = |msg: String| {
        let _ = task.events.send(AppEvent::Notification(msg));
    };

    // git worktree creation blocks; run it on the blocking pool, not a worker.
    let (mgr, issue, title, base) = (
        task.worktree.clone(),
        task.issue.clone(),
        task.title.clone(),
        task.base.clone(),
    );
    let create = tokio::task::spawn_blocking(move || mgr.create(&issue, &title, &base));
    let worktree = tokio::select! {
        // A cancel/shutdown during the (blocking, un-abortable) git op must not
        // pin teardown behind it: stop awaiting and return so `tracker.wait()`
        // stays responsive. The detached blocking thread finishes git on its own
        // and the runtime reclaims it — we just don't gate shutdown on it.
        () = task.token.cancelled() => return,
        res = create => match res {
            Ok(Ok(worktree)) => worktree,
            Ok(Err(e)) => return notify(format!("worktree for {} failed: {e}", task.issue)),
            Err(e) => return notify(format!("worktree task for {} panicked: {e}", task.issue)),
        },
    };

    // Cancelled just as git finished? Bail before spawning a process.
    if task.token.is_cancelled() {
        return;
    }

    // Session record: deterministic id, resume if we've launched this before.
    let (session_id, resume, snapshot) = {
        let Ok(mut store) = task.store.lock() else {
            return notify("session store lock poisoned".to_string());
        };
        let resume = store.get(&task.issue).is_some();
        let session = store.ensure(&task.issue, worktree.path.clone(), worktree.branch.clone());
        let session_id = session.session_id.clone();
        store.set_status(&task.issue, AgentStatus::Spawning);
        // Snapshot under the lock; persist after dropping it so blocking fs I/O
        // never runs under the store mutex (a hook waiting on the same lock must
        // not block behind a disk rename).
        let snapshot = store
            .snapshot_bytes()
            .ok()
            .map(|b| (store.path().to_path_buf(), b));
        (session_id, resume, snapshot)
    };
    if let Some((path, bytes)) = snapshot {
        let _ =
            tokio::task::spawn_blocking(move || SessionStore::write_snapshot(&path, &bytes)).await;
    }

    // Hook settings so this agent's notifications find their way back to us.
    let settings = task.hooks_dir.join(format!("{}.settings.json", task.issue));
    if let Err(e) = crate::notify::write_settings(
        &settings,
        &task.exe.to_string_lossy(),
        task.hook_port,
        &task.hook_token,
    ) {
        return notify(format!("hook settings for {} failed: {e}", task.issue));
    }

    let mut spawn_cfg = SpawnConfig::claude(
        &task.issue,
        worktree.path,
        &session_id,
        resume,
        task.rows,
        task.cols,
    )
    .arg("--settings")
    .arg(settings.to_string_lossy().to_string());
    for guardrail in &task.guardrails {
        spawn_cfg = spawn_cfg.arg(guardrail);
    }

    let backend = match (task.spawn)(spawn_cfg, task.events.clone()) {
        Ok(backend) => backend,
        Err(e) => return notify(format!("spawning agent for {} failed: {e}", task.issue)),
    };
    let _ = task.events.send(AppEvent::AgentSpawned {
        issue: task.issue.clone(),
        backend: Arc::clone(&backend),
    });

    // The process is up, so leave Spawning immediately — otherwise a healthy but
    // quiet agent (thinking/streaming before its first PostToolUse) would show
    // `◌ spawning` forever, indistinguishable from a wedged spawn. The hook bus
    // refines this to NeedsYou/Idle once it has something to report.
    crate::session::mutate_and_persist(&task.store, |store| {
        store.set_status(&task.issue, AgentStatus::Running);
    })
    .await;
    let _ = task.events.send(AppEvent::AgentStatusChanged {
        issue: task.issue.clone(),
        status: AgentStatus::Running,
    });

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

    // The agent task is the sole authority on post-mortem status, graded off the
    // pre-teardown snapshot so our own SIGTERM/SIGKILL can't recolour the verdict.
    // A self-exit is graded by its exit code (Done/Failed). A deliberate cancel of
    // a still-running process is Idle (resumable) — but a cancel that lost the race
    // to the process's own exit honours the real exit code, so a non-zero crash
    // reads Failed, never a laundered Idle. (The backend's own AgentExited event
    // only drives the footer + frees the render handle.)
    let status = match pre_shutdown {
        // Still alive when we cancelled: we killed it — a clean, resumable stop.
        Lifecycle::Running => AgentStatus::Idle,
        Lifecycle::Exited(Some(0)) | Lifecycle::Exited(None) => AgentStatus::Done,
        Lifecycle::Exited(Some(_)) => AgentStatus::Failed,
    };
    crate::session::mutate_and_persist(&task.store, |store| {
        store.set_status(&task.issue, status);
    })
    .await;
    let _ = task.events.send(AppEvent::AgentStatusChanged {
        issue: task.issue.clone(),
        status,
    });
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
            worktree: wt,
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
    async fn an_agent_that_ignores_sigterm_is_escalated_to_force_kill() {
        let (dir, wt) = temp_repo();
        let (tx, mut rx) = crate::event::channel();
        let registry: Registry = Arc::new(Mutex::new(Vec::new()));
        let cfg = config(&dir, wt, tx, ignoring_spawn(Arc::clone(&registry)), 4);
        let store = Arc::clone(&cfg.store);
        let (handle, join) = Supervisor::start(cfg, &Handle::current());

        handle.launch("ENG-1".into(), "One".into());
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

        // A cancel of a still-running process is still graded a clean, resumable
        // Idle (off the pre-teardown snapshot) and persisted as such.
        assert!(
            wait_for(|| {
                while let Ok(ev) = rx.try_recv() {
                    if let AppEvent::AgentStatusChanged { issue, status } = ev
                        && issue == "ENG-1"
                        && status == AgentStatus::Idle
                    {
                        return true;
                    }
                }
                false
            })
            .await,
            "cancel of an unresponsive agent still resolves to Idle"
        );
        assert_eq!(
            store.lock().unwrap().get("ENG-1").map(|s| s.status),
            Some(AgentStatus::Idle),
            "durable store records the resumable Idle"
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

        handle.launch("ENG-1".into(), "One".into());
        handle.launch("ENG-2".into(), "Two".into());

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

        handle.launch("ENG-1".into(), "One".into());
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
                    handle.launch("ENG-1".into(), "One".into());
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
        assert_eq!(args.get(1), Some(&SessionStore::session_id_for("ENG-1")));

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

        handle.launch("ENG-1".into(), "One".into());
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
        handle.launch("ENG-1".into(), "One".into());
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
                handle.launch("ENG-1".into(), "One".into());
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

        handle.launch("ENG-1".into(), "One".into());
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
    async fn spawning_moves_to_running_once_the_process_is_up() {
        // A healthy-but-quiet agent (no PostToolUse yet) must not be stuck on
        // Spawning: the supervisor emits Running the moment the backend is up.
        let (dir, wt) = temp_repo();
        let (tx, mut rx) = crate::event::channel();
        let registry: Registry = Arc::new(Mutex::new(Vec::new()));
        let cfg = config(&dir, wt, tx, recording_spawn(Arc::clone(&registry)), 4);
        let store = Arc::clone(&cfg.store);
        let (handle, join) = Supervisor::start(cfg, &Handle::current());

        handle.launch("ENG-1".into(), "One".into());

        // The supervisor emits AgentStatusChanged{Running} after AgentSpawned…
        let saw_running = wait_for(|| {
            while let Ok(ev) = rx.try_recv() {
                if let AppEvent::AgentStatusChanged { issue, status } = ev
                    && issue == "ENG-1"
                    && status == AgentStatus::Running
                {
                    return true;
                }
            }
            false
        })
        .await;
        assert!(
            saw_running,
            "the node left Spawning once the process was up"
        );

        // …and persists it, so a cockpit restart doesn't show a stale Spawning.
        let persisted_running = wait_for(|| {
            store
                .lock()
                .ok()
                .and_then(|s| s.get("ENG-1").map(|r| r.status == AgentStatus::Running))
                .unwrap_or(false)
        })
        .await;
        assert!(persisted_running, "Running was persisted to the store");

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

        handle.launch("ENG-1".into(), "One".into());
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
        handle.launch("ENG-1".into(), "One".into());

        let saw_still_stopping = wait_for(|| {
            while let Ok(ev) = rx.try_recv() {
                if let AppEvent::Notification(msg) = &ev {
                    assert!(
                        !msg.contains("already has a running agent"),
                        "a cancelling record must not report 'already running'"
                    );
                    if msg.contains("still stopping ENG-1") {
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

        handle.launch("ENG-1".into(), "One".into());
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
                handle.launch("ENG-1".into(), "One".into());
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
