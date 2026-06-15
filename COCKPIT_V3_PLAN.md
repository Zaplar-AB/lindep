# Cockpit v3 — Implementation Plan

**"Everything is a focusable column."** Replace the cockpit's mode-based shell (single full-screen attach + read-only chat wall + composer + Deps/Chat toggle) with a tmux-style tiling window manager whose panes are live, interactive agents.

Status: v3.0 + v3.1 implemented on `feat/cockpit-v3-windowing`. **v3.2 ("one coin, count-driven layout") agreed 2026-06-15 (grill round 2) — see §7 for the delta; it supersedes the parts of §1–§6 it touches.** Branch baseline: `feat/v1-multi-agent-spine`.

---

## 1. Target model (agreed)

A horizontal strip of **windows**, each a focusable column with a type:

| Type | What it is | Keys when focused |
|---|---|---|
| **Spine** (permanent, index 0) | Issues / Agents roster (keeps `r` tab toggle) | direct keys (no prefix) |
| **Agent** | a live `claude` PTY — old single "attached", now N simultaneous | all keys → the PTY |
| **Deps** | dependency tree; `Issue(key)` = per-issue tree, `Fleet` = the old Overview map | direct keys (no prefix) |

**One input rule:** the focused window gets your keys; **`Ctrl-a` (prefix) is the sole escape** to cockpit commands — identical in agent/deps/spine; press twice → literal `Ctrl-a` to the agent. Generalizes today's `pending_leader` (`Ctrl-a d` detach).

**Verbs (behind prefix):** `←/→` or `h/l` focus · `z` zoom (non-destructive) · `p` pin · `w` close=undock · `x` kill (confirmed) · `|` layout (filmstrip⇄mosaic) · `Enter/Space` attach+spawn button · `q` quit · `/` search · `?` help · `r` roster tab.

**Visuals:** focused = **violet thick/double border**; unfocused = thin border in **status hue** (running-orange, needs-you breathing-amber, idle-cyan…); focused window's own status reads from its **title bar**.

**Pin = persist** (docked, in pin order). Unpinned = transient **preview** of the spine selection; nothing auto-pins; the Agents roster is the refind net. **Close = undock** (agent keeps running). **Kill = `Ctrl-a x`, confirmed, separate.** Restart = persist layout + **auto-resume** (gated).

---

## 2. The verdict that reshaped this plan

Five subsystems were designed in parallel (window-model, layout-engine, input-prefix, visuals, lifecycle). Two adversarial critics (migration-risk + ratatui/PTY-feasibility) independently reached the same conclusions:

> **CONDITIONAL GO with mandatory resequencing.** The supervisor/backend/session control plane needs **zero changes** — N interactive PTYs, per-window resize, `--resume`, and cancel/kill all already exist; v3 is a UI/App/keymap reshape over a proven process layer. **But** the four UI tracks falsely split one tightly-coupled change into four "independently-shippable" ones; they invented three incompatible `Window` types and three different shims for the same missing piece. And two designs are technically unsound as drafted.

### Two hard technical corrections

1. **The filmstrip as drafted cannot render.** `tui_term 0.3.4` paints the **top-left `area.width × area.height` subgrid** of the vt100 parser into a Rect — there is **no horizontal offset into the parser**. So a partially-scrolled-in 80-col column shows the *left* N cols of claude's UI (sliced, not windowed); resizing the parser to fit defeats the ≥80 floor and churns SIGWINCH; and `PseudoTerminal::render` calls `Clear` first, so overlapping Rects erase each other.
   → **Fix: snap-to-whole-column scrolling.** Show only columns that fit *entirely*; never render a partial PTY column (the width-0 DROP rule, generalized to "any column not fully inside the strip is dropped"). This is how tmux window-lists behave anyway, and it eliminates the partial-clip and Clear-overlap problems in one rule.

2. **`auto-resume ALL` is unsound in policy.** Mechanism is fine (`claude --resume` is wired), but it collides with `max_concurrent=6` (the 7th `launch()` silently no-ops) and would re-spawn `Done`/`Failed` conversations.
   → **Fix:** resume only **docked ∩ was-live** sessions (never Done/Failed); eager-resume the focused window + up to `max_concurrent-1` pinned, lazy-resume the rest on first focus; render not-yet-spawned docked windows as **"resuming…" cards** (no parser/resize). **Update (2026-06-15): promoted to default-ON** — the verification preconditions are in place (lazy >cap stagger via `resume_one`'s capacity guard, the None-backend cards, and a per-resume grace bound), so `--no-resume` is now the opt-out, not the default.

### Cross-cutting decisions locked before any code (the "type charter")

- **Sole owner:** a new `src/window.rs` owns `WindowKind`, `DepsRoot`, `Window`, `WindowId`, `LayoutMode`, `WindowSet`. Every other module consumes them. (Kills the 3-way duplicate-type conflict.)
- **`preview_size` key = `WindowId` (monotonic `u64`), one flat `HashMap<WindowId,(u16,u16)>`.** Required for zoom (same issue, two geometries during the toggle frame) and future-proof. Migrate **all 5 drop sites + 2 guards atomically, once** (`ui.rs:134/653` guards; `attach:1141`, `detach:1405`, `AgentExited:844`, `AgentReaped:935` drops). `AgentExited/Reaped` only know the issue → must enumerate windows for that issue to drop the right entries.
- **Scroll lives in `on_key`/`apply_event` + a `Resize` handler, NEVER in `draw`.** Preserves the documented render-mutation contract (only `ListState` offsets + `preview_size` may mutate in render). `draw` reads `scroll_x`; it never writes it.
- **Width is count-INDEPENDENT** (fixed min/pref). Browsing/pinning/scrolling never resizes a live pane — reflow happens **only** on the explicit `|` toggle and on `z` zoom. A lone window **letterboxes** (centered, ≥80 cols) rather than compress-to-fit. (Resolves the n==1 disagreement in favor of no-reflow-on-browse.)
- **`is_chat_visible`/poll-cadence stays allocation-free** and keys off the **post-scroll visible set** (not "docked"). An idle agent scrolled off-screen must NOT pin the loop at 16ms — preserves the idle-quiet/battery property. The **None-backend render path** (text-summary card, never touch parser/resize) must exist **before** `MAX_CHAT_PANES` is uncapped.
- **Auto-resume target set** = persisted docked Agent windows **∩** resumable-after-reconcile sessions; Done/Failed excluded.

---

## 3. Phasing (unified, each phase keeps the build green except Phase 3)

The two critics' phasings are merged. The key insight from feasibility: **land MOSAIC before FILMSTRIP** — mosaic reuses `split_grid` verbatim (zero new tiling code) and proves N interactive PTYs render/resize/forward correctly, sidestepping the tui-term clipping problem entirely. If filmstrip slips, the product still works in mosaic.

### Phase 0 — Type charter (no code behavior)
Write the one-page contract from §2: sole type owner (`src/window.rs`), `preview_size` key = `WindowId`, scroll-in-handler-not-draw, count-independent width, auto-resume set. Ships nothing; makes the rest mergeable.

### Phase 1 — Additive foundation (compiles green, zero behavior change)
- Land canonical types in `src/window.rs` **behind** the existing fields (don't delete `attached`/`mode`/`right_view`/`chat_split` yet).
- `theme.rs`: add `VIOLET_400`/`VIOLET_200`, `focus_border_style()` (steady, frame-free), `window_status_hue()` (lift `chat_pane_chrome` verbatim) — all unreferenced.
- **Re-key `preview_size` → `WindowId` in one atomic commit** across all 5 drop sites + 2 guards, while `attached` still exists (so it's never migrated twice).
- Add latent **None-backend render tolerance** (text-summary card) so uncapping later can't panic.
- *Independently shippable: tree compiles, all existing tests pass, no visible change.*

### Phase 2 — Prefix dispatcher over the existing single-attach (input only)
- Generalize `pending_leader` → `prefix_armed: bool` + 3-state machine; rewrite `on_key` into the **window router** with `focused_window_type()` shimmed to `attached.is_some() ? Agent : Spine`.
- `Ctrl-a w` detach · `Ctrl-a x` kill+confirm (`pending_confirm`) · double-`Ctrl-a` → `0x01` — all proven against the **current single PTY**, reusing working detach-leader code.
- **Atomically:** add `Ctrl-a q` (Quit) **and** demote `on_escape` (Esc → focused window) in the **same commit** (else the app is unquittable).
- **Rewrite `render_hints`/`render_help` remap-driven** (kills the "hints lie" interim); gate not-yet-functional verbs (`Focus*`/`Zoom`/`Pin`/`Layout`) out of help until Phase 4.
- Reconstruct the prefix `KeyEvent` from `keymap.prefix` for the double-tap (`bool` alone loses it).
- *Independently shippable: single-attach still works, now driven through the prefix.*

### Phase 3 — Multi-window MOSAIC (the one real breaking cut, co-sequenced render+input)
The unavoidable coupled change — everything before kept the build green so this is the only risky merge.
- `WindowSet.windows`/`focus` become **source of truth**; delete `attached`/`mode`/`right_view`/`chat_split`/**compose subsystem**.
- `on_key` routes by **real** `windows[focus].kind`; `AgentSpawned` appends+focuses a window; the button merges `attach`+`launch_agent` into `open_or_focus_agent`; **close (`w`) vs kill (`x`) split**; backend-reclaim predicate generalizes `attached||pinned` → "any window references it || pinned" (the late-hook/tombstone guards at `app.rs:865/879/908` stay **untouched** — verified survivable).
- Layout = **mosaic only** (`split_grid` reused verbatim over the window set, behind the per-`WindowId` resize guard).
- Re-point `is_chat_visible`/`has_chat_panes` at an **allocation-free** window-membership predicate; render None-backend cards.
- Deps windows render (`render_focus_panes` for `Issue`, `render_overview` for `Fleet` as Deps bodies).
- **Deps nav (DECIDED — preserve):** port `Enter`-re-root / `Back` history / subtree-collapse into `DepsCursor`, extended to `{side, up, down, root, history, collapsed}` so each Deps window navigates independently.
- *Invalidates the single-attach/compose/chat_panes-cap/pin-cap tests (expected); preserves the reaped/tombstone tests.*

### Phase 4 — FILMSTRIP with snap-to-whole-column scroll
- New pure `src/layout.rs`: `place_windows`, `scroll_offset`, `win_widths` — **unit-tested before any wiring** (like the existing `tail_fit`/`split` tests).
- Filmstrip: spine pinned left (`Length 44`); non-spine windows fixed width `clamp(viewport, 80..=120)`; **only whole columns that fully fit are drawn**, others dropped (no partial PTY columns). Lone window letterboxes.
- `Ctrl-a |` toggles filmstrip⇄mosaic; `Ctrl-a z` non-destructive zoom (save pre-zoom `scroll_x` to restore exact position; zoom follows focus, not a captured index); horizontal scroll keeps the focused column in view (`scroll_offset` called from focus-move verbs + Resize handler).
- Enable `Focus*`/`Zoom`/`Layout` verbs + their help entries.
- `window_block(title, focused, status, exited, frame)` replaces `pane_block`; `BorderType::Double` (1 cell — no inner-geometry change, no extra reflow) + violet when focused, else status hue (needs-you breathes via `needs_you_style`).

### Phase 5 — Persistence (additive, dark-shippable)
- Sibling `.lindep/cockpit.json` (NOT folded into `state.json` — avoids cross-writer contention; render thread is sole writer) via `SessionStore::write_snapshot` discipline + versioned like `Persisted`.
- Persist docked windows (in pin order) + layout mode + focus-ref (by issue/root **identity**, not index). Load+apply before `event_loop`; **prune-on-restore** vs the reconcile survivor set.
- Missing file = today's empty default (= current behavior) → zero risk, no flag.

### Phase 6 — Auto-resume-all (additive, gated `--no-resume` from day one)
- In `start_control_plane`, after `Supervisor::start` + the rehydration loop: resume **docked ∩ was-live** only; eager focused + up to `max_concurrent-1` pinned, **lazy-resume the rest on first focus**.
- "resuming N…" header spinner; `is_animating()` true while any resume is in flight. Implemented as a per-issue `resuming: HashMap<issue, deadline>` (not the originally-planned `AppEvent::Resuming { remaining }` + bare counter): each resume carries its **own** grace deadline so a wedged one self-clears without a trickle of later resumes pushing a shared deadline forward, and so a launch the supervisor no-ops can't pin the loop.
- Titles from `app.graph.get(key)`, fall back to worktree branch (Session has no title).
- **Update (2026-06-15): shipped default-ON** (`--no-resume` opts out) once the >cap stagger + None-backend path were verified together — see §2.2.

---

## 4. Behaviors at risk — explicit calls needed (don't drop silently)

The critics found these tested/real behaviors the parallel designs silently dropped. Each needs a conscious decision:

| Behavior | Today | v3 risk | **Decision** |
|---|---|---|---|
| **Graph re-root / Back / collapse** | `Enter` re-roots the lens, `b` pops history, subtree collapse — tied to spine `root` (4+ tests) | `DepsCursor` as drafted carries only `{side,up,down}` — re-root/history/collapse omitted | ✅ **DECIDED: preserve per-window.** Extend `DepsCursor` to `{side, up, down, root, history, collapsed}` so each Deps window keeps full re-root / Back / collapse independently. Lands in Phase 3. |
| **Over-capacity agents** | `MAX_CHAT_PANES=4` + `PIN_CAP=3` capped it | uncapped docking, but `max_concurrent=6` caps live backends → a docked 7th can't spawn | ✅ **DECIDED: raise the cap.** Bump `max_concurrent` (recommend **6→12**, config-tunable via `.lindep/config.toml`); eager-resume stagger becomes `focused + (cap-1)`. The None-backend card path stays — but only for restart/resuming placeholders, not a routine "at capacity" state. (Practical ceiling is now machine resources + how many ≥80-col columns fit; revisit the number after dogfooding.) |
| **Nudge a background agent without focusing it** | composer (`i`) could message a non-focused pinned agent | removed ("reply = focus + type") | ✅ **DECIDED: remove the composer.** Uniform with the windowing model. Revisit a `Ctrl-a i` quick-send only if the "watch A while nudging B" workflow is actually missed in practice. |
| **Search `/`, filter `f`, sort `s`** | dashboard direct keys | go to the PTY once a non-spine window is focused | **Spine-scoped + global escape** (engineering call): reachable directly when spine focused; `Ctrl-a /` works from any focus. Filter/sort require spine focus (they act on the spine list). |
| **Overview `g` + Esc-to-exit** | toggle map, Esc returns | folded into `Deps{Fleet}`; `g`/Esc gone | New gesture: open a Fleet deps window + `Ctrl-a w` to close. Note the muscle-memory change in help. |
| **One-frame stale content after resize** | n/a (single attach) | claude repaints async on SIGWINCH; our grid resizes sync → 1 stale frame after zoom/toggle | Cosmetic; accept. ~1 frame at 16ms cadence. |

---

## 5. Files touched

- **`src/window.rs`** (new) — sole owner of `WindowKind`/`DepsRoot`/`Window`/`WindowId`/`LayoutMode`/`WindowSet`.
- **`src/layout.rs`** (new, Phase 4) — pure `place_windows`/`scroll_offset`/`win_widths` + `split_1d`/`split_grid` moved here; unit-tested.
- **`src/app.rs`** — replace `focus`/`mode`/`right_view`/`chat_split`/`attached`/`pinned`/`pending_leader` with the window model; re-key `preview_size`; rewrite `on_key`→router + prefix dispatcher; `close_window`/`arm_kill`/`confirm_kill`/`open_or_focus_agent`; generalize `forward_to_agent(issue,key)`; remove compose; `persist_cockpit`/`load_cockpit`; `resuming_count`/`kill_confirm` fields; `is_animating()` extension.
- **`src/ui.rs`** — `render_body` + `place_windows` dispatch; `window_block` replaces `pane_block`; `render_one_chat` keyed by `WindowId`, drop `is_target`; `render_focus_panes`/`render_overview` become Deps bodies; remove `render_attached` short-circuit, `render_chat_panes`, `render_composer`, `chat_wall_title`; `render_kill_confirm`; "resuming N…" header span; remap-driven `render_hints`/`render_help`.
- **`src/keymap.rs`** — `pending_leader`/`detach_seqs` → `prefix: Binding` + `verbs: HashMap<Binding,Action>`; `is_prefix`/`prefix_verb`; new `Action`s (`FocusLeft/Right`, `ZoomToggle`, `PinWindow`, `CloseWindow`, `KillWindow`, `LayoutToggle`, `AttachOrSpawn`); remove v2-only actions; `prefix` config field.
- **`src/theme.rs`** — `VIOLET_400`/`VIOLET_200` + house-rule comment; `focus_border_style()`; `window_status_hue()`.
- **`src/session.rs`** — sibling `CockpitState` + `COCKPIT_VERSION` + load/save via `write_snapshot`; `cockpit_path()`.
- **`src/main.rs`** — load/apply `CockpitState` before `event_loop`; `auto_resume_all` in `start_control_plane`; `--no-resume` CLI flag; `AppEvent::Resuming` handling.
- **`src/event.rs`** — `AppEvent::Resuming { remaining: usize }`.

Control plane (`supervisor.rs`, `backend.rs`, `worktree.rs`, `notify.rs`) — **no changes** (verified).

---

## 6. Test impact

- **Preserve (must not break):** late-hook/tombstone invariants (`a_late_hook_cannot_resurrect_*`, reaped/`is_terminal` guards at `app.rs:865/879/908`); session reconcile; `agent_order` salience sort; the resize geometry-guard discipline.
- **Rewrite (expected):** single-attach, compose, `chat_panes`-cap, `PIN_CAP`, `chat_layout` stack/side/grid cycle, mode-toggle (`g`), `enter_on_a_blocker_re_roots_and_back_returns` (→ per-window), overview centering.
- **Add:** `scroll_offset`/`place_windows` pure unit tests (snap-to-whole-column, lone-window letterbox, focus-keeps-in-view); prefix state-machine tests (verb dispatch, double-tap `0x01`, kill-confirm precedence vs PTY); None-backend card render; persistence round-trip + prune-on-restore.

---

## 7. v3.2 — "one coin, count-driven layout" (grill round 2, 2026-06-15)

A walk-through reframed the model as an **IDE**: the Spine is the file explorer; each
issue is one **coin** (chat ⇄ deps, flipped by `Tab`); pinned coins are the open
editors; the rail is the "Open Editors" overflow that only appears past a threshold;
a running-but-unpinned agent shows **only** as the animated marker on its Spine row.
Five user-agreed decisions, each with its code delta:

1. **One coin window, pinned or not.** Collapse `WindowKind::{Context, Agent,
   Deps(Issue)}` into a single `WindowKind::Coin { issue, mode: CoinMode }`. The
   *unpinned* coin **is** the preview (one, follows the Spine selection); pinning
   flips `pinned=true` **in place** (the PTY + `WindowId` survive). `Deps(Fleet)`
   becomes `WindowKind::Fleet`; `DepsRoot` is deleted. `Tab` flips the **focused**
   coin's face — preview or pinned — building the deps cursor on demand. Graduation
   (`pin_preview`) just sets `pinned`; merge stays (one coin per issue).

2. **Count-driven layout (was the manual `|` toggle).** ≤ `MOSAIC_MAX` (4) non-Spine
   windows → **mosaic** (tiled, all live); more → **rail** (focused coin big + the
   rest as overflow cards). The threshold also caps live PTYs. `Ctrl-a |` becomes a
   sticky session override; auto otherwise. `WindowSet` keeps an *effective* `layout`
   field, refreshed on every structural change via `refresh_layout()`.

3. **Live preview, replaced as you browse.** The single unpinned coin keeps mirroring
   the Spine selection (chat-first when live, else deps). Pin to keep. Unchanged from
   v3.1 except it's now the same kind as a pinned coin.

4. **Preview never a card.** `layout::rail` excludes the preview index from the cards;
   it's drawn only when it's the big/tiled pane (Spine or preview focused). Focused
   into a pinned coin, the preview isn't drawn at all — so an unpinned agent's only
   signal is the Spine-row marker.

5. **A dedicated "home to nav" verb.** `Tab` stays a strict chat⇄deps flip; a new
   `Action::FocusNav` prefix verb (`Ctrl-a g` / `Ctrl-a 0`) jumps focus straight to
   the Spine in one hop, so you never step through the deps pane to get back.

**Persistence format unchanged (v1):** a pinned coin persists by its current face
(`PersistedKind::Agent`=chat, `Deps`=deps, `Fleet`), restoring into a `Coin{mode}` /
`Fleet`. `references_agent` is broadened so a pinned coin keeps its backend alive
across face flips (a deps-face coin can flip back to chat). A pinned coin restored in
chat face auto-resumes (Phase 6); flipping a docked coin to chat lazy-resumes it.
**Control plane: still no changes.**

**Not a bug — the "switching a chat idles it" report (2026-06-15):** investigated and
ruled out. Each agent has its own `pty-read-{issue}` OS thread draining its PTY
continuously, independent of focus/visibility; the visibility gating
(`is_index_visible`) suppresses only *repaints* (idle-quiet), never reads. `Idle` is
set **solely** by claude's own `Stop` hook (notify.rs). So switching never stops an
agent — an off-screen chat just isn't painted live (carded, or the transient preview
vanishes) until you return. v3.2's tiled ≤4 layout makes concurrent live chats
visible, which should dissolve the perception.

---

## 8. v3.2 followups (dogfooding round, 2026-06-15)

Implemented on top of §7 after hands-on use; all green (204 tests, clippy+fmt clean).

1. **Resume self-heal (`supervisor.rs`).** Every relaunch used `claude --resume <id>`
   once a session record existed, which hard-fails "No conversation found" when the
   conversation has been deleted — looping forever, "can't open a new one."
   `supervise()` now wraps spawn→run→grade in a loop: on a resume launch's own
   non-zero self-exit whose screen shows that banner (`missing_conversation`, polled
   ~1 s — false-positive implausible, false-negative degrades to the pre-fix Failed,
   which is relaunchable), it retries ONCE with a fresh `--session-id` (the
   deterministic id recreates the conversation) before grading. The failed attempt is
   never graded Failed, so no tombstone strands the issue. Control plane otherwise
   untouched.

2. **Pinned coin is the active view — no duplicate preview (#2).** `reaim_preview`
   now SUPPRESSES the preview (`WindowSet::clear_preview`) when the selection already
   has a pinned coin, so navigating to a pinned issue surfaces that coin instead of a
   parallel preview. New `App::active_index()` = the selection's pinned coin, else the
   preview; it's the rail's big pane when the Spine is focused. `layout::rail` now
   takes `active_idx` (big pane) **and** `preview_idx` (card exclusion).

3. **Preview no longer vanishes on focus (#3).** `layout::mosaic` tiles **every**
   non-Spine window (dropped the focus-dependent `preview_shown`), so focusing a
   pinned coin keeps the active-view tile on screen. `mosaic_visible` is now width-only.

4. **Needs-you jump lands on the pinned coin (#4).** `Ctrl-a n` to an issue that has
   a pinned coin focuses it on its chat face (`App::focus_pinned_chat`, also backing
   the attach/spawn button).

5. **`reaim_preview` re-aims a *focused* preview.** Dropped the vestigial "skip while
   the preview is focused" guard (in-place deps nav goes through `dispatch_deps`,
   never `reaim_preview`), so a verb-driven jump (`Ctrl-a n`) while the preview is
   focused updates the focused pane, not just the selection. Regression test added.
   *(Bug surfaced by the adversarial review.)*

6. **Issue summary overlay (`i` / `Ctrl-a i`).** A dismissable details card for the
   selected/focused issue — status · priority · assignee · team · blocked/cycle flags,
   plus its direct blockers and blocked work with statuses. Pure local-graph read (no
   Linear fetch); any key closes it. `Action::ToggleSummary`, `App.show_summary`,
   `ui::render_summary`.

**Process note.** The adversarial review's write-capable workflow agents reverted
`src/layout.rs` via `git checkout` (recovered from the session transcript). Future
reviews must sandbox reviewers — read-only `Explore` agentType and/or
`isolation: 'worktree'` — and `git status` should be checked after any review run.
