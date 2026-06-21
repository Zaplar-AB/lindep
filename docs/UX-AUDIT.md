# lindep — feedback status & work log

> Snapshot: 2026-06-22. Tracks every piece of feedback raised so far — what's
> **done**, what was **already in the tree**, and what's **still open**. Status
> key: ✅ done · 🔁 already in the tree (rebuild to get it) · ⚠️ partial · ❌ not
> done · ⛔ won't-fix (deliberate decision).
>
> **This branch lands three waves of work** (so "what got done" spans §1–§3):
> 1. **§1** — the UX punch-list campaign (`02aeb7f`): zoom focus, `Ctrl-L`, pin
>    toggle, kill-flip, ad-hoc cleanup, the M-series.
> 2. **§2** — the command-mode session (`7256c86`): sticky command mode, focus
>    colours, help wrap, configurable base branch.
> 3. **§3** — the deferred punch-list (`414db3d`): copy-mode, EAW column fix, the
>    "✓ caught up" placeholder, cancel-with-edits wording.

## 0. Build health

- `cargo build` — clean.
- `cargo clippy --all-targets -- -D warnings` — clean.
- `cargo test` — **532 passed, 0 failed**.

If a complaint below reads ✅/🔁 but you still see it, you're on an old binary —
**rebuild** (`cargo install --path lindep`).

---

## 1. Wave 1 — UX punch-list + ad-hoc cleanup ✅ (`02aeb7f`)

The v1.7 UX-audit follow-ups, plus two bugs you reported live (zoom focus, ad-hoc
rows) and a rendering escape hatch. Designed/verified with a multi-agent
fan-out + adversarial review; integrated and tested as one batch.

### 1a. Zoom from the nav bar now focuses the pane it enlarges
Zooming (`Ctrl-A z`) while the Spine was focused painted the agent full-screen
but left focus on the now-hidden Spine, so `j`/`k`/`⏎` went to an off-screen
surface. Zoom-in from the Spine now moves focus into the big pane (a Spine-only
zoom no-ops with a footer instead of blanking). Code: `app.rs` `zoom_toggle`
(mirrors the existing "landing on the Spine clears zoom" rule). +1 test.

### 1b. `Ctrl-L` forces a full repaint — the stray-cell escape hatch
"Text left on screen" is a wide-glyph (double-width) **stagger** in a PTY pane:
ratatui's per-frame diff can't know the terminal desynced, and nothing forced a
clear. `Ctrl-L` now drops the diff baseline (`terminal.clear()` + repaint) via the
global-chord band, so it works even inside a focused chat. Code: `keymap.rs`
`Action::Redraw` + `GLOBAL_DEFAULTS`, `app.rs` `request_redraw`/`take_force_redraw`,
`main.rs` loop. The §3b column fix tightens our own glyph columns; the deeper
per-terminal EAW audit (root cause) is still open (§5). +1 test.

### 1c. Pin is a real toggle (H2) ✅
`Ctrl-A p` on a focused **pinned** coin now **unpins** it (symmetric with the
Spine `p`), keeping the live agent alive and demoting the issue to a follower
preview. `Ctrl-A w` (close) still dismisses non-pinnable windows (Fleet / ad-hoc).
Code: `app.rs` `pin_window` tail split. +2 tests.

### 1d. Destructive confirms are principled (H3) ✅ — your call
Kept the split — kill/discard **confirm**, close/restart/unpin **instant** — and
documented it as a principle (irreversible/data-losing confirms; recoverable is
instant), with **no key moves** (your decision). Code comment on `arm_kill`.

### 1e. Kill flips the coin to its deps face — in place (item 10) ✅
A confirmed kill used to remove a pinned coin's tile entirely. It now flips the
coin to its **deps face where it sits**, for the transient preview and a pinned
tile alike — the window never blanks/vanishes mid-kill. Code: `window.rs`
`flip_issue_to_deps`, `app.rs` `on_kill_confirm_key`. +4 tests.

### 1f. Chat input is always visible (item 3) ✅
A focused chat's grid is **bottom-anchored** (input row pinned to the pane bottom)
whether the grid is taller OR shorter than the pane, with a `Clear` on the top
padding so a shrunk/EXITED grid can't strand the input or ghost old rows. Code:
`ui.rs` `render_agent_window`. +2 tests.

### 1g. "running" vs "working" unified (item 7) ✅
`AgentStatus::Running` reads **WORKING** everywhere (bands/titles/footers/ledger),
and `Spawning` reads one term. Code: `theme.rs`, `ui.rs` `ledger_summary_line` /
`render_ledger`. +1 test.

### 1h. The M-series + D2 ✅
- **M1/M4** — stale "agents roster / `r` toggle" and take-over-key comments
  corrected (`window.rs`, `app.rs`).
- **M2** — `f` is relabeled a **toggle** (not "cycle") and now **persists across a
  project switch** (`keymap.rs`/`app.rs`, `Filter::toggle`). +2 tests.
- **M3/M6** — deps entry is coherent: **`Tab`** is the primary chat↔deps flip
  (reach it inside a chat with the prefixed form), `d` documented as "always lands
  on deps". +1 test.
- **M5** — **Fleet** / **all agents (global)** / **next agent** labels
  disambiguated (`keymap.rs`, `ui.rs`). +1 test.
- **D2** — the `Esc` asymmetry (deps = back; chat = forwarded to the agent) is
  documented, with a clear non-`Esc` "back to Spine" (`Ctrl-A 0`). +3 tests.

### 1i. Ad-hoc (`ask-*`) agents disappear when killed/reaped ✅ — your report
A free agent (not tied to a Linear issue) is grafted as a synthetic, **edgeless**
Spine row. Killing it left a dead, **unremovable** row — `AgentReaped` cleaned up
the fleet/backend but never the graph node. Now the reap removes the node
(`model.rs` `Graph::remove_issue`, which prunes every reference), closes its
window, and re-aims the selection — **synthetic ids only**; a real issue's row
outlives its agent. An adversarial review caught (and we fixed) the single-node
edge case where emptying the Spine re-seeded a ghost preview; `rebuild_order` now
clears a dangling root and `reaim_preview` refuses to seed on a missing node.
Code: `app.rs` `AgentReaped`. +4 tests.

**Skipped:** **H1** (the several launch chords — `Enter` / `Space` / `Ctrl-A
Enter` / `Ctrl-A Space`). You like the redundancy; left as-is. `Ctrl-A c` stays the
**repos** picker (the only way to add a second repo to an agent on a single-repo
project), not a launch alias.

---

## 2. Wave 2 — command mode & base branch ✅ (`7256c86`)

- **Sticky command mode** (`Ctrl-A` arms a mode that chains the window-arrangement
  verbs — zoom/close/layout/restart — with bare keys; drops back out on a focus
  move/pin/flip/launch, `Esc`, a second prefix, or landing on a chat). `app.rs`
  `on_prefix_key` / `verb_chains_in_command_mode`. Note: this re-introduces command
  mode, deliberately removed in v1.7 (ENG-562) — intentional, documented in code.
- **Command-mode amber wash + two-signal colours** — armed = the whole focused
  surface goes amber; at rest **violet = focus**, **green = selection** (kept
  distinct; the unify-them experiment was reverted because it breaks on a focused
  pinned agent). `theme.rs` / `ui.rs`.
- **Help `?` overlay wraps** long descriptions instead of clipping, and the scroll
  range is sized to the real wrapped height so nothing is cut off. `ui.rs`
  `render_help`.
- **Configurable per-project `base_branch`** — a new issue branch forks from a
  freshly-fetched `origin/<base>` instead of local `HEAD`, with a safe fall-through
  (`origin/<base>` → `<base>` → `origin/HEAD` → `HEAD`) so a typo or an absent
  branch never blocks a launch; set in the onboarding wizard or `Ctrl-A o`.
  `worktree.rs` `resolve_base`/`is_valid_base`, `registry.rs`, `workspace.rs`,
  `onboard.rs`. +6 tests.

---

## 3. Wave 3 — deferred punch-list ✅ (`414db3d`)

The small/safe deferred items, plus copy-mode. Two items were scoped for this pass
but are **not yet implemented** — idle backstop and transient-footer expiry — and
stay open in §5.

### 3a. Per-window copy-mode (`Ctrl-a [`) ✅ — was §5's "copy/selection scoped to a pane"
`Ctrl-a [` on a focused chat enters tmux-style copy-mode: scroll the agent's
scrollback (`↑↓`, `PgUp`/`PgDn`, `g`/`G`), `space` to start a line selection, `y`
(or `Enter`) to yank the highlighted lines to the host clipboard via a
dependency-free **OSC52** escape, `Esc`/`q` to exit. A violet border + `⧉ COPY`
chip mark the latched pane, and leaving copy-mode snaps the terminal back to the
live bottom. This is the "real fix" §5 used to describe — it sidesteps the native
rectangular-drag-spans-every-pane problem by selecting within the one focused pane.
Code: `app.rs` (`CopyMode`, `enter_copy_mode`/`handle_copy_key`/`yank_copy_selection`,
`status_text`), `ui.rs` `render_copy_pane`, `keymap.rs` `Action::CopyMode`, `main.rs`
OSC52 drain. +5 tests (handler + render) plus a base64 vector test.

### 3b. EAW column-alignment fix ✅
Status/priority glyph columns render through a fixed-width `cell()` field sharing
one `disp_w()` authority with `truncate()`, so a glyph that measures wide in the
renderer's table can't stagger the KEY/title columns. Byte-identical for today's
one-cell glyphs; a hard invariant for future ones. `ui.rs`. +2 tests. (The *deeper*
per-terminal EAW audit behind the §1b stagger is still open — §5.)

### 3c. "✓ caught up" placeholder (B0a) ✅
An empty NEEDS-YOU band now shows a standing, non-selectable `✓ caught up` row
instead of the section silently vanishing, so clearing your last alert reads as
"you're clear", not "did it lose my agent?". `ui.rs` `render_banded_spine`. +2 tests.

### 3d. Cancel-with-edits wording ✅
The re-config cancel path now says **"edits discarded — configuration unchanged"**
when the draft actually differs (vs a plain "configuration unchanged" no-op), via a
read-only `registry::binding_differs` predicate that shares `write_binding`'s
render-and-compare so `toml_edit` reformatting never reads as a spurious edit.
`registry.rs` / `onboard.rs`. +4 tests.

> **B0b** (the pin/unpin overload) landed in Wave 1 as §1c — `Ctrl-A p` is now a
> real toggle. Together with **B0a** above, both of the audit's "B0" design calls
> are resolved.

---

## 4. Already in the tree 🔁 (reported broken, but the current code handles them)

| # | Your report | Reality |
|---|-------------|---------|
| 1 | Top band header not always visible | A 1-row **sticky header** pins the top band (`ui.rs` `render_banded_spine`). |
| 2 | Pin from navbar should pin the issue's view | `p` (spine) and `Ctrl-A p` (window) share a handler; both graduate the preview into a persistent pinned view (now also a full toggle — see §1c). |
| 5 | Issue summary header should wrap | Already wraps (title sized to wrapped height). |
| 6 | Separator for working/idle bands too | Every populated band gets a divider, symmetrically. |
| 8 | `▶` working/ready share a glyph | Not a collision: READY = `▸`, WORKING band = `◎`, working agent = animated spinner — different glyphs *and* colours, with a regression test. |
| 9 | `r` flips agent/issue view → worse view | The agents-roster + its `r` toggle were removed in v1.7; `r` is only `Ctrl-A r` = restart (stale comments fixed in §1h M1). |

---

## 5. Still open

| # | Item | Status | Notes |
|---|------|--------|-------|
| 4 | Agent goes idle but doesn't show idle | ❌ | `Running→Idle` happens only via Claude's `Stop` hook (`notify.rs`). No quiescence backstop, so a missed/late hook leaves it stuck on WORKING. The intended `QUIESCENCE_FRAMES` (~120 s) timer in `tick_frame` was scoped for Wave 3 but **not yet implemented** — highest-value small next step. |
| — | EAW double-width width audit (deep) | ❌ | §3b aligns *our* glyph columns; the root cause behind the §1b stagger — a per-terminal width-accounting pass that agrees with the terminal's own EAW mode — remains. `Ctrl-L` (§1b) is the pragmatic hatch until then. |
| — | Transient-footer expiry | ❌ | Footer/status lines don't auto-time-out — the render only repaints on events, so a transient message can linger. Intended design: `Footer { text, expires_at }` with a ~3 s TTL reaped in `tick_frame`; the renderer already reads the footer through one `App::status_text()` accessor so it can land behind that. Scoped for Wave 3 but not implemented. |
| — | **Vertical pipeline** (auto-run instructions on agent start) | ❌ | Agents launch as a blank interactive `claude` — no prompt/issue text/skill injected. The furthest-from-done piece of the vision. |
| — | **Horizontal auto-dispatch** | ⚠️ | Manual `Ctrl-A g` batch-to-cap only; no auto-draining background queue. |
| — | Broader staleness policy | ⚠️ | `base_branch` (§2) covers fork-from-fresh; auto-pull of an *existing* branch + an "N behind" chip remain open. |

> **Deliberately skipped (not open):** **H1** — the several launch chords
> (`Enter` / `Space` / `Ctrl-A Enter` / `Ctrl-A Space`) are kept on purpose (you
> like the redundancy; see §1i). Not an oversight.

---

## 6. Suggested order of attack

1. **Idle backstop** (§5 item 4) — small, daily annoyance, self-contained; the one
   piece scoped for Wave 3 that didn't land.
2. **Transient-footer expiry** (§5) — also scoped but unbuilt; `status_text()` is
   already the seam it slots behind.
3. **Vertical pipeline MVP** (§5) — even auto-injecting the issue text unblocks the
   rest of the agent-workflow vision.
4. **Broader staleness policy** then the **horizontal auto-dispatch loop** (§5).
5. **Deep EAW width audit** (§5) — replace the `Ctrl-L` hatch and the §3b column
   guard with a real per-terminal pass.
