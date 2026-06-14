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
    /// returned unchanged. If the branch already exists (e.g. a prior worktree
    /// was pruned) it is reused rather than re-created, so an agent resumes on
    /// its own history.
    pub fn create(&self, issue: &str, title: &str, base: &str) -> Result<Worktree, WorktreeError> {
        // Clean up admin metadata for any worktree whose directory vanished
        // (e.g. a crash mid-removal) so a re-create isn't blocked by a ghost.
        self.git(&["worktree", "prune"])?;

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
        let path_str = path.to_string_lossy();
        let branch = self.branch_name(issue, title);

        if self.branch_exists(&branch)? {
            self.git(&["worktree", "add", path_str.as_ref(), &branch])?;
        } else {
            self.git(&["worktree", "add", "-b", &branch, path_str.as_ref(), base])?;
        }

        Ok(Worktree {
            issue: issue.to_string(),
            path,
            branch,
        })
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
                    branch = Some(b.trim().trim_start_matches("refs/heads/").to_string());
                }
            }
            let Some(path) = path else { continue };
            // Keep only an immediate `<root>/<ISSUE>` child — not the root
            // itself, nor anything nested deeper.
            if let Ok(rel) = path.strip_prefix(&root)
                && rel.components().count() == 1
                && let Some(issue) = rel.components().next().and_then(|c| c.as_os_str().to_str())
            {
                result.push(Worktree {
                    issue: issue.to_string(),
                    path: path.clone(),
                    branch: branch.unwrap_or_default(),
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

    /// Whether a local branch exists. `show-ref --verify --quiet` exits 0 if the
    /// ref resolves and 1 if it doesn't, with no stderr in either case.
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

/// `$USER`, then `lindep`. Keeps Felix's `felix/…` namespace off teammates'
/// branches while matching the design's example for him.
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
        // create() prunes first, so a fresh create for another issue cleans the ghost.
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
}
