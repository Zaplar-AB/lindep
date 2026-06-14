//! lindep — draw Linear issue dependencies in the terminal.
//!
//! An interactive TUI that shows, for one Linear project, what each issue is
//! blocked by and what it blocks. Authenticates with a personal API key in
//! `LINEAR_API_KEY`; `--demo` runs on a synthetic graph with no key.

mod app;
mod demo;
mod linear;
mod model;
mod picker;
mod theme;
mod ui;

use std::io;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use ratatui::DefaultTerminal;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{self, Event};

use app::App;
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
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app);
    ratatui::restore();
    result
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        original(info);
    }));
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App) -> io::Result<()> {
    // The view is a pure function of state, and state only changes on a key or a
    // resize — so repaint only after one of those, never on the idle poll timeout.
    // (Without this the frame re-rendered ~5×/s forever, recomputing graph metrics
    // and keeping a core warm while the user did nothing.)
    terminal.draw(|frame| ui::draw(app, frame))?;
    while !app.should_quit {
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) => app.on_key(key),
            Event::Resize(_, _) => {}
            _ => continue, // mouse / focus / paste change nothing on screen
        }
        terminal.draw(|frame| ui::draw(app, frame))?;
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
