//! Git worktree manager — one worktree + branch per Linear issue.
//!
//! Each agent works in isolation in its own checkout so concurrent agents never
//! step on each other's files. We shell out to `git` (libgit2's worktree API is
//! thin and its edge cases are worse than the CLI's) and reconcile against
//! `git worktree list --porcelain`, so a crash that leaves a half-made worktree
//! behind is detectable and recoverable on the next run.
//!
//! Layout is deterministic:
//! * worktree path — `<repo>/.lindep/worktrees/<ISSUE>` (`.lindep/` is gitignored)
//! * branch — `<prefix>/<issue>-<slug>`, e.g. `felix/eng-392-spike-embed-a-pty`
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
use std::sync::{Arc, Mutex};

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
    /// which git canonicalises, can be matched against ours).
    repo_root: PathBuf,
    /// Branch namespace, e.g. `felix`. Defaults to `$USER`, then `lindep`.
    branch_prefix: String,
    /// Serializes repo-mutating `git worktree add`/`prune`/`remove` across all
    /// clones of this manager. `git worktree` mutates shared repo state (the
    /// `worktrees/` admin dir, the ref store, the common index lock), so two
    /// concurrent `create`/`remove` calls — one per agent, each in its own
    /// `spawn_blocking` — can otherwise collide on git's locks and fail with
    /// `could not lock` / `File exists`. The guarded sections are brief and
    /// purely synchronous (no `.await` is ever held across this lock), so a
    /// single global git lock costs little and removes the race.
    git_lock: Arc<Mutex<()>>,
}

impl WorktreeManager {
    /// Open a manager rooted at `repo_root`, deriving the branch prefix from
    /// `$USER`. The path is canonicalised, so it must already exist.
    pub fn new(repo_root: impl AsRef<Path>) -> Result<Self, WorktreeError> {
        Self::with_prefix(repo_root, default_branch_prefix())
    }

    /// Open a manager with an explicit branch prefix (used by tests and by
    /// teammates who don't want `$USER` as their namespace).
    pub fn with_prefix(
        repo_root: impl AsRef<Path>,
        branch_prefix: impl Into<String>,
    ) -> Result<Self, WorktreeError> {
        let repo_root = repo_root.as_ref();
        let canonical = repo_root.canonicalize().map_err(|e| WorktreeError::Io {
            path: repo_root.to_path_buf(),
            source: e,
        })?;
        Ok(WorktreeManager {
            repo_root: canonical,
            branch_prefix: branch_prefix.into(),
            git_lock: Arc::new(Mutex::new(())),
        })
    }

    /// `<repo>/.lindep/worktrees`.
    fn worktrees_root(&self) -> PathBuf {
        self.repo_root.join(".lindep").join("worktrees")
    }

    /// The deterministic worktree path for an issue.
    pub fn worktree_path(&self, issue: &str) -> PathBuf {
        self.worktrees_root().join(issue)
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
        // valid relaunch never depends on an advisory cleanup step succeeding.
        if let Some(existing) = self.find(issue)? {
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
        } else {
            let branch = self.branch_name(issue, title);
            self.git(&["worktree", "add", "-b", &branch, path_str.as_ref(), base])?;
            Ok(Worktree {
                issue: issue.to_string(),
                path,
                branch,
            })
        }
    }

    /// Every worktree we manage (those under `.lindep/worktrees/<ISSUE>`),
    /// parsed live from `git worktree list --porcelain`. The main working tree
    /// and any unrelated worktrees are intentionally excluded.
    pub fn list(&self) -> Result<Vec<Worktree>, WorktreeError> {
        let out = self.git(&["worktree", "list", "--porcelain"])?;
        let root = self.worktrees_root();
        let mut result = Vec::new();

        // Records are blank-line separated; within a record, `worktree <path>`
        // and `branch refs/heads/<name>` are the lines we care about.
        for record in out.split("\n\n") {
            let mut path: Option<PathBuf> = None;
            let mut branch: Option<String> = None;
            for line in record.lines() {
                if let Some(p) = line.strip_prefix("worktree ") {
                    path = Some(PathBuf::from(p.trim()));
                } else if let Some(b) = line.strip_prefix("branch ") {
                    // Strip exactly one `refs/heads/` (not `trim_start_matches`,
                    // which would peel a repeated prefix), leaving any other ref
                    // form intact.
                    let raw = b.trim();
                    branch = Some(raw.strip_prefix("refs/heads/").unwrap_or(raw).to_string());
                }
            }
            let Some(path) = path else { continue };
            // A managed worktree always has a branch checked out (we only ever
            // create it via `worktree add -b`/`add <branch>`). Porcelain emits
            // no `branch …` line for a detached HEAD, so a record with no branch
            // is an anomalous state we never produce — skip it rather than
            // surface an empty-string branch that would be persisted downstream
            // and silently corrupt the issue→branch mapping. A subsequent
            // `create` reclaims the orphan checkout deterministically.
            let Some(branch) = branch else { continue };
            // Keep only an immediate `<root>/<ISSUE>` child — not the root
            // itself, nor anything nested deeper.
            if let Ok(rel) = path.strip_prefix(&root)
                && rel.components().count() == 1
                && let Some(issue) = rel.components().next().and_then(|c| c.as_os_str().to_str())
            {
                result.push(Worktree {
                    issue: issue.to_string(),
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

    /// The path of the worktree that currently has `branch` checked out, if any.
    /// Scans the *raw* worktree list (the main tree and any unmanaged worktrees
    /// included), since a branch may be held anywhere — not only under our root.
    fn branch_holder(&self, branch: &str) -> Result<Option<PathBuf>, WorktreeError> {
        let out = self.git(&["worktree", "list", "--porcelain"])?;
        let wanted = format!("refs/heads/{branch}");
        for record in out.split("\n\n") {
            let mut path: Option<PathBuf> = None;
            let mut holds = false;
            for line in record.lines() {
                if let Some(p) = line.strip_prefix("worktree ") {
                    path = Some(PathBuf::from(p.trim()));
                } else if let Some(b) = line.strip_prefix("branch ") {
                    holds = b.trim() == wanted.as_str();
                }
            }
            if holds && let Some(path) = path {
                return Ok(Some(path));
            }
        }
        Ok(None)
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

/// Default branch namespace: `$USER`, then `lindep`. Keeps Felix's `felix/…`
/// namespace off teammates' branches while matching the design's example for him.
fn default_branch_prefix() -> String {
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
