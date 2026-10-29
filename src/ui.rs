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
use crate::text::{char_width, fit, human_age, human_duration, human_size, hms, now, one_line, sanitize, width, wrap, wrap_words, ymd};

// Three of the four middle braille dots, the gap going round clockwise: the middle
// rows sit level with lowercase letters, where the top ones (⠋) stand above them.
pub const SPIN: [char; 4] = ['⠴', '⠦', '⠖', '⠲'];
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
    // one turn a second
    SPIN[(now() * 4.0) as usize % SPIN.len()]
}

// While the agent works, pairs of dots (● ahead, • behind) run right along the rule
// under the header, one cell per RAIL_STEP seconds. The gaps between pairs differ and
// repeat only every sum(RAIL_GAPS) cells, so the spacing never looks regular.
const RAIL_GAPS: [usize; 7] = [17, 29, 11, 23, 35, 14, 26];
pub const RAIL_STEP: f64 = 0.15;

pub fn rail_phase() -> usize {
    (now() / RAIL_STEP) as usize
}

/// The dot on each of `cells` cells at step `phase`, if any. A pair runs off the right
/// end, its trailing dot last, and comes back in at the left.
pub fn rail(phase: usize, cells: usize) -> Vec<Option<char>> {
    let total: usize = RAIL_GAPS.iter().sum();
    let mut out = vec![None; cells];
    let mut at = 0;
    for gap in RAIL_GAPS {
        let mut head = (at + phase % total) % total;
        while head <= cells {
            if head > 0 {
                out[head - 1] = Some('•');
            }
            if head < cells {
                out[head] = Some('●');
            }
            head += total;
        }
        at += gap;
    }
    out
}

/// `line` with the rail's dots on whichever of its cells are ─ (corners and titles stay).
pub fn on_rail(line: Line<'static>, phase: usize) -> Line<'static> {
    let lit = rail(phase, line.spans.iter().map(|s| width(&s.content)).sum());
    let lit_style = yellow().add_modifier(Modifier::BOLD);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut x = 0;
    for span in &line.spans {
        for c in span.content.chars() {
            let (glyph, style) = match lit.get(x).copied().flatten() {
                // a rule is dim, and patching keeps that: the dots drop it
                Some(dot) if c == '─' => (dot, span.style.remove_modifier(Modifier::DIM).patch(lit_style)),
                _ => (c, span.style),
            };
            match spans.last_mut() {
                Some(last) if last.style == style => last.content.to_mut().push(glyph),
                _ => spans.push(Span::styled(glyph.to_string(), style)),
            }
            x += char_width(c);
        }
    }
    Line { spans, ..line }
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

// ------------------------------------------------------------------ mouse

/// (col, row, cols, rows) on the screen.
pub type Area = (usize, usize, usize, usize);

/// What a click can land on, as the frame drew it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Region {
    /// a tab or a toggle in the line under the header
    Tab(usize),
    /// the n-th item of the list
    Row(usize),
    /// a source in B's first column
    Source(usize),
    /// B's n-th column, borders and all
    Column(usize),
    /// a preview or detail that scrolls
    Preview,
    /// a scrollbar
    Bar(Bar),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Press {
    Click,
    Double,
    /// notches down, negative up
    Wheel(isize),
    /// moved with the left button held
    Drag,
    /// the left button let go
    Release,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mouse {
    pub press: Press,
    pub x: usize,
    pub y: usize,
}

/// What a scrollbar scrolls: a list, whose cursor follows into view, or a text.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Scroll {
    List,
    Text,
    /// B's sources, which follow the source picked
    Sources,
}

/// A scrollbar as drawn: `view` of `total` lines from `top`, in rows `y..y + view`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bar {
    pub scrolls: Scroll,
    pub y: usize,
    pub total: usize,
    pub view: usize,
    pub top: usize,
}

impl Bar {
    fn last_top(&self) -> usize {
        self.total.saturating_sub(self.view)
    }

    /// The first line shown when the thumb starts `row` rows down the track: the thumb's
    /// travel spread over the lines it can scroll.
    fn top_at(&self, row: usize) -> usize {
        let travel = self.view - thumb(self.total, self.view, self.top, self.view).1;
        (row.min(travel) * self.last_top() + travel / 2).checked_div(travel).unwrap_or(0)
    }
}

/// (first row, rows) of the thumb in a track of `track` rows, as ratatui's Scrollbar draws it
/// for a ScrollbarState of `total - view + 1` positions and a viewport of `view`.
pub fn thumb(total: usize, view: usize, top: usize, track: usize) -> (usize, usize) {
    if track == 0 || total == 0 {
        return (0, 0);
    }
    let round = |n: usize, d: usize| (n + d / 2) / d;
    let len = round(view * track, total).clamp(1, track);
    let start = round(top.min(total.saturating_sub(view)) * track, total).min(track - len);
    (start, len)
}

/// A scrollbar column, one span per row of the view, when `total` lines do not fit in `view`.
pub fn scrollbar(total: usize, view: usize, top: usize) -> Option<Vec<Span<'static>>> {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState, StatefulWidget};
    if total <= view || view == 0 {
        return None;
    }
    let area = Rect::new(0, 0, 1, view.min(u16::MAX as usize) as u16);
    let mut buf = Buffer::empty(area);
    let mut state = ScrollbarState::new(total - view + 1).position(top).viewport_content_length(view);
    Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        // only the thumb, half a cell wide and dim, as the tsk pane draws it; the track
        // is left blank but still takes clicks
        .track_symbol(None)
        .thumb_symbol("▌")
        .thumb_style(dim())
        .render(area, &mut buf, &mut state);
    // where ratatui put the thumb, in plain dim without the buffer's explicit colour resets
    Some((0..area.height).map(|y| if buf[(0, y)].symbol() == "▌" { Span::styled("▌", dim()) } else { Span::raw(" ") }).collect())
}

/// The width left for lines next to a scrollbar: one cell less when they do not fit.
pub fn beside_bar(cells: usize, total: usize, view: usize) -> usize {
    if total > view && view > 0 { cells.saturating_sub(1) } else { cells }
}

/// Put a scrollbar at the end of `lines` (the rows of a view drawn one cell narrower) when
/// `total` lines do not fit in them, and mark it at column `x`, row `y`.
pub fn add_bar(hits: &mut Vec<(Area, Region)>, lines: &mut [Line<'static>], x: usize, y: usize, scrolls: Scroll, total: usize, top: usize) {
    let view = lines.len();
    let Some(spans) = scrollbar(total, view, top) else { return };
    for (line, span) in lines.iter_mut().zip(spans) {
        line.spans.push(span);
    }
    hits.push(((x, y, 1, view), Region::Bar(Bar { scrolls, y, total, view, top })));
}

/// `view` rows of `text` from `scroll` (clamped to the end) at column `x`, row `y`, with a
/// scrollbar when it does not fit; `text` is laid out one cell narrower then.
pub fn text_rows(hits: &mut Vec<(Area, Region)>, text: &[Line<'static>], cells: usize, view: usize, scroll: &mut usize, (x, y): (usize, usize)) -> Vec<Line<'static>> {
    let w = beside_bar(cells, text.len(), view);
    *scroll = (*scroll).min(text.len().saturating_sub(view));
    let mut rows = window(text, *scroll, view, w);
    add_bar(hits, &mut rows, x + w, y, Scroll::Text, text.len(), *scroll);
    rows
}

/// What a press does to a scrollbar.
#[derive(Debug, PartialEq)]
pub enum BarUse {
    /// not a scrollbar's
    No,
    /// the thumb taken or let go
    Held,
    /// show from this line on
    To(Scroll, usize),
}

/// A click on the track pages, a press on the thumb takes it, and a drag moves it; `drag`
/// keeps the scrollbar held and how far down the thumb it was taken.
///
/// A drag goes by the scrollbar of that kind as last drawn, not as it was when taken: the list
/// may have grown, or the screen changed size, since.
pub fn on_bar(drag: &mut Option<(Bar, usize)>, hits: &[(Area, Region)], m: Mouse) -> BarUse {
    match m.press {
        Press::Drag => match drag {
            Some((held, grab)) => {
                let now = hits.iter().rev().find_map(|(_, r)| match r {
                    Region::Bar(b) if b.scrolls == held.scrolls => Some(*b),
                    _ => None,
                });
                let bar = now.unwrap_or(*held);
                BarUse::To(bar.scrolls, bar.top_at(m.y.saturating_sub(bar.y + *grab)))
            }
            None => BarUse::No,
        },
        Press::Release => {
            if drag.take().is_some() {
                BarUse::Held
            } else {
                BarUse::No
            }
        }
        // a double-click on the track is two pages
        Press::Click | Press::Double => {
            // a new press lets go of a thumb whose release never came
            drag.take();
            let Some(Region::Bar(bar)) = hit(hits, m.x, m.y) else { return BarUse::No };
            let (start, len) = thumb(bar.total, bar.view, bar.top, bar.view);
            let row = m.y - bar.y;
            if row < start {
                BarUse::To(bar.scrolls, bar.top.saturating_sub(bar.view))
            } else if row >= start + len {
                BarUse::To(bar.scrolls, (bar.top + bar.view).min(bar.last_top()))
            } else {
                *drag = Some((bar, row - start));
                BarUse::Held
            }
        }
        Press::Wheel(_) => BarUse::No,
    }
}

/// The region at (x, y); a region marked later lies over the ones before it.
pub fn hit(hits: &[(Area, Region)], x: usize, y: usize) -> Option<Region> {
    hits.iter().rev().find(|((ax, ay, w, h), _)| (*ax..ax + w).contains(&x) && (*ay..ay + h).contains(&y)).map(|(_, r)| *r)
}

/// Mark one region per item row of a list drawn from `top` at screen row `y`.
pub fn mark_rows(hits: &mut Vec<(Area, Region)>, x: usize, y: usize, cells: usize, top: usize, shown: usize, total: usize) {
    for n in top..total.min(top + shown) {
        hits.push(((x, y + n - top, cells, 1), Region::Row(n)));
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
    /// since when the agent has been busy, while it is
    pub agent_busy: Option<f64>,
    /// seconds the agent's last busy stretch took, once it is idle
    pub agent_took: Option<f64>,
    /// (image path, (col, row, cols, rows)) the current frame wants shown
    pub image_want: Option<(PathBuf, (usize, usize, usize, usize))>,
    /// where the current frame put what a click can land on
    pub hits: Vec<(Area, Region)>,
    /// the scrollbar whose thumb the mouse holds
    pub drag: Option<(Bar, usize)>,
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
            agent_busy: None,
            agent_took: None,
            view_name: "",
            image_want: None,
            hits: Vec::new(),
            drag: None,
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

    pub fn mark(&mut self, area: Area, region: Region) {
        self.hits.push((area, region));
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
        if let Some(since) = self.agent_busy {
            let busy = format!("{} busy {} ", spin(), human_age(now - since));
            segs.push(seg(busy.clone(), yellow(), width(&busy)));
        } else if let Some(took) = self.agent_took {
            let took = format!("took {} ", human_duration(Some(took * 1000.0)));
            segs.push(seg(took.clone(), dim(), width(&took)));
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

/// One key of the help page: the key in its column, what it does word-wrapped beside it.
pub fn help_row(cells: usize, key: &str, what: &str) -> Vec<Line<'static>> {
    const KEY: usize = 14;
    let key = format!("  {key}");
    let rows = wrap_words(what, cells.saturating_sub(KEY).max(1));
    rows.iter().enumerate().map(|(i, row)| compose(cells, &[seg(if i == 0 { key.as_str() } else { "" }, yellow(), KEY), flex(row.as_str(), dim())], none())).collect()
}

/// `text` word-wrapped after `lead`, its later rows indented to line up under the first.
pub fn hanging(cells: usize, lead: &str, text: &str, style: Style) -> Vec<Line<'static>> {
    let indent = width(lead);
    let pad = " ".repeat(indent);
    let rows = wrap_words(text, cells.saturating_sub(indent).max(1));
    rows.iter().enumerate().map(|(i, row)| plain(cells, &format!("{}{row}", if i == 0 { lead } else { &pad }), style)).collect()
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

/// What the task is doing: its call's description, else its command, else its id.
fn task_label(task: &TaskItem) -> String {
    one_line([task.desc.as_str(), task.command.as_str()].into_iter().find(|t| !t.is_empty()).unwrap_or(&task.name))
}

pub fn task_row(task: &TaskItem, cells: usize, now: f64, bg: Style) -> Line<'static> {
    let agent = task.kind == TaskKind::Agent;
    let mut segs = vec![
        seg(if agent { "◆" } else { "⚙" }, if agent { magenta() } else { cyan() }, 2),
        seg(if agent { "subagent" } else { "bg shell" }, dim(), 9),
        task_status(task, now),
        flex(task_label(task), none()),
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
    if !task.desc.is_empty() {
        lines.extend(wrapped(cells, &task.desc, dim()));
    }
    if !task.command.is_empty() {
        lines.push(rule(cells, "command", "", dim()));
        lines.extend(wrapped(cells, &task.command, none()));
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

    /// The thumb rows ratatui's Scrollbar draws.
    fn drawn_thumb(total: usize, view: usize, top: usize) -> Option<(usize, usize)> {
        let spans = scrollbar(total, view, top)?;
        let rows: Vec<usize> = spans.iter().enumerate().filter(|(_, s)| s.content == "▌").map(|(n, _)| n).collect();
        Some((rows[0], rows.len()))
    }

    #[test]
    fn the_thumb_shows_where_the_view_is_and_how_much_of_it() {
        // fits: no scrollbar at all
        assert_eq!(scrollbar(10, 10, 0), None);
        assert_eq!(scrollbar(3, 10, 0), None);
        assert_eq!(beside_bar(80, 10, 10), 80);
        assert_eq!(beside_bar(80, 11, 10), 79);
        // a quarter of the lines: a quarter of the track, from the top to the bottom
        assert_eq!(thumb(40, 10, 0, 10), (0, 3));
        assert_eq!(thumb(40, 10, 30, 10), (7, 3));
        assert_eq!(thumb(40, 10, 15, 10), (4, 3));
        // a long list still gets a thumb of one row, which reaches the bottom
        assert_eq!(thumb(10_000, 10, 0, 10), (0, 1));
        assert_eq!(thumb(10_000, 10, 9_990, 10), (9, 1));
        // the same as ratatui draws, wherever the view is
        for (total, view) in [(11, 10), (40, 10), (100, 7), (1000, 23), (25, 24)] {
            for top in 0..=total - view {
                assert_eq!(drawn_thumb(total, view, top), Some(thumb(total, view, top, view)), "{total} {view} {top}");
            }
        }
        let spans = scrollbar(40, 4, 0).unwrap();
        assert_eq!(spans.iter().map(|s| s.content.as_ref()).collect::<String>(), "▌   ");
        assert_eq!(spans[0].style, dim());
    }

    #[test]
    fn the_track_pages_and_the_thumb_drags() {
        let bar = Bar { scrolls: Scroll::Text, y: 5, total: 40, view: 10, top: 15 };
        let hits = vec![((79, 5, 1, 10), Region::Bar(bar))];
        let mut drag = None;
        let at = |press, y| Mouse { press, x: 79, y };
        // the thumb is rows 4..7 of the track: rows 9..12 on the screen
        assert_eq!(on_bar(&mut drag, &hits, at(Press::Click, 6)), BarUse::To(Scroll::Text, 5));
        assert_eq!(on_bar(&mut drag, &hits, at(Press::Click, 14)), BarUse::To(Scroll::Text, 25));
        assert_eq!(on_bar(&mut drag, &hits, Mouse { press: Press::Click, x: 78, y: 6 }), BarUse::No);
        // taken one row down the thumb, dragged to the top and the bottom
        assert_eq!(on_bar(&mut drag, &hits, at(Press::Click, 10)), BarUse::Held);
        assert_eq!(on_bar(&mut drag, &hits, at(Press::Drag, 6)), BarUse::To(Scroll::Text, 0));
        assert_eq!(on_bar(&mut drag, &hits, at(Press::Drag, 9)), BarUse::To(Scroll::Text, 13));
        assert_eq!(on_bar(&mut drag, &hits, at(Press::Drag, 13)), BarUse::To(Scroll::Text, 30));
        assert_eq!(on_bar(&mut drag, &hits, at(Press::Drag, 30)), BarUse::To(Scroll::Text, 30));
        assert_eq!(on_bar(&mut drag, &hits, at(Press::Release, 30)), BarUse::Held);
        assert_eq!(on_bar(&mut drag, &hits, at(Press::Drag, 6)), BarUse::No);
    }

    #[test]
    fn a_drag_follows_the_scrollbar_as_last_drawn() {
        let bar = Bar { scrolls: Scroll::List, y: 5, total: 40, view: 10, top: 0 };
        let mut drag = None;
        on_bar(&mut drag, &[((79, 5, 1, 10), Region::Bar(bar))], Mouse { press: Press::Click, x: 79, y: 5 });
        // meanwhile the list grew to 80 and the screen got taller
        let grown = Bar { total: 80, view: 20, y: 3, ..bar };
        let hits = vec![((79, 3, 1, 20), Region::Bar(grown)), ((79, 30, 1, 5), Region::Bar(Bar { scrolls: Scroll::Text, ..bar }))];
        assert_eq!(on_bar(&mut drag, &hits, Mouse { press: Press::Drag, x: 79, y: 40 }), BarUse::To(Scroll::List, 60));
        // gone from the screen: the one taken
        assert_eq!(on_bar(&mut drag, &[], Mouse { press: Press::Drag, x: 79, y: 40 }), BarUse::To(Scroll::List, 30));
    }

    #[test]
    fn a_press_elsewhere_lets_go_of_a_thumb_whose_release_was_lost() {
        let bar = Bar { scrolls: Scroll::List, y: 5, total: 40, view: 10, top: 15 };
        let hits = vec![((0, 5, 79, 10), Region::Column(1)), ((79, 5, 1, 10), Region::Bar(bar))];
        let mut drag = None;
        assert_eq!(on_bar(&mut drag, &hits, Mouse { press: Press::Click, x: 79, y: 10 }), BarUse::Held);
        // no release; a press on a row, then a drag: the row is the view's, the drag nobody's
        assert_eq!(on_bar(&mut drag, &hits, Mouse { press: Press::Click, x: 10, y: 7 }), BarUse::No);
        assert!(drag.is_none());
        assert_eq!(on_bar(&mut drag, &hits, Mouse { press: Press::Drag, x: 10, y: 12 }), BarUse::No);
    }

    #[test]
    fn a_click_lands_on_the_region_marked_last() {
        let mut hits = vec![((0, 0, 10, 10), Region::Column(1))];
        mark_rows(&mut hits, 0, 2, 10, 5, 3, 7);
        assert_eq!(hit(&hits, 4, 2), Some(Region::Row(5)));
        assert_eq!(hit(&hits, 9, 3), Some(Region::Row(6)));
        // past the last item: the column, not a row
        assert_eq!(hit(&hits, 4, 4), Some(Region::Column(1)));
        assert_eq!(hit(&hits, 10, 2), None);
        assert_eq!(hit(&hits, 0, 10), None);
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
            // Cut the row by column, after the 9-cell time: the pending spinner may
            // turn between status_seg and call_row. Every status mark is one cell.
            let row: Vec<char> = visible(&call_row(&call, 100, true, none())).chars().collect();
            let cell: String = row[9..9 + STATUS].iter().collect();
            let after: String = row[9 + STATUS..].iter().collect();
            assert!(cell.ends_with(' ') && !cell.trim().is_empty(), "{name}: {cell:?}");
            assert!(after.starts_with(&verdict_seg(&call).text), "{name}: {after:?}");
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
    fn rail_pairs_run_right_with_uneven_gaps() {
        for phase in [0, 7, 40, 154, 155, 1000] {
            let lit = rail(phase, 40);
            assert_eq!(lit.len(), 40);
            for (x, dot) in lit.iter().enumerate() {
                // every head has its dot behind it, unless that dot is off the left end
                if *dot == Some('●') && x > 0 {
                    assert_eq!(lit[x - 1], Some('•'), "phase {phase} cell {x}");
                }
            }
            // a step later everything sits one cell to the right
            let next = rail(phase + 1, 40);
            assert_eq!(next[1..], lit[..39], "phase {phase}");
        }
        let heads: Vec<usize> = rail(0, 120).iter().enumerate().filter(|(_, d)| **d == Some('●')).map(|(x, _)| x).collect();
        let gaps: Vec<usize> = heads.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(gaps.windows(2).any(|w| w[0] != w[1]), "{gaps:?}");
    }

    #[test]
    fn on_rail_dots_only_the_dashes() {
        let line = compose(30, &[seg("┌─ list ", blue(), 8), seg("─".repeat(21), blue(), 21), seg("┐", blue(), 1)], none());
        for phase in 0..200 {
            let railed = visible(&on_rail(line.clone(), phase));
            assert_eq!(width(&railed), 30);
            assert!(railed.starts_with('┌') && railed.ends_with('┐') && railed.contains(" list "), "{railed}");
        }
        assert!((0..200).any(|p| visible(&on_rail(line.clone(), p)).contains("•●")));
        // on a dim rule the dots are not dim
        let dim_rule = rule(30, "list", "", dim());
        let railed = (0..200).map(|p| on_rail(dim_rule.clone(), p)).find(|l| visible(l).contains('●')).unwrap();
        let dot = railed.spans.iter().find(|s| s.content.contains('●')).unwrap();
        assert!(!dot.style.add_modifier.contains(Modifier::DIM) && dot.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn header_says_how_long_the_agent_has_been_busy() {
        let mut ui = Ui::new("deck".into());
        let s = Session::new(PathBuf::from("/nonexistent"), None);
        assert!(!visible(&ui.header(&s, 80)).contains("busy"));
        ui.agent_busy = Some(now() - 12.0);
        let header = visible(&ui.header(&s, 80));
        assert_eq!(width(&header), 80);
        assert!(header.contains(" busy 12s "), "{header}");
        // done, it keeps what the stretch took until the next one
        ui.agent_busy = None;
        ui.agent_took = Some(252.0);
        let header = visible(&ui.header(&s, 80));
        assert_eq!(width(&header), 80);
        assert!(header.contains(" took 4m12s ") && !header.contains("busy"), "{header}");
        ui.agent_busy = Some(now());
        assert!(!visible(&ui.header(&s, 80)).contains("took"));
    }

    #[test]
    fn help_row_wraps_by_word_under_its_description() {
        let rows = help_row(30, "mouse", "wheel moves · click picks · double-click opens");
        let text: Vec<String> = rows.iter().map(visible).collect();
        assert_eq!(text, ["  mouse       wheel moves ·", "              click picks ·", "              double-click", "              opens"].map(|t| format!("{t:<30}")));
        assert_eq!(help_row(30, "q", "quit").len(), 1);
        let note: Vec<String> = hanging(20, "  ⏸ ", "waiting for a permission answer", dim()).iter().map(visible).collect();
        assert_eq!(note, ["  ⏸ waiting for a", "    permission", "    answer"].map(|t| format!("{t:<20}")));
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
