//! The Claude Code sessions running on this machine, and where each one keeps its files.
//!
//! Every interactive Claude Code process writes `~/.claude/sessions/<pid>.json`:
//!
//! ```text
//! {"pid": 33111, "sessionId": "8e379c28-…", "cwd": "/Users/me/src/claude-deck",
//!  "name": "claude-deck-93", "status": "idle", "updatedAt": 1792736225850, …}
//! ```
//!
//! The record outlives its process, so a pid that no longer runs is skipped. After
//! /clear the same process carries on under a new `sessionId`.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::data::{claude_config_dir, find_transcript, project_slug, temp_root};

#[derive(Clone, Debug)]
pub struct Live {
    pub pid: i32,
    pub session_id: String,
    pub cwd: String,
    pub name: String,
    pub busy: bool,
    /// epoch seconds of the record's last update
    pub updated: f64,
    /// epoch seconds the status turned what it is, busy or idle
    pub status_since: f64,
    pub started: f64,
}

impl Live {
    pub fn project(&self) -> &str {
        Path::new(&self.cwd).file_name().and_then(|n| n.to_str()).unwrap_or(&self.cwd)
    }

    pub fn target(&self) -> Target {
        target_for(&self.session_id, &self.cwd)
    }
}

/// A session to open: its temp directory, transcript and label.
#[derive(Clone, Debug)]
pub struct Target {
    pub dir: PathBuf,
    pub transcript: Option<PathBuf>,
    pub label: String,
    /// the Claude Code process to follow across /clear, when known
    pub pid: Option<i32>,
}

pub fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // EPERM: the process exists but belongs to someone else
    let sent = unsafe { libc::kill(pid, 0) } == 0;
    sent || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn parse(record: &Value) -> Option<Live> {
    let pid = number(&record["pid"])? as i32;
    let session_id = record["sessionId"].as_str().filter(|s| !s.is_empty())?.to_string();
    let cwd = record["cwd"].as_str().filter(|s| !s.is_empty())?.to_string();
    let ms = |field: &str| number(&record[field]).map(|v| v / 1000.0).unwrap_or(0.0);
    Some(Live {
        pid,
        name: record["name"].as_str().unwrap_or("").to_string(),
        busy: record["status"].as_str() == Some("busy"),
        updated: ms("updatedAt").max(ms("statusUpdatedAt")),
        status_since: if ms("statusUpdatedAt") > 0.0 { ms("statusUpdatedAt") } else { ms("updatedAt") },
        started: ms("startedAt"),
        session_id,
        cwd,
    })
}

fn read_record(path: &Path) -> Option<Value> {
    serde_json::from_slice::<Value>(&fs::read(path).ok()?).ok().filter(Value::is_object)
}

/// The record of process `pid`, following it to the job it parked on.
pub fn record_of(pid: i32) -> Option<Live> {
    let sessions = claude_config_dir().join("sessions");
    let record = read_record(&sessions.join(format!("{pid}.json")))?;
    if let Some(parked) = record["parkedJobId"].as_str().filter(|p| !p.is_empty()) {
        for entry in fs::read_dir(&sessions).ok()?.flatten() {
            if let Some(job) = read_record(&entry.path()) {
                if job["jobId"].as_str() == Some(parked) {
                    if let Some(live) = parse(&job).filter(|l| pid_alive(l.pid)) {
                        return Some(live);
                    }
                }
            }
        }
    }
    parse(&record)
}

/// Every running session, most recently updated first.
pub fn live_sessions() -> Vec<Live> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(claude_config_dir().join("sessions")) else { return out };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(live) = read_record(&path).as_ref().and_then(parse) else { continue };
        if pid_alive(live.pid) && !out.iter().any(|l: &Live| l.session_id == live.session_id) {
            out.push(live);
        }
    }
    out.sort_by(|a, b| b.updated.total_cmp(&a.updated));
    out
}

pub fn target_for(session_id: &str, cwd: &str) -> Target {
    let dir = temp_root().join(project_slug(cwd)).join(session_id);
    let transcript = find_transcript(&dir);
    let project = Path::new(cwd).file_name().and_then(|n| n.to_str()).unwrap_or(cwd);
    let id: String = session_id.chars().take(8).collect();
    Target { dir, transcript, label: format!("{project} · {id}"), pid: None }
}

/// A session named on the command line: a directory, or a session id or its prefix.
pub fn resolve(arg: &str) -> Result<Target, String> {
    let path = Path::new(arg);
    if path.is_dir() {
        let dir = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        // by id: the same directory may be spelt /tmp or /private/tmp
        let id = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let pid = live_sessions().into_iter().find(|l| l.session_id == id).map(|l| l.pid);
        return Ok(Target { transcript: find_transcript(&dir), label: crate::app::default_label(&dir), pid, dir });
    }
    let live: Vec<Live> = live_sessions().into_iter().filter(|l| l.session_id.starts_with(arg)).collect();
    match live.len() {
        1 => return Ok(Target { pid: Some(live[0].pid), ..live[0].target() }),
        n if n > 1 => return Err(format!("{arg}: {n} running sessions start with this")),
        _ => {}
    }
    // a session that is no longer running: its transcript names its project
    let mut found: Vec<PathBuf> = Vec::new();
    if let Ok(projects) = fs::read_dir(claude_config_dir().join("projects")) {
        for project in projects.flatten() {
            let Ok(files) = fs::read_dir(project.path()) else { continue };
            for file in files.flatten() {
                let name = file.file_name().to_string_lossy().into_owned();
                if name.starts_with(arg) && name.ends_with(".jsonl") {
                    found.push(file.path());
                }
            }
        }
    }
    match found.len() {
        0 => Err(format!("{arg}: no such directory or session id")),
        1 => {
            let transcript = found.remove(0);
            let id = transcript.file_stem().unwrap_or_default().to_string_lossy().into_owned();
            let slug = transcript.parent().and_then(|p| p.file_name()).unwrap_or_default().to_string_lossy().into_owned();
            let dir = temp_root().join(&slug).join(&id);
            Ok(Target { label: crate::app::default_label(&dir), dir, transcript: Some(transcript), pid: None })
        }
        n => Err(format!("{arg}: {n} sessions start with this")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_record_reads_a_pid_given_as_text_and_times_in_seconds() {
        let live = parse(&json!({ "pid": "123", "sessionId": "abc", "cwd": "/x/proj", "status": "busy", "updatedAt": 2000, "statusUpdatedAt": 3000 })).unwrap();
        assert_eq!((live.pid, live.busy, live.updated, live.project()), (123, true, 3.0, "proj"));
        assert!(parse(&json!({ "pid": 1, "cwd": "/x" })).is_none());
    }

    #[test]
    fn this_process_is_alive_and_pid_zero_is_not() {
        assert!(pid_alive(std::process::id() as i32));
        assert!(!pid_alive(0));
    }
}
