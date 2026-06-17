//! Global workspace registry — `~/.lindep/registry.toml` + the `~/.lindep` layout.
//!
//! v1.6 pivots lindep from a **repo-local tool** (run inside a checkout, anchored
//! by `git rev-parse --show-toplevel`) into a **workspace manager run from
//! anywhere**. Instead of pointing each Linear project at one on-disk repo
//! (`projects.toml`'s `repo_root`), a single global registry names every repo you
//! own **once** — by `handle` — and binds each Linear project to a *set* of those
//! handles. lindep then owns the on-disk location: it provisions an isolated
//! per-project directory under `~/.lindep/projects/<handle>/` and materialises
//! per-issue worktrees there via the 3-layer git model (bare mirror → reference
//! clone → worktree; see [`crate::mirror`]).
//!
//! ```toml
//! # ~/.lindep/registry.toml
//! [[repo]]                                  # every repo you own, named once
//! handle = "lindep"
//! remote = "git@github.com:zaplar/lindep"   # canonical fetch source
//! local  = "/home/felix/code/lindep"        # OPTIONAL: your clone, a read-only --reference alternate
//!
//! [[project]]                               # a Linear project ↔ a set of repos
//! id            = "323e926b-…"              # Linear project UUID (the stable key)
//! handle        = "lindep-core"             # the per-project dir name
//! name          = "Lindep Core"
//! candidates    = ["lindep", "shared-proto"] # fixed superset — the trust boundary
//! primary       = "lindep"                  # always materialised at launch
//! branch_prefix = "felix"                   # optional per-project branch namespace
//! ```
//!
//! Loading never aborts startup: an unreadable or malformed file, or a single bad
//! `[[repo]]`/`[[project]]` entry, becomes a warning and is skipped — the same
//! warn-never-abort discipline as the v1.5 `projects.toml` loader and the keymap.
//! A project whose `primary`/`candidates` reference an unknown repo handle, or
//! whose own handle isn't a safe directory name, is dropped with a warning rather
//! than corrupting the on-disk layout.
//!
//! This module is the single source of truth for the `~/.lindep` layout: the
//! session store, supervisor, notification bus and clone substrate all derive a
//! project's on-disk locations from [`Layout`], never by re-joining paths
//! themselves — so the per-project directory name lives in exactly one place.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Anything that can go wrong loading or resolving the registry. One `thiserror`
/// enum per subsystem, shaped like [`crate::session::StateError`]. Loading itself
/// converts most of these to warnings (it never aborts); [`UnknownProject`] is the
/// one a *lookup* surfaces.
///
/// [`UnknownProject`]: RegistryError::UnknownProject
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("reading registry at {}: {source}", .path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("registry {} is invalid TOML: {source}", .path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("registry {} has an invalid [[{table}]] entry #{index}: {source}", .path.display())]
    ParseEntry {
        path: PathBuf,
        table: &'static str,
        index: usize,
        // Boxed: `toml::de::Error` is large; an unboxed copy would bloat the enum.
        #[source]
        source: Box<toml::de::Error>,
    },
    #[error(
        "no registry entry for Linear project {project_id}; \
         add a [[project]] entry to ~/.lindep/registry.toml"
    )]
    UnknownProject { project_id: String },
    #[error("writing registry at {}: {source}", .path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("registry {} is invalid TOML, can't edit it in place: {source}", .path.display())]
    EditParse {
        path: PathBuf,
        #[source]
        source: Box<toml_edit::TomlError>,
    },
}

/// One repo you own, named once by its `handle`. The handle is the stable
/// identity (replacing v1.5's `repo_root`); the mirror, every reference clone and
/// every worktree are derived from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoEntry {
    /// Stable, path-safe identity — the mirror is `~/.lindep/mirrors/<handle>.git`.
    pub handle: String,
    /// Canonical fetch/push source. `None` for a **local-only** repo, whose bare
    /// mirror is synthesised from [`local`](Self::local) and which can't open PRs
    /// until a remote is set.
    pub remote: Option<String>,
    /// An optional existing clone of yours, used **read-only** as a `--reference`
    /// alternate to reuse objects (never written to). For a local-only repo it is
    /// also the seed the synthesised mirror is built from, so it is required there.
    pub local: Option<PathBuf>,
}

impl RepoEntry {
    /// A repo with no `remote` — its mirror is synthesised from [`local`](Self::local)
    /// and PRs/auto-push to a true remote are disabled until `remote` is set.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "substrate API for the teardown / PR-status path (ENG-541); exercised by tests"
        )
    )]
    pub fn is_local_only(&self) -> bool {
        self.remote.is_none()
    }

    /// The source a bare mirror is cloned from: the true `remote` when present,
    /// else the read-only `local` clone (a local-only repo). `None` only for an
    /// entry that has neither — which the loader rejects, so a resolved entry
    /// always yields `Some`.
    pub fn mirror_source(&self) -> Option<String> {
        self.remote.clone().or_else(|| {
            self.local
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
        })
    }
}

/// One declared scratch datastore for a project (ENG-561). lindep runs `provision`
/// at launch and `teardown` at discard, injecting `env` into the agent — staying
/// engine-agnostic (the project owns the commands, exactly as lindep shells `git`
/// rather than embedding it). Placeholders `{issue}/{slug}/{project}/{workspace}/{port}`
/// are substituted into `provision`/`teardown`/`env`; see [`crate::scratch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchSpec {
    /// Identifier within the project (path-safe) — used in the session record + footers.
    pub name: String,
    /// Shell command run at launch to create the resource. Must be idempotent
    /// (resume re-runs it). May print `KEY=VALUE` lines on stdout to inject extra env.
    pub provision: String,
    /// Shell command run at discard/sweep to drop it. May be empty (nothing to undo).
    pub teardown: String,
    /// Environment handed to the agent (values substituted). A sorted map for
    /// deterministic injection order.
    pub env: std::collections::BTreeMap<String, String>,
    /// Mint a free TCP port and expose it as `{port}`.
    pub needs_port: bool,
    /// A provision failure aborts the launch; otherwise it's footered and the agent
    /// still runs (default).
    pub required: bool,
    /// Keep the resource across cancel/resume (a container); else recreate each launch.
    pub persist: bool,
}

/// One Linear project's binding to a set of repos. The `handle` names its isolated
/// on-disk world under `~/.lindep/projects/<handle>/`; `candidates` is the fixed
/// trust boundary the up-front select and the agent lazy-pull both draw from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectDescriptor {
    /// Opaque Linear project id (a UUID) — the stable workspace key, folded into
    /// each session id and used to scope events.
    pub project_id: String,
    /// Path-safe directory name for this project's isolated world. Unique across
    /// the registry (handle uniqueness replaces v1.5's repo-root collision guard).
    pub handle: String,
    /// Human label for the switcher / fleet view.
    pub name: String,
    /// The fixed superset of repo handles this project may materialise — the trust
    /// boundary. Every entry resolves to a known [`RepoEntry`]; `primary` is a
    /// member. Unknown handles are dropped at load with a warning.
    pub candidates: Vec<String>,
    /// The repo handle always materialised at launch (a member of `candidates`).
    pub primary: String,
    /// Optional per-project branch namespace; `None` uses the worktree manager's
    /// compiled-in default (the git user name).
    pub branch_prefix: Option<String>,
    /// Declared scratch datastores (ENG-561), provisioned per issue at launch and
    /// torn down at discard. Empty for a project with no `[[scratch]]` entries.
    pub scratch: Vec<ScratchSpec>,
}

/// The `~/.lindep` on-disk layout. The **single** place the per-project directory
/// name and the 3-layer git paths are defined, so a relocation is a one-line
/// change here rather than scattered `join(".lindep")` calls.
///
/// ```text
/// <root>/                       (~/.lindep, or $LINDEP_HOME)
///   registry.toml
///   mirrors/<repo_handle>.git           L1: shared bare object DB
///   projects/<project_handle>/          one Linear project, its isolated world
///     state.json  cockpit.json  ledger.json  hooks/
///     repos/<repo_handle>/              L2: reference clone (own refs, ~0 objects)
///     worktrees/<ISSUE>/<repo_handle>/  L3: per-issue, per-repo checkout
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    /// A layout rooted at an explicit directory — used by tests with a temp dir.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Layout { root: root.into() }
    }

    /// The real layout: `$LINDEP_HOME` if set (so tests and power users can
    /// relocate the whole tree), else `$HOME/.lindep`. `None` only when neither is
    /// set — a headless environment with no home, where the control plane stays
    /// off rather than scattering state into the filesystem root.
    pub fn from_env() -> Option<Self> {
        if let Some(explicit) = std::env::var_os("LINDEP_HOME") {
            return Some(Layout::new(PathBuf::from(explicit)));
        }
        std::env::var_os("HOME").map(|home| Layout::new(PathBuf::from(home).join(".lindep")))
    }

    /// The `~/.lindep` root.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "scanned by mirror reference-counting / reclaim (ENG-540/541); used by tests"
        )
    )]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `~/.lindep/registry.toml`.
    pub fn registry_path(&self) -> PathBuf {
        self.root.join("registry.toml")
    }

    /// `~/.lindep/mirrors` — the shared bare object DBs (L1).
    pub fn mirrors_dir(&self) -> PathBuf {
        self.root.join("mirrors")
    }

    /// `~/.lindep/mirrors/<repo_handle>.git` — one repo's shared bare mirror (L1).
    pub fn mirror_path(&self, repo_handle: &str) -> PathBuf {
        self.mirrors_dir().join(format!("{repo_handle}.git"))
    }

    /// `~/.lindep/projects` — every project's isolated world.
    pub fn projects_dir(&self) -> PathBuf {
        self.root.join("projects")
    }

    /// `~/.lindep/projects/<project_handle>` — one project's isolated world.
    pub fn project_dir(&self, project_handle: &str) -> PathBuf {
        self.projects_dir().join(project_handle)
    }

    /// `<project>/repos` — this project's reference clones (L2).
    pub fn repos_dir(&self, project_handle: &str) -> PathBuf {
        self.project_dir(project_handle).join("repos")
    }

    /// `<project>/repos/<repo_handle>` — one repo's reference clone (L2). The
    /// [`crate::worktree::WorktreeManager`] is re-rooted here.
    pub fn repo_clone_path(&self, project_handle: &str, repo_handle: &str) -> PathBuf {
        self.repos_dir(project_handle).join(repo_handle)
    }

    /// `<project>/worktrees` — the root every per-issue worktree hangs off (L3).
    pub fn worktrees_dir(&self, project_handle: &str) -> PathBuf {
        self.project_dir(project_handle).join("worktrees")
    }

    /// `<project>/worktrees/<ISSUE>` — the per-issue **workspace** directory (the
    /// agent's cwd for a multi-repo issue), with each repo a sibling subdir.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "the multi-repo agent cwd (ENG-536 up-front repo select); used by tests"
        )
    )]
    pub fn issue_workspace_dir(&self, project_handle: &str, issue: &str) -> PathBuf {
        self.worktrees_dir(project_handle).join(issue)
    }

    /// `<project>/state.json` — this project's session store.
    pub fn state_path(&self, project_handle: &str) -> PathBuf {
        self.project_dir(project_handle).join("state.json")
    }

    /// `<project>/cockpit.json` — this project's window layout.
    pub fn cockpit_path(&self, project_handle: &str) -> PathBuf {
        self.project_dir(project_handle).join("cockpit.json")
    }

    /// `<project>/ledger.json` — this project's per-issue agent run history.
    pub fn ledger_path(&self, project_handle: &str) -> PathBuf {
        self.project_dir(project_handle).join("ledger.json")
    }

    /// `<project>/hooks` — per-issue Claude hook settings files.
    pub fn hooks_dir(&self, project_handle: &str) -> PathBuf {
        self.project_dir(project_handle).join("hooks")
    }
}

/// The resolved registry: every repo keyed by handle, every project keyed by its
/// Linear `project_id`, plus the [`Layout`] its on-disk world lives under.
#[derive(Debug, Clone)]
pub struct Registry {
    layout: Layout,
    repos: HashMap<String, RepoEntry>,
    projects: HashMap<String, ProjectDescriptor>,
}

impl Registry {
    /// Load the registry for the real [`Layout::from_env`], or an empty registry
    /// (rooted at a best-effort home) when no home is configured. Returns the
    /// registry plus any warnings to surface — a bad file never aborts startup.
    pub fn load() -> (Self, Vec<String>) {
        match Layout::from_env() {
            Some(layout) => Self::load_at(layout),
            None => (
                Registry {
                    layout: Layout::new(PathBuf::from(".lindep")),
                    repos: HashMap::new(),
                    projects: HashMap::new(),
                },
                vec!["no HOME/$LINDEP_HOME set; agents disabled".to_string()],
            ),
        }
    }

    /// Load the registry under an explicit [`Layout`] — the test/`load` seam.
    /// Parses `<root>/registry.toml`, validating handles and cross-references,
    /// turning every problem into a warning.
    pub fn load_at(layout: Layout) -> (Self, Vec<String>) {
        let path = layout.registry_path();
        let mut warnings = Vec::new();
        let repos;
        let projects;
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let (r, p) = parse_registry(&text, &path, &mut warnings);
                repos = r;
                projects = p;
            }
            // A missing registry is an empty workspace, not an error — the user
            // hasn't onboarded any repos yet.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                repos = HashMap::new();
                projects = HashMap::new();
            }
            Err(source) => {
                warnings.push(RegistryError::Read { path, source }.to_string());
                repos = HashMap::new();
                projects = HashMap::new();
            }
        }
        (
            Registry {
                layout,
                repos,
                projects,
            },
            warnings,
        )
    }

    /// The layout this registry's on-disk world lives under.
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Resolve a project to its descriptor, or an actionable
    /// [`RegistryError::UnknownProject`] if it isn't registered.
    pub fn project(&self, project_id: &str) -> Result<&ProjectDescriptor, RegistryError> {
        self.projects
            .get(project_id)
            .ok_or_else(|| RegistryError::UnknownProject {
                project_id: project_id.to_string(),
            })
    }

    /// Look up a repo by handle.
    pub fn repo(&self, handle: &str) -> Option<&RepoEntry> {
        self.repos.get(handle)
    }

    /// Every registered project id — the set the startup picker and the in-cockpit
    /// switcher offer (the registered projects, since lindep provisions clones
    /// itself rather than gating on a cwd repo).
    pub fn project_ids(&self) -> Vec<String> {
        self.projects.keys().cloned().collect()
    }

    /// Every registered repo handle — the onboarding wizard offers these for reuse
    /// and dedupes against them so it never writes a second `[[repo]]` for a handle
    /// already present.
    pub fn repo_handles(&self) -> Vec<String> {
        self.repos.keys().cloned().collect()
    }

    /// Every registered project's on-disk `handle` — the onboarding wizard derives a
    /// fresh handle for a new project and must keep it unique across the registry
    /// (each handle is a distinct `~/.lindep/projects/<handle>/` world).
    pub fn project_handles(&self) -> Vec<String> {
        self.projects.values().map(|p| p.handle.clone()).collect()
    }

    /// The resolved [`RepoEntry`]s a project may materialise (its `candidates`),
    /// in declared order. Every handle is known (the loader dropped any that
    /// weren't), so this never returns a dangling reference.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "feeds the up-front repo multi-select (ENG-536); covered by registry tests"
        )
    )]
    pub fn candidate_repos(&self, project_id: &str) -> Vec<RepoEntry> {
        self.project(project_id)
            .map(|p| {
                p.candidates
                    .iter()
                    .filter_map(|h| self.repos.get(h).cloned())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Whether `handle` is a safe single path component / git ref segment: non-empty,
/// ASCII alphanumerics plus `-`/`_`/`.`, no leading `-` or `.`, and not a `git`
/// reserved name. A handle becomes a directory under `~/.lindep` and (for repos) a
/// `<handle>.git` mirror, so a `/`, `..`, or control char must never reach a path.
/// Mirrors [`crate::worktree::validate_issue_id`]'s discipline, with `.` allowed
/// (real repo names like `dotfiles.git` use it) but never leading.
pub fn is_safe_handle(handle: &str) -> bool {
    if handle.is_empty()
        || handle.starts_with('-')
        || handle.starts_with('.')
        || handle == "."
        || handle == ".."
    {
        return false;
    }
    !handle.contains("..")
        && handle
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// Derive a [`is_safe_handle`]-safe repo handle from a remote URL or local path:
/// the last path segment, with any `.git` suffix dropped and every byte that isn't
/// handle-safe collapsed to `-`. `"core"` for `git@github.com:org/core.git`,
/// `/home/me/core/`, or `https://host/org/core`. May return an empty string for a
/// degenerate input (the caller validates with [`is_safe_handle`] and asks again).
pub fn handle_from_source(source: &str) -> String {
    let trimmed = source.trim().trim_end_matches('/');
    // The last segment after a `/` or `:` — the `:` also splits an scp-style
    // `git@host:org/repo` remote when there is no `/`.
    let last = trimmed.rsplit(['/', ':']).next().unwrap_or(trimmed);
    let stem = last.strip_suffix(".git").unwrap_or(last);
    let mapped: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    // A leading `-`/`.` is rejected by `is_safe_handle`, so trim any.
    mapped.trim_start_matches(['-', '.']).to_string()
}

/// A repo the onboarding wizard will add to the registry — the resolved `[[repo]]`
/// fields. `remote`/`local` mirror [`RepoEntry`]; at least one must be present (the
/// wizard guarantees it), and `local` is stored as the raw string the user gave so
/// a `~` still expands at load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoDraft {
    pub handle: String,
    pub remote: Option<String>,
    pub local: Option<String>,
}

/// A project binding the onboarding wizard will write — the `[[project]]` fields.
/// Scratch datastores ride alongside as [`ScratchDraft`]s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectDraft {
    pub id: String,
    pub handle: String,
    pub name: String,
    pub candidates: Vec<String>,
    pub primary: String,
    pub branch_prefix: Option<String>,
}

/// A scratch datastore the wizard will write as a nested `[[project.scratch]]` — the
/// on-disk shape of [`ScratchSpec`] (the wizard collects it; the loader resolves it).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScratchDraft {
    pub name: String,
    pub provision: String,
    pub teardown: String,
    pub env: std::collections::BTreeMap<String, String>,
    pub needs_port: bool,
    pub required: bool,
    pub persist: bool,
}

/// Write a project binding into `registry.toml`, **preserving the file's existing
/// comments and ordering** via format-preserving `toml_edit` (the `toml` crate's
/// serializer can't round-trip those):
///
/// * each repo in `new_repos` is appended as a fresh `[[repo]]` (the wizard passes
///   only genuinely-new handles, so an already-registered repo is left untouched);
/// * the `[[project]]` whose `id` matches is **updated in place** — so re-running the
///   wizard edits the binding instead of duplicating it — or appended when new, with
///   any `scratch` rendered as nested `[[project.scratch]]`.
///
/// The first block this call appends is tagged with a provenance comment; an updated
/// project keeps its own leading comment. The write is atomic (tmp → rename), like the
/// rest of lindep's on-disk state.
pub fn write_binding(
    layout: &Layout,
    new_repos: &[RepoDraft],
    project: &ProjectDraft,
    scratch: &[ScratchDraft],
) -> Result<(), RegistryError> {
    use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, value};

    let path = layout.registry_path();
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => return Err(RegistryError::Write { path, source }),
    };
    let mut doc = existing
        .parse::<DocumentMut>()
        .map_err(|source| RegistryError::EditParse {
            path: path.clone(),
            source: Box::new(source),
        })?;

    // The first block we append gets a provenance marker; subsequent ones just a
    // blank-line separator. (`first_new` is threaded so the marker lands once.)
    let mut first_new = true;
    fn mark(t: &mut Table, first_new: &mut bool) {
        t.decor_mut().set_prefix(if *first_new {
            "\n# ── added by lindep ──\n"
        } else {
            "\n"
        });
        *first_new = false;
    }

    let not_aot = |path: &Path, key: &'static str| RegistryError::Write {
        path: path.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("`{key}` in registry.toml is not an array of tables"),
        ),
    };

    // Append the genuinely-new repos.
    if !new_repos.is_empty() {
        let item = doc
            .entry("repo")
            .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
        let repos = item
            .as_array_of_tables_mut()
            .ok_or_else(|| not_aot(&path, "repo"))?;
        for d in new_repos {
            let mut t = Table::new();
            t["handle"] = value(d.handle.clone());
            if let Some(r) = &d.remote {
                t["remote"] = value(r.clone());
            }
            if let Some(l) = &d.local {
                t["local"] = value(l.clone());
            }
            mark(&mut t, &mut first_new);
            repos.push(t);
        }
    }

    // Build the `[[project]]` table. `candidates` is omitted when it's just the
    // primary (the loader always folds `primary` in), keeping a single-repo block tidy.
    let candidates = if project.candidates.iter().all(|c| c == &project.primary) {
        Vec::new()
    } else {
        project.candidates.clone()
    };
    let mut ptable = Table::new();
    ptable["id"] = value(project.id.clone());
    ptable["handle"] = value(project.handle.clone());
    if !project.name.is_empty() {
        ptable["name"] = value(project.name.clone());
    }
    if !candidates.is_empty() {
        ptable["candidates"] = value(candidates.into_iter().collect::<Array>());
    }
    ptable["primary"] = value(project.primary.clone());
    if let Some(bp) = &project.branch_prefix {
        ptable["branch_prefix"] = value(bp.clone());
    }
    if !scratch.is_empty() {
        let mut aot = ArrayOfTables::new();
        for s in scratch {
            let mut st = Table::new();
            st["name"] = value(s.name.clone());
            st["provision"] = value(s.provision.clone());
            if !s.teardown.is_empty() {
                st["teardown"] = value(s.teardown.clone());
            }
            if !s.env.is_empty() {
                let mut env = toml_edit::InlineTable::new();
                for (k, v) in &s.env {
                    env.insert(k.as_str(), v.clone().into());
                }
                st["env"] = value(env);
            }
            if s.needs_port {
                st["needs_port"] = value(true);
            }
            if s.required {
                st["required"] = value(true);
            }
            if s.persist {
                st["persist"] = value(true);
            }
            aot.push(st);
        }
        ptable["scratch"] = Item::ArrayOfTables(aot);
    }

    // Upsert by `id`: replace the matching project in place (keeping the comment that
    // sits above it), else append a new one.
    let item = doc
        .entry("project")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    let projects = item
        .as_array_of_tables_mut()
        .ok_or_else(|| not_aot(&path, "project"))?;
    let existing_idx = projects
        .iter()
        .position(|t| t.get("id").and_then(|v| v.as_str()) == Some(project.id.as_str()));
    match existing_idx.and_then(|i| projects.get_mut(i)) {
        Some(slot) => {
            let prefix = slot
                .decor()
                .prefix()
                .and_then(|p| p.as_str())
                .map(str::to_string);
            *slot = ptable;
            if let Some(p) = prefix {
                slot.decor_mut().set_prefix(p);
            }
        }
        None => {
            mark(&mut ptable, &mut first_new);
            projects.push(ptable);
        }
    }

    let out = doc.to_string();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| RegistryError::Write {
            path: path.clone(),
            source,
        })?;
    }
    let mut tmp = path.clone();
    tmp.set_file_name("registry.toml.tmp");
    std::fs::write(&tmp, &out).map_err(|source| RegistryError::Write {
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, &path).map_err(|source| RegistryError::Write { path, source })
}

/// Parse the registry document: repos first (so projects can cross-reference
/// them), then projects with full validation. Every problem is pushed as a warning
/// and the offending entry skipped; the rest still load.
fn parse_registry(
    text: &str,
    path: &Path,
    warnings: &mut Vec<String>,
) -> (
    HashMap<String, RepoEntry>,
    HashMap<String, ProjectDescriptor>,
) {
    let mut repos: HashMap<String, RepoEntry> = HashMap::new();
    let mut projects: HashMap<String, ProjectDescriptor> = HashMap::new();

    let doc = match text.parse::<toml::Table>() {
        Ok(doc) => doc,
        Err(source) => {
            warnings.push(
                RegistryError::Parse {
                    path: path.to_path_buf(),
                    source,
                }
                .to_string(),
            );
            return (repos, projects);
        }
    };

    // ── [[repo]] ──────────────────────────────────────────────────────────────
    for (index, raw) in array_of(&doc, "repo", path, warnings)
        .into_iter()
        .enumerate()
    {
        match raw.clone().try_into::<RepoFile>() {
            Ok(entry) => match entry.into_repo() {
                Ok(repo) => {
                    if !is_safe_handle(&repo.handle) {
                        warnings.push(format!(
                            "registry {}: repo handle `{}` is not a safe name; skipping",
                            path.display(),
                            repo.handle
                        ));
                        continue;
                    }
                    repos.insert(repo.handle.clone(), repo);
                }
                Err(msg) => warnings.push(format!(
                    "registry {}: [[repo]] entry #{index}: {msg}",
                    path.display()
                )),
            },
            Err(source) => warnings.push(
                RegistryError::ParseEntry {
                    path: path.to_path_buf(),
                    table: "repo",
                    index,
                    source: Box::new(source),
                }
                .to_string(),
            ),
        }
    }

    // ── [[project]] ───────────────────────────────────────────────────────────
    for (index, raw) in array_of(&doc, "project", path, warnings)
        .into_iter()
        .enumerate()
    {
        let entry = match raw.clone().try_into::<ProjectFile>() {
            Ok(entry) => entry,
            Err(source) => {
                warnings.push(
                    RegistryError::ParseEntry {
                        path: path.to_path_buf(),
                        table: "project",
                        index,
                        source: Box::new(source),
                    }
                    .to_string(),
                );
                continue;
            }
        };
        match entry.into_descriptor(&repos, path, warnings) {
            Some(p) => {
                projects.insert(p.project_id.clone(), p);
            }
            None => continue, // a warning was already pushed
        }
    }

    (repos, projects)
}

/// Pull `doc["<key>"]` as an array of tables, warning (and yielding empty) if the
/// key is present but isn't an array — so a typo'd `[project]` (single table)
/// surfaces instead of silently contributing nothing.
fn array_of<'a>(
    doc: &'a toml::Table,
    key: &'static str,
    path: &Path,
    warnings: &mut Vec<String>,
) -> Vec<&'a toml::Value> {
    match doc.get(key) {
        None => Vec::new(),
        Some(toml::Value::Array(entries)) => entries.iter().collect(),
        Some(_) => {
            warnings.push(format!(
                "registry {}: `{key}` must be an array of [[{key}]] tables",
                path.display()
            ));
            Vec::new()
        }
    }
}

/// On-disk shape of a `[[repo]]` table (paths as raw strings so `~` can expand).
#[derive(Debug, Deserialize, Serialize)]
struct RepoFile {
    handle: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    remote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    local: Option<String>,
}

impl RepoFile {
    fn into_repo(self) -> Result<RepoEntry, String> {
        let remote = self.remote.filter(|s| !s.trim().is_empty());
        let local = self
            .local
            .filter(|s| !s.trim().is_empty())
            .map(|s| expand_tilde(&s));
        if remote.is_none() && local.is_none() {
            return Err(format!(
                "repo `{}` has neither `remote` nor `local`",
                self.handle
            ));
        }
        Ok(RepoEntry {
            handle: self.handle,
            remote,
            local,
        })
    }
}

/// On-disk shape of a `[[project]]` table.
#[derive(Debug, Deserialize, Serialize)]
struct ProjectFile {
    id: String,
    handle: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    candidates: Vec<String>,
    primary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    branch_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    scratch: Vec<ScratchFile>,
}

/// On-disk shape of a `[[project.scratch]]` table (ENG-561).
#[derive(Debug, Deserialize, Serialize)]
struct ScratchFile {
    name: String,
    #[serde(default)]
    provision: String,
    #[serde(default)]
    teardown: String,
    #[serde(default)]
    env: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    needs_port: bool,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    persist: bool,
}

impl ProjectFile {
    /// Validate and resolve a project against the known repos. Returns `None` (with
    /// a pushed warning) for an unsafe handle, an unknown `primary`, or no usable
    /// candidate — anything that would corrupt the on-disk layout.
    fn into_descriptor(
        self,
        repos: &HashMap<String, RepoEntry>,
        path: &Path,
        warnings: &mut Vec<String>,
    ) -> Option<ProjectDescriptor> {
        let here = || path.display();
        if !is_safe_handle(&self.handle) {
            warnings.push(format!(
                "registry {}: project `{}` handle `{}` is not a safe directory name; skipping",
                here(),
                self.id,
                self.handle
            ));
            return None;
        }
        // Resolve candidates against known repos; drop unknowns with a warning.
        // The primary is always considered a candidate even if omitted from the list.
        let mut candidates: Vec<String> = Vec::new();
        for handle in self.candidates.iter().chain(std::iter::once(&self.primary)) {
            if !repos.contains_key(handle) {
                warnings.push(format!(
                    "registry {}: project `{}` references unknown repo `{handle}`; dropping it",
                    here(),
                    self.id
                ));
                continue;
            }
            if !candidates.contains(handle) {
                candidates.push(handle.clone());
            }
        }
        // The primary must resolve, or the project can't be launched at all.
        if !repos.contains_key(&self.primary) {
            warnings.push(format!(
                "registry {}: project `{}` primary repo `{}` is unknown; skipping the project",
                here(),
                self.id,
                self.primary
            ));
            return None;
        }
        // Resolve scratch specs (ENG-561): a malformed one (missing/unsafe name, or
        // no provision command) is dropped with a warning rather than failing the
        // whole project — the same warn-never-abort discipline as the rest of the loader.
        let mut scratch: Vec<ScratchSpec> = Vec::new();
        for sf in self.scratch {
            if sf.name.trim().is_empty() || !is_safe_handle(&sf.name) {
                warnings.push(format!(
                    "registry {}: project `{}` has a [[scratch]] with a missing or unsafe `name`; skipping it",
                    here(),
                    self.id
                ));
                continue;
            }
            if sf.provision.trim().is_empty() {
                warnings.push(format!(
                    "registry {}: project `{}` scratch `{}` has no `provision` command; skipping it",
                    here(),
                    self.id,
                    sf.name
                ));
                continue;
            }
            scratch.push(ScratchSpec {
                name: sf.name,
                provision: sf.provision,
                teardown: sf.teardown,
                env: sf.env,
                needs_port: sf.needs_port,
                required: sf.required,
                persist: sf.persist,
            });
        }
        Some(ProjectDescriptor {
            project_id: self.id,
            handle: self.handle,
            name: self.name,
            candidates,
            primary: self.primary,
            branch_prefix: self.branch_prefix.filter(|s| !s.trim().is_empty()),
            scratch,
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_root(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("lindep-reg-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_registry(root: &Path, body: &str) -> Layout {
        std::fs::write(root.join("registry.toml"), body).unwrap();
        Layout::new(root)
    }

    #[test]
    fn parses_repos_and_projects_keyed_by_handle_and_id() {
        let root = temp_root("ok");
        let layout = write_registry(
            &root,
            r#"
            [[repo]]
            handle = "lindep"
            remote = "git@github.com:zaplar/lindep"
            local  = "/home/felix/code/lindep"

            [[repo]]
            handle = "shared-proto"
            remote = "git@github.com:zaplar/shared-proto"

            [[project]]
            id = "p1"
            handle = "lindep-core"
            name = "Lindep Core"
            candidates = ["lindep", "shared-proto"]
            primary = "lindep"
            branch_prefix = "felix"
            "#,
        );
        let (reg, warnings) = Registry::load_at(layout);
        assert!(warnings.is_empty(), "{warnings:?}");

        let repo = reg.repo("lindep").unwrap();
        assert_eq!(repo.remote.as_deref(), Some("git@github.com:zaplar/lindep"));
        assert_eq!(repo.local, Some(PathBuf::from("/home/felix/code/lindep")));
        assert!(!repo.is_local_only());

        let proj = reg.project("p1").unwrap();
        assert_eq!(proj.handle, "lindep-core");
        assert_eq!(proj.primary, "lindep");
        assert_eq!(proj.candidates, vec!["lindep", "shared-proto"]);
        assert_eq!(proj.branch_prefix.as_deref(), Some("felix"));
        assert_eq!(reg.candidate_repos("p1").len(), 2);
    }

    #[test]
    fn parses_a_project_scratch_spec_and_drops_malformed_ones() {
        let root = temp_root("scratch");
        let layout = write_registry(
            &root,
            r#"
            [[repo]]
            handle = "api"
            local = "/tmp/api"

            [[project]]
            id = "p1"
            handle = "proj"
            primary = "api"

              [[project.scratch]]
              name = "db"
              provision = "createdb scratch_{slug}"
              teardown = "dropdb scratch_{slug}"
              env = { DATABASE_URL = "postgres:///scratch_{slug}" }
              required = true

              [[project.scratch]]
              name = "noprov"

              [[project.scratch]]
              name = "../evil"
              provision = "x"
            "#,
        );
        let (reg, warnings) = Registry::load_at(layout);
        let proj = reg.project("p1").unwrap();
        assert_eq!(proj.scratch.len(), 1, "only the valid scratch resolves");
        let db = &proj.scratch[0];
        assert_eq!(db.name, "db");
        assert_eq!(db.provision, "createdb scratch_{slug}");
        assert!(db.required);
        assert!(!db.needs_port);
        assert_eq!(
            db.env.get("DATABASE_URL").map(String::as_str),
            Some("postgres:///scratch_{slug}")
        );
        // The provision-less and unsafe-name entries are each dropped with a warning.
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }

    #[test]
    fn the_primary_is_always_a_candidate_even_if_omitted_from_the_list() {
        let root = temp_root("primary");
        let layout = write_registry(
            &root,
            r#"
            [[repo]]
            handle = "api"
            remote = "git@github.com:zaplar/api"

            [[project]]
            id = "p1"
            handle = "proj"
            primary = "api"
            "#,
        );
        let (reg, warnings) = Registry::load_at(layout);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(reg.project("p1").unwrap().candidates, vec!["api"]);
    }

    #[test]
    fn a_local_only_repo_has_no_remote_and_mirrors_from_local() {
        let root = temp_root("local");
        let layout = write_registry(
            &root,
            r#"
            [[repo]]
            handle = "scratch"
            local  = "/home/felix/scratch"

            [[project]]
            id = "p1"
            handle = "proj"
            primary = "scratch"
            "#,
        );
        let (reg, warnings) = Registry::load_at(layout);
        assert!(warnings.is_empty(), "{warnings:?}");
        let repo = reg.repo("scratch").unwrap();
        assert!(repo.is_local_only());
        assert_eq!(repo.mirror_source().as_deref(), Some("/home/felix/scratch"));
    }

    #[test]
    fn a_repo_with_neither_remote_nor_local_is_skipped() {
        let root = temp_root("empty-repo");
        let layout = write_registry(
            &root,
            r#"
            [[repo]]
            handle = "broken"
            "#,
        );
        let (reg, warnings) = Registry::load_at(layout);
        assert!(reg.repo("broken").is_none());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("neither"));
    }

    #[test]
    fn a_project_with_an_unknown_primary_is_skipped() {
        let root = temp_root("bad-primary");
        let layout = write_registry(
            &root,
            r#"
            [[repo]]
            handle = "api"
            remote = "git@github.com:zaplar/api"

            [[project]]
            id = "p1"
            handle = "proj"
            primary = "missing"
            "#,
        );
        let (reg, warnings) = Registry::load_at(layout);
        assert!(reg.project("p1").is_err());
        assert!(warnings.iter().any(|w| w.contains("primary repo")));
    }

    #[test]
    fn an_unknown_candidate_is_dropped_but_the_project_survives() {
        let root = temp_root("bad-cand");
        let layout = write_registry(
            &root,
            r#"
            [[repo]]
            handle = "api"
            remote = "git@github.com:zaplar/api"

            [[project]]
            id = "p1"
            handle = "proj"
            candidates = ["api", "ghost"]
            primary = "api"
            "#,
        );
        let (reg, warnings) = Registry::load_at(layout);
        let proj = reg.project("p1").unwrap();
        assert_eq!(proj.candidates, vec!["api"], "the ghost candidate is gone");
        assert!(warnings.iter().any(|w| w.contains("ghost")));
    }

    #[test]
    fn an_unsafe_project_handle_is_skipped() {
        let root = temp_root("unsafe");
        let layout = write_registry(
            &root,
            r#"
            [[repo]]
            handle = "api"
            remote = "git@github.com:zaplar/api"

            [[project]]
            id = "p1"
            handle = "../escape"
            primary = "api"
            "#,
        );
        let (reg, warnings) = Registry::load_at(layout);
        assert!(reg.project("p1").is_err());
        assert!(warnings.iter().any(|w| w.contains("safe directory name")));
    }

    #[test]
    fn a_missing_registry_is_an_empty_workspace_with_no_warning() {
        let root = temp_root("missing");
        // No registry.toml written.
        let (reg, warnings) = Registry::load_at(Layout::new(&root));
        assert!(warnings.is_empty());
        assert!(reg.project_ids().is_empty());
        assert!(reg.project("anything").is_err());
    }

    #[test]
    fn malformed_toml_warns_and_yields_an_empty_registry() {
        let root = temp_root("malformed");
        let layout = write_registry(&root, "this is = = not valid [[[");
        let (reg, warnings) = Registry::load_at(layout);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("invalid TOML"));
        assert!(reg.project_ids().is_empty());
    }

    #[test]
    fn resolve_of_an_unknown_project_is_actionable() {
        let (reg, _) = Registry::load_at(Layout::new(temp_root("unknown")));
        let err = reg.project("nope").unwrap_err();
        assert!(matches!(err, RegistryError::UnknownProject { .. }));
        assert!(err.to_string().contains("registry.toml"));
    }

    #[test]
    fn is_safe_handle_accepts_real_names_and_rejects_path_tricks() {
        for ok in ["lindep", "shared-proto", "api_2", "dotfiles.git", "a"] {
            assert!(is_safe_handle(ok), "{ok:?} should be accepted");
        }
        for bad in [
            "",
            "-x",
            ".hidden",
            "..",
            "a/b",
            "../escape",
            "a..b",
            "x y",
            "x\\y",
        ] {
            assert!(!is_safe_handle(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn the_layout_centralizes_the_three_git_layers() {
        let layout = Layout::new("/root");
        assert_eq!(layout.registry_path(), PathBuf::from("/root/registry.toml"));
        assert_eq!(
            layout.mirror_path("lindep"),
            PathBuf::from("/root/mirrors/lindep.git")
        );
        assert_eq!(
            layout.repo_clone_path("lindep-core", "lindep"),
            PathBuf::from("/root/projects/lindep-core/repos/lindep")
        );
        assert_eq!(
            layout.issue_workspace_dir("lindep-core", "ENG-1"),
            PathBuf::from("/root/projects/lindep-core/worktrees/ENG-1")
        );
        assert_eq!(
            layout.state_path("lindep-core"),
            PathBuf::from("/root/projects/lindep-core/state.json")
        );
        assert_eq!(
            layout.hooks_dir("lindep-core"),
            PathBuf::from("/root/projects/lindep-core/hooks")
        );
    }

    #[test]
    fn from_env_prefers_lindep_home() {
        // Drive the explicit override without mutating a shared HOME assumption.
        // SAFETY: single-threaded test; we set then restore the override.
        let prev = std::env::var_os("LINDEP_HOME");
        unsafe { std::env::set_var("LINDEP_HOME", "/tmp/lindep-home-test") };
        let layout = Layout::from_env().unwrap();
        assert_eq!(layout.root(), Path::new("/tmp/lindep-home-test"));
        match prev {
            Some(v) => unsafe { std::env::set_var("LINDEP_HOME", v) },
            None => unsafe { std::env::remove_var("LINDEP_HOME") },
        }
    }

    #[test]
    fn handle_from_source_strips_path_git_suffix_and_unsafe_chars() {
        assert_eq!(
            handle_from_source("git@github.com:zaplar/core-pms.git"),
            "core-pms"
        );
        assert_eq!(
            handle_from_source("https://github.com/zaplar/core-pms"),
            "core-pms"
        );
        assert_eq!(handle_from_source("/home/felix/code/lindep/"), "lindep");
        assert_eq!(handle_from_source("dotfiles.git"), "dotfiles");
        // A space (and other unsafe bytes) collapse to '-'; a leading dash is trimmed.
        assert_eq!(handle_from_source("My Repo"), "My-Repo");
        // The result is always handle-safe (or empty, which the wizard re-prompts on).
        assert!(is_safe_handle(&handle_from_source(
            "git@github.com:org/api_2.git"
        )));
    }

    #[test]
    fn write_binding_writes_a_loadable_repo_and_project() {
        let root = temp_root("append-new");
        let layout = Layout::new(&root);
        let repo = RepoDraft {
            handle: "core-pms".into(),
            remote: Some("git@github.com:zaplar/core-pms".into()),
            local: Some("/home/felix/code/core-pms".into()),
        };
        let project = ProjectDraft {
            id: "p-uuid".into(),
            handle: "core-pms".into(),
            name: "Core PMS".into(),
            candidates: vec!["core-pms".into()],
            primary: "core-pms".into(),
            branch_prefix: None,
        };
        write_binding(&layout, std::slice::from_ref(&repo), &project, &[]).unwrap();

        let (reg, warnings) = Registry::load_at(Layout::new(&root));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            reg.repo("core-pms").unwrap().remote.as_deref(),
            Some("git@github.com:zaplar/core-pms")
        );
        let proj = reg.project("p-uuid").unwrap();
        assert_eq!(proj.primary, "core-pms");
        assert_eq!(proj.candidates, vec!["core-pms"]);
    }

    #[test]
    fn write_binding_preserves_existing_comments_and_repos() {
        let root = temp_root("append-keep");
        // A registry the user hand-wrote, with a comment and one repo already present.
        let layout = write_registry(
            &root,
            "# my notes — keep me\n[[repo]]\nhandle = \"lindep\"\nremote = \"git@github.com:zaplar/lindep\"\n",
        );
        // Reuse the existing `lindep` repo (no new [[repo]]) and add a project.
        let project = ProjectDraft {
            id: "p2".into(),
            handle: "lindep-core".into(),
            name: "Lindep Core".into(),
            candidates: vec!["lindep".into()],
            primary: "lindep".into(),
            branch_prefix: Some("felix".into()),
        };
        write_binding(&layout, &[], &project, &[]).unwrap();

        let text = std::fs::read_to_string(root.join("registry.toml")).unwrap();
        assert!(
            text.contains("# my notes — keep me"),
            "the comment survives: {text}"
        );

        let (reg, warnings) = Registry::load_at(Layout::new(&root));
        assert!(warnings.is_empty(), "{warnings:?}");
        // Both the pre-existing repo and the appended project resolve.
        assert!(reg.repo("lindep").is_some());
        let proj = reg.project("p2").unwrap();
        assert_eq!(proj.branch_prefix.as_deref(), Some("felix"));
    }

    #[test]
    fn write_binding_updates_an_existing_project_in_place() {
        let root = temp_root("upsert");
        let layout = write_registry(
            &root,
            "[[repo]]\nhandle = \"core\"\nremote = \"git@github.com:zaplar/core\"\n\n\
             [[project]]\nid = \"p3\"\nhandle = \"core\"\nname = \"Core\"\nprimary = \"core\"\n",
        );
        // Re-run the wizard on the same project id with a branch prefix added.
        let project = ProjectDraft {
            id: "p3".into(),
            handle: "core".into(),
            name: "Core".into(),
            candidates: vec!["core".into()],
            primary: "core".into(),
            branch_prefix: Some("felix".into()),
        };
        write_binding(&layout, &[], &project, &[]).unwrap();

        let text = std::fs::read_to_string(root.join("registry.toml")).unwrap();
        // Edited in place, not duplicated: exactly one [[project]] header remains.
        assert_eq!(
            text.matches("[[project]]").count(),
            1,
            "no duplicate project: {text}"
        );

        let (reg, warnings) = Registry::load_at(Layout::new(&root));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            reg.project("p3").unwrap().branch_prefix.as_deref(),
            Some("felix")
        );
    }

    #[test]
    fn write_binding_renders_nested_scratch_tables() {
        let root = temp_root("scratch-write");
        let layout = Layout::new(&root);
        let repo = RepoDraft {
            handle: "api".into(),
            remote: Some("git@github.com:zaplar/api".into()),
            local: None,
        };
        let project = ProjectDraft {
            id: "p4".into(),
            handle: "api".into(),
            name: "API".into(),
            candidates: vec!["api".into()],
            primary: "api".into(),
            branch_prefix: None,
        };
        let scratch = ScratchDraft {
            name: "db".into(),
            provision: "createdb scratch_{slug}".into(),
            teardown: "dropdb scratch_{slug}".into(),
            env: std::collections::BTreeMap::from([(
                "DATABASE_URL".to_string(),
                "postgres:///scratch_{slug}".to_string(),
            )]),
            needs_port: false,
            required: true,
            persist: false,
        };
        write_binding(
            &layout,
            std::slice::from_ref(&repo),
            &project,
            std::slice::from_ref(&scratch),
        )
        .unwrap();

        let (reg, warnings) = Registry::load_at(Layout::new(&root));
        assert!(warnings.is_empty(), "{warnings:?}");
        let specs = &reg.project("p4").unwrap().scratch;
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "db");
        assert!(specs[0].required);
        assert_eq!(
            specs[0].env.get("DATABASE_URL").map(String::as_str),
            Some("postgres:///scratch_{slug}")
        );
    }
}
