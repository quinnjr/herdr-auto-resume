use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;

pub const DEFAULT_POLL_SECONDS: u64 = 10;
pub const DEFAULT_COOLDOWN_SECONDS: u64 = 300;
pub const DEFAULT_CONNECT_GRACE_SECONDS: u64 = 120;

/// Built by [`load`] field by field; `Default` is the single source of
/// defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub poll_seconds: u64,
    pub cooldown_seconds: u64,
    pub connect_grace_seconds: u64,
    pub commands: HashMap<String, String>,
    /// Chat message submitted with every relaunch when the template
    /// carries `{message}` (e.g. kiro's positional chat input). Unset by
    /// default: templates render exactly as before until configured.
    /// Empty or whitespace-only reads as unset.
    pub resume_message: Option<String>,
}

/// `key`'s value, with an exported-but-empty variable read as unset.
/// Every env override in this crate reads through here.
pub fn env_nonempty(key: &str) -> Option<OsString> {
    std::env::var_os(key).filter(|v| !v.is_empty())
}

/// [`env_nonempty`] as a `String`; a non-UTF-8 value reads as unset.
pub fn env_string(key: &str) -> Option<String> {
    env_nonempty(key)?.into_string().ok()
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
/// known (`resume_template_key` / `resume_command`), and the `kiro` template's resume flag feeds argv
/// session recovery (`session_from_argv_with_commands`; only `--resume-id`
/// yields a value — kiro-cli's bare `--resume` is a switch). Drop the
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
        (
            "kiro".into(),
            "kiro-cli chat --resume-id {value} {message}".into(),
        ),
        ("kiro-fallback".into(), "kiro-cli chat -r {message}".into()),
    ])
}

/// `config.json` path: `$HERDR_PLUGIN_CONFIG_DIR/config.json` when the
/// env var is set (non-empty), else under the default config-tree dir
/// ([`default_dir`]). None (defaults, no file read) when neither is
/// known: a relative `~/...` path must never be read.
fn config_file_path() -> Option<PathBuf> {
    env_nonempty("HERDR_PLUGIN_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(default_dir)
        .map(|dir| dir.join("config.json"))
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
/// Missing file, unreadable file, or invalid JSON falls back to defaults.
/// Each field is extracted independently: a wrongly typed field (or
/// `commands` entry) is skipped without discarding the rest.
/// Every such problem is returned, not printed: `startup` /
/// `supervise-all` report them once per invocation (stderr and
/// `state::append_log`; `state` depends on this module, not the
/// reverse), monitors stay silent.
pub fn load() -> (Config, Vec<String>) {
    let mut config = Config::default();
    let mut warnings = Vec::new();
    let mut warn = |msg: String| warnings.push(msg);
    let Some(path) = config_file_path() else {
        return (config, warnings);
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return (config, warnings),
    };
    let raw: serde_json::Value = match serde_json::from_str(&text) {
        Ok(r) => r,
        Err(e) => {
            warn(format!(
                "invalid config JSON in {}: {e}; using defaults",
                path.display()
            ));
            return (config, warnings);
        }
    };
    let Some(obj) = raw.as_object() else {
        warn(format!(
            "config {} is not a JSON object; using defaults",
            path.display()
        ));
        return (config, warnings);
    };
    let mut bad = |field: &str, v: &serde_json::Value| {
        warn(format!(
            "config {}: ignoring {field}: {v} (wrong type)",
            path.display()
        ));
    };
    // JSON `null` reads as absent (silently) for every field.
    let get = |field: &str| obj.get(field).filter(|v| !v.is_null());
    for (field, slot) in [
        ("poll_seconds", &mut config.poll_seconds),
        ("cooldown_seconds", &mut config.cooldown_seconds),
        ("connect_grace_seconds", &mut config.connect_grace_seconds),
    ] {
        if let Some(v) = get(field) {
            match v.as_u64() {
                Some(n) => *slot = n,
                None => bad(field, v),
            }
        }
    }
    match get("commands") {
        None => {}
        Some(serde_json::Value::Object(cmds)) => {
            // Per-entry: a non-string entry is skipped on its own; a
            // `null` one keeps the default silently. `""` is a string
            // and stays the documented opt-out.
            for (agent, tpl) in cmds {
                match tpl {
                    serde_json::Value::String(t) => {
                        config.commands.insert(agent.clone(), t.clone());
                    }
                    serde_json::Value::Null => {}
                    _ => bad(&format!("commands.{agent}"), tpl),
                }
            }
        }
        Some(v) => bad("commands", v),
    }
    match get("resume_message") {
        None => {}
        // Blank would arm the two-step continue and submit a bare Enter.
        Some(serde_json::Value::String(msg)) => {
            config.resume_message = Some(msg.clone()).filter(|m| !m.trim().is_empty());
        }
        Some(v) => bad("resume_message", v),
    }
    clamp(&mut config);
    (config, warnings)
}

/// Resolve the plugin state dir: `HERDR_PLUGIN_STATE_DIR`, then
/// `HERDR_PLUGIN_CONFIG_DIR` (legacy fallback; empty reads as unset), else the default
/// config-tree path. Only mutable state (registry/locks/log/sentinels)
/// lives here; `config.json` is read from `HERDR_PLUGIN_CONFIG_DIR`
/// (else the same default dir) via `config_file_path`. None when neither
/// env var is set and `HOME` is unset or empty: fail closed rather than
/// write state under a relative `~` in the cwd.
pub fn try_state_dir() -> Option<PathBuf> {
    env_nonempty("HERDR_PLUGIN_STATE_DIR")
        .or_else(|| env_nonempty("HERDR_PLUGIN_CONFIG_DIR"))
        .map(PathBuf::from)
        .or_else(default_dir)
}

/// [`try_state_dir`], for library code. `main`'s `run()` checks
/// `try_state_dir()` before dispatching any command and exits 2 when it
/// is None, so this cannot panic in the binary.
pub fn state_dir() -> PathBuf {
    try_state_dir().expect("state dir checked by run() at startup")
}

/// The default plugin dir under `$HOME`.
const DEFAULT_SUBDIR: &str = ".config/herdr/plugins/config/quinnjr.auto-resume";

/// Default plugin dir when no env override is set:
/// `$HOME/.config/herdr/plugins/config/quinnjr.auto-resume`; None when
/// `HOME` is unset or empty.
fn default_dir() -> Option<PathBuf> {
    env_nonempty("HOME").map(|home| PathBuf::from(home).join(DEFAULT_SUBDIR))
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

    /// Save both env vars and `HOME`, run `f` under the lock, then restore
    /// (drop guard: restored even when `f` panics).
    pub fn with_saved_env(f: impl FnOnce()) {
        let _guard = lock_env();
        let restore = crate::test_support::EnvRestore::capture([
            "HERDR_PLUGIN_CONFIG_DIR",
            "HERDR_PLUGIN_STATE_DIR",
            "HOME",
        ]);
        f();
        drop(restore);
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{unique_temp_dir, with_saved_env};
    use super::*;

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
            let loaded = load().0;
            assert_eq!(loaded.poll_seconds, 42);
            assert_eq!(loaded.commands["claude"], "claude --yolo --resume {value}");
            // Defaults not overridden stay intact.
            assert_eq!(loaded.commands["opencode"], "opencode --session {value}");
            assert_eq!(
                loaded.commands["kiro"],
                "kiro-cli chat --resume-id {value} {message}"
            );
            std::fs::remove_dir_all(&dir).ok();
        });
    }

    #[test]
    fn load_returns_defaults_when_no_file() {
        with_saved_env(|| {
            let dir = unique_temp_dir();
            std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", &dir);
            std::env::remove_var("HERDR_PLUGIN_STATE_DIR");
            assert_eq!(load().0, Config::default());
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
            let home = PathBuf::from("/home/someone");
            std::env::set_var("HOME", &home);
            assert_eq!(
                state_dir(),
                home.join(".config/herdr/plugins/config/quinnjr.auto-resume")
            );
        });
    }

    #[test]
    fn state_dir_treats_empty_home_as_unset() {
        with_saved_env(|| {
            std::env::remove_var("HERDR_PLUGIN_CONFIG_DIR");
            std::env::remove_var("HERDR_PLUGIN_STATE_DIR");
            std::env::remove_var("HOME");
            assert_eq!(try_state_dir(), None);
            std::env::set_var("HOME", "");
            assert_eq!(try_state_dir(), None);
            // Either env override still resolves without HOME.
            std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", "/cfg");
            assert_eq!(try_state_dir(), Some(PathBuf::from("/cfg")));
        });
    }

    #[test]
    fn load_reads_default_path_without_config_dir_env() {
        with_saved_env(|| {
            let home = unique_temp_dir();
            std::env::set_var("HOME", &home);
            std::env::remove_var("HERDR_PLUGIN_CONFIG_DIR");
            std::env::remove_var("HERDR_PLUGIN_STATE_DIR");
            let dir = home.join(".config/herdr/plugins/config/quinnjr.auto-resume");
            std::fs::create_dir_all(&dir).expect("create default dir");
            std::fs::write(dir.join("config.json"), r#"{"poll_seconds": 7}"#)
                .expect("write config.json");
            assert_eq!(load().0.poll_seconds, 7);
            std::fs::remove_dir_all(&home).ok();
        });
    }

    #[test]
    fn load_bad_scalar_keeps_other_overrides() {
        with_saved_env(|| {
            let dir = unique_temp_dir();
            std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", &dir);
            std::env::remove_var("HERDR_PLUGIN_STATE_DIR");
            std::fs::write(
                dir.join("config.json"),
                r#"{"poll_seconds": "10", "cooldown_seconds": 60, "commands": {"claude": "claude -c {value}"}}"#,
            )
            .expect("write config.json");
            let (loaded, warnings) = load();
            assert_eq!(loaded.poll_seconds, DEFAULT_POLL_SECONDS);
            assert_eq!(loaded.cooldown_seconds, 60);
            assert_eq!(warnings.len(), 1, "{warnings:?}");
            assert!(
                warnings[0].contains("ignoring poll_seconds"),
                "{warnings:?}"
            );
            assert_eq!(loaded.commands["claude"], "claude -c {value}");
            std::fs::remove_dir_all(&dir).ok();
        });
    }

    #[test]
    fn load_bad_command_entry_keeps_others() {
        with_saved_env(|| {
            let dir = unique_temp_dir();
            std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", &dir);
            std::env::remove_var("HERDR_PLUGIN_STATE_DIR");
            std::fs::write(
                dir.join("config.json"),
                r#"{"commands": {"kiro-fallback": null, "codex": 5, "claude": "claude -c {value}", "pi": ""}}"#,
            )
            .expect("write config.json");
            let loaded = load().0;
            assert_eq!(loaded.commands["claude"], "claude -c {value}");
            // Bad entries keep their defaults; "" stays the opt-out.
            assert_eq!(
                loaded.commands["kiro-fallback"],
                "kiro-cli chat -r {message}"
            );
            assert_eq!(loaded.commands["codex"], "codex resume {value}");
            assert_eq!(loaded.commands["pi"], "");
            std::fs::remove_dir_all(&dir).ok();
        });
    }

    #[test]
    fn load_treats_null_fields_as_absent() {
        with_saved_env(|| {
            let dir = unique_temp_dir();
            std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", &dir);
            std::env::remove_var("HERDR_PLUGIN_STATE_DIR");
            std::fs::write(
                dir.join("config.json"),
                r#"{"poll_seconds": null, "cooldown_seconds": null, "connect_grace_seconds": null, "resume_message": null, "commands": null}"#,
            )
            .expect("write config.json");
            let loaded = load().0;
            let defaults = Config::default();
            assert_eq!(loaded.poll_seconds, defaults.poll_seconds);
            assert_eq!(loaded.cooldown_seconds, defaults.cooldown_seconds);
            assert_eq!(loaded.connect_grace_seconds, defaults.connect_grace_seconds);
            assert_eq!(loaded.resume_message, None);
            assert_eq!(loaded.commands, defaults.commands);
            // A null `commands` entry keeps its default; "" opts out.
            std::fs::write(
                dir.join("config.json"),
                r#"{"poll_seconds": 7, "commands": {"codex": null, "pi": ""}}"#,
            )
            .expect("write config.json");
            let loaded = load().0;
            assert_eq!(loaded.poll_seconds, 7);
            assert_eq!(loaded.commands["codex"], "codex resume {value}");
            assert_eq!(loaded.commands["pi"], "");
            std::fs::remove_dir_all(&dir).ok();
        });
    }

    #[test]
    fn load_treats_blank_resume_message_as_unset() {
        with_saved_env(|| {
            let dir = unique_temp_dir();
            std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", &dir);
            std::env::remove_var("HERDR_PLUGIN_STATE_DIR");
            for blank in ["", "  \t\n "] {
                let json = serde_json::json!({ "resume_message": blank });
                std::fs::write(dir.join("config.json"), json.to_string()).expect("write");
                assert_eq!(load().0.resume_message, None, "{blank:?}");
            }
            std::fs::write(dir.join("config.json"), r#"{"resume_message": " go "}"#)
                .expect("write");
            assert_eq!(load().0.resume_message.as_deref(), Some(" go "));
            std::fs::remove_dir_all(&dir).ok();
        });
    }

    #[test]
    fn no_home_reads_no_config() {
        with_saved_env(|| {
            std::env::remove_var("HERDR_PLUGIN_CONFIG_DIR");
            for home in [None, Some("")] {
                match home {
                    Some(h) => std::env::set_var("HOME", h),
                    None => std::env::remove_var("HOME"),
                }
                assert_eq!(config_file_path(), None, "HOME {home:?}");
                assert_eq!(load(), (Config::default(), vec![]));
            }
        });
    }

    #[test]
    fn empty_dir_env_vars_read_as_unset() {
        with_saved_env(|| {
            let home = unique_temp_dir();
            std::env::set_var("HOME", &home);
            std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", "");
            std::env::set_var("HERDR_PLUGIN_STATE_DIR", "");
            let default = home.join(".config/herdr/plugins/config/quinnjr.auto-resume");
            assert_eq!(state_dir(), default);
            assert_eq!(config_file_path(), Some(default.join("config.json")));
            // An empty STATE_DIR falls through to a set CONFIG_DIR.
            let config_dir = unique_temp_dir();
            std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", &config_dir);
            assert_eq!(state_dir(), config_dir);
            std::fs::remove_dir_all(&home).ok();
            std::fs::remove_dir_all(&config_dir).ok();
        });
    }
}
