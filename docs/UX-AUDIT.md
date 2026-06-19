# lindep — UX / UI audit

> **Status:** diagnosed 2026-06-18; **implemented + reviewed 2026-06-19.** Root
> causes and fixes below were confirmed against the code at `main` (commit `3ab03ef`)
> and are now applied in this branch. This doc is the working punch-list — Part A is
> the review pass we walked through together, Part B is the broader machine-assisted
> sweep, and **Part C records the post-implementation review pass** (a Rust + UI/UX
> re-audit of the fixes themselves, with the follow-up corrections it surfaced).

## How to read this

Each item has **Symptom** (what the user sees), **Root cause** (the exact code,
`file:line`), and **Fix** (the planned change). Severity is the *flow* impact, not
the code size:

- **[HIGH]** — actively misleads, loses work/state, or blocks a common path.
- **[MED]** — confusing or inconsistent; a workaround exists.
- **[LOW]** — polish; a papercut.

The cockpit's vocabulary (so the rest reads cleanly): the **Spine** is window 0,
the readiness-banded issue list. A **Coin** is one issue with two faces (Chat =
live `claude` PTY, Deps = dependency tree). The unpinned **preview** coin follows
the Spine selection; **pinning** graduates it to a docked coin. `Ctrl-a` is the
window-verb prefix; direct keys drive the Spine/Deps, an Agent pane forwards every
key to its PTY.

---

## Part A — items raised in review

### A1. The top band header scrolls off-screen · [HIGH]

**Symptom.** When the issue list is scrolled, the topmost band header (e.g.
`NEEDS YOU`) disappears — you lose the label for the band the cursor is in.

**Root cause.** `render_banded_spine` splices the band dividers in as *ordinary,
non-selectable list rows* and renders the whole thing through the scrolling
`banded_list_state` (`src/ui.rs:495-519`). The dividers are part of the
scrollable content, so the header above the viewport scrolls away like any row:

```rust
// src/ui.rs:498-503
for key in &app.order {
    let band = app.readiness(key);
    if prev_band != Some(band) {
        items.push(band_divider(band, rule_width));   // <- a scrollable row
        prev_band = Some(band);
    }
    ...
}
```

**Fix.** Make the *current* band header sticky: render the band of the topmost
visible row as a fixed first line of the inner area, then render the list below
it. (Minimum viable: pin only the selected row's band header.)

---

### A2. `pin` from the Spine does nothing · [HIGH]

**Symptom.** You're browsing the Spine, looking at an issue, press the pin verb —
nothing useful happens. You expected it to pin that issue's view, the way it does
when you're standing in the issue's own window.

**Root cause.** `pin_window` keys off *focus*. Focus 0 is the Spine, so it bails
with a message and never touches the previewed issue (`src/app.rs:1706-1709`):

```rust
fn pin_window(&mut self) {
    if self.windows.focus == 0 {
        self.status_msg = Some("the spine is always pinned".into());
        return;
    }
    ...
}
```

To actually pin, you must first Tab into the preview coin, *then* pin — an extra
step that isn't discoverable.

**Fix.** When the Spine is focused, pin should graduate the **previewed coin** for
the current selection (creating it if needed) — i.e. pin *that issue's view*,
identical to pinning from inside the issue's window.

---

### A3. Agent chat input box ends up below the visible pane · [MED]

**Symptom.** Occasionally the text box you type into for an agent is pushed below
the window and you can't see it.

**Root cause.** The PTY screen is painted with `PseudoTerminal::new(parser.screen())`
(`src/ui.rs:649`), which paints the **top-left** sub-grid with no vertical offset
(the tui-term 0.3.4 limit noted at `src/layout.rs:18`). The pane *is* resized to
match (`src/ui.rs:644`, `backend.resize`), but the resize is guarded and only
recorded when it succeeds (`src/ui.rs:638-646`); during a layout transition (or a
resize that returned `Err`) the vt100 grid is briefly taller than the pane, so the
bottom rows — where `claude` draws its input box — are clipped off the bottom.

**Fix.** When `screen.rows > pane.height`, bottom-align the paint so the input row
(always last) is the one that survives; and force a resync of the PTY size on
focus / layout change. (This one is the least-pinned-down; a live repro would
confirm the exact trigger.)

---

### A4. Agent stays "working" instead of going idle · [HIGH]

**Symptom.** An agent finishes its turn and is actually resting, but the cockpit
keeps showing it as WORKING (orange spinner). Sometimes it never settles to IDLE.

**Root cause.** Two compounding issues:

1. `Idle` is set **only** by `claude`'s `Stop` hook (`src/notify.rs:328,446`).
   There is no inactivity fallback.
2. Hooks are each handled in their own task (`tokio::spawn` per connection,
   `src/notify.rs:149`), so event ordering isn't guaranteed. A non-working idle
   nudge or a late `PostToolUse` can re-promote an already-Idle agent to Running.
   In `apply_event`, any non-`NeedsYou` action flips the agent back to Running:

```rust
// src/app.rs:2559-2566
let was_needs_you = self.fleet.get(&issue) == Some(&AgentStatus::NeedsYou);
if working || !was_needs_you {                 // <- Idle + idle-nudge => Running
    self.fleet.insert(issue.clone(), AgentStatus::Running);
}
```

So the ~60 s `idle_prompt` notification (which arrives `working:false`,
`src/notify.rs:439-444`) flips an Idle agent to WORKING; and a `PostToolUse` that
lands after `Stop` does the same.

**Fix.** Only a genuine *working* signal should promote, and a **new turn**
(`UserPromptSubmit` / elicitation-complete) — not a mid-turn `PostToolUse` — should
revive an Idle agent. Concretely: route `UserPromptSubmit` as a real status change
(revives Idle = new turn), make a `working` action promote only non-Idle live
states, and mirror the same rule in `notify.rs`'s `implied` so the durable store
agrees. (See the "RESOLVED in review" note — word is **WORKING**, and IDLE is now
its own band, which makes a stuck-WORKING agent much more visible.)

---

### A5. Issue summary header doesn't wrap (and the modal is a dead-end) · [MED]

**Symptom.** A long issue title in the `i` summary is cut off instead of wrapping;
a long dependency list runs off the bottom with no way to scroll; and any keypress
closes the panel.

**Root cause.** The title is one `Line` (`src/ui.rs:1216-1219`) and the panel is
`Paragraph::new(lines).block(block)` with **no `.wrap(...)`** (`src/ui.rs:1294`),
so it truncates. The box height is computed from the line count
(`centered_rect(78, lines.len() as u16 + 2, …)`, `src/ui.rs:1285`), so there's no
scroll, and the title says "any key to close" (`src/ui.rs:1291`).

**Fix.** Add `Wrap { trim: false }`, grow the height estimate to account for a
wrapped title, make the modal scrollable, and dismiss on `Esc`/`i` only (so a
stray key doesn't lose your place).

---

### A6 + A7. "RUNNING / WORKING / Running" collision and `▶`/`▸` glyph collision · [MED]

**Symptom.** Three words for one idea — the **RUNNING** band, the **WORKING**
window title, and `AgentStatus::Running`. And the RUNNING band header `▶` looks
almost identical to the READY band header `▸`, separated only by colour.

**Root cause.**
- Band label `RUNNING` (`src/ui.rs:528`), window title `WORKING` (`src/theme.rs:76`),
  enum `AgentStatus::Running` (`src/session.rs:44`).
- `▸` is overloaded **four** ways: the selection cursor
  (`highlight_symbol("▸ ")`, `src/ui.rs:141,475,774`), the READY band header
  (`src/ui.rs:529`), the per-row dispatch rail (`src/ui.rs:1468`), and the
  "+N hidden" marker (`src/ui.rs:1594`) — and the RUNNING header `▶`
  (`src/ui.rs:528`) is a near-identical triangle.

**Fix (RESOLVED in review).** Split the RUNNING band into **WORKING** (spawning +
actively churning) and **IDLE** (alive, resting); unify the active word to
**WORKING** everywhere (band header + window title); drop `▶`; give each band
header a distinct, non-triangle glyph and reserve `▸` for the READY rail + cursor.
Target headers: `⚑ NEEDS YOU` · `◉ WORKING` · `◦ IDLE` · `▸ READY` · `⊘ BLOCKED` ·
`✓ DONE`.

---

### A8. `r` silently degrades the Spine to a flat list · [MED]

**Symptom.** Pressing `r` flips the Spine from the readiness bands to a plain,
band-less, id-sorted list — with no signpost that anything changed or how to get
back. It reads as a downgrade.

**Root cause.** `r` is now `CycleSort` (`src/keymap.rs:146`), toggling
`Sort::Readiness → Sort::Key` (`src/app.rs:121-124`), and the handler
(`src/app.rs:1197-1200`) sets no footer. `Sort::Key` has no bands. The key used to
flip the old agents/issues roster, which was folded away in v1.7 — so the binding
survived its purpose.

**Fix (RESOLVED in review).** Remove the flat id-sort and the `r`/CycleSort
binding entirely; the readiness schedule is the single Spine view.

---

## Resolved design decisions (from the review)

These were ambiguous calls; settled with the user on 2026-06-18:

1. **Active band split** — `RUNNING` → **`WORKING`** + **`IDLE`** as two bands.
2. **One word for the active state** — **WORKING** (band header *and* window
   title); `RUNNING` disappears from the UI.
3. **Glyph cleanup** — distinct glyph per band header; `▶` removed; `▸` reserved
   for the READY rail + cursor.
4. **Flat sort removed** — delete `Sort::Key` + the `r`/`CycleSort` binding.

New band order (the `Readiness` `Ord`): `NEEDS-YOU < WORKING < IDLE < READY <
BLOCKED < DONE`.

---

## Additional issues found while diagnosing

These came up while tracing the eight above; not yet decided.

- **B0a. Empty bands vanish with no "caught up" affordance · [LOW]** — the band
  loop only emits a header when it first sees an issue in that band
  (`src/ui.rs:500`), so clearing the NEEDS-YOU band makes the section silently
  disappear. Reads as "did it lose my agent?" rather than "you're clear." A
  one-line placeholder for the just-emptied attention bands would reassure.

- **B0b. `pin` on an already-pinned coin closes it · [MED]** — the same verb that
  pins also *undocks* (`close_window`) a pinned coin (`src/app.rs:1734`).
  "Unpin = close the window" is a surprising overload of one key; easy to lose a
  window you meant to keep. Reconsider once A2 lands.

- **B0c. The preview coin's impermanence is invisible · [MED]** — a pinned coin
  shows `⊙ pin` in its title (`src/ui.rs:594`), but the *unpinned* preview has no
  marker telling you it's transient and will move when you change selection. Its
  impermanence only becomes apparent when it vanishes.

- **B0d. Inconsistent footer feedback · [LOW]** — some actions set a footer line,
  others (like `CycleSort`) are silent, so the user can't rely on the footer to
  confirm "that did something." Worth a consistent rule: every state-changing
  direct action leaves a one-line trace.

---

## Part B — Broader audit

> Generated by the `lindep-ux-audit` multi-agent sweep: 11 UX-dimension finders →
> adversarial verification of every candidate against the code *and* its
> doc-comment rationale → synthesis. **51 candidate findings survived
> verification**; after merging cross-dimension duplicates, **48 distinct issues**
> are listed below (8 high · 16 medium · 24 low). Each was checked to be net-new
> (not one of the 12 items in Part A) and to hold up against the documented design
> intent — where a rationale exists, it is noted.
>
> Cross-references to "known issue #N" point to the working list the audit was
> seeded with: the Part A items A1–A8 plus the four additional issues (B0a–d).

### Window-manager verbs & input dispatch

**[HIGH] Window verbs and destructive confirms fire underneath a still-visible help/summary/ledger overlay, which never closes.** [merged across Input-modality and Overlays-consistency dimensions]
**Symptom:** With `?`/`i`/`t` open, pressing `Ctrl-a` then a verb runs the verb while the read-only overlay stays painted on screen. `Ctrl-a s` floats the project switcher on top of a still-rendered help card; `Ctrl-a x`/`Ctrl-a d` arms a kill/discard whose red confirm prompt renders in the detail bar that the ~40-row help card covers — so a reflexive `y` confirms the kill blind. The doc promises the opposite ("any key (Esc included) dismisses it").
**Root cause:** In `on_key`, the prefix arms and returns at band 3 (`app.rs:744` `if self.keymap.is_prefix(key) { self.prefix_armed = true; return; }`) BEFORE the overlay-dismiss at band 5 (`app.rs:764` `if self.show_help || self.show_summary || self.show_ledger {`). `dispatch_verb` (`app.rs:852`) and `open_project_switcher` (`app.rs:989`) never clear the overlay flags; `draw` stacks them (`ui.rs:48-90`). `arm_kill` sets the red prompt (`app.rs:1796`, styled at `ui.rs:910`) without clearing help, and `centered_rect` clamps the card to full screen height (`ui.rs:1675`).
**Fix:** Clear `show_help`/`show_summary`/`show_ledger` at the top of `on_prefix_key` before dispatching, so an info overlay and a window modal/confirm can never coexist. This closes the overlay and runs the verb in one gesture.
**Note:** The dismiss-on-any-key contract is documented (`app.rs:762-763`) to keep a typo from trapping the user — which argues the prefix *should* dismiss it too, not bypass it.

**[HIGH] Closing a still-running docked agent tells you to press `r` to "refind" — but `r` re-sorts the Spine and the "roster" was deleted.** [merged across Window-manager and Input-modality dimensions]
**Symptom:** `Ctrl-a w` on a coin whose agent is still live footers `closed {issue} · still running — r to refind`. Pressing `r` cycles the Spine sort instead of re-finding the agent. The closely-related "refind via the roster" guidance points at a view removed in v1.7.
**Root cause:** `app.rs:1767` emits the footer; `r` is bound to `CycleSort` (`keymap.rs:146` `(Action::CycleSort, "sort", &["r"])`); `grep refind` finds only comments and the footer string — no refind action exists. The roster was folded away (`ARCHITECTURE.md:234` "There is no separate agents-roster tab any more"). The real re-attach path is Enter on the issue's Spine row: `app.rs:1430` `if self.backends.contains_key(&key) { self.open_agent_window(&key); return; }`.
**Fix:** Replace the footer with the real gesture, e.g. `closed {issue} · still running — select it & ⏎ to re-open`, and drop the stale "refind via the roster" comments at `app.rs:1737`/`1762`. Do not touch the backend-retention logic (`app.rs:1764-1765`) — only the wording is wrong.
**Note:** The close/undock semantics (keep a live agent's backend alive, reclaim only when dead) are correct and documented; only the recovery hint is stale.

**[MEDIUM] "Back to nav" (`Ctrl-a 0`) while zoomed hides the Spine instead of showing it.**
**Symptom:** While the big pane is zoomed, the gesture documented as "Jump focus straight home to the Spine in one hop" leaves the coin filling the viewport and the Spine entirely off-screen — reading as a silent no-op. Direct keys still route to the (invisible) Spine, so the user can blind-dispatch issues they cannot see. `Ctrl-a /` has the same trap.
**Root cause:** `FocusNav` sets `focus = 0` but never clears zoom (`app.rs:862` → `focus_nav()` at `window.rs:680`). The zoomed renderer draws `rail_big_index(n, focus, active)` (`ui.rs:264-266`), and with `focus == 0` that returns the active coin, never the Spine (`layout.rs:52-54`). `StartSearch` sets `focus = 0` directly (`app.rs:872`) without clearing zoom either.
**Fix:** Clear `zoomed` inside `focus_nav()` (mirroring `close_focused`/`close_issue` at `window.rs:621,647`), and route `StartSearch` through `focus_nav()` so both Spine-returning paths reveal the Spine.
**Note:** Zoom persisting across `FocusLeft`/`Right` is deliberate and correct (`window.rs:684-689`); the rationale does not cover `focus=0`, which has no window to surface except the active coin.

**[MEDIUM] Pinning a layout (`Ctrl-a |`) is a one-way door — no in-session return to automatic mosaic/rail.**
**Symptom:** One tap of `|` to peek at the other layout silently and permanently disables the adaptive layout the docs advertise ("They tile automatically by count"). Dock six coins later and you're stuck in a cramped six-tile mosaic that would normally have become a rail, with no gesture to restore auto and no footer signal that auto is off.
**Root cause:** `force_layout` sets `layout_manual = true` (`window.rs:723-726`); `refresh_layout` then returns early forever (`window.rs:714-718`). `layout_manual` is only reset by `WindowSet::new()` (project switch). `LayoutMode` has two variants, so `|` thereafter only flips between manual modes; the footer only ever prints `layout: rail`/`layout: mosaic` (`app.rs:1940`).
**Fix:** Make `Ctrl-a |` cycle tri-state (auto → rail → mosaic → auto), where the auto step sets `layout_manual = false` and calls `refresh_layout()`; reflect the live state in the footer ("layout: auto (mosaic)" vs "layout: rail (manual)") and update the help line (`ui.rs:1128`). No serialization change needed — the override is already session-only.
**Note:** The session-only scope is documented (`app.rs:2124-2126`); the trap is the absence of any in-session return-to-auto gesture or footer signal.

**[LOW] Zoom has no on-screen indicator — a zoomed coin looks like a bug (the Spine just vanished).**
**Symptom:** In steady state a zoomed coin is visually identical to a one-coin mosaic except the permanent Spine is missing, with no marker and no breadcrumb back. A user who zoomed a while ago reads it as the app having broken.
**Root cause:** `render_strip` draws only the big pane when zoomed and returns (`ui.rs:263-268`); the title bar shows mark/label/key/pin but no zoom glyph (`ui.rs:587-595`); the word "zoom" appears only in the prefix-armed hint (`ui.rs:978`).
**Fix:** Push a `⤢ zoom` chip into the title span vec in `render_agent_window` (`ui.rs:587-595`) next to the existing `⊙ pin` chip, so the latched state and its escape key are always visible.
**Note:** That zoom hides the Spine is documented as intentional and non-destructive (`ui.rs:263`, `window.rs:684-686`); neither doc addresses the missing persistent indicator. The header and hint lines still render, so the screen is not blank.

**[LOW] `Ctrl-a |` while zoomed flips (and permanently latches) the layout with no visible change.**
**Symptom:** Pressing `Ctrl-a |` while zoomed footers a new mode but the screen is unchanged (the zoom path ignores `layout`), and it has silently latched layout into manual for the session. The flip only becomes visible on un-zoom.
**Root cause:** `toggle_layout` flips layout, sets `layout_manual = true`, and footers `layout: {mode}` (`app.rs:1933-1941`), but the zoomed renderer always draws `rail_big_index` regardless of `layout` (`ui.rs:264-266`).
**Fix:** Refuse the layout toggle while zoomed with an explanatory footer ("un-zoom (Ctrl-a z) to change layout") rather than un-zooming as a side effect of pressing `|`.

**[LOW] Pressing the prefix twice anywhere but an agent silently eats both keystrokes.**
**Symptom:** On the Spine, a Deps face, or the Fleet, `Ctrl-a Ctrl-a` does nothing — no re-arm, no footer, no beep — and the armed state is lost, so the next key lands as a raw direct action.
**Root cause:** `on_prefix_key` forwards a double-prefix to a focused agent only `if let Some(issue) = ...agent_issue()`, else implicitly returns (`app.rs:838-844`).
**Fix:** When a double-prefix lands with no focused agent, re-arm the prefix or set a brief footer (e.g. "no agent here — Ctrl-a then a verb").
**Note:** This falls under the documented "an unbound prefix key is a harmless no-op" policy (`app.rs:846`); the agent-forwarding asymmetry is itself justified (`app.rs:835-837`), so this is the low end of the scale.

**[LOW] Esc is a complete no-op on the Spine, Deps and Fleet — there is no "back out" key for non-agent windows.**
**Symptom:** Esc — the universal cancel/up-a-level key — does nothing on the Spine, a Deps face, or the Fleet. A user in a deep Deps re-root reaches for Esc to back out and nothing happens.
**Root cause:** Each dispatch is guarded `if key.code != KeyCode::Esc` (`app.rs:794,802,812`) and Esc is deliberately absent from the Action enum (`keymap.rs:38`).
**Fix:** On a Deps face, map Esc to the existing `Back` (pop the re-root history, today only on Backspace/`b`) — that is the one window where Esc conventionally pops a stack. Spine and Fleet are the base layer, so leaving Esc inert there is defensible.
**Note:** Esc is documented as "reserved (a fixed, context-sensitive key)" (`keymap.rs:30`) and is wired in every modal/overlay/PTY; the gap is only that a Deps dive-in stack has no Esc affordance.

### Spine navigation & selection

**[MEDIUM] No way to jump to top/bottom or page through the Spine — only single-step j/k.**
**Symptom:** The only Spine movement is one row at a time. Reaching the DONE band at the bottom of a long readiness schedule (or back to NEEDS-YOU at the top) requires one keypress per issue, and the muscle-memory keys (gG, PgUp/PgDn, Home/End) silently no-op.
**Root cause:** The Action enum defines only `MoveUp, MoveDown` (`keymap.rs:42-43`); `DIRECT_DEFAULTS` binds nothing to Home/End/PageUp/PageDown (`keymap.rs:124-151`) though those names parse for remapping (`keymap.rs:301-304`); `dispatch_spine` handles only `MoveDown=>move_selection(1)`/`MoveUp=>move_selection(-1)` (`app.rs:1183-1184`).
**Fix:** Add `MoveTop`/`MoveBottom` (default Home/End, since `g`=OpenFleet) and `PageUp`/`PageDown`, routed in `dispatch_spine` (and `dispatch_deps` for symmetry). Implement paging with the existing `clamp_selection` (`window.rs:852`), NOT `move_state` — the latter wraps via `rem_euclid` so PageDown near the bottom would wrap to the top.

**[MEDIUM] Incremental search moves the highlight but the preview/detail pane keeps showing the filtered-out issue.**
**Symptom:** While typing a search, if a keystroke hides the selected issue the Spine highlight jumps to the new first match but the preview coin keeps rendering the previously-selected issue's tree/chat. The mismatch persists even after Enter, and the user must press an extra arrow key to force the panes to agree.
**Root cause:** When `rebuild_order` re-aims `self.root` to `order[0]` because the selection was hidden, it calls only `self.sync_list_selection()` — not `reaim_preview()` (`app.rs:562-568`). Contrast `move_selection` (`app.rs:1298-1299`) and `aim_spine` (`app.rs:671-673`), which both re-aim the preview. `on_search_key` routes every keystroke through `rebuild_order` (`app.rs:3162-3180`).
**Fix:** Call `self.reaim_preview()` inside the re-aim branch at `app.rs:565-567`, immediately after `self.root = self.order[0].clone()`, so it fires only when root actually moved. `reaim_preview` already self-guards against pinned coins.
**Note:** The `rebuild_order` doc deliberately scopes consistency to "the list highlight and the detail bar" (`app.rs:563-564`); it never addresses the follower preview, so the rationale does not defend leaving it stale.

**[LOW] From a filter-hidden selection, j/k cannot reach the first visible row (index 0 is skipped).**
**Symptom:** After a cycle/needs-you jump lands on a filter-hidden issue (no highlight), pressing `j` lands on row 2 (skipping row 1) and `k` flings to the last row — j does not re-enter the list at the top.
**Root cause:** A hidden selection sets `list_state` to None (`app.rs:651-656`); `move_state` then does `cur = state.selected().unwrap_or(0)` so None+`j` → index 1 (`window.rs:845-846`), and `move_selection` has no None special-case (`app.rs:1293-1300`).
**Fix:** In `App::move_selection` only, special-case the None state: select index 0 for a downward step and `len-1` for upward before delegating to `move_state` (don't touch the shared primitive, which other lists use).
**Note:** The advertised recovery is "hidden by filter (clear it to list)" (`app.rs:3155-3156`), and `rebuild_order` keeps filter/search highlights on a visible row (`app.rs:563-567`), so this only arises via the narrow jump-onto-hidden path and self-corrects.

**[LOW] j/k wrap silently between NEEDS-YOU (top) and DONE (bottom) with no edge signal.**
**Symptom:** `k` on the first row teleports to the DONE band and `j` on the last wraps to NEEDS-YOU, with a full-viewport scroll and no edge bump, footer, or flash — easy to hit by accident given there is no Home/End.
**Root cause:** `move_state` wraps unconditionally via `(cur + delta).rem_euclid(len)` with no clamp option or end signal (`window.rs:840-848`); `move_selection` calls it for both directions (`app.rs:1293-1301`).
**Fix:** Keep the wrap but set a brief footer ("top of schedule — wrapped to DONE" / "bottom — wrapped to NEEDS-YOU") when `move_selection` crosses the boundary, matching the `set_jump_status` pattern. Prefer this over clamping, which would remove the only fast cross-list traversal.
**Note:** The wrap is a documented house convention shared across list-nav (`window.rs:837-839`) and is a near-universal TUI norm — hence low.

### Keybinding discoverability & help overlay

**[HIGH] The `?` help overlay silently clips its bottom rows on a short terminal, with no scroll and no "more below" marker.** [merged across Keybinding-discoverability and Overlays-consistency dimensions]
**Symptom:** The overlay — the documented authoritative binding reference — sizes to ~41 rows but renders a plain non-scrolling `Paragraph`. On any terminal under ~41 rows (routine in this tiling/split-pane cockpit), the entire "— workspace —" section (switch-project, configure, global-view, editor, discard, reclaim), `Quit`, and the "rebind … in ~/.config/lindep/config.toml" footer fall off the bottom. There is no scrollbar, no marker, and the next keypress dismisses the whole overlay.
**Root cause:** `render_help` sizes `centered_rect(78, rows.len() as u16 + 7, ...)` (`ui.rs:1158`) and renders `Paragraph::new(lines).block(block)` with no `.scroll(...)` (`ui.rs:1191`); `centered_rect` clamps `h = height.min(area.height)` (`ui.rs:1675`) but the Paragraph has no offset. The same file already scrolls a tall Paragraph elsewhere (`ui.rs:902`).
**Fix:** Reuse the `ui.rs:902` pattern — add a `help_scroll` to App, render `.scroll((help_scroll, 0))`, advance on Up/Down/PgUp/PgDn while `show_help` (intercept before the dismiss branch), and dismiss only on `?`/Esc. Minimal alternative: move `Quit` + the rebind-path footer to the top of the lines vec so the two load-bearing rows are never the ones clipped, and render a "▾ more" marker.
**Note:** No doc justifies the clipping; the any-key-dismiss contract (`app.rs:762-763`) is exactly what removes any chance to scroll, reinforcing the finding.

**[MEDIUM] Pressing the prefix and then an unbound (or unlisted) key gives zero feedback that the prefix was even armed.**
**Symptom:** After `Ctrl-a`, an unbound second key disarms silently — no footer, no flash — and the next keystroke is reinterpreted as a direct key. The armed-hint footer also advertises only ~8 of 18 prefix verbs, so a user who reasonably guesses a real-but-unlisted verb (`Ctrl-a a` global-view, `Ctrl-a s`) gets identical silence to a dead key.
**Root cause:** `on_prefix_key` returns on an unbound key with no acknowledgment (`app.rs:845-847` `let Some(verb) = ... else { return; }`); the armed footer omits half the bound verbs (`ui.rs:977-986` vs `keymap.rs:155-190`).
**Fix:** On an unbound prefix key, set a brief footer like `{prefix} {key}: no window command — press ? for the list`. Pair it with making the armed footer enumerate all verbs or end in "· ? for all", since even a correctly-typed unlisted verb currently looks like nothing happened.
**Note:** The "harmless no-op" comment (`app.rs:846`) justifies only non-destructiveness, not the lack of acknowledgment.

**[MEDIUM] Many `Ctrl-a` verbs work from any focus, but the binding reference can't even be *opened* from inside an agent chat — the one pane that most needs it.**
**Symptom:** A focused Chat coin forwards every key (including `?`) to the PTY, and `Ctrl-a ?` is a no-op there, so a user driving an agent cannot summon the overlay that documents the prefix verbs. The chat footer lists only 5 of ~18 verbs. To learn that `Ctrl-a e` opens the editor while driving an agent, the user must `Ctrl-a 0` home, open `?`, read, and return.
**Root cause:** A focused Chat coin forwards every key to its PTY (`app.rs:776-787`); `ToggleHelp` is not in `VERB_DEFAULTS`, so `Ctrl-a ?` resolves to nothing (`app.rs:845-848`, `keymap.rs:155-189`), while every other window verb dispatches from any focus (`app.rs:852-888`).
**Fix:** Special-case `Ctrl-a ?` in `on_prefix_key`: if the key equals the ToggleHelp direct binding AND the focused window is a Chat coin (or Fleet), toggle `show_help` — without re-adding help to `VERB_DEFAULTS` (which would re-introduce the prefix/direct duplication ENG-562 removed). Add `· {p} ? help` to the chat footer hint.
**Note:** The "overlay/issue actions are direct-only … they're spine-list operations" rationale (`keymap.rs:168-173`) correctly covers search/summary/ledger but misclassifies help, which documents the from-any-focus prefix verbs and is not a spine-list operation.

**[LOW] The from-any-focus needs-you jump (`Ctrl-a n`) never appears in the `?` overlay or any chat footer.**
**Symptom:** A user trapped in a chat pane (where direct `n` goes to the PTY) cannot discover that `Ctrl-a n` jumps to the next agent that needs them — `Ctrl-a n` appears on no surface, while the parallel `Ctrl-a Tab` (ContextToggle) is surfaced via its parenthetical.
**Root cause:** `JumpNeedsYou` is listed only by its direct key (`ui.rs:1082-1083`); its live prefix form (`keymap.rs:174`, dispatched at `app.rs:878`) is omitted from the help overlay and the chat footer (`ui.rs:992-998`).
**Fix:** Mirror the existing ContextToggle convention — change the spine-section description at `ui.rs:1083` to "jump to the next agent that needs you (Ctrl-a n from a chat)". (ContextToggle itself is already discoverable at `ui.rs:1049-1052` and `ui.rs:993`, so no change is needed there.)
**Note:** The binding's existence is justified as "reachability, not redundancy" (`keymap.rs:170-173`); the gap is purely its discoverability, hence one missing parenthetical.

### Overlays & modals consistency

**[HIGH] A background notification can overwrite the kill/discard confirmation text while leaving the red "confirm kill" styling and hint armed.** [merged across Overlays-consistency and Footer/header dimensions]
**Symptom:** After arming "kill agent on ZAP-7?", routine chatter like "ZAP-2: ran Bash" replaces the prompt text — but it stays painted in alarming red bold under a "y / ⏎ confirm kill" hint. The visible message no longer names what `y` will destroy; the next `y` still kills ZAP-7.
**Root cause:** The kill prompt is stored in the shared `status_msg` slot (`app.rs:1796`); the detail bar styles it red purely on the flag (`ui.rs:910` `if app.kill_confirm.is_some()`) and the hint likewise (`ui.rs:974`). Background writers overwrite `status_msg` with no confirm guard: `AgentAction` (`app.rs:2576`), `AgentCommitted` (`app.rs:2610`), `set_footer` for Notification/AgentExited (`app.rs:2657`). The author already guards this exact hazard for `RepoRequested` (`app.rs:2626-2641`) but nowhere else.
**Fix:** Introduce `fn confirm_pending(&self)` (kill/discard/repo) and have `set_footer` plus the AgentAction/AgentCommitted/MaterializeProgress arms skip the `status_msg` write while it is true — exactly as `RepoRequested` already does. Or render the prompt from a dedicated field rather than shared `status_msg` so text, styling, and hint cannot diverge.
**Note:** No doc defends the gap; the `RepoRequested` comment ("a 'y to pull' footer shadowing a pending kill would make `y` destroy the agent under a misleading message") argues directly for the fix.

**[MEDIUM] A repo-pull confirmation can be raised while a full modal owns the keyboard, so its "y to pull" footer lies and the confirm leaks to a later key.**
**Symptom:** If an agent fires `request-repo` while the project switcher is open, the footer says "y to pull" yet typing `y` filters the switcher and never reaches the confirm. After closing the switcher with Esc, `repo_confirm` is still pending, so the very next key silently denies (or confirms) a pull the user has likely forgotten — a modal they never saw governs their next keystroke.
**Root cause:** The `RepoRequested` guard checks only kill/discard/repo confirms (`app.rs:2633-2636`), omitting `project_switcher`/`reclaim`/`repo_select`/`global_view`. But in `on_key` those full modals resolve and return at bands 1b-1e (`app.rs:693-717`) before the `repo_confirm` band 2c (`app.rs:738`). The footer renders unconditionally from `status_msg` (`ui.rs:908`) and the 60%-height switcher leaves the footer row visible (`picker.rs:148`).
**Fix:** Extend the `app.rs:2633` guard to also bail (footer-only) when any full modal is open. Better, add a single `self.full_modal_open()` helper and gate all keyboard-capturing confirmations through it so the on_key band order and apply_event guards stay in sync.
**Note:** The guard's own comment endorses this deferral pattern but reasons only about destructive confirms (`app.rs:2626-2632`), leaving full modals an unintended gap; the agent's request is re-issuable, so dropping it is safe.

**[MEDIUM] The help overlay is the only modal with no "how to exit" hint.**
**Symptom:** Every other overlay states its exit (picker "esc cancel/close", global "esc back", summary/ledger "any key to close"). The help overlay's only chrome is the bare title ` lindep `, so a user opening the one panel meant to teach the interface is left guessing whether Esc, q, or any key closes it.
**Root cause:** `render_help` titles the block ` lindep ` with no exit hint (`ui.rs:1187-1189`), unlike `ui.rs:1291` (summary) and `ui.rs:1409` (ledger).
**Fix:** Append "· any key to close" to the ` lindep ` title (or a pinned bottom line), matching summary/ledger verbatim.
**Note:** The actual behavior is "any key (Esc included) dismisses it" (`app.rs:762-764`), so surfacing that hint is purely additive.

**[LOW] Three different dismissal conventions across the overlay family make exit behavior unpredictable.**
**Symptom:** The same gesture means opposite things: `j`/`Down` navigates a switcher, cancels a kill confirm, and slams a summary shut. After learning that pickers let you arrow a list, the first arrow reflex in a summary closes it.
**Root cause:** Info overlays close on any key (`app.rs:764-768`); confirm prompts treat y/Y/Enter as confirm and every other key as cancel (`app.rs:1885-1891`, `1832-1838`, `1864-1870`); full modals reserve Esc/Ctrl-C and route other keys to filter/navigate (`app.rs:998-1028`, `1622-1666`).
**Fix:** Unify the dismissal verb on Esc — give help/summary/ledger Esc-or-toggle-key dismissal and have the confirm prompts also accept Esc as cancel (today only the discard footer even hints "any key to cancel"). Keep y/n for confirms.
**Note:** Each family's local behavior is individually documented; the cross-family divergence is not. The "info overlays can't be scrolled" half overlaps known issue #5, so the net-new contribution here is narrowly the differing dismissal verb.

### Agent lifecycle feedback

**[HIGH] A failed agent vanishes into the READY band, pixel-identical to never-launched work, and even flashes celebratory green.**
**Symptom:** From the Spine — the only surface for a windowless dispatch — a crashed agent's row shows a plain `▸` rail, normal title, and no `✗`/red, identical to a fresh undispatched issue. The only transient cue is a flash, and Failed shares Done's GREEN flash for ~4 frames, so a crash even flashes like a clean finish, then disappears. The user believes failed work succeeded or never ran.
**Root cause:** `readiness` falls through to graph truth, so a terminal Failed shows `Ready` again (`app.rs:582-608`). The READY gutter renders the `▸` rail unconditionally and suppresses the agent marker/tint (`ui.rs:1458-1469,1525-1526`). `Flash::Finished` is inserted for `Done | Failed` alike (`app.rs:2538-2540`) and rendered `bg(STATUS_600)` = green (`ui.rs:1480`, `theme.rs:44`). No sticky attention flag is set for Failed.
**Fix:** (1) Add a `Flash::Failed` variant backed by RED_400 so a crash no longer flashes green (`app.rs:2538`, `ui.rs:1480`). (2) In the READY gutter, when the issue's last fleet entry is `Failed`/`Stopped`, render the red `✗` from `agent_marker` instead of the plain `▸` — a red ✗ signals "re-dispatch this because it crashed" without the false-success problem a green ✓ would cause.
**Note:** The marker-suppression rationale (`ui.rs:1461-1463`) is written entirely about not showing a stale green ✓ for a *Done* revert; it never considers that the same path erases the Failed signal — and RED_400 was added precisely so "a crash reads as 'wrong'" (`theme.rs:46-48`), arguing for the fix.

**[MEDIUM] A wedged fresh launch is stuck on a permanent "starting…" card with no timeout and no way to kill it.**
**Symptom:** If setup wedges before `AgentSpawned` (a hung `git worktree add`), the window renders "◌ starting agent on {key}…" forever. Re-pressing the button footers "already opening…"; `Ctrl-a x` is refused with "agent on {issue} is not running" (no fleet entry yet). The row is unrecoverable without restarting the cockpit.
**Root cause:** A button launch inserts into `pending_launch` (a plain HashSet, no deadline) but adds no fleet entry until `AgentSpawned` (`app.rs:1495-1497`, `2487`). `arm_kill` gates on a live fleet entry (`app.rs:1792-1793`). The wedge-deadline self-heal exists only for the `resuming` map, not `pending_launch` — and `tick_frame` documents the gap: "A button launch arms no `resuming` entry, so its own `pending_launch` is untouched" (`app.rs:2979-2980`).
**Fix:** Give `pending_launch` the same per-issue grace deadline the `resuming` map has, expired in `tick_frame` exactly as resumes are (`app.rs:2981-2990`) — drop the card and footer "launch on {key} timed out — press Enter to retry". Let `arm_kill`/the button cancel a `pending_launch` with no fleet entry (send `workspace.cancel`) so a wedged start is recoverable.
**Note:** The team already shipped and tested this fix for the resume path (`app.rs:4784`); the fresh-launch path has the identical hazard with no fix.

**[MEDIUM] An unrelated notification clears every issue's in-flight launch guard.**
**Symptom:** While issue X is mid-launch, an auto-push-failed or scratch-skipped toast for issue Y removes X's double-press protection. A second Enter on X re-fires a launch, which the supervisor backstops with a contradictory "{X} already has a running agent" footer plus a spurious extra window-open.
**Root cause:** The catch-all `Notification` arm calls `self.pending_launch.clear()` for ALL issues (`app.rs:2427-2429`), but the supervisor emits bare `Notification` for many cross-issue events: capacity (`supervisor.rs:345`), repo-skip (`609`), scratch-skip (`714`), auto-push failure (`notify.rs:913`) — none carrying the rejected issue id.
**Fix:** Stop clearing the whole set on every Notification. Give just the three true launch rejections (capacity, already-running, still-stopping) a typed carrier (e.g. `AppEvent::LaunchRejected { issue }`) so `apply_event` removes only that issue; leave all other Notifications untouched.
**Note:** The field doc scopes the clear to "the rejected issue," not all of them (`app.rs:206-209`); the supervisor's own dedup prevents an actual duplicate spawn, bounding the harm to the confusing footer.

### Footer / header / status information architecture

**[MEDIUM] An agent exiting clears the sticky needs-you alert even when another agent still needs you.** [merged across Agent-lifecycle and Footer/header dimensions]
**Symptom:** If agent A is flagged NeedsYou (sticky footer up) and an unrelated agent B finishes, B's exit clears `needs_you_alert` — even though A is still NeedsYou. A's prompt then loses its "don't let routine chatter bury me" protection, and the next AgentAction line overwrites it, while the header `⚑N needs you` badge still shows A — the two surfaces disagree.
**Root cause:** The `AgentExited` arm routes through `self.set_footer(...)` (`app.rs:2508`), and `set_footer` unconditionally does `needs_you_alert = false` (`app.rs:2657-2660`). `AgentExited` fires on every process death (`backend.rs:393`) and does not touch the fleet (test `app.rs:4656-4657`). The sibling `AgentReaped` arm does it correctly via `clear_needs_you_alert_if_resolved()` (`app.rs:2591`).
**Fix:** In the `AgentExited` arm, write the footer via `self.status_msg = Some(...)` (not `set_footer`) and then call `clear_needs_you_alert_if_resolved()`, so an exit drops the guard only when no agent still needs you — mirroring `AgentReaped`.
**Note:** `set_footer`'s doc justifies superseding the alert for "deliberate, low-frequency events" (`app.rs:2655-2656`) — which fits a user action, but not an unrelated agent's autonomous exit; the parallel arms prove the intended multi-needy pattern.

**[MEDIUM] The most important header badges (needs-you, elsewhere, resuming) are appended last and clipped first on a narrow terminal.**
**Symptom:** On a half-width pane or with a longer project name, the genuinely actionable alerts (`needs you ⚑`, `⚑N elsewhere`, `resuming N…`) are the first to fall off the right edge, while the rarely-acted-on "N issues · M edges" stays fully visible.
**Root cause:** The left half is one un-wrapped Line pushed name → issues → edges → cycles → agents → needs-you → elsewhere → resuming (`ui.rs:159-215`), rendered with no `.wrap` in a `Constraint::Min(0)` area (`ui.rs:241-243`), so it hard-truncates at the right and clips the last-pushed spans first.
**Fix:** Order the header by salience, not data category — render the alert cluster (needs-you/elsewhere/resuming) immediately after the project name and before issues/edges/cycles; or give the alert cluster a reserved fixed-width slot that project metadata can't squeeze out.
**Note:** The badges' importance is documented ("so a backgrounded prompt is never invisible", `ui.rs:192-194`), which argues for fixing their clip order.

**[MEDIUM] Degraded mode has no persistent indicator — the only "agents off, here's why" signal is a one-shot footer that the next keystroke wipes.** [merged across Footer/header and Empty/edge dimensions]
**Symptom:** When the control plane fails to arm (unregistered project, declined onboarding, hook-port clash), the actionable reason is shown once as a transient footer and cleared by the very first Spine/Deps key. The cockpit then shows full chrome — header counts, a READY band, the "dispatch" divider, the `▸` rail — with no standing signal agents are off; pressing Enter yields only the opaque "agent control plane unavailable".
**Root cause:** The degraded reason is sent as a transient `Notification`/`status_msg` (`main.rs:558-563,625-628`); `acknowledge` clears it on any Spine/Deps key (`app.rs:828-829`); `render_header` never renders a degraded marker (`ui.rs:157-247`); the launch refusal is the bare string at `app.rs:1500`.
**Fix:** Persist the degrade reason on App and render a dim `⚠ agents off` chip in the header whenever `workspace.is_none()` on a non-demo run (gate on a dedicated flag, since `--demo` also leaves workspace None by design). Reuse the stored reason in the `app.rs:1500` refusal instead of the jargon string. The reason strings already exist — this is plumbing them somewhere persistent.
**Note:** The banner suppression is documented as wanting "the real reason [to] stand" so the user isn't left "believing agents work when the control plane never armed" (`main.rs:376-383`) — a goal the transient-footer delivery defeats on the first keypress.

**[LOW] Transient footers never auto-expire and aren't cleared while a chat pane is focused, so stale confirmations linger over the detail bar.**
**Symptom:** A one-off footer like "layout: mosaic" or "opened X in editor" sits frozen in the detail bar indefinitely while the user works in a chat — and the per-issue "blocks N · blocked-by M · ⊘ blocked" context is suppressed the whole time. Nothing distinguishes a live message from a stale leftover.
**Root cause:** `tick_frame` never touches `status_msg` (`app.rs:2967-2991`); the Chat branch forwards keys and clears only `needs_you_alert`, not `status_msg` — no `acknowledge` call (`app.rs:776-787`); `render_detail` returns early on `status_msg.is_some()`, hiding the issue line (`ui.rs:907-920`).
**Fix:** Tag footers transient vs sticky at the call site (e.g. `status_msg: Option<(String, Option<expiry_frame>)>`), have `set_footer` stamp a TTL while confirmations/needs-you store `None`, and expire transient ones in `tick_frame` exactly like the existing flash/resume reaps. Optionally also `acknowledge()` when a chat pane first gains focus.
**Note:** `set_footer`'s own doc calls this the "transient footer line" (`app.rs:2655`) yet gives it no expiry; an actively-working agent overwrites it via AgentAction, so the freeze manifests mainly in idle gaps.

**[LOW] Header "N issues" count includes external blockers that never appear as Spine rows, so it disagrees with the Spine's own count.**
**Symptom:** On any project with cross-team external blockers, the header reads e.g. "42 issues" while the Spine title and list show "ISSUES 39" — two prominent counts labelled "issues" disagree, padded with phantom nodes the user can never select or dispatch.
**Root cause:** The header prints `g.len()` (`ui.rs:164`), and `Graph::len()` counts all nodes including materialized `external` blockers (`model.rs:391`, `162`); but `rebuild_order` skips externals (`app.rs:531`) and the Spine title uses `app.order.len()` (`ui.rs:445`). Externals arise in normal Linear ingestion (`linear.rs:347,355`).
**Fix:** Have the header reuse the Spine's external-skip predicate (or add `Graph::issue_count()` excluding externals) so the two counts can't drift; or relabel as "N issues · K external".
**Note:** The Spine exclusion is documented ("externals show in trees, not the project list", `app.rs:531`); nothing justifies the header including them — they're just derived from different sources.

### Empty / edge / error states

**[HIGH] Demo / read-only viewer advertises "Enter: open agent" and a dispatch lane, then refuses with "control plane unavailable".**
**Symptom:** In `--demo` — the literal Quick-Start command and documented exploration entry point — the banner says "Enter: open agent", the READY band shows a "dispatch" divider and a `▸` rail, and a first-time user who presses Enter gets the jargon dead-end "agent control plane unavailable", contradicting the "clean read-only graph viewer" contract.
**Root cause:** The live banner fires for `control_plane.is_some() || demo` (`main.rs:384-389`), but `control_plane_enabled(demo)` is `!demo`, so demo never arms the control plane and `workspace == None`. The READY dispatch hint (`ui.rs:534-535`) and `▸` rail (`ui.rs:1458-1468`) render unconditionally; Enter falls to `app.rs:1500`.
**Fix:** In `--demo` specifically: drop "Enter: open agent" from the banner; gate the READY "dispatch" hint and `▸` rail on `workspace.is_some()`; replace the `app.rs:1500` jargon with an actionable line ("read-only demo — agents need a real project; drop --demo").
**Note:** Refusing the launch is justified (`main.rs:347-353` — no real worktree/claude for a fictional issue), and keeping the banner is a deliberate "demo is a viewer, not a failure" choice (`main.rs:382-383`) — but neither justifies advertising dispatch the demo can never perform, nor the user-facing "control plane" jargon.

**[MEDIUM] An archived/moved issue whose agent still runs is counted and `n`-jumpable but has no row in the Spine "agents roster".**
**Symptom:** An agent that needs you — the most urgent thing the cockpit surfaces — can have no row in the Spine (the promised roster). `n` lands the footer on it ("needs you 1/1 — ENG-archived") with NO highlight, and the header counts it, so the count and the jump point at an invisible row.
**Root cause:** `rebuild_order` builds `order` purely from `self.graph.keys()` (`app.rs:526`), so a fleet member that left the graph gets no row; yet `workspace_summary` tallies the fleet (`app.rs:2871`) and `jump_to_needs_you` deliberately sources from the fleet (`app.rs:3019-3024`). The state is reachable on a project switch-back where the fleet is cleared then re-emitted while the issue is gone from the fresh graph.
**Fix:** At minimum, distinguish off-graph from filter-hidden in `hidden_note()` — when `!self.graph.contains(key)`, return a suffix like " · agent running on an issue no longer in this project (g for ALL AGENTS)" so the empty highlight self-explains and points at the global overlay that does list it. A fuller fix injects a synthetic muted Spine row.
**Note:** The jump comment (`app.rs:3011-3016`) deliberately sources from the fleet so the header count isn't a lie — but it trades an unreachable count for a jump that lands on an invisible, untagged selection. The global ALL AGENTS overlay does list the agent, mitigating total invisibility.

**[LOW] Filtering or searching to an empty result leaves a blank "ISSUES 0" Spine with no message and no footer feedback.**
**Symptom:** Pressing `f` (has-deps on a flat backlog) empties the list — the entire Spine, band headers included, vanishes with no "0 of N", no "no matches", and no highlighted row. It reads as a crash or lost project, with no on-screen hint that the filter caused it or how to undo it.
**Root cause:** `CycleFilter` mutates the filter and calls `rebuild_order` with no footer (`app.rs:1193-1196`); neither `render_spine` nor `render_banded_spine` has an `order.is_empty()` branch (`ui.rs:449-520`); the header just shows "ISSUES 0".
**Fix:** After the rebuild, when `self.order.is_empty()` set a footer reusing the existing `hidden_note` wording (e.g. "filter:has-deps · 0 of N — clear to list"), and add one placeholder line in the Spine body, reusing the MUTED+italic style the deps empty-state already uses (`ui.rs:759-765`).
**Note:** The maintainers already signpost a hidden selection on the *jump* path (`hidden_note`, `app.rs:3152-3160`) but not when the user's own `f` blanks the whole list. The header right-side does show a muted `filter:has-deps`, so feedback is weak rather than zero; the search-empty case is already handled by the search hint line.

**[LOW] Pressing `i` on an off-graph selection opens an invisible summary overlay that silently swallows the next keystroke.**
**Symptom:** With the selection aimed at an archived agent (reachable via `n`), pressing `i` paints nothing — and because "any key closes" the summary, the user's next keystroke (e.g. `j` to move down) is silently eaten dismissing an overlay they never saw. Two consecutive inputs produce no coherent feedback.
**Root cause:** `render_summary` early-returns before its `Clear` widget when the key isn't in the graph (`ui.rs:1203-1205` `let Some(issue) = g.get(key) else { return; }`), but `detail_key` returns the off-graph root anyway (`app.rs:494-496`), and `show_summary` is set true unconditionally (`app.rs:1203`). (Note: `render_ledger` does NOT share this defect — it paints a real card for an off-graph key, contrary to the original evidence.)
**Fix:** In the summary toggle, refuse to open onto a missing node: if `detail_key` resolves to a key not in the graph, set a footer ("no summary — {key} isn't in this project's graph (archived/moved)") and do NOT set `show_summary`, keeping open/close keystrokes symmetric.
**Note:** The off-graph fleet state is documented as real and designed-for elsewhere (`app.rs:589-591`, `3012-3015`); the summary overlay alone fails to handle it.

### Project switching & Deps window

**[HIGH] A re-rooted pinned Deps coin shows one issue's tree but flips (Tab) to a different issue's chat.**
**Symptom:** On a docked Deps coin (identity ISSUE-A), Enter re-roots to ISSUE-B so the title bar and both trees read ISSUE-B. Tab — "show me this issue's agent" — then silently flips to ISSUE-A's chat with no explanation, and can lazy-resume the *wrong* issue's agent.
**Root cause:** The deps title renders the roving `cursor.root` (`ui.rs:668-678`), but `flip_coin_face` toggles using the coin's fixed identity `w.kind.coin()`, not the cursor root (`window.rs:545-558`); identity is stable while re-root moves only `cursor.root` (`window.rs:257-275`, `324-326`). A pinned coin is the independent-explorer case that doesn't move the Spine (`app.rs:1233-1234`), and `flip_active_coin` may lazy-resume the agent (`app.rs:1386`).
**Fix:** When `cursor.root != coin.issue()`, append the identity to the deps title (e.g. `◆ ISSUE-B …  (chat → ISSUE-A)`), surfacing the divergence before Tab — using the existing root-vs-identity code in both the big-pane and card renderers. Avoid making Tab re-target the rooted issue's chat, which conflicts with the documented one-coin-per-identity merge model.
**Note:** The identity-vs-root split is documented for persistence/merge keys (`window.rs:325-326`, `app.rs:1233`), but those rationales never address the on-screen-vs-Tab mismatch or make it visible.

**[MEDIUM] Switching projects silently wipes every docked agent window; running agents survive but their chat panes vanish.**
**Symptom:** The doc promises continuity ("a later switch back re-attaches to their real screens"), but a user who had three agent chats tiled, switches away, and switches back finds a bare Spine + preview and must manually Enter each agent one at a time. The re-attach happens to the backend, not to any visible window.
**Root cause:** `activate_project` does `self.windows = WindowSet::new()` (`app.rs:1130`), discarding all docked coins, and disables layout persistence for the session (`app.rs:1159-1160`). On switch-back, `reemit_statuses` emits `AgentStatusChanged` whose handler only inserts a fleet entry (`app.rs:2531-2548`) — it opens no window. (The original AgentSpawned citation is mis-pinned; AgentSpawned isn't fired on switch-back.)
**Fix:** Cheapest increment — after the switch-back re-emit, count live windowless agents in the target project and footer "N agents running here — Enter to open" (the doc'd "Per-project layout persistence" at `app.rs:1156-1158` is the full fix). Hook `activate_project` or the AgentStatusChanged handler, not AgentSpawned.
**Note:** No work or scrollback is lost — backends are stashed as live Arcs and restored (`app.rs:1091-1115`); the loss is purely the on-screen layout, recoverable with one Enter per agent, hence medium not high.

**[LOW] Spine filter carries across a project switch, silently hiding most of the new project.**
**Symptom:** Set `filter:has-deps` in project A, switch to B, and B opens with `has-deps` still applied — every standalone issue in B is hidden on arrival, while the footer reports the full count ("switched to B · N issues"). The two disagree; it reads as data loss.
**Root cause:** `activate_project`'s clean-view block resets fleet/windows/search/flash/needs-you but omits `self.filter` (`app.rs:1119-1128`), contradicting its own comment "everything else starts fresh"; the footer reports `self.graph.len()` (`app.rs:1173-1177`) while the Spine honors the carried filter.
**Fix:** Reset `self.filter` to `Filter::All` inside the clean-view block, next to the search reset. (Drop the `sort` half of the original proposal — `Sort::Key`/CycleSort are slated for removal.)
**Note:** render_header does show a persistent (but MUTED, easy-to-miss) `filter:has-deps`, and it's recoverable with one `f` — hence low.

**[LOW] Global all-agents screen doesn't distinguish "here" from "elsewhere" and orders project clusters by raw UUID.**
**Symptom:** Every agent row reads `<glyph> <project name> · ISSUE` with no marker for the project you're inside, so the user can't tell cheap local re-root rows from costly cross-project switch rows until after committing. Clusters appear in arbitrary order because the sort keys off the invisible UUID, not the displayed name.
**Root cause:** `render_global_overlay` renders glyph + name + issue with no active-project marker (`ui.rs:127-138`); `all_agents` sorts by `project_id` UUID (`app.rs:2925`); Enter routes to `land_on`, which is a cheap re-root for the active project but a full switch otherwise (`app.rs:3097-3100`).
**Fix:** Sort the primary key by resolved project name (the renderer already has the name map), and mark the active project's rows (sourced from `self.fleet` vs `world`) with a "here" tag or distinct color so Enter's cost is predictable.
**Note:** Only manifests in multi-project workspaces on a low-traffic modal — but that modal is exactly where cross-project cost should be legible.

### Onboarding wizard flow

**[MEDIUM] Esc on step 1 (Repos) silently abandons the whole wizard instead of going back.**
**Symptom:** Esc means "back" on every step except the first; on Repos it tears down the wizard. A user walking backward through the flow lands on Repos and the same key that meant "back" a moment ago now discards all entered repos/prefix/scratch with no confirmation. On the re-configure path it's worse — the cancel is reported as "configuration unchanged" even though real edits were thrown away.
**Root cause:** `onboard.rs:592` `KeyCode::Esc => return Ok(false)` on Repos vs `onboard.rs:603,625` stepping back elsewhere; the backward walk routes Primary/BranchPrefix Esc to Repos. `run_for_project` maps `Ok(false)` to "configuration unchanged" (`onboard.rs:65-67`) while `for_project` pre-populates edits (`onboard.rs:240-278`).
**Fix:** Gate Esc on Repos behind a two-tap confirm, reusing the existing `acknowledged_unreachable`-style idiom (first Esc sets "esc again to cancel setup"). In `run_for_project`, only emit "configuration unchanged" when the cancelled draft equals the loaded binding; otherwise say "edits discarded".
**Note:** The per-step hint footer does differentiate "esc cancel" (Repos) from "esc back" (`onboard.rs:709-713`), a partial mitigation — but it's a single muted line a reflex-tapping user won't re-read, and the teardown is unconfirmed.

**[LOW] The remote-reachability probe freezes the whole UI for up to 12s per remote with no spinner or way to cancel.**
**Symptom:** On Enter at Confirm, the wizard paints a single static "checking remotes…" line, then blocks while probing each remote serially (each capped at 12s). For the rare unroutable-HTTPS case this is tens of seconds during which the frozen line is indistinguishable from a hang and even Ctrl-C can't abort.
**Root cause:** `unreachable_remotes()` probes serially and synchronously (`onboard.rs:493-498`, 12s cap at `onboard.rs:547`); `run_loop` paints once then blocks inside the probe without returning to `event::read()` (`onboard.rs:651-653`).
**Fix:** Animate the line and show per-remote progress ("checking <handle> (2/3)…") by pulling the iteration into `run_loop` so it can `terminal.draw()` between probes. (Off-thread/cancel machinery is heavier than warranted.)
**Note:** The probe is deliberately hard-capped because "the wizard owns the raw terminal — a hang there is unescapable" (`mirror.rs:330-334`), and `GIT_TERMINAL_PROMPT=0` + ssh `BatchMode` make credential-prompt remotes fail fast, not hang — so the freeze is usually sub-second; this is cosmetic polish, hence low.

**[LOW] A partially-filled scratch form is a dead-end without finishing or manually clearing it.**
**Symptom:** On the optional Scratch step, a user who types a name then changes their mind cannot advance: Enter routes to `add_scratch`, which errors "scratch needs a provision command" — demanding a command they never wanted. The only exits (backspace the name empty, Esc back) are undiscoverable from that error.
**Root cause:** `onboard.rs:629-632` advances only when the name is empty; otherwise Enter calls `add_scratch` → `to_draft`, which errors on a blank provision (`onboard.rs:151-153`).
**Fix:** When the provision is empty while the name is set, change the error to point at the exit: "add a provision command, or clear the name (backspace) to skip". No new keybinding needed — Esc already preserves state (it only changes the step, contrary to the original "discards everything" claim).
**Note:** The step is explicitly optional and Esc is non-destructive, so the misdirecting error — not a true trap — is the substantive issue.

**[LOW] A mistyped local path gives a misleading "not a directory, a URL, or a registered repo" error.**
**Symptom:** A path that doesn't exist (a typo like `./cor` for `./core`) is told it is neither a path, a URL, nor a repo — with no hint the path simply doesn't exist, so the obvious fix (correct the typo) isn't suggested.
**Root cause:** `resolve` only treats input as a path when `expanded.is_dir()` (`onboard.rs:301`); a non-existent path isn't URL-shaped (`onboard.rs:515-517`) and falls through to the generic catch-all (`onboard.rs:346`).
**Fix:** When the input starts with `~`/`.`/`/` or contains a path separator but `is_dir()` is false, return a path-specific error ("no directory at <expanded> — check the path"), reusing the already-computed `expanded`.
**Note:** The URL-gate doc's philosophy — "so a typo doesn't get silently registered" (`onboard.rs:513-514`) — argues for clearer path feedback, not against it.

**[LOW] Re-configuring and pressing Enter through Confirm writes the file and says "restart to apply" even when nothing changed.**
**Symptom:** Re-opening the wizard just to look and pressing Enter through Confirm rewrites registry.toml and tells the user a restart is needed — contradicting the "configuration unchanged" message that appears only on cancel, and nudging an unnecessary relaunch.
**Root cause:** `run_for_project` pre-populates from the existing binding then calls `write_binding` unconditionally and returns `Ok(true)` (`onboard.rs:661-667`), mapped to "saved … restart lindep to apply" (`onboard.rs:63-64`); there is no diff against the loaded binding.
**Fix:** Render the prospective registry document and compare it to the existing file text; if identical, skip the write and return "configuration unchanged".
**Note:** `write_binding` preserves comments/structure so a no-op write is byte-stable — no data loss, only the misleading message, hence low.

### Visual language consistency & legibility

**[LOW] The ◦ glyph means both "idle agent" and "medium priority" — colliding twice in one Spine row.**
**Symptom:** An idle agent on a medium-priority issue renders `◦ ○ ◦ ENG-123` — the identical bullet twice in one row, distinguished only by teal-vs-grey hue, defeating the codebase's own monochrome-survivability value.
**Root cause:** `agent_marker` maps Idle to ◦ (`theme.rs:160`) and `priority_marker` maps Medium to ◦ (`theme.rs:207`); `issue_line` places them two columns apart (`ui.rs:1489-1492`).
**Fix:** Change `Priority::Medium` off the bullet at `theme.rs:207` (e.g. `–` or `▪`), reserving ◦ for the idle agent; add a test asserting the agent-marker and priority-marker glyph sets are disjoint.
**Note:** The monochrome rule is documented and tested *within* the agent set (`theme.rs:144-146`), but not across the agent/priority columns that share a row. Both states are low-salience, so practical confusion is minor.

**[LOW] ○ distinguishes "Backlog" from "Todo" by colour alone — fails in monochrome.**
**Symptom:** Two distinct workflow states (Todo vs Backlog) render the same empty ring, separated only by INK-vs-MUTED foreground — identical on any monochrome/low-contrast terminal.
**Root cause:** `status_glyph` maps both Unstarted and Backlog to ○ with no shape backup (`theme.rs:112-113`); `status_glyph` has no distinct-glyph test (contrast `theme.rs:229`).
**Fix:** Add a distinct-glyph test over the Status variants mirroring `theme.rs:229`, which mechanically forces Backlog onto a non-colliding shape (avoid ⊘/⊝ — ⊘ is already Canceled/blocked).
**Note:** The detail bar and summary modal already print the literal "Todo"/"Backlog" label beside the glyph, so the collision loses information only in the label-less Spine/deps views, where status is secondary to the readiness banding — hence low.

**[LOW] ⊘ carries two unrelated meanings — "Canceled" (grey) and "Blocked" (amber) — in the same row.**
**Symptom:** A Canceled issue that is also graph-blocked renders `⊘ … ⊘` on one line: a grey ⊘ meaning "canceled" (a dead, done-ish state) and a trailing amber ⊘ meaning "blocked" (live, needs unblocking) — nearly opposite meanings keyed only by position and hue.
**Root cause:** `status_glyph` uses ⊘ for Canceled (`theme.rs:115`) while ⊘ is also the BLOCKED header (`ui.rs:530`) and inline blocked badge (`ui.rs:1497`); both render in one row.
**Fix:** Reassign `Status::Canceled` to a distinct shape (e.g. ⊗, clearly different from Failed's ✗) at `theme.rs:115`, reserving ⊘ ("no entry") for blocked; add a "one glyph, one meaning" note beside the existing colour rule.
**Note:** The bare collision exists only in the compact Spine row — the summary/detail panes carry adjacent text labels ("Canceled", "⊘ blocked") that disambiguate, and the co-occurrence (a dead issue with a live blocker) is an edge case in the recessive DONE band.

**[LOW] ▲/▼ mean both dependency direction and issue priority, colliding inside the summary overlay.**
**Symptom:** Within the summary card the up-triangle means "this issue is urgent" on the header line and "these are its blockers" (▲ BLOCKED BY) a few lines later; the down-triangle similarly flips between "low priority" and "downstream".
**Root cause:** `priority_marker` uses ▲/△/▽ (`theme.rs:205-208`); ▲/▼ are also the direction headers (`ui.rs:716,721,1241,1246`); `render_summary` draws both the priority ▲ (`ui.rs:1214`) and the ▲ header (`ui.rs:1251`).
**Fix:** Move only the priority markers off the triangle family (e.g. Urgent `‼`/High `!`/Low `·`), leaving ▲/▼ to mean graph direction everywhere — a contained four-line change in `priority_marker`.
**Note:** The direction triangles are always glued to a disambiguating word ("▲ BLOCKED BY") and differ in colour and position from the isolated amber priority ▲, so this is a shared-vocabulary wart rather than an active misread — hence low.

**[LOW] Column alignment assumes width-1 glyphs, but the status/priority glyphs are East-Asian "Ambiguous" and render double-width on some terminals.**
**Symptom:** On a terminal configured ambiguous-width=wide (mostly CJK locales/fonts), ◐/▲ occupy 2 cells but ⊘/◦ occupy 1, so the status/priority columns — and the key/title alignment to their right — stagger by a cell per row; `truncate` also over-budgets titles and overruns the pane.
**Root cause:** Status glyphs ●◐○◇ and priority ▲△▽ are EAW=Ambiguous while ⊘/· and ◦ are Narrow (`theme.rs:110-114,205-208`); `issue_line` formats each as one cell (`ui.rs:1491-1493`); `truncate` scores Ambiguous as width 1 (`ui.rs:1654-1671`). (The gutter agent markers are uniformly Narrow, so only the status and priority columns mix widths.)
**Fix:** Render each marker in a fixed-width field, or pick status/priority glyphs from a single width class, so a narrow glyph still consumes the same column width; add a test asserting all glyphs in a column share one `UnicodeWidthStr::width`. Make `truncate` width-mode-aware.
**Note:** The codebase demonstrably cares about display-width (the deliberately single-width spinner, `theme.rs:120-122`; the `truncate` doc) — this discipline just wasn't extended to the status/priority sets. Only manifests on a minority terminal config, hence low.

---

## Part C — Post-implementation review pass (2026-06-19)

After Parts A & B landed, the fixes themselves were re-audited — a Rust-correctness +
UI/UX pass over the diff, fanned out across the same 11 dimensions and adversarially
verified (every candidate finding re-checked against the actual code before it counted).
The diff held up: **no crashes, panics, or data-loss bugs; `cargo check` / `clippy` /
`test` all green.** The pass surfaced 33 verified follow-ups — almost all "the fix did
90% and stopped one line short of the affordance being learnable." Those now applied:

**Window-manager & input**
- **FocusLeft/Right onto the Spine while zoomed** re-opened the M1 blind-dispatch trap
  (zoom was only cleared by `FocusNav`/search). `after_focus_change` now clears zoom
  whenever focus lands on index 0.
- **A cancelled / timed-out wedged launch (M9)** stranded a permanent "◌ starting
  agent…" card that contradicted the footer. Both recovery paths now `undock_issue`
  and the timeout hint points at the gesture that actually works (select + ⏎).

**Overlays & confirms**
- **The ledger** was left on the old "any key closes" convention (no scroll, stale
  title) after help/summary moved to scroll-+-Esc/toggle. It now matches them
  (`ledger_scroll`, arrow/page scroll, `esc / t close`, scrollable long history).
- **H4 had a residual hole:** a background *needs-you* (local or elsewhere) and the
  elsewhere repo-request still wrote over an armed kill/discard/repo prompt. All three
  now gate on `confirm_pending()`, so a reflexive `y` can't fire under a swapped message.
- **`Ctrl-a ?`** from a chat/Fleet now *toggles* help (was open-only) and is advertised
  in the chat footer.

**Spine, header & theme**
- Single-row Spine no longer emits a spurious "wrapped to the bottom" footer on j/k.
- Sticky band header computes its scroll offset deterministically (no double-paint on a
  downward scroll; the parallel `is_divider` Vec is gone).
- Deps paging (PgUp/PgDn) uses the focused deps pane's height, not the whole terminal.
- Header "N issues" uses an allocation-free external count; the `⚠ agents off` chip is
  now *dim* (a standing condition, not a fresh alert); a demo run gets its own dim
  `read-only demo` chip; `--demo --snapshot` no longer paints `⚠ agents off`.
- Summary card sizes to the *real* wrapped height (no dead-end on a long title); global
  all-agents sort tie-breaks on project id (same-named projects stay distinct).
- Medium priority moved off the filled `▪` (collided with the Stopped `◼`) to the
  outline `◻`.

**Onboarding**
- The two-tap cancel warning is cleared once disarmed; a no-op re-config now skips the
  write and reports "configuration unchanged" (`write_binding` → `Result<bool>`).

**Deferred (low value / out of scope), with reasons**
- **EAW ambiguous-width column stagger** — minority CJK-wide terminals; needs a
  width-mode-aware `truncate` + a one-width-per-column glyph audit. Documented, not done.
- **B0a "caught up" empty-band placeholder** and **B0b pin/unpin overload** — explicitly
  undecided design calls.
- **Transient-footer auto-expiry** and **cancel-with-edits "edits discarded" wording** —
  cosmetic; the latter needs dirty-tracking the no-op-write machinery doesn't yet give.
