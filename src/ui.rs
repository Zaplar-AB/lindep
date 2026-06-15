//! All terminal rendering for the cockpit. Reads [`App`] state and paints the
//! window strip (the Spine, live Agent PTYs, and Deps trees) plus the header,
//! detail bar and help overlay. No state mutation beyond the `ListState` scroll
//! offsets and the per-window `preview_size` ratatui/PTY resize bookkeeping
//! needs — the documented render-mutation contract.

use std::sync::Arc;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph,
};
use tui_term::widget::PseudoTerminal;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Flash, LeftView};
use crate::backend::Lifecycle;
use crate::keymap::Action;
use crate::layout;
use crate::model::{Direction, Graph, Status};
use crate::session::AgentStatus;
use crate::theme::{self, *};
use crate::window::{CoinMode, DepsSide, LayoutMode, NodeKind, TreeRow, WindowId, WindowKind};

const MAX_TITLE: usize = 64;
/// Below this inner width a `claude` PTY preview is unreadable, so a window
/// narrower than this (a thin mosaic tile, or a letterboxed lone column on a
/// cramped terminal) collapses to a one-line summary instead of a garbled grid.
const MIN_PTY_W: u16 = 24;

pub fn draw(app: &mut App, frame: &mut Frame) {
    let [header, body, detail, hints] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    render_header(app, frame, header);
    render_strip(app, frame, body);
    render_detail(app, frame, detail);
    render_hints(app, frame, hints);

    if app.show_help {
        render_help(app, frame);
    }
    if app.show_summary {
        render_summary(app, frame);
    }
}

// ── Header ────────────────────────────────────────────────────────────────

fn render_header(app: &App, frame: &mut Frame, area: Rect) {
    let g = &app.graph;
    let mut spans = vec![
        Span::styled("  lindep ", Style::new().fg(GREEN_500).bold()),
        Span::styled("· ", Style::new().fg(BORDER)),
        Span::styled(g.project.clone(), Style::new().fg(GREEN_100).bold()),
        Span::styled("  ", Style::new()),
        Span::styled(format!("{} issues", g.len()), Style::new().fg(MUTED)),
        Span::styled(" · ", Style::new().fg(BORDER)),
        Span::styled(format!("{} edges", g.edge_count()), Style::new().fg(MUTED)),
    ];
    if g.cycle_count() > 0 {
        spans.push(Span::styled(" · ", Style::new().fg(BORDER)));
        spans.push(Span::styled(
            format!("{} cycles ↺", g.cycle_count()),
            Style::new().fg(AMBER_400),
        ));
    }
    let (agents, needs_you) = app.fleet_summary();
    if agents > 0 {
        spans.push(Span::styled(" · ", Style::new().fg(BORDER)));
        spans.push(Span::styled(
            format!("{agents} agent{}", if agents == 1 { "" } else { "s" }),
            Style::new().fg(GREEN_400).bold(),
        ));
        if needs_you > 0 {
            spans.push(Span::styled(
                format!(" · {needs_you} needs you ⚑"),
                theme::needs_you_style(app.frame),
            ));
        }
    }
    // Auto-resume spinner: while docked agents are still coming back, the header
    // breathes a "resuming N…" so the cockpit reads as busy, not stalled.
    if app.resuming_count() > 0 {
        spans.push(Span::styled(" · ", Style::new().fg(BORDER)));
        spans.push(Span::styled(
            format!(
                "{} resuming {}…",
                theme::agent_spinner(app.frame),
                app.resuming_count()
            ),
            Style::new().fg(ORANGE_400).bold(),
        ));
    }

    let right = if app.search_active || !app.search_query.is_empty() {
        Line::from(vec![
            Span::styled("/", Style::new().fg(GREEN_400)),
            Span::styled(app.search_query.clone(), Style::new().fg(INK)),
            Span::styled(
                if app.search_active { "▏" } else { "" },
                Style::new().fg(GREEN_500),
            ),
            Span::styled("  ", Style::new()),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                format!("filter:{} ", app.filter.label()),
                Style::new().fg(MUTED),
            ),
            Span::styled(
                format!("sort:{}  ", app.sort.label()),
                Style::new().fg(MUTED),
            ),
        ])
    };

    let right_w = u16::try_from(right.width()).unwrap_or(u16::MAX);
    let [left, right_area] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(right_w)]).areas(area);
    frame.render_widget(Paragraph::new(Line::from(spans)), left);
    frame.render_widget(
        Paragraph::new(right).alignment(Alignment::Right),
        right_area,
    );
}

// ── The window strip ──────────────────────────────────────────────────────

fn render_strip(app: &mut App, frame: &mut Frame, area: Rect) {
    if area.area() == 0 {
        return;
    }
    let n = app.windows.windows.len();
    let focus = app.windows.focus;
    // The active window represents the selection (its pinned coin, else the
    // preview); it's the big pane when the Spine is focused.
    let active = app.active_index();
    let preview = app.windows.preview_index();

    // Zoom: the big pane fills the whole viewport (hiding the Spine and the rail).
    if app.windows.zoomed {
        let big = layout::rail_big_index(n, focus, active).unwrap_or(focus);
        render_window_at(app, frame, area, big);
        return;
    }

    match app.windows.layout {
        // Mosaic: the Spine pinned left, every non-spine window tiled live in the rest.
        LayoutMode::Mosaic => {
            for p in layout::mosaic(area, n) {
                render_window_at(app, frame, p.rect, p.index);
            }
        }
        // Rail: the Spine, one big pane (focused window, or the active window when
        // the Spine is focused), and a column of compact text cards for every other
        // docked window (never the preview).
        LayoutMode::Rail => {
            let (full, cards) = layout::rail(area, n, focus, active, preview);
            for p in full {
                render_window_at(app, frame, p.rect, p.index);
            }
            for p in cards {
                render_card(app, frame, p.rect, p.index);
            }
        }
    }
}

/// Render the window at `idx` as a full pane (the Spine, a live PTY screen, or a
/// dependency body) into `rect`. Used for the rail's big pane, every mosaic tile,
/// and the zoomed pane.
fn render_window_at(app: &mut App, frame: &mut Frame, rect: Rect, idx: usize) {
    if idx >= app.windows.windows.len() {
        return;
    }
    let focused = idx == app.windows.focus;
    // Clone the small per-window facts so the render fns can re-borrow `app` for
    // the specific field each needs (PTY backend, deps cursor, list).
    let id = app.windows.windows[idx].id;
    let pinned = app.windows.windows[idx].pinned;
    let kind = app.windows.windows[idx].kind.clone();
    match kind {
        WindowKind::Spine => render_spine(app, frame, rect, focused),
        // A coin renders its current face; `pinned` is the window's real pin flag,
        // so the preview (unpinned) shows no pin chip and reads as transient.
        WindowKind::Coin {
            issue,
            mode: CoinMode::Chat,
        } => render_agent_window(app, frame, rect, id, &issue, focused, pinned),
        WindowKind::Coin {
            mode: CoinMode::Deps,
            ..
        } => render_deps_window(app, frame, rect, idx, focused, pinned),
        WindowKind::Fleet => render_fleet_window(app, frame, rect, focused, pinned),
    }
}

/// Render a compact, text-only status card for a rail window — never a live PTY,
/// so the rail sidesteps tui-term's horizontal-clipping limit and a carded agent
/// never forces a fast poll (idle-quiet). A card is never the focused window (the
/// focused window is always the big pane), so it always paints in its status hue.
fn render_card(app: &App, frame: &mut Frame, rect: Rect, idx: usize) {
    if rect.area() == 0 {
        return;
    }
    let w = &app.windows.windows[idx];
    let pinned = w.pinned;
    let (mut title, hue, breathe, body): (Vec<Span<'static>>, Color, bool, String) = match &w.kind {
        WindowKind::Coin {
            issue,
            mode: CoinMode::Chat,
        } => {
            let status = app.fleet.get(issue).copied();
            let exited = app
                .backends
                .get(issue)
                .is_some_and(|b| matches!(b.status(), Lifecycle::Exited(_)));
            let (hue, label) = theme::window_status_hue(status, exited);
            let (mark, mstyle) = match status {
                Some(s) => theme::agent_marker(s, app.frame),
                None => ("○", Style::new().fg(hue)),
            };
            let key = app
                .graph
                .get(issue)
                .map_or(issue.as_str(), |i| i.key.as_str());
            (
                vec![
                    Span::raw(" "),
                    Span::styled(mark, mstyle),
                    Span::styled(format!(" {key} "), Style::new().fg(INK).bold()),
                ],
                hue,
                status == Some(AgentStatus::NeedsYou),
                label.to_string(),
            )
        }
        WindowKind::Coin {
            issue,
            mode: CoinMode::Deps,
        } => {
            // The current exploration root (re-rooting moves it), else the coin's
            // identity.
            let root = w
                .deps
                .as_ref()
                .map(|c| c.root.clone())
                .unwrap_or_else(|| issue.clone());
            (
                vec![
                    Span::styled(" ◆ ", Style::new().fg(GREEN_500)),
                    Span::styled(format!("{root} "), Style::new().fg(INK).bold()),
                ],
                GREEN_500,
                false,
                "deps".to_string(),
            )
        }
        WindowKind::Fleet => (
            vec![Span::styled(" GRAPH ", Style::new().fg(GREEN_100).bold())],
            GREEN_500,
            false,
            "overview".to_string(),
        ),
        WindowKind::Spine => return, // the Spine is never carded
    };
    if pinned {
        title.push(Span::styled("⊙ ", Style::new().fg(ORANGE_400)));
    }
    let block = window_block(Line::from(title), false, hue, breathe, app.frame);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if inner.area() > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {body}"),
                Style::new().fg(hue),
            ))),
            inner,
        );
    }
}

/// The focused window's border = a steady violet double frame; an unfocused one
/// = a thin frame in its status hue (a needs-you agent breathes).
fn window_block(
    title: Line<'static>,
    focused: bool,
    hue: Color,
    breathe: bool,
    frame: u64,
) -> Block<'static> {
    let (border_type, border_style) = if focused {
        (BorderType::Double, theme::focus_border_style())
    } else if breathe {
        (BorderType::Plain, theme::needs_you_style(frame))
    } else {
        (BorderType::Plain, Style::new().fg(hue))
    };
    // The focused window gets a bright violet focus bar leading its title, so the
    // focus is unmistakable even where a double border is subtle.
    let mut title = title;
    if focused {
        title
            .spans
            .insert(0, Span::styled("▌", Style::new().fg(VIOLET_200)));
    }
    Block::default()
        .borders(Borders::ALL)
        .border_type(border_type)
        .border_style(border_style)
        .title(title)
}

// ── Spine (issue list / agents roster) ─────────────────────────────────────

/// The Spine's tab strip — ISSUES | AGENTS, the active tab lit with its count.
fn left_tabs_title(app: &App) -> Line<'static> {
    let tab = |label: &str, count: usize, active: bool| {
        let style = if active {
            Style::new().fg(GREEN_100).bg(GREEN_700).bold()
        } else {
            Style::new().fg(MUTED)
        };
        Span::styled(format!(" {label} {count} "), style)
    };
    Line::from(vec![
        tab("ISSUES", app.order.len(), app.left_view == LeftView::Issues),
        Span::raw(" "),
        tab("AGENTS", app.fleet.len(), app.left_view == LeftView::Agents),
    ])
}

fn render_spine(app: &mut App, frame: &mut Frame, area: Rect, focused: bool) {
    let block = window_block(left_tabs_title(app), focused, BORDER, false, app.frame);

    match app.left_view {
        LeftView::Issues => {
            let items: Vec<ListItem> = app.order.iter().map(|k| issue_item(app, k)).collect();
            let list = list_widget(items, block, focused);
            frame.render_stateful_widget(list, area, &mut app.list_state);
        }
        LeftView::Agents => {
            let agents = app.agent_order();
            if agents.is_empty() {
                let inner = block.inner(area);
                frame.render_widget(block, area);
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::raw(""),
                        Line::from(Span::styled(
                            "  no agents running",
                            Style::new().fg(MUTED).italic(),
                        )),
                        Line::from(Span::styled(
                            "  Enter on an issue opens one",
                            Style::new().fg(BORDER),
                        )),
                    ]),
                    inner,
                );
                return;
            }
            // The roster highlight tracks the selection (the single source of
            // truth), derived fresh — no second persistent selection to sync.
            let selected = agents.iter().position(|k| *k == app.root);
            let items: Vec<ListItem> = agents.iter().map(|k| issue_item(app, k)).collect();
            let list = list_widget(items, block, focused);
            let mut state = ListState::default();
            state.select(selected);
            frame.render_stateful_widget(list, area, &mut state);
        }
    }
}

fn list_widget<'a>(items: Vec<ListItem<'a>>, block: Block<'a>, active: bool) -> List<'a> {
    List::new(items)
        .block(block)
        .highlight_symbol(if active { "▸ " } else { "  " })
        .highlight_spacing(HighlightSpacing::Always)
        .highlight_style(if active {
            theme::cursor_active()
        } else {
            theme::cursor_idle()
        })
}

// ── Agent windows (live PTY screens) ────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn render_agent_window(
    app: &mut App,
    frame: &mut Frame,
    rect: Rect,
    id: WindowId,
    issue: &str,
    focused: bool,
    pinned: bool,
) {
    if rect.area() == 0 {
        return;
    }
    let status = app.fleet.get(issue).copied();
    let backend = app.backends.get(issue).map(Arc::clone);
    let exited = backend
        .as_ref()
        .is_some_and(|b| matches!(b.status(), Lifecycle::Exited(_)));
    let (hue, label) = theme::window_status_hue(status, exited);
    let breathe = !focused && status == Some(AgentStatus::NeedsYou);

    let (mark, mstyle) = match status {
        Some(s) => theme::agent_marker(s, app.frame),
        None => ("○", Style::new().fg(hue)),
    };
    let key = app.graph.get(issue).map_or(issue, |i| i.key.as_str());
    let mut title = vec![
        Span::raw(" "),
        Span::styled(mark, mstyle),
        Span::styled(format!(" {label}  "), Style::new().fg(hue).bold()),
        Span::styled(format!("{key} "), Style::new().fg(INK).bold()),
    ];
    if pinned {
        title.push(Span::styled("⊙ pin ", Style::new().fg(ORANGE_400)));
    }
    let block = window_block(Line::from(title), focused, hue, breathe, app.frame);
    let pane = block.inner(rect);
    frame.render_widget(block, rect);
    if pane.area() == 0 {
        return;
    }

    // No backend yet (a just-pressed button, or a docked window awaiting its
    // auto-resume): a calm card, never a parser/resize. The render is otherwise a
    // pure function of state, so it must not synthesise a PTY here.
    let Some(backend) = backend else {
        let msg = if status.is_some() {
            format!("  ◌ resuming {key}…")
        } else {
            format!("  ◌ starting agent on {key}…")
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::raw(""),
                Line::from(Span::styled(msg, Style::new().fg(MUTED).italic())),
            ]),
            pane,
        );
        return;
    };

    // Too small for a real preview — collapse to a one-line summary.
    if pane.height < 2 || pane.width < MIN_PTY_W {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {key} · {label}"),
                Style::new().fg(MUTED),
            ))),
            pane,
        );
        return;
    }

    // Reflow a live agent to its window only on a real geometry change (so
    // browsing/scrolling never churns SIGWINCHes). A dead agent keeps its frozen
    // final screen. Keyed by WindowId so zoom's two geometries don't collide.
    let size = (pane.height, pane.width);
    if !exited && app.preview_size.get(&id) != Some(&size) {
        let _ = backend.resize(pane.height, pane.width);
        app.preview_size.insert(id, size);
    }
    if let Ok(parser) = backend.parser().read() {
        frame.render_widget(PseudoTerminal::new(parser.screen()), pane);
    }
}

// ── Deps windows (per-issue dependency tree) ────────────────────────────────

fn render_deps_window(
    app: &mut App,
    frame: &mut Frame,
    rect: Rect,
    idx: usize,
    focused: bool,
    pinned: bool,
) {
    if rect.area() == 0 {
        return;
    }
    // The deps root drives the title; clone what we need so the cursor's
    // ListStates can be borrowed mutably for the stateful tree render.
    let root = app.windows.windows[idx]
        .deps
        .as_ref()
        .map(|c| c.root.clone())
        .unwrap_or_default();
    let status = app.fleet.get(&root).copied();
    let breathe = !focused && status == Some(AgentStatus::NeedsYou);

    let mut title = vec![
        Span::styled(" ◆ ", Style::new().fg(GREEN_500)),
        Span::styled(format!("{root}  "), Style::new().fg(INK).bold()),
    ];
    if let Some(issue) = app.graph.get(&root) {
        title.push(Span::styled(
            truncate(&issue.title, MAX_TITLE.saturating_sub(12)),
            Style::new().fg(MUTED),
        ));
        title.push(Span::raw(" "));
    }
    if let Some(s) = status {
        let (mark, mstyle) = theme::agent_marker(s, app.frame);
        title.push(Span::styled(mark, mstyle));
        title.push(Span::raw(" "));
    }
    if pinned {
        title.push(Span::styled("⊙ pin ", Style::new().fg(ORANGE_400)));
    }
    let block = window_block(Line::from(title), focused, GREEN_500, breathe, app.frame);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if inner.area() == 0 {
        return;
    }

    let up_n = app.graph.direct_count(&root, Direction::Upstream);
    let up_t = app.graph.transitive(&root, Direction::Upstream);
    let down_n = app.graph.direct_count(&root, Direction::Downstream);
    let down_t = app.graph.transitive(&root, Direction::Downstream);

    let [up_head, up_body, down_head, down_body] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);

    frame.render_widget(
        section_header("▲ UPSTREAM", "must finish first", up_n, up_t),
        up_head,
    );
    render_tree(app, frame, up_body, idx, Direction::Upstream, focused);
    frame.render_widget(
        section_header("▼ DOWNSTREAM", "this unblocks", down_n, down_t),
        down_head,
    );
    render_tree(app, frame, down_body, idx, Direction::Downstream, focused);
}

fn render_tree(
    app: &mut App,
    frame: &mut Frame,
    area: Rect,
    idx: usize,
    dir: Direction,
    focused: bool,
) {
    // The active tree is highlighted only when this window is focused *and* it's
    // the side the cursor is on.
    let (rows, active, state): (&[TreeRow], bool, &mut ListState) = {
        let cursor = match app.windows.windows[idx].deps.as_mut() {
            Some(c) => c,
            None => return,
        };
        match dir {
            Direction::Upstream => {
                let active = focused && cursor.side == DepsSide::Up;
                (&cursor.up_rows, active, &mut cursor.up_state)
            }
            Direction::Downstream => {
                let active = focused && cursor.side == DepsSide::Down;
                (&cursor.down_rows, active, &mut cursor.down_state)
            }
        }
    };

    if rows.is_empty() {
        let msg = match dir {
            Direction::Upstream => "  ✓ no blockers — ready to start",
            Direction::Downstream => "  · blocks nothing downstream",
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                msg,
                Style::new().fg(MUTED).italic(),
            ))),
            area,
        );
        return;
    }

    let items: Vec<ListItem> = rows
        .iter()
        .map(|r| ListItem::new(tree_line(&app.graph, r, dir)))
        .collect();
    let list = List::new(items)
        .highlight_symbol(if active { "▸ " } else { "  " })
        .highlight_spacing(HighlightSpacing::Always)
        .highlight_style(if active {
            theme::cursor_active()
        } else {
            theme::cursor_idle()
        });
    frame.render_stateful_widget(list, area, state);
}

// ── Fleet window (the layered overview map) ─────────────────────────────────

fn render_fleet_window(app: &mut App, frame: &mut Frame, rect: Rect, focused: bool, pinned: bool) {
    if rect.area() == 0 {
        return;
    }
    let mut title = vec![Span::styled(
        " GRAPH OVERVIEW ",
        Style::new().fg(GREEN_100).bold(),
    )];
    if pinned {
        title.push(Span::styled("⊙ pin ", Style::new().fg(ORANGE_400)));
    }
    let block = window_block(Line::from(title), focused, GREEN_500, false, app.frame);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if inner.area() == 0 {
        return;
    }

    let g = &app.graph;
    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            " flow: roots (no blockers) ───▶ leaves (block nothing)",
            Style::new().fg(MUTED),
        )),
        Line::raw(""),
    ];

    let bands = g.levels();
    let chips_per_row = ((inner.width.saturating_sub(6)) / 14).max(1) as usize;
    let mut root_line: Option<usize> = None;
    for (level, band) in bands.iter().enumerate() {
        for (row, chunk) in band.chunks(chips_per_row).enumerate() {
            let label = if row == 0 {
                format!(" L{level:<2} ")
            } else {
                "     ".to_string()
            };
            let mut spans = vec![Span::styled(label, Style::new().fg(GREEN_400).bold())];
            for key in chunk {
                let (glyph, color) = status_for(g, key);
                let key_style = if *key == app.root {
                    root_line = Some(lines.len());
                    Style::new().fg(GREEN_100).bg(GREEN_700).bold()
                } else {
                    Style::new().fg(INK)
                };
                spans.push(Span::styled(format!("{glyph} "), Style::new().fg(color)));
                spans.push(Span::styled(key.to_string(), key_style));
                if let Some(status) = app.fleet.get(key) {
                    let (mark, mstyle) = theme::agent_marker(*status, app.frame);
                    spans.push(Span::styled(format!(" {mark}"), mstyle));
                }
                spans.push(Span::raw("  "));
            }
            if row == 0 {
                spans.push(Span::styled(
                    format!("  ({})", band.len()),
                    Style::new().fg(BORDER),
                ));
            }
            lines.push(Line::from(spans));
        }
    }

    if g.cycle_count() > 0 {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            format!(" ── CYCLES ({}) ", g.cycle_count()),
            Style::new().fg(AMBER_400).bold(),
        )));
        for cycle in g.cycles() {
            let path = cycle.join(" → ");
            lines.push(Line::from(vec![
                Span::styled("   ↺  ", Style::new().fg(AMBER_400)),
                Span::styled(
                    truncate(&path, (inner.width as usize).saturating_sub(8).max(1)),
                    Style::new().fg(INK),
                ),
            ]));
        }
    }

    let externals = g.externals();
    if !externals.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            format!(" ── EXTERNAL BLOCKERS ({}) ", externals.len()),
            Style::new().fg(GREEN_400).bold(),
        )));
        for ext in externals {
            let blocks = g.neighbours(&ext.key, Direction::Downstream).join(", ");
            lines.push(Line::from(vec![
                Span::styled("   ⇗  ", Style::new().fg(GREEN_400)),
                Span::styled(format!("{}  ", ext.key), Style::new().fg(GREEN_400).bold()),
                Span::styled(truncate(&ext.title, 32), Style::new().fg(MUTED)),
                Span::styled(
                    format!("  · team {} · blocks {}", ext.team(), blocks),
                    Style::new().fg(BORDER),
                ),
            ]));
        }
    }

    // Scroll to keep the highlighted selection in view as you arrow the spine.
    let height = inner.height as usize;
    let offset = match root_line {
        Some(l) if lines.len() > height => {
            l.saturating_sub(height / 2)
                .min(lines.len().saturating_sub(height)) as u16
        }
        _ => 0,
    };
    frame.render_widget(Paragraph::new(lines).scroll((offset, 0)), inner);
}

// ── Detail bar + hints ───────────────────────────────────────────────────────

fn render_detail(app: &App, frame: &mut Frame, area: Rect) {
    if let Some(msg) = &app.status_msg {
        // A pending kill confirmation is the one destructive prompt, so flag it red.
        let style = if app.kill_confirm.is_some() {
            Style::new().fg(RED_400).bold()
        } else {
            Style::new().fg(AMBER_400)
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(format!(" {msg}"), style))),
            area,
        );
        return;
    }
    let Some(issue) = app.focused_issue() else {
        return;
    };
    let g = &app.graph;
    let (glyph, color) = theme::status_glyph(issue.status);
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(format!("{glyph} "), Style::new().fg(color)),
        Span::styled(format!("{} ", issue.key), Style::new().fg(INK).bold()),
        Span::styled(status_label(issue.status), Style::new().fg(color)),
    ];
    if let Some(a) = &issue.assignee {
        spans.push(Span::styled(format!(" · @{a}"), Style::new().fg(MUTED)));
    }
    spans.push(Span::styled(
        format!(
            " · blocks {} (↓{})",
            g.direct_count(&issue.key, Direction::Downstream),
            g.transitive(&issue.key, Direction::Downstream)
        ),
        Style::new().fg(MUTED),
    ));
    spans.push(Span::styled(
        format!(
            " · blocked-by {} (↑{})",
            g.direct_count(&issue.key, Direction::Upstream),
            g.transitive(&issue.key, Direction::Upstream)
        ),
        Style::new().fg(MUTED),
    ));
    if g.is_blocked(&issue.key) {
        spans.push(Span::styled(" · ⊘ blocked", Style::new().fg(AMBER_400)));
    }
    if g.in_cycle(&issue.key) {
        spans.push(Span::styled(" · ↺ in cycle", Style::new().fg(AMBER_400)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Context-sensitive hint line, built live from the keymap so a rebind shows
/// correctly. The prefix (`Ctrl-a`) leads the verbs; direct keys vary by the
/// focused window's kind.
fn render_hints(app: &App, frame: &mut Frame, area: Rect) {
    let p = app.prefix_label();
    // Remap-driven, like render_help (plan §3 Phase 2: "kills the hints lie") — the
    // shown keys are read live from the keymap so a rebind can't make the footer
    // contradict reality. `vk` = a bare verb key (the prefix is already implied);
    // `dk` = the direct key(s). Only pure-motion glyphs (↑↓ ←→ ⏎ space) stay
    // iconic; the help overlay is the authoritative per-binding reference.
    let vk = |a| app.keymap.verb_key_label(a);
    let dk = |a| app.keymap.label_for(a);
    let text: String = if app.search_active {
        " type to filter · ⏎ accept · esc clear".to_string()
    } else if app.kill_confirm.is_some() {
        " y / ⏎ confirm kill · any other key cancels".to_string()
    } else if app.prefix_armed {
        format!(
            " {p} armed: ←→ focus · {} chat/deps · {} zoom · {} pin · {} close · {} kill · {} rail/mosaic · {} quit · {} help · {p} again → agent",
            vk(Action::ContextToggle),
            vk(Action::ZoomToggle),
            vk(Action::PinWindow),
            vk(Action::CloseWindow),
            vk(Action::KillWindow),
            vk(Action::LayoutToggle),
            vk(Action::Quit),
            vk(Action::ToggleHelp),
        )
    } else {
        match app.windows.focused_kind() {
            WindowKind::Coin {
                mode: CoinMode::Chat,
                ..
            } => format!(
                " keys → agent · {p} escape · {p} {} chat/deps · {p} {} nav · {p} {} close · {p} {} kill",
                vk(Action::ContextToggle),
                vk(Action::FocusNav),
                vk(Action::CloseWindow),
                vk(Action::KillWindow),
            ),
            WindowKind::Coin {
                mode: CoinMode::Deps,
                ..
            } => format!(
                " ↑↓ move · ←→ side · ⏎ re-root · space collapse · {} back · {} chat/deps · {p} {} nav · {} help",
                dk(Action::Back),
                dk(Action::ContextToggle),
                vk(Action::FocusNav),
                dk(Action::ToggleHelp),
            ),
            WindowKind::Fleet => format!(
                " the project map · {p} {} nav · {p} {} close · {} help",
                vk(Action::FocusNav),
                vk(Action::CloseWindow),
                dk(Action::ToggleHelp),
            ),
            WindowKind::Spine => format!(
                " ↑↓ move · ⏎ open agent · {} chat/deps · {} deps · {} map · {} roster · {} needs-you · {} find · {} summary · {p} {} quit · {} help",
                dk(Action::ContextToggle),
                dk(Action::OpenDeps),
                dk(Action::OpenFleet),
                dk(Action::ToggleRoster),
                dk(Action::JumpNeedsYou),
                dk(Action::StartSearch),
                dk(Action::ToggleSummary),
                vk(Action::Quit),
                dk(Action::ToggleHelp),
            ),
        }
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(text, Style::new().fg(MUTED))))
            .style(Style::new().bg(WELL)),
        area,
    );
}

fn render_help(app: &App, frame: &mut Frame) {
    let direct = |action| app.keymap.label_for(action);
    let verb = |action| app.keymap.verb_label(action);

    let rows: Vec<(String, &str)> = vec![
        ("— the spine —".to_string(), ""),
        (
            format!("{} {}", direct(Action::MoveUp), direct(Action::MoveDown)),
            "move the selection",
        ),
        (
            direct(Action::Enter),
            "open / focus an agent on the selection (active window → chat)",
        ),
        (
            direct(Action::ContextToggle),
            "flip the active window: chat ↔ deps (Ctrl-a Tab in a chat)",
        ),
        (
            direct(Action::OpenDeps),
            "dive into the active window's dependency tree",
        ),
        (
            direct(Action::OpenFleet),
            "open the project overview (Fleet) tab",
        ),
        (
            direct(Action::ToggleRoster),
            "flip the spine: issues ↔ agents roster",
        ),
        (
            direct(Action::StartSearch),
            "fuzzy-find issues by id or title",
        ),
        (
            direct(Action::ToggleSummary),
            "summary overlay for the selected issue (any key closes)",
        ),
        (
            format!(
                "{} {}",
                direct(Action::CycleFilter),
                direct(Action::CycleSort)
            ),
            "cycle the filter / sort",
        ),
        (
            direct(Action::JumpNeedsYou),
            "jump to the next agent that needs you",
        ),
        (direct(Action::JumpCycle), "jump through issues on a cycle"),
        ("— a dependency window —".to_string(), ""),
        (
            direct(Action::SwitchSide),
            "switch the active tree (up ↔ down)",
        ),
        (
            direct(Action::Enter),
            "re-root the lens onto the selected node",
        ),
        (
            direct(Action::ToggleCollapse),
            "collapse / expand the subtree",
        ),
        (direct(Action::Back), "back to the previous root"),
        (format!("— windows ({} prefix) —", app.prefix_label()), ""),
        (
            format!("{} {}", verb(Action::FocusLeft), verb(Action::FocusRight)),
            "focus the window left / right",
        ),
        (verb(Action::FocusNav), "jump focus home to the nav (spine)"),
        (
            verb(Action::AttachOrSpawn),
            "open / focus an agent (from any window)",
        ),
        (
            verb(Action::ZoomToggle),
            "zoom the focused window (non-destructive)",
        ),
        (
            verb(Action::PinWindow),
            "pin = graduate the preview coin to a permanent tab",
        ),
        (
            verb(Action::CloseWindow),
            "close = undock a tab (an agent keeps running)",
        ),
        (
            verb(Action::KillWindow),
            "kill the focused agent (confirmed)",
        ),
        (
            verb(Action::LayoutToggle),
            "force rail ⇄ mosaic (auto by coin count)",
        ),
        (verb(Action::Quit), "quit the cockpit"),
    ];

    let area = centered_rect(78, rows.len() as u16 + 7, frame.area());
    frame.render_widget(Clear, area);

    let mut lines = vec![
        Line::from(Span::styled(" Keys", Style::new().fg(GREEN_100).bold())),
        Line::raw(""),
    ];
    for (key, desc) in &rows {
        if desc.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("  {key}"),
                Style::new().fg(MUTED),
            )));
        } else {
            lines.push(Line::from(vec![
                Span::styled(format!("  {key:<18}"), Style::new().fg(GREEN_400).bold()),
                Span::styled(*desc, Style::new().fg(INK)),
            ]));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "  rebind any of these in ~/.config/lindep/config.toml  [keys] / [verbs]",
        Style::new().fg(MUTED),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(GREEN_600))
        .title(Line::from(Span::styled(
            " lindep ",
            Style::new().fg(GREEN_500).bold(),
        )));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// The issue-summary overlay (`i`): a dismissable, at-a-glance card for the
/// selected (or focused) issue — its status/priority/assignee/team, blocked/cycle
/// flags, and its direct blockers + blocked work with their statuses. Pure read of
/// the local graph; no network. Any key closes it (see `App::on_key`).
fn render_summary(app: &App, frame: &mut Frame) {
    let Some(key) = app.detail_key() else {
        return;
    };
    let g = &app.graph;
    let Some(issue) = g.get(key) else {
        return;
    };

    let (glyph, color) = theme::status_glyph(issue.status);
    let (pmark, pcolor) = theme::priority_marker(issue.priority);
    let mut lines = vec![
        Line::from(vec![
            Span::styled(format!(" {glyph} "), Style::new().fg(color)),
            Span::styled(format!("{} ", issue.key), Style::new().fg(INK).bold()),
            Span::styled(status_label(issue.status), Style::new().fg(color)),
            Span::styled(format!("   {pmark}"), Style::new().fg(pcolor)),
        ]),
        Line::from(Span::styled(
            format!("  {}", issue.title),
            Style::new().fg(GREEN_100).bold(),
        )),
        Line::raw(""),
    ];
    let mut meta = vec![Span::styled(
        format!("  team {}", issue.team()),
        Style::new().fg(MUTED),
    )];
    if let Some(a) = &issue.assignee {
        meta.push(Span::styled(format!("  · @{a}"), Style::new().fg(MUTED)));
    }
    if g.is_blocked(key) {
        meta.push(Span::styled("  · ⊘ blocked", Style::new().fg(AMBER_400)));
    }
    if g.in_cycle(key) {
        meta.push(Span::styled("  · ↺ in cycle", Style::new().fg(AMBER_400)));
    }
    lines.push(Line::from(meta));
    lines.push(Line::raw(""));

    for (dir, label, empty) in [
        (
            Direction::Upstream,
            "▲ BLOCKED BY",
            "✓ nothing — ready to start",
        ),
        (
            Direction::Downstream,
            "▼ BLOCKS",
            "· blocks nothing downstream",
        ),
    ] {
        let (direct, total) = (g.direct_count(key, dir), g.transitive(key, dir));
        lines.push(Line::from(Span::styled(
            format!(" {label}  ({direct} direct · {total} total)"),
            Style::new().fg(GREEN_400).bold(),
        )));
        let neighbours = g.neighbours(key, dir);
        if neighbours.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("   {empty}"),
                Style::new().fg(MUTED).italic(),
            )));
        } else {
            for nk in neighbours {
                let (ng, nc) = status_for(g, nk);
                let mut spans = vec![
                    Span::styled(format!("   {ng} "), Style::new().fg(nc)),
                    Span::styled(format!("{nk:<10} "), Style::new().fg(INK).bold()),
                ];
                if let Some(ni) = g.get(nk) {
                    spans.push(Span::styled(
                        truncate(&ni.title, 44),
                        Style::new().fg(MUTED),
                    ));
                }
                lines.push(Line::from(spans));
            }
        }
        lines.push(Line::raw(""));
    }

    let area = centered_rect(78, lines.len() as u16 + 2, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(GREEN_600))
        .title(Line::from(Span::styled(
            " issue summary · any key to close ",
            Style::new().fg(GREEN_500).bold(),
        )));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

// ── Line builders ─────────────────────────────────────────────────────────

/// One issue row for the spine list / roster: gutter · status · priority · KEY ·
/// title (· blocked · cycle · animated agent marker).
fn issue_line<'a>(
    graph: &Graph,
    key: &str,
    agent: Option<AgentStatus>,
    frame: u64,
    flash: Option<Flash>,
) -> Line<'a> {
    let Some(issue) = graph.get(key) else {
        return Line::from(key.to_string());
    };
    let (glyph, color) = theme::status_glyph(issue.status);
    let (pmark, pcolor) = theme::priority_marker(issue.priority);

    let gutter = match agent {
        Some(status) => Span::styled("▎", Style::new().fg(theme::agent_glyph(status).1)),
        None => Span::raw(" "),
    };
    let key_style = match flash {
        Some(Flash::Launched) => Style::new().fg(INK).bg(GREEN_700).bold(),
        Some(Flash::Finished) => Style::new().fg(GREEN_100).bg(STATUS_600).bold(),
        None => Style::new().fg(INK).bold(),
    };
    let mut spans = vec![
        gutter,
        Span::styled(format!("{glyph} "), Style::new().fg(color)),
        Span::styled(format!("{pmark} "), Style::new().fg(pcolor)),
        Span::styled(format!("{:<8} ", issue.key), key_style),
        Span::styled(truncate(&issue.title, MAX_TITLE), Style::new().fg(MUTED)),
    ];
    if graph.is_blocked(key) {
        spans.push(Span::styled(" ⊘", Style::new().fg(AMBER_400)));
    }
    if graph.in_cycle(key) {
        spans.push(Span::styled(" ↺", Style::new().fg(AMBER_400)));
    }
    if let Some(status) = agent {
        let (mark, mstyle) = theme::agent_marker(status, frame);
        spans.push(Span::raw(" "));
        spans.push(Span::styled(mark, mstyle));
    }
    Line::from(spans)
}

/// One issue row as a list item, carrying the whole-row status tint on the
/// *item* style (so it spans the full row including the highlight gutter).
fn issue_item<'a>(app: &App, key: &str) -> ListItem<'a> {
    let flash = app
        .flash
        .get(key)
        .and_then(|&(kind, until)| (app.frame < until).then_some(kind));
    let status = app.fleet.get(key).copied();
    let item = ListItem::new(issue_line(&app.graph, key, status, app.frame, flash));
    match status {
        Some(s) => item.style(Style::new().bg(theme::agent_row_bg(s, app.frame))),
        None => item,
    }
}

/// One row of a dependency tree, including its box-drawing prefix and any
/// cycle / ref / external annotation.
fn tree_line<'a>(graph: &Graph, row: &TreeRow, dir: Direction) -> Line<'a> {
    let mut spans = vec![Span::styled(row.prefix.clone(), Style::new().fg(BORDER))];

    match row.kind {
        NodeKind::External => {
            spans.push(Span::styled("⇗ ", Style::new().fg(GREEN_400)));
            spans.push(Span::styled(
                format!("{:<8} ", row.key),
                Style::new().fg(GREEN_400).bold(),
            ));
            let title = graph.get(&row.key).map(|i| i.title.as_str()).unwrap_or("");
            spans.push(Span::styled(truncate(title, 40), Style::new().fg(MUTED)));
            spans.push(Span::styled(" [ext]", Style::new().fg(BORDER)));
            return Line::from(spans);
        }
        NodeKind::Cycle => {
            spans.push(Span::styled("↺ ", Style::new().fg(AMBER_400)));
            spans.push(Span::styled(
                format!("{:<8} ", row.key),
                Style::new().fg(AMBER_400).bold(),
            ));
            spans.push(Span::styled(
                "cycle — back-edge",
                Style::new().fg(AMBER_400),
            ));
            return Line::from(spans);
        }
        NodeKind::Ref => {
            let (glyph, color) = status_for(graph, &row.key);
            spans.push(Span::styled(format!("{glyph} "), Style::new().fg(color)));
            spans.push(Span::styled(
                format!("{:<8} ", row.key),
                Style::new().fg(MUTED).add_modifier(Modifier::DIM),
            ));
            spans.push(Span::styled("↗ shown above", Style::new().fg(BORDER)));
            return Line::from(spans);
        }
        NodeKind::Normal => {}
    }

    let issue = graph.get(&row.key);
    let (glyph, color) = status_for(graph, &row.key);
    let (pmark, pcolor) = match issue {
        Some(i) => theme::priority_marker(i.priority),
        None => (" ", MUTED),
    };
    spans.push(Span::styled(format!("{glyph} "), Style::new().fg(color)));
    spans.push(Span::styled(format!("{pmark} "), Style::new().fg(pcolor)));
    spans.push(Span::styled(
        format!("{:<8} ", row.key),
        Style::new().fg(INK).bold(),
    ));
    if let Some(i) = issue {
        spans.push(Span::styled(truncate(&i.title, 44), Style::new().fg(MUTED)));
    }
    if graph.in_cycle(&row.key) {
        spans.push(Span::styled(" ↺", Style::new().fg(AMBER_400)));
    }
    if row.collapsed {
        let hidden = graph.neighbours(&row.key, dir).len();
        spans.push(Span::styled(
            format!("  ▸ +{hidden}"),
            Style::new().fg(GREEN_400),
        ));
    }
    Line::from(spans)
}

// ── Small helpers ─────────────────────────────────────────────────────────

fn section_header(label: &str, sub: &str, direct: usize, total: usize) -> Paragraph<'static> {
    Paragraph::new(Line::from(vec![
        Span::styled(format!("{label} "), Style::new().fg(GREEN_400).bold()),
        Span::styled(format!("· {sub} "), Style::new().fg(MUTED)),
        Span::styled(
            format!("({direct} direct · {total} total)"),
            Style::new().fg(BORDER),
        ),
    ]))
}

fn status_for(graph: &Graph, key: &str) -> (&'static str, ratatui::style::Color) {
    match graph.get(key) {
        Some(i) => theme::status_glyph(i.status),
        None => ("·", MUTED),
    }
}

fn status_label(status: Status) -> &'static str {
    match status {
        Status::Triage => "Triage",
        Status::Backlog => "Backlog",
        Status::Unstarted => "Todo",
        Status::Started => "In Progress",
        Status::Completed => "Done",
        Status::Canceled => "Canceled",
        Status::Duplicate => "Duplicate",
        Status::Unknown => "—",
    }
}

/// Truncate to a display-*width* budget (cells), not a char count, so wide
/// (CJK / emoji) characters don't overflow the column. Reserves one cell for the
/// ellipsis.
fn truncate(s: &str, max: usize) -> String {
    if UnicodeWidthStr::width(s) <= max {
        return s.to_string();
    }
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > budget {
            break;
        }
        w += cw;
        out.push(c);
    }
    out.push('…');
    out
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

#[cfg(test)]
mod tests {
    // Rendering is exercised end-to-end by the snapshot tests in `main.rs`
    // (`render_snapshot`), which drive `draw` against a `TestBackend` at many
    // sizes. The pure geometry helpers live in `crate::layout` with their own
    // unit tests.
    use super::*;

    #[test]
    fn truncate_respects_a_width_budget() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hell…");
    }
}
