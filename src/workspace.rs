//! Workspace supervisor — one fleet per Linear project, kept alive across
//! navigation.
//!
//! The v1 [`Supervisor`](crate::supervisor::Supervisor) owns exactly one
//! project's fleet (its own worktree root, state file and hook settings). The
//! workspace lifts ownership a level: it spins up one supervisor per registered
//! [`ProjectDescriptor`](crate::registry::ProjectDescriptor) and routes
//! launch/cancel by `project_id` — so agents started in one project keep running
//! while you work in another. **Backing out of a project never cancels its agents
//! — only [`shutdown`](WorkspaceHandle::shutdown) does**, and it fans out across
//! every project's tracker.
//!
//! Under v1.6's managed workspaces, building a project's plane is also where its
//! repos are *provisioned*: the project's primary repo is materialised through the
//! 3-layer git model ([`crate::mirror`]) and the worktree manager is re-rooted at
//! that reference clone. So "switch into / open a project" and "materialise its
//! isolated workspace" are one mechanism — [`build_plane`].
//!
//! Like the supervisor, the workspace lives entirely inside its own task; the
//! cockpit holds a cheap, cloneable [`WorkspaceHandle`]. A project's plane is
//! built lazily on first launch or switch (the active project's is built eagerly
//! at boot and handed in), so entering a not-yet-opened project clones its repos,
//! reconciles its store against its live worktrees and rehydrates its fleet view.

use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::backend::SpawnFn;
use crate::event::{AppEvent, AppEventTx};
use crate::registry::Registry;
use crate::session::{AgentStatus, SessionStore};
use crate::supervisor::{RepoProvision, Supervisor, SupervisorConfig, SupervisorHandle};
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
    /// Assemble a [`SupervisorConfig`] from this builder plus the project's own
    /// id, hooks directory, worktree manager and session store. Shared by the
    /// eager (active-project, at boot) and lazy (background-project) paths so the
    /// config is assembled in exactly one place.
    pub fn supervisor_config(
        &self,
        project_id: &str,
        hooks_dir: PathBuf,
        worktree: WorktreeManager,
        provision: RepoProvision,
        store: Arc<Mutex<SessionStore>>,
    ) -> SupervisorConfig {
        SupervisorConfig {
            project_id: project_id.to_string(),
            worktree,
            provision,
            store,
            events: self.events.clone(),
            spawn: Arc::clone(&self.spawn),
            exe: self.exe.clone(),
            hook_port: self.hook_port,
            hook_token: self.hook_token.clone(),
            hooks_dir,
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

/// Where [`build_plane`] surfaces first-materialisation clone progress. The slow
/// `git clone --mirror` only runs the first time a project's repo is provisioned;
/// these two callers see it in different places:
///
/// - [`CloneProgressOut::Footer`] — the in-cockpit path (switch / lazy open): the
///   render loop is live, so each tick rides an [`AppEvent::MaterializeProgress`]
///   to the footer.
/// - [`CloneProgressOut::Stderr`] — the eager boot build, which runs *before* the
///   TUI starts: there is no render loop yet, so it writes git's own style of
///   in-place meter straight to stderr.
#[derive(Clone, Copy)]
pub enum CloneProgressOut {
    Footer,
    Stderr,
}

/// Reconcile a project's store against its live worktrees and rehydrate the fleet
/// view from the durable store — the "the process is disposable, the conversation is
/// durable" restart behaviour, now **bidirectional** (ENG-540):
///
/// 1. **Materialise-up** — re-provision each resumable session's recorded repo set
///    ([`crate::session::Session::repos`]) from the durable store: ensure each L2
///    clone (which `fsck`-self-heals a broken alternate) and re-create its worktree
///    (idempotent, reusing the kept branch). A crash mid-clone / mid-worktree thus
///    self-heals on restart, and a resumable multi-repo agent comes back with all of
///    its repos. Warn-never-abort: a repo that can't be re-provisioned (offline, a
///    handle no longer a candidate) is skipped, not fatal.
/// 2. **Prune-down** — drop sessions whose (primary) worktree is gone.
/// 3. **Downgrade** — re-emit each surviving session's status, resolving a was-live
///    `Spawning`/`Running` (its process died with the old run) to `Idle` — resumable,
///    not falsely live.
///
/// Returns the was-live set (the auto-resume / cockpit-restore candidates).
pub fn reconcile_and_rehydrate(
    worktree: &WorktreeManager,
    provision: &RepoProvision,
    store: &Arc<Mutex<SessionStore>>,
    events: &AppEventTx,
    project_id: &str,
) -> HashSet<String> {
    // Materialise-up first, so the prune-down below sees a rebuilt worktree as live.
    // Only resumable (was-live) sessions with a recorded repo set are rebuilt — a
    // terminal session doesn't need its checkout back, and an empty repo set (a
    // pre-ENG-536 record) leaves the existing prune behaviour untouched.
    let to_rebuild: Vec<(String, Vec<String>)> = match store.lock() {
        Ok(store) => store
            .sessions()
            .filter(|s| s.status.is_live() && !s.repos.is_empty())
            .map(|s| (s.issue.clone(), s.repos.clone()))
            .collect(),
        Err(_) => Vec::new(),
    };
    for (issue, repos) in &to_rebuild {
        for handle in repos {
            if let Err(e) = materialize_session_repo(provision, issue, handle) {
                let _ = events.send(AppEvent::Notification(format!(
                    "reconcile {issue}/{handle}: {e}"
                )));
            }
        }
    }

    // Prune-down only when the live set was actually OBSERVED. `list()` fails on a
    // transient error (a stale `index.lock`/worktree lock held by a concurrent git op,
    // a not-yet-pruned repo) — and `unwrap_or_default()` would turn that into an empty
    // live set, so `reconcile([])` would prune EVERY session and `save()` would persist
    // the wipe, destroying the durable conversation index over a recoverable read error
    // (the inverse of warn-never-abort). On Err we skip the prune entirely, leaving the
    // store intact, and still run the downgrade pass below.
    match worktree.list() {
        Ok(worktrees) => {
            let live: Vec<String> = worktrees.into_iter().map(|w| w.issue).collect();
            if let Ok(mut store) = store.lock() {
                store.reconcile(live);
                let _ = store.save();
            }
        }
        Err(e) => {
            let _ = events.send(AppEvent::Notification(format!(
                "reconcile {project_id}: worktree list failed, keeping all sessions ({e})"
            )));
        }
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

/// Re-provision one repo of a session for the bidirectional reconcile: refresh the
/// mirror (throttled, so a rebuild isn't from a stale cache), ensure the L2 clone
/// (which `fsck`-self-heals a broken alternate, or rebuilds the clone), and re-create
/// the per-issue worktree (idempotent — `create` prefers the issue's kept branch over
/// cutting a fresh one, so committed work is preserved). The post-commit hook is
/// **not** (re)installed here: the launch/resume path rewrites it with the current
/// run's port+token before the agent runs (the stale-port discipline), so reconcile
/// only needs the checkout present on disk.
fn materialize_session_repo(
    provision: &RepoProvision,
    issue: &str,
    handle: &str,
) -> Result<(), String> {
    let entry = provision
        .candidates
        .get(handle)
        .ok_or_else(|| format!("`{handle}` is no longer a candidate"))?;
    let _ = crate::mirror::refresh_mirror(&provision.layout, entry);
    let clone = crate::mirror::ensure_clone(&provision.layout, &provision.project_handle, entry)
        .map_err(|e| e.to_string())?;
    let worktrees_root = provision.layout.worktrees_dir(&provision.project_handle);
    let mgr =
        WorktreeManager::with_layout(&clone, &provision.branch_prefix, &worktrees_root, handle)
            .map_err(|e| e.to_string())?;
    mgr.create(issue, "", "HEAD").map_err(|e| e.to_string())?;
    Ok(())
}

/// Tear down a finished issue's workspace (ENG-541): for each repo the issue
/// materialised, **push its branch first** (so committed work is safe on the true
/// remote — skipped for a local-only repo, whose origin is the synthesised mirror),
/// then remove the worktree, **keeping the branch**. The L2 clones and the shared
/// mirror are left intact (reference-counted separately by the reclaim prompt) — only
/// the per-issue worktrees are reclaimed. Best-effort and warn-never-abort: a repo
/// whose push is **rejected keeps its worktree** (its work may not be safe yet),
/// surfaced as a footer, so unpushed work is never silently discarded; the session
/// record is dropped only when every repo's worktree was reclaimed.
fn teardown_issue(
    registry: &Registry,
    store: &Arc<Mutex<SessionStore>>,
    events: &AppEventTx,
    project_id: &str,
    issue: &str,
) {
    let Ok(descriptor) = registry.project(project_id).cloned() else {
        return;
    };
    let layout = registry.layout();
    let prefix = descriptor
        .branch_prefix
        .clone()
        .unwrap_or_else(crate::worktree::default_branch_prefix);
    let repos = match store.lock() {
        Ok(s) => s
            .get(issue)
            .map(|sess| sess.repos.clone())
            .unwrap_or_default(),
        Err(_) => return,
    };
    let repos = if repos.is_empty() {
        vec![descriptor.primary.clone()]
    } else {
        repos
    };

    let mut all_removed = true;
    for handle in &repos {
        let Some(entry) = registry.repo(handle).cloned() else {
            continue;
        };
        let clone = layout.repo_clone_path(&descriptor.handle, handle);
        if !clone.exists() {
            continue;
        }
        let worktrees_root = layout.worktrees_dir(&descriptor.handle);
        let Ok(mgr) = WorktreeManager::with_layout(&clone, &prefix, &worktrees_root, handle) else {
            continue;
        };
        let wt_path = mgr.worktree_path(issue);
        if crate::mirror::can_push_to_remote(&entry)
            && wt_path.is_dir()
            // Serialize against an in-flight auto-push of the same handle (shared
            // per-handle push mutex) so teardown's push can't lose a race on git's
            // local ref lock and report a phantom "push rejected".
            && let Err(e) = crate::notify::push_head_serialized(handle, &wt_path)
        {
            let _ = events.send(AppEvent::Notification(format!(
                "teardown {issue}/{handle}: push rejected, keeping worktree ({e})"
            )));
            all_removed = false;
            continue;
        }
        if let Err(e) = mgr.remove(issue) {
            let _ = events.send(AppEvent::Notification(format!(
                "teardown {issue}/{handle}: {e}"
            )));
            all_removed = false;
        }
    }

    // Tear down the issue's scratch datastores (ENG-561) recorded at launch, running
    // each stored (already-substituted) teardown. Each that SUCCEEDS is pruned from the
    // record; a failure keeps only that record, so a re-discard (e.g. after a repo push
    // retry) retries just the still-undone teardowns. Without per-record pruning, a
    // non-idempotent teardown (`dropdb` errors once the DB is gone) would re-run on
    // every retry, keep `all_removed` false, and wedge the discard forever.
    let scratch = match store.lock() {
        Ok(s) => s
            .get(issue)
            .map(|sess| sess.scratch.clone())
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let mut remaining = Vec::new();
    for record in scratch {
        if let Err(e) = crate::scratch::teardown(&record) {
            let _ = events.send(AppEvent::Notification(format!(
                "teardown {issue}/{}: {e}",
                record.name
            )));
            all_removed = false;
            remaining.push(record);
        }
    }

    if let Ok(mut s) = store.lock() {
        if all_removed {
            s.forget(issue);
        } else {
            // Keep the session, but persist the pruned scratch set so a retry doesn't
            // re-run a teardown that already succeeded.
            s.set_scratch(issue, remaining);
        }
        let _ = s.save();
    }
}

/// Materialise a fenced + confirmed lazy-pull (ENG-542): the agent requested an
/// extra repo and the human confirmed it. **Re-fence** the handle to the project's
/// candidate set (the loopback POST is forgeable, so we never trust the CLI's fence
/// alone) and re-validate it is path-safe, then run L1→L2 ([`crate::mirror::ensure_clone`]
/// takes the per-handle mirror `flock` internally), install the post-commit hook with
/// THIS run's port+token (the stale-port trap), create the L3 worktree, regenerate
/// `WORKSPACE.md` over the full set, and update [`Session::repos`] **last** — the
/// "tell" step is strictly last, so a crash mid-pull never leaves the set claiming a
/// repo that isn't on disk and the agent never sees a half-materialised repo.
/// `--add-dir` is argv-baked, so full tool access is honestly deferred to a resume.
///
/// [`Session::repos`]: crate::session::Session::repos
#[allow(clippy::too_many_arguments)]
fn materialize_repo_request(
    registry: &Registry,
    exe: &str,
    port: u16,
    token: &str,
    store: &Arc<Mutex<SessionStore>>,
    events: &AppEventTx,
    project_id: &str,
    issue: &str,
    handle: &str,
) {
    let notify = |msg: String| {
        let _ = events.send(AppEvent::Notification(msg));
    };
    let Ok(descriptor) = registry.project(project_id).cloned() else {
        return;
    };
    if !crate::registry::is_safe_handle(handle)
        || !descriptor.candidates.iter().any(|h| h == handle)
    {
        notify(format!(
            "repo `{handle}` is not a candidate of {} — pull denied",
            descriptor.name
        ));
        return;
    }
    let Some(entry) = registry.repo(handle).cloned() else {
        notify(format!("repo `{handle}` is not registered"));
        return;
    };
    let layout = registry.layout();
    let prefix = descriptor
        .branch_prefix
        .clone()
        .unwrap_or_else(crate::worktree::default_branch_prefix);
    let worktrees_root = layout.worktrees_dir(&descriptor.handle);

    // Best-effort, throttled mirror refresh so a (re)built clone forks off current
    // upstream rather than the mirror's state at first create.
    let _ = crate::mirror::refresh_mirror(layout, &entry);
    let clone = match crate::mirror::ensure_clone(layout, &descriptor.handle, &entry) {
        Ok(c) => c,
        Err(e) => return notify(format!("pulling `{handle}` into {issue}: {e}")),
    };
    let _ = crate::notify::write_post_commit_hook(&clone, exe, port, token, handle);
    let mgr = match WorktreeManager::with_layout(&clone, &prefix, &worktrees_root, handle) {
        Ok(m) => m,
        Err(e) => return notify(format!("pulling `{handle}` into {issue}: {e}")),
    };
    // The title isn't known here; create() reuses any existing `<prefix>/<issue>…`
    // branch (cut by a sibling repo at launch) over minting a fresh one, so the
    // issue's repos stay on the same branch name.
    if let Err(e) = mgr.create(issue, "", "HEAD") {
        return notify(format!("pulling `{handle}` into {issue}: {e}"));
    }

    // Commit the new repo set to the store, then regenerate the manifest over it —
    // the "tell" step strictly last. The set is unioned with the CURRENT durable set
    // re-read UNDER the write lock, not a stale earlier read, so two concurrent pulls
    // for the same issue (each adding a different repo) can't lose-update each other:
    // each union is computed against the freshest set under the lock that writes it.
    let repos = match store.lock() {
        Ok(mut s) => {
            let mut cur = s.get(issue).map(|x| x.repos.clone()).unwrap_or_default();
            if cur.is_empty() {
                cur.push(descriptor.primary.clone());
            }
            if !cur.iter().any(|h| h == handle) {
                cur.push(handle.to_string());
            }
            s.set_repos(issue, cur.clone());
            let _ = s.save();
            cur
        }
        Err(_) => return,
    };

    let mut manifest = Vec::new();
    for h in &repos {
        let c = layout.repo_clone_path(&descriptor.handle, h);
        if let Ok(m) = WorktreeManager::with_layout(&c, &prefix, &worktrees_root, h)
            && let Ok(Some(wt)) = m.find(issue)
        {
            manifest.push((h.clone(), wt.branch));
        }
    }
    let workspace_dir = layout.issue_workspace_dir(&descriptor.handle, issue);
    let _ = crate::worktree::write_workspace_md(&workspace_dir, issue, &manifest);

    notify(format!(
        "pulled `{handle}` into {issue} — resume the agent for full tool access"
    ));
}

/// Re-emit every session's CURRENT status for `project_id` (no restart
/// downgrade), so switching into an already-running project repopulates its fleet
/// view without falsely idling agents that are still live. A *fresh* plane gets
/// this via [`reconcile_and_rehydrate`] in [`build_plane`] instead (which DOES
/// downgrade, because a never-started project's records are from a dead process).
fn reemit_statuses(store: &Arc<Mutex<SessionStore>>, events: &AppEventTx, project_id: &str) {
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

/// Build a project's plane off the render thread: **materialise its primary repo**
/// through the 3-layer git model ([`crate::mirror::ensure_clone`]), re-root the
/// worktree manager at that reference clone, open the per-project session store
/// (under `~/.lindep/projects/<handle>/`), reconcile + rehydrate, then start its
/// supervisor. The blocking pieces (the git clones, canonicalising the worktree
/// root, reading the state file, listing worktrees) run on the blocking pool so
/// the workspace command loop never stalls. Returns the started plane **and** the
/// was-live resumable set from rehydration (the auto-resume / cockpit-restore
/// candidates the boot caller wants; the lazy switch path discards it), or `None`
/// (with a footer line) if the project isn't registered or its primary repo can't
/// be provisioned.
pub async fn build_plane(
    rt: &Handle,
    builder: &PlaneBuilder,
    registry: &Registry,
    project_id: &str,
    stores: &StoreRegistry,
    progress: CloneProgressOut,
) -> Option<(ProjectPlane, HashSet<String>)> {
    // Resolve the project + its primary repo against the registry.
    let descriptor = match registry.project(project_id) {
        Ok(d) => d.clone(),
        Err(e) => {
            let _ = builder.events.send(AppEvent::Notification(e.to_string()));
            return None;
        }
    };
    let Some(primary) = registry.repo(&descriptor.primary).cloned() else {
        let _ = builder.events.send(AppEvent::Notification(format!(
            "project {}: primary repo `{}` is not registered",
            descriptor.name, descriptor.primary
        )));
        return None;
    };
    let layout = registry.layout().clone();
    let project_handle = descriptor.handle.clone();

    // The provisioning context lets each launch materialise its up-front-selected
    // secondary repos (ENG-536) beyond the primary, and lets the bidirectional
    // reconcile (ENG-540) rebuild a crashed session's repos from its handle set —
    // both fenced to this project's candidate set (the trust boundary).
    let provision = RepoProvision {
        layout: layout.clone(),
        project_handle: project_handle.clone(),
        branch_prefix: descriptor
            .branch_prefix
            .clone()
            .unwrap_or_else(crate::worktree::default_branch_prefix),
        primary: descriptor.primary.clone(),
        candidates: descriptor
            .candidates
            .iter()
            .filter_map(|h| registry.repo(h).map(|e| (h.clone(), e.clone())))
            .collect(),
        scratch: descriptor.scratch.clone(),
    };

    // Materialise the primary repo (mirror → reference clone) and re-root the
    // worktree manager there — all on the blocking pool, since `git clone` is slow.
    // The first time, that clone is a real hundreds-of-MB `git clone --mirror`, so
    // it streams `--progress` to `progress` (footer in-cockpit, stderr at boot)
    // rather than leaving the cockpit looking frozen for 30 s+.
    let worktree = {
        let layout = layout.clone();
        let handle = project_handle.clone();
        let prefix = descriptor.branch_prefix.clone();
        let primary = primary.clone();
        let exe = builder.exe.to_string_lossy().into_owned();
        let port = builder.hook_port;
        let token = builder.hook_token.clone();
        let events = builder.events.clone();
        let pid = project_id.to_string();
        let pname = descriptor.name.clone();
        // Did the clone actually draw a progress meter (vs. an already-present mirror
        // that returns instantly)? Shared with the sink on the blocking thread so
        // EVERY result arm below — success, clone error, or task panic — can close
        // the meter out: the boot path owes a trailing newline, the footer path a
        // terminal "materialised …" line. `Arc<AtomicBool>` because the sink runs on
        // another thread; the post-`await` load happens-after it via the join.
        let emitted = Arc::new(AtomicBool::new(false));
        let result = tokio::task::spawn_blocking({
            let emitted = Arc::clone(&emitted);
            move || -> Result<WorktreeManager, String> {
                // Best-effort, throttled mirror refresh so a (re)built clone forks off
                // current upstream rather than the mirror's state at first create.
                let _ = crate::mirror::refresh_mirror(&layout, &primary);
                let sink = |phase: &str, percent: u8| match progress {
                    CloneProgressOut::Footer => {
                        emitted.store(true, Ordering::Relaxed);
                        let _ = events.send(AppEvent::MaterializeProgress {
                            project_id: pid.clone(),
                            phase: phase.to_string(),
                            percent,
                        });
                    }
                    // Pre-TUI: an in-place meter, but only when stderr is a real
                    // terminal — a redirected stderr (`lindep 2>log`) must not collect
                    // `\r`/escape chatter. `\x1b[K` clears a longer previous phase; the
                    // closing newline is printed by the caller once the clone finishes.
                    CloneProgressOut::Stderr => {
                        let mut err = std::io::stderr();
                        if err.is_terminal() {
                            emitted.store(true, Ordering::Relaxed);
                            let _ =
                                write!(err, "\r  materialising {pname} · {phase} {percent}%\x1b[K");
                            let _ = err.flush();
                        }
                    }
                };
                let sink_ref: crate::mirror::ProgressFn = &sink;
                let clone = crate::mirror::ensure_clone_with_progress(
                    &layout,
                    &handle,
                    &primary,
                    Some(sink_ref),
                )
                .map_err(|e| e.to_string())?;
                // Install/refresh the v1.6 auto-push post-commit hook with THIS run's
                // port + token (the stale-port trap). Hooks are shared per L2 clone, so
                // one install covers every worktree of this (project, repo).
                let _ = crate::notify::write_post_commit_hook(
                    &clone,
                    &exe,
                    port,
                    &token,
                    &primary.handle,
                );
                let worktrees_root = layout.worktrees_dir(&handle);
                let prefix = prefix.unwrap_or_else(crate::worktree::default_branch_prefix);
                WorktreeManager::with_layout(&clone, prefix, &worktrees_root, &primary.handle)
                    .map_err(|e| e.to_string())
            }
        })
        .await;

        let drew_meter = emitted.load(Ordering::Relaxed);
        // Close the boot meter's in-place line in EVERY arm so later output (or the
        // error line) starts fresh — the success-only newline was the asymmetry that
        // left a dangling meter when a clone failed mid-download.
        let close_stderr_meter = || {
            if drew_meter && matches!(progress, CloneProgressOut::Stderr) {
                eprintln!();
            }
        };
        match result {
            Ok(Ok(worktree)) => {
                close_stderr_meter();
                // In-cockpit: settle the lingering "materialising … 100%" tick into a
                // terminal "materialised …" line (only if we actually drew progress —
                // the fast path leaves the "switched to …" footer untouched).
                if drew_meter && matches!(progress, CloneProgressOut::Footer) {
                    let _ = builder.events.send(AppEvent::MaterializeDone {
                        project_id: project_id.to_string(),
                    });
                }
                worktree
            }
            Ok(Err(e)) => {
                close_stderr_meter();
                // The footer path's error Notification below replaces any stale tick.
                let _ = builder.events.send(AppEvent::Notification(format!(
                    "project {}: can't provision repo: {e}",
                    descriptor.name
                )));
                return None;
            }
            Err(_) => {
                close_stderr_meter();
                // The blocking provision task panicked. Surface it like the clone-error
                // arm, or the user-initiated switch/launch returns None and the cockpit
                // just silently does nothing (the docstring promises a footer line).
                let _ = builder.events.send(AppEvent::Notification(format!(
                    "project {}: provisioning task crashed",
                    descriptor.name
                )));
                return None;
            }
        }
    };

    let state_path = layout.state_path(&project_handle);
    let pid = descriptor.project_id.clone();
    let store = match tokio::task::spawn_blocking({
        let state_path = state_path.clone();
        let pid = pid.clone();
        move || SessionStore::open_project(&pid, state_path)
    })
    .await
    {
        Ok(Ok(store)) => store,
        // A state file from a NEWER lindep must not be clobbered with our older
        // format — leave it untouched and skip building this plane.
        Ok(Err(e @ crate::session::StateError::Version { .. })) => {
            let _ = builder.events.send(AppEvent::Notification(format!(
                "project {}: {e}",
                descriptor.name
            )));
            return None;
        }
        Ok(Err(e)) => {
            let _ = builder.events.send(AppEvent::Notification(format!(
                "project {}: session state unreadable ({e}); starting fresh",
                descriptor.name
            )));
            SessionStore::empty(state_path).for_project(&descriptor.project_id)
        }
        Err(_) => {
            let _ = builder.events.send(AppEvent::Notification(format!(
                "project {}: session-state task crashed",
                descriptor.name
            )));
            return None;
        }
    };
    let store = Arc::new(Mutex::new(store));
    // Register the store so the notification bus and the global view can find this
    // project's sessions even while you're inside another project.
    if let Ok(mut reg) = stores.lock() {
        reg.insert(descriptor.project_id.clone(), Arc::clone(&store));
    }

    // Reconcile + rehydrate off the workers (worktree.list() + any materialise-up
    // shells out to git). Bidirectional (ENG-540): rebuild a crashed session's repos
    // from its durable handle set, then prune-down + downgrade was-live to Idle.
    let resumable = {
        let worktree = worktree.clone();
        let provision = provision.clone();
        let store = Arc::clone(&store);
        let events = builder.events.clone();
        let project_id = descriptor.project_id.clone();
        let events_for_panic = builder.events.clone();
        let name = descriptor.name.clone();
        tokio::task::spawn_blocking(move || {
            reconcile_and_rehydrate(&worktree, &provision, &store, &events, &project_id)
        })
        .await
        .unwrap_or_else(|_| {
            let _ = events_for_panic.send(AppEvent::Notification(format!(
                "project {name}: reconcile task crashed"
            )));
            HashSet::new()
        })
    };

    let hooks_dir = layout.hooks_dir(&project_handle);
    let cfg = builder.supervisor_config(
        &descriptor.project_id,
        hooks_dir,
        worktree,
        provision,
        store,
    );
    let (handle, join) = Supervisor::start(cfg, rt);
    Some((ProjectPlane { handle, join }, resumable))
}

/// Commands the workspace processes, each addressed by `project_id` so the right
/// project's supervisor handles it.
enum WorkspaceCommand {
    Launch {
        project_id: String,
        issue: String,
        title: String,
        size: Option<(u16, u16)>,
        /// Up-front-selected repo handles beyond the primary (ENG-536); empty for a
        /// single-repo launch.
        repos: Vec<String>,
    },
    Cancel {
        project_id: String,
        issue: String,
    },
    /// Tear down a finished issue's workspace (ENG-541): push each repo's branch,
    /// then remove the per-issue worktrees (keeping branches). Runs on the blocking
    /// pool — best-effort, never blocking the command loop.
    Teardown {
        project_id: String,
        issue: String,
    },
    /// Materialise a confirmed lazy-pull (ENG-542): clone + worktree an extra repo
    /// into a running issue's workspace, then update its repo set. Re-fenced to the
    /// candidate set and run on the blocking pool.
    MaterializeRepo {
        project_id: String,
        issue: String,
        repo_handle: String,
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
        self.launch_with_repos(project_id, issue, title, size, Vec::new());
    }

    /// Launch with an explicit up-front repo selection (ENG-536). The project's
    /// primary is always materialised; `repos` adds the rest of the chosen set.
    pub fn launch_with_repos(
        &self,
        project_id: String,
        issue: String,
        title: String,
        size: Option<(u16, u16)>,
        repos: Vec<String>,
    ) {
        let _ = self.cmd_tx.send(WorkspaceCommand::Launch {
            project_id,
            issue,
            title,
            size,
            repos,
        });
    }

    /// Stop a single agent in `project_id`, leaving every other agent — in this
    /// project and others — running.
    pub fn cancel(&self, project_id: String, issue: String) {
        let _ = self
            .cmd_tx
            .send(WorkspaceCommand::Cancel { project_id, issue });
    }

    /// Tear down a finished issue's workspace (ENG-541): push each repo's branch,
    /// then remove its per-issue worktrees (keeping branches). Only meaningful for a
    /// non-live agent — the cockpit gates it on that.
    pub fn teardown(&self, project_id: String, issue: String) {
        let _ = self
            .cmd_tx
            .send(WorkspaceCommand::Teardown { project_id, issue });
    }

    /// Materialise a confirmed lazy-pull (ENG-542): clone + worktree `repo_handle`
    /// into `(project_id, issue)`'s workspace and grow its repo set. Re-fenced to the
    /// candidate set inside the workspace.
    pub fn materialize_repo(&self, project_id: String, issue: String, repo_handle: String) {
        let _ = self.cmd_tx.send(WorkspaceCommand::MaterializeRepo {
            project_id,
            issue,
            repo_handle,
        });
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

    /// A handle whose receiver is dropped — sends are silently discarded. Lets a
    /// cockpit unit test exercise the launch-button / repo-select flow (which needs
    /// a `Some(workspace)`) without standing up a real workspace task.
    #[cfg(test)]
    pub(crate) fn detached() -> Self {
        let (cmd_tx, _rx) = mpsc::unbounded_channel();
        WorkspaceHandle { cmd_tx }
    }
}

/// The workspace owner. Lives inside its own task; holds one [`ProjectPlane`]
/// per project it has started, and the ingredients to start more on demand.
pub struct Workspace {
    rt: Handle,
    builder: PlaneBuilder,
    registry: Registry,
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
        registry: Registry,
        initial: HashMap<String, ProjectPlane>,
        stores: StoreRegistry,
    ) -> (WorkspaceHandle, JoinHandle<()>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let spawn_handle = rt.clone();
        let workspace = Workspace {
            rt,
            builder,
            registry,
            planes: initial,
            stores,
        };
        let join = spawn_handle.spawn(workspace.run(cmd_rx));
        (WorkspaceHandle { cmd_tx }, join)
    }

    // The command loop is a single serial task. A `Launch`/`Activate` of a not-yet-
    // built project `.await`s `ensure_plane` → `build_plane`, which can run a slow
    // first-time `git clone --mirror` (hundreds of MB) — so a command sent during a
    // cold first-clone (including `Cancel` of another project, or `Shutdown`) waits
    // behind it. This is bounded, not a hang: the clone runs on the blocking pool so
    // the runtime/render loop/hook server stay responsive, and `ControlPlaneGuard::
    // shutdown` caps the quit wait with `SHUTDOWN_GRACE`. Decoupling the build onto a
    // spawned task would keep the loop draining other commands, but adds per-project
    // "building" dedup + deferred-launch state — only worth it if this latency is
    // observed to matter.
    async fn run(mut self, mut cmd_rx: mpsc::UnboundedReceiver<WorkspaceCommand>) {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                WorkspaceCommand::Launch {
                    project_id,
                    issue,
                    title,
                    size,
                    repos,
                } => {
                    if let Some(handle) = self.ensure_plane(&project_id).await {
                        handle.launch_with_repos(issue, title, size, repos);
                    }
                }
                WorkspaceCommand::Cancel { project_id, issue } => {
                    // Only a started project can have a live agent to cancel; an
                    // unstarted one is a no-op (nothing to stop).
                    if let Some(plane) = self.planes.get(&project_id) {
                        plane.handle.cancel(issue);
                    }
                }
                WorkspaceCommand::Teardown { project_id, issue } => {
                    // Push + remove the issue's worktrees off the command loop (it
                    // shells out to git). Fire-and-forget — the cockpit already
                    // undocked the window; a footer surfaces any per-repo problem.
                    let store = self
                        .stores
                        .lock()
                        .ok()
                        .and_then(|reg| reg.get(&project_id).cloned());
                    if let Some(store) = store {
                        let registry = self.registry.clone();
                        let events = self.builder.events.clone();
                        tokio::task::spawn_blocking(move || {
                            teardown_issue(&registry, &store, &events, &project_id, &issue);
                        });
                    }
                }
                WorkspaceCommand::MaterializeRepo {
                    project_id,
                    issue,
                    repo_handle,
                } => {
                    // Ensure the plane exists (the store + clones must be live), then
                    // pull the repo on the blocking pool — best-effort, off the loop.
                    if self.ensure_plane(&project_id).await.is_some() {
                        let store = self
                            .stores
                            .lock()
                            .ok()
                            .and_then(|reg| reg.get(&project_id).cloned());
                        if let Some(store) = store {
                            let registry = self.registry.clone();
                            let events = self.builder.events.clone();
                            let exe = self.builder.exe.to_string_lossy().into_owned();
                            let port = self.builder.hook_port;
                            let token = self.builder.hook_token.clone();
                            tokio::task::spawn_blocking(move || {
                                materialize_repo_request(
                                    &registry,
                                    &exe,
                                    port,
                                    token.as_str(),
                                    &store,
                                    &events,
                                    &project_id,
                                    &issue,
                                    &repo_handle,
                                );
                            });
                        }
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

    /// Return the supervisor handle for `project_id`, building (provisioning its
    /// repos, reconciling + rehydrating) its plane the first time. `None` if the
    /// project isn't in the registry or its primary repo can't be provisioned —
    /// surfaced as a footer line by [`build_plane`].
    async fn ensure_plane(&mut self, project_id: &str) -> Option<SupervisorHandle> {
        if let Some(plane) = self.planes.get(project_id) {
            return Some(plane.handle.clone());
        }
        let (plane, _resumable) = build_plane(
            &self.rt,
            &self.builder,
            &self.registry,
            project_id,
            &self.stores,
            // The render loop is live here (switch / lazy open), so a slow first
            // clone ticks into the footer rather than looking frozen.
            CloneProgressOut::Footer,
        )
        .await?;
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
    use crate::registry::Layout;
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Build a [`Registry`] under a fresh temp `~/.lindep` mapping each project to
    /// its own **local-only** repo (the temp git repo path), so a plane build
    /// provisions a real mirror + reference clone from it. The project's handle
    /// doubles as its single repo's handle.
    fn registry_with(tag: &str, projects: &[(&str, &str, &Path)]) -> Registry {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("lindep-wsreg-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut body = String::new();
        for (_, handle, local) in projects {
            body.push_str(&format!(
                "[[repo]]\nhandle = \"{handle}\"\nlocal = \"{}\"\n\n",
                local.display()
            ));
        }
        for (id, handle, _) in projects {
            body.push_str(&format!(
                "[[project]]\nid = \"{id}\"\nhandle = \"{handle}\"\nprimary = \"{handle}\"\n\n"
            ));
        }
        std::fs::write(root.join("registry.toml"), body).unwrap();
        let (reg, warnings) = Registry::load_at(Layout::new(&root));
        assert!(warnings.is_empty(), "{warnings:?}");
        reg
    }

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
            log.lock().unwrap().insert(
                (cfg.project_id.clone(), cfg.issue.clone()),
                Arc::clone(&fake),
            );
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

        // Two registered projects, each its own repo (provisioned by the clone
        // substrate from the local repo paths).
        let registry = registry_with("two", &[("proj-a", "a", &repo_a), ("proj-b", "b", &repo_b)]);

        let (ws, join) = Workspace::start(
            Handle::current(),
            b,
            registry,
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

    /// A spawn fn that records each launch's full [`SpawnConfig`] (cwd + args) by
    /// issue, so a test can assert the multi-repo cwd + `--add-dir` wiring (ENG-536).
    type CfgLog = Arc<Mutex<HashMap<String, SpawnConfig>>>;
    fn capturing_spawn(count: Arc<AtomicUsize>, log: CfgLog) -> Arc<SpawnFn> {
        Arc::new(move |cfg: SpawnConfig, _events: AppEventTx| {
            log.lock().unwrap().insert(cfg.issue.clone(), cfg.clone());
            count.fetch_add(1, Ordering::Relaxed);
            Ok(FakeBackend::new(&cfg.issue) as Arc<dyn AgentBackend>)
        })
    }

    /// A registry under a fresh temp `~/.lindep` with one project bound to TWO
    /// local-only repos (`api` primary, `web` secondary) — the multi-repo launch path.
    fn registry_two_repo(tag: &str, api: &Path, web: &Path) -> (Registry, Layout) {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("lindep-mreg-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let body = format!(
            "[[repo]]\nhandle = \"api\"\nlocal = \"{}\"\n\n\
             [[repo]]\nhandle = \"web\"\nlocal = \"{}\"\n\n\
             [[project]]\nid = \"proj\"\nhandle = \"proj\"\n\
             candidates = [\"api\", \"web\"]\nprimary = \"api\"\n",
            api.display(),
            web.display(),
        );
        std::fs::write(root.join("registry.toml"), body).unwrap();
        let layout = Layout::new(&root);
        let (reg, warnings) = Registry::load_at(layout.clone());
        assert!(warnings.is_empty(), "{warnings:?}");
        (reg, layout)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_up_front_multi_select_materialises_each_chosen_repo() {
        let api = temp_repo("mr-api");
        let web = temp_repo("mr-web");
        let (registry, layout) = registry_two_repo("mr", &api, &web);
        let (tx, _rx) = crate::event::channel();
        let spawns = Arc::new(AtomicUsize::new(0));
        let cfgs: CfgLog = Arc::new(Mutex::new(HashMap::new()));
        let b = builder(
            tx,
            capturing_spawn(Arc::clone(&spawns), Arc::clone(&cfgs)),
            Arc::new(AtomicUsize::new(0)),
            10,
        );
        let stores: StoreRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (ws, join) = Workspace::start(
            Handle::current(),
            b,
            registry,
            HashMap::new(),
            Arc::clone(&stores),
        );

        // Launch ENG-1 picking the secondary `web` on top of the always-on `api`.
        ws.launch_with_repos(
            "proj".into(),
            "ENG-1".into(),
            "Feature".into(),
            None,
            vec!["web".into()],
        );
        assert!(
            eventually(|| spawns.load(Ordering::Relaxed) >= 1).await,
            "the multi-repo agent spawns"
        );

        let cfg = cfgs
            .lock()
            .unwrap()
            .get("ENG-1")
            .cloned()
            .expect("a spawn for ENG-1");
        // cwd is the per-issue workspace PARENT (worktrees/<ISSUE>), not one repo.
        assert_eq!(cfg.cwd, layout.issue_workspace_dir("proj", "ENG-1"));
        // Each materialised repo gets its own `--add-dir` (full tool access to each).
        let add_dirs: Vec<&str> = cfg
            .args
            .windows(2)
            .filter(|w| w[0] == "--add-dir")
            .map(|w| w[1].as_str())
            .collect();
        assert_eq!(add_dirs.len(), 2, "one --add-dir per repo: {:?}", cfg.args);
        assert!(add_dirs.iter().any(|d| d.ends_with("/api")));
        assert!(add_dirs.iter().any(|d| d.ends_with("/web")));
        // Both repos are checked out as siblings under the workspace dir…
        let ws_dir = layout.issue_workspace_dir("proj", "ENG-1");
        assert!(
            ws_dir.join("api").is_dir() && ws_dir.join("web").is_dir(),
            "both repos checked out on disk"
        );
        // …a WORKSPACE.md tells the agent which repos exist…
        assert!(
            ws_dir.join("WORKSPACE.md").is_file(),
            "a manifest is written"
        );
        // …and the session durably records the materialised set (for ENG-540 rehydrate).
        let store = stores.lock().unwrap().get("proj").cloned().unwrap();
        let repos = store.lock().unwrap().get("ENG-1").unwrap().repos.clone();
        assert_eq!(repos, vec!["api", "web"]);

        ws.shutdown();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), join).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_launch_survives_a_secondary_repo_that_cant_be_materialised() {
        // Best-effort secondaries (ENG-536): a secondary whose source can't be cloned
        // (here a real-but-non-git path) is skipped, not fatal — the agent still spawns
        // on its primary, and only what actually materialised is recorded as durable, so
        // the offline/reclaimed-mirror case can't strand an otherwise-runnable agent.
        let api = temp_repo("be-api");
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("lindep-be-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let broken = root.join("not-a-git-repo");
        std::fs::create_dir_all(&broken).unwrap();
        let body = format!(
            "[[repo]]\nhandle = \"api\"\nlocal = \"{}\"\n\n\
             [[repo]]\nhandle = \"web\"\nlocal = \"{}\"\n\n\
             [[project]]\nid = \"proj\"\nhandle = \"proj\"\n\
             candidates = [\"api\", \"web\"]\nprimary = \"api\"\n",
            api.display(),
            broken.display(),
        );
        std::fs::write(root.join("registry.toml"), body).unwrap();
        let layout = Layout::new(&root);
        let (registry, warnings) = Registry::load_at(layout.clone());
        assert!(warnings.is_empty(), "{warnings:?}");

        let (tx, _rx) = crate::event::channel();
        let spawns = Arc::new(AtomicUsize::new(0));
        let cfgs: CfgLog = Arc::new(Mutex::new(HashMap::new()));
        let b = builder(
            tx,
            capturing_spawn(Arc::clone(&spawns), Arc::clone(&cfgs)),
            Arc::new(AtomicUsize::new(0)),
            10,
        );
        let stores: StoreRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (ws, join) = Workspace::start(
            Handle::current(),
            b,
            registry,
            HashMap::new(),
            Arc::clone(&stores),
        );

        ws.launch_with_repos(
            "proj".into(),
            "ENG-1".into(),
            "Feature".into(),
            None,
            vec!["web".into()],
        );
        assert!(
            eventually(|| spawns.load(Ordering::Relaxed) >= 1).await,
            "the agent still spawns on its primary despite the broken secondary"
        );

        // The primary checked out; the broken secondary did not, and is recorded nowhere.
        assert!(layout.repo_clone_path("proj", "api").exists());
        let store = stores.lock().unwrap().get("proj").cloned().unwrap();
        let repos = store.lock().unwrap().get("ENG-1").unwrap().repos.clone();
        assert_eq!(repos, vec!["api"], "only the materialised repo is durable");

        ws.shutdown();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), join).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_launch_provisions_scratch_injects_its_env_and_records_it() {
        // ENG-561 end to end: a project's [[scratch]] is provisioned at launch, its
        // resolved env (the spec table + any KEY=VALUE the command prints) is injected
        // into the agent, and the resource is recorded on the session for teardown.
        let api = temp_repo("scr-api");
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("lindep-scr-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let body = format!(
            "[[repo]]\nhandle = \"api\"\nlocal = \"{}\"\n\n\
             [[project]]\nid = \"proj\"\nhandle = \"proj\"\n\
             candidates = [\"api\"]\nprimary = \"api\"\n\n\
             [[project.scratch]]\nname = \"db\"\n\
             provision = \"echo CAPTURED=yes\"\nteardown = \"true\"\n\
             env = {{ SCRATCH_DB = \"scratch_{{slug}}\" }}\n",
            api.display(),
        );
        std::fs::write(root.join("registry.toml"), body).unwrap();
        let layout = Layout::new(&root);
        let (registry, warnings) = Registry::load_at(layout.clone());
        assert!(warnings.is_empty(), "{warnings:?}");

        let (tx, _rx) = crate::event::channel();
        let spawns = Arc::new(AtomicUsize::new(0));
        let cfgs: CfgLog = Arc::new(Mutex::new(HashMap::new()));
        let b = builder(
            tx,
            capturing_spawn(Arc::clone(&spawns), Arc::clone(&cfgs)),
            Arc::new(AtomicUsize::new(0)),
            10,
        );
        let stores: StoreRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (ws, join) = Workspace::start(
            Handle::current(),
            b,
            registry,
            HashMap::new(),
            Arc::clone(&stores),
        );

        ws.launch_with_repos(
            "proj".into(),
            "ENG-1".into(),
            "Feature".into(),
            None,
            vec![],
        );
        assert!(
            eventually(|| spawns.load(Ordering::Relaxed) >= 1).await,
            "the agent spawns"
        );

        let cfg = cfgs.lock().unwrap().get("ENG-1").cloned().expect("a spawn");
        // The spec's env table value is substituted: {slug} for "ENG-1" canonicalises
        // to the readable prefix `eng_1` plus a collision-free hash suffix.
        assert!(
            cfg.env.iter().any(|(k, v)| k == "SCRATCH_DB"
                && v.starts_with("scratch_eng_1")
                && *v == format!("scratch_{}", crate::scratch::slug("ENG-1"))),
            "scratch env is injected: {:?}",
            cfg.env
        );
        // …and a KEY=VALUE printed by the provision command is captured + injected.
        assert!(
            cfg.env
                .contains(&("CAPTURED".to_string(), "yes".to_string())),
            "stdout-captured env is injected: {:?}",
            cfg.env
        );
        // The resource is recorded on the session so teardown/sweep can drop it.
        let store = stores.lock().unwrap().get("proj").cloned().unwrap();
        let scratch = store.lock().unwrap().get("ENG-1").unwrap().scratch.clone();
        assert_eq!(scratch.len(), 1);
        assert_eq!(scratch[0].name, "db");
        assert_eq!(scratch[0].teardown, "true");

        ws.shutdown();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), join).await;
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

        let registry = registry_with("cap", &[("proj-a", "a", &repo_a), ("proj-b", "b", &repo_b)]);
        let (ws, join) = Workspace::start(
            Handle::current(),
            b,
            registry,
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
        assert!(
            at_capacity,
            "the workspace-wide cap rejects the third launch"
        );
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
        // A dummy provision: these sessions have an empty repo set, so the
        // materialise-up pass is skipped and only the prune/downgrade runs.
        let provision = RepoProvision {
            layout: Layout::new(repo.join(".lindep")),
            project_handle: "p".to_string(),
            branch_prefix: "felix".to_string(),
            primary: "p".to_string(),
            candidates: HashMap::new(),
            scratch: Vec::new(),
        };
        let resumable = reconcile_and_rehydrate(&wt, &provision, &store, &tx, "proj-x");

        assert!(
            resumable.contains("ENG-live"),
            "the was-live session is resumable"
        );
        assert!(
            !resumable.contains("ENG-done"),
            "a terminal session is not resumable"
        );
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
    fn bidirectional_reconcile_rebuilds_a_vanished_worktree_from_the_repo_set() {
        // A crash that deleted a resumable session's worktree must be rebuilt on
        // restart from its durable Session.repos (reusing the kept branch), not
        // pruned — otherwise a multi-repo agent strands until a fresh launch.
        let api_repo = temp_repo("recon-api");
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("lindep-recon-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let layout = Layout::new(&root);
        let entry = crate::registry::RepoEntry {
            handle: "api".to_string(),
            remote: None,
            local: Some(api_repo),
        };
        let provision = RepoProvision {
            layout: layout.clone(),
            project_handle: "proj".to_string(),
            branch_prefix: "felix".to_string(),
            primary: "api".to_string(),
            candidates: HashMap::from([("api".to_string(), entry.clone())]),
            scratch: Vec::new(),
        };
        // Materialise the primary clone + a worktree for ENG-1.
        let clone = crate::mirror::ensure_clone(&layout, "proj", &entry).unwrap();
        let wt_root = layout.worktrees_dir("proj");
        let mgr = WorktreeManager::with_layout(&clone, "felix", wt_root, "api").unwrap();
        let worktree = mgr.create("ENG-1", "Feature", "HEAD").unwrap();
        assert!(worktree.path.is_dir());

        let store = Arc::new(Mutex::new(
            SessionStore::open_project("proj", layout.state_path("proj")).unwrap(),
        ));
        {
            let mut s = store.lock().unwrap();
            s.ensure("ENG-1", worktree.path.clone(), worktree.branch.clone());
            s.set_status("ENG-1", AgentStatus::Running);
            s.set_repos("ENG-1", vec!["api".to_string()]);
        }

        // Simulate a crash: the worktree directory is gone.
        std::fs::remove_dir_all(&worktree.path).unwrap();
        assert!(!worktree.path.is_dir());

        let (tx, _rx) = crate::event::channel();
        let resumable = reconcile_and_rehydrate(&mgr, &provision, &store, &tx, "proj");

        assert!(worktree.path.is_dir(), "the vanished worktree was rebuilt");
        assert!(
            store.lock().unwrap().get("ENG-1").is_some(),
            "the session survives (rebuilt, not pruned)"
        );
        assert!(
            resumable.contains("ENG-1"),
            "it's resumable, downgraded to Idle"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn teardown_removes_a_finished_issues_worktrees_but_keeps_the_branch() {
        let api_repo = temp_repo("teardown-api");
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("lindep-teardown-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("registry.toml"),
            format!(
                "[[repo]]\nhandle = \"api\"\nlocal = \"{}\"\n\n\
                 [[project]]\nid = \"proj\"\nhandle = \"proj\"\nprimary = \"api\"\n",
                api_repo.display()
            ),
        )
        .unwrap();
        let layout = Layout::new(&root);
        let (registry, warnings) = Registry::load_at(layout.clone());
        assert!(warnings.is_empty(), "{warnings:?}");

        // Materialise the clone + a worktree for ENG-1.
        let entry = registry.repo("api").unwrap().clone();
        let clone = crate::mirror::ensure_clone(&layout, "proj", &entry).unwrap();
        let mgr =
            WorktreeManager::with_layout(&clone, "felix", layout.worktrees_dir("proj"), "api")
                .unwrap();
        let worktree = mgr.create("ENG-1", "Feature", "HEAD").unwrap();
        let branch = worktree.branch.clone();
        assert!(worktree.path.is_dir());

        let store = Arc::new(Mutex::new(
            SessionStore::open_project("proj", layout.state_path("proj")).unwrap(),
        ));
        {
            let mut s = store.lock().unwrap();
            s.ensure("ENG-1", worktree.path.clone(), worktree.branch.clone());
            s.set_status("ENG-1", AgentStatus::Done);
            s.set_repos("ENG-1", vec!["api".to_string()]);
        }

        let (tx, _rx) = crate::event::channel();
        teardown_issue(&registry, &store, &tx, "proj", "ENG-1");

        // The worktree is gone and the session forgotten, but the branch is KEPT
        // (its commits outlive the disposable checkout — a re-launch resumes it).
        assert!(!worktree.path.is_dir(), "the worktree was removed");
        assert!(
            store.lock().unwrap().get("ENG-1").is_none(),
            "the session was forgotten"
        );
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&clone)
            .args(["branch", "--list", &branch])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&out.stdout).contains(&branch),
            "the branch is kept after teardown"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Start ENG-1 in `proj` with just the primary `api` materialised, returning the
    /// registry, layout and the store — the fixture both lazy-pull tests build on.
    fn started_single_repo_issue(tag: &str) -> (Registry, Layout, Arc<Mutex<SessionStore>>) {
        let api = temp_repo(&format!("{tag}-api"));
        let web = temp_repo(&format!("{tag}-web"));
        let (registry, layout) = registry_two_repo(tag, &api, &web);
        let entry_api = registry.repo("api").unwrap().clone();
        let clone = crate::mirror::ensure_clone(&layout, "proj", &entry_api).unwrap();
        let mgr =
            WorktreeManager::with_layout(&clone, "felix", layout.worktrees_dir("proj"), "api")
                .unwrap();
        let wt = mgr.create("ENG-1", "Feature", "HEAD").unwrap();
        let store = Arc::new(Mutex::new(
            SessionStore::open_project("proj", layout.state_path("proj")).unwrap(),
        ));
        {
            let mut s = store.lock().unwrap();
            s.ensure("ENG-1", wt.path.clone(), wt.branch.clone());
            s.set_repos("ENG-1", vec!["api".to_string()]);
        }
        (registry, layout, store)
    }

    #[test]
    fn a_confirmed_lazy_pull_materialises_an_in_candidate_repo_and_grows_the_set() {
        let (registry, layout, store) = started_single_repo_issue("pull");
        let (tx, _rx) = crate::event::channel();

        materialize_repo_request(
            &registry, "lindep", 1, "", &store, &tx, "proj", "ENG-1", "web",
        );

        // The pulled repo's worktree is on disk, the durable set grew (tell step last),
        // and WORKSPACE.md now lists both repos.
        let web_wt = layout.worktrees_dir("proj").join("ENG-1").join("web");
        assert!(web_wt.is_dir(), "the pulled repo's worktree exists");
        let repos = store.lock().unwrap().get("ENG-1").unwrap().repos.clone();
        assert_eq!(repos, vec!["api", "web"]);
        let md = std::fs::read_to_string(
            layout
                .issue_workspace_dir("proj", "ENG-1")
                .join("WORKSPACE.md"),
        )
        .unwrap();
        assert!(md.contains("web"), "the manifest names the pulled repo");
    }

    #[test]
    fn a_lazy_pull_unions_with_the_current_durable_set_under_the_lock() {
        // The repos write re-reads the CURRENT set under the write lock and unions, so a
        // concurrent pull that added another repo isn't lost. Simulate that prior add by
        // seeding the durable set with `db`, then pull `web`: the result keeps both.
        let (registry, layout, store) = started_single_repo_issue("union");
        store
            .lock()
            .unwrap()
            .set_repos("ENG-1", vec!["api".to_string(), "db".to_string()]);
        let (tx, _rx) = crate::event::channel();

        materialize_repo_request(
            &registry, "lindep", 1, "", &store, &tx, "proj", "ENG-1", "web",
        );

        assert_eq!(
            store.lock().unwrap().get("ENG-1").unwrap().repos,
            vec!["api", "db", "web"],
            "the pull unions with the freshly-read set, never overwriting a concurrent add"
        );
        let _ = layout;
    }

    #[test]
    fn an_out_of_candidate_lazy_pull_is_denied_and_changes_nothing() {
        let (registry, _layout, store) = started_single_repo_issue("deny");
        let (tx, mut rx) = crate::event::channel();

        // `evil` is not a candidate → denied; nothing materialised, the set unchanged.
        materialize_repo_request(
            &registry, "lindep", 1, "", &store, &tx, "proj", "ENG-1", "evil",
        );

        assert_eq!(
            store.lock().unwrap().get("ENG-1").unwrap().repos,
            vec!["api"],
            "the repo set is unchanged"
        );
        let mut denied = false;
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::Notification(m) = ev
                && m.contains("not a candidate")
            {
                denied = true;
            }
        }
        assert!(denied, "an out-of-candidate pull surfaces a denial footer");
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
            registry_with("empty", &[]),
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
