# ENG-392 — Spike verdict: embedding a live `claude` PTY in a ratatui pane

**Verdict: GO.** `tui-term` is the right tool; no ratatui bump is needed, and the
PTY plumbing is proven end-to-end. One purely-visual confirmation is left to a
human terminal (see *What only a human can confirm*).

This spike was **folded into the production backend** (ENG-396) rather than
shipped as throwaway code — the real `PtyAgent` is the spike, and it carries
automated tests that exercise the same code path against a non-`claude` program.

## What was verified empirically

* **Version compatibility (the central risk).** `tui-term 0.3.4` depends on
  `ratatui-core 0.1` / `ratatui-widgets 0.3`; our `ratatui 0.30.1` pins
  `ratatui-core 0.1.1` / `ratatui-widgets 0.3.1`, which semver-unify to a single
  build of each. So `tui_term::widget::PseudoTerminal` implements the *same*
  `ratatui_core::widgets::Widget` our `Frame` renders. **The whole tree compiles
  with no ratatui bump** (`cargo build` green on first try).
* **`PseudoTerminal` renders our parser.** `tui-term` implements its `Screen`
  trait for `vt100::Screen`, so `PseudoTerminal::new(parser.screen())` renders
  directly through `frame.render_widget`. Enable `features = ["unstable"]` for
  the portable-pty helpers; the default `vt100` feature gives the parser.
* **PTY lifecycle works.** `NativePtySystem::openpty` → `slave.spawn_command` →
  `master.try_clone_reader()` (output pump → `vt100::Parser` on an OS thread) →
  `master.take_writer()` (input). Dropping the slave after spawn makes the master
  observe EOF when the child exits, which cleanly ends the read pump.
* **Input → output → resize → exit, end to end.** An automated test
  (`backend::tests::pty_agent_hosts_a_real_process_end_to_end`) spawns a real
  `sh` program on a PTY, asserts its banner reaches the parser, sends it a line,
  sees the response, and confirms the exit surfaces as an `AppEvent`. Resize
  (`resize_updates_the_parser_grid`) and process-group teardown
  (`shutdown_terminates_the_process_and_is_idempotent`) are likewise tested.
* **Resize does both halves.** `resize()` calls `parser.screen_mut().set_size()`
  **and** `master.resize(PtySize{..})`, so our render reflows *and* the child
  gets `SIGWINCH` — as the spike notes required.
* **Key encoding is complete.** Unlike the tui-term examples (which `todo!()` on
  Esc / Ctrl / function keys), `backend::key_to_bytes` encodes Enter (`\r`),
  Backspace (`\x7f`), Esc, Ctrl-<char> control bytes, Alt prefixes, arrows,
  Home/End, Page keys, and F1–F12 — everything `claude`'s permission UI needs.

## What only a human can confirm (your terminal, real `claude`)

The automated tests use `sh`, not `claude`, because a headless sandbox can't
render or eyeball an interactive TUI. On a real terminal, please confirm:

1. `claude`'s colors, prompt, streaming output and **permission UI** render
   acceptably inside the pane (attach with `t`).
2. Typing reaches `claude`; arrows / Enter / Esc / Ctrl-C behave.
3. Resizing the terminal reflows `claude`'s UI while attached.

## Known gaps / caveats (carry-forward, not blockers)

* **vt100-only.** `tui-term` parses via `vt100`, which is a solid xterm subset.
  Mouse reporting, alt-screen nuances and 24-bit true-color may render
  approximately. If `claude`'s UI looks degraded, the documented fallback is the
  `vt100-ctt` fork (a drop-in `Screen` impl) — isolate behind our `parser()`.
* **`--session-id` / `--resume` in interactive mode.** We pass `--session-id
  <uuid>` on first launch and `--resume <uuid>` thereafter (both confirmed
  present in `claude --version 2.1.177` via `--help`). The exact interactive
  behaviour of pre-assigning a session id should be eyeballed once.
* **Kill semantics.** A killed mid-task agent loses its in-flight tool action;
  `--resume` recovers the conversation, not the action. Accepted for v1.
* **Blocking pumps.** PTY read/wait run on dedicated OS threads (not tokio
  workers), which is correct for long-lived blocking I/O.

## Bottom line

tui-term 0.3.4 + portable-pty 0.9 + vt100 0.16 host a real interactive program
in a ratatui pane on our existing stack, with working input, resize and
teardown. **Proceed** — the control plane (ENG-396 → 400) is built on it.
