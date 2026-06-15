//! All terminal rendering for the app. Reads [`App`] state and paints either
//! the focus lens or the layered overview, plus the header, detail bar and help
//! overlay. No state mutation beyond the `ListState` scroll offsets ratatui
//! needs.

use std::sync::Arc;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, HighlightSpacing, List, ListItem, Paragraph};
use tui_term::widget::PseudoTerminal;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Flash, Mode, NodeKind, Pane, RightView, TreeRow};
use crate::backend::{AgentBackend, Lifecycle};
use crate::model::{Direction, Graph, Status};
use crate::session::AgentStatus;
use crate::theme::{self, *};

const LIST_WIDTH: u16 = 44;
const MAX_TITLE: usize = 64;

pub fn draw(app: &mut App, frame: &mut Frame) {
    // Attached to an agent: its PTY takes over the whole screen.
    if app.attached.is_some() {
        render_attached(app, frame);
        return;
    }

    let [header, body, detail, hints] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    render_header(app, frame, header);
    match app.mode {
        Mode::Lens => render_lens(app, frame, body),
        Mode::Overview => render_overview(app, frame, body),
    }
    render_detail(app, frame, detail);
    render_hints(app, frame, hints);

    if app.show_help {
        render_help(app, frame);
    }
}

// ── Attach pane (live agent PTY) ─────────────────────────────────────────────

/// Full-screen takeover rendering one agent's live terminal via tui-term, with
/// a racing-green frame and a detach hint so "attached" is unmistakable.
fn render_attached(app: &mut App, frame: &mut Frame) {
    let [header, body, hints] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let Some(issue) = app.attached.clone() else {
        return;
    };
    // Clone the Arc out so we can both read its parser and mutate `app` (the
    // resize bookkeeping) without overlapping borrows.
    let Some(backend) = app.backends.get(&issue).cloned() else {
        // The agent vanished from under us — drop back to the dashboard.
        app.attached = None;
        return;
    };

    // A dead agent is shown as a frozen, amber EXITED pane rather than a live
    // racing-green one, so "this agent is gone" is unmistakable.
    let key = app
        .graph
        .get(&issue)
        .map_or(issue.clone(), |i| i.key.clone());
    let detach = app.detach_key_label();
    let exited = matches!(backend.status(), Lifecycle::Exited(_));
    let (title, header_style, border) = if let Lifecycle::Exited(code) = backend.status() {
        let code = code.map_or_else(|| "signal".to_string(), |c| c.to_string());
        (
            format!(" ○ EXITED ({code})  {key}  · {detach} to leave "),
            Style::new().fg(INK).bg(AMBER_500).bold(),
            AMBER_400,
        )
    } else {
        let label = match app.graph.get(&issue) {
            Some(i) => format!(" ● ATTACHED  {}  {} ", i.key, truncate(&i.title, MAX_TITLE)),
            None => format!(" ● ATTACHED  {issue} "),
        };
        (
            label,
            Style::new().fg(GREEN_100).bg(GREEN_700).bold(),
            GREEN_500,
        )
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(title, header_style))),
        header,
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(border))
        .title(Line::from(Span::styled(
            " agent ",
            Style::new().fg(border).bold(),
        )));
    let inner = block.inner(body);
    frame.render_widget(block, body);

    // Keep a live agent's terminal sized to its pane (parser + PTY master both),
    // but only when it actually changed — this covers attach and live resize. A
    // dead agent keeps its final screen, so we don't resize it.
    let size = (inner.height, inner.width);
    if !exited && inner.area() > 0 && app.preview_size.get(&issue) != Some(&size) {
        let _ = backend.resize(inner.height, inner.width);
        app.preview_size.insert(issue.clone(), size);
    }

    if inner.area() > 0
        && let Ok(parser) = backend.parser().read()
    {
        frame.render_widget(PseudoTerminal::new(parser.screen()), inner);
    }

    let hint = if exited {
        format!(" agent has exited · {detach} to return to the dashboard")
    } else if app.detach_armed() {
        format!(" detach: finish the chord ({detach}) · repeat the leader to send it through")
    } else {
        format!(" keys go to the agent · {detach} to detach · resize reflows")
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, Style::new().fg(MUTED))))
            .style(Style::new().bg(WELL)),
        hints,
    );
}

// ── Header ──────────────────────────────────────────────────────────────────

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
            // Pulses in step with the per-node flags, so the header's call for
            // help breathes rather than sitting static.
            spans.push(Span::styled(
                format!(" · {needs_you} needs you ⚑"),
                theme::needs_you_style(app.frame),
            ));
        }
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

    // Reserve the right side's width so the two halves never overwrite each
    // other on a narrow terminal; the left content truncates cleanly instead.
    let right_w = u16::try_from(right.width()).unwrap_or(u16::MAX);
    let [left, right_area] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(right_w)]).areas(area);
    frame.render_widget(Paragraph::new(Line::from(spans)), left);
    frame.render_widget(
        Paragraph::new(right).alignment(Alignment::Right),
        right_area,
    );
}

// ── Lens (two-pane focus view) ───────────────────────────────────────────────

fn render_lens(app: &mut App, frame: &mut Frame, area: Rect) {
    let [left, right] =
        Layout::horizontal([Constraint::Length(LIST_WIDTH), Constraint::Min(0)]).areas(area);

    render_issue_list(app, frame, left);
    // The left list (the navigation spine) is constant; the right pane swaps
    // between the dependency trees and the live agent chats.
    match app.right_view {
        RightView::Deps => render_focus_panes(app, frame, right),
        RightView::Chat => render_chat_panes(app, frame, right),
    }
}

fn render_issue_list(app: &mut App, frame: &mut Frame, area: Rect) {
    let active = app.focus == Pane::List;
    let title = Line::from(vec![
        Span::styled(" ISSUES ", Style::new().fg(GREEN_100).bold()),
        Span::styled(format!("{} ", app.order.len()), Style::new().fg(MUTED)),
    ]);
    let block = pane_block(title, active);

    let items: Vec<ListItem> = app
        .order
        .iter()
        .map(|k| {
            let flash = app
                .flash
                .get(k)
                .and_then(|&(kind, until)| (app.frame < until).then_some(kind));
            ListItem::new(issue_line(
                &app.graph,
                k,
                app.fleet.get(k).copied(),
                app.frame,
                flash,
            ))
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_symbol(if active { "▸ " } else { "  " })
        .highlight_spacing(HighlightSpacing::Always)
        .highlight_style(if active {
            theme::cursor_active()
        } else {
            theme::cursor_idle()
        });
    frame.render_stateful_widget(list, area, &mut app.list_state);
}

fn render_focus_panes(app: &mut App, frame: &mut Frame, area: Rect) {
    // Title of the right pane = the focused issue itself, with its agent marker
    // when one's running so the deps view still shows agent state at a glance.
    let title = match app.focused_issue() {
        Some(i) => {
            let mut spans = vec![
                Span::styled(" ◆ ", Style::new().fg(GREEN_500)),
                Span::styled(format!("{}  ", i.key), Style::new().fg(INK).bold()),
                Span::styled(truncate(&i.title, MAX_TITLE), Style::new().fg(MUTED)),
                Span::raw(" "),
            ];
            if let Some(status) = app.fleet.get(&i.key) {
                let (mark, mstyle) = theme::agent_marker(*status, app.frame);
                spans.push(Span::styled(mark, mstyle));
                spans.push(Span::raw(" "));
            }
            Line::from(spans)
        }
        None => Line::from(" no issue "),
    };
    let active = matches!(app.focus, Pane::Upstream | Pane::Downstream);
    let block = pane_block(title, active);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let up_n = app.graph.direct_count(&app.root, Direction::Upstream);
    let up_t = app.graph.transitive(&app.root, Direction::Upstream);
    let down_n = app.graph.direct_count(&app.root, Direction::Downstream);
    let down_t = app.graph.transitive(&app.root, Direction::Downstream);

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
    render_tree(app, frame, up_body, Direction::Upstream);

    frame.render_widget(
        section_header("▼ DOWNSTREAM", "this unblocks", down_n, down_t),
        down_head,
    );
    render_tree(app, frame, down_body, Direction::Downstream);
}

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

fn render_tree(app: &mut App, frame: &mut Frame, area: Rect, dir: Direction) {
    let (rows, active) = match dir {
        Direction::Upstream => (&app.up_rows, app.focus == Pane::Upstream),
        Direction::Downstream => (&app.down_rows, app.focus == Pane::Downstream),
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

    let state = match dir {
        Direction::Upstream => &mut app.up_state,
        Direction::Downstream => &mut app.down_state,
    };
    frame.render_stateful_widget(list, area, state);
}

// ── Chat wall (live agent screens, read-only) ────────────────────────────────

/// The right pane in chat mode: a vertical stack of read-only `claude` screens —
/// pinned chats first, then the current selection — each reflowed to its pane so
/// you can watch several agents at once without a full-screen attach.
fn render_chat_panes(app: &mut App, frame: &mut Frame, area: Rect) {
    let panes = app.chat_panes();

    let title = chat_wall_title(app);
    let block = pane_block(title, true);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.area() == 0 {
        return;
    }

    // Empty state: nothing live to show here. Teach the open/pin flow.
    if panes.is_empty() {
        let msg = match app.focused_issue() {
            Some(i) => format!(
                "  no agent on {} — a to open one · p to pin its chat",
                i.key
            ),
            None => "  no agents running · select an issue and press a to open one".to_string(),
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::raw(""),
                Line::from(Span::styled(msg, Style::new().fg(MUTED).italic())),
                Line::raw(""),
                Line::from(Span::styled(
                    "  v returns to the dependency view",
                    Style::new().fg(BORDER),
                )),
            ]),
            inner,
        );
        return;
    }

    // Clone the backend handles (and pinned-ness) out BEFORE the loop so no
    // borrow of `app.backends` overlaps the `&mut app.preview_size` writes each
    // pane makes. The Arc clones are cheap (a refcount bump).
    let agents: Vec<(String, Arc<dyn AgentBackend>, bool)> = panes
        .iter()
        .filter_map(|k| {
            app.backends
                .get(k)
                .map(|b| (k.clone(), Arc::clone(b), app.pinned.iter().any(|p| p == k)))
        })
        .collect();

    for ((issue, backend, pinned), rect) in agents.into_iter().zip(chat_layout(inner, panes.len()))
    {
        render_one_chat(app, frame, rect, &issue, backend.as_ref(), pinned);
    }
}

/// The chat wall's outer title: the focused issue + a CHAT badge.
fn chat_wall_title(app: &App) -> Line<'static> {
    match app.focused_issue() {
        Some(i) => Line::from(vec![
            Span::styled(" ◆ ", Style::new().fg(GREEN_500)),
            Span::styled(format!("{}  ", i.key), Style::new().fg(INK).bold()),
            Span::styled(
                truncate(&i.title, MAX_TITLE.saturating_sub(8)),
                Style::new().fg(MUTED),
            ),
            Span::styled(" CHAT ", Style::new().fg(ORANGE_400).bold()),
        ]),
        None => Line::from(Span::styled(" CHAT ", Style::new().fg(ORANGE_400).bold())),
    }
}

/// Render one agent's chat pane: a state-coloured frame, then its live (or
/// frozen, if exited) PTY screen. Resizes the agent to the pane only on a real
/// geometry change, and never resizes a dead one.
fn render_one_chat(
    app: &mut App,
    frame: &mut Frame,
    rect: Rect,
    issue: &str,
    backend: &dyn AgentBackend,
    pinned: bool,
) {
    if rect.area() == 0 {
        return;
    }
    let status = app.fleet.get(issue).copied();
    let exited = matches!(backend.status(), Lifecycle::Exited(_));
    let (border, label) = chat_pane_chrome(status, exited);
    // `issue` is itself the graph key (the graph never shrinks), so no lookup.
    let key = issue;

    // The marker follows the fleet status — so a finished pane reads ✓/✗, not a
    // stale spinner — falling back to a neutral dot only when status is unknown.
    let (mark, mstyle) = match status {
        Some(s) => theme::agent_marker(s, app.frame),
        None => ("○", Style::new().fg(border)),
    };
    let mut title = vec![
        Span::raw(" "),
        Span::styled(mark, mstyle),
        Span::styled(format!(" {label}  "), Style::new().fg(border).bold()),
        Span::styled(format!("{key} "), Style::new().fg(INK).bold()),
    ];
    if pinned {
        // A single-width pin marker — every glyph in this UI is one cell, so a
        // wide emoji (which terminals size inconsistently) would skew the title.
        title.push(Span::styled("⊙ pin ", Style::new().fg(ORANGE_400)));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(border))
        .title(Line::from(title));
    let pane = block.inner(rect);
    frame.render_widget(block, rect);

    if pane.area() == 0 {
        return;
    }
    // Too short for a real terminal preview — collapse to a one-line summary so
    // the pane never renders a 1-row garbled screen.
    if pane.height < 2 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {key} · {label}"),
                Style::new().fg(MUTED),
            ))),
            pane,
        );
        return;
    }

    // Reflow a live agent to fit this pane (only when its geometry actually
    // changed — so browsing doesn't churn SIGWINCHes). A dead agent keeps its
    // frozen final screen untouched.
    let size = (pane.height, pane.width);
    if !exited && app.preview_size.get(issue) != Some(&size) {
        let _ = backend.resize(pane.height, pane.width);
        app.preview_size.insert(issue.to_string(), size);
    }
    if let Ok(parser) = backend.parser().read() {
        frame.render_widget(PseudoTerminal::new(parser.screen()), pane);
    }
}

/// Border colour + short label for a chat pane. The fleet status (the
/// supervisor's verdict) drives it, so a finished agent reads DONE/FAILED/
/// STOPPED rather than a transient amber EXITED. The bare amber EXITED is only
/// the fallback for the sub-frame window where the PTY is gone but no terminal
/// status has landed yet.
fn chat_pane_chrome(status: Option<AgentStatus>, exited: bool) -> (Color, &'static str) {
    match status {
        Some(AgentStatus::Spawning) => (GREEN_400, "STARTING"),
        Some(AgentStatus::Running) => (ORANGE_400, "WORKING"),
        Some(AgentStatus::NeedsYou) => (AMBER_400, "NEEDS YOU"),
        Some(AgentStatus::Idle) => (STATUS_400, "IDLE"),
        Some(AgentStatus::Stopped) => (MUTED, "STOPPED"),
        Some(AgentStatus::Done) => (STATUS_400, "DONE"),
        Some(AgentStatus::Failed) => (RED_400, "FAILED"),
        None if exited => (AMBER_400, "EXITED"),
        None => (BORDER, "AGENT"),
    }
}

/// Split `area` into `k` stacked panes, giving the remainder rows to the top
/// panes (so heights differ by at most one). Empty for `k == 0` or a zero-height
/// area; a pane can come back zero-height when `k` exceeds the rows available —
/// callers guard on `rect.area()` before drawing.
fn chat_layout(area: Rect, k: usize) -> Vec<Rect> {
    if k == 0 || area.height == 0 {
        return Vec::new();
    }
    let k = k as u16;
    let base = area.height / k;
    let extra = area.height % k;
    let mut rects = Vec::with_capacity(k as usize);
    let mut y = area.y;
    for i in 0..k {
        let h = base + u16::from(i < extra);
        rects.push(Rect::new(area.x, y, area.width, h));
        y = y.saturating_add(h);
    }
    rects
}

// ── Overview (layered, edge-free) ────────────────────────────────────────────

fn render_overview(app: &App, frame: &mut Frame, area: Rect) {
    let g = &app.graph;
    let block = pane_block(
        Line::from(Span::styled(
            " GRAPH OVERVIEW ",
            Style::new().fg(GREEN_100).bold(),
        )),
        false,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        " flow: roots (no blockers) ───▶ leaves (block nothing)",
        Style::new().fg(MUTED),
    )));
    lines.push(Line::raw(""));

    let bands = g.levels();
    // Lay every issue out, wrapping each level across as many rows as it needs
    // (no "+N more" truncation) so the overview is a genuine full top-down map.
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

    // Scroll to keep the highlighted root in view as you arrow through issues.
    // Lines are pre-wrapped (one visual row each), so a plain line offset is exact.
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
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {msg}"),
                Style::new().fg(AMBER_400),
            ))),
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

fn render_hints(app: &App, frame: &mut Frame, area: Rect) {
    let text = if app.search_active {
        " type to filter · ⏎ accept · esc clear"
    } else if app.mode == Mode::Overview {
        " ↑↓ move · a open · t attach · x stop · n needs-you · g lens · c cycles · f filter · s sort · ? help · q quit"
    } else if app.right_view == RightView::Chat {
        " ↑↓ pick · ]/[ switch agent · a open/resume · t attach · x stop · p pin · v deps · n needs-you · ? help · q quit"
    } else {
        " ↑↓ move · ←→/tab pane · ⏎ focus · a open · t attach · x stop · v chat · p pin · ]/[ agents · n needs-you · b back · / find · g graph · ? help · q quit"
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(text, Style::new().fg(MUTED))))
            .style(Style::new().bg(WELL)),
        area,
    );
}

fn render_help(app: &App, frame: &mut Frame) {
    use crate::keymap::Action::*;

    let k = |action| app.keymap.label_for(action);
    // Key column built live from the active keymap, so a rebind shows correctly.
    let rows: Vec<(String, &str)> = vec![
        (
            format!("{} {}", k(MoveUp), k(MoveDown)),
            "move within the active pane",
        ),
        (
            format!("{} {}", k(FocusList), k(CyclePane)),
            "switch pane (list ↔ up ↔ down)",
        ),
        (k(CycleFocus), "cycle focus through the three panes"),
        (k(Enter), "focus list → trees; on a node, re-root the lens"),
        ("— agents —".to_string(), ""),
        (
            k(LaunchAgent),
            "open an agent on the issue (resumes if it ran before)",
        ),
        (
            format!("{} / {}", k(Attach), k(Detach)),
            "attach to its terminal / detach (while attached)",
        ),
        (k(CancelAgent), "stop the agent on the focused issue"),
        (k(ToggleChat), "right pane: dependencies ↔ live chats"),
        (k(TogglePin), "pin / unpin this issue's chat to the wall"),
        (
            format!("{} / {}", k(CycleChat), k(CycleChatBack)),
            "switch to the next / previous agent's chat",
        ),
        (k(JumpNeedsYou), "jump to the next agent that needs you"),
        ("— graph —".to_string(), ""),
        (k(Back), "back to the previously focused issue"),
        (k(ToggleCollapse), "collapse / expand the selected subtree"),
        (k(StartSearch), "fuzzy-find issues by id or title"),
        (k(CycleFilter), "cycle filter: all / blocked / has-deps"),
        (
            k(CycleSort),
            "cycle sort: ready / blocked / status / priority / id",
        ),
        (k(JumpCycle), "jump through issues that sit on a cycle"),
        (k(ToggleGraph), "toggle the layered graph overview"),
        (
            format!("{} / Esc", k(Quit)),
            "quit (esc first closes overlays)",
        ),
    ];

    // Size the overlay to its content (clamped to the screen by centered_rect).
    let area = centered_rect(74, rows.len() as u16 + 7, frame.area());
    frame.render_widget(Clear, area);

    let mut lines = vec![
        Line::from(Span::styled(" Keys", Style::new().fg(GREEN_100).bold())),
        Line::raw(""),
    ];
    for (key, desc) in &rows {
        if desc.is_empty() {
            // A section divider (empty description) — a quiet group label.
            lines.push(Line::from(Span::styled(
                format!("  {key}"),
                Style::new().fg(MUTED),
            )));
        } else {
            lines.push(Line::from(vec![
                Span::styled(format!("  {key:<16}"), Style::new().fg(GREEN_400).bold()),
                Span::styled(*desc, Style::new().fg(INK)),
            ]));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "  rebind any of these in ~/.config/lindep/config.toml  [keys]",
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

// ── Line builders ─────────────────────────────────────────────────────────

/// One issue row for the left list: a leading agent gutter bar · status ·
/// priority · KEY · title (· blocked · cycle · animated agent marker). The
/// gutter and marker make "this issue has an agent, in this state" scannable
/// from the left edge; `flash` briefly pops the KEY on a launch/finish.
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

    // Leading gutter: a state-coloured bar marks an issue that has an agent as a
    // scannable left column; a blank keeps the column aligned otherwise. It sits
    // outside the row's selection highlight, so a selected row still reads green.
    let gutter = match agent {
        Some(status) => Span::styled("▎", Style::new().fg(theme::agent_glyph(status).1)),
        None => Span::raw(" "),
    };

    // The KEY span pops for a few frames on a launch/finish flash.
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

fn pane_block(title: Line<'static>, active: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(if active { GREEN_500 } else { BORDER }))
        .title(title)
}

/// Truncate to a display-*width* budget (cells), not a char count, so wide
/// (CJK / emoji) characters don't overflow the column. Reserves one cell for
/// the ellipsis.
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
