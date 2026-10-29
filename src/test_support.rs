//! Shared helpers for env-mutating tests.
//!
//! Every test that touches process-global state (`HERDR_BIN_PATH`,
//! `HERDR_PLUGIN_CONFIG_DIR`, `HERDR_PLUGIN_STATE_DIR`) must hold
//! [`lock_env`] for the whole mutation. Per-module locks are banned:
//! separate locks do not serialize *across* modules, which caused
//! cross-test interference (state tests resolving dirs set by config
//! tests and vice versa).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Hold for the duration of any process-env mutation in tests.
/// Poison-tolerant: a failed test must not wedge the rest.
pub(crate) fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Create a fresh unique temp dir. Callers own cleanup.
pub(crate) fn unique_temp_dir(prefix: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "auto-resume-{}-{}-{}-{}",
        prefix,
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

/// Read the fake-herdr calls log (`@CALL_LOG@` target) under `state_dir`.
/// Empty when the fake never logged (missing file is not an error).
pub(crate) fn herdr_calls(state_dir: &std::path::Path) -> String {
    std::fs::read_to_string(state_dir.join("calls.log")).unwrap_or_default()
}

/// Snapshot of process env vars, restored on drop (set back when they
/// were set, removed when they were unset) — so a panicking test still
/// restores them. Capture BEFORE any `set_var`/`remove_var` of the keys,
/// or the restore puts back the mutated value. Duplicate keys are
/// captured once.
pub(crate) struct EnvRestore(Vec<(String, Option<std::ffi::OsString>)>);

impl EnvRestore {
    pub(crate) fn capture<'a>(keys: impl IntoIterator<Item = &'a str>) -> Self {
        let mut saved: Vec<(String, Option<std::ffi::OsString>)> = Vec::new();
        for k in keys {
            if !saved.iter().any(|(s, _)| s == k) {
                saved.push((k.to_string(), std::env::var_os(k)));
            }
        }
        EnvRestore(saved)
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        for (k, v) in &self.0 {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}

/// Env vars [`run_with_fake_herdr`] pins or that its callers rely on it
/// restoring.
const HARNESS_ENV: &[&str] = &[
    "HERDR_BIN_PATH",
    "HERDR_PLUGIN_CONFIG_DIR",
    "HERDR_PLUGIN_STATE_DIR",
    "HERDR_PANE_ID",
    "HERDR_PLUGIN_EVENT_JSON",
    "HOME",
    "XDG_DATA_HOME",
];

/// Hermetic harness shared by main/monitor tests: fake `HERDR_BIN_PATH`
/// plus isolated state/config dirs, with `HOME` and `XDG_DATA_HOME`
/// pinned inside the base dir so nothing reads the developer's real
/// home (e.g. `~/.local/share/claude-profiles`).
///
/// Layout: `base/bin/herdr` (executable; `@CALL_LOG@` in the script is
/// replaced with the calls-log path) and `base/state` for both
/// `HERDR_PLUGIN_CONFIG_DIR` and `HERDR_PLUGIN_STATE_DIR`; `HOME` is
/// `base/home` and `XDG_DATA_HOME` is `base/xdg` (neither created, so no
/// claude-profile install is seen unless a test builds one or overrides
/// via `extra_env`). The calls log lives at `base/state/calls.log` (the
/// `state_dir` handed to `f`). Applies `extra_env` (`Some` = set, `None` =
/// remove) after the pins, runs `f(&state_dir`), then restores every
/// [`HARNESS_ENV`] key plus every `extra_env` key to its value from
/// before the harness ran (captured once, before any pin, so an
/// `extra_env` override of a pinned key restores the caller's value, not
/// the pin) and removes the base dir. Serialized on the crate-wide env
/// lock; a panic in `f` propagates (drop guards still restore the env
/// and remove the base dir).
pub(crate) fn run_with_fake_herdr(
    script: &str,
    extra_env: &[(&str, Option<&str>)],
    f: impl FnOnce(&std::path::Path),
) {
    let _guard = lock_env();
    let base = unique_temp_dir("harness-test");
    // Declared before the env snapshot so it drops after it: the base
    // dir goes even when `f` panics.
    let _cleanup = RemoveDir(base.clone());
    let _restore = EnvRestore::capture(
        HARNESS_ENV
            .iter()
            .copied()
            .chain(extra_env.iter().map(|(k, _)| *k)),
    );
    let bin_dir = base.join("bin");
    std::fs::create_dir_all(&bin_dir).expect("create bin dir");
    let state_dir = base.join("state");
    std::fs::create_dir_all(&state_dir).expect("create state dir");
    let call_log = state_dir.join("calls.log");
    let body = script.replace("@CALL_LOG@", &call_log.to_string_lossy());
    let bin = bin_dir.join("herdr");
    std::fs::write(&bin, body).expect("write fake herdr");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
        .expect("chmod fake herdr");
    std::env::set_var("HERDR_BIN_PATH", &bin);
    std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", &state_dir);
    std::env::set_var("HERDR_PLUGIN_STATE_DIR", &state_dir);
    std::env::set_var("HOME", base.join("home"));
    std::env::set_var("XDG_DATA_HOME", base.join("xdg"));
    for (k, v) in extra_env {
        match v {
            Some(s) => std::env::set_var(k, s),
            None => std::env::remove_var(k),
        }
    }
    f(&state_dir);
}

/// Removes its directory on drop (panic included).
pub(crate) struct RemoveDir(pub(crate) PathBuf);

impl Drop for RemoveDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// Spawn a sleeper whose argv[0] carries the monitor marker (this test
/// binary's file name) plus `monitor <pane_id>` tokens, so the state
/// module's monitor-liveness check accepts it as the live monitor for
/// the pane. argv[0] may contain spaces (split on space counts), so
/// `sleep` still sees a valid duration in argv[1] and stays alive.
/// Callers kill and reap it.
#[cfg(unix)]
pub(crate) fn spawn_fake_monitor(pane_id: &str) -> std::process::Child {
    use std::os::unix::process::CommandExt;
    let marker = crate::state::our_binary_marker();
    let fake_arg0 = format!("{marker} monitor {pane_id}");
    let child = std::process::Command::new("sleep")
        .arg("60")
        .arg0(&fake_arg0)
        .spawn()
        .expect("spawn sleep");
    // Between fork and exec the child's cmdline is still ours; wait
    // (bounded) until the exec'd argv is visible to `/proc` readers, or
    // without `/proc` to the `/bin/ps` liveness check itself.
    #[cfg(target_os = "linux")]
    let ready = {
        let cmdline = format!("/proc/{}/cmdline", child.id());
        move || std::fs::read(&cmdline).is_ok_and(|c| c.starts_with(fake_arg0.as_bytes()))
    };
    #[cfg(not(target_os = "linux"))]
    let ready = || crate::state::pid_is_monitor_for_pane(child.id(), pane_id);
    for _ in 0..200 {
        if ready() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    child
}

/// [`crate::state::remember_with_snapshot`] on a fresh registry read.
pub(crate) fn remember(pane_id: &str, session: crate::herdr::SessionRef) -> bool {
    crate::state::remember_with_snapshot(pane_id, session, &crate::state::read_registry())
}

/// [`crate::state::forget_with_snapshot`] on a fresh registry read.
pub(crate) fn forget(pane_id: &str) -> bool {
    crate::state::forget_with_snapshot(pane_id, &crate::state::read_registry())
}

/// [`crate::state::record_shell_with_snapshot`] on a fresh shell-record read.
pub(crate) fn record_shell(pane_id: &str, record: crate::state::ShellRecord) -> bool {
    let current = crate::state::shell_record(pane_id);
    crate::state::record_shell_with_snapshot(pane_id, record, current.as_ref())
}

/// Remove a pane's monitor lock unconditionally (invalid ids touch
/// nothing). Monitors release via [`crate::state::release_monitor_lock`].
pub(crate) fn clear_monitor_lock(pane_id: &str) {
    if crate::state::valid_pane_id(pane_id) {
        std::fs::remove_file(crate::state::monitor_lock_path(pane_id)).ok();
    }
}

/// True iff the pane's monitor lock currently records `pid` (and this
/// pane). Invalid ids and missing/corrupt locks read as false.
pub(crate) fn monitor_lock_owned_by(pane_id: &str, pid: u32) -> bool {
    crate::state::valid_pane_id(pane_id)
        && crate::state::lock_file_names(&crate::state::monitor_lock_path(pane_id), pane_id, pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_restores_caller_value_of_overridden_pinned_key() {
        let before = {
            let _guard = lock_env();
            std::env::var_os("HOME")
        };
        run_with_fake_herdr(
            "#!/bin/sh\nexit 0\n",
            &[("HOME", Some("/override"))],
            |_| {
                assert_eq!(std::env::var("HOME").as_deref(), Ok("/override"));
            },
        );
        let _guard = lock_env();
        assert_eq!(std::env::var_os("HOME"), before);
    }
}
