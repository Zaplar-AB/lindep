//! First-run Linear API key entry.
//!
//! When `lindep` starts on an interactive terminal with no usable `LINEAR_API_KEY`,
//! let the user paste a key *in the app* — validate it against Linear, persist it to
//! `~/.config/lindep/.env`, and continue the same run — instead of bouncing them to a
//! stderr hint they must act on and relaunch (the cold-start dead-end the demo banner
//! ironically points back into).
//!
//! An exported / already-`.env`-loaded key always wins: we never prompt when one
//! already works (see `main::ensure_key_interactive`), so scripts and CI are
//! unaffected, and a non-TTY run keeps the old stderr behaviour.

use std::io;
use std::path::{Path, PathBuf};

use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::linear::Client;
use crate::theme::{AMBER_400, GREEN_100, GREEN_400, INK, MUTED, RED_400, VIOLET_400};

/// Run the key-entry screen. Returns `Ok(Some(key))` once a key validates against
/// Linear (and has been persisted + exported for this run), or `Ok(None)` when the
/// user cancels (the caller then falls back to the stderr hint). Owns its own
/// alternate screen; the caller restores nothing.
pub fn prompt_for_key() -> Result<Option<String>, String> {
    let mut terminal = ratatui::init();
    let outcome = run_loop(&mut terminal);
    ratatui::restore();
    outcome
}

struct KeyPrompt {
    input: String,
    error: Option<String>,
    /// True while a validation probe is in flight, so the screen can say "checking…"
    /// before the (blocking, but timeout-bounded) Linear round-trip.
    checking: bool,
}

fn run_loop(terminal: &mut DefaultTerminal) -> Result<Option<String>, String> {
    let mut state = KeyPrompt {
        input: String::new(),
        error: None,
        checking: false,
    };
    loop {
        terminal
            .draw(|frame| draw(&state, frame))
            .map_err(|e| e.to_string())?;
        let Event::Key(key) = event::read().map_err(|e| e.to_string())? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Ctrl-C and Esc both skip the prompt (→ stderr hint + the --demo suggestion).
        if ctrl && matches!(key.code, KeyCode::Char('c')) {
            return Ok(None);
        }
        match key.code {
            KeyCode::Esc => return Ok(None),
            KeyCode::Enter => {
                let candidate = state.input.trim().to_string();
                if candidate.is_empty() || candidate == "lin_api_xxxxxxxx" {
                    state.error = Some(
                        "paste your lin_api_… key, or press esc to quit; run `lindep --demo` for demo"
                            .into(),
                    );
                    continue;
                }
                // Probe Linear so a bad key is caught here, not after the picker. Paint
                // a "checking…" frame first — the call blocks up to the client's
                // 30s/10s timeouts, so the user shouldn't see a frozen screen.
                state.checking = true;
                state.error = None;
                terminal
                    .draw(|frame| draw(&state, frame))
                    .map_err(|e| e.to_string())?;
                match Client::new(candidate.clone()).list_projects() {
                    Ok(projects) if projects.is_empty() => {
                        state.checking = false;
                        state.error = Some(
                            "key works, but this account has no visible projects — check Linear, or esc to quit"
                                .into(),
                        );
                    }
                    Ok(_) => {
                        // Best-effort persistence — a working key shouldn't fail the
                        // launch just because the disk write hiccupped; worst case the
                        // next run prompts again.
                        let _ = persist_key(&candidate);
                        // Use it for the rest of this run. Safe: we're single-threaded
                        // here (pre-TUI, before any tokio runtime spawns).
                        unsafe { std::env::set_var("LINEAR_API_KEY", &candidate) };
                        return Ok(Some(candidate));
                    }
                    Err(e) => {
                        state.checking = false;
                        state.error = Some(e);
                    }
                }
            }
            KeyCode::Backspace => {
                state.input.pop();
                state.error = None;
            }
            KeyCode::Char(c) if !ctrl => {
                state.input.push(c);
                state.error = None;
            }
            _ => {}
        }
    }
}

fn draw(state: &KeyPrompt, frame: &mut Frame) {
    let area = centered(frame.area(), 66, 11);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(VIOLET_400).add_modifier(Modifier::BOLD))
        .title(Span::styled(
            " connect lindep to Linear ",
            Style::new().fg(GREEN_100).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // The status line: the in-flight probe, the last error (red), or a hint.
    let status = if state.checking {
        Span::styled("  checking with Linear…", Style::new().fg(AMBER_400))
    } else if let Some(err) = &state.error {
        Span::styled(format!("  {err}"), Style::new().fg(RED_400))
    } else {
        Span::styled(
            "  ⏎ validate & save to ~/.config/lindep/.env · esc quit",
            Style::new().fg(MUTED),
        )
    };

    let (shown, shown_style) = if state.input.is_empty() {
        ("lin_api_…".to_string(), Style::new().fg(MUTED))
    } else {
        (
            state.input.clone(),
            Style::new().fg(GREEN_100).add_modifier(Modifier::BOLD),
        )
    };

    let lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "  No LINEAR_API_KEY found — paste a personal key to launch agents:",
            Style::new().fg(INK),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled("  key  ", Style::new().fg(MUTED)),
            Span::styled(shown, shown_style),
            Span::styled("▏", Style::new().fg(GREEN_400)),
        ]),
        Line::raw(""),
        Line::from(status),
        Line::raw(""),
        Line::from(Span::styled(
            "  create one at https://linear.app/settings/api",
            Style::new().fg(MUTED),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

/// A centered sub-rect, clamped so a tiny terminal never panics.
fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect::new(x, y, w, h)
}

/// Upsert `LINEAR_API_KEY=<key>` into `~/.config/lindep/.env`, preserving every other
/// line. Atomic (tmp + rename). Returns the written path.
fn persist_key(key: &str) -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "$HOME is not set"))?;
    let dir = Path::new(&home).join(".config/lindep");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(".env");
    let body = upsert_env(&std::fs::read_to_string(&path).unwrap_or_default(), key);
    let tmp = dir.join(".env.tmp");
    std::fs::write(&tmp, body.as_bytes())?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Replace an existing `LINEAR_API_KEY=` line in `existing`, or append one, leaving
/// every other line untouched. Pure, so the upsert logic is unit-testable.
fn upsert_env(existing: &str, key: &str) -> String {
    let line = format!("LINEAR_API_KEY={key}");
    let mut replaced = false;
    let mut out: Vec<String> = existing
        .lines()
        .map(|l| {
            if l.trim_start().starts_with("LINEAR_API_KEY=") {
                replaced = true;
                line.clone()
            } else {
                l.to_string()
            }
        })
        .collect();
    if !replaced {
        out.push(line);
    }
    let mut body = out.join("\n");
    body.push('\n');
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn upsert_appends_when_no_key_line_exists() {
        // A first key is appended, and an unrelated line is preserved.
        let out = upsert_env("FOO=bar\n", "lin_api_new");
        assert!(out.contains("FOO=bar"), "unrelated lines survive: {out}");
        assert!(
            out.contains("LINEAR_API_KEY=lin_api_new"),
            "key appended: {out}"
        );
    }

    #[test]
    fn upsert_replaces_an_existing_key_line_without_duplicating() {
        let out = upsert_env("FOO=bar\nLINEAR_API_KEY=old\nBAZ=qux\n", "lin_api_new");
        assert!(
            out.contains("FOO=bar") && out.contains("BAZ=qux"),
            "others survive"
        );
        assert!(out.contains("LINEAR_API_KEY=lin_api_new"), "key replaced");
        assert!(!out.contains("old"), "stale key removed");
        assert_eq!(
            out.matches("LINEAR_API_KEY=").count(),
            1,
            "no duplicate key line"
        );
    }

    #[test]
    fn upsert_into_empty_yields_just_the_key_line() {
        assert_eq!(upsert_env("", "lin_api_x"), "LINEAR_API_KEY=lin_api_x\n");
    }

    #[test]
    fn the_prompt_renders_without_panicking_at_several_sizes() {
        // Mirrors the wizard's adversarial-size render test: a tiny terminal must
        // clamp, not panic.
        for (w, h) in [(80, 24), (40, 10), (20, 4), (8, 2)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            let state = KeyPrompt {
                input: "lin_api_abc".into(),
                error: Some("401 Unauthorized".into()),
                checking: false,
            };
            terminal.draw(|frame| draw(&state, frame)).unwrap();
        }
    }
}
