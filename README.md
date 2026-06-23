# lindep

A terminal **cockpit for running coding agents** across a Linear project's
dependency graph.

Linear lets you mark that one issue *blocks* another, but there's no good way to
see the whole dependency web at a glance. `lindep` pulls a project's issues and
their `blocks` relations and renders them as a clean, navigable graph, banded by
readiness so "what is blocked by what" and "what's ready to pick up" read at a
glance.

From that same view, lindep launches and supervises real `claude` (Claude Code)
agents, usually one per issue, each in its own git worktree and branch, so you
can dispatch and steer a whole fleet without leaving the terminal. The agent
backend is tool-agnostic, so Codex, Aider, and other coding agents are on the
way. Linear stays the source of truth; lindep is the visibility and
orchestration layer on top. With `--demo` it stays a read-only graph viewer.

<img width="1926" height="999" alt="image" src="https://github.com/user-attachments/assets/c6603ce5-01bb-4eed-b01a-73fb113d3ef4" />

## Install

```sh
cargo install --path .        # installs `lindep` to ~/.cargo/bin
# or just run it from the repo:
cargo run --release -- --demo
```

## Use

Provide a Linear personal API key (create one at linear.app/settings/api) in a
`.env` file or as an environment variable:

```sh
cp .env.example .env          # then paste your key into .env
# or:
export LINEAR_API_KEY=lin_api_xxxxxxxx
```

`.env` is loaded from the current directory (or a parent), then from
`~/.config/lindep/.env`; put your key in the latter so the installed `lindep`
works from any directory. Both files are gitignored; an exported variable wins
over either.

```sh
lindep                        # pick a project from an interactive list (recommended)
lindep "Core PMS"             # jump straight to a project (name or unique substring)
lindep --list                 # print every project and exit
lindep --graph "Core PMS"     # open straight into the layered overview
lindep --demo                 # explore a synthetic graph, no key needed
```

Running with no project opens a **searchable picker** (type to filter, arrows
to move, Enter to open), so you never have to quote names with spaces.

The key is sent verbatim in the `Authorization` header (a personal key, **not**
a `Bearer` OAuth token).

## How it reads

The **spine**, the permanent left column, lists every issue in the project as a
**readiness schedule**: one scroll banded top→bottom **NEEDS-YOU · WORKING · IDLE ·
READY · BLOCKED · DONE**, so "what should I look at right now" is always at the
top and the dispatchable work (the **READY** lane, marked with a `▸` rail) reads
at a glance. Within each band, highest-downstream-impact first (then priority).
Press <kbd>d</kbd> to open a
**dependency window**: a
**lens** on the selected issue:

- **Upstream**: its blockers, transitively. What must finish first.
- **Downstream**: what it blocks, transitively. What it's holding up.

Press <kbd>g</kbd> for the **overview** (the Fleet window): a top-down map of
*every* issue laid out in dependency layers (roots with no blockers at L0, flowing
down to leaves), with cycles and external blockers called out. The view scrolls to
keep your selection in sight.

Move the selection on the spine and an open dependency window re-aims instantly.
Focus a dependency window (<kbd>Ctrl-A</kbd> <kbd>→</kbd>) and press
<kbd>Enter</kbd> on any node to *re-root* the lens on it and walk the graph one hop
at a time; <kbd>←</kbd>/<kbd>→</kbd> (or <kbd>h</kbd>/<kbd>l</kbd>) flip between its upstream and downstream trees.

When the **spine** is focused:

| key | action |
|-----|--------|
| <kbd>↑</kbd>/<kbd>↓</kbd> · <kbd>k</kbd>/<kbd>j</kbd> | move the selection |
| <kbd>Enter</kbd> | open / focus an agent on the selection |
| <kbd>d</kbd> | open a dependency window for the selection |
| <kbd>g</kbd> | open the layered top-down overview (Fleet) window |
| <kbd>/</kbd> | fuzzy-find issues by id or title |
| <kbd>f</kbd> | filter: all / has-deps |
| <kbd>c</kbd> | jump through issues that sit on a cycle |
| <kbd>n</kbd> | jump to the next issue whose agent needs you |
| <kbd>i</kbd> | summary card for the selected issue (details + deps) |
| <kbd>t</kbd> | agent run ledger for the selected issue (its session history) |
| <kbd>?</kbd> | help (quit is <kbd>Ctrl-A</kbd> <kbd>q</kbd>, see the cockpit, below) |

When a **dependency window** is focused: <kbd>↑</kbd>/<kbd>↓</kbd> move,
<kbd>←</kbd>/<kbd>→</kbd> (or <kbd>h</kbd>/<kbd>l</kbd>) switch the active tree
(blocked-by ↔ blocks), <kbd>Enter</kbd> re-root onto the selected node,
<kbd>Space</kbd> collapse / expand, <kbd>b</kbd>/<kbd>Backspace</kbd> back to the
previous root. <kbd>Tab</kbd> flips a docked agent window between its chat and
deps faces.
Window management (focus, zoom, pin, close, layout) lives behind the
<kbd>Ctrl-A</kbd> prefix; see the cockpit section.

### What the marks mean

- Status: `●` done · `◐` in progress · `○` todo · `·` backlog · `◇` triage · `⊗` canceled
- Priority: `‼` urgent · `!` high · `◻` medium · `▫` low
- `▸` (left rail) ready to dispatch: unblocked, unresolved, no live agent
- `⊘` blocked (an unresolved blocker) · `↺` on a dependency cycle
- `⇗ … [ext]` a cross-project blocker, shown as a terminal leaf
- `↺ … back-edge` where a tree would loop; `↗ shown above` where a node re-appears

## Multi-agent cockpit

When the active Linear project is registered in `~/.lindep/registry.toml` (see
[Managed workspaces](#managed-workspaces-v16) below), lindep doubles as a
**cockpit that launches and supervises real `claude` (Claude Code) agents**:
usually one per issue, plus optional ad-hoc `ask-*` agents, each in its own git
worktree + branch in a workspace lindep clones and owns (you can run it from
anywhere, not just inside a checkout). Linear stays the
source of truth; lindep is the visibility + orchestration layer (it spawns the
real `claude`, it does not reimplement it). Open a project that isn't registered
yet and lindep walks you through connecting it to a repo (see
[Managed workspaces](#managed-workspaces-v16)); with `--demo` it stays the
read-only graph viewer.

The cockpit is a **tiling window manager**: a horizontal strip of focusable
columns: the permanent **spine** (the readiness-banded issue schedule), live
**agent** PTYs (issue-backed or ad-hoc), and **dependency** windows. The focused window takes every
keystroke; **`Ctrl-A` is the prefix** that escapes to window commands (press it
twice to send a literal `Ctrl-A` through to the agent).

| Key | Action |
|-----|--------|
| <kbd>Enter</kbd> | On the spine: launch / open an agent on the focused issue (its own worktree + branch) |
| <kbd>Ctrl-A</kbd> <kbd>←</kbd>/<kbd>→</kbd> · <kbd>h</kbd>/<kbd>l</kbd> | Focus the window left / right |
| <kbd>Ctrl-A</kbd> <kbd>Enter</kbd> | Open / focus an agent on the selection (from any window) |
| <kbd>Ctrl-A</kbd> <kbd>z</kbd> | Zoom the focused window full-screen (non-destructive) |
| <kbd>Ctrl-A</kbd> <kbd>p</kbd> | Pin / unpin the focused window; pinned windows persist and auto-resume on restart |
| <kbd>Ctrl-A</kbd> <kbd>w</kbd> | Close = undock the focused window (the agent keeps running) |
| <kbd>Ctrl-A</kbd> <kbd>x</kbd> | Kill the focused agent (confirmed) |
| <kbd>Ctrl-A</kbd> <kbd>r</kbd> | Restart the on-screen issue's agent (reclaim a dead one + relaunch) |
| <kbd>Ctrl-A</kbd> <kbd>j</kbd> | Walk to the next live agent |
| <kbd>Ctrl-A</kbd> <kbd>g</kbd> | Dispatch every READY issue up to the concurrency cap |
| <kbd>Ctrl-A</kbd> <kbd>c</kbd> | Launch, choosing which repos the agent spans; opens the repo select even on a single-repo project. <kbd>a</kbd> in the modal adds another repo: pick a registered one this project doesn't yet list, or type a brand-new repo (a remote URL or local path the registry has never seen) to register and pull in (persisted, no restart) |
| <kbd>Ctrl-A</kbd> <kbd>?</kbd> | Start an ad-hoc agent with a throwaway worktree |
| <kbd>Ctrl-A</kbd> <code>&#124;</code> | Pin the layout: rail ⇄ mosaic (otherwise auto, by docked-window count) |
| <kbd>Ctrl-A</kbd> <kbd>s</kbd> | Switch project (opens the project switcher) |
| <kbd>Ctrl-A</kbd> <kbd>o</kbd> | (Re)configure this project; re-open the setup wizard (applies on restart) |
| <kbd>Ctrl-A</kbd> <kbd>a</kbd> | Global all-agents screen: every project's live agents |
| <kbd>Ctrl-A</kbd> <kbd>e</kbd> | Open the focused agent's workspace in your editor |
| <kbd>Ctrl-A</kbd> <kbd>d</kbd> | Discard a finished issue's workspace (push branches + remove worktrees) |
| <kbd>Ctrl-A</kbd> <kbd>m</kbd> | Reclaim disk: free unreferenced mirrors |
| <kbd>Ctrl-A</kbd> <kbd>0</kbd> | Jump focus home to the spine |
| <kbd>Ctrl-L</kbd> | Force a full repaint; clears any stray cell left by a wide-glyph stagger in an agent pane (works from any focus) |
| <kbd>Ctrl-A</kbd> <kbd>q</kbd> | Quit the cockpit |
| <kbd>n</kbd> · <kbd>Ctrl-A</kbd> <kbd>n</kbd> | Jump to the next issue whose agent needs you (works from any focus) |
| <kbd>f</kbd> | Toggle the issue filter (all / has-deps) |
| <kbd>p</kbd> | Pin / unpin the selected issue straight from the spine (toggle, no prefix needed) |
| <kbd>i</kbd> · <kbd>t</kbd> | Summary card · agent run ledger for the selected issue |

Each issue **row** carries its agent's state two ways: a whole-row colour tint
plus a marker in a fixed left gutter: `◌` spawning · `⠋` working (an animated
spinner) · `⚑` needs you (the row breathes) · `◦` idle · `◼` stopped (you
cancelled it) · `✓` done · `✗` failed; the header shows a fleet summary
(`3 agents · 1 needs you`). A **needs-you row shows the agent's *ask* in place of
the title** ("approve Bash: git push", "plan ready for review"), so the top
**NEEDS-YOU** band reads as a live to-do list you can triage without attaching to
a pane; the same ask also rides the detail bar, the <kbd>i</kbd> summary, a
backgrounded agent's rail card, the <kbd>n</kbd> jump, and the all-agents screen.
Live agents float to the top **NEEDS-YOU**,
**WORKING** and **IDLE** bands of the schedule, so the spine *is* the
agents roster; there's no separate tab. **Dispatch** is just <kbd>Enter</kbd> on
a **READY** row (it launches the agent); a **BLOCKED** row is refused with its
blocker named, so you never launch work that can't progress. The Fleet overview
(<kbd>g</kbd>) tints every node by the same readiness, so its READY lane reads as
"what can I dispatch."

Open as many **agent windows** as fit side by side and drive each live `claude`
directly: the focused window gets every keystroke, so answering a prompt or
nudging an agent is just focusing its column and typing. They tile automatically
by count (a **mosaic** for a few, a scrolling **rail** beyond that), or pin a
layout yourself (<kbd>Ctrl-A</kbd> <code>&#124;</code>); zoom one
full-screen (<kbd>Ctrl-A</kbd> <kbd>z</kbd>); **pin** the ones worth keeping
(<kbd>Ctrl-A</kbd> <kbd>p</kbd>): pinned windows and the layout persist to the
project's `~/.lindep/projects/<handle>/cockpit.json` and **auto-resume** on the
next launch (`--no-resume` opts out).

Agents that raise a permission prompt or go idle light up live via Claude
**hooks** posted to a loopback endpoint, no polling. Sessions are durable:
lindep persists each agent's `session_id` and `--resume`s it on relaunch, so the
*process* is disposable but the *conversation* is not. Every agent commit
**auto-pushes** to its branch's remote (post-commit hook → non-blocking push), so
work is never invisible, and a *rejected* push (a diverged or protected remote)
raises a standing `⇡ N unpushed` chip rather than reporting a false success, so
stranded commits are never silently lost. Press <kbd>Ctrl-A</kbd> <kbd>e</kbd> to open the focused
agent's workspace in your editor (`$VISUAL`/`$EDITOR`, else `code`). With `--demo`
lindep is just the graph viewer.

`Ctrl-A ?` starts an **ad-hoc** agent that is not tied to a Linear issue. It gets
a synthetic `ask-*` row, a normal isolated worktree/session, and the same repo
picker as issue launches; when it exits, lindep removes the throwaway worktree
without pushing that branch.

### Managed workspaces (v1.6)

lindep is a **repo-independent workspace manager**: run it from anywhere; it owns
the on-disk location rather than living inside one checkout. A global
`~/.lindep/registry.toml` names every repo you own once (by `handle`) and binds
each Linear project to a set of them:

```toml
[[repo]]                                  # every repo, named once
handle = "lindep"
remote = "git@github.com:zaplar/lindep"   # canonical fetch/push source
local  = "/home/felix/code/lindep"        # OPTIONAL: a read-only --reference alternate

[[project]]                               # a Linear project ↔ a set of repos
id            = "323e926b-…"              # Linear project UUID
handle        = "lindep-core"             # the per-project dir name
candidates    = ["lindep", "shared-proto"] # the trust boundary
primary       = "lindep"                  # always materialised at launch
branch_prefix = "felix"
base_branch   = "develop"                 # OPTIONAL: fork new issue branches from
                                          # a fresh origin/develop (default: HEAD)
```

`base_branch` is the branch each **new** per-issue worktree forks from. Unset, it
keeps the historical behaviour (the clone's local `HEAD`). Set to a name like
`develop` and a brand-new issue branch is cut from a *freshly fetched*
`origin/develop`, with a safe fall-through (`origin/<base>` → `<base>` →
`origin/HEAD` → `HEAD`) so a typo or a branch absent from a given repo never
blocks a launch. It applies only to brand-new branches (resuming or recovering an
issue keeps its existing committed branch) and per repo where the branch exists.

You don't have to write this by hand. The first time you open a project that
isn't in the registry, lindep runs a short **setup wizard**: point it at a local
clone or paste a remote URL, pick the primary repo (advanced fields like
`branch_prefix`, `base_branch` and per-issue scratch datastores are skippable),
and it appends the `[[repo]]`/`[[project]]` blocks for you, leaving any existing
comments intact.
Re-open the wizard any time on a connected project with <kbd>Ctrl-A</kbd>
<kbd>o</kbd> to add a repo or a scratch datastore; the edit is written in place
(your comments preserved) and applies on the next launch.

Opening a project **materialises its isolated workspace** under
`~/.lindep/projects/<handle>/` via a 3-layer git model: a shared bare **mirror**
(`~/.lindep/mirrors/<handle>.git`) → a per-(project,repo) **reference clone** that
borrows the mirror's objects and pushes to the true remote → per-issue
**worktrees**. Two projects can share a repo with no collision (each has its own
ref namespace), and the in-cockpit switcher (<kbd>Ctrl-A</kbd> <kbd>s</kbd>) lists
**every** registered project, cloning one the first time you open it. Backing out
never stops a project's agents: each keeps its own supervised fleet running while
you work in another; a backgrounded project waiting on you shows a `⚑N elsewhere`
badge in the header (and a `⚑` in the switcher).

### Rebinding keys

Every key is remappable from `./.lindep/config.toml` (relative to where you launch
lindep) then `~/.config/lindep/config.toml` (personal wins), the same
two-location pattern as `.env`. Since lindep now runs from anywhere, the personal
file is usually the one to edit. Direct keys (the spine / dependency windows) go
under `[keys]`; the window **verbs** reached behind the prefix go under `[verbs]`;
the prefix chord itself is `prefix`:

```toml
prefix = "ctrl-a"                  # the escape to window commands (default)

[keys]
deps = "D"                         # open a dependency window (default d)
filter = ["f", "ctrl-f"]           # an action may take several keys

[verbs]
kill = "k"                         # Ctrl-A k to kill the focused agent (default x)
close = "ctrl-w"
layout = "|"
open-in-editor = "e"               # Ctrl-A e to open the agent's workspace (default e)
```

Direct-key action names: `move-up` `move-down` `move-top` `move-bottom`
`page-up` `page-down` `switch-side` `enter` `toggle-collapse` `back` `deps`
`fleet` `jump-cycle` `jump-needs-you` `filter` `search` `help` `summary`
`ledger` `context` `pin`. Verb names:
`focus-left` `focus-right` `focus-nav` `zoom` `pin` `close` `kill` `layout`
`open` `quit` `jump-needs-you` `context` `switch-project` `open-in-editor`
`reclaim-mirrors` `discard-workspace` `global-view` `configure-project` `restart`
`next-agent` `dispatch-ready` `choose-repos` `ask` `copy-mode`.

**Copy-mode** (`Ctrl-a [`, tmux-style) on a focused agent chat: scroll its scrollback
(`↑`/`↓`, `PgUp`/`PgDn`, `g`/`G`), press `space` to start a line selection, then `y`
(or `Enter`) to yank the highlighted lines to your clipboard via OSC52; `Esc`/`q` exits.
OSC52 works over SSH/tmux where the host terminal allows clipboard writes.

(The overlay/issue actions, `search` `help` `summary` `ledger`, are direct-only
as of v1.7: the prefix keeps spatial pane verbs. `jump-needs-you` and `context`
appear in both tables: a bare key on the spine, the prefix form from any focus.)

Keys: single chars (`a`, `/`), `f1`–`f12`, the named keys (`enter` `tab` `space`
`up` `down` `left` `right` `home` `end` `pageup` `pagedown` `backspace` `delete`
`insert`), and `ctrl-<letter>` / `alt-<key>`. (`esc` is reserved: it always goes
to the focused window.) Press `?` in the cockpit to see the *live* bindings. Bad
entries (an unknown action, an unparseable or reserved key, or a chord already
taken by another action) warn on stderr at startup and leave that action at its
default.

**The prefix.** Window commands sit behind `Ctrl-A` so they never collide with
`claude`'s own line editing; `Ctrl-<letter>` works on every keyboard and
terminal. Press the prefix twice to send a literal `Ctrl-A` through to the focused
agent, so it's never wholly unreachable. Pick any chord for `prefix`; if it
shadows a direct key or verb, lindep warns at startup.

**Command mode (the sticky prefix).** `Ctrl-A` arms a **command mode** that
*stays on* so you can fire several window-arrangement verbs with bare keys:
`Ctrl-A z w w` instead of re-prefixing each. While it's armed the whole focused
surface turns **amber** (its border, title bar, selected row and the hint footer)
so you always know you're in it. It chains the verbs that rearrange windows
without then needing the pane's own keys: **zoom, close, layout, restart**.
Everything that repositions you to *act* on a pane (a focus move, pin, the
chat/deps flip, dispatch, launching or focusing an agent) drops you back out, as
does landing on an **agent chat**, `Esc`, or simply any navigation/typing key
(which also still does its thing, so a keystroke is never eaten). The split is
deliberate: lindep's pane navigation (`Enter`, arrows, `h`/`l`) shares keys with
the window verbs, so a mode that kept reinterpreting them would hijack the very
keys you need next.

### Agent limits

The supervisor hosts up to **12** live agents at once by default; docking is
uncapped above that: extra docked agents wait as "resuming…" cards until a slot
frees. Override the ceiling in the same `config.toml`:

```toml
[agents]
max_concurrent = 16
```

## Notes

- **Cycles** are detected up front and rendered safely: a back-edge becomes a
  single `↺` leaf instead of an infinite tree. The overview lists every cycle.
- **External** (cross-project) blockers are kept as terminal leaves so the graph
  stays scoped to one project while still showing outside risk.
- `--snapshot [WxH]` renders one frame as plain text (for screenshots / CI).
- **Copy / paste**: lindep doesn't capture the mouse, so use your terminal's normal
  select/copy (often Shift- or Option-drag) to copy issue keys and paths. Pasting a
  multi-line prompt into a focused agent chat is reassembled as one block (bracketed
  paste), not submitted line-by-line.

## Develop

```sh
cargo test          # graph algorithms, navigation, headless render snapshots
cargo clippy
cargo run -- --demo --snapshot 118x32   # see a frame without a terminal
cargo doc --open    # per-module / per-item API docs
```

Modules: `model` (graph + cycle/level algorithms, pure), `linear` (GraphQL
client), `app` (state + input), `ui` (ratatui rendering), `theme` (palette),
`demo` (synthetic graph). Cockpit spine: `event` (tokio runtime + `AppEvent`
channel), `worktree` (one git worktree/branch per issue or `ask-*` id), `session` (durable
per-`(project, issue)` session-id state), `backend` (the `AgentBackend` trait +
PTY-hosted Claude backend), `notify` (Claude hooks → loopback endpoint → events),
`supervisor` (launch / cancel / shutdown of the agent fleet), `keymap`
(remappable bindings from `config.toml`), `window` + `layout` (the tiling window
manager), `picker` (project picker / switcher), `registry` (the global
`~/.lindep/registry.toml` + the `~/.lindep` layout), `mirror` (the 3-layer git
clone substrate: bare mirror → reference clone), `workspace` (one supervised fleet
per project, materialised from the registry and kept alive across switches),
`ledger` (durable per-issue agent run history).
