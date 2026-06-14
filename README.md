# lindep

Draw Linear issue dependencies in the terminal — an interactive TUI for seeing
**what is blocked by what** across a project.

Linear lets you mark that one issue *blocks* another, but there's no good way to
see the whole dependency web at a glance. `lindep` pulls a project's issues and
their `blocks` relations and renders them as a clean, navigable graph.

```
  lindep · Inference Platform  12 issues · 12 edges · 1 cycles ↺          filter:all sort:blocked
┌ ISSUES 11 ───────────────────────────────┐┌ ◆ ZAP-204  Streaming token API ───────────────────────┐
│▸ ◐ ▲ ZAP-204  Streaming token API ⊘ ↺    ││▲ UPSTREAM · must finish first (4 direct · 8 total)     │
│  ◐ ▲ ZAP-201  GPU pool autoscaler ⊘      ││  ├─ ◐ ◦ ZAP-188  gRPC transport upgrade                │
│  ○ ▲ ZAP-212  Multi-region failover ⊘ ↺  ││  │  └─ ● △ ZAP-150  Protobuf schema freeze             │
│  ◐ △ ZAP-240  Token usage metering ⊘ ↺   ││  ├─ ◐ ▲ ZAP-201  GPU pool autoscaler                   │
│  ◐   ZAP-205  SSE backpressure ⊘         ││  │  └─ ⇗ INFRA-77 Terraform GPU module [ext]           │
│  …                                       ││  └─ ○ ▲ ZAP-212  Multi-region failover ↺               │
│                                          ││▼ DOWNSTREAM · this unblocks (3 direct · 7 total)       │
│                                          ││  ├─ ◐   ZAP-205  SSE backpressure                      │
│                                          ││  └─ ◐ △ ZAP-240  Token usage metering ↺                │
└──────────────────────────────────────────┘└────────────────────────────────────────────────────────┘
 ◐ ZAP-204 In Progress · @r.okafor · blocks 3 (↓7) · blocked-by 4 (↑8) · ⊘ blocked · ↺ in cycle
```

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

The left pane lists every issue in the project, sorted **ready-first** by default
— unblocked, ready-to-start work on top (press <kbd>s</kbd> to re-sort). The right
pane is a **lens** on the selected issue:

- **Upstream** — its blockers, transitively. What must finish first.
- **Downstream** — what it blocks, transitively. What it's holding up.

Press <kbd>g</kbd> for the **overview**: a top-down map of *every* issue laid out
in dependency layers (roots with no blockers at L0, flowing down to leaves), with
cycles and external blockers called out. The view scrolls to keep your selection
in sight.

Move the selection and the lens re-aims instantly. Press <kbd>Enter</kbd> on any
node in a tree to *re-root* the lens on it and walk the graph one hop at a time.

| key | action |
|-----|--------|
| <kbd>↑</kbd>/<kbd>↓</kbd> · <kbd>k</kbd>/<kbd>j</kbd> | move within the active pane |
| <kbd>←</kbd>/<kbd>→</kbd> · <kbd>h</kbd>/<kbd>l</kbd> · <kbd>Tab</kbd> | switch pane (list ↔ upstream ↔ downstream) |
| <kbd>Enter</kbd> | focus the list → trees; on a tree node, re-root the lens |
| <kbd>b</kbd> / <kbd>Backspace</kbd> | back to the previously focused issue |
| <kbd>Space</kbd> | collapse / expand the selected subtree |
| <kbd>/</kbd> | fuzzy-find issues by id or title |
| <kbd>f</kbd> | filter: all / blocked / has-deps |
| <kbd>s</kbd> | sort: ready / blocked / status / priority / id |
| <kbd>c</kbd> | jump through issues that sit on a cycle |
| <kbd>g</kbd> | toggle the layered top-down overview (every issue, roots → leaves) |
| <kbd>?</kbd> | help · <kbd>q</kbd>/<kbd>Esc</kbd> quit |

### What the marks mean

- Status: `●` done · `◐` in progress · `○` todo/backlog · `◇` triage · `⊘` canceled
- Priority: `▲` urgent · `△` high · `◦` medium · `▽` low
- `⊘` blocked (an unresolved blocker) · `↺` on a dependency cycle
- `⇗ … [ext]` a cross-project blocker, shown as a terminal leaf
- `↺ … back-edge` where a tree would loop; `↗ shown above` where a node re-appears

## Multi-agent cockpit

Inside a git repo, lindep doubles as a **cockpit that launches and supervises
real `claude` (Claude Code) agents** — one per issue, each in its own git
worktree + branch. Linear stays the source of truth; lindep is the visibility +
orchestration layer (it spawns the real `claude`, it does not reimplement it).

| Key | Action |
|-----|--------|
| `a` | Launch a Claude agent on the focused issue (its own worktree + branch) |
| `t` | Attach to that agent's live terminal — every key goes to the agent |
| `F10` | Detach back to the dashboard (the agent keeps running) |
| `x` | Stop the agent on the focused issue |
| `n` | Jump to the next issue whose agent needs you |

Each issue node is annotated with its agent's state: `◌` spawning · `▸` running ·
`⚑` needs you · `◦` idle · `✓` done · `✗` failed; the header shows a fleet
summary (`3 agents · 1 needs you`). Agents that raise a permission prompt or go
idle light up live via Claude **hooks** posted to a loopback endpoint — no
polling. Sessions are durable: lindep persists each agent's `session_id` and
`--resume`s it on relaunch, so the *process* is disposable but the *conversation*
is not. All cockpit state lives under `.lindep/` (gitignored); outside a git
repo (or with `--demo`) lindep is just the graph viewer.

### Rebinding keys

Every key above (and the graph-navigation keys) is remappable in a `[keys]`
table, read from `<repo>/.lindep/config.toml` then `~/.config/lindep/config.toml`
(personal wins) — the same two-location pattern as `.env`. Useful when a
function key is missing or your terminal grabs it:

```toml
[keys]
detach = "f8"                      # default f10; the key that varies by keyboard
# detach = "ctrl-a d"              # …or a tmux-style leader sequence (see below)
launch-agent = "a"
jump-needs-you = ["n", "ctrl-g"]   # an action may take several keys
```

Keys: single chars (`a`, `/`), `f1`–`f12`, the named keys (`enter` `tab` `space`
`up` `down` `left` `right` `home` `end` `pageup` `pagedown` `backspace` `delete`
`insert`), and `ctrl-<letter>` / `alt-<key>`. (`esc` is reserved.) Press `?` in
the cockpit to see the *live* bindings; the attach pane always shows the current
detach key. Bad entries warn on stderr at startup and fall back to the default.

**Leader sequences for detach.** A value with a space — e.g. `detach = "ctrl-a d"`
— is a tmux-style leader: press the leader (`Ctrl-A`), then the next key (`d`).
This is the robust choice when no function key is available, since `Ctrl-<letter>`
works on every keyboard and terminal. Pressing the leader twice while attached
sends it through to the agent, so it's never wholly unreachable. Only `detach`
can be a sequence — that's the one place a key must be reserved from the agent.

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
issue↔session-id state), `backend` (the `AgentBackend` trait + PTY-hosted Claude
backend), `notify` (Claude hooks → loopback endpoint → events), `supervisor`
(launch / cancel / shutdown of the agent fleet), `keymap` (remappable bindings
from `config.toml`).
