//! Session state store — the durable map from a Linear issue to its agent.
//!
//! The *process* hosting an agent is disposable; the *conversation* is not. We
//! persist, per issue, the worktree + branch it runs in, the repo handles it
//! materialised, and the `claude` `session_id` that names its conversation, so a
//! cockpit restart can `claude --resume` straight back into every live agent.
//! State is one atomically-written `serde_json` file per project, under v1.6's
//! managed layout at `~/.lindep/projects/<handle>/state.json` (see
//! [`crate::registry::Layout`]); full transcripts are **not** inlined — a session
//! only references its NDJSON log by path.
//!
//! The `session_id` is a deterministic UUIDv5 of `"{project_id}:{issue}"` under
//! a fixed namespace, so even if the state file is lost the same id regenerates
//! and `--resume` still finds the conversation — and the same issue key in two
//! projects gets two distinct conversations. (Sessions persisted before v1.5
//! were keyed on the bare issue; their stored id is preserved verbatim on the
//! v1→v2 migration so `--resume` continuity survives the rekey.) `STATE_VERSION`
//! is `3` as of v1.6, which added the per-issue repo handle set additively.

use std::collections::HashMap;
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::event::{AppEvent, AppEventTx};

/// Fixed namespace for per-issue session UUIDv5s. Arbitrary but constant — it
/// only has to be stable across machines and runs.
const SESSION_NAMESPACE: Uuid = Uuid::from_u128(0x6c69_6e64_6570_5f73_6573_7369_6f6e_7331);

/// Where an agent stands, as far as the cockpit knows. Persisted with the
/// session and mirrored live by the supervisor + notification bus; the fleet
/// view renders it. (Absence of a session is the "no agent" state, so it isn't
/// a variant here.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// Launch requested; process starting.
    Spawning,
    /// Actively working (producing output, running tools).
    Running,
    /// Waiting on the human — a permission prompt or an idle nudge.
    NeedsYou,
    /// Quiet but alive: the agent finished a turn (a `Stop` hook fired) and is
    /// resting with its process still up, ready to continue. Distinct from
    /// [`AgentStatus::Stopped`], whose process is gone.
    Idle,
    /// The user deliberately stopped the agent: its process is dead, but its
    /// conversation is resumable (`--resume`). This is the post-cancel resting
    /// state — separated from `Idle` so a *stopped* agent stops counting as
    /// "on" the instant you stop it.
    Stopped,
    /// Finished its task cleanly.
    Done,
    /// Exited abnormally / errored.
    Failed,
}

impl AgentStatus {
    /// Whether this status means the agent is waiting on the human. Takes
    /// `&self` so it can be passed directly as `Option::is_some_and`'s predicate.
    pub const fn needs_you(&self) -> bool {
        matches!(self, AgentStatus::NeedsYou)
    }

    /// Whether a live process backs this status — i.e. the agent is genuinely
    /// "on". This is what the header counts: a `Stopped`/`Done`/`Failed` agent
    /// (and an issue with no agent at all) is *not* live, so the count drops the
    /// moment you stop or finish one. `Idle` is live (resting, still up).
    pub const fn is_live(&self) -> bool {
        matches!(
            self,
            AgentStatus::Spawning
                | AgentStatus::Running
                | AgentStatus::NeedsYou
                | AgentStatus::Idle
        )
    }

    /// Whether the agent's process is gone for good — a `Stopped` (deliberate
    /// stop), `Done` (clean finish) or `Failed` (crash). The exact complement of
    /// [`is_live`](Self::is_live); it closes a [`crate::ledger`] episode.
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self,
            AgentStatus::Stopped | AgentStatus::Done | AgentStatus::Failed
        )
    }

    /// Whether this status should drive the animation tick — the states where
    /// the agent is actively doing (or waiting to do) something, so a quiet
    /// cockpit of only resting/terminal agents still never busy-repaints.
    pub const fn is_animating(&self) -> bool {
        matches!(
            self,
            AgentStatus::Spawning | AgentStatus::Running | AgentStatus::NeedsYou
        )
    }

    /// Ordering rank for the agents roster — lower sorts first. needs-you leads
    /// (it's the one that must be acted on), then live work (running, then the
    /// brief spawning), then idle, then the terminal states — a crash (Failed)
    /// ahead of a clean stop/finish so a failure stays visible at the top of the
    /// dead pile. Ranks 0–2 are exactly the [`AgentStatus::is_animating`] set, so
    /// the roster's "live work floats up" order tracks what visibly moves.
    pub const fn salience_rank(self) -> u8 {
        match self {
            AgentStatus::NeedsYou => 0,
            AgentStatus::Running => 1,
            AgentStatus::Spawning => 2,
            AgentStatus::Idle => 3,
            AgentStatus::Failed => 4,
            AgentStatus::Stopped => 5,
            AgentStatus::Done => 6,
        }
    }
}

/// One persisted agent session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub issue: String,
    /// The Linear project this session belongs to. Disambiguates the same issue
    /// key across projects and folds into the deterministic `session_id`.
    /// `#[serde(default)]` so a pre-v1.5 (v1) state file — which has no
    /// `project_id` — still loads; the owning store stamps it on load via
    /// [`SessionStore::for_project`], preserving the stored `session_id`.
    #[serde(default)]
    pub project_id: String,
    /// The agent's working directory: the single worktree for a one-repo issue,
    /// or the per-issue **workspace** parent (with each repo a sibling subdir) for
    /// a multi-repo issue.
    pub worktree_path: PathBuf,
    pub branch: String,
    /// The repo handles this issue has materialised — the per-issue repo set
    /// (v1.6, `STATE_VERSION 3`). Empty on a fresh single-repo record and on a
    /// migrated v2 file (carried by `#[serde(default)]`); populated by the launch
    /// path and the agent lazy-pull as repos are checked out.
    #[serde(default)]
    pub repos: Vec<String>,
    /// `claude`'s session id (a UUID string), passed as `--session-id` /
    /// `--resume`.
    pub session_id: String,
    pub status: AgentStatus,
    /// Path to this session's NDJSON transcript log, if one has been recorded.
    /// The transcript itself is never inlined into the state file.
    #[serde(default)]
    pub transcript_path: Option<PathBuf>,
    /// Advisory wall-clock metadata (Unix seconds), not a monotonic clock: a
    /// backward jump or a pre-epoch clock can make these non-increasing, so they
    /// must **not** be used for ordering or staleness logic. If duration/ordering
    /// is ever needed, add a monotonic source rather than trusting these.
    pub created_at: u64,
    pub updated_at: u64,
}

/// Anything that can go wrong persisting or loading session state.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("reading state at {}: {source}", .path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("writing state at {}: {source}", .path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("state file {} is corrupt: {source}", .path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "state file {} was written by a newer lindep (format v{found}, this build understands v{supported})",
        .path.display()
    )]
    Version {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    #[error("serializing state: {0}")]
    Serialize(#[source] serde_json::Error),
}

/// On-disk shape: a plain version tag plus the session list. Versioned so the
/// format can evolve without silently mis-parsing an old file. A file missing
/// the tag entirely (predating it, or hand-edited) is treated as v1 rather than
/// rejected as corrupt — see [`default_state_version`].
#[derive(Serialize, Deserialize)]
struct Persisted {
    #[serde(default = "default_state_version")]
    version: u32,
    sessions: Vec<Session>,
}

/// v1 → v2 (lindep v1.5): added `Session::project_id` and folded the project into
/// the deterministic `session_id`. v2 → v3 (lindep v1.6): added `Session::repos`,
/// the per-issue repo handle set under managed workspaces. Both bumps are purely
/// additive — the new fields ride a `#[serde(default)]`, so every version
/// deserializes as-is; the only migration is stamping the owning `project_id` onto
/// unstamped records (done in [`SessionStore::for_project`], which preserves the
/// stored `session_id`). v1.6 abandons the in-repo `.lindep` location wholesale
/// (state now lives under `~/.lindep/projects/<handle>/`), so there is no legacy
/// adoption: a fresh per-project store stands in where none exists.
const STATE_VERSION: u32 = 3;

/// Serde default for a version-less file: the first format that carried a tag.
fn default_state_version() -> u32 {
    1
}

/// The in-memory store, backed by an atomically-written JSON file. One store
/// owns exactly one project's sessions (the workspace holds one per project).
#[derive(Debug, Clone)]
pub struct SessionStore {
    path: PathBuf,
    /// The Linear project this store's sessions belong to — stamped onto new
    /// records and onto migrated v1 records (see [`SessionStore::for_project`]).
    /// Empty for a bare [`SessionStore::load`] that hasn't been claimed yet.
    project_id: String,
    sessions: HashMap<String, Session>, // keyed by issue id
}

impl SessionStore {
    /// Load the store from `path`, or start empty if the file doesn't exist yet.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, StateError> {
        let path = path.into();
        let sessions = match std::fs::read(&path) {
            Ok(bytes) => {
                let persisted: Persisted =
                    serde_json::from_slice(&bytes).map_err(|source| StateError::Parse {
                        path: path.clone(),
                        source,
                    })?;
                // Refuse a file from a future format rather than silently
                // mis-reading it; a version-less or older file is migrated up.
                if persisted.version > STATE_VERSION {
                    return Err(StateError::Version {
                        path: path.clone(),
                        found: persisted.version,
                        supported: STATE_VERSION,
                    });
                }
                // v1, v2 and v3 share the `Session` shape: v2 added `project_id`
                // and v3 added `repos`, each carried by `#[serde(default)]` and (for
                // `project_id`) stamped onto unstamped records later by
                // [`SessionStore::for_project`] (which preserves the stored
                // `session_id`, so `--resume` survives). So every loadable version
                // deserializes as-is here — there is no per-version transform yet.
                // A future *structural* bump would branch on `persisted.version`
                // before this point; the `> STATE_VERSION` guard above has already
                // rejected anything newer than we understand.
                persisted
                    .sessions
                    .into_iter()
                    .map(|s| (s.issue.clone(), s))
                    .collect()
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(source) => {
                return Err(StateError::Read {
                    path: path.clone(),
                    source,
                });
            }
        };
        Ok(SessionStore {
            path,
            project_id: String::new(),
            sessions,
        })
    }

    /// Claim this store for `project_id`: record it (so new records and their
    /// deterministic ids are project-scoped) and stamp it onto any record that
    /// lacks one — the v1→v2 migration for a pre-v1.5 `state.json`. Stored
    /// `session_id`s are **preserved verbatim**, never recomputed, so a
    /// migrated agent keeps the conversation `--resume` expects even though new
    /// agents derive their id from `{project_id}:{issue}`.
    pub fn for_project(mut self, project_id: &str) -> Self {
        self.project_id = project_id.to_string();
        for session in self.sessions.values_mut() {
            if session.project_id.is_empty() {
                session.project_id = project_id.to_string();
            }
        }
        self
    }

    /// Open project `project_id`'s store at its per-project `state_path` under
    /// `~/.lindep/projects/<handle>/`. v1.6 abandons the pre-v1.6 in-repo
    /// `.lindep/state*.json` location outright — there is no legacy adoption — so a
    /// never-opened project simply starts empty. Stored `session_id`s are
    /// preserved verbatim by [`for_project`](Self::for_project).
    pub fn open_project(project_id: &str, state_path: PathBuf) -> Result<Self, StateError> {
        Ok(SessionStore::load(state_path)?.for_project(project_id))
    }

    /// The deterministic `claude` session id for a `(project_id, issue)` pair.
    /// Pure — the same pair always yields the same id, with or without a
    /// persisted record, and the same issue in two projects yields two ids.
    pub fn session_id_for(project_id: &str, issue: &str) -> String {
        Uuid::new_v5(
            &SESSION_NAMESPACE,
            format!("{project_id}:{issue}").as_bytes(),
        )
        .to_string()
    }

    /// Get-or-create the record for `issue` running in `worktree_path` on
    /// `branch`. A fresh record starts `Spawning` with the deterministic session
    /// id for this store's project; an existing one is returned untouched (so
    /// `--resume` reuses its id).
    pub fn ensure(&mut self, issue: &str, worktree_path: PathBuf, branch: String) -> &Session {
        let now = crate::ledger::now_unix();
        let project_id = self.project_id.clone();
        self.sessions
            .entry(issue.to_string())
            .or_insert_with(|| Session {
                issue: issue.to_string(),
                session_id: Self::session_id_for(&project_id, issue),
                project_id,
                worktree_path,
                branch,
                repos: Vec::new(),
                status: AgentStatus::Spawning,
                transcript_path: None,
                created_at: now,
                updated_at: now,
            })
    }

    /// Look up a session by issue.
    pub fn get(&self, issue: &str) -> Option<&Session> {
        self.sessions.get(issue)
    }

    /// Update an agent's status (and bump `updated_at`). No-op if the issue has
    /// no record. Returns whether a record was found and changed.
    pub fn set_status(&mut self, issue: &str, status: AgentStatus) -> bool {
        if let Some(s) = self.sessions.get_mut(issue) {
            s.status = status;
            s.updated_at = crate::ledger::now_unix();
            true
        } else {
            false
        }
    }

    /// Record (or clear) the transcript log path for an issue.
    pub fn set_transcript(&mut self, issue: &str, transcript_path: Option<PathBuf>) -> bool {
        if let Some(s) = self.sessions.get_mut(issue) {
            s.transcript_path = transcript_path;
            s.updated_at = crate::ledger::now_unix();
            true
        } else {
            false
        }
    }

    /// Record the per-issue materialised repo handle set (v1.6 `STATE_VERSION 3`).
    /// Written by the launch path once the chosen repos are checked out and, for a
    /// mid-session lazy-pull, only **after** the new clone lands — so a crash never
    /// leaves the set claiming a repo that isn't on disk. No-op if the issue has no
    /// record; returns whether one was found and changed.
    pub fn set_repos(&mut self, issue: &str, repos: Vec<String>) -> bool {
        if let Some(s) = self.sessions.get_mut(issue) {
            s.repos = repos;
            s.updated_at = crate::ledger::now_unix();
            true
        } else {
            false
        }
    }

    /// Reverse lookups for the notification bus, which only knows a hook's
    /// `session_id` and `cwd`.
    pub fn issue_for_session_id(&self, session_id: &str) -> Option<&str> {
        self.sessions
            .values()
            .find(|s| s.session_id == session_id)
            .map(|s| s.issue.as_str())
    }

    /// Map a hook's `cwd` back to an issue by matching the worktree path. An exact
    /// match wins; otherwise the session whose `worktree_path` is the closest
    /// ancestor of `cwd`. A multi-repo issue's agent runs in the per-issue
    /// **workspace** parent (`worktrees/<ISSUE>`) with each repo a sibling subdir,
    /// so a hook fired from inside a repo subdir (`cwd = <workspace>/<repo>`, e.g. a
    /// post-commit hook) still resolves to its issue. Distinct issues' worktree
    /// paths are siblings, never nested, so the longest-prefix match is unambiguous.
    pub fn issue_for_cwd(&self, cwd: &Path) -> Option<&str> {
        self.sessions
            .values()
            .filter(|s| cwd == s.worktree_path || cwd.starts_with(&s.worktree_path))
            .max_by_key(|s| s.worktree_path.as_os_str().len())
            .map(|s| s.issue.as_str())
    }

    /// Drop the record for `issue` — e.g. after its workspace is torn down
    /// (ENG-541). Returns whether a record was removed. The caller persists the
    /// store afterwards (via [`mutate_and_persist`]).
    pub fn forget(&mut self, issue: &str) -> bool {
        self.sessions.remove(issue).is_some()
    }

    /// Drop records whose worktree is no longer live, given the set of issues
    /// that currently have a worktree (from [`crate::worktree::WorktreeManager::list`]).
    /// Returns the issues that were pruned, so the caller can log them.
    pub fn reconcile<I, S>(&mut self, live_issues: I) -> Vec<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let live: std::collections::HashSet<String> = live_issues
            .into_iter()
            .map(|s| s.as_ref().to_string())
            .collect();
        // Prune in place: one hash lookup per key, and we only clone the keys we
        // genuinely have to return (the pruned ones), not every stale key twice.
        let mut pruned = Vec::new();
        self.sessions.retain(|issue, _| {
            let keep = live.contains(issue.as_str());
            if !keep {
                pruned.push(issue.clone());
            }
            keep
        });
        pruned
    }

    /// Persist atomically *and durably*: write a sibling temp file, `fsync` it,
    /// rename over the target, then `fsync` the parent directory. A reader never
    /// observes a half-written file (rename is atomic), and because the temp
    /// data and the rename are both flushed before we return, a crash mid-write
    /// — including power/kernel loss — leaves either the previous good state or
    /// the new one intact, never a truncated `state.json`.
    ///
    /// This synchronous helper is for callers already holding the store lock; the
    /// off-lock production paths use [`mutate_and_persist`]. Either way the per-
    /// path commit gate in [`write_snapshot`](Self::write_snapshot) orders writes
    /// by their `seq`, so two writers can't revert each other.
    pub fn save(&self) -> Result<(), StateError> {
        let (bytes, seq) = self.snapshot_with_seq()?;
        Self::write_snapshot(&self.path, &bytes, seq)
    }

    /// An empty store rooted at `path` — used to degrade gracefully when an
    /// existing `state.json` is corrupt/unreadable: start fresh (the bad file is
    /// overwritten on the first save) rather than disabling the cockpit.
    pub fn empty(path: impl Into<PathBuf>) -> Self {
        SessionStore {
            path: path.into(),
            project_id: String::new(),
            sessions: HashMap::new(),
        }
    }

    /// Serialize the store to its on-disk JSON bytes (stable issue order so the
    /// file diffs cleanly). Cheap — callers hold the store lock for this, then
    /// persist the bytes off the lock via [`write_snapshot`](Self::write_snapshot)
    /// / [`mutate_and_persist`] so blocking fs I/O never runs under the mutex.
    pub fn snapshot_bytes(&self) -> Result<Vec<u8>, StateError> {
        let mut sessions: Vec<Session> = self.sessions.values().cloned().collect();
        // Stable order so the file diffs cleanly. A store owns exactly one project,
        // so `project_id` is constant across these records; it stays in the sort key
        // as a future-proof guard rather than a discriminator that does work today.
        sessions.sort_by(|a, b| {
            (a.project_id.as_str(), a.issue.as_str())
                .cmp(&(b.project_id.as_str(), b.issue.as_str()))
        });
        let persisted = Persisted {
            version: STATE_VERSION,
            sessions,
        };
        serde_json::to_vec_pretty(&persisted).map_err(StateError::Serialize)
    }

    /// Snapshot the bytes **and** claim a monotonic ordering `seq` in one step,
    /// to be called under the store lock. The off-lock persists snapshot in lock
    /// order but their blocking writes finish out of order; passing `seq` into
    /// [`write_snapshot`](Self::write_snapshot) lets its commit gate drop a
    /// late-landing older write, so a newer durable transition is never reverted.
    pub fn snapshot_with_seq(&self) -> Result<(Vec<u8>, u64), StateError> {
        let bytes = self.snapshot_bytes()?;
        // Assigned under the caller's lock, so seqs strictly increase in the same
        // order the mutations were applied.
        let seq = SAVE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok((bytes, seq))
    }

    /// The state file this store persists to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// All persisted sessions, for seeding the fleet view on a cockpit restart.
    pub fn sessions(&self) -> impl Iterator<Item = &Session> {
        self.sessions.values()
    }

    /// Atomically, durably, **and in order** write pre-serialized state bytes to
    /// `path` for the snapshot stamped `seq` (see
    /// [`snapshot_with_seq`](Self::snapshot_with_seq)): write a sibling temp,
    /// `fsync` it, then — under a per-path commit gate — rename over the target
    /// and `fsync` the parent dir.
    ///
    /// * **Atomic + durable.** A reader never observes a half-written file (rename
    ///   is atomic), and a crash mid-write leaves either the previous good state
    ///   or the new one, never a truncated `state.json`.
    /// * **Ordered.** The off-lock persists (supervisor + hook bus) snapshot under
    ///   the store lock but their blocking writes finish in arbitrary order on the
    ///   pool. The gate skips any write whose `seq` is older than what already
    ///   committed for this path, so a late-landing older snapshot can never
    ///   revert a newer durable transition. The gate is keyed by path, so
    ///   independent state files never block or shadow each other.
    pub fn write_snapshot(path: &Path, json: &[u8], seq: u64) -> Result<(), StateError> {
        let parent = path.parent();
        if let Some(parent) = parent {
            std::fs::create_dir_all(parent).map_err(|source| StateError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        // The temp name carries (pid, seq) so concurrent off-lock writes — even
        // across distinct lindep processes sharing a repo — never collide.
        let tmp = path.with_extension(format!("json.tmp.{}.{seq}", std::process::id()));
        // Write + flush the temp's data blocks to disk *before* the rename, so the
        // rename can never become visible pointing at unflushed contents. This
        // (the slow part) runs OFF the commit gate so writers prepare in parallel.
        let write_tmp = || -> std::io::Result<()> {
            let mut f = File::create(&tmp)?;
            f.write_all(json)?;
            f.sync_all()
        };
        write_tmp().map_err(|source| StateError::Write {
            path: tmp.clone(),
            source,
        })?;

        // Commit under the per-path gate. Holding it across the rename serializes
        // the durable effect; the seq check drops a stale (older) writer so an
        // out-of-order completion can't revert newer state.
        let mut committed = COMMIT_SEQS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let highest = committed.entry(path.to_path_buf()).or_insert(0);
        if seq < *highest {
            // A newer snapshot already committed for this path — ours is stale.
            let _ = std::fs::remove_file(&tmp);
            return Ok(());
        }

        std::fs::rename(&tmp, path).map_err(|source| {
            // Best-effort cleanup so a failed rename doesn't litter temp files.
            let _ = std::fs::remove_file(&tmp);
            StateError::Write {
                path: path.to_path_buf(),
                source,
            }
        })?;

        // fsync the parent directory so the rename itself is durable — otherwise a
        // power loss after the data flush can still roll the rename back, silently
        // reverting a committed status change.
        if let Some(parent) = parent {
            File::open(parent)
                .and_then(|d| d.sync_all())
                .map_err(|source| StateError::Write {
                    path: parent.to_path_buf(),
                    source,
                })?;
        }
        *highest = seq;
        Ok(())
    }
}

/// Monotonic counter stamping each snapshot with an ordering `seq` (claimed under
/// the store lock by [`SessionStore::snapshot_with_seq`]). It both makes the temp
/// file unique per (process, call) and lets the commit gate drop a stale write.
static SAVE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Highest snapshot `seq` already committed, per state-file path. The off-lock
/// persists snapshot in lock order but finish their blocking writes out of order;
/// [`SessionStore::write_snapshot`] consults this under the lock so a late older
/// write is dropped instead of reverting a newer durable transition. Keyed by
/// path so two independent state files never gate each other.
static COMMIT_SEQS: std::sync::LazyLock<std::sync::Mutex<HashMap<PathBuf, u64>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Whether the last off-lock persist failed, so a wedged disk surfaces exactly
/// one footer notification per failure episode (not one per hook) and goes quiet
/// again on the next success. See [`persist_snapshot`].
static PERSIST_FAILING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Mutate the store under its lock, then persist the snapshot OFF the lock on the
/// blocking pool. Holding the std `Mutex` across the (blocking) fs write would
/// stall a tokio worker and serialize every concurrent hook/teardown behind disk
/// latency; here the in-memory mutation runs under the lock and only the durable
/// write happens after it's released. A poisoned lock or serialize error drops
/// the persist (the in-memory store stays authoritative; the next save recovers).
/// The snapshot is stamped with an ordering `seq` under the lock so the off-lock
/// write commits in mutation order; `events` carries a single footer
/// notification if the durable write itself fails (see [`persist_snapshot`]).
pub async fn mutate_and_persist(
    store: &std::sync::Arc<std::sync::Mutex<SessionStore>>,
    events: &AppEventTx,
    mutate: impl FnOnce(&mut SessionStore),
) {
    let Some((path, bytes, seq)) = (match store.lock() {
        Ok(mut s) => {
            mutate(&mut s);
            s.snapshot_with_seq()
                .ok()
                .map(|(b, seq)| (s.path().to_path_buf(), b, seq))
        }
        Err(_) => None,
    }) else {
        return;
    };
    persist_snapshot(events, path, bytes, seq).await;
}

/// Persist a captured snapshot OFF the lock on the blocking pool, committing in
/// `seq` order (the commit gate in [`SessionStore::write_snapshot`] drops a
/// late-landing older write). The in-memory store stays authoritative, so a write
/// failure doesn't change control flow — but it isn't swallowed either: a genuine
/// durable-write failure surfaces exactly one footer notification per failure
/// episode (throttled via [`PERSIST_FAILING`] so a stuck disk doesn't flood the
/// footer on every hook), going quiet again on the next success.
pub async fn persist_snapshot(events: &AppEventTx, path: PathBuf, bytes: Vec<u8>, seq: u64) {
    use std::sync::atomic::Ordering;
    let outcome =
        tokio::task::spawn_blocking(move || SessionStore::write_snapshot(&path, &bytes, seq)).await;
    let error = match outcome {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(e.to_string()),
        Err(join) => Some(format!("persist task aborted: {join}")),
    };
    match error {
        None => PERSIST_FAILING.store(false, Ordering::Relaxed),
        Some(msg) => {
            if !PERSIST_FAILING.swap(true, Ordering::Relaxed) {
                let _ = events.send(AppEvent::Notification(format!(
                    "session state save failed: {msg}"
                )));
            }
        }
    }
}

// ── Cockpit window-layout persistence (Cockpit v3) ──────────────────────────
//
// The *durable conversation* lives in `state.json` (above). The cockpit's
// *window layout* — which docked windows are open, in what order, the layout
// mode, and which one had focus — is a separate, view-only concern, so it lives
// in a sibling `cockpit.json`. Kept apart from `state.json` deliberately: that
// file has many off-thread writers (supervisor + hook bus), whereas the cockpit
// layout has exactly one writer (the render thread), so folding them together
// would invite cross-writer contention for no benefit. Reuses the same atomic,
// ordered [`SessionStore::write_snapshot`] discipline and version guard.

/// Bump when the on-disk `cockpit.json` shape changes incompatibly.
const COCKPIT_VERSION: u32 = 1;

fn default_cockpit_version() -> u32 {
    1
}

/// The kind of a persisted docked window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistedKind {
    Agent,
    Deps,
    Fleet,
}

/// One docked window, persisted by *identity* (its issue/root), never by index —
/// so a restore re-finds it against the reconcile survivor set rather than
/// trusting a position that may no longer mean the same thing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedWindow {
    pub kind: PersistedKind,
    /// The issue (Agent) or root (Deps); `None` for the singleton Fleet window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<String>,
}

/// The persisted cockpit layout: the docked windows in pin order, the layout
/// mode, and the focused window's identity. A missing file deserializes to the
/// empty default — exactly today's behaviour — so persistence ships risk-free
/// behind no flag.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CockpitState {
    #[serde(default = "default_cockpit_version")]
    pub version: u32,
    /// `"rail"` or `"mosaic"`; empty/unknown (incl. the legacy `"filmstrip"`)
    /// restores the default rail layout.
    #[serde(default)]
    pub layout: String,
    #[serde(default)]
    pub windows: Vec<PersistedWindow>,
    /// The focused window's identity, or `None` for the Spine / an unpinned
    /// preview (which isn't persisted, so focus falls back to the Spine).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<PersistedWindow>,
}

impl Default for CockpitState {
    fn default() -> Self {
        CockpitState {
            version: COCKPIT_VERSION,
            layout: String::new(),
            windows: Vec::new(),
            focus: None,
        }
    }
}

/// Monotonic ordering seq for cockpit writes. The render thread is the sole
/// writer, so this only has to make [`SessionStore::write_snapshot`]'s per-path
/// commit gate happy (and the temp file unique per call).
static COCKPIT_SAVE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl CockpitState {
    /// Load the layout, or the empty default if the file is absent. A file from a
    /// newer format is refused (so an older build can't clobber it); a corrupt
    /// file surfaces as `Parse` so the caller can degrade to the default.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, StateError> {
        let path = path.as_ref();
        match std::fs::read(path) {
            Ok(bytes) => {
                let state: CockpitState =
                    serde_json::from_slice(&bytes).map_err(|source| StateError::Parse {
                        path: path.to_path_buf(),
                        source,
                    })?;
                if state.version > COCKPIT_VERSION {
                    return Err(StateError::Version {
                        path: path.to_path_buf(),
                        found: state.version,
                        supported: COCKPIT_VERSION,
                    });
                }
                Ok(state)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(CockpitState::default()),
            Err(source) => Err(StateError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Persist atomically + durably via the shared [`SessionStore::write_snapshot`]
    /// discipline. Synchronous — called only by the render thread (the sole
    /// writer) on a structural change or on quit, never per keystroke.
    pub fn save(&self, path: &Path) -> Result<(), StateError> {
        let bytes = serde_json::to_vec_pretty(self).map_err(StateError::Serialize)?;
        let seq = COCKPIT_SAVE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        SessionStore::write_snapshot(path, &bytes, seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_state_path() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("lindep-state-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join(".lindep").join("state.json")
    }

    fn seeded(path: &Path) -> SessionStore {
        let mut store = SessionStore::load(path).unwrap();
        store.ensure("ENG-1", PathBuf::from("/wt/ENG-1"), "felix/eng-1".into());
        store.ensure("ENG-2", PathBuf::from("/wt/ENG-2"), "felix/eng-2".into());
        store
    }

    #[test]
    fn is_live_counts_only_agents_with_a_running_process() {
        use AgentStatus::*;
        for s in [Spawning, Running, NeedsYou, Idle] {
            assert!(s.is_live(), "{s:?} is a live agent");
        }
        for s in [Stopped, Done, Failed] {
            assert!(!s.is_live(), "{s:?} is not live — its process is gone");
        }
    }

    #[test]
    fn cockpit_state_round_trips_through_its_file() {
        let path = temp_state_path().with_file_name("cockpit.json");
        // A missing file is the empty default.
        assert!(CockpitState::load(&path).unwrap().windows.is_empty());

        let state = CockpitState {
            layout: "mosaic".into(),
            windows: vec![
                PersistedWindow {
                    kind: PersistedKind::Agent,
                    issue: Some("ENG-1".into()),
                },
                PersistedWindow {
                    kind: PersistedKind::Deps,
                    issue: Some("ENG-2".into()),
                },
                PersistedWindow {
                    kind: PersistedKind::Fleet,
                    issue: None,
                },
            ],
            focus: Some(PersistedWindow {
                kind: PersistedKind::Agent,
                issue: Some("ENG-1".into()),
            }),
            ..CockpitState::default()
        };
        state.save(&path).unwrap();
        let reloaded = CockpitState::load(&path).unwrap();
        assert_eq!(reloaded.layout, "mosaic");
        assert_eq!(reloaded.windows, state.windows);
        assert_eq!(reloaded.focus, state.focus);
    }

    #[test]
    fn cockpit_state_rejects_a_file_from_a_newer_format() {
        let path = temp_state_path().with_file_name("cockpit.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"version":999,"layout":"","windows":[]}"#).unwrap();
        assert!(matches!(
            CockpitState::load(&path),
            Err(StateError::Version { found: 999, .. })
        ));
    }

    #[test]
    fn stopped_round_trips_through_the_state_file() {
        let path = temp_state_path();
        let mut store = seeded(&path);
        store.set_status("ENG-1", AgentStatus::Stopped);
        store.save().unwrap();
        let reloaded = SessionStore::load(&path).unwrap();
        assert_eq!(reloaded.get("ENG-1").unwrap().status, AgentStatus::Stopped);
    }

    #[test]
    fn session_id_is_deterministic_per_project_and_issue() {
        let a = SessionStore::session_id_for("proj", "ENG-392");
        let b = SessionStore::session_id_for("proj", "ENG-392");
        let c = SessionStore::session_id_for("proj", "ENG-393");
        assert_eq!(a, b, "same (project, issue) → same id");
        assert_ne!(a, c, "different issue → different id");
        assert!(Uuid::parse_str(&a).is_ok(), "it's a valid UUID");
    }

    #[test]
    fn the_same_issue_in_two_projects_yields_distinct_session_ids() {
        // The whole point of folding project_id into the v5 input: two projects'
        // ENG-1 must be distinct conversations, never a shared --resume target.
        let p1 = SessionStore::session_id_for("project-a", "ENG-1");
        let p2 = SessionStore::session_id_for("project-b", "ENG-1");
        assert_ne!(p1, p2);
    }

    #[test]
    fn a_v1_state_file_is_stamped_with_the_owning_project_but_keeps_its_session_id() {
        // A pre-v1.5 file has no project_id and a session_id derived from the bare
        // issue. The migration must stamp the owning project_id WITHOUT recomputing
        // the id, or a running agent loses its conversation on the first restart.
        let path = temp_state_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"version":1,"sessions":[{"issue":"ENG-7","worktree_path":"/wt/ENG-7","branch":"felix/eng-7","session_id":"legacy-sid-keep-me","status":"idle","created_at":0,"updated_at":0}]}"#,
        )
        .unwrap();
        let store = SessionStore::load(&path).unwrap().for_project("proj-x");
        let s = store.get("ENG-7").unwrap();
        assert_eq!(s.project_id, "proj-x", "the owning project is stamped");
        assert_eq!(
            s.session_id, "legacy-sid-keep-me",
            "the stored id is preserved, not recomputed — --resume survives"
        );
    }

    #[test]
    fn open_project_loads_its_per_project_file_and_starts_empty_when_absent() {
        // v1.6: state lives under ~/.lindep/projects/<handle>/state.json; a
        // never-opened project simply starts empty (no legacy in-repo adoption).
        let path = temp_state_path();
        let absent = SessionStore::open_project("proj-x", path.clone()).unwrap();
        assert!(
            absent.get("ENG-1").is_none(),
            "a fresh project starts empty"
        );

        // What it persists, it reloads — stamped with the owning project.
        let mut store = SessionStore::open_project("proj-x", path.clone()).unwrap();
        store.ensure("ENG-1", "/wt/ENG-1".into(), "b".into());
        store.set_status("ENG-1", AgentStatus::Done);
        store.save().unwrap();
        let reloaded = SessionStore::open_project("proj-x", path).unwrap();
        assert_eq!(reloaded.get("ENG-1").unwrap().project_id, "proj-x");
        assert_eq!(reloaded.get("ENG-1").unwrap().status, AgentStatus::Done);
    }

    #[test]
    fn a_v2_state_file_loads_as_v3_with_an_empty_repo_set() {
        // v3 adds `repos`; a v2 file (no `repos`) must load with it defaulted to
        // empty, never rejected — the additive-field migration.
        let path = temp_state_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"version":2,"sessions":[{"issue":"ENG-7","project_id":"p","worktree_path":"/wt/ENG-7","branch":"felix/eng-7","session_id":"sid","status":"idle","created_at":0,"updated_at":0}]}"#,
        )
        .unwrap();
        let store = SessionStore::load(&path).unwrap();
        assert!(
            store.get("ENG-7").unwrap().repos.is_empty(),
            "the absent repo set defaults to empty"
        );
    }

    #[test]
    fn the_repo_set_round_trips_through_the_state_file() {
        let path = temp_state_path();
        let mut store = SessionStore::load(&path).unwrap().for_project("p");
        store.ensure("ENG-1", "/wt/ENG-1".into(), "b".into());
        // Stamp a multi-repo set onto the record, then persist + reload.
        if let Some(s) = store.sessions.get_mut("ENG-1") {
            s.repos = vec!["lindep".into(), "shared-proto".into()];
        }
        store.save().unwrap();
        let reloaded = SessionStore::load(&path).unwrap();
        assert_eq!(
            reloaded.get("ENG-1").unwrap().repos,
            vec!["lindep".to_string(), "shared-proto".to_string()]
        );
    }

    #[test]
    fn ensure_creates_then_returns_the_same_record() {
        let path = temp_state_path();
        let mut store = SessionStore::load(&path).unwrap();
        let id1 = store
            .ensure("ENG-1", "/wt/ENG-1".into(), "b".into())
            .session_id
            .clone();
        let id2 = store
            .ensure("ENG-1", "/other".into(), "z".into())
            .session_id
            .clone();
        assert_eq!(id1, id2, "second ensure returns the original record");
        assert_eq!(
            store.get("ENG-1").unwrap().worktree_path,
            PathBuf::from("/wt/ENG-1")
        );
    }

    #[test]
    fn round_trips_through_the_file() {
        let path = temp_state_path();
        let mut store = seeded(&path);
        store.set_status("ENG-1", AgentStatus::Running);
        store.save().unwrap();

        let reloaded = SessionStore::load(&path).unwrap();
        assert_eq!(reloaded.get("ENG-1").unwrap().status, AgentStatus::Running);
        assert_eq!(reloaded.get("ENG-2").unwrap().status, AgentStatus::Spawning);
    }

    #[test]
    fn load_of_a_missing_file_is_an_empty_store() {
        let store = SessionStore::load(temp_state_path()).unwrap();
        assert!(store.get("anything").is_none());
    }

    #[test]
    fn load_rejects_a_state_file_from_a_newer_format() {
        let path = temp_state_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"version":999,"sessions":[]}"#).unwrap();
        assert!(matches!(
            SessionStore::load(&path),
            Err(StateError::Version { found: 999, .. })
        ));
    }

    #[test]
    fn load_rejects_a_corrupt_state_file() {
        // Truncated / non-JSON bytes (e.g. an interrupted write on a non-atomic
        // filesystem, or a hand-edit) must surface as Parse, not silently load.
        let path = temp_state_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        assert!(matches!(
            SessionStore::load(&path),
            Err(StateError::Parse { .. })
        ));

        // Valid JSON of the wrong shape (sessions is an object, not a list) is
        // also a Parse error, not a panic or a half-built store.
        let path = temp_state_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"version":1,"sessions":{}}"#).unwrap();
        assert!(matches!(
            SessionStore::load(&path),
            Err(StateError::Parse { .. })
        ));
    }

    #[test]
    fn load_treats_a_version_less_file_as_v1() {
        // A file predating the version tag (or hand-edited to drop it) must load
        // as legacy v1 via the serde default, not fail as corrupt.
        let path = temp_state_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"sessions":[{"issue":"ENG-7","worktree_path":"/wt/ENG-7","branch":"felix/eng-7","session_id":"sid","status":"running","created_at":0,"updated_at":0}]}"#,
        )
        .unwrap();
        let store = SessionStore::load(&path).expect("version-less file loads as v1");
        assert_eq!(store.get("ENG-7").unwrap().status, AgentStatus::Running);
    }

    /// Every `state.json.tmp.{pid}.{seq}` sibling still present next to `path`.
    /// The real temp carries a per-call `seq` suffix, so a fixed-name `exists()`
    /// check would never match it — scan the directory for the real artifact.
    fn lingering_temps(path: &Path) -> Vec<String> {
        let dir = path.parent().unwrap();
        std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.starts_with("state.json.tmp."))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp_behind() {
        let path = temp_state_path();
        let store = seeded(&path);
        store.save().unwrap();
        assert!(path.exists(), "the state file was written");
        // The unique temp file must have been renamed away, not left behind.
        let lingering = lingering_temps(&path);
        assert!(
            lingering.is_empty(),
            "no temp file lingers after a successful save: {lingering:?}"
        );
    }

    #[test]
    fn reconcile_prunes_sessions_whose_worktree_vanished() {
        let path = temp_state_path();
        let mut store = seeded(&path);
        // Only ENG-1 still has a live worktree.
        let pruned = store.reconcile(["ENG-1"]);
        assert_eq!(pruned, vec!["ENG-2"]);
        assert!(store.get("ENG-1").is_some());
        assert!(store.get("ENG-2").is_none());
    }

    #[test]
    fn reverse_lookups_map_session_id_and_cwd_to_issue() {
        let path = temp_state_path();
        let store = seeded(&path);
        // `seeded` builds the store via plain `load` (project_id ""), so ensure
        // derives ids under the empty project.
        let sid = SessionStore::session_id_for("", "ENG-2");
        assert_eq!(store.issue_for_session_id(&sid), Some("ENG-2"));
        assert_eq!(store.issue_for_cwd(Path::new("/wt/ENG-1")), Some("ENG-1"));
        assert_eq!(store.issue_for_cwd(Path::new("/nope")), None);
    }

    #[test]
    fn set_repos_records_the_per_issue_materialised_set() {
        let path = temp_state_path();
        let mut store = seeded(&path);
        assert!(
            store.get("ENG-1").unwrap().repos.is_empty(),
            "a fresh record has no repos"
        );
        assert!(store.set_repos("ENG-1", vec!["api".into(), "web".into()]));
        assert_eq!(store.get("ENG-1").unwrap().repos, vec!["api", "web"]);
        // Latest selection wins; an unknown issue is a no-op.
        assert!(store.set_repos("ENG-1", vec!["api".into()]));
        assert_eq!(store.get("ENG-1").unwrap().repos, vec!["api"]);
        assert!(!store.set_repos("ENG-404", vec!["x".into()]));
    }

    #[test]
    fn issue_for_cwd_resolves_a_multi_repo_repo_subdir_to_its_issue() {
        // A multi-repo agent runs in worktrees/<ISSUE>/ with each repo a sibling
        // subdir, so a hook fired from inside a repo subdir (e.g. a post-commit
        // hook, cwd = <workspace>/<repo>) must still resolve to its issue.
        let path = temp_state_path();
        let store = seeded(&path);
        assert_eq!(
            store.issue_for_cwd(Path::new("/wt/ENG-1/api")),
            Some("ENG-1")
        );
        assert_eq!(
            store.issue_for_cwd(Path::new("/wt/ENG-2/web/src")),
            Some("ENG-2")
        );
        // An unrelated path still resolves to nothing.
        assert_eq!(store.issue_for_cwd(Path::new("/wt-other/x")), None);
    }

    #[test]
    fn set_status_and_set_transcript_report_whether_a_record_was_found() {
        let path = temp_state_path();
        let mut store = seeded(&path);
        // A seeded issue is found and changed.
        assert!(
            store.set_status("ENG-1", AgentStatus::Done),
            "seeded issue is updated"
        );
        assert_eq!(store.get("ENG-1").unwrap().status, AgentStatus::Done);
        assert!(
            store.set_transcript("ENG-1", Some(PathBuf::from("/t/ENG-1.ndjson"))),
            "seeded issue's transcript is recorded"
        );
        // An unknown issue (e.g. one reaped/reconciled away) is a no-op that
        // reports false — the contract the call sites rely on to detect a lost
        // status update.
        assert!(
            !store.set_status("ENG-404", AgentStatus::Done),
            "unknown issue is not changed"
        );
        assert!(
            !store.set_transcript("ENG-404", None),
            "unknown issue's transcript is not recorded"
        );
        assert!(store.get("ENG-404").is_none(), "no record was created");
    }

    #[test]
    fn concurrent_locked_saves_stay_consistent_and_leave_no_temp() {
        use std::sync::{Arc, Mutex};

        // Mirror the real architecture: every caller mutates + saves through a
        // shared Arc<Mutex<SessionStore>>. The per-PID temp suffix is constant
        // for the process, so this only stays correct because the mutex
        // serializes saves — the invariant the save() doc now spells out.
        let path = temp_state_path();
        let store = Arc::new(Mutex::new(SessionStore::load(&path).unwrap()));
        const TASKS: usize = 8;
        const ITERS: usize = 25;

        let handles: Vec<_> = (0..TASKS)
            .map(|t| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    let issue = format!("ENG-{t}");
                    for i in 0..ITERS {
                        let mut guard = store.lock().unwrap();
                        guard.ensure(
                            &issue,
                            PathBuf::from(format!("/wt/{issue}")),
                            format!("felix/eng-{t}"),
                        );
                        // Alternate status so updated state really is written.
                        let status = if i % 2 == 0 {
                            AgentStatus::Running
                        } else {
                            AgentStatus::Idle
                        };
                        assert!(guard.set_status(&issue, status));
                        guard.save().expect("save under the lock succeeds");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("worker thread did not panic");
        }

        // The reloaded store has exactly one record per task and no .tmp lingers.
        let reloaded = SessionStore::load(&path).unwrap();
        for t in 0..TASKS {
            assert!(
                reloaded.get(&format!("ENG-{t}")).is_some(),
                "each task's record survived the concurrent saves"
            );
        }
        let lingering = lingering_temps(&path);
        assert!(
            lingering.is_empty(),
            "no temp file lingers after concurrent saves: {lingering:?}"
        );
    }

    #[test]
    fn an_out_of_order_stale_write_does_not_clobber_a_newer_one() {
        // The off-lock persist race: a newer snapshot (higher seq) commits, then
        // an earlier snapshot's delayed write lands. The per-path commit gate must
        // drop the stale write so the disk keeps the newer transition rather than
        // reverting to the older one — the durable lost-update the seq guards.
        let path = temp_state_path();

        let mut earlier = SessionStore::load(&path).unwrap();
        earlier.ensure("ENG-1", "/wt/ENG-1".into(), "b".into());
        earlier.set_status("ENG-1", AgentStatus::Running);
        let (earlier_bytes, earlier_seq) = earlier.snapshot_with_seq().unwrap();

        let mut later = SessionStore::load(&path).unwrap();
        later.ensure("ENG-1", "/wt/ENG-1".into(), "b".into());
        later.set_status("ENG-1", AgentStatus::Done);
        let (later_bytes, later_seq) = later.snapshot_with_seq().unwrap();
        assert!(
            earlier_seq < later_seq,
            "the later transition claims the higher seq"
        );

        // Commit the LATER snapshot, then let the EARLIER (delayed) write land.
        SessionStore::write_snapshot(&path, &later_bytes, later_seq).unwrap();
        SessionStore::write_snapshot(&path, &earlier_bytes, earlier_seq).unwrap();

        // Disk must still reflect the later transition; the stale write was dropped.
        let reloaded = SessionStore::load(&path).unwrap();
        assert_eq!(
            reloaded.get("ENG-1").unwrap().status,
            AgentStatus::Done,
            "the stale older write did not revert the newer durable state"
        );
        let lingering = lingering_temps(&path);
        assert!(
            lingering.is_empty(),
            "the dropped stale write left no temp behind: {lingering:?}"
        );
    }
}
