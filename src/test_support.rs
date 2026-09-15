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
