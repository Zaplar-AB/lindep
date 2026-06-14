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
```

Modules: `model` (graph + cycle/level algorithms, pure), `linear` (GraphQL
client), `app` (state + input), `ui` (ratatui rendering), `theme` (palette),
`demo` (synthetic graph).
