mod app;
mod config;
mod data;
mod herdr;
mod kitty;
mod log;
mod overview;
mod render;
mod sessions;
mod setup;
mod text;
mod ui;
mod views;

use std::io::IsTerminal;
use std::path::PathBuf;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

const USAGE: &str = "usage: claude-deck [SESSION]

  claude-deck            every running Claude Code session; enter opens one
  claude-deck SESSION    one session: a session id (or its start) or a session directory
  claude-deck setup herdr [--check]   write the herdr plugin and link it
  claude-deck setup hooks [--check]   install the permission-verdict hook in Claude Code
  claude-deck herdr ACTION            what the herdr plugin runs: event, toggle, open,
                                      open-a, open-b, open-c, close, path, reveal
  claude-deck probe DIR  the parsed session as JSON lines, for checking the reader
  claude-deck --version

Settings (log level, image cache, herdr options) are read from config.env in
$(herdr plugin config-dir bigsan.claude-deck), standalone too, and DECK_<KEY> environment
variables, e.g. DECK_LOG_LEVEL=debug; setup herdr writes a config.example.env listing them.

Inside a herdr pane the deck is opened by the plugin with DECK_SESSION_DIR set.";

fn main() {
    // a closed pipe ends the program quietly, as for any command-line tool
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let args: Vec<String> = std::env::args().skip(1).collect();
    log::init();
    info!("claude-deck {} started: {:?} (log level {})", env!("CARGO_PKG_VERSION"), args, config::get("LOG_LEVEL"));
    let view = env("DECK_VIEW").unwrap_or_else(|| "A".into());
    match args.first().map(String::as_str) {
        Some("probe") => {
            for dir in &args[1..] {
                probe(PathBuf::from(dir));
            }
            return;
        }
        Some("herdr") => std::process::exit(herdr::main(args.get(1).map(String::as_str).unwrap_or("toggle"))),
        Some("setup") => std::process::exit(setup::main(&args[1..])),
        Some("-h" | "--help" | "help") => {
            println!("claude-deck {}\n\n{USAGE}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some("-V" | "--version" | "version") => {
            println!("claude-deck {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        _ => {}
    }
    let deck = if let Some(dir) = env("DECK_SESSION_DIR") {
        // opened by the herdr plugin beside its agent pane
        let dir = PathBuf::from(dir);
        let target = sessions::Target { transcript: env("DECK_TRANSCRIPT").map(PathBuf::from), label: env("DECK_LABEL").unwrap_or_else(|| app::default_label(&dir)), pid: None, dir };
        let app = app::App::open(target, &view, false);
        app::Deck::session(match env("DECK_AGENT_PANE") {
            Some(pane) => app.in_pane(pane),
            None => app,
        })
    } else if let Some(arg) = args.first() {
        match sessions::resolve(arg) {
            Ok(target) => app::Deck::session(app::App::open(target, &view, false)),
            Err(err) => {
                eprintln!("claude-deck: {err}");
                std::process::exit(2);
            }
        }
    } else {
        let home = std::env::current_dir().ok().map(|d| d.to_string_lossy().into_owned());
        app::Deck::sessions(home, &view)
    };
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        deck.print();
        return;
    }
    if let Err(err) = app::run(deck) {
        eprintln!("claude-deck: {err}");
        std::process::exit(1);
    }
}

/// One JSON line per session and per call, for comparing with the Python reader.
fn probe(dir: PathBuf) {
    let mut s = data::Session::new(dir, None);
    s.refresh();
    println!("{}", serde_json::json!({ "files": s.files.len(), "tasks": s.tasks.len(), "live": s.live.len(), "calls": s.calls.len(), "gate": s.has_gate }));
    let mut calls: Vec<&data::Call> = s.calls.values().collect();
    calls.sort_by(|a, b| a.id.cmp(&b.id));
    for c in calls {
        let verdict = format!("{:?}", c.verdict).to_lowercase().replace("none", "");
        println!("{}", serde_json::json!({ "id": c.id, "tool": c.tool, "family": c.family.name(), "exit": c.exit, "denied": c.denied, "verdict": verdict, "task": c.task, "dur": c.duration_ms().map(|d| d.round() as i64), "outlen": c.output.chars().count(), "agent": c.agent }));
    }
}
