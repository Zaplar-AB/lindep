# lindep architecture — the v1 multi-agent spine

This is the connective-tissue doc: how the pieces fit, where work runs, how data
flows, and how to operate the cockpit. Per-module and per-item details live in
the source as `//!` / `///` docs (`cargo doc --open` renders them). The original
vision + locked decisions live in the Linear design doc *"lindep — Architecture &
Build Plan"*; the tui-term feasibility verdict is in
[`ENG-392-pty-spike-verdict.md`](ENG-392-pty-spike-verdict.md).

## What lindep is

**Linear is the source of truth for planning. lindep is the visibility +
orchestration layer.** The existing dependency-graph TUI is now a *cockpit* that
launches and supervises fleets of real `claude` (Claude Code) agents — each
anchored to a Linear issue, each in its own git worktree. We **spawn the real
`claude`**; we do not reimplement it.

Inside a git repo lindep is the cockpit; with `--demo` or outside a repo it
degrades cleanly to the read-only graph viewer.

## The six layers → modules

| Layer | Module(s) | Responsibility |
|-------|-----------|----------------|
| 1. Cockpit (TUI) | `app`, `ui`, `theme`, `keymap` | State, input (via a remappable keymap), rendering — incl. the fleet overlay and the attach pane |
| 2. Linear client | `linear` | Blocking `ureq` GraphQL read (personal key); write-back is v2 |
| 3. Control plane | `backend` | `AgentBackend` trait + `PtyAgent` (PTY host); Codex/Aider slot in here |
| 4. Pipeline engine | *(v3)* | Generic stage machine over `.lindep/pipeline.toml` — not in v1 |
| 5. Notification bus | `notify` | Claude hooks → loopback endpoint → `AppEvent` |
| 6. Worktree manager | `worktree` | One git worktree + branch per issue, crash-safe |
| (glue) | `event`, `supervisor`, `session` | Async backbone, fleet owner, durable state |

## Concurrency model — where work runs

The hard rule that shapes everything: **the render loop stays synchronous and is
the single writer of `App` state.** Everything else runs off-thread and reaches
the loop through one channel.

```
┌─────────────────────────── main thread (synchronous) ───────────────────────────┐
│  event_loop:  poll(stdin, 16ms attached / 250ms idle)                            │
│    • crossterm key/resize → App::on_key  (the only App mutator from input)       │
│    • drain AppEventRx.try_recv() → App::apply_event  (the only App mutator off-   │
│      thread events feed)                                                          │
│    • repaint ONLY when something changed (dirty flag) → ui::draw(&App)            │
└──────────────────────────────────────────────────────────────────────────────────┘
        ▲ AppEvent (mpsc, unbounded)                    │ SupervisorCmd (mpsc)
        │                                                ▼
┌─────────────────────────────── tokio runtime (multi-thread) ──────────────────────┐
│  supervisor task        — owns the fleet map; launch/cancel/reap/shutdown          │
│  per-agent run_agent     — worktree→session→hooks→spawn→supervise→teardown→reap    │
│  hook endpoint (axum-free TcpListener) — 127.0.0.1, maps hooks → AppEvent          │
└──────────────────────────────────────────────────────────────────────────────────┘
        │ (per agent)
┌────────────────────────── dedicated OS threads (blocking) ─────────────────────────┐
│  PTY read pump  — reader → vt100::Parser (Arc<RwLock>) → AppEvent::AgentOutput      │
│  PTY wait/reap  — child.wait() → set Lifecycle::Exited → AgentExited + notify_one   │
└──────────────────────────────────────────────────────────────────────────────────┘
```

**Why this split.** PTY reads/`wait()` are blocking and long-lived, so they get
dedicated OS threads (not tokio workers). The supervisor and endpoint are async
(channels, timeouts, structured cancellation). The render loop is sync because
ratatui rendering is a pure function of state and must never `.await`.

**Shared state, deliberately minimal:**

- `Arc<Mutex<SessionStore>>` — supervisor + hook endpoint. Held only across
  synchronous critical sections, **never across an `.await`**.
- `Arc<RwLock<vt100::Parser>>` per agent — written by the read pump, read by the
  render thread for the attach pane.
- `Arc<dyn AgentBackend>` per agent — the cockpit holds one (to render/drive when
  attached), the agent task holds one (lifecycle). When both drop, `PtyAgent`'s
  `Drop` runs and signals the process group.

Everything else is messages: `AppEvent` (off-thread → loop) and `Command`
(cockpit → supervisor).

## Data flow

**Input.** A keypress hits `App::on_key`. While **attached**, the agent owns the
keyboard: the key is encoded by `backend::key_to_bytes` and written to the PTY via
`AgentBackend::send_input` — only the `F10` detach key is intercepted. While
on the **dashboard**, keys drive the cockpit (`a` launch, `t` attach, `x` stop,
`n` next-needs-you, plus all the graph navigation).

**Output + notifications (everything that changes the screen off-thread):**

```
PTY bytes ─► read pump ─► vt100 parser ─► AppEvent::AgentOutput ─┐
claude hook ─► `lindep --hook-forward` ─► 127.0.0.1 endpoint ─► route ─► AppEvent::{AgentNeedsYou,AgentStatusChanged,AgentAction} ─┤
supervisor ─► AppEvent::{AgentSpawned,AgentStatusChanged,Notification} ─┤
                                                                         ▼
                                                       AppEventRx (drained each tick)
                                                                         ▼
                                                          App::apply_event → repaint
```

`apply_event` is the **single funnel** for off-thread state changes, keeping
rendering a pure function of `App`. Status authority is split cleanly to avoid a
race: the **supervisor** owns fleet status via `AgentStatusChanged` (Idle on a
deliberate cancel, Done/Failed on a self-exit); `AgentExited` only updates the
footer line and reclaims the dead PTY handle — it never sets status.

**Commands.** The cockpit holds a cheap, cloneable `SupervisorHandle` and sends
fire-and-forget `Command`s (`Launch`/`Cancel`/`Shutdown`). The supervisor's
single task processes them serially; agent tasks send an internal
`Command::Reaped` back when teardown completes.

## The agent lifecycle

```
            a (launch)
               │
               ▼
   ┌─ supervisor.launch ─ synchronous: guard (running? at capacity?),
   │   assign generation, insert AgentRecord{gen, token}, spawn run_agent task
   │
   ▼ run_agent (off the command loop, so slow git never blocks cancel/shutdown)
   worktree.create ──(cancelled during git? bail)──┐
   session.ensure (deterministic id; --resume if record exists)
   write .lindep/hooks/<ISSUE>.settings.json
   backend spawn (PtyAgent: openpty → spawn_command → read/wait threads)
   emit AgentSpawned{backend}
               │
               ▼   select! { token.cancelled()  |  exit_notify.notified() }
        ┌──────┴───────┐
   cancelled            self-exited
        │                    │
   SIGTERM group        (already gone)
   await_exit (poll status, ≤grace)
   └─ still alive? SIGKILL, await_exit again
        │                    │
        ▼                    ▼
   status = Idle        status = Done/Failed (from exit code)
        └──────┬─────────────┘
               ▼
   set_status + save; emit AgentStatusChanged
   send Command::Reaped{issue, gen}  ──►  supervisor drops the record (if gen matches)
```

Two subtleties worth knowing, both the result of review findings:

- **The reaper.** A finished agent is removed from the supervisor's map only when
  its task reports `Reaped`. This is what makes relaunch-after-exit work and stops
  dead agents from leaking concurrency slots. The `generation` guards a late reap
  from dropping a freshly relaunched record.
- **Confirmed teardown.** Shutdown awaits actual process death by **polling the
  monotonic `Lifecycle`**, not by re-awaiting the one-shot exit `Notify` (which a
  racing `select!` can consume — a lost-wakeup that would stall shutdown). SIGKILL
  escalates if SIGTERM is ignored, so no process group outlives the cockpit.

## State & durability (`.lindep/`, gitignored)

```
.lindep/
  worktrees/<ISSUE>/         one git worktree + branch felix/<issue>-<slug> per issue
  hooks/<ISSUE>.settings.json generated hook config passed via `claude --settings`
  state.json                 atomic (temp+rename) serde_json session store, versioned
  transcripts/…              NDJSON logs referenced by path (never inlined)
```

The **process is disposable; the conversation is durable.** Each issue's `claude`
`session_id` is a deterministic UUIDv5 of the issue id, so even if `state.json` is
lost the same id regenerates and `--resume` reconnects. On startup the store is
reconciled against live worktrees (records whose worktree vanished are dropped).

## Notifications & hooks

The cockpit hosts a loopback HTTP endpoint on an ephemeral `127.0.0.1` port. At
launch it writes a per-issue `settings.json` registering, for `Notification` /
`Stop` / `PostToolUse`, the command `lindep --hook-forward <port>` — i.e. **this
binary in a one-shot forwarder mode**, so there is no `curl` dependency and the
forwarder always knows where to POST. Claude runs it via `--settings`, which
layers onto the repo's settings rather than clobbering a checked-in
`.claude/settings.json`.

A hook's stdin JSON is relayed verbatim; the endpoint maps it back to an issue by
`session_id` (falling back to `cwd`/worktree path) and emits the matching
`AppEvent`. So a permission prompt or an idle nudge lights up the right node live,
even with many agents running. The endpoint binds loopback only, bounds each
connection with a timeout, and survives transient `accept()` errors.

## Operating guide

```sh
cargo run                    # cockpit, in a git repo (or: cargo run -- "Project")
cargo run -- --demo          # graph viewer only, no agents, no key needed
```

| Key | Action |
|-----|--------|
| `a` | Launch a Claude agent on the focused issue |
| `t` | Attach to its live terminal (all keys go to the agent) |
| `F10` | Detach (agent keeps running) |
| `x` | Stop the focused issue's agent |
| `n` | Jump to the next issue whose agent needs you |
| `?` | Full keymap (graph navigation keys unchanged) |

Node glyphs: `◌` spawning · `▸` running · `⚑` needs you · `◦` idle · `✓` done ·
`✗` failed. Header shows `N agents · M needs you`.

Every binding is remappable via a `[keys]` table in `config.toml`
(`<repo>/.lindep/config.toml`, then `~/.config/lindep/config.toml` — personal
wins; same two-location pattern as `.env`). The `keymap` module owns the
`Action` enum, the binding parser, the defaults, conflict detection, and loading;
`App` dispatches keys through it. `Esc` and the search/help overlays are fixed.
The attach pane and the `?` help render the *live* bindings, so a rebind (e.g.
`detach = "f8"` when a keyboard lacks F10) is always shown correctly — you can't
strand yourself attached. Bad config warns on stderr at startup and falls back to
defaults rather than aborting.

`detach` may also be a **tmux-style leader sequence** (`detach = "ctrl-a d"`) —
the only action that can, because a reserved-key gesture is needed solely while
attached (the dashboard has free single keys). The leader (`Ctrl-A`) arms a
pending state in `App`; the next key completes the detach, a repeat of the leader
passes one leader chord through to the agent, and anything else cancels and
forwards. `Ctrl-<letter>` leaders work on every keyboard/terminal, which a lone
function key does not.

## Key decisions (and what we learned building it)

- **In-process PTY via tui-term/portable-pty**, not the coarse `claude --bg` API.
  tui-term 0.3.4 unifies with ratatui 0.30.1 — no bump (see the spike verdict).
- **Hooks are the event bus**, fired in interactive *and* (future) headless runs.
- **`--max-budget-usd` / `--max-turns` are `--print`-only** (confirmed against
  claude 2.1.177) — so interactive v1 agents use `--permission-mode default` and
  the concurrency cap; budget guardrails belong to the headless/phase-3 path.
- **Tool-agnostic backend.** `PtyAgent` hosts *any* PTY CLI; "the Claude backend"
  is just `SpawnConfig::claude` choosing the program + args. Codex/Aider are the
  same host with a different command builder.
- **Rust discipline** (per the `rust-best-practices` / `rust-async-patterns`
  skills): one `thiserror` enum per subsystem, no `unwrap`/`expect` outside tests,
  `TaskTracker`/`CancellationToken`, channels over shared state, no lock across an
  `.await`. The tree is `clippy --all-targets -D warnings` and `fmt` clean.

## Testing

81 tests run headlessly. Notably: the worktree manager against a real temp git
repo; the session store round-trip/reconcile; the hook endpoint via real loopback
POSTs mapping concurrent agents; the **PTY plumbing end-to-end against a non-`claude`
program** (`sh`); the supervisor (launch/cancel/shutdown/relaunch/reap) against a
`FakeBackend` injected via the `SpawnFn` seam; and fleet/attach snapshot renders.

The `FakeBackend` is faithful to `PtyAgent`'s notify semantics (its
`shutdown`/`force_kill` deliberately do **not** re-notify), so the cancel-path
tests would hang if anyone reverted the status-poll teardown to awaiting the
consumable `Notify` — a built-in regression guard.

**The one thing tests can't cover** (and the spike's real question): how
`claude`'s own TUI looks *inside* the tui-term pane — colors, streaming, the
permission UI. That needs a human at a real terminal: `cargo run`, `a`, then `t`.

## Deferred (roadmap)

- **v2 — Linear write-back:** issue-state transitions, progress comments, PR
  attachments; OAuth `actor=app` agents surfacing AgentSession/AgentActivity.
- **v3 — Pipeline engine:** configurable `.lindep/pipeline.toml` stage machine,
  unattended headless stages, human gates.
- **Later:** Codex/Aider backends; survivable detached runs; a hosted Linear
  webhook receiver (the agent-side "needs you" is already fully local).

### Known v1 limitations

- Visual fidelity of claude-in-tui-term is vt100-level; a `vt100-ctt` fallback is
  noted in the spike verdict if needed.
- A hung `git worktree add` delays only *that* launch: it runs off the command
  loop, the launch races its blocking git against the cancel token, and quit
  bounds the teardown wait (`SHUTDOWN_GRACE`) and restores the terminal
  regardless — so a wedged git can't freeze cancel/shutdown or the exit.
- A killed mid-task agent loses its in-flight tool action; `--resume` recovers the
  conversation, not the action.
