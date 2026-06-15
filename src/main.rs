//! lindep — draw Linear issue dependencies in the terminal.
//!
//! An interactive TUI that shows, for one Linear project, what each issue is
//! blocked by and what it blocks. Authenticates with a personal API key in
//! `LINEAR_API_KEY`; `--demo` runs on a synthetic graph with no key.

mod app;
mod demo;
mod event;
mod keymap;
mod linear;
mod model;
mod picker;
mod theme;
mod ui;
// Multi-agent spine.
mod backend;
mod notify;
mod session;
mod supervisor;
mod worktree;

use std::io;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use ratatui::DefaultTerminal;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event as term_event;
use ratatui::crossterm::event::Event;

use app::App;
use event::AppEventRx;
use linear::{Client, ProjectRef};

#[derive(Parser)]
#[command(
    name = "lindep",
    about = "Draw Linear issue dependencies in the terminal",
    long_about = "Interactive terminal view of a Linear project's dependency graph.\n\
                  Set LINEAR_API_KEY (a personal lin_api_… key) or pass --demo."
)]
struct Cli {
    /// Project name, or a unique substring of it. Omit to choose from a list.
    project: Option<String>,

    /// Use a built-in synthetic graph — no Linear API key required.
    #[arg(long)]
    demo: bool,

    /// Open directly in the layered graph overview instead of the focus lens.
    #[arg(long)]
    graph: bool,

    /// List every project visible to your API key, then exit.
    #[arg(long)]
    list: bool,

    /// Render one frame as plain text and exit (screenshots / CI). Optional
    /// size, e.g. --snapshot 120x40.
    #[arg(long, num_args = 0..=1, default_missing_value = "120x40", value_name = "WxH")]
    snapshot: Option<String>,

    /// Internal: read a Claude hook's JSON from stdin and POST it to the
    /// cockpit's endpoint on the given loopback port, then exit. This is how
    /// `lindep` wires itself as its own hook forwarder; not for direct use.
    #[arg(long, hide = true, value_name = "PORT")]
    hook_forward: Option<u16>,
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("lindep: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn real_main() -> Result<(), String> {
    load_env();
    let cli = Cli::parse();

    // Hook-forwarder fast path: no TUI, no Linear — just relay stdin to the
    // cockpit's loopback endpoint and exit. `claude` invokes this for us.
    if let Some(port) = cli.hook_forward {
        return notify::forward(port).map_err(|e| e.to_string());
    }

    // --list is a quick, key-only path.
    if cli.list {
        let client = Client::new(require_key()?);
        let mut projects = client.list_projects()?;
        projects.sort_by_key(|a| a.name.to_lowercase());
        println!("Projects ({}):", projects.len());
        for p in projects {
            println!("  {}", p.name);
        }
        return Ok(());
    }

    // Headless snapshot path: no TTY, so a project must be named explicitly.
    if let Some(spec) = &cli.snapshot {
        let graph = if cli.demo {
            demo::graph()
        } else {
            let client = Client::new(require_key()?);
            let name = cli
                .project
                .as_deref()
                .ok_or("for --snapshot, name a project (or use --demo)")?;
            client.fetch_graph(&client.resolve_project(name)?)?
        };
        if graph.is_empty() {
            return Err("no issues found for that project".into());
        }
        let mut app = App::new(graph);
        if cli.graph {
            app.mode = app::Mode::Overview;
        }
        let (w, h) = parse_size(spec);
        print!("{}", render_snapshot(&mut app, w, h)?);
        return Ok(());
    }

    // Interactive path. Restore the terminal cleanly even on panic.
    install_panic_hook();

    let graph = if cli.demo {
        demo::graph()
    } else {
        let client = Client::new(require_key()?);
        let Some(project) = resolve_or_pick(&client, cli.project.as_deref())? else {
            return Ok(()); // user quit the picker
        };
        eprintln!("Loading {}…", project.name);
        client.fetch_graph(&project)?
    };

    if graph.is_empty() {
        return Err("no issues found for that project".into());
    }

    let mut app = App::new(graph);
    if cli.graph {
        app.mode = app::Mode::Overview;
    }
    run_tui(app).map_err(|e| e.to_string())
}

/// Load `LINEAR_API_KEY` (and anything else) from `.env`: first the current
/// directory or a parent, then `~/.config/lindep/.env` for an installed binary.
/// Variables already present in the real environment always win.
fn load_env() {
    dotenvy::dotenv().ok();
    if std::env::var_os("LINEAR_API_KEY").is_none()
        && let Some(home) = std::env::var_os("HOME")
    {
        dotenvy::from_path(Path::new(&home).join(".config/lindep/.env")).ok();
    }
}

fn require_key() -> Result<String, String> {
    validate_key(std::env::var("LINEAR_API_KEY").ok())
}

/// Validate the configured key, treating an absent, empty, or still-placeholder
/// value as "not set" so the user gets the setup hint rather than a raw 401.
fn validate_key(raw: Option<String>) -> Result<String, String> {
    let key = raw.unwrap_or_default().trim().to_string();
    if key.is_empty() || key == "lin_api_xxxxxxxx" {
        return Err("no LINEAR_API_KEY found.\n\n  \
             Create a personal key at https://linear.app/settings/api , then either\n    \
             • put it in a .env file next to where you run lindep:\n        \
             LINEAR_API_KEY=lin_api_xxxxxxxx\n    \
             • or export it:  export LINEAR_API_KEY=lin_api_xxxxxxxx\n\n  \
             Or explore the demo graph:  lindep --demo"
            .to_string());
    }
    Ok(key)
}

/// Pick the project to graph: resolve a named one, auto-select a lone project,
/// or open the interactive picker. `None` means the user quit the picker.
fn resolve_or_pick(client: &Client, name: Option<&str>) -> Result<Option<ProjectRef>, String> {
    if let Some(name) = name {
        return Ok(Some(client.resolve_project(name)?));
    }
    let mut projects = client.list_projects()?;
    match projects.len() {
        0 => Err("no projects visible to this API key".into()),
        1 => Ok(Some(projects.remove(0))),
        _ => picker::pick(projects).map_err(|e| e.to_string()),
    }
}

fn parse_size(spec: &str) -> (u16, u16) {
    spec.split_once(['x', 'X'])
        .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)))
        .unwrap_or((120, 40))
}

/// Render a single frame to an off-screen buffer and return it as text.
fn render_snapshot(app: &mut App, w: u16, h: u16) -> Result<String, String> {
    // TestBackend panics on a zero-area buffer; a live terminal is always >=1x1.
    let backend = TestBackend::new(w.max(1), h.max(1));
    let mut terminal = Terminal::new(backend).map_err(|e| e.to_string())?;
    terminal
        .draw(|frame| ui::draw(app, frame))
        .map_err(|e| e.to_string())?;
    Ok(terminal.backend().to_string())
}

fn run_tui(mut app: App) -> io::Result<()> {
    // Load the keymap from config (repo `.lindep/config.toml`, then personal
    // `~/.config/lindep/config.toml`), surfacing any problems on stderr before
    // we enter the alternate screen. Bad config never aborts — defaults stand in.
    let (km, warnings) = keymap::load(std::env::current_dir().ok().as_deref());
    for w in &warnings {
        eprintln!("lindep: config: {w}");
    }
    app.keymap = km;

    // The runtime carries every background subsystem (supervisor, hook endpoint,
    // PTY pumps); the render loop stays synchronous and on this thread.
    let runtime = event::runtime()?;
    let (tx, rx) = event::channel();

    // Stand up the agent control plane. It's best-effort: lindep also runs as a
    // plain graph viewer (--demo, a non-git directory), so if this can't start
    // the cockpit still works — just without launching agents.
    let control_plane = start_control_plane(&runtime, tx.clone());
    if let Some((handle, _)) = &control_plane {
        app.supervisor = Some(handle.clone());
    }

    // Greet the user via the event path so the footer shows the cockpit is live.
    {
        let banner = format!(
            "cockpit live · {} · {} issues — a: launch agent · ? help",
            app.graph.project,
            app.graph.len()
        );
        let tx = tx.clone();
        runtime.spawn(async move {
            let _ = tx.send(event::AppEvent::Notification(banner));
        });
    }

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app, rx);

    // Stop every agent before restoring the terminal, so no PTY process group
    // outlives the cockpit.
    if let Some((handle, join)) = control_plane {
        handle.shutdown();
        let _ = runtime.block_on(join);
    }
    ratatui::restore();
    result
}

/// Build and start the agent control plane: worktree manager, session store
/// (reconciled against live worktrees), hook endpoint, and the supervisor.
/// Returns `None` (degrading to a read-only viewer) outside a git repo or if the
/// loopback endpoint can't bind.
fn start_control_plane(
    runtime: &tokio::runtime::Runtime,
    events: event::AppEventTx,
) -> Option<(supervisor::SupervisorHandle, tokio::task::JoinHandle<()>)> {
    use std::sync::{Arc, Mutex};

    let repo_root = std::env::current_dir().ok()?;
    let worktree = worktree::WorktreeManager::new(&repo_root).ok()?;
    let store = Arc::new(Mutex::new(
        session::SessionStore::load(session::SessionStore::state_path(&repo_root)).ok()?,
    ));

    // On startup, drop session records whose worktree vanished while we were off.
    {
        let live: Vec<String> = worktree
            .list()
            .unwrap_or_default()
            .into_iter()
            .map(|w| w.issue)
            .collect();
        if let Ok(mut store) = store.lock() {
            store.reconcile(live);
            let _ = store.save();
        }
    }

    // The hook endpoint must bind before agents launch so their settings can
    // point at it. block_on is safe here — we're on the synchronous main thread.
    let hook_port = runtime
        .block_on(notify::serve(events.clone(), Arc::clone(&store)))
        .ok()?;

    let exe = std::env::current_exe().unwrap_or_else(|_| Path::new("lindep").to_path_buf());
    let (cols, rows) = ratatui::crossterm::terminal::size().unwrap_or((80, 24));

    let config = supervisor::SupervisorConfig {
        worktree,
        store,
        events,
        spawn: backend::pty_spawn(),
        exe,
        hook_port,
        hooks_dir: repo_root.join(".lindep").join("hooks"),
        base: "HEAD".to_string(),
        rows,
        cols,
        max_concurrent: 6,
        // Interactive agents use the normal permission flow; budget/turn caps
        // are a headless (phase-3) concern and only apply with `--print`.
        guardrails: vec!["--permission-mode".to_string(), "default".to_string()],
    };
    Some(supervisor::Supervisor::start(config, runtime.handle()))
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        original(info);
    }));
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App, mut rx: AppEventRx) -> io::Result<()> {
    use std::time::Instant;
    use tokio::sync::mpsc::error::TryRecvError;

    // Animation advances on a wall-clock cadence (~10 fps), deliberately
    // decoupled from the poll rate (which varies 16–250 ms): a spinner looks the
    // same whether we're polling fast for a live PTY or idling, and never strobes.
    const ANIM_FRAME: Duration = Duration::from_millis(100);
    let mut last_tick = Instant::now();

    // The view is a pure function of state. State changes from three sources:
    // terminal input (key/resize), background `AppEvent`s drained from the
    // channel, and the animation frame tick. We repaint only when one of those
    // actually changed something — so a fully idle cockpit, with no input, no
    // agents talking and nothing animating, still never busy-repaints (the
    // property the original key-only loop guaranteed).
    terminal.draw(|frame| ui::draw(app, frame))?;
    while !app.should_quit {
        let mut dirty = false;

        // Bound input latency with a poll timeout; this also paces how often we
        // check the event channel and the animation clock. The poll itself is
        // cheap and never repaints. We poll fast (16 ms) when a live PTY screen
        // is on screen — attached, or a chat wall of live agents — so input and
        // output feel live; at the animation cadence (100 ms) when something is
        // merely animating off to the side (a spinner in the list); and idle at
        // 250 ms when nothing moves, keeping a quiet cockpit truly quiet.
        let chats_live = app.right_view == app::RightView::Chat
            && app.mode == app::Mode::Lens
            && !app.chat_panes().is_empty();
        let timeout = if app.attached.is_some() || chats_live {
            Duration::from_millis(16)
        } else if app.is_animating() {
            ANIM_FRAME
        } else {
            Duration::from_millis(250)
        };
        if term_event::poll(timeout)? {
            match term_event::read()? {
                Event::Key(key) => {
                    app.on_key(key);
                    dirty = true;
                }
                Event::Resize(_, _) => dirty = true,
                _ => {} // mouse / focus / paste change nothing on screen
            }
        }

        // Drain every queued background event in one batch; repaint once if any
        // of them mutated visible state.
        loop {
            match rx.try_recv() {
                Ok(ev) => dirty |= app.apply_event(ev),
                Err(TryRecvError::Empty) => break,
                // All senders gone: no more background events will arrive, but
                // the user may still be driving the UI, so keep looping.
                Err(TryRecvError::Disconnected) => break,
            }
        }

        // Advance the animation frame on the wall-clock cadence while something
        // is animating, forcing one repaint per frame. A still cockpit (no live
        // agents, no flash → `is_animating()` false) never advances the frame and
        // so never repaints on its own — the idle-quiet property holds. When idle
        // we keep the clock fresh so the first frame after a lull waits a full
        // interval rather than firing instantly.
        if app.is_animating() {
            if last_tick.elapsed() >= ANIM_FRAME {
                app.tick_frame();
                last_tick = Instant::now();
                dirty = true;
            }
        } else {
            last_tick = Instant::now();
        }

        if dirty {
            terminal.draw(|frame| ui::draw(app, frame))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_renders_core_chrome() {
        let mut app = App::new(demo::graph());
        let out = render_snapshot(&mut app, 120, 40).expect("render");
        // Header + both dependency directions + a known issue are present.
        assert!(out.contains("Inference Platform"), "header missing:\n{out}");
        assert!(out.contains("UPSTREAM"), "upstream header missing");
        assert!(out.contains("DOWNSTREAM"), "downstream header missing");
        assert!(out.contains("ZAP-204"), "focus issue missing");
        assert!(out.contains("cycles"), "cycle count missing");
    }

    #[test]
    fn overview_renders_levels_and_externals() {
        let mut app = App::new(demo::graph());
        app.mode = app::Mode::Overview;
        let out = render_snapshot(&mut app, 120, 40).expect("render");
        assert!(out.contains("GRAPH OVERVIEW"));
        assert!(out.contains("L0"));
        assert!(out.contains("INFRA-77"), "external blocker missing");
    }

    #[test]
    fn fleet_overlay_marks_agents_in_header_and_list() {
        let mut app = App::new(demo::graph());
        app.fleet
            .insert("ZAP-204".into(), crate::session::AgentStatus::NeedsYou);
        app.fleet
            .insert("ZAP-201".into(), crate::session::AgentStatus::Running);
        let out = render_snapshot(&mut app, 120, 40).expect("render");
        assert!(out.contains("2 agents"), "header counts agents:\n{out}");
        assert!(out.contains("needs you"), "header flags attention");
        assert!(
            out.contains('⚑'),
            "needs-you flag is visible in the overlay"
        );
    }

    #[test]
    fn attached_view_renders_the_agent_pane_without_panic() {
        let mut app = App::new(demo::graph());
        let fake = crate::backend::fake::FakeBackend::new("ZAP-204");
        app.backends.insert(
            "ZAP-204".into(),
            fake as std::sync::Arc<dyn crate::backend::AgentBackend>,
        );
        app.attached = Some("ZAP-204".into());
        let out = render_snapshot(&mut app, 100, 30).expect("render");
        assert!(out.contains("ATTACHED"), "attach header shown:\n{out}");
        assert!(out.contains("detach"), "detach hint shown");
    }

    #[test]
    fn chat_view_renders_the_selected_agents_pane() {
        let mut app = App::new(demo::graph());
        app.right_view = app::RightView::Chat;
        let fake = crate::backend::fake::FakeBackend::new("ZAP-204");
        app.backends.insert(
            "ZAP-204".into(),
            fake as std::sync::Arc<dyn crate::backend::AgentBackend>,
        );
        app.fleet
            .insert("ZAP-204".into(), crate::session::AgentStatus::Running);
        app.root = "ZAP-204".into();
        let out = render_snapshot(&mut app, 120, 40).expect("render");
        assert!(out.contains("CHAT"), "the chat badge shows:\n{out}");
        assert!(
            out.contains("WORKING"),
            "a working agent's pane is labelled"
        );
        assert!(out.contains("ZAP-204"));
    }

    #[test]
    fn chat_view_labels_a_finished_agent_done_not_exited() {
        // Regression for the review's m3: a finished pinned agent must read DONE,
        // and a stopped/done agent must not inflate the header count.
        let mut app = App::new(demo::graph());
        app.right_view = app::RightView::Chat;
        let fake = crate::backend::fake::FakeBackend::new("ZAP-201");
        app.backends.insert(
            "ZAP-201".into(),
            fake as std::sync::Arc<dyn crate::backend::AgentBackend>,
        );
        app.fleet
            .insert("ZAP-201".into(), crate::session::AgentStatus::Done);
        app.pinned = vec!["ZAP-201".into()];
        app.root = "ZAP-201".into();
        let out = render_snapshot(&mut app, 120, 40).expect("render");
        assert!(out.contains("DONE"), "a finished agent reads DONE:\n{out}");
        assert!(!out.contains("EXITED"), "not the transient amber EXITED");
        assert!(
            !out.contains("1 agent"),
            "a finished (non-live) agent isn't counted in the header:\n{out}"
        );
    }

    #[test]
    fn chat_view_empty_state_teaches_how_to_open_an_agent() {
        let mut app = App::new(demo::graph());
        app.right_view = app::RightView::Chat; // no backends → an empty wall
        let out = render_snapshot(&mut app, 120, 40).expect("render");
        assert!(out.contains("CHAT"));
        assert!(
            out.contains("no agent"),
            "the empty state nudges opening one:\n{out}"
        );
    }

    #[test]
    fn chat_view_survives_adversarial_sizes() {
        // A multi-pane chat wall (pins + selection, incl. a dead pinned agent)
        // must not panic when the split yields tiny or zero-area panes.
        for (w, h) in [
            (0u16, 0u16),
            (1, 1),
            (3, 2),
            (5, 3),
            (20, 4),
            (44, 6),
            (80, 10),
            (120, 40),
        ] {
            let mut app = App::new(demo::graph());
            app.right_view = app::RightView::Chat;
            for key in ["ZAP-204", "ZAP-201", "ZAP-205"] {
                let fake = crate::backend::fake::FakeBackend::new(key);
                app.backends.insert(
                    key.into(),
                    fake as std::sync::Arc<dyn crate::backend::AgentBackend>,
                );
                app.fleet
                    .insert(key.into(), crate::session::AgentStatus::Running);
            }
            app.pinned = vec!["ZAP-201".into(), "ZAP-205".into()];
            app.root = "ZAP-204".into();
            render_snapshot(&mut app, w, h).expect("chat wall must not panic");
        }
    }

    #[test]
    fn help_overlay_shows_live_bindings_and_the_config_hint() {
        let mut app = App::new(demo::graph());
        app.show_help = true;
        let out = render_snapshot(&mut app, 90, 30).expect("render");
        assert!(
            out.contains("F10"),
            "help shows the (default) detach key:\n{out}"
        );
        assert!(
            out.contains("config.toml"),
            "help points at the config file"
        );
    }

    #[test]
    fn attach_pane_reflects_a_rebound_detach_key() {
        let mut app = App::new(demo::graph());
        // Rebind detach F10 → F8, as a config file would.
        app.keymap
            .apply(&[("detach".to_string(), vec!["f8".to_string()])]);
        let fake = crate::backend::fake::FakeBackend::new("ZAP-204");
        app.backends.insert(
            "ZAP-204".into(),
            fake as std::sync::Arc<dyn crate::backend::AgentBackend>,
        );
        app.attached = Some("ZAP-204".into());
        let out = render_snapshot(&mut app, 100, 30).expect("render");
        assert!(
            out.contains("F8 to detach"),
            "attach pane shows the rebound key:\n{out}"
        );
        assert!(!out.contains("F10"), "the old default key is gone");
    }

    #[test]
    fn attached_view_survives_adversarial_sizes() {
        for (w, h) in [(0u16, 0u16), (1, 1), (3, 2), (5, 3), (44, 4), (100, 30)] {
            let mut app = App::new(demo::graph());
            let fake = crate::backend::fake::FakeBackend::new("ZAP-204");
            app.backends.insert(
                "ZAP-204".into(),
                fake as std::sync::Arc<dyn crate::backend::AgentBackend>,
            );
            app.attached = Some("ZAP-204".into());
            render_snapshot(&mut app, w, h).expect("attach pane must not panic");
        }
    }

    #[test]
    fn parse_size_handles_variants() {
        assert_eq!(parse_size("100x30"), (100, 30));
        assert_eq!(parse_size("80X24"), (80, 24));
        assert_eq!(parse_size("garbage"), (120, 40));
    }

    #[test]
    fn validate_key_rejects_missing_empty_and_placeholder() {
        assert!(validate_key(None).is_err());
        assert!(validate_key(Some("   ".into())).is_err());
        assert!(validate_key(Some("lin_api_xxxxxxxx".into())).is_err());
        assert_eq!(
            validate_key(Some("  lin_api_real  ".into())).unwrap(),
            "lin_api_real"
        );
    }

    #[test]
    fn renders_at_adversarial_sizes_without_panic() {
        // Includes very narrow widths (1, 5, 9) that previously underflowed the
        // overview's cycle-line width budget, and 0x0 which is clamped to 1x1.
        for (w, h) in [
            (0u16, 0u16),
            (1, 1),
            (5, 40),
            (9, 40),
            (10, 40),
            (20, 3),
            (40, 8),
            (44, 4),
            (80, 24),
            (200, 60),
        ] {
            for mode in [app::Mode::Lens, app::Mode::Overview] {
                let mut app = App::new(demo::graph());
                app.mode = mode;
                app.show_help = true; // also exercise the overlay
                render_snapshot(&mut app, w, h).expect("must not panic");
            }
        }
    }
}
