//! One image shown through the Kitty graphics protocol, written to our own output.
//!
//! herdr parses the protocol from pane output and composites the image in a
//! capable outer terminal (Ghostty, kitty, WezTerm); Ghostty run directly draws it
//! itself. ratatui knows nothing of the image: after a frame the placement is written
//! when it changed. Each file version is transmitted once, read off the UI thread and
//! only after the cursor rested on it for a moment, and the last few stay in the
//! terminal, so going back to an image only places it again.
//!
//! The letterbox arithmetic is adapted from herdr-scratchdock's bin/viewer.py
//! (https://github.com/mvaios/herdr-scratchdock, MIT, Copyright (c) 2026 Vahram Mirzoyan).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

use base64::Engine;

const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024; // larger PNGs are scaled down with sips first
/// How long the cursor rests on an image before it is sent: scrolling past sends nothing.
/// Tapping j by hand comes at 6 to 8 keys a second, so a rest shorter than that is passing by.
const SETTLE: Duration = Duration::from_millis(300);
/// Images kept in the terminal unless IMAGE_CACHE says otherwise.
const KEEP: usize = 8;

fn keep() -> usize {
    crate::config::get("IMAGE_CACHE").trim().parse::<usize>().unwrap_or(KEEP).max(1)
}

fn name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// The file versions the terminal holds now, for marking them in lists.
static CACHED: std::sync::Mutex<Vec<(PathBuf, f64, u64)>> = std::sync::Mutex::new(Vec::new());

/// True when this version of `path` is in the terminal's image cache.
pub fn cached(path: &Path, mtime: f64, size: u64) -> bool {
    CACHED.lock().is_ok_and(|c| c.iter().any(|(p, m, s)| p == path && *m == mtime && *s == size))
}

fn png_dimensions(blob: &[u8]) -> Option<(u32, u32)> {
    if blob.len() < 24 || &blob[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let be = |at: usize| u32::from_be_bytes([blob[at], blob[at + 1], blob[at + 2], blob[at + 3]]);
    Some((be(16), be(20)))
}

/// The image as PNG no bigger than `fit` pixels: read as is, or converted and scaled down
/// with sips. Slow for big or non-PNG files, so it runs on the image-load thread, never
/// the UI's. A smaller image is less to send and less for the terminal to decode.
fn png_bytes(path: &Path, fit: (u32, u32)) -> Option<(Vec<u8>, u32, u32)> {
    let started = Instant::now();
    let result = png_bytes_timed(path, fit);
    match &result {
        Some((blob, w, h)) => crate::info!("image {}: ready as {} PNG ({w}x{h}) in {:.1} ms", name(path), crate::text::human_size(blob.len() as u64), started.elapsed().as_secs_f64() * 1000.0),
        None => crate::warn!("image {}: cannot read it as an image ({:.1} ms)", name(path), started.elapsed().as_secs_f64() * 1000.0),
    }
    result
}

/// The longest side that makes (w, h) fit in `fit`, or None when it fits already.
fn shrink_to(w: u32, h: u32, fit: (u32, u32)) -> Option<u32> {
    let scale = (fit.0 as f64 / w as f64).min(fit.1 as f64 / h as f64);
    // a little too big is not worth a conversion
    (scale < 0.9).then(|| ((w.max(h) as f64 * scale).round() as u32).max(16))
}

fn png_bytes_timed(path: &Path, fit: (u32, u32)) -> Option<(Vec<u8>, u32, u32)> {
    let mut longest = 2000;
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")) {
        let blob = std::fs::read(path).ok()?;
        if let Some((w, h)) = png_dimensions(&blob) {
            match shrink_to(w, h, fit) {
                None if blob.len() <= MAX_IMAGE_BYTES => return Some((blob, w, h)),
                Some(side) => longest = side.min(longest),
                None => {}
            }
            crate::debug!("image {}: {w}x{h} PNG of {}, scaling to fit {}x{}", name(path), crate::text::human_size(blob.len() as u64), fit.0, fit.1);
        }
    } else {
        longest = longest.min(fit.0.max(fit.1));
    }
    let sips = Instant::now();
    let tmp = std::env::var_os("TMPDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let converted = tmp.join(format!("claude-deck-{}-{n}.png", std::process::id()));
    let status = std::process::Command::new("sips")
        .args(["-s", "format", "png", "-Z", &longest.to_string()])
        .arg(path)
        .arg("--out")
        .arg(&converted)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if !status.is_ok_and(|s| s.success()) {
        // no sips (not macOS): a PNG still goes as it is
        let _ = std::fs::remove_file(&converted);
        let blob = std::fs::read(path).ok()?;
        let (w, h) = png_dimensions(&blob)?;
        return Some((blob, w, h));
    }
    let blob = std::fs::read(&converted).ok()?;
    let _ = std::fs::remove_file(&converted);
    let (w, h) = png_dimensions(&blob)?;
    crate::info!("image {}: converted with sips to {w}x{h} (longest side {longest}) in {:.1} ms", name(path), sips.elapsed().as_secs_f64() * 1000.0);
    Some((blob, w, h))
}

/// A file version: path, modification time, size.
type Key = (PathBuf, f64, u64);
type Png = Option<(Vec<u8>, u32, u32)>;
/// (col, row, cols, rows), 0-based cells
type Area = (usize, usize, usize, usize);

fn key_of(path: &Path) -> Option<Key> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    Some((path.to_path_buf(), modified, meta.len()))
}

/// A file version scaled to fit so many pixels.
type Want = (Key, (u32, u32));

/// An image the terminal holds.
struct Held {
    key: Want,
    id: u32,
    dims: (u32, u32),
    used: Instant,
}

pub struct Kitty {
    base: u32,
    next: u32,
    held: Vec<Held>,
    /// (image id, cell geometry) of what is on screen
    placed: Option<(u32, Area)>,
    /// the image wanted and since when
    want: Option<(Want, Instant)>,
    loading: Option<(Want, Receiver<Png>)>,
    /// place again even if unchanged: the screen was cleared
    pub force: bool,
    /// pixels per cell
    pub cell: (u32, u32),
}

impl Kitty {
    pub fn new() -> Kitty {
        let base = 1_000_000 + (std::process::id() % 100_000) * 4096;
        Kitty { base, next: 0, held: Vec::new(), placed: None, want: None, loading: None, force: false, cell: (8, 17) }
    }

    /// Pixels per cell from the terminal's size report, when it gives one.
    pub fn measure(&mut self) {
        if let Ok(size) = ratatui::crossterm::terminal::window_size() {
            if size.width > 0 && size.height > 0 && size.columns > 0 && size.rows > 0 {
                self.cell = ((size.width / size.columns).max(1) as u32, (size.height / size.rows).max(1) as u32);
            }
        }
    }

    /// An image is wanted but not shown yet: redraw soon.
    pub fn pending(&self) -> bool {
        self.want.is_some()
    }

    fn send(control: &str, payload: &[u8]) -> String {
        if payload.is_empty() {
            return format!("\x1b_G{control};\x1b\\");
        }
        let data = base64::engine::general_purpose::STANDARD.encode(payload);
        let chunks: Vec<&str> = data.as_bytes().chunks(4096).map(|c| std::str::from_utf8(c).unwrap_or("")).collect();
        let mut out = String::new();
        for (n, chunk) in chunks.iter().enumerate() {
            let more = if n + 1 < chunks.len() { 1 } else { 0 };
            if n == 0 {
                out.push_str(&format!("\x1b_G{control},m={more};{chunk}\x1b\\"));
            } else {
                out.push_str(&format!("\x1b_Gm={more};{chunk}\x1b\\"));
            }
        }
        out
    }

    /// Escape sequences showing `path` letterboxed in `area`; empty when nothing changed.
    pub fn show(&mut self, path: &Path, area: Area) -> String {
        let Some(file) = key_of(path) else { return self.hide() };
        let fit = (area.2 as u32 * self.cell.0, area.3 as u32 * self.cell.1);
        let key: Want = (file, fit);
        if let Some(n) = self.held.iter().position(|h| h.key == key) {
            if self.want.take().is_some() || self.placed.is_none_or(|(id, _)| id != self.held[n].id) {
                crate::info!("image {}: in the terminal's cache (id {}), placed without sending", name(&key.0.0), self.held[n].id);
            }
            self.held[n].used = Instant::now();
            let (id, dims) = (self.held[n].id, self.held[n].dims);
            return self.place(id, dims, area);
        }
        if self.want.as_ref().map(|w| &w.0) != Some(&key) {
            crate::debug!("image {}: wanted; sent if the cursor stays {} ms", name(&key.0.0), SETTLE.as_millis());
            self.want = Some((key, Instant::now()));
            return self.hide();
        }
        let since = self.want.as_ref().map(|w| w.1).unwrap_or_else(Instant::now);
        if since.elapsed() < SETTLE {
            return self.hide();
        }
        let png = match &self.loading {
            Some((k, rx)) if *k == key => match rx.try_recv() {
                Ok(png) => png,
                Err(TryRecvError::Empty) => return self.hide(),
                Err(TryRecvError::Disconnected) => None,
            },
            _ => {
                let (tx, rx) = channel();
                let (file, fit) = (key.0.0.clone(), key.1);
                crate::info!("image {}: cursor settled, reading it on the image-load thread", name(&file));
                crate::log::spawn("image-load", move || {
                    let _ = tx.send(png_bytes(&file, fit));
                });
                self.loading = Some((key, rx));
                return self.hide();
            }
        };
        self.loading = None;
        self.want = None;
        let Some((blob, w, h)) = png else { return self.hide() };
        crate::debug!("image {}: loaded, back on the UI thread", name(&key.0.0));
        let mut out = String::new();
        let keep = keep();
        while self.held.len() >= keep {
            // the least recently shown makes room
            if let Some(n) = (0..self.held.len()).min_by_key(|&n| self.held[n].used) {
                let old = self.held.remove(n);
                crate::info!("image {}: dropped from the terminal's cache (id {}), {keep} kept", name(&old.key.0.0), old.id);
                out.push_str(&format!("\x1b_Ga=d,d=I,i={},q=2;\x1b\\", old.id));
                if self.placed.is_some_and(|(id, _)| id == old.id) {
                    self.placed = None;
                }
            } else {
                break;
            }
        }
        let id = self.base + self.next % 4096;
        self.next += 1;
        let sent = Self::send(&format!("a=t,f=100,i={id},q=2"), &blob);
        crate::info!("image {}: sent to the terminal as id {id}: {} of escape sequences", name(&key.0.0), crate::text::human_size(sent.len() as u64));
        out.push_str(&sent);
        self.held.push(Held { key, id, dims: (w, h), used: Instant::now() });
        self.publish();
        out.push_str(&self.place(id, (w, h), area));
        out
    }

    fn place(&mut self, id: u32, dims: (u32, u32), area: Area) -> String {
        let (col, row, cols, rows) = area;
        let (w, h) = (dims.0 as f64, dims.1 as f64);
        let (cell_w, cell_h) = (self.cell.0 as f64, self.cell.1 as f64);
        let scale = (cols as f64 * cell_w / w).min(rows as f64 * cell_h / h).min(1.0);
        let used_cols = ((w * scale / cell_w).round_ties_even() as usize).clamp(1, cols.max(1));
        let used_rows = ((h * scale / cell_h).round_ties_even() as usize).clamp(1, rows.max(1));
        let top = row + (rows - used_rows) / 2 + 1;
        let left = col + (cols - used_cols) / 2 + 1;
        let geometry = (left, top, used_cols, used_rows);
        if self.placed == Some((id, geometry)) && !self.force {
            return String::new();
        }
        self.force = false;
        let mut out = String::new();
        if let Some((old, _)) = self.placed.filter(|(old, _)| *old != id) {
            out.push_str(&format!("\x1b_Ga=d,d=i,i={old},q=2;\x1b\\"));
        }
        out.push_str(&format!("\x1b[{top};{left}H\x1b_Ga=p,i={id},p=1,c={used_cols},r={used_rows},C=1,q=2;\x1b\\"));
        self.placed = Some((id, geometry));
        out
    }

    /// No image on this frame.
    pub fn none(&mut self) -> String {
        self.want = None;
        self.loading = None;
        self.hide()
    }

    /// Take the image off the screen (it stays in the terminal).
    pub fn hide(&mut self) -> String {
        match self.placed.take() {
            Some((id, _)) => format!("\x1b_Ga=d,d=i,i={id},q=2;\x1b\\"),
            None => String::new(),
        }
    }

    fn publish(&self) {
        if let Ok(mut c) = CACHED.lock() {
            *c = self.held.iter().map(|h| h.key.0.clone()).collect();
        }
    }

    /// Free every image the terminal holds for us.
    pub fn forget(&mut self) -> String {
        self.placed = None;
        if let Ok(mut c) = CACHED.lock() {
            c.clear();
        }
        self.held.drain(..).map(|h| format!("\x1b_Ga=d,d=I,i={},q=2;\x1b\\", h.id)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_image_is_scaled_only_when_clearly_bigger_than_its_box() {
        assert_eq!(shrink_to(2400, 1000, (1310, 546)), Some(1310));
        assert_eq!(shrink_to(1000, 2000, (800, 600)), Some(600));
        assert_eq!(shrink_to(1300, 540, (1310, 546)), None);
    }

    #[test]
    fn an_unchanged_placement_is_not_sent_again() {
        let mut k = Kitty::new();
        let first = k.place(k.base, (800, 600), (0, 3, 40, 20));
        assert!(first.contains("a=p"));
        assert_eq!(k.place(k.base, (800, 600), (0, 3, 40, 20)), "");
        k.force = true;
        assert!(k.place(k.base, (800, 600), (0, 3, 40, 20)).contains("a=p"));
        // another image takes the old one down first
        let other = k.place(k.base + 1, (800, 600), (0, 3, 40, 20));
        assert!(other.starts_with(&format!("\x1b_Ga=d,d=i,i={}", k.base)));
        assert!(k.hide().contains("a=d,d=i"));
        assert_eq!(k.hide(), "");
    }

    #[test]
    fn a_large_transmission_is_split_in_chunks_with_the_control_once() {
        let out = Kitty::send("a=t,f=100,i=1,q=2", &[7u8; 5000]);
        assert_eq!(out.matches("\x1b_G").count(), 2);
        assert!(out.starts_with("\x1b_Ga=t,f=100,i=1,q=2,m=1;"));
        assert!(out.contains("\x1b_Gm=0;"));
    }
}
