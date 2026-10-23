//! Previews: syntax highlighting, formatting, colouring tool output by its shape,
//! folding long output, and wrapping styled text into screen lines with a gutter.
//!
//! Highlighting uses bat's syntaxes and themes (two-face over syntect), loaded once.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use regex::Regex;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme};
use syntect::parsing::SyntaxSet;

use crate::text::char_width;

pub type Seg = (Style, String);
/// One source line as styled segments.
pub type Logical = Vec<Seg>;

pub const HIGHLIGHT_LIMIT: usize = 256 * 1024;
pub const FOLD_OVER: usize = 60;
pub const FOLD_KEEP: usize = 20;

pub fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}
pub fn fg(color: Color) -> Style {
    Style::new().fg(color)
}

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);
static THEME: LazyLock<Theme> = LazyLock::new(|| {
    let themes = two_face::theme::extra();
    themes.get(two_face::theme::EmbeddedThemeName::MonokaiExtended).clone()
});
static CACHE: LazyLock<Mutex<HashMap<(String, u64), Option<Vec<Logical>>>>> = LazyLock::new(Default::default);

/// Load the syntax definitions now, so the first preview does not pay for it.
pub fn warm_up() {
    LazyLock::force(&SYNTAXES);
    LazyLock::force(&THEME);
}

fn hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// `text` coloured for the language its file `name` implies, or None.
pub fn highlight(text: &str, name: &str) -> Option<Vec<Logical>> {
    if text.is_empty() || text.len() > HIGHLIGHT_LIMIT {
        return None;
    }
    let key = (name.to_string(), hash(text));
    if let Some(hit) = CACHE.lock().ok()?.get(&key) {
        return hit.clone();
    }
    let result = highlight_uncached(text, name);
    if let Ok(mut cache) = CACHE.lock() {
        if cache.len() > 200 {
            cache.clear();
        }
        cache.insert(key, result.clone());
    }
    result
}

fn highlight_uncached(text: &str, name: &str) -> Option<Vec<Logical>> {
    let ss = &*SYNTAXES;
    let base = name.rsplit('/').next().unwrap_or(name);
    let ext = base.rsplit_once('.').map(|(_, e)| e).unwrap_or(base);
    let syntax = ss.find_syntax_by_extension(ext).or_else(|| ss.find_syntax_by_token(base))?;
    if syntax.name == "Plain Text" {
        return None;
    }
    let mut lines = HighlightLines::new(syntax, &THEME);
    let mut out = Vec::new();
    for line in syntect::util::LinesWithEndings::from(text) {
        let ranges = lines.highlight_line(line, ss).ok()?;
        let mut logical: Logical = Vec::new();
        for (style, piece) in ranges {
            let piece = piece.trim_end_matches(['\n', '\r']);
            if piece.is_empty() {
                continue;
            }
            let c = style.foreground;
            let mut s = Style::new().fg(Color::Rgb(c.r, c.g, c.b));
            for (font, modifier) in [(FontStyle::BOLD, Modifier::BOLD), (FontStyle::ITALIC, Modifier::ITALIC), (FontStyle::UNDERLINE, Modifier::UNDERLINED)] {
                if style.font_style.contains(font) {
                    s = s.add_modifier(modifier);
                }
            }
            logical.push((s, piece.to_string()));
        }
        out.push(logical);
    }
    if text.ends_with('\n') && out.len() > 1 {
        // LinesWithEndings yields no empty line after the last newline; keep line count as split() does
    }
    Some(out)
}

pub fn plain(text: &str, style: Style) -> Vec<Logical> {
    text.split('\n').map(|l| if l.is_empty() { Vec::new() } else { vec![(style, l.to_string())] }).collect()
}

// ------------------------------------------------------------------ formatting

/// (formatted text, name for highlighting), or None when nothing applies.
pub fn format_text(text: &str, name: &str) -> Option<(String, String)> {
    let lower = name.to_lowercase();
    let stripped = text.trim();
    if stripped.is_empty() {
        return None;
    }
    let first = stripped.chars().next()?;
    let pretty = |s: &str| serde_json::from_str::<serde_json::Value>(s).ok().and_then(|v| serde_json::to_string_pretty(&v).ok());
    if lower.ends_with(".jsonl") || lower.ends_with(".ndjson") || ("{[".contains(first) && stripped.contains('\n') && all_json_lines(stripped)) {
        let parts: Option<Vec<String>> = stripped.lines().filter(|l| !l.trim().is_empty()).map(pretty).collect();
        if let Some(parts) = parts {
            return Some((parts.join("\n"), "x.json".into()));
        }
    }
    if lower.ends_with(".json") || "{[".contains(first) {
        if let Some(p) = pretty(stripped) {
            return Some((p, "x.json".into()));
        }
    }
    if lower.ends_with(".xml") || lower.ends_with(".svg") || lower.ends_with(".plist") || stripped.starts_with("<?xml") {
        return pretty_xml(stripped).map(|x| (x, "x.xml".into()));
    }
    None
}

fn all_json_lines(text: &str) -> bool {
    let lines: Vec<&str> = text.lines().take(50).filter(|l| !l.trim().is_empty()).collect();
    lines.len() >= 2 && lines.iter().all(|l| serde_json::from_str::<serde_json::Value>(l).is_ok())
}

static XML_TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<!--.*?-->|<[^>]+>|[^<]+").unwrap());

/// One tag or text run per line, indented by nesting.
fn pretty_xml(text: &str) -> Option<String> {
    let mut out = Vec::new();
    let mut depth: usize = 0;
    for m in XML_TOKEN.find_iter(text) {
        let token = m.as_str().trim();
        if token.is_empty() {
            continue;
        }
        if token.starts_with("</") {
            depth = depth.saturating_sub(1);
            out.push(format!("{}{token}", "  ".repeat(depth)));
        } else if token.starts_with('<') {
            out.push(format!("{}{token}", "  ".repeat(depth)));
            let opens = !(token.starts_with("<?") || token.starts_with("<!") || token.ends_with("/>"));
            if opens {
                depth += 1;
            }
        } else {
            out.push(format!("{}{}", "  ".repeat(depth), token.split_whitespace().collect::<Vec<_>>().join(" ")));
        }
    }
    (out.len() > 1).then(|| out.join("\n"))
}

// ------------------------------------------------------------------ tool output

static ERROR_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(error|errors|failed|failure|fatal|traceback|exception|denied|panic)\b|no such file|not found|exit code [1-9]|^E\s").unwrap()
});
static READ_NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(\d+)(?:→|\t)").unwrap());
static GREP_HIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([^\s:][^:]*):(\d+)([:-])").unwrap());
static TREE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([\s│├└─┬┼]+)").unwrap());

/// The Read tool's `  12→line` numbering removed: (text, first line number).
pub fn strip_read_numbers(text: &str) -> Option<(String, usize)> {
    let lines: Vec<&str> = text.split('\n').collect();
    let sample: Vec<&str> = lines.iter().take(20).copied().filter(|l| !l.trim().is_empty()).collect();
    if sample.is_empty() {
        return None;
    }
    let mut numbers = Vec::new();
    for line in &sample {
        numbers.push(READ_NUMBER.captures(line)?[1].parse::<usize>().ok()?);
    }
    if numbers.iter().enumerate().any(|(i, n)| *n != numbers[0] + i) {
        return None; // numbers that do not count up are content, not a gutter
    }
    let stripped: Vec<String> = lines.iter().map(|l| expand_tabs(&READ_NUMBER.replace(l, ""))).collect();
    Some((stripped.join("\n"), numbers[0]))
}

pub fn expand_tabs(line: &str) -> String {
    if !line.contains('\t') {
        return line.to_string();
    }
    let mut out = String::new();
    let mut column = 0;
    for c in line.chars() {
        if c == '\t' {
            let pad = 4 - column % 4;
            out.push_str(&" ".repeat(pad));
            column += pad;
        } else {
            out.push(c);
            column += char_width(c);
        }
    }
    out
}

/// Colour by what the output looks like: a diff, grep hits, a tree, error lines.
pub fn colour_shape(text: &str) -> Vec<Logical> {
    let lines: Vec<&str> = text.split('\n').collect();
    let is_diff = lines.iter().take(200).any(|l| l.starts_with("@@ ") || l.starts_with("diff --git") || l.starts_with("+++ ") || l.starts_with("--- a/"));
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let one = |style: Style| vec![(style, line.to_string())];
        if line.is_empty() {
            out.push(Vec::new());
        } else if is_diff && ["+++", "---", "diff --git", "index "].iter().any(|p| line.starts_with(p)) {
            out.push(one(Style::new().add_modifier(Modifier::BOLD)));
        } else if is_diff && line.starts_with("@@") {
            out.push(one(fg(Color::Cyan)));
        } else if is_diff && line.starts_with('+') {
            out.push(one(fg(Color::Green)));
        } else if is_diff && line.starts_with('-') {
            out.push(one(fg(Color::Red)));
        } else if ERROR_LINE.is_match(line) {
            out.push(one(fg(Color::Red)));
        } else if let Some(hit) = GREP_HIT.captures(line).filter(|h| h[1].len() < 200) {
            let end = hit.get(0).unwrap().end();
            out.push(vec![
                (fg(Color::Magenta), hit[1].to_string()),
                (dim(), ":".into()),
                (fg(Color::Green), hit[2].to_string()),
                (dim(), hit[3].to_string()),
                (Style::new(), line[end..].to_string()),
            ]);
        } else if let Some(tree) = TREE.captures(line).filter(|t| t[1].contains(['├', '└', '│'])) {
            let end = tree.get(0).unwrap().end();
            out.push(vec![(dim(), tree[1].to_string()), (Style::new(), line[end..].to_string())]);
        } else {
            out.push(one(Style::new()));
        }
    }
    out
}

/// Numbered lines, the middle of a long output collapsed to one marker line.
pub fn fold(lines: Vec<Logical>, start: usize, folded: bool) -> Vec<(Option<usize>, Logical)> {
    let total = lines.len();
    let items: Vec<(Option<usize>, Logical)> = lines.into_iter().enumerate().map(|(i, l)| (Some(start + i), l)).collect();
    if !folded || total <= FOLD_OVER {
        return items;
    }
    let hidden = total - 2 * FOLD_KEEP;
    let mut out: Vec<(Option<usize>, Logical)> = Vec::with_capacity(2 * FOLD_KEEP + 1);
    let mut iter = items.into_iter();
    out.extend(iter.by_ref().take(FOLD_KEEP));
    out.push((None, vec![(dim(), format!("··· {hidden} lines folded · u to unfold ···"))]));
    out.extend(iter.skip(hidden));
    out
}

/// Screen lines exactly `cells` wide; `numbers` adds a gutter, `base` styles
/// segments that carry no colour of their own and the padding.
pub fn wrap_items(items: &[(Option<usize>, Logical)], cells: usize, numbers: bool, base: Style) -> Vec<Line<'static>> {
    let digits = if numbers { items.iter().filter_map(|(n, _)| *n).max().map(|n| n.to_string().len()).unwrap_or(1) } else { 0 };
    let gutter = if numbers { digits + 3 } else { 0 };
    let avail = cells.saturating_sub(gutter).max(1);
    let mut out = Vec::new();
    for (number, logical) in items {
        let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
        let mut current: Vec<Span<'static>> = Vec::new();
        let mut used = 0;
        for (style, piece) in logical {
            let style = base.patch(*style);
            let mut chunk = String::new();
            for c in piece.chars() {
                let w = char_width(c);
                if used + w > avail {
                    if !chunk.is_empty() {
                        current.push(Span::styled(std::mem::take(&mut chunk), style));
                    }
                    current.push(Span::styled(" ".repeat(avail.saturating_sub(used)), base));
                    rows.push(std::mem::take(&mut current));
                    used = 0;
                }
                chunk.push(c);
                used += w;
            }
            if !chunk.is_empty() {
                current.push(Span::styled(chunk, style));
            }
        }
        current.push(Span::styled(" ".repeat(avail.saturating_sub(used)), base));
        rows.push(current);
        for (i, mut row) in rows.into_iter().enumerate() {
            if numbers {
                let label = if i == 0 { number.map(|n| n.to_string()).unwrap_or_default() } else { String::new() };
                row.insert(0, Span::styled(format!("{label:>digits$} │ "), dim()));
            }
            out.push(Line::from(row));
        }
    }
    out
}

/// Cells a rendered line takes.
#[cfg(test)]
pub fn line_width(line: &Line) -> usize {
    line.spans.iter().map(|s| crate::text::width(&s.content)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visible(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn wrap_keeps_style_across_wraps_and_pads_by_display_width() {
        let items = vec![(Some(1), vec![(fg(Color::Red), "本機匯入管理".to_string())]), (Some(2), vec![(Style::new(), "ok".to_string())])];
        let rows = wrap_items(&items, 9, true, Style::new());
        assert_eq!(rows.iter().map(line_width).collect::<Vec<_>>(), vec![9, 9, 9, 9]);
        assert!(visible(&rows[0]).starts_with("1 │ 本機") && visible(&rows[1]).starts_with("  │ 匯入") && visible(&rows[3]).starts_with("2 │ ok"));
        assert_eq!(rows[1].spans[1].style.fg, Some(Color::Red));
    }

    #[test]
    fn a_wide_character_in_a_one_cell_column_does_not_underflow() {
        let rows = wrap_items(&[(Some(1), vec![(Style::new(), "本機".to_string())])], 4, true, Style::new());
        assert!(!rows.is_empty());
    }

    #[test]
    fn format_text_pretty_prints_json_and_jsonl_in_key_order() {
        assert_eq!(format_text("{\"b\":1,\"a\":2}", "x.json").unwrap().0, "{\n  \"b\": 1,\n  \"a\": 2\n}");
        assert_eq!(format_text("{\"a\":1}\n{\"b\":2}", "log.jsonl").unwrap().0.matches('{').count(), 2);
        assert!(format_text("plain words", "notes.txt").is_none());
        assert_eq!(format_text("<a><b>x</b></a>", "x.xml").unwrap().0, "<a>\n  <b>\n    x\n  </b>\n</a>");
    }

    #[test]
    fn read_numbers_are_replaced_by_the_gutter_starting_at_the_same_line() {
        assert_eq!(strip_read_numbers("    41\tdef a():\n    42\t\tpass").unwrap(), ("def a():\n    pass".to_string(), 41));
        assert!(strip_read_numbers("plain output").is_none());
    }

    #[test]
    fn colour_shape_marks_diffs_grep_hits_and_errors() {
        let diff = colour_shape("@@ -1 +1 @@\n-old\n+new");
        assert_eq!((diff[1][0].0.fg, diff[2][0].0.fg), (Some(Color::Red), Some(Color::Green)));
        assert_eq!(colour_shape("bin/data.py:12:def x")[0][0], (fg(Color::Magenta), "bin/data.py".to_string()));
        assert_eq!(colour_shape("ls: x: No such file or directory")[0][0].0.fg, Some(Color::Red));
        assert_eq!(colour_shape("all good")[0][0].0, Style::new());
    }

    #[test]
    fn fold_keeps_head_and_tail_and_true_line_numbers() {
        let lines: Vec<Logical> = (1..=100).map(|i| vec![(Style::new(), i.to_string())]).collect();
        let items = fold(lines.clone(), 1, true);
        assert_eq!(items.len(), 41);
        assert_eq!(items[20].0, None);
        assert!(items[20].1[0].1.contains("60 lines folded"));
        assert_eq!(items[40].0, Some(100));
        assert_eq!(fold(lines, 1, false).len(), 100);
    }

    #[test]
    fn highlight_colours_known_languages_only() {
        assert!(highlight("fn main() {}\n", "main.rs").is_some());
        assert!(highlight("[a]\nb = 1\n", "Cargo.toml").is_some());
        assert!(highlight("just words", "notes.txt").is_none());
    }
}
