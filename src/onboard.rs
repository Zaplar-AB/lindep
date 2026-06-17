//! First-run onboarding wizard — shown when you open a Linear project that isn't
//! in `~/.lindep/registry.toml` yet. lindep already knows the project (its id and
//! name come from Linear); the one thing it can't infer is **which git repos the
//! project uses**, so the wizard collects exactly that and appends a `[[project]]`
//! (plus any new `[[repo]]`) to the registry, preserving the file's existing
//! comments. Everything else is derived: the on-disk `handle` from the project name,
//! `branch_prefix` defaults to the git user name when skipped, scratch datastores
//! stay hand-authored.
//!
//! It runs **before** the cockpit's alternate screen, the same way [`crate::picker`]
//! does — `run` owns its own `ratatui::init`/`restore`. The wizard's pure state
//! transitions ([`Wizard`]) are unit-tested; the git/disk probing in [`Wizard::resolve`]
//! is the only impure part.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, HighlightSpacing, List, ListItem, ListState, Paragraph};

use crate::linear::ProjectRef;
use crate::registry::{self, ProjectDraft, RepoDraft, ScratchDraft};
use crate::theme::{self, *};
use crate::window::move_state;

/// Run the onboarding wizard for an unregistered `project`. Returns `Ok(true)` when
/// the user completed it and a binding was written (the caller then reloads the
/// registry), or `Ok(false)` when they cancelled (the caller degrades to the
/// read-only graph viewer, exactly as an unregistered project did before). Manages
/// its own alternate screen; the caller restores nothing.
pub fn run(project: &ProjectRef, registry: &registry::Registry) -> io::Result<bool> {
    let mut wizard = Wizard::new(
        project.clone(),
        registry.layout().clone(),
        registry.repo_handles(),
        registry.project_handles(),
    );
    let mut terminal = ratatui::init();
    let result = run_loop(&mut terminal, &mut wizard);
    ratatui::restore();
    result
}

/// Re-enter the wizard for `project` from inside the cockpit (the `configure-project`
/// verb). Reloads the registry fresh, pre-populates from the project's existing
/// binding when there is one (so this *edits* rather than restarts), writes the
/// result, and returns a one-line footer. The change lands in `registry.toml` and
/// applies on the next launch — the running workspace keeps its current binding for
/// this session (re-rooting a live worktree manager mid-flight isn't safe), so the
/// footer says so. Owns its own alternate screen; the caller suspends the cockpit's.
pub fn run_for_project(project: &ProjectRef) -> io::Result<String> {
    let (registry, _warnings) = registry::Registry::load();
    let mut wizard = Wizard::for_project(project.clone(), &registry);
    let mut terminal = ratatui::init();
    let result = run_loop(&mut terminal, &mut wizard);
    ratatui::restore();
    Ok(if result? {
        "saved to ~/.lindep/registry.toml — restart lindep to apply".to_string()
    } else {
        "configuration unchanged".to_string()
    })
}

/// The wizard's steps, in order. [`Step::Primary`] is skipped when the project has a
/// single repo (it is the primary by definition).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Step {
    /// Add repos by path / URL / existing handle (at least one required).
    Repos,
    /// Pick the primary repo (always materialised at launch) among several.
    Primary,
    /// Optional per-project branch namespace — skippable (Enter on empty = default).
    BranchPrefix,
    /// Optional per-issue scratch datastores — skippable (Enter on empty = none).
    Scratch,
    /// Review the binding and write it.
    Confirm,
}

/// One repo chosen for the project being onboarded.
struct RepoRow {
    handle: String,
    /// The `[[repo]]` block to write, or `None` when `handle` is already registered
    /// (reuse — it becomes a candidate but no second block is written).
    draft: Option<RepoDraft>,
    /// A short human label for the list (the remote, the path, or a note).
    label: String,
}

/// A scratch-datastore entry being filled in on [`Step::Scratch`]. Seven navigable
/// fields: four text (`name`, `provision`, `teardown`, `env`) then three flags.
#[derive(Default)]
struct ScratchForm {
    name: String,
    provision: String,
    teardown: String,
    /// Space-separated `KEY=VALUE` pairs, parsed into the env map on add.
    env: String,
    needs_port: bool,
    required: bool,
    persist: bool,
    /// Which field has focus: `0..=3` text, `4..=6` flags.
    focus: usize,
}

impl ScratchForm {
    const FIELDS: usize = 7;

    fn move_focus(&mut self, delta: i32) {
        let n = Self::FIELDS as i32;
        self.focus = (((self.focus as i32 + delta) % n + n) % n) as usize;
    }

    /// Type into the focused text field, or toggle the focused flag on a space.
    fn type_char(&mut self, c: char) {
        match self.focus {
            0 => self.name.push(c),
            1 => self.provision.push(c),
            2 => self.teardown.push(c),
            3 => self.env.push(c),
            4 if c == ' ' => self.needs_port = !self.needs_port,
            5 if c == ' ' => self.required = !self.required,
            6 if c == ' ' => self.persist = !self.persist,
            _ => {}
        }
    }

    fn backspace(&mut self) {
        match self.focus {
            0 => self.name.pop(),
            1 => self.provision.pop(),
            2 => self.teardown.pop(),
            3 => self.env.pop(),
            _ => None,
        };
    }

    /// Validate and resolve into a [`ScratchDraft`] (`name` safe, `provision` set, env
    /// parses), or an error message for the footer.
    fn to_draft(&self) -> Result<ScratchDraft, String> {
        let name = self.name.trim();
        if !registry::is_safe_handle(name) {
            return Err("scratch name must be a safe handle (letters, digits, - _ .)".into());
        }
        if self.provision.trim().is_empty() {
            return Err("scratch needs a provision command".into());
        }
        let mut env = std::collections::BTreeMap::new();
        for tok in self.env.split_whitespace() {
            match tok.split_once('=') {
                Some((k, v)) if !k.is_empty() => {
                    env.insert(k.to_string(), v.to_string());
                }
                _ => return Err(format!("env must be KEY=VALUE pairs (got '{tok}')")),
            }
        }
        Ok(ScratchDraft {
            name: name.to_string(),
            provision: self.provision.trim().to_string(),
            teardown: self.teardown.trim().to_string(),
            env,
            needs_port: self.needs_port,
            required: self.required,
            persist: self.persist,
        })
    }
}

/// The onboarding wizard state.
pub struct Wizard {
    project: ProjectRef,
    /// Where to write — the registry's on-disk layout.
    layout: registry::Layout,
    /// Repo handles already in the registry, for reuse + dedup (never written twice).
    registered_repos: Vec<String>,
    /// Project handles already in the registry, so a new one stays unique.
    taken_handles: Vec<String>,
    /// The project's existing on-disk handle when editing — kept verbatim rather than
    /// re-derived (renaming a handle would orphan its `~/.lindep/projects/<handle>/`).
    fixed_handle: Option<String>,

    step: Step,
    /// The repos chosen for this project (in entry order).
    repos: Vec<RepoRow>,
    /// The live text input (the repo field on [`Step::Repos`], the prefix on
    /// [`Step::BranchPrefix`]).
    input: String,
    /// Cursor over [`Self::repos`] on the primary-select step.
    primary: ListState,
    branch_prefix: String,
    /// Scratch datastores collected so far, and the in-progress entry.
    scratch: Vec<ScratchDraft>,
    scratch_form: ScratchForm,
    /// A transient message shown under the body (a bad input, a write failure).
    error: Option<String>,
}

impl Wizard {
    fn new(
        project: ProjectRef,
        layout: registry::Layout,
        registered_repos: Vec<String>,
        taken_handles: Vec<String>,
    ) -> Self {
        Wizard {
            project,
            layout,
            registered_repos,
            taken_handles,
            fixed_handle: None,
            step: Step::Repos,
            repos: Vec::new(),
            input: String::new(),
            primary: ListState::default(),
            branch_prefix: String::new(),
            scratch: Vec::new(),
            scratch_form: ScratchForm::default(),
            error: None,
        }
    }

    /// Build a wizard for re-configuring `project`, pre-populated from its existing
    /// registry binding when there is one (so the cockpit re-entry edits in place).
    /// With no binding yet it behaves exactly like first-run onboarding.
    fn for_project(project: ProjectRef, registry: &registry::Registry) -> Self {
        let mut w = Wizard::new(
            project.clone(),
            registry.layout().clone(),
            registry.repo_handles(),
            registry.project_handles(),
        );
        if let Ok(desc) = registry.project(&project.id) {
            // Keep this project's own handle stable; don't let uniquify bump it.
            w.taken_handles.retain(|h| h != &desc.handle);
            w.fixed_handle = Some(desc.handle.clone());
            for repo in registry.candidate_repos(&project.id) {
                let label = repo
                    .remote
                    .clone()
                    .unwrap_or_else(|| "local-only".to_string());
                w.repos.push(RepoRow {
                    handle: repo.handle.clone(),
                    draft: None, // already registered — a candidate, not a new block
                    label,
                });
            }
            let primary_idx = w.repos.iter().position(|r| r.handle == desc.primary);
            w.primary.select(Some(primary_idx.unwrap_or(0)));
            w.branch_prefix = desc.branch_prefix.clone().unwrap_or_default();
            w.scratch = desc
                .scratch
                .iter()
                .map(|s| ScratchDraft {
                    name: s.name.clone(),
                    provision: s.provision.clone(),
                    teardown: s.teardown.clone(),
                    env: s.env.clone(),
                    needs_port: s.needs_port,
                    required: s.required,
                    persist: s.persist,
                })
                .collect();
        }
        w
    }

    /// Resolve a typed repo input into a [`RepoRow`]: an exact existing handle is
    /// reused; a directory is probed for its `origin` remote; anything URL-shaped is
    /// taken as a remote. The only impure method (filesystem + `git`).
    fn resolve(&self, raw: &str) -> Result<RepoRow, String> {
        let input = raw.trim();
        if input.is_empty() {
            return Err("type a path or a remote URL".into());
        }
        // 1. An exact existing registered handle → reuse (candidate only, no block).
        if self.registered_repos.iter().any(|h| h == input) {
            return Ok(RepoRow {
                handle: input.to_string(),
                draft: None,
                label: "already registered".into(),
            });
        }
        // 2. A local directory → derive the remote from its `origin`, the handle from
        // the remote (else the path). No remote is fine: a local-only repo.
        let expanded = expand_tilde(input);
        if expanded.is_dir() {
            let remote = git_origin(&expanded);
            let handle = registry::handle_from_source(remote.as_deref().unwrap_or(input));
            if !registry::is_safe_handle(&handle) {
                return Err("couldn't derive a name from that path — try another".into());
            }
            let label = remote
                .clone()
                .unwrap_or_else(|| "local-only (no origin remote)".to_string());
            // Store an ABSOLUTE path: `is_dir()` resolved a relative input (`./core`,
            // `../sibling`) against the wizard's cwd, but the loader and `git clone`
            // run later from a different cwd, so a verbatim relative `local` would
            // resolve elsewhere or vanish. `canonicalize` can't fail here (the dir
            // just existed); fall back to the expanded path if it somehow does.
            let local = expanded
                .canonicalize()
                .unwrap_or(expanded)
                .to_string_lossy()
                .into_owned();
            return Ok(RepoRow {
                handle: handle.clone(),
                draft: Some(RepoDraft {
                    handle,
                    remote,
                    local: Some(local),
                }),
                label,
            });
        }
        // 3. Otherwise it must look like a remote URL.
        if looks_like_url(input) {
            let handle = registry::handle_from_source(input);
            if !registry::is_safe_handle(&handle) {
                return Err("couldn't derive a name from that URL — try another".into());
            }
            return Ok(RepoRow {
                handle: handle.clone(),
                draft: Some(RepoDraft {
                    handle,
                    remote: Some(input.to_string()),
                    local: None,
                }),
                label: input.to_string(),
            });
        }
        Err("not a directory, a URL, or a registered repo — give a path or remote URL".into())
    }

    /// Add the current input as a repo (deduped by handle). A no-op on empty input.
    fn add_repo(&mut self) {
        if self.input.trim().is_empty() {
            return;
        }
        match self.resolve(&self.input) {
            Ok(row) => {
                if self.repos.iter().any(|r| r.handle == row.handle) {
                    self.error = Some(format!("'{}' is already added", row.handle));
                } else {
                    self.repos.push(row);
                    self.input.clear();
                    self.error = None;
                }
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// Leave the repos step: require at least one repo, then go to primary-select
    /// (or straight to the branch prefix when there's only one repo).
    fn advance_from_repos(&mut self) {
        if self.repos.is_empty() {
            self.error = Some("add at least one repo first".into());
            return;
        }
        self.error = None;
        self.primary.select(Some(0));
        self.step = if self.repos.len() == 1 {
            Step::BranchPrefix
        } else {
            Step::Primary
        };
    }

    fn primary_move(&mut self, delta: i32) {
        move_state(&mut self.primary, self.repos.len(), delta);
    }

    /// Leave the branch-prefix step. An empty prefix is fine — it falls back to
    /// `$USER`/`lindep` at launch (see [`crate::worktree::default_branch_prefix`]) —
    /// but a non-empty one must be a valid git ref segment, or `git worktree add -b
    /// <prefix>/<issue>` would reject every launch in this project. Catch a bad prefix
    /// here, in the wizard, rather than silently bricking the project at first launch.
    fn advance_from_branch_prefix(&mut self) {
        let prefix = self.branch_prefix.trim();
        if !prefix.is_empty() && !crate::worktree::is_valid_branch_prefix(prefix) {
            self.error = Some(
                "branch prefix isn't a valid git ref — no spaces or ~^:?*[\\, \
                 no leading/trailing '/' or '.'"
                    .into(),
            );
            return;
        }
        self.error = None;
        self.step = Step::Scratch;
    }

    /// Add the in-progress scratch entry to the list (validated), then reset the form;
    /// a bad entry sets the footer error instead. A no-op when the name is blank.
    fn add_scratch(&mut self) {
        if self.scratch_form.name.trim().is_empty() {
            return;
        }
        match self.scratch_form.to_draft() {
            Ok(draft) => {
                if self.scratch.iter().any(|s| s.name == draft.name) {
                    self.error = Some(format!("scratch '{}' is already added", draft.name));
                } else {
                    self.scratch.push(draft);
                    self.scratch_form = ScratchForm::default();
                    self.error = None;
                }
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// The handle for this project's isolated dir: the existing one when editing, else
    /// derived from the Linear name and uniquified (`core-pms`, then `core-pms-2`, …).
    fn unique_handle(&self) -> String {
        if let Some(fixed) = &self.fixed_handle {
            return fixed.clone();
        }
        let base = registry::handle_from_source(&self.project.name);
        let base = if base.is_empty() {
            "project".to_string()
        } else {
            base
        };
        let mut candidate = base.clone();
        let mut n = 2;
        while self.taken_handles.contains(&candidate) {
            candidate = format!("{base}-{n}");
            n += 1;
        }
        candidate
    }

    /// The resolved project binding to write.
    fn project_draft(&self) -> ProjectDraft {
        let idx = self
            .primary
            .selected()
            .unwrap_or(0)
            .min(self.repos.len().saturating_sub(1));
        let primary = self
            .repos
            .get(idx)
            .map(|r| r.handle.clone())
            .unwrap_or_default();
        let candidates = self.repos.iter().map(|r| r.handle.clone()).collect();
        let prefix = self.branch_prefix.trim();
        ProjectDraft {
            id: self.project.id.clone(),
            handle: self.unique_handle(),
            name: self.project.name.clone(),
            candidates,
            primary,
            branch_prefix: (!prefix.is_empty()).then(|| prefix.to_string()),
        }
    }

    /// The new `[[repo]]` blocks to write (reused handles contribute none).
    fn new_repos(&self) -> Vec<RepoDraft> {
        self.repos.iter().filter_map(|r| r.draft.clone()).collect()
    }
}

/// Whether `s` looks like a git remote rather than a bare word — so a typo doesn't
/// get silently registered as a remote named after itself.
fn looks_like_url(s: &str) -> bool {
    s.contains("://") || s.contains('@') || s.ends_with(".git")
}

/// Expand a leading `~`/`~/` to `$HOME` (mirrors the registry loader so a path the
/// wizard accepts is the same one the loader later expands).
fn expand_tilde(raw: &str) -> PathBuf {
    if raw == "~"
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home);
    }
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(raw)
}

/// The `origin` remote URL of a git clone, or `None` if it isn't a repo or has no
/// origin (a local-only repo lindep mirrors from the clone itself).
fn git_origin(dir: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!url.is_empty()).then_some(url)
}

fn run_loop(terminal: &mut DefaultTerminal, wizard: &mut Wizard) -> io::Result<bool> {
    loop {
        terminal.draw(|frame| draw(wizard, frame))?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // Ctrl-C always cancels the whole wizard (matches the picker).
        if ctrl && matches!(key.code, KeyCode::Char('c')) {
            return Ok(false);
        }
        match wizard.step {
            Step::Repos => match key.code {
                KeyCode::Esc => return Ok(false),
                // Enter on a filled field adds the repo; on an empty field it advances.
                KeyCode::Enter if wizard.input.trim().is_empty() => wizard.advance_from_repos(),
                KeyCode::Enter => wizard.add_repo(),
                KeyCode::Backspace => {
                    wizard.input.pop();
                }
                KeyCode::Char(c) if !ctrl && !alt => wizard.input.push(c),
                _ => {}
            },
            Step::Primary => match key.code {
                KeyCode::Esc => wizard.step = Step::Repos,
                KeyCode::Up => wizard.primary_move(-1),
                KeyCode::Down => wizard.primary_move(1),
                KeyCode::Enter => wizard.step = Step::BranchPrefix,
                _ => {}
            },
            Step::BranchPrefix => match key.code {
                KeyCode::Esc => {
                    wizard.step = if wizard.repos.len() > 1 {
                        Step::Primary
                    } else {
                        Step::Repos
                    }
                }
                KeyCode::Enter => wizard.advance_from_branch_prefix(),
                KeyCode::Backspace => {
                    wizard.branch_prefix.pop();
                }
                KeyCode::Char(c) if !ctrl && !alt => wizard.branch_prefix.push(c),
                _ => {}
            },
            Step::Scratch => match key.code {
                KeyCode::Esc => wizard.step = Step::BranchPrefix,
                KeyCode::Up => wizard.scratch_form.move_focus(-1),
                KeyCode::Down => wizard.scratch_form.move_focus(1),
                // Enter on a blank form advances; on a filled one adds the entry.
                KeyCode::Enter if wizard.scratch_form.name.trim().is_empty() => {
                    wizard.error = None;
                    wizard.step = Step::Confirm;
                }
                KeyCode::Enter => wizard.add_scratch(),
                KeyCode::Backspace => wizard.scratch_form.backspace(),
                // Space toggles a focused flag, else types into the focused text field.
                KeyCode::Char(c) if !ctrl && !alt => wizard.scratch_form.type_char(c),
                _ => {}
            },
            Step::Confirm => match key.code {
                KeyCode::Esc => wizard.step = Step::Scratch,
                KeyCode::Enter => {
                    match registry::write_binding(
                        &wizard.layout,
                        &wizard.new_repos(),
                        &wizard.project_draft(),
                        &wizard.scratch,
                    ) {
                        Ok(()) => return Ok(true),
                        // A write failure stays in the wizard so the user can retry or
                        // cancel rather than crashing out of the cockpit launch.
                        Err(e) => wizard.error = Some(e.to_string()),
                    }
                }
                _ => {}
            },
        }
    }
}

fn draw(wizard: &mut Wizard, frame: &mut Frame) {
    let [header, body, error, hint] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let title = Line::from(vec![
        Span::styled("  lindep ", Style::new().fg(GREEN_500).bold()),
        Span::styled("· connect ", Style::new().fg(GREEN_100)),
        Span::styled(wizard.project.name.clone(), Style::new().fg(INK).bold()),
    ]);
    let step_no = match wizard.step {
        Step::Repos => "1 · repos",
        Step::Primary => "2 · primary repo",
        Step::BranchPrefix => "3 · branch prefix (optional)",
        Step::Scratch => "4 · scratch datastores (optional)",
        Step::Confirm => "5 · confirm",
    };
    frame.render_widget(
        Paragraph::new(vec![
            title,
            Line::from(Span::styled(format!("  {step_no}"), Style::new().fg(MUTED))),
        ]),
        header,
    );

    let hint_text = match wizard.step {
        Step::Repos => " type a path or remote URL · ⏎ add · ⏎ on empty → next · esc cancel",
        Step::Primary => " ↑↓ move · ⏎ choose primary · esc back",
        Step::BranchPrefix => " type a prefix · ⏎ accept (empty = default) · esc back",
        Step::Scratch => " ↑↓ field · type/space edit · ⏎ add · ⏎ on empty → next · esc back",
        Step::Confirm => " ⏎ write to ~/.lindep/registry.toml · esc back",
    };

    match wizard.step {
        Step::Repos => draw_repos(wizard, frame, body),
        Step::Primary => draw_primary(wizard, frame, body),
        Step::BranchPrefix => draw_branch_prefix(wizard, frame, body),
        Step::Scratch => draw_scratch(wizard, frame, body),
        Step::Confirm => draw_confirm(wizard, frame, body),
    }

    if let Some(msg) = &wizard.error {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  {msg}"),
                Style::new().fg(AMBER_400).bold(),
            ))),
            error,
        );
    }
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hint_text, Style::new().fg(MUTED))))
            .style(Style::new().bg(WELL)),
        hint,
    );
}

fn framed(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(GREEN_500))
        .title(Line::from(Span::styled(
            format!(" {title} "),
            Style::new().fg(GREEN_100).bold(),
        )))
}

fn draw_repos(wizard: &Wizard, frame: &mut Frame, area: Rect) {
    let block = framed("REPOS");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [field, list, reuse] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(inner);

    let field_line = Line::from(vec![
        Span::styled(" repo ", Style::new().fg(GREEN_400)),
        Span::styled(wizard.input.clone(), Style::new().fg(INK)),
        Span::styled("▏", Style::new().fg(GREEN_500)),
    ]);
    frame.render_widget(Paragraph::new(field_line), field);

    let items: Vec<ListItem> = if wizard.repos.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "  (no repos yet — add the one this project lives in)",
            Style::new().fg(MUTED),
        )))]
    } else {
        wizard
            .repos
            .iter()
            .map(|r| {
                ListItem::new(Line::from(vec![
                    Span::styled("  • ", Style::new().fg(GREEN_400)),
                    Span::styled(r.handle.clone(), Style::new().fg(INK)),
                    Span::styled(format!("  {}", r.label), Style::new().fg(MUTED)),
                ]))
            })
            .collect()
    };
    frame.render_widget(List::new(items), list);

    if !wizard.registered_repos.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(
                    " reuse a registered repo: {}",
                    wizard.registered_repos.join(", ")
                ),
                Style::new().fg(MUTED),
            ))),
            reuse,
        );
    }
}

fn draw_primary(wizard: &mut Wizard, frame: &mut Frame, area: Rect) {
    let block = framed("PRIMARY REPO");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let items: Vec<ListItem> = wizard
        .repos
        .iter()
        .map(|r| {
            ListItem::new(Line::from(vec![
                Span::styled(r.handle.clone(), Style::new().fg(INK)),
                Span::styled(format!("  {}", r.label), Style::new().fg(MUTED)),
            ]))
        })
        .collect();
    let list = List::new(items)
        .highlight_symbol("▸ ")
        .highlight_spacing(HighlightSpacing::Always)
        .highlight_style(theme::cursor_active());
    frame.render_stateful_widget(list, inner, &mut wizard.primary);
}

fn draw_branch_prefix(wizard: &Wizard, frame: &mut Frame, area: Rect) {
    let block = framed("BRANCH PREFIX");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [field, note] = Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(inner);
    let field_line = Line::from(vec![
        Span::styled(" prefix ", Style::new().fg(GREEN_400)),
        Span::styled(wizard.branch_prefix.clone(), Style::new().fg(INK)),
        Span::styled("▏", Style::new().fg(GREEN_500)),
    ]);
    frame.render_widget(Paragraph::new(field_line), field);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "  the per-project branch namespace; leave empty to use your git user name",
            Style::new().fg(MUTED),
        ))),
        note,
    );
}

fn draw_scratch(wizard: &Wizard, frame: &mut Frame, area: Rect) {
    let block = framed("SCRATCH DATASTORES");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [added, form] = Layout::vertical([Constraint::Length(4), Constraint::Min(0)]).areas(inner);

    // The entries added so far (empty is fine — this whole step is optional).
    let mut added_lines = vec![Line::from(Span::styled(
        "  added (per-issue, isolated; provisioned at launch, torn down at discard):",
        Style::new().fg(MUTED),
    ))];
    if wizard.scratch.is_empty() {
        added_lines.push(Line::from(Span::styled(
            "    (none — skip with ⏎ on the empty form below)",
            Style::new().fg(MUTED),
        )));
    } else {
        for s in &wizard.scratch {
            added_lines.push(Line::from(vec![
                Span::styled("    • ", Style::new().fg(GREEN_400)),
                Span::styled(s.name.clone(), Style::new().fg(INK)),
                Span::styled(format!("  {}", s.provision), Style::new().fg(MUTED)),
            ]));
        }
    }
    frame.render_widget(Paragraph::new(added_lines), added);

    // The in-progress entry — the focused field carries a "▸" and a text cursor / a
    // checkbox glyph for the flags.
    let f = &wizard.scratch_form;
    let text_row = |idx: usize, label: &str, val: &str| {
        let focused = f.focus == idx;
        let marker = if focused { "▸ " } else { "  " };
        let mut spans = vec![
            Span::styled(format!("  {marker}{label:<11}"), Style::new().fg(GREEN_400)),
            Span::styled(val.to_string(), Style::new().fg(INK)),
        ];
        if focused {
            spans.push(Span::styled("▏", Style::new().fg(GREEN_500)));
        }
        Line::from(spans)
    };
    let flag_row = |idx: usize, label: &str, on: bool| {
        let focused = f.focus == idx;
        let marker = if focused { "▸ " } else { "  " };
        let (glyph, gstyle) = theme::repo_check(on);
        Line::from(vec![
            Span::styled(format!("  {marker}{label:<11}"), Style::new().fg(GREEN_400)),
            Span::styled(format!("{glyph} "), gstyle),
        ])
    };
    let form_lines = vec![
        text_row(0, "name", &f.name),
        text_row(1, "provision", &f.provision),
        text_row(2, "teardown", &f.teardown),
        text_row(3, "env", &f.env),
        flag_row(4, "needs_port", f.needs_port),
        flag_row(5, "required", f.required),
        flag_row(6, "persist", f.persist),
        Line::raw(""),
        Line::from(Span::styled(
            "  env is space-separated KEY=VALUE; {issue}/{slug}/{port} are substituted",
            Style::new().fg(MUTED),
        )),
    ];
    frame.render_widget(Paragraph::new(form_lines), form);
}

fn draw_confirm(wizard: &Wizard, frame: &mut Frame, area: Rect) {
    let block = framed("CONFIRM");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let draft = wizard.project_draft();
    let mut lines = vec![
        Line::from(vec![
            Span::styled("  project  ", Style::new().fg(MUTED)),
            Span::styled(draft.name.clone(), Style::new().fg(INK)),
            Span::styled(
                format!("  →  {}/", draft.handle),
                Style::new().fg(GREEN_400),
            ),
        ]),
        Line::raw(""),
    ];
    for r in &wizard.repos {
        let tag = if r.handle == draft.primary {
            Span::styled("  (primary)", Style::new().fg(GREEN_400))
        } else {
            Span::raw("")
        };
        lines.push(Line::from(vec![
            Span::styled("  • ", Style::new().fg(GREEN_400)),
            Span::styled(r.handle.clone(), Style::new().fg(INK)),
            tag,
            Span::styled(format!("   {}", r.label), Style::new().fg(MUTED)),
        ]));
    }
    lines.push(Line::raw(""));
    let prefix = draft
        .branch_prefix
        .clone()
        .unwrap_or_else(|| "(git user name)".to_string());
    lines.push(Line::from(vec![
        Span::styled("  branch prefix  ", Style::new().fg(MUTED)),
        Span::styled(prefix, Style::new().fg(INK)),
    ]));
    lines.push(Line::raw(""));
    if wizard.scratch.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no scratch datastores",
            Style::new().fg(MUTED),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!("  scratch  {}", wizard.scratch.len()),
            Style::new().fg(MUTED),
        )));
        for s in &wizard.scratch {
            lines.push(Line::from(vec![
                Span::styled("    • ", Style::new().fg(GREEN_400)),
                Span::styled(s.name.clone(), Style::new().fg(INK)),
                Span::styled(format!("   {}", s.provision), Style::new().fg(MUTED)),
            ]));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wizard(registered: &[&str], taken: &[&str]) -> Wizard {
        Wizard::new(
            ProjectRef {
                id: "p-uuid".into(),
                name: "Core PMS".into(),
            },
            registry::Layout::new("/tmp/lindep-onboard-test"),
            registered.iter().map(|s| s.to_string()).collect(),
            taken.iter().map(|s| s.to_string()).collect(),
        )
    }

    #[test]
    fn a_url_resolves_to_a_new_remote_repo() {
        let w = wizard(&[], &[]);
        let row = w.resolve("git@github.com:zaplar/core-pms.git").unwrap();
        assert_eq!(row.handle, "core-pms");
        let draft = row.draft.unwrap();
        assert_eq!(
            draft.remote.as_deref(),
            Some("git@github.com:zaplar/core-pms.git")
        );
        assert!(draft.local.is_none());
    }

    #[test]
    fn an_existing_handle_resolves_to_a_reused_repo() {
        let w = wizard(&["lindep"], &[]);
        let row = w.resolve("lindep").unwrap();
        assert_eq!(row.handle, "lindep");
        assert!(row.draft.is_none(), "a reused repo writes no new [[repo]]");
    }

    #[test]
    fn a_bare_word_that_is_not_registered_is_rejected() {
        let w = wizard(&[], &[]);
        assert!(w.resolve("not-a-url").is_err());
    }

    #[test]
    fn advancing_with_one_repo_skips_the_primary_step() {
        let mut w = wizard(&[], &[]);
        w.input = "git@github.com:zaplar/core-pms.git".into();
        w.add_repo();
        assert_eq!(w.repos.len(), 1);
        w.advance_from_repos();
        assert_eq!(w.step, Step::BranchPrefix, "one repo → no primary choice");
    }

    #[test]
    fn advancing_with_two_repos_asks_for_the_primary() {
        let mut w = wizard(&["shared"], &[]);
        w.input = "git@github.com:zaplar/core.git".into();
        w.add_repo();
        w.input = "shared".into();
        w.add_repo();
        assert_eq!(w.repos.len(), 2);
        w.advance_from_repos();
        assert_eq!(w.step, Step::Primary);
    }

    #[test]
    fn adding_the_same_repo_twice_is_refused() {
        let mut w = wizard(&[], &[]);
        w.input = "git@github.com:zaplar/core.git".into();
        w.add_repo();
        w.input = "https://github.com/zaplar/core".into(); // same derived handle "core"
        w.add_repo();
        assert_eq!(w.repos.len(), 1, "dedup is by derived handle");
        assert!(w.error.is_some());
    }

    #[test]
    fn the_project_handle_is_uniquified_against_the_registry() {
        // "Core PMS" derives "Core-PMS" — free here, so it's kept verbatim.
        let mut free = wizard(&[], &[]);
        free.input = "git@github.com:zaplar/core.git".into();
        free.add_repo();
        assert_eq!(free.project_draft().handle, "Core-PMS");
        // A clash with an existing project handle bumps a numeric suffix.
        let mut clash = wizard(&[], &["Core-PMS"]);
        clash.input = "git@github.com:zaplar/core.git".into();
        clash.add_repo();
        assert_eq!(clash.project_draft().handle, "Core-PMS-2");
    }

    #[test]
    fn project_draft_marks_primary_and_collects_candidates() {
        let mut w = wizard(&["shared"], &[]);
        w.input = "git@github.com:zaplar/core.git".into();
        w.add_repo();
        w.input = "shared".into();
        w.add_repo();
        w.advance_from_repos();
        w.primary_move(1); // move primary cursor to "shared"
        w.step = Step::Confirm;
        let draft = w.project_draft();
        assert_eq!(draft.primary, "shared");
        assert_eq!(draft.candidates, vec!["core", "shared"]);
        // Only the freshly-added remote repo is written; the reused one isn't.
        assert_eq!(w.new_repos().len(), 1);
        assert_eq!(w.new_repos()[0].handle, "core");
    }

    #[test]
    fn the_branch_prefix_is_optional() {
        let mut w = wizard(&[], &[]);
        w.input = "git@github.com:zaplar/core.git".into();
        w.add_repo();
        assert!(w.project_draft().branch_prefix.is_none(), "empty = default");
        w.branch_prefix = "felix".into();
        assert_eq!(w.project_draft().branch_prefix.as_deref(), Some("felix"));
    }

    #[test]
    fn a_scratch_entry_is_validated_and_added() {
        let mut w = wizard(&[], &[]);
        // A name without a provision command is refused.
        w.scratch_form.name = "db".into();
        w.add_scratch();
        assert!(w.scratch.is_empty());
        assert!(w.error.is_some());
        // With a provision command and an env pair it's accepted and the form resets.
        w.scratch_form.provision = "createdb scratch_{slug}".into();
        w.scratch_form.env = "DATABASE_URL=postgres:///scratch_{slug}".into();
        w.scratch_form.required = true;
        w.add_scratch();
        assert_eq!(w.scratch.len(), 1);
        assert_eq!(w.scratch[0].name, "db");
        assert!(w.scratch[0].required);
        assert_eq!(
            w.scratch[0].env.get("DATABASE_URL").map(String::as_str),
            Some("postgres:///scratch_{slug}")
        );
        assert!(
            w.scratch_form.name.is_empty(),
            "the form resets after adding"
        );
    }

    #[test]
    fn a_scratch_with_malformed_env_is_refused() {
        let mut w = wizard(&[], &[]);
        w.scratch_form.name = "db".into();
        w.scratch_form.provision = "createdb x".into();
        w.scratch_form.env = "NOTAPAIR".into();
        w.add_scratch();
        assert!(w.scratch.is_empty());
        assert!(w.error.as_deref().unwrap().contains("KEY=VALUE"));
    }

    #[test]
    fn the_scratch_form_toggles_flags_with_space_only_when_focused() {
        let mut w = wizard(&[], &[]);
        // Focus the name (text) field — a space types into it.
        w.scratch_form.type_char(' ');
        assert_eq!(w.scratch_form.name, " ");
        assert!(!w.scratch_form.needs_port);
        // Focus the needs_port flag (index 4) — a space toggles it.
        w.scratch_form.focus = 4;
        w.scratch_form.type_char(' ');
        assert!(w.scratch_form.needs_port);
    }

    #[test]
    fn for_project_prepopulates_from_an_existing_binding() {
        // Build a registry on disk, then re-enter the wizard for that project.
        let root = std::env::temp_dir().join(format!("lindep-onboard-edit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("registry.toml"),
            "[[repo]]\nhandle = \"core\"\nremote = \"git@github.com:zaplar/core\"\n\n\
             [[project]]\nid = \"p9\"\nhandle = \"core-world\"\nname = \"Core\"\n\
             primary = \"core\"\nbranch_prefix = \"felix\"\n",
        )
        .unwrap();
        let (reg, _) = registry::Registry::load_at(registry::Layout::new(&root));
        let w = Wizard::for_project(
            ProjectRef {
                id: "p9".into(),
                name: "Core".into(),
            },
            &reg,
        );
        // The existing repo, primary, branch prefix and handle are carried over.
        assert_eq!(w.repos.len(), 1);
        assert_eq!(w.repos[0].handle, "core");
        assert_eq!(w.branch_prefix, "felix");
        assert_eq!(
            w.project_draft().handle,
            "core-world",
            "the on-disk handle is kept"
        );
        // The reused repo writes no new [[repo]] block.
        assert!(w.new_repos().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_wizard_renders_each_step_without_panic() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut w = wizard(&["shared"], &[]);
        w.input = "git@github.com:zaplar/core.git".into();
        w.add_repo();
        w.input = "shared".into();
        w.add_repo();
        for step in [
            Step::Repos,
            Step::Primary,
            Step::BranchPrefix,
            Step::Scratch,
            Step::Confirm,
        ] {
            w.step = step;
            if step != Step::Repos {
                w.primary.select(Some(0));
            }
            let mut term = Terminal::new(TestBackend::new(90, 24)).unwrap();
            term.draw(|f| draw(&mut w, f)).unwrap();
            let out = term.backend().to_string();
            assert!(out.contains("connect"), "header shows on {step:?}");
        }
    }
}
