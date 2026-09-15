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
    // pane_id is embedded so the liveness check can verify the process
    // is the monitor for THIS pane (guards against PID reuse by a
    // monitor for another pane or an unrelated same-named binary).
    let text = serde_json::json!({ "pid": pid, "pane_id": pane_id }).to_string();
    std::fs::write(&path, text).ok();
}

/// Remove a pane's monitor lock. Best-effort.
pub fn clear_monitor_lock(pane_id: &str) {
    std::fs::remove_file(monitor_lock_path(pane_id)).ok();
}

fn lock_record(pane_id: &str) -> Option<(u32, Option<String>)> {
    let text = std::fs::read_to_string(monitor_lock_path(pane_id)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let pid = v.get("pid")?.as_u64().map(|p| p as u32)?;
    let locked_pane = v
        .get("pane_id")
        .and_then(|p| p.as_str())
        .map(str::to_string);
    Some((pid, locked_pane))
}

/// PID of the live monitor for a pane, or `None` when no lock exists, the
/// lock is unreadable, the lock names a different pane, or the recorded
/// process is not the monitor for this pane (guards against PID reuse:
/// monitors are spawned as `auto-resume monitor <pane-id>`, so the pane
/// id must appear as an argv token in the process cmdline).
pub fn live_monitor_pid(pane_id: &str) -> Option<u32> {
    let (pid, locked_pane) = lock_record(pane_id)?;
    if let Some(locked) = locked_pane {
        if locked != pane_id {
            return None;
        }
    }
    if pid_is_monitor_for_pane(pid, pane_id) {
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

/// True when a raw cmdline (NUL- or space-separated) belongs to the
/// monitor for `pane_id`: it names our binary and carries the pane id as
/// an exact argv token (exact, so `w7G:p1` never matches `w7G:p10`).
/// Pure for testability; `pid_is_monitor_for_pane` feeds it `/proc` data.
fn cmdline_is_monitor_for_pane(cmdline: &str, pane_id: &str) -> bool {
    if !cmdline.contains(&our_binary_marker()) {
        return false;
    }
    cmdline
        .split(|c| c == '\0' || c == ' ')
        .any(|tok| tok == pane_id)
}

fn pid_is_monitor_for_pane(pid: u32, pane_id: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok();
        match cmdline {
            None => false,
            Some(bytes) => {
                let flat = String::from_utf8_lossy(&bytes);
                cmdline_is_monitor_for_pane(&flat, pane_id)
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // No /proc: cannot verify argv; fall back to existence check.
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

/// Path of the stop-sentinel for a monitor pid. Task 6's `stop` creates
/// `stop-<pid>`; the monitor loop checks it each poll and exits.
pub fn stop_sentinel_path(pid: u32) -> PathBuf {
    config::state_dir().join(format!("stop-{pid}"))
}

/// True when a stop was requested for this monitor process.
pub fn stop_requested(pid: u32) -> bool {
    stop_sentinel_path(pid).exists()
}

/// Remove a monitor's stop-sentinel. Best-effort.
pub fn clear_stop_sentinel(pid: u32) {
    std::fs::remove_file(stop_sentinel_path(pid)).ok();
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
    fn monitor_lock_embeds_pane_id() {
        let dir = with_temp_state_dir(|dir| {
            write_monitor_lock_pid("w7G:p1", 12345);
            let text =
                std::fs::read_to_string(dir.join("monitors/w7G_p1.json")).expect("lock exists");
            let v: serde_json::Value = serde_json::from_str(&text).expect("valid json");
            assert_eq!(v["pane_id"], "w7G:p1");
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    fn fake_monitor_cmdline(pane_id: &str) -> String {
        format!("{}\0monitor\0{pane_id}\0", our_binary_marker())
    }

    #[test]
    fn cmdline_matches_own_pane_only() {
        // Own pane's monitor invocation matches.
        assert!(cmdline_is_monitor_for_pane(
            &fake_monitor_cmdline("w7G:p1"),
            "w7G:p1"
        ));
        // Another pane's monitor does not (PID-reuse guard).
        assert!(!cmdline_is_monitor_for_pane(
            &fake_monitor_cmdline("w7G:p2"),
            "w7G:p1"
        ));
        // Prefix pane ids never match (exact argv token).
        assert!(!cmdline_is_monitor_for_pane(
            &fake_monitor_cmdline("w7G:p10"),
            "w7G:p1"
        ));
        // Unrelated binary with the pane id on its cmdline does not.
        assert!(!cmdline_is_monitor_for_pane("other-bin\0w7G:p1\0", "w7G:p1"));
    }

    #[test]
    fn live_monitor_pid_rejects_monitor_for_other_pane() {
        let dir = with_temp_state_dir(|_| {
            // Our own process is not a `monitor w7G:p1` invocation, so
            // even our live pid is rejected for that pane now.
            write_monitor_lock_pid("w7G:p1", std::process::id());
            assert_eq!(live_monitor_pid("w7G:p1"), None);
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
