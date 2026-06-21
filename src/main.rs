//! lindep — draw Linear issue dependencies in the terminal.
//!
//! An interactive TUI that shows, for one Linear project, what each issue is
//! blocked by and what it blocks. Authenticates with a personal API key in
//! `LINEAR_API_KEY`; `--demo` runs on a synthetic graph with no key.

mod app;
mod demo;
mod event;
mod key_setup;
mod keymap;
mod layout;
mod ledger;
mod linear;
mod model;
mod picker;
mod theme;
mod ui;
mod window;
// Multi-agent spine.
mod backend;
mod mirror;
mod notify;
mod onboard;
mod registry;
mod scratch;
mod session;
mod supervisor;
mod workspace;
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
use ratatui::layout::Rect;

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

    /// Open directly in the layered graph overview (a Fleet window) instead of
    /// the focus lens.
    #[arg(long)]
    graph: bool,

    /// Don't auto-resume docked agents on startup (Cockpit v3). The window layout
    /// is still restored — only the agents stay dark until you open them.
    #[arg(long)]
    no_resume: bool,

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

    /// Internal: per-run bearer token proving a forwarded hook came from this
    /// cockpit (paired with `--hook-forward`). Minted per run; not for direct use.
    #[arg(long, hide = true, value_name = "TOKEN")]
    hook_token: Option<String>,

    /// Internal (v1.6 auto-push): a `post-commit` git hook fires this to POST an
    /// `AgentCommitted` event to the cockpit. git gives a post-commit hook no
    /// stdin, so this mode synthesizes the payload from the worktree. Not for
    /// direct use.
    #[arg(long, hide = true, value_name = "PORT")]
    post_commit: Option<u16>,

    /// Internal: which repo handle a `--post-commit` event concerns (paired with
    /// `--post-commit`). Not for direct use.
    #[arg(long, hide = true, value_name = "HANDLE")]
    repo_handle: Option<String>,

    /// Request that an extra repo be pulled into this agent's workspace (v1.6 fenced
    /// lazy-pull, ENG-542). Run by the agent itself *inside* its workspace as
    /// `lindep request-repo <handle>`; reads the cockpit endpoint + project from the
    /// agent's env. Fenced to the project's candidate set — an out-of-set handle
    /// exits non-zero. Visible (not `hide`) so an agent can discover it via `--help`.
    #[arg(long, value_name = "HANDLE")]
    request_repo: Option<String>,
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
        return notify::forward(port, cli.hook_token.as_deref().unwrap_or(""))
            .map_err(|e| e.to_string());
    }

    // Post-commit forwarder fast path (v1.6 auto-push): a git post-commit hook
    // synthesizes an `AgentCommitted` event and POSTs it, then exits. No TUI.
    if let Some(port) = cli.post_commit {
        return notify::forward_post_commit(
            port,
            cli.hook_token.as_deref().unwrap_or(""),
            cli.repo_handle.as_deref().unwrap_or(""),
        )
        .map_err(|e| e.to_string());
    }

    // Request-repo fast path (v1.6 fenced lazy-pull, ENG-542): the agent runs
    // `lindep request-repo <handle>` inside its workspace. Fence to the project's
    // candidate set (non-zero exit out-of-set), else POST a `RepoRequested` event.
    if let Some(handle) = cli.request_repo.as_deref() {
        return run_request_repo(handle);
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
        // Mirror run_tui: a snapshot is a read-only render, so mark a demo run as demo
        // (not degraded) — else the M13 "⚠ agents off" chip bakes into a --demo snapshot.
        app.demo = cli.demo;
        if cli.graph {
            app.windows.open_fleet();
        }
        let (w, h) = parse_size(spec);
        print!("{}", render_snapshot(&mut app, w, h)?);
        return Ok(());
    }

    // Interactive path. Restore the terminal cleanly even on panic.
    install_panic_hook();

    let (graph, project, client, projects) = if cli.demo {
        (demo::graph(), None, None, Vec::new())
    } else {
        let client = Client::new(ensure_key_interactive()?);
        let Some(project) = resolve_or_pick(&client, cli.project.as_deref())? else {
            eprintln!(
                "lindep: no project selected — rerun lindep to pick one, or lindep --demo to explore."
            );
            return Ok(()); // user quit the picker
        };
        eprintln!("Loading {}…", project.name);
        let graph = client.fetch_graph(&project)?;
        // The full project list powers the in-cockpit switcher (Ctrl-a s).
        // Best-effort: if it fails we still run, just without switching.
        let projects = client.list_projects().unwrap_or_default();
        (
            graph,
            Some(project),
            Some(std::sync::Arc::new(client)),
            projects,
        )
    };

    let mut app = App::new(graph);
    if cli.graph {
        app.windows.open_fleet();
    }
    run_tui(app, cli.demo, project, client, projects, cli.no_resume).map_err(|e| e.to_string())
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

/// The interactive key gate. An exported / `.env`-loaded key always wins; otherwise,
/// on a real terminal, let the user paste one in-app (validated against Linear, saved
/// to `~/.config/lindep/.env`, exported for this run — see [`key_setup`]). A non-TTY
/// run (CI, a pipe) keeps the actionable stderr hint, so nothing scripted regresses.
fn ensure_key_interactive() -> Result<String, String> {
    if let Ok(key) = require_key() {
        return Ok(key);
    }
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && let Some(key) = key_setup::prompt_for_key()?
    {
        return Ok(key);
    }
    require_key() // no TTY, or the user skipped → the stderr hint (+ the --demo offer)
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
        _ => {
            let needs = projects_needing_you(&projects);
            picker::pick(projects, &needs).map_err(|e| e.to_string())
        }
    }
}

/// Which of `projects` have a persisted agent waiting on you — scanned from each
/// configured project's saved session state so the startup picker flags them with
/// the same `⚑` as the in-cockpit switcher (ENG-562). Best-effort: a project with
/// no `~/.lindep` registry entry or no state file simply isn't flagged.
fn projects_needing_you(projects: &[ProjectRef]) -> std::collections::HashSet<String> {
    use crate::session::{AgentStatus, SessionStore};
    let (registry, _warnings) = crate::registry::Registry::load();
    let mut needs = std::collections::HashSet::new();
    for p in projects {
        let Ok(descriptor) = registry.project(&p.id) else {
            continue; // not configured locally — it has no agents to need you
        };
        let state_path = registry.layout().state_path(&descriptor.handle);
        if let Ok(store) = SessionStore::open_project(&p.id, state_path)
            && store.sessions().any(|s| s.status == AgentStatus::NeedsYou)
        {
            needs.insert(p.id.clone());
        }
    }
    needs
}

fn parse_size(spec: &str) -> (u16, u16) {
    spec.split_once(['x', 'X'])
        .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)))
        .unwrap_or((120, 40))
}

/// Render a single frame to an off-screen buffer and return it as text.
fn render_snapshot(app: &mut App, w: u16, h: u16) -> Result<String, String> {
    // TestBackend panics on a zero-area buffer; a live terminal is always >=1x1.
    let (w, h) = (w.max(1), h.max(1));
    // Sync the viewport so the strip's scroll keeps the focused window in view —
    // the interactive loop does this on resize; a one-shot snapshot must too.
    app.set_viewport(Rect::new(0, 0, w, h));
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).map_err(|e| e.to_string())?;
    terminal
        .draw(|frame| ui::draw(app, frame))
        .map_err(|e| e.to_string())?;
    Ok(terminal.backend().to_string())
}

fn run_tui(
    mut app: App,
    demo: bool,
    project: Option<ProjectRef>,
    client: Option<std::sync::Arc<Client>>,
    projects: Vec<ProjectRef>,
    no_resume: bool,
) -> io::Result<()> {
    // Load the keymap from config (repo `.lindep/config.toml`, then personal
    // `~/.config/lindep/config.toml`), surfacing any problems on stderr before
    // we enter the alternate screen. Bad config never aborts — defaults stand in.
    let (km, settings, warnings) = keymap::load(std::env::current_dir().ok().as_deref());
    for w in &warnings {
        eprintln!("lindep: config: {w}");
    }
    app.keymap = km;
    app.demo = demo; // read-only viewer: refusals/banner/dispatch affordances adapt (H6)
    // Resolve the live-backend ceiling: a validated `[agents] max_concurrent`
    // override, else the compiled-in default.
    let (max_concurrent, mc_warning) = resolve_max_concurrent(settings.max_concurrent);
    if let Some(w) = mc_warning {
        eprintln!("lindep: config: {w}");
    }

    // The runtime carries every background subsystem (supervisor, hook endpoint,
    // PTY pumps); the render loop stays synchronous and on this thread.
    let runtime = event::runtime()?;
    let (tx, rx) = event::channel();

    // Stand up the agent control plane. It's best-effort: lindep also runs as a
    // plain graph viewer, so if this can't start the cockpit still works — just
    // without launching agents.
    //
    // `--demo` runs on a synthetic graph of fictional issues, so it MUST stay a
    // read-only viewer (the documented contract) even when launched from inside a
    // real git repo: arming the control plane there would let the button shell out
    // a real `git worktree add` and spawn a real `claude` for a made-up issue, and
    // reconcile/save would mutate this repo's on-disk session state. Leaving
    // `app.supervisor = None` makes the button report "control plane unavailable"
    // instead — exactly the non-git degradation path. When armed it also restores
    // the saved window layout and (unless `--no-resume`) brings docked agents back.
    let control_plane = match project.as_ref().filter(|_| control_plane_enabled(demo)) {
        Some(project) => start_control_plane(
            &runtime,
            tx.clone(),
            &mut app,
            project,
            no_resume,
            max_concurrent,
        ),
        None => None,
    };

    // Wire the in-cockpit project switcher (Ctrl-a s): it needs the Linear client +
    // runtime to fetch a target project's graph off the render thread. Wired only
    // when the control plane is actually up — switching re-emits a project's fleet
    // through the workspace, so without it switching couldn't run agents anyway.
    // start_control_plane has already set `app.active_project` (the current project).
    if let Some(client) = client.filter(|_| control_plane.is_some()) {
        app.enable_project_switching(client, runtime.handle().clone(), tx.clone(), projects);
    }

    // Greet the user via the event path so the footer shows the cockpit is live —
    // but ONLY when it actually is. Each degraded launch (project not connected, repo
    // unreachable, hook endpoint unbindable, state from a newer lindep) already
    // emitted its own specific, actionable notice from the failing path; a blanket
    // "cockpit live" here would overwrite that accurate reason and leave the user
    // believing agents work when the control plane never armed. So when degraded we
    // stay quiet and let the real reason stand. `--demo` is an intentional read-only
    // viewer, not a failure, so it keeps the live banner.
    if control_plane.is_some() || demo {
        // `--demo` is a read-only viewer — don't advertise "Enter: open agent" it
        // can't honour; point at the way to get real agents instead (H6).
        let banner = if control_plane.is_some() {
            format!(
                "cockpit live · {} · {} issues — Enter: open agent · ? help",
                app.graph.project,
                app.graph.len()
            )
        } else {
            format!(
                "read-only demo · {} · {} issues — drop --demo to run agents (needs a Linear key) · ? help",
                app.graph.project,
                app.graph.len()
            )
        };
        let tx = tx.clone();
        runtime.spawn(async move {
            let _ = tx.send(event::AppEvent::Notification(banner));
        });
    }

    // Tear the control plane down through a Drop guard so it fires on *both* the
    // normal return and a panic unwinding out of `event_loop`: a render/layout
    // panic must still escalate SIGTERM→SIGKILL and wait for every agent to die,
    // not leave that to each backend's bare-SIGTERM `Drop` (a SIGTERM-ignoring
    // claude mid-tool-run would otherwise survive the cockpit). The panic hook
    // restores the terminal; this restores the invariant that no PTY process
    // group outlives us.
    let mut guard = ControlPlaneGuard {
        plane: control_plane,
        runtime: &runtime,
    };

    let mut terminal = ratatui::init();
    enable_bracketed_paste();
    let result = event_loop(&mut terminal, &mut app, rx);

    // Capture the final window layout (notably the focus, which we don't persist
    // per-keystroke) so the next launch reopens exactly where you left off.
    if let Some(path) = app.cockpit_path.clone() {
        let _ = app.snapshot_cockpit().save(&path);
    }

    // Close any still-open ledger runs (with each agent's last-known terminal
    // status, if it has one) and persist — so a clean quit leaves no dangling
    // "running" run for the next launch to read as interrupted. The ledger is
    // keyed by (project_id, issue) and spans EVERY project, so close over the
    // project-keyed `world` and honour the `project_id` the closer passes — NOT
    // the active-only, issue-keyed `fleet`, which would stamp a backgrounded
    // project's open run with the active project's status for a shared issue id.
    if app.ledger_path.is_some() || app.layout.is_some() {
        let now = ledger::now_unix();
        let world = std::mem::take(&mut app.world);
        app.ledger.close_open(now, |project_id, issue| {
            world.get(project_id).and_then(|m| m.get(issue)).copied()
        });
        app.save_ledgers();
    }

    // Normal path: stop agents before restoring the terminal. (On a panic the
    // guard's `Drop` does the same during unwind; here we drop it explicitly so
    // teardown is ordered before `ratatui::restore`.)
    guard.shutdown();
    drop(guard);
    disable_bracketed_paste();
    ratatui::restore();
    result
}

/// How long quit waits for the agent fleet to tear down before abandoning a
/// launch wedged in a blocking git op, so a hung `git` can't freeze the terminal
/// on exit (see [`ControlPlaneGuard::shutdown`]).
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Drains the agent control plane on scope exit — normal return *or* panic
/// unwind — so a panicking render loop can't leak a live PTY process group.
/// `shutdown` is idempotent so the explicit non-panic call and the `Drop`
/// fallback don't double-tear-down.
struct ControlPlaneGuard<'a> {
    plane: Option<(workspace::WorkspaceHandle, tokio::task::JoinHandle<()>)>,
    runtime: &'a tokio::runtime::Runtime,
}

impl ControlPlaneGuard<'_> {
    /// Signal shutdown and block until every agent's process has actually been
    /// torn down. Taking the plane out makes a second call (e.g. from `Drop`
    /// after the explicit call) a no-op.
    fn shutdown(&mut self) {
        if let Some((handle, join)) = self.plane.take() {
            handle.shutdown();
            // Bound the wait. A launch wedged in a blocking `git worktree` op (a
            // stale `index.lock`, a credential prompt on a no-tty, a slow network
            // FS) must not pin quit forever behind the supervisor's `tracker.wait`
            // — that would leave the user in a frozen, raw-mode terminal, the exact
            // corrupted-exit the panic hook exists to prevent. If the grace elapses
            // we abandon the join; the terminal is restored by the caller either
            // way, and the runtime reclaims the detached blocking thread on drop.
            let _ = self
                .runtime
                .block_on(async { tokio::time::timeout(SHUTDOWN_GRACE, join).await });
        }
    }
}

impl Drop for ControlPlaneGuard<'_> {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Whether the cockpit may arm a real agent control plane. `--demo` runs on a
/// synthetic graph of fictional issues, so it must stay a read-only viewer (the
/// documented contract) even inside a real git repo — otherwise the button would
/// shell out a real `git worktree add` + `claude` for a made-up issue and mutate
/// the repo's on-disk session state. (Outside a repo `start_control_plane`
/// already returns `None`; this gate covers the demo-inside-a-repo case.)
fn control_plane_enabled(demo: bool) -> bool {
    !demo
}

/// Default ceiling on live agents the supervisor hosts at once. Cockpit v3 raised
/// this from 6 and made it overridable via `[agents] max_concurrent` in
/// `config.toml`; docking is uncapped above it, with extra docked agents shown as
/// "resuming…" cards until a slot frees.
const MAX_CONCURRENT_AGENTS: usize = 12;

/// Resolve the live-backend ceiling from an optional `[agents] max_concurrent`
/// override. A nonsensical `0` is rejected (it would let the supervisor host no
/// agents at all) and clamped up to the default, with a warning for the caller to
/// surface; absent or valid values pass through.
fn resolve_max_concurrent(setting: Option<usize>) -> (usize, Option<String>) {
    match setting {
        Some(0) => (
            MAX_CONCURRENT_AGENTS,
            Some(format!(
                "agents.max_concurrent must be ≥ 1; using default {MAX_CONCURRENT_AGENTS}"
            )),
        ),
        Some(n) => (n, None),
        None => (MAX_CONCURRENT_AGENTS, None),
    }
}

/// Build and start the agent control plane from the global registry: provision the
/// active project's repos (mirror → reference clone), re-root its worktree manager,
/// open its per-project session store (reconciled against live worktrees), bind the
/// hook endpoint, and start the workspace. Also wires the workspace handle into
/// `app`, restores the saved window layout + ledger from the project's isolated
/// `~/.lindep/projects/<handle>/` dir (pruned against the was-live resumable set),
/// and — unless `no_resume` — brings docked agents back. Returns `None` (degrading
/// to a read-only viewer) when the project isn't registered or the endpoint can't
/// bind.
///
/// v1.6: lindep runs from **anywhere**. It no longer anchors at a cwd git repo
/// (`git_toplevel` is gone); it owns the on-disk location and provisions clones
/// itself, so launching outside any repo is fully supported.
fn start_control_plane(
    runtime: &tokio::runtime::Runtime,
    events: event::AppEventTx,
    app: &mut App,
    active: &ProjectRef,
    no_resume: bool,
    max_concurrent: usize,
) -> Option<(workspace::WorkspaceHandle, tokio::task::JoinHandle<()>)> {
    use std::sync::{Arc, Mutex};

    let (mut registry, warnings) = registry::Registry::load();
    for w in warnings {
        let _ = events.send(event::AppEvent::Notification(format!("registry: {w}")));
    }
    // The active project must be registered to run agents — lindep needs to know
    // which repos it owns. If it isn't, run the onboarding wizard to connect it now
    // (it writes ~/.lindep/registry.toml); we're still pre-TUI here, so the wizard
    // owns the terminal exactly like the project picker did. Cancelling — or a write
    // that doesn't take — degrades to the read-only graph, the prior behaviour.
    if registry.project(&active.id).is_err() {
        match onboard::run(active, &registry) {
            Ok(true) => {
                let (reloaded, warnings) = registry::Registry::load();
                for w in warnings {
                    let _ = events.send(event::AppEvent::Notification(format!("registry: {w}")));
                }
                registry = reloaded;
            }
            Ok(false) => {
                let reason = format!(
                    "agents disabled: {} isn't connected to a repo — re-open it to set up, \
                     or edit ~/.lindep/registry.toml",
                    active.name
                );
                let _ = events.send(event::AppEvent::Notification(reason.clone()));
                app.degrade_reason = Some(reason);
                return None;
            }
            Err(e) => {
                let reason = format!("agents disabled: onboarding couldn't run ({e})");
                let _ = events.send(event::AppEvent::Notification(reason.clone()));
                app.degrade_reason = Some(reason);
                return None;
            }
        }
    }
    let descriptor = match registry.project(&active.id) {
        Ok(d) => d.clone(),
        Err(_) => {
            let reason = format!(
                "agents disabled: project {} is not in ~/.lindep/registry.toml",
                active.name
            );
            let _ = events.send(event::AppEvent::Notification(reason.clone()));
            app.degrade_reason = Some(reason);
            return None;
        }
    };
    let layout = registry.layout().clone();
    // Surface the layout so the disk-reclaim prompt (ENG-540, `Ctrl-a m`) can scan
    // mirrors/clones; the registry itself moves into the workspace below.
    app.layout = Some(layout.clone());
    // The switcher offers every registered project — lindep provisions clones on
    // demand, so there is no cwd gating (the v1.5 `mapped_projects` source). Taken
    // once and reused for the candidate snapshot below.
    let project_ids = registry.project_ids();
    app.mapped_projects = project_ids.iter().cloned().collect();
    // Surface each project's candidate repos for the up-front multi-select (ENG-536):
    // the registry moves into the workspace below, so the cockpit takes a snapshot
    // now — (handle, local-only, primary) per candidate, enough to render the select.
    app.project_candidates = project_ids
        .iter()
        .filter_map(|pid| {
            let primary = registry.project(pid).ok()?.primary.clone();
            let choices = registry
                .candidate_repos(pid)
                .into_iter()
                .map(|e| picker::RepoChoice {
                    handle: e.handle.clone(),
                    local: e.is_local_only(),
                    primary: e.handle == primary,
                })
                .collect();
            Some((pid.clone(), choices))
        })
        .collect();
    // Every registered repo (handle + local-only), sorted — the at-launch repo picker's
    // "add another repo" (CF-20) offers any of these the active project doesn't yet list
    // as a candidate, so one agent can be given a repo the project wasn't set up with.
    app.registered_repos = {
        let mut repos: Vec<picker::RepoChoice> = registry
            .repo_handles()
            .into_iter()
            .filter_map(|h| {
                registry.repo(&h).map(|e| picker::RepoChoice {
                    handle: e.handle.clone(),
                    local: e.is_local_only(),
                    primary: false,
                })
            })
            .collect();
        repos.sort_by(|a, b| a.handle.cmp(&b.handle));
        repos
    };
    // Snapshot id → handle so a later switch can re-point the ledger to the target's
    // own file (H3); the registry moves into the workspace below.
    app.project_handles = project_ids
        .iter()
        .filter_map(|pid| {
            registry
                .project(pid)
                .ok()
                .map(|d| (pid.clone(), d.handle.clone()))
        })
        .collect();

    // Workspace store registry: the one loopback hook endpoint resolves each hook to
    // its owning project's store through it (a hook carries only a session id / cwd,
    // never a trusted project id). `build_plane` inserts each project's store as its
    // plane builds. Bind the endpoint before any agent launches so their settings
    // can point at it — block_on is safe here on the synchronous main thread.
    let stores: workspace::StoreRegistry = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let endpoint = match runtime.block_on(notify::serve(events.clone(), Arc::clone(&stores))) {
        Ok(endpoint) => endpoint,
        // Without the hook endpoint agents can't report back, so we degrade — but say
        // why instead of vanishing silently (the cockpit otherwise just shows the
        // graph with no clue agents are off). A bind failure is usually a port clash
        // with another lindep, which editing the registry won't fix.
        Err(e) => {
            let reason = format!(
                "agents disabled: couldn't bind the local hook endpoint ({e}); another lindep may already be running"
            );
            let _ = events.send(event::AppEvent::Notification(reason.clone()));
            app.degrade_reason = Some(reason);
            return None;
        }
    };
    let notify::Endpoint {
        port: hook_port,
        token: hook_token,
    } = endpoint;

    let exe = std::env::current_exe().unwrap_or_else(|_| Path::new("lindep").to_path_buf());
    let (cols, rows) = ratatui::crossterm::terminal::size().unwrap_or((80, 24));

    // Shared workspace ingredients: every project's supervisor is built from this
    // one builder, so the live-agent cap and its counter are genuinely shared
    // across projects — N agents total, not N per project.
    let builder = workspace::PlaneBuilder {
        events: events.clone(),
        spawn: backend::pty_spawn(),
        exe,
        hook_port,
        hook_token,
        rows,
        cols,
        // Cockpit v3 uncaps docking, but live backends are bounded — default 12,
        // overridable via `[agents] max_concurrent`. Enforced workspace-wide.
        max_concurrent,
        live_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        // Interactive agents use the normal permission flow; budget/turn caps are
        // a headless (phase-3) concern and only apply with `--print`.
        guardrails: vec!["--permission-mode".to_string(), "default".to_string()],
    };

    // Build the active project's plane eagerly — provisioning its primary repo,
    // re-rooting its worktree manager, reconciling + rehydrating its store. We're on
    // the main thread at boot, before the TUI, so block on it. Other registered
    // projects' fleets start lazily on first launch/switch. The returned resumable
    // set seeds both auto-resume and the cockpit-layout restore (a docked agent only
    // comes back if it was live, never Done/Failed/Stopped).
    let plane_result = runtime.block_on(workspace::build_plane(
        runtime.handle(),
        &builder,
        &registry,
        &active.id,
        &stores,
        // We're on the synchronous main thread before the TUI starts — no render
        // loop to take footer events — so a slow first clone streams its meter
        // straight to stderr (where "Loading {name}…" already printed).
        workspace::CloneProgressOut::Stderr,
    ));
    let (plane, resumable) = match plane_result {
        Some(pr) => pr,
        None => {
            // build_plane already emitted the specific reason as a Notification, but
            // that's a transient footer the first keystroke wipes. Persist a standing
            // reason too, so the "⚠ agents off" chip + launch refusal name a real cause
            // instead of the opaque "control plane unavailable" jargon (M13). This is
            // the most common real degradation (a repo lindep can't clone/provision).
            app.degrade_reason.get_or_insert_with(|| {
                format!(
                    "agents disabled: couldn't provision {}'s repos — see the startup notice",
                    active.name
                )
            });
            return None;
        }
    };
    let mut planes = std::collections::HashMap::new();
    planes.insert(active.id.clone(), plane);

    let (ws_handle, ws_join) =
        workspace::Workspace::start(runtime.handle().clone(), builder, registry, planes, stores);
    app.active_project = active.id.clone();
    app.workspace = Some(ws_handle.clone());

    // Restore the saved window layout + ledger from this project's isolated
    // `~/.lindep/projects/<handle>/` dir, then point the cockpit at the files so the
    // render thread persists future changes. Same degrade-gracefully discipline: a
    // newer file is left untouched; a corrupt one starts fresh and is overwritten.
    let cockpit_path = layout.cockpit_path(&descriptor.handle);
    match session::CockpitState::load(&cockpit_path) {
        Ok(state) => {
            app.apply_cockpit(&state, &resumable);
            app.cockpit_path = Some(cockpit_path);
        }
        Err(e @ session::StateError::Version { .. }) => {
            let _ = events.send(event::AppEvent::Notification(format!(
                "cockpit layout is from a newer lindep; leaving it untouched ({e})"
            )));
        }
        Err(e) => {
            let _ = events.send(event::AppEvent::Notification(format!(
                "cockpit layout unreadable ({e}); starting fresh"
            )));
            app.cockpit_path = Some(cockpit_path);
        }
    }

    let ledger_path = layout.ledger_path(&descriptor.handle);
    match ledger::Ledger::load(&ledger_path) {
        Ok(l) => {
            app.ledger = l;
            app.ledger_path = Some(ledger_path);
        }
        Err(e @ session::StateError::Version { .. }) => {
            app.project_handles.remove(&active.id);
            let _ = events.send(event::AppEvent::Notification(format!(
                "agent ledger is from a newer lindep; leaving it untouched ({e})"
            )));
        }
        Err(e) => {
            let _ = events.send(event::AppEvent::Notification(format!(
                "agent ledger unreadable ({e}); starting fresh"
            )));
            app.ledger_path = Some(ledger_path);
        }
    }

    if !no_resume {
        app.begin_resume(&resumable, max_concurrent);
    }

    Some((ws_handle, ws_join))
}

/// The `lindep request-repo <handle>` CLI front (ENG-542). Reads the cockpit endpoint
/// and owning project from the agent's environment (set at spawn), then **fences** the
/// handle to the project's candidate set — exiting non-zero out-of-set so the agent
/// gets a clear rejection — before POSTing a `RepoRequested` event. The fence lives
/// here (not in the loopback `route`, which has no registry) so the agent sees the
/// verdict; the workspace re-fences on confirm (defense-in-depth against a forged POST).
fn run_request_repo(handle: &str) -> Result<(), String> {
    let port: u16 = std::env::var("LINDEP_HOOK_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or("request-repo must be run inside a lindep agent (no LINDEP_HOOK_PORT)")?;
    let token = std::env::var("LINDEP_HOOK_TOKEN").unwrap_or_default();
    let project_id =
        std::env::var("LINDEP_PROJECT").map_err(|_| "request-repo: no LINDEP_PROJECT in env")?;
    if !registry::is_safe_handle(handle) {
        return Err(format!("`{handle}` is not a valid repo handle"));
    }
    let (reg, _warnings) = registry::Registry::load();
    let candidates: Vec<String> = reg
        .candidate_repos(&project_id)
        .into_iter()
        .map(|e| e.handle)
        .collect();
    if !candidates.iter().any(|h| h == handle) {
        return Err(format!(
            "`{handle}` is not a candidate repo for this project (allowed: {})",
            candidates.join(", ")
        ));
    }
    notify::forward_request_repo(port, &token, handle).map_err(|e| e.to_string())
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        disable_bracketed_paste();
        ratatui::restore();
        original(info);
    }));
}

/// Enable terminal bracketed-paste mode (DECSET 2004) alongside the cockpit's
/// alt-screen, so a multi-line paste arrives as one `Event::Paste` we forward to the
/// focused agent verbatim — instead of the terminal delivering each line as its own
/// submit (the line-by-line paste bug). Disabled on teardown.
fn enable_bracketed_paste() {
    let _ = ratatui::crossterm::execute!(
        io::stdout(),
        ratatui::crossterm::event::EnableBracketedPaste
    );
}

fn disable_bracketed_paste() {
    let _ = ratatui::crossterm::execute!(
        io::stdout(),
        ratatui::crossterm::event::DisableBracketedPaste
    );
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App, mut rx: AppEventRx) -> io::Result<()> {
    use std::time::Instant;
    use tokio::sync::mpsc::error::TryRecvError;

    // Animation advances on a wall-clock cadence (~10 fps), deliberately
    // decoupled from the poll rate (which varies 16–250 ms): a spinner looks the
    // same whether we're polling fast for a live PTY or idling, and never strobes.
    const ANIM_FRAME: Duration = Duration::from_millis(100);
    let mut last_tick = Instant::now();

    // Seed the viewport so the visible-window set (which paces the poll loop)
    // matches what `draw` places before the first resize event arrives.
    if let Ok(size) = terminal.size() {
        app.set_viewport(Rect::new(0, 0, size.width, size.height));
    }

    // The view is a pure function of state. State changes from three sources:
    // terminal input (key/resize), background `AppEvent`s drained from the
    // channel, and the animation frame tick. We repaint only when one of those
    // actually changed something — so a fully idle cockpit, with no input, no
    // agents talking and nothing animating, still never busy-repaints (the
    // property the original key-only loop guaranteed).
    terminal.draw(|frame| ui::draw(app, frame))?;
    while !app.should_quit {
        let mut dirty = false;

        // Poll fast (16 ms) only when an interactive PTY screen is actually on
        // the visible strip — so input/output feel live without busy-repainting
        // for an agent scrolled off-screen; at the animation cadence (100 ms)
        // when something merely animates; else a short 50 ms idle so a needs-you
        // prompt lights up promptly. Only the dirty flag triggers a redraw.
        let timeout = if app.has_visible_live_agent() {
            Duration::from_millis(16)
        } else if app.is_animating() {
            ANIM_FRAME
        } else {
            Duration::from_millis(50)
        };
        if term_event::poll(timeout)? {
            match term_event::read()? {
                Event::Key(key) => {
                    app.on_key(key);
                    dirty = true;
                }
                Event::Resize(w, h) => {
                    app.set_viewport(Rect::new(0, 0, w, h));
                    dirty = true;
                }
                Event::Paste(text) => dirty |= app.forward_paste(&text),
                _ => {} // mouse / focus change nothing on screen
            }
        }

        // The `configure-project` verb (Ctrl-a o) re-opens the onboarding wizard. The
        // wizard owns the terminal (its own alternate screen), so suspend the
        // cockpit's, run it, then resume and report. It writes registry.toml; the
        // change applies on the next launch — the live workspace is untouched.
        if let Some(project) = app.take_configure_request() {
            ratatui::restore();
            let (footer, wrote) = onboard::run_for_project(&project)?;
            *terminal = ratatui::init();
            enable_bracketed_paste();
            if let Ok(size) = terminal.size() {
                app.set_viewport(Rect::new(0, 0, size.width, size.height));
            }
            // The live workspace keeps its old binding until restart, so raise a
            // standing chip when the re-config actually changed something.
            if wrote {
                app.config_restart_pending = true;
            }
            app.note_status(footer);
            dirty = true;
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
        // agents, no flash, no resume → `is_animating()` false) never advances the
        // frame and so never repaints on its own — the idle-quiet property holds.
        if app.is_animating() {
            if last_tick.elapsed() >= ANIM_FRAME {
                app.tick_frame();
                last_tick = Instant::now();
                dirty = true;
            }
        } else {
            last_tick = Instant::now();
        }

        // A requested full repaint (Ctrl-L) drops ratatui's diff baseline so every
        // cell is rewritten — the escape hatch for a stray cell a wide-glyph stagger
        // left in a PTY pane, which the per-frame diff can't otherwise know to clear.
        if app.take_force_redraw() {
            terminal.clear()?;
            dirty = true;
        }
        if dirty {
            terminal.draw(|frame| ui::draw(app, frame))?;
        }

        // Persist the window layout when a structural change asked us to. The
        // render thread is the sole cockpit-file writer, and this fires only on a
        // pin/close/layout change (never per keystroke), so the synchronous,
        // durable write is cheap. Best-effort: the layout is cosmetic, so a write
        // failure must not interrupt the session.
        if app.cockpit_dirty {
            app.cockpit_dirty = false;
            if let Some(path) = app.cockpit_path.clone() {
                let _ = app.snapshot_cockpit().save(&path);
            }
        }

        // Persist the agent ledger when a run started/ended (same cadence as the
        // cockpit layout — a lifecycle change, never a keystroke). Best-effort: the
        // ledger is view-only history, so a write failure must not end the session.
        if app.ledger_dirty {
            app.ledger_dirty = false;
            app.save_ledgers();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::AgentStatus;
    use crate::window::{CoinMode, LayoutMode, WindowKind};
    use std::sync::Arc;

    fn fake(app: &mut App, issue: &str) {
        let backend = crate::backend::fake::FakeBackend::new(issue);
        app.backends.insert(
            issue.into(),
            backend as Arc<dyn crate::backend::AgentBackend>,
        );
    }

    #[test]
    fn the_header_rolls_up_agents_across_projects() {
        // ENG-406: the header counts live agents across the WHOLE workspace — the
        // active project's fleet plus any backgrounded project's agents in `world`.
        let mut app = App::new(demo::graph());
        app.active_project = "proj-a".into();
        app.fleet.insert("ZAP-201".into(), AgentStatus::Running); // active project
        app.world
            .entry("proj-b".into())
            .or_default()
            .insert("ENG-9".into(), AgentStatus::NeedsYou); // a backgrounded project
        let out = render_snapshot(&mut app, 120, 40).expect("render");
        assert!(
            out.contains("2 agents"),
            "header rolls up agents across projects:\n{out}"
        );
        assert!(
            out.contains("needs you"),
            "the cross-project needs-you shows"
        );
    }

    #[test]
    fn the_global_all_agents_screen_lists_every_project() {
        // ENG-406: Ctrl-a a opens a third top-level surface listing every agent.
        let mut app = App::new(demo::graph());
        app.active_project = "proj-a".into();
        let issue = app.order.first().cloned().unwrap();
        app.fleet.insert(issue, AgentStatus::Running);
        app.project_list = vec![
            crate::linear::ProjectRef {
                id: "proj-a".into(),
                name: "Alpha".into(),
            },
            crate::linear::ProjectRef {
                id: "proj-b".into(),
                name: "Beta".into(),
            },
        ];
        app.world
            .entry("proj-b".into())
            .or_default()
            .insert("ENG-9".into(), AgentStatus::NeedsYou);
        // Open the global screen (its fields are public, so the test builds it).
        let rows = app.all_agents();
        let mut state = ratatui::widgets::ListState::default();
        state.select(Some(0));
        app.global_view = Some(crate::app::GlobalView { rows, state });
        let out = render_snapshot(&mut app, 120, 40).expect("render");
        assert!(
            out.contains("ALL AGENTS"),
            "the global screen title shows:\n{out}"
        );
        assert!(
            out.contains("Beta"),
            "a backgrounded project's agent is listed"
        );
        assert!(out.contains("ENG-9"), "its issue is listed");
    }

    #[test]
    fn snapshot_renders_core_chrome() {
        // The cockpit opens with a dependency window on the default selection, so
        // the header + both dependency directions + a known issue are all present.
        let mut app = App::new(demo::graph());
        let out = render_snapshot(&mut app, 120, 40).expect("render");
        assert!(out.contains("Inference Platform"), "header missing:\n{out}");
        assert!(
            out.contains("BLOCKED BY"),
            "upstream (blocked-by) header missing"
        );
        assert!(out.contains("BLOCKS"), "downstream (blocks) header missing");
        assert!(out.contains("ZAP-204"), "focus issue missing");
        assert!(out.contains("cycles"), "cycle count missing");
    }

    #[test]
    fn a_fleet_window_renders_levels_and_externals() {
        // The old `--graph` overview is now a focusable Fleet window.
        let mut app = App::new(demo::graph());
        app.windows.open_fleet();
        let out = render_snapshot(&mut app, 120, 40).expect("render");
        assert!(
            out.contains("GRAPH OVERVIEW"),
            "the fleet map renders:\n{out}"
        );
        assert!(out.contains("L0"));
        assert!(out.contains("INFRA-77"), "external blocker missing");
    }

    #[test]
    fn the_spine_bands_issues_by_readiness() {
        // ENG-558: the Issues spine is a readiness schedule — section dividers
        // NEEDS-YOU · WORKING · IDLE · READY · BLOCKED · DONE, top→bottom. Reuses
        // the existing list (no new view). Host agents on otherwise-blocked issues
        // so the agent bands appear without emptying the READY band.
        let mut app = App::new(demo::graph());
        // A live workspace so the READY lane shows its dispatch affordance (H6 gates
        // it on workspace.is_some(); the demo/degraded case is covered separately).
        app.workspace = Some(crate::workspace::WorkspaceHandle::detached());
        app.fleet.insert("ZAP-201".into(), AgentStatus::NeedsYou);
        app.fleet.insert("ZAP-205".into(), AgentStatus::Running);
        app.fleet.insert("ZAP-210".into(), AgentStatus::Idle);
        let out = render_snapshot(&mut app, 160, 48).expect("render");
        // Match the glyph-prefixed divider headers so a band label can't collide
        // with the same word used as a window-status title (e.g. "WORKING").
        let headers = [
            "⚐ NEEDS YOU",
            "◎ WORKING",
            "◯ IDLE",
            "▸ READY",
            "⊘ BLOCKED",
            "✓ DONE",
        ];
        for band in headers {
            assert!(out.contains(band), "band header {band} missing:\n{out}");
        }
        // The READY divider carries the dispatch affordance.
        assert!(out.contains("dispatch"), "ready lane hint missing:\n{out}");
        // Bands are ordered top→bottom.
        let pos = |s: &str| out.find(s).expect("band header present");
        for w in headers.windows(2) {
            assert!(pos(w[0]) < pos(w[1]), "{} above {}:\n{out}", w[0], w[1]);
        }
    }

    #[test]
    fn fleet_overlay_marks_agents_in_header_and_list() {
        let mut app = App::new(demo::graph());
        app.fleet.insert("ZAP-204".into(), AgentStatus::NeedsYou);
        app.fleet.insert("ZAP-201".into(), AgentStatus::Running);
        let out = render_snapshot(&mut app, 120, 40).expect("render");
        assert!(out.contains("2 agents"), "header counts agents:\n{out}");
        assert!(out.contains("needs you"), "header flags attention");
        assert!(out.contains('⚑'), "needs-you flag is visible");
    }

    #[test]
    fn the_ledger_overlay_lists_an_issues_agent_runs() {
        // The durable session ledger surfaced in the Ctrl-a t overlay: a completed
        // run on the selection shows its outcome and prompt count.
        let mut app = App::new(demo::graph());
        let issue = app.root.clone();
        app.ledger.begin("", &issue, "sid".into(), 1_000);
        app.ledger.note_needs_you("", &issue);
        app.ledger
            .note_terminal("", &issue, AgentStatus::Done, 1_600);
        app.show_ledger = true;
        let out = render_snapshot(&mut app, 120, 40).expect("render");
        assert!(
            out.contains("agent session ledger"),
            "the ledger overlay renders:\n{out}"
        );
        assert!(out.contains(&issue), "it names the issue");
        assert!(out.contains("done"), "it shows the run's outcome");
    }

    #[test]
    fn an_agent_window_renders_its_status_and_key() {
        let mut app = App::new(demo::graph());
        app.windows.push(
            WindowKind::Coin {
                issue: "ZAP-204".into(),
                mode: CoinMode::Chat,
            },
            true,
            None,
        ); // a focused agent tab
        fake(&mut app, "ZAP-204");
        app.fleet.insert("ZAP-204".into(), AgentStatus::Running);
        let out = render_snapshot(&mut app, 120, 40).expect("render");
        assert!(
            out.contains("WORKING"),
            "a working agent is labelled:\n{out}"
        );
        assert!(out.contains("ZAP-204"));
    }

    #[test]
    fn a_finished_agent_window_reads_done_not_exited_and_is_uncounted() {
        // A finished agent's window reads DONE (the supervisor's verdict), never
        // the transient amber EXITED, and doesn't inflate the header count.
        let mut app = App::new(demo::graph());
        app.windows.push(
            WindowKind::Coin {
                issue: "ZAP-201".into(),
                mode: CoinMode::Chat,
            },
            true,
            None,
        );
        let backend = crate::backend::fake::FakeBackend::new("ZAP-201");
        backend.finish(Some(0));
        app.backends.insert(
            "ZAP-201".into(),
            backend as Arc<dyn crate::backend::AgentBackend>,
        );
        app.fleet.insert("ZAP-201".into(), AgentStatus::Done);
        let out = render_snapshot(&mut app, 120, 40).expect("render");
        assert!(out.contains("DONE"), "a finished agent reads DONE:\n{out}");
        assert!(!out.contains("EXITED"), "not the transient amber EXITED");
        assert!(
            !out.contains("1 agent"),
            "a finished (non-live) agent isn't counted in the header:\n{out}"
        );
    }

    #[test]
    fn a_focused_chat_with_a_short_frozen_grid_pins_its_input_row_to_the_pane_bottom() {
        // ITEM 3: a focused chat whose grid is SHORTER than its pane (a frozen-short
        // EXITED screen, or a grow-then-skipped resize) must bottom-anchor so its input
        // row sits at the pane's bottom edge, never stranded mid-pane by a naive
        // top-align. We drive a deliberately short (6-row) frozen grid with a unique
        // marker on its LAST row and assert that marker lands in the LOWER half of the
        // rendered frame — which top-align (the old behavior) could not do.
        use crate::backend::AgentBackend; // brings `.parser()` into scope
        let mut app = App::new(demo::graph());
        app.windows.push(
            WindowKind::Coin {
                issue: "ZAP-204".into(),
                mode: CoinMode::Chat,
            },
            true,
            None,
        ); // focused agent tab → the big pane
        let backend = crate::backend::fake::FakeBackend::new("ZAP-204");
        // A short grid with the "input box" on its last row; the fake's resize never
        // reflows the parser, so this 6-row grid is what render sees.
        {
            let parser = backend.parser();
            let mut guard = parser.write().unwrap();
            guard.screen_mut().set_size(6, 80);
            // Move the cursor to the last grid row, column 1, and draw the marker there.
            guard.process(b"\x1b[6;1HINPUTBOX_MARK");
        }
        app.backends.insert(
            "ZAP-204".into(),
            backend as Arc<dyn crate::backend::AgentBackend>,
        );
        app.fleet.insert("ZAP-204".into(), AgentStatus::Running);
        let out = render_snapshot(&mut app, 120, 40).expect("render");
        let lines: Vec<&str> = out.lines().collect();
        let marker_row = lines
            .iter()
            .position(|l| l.contains("INPUTBOX_MARK"))
            .unwrap_or_else(|| panic!("the focused chat's input row must render:\n{out}"));
        assert!(
            marker_row > lines.len() / 2,
            "the input row must be bottom-anchored into the lower half (row {marker_row} of {}):\n{out}",
            lines.len()
        );
    }

    #[test]
    fn a_focused_exited_chat_with_a_tall_frozen_grid_keeps_its_input_row_visible() {
        // ITEM 3: an EXITED agent's resize is skipped, so a grid that froze TALLER than
        // its current pane must still bottom-anchor — the input row (grid's last row)
        // survives instead of being clipped off the bottom. Guards the taller-grid
        // branch for the exited+focused case the item names.
        use crate::backend::AgentBackend; // brings `.parser()` into scope
        let mut app = App::new(demo::graph());
        app.windows.push(
            WindowKind::Coin {
                issue: "ZAP-204".into(),
                mode: CoinMode::Chat,
            },
            true,
            None,
        );
        let backend = crate::backend::fake::FakeBackend::new("ZAP-204");
        {
            let parser = backend.parser();
            let mut guard = parser.write().unwrap();
            // 200 rows guarantees the frozen grid is taller than any sane pane; the
            // marker on the last row is what a clip would have eaten.
            guard.screen_mut().set_size(200, 80);
            guard.process(b"\x1b[1;1HTOP_OF_GRID\x1b[200;1HINPUTBOX_MARK");
        }
        backend.finish(Some(0)); // EXITED → resize is skipped, grid stays tall
        app.backends.insert(
            "ZAP-204".into(),
            backend as Arc<dyn crate::backend::AgentBackend>,
        );
        app.fleet.insert("ZAP-204".into(), AgentStatus::Done);
        let out = render_snapshot(&mut app, 120, 40).expect("render");
        assert!(
            out.contains("INPUTBOX_MARK"),
            "the input row of a tall frozen exited grid must survive:\n{out}"
        );
        assert!(
            !out.contains("TOP_OF_GRID"),
            "the bottom-anchor drops the grid's TOP rows, not its input row:\n{out}"
        );
    }

    #[test]
    fn agent_windows_survive_adversarial_sizes_in_both_layouts() {
        // A strip of several windows (spine + deps + agents) must not panic at any
        // size, in either layout — the snap-to-whole-column / mosaic geometry.
        for layout in [LayoutMode::Rail, LayoutMode::Mosaic] {
            for (w, h) in [
                (0u16, 0u16),
                (1, 1),
                (3, 2),
                (5, 3),
                (20, 4),
                (44, 6),
                (80, 10),
                (120, 40),
                (200, 60),
            ] {
                let mut app = App::new(demo::graph());
                app.windows.force_layout(layout);
                for key in ["ZAP-204", "ZAP-201", "ZAP-205"] {
                    app.windows.push(
                        WindowKind::Coin {
                            issue: key.into(),
                            mode: CoinMode::Chat,
                        },
                        true,
                        None,
                    );
                    fake(&mut app, key);
                    app.fleet.insert(key.into(), AgentStatus::Running);
                }
                render_snapshot(&mut app, w, h).expect("the strip must not panic");
            }
        }
    }

    #[test]
    fn a_docked_agent_without_a_backend_renders_a_resuming_card() {
        // A restored docked agent (Phase 5/6) has no backend until it resumes —
        // it must paint a calm card (never touch a parser/resize) and survive.
        let mut app = App::new(demo::graph());
        app.windows.push(
            WindowKind::Coin {
                issue: "ZAP-204".into(),
                mode: CoinMode::Chat,
            },
            true,
            None,
        );
        app.fleet.insert("ZAP-204".into(), AgentStatus::Idle); // rehydrated was-live
        let out = render_snapshot(&mut app, 120, 40).expect("render");
        assert!(out.contains("ZAP-204"));
        assert!(
            out.contains("resuming"),
            "a backend-less docked agent shows a resuming card:\n{out}"
        );
    }

    #[test]
    fn the_resuming_header_spinner_shows_while_resuming() {
        let mut app = App::new(demo::graph());
        app.mark_resuming_for_test("ZAP-1");
        app.mark_resuming_for_test("ZAP-2");
        app.mark_resuming_for_test("ZAP-3");
        let out = render_snapshot(&mut app, 120, 40).expect("render");
        assert!(
            out.contains("resuming 3"),
            "the resume spinner shows:\n{out}"
        );
    }

    #[test]
    fn the_hint_footer_follows_a_keymap_rebind() {
        // Regression: render_hints must read live keys, not hardcoded ones, or it
        // lies after a rebind (plan §3 Phase 2). Rebind kill x→k and check the
        // armed footer shows the new key, not the old.
        let mut app = App::new(demo::graph());
        let warnings = app.keymap.apply_verbs(&[("kill".into(), vec!["k".into()])]);
        assert!(warnings.is_empty(), "rebind kill→k is clean: {warnings:?}");
        app.prefix_armed = true; // show the armed cheat-line
        let out = render_snapshot(&mut app, 120, 40).expect("render");
        assert!(
            out.contains("K kill"),
            "the armed footer shows the rebound kill key:\n{out}"
        );
        assert!(
            !out.contains("X kill"),
            "…and not the stale default:\n{out}"
        );
    }

    #[test]
    fn draw_is_pure_under_the_rail() {
        // The render-mutation contract (plan §2): draw mutates ONLY ListState
        // offsets + preview_size — never the window vector, focus, layout, or the
        // context window's mode. Render the rail (Spine + big pane + cards) and
        // assert that structural state is byte-for-byte unchanged.
        let mut app = App::new(demo::graph());
        app.set_viewport(Rect::new(0, 0, 200, 40));
        for k in ["ZAP-1", "ZAP-2", "ZAP-3"] {
            app.windows.push(
                WindowKind::Coin {
                    issue: k.into(),
                    mode: CoinMode::Chat,
                },
                true,
                None,
            );
        }
        app.windows.force_layout(LayoutMode::Rail); // exercise the rail explicitly
        let n = app.windows.windows.len();
        let focus = app.windows.focus;
        let layout = app.windows.layout;
        let ctx = app.windows.preview();
        let backend = TestBackend::new(200, 40);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| ui::draw(&mut app, frame))
            .expect("draw");
        assert_eq!(
            app.windows.windows.len(),
            n,
            "draw must not add/remove windows"
        );
        assert_eq!(app.windows.focus, focus, "draw must not move focus");
        assert_eq!(
            app.windows.layout, layout,
            "draw must not change the layout"
        );
        assert_eq!(
            app.windows.preview(),
            ctx,
            "draw must not flip the preview's face"
        );
    }

    #[test]
    fn max_concurrent_resolves_override_and_rejects_zero() {
        assert_eq!(
            resolve_max_concurrent(None),
            (MAX_CONCURRENT_AGENTS, None),
            "no override → the compiled-in default, silently"
        );
        assert_eq!(
            resolve_max_concurrent(Some(20)),
            (20, None),
            "a valid override is honoured"
        );
        let (value, warning) = resolve_max_concurrent(Some(0));
        assert_eq!(value, MAX_CONCURRENT_AGENTS, "0 clamps up to the default");
        assert!(warning.is_some(), "0 is rejected with a warning");
    }

    #[test]
    fn help_overlay_shows_the_prefix_and_config_hint() {
        let mut app = App::new(demo::graph());
        app.show_help = true;
        let out = render_snapshot(&mut app, 90, 44).expect("render");
        assert!(
            out.contains("Ctrl-A"),
            "help shows the (default) prefix chord:\n{out}"
        );
        assert!(
            out.contains("config.toml"),
            "help points at the config file"
        );
    }

    #[test]
    fn help_overlay_names_the_three_scopes_distinctly() {
        // M5: the per-project Fleet window (g), the cross-project all-agents screen
        // (Ctrl-a a) and the next-agent walk (Ctrl-a j) used to share "agent(s)"/
        // "overview" vocabulary and read as interchangeable. The help must now name
        // each with its own lead phrase so the three are unmistakable.
        let mut app = App::new(demo::graph());
        app.show_help = true;
        // The help list wraps long descriptions, so it always scrolls — the OpenFleet
        // row sits near the top and the NextAgent / GlobalView rows near the bottom.
        // Render the top of the overlay for the Fleet label…
        let top = render_snapshot(&mut app, 120, 100).expect("render");
        assert!(
            top.contains("open the Fleet"),
            "the g jump names the per-project Fleet window:\n{top}"
        );
        // The old ambiguous wording must be gone, so the labels can't drift back.
        assert!(
            !top.contains("global all-agents screen"),
            "the ambiguous 'global all-agents screen' label is retired:\n{top}"
        );
        // …then scroll to the bottom for the lower scope rows (clamped to max_scroll).
        app.help_scroll = u16::MAX;
        let bottom = render_snapshot(&mut app, 120, 100).expect("render");
        assert!(
            bottom.contains("all agents (global)"),
            "Ctrl-a a names the cross-project all-agents screen:\n{bottom}"
        );
        assert!(
            bottom.contains("next agent"),
            "Ctrl-a j names the next-agent walk:\n{bottom}"
        );
    }

    #[test]
    fn parse_size_handles_variants() {
        assert_eq!(parse_size("100x30"), (100, 30));
        assert_eq!(parse_size("80X24"), (80, 24));
        assert_eq!(parse_size("garbage"), (120, 40));
    }

    #[test]
    fn demo_mode_never_arms_the_control_plane() {
        // The documented contract: `--demo` is a read-only graph viewer even when
        // launched from inside a real git repo. Gating on demo here is what stops
        // `run_tui` from binding the hook endpoint, reconciling/saving session
        // state, and arming the supervisor on fictional issues. A real (non-demo)
        // run is still free to stand the control plane up.
        assert!(
            !control_plane_enabled(true),
            "--demo must degrade to the read-only viewer, never arm a live cockpit"
        );
        assert!(
            control_plane_enabled(false),
            "a real run must be allowed to arm the control plane"
        );
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
            // Exercise the spine + a deps window + a fleet map + the help overlay.
            let mut app = App::new(demo::graph());
            app.windows.open_fleet();
            app.show_help = true;
            render_snapshot(&mut app, w, h).expect("must not panic");
        }
    }
}
