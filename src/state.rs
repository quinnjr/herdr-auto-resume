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
/// Skips the write when the stored value is unchanged (monitors call this
/// every poll while the agent is alive).
pub fn remember(pane_id: &str, session: SessionRef) {
    let mut reg = load_registry();
    if reg.get(pane_id) == Some(&session) {
        return;
    }
    reg.insert(pane_id.to_string(), session);
    save_registry(&reg);
}

/// Pane ids accepted for lock-file handling: nonempty, at most 128
/// bytes, ASCII alphanumeric plus `:`, `_`, `-`. Anything else fails
/// closed (callers return `None`/`false` and log).
pub fn valid_pane_id(pane_id: &str) -> bool {
    !pane_id.is_empty()
        && pane_id.len() <= 128
        && pane_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == ':' || c == '_' || c == '-')
}

/// Injective filename encoding for pane ids: `_` -> `__u__`,
/// `:` -> `__c__`, every other byte outside `[A-Za-z0-9-]` escaped as
/// `__xHH__` hex. The output alphabet is filename-safe (no `/`, `.`,
/// or NUL can appear), so even an unvalidated id cannot traverse out
/// of the monitors dir; valid ids additionally round-trip uniquely
/// (`a:b` and `a_b` map to different names). Callers still gate on
/// `valid_pane_id` first and fail closed on invalid ids.
fn encode_pane_id(pane_id: &str) -> String {
    let mut out = String::with_capacity(pane_id.len());
    for b in pane_id.bytes() {
        match b {
            b'_' => out.push_str("__u__"),
            b':' => out.push_str("__c__"),
            b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' | b'-' => out.push(b as char),
            _ => out.push_str(&format!("__x{b:02x}__")),
        }
    }
    out
}

/// Lock-file path for a pane's monitor, via the injective
/// [`encode_pane_id`] mapping (e.g. `w7G:p1` -> `monitors/w7G__c__p1.json`).
pub fn monitor_lock_path(pane_id: &str) -> PathBuf {
    monitors_dir().join(format!("{}.json", encode_pane_id(pane_id)))
}

/// Write this process's monitor lock for a pane. Returns true when we
/// hold the claim. Same signature as before so existing call sites
/// compile unchanged; the boolean is simply ignored by fire-and-forget
/// callers.
pub fn write_monitor_lock(pane_id: &str) -> bool {
    write_monitor_lock_pid(pane_id, std::process::id())
}

/// Claim a pane's monitor lock for `pid` atomically: `create_new`
/// wins the race; on `AlreadyExists` a live rival keeps its claim
/// (return false without overwriting) while a stale lock is reclaimed
/// (overwrite, return true). True = we hold the claim.
pub fn write_monitor_lock_pid(pane_id: &str, pid: u32) -> bool {
    if !valid_pane_id(pane_id) {
        eprintln!("write_monitor_lock: rejected invalid pane id {pane_id:?}");
        return false;
    }
    let path = monitor_lock_path(pane_id);
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            eprintln!(
                "write_monitor_lock: cannot create dir {}",
                parent.display()
            );
            return false;
        }
    }
    // pane_id is embedded so the liveness check can verify the process
    // is the monitor for THIS pane (guards against PID reuse by a
    // monitor for another pane or an unrelated same-named binary).
    let text = serde_json::json!({ "pid": pid, "pane_id": pane_id }).to_string();
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut f) => {
            use std::io::Write;
            if let Err(e) = f.write_all(text.as_bytes()) {
                eprintln!("write_monitor_lock: write {} failed: {e}", path.display());
                return false;
            }
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            match live_monitor_pid(pane_id) {
                Some(_) => false,
                None => {
                    eprintln!("write_monitor_lock: reclaiming stale lock for {pane_id:?}");
                    if let Err(e) = std::fs::write(&path, text) {
                        eprintln!(
                            "write_monitor_lock: reclaim {} failed: {e}",
                            path.display()
                        );
                        return false;
                    }
                    true
                }
            }
        }
        Err(e) => {
            eprintln!(
                "write_monitor_lock: create {} failed: {e}",
                path.display()
            );
            false
        }
    }
}

/// Remove a pane's monitor lock. Best-effort; invalid ids are rejected
/// without touching the filesystem.
pub fn clear_monitor_lock(pane_id: &str) {
    if !valid_pane_id(pane_id) {
        eprintln!("clear_monitor_lock: rejected invalid pane id {pane_id:?}");
        return;
    }
    std::fs::remove_file(monitor_lock_path(pane_id)).ok();
}

/// Parse one lock file's contents: `Ok((pane_id, pid))`, or `Err(kind)`
/// with a short machine-readable reason (`invalid-json`, `missing-pid`,
/// `missing-pane-id`).
fn parse_lock_text(text: &str) -> Result<(String, u32), &'static str> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|_| "invalid-json")?;
    let pid = v
        .get("pid")
        .and_then(|p| p.as_u64())
        .map(|p| p as u32)
        .ok_or("missing-pid")?;
    let pane_id = v
        .get("pane_id")
        .and_then(|p| p.as_str())
        .map(str::to_string)
        .ok_or("missing-pane-id")?;
    Ok((pane_id, pid))
}

/// All recorded `(pane_id, pid)` monitor locks (live or stale), read
/// from the lock files' embedded JSON. Used by `status`/`stop`.
/// Corrupt or unreadable files are skipped with an `eprintln` naming
/// the path and the reason (see [`unreadable_locks`]).
pub fn monitor_locks() -> Vec<(String, u32)> {
    let dir = monitors_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!(
                    "monitor_locks: skipping {}: unreadable ({e})",
                    path.display()
                );
                continue;
            }
        };
        match parse_lock_text(&text) {
            Ok((pane_id, pid)) => out.push((pane_id, pid)),
            Err(kind) => {
                eprintln!("monitor_locks: skipping {}: {kind}", path.display());
            }
        }
    }
    out.sort();
    out
}

/// File names (not paths) of monitor lock files that exist but cannot
/// be used: unreadable or corrupt JSON / missing fields. Exposed so
/// `status` can surface them; [`monitor_locks`] already logs each skip
/// to stderr when it encounters them.
pub fn unreadable_locks() -> Vec<String> {
    let dir = monitors_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let usable = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| parse_lock_text(&text).ok())
            .is_some();
        if !usable {
            out.push(path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            ));
        }
    }
    out.sort();
    out
}

fn lock_record(pane_id: &str) -> Option<(u32, Option<String>)> {
    if !valid_pane_id(pane_id) {
        eprintln!("live_monitor_pid: rejected invalid pane id {pane_id:?}");
        return None;
    }
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
/// monitor for `pane_id`: it names our binary and carries BOTH
/// `monitor` and the pane id as exact argv tokens (exact, so `w7G:p1`
/// never matches `w7G:p10`; the `monitor` token keeps e.g. a
/// `hook-pane` invocation for the same pane from matching).
/// Pure for testability; `pid_is_monitor_for_pane` feeds it `/proc` data.
fn cmdline_is_monitor_for_pane(cmdline: &str, pane_id: &str) -> bool {
    if !cmdline.contains(&our_binary_marker()) {
        return false;
    }
    let mut has_monitor = false;
    let mut has_pane = false;
    for tok in cmdline.split(|c| c == '\0' || c == ' ') {
        if tok == "monitor" {
            has_monitor = true;
        }
        if tok == pane_id {
            has_pane = true;
        }
    }
    has_monitor && has_pane
}

/// Pure predicate over `/bin/ps -p <pid> -o command=` output: true when
/// it describes the monitor for `pane_id`. The thin `ps` wrapper below
/// feeds it live output; unit tests cover this function directly (no
/// live processes needed).
fn ps_output_is_monitor(output: &str, pane_id: &str) -> bool {
    cmdline_is_monitor_for_pane(output, pane_id)
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
        // No /proc: verify argv via ps instead of a bare existence check
        // (kill -0 cannot tell a monitor for this pane from PID reuse).
        let out = std::process::Command::new("/bin/ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                ps_output_is_monitor(&String::from_utf8_lossy(&o.stdout), pane_id)
            }
            _ => false,
        }
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
        let dir = with_temp_state_dir(|_| {
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
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn remember_skips_write_when_value_unchanged() {
        let dir = with_temp_state_dir(|dir| {
            let reg_path = dir.join("registry.json");
            let sess = SessionRef {
                agent: "kiro".into(),
                value: "sess-1".into(),
            };
            remember("w7G:p1", sess.clone());
            let mtime_before = std::fs::metadata(&reg_path)
                .expect("registry exists")
                .modified()
                .expect("mtime");
            std::thread::sleep(std::time::Duration::from_millis(10));
            remember("w7G:p1", sess);
            let mtime_after = std::fs::metadata(&reg_path)
                .expect("registry exists")
                .modified()
                .expect("mtime");
            assert_eq!(
                mtime_before, mtime_after,
                "unchanged value must not rewrite registry"
            );
            // A changed value still writes.
            std::thread::sleep(std::time::Duration::from_millis(10));
            remember(
                "w7G:p1",
                SessionRef {
                    agent: "kiro".into(),
                    value: "sess-2".into(),
                },
            );
            let mtime_changed = std::fs::metadata(&reg_path)
                .expect("registry exists")
                .modified()
                .expect("mtime");
            assert!(
                mtime_changed > mtime_after,
                "changed value must rewrite registry"
            );
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn monitor_lock_names_sanitize_colon() {
        let dir = with_temp_state_dir(|_| {
            let p = monitor_lock_path("w7G:p1");
            assert_eq!(
                p.file_name().unwrap().to_string_lossy(),
                "w7G__c__p1.json"
            );
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
            let text = std::fs::read_to_string(dir.join("monitors/w7G__c__p1.json"))
                .expect("lock exists");
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
    fn valid_pane_id_accepts_normal_rejects_weird() {
        assert!(valid_pane_id("w7G:p1"));
        assert!(valid_pane_id("a-b_c:d"));
        assert!(!valid_pane_id(""));
        assert!(!valid_pane_id("../../evil"));
        assert!(!valid_pane_id("a/b"));
        assert!(!valid_pane_id("a b"));
        assert!(!valid_pane_id("w7G*p1"));
        assert!(valid_pane_id(&"x".repeat(128)));
        assert!(!valid_pane_id(&"x".repeat(129)));
    }

    #[test]
    fn traversal_pane_id_rejected() {
        let dir = with_temp_state_dir(|dir| {
            assert!(!write_monitor_lock("../../evil"));
            assert_eq!(live_monitor_pid("../../evil"), None);
            // Clear of an invalid id is a no-op (must not panic).
            clear_monitor_lock("../../evil");
            // Nothing escaped the state dir.
            assert!(std::fs::read_dir(dir)
                .expect("state dir")
                .find(|e| e.as_ref().is_ok_and(|e| e.file_name() == *"evil"))
                .is_none());
            // Even a direct path mapping stays inside the monitors dir.
            let p = monitor_lock_path("../../evil");
            assert_eq!(
                p.parent().map(|p| p.to_path_buf()),
                Some(dir.join("monitors"))
            );
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn colon_and_underscore_map_to_different_files() {
        let dir = with_temp_state_dir(|_| {
            let a = monitor_lock_path("a:b");
            let b = monitor_lock_path("a_b");
            assert_ne!(a, b);
            assert_eq!(a.file_name().unwrap().to_string_lossy(), "a__c__b.json");
            assert_eq!(b.file_name().unwrap().to_string_lossy(), "a__u__b.json");
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn monitor_lock_claim_reclaims_stale() {
        let dir = with_temp_state_dir(|_| {
            // No existing lock: claim succeeds.
            assert!(write_monitor_lock_pid("w7G:p1", 2147483647));
            // Existing lock names a dead pid: stale, so reclaim succeeds
            // and the file now carries the new pid.
            assert!(write_monitor_lock_pid("w7G:p1", 2147483646));
            assert_eq!(
                monitor_locks(),
                vec![("w7G:p1".to_string(), 2147483646)]
            );
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn monitor_locks_lists_written_locks() {
        let dir = with_temp_state_dir(|_| {
            write_monitor_lock_pid("w7G:p1", 111);
            write_monitor_lock_pid("w7G:p2", 222);
            assert_eq!(
                monitor_locks(),
                vec![
                    ("w7G:p1".to_string(), 111),
                    ("w7G:p2".to_string(), 222),
                ]
            );
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unreadable_locks_reports_corrupt_files() {
        let dir = with_temp_state_dir(|dir| {
            write_monitor_lock_pid("w7G:p1", 111);
            std::fs::write(dir.join("monitors/corrupt.json"), "{oops")
                .expect("write corrupt lock");
            assert!(unreadable_locks().iter().any(|n| n == "corrupt.json"));
            // Corrupt entry is skipped; the good one is still listed.
            assert_eq!(monitor_locks(), vec![("w7G:p1".to_string(), 111)]);
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stop_sentinel_round_trips() {
        let dir = with_temp_state_dir(|_| {
            let pid = 424242u32;
            assert!(!stop_requested(pid));
            std::fs::write(stop_sentinel_path(pid), b"stop").expect("sentinel");
            assert!(stop_requested(pid));
            clear_stop_sentinel(pid);
            assert!(!stop_requested(pid));
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_registry_returns_empty_on_corrupt_json() {
        let dir = with_temp_state_dir(|dir| {
            std::fs::write(dir.join("registry.json"), "{oops").expect("corrupt");
            assert!(load_registry().is_empty());
            std::fs::write(dir.join("registry.json"), "[1,2]").expect("wrong shape");
            assert!(load_registry().is_empty());
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cmdline_requires_monitor_token() {
        let m = our_binary_marker();
        assert!(!cmdline_is_monitor_for_pane(
            &format!("{m} hook-pane w7G:p1"),
            "w7G:p1"
        ));
        assert!(cmdline_is_monitor_for_pane(
            &format!("{m} monitor w7G:p1"),
            "w7G:p1"
        ));
    }

    #[test]
    fn ps_output_detects_monitor() {
        let m = our_binary_marker();
        assert!(ps_output_is_monitor(
            &format!("{m} monitor w7G:p1"),
            "w7G:p1"
        ));
        assert!(!ps_output_is_monitor(
            &format!("{m} hook-pane w7G:p1"),
            "w7G:p1"
        ));
        assert!(!ps_output_is_monitor(
            &format!("{m} monitor w7G:p2"),
            "w7G:p1"
        ));
        assert!(!ps_output_is_monitor("", "w7G:p1"));
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
