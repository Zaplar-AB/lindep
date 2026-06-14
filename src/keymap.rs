//! Remappable key bindings for the cockpit.
//!
//! Every cockpit action has a default key but can be rebound in a `[keys]` table
//! in `config.toml`, read from two places (later wins, so personal overrides a
//! repo's): `<repo>/.lindep/config.toml` then `~/.config/lindep/config.toml` —
//! mirroring how `.env` is loaded.
//!
//! ```toml
//! [keys]
//! detach = "f10"          # the one that varies by keyboard/terminal
//! # detach = "ctrl-a d"   # …or a tmux-style leader sequence (detach only)
//! launch-agent = "a"
//! jump-needs-you = ["n", "ctrl-n"]   # an action may have several keys
//! ```
//!
//! Accepted key syntax: `a`, `/`, `?` (single chars); `f1`..`f12`; the named keys
//! `enter` `tab` `backtab` `space` `backspace` `up` `down` `left` `right` `home`
//! `end` `pageup` `pagedown` `delete` `insert`; and `ctrl-`/`alt-` prefixes
//! (`ctrl-<letter>` is the only reliable control combo — see the crossterm note
//! on [`parse_binding`]). `esc` is reserved (a fixed, context-sensitive key).
//!
//! A value with a space (e.g. `"ctrl-a d"`) is a **leader sequence**: press the
//! leader, then the next key. Only `detach` may be a sequence — a reserved-key
//! gesture is needed solely while attached, where the agent wants every single
//! key; the dashboard has plenty of free single keys. Pressing the leader twice
//! while attached sends it through to the agent, so the leader is never lost.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;

/// A remappable cockpit action. `Esc` and the search/help overlays are fixed and
/// deliberately absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Quit,
    MoveUp,
    MoveDown,
    FocusList,
    CyclePane,
    CycleFocus,
    Enter,
    ToggleCollapse,
    Back,
    JumpCycle,
    JumpNeedsYou,
    LaunchAgent,
    CancelAgent,
    Attach,
    Detach,
    CycleFilter,
    CycleSort,
    ToggleGraph,
    StartSearch,
    ToggleHelp,
}

/// `(action, config name, default keys)`. The single source of truth for the
/// default keymap, the accepted config names, and (for `Detach`) the label the
/// attach pane shows.
const DEFAULTS: &[(Action, &str, &[&str])] = &[
    (Action::Quit, "quit", &["q"]),
    (Action::MoveUp, "move-up", &["up", "k"]),
    (Action::MoveDown, "move-down", &["down", "j"]),
    (Action::FocusList, "focus-list", &["left", "h"]),
    (Action::CyclePane, "cycle-pane", &["right", "l"]),
    (Action::CycleFocus, "cycle-focus", &["tab"]),
    (Action::Enter, "enter", &["enter"]),
    (Action::ToggleCollapse, "toggle-collapse", &["space"]),
    (Action::Back, "back", &["backspace", "b"]),
    (Action::JumpCycle, "jump-cycle", &["c"]),
    (Action::JumpNeedsYou, "jump-needs-you", &["n"]),
    (Action::LaunchAgent, "launch-agent", &["a"]),
    (Action::CancelAgent, "stop-agent", &["x"]),
    (Action::Attach, "attach", &["t"]),
    (Action::Detach, "detach", &["f10"]),
    (Action::CycleFilter, "filter", &["f"]),
    (Action::CycleSort, "sort", &["s"]),
    (Action::ToggleGraph, "graph", &["g"]),
    (Action::StartSearch, "search", &["/"]),
    (Action::ToggleHelp, "help", &["?"]),
];

/// A normalized key chord: a [`KeyCode`] plus whether Ctrl/Alt were held. Shift
/// is ignored — it's already reflected in the character a terminal delivers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Binding {
    code: KeyCode,
    ctrl: bool,
    alt: bool,
}

impl Binding {
    fn of(key: KeyEvent) -> Self {
        Binding {
            code: key.code,
            ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
            alt: key.modifiers.contains(KeyModifiers::ALT),
        }
    }

    /// Human label, e.g. `F10`, `Ctrl-A`, `Space`, `↑`.
    fn label(&self) -> String {
        let mut out = String::new();
        if self.ctrl {
            out.push_str("Ctrl-");
        }
        if self.alt {
            out.push_str("Alt-");
        }
        out.push_str(&match self.code {
            KeyCode::Char(' ') => "Space".to_string(),
            KeyCode::Char(c) => c.to_uppercase().to_string(),
            KeyCode::F(n) => format!("F{n}"),
            KeyCode::Enter => "Enter".to_string(),
            KeyCode::Tab => "Tab".to_string(),
            KeyCode::BackTab => "BackTab".to_string(),
            KeyCode::Backspace => "Backspace".to_string(),
            KeyCode::Up => "↑".to_string(),
            KeyCode::Down => "↓".to_string(),
            KeyCode::Left => "←".to_string(),
            KeyCode::Right => "→".to_string(),
            KeyCode::Home => "Home".to_string(),
            KeyCode::End => "End".to_string(),
            KeyCode::PageUp => "PgUp".to_string(),
            KeyCode::PageDown => "PgDn".to_string(),
            KeyCode::Delete => "Del".to_string(),
            KeyCode::Insert => "Ins".to_string(),
            KeyCode::Esc => "Esc".to_string(),
            other => format!("{other:?}"),
        });
        out
    }
}

/// Parse a key string into a [`Binding`].
///
/// Note on Ctrl: terminals encode `Ctrl-<letter>` as a single control byte that
/// crossterm reports as `Char(<lowercase letter>)` + CONTROL — so only
/// `ctrl-a`..`ctrl-z` are reliable. `Ctrl-]` and friends arrive as something else
/// entirely (e.g. `Char('5')`), so they're rejected here rather than silently
/// never matching.
fn parse_binding(spec: &str) -> Result<Binding, String> {
    let lower = spec.trim().to_ascii_lowercase();
    let mut rest = lower.as_str();
    let (mut ctrl, mut alt) = (false, false);
    loop {
        if let Some(r) = rest
            .strip_prefix("ctrl-")
            .or_else(|| rest.strip_prefix("c-"))
        {
            ctrl = true;
            rest = r;
        } else if let Some(r) = rest
            .strip_prefix("alt-")
            .or_else(|| rest.strip_prefix("m-"))
        {
            alt = true;
            rest = r;
        } else {
            break;
        }
    }

    let code = match rest {
        "" => return Err(format!("empty key in '{spec}'")),
        "enter" | "return" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "space" => KeyCode::Char(' '),
        "backspace" | "bs" => KeyCode::Backspace,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "esc" | "escape" => KeyCode::Esc,
        fkey if fkey.starts_with('f') && fkey[1..].parse::<u8>().is_ok() => {
            let n: u8 = fkey[1..]
                .parse()
                .map_err(|_| format!("bad function key '{spec}'"))?;
            if (1..=12).contains(&n) {
                KeyCode::F(n)
            } else {
                return Err(format!("function key out of range: '{spec}' (F1–F12)"));
            }
        }
        other => {
            let mut chars = other.chars();
            let c = chars
                .next()
                .ok_or_else(|| format!("empty key in '{spec}'"))?;
            if chars.next().is_some() {
                return Err(format!("unknown key '{spec}'"));
            }
            if ctrl && !c.is_ascii_alphabetic() {
                return Err(format!(
                    "'{spec}' isn't a reliable Ctrl combo (only ctrl-a..ctrl-z work)"
                ));
            }
            KeyCode::Char(c)
        }
    };
    Ok(Binding { code, ctrl, alt })
}

/// Parse a binding string into its chord sequence: space-separated chords, e.g.
/// `"ctrl-a d"` → a leader (`Ctrl-A`) then `d`. Most bindings are a single chord.
fn parse_keys(spec: &str) -> Result<Vec<Binding>, String> {
    let chords = spec
        .split_whitespace()
        .map(parse_binding)
        .collect::<Result<Vec<_>, _>>()?;
    if chords.is_empty() {
        return Err(format!("empty key in '{spec}'"));
    }
    Ok(chords)
}

/// The active key → action mapping. Single-chord bindings live in `singles`;
/// `detach` may *additionally* be a two-key leader sequence — the only action
/// allowed one, because a reserved-key gesture is only needed while attached
/// (where the agent wants every single key). See the module docs.
#[derive(Debug, Clone)]
pub struct Keymap {
    singles: HashMap<Binding, Action>,
    detach_seqs: Vec<(Binding, Binding)>,
}

impl Default for Keymap {
    fn default() -> Self {
        let mut singles = HashMap::new();
        for (action, _name, keys) in DEFAULTS {
            for key in *keys {
                if let Ok(binding) = parse_binding(key) {
                    singles.insert(binding, *action);
                }
            }
        }
        Keymap {
            singles,
            detach_seqs: Vec::new(),
        }
    }
}

impl Keymap {
    /// The action bound to a single key (dashboard keys + a single-key detach).
    pub fn action_for(&self, key: KeyEvent) -> Option<Action> {
        self.singles.get(&Binding::of(key)).copied()
    }

    /// Whether `key` is the leader (first chord) of a detach sequence.
    pub fn is_detach_leader(&self, key: KeyEvent) -> bool {
        let chord = Binding::of(key);
        self.detach_seqs.iter().any(|(leader, _)| *leader == chord)
    }

    /// Whether pressing `leader` then `key` completes a detach sequence.
    pub fn detach_completes(&self, leader: KeyEvent, key: KeyEvent) -> bool {
        let (l, k) = (Binding::of(leader), Binding::of(key));
        self.detach_seqs.iter().any(|(a, b)| *a == l && *b == k)
    }

    /// Whether two events are the same chord — used to let a doubled leader pass
    /// through to the agent.
    pub fn same_chord(&self, a: KeyEvent, b: KeyEvent) -> bool {
        Binding::of(a) == Binding::of(b)
    }

    /// Joined labels of every binding for `action`, for help and the attach
    /// pane: e.g. `↑ / k`, or `F10 / Ctrl-A D` when detach has a key and a chord.
    pub fn label_for(&self, action: Action) -> String {
        let mut labels: Vec<String> = self
            .singles
            .iter()
            .filter(|(_, a)| **a == action)
            .map(|(b, _)| b.label())
            .collect();
        if action == Action::Detach {
            for (leader, completion) in &self.detach_seqs {
                labels.push(format!("{} {}", leader.label(), completion.label()));
            }
        }
        labels.sort();
        if labels.is_empty() {
            "—".to_string()
        } else {
            labels.join(" / ")
        }
    }

    /// Apply `[keys]` overrides. A named action's bindings are *replaced* by the
    /// given one(s); a value with a space is a leader sequence (detach only).
    /// Returns warnings for unknown actions, bad/reserved keys, misplaced
    /// sequences and conflicts. On any bad entry for an action, that action keeps
    /// its default.
    pub fn apply(&mut self, overrides: &[(String, Vec<String>)]) -> Vec<String> {
        let mut warnings = Vec::new();
        for (name, keys) in overrides {
            let Some(&(action, _, _)) = DEFAULTS.iter().find(|(_, n, _)| n == name) else {
                warnings.push(format!("unknown action '{name}'"));
                continue;
            };

            let mut new_singles = Vec::new();
            let mut new_seqs = Vec::new();
            let mut ok = true;
            for spec in keys {
                match parse_keys(spec) {
                    Ok(chords) if chords.iter().any(|c| c.code == KeyCode::Esc) => {
                        warnings.push(format!("'{name}': esc is reserved"));
                        ok = false;
                    }
                    Ok(chords) if chords.len() == 1 => new_singles.push(chords[0]),
                    Ok(chords) if chords.len() == 2 && action == Action::Detach => {
                        new_seqs.push((chords[0], chords[1]));
                    }
                    Ok(chords) if chords.len() == 2 => {
                        warnings.push(format!(
                            "'{name}': key sequences are only supported for 'detach'"
                        ));
                        ok = false;
                    }
                    Ok(_) => {
                        warnings.push(format!(
                            "'{name}': '{spec}' has too many keys (a key, or a leader + key)"
                        ));
                        ok = false;
                    }
                    Err(e) => {
                        warnings.push(format!("'{name}': {e}"));
                        ok = false;
                    }
                }
            }
            if !ok {
                continue; // keep the default for this action
            }

            // Replace this action's bindings, refusing to steal another action's.
            self.singles.retain(|_, a| *a != action);
            if action == Action::Detach {
                self.detach_seqs.clear();
            }
            for chord in new_singles {
                match self.singles.get(&chord) {
                    Some(other) => warnings.push(format!(
                        "'{name}': {} is already bound to {other:?}; ignored",
                        chord.label()
                    )),
                    None => {
                        self.singles.insert(chord, action);
                    }
                }
            }
            self.detach_seqs.extend(new_seqs);
        }
        warnings
    }
}

/// On-disk shape of `config.toml` (only the `[keys]` table matters here).
#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    keys: HashMap<String, KeySpec>,
}

/// A binding value: one key or several.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum KeySpec {
    One(String),
    Many(Vec<String>),
}

/// Load the keymap: defaults, then `<repo>/.lindep/config.toml`, then
/// `~/.config/lindep/config.toml` (personal wins). Returns the map plus any
/// warnings to surface (bad config never aborts startup — defaults stand in).
pub fn load(repo_root: Option<&Path>) -> (Keymap, Vec<String>) {
    let mut keymap = Keymap::default();
    let mut warnings = Vec::new();

    let mut paths: Vec<PathBuf> = Vec::new();
    if let Some(root) = repo_root {
        paths.push(root.join(".lindep").join("config.toml"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        paths.push(PathBuf::from(home).join(".config/lindep/config.toml"));
    }

    for path in paths {
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                warnings.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        match toml::from_str::<ConfigFile>(&text) {
            Ok(cfg) => {
                let overrides: Vec<(String, Vec<String>)> = cfg
                    .keys
                    .into_iter()
                    .map(|(name, spec)| {
                        let keys = match spec {
                            KeySpec::One(k) => vec![k],
                            KeySpec::Many(ks) => ks,
                        };
                        (name, keys)
                    })
                    .collect();
                for w in keymap.apply(&overrides) {
                    warnings.push(format!("{}: {w}", path.display()));
                }
            }
            Err(e) => warnings.push(format!("{}: invalid TOML: {e}", path.display())),
        }
    }
    (keymap, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn defaults_match_the_historical_bindings() {
        let km = Keymap::default();
        assert_eq!(
            km.action_for(ev(KeyCode::Char('a'), KeyModifiers::NONE)),
            Some(Action::LaunchAgent)
        );
        assert_eq!(
            km.action_for(ev(KeyCode::F(10), KeyModifiers::NONE)),
            Some(Action::Detach)
        );
        // Movement keeps both the arrow and the vi-style letter.
        assert_eq!(
            km.action_for(ev(KeyCode::Down, KeyModifiers::NONE)),
            Some(Action::MoveDown)
        );
        assert_eq!(
            km.action_for(ev(KeyCode::Char('j'), KeyModifiers::NONE)),
            Some(Action::MoveDown)
        );
    }

    #[test]
    fn parses_the_documented_key_syntax() {
        assert_eq!(parse_binding("f8").unwrap().code, KeyCode::F(8));
        assert_eq!(parse_binding("space").unwrap().code, KeyCode::Char(' '));
        assert_eq!(parse_binding("UP").unwrap().code, KeyCode::Up);
        let ctrl_a = parse_binding("ctrl-a").unwrap();
        assert_eq!(ctrl_a.code, KeyCode::Char('a'));
        assert!(ctrl_a.ctrl);
        // The crossterm trap: ctrl-] never arrives as Char(']'), so reject it.
        assert!(parse_binding("ctrl-]").is_err());
        assert!(parse_binding("f13").is_err());
    }

    #[test]
    fn rebinding_detach_replaces_the_default() {
        let mut km = Keymap::default();
        let warnings = km.apply(&[("detach".into(), vec!["f8".into()])]);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            km.action_for(ev(KeyCode::F(8), KeyModifiers::NONE)),
            Some(Action::Detach)
        );
        // The old F10 no longer detaches.
        assert_eq!(km.action_for(ev(KeyCode::F(10), KeyModifiers::NONE)), None);
    }

    #[test]
    fn an_action_can_take_several_keys() {
        let mut km = Keymap::default();
        km.apply(&[("jump-needs-you".into(), vec!["n".into(), "ctrl-g".into()])]);
        assert_eq!(
            km.action_for(ev(KeyCode::Char('n'), KeyModifiers::NONE)),
            Some(Action::JumpNeedsYou)
        );
        assert_eq!(
            km.action_for(ev(KeyCode::Char('g'), KeyModifiers::CONTROL)),
            Some(Action::JumpNeedsYou)
        );
    }

    #[test]
    fn bad_config_warns_and_keeps_the_default() {
        let mut km = Keymap::default();
        let w = km.apply(&[
            ("launch-agent".into(), vec!["nope-key".into()]),
            ("bogus-action".into(), vec!["z".into()]),
            ("detach".into(), vec!["esc".into()]),
        ]);
        assert_eq!(w.len(), 3, "{w:?}");
        // launch-agent kept its default despite the bad key.
        assert_eq!(
            km.action_for(ev(KeyCode::Char('a'), KeyModifiers::NONE)),
            Some(Action::LaunchAgent)
        );
    }

    #[test]
    fn a_conflicting_rebind_is_refused_with_a_warning() {
        let mut km = Keymap::default();
        // Bind stop-agent to 'a', which launch-agent already owns.
        let w = km.apply(&[("stop-agent".into(), vec!["a".into()])]);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("already bound"));
        // 'a' still launches; stop-agent lost its 'x' (replaced) but didn't steal 'a'.
        assert_eq!(
            km.action_for(ev(KeyCode::Char('a'), KeyModifiers::NONE)),
            Some(Action::LaunchAgent)
        );
    }

    #[test]
    fn detach_can_be_a_leader_sequence() {
        let mut km = Keymap::default();
        let w = km.apply(&[("detach".into(), vec!["ctrl-a d".into()])]);
        assert!(w.is_empty(), "{w:?}");

        let ctrl_a = ev(KeyCode::Char('a'), KeyModifiers::CONTROL);
        let d = ev(KeyCode::Char('d'), KeyModifiers::NONE);
        assert!(km.is_detach_leader(ctrl_a));
        assert!(km.detach_completes(ctrl_a, d));
        assert!(!km.detach_completes(ctrl_a, ev(KeyCode::Char('x'), KeyModifiers::NONE)));
        // The old F10 single no longer detaches; the label shows the chord.
        assert_eq!(km.action_for(ev(KeyCode::F(10), KeyModifiers::NONE)), None);
        assert_eq!(km.label_for(Action::Detach), "Ctrl-A D");
    }

    #[test]
    fn detach_can_have_both_a_single_key_and_a_sequence() {
        let mut km = Keymap::default();
        km.apply(&[("detach".into(), vec!["f8".into(), "ctrl-a d".into()])]);
        assert_eq!(
            km.action_for(ev(KeyCode::F(8), KeyModifiers::NONE)),
            Some(Action::Detach)
        );
        assert!(km.detach_completes(
            ev(KeyCode::Char('a'), KeyModifiers::CONTROL),
            ev(KeyCode::Char('d'), KeyModifiers::NONE)
        ));
    }

    #[test]
    fn sequences_are_rejected_for_non_detach_actions() {
        let mut km = Keymap::default();
        let w = km.apply(&[("launch-agent".into(), vec!["g a".into()])]);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("only supported for 'detach'"), "{w:?}");
        // launch-agent kept its default 'a'.
        assert_eq!(
            km.action_for(ev(KeyCode::Char('a'), KeyModifiers::NONE)),
            Some(Action::LaunchAgent)
        );
    }

    #[test]
    fn config_toml_accepts_a_string_or_a_list() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [keys]
            detach = "f8"
            jump-needs-you = ["n", "ctrl-g"]
            "#,
        )
        .unwrap();
        assert!(matches!(cfg.keys.get("detach"), Some(KeySpec::One(s)) if s == "f8"));
        assert!(matches!(cfg.keys.get("jump-needs-you"), Some(KeySpec::Many(v)) if v.len() == 2));
    }

    #[test]
    fn label_for_detach_reflects_a_rebind() {
        let mut km = Keymap::default();
        assert_eq!(km.label_for(Action::Detach), "F10");
        km.apply(&[("detach".into(), vec!["f8".into()])]);
        assert_eq!(km.label_for(Action::Detach), "F8");
    }
}
