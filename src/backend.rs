//! The agent control plane: a tool-agnostic [`AgentBackend`] trait and a
//! [`PtyAgent`] that hosts any interactive CLI on a real pseudo-terminal.
//!
//! The **Claude backend** is `PtyAgent` driven by a [`SpawnConfig::claude`]
//! invocation; a future Codex/Aider backend is the *same* host with a different
//! command builder — which is the whole point of keeping the abstraction at
//! "host a PTY program" rather than "know about claude".
//!
//! Threading model (no tokio runtime required — the channels are just data):
//! * one OS thread pumps PTY output into a shared [`vt100::Parser`] and signals
//!   a repaint via [`AppEvent::AgentOutput`];
//! * one OS thread blocks in `child.wait()` and, on exit, records the status,
//!   emits [`AppEvent::AgentExited`] and wakes [`AgentBackend::exit_notify`];
//! * input is written directly under a short-lived mutex (keystrokes are tiny).
//!
//! Teardown signals the child's **process group** (the child is a `setsid`
//! session leader, so this reaches every tool it spawned) and is also run from
//! `Drop`, giving kill-on-drop semantics.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::Notify;
use vt100::Parser;

use crate::event::{AppEvent, AppEventTx};

/// Lines of scrollback the vt100 parser retains per agent.
const SCROLLBACK: usize = 1000;

/// Anything that can go wrong hosting an agent on a PTY.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    // portable-pty surfaces `anyhow::Error`, which isn't a `std::error::Error`
    // we can `#[source]`-wrap; `{e:#}` preserves its full cause chain as text.
    #[error("pty error: {0}")]
    Pty(String),
    #[error("spawning `{program}`: {detail}")]
    Spawn { program: String, detail: String },
    #[error("agent i/o: {0}")]
    Io(#[source] std::io::Error),
    // Thread spawn fails with a real `io::Error` (EAGAIN under fd/RLIMIT
    // pressure); keep it as a `#[source]` for the cause chain while still showing
    // the reason in Display (the supervisor renders `{e}` into the footer).
    #[error("spawning agent thread: {0}")]
    Thread(#[source] std::io::Error),
    #[error("an agent lock was poisoned")]
    Poisoned,
}

/// Process-level lifecycle — distinct from the conversational `AgentStatus`
/// (needs-you / idle / done), which the notification bus derives from hooks. A
/// backend only knows whether its process is starting, running or gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    Running,
    Exited(Option<i32>),
}

/// A fully-resolved command to host on a PTY. Generic on purpose: what makes an
/// agent "claude" is only the `program` + `args`, built by [`SpawnConfig::claude`].
#[derive(Debug, Clone)]
pub struct SpawnConfig {
    /// The Linear project the agent's issue belongs to — tags the lifecycle
    /// events this backend emits so the cockpit files them under the right
    /// project's fleet/window set.
    pub project_id: String,
    pub issue: String,
    pub cwd: PathBuf,
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub rows: u16,
    pub cols: u16,
}

impl SpawnConfig {
    /// Build a `claude` invocation for an issue's worktree.
    ///
    /// A brand-new conversation pre-assigns its id with `--session-id` so the
    /// cockpit owns it; a returning one passes `--resume <id>`. Guardrails
    /// (`--permission-mode`, the `--settings` hook file, …) are layered on with
    /// [`SpawnConfig::arg`] by the supervisor.
    pub fn claude(
        project_id: impl Into<String>,
        issue: impl Into<String>,
        cwd: PathBuf,
        session_id: &str,
        resume: bool,
        rows: u16,
        cols: u16,
    ) -> Self {
        let args = if resume {
            vec!["--resume".to_string(), session_id.to_string()]
        } else {
            vec!["--session-id".to_string(), session_id.to_string()]
        };
        SpawnConfig {
            project_id: project_id.into(),
            issue: issue.into(),
            cwd,
            program: "claude".to_string(),
            args,
            // The spawn path does `env_clear()` first (so the cockpit's secrets
            // never reach the agent), so we must re-add everything claude needs.
            env: claude_env(),
            rows,
            cols,
        }
    }

    /// Append a command-line argument (chainable).
    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }
}

/// Variables an `env_clear()`ed claude agent legitimately needs, passed through
/// from the cockpit's environment when present. The point of the allowlist (vs
/// inheriting the whole environment) is that the cockpit's secrets — chiefly the
/// personal `LINEAR_API_KEY` loaded via dotenvy — never reach an autonomous
/// agent or the arbitrary tools it spawns in its worktree.
const CLAUDE_ENV_ALLOW: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
    "SSH_AUTH_SOCK",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_MODEL",
    "CLAUDE_CONFIG_DIR",
];

/// Build the deliberate child environment for a claude agent: every allowlisted
/// variable the cockpit actually has, plus a stable terminal identity so the
/// hosted TUI renders consistently regardless of the launching environment.
fn claude_env() -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = CLAUDE_ENV_ALLOW
        .iter()
        .filter_map(|&key| std::env::var(key).ok().map(|val| (key.to_string(), val)))
        .collect();
    // Pin the terminal so claude keys its rendering off a known-good profile
    // even when launched from a minimal/unknown TERM (the spike's failure mode).
    env.push(("TERM".to_string(), "xterm-256color".to_string()));
    env.push(("COLORTERM".to_string(), "truecolor".to_string()));
    env
}

/// A tool-agnostic handle to one hosted agent. Every method takes `&self` and is
/// safe to call from any thread (the render thread drives input/resize/parser;
/// the supervisor drives lifecycle), so the cockpit shares a single
/// `Arc<dyn AgentBackend>` per agent.
pub trait AgentBackend: Send + Sync + std::fmt::Debug {
    /// Forward already-encoded input bytes (see [`key_to_bytes`]) to the PTY.
    fn send_input(&self, bytes: &[u8]) -> Result<(), AgentError>;
    /// Resize the terminal: the vt100 grid (so our render reflows) **and** the
    /// kernel PTY (so the child receives `SIGWINCH`).
    fn resize(&self, rows: u16, cols: u16) -> Result<(), AgentError>;
    /// The shared screen buffer the cockpit renders via tui-term's
    /// `PseudoTerminal`.
    fn parser(&self) -> Arc<RwLock<Parser>>;
    /// Current process lifecycle.
    fn status(&self) -> Lifecycle;
    /// Fires once when the agent process exits, so the supervisor can reap it
    /// without polling. Uses `notify_one`, so a waiter that arrives *after* the
    /// exit still gets the signal.
    fn exit_notify(&self) -> Arc<Notify>;
    /// Terminate the agent and its process group (SIGTERM). Idempotent.
    fn shutdown(&self);
    /// Forcibly kill the process group (SIGKILL) — the escalation when an agent
    /// ignores the graceful `shutdown`.
    fn force_kill(&self);
    /// The agent's working directory — the per-issue workspace (the single worktree
    /// for a one-repo issue, or the parent dir with each repo a sibling subdir).
    /// What the v1.6 `OpenInEditor` action opens in an external editor.
    fn cwd(&self) -> &std::path::Path;
}

/// How the supervisor (and tests) construct backends. Injecting this is what
/// lets the supervisor be exercised with a `FakeBackend` (test-only) instead of
/// real `claude`.
pub type SpawnFn =
    dyn Fn(SpawnConfig, AppEventTx) -> Result<Arc<dyn AgentBackend>, AgentError> + Send + Sync;

/// Open `dir` in an external editor (v1.6 `OpenInEditor`), **detached** and
/// **inheriting** the cockpit's environment — the deliberate inverse of the agent
/// spawn's `env_clear`: the editor is the user's own handoff tool, not a sandboxed
/// agent. Resolves the editor from `$VISUAL`, then `$EDITOR`, then `code`. The
/// child gets its own session (`setsid`) with stdio nulled and its handle dropped,
/// so it never touches the TUI's terminal and survives cockpit teardown. Returns
/// the editor command that was launched, or an error string for a footer.
pub fn open_in_editor(dir: &std::path::Path) -> Result<String, String> {
    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or_else(|| "code".to_string());
    // Allow a multi-word command (e.g. `code -n`): the first word is the program,
    // the rest fixed arguments, then the directory to open.
    let mut parts = editor.split_whitespace();
    let program = parts
        .next()
        .ok_or_else(|| "empty editor command".to_string())?;
    let mut cmd = std::process::Command::new(program);
    cmd.args(parts);
    cmd.arg(dir);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `setsid` in the forked child before `exec` — async-signal-safe,
        // no allocation. Detaches the editor from the cockpit's controlling
        // terminal so it survives teardown and never steals the TUI's input.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    match cmd.spawn() {
        // Detached: drop the handle, never wait on it.
        Ok(child) => {
            drop(child);
            Ok(editor)
        }
        Err(e) => Err(format!("{program}: {e}")),
    }
}

/// The production spawn function: host the configured command on a real PTY.
pub fn pty_spawn() -> Arc<SpawnFn> {
    Arc::new(PtyAgent::spawn)
}

// ── PtyAgent — the real backend ──────────────────────────────────────────────

/// Hosts one interactive CLI on a pseudo-terminal.
pub struct PtyAgent {
    issue: String,
    /// The working directory the agent was spawned in — its per-issue workspace.
    cwd: PathBuf,
    parser: Arc<RwLock<Parser>>,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    /// Child pid, captured at spawn for process-group signalling.
    pid: Option<u32>,
    /// Split-off killer so we can terminate the child from a thread other than
    /// the one blocked in `wait()`.
    killer: Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>,
    lifecycle: Arc<Mutex<Lifecycle>>,
    exit_notify: Arc<Notify>,
    /// Set by the wait thread the moment `child.wait()` reaps the zombie. Gates
    /// the `killpg` paths to minimize the chance of signalling a pid the OS may
    /// have recycled (best-effort — see [`PtyAgent::signal_group`]).
    reaped: Arc<AtomicBool>,
    /// Used only to surface a footer diagnostic when a process-group signal fails
    /// for a real reason (EPERM/…) — a leak that otherwise produces no output.
    events: AppEventTx,
    terminated: AtomicBool,
}

impl PtyAgent {
    /// Spawn `cfg.program` on a PTY in `cfg.cwd` and start the output/wait pumps.
    pub fn spawn(
        cfg: SpawnConfig,
        events: AppEventTx,
    ) -> Result<Arc<dyn AgentBackend>, AgentError> {
        let (rows, cols) = (cfg.rows.max(1), cfg.cols.max(1));
        let pty = NativePtySystem::default();
        let pair = pty
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| AgentError::Pty(format!("{e:#}")))?;

        let mut cmd = CommandBuilder::new(&cfg.program);
        cmd.cwd(&cfg.cwd);
        // Start from a clean slate so the cockpit's secrets (e.g. LINEAR_API_KEY
        // loaded via dotenvy) never leak into an autonomous agent or the tools it
        // spawns. Only the deliberately-chosen pairs below reach the child.
        cmd.env_clear();
        // PATH is not a secret and is required both to resolve `cfg.program` and
        // for the child to find its own tools; re-add the cockpit's unless the
        // caller already supplies one. (`SpawnConfig::claude` provides a full
        // allowlist including PATH; bare callers — e.g. tests — rely on this.)
        if !cfg.env.iter().any(|(k, _)| k == "PATH")
            && let Ok(path) = std::env::var("PATH")
        {
            cmd.env("PATH", path);
        }
        for (key, value) in &cfg.env {
            cmd.env(key, value);
        }
        for arg in &cfg.args {
            cmd.arg(arg);
        }

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| AgentError::Spawn {
                program: cfg.program.clone(),
                detail: format!("{e:#}"),
            })?;
        let pid = child.process_id();
        let killer = child.clone_killer();
        // Drop our handle to the slave so the master observes EOF the moment the
        // child (the last slave holder) exits — that's what ends the read pump.
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| AgentError::Pty(format!("{e:#}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| AgentError::Pty(format!("{e:#}")))?;

        let parser = Arc::new(RwLock::new(Parser::new(rows, cols, SCROLLBACK)));
        let lifecycle = Arc::new(Mutex::new(Lifecycle::Running));
        let exit_notify = Arc::new(Notify::new());
        // Flipped by the wait thread the instant `child.wait()` reaps the zombie,
        // so signal paths almost never killpg() a pid the OS may already have
        // recycled (best-effort — the store narrows but can't fully close it).
        let reaped = Arc::new(AtomicBool::new(false));

        // Output pump.
        {
            let parser = Arc::clone(&parser);
            let events = events.clone();
            // `Arc<str>` so each per-chunk `AgentOutput` clones a refcount, not a String.
            let project_id: Arc<str> = Arc::from(cfg.project_id.as_str());
            let issue: Arc<str> = Arc::from(cfg.issue.as_str());
            thread::Builder::new()
                .name(format!("pty-read-{issue}"))
                .spawn(move || read_pump(reader, &parser, &events, &project_id, &issue))
                // The child is already live; a thread-spawn failure here (EAGAIN
                // under fd/thread pressure) would orphan its whole process group
                // with no handle to signal it, so tear it down before bailing.
                .map_err(|e| {
                    kill_orphan(pid, &mut child.clone_killer());
                    AgentError::Thread(e)
                })?;
        }
        // Wait / reap.
        {
            let events = events.clone();
            let project_id = cfg.project_id.clone();
            let issue = cfg.issue.clone();
            let lifecycle = Arc::clone(&lifecycle);
            let exit_notify = Arc::clone(&exit_notify);
            let reaped = Arc::clone(&reaped);
            // A handle to signal the child if *this* spawn fails: `child` is about
            // to move into the closure, so capture a killer first.
            let mut orphan_killer = child.clone_killer();
            let spawned = thread::Builder::new()
                .name(format!("pty-wait-{issue}"))
                .spawn(move || {
                    let code = child.wait().ok().map(|s| (s.exit_code() & 0xff) as i32);
                    // Mark reaped *before* publishing the exit, so a shutdown
                    // racing in on the notification almost never signals a
                    // recycled pid (the store narrows the window; see signal_group).
                    reaped.store(true, Ordering::SeqCst);
                    if let Ok(mut l) = lifecycle.lock() {
                        *l = Lifecycle::Exited(code);
                    }
                    let _ = events.send(AppEvent::AgentExited {
                        project_id,
                        issue,
                        code,
                    });
                    exit_notify.notify_one();
                });
            if let Err(e) = spawned {
                // The read pump is now blocked on a child that nothing will reap;
                // killing the group makes the pump see EOF and exit cleanly.
                kill_orphan(pid, &mut orphan_killer);
                return Err(AgentError::Thread(e));
            }
        }

        Ok(Arc::new(PtyAgent {
            issue: cfg.issue,
            cwd: cfg.cwd,
            parser,
            writer: Mutex::new(writer),
            master: Mutex::new(pair.master),
            pid,
            killer: Mutex::new(Some(killer)),
            lifecycle,
            exit_notify,
            reaped,
            events,
            terminated: AtomicBool::new(false),
        }))
    }
}

impl AgentBackend for PtyAgent {
    fn send_input(&self, bytes: &[u8]) -> Result<(), AgentError> {
        let mut writer = self.writer.lock().map_err(|_| AgentError::Poisoned)?;
        writer.write_all(bytes).map_err(AgentError::Io)?;
        writer.flush().map_err(AgentError::Io)
    }

    fn resize(&self, rows: u16, cols: u16) -> Result<(), AgentError> {
        let (rows, cols) = (rows.max(1), cols.max(1));
        // Both halves must learn the size, or claude's UI and our render diverge — but
        // resize the kernel PTY FIRST and mutate the vt100 grid only on success, so a
        // failed / poisoned master resize leaves BOTH halves at the old (consistent)
        // size. (The old order shrank the grid first; on a master `Err` that left the
        // grid ahead of the child AND defeated the A3 bottom-align, since the grid no
        // longer read as "too tall".) (E-S2.)
        let master = self.master.lock().map_err(|_| AgentError::Poisoned)?;
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| AgentError::Pty(format!("{e:#}")))?;
        if let Ok(mut parser) = self.parser.write() {
            parser.screen_mut().set_size(rows, cols);
        }
        Ok(())
    }

    fn parser(&self) -> Arc<RwLock<Parser>> {
        Arc::clone(&self.parser)
    }

    fn status(&self) -> Lifecycle {
        // A poisoned lock means a pump thread panicked; report Running rather
        // than fabricate an exit — the wait thread still owns the real verdict.
        self.lifecycle
            .lock()
            .map(|l| *l)
            .unwrap_or(Lifecycle::Running)
    }

    fn exit_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.exit_notify)
    }

    fn shutdown(&self) {
        if self.terminated.swap(true, Ordering::SeqCst) {
            return; // already torn down
        }
        // Signal the whole group so the child AND any tools it spawned die. The
        // child is a setsid session leader, so its pid is the group id.
        #[cfg(unix)]
        self.signal_group(libc::SIGTERM);
        // Portable fallback (and the whole story on Windows): kill the child.
        self.kill_child();
    }

    fn force_kill(&self) {
        // SIGKILL the whole group — unblockable, used only after a graceful
        // shutdown's grace period has elapsed.
        #[cfg(unix)]
        self.signal_group(libc::SIGKILL);
        self.kill_child();
    }

    fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }
}

impl PtyAgent {
    /// Signal the child's process group, but only while it is still un-reaped:
    /// once the wait thread has `wait()`ed the zombie its pid (== pgid) can be
    /// recycled, so signalling it could hit an unrelated, newly-created group.
    /// The `reaped` gate is best-effort: it narrows, but cannot fully close, the
    /// window between the wait thread's `wait()` and its `reaped.store(true)`.
    #[cfg(unix)]
    fn signal_group(&self, sig: libc::c_int) {
        if self.reaped.load(Ordering::SeqCst) {
            return; // already reaped — the pid may have been recycled
        }
        let Some(pid) = self.pid else { return };
        // SAFETY: `killpg` is a plain FFI call — sound to invoke for any pid_t.
        // The `reaped` gate above narrows (but cannot fully close) the TOCTOU
        // between the wait thread's `wait()` and its `reaped.store(true)`; the
        // residual window is accepted — sequential pid allocation makes a recycle
        // astronomically unlikely, and the worst case is a misdirected signal,
        // never unsoundness.
        let rc = unsafe { libc::killpg(pid as libc::pid_t, sig) };
        if rc == 0 {
            return;
        }
        // ESRCH just means the group is already gone (a benign race with a
        // self-exit). Anything else — EPERM, EINVAL — is a real failure that can
        // strand a grandchild tool with no other diagnostic, so surface it.
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ESRCH) {
            let _ = self.events.send(AppEvent::Notification(format!(
                "{}: process-group signal failed: {err}",
                self.issue
            )));
        }
    }

    /// Portable child kill (the whole story on Windows). Skipped once reaped so
    /// we don't ask portable-pty to signal a pid it has already waited on.
    fn kill_child(&self) {
        if self.reaped.load(Ordering::SeqCst) {
            return;
        }
        if let Ok(mut killer) = self.killer.lock()
            && let Some(killer) = killer.as_mut()
        {
            let _ = killer.kill();
        }
    }
}

/// Tear down a child that was spawned but whose `PtyAgent` will never be built
/// (a post-spawn setup failure). Without this the process — a setsid group
/// leader — would outlive the cockpit with no handle able to signal it, since
/// `Drop`/`shutdown` never run for a struct that was never constructed.
fn kill_orphan(pid: Option<u32>, killer: &mut Box<dyn ChildKiller + Send + Sync>) {
    // The child is not yet reaped on this path, so the pid is unambiguously ours.
    #[cfg(unix)]
    if let Some(pid) = pid {
        // SAFETY: signalling our own just-spawned child's process group.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
    let _ = killer.kill();
}

impl Drop for PtyAgent {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl std::fmt::Debug for PtyAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtyAgent")
            .field("issue", &self.issue)
            .field("pid", &self.pid)
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

/// Copy PTY output into the shared parser, signalling a repaint per chunk. The
/// render loop coalesces many queued signals into a single redraw per tick, so
/// emitting one per read is fine.
fn read_pump(
    mut reader: Box<dyn Read + Send>,
    parser: &Arc<RwLock<Parser>>,
    events: &AppEventTx,
    project_id: &Arc<str>,
    issue: &Arc<str>,
) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break, // EOF: the child is gone
            Ok(n) => {
                if let Ok(mut parser) = parser.write() {
                    parser.process(&buf[..n]);
                }
                // Refcount bumps, not allocations — this fires per PTY read chunk.
                let _ = events.send(AppEvent::AgentOutput {
                    project_id: Arc::clone(project_id),
                    issue: Arc::clone(issue),
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break, // EIO etc. once the PTY closes
        }
    }
}

// ── Input encoding ───────────────────────────────────────────────────────────

/// Encode a crossterm key into the bytes a PTY program expects. Unlike the
/// tui-term examples (which `todo!()` on Esc / Ctrl / function keys), this is
/// complete — claude's permission UI needs Esc, Enter and Ctrl-C to work.
pub fn key_to_bytes(key: KeyEvent) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let mods = key.modifiers;

    match key.code {
        KeyCode::Char(c) if ctrl => ctrl_byte(c),
        KeyCode::Char(c) => {
            let mut bytes = c.to_string().into_bytes();
            if alt {
                let mut out = vec![0x1b];
                out.append(&mut bytes);
                out
            } else {
                bytes
            }
        }
        // Real terminals send CR for Enter and DEL for Backspace; raw-mode TUIs
        // like claude expect exactly that. Shift/Alt-Enter is the editor's
        // "newline in prompt" gesture; the broadly-recognised encoding is a
        // meta-prefixed CR (ESC CR), so it stays distinct from a bare submit.
        KeyCode::Enter if mods.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) => {
            vec![0x1b, b'\r']
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => vec![0x1b, b'[', b'Z'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Left => csi(mods, b'D'),
        KeyCode::Right => csi(mods, b'C'),
        KeyCode::Up => csi(mods, b'A'),
        KeyCode::Down => csi(mods, b'B'),
        KeyCode::Home => csi(mods, b'H'),
        KeyCode::End => csi(mods, b'F'),
        KeyCode::PageUp => vec![0x1b, b'[', b'5', b'~'],
        KeyCode::PageDown => vec![0x1b, b'[', b'6', b'~'],
        KeyCode::Delete => vec![0x1b, b'[', b'3', b'~'],
        KeyCode::Insert => vec![0x1b, b'[', b'2', b'~'],
        KeyCode::F(n) => function_key(n),
        _ => Vec::new(), // CapsLock, media keys, … carry nothing for the PTY
    }
}

/// CSI encoding for the arrow / Home / End keys. Bare (`ESC [ <final>`) when
/// unmodified; with modifiers, the xterm `ESC [ 1 ; <mod> <final>` form so the
/// agent's editor can distinguish word-wise (Ctrl/Alt) and selection (Shift)
/// movement from a plain cursor key.
fn csi(mods: KeyModifiers, final_byte: u8) -> Vec<u8> {
    match csi_modifier(mods) {
        // xterm parameter: 1 + shift + 2·alt + 4·ctrl, emitted as a decimal byte
        // (2..=8), so it is always a single ASCII digit here.
        Some(m) => vec![0x1b, b'[', b'1', b';', b'0' + m, final_byte],
        None => vec![0x1b, b'[', final_byte],
    }
}

/// The xterm CSI modifier parameter for a key chord, or `None` when no relevant
/// modifier is held (so the caller emits the shorter bare form). Encodes
/// `1 + shift + 2·alt + 4·ctrl`, i.e. a value in `2..=8`.
fn csi_modifier(mods: KeyModifiers) -> Option<u8> {
    let mut m = 0;
    if mods.contains(KeyModifiers::SHIFT) {
        m += 1;
    }
    if mods.contains(KeyModifiers::ALT) {
        m += 2;
    }
    if mods.contains(KeyModifiers::CONTROL) {
        m += 4;
    }
    if m == 0 { None } else { Some(1 + m) }
}

/// Map a Ctrl-modified character to its control byte (Ctrl-A→0x01 … Ctrl-Z→0x1a,
/// plus the standard `@ [ \ ] ^ _` controls). Anything else falls back to the
/// bare character.
fn ctrl_byte(c: char) -> Vec<u8> {
    if c.is_ascii_alphabetic() {
        vec![(c.to_ascii_uppercase() as u8) - b'A' + 1]
    } else {
        match c {
            ' ' | '@' => vec![0],
            '[' => vec![0x1b],
            '\\' => vec![0x1c],
            ']' => vec![0x1d],
            '^' => vec![0x1e],
            '_' => vec![0x1f],
            other => other.to_string().into_bytes(),
        }
    }
}

/// xterm-style function key encodings (F1–F4 use SS3, F5+ use CSI `~`).
fn function_key(n: u8) -> Vec<u8> {
    match n {
        1 => vec![0x1b, b'O', b'P'],
        2 => vec![0x1b, b'O', b'Q'],
        3 => vec![0x1b, b'O', b'R'],
        4 => vec![0x1b, b'O', b'S'],
        5 => vec![0x1b, b'[', b'1', b'5', b'~'],
        6 => vec![0x1b, b'[', b'1', b'7', b'~'],
        7 => vec![0x1b, b'[', b'1', b'8', b'~'],
        8 => vec![0x1b, b'[', b'1', b'9', b'~'],
        9 => vec![0x1b, b'[', b'2', b'0', b'~'],
        10 => vec![0x1b, b'[', b'2', b'1', b'~'],
        11 => vec![0x1b, b'[', b'2', b'3', b'~'],
        12 => vec![0x1b, b'[', b'2', b'4', b'~'],
        _ => Vec::new(),
    }
}

// ── Fake backend (tests) ─────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A backend that records what it was told to do, without any real process,
    /// so the supervisor can be tested without `claude`.
    pub struct FakeBackend {
        // (Debug is impl'd by hand below; vt100::Parser isn't Debug.)
        issue: String,
        parser: Arc<RwLock<Parser>>,
        pub inputs: Mutex<Vec<Vec<u8>>>,
        pub last_resize: Mutex<Option<(u16, u16)>>,
        lifecycle: Mutex<Lifecycle>,
        exit_notify: Arc<Notify>,
        pub shutdowns: AtomicUsize,
        /// Models an agent that ignores SIGTERM: when set, `shutdown()` leaves it
        /// Running, so only `force_kill()` (SIGKILL) ends it — which is what makes
        /// the supervisor's escalation path observable in a test.
        ignore_sigterm: bool,
        force_kills: AtomicUsize,
    }

    impl FakeBackend {
        pub fn new(issue: &str) -> Arc<Self> {
            Self::with_sigterm(issue, false)
        }

        /// Like [`new`](Self::new) but the fake refuses SIGTERM, forcing the
        /// supervisor to escalate to `force_kill()` (SIGKILL).
        pub fn new_ignoring_sigterm(issue: &str) -> Arc<Self> {
            Self::with_sigterm(issue, true)
        }

        fn with_sigterm(issue: &str, ignore_sigterm: bool) -> Arc<Self> {
            Arc::new(FakeBackend {
                issue: issue.to_string(),
                parser: Arc::new(RwLock::new(Parser::new(24, 80, 0))),
                inputs: Mutex::new(Vec::new()),
                last_resize: Mutex::new(None),
                lifecycle: Mutex::new(Lifecycle::Running),
                exit_notify: Arc::new(Notify::new()),
                shutdowns: AtomicUsize::new(0),
                ignore_sigterm,
                force_kills: AtomicUsize::new(0),
            })
        }

        /// Simulate the agent exiting with `code`.
        pub fn finish(&self, code: Option<i32>) {
            *self.lifecycle.lock().unwrap() = Lifecycle::Exited(code);
            self.exit_notify.notify_one();
        }

        pub fn shutdown_count(&self) -> usize {
            self.shutdowns.load(Ordering::SeqCst)
        }

        /// How many times `force_kill` (the SIGKILL escalation) was invoked.
        pub fn force_kill_count(&self) -> usize {
            self.force_kills.load(Ordering::SeqCst)
        }

        /// The issue this fake serves — used by tests to identify it.
        pub fn issue(&self) -> &str {
            &self.issue
        }
    }

    impl std::fmt::Debug for FakeBackend {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("FakeBackend")
                .field("issue", &self.issue)
                .finish_non_exhaustive()
        }
    }

    impl AgentBackend for FakeBackend {
        fn send_input(&self, bytes: &[u8]) -> Result<(), AgentError> {
            self.inputs.lock().unwrap().push(bytes.to_vec());
            Ok(())
        }
        fn resize(&self, rows: u16, cols: u16) -> Result<(), AgentError> {
            *self.last_resize.lock().unwrap() = Some((rows, cols));
            Ok(())
        }
        fn parser(&self) -> Arc<RwLock<Parser>> {
            Arc::clone(&self.parser)
        }
        fn status(&self) -> Lifecycle {
            // Mirror PtyAgent: degrade to Running on a poisoned lock rather than
            // panic, so the fake is faithful to the real backend's behaviour.
            self.lifecycle
                .lock()
                .map(|l| *l)
                .unwrap_or(Lifecycle::Running)
        }
        fn exit_notify(&self) -> Arc<Notify> {
            Arc::clone(&self.exit_notify)
        }
        fn shutdown(&self) {
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
            // A well-behaved agent dies on SIGTERM. Like the real PtyAgent, the
            // signal handler itself does NOT wake the exit waiter — only the
            // process actually exiting flips the status. The supervisor confirms
            // death by polling status(), not by awaiting a second notify, so we
            // deliberately don't `notify_one()` here (guards that invariant).
            // An `ignore_sigterm` fake refuses to die here, so the supervisor must
            // time out and escalate to `force_kill()`.
            if !self.ignore_sigterm {
                *self.lifecycle.lock().unwrap() = Lifecycle::Exited(None);
            }
        }
        fn force_kill(&self) {
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
            self.force_kills.fetch_add(1, Ordering::SeqCst);
            *self.lifecycle.lock().unwrap() = Lifecycle::Exited(None);
        }
        fn cwd(&self) -> &std::path::Path {
            // The fake has no real working tree; the supervisor never opens it.
            std::path::Path::new("")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_until(ms: u64, mut cond: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(ms) {
            if cond() {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        cond()
    }

    fn screen_contains(parser: &Arc<RwLock<Parser>>, needle: &str) -> bool {
        parser.read().unwrap().screen().contents().contains(needle)
    }

    fn sh(issue: &str, script: &str) -> SpawnConfig {
        SpawnConfig {
            project_id: "test-project".to_string(),
            issue: issue.to_string(),
            cwd: std::env::temp_dir(),
            program: "sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            env: Vec::new(),
            rows: 24,
            cols: 80,
        }
    }

    #[test]
    fn claude_config_picks_session_id_vs_resume() {
        let fresh = SpawnConfig::claude("proj", "ENG-1", "/wt".into(), "sid-123", false, 24, 80);
        assert_eq!(fresh.program, "claude");
        assert_eq!(fresh.args, vec!["--session-id", "sid-123"]);

        let resumed = SpawnConfig::claude("proj", "ENG-1", "/wt".into(), "sid-123", true, 24, 80)
            .arg("--permission-mode")
            .arg("default");
        assert_eq!(
            resumed.args,
            vec!["--resume", "sid-123", "--permission-mode", "default"]
        );
    }

    #[test]
    fn claude_env_pins_the_terminal_and_excludes_secrets() {
        // The allowlist must carry a stable terminal identity and must NOT pass
        // the cockpit's secrets through to an autonomous agent. LINEAR_API_KEY in
        // particular is loaded by the cockpit but has no place in the child env.
        let env = claude_env();
        let val = |k: &str| {
            env.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(val("TERM"), Some("xterm-256color"));
        assert_eq!(val("COLORTERM"), Some("truecolor"));
        assert!(
            !env.iter().any(|(k, _)| k == "LINEAR_API_KEY"),
            "the personal Linear key must never reach a spawned agent"
        );
        // Only allowlisted names (plus the two pinned terminal vars) appear.
        for (k, _) in &env {
            assert!(
                CLAUDE_ENV_ALLOW.contains(&k.as_str()) || k == "TERM" || k == "COLORTERM",
                "unexpected variable `{k}` leaked into the agent environment"
            );
        }
    }

    #[test]
    fn key_to_bytes_covers_the_keys_claude_needs() {
        use KeyCode::*;
        let plain = |code| key_to_bytes(KeyEvent::new(code, KeyModifiers::NONE));
        assert_eq!(plain(Char('a')), b"a");
        assert_eq!(plain(Enter), vec![b'\r']);
        assert_eq!(plain(Esc), vec![0x1b]);
        assert_eq!(plain(Backspace), vec![0x7f]);
        assert_eq!(plain(Up), vec![0x1b, b'[', b'A']);
        assert_eq!(plain(Tab), vec![b'\t']);
        // Ctrl-C is 0x03 — without it you can't interrupt claude.
        let ctrl_c = key_to_bytes(KeyEvent::new(Char('c'), KeyModifiers::CONTROL));
        assert_eq!(ctrl_c, vec![0x03]);
        // Alt-x prefixes ESC.
        let alt_x = key_to_bytes(KeyEvent::new(Char('x'), KeyModifiers::ALT));
        assert_eq!(alt_x, vec![0x1b, b'x']);
    }

    #[test]
    fn modified_arrows_use_the_xterm_csi_form_and_differ_from_bare() {
        use KeyCode::*;
        let key = |code, mods| key_to_bytes(KeyEvent::new(code, mods));

        // The whole point: a modified arrow must NOT collapse to its bare form,
        // or claude's editor can't tell word-wise movement from a plain cursor.
        let bare_left = key(Left, KeyModifiers::NONE);
        let ctrl_left = key(Left, KeyModifiers::CONTROL);
        assert_eq!(bare_left, vec![0x1b, b'[', b'D']);
        assert_ne!(ctrl_left, bare_left);
        // Ctrl = 1 + 4 = 5  →  ESC [ 1 ; 5 D
        assert_eq!(ctrl_left, vec![0x1b, b'[', b'1', b';', b'5', b'D']);
        // Shift = 1 + 1 = 2, Alt = 1 + 2 = 3 — each distinct on Up and Home.
        assert_eq!(
            key(Up, KeyModifiers::SHIFT),
            vec![0x1b, b'[', b'1', b';', b'2', b'A']
        );
        assert_eq!(
            key(Home, KeyModifiers::ALT),
            vec![0x1b, b'[', b'1', b';', b'3', b'H']
        );
        // Shift-Enter is the editor's newline gesture, distinct from a submit.
        assert_eq!(key(Enter, KeyModifiers::NONE), vec![b'\r']);
        assert_eq!(key(Enter, KeyModifiers::SHIFT), vec![0x1b, b'\r']);
    }

    #[test]
    fn spawn_reports_a_spawn_error_for_a_missing_program() {
        // Program-not-found is the most common real launch failure (claude not on
        // PATH); the supervisor's "spawn failed" branch depends on this surfacing.
        let (tx, _rx) = crate::event::channel();
        let cfg = SpawnConfig {
            project_id: "test-project".to_string(),
            issue: "ENG-1".to_string(),
            cwd: std::env::temp_dir(),
            program: "lindep-no-such-binary-zzz".to_string(),
            args: Vec::new(),
            env: Vec::new(),
            rows: 24,
            cols: 80,
        };
        let result = PtyAgent::spawn(cfg, tx);
        assert!(
            matches!(&result, Err(AgentError::Spawn { program, .. }) if program == "lindep-no-such-binary-zzz"),
            "a missing program yields AgentError::Spawn carrying the program name, got {result:?}"
        );
    }

    #[test]
    fn pty_agent_hosts_a_real_process_end_to_end() {
        // Proves the PTY plumbing — spawn, output, input, exit — without claude.
        let (tx, mut rx) = crate::event::channel();
        let cfg = sh(
            "ENG-1",
            "printf 'READY\\n'; head -n1 >/dev/null; printf 'BYE\\n'",
        );
        let agent = PtyAgent::spawn(cfg, tx).unwrap();

        assert!(
            wait_until(3000, || screen_contains(&agent.parser(), "READY")),
            "output pump rendered the child's banner"
        );

        agent.send_input(b"hello\n").unwrap();
        assert!(
            wait_until(3000, || screen_contains(&agent.parser(), "BYE")),
            "input reached the child, which then printed and exited"
        );

        assert!(
            wait_until(3000, || matches!(agent.status(), Lifecycle::Exited(_))),
            "status() reflects the child's exit"
        );
        let mut saw_exit = false;
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::AgentExited { issue, .. } = ev {
                assert_eq!(issue, "ENG-1");
                saw_exit = true;
            }
        }
        assert!(saw_exit, "AgentExited was emitted on the event channel");
    }

    #[test]
    fn resize_updates_the_parser_grid() {
        let (tx, _rx) = crate::event::channel();
        let agent = PtyAgent::spawn(sh("ENG-1", "sleep 5"), tx).unwrap();
        agent.resize(30, 100).unwrap();
        assert_eq!(agent.parser().read().unwrap().screen().size(), (30, 100));
        agent.shutdown();
    }

    #[test]
    fn shutdown_terminates_the_process_and_is_idempotent() {
        let (tx, _rx) = crate::event::channel();
        let agent = PtyAgent::spawn(sh("ENG-1", "sleep 30"), tx).unwrap();
        agent.shutdown();
        agent.shutdown(); // second call is a no-op, never panics
        assert!(
            wait_until(3000, || matches!(agent.status(), Lifecycle::Exited(_))),
            "the process group was signalled and the child reaped"
        );
    }

    #[tokio::test]
    async fn fake_shutdown_does_not_notify_but_finish_does() {
        // Pins the regression guard the supervisor's cancel-path tests rely on:
        // shutdown()/force_kill() flip status to Exited WITHOUT firing the exit
        // notify (only a real process exit does), while finish() does notify. If
        // anyone reverts the fake to re-notify on shutdown, this fails here —
        // deterministically — instead of hanging at a far-away join.
        use std::time::Duration;
        use tokio::time::timeout;

        let fake = fake::FakeBackend::new("ENG-1");
        let notify = fake.exit_notify();

        fake.shutdown();
        assert!(matches!(fake.status(), Lifecycle::Exited(_)));
        // A waiter arriving after shutdown must NOT be woken by it.
        assert!(
            timeout(Duration::from_millis(200), notify.notified())
                .await
                .is_err(),
            "shutdown() must not fire the exit notify (only a real exit does)"
        );

        // finish(), modelling an actual process exit, DOES wake the waiter.
        let fake2 = fake::FakeBackend::new("ENG-2");
        let notify2 = fake2.exit_notify();
        fake2.finish(Some(0));
        assert!(
            timeout(Duration::from_millis(200), notify2.notified())
                .await
                .is_ok(),
            "finish() models a real exit and must fire the exit notify"
        );
    }

    #[test]
    fn shutdown_after_self_exit_does_not_signal_a_recycled_pid() {
        // After the child self-exits and the wait thread reaps it, the captured
        // pid (== pgid) can be recycled. A late shutdown — Drop runs on every
        // teardown — must NOT killpg() it, or it could hit a stranger's group.
        let (tx, _rx) = crate::event::channel();
        let agent = PtyAgent::spawn(sh("ENG-1", "exit 0"), tx).unwrap();
        assert!(
            wait_until(3000, || matches!(agent.status(), Lifecycle::Exited(_))),
            "the child self-exited and was reaped"
        );
        // Once reaped, shutdown() is a no-op that never signals the (now stale)
        // pid; it must also never panic.
        agent.shutdown();
        agent.force_kill();
    }

    #[test]
    fn exit_code_is_masked_to_the_low_byte() {
        // Unix wait-status carries the exit code in the low 8 bits; mask so a
        // future high-bit-packed encoding can't wrap into a misclassification.
        let (tx, _rx) = crate::event::channel();
        let agent = PtyAgent::spawn(sh("ENG-1", "exit 3"), tx).unwrap();
        assert!(
            wait_until(3000, || matches!(
                agent.status(),
                Lifecycle::Exited(Some(3))
            )),
            "exit code 3 is reported faithfully through the low-byte mask"
        );
    }
}
