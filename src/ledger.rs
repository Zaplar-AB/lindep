//! Per-issue agent ledger — a durable, user-visible history of which `claude`
//! sessions ran for an issue and how each turned out.
//!
//! The [`SessionStore`](crate::session::SessionStore) keeps only the *current*
//! status of an agent and overwrites it in place; once an agent is reaped its
//! record is dropped, so there is no answer to "what has run on this issue, and
//! when?". The ledger fills that gap. It records one [`Episode`] per agent run —
//! from the launch (`AgentSpawned`) to the process's exit (a terminal
//! `AgentStatusChanged`, or the reap) — with start/end wall-clock, how many times
//! it asked for you, and the outcome. It is **append-only** and bounded
//! ([`MAX_EPISODES_PER_ISSUE`]) so it never grows without limit.
//!
//! It lives apart from `state.json`: that file has many off-thread writers
//! (supervisor + hook bus) and is the durable *conversation*; the ledger is a
//! view-only *history* written solely by the render thread (which sees every
//! project's lifecycle events before the scoping guard), so it sits in its own
//! sibling `.lindep/ledger.json` and reuses the same atomic, ordered
//! [`SessionStore::write_snapshot`] discipline and version guard as `cockpit.json`.

use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::session::{AgentStatus, SessionStore, StateError};

/// Bump when the on-disk `ledger.json` shape changes incompatibly.
const LEDGER_VERSION: u32 = 1;

/// Most runs retained per issue. A relaunch-heavy issue still keeps a useful tail
/// without the file (or the overlay) growing without bound; older runs age out.
const MAX_EPISODES_PER_ISSUE: usize = 30;

fn default_ledger_version() -> u32 {
    1
}

/// One agent run for an issue: a launch and everything until its process exits.
/// Wall-clock seconds (advisory — a backward clock can make `ended_at < started_at`,
/// so durations are clamped at display time, never trusted for ordering).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Episode {
    /// The `claude` session id this run drove (deterministic per project+issue, so
    /// every run of an issue shares it — it names the *conversation*, not the run).
    pub session_id: String,
    /// When the agent was launched (Unix seconds).
    pub started_at: u64,
    /// When its process exited, or `None` while it's still running.
    pub ended_at: Option<u64>,
    /// How the run ended (`Stopped`/`Done`/`Failed`), or `None` if it's still
    /// running or was closed without a terminal verdict (e.g. a setup failure).
    pub outcome: Option<AgentStatus>,
    /// How many times this run raised a "needs you" prompt.
    pub needs_you: u32,
}

impl Episode {
    /// Whether this run is still in progress (no recorded exit).
    pub fn is_open(&self) -> bool {
        self.ended_at.is_none()
    }

    /// The run's duration in seconds, clamped to ≥0 against a backward clock.
    /// `None` while the run is still open.
    pub fn duration_secs(&self) -> Option<u64> {
        self.ended_at.map(|end| end.saturating_sub(self.started_at))
    }
}

/// On-disk shape: a version tag plus one log per (project, issue). A tuple map key
/// wouldn't serialize to JSON (object keys must be strings), so the persisted form
/// is a flat list keyed explicitly.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Persisted {
    #[serde(default = "default_ledger_version")]
    version: u32,
    #[serde(default)]
    issues: Vec<IssueLog>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IssueLog {
    project_id: String,
    issue: String,
    episodes: Vec<Episode>,
}

/// The in-memory ledger, keyed by `(project_id, issue)`. Written only by the
/// render thread; persisted atomically to `ledger.json`.
#[derive(Debug, Default)]
pub struct Ledger {
    runs: HashMap<(String, String), Vec<Episode>>,
}

impl Ledger {
    /// Load the ledger, or the empty default if the file is absent. A file from a
    /// newer format is refused (so an older build can't clobber it); a corrupt file
    /// surfaces as `Parse` so the caller can degrade to the default. Any run left
    /// *open* by a previous process (an unclean exit) is closed on load with no
    /// verdict, so it reads as "interrupted" rather than a perpetual "running".
    pub fn load(path: impl AsRef<Path>) -> Result<Self, StateError> {
        let path = path.as_ref();
        let persisted: Persisted = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|source| StateError::Parse {
                path: path.to_path_buf(),
                source,
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Persisted::default(),
            Err(source) => {
                return Err(StateError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        if persisted.version > LEDGER_VERSION {
            return Err(StateError::Version {
                path: path.to_path_buf(),
                found: persisted.version,
                supported: LEDGER_VERSION,
            });
        }
        let mut runs = HashMap::new();
        for log in persisted.issues {
            let mut episodes = log.episodes;
            // Close any run that a prior process left open — its end time is
            // unknown, so we mark it interrupted (ended == started, no verdict)
            // rather than letting it masquerade as still-running.
            for ep in &mut episodes {
                if ep.is_open() {
                    ep.ended_at = Some(ep.started_at);
                }
            }
            runs.insert((log.project_id, log.issue), episodes);
        }
        Ok(Ledger { runs })
    }

    /// The runs recorded for `(project_id, issue)`, most recent last.
    pub fn episodes(&self, project_id: &str, issue: &str) -> &[Episode] {
        self.runs
            .get(&(project_id.to_string(), issue.to_string()))
            .map_or(&[], Vec::as_slice)
    }

    /// Project ids currently represented in this in-memory ledger.
    pub fn project_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .runs
            .keys()
            .map(|(project_id, _)| project_id.clone())
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// Replace this ledger's entries for `project_id` with the entries from
    /// `other` for that same project. Used when switching projects: each project's
    /// file is self-contained, but the live cockpit keeps one in-memory view so
    /// background events can still be recorded before their project is active.
    pub fn merge_project(&mut self, project_id: &str, other: Ledger) {
        self.runs.retain(|(pid, _), _| pid != project_id);
        self.runs.extend(
            other
                .runs
                .into_iter()
                .filter(|((pid, _), _)| pid == project_id),
        );
    }

    /// Begin a run: append a fresh open [`Episode`]. Any still-open run for the
    /// issue is closed first (a relaunch implies the prior process is gone), so
    /// there is at most one open episode per issue. Bounded to the most recent
    /// [`MAX_EPISODES_PER_ISSUE`].
    pub fn begin(&mut self, project_id: &str, issue: &str, session_id: String, now: u64) {
        let runs = self
            .runs
            .entry((project_id.to_string(), issue.to_string()))
            .or_default();
        if let Some(open) = runs.iter_mut().rev().find(|e| e.is_open()) {
            open.ended_at = Some(now);
        }
        runs.push(Episode {
            session_id,
            started_at: now,
            ended_at: None,
            outcome: None,
            needs_you: 0,
        });
        // Keep only the most recent runs.
        let overflow = runs.len().saturating_sub(MAX_EPISODES_PER_ISSUE);
        if overflow > 0 {
            runs.drain(..overflow);
        }
    }

    /// Record that the current run raised a "needs you" prompt (a counter on the
    /// open episode). A no-op if no run is open for the issue.
    pub fn note_needs_you(&mut self, project_id: &str, issue: &str) {
        if let Some(open) = self.open_mut(project_id, issue) {
            open.needs_you = open.needs_you.saturating_add(1);
        }
    }

    /// Close the current run with a terminal `outcome` (`Stopped`/`Done`/`Failed`).
    /// A no-op if no run is open. A later, weaker close (`note_closed`) can't
    /// overwrite a verdict already recorded here.
    pub fn note_terminal(&mut self, project_id: &str, issue: &str, outcome: AgentStatus, now: u64) {
        if let Some(open) = self.open_mut(project_id, issue) {
            open.ended_at = Some(now);
            open.outcome = Some(outcome);
        }
    }

    /// Close the current run with no verdict — the agent was reaped without a
    /// terminal status (e.g. a setup failure). A no-op if the run already ended,
    /// so it never clobbers a real `note_terminal` outcome.
    pub fn note_closed(&mut self, project_id: &str, issue: &str, now: u64) {
        if let Some(open) = self.open_mut(project_id, issue) {
            open.ended_at = Some(now);
        }
    }

    /// Close every still-open run (on cockpit quit), so a clean shutdown doesn't
    /// leave a dangling "running" episode for the next launch to interpret. Records
    /// the agents' last-known `outcome` where the caller can supply one via
    /// `status_of`, else closes with no verdict.
    pub fn close_open<F>(&mut self, now: u64, status_of: F)
    where
        F: Fn(&str, &str) -> Option<AgentStatus>,
    {
        for ((project_id, issue), runs) in &mut self.runs {
            if let Some(open) = runs.iter_mut().rev().find(|e| e.is_open()) {
                open.ended_at = Some(now);
                open.outcome = status_of(project_id, issue).filter(AgentStatus::is_terminal);
            }
        }
    }

    /// The most-recent open episode for an issue, if any.
    fn open_mut(&mut self, project_id: &str, issue: &str) -> Option<&mut Episode> {
        self.runs
            .get_mut(&(project_id.to_string(), issue.to_string()))?
            .iter_mut()
            .rev()
            .find(|e| e.is_open())
    }

    fn issue_logs_for(&self, project_filter: Option<&str>) -> Vec<IssueLog> {
        let mut issues: Vec<IssueLog> = self
            .runs
            .iter()
            .filter(|((project_id, _), _)| project_filter.is_none_or(|want| project_id == want))
            .map(|((project_id, issue), episodes)| IssueLog {
                project_id: project_id.clone(),
                issue: issue.clone(),
                episodes: episodes.clone(),
            })
            .collect();
        issues.sort_by(|a, b| {
            (a.project_id.as_str(), a.issue.as_str())
                .cmp(&(b.project_id.as_str(), b.issue.as_str()))
        });
        issues
    }

    fn save_logs(path: &Path, issues: Vec<IssueLog>) -> Result<(), StateError> {
        let persisted = Persisted {
            version: LEDGER_VERSION,
            issues,
        };
        let bytes = serde_json::to_vec_pretty(&persisted).map_err(StateError::Serialize)?;
        let seq = LEDGER_SAVE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        SessionStore::write_snapshot(path, &bytes, seq)
    }

    /// Persist atomically + durably via the shared [`SessionStore::write_snapshot`]
    /// discipline (the same as `cockpit.json`). Synchronous — called only by the
    /// render thread on a lifecycle change, never per keystroke. Issues are written
    /// in a stable order so the file diffs cleanly.
    pub fn save(&self, path: &Path) -> Result<(), StateError> {
        Self::save_logs(path, self.issue_logs_for(None))
    }

    /// Persist only one project's issue logs to that project's ledger file.
    pub fn save_project(&self, path: &Path, project_id: &str) -> Result<(), StateError> {
        Self::save_logs(path, self.issue_logs_for(Some(project_id)))
    }
}

/// Monotonic ordering seq for ledger writes. The render thread is the sole writer,
/// so this only has to satisfy [`SessionStore::write_snapshot`]'s per-path commit
/// gate (and make the temp file unique per call).
static LEDGER_SAVE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Unix seconds now, as advisory wall-clock metadata (a ledger shows wall time, so
/// this is the right clock — unlike ordering/staleness logic, which must not trust
/// it). A pre-epoch clock yields 0 rather than panicking.
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A compact "how long ago" label for a past wall-clock instant, e.g. `just now`,
/// `5m ago`, `2h ago`, `3d ago`. Clamped against a backward clock (`then > now`
/// reads `just now`).
pub fn ago(now: u64, then: u64) -> String {
    let secs = now.saturating_sub(then);
    // `< 60`, not `< 45`: with a 45s cutoff, 45–59s would hit the minutes branch and
    // render the degenerate "0m ago" (`secs / 60 == 0`). Keeping "just now" until a
    // full minute means the minutes branch only ever prints `>= 1m`.
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// A compact duration label, e.g. `8s`, `12m`, `1h3m`.
pub fn duration_label(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// A short human label for a run's outcome, for the overlay/i-panel.
pub fn outcome_label(outcome: Option<AgentStatus>) -> &'static str {
    match outcome {
        Some(AgentStatus::Done) => "done",
        Some(AgentStatus::Failed) => "failed",
        Some(AgentStatus::Stopped) => "stopped",
        // A run closed without a terminal verdict (interrupted / reaped raw).
        Some(_) | None => "ended",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_path() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("lindep-ledger-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join(".lindep").join("ledger.json")
    }

    #[test]
    fn a_run_records_start_needs_you_and_terminal_outcome() {
        let mut l = Ledger::default();
        l.begin("p", "ENG-1", "sid".into(), 100);
        l.note_needs_you("p", "ENG-1");
        l.note_needs_you("p", "ENG-1");
        l.note_terminal("p", "ENG-1", AgentStatus::Done, 160);

        let eps = l.episodes("p", "ENG-1");
        assert_eq!(eps.len(), 1);
        assert_eq!(eps[0].started_at, 100);
        assert_eq!(eps[0].ended_at, Some(160));
        assert_eq!(eps[0].outcome, Some(AgentStatus::Done));
        assert_eq!(eps[0].needs_you, 2);
        assert_eq!(eps[0].duration_secs(), Some(60));
        assert!(!eps[0].is_open());
    }

    #[test]
    fn a_relaunch_appends_a_new_run_and_closes_any_open_one() {
        let mut l = Ledger::default();
        l.begin("p", "ENG-1", "sid".into(), 100); // never explicitly ended…
        l.begin("p", "ENG-1", "sid".into(), 200); // …a relaunch closes it
        let eps = l.episodes("p", "ENG-1");
        assert_eq!(eps.len(), 2);
        assert_eq!(eps[0].ended_at, Some(200), "the prior open run was closed");
        assert!(eps[1].is_open(), "the new run is open");
    }

    #[test]
    fn note_closed_does_not_overwrite_a_terminal_verdict() {
        let mut l = Ledger::default();
        l.begin("p", "ENG-1", "sid".into(), 100);
        l.note_terminal("p", "ENG-1", AgentStatus::Failed, 150);
        l.note_closed("p", "ENG-1", 200); // a late reap must not erase the verdict
        let eps = l.episodes("p", "ENG-1");
        assert_eq!(eps[0].outcome, Some(AgentStatus::Failed));
        assert_eq!(eps[0].ended_at, Some(150));
    }

    #[test]
    fn episodes_are_bounded_to_the_most_recent() {
        let mut l = Ledger::default();
        for i in 0..(MAX_EPISODES_PER_ISSUE as u64 + 5) {
            l.begin("p", "ENG-1", "sid".into(), i);
            l.note_terminal("p", "ENG-1", AgentStatus::Done, i);
        }
        let eps = l.episodes("p", "ENG-1");
        assert_eq!(eps.len(), MAX_EPISODES_PER_ISSUE, "older runs age out");
        assert_eq!(
            eps.last().unwrap().started_at,
            MAX_EPISODES_PER_ISSUE as u64 + 4
        );
    }

    #[test]
    fn the_same_issue_in_two_projects_keeps_separate_logs() {
        let mut l = Ledger::default();
        l.begin("proj-a", "ENG-1", "a".into(), 1);
        l.begin("proj-b", "ENG-1", "b".into(), 2);
        assert_eq!(l.episodes("proj-a", "ENG-1").len(), 1);
        assert_eq!(l.episodes("proj-b", "ENG-1").len(), 1);
    }

    #[test]
    fn save_project_files_each_projects_slice_in_isolation() {
        // NEW-23: the in-memory ledger spans EVERY project (it records events before
        // the active-project scoping guard), but each project must persist to its OWN
        // file. `save_project` writes only its slice — so a backgrounded project's
        // episode can never leak into the active project's ledger.json (the H3
        // cross-file mis-file the one-file-per-project model exists to prevent). The
        // sibling `..keeps_separate_logs` proves in-memory separation; this proves the
        // separation survives the round-trip to disk, which `save_ledgers` relies on.
        let mut l = Ledger::default();
        l.begin("proj-a", "ENG-1", "a".into(), 1);
        l.note_terminal("proj-a", "ENG-1", AgentStatus::Done, 5);
        l.begin("proj-b", "ENG-9", "b".into(), 2);
        l.note_terminal("proj-b", "ENG-9", AgentStatus::Failed, 6);

        let path_a = temp_path();
        let path_b = temp_path();
        l.save_project(&path_a, "proj-a").unwrap();
        l.save_project(&path_b, "proj-b").unwrap();

        let a = Ledger::load(&path_a).unwrap();
        assert_eq!(
            a.episodes("proj-a", "ENG-1").len(),
            1,
            "A's own run is in A's file"
        );
        assert!(
            a.episodes("proj-b", "ENG-9").is_empty(),
            "B's run must NOT leak into A's ledger.json"
        );
        assert!(a.project_ids().contains(&"proj-a".to_string()));
        assert!(
            !a.project_ids().contains(&"proj-b".to_string()),
            "A's file holds only A's projects"
        );

        let b = Ledger::load(&path_b).unwrap();
        assert_eq!(
            b.episodes("proj-b", "ENG-9").len(),
            1,
            "B's own run is in B's file"
        );
        assert!(
            b.episodes("proj-a", "ENG-1").is_empty(),
            "A's run must NOT leak into B's ledger.json"
        );
    }

    #[test]
    fn round_trips_through_its_file_and_closes_danglers_on_load() {
        let path = temp_path();
        let mut l = Ledger::default();
        l.begin("p", "ENG-1", "sid".into(), 100);
        l.note_terminal("p", "ENG-1", AgentStatus::Done, 150);
        l.begin("p", "ENG-2", "sid2".into(), 200); // left OPEN (unclean exit)
        l.save(&path).unwrap();

        let reloaded = Ledger::load(&path).unwrap();
        assert_eq!(
            reloaded.episodes("p", "ENG-1")[0].outcome,
            Some(AgentStatus::Done)
        );
        let dangler = &reloaded.episodes("p", "ENG-2")[0];
        assert!(
            !dangler.is_open(),
            "a prior-run open episode is closed on load"
        );
        assert_eq!(
            dangler.outcome, None,
            "with no verdict → reads as interrupted"
        );
    }

    #[test]
    fn load_of_a_missing_file_is_empty_and_a_newer_format_is_refused() {
        assert!(
            Ledger::load(temp_path())
                .unwrap()
                .episodes("p", "x")
                .is_empty()
        );
        let path = temp_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, br#"{"version":999,"issues":[]}"#).unwrap();
        assert!(matches!(
            Ledger::load(&path),
            Err(StateError::Version { found: 999, .. })
        ));
    }

    #[test]
    fn ago_and_duration_labels_are_compact() {
        assert_eq!(ago(1000, 990), "just now");
        // 45–59s ago stays "just now" rather than the degenerate "0m ago".
        assert_eq!(ago(1000, 950), "just now");
        assert_eq!(ago(1000, 939), "1m ago");
        assert_eq!(ago(1000, 700), "5m ago");
        assert_eq!(ago(10_000, 2800), "2h ago");
        assert_eq!(ago(200_000, 100_000), "1d ago");
        assert_eq!(
            ago(100, 500),
            "just now",
            "a backward clock clamps to just now"
        );
        assert_eq!(duration_label(8), "8s");
        assert_eq!(duration_label(720), "12m");
        assert_eq!(duration_label(3780), "1h3m");
    }

    #[test]
    fn close_open_uses_the_supplied_terminal_status() {
        let mut l = Ledger::default();
        l.begin("p", "ENG-1", "sid".into(), 100);
        // On quit, a still-live agent's last-known terminal status is recorded; a
        // non-terminal status (still Running) closes with no verdict.
        l.close_open(300, |_, issue| {
            (issue == "ENG-1").then_some(AgentStatus::Stopped)
        });
        let eps = l.episodes("p", "ENG-1");
        assert_eq!(eps[0].ended_at, Some(300));
        assert_eq!(eps[0].outcome, Some(AgentStatus::Stopped));
    }
}
