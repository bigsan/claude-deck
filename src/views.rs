//! The three views of one session, switched with `v`:
//!
//! ```text
//! A  tabs      Files / Tasks / Calls, a list over a preview; enter opens a call full
//!              screen, where n/p step through calls and e jumps to the next failure
//! B  columns   sources │ list │ detail, the detail following the cursor
//! C  timeline  files, tasks and calls in one stream, expanded in place
//! ```

use ratatui::text::Line;

use crate::data::Session;
use crate::text::{now, width};
use crate::ui::*;

pub trait View {
    fn name(&self) -> &'static str;
    fn help(&self) -> &'static [(&'static str, &'static str)];
    fn render(&mut self, ui: &mut Ui, s: &Session, cells: usize, height: usize) -> Vec<Line<'static>>;
    /// True when the key was used.
    fn handle(&mut self, ui: &mut Ui, s: &Session, key: &str) -> bool;
    /// Select `key` (the item picked in another view) if this view lists it.
    fn adopt(&mut self, ui: &mut Ui, s: &Session, key: &str);
    fn selection<'a>(&mut self, ui: &mut Ui, s: &'a Session) -> Option<Item<'a>>;
    /// True when the click or wheel was used; `ui.hits` says what is where.
    fn mouse(&mut self, ui: &mut Ui, s: &Session, m: Mouse) -> bool;
}

/// Wheel notches as lines to scroll.
const WHEEL_LINES: isize = 3;

fn scrolled(scroll: usize, notches: isize) -> usize {
    (scroll as isize + notches * WHEEL_LINES).max(0) as usize
}

const DOWN: &str = "\x1b[B";
const UP: &str = "\x1b[A";
const LEFT: &str = "\x1b[D";
const RIGHT: &str = "\x1b[C";
const PGUP: &str = "\x1b[5~";
const PGDN: &str = "\x1b[6~";
const ESC: &str = "\x1b";
const BS: &str = "\x7f";

fn is_enter(key: &str) -> bool {
    key == "\r" || key == "\n"
}

fn kind_of(key: &str) -> &str {
    key.split_once(':').map(|(k, _)| k).unwrap_or(key)
}

// ------------------------------------------------------------------ A

/// A: one source at a time, a list over a preview.
pub struct TabsView {
    tab: usize,
    keys: [Option<String>; 3],
    index: [usize; 3],
    /// At the top of a list the cursor stays on the newest item as new ones arrive.
    following: [bool; 3],
    tops: [usize; 3],
    full: bool,
    scroll: usize,
    /// the item `scroll` belongs to: another item starts at the top
    scrolled: Option<String>,
    preview: bool,
    family: usize,
}

impl TabsView {
    pub fn new() -> TabsView {
        TabsView { tab: 2, keys: Default::default(), index: [0; 3], following: [true; 3], tops: [0; 3], full: false, scroll: 0, scrolled: None, preview: true, family: 0 }
    }

    fn items<'a>(&self, ui: &Ui, s: &'a Session) -> Vec<Item<'a>> {
        let items = match self.tab {
            0 => s.files.iter().map(Item::File).collect(),
            1 => s.tasks.iter().map(Item::Task).collect(),
            _ => {
                let fam = FAMILIES[self.family];
                s.calls_newest().into_iter().filter(|c| fam == "all" || c.family.name() == fam).map(Item::Call).collect()
            }
        };
        ui.filtered(items)
    }

    fn current(&mut self, ui: &Ui, items: &[Item]) -> usize {
        let t = self.tab;
        let i = if self.following[t] { 0 } else { pick(items, self.keys[t].as_deref().or(ui.focus.as_deref()), self.index[t]) };
        self.index[t] = i;
        if let Some(item) = items.get(i) {
            self.keys[t] = Some(item.key());
        }
        i
    }

    fn move_to(&mut self, ui: &mut Ui, items: &[Item], i: isize) {
        if items.is_empty() {
            return;
        }
        let i = i.clamp(0, items.len() as isize - 1) as usize;
        let t = self.tab;
        self.index[t] = i;
        self.keys[t] = Some(items[i].key());
        self.following[t] = i == 0;
        ui.focus = Some(items[i].key());
        self.scroll = 0;
    }
}

const TABS_HELP: &[(&str, &str)] = &[
    ("1 2 3 tab", "files / tasks / calls"),
    ("j k", "move, or scroll when open"),
    ("l → enter", "open full screen"),
    ("h ← esc", "back to the list"),
    ("n p", "next / prev, also when open"),
    ("e", "next failure"),
    ("t", "calls: shell → file → other → all"),
    ("x", "failures only"),
    ("/", "filter"),
    ("space", "preview on / off"),
    ("y", "copy command or path"),
    ("esc", "back / clear filter"),
];

impl View for TabsView {
    fn name(&self) -> &'static str {
        "A tabs"
    }

    fn help(&self) -> &'static [(&'static str, &'static str)] {
        TABS_HELP
    }

    fn render(&mut self, ui: &mut Ui, s: &Session, cells: usize, height: usize) -> Vec<Line<'static>> {
        let now = now();
        let items = self.items(ui, s);
        let i = self.current(ui, &items);
        let fresh = s.files.iter().filter(|f| f.fresh(now)).count();
        let running = s.tasks.iter().any(|t| t.running(now));
        let failed = s.calls.values().filter(|c| c.failed() && !c.refused()).count();
        let tabs = [
            ("1 Files", format!(" {}", s.files.len()), if fresh > 0 { format!(" ●{fresh}") } else { String::new() }, yellow()),
            ("2 Tasks", format!(" {}", s.tasks.len()), if running { format!(" {}", spin()) } else { String::new() }, yellow()),
            ("3 Calls", format!(" {}", s.calls.len()), if failed > 0 { format!(" ✗{failed}") } else { String::new() }, red()),
        ];
        let mut segs = Vec::new();
        let mut x = 0;
        for (n, (title, count, mark, mark_style)) in tabs.into_iter().enumerate() {
            let bg = if n == self.tab { tab_bg() } else { none() };
            let title_style = if n == self.tab { bold().patch(bg) } else { none() };
            let (tw, cw, mw) = (width(title) + 1, width(&count), width(&mark) + 1);
            ui.mark((x, 1, tw + cw + mw, 1), Region::Tab(n));
            x += tw + cw + mw + 1;
            segs.push(seg(format!(" {title}"), title_style, tw));
            segs.push(seg(count, dim().patch(bg), cw));
            segs.push(seg(format!("{mark} "), mark_style.patch(bg), mw));
            segs.push(seg(" ", none(), 1));
        }
        segs.push(flex("", none()));
        let mut lines = vec![ui.header(s, cells), compose(cells, &segs, none())];
        let body = height.saturating_sub(lines.len() + 1);
        let item = items.get(i).copied();
        // another tab, or a new item followed at the top, not only move_to
        let key = item.map(|x| x.key());
        if key != self.scrolled {
            self.scroll = 0;
            self.scrolled = key;
        }
        if self.full && item.is_some() {
            let (detail, image) = detail_of(ui, item, i, items.len(), cells, s, now);
            let kind = ["file", "task", "call"][self.tab];
            lines.push(rule(cells, &format!("{kind} {}/{}", i + 1, items.len()), "n/p next · e failure · h/esc back", dim()));
            self.scroll = self.scroll.min((detail.len() + 1).saturating_sub(body));
            ui.mark((0, lines.len(), cells, body.saturating_sub(1)), Region::Preview);
            lines.extend(window(&detail, self.scroll, body.saturating_sub(1), cells));
            if let Some(path) = image {
                ui.want_image(path, (0, lines.len() + 1 - body.saturating_sub(1), cells, body.saturating_sub(2)));
            }
        } else {
            let title = match self.tab {
                0 => "scratchpad · newest first".to_string(),
                1 => "tasks/".to_string(),
                _ => format!("calls · {}", FAMILIES[self.family]),
            };
            lines.push(rule(cells, &title, &ui.filter_label(), dim()));
            let list_h = if self.preview && item.is_some() && body >= 10 { 3.max((body - 2) * 45 / 100) } else { body.saturating_sub(1) };
            let t = self.tab;
            self.tops[t] = visible_top(i, self.tops[t], list_h, items.len());
            let mut rows: Vec<Line<'static>> = items
                .iter()
                .enumerate()
                .skip(self.tops[t])
                .take(list_h)
                .map(|(n, x)| item_row(*x, cells, now, s.has_gate, if n == i { sel_bg() } else { none() }))
                .collect();
            if items.is_empty() {
                let empty = if ui.query.is_empty() && !ui.failed_only { "  (nothing yet)" } else { "  (nothing matches)" };
                rows = vec![plain(cells, empty, dim())];
            }
            mark_rows(&mut ui.hits, 0, lines.len(), cells, self.tops[t], list_h, items.len());
            lines.extend(window(&rows, 0, list_h, cells));
            if list_h + 1 < body {
                lines.push(rule(cells, "", "", dim()));
                let (detail, image) = detail_of(ui, item, i, items.len(), cells, s, now);
                let preview_h = body - 1 - list_h - 1;
                // the wheel scrolls the preview; moving the cursor puts it back at the top
                self.scroll = self.scroll.min(detail.len().saturating_sub(preview_h));
                ui.mark((0, lines.len(), cells, preview_h), Region::Preview);
                lines.extend(window(&detail, self.scroll, preview_h, cells));
                if let Some(path) = image {
                    ui.want_image(path, (0, lines.len() + 1 - preview_h, cells, preview_h.saturating_sub(1)));
                }
            }
        }
        lines.truncate(height.saturating_sub(1));
        while lines.len() < height.saturating_sub(1) {
            lines.push(blank(cells));
        }
        let hints: &[(&str, &str)] = if self.full {
            &[("h", "back"), ("j/k", "scroll"), ("n/p", "next"), ("e", "failure"), ("= # u", "format"), ("?", "keys")]
        } else {
            &[("1-3", "tab"), ("j/k", "move"), ("l/⏎", "open"), ("n/p", "next"), ("e", "failure"), ("v", "view"), ("?", "keys")]
        };
        lines.push(ui.footer(cells, hints));
        lines
    }

    fn handle(&mut self, ui: &mut Ui, s: &Session, key: &str) -> bool {
        let items = self.items(ui, s);
        let i = self.current(ui, &items) as isize;
        if self.full {
            match key {
                "j" | DOWN => self.scroll += 1,
                "k" | UP => self.scroll = self.scroll.saturating_sub(1),
                " " | PGDN => self.scroll += 10,
                PGUP => self.scroll = self.scroll.saturating_sub(10),
                "n" => self.move_to(ui, &items, i + 1),
                "p" | "N" => self.move_to(ui, &items, i - 1),
                "e" if !items.is_empty() => self.move_to(ui, &items, next_failure(&items, i as usize, 1) as isize),
                ESC | "q" | BS | "h" | LEFT => {
                    self.full = false;
                    self.scroll = 0;
                }
                _ => return false,
            }
            return true;
        }
        match key {
            "1" | "2" | "3" => self.tab = key.parse::<usize>().unwrap_or(1) - 1,
            "\t" => self.tab = (self.tab + 1) % 3,
            "j" | DOWN => self.move_to(ui, &items, i + 1),
            "k" | UP => self.move_to(ui, &items, i - 1),
            "g" => self.move_to(ui, &items, 0),
            "G" => self.move_to(ui, &items, items.len() as isize - 1),
            k if (is_enter(k) || k == "l" || k == RIGHT) && !items.is_empty() => {
                self.full = true;
                self.scroll = 0;
            }
            "n" | "p" | "N" if !items.is_empty() => self.move_to(ui, &items, i + if key == "n" { 1 } else { -1 }),
            "e" if !items.is_empty() => self.move_to(ui, &items, next_failure(&items, i as usize, 1) as isize),
            "t" => self.family = (self.family + 1) % FAMILIES.len(),
            " " => self.preview = !self.preview,
            _ => return false,
        }
        true
    }

    fn adopt(&mut self, _ui: &mut Ui, _s: &Session, key: &str) {
        self.tab = match kind_of(key) {
            "file" => 0,
            "task" => 1,
            _ => 2,
        };
        self.keys[self.tab] = Some(key.to_string());
        self.following[self.tab] = false;
    }

    fn selection<'a>(&mut self, ui: &mut Ui, s: &'a Session) -> Option<Item<'a>> {
        let items = self.items(ui, s);
        let i = self.current(ui, &items);
        items.get(i).copied()
    }

    fn mouse(&mut self, ui: &mut Ui, s: &Session, m: Mouse) -> bool {
        let items = self.items(ui, s);
        let i = self.current(ui, &items) as isize;
        match (hit(&ui.hits, m.x, m.y), m.press) {
            (Some(Region::Tab(n)), Press::Click | Press::Double) => {
                self.tab = n;
                self.full = false;
                self.scroll = 0;
            }
            (Some(Region::Row(n)), Press::Click) => self.move_to(ui, &items, n as isize),
            (Some(Region::Row(n)), Press::Double) => {
                self.move_to(ui, &items, n as isize);
                self.full = true;
            }
            (Some(Region::Preview), Press::Wheel(d)) => self.scroll = scrolled(self.scroll, d),
            (_, Press::Wheel(d)) if self.full => self.scroll = scrolled(self.scroll, d),
            // over the list, or anywhere else, the wheel moves the cursor
            (_, Press::Wheel(d)) => self.move_to(ui, &items, i + d),
            _ => return false,
        }
        true
    }
}

// ------------------------------------------------------------------ B

/// The focused box: heavy blue lines.
fn focus_line() -> ratatui::style::Style {
    blue().add_modifier(ratatui::style::Modifier::BOLD)
}

/// A focused title: white on blue.
fn focus_title() -> ratatui::style::Style {
    ratatui::style::Style::new().fg(ratatui::style::Color::White).bg(ratatui::style::Color::Indexed(25)).add_modifier(ratatui::style::Modifier::BOLD)
}

/// The selected row: bright where the keys go, grey elsewhere.
fn selection(active: bool) -> ratatui::style::Style {
    if active { ratatui::style::Style::new().bg(ratatui::style::Color::Indexed(25)) } else { sel_bg() }
}

/// A section rule in the stacked (narrow) layout: heavy with a ▶ title when focused.
fn focus_rule(cells: usize, title: &str, right: &str, focused: bool) -> Line<'static> {
    if !focused {
        return rule(cells, title, right, dim());
    }
    let label = format!(" ▶ {title} ");
    let tail = if right.is_empty() { "━".to_string() } else { format!(" {right} ━") };
    let (lw, tw) = (width(&label), width(&tail));
    let fill = cells.saturating_sub(1 + lw + tw);
    compose(cells, &[seg("━", focus_line(), 1), seg(label, focus_title(), lw), seg("━".repeat(fill), focus_line(), fill), seg(tail, focus_line(), tw)], none())
}

/// Columns side by side, each in a box; the focused column's box is drawn in heavy
/// blue lines with its title on blue, the others in thin grey ones.
///
/// Neighbouring boxes share a border, which is heavy when it belongs to the focused column.
pub fn framed(shown: &[usize], widths: &[usize; 3], titles: &[String; 3], content: &[Vec<Line<'static>>; 3], focus: usize, height: usize) -> Vec<Line<'static>> {
    use ratatui::text::Span;
    // n-th border, 0 = left edge
    let heavy = |n: usize| shown[n.saturating_sub(1)..(n + 1).min(shown.len())].contains(&focus);
    let bar = |n: usize, light: &str, thick: &str| if heavy(n) { Span::styled(thick.to_string(), focus_line()) } else { Span::styled(light.to_string(), dim()) };
    let last = shown.len();
    let mut top = Vec::new();
    let mut bottom = Vec::new();
    for (n, &col) in shown.iter().enumerate() {
        let w = widths[col];
        top.push(if n == 0 { bar(n, "┌", "┏") } else { bar(n, "┬", "┳") });
        bottom.push(if n == 0 { bar(n, "└", "┗") } else { bar(n, "┴", "┻") });
        if col == focus {
            let label = crate::text::fit(&format!(" ▶ {} ", titles[col]), w.saturating_sub(1), false, false);
            let fill = w.saturating_sub(1 + width(&label));
            top.push(Span::styled("━", focus_line()));
            top.push(Span::styled(label, focus_title()));
            top.push(Span::styled("━".repeat(fill), focus_line()));
            bottom.push(Span::styled("━".repeat(w), focus_line()));
        } else {
            let label = crate::text::fit(&format!("─ {} ", titles[col]), w, false, false);
            let fill = w.saturating_sub(width(&label));
            top.push(Span::styled(format!("{label}{}", "─".repeat(fill)), dim()));
            bottom.push(Span::styled("─".repeat(w), dim()));
        }
    }
    top.push(bar(last, "┐", "┓"));
    bottom.push(bar(last, "┘", "┛"));
    let mut rows = vec![Line::from(top)];
    for r in 0..height {
        let mut row = Vec::new();
        for (n, &col) in shown.iter().enumerate() {
            row.push(bar(n, "│", "┃"));
            match content[col].get(r) {
                Some(line) => row.extend(line.spans.iter().cloned()),
                None => row.push(Span::raw(" ".repeat(widths[col]))),
            }
        }
        row.push(bar(last, "│", "┃"));
        rows.push(Line::from(row));
    }
    rows.push(Line::from(bottom));
    rows
}

const SOURCES: [(&str, &str); 11] = [
    ("scratch", "SCRATCHPAD"),
    ("images", " └ images"),
    ("tasks", "TASKS"),
    ("tshell", " └ bg shell"),
    ("tagent", " └ subagents"),
    ("calls", "CALLS"),
    ("shell", " └ shell"),
    ("file", " └ file"),
    ("other", " └ other"),
    ("failed", " └ ✗ failed"),
    ("denied", " └ ⊘ denied"),
];

const COLUMNS_HELP: &[(&str, &str)] = &[
    ("h l ← → tab", "move between columns (the blue box has focus)"),
    ("j k", "move, or scroll in the detail"),
    ("n p", "next / prev item from anywhere"),
    ("g G", "first / last in the focused column"),
    ("[ ]", "previous / next source"),
    ("e", "next failure"),
    ("z", "zoom the detail"),
    ("x", "failures only"),
    ("/", "filter"),
    ("y", "copy command or path"),
    ("esc", "back a column"),
];

/// B: sources │ list │ detail, the detail following the cursor.
pub struct ColumnsView {
    source: usize,
    focus: usize,
    keys: std::collections::HashMap<&'static str, String>,
    index: std::collections::HashMap<&'static str, usize>,
    following: std::collections::HashMap<&'static str, bool>,
    top: usize,
    scroll: usize,
    zoom: bool,
}

impl ColumnsView {
    pub fn new() -> ColumnsView {
        ColumnsView { source: 6, focus: 1, keys: Default::default(), index: Default::default(), following: Default::default(), top: 0, scroll: 0, zoom: false }
    }

    fn sid(&self) -> &'static str {
        SOURCES[self.source].0
    }

    fn items_for<'a>(&self, ui: &Ui, s: &'a Session, source: &str) -> Vec<Item<'a>> {
        let calls = || s.calls_newest().into_iter();
        let items: Vec<Item<'a>> = match source {
            "scratch" => s.files.iter().map(Item::File).collect(),
            "images" => s.files.iter().filter(|f| f.rel.starts_with("images/") || f.is_image).map(Item::File).collect(),
            "tasks" => s.tasks.iter().map(Item::Task).collect(),
            "tshell" => s.tasks.iter().filter(|t| t.kind == crate::data::TaskKind::Shell).map(Item::Task).collect(),
            "tagent" => s.tasks.iter().filter(|t| t.kind == crate::data::TaskKind::Agent).map(Item::Task).collect(),
            "calls" => calls().map(Item::Call).collect(),
            "failed" => calls().filter(|c| c.failed() && !c.refused()).map(Item::Call).collect(),
            "denied" => calls().filter(|c| c.refused()).map(Item::Call).collect(),
            family => calls().filter(|c| c.family.name() == family).map(Item::Call).collect(),
        };
        ui.filtered(items)
    }

    fn selected(&mut self, ui: &Ui, items: &[Item]) -> usize {
        let sid = self.sid();
        let i = if *self.following.get(sid).unwrap_or(&true) {
            0
        } else {
            pick(items, self.keys.get(sid).map(String::as_str).or(ui.focus.as_deref()), *self.index.get(sid).unwrap_or(&0))
        };
        self.index.insert(sid, i);
        if let Some(item) = items.get(i) {
            self.keys.insert(sid, item.key());
        }
        i
    }

    fn move_to(&mut self, ui: &mut Ui, items: &[Item], i: isize) {
        if items.is_empty() {
            return;
        }
        let sid = self.sid();
        let i = i.clamp(0, items.len() as isize - 1) as usize;
        self.index.insert(sid, i);
        self.keys.insert(sid, items[i].key());
        self.following.insert(sid, i == 0);
        ui.focus = Some(items[i].key());
        self.scroll = 0;
    }

    fn badge(&self, ui: &Ui, s: &Session, sid: &str) -> Seg {
        let count = self.items_for(ui, s, sid).len();
        if sid == "scratch" {
            let now = now();
            let fresh = s.files.iter().filter(|f| f.fresh(now)).count();
            return if fresh > 0 { seg_r(format!("●{fresh} {count}"), yellow(), 7) } else { seg_r(count.to_string(), dim(), 7) };
        }
        if sid == "failed" || sid == "denied" {
            return seg_r(count.to_string(), if count > 0 { red() } else { dim() }, 7);
        }
        seg_r(count.to_string(), dim(), 7)
    }

    fn list_rows(&mut self, items: &[Item], i: usize, cells: usize, height: usize, now: f64) -> Vec<Line<'static>> {
        let active = self.focus == 1;
        self.top = visible_top(i, self.top, height, items.len());
        let mut rows: Vec<Line<'static>> =
            items.iter().enumerate().skip(self.top).take(height).map(|(n, x)| item_row(*x, cells, now, false, if n == i { selection(active) } else { none() })).collect();
        if items.is_empty() {
            rows = vec![plain(cells, "  —", dim())];
        }
        window(&rows, 0, height, cells)
    }
}

impl View for ColumnsView {
    fn name(&self) -> &'static str {
        "B columns"
    }

    fn help(&self) -> &'static [(&'static str, &'static str)] {
        COLUMNS_HELP
    }

    fn render(&mut self, ui: &mut Ui, s: &Session, cells: usize, height: usize) -> Vec<Line<'static>> {
        let now = now();
        let (sid, label) = SOURCES[self.source];
        let items = self.items_for(ui, s, sid);
        let i = self.selected(ui, &items);
        let item = items.get(i).copied();
        let mut lines = vec![ui.header(s, cells)];
        let body = height.saturating_sub(2);
        let focus = self.focus;
        if cells < 100 {
            let dots: String = (0..SOURCES.len()).map(|n| if n == self.source { '●' } else { '·' }).collect();
            let name = label.replace(" └ ", "");
            let count = items.len().to_string();
            let (nw, cw, dw) = (width(&name) + 1, count.len() + 1, dots.chars().count());
            lines.push(compose(
                cells,
                &if self.focus == 0 {
                    // the source chooser has the keys: j/k change the source
                    [seg(" ▶ ", focus_title(), 3), seg(name, focus_title(), nw), seg(count, focus_title(), cw), seg(" j/k", focus_line(), 4), flex("", none()), seg(dots, blue(), dw)]
                } else {
                    [seg("[ ", blue(), 2), seg(name, bold(), nw), seg(count, dim(), cw), seg("]", blue(), 1), flex("", none()), seg(dots, dim(), dw)]
                },
                none(),
            ));
            let list_h = 3.max(body.saturating_sub(3) * 40 / 100);
            ui.mark((0, 1, cells, 1), Region::Column(0));
            // one dot per source at the right end of the line
            (0..dw).for_each(|n| ui.mark((cells.saturating_sub(dw) + n, 1, 1, 1), Region::Source(n)));
            ui.mark((0, 2, cells, 1 + list_h), Region::Column(1));
            lines.push(focus_rule(cells, &format!("{} · newest first", label.trim()), &ui.filter_label(), focus == 1));
            lines.extend(self.list_rows(&items, i, cells, list_h, now));
            mark_rows(&mut ui.hits, 0, 3, cells, self.top, list_h, items.len());
            let (detail, image) = if item.is_some() { detail_of(ui, item, i, items.len(), cells, s, now) } else { (vec![plain(cells, "empty", dim())], None) };
            let detail_h = body.saturating_sub(2 + list_h + 1);
            let title = if items.is_empty() { "detail".to_string() } else { format!("detail {}/{}", i + 1, items.len()) };
            ui.mark((0, lines.len(), cells, 1 + detail_h), Region::Column(2));
            lines.push(focus_rule(cells, &title, if focus == 2 { "j/k scroll" } else { "" }, focus == 2));
            self.scroll = self.scroll.min(detail.len().saturating_sub(detail_h));
            lines.extend(window(&detail, self.scroll, detail_h, cells));
            if let Some(path) = image {
                ui.want_image(path, (0, lines.len() + 1 - detail_h, cells, detail_h.saturating_sub(1)));
            }
        } else {
            // Three boxed columns; the focused one gets a blue frame. Borders take
            // one cell between columns and one at each edge.
            let shown: Vec<usize> = if self.zoom { vec![2] } else { vec![0, 1, 2] };
            let inner = cells - (shown.len() + 1);
            let src_w = 22;
            let list_w = 36.max((inner - src_w) * 45 / 100);
            let widths = if self.zoom { [0, 0, inner] } else { [src_w, list_w, inner.saturating_sub(src_w + list_w)] };
            let box_h = body.saturating_sub(2);
            let mut content: [Vec<Line<'static>>; 3] = Default::default();
            let mut titles: [String; 3] = Default::default();
            if !self.zoom {
                let src_rows: Vec<Line<'static>> = SOURCES
                    .iter()
                    .enumerate()
                    .map(|(n, (other, text))| {
                        let bg = if n == self.source { selection(self.focus == 0) } else { none() };
                        compose(src_w, &[flex(format!(" {text}"), if text.starts_with(' ') { none() } else { bold() }), self.badge(ui, s, other)], bg)
                    })
                    .collect();
                content[0] = window(&src_rows, 0, box_h, src_w);
                titles[0] = "sources".into();
                content[1] = self.list_rows(&items, i, list_w, box_h, now);
                let filter = ui.filter_label();
                titles[1] = format!("{} · {}", label.trim(), if filter.is_empty() { "newest first".to_string() } else { filter });
            }
            let det_w = widths[2];
            let (detail, image) =
                if item.is_some() { detail_of(ui, item, i, items.len(), det_w - 1, s, now) } else { (vec![plain(det_w - 1, "nothing here yet", dim())], None) };
            self.scroll = self.scroll.min(detail.len().saturating_sub(box_h));
            content[2] = window(&detail, self.scroll, box_h, det_w - 1)
                .into_iter()
                .map(|line| {
                    let mut spans = vec![ratatui::text::Span::raw(" ")];
                    spans.extend(line.spans);
                    Line::from(spans)
                })
                .collect();
            titles[2] = format!("{}{}", if items.is_empty() { "detail".to_string() } else { format!("detail {}/{}", i + 1, items.len()) }, if self.zoom { " · zoomed" } else { "" });
            // each column's box, then the rows inside it, which lie over the box
            let mut x = 1;
            for &col in &shown {
                ui.mark((x, 1, widths[col], box_h + 2), Region::Column(col));
                match col {
                    0 => (0..SOURCES.len().min(box_h)).for_each(|n| ui.mark((x, 2 + n, widths[0], 1), Region::Source(n))),
                    1 => mark_rows(&mut ui.hits, x, 2, widths[1], self.top, box_h, items.len()),
                    _ => {}
                }
                x += widths[col] + 1;
            }
            lines.extend(framed(&shown, &widths, &titles, &content, self.focus, box_h));
            if let Some(path) = image {
                let left = 1 + shown.iter().filter(|&&c| c < 2).map(|&c| widths[c] + 1).sum::<usize>() + 1;
                ui.want_image(path, (left, 3, det_w.saturating_sub(2), box_h.saturating_sub(2)));
            }
        }
        lines.truncate(height.saturating_sub(1));
        while lines.len() < height.saturating_sub(1) {
            lines.push(blank(cells));
        }
        let hints = [("h/l", "column"), ("j/k", "move"), ("n/p", "next"), ("e", "failure"), ("[ ]", "source"), ("z", "zoom"), ("v", "view"), ("?", "keys")];
        lines.push(ui.footer(cells, &hints));
        lines
    }

    fn handle(&mut self, ui: &mut Ui, s: &Session, key: &str) -> bool {
        let items = self.items_for(ui, s, self.sid());
        let i = self.selected(ui, &items) as isize;
        let (down, up) = (key == "j" || key == DOWN, key == "k" || key == UP);
        match key {
            "[" | "]" => {
                self.source = (self.source as isize + if key == "]" { 1 } else { -1 }).rem_euclid(SOURCES.len() as isize) as usize;
                self.scroll = 0;
            }
            k if k == "\t" || k == "l" || k == RIGHT || (is_enter(k) && self.focus < 2) => self.focus = (self.focus + 1).min(2),
            // in the first column these go up, to the list of sessions
            k if (k == "h" || k == LEFT || k == ESC) && !self.zoom && self.focus > 0 => self.focus -= 1,
            ESC if self.zoom => self.zoom = false,
            "z" => {
                self.zoom = !self.zoom;
                if self.zoom {
                    self.focus = 2;
                }
            }
            "n" => self.move_to(ui, &items, i + 1),
            "p" | "N" => self.move_to(ui, &items, i - 1),
            "e" if !items.is_empty() => self.move_to(ui, &items, next_failure(&items, i as usize, 1) as isize),
            _ if down || up => {
                let step: isize = if down { 1 } else { -1 };
                match self.focus {
                    0 => {
                        self.source = (self.source as isize + step).rem_euclid(SOURCES.len() as isize) as usize;
                        self.scroll = 0;
                    }
                    1 => self.move_to(ui, &items, i + step),
                    _ => self.scroll = (self.scroll as isize + step).max(0) as usize,
                }
            }
            " " | PGDN => self.scroll += 10,
            PGUP => self.scroll = self.scroll.saturating_sub(10),
            // first / last of whatever the focused column holds
            "g" | "G" => {
                let first = key == "g";
                match self.focus {
                    0 => {
                        self.source = if first { 0 } else { SOURCES.len() - 1 };
                        self.scroll = 0;
                    }
                    1 => self.move_to(ui, &items, if first { 0 } else { items.len() as isize - 1 }),
                    // render clamps the scroll to the end of the detail
                    _ => self.scroll = if first { 0 } else { usize::MAX / 2 },
                }
            }
            _ => return false,
        }
        true
    }

    fn adopt(&mut self, ui: &mut Ui, s: &Session, key: &str) {
        if !self.items_for(ui, s, self.sid()).iter().any(|x| x.key() == key) {
            self.source = match kind_of(key) {
                "file" => 0,
                "task" => 2,
                _ => 5,
            };
        }
        let sid = self.sid();
        self.keys.insert(sid, key.to_string());
        self.following.insert(sid, false);
    }

    fn selection<'a>(&mut self, ui: &mut Ui, s: &'a Session) -> Option<Item<'a>> {
        let items = self.items_for(ui, s, self.sid());
        let i = self.selected(ui, &items);
        items.get(i).copied()
    }

    fn mouse(&mut self, ui: &mut Ui, s: &Session, m: Mouse) -> bool {
        let items = self.items_for(ui, s, self.sid());
        let i = self.selected(ui, &items) as isize;
        let region = hit(&ui.hits, m.x, m.y);
        let Press::Wheel(d) = m.press else {
            match region {
                Some(Region::Source(n)) => {
                    self.source = n;
                    self.scroll = 0;
                    // a double-click goes on into the list, as enter does
                    self.focus = if m.press == Press::Double { 1 } else { 0 };
                }
                Some(Region::Row(n)) => {
                    self.move_to(ui, &items, n as isize);
                    self.focus = if m.press == Press::Double { 2 } else { 1 };
                }
                Some(Region::Column(col)) => self.focus = col,
                _ => return false,
            }
            return true;
        };
        // the wheel works on the column under it, focused or not
        match region {
            Some(Region::Source(_) | Region::Column(0)) => {
                self.source = (self.source as isize + d).rem_euclid(SOURCES.len() as isize) as usize;
                self.scroll = 0;
            }
            Some(Region::Row(_) | Region::Column(1)) => self.move_to(ui, &items, i + d),
            Some(Region::Column(_)) => self.scroll = scrolled(self.scroll, d),
            _ => return false,
        }
        true
    }
}

// ------------------------------------------------------------------ C

const TIMELINE_HELP: &[(&str, &str)] = &[
    ("j k", "move, or scroll when open"),
    ("enter space", "expand in place"),
    ("n p", "next / prev command (expansion follows)"),
    ("e", "next failure"),
    ("l → o", "open full screen"),
    ("h ← esc", "back; in the list, collapse"),
    ("f t s c", "toggle files / tasks / shell / other calls"),
    ("x", "failures only"),
    ("/", "filter"),
    ("y", "copy command or path"),
    ("esc", "collapse / back"),
];

/// C: files, tasks and calls in one stream, newest first.
pub struct TimelineView {
    /// files, tasks, shell calls, other calls
    show: [bool; 4],
    key: Option<String>,
    index: usize,
    open: Option<String>,
    following: bool,
    full: bool,
    scroll: usize,
    top: usize,
    /// the item the last click picked, and whether it was expanded before that click
    clicked: Option<(String, bool)>,
}

const SHOW_KEYS: [&str; 4] = ["f", "t", "s", "c"];

impl TimelineView {
    pub fn new() -> TimelineView {
        TimelineView { show: [true, true, true, false], key: None, index: 0, open: None, following: true, full: false, scroll: 0, top: 0, clicked: None }
    }

    fn items<'a>(&self, ui: &Ui, s: &'a Session) -> Vec<Item<'a>> {
        let mut out: Vec<Item<'a>> = Vec::new();
        if self.show[0] {
            out.extend(s.files.iter().take(300).map(Item::File));
        }
        if self.show[1] {
            out.extend(s.tasks.iter().map(Item::Task));
        }
        for call in s.calls_in_order() {
            let shell = call.family == crate::data::Family::Shell;
            if (shell && self.show[2]) || (!shell && self.show[3]) {
                out.push(Item::Call(call));
            }
        }
        out.sort_by(|a, b| b.at().total_cmp(&a.at()));
        ui.filtered(out)
    }

    fn selected(&mut self, ui: &Ui, items: &[Item]) -> usize {
        let i = if self.following { 0 } else { pick(items, self.key.as_deref().or(ui.focus.as_deref()), self.index) };
        self.index = i;
        if let Some(item) = items.get(i) {
            self.key = Some(item.key());
        }
        i
    }

    fn move_to(&mut self, ui: &mut Ui, items: &[Item], i: isize) {
        if items.is_empty() {
            return;
        }
        let i = i.clamp(0, items.len() as isize - 1) as usize;
        self.index = i;
        self.key = Some(items[i].key());
        self.following = i == 0;
        ui.focus = self.key.clone();
        if self.open.is_some() {
            self.open = self.key.clone();
        }
        self.scroll = 0;
    }

    fn jump(&mut self, ui: &mut Ui, items: &[Item], i: usize, step: isize, test: impl Fn(&Item) -> bool) {
        let mut j = i as isize + step;
        while 0 <= j && (j as usize) < items.len() {
            if test(&items[j as usize]) {
                self.move_to(ui, items, j);
                return;
            }
            j += step;
        }
    }

    fn row(&self, s: &Session, item: Item, cells: usize, now: f64, bg: ratatui::style::Style) -> Line<'static> {
        let t = seg(crate::text::hms(item.at()), dim(), 9);
        let wide = cells >= 70;
        let mut segs = vec![t];
        match item {
            Item::File(f) => {
                let fresh = f.fresh(now);
                segs.push(seg(if fresh { "●" } else { "✎" }, if fresh { yellow() } else { dim() }, 2));
                if wide {
                    segs.push(seg("file", dim(), 6));
                }
                segs.push(flex(&f.rel, if fresh { bold() } else { none() }));
                segs.extend(cached_seg(f));
                segs.push(seg_r(crate::text::human_size(f.size), dim(), 8));
            }
            Item::Task(task) => {
                let agent = task.kind == crate::data::TaskKind::Agent;
                segs.push(seg(if agent { "◆" } else { "⚙" }, if agent { magenta() } else { cyan() }, 2));
                if wide {
                    segs.push(seg(if agent { "agent" } else { "task" }, dim(), 6));
                }
                segs.push(flex(&task.name, none()));
                segs.push(task_status(task, now));
            }
            Item::Call(call) => {
                segs.push(seg("❯", blue(), 2));
                if wide {
                    segs.push(seg(call.family.name(), dim(), 6));
                }
                segs.push(status_seg(call));
                if wide && s.has_gate {
                    segs.push(verdict_seg(call));
                }
                segs.push(seg(if call.agent.is_some() { "◆" } else { " " }, magenta(), 2));
                if call.family != crate::data::Family::Shell {
                    segs.push(seg(&call.tool, cyan(), 10.min(width(&call.tool) + 1)));
                }
                segs.push(flex(&call.summary, none()));
                if wide {
                    segs.push(seg_r(crate::text::human_duration(call.duration_ms()), dim(), 7));
                }
            }
        }
        compose(cells, &segs, bg)
    }

    fn expansion(&self, s: &Session, item: Item, cells: usize) -> Vec<Line<'static>> {
        let inner = cells.saturating_sub(11);
        let (body, text, style): (Vec<Line<'static>>, String, ratatui::style::Style) = match item {
            Item::Call(call) => {
                let command = call.input["command"].as_str().filter(|c| !c.is_empty()).unwrap_or(&call.summary);
                let body = crate::text::wrap(command, inner).iter().take(4).map(|l| plain_bg(inner, l, none(), cmd_bg())).collect();
                let live = if call.pending() { call.live.as_deref().and_then(|n| s.task_named(n)).filter(|t| !t.text.is_empty()) } else { None };
                let text = if !call.output.is_empty() {
                    call.output.clone()
                } else if let Some(t) = live {
                    t.text.clone()
                } else if call.pending() {
                    "(running)".into()
                } else {
                    "(no output)".into()
                };
                (body, text, if call.failed() { red() } else { none() })
            }
            Item::Task(task) => (Vec::new(), if task.text.is_empty() { "(empty)".into() } else { task.text.clone() }, none()),
            Item::File(f) => {
                if f.is_image {
                    return gutter(vec![plain(inner, "image · o to view", dim())]);
                }
                (Vec::new(), crate::data::read_text(&f.path, crate::data::PREVIEW_BYTES, false).unwrap_or_else(|| "(binary file)".into()), none())
            }
        };
        let lines: Vec<&str> = text.split('\n').collect();
        let head = lines.iter().take(8).copied().collect::<Vec<_>>().join("\n");
        let mut shown: Vec<Line<'static>> = crate::text::wrap(&head, inner).iter().take(10).map(|l| plain(inner, l, style)).collect();
        if lines.len() > 8 {
            shown.push(plain(inner, &format!("… {} more lines · o full screen", lines.len() - 8), dim()));
        }
        gutter(body.into_iter().chain(shown).collect())
    }
}

/// Lines indented under the time column of the timeline, behind a dim bar.
fn gutter(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    use ratatui::text::Span;
    lines
        .into_iter()
        .map(|line| {
            let mut spans = vec![Span::raw(" ".repeat(9)), Span::styled("│", dim()), Span::raw(" ")];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

impl View for TimelineView {
    fn name(&self) -> &'static str {
        "C timeline"
    }

    fn help(&self) -> &'static [(&'static str, &'static str)] {
        TIMELINE_HELP
    }

    fn render(&mut self, ui: &mut Ui, s: &Session, cells: usize, height: usize) -> Vec<Line<'static>> {
        let now = now();
        let items = self.items(ui, s);
        let i = self.selected(ui, &items);
        let mut chips = Vec::new();
        let mut x = 0;
        for (n, name) in ["files", "tasks", "shell", "other calls"].iter().enumerate() {
            chips.push(seg(SHOW_KEYS[n], blue(), 1));
            chips.push(seg(format!(" {name}  "), if self.show[n] { none() } else { dim() }, width(name) + 3));
            // full screen the keys leave these alone, and so do clicks
            if !self.full {
                ui.mark((x, 1, width(name) + 2, 1), Region::Tab(n));
            }
            x += width(name) + 4;
        }
        if !self.full {
            ui.mark((x, 1, 15, 1), Region::Tab(SHOW_KEYS.len()));
        }
        chips.push(seg("x", blue(), 1));
        chips.push(seg(" failures only", if ui.failed_only { red() } else { dim() }, 14));
        chips.push(flex("", none()));
        let mut lines = vec![ui.header(s, cells), compose(cells, &chips, none())];
        let body = height.saturating_sub(lines.len() + 1);
        if self.full && !items.is_empty() {
            let item = items[i];
            let calls: Vec<String> = items.iter().filter(|x| matches!(x, Item::Call(_))).map(Item::key).collect();
            let n = calls.iter().position(|k| *k == item.key()).unwrap_or(0);
            let (detail, image) = detail_of(ui, Some(item), n, calls.len(), cells, s, now);
            lines.push(rule(cells, "full view", "n/p command · e failure · h/esc back", dim()));
            self.scroll = self.scroll.min((detail.len() + 1).saturating_sub(body));
            ui.mark((0, lines.len(), cells, body.saturating_sub(1)), Region::Preview);
            lines.extend(window(&detail, self.scroll, body.saturating_sub(1), cells));
            if let Some(path) = image {
                ui.want_image(path, (0, lines.len() + 1 - body.saturating_sub(1), cells, body.saturating_sub(2)));
            }
        } else {
            let right = format!("{} {} events", ui.filter_label(), items.len()).trim().to_string();
            lines.push(rule(cells, "live · newest first", &right, dim()));
            let mut display: Vec<Line<'static>> = Vec::new();
            // the item each display line belongs to, for clicks
            let mut owners: Vec<Option<usize>> = Vec::new();
            let (mut sel_first, mut sel_last) = (0, 0);
            let mut last_at: Option<f64> = None;
            for (n, item) in items.iter().enumerate() {
                if let Some(last) = last_at {
                    if last - item.at() > 600.0 {
                        display.push(rule(cells, &format!("{} · {} min earlier", crate::text::hm(item.at()), ((last - item.at()) / 60.0) as i64), "", dim()));
                        owners.push(None);
                    }
                }
                last_at = Some(item.at());
                if n == i {
                    sel_first = display.len();
                }
                display.push(self.row(s, *item, cells, now, if n == i { sel_bg() } else { none() }));
                if self.open.as_deref() == Some(item.key().as_str()) {
                    display.extend(self.expansion(s, *item, cells));
                }
                owners.resize(display.len(), Some(n));
                if n == i {
                    sel_last = display.len() - 1;
                }
            }
            let view_h = body.saturating_sub(1);
            if sel_first < self.top {
                self.top = sel_first;
            } else if sel_last >= self.top + view_h {
                self.top = sel_first.min((sel_last + 1).saturating_sub(view_h));
            }
            self.top = self.top.min(display.len().saturating_sub(view_h));
            if display.is_empty() {
                display.push(plain(cells, "  (nothing yet)", dim()));
            }
            for (r, owner) in owners.iter().skip(self.top).take(view_h).enumerate() {
                if let Some(n) = owner {
                    ui.mark((0, lines.len() + r, cells, 1), Region::Row(*n));
                }
            }
            lines.extend(window(&display, self.top, view_h, cells));
        }
        lines.truncate(height.saturating_sub(1));
        while lines.len() < height.saturating_sub(1) {
            lines.push(blank(cells));
        }
        let hints: &[(&str, &str)] = if self.full {
            &[("h", "back"), ("j/k", "scroll"), ("n/p", "command"), ("e", "failure"), ("= # u", "format"), ("?", "keys")]
        } else {
            &[("j/k", "move"), ("⏎", "expand"), ("l", "open"), ("h", "collapse"), ("n/p", "command"), ("e", "failure"), ("f t s c", "filter"), ("v", "view"), ("?", "keys")]
        };
        lines.push(ui.footer(cells, hints));
        lines
    }

    fn handle(&mut self, ui: &mut Ui, s: &Session, key: &str) -> bool {
        let items = self.items(ui, s);
        let i = self.selected(ui, &items);
        let is_call = |x: &Item| matches!(x, Item::Call(_));
        if self.full {
            match key {
                "j" | DOWN => self.scroll += 1,
                "k" | UP => self.scroll = self.scroll.saturating_sub(1),
                " " | PGDN => self.scroll += 10,
                PGUP => self.scroll = self.scroll.saturating_sub(10),
                "n" => self.jump(ui, &items, i, 1, is_call),
                "p" | "N" => self.jump(ui, &items, i, -1, is_call),
                "e" if !items.is_empty() => self.move_to(ui, &items, next_failure(&items, i, 1) as isize),
                ESC | "q" | "o" | BS | "h" | LEFT => self.full = false,
                _ => return false,
            }
            return true;
        }
        match key {
            "j" | DOWN => self.move_to(ui, &items, i as isize + 1),
            "k" | UP => self.move_to(ui, &items, i as isize - 1),
            "g" => self.move_to(ui, &items, 0),
            "G" => self.move_to(ui, &items, items.len() as isize - 1),
            k if (is_enter(k) || k == " ") && !items.is_empty() => {
                let key = items[i].key();
                self.open = if self.open.as_deref() == Some(key.as_str()) { None } else { Some(key) };
            }
            "n" => self.jump(ui, &items, i, 1, is_call),
            "p" | "N" => self.jump(ui, &items, i, -1, is_call),
            "e" if !items.is_empty() => {
                let j = next_failure(&items, i, 1);
                if items[j].failing() {
                    self.move_to(ui, &items, j as isize);
                }
            }
            k if (k == "o" || k == "l" || k == RIGHT) && !items.is_empty() => {
                self.full = true;
                self.scroll = 0;
            }
            // with nothing expanded these go up, to the list of sessions
            "h" | LEFT | ESC if self.open.is_some() => self.open = None,
            k if SHOW_KEYS.contains(&k) => {
                let n = SHOW_KEYS.iter().position(|x| *x == k).unwrap_or(0);
                self.show[n] = !self.show[n];
            }
            _ => return false,
        }
        true
    }

    fn adopt(&mut self, _ui: &mut Ui, _s: &Session, key: &str) {
        self.key = Some(key.to_string());
        self.following = false;
    }

    fn selection<'a>(&mut self, ui: &mut Ui, s: &'a Session) -> Option<Item<'a>> {
        let items = self.items(ui, s);
        let i = self.selected(ui, &items);
        items.get(i).copied()
    }

    fn mouse(&mut self, ui: &mut Ui, s: &Session, m: Mouse) -> bool {
        let items = self.items(ui, s);
        let i = self.selected(ui, &items);
        let clicked = self.clicked.take();
        let region = hit(&ui.hits, m.x, m.y);
        match (region, m.press) {
            (_, Press::Wheel(d)) if self.full => self.scroll = scrolled(self.scroll, d),
            (_, Press::Wheel(d)) => self.move_to(ui, &items, i as isize + d),
            // a toggle takes single clicks: a quick second one is not a double-click here
            (Some(Region::Tab(n)), Press::Click) if n < SHOW_KEYS.len() => self.show[n] = !self.show[n],
            (Some(Region::Tab(_)), Press::Click) => ui.failed_only = !ui.failed_only,
            (Some(Region::Row(n)), Press::Click) => {
                let Some(item) = items.get(n) else { return false };
                let key = item.key();
                self.clicked = Some((key.clone(), self.open.as_deref() == Some(key.as_str())));
                // an expansion elsewhere follows the cursor here
                self.move_to(ui, &items, n as isize);
            }
            (_, Press::Double) => {
                // A double-click expands in place, as enter does, the item the first click
                // picked: the rows may have moved under the second, and the first click has
                // already brought an expansion along. It collapses only what was open before.
                let (key, was_open) = match (clicked, region) {
                    (Some(c), _) => c,
                    (None, Some(Region::Row(n))) => match items.get(n) {
                        Some(item) => (item.key(), self.open.as_deref() == Some(item.key().as_str())),
                        None => return false,
                    },
                    _ => return false,
                };
                let Some(n) = items.iter().position(|x| x.key() == key) else { return false };
                self.move_to(ui, &items, n as isize);
                self.open = if was_open { None } else { Some(key) };
            }
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g_and_big_g_go_to_the_ends_of_the_focused_column() {
        let mut ui = Ui::new(String::new());
        let s = Session::new(std::path::PathBuf::from("/nonexistent"), None);
        let mut b = ColumnsView::new();
        b.focus = 0;
        assert!(b.handle(&mut ui, &s, "G"));
        assert_eq!(b.source, SOURCES.len() - 1);
        assert!(b.handle(&mut ui, &s, "g"));
        assert_eq!(b.source, 0);
        b.focus = 2;
        b.handle(&mut ui, &s, "G");
        assert!(b.scroll > 1000);
        b.handle(&mut ui, &s, "g");
        assert_eq!(b.scroll, 0);
    }

    fn with_files(ages: &[f64]) -> Session {
        let mut s = Session::new(std::path::PathBuf::from("/nonexistent"), None);
        let t = now();
        for (n, age) in ages.iter().enumerate() {
            let rel = format!("f{n}.txt");
            s.files.push(crate::data::FileItem { path: std::path::PathBuf::from("/nonexistent").join(&rel), rel, size: 1, mtime: t - age, is_image: false });
        }
        s
    }

    fn click(x: usize, y: usize) -> Mouse {
        Mouse { press: Press::Click, x, y }
    }

    fn selected_rel(v: &mut dyn View, ui: &mut Ui, s: &Session) -> String {
        match v.selection(ui, s) {
            Some(Item::File(f)) => f.rel.clone(),
            _ => String::new(),
        }
    }

    #[test]
    fn a_click_in_tabs_picks_the_tab_and_the_row_under_it() {
        let mut ui = Ui::new(String::new());
        let s = with_files(&[100.0, 200.0, 300.0, 400.0, 500.0]);
        let mut a = TabsView::new();
        a.render(&mut ui, &s, 80, 24);
        // " 1 Files 5 " starts the tab line
        assert!(a.mouse(&mut ui, &s, click(3, 1)));
        assert_eq!(a.tab, 0);
        ui.hits.clear();
        a.render(&mut ui, &s, 80, 24);
        // header, tabs, rule, then the list
        a.mouse(&mut ui, &s, click(10, 5));
        assert_eq!(selected_rel(&mut a, &mut ui, &s), "f2.txt");
        a.mouse(&mut ui, &s, Mouse { press: Press::Wheel(1), x: 10, y: 5 });
        assert_eq!(selected_rel(&mut a, &mut ui, &s), "f3.txt");
        assert!(!a.full);
        a.mouse(&mut ui, &s, Mouse { press: Press::Double, x: 10, y: 3 });
        assert_eq!((selected_rel(&mut a, &mut ui, &s).as_str(), a.full), ("f0.txt", true));
        // below the last item there is no row to pick
        assert!(!a.mouse(&mut ui, &s, click(10, 9)) || selected_rel(&mut a, &mut ui, &s) == "f0.txt");
    }

    #[test]
    fn the_preview_starts_at_the_top_for_another_item() {
        let mut ui = Ui::new(String::new());
        let mut s = with_files(&[100.0, 200.0]);
        let long = std::env::temp_dir().join(format!("claude-deck-a-{}.txt", std::process::id()));
        std::fs::write(&long, (0..200).map(|n| format!("line {n}\n")).collect::<String>()).unwrap();
        s.files[0].path = long.clone();
        let mut a = TabsView::new();
        a.tab = 0;
        a.render(&mut ui, &s, 80, 24);
        let preview_y = (0..24).find(|&y| hit(&ui.hits, 5, y) == Some(Region::Preview)).unwrap();
        a.mouse(&mut ui, &s, Mouse { press: Press::Wheel(2), x: 5, y: preview_y });
        a.render(&mut ui, &s, 80, 24);
        assert_eq!(a.scroll, 6);
        // back from another tab, by key
        a.handle(&mut ui, &s, "2");
        a.render(&mut ui, &s, 80, 24);
        a.handle(&mut ui, &s, "1");
        a.render(&mut ui, &s, 80, 24);
        assert_eq!(a.scroll, 0);
        // a new file at the top, which the cursor follows
        a.scroll = 6;
        a.render(&mut ui, &s, 80, 24);
        assert_eq!(a.scroll, 6);
        let mut fresh = s.files[1].clone();
        fresh.rel = "new.txt".into();
        fresh.mtime = now();
        s.files.insert(0, fresh);
        a.render(&mut ui, &s, 80, 24);
        assert_eq!((selected_rel(&mut a, &mut ui, &s).as_str(), a.scroll), ("new.txt", 0));
        let _ = std::fs::remove_file(&long);
    }

    #[test]
    fn a_click_in_columns_focuses_the_column_and_picks_the_source() {
        let mut ui = Ui::new(String::new());
        let s = with_files(&[100.0, 200.0]);
        let mut b = ColumnsView::new();
        b.render(&mut ui, &s, 120, 30);
        // header, top border, then the sources one per row, inside the left border
        b.mouse(&mut ui, &s, click(5, 2));
        assert_eq!((b.source, b.focus), (0, 0));
        b.mouse(&mut ui, &s, click(110, 10));
        assert_eq!(b.focus, 2);
        ui.hits.clear();
        b.render(&mut ui, &s, 120, 30);
        b.mouse(&mut ui, &s, Mouse { press: Press::Double, x: 30, y: 3 });
        assert_eq!((selected_rel(&mut b, &mut ui, &s).as_str(), b.focus), ("f1.txt", 2));
        // the wheel over the sources changes the source without moving the focus
        b.mouse(&mut ui, &s, Mouse { press: Press::Wheel(1), x: 5, y: 8 });
        assert_eq!((b.source, b.focus), (1, 2));
        // stacked: the source line, the list, the detail
        ui.hits.clear();
        b.render(&mut ui, &s, 80, 30);
        b.mouse(&mut ui, &s, click(10, 1));
        assert_eq!(b.focus, 0);
        // the dots at the end of that line pick a source
        b.mouse(&mut ui, &s, click(80 - SOURCES.len() + 2, 1));
        assert_eq!((b.source, b.focus), (2, 0));
        // and the dot drawn there is the one picked
        let line: String = b.render(&mut ui, &s, 80, 30)[1].spans.iter().map(|s| s.content.to_string()).collect();
        assert!(line.ends_with("··●········"), "{line}");
    }

    #[test]
    fn a_click_in_the_timeline_skips_gaps_and_expansions() {
        let mut ui = Ui::new(String::new());
        // the third file comes after a gap of more than ten minutes, under a rule
        let s = with_files(&[1.0, 2.0, 1000.0]);
        let mut c = TimelineView::new();
        c.render(&mut ui, &s, 80, 24);
        c.mouse(&mut ui, &s, click(20, 6));
        assert_eq!(selected_rel(&mut c, &mut ui, &s), "f2.txt");
        assert!(!c.mouse(&mut ui, &s, click(20, 5)) || selected_rel(&mut c, &mut ui, &s) == "f2.txt");
        c.mouse(&mut ui, &s, Mouse { press: Press::Double, x: 20, y: 3 });
        assert_eq!(c.open.as_deref(), Some("file:f0.txt"));
        // f0's expansion now sits between f0 and f1
        ui.hits.clear();
        c.render(&mut ui, &s, 80, 24);
        c.mouse(&mut ui, &s, click(20, 4));
        assert_eq!(selected_rel(&mut c, &mut ui, &s), "f0.txt");
        c.mouse(&mut ui, &s, click(20, 5));
        assert_eq!(selected_rel(&mut c, &mut ui, &s), "f1.txt");
        // the chips toggle what is shown
        c.mouse(&mut ui, &s, click(2, 1));
        assert!(!c.show[0]);
        // but not full screen, where f t s c x do nothing either
        c.show[0] = true;
        c.full = true;
        ui.hits.clear();
        c.render(&mut ui, &s, 80, 24);
        assert!(!c.mouse(&mut ui, &s, click(10, 1)));
        assert!(c.show[1]);
    }

    #[test]
    fn a_double_click_in_the_timeline_expands_what_the_first_click_picked() {
        let mut ui = Ui::new(String::new());
        let mut s = with_files(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        // f0 expands to eight lines, f2 (missing) to one
        let long = std::env::temp_dir().join(format!("claude-deck-c-{}.txt", std::process::id()));
        std::fs::write(&long, "1\n2\n3\n4\n5\n6\n7\n8\n").unwrap();
        s.files[0].path = long.clone();
        let mut c = TimelineView::new();
        c.open = Some("file:f0.txt".into());
        c.following = false;
        c.render(&mut ui, &s, 80, 30);
        let row_of = |ui: &Ui, n: usize| (0..30).find(|&y| hit(&ui.hits, 20, y) == Some(Region::Row(n))).unwrap();
        let y2 = row_of(&ui, 2);
        // the first click moves the expansion to f2, which moves f2 up past f0's long
        // expansion; the second lands on the old place, now on another item
        c.mouse(&mut ui, &s, click(20, y2));
        assert_eq!(c.open.as_deref(), Some("file:f2.txt"));
        ui.hits.clear();
        c.render(&mut ui, &s, 80, 30);
        assert!(!matches!(hit(&ui.hits, 20, y2), Some(Region::Row(2))));
        c.mouse(&mut ui, &s, Mouse { press: Press::Double, x: 20, y: y2 });
        let _ = std::fs::remove_file(&long);
        assert_eq!((selected_rel(&mut c, &mut ui, &s).as_str(), c.open.as_deref()), ("f2.txt", Some("file:f2.txt")));
        // a double-click on the expanded item collapses it
        let y = row_of(&ui, 2);
        c.mouse(&mut ui, &s, click(20, y));
        c.mouse(&mut ui, &s, Mouse { press: Press::Double, x: 20, y });
        assert_eq!(c.open, None);
    }

    #[test]
    fn the_focused_column_is_framed_in_heavy_blue_lines() {
        let titles: [String; 3] = ["a".into(), "b".into(), "c".into()];
        let rows = framed(&[0, 1, 2], &[6, 6, 6], &titles, &Default::default(), 1, 1);
        // the four vertical borders of the middle row: left edge, two shared, right edge
        let heavy: Vec<bool> = rows[1].spans.iter().filter(|s| s.content == "│" || s.content == "┃").map(|s| s.content == "┃" && s.style == focus_line()).collect();
        assert_eq!(heavy, vec![false, true, true, false]);
        let top: String = rows[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(top.contains("▶ b") && top.starts_with("┌─ a"));
        assert_eq!(top.chars().count(), 1 + 6 + 1 + 6 + 1 + 6 + 1);
    }
}
