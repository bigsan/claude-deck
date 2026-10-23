//! Settings, from (later wins):
//!
//! ```text
//! the defaults below
//! config.env in the herdr plugin's config directory   KEY=VALUE lines, # comments
//! DECK_<KEY> environment variables
//! ```
//!
//! One file whether the deck runs in a herdr pane or on its own: herdr names the
//! directory (`herdr plugin config-dir bigsan.claude-deck`, `$HERDR_PLUGIN_CONFIG_DIR` in a
//! pane), and outside herdr the same default path is read. A key may be written with or
//! without its DECK_ prefix.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::data::home;

/// (key, default, what it does)
pub const KEYS: &[(&str, &str, &str)] = &[
    ("LOG_LEVEL", "warn", "off, error, warn, info, debug or trace"),
    ("LOG_FILE", "", "where the log goes; default ~/.local/state/claude-deck/claude-deck.log"),
    ("MOUSE", "1", "1 lets the mouse scroll, click and double-click; 0 leaves it to the terminal, to drag-select text"),
    ("IMAGE_CACHE", "8", "images kept in the terminal, so going back to one does not send it again"),
    // herdr plugin
    ("OPEN_ON", "", "agent statuses that open the deck, comma-separated"),
    ("CLOSE_ON", "", "statuses that close it again"),
    ("DIRECTION", "right", "right or down"),
    ("RATIO", "0.40", "the share of the split the deck takes"),
    ("FOCUS", "1", "1 moves the focus to the deck when it opens, 0 keeps it on the agent"),
    ("VIEW", "A", "the view it opens in: A tabs, B columns, C timeline"),
    ("SCRATCHPAD_ROOT", "", "the Claude temp root; default /tmp/claude-<uid>"),
];

pub fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home().join(".config"))
}

pub fn state_home() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home().join(".local/state"))
}

/// The config file: the herdr plugin's.
pub fn file() -> PathBuf {
    let dir = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| config_home().join("herdr/plugins/config").join(crate::herdr::PLUGIN_ID));
    dir.join("config.env")
}

fn read_file(path: &Path, into: &mut HashMap<String, String>) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim().to_uppercase();
        let key = key.strip_prefix("DECK_").unwrap_or(&key).to_string();
        if into.contains_key(&key) {
            into.insert(key, value.trim().trim_matches(['\'', '"']).to_string());
        }
    }
}

/// Every setting, read now.
pub fn load() -> HashMap<String, String> {
    let mut cfg: HashMap<String, String> = KEYS.iter().map(|(k, v, _)| (k.to_string(), v.to_string())).collect();
    read_file(&file(), &mut cfg);
    for (key, value) in cfg.iter_mut() {
        if let Ok(over) = std::env::var(format!("DECK_{key}")) {
            *value = over;
        }
    }
    cfg
}

/// One setting, read once per process.
pub fn get(key: &str) -> String {
    static CFG: OnceLock<HashMap<String, String>> = OnceLock::new();
    CFG.get_or_init(load).get(key).cloned().unwrap_or_default()
}

/// A config.env listing every key with its default and what it does.
pub fn example(header: &str) -> String {
    let mut out = String::from(header);
    for (key, default, what) in KEYS {
        out.push_str(&format!("\n# {what}\n{key}={default}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_sets_known_keys_with_or_without_the_prefix() {
        let dir = std::env::temp_dir().join(format!("claude-deck-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.env");
        std::fs::write(&path, "# x\nDECK_LOG_LEVEL=debug\nIMAGE_CACHE = '3'\nUNKNOWN=1\n").unwrap();
        let mut cfg: HashMap<String, String> = KEYS.iter().map(|(k, v, _)| (k.to_string(), v.to_string())).collect();
        read_file(&path, &mut cfg);
        assert_eq!((cfg["LOG_LEVEL"].as_str(), cfg["IMAGE_CACHE"].as_str()), ("debug", "3"));
        assert!(!cfg.contains_key("UNKNOWN"));
    }
}
