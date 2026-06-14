//! Zaplar Design System (ZDS) palette and the glyph/colour mapping for issue
//! state and priority.
//!
//! House rule from ZDS: **racing green is reserved for actions / the agent /
//! the active selection.** Status is communicated with the status-green ramp;
//! amber carries warnings (blocked, urgent, cycles); graphite is structure and
//! quiet text. Everything assumes a dark terminal background.

use ratatui::style::{Color, Modifier, Style};

use crate::model::{Priority, Status};
use crate::session::AgentStatus;

// ── Racing green — selection / active affordances only ──────────────────────
pub const GREEN_100: Color = Color::Rgb(0xEA, 0xF1, 0xED);
pub const GREEN_400: Color = Color::Rgb(0x6F, 0xA6, 0x8C);
pub const GREEN_500: Color = Color::Rgb(0x41, 0x85, 0x5F);
pub const GREEN_600: Color = Color::Rgb(0x20, 0x69, 0x4A);
pub const GREEN_700: Color = Color::Rgb(0x15, 0x52, 0x39);

// ── Graphite neutrals — text and structure ─────────────────────────────────
pub const INK: Color = Color::Rgb(0xF4, 0xF5, 0xF6); // primary text
pub const MUTED: Color = Color::Rgb(0x8A, 0x8F, 0x96); // secondary / dim text
pub const BORDER: Color = Color::Rgb(0x2C, 0x2F, 0x33); // idle pane borders
pub const WELL: Color = Color::Rgb(0x14, 0x15, 0x17); // footer well

// ── Amber — warnings: blocked, urgent, cycles ───────────────────────────────
pub const AMBER_400: Color = Color::Rgb(0xF0, 0xAD, 0x43);
pub const AMBER_500: Color = Color::Rgb(0xE1, 0x8C, 0x1B);

// ── Status-green ramp — workflow state ──────────────────────────────────────
pub const STATUS_400: Color = Color::Rgb(0x4F, 0xC9, 0x8C);
pub const STATUS_600: Color = Color::Rgb(0x0F, 0x84, 0x44);

/// Selection style for the pane that currently holds focus — the one moving,
/// racing-green element on screen.
pub fn cursor_active() -> Style {
    Style::new()
        .bg(GREEN_700)
        .fg(GREEN_100)
        .add_modifier(Modifier::BOLD)
}

/// Selection style for a pane that is *not* focused: we still mark where the
/// cursor rests, but quietly, so only one selection ever looks "live".
pub fn cursor_idle() -> Style {
    Style::new().fg(GREEN_400)
}

/// Glyph + colour for a workflow state. Matched defensively — Linear's
/// `state.type` is a plain string, so unknown values fall back to a neutral dot.
pub fn status_glyph(status: Status) -> (&'static str, Color) {
    match status {
        Status::Completed | Status::Duplicate => ("●", STATUS_600),
        Status::Started => ("◐", STATUS_400),
        Status::Unstarted => ("○", INK),
        Status::Backlog => ("○", MUTED),
        Status::Triage => ("◇", AMBER_400),
        Status::Canceled => ("⊘", MUTED),
        Status::Unknown => ("·", MUTED),
    }
}

/// Glyph + colour for an agent's state on its issue node. Racing green marks a
/// live agent (the house rule: green = the agent / active); amber pulls the eye
/// to the one state that needs the human.
pub fn agent_glyph(status: AgentStatus) -> (&'static str, Color) {
    match status {
        AgentStatus::Spawning => ("◌", GREEN_400),
        AgentStatus::Running => ("▸", GREEN_500),
        AgentStatus::NeedsYou => ("⚑", AMBER_400),
        AgentStatus::Idle => ("◦", GREEN_400),
        AgentStatus::Done => ("✓", STATUS_400),
        AgentStatus::Failed => ("✗", AMBER_500),
    }
}

/// Glyph + colour for priority. A leading space keeps the column aligned when a
/// priority marker is absent.
pub fn priority_marker(priority: Priority) -> (&'static str, Color) {
    match priority {
        Priority::Urgent => ("▲", AMBER_500),
        Priority::High => ("△", AMBER_400),
        Priority::Medium => ("◦", MUTED),
        Priority::Low => ("▽", MUTED),
        Priority::None => (" ", MUTED),
    }
}
