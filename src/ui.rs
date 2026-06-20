//! All terminal rendering for the cockpit. Reads [`App`] state and paints the
//! window strip (the Spine, live Agent PTYs, and Deps trees) plus the header,
//! detail bar and help overlay. No state mutation beyond the `ListState` scroll
//! offsets and the per-window `preview_size` ratatui/PTY resize bookkeeping
//! needs — the documented render-mutation contract.

use std::sync::Arc;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph,
    Widget, Wrap,
};
use tui_term::widget::PseudoTerminal;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Filter, Flash, Readiness};
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

fn display_status(app: &App, issue: &str, status: Option<AgentStatus>) -> Option<AgentStatus> {
    status.map(|s| app.display_agent_status(issue, s))
}

pub fn draw(app: &mut App, frame: &mut Frame) {
    // Accessibility floor: below this the body + modals collapse to ~0 rows and render
    // an unusable sliver with no signpost. Bail with a "too small" line instead (like
    // htop / lazygit / k9s), gating on both axes.
    let full = frame.area();
    if full.height < 8 || full.width < MIN_PTY_W {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " terminal too small — resize to at least 24×8 ",
                Style::new().fg(MUTED),
            ))),
            full,
        );
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
    render_strip(app, frame, body);
    render_detail(app, frame, detail);
    render_hints(app, frame, hints);

    if app.show_help {
        render_help(app, frame);
    }
    if app.show_summary {
        render_summary(app, frame);
    }
    if app.show_ledger {
        render_ledger(app, frame);
    }
    // The project switcher floats above everything else while it's open.
    if app.project_switcher.is_some() {
        let area = frame.area();
        // Snapshot the cross-project needs-you set + per-project counts before the
        // mutable picker borrow.
        let needs_you = app.projects_needing_you();
        let counts = app.project_agent_counts();
        if let Some(picker) = app.project_switcher.as_mut() {
            crate::picker::render_overlay(picker, frame, area, &needs_you, &counts);
        }
    }
    // The up-front repo multi-select (ENG-536) floats above everything too.
    if let Some(select) = app.repo_select.as_mut() {
        let area = frame.area();
        crate::picker::render_repo_overlay(&mut select.picker, frame, area);
    }
    // The disk-reclaim prompt (ENG-540) likewise.
    if let Some(prompt) = app.reclaim.as_mut() {
        let area = frame.area();
        crate::picker::render_reclaim_overlay(prompt, frame, area);
    }
    // The global all-agents screen (ENG-406) — a third top-level surface.
    if app.global_view.is_some() {
        // Snapshot project names before the mutable view borrow.
        let names: std::collections::HashMap<String, String> = app
            .project_list
            .iter()
            .map(|p| (p.id.clone(), p.name.clone()))
            .collect();
        let area = frame.area();
        let active = app.active_project.clone();
        if let Some(view) = app.global_view.as_mut() {
            render_global_overlay(view, &names, &active, frame, area);
        }
    }
}

/// Render the global all-agents screen (ENG-406): a centered modal listing every
/// agent across the workspace as `<glyph> project · ISSUE`, reusing the same status
/// glyph the spine gutter shows (steady — the modal doesn't drive the animation tick).
fn render_global_overlay(
    view: &mut crate::app::GlobalView,
    names: &std::collections::HashMap<String, String>,
    active: &str,
    frame: &mut Frame,
    full: Rect,
) {
    let width = (u32::from(full.width) * 7 / 10)
        .clamp(u32::from(50.min(full.width)), u32::from(full.width)) as u16;
    let height = (u32::from(full.height) * 7 / 10)
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
            " ALL AGENTS ",
            Style::new().fg(GREEN_100).bold(),
        )));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [body, hint] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);

    let items: Vec<ListItem> = view
        .rows
        .iter()
        .map(|(pid, issue, status)| {
            let (glyph, gstyle) = theme::agent_marker(*status, 0);
            let name = names.get(pid).cloned().unwrap_or_else(|| pid.clone());
            // Mark rows in the project you're already inside: Enter there is a cheap
            // re-root, while a row elsewhere is a full (costly) project switch — so the
            // cost is predictable before you commit.
            let here = pid == active;
            let mut spans = vec![
                Span::styled(format!("{glyph} "), gstyle),
                Span::styled(
                    name,
                    if here {
                        Style::new().fg(GREEN_100).bold()
                    } else {
                        Style::new().fg(MUTED)
                    },
                ),
                Span::styled(format!(" · {issue}"), Style::new().fg(INK)),
            ];
            if here {
                spans.push(Span::styled("  · here", Style::new().fg(GREEN_400)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let list = List::new(items)
        .highlight_symbol("▸ ")
        .highlight_spacing(HighlightSpacing::Always)
        .highlight_style(theme::cursor_active());
    frame.render_stateful_widget(list, body, &mut view.state);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " ↑↓ move · ⏎ go · esc back",
            Style::new().fg(MUTED),
        ))),
        hint,
    );
}

// ── Header ────────────────────────────────────────────────────────────────

fn render_header(app: &App, frame: &mut Frame, area: Rect) {
    let g = &app.graph;
    // Order by *salience*, not data category (M12): the left half is one unwrapped
    // Line that hard-truncates at the right edge, so whatever is pushed last clips
    // first. The actionable alert cluster (needs-you / elsewhere / resuming / agents-
    // off) therefore comes right after the project name, ahead of the rarely-acted-on
    // issue/edge/cycle metadata — so a narrow pane drops counts, never the alerts.
    let dot = || Span::styled(" · ", Style::new().fg(BORDER));
    let mut spans = vec![
        Span::styled("  lindep ", Style::new().fg(GREEN_500).bold()),
        Span::styled("· ", Style::new().fg(BORDER)),
        Span::styled(g.project.clone(), Style::new().fg(GREEN_100).bold()),
    ];
    // Workspace roll-up (ENG-406): needs-you across EVERY project. The "elsewhere"
    // badge breaks out the backgrounded subset; `agents` (the live count) is metadata.
    let (agents, needs_you) = app.workspace_summary();
    // Disjoint split so the two badges never read as additive (elsewhere ⊆ total): the
    // "elsewhere" count is in projects you've switched away from, so subtract it to get
    // the local, actionable-now count. A backgrounded prompt is never invisible
    // (Ctrl-a s / the global screen / cross-project `n` reach it; the switcher flags it).
    let elsewhere = app.elsewhere_needs_you();
    let here = needs_you.saturating_sub(elsewhere);
    if here > 0 {
        spans.push(dot());
        spans.push(Span::styled(
            format!("{here} needs you ⚑"),
            theme::needs_you_style(app.frame),
        ));
    }
    if elsewhere > 0 {
        spans.push(dot());
        spans.push(Span::styled(
            // Surface the cross-project teleport so a backgrounded prompt is reachable
            // by being invited to use it, not just visible (CF-14).
            format!(
                "⚑{elsewhere} elsewhere · {} jumps",
                app.keymap.verb_label(Action::JumpNeedsYou)
            ),
            theme::needs_you_style(app.frame),
        ));
    }
    // Auto-resume spinner: while docked agents are still coming back, the header
    // breathes a "resuming N…" so the cockpit reads as busy, not stalled.
    if app.resuming_count() > 0 {
        spans.push(dot());
        spans.push(Span::styled(
            format!(
                "{} resuming {}…",
                theme::agent_spinner(app.frame),
                app.resuming_count()
            ),
            Style::new().fg(ORANGE_400).bold(),
        ));
    }
    // Degraded indicator (M13): on a non-demo run with no workspace, agents never
    // armed. A standing chip says so (the reason lives in the launch refusal / footer),
    // instead of the reason flashing once and being wiped by the next key. Rendered
    // *dim* — it's a persistent standing condition, not a fresh alert, so it must not
    // compete with the pulsing needs-you cluster for the eye.
    if app.workspace.is_none() && !app.demo {
        spans.push(dot());
        spans.push(Span::styled("⚠ agents off", Style::new().fg(AMBER_400)));
    } else if app.demo {
        // A demo is an intentional read-only viewer, not a degraded run — give it its
        // own standing chip so the read-only state is legible after the first keystroke
        // wipes the boot banner (its symmetric counterpart to "agents off").
        spans.push(dot());
        spans.push(Span::styled("◌ read-only demo", Style::new().fg(MUTED)));
    }
    // A re-config (Ctrl-a o) wrote registry.toml but the live session still runs the
    // old binding — a standing dim chip keeps that divergence visible until restart,
    // since the "saved … restart to apply" footer is wiped by the next keystroke.
    if app.config_restart_pending {
        spans.push(dot());
        spans.push(Span::styled(
            "⟳ restart to apply config",
            Style::new().fg(AMBER_400),
        ));
    }
    // A discard that kept the worktree (rejected push or cleanup failure) left local
    // state on disk — a standing chip so the work isn't silently stranded.
    if !app.kept_worktrees.is_empty() {
        spans.push(dot());
        let kept = app.kept_worktrees.len();
        spans.push(Span::styled(
            format!("⚠ {kept} kept worktree{}", if kept == 1 { "" } else { "s" }),
            Style::new().fg(AMBER_400),
        ));
    }
    // A rejected auto-push left commits stranded on the local clone (never reaching
    // the true remote) — a standing, higher-salience chip (cross-project, so it counts
    // strands in backgrounded projects too) so the failure isn't lost when the next
    // footer wipes the transient line (the data-integrity contract).
    let unpushed = app.unpushed_count();
    if unpushed > 0 {
        spans.push(dot());
        spans.push(Span::styled(
            format!("⇡ {unpushed} unpushed"),
            Style::new().fg(RED_400),
        ));
    }
    // A lightweight "you're winning" tally of clean finishes this session (CF-14).
    if app.shipped_today > 0 {
        spans.push(dot());
        spans.push(Span::styled(
            format!("✓ {} shipped", app.shipped_today),
            Style::new().fg(GREEN_400),
        ));
    }
    // ── Lower-salience graph + agent metadata. ──
    spans.push(dot());
    spans.push(Span::styled(
        // Exclude materialized external blockers (they show in trees, never as Spine
        // rows), so this matches the Spine's own "ISSUES N" count instead of drifting —
        // and use the allocation-free count, off the per-frame render hot path.
        format!("{} issues", g.len().saturating_sub(g.external_count())),
        Style::new().fg(MUTED),
    ));
    spans.push(dot());
    spans.push(Span::styled(
        format!("{} edges", g.edge_count()),
        Style::new().fg(MUTED),
    ));
    if g.cycle_count() > 0 {
        spans.push(dot());
        spans.push(Span::styled(
            format!("{} cycles ↺", g.cycle_count()),
            Style::new().fg(AMBER_400),
        ));
    }
    if agents > 0 {
        spans.push(dot());
        spans.push(Span::styled(
            format!("{agents} agent{}", if agents == 1 { "" } else { "s" }),
            Style::new().fg(GREEN_400).bold(),
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
        Line::from(vec![Span::styled(
            format!("filter:{}  ", app.filter.label()),
            Style::new().fg(MUTED),
        )])
    };

    let min_left = 24u16.min(area.width);
    let right_w = u16::try_from(right.width())
        .unwrap_or(u16::MAX)
        .min(area.width.saturating_sub(min_left));
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
            let (full, cards, overflow) = layout::rail(area, n, focus, active, preview);
            for p in full {
                render_window_at(app, frame, p.rect, p.index);
            }
            for p in cards {
                render_card(app, frame, p.rect, p.index);
            }
            if let Some(overflow) = overflow {
                render_rail_overflow(frame, overflow.rect, overflow.hidden);
            }
        }
    }
}

fn render_rail_overflow(frame: &mut Frame, rect: Rect, hidden: usize) {
    if rect.area() == 0 {
        return;
    }
    // The overflow slot is always exactly one row tall (the rail splits into
    // `visible + 1` equal slots, each 1 row), so a bordered block would spend that
    // single row on its top border and zero out the inner area — suppressing the
    // count entirely (NEW-03). Render the count directly as a borderless muted line
    // so `+N` actually shows; the `⋯` carries the "more" cue the border used to.
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" ⋯ +{hidden} more"),
            Style::new().fg(MUTED).bold(),
        ))),
        rect,
    );
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
            let status = display_status(app, issue, app.fleet.get(issue).copied());
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

/// The Spine's title — the issue list, lit with its count. (v1.7 folded the
/// agents roster into the readiness bands, so there's no longer an AGENTS tab.)
fn spine_title(app: &App) -> Line<'static> {
    Line::from(Span::styled(
        format!(" ISSUES {} ", app.order.len()),
        Style::new().fg(GREEN_100).bg(GREEN_700).bold(),
    ))
}

fn render_spine(app: &mut App, frame: &mut Frame, area: Rect, focused: bool) {
    let block = window_block(spine_title(app), focused, BORDER, false, app.frame);

    // The readiness schedule is the single Spine view: bands + a ready rail.
    // Re-band first if an agent event left the order stale (the schedule orders
    // by fleet state, which events mutate without re-sorting) so the dividers stay
    // contiguous and navigation matches the rows on screen.
    if !app.order_is_banded() {
        app.rebuild_order();
    }
    render_banded_spine(app, frame, area, focused, block);
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

/// The readiness-banded Issues spine (ENG-558): the existing list, the same
/// single `order` Vec, with thin section dividers spliced between readiness
/// bands. The dividers are non-selectable rows, so navigation still indexes
/// `order` (issue keys only); we render through `app.banded_list_state`, whose
/// selected index is offset by the dividers above the selection and whose scroll
/// offset persists across frames. No new view, no new color hue: the only
/// net-new visuals are these dividers and the `▸` ready rail in `issue_line`.
fn render_banded_spine(app: &mut App, frame: &mut Frame, area: Rect, focused: bool, block: Block) {
    // Paint the border/title ourselves, then split the inner area into a fixed
    // sticky band-header row + the scrolling list below it — so the header for the
    // band the cursor sits in is always on screen, even when its divider has
    // scrolled off the top (A1).
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if app.order.is_empty() {
        // An empty list reads as a crash / lost project without a word — say why, and
        // how to undo it (a filter/search hid everything vs a genuinely empty project).
        let msg = if app.filter != Filter::All || !app.search_query.is_empty() {
            "  no matches — clear the filter / search to list"
        } else {
            "  no issues in this project"
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                msg,
                Style::new().fg(MUTED).add_modifier(Modifier::ITALIC),
            ))),
            inner,
        );
        return;
    }
    // Divider content width = inner − the always-on 2-col highlight gutter
    // ("▸ " / "  "). A short rule just truncates; it never wraps.
    let rule_width = inner.width.saturating_sub(2);
    // Dispatch is only real with a live workspace — gate the READY lane hint + rail
    // so the read-only demo / a degraded run never advertise a launch they refuse (H6).
    let dispatch = app.workspace.is_some();
    let mut items: Vec<ListItem> = Vec::with_capacity(app.order.len() + 6);
    // Band per *rendered* item (dividers included), so the sticky header can read the
    // band of the topmost visible row. A divider is exactly the first row of a band:
    // `i == 0 || bands[i] != bands[i-1]` — so no separate `is_divider` Vec is needed.
    let mut bands: Vec<Readiness> = Vec::with_capacity(app.order.len() + 6);
    let mut selected = None;
    let mut prev_band: Option<Readiness> = None;
    for key in &app.order {
        let band = app.readiness(key);
        if prev_band != Some(band) {
            items.push(band_divider(band, rule_width, dispatch));
            bands.push(band);
            prev_band = Some(band);
        }
        let selected_here = focused && *key == app.root;
        if *key == app.root {
            selected = Some(items.len());
        }
        items.push(issue_item(
            app,
            key,
            band == Readiness::Ready,
            selected_here,
        ));
        bands.push(band);
    }
    let [sticky_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    // Choose the scroll offset deterministically so (a) the selected row stays visible
    // and (b) the topmost visible row is never a band divider — its label is shown by
    // the sticky header above, so letting the list paint it too would double it. ratatui
    // leaves our offset untouched while the selection is within the window, so even a
    // downward scroll (PageDown/MoveBottom) can't re-land the top on a divider post-render
    // and re-introduce the double-paint (A1).
    let divider_at = |i: usize| i == 0 || bands[i] != bands[i - 1];
    let list_h = (list_area.height as usize).max(1);
    let mut off = app.banded_list_state.offset().min(items.len() - 1);
    if let Some(sel) = selected {
        if sel < off {
            off = sel;
        } else if sel >= off + list_h {
            off = sel + 1 - list_h;
        }
    }
    // Never leave a divider at the very top. The selected row is never a divider, so it
    // stays visible one row below the skip; with no selection we just preserve the
    // persisted offset minus the leading divider.
    if divider_at(off) && off + 1 < items.len() && selected.is_none_or(|sel| sel > off) {
        off += 1;
    }
    *app.banded_list_state.offset_mut() = off;
    // The list draws without its own block (we already painted it above).
    let list = list_widget(items, Block::default(), focused);
    app.banded_list_state.select(selected);
    frame.render_stateful_widget(list, list_area, &mut app.banded_list_state);
    // Sticky header: the band of the (non-divider) topmost row the list settled on.
    let mut sticky = band_divider_line(bands[off.min(bands.len() - 1)], rule_width, dispatch);
    // Match the list's always-on 2-col highlight gutter so the sticky header lines
    // up exactly with the in-list dividers below it.
    sticky.spans.insert(0, Span::raw("  "));
    frame.render_widget(Paragraph::new(sticky), sticky_area);
}

/// The (glyph, label, accent) for a readiness band header. Factored out so the
/// renderer and the disjointness tests share one source of truth.
///
/// One distinct glyph per band, each disjoint from the row marker it sits above so
/// a header never stutters with its own rows (the v1.7 IDLE `◦` and WORKING `◉`
/// regressions — see `band_headers_never_stutter_with_their_member_rows`):
///   • `⚐` (outline flag) ≠ the row's filled, breathing `⚑`,
///   • `◎` (bullseye)     ≠ the repo-select checkbox `◉`,
///   • `◯` (large ring)   ≠ the idle marker `◦`.
/// `▸` (READY) and `✓` (DONE) intentionally echo their own agent markers — like the
/// rail, the lane rows *should* match — so they stay distinct by COLOUR (a muted header
/// vs the live marker hue), which `band_headers_never_stutter…` verifies; `⊘` (BLOCKED)
/// is off every marker / priority / checkbox set.
fn band_header(band: Readiness) -> (&'static str, &'static str, Color) {
    match band {
        Readiness::NeedsYou => ("⚐", "NEEDS YOU", RED_400),
        Readiness::Working => ("◎", "WORKING", ORANGE_400),
        Readiness::Idle => ("◯", "IDLE", STATUS_400),
        Readiness::Ready => ("▸", "READY", INK),
        Readiness::Blocked => ("⊘", "BLOCKED", AMBER_400),
        Readiness::Done => ("✓", "DONE", MUTED),
    }
}

/// A thin section header between readiness bands. Reuses each band's *existing*
/// accent (no new hue): the glyph + label carry the colour, a dim rule fills the
/// row. The READY header gets a right-aligned `dispatch` hint; DONE dims.
fn band_divider<'a>(band: Readiness, rule_width: u16, dispatch: bool) -> ListItem<'a> {
    ListItem::new(band_divider_line(band, rule_width, dispatch))
}

/// The band-header content as a bare `Line` — shared by the in-list dividers and
/// the sticky header pinned to the top of the spine viewport (A1), so the two
/// always read identically. `dispatch` gates the READY lane's launch affordance.
fn band_divider_line<'a>(band: Readiness, rule_width: u16, dispatch: bool) -> Line<'a> {
    let (glyph, label, accent) = band_header(band);
    let head = format!("{glyph} {label} ");
    // The READY divider carries the dispatch affordance only when agents can
    // actually launch (a live workspace) — never in the read-only demo or a degraded
    // run, where Enter can dispatch nothing (H6).
    let hint = if dispatch && matches!(band, Readiness::Ready) {
        "  dispatch "
    } else {
        ""
    };
    let rule_len =
        (rule_width as usize).saturating_sub(head.chars().count() + hint.chars().count());
    let dim = if matches!(band, Readiness::Done) {
        Modifier::DIM
    } else {
        Modifier::empty()
    };
    let mut spans = vec![
        Span::styled(head, Style::new().fg(accent).bold().add_modifier(dim)),
        Span::styled(
            "─".repeat(rule_len),
            Style::new().fg(BORDER).add_modifier(dim),
        ),
    ];
    if !hint.is_empty() {
        spans.push(Span::styled(hint, Style::new().fg(MUTED)));
    }
    Line::from(spans)
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
    let status = display_status(app, issue, app.fleet.get(issue).copied());
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
    } else {
        // The unpinned preview is transient — it follows the Spine selection and
        // vanishes when you move; mark it so its impermanence is visible (B0c).
        title.push(Span::styled("~ preview ", Style::new().fg(MUTED)));
    }
    if app.windows.zoomed {
        // A zoomed coin hides the Spine entirely, so without a marker it reads as the
        // app having broken; surface the latched state (and its escape key in the hint).
        title.push(Span::styled("⤢ zoom ", Style::new().fg(VIOLET_200)));
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

    // Too small for a real preview — collapse to a one-line summary. When the tile is
    // FOCUSED, say so in amber: keystrokes are landing in this invisible pane, so tell
    // the user how to enlarge it instead of letting them type blind into claude (D-MED).
    if pane.height < 2 || pane.width < MIN_PTY_W {
        let (text, style) = if focused {
            (
                format!(
                    " {} zoom · {key} too small",
                    app.keymap.verb_label(Action::ZoomToggle)
                ),
                Style::new().fg(AMBER_400),
            )
        } else {
            (format!(" {key} · {label}"), Style::new().fg(MUTED))
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(text, style))).wrap(Wrap { trim: true }),
            pane,
        );
        return;
    }

    // Reflow a live agent to its window only on a real geometry change (so
    // browsing/scrolling never churns SIGWINCHes). A dead agent keeps its frozen
    // final screen. Keyed by WindowId so zoom's two geometries don't collide.
    let size = (pane.height, pane.width);
    if !exited && app.preview_size.get(&id) != Some(&size) {
        // Only record the size once the resize actually took. Recording it on a
        // failed resize would make the `!= size` guard skip every retry, leaving our
        // render half at the new geometry while claude's PTY stays at the old one —
        // the exact divergence backend::resize warns about. A failure here leaves
        // preview_size stale so the next frame retries at the correct geometry.
        if backend.resize(pane.height, pane.width).is_ok() {
            app.preview_size.insert(id, size);
        }
    }
    if let Ok(parser) = backend.parser().read() {
        let screen = parser.screen();
        let (srows, _scols) = screen.size();
        if srows > pane.height {
            // The vt100 grid is taller than the pane — a transient after a layout
            // change or a resize that returned `Err` (so `preview_size` stayed stale
            // and the grid is mid-reflow) for a LIVE agent. tui-term paints top-left
            // with no offset, so a naive paint would clip the BOTTOM rows — exactly
            // where `claude` draws its input box. Bottom-align instead: render the full
            // grid to a scratch buffer and blit only its bottom `pane.height` rows, so
            // the input row (always last) always survives (A3). For a live agent this
            // self-corrects next frame once the resize takes; for an *exited* agent the
            // resize above is skipped (`!exited`), so a dead grid that froze taller than
            // its current pane keeps this (bounded) blit each frame until the card closes.
            // The scratch is sized to `pane.width`, not the grid's `_scols`: a stale/dead
            // grid of a different width crops or blanks its right edge here exactly as the
            // top-left `else` paint would — deliberately matching that horizontal behavior.
            let area = Rect::new(0, 0, pane.width, srows);
            let mut scratch = Buffer::empty(area);
            PseudoTerminal::new(screen).render(area, &mut scratch);
            let skip = srows - pane.height;
            let dst = frame.buffer_mut();
            for y in 0..pane.height {
                for x in 0..pane.width {
                    dst[(pane.x + x, pane.y + y)] = scratch[(x, y + skip)].clone();
                }
            }
        } else {
            frame.render_widget(PseudoTerminal::new(screen), pane);
        }
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
    let status = display_status(app, &root, app.fleet.get(&root).copied());
    let breathe = !focused && status == Some(AgentStatus::NeedsYou);
    // The coin's *identity* (fixed) can diverge from the displayed deps root after a
    // re-root: Tab flips to the identity's chat, not the rooted issue's. Surface that
    // before Tab so flipping the face isn't a silent bait-and-switch (H7).
    let identity = app.windows.windows[idx]
        .kind
        .coin()
        .map(|(issue, _)| issue.to_string());

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
    if let Some(identity) = &identity
        && *identity != root
    {
        title.push(Span::styled(
            format!("(chat → {identity}) "),
            Style::new().fg(AMBER_400),
        ));
    }
    if pinned {
        title.push(Span::styled("⊙ pin ", Style::new().fg(ORANGE_400)));
    } else {
        // The unpinned preview is transient — it follows the Spine selection and
        // vanishes when you move; mark it so its impermanence is visible (B0c).
        title.push(Span::styled("~ preview ", Style::new().fg(MUTED)));
    }
    if app.windows.zoomed {
        title.push(Span::styled("⤢ zoom ", Style::new().fg(VIOLET_200)));
    }
    let block = window_block(Line::from(title), focused, GREEN_500, breathe, app.frame);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if inner.area() == 0 {
        return;
    }
    if inner.height < 2 || inner.width < MIN_PTY_W {
        let (text, style) = if focused {
            (
                format!(
                    " {} zoom · {root} deps too small",
                    app.keymap.verb_label(Action::ZoomToggle)
                ),
                Style::new().fg(AMBER_400),
            )
        } else {
            (format!(" {root} · deps"), Style::new().fg(MUTED))
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(text, style))).wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }
    if crate::worktree::is_synthetic_ask_id(&root) {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " ad-hoc agent · no dependency graph",
                Style::new().fg(MUTED).italic(),
            )))
            .wrap(Wrap { trim: true }),
            inner,
        );
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
    // Record one tree's visible height so PageUp/PageDown page by THIS pane's screenful
    // (a tiled deps coin), not the whole terminal — only for the focused coin, the one
    // the keys drive (M3). The two bodies are equal-split, so either height serves.
    if focused {
        app.deps_view_h = up_body.height;
    }

    frame.render_widget(
        section_header("▲ BLOCKED BY", "must finish first", up_n, up_t),
        up_head,
    );
    render_tree(app, frame, up_body, idx, Direction::Upstream, focused);
    frame.render_widget(
        section_header("▼ BLOCKS", "this unblocks", down_n, down_t),
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
    if app.windows.zoomed {
        title.push(Span::styled("⤢ zoom ", Style::new().fg(VIOLET_200)));
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
    let max_key_w = bands
        .iter()
        .flat_map(|band| band.iter())
        .map(|key| UnicodeWidthStr::width(key.as_str()))
        .max()
        .unwrap_or(7);
    let per_chip = (max_key_w + 6).max(10);
    let chips_per_row = (usize::from(inner.width.saturating_sub(6)) / per_chip).max(1);
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
                // Keep the workflow-state glyph shape, but tint it by readiness
                // (ENG-560) so the map reads "what can I dispatch," consistent
                // with the spine bands — a pure recolour, no new hue.
                let (glyph, _) = status_for(g, key);
                let color = readiness_tint(app.readiness(key));
                let key_style = if *key == app.root {
                    root_line = Some(lines.len());
                    Style::new().fg(GREEN_100).bg(GREEN_700).bold()
                } else {
                    Style::new().fg(INK)
                };
                spans.push(Span::styled(format!("{glyph} "), Style::new().fg(color)));
                spans.push(Span::styled(key.to_string(), key_style));
                if let Some(status) = display_status(app, key, app.fleet.get(key).copied()) {
                    let (mark, mstyle) = theme::agent_marker(status, app.frame);
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
        let style =
            if app.kill_confirm.is_some() || app.discard_confirm.is_some() || app.quit_confirm {
                Style::new().fg(RED_400).bold()
            } else if app.repo_confirm.is_some() {
                Style::new().fg(AMBER_400).bold()
            } else {
                Style::new().fg(AMBER_400)
            };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(format!(" {msg}"), style))),
            area,
        );
        return;
    }
    // Describe the issue the pane shows — a re-rooted deps coin's cursor root included
    // — so the detail bar agrees with the `i` summary and the dispatch/kill verbs (H6).
    let Some(issue) = app.detail_key().and_then(|k| app.graph.get(k)) else {
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
        " type to filter · ⏎ accept · esc clear · ctrl-c close".to_string()
    } else if app.kill_confirm.is_some() {
        " y / ⏎ confirm kill · any other key cancels".to_string()
    } else if app.discard_confirm.is_some() {
        " y / ⏎ confirm discard · any other key cancels".to_string()
    } else if app.repo_confirm.is_some() {
        " y / ⏎ pull repo · any other key denies".to_string()
    } else if app.quit_confirm {
        " y / ⏎ confirm quit · esc cancels".to_string()
    } else if app.prefix_armed {
        format!(
            " {p} armed: ←→ focus · {} chat/deps · {} zoom · {} pin · {} close · {} kill · {} layout · {} quit · {p} again → agent · {} for all",
            vk(Action::ContextToggle),
            vk(Action::ZoomToggle),
            vk(Action::PinWindow),
            vk(Action::CloseWindow),
            vk(Action::KillWindow),
            vk(Action::LayoutToggle),
            vk(Action::Quit),
            dk(Action::ToggleHelp),
        )
    } else {
        match app.windows.focused_kind() {
            WindowKind::Coin {
                mode: CoinMode::Chat,
                ..
            } => format!(
                " keys → agent · {p} escape · {p} {} chat/deps · {p} {} nav · {p} {} close · {p} {} kill · {p} {} help",
                vk(Action::ContextToggle),
                vk(Action::FocusNav),
                vk(Action::CloseWindow),
                vk(Action::KillWindow),
                dk(Action::ToggleHelp),
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
                " ↑↓ move · ⇞⇟ page · ⏎ open agent · {} deps · {} map · {} needs you · {} find · {} summary · {} ledger · {p} {} quit · {} help",
                dk(Action::OpenDeps),
                dk(Action::OpenFleet),
                dk(Action::JumpNeedsYou),
                dk(Action::StartSearch),
                dk(Action::ToggleSummary),
                dk(Action::ToggleLedger),
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

fn render_help(app: &mut App, frame: &mut Frame) {
    let direct = |action| app.keymap.label_for(action);
    let verb = |action| app.keymap.verb_label(action);
    let prefix = app.prefix_label();

    let rows: Vec<(String, std::borrow::Cow<'static, str>)> = vec![
        ("— the spine —".to_string(), "".into()),
        (
            format!("{} {}", direct(Action::MoveUp), direct(Action::MoveDown)),
            "move the selection".into(),
        ),
        (
            format!("{} {}", direct(Action::PageUp), direct(Action::PageDown)),
            "page up / down (Home / End jump to top / bottom)".into(),
        ),
        (
            direct(Action::Enter),
            "open / focus an agent on the selection".into(),
        ),
        (
            direct(Action::ContextToggle),
            format!(
                "flip the current window's face: chat ↔ deps ({} {} in a chat)",
                prefix,
                app.keymap.verb_key_label(Action::ContextToggle)
            )
            .into(),
        ),
        (
            direct(Action::OpenDeps),
            "dive into the current window's deps".into(),
        ),
        (
            direct(Action::OpenFleet),
            "open the project overview (the Fleet)".into(),
        ),
        (
            direct(Action::StartSearch),
            "fuzzy-find issues by id or title".into(),
        ),
        (
            direct(Action::ToggleSummary),
            "summary overlay for the selected issue (↑↓ scroll · esc / i close)".into(),
        ),
        (
            direct(Action::ToggleLedger),
            "agent ledger: this issue's session history (↑↓ scroll · esc / t close)".into(),
        ),
        (direct(Action::CycleFilter), "cycle the issue filter".into()),
        (
            direct(Action::JumpNeedsYou),
            format!(
                "jump to the next agent that needs you ({} {} from a chat)",
                prefix,
                app.keymap.verb_key_label(Action::JumpNeedsYou)
            )
            .into(),
        ),
        (
            direct(Action::JumpCycle),
            "jump through issues on a cycle".into(),
        ),
        ("— deps —".to_string(), "".into()),
        (
            direct(Action::SwitchSide),
            "switch the active tree (blocked-by ↔ blocks)".into(),
        ),
        (
            direct(Action::Enter),
            "re-root the deps tree on the selected node".into(),
        ),
        (
            direct(Action::ToggleCollapse),
            "collapse / expand the subtree".into(),
        ),
        (direct(Action::Back), "back to the previous root".into()),
        (format!("— windows ({} prefix) —", prefix), "".into()),
        (
            format!("{} {}", verb(Action::FocusLeft), verb(Action::FocusRight)),
            "focus the window left / right".into(),
        ),
        (
            verb(Action::FocusNav),
            "jump focus home to the nav (spine)".into(),
        ),
        (
            verb(Action::AttachOrSpawn),
            "open / focus an agent (from any window)".into(),
        ),
        (
            verb(Action::ZoomToggle),
            "zoom the focused window (non-destructive)".into(),
        ),
        (
            verb(Action::PinWindow),
            "pin = keep this window permanently (it stops following the selection)"
                .into(),
        ),
        (
            verb(Action::CloseWindow),
            "close = remove a pinned window (its agent keeps running)".into(),
        ),
        (
            verb(Action::KillWindow),
            "kill the focused agent (confirmed)".into(),
        ),
        (
            verb(Action::RestartAgent),
            "restart the on-screen issue's agent (reclaim + relaunch)".into(),
        ),
        (
            verb(Action::NextAgent),
            "walk to the next live agent".into(),
        ),
        (
            verb(Action::DispatchReady),
            "launch every READY issue up to the capacity cap".into(),
        ),
        (
            verb(Action::ChooseRepos),
            "launch, choosing repos — opens the select even for one repo; a adds another".into(),
        ),
        (
            verb(Action::AskAgent),
            "start an ad-hoc agent with a throwaway worktree".into(),
        ),
        (
            verb(Action::LayoutToggle),
            "cycle layout: auto (by window count) → rail → mosaic".into(),
        ),
        (format!("— project & disk ({} prefix) —", prefix), "".into()),
        (
            verb(Action::SwitchProject),
            "switch project (set one up the first time you open it)".into(),
        ),
        (
            verb(Action::ConfigureProject),
            "(re)configure this project's repos / scratch — applies on restart".into(),
        ),
        (
            verb(Action::GlobalView),
            "global all-agents screen (every project)".into(),
        ),
        (
            verb(Action::OpenInEditor),
            "open the focused agent's workspace in your editor".into(),
        ),
        (
            verb(Action::DiscardWorkspace),
            "discard a finished issue's workspace (push + remove worktrees)".into(),
        ),
        (
            verb(Action::ReclaimMirrors),
            "reclaim disk: free unreferenced mirrors".into(),
        ),
        (verb(Action::Quit), "quit the cockpit".into()),
    ];

    let area = centered_rect(78, rows.len() as u16 + 10, frame.area());
    frame.render_widget(Clear, area);

    // A one-line orientation so the section headers below assemble into a model
    // instead of reading as four unrelated topics (the vocabulary findings).
    let mut lines = vec![
        Line::from(Span::styled(" Keys", Style::new().fg(GREEN_100).bold())),
        Line::raw(""),
        Line::from(Span::styled(
            "  tiled windows: Spine (window 0) · coins (one issue, chat/deps) · the Fleet.",
            Style::new().fg(MUTED),
        )),
        Line::from(Span::styled(
            format!(
                "  {prefix} = window/project verbs · pin a coin to keep it (else it previews)."
            ),
            Style::new().fg(MUTED),
        )),
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
                Span::styled(desc.as_ref(), Style::new().fg(INK)),
            ]));
        }
    }

    // The config footer is load-bearing (the rebind paths + the agents cap), so PIN it
    // to the bottom of the card — the bindings scroll above it and it is never clipped on
    // a short terminal, even as the verb list grows (H3).
    let footer_lines = vec![
        Line::from(Span::styled(
            "  rebind in ./.lindep/config.toml or ~/.config/lindep/config.toml — [keys] / [verbs]",
            Style::new().fg(MUTED),
        )),
        Line::from(Span::styled(
            "  cap live agents: [agents] max_concurrent = N in the same file",
            Style::new().fg(MUTED),
        )),
    ];

    let base = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(GREEN_600))
        .title(Line::from(Span::styled(
            " lindep ",
            Style::new().fg(GREEN_500).bold(),
        )));
    let inner = base.inner(area);
    let [body, footer] = Layout::vertical([Constraint::Min(0), Constraint::Length(2)]).areas(inner);

    // Scroll the bindings (not the pinned footer); clamp the offset to the real height
    // and write it back so an over-scroll leaves no dead presses (H3).
    let max_scroll = (lines.len() as u16).saturating_sub(body.height);
    app.help_scroll = app.help_scroll.min(max_scroll);
    let scroll = app.help_scroll;

    // Exit + scroll affordance on the bottom border; ▴/▾ appear only on overflow.
    let hint = match (scroll > 0, scroll < max_scroll) {
        (true, true) => " ▴▾ scroll · ? / esc close ",
        (false, true) => " ▾ more · ↓ scroll · ? / esc close ",
        (true, false) => " ▴ more · ↑ scroll · ? / esc close ",
        (false, false) => " ? / esc close ",
    };
    frame.render_widget(
        base.title_bottom(Line::from(Span::styled(hint, Style::new().fg(MUTED))).right_aligned()),
        area,
    );
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), body);
    frame.render_widget(Paragraph::new(footer_lines), footer);
}

/// The issue-summary overlay (`i`): a dismissable, at-a-glance card for the
/// selected (or focused) issue — its status/priority/assignee/team, blocked/cycle
/// flags, and its direct blockers + blocked work with their statuses. Pure read of
/// the local graph; no network. Any key closes it (see `App::on_key`).
fn render_summary(app: &mut App, frame: &mut Frame) {
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

    // A compact agent-history line: the durable ledger, at a glance, so the
    // summary card answers "has anyone run an agent on this, and how did it go?"
    // (the full timeline is the Ctrl-a t overlay).
    lines.push(ledger_summary_line(app, key));

    // Wrap a long title instead of truncating it, and size the card to the real wrapped
    // height so the title and a long dependency list always scroll fully into reach (A5).
    // ratatui's exact word-wrap counter (`Paragraph::line_count`) is feature-gated, so
    // sum each line's char-wrap rows (ceil(width / inner_w)) at the true inner width —
    // fixing both the old `title/74`-capped estimate (which dead-ended a long title) and
    // its one-row-per-line assumption. Char-wrap can undercount word-wrap by a row in
    // pathological cases, but only the harmless trailing blank, never a real row.
    let card_w = 78u16.min(frame.area().width);
    let inner_w = (card_w.saturating_sub(2)).max(1) as usize;
    let content = lines
        .iter()
        .map(|l| l.width().max(1).div_ceil(inner_w))
        .sum::<usize>() as u16;
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    let area = centered_rect(78, content + 2, frame.area());
    frame.render_widget(Clear, area);
    let inner_h = area.height.saturating_sub(2);
    let max_scroll = content.saturating_sub(inner_h);
    app.summary_scroll = app.summary_scroll.min(max_scroll);
    let scroll = app.summary_scroll;
    // Dismiss only on Esc / `i`, so the scroll hint replaces the old "any key" title.
    let hint = if max_scroll > 0 {
        " ↑↓ scroll · esc / i close "
    } else {
        " esc / i close "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(GREEN_600))
        .title(Line::from(Span::styled(
            " issue summary ",
            Style::new().fg(GREEN_500).bold(),
        )))
        .title_bottom(Line::from(Span::styled(hint, Style::new().fg(MUTED))).right_aligned());
    frame.render_widget(para.block(block).scroll((scroll, 0)), area);
}

/// The one-line agent-history summary for the `i` panel: run count, last outcome
/// and total prompts, or a "no agent yet" note.
fn ledger_summary_line<'a>(app: &App, issue: &str) -> Line<'a> {
    let episodes = app.ledger.episodes(&app.active_project, issue);
    let Some(last) = episodes.last() else {
        return Line::from(Span::styled(
            " ⌁ agent: never run",
            Style::new().fg(MUTED).italic(),
        ));
    };
    let now = crate::ledger::now_unix();
    let runs = episodes.len();
    let prompts: u32 = episodes.iter().map(|e| e.needs_you).sum();
    let when = crate::ledger::ago(now, last.started_at);
    let outcome = if last.is_open() {
        "running".to_string()
    } else {
        crate::ledger::outcome_label(last.outcome).to_string()
    };
    let mut text = format!(
        " ⌁ agent: {runs} run{} · last {outcome} {when}",
        if runs == 1 { "" } else { "s" }
    );
    if prompts > 0 {
        text.push_str(&format!(" · ⚑{prompts}"));
    }
    Line::from(Span::styled(text, Style::new().fg(STATUS_400)))
}

/// The agent session ledger overlay (`Ctrl-a t`): the durable, at-a-glance history
/// of every `claude` run on the selected issue — when each started, how long it
/// ran, how it ended, and how many times it needed you. Answers "what has run on
/// this issue?", which the live fleet view (current status only) cannot. Any key
/// closes it (see `App::on_key`).
fn render_ledger(app: &mut App, frame: &mut Frame) {
    let Some(key) = app.detail_key() else {
        return;
    };
    let key = key.to_string();
    let episodes = app.ledger.episodes(&app.active_project, &key);
    let now = crate::ledger::now_unix();

    let mut lines = vec![
        Line::from(vec![
            Span::styled(" ⌁ ", Style::new().fg(GREEN_400)),
            Span::styled(format!("{key} "), Style::new().fg(INK).bold()),
            Span::styled("agent session ledger", Style::new().fg(GREEN_400).bold()),
        ]),
        Line::raw(""),
    ];

    if episodes.is_empty() {
        lines.push(Line::from(Span::styled(
            "   no agent has run on this issue yet — ⏎ on the spine launches one",
            Style::new().fg(MUTED).italic(),
        )));
    } else {
        let prompts: u32 = episodes.iter().map(|e| e.needs_you).sum();
        lines.push(Line::from(Span::styled(
            format!(
                "   {} run{} · {prompts} prompt{} for you total",
                episodes.len(),
                if episodes.len() == 1 { "" } else { "s" },
                if prompts == 1 { "" } else { "s" },
            ),
            Style::new().fg(MUTED),
        )));
        lines.push(Line::raw(""));
        // Most recent first — the run you most likely care about leads.
        for ep in episodes.iter().rev() {
            let (glyph, gstyle, label) = if ep.is_open() {
                (
                    theme::agent_spinner(app.frame),
                    Style::new().fg(ORANGE_400).bold(),
                    "running…".to_string(),
                )
            } else {
                let label = crate::ledger::outcome_label(ep.outcome);
                let (g, c) = ledger_outcome_glyph(ep.outcome);
                (g, Style::new().fg(c), label.to_string())
            };
            let mut spans = vec![
                Span::styled(format!("   {glyph} "), gstyle),
                Span::styled(format!("{label:<9} "), gstyle),
                Span::styled(
                    format!("started {}", crate::ledger::ago(now, ep.started_at)),
                    Style::new().fg(INK),
                ),
            ];
            if let Some(secs) = ep.duration_secs() {
                spans.push(Span::styled(
                    format!("  · ran {}", crate::ledger::duration_label(secs)),
                    Style::new().fg(MUTED),
                ));
            }
            if ep.needs_you > 0 {
                spans.push(Span::styled(
                    format!("  · ⚑{}", ep.needs_you),
                    Style::new().fg(AMBER_400),
                ));
            }
            lines.push(Line::from(spans));
        }
    }
    lines.push(Line::raw(""));

    // A long run-history can outgrow a short terminal, so scroll instead of clipping the
    // oldest runs off the bottom (the same H3/A5 fix help and summary got). Clamp the
    // offset to the real content height and write it back, so an over-scroll leaves no
    // dead presses before it moves.
    let content = lines.len() as u16;
    let area = centered_rect(78, content + 2, frame.area());
    frame.render_widget(Clear, area);
    let inner_h = area.height.saturating_sub(2);
    let max_scroll = content.saturating_sub(inner_h);
    app.ledger_scroll = app.ledger_scroll.min(max_scroll);
    let scroll = app.ledger_scroll;
    // Dismiss only on Esc / `t`, matching help & summary; the hint replaces the old
    // "any key to close" so the convention reads the same across all three overlays.
    let hint = if max_scroll > 0 {
        " ↑↓ scroll · esc / t close "
    } else {
        " esc / t close "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(GREEN_600))
        .title(Line::from(Span::styled(
            " agent ledger ",
            Style::new().fg(GREEN_500).bold(),
        )))
        .title_bottom(Line::from(Span::styled(hint, Style::new().fg(MUTED))).right_aligned());
    frame.render_widget(Paragraph::new(lines).block(block).scroll((scroll, 0)), area);
}

/// Glyph + colour for a finished run's outcome in the ledger — shapes match the
/// fleet markers (`agent_marker`) so the vocabulary is consistent, and each
/// differs by shape (not only colour) for monochrome terminals.
fn ledger_outcome_glyph(outcome: Option<AgentStatus>) -> (&'static str, Color) {
    match outcome {
        Some(AgentStatus::Done) => ("✓", STATUS_400),
        Some(AgentStatus::Failed) => ("✗", RED_400),
        Some(AgentStatus::Stopped) => ("◼", MUTED),
        // Closed without a terminal verdict (interrupted / reaped raw).
        _ => ("·", MUTED),
    }
}

// ── Line builders ─────────────────────────────────────────────────────────

/// One issue row for the spine list / roster: agent-marker · status · priority ·
/// KEY · title (· blocked · cycle). The leftmost gutter holds the live agent
/// marker (or a blank when the issue has no agent).
#[allow(clippy::too_many_arguments)]
fn issue_line<'a>(
    graph: &Graph,
    key: &str,
    agent: Option<AgentStatus>,
    frame: u64,
    flash: Option<Flash>,
    ready_band: bool,
    dispatch: bool,
    selected_here: bool,
) -> Line<'a> {
    let Some(issue) = graph.get(key) else {
        return Line::from(key.to_string());
    };
    let (glyph, color) = theme::status_glyph(issue.status);
    let (pmark, pcolor) = theme::priority_marker(issue.priority);
    // A finished/abandoned issue recedes so open work reads first.
    let resolved = issue.status.is_resolved();

    // Left gutter: the live agent marker (spinner while working, breathing flag
    // for needs-you, ✓ done) pinned to a FIXED leftmost column so it stays
    // visible no matter how long the title is — it used to ride the right edge
    // and a long title pushed it off-screen. Always two columns — marker (or
    // space) plus a trailing space — so the marker never butts against the
    // status glyph (`⚑◇` → `⚑ ◇`) and the status column stays aligned whether or
    // not an issue has an agent. The trailing space is fg-only styled, so it
    // shows nothing of its own — the whole-row tint (`agent_row_bg`) covers it.
    let gutter = if ready_band {
        // READY band owns the gutter with the dispatch rail (bright `▸`, no hue —
        // racing green is reserved for the agent/selection). Two exceptions surface
        // a *terminal* agent that reverted its issue to Ready, so a re-dispatchable
        // row isn't pixel-identical to fresh, never-launched work (H5):
        //   • Failed → red `✗` ("re-dispatch, it crashed");
        //   • Stopped → `◼` ("you stopped this; resumable").
        // A *clean* Done revert still shows the rail, never a green `✓` that would
        // read as success and contradict the READY band.
        match agent {
            Some(s @ (AgentStatus::Failed | AgentStatus::Stopped)) => {
                let (mark, mstyle) = theme::agent_marker(s, frame);
                Span::styled(format!("{mark} "), mstyle)
            }
            // No dispatch rail in the read-only demo / a degraded run — Enter can
            // launch nothing there, so the lane shows no launchpad arrow (H6).
            _ if !dispatch => Span::raw("  "),
            // Blank on the focused-selected row, where the list's own `▸ ` highlight
            // cursor already draws the arrow (no doubled `▸ ▸`).
            _ if selected_here => Span::raw("  "),
            _ => Span::styled("▸ ", Style::new().fg(INK).bold()),
        }
    } else if let Some(status) = agent {
        let (mark, mstyle) = theme::agent_marker(status, frame);
        Span::styled(format!("{mark} "), mstyle)
    } else {
        Span::raw("  ")
    };
    // A resolved issue's key + title dim; the status glyph stays bright as the
    // scannable "done" marker.
    let key_style = match flash {
        Some(Flash::Launched) => Style::new().fg(INK).bg(GREEN_700).bold(),
        Some(Flash::Finished) => Style::new().fg(GREEN_100).bg(STATUS_600).bold(),
        Some(Flash::Failed) => Style::new().fg(INK).bg(RED_400).bold(),
        Some(Flash::Stopped) => Style::new().fg(INK).bg(BORDER).bold(),
        None if resolved => Style::new().fg(MUTED).add_modifier(Modifier::DIM),
        None => Style::new().fg(INK).bold(),
    };
    let title_style = if resolved {
        Style::new().fg(MUTED).add_modifier(Modifier::DIM)
    } else {
        Style::new().fg(MUTED)
    };
    let mut spans = vec![
        gutter,
        Span::styled(format!("{glyph} "), Style::new().fg(color)),
        Span::styled(format!("{pmark} "), Style::new().fg(pcolor)),
        Span::styled(format!("{:<8} ", issue.key), key_style),
        Span::styled(truncate(&issue.title, MAX_TITLE), title_style),
    ];
    if graph.is_blocked(key) {
        spans.push(Span::styled(" ⊘", Style::new().fg(AMBER_400)));
    }
    if graph.in_cycle(key) {
        spans.push(Span::styled(" ↺", Style::new().fg(AMBER_400)));
    }
    Line::from(spans)
}

/// One issue row as a list item, carrying the whole-row status tint on the
/// *item* style (so it spans the full row including the highlight gutter).
fn issue_item<'a>(app: &App, key: &str, ready_band: bool, selected_here: bool) -> ListItem<'a> {
    let flash = app
        .flash
        .get(key)
        .and_then(|&(kind, until)| (app.frame < until).then_some(kind));
    let status = display_status(app, key, app.fleet.get(key).copied());
    let item = ListItem::new(issue_line(
        &app.graph,
        key,
        status,
        app.frame,
        flash,
        ready_band,
        app.workspace.is_some(),
        selected_here,
    ));
    // A READY-band row is the dispatch launchpad: no agent row tint — a terminal
    // agent that reverted to Ready must not paint a stale (possibly green)
    // background under the rail. (The selected row's highlight bg covers it anyway.)
    match status {
        Some(s) if !ready_band => item.style(Style::new().bg(theme::agent_row_bg(s, app.frame))),
        _ => item,
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

/// Node tint for the readiness-colored Fleet overview (ENG-560): the classifier,
/// not the raw workflow status, drives the colour — reusing the existing hue
/// vocabulary, never a new one. Ready is bright `INK`, **never green** (the
/// theme house rule reserves green for the agent / selection — and raw
/// `status_glyph` painted "Started" issues green, which this corrects); Blocked
/// keeps the amber of its `⊘`; Done recedes to `MUTED`. NeedsYou / Working / Idle
/// match their agent-marker hues so the map and the spine bands read the same
/// (Idle's status-green is the resting-agent hue — green here *is* the agent).
fn readiness_tint(r: Readiness) -> Color {
    match r {
        Readiness::NeedsYou => RED_400,
        Readiness::Working => ORANGE_400,
        Readiness::Idle => STATUS_400,
        Readiness::Ready => INK,
        Readiness::Blocked => AMBER_400,
        Readiness::Done => MUTED,
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

    #[test]
    fn the_fleet_recolor_reuses_hues_and_never_paints_a_graph_band_green() {
        // ENG-560: node tints come from the readiness classifier, not the raw
        // workflow status, reusing the existing hue vocabulary.
        assert_eq!(readiness_tint(Readiness::NeedsYou), RED_400);
        assert_eq!(readiness_tint(Readiness::Working), ORANGE_400);
        assert_eq!(readiness_tint(Readiness::Idle), STATUS_400);
        assert_eq!(readiness_tint(Readiness::Ready), INK);
        assert_eq!(readiness_tint(Readiness::Blocked), AMBER_400);
        assert_eq!(readiness_tint(Readiness::Done), MUTED);
        // Load-bearing house rule: green signals the agent / selection — so the
        // agent-less *graph* bands (Ready/Blocked/Done) must never be green (raw
        // `status_glyph` made "Started" issues status-green — exactly what 560
        // corrects). The agent bands carry their own agent-marker hues; Idle's
        // green is the resting-agent green, which *is* green-as-the-agent.
        for r in [Readiness::Ready, Readiness::Blocked, Readiness::Done] {
            assert_ne!(
                readiness_tint(r),
                GREEN_400,
                "{r:?} must not be racing green"
            );
            assert_ne!(
                readiness_tint(r),
                STATUS_400,
                "{r:?} must not be status green"
            );
        }
    }

    #[test]
    fn ready_band_row_shows_the_rail_not_a_terminal_agent_marker() {
        // Review regression: a terminal (Done) agent reverts its issue to the
        // READY band; the gutter must be the bright ▸ rail, never the agent's ✓
        // (which is green) — the row must never contradict its own band header.
        use crate::model::{Issue, Priority, Status};
        let mut g = Graph::new("t");
        g.add_issue(Issue {
            key: "A".into(),
            title: "a".into(),
            status: Status::Unstarted,
            priority: Priority::None,
            assignee: None,
            external: false,
        });
        g.finalize();
        // ready_band = true, not the selected row, with a Done agent present.
        let line = issue_line(&g, "A", Some(AgentStatus::Done), 0, None, true, true, false);
        assert_eq!(
            line.spans[0].content, "▸ ",
            "a ready row shows the dispatch rail"
        );
        assert_eq!(
            line.spans[0].style.fg,
            Some(INK),
            "the rail is INK — never the terminal agent's green ✓"
        );
        // On the focused-selected row the list's highlight cursor provides the
        // arrow, so the gutter is blank (no doubled ▸ ▸).
        let sel = issue_line(&g, "A", Some(AgentStatus::Done), 0, None, true, true, true);
        assert_eq!(
            sel.spans[0].content, "  ",
            "selected ready row defers the arrow to the highlight cursor"
        );
        // H6: with no live workspace (read-only demo / degraded) the dispatch rail
        // disappears — Enter can launch nothing, so the lane shows no launchpad arrow.
        let demo = issue_line(&g, "A", None, 0, None, true, false, false);
        assert_eq!(
            demo.spans[0].content, "  ",
            "no dispatch rail without a workspace"
        );
        assert!(
            !band_divider_line(Readiness::Ready, 40, false)
                .spans
                .iter()
                .any(|s| s.content.contains("dispatch")),
            "no dispatch hint without a workspace"
        );
        assert!(
            band_divider_line(Readiness::Ready, 40, true)
                .spans
                .iter()
                .any(|s| s.content.contains("dispatch")),
            "the dispatch hint returns with a workspace"
        );
    }

    #[test]
    fn a_failed_agent_reverted_to_ready_shows_the_red_cross_not_the_rail() {
        // H5: a crash reverts its issue to the READY band; the gutter must surface
        // the red ✗ (re-dispatch — it crashed), never the plain rail that would be
        // pixel-identical to fresh, never-launched work.
        use crate::model::{Issue, Priority, Status};
        let mut g = Graph::new("t");
        g.add_issue(Issue {
            key: "A".into(),
            title: "a".into(),
            status: Status::Unstarted,
            priority: Priority::None,
            assignee: None,
            external: false,
        });
        g.finalize();
        let line = issue_line(
            &g,
            "A",
            Some(AgentStatus::Failed),
            0,
            None,
            true,
            true,
            false,
        );
        assert_eq!(line.spans[0].content, "✗ ", "a crashed ready row shows ✗");
        assert_eq!(
            line.spans[0].style.fg,
            Some(RED_400),
            "the crash marker is red, never the dispatch rail's INK"
        );
        // Even when selected: ✗ is a distinct glyph from the cursor's ▸, so there's
        // no doubling to suppress — the crash must stay visible.
        let sel = issue_line(
            &g,
            "A",
            Some(AgentStatus::Failed),
            0,
            None,
            true,
            true,
            true,
        );
        assert_eq!(sel.spans[0].content, "✗ ", "selection never hides a crash");
    }

    #[test]
    fn rail_overflow_renders_the_count_in_its_one_row_slot() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        // The rail always hands the overflow marker an exactly-one-row slot. The old
        // bordered block spent that single row on its top border and zeroed the inner
        // area, so the `+N` count rendered in 0% of overflow cases (NEW-03). The
        // borderless line must surface the count in a 1-row rect.
        let mut term = Terminal::new(TestBackend::new(32, 1)).unwrap();
        term.draw(|f| render_rail_overflow(f, Rect::new(0, 0, 32, 1), 7))
            .unwrap();
        let out = term.backend().to_string();
        assert!(
            out.contains("+7"),
            "the hidden-count `+7` must render in a 1-row slot, got {out:?}"
        );
    }

    #[test]
    fn the_needs_you_band_header_is_red_and_distinct_from_blocked_and_working() {
        // R5 / CF-13: NEEDS YOU must read as red (must-act) and stay distinct in hue
        // from the amber BLOCKED and orange WORKING headers — three warm bands that
        // must never be confused at a glance.
        let needs = band_header(Readiness::NeedsYou).2;
        let blocked = band_header(Readiness::Blocked).2;
        let working = band_header(Readiness::Working).2;
        assert_eq!(needs, RED_400, "NEEDS YOU is red");
        assert_ne!(needs, blocked, "NEEDS YOU red must differ from BLOCKED amber");
        assert_ne!(needs, working, "NEEDS YOU red must differ from WORKING orange");
        assert_ne!(blocked, working, "BLOCKED amber must differ from WORKING orange");
    }

    #[test]
    fn band_headers_never_stutter_with_their_member_rows() {
        // A band header must be visually distinct (glyph OR colour) from the gutter
        // marker of the rows beneath it, so the header reads as a header and not as
        // just another row — the v1.7 IDLE `◦`/`◦` and WORKING-vs-`◉` regressions.
        // READY is exempt: its rows intentionally echo the `▸` rail (covered by
        // `ready_band_row_shows_the_rail_not_a_terminal_agent_marker`).
        let cases = [
            (AgentStatus::Spawning, Readiness::Working),
            (AgentStatus::Running, Readiness::Working),
            (AgentStatus::NeedsYou, Readiness::NeedsYou),
            (AgentStatus::Idle, Readiness::Idle),
            (AgentStatus::Done, Readiness::Done),
        ];
        for (status, band) in cases {
            let (hglyph, _, haccent) = band_header(band);
            let (mglyph, mstyle) = theme::agent_marker(status, 0);
            assert!(
                hglyph != mglyph || Some(haccent) != mstyle.fg,
                "{band:?} header `{hglyph}` is identical in glyph+colour to its \
                 {status:?} row marker `{mglyph}` — it will stutter"
            );
        }
    }

    #[test]
    fn band_header_glyphs_are_off_the_priority_and_repo_check_sets() {
        // The v1.7 WORKING header reused the repo-select `◉`; guard the whole class
        // — no band-header glyph may collide with a priority marker or a repo
        // checkbox, the same monochrome discipline theme.rs enforces within each set.
        use crate::model::Priority;
        let bands = [
            Readiness::NeedsYou,
            Readiness::Working,
            Readiness::Idle,
            Readiness::Ready,
            Readiness::Blocked,
            Readiness::Done,
        ];
        for band in bands {
            let (hglyph, _, _) = band_header(band);
            for p in [
                Priority::Urgent,
                Priority::High,
                Priority::Medium,
                Priority::Low,
            ] {
                assert_ne!(
                    hglyph,
                    theme::priority_marker(p).0,
                    "{band:?} header collides with priority {p:?}"
                );
            }
            for checked in [true, false] {
                assert_ne!(
                    hglyph,
                    theme::repo_check(checked).0,
                    "{band:?} header collides with a repo checkbox"
                );
            }
        }
    }
}
