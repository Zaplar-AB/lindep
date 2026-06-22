//! Interactive project picker — the entry screen shown when no project is named
//! on the command line. Type to filter, arrows to move, Enter to open. This
//! sidesteps shell-quoting project names that contain spaces.

use std::io;

use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph,
};

use crate::linear::ProjectRef;
use crate::theme::{self, *};
use crate::window::move_state;

fn empty_picker_line(query: &str, total: usize) -> Line<'static> {
    let msg = if total == 0 {
        " no projects available"
    } else if query.is_empty() {
        " no projects to show"
    } else {
        " no matches — edit the filter"
    };
    Line::from(Span::styled(
        msg,
        Style::new().fg(MUTED).add_modifier(Modifier::ITALIC),
    ))
}

/// The project list + live filter state. Drives both the startup full-screen
/// [`pick`] and the in-cockpit switch overlay ([`render_overlay`]); the cockpit
/// owns an instance and feeds it keys directly (see `App::on_switcher_key`).
pub(crate) struct Picker {
    projects: Vec<ProjectRef>,
    order: Vec<usize>, // indices into `projects` that match the filter
    state: ListState,
    pub(crate) query: String,
}

impl Picker {
    pub(crate) fn new(mut projects: Vec<ProjectRef>) -> Self {
        projects.sort_by_key(|p| p.name.to_lowercase());
        let mut picker = Picker {
            projects,
            order: Vec::new(),
            state: ListState::default(),
            query: String::new(),
        };
        picker.refilter();
        picker
    }

    pub(crate) fn refilter(&mut self) {
        let needle = self.query.to_lowercase();
        self.order = self
            .projects
            .iter()
            .enumerate()
            .filter(|(_, p)| needle.is_empty() || p.name.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        if self.order.is_empty() {
            self.state.select(None);
        } else {
            let i = self.state.selected().unwrap_or(0).min(self.order.len() - 1);
            self.state.select(Some(i));
        }
    }

    pub(crate) fn move_by(&mut self, delta: i32) {
        move_state(&mut self.state, self.order.len(), delta);
    }

    pub(crate) fn selected(&self) -> Option<ProjectRef> {
        self.state
            .selected()
            .and_then(|i| self.order.get(i))
            .map(|&idx| self.projects[idx].clone())
    }
}

/// One row in the up-front repo multi-select (ENG-536): a candidate repo, whether
/// it's local-only (a `(local)` tag — no remote, so PRs/auto-push are off), and
/// whether it's the project's `primary` (always materialised, so pre-checked and
/// pinned).
#[derive(Clone)]
pub(crate) struct RepoChoice {
    pub handle: String,
    pub local: bool,
    pub primary: bool,
}

/// The "add another repo" sub-list (CF-20): registered repos the project doesn't yet
/// list as candidates, with their own cursor — plus a text field for typing a brand-new
/// repo (a URL or local path) the registry doesn't know yet, resolved the same way the
/// onboarding wizard resolves its Repos step. Open from the [`RepoPicker`] with `a`;
/// picking a row (empty input) or confirming a typed repo appends it to the checklist
/// (checked) so this launch spans it and the confirm persists it as a project candidate.
struct AddList {
    rows: Vec<RepoChoice>,
    state: ListState,
    /// The live text input — a URL/path/handle the human is typing to add a repo the
    /// project (and possibly the registry) doesn't list. Empty = pick from `rows`.
    input: String,
}

/// The repo multi-select modal — a checkbox list over a project's candidate repos
/// in **declared order** (no sort, unlike the project [`Picker`]). The primary is
/// pre-checked and can't be unchecked; Space toggles the rest. Owned by `App` and
/// fed keys directly (see `App::on_repo_select_key`), like the project switcher. While
/// the `adding` sub-list is open it captures movement/confirm so the human can pull in
/// a registered repo the project doesn't yet list (CF-20).
pub(crate) struct RepoPicker {
    rows: Vec<RepoChoice>,
    checked: Vec<bool>,
    state: ListState,
    adding: Option<AddList>,
}

impl RepoPicker {
    pub(crate) fn new(rows: Vec<RepoChoice>) -> Self {
        let checked = rows.iter().map(|r| r.primary).collect();
        let mut state = ListState::default();
        if !rows.is_empty() {
            state.select(Some(0));
        }
        RepoPicker {
            rows,
            checked,
            state,
            adding: None,
        }
    }

    /// Movement drives the add-list while it's open, else the main checklist.
    pub(crate) fn move_by(&mut self, delta: i32) {
        match &mut self.adding {
            Some(add) => move_state(&mut add.state, add.rows.len(), delta),
            None => move_state(&mut self.state, self.rows.len(), delta),
        }
    }

    /// Toggle the cursor's repo. The primary is always materialised, so toggling it
    /// is a deliberate no-op (it stays checked). Inert while the add-list is open.
    pub(crate) fn toggle(&mut self) {
        if self.adding.is_some() {
            return;
        }
        if let Some(i) = self.state.selected()
            && !self.rows[i].primary
        {
            self.checked[i] = !self.checked[i];
        }
    }

    /// Whether the "add another repo" sub-list is currently open.
    pub(crate) fn is_adding(&self) -> bool {
        self.adding.is_some()
    }

    /// Open the "add another repo" sub-list over `registered` minus the repos already
    /// offered here (the project's current candidates). Always opens — even with nothing
    /// left to pick — because the sub-list also hosts the typed-add field, the only way to
    /// reach a repo the registry doesn't know yet (CF-20). The empty-pick-list case is a
    /// valid state (type a URL/path), not a dead end.
    pub(crate) fn open_add(&mut self, registered: &[RepoChoice]) {
        let here: std::collections::HashSet<&str> =
            self.rows.iter().map(|r| r.handle.as_str()).collect();
        let rows: Vec<RepoChoice> = registered
            .iter()
            .filter(|r| !here.contains(r.handle.as_str()))
            .cloned()
            .collect();
        let mut state = ListState::default();
        if !rows.is_empty() {
            state.select(Some(0));
        }
        self.adding = Some(AddList {
            rows,
            state,
            input: String::new(),
        });
    }

    /// Push `c` onto the add-list's text input (typing a URL/path/handle). Inert unless
    /// the add-list is open.
    pub(crate) fn add_input_push(&mut self, c: char) {
        if let Some(add) = self.adding.as_mut() {
            add.input.push(c);
        }
    }

    /// Backspace the add-list's text input. Inert unless the add-list is open.
    pub(crate) fn add_input_pop(&mut self) {
        if let Some(add) = self.adding.as_mut() {
            add.input.pop();
        }
    }

    /// The add-list's current text input, trimmed — empty when nothing's typed (so the
    /// caller picks the highlighted row instead) or the add-list is closed.
    pub(crate) fn add_input(&self) -> &str {
        self.adding.as_ref().map_or("", |a| a.input.trim())
    }

    /// Append `choice` to the checklist as a checked, non-primary row and close the
    /// add-list — the typed-add path's commit, after the caller has resolved and
    /// registered the repo. Dedups: a handle already on the checklist is just re-checked.
    pub(crate) fn push_checked_and_close(&mut self, choice: RepoChoice) {
        self.adding = None;
        if let Some(i) = self.rows.iter().position(|r| r.handle == choice.handle) {
            self.checked[i] = true;
            return;
        }
        self.rows.push(RepoChoice {
            handle: choice.handle,
            local: choice.local,
            primary: false,
        });
        self.checked.push(true);
    }

    /// Commit the add-list cursor: append the chosen repo to the checklist as a
    /// checked, non-primary row, then close the sub-list. A no-op if nothing's open.
    pub(crate) fn confirm_add(&mut self) {
        let Some(add) = self.adding.take() else {
            return;
        };
        if let Some(i) = add.state.selected()
            && let Some(choice) = add.rows.get(i)
        {
            self.rows.push(RepoChoice {
                handle: choice.handle.clone(),
                local: choice.local,
                primary: false,
            });
            self.checked.push(true);
        }
    }

    /// Back out of the add-list without adding anything (Esc inside the sub-list).
    pub(crate) fn cancel_add(&mut self) {
        self.adding = None;
    }

    /// The checked handles, in declared order — always including the primary, plus any
    /// repos added via the sub-list (appended after the candidates).
    pub(crate) fn selected_handles(&self) -> Vec<String> {
        self.rows
            .iter()
            .zip(&self.checked)
            .filter(|&(_, &c)| c)
            .map(|(r, _)| r.handle.clone())
            .collect()
    }
}

/// Render the picker as a centered modal over the running cockpit (the in-session
/// project switcher, `Ctrl-a s`). Mirrors the full-screen [`draw`] but boxed and
/// with a `Clear` behind it so it floats above the cockpit; the cockpit feeds it
/// keys (see `App::on_switcher_key`) instead of running its own event loop.
pub(crate) fn render_overlay(
    picker: &mut Picker,
    frame: &mut Frame,
    full: Rect,
    needs_you: &std::collections::HashSet<String>,
    counts: &std::collections::HashMap<String, (usize, usize)>,
) {
    // Widen to u32 for the arithmetic so a pathologically large terminal can't
    // overflow u16 (`width * 6`) and panic in a debug build.
    let width = (u32::from(full.width) * 6 / 10)
        .clamp(u32::from(40.min(full.width)), u32::from(full.width)) as u16;
    let height = (u32::from(full.height) * 6 / 10)
        .clamp(u32::from(8.min(full.height)), u32::from(full.height)) as u16;
    let area = Rect {
        x: full.x + (full.width.saturating_sub(width)) / 2,
        y: full.y + (full.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(GREEN_500))
        .title(Line::from(Span::styled(
            " SWITCH PROJECT ",
            Style::new().fg(GREEN_100).bold(),
        )));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [query, body, hint] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(inner);

    let query_line = Line::from(vec![
        Span::styled(" /", Style::new().fg(GREEN_400)),
        Span::styled(picker.query.clone(), Style::new().fg(INK)),
        Span::styled("▏", Style::new().fg(GREEN_500)),
        Span::raw("  "),
        Span::styled(
            format!("{}/{}", picker.order.len(), picker.projects.len()),
            Style::new().fg(MUTED),
        ),
    ]);
    frame.render_widget(Paragraph::new(query_line), query);

    if picker.order.is_empty() {
        frame.render_widget(
            Paragraph::new(empty_picker_line(&picker.query, picker.projects.len())),
            body,
        );
    } else {
        // A project with an agent that needs you carries a breathing ⚑ — so a
        // backgrounded prompt is visible right where you'd switch to handle it.
        let items: Vec<ListItem> = picker
            .order
            .iter()
            .map(|&i| {
                let project = &picker.projects[i];
                let mut spans = vec![Span::styled(project.name.clone(), Style::new().fg(INK))];
                // Per-project agent counts (ENG-406): `· 3 agents · 1 needs you`, so a
                // backgrounded project's load shows in the list without entering it.
                // Steady (not breathing) — the modal doesn't drive the animation tick.
                if let Some(&(live, needs)) = counts.get(&project.id) {
                    if live > 0 {
                        spans.push(Span::styled(
                            format!("  · {live} agent{}", if live == 1 { "" } else { "s" }),
                            Style::new().fg(GREEN_400),
                        ));
                    }
                    if needs > 0 {
                        spans.push(Span::styled(
                            format!(" · {needs} needs you"),
                            theme::needs_you_style(0),
                        ));
                    }
                }
                if needs_you.contains(&project.id) {
                    spans.push(Span::styled("  ⚑", theme::needs_you_style(0)));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        let list = List::new(items)
            .highlight_symbol("▸ ")
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(theme::cursor_active(false));
        frame.render_stateful_widget(list, body, &mut picker.state);
    }

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " type to filter · ↑↓ move · ⏎ switch · esc cancel",
            Style::new().fg(MUTED),
        ))),
        hint,
    );
}

/// An open disk-reclaim prompt (ENG-540): the unreferenced mirrors the user may
/// free, with a cursor. A snapshot taken (a quick filesystem walk) when opened —
/// closing and reopening rescans. Owned by `App`, fed keys like the other modals.
pub(crate) struct ReclaimPrompt {
    mirrors: Vec<crate::mirror::ReclaimableMirror>,
    state: ListState,
}

impl ReclaimPrompt {
    pub(crate) fn new(mirrors: Vec<crate::mirror::ReclaimableMirror>) -> Self {
        let mut state = ListState::default();
        if !mirrors.is_empty() {
            state.select(Some(0));
        }
        ReclaimPrompt { mirrors, state }
    }

    pub(crate) fn move_by(&mut self, delta: i32) {
        move_state(&mut self.state, self.mirrors.len(), delta);
    }

    /// The cursor's mirror (cloned so the caller can act after dropping the borrow).
    pub(crate) fn selected(&self) -> Option<crate::mirror::ReclaimableMirror> {
        self.state
            .selected()
            .and_then(|i| self.mirrors.get(i))
            .cloned()
    }
}

/// Human-readable byte size for the reclaim prompt — `842 MB`, `1.3 GB`, etc.
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = n as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} {}", UNITS[0])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// Render the disk-reclaim prompt as a centered modal (ENG-540), mirroring
/// [`render_overlay`]'s framing. Each row is an unreferenced mirror and its size.
pub(crate) fn render_reclaim_overlay(prompt: &mut ReclaimPrompt, frame: &mut Frame, full: Rect) {
    let width = (u32::from(full.width) * 6 / 10)
        .clamp(u32::from(50.min(full.width)), u32::from(full.width)) as u16;
    let height = (u32::from(full.height) * 6 / 10)
        .clamp(u32::from(8.min(full.height)), u32::from(full.height)) as u16;
    let area = Rect {
        x: full.x + (full.width.saturating_sub(width)) / 2,
        y: full.y + (full.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(AMBER_400))
        .title(Line::from(Span::styled(
            " RECLAIM DISK ",
            Style::new().fg(GREEN_100).bold(),
        )));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [intro, body, hint] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(inner);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " unreferenced mirrors — safe to free (re-clone on next use)",
            Style::new().fg(MUTED),
        ))),
        intro,
    );

    let items: Vec<ListItem> = prompt
        .mirrors
        .iter()
        .map(|m| {
            ListItem::new(Line::from(vec![
                Span::styled(m.handle.clone(), Style::new().fg(INK)),
                Span::styled(
                    format!("  ({})", human_bytes(m.size_bytes)),
                    Style::new().fg(AMBER_400),
                ),
            ]))
        })
        .collect();
    let list = List::new(items)
        .highlight_symbol("▸ ")
        .highlight_spacing(HighlightSpacing::Always)
        .highlight_style(theme::cursor_active(false));
    frame.render_stateful_widget(list, body, &mut prompt.state);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " ↑↓ move · ⏎/d reclaim · esc close",
            Style::new().fg(MUTED),
        ))),
        hint,
    );
}

/// Render the repo multi-select as a centered modal (ENG-536's up-front select),
/// mirroring [`render_overlay`]'s framing. Each row shows a checkbox glyph, the repo
/// handle, and a `(primary)` / `(local)` tag.
pub(crate) fn render_repo_overlay(picker: &mut RepoPicker, frame: &mut Frame, full: Rect) {
    let width = (u32::from(full.width) * 6 / 10)
        .clamp(u32::from(40.min(full.width)), u32::from(full.width)) as u16;
    let height = (u32::from(full.height) * 6 / 10)
        .clamp(u32::from(8.min(full.height)), u32::from(full.height)) as u16;
    let area = Rect {
        x: full.x + (full.width.saturating_sub(width)) / 2,
        y: full.y + (full.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(GREEN_500))
        .title(Line::from(Span::styled(
            " SELECT REPOS ",
            Style::new().fg(GREEN_100).bold(),
        )));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [body, hint] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);

    // The "add another repo" sub-list owns the body while it's open (CF-20): a text field
    // for typing a brand-new repo (URL/path) above a list of registered repos the project
    // doesn't yet list. Typing targets the field; an empty field picks the highlighted row.
    if let Some(add) = picker.adding.as_mut() {
        let [field, list_area] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(body);

        // The typed-add field: a prompt, the live input, and a cursor bar. A long URL/path
        // is windowed to its TAIL (a leading "…") so the caret end — what the human is
        // actively typing, and where the repo name sits — stays on screen instead of
        // scrolling off the right edge. Char-based so a multi-byte path never splits a char.
        let budget = usize::from(field.width).saturating_sub(4); // " + " prefix + cursor bar
        let chars: Vec<char> = add.input.chars().collect();
        let shown: String = if budget > 1 && chars.len() > budget {
            std::iter::once('…')
                .chain(chars[chars.len() - (budget - 1)..].iter().copied())
                .collect()
        } else {
            add.input.clone()
        };
        let field_line = Line::from(vec![
            Span::styled(" + ", Style::new().fg(GREEN_400)),
            Span::styled(shown, Style::new().fg(INK)),
            Span::styled("▏", Style::new().fg(GREEN_500)),
        ]);
        let field_hint = Line::from(Span::styled(
            "   type a URL or local path to add a new repo",
            Style::new().fg(MUTED),
        ));
        frame.render_widget(Paragraph::new(vec![field_line, field_hint]), field);

        if add.rows.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "   (no other registered repos — type one above)",
                    Style::new().fg(MUTED),
                ))),
                list_area,
            );
        } else {
            let items: Vec<ListItem> = add
                .rows
                .iter()
                .map(|repo| {
                    let mut spans = vec![
                        Span::styled("  ", Style::new()),
                        Span::styled(repo.handle.clone(), Style::new().fg(INK)),
                    ];
                    if repo.local {
                        spans.push(Span::styled("  (local)", Style::new().fg(AMBER_400)));
                    }
                    ListItem::new(Line::from(spans))
                })
                .collect();
            let list = List::new(items)
                .highlight_symbol("▸ ")
                .highlight_spacing(HighlightSpacing::Always)
                .highlight_style(theme::cursor_active(false));
            frame.render_stateful_widget(list, list_area, &mut add.state);
        }

        // The hint adapts to what's actually possible: typed text → ⏎ adds it; otherwise an
        // empty pick-list (every registered repo already listed) → only typing is left, so
        // don't dangle a "↑↓ pick" that points at nothing; else → pick the highlighted row.
        let action = if !add.input.trim().is_empty() {
            " ⏎ add typed repo · esc back"
        } else if add.rows.is_empty() {
            " type a URL/path above · esc back"
        } else {
            " ↑↓ pick · ⏎ add · esc back"
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(action, Style::new().fg(MUTED)))),
            hint,
        );
        return;
    }

    let items: Vec<ListItem> = picker
        .rows
        .iter()
        .zip(&picker.checked)
        .map(|(repo, &checked)| {
            let (glyph, glyph_style) = theme::repo_check(checked);
            let mut spans = vec![
                Span::styled(format!("{glyph} "), glyph_style),
                Span::styled(repo.handle.clone(), Style::new().fg(INK)),
            ];
            if repo.primary {
                spans.push(Span::styled("  (primary)", Style::new().fg(GREEN_400)));
            } else if repo.local {
                spans.push(Span::styled("  (local)", Style::new().fg(AMBER_400)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let list = List::new(items)
        .highlight_symbol("▸ ")
        .highlight_spacing(HighlightSpacing::Always)
        .highlight_style(theme::cursor_active(false));
    frame.render_stateful_widget(list, body, &mut picker.state);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " space toggles · a add repo · ↑↓ move · ⏎ launch · esc cancel",
            Style::new().fg(MUTED),
        ))),
        hint,
    );
}

/// Run the picker until the user selects a project (`Some`) or quits (`None`).
/// Manages its own alternate screen; the caller restores nothing.
pub fn pick(
    projects: Vec<ProjectRef>,
    needs_you: &std::collections::HashSet<String>,
) -> io::Result<Option<ProjectRef>> {
    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut Picker::new(projects), needs_you);
    ratatui::restore();
    result
}

fn run(
    terminal: &mut DefaultTerminal,
    picker: &mut Picker,
    needs_you: &std::collections::HashSet<String>,
) -> io::Result<Option<ProjectRef>> {
    loop {
        terminal.draw(|frame| draw(picker, frame, needs_you))?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return Ok(None),
            KeyCode::Char('c') if ctrl => return Ok(None),
            KeyCode::Enter => {
                if let Some(project) = picker.selected() {
                    return Ok(Some(project));
                }
            }
            KeyCode::Down => picker.move_by(1),
            KeyCode::Up => picker.move_by(-1),
            KeyCode::Backspace => {
                picker.query.pop();
                picker.refilter();
            }
            KeyCode::Char(c) => {
                picker.query.push(c);
                picker.refilter();
            }
            _ => {}
        }
    }
}

fn draw(picker: &mut Picker, frame: &mut Frame, needs_you: &std::collections::HashSet<String>) {
    let [header, body, hint] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    // Header: title on the left, the live filter box on the right (split so they
    // never overwrite each other on a narrow terminal).
    let title = Line::from(vec![
        Span::styled("  lindep ", Style::new().fg(GREEN_500).bold()),
        Span::styled("· select a project  ", Style::new().fg(GREEN_100)),
        Span::styled(
            format!("{}/{}", picker.order.len(), picker.projects.len()),
            Style::new().fg(MUTED),
        ),
    ]);
    let query = Line::from(vec![
        Span::styled("/", Style::new().fg(GREEN_400)),
        Span::styled(picker.query.clone(), Style::new().fg(INK)),
        Span::styled("▏", Style::new().fg(GREEN_500)),
        Span::raw("  "),
    ]);
    let qw = u16::try_from(query.width()).unwrap_or(u16::MAX);
    let [hl, hr] = Layout::horizontal([Constraint::Min(0), Constraint::Length(qw)]).areas(header);
    frame.render_widget(Paragraph::new(title), hl);
    frame.render_widget(Paragraph::new(query).alignment(Alignment::Right), hr);

    let project_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(GREEN_500))
        .title(Line::from(Span::styled(
            " PROJECTS ",
            Style::new().fg(GREEN_100).bold(),
        )));
    let project_inner = project_block.inner(body);
    frame.render_widget(project_block, body);
    if picker.order.is_empty() {
        frame.render_widget(
            Paragraph::new(empty_picker_line(&picker.query, picker.projects.len())),
            project_inner,
        );
    } else {
        let items: Vec<ListItem> = picker
            .order
            .iter()
            .map(|&i| {
                let project = &picker.projects[i];
                let mut spans = vec![Span::styled(project.name.clone(), Style::new().fg(INK))];
                // The same ⚑ the in-cockpit switcher shows (render_overlay), now at
                // launch too (ENG-562) — see which project wants you before you pick.
                if needs_you.contains(&project.id) {
                    spans.push(Span::styled("  ⚑", theme::needs_you_style(0)));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        let list = List::new(items)
            .highlight_symbol("▸ ")
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(theme::cursor_active(false));
        frame.render_stateful_widget(list, project_inner, &mut picker.state);
    }

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " type to filter · ↑↓ move · ⏎ open · esc quit",
            Style::new().fg(MUTED),
        )))
        .style(Style::new().bg(WELL)),
        hint,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projects() -> Vec<ProjectRef> {
        ["Inference Platform", "Billing", "Infra"]
            .iter()
            .enumerate()
            .map(|(i, n)| ProjectRef {
                id: i.to_string(),
                name: (*n).to_string(),
            })
            .collect()
    }

    #[test]
    fn sorts_and_filters_case_insensitively() {
        let mut p = Picker::new(projects());
        assert_eq!(p.projects[0].name, "Billing"); // sorted
        p.query = "inf".into();
        p.refilter();
        assert_eq!(p.order.len(), 2); // Infra, Inference Platform
        assert!(p.selected().is_some());
    }

    #[test]
    fn no_match_clears_selection_and_navigation_is_safe() {
        let mut p = Picker::new(projects());
        p.query = "zzz".into();
        p.refilter();
        assert!(p.order.is_empty());
        assert!(p.selected().is_none());
        p.move_by(1); // must not panic
    }

    #[test]
    fn movement_wraps() {
        let mut p = Picker::new(projects());
        assert_eq!(p.state.selected(), Some(0));
        p.move_by(-1);
        assert_eq!(p.state.selected(), Some(2)); // wrapped to last
    }

    #[test]
    fn renders_without_panic_and_flags_a_needy_project() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut p = Picker::new(projects());
        // Billing (id 1) has an agent waiting — the startup picker flags it with
        // the same ⚑ as the in-cockpit switcher (ENG-562 picker symmetry).
        let needs = std::collections::HashSet::from(["1".to_string()]);
        let mut term = Terminal::new(TestBackend::new(80, 12)).unwrap();
        term.draw(|f| draw(&mut p, f, &needs)).unwrap();
        let out = term.backend().to_string();
        assert!(out.contains("select a project"));
        assert!(out.contains("Billing"));
        assert!(out.contains('⚑'), "the needy project is flagged at launch");
    }

    fn repo_choices() -> Vec<RepoChoice> {
        vec![
            RepoChoice {
                handle: "lindep".into(),
                local: false,
                primary: true,
            },
            RepoChoice {
                handle: "shared-proto".into(),
                local: false,
                primary: false,
            },
            RepoChoice {
                handle: "scratch".into(),
                local: true,
                primary: false,
            },
        ]
    }

    #[test]
    fn the_repo_picker_pre_checks_the_primary_and_toggles_the_rest() {
        let mut p = RepoPicker::new(repo_choices());
        // Only the primary is checked up front.
        assert_eq!(p.selected_handles(), vec!["lindep"]);
        // The cursor starts on the primary; toggling it is a no-op (always on).
        p.toggle();
        assert_eq!(p.selected_handles(), vec!["lindep"]);
        // Move to shared-proto and check it.
        p.move_by(1);
        p.toggle();
        assert_eq!(p.selected_handles(), vec!["lindep", "shared-proto"]);
        // Toggle it back off.
        p.toggle();
        assert_eq!(p.selected_handles(), vec!["lindep"]);
    }

    #[test]
    fn add_list_offers_only_repos_the_project_doesnt_already_list() {
        // CF-20: rows are api(primary)+web; the registry also has `shared` and a
        // local-only `scratch`. "add another repo" must offer only the two NOT already
        // candidates, and picking one appends it checked (so the launch spans it).
        let mut p = RepoPicker::new(vec![repo_named("api", true), repo_named("web", false)]);
        let registered = vec![
            RepoChoice { handle: "api".into(), local: false, primary: false },
            RepoChoice { handle: "web".into(), local: false, primary: false },
            RepoChoice { handle: "shared".into(), local: false, primary: false },
            RepoChoice { handle: "scratch".into(), local: true, primary: false },
        ];
        p.open_add(&registered);
        assert!(p.is_adding());
        let offered: Vec<&str> = p
            .adding
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .map(|r| r.handle.as_str())
            .collect();
        assert_eq!(offered, vec!["shared", "scratch"], "only the non-candidates");

        // Move to `scratch` and add it — it lands checked, after the candidates.
        p.move_by(1);
        p.confirm_add();
        assert!(!p.is_adding(), "confirming closes the sub-list");
        assert_eq!(p.selected_handles(), vec!["api", "scratch"]);
    }

    #[test]
    fn open_add_still_opens_with_an_empty_pick_list_for_the_typed_field() {
        // CF-20 typed add: even when every registered repo is already a candidate, the
        // add-list opens — it hosts the text field, the only path to a repo the registry
        // doesn't know yet. The pick-list is just empty (nothing to highlight).
        let mut p = RepoPicker::new(vec![repo_named("api", true), repo_named("web", false)]);
        let registered = vec![
            RepoChoice { handle: "api".into(), local: false, primary: false },
            RepoChoice { handle: "web".into(), local: false, primary: false },
        ];
        p.open_add(&registered);
        assert!(p.is_adding(), "opens for the typed field");
        assert!(
            p.adding.as_ref().unwrap().rows.is_empty(),
            "but offers no rows to pick"
        );

        // Type a URL and it surfaces as the input the caller resolves; committing a
        // resolved choice appends it checked.
        for c in "git@github.com:zaplar/new".chars() {
            p.add_input_push(c);
        }
        assert_eq!(p.add_input(), "git@github.com:zaplar/new");
        p.push_checked_and_close(RepoChoice { handle: "new".into(), local: false, primary: false });
        assert!(!p.is_adding(), "committing closes the sub-list");
        assert_eq!(p.selected_handles(), vec!["api", "new"]);
    }

    #[test]
    fn cancel_add_adds_nothing() {
        let mut p = RepoPicker::new(vec![repo_named("api", true)]);
        let registered = vec![RepoChoice { handle: "web".into(), local: false, primary: false }];
        p.open_add(&registered);
        assert!(p.is_adding());
        p.cancel_add();
        assert!(!p.is_adding());
        assert_eq!(p.selected_handles(), vec!["api"], "backing out adds no repo");
    }

    fn repo_named(handle: &str, primary: bool) -> RepoChoice {
        RepoChoice { handle: handle.into(), local: false, primary }
    }

    #[test]
    fn the_repo_select_overlay_renders_with_checkbox_and_tags() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut p = RepoPicker::new(repo_choices());
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| render_repo_overlay(&mut p, f, f.area()))
            .unwrap();
        let out = term.backend().to_string();
        assert!(out.contains("SELECT REPOS"));
        assert!(out.contains("lindep"));
        assert!(out.contains("(primary)"));
        assert!(out.contains("(local)"), "a local-only repo is tagged");
    }

    #[test]
    fn the_add_list_overlay_renders_the_typed_field_and_remaining_repos() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        // api is already a candidate; shared/scratch are the addable registered repos.
        let mut p = RepoPicker::new(vec![repo_named("api", true)]);
        p.open_add(&[
            RepoChoice { handle: "api".into(), local: false, primary: false },
            RepoChoice { handle: "shared".into(), local: false, primary: false },
        ]);
        for c in "git@host/new".chars() {
            p.add_input_push(c);
        }
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| render_repo_overlay(&mut p, f, f.area()))
            .unwrap();
        let out = term.backend().to_string();
        assert!(out.contains("git@host/new"), "the typed input is shown");
        assert!(
            out.contains("add a new repo"),
            "the typed-add prompt is shown"
        );
        assert!(out.contains("shared"), "an addable registered repo is listed");
        assert!(
            out.contains("add typed repo"),
            "with text typed, the hint offers to add it"
        );
    }

    #[test]
    fn human_bytes_scales_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert!(human_bytes(900 * 1024 * 1024).ends_with("MB"));
        assert!(human_bytes(3 * 1024 * 1024 * 1024).ends_with("GB"));
    }

    #[test]
    fn the_reclaim_overlay_lists_mirrors_with_sizes() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut p = ReclaimPrompt::new(vec![crate::mirror::ReclaimableMirror {
            handle: "core".into(),
            size_bytes: 842 * 1024 * 1024,
        }]);
        assert_eq!(p.selected().unwrap().handle, "core");
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| render_reclaim_overlay(&mut p, f, f.area()))
            .unwrap();
        let out = term.backend().to_string();
        assert!(out.contains("RECLAIM DISK"));
        assert!(out.contains("core"));
        assert!(out.contains("MB"), "a size is shown");
    }

    #[test]
    fn the_switch_overlay_renders_without_panic() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut p = Picker::new(projects());
        let needs = std::collections::HashSet::from(["1".to_string()]); // Billing (id 1)
        // Per-project counts: Billing (id 1) has 3 agents, 1 needing you.
        let counts = std::collections::HashMap::from([("1".to_string(), (3usize, 1usize))]);
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| render_overlay(&mut p, f, f.area(), &needs, &counts))
            .unwrap();
        let out = term.backend().to_string();
        assert!(out.contains("SWITCH PROJECT"));
        assert!(out.contains("Billing"));
        assert!(out.contains('⚑'), "a project that needs you is flagged");
        assert!(out.contains("3 agents"), "per-project agent count shows");
    }
}
