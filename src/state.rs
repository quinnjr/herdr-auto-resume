use std::collections::HashMap;
use std::path::PathBuf;

use crate::config;
use crate::herdr::SessionRef;

fn registry_path() -> PathBuf {
    config::state_dir().join("registry.json")
}

fn monitors_dir() -> PathBuf {
    config::state_dir().join("monitors")
}

fn log_path() -> PathBuf {
    config::state_dir().join("log.txt")
}

/// Load the durable pane-id -> session registry. Missing file, unreadable
/// file, or invalid JSON yields an empty map.
pub fn load_registry() -> HashMap<String, SessionRef> {
    let text = match std::fs::read_to_string(registry_path()) {
        Ok(t) => t,
        Err(_) => return HashMap::new(),
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// Persist the registry atomically via `registry.json.tmp` + rename.
pub fn save_registry(reg: &HashMap<String, SessionRef>) {
    let dir = config::state_dir();
    std::fs::create_dir_all(&dir).ok();
    let text = serde_json::to_string(reg).unwrap_or_else(|_| "{}".to_string());
    let tmp = dir.join("registry.json.tmp");
    if std::fs::write(&tmp, text).is_err() {
        return;
    }
    std::fs::rename(&tmp, registry_path()).ok();
}

/// Record (or overwrite) the session for a pane, persisting to disk.
pub fn remember(pane_id: &str, session: SessionRef) {
    let mut reg = load_registry();
    reg.insert(pane_id.to_string(), session);
    save_registry(&reg);
}

/// Lock-file path for a pane's monitor; `:` is not filename-safe on all
/// platforms, so it maps to `_` (e.g. `w7G:p1` -> `monitors/w7G_p1.json`).
pub fn monitor_lock_path(pane_id: &str) -> PathBuf {
    monitors_dir().join(format!("{}.json", pane_id.replace(':', "_")))
}

/// Write this process's monitor lock for a pane.
pub fn write_monitor_lock(pane_id: &str) {
    write_monitor_lock_pid(pane_id, std::process::id());
}

fn write_monitor_lock_pid(pane_id: &str, pid: u32) {
    let path = monitor_lock_path(pane_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let text = serde_json::json!({ "pid": pid }).to_string();
    std::fs::write(&path, text).ok();
}

/// Remove a pane's monitor lock. Best-effort.
pub fn clear_monitor_lock(pane_id: &str) {
    std::fs::remove_file(monitor_lock_path(pane_id)).ok();
}

fn lock_pid(pane_id: &str) -> Option<u32> {
    let text = std::fs::read_to_string(monitor_lock_path(pane_id)).ok()?;
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()?
        .get("pid")?
        .as_u64()
        .map(|p| p as u32)
}

/// PID of the live monitor for a pane, or `None` when no lock exists, the
/// lock is unreadable, or the recorded process is no longer our monitor
/// (guards against PID reuse).
///
/// Liveness check without new deps: on Linux read `/proc/<pid>/cmdline`
/// and require our own binary name in it; elsewhere fall back to
/// `kill -0 <pid>` via `std::process::Command`.
pub fn live_monitor_pid(pane_id: &str) -> Option<u32> {
    let pid = lock_pid(pane_id)?;
    if pid_is_live_monitor(pid) {
        Some(pid)
    } else {
        None
    }
}

fn our_binary_marker() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "auto-resume".to_string())
}

fn pid_is_live_monitor(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok();
        match cmdline {
            None => false,
            Some(bytes) => {
                let flat = String::from_utf8_lossy(&bytes).replace('\0', " ");
                flat.contains(&our_binary_marker())
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

/// Append a timestamped line to `log.txt`. Best-effort.
pub fn append_log(line: &str) {
    let dir = config::state_dir();
    std::fs::create_dir_all(&dir).ok();
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let entry = format!("[{secs}] {line}\n");
    use std::io::Write;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
        .and_then(|mut f| f.write_all(entry.as_bytes()))
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Serializes the state tests: they all mutate the process-global
    /// `HERDR_PLUGIN_CONFIG_DIR`, so they must never run concurrently.
    /// (Poison-tolerant: a failed test must not wedge the rest.)
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn unique_temp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "auto-resume-state-test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
            n
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Run `f` with `HERDR_PLUGIN_CONFIG_DIR` pointed at a fresh temp dir,
    /// restoring the previous value afterwards (env mutation is
    /// process-global). Returns the temp dir for post-assertions.
    fn with_temp_state_dir(f: impl FnOnce(&PathBuf)) -> PathBuf {
        let _guard = lock_env();
        let prev = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR");
        let dir = unique_temp_dir();
        std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", &dir);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&dir)));
        match prev {
            Some(v) => std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", v),
            None => std::env::remove_var("HERDR_PLUGIN_CONFIG_DIR"),
        }
        assert!(result.is_ok());
        dir
    }

    #[test]
    fn registry_round_trips() {
        let _guard = lock_env();
        let prev = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR");
        let dir = unique_temp_dir();
        std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", &dir);
        let result = std::panic::catch_unwind(|| {
            remember(
                "w7G:p1",
                SessionRef {
                    agent: "kiro".into(),
                    value: "sess-1".into(),
                },
            );
            let reg = load_registry();
            assert_eq!(reg["w7G:p1"].value, "sess-1");
        });
        match prev {
            Some(v) => std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", v),
            None => std::env::remove_var("HERDR_PLUGIN_CONFIG_DIR"),
        }
        std::fs::remove_dir_all(&dir).ok();
        assert!(result.is_ok());
    }

    #[test]
    fn monitor_lock_names_sanitize_colon() {
        let dir = with_temp_state_dir(|_| {
            let p = monitor_lock_path("w7G:p1");
            assert_eq!(p.file_name().unwrap().to_string_lossy(), "w7G_p1.json");
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn live_monitor_pid_rejects_dead_pid() {
        let dir = with_temp_state_dir(|_| {
            // Record a PID that almost certainly does not exist.
            write_monitor_lock_pid("w7G:p1", 2147483647);
            assert_eq!(live_monitor_pid("w7G:p1"), None);
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn live_monitor_pid_accepts_current_process() {
        let dir = with_temp_state_dir(|_| {
            // Current test process runs the `auto-resume` test binary, so
            // its cmdline contains our binary marker.
            write_monitor_lock_pid("w7G:p1", std::process::id());
            assert_eq!(live_monitor_pid("w7G:p1"), Some(std::process::id()));
            clear_monitor_lock("w7G:p1");
            assert_eq!(live_monitor_pid("w7G:p1"), None);
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn append_log_writes_timestamped_line() {
        let dir = with_temp_state_dir(|dir| {
            append_log("hello");
            let text = std::fs::read_to_string(dir.join("log.txt")).expect("log exists");
            assert!(text.contains("hello"));
            assert!(text.starts_with('['));
        });
        std::fs::remove_dir_all(&dir).ok();
    }
}
