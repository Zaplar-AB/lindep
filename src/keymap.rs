//! Remappable key bindings for the cockpit.
//!
//! Cockpit v3 has **one input rule**: the focused window gets your keys, and the
//! **prefix** (`Ctrl-a` by default) is the sole escape to window-manager
//! commands — identical whether an Agent, Deps or the Spine has focus. So there
//! are two binding tables:
//!
//! * **`[keys]`** — *direct* keys, consulted only when a non-Agent window
//!   (Spine / Deps) is focused: movement, search, the roster toggle, re-root,
//!   collapse. An Agent window forwards every direct key to its PTY.
//! * **`[verbs]`** — *prefix* keys, reached as `<prefix> <key>` from any focus:
//!   focus-move, zoom, pin, close, kill, layout, the attach/spawn button, quit,
//!   search, help, roster.
//!
//! ```toml
//! prefix = "ctrl-a"        # the escape chord (pressed twice → literal Ctrl-A)
//!
//! [keys]
//! jump-needs-you = ["n", "ctrl-n"]   # a direct action may take several keys
//!
//! [verbs]
//! kill = "k"               # rebind the prefixed kill verb Ctrl-a k
//! ```
//!
//! Accepted key syntax: `a`, `/`, `?` (single chars); `f1`..`f12`; the named keys
//! `enter` `tab` `backtab` `space` `backspace` `up` `down` `left` `right` `home`
//! `end` `pageup` `pagedown` `delete` `insert`; and `ctrl-`/`alt-` prefixes
//! (`ctrl-<letter>` is the only reliable control combo — see the crossterm note
//! on [`parse_binding`]). `esc` is reserved (a fixed, context-sensitive key).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;

/// A remappable cockpit action. `Esc` is fixed and deliberately absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    // ── Direct keys (Spine / Deps focus) ──────────────────────────────────
    MoveUp,
    MoveDown,
    /// In a Deps window, flip the active tree (upstream ↔ downstream).
    SwitchSide,
    /// Flip the context (active) window between chat and deps. Bare `Tab` when
    /// the Spine or a Deps pane is focused; `Ctrl-a Tab` from inside a chat.
    ContextToggle,
    /// Spine: the attach/spawn button. Deps: re-root onto the selected node.
    Enter,
    /// Deps: collapse / expand the selected subtree. Spine: also the button.
    ToggleCollapse,
    /// Deps: pop the re-root history.
    Back,
    /// Spine: open (or re-root) a per-issue dependency window for the selection.
    OpenDeps,
    /// Spine: open (or focus) the project-wide Fleet overview window.
    OpenFleet,
    /// Spine: jump through issues that sit on a dependency cycle.
    JumpCycle,
    /// Jump to the next agent that needs you.
    JumpNeedsYou,
    /// Flip the Spine between the issue list and the agents roster.
    ToggleRoster,
    /// Spine: cycle the issue filter.
    CycleFilter,
    /// Spine: cycle the issue sort.
    CycleSort,
    /// Fuzzy-find issues (Spine list).
    StartSearch,
    ToggleHelp,
    /// Pop a dismissable overlay summarising the selected issue (details + deps).
    ToggleSummary,
    /// Pop a dismissable overlay listing the selected issue's agent run history —
    /// the session ledger (which `claude` runs ran for it, when, and how each
    /// ended). Mnemonic: agent **t**imeline.
    ToggleLedger,

    // ── Prefix verbs (any focus, behind the prefix) ───────────────────────
    FocusLeft,
    FocusRight,
    /// Jump focus straight home to the Spine in one hop — the dedicated
    /// "back to nav" gesture, so you never step through the deps pane to return.
    FocusNav,
    /// Non-destructive zoom of the focused window to fill the strip.
    ZoomToggle,
    /// Pin / unpin the focused window (persistence).
    PinWindow,
    /// Close = undock the focused window (an agent keeps running).
    CloseWindow,
    /// Kill the focused agent (confirmed) — separate from close.
    KillWindow,
    /// Toggle rail ⇄ mosaic.
    LayoutToggle,
    /// Open (or focus) an agent on the spine selection — the button, reachable
    /// from any window via the prefix.
    AttachOrSpawn,
    Quit,
    /// Latch into command mode: keys act as window verbs *without* re-pressing the
    /// prefix, until Esc or the prefix exits. Keeps the one-shot `prefix key`
    /// rhythm available, while a run of verbs (focus, pin, zoom…) needs no repeats.
    CommandMode,
    /// Open the project switcher overlay — pick another mapped Linear project to
    /// view; the project you leave keeps its agents running in the background.
    SwitchProject,
    /// Open the focused agent's workspace in an external editor (VS Code / Cursor /
    /// …), detached, for review or a real-editor handoff (v1.6, `Ctrl-a e`).
    OpenInEditor,
    /// Open the disk-reclaim prompt: surface unreferenced bare mirrors (no live
    /// reference clone borrows them) so their object DBs can be freed (v1.6,
    /// `Ctrl-a m`). A referenced mirror is never offered — deleting it would corrupt
    /// every referrer (the alternates-fragility guard).
    ReclaimMirrors,
    /// Discard a finished issue's workspace (confirmed): push each repo's branch,
    /// then remove its per-issue worktrees (keeping branches), reclaiming the
    /// checkout disk (v1.6, `Ctrl-a d`). Gated on the agent not being live.
    DiscardWorkspace,
    /// Open the global all-agents screen — every live agent across the whole
    /// workspace as `project · ISSUE · status`, reachable from any graph (v1.6,
    /// `Ctrl-a a`). Enter on a row re-roots onto it (switching projects if needed).
    GlobalView,
    /// Re-open the onboarding wizard for the active project to edit its repo binding /
    /// scratch datastores (v1.6, `Ctrl-a o`). Writes `~/.lindep/registry.toml`; the
    /// change applies on the next launch (the running workspace keeps its binding).
    ConfigureProject,
}

/// `(action, config name, default keys)` for the **direct** keys consulted when
/// the Spine or a Deps window is focused.
const DIRECT_DEFAULTS: &[(Action, &str, &[&str])] = &[
    (Action::MoveUp, "move-up", &["up", "k"]),
    (Action::MoveDown, "move-down", &["down", "j"]),
    (
        Action::SwitchSide,
        "switch-side",
        &["left", "h", "right", "l"],
    ),
    // Bare `Tab` flips the active window chat⇄deps when the Spine or a Deps pane
    // is focused (an Agent pane forwards Tab to its PTY — reach it via Ctrl-a Tab,
    // the verb below).
    (Action::ContextToggle, "context", &["tab"]),
    (Action::Enter, "enter", &["enter"]),
    (Action::ToggleCollapse, "toggle-collapse", &["space"]),
    (Action::Back, "back", &["backspace", "b"]),
    (Action::OpenDeps, "deps", &["d"]),
    (Action::OpenFleet, "fleet", &["g"]),
    (Action::JumpCycle, "jump-cycle", &["c"]),
    (Action::JumpNeedsYou, "jump-needs-you", &["n"]),
    (Action::ToggleRoster, "agents", &["r"]),
    (Action::CycleFilter, "filter", &["f"]),
    (Action::CycleSort, "sort", &["s"]),
    (Action::StartSearch, "search", &["/"]),
    (Action::ToggleHelp, "help", &["?"]),
    (Action::ToggleSummary, "summary", &["i"]),
    (Action::ToggleLedger, "ledger", &["t"]),
];

/// `(action, config name, default keys)` for the **prefix** verbs, reached as
/// `<prefix> <key>` from any focus.
const VERB_DEFAULTS: &[(Action, &str, &[&str])] = &[
    (Action::FocusLeft, "focus-left", &["left", "h"]),
    (Action::FocusRight, "focus-right", &["right", "l"]),
    // `Ctrl-a g` / `Ctrl-a 0` (tmux's "window 0") jump straight home to the nav.
    (Action::FocusNav, "focus-nav", &["g", "0"]),
    (Action::ZoomToggle, "zoom", &["z"]),
    (Action::PinWindow, "pin", &["p"]),
    (Action::CloseWindow, "close", &["w"]),
    (Action::KillWindow, "kill", &["x"]),
    (Action::LayoutToggle, "layout", &["|"]),
    (Action::AttachOrSpawn, "open", &["enter", "space"]),
    (Action::Quit, "quit", &["q"]),
    // `Ctrl-a .` latches command mode (keys = verbs until Esc / the prefix). The
    // one-shot `Ctrl-a key` rhythm is untouched; this just removes the repeats.
    (Action::CommandMode, "command-mode", &["."]),
    (Action::StartSearch, "search", &["/"]),
    (Action::ToggleHelp, "help", &["?"]),
    (Action::ToggleSummary, "summary", &["i"]),
    (Action::ToggleLedger, "ledger", &["t"]),
    (Action::ToggleRoster, "roster", &["r"]),
    (Action::JumpNeedsYou, "jump-needs-you", &["n"]),
    // `Ctrl-a s` opens the project switcher (`p` is taken by `pin`).
    (Action::SwitchProject, "switch-project", &["s"]),
    // `Ctrl-a e` opens the focused agent's workspace in an external editor.
    (Action::OpenInEditor, "open-in-editor", &["e"]),
    // `Ctrl-a m` opens the disk-reclaim prompt (unreferenced mirrors).
    (Action::ReclaimMirrors, "reclaim-mirrors", &["m"]),
    // `Ctrl-a d` discards a finished issue's workspace (push + remove worktrees).
    (Action::DiscardWorkspace, "discard-workspace", &["d"]),
    // `Ctrl-a a` opens the global all-agents screen (every project's live agents).
    (Action::GlobalView, "global-view", &["a"]),
    // `Ctrl-a o` re-opens the onboarding wizard to (re)configure the active project.
    (Action::ConfigureProject, "configure-project", &["o"]),
    // `Ctrl-a Tab` flips the active window chat⇄deps from any focus — notably
    // from inside a chat, where a bare Tab would go to the agent's PTY.
    (Action::ContextToggle, "context", &["tab"]),
];

/// The default prefix chord — tmux's `Ctrl-A`, which never collides with
/// claude's line editing and works on every keyboard/terminal.
const DEFAULT_PREFIX: &str = "ctrl-a";

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

    /// Reconstruct a [`KeyEvent`] from this chord — used to forward the literal
    /// prefix to an agent's PTY on a double-tap (a `bool` alone loses the chord).
    fn to_key_event(self) -> KeyEvent {
        let mut mods = KeyModifiers::NONE;
        if self.ctrl {
            mods |= KeyModifiers::CONTROL;
        }
        if self.alt {
            mods |= KeyModifiers::ALT;
        }
        KeyEvent::new(self.code, mods)
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

/// The active key bindings: the prefix chord, the direct keys (Spine/Deps), and
/// the prefix verbs.
#[derive(Debug, Clone)]
pub struct Keymap {
    prefix: Binding,
    singles: HashMap<Binding, Action>,
    verbs: HashMap<Binding, Action>,
}

impl Default for Keymap {
    fn default() -> Self {
        let load = |table: &[(Action, &str, &[&str])]| {
            let mut map = HashMap::new();
            for (action, _name, keys) in table {
                for key in *keys {
                    if let Ok(binding) = parse_binding(key) {
                        map.insert(binding, *action);
                    }
                }
            }
            map
        };
        Keymap {
            prefix: parse_binding(DEFAULT_PREFIX).expect("the default prefix parses"),
            singles: load(DIRECT_DEFAULTS),
            verbs: load(VERB_DEFAULTS),
        }
    }
}

impl Keymap {
    /// Whether `key` is the prefix chord (the escape to window-manager verbs).
    pub fn is_prefix(&self, key: KeyEvent) -> bool {
        Binding::of(key) == self.prefix
    }

    /// The prefix chord as a [`KeyEvent`], for forwarding the literal chord to an
    /// agent's PTY on a double-tap.
    pub fn prefix_event(&self) -> KeyEvent {
        self.prefix.to_key_event()
    }

    /// Human label for the prefix, e.g. `Ctrl-A`.
    pub fn prefix_label(&self) -> String {
        self.prefix.label()
    }

    /// The direct action bound to a single key (Spine / Deps focus).
    pub fn action_for(&self, key: KeyEvent) -> Option<Action> {
        self.singles.get(&Binding::of(key)).copied()
    }

    /// The verb bound to `key` when reached behind the prefix.
    pub fn verb_for(&self, key: KeyEvent) -> Option<Action> {
        self.verbs.get(&Binding::of(key)).copied()
    }

    /// Joined labels of the direct keys bound to `action` (for help/hints):
    /// e.g. `↑ / k`. `—` when unbound.
    pub fn label_for(&self, action: Action) -> String {
        join_labels(&self.singles, action)
    }

    /// Joined *bare* key labels of the verb keys bound to `action` — no prefix
    /// shown, e.g. `W` — for the compact armed-hint footer (where the prefix is
    /// already armed). `—` when unbound.
    pub fn verb_key_label(&self, action: Action) -> String {
        join_labels(&self.verbs, action)
    }

    /// Joined labels of the verb keys bound to `action`, each shown with the
    /// prefix — e.g. `Ctrl-A W`. `—` when unbound.
    pub fn verb_label(&self, action: Action) -> String {
        let prefix = self.prefix.label();
        let mut labels: Vec<String> = self
            .verbs
            .iter()
            .filter(|(_, a)| **a == action)
            .map(|(b, _)| format!("{prefix} {}", b.label()))
            .collect();
        labels.sort();
        if labels.is_empty() {
            "—".to_string()
        } else {
            labels.join(" / ")
        }
    }

    /// Apply config overrides. Each named action's bindings in the chosen table
    /// (`direct` = `[keys]`, else `[verbs]`) are *replaced* by the given keys.
    /// Returns warnings for unknown actions, bad/reserved keys and conflicts. A
    /// bad entry — an unknown action, an unparseable or reserved key, or a rebind
    /// whose every requested key is already taken — leaves that action at its
    /// default rather than unbinding it.
    fn apply_table(
        map: &mut HashMap<Binding, Action>,
        defaults: &[(Action, &str, &[&str])],
        overrides: &[(String, Vec<String>)],
        table_label: &str,
    ) -> Vec<String> {
        let mut warnings = Vec::new();
        for (name, keys) in overrides {
            let Some(&(action, _, _)) = defaults.iter().find(|(_, n, _)| n == name) else {
                warnings.push(format!("unknown {table_label} action '{name}'"));
                continue;
            };
            let mut parsed = Vec::new();
            let mut ok = true;
            for spec in keys {
                match parse_binding(spec) {
                    Ok(b) if b.code == KeyCode::Esc => {
                        warnings.push(format!("'{name}': esc is reserved"));
                        ok = false;
                    }
                    Ok(b) => parsed.push(b),
                    Err(e) => {
                        warnings.push(format!("'{name}': {e}"));
                        ok = false;
                    }
                }
            }
            if !ok {
                continue; // keep the default for this action
            }
            // Resolve conflicts BEFORE touching `map`, so an all-conflicting rebind
            // leaves the action at its default (per this method's contract) instead
            // of unbinding it. A chord already owned by THIS action isn't a conflict
            // — we're about to replace its bindings anyway.
            let mut free = Vec::new();
            for chord in parsed {
                match map.get(&chord) {
                    Some(other) if *other != action => warnings.push(format!(
                        "'{name}': {} is already bound to {other:?}; ignored",
                        chord.label()
                    )),
                    _ => free.push(chord),
                }
            }
            if free.is_empty() {
                continue; // every requested key was taken — keep the defaults
            }
            // At least one new chord survives: now replace this action's bindings.
            map.retain(|_, a| *a != action);
            for chord in free {
                map.insert(chord, action);
            }
        }
        warnings
    }

    /// Apply `[keys]` (direct) overrides.
    pub fn apply(&mut self, overrides: &[(String, Vec<String>)]) -> Vec<String> {
        Self::apply_table(&mut self.singles, DIRECT_DEFAULTS, overrides, "key")
    }

    /// Apply `[verbs]` (prefix) overrides.
    pub fn apply_verbs(&mut self, overrides: &[(String, Vec<String>)]) -> Vec<String> {
        Self::apply_table(&mut self.verbs, VERB_DEFAULTS, overrides, "verb")
    }

    /// Set the prefix chord from a config string, warning (and keeping the
    /// default) on a bad or reserved value.
    ///
    /// Shadow detection is **not** done here — it's deferred to
    /// [`Keymap::warn_prefix_shadows`], run after every override is applied. A
    /// `[keys]`/`[verbs]` override can bind a chord *onto* the prefix (or move
    /// one *off* it), and `load` sets the prefix before applying overrides, so
    /// only the final map can tell what the prefix actually shadows. Detecting
    /// here (against the pre-override defaults) both missed a rebind-onto-prefix
    /// and false-warned on a default the user then moved away.
    pub fn set_prefix(&mut self, spec: &str) -> Vec<String> {
        match parse_binding(spec) {
            Ok(b) if b.code == KeyCode::Esc => {
                vec!["'prefix': esc is reserved".to_string()]
            }
            Ok(b) => {
                self.prefix = b;
                Vec::new()
            }
            Err(e) => vec![format!("'prefix': {e}")],
        }
    }

    /// Warn about any direct key or verb whose chord equals the prefix. `on_key`
    /// consults `is_prefix` before `action_for`/`verb_for`, so such a binding can
    /// never fire — it's silently dead without this. Run last in [`load`], over
    /// the **final** merged map (after the prefix and every override are applied),
    /// since an override can bind a chord onto the prefix or move one off it. The
    /// default `Ctrl-A` prefix collides with nothing, so this is silent unless a
    /// custom prefix (or a rebind onto it) actually shadows a binding.
    pub fn warn_prefix_shadows(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if let Some(a) = self.singles.get(&self.prefix) {
            warnings.push(format!(
                "'prefix': {} is also the direct key for {a:?}; that binding is now shadowed",
                self.prefix.label()
            ));
        }
        if let Some(a) = self.verbs.get(&self.prefix) {
            warnings.push(format!(
                "'prefix': {} is also a verb ({a:?}); that binding is now shadowed",
                self.prefix.label()
            ));
        }
        warnings
    }
}

/// Joined, sorted labels of every chord in `map` bound to `action`.
fn join_labels(map: &HashMap<Binding, Action>, action: Action) -> String {
    let mut labels: Vec<String> = map
        .iter()
        .filter(|(_, a)| **a == action)
        .map(|(b, _)| b.label())
        .collect();
    labels.sort();
    if labels.is_empty() {
        "—".to_string()
    } else {
        labels.join(" / ")
    }
}

/// On-disk shape of `config.toml`.
#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    /// The prefix chord (`Ctrl-a` default).
    prefix: Option<String>,
    /// Direct keys (Spine / Deps focus).
    #[serde(default)]
    keys: HashMap<String, KeySpec>,
    /// Prefix verbs (any focus).
    #[serde(default)]
    verbs: HashMap<String, KeySpec>,
    /// The `[agents]` table — non-keybinding settings.
    #[serde(default)]
    agents: AgentsConfig,
}

/// The `[agents]` table of `config.toml`.
#[derive(Debug, Default, Deserialize)]
struct AgentsConfig {
    /// Override the live-backend ceiling. Docking is uncapped regardless; this
    /// bounds how many agents run at once (Cockpit v3 default is 12).
    max_concurrent: Option<usize>,
}

/// Non-keymap settings parsed from the same `config.toml` as the keymap, so the
/// single load reads the whole file once. Absent fields stay `None` and the
/// caller substitutes its compiled-in defaults.
#[derive(Debug, Default, Clone, Copy)]
pub struct Settings {
    /// `[agents] max_concurrent` override, if the user set one.
    pub max_concurrent: Option<usize>,
}

/// A binding value: one key or several.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum KeySpec {
    One(String),
    Many(Vec<String>),
}

impl KeySpec {
    fn into_vec(self) -> Vec<String> {
        match self {
            KeySpec::One(k) => vec![k],
            KeySpec::Many(ks) => ks,
        }
    }
}

/// Load the keymap: defaults, then `<local_root>/.lindep/config.toml`, then
/// `~/.config/lindep/config.toml` (personal wins). `local_root` is the directory
/// lindep was launched from (v1.6 runs from anywhere, so this is the cwd, not a
/// git repo root); the personal file is the durable one. Returns the map plus any
/// warnings to surface (bad config never aborts startup — defaults stand in).
pub fn load(local_root: Option<&Path>) -> (Keymap, Settings, Vec<String>) {
    let mut keymap = Keymap::default();
    let mut settings = Settings::default();
    let mut warnings = Vec::new();

    let mut paths: Vec<PathBuf> = Vec::new();
    if let Some(root) = local_root {
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
                // Apply overrides in a deterministic (action-name) order.
                // `cfg.keys`/`cfg.verbs` are HashMaps, whose iteration order
                // varies per process (random SipHash seed). `apply_table`
                // resolves conflicts against the live map in order and only frees
                // a chord when it reaches the action that owns it, so two
                // interacting rebinds (one taking another's default chord) would
                // otherwise resolve differently — and emit different warnings —
                // across launches. Sorting pins the outcome for a given config.
                let collect = |table: HashMap<String, KeySpec>| -> Vec<(String, Vec<String>)> {
                    let mut out: Vec<(String, Vec<String>)> = table
                        .into_iter()
                        .map(|(name, spec)| (name, spec.into_vec()))
                        .collect();
                    out.sort_by(|a, b| a.0.cmp(&b.0));
                    out
                };
                if let Some(prefix) = &cfg.prefix {
                    for w in keymap.set_prefix(prefix) {
                        warnings.push(format!("{}: {w}", path.display()));
                    }
                }
                for w in keymap.apply(&collect(cfg.keys)) {
                    warnings.push(format!("{}: {w}", path.display()));
                }
                for w in keymap.apply_verbs(&collect(cfg.verbs)) {
                    warnings.push(format!("{}: {w}", path.display()));
                }
                // Personal config (read last) wins over the repo's.
                if let Some(mc) = cfg.agents.max_concurrent {
                    settings.max_concurrent = Some(mc);
                }
            }
            Err(e) => warnings.push(format!("{}: invalid TOML: {e}", path.display())),
        }
    }
    // Scan for prefix shadows once, over the final merged map — after every
    // file's prefix and overrides are applied — since a later override can bind a
    // chord onto the prefix an earlier file set. (Silent under the default
    // Ctrl-A prefix, which collides with nothing.)
    warnings.extend(keymap.warn_prefix_shadows());
    (keymap, settings, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn direct_movement_keeps_arrows_and_vi_letters() {
        let km = Keymap::default();
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
    fn tab_toggles_the_context_window_not_the_tree_side() {
        let km = Keymap::default();
        // Bare Tab (Spine / Deps focus) flips the active window chat⇄deps…
        assert_eq!(
            km.action_for(ev(KeyCode::Tab, KeyModifiers::NONE)),
            Some(Action::ContextToggle)
        );
        // …and Ctrl-a Tab does the same from any focus (notably inside a chat).
        assert_eq!(
            km.verb_for(ev(KeyCode::Tab, KeyModifiers::NONE)),
            Some(Action::ContextToggle)
        );
        // SwitchSide (up↔down tree) is still reachable, just not via Tab.
        assert_eq!(
            km.action_for(ev(KeyCode::Char('h'), KeyModifiers::NONE)),
            Some(Action::SwitchSide)
        );
        assert_eq!(
            km.action_for(ev(KeyCode::Char('l'), KeyModifiers::NONE)),
            Some(Action::SwitchSide)
        );
    }

    #[test]
    fn the_prefix_is_ctrl_a_by_default() {
        let km = Keymap::default();
        assert!(km.is_prefix(ev(KeyCode::Char('a'), KeyModifiers::CONTROL)));
        assert!(!km.is_prefix(ev(KeyCode::Char('a'), KeyModifiers::NONE)));
        assert_eq!(km.prefix_label(), "Ctrl-A");
    }

    #[test]
    fn the_prefix_event_round_trips_to_its_chord() {
        let km = Keymap::default();
        let ev = km.prefix_event();
        assert!(km.is_prefix(ev), "the reconstructed event is the prefix");
        assert_eq!(ev.code, KeyCode::Char('a'));
        assert!(ev.modifiers.contains(KeyModifiers::CONTROL));
    }

    #[test]
    fn the_window_verbs_bind_behind_the_prefix() {
        let km = Keymap::default();
        assert_eq!(
            km.verb_for(ev(KeyCode::Char('w'), KeyModifiers::NONE)),
            Some(Action::CloseWindow)
        );
        assert_eq!(
            km.verb_for(ev(KeyCode::Char('x'), KeyModifiers::NONE)),
            Some(Action::KillWindow)
        );
        assert_eq!(
            km.verb_for(ev(KeyCode::Char('q'), KeyModifiers::NONE)),
            Some(Action::Quit)
        );
        assert_eq!(
            km.verb_for(ev(KeyCode::Char('z'), KeyModifiers::NONE)),
            Some(Action::ZoomToggle)
        );
    }

    #[test]
    fn quit_is_a_prefix_verb_not_a_direct_key() {
        // q alone no longer quits — it must go through the prefix (so a stray q
        // on the spine, or to an agent, never tears the cockpit down).
        let km = Keymap::default();
        assert_eq!(
            km.action_for(ev(KeyCode::Char('q'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            km.verb_for(ev(KeyCode::Char('q'), KeyModifiers::NONE)),
            Some(Action::Quit)
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
    fn rebinding_a_direct_key_replaces_the_default() {
        let mut km = Keymap::default();
        let warnings = km.apply(&[("search".into(), vec!["ctrl-f".into()])]);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            km.action_for(ev(KeyCode::Char('f'), KeyModifiers::CONTROL)),
            Some(Action::StartSearch)
        );
        // The old `/` no longer starts a direct search.
        assert_eq!(
            km.action_for(ev(KeyCode::Char('/'), KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn rebinding_a_verb_replaces_the_default() {
        let mut km = Keymap::default();
        let warnings = km.apply_verbs(&[("kill".into(), vec!["k".into()])]);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            km.verb_for(ev(KeyCode::Char('k'), KeyModifiers::NONE)),
            Some(Action::KillWindow)
        );
        assert_eq!(
            km.verb_for(ev(KeyCode::Char('x'), KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn rebinding_the_prefix_takes_effect() {
        let mut km = Keymap::default();
        let warnings = km.set_prefix("ctrl-b");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(km.is_prefix(ev(KeyCode::Char('b'), KeyModifiers::CONTROL)));
        assert!(!km.is_prefix(ev(KeyCode::Char('a'), KeyModifiers::CONTROL)));
        assert_eq!(km.prefix_label(), "Ctrl-B");
    }

    #[test]
    fn a_prefix_that_shadows_a_binding_warns() {
        let mut km = Keymap::default();
        // 'd' is the direct key for OpenDeps; choosing it as the prefix swallows
        // that chord. Setting the prefix is itself clean; the shadow surfaces in
        // the post-override scan, which runs over the final map.
        assert!(
            km.set_prefix("d").is_empty(),
            "setting a valid prefix is clean"
        );
        let warnings = km.warn_prefix_shadows();
        assert!(
            warnings.iter().any(|w| w.contains("shadowed")),
            "a prefix colliding with an existing binding warns: {warnings:?}"
        );
        assert!(km.is_prefix(ev(KeyCode::Char('d'), KeyModifiers::NONE)));
    }

    #[test]
    fn a_rebind_onto_a_custom_prefix_is_detected_as_shadowed() {
        // The ordering bug: set a custom prefix, THEN bind a direct key onto that
        // same chord. Ctrl-B is unbound in the defaults, so the rebind is accepted
        // with no conflict — but on_key consumes the prefix first, so MoveUp on
        // Ctrl-B can never fire. set_prefix ran before the override and so could
        // not see it; warn_prefix_shadows, run after, must catch it.
        let mut km = Keymap::default();
        assert!(km.set_prefix("ctrl-b").is_empty());
        assert!(
            km.apply(&[("move-up".into(), vec!["ctrl-b".into()])])
                .is_empty(),
            "rebinding onto a free chord is accepted silently"
        );
        let warnings = km.warn_prefix_shadows();
        assert!(
            warnings.iter().any(|w| w.contains("shadowed")),
            "a binding rebound onto the prefix is reported as shadowed: {warnings:?}"
        );
    }

    #[test]
    fn esc_is_reserved_everywhere() {
        let mut km = Keymap::default();
        assert!(!km.set_prefix("esc").is_empty());
        assert_eq!(km.apply(&[("search".into(), vec!["esc".into()])]).len(), 1);
        assert_eq!(
            km.apply_verbs(&[("kill".into(), vec!["esc".into()])]).len(),
            1
        );
    }

    #[test]
    fn a_conflicting_rebind_is_refused_with_a_warning() {
        let mut km = Keymap::default();
        // Bind filter to 'r', which the roster toggle already owns.
        let w = km.apply(&[("filter".into(), vec!["r".into()])]);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("already bound"));
        // 'r' still toggles the roster (the rebind was refused, not stolen)…
        assert_eq!(
            km.action_for(ev(KeyCode::Char('r'), KeyModifiers::NONE)),
            Some(Action::ToggleRoster)
        );
        // …and because every requested key conflicted, filter KEEPS its default
        // 'f' rather than being left unbound.
        assert_eq!(
            km.action_for(ev(KeyCode::Char('f'), KeyModifiers::NONE)),
            Some(Action::CycleFilter)
        );
    }

    #[test]
    fn bad_config_warns_and_keeps_the_default() {
        let mut km = Keymap::default();
        let w = km.apply(&[
            ("search".into(), vec!["nope-key".into()]),
            ("bogus-action".into(), vec!["z".into()]),
        ]);
        assert_eq!(w.len(), 2, "{w:?}");
        // search kept its default despite the bad key.
        assert_eq!(
            km.action_for(ev(KeyCode::Char('/'), KeyModifiers::NONE)),
            Some(Action::StartSearch)
        );
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
    fn verb_label_shows_the_prefix() {
        let km = Keymap::default();
        assert_eq!(km.verb_label(Action::CloseWindow), "Ctrl-A W");
        assert_eq!(km.verb_label(Action::Quit), "Ctrl-A Q");
    }

    #[test]
    fn config_toml_accepts_prefix_keys_and_verbs_tables() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            prefix = "ctrl-b"
            [keys]
            jump-needs-you = ["n", "ctrl-g"]
            [verbs]
            kill = "k"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.prefix.as_deref(), Some("ctrl-b"));
        assert!(matches!(cfg.keys.get("jump-needs-you"), Some(KeySpec::Many(v)) if v.len() == 2));
        assert!(matches!(cfg.verbs.get("kill"), Some(KeySpec::One(s)) if s == "k"));
    }
}
