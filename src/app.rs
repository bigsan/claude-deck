//! The deck: the list of every running session, and the viewer of one session
//! (its views, input, and the rescan loop), with the redraw loop around both.
//!
//! Read-only: nothing here writes to a session.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::text::Line;

use crate::data::Session;
use crate::kitty::Kitty;
use crate::overview::Overview;
use crate::sessions::{Target, record_of};
use crate::text::{now, one_line};
use crate::ui::*;
use crate::views::{ColumnsView, TabsView, TimelineView, View};

const VIEWS: &str = "ABC";
const VERSION: &str = concat!("claude-deck ", env!("CARGO_PKG_VERSION"));
const POLL: Duration = Duration::from_millis(250);
// Two clicks on the same cell this close together are a double-click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
// Mouse tracking in SGR encoding: presses, the wheel, and moves only while a button is held
// (to drag a scrollbar's thumb); and off again.
const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const MOUSE_OFF: &str = "\x1b[?1006l\x1b[?1002l\x1b[?1000l";
// How often to look for changes: every second while something changes, backing
// off to 2 s after 10 s of quiet and 5 s after 30 s. A scratchpad big enough that
// one look is expensive is looked at no more often than BUDGET allows.
const RESCAN: f64 = 1.0;
const BACKOFF: [(f64, f64); 2] = [(30.0, 5.0), (10.0, 2.0)];
const BUDGET: f64 = 0.03; // at most ~3% of one core on rescanning
const IDLE_REDRAW: f64 = 5.0;
// How often to check whether the followed process moved to another session (/clear).
const FOLLOW: f64 = 5.0;

static TERMINATED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_: libc::c_int) {
    TERMINATED.store(true, Ordering::SeqCst);
}

/// The terminal went away (its pane or window closed without a signal reaching us).
fn hung_up() -> bool {
    let mut fd = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
    let ready = unsafe { libc::poll(&mut fd, 1, 0) };
    ready > 0 && fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
}

/// End the process when the terminal hangs up, or when a stop signal is not obeyed.
///
/// crossterm reads a hung-up tty in a loop that never returns (each read gives 0 bytes,
/// and it stops only on "would block"), so the main loop cannot notice by itself: the
/// deck would spin at full CPU long after its pane closed.
fn watch_terminal() {
    crate::log::spawn("tty-watch", || {
        let mut asked: Option<Instant> = None;
        loop {
            std::thread::sleep(Duration::from_millis(500));
            if hung_up() {
                crate::info!("the terminal hung up: exiting");
                std::process::exit(0);
            }
            if TERMINATED.load(Ordering::SeqCst) {
                // the main loop restores the terminal and leaves; if it is stuck, go anyway
                if asked.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(2) {
                    std::process::exit(0);
                }
            }
        }
    });
}

/// The keys screen, scrolled with a scrollbar when it does not fit.
#[derive(Default)]
struct Help {
    top: usize,
    hits: Vec<(Area, Region)>,
    drag: Option<(Bar, usize)>,
}

impl Help {
    /// `height` rows of the screen `build` makes for a width, one cell narrower beside a scrollbar.
    fn render(&mut self, build: impl Fn(usize) -> Vec<Line<'static>>, cells: usize, height: usize) -> Vec<Line<'static>> {
        self.hits.clear();
        let mut lines = build(cells);
        if lines.len() > height {
            lines = build(cells.saturating_sub(1));
        }
        let w = beside_bar(cells, lines.len(), height);
        self.top = self.top.min(lines.len().saturating_sub(height));
        let mut rows = window(&lines, self.top, height, w);
        add_bar(&mut self.hits, &mut rows, w, 0, Scroll::Text, lines.len(), self.top);
        rows
    }

    /// False to close: a click does, as any key does; the wheel and the scrollbar scroll.
    fn mouse(&mut self, m: Mouse) -> bool {
        match on_bar(&mut self.drag, &self.hits, m) {
            BarUse::To(_, top) => self.top = top,
            BarUse::Held => {}
            BarUse::No => match m.press {
                Press::Wheel(d) => self.top = (self.top as isize + d * 3).max(0) as usize,
                Press::Click | Press::Double => return false,
                Press::Drag | Press::Release => {}
            },
        }
        true
    }
}

/// What a key asks of the deck.
enum Flow {
    Stay,
    Quit,
    /// back to the list of sessions
    Back,
}

/// The viewer of one session.
pub struct App {
    pub session: Session,
    ui: Ui,
    views: Vec<Box<dyn View>>,
    view: usize,
    help_on: bool,
    help: Help,
    dirty: bool,
    scan_cost: f64,
    last_change: f64,
    last_scan: f64,
    /// the Claude Code process whose session this is, followed across /clear
    pid: Option<i32>,
    /// the herdr pane running that process, asked for its pid in the background
    agent_pane: Option<String>,
    /// (a lookup is running, what the last one found)
    pid_found: std::sync::Arc<std::sync::Mutex<(bool, Option<Option<i32>>)>>,
    last_follow: f64,
    /// opened from the list of sessions, which esc and h return to
    nested: bool,
}

/// `<project> · <session id prefix>` for a session directory.
pub fn default_label(dir: &std::path::Path) -> String {
    let parent = dir.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let project = parent.rsplit('-').next().filter(|p| !p.is_empty()).unwrap_or(&parent).to_string();
    let id: String = dir.file_name().map(|n| n.to_string_lossy().chars().take(8).collect()).unwrap_or_default();
    format!("{project} · {id}")
}

impl App {
    pub fn new(dir: PathBuf, transcript: Option<PathBuf>, label: Option<String>, view: &str) -> App {
        let label = label.filter(|l| !l.is_empty()).unwrap_or_else(|| default_label(&dir));
        let view = view.to_uppercase().chars().next().and_then(|c| VIEWS.find(c)).unwrap_or(0);
        let views: Vec<Box<dyn View>> = vec![Box::new(TabsView::new()), Box::new(ColumnsView::new()), Box::new(TimelineView::new())];
        App {
            session: Session::new(dir, transcript),
            ui: Ui::new(label),
            view: view.min(views.len() - 1),
            views,
            help_on: false,
            help: Help::default(),
            dirty: true,
            scan_cost: 0.0,
            last_change: now(),
            last_scan: 0.0,
            pid: None,
            agent_pane: None,
            pid_found: Default::default(),
            last_follow: now(),
            nested: false,
        }
    }

    pub fn open(target: Target, view: &str, nested: bool) -> App {
        crate::info!("open session {} (transcript {:?}, pid {:?}, view {view})", target.dir.display(), target.transcript, target.pid);
        let mut app = App::new(target.dir, target.transcript, Some(target.label), view);
        app.pid = target.pid;
        app.nested = nested;
        app
    }

    /// Follow the Claude process in this herdr pane.
    pub fn in_pane(mut self, agent_pane: String) -> App {
        self.agent_pane = Some(agent_pane);
        self.last_follow = 0.0;
        self
    }

    /// The followed pid; looked up off the UI thread when the process is gone or not known yet.
    fn followed_pid(&mut self) -> Option<i32> {
        if self.pid.is_some_and(crate::sessions::pid_alive) {
            return self.pid;
        }
        let pane = self.agent_pane.clone()?;
        let mut found = self.pid_found.lock().ok()?;
        if let Some(pid) = found.1.take() {
            self.pid = pid;
        } else if !found.0 {
            found.0 = true;
            let slot = self.pid_found.clone();
            crate::log::spawn("pid-lookup", move || {
                let pid = crate::herdr::agent_pid(&pane);
                crate::debug!("agent pid of pane {pane}: {pid:?}");
                if let Ok(mut s) = slot.lock() {
                    *s = (false, Some(pid));
                }
            });
        }
        self.pid
    }

    fn view_letter(&self) -> String {
        VIEWS[self.view..self.view + 1].to_string()
    }

    /// Switch to the session the followed process is in now, if /clear moved it.
    fn follow(&mut self) {
        if now() - self.last_follow < FOLLOW {
            return;
        }
        self.last_follow = now();
        let Some(pid) = self.followed_pid() else { return };
        let Some(live) = record_of(pid) else { return };
        let current = self.session.dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if live.session_id == current {
            return;
        }
        let target = live.target();
        crate::info!("pid {pid} moved to session {} (/clear): following", live.session_id);
        self.session = Session::new(target.dir, target.transcript);
        self.ui.label = target.label;
        self.ui.focus = None;
        self.rescan();
        self.ui.say(format!("session moved → {}", live.session_id.chars().take(8).collect::<String>()));
    }

    // -- actions

    fn copy(&mut self) {
        let Some(item) = self.views[self.view].selection(&mut self.ui, &self.session) else { return };
        let text = match item {
            Item::Call(c) => c.input["command"].as_str().map(str::to_string).unwrap_or_else(|| c.summary.clone()),
            Item::File(f) => f.path.display().to_string(),
            Item::Task(t) => t.path.display().to_string(),
        };
        for argv in [&["pbcopy"][..], &["wl-copy"], &["xclip", "-selection", "clipboard"]] {
            let child = std::process::Command::new(argv[0])
                .args(&argv[1..])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            let Ok(mut child) = child else { continue };
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().map(|s| s.success()).unwrap_or(false) {
                self.ui.say(format!("copied · {}", one_line(&text).chars().take(60).collect::<String>()));
                return;
            }
        }
        self.ui.say("no clipboard tool (pbcopy, wl-copy, xclip)");
    }

    fn help_lines(&self, cells: usize) -> Vec<Line<'static>> {
        let view = &self.views[self.view];
        let mut lines = vec![self.ui.header(&self.session, cells), rule(cells, &format!("keys · {}", view.name()), VERSION, dim())];
        let common: &[(&str, &str)] = &[
            ("v", "next view (A → B → C)"),
            ("=", "format JSON / XML on or off"),
            ("#", "line numbers on or off"),
            ("u", "fold long output on or off"),
            ("x", "failures only"),
            ("/", "filter by text"),
            ("y", "copy"),
            ("r", "rescan now"),
            ("q", "quit"),
            ("?", "close this"),
        ];
        let back: &[(&str, &str)] = if self.nested { &[("esc h", "back to all sessions")] } else { &[] };
        let mouse: &[(&str, &str)] = if mouse_on() { &[("mouse", "wheel moves / scrolls · click picks · double-click opens · scrollbar: click pages, drag")] } else { &[] };
        for (key, what) in view.help().iter().chain([("", "")].iter()).chain(back.iter()).chain(common.iter()).chain(mouse.iter()) {
            lines.push(compose(cells, &[seg(format!("  {key}"), yellow(), 14), flex(*what, dim())], none()));
        }
        lines
    }

    // -- input

    fn handle(&mut self, key: &str) -> Flow {
        self.dirty = true;
        if self.ui.typing {
            match key {
                "\r" | "\n" => self.ui.typing = false,
                "\x1b" => {
                    self.ui.typing = false;
                    self.ui.query.clear();
                }
                "\x7f" | "\x08" => {
                    self.ui.query.pop();
                }
                k if k.chars().count() == 1 && !k.chars().next().is_some_and(char::is_control) => self.ui.query.push_str(k),
                _ => {}
            }
            return Flow::Stay;
        }
        if self.help_on {
            self.help_on = false;
            return if key == "q" { Flow::Quit } else { Flow::Stay };
        }
        if key == "\x03" || key == "\x04" {
            return Flow::Quit;
        }
        if self.views[self.view].handle(&mut self.ui, &self.session, key) {
            return Flow::Stay;
        }
        // B's first column and C's list keep these, as before there was a list to go back to
        if !self.nested && self.view > 0 && matches!(key, "h" | "\x1b[D" | "\x1b") {
            return Flow::Stay;
        }
        let filtered = !self.ui.query.is_empty() || self.ui.failed_only;
        match key {
            "q" => return Flow::Quit,
            "\x1b" | "h" | "\x1b[D" if self.nested && !(key == "\x1b" && filtered) => return Flow::Back,
            "v" | "A" | "B" | "C" => {
                self.view = if key == "v" { (self.view + 1) % self.views.len() } else { VIEWS.find(key).unwrap_or(0).min(self.views.len() - 1) };
                if let Some(focus) = self.ui.focus.clone() {
                    self.views[self.view].adopt(&mut self.ui, &self.session, &focus);
                }
                self.ui.say(format!("view {}", self.views[self.view].name()));
            }
            "x" => self.ui.failed_only = !self.ui.failed_only,
            "/" => {
                self.ui.typing = true;
                self.ui.query.clear();
            }
            "\x1b" => {
                self.ui.query.clear();
                self.ui.failed_only = false;
            }
            "y" => self.copy(),
            "=" => {
                self.ui.options.format = !self.ui.options.format;
                self.ui.say(if self.ui.options.format { "formatted (JSON, JSONL, XML)" } else { "raw" });
            }
            "#" => self.ui.options.numbers = !self.ui.options.numbers,
            "u" => {
                self.ui.options.fold = !self.ui.options.fold;
                self.ui.say(if self.ui.options.fold { "long output folded" } else { "long output unfolded" });
            }
            "r" => {
                self.rescan();
                self.ui.say(format!("rescanned · {} files in {:.0} ms", self.session.files.len(), self.scan_cost * 1000.0));
            }
            "?" => {
                self.help_on = true;
                self.help.top = 0;
            }
            _ => {}
        }
        Flow::Stay
    }

    fn mouse(&mut self, m: Mouse) {
        if self.ui.typing {
            return;
        }
        if self.help_on {
            self.help_on = self.help.mouse(m);
            self.dirty = true;
            return;
        }
        if self.views[self.view].mouse(&mut self.ui, &self.session, m) {
            self.dirty = true;
        }
    }

    // -- loop

    fn rescan(&mut self) {
        let started = Instant::now();
        let changed = self.session.refresh();
        self.scan_cost = started.elapsed().as_secs_f64();
        crate::debug!("rescan {:.1} ms: {} files, {} tasks, {} calls{}", self.scan_cost * 1000.0, self.session.files.len(), self.session.tasks.len(), self.session.calls.len(), if changed { ", changed" } else { "" });
        if changed {
            self.dirty = true;
            self.last_change = now();
        }
    }

    fn rescan_interval(&self) -> f64 {
        let quiet = now() - self.last_change;
        let interval = BACKOFF.iter().find(|(after, _)| quiet >= *after).map(|(_, wait)| *wait).unwrap_or(RESCAN);
        interval.max(self.scan_cost / BUDGET)
    }

    fn busy(&self) -> bool {
        let now = now();
        let s = &self.session;
        s.files.iter().take(50).any(|f| f.fresh(now)) || s.tasks.iter().any(|t| t.running(now)) || s.calls.values().any(|c| c.pending())
    }

    fn frame(&mut self, cells: usize, height: usize) -> Vec<Line<'static>> {
        self.ui.image_want = None;
        self.ui.hits.clear();
        self.ui.view_name = self.views[self.view].name();
        if self.help_on {
            let mut help = std::mem::take(&mut self.help);
            let lines = help.render(|w| self.help_lines(w), cells, height);
            self.help = help;
            lines
        } else {
            self.views[self.view].render(&mut self.ui, &self.session, cells, height)
        }
    }

    /// Rescan when due; true when the screen should be redrawn.
    fn tick(&mut self) -> bool {
        self.follow();
        let t = now();
        if t - self.last_scan >= self.rescan_interval() {
            self.rescan();
            self.last_scan = t;
        }
        std::mem::take(&mut self.dirty)
    }
}

/// The screen showing: the list of sessions, or one session.
pub struct Deck {
    overview: Option<Overview>,
    app: Option<App>,
    /// the view a session opens in
    view: String,
    help_on: bool,
    help: Help,
    kitty: Kitty,
    /// the last click, for telling a double-click
    last_click: Option<(Instant, u16, u16)>,
}

impl Deck {
    pub fn sessions(home: Option<String>, view: &str) -> Deck {
        Deck { overview: Some(Overview::new(home)), app: None, view: view.to_string(), help_on: false, help: Help::default(), kitty: Kitty::new(), last_click: None }
    }

    pub fn session(app: App) -> Deck {
        Deck { overview: None, view: app.view_letter(), app: Some(app), help_on: false, help: Help::default(), kitty: Kitty::new(), last_click: None }
    }

    /// One line about the session, or the list of sessions, for a pipe instead of a terminal.
    pub fn print(mut self) {
        if let Some(app) = &mut self.app {
            let s = &mut app.session;
            s.refresh();
            println!("claude-deck: {} · {} files · {} tasks · {} calls", s.dir.display(), s.files.len(), s.tasks.len(), s.calls.len());
        } else if let Some(o) = &mut self.overview {
            o.print();
        }
    }

    fn overview_help(o: &Overview, cells: usize) -> Vec<Line<'static>> {
        let mut lines = vec![rule(cells, "keys · all sessions", VERSION, dim())];
        let mouse: &[(&str, &str)] = if mouse_on() { &[("mouse", "wheel moves, or scrolls the preview under it · click picks · double-click opens · scrollbar: click pages, drag")] } else { &[] };
        for (key, what) in o.help().iter().chain(mouse.iter()) {
            lines.push(compose(cells, &[seg(format!("  {key}"), yellow(), 14), flex(*what, dim())], none()));
        }
        lines.push(blank(cells));
        lines.push(plain(cells, "  ⏸ waiting for a permission answer · busy · idle; waiting sessions come first", dim()));
        lines
    }

    fn busy(&self) -> bool {
        match (&self.app, &self.overview) {
            (Some(app), _) => app.busy(),
            (None, Some(o)) => o.busy(),
            _ => false,
        }
    }

    fn tick(&mut self) -> bool {
        match (&mut self.app, &mut self.overview) {
            (Some(app), _) => app.tick(),
            (None, Some(o)) => o.sync(),
            _ => false,
        }
    }

    /// False to quit.
    fn handle(&mut self, key: &str) -> bool {
        if let Some(app) = &mut self.app {
            return match app.handle(key) {
                Flow::Stay => true,
                Flow::Quit => false,
                Flow::Back => {
                    crate::info!("back to all sessions");
                    self.view = app.view_letter();
                    self.app = None;
                    true
                }
            };
        }
        let Some(o) = &mut self.overview else { return false };
        if self.help_on {
            self.help_on = false;
            return key != "q";
        }
        if key == "?" && !o.typing {
            self.help_on = true;
            self.help.top = 0;
            return true;
        }
        match o.handle(key) {
            crate::overview::Flow::Stay => true,
            crate::overview::Flow::Quit => false,
            crate::overview::Flow::Open(target) => {
                let mut app = App::open(target, &self.view, true);
                app.rescan();
                self.app = Some(app);
                true
            }
        }
    }

    /// A mouse event as the deck sees it: presses of the left button and wheel notches.
    fn press(&mut self, m: MouseEvent) -> Option<Mouse> {
        let press = match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let double = self.last_click.is_some_and(|(at, x, y)| at.elapsed() < DOUBLE_CLICK && (x, y) == (m.column, m.row));
                self.last_click = if double { None } else { Some((Instant::now(), m.column, m.row)) };
                if double { Press::Double } else { Press::Click }
            }
            MouseEventKind::Drag(MouseButton::Left) => Press::Drag,
            MouseEventKind::Up(MouseButton::Left) => Press::Release,
            MouseEventKind::ScrollDown => Press::Wheel(1),
            MouseEventKind::ScrollUp => Press::Wheel(-1),
            _ => return None,
        };
        Some(Mouse { press, x: m.column as usize, y: m.row as usize })
    }

    /// A click now goes to the keys screen or the filter line, not to what is under it.
    fn eats_clicks(&self) -> bool {
        match (&self.app, &self.overview) {
            (Some(app), _) => app.help_on || app.ui.typing,
            (None, Some(o)) => self.help_on || o.typing,
            _ => false,
        }
    }

    /// False to quit.
    fn mouse(&mut self, m: Mouse) -> bool {
        if self.eats_clicks() {
            // an eaten click does not start a double-click
            self.last_click = None;
        }
        if let Some(app) = &mut self.app {
            app.mouse(m);
            return true;
        }
        let Some(o) = &mut self.overview else { return false };
        if self.help_on {
            self.help_on = self.help.mouse(m);
            return true;
        }
        match o.mouse(m) {
            crate::overview::Flow::Stay => true,
            crate::overview::Flow::Quit => false,
            crate::overview::Flow::Open(target) => {
                let mut app = App::open(target, &self.view, true);
                app.rescan();
                self.app = Some(app);
                true
            }
        }
    }

    fn frame(&mut self, cells: usize, height: usize) -> (Vec<Line<'static>>, Option<(PathBuf, (usize, usize, usize, usize))>) {
        if let Some(app) = &mut self.app {
            let lines = app.frame(cells, height);
            return (lines, app.ui.image_want.clone());
        }
        let Some(o) = &mut self.overview else { return (Vec::new(), None) };
        if self.help_on {
            return (self.help.render(|w| Deck::overview_help(o, w), cells, height), None);
        }
        (o.render(cells, height), None)
    }

    fn draw(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        let started = Instant::now();
        let size = terminal.size()?;
        let (lines, image) = self.frame(size.width as usize, size.height as usize);
        let built = started.elapsed();
        terminal.draw(|f| {
            let area = f.area();
            let buf = f.buffer_mut();
            for (y, line) in lines.iter().enumerate().take(area.height as usize) {
                buf.set_line(area.x, area.y + y as u16, line, area.width);
            }
        })?;
        let drawn = started.elapsed();
        // Image escape sequences for this frame, when the image or its place changed.
        let place = match &image {
            Some((path, area)) => self.kitty.show(path, *area),
            None => self.kitty.none(),
        };
        if !place.is_empty() {
            let before = Instant::now();
            let mut out = std::io::stdout().lock();
            out.write_all(place.as_bytes())?;
            out.flush()?;
            // a write blocks until the terminal has taken it in
            crate::debug!("image escapes: {} written in {:.1} ms", crate::text::human_size(place.len() as u64), before.elapsed().as_secs_f64() * 1000.0);
        }
        crate::log::write(
            crate::log::Level::Trace,
            format_args!("frame: built {:.1} ms, drawn {:.1} ms, total {:.1} ms", built.as_secs_f64() * 1000.0, (drawn - built).as_secs_f64() * 1000.0, started.elapsed().as_secs_f64() * 1000.0),
        );
        Ok(())
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        let mut last_draw = 0.0;
        let mut dirty = true;
        loop {
            if TERMINATED.load(Ordering::SeqCst) {
                return Ok(());
            }
            dirty |= self.tick();
            let t = now();
            if dirty || t - last_draw > if self.busy() { 0.5 } else { IDLE_REDRAW } {
                self.draw(terminal)?;
                dirty = false;
                last_draw = t;
            }
            // an image waiting to be sent is looked at again soon
            let wait = if self.kitty.pending() { Duration::from_millis(40) } else { POLL };
            if !event::poll(wait)? {
                dirty |= self.kitty.pending();
                continue;
            }
            loop {
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        if let Some(name) = key_name(key) {
                            crate::log::write(crate::log::Level::Trace, format_args!("key {name:?}"));
                            if !self.handle(&name) {
                                return Ok(());
                            }
                            dirty = true;
                        }
                    }
                    Event::Mouse(m) => {
                        if let Some(m) = self.press(m) {
                            crate::log::write(crate::log::Level::Trace, format_args!("mouse {m:?}"));
                            if !self.mouse(m) {
                                return Ok(());
                            }
                            dirty = true;
                        }
                    }
                    Event::Resize(..) => {
                        // the resize clears the screen, and the image with it
                        self.kitty.force = true;
                        self.kitty.measure();
                        dirty = true;
                    }
                    _ => {}
                }
                if TERMINATED.load(Ordering::SeqCst) || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
    }
}

/// A key as the bytes a terminal sends for it, the names the views match on.
fn key_name(key: KeyEvent) -> Option<String> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    Some(match key.code {
        KeyCode::Char('c') if ctrl => "\x03".into(),
        KeyCode::Char('d') if ctrl => "\x04".into(),
        KeyCode::Char(_) if ctrl => return None,
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "\r".into(),
        KeyCode::Esc => "\x1b".into(),
        KeyCode::Tab => "\t".into(),
        KeyCode::Backspace => "\x7f".into(),
        KeyCode::Up => "\x1b[A".into(),
        KeyCode::Down => "\x1b[B".into(),
        KeyCode::Right => "\x1b[C".into(),
        KeyCode::Left => "\x1b[D".into(),
        KeyCode::PageUp => "\x1b[5~".into(),
        KeyCode::PageDown => "\x1b[6~".into(),
        _ => return None,
    })
}

/// MOUSE is not 0: the deck takes the mouse from the terminal.
fn mouse_on() -> bool {
    crate::config::get("MOUSE").trim() != "0"
}

/// Run the deck on the terminal until the user quits or the process is told to stop.
pub fn run(mut deck: Deck) -> std::io::Result<()> {
    unsafe {
        libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, on_term as *const () as libc::sighandler_t);
    }
    watch_terminal();
    crate::render::warm_up();
    let mut terminal = ratatui::init();
    deck.kitty.measure();
    let mouse = mouse_on();
    if mouse {
        // a panic leaves through ratatui's hook, which does not know about the mouse
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            print!("{MOUSE_OFF}");
            let _ = std::io::stdout().flush();
            hook(info);
        }));
    }
    // no autowrap: a full last column must not scroll the screen
    print!("\x1b[?7l{}", if mouse { MOUSE_ON } else { "" });
    let _ = std::io::stdout().flush();
    let result = deck.run(&mut terminal);
    print!("{}\x1b[?7h{}", deck.kitty.forget(), if mouse { MOUSE_OFF } else { "" });
    let _ = std::io::stdout().flush();
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn down(column: u16, row: u16) -> MouseEvent {
        MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column, row, modifiers: KeyModifiers::NONE }
    }

    fn deck() -> Deck {
        Deck::session(App::new(PathBuf::from("/nonexistent"), None, None, "A"))
    }

    #[test]
    fn a_second_click_on_the_same_cell_soon_after_is_a_double_click() {
        let mut d = deck();
        assert_eq!(d.press(down(3, 4)).map(|m| m.press), Some(Press::Click));
        assert_eq!(d.press(down(3, 4)).map(|m| m.press), Some(Press::Double));
        // a third starts over
        assert_eq!(d.press(down(3, 4)).map(|m| m.press), Some(Press::Click));
        // another cell
        assert_eq!(d.press(down(4, 4)).map(|m| m.press), Some(Press::Click));
        // too late
        d.last_click = Instant::now().checked_sub(DOUBLE_CLICK + Duration::from_millis(50)).map(|at| (at, 4, 4));
        assert_eq!(d.press(down(4, 4)).map(|m| m.press), Some(Press::Click));
        let up = MouseEvent { kind: MouseEventKind::Up(MouseButton::Left), ..down(4, 4) };
        assert_eq!(d.press(up).map(|m| m.press), Some(Press::Release));
        let right = MouseEvent { kind: MouseEventKind::Down(MouseButton::Right), ..down(4, 4) };
        assert_eq!(d.press(right), None);
    }

    #[test]
    fn the_keys_scroll_when_they_do_not_fit_and_a_click_beside_closes_them() {
        let mut d = deck();
        let app = d.app.as_mut().unwrap();
        app.handle("?");
        let lines = app.frame(80, 10);
        assert_eq!(lines.len(), 10);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.ends_with('▌'), "{text}");
        // the wheel scrolls, a click on the track pages, a click elsewhere closes
        app.mouse(Mouse { press: Press::Wheel(1), x: 10, y: 5 });
        assert_eq!(app.help.top, 3);
        app.frame(80, 10);
        app.mouse(Mouse { press: Press::Click, x: 79, y: 9 });
        assert!(app.help_on);
        // a page, or up to the end
        let total = app.help_lines(79).len();
        assert_eq!(app.help.top, 13.min(total - 10));
        app.mouse(Mouse { press: Press::Click, x: 10, y: 5 });
        assert!(!app.help_on);
        // tall enough: no scrollbar
        app.handle("?");
        let lines = app.frame(80, 60);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(crate::text::width(&text), 80);
        assert!(app.help.hits.is_empty());
    }

    #[test]
    fn a_click_that_closes_the_keys_does_not_start_a_double_click() {
        let mut d = deck();
        d.app.as_mut().unwrap().help_on = true;
        let m = d.press(down(3, 4)).unwrap();
        assert!(d.mouse(m));
        assert!(!d.app.as_ref().unwrap().help_on);
        assert_eq!(d.press(down(3, 4)).map(|m| m.press), Some(Press::Click));
        // nor one ignored while typing a filter
        d.app.as_mut().unwrap().ui.typing = true;
        let m = d.press(down(3, 4)).unwrap();
        d.mouse(m);
        assert_eq!(d.press(down(3, 4)).map(|m| m.press), Some(Press::Click));
    }
}
