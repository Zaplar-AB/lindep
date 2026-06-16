//! Workspace supervisor — one fleet per Linear project, kept alive across
//! navigation.
//!
//! The v1 [`Supervisor`](crate::supervisor::Supervisor) owns exactly one
//! project's fleet (its own worktree root, state file and hook settings). The
//! workspace lifts ownership a level: it spins up one supervisor per
//! [`ProjectMapping`](crate::projects::ProjectMapping) and routes launch/cancel
//! by `project_id` — the groundwork for running several projects' fleets at once,
//! so agents started in one project keep running while you work in another.
//! **Backing out of a project never cancels its agents — only
//! [`shutdown`](WorkspaceHandle::shutdown) does**, and it fans out across every
//! project's tracker.
//!
//! Switching the *active* project from the UI is not wired yet (ENG-401): today
//! the cockpit launches into the one project it booted into, so the per-project
//! routing here is exercised by tests and staged for the switcher rather than
//! reachable end-to-end.
//!
//! Like the supervisor, the workspace lives entirely inside its own task; the
//! cockpit holds a cheap, cloneable [`WorkspaceHandle`]. A project's plane is
//! built lazily on first launch (the active project's is built eagerly at boot
//! and handed in), so entering a not-yet-started project reconciles its store
//! against its live worktrees and rehydrates its fleet view (reachable once
//! switching lands — ENG-401).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::backend::SpawnFn;
use crate::event::{AppEvent, AppEventTx};
use crate::projects::{ProjectMapping, WorkspaceConfig};
use crate::session::{AgentStatus, SessionStore};
use crate::supervisor::{Supervisor, SupervisorConfig, SupervisorHandle};
use crate::worktree::WorktreeManager;

/// Every started project's session store, keyed by `project_id`. Shared between
/// the workspace (which inserts a store when it builds a project's plane) and the
/// notification bus (which scans the stores to resolve a hook back to its owning
/// `(project_id, issue)`, since the one loopback endpoint serves every project).
pub type StoreRegistry = Arc<Mutex<HashMap<String, Arc<Mutex<SessionStore>>>>>;

/// The shared, per-project-invariant ingredients for building a supervisor:
/// everything a [`SupervisorConfig`] needs except the project's own worktree,
/// store and id. One builder serves every project so the workspace-wide cap and
/// its `live_count` are genuinely shared.
#[derive(Clone)]
pub struct PlaneBuilder {
    pub events: AppEventTx,
    pub spawn: Arc<SpawnFn>,
    pub exe: PathBuf,
    pub hook_port: u16,
    pub hook_token: String,
    pub base: String,
    pub rows: u16,
    pub cols: u16,
    pub max_concurrent: usize,
    pub live_count: Arc<AtomicUsize>,
    pub guardrails: Vec<String>,
}

impl PlaneBuilder {
    /// Assemble a [`SupervisorConfig`] for `mapping`'s project from this builder
    /// plus the project's own worktree manager and session store. Shared by the
    /// eager (active-project, at boot) and lazy (background-project) paths so the
    /// config is assembled in exactly one place.
    pub fn supervisor_config(
        &self,
        mapping: &ProjectMapping,
        worktree: WorktreeManager,
        store: Arc<Mutex<SessionStore>>,
    ) -> SupervisorConfig {
        SupervisorConfig {
            project_id: mapping.project_id.clone(),
            worktree,
            store,
            events: self.events.clone(),
            spawn: Arc::clone(&self.spawn),
            exe: self.exe.clone(),
            hook_port: self.hook_port,
            hook_token: self.hook_token.clone(),
            hooks_dir: mapping.hooks_dir(),
            base: self.base.clone(),
            rows: self.rows,
            cols: self.cols,
            max_concurrent: self.max_concurrent,
            live_count: Arc::clone(&self.live_count),
            guardrails: self.guardrails.clone(),
        }
    }
}

/// One project's running fleet: a handle to drive its supervisor and the
/// supervisor task's join handle (awaited on workspace shutdown).
pub struct ProjectPlane {
    pub handle: SupervisorHandle,
    pub join: JoinHandle<()>,
}

/// Reconcile a project's store against its live worktrees and rehydrate the
/// fleet view from the durable store — the "the process is disposable, the
/// conversation is durable" restart behaviour, now per project. Drops sessions
/// whose worktree vanished, then re-emits each surviving session's last-known
/// status (a was-`Spawning`/`Running` process is gone after a restart, so it
/// resolves to `Idle` — resumable, not falsely live). Returns the was-live set
/// (the auto-resume / cockpit-restore candidates) for the caller that wants it.
pub fn reconcile_and_rehydrate(
    worktree: &WorktreeManager,
    store: &Arc<Mutex<SessionStore>>,
    events: &AppEventTx,
    project_id: &str,
) -> HashSet<String> {
    let live: Vec<String> = worktree
        .list()
        .unwrap_or_default()
        .into_iter()
        .map(|w| w.issue)
        .collect();
    if let Ok(mut store) = store.lock() {
        store.reconcile(live);
        let _ = store.save();
    }
    let mut resumable = HashSet::new();
    if let Ok(store) = store.lock() {
        for session in store.sessions() {
            if session.status.is_live() {
                resumable.insert(session.issue.clone());
            }
            let status = match session.status {
                AgentStatus::Spawning | AgentStatus::Running => AgentStatus::Idle,
                other => other,
            };
            let _ = events.send(AppEvent::AgentStatusChanged {
                project_id: project_id.to_string(),
                issue: session.issue.clone(),
                status,
            });
        }
    }
    resumable
}

/// Re-emit every session's CURRENT status for `project_id` (no restart
/// downgrade), so switching into an already-running project repopulates its fleet
/// view without falsely idling agents that are still live. A *fresh* plane gets
/// this via [`reconcile_and_rehydrate`] in [`build_plane`] instead (which DOES
/// downgrade, because a never-started project's records are from a dead process).
pub fn reemit_statuses(
    store: &Arc<Mutex<SessionStore>>,
    events: &AppEventTx,
    project_id: &str,
) {
    if let Ok(store) = store.lock() {
        for session in store.sessions() {
            let _ = events.send(AppEvent::AgentStatusChanged {
                project_id: project_id.to_string(),
                issue: session.issue.clone(),
                status: session.status,
            });
        }
    }
}

/// Build a project's plane off the render thread: its worktree manager, session
/// store (per-project file, adopting a legacy `state.json` on first v1.5 boot),
/// reconcile + rehydrate, then start its supervisor. The blocking pieces
/// (canonicalizing the worktree root, reading the state file, listing worktrees)
/// run on the blocking pool so the workspace command loop never stalls. Returns
/// `None` (with a footer line) if the worktree root can't be opened.
pub async fn build_plane(
    rt: &Handle,
    builder: &PlaneBuilder,
    mapping: &ProjectMapping,
    stores: &StoreRegistry,
) -> Option<ProjectPlane> {
    let repo_root = mapping.repo_root.clone();
    let branch_prefix = mapping.branch_prefix.clone();
    let worktree = match tokio::task::spawn_blocking(move || match branch_prefix {
        Some(prefix) => WorktreeManager::with_prefix(&repo_root, prefix),
        None => WorktreeManager::new(&repo_root),
    })
    .await
    {
        Ok(Ok(worktree)) => worktree,
        Ok(Err(e)) => {
            let _ = builder.events.send(AppEvent::Notification(format!(
                "project {}: can't open repo: {e}",
                mapping.name
            )));
            return None;
        }
        Err(_) => return None,
    };

    let project_id = mapping.project_id.clone();
    let state_path = mapping.state_path();
    let legacy = mapping.legacy_state_path();
    let store = match tokio::task::spawn_blocking(move || {
        SessionStore::open_project(&project_id, state_path, Some(legacy))
    })
    .await
    {
        Ok(Ok(store)) => store,
        Ok(Err(e)) => {
            let _ = builder.events.send(AppEvent::Notification(format!(
                "project {}: session state unreadable ({e}); starting fresh",
                mapping.name
            )));
            SessionStore::empty(mapping.state_path()).for_project(&mapping.project_id)
        }
        Err(_) => return None,
    };
    let store = Arc::new(Mutex::new(store));
    // Register the store so the notification bus and the global view can find this
    // project's sessions even while you're inside another project.
    if let Ok(mut registry) = stores.lock() {
        registry.insert(mapping.project_id.clone(), Arc::clone(&store));
    }

    // Reconcile + rehydrate off the workers (worktree.list() shells out to git).
    {
        let worktree = worktree.clone();
        let store = Arc::clone(&store);
        let events = builder.events.clone();
        let project_id = mapping.project_id.clone();
        let _ = tokio::task::spawn_blocking(move || {
            reconcile_and_rehydrate(&worktree, &store, &events, &project_id)
        })
        .await;
    }

    let cfg = builder.supervisor_config(mapping, worktree, store);
    let (handle, join) = Supervisor::start(cfg, rt);
    Some(ProjectPlane { handle, join })
}

/// Commands the workspace processes, each addressed by `project_id` so the right
/// project's supervisor handles it.
enum WorkspaceCommand {
    Launch {
        project_id: String,
        issue: String,
        title: String,
        size: Option<(u16, u16)>,
    },
    Cancel {
        project_id: String,
        issue: String,
    },
    /// Bring `project_id` online without launching anything: build its plane if
    /// it isn't running, then re-emit its current fleet statuses so the cockpit's
    /// switched-to view repopulates. The driver behind project switching.
    Activate {
        project_id: String,
    },
    Shutdown,
}

/// Cheap, cloneable handle the cockpit holds to drive every project's fleet.
#[derive(Clone)]
pub struct WorkspaceHandle {
    cmd_tx: mpsc::UnboundedSender<WorkspaceCommand>,
}

impl WorkspaceHandle {
    /// Launch an agent on `(project_id, issue)`, building that project's plane
    /// first if it isn't running yet. `size` is the tile the cockpit will render
    /// the agent in (see [`SupervisorHandle::launch`]).
    pub fn launch(
        &self,
        project_id: String,
        issue: String,
        title: String,
        size: Option<(u16, u16)>,
    ) {
        let _ = self.cmd_tx.send(WorkspaceCommand::Launch {
            project_id,
            issue,
            title,
            size,
        });
    }

    /// Stop a single agent in `project_id`, leaving every other agent — in this
    /// project and others — running.
    pub fn cancel(&self, project_id: String, issue: String) {
        let _ = self
            .cmd_tx
            .send(WorkspaceCommand::Cancel { project_id, issue });
    }

    /// Switch the cockpit into `project_id`: builds its plane if needed and
    /// re-emits its current fleet statuses (the project you leave keeps running).
    pub fn activate(&self, project_id: String) {
        let _ = self.cmd_tx.send(WorkspaceCommand::Activate { project_id });
    }

    /// Begin a clean shutdown of every project's fleet. Pair with awaiting the
    /// workspace task's `JoinHandle` so the process waits for all agents to die.
    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(WorkspaceCommand::Shutdown);
    }
}

/// The workspace owner. Lives inside its own task; holds one [`ProjectPlane`]
/// per project it has started, and the ingredients to start more on demand.
pub struct Workspace {
    rt: Handle,
    builder: PlaneBuilder,
    config: WorkspaceConfig,
    planes: HashMap<String, ProjectPlane>,
    stores: StoreRegistry,
}

impl Workspace {
    /// Spawn the workspace task, seeded with the already-built planes (the active
    /// project's, constructed eagerly at boot). Returns a handle to drive it plus
    /// the task's join handle (await after `shutdown()` to block until every
    /// project's agents are torn down).
    pub fn start(
        rt: Handle,
        builder: PlaneBuilder,
        config: WorkspaceConfig,
        initial: HashMap<String, ProjectPlane>,
        stores: StoreRegistry,
    ) -> (WorkspaceHandle, JoinHandle<()>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let spawn_handle = rt.clone();
        let workspace = Workspace {
            rt,
            builder,
            config,
            planes: initial,
            stores,
        };
        let join = spawn_handle.spawn(workspace.run(cmd_rx));
        (WorkspaceHandle { cmd_tx }, join)
    }

    async fn run(mut self, mut cmd_rx: mpsc::UnboundedReceiver<WorkspaceCommand>) {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                WorkspaceCommand::Launch {
                    project_id,
                    issue,
                    title,
                    size,
                } => {
                    if let Some(handle) = self.ensure_plane(&project_id).await {
                        handle.launch(issue, title, size);
                    }
                }
                WorkspaceCommand::Cancel { project_id, issue } => {
                    // Only a started project can have a live agent to cancel; an
                    // unstarted one is a no-op (nothing to stop).
                    if let Some(plane) = self.planes.get(&project_id) {
                        plane.handle.cancel(issue);
                    }
                }
                WorkspaceCommand::Activate { project_id } => {
                    let existed = self.planes.contains_key(&project_id);
                    // Build the plane if this is the first time we enter the project
                    // (build_plane reconciles + rehydrates it). If it was already
                    // running — its agents may be live — re-emit current statuses
                    // verbatim so the switched-to fleet repopulates without the
                    // restart downgrade a fresh build applies.
                    if self.ensure_plane(&project_id).await.is_some() && existed {
                        let store = self
                            .stores
                            .lock()
                            .ok()
                            .and_then(|reg| reg.get(&project_id).cloned());
                        if let Some(store) = store {
                            reemit_statuses(&store, &self.builder.events, &project_id);
                        }
                    }
                }
                WorkspaceCommand::Shutdown => break,
            }
        }
        // Fan out teardown: signal every project's supervisor, then await each so
        // no process group outlives the cockpit. Signalling all first lets them
        // tear down concurrently; the awaits just collect them (≈ slowest fleet,
        // not the sum). The caller bounds the whole wait with a grace timeout.
        for plane in self.planes.values() {
            plane.handle.shutdown();
        }
        for (_, plane) in self.planes.drain() {
            let _ = plane.join.await;
        }
    }

    /// Return the supervisor handle for `project_id`, building (and reconciling +
    /// rehydrating) its plane the first time. `None` if the project isn't mapped
    /// in `projects.toml` or its repo can't be opened — surfaced as a footer line.
    async fn ensure_plane(&mut self, project_id: &str) -> Option<SupervisorHandle> {
        if let Some(plane) = self.planes.get(project_id) {
            return Some(plane.handle.clone());
        }
        let mapping = match self.config.resolve(project_id) {
            Ok(mapping) => mapping.clone(),
            Err(e) => {
                let _ = self
                    .builder
                    .events
                    .send(AppEvent::Notification(e.to_string()));
                return None;
            }
        };
        let plane = build_plane(&self.rt, &self.builder, &mapping, &self.stores).await?;
        let handle = plane.handle.clone();
        self.planes.insert(project_id.to_string(), plane);
        Some(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::FakeBackend;
    use crate::backend::{AgentBackend, SpawnConfig};
    use crate::event::AppEvent;
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A process-unique git repo, mirroring the worktree/session test helpers.
    fn temp_repo(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("lindep-ws-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&dir)
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success(),
                "git {args:?} failed"
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "Test"]);
        run(&["commit", "-q", "--allow-empty", "-m", "root"]);
        dir
    }

    /// A spawn fn handing out fakes, recording how many times each issue spawned.
    fn fake_spawn(count: Arc<AtomicUsize>) -> Arc<SpawnFn> {
        Arc::new(move |cfg: SpawnConfig, _events: AppEventTx| {
            count.fetch_add(1, Ordering::Relaxed);
            Ok(FakeBackend::new(&cfg.issue) as Arc<dyn AgentBackend>)
        })
    }

    /// Every fake handed out, keyed by `(project_id, issue)`, so a test can assert
    /// on a specific project's backend (e.g. that cancelling one leaves another's
    /// alive).
    type BackendLog = Arc<Mutex<HashMap<(String, String), Arc<FakeBackend>>>>;

    /// Like [`fake_spawn`] but also stashes each fake so the test can inspect it.
    /// Publishes the log entry BEFORE bumping the counter, so a reader that observes
    /// `count >= n` is guaranteed the corresponding backends are already in the log
    /// (the two spawns race on separate supervisor tasks).
    fn recording_spawn(count: Arc<AtomicUsize>, log: BackendLog) -> Arc<SpawnFn> {
        Arc::new(move |cfg: SpawnConfig, _events: AppEventTx| {
            let fake = FakeBackend::new(&cfg.issue);
            log.lock()
                .unwrap()
                .insert((cfg.project_id.clone(), cfg.issue.clone()), Arc::clone(&fake));
            count.fetch_add(1, Ordering::Relaxed);
            Ok(fake as Arc<dyn AgentBackend>)
        })
    }

    fn builder(
        events: AppEventTx,
        spawn: Arc<SpawnFn>,
        live_count: Arc<AtomicUsize>,
        max_concurrent: usize,
    ) -> PlaneBuilder {
        PlaneBuilder {
            events,
            spawn,
            exe: PathBuf::from("lindep"),
            hook_port: 1,
            hook_token: String::new(),
            base: "HEAD".to_string(),
            rows: 24,
            cols: 80,
            max_concurrent,
            live_count,
            guardrails: vec![],
        }
    }

    /// Poll `cond` up to ~4s (the fake agents settle fast); returns whether it held.
    async fn eventually(mut cond: impl FnMut() -> bool) -> bool {
        for _ in 0..200 {
            if cond() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        cond()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_same_issue_in_two_projects_runs_two_independent_agents() {
        let repo_a = temp_repo("a");
        let repo_b = temp_repo("b");
        let (tx, _rx) = crate::event::channel();
        let spawns = Arc::new(AtomicUsize::new(0));
        let live = Arc::new(AtomicUsize::new(0));
        let backends: BackendLog = Arc::new(Mutex::new(HashMap::new()));
        let b = builder(
            tx,
            recording_spawn(Arc::clone(&spawns), Arc::clone(&backends)),
            live,
            10,
        );

        // Two mapped projects, distinct repos.
        let mut config = WorkspaceConfig::default();
        config.ensure_mapped("proj-a", "A", &repo_a);
        config.ensure_mapped("proj-b", "B", &repo_b);
        // (A real boot supplies branch_prefix via projects.toml; ensure_mapped
        // defaults it to None, which is fine — the worktree manager picks one.)

        let (ws, join) = Workspace::start(
            Handle::current(),
            b,
            config,
            HashMap::new(),
            Arc::new(Mutex::new(HashMap::new())),
        );

        // Launch ENG-1 in BOTH projects: distinct worktree roots, so both spawn.
        ws.launch("proj-a".into(), "ENG-1".into(), "one".into(), None);
        ws.launch("proj-b".into(), "ENG-1".into(), "one".into(), None);

        // Both agents come up (two spawns for the same issue key, different repos).
        assert!(
            eventually(|| spawns.load(Ordering::Relaxed) >= 2).await,
            "the same issue key launched in two projects yields two agents"
        );
        assert_eq!(spawns.load(Ordering::Relaxed), 2);
        let backend = |proj: &str| {
            backends
                .lock()
                .unwrap()
                .get(&(proj.to_string(), "ENG-1".to_string()))
                .cloned()
                .expect("both projects spawned a backend")
        };
        let (agent_a, agent_b) = (backend("proj-a"), backend("proj-b"));

        // Cancelling one project's agent tears IT down…
        ws.cancel("proj-a".into(), "ENG-1".into());
        assert!(
            eventually(|| agent_a.shutdown_count() > 0).await,
            "the cancelled project's agent is torn down"
        );
        // …and leaves the OTHER project's agent untouched — the core isolation
        // promise. Give any erroneous cross-project cancel time to land first.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            agent_b.shutdown_count(),
            0,
            "cancelling proj-a must not touch proj-b's agent"
        );

        // Shutdown reaps every project's fleet within grace — fail loudly if it hangs.
        ws.shutdown();
        tokio::time::timeout(std::time::Duration::from_secs(5), join)
            .await
            .expect("workspace shut down within grace")
            .expect("workspace task joined cleanly");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_live_agent_cap_spans_projects() {
        // The cap's load-bearing promise: max_concurrent bounds live agents across
        // the WHOLE workspace via the shared live_count, not per project. With a cap
        // of 2, two agents in project A must leave no room for one in project B.
        let repo_a = temp_repo("cap-a");
        let repo_b = temp_repo("cap-b");
        let (tx, mut rx) = crate::event::channel();
        let spawns = Arc::new(AtomicUsize::new(0));
        let live = Arc::new(AtomicUsize::new(0));
        let b = builder(tx, fake_spawn(Arc::clone(&spawns)), live, 2);

        let mut config = WorkspaceConfig::default();
        config.ensure_mapped("proj-a", "A", &repo_a);
        config.ensure_mapped("proj-b", "B", &repo_b);
        let (ws, join) = Workspace::start(
            Handle::current(),
            b,
            config,
            HashMap::new(),
            Arc::new(Mutex::new(HashMap::new())),
        );

        // Fill the workspace cap with two agents in project A.
        ws.launch("proj-a".into(), "ENG-1".into(), "one".into(), None);
        ws.launch("proj-a".into(), "ENG-2".into(), "two".into(), None);
        assert!(
            eventually(|| spawns.load(Ordering::Relaxed) >= 2).await,
            "project A fills the cap with two agents"
        );

        // A third launch in project B is refused — the cap is workspace-wide, so a
        // per-project counter regression (which would let B spawn) fails here.
        ws.launch("proj-b".into(), "ENG-1".into(), "three".into(), None);
        let at_capacity = eventually(|| {
            while let Ok(ev) = rx.try_recv() {
                if let AppEvent::Notification(m) = ev
                    && m.contains("at capacity")
                {
                    return true;
                }
            }
            false
        })
        .await;
        assert!(at_capacity, "the workspace-wide cap rejects the third launch");
        assert_eq!(
            spawns.load(Ordering::Relaxed),
            2,
            "no third agent spawned in another project"
        );

        ws.shutdown();
        tokio::time::timeout(std::time::Duration::from_secs(5), join)
            .await
            .expect("workspace shut down within grace")
            .expect("workspace task joined cleanly");
    }

    #[test]
    fn reconcile_and_rehydrate_downgrades_live_sessions_and_reports_them_resumable() {
        // The per-project "process disposable, conversation durable" restart logic:
        // a was-Spawning/Running session (its process is gone) must rehydrate as
        // Idle (resumable, not falsely live) and be returned in the resumable set,
        // while a terminal session keeps its status and is NOT resumable.
        let repo = temp_repo("rehydrate");
        let wt = WorktreeManager::new(&repo).unwrap();
        // Real worktrees so both issues survive reconcile's prune.
        let live_wt = wt.create("ENG-live", "live", "HEAD").unwrap();
        let done_wt = wt.create("ENG-done", "done", "HEAD").unwrap();

        let state = repo.join(".lindep").join("state.json");
        let store = Arc::new(Mutex::new(
            SessionStore::load(&state).unwrap().for_project("proj-x"),
        ));
        {
            let mut s = store.lock().unwrap();
            s.ensure("ENG-live", live_wt.path.clone(), live_wt.branch.clone());
            s.ensure("ENG-done", done_wt.path.clone(), done_wt.branch.clone());
            s.set_status("ENG-live", AgentStatus::Running);
            s.set_status("ENG-done", AgentStatus::Done);
        }

        let (tx, mut rx) = crate::event::channel();
        let resumable = reconcile_and_rehydrate(&wt, &store, &tx, "proj-x");

        assert!(resumable.contains("ENG-live"), "the was-live session is resumable");
        assert!(!resumable.contains("ENG-done"), "a terminal session is not resumable");
        assert_eq!(resumable.len(), 1);

        let (mut live_status, mut done_status) = (None, None);
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::AgentStatusChanged {
                project_id,
                issue,
                status,
            } = ev
            {
                assert_eq!(project_id, "proj-x", "events carry the owning project");
                match issue.as_str() {
                    "ENG-live" => live_status = Some(status),
                    "ENG-done" => done_status = Some(status),
                    other => panic!("unexpected issue {other}"),
                }
            }
        }
        assert_eq!(
            live_status,
            Some(AgentStatus::Idle),
            "a was-Running session rehydrates as Idle"
        );
        assert_eq!(
            done_status,
            Some(AgentStatus::Done),
            "a terminal session keeps its status verbatim"
        );
    }

    #[test]
    fn reemit_statuses_re_emits_current_status_verbatim_without_downgrade() {
        // Switching INTO a project whose agents are still live must show them as
        // running — so unlike reconcile_and_rehydrate, reemit_statuses must NOT
        // downgrade a Running session to Idle.
        let repo = temp_repo("reemit");
        let state = repo.join(".lindep").join("state.json");
        let store = Arc::new(Mutex::new(
            SessionStore::load(&state).unwrap().for_project("proj-x"),
        ));
        {
            let mut s = store.lock().unwrap();
            s.ensure("ENG-1", "/wt/ENG-1".into(), "b".into());
            s.set_status("ENG-1", AgentStatus::Running);
        }

        let (tx, mut rx) = crate::event::channel();
        reemit_statuses(&store, &tx, "proj-x");

        let mut got = None;
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::AgentStatusChanged {
                project_id,
                issue,
                status,
            } = ev
            {
                assert_eq!((project_id.as_str(), issue.as_str()), ("proj-x", "ENG-1"));
                got = Some(status);
            }
        }
        assert_eq!(
            got,
            Some(AgentStatus::Running),
            "status is re-emitted verbatim, never downgraded to Idle"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn launching_an_unmapped_project_is_a_no_op_with_a_footer() {
        let (tx, mut rx) = crate::event::channel();
        let spawns = Arc::new(AtomicUsize::new(0));
        let live = Arc::new(AtomicUsize::new(0));
        let b = builder(tx, fake_spawn(Arc::clone(&spawns)), live, 10);
        let (ws, join) = Workspace::start(
            Handle::current(),
            b,
            WorkspaceConfig::default(),
            HashMap::new(),
            Arc::new(Mutex::new(HashMap::new())),
        );

        ws.launch("ghost".into(), "ENG-1".into(), "x".into(), None);

        // No agent spawns; a footer notification names the unmapped project.
        let mut saw_footer = false;
        for _ in 0..50 {
            while let Ok(ev) = rx.try_recv() {
                if let AppEvent::Notification(msg) = ev
                    && msg.contains("ghost")
                {
                    saw_footer = true;
                }
            }
            if saw_footer {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(saw_footer, "an unmapped project surfaces a footer line");
        assert_eq!(spawns.load(Ordering::Relaxed), 0, "nothing was launched");

        ws.shutdown();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), join).await;
    }
}
