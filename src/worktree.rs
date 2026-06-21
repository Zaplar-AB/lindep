//! Git worktree manager — one worktree + branch per Linear issue.
//!
//! Each agent works in isolation in its own checkout so concurrent agents never
//! step on each other's files. We shell out to `git` (libgit2's worktree API is
//! thin and its edge cases are worse than the CLI's) and reconcile against
//! `git worktree list --porcelain`, so a crash that leaves a half-made worktree
//! behind is detectable and recoverable on the next run.
//!
//! Layout is deterministic. A **standalone** manager (legacy / tests) keeps a
//! worktree per issue beside the repo:
//! * worktree path — `<repo>/.lindep/worktrees/<ISSUE>` (`.lindep/` is gitignored)
//! * branch — `<prefix>/<issue>-<slug>`, e.g. `felix/eng-392-spike-embed-a-pty`
//!
//! Under v1.6's managed workspaces the manager is re-rooted at an L2 reference
//! clone ([`crate::mirror`]) and given an explicit `worktrees_root` plus a repo
//! `handle`, so per-issue worktrees hang off the *project* directory with a
//! per-repo leaf — letting one issue carry several repos side by side:
//! * worktree path — `<project>/worktrees/<ISSUE>/<handle>`
//! * the per-issue **workspace** (the agent's cwd for a multi-repo issue) is the
//!   shared parent `<project>/worktrees/<ISSUE>`, with each repo a sibling subdir.
//!
//! These functions are **synchronous and blocking**. They are quick, but a
//! caller running on the tokio runtime (the supervisor) should invoke them via
//! `spawn_blocking` so a slow `git` never stalls a runtime worker.
//!
//! **Branch lifetime:** [`WorktreeManager::remove`] removes the working tree and
//! prunes git's admin metadata but deliberately **keeps the branch** — the
//! agent's commits live there and outlive a disposable checkout. Deleting a
//! branch is a separate, explicit decision left to a later phase.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Anything that can go wrong driving `git`.
#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    /// `git` could not be launched at all (not installed / not on `PATH`).
    #[error("could not run git: {0}")]
    Spawn(#[source] std::io::Error),

    /// A `git` invocation exited non-zero.
    #[error("`{command}` failed (exit {code:?}): {stderr}")]
    Git {
        command: String,
        code: Option<i32>,
        stderr: String,
    },

    /// A filesystem operation we perform ourselves (e.g. creating the parent
    /// directory) failed.
    #[error("filesystem error at {}: {source}", .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The branch we would reuse for this issue is already checked out in
    /// another live worktree, so `git worktree add` can't take it (a branch is
    /// checkable out in at most one worktree). Surfaced as a clear error rather
    /// than letting git's raw `is already used by worktree at …` stderr bubble
    /// up looking like user error.
    #[error("branch `{branch}` is already checked out in worktree at {}", .holder.display())]
    BranchInUse { branch: String, holder: PathBuf },

    /// The issue identifier isn't a safe path/branch component — it would escape
    /// the `.lindep/worktrees` sandbox or form an invalid git ref.
    #[error("invalid issue id `{0}`: expected an identifier like `ENG-392`")]
    InvalidIssueId(String),
}

/// A live worktree: the issue it serves, where it lives, and the branch checked
/// out in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub issue: String,
    pub path: PathBuf,
    pub branch: String,
}

/// Creates, lists and removes per-issue worktrees rooted at one repository.
#[derive(Debug, Clone)]
pub struct WorktreeManager {
    /// Canonical path to the main working tree (so `git worktree list` paths,
    /// which git canonicalises, can be matched against ours). Under v1.6 this is
    /// the L2 reference clone for one (project, repo).
    repo_root: PathBuf,
    /// Canonical root every per-issue worktree hangs off. Standalone managers use
    /// `<repo>/.lindep/worktrees`; a v1.6 managed manager uses the shared
    /// `<project>/worktrees`, so every repo of a project nests under one per-issue
    /// directory.
    worktrees_root: PathBuf,
    /// The repo's handle, appended as a leaf under `<ISSUE>/` so several repos of
    /// one issue coexist (`worktrees/<ISSUE>/<handle>`). `None` for a standalone
    /// manager, whose worktree is `worktrees/<ISSUE>` directly (no leaf).
    repo_handle: Option<String>,
    /// Branch namespace, e.g. `felix`. Defaults to `$USER`, then `lindep`.
    branch_prefix: String,
    /// Serializes repo-mutating `git worktree add`/`prune`/`remove` for one L2 clone.
    /// `git worktree` mutates shared repo state (the `worktrees/` admin dir, the ref
    /// store, the common index lock), so two concurrent `create`/`remove` calls — one
    /// per agent, each in its own `spawn_blocking` — can otherwise collide on git's
    /// locks and fail with `could not lock` / `File exists`. Keyed by the canonical
    /// clone path via [`git_lock_for`], so EVERY manager over the same clone shares
    /// this lock (not just clones of one instance) — different issues selecting the
    /// same secondary repo, or a launch racing a teardown, all serialize. The guarded
    /// sections are brief and purely synchronous (no `.await` is ever held across it).
    git_lock: Arc<Mutex<()>>,
}

impl WorktreeManager {
    /// Open a manager rooted at `repo_root`, deriving the branch prefix from
    /// `$USER`. The path is canonicalised, so it must already exist.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "standalone constructor; v1.6 production uses with_layout, tests use this"
        )
    )]
    pub fn new(repo_root: impl AsRef<Path>) -> Result<Self, WorktreeError> {
        Self::with_prefix(repo_root, default_branch_prefix())
    }

    /// Open a **standalone** manager with an explicit branch prefix (used by tests
    /// and by teammates who don't want `$USER` as their namespace). Worktrees live
    /// at `<repo>/.lindep/worktrees/<ISSUE>` with no per-repo leaf.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "standalone constructor; v1.6 production uses with_layout, tests use this"
        )
    )]
    pub fn with_prefix(
        repo_root: impl AsRef<Path>,
        branch_prefix: impl Into<String>,
    ) -> Result<Self, WorktreeError> {
        let repo_root = repo_root.as_ref();
        let canonical = repo_root.canonicalize().map_err(|e| WorktreeError::Io {
            path: repo_root.to_path_buf(),
            source: e,
        })?;
        let worktrees_root = canonical.join(".lindep").join("worktrees");
        let git_lock = git_lock_for(&canonical);
        Ok(WorktreeManager {
            repo_root: canonical,
            worktrees_root,
            repo_handle: None,
            branch_prefix: branch_prefix.into(),
            git_lock,
        })
    }

    /// Open a **managed** manager for one (project, repo) under v1.6's layout:
    /// `repo_root` is the L2 reference clone, `worktrees_root` the project's shared
    /// worktrees directory, and `handle` the per-repo leaf. The worktrees root is
    /// created and canonicalised here so `git worktree list`'s canonical paths
    /// strip cleanly against it.
    pub fn with_layout(
        repo_root: impl AsRef<Path>,
        branch_prefix: impl Into<String>,
        worktrees_root: impl AsRef<Path>,
        handle: impl Into<String>,
    ) -> Result<Self, WorktreeError> {
        let repo_root = repo_root.as_ref();
        let canonical = repo_root.canonicalize().map_err(|e| WorktreeError::Io {
            path: repo_root.to_path_buf(),
            source: e,
        })?;
        let worktrees_root = worktrees_root.as_ref();
        std::fs::create_dir_all(worktrees_root).map_err(|e| WorktreeError::Io {
            path: worktrees_root.to_path_buf(),
            source: e,
        })?;
        let worktrees_root = worktrees_root
            .canonicalize()
            .map_err(|e| WorktreeError::Io {
                path: worktrees_root.to_path_buf(),
                source: e,
            })?;
        let git_lock = git_lock_for(&canonical);
        Ok(WorktreeManager {
            repo_root: canonical,
            worktrees_root,
            repo_handle: Some(handle.into()),
            branch_prefix: branch_prefix.into(),
            git_lock,
        })
    }

    /// The canonical root every per-issue worktree hangs off.
    fn worktrees_root(&self) -> PathBuf {
        self.worktrees_root.clone()
    }

    /// The per-issue **workspace** directory — the shared parent of every repo's
    /// worktree for an issue (`worktrees/<ISSUE>`). For a standalone manager (no
    /// repo leaf) this *is* the worktree; for a managed one it's the agent's cwd
    /// when several repos sit side by side.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "the multi-repo agent cwd (ENG-536 up-front repo select); used by tests"
        )
    )]
    pub fn issue_workspace_dir(&self, issue: &str) -> PathBuf {
        self.worktrees_root.join(issue)
    }

    /// The deterministic worktree path for an issue — `worktrees/<ISSUE>` for a
    /// standalone manager, or `worktrees/<ISSUE>/<handle>` for a managed one.
    pub fn worktree_path(&self, issue: &str) -> PathBuf {
        let base = self.worktrees_root.join(issue);
        match &self.repo_handle {
            Some(handle) => base.join(handle),
            None => base,
        }
    }

    /// Extract the issue from a worktree path made relative to [`worktrees_root`].
    /// A standalone manager wants a single component (`<ISSUE>`); a managed one
    /// wants exactly `<ISSUE>/<handle>` matching *this* repo's handle, so a sibling
    /// repo's worktree under the shared per-issue parent is correctly excluded.
    ///
    /// [`worktrees_root`]: Self::worktrees_root
    fn issue_of_rel(&self, rel: &Path) -> Option<String> {
        let mut comps = rel
            .components()
            .map(|c| c.as_os_str().to_str().map(str::to_string));
        let issue = comps.next()??;
        match &self.repo_handle {
            None => comps.next().is_none().then_some(issue),
            Some(handle) => {
                let leaf = comps.next()??;
                (leaf == *handle && comps.next().is_none()).then_some(issue)
            }
        }
    }

    /// The deterministic branch name for an issue + title.
    pub fn branch_name(&self, issue: &str, title: &str) -> String {
        let slug = slugify(title);
        let issue = issue.to_lowercase();
        if slug.is_empty() {
            format!("{}/{}", self.branch_prefix, issue)
        } else {
            format!("{}/{}-{}", self.branch_prefix, issue, slug)
        }
    }

    /// Create the worktree + branch for `issue`, forking from `base` (a branch,
    /// tag or commit-ish such as `HEAD` or `main`).
    ///
    /// Idempotent: if a worktree for this issue is already registered it is
    /// returned unchanged. The branch is **pinned to the issue at first create**,
    /// not to the (mutable) title: on a later recreate we prefer any existing
    /// `<prefix>/<issue>…` branch over minting a fresh one from `base`, so a
    /// title edit between a `remove` and a `create` never orphans the prior
    /// branch's committed work. Only when no such branch exists do we cut a new
    /// one named for the current title, so an agent resumes on its own history.
    pub fn create(&self, issue: &str, title: &str, base: &str) -> Result<Worktree, WorktreeError> {
        // Validate at the trust boundary before the id reaches any `PathBuf::join`
        // or git ref. Linear identifiers always pass; a `/`, `..`, leading `-` or
        // control char (a malformed/spoofed/non-Linear source) fails fast here
        // rather than silently escaping the sandbox or going invisible to `list`.
        validate_issue_id(issue)?;

        // Serialize the whole add/prune sequence against concurrent launches:
        // `git worktree add`/`prune` mutate shared repo state and race otherwise.
        let _guard = self.lock_git();

        // Short-circuit on an already-registered worktree *before* prune, so a
        // valid relaunch never depends on an advisory cleanup step succeeding —
        // but only when its checkout is actually on disk. After a crash, `git
        // worktree list` keeps reporting a worktree whose directory was deleted
        // (it is merely `prunable`), so find() can return a record pointing at a
        // vanished dir; returning it would spawn the agent with a non-existent
        // cwd and wedge the issue on every relaunch. A vanished dir falls through
        // to the remove + prune + re-add recovery below (which reuses the kept
        // branch, so committed work is preserved).
        if let Some(existing) = self.find(issue)?
            && existing.path.is_dir()
        {
            return Ok(existing);
        }

        let path = self.worktree_path(issue);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| WorktreeError::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }

        // A crash (or a failed `worktree add`) can leave a non-empty target dir
        // behind; `prune` cannot reclaim that on its own (it only clears admin
        // entries for vanished dirs), and `worktree add` would then fail hard
        // with `'<path>' already exists`, permanently blocking relaunch. find()
        // already returned None, so no managed record points at this path — the
        // dir is gitignored scratch space we own. Remove it, *then* prune so the
        // following prune also clears any admin entry orphaned by that removal
        // (e.g. an anomalous detached checkout we filter out of list()).
        if path.exists() {
            std::fs::remove_dir_all(&path).map_err(|e| WorktreeError::Io {
                path: path.clone(),
                source: e,
            })?;
        }

        // Clean up admin metadata for any worktree whose directory vanished
        // (crash mid-removal, or the orphan dir just removed) so the
        // `worktree add` below isn't blocked by a ghost. Only needed on the add
        // path, hence run here rather than unconditionally up front.
        self.git(&["worktree", "prune"])?;

        let path_str = path.to_string_lossy();

        // Prefer an existing issue branch (any `<prefix>/<issue>…`) so a
        // title-changed recreate resumes the old branch instead of abandoning
        // its commits; fall back to a fresh title-named branch only if none.
        if let Some(branch) = self.existing_issue_branch(issue)? {
            // A branch can be checked out in at most one worktree. If something
            // already holds it (e.g. a case-variant issue id resolving to the
            // same branch but a different path, or a not-yet-pruned holder),
            // `worktree add` would fail with a confusing raw-git error — turn it
            // into a clear, attributable one instead.
            if let Some(holder) = self.branch_holder(&branch)? {
                return Err(WorktreeError::BranchInUse { branch, holder });
            }
            self.git(&["worktree", "add", path_str.as_ref(), &branch])?;
            Ok(Worktree {
                issue: issue.to_string(),
                path,
                branch,
            })
        } else if let Some(branch) = self.existing_remote_issue_branch(issue)? {
            // No LOCAL head, but the agent's pushed work survives as a remote-tracking
            // ref — the case after an L2 clone rebuild (fsck self-heal nuked + re-cloned
            // the clone, destroying local heads but re-fetching `origin/*`). Re-create
            // the local branch FROM `origin/<branch>` so the agent resumes its committed
            // work, never an empty branch off `base`. Without this, a rebuilt clone
            // silently strands every commit not yet merged to the default branch.
            if let Some(holder) = self.branch_holder(&branch)? {
                return Err(WorktreeError::BranchInUse { branch, holder });
            }
            let start = format!("origin/{branch}");
            self.git(&["worktree", "add", "-b", &branch, path_str.as_ref(), &start])?;
            Ok(Worktree {
                issue: issue.to_string(),
                path,
                branch,
            })
        } else {
            // Brand-new branch: resolve the configured base to a fresh start-point
            // (`origin/<base>` after a best-effort fetch, with fall-through to HEAD).
            // Only here — resumes/recoveries above keep their committed branch, so a
            // configured base never re-bases or orphans existing work.
            let branch = self.branch_name(issue, title);
            let start = self.resolve_base(base);
            self.git(&["worktree", "add", "-b", &branch, path_str.as_ref(), &start])?;
            Ok(Worktree {
                issue: issue.to_string(),
                path,
                branch,
            })
        }
    }

    /// Resolve a configured base to a start-point `git worktree add -b` will accept,
    /// preferring a *fresh* remote-tracking ref. Infallible: a mistyped or absent
    /// base falls through to `HEAD` so it can never block a launch. Consulted only on
    /// the fresh-branch path (an existing issue branch is reused regardless), so this
    /// is the one place that pays a network fetch — and only when a base is set.
    ///
    /// `HEAD` (the unset default) short-circuits with no network: byte-identical to
    /// the historical behaviour. Otherwise the clone's remote-tracking refs are
    /// freshened (`refresh_mirror` only advances the L1 mirror, not this L2 clone),
    /// then candidates are probed in order: `origin/<base>` → `<base>` →
    /// `origin/HEAD` (the repo's real default) → `HEAD`.
    fn resolve_base(&self, base: &str) -> String {
        if base == "HEAD" {
            return "HEAD".to_string();
        }
        // Best-effort: offline / a missing remote just leaves the chain to fall
        // through to a local ref or HEAD. Never fatal to a launch.
        let _ = self.git(&["fetch", "--prune", "origin"]);
        let mut candidates: Vec<String> = Vec::new();
        if !base.starts_with("origin/") {
            candidates.push(format!("origin/{base}"));
        }
        candidates.push(base.to_string());
        candidates.push("origin/HEAD".to_string());
        candidates.push("HEAD".to_string());
        for c in &candidates {
            if self
                .git(&["rev-parse", "--verify", "--quiet", &format!("{c}^{{commit}}")])
                .is_ok()
            {
                return c.clone();
            }
        }
        "HEAD".to_string()
    }

    /// Every worktree we manage (those under `.lindep/worktrees/<ISSUE>`),
    /// parsed live from `git worktree list --porcelain`. The main working tree
    /// and any unrelated worktrees are intentionally excluded.
    pub fn list(&self) -> Result<Vec<Worktree>, WorktreeError> {
        let out = self.git(&["worktree", "list", "--porcelain"])?;
        let root = self.worktrees_root();
        let mut result = Vec::new();

        for (path, branch) in parse_porcelain(&out) {
            // A managed worktree always has a branch checked out (we only ever
            // create it via `worktree add -b`/`add <branch>`). Porcelain emits
            // no `branch …` line for a detached HEAD, so a record with no branch
            // is an anomalous state we never produce — skip it rather than
            // surface an empty-string branch that would be persisted downstream
            // and silently corrupt the issue→branch mapping. A subsequent
            // `create` reclaims the orphan checkout deterministically.
            let Some(raw) = branch else { continue };
            // Strip exactly one `refs/heads/` (not `trim_start_matches`, which
            // would peel a repeated prefix), leaving any other ref form intact.
            let branch = raw.strip_prefix("refs/heads/").unwrap_or(raw).to_string();
            // Map the worktree path back to its issue. Standalone managers expect
            // an immediate `<root>/<ISSUE>` child; a managed manager expects
            // `<root>/<ISSUE>/<handle>` and keeps only its own repo's leaf — so
            // sibling repos sharing the per-issue parent don't bleed into this
            // repo's list. Neither the root itself nor anything nested deeper.
            if let Ok(rel) = path.strip_prefix(&root)
                && let Some(issue) = self.issue_of_rel(rel)
            {
                result.push(Worktree {
                    issue,
                    path: path.clone(),
                    branch,
                });
            }
        }
        Ok(result)
    }

    /// The managed worktree for `issue`, if one is currently registered.
    pub fn find(&self, issue: &str) -> Result<Option<Worktree>, WorktreeError> {
        Ok(self.list()?.into_iter().find(|w| w.issue == issue))
    }

    /// Remove the worktree for `issue` and prune git's metadata. The branch is
    /// kept (see module docs). Idempotent: removing an absent worktree still
    /// prunes and succeeds.
    // Part of the manager's contract (ENG-394) and covered by tests; the cockpit
    // keeps worktrees across runs for `--resume`, so no v1 keybinding calls it yet.
    #[allow(
        dead_code,
        reason = "tested cleanup API; no UI binding until a discard action lands"
    )]
    pub fn remove(&self, issue: &str) -> Result<(), WorktreeError> {
        // Serialize against concurrent create/remove for other issues (see the
        // `git_lock` field): all three share git's repo-level locks.
        let _guard = self.lock_git();
        if self.find(issue)?.is_some() {
            let path = self.worktree_path(issue);
            // --force: discard even a dirty checkout; the work is safe on the
            // branch, which we keep.
            self.git(&["worktree", "remove", "--force", &path.to_string_lossy()])?;
        }
        self.git(&["worktree", "prune"])?;
        Ok(())
    }

    // ── git plumbing ─────────────────────────────────────────────────────────

    /// Acquire the cross-clone git lock. The guard protects only `()`, so a
    /// poisoned lock (a panic in another thread while holding it) carries no
    /// invalid state — we recover the guard rather than propagating a panic,
    /// keeping the no-`unwrap`/no-panic invariant.
    fn lock_git(&self) -> std::sync::MutexGuard<'_, ()> {
        self.git_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Run `git -C <repo_root> <args>`, returning stdout on success.
    fn git(&self, args: &[&str]) -> Result<String, WorktreeError> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .args(args)
            .output()
            .map_err(WorktreeError::Spawn)?;
        if !output.status.success() {
            return Err(WorktreeError::Git {
                command: format!("git {}", args.join(" ")),
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// The local branch pinned to `issue`, if one already exists, regardless of
    /// the title slug it was first cut with. Matches `<prefix>/<issue>` exactly
    /// and any `<prefix>/<issue>-<slug>` (the issue is lowercased to mirror
    /// [`Self::branch_name`]). When several exist (a title changed across
    /// recreates) the lexicographically-first is chosen purely for determinism;
    /// if two `<prefix>/<issue>…` branches have diverged (e.g. one introduced
    /// externally) this resumes one of them by byte order, not by recency — the
    /// other is left intact but unreferenced here. lindep itself never creates
    /// the divergent case, and reusing one still beats minting a new empty branch
    /// from `base` and orphaning that work.
    fn existing_issue_branch(&self, issue: &str) -> Result<Option<String>, WorktreeError> {
        let issue = issue.to_lowercase();
        // List every local branch under `refs/heads/<prefix>/` (one short name
        // per line) and filter in Rust rather than leaning on git's glob
        // matching semantics, which differ subtly across `for-each-ref` vs
        // `branch --list`. A branch belongs to this issue iff its name is
        // exactly `<prefix>/<issue>` or starts with `<prefix>/<issue>-`.
        let prefix = format!("refs/heads/{}/", self.branch_prefix);
        let out = self.git(&["for-each-ref", "--format=%(refname:short)", &prefix])?;
        let exact = format!("{}/{}", self.branch_prefix, issue);
        let with_slug = format!("{exact}-");
        Ok(out
            .lines()
            .map(str::trim)
            .filter(|name| *name == exact.as_str() || name.starts_with(with_slug.as_str()))
            .min()
            .map(str::to_string))
    }

    /// The issue's branch as a **remote-tracking** ref (`refs/remotes/origin/<prefix>/…`),
    /// if one exists, returned as the LOCAL branch name (the `origin/` stripped). This
    /// is where the agent's pushed work lives after an L2 clone rebuild destroyed the
    /// local head — [`create`](Self::create) re-creates the local branch from it rather
    /// than cutting an empty one off `base`. Mirrors [`existing_issue_branch`](Self::existing_issue_branch)'s
    /// match (exact `<prefix>/<issue>` or any `<prefix>/<issue>-<slug>`).
    fn existing_remote_issue_branch(&self, issue: &str) -> Result<Option<String>, WorktreeError> {
        let issue = issue.to_lowercase();
        let prefix = format!("refs/remotes/origin/{}/", self.branch_prefix);
        let out = self.git(&["for-each-ref", "--format=%(refname:short)", &prefix])?;
        // `%(refname:short)` yields `origin/<prefix>/<issue>…`; strip the `origin/`.
        let exact = format!("{}/{}", self.branch_prefix, issue);
        let with_slug = format!("{exact}-");
        Ok(out
            .lines()
            .map(str::trim)
            .filter_map(|n| n.strip_prefix("origin/"))
            .filter(|name| *name == exact.as_str() || name.starts_with(with_slug.as_str()))
            .min()
            .map(str::to_string))
    }

    /// The path of the worktree that currently has `branch` checked out, if any.
    /// Scans the *raw* worktree list (the main tree and any unmanaged worktrees
    /// included), since a branch may be held anywhere — not only under our root.
    fn branch_holder(&self, branch: &str) -> Result<Option<PathBuf>, WorktreeError> {
        let out = self.git(&["worktree", "list", "--porcelain"])?;
        let wanted = format!("refs/heads/{branch}");
        Ok(parse_porcelain(&out)
            .find(|(_, b)| *b == Some(wanted.as_str()))
            .map(|(path, _)| path))
    }

    /// Whether a local branch exists. `show-ref --verify --quiet` exits 0 if the
    /// ref resolves and 1 if it doesn't, with no stderr in either case.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "branch existence is now checked via existing_issue_branch; \
                      kept as a precise single-branch probe used by tests"
        )
    )]
    fn branch_exists(&self, branch: &str) -> Result<bool, WorktreeError> {
        let refname = format!("refs/heads/{branch}");
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .args(["show-ref", "--verify", "--quiet", &refname])
            .output()
            .map_err(WorktreeError::Spawn)?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            other => Err(WorktreeError::Git {
                command: format!("git show-ref --verify --quiet {refname}"),
                code: other,
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            }),
        }
    }
}

/// Validate that an issue id is safe as a single path component and git ref
/// segment. Accepts ASCII alphanumerics plus `-`/`_`, non-empty, no leading `-`.
/// Defense-in-depth against a `/`, `..`, or control char ever reaching a path or
/// ref (which would escape `.lindep/worktrees` or make `list()` blind to the
/// worktree). `pub` so any id-producing boundary can reuse the same gate.
pub fn validate_issue_id(issue: &str) -> Result<(), WorktreeError> {
    let ok = !issue.is_empty()
        && !issue.starts_with('-')
        && issue
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(WorktreeError::InvalidIssueId(issue.to_string()))
    }
}

static ASK_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Mint a path/ref-safe synthetic issue id for an ad-hoc "ask" agent.
pub fn synthetic_ask_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let counter = ASK_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("ask-{}-{}", base36(nanos), base36(counter.into()))
}

pub fn is_synthetic_ask_id(issue: &str) -> bool {
    issue
        .strip_prefix("ask-")
        .is_some_and(|rest| rest.split('-').count() == 2 && validate_issue_id(issue).is_ok())
}

fn base36(mut value: u128) -> String {
    if value == 0 {
        return "0".to_string();
    }
    let mut out = Vec::new();
    while value > 0 {
        let digit = (value % 36) as u8;
        let byte = match digit {
            0..=9 => b'0' + digit,
            _ => b'a' + (digit - 10),
        };
        out.push(byte);
        value /= 36;
    }
    out.reverse();
    String::from_utf8(out).expect("base36 emits ascii")
}

/// Whether `prefix` is safe as the leading segment(s) of a branch ref
/// `<prefix>/<issue>` (the form [`WorktreeManager::branch_name`] builds). Replicates
/// the `git check-ref-format` rules that bear on a prefix: non-empty, no whitespace
/// or control char, none of the ref metacharacters `~^:?*[\`, no `..` or `@{`, no
/// leading/trailing `/`, no empty `//` segment, and no segment that starts with `.`
/// or ends in `.` / `.lock`. The issue id appended after it is already constrained
/// by [`validate_issue_id`], so a valid prefix guarantees a valid ref. `pub` so the
/// onboarding wizard and the registry loader gate the same value the same way —
/// otherwise a bad prefix (a space, `~`, a leading `/`) would be accepted and then
/// fail `git worktree add -b` for *every* issue in the project, bricking it silently
/// at first launch.
/// Whether `base` is a safe git start-point to interpolate into
/// `git worktree add -b <branch> <path> <base>` — a branch name, `origin/<b>`, a
/// tag, a SHA, or `HEAD`. Rejects the empties, a leading `-` (CLI-option
/// injection), `..`, `@{`, control/whitespace and the glob/range metacharacters.
/// Unlike [`is_valid_branch_prefix`] it permits the mid-ref `/` of `origin/develop`.
/// This is a pure *sanitisation* gate (no repo/network access) shared by the
/// registry loader and the wizard; existence is checked later by `resolve_base`.
pub fn is_valid_base(base: &str) -> bool {
    let b = base.trim();
    if b.is_empty()
        || b.starts_with('-')
        || b.starts_with('/')
        || b.ends_with('/')
        || b.contains("..")
        || b.contains("@{")
    {
        return false;
    }
    if b == "HEAD" {
        return true;
    }
    !b.chars().any(|c| {
        c.is_whitespace() || c.is_control() || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
    })
}

pub fn is_valid_branch_prefix(prefix: &str) -> bool {
    if prefix.is_empty()
        || prefix.starts_with('/')
        || prefix.ends_with('/')
        || prefix.contains("..")
        || prefix.contains("@{")
    {
        return false;
    }
    if prefix.chars().any(|c| {
        c.is_whitespace() || c.is_control() || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
    }) {
        return false;
    }
    // Each `/`-separated segment must be a valid ref component.
    prefix.split('/').all(|seg| {
        !seg.is_empty() && !seg.starts_with('.') && !seg.ends_with('.') && !seg.ends_with(".lock")
    })
}

/// Generate the `WORKSPACE.md` an agent reads at the root of a multi-repo issue's
/// workspace (its cwd, `worktrees/<ISSUE>`), telling it which repos are checked out
/// as sibling subdirectories and how to operate across them. Written before the
/// agent spawns by the up-front select (ENG-536) and regenerated after a mid-session
/// lazy-pull (ENG-542). Single-repo issues cd straight into the one worktree and get
/// no file. `repos` is `(handle, branch)` in materialisation order, primary first.
pub fn write_workspace_md(
    workspace_dir: &Path,
    issue: &str,
    repos: &[(String, String)],
) -> std::io::Result<()> {
    std::fs::create_dir_all(workspace_dir)?;
    let mut body = String::new();
    body.push_str(&format!("# {issue} — multi-repo workspace\n\n"));
    body.push_str(
        "This issue spans several repositories. Each is checked out as a sibling\n\
         subdirectory of this directory (your working directory):\n\n",
    );
    for (handle, branch) in repos {
        body.push_str(&format!("- `{handle}/` — branch `{branch}`\n"));
    }
    body.push_str(
        "\nWork across them as you would in any multi-checkout: operate on each repo\n\
         inside its own subdirectory (e.g. `cd <repo>` or `git -C <repo> …`). Each\n\
         repo is an independent git worktree on its own branch; commits in any of\n\
         them are pushed to that repo's own remote automatically.\n",
    );
    std::fs::write(workspace_dir.join("WORKSPACE.md"), body)
}

/// One shared worktree lock per **canonical L2 clone path**. `git worktree
/// add/prune/remove` mutate the clone's shared admin dir + ref store, so two
/// `WorktreeManager`s built over the SAME clone — e.g. concurrent launches of
/// different issues both selecting one secondary repo, or a launch racing a teardown
/// / lazy-pull on that repo — must serialize. A per-instance lock wouldn't (each
/// `with_layout` mints a fresh manager), so the lock is keyed by the clone path in a
/// process-global table, mirroring `notify::push_mutex`. The primary manager is
/// shared across launches and so already serialized; this extends the same guarantee
/// to every secondary repo.
fn git_lock_for(repo_root: &Path) -> Arc<Mutex<()>> {
    use std::collections::HashMap;
    use std::sync::{LazyLock, PoisonError};
    static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    LOCKS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(repo_root.to_path_buf())
        .or_default()
        .clone()
}

/// Default branch namespace: `$USER`, then `lindep`. Keeps Felix's `felix/…`
/// namespace off teammates' branches while matching the design's example for him.
pub(crate) fn default_branch_prefix() -> String {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| "lindep".to_string())
}

/// Turn an issue title into a short, branch-safe slug: lowercase ASCII
/// alphanumerics, single dashes between words, capped to keep the ref tidy.
fn slugify(title: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !out.is_empty() && !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.truncate(40);
    out.trim_matches('-').to_string()
}

/// Parse `git worktree list --porcelain` into `(path, branch)` records. Records
/// are blank-line separated; within a record, `worktree <path>` and
/// `branch <ref>` are the lines we care about. `branch` is the raw `refs/heads/…`
/// ref (or `None` for a detached HEAD, which porcelain omits) — each caller
/// interprets it. A record with no `worktree` line is skipped.
fn parse_porcelain(out: &str) -> impl Iterator<Item = (PathBuf, Option<&str>)> {
    out.split("\n\n").filter_map(|record| {
        let mut path = None;
        let mut branch = None;
        for line in record.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p.trim()));
            } else if let Some(b) = line.strip_prefix("branch ") {
                branch = Some(b.trim());
            }
        }
        path.map(|p| (p, branch))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A throwaway git repo with one commit, so `worktree add` has a valid base.
    /// Returns the manager and the temp dir (kept alive for the test's lifetime).
    struct TempRepo {
        dir: PathBuf,
        mgr: WorktreeManager,
    }

    impl TempRepo {
        fn new() -> Self {
            // A process-unique directory under the OS temp dir, no extra crates.
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("lindep-wt-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();

            let run = |args: &[&str]| {
                let ok = Command::new("git")
                    .arg("-C")
                    .arg(&dir)
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success();
                assert!(ok, "git {args:?} failed");
            };
            run(&["init", "-q", "-b", "main"]);
            run(&["config", "user.email", "t@example.com"]);
            run(&["config", "user.name", "Test"]);
            run(&["commit", "-q", "--allow-empty", "-m", "root"]);

            let mgr = WorktreeManager::with_prefix(&dir, "felix").unwrap();
            TempRepo { dir, mgr }
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn resolve_base_passes_head_through_with_no_network() {
        // The unset default short-circuits to HEAD verbatim — byte-identical to the
        // historical behaviour, and (crucially) without a fetch.
        let repo = TempRepo::new();
        assert_eq!(repo.mgr.resolve_base("HEAD"), "HEAD");
    }

    #[test]
    fn resolve_base_falls_through_to_a_local_branch_when_no_remote() {
        // No origin here, so `origin/<base>` misses and the chain falls through to the
        // verbatim local branch (`main`, which TempRepo created).
        let repo = TempRepo::new();
        assert_eq!(repo.mgr.resolve_base("main"), "main");
    }

    #[test]
    fn a_bogus_base_never_blocks_a_launch() {
        // The whole safety contract: a mistyped/absent base falls through to HEAD, so
        // create() still cuts the branch rather than erroring the launch.
        let repo = TempRepo::new();
        assert_eq!(repo.mgr.resolve_base("no-such-branch"), "HEAD");
        let wt = repo
            .mgr
            .create("ENG-1", "Feature", "no-such-branch")
            .expect("a bogus base still creates a worktree (fall-through to HEAD)");
        assert!(
            wt.branch.starts_with("felix/") && wt.branch.contains("eng-1"),
            "branch cut under the prefix: {}",
            wt.branch
        );
        assert!(wt.path.is_dir());
    }

    #[test]
    fn slugify_makes_a_tidy_branch_safe_slug() {
        assert_eq!(
            slugify("Spike: embed a live `claude` PTY!"),
            "spike-embed-a-live-claude-pty"
        );
        assert_eq!(slugify("   "), "");
        assert_eq!(slugify("ALL—CAPS…and—symbols"), "all-caps-and-symbols");
    }

    #[test]
    fn validate_issue_id_accepts_real_ids_and_rejects_path_tricks() {
        for ok in ["ENG-392", "ZAP-1", "zap_7", "A1"] {
            assert!(validate_issue_id(ok).is_ok(), "{ok:?} should be accepted");
        }
        // Empty, leading-dash, path separators, traversal, whitespace, dots — each
        // would escape `.lindep/worktrees` or form an invalid ref / a worktree
        // `list()` can't see.
        for bad in ["", "-x", "../escape", "ENG-1/sub", "a b", "a.b", "x\\y"] {
            assert!(
                matches!(
                    validate_issue_id(bad),
                    Err(WorktreeError::InvalidIssueId(_))
                ),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn synthetic_ask_ids_are_safe_and_distinct() {
        let a = synthetic_ask_id();
        let b = synthetic_ask_id();

        assert_ne!(a, b);
        assert!(is_synthetic_ask_id(&a));
        assert!(validate_issue_id(&a).is_ok());
    }

    #[test]
    fn valid_branch_prefix_accepts_real_namespaces_and_rejects_bad_refs() {
        for ok in ["felix", "lindep", "team/felix", "a-b_c", "user.name"] {
            assert!(is_valid_branch_prefix(ok), "{ok:?} should be accepted");
        }
        // A space, the ref metacharacters, `..`/`@{`, leading/trailing `/`, an empty
        // `//` segment, and a segment starting `.` / ending `.`/`.lock` would each make
        // `git worktree add -b <prefix>/<issue>` reject the ref for every issue.
        for bad in [
            "",
            "has space",
            "a~b",
            "a^b",
            "a:b",
            "a?b",
            "a*b",
            "a[b",
            "a\\b",
            "a..b",
            "a@{b",
            "/lead",
            "trail/",
            "a//b",
            ".hidden",
            "ends.",
            "x.lock",
        ] {
            assert!(!is_valid_branch_prefix(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn branch_name_is_deterministic_and_prefixed() {
        let repo = TempRepo::new();
        assert_eq!(
            repo.mgr.branch_name("ENG-392", "Spike: embed a PTY"),
            "felix/eng-392-spike-embed-a-pty"
        );
    }

    #[test]
    fn create_makes_the_worktree_and_branch() {
        let repo = TempRepo::new();
        let wt = repo.mgr.create("ENG-1", "First issue", "HEAD").unwrap();
        assert!(wt.path.is_dir(), "checkout directory exists");
        assert_eq!(wt.branch, "felix/eng-1-first-issue");
        assert!(repo.mgr.worktrees_root().join("ENG-1").is_dir());
    }

    #[test]
    fn create_is_idempotent() {
        let repo = TempRepo::new();
        let first = repo.mgr.create("ENG-1", "First issue", "HEAD").unwrap();
        let second = repo.mgr.create("ENG-1", "First issue", "HEAD").unwrap();
        assert_eq!(first, second, "second create returns the same worktree");
        assert_eq!(repo.mgr.list().unwrap().len(), 1, "no duplicate worktree");
    }

    #[test]
    fn list_returns_only_managed_worktrees() {
        let repo = TempRepo::new();
        repo.mgr.create("ENG-1", "One", "HEAD").unwrap();
        repo.mgr.create("ENG-2", "Two", "HEAD").unwrap();
        let mut issues: Vec<_> = repo
            .mgr
            .list()
            .unwrap()
            .into_iter()
            .map(|w| w.issue)
            .collect();
        issues.sort();
        // The main working tree is excluded; only our two appear.
        assert_eq!(issues, vec!["ENG-1", "ENG-2"]);
    }

    #[test]
    fn remove_drops_the_worktree_but_keeps_the_branch() {
        let repo = TempRepo::new();
        let wt = repo.mgr.create("ENG-1", "One", "HEAD").unwrap();
        repo.mgr.remove("ENG-1").unwrap();
        assert!(!wt.path.exists(), "checkout directory is gone");
        assert!(
            repo.mgr.find("ENG-1").unwrap().is_none(),
            "no longer registered"
        );
        assert!(
            repo.mgr.branch_exists(&wt.branch).unwrap(),
            "the branch is kept so work isn't lost"
        );
    }

    #[test]
    fn remove_is_idempotent_on_an_absent_worktree() {
        let repo = TempRepo::new();
        repo.mgr.remove("ENG-404").unwrap(); // never created — still fine
    }

    #[test]
    fn create_reuses_an_existing_branch_after_the_worktree_is_pruned() {
        let repo = TempRepo::new();
        let first = repo.mgr.create("ENG-1", "One", "HEAD").unwrap();
        repo.mgr.remove("ENG-1").unwrap(); // worktree gone, branch kept
        let again = repo.mgr.create("ENG-1", "One", "HEAD").unwrap();
        assert_eq!(first.branch, again.branch, "the kept branch is reused");
        assert!(again.path.is_dir());
    }

    #[test]
    fn reconcile_detects_a_worktree_whose_directory_vanished() {
        // Simulate a crash: the checkout directory is deleted out from under git.
        // `list()` (via prune in create, or a direct prune) must stop reporting it.
        let repo = TempRepo::new();
        let wt = repo.mgr.create("ENG-1", "One", "HEAD").unwrap();
        std::fs::remove_dir_all(&wt.path).unwrap();
        // create() prunes on the add path, so a fresh create for another issue
        // (which reaches that path) cleans the unrelated ghost in passing.
        repo.mgr.create("ENG-2", "Two", "HEAD").unwrap();
        let issues: Vec<_> = repo
            .mgr
            .list()
            .unwrap()
            .into_iter()
            .map(|w| w.issue)
            .collect();
        assert_eq!(
            issues,
            vec!["ENG-2"],
            "the vanished ENG-1 worktree was pruned"
        );
    }

    #[test]
    fn recreating_the_same_issue_after_its_worktree_dir_vanished_recovers() {
        // A crash deletes the checkout dir, but `git worktree list` keeps
        // reporting it (merely `prunable`) until a prune runs — so find() returns
        // a record whose path no longer exists. Re-creating the SAME issue must
        // not short-circuit onto that ghost: returning a non-existent path would
        // spawn the agent with a missing cwd and wedge the issue on every
        // relaunch. create() must fall through to prune + re-add, reusing the kept
        // branch so committed work survives. (Distinct from the reconcile test,
        // which recovers only as a side effect of creating a *different* issue.)
        let repo = TempRepo::new();
        let first = repo.mgr.create("ENG-1", "One", "HEAD").unwrap();
        std::fs::remove_dir_all(&first.path).unwrap();
        assert!(
            repo.mgr
                .find("ENG-1")
                .unwrap()
                .is_some_and(|w| !w.path.is_dir()),
            "git still lists the vanished worktree before recovery — the stale short-circuit input"
        );

        let again = repo.mgr.create("ENG-1", "One", "HEAD").unwrap();
        assert!(
            again.path.is_dir(),
            "the checkout was recreated, not a stale path returned"
        );
        assert_eq!(
            first.branch, again.branch,
            "the kept branch is reused, preserving committed work"
        );
    }

    #[test]
    fn create_reclaims_an_orphan_dir_with_no_admin_entry() {
        // Crash mid-create: a non-empty checkout dir is left at the target path
        // with no registered worktree. `prune` can't reclaim that (no admin
        // entry references a *present* dir), so a naive `worktree add` would
        // fail with `already exists` and block relaunch forever. create() must
        // reclaim the orphan and succeed.
        let repo = TempRepo::new();
        let orphan = repo.mgr.worktree_path("ENG-7");
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::write(orphan.join("leftover.txt"), b"half-made").unwrap();
        assert!(
            repo.mgr.find("ENG-7").unwrap().is_none(),
            "nothing is registered for the orphan dir"
        );

        let wt = repo.mgr.create("ENG-7", "Seven", "HEAD").unwrap();
        assert!(wt.path.is_dir(), "a fresh checkout replaced the orphan");
        assert!(
            !wt.path.join("leftover.txt").exists(),
            "the orphan's leftover content was cleared"
        );
        assert_eq!(
            repo.mgr.find("ENG-7").unwrap().map(|w| w.branch),
            Some("felix/eng-7-seven".to_string()),
            "the worktree is now properly registered"
        );
    }

    #[test]
    fn recreate_with_a_changed_title_reuses_the_issues_branch_not_a_fresh_one() {
        // The branch is pinned to the issue, not the mutable title. A
        // remove→recreate with a new title must resume the prior branch (where
        // the agent's commits live), not mint an empty `…-new-title` branch and
        // orphan that work.
        let repo = TempRepo::new();
        let first = repo.mgr.create("ENG-1", "Old title", "HEAD").unwrap();
        assert_eq!(first.branch, "felix/eng-1-old-title");
        repo.mgr.remove("ENG-1").unwrap(); // worktree gone, branch kept

        let again = repo.mgr.create("ENG-1", "Brand new title", "HEAD").unwrap();
        assert_eq!(
            again.branch, "felix/eng-1-old-title",
            "the original issue branch is reused despite the title change"
        );
        assert!(
            !repo
                .mgr
                .branch_exists("felix/eng-1-brand-new-title")
                .unwrap(),
            "no fresh empty branch was minted from the new title"
        );
    }

    #[test]
    fn list_skips_a_detached_head_worktree_under_the_root() {
        // The manager never creates a detached checkout; porcelain emits no
        // `branch …` line for one. Such a record must be skipped rather than
        // surface an empty-string branch that would be persisted downstream.
        let repo = TempRepo::new();
        repo.mgr.create("ENG-1", "One", "HEAD").unwrap();
        let detached = repo.mgr.worktree_path("ENG-D");
        let detached_str = detached.to_string_lossy().into_owned();
        let ok = Command::new("git")
            .arg("-C")
            .arg(&repo.dir)
            .args(["worktree", "add", "--detach", detached_str.as_str(), "HEAD"])
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "set up a detached worktree under the managed root");

        let issues: Vec<_> = repo
            .mgr
            .list()
            .unwrap()
            .into_iter()
            .map(|w| w.issue)
            .collect();
        assert_eq!(
            issues,
            vec!["ENG-1"],
            "the detached ENG-D record is filtered out; only the branched one remains"
        );
        assert!(
            repo.mgr
                .list()
                .unwrap()
                .iter()
                .all(|w| !w.branch.is_empty()),
            "no managed worktree is ever reported with an empty branch"
        );
    }

    /// Build a managed manager (v1.6 layout) over a throwaway repo, sharing an
    /// explicit `worktrees_root` with any sibling repos of the same project.
    fn managed(worktrees_root: &Path, handle: &str) -> (PathBuf, WorktreeManager) {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("lindep-mwt-{}-{n}", std::process::id()));
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
        let mgr = WorktreeManager::with_layout(&dir, "felix", worktrees_root, handle).unwrap();
        (dir, mgr)
    }

    #[test]
    fn a_managed_manager_nests_the_worktree_under_issue_then_handle() {
        let wt_root = std::env::temp_dir().join(format!("lindep-mwtr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&wt_root);
        let (_repo, mgr) = managed(&wt_root, "api");
        let wt = mgr.create("ENG-1", "First", "HEAD").unwrap();
        // The worktree nests as worktrees/<ISSUE>/<handle>…
        assert_eq!(wt.path, mgr.worktrees_root().join("ENG-1").join("api"));
        assert!(wt.path.is_dir());
        // …and the per-issue workspace (the agent's cwd) is the shared parent.
        assert_eq!(
            mgr.issue_workspace_dir("ENG-1"),
            mgr.worktrees_root().join("ENG-1")
        );
        // Branch naming is unchanged by the re-rooting.
        assert_eq!(wt.branch, "felix/eng-1-first");
        let _ = std::fs::remove_dir_all(&wt_root);
    }

    #[test]
    fn two_repos_of_one_issue_share_the_parent_but_list_only_their_own_leaf() {
        // Two repos of the same project share one worktrees_root; each issue's
        // repos nest as sibling subdirs under worktrees/<ISSUE>/. Each manager
        // must list only ITS handle's worktree, mapping the path back to the issue
        // by stripping the handle leaf.
        let wt_root = std::env::temp_dir().join(format!("lindep-mwtr2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&wt_root);
        let (_a, api) = managed(&wt_root, "api");
        let (_b, web) = managed(&wt_root, "web");
        let a = api.create("ENG-1", "Feature", "HEAD").unwrap();
        let b = web.create("ENG-1", "Feature", "HEAD").unwrap();

        assert_eq!(
            a.path.parent(),
            b.path.parent(),
            "siblings share the parent"
        );
        assert_eq!(a.path.file_name().unwrap(), "api");
        assert_eq!(b.path.file_name().unwrap(), "web");

        let api_issues: Vec<_> = api.list().unwrap().into_iter().map(|w| w.issue).collect();
        let web_issues: Vec<_> = web.list().unwrap().into_iter().map(|w| w.issue).collect();
        assert_eq!(
            api_issues,
            vec!["ENG-1"],
            "api lists its issue, mapped past the leaf"
        );
        assert_eq!(
            web_issues,
            vec!["ENG-1"],
            "web lists its issue independently"
        );
        let _ = std::fs::remove_dir_all(&wt_root);
    }

    #[test]
    fn write_workspace_md_lists_each_repo_and_its_branch() {
        let dir = std::env::temp_dir().join(format!("lindep-wsmd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_workspace_md(
            &dir,
            "ENG-7",
            &[
                ("api".to_string(), "felix/eng-7-x".to_string()),
                ("web".to_string(), "felix/eng-7-x".to_string()),
            ],
        )
        .unwrap();
        let body = std::fs::read_to_string(dir.join("WORKSPACE.md")).unwrap();
        assert!(body.contains("ENG-7"), "names the issue");
        assert!(
            body.contains("`api/`") && body.contains("`web/`"),
            "lists each repo subdir"
        );
        assert!(body.contains("felix/eng-7-x"), "shows each repo's branch");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_resumes_a_branch_that_lives_only_as_a_remote_tracking_ref() {
        // After an L2 clone rebuild (fsck self-heal nuke+re-clone), the agent's pushed
        // work branch has no LOCAL head — it survives only as `origin/<branch>`. create()
        // must re-create the local branch FROM that remote-tracking ref so the agent
        // resumes its committed work, never an empty branch off `base`.
        let base = std::env::temp_dir().join(format!("lindep-remoteresume-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let remote = base.join("remote");
        let clone = base.join("clone");
        let run = |dir: &Path, args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(dir)
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success(),
                "git {args:?} in {}",
                dir.display()
            );
        };
        std::fs::create_dir_all(&remote).unwrap();
        run(&remote, &["init", "-q", "-b", "main"]);
        run(&remote, &["config", "user.email", "t@example.com"]);
        run(&remote, &["config", "user.name", "Test"]);
        std::fs::write(remote.join("base.txt"), b"base").unwrap();
        run(&remote, &["add", "."]);
        run(&remote, &["commit", "-q", "-m", "base"]);
        // The agent's pushed work branch, carrying a unique file.
        run(&remote, &["checkout", "-q", "-b", "felix/eng-7-x"]);
        std::fs::write(remote.join("work.txt"), b"agent work").unwrap();
        run(&remote, &["add", "."]);
        run(&remote, &["commit", "-q", "-m", "work"]);
        run(&remote, &["checkout", "-q", "main"]);
        // Clone it: `origin/felix/eng-7-x` is a remote-tracking ref, with no local head
        // (exactly the post-rebuild state of an L2 clone).
        assert!(
            Command::new("git")
                .args(["clone", "-q"])
                .arg(&remote)
                .arg(&clone)
                .output()
                .unwrap()
                .status
                .success(),
            "clone failed"
        );

        let wt_root = base.join("worktrees");
        let mgr = WorktreeManager::with_layout(&clone, "felix", wt_root, "api").unwrap();
        let wt = mgr.create("ENG-7", "x", "HEAD").unwrap();
        assert_eq!(
            wt.branch, "felix/eng-7-x",
            "resumed the pushed branch, not a fresh one"
        );
        assert!(
            wt.path.join("work.txt").is_file(),
            "the agent's committed work is in the resumed checkout"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn reusing_a_branch_already_checked_out_yields_a_clear_error() {
        // Case-variant issue ids map to different paths but the same (lowercased)
        // branch. With the first still live, reusing its branch for the second
        // must fail with a clear BranchInUse error, not raw git stderr.
        let repo = TempRepo::new();
        let first = repo.mgr.create("ENG-1", "One", "HEAD").unwrap();

        let err = repo
            .mgr
            .create("eng-1", "One", "HEAD")
            .expect_err("the live branch can't be checked out a second time");
        match err {
            WorktreeError::BranchInUse { branch, holder } => {
                assert_eq!(branch, first.branch);
                assert_eq!(
                    holder.canonicalize().unwrap(),
                    first.path.canonicalize().unwrap(),
                    "the error names the worktree that holds the branch"
                );
            }
            other => panic!("expected BranchInUse, got {other:?}"),
        }
    }
}
