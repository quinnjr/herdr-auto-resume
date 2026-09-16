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
    /// Chat message submitted with every relaunch when the template
    /// carries `{message}` (e.g. kiro's positional chat input). Unset by
    /// default: templates render exactly as before until configured.
    #[serde(default)]
    pub resume_message: Option<String>,
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
            resume_message: None,
        }
    }
}

/// Default resume templates. `{value}` marks session-valued templates.
///
/// Sync debt: `kiro` and `kiro-fallback` must stay in sync with
/// `resume.rs` — `<agent>-fallback` is preferred when no session value is
/// known (`resume_argv`), and the `kiro` template's resume flag feeds argv
/// session recovery (`session_from_argv_with_commands`, which also accepts
/// the live-CLI `--resume` spelling alongside `--resume-id`). Drop the
/// `-r` (`kiro-cli chat -r`) alt spelling only when kiro-cli removes it or
/// the valued `kiro` template always resolves (making the valueless path
/// dead); per-user opt-out is `"kiro-fallback": ""` in `config.json`.
pub fn default_commands() -> HashMap<String, String> {
    HashMap::from([
        ("claude".into(), "claude --resume {value}".into()),
        ("opencode".into(), "opencode --session {value}".into()),
        ("codex".into(), "codex resume {value}".into()),
        ("pi".into(), "pi --session {value}".into()),
        ("hermes".into(), "hermes --resume {value}".into()),
        ("kiro".into(), "kiro-cli chat --resume-id {value} {message}".into()),
        ("kiro-fallback".into(), "kiro-cli chat -r {message}".into()),
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
    #[serde(default)]
    resume_message: Option<String>,
}

fn config_file_path() -> Option<PathBuf> {
    std::env::var_os("HERDR_PLUGIN_CONFIG_DIR")
        .map(|dir| PathBuf::from(dir).join("config.json"))
}

/// Clamp loaded values into sane bounds: a zero poll would busy-loop
/// the monitor, an unbounded grace would stall its start, and an absurd
/// cooldown would panic on `Instant + Duration`.
fn clamp(config: &mut Config) {
    if config.poll_seconds < 1 {
        config.poll_seconds = 1;
    }
    if config.cooldown_seconds > 86_400 {
        config.cooldown_seconds = 86_400;
    }
    if config.connect_grace_seconds > 3600 {
        config.connect_grace_seconds = 3600;
    }
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
        Err(e) => {
            eprintln!(
                "auto-resume: invalid config JSON in {}: {e}; using defaults",
                path.display()
            );
            return config;
        }
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
    if let Some(msg) = raw.resume_message {
        config.resume_message = Some(msg);
    }
    clamp(&mut config);
    config
}

/// Resolve the plugin state dir: `HERDR_PLUGIN_STATE_DIR`, then
/// `HERDR_PLUGIN_CONFIG_DIR` (legacy fallback), else the default
/// config-tree path. Only mutable state (registry/locks/log/sentinels)
/// lives here; `config.json` is always read from
/// `HERDR_PLUGIN_CONFIG_DIR` via `config_file_path`.
pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("HERDR_PLUGIN_STATE_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from("~")
    });
    home.join(".config/herdr/plugins/config/quinnjr.auto-resume")
}

/// Shared helpers for this module's env-mutating tests. The lock and temp
/// dir come from the crate-wide `crate::test_support` (one lock serializes
/// ALL modules' env mutation); `with_saved_env` keeps this module's
/// save/restore-everything semantics.
#[cfg(test)]
mod test_support {
    use std::path::PathBuf;

    pub fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::lock_env()
    }

    pub fn unique_temp_dir() -> PathBuf {
        crate::test_support::unique_temp_dir("config-test")
    }

    /// Save both env vars, run `f` under the lock, then restore.
    pub fn with_saved_env(f: impl FnOnce()) {
        let _guard = lock_env();
        let prev_config = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR");
        let prev_state = std::env::var_os("HERDR_PLUGIN_STATE_DIR");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        match prev_config {
            Some(v) => std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", v),
            None => std::env::remove_var("HERDR_PLUGIN_CONFIG_DIR"),
        }
        match prev_state {
            Some(v) => std::env::set_var("HERDR_PLUGIN_STATE_DIR", v),
            None => std::env::remove_var("HERDR_PLUGIN_STATE_DIR"),
        }
        assert!(result.is_ok());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::test_support::{unique_temp_dir, with_saved_env};

    #[test]
    fn load_clamps_poll_and_grace() {
        let mut c = Config {
            poll_seconds: 0,
            cooldown_seconds: 300,
            connect_grace_seconds: 99_999,
            commands: default_commands(),
            resume_message: None,
        };
        clamp(&mut c);
        assert_eq!(c.poll_seconds, 1);
        assert_eq!(c.connect_grace_seconds, 3600);
        // Sane values pass through untouched.
        let mut sane = Config::default();
        clamp(&mut sane);
        assert_eq!(sane, Config::default());
    }

    #[test]
    fn clamp_caps_cooldown() {
        let mut c = Config {
            cooldown_seconds: u64::MAX,
            ..Config::default()
        };
        clamp(&mut c);
        assert_eq!(c.cooldown_seconds, 86_400);
    }

    #[test]
    fn default_commands_cover_owner_agents() {
        let cmds = default_commands();
        assert_eq!(cmds["claude"], "claude --resume {value}");
        assert_eq!(cmds["opencode"], "opencode --session {value}");
        assert_eq!(cmds["kiro"], "kiro-cli chat --resume-id {value} {message}");
        assert_eq!(cmds["kiro-fallback"], "kiro-cli chat -r {message}");
    }

    #[test]
    fn load_merges_commands_over_defaults() {
        with_saved_env(|| {
            let dir = unique_temp_dir();
            std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", &dir);
            std::env::remove_var("HERDR_PLUGIN_STATE_DIR");
            std::fs::write(
                dir.join("config.json"),
                r#"{"poll_seconds": 42, "commands": {"claude": "claude --yolo --resume {value}"}}"#,
            )
            .expect("write config.json");
            let loaded = load();
            assert_eq!(loaded.poll_seconds, 42);
            assert_eq!(loaded.commands["claude"], "claude --yolo --resume {value}");
            // Defaults not overridden stay intact.
            assert_eq!(loaded.commands["opencode"], "opencode --session {value}");
            assert_eq!(loaded.commands["kiro"], "kiro-cli chat --resume-id {value} {message}");
            std::fs::remove_dir_all(&dir).ok();
        });
    }

    #[test]
    fn load_returns_defaults_when_no_file() {
        with_saved_env(|| {
            let dir = unique_temp_dir();
            std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", &dir);
            std::env::remove_var("HERDR_PLUGIN_STATE_DIR");
            assert_eq!(load(), Config::default());
            std::fs::remove_dir_all(&dir).ok();
        });
    }

    #[test]
    fn state_dir_prefers_state_dir_over_config_dir() {
        with_saved_env(|| {
            let config_dir = unique_temp_dir();
            let state_dir_path = unique_temp_dir();
            std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", &config_dir);
            std::env::set_var("HERDR_PLUGIN_STATE_DIR", &state_dir_path);
            assert_eq!(state_dir(), state_dir_path);
            std::fs::remove_dir_all(&config_dir).ok();
            std::fs::remove_dir_all(&state_dir_path).ok();
        });
    }

    #[test]
    fn state_dir_falls_back_to_config_dir() {
        with_saved_env(|| {
            let config_dir = unique_temp_dir();
            std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", &config_dir);
            std::env::remove_var("HERDR_PLUGIN_STATE_DIR");
            assert_eq!(state_dir(), config_dir);
            std::fs::remove_dir_all(&config_dir).ok();
        });
    }

    #[test]
    fn state_dir_defaults_under_home() {
        with_saved_env(|| {
            std::env::remove_var("HERDR_PLUGIN_CONFIG_DIR");
            std::env::remove_var("HERDR_PLUGIN_STATE_DIR");
            let home =
                std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| {
                    PathBuf::from("~")
                });
            assert_eq!(
                state_dir(),
                home.join(".config/herdr/plugins/config/quinnjr.auto-resume")
            );
        });
    }
}
