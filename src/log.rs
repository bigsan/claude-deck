//! A small log file: `LOG_LEVEL` (off, error, warn, info, debug, trace) and `LOG_FILE`
//! from the config. The TUI owns the terminal, so nothing is ever written to stderr.
//!
//! One line per event, with the thread it happened on, so it shows what runs off the
//! UI thread:
//!
//! ```text
//! 2026-10-23 15:04:05.120 12345 main         INFO  image ui.png: wanted
//! 2026-10-23 15:04:05.245 12345 image-load   INFO  image ui.png: read 223.3K png in 1.9 ms
//! ```

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::text::{hms, now, ymd};

#[derive(Clone, Copy, PartialEq, PartialOrd)]
pub enum Level {
    Off = 0,
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
    Trace = 5,
}

impl Level {
    pub fn parse(text: &str) -> Level {
        match text.trim().to_lowercase().as_str() {
            "off" | "none" | "0" | "false" | "disabled" | "" => Level::Off,
            "error" => Level::Error,
            "info" => Level::Info,
            "debug" => Level::Debug,
            "trace" => Level::Trace,
            _ => Level::Warn,
        }
    }

    fn name(self) -> &'static str {
        ["OFF", "ERROR", "WARN", "INFO", "DEBUG", "TRACE"][self as usize]
    }
}

static LEVEL: AtomicU8 = AtomicU8::new(0);
static FILE: OnceLock<Mutex<File>> = OnceLock::new();
const ROTATE_AT: u64 = 5 * 1024 * 1024;

pub fn path() -> PathBuf {
    let set = crate::config::get("LOG_FILE");
    if set.is_empty() { crate::config::state_home().join("claude-deck/claude-deck.log") } else { PathBuf::from(set) }
}

/// Open the log as the config says; a log that cannot be opened is off.
pub fn init() {
    let level = Level::parse(&crate::config::get("LOG_LEVEL"));
    if level == Level::Off {
        return;
    }
    let path = path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // one old file kept beside the current one
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > ROTATE_AT) {
        let _ = std::fs::rename(&path, path.with_extension("log.1"));
    }
    let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) else { return };
    if FILE.set(Mutex::new(file)).is_ok() {
        LEVEL.store(level as u8, Ordering::Relaxed);
    }
}

pub fn enabled(level: Level) -> bool {
    level as u8 <= LEVEL.load(Ordering::Relaxed)
}

pub fn write(level: Level, message: std::fmt::Arguments) {
    if !enabled(level) {
        return;
    }
    let Some(file) = FILE.get() else { return };
    let t = now();
    let millis = ((t.fract()) * 1000.0) as u32;
    let thread = std::thread::current();
    let name = thread.name().unwrap_or("?");
    let line = format!("{} {}.{millis:03} {} {name:<12} {:<5} {message}\n", ymd(t), hms(t), std::process::id(), level.name());
    if let Ok(mut f) = file.lock() {
        let _ = f.write_all(line.as_bytes());
    }
}

#[macro_export]
macro_rules! error { ($($t:tt)*) => { $crate::log::write($crate::log::Level::Error, format_args!($($t)*)) } }
#[macro_export]
macro_rules! warn { ($($t:tt)*) => { $crate::log::write($crate::log::Level::Warn, format_args!($($t)*)) } }
#[macro_export]
macro_rules! info { ($($t:tt)*) => { $crate::log::write($crate::log::Level::Info, format_args!($($t)*)) } }
#[macro_export]
macro_rules! debug { ($($t:tt)*) => { $crate::log::write($crate::log::Level::Debug, format_args!($($t)*)) } }

/// A thread with a name the log shows.
pub fn spawn<F: FnOnce() + Send + 'static>(name: &str, f: F) {
    if let Err(err) = std::thread::Builder::new().name(name.to_string()).spawn(f) {
        crate::error!("cannot start thread {name}: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_read_loosely_and_unknown_means_warn() {
        assert!(Level::parse("OFF") == Level::Off && Level::parse("disabled") == Level::Off);
        assert!(Level::parse("Debug") == Level::Debug);
        assert!(Level::parse("loud") == Level::Warn);
    }
}
