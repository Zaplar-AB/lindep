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
// Darker still — the quiet idle-cursor background. Dimmer than GREEN_700 (so the
// idle selection stays subordinate to the active one) yet brighter in the green
// channel than any `agent_row_bg` tint, so the cursor reads on a coloured row.
pub const GREEN_900: Color = Color::Rgb(0x0E, 0x3A, 0x29);

// ── Graphite neutrals — text and structure ─────────────────────────────────
pub const INK: Color = Color::Rgb(0xF4, 0xF5, 0xF6); // primary text
pub const MUTED: Color = Color::Rgb(0x8A, 0x8F, 0x96); // secondary / dim text
pub const BORDER: Color = Color::Rgb(0x2C, 0x2F, 0x33); // idle pane borders
pub const WELL: Color = Color::Rgb(0x14, 0x15, 0x17); // footer well

// ── Amber — warnings: blocked, urgent, cycles. Deliberately *not* reused for
//    "working" or "needs-you" — see ORANGE / RED. ───────────────────────────
pub const AMBER_400: Color = Color::Rgb(0xF0, 0xAD, 0x43);
pub const AMBER_500: Color = Color::Rgb(0xE1, 0x8C, 0x1B);
/// Deep amber — the selected-row fill while command mode (the sticky prefix) is
/// armed; pairs with the amber border so the whole focused surface shifts hue.
pub const AMBER_900: Color = Color::Rgb(0x45, 0x31, 0x0E);

// ── Orange — an agent actively working ("doing things"). Warm and lively like
//    the user asked for, but a distinct hue from amber so "needs you" stays the
//    single most eye-grabbing state. Working earns attention through *motion*
//    (the spinner), not by competing with amber. ──────────────────────────────
pub const ORANGE_400: Color = Color::Rgb(0xF2, 0x7A, 0x21);

// ── Status-green ramp — workflow state ──────────────────────────────────────
pub const STATUS_400: Color = Color::Rgb(0x4F, 0xC9, 0x8C);
pub const STATUS_600: Color = Color::Rgb(0x0F, 0x84, 0x44);

// ── Red — failed agents and the one must-act agent state (needs-you). Distinct
//    from amber so human attention reads as urgent, not merely warning. ───────
pub const RED_300: Color = Color::Rgb(0xF2, 0x7A, 0x68);
pub const RED_400: Color = Color::Rgb(0xE0, 0x5A, 0x4B);

// ── Violet — the cockpit-v3 focus colour. House rule: racing green stays
//    reserved for the agent / active selection (see the module header), so the
//    *window-manager* focus ring needs its own hue that never competes with a
//    green selection inside a pane. Violet is that hue — the thick/double border
//    of whichever column currently owns your keys. VIOLET_400 is the steady
//    focus ring; VIOLET_200 is a brighter accent for a focused title chip. ─────
pub const VIOLET_400: Color = Color::Rgb(0x9B, 0x87, 0xF5);
pub const VIOLET_200: Color = Color::Rgb(0xC4, 0xB9, 0xFA);

/// Border style for the focused window — a steady (frame-independent) violet, so
/// the focus ring never strobes. Pairs with `BorderType::Double` at the call
/// site; status hue ([`window_status_hue`]) carries the *unfocused* borders.
pub fn focus_border_style(armed: bool) -> Style {
    Style::new().fg(focus_accent(armed)).add_modifier(Modifier::BOLD)
}

/// The focus accent: violet at rest, amber while command mode (the sticky prefix)
/// is armed. Used by the focused border and its title bar leader — and, *only
/// while armed*, the selected-row fill + nav chip — so the focused surface shifts
/// to one amber wash the moment you arm a verb, then returns to violet focus +
/// green selection. Keeping the selection green at rest is deliberate: focus
/// (which pane owns your keys) and selection (where the row cursor rests) are
/// distinct signals shown at once on different panes, so they get distinct hues.
pub fn focus_accent(armed: bool) -> Color {
    if armed { AMBER_400 } else { VIOLET_400 }
}

/// Border hue + short label for an *unfocused* window, by its agent status — the
/// status the title bar / border communicates at a glance (running-orange,
/// needs-you breathing-red, idle-cyan…). Lifted verbatim from the v2
/// `chat_pane_chrome` so the colour vocabulary is unchanged; `None` is a
/// non-agent window (Deps/Spine) or one whose status hasn't landed yet, and
/// `exited` is the sub-frame window where the PTY is gone but no terminal status
/// has arrived. needs-you *breathes* via [`needs_you_style`] at the call site.
pub fn window_status_hue(status: Option<AgentStatus>, exited: bool) -> (Color, &'static str) {
    match status {
        // Spawning shares the WORKING word with Running — both are the Working
        // readiness band; the steady ◌ marker (vs Running's spinner) carries the
        // "just starting" nuance non-verbally, so the title word stays unified.
        Some(AgentStatus::Spawning) => (GREEN_400, "WORKING"),
        Some(AgentStatus::Running) => (ORANGE_400, "WORKING"),
        Some(AgentStatus::NeedsYou) => (RED_400, "NEEDS YOU"),
        Some(AgentStatus::Idle) => (STATUS_400, "IDLE"),
        Some(AgentStatus::Stopped) => (MUTED, "STOPPED"),
        Some(AgentStatus::Done) => (STATUS_400, "DONE"),
        Some(AgentStatus::Failed) => (RED_400, "FAILED"),
        None if exited => (AMBER_400, "EXITED"),
        None => (BORDER, "AGENT"),
    }
}

/// Selection style for the pane that currently holds focus: racing-green at rest
/// (the GREEN_700 step above the idle GREEN_900), deep amber (AMBER_900) only
/// while command mode is armed — so the selection keeps its own green hue, clearly
/// distinct from the violet focus border, except during the transient armed wash.
pub fn cursor_active(armed: bool) -> Style {
    Style::new()
        .bg(if armed { AMBER_900 } else { GREEN_700 })
        .fg(if armed { INK } else { GREEN_100 })
        .add_modifier(Modifier::BOLD)
}

/// Selection style for a pane that is *not* focused: we still mark where the
/// cursor rests, but quietly, so only one selection ever looks "live". Carries a
/// dim green *background* (not just green text) so the cursor stays legible even
/// when it lands on a whole-row [`agent_row_bg`] tint — without that bg the only
/// cue would be a grey→green text shift over a coloured row, easy to miss. Still
/// clearly subordinate to [`cursor_active`]: dimmer, and not bold.
pub fn cursor_idle() -> Style {
    Style::new().bg(GREEN_900).fg(GREEN_100)
}

/// Glyph + colour for a workflow state. Matched defensively — Linear's
/// `state.type` is a plain string, so unknown values fall back to a neutral dot.
pub fn status_glyph(status: Status) -> (&'static str, Color) {
    // One shape per state, so the column survives a monochrome terminal (see
    // `status_glyphs_disambiguate_the_colour_only_collisions`): Backlog is `·` (not a second `○` that only
    // colour told apart from Todo), and Canceled is `⊗` (not `⊘`, which is reserved for
    // *blocked* — a live "needs unblocking" state, the opposite of a dead one).
    match status {
        // STATUS_400 (8.76:1), not STATUS_600 (3.83:1, below AA): the done dot is the
        // only mark distinguishing a completed issue in the label-less Spine/deps views,
        // so it must stay legible (STATUS_600 remains the Flash::Finished background).
        Status::Completed | Status::Duplicate => ("●", STATUS_400),
        Status::Started => ("◐", STATUS_400),
        Status::Unstarted => ("○", INK),
        Status::Backlog => ("·", MUTED),
        Status::Triage => ("◇", AMBER_400),
        Status::Canceled => ("⊗", MUTED),
        Status::Unknown => ("?", MUTED),
    }
}

/// Braille spinner frames — one single-width cell, so swapping frames never
/// shifts a column. Eight frames at ~10 fps is a smooth, calm rotation.
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// The spinner glyph for a given animation frame.
pub fn agent_spinner(frame: u64) -> &'static str {
    SPINNER[(frame % SPINNER.len() as u64) as usize]
}

/// The pulse style for the needs-you flag: a ~1.2 s heartbeat between two reds.
/// Never a terminal blink attribute (those flash the whole cell and are an
/// accessibility hazard) — just a luminance breath, and NOT BOLD↔DIM: DIM is widely
/// unimplemented (aliased to normal) or renders barely-legible, so the cockpit's most
/// important cue would vanish on those terminals. A colour change every terminal
/// honours keeps the dim half readable — aligning the flag/badges with the row tint,
/// which already breathes between two distinct RGBs.
pub fn needs_you_style(frame: u64) -> Style {
    let hue = if (frame / 6).is_multiple_of(2) {
        RED_400
    } else {
        RED_300
    };
    Style::new().fg(hue).add_modifier(Modifier::BOLD)
}

/// Marker — glyph + full style — for an agent's state at `frame`. This is the
/// single status glyph the spine gutter, the overview chips and the chat-pane
/// titles render: a *working* agent visibly spins, a *needs-you* agent pulses,
/// *starting* shows a steady ◌, and the resting/terminal states are steady. Every
/// state's glyph *shape* differs (not only its colour), so the signal survives a
/// monochrome terminal — see `every_agent_state_has_a_distinct_marker_glyph`.
/// Pure in `frame`, so the renderer stays a function of state (the frame counter
/// lives on `App`).
pub fn agent_marker(status: AgentStatus, frame: u64) -> (&'static str, Style) {
    match status {
        // Starting: a steady dotted ring, distinct from Running's live spin, so a
        // freshly-spawned agent reads as "starting" — it hasn't done anything yet
        // — rather than "working".
        AgentStatus::Spawning => ("◌", Style::new().fg(GREEN_400)),
        AgentStatus::Running => (
            agent_spinner(frame),
            Style::new().fg(ORANGE_400).add_modifier(Modifier::BOLD),
        ),
        AgentStatus::NeedsYou => ("⚑", needs_you_style(frame)),
        AgentStatus::Idle => ("◦", Style::new().fg(STATUS_400)),
        AgentStatus::Stopped => ("◼", Style::new().fg(MUTED)),
        AgentStatus::Done => ("✓", Style::new().fg(STATUS_400)),
        AgentStatus::Failed => ("✗", Style::new().fg(RED_400).add_modifier(Modifier::BOLD)),
    }
}

/// A subtle, dark background tint for a list row whose issue has an agent — so
/// the *whole* row, not just a left gutter, carries the state colour. Kept very
/// dark (low luminance, just enough chroma to read as the status hue) for two
/// reasons: INK / MUTED text must stay legible on a dark terminal, and the
/// racing-green selection highlight ([`cursor_active`], a `GREEN_700` background)
/// must still clearly win on the focused row. needs-you *breathes* in step with
/// [`needs_you_style`] so the one must-act state pulses across the full row;
/// every other state is steady (its marker/spinner already carries any motion).
pub fn agent_row_bg(status: AgentStatus, frame: u64) -> Color {
    match status {
        AgentStatus::Spawning => Color::Rgb(0x10, 0x1C, 0x16),
        AgentStatus::Running => Color::Rgb(0x2B, 0x1A, 0x0C),
        // The same ~1.2 s heartbeat as the flag/header, applied to the row tint.
        AgentStatus::NeedsYou if (frame / 6).is_multiple_of(2) => Color::Rgb(0x3A, 0x12, 0x10),
        AgentStatus::NeedsYou => Color::Rgb(0x24, 0x0E, 0x0D),
        AgentStatus::Idle => Color::Rgb(0x0E, 0x20, 0x19),
        AgentStatus::Stopped => Color::Rgb(0x17, 0x18, 0x1A),
        AgentStatus::Done => Color::Rgb(0x0C, 0x22, 0x17),
        AgentStatus::Failed => Color::Rgb(0x2C, 0x12, 0x10),
    }
}

/// Glyph + style for a repo multi-select checkbox (ENG-536's up-front select): a
/// checked repo reads in racing green (the house colour for the active selection),
/// an unchecked one in muted graphite. The two glyph *shapes* differ (filled vs
/// empty ring) so the state survives a monochrome terminal, like every marker here.
pub fn repo_check(checked: bool) -> (&'static str, Style) {
    if checked {
        ("◉", Style::new().fg(GREEN_500).add_modifier(Modifier::BOLD))
    } else {
        ("○", Style::new().fg(MUTED))
    }
}

/// Glyph + colour for priority. A leading space keeps the column aligned when a
/// priority marker is absent. The glyphs stay off the `▲/▼` triangle family — those
/// are reserved for dependency *direction* (upstream/downstream) everywhere — and off
/// `◦`, which is the idle-agent marker; intensity reads Urgent `‼` › High `!` ›
/// Medium `◻` › Low `▫`. Medium is an *outline* square, not the filled `▪`, so it
/// can't be mistaken for the filled `◼` Stopped-agent marker (also MUTED) one row
/// over — each glyph stays a distinct shape for monochrome legibility.
pub fn priority_marker(priority: Priority) -> (&'static str, Color) {
    match priority {
        Priority::Urgent => ("‼", AMBER_500),
        Priority::High => ("!", AMBER_400),
        Priority::Medium => ("◻", MUTED),
        Priority::Low => ("▫", MUTED),
        Priority::None => (" ", MUTED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const ALL: [AgentStatus; 7] = [
        AgentStatus::Spawning,
        AgentStatus::Running,
        AgentStatus::NeedsYou,
        AgentStatus::Idle,
        AgentStatus::Stopped,
        AgentStatus::Done,
        AgentStatus::Failed,
    ];

    #[test]
    fn every_agent_state_has_a_distinct_marker_glyph() {
        // Colour alone never carries the signal — each state must read in a
        // monochrome terminal too, so the marker glyph shapes must all differ. The
        // spinner frame is fixed here; it's a braille char distinct from every
        // steady glyph at any frame.
        let glyphs: HashSet<&str> = ALL.iter().map(|s| agent_marker(*s, 0).0).collect();
        assert_eq!(
            glyphs.len(),
            ALL.len(),
            "two states share a marker glyph: {glyphs:?}"
        );
    }

    #[test]
    fn the_working_set_shares_one_user_facing_label() {
        // ITEM 7: Spawning and Running are both the Working readiness band
        // (AgentStatus::is_working), so their per-status window-title label must be the
        // SAME word — a freshly-spawned agent must not read as "STARTING" in the title
        // while the band it sits in says "WORKING". The marker glyph (◌ vs spinner)
        // carries the start/active nuance non-verbally; the word stays unified.
        let spawning = window_status_hue(Some(AgentStatus::Spawning), false).1;
        let running = window_status_hue(Some(AgentStatus::Running), false).1;
        assert_eq!(running, "WORKING", "Running renders as the canonical WORKING");
        assert_eq!(
            spawning, running,
            "a spawning agent must share Running's WORKING label, not a second word"
        );
    }

    #[test]
    fn every_agent_status_has_exactly_one_title_label() {
        // A status maps to exactly one user-facing title word — guard against a future
        // edit re-introducing a per-call-site synonym (the running/working split this
        // item closed). Each of the seven states yields a single non-empty label.
        for s in ALL {
            let label = window_status_hue(Some(s), false).1;
            assert!(!label.is_empty(), "{s:?} must carry a title label");
        }
        // The two working-set states collapse to one word; the five others are theirs
        // alone — so the distinct-label count over the 7 states is exactly 6.
        let labels: HashSet<&str> = ALL
            .iter()
            .map(|s| window_status_hue(Some(*s), false).1)
            .collect();
        assert_eq!(
            labels.len(),
            6,
            "Spawning+Running share WORKING; the other five are distinct: {labels:?}"
        );
    }

    #[test]
    fn the_needs_you_row_tint_breathes_while_other_states_hold_steady() {
        // needs-you is the one row tint that pulses (it shares the flag's
        // heartbeat), so two phases of the cycle must differ…
        assert_ne!(
            agent_row_bg(AgentStatus::NeedsYou, 0),
            agent_row_bg(AgentStatus::NeedsYou, 6),
            "the needs-you row tint must breathe between frames"
        );
        // …while a working agent's tint is steady (its spinner carries the motion).
        assert_eq!(
            agent_row_bg(AgentStatus::Running, 0),
            agent_row_bg(AgentStatus::Running, 6),
            "non-needs-you row tints must not flicker"
        );
    }

    #[test]
    fn the_needs_you_pulse_breathes_by_colour_not_dim() {
        // DIM is unportable, so the must-act pulse must change colour, both phases BOLD
        // (never DIM) so it stays legible on terminals that don't render DIM at all.
        let a = needs_you_style(0);
        let b = needs_you_style(6);
        assert_ne!(
            a.fg, b.fg,
            "the pulse changes colour between its two phases"
        );
        assert!(
            a.add_modifier.contains(Modifier::BOLD) && b.add_modifier.contains(Modifier::BOLD),
            "both phases stay bold"
        );
        assert!(
            !a.add_modifier.contains(Modifier::DIM) && !b.add_modifier.contains(Modifier::DIM),
            "never DIM — many terminals render it invisible"
        );
    }

    #[test]
    fn a_repo_checkbox_reads_in_monochrome() {
        // Checked vs unchecked must differ by glyph shape, not only colour.
        assert_ne!(repo_check(true).0, repo_check(false).0);
    }

    #[test]
    fn the_spinner_cycles_through_every_frame_and_never_repeats_adjacently() {
        let frames: Vec<&str> = (0..SPINNER.len() as u64).map(agent_spinner).collect();
        assert_eq!(frames, SPINNER, "agent_spinner walks the frames in order");
        assert_eq!(
            agent_spinner(0),
            agent_spinner(SPINNER.len() as u64),
            "it wraps"
        );
    }

    #[test]
    fn status_glyphs_disambiguate_the_colour_only_collisions() {
        use crate::model::Status;
        // Backlog and Todo used to share `○`, told apart only by colour — now distinct.
        assert_ne!(
            status_glyph(Status::Backlog).0,
            status_glyph(Status::Unstarted).0,
            "Backlog must not reuse Todo's ○"
        );
        // Canceled must not reuse `⊘` (reserved for *blocked*) nor `✗` (a failed agent).
        assert_ne!(
            status_glyph(Status::Canceled).0,
            "⊘",
            "⊘ is blocked, not canceled"
        );
        assert_ne!(
            status_glyph(Status::Canceled).0,
            agent_marker(AgentStatus::Failed, 0).0,
            "canceled is not a crash"
        );
    }

    #[test]
    fn priority_glyphs_are_distinct_and_off_the_reserved_glyphs() {
        use crate::model::Priority;
        const ALL: [Priority; 5] = [
            Priority::Urgent,
            Priority::High,
            Priority::Medium,
            Priority::Low,
            Priority::None,
        ];
        let glyphs: HashSet<&str> = ALL.iter().map(|p| priority_marker(*p).0).collect();
        assert_eq!(glyphs.len(), ALL.len(), "each priority is a distinct glyph");
        // Off the idle-agent `◦` and the direction triangles (those mean graph
        // direction everywhere), so a row never shows the same glyph in two columns.
        for p in ALL {
            let g = priority_marker(p).0;
            assert!(
                !["◦", "▲", "▼", "△", "▽"].contains(&g),
                "{p:?} uses a reserved glyph: {g}"
            );
        }
    }
}
