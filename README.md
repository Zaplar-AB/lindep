# lindep

Draw Linear issue dependencies in the terminal — an interactive TUI for seeing
**what is blocked by what** across a project.

Linear lets you mark that one issue *blocks* another, but there's no good way to
see the whole dependency web at a glance. `lindep` pulls a project's issues and
their `blocks` relations and renders them as a clean, navigable graph.

<img width="1277" height="737" alt="image" src="https://github.com/user-attachments/assets/382ae598-8cec-4cce-b35a-77cdffbdd4a4" />


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
`~/.config/lindep/.env` — put your key in the latter so the installed `lindep`
works from any directory. Both files are gitignored; an exported variable wins
over either.

```sh
lindep                        # pick a project from an interactive list (recommended)
lindep "Core PMS"             # jump straight to a project (name or unique substring)
lindep --list                 # print every project and exit
lindep --graph "Core PMS"     # open straight into the layered overview
lindep --demo                 # explore a synthetic graph, no key needed
```

Running with no project opens a **searchable picker** — type to filter, arrows
to move, Enter to open — so you never have to quote names with spaces.

The key is sent verbatim in the `Authorization` header (a personal key, **not**
a `Bearer` OAuth token).

## How it reads

The **spine** — the permanent left column — lists every issue in the project,
sorted **ready-first** by default — unblocked, ready-to-start work on top (press
<kbd>s</kbd> to re-sort). Press <kbd>d</kbd> to open a **dependency window**: a
**lens** on the selected issue:

- **Upstream** — its blockers, transitively. What must finish first.
- **Downstream** — what it blocks, transitively. What it's holding up.

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
| <kbd>f</kbd> | filter: all / blocked / has-deps |
| <kbd>s</kbd> | sort: ready / blocked / status / priority / id |
| <kbd>c</kbd> | jump through issues that sit on a cycle |
| <kbd>r</kbd> | flip the spine: issue list ↔ agents roster |
| <kbd>i</kbd> | summary card for the selected issue (details + deps) |
| <kbd>t</kbd> | agent run ledger for the selected issue (its session history) |
| <kbd>?</kbd> | help (quit is <kbd>Ctrl-A</kbd> <kbd>q</kbd> — see the cockpit, below) |

When a **dependency window** is focused: <kbd>↑</kbd>/<kbd>↓</kbd> move,
<kbd>←</kbd>/<kbd>→</kbd> (or <kbd>h</kbd>/<kbd>l</kbd>) switch the active tree
(upstream ↔ downstream), <kbd>Enter</kbd> re-root onto the selected node,
<kbd>Space</kbd> collapse / expand, <kbd>b</kbd>/<kbd>Backspace</kbd> back to the
previous root. <kbd>Tab</kbd> flips a docked agent window between its chat and
deps faces.
Window management (focus, zoom, pin, close, layout) lives behind the
<kbd>Ctrl-A</kbd> prefix — see the cockpit section.

### What the marks mean

- Status: `●` done · `◐` in progress · `○` todo/backlog · `◇` triage · `⊘` canceled
- Priority: `▲` urgent · `△` high · `◦` medium · `▽` low
- `⊘` blocked (an unresolved blocker) · `↺` on a dependency cycle
- `⇗ … [ext]` a cross-project blocker, shown as a terminal leaf
- `↺ … back-edge` where a tree would loop; `↗ shown above` where a node re-appears

## Multi-agent cockpit

When the active Linear project is registered in `~/.lindep/registry.toml` (see
[Managed workspaces](#managed-workspaces-v16) below), lindep doubles as a
**cockpit that launches and supervises real `claude` (Claude Code) agents** — one
per issue, each in its own git worktree + branch, in a workspace lindep clones and
owns (you can run it from anywhere, not just inside a checkout). Linear stays the
source of truth; lindep is the visibility + orchestration layer (it spawns the
real `claude`, it does not reimplement it). Open a project that isn't registered
yet and lindep walks you through connecting it to a repo (see
[Managed workspaces](#managed-workspaces-v16)); with `--demo` it stays the
read-only graph viewer.

The cockpit is a **tiling window manager**: a horizontal strip of focusable
columns — the permanent **spine** (issue list / agents roster), live **agent**
PTYs (one per issue), and **dependency** windows. The focused window takes every
keystroke; **`Ctrl-A` is the prefix** that escapes to window commands (press it
twice to send a literal `Ctrl-A` through to the agent).

| Key | Action |
|-----|--------|
| <kbd>Enter</kbd> | On the spine: launch / open an agent on the focused issue (its own worktree + branch) |
| <kbd>Ctrl-A</kbd> <kbd>←</kbd>/<kbd>→</kbd> · <kbd>h</kbd>/<kbd>l</kbd> | Focus the window left / right |
| <kbd>Ctrl-A</kbd> <kbd>Enter</kbd> | Open / focus an agent on the selection (from any window) |
| <kbd>Ctrl-A</kbd> <kbd>z</kbd> | Zoom the focused window full-screen (non-destructive) |
| <kbd>Ctrl-A</kbd> <kbd>p</kbd> | Pin / unpin the focused window — pinned windows persist and auto-resume on restart |
| <kbd>Ctrl-A</kbd> <kbd>w</kbd> | Close = undock the focused window (the agent keeps running) |
| <kbd>Ctrl-A</kbd> <kbd>x</kbd> | Kill the focused agent (confirmed) |
| <kbd>Ctrl-A</kbd> <code>&#124;</code> | Pin the layout: rail ⇄ mosaic (otherwise auto, by docked-window count) |
| <kbd>Ctrl-A</kbd> <kbd>s</kbd> | Switch project (opens the project switcher) |
| <kbd>Ctrl-A</kbd> <kbd>o</kbd> | (Re)configure this project — re-open the setup wizard (applies on restart) |
| <kbd>Ctrl-A</kbd> <kbd>a</kbd> | Global all-agents screen — every project's live agents |
| <kbd>Ctrl-A</kbd> <kbd>e</kbd> | Open the focused agent's workspace in your editor |
| <kbd>Ctrl-A</kbd> <kbd>d</kbd> | Discard a finished issue's workspace (push branches + remove worktrees) |
| <kbd>Ctrl-A</kbd> <kbd>m</kbd> | Reclaim disk — free unreferenced mirrors |
| <kbd>Ctrl-A</kbd> <kbd>g</kbd> | Jump focus home to the spine |
| <kbd>Ctrl-A</kbd> <kbd>q</kbd> | Quit the cockpit |
| <kbd>n</kbd> | Jump to the next issue whose agent needs you |
| <kbd>r</kbd> | Flip the spine: issue list ↔ agents roster |
| <kbd>i</kbd> · <kbd>t</kbd> | Summary card · agent run ledger for the selected issue |

Each issue **row** carries its agent's state two ways: a whole-row colour tint
plus a marker in a fixed left gutter — `◌` spawning · `⠙` running (an animated spinner) · `⚑` needs you (the row
breathes) · `◦` idle · `✓` done · `✗` failed; the header shows a fleet summary
(`3 agents · 1 needs you`). The **agents roster** (`r`) is a salience-sorted tab
on the spine — needs-you first, then live work, then idle, then finished — so
triage is one glance.

Open as many **agent windows** as fit side by side and drive each live `claude`
directly — the focused window gets every keystroke, so answering a prompt or
nudging an agent is just focusing its column and typing. They tile automatically
by count — a **mosaic** for a few, a scrolling **rail** beyond that — or pin a
layout yourself (<kbd>Ctrl-A</kbd> <code>&#124;</code>); zoom one
full-screen (<kbd>Ctrl-A</kbd> <kbd>z</kbd>); **pin** the ones worth keeping
(<kbd>Ctrl-A</kbd> <kbd>p</kbd>) — pinned windows and the layout persist to the
project's `~/.lindep/projects/<handle>/cockpit.json` and **auto-resume** on the
next launch (`--no-resume` opts out).

Agents that raise a permission prompt or go idle light up live via Claude
**hooks** posted to a loopback endpoint — no polling. Sessions are durable:
lindep persists each agent's `session_id` and `--resume`s it on relaunch, so the
*process* is disposable but the *conversation* is not. Every agent commit
**auto-pushes** to its branch's remote (post-commit hook → non-blocking push), so
work is never invisible. Press <kbd>Ctrl-A</kbd> <kbd>e</kbd> to open the focused
agent's workspace in your editor (`$VISUAL`/`$EDITOR`, else `code`). With `--demo`
lindep is just the graph viewer.

### Managed workspaces (v1.6)

lindep is a **repo-independent workspace manager**: run it from anywhere — it owns
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
```

You don't have to write this by hand. The first time you open a project that
isn't in the registry, lindep runs a short **setup wizard** — point it at a local
clone or paste a remote URL, pick the primary repo (advanced fields like
`branch_prefix` and per-issue scratch datastores are skippable) — and it appends
the `[[repo]]`/`[[project]]` blocks for you, leaving any existing comments intact.
Re-open the wizard any time on a connected project with <kbd>Ctrl-A</kbd>
<kbd>o</kbd> to add a repo or a scratch datastore; the edit is written in place
(your comments preserved) and applies on the next launch.

Opening a project **materialises its isolated workspace** under
`~/.lindep/projects/<handle>/` via a 3-layer git model — a shared bare **mirror**
(`~/.lindep/mirrors/<handle>.git`) → a per-(project,repo) **reference clone** that
borrows the mirror's objects and pushes to the true remote → per-issue
**worktrees**. Two projects can share a repo with no collision (each has its own
ref namespace), and the in-cockpit switcher (<kbd>Ctrl-A</kbd> <kbd>s</kbd>) lists
**every** registered project, cloning one the first time you open it. Backing out
never stops a project's agents — each keeps its own supervised fleet running while
you work in another; a backgrounded project waiting on you shows a `⚑N elsewhere`
badge in the header (and a `⚑` in the switcher).

### Rebinding keys

Every key is remappable from `./.lindep/config.toml` (relative to where you launch
lindep) then `~/.config/lindep/config.toml` (personal wins) — the same
two-location pattern as `.env`. Since lindep now runs from anywhere, the personal
file is usually the one to edit. Direct keys (the spine / dependency windows) go
under `[keys]`; the window **verbs** reached behind the prefix go under `[verbs]`;
the prefix chord itself is `prefix`:

```toml
prefix = "ctrl-a"                  # the escape to window commands (default)

[keys]
deps = "D"                         # open a dependency window (default d)
filter = ["f", "ctrl-f"]           # an action may take several keys
sort = "s"

[verbs]
kill = "k"                         # Ctrl-A k to kill the focused agent (default x)
close = "ctrl-w"
layout = "|"
open-in-editor = "e"               # Ctrl-A e to open the agent's workspace (default e)
```

Direct-key action names: `move-up` `move-down` `switch-side` `enter`
`toggle-collapse` `back` `deps` `fleet` `jump-cycle` `jump-needs-you` `agents`
`filter` `sort` `search` `help` `summary` `ledger` `context`. Verb names:
`focus-left` `focus-right` `focus-nav` `zoom` `pin` `close` `kill` `layout`
`open` `quit` `search` `help` `roster` `jump-needs-you` `summary` `ledger`
`context` `switch-project` `open-in-editor` `reclaim-mirrors` `discard-workspace`
`global-view` `configure-project` `command-mode`.

Keys: single chars (`a`, `/`), `f1`–`f12`, the named keys (`enter` `tab` `space`
`up` `down` `left` `right` `home` `end` `pageup` `pagedown` `backspace` `delete`
`insert`), and `ctrl-<letter>` / `alt-<key>`. (`esc` is reserved — it always goes
to the focused window.) Press `?` in the cockpit to see the *live* bindings. Bad
entries — an unknown action, an unparseable or reserved key, or a chord already
taken by another action — warn on stderr at startup and leave that action at its
default.

**The prefix.** Window commands sit behind `Ctrl-A` so they never collide with
`claude`'s own line editing — `Ctrl-<letter>` works on every keyboard and
terminal. Press the prefix twice to send a literal `Ctrl-A` through to the focused
agent, so it's never wholly unreachable. Pick any chord for `prefix`; if it
shadows a direct key or verb, lindep warns at startup.

### Agent limits

The supervisor hosts up to **12** live agents at once by default; docking is
uncapped above that — extra docked agents wait as "resuming…" cards until a slot
frees. Override the ceiling in the same `config.toml`:

```toml
[agents]
max_concurrent = 16
```

## Notes

- **Cycles** are detected up front and rendered safely — a back-edge becomes a
  single `↺` leaf instead of an infinite tree. The overview lists every cycle.
- **External** (cross-project) blockers are kept as terminal leaves so the graph
  stays scoped to one project while still showing outside risk.
- `--snapshot [WxH]` renders one frame as plain text (for screenshots / CI).

## Develop

```sh
cargo test          # graph algorithms, navigation, headless render snapshots
cargo clippy
cargo run -- --demo --snapshot 118x32   # see a frame without a terminal
cargo doc --open    # per-module / per-item API docs
```

For how the cockpit fits together — the six layers, the concurrency model, the
agent lifecycle, the `.lindep/` state layout and the operating guide — see
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md). The tui-term feasibility verdict
is in [`docs/ENG-392-pty-spike-verdict.md`](docs/ENG-392-pty-spike-verdict.md).

Modules: `model` (graph + cycle/level algorithms, pure), `linear` (GraphQL
client), `app` (state + input), `ui` (ratatui rendering), `theme` (palette),
`demo` (synthetic graph). Cockpit spine: `event` (tokio runtime + `AppEvent`
channel), `worktree` (one git worktree/branch per issue), `session` (durable
per-`(project, issue)` session-id state), `backend` (the `AgentBackend` trait +
PTY-hosted Claude backend), `notify` (Claude hooks → loopback endpoint → events),
`supervisor` (launch / cancel / shutdown of the agent fleet), `keymap`
(remappable bindings from `config.toml`), `window` + `layout` (the tiling window
manager), `picker` (project picker / switcher), `registry` (the global
`~/.lindep/registry.toml` + the `~/.lindep` layout), `mirror` (the 3-layer git
clone substrate: bare mirror → reference clone), `workspace` (one supervised fleet
per project, materialised from the registry and kept alive across switches),
`ledger` (durable per-issue agent run history).
