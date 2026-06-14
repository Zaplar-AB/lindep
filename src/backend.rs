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
    #[error("pty error: {0}")]
    Pty(String),
    #[error("spawning `{program}`: {detail}")]
    Spawn { program: String, detail: String },
    #[error("agent i/o: {0}")]
    Io(#[source] std::io::Error),
    #[error("spawning agent thread: {0}")]
    Thread(String),
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
            issue: issue.into(),
            cwd,
            program: "claude".to_string(),
            args,
            env: Vec::new(),
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
}

/// How the supervisor (and tests) construct backends. Injecting this is what
/// lets the supervisor be exercised with a `FakeBackend` (test-only) instead of
/// real `claude`.
pub type SpawnFn =
    dyn Fn(SpawnConfig, AppEventTx) -> Result<Arc<dyn AgentBackend>, AgentError> + Send + Sync;

/// The production spawn function: host the configured command on a real PTY.
pub fn pty_spawn() -> Arc<SpawnFn> {
    Arc::new(PtyAgent::spawn)
}

// ── PtyAgent — the real backend ──────────────────────────────────────────────

/// Hosts one interactive CLI on a pseudo-terminal.
pub struct PtyAgent {
    issue: String,
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
            .map_err(|e| AgentError::Pty(e.to_string()))?;

        let mut cmd = CommandBuilder::new(&cfg.program);
        cmd.cwd(&cfg.cwd);
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
                detail: e.to_string(),
            })?;
        let pid = child.process_id();
        let killer = child.clone_killer();
        // Drop our handle to the slave so the master observes EOF the moment the
        // child (the last slave holder) exits — that's what ends the read pump.
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| AgentError::Pty(e.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| AgentError::Pty(e.to_string()))?;

        let parser = Arc::new(RwLock::new(Parser::new(rows, cols, SCROLLBACK)));
        let lifecycle = Arc::new(Mutex::new(Lifecycle::Running));
        let exit_notify = Arc::new(Notify::new());

        // Output pump.
        {
            let parser = Arc::clone(&parser);
            let events = events.clone();
            let issue = cfg.issue.clone();
            thread::Builder::new()
                .name(format!("pty-read-{issue}"))
                .spawn(move || read_pump(reader, &parser, &events, &issue))
                .map_err(|e| AgentError::Thread(e.to_string()))?;
        }
        // Wait / reap.
        {
            let events = events.clone();
            let issue = cfg.issue.clone();
            let lifecycle = Arc::clone(&lifecycle);
            let exit_notify = Arc::clone(&exit_notify);
            thread::Builder::new()
                .name(format!("pty-wait-{issue}"))
                .spawn(move || {
                    let code = child.wait().ok().map(|s| s.exit_code() as i32);
                    if let Ok(mut l) = lifecycle.lock() {
                        *l = Lifecycle::Exited(code);
                    }
                    let _ = events.send(AppEvent::AgentExited { issue, code });
                    exit_notify.notify_one();
                })
                .map_err(|e| AgentError::Thread(e.to_string()))?;
        }

        Ok(Arc::new(PtyAgent {
            issue: cfg.issue,
            parser,
            writer: Mutex::new(writer),
            master: Mutex::new(pair.master),
            pid,
            killer: Mutex::new(Some(killer)),
            lifecycle,
            exit_notify,
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
        // Both halves must learn the size, or claude's UI and our render diverge.
        if let Ok(mut parser) = self.parser.write() {
            parser.screen_mut().set_size(rows, cols);
        }
        let master = self.master.lock().map_err(|_| AgentError::Poisoned)?;
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| AgentError::Pty(e.to_string()))
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
        if let Some(pid) = self.pid {
            // SAFETY: a plain libc call signalling our own child's process
            // group; harmless ESRCH if it has already exited.
            unsafe {
                libc::killpg(pid as libc::pid_t, libc::SIGTERM);
            }
        }
        // Portable fallback (and the whole story on Windows): kill the child.
        if let Ok(mut killer) = self.killer.lock()
            && let Some(killer) = killer.as_mut()
        {
            let _ = killer.kill();
        }
    }

    fn force_kill(&self) {
        // SIGKILL the whole group — unblockable, used only after a graceful
        // shutdown's grace period has elapsed.
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            // SAFETY: signalling our own child's process group.
            unsafe {
                libc::killpg(pid as libc::pid_t, libc::SIGKILL);
            }
        }
        if let Ok(mut killer) = self.killer.lock()
            && let Some(killer) = killer.as_mut()
        {
            let _ = killer.kill();
        }
    }
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
    issue: &str,
) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break, // EOF: the child is gone
            Ok(n) => {
                if let Ok(mut parser) = parser.write() {
                    parser.process(&buf[..n]);
                }
                let _ = events.send(AppEvent::AgentOutput {
                    issue: issue.to_string(),
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
        // like claude expect exactly that.
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => vec![0x1b, b'[', b'Z'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Left => csi(b'D'),
        KeyCode::Right => csi(b'C'),
        KeyCode::Up => csi(b'A'),
        KeyCode::Down => csi(b'B'),
        KeyCode::Home => csi(b'H'),
        KeyCode::End => csi(b'F'),
        KeyCode::PageUp => vec![0x1b, b'[', b'5', b'~'],
        KeyCode::PageDown => vec![0x1b, b'[', b'6', b'~'],
        KeyCode::Delete => vec![0x1b, b'[', b'3', b'~'],
        KeyCode::Insert => vec![0x1b, b'[', b'2', b'~'],
        KeyCode::F(n) => function_key(n),
        _ => Vec::new(), // CapsLock, media keys, … carry nothing for the PTY
    }
}

/// `ESC [ <final>` — the CSI prefix shared by the arrow / Home / End keys.
fn csi(final_byte: u8) -> Vec<u8> {
    vec![0x1b, b'[', final_byte]
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
    }

    impl FakeBackend {
        pub fn new(issue: &str) -> Arc<Self> {
            Arc::new(FakeBackend {
                issue: issue.to_string(),
                parser: Arc::new(RwLock::new(Parser::new(24, 80, 0))),
                inputs: Mutex::new(Vec::new()),
                last_resize: Mutex::new(None),
                lifecycle: Mutex::new(Lifecycle::Running),
                exit_notify: Arc::new(Notify::new()),
                shutdowns: AtomicUsize::new(0),
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
            *self.lifecycle.lock().unwrap()
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
            *self.lifecycle.lock().unwrap() = Lifecycle::Exited(None);
        }
        fn force_kill(&self) {
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
            *self.lifecycle.lock().unwrap() = Lifecycle::Exited(None);
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
        let fresh = SpawnConfig::claude("ENG-1", "/wt".into(), "sid-123", false, 24, 80);
        assert_eq!(fresh.program, "claude");
        assert_eq!(fresh.args, vec!["--session-id", "sid-123"]);

        let resumed = SpawnConfig::claude("ENG-1", "/wt".into(), "sid-123", true, 24, 80)
            .arg("--permission-mode")
            .arg("default");
        assert_eq!(
            resumed.args,
            vec!["--resume", "sid-123", "--permission-mode", "default"]
        );
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
}
