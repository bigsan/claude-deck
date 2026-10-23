//! Drawing shared by every view: screen lines built from segments, one row per
//! item, and the detail of an item (a call, a file, a task).
//!
//! Every line a view produces is exactly `cells` wide, so ratatui never has to
//! cut a wide character at the right edge.

use std::collections::HashMap;
use std::path::PathBuf;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::data::{Call, Family, FileItem, PREVIEW_BYTES, Session, TaskItem, TaskKind, Verdict, read_text};
use crate::render::{self, Logical};
use crate::text::{fit, human_age, human_duration, human_size, hms, now, sanitize, width, wrap, ymd};

pub const SPIN: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
pub const FAMILIES: [&str; 4] = ["shell", "file", "other", "all"];

pub fn none() -> Style {
    Style::new()
}
pub fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}
pub fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}
pub fn red() -> Style {
    Style::new().fg(Color::Red)
}
pub fn green() -> Style {
    Style::new().fg(Color::Green)
}
pub fn yellow() -> Style {
    Style::new().fg(Color::Yellow)
}
pub fn blue() -> Style {
    Style::new().fg(Color::LightBlue)
}
pub fn magenta() -> Style {
    Style::new().fg(Color::Magenta)
}
pub fn cyan() -> Style {
    Style::new().fg(Color::Cyan)
}
pub fn sel_bg() -> Style {
    Style::new().bg(Color::Indexed(237))
}
pub fn tab_bg() -> Style {
    Style::new().bg(Color::Indexed(24))
}
pub fn cmd_bg() -> Style {
    Style::new().bg(Color::Indexed(235))
}

pub fn spin() -> char {
    SPIN[(now() * 8.0) as usize % SPIN.len()]
}

// ------------------------------------------------------------------ items

/// Anything a list shows: a scratchpad file, a task, a tool call.
#[derive(Clone, Copy)]
pub enum Item<'a> {
    File(&'a FileItem),
    Task(&'a TaskItem),
    Call(&'a Call),
}

impl Item<'_> {
    pub fn key(&self) -> String {
        match self {
            Item::File(f) => f.key(),
            Item::Task(t) => t.key(),
            Item::Call(c) => c.key(),
        }
    }

    pub fn at(&self) -> f64 {
        match self {
            Item::File(f) => f.mtime,
            Item::Task(t) => t.mtime,
            Item::Call(c) => c.started,
        }
    }

    /// A failed call, or a task that exited non-zero.
    pub fn failing(&self) -> bool {
        match self {
            Item::Call(c) => c.failed(),
            Item::Task(t) => t.exit.is_some_and(|e| e != 0),
            Item::File(_) => false,
        }
    }

    /// What `/` matches against.
    pub fn text(&self) -> &str {
        match self {
            Item::File(f) => &f.rel,
            Item::Task(t) => &t.name,
            Item::Call(c) => &c.summary,
        }
    }
}

// ------------------------------------------------------------------ shared state

/// Toggled by `=`, `#` and `u`, shared by every view.
#[derive(Clone, Copy, Hash)]
pub struct Options {
    pub format: bool,
    pub numbers: bool,
    pub fold: bool,
}

/// What the views share: options, filter, focus, the status line, the image to show.
pub struct Ui {
    pub options: Options,
    pub focus: Option<String>,
    pub query: String,
    pub typing: bool,
    pub failed_only: bool,
    pub status: String,
    pub status_until: f64,
    pub label: String,
    pub view_name: &'static str,
    /// (image path, (col, row, cols, rows)) the current frame wants shown
    pub image_want: Option<(PathBuf, (usize, usize, usize, usize))>,
    wrapped: HashMap<u64, Vec<Line<'static>>>,
    /// file previews as read, per file version: a frame does not read the file again
    texts: HashMap<(PathBuf, u64, u64), Option<std::rc::Rc<String>>>,
}

impl Ui {
    pub fn new(label: String) -> Ui {
        Ui {
            options: Options { format: false, numbers: true, fold: true },
            focus: None,
            query: String::new(),
            typing: false,
            failed_only: false,
            status: String::new(),
            status_until: 0.0,
            label,
            view_name: "",
            image_want: None,
            wrapped: HashMap::new(),
            texts: HashMap::new(),
        }
    }

    pub fn filtered<'a>(&self, items: Vec<Item<'a>>) -> Vec<Item<'a>> {
        let mut items = items;
        if self.failed_only {
            items.retain(Item::failing);
        }
        if !self.query.is_empty() {
            let q = self.query.to_lowercase();
            items.retain(|x| x.text().to_lowercase().contains(&q));
        }
        items
    }

    pub fn filter_label(&self) -> String {
        let mut parts = Vec::new();
        if self.failed_only {
            parts.push("failures only".to_string());
        }
        if !self.query.is_empty() || self.typing {
            parts.push(format!("/{}{}", self.query, if self.typing { "▏" } else { "" }));
        }
        parts.join(" ")
    }

    pub fn say(&mut self, message: impl Into<String>) {
        self.status = message.into();
        self.status_until = now() + 2.5;
    }

    pub fn want_image(&mut self, path: PathBuf, area: (usize, usize, usize, usize)) {
        if area.2 > 2 && area.3 > 2 {
            self.image_want = Some((path, area));
        }
    }

    pub fn mode_label(&self) -> String {
        let o = self.options;
        format!("{}{}{}", if o.format { "formatted" } else { "raw" }, if o.numbers { " · numbered" } else { "" }, if o.fold { "" } else { " · unfolded" })
    }

    pub fn header(&self, s: &Session, cells: usize) -> Line<'static> {
        let now = now();
        let fresh = s.files.iter().filter(|f| f.fresh(now)).count();
        let running = s.tasks.iter().filter(|t| t.running(now)).count() + s.calls.values().filter(|c| c.pending()).count();
        let failed = s.calls.values().filter(|c| c.failed() && !c.refused()).count();
        let denied = s.calls.values().filter(|c| c.refused()).count();
        let on = |n: usize, style: Style| if n > 0 { style } else { dim() };
        let mut segs = vec![seg("claude ", magenta().add_modifier(Modifier::BOLD), 7), flex(&self.label, none())];
        if !s.has_gate && cells >= 70 {
            segs.push(seg("no gate.jsonl ", dim(), 14));
        }
        segs.push(seg(format!("{} ", self.view_name), blue(), width(self.view_name) + 1));
        let busy = if running > 0 { spin() } else { '⚙' };
        segs.extend([
            seg(format!("●{fresh} "), on(fresh, yellow()), fresh.to_string().len() + 2),
            seg(format!("{busy}{running} "), on(running, yellow()), running.to_string().len() + 2),
            seg(format!("✗{failed} "), on(failed, red()), failed.to_string().len() + 2),
            seg(format!("⊘{denied}"), on(denied, red()), denied.to_string().len() + 1),
        ]);
        compose(cells, &segs, none())
    }

    pub fn footer(&self, cells: usize, hints: &[(&str, &str)]) -> Line<'static> {
        if now() < self.status_until {
            return plain(cells, &self.status, yellow());
        }
        if self.typing {
            return plain(cells, &format!("filter: {}▏  (enter keep · esc clear)", self.query), blue());
        }
        keys_line(cells, hints)
    }

    fn cached(&mut self, key: u64, build: impl FnOnce(&mut Ui) -> Vec<Line<'static>>) -> Vec<Line<'static>> {
        if let Some(hit) = self.wrapped.get(&key) {
            return hit.clone();
        }
        let lines = build(self);
        if self.wrapped.len() > 64 {
            self.wrapped.clear();
        }
        self.wrapped.insert(key, lines.clone());
        lines
    }

    /// The file's preview text (None when binary), read once per file version.
    pub fn file_text(&mut self, item: &FileItem) -> Option<std::rc::Rc<String>> {
        let key = (item.path.clone(), item.mtime.to_bits(), item.size);
        if let Some(hit) = self.texts.get(&key) {
            return hit.clone();
        }
        let text = read_text(&item.path, PREVIEW_BYTES, false).map(std::rc::Rc::new);
        if self.texts.len() > 32 {
            self.texts.clear();
        }
        self.texts.insert(key, text.clone());
        text
    }

    /// `text` formatted, highlighted and wrapped per the options, cached per version and width.
    pub fn body_lines(&mut self, key: impl std::hash::Hash, text: &str, name: &str, cells: usize, style: Style) -> Vec<Line<'static>> {
        let cache_key = hash(&("body", key, cells, self.options.format, self.options.numbers, format!("{style:?}")));
        self.cached(cache_key, |ui| {
            let mut note = None;
            let mut text = text.to_string();
            let mut name = name.to_string();
            if ui.options.format {
                match render::format_text(&text, &name) {
                    Some((t, n)) => (text, name) = (t, n),
                    None => note = Some("(no formatter for this content: shown as is)"),
                }
            }
            let coloured = render::highlight(&text, &name);
            let base = if coloured.is_some() { none() } else { style };
            let mut lines = styled_wrap(coloured.unwrap_or_else(|| render::plain(&text, none())), cells, ui.options.numbers, base);
            if let Some(note) = note {
                lines.insert(0, plain(cells, note, dim()));
            }
            lines
        })
    }

    /// The call's command (shell) or input (JSON), highlighted on the command background.
    pub fn command_lines(&mut self, call: &Call, cells: usize) -> Vec<Line<'static>> {
        self.cached(hash(&("cmd", &call.id, cells)), |_| {
            let (text, name) = match call.command() {
                Some(command) => (sanitize(command, false), "command.sh"),
                None => (sanitize(&json_indent1(&call.input), false), "input.json"),
            };
            let coloured = render::highlight(&text, name).unwrap_or_else(|| render::plain(&text, none()));
            styled_wrap(coloured, cells, false, cmd_bg())
        })
    }

    /// The call's output: formatted, highlighted or coloured by shape, folded, numbered.
    pub fn output_lines(&mut self, call: &Call, s: &Session, cells: usize) -> Vec<Line<'static>> {
        let live = if call.pending() { call.live.as_deref().and_then(|name| s.task_named(name)) } else { None };
        let o = self.options;
        let cache_key = hash(&("out", &call.id, call.ended.map(f64::to_bits), live.map(|t| t.size), cells, o.format, o.numbers, o.fold));
        self.cached(cache_key, |_| {
            let mut text = if !call.output.is_empty() {
                call.output.clone()
            } else if let Some(t) = live.filter(|t| !t.text.is_empty()) {
                t.text.clone()
            } else if call.pending() {
                "(running)".into()
            } else {
                "(no output)".into()
            };
            let mut start = 1;
            let mut coloured: Option<Vec<Logical>> = None;
            let formatted = if o.format { render::format_text(&text, "output.txt") } else { None };
            let read = if call.tool == "Read" && formatted.is_none() { render::strip_read_numbers(&text) } else { None };
            if let Some((t, name)) = formatted {
                coloured = render::highlight(&t, &name);
                text = t;
            } else if let Some((t, first)) = read {
                let file = call.input["file_path"].as_str().unwrap_or("file.txt");
                let name = file.rsplit('/').next().unwrap_or(file).to_string();
                coloured = render::highlight(&t, &name);
                text = t;
                start = first;
            }
            let text: String = text.split('\n').map(render::expand_tabs).collect::<Vec<_>>().join("\n");
            let lines = coloured.unwrap_or_else(|| render::colour_shape(&text));
            render::wrap_items(&render::fold(lines, start, o.fold), cells, o.numbers, none())
        })
    }
}

fn hash(value: &impl std::hash::Hash) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut h);
    h.finish()
}

/// JSON with one space of indent per level, the way `json.dumps(indent=1)` writes it.
fn json_indent1(value: &serde_json::Value) -> String {
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut ser = serde_json::Serializer::with_formatter(&mut out, formatter);
    if serde::Serialize::serialize(value, &mut ser).is_err() {
        return value.to_string();
    }
    String::from_utf8(out).unwrap_or_default()
}

/// Logical lines numbered from 1 and wrapped; a trailing empty line is dropped.
pub fn styled_wrap(mut lines: Vec<Logical>, cells: usize, numbers: bool, style: Style) -> Vec<Line<'static>> {
    if lines.len() > 1 && lines.last().is_some_and(Vec::is_empty) {
        lines.pop();
    }
    let items: Vec<(Option<usize>, Logical)> = lines.into_iter().enumerate().map(|(i, l)| (Some(i + 1), l)).collect();
    render::wrap_items(&items, cells, numbers, style)
}

// ------------------------------------------------------------------ drawing

/// One piece of a screen line; `size` None takes the rest.
pub struct Seg {
    pub text: String,
    pub style: Style,
    pub size: Option<usize>,
    pub right: bool,
}

pub fn seg(text: impl Into<String>, style: Style, size: usize) -> Seg {
    Seg { text: text.into(), style, size: Some(size), right: false }
}

/// Right-aligned in its `size` cells.
pub fn seg_r(text: impl Into<String>, style: Style, size: usize) -> Seg {
    Seg { text: text.into(), style, size: Some(size), right: true }
}

pub fn flex(text: impl Into<String>, style: Style) -> Seg {
    Seg { text: text.into(), style, size: None, right: false }
}

/// One screen line from segments, `bg` under all of them and the padding.
pub fn compose(cells: usize, segs: &[Seg], bg: Style) -> Line<'static> {
    let fixed: usize = segs.iter().filter_map(|s| s.size).sum();
    let rest = cells.saturating_sub(fixed);
    let mut spans = Vec::with_capacity(segs.len() + 1);
    let mut used = 0;
    for s in segs {
        let size = s.size.unwrap_or(rest).min(cells - used);
        if size == 0 {
            continue;
        }
        spans.push(Span::styled(fit(&s.text, size, true, s.right), bg.patch(s.style)));
        used += size;
    }
    if used < cells {
        spans.push(Span::styled(" ".repeat(cells - used), bg));
    }
    Line::from(spans)
}

pub fn plain(cells: usize, text: &str, style: Style) -> Line<'static> {
    compose(cells, &[flex(text, style)], none())
}

pub fn plain_bg(cells: usize, text: &str, style: Style, bg: Style) -> Line<'static> {
    compose(cells, &[flex(text, style)], bg)
}

pub fn blank(cells: usize) -> Line<'static> {
    Line::from(" ".repeat(cells))
}

pub fn rule(cells: usize, title: &str, right: &str, style: Style) -> Line<'static> {
    let left = if title.is_empty() { "─".to_string() } else { format!("─ {title} ") };
    let tail = if right.is_empty() { "─".to_string() } else { format!(" {right} ─") };
    let fill = cells.saturating_sub(width(&left) + width(&tail));
    let (lw, tw) = (width(&left), width(&tail));
    compose(cells, &[seg(left, style, lw), seg("─".repeat(fill), style, fill), seg(tail, style, tw)], none())
}

/// Pre-styled spans as one line, cut to `cells` by visible width (losing the
/// styles when it has to be cut).
pub fn raw(cells: usize, spans: Vec<Span<'static>>) -> Line<'static> {
    let visible: String = spans.iter().map(|s| s.content.as_ref()).collect();
    let used = width(&visible);
    if used > cells {
        return plain(cells, &visible, none());
    }
    let mut spans = spans;
    spans.push(Span::raw(" ".repeat(cells - used)));
    Line::from(spans)
}

/// Styled pieces joined with a dim ` · `, empty pieces skipped.
fn joined(pieces: Vec<Vec<Span<'static>>>) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    for piece in pieces.into_iter().filter(|p| p.iter().any(|s| !s.content.is_empty())) {
        if !out.is_empty() {
            out.extend([Span::raw(" "), Span::styled("·", dim()), Span::raw(" ")]);
        }
        out.extend(piece);
    }
    out
}

pub fn keys_line(cells: usize, pairs: &[(&str, &str)]) -> Line<'static> {
    let mut segs = Vec::new();
    for (key, what) in pairs {
        segs.push(seg(*key, blue(), width(key)));
        segs.push(seg(format!(" {what}  "), dim(), width(what) + 3));
    }
    let mut used: usize = segs.iter().filter_map(|s| s.size).sum();
    while used > cells && segs.len() > 2 {
        used -= segs.pop().and_then(|s| s.size).unwrap_or(0) + segs.pop().and_then(|s| s.size).unwrap_or(0);
    }
    segs.push(flex("", none()));
    compose(cells, &segs, none())
}

pub fn wrapped(cells: usize, text: &str, style: Style) -> Vec<Line<'static>> {
    wrap(text, cells.max(1)).iter().map(|line| plain(cells, line, style)).collect()
}

/// `height` lines of `lines` from `top`, padded with blank lines.
pub fn window(lines: &[Line<'static>], top: usize, height: usize, cells: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = lines.iter().skip(top).take(height).cloned().collect();
    while out.len() < height {
        out.push(blank(cells));
    }
    out
}

/// The first visible row, moved just enough to keep `selected` on screen.
pub fn visible_top(selected: usize, top: usize, height: usize, total: usize) -> usize {
    if height == 0 {
        return 0;
    }
    let mut top = top;
    if selected < top {
        top = selected;
    } else if selected >= top + height {
        top = selected + 1 - height;
    }
    top.min(total.saturating_sub(height))
}

/// The index of the item with `key`, else `index` clamped.
pub fn pick(items: &[Item], key: Option<&str>, index: usize) -> usize {
    if let Some(key) = key {
        if let Some(i) = items.iter().position(|x| x.key() == key) {
            return i;
        }
    }
    index.min(items.len().saturating_sub(1))
}

pub fn next_failure(items: &[Item], start: usize, step: isize) -> usize {
    let total = items.len() as isize;
    for offset in 1..=total {
        let i = (start as isize + step * offset).rem_euclid(total) as usize;
        if items[i].failing() {
            return i;
        }
    }
    start
}

// ------------------------------------------------------------------ rows

/// The status column: its longest text, "⊘ deny", plus a cell before the verdict or
/// agent mark that follows it.
const STATUS: usize = 7;

pub fn status_seg(call: &Call) -> Seg {
    if call.pending() {
        return seg(format!("{} run", spin()), yellow(), STATUS);
    }
    if call.verdict == Verdict::Reject {
        return seg("⊘ no", red(), STATUS);
    }
    if call.denied || call.verdict == Verdict::Deny {
        return seg("⊘ deny", red(), STATUS);
    }
    if call.interrupted {
        return seg("⌁ int", yellow(), STATUS);
    }
    let failed = red().add_modifier(Modifier::BOLD);
    if call.family == Family::Shell {
        return match call.exit {
            Some(0) => seg("✓", green(), STATUS),
            Some(code) => seg(format!("✗ {code}"), failed, STATUS),
            None => seg("✗", failed, STATUS),
        };
    }
    if call.is_error { seg("✗", failed, STATUS) } else { seg("✓", green(), STATUS) }
}

fn verdict_text(verdict: Verdict) -> Option<(&'static str, Style, &'static str)> {
    match verdict {
        Verdict::Auto => Some(("auto", green(), "allowed without asking")),
        Verdict::Ask => Some(("ask", blue(), "asked → allowed")),
        Verdict::Deny => Some(("deny", red(), "denied")),
        Verdict::Reject => Some(("rej", red(), "you said no")),
        Verdict::Wait => Some(("wait", yellow(), "waiting for you")),
        Verdict::None => None,
    }
}

pub fn verdict_seg(call: &Call) -> Seg {
    match verdict_text(call.verdict) {
        Some((text, style, _)) => seg(text, style, 5),
        None => seg("·", dim(), 5),
    }
}

pub fn call_row(call: &Call, cells: usize, gate: bool, bg: Style) -> Line<'static> {
    let narrow = cells < 70;
    let mut segs = vec![seg(hms(call.started), dim(), 9), status_seg(call)];
    if gate && !narrow {
        segs.push(verdict_seg(call));
    }
    if !narrow {
        segs.push(seg_r(human_duration(call.duration_ms()), dim(), 7));
        segs.push(seg(" ", none(), 1));
    }
    segs.push(seg(if call.agent.is_some() { "◆" } else { " " }, magenta(), 2));
    if call.family != Family::Shell {
        segs.push(seg(&call.tool, cyan(), 10.min(width(&call.tool) + 1)));
    }
    segs.push(flex(&call.summary, none()));
    compose(cells, &segs, bg)
}

/// An image the terminal already holds: showing it again sends nothing.
pub fn image_cached(item: &FileItem) -> bool {
    item.is_image && crate::kitty::cached(&item.path, item.mtime, item.size)
}

/// The mark after a cached image's name.
pub fn cached_seg(item: &FileItem) -> Option<Seg> {
    image_cached(item).then(|| seg(" cached", green().add_modifier(Modifier::DIM), 7))
}

pub fn file_row(item: &FileItem, cells: usize, now: f64, bg: Style) -> Line<'static> {
    let fresh = item.fresh(now);
    let mut segs = vec![
        seg(if fresh { "●" } else { " " }, yellow(), 2),
        seg(if item.is_image { "▣" } else { " " }, magenta(), 2),
        flex(&item.rel, if fresh { bold() } else { none() }),
    ];
    segs.extend(cached_seg(item));
    segs.push(seg_r(human_size(item.size), dim(), 8));
    segs.push(seg_r(human_age(now - item.mtime), if fresh { yellow() } else { dim() }, 5));
    compose(cells, &segs, bg)
}

pub fn task_status(task: &TaskItem, now: f64) -> Seg {
    if task.kind == TaskKind::Agent {
        let running = task.running(now);
        let text = if running { format!("{} {} rows", spin(), task.rows) } else { format!("{} rows", task.rows) };
        return seg(text, if running { yellow() } else { dim() }, 11);
    }
    match task.exit {
        None if task.running(now) => seg(format!("{} running", spin()), yellow(), 11),
        None => seg("? no exit", dim(), 11),
        Some(0) => seg("✓ done", green(), 11),
        Some(code) => seg(format!("✗ exit {code}"), red(), 11),
    }
}

pub fn task_row(task: &TaskItem, cells: usize, now: f64, bg: Style) -> Line<'static> {
    let agent = task.kind == TaskKind::Agent;
    let mut segs = vec![
        seg(if agent { "◆" } else { "⚙" }, if agent { magenta() } else { cyan() }, 2),
        seg(if agent { "subagent" } else { "bg shell" }, dim(), 9),
        task_status(task, now),
        flex(&task.name, none()),
    ];
    if cells >= 70 {
        segs.push(seg_r(human_size(task.size), dim(), 8));
    }
    segs.push(seg_r(human_age(now - task.mtime), dim(), 5));
    compose(cells, &segs, bg)
}

pub fn item_row(item: Item, cells: usize, now: f64, gate: bool, bg: Style) -> Line<'static> {
    match item {
        Item::Call(c) => call_row(c, cells, gate, bg),
        Item::File(f) => file_row(f, cells, now, bg),
        Item::Task(t) => task_row(t, cells, now, bg),
    }
}

// ------------------------------------------------------------------ details

fn span(text: impl Into<String>, style: Style) -> Span<'static> {
    Span::styled(text.into(), style)
}

pub fn call_detail(ui: &mut Ui, call: &Call, index: usize, total: usize, cells: usize, s: &Session) -> Vec<Line<'static>> {
    let status = status_seg(call);
    let mut meta: Vec<Vec<Span<'static>>> = vec![
        vec![span(format!("{} {}/{}", call.family.name(), index + 1, total), bold())],
        vec![span(hms(call.started), none())],
        vec![span(ymd(call.started), none())],
    ];
    if call.pending() {
        meta.push(vec![span(format!("{} running", spin()), yellow())]);
    } else if let Some(code) = call.exit.filter(|e| *e != 0 && call.family == Family::Shell) {
        meta.push(vec![span(format!("exit {code}"), red().add_modifier(Modifier::BOLD))]);
    } else {
        meta.push(vec![span(status.text, status.style)]);
    }
    if call.duration_ms().is_some() {
        meta.push(vec![span(human_duration(call.duration_ms()), none())]);
    }
    let mut more: Vec<Vec<Span<'static>>> = Vec::new();
    if let Some((_, style, label)) = verdict_text(call.verdict) {
        more.push(vec![span(label, style)]);
    }
    if !call.mode.is_empty() {
        more.push(vec![span(format!("mode {}", call.mode), dim())]);
    }
    if let Some(agent) = &call.agent {
        more.push(vec![span(format!("◆ {}", agent.chars().take(10).collect::<String>()), magenta())]);
    }
    let mut lines = vec![raw(cells, joined(meta))];
    if !more.is_empty() {
        lines.push(raw(cells, joined(more)));
    }
    if !call.desc.is_empty() {
        lines.extend(wrapped(cells, &call.desc, dim()));
    }
    lines.push(rule(cells, &call.tool, "y copy", dim()));
    lines.extend(ui.command_lines(call, cells));
    let live = if call.pending() { call.live.as_deref().and_then(|name| s.task_named(name)) } else { None };
    let out = if !call.output.is_empty() { call.output.as_str() } else { live.map(|t| t.text.as_str()).unwrap_or("") };
    let title = match live {
        Some(t) => format!("live output · tasks/{}.output", t.name),
        None => "output".into(),
    };
    let count = out.matches('\n').count() + 1;
    lines.push(rule(cells, &title, &format!("{count} lines · {} · = # u", ui.mode_label()), dim()));
    lines.extend(ui.output_lines(call, s, cells));
    if let Some(task_id) = &call.task {
        let task = s.tasks.iter().find(|t| &t.name == task_id);
        let right = match task {
            Some(t) => task_status(t, now()).text,
            None => "not found".into(),
        };
        lines.push(rule(cells, &format!("tasks/{task_id}.output"), &right, dim()));
        if let Some(t) = task {
            let tail: String = {
                let n = t.text.chars().count();
                t.text.chars().skip(n.saturating_sub(8000)).collect()
            };
            lines.extend(wrapped(cells, &tail, none()));
        }
    }
    lines
}

pub fn file_detail(ui: &mut Ui, item: &FileItem, cells: usize, now: f64) -> (Vec<Line<'static>>, Option<PathBuf>) {
    let mut head = vec![
        span(item.rel.clone(), bold()),
        span(format!(" · {} · {} · {} ago", human_size(item.size), hms(item.mtime), human_age(now - item.mtime)), dim()),
    ];
    if item.fresh(now) {
        head.push(span(" ● changing", yellow()));
    }
    if image_cached(item) {
        head.push(span(" · cached in the terminal", green()));
    }
    let mut lines = vec![raw(cells, head)];
    if item.is_image {
        return (lines, Some(item.path.clone()));
    }
    let Some(text) = ui.file_text(item) else {
        lines.push(rule(cells, "preview", "", dim()));
        lines.push(plain(cells, "(binary file)", dim()));
        return (lines, None);
    };
    lines.push(rule(cells, "preview", &format!("{} · = format · # numbers", ui.mode_label()), dim()));
    let name = item.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    lines.extend(ui.body_lines(("file", &item.path, item.mtime.to_bits(), item.size), &text, &name, cells, none()));
    (lines, None)
}

pub fn task_detail(task: &TaskItem, cells: usize, now: f64) -> Vec<Line<'static>> {
    let status = task_status(task, now);
    let agent = task.kind == TaskKind::Agent;
    let kind = if agent { span("◆ subagent", magenta()) } else { span("⚙ bg shell", cyan()) };
    let mut lines = vec![raw(
        cells,
        vec![kind, span(format!(" {} ", task.name), none()), span(format!("· {} ·", human_size(task.size)), dim()), span(" ", none()), span(status.text, status.style)],
    )];
    if agent {
        if let Ok(target) = std::fs::read_link(&task.path) {
            lines.push(plain(cells, &format!("→ {}", target.display()), dim()));
        }
    }
    lines.push(rule(cells, if agent { "transcript (condensed)" } else { "output tail" }, "", dim()));
    lines.extend(wrapped(cells, if task.text.is_empty() { "(empty)" } else { &task.text }, none()));
    lines
}

pub fn detail_of(ui: &mut Ui, item: Option<Item>, index: usize, total: usize, cells: usize, s: &Session, now: f64) -> (Vec<Line<'static>>, Option<PathBuf>) {
    match item {
        Some(Item::Call(c)) => (call_detail(ui, c, index, total, cells, s), None),
        Some(Item::File(f)) => file_detail(ui, f, cells, now),
        Some(Item::Task(t)) => (task_detail(t, cells, now), None),
        None => (vec![plain(cells, "nothing selected", dim())], None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn visible(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn a_rejected_call_reads_as_no_not_deny() {
        let mut call = Call::new("t1", "Bash", json!({ "command": "rm x" }), 1.0, None);
        call.finish(2.0, "The user doesn't want to proceed with this tool use.", true, &json!({}));
        call.verdict = Verdict::Reject;
        assert_eq!(status_seg(&call).text, "⊘ no");
    }

    /// deny, reject, pending, auto and ask calls, each with its verdict.
    fn verdict_calls() -> Vec<(&'static str, Call)> {
        let call = |verdict: Verdict, finish: Option<(&str, bool)>| {
            let mut call = Call::new("t1", "Bash", json!({ "command": "rm x" }), 1.0, None);
            if let Some((output, error)) = finish {
                call.finish(2.0, output, error, &json!({}));
            }
            call.verdict = verdict;
            call
        };
        vec![
            ("deny", call(Verdict::Deny, Some(("Permission to use Bash has been denied.", true)))),
            ("reject", call(Verdict::Reject, Some(("The user doesn't want to proceed with this tool use.", true)))),
            ("pending", call(Verdict::Wait, None)),
            ("auto", call(Verdict::Auto, Some(("ok", false)))),
            ("ask", call(Verdict::Ask, Some(("ok", false)))),
        ]
    }

    #[test]
    fn every_status_leaves_a_cell_before_the_verdict() {
        for (name, call) in verdict_calls() {
            let status = status_seg(&call);
            assert!(width(&status.text) < status.size.unwrap(), "{name}: {:?} fills its column", status.text);
            let row = visible(&call_row(&call, 100, true, none()));
            let after = &row[row.find(&status.text).unwrap() + status.text.len()..];
            let verdict = verdict_seg(&call).text;
            assert!(after.starts_with(' ') && after.trim_start().starts_with(&verdict), "{name}: {row:?}");
        }
    }

    #[test]
    fn a_denied_call_reads_deny_once_in_each_column() {
        let (_, call) = verdict_calls().remove(0);
        assert_eq!(status_seg(&call).text, "⊘ deny");
        assert!(!visible(&call_row(&call, 100, true, none())).contains("denydeny"));
    }

    #[test]
    fn compose_fills_exactly_the_width_with_wide_text() {
        let line = compose(20, &[seg("12:00:00", dim(), 9), flex("本機匯入管理工具測試", none()), seg_r("3s", dim(), 4)], sel_bg());
        assert_eq!(width(&visible(&line)), 20);
        assert!(visible(&line).ends_with("  3s"));
        assert_eq!(line.spans[1].style.bg, Some(Color::Indexed(237)));
    }

    #[test]
    fn rule_and_keys_line_fit_the_width() {
        assert_eq!(visible(&rule(20, "calls", "x", dim())), "─ calls ──────── x ─");
        let keys = keys_line(12, &[("j/k", "move"), ("v", "view")]);
        assert_eq!(visible(&keys), "j/k move    ");
    }

    #[test]
    fn visible_top_scrolls_just_enough() {
        assert_eq!(visible_top(0, 5, 10, 100), 0);
        assert_eq!(visible_top(14, 0, 10, 100), 5);
        assert_eq!(visible_top(3, 2, 10, 6), 0);
    }
}
