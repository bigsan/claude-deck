//! Every running session in one list: what each is doing and whether it waits for you.
//!
//! The list comes from the session records at once; the counts need each session's
//! transcript and gate.jsonl, which a background thread reads, one session after
//! another, and keeps reading as they grow.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ratatui::text::Line;

use crate::data::{Call, Session, Verdict};
use crate::sessions::{Live, Target, live_sessions};
use crate::text::{human_age, now, width};
use crate::ui::*;

const RELOOK: Duration = Duration::from_secs(2);
const RECENT: usize = 30;

/// What one session's transcript and gate say.
#[derive(Clone, Default)]
pub struct Stats {
    pub calls: usize,
    pub failed: usize,
    pub denied: usize,
    /// asks still open: Claude waits for a permission answer
    pub waiting: usize,
    pub running: usize,
    pub last: f64,
    pub has_gate: bool,
    /// the newest calls, output dropped
    pub recent: Vec<Call>,
}

impl Stats {
    fn of(s: &Session) -> Stats {
        let mut recent: Vec<Call> = s.calls_newest().into_iter().take(RECENT).cloned().collect();
        for call in &mut recent {
            call.output = String::new();
        }
        Stats {
            calls: s.calls.len(),
            failed: s.calls.values().filter(|c| c.failed() && !c.refused()).count(),
            denied: s.calls.values().filter(|c| c.refused()).count(),
            waiting: s.calls.values().filter(|c| c.verdict == Verdict::Wait).count(),
            running: s.calls.values().filter(|c| c.pending()).count(),
            last: s.last_activity(),
            has_gate: s.has_gate,
            recent,
        }
    }
}

#[derive(Default)]
struct Shared {
    live: Vec<Live>,
    stats: HashMap<String, Stats>,
    loaded: bool,
}

/// The background reader; stops when dropped.
struct Worker {
    shared: Arc<Mutex<Shared>>,
    generation: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
}

impl Worker {
    fn start() -> Worker {
        let shared = Arc::new(Mutex::new(Shared { live: live_sessions(), ..Default::default() }));
        let generation = Arc::new(AtomicU64::new(1));
        let stop = Arc::new(AtomicBool::new(false));
        let (s, g, x) = (shared.clone(), generation.clone(), stop.clone());
        crate::log::spawn("sessions", move || Worker::run(s, g, x));
        Worker { shared, generation, stop }
    }

    fn run(shared: Arc<Mutex<Shared>>, generation: Arc<AtomicU64>, stop: Arc<AtomicBool>) {
        let mut sessions: HashMap<String, Session> = HashMap::new();
        let mut first = true;
        while !stop.load(Ordering::Relaxed) {
            let pass = std::time::Instant::now();
            let live = if first { shared.lock().map(|s| s.live.clone()).unwrap_or_default() } else { live_sessions() };
            if let Ok(mut s) = shared.lock() {
                s.live = live.clone();
                s.stats.retain(|id, _| live.iter().any(|l| &l.session_id == id));
            }
            generation.fetch_add(1, Ordering::Relaxed);
            sessions.retain(|id, _| live.iter().any(|l| &l.session_id == id));
            for record in &live {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let session = sessions.entry(record.session_id.clone()).or_insert_with(|| {
                    let target = record.target();
                    let mut s = Session::new(target.dir, target.transcript);
                    s.scan_files = false;
                    s.light = true;
                    s
                });
                if session.refresh() || first {
                    let stats = Stats::of(session);
                    if let Ok(mut s) = shared.lock() {
                        s.stats.insert(record.session_id.clone(), stats);
                    }
                    generation.fetch_add(1, Ordering::Relaxed);
                }
            }
            if first {
                crate::info!("sessions: read {} running sessions in {:.0} ms", live.len(), pass.elapsed().as_secs_f64() * 1000.0);
                if let Ok(mut s) = shared.lock() {
                    s.loaded = true;
                }
                generation.fetch_add(1, Ordering::Relaxed);
                first = false;
            }
            let until = std::time::Instant::now() + RELOOK;
            while std::time::Instant::now() < until && !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// One row of the list.
struct Row {
    live: Live,
    stats: Option<Stats>,
}

impl Row {
    fn last(&self) -> f64 {
        self.stats.as_ref().map(|s| s.last).unwrap_or(0.0).max(self.live.updated)
    }
    fn waiting(&self) -> usize {
        self.stats.as_ref().map(|s| s.waiting).unwrap_or(0)
    }
}

pub struct Overview {
    worker: Worker,
    seen: u64,
    rows: Vec<Row>,
    loaded: bool,
    key: Option<String>,
    /// until the cursor is moved (or put on this folder's session) it stays on the top row
    pinned: bool,
    index: usize,
    top: usize,
    preview: bool,
    pub query: String,
    pub typing: bool,
    /// the directory whose newest session gets the cursor at first
    home: Option<String>,
    /// where the last frame put its rows, for clicks
    hits: Vec<(Area, Region)>,
}

const HELP: &[(&str, &str)] = &[
    ("j k", "move"),
    ("l → enter", "open the session"),
    ("g G", "first / last"),
    ("/", "filter by name or folder"),
    ("space", "preview on / off"),
    ("esc", "clear the filter"),
    ("q", "quit"),
    ("?", "close this"),
];

pub enum Flow {
    Stay,
    Quit,
    Open(Target),
}

impl Overview {
    pub fn new(home: Option<String>) -> Overview {
        let mut o = Overview {
            worker: Worker::start(),
            seen: 0,
            rows: Vec::new(),
            loaded: false,
            key: None,
            pinned: false,
            index: 0,
            top: 0,
            preview: true,
            query: String::new(),
            typing: false,
            home,
            hits: Vec::new(),
        };
        o.sync();
        o
    }

    /// Take what the worker found since the last look; true when anything changed.
    pub fn sync(&mut self) -> bool {
        let generation = self.worker.generation.load(Ordering::Relaxed);
        if generation == self.seen {
            return false;
        }
        self.seen = generation;
        let Ok(shared) = self.worker.shared.lock() else { return false };
        self.loaded = shared.loaded;
        self.rows = shared.live.iter().map(|l| Row { live: l.clone(), stats: shared.stats.get(&l.session_id).cloned() }).collect();
        drop(shared);
        // waiting for you first, then working, then the most recently active
        self.rows.sort_by(|a, b| {
            (b.waiting() > 0).cmp(&(a.waiting() > 0)).then((b.live.busy).cmp(&a.live.busy)).then(b.last().total_cmp(&a.last()))
        });
        if !self.pinned {
            if let Some(home) = &self.home {
                let mine = self.rows.iter().filter(|r| &r.live.cwd == home).max_by(|a, b| a.live.updated.total_cmp(&b.live.updated));
                if let Some(row) = mine {
                    self.key = Some(row.live.session_id.clone());
                    self.pinned = true;
                }
            }
        }
        true
    }

    pub fn busy(&self) -> bool {
        !self.loaded || self.rows.iter().any(|r| r.live.busy)
    }

    fn shown(&self) -> Vec<usize> {
        let q = self.query.to_lowercase();
        (0..self.rows.len())
            .filter(|&i| {
                let l = &self.rows[i].live;
                q.is_empty() || l.name.to_lowercase().contains(&q) || l.cwd.to_lowercase().contains(&q) || l.session_id.starts_with(&q)
            })
            .collect()
    }

    fn current(&mut self, shown: &[usize]) -> usize {
        if !self.pinned {
            self.index = 0;
            self.key = None;
        }
        let i = self
            .key
            .as_ref()
            .and_then(|k| shown.iter().position(|&r| &self.rows[r].live.session_id == k))
            .unwrap_or(self.index.min(shown.len().saturating_sub(1)));
        self.index = i;
        if let Some(&r) = shown.get(i) {
            self.key = Some(self.rows[r].live.session_id.clone());
        }
        i
    }

    fn move_to(&mut self, shown: &[usize], i: isize) {
        if shown.is_empty() {
            return;
        }
        let i = i.clamp(0, shown.len() as isize - 1) as usize;
        self.index = i;
        self.key = Some(self.rows[shown[i]].live.session_id.clone());
        self.pinned = true;
    }

    fn header(&self, cells: usize) -> Line<'static> {
        let total = self.rows.len();
        let busy = self.rows.iter().filter(|r| r.live.busy).count();
        let waiting: usize = self.rows.iter().map(Row::waiting).sum();
        let failed: usize = self.rows.iter().filter_map(|r| r.stats.as_ref()).map(|s| s.failed).sum();
        let on = |n: usize, style| if n > 0 { style } else { dim() };
        let mark = if busy > 0 { spin() } else { '⚙' };
        let loading = if self.loaded { String::new() } else { format!("{} reading ", spin()) };
        let lw = width(&loading);
        compose(
            cells,
            &[
                seg("claude ", magenta().add_modifier(ratatui::style::Modifier::BOLD), 7),
                flex(format!("all sessions · {total}"), none()),
                seg(loading, yellow(), lw),
                seg(format!("⏸{waiting} "), on(waiting, yellow()), waiting.to_string().len() + 2),
                seg(format!("{mark}{busy} "), on(busy, yellow()), busy.to_string().len() + 2),
                seg(format!("✗{failed}"), on(failed, red()), failed.to_string().len() + 1),
            ],
            none(),
        )
    }

    fn row(&self, row: &Row, cells: usize, now: f64, bg: ratatui::style::Style) -> Line<'static> {
        let l = &row.live;
        let narrow = cells < 70;
        let waiting = row.waiting();
        let mut segs = vec![if waiting > 0 {
            seg("⏸", yellow(), 2)
        } else if l.busy {
            seg(spin().to_string(), yellow(), 2)
        } else {
            seg("·", dim(), 2)
        }];
        let name = if l.name.is_empty() { l.session_id.chars().take(8).collect() } else { l.name.clone() };
        segs.push(flex(name, bold()));
        segs.push(seg(" ", none(), 1));
        if !narrow {
            segs.push(seg(l.project(), cyan(), 18));
            segs.push(seg(" ", none(), 1));
        }
        segs.push(seg(if l.busy { "busy" } else { "idle" }, if l.busy { yellow() } else { dim() }, 5));
        segs.push(seg_r(human_age(now - row.last()), dim(), 4));
        match &row.stats {
            None => segs.push(seg_r("…", dim(), if narrow { 11 } else { 19 })),
            Some(s) => {
                if !narrow {
                    segs.push(seg_r(format!("{} calls", s.calls), dim(), 10));
                }
                segs.push(seg_r(if s.failed > 0 { format!("✗{}", s.failed) } else { "·".into() }, if s.failed > 0 { red() } else { dim() }, 5));
                segs.push(seg_r(if waiting > 0 { format!("⏸{waiting}") } else { "·".into() }, if waiting > 0 { yellow() } else { dim() }, 4));
            }
        }
        segs.push(seg(" ", none(), 1));
        compose(cells, &segs, bg)
    }

    fn detail(&self, row: &Row, cells: usize) -> Vec<Line<'static>> {
        let l = &row.live;
        let sep = || ratatui::text::Span::styled(" · ", dim());
        let span = |t: String, s| ratatui::text::Span::styled(t, s);
        let mut lines = vec![
            raw(cells, vec![span(l.name.clone(), bold()), sep(), span(l.session_id.clone(), dim()), sep(), span(if l.started > 0.0 { format!("pid {} · since {}", l.pid, crate::text::hm(l.started)) } else { format!("pid {}", l.pid) }, dim())]),
            plain(cells, &l.cwd, cyan()),
        ];
        let Some(s) = &row.stats else {
            lines.push(plain(cells, "reading the transcript…", dim()));
            return lines;
        };
        let mut facts = vec![span(format!("{} calls", s.calls), none())];
        if s.running > 0 {
            facts.extend([sep(), span(format!("{} running", s.running), yellow())]);
        }
        if s.failed > 0 {
            facts.extend([sep(), span(format!("✗ {} failed", s.failed), red())]);
        }
        if s.denied > 0 {
            facts.extend([sep(), span(format!("⊘ {} denied", s.denied), red())]);
        }
        if s.waiting > 0 {
            facts.extend([sep(), span(format!("⏸ {} waiting for you", s.waiting), yellow())]);
        }
        if !s.has_gate {
            facts.extend([sep(), span("no gate.jsonl".into(), dim())]);
        }
        lines.push(raw(cells, facts));
        lines.push(rule(cells, "recent calls", "", dim()));
        lines.extend(s.recent.iter().map(|c| call_row(c, cells, s.has_gate, none())));
        lines
    }

    pub fn render(&mut self, cells: usize, height: usize) -> Vec<Line<'static>> {
        let now = now();
        let shown = self.shown();
        let i = self.current(&shown);
        let mut lines = vec![self.header(cells)];
        let body = height.saturating_sub(lines.len() + 1);
        let filter = if self.query.is_empty() && !self.typing { String::new() } else { format!("/{}{}", self.query, if self.typing { "▏" } else { "" }) };
        lines.push(rule(cells, "waiting · busy · newest first", &filter, dim()));
        let selected = shown.get(i).map(|&r| &self.rows[r]);
        let list_h = if self.preview && selected.is_some() && body >= 12 { 3.max((body - 2) * 50 / 100).min(shown.len().max(1)) } else { body.saturating_sub(1) };
        self.top = visible_top(i, self.top, list_h, shown.len());
        let mut rows: Vec<Line<'static>> =
            shown.iter().enumerate().skip(self.top).take(list_h).map(|(n, &r)| self.row(&self.rows[r], cells, now, if n == i { sel_bg() } else { none() })).collect();
        if shown.is_empty() {
            let empty = if !self.loaded { "  reading sessions…" } else if self.query.is_empty() { "  (no Claude Code session is running)" } else { "  (nothing matches)" };
            rows = vec![plain(cells, empty, dim())];
        }
        self.hits.clear();
        mark_rows(&mut self.hits, 0, lines.len(), cells, self.top, list_h, shown.len());
        lines.extend(window(&rows, 0, list_h, cells));
        if let Some(row) = selected.filter(|_| list_h + 1 < body) {
            lines.push(rule(cells, "", "", dim()));
            let detail = self.detail(row, cells);
            lines.extend(window(&detail, 0, body - 1 - list_h - 1, cells));
        }
        lines.truncate(height.saturating_sub(1));
        while lines.len() < height.saturating_sub(1) {
            lines.push(blank(cells));
        }
        lines.push(if self.typing {
            plain(cells, &format!("filter: {}▏  (enter keep · esc clear)", self.query), blue())
        } else {
            keys_line(cells, &[("j/k", "move"), ("l/⏎", "open"), ("/", "filter"), ("space", "preview"), ("q", "quit"), ("?", "keys")])
        });
        lines
    }

    pub fn help(&self) -> &'static [(&'static str, &'static str)] {
        HELP
    }

    pub fn handle(&mut self, key: &str) -> Flow {
        if self.typing {
            match key {
                "\r" | "\n" => self.typing = false,
                "\x1b" => {
                    self.typing = false;
                    self.query.clear();
                }
                "\x7f" | "\x08" => {
                    self.query.pop();
                }
                k if k.chars().count() == 1 && !k.chars().next().is_some_and(char::is_control) => self.query.push_str(k),
                _ => {}
            }
            return Flow::Stay;
        }
        let shown = self.shown();
        let i = self.current(&shown) as isize;
        match key {
            "q" | "\x03" | "\x04" => return Flow::Quit,
            "j" | "\x1b[B" => self.move_to(&shown, i + 1),
            "k" | "\x1b[A" => self.move_to(&shown, i - 1),
            "g" => self.move_to(&shown, 0),
            "G" => self.move_to(&shown, shown.len() as isize - 1),
            "\r" | "\n" | "l" | "\x1b[C" => return self.open(&shown, i as usize),
            "/" => {
                self.typing = true;
                self.query.clear();
            }
            "\x1b" => self.query.clear(),
            " " => self.preview = !self.preview,
            _ => {}
        }
        Flow::Stay
    }

    fn open(&mut self, shown: &[usize], i: usize) -> Flow {
        let Some(&r) = shown.get(i) else { return Flow::Stay };
        // back from the session, the cursor is still on it
        self.pinned = true;
        let live = &self.rows[r].live;
        Flow::Open(Target { pid: Some(live.pid), ..live.target() })
    }

    /// A click picks a session, a double-click opens it, the wheel moves the cursor.
    pub fn mouse(&mut self, m: Mouse) -> Flow {
        if self.typing {
            return Flow::Stay;
        }
        let shown = self.shown();
        let i = self.current(&shown) as isize;
        match (hit(&self.hits, m.x, m.y), m.press) {
            (_, Press::Wheel(d)) => self.move_to(&shown, i + d),
            (Some(Region::Row(n)), Press::Click) => self.move_to(&shown, n as isize),
            (Some(Region::Row(n)), Press::Double) => {
                self.move_to(&shown, n as isize);
                return self.open(&shown, n);
            }
            _ => {}
        }
        Flow::Stay
    }

    /// The list as text, for a pipe instead of a terminal.
    pub fn print(&mut self) {
        while !self.loaded {
            std::thread::sleep(Duration::from_millis(50));
            self.sync();
        }
        let now = now();
        for row in &self.rows {
            let l = &row.live;
            let s = row.stats.clone().unwrap_or_default();
            println!(
                "{}\t{}\t{}\t{}\t{}\t{} calls\t{} failed\t{} waiting\t{}",
                &l.session_id[..8.min(l.session_id.len())],
                l.name,
                l.project(),
                if l.busy { "busy" } else { "idle" },
                human_age(now - row.last()),
                s.calls,
                s.failed,
                s.waiting,
                l.cwd
            );
        }
    }
}
