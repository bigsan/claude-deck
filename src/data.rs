//! Everything the deck shows about one session, read from disk.
//!
//! A Claude Code session leaves its temp directory,
//! `/tmp/claude-<uid>/<project slug>/<session id>/`:
//!
//! ```text
//! scratchpad/   the agent's working files
//! tasks/        b*.output  a shell command's output; kept only for background commands
//!               a*.output  a symlink to a subagent's transcript
//! images/       pasted images
//! gate.jsonl    written by the session-deck-hooks Claude Code plugin, if installed
//! ```
//!
//! and its transcript, `~/.claude/projects/<slug>/<session id>.jsonl` (subagents under
//! `<session id>/subagents/agent-<id>.jsonl`). Nothing here writes anywhere, and every
//! reader is incremental: a refresh reads only what was appended since the last one.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::text::{one_line, parse_ts, sanitize};

pub const MAX_FILES: usize = 2000;
pub const MAX_WALK: usize = 20000;
const MAX_DEPTH: usize = 12;
pub const PREVIEW_BYTES: usize = 256 * 1024;
const OUTPUT_TAIL: u64 = 64 * 1024;
pub const FRESH: f64 = 30.0;

const IMAGE_SUFFIXES: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "tiff", "tif", "heic"];
/// Directories that are someone else's code or build output; never the agent's work.
const SKIP_DIRS: &[&str] = &[
    ".git", ".hg", ".svn", "node_modules", "bower_components", "vendor", "Pods", ".venv", "venv",
    "__pycache__", ".mypy_cache", ".pytest_cache", ".ruff_cache", ".tox", ".next", ".nuxt", ".turbo",
    ".gradle", ".cache", "target",
];
const SHELL_TOOLS: &[&str] = &["Bash", "BashOutput", "KillShell", "KillBash"];
const FILE_TOOLS: &[&str] = &["Read", "Write", "Edit", "MultiEdit", "NotebookEdit", "Glob", "Grep"];

fn mtime(meta: &fs::Metadata) -> f64 {
    meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn birth(meta: &fs::Metadata) -> f64 {
    meta.created().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs_f64()).unwrap_or_else(|| mtime(meta))
}

/// The file as text (the last `limit` bytes with `tail`), or None when it looks binary.
pub fn read_text(path: &Path, limit: usize, tail: bool) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    if tail {
        let size = file.metadata().ok()?.len();
        file.seek(SeekFrom::Start(size.saturating_sub(limit as u64))).ok()?;
    }
    let mut data = Vec::new();
    file.take(limit as u64).read_to_end(&mut data).ok()?;
    if data[..data.len().min(8192)].contains(&0) {
        return None;
    }
    Some(sanitize(&String::from_utf8_lossy(&data), false).replace('\t', "    "))
}

// ------------------------------------------------------------------- files

#[derive(Clone, Debug)]
pub struct FileItem {
    pub path: PathBuf,
    pub rel: String,
    pub size: u64,
    pub mtime: f64,
    pub is_image: bool,
}

impl FileItem {
    pub fn key(&self) -> String {
        format!("file:{}", self.rel)
    }
    pub fn fresh(&self, now: f64) -> bool {
        now - self.mtime < FRESH
    }
}

/// scratchpad/ and images/: the newest MAX_FILES files.
///
/// The whole tree is walked (up to MAX_WALK files) before the newest are kept, so a
/// file written just now is listed even in a scratchpad holding a cloned repository.
pub fn scan_files(session_dir: &Path) -> Vec<FileItem> {
    let mut found: Vec<(f64, u64, PathBuf, String)> = Vec::new();
    fn walk(dir: &Path, prefix: &str, depth: usize, found: &mut Vec<(f64, u64, PathBuf, String)>) {
        if depth > MAX_DEPTH || found.len() >= MAX_WALK {
            return;
        }
        let Ok(entries) = fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) {
                    walk(&entry.path(), &format!("{prefix}{name}/"), depth + 1, found);
                }
                continue;
            }
            if name == ".DS_Store" {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            found.push((mtime(&meta), meta.len(), entry.path(), format!("{prefix}{name}")));
            if found.len() >= MAX_WALK {
                return;
            }
        }
    }
    walk(&session_dir.join("scratchpad"), "", 0, &mut found);
    walk(&session_dir.join("images"), "images/", 0, &mut found);
    found.sort_by(|a, b| b.0.total_cmp(&a.0));
    found.truncate(MAX_FILES);
    found
        .into_iter()
        .map(|(mtime, size, path, rel)| {
            let is_image = path.extension().map(|e| IMAGE_SUFFIXES.contains(&e.to_string_lossy().to_lowercase().as_str())).unwrap_or(false);
            FileItem { path, rel, size, mtime, is_image }
        })
        .collect()
}

// ------------------------------------------------------------------- tasks

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskKind {
    Shell,
    Agent,
}

#[derive(Clone, Debug)]
pub struct TaskItem {
    pub path: PathBuf,
    pub name: String,
    pub kind: TaskKind,
    pub size: u64,
    pub mtime: f64,
    pub born: f64,
    pub exit: Option<i32>,
    pub rows: usize,
    pub text: String,
    /// what the call that started it said it was doing: its description and command
    pub desc: String,
    pub command: String,
}

static EXITED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[exited with code (-?\d+)\]\s*$").unwrap());

impl TaskItem {
    pub fn key(&self) -> String {
        format!("task:{}", self.name)
    }

    pub fn running(&self, now: f64) -> bool {
        match self.kind {
            TaskKind::Agent => now - self.mtime < FRESH,
            TaskKind::Shell => self.exit.is_none() && now - self.mtime < 3600.0,
        }
    }

    fn load(&mut self) {
        match self.kind {
            TaskKind::Shell => {
                self.text = read_text(&self.path, OUTPUT_TAIL as usize, true).unwrap_or_default();
                self.exit = EXITED.captures(&self.text).and_then(|c| c[1].parse().ok());
            }
            TaskKind::Agent => {
                let (text, rows) = condense_agent(&self.path, 400);
                self.text = text;
                self.rows = rows;
            }
        }
    }
}

/// A subagent transcript as readable lines: prompts, text, tool calls.
pub fn condense_agent(path: &Path, limit: usize) -> (String, usize) {
    let Ok(raw) = fs::read_to_string(path) else { return (String::new(), 0) };
    let mut lines: Vec<String> = Vec::new();
    let mut rows = 0;
    for line in raw.lines() {
        rows += 1;
        let Ok(record) = serde_json::from_str::<Value>(line) else { continue };
        let message = &record["message"];
        match &message["content"] {
            Value::String(text) => lines.push(format!("▸ {}", clip(&one_line(text), 300))),
            Value::Array(blocks) => {
                for block in blocks {
                    match block["type"].as_str() {
                        Some("text") if message["role"] == "assistant" => {
                            lines.push(format!("◆ {}", clip(&one_line(block["text"].as_str().unwrap_or("")), 300)))
                        }
                        Some("tool_use") => {
                            let name = block["name"].as_str().unwrap_or("?");
                            lines.push(format!("  ❯ {name} {}", clip(&summarize(name, &block["input"]), 200)))
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    let start = lines.len().saturating_sub(limit);
    (sanitize(&lines[start..].join("\n"), false), rows)
}

fn clip(text: &str, chars: usize) -> String {
    text.chars().take(chars).collect()
}

/// tasks/*.output, newest first; unchanged files keep their parsed content.
pub fn scan_tasks(session_dir: &Path, previous: &HashMap<String, TaskItem>) -> Vec<TaskItem> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(session_dir.join("tasks")) else { return out };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("output") {
            continue;
        }
        // through the symlink: the subagent transcript grows, not the link
        let Ok(meta) = fs::metadata(&path) else { continue };
        let name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let (size, modified) = (meta.len(), mtime(&meta));
        if let Some(old) = previous.get(&name) {
            if old.size == size && old.mtime == modified {
                out.push(old.clone());
                continue;
            }
        }
        let is_link = fs::symlink_metadata(&path).map(|m| m.file_type().is_symlink()).unwrap_or(false);
        let kind = if name.starts_with('a') && is_link { TaskKind::Agent } else { TaskKind::Shell };
        let mut item = TaskItem { path, name, kind, size, mtime: modified, born: birth(&meta), exit: None, rows: 0, text: String::new(), desc: String::new(), command: String::new() };
        item.load();
        out.push(item);
    }
    out.sort_by(|a, b| b.mtime.total_cmp(&a.mtime));
    out
}

// ------------------------------------------------------------------- calls

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Shell,
    File,
    Other,
}

impl Family {
    pub fn of(tool: &str) -> Family {
        if SHELL_TOOLS.contains(&tool) {
            Family::Shell
        } else if FILE_TOOLS.contains(&tool) {
            Family::File
        } else {
            Family::Other
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Family::Shell => "shell",
            Family::File => "file",
            Family::Other => "other",
        }
    }
}

/// What a call did, in one line: the command, the path, the pattern.
pub fn summarize(_tool: &str, input: &Value) -> String {
    let Value::Object(map) = input else { return one_line(&input.to_string()) };
    for field in ["command", "file_path", "notebook_path", "url", "pattern", "query", "description", "prompt", "skill"] {
        if let Some(Value::String(value)) = map.get(field) {
            if value.is_empty() {
                continue;
            }
            if field == "pattern" {
                if let Some(Value::String(path)) = map.get("path") {
                    return format!("{value}  in {path}");
                }
            }
            return one_line(value);
        }
    }
    clip(&one_line(&py_json(input)), 300)
}

/// JSON the way Python's `json.dumps` writes it by default: `", "` and `": "` between items.
pub fn py_json(value: &Value) -> String {
    struct Spaced;
    impl serde_json::ser::Formatter for Spaced {
        fn begin_array_value<W: ?Sized + std::io::Write>(&mut self, writer: &mut W, first: bool) -> std::io::Result<()> {
            if first { Ok(()) } else { writer.write_all(b", ") }
        }
        fn begin_object_key<W: ?Sized + std::io::Write>(&mut self, writer: &mut W, first: bool) -> std::io::Result<()> {
            if first { Ok(()) } else { writer.write_all(b", ") }
        }
        fn begin_object_value<W: ?Sized + std::io::Write>(&mut self, writer: &mut W) -> std::io::Result<()> {
            writer.write_all(b": ")
        }
    }
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut out, Spaced);
    match serde::Serialize::serialize(value, &mut ser) {
        Ok(()) => String::from_utf8(out).unwrap_or_default(),
        Err(_) => value.to_string(),
    }
}

static DENIAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)Permission to use .* (has been )?denied|doesn't want to proceed|was blocked|denied by").unwrap());
static EXIT_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^Exit code (-?\d+)").unwrap());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    None,
    Auto,
    Ask,
    Deny,
    Reject,
    Wait,
}

#[derive(Clone, Debug)]
pub struct Call {
    pub id: String,
    pub tool: String,
    pub input: Value,
    pub started: f64,
    pub agent: Option<String>,
    pub family: Family,
    pub summary: String,
    pub desc: String,
    pub ended: Option<f64>,
    pub output: String,
    pub is_error: bool,
    pub exit: Option<i32>,
    pub interrupted: bool,
    pub denied: bool,
    #[allow(dead_code)] // read by the Python reader's callers; kept for parity
    pub background: bool,
    /// tasks/ id of a background command's output
    pub task: Option<String>,
    /// tasks/ output of this call while it runs, by name
    pub live: Option<String>,
    pub verdict: Verdict,
    pub mode: String,
    /// order read from the transcripts, which breaks ties between calls started together
    pub seq: usize,
}

impl Call {
    pub fn new(id: &str, tool: &str, input: Value, started: f64, agent: Option<String>) -> Call {
        let desc = input["description"].as_str().unwrap_or("").to_string();
        let background = input["run_in_background"].as_bool().unwrap_or(false);
        Call {
            id: id.to_string(),
            tool: tool.to_string(),
            family: Family::of(tool),
            summary: summarize(tool, &input),
            input,
            started,
            agent,
            desc,
            ended: None,
            output: String::new(),
            is_error: false,
            exit: None,
            interrupted: false,
            denied: false,
            background,
            task: None,
            live: None,
            verdict: Verdict::None,
            mode: String::new(),
            seq: 0,
        }
    }

    pub fn key(&self) -> String {
        format!("call:{}", self.id)
    }
    pub fn pending(&self) -> bool {
        self.ended.is_none()
    }
    pub fn duration_ms(&self) -> Option<f64> {
        self.ended.map(|e| ((e - self.started) * 1000.0).max(0.0))
    }
    pub fn refused(&self) -> bool {
        self.denied || matches!(self.verdict, Verdict::Deny | Verdict::Reject)
    }
    pub fn failed(&self) -> bool {
        self.refused()
            || self.interrupted
            || self.exit.is_some_and(|e| e != 0)
            || (self.is_error && self.family != Family::Shell)
    }

    pub fn finish(&mut self, at: f64, text: &str, is_error: bool, extra: &Value) {
        self.ended = Some(at);
        // Read output keeps its tabs: they separate the line-number gutter from the text.
        let clean = sanitize(text, self.tool == "Read");
        self.output = clean.chars().take(PREVIEW_BYTES).collect();
        self.is_error = is_error;
        self.interrupted = extra["interrupted"].as_bool().unwrap_or(false);
        if let Some(task) = extra["backgroundTaskId"].as_str().filter(|t| !t.is_empty()) {
            self.task = Some(task.to_string());
        }
        let head: String = text.chars().take(400).collect();
        if is_error && DENIAL.is_match(&head) {
            self.denied = true;
        } else if self.family == Family::Shell {
            self.exit = match EXIT_CODE.captures(text) {
                Some(c) => c[1].parse().ok(),
                None if is_error => None,
                None => Some(0),
            };
        }
    }

    pub fn command(&self) -> Option<&str> {
        if self.family == Family::Shell { self.input["command"].as_str() } else { None }
    }
}

/// Something the user sent, and the turn it started: what Claude said and ran until
/// the next one.
#[derive(Clone, Debug)]
pub struct Prompt {
    pub id: String,
    pub text: String,
    pub images: usize,
    pub at: f64,
    /// when the next prompt came, which ends this turn's share of the calls
    pub until: Option<f64>,
    /// the turn's latest record
    pub last: f64,
    /// Claude's reply was final, or the turn was interrupted
    pub done: bool,
    /// the turn as lines: ◆ what Claude said, ❯ the tools it ran
    pub lines: Vec<String>,
    /// tokens Claude wrote, per message (a message is spread over several records)
    output: HashMap<String, u64>,
    /// tokens the turn's latest request carried
    pub context_tokens: u64,
}

const TURN_LINES: usize = 400;
const REPLY_CHARS: usize = 4000;

impl Prompt {
    pub fn key(&self) -> String {
        format!("prompt:{}", self.id)
    }

    pub fn output_tokens(&self) -> u64 {
        self.output.values().sum()
    }

    /// The turn so far, or all of it once done.
    pub fn took(&self) -> f64 {
        (self.last - self.at).max(0.0)
    }

    /// The prompt in a user record, as typed: a slash command with its arguments, a
    /// `!` command, pasted text without its wrapper. None for everything else a user
    /// record carries: tool results, notes Claude Code adds, a command's echoed output.
    fn read(record: &Value) -> Option<Prompt> {
        if record["type"] != "user" || record["isMeta"].as_bool() == Some(true) {
            return None;
        }
        let id = record["promptId"].as_str()?;
        let (text, images) = match &record["message"]["content"] {
            Value::String(s) => (s.clone(), 0),
            Value::Array(blocks) => {
                if blocks.iter().any(|b| b["type"] == "tool_result") {
                    return None;
                }
                let text = blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n");
                (text, blocks.iter().filter(|b| b["type"] == "image").count())
            }
            _ => return None,
        };
        let t = text.trim();
        let tag = |name: &str| t.split(&format!("<{name}>")).nth(1).and_then(|r| r.split(&format!("</{name}>")).next()).map(str::trim);
        let text = if let Some(name) = tag("command-name") {
            format!("{name} {}", tag("command-args").unwrap_or("")).trim_end().to_string()
        } else if let Some(command) = tag("bash-input") {
            format!("! {command}")
        } else if t.starts_with("<pasted_content") {
            PASTED.replace_all(t, "").trim().to_string()
        } else if t.starts_with('<') || t.starts_with("[Request interrupted") {
            return None;
        } else {
            t.to_string()
        };
        if text.is_empty() && images == 0 {
            return None;
        }
        let at = parse_ts(record["timestamp"].as_str().unwrap_or(""));
        Some(Prompt { id: id.to_string(), text, images, at, until: None, last: at, done: false, lines: Vec::new(), output: HashMap::new(), context_tokens: 0 })
    }

    /// Take in a record of this turn.
    fn follow(&mut self, record: &Value, at: f64) {
        self.last = self.last.max(at);
        let message = &record["message"];
        if record["type"] == "user" {
            if let Value::Array(blocks) = &message["content"] {
                if blocks.iter().any(|b| b["text"].as_str().is_some_and(|t| t.starts_with("[Request interrupted"))) {
                    self.done = true;
                    self.push("⌁ interrupted".into());
                }
            }
            return;
        }
        if record["type"] != "assistant" {
            return;
        }
        for block in message["content"].as_array().into_iter().flatten() {
            match block["type"].as_str() {
                // what Claude said is what a turn is read for: kept as written, lines and all
                Some("text") => self.push(format!("◆ {}", clip(block["text"].as_str().unwrap_or("").trim(), REPLY_CHARS))),
                Some("tool_use") => {
                    let name = block["name"].as_str().unwrap_or("?");
                    self.push(format!("  ❯ {name} {}", clip(&summarize(name, &block["input"]), 200)));
                }
                _ => {}
            }
        }
        let usage = &message["usage"];
        if usage.is_object() {
            let n = |k: &str| usage[k].as_u64().unwrap_or(0);
            self.output.insert(message["id"].as_str().unwrap_or("").to_string(), n("output_tokens"));
            self.context_tokens = n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens");
        }
        // a background task's notice can wake Claude again after its reply, with no prompt
        if let Some(reason) = message["stop_reason"].as_str() {
            self.done = reason == "end_turn";
        }
    }

    fn push(&mut self, line: String) {
        if self.lines.len() < TURN_LINES {
            self.lines.push(line);
        }
    }
}

static PASTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"</?pasted_content[^>]*>").unwrap());

fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| match b["type"].as_str() {
                Some("text") => Some(b["text"].as_str().unwrap_or("").to_string()),
                Some("image") => Some("[image]".to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Reads complete JSON lines appended to a file since the last read.
pub struct JsonlTail {
    pub path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
}

impl JsonlTail {
    pub fn new(path: PathBuf) -> JsonlTail {
        JsonlTail { path, offset: 0, partial: Vec::new() }
    }

    pub fn read(&mut self) -> Vec<Value> {
        let Ok(meta) = fs::metadata(&self.path) else { return Vec::new() };
        let size = meta.len();
        if size < self.offset {
            // truncated or replaced: start over
            self.offset = 0;
            self.partial.clear();
        }
        if size == self.offset {
            return Vec::new();
        }
        let Ok(mut file) = fs::File::open(&self.path) else { return Vec::new() };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut data = Vec::new();
        if file.take(size - self.offset).read_to_end(&mut data).is_err() {
            return Vec::new();
        }
        self.offset += data.len() as u64;
        let mut buffer = std::mem::take(&mut self.partial);
        buffer.extend_from_slice(&data);
        let mut out = Vec::new();
        let mut start = 0;
        for (i, byte) in buffer.iter().enumerate() {
            if *byte == b'\n' {
                if let Ok(value) = serde_json::from_slice::<Value>(&buffer[start..i]) {
                    if value.is_object() {
                        out.push(value);
                    }
                }
                start = i + 1;
            }
        }
        self.partial = buffer[start..].to_vec();
        out
    }
}

/// The tool calls in one transcript file.
pub struct Transcript {
    tail: JsonlTail,
    agent: Option<String>,
}

impl Transcript {
    pub fn new(path: PathBuf, agent: Option<String>) -> Transcript {
        Transcript { tail: JsonlTail::new(path), agent }
    }

    /// Take in what was appended: tool calls into `calls`, and, from the main transcript,
    /// prompts and their turns into `prompts`.
    pub fn feed(&mut self, calls: &mut HashMap<String, Call>, prompts: &mut Vec<Prompt>) -> bool {
        let mut changed = false;
        for record in self.tail.read() {
            let at = parse_ts(record["timestamp"].as_str().unwrap_or(""));
            if self.agent.is_none() {
                if let Some(prompt) = Prompt::read(&record) {
                    if let Some(previous) = prompts.last_mut() {
                        previous.until = Some(prompt.at);
                    }
                    prompts.push(prompt);
                    changed = true;
                } else if let Some(turn) = prompts.last_mut() {
                    turn.follow(&record, at);
                    changed = true;
                }
            }
            let Some(blocks) = record["message"]["content"].as_array() else { continue };
            for block in blocks {
                match block["type"].as_str() {
                    Some("tool_use") => {
                        let Some(id) = block["id"].as_str() else { continue };
                        if calls.contains_key(id) {
                            continue;
                        }
                        let input = if block["input"].is_object() { block["input"].clone() } else { Value::Object(Default::default()) };
                        let mut call = Call::new(id, block["name"].as_str().unwrap_or("?"), input, at, self.agent.clone());
                        call.seq = calls.len();
                        calls.insert(id.to_string(), call);
                        changed = true;
                    }
                    Some("tool_result") => {
                        let Some(call) = block["tool_use_id"].as_str().and_then(|id| calls.get_mut(id)) else { continue };
                        if call.ended.is_none() {
                            let is_error = block["is_error"].as_bool().unwrap_or(false);
                            call.finish(at, &result_text(&block["content"]), is_error, &record["toolUseResult"]);
                            changed = true;
                        }
                    }
                    _ => {}
                }
            }
        }
        changed
    }
}

// ------------------------------------------------------------------- verdicts

/// Merge gate.jsonl into each call's verdict.
///
/// Per tool_use id the hook logs pre → (ask) → ok | fail | denied. An ask carries no
/// id, so it is paired with the latest pre of the same input hash before it.
///
/// ```text
/// pre, ok|fail            auto    allowed by a rule or the mode
/// pre, ask, ok|fail       ask     you were asked and allowed it
/// pre, ask, then nothing  reject  you said no (or wait, while it is open)
/// pre, denied             deny    the auto-mode classifier refused
/// pre only, denial text   deny    a deny rule refused
/// ```
pub fn verdicts(lines: &[Value], calls: &mut HashMap<String, Call>) {
    let mut by_id: HashMap<String, (HashSet<String>, String)> = HashMap::new();
    let mut pres: Vec<&Value> = Vec::new();
    let mut asks: Vec<&Value> = Vec::new();
    for line in lines {
        let ev = line["ev"].as_str().unwrap_or("");
        if ev == "ask" {
            asks.push(line);
            continue;
        }
        let Some(id) = line["id"].as_str() else { continue };
        let entry = by_id.entry(id.to_string()).or_insert_with(|| (HashSet::new(), line["mode"].as_str().unwrap_or("").to_string()));
        entry.0.insert(ev.to_string());
        if ev == "pre" {
            pres.push(line);
        }
    }
    let ts = |v: &Value| v["ts"].as_f64().unwrap_or(0.0);
    let mut taken: HashSet<String> = HashSet::new();
    for ask in asks {
        let chosen = pres
            .iter()
            .filter(|p| p["key"] == ask["key"] && !taken.contains(p["id"].as_str().unwrap_or("")) && ts(p) <= ts(ask) + 2.0)
            .max_by(|a, b| ts(a).total_cmp(&ts(b)));
        if let Some(pre) = chosen {
            let id = pre["id"].as_str().unwrap_or("").to_string();
            taken.insert(id.clone());
            if let Some(entry) = by_id.get_mut(&id) {
                entry.0.insert("ask".into());
            }
        }
    }
    for (id, (evs, mode)) in by_id {
        let Some(call) = calls.get_mut(&id) else { continue };
        call.mode = mode;
        let completed = evs.contains("ok") || evs.contains("fail");
        call.verdict = if evs.contains("denied") {
            Verdict::Deny
        } else if evs.contains("ask") && completed {
            Verdict::Ask
        } else if evs.contains("ask") {
            if call.pending() { Verdict::Wait } else { Verdict::Reject }
        } else if completed {
            Verdict::Auto
        } else if call.denied {
            Verdict::Deny
        } else if !call.pending() {
            Verdict::Auto
        } else {
            Verdict::None
        };
    }
}

// ------------------------------------------------------------------- session

pub fn project_slug(path: &str) -> String {
    path.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

pub fn claude_config_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| home().join(".claude"))
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

pub fn temp_root() -> PathBuf {
    let base = std::env::var_os("CLAUDE_CODE_TMPDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join(format!("claude-{}", unsafe { libc::getuid() }))
}

/// `~/.claude/projects/<slug>/<session>.jsonl` for a session temp directory.
pub fn find_transcript(session_dir: &Path) -> Option<PathBuf> {
    let projects = claude_config_dir().join("projects");
    let session = session_dir.file_name()?.to_string_lossy().into_owned();
    let slug = session_dir.parent()?.file_name()?.to_string_lossy().into_owned();
    let direct = projects.join(&slug).join(format!("{session}.jsonl"));
    if direct.exists() {
        return Some(direct);
    }
    for entry in fs::read_dir(&projects).ok()?.flatten() {
        let candidate = entry.path().join(format!("{session}.jsonl"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// One session's files, tasks and calls, refreshed incrementally.
pub struct Session {
    pub dir: PathBuf,
    pub transcript_path: Option<PathBuf>,
    pub files: Vec<FileItem>,
    /// background shells and subagents
    pub tasks: Vec<TaskItem>,
    /// output files of foreground commands still running
    pub live: Vec<TaskItem>,
    pub calls: HashMap<String, Call>,
    /// what the user sent, oldest first
    pub prompts: Vec<Prompt>,
    transcripts: Vec<(PathBuf, Transcript)>,
    gate: JsonlTail,
    gate_lines: Vec<Value>,
    pub has_gate: bool,
    signature: u64,
    pub scan_files: bool,
    /// drop each call's output once read, for summaries of many sessions
    pub light: bool,
}

impl Session {
    pub fn new(dir: PathBuf, transcript: Option<PathBuf>) -> Session {
        let transcript_path = transcript.or_else(|| find_transcript(&dir));
        let transcripts = transcript_path.iter().map(|p| (p.clone(), Transcript::new(p.clone(), None))).collect();
        let gate = JsonlTail::new(dir.join("gate.jsonl"));
        Session {
            dir,
            transcript_path,
            files: Vec::new(),
            tasks: Vec::new(),
            live: Vec::new(),
            calls: HashMap::new(),
            prompts: Vec::new(),
            transcripts,
            gate,
            gate_lines: Vec::new(),
            has_gate: false,
            signature: 0,
            scan_files: true,
            light: false,
        }
    }

    fn add_subagents(&mut self) {
        let Some(path) = &self.transcript_path else { return };
        let folder = path.with_extension("").join("subagents");
        let Ok(entries) = fs::read_dir(folder) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            if !(name.starts_with("agent-") && name.ends_with(".jsonl")) || self.transcripts.iter().any(|(p, _)| *p == path) {
                continue;
            }
            let agent = name.trim_start_matches("agent-").trim_end_matches(".jsonl").to_string();
            self.transcripts.push((path.clone(), Transcript::new(path, Some(agent))));
        }
    }

    /// Re-read what changed; true when anything the views show is different.
    pub fn refresh(&mut self) -> bool {
        if self.scan_files {
            self.files = scan_files(&self.dir);
        }
        let previous: HashMap<String, TaskItem> = self.tasks.iter().chain(self.live.iter()).map(|t| (t.name.clone(), t.clone())).collect();
        let found = scan_tasks(&self.dir, &previous);
        self.add_subagents();
        let mut changed = false;
        for (_, transcript) in &mut self.transcripts {
            changed |= transcript.feed(&mut self.calls, &mut self.prompts);
        }
        if self.light && changed {
            for call in self.calls.values_mut().filter(|c| !c.output.is_empty()) {
                call.output = String::new();
            }
        }
        self.split_tasks(found);
        let new = self.gate.read();
        if !new.is_empty() {
            self.gate_lines.extend(new.iter().cloned());
            self.has_gate = true;
        }
        if !new.is_empty() || changed {
            verdicts(&self.gate_lines, &mut self.calls);
        }
        let signature = self.signature_now();
        let dirty = signature != self.signature;
        self.signature = signature;
        dirty
    }

    fn signature_now(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for f in self.files.iter().take(200) {
            (&f.rel, f.size, f.mtime.to_bits()).hash(&mut h);
        }
        for t in self.tasks.iter().chain(self.live.iter()) {
            (&t.name, t.size, t.mtime.to_bits()).hash(&mut h);
        }
        self.calls.len().hash(&mut h);
        if let Some(p) = self.prompts.last() {
            (self.prompts.len(), p.lines.len(), p.done, p.last.to_bits()).hash(&mut h);
        }
        self.calls.values().filter(|c| c.ended.is_some()).count().hash(&mut h);
        self.gate_lines.len().hash(&mut h);
        h.finish()
    }

    /// Sort tasks/ into lasting tasks and the live output of running commands.
    ///
    /// Claude Code gives every shell command a tasks/b*.output file and deletes it
    /// when the command ends; only a background command's file stays, and the
    /// transcript names it (backgroundTaskId). The others are paired, oldest first,
    /// with the running shell calls whose start is closest to the file's.
    fn split_tasks(&mut self, found: Vec<TaskItem>) {
        let background: HashSet<String> = self.calls.values().filter_map(|c| c.task.clone()).collect();
        let (mut tasks, mut live): (Vec<TaskItem>, Vec<TaskItem>) =
            found.into_iter().partition(|t| t.kind == TaskKind::Agent || background.contains(&t.name));
        for task in &mut tasks {
            if let Some(call) = self.calls.values().find(|c| c.task.as_deref() == Some(task.name.as_str())) {
                task.desc = call.desc.clone();
                task.command = call.input["command"].as_str().unwrap_or("").to_string();
            }
        }
        live.sort_by(|a, b| a.born.total_cmp(&b.born));
        for call in self.calls.values_mut() {
            call.live = None;
        }
        let mut running: Vec<(String, f64)> =
            self.calls.values().filter(|c| c.pending() && c.family == Family::Shell).map(|c| (c.id.clone(), c.started)).collect();
        for item in &live {
            let Some(best) = (0..running.len()).min_by(|&a, &b| (running[a].1 - item.born).abs().total_cmp(&(running[b].1 - item.born).abs())) else {
                break;
            };
            let (id, _) = running.remove(best);
            if let Some(call) = self.calls.get_mut(&id) {
                call.live = Some(item.name.clone());
            }
        }
        self.tasks = tasks;
        self.live = live;
    }

    pub fn calls_newest(&self) -> Vec<&Call> {
        let mut calls: Vec<&Call> = self.calls.values().collect();
        calls.sort_by(|a, b| b.started.total_cmp(&a.started).then_with(|| a.seq.cmp(&b.seq)));
        calls
    }

    /// Calls in the order they were read.
    pub fn calls_in_order(&self) -> Vec<&Call> {
        let mut calls: Vec<&Call> = self.calls.values().collect();
        calls.sort_by_key(|c| c.seq);
        calls
    }

    pub fn prompts_newest(&self) -> impl Iterator<Item = &Prompt> {
        self.prompts.iter().rev()
    }

    /// The calls a prompt's turn made, subagents' too.
    pub fn turn_calls(&self, prompt: &Prompt) -> Vec<&Call> {
        let mut calls: Vec<&Call> = self.calls.values().filter(|c| c.started >= prompt.at && prompt.until.is_none_or(|u| c.started < u)).collect();
        calls.sort_by_key(|c| c.seq);
        calls
    }

    pub fn task_named(&self, name: &str) -> Option<&TaskItem> {
        self.tasks.iter().chain(self.live.iter()).find(|t| t.name == name)
    }

    pub fn last_activity(&self) -> f64 {
        let call = self.calls.values().map(|c| c.ended.unwrap_or(c.started)).fold(0.0, f64::max);
        let file = self.files.first().map(|f| f.mtime).unwrap_or(0.0);
        call.max(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bash(id: &str, command: &str, extra: Value) -> Call {
        let mut input = json!({ "command": command });
        if let Value::Object(map) = extra {
            input.as_object_mut().unwrap().extend(map);
        }
        Call::new(id, "Bash", input, 100.0, None)
    }

    #[test]
    fn summary_falls_back_to_json_spaced_like_python() {
        assert_eq!(summarize("X", &json!({ "a": [1, 2], "b": { "c": "本" } })), r#"{"a": [1, 2], "b": {"c": "本"}}"#);
    }

    #[test]
    fn exit_code_only_comes_from_a_failed_result() {
        let mut ok = bash("a", "true", json!({}));
        let mut failed = bash("b", "false", json!({}));
        ok.finish(101.0, "done", false, &json!({}));
        failed.finish(101.5, "Exit code 2\nls: nope", true, &json!({}));
        assert_eq!((ok.exit, ok.failed()), (Some(0), false));
        assert_eq!((failed.exit, failed.failed()), (Some(2), true));
        assert_eq!(failed.duration_ms(), Some(1500.0));
    }

    #[test]
    fn denial_text_marks_the_call_denied_not_exited() {
        let mut call = bash("c", "rm -rf x", json!({}));
        call.finish(101.0, "Permission to use Bash with command rm -rf x has been denied.", true, &json!({}));
        assert!(call.denied);
        assert_eq!(call.exit, None);
    }

    #[test]
    fn background_call_links_to_its_task_output() {
        let mut call = bash("d", "npm run dev", json!({ "run_in_background": true }));
        call.finish(101.0, "Command running in background with ID: b123", false, &json!({ "backgroundTaskId": "b123" }));
        assert_eq!(call.task.as_deref(), Some("b123"));
    }

    fn merged(lines: Vec<Value>, ended: bool, error: bool) -> Verdict {
        let mut call = bash("t1", "echo", json!({}));
        if ended {
            call.finish(101.0, if error { "Permission to use Bash denied" } else { "ok" }, error, &json!({}));
        }
        let mut calls = HashMap::from([("t1".to_string(), call)]);
        verdicts(&lines, &mut calls);
        calls["t1"].verdict
    }

    #[test]
    fn each_path_through_the_gate() {
        let pre = json!({ "ev": "pre", "id": "t1", "key": "k", "ts": 1 });
        let ask = json!({ "ev": "ask", "key": "k", "ts": 2 });
        let ok = json!({ "ev": "ok", "id": "t1", "key": "k", "ts": 3 });
        assert_eq!(merged(vec![pre.clone(), ok.clone()], true, false), Verdict::Auto);
        assert_eq!(merged(vec![pre.clone(), ask.clone(), ok], true, false), Verdict::Ask);
        assert_eq!(merged(vec![pre.clone(), ask.clone()], true, true), Verdict::Reject);
        assert_eq!(merged(vec![pre.clone(), ask], false, false), Verdict::Wait);
        assert_eq!(merged(vec![pre.clone(), json!({ "ev": "denied", "id": "t1", "ts": 2 })], true, true), Verdict::Deny);
        assert_eq!(merged(vec![pre], true, true), Verdict::Deny);
    }

    #[test]
    fn an_ask_written_before_its_pre_still_pairs() {
        let lines = vec![json!({ "ev": "ask", "key": "k", "ts": 2.0 }), json!({ "ev": "pre", "id": "t1", "key": "k", "ts": 2.1 }), json!({ "ev": "ok", "id": "t1", "ts": 3 })];
        assert_eq!(merged(lines, true, false), Verdict::Ask);
    }

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("claude-deck-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn jsonl_tail_holds_back_a_partial_line() {
        let dir = tempdir("tail");
        let path = dir.join("t.jsonl");
        fs::write(&path, "{\"a\": 1}\n{\"b\"").unwrap();
        let mut tail = JsonlTail::new(path.clone());
        assert_eq!(tail.read(), vec![json!({ "a": 1 })]);
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        std::io::Write::write_all(&mut f, b": 2}\n").unwrap();
        assert_eq!(tail.read(), vec![json!({ "b": 2 })]);
    }

    #[test]
    fn prompts_start_turns_that_gather_what_claude_said_and_ran() {
        let dir = tempdir("prompts");
        let session = dir.join("slug").join("sess");
        fs::create_dir_all(&session).unwrap();
        let ts = |s: u32| format!("2026-10-22T06:00:{s:02}.000Z");
        let user = |s: u32, content: Value, pid: bool| {
            let mut r = json!({ "type": "user", "timestamp": ts(s), "message": { "role": "user", "content": content } });
            if pid {
                r["promptId"] = json!(format!("p{s}"));
            }
            r
        };
        let said = |s: u32, id: &str, content: Value, stop: &str, out: u64| {
            json!({ "type": "assistant", "timestamp": ts(s), "message": { "id": id, "role": "assistant", "content": content, "stop_reason": stop,
                "usage": { "input_tokens": 10, "cache_read_input_tokens": 1000, "cache_creation_input_tokens": 0, "output_tokens": out } } })
        };
        let rows = [
            // Claude Code's own notes and echoes are not prompts
            json!({ "type": "user", "isMeta": true, "timestamp": ts(0), "message": { "content": "<local-command-caveat>…</local-command-caveat>" } }),
            user(1, json!("<command-name>/clear</command-name>"), false),
            user(2, json!("fix the build"), true),
            said(3, "m1", json!([{ "type": "text", "text": "Looking.\n\n- one" }]), "tool_use", 5),
            // the same message's next block repeats its usage: counted once
            said(3, "m1", json!([{ "type": "tool_use", "id": "t1", "name": "Bash", "input": { "command": "cargo build" } }]), "tool_use", 5),
            json!({ "type": "user", "timestamp": ts(9), "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t1", "content": "ok" }] } }),
            said(12, "m2", json!([{ "type": "text", "text": "Fixed." }]), "end_turn", 40),
            user(13, json!("<task-notification><task-id>b1</task-id></task-notification>"), true),
            user(20, json!("<command-name>/review</command-name>\n<command-args>T12</command-args>"), true),
            user(30, json!([{ "type": "image", "source": {} }, { "type": "text", "text": "<pasted_content id=\"1\">a\nb</pasted_content>" }]), true),
            user(31, json!([{ "type": "text", "text": "[Request interrupted by user]" }]), true),
            user(40, json!("<bash-input>git status</bash-input>"), true),
            said(41, "m3", json!([{ "type": "tool_use", "id": "t2", "name": "Read", "input": { "file_path": "/x" } }]), "tool_use", 1),
        ];
        let transcript = dir.join("sess.jsonl");
        fs::write(&transcript, rows.iter().map(|r| r.to_string() + "\n").collect::<String>()).unwrap();
        let mut s = Session::new(session, Some(transcript));
        assert!(s.refresh());
        let texts: Vec<&str> = s.prompts.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(texts, ["fix the build", "/review T12", "a\nb", "! git status"]);
        let first = &s.prompts[0];
        assert_eq!(first.lines, ["◆ Looking.\n\n- one", "  ❯ Bash cargo build", "◆ Fixed."]);
        assert_eq!((first.done, first.took(), first.until), (true, 11.0, Some(parse_ts(&ts(20)))));
        assert_eq!((first.output_tokens(), first.context_tokens), (45, 1010));
        assert_eq!(s.turn_calls(first).iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["t1"]);
        // the pasted prompt with its image was interrupted; the last one is still going
        assert_eq!((s.prompts[2].images, s.prompts[2].done, s.prompts[2].lines.as_slice()), (1, true, &["⌁ interrupted".to_string()][..]));
        let last = s.prompts.last().unwrap();
        assert_eq!((last.done, last.until, s.turn_calls(last).len()), (false, None, 1));
        assert!(!s.refresh());
    }

    #[test]
    fn background_output_is_a_task_and_a_running_commands_output_is_live() {
        let dir = tempdir("split");
        let session = dir.join("slug").join("sess");
        fs::create_dir_all(session.join("tasks")).unwrap();
        fs::write(session.join("tasks/bbg.output"), "serving\n").unwrap();
        fs::write(session.join("tasks/bfg.output"), "step 1\nstep 2\n").unwrap();
        let use_ = |id: &str, cmd: &str, bg: bool| json!({ "timestamp": "2026-10-22T06:00:00.000Z", "message": { "role": "assistant", "content": [{ "type": "tool_use", "id": id, "name": "Bash", "input": { "command": cmd, "run_in_background": bg } }] } });
        let rows = [
            use_("t1", "npm run dev", true),
            json!({ "timestamp": "2026-10-22T06:00:01.000Z", "toolUseResult": { "backgroundTaskId": "bbg" }, "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t1", "content": "Command running in background with ID: bbg" }] } }),
            use_("t2", "make test", false),
            json!({ "timestamp": "2026-10-22T06:00:02.000Z", "message": { "role": "assistant", "content": [{ "type": "tool_use", "id": "t3", "name": "Bash", "input": { "command": "ls x" } }] } }),
            json!({ "timestamp": "2026-10-22T06:00:03.250Z", "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t3", "is_error": true, "content": "Exit code 1\nnope" }] } }),
        ];
        let transcript = dir.join("sess.jsonl");
        fs::write(&transcript, rows.iter().map(|r| r.to_string() + "\n").collect::<String>()).unwrap();
        let mut s = Session::new(session, Some(transcript));
        assert!(s.refresh());
        assert_eq!(s.tasks.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["bbg"]);
        assert_eq!(s.tasks[0].command, "npm run dev");
        assert_eq!(s.calls["t2"].live.as_deref(), Some("bfg"));
        assert_eq!((s.calls["t3"].exit, s.calls["t3"].duration_ms()), (Some(1), Some(1250.0)));
        assert!(!s.refresh());
    }

    #[test]
    fn scan_keeps_the_newest_file_past_the_cap_and_skips_vendor_dirs() {
        let dir = tempdir("scan");
        let repo = dir.join("scratchpad/repo");
        fs::create_dir_all(repo.join("node_modules")).unwrap();
        fs::write(repo.join("node_modules/dep.js"), "x").unwrap();
        for n in 0..(MAX_FILES + 5) {
            let path = repo.join(format!("f{n}.txt"));
            fs::write(&path, "x").unwrap();
            let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1000 + n as u64);
            fs::File::options().write(true).open(&path).unwrap().set_modified(t).unwrap();
        }
        fs::write(dir.join("scratchpad/new.txt"), "x").unwrap();
        let files = scan_files(&dir);
        assert_eq!(files.len(), MAX_FILES);
        assert_eq!(files[0].rel, "new.txt");
        assert!(!files.iter().any(|f| f.rel.contains("node_modules")));
    }
}
