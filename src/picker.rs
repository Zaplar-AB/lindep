//! Interactive project picker — the entry screen shown when no project is named
//! on the command line. Type to filter, arrows to move, Enter to open. This
//! sidesteps shell-quoting project names that contain spaces.

use std::io;

use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, HighlightSpacing, List, ListItem, ListState, Paragraph};

use crate::linear::ProjectRef;
use crate::theme::{self, *};

struct Picker {
    projects: Vec<ProjectRef>,
    order: Vec<usize>, // indices into `projects` that match the filter
    state: ListState,
    query: String,
}

impl Picker {
    fn new(mut projects: Vec<ProjectRef>) -> Self {
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

    fn refilter(&mut self) {
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

    fn move_by(&mut self, delta: i32) {
        if self.order.is_empty() {
            return;
        }
        let n = self.order.len() as i32;
        let cur = self.state.selected().unwrap_or(0) as i32;
        self.state
            .select(Some((cur + delta).rem_euclid(n) as usize));
    }

    fn selected(&self) -> Option<ProjectRef> {
        self.state
            .selected()
            .and_then(|i| self.order.get(i))
            .map(|&idx| self.projects[idx].clone())
    }
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
}
