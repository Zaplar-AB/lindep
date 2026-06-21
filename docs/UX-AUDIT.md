# lindep — feedback status & work log

> Snapshot: 2026-06-21. Tracks every piece of feedback raised so far, what's
> **done**, what was **already done** (so you may just need to rebuild), and
> what's **still open**. Status key: ✅ done this session · 🔁 already in the
> tree (rebuild to get it) · ⚠️ partial / known bug · ❌ not done · 🔧 in progress.

## 0. Current build health

- `cargo build` — clean.
- `cargo clippy` — clean (`-D warnings`).
- `cargo test` — **494 passed, 0 failed**.

The current tree is healthy. Several early complaints are *already fixed in this
code* (see §2), which strongly suggests the `lindep` you were running is an older
install — **rebuild/reinstall** (`cargo install --path lindep`) to pick them up.

---

## 1. Implemented this session ✅

### 1a. Sticky command mode (the `Ctrl-A` prefix)
The prefix no longer fires one verb and disarms. `Ctrl-A` now **arms a command
mode that stays on**, so you can fire several window-arrangement verbs with bare
keys (`Ctrl-A z w w`) without re-prefixing.

- Chains only the window-arrangement verbs that don't then need the pane's own
  keys: **zoom, close, layout, restart**.
- **Drops back out** on: a focus move, pin, chat/deps flip, dispatch, launch
  (these reposition you to act on a pane), `Esc`, a second prefix (still forwards
  a literal `Ctrl-A`), landing on an **agent chat**, or any non-verb key.
- A non-verb key **exits *and* still does its thing** (`ExitReprocess`) — no
  keystroke is ever silently eaten.
- *Why the split:* lindep's pane navigation (`Enter`, arrows, `h`/`l`/`j`) shares
  keys with the window verbs, so a mode that kept reinterpreting them would hijack
  the keys you need next. The full suite caught this (6 deps tests) before it shipped.
- Code: `app.rs` `on_key` band 1, `on_prefix_key` (returns `CommandStep`),
  `verb_chains_in_command_mode`, `command_mode_preempted`, `focused_is_chat`.
- Note: this **re-introduces** command mode, which was deliberately removed in
  v1.7 (ENG-562) — intentional, documented in code.

### 1b. Command-mode indicator + unified focus colour
While command mode is armed, the **whole focused surface turns amber** (border,
`▌` title bar, selected row, nav chip, and the hint footer) so you always know
you're in it.

- The two on-screen signals are kept **distinct**: **violet = focus** (which pane
  owns the keyboard), **green = selection** (where the row cursor rests). They
  coexist on different panes; only the transient command-mode amber overrides them
  on the one focused surface.
- *History:* you first asked to unify selection colour with the focus border, then
  correctly realized that breaks when you focus a pinned agent (the agent's violet
  border vs the nav's selection) — so that unification was **reverted**. A
  multi-agent colour audit confirmed the two-signal design and caught one extra
  collision: a focused+armed window showing an **EXITED** agent painted the status
  label amber too (two ambers on one frame) — now the label goes graphite while armed.
- Code: `theme.rs` `focus_accent`/`focus_border_style`/`cursor_active(armed)`;
  `ui.rs` `window_block`, `spine_title`, `list_widget`, deps highlight, hint bar,
  EXITED-label fix. Removed the unused `VIOLET_700`.

### 1c. Help `?` overlay wraps
Long binding descriptions used to clip at the right edge. They now **wrap**, and
the scroll range is sized to the real wrapped height so nothing is cut off.
Code: `ui.rs` `render_help`.

**Tests:** +5 new command-mode tests; one stale one-shot test removed; README
prefix section updated.

---

## 2. Already in the tree 🔁 (rebuild to get these)

These were reported as broken but the current code already handles them — likely
an old binary. Verify after a rebuild.

| # | Your report | Reality |
|---|-------------|---------|
| 1 | Top band header (ready/blocked/…) not always visible | A 1-row **sticky header** always shows the top band; offset skips the top divider (`ui.rs` `render_banded_spine`). *Only the topmost* header is pinned, not all. |
| 2 | Pin from navbar should pin the issue's view like the window | `p` (spine) and `Ctrl-A p` (window) are the **same handler**; both graduate the preview coin into a persistent, auto-resuming pinned view (`app.rs` `pin_window`). |
| 5 | Issue summary header should wrap | Already wraps (`Paragraph … Wrap{trim:false}`, title sized to wrapped height). |
| 6 | Separator for working/idle bands too | Every populated band gets a divider, symmetrically (`ui.rs` `render_banded_spine`). |
| 8 | `▶` working/ready share a glyph, differ only by colour | Not a collision: READY = `▸` (violet/ink), WORKING band = `◎` (orange), working agent = animated braille spinner. Different glyphs *and* colours; there's even a regression test. |
| 9 | `r` flips agent/issue view → worse view | The agents-roster + its `r` toggle were **removed in v1.7**; `r` is only `Ctrl-A r` = restart now. Stale *comments* still reference the old toggle (see §5 M1). |

---

## 3. Still open from your UX list ❌ / ⚠️

| # | Item | Status | Notes |
|---|------|--------|-------|
| 4 | Agent goes idle but doesn't show idle | ❌ | `Running→Idle` happens **only** via Claude's `Stop` hook (`notify.rs`). No quiescence backstop, so a missed/late/dropped hook leaves it stuck on WORKING. **Highest-value small fix:** a timer that demotes to Idle after N seconds of no output + no hook. |
| 3 | Chat input box ends up "under" / off-screen | ⚠️ | A bottom-align mitigation exists (`ui.rs` ~885-908) but only when grid rows > pane rows; a failed/skipped resize or an EXITED agent's frozen-tall grid can still hide the input row. |
| 10 | Kill-in-preview should flip the coin, not go to nothing | ⚠️ | Kill destroys the coin + rebuilds the preview; for the *selected* issue it incidentally lands on the deps face (not blank), but a **pinned** coin's tile is removed entirely. Make "flip to deps face on kill" intentional. |
| 7 | Unify "running" vs "working" | ❌ | One state (`AgentStatus::Running`) shows as **WORKING** in bands/titles but **"running"** in footers/ledger; plus `Spawning` = "STARTING"/"resuming…"/"spawning". Terminology cleanup. |

---

## 4. Roadmap reality-check

| Area | Status | Notes |
|------|--------|-------|
| Current version works (render/build) | ✅ | Builds, clippy, 490 tests green. |
| Worktree/git model ("main repos + worktrees, original in .lindep?") | ✅ exists | Exactly the 3-layer model: bare **mirror** (`~/.lindep/mirrors/<h>.git`, the "original" you weren't sure about) → **reference clone** (the "main repo", pushes to true remote) → per-issue **worktree**. |
| Multiple concurrent background agents | ✅ exists | Cap **12** (configurable `[agents] max_concurrent`), workspace-wide, keep running across project switches. |
| Staleness / pull | ⚠️ | Worktrees no longer *have* to fork from stale local HEAD — a project `base_branch` (§6) forks from a freshly fetched `origin/<base>`. Still no auto-pull of an *existing* branch and no "behind" indicator; that broader policy is open. |
| **Vertical pipeline** (instructions auto-run on agent start) | ❌ | Agents launch as a **blank interactive `claude`** — **no prompt, no issue text, no skill** injected. The pipeline engine is marked v3/not-built. This is the furthest-from-done piece of your vision. |
| **Horizontal auto-dispatch** (auto-start agents for READY issues) | ⚠️ | Manual `Ctrl-A g` batch up to the cap only. The auto-draining queue the keymap comment promises **doesn't exist**; no background watcher. |

---

## 5. Additional friction found ("multiple things do the same thing") — not yet fixed

Ranked. None of these are fixed yet (command mode touched the *edges* of H1/H2/H3
but didn't resolve them).

- **H1** — 4–5 chords all launch the selection (`Enter`, `Space`, `Ctrl-A Enter`,
  `Ctrl-A Space`, and `Ctrl-A c`). High.
- **H2** — "pin" is split-brained: spine `p` toggles, but you unpin a focused
  window with `Ctrl-A w` (close), not `Ctrl-A p`. High. *(This is likely why item
  2 still felt off even though pinning works.)*
- **H3** — close / kill / discard / restart / unpin: confusable destructive
  cluster with inconsistent confirms (`w`/`x` adjacent, opposite blast radius;
  kill/discard confirm, close/restart don't). High.
- **M1** — stale "agents roster / `r` toggle" doc comments (`window.rs:60`,
  `app.rs:5`) — ties to item 9. Med.
- **M2** — `f` filter is a 2-state residual but named "cycle"; resets on project
  switch. Med.
- **M3** — `Tab` overloaded (chat eats it → need `Ctrl-A Tab`); flip vs switch-side. Med.
- **M4** — `Enter` means 3 things (dispatch / re-root / re-root-without-attach in
  the global view); a comment mislabels the take-over key as `t` (it's the ledger). Med.
- **M5** — "Fleet" (`g`) vs "global all-agents" (`Ctrl-A a`) vs "next agent"
  (`Ctrl-A j`): three scopes, names don't disambiguate. Med.
- **M6** — `d` and `Tab` both reach the deps face via different paths. Med.
- **D2** — `Esc` opposite on the two faces of one coin (eaten on chat, "back" on deps). Low.

---

## 6. Configurable base branch ✅ (done this session)

Per-project `base_branch` on `[[project]]` — the branch a **new** per-issue
worktree forks from, instead of the hardcoded local `HEAD`. Also closes part of
the staleness gap (§4): a set base forks from a *freshly fetched* ref.

- **Resolution:** unset = `HEAD` (today's behaviour, no network). Set (e.g.
  `develop`) = fork from a just-in-time-fetched `origin/develop`, with a safe
  fall-through `origin/<base>` → `<base>` → `origin/HEAD` → `HEAD` so a typo or a
  branch missing from a given repo never blocks a launch. The fetch happens lazily
  inside `resolve_base`, only on the brand-new-branch path and only when a base is
  set — no network on resumes or for the default.
- **Scope:** brand-new branches only — resumes/recoveries keep their committed
  branch; per repo where the branch exists (a multi-repo project's other repos
  fall through to their own default).
- **Decisions you made:** unset ⇒ `HEAD` (opt-in, zero change for existing
  projects); ad-hoc `ask-*` agents honour the base.
- **Settable** in the setup wizard (new "base branch" step) and `Ctrl-A o`;
  back-compat is clean (serde default/skip — existing `registry.toml` unchanged).
- Code: `worktree.rs` (`resolve_base`, `is_valid_base`, create arm 3),
  `registry.rs` (`ProjectDescriptor`/`ProjectFile`/`ProjectDraft` + loader +
  `write_binding`), `supervisor.rs` (`RepoProvision.base`), `workspace.rs`
  (per-project provision, two materialize sites), `onboard.rs` (wizard step),
  `main.rs`. Verified by a multi-agent audit/design/adversarial pass; +6 tests.

---

## 7. Suggested order of attack

1. **Rebuild your install** — clears §2 (items 1, 2, 5, 6, 8, 9), and picks up the
   command mode, colours, help wrap (§1) and the base branch (§6).
2. **Idle backstop** (§3 item 4) — small, daily annoyance, self-contained.
3. **Command-surface cleanup** (§5 H1/H2/H3) — the core "multiple things do the
   same thing."
4. **Vertical pipeline MVP** (§4) — even auto-injecting the issue text is a big
   step; unblocks the rest of your agent-workflow vision.
5. **Broader staleness policy** (§4) — base branch (§6) covers the fork-from-fresh
   case; the remaining call is auto-pull of an existing branch vs an "N behind"
   chip vs leaving it to the agent.
6. **Horizontal auto-dispatch loop** (§4).
7. Polish: kill-flips-coin (item 10), chat-input edge cases (item 3),
   running/working terminology (item 7), stale-doc purge (M1).
