//! Clone substrate — the bottom two of v1.6's three git layers.
//!
//! lindep provisions a project's repos cheaply and in isolation by interposing a
//! shared **bare mirror** between the true remote and each per-(project, repo)
//! **reference clone**:
//!
//! | Layer | What | Where |
//! | -- | -- | -- |
//! | **L1 mirror** | `git clone --mirror <remote>` — one bare object DB per repo, shared by every project | [`Layout::mirror_path`] |
//! | **L2 reference clone** | a working clone that **borrows** the mirror's objects (`objects/info/alternates`) and pushes to the true remote | [`Layout::repo_clone_path`] |
//!
//! The per-issue **worktree** (L3) hangs off the L2 clone via the existing
//! [`crate::worktree::WorktreeManager`], re-rooted there.
//!
//! **Why a mirror at all.** Two projects that both use `lindep` share one object
//! DB instead of cloning it twice; a clone is then near-free (it copies refs and a
//! working tree, not the object history). The cost is the **alternates fragility**
//! the design calls out: an L2 clone depends on its mirror's objects, so the
//! mirror must never be gc-pruned or deleted while a clone references it. We never
//! `--dissociate` (that would copy the objects back, defeating the point), we
//! reference-count mirrors before deletion (elsewhere), and we `fsck` an L2 clone
//! on open, self-healing a broken alternate link or — last resort — rebuilding the
//! clone from the mirror.
//!
//! Every provisioning step is **idempotent** and **crash-safe**: it clones into a
//! sibling `*.partial.<pid>.<seq>` directory, validates it, then atomically
//! renames it into place, so a crash mid-clone leaves debris that the next run
//! sweeps rather than a half-built repo git would choke on. A per-handle
//! filesystem `flock` serialises mirror creation across concurrent launches (and
//! across separate lindep processes sharing `~/.lindep`).
//!
//! These functions are **synchronous and blocking** (they shell out to `git`); a
//! caller on the tokio runtime invokes them via `spawn_blocking`, exactly like the
//! worktree manager.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::registry::{Layout, RepoEntry};

/// Anything that can go wrong provisioning a mirror or reference clone.
#[derive(Debug, thiserror::Error)]
pub enum MirrorError {
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

    /// A filesystem operation we perform ourselves failed.
    #[error("filesystem error at {}: {source}", .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The repo entry has no source to mirror from (neither `remote` nor `local`).
    /// The registry loader rejects these, so this is a belt-and-suspenders guard.
    #[error("repo `{0}` has no remote or local source to mirror")]
    NoSource(String),

    /// An existing mirror was cloned from a different remote than the registry now
    /// names — refusing to silently repoint (and corrupt) every project that
    /// references it. The user must reconcile the registry or remove the mirror.
    #[error("mirror for `{handle}` points at {existing} but the registry says {wanted}")]
    RemoteMismatch {
        handle: String,
        existing: String,
        wanted: String,
    },
}

/// Ensure the L1 bare mirror for `repo` exists at [`Layout::mirror_path`] and is
/// cloned from the registry's source, returning its path. Idempotent: an existing,
/// matching mirror is returned untouched; a mismatched remote is a hard error
/// rather than a silent repoint. Serialised per handle by a filesystem `flock` so
/// two concurrent launches don't both clone the same mirror.
pub fn ensure_mirror(layout: &Layout, repo: &RepoEntry) -> Result<PathBuf, MirrorError> {
    let mirror = layout.mirror_path(&repo.handle);
    let source = repo
        .mirror_source()
        .ok_or_else(|| MirrorError::NoSource(repo.handle.clone()))?;

    // Fast path: a valid mirror already exists. Checked before taking the lock so
    // the common case never contends.
    if is_git_dir(&mirror) {
        verify_mirror_source(&mirror, repo, &source)?;
        return Ok(mirror);
    }

    let mirrors_dir = layout.mirrors_dir();
    mkdir_p(&mirrors_dir)?;
    let _lock = FileLock::acquire(&lock_path(&mirrors_dir, &repo.handle))?;

    // Re-check under the lock: another process may have created it while we waited.
    if is_git_dir(&mirror) {
        verify_mirror_source(&mirror, repo, &source)?;
        return Ok(mirror);
    }

    sweep_partials(&mirrors_dir, &format!("{}.git", repo.handle));
    let tmp = partial_path(&mirror);
    // `--mirror` gives a bare repo whose refs map 1:1 to the source — the shared
    // object DB every reference clone borrows from.
    git(
        &["clone", "--mirror", source.as_str(), &tmp.to_string_lossy()],
        None,
    )?;
    if !is_git_dir(&tmp) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(MirrorError::Git {
            command: format!("git clone --mirror {source}"),
            code: None,
            stderr: "clone produced no git directory".to_string(),
        });
    }
    rename(&tmp, &mirror)?;
    Ok(mirror)
}

/// Ensure the L2 reference clone for `(project, repo)` exists at
/// [`Layout::repo_clone_path`] and returns its path. Materialises the mirror
/// first, then clones from it borrowing its objects (`--shared`, the local-source
/// equivalent of `--reference`), and points `origin` at the true remote so an
/// agent's `git push` reaches it. Idempotent + self-healing: an existing clone is
/// `fsck`-validated (its alternate link repaired, or the clone rebuilt) before
/// being returned.
pub fn ensure_clone(
    layout: &Layout,
    project_handle: &str,
    repo: &RepoEntry,
) -> Result<PathBuf, MirrorError> {
    let mirror = ensure_mirror(layout, repo)?;
    let dst = layout.repo_clone_path(project_handle, &repo.handle);

    if dst.exists() {
        match validate_clone(&dst, &mirror) {
            Ok(()) => return Ok(dst),
            // Unrecoverable (objects gone, alternate unrepairable): nuke + rebuild
            // from the mirror below.
            Err(_) => {
                let _ = std::fs::remove_dir_all(&dst);
            }
        }
    }

    let repos_dir = layout.repos_dir(project_handle);
    mkdir_p(&repos_dir)?;
    sweep_partials(&repos_dir, &repo.handle);
    let tmp = partial_path(&dst);

    // `--shared` borrows the mirror's object DB via `objects/info/alternates`
    // (~0 objects copied), the offline-fast local equivalent of `--reference`.
    // We deliberately never `--dissociate`: the borrow is the whole point.
    git(
        &[
            "clone",
            "--shared",
            &mirror.to_string_lossy(),
            &tmp.to_string_lossy(),
        ],
        None,
    )?;
    if !is_git_dir(&tmp.join(".git")) && !is_git_dir(&tmp) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(MirrorError::Git {
            command: "git clone --shared".to_string(),
            code: None,
            stderr: "reference clone produced no git directory".to_string(),
        });
    }
    // Point origin at the true remote so `git push origin HEAD` reaches it; a
    // local-only repo (no remote) keeps origin = the synthesised mirror, so its
    // push lands there. `--mirror`-cloned origin already equals the mirror path.
    if let Some(remote) = repo.remote.as_deref() {
        git(&["remote", "set-url", "origin", remote], Some(&tmp))?;
    }
    write_validated_marker(&tmp);
    rename(&tmp, &dst)?;
    Ok(dst)
}

/// Whether a repo entry can open pull requests / push to a true remote — false for
/// a local-only repo whose `origin` is the synthesised mirror.
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "PR-status gating for local-only repos (ENG-541 teardown); used by tests"
    )
)]
pub fn can_push_to_remote(repo: &RepoEntry) -> bool {
    !repo.is_local_only()
}

/// Push the worktree's current branch to its `origin` (`git push -u origin HEAD`),
/// for v1.6 auto-push. **Never** force-pushes or rebases — a rejected push is
/// surfaced as a passive footer by the caller, never papered over. `origin` is the
/// repo's true remote (a local-only repo's `origin` is its synthesised mirror).
/// `GIT_TERMINAL_PROMPT=0` (in [`git`]) makes a credential prompt fail fast rather
/// than hang the push.
pub fn push_head(worktree: &Path) -> Result<(), MirrorError> {
    git(&["push", "-u", "origin", "HEAD"], Some(worktree))?;
    Ok(())
}

// ── validation & self-heal ──────────────────────────────────────────────────

/// Validate an existing L2 clone against its mirror, self-healing where possible:
/// repair a missing/wrong `objects/info/alternates` link, and — gated by a cached
/// marker so we don't fsck every open — run a connectivity-only `fsck`, rebuilding
/// (via the caller's nuke + re-clone) only when that fails.
fn validate_clone(dst: &Path, mirror: &Path) -> Result<(), MirrorError> {
    if !is_work_clone(dst) {
        return Err(MirrorError::Io {
            path: dst.to_path_buf(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "not a git work clone"),
        });
    }
    repair_alternate(dst, mirror)?;
    // A previously-validated clone is trusted (the marker is dropped whenever the
    // clone is rebuilt), so the connectivity fsck — seconds at scale — runs only on
    // the first open or after a self-heal.
    if validated_marker(dst).exists() {
        return Ok(());
    }
    let objects = mirror.join("objects");
    if !objects.exists() {
        return Err(MirrorError::Io {
            path: objects,
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "mirror objects missing"),
        });
    }
    git(&["fsck", "--connectivity-only"], Some(dst))?;
    write_validated_marker(dst);
    Ok(())
}

/// Ensure the clone's `objects/info/alternates` points at the mirror's object DB,
/// rewriting it if absent or stale (the alternate link is what makes the borrow
/// work; a moved/rebuilt mirror leaves it dangling).
fn repair_alternate(dst: &Path, mirror: &Path) -> Result<(), MirrorError> {
    let alternates = dst
        .join(".git")
        .join("objects")
        .join("info")
        .join("alternates");
    let want = mirror.join("objects");
    let want_line = want.to_string_lossy();
    let current = std::fs::read_to_string(&alternates).ok();
    let ok = current
        .as_deref()
        .is_some_and(|c| c.lines().any(|l| l.trim() == want_line));
    if !ok {
        if let Some(parent) = alternates.parent() {
            mkdir_p(parent)?;
        }
        std::fs::write(&alternates, format!("{want_line}\n")).map_err(|source| {
            MirrorError::Io {
                path: alternates,
                source,
            }
        })?;
    }
    Ok(())
}

// ── filesystem helpers ──────────────────────────────────────────────────────

/// Whether `path` is a git directory (bare repo or `.git` dir): it has an
/// `objects` directory and a `HEAD`. Cheap and filesystem-only.
fn is_git_dir(path: &Path) -> bool {
    path.join("objects").is_dir() && path.join("HEAD").is_file()
}

/// Whether `path` is a git *work* clone (a `.git` subdirectory with objects).
fn is_work_clone(path: &Path) -> bool {
    is_git_dir(&path.join(".git"))
}

/// The `*.partial.<pid>.<seq>` sibling a clone is staged in before its atomic
/// rename into place — unique per (process, call) so concurrent stages never
/// collide.
fn partial_path(target: &Path) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "clone".to_string());
    target.with_file_name(format!("{name}.partial.{}.{seq}", std::process::id()))
}

/// Remove any `<name>.partial.*` debris left in `dir` by a crashed clone.
fn sweep_partials(dir: &Path, name: &str) {
    let prefix = format!("{name}.partial.");
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(prefix.as_str())
            {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}

fn validated_marker(dst: &Path) -> PathBuf {
    dst.join(".git").join("lindep-validated")
}

/// Drop a marker recording that this clone passed validation, so later opens skip
/// the connectivity `fsck`. Best-effort: a write failure just means we re-fsck.
fn write_validated_marker(dst: &Path) {
    let marker = validated_marker(dst);
    if let Some(parent) = marker.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&marker, b"ok\n");
}

fn mkdir_p(dir: &Path) -> Result<(), MirrorError> {
    std::fs::create_dir_all(dir).map_err(|source| MirrorError::Io {
        path: dir.to_path_buf(),
        source,
    })
}

fn rename(from: &Path, to: &Path) -> Result<(), MirrorError> {
    std::fs::rename(from, to).map_err(|source| {
        let _ = std::fs::remove_dir_all(from);
        MirrorError::Io {
            path: to.to_path_buf(),
            source,
        }
    })
}

fn lock_path(mirrors_dir: &Path, handle: &str) -> PathBuf {
    mirrors_dir.join(format!(".{handle}.lock"))
}

/// Confirm an existing mirror was cloned from the source the registry now names,
/// so a handle isn't silently repointed at a different repo (which would corrupt
/// every project that borrows it). A local-only mirror is exempt (its source is a
/// path we may have moved). Best-effort: if git can't report the url we allow it.
fn verify_mirror_source(mirror: &Path, repo: &RepoEntry, source: &str) -> Result<(), MirrorError> {
    // Only the true-remote case is load-bearing; local-only mirrors have no stable
    // remote url to compare.
    let Some(wanted) = repo.remote.as_deref() else {
        return Ok(());
    };
    let existing = git(&["remote", "get-url", "origin"], Some(mirror))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if existing.is_empty() || existing == wanted || existing == source {
        Ok(())
    } else {
        Err(MirrorError::RemoteMismatch {
            handle: repo.handle.clone(),
            existing,
            wanted: wanted.to_string(),
        })
    }
}

/// Run `git <args>` (optionally `-C <cwd>`), returning stdout on success.
fn git(args: &[&str], cwd: Option<&Path>) -> Result<String, MirrorError> {
    let mut cmd = Command::new("git");
    if let Some(cwd) = cwd {
        cmd.arg("-C").arg(cwd);
    }
    // Never let git block on a credential or host-key prompt — fail fast instead.
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.args(args);
    let output = cmd.output().map_err(MirrorError::Spawn)?;
    if !output.status.success() {
        return Err(MirrorError::Git {
            command: format!("git {}", args.join(" ")),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A cross-process advisory lock held for the lifetime of the guard. On Unix it is
/// a real `flock(LOCK_EX)` on a lock file; elsewhere it degrades to a no-op (the
/// in-process tmp→rename still prevents same-process corruption). Released on drop.
struct FileLock {
    #[cfg(unix)]
    file: std::fs::File,
}

impl FileLock {
    #[cfg(unix)]
    fn acquire(path: &Path) -> Result<Self, MirrorError> {
        use std::os::unix::io::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .map_err(|source| MirrorError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        // SAFETY: `flock` on a valid open fd; blocks until the exclusive lock is
        // granted, then the kernel releases it when `file` (and thus the fd) drops.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(MirrorError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::last_os_error(),
            });
        }
        Ok(FileLock { file })
    }

    #[cfg(not(unix))]
    fn acquire(_path: &Path) -> Result<Self, MirrorError> {
        Ok(FileLock {})
    }
}

#[cfg(unix)]
impl Drop for FileLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        // SAFETY: releasing the advisory lock on our own fd; errors are immaterial
        // (the fd is about to close, which releases the lock regardless).
        unsafe {
            let _ = libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    /// A throwaway bare repo with one commit, standing in for a "true remote".
    /// Returns its path (under a unique temp dir).
    fn fake_remote(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let base =
            std::env::temp_dir().join(format!("lindep-mirror-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let work = base.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let run = |cwd: &Path, args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?} failed in {}", cwd.display());
        };
        run(&work, &["init", "-q", "-b", "main"]);
        run(&work, &["config", "user.email", "t@example.com"]);
        run(&work, &["config", "user.name", "Test"]);
        std::fs::write(work.join("README.md"), b"hi\n").unwrap();
        run(&work, &["add", "."]);
        run(&work, &["commit", "-q", "-m", "root"]);
        let bare = base.join("remote.git");
        let ok = Command::new("git")
            .args(["clone", "--bare", "-q"])
            .arg(&work)
            .arg(&bare)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "bare clone failed");
        bare
    }

    fn temp_layout(tag: &str) -> Layout {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("lindep-lay-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        Layout::new(root)
    }

    fn git_out(cwd: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn repo(handle: &str, remote: PathBuf) -> RepoEntry {
        RepoEntry {
            handle: handle.to_string(),
            remote: Some(remote.to_string_lossy().into_owned()),
            local: None,
        }
    }

    #[test]
    fn ensure_mirror_clones_a_bare_object_db_idempotently() {
        let layout = temp_layout("mir");
        let remote = fake_remote("mir");
        let r = repo("lindep", remote);
        let first = ensure_mirror(&layout, &r).unwrap();
        assert!(is_git_dir(&first), "a bare mirror was created");
        assert_eq!(first, layout.mirror_path("lindep"));
        // A second call returns the same mirror without re-cloning.
        let second = ensure_mirror(&layout, &r).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn ensure_clone_borrows_objects_and_points_origin_at_the_true_remote() {
        let layout = temp_layout("clone");
        let remote = fake_remote("clone");
        let r = repo("lindep", remote.clone());
        let dst = ensure_clone(&layout, "proj", &r).unwrap();
        assert!(is_work_clone(&dst), "a working reference clone exists");

        // origin points at the TRUE remote (so an agent's push reaches it)…
        let origin = git_out(&dst, &["remote", "get-url", "origin"]);
        assert_eq!(origin, remote.to_string_lossy());

        // …and its objects are borrowed from the mirror via alternates (~0 copied).
        let alternates = dst
            .join(".git")
            .join("objects")
            .join("info")
            .join("alternates");
        let contents = std::fs::read_to_string(&alternates).unwrap();
        assert!(
            contents.contains(
                &layout
                    .mirror_path("lindep")
                    .join("objects")
                    .to_string_lossy()
                    .into_owned()
            ),
            "alternates link points at the mirror: {contents:?}"
        );
    }

    #[test]
    fn one_mirror_is_reused_across_two_projects() {
        let layout = temp_layout("reuse");
        let remote = fake_remote("reuse");
        let r = repo("lindep", remote);
        let a = ensure_clone(&layout, "proj-a", &r).unwrap();
        let b = ensure_clone(&layout, "proj-b", &r).unwrap();
        assert_ne!(a, b, "each project gets its own clone");
        // Both borrow from the one shared mirror.
        let mirror = layout.mirror_path("lindep");
        assert!(is_git_dir(&mirror));
        for clone in [&a, &b] {
            let alt = std::fs::read_to_string(
                clone
                    .join(".git")
                    .join("objects")
                    .join("info")
                    .join("alternates"),
            )
            .unwrap();
            assert!(alt.contains(&mirror.join("objects").to_string_lossy().into_owned()));
        }
    }

    #[test]
    fn ensure_clone_is_idempotent_and_caches_validation() {
        let layout = temp_layout("idem");
        let remote = fake_remote("idem");
        let r = repo("lindep", remote);
        let first = ensure_clone(&layout, "proj", &r).unwrap();
        assert!(
            validated_marker(&first).exists(),
            "validation marker written"
        );
        let second = ensure_clone(&layout, "proj", &r).unwrap();
        assert_eq!(first, second, "the existing clone is reused");
    }

    #[test]
    fn a_missing_alternate_link_is_repaired_on_open() {
        let layout = temp_layout("repair");
        let remote = fake_remote("repair");
        let r = repo("lindep", remote);
        let dst = ensure_clone(&layout, "proj", &r).unwrap();
        let alternates = dst
            .join(".git")
            .join("objects")
            .join("info")
            .join("alternates");
        // Simulate a broken/absent alternate link + drop the cached marker so the
        // next open must re-validate.
        std::fs::remove_file(&alternates).unwrap();
        std::fs::remove_file(validated_marker(&dst)).unwrap();
        let again = ensure_clone(&layout, "proj", &r).unwrap();
        assert_eq!(again, dst, "the clone was reused, not rebuilt");
        assert!(alternates.exists(), "the alternate link was repaired");
    }

    #[test]
    fn a_pushed_branch_on_the_clone_reaches_the_true_remote() {
        let layout = temp_layout("push");
        let remote = fake_remote("push");
        let r = repo("lindep", remote.clone());
        let dst = ensure_clone(&layout, "proj", &r).unwrap();
        // Commit on a feature branch and push it to origin (the true remote).
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&dst)
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?} failed");
        };
        run(&["checkout", "-q", "-b", "felix/eng-1"]);
        std::fs::write(dst.join("new.txt"), b"work\n").unwrap();
        run(&["add", "."]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "Test"]);
        run(&["commit", "-q", "-m", "do work"]);
        // Auto-push via the real helper, not a raw git call.
        push_head(&dst).unwrap();
        // The branch now exists in the true remote.
        let branches = git_out(&remote, &["branch", "--list", "felix/eng-1"]);
        assert!(
            branches.contains("felix/eng-1"),
            "the agent's branch reached the true remote: {branches:?}"
        );
    }

    #[test]
    fn a_local_only_repo_synthesizes_a_mirror_and_cannot_push_to_a_remote() {
        let layout = temp_layout("localonly");
        // A local-only repo: no remote, only a local clone path. Use a bare repo
        // path as the "local" so mirroring it works without a working tree.
        let local = fake_remote("localonly");
        let r = RepoEntry {
            handle: "scratch".to_string(),
            remote: None,
            local: Some(local),
        };
        assert!(!can_push_to_remote(&r));
        let dst = ensure_clone(&layout, "proj", &r).unwrap();
        assert!(is_work_clone(&dst));
        // origin stays the synthesised mirror (no true remote to push to).
        let origin = git_out(&dst, &["remote", "get-url", "origin"]);
        assert_eq!(origin, layout.mirror_path("scratch").to_string_lossy());
    }

    #[test]
    fn an_existing_mirror_from_a_different_remote_is_refused() {
        let layout = temp_layout("mismatch");
        let remote_a = fake_remote("mismatch-a");
        let remote_b = fake_remote("mismatch-b");
        ensure_mirror(&layout, &repo("lindep", remote_a)).unwrap();
        // Same handle, different remote → refuse rather than corrupt referrers.
        let err = ensure_mirror(&layout, &repo("lindep", remote_b)).unwrap_err();
        assert!(matches!(err, MirrorError::RemoteMismatch { .. }), "{err:?}");
    }
}
