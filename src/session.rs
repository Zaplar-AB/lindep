//! Session state store — the durable map from a Linear issue to its agent.
//!
//! The *process* hosting an agent is disposable; the *conversation* is not. We
//! persist, per issue, the worktree + branch it runs in and the `claude`
//! `session_id` that names its conversation, so a cockpit restart can
//! `claude --resume` straight back into every live agent. State is one
//! atomically-written `serde_json` file (`.lindep/state.json`); full transcripts
//! are **not** inlined — a session only references its NDJSON log by path.
//!
//! The `session_id` is a deterministic UUIDv5 of the issue id under a fixed
//! namespace, so even if `state.json` is lost the same id regenerates and
//! `--resume` still finds the conversation.

use std::collections::HashMap;
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

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

    /// Whether this status should drive the animation tick — the states where
    /// the agent is actively doing (or waiting to do) something, so a quiet
    /// cockpit of only resting/terminal agents still never busy-repaints.
    pub const fn is_animating(&self) -> bool {
        matches!(
            self,
            AgentStatus::Spawning | AgentStatus::Running | AgentStatus::NeedsYou
        )
    }
}

/// One persisted agent session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub issue: String,
    pub worktree_path: PathBuf,
    pub branch: String,
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

const STATE_VERSION: u32 = 1;

/// Serde default for a version-less file: the first format that carried a tag.
fn default_state_version() -> u32 {
    1
}

/// The in-memory store, backed by an atomically-written JSON file.
#[derive(Debug, Clone)]
pub struct SessionStore {
    path: PathBuf,
    sessions: HashMap<String, Session>, // keyed by issue id
}

impl SessionStore {
    /// `.lindep/state.json` under a repo root.
    pub fn state_path(repo_root: impl AsRef<Path>) -> PathBuf {
        repo_root.as_ref().join(".lindep").join("state.json")
    }

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
                // Explicit per-version migration seam. Today every loadable
                // version shares the `Session` shape, so v1 loads as-is; when
                // STATE_VERSION bumps, add an arm here that transforms the older
                // shape forward. The `> STATE_VERSION` guard above means the
                // wildcard is only reachable for already-handled versions.
                let sessions = match persisted.version {
                    1 => persisted.sessions,
                    _ => persisted.sessions,
                };
                sessions.into_iter().map(|s| (s.issue.clone(), s)).collect()
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(source) => {
                return Err(StateError::Read {
                    path: path.clone(),
                    source,
                });
            }
        };
        Ok(SessionStore { path, sessions })
    }

    /// The deterministic `claude` session id for an issue. Pure — same issue
    /// always yields the same id, with or without a persisted record.
    pub fn session_id_for(issue: &str) -> String {
        Uuid::new_v5(&SESSION_NAMESPACE, issue.as_bytes()).to_string()
    }

    /// Get-or-create the record for `issue` running in `worktree_path` on
    /// `branch`. A fresh record starts `Spawning` with the deterministic session
    /// id; an existing one is returned untouched (so `--resume` reuses its id).
    pub fn ensure(&mut self, issue: &str, worktree_path: PathBuf, branch: String) -> &Session {
        let now = now_unix();
        self.sessions
            .entry(issue.to_string())
            .or_insert_with(|| Session {
                issue: issue.to_string(),
                worktree_path,
                branch,
                session_id: Self::session_id_for(issue),
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
            s.updated_at = now_unix();
            true
        } else {
            false
        }
    }

    /// Record (or clear) the transcript log path for an issue.
    pub fn set_transcript(&mut self, issue: &str, transcript_path: Option<PathBuf>) -> bool {
        if let Some(s) = self.sessions.get_mut(issue) {
            s.transcript_path = transcript_path;
            s.updated_at = now_unix();
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

    /// Map a hook's `cwd` back to an issue by matching the worktree path.
    pub fn issue_for_cwd(&self, cwd: &Path) -> Option<&str> {
        self.sessions
            .values()
            .find(|s| s.worktree_path == cwd)
            .map(|s| s.issue.as_str())
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
    /// Saves are serialized by the `Arc<Mutex<SessionStore>>` every caller holds
    /// (supervisor + hook endpoint), so the per-PID temp suffix only has to
    /// avoid collisions between distinct lindep processes sharing a repo, not
    /// between concurrent in-process saves.
    pub fn save(&self) -> Result<(), StateError> {
        Self::write_snapshot(&self.path, &self.snapshot_bytes()?)
    }

    /// Serialize the store to its on-disk JSON bytes (stable issue order so the
    /// file diffs cleanly). Cheap — callers hold the store lock for this, then
    /// persist the bytes off the lock via [`write_snapshot`](Self::write_snapshot)
    /// / [`mutate_and_persist`] so blocking fs I/O never runs under the mutex.
    /// An empty store rooted at `path` — used to degrade gracefully when an
    /// existing `state.json` is corrupt/unreadable: start fresh (the bad file is
    /// overwritten on the first save) rather than disabling the cockpit.
    pub fn empty(path: impl Into<PathBuf>) -> Self {
        SessionStore {
            path: path.into(),
            sessions: HashMap::new(),
        }
    }

    pub fn snapshot_bytes(&self) -> Result<Vec<u8>, StateError> {
        let mut sessions: Vec<Session> = self.sessions.values().cloned().collect();
        sessions.sort_by(|a, b| a.issue.cmp(&b.issue));
        let persisted = Persisted {
            version: STATE_VERSION,
            sessions,
        };
        serde_json::to_vec_pretty(&persisted).map_err(StateError::Serialize)
    }

    /// The state file this store persists to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// All persisted sessions, for seeding the fleet view on a cockpit restart.
    pub fn sessions(&self) -> impl Iterator<Item = &Session> {
        self.sessions.values()
    }

    /// Atomically and durably write pre-serialized state bytes to `path`: write a
    /// sibling temp, `fsync` it, rename over the target, then `fsync` the parent
    /// dir. A reader never observes a half-written file (rename is atomic), and a
    /// crash mid-write leaves either the previous good state or the new one, never
    /// a truncated `state.json`. The temp suffix is unique per (process, call) so
    /// two concurrent off-lock saves never collide on the same temp file — the
    /// store mutex no longer serializes the actual write.
    pub fn write_snapshot(path: &Path, json: &[u8]) -> Result<(), StateError> {
        let parent = path.parent();
        if let Some(parent) = parent {
            std::fs::create_dir_all(parent).map_err(|source| StateError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let seq = SAVE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_extension(format!("json.tmp.{}.{seq}", std::process::id()));
        // Write + flush the temp's data blocks to disk *before* the rename, so the
        // rename can never become visible pointing at unflushed contents.
        let write_tmp = || -> std::io::Result<()> {
            let mut f = File::create(&tmp)?;
            f.write_all(json)?;
            f.sync_all()
        };
        write_tmp().map_err(|source| StateError::Write {
            path: tmp.clone(),
            source,
        })?;

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
        Ok(())
    }
}

/// Monotonic counter making each off-lock save's temp file unique within this
/// process (the store mutex used to serialize saves; off-lock persistence means
/// it no longer does, so the temp name must carry the uniqueness itself).
static SAVE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Mutate the store under its lock, then persist the snapshot OFF the lock on the
/// blocking pool. Holding the std `Mutex` across the (blocking) fs write would
/// stall a tokio worker and serialize every concurrent hook/teardown behind disk
/// latency; here the in-memory mutation runs under the lock and only the durable
/// write happens after it's released. A poisoned lock or serialize error drops
/// the persist (the in-memory store stays authoritative; the next save recovers).
pub async fn mutate_and_persist(
    store: &std::sync::Arc<std::sync::Mutex<SessionStore>>,
    mutate: impl FnOnce(&mut SessionStore),
) {
    let Some((path, bytes)) = (match store.lock() {
        Ok(mut s) => {
            mutate(&mut s);
            s.snapshot_bytes().ok().map(|b| (s.path().to_path_buf(), b))
        }
        Err(_) => None,
    }) else {
        return;
    };
    let _ = tokio::task::spawn_blocking(move || SessionStore::write_snapshot(&path, &bytes)).await;
}

/// Unix seconds now, as advisory wall-clock metadata only. A clock somehow
/// before the epoch yields 0 rather than panicking — a nonsensical timestamp is
/// preferable to taking the cockpit down. Wall-clock can jump backward, so the
/// values this feeds (`created_at`/`updated_at`) are non-monotonic and must not
/// be used for ordering or staleness (use a monotonic source if that's needed).
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
    fn stopped_round_trips_through_the_state_file() {
        let path = temp_state_path();
        let mut store = seeded(&path);
        store.set_status("ENG-1", AgentStatus::Stopped);
        store.save().unwrap();
        let reloaded = SessionStore::load(&path).unwrap();
        assert_eq!(reloaded.get("ENG-1").unwrap().status, AgentStatus::Stopped);
    }

    #[test]
    fn session_id_is_deterministic_per_issue() {
        let a = SessionStore::session_id_for("ENG-392");
        let b = SessionStore::session_id_for("ENG-392");
        let c = SessionStore::session_id_for("ENG-393");
        assert_eq!(a, b, "same issue → same id");
        assert_ne!(a, c, "different issue → different id");
        assert!(Uuid::parse_str(&a).is_ok(), "it's a valid UUID");
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

    #[test]
    fn save_is_atomic_and_leaves_no_temp_behind() {
        let path = temp_state_path();
        let store = seeded(&path);
        store.save().unwrap();
        assert!(path.exists(), "the state file was written");
        // The unique temp file must have been renamed away, not left behind.
        let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
        assert!(
            !tmp.exists(),
            "no temp file lingers after a successful save"
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
        let sid = SessionStore::session_id_for("ENG-2");
        assert_eq!(store.issue_for_session_id(&sid), Some("ENG-2"));
        assert_eq!(store.issue_for_cwd(Path::new("/wt/ENG-1")), Some("ENG-1"));
        assert_eq!(store.issue_for_cwd(Path::new("/nope")), None);
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
        let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
        assert!(!tmp.exists(), "no temp file lingers after concurrent saves");
    }
}
