# lindep — feedback status & work log

> Snapshot: 2026-06-21. Tracks every piece of feedback raised so far — what's
> **done**, what was **already in the tree**, and what's **still open**. Status
> key: ✅ done · 🔁 already in the tree (rebuild to get it) · ⚠️ partial · ❌ not done.
>
> Latest push: commit `02aeb7f` on `felix/ask-dje77vri0if0-0-ad-hoc-agent` — the
> UX punch-list campaign in §1. `cargo test` **518 passed, 0 failed**;
> `cargo clippy --all-targets -- -D warnings` clean.

## 0. Build health

- `cargo build` / `cargo clippy -D warnings` — clean.
- `cargo test` — **518 passed, 0 failed**.

If a complaint below reads ✅/🔁 but you still see it, you're on an old binary —
**rebuild** (`cargo install --path lindep`).

---

## 1. Latest campaign — UX punch-list + ad-hoc cleanup ✅ (this session, `02aeb7f`)

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
`main.rs` loop. The deep EAW width audit (root cause) is still open (§4). +1 test.

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

## 2. Earlier this session 🔁 (already committed — rebuild to get these)

Committed in `7256c86`, documented in code:

- **Sticky command mode** (`Ctrl-A` arms a mode that chains the window-arrangement
  verbs — zoom/close/layout/restart — with bare keys; drops back out on a focus
  move/pin/flip/launch, `Esc`, a second prefix, or landing on a chat). `app.rs`
  `on_prefix_key` / `verb_chains_in_command_mode`.
- **Command-mode amber wash + two-signal colours** — armed = the whole focused
  surface goes amber; at rest **violet = focus**, **green = selection** (kept
  distinct; the unify-them experiment was reverted because it breaks on a focused
  pinned agent). `theme.rs` / `ui.rs`.
- **Help `?` overlay wraps** long descriptions instead of clipping.
- **Configurable per-project `base_branch`** — a new issue branch forks from a
  freshly-fetched `origin/<base>` instead of local `HEAD`, with a safe fall-through;
  set in the wizard or `Ctrl-A o`. `worktree.rs` `resolve_base`.

---

## 3. Already in the tree 🔁 (reported broken, but the current code handles them)

| # | Your report | Reality |
|---|-------------|---------|
| 1 | Top band header not always visible | A 1-row **sticky header** pins the top band (`ui.rs` `render_banded_spine`). |
| 2 | Pin from navbar should pin the issue's view | `p` (spine) and `Ctrl-A p` (window) share a handler; both graduate the preview into a persistent pinned view (now also a full toggle — see §1c). |
| 5 | Issue summary header should wrap | Already wraps (title sized to wrapped height). |
| 6 | Separator for working/idle bands too | Every populated band gets a divider, symmetrically. |
| 8 | `▶` working/ready share a glyph | Not a collision: READY = `▸`, WORKING band = `◎`, working agent = animated spinner — different glyphs *and* colours, with a regression test. |
| 9 | `r` flips agent/issue view → worse view | The agents-roster + its `r` toggle were removed in v1.7; `r` is only `Ctrl-A r` = restart (stale comments fixed in §1h M1). |

---

## 4. Still open

| # | Item | Status | Notes |
|---|------|--------|-------|
| 4 | Agent goes idle but doesn't show idle | ❌ | `Running→Idle` happens only via Claude's `Stop` hook (`notify.rs`). No quiescence backstop, so a missed/late hook leaves it stuck on WORKING. **Highest-value small fix:** a timer demoting to Idle after N s of no output + no hook. |
| — | **Copy/selection scoped to the focused window** | ❌ (new) | You want a drag-select to grab only the focused pane. Today the app **doesn't capture the mouse** (`ratatui::init` = raw + alt screen + bracketed paste, *not* mouse capture), so your **terminal** selects — and a rectangular drag spans the side-by-side rail/mosaic columns, grabbing every pane. **Now:** zoom the pane (`Ctrl-A z`) so it fills the screen (native selection then covers only it — §1a helps); or use **Alt/Option+drag** (block selection) to isolate one column-pane. **Real fix (copy-mode):** capture the mouse, track a drag scoped to the focused rect, highlight it, write to the clipboard via **OSC 52**. Buildable, but a real feature: while active it disables the terminal's native selection + scrollback (so it'd be a toggle/mode), and the `claude` PTY panes need a mouse-forwarding decision. |
| — | EAW double-width width audit | ❌ | The root cause behind the §1b stagger — a per-terminal width-accounting pass. `Ctrl-L` is the pragmatic hatch until then. |
| — | **Vertical pipeline** (auto-run instructions on agent start) | ❌ | Agents launch as a blank interactive `claude` — no prompt/issue text/skill injected. The furthest-from-done piece of the vision. |
| — | **Horizontal auto-dispatch** | ⚠️ | Manual `Ctrl-A g` batch-to-cap only; no auto-draining background queue. |
| — | Broader staleness policy | ⚠️ | `base_branch` (§2) covers fork-from-fresh; auto-pull of an *existing* branch + an "N behind" chip remain open. |
| — | Transient-footer expiry | ❌ | Footer/status lines don't auto-time-out — the render only repaints on events, so a transient message can linger until the next redraw. (Deferred from the v1.7 audit, Part C.) |
| — | Cancel-with-edits wording | ❌ | The confirm wording when cancelling an action that has unsaved edits. (Deferred from the v1.7 audit, Part C.) |
| — | B0a / B0b design calls | ❌ | Two UI design decisions punted in the v1.7 audit (Part C); specifics need a re-read of the audit notes. |

> **Deliberately skipped (not open):** **H1** — the several launch chords
> (`Enter` / `Space` / `Ctrl-A Enter` / `Ctrl-A Space`) are kept on purpose (you
> like the redundancy; see §1i). Not an oversight.

---

## 5. Suggested order of attack

1. **Idle backstop** (§4 item 4) — small, daily annoyance, self-contained.
2. **Copy-mode** (§4) — if per-window copy matters day-to-day; decide the
   native-selection trade-off first.
3. **Vertical pipeline MVP** (§4) — even auto-injecting the issue text unblocks
   the rest of the agent-workflow vision.
4. **Broader staleness policy** then the **horizontal auto-dispatch loop** (§4).
5. EAW width audit (§4) — replace the `Ctrl-L` hatch with a real fix.
