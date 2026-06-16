//! Project↔repo mapping — `.lindep/projects.toml`.
//!
//! Multi-project supervision needs to know, for every Linear project, which
//! local git repo its agents run in. A declarative `[[project]]` table maps a
//! Linear `project_id` (and a human `name` label) to a `repo_root` and an
//! optional `branch_prefix`. The cockpit overlays a checked-in
//! `<repo>/.lindep/projects.toml` with a personal
//! `~/.config/lindep/projects.toml` (personal wins, by `project_id`) — the same
//! repo-then-personal precedence as the keymap's `config.toml` ([`crate::keymap`]).
//!
//! ```toml
//! # <repo>/.lindep/projects.toml
//! [[project]]
//! id = "323e926b-7bf6-414f-aced-9363ec664dc7"  # Linear project UUID
//! name = "lindep"
//! repo_root = "/home/felix/Zaplar-dev-home/lindep"
//! branch_prefix = "felix"                       # optional; per-project branch ns
//! ```
//!
//! Loading never aborts startup: an unreadable or malformed file becomes a
//! warning and is skipped (defaults / the seeded single-project mapping stand
//! in), mirroring `keymap::load`. Looking up an *unmapped* project at launch
//! time is a real error ([`ConfigError::UnmappedProject`]) rather than a silent
//! fall back to the current directory.
//!
//! The path helpers here are the single source of truth for where a project's
//! worktrees and hook settings live; the session store, supervisor and
//! notification bus consume them so two projects never collide. (`state.json`'s
//! per-project relocation + migration is owned by the session store — ENG-403 —
//! which adds the `state_path` helper on top of this.)

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Anything that can go wrong loading or resolving the project mapping. One
/// thiserror enum per subsystem, shaped like [`crate::session::StateError`].
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading project config at {}: {source}", .path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("project config {} is invalid TOML: {source}", .path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("project config {} has an invalid [[project]] entry #{index}: {source}", .path.display())]
    ParseEntry {
        path: PathBuf,
        index: usize,
        // Boxed: `toml::de::Error` is large, and an unboxed copy here would bloat
        // `ConfigError` enough to trip `clippy::result_large_err` on `resolve`.
        #[source]
        source: Box<toml::de::Error>,
    },
    #[error(
        "no repo mapping for Linear project {project_id}; \
         add a [[project]] entry to .lindep/projects.toml"
    )]
    UnmappedProject { project_id: String },
}

/// One Linear project's binding to a local git repo. The path helpers are the
/// authoritative layout: every subsystem derives a project's on-disk locations
/// from here, never by re-joining `.lindep/...` itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectMapping {
    /// Opaque Linear project id (a UUID) — the stable workspace key.
    pub project_id: String,
    /// Human label for the switcher / fleet view.
    pub name: String,
    /// The local git repo this project's agents run in.
    pub repo_root: PathBuf,
    /// Optional per-project branch namespace; `None` uses the worktree
    /// manager's compiled-in default (the git user name).
    pub branch_prefix: Option<String>,
}

impl ProjectMapping {
    /// `<repo_root>/.lindep/hooks` — per-issue Claude hook settings files live
    /// here, matching the v1 supervisor's `hooks_dir` wiring.
    pub fn hooks_dir(&self) -> PathBuf {
        self.repo_root.join(".lindep").join("hooks")
    }

    /// `<repo_root>/.lindep/state.<project_id>.json` — this project's session
    /// store, kept per-project so a future multi-project-per-repo layout never
    /// shares a state file. (Today [`WorkspaceConfig`] rejects two projects mapped
    /// to one repo, since the worktree/branch/hooks layout would still collide —
    /// see [`WorkspaceConfig::load_paths`]; full namespacing is ENG-401.)
    pub fn state_path(&self) -> PathBuf {
        self.repo_root
            .join(".lindep")
            .join(format!("state.{}.json", self.project_id))
    }

    /// The pre-v1.5 single-project `<repo_root>/.lindep/state.json`, adopted once
    /// on upgrade (see [`crate::session::SessionStore::open_project`]). Delegates
    /// to the canonical helper so the legacy path has one definition.
    pub fn legacy_state_path(&self) -> PathBuf {
        crate::session::SessionStore::state_path(&self.repo_root)
    }
}

/// The resolved workspace mapping: every configured project keyed by its Linear
/// `project_id`.
#[derive(Debug, Default, Clone)]
pub struct WorkspaceConfig {
    projects: HashMap<String, ProjectMapping>,
}

impl WorkspaceConfig {
    /// Load the mapping: a checked-in `<repo>/.lindep/projects.toml` overlaid by
    /// a personal `~/.config/lindep/projects.toml` (personal entries win by
    /// `project_id`). Returns the config plus any warnings to surface — a bad
    /// file never aborts startup, exactly like [`crate::keymap::load`].
    pub fn load(repo_root: Option<&Path>) -> (Self, Vec<String>) {
        let mut paths: Vec<PathBuf> = Vec::new();
        if let Some(root) = repo_root {
            paths.push(root.join(".lindep").join("projects.toml"));
        }
        if let Some(home) = std::env::var_os("HOME") {
            paths.push(PathBuf::from(home).join(".config/lindep/projects.toml"));
        }
        Self::load_paths(&paths)
    }

    /// Core loader over an explicit, ordered path list (earliest first, latest
    /// wins). Split out so tests drive it with temp files instead of mutating
    /// the process-global `HOME`.
    fn load_paths(paths: &[PathBuf]) -> (Self, Vec<String>) {
        let mut cfg = WorkspaceConfig::default();
        let mut warnings = Vec::new();
        for path in paths {
            let text = match std::fs::read_to_string(path) {
                Ok(text) => text,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => {
                    warnings.push(
                        ConfigError::Read {
                            path: path.clone(),
                            source,
                        }
                        .to_string(),
                    );
                    continue;
                }
            };
            // Parse the document first, then convert each `[[project]]` table on its
            // own: one malformed entry (e.g. a missing `repo_root`) warns and is
            // skipped, without discarding the file's other, valid projects.
            let doc = match text.parse::<toml::Table>() {
                Ok(doc) => doc,
                Err(source) => {
                    warnings.push(
                        ConfigError::Parse {
                            path: path.clone(),
                            source,
                        }
                        .to_string(),
                    );
                    continue;
                }
            };
            let entries = match doc.get("project") {
                // A file with no `[[project]]` tables contributes nothing.
                None => continue,
                Some(toml::Value::Array(entries)) => entries,
                Some(_) => {
                    warnings.push(format!(
                        "project config {}: `project` must be an array of [[project]] tables",
                        path.display()
                    ));
                    continue;
                }
            };
            for (index, raw) in entries.iter().enumerate() {
                match raw.clone().try_into::<ProjectEntry>() {
                    Ok(entry) => {
                        let mapping = entry.into_mapping(path);
                        // A later file (personal) overrides an earlier one (repo)
                        // for the same project_id.
                        cfg.projects.insert(mapping.project_id.clone(), mapping);
                    }
                    Err(source) => warnings.push(
                        ConfigError::ParseEntry {
                            path: path.clone(),
                            index,
                            source: Box::new(source),
                        }
                        .to_string(),
                    ),
                }
            }
        }
        cfg.reject_repo_root_collisions(&mut warnings);
        (cfg, warnings)
    }

    /// Drop any project whose `repo_root` is already claimed by another. The
    /// on-disk layout (worktrees, branches, hooks, the legacy `state.json`) is
    /// keyed by issue, not project, so two Linear projects sharing one repo would
    /// collide — a same-keyed issue would resolve to the same worktree dir and
    /// branch, silently running two conversations on one tree. Until that layout is
    /// namespaced by project (ENG-401), refuse the duplicate rather than corrupt.
    /// The lexicographically-smallest `project_id` wins, deterministically.
    fn reject_repo_root_collisions(&mut self, warnings: &mut Vec<String>) {
        let mut owner: HashMap<PathBuf, String> = HashMap::new();
        let mut ids: Vec<String> = self.projects.keys().cloned().collect();
        ids.sort();
        for id in ids {
            let root = self.projects[&id].repo_root.clone();
            // Best-effort identity: canonicalize when the path exists (collapsing
            // `..`/symlinks), else compare it as resolved (it may not exist until
            // first launch).
            let key = root.canonicalize().unwrap_or_else(|_| root.clone());
            match owner.get(&key) {
                None => {
                    owner.insert(key, id);
                }
                Some(winner) => {
                    warnings.push(format!(
                        "project {id}: repo_root {} is already mapped to project {winner}; \
                         ignoring the duplicate (two projects can't share one repo yet)",
                        root.display()
                    ));
                    self.projects.remove(&id);
                }
            }
        }
    }

    /// Resolve a project to its repo mapping, or an actionable
    /// [`ConfigError::UnmappedProject`] if it isn't configured — never a silent
    /// fall back to the current directory.
    pub fn resolve(&self, project_id: &str) -> Result<&ProjectMapping, ConfigError> {
        self.projects
            .get(project_id)
            .ok_or_else(|| ConfigError::UnmappedProject {
                project_id: project_id.to_string(),
            })
    }

    /// The configured project ids — the set the in-cockpit switcher offers, since
    /// only a mapped project (one with a repo) can run agents.
    pub fn mapped_ids(&self) -> Vec<String> {
        self.projects.keys().cloned().collect()
    }

    /// Inject a single-project mapping for `id` if none is configured, so a
    /// cockpit launched in an unmapped project still boots — the degenerate
    /// one-project case that reproduces pre-v1.5 behaviour with zero setup.
    /// Returns `true` if it had to synthesize one (the caller may then write a
    /// starter `projects.toml`).
    pub fn ensure_mapped(&mut self, id: &str, name: &str, repo_root: &Path) -> bool {
        if self.projects.contains_key(id) {
            return false;
        }
        self.projects.insert(
            id.to_string(),
            ProjectMapping {
                project_id: id.to_string(),
                name: name.to_string(),
                repo_root: repo_root.to_path_buf(),
                branch_prefix: None,
            },
        );
        true
    }
}

/// A starter `projects.toml` body for the active project, written under
/// `<repo>/.lindep/` on first run (when no repo-level file exists) so adding
/// further projects is discoverable. `.lindep/` is gitignored, so this never
/// touches the tracked tree.
pub fn seed_file_contents(mapping: &ProjectMapping) -> String {
    // Serialize the typed entry with the `toml` crate so every value (a free-text
    // Linear name, a local path) is escaped correctly — a name containing a quote,
    // backslash, newline or control char must never emit a file that then fails to
    // parse (which would warn on every subsequent launch).
    let file = ProjectsFile {
        project: vec![ProjectEntry {
            id: mapping.project_id.clone(),
            name: mapping.name.clone(),
            repo_root: mapping.repo_root.to_string_lossy().into_owned(),
            branch_prefix: mapping.branch_prefix.clone(),
        }],
    };
    let body = toml::to_string(&file).unwrap_or_default();
    let header = "# lindep — project↔repo mapping.\n\
         #\n\
         # Add a [[project]] table per Linear project you want to supervise.\n\
         # `id` is the Linear project UUID; `repo_root` is its local git repo.\n\
         # Personal/overriding entries can live in ~/.config/lindep/projects.toml.\n\n";
    // Surface the optional per-project branch namespace when it isn't already set.
    let hint = if mapping.branch_prefix.is_none() {
        "# branch_prefix = \"yourname\"\n"
    } else {
        ""
    };
    format!("{header}{body}{hint}")
}

/// On-disk shape of `projects.toml`: an array of `[[project]]` tables. Also the
/// shape [`seed_file_contents`] serializes, so the starter template is produced
/// by the same encoder that reads it.
#[derive(Debug, Default, Deserialize, Serialize)]
struct ProjectsFile {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    project: Vec<ProjectEntry>,
}

/// One `[[project]]` table as written on disk (paths still as raw strings, so
/// `~`/relative forms can be expanded against the config file before becoming a
/// [`PathBuf`]).
#[derive(Debug, Deserialize, Serialize)]
struct ProjectEntry {
    id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    name: String,
    repo_root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    branch_prefix: Option<String>,
}

impl ProjectEntry {
    fn into_mapping(self, config_path: &Path) -> ProjectMapping {
        ProjectMapping {
            project_id: self.id,
            name: self.name,
            repo_root: resolve_repo_root(&self.repo_root, config_path),
            // Treat a blank prefix as "use the default".
            branch_prefix: self.branch_prefix.filter(|s| !s.trim().is_empty()),
        }
    }
}

/// Expand a leading `~`/`~/` to `$HOME`, else leave the path untouched.
fn expand_tilde(raw: &str) -> PathBuf {
    if raw == "~"
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home);
    } else if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(raw)
}

/// Resolve a configured `repo_root`: expand `~`, take absolute paths as-is, and
/// anchor a relative path at the repo that owns a checked-in
/// `<repo>/.lindep/projects.toml` (falling back to the current dir for the
/// personal `~/.config/lindep` file or when the layout is unexpected). The path
/// is **not** canonicalized here — `WorktreeManager::with_prefix` does that at
/// use, and would error on a not-yet-created path.
fn resolve_repo_root(raw: &str, config_path: &Path) -> PathBuf {
    let expanded = expand_tilde(raw);
    if expanded.is_absolute() {
        return expanded;
    }
    let anchor = config_path
        .parent()
        .filter(|p| p.file_name() == Some(std::ffi::OsStr::new(".lindep")))
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    anchor.join(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A process-unique temp dir, no extra crates (matching the worktree/session
    /// test helpers).
    fn temp_dir(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("lindep-proj-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write `contents` to `<root>/.lindep/projects.toml` and return that path.
    fn write_repo_config(root: &Path, contents: &str) -> PathBuf {
        let path = root.join(".lindep").join("projects.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn parses_a_two_project_file_keyed_by_id() {
        let root = temp_dir("two");
        let path = write_repo_config(
            &root,
            r#"
            [[project]]
            id = "p1"
            name = "Alpha"
            repo_root = "/repos/alpha"
            branch_prefix = "felix"

            [[project]]
            id = "p2"
            name = "Beta"
            repo_root = "/repos/beta"
            "#,
        );
        let (cfg, warnings) = WorkspaceConfig::load_paths(&[path]);
        assert!(warnings.is_empty(), "{warnings:?}");
        let a = cfg.resolve("p1").unwrap();
        assert_eq!(a.name, "Alpha");
        assert_eq!(a.repo_root, PathBuf::from("/repos/alpha"));
        assert_eq!(a.branch_prefix.as_deref(), Some("felix"));
        let b = cfg.resolve("p2").unwrap();
        assert_eq!(b.repo_root, PathBuf::from("/repos/beta"));
        // An omitted branch_prefix is None (the worktree manager's default).
        assert_eq!(b.branch_prefix, None);
    }

    #[test]
    fn personal_overrides_repo_by_project_id() {
        let repo = temp_dir("ov-repo");
        let personal = temp_dir("ov-home");
        let repo_path = write_repo_config(
            &repo,
            r#"
            [[project]]
            id = "p1"
            name = "Repo label"
            repo_root = "/repos/from-repo"
            "#,
        );
        // The personal file is read last and wins for the shared id; it can also
        // add a project the repo file never mentioned.
        let personal_path = personal.join("projects.toml");
        std::fs::write(
            &personal_path,
            r#"
            [[project]]
            id = "p1"
            name = "Personal label"
            repo_root = "/repos/from-personal"

            [[project]]
            id = "p9"
            name = "Only personal"
            repo_root = "/repos/p9"
            "#,
        )
        .unwrap();

        let (cfg, warnings) = WorkspaceConfig::load_paths(&[repo_path, personal_path]);
        assert!(warnings.is_empty(), "{warnings:?}");
        let p1 = cfg.resolve("p1").unwrap();
        assert_eq!(p1.name, "Personal label");
        assert_eq!(p1.repo_root, PathBuf::from("/repos/from-personal"));
        assert!(cfg.resolve("p9").is_ok());
    }

    #[test]
    fn resolve_of_an_unmapped_project_is_an_actionable_error() {
        let cfg = WorkspaceConfig::default();
        let err = cfg.resolve("nope").unwrap_err();
        assert!(matches!(err, ConfigError::UnmappedProject { .. }));
        assert!(err.to_string().contains("nope"));
        assert!(err.to_string().contains("projects.toml"));
    }

    #[test]
    fn a_relative_repo_root_anchors_at_the_repo_owning_the_config() {
        let root = temp_dir("rel");
        let path = write_repo_config(
            &root,
            r#"
            [[project]]
            id = "p1"
            name = "Rel"
            repo_root = "nested/repo"
            "#,
        );
        let (cfg, _) = WorkspaceConfig::load_paths(&[path]);
        // `<root>/.lindep/projects.toml` → relative paths anchor at `<root>`.
        assert_eq!(
            cfg.resolve("p1").unwrap().repo_root,
            root.join("nested/repo")
        );
    }

    #[test]
    fn tilde_in_repo_root_expands_to_home() {
        let Some(home) = std::env::var_os("HOME") else {
            return; // no HOME in this environment; nothing to assert
        };
        let root = temp_dir("tilde");
        let path = write_repo_config(
            &root,
            r#"
            [[project]]
            id = "p1"
            name = "Tilde"
            repo_root = "~/code/thing"
            "#,
        );
        let (cfg, _) = WorkspaceConfig::load_paths(&[path]);
        assert_eq!(
            cfg.resolve("p1").unwrap().repo_root,
            PathBuf::from(home).join("code/thing")
        );
    }

    #[test]
    fn a_missing_file_is_an_empty_config_with_no_warning() {
        let (cfg, warnings) =
            WorkspaceConfig::load_paths(&[PathBuf::from("/does/not/exist/projects.toml")]);
        assert!(warnings.is_empty());
        assert!(cfg.resolve("anything").is_err());
    }

    #[test]
    fn malformed_toml_warns_and_keeps_the_other_files_entries() {
        let bad_root = temp_dir("bad");
        let good_root = temp_dir("good");
        let bad = write_repo_config(&bad_root, "this is not = = valid toml [[[");
        let good = good_root.join("projects.toml");
        std::fs::write(
            &good,
            r#"
            [[project]]
            id = "ok"
            name = "Fine"
            repo_root = "/repos/ok"
            "#,
        )
        .unwrap();

        let (cfg, warnings) = WorkspaceConfig::load_paths(&[bad, good]);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("invalid TOML"));
        // The good file still loaded despite the bad one.
        assert!(cfg.resolve("ok").is_ok());
    }

    #[test]
    fn two_projects_mapped_to_the_same_repo_drop_the_duplicate() {
        let root = temp_dir("dup");
        // Both projects point at the SAME repo_root — a collision the on-disk
        // layout (worktrees/branches/hooks/legacy state) cannot host yet.
        let path = write_repo_config(
            &root,
            r#"
            [[project]]
            id = "p1"
            name = "A"
            repo_root = "/repos/shared"

            [[project]]
            id = "p2"
            name = "B"
            repo_root = "/repos/shared"
            "#,
        );
        let (cfg, warnings) = WorkspaceConfig::load_paths(&[path]);
        // The lexicographically-smaller id wins; the other is dropped with a warning.
        assert!(cfg.resolve("p1").is_ok(), "the first project survives");
        assert!(cfg.resolve("p2").is_err(), "the duplicate is rejected");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("p2"));
        assert!(warnings[0].contains("already mapped"));
    }

    #[test]
    fn a_single_malformed_entry_is_skipped_and_the_rest_load() {
        let root = temp_dir("partial");
        // The middle entry is missing the required `repo_root`.
        let path = write_repo_config(
            &root,
            r#"
            [[project]]
            id = "good1"
            name = "Good one"
            repo_root = "/repos/good1"

            [[project]]
            id = "bad"
            name = "Missing repo_root"

            [[project]]
            id = "good2"
            repo_root = "/repos/good2"
            "#,
        );
        let (cfg, warnings) = WorkspaceConfig::load_paths(&[path]);
        assert!(cfg.resolve("good1").is_ok(), "the entry before the bad one loads");
        assert!(cfg.resolve("good2").is_ok(), "the entry after the bad one loads");
        assert!(cfg.resolve("bad").is_err(), "the malformed entry is skipped");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("invalid [[project]] entry"));
    }

    #[test]
    fn ensure_mapped_seeds_only_when_absent() {
        let mut cfg = WorkspaceConfig::default();
        assert!(cfg.ensure_mapped("p1", "Seeded", Path::new("/repos/seed")));
        let m = cfg.resolve("p1").unwrap();
        assert_eq!(m.name, "Seeded");
        assert_eq!(m.repo_root, PathBuf::from("/repos/seed"));
        assert_eq!(m.branch_prefix, None);
        // A second call for the same id is a no-op (an existing mapping wins).
        assert!(!cfg.ensure_mapped("p1", "Other", Path::new("/repos/other")));
        assert_eq!(
            cfg.resolve("p1").unwrap().repo_root,
            PathBuf::from("/repos/seed")
        );
    }

    #[test]
    fn hooks_dir_matches_the_v1_layout() {
        let m = ProjectMapping {
            project_id: "p".into(),
            name: "n".into(),
            repo_root: PathBuf::from("/repo"),
            branch_prefix: None,
        };
        assert_eq!(m.hooks_dir(), PathBuf::from("/repo/.lindep/hooks"));
    }

    #[test]
    fn the_seed_template_round_trips_back_to_the_same_mapping() {
        let m = ProjectMapping {
            project_id: "323e926b".into(),
            name: "lindep".into(),
            repo_root: PathBuf::from("/home/felix/lindep"),
            branch_prefix: None,
        };
        let toml = seed_file_contents(&m);
        let root = temp_dir("seed");
        let path = write_repo_config(&root, &toml);
        let (cfg, warnings) = WorkspaceConfig::load_paths(&[path]);
        assert!(warnings.is_empty(), "{warnings:?}");
        let back = cfg.resolve("323e926b").unwrap();
        assert_eq!(back.project_id, m.project_id);
        assert_eq!(back.name, m.name);
        assert_eq!(back.repo_root, m.repo_root);
    }
}
