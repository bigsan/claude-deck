//! The herdr plugin's actions and events: dock a Claude Code session's deck beside its
//! agent pane, follow it, close it.
//!
//! Subcommands of `claude-deck herdr`: event | open | open-a | open-b | open-c | close |
//! toggle | path | reveal.
//!
//! Adapted from herdr-scratchdock's bin/dock.py (https://github.com/mvaios/herdr-scratchdock,
//! MIT, Copyright (c) 2026 Vahram Mirzoyan): the pane → session resolution, the per-pane
//! dock state and lock, and the split resizing are its; the Codex resolver is dropped and
//! the dock is rooted at the session directory instead of its scratchpad.
//!
//! `event` is the manifest hook and must stay quiet and idempotent: herdr fires it on
//! every agent state transition. The other subcommands are user-facing actions.

use std::collections::HashMap;
use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use crate::data::{home, project_slug, temp_root};
use crate::sessions::{Target, record_of, target_for};

pub const PLUGIN_ID: &str = "bigsan.claude-deck";

// A session directory created within this many seconds before the agent process
// started still counts as that process's own.
const START_SLACK: f64 = 10.0;


fn herdr_bin() -> String {
    std::env::var("HERDR_BIN_PATH").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "herdr".into())
}

fn plugin_id() -> String {
    std::env::var("HERDR_PLUGIN_ID").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| PLUGIN_ID.into())
}

// ---------------------------------------------------------------- config/state

/// The settings: see config.rs.
pub fn config() -> HashMap<String, String> {
    crate::config::load()
}

fn state_dir() -> PathBuf {
    let base = std::env::var_os("HERDR_PLUGIN_STATE_DIR").map(PathBuf::from).unwrap_or_else(|| home().join(".local/state/claude-deck"));
    let path = base.join("docks");
    let _ = fs::create_dir_all(&path);
    path
}

fn safe_name(agent_pane: &str) -> String {
    agent_pane.chars().map(|c| if c.is_ascii_alphanumeric() || "_.:-".contains(c) { c } else { '_' }).collect()
}

fn state_file(agent_pane: &str) -> PathBuf {
    state_dir().join(format!("{}.json", safe_name(agent_pane)))
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice::<Value>(&fs::read(path).ok()?).ok().filter(Value::is_object)
}

fn load_state(agent_pane: &str) -> Value {
    read_json(&state_file(agent_pane)).unwrap_or_else(|| json!({}))
}

fn save_state(agent_pane: &str, data: &Value) {
    // Write-then-rename: a reader catching a half-written file would see "no dock".
    let path = state_file(agent_pane);
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    if fs::write(&temporary, data.to_string()).is_err() || fs::rename(&temporary, &path).is_err() {
        let _ = fs::remove_file(&temporary);
    }
}

fn clear_state(agent_pane: &str) {
    let _ = fs::remove_file(state_file(agent_pane));
}

/// The per-pane lock, held while the guard lives; None if another hook process has it.
struct Lock(Option<fs::File>);

impl Lock {
    fn take(agent_pane: &str) -> Option<Lock> {
        let path = state_dir().join(format!("{}.lock", safe_name(agent_pane)));
        let Ok(file) = fs::File::create(path) else { return Some(Lock(None)) };
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return None;
        }
        Some(Lock(Some(file)))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        if let Some(file) = &self.0 {
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

// ------------------------------------------------------------------ herdr calls

fn herdr(args: &[&str]) -> Option<Value> {
    let out = Command::new(herdr_bin()).args(args).output().ok()?;
    if !out.status.success() {
        crate::warn!("herdr {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

type Panes = HashMap<String, Value>;

/// Every pane by id, or None when herdr could not be asked (not: no panes).
fn snapshot_panes() -> Option<Panes> {
    let doc = herdr(&["api", "snapshot"])?;
    let snap = &doc["result"]["snapshot"];
    let mut panes: Panes = HashMap::new();
    for row in snap["panes"].as_array().into_iter().flatten().chain(snap["agents"].as_array().into_iter().flatten()) {
        let Some(id) = row["pane_id"].as_str() else { continue };
        let entry = panes.entry(id.to_string()).or_insert_with(|| json!({}));
        if let (Some(into), Some(from)) = (entry.as_object_mut(), row.as_object()) {
            into.extend(from.clone());
        }
    }
    Some(panes)
}

/// PID of the claude process running in `pane_id`.
pub fn agent_pid(pane_id: &str) -> Option<i32> {
    let doc = herdr(&["pane", "process-info", "--pane", pane_id])?;
    let procs = doc["result"]["process_info"]["foreground_processes"].as_array()?.clone();
    let base = |s: &str| Path::new(s).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    for proc in procs {
        let argv: Vec<&str> = proc["argv"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        let argv0 = proc["argv0"].as_str().or(argv.first().copied()).unwrap_or("");
        let mut names = vec![base(argv0)];
        names.extend(argv.iter().skip(1).take(2).map(|a| base(a)));
        if names.iter().any(|n| n == "claude") {
            return proc["pid"].as_i64().map(|p| p as i32);
        }
    }
    None
}

fn process_start(pid: i32) -> Option<f64> {
    let out = Command::new("ps").args(["-o", "lstart=", "-p", &pid.to_string()]).output().ok()?;
    let stamp = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if stamp.is_empty() {
        return None;
    }
    // "Thu Oct 22 03:01:57 2026", local time
    let c = std::ffi::CString::new(stamp).ok()?;
    let format = std::ffi::CString::new("%a %b %d %H:%M:%S %Y").ok()?;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let end = unsafe { libc::strptime(c.as_ptr(), format.as_ptr(), &mut tm) };
    if end.is_null() {
        return None;
    }
    tm.tm_isdst = -1;
    Some(unsafe { libc::mktime(&mut tm) } as f64)
}

// -------------------------------------------------------------------- resolving

fn scratchpad_root(cfg: &HashMap<String, String>) -> PathBuf {
    match cfg.get("SCRATCHPAD_ROOT").filter(|r| !r.is_empty()) {
        Some(root) => PathBuf::from(root),
        None => temp_root(),
    }
}

fn birth_time(path: &Path) -> f64 {
    fs::metadata(path).ok().and_then(|m| m.created().or_else(|_| m.modified()).ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn rooted(target: Target, session_id: &str, cwd: &str, cfg: &HashMap<String, String>) -> Target {
    Target { dir: scratchpad_root(cfg).join(project_slug(cwd)).join(session_id), ..target }
}

/// The session of the Claude process running in `pane_id`.
///
/// Exact from Claude Code's per-process session record; otherwise inferred from
/// the pane's cwd and the agent process's start time (see scratchdock's README).
fn resolve(pane_id: &str, pane: &Value, cfg: &HashMap<String, String>) -> Option<Target> {
    if pane["agent"].as_str().unwrap_or("") != "claude" {
        return None;
    }
    let pid = agent_pid(pane_id);
    let target = match pid.and_then(record_of) {
        Some(live) => rooted(target_for(&live.session_id, &live.cwd), &live.session_id, &live.cwd, cfg),
        None => {
            let cwd = pane["cwd"].as_str().or(pane["foreground_cwd"].as_str()).filter(|c| !c.is_empty())?;
            let base = scratchpad_root(cfg).join(project_slug(cwd));
            let mut sessions: Vec<PathBuf> = fs::read_dir(&base).ok()?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
            if sessions.is_empty() {
                return None;
            }
            if let Some(started) = pid.and_then(process_start) {
                let own: Vec<PathBuf> = sessions.iter().filter(|s| birth_time(s) >= started - START_SLACK).cloned().collect();
                if !own.is_empty() {
                    sessions = own;
                }
            }
            let newest = sessions.into_iter().max_by(|a, b| birth_time(a).total_cmp(&birth_time(b)))?;
            let id = newest.file_name()?.to_string_lossy().into_owned();
            rooted(target_for(&id, cwd), &id, cwd, cfg)
        }
    };
    let _ = fs::create_dir_all(&target.dir);
    Some(Target { pid, ..target })
}

fn target_json(target: &Target) -> Value {
    json!({
        "dir": target.dir.to_string_lossy(),
        "transcript": target.transcript.as_ref().map(|t| t.to_string_lossy().into_owned()).unwrap_or_default(),
        "label": target.label,
    })
}

/// Record the new session when it moved under an open deck (/clear, a parked job).
fn retarget(agent_pane: &str, cfg: &HashMap<String, String>, panes: Option<&Panes>) {
    let mut state = load_state(agent_pane);
    let Some(pane) = panes.and_then(|p| p.get(agent_pane)) else { return };
    if state["dock_pane"].as_str().is_none() {
        return;
    }
    let Some(target) = resolve(agent_pane, pane, cfg) else { return };
    if state["dir"].as_str() != Some(&*target.dir.to_string_lossy()) {
        if let (Some(into), Value::Object(from)) = (state.as_object_mut(), target_json(&target)) {
            into.extend(from);
        }
        save_state(agent_pane, &state);
    }
}

// ------------------------------------------------------------------ dock opening

/// The deck pane recorded for `agent_pane`, if still open (or herdr unreachable).
fn live_dock(agent_pane: &str, panes: Option<&Panes>) -> Option<String> {
    let dock = load_state(agent_pane)["dock_pane"].as_str()?.to_string();
    match panes {
        None => Some(dock),
        Some(p) if p.contains_key(&dock) => Some(dock),
        Some(_) => {
            clear_state(agent_pane);
            None
        }
    }
}

fn contains(outer: &Value, inner: &Value) -> bool {
    let n = |v: &Value, k: &str| v[k].as_f64();
    let (Some(ox), Some(oy), Some(ow), Some(oh), Some(ix), Some(iy), Some(iw), Some(ih)) =
        (n(outer, "x"), n(outer, "y"), n(outer, "width"), n(outer, "height"), n(inner, "x"), n(inner, "y"), n(inner, "width"), n(inner, "height"))
    else {
        return false;
    };
    ox <= ix && oy <= iy && ox + ow >= ix + iw && oy + oh >= iy + ih
}

/// Move the fresh 50/50 divider so the deck takes RATIO of its split.
fn resize_dock(dock_pane: &str, cfg: &HashMap<String, String>) {
    let Ok(ratio) = cfg["RATIO"].parse::<f64>() else { return };
    if !(0.05..=0.95).contains(&ratio) {
        return;
    }
    let Some(doc) = herdr(&["pane", "layout", "--pane", dock_pane]) else { return };
    let layout = &doc["result"]["layout"];
    let Some(rect) = layout["panes"].as_array().into_iter().flatten().find(|p| p["pane_id"].as_str() == Some(dock_pane)).map(|p| p["rect"].clone()) else {
        return;
    };
    let direction = cfg["DIRECTION"].as_str();
    let vertical = direction == "down";
    let size_key = if vertical { "height" } else { "width" };
    let split = layout["splits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["direction"].as_str() == Some(direction) && contains(&s["rect"], &rect))
        .min_by(|a, b| a["rect"][size_key].as_f64().unwrap_or(0.0).total_cmp(&b["rect"][size_key].as_f64().unwrap_or(0.0)));
    let Some(current) = split.and_then(|s| s["ratio"].as_f64()) else { return };
    let delta = (1.0 - ratio) - current;
    if delta.abs() < 0.01 {
        return;
    }
    let (grow, shrink) = if vertical { ("down", "up") } else { ("right", "left") };
    let amount = format!("{:.4}", delta.abs());
    herdr(&["pane", "resize", "--pane", dock_pane, "--direction", if delta > 0.0 { grow } else { shrink }, "--amount", &amount]);
}

fn open_dock(agent_pane: &str, cfg: &HashMap<String, String>, panes: Option<&Panes>) -> Result<String, String> {
    let Some(_lock) = Lock::take(agent_pane) else { return Ok("an open is already in flight".into()) };
    let fresh;
    let panes = match panes {
        Some(p) => Some(p),
        None => {
            fresh = snapshot_panes();
            fresh.as_ref()
        }
    };
    if let Some(existing) = live_dock(agent_pane, panes) {
        if cfg["FOCUS"] == "1" {
            herdr(&["plugin", "pane", "focus", &existing]);
            return Ok(format!("already docked in {existing}: focused it"));
        }
        return Ok(format!("already docked in {existing}"));
    }
    let pane = panes.and_then(|p| p.get(agent_pane)).ok_or_else(|| format!("no such pane: {agent_pane}"))?;
    let target = resolve(agent_pane, pane, cfg).ok_or_else(|| format!("no Claude session found for {agent_pane}"))?;
    let dir = target.dir.to_string_lossy().into_owned();
    let transcript = target.transcript.as_ref().map(|t| t.to_string_lossy().into_owned()).unwrap_or_default();
    let envs = [
        format!("DECK_SESSION_DIR={dir}"),
        format!("DECK_TRANSCRIPT={transcript}"),
        format!("DECK_LABEL={}", target.label),
        format!("DECK_AGENT_PANE={agent_pane}"),
        format!("DECK_VIEW={}", cfg["VIEW"]),
    ];
    let id = plugin_id();
    let mut args: Vec<&str> =
        vec!["plugin", "pane", "open", "--plugin", &id, "--entrypoint", "deck", "--placement", "split", "--target-pane", agent_pane, "--direction", &cfg["DIRECTION"], "--cwd", &dir];
    for env in &envs {
        args.extend(["--env", env.as_str()]);
    }
    args.push(if cfg["FOCUS"] == "1" { "--focus" } else { "--no-focus" });
    let doc = herdr(&args).ok_or("herdr plugin pane open failed")?;
    let dock = doc["result"]["plugin_pane"]["pane"]["pane_id"].as_str().ok_or("herdr plugin pane open failed")?.to_string();
    let mut state = target_json(&target);
    state["dock_pane"] = json!(dock);
    save_state(agent_pane, &state);
    resize_dock(&dock, cfg);
    Ok(format!("docked {dir} in {dock}"))
}

fn close_dock(agent_pane: &str, panes: Option<&Panes>) -> String {
    let dock = live_dock(agent_pane, panes);
    clear_state(agent_pane);
    match dock {
        None => format!("no deck open for {agent_pane}"),
        Some(dock) => {
            herdr(&["plugin", "pane", "close", &dock]);
            format!("closed {dock}")
        }
    }
}

// --------------------------------------------------------------------- dispatch

fn states() -> Vec<(String, Value)> {
    let Ok(entries) = fs::read_dir(state_dir()) else { return Vec::new() };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .filter_map(|p| Some((p.file_stem()?.to_string_lossy().into_owned(), read_json(&p)?)))
        .collect()
}

/// The focused pane, or the agent it belongs to when the deck itself is focused.
fn focused_agent_pane(panes: Option<&Panes>) -> Option<String> {
    let pane_id = std::env::var("HERDR_PANE_ID").ok().filter(|p| !p.is_empty()).or_else(|| {
        let context: Value = serde_json::from_str(&std::env::var("HERDR_PLUGIN_CONTEXT_JSON").unwrap_or_default()).unwrap_or(Value::Null);
        context["focused_pane_id"].as_str().map(str::to_string)
    })?;
    if let Some((agent, _)) = states().into_iter().find(|(_, s)| s["dock_pane"].as_str() == Some(&pane_id)) {
        return Some(agent);
    }
    match panes {
        None => Some(pane_id),
        Some(p) => p.contains_key(&pane_id).then_some(pane_id),
    }
}

fn handle_event(cfg: &HashMap<String, String>) {
    let event: Value = serde_json::from_str(&std::env::var("HERDR_PLUGIN_EVENT_JSON").unwrap_or_default()).unwrap_or(Value::Null);
    let data = &event["data"];
    let (kind, Some(pane_id)) = (data["type"].as_str().unwrap_or(""), data["pane_id"].as_str()) else { return };
    if kind == "pane_closed" {
        if load_state(pane_id).as_object().is_some_and(|o| !o.is_empty()) {
            close_dock(pane_id, snapshot_panes().as_ref());
        } else {
            for (agent, state) in states() {
                if state["dock_pane"].as_str() == Some(pane_id) {
                    let _ = fs::remove_file(state_dir().join(format!("{agent}.json")));
                }
            }
        }
        return;
    }
    if kind != "pane_agent_status_changed" || data["agent"].as_str().unwrap_or("") != "claude" {
        return;
    }
    let status = data["agent_status"].as_str().unwrap_or("");
    let list = |key: &str| cfg[key].split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect::<Vec<_>>();
    if list("CLOSE_ON").iter().any(|s| s == status) {
        close_dock(pane_id, snapshot_panes().as_ref());
        return;
    }
    let panes = snapshot_panes();
    if live_dock(pane_id, panes.as_ref()).is_some() {
        retarget(pane_id, cfg, panes.as_ref());
    } else if list("OPEN_ON").iter().any(|s| s == status) {
        let _ = open_dock(pane_id, cfg, panes.as_ref());
    }
}

/// Every running session in a split beside the focused pane, the cursor on that pane's.
fn open_sessions(cfg: &HashMap<String, String>, panes: Option<&Panes>) -> i32 {
    let Some(pane_id) = focused_agent_pane(panes) else {
        eprintln!("claude-deck: no focused pane (run this from inside herdr)");
        return 1;
    };
    let pane = panes.and_then(|p| p.get(&pane_id));
    let cwd = pane.and_then(|p| p["cwd"].as_str().or(p["foreground_cwd"].as_str())).unwrap_or("").to_string();
    let id = plugin_id();
    let mut args = vec!["plugin", "pane", "open", "--plugin", &id, "--entrypoint", "sessions", "--placement", "split", "--target-pane", &pane_id, "--direction", &cfg["DIRECTION"]];
    if !cwd.is_empty() {
        args.extend(["--cwd", &cwd]);
    }
    args.push("--focus");
    match herdr(&args) {
        Some(_) => 0,
        None => {
            eprintln!("claude-deck: herdr plugin pane open failed");
            1
        }
    }
}

/// `claude-deck herdr <mode>`: the exit code.
pub fn main(mode: &str) -> i32 {
    let mut cfg = config();
    crate::info!("herdr {mode}");
    if mode == "event" {
        // a hook must never fail loudly
        let _ = std::panic::catch_unwind(|| handle_event(&cfg));
        return 0;
    }
    let panes = snapshot_panes();
    if mode == "sessions" {
        return open_sessions(&cfg, panes.as_ref());
    }
    let Some(agent_pane) = focused_agent_pane(panes.as_ref()) else {
        eprintln!("claude-deck: no focused pane (run this from inside herdr)");
        return 1;
    };
    let mut mode = mode.to_string();
    if let Some(view) = mode.strip_prefix("open-") {
        cfg.insert("VIEW".into(), view.to_uppercase());
        close_dock(&agent_pane, panes.as_ref());
        mode = "open".into();
    }
    let result: Result<String, String> = match mode.as_str() {
        "open" => open_dock(&agent_pane, &cfg, panes.as_ref()),
        "close" => Ok(close_dock(&agent_pane, panes.as_ref())),
        "toggle" => {
            if live_dock(&agent_pane, panes.as_ref()).is_some() {
                Ok(close_dock(&agent_pane, panes.as_ref()))
            } else {
                open_dock(&agent_pane, &cfg, panes.as_ref())
            }
        }
        "path" | "reveal" => {
            let pane = panes.as_ref().and_then(|p| p.get(&agent_pane)).cloned().unwrap_or(json!({}));
            match resolve(&agent_pane, &pane, &cfg) {
                None => Err(format!("no Claude session found for {agent_pane}")),
                Some(t) if mode == "path" => Ok(format!("{}\n{}", t.dir.display(), t.transcript.map(|p| p.display().to_string()).unwrap_or_default())),
                Some(t) => {
                    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
                    let _ = Command::new(opener).arg(&t.dir).spawn();
                    Ok(format!("revealed {}", t.dir.display()))
                }
            }
        }
        other => {
            eprintln!("claude-deck: unknown herdr command {other:?}");
            return 2;
        }
    };
    match result {
        Ok(message) => {
            println!("{message}");
            0
        }
        Err(message) => {
            eprintln!("{message}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_split_contains_the_pane_inside_it() {
        let outer = json!({ "x": 0, "y": 0, "width": 100, "height": 40 });
        assert!(contains(&outer, &json!({ "x": 60, "y": 0, "width": 40, "height": 40 })));
        assert!(!contains(&outer, &json!({ "x": 60, "y": 0, "width": 41, "height": 40 })));
        assert!(!contains(&outer, &json!({})));
    }

    #[test]
    fn pane_ids_become_safe_file_names() {
        assert_eq!(safe_name("w1/p:2 x"), "w1_p:2_x");
    }
}
