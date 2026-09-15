use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

pub const DEFAULT_POLL_SECONDS: u64 = 10;
pub const DEFAULT_COOLDOWN_SECONDS: u64 = 300;
pub const DEFAULT_CONNECT_GRACE_SECONDS: u64 = 120;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Config {
    #[serde(default = "default_poll_seconds")]
    pub poll_seconds: u64,
    #[serde(default = "default_cooldown_seconds")]
    pub cooldown_seconds: u64,
    #[serde(default = "default_connect_grace_seconds")]
    pub connect_grace_seconds: u64,
    #[serde(default = "default_commands")]
    pub commands: HashMap<String, String>,
}

fn default_poll_seconds() -> u64 {
    DEFAULT_POLL_SECONDS
}

fn default_cooldown_seconds() -> u64 {
    DEFAULT_COOLDOWN_SECONDS
}

fn default_connect_grace_seconds() -> u64 {
    DEFAULT_CONNECT_GRACE_SECONDS
}

impl Default for Config {
    fn default() -> Self {
        Self {
            poll_seconds: DEFAULT_POLL_SECONDS,
            cooldown_seconds: DEFAULT_COOLDOWN_SECONDS,
            connect_grace_seconds: DEFAULT_CONNECT_GRACE_SECONDS,
            commands: default_commands(),
        }
    }
}

pub fn default_commands() -> HashMap<String, String> {
    HashMap::from([
        ("claude".into(), "claude --resume {value}".into()),
        ("opencode".into(), "opencode --session {value}".into()),
        ("codex".into(), "codex resume {value}".into()),
        ("pi".into(), "pi --session {value}".into()),
        ("hermes".into(), "hermes --resume {value}".into()),
        ("kiro".into(), "kiro-cli chat --resume-id {value}".into()),
    ])
}

/// Partial on-disk shape: every field optional so `config.json` only
/// overrides what it sets. `commands` entries merge over the defaults.
#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    #[serde(default)]
    poll_seconds: Option<u64>,
    #[serde(default)]
    cooldown_seconds: Option<u64>,
    #[serde(default)]
    connect_grace_seconds: Option<u64>,
    #[serde(default)]
    commands: Option<HashMap<String, String>>,
}

fn config_file_path() -> Option<PathBuf> {
    std::env::var_os("HERDR_PLUGIN_CONFIG_DIR")
        .map(|dir| PathBuf::from(dir).join("config.json"))
}

/// Load config, merging `config.json` `commands` over the defaults.
/// Missing file, unreadable file, or invalid JSON falls back to defaults
/// (scalar overrides apply only when the file parses).
pub fn load() -> Config {
    let mut config = Config::default();
    let path = match config_file_path() {
        Some(p) => p,
        None => return config,
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return config,
    };
    let raw: RawConfig = match serde_json::from_str(&text) {
        Ok(r) => r,
        Err(_) => return config,
    };
    if let Some(v) = raw.poll_seconds {
        config.poll_seconds = v;
    }
    if let Some(v) = raw.cooldown_seconds {
        config.cooldown_seconds = v;
    }
    if let Some(v) = raw.connect_grace_seconds {
        config.connect_grace_seconds = v;
    }
    if let Some(cmds) = raw.commands {
        config.commands.extend(cmds);
    }
    config
}

/// Resolve the plugin state dir: `HERDR_PLUGIN_CONFIG_DIR`, then
/// `HERDR_PLUGIN_STATE_DIR`, else the default config-tree path.
pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("HERDR_PLUGIN_STATE_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from("~")
    });
    home.join(".config/herdr/plugins/config/quinnjr.auto-resume")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_commands_cover_owner_agents() {
        let cmds = default_commands();
        assert_eq!(cmds["claude"], "claude --resume {value}");
        assert_eq!(cmds["opencode"], "opencode --session {value}");
        assert_eq!(cmds["kiro"], "kiro-cli chat --resume-id {value}");
    }
}
