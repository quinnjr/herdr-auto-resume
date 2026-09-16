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

/// Hermetic harness shared by main/monitor tests: fake `HERDR_BIN_PATH`
/// plus isolated state/config dirs.
///
/// Layout: `base/bin/herdr` (executable; `@CALL_LOG@` in the script is
/// replaced with the calls-log path) and `base/state` for both
/// `HERDR_PLUGIN_CONFIG_DIR` and `HERDR_PLUGIN_STATE_DIR`. The calls log
/// lives at `base/state/calls.log` (the `state_dir` handed to `f`).
/// Applies `extra_env` (`Some` = set, `None` = remove), runs `f(&state_dir`),
/// then restores BIN+CONFIG+STATE+PANE_ID+EVENT_JSON and removes the base
/// dir. Serialized on the crate-wide env lock; asserts the closure did not
/// panic.
pub(crate) fn run_with_fake_herdr(
    script: &str,
    extra_env: &[(&str, Option<&str>)],
    f: impl FnOnce(&std::path::Path),
) {
    let _guard = lock_env();
    let prev_bin = std::env::var_os("HERDR_BIN_PATH");
    let prev_config = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR");
    let prev_state = std::env::var_os("HERDR_PLUGIN_STATE_DIR");
    let prev_pane = std::env::var_os("HERDR_PANE_ID");
    let prev_event = std::env::var_os("HERDR_PLUGIN_EVENT_JSON");
    let base = unique_temp_dir("harness-test");
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
    for (k, v) in extra_env {
        match v {
            Some(s) => std::env::set_var(k, s),
            None => std::env::remove_var(k),
        }
    }
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&state_dir)));
    match prev_bin {
        Some(v) => std::env::set_var("HERDR_BIN_PATH", v),
        None => std::env::remove_var("HERDR_BIN_PATH"),
    }
    match prev_config {
        Some(v) => std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", v),
        None => std::env::remove_var("HERDR_PLUGIN_CONFIG_DIR"),
    }
    match prev_state {
        Some(v) => std::env::set_var("HERDR_PLUGIN_STATE_DIR", v),
        None => std::env::remove_var("HERDR_PLUGIN_STATE_DIR"),
    }
    match prev_pane {
        Some(v) => std::env::set_var("HERDR_PANE_ID", v),
        None => std::env::remove_var("HERDR_PANE_ID"),
    }
    match prev_event {
        Some(v) => std::env::set_var("HERDR_PLUGIN_EVENT_JSON", v),
        None => std::env::remove_var("HERDR_PLUGIN_EVENT_JSON"),
    }
    std::fs::remove_dir_all(&base).ok();
    assert!(r.is_ok());
}
