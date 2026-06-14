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
    /// Quiet but alive (Stop hook fired, conversation idle).
    Idle,
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
/// format can evolve without silently mis-parsing an old file.
#[derive(Serialize, Deserialize)]
struct Persisted {
    version: u32,
    sessions: Vec<Session>,
}

const STATE_VERSION: u32 = 1;

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
                // mis-reading it; older versions remain loadable.
                if persisted.version > STATE_VERSION {
                    return Err(StateError::Version {
                        path: path.clone(),
                        found: persisted.version,
                        supported: STATE_VERSION,
                    });
                }
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
        let stale: Vec<String> = self
            .sessions
            .keys()
            .filter(|issue| !live.contains(*issue))
            .cloned()
            .collect();
        for issue in &stale {
            self.sessions.remove(issue);
        }
        stale
    }

    /// Persist atomically: write a sibling temp file, then rename over the
    /// target. A reader therefore never observes a half-written file, and a
    /// crash mid-write leaves the previous good state intact.
    pub fn save(&self) -> Result<(), StateError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StateError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let mut sessions: Vec<Session> = self.sessions.values().cloned().collect();
        // Stable on-disk order (by issue) so the file diffs cleanly between saves.
        sessions.sort_by(|a, b| a.issue.cmp(&b.issue));
        let persisted = Persisted {
            version: STATE_VERSION,
            sessions,
        };
        let json = serde_json::to_vec_pretty(&persisted).map_err(StateError::Serialize)?;

        // Unique temp name so two saves can't clobber each other's temp file.
        let tmp = self
            .path
            .with_extension(format!("json.tmp.{}", std::process::id()));
        std::fs::write(&tmp, &json).map_err(|source| StateError::Write {
            path: tmp.clone(),
            source,
        })?;
        std::fs::rename(&tmp, &self.path).map_err(|source| {
            // Best-effort cleanup so a failed rename doesn't litter temp files.
            let _ = std::fs::remove_file(&tmp);
            StateError::Write {
                path: self.path.clone(),
                source,
            }
        })
    }
}

/// Unix seconds now. A clock somehow before the epoch yields 0 rather than
/// panicking — a nonsensical timestamp is preferable to taking the cockpit down.
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
}
