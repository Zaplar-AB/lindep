//! Interactive project picker — the entry screen shown when no project is named
//! on the command line. Type to filter, arrows to move, Enter to open. This
//! sidesteps shell-quoting project names that contain spaces.

use std::io;

use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph,
};

use crate::linear::ProjectRef;
use crate::theme::{self, *};

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
        if self.order.is_empty() {
            return;
        }
        let n = self.order.len() as i32;
        let cur = self.state.selected().unwrap_or(0) as i32;
        self.state
            .select(Some((cur + delta).rem_euclid(n) as usize));
    }

    pub(crate) fn selected(&self) -> Option<ProjectRef> {
        self.state
            .selected()
            .and_then(|i| self.order.get(i))
            .map(|&idx| self.projects[idx].clone())
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

    // A project with an agent that needs you carries a breathing ⚑ — so a
    // backgrounded prompt is visible right where you'd switch to handle it.
    let items: Vec<ListItem> = picker
        .order
        .iter()
        .map(|&i| {
            let project = &picker.projects[i];
            let mut spans = vec![Span::styled(project.name.clone(), Style::new().fg(INK))];
            if needs_you.contains(&project.id) {
                // Steady (not breathing): the switcher is a modal and a
                // backgrounded needs-you doesn't drive the animation tick, so an
                // animated flag here would never actually repaint.
                spans.push(Span::styled("  ⚑", Style::new().fg(AMBER_400).bold()));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let list = List::new(items)
        .highlight_symbol("▸ ")
        .highlight_spacing(HighlightSpacing::Always)
        .highlight_style(theme::cursor_active());
    frame.render_stateful_widget(list, body, &mut picker.state);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " type to filter · ↑↓ move · ⏎ switch · esc cancel",
            Style::new().fg(MUTED),
        ))),
        hint,
    );
}

/// Run the picker until the user selects a project (`Some`) or quits (`None`).
/// Manages its own alternate screen; the caller restores nothing.
pub fn pick(projects: Vec<ProjectRef>) -> io::Result<Option<ProjectRef>> {
    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut Picker::new(projects));
    ratatui::restore();
    result
}

fn run(terminal: &mut DefaultTerminal, picker: &mut Picker) -> io::Result<Option<ProjectRef>> {
    loop {
        terminal.draw(|frame| draw(picker, frame))?;
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

fn draw(picker: &mut Picker, frame: &mut Frame) {
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

    let items: Vec<ListItem> = picker
        .order
        .iter()
        .map(|&i| {
            ListItem::new(Line::from(Span::styled(
                picker.projects[i].name.clone(),
                Style::new().fg(INK),
            )))
        })
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(GREEN_500))
                .title(Line::from(Span::styled(
                    " PROJECTS ",
                    Style::new().fg(GREEN_100).bold(),
                ))),
        )
        .highlight_symbol("▸ ")
        .highlight_spacing(HighlightSpacing::Always)
        .highlight_style(theme::cursor_active());
    frame.render_stateful_widget(list, body, &mut picker.state);

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
    fn renders_without_panic() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut p = Picker::new(projects());
        let mut term = Terminal::new(TestBackend::new(80, 12)).unwrap();
        term.draw(|f| draw(&mut p, f)).unwrap();
        let out = term.backend().to_string();
        assert!(out.contains("select a project"));
        assert!(out.contains("Billing"));
    }

    #[test]
    fn the_switch_overlay_renders_without_panic() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut p = Picker::new(projects());
        let needs = std::collections::HashSet::from(["1".to_string()]); // Billing (id 1)
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| render_overlay(&mut p, f, f.area(), &needs))
            .unwrap();
        let out = term.backend().to_string();
        assert!(out.contains("SWITCH PROJECT"));
        assert!(out.contains("Billing"));
        assert!(out.contains('⚑'), "a project that needs you is flagged");
    }
}
