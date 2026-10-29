//! Text as a terminal pane draws it: escape-free, tab-expanded, measured in cells.

use unicode_width::UnicodeWidthChar;

/// Terminal cells a character takes: 2 for wide (CJK) characters, 0 for combining marks.
pub fn char_width(c: char) -> usize {
    if c == '\t' {
        return 1;
    }
    c.width().unwrap_or(0)
}

pub fn width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// Output as the pane can draw it: no escape sequences, tabs expanded (unless
/// `keep_tabs`), and a carriage return keeping only what was drawn last on its
/// line, the way a progress bar ends up.
pub fn sanitize(text: &str, keep_tabs: bool) -> String {
    let stripped = strip_escapes(text);
    let mut out = String::with_capacity(stripped.len());
    for (n, line) in stripped.split('\n').enumerate() {
        if n > 0 {
            out.push('\n');
        }
        let line = match line.trim_end_matches('\r').rsplit_once('\r') {
            Some((_, last)) if line.contains('\r') => last,
            _ => line.trim_end_matches('\r'),
        };
        let mut column = 0;
        for c in line.chars() {
            match c {
                '\t' if keep_tabs => {
                    out.push('\t');
                    column += 1;
                }
                '\t' => {
                    let pad = 4 - column % 4;
                    out.extend(std::iter::repeat_n(' ', pad));
                    column += pad;
                }
                c if (c as u32) < 0x20 || c == '\u{7f}' => {}
                c => {
                    out.push(c);
                    column += char_width(c);
                }
            }
        }
    }
    out
}

/// Remove CSI, OSC and two-byte escape sequences.
fn strip_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                // parameters and intermediates, then one final byte in @..~
                for d in chars.by_ref() {
                    if ('@'..='~').contains(&d) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                while let Some(d) = chars.next() {
                    if d == '\x07' {
                        break;
                    }
                    if d == '\x1b' {
                        chars.next();
                        break;
                    }
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    out
}

/// `text` cut to `cells` terminal cells with an ellipsis, optionally padded.
pub fn fit(text: &str, cells: usize, pad: bool, right: bool) -> String {
    if cells == 0 {
        return String::new();
    }
    let mut out: String;
    if width(text) > cells {
        out = String::new();
        let mut used = 0;
        for c in text.chars() {
            let w = char_width(c);
            if used + w > cells - 1 {
                break;
            }
            out.push(c);
            used += w;
        }
        out.push('…');
    } else {
        out = text.to_string();
    }
    if pad {
        let gap = cells.saturating_sub(width(&out));
        if right {
            out = " ".repeat(gap) + &out;
        } else {
            out.push_str(&" ".repeat(gap));
        }
    }
    out
}

/// Hard-wrap each line of `text` at `cells` terminal cells, never splitting a wide character.
pub fn wrap(text: &str, cells: usize) -> Vec<String> {
    let mut out = Vec::new();
    if cells == 0 {
        return out;
    }
    for line in text.split('\n') {
        if line.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut used = 0;
        for c in line.chars() {
            let w = char_width(c);
            if used + w > cells {
                out.push(std::mem::take(&mut current));
                used = 0;
            }
            current.push(c);
            used += w;
        }
        out.push(current);
    }
    out
}

/// Wrap `text` at `cells` terminal cells between words; a word wider than that gets a row of its own.
pub fn wrap_words(text: &str, cells: usize) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    for word in text.split(' ') {
        match rows.last_mut() {
            Some(row) if width(row) + 1 + width(word) <= cells => {
                row.push(' ');
                row.push_str(word);
            }
            _ => rows.push(word.to_string()),
        }
    }
    rows
}

/// Several lines as one: each run of newlines (and the space around it) becomes ` ⏎ `.
pub fn one_line(text: &str) -> String {
    let parts: Vec<&str> = text.trim().split('\n').map(str::trim).filter(|p| !p.is_empty()).collect();
    parts.join(" ⏎ ")
}

pub fn human_size(size: u64) -> String {
    let mut value = size as f64;
    for unit in ["B", "K", "M", "G"] {
        if value < 1024.0 || unit == "G" {
            return if unit == "B" { format!("{value:.0}B") } else { format!("{value:.1}{unit}") };
        }
        value /= 1024.0;
    }
    unreachable!()
}

pub fn human_age(seconds: f64) -> String {
    let s = seconds.max(0.0);
    if s < 60.0 {
        format!("{s:.0}s")
    } else if s < 3600.0 {
        format!("{}m", (s / 60.0) as u64)
    } else if s < 86400.0 {
        format!("{}h", (s / 3600.0) as u64)
    } else {
        format!("{}d", (s / 86400.0) as u64)
    }
}

pub fn human_duration(ms: Option<f64>) -> String {
    match ms {
        None => String::new(),
        Some(ms) if ms < 1000.0 => format!("{ms:.0}ms"),
        Some(ms) if ms < 60000.0 => format!("{:.1}s", ms / 1000.0),
        Some(ms) => format!("{}m{:02.0}s", (ms / 60000.0) as u64, (ms % 60000.0) / 1000.0),
    }
}

/// Epoch seconds of an RFC 3339 UTC stamp such as `2026-10-22T06:00:01.250Z`; 0 when unparsable.
pub fn parse_ts(stamp: &str) -> f64 {
    let b = stamp.as_bytes();
    if b.len() < 19 {
        return 0.0;
    }
    let num = |from: usize, to: usize| stamp.get(from..to).and_then(|s| s.parse::<i64>().ok());
    let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(s)) =
        (num(0, 4), num(5, 7), num(8, 10), num(11, 13), num(14, 16), num(17, 19))
    else {
        return 0.0;
    };
    // days from civil (Howard Hinnant)
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let mut secs = (days * 86400 + h * 3600 + mi * 60 + s) as f64;
    if b.get(19) == Some(&b'.') {
        let frac: String = stamp[20..].chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(f) = format!("0.{frac}").parse::<f64>() {
            secs += f;
        }
    }
    secs
}

/// Local wall-clock `HH:MM:SS` for epoch seconds.
pub fn hms(epoch: f64) -> String {
    if epoch <= 0.0 {
        return "--:--:--".into();
    }
    let tm = local_tm(epoch);
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

pub fn hm(epoch: f64) -> String {
    hms(epoch)[..5].to_string()
}

pub fn ymd(epoch: f64) -> String {
    if epoch <= 0.0 {
        return String::new();
    }
    let tm = local_tm(epoch);
    format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)
}

// libc marks time_t deprecated on musl while it moves to 64 bits; the cast is the same either way.
#[allow(deprecated)]
fn local_tm(epoch: f64) -> libc::tm {
    let t = epoch as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    tm
}

pub fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_escapes_and_keeps_last_carriage_return_segment() {
        assert_eq!(sanitize("\x1b[31mred\x1b[0m\tx\n10%\r50%\r100%", false), "red x\n100%");
        assert_eq!(sanitize("a\tb\x07", true), "a\tb");
    }

    #[test]
    fn fit_counts_wide_characters_as_two_cells() {
        assert_eq!(width("本機ab"), 6);
        assert_eq!(fit("本機匯入管理", 7, false, false), "本機匯…");
        assert_eq!(width(&fit("本機", 6, true, false)), 6);
    }

    #[test]
    fn one_line_joins_runs_of_newlines_once() {
        assert_eq!(one_line("  a \n\n  \n b\nc  "), "a ⏎ b ⏎ c");
    }

    #[test]
    fn wrap_never_splits_a_wide_character() {
        assert_eq!(wrap("本機匯入", 5), vec!["本機", "匯入"]);
    }

    #[test]
    fn parse_ts_reads_utc_with_fraction() {
        assert_eq!(parse_ts("1970-01-01T00:00:01.250Z"), 1.25);
        assert_eq!(parse_ts("2026-10-22T06:00:00.000Z") - parse_ts("2026-10-22T05:59:59.000Z"), 1.0);
    }
}
