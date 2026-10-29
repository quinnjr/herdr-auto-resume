use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::config;
use crate::herdr::SessionRef;

fn registry_path() -> PathBuf {
    config::state_dir().join("registry.json")
}

pub fn monitors_dir() -> PathBuf {
    config::state_dir().join("monitors")
}

fn log_path() -> PathBuf {
    config::state_dir().join("log.txt")
}

fn state_lock_path() -> PathBuf {
    config::state_dir().join("state.lock")
}

/// The legacy `$HERDR_PLUGIN_CONFIG_DIR/registry.json`, when it is a
/// different path from the current target.
/// REMOVE-AFTER: delete once installs older than v0.1.x are negligible.
fn legacy_registry_path(target: &Path) -> Option<PathBuf> {
    let src = PathBuf::from(config::env_nonempty("HERDR_PLUGIN_CONFIG_DIR")?).join("registry.json");
    (src != target).then_some(src)
}

/// A process-unique `<pid>.<n>` suffix (`n` a process-wide counter), so
/// concurrent processes and threads — and monitors of an older release
/// still running mid-upgrade, which staged through a fixed
/// `<name>.tmp` — never share one file.
fn unique_suffix() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}.{n}", std::process::id())
}

/// Every staging file's name: `<base>.stage-<pid>.<n>.tmp`
/// ([`unique_suffix`]). The `.stage-` marker is what
/// [`is_staging_name`] sweeps.
fn stage_name(base: &str) -> String {
    format!("{base}.stage-{}.tmp", unique_suffix())
}

/// Staging name next to `path` ([`stage_name`]). A tmp orphaned by a
/// crash between write and rename is deleted later by [`sweep_orphans`].
fn tmp_path_for(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(stage_name(&name))
}

/// Staging files untouched this long are orphans: every write renames
/// (or removes) its own within milliseconds.
const ORPHAN_AGE: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// True for a state-dir file name this crate (or an older release)
/// stages through: any [`stage_name`] (map tmps, monitor lock reclaim
/// tmps), or an older release's fixed map tmp.
fn is_staging_name(name: &str) -> bool {
    const LEGACY: &[&str] = &["registry.json.tmp", "shells.json.tmp", "profiles.json.tmp"];
    (name.ends_with(".tmp") && name.contains(".stage-")) || LEGACY.contains(&name)
}

/// Delete orphaned staging files ([`is_staging_name`]) in the state dir
/// whose mtime is older than [`ORPHAN_AGE`]. Run by
/// `startup`/`supervise-all` WITHOUT `state.lock` (it must not stall
/// the pane scan behind a busy lock): staging names are unique per
/// writer (bar the legacy fixed one) and each file lives only between
/// its write and rename (milliseconds), so one that old has no live
/// writer. Best-effort; returns the
/// number removed.
pub fn sweep_orphans() -> usize {
    sweep_orphans_older_than(ORPHAN_AGE)
}

fn sweep_orphans_older_than(age: std::time::Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(config::state_dir()) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        if !is_staging_name(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|e| e > age));
        if old && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

thread_local! {
    /// True while this thread is inside a [`with_state_lock_within`] scope
    /// (holding `state.lock`, or in its unlocked fallback).
    static IN_LOCK_SCOPE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` holding an exclusive inter-process lock on `state.lock`, so
/// each load-modify-save of the shared JSON maps is serialized across
/// monitor processes (and threads: each call opens its own handle).
/// The lock is released when the handle drops, including on process
/// death. `Some(f())` once held, waiting at most `wait` (writers pass
/// [`STATE_LOCK_WAIT`]).
///
/// Two failures differ ([`StateLockFail`]): where locking is
/// unsupported or errors outright, `f` runs unlocked — best-effort
/// beats never writing on such a filesystem; while another process
/// holds the lock for the whole wait, `f` is skipped (None): an
/// unlocked write could undo that holder's. Either is logged to
/// `log.txt` at most once a minute per process.
///
/// Re-entrant per thread: a nested call runs `f` directly instead of
/// deadlocking on a second `flock` handle; likewise [`append_log`]'s
/// rotation (a failed write logging from inside `f`).
///
/// The scope is marked only while `f` runs (lock held, or the
/// unsupported-lock fallback); failures are logged outside it, so the
/// log's rotation never runs unlocked while another process holds the
/// lock ([`append_log_capped`] skips it instead).
fn with_state_lock_within<T>(wait: std::time::Duration, f: impl FnOnce() -> T) -> Option<T> {
    if IN_LOCK_SCOPE.with(std::cell::Cell::get) {
        return Some(f());
    }
    let path = state_lock_path();
    match open_state_lock(wait) {
        Err(StateLockFail::Busy) => {
            let msg = format!(
                "state: lock {} busy after {wait:?}; skipped write",
                path.display()
            );
            eprintln!("{msg}");
            append_log_throttled(&format!("{} busy", path.display()), &msg);
            None
        }
        Err(StateLockFail::Unsupported(e)) => {
            let msg = format!(
                "state: lock {} failed: {e}; proceeding unlocked",
                path.display()
            );
            eprintln!("{msg}");
            append_log_throttled(&path.display().to_string(), &msg);
            Some(in_lock_scope(f))
        }
        Ok(handle) => {
            let out = in_lock_scope(f);
            drop(handle);
            Some(out)
        }
    }
}

/// How long [`with_state_lock_within`] waits for a held `state.lock` by
/// default.
const STATE_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Why `state.lock` was not taken ([`with_state_lock_within`]).
#[derive(Debug)]
enum StateLockFail {
    /// Locking is unavailable (open failed, or `try_lock` errored, e.g.
    /// `ENOLCK`/`EOPNOTSUPP`): proceed unlocked.
    Unsupported(std::io::Error),
    /// Another process held the lock for the whole wait: skip.
    Busy,
}

/// Take `state.lock` on `file`, retrying `try_lock` every 10ms until
/// `wait` elapses.
fn try_lock_within(file: &std::fs::File, wait: std::time::Duration) -> Result<(), StateLockFail> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::Error(e)) => return Err(StateLockFail::Unsupported(e)),
            Err(std::fs::TryLockError::WouldBlock) => {
                if std::time::Instant::now() >= deadline {
                    return Err(StateLockFail::Busy);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
}

/// Open `state.lock` (creating it and its dir) and take it within
/// `wait` ([`try_lock_within`]); the lock lasts as long as the handle.
fn open_state_lock(wait: std::time::Duration) -> Result<std::fs::File, StateLockFail> {
    let path = state_lock_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(StateLockFail::Unsupported)
        .and_then(|file| try_lock_within(&file, wait).map(|()| file))
}

/// Run `f` marked as inside a [`with_state_lock_within`] scope.
fn in_lock_scope<T>(f: impl FnOnce() -> T) -> T {
    IN_LOCK_SCOPE.with(|h| h.set(true));
    // Reset on unwind too, so a panicking `f` cannot leave this thread
    // believing it still holds the lock.
    struct Release;
    impl Drop for Release {
        fn drop(&mut self) {
            IN_LOCK_SCOPE.with(|h| h.set(false));
        }
    }
    let _release = Release;
    f()
}

/// Atomically persist `value` as JSON at `path` via a tmp
/// ([`tmp_path_for`]) + rename; a failed write or rename removes the
/// tmp. True on success; failures (serialization included, which
/// writes nothing) are logged and return false.
fn write_json_atomic<T: Serialize>(path: &Path, value: &T, who: &str) -> bool {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let text = match serde_json::to_string(value) {
        Ok(text) => text,
        Err(e) => {
            let msg = format!("{who}: serializing {} failed: {e}", path.display());
            eprintln!("{msg}");
            append_log(&msg);
            return false;
        }
    };
    let tmp = tmp_path_for(path);
    let result = std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, path));
    match result {
        Ok(()) => true,
        Err(e) => {
            std::fs::remove_file(&tmp).ok();
            let msg = format!("{who}: persisting {} failed: {e}", path.display());
            eprintln!("{msg}");
            append_log(&msg);
            false
        }
    }
}

/// Why [`read_map`] could not load a map.
#[derive(Debug)]
enum MapError {
    /// A read error that may clear on its own (EACCES, EIO, ...).
    Unreadable(String),
    /// The file was read but is not a UTF-8 JSON object; it will never
    /// parse, so writers quarantine it ([`edit_map_locked`]).
    Corrupt(String),
}

impl std::fmt::Display for MapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MapError::Unreadable(e) | MapError::Corrupt(e) => f.write_str(e),
        }
    }
}

/// A JSON map as read for an edit: the entries that deserialized, plus
/// the raw JSON of those that did not (e.g. written by a newer release),
/// which [`edit_map_locked`] writes back unchanged instead of deleting.
#[derive(Debug, Serialize)]
struct MapFile<T> {
    #[serde(flatten)]
    entries: HashMap<String, T>,
    #[serde(flatten)]
    unparsed: HashMap<String, serde_json::Value>,
}

impl<T> MapFile<T> {
    fn empty() -> Self {
        Self {
            entries: HashMap::new(),
            unparsed: HashMap::new(),
        }
    }
}

/// Read the JSON map at `path`: a missing file is an empty map, and a
/// single bad entry is skipped (reported on stderr only: read-only
/// callers never touch disk) instead of dropping the others.
/// Invalid UTF-8 or contents that are not a JSON object is
/// [`MapError::Corrupt`]; any other read error is
/// [`MapError::Unreadable`].
fn read_map<T: DeserializeOwned>(path: &Path, who: &str) -> Result<HashMap<String, T>, MapError> {
    parse_map(path, who, None)
}

/// [`read_map`] for writers, keeping the bad entries' raw JSON
/// ([`MapFile`]). A bad entry is also logged to `log.txt` once per file
/// and key per process.
fn read_map_file<T: DeserializeOwned>(path: &Path, who: &str) -> Result<MapFile<T>, MapError> {
    let mut unparsed = HashMap::new();
    let entries = parse_map(path, who, Some(&mut unparsed))?;
    Ok(MapFile { entries, unparsed })
}

/// [`read_map`]'s parse; with `unparsed` (writers only), bad entries
/// are kept there and logged ([`read_map_file`]).
fn parse_map<T: DeserializeOwned>(
    path: &Path,
    who: &str,
    mut unparsed: Option<&mut HashMap<String, serde_json::Value>>,
) -> Result<HashMap<String, T>, MapError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            return Err(MapError::Corrupt(format!(
                "corrupt {}: {e}",
                path.display()
            )))
        }
        Err(e) => {
            return Err(MapError::Unreadable(format!(
                "unreadable {}: {e}",
                path.display()
            )))
        }
    };
    let raw: HashMap<String, serde_json::Value> = serde_json::from_str(&text)
        .map_err(|e| MapError::Corrupt(format!("corrupt {}: {e}", path.display())))?;
    let mut entries = HashMap::new();
    for (k, v) in raw {
        match T::deserialize(&v) {
            Ok(s) => {
                entries.insert(k, s);
            }
            Err(e) => {
                let msg = format!(
                    "{who}: skipping bad entry for {k:?} in {}: {e} (kept on disk)",
                    path.display()
                );
                eprintln!("{msg}");
                if let Some(unparsed) = unparsed.as_deref_mut() {
                    append_log_once(&format!("{}#{k}", path.display()), &msg);
                    unparsed.insert(k, v);
                }
            }
        }
    }
    Ok(entries)
}

/// [`append_log`] at most once per `key` per process.
fn append_log_once(key: &str, line: &str) {
    static SEEN: std::sync::Mutex<Option<std::collections::HashSet<String>>> =
        std::sync::Mutex::new(None);
    let first = SEEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(Default::default)
        .insert(key.to_string());
    if first {
        append_log(line);
    }
}

/// [`read_map`] for read-only callers: an unreadable or corrupt file
/// reads as an empty map (logged). Writers use [`read_map`] directly so
/// a file is never overwritten with a map missing its entries.
fn load_tolerant_map<T: DeserializeOwned>(path: &Path, who: &str) -> HashMap<String, T> {
    read_map(path, who).unwrap_or_else(|e| {
        eprintln!("{who}: {e}");
        HashMap::new()
    })
}

/// Copy a corrupt map file's bytes to `<name>.corrupt-<unix-secs>.<pid>.<n>`
/// ([`unique_suffix`], so two quarantines within one second never
/// overwrite each other) so writers can start over (the original bytes
/// stay inspectable). A copy, not a rename: the caller then replaces
/// `<name>` atomically, so the file is never absent (an absent registry
/// would re-run the legacy migration). The copy's path on success.
fn quarantine_corrupt(path: &Path) -> std::io::Result<PathBuf> {
    let mut dest = path.as_os_str().to_owned();
    dest.push(format!(".corrupt-{}.{}", unix_secs(), unique_suffix()));
    let dest = PathBuf::from(dest);
    std::fs::copy(path, &dest)?;
    Ok(dest)
}

fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// [`append_log`] at most once per minute per `key` (per process), for
/// conditions that would otherwise log every poll.
fn append_log_throttled(key: &str, line: &str) {
    static LAST: std::sync::Mutex<Option<HashMap<String, u64>>> = std::sync::Mutex::new(None);
    let now = unix_secs();
    {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        let last = last.get_or_insert_with(HashMap::new);
        if last.get(key).is_some_and(|&t| now < t + 60) {
            return;
        }
        last.insert(key.to_string(), now);
    }
    append_log(line);
}

/// The legacy registry to migrate from (or, for `status`, read in
/// place): only while `target` is absent. A quarantine never removes
/// `target` ([`quarantine_corrupt`] copies, then the replacement is
/// written over it), so a quarantined registry never resurrects the
/// legacy file's stale sessions.
/// REMOVE-AFTER: delete once installs older than v0.1.x are negligible.
fn pending_legacy_registry(target: &Path) -> Option<PathBuf> {
    if target.exists() {
        return None;
    }
    legacy_registry_path(target).filter(|s| s.exists())
}

/// One-shot registry migration (caller holds the state lock): when
/// [`pending_legacy_registry`] names a source, copy it over atomically
/// (tmp + rename, so a crash never leaves a half-written registry).
/// REMOVE-AFTER: delete once installs older than v0.1.x are negligible.
fn migrate_registry_locked(target: &Path) {
    let Some(src) = pending_legacy_registry(target) else {
        return;
    };
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let tmp = tmp_path_for(target);
    // Staged by read + write, not `fs::copy`: a copy may keep the
    // source's old mtime (macOS clonefile), which would let a
    // concurrent orphan sweep delete the tmp mid-migration.
    match std::fs::read(&src)
        .and_then(|bytes| std::fs::write(&tmp, bytes))
        .and_then(|_| std::fs::rename(&tmp, target))
    {
        Ok(_) => eprintln!(
            "load_registry: migrated registry from {} to {}",
            src.display(),
            target.display()
        ),
        Err(e) => {
            std::fs::remove_file(&tmp).ok();
            eprintln!(
                "load_registry: migration from {} failed: {e}",
                src.display()
            )
        }
    }
}

/// Strict registry load for writers already holding the state lock.
fn load_registry_locked() -> Result<MapFile<SessionRef>, MapError> {
    let target = registry_path();
    migrate_registry_locked(&target);
    read_map_file(&target, "load_registry")
}

/// Load the durable pane-id -> session registry. Missing file, unreadable
/// file, or invalid JSON yields an empty map; bad entries are skipped.
///
/// One-shot auto-migration: when the target `state_dir()/registry.json` is
/// absent but `$HERDR_PLUGIN_CONFIG_DIR/registry.json` exists at a different
/// path ([`pending_legacy_registry`]), the legacy file is copied over
/// (under the state lock; skipped while it is busy, so a later load
/// retries) before loading. While not migrated (lock busy, or the copy
/// failed) the legacy file is read in place, as [`read_registry`] does.
/// Read-only callers use [`read_registry`] instead.
/// REMOVE-AFTER: delete once installs older than v0.1.x are negligible.
pub fn load_registry() -> HashMap<String, SessionRef> {
    load_registry_within(STATE_LOCK_WAIT)
}

/// [`load_registry`] with an explicit migration lock wait.
fn load_registry_within(wait: std::time::Duration) -> HashMap<String, SessionRef> {
    let target = registry_path();
    if pending_legacy_registry(&target).is_some() {
        with_state_lock_within(wait, || migrate_registry_locked(&target));
        if let Some(src) = pending_legacy_registry(&target) {
            return load_tolerant_map(&src, "load_registry");
        }
    }
    load_tolerant_map(&target, "load_registry")
}

/// Pure read of the effective registry: the state-dir registry when
/// present, else the legacy config-dir registry while it is still
/// pending migration ([`pending_legacy_registry`]). Never migrates,
/// creates directories, or writes anything (for `status`).
pub fn read_registry() -> HashMap<String, SessionRef> {
    let target = registry_path();
    if let Some(src) = pending_legacy_registry(&target) {
        return load_tolerant_map(&src, "read_registry");
    }
    load_tolerant_map(&target, "read_registry")
}

/// Double-checked edit of one shared JSON map: `noop` (decided by the
/// caller on an unlocked read) skips the lock entirely; otherwise, under
/// the state lock, the map is reloaded via `load_locked` and `edit`
/// re-decides on it (false = nothing to write). A corrupt file (not a
/// UTF-8 JSON object) is quarantined ([`quarantine_corrupt`], logged)
/// and the edit proceeds on an empty map written atomically over it, so
/// one bad file cannot block every later write. Any other reload failure (EACCES, EIO, ...) may
/// be transient: it aborts without writing, so the other panes' entries
/// are never wiped, logged at most once a minute per file. Entries that
/// did not deserialize are written back unchanged ([`MapFile`]) unless
/// `edit` replaced them. The lock wait is `wait`
/// ([`with_state_lock_within`]). True on persist or no-op; false on an
/// aborted edit, a busy lock (nothing written) or write failure
/// (logged).
fn edit_map_locked<V: Serialize>(
    path: &Path,
    noop: bool,
    wait: std::time::Duration,
    load_locked: impl FnOnce() -> Result<MapFile<V>, MapError>,
    edit: impl FnOnce(&mut HashMap<String, V>) -> bool,
    who: &str,
) -> bool {
    if noop {
        return true;
    }
    with_state_lock_within(wait, || {
        let (mut map, quarantined) = match load_locked() {
            Ok(map) => (map, false),
            Err(MapError::Corrupt(e)) => match quarantine_corrupt(path) {
                Ok(dest) => {
                    let msg = format!("{who}: {e}; copied aside to {}", dest.display());
                    eprintln!("{msg}");
                    append_log(&msg);
                    (MapFile::empty(), true)
                }
                Err(qe) => {
                    let msg = format!("{who}: {e}; quarantine failed: {qe}; not writing");
                    eprintln!("{msg}");
                    append_log_throttled(&path.display().to_string(), &msg);
                    return false;
                }
            },
            Err(e @ MapError::Unreadable(_)) => {
                let msg = format!("{who}: {e}; not writing");
                eprintln!("{msg}");
                append_log_throttled(&path.display().to_string(), &msg);
                return false;
            }
        };
        // After a quarantine, write even an unchanged (empty) map so the
        // corrupt bytes are replaced.
        if !edit(&mut map.entries) && !quarantined {
            return true;
        }
        // An edited key supersedes its unparsed raw entry (no duplicate
        // keys in the written object).
        let entries = &map.entries;
        map.unparsed.retain(|k, _| !entries.contains_key(k));
        write_json_atomic(path, &map, who)
    })
    .unwrap_or(false)
}

/// Record (or overwrite) `key` → `value` in the map at `path`, skipping
/// the write — and the lock — when `current` (`key`'s entry on a
/// lock-free read) already equals it; re-checked under the lock on
/// `load_locked`'s reload ([`edit_map_locked`], waiting `wait`).
fn upsert_locked<V: Serialize + PartialEq>(
    path: &Path,
    key: &str,
    value: V,
    current: Option<&V>,
    wait: std::time::Duration,
    load_locked: impl FnOnce() -> Result<MapFile<V>, MapError>,
    who: &str,
) -> bool {
    let noop = current == Some(&value);
    edit_map_locked(
        path,
        noop,
        wait,
        load_locked,
        |map| {
            if map.get(key) == Some(&value) {
                return false;
            }
            map.insert(key.to_string(), value);
            true
        },
        who,
    )
}

/// Forget the session for a pane: one-shot resurrection consumed the
/// entry (a later live agent re-arms via `remember`). No-op — and no
/// file write — when the pane has no entry. True unless the write failed.
/// The no-op is decided on the caller's registry `snapshot` (an
/// unlocked view, mirrors [`remember_with_snapshot`]; re-checked under
/// the lock), so an absent pane never contends for `state.lock`.
pub(crate) fn forget_with_snapshot(pane_id: &str, snapshot: &HashMap<String, SessionRef>) -> bool {
    edit_map_locked(
        &registry_path(),
        !snapshot.contains_key(pane_id),
        STATE_LOCK_WAIT,
        load_registry_locked,
        |reg| reg.remove(pane_id).is_some(),
        "forget",
    )
}

/// Record (or overwrite) the session for a pane, persisting to disk.
/// Skips the write when the stored value is unchanged (monitors call this
/// every poll while the agent is alive; the unchanged check runs on an
/// unlocked read first, so steady state never takes `state.lock`).
/// Refuses unsafe session values (returns false: nothing persisted).
/// The caller's registry `snapshot` (e.g. a monitor's per-poll
/// [`load_registry`]) is that unlocked view, sparing a second registry
/// read; a write still reloads the registry under the lock first.
/// True on persist or no-op; false on refusal, a busy lock or write
/// failure (logged).
pub(crate) fn remember_with_snapshot(
    pane_id: &str,
    session: SessionRef,
    snapshot: &HashMap<String, SessionRef>,
) -> bool {
    remember_with_snapshot_within(pane_id, session, snapshot, STATE_LOCK_WAIT)
}

/// [`remember_with_snapshot`] waiting at most `wait` for `state.lock`
/// (then skipping the write, as [`with_state_lock_within`] does): `hook-pane`
/// runs synchronously on herdr's hook path.
pub(crate) fn remember_with_snapshot_within(
    pane_id: &str,
    session: SessionRef,
    snapshot: &HashMap<String, SessionRef>,
    wait: std::time::Duration,
) -> bool {
    if !is_safe_session_value(&session.value) {
        eprintln!("remember: refusing unsafe session value for pane {pane_id:?}");
        return false;
    }
    upsert_locked(
        &registry_path(),
        pane_id,
        session,
        snapshot.get(pane_id),
        wait,
        load_registry_locked,
        "remember",
    )
}

use crate::resume::is_safe_session_value;

/// The shell an agent was last seen running under, per pane. `live`
/// stays true while that agent is the pane's job; an exit inside the
/// same shell (Ctrl-C, `/exit`) retires it. Only a live record whose
/// shell has since been replaced — the session host crashed or
/// restarted — licenses a relaunch.
/// `argv_seen` is the agent process last seen under this shell, with
/// its own session flag if any (or the one we relaunched): a value
/// lingers in argv after the agent moves on, so argv is evidence of a
/// new session only when the monitor saw it appear ([`SeenArgv::is_new`]). Persisted so a
/// restarted monitor still knows it; kept while the same shell lives
/// (a user exit included), absent for a new shell, cleared for a
/// different agent. A record without it (e.g. written by v0.6.0) is no
/// evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellRecord {
    pub shell: String,
    pub live: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv_seen: Option<SeenArgv>,
}

/// An agent process as last seen, with its own session flag value
/// (None: it carried no flag, e.g. a bare `opencode`). `pid` is that
/// process's pid; None marks the process our relaunch started (pid not
/// yet seen) or one herdr reported without a pid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeenArgv {
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

impl SeenArgv {
    /// Whether `current` (the agent process seen this poll) carries new
    /// evidence of a session, given the last sighting `seen` under the
    /// same shell: a different process started (hand-start after a
    /// user exit, even when the exited one had no flag), or the same
    /// process gained a different flag value. No sighting, no pid for
    /// the current process, or no flag on it is never evidence; nor is
    /// the same process's flag read where the last read had none.
    pub fn is_new(seen: Option<&SeenArgv>, current: &SeenArgv) -> bool {
        let (Some(seen), Some(pid), Some(_)) = (seen, current.pid, &current.value) else {
            return false;
        };
        match seen.pid {
            Some(p) if p != pid => true,
            Some(_) => seen.value.is_some() && seen.value != current.value,
            None => seen.value != current.value,
        }
    }

    /// The sighting to keep after `current`: it replaces `seen`, except
    /// that the same process seen without its flag keeps the flag
    /// already known (argv missing from one report is not a change).
    pub fn merge(seen: Option<SeenArgv>, current: Option<SeenArgv>) -> Option<SeenArgv> {
        match (seen, current) {
            (Some(s), Some(mut c)) if c.pid.is_some() && c.pid == s.pid => {
                c.value = c.value.or(s.value);
                Some(c)
            }
            (seen, current) => current.or(seen),
        }
    }
}

fn shells_path() -> PathBuf {
    config::state_dir().join("shells.json")
}

/// Load the pane-id -> shell record map. Missing file, unreadable file,
/// or invalid JSON yields an empty map (no record never relaunches); a
/// bad entry is skipped without dropping the others.
pub fn load_shells() -> HashMap<String, ShellRecord> {
    load_tolerant_map(&shells_path(), "load_shells")
}

pub fn shell_record(pane_id: &str) -> Option<ShellRecord> {
    load_shells().remove(pane_id)
}

/// Record (or overwrite) the shell record for a pane, atomically via a
/// tmp + rename under the state lock. Skips the write when
/// unchanged (monitors call this every poll while the agent is alive;
/// the unchanged check runs unlocked first, re-checked under the lock).
/// The caller's `current` record (e.g. a monitor's per-poll
/// [`shell_record`]) is that unlocked view (mirrors
/// [`remember_with_snapshot`]).
/// True on persist or no-op; false on write failure (logged).
pub(crate) fn record_shell_with_snapshot(
    pane_id: &str,
    record: ShellRecord,
    current: Option<&ShellRecord>,
) -> bool {
    upsert_locked(
        &shells_path(),
        pane_id,
        record,
        current,
        STATE_LOCK_WAIT,
        || read_map_file(&shells_path(), "record_shell"),
        "record_shell",
    )
}

fn profiles_path() -> PathBuf {
    config::state_dir().join("profiles.json")
}

fn load_profiles() -> HashMap<String, String> {
    load_tolerant_map(&profiles_path(), "load_profiles")
}

/// The claude-profile a pane's Claude was last seen running under.
pub fn profile_for(pane_id: &str) -> Option<String> {
    load_profiles().remove(pane_id)
}

/// Record the pane's live claude-profile, atomically via a
/// tmp + rename under the state lock. Skips the write when unchanged
/// (monitors call this every poll while Claude is alive; the unchanged
/// check runs unlocked first, re-checked under the lock). True on
/// persist or no-op; false on write failure (logged).
pub fn record_profile(pane_id: &str, profile: &str) -> bool {
    upsert_locked(
        &profiles_path(),
        pane_id,
        profile.to_string(),
        load_profiles().get(pane_id),
        STATE_LOCK_WAIT,
        || read_map_file(&profiles_path(), "record_profile"),
        "record_profile",
    )
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

/// Claim this process's monitor lock for a pane ([`write_monitor_lock_pid`]).
/// True when we hold the claim.
pub fn write_monitor_lock(pane_id: &str) -> bool {
    write_monitor_lock_pid(pane_id, std::process::id())
}

/// Claim a pane's monitor lock for `pid` atomically: the full lock is
/// staged in a claimer-scoped tmp (in the state dir, so lock scans never
/// see it) and hard-linked into place, an atomic create-if-absent that
/// never exposes a half-written lock. On `AlreadyExists` a live rival
/// keeps its claim (return false without overwriting) while a stale
/// lock is reclaimed (overwrite, return true). A live pre-v0.1.x
/// monitor for the pane ([`BadLock::lock`]) also refuses the claim; a
/// dead one's file is deleted once we hold it
/// ([`remove_dead_legacy_lock`]). True = we hold the claim.
pub fn write_monitor_lock_pid(pane_id: &str, pid: u32) -> bool {
    if !valid_pane_id(pane_id) {
        eprintln!("write_monitor_lock: rejected invalid pane id {pane_id:?}");
        return false;
    }
    // A live pre-v0.1.x monitor for this pane holds a lock under its old
    // name, which the create-if-absent below cannot see: yield to it.
    if let Some(rival) = live_legacy_monitor_pid(pane_id).filter(|&r| r != pid) {
        eprintln!("write_monitor_lock: pane {pane_id:?} has a live legacy monitor (pid {rival})");
        return false;
    }
    let path = monitor_lock_path(pane_id);
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            eprintln!("write_monitor_lock: cannot create dir {}", parent.display());
            return false;
        }
    }
    // pane_id is embedded so the liveness check can verify the process
    // is the monitor for THIS pane (guards against PID reuse by a
    // monitor for another pane or an unrelated same-named binary).
    let text = serde_json::json!({ "pid": pid, "pane_id": pane_id }).to_string();
    let tmp = config::state_dir().join(stage_name(&format!("monitor-{}", encode_pane_id(pane_id))));
    if let Err(e) = std::fs::write(&tmp, &text) {
        std::fs::remove_file(&tmp).ok();
        eprintln!("write_monitor_lock: stage {} failed: {e}", tmp.display());
        return false;
    }
    let claimed = match create_lock_file(&tmp, &path, &text) {
        Ok(()) => true,
        // A live rival keeps its claim.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            match live_monitor_pid(pane_id) {
                Some(_) => false,
                None => {
                    eprintln!("write_monitor_lock: reclaiming stale lock for {pane_id:?}");
                    // Check-then-write is racy, so reclaim by renaming
                    // the staged tmp over the lock, then re-read: whoever
                    // renamed last wins, and a loser that reads a foreign
                    // pid backs off. A loser that confirmed before the
                    // winner's rename is caught by its per-poll
                    // [`hold_monitor_lock`] re-check.
                    if let Err(e) = std::fs::rename(&tmp, &path) {
                        eprintln!("write_monitor_lock: reclaim {} failed: {e}", path.display());
                        false
                    } else if !lock_file_names(&path, pane_id, pid) {
                        eprintln!("write_monitor_lock: lost reclaim race for {pane_id:?}");
                        false
                    } else {
                        true
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("write_monitor_lock: create {} failed: {e}", path.display());
            false
        }
    };
    std::fs::remove_file(&tmp).ok();
    if claimed {
        remove_dead_legacy_lock(pane_id);
    }
    claimed
}

/// Create `path` with the staged `tmp`'s content only if absent
/// (`AlreadyExists` otherwise), via `hard_link`. Where hard links are
/// refused (e.g. Android's SELinux policy), falls back to `create_new` +
/// write, which a racing claimant can briefly read as empty.
fn create_lock_file(tmp: &Path, path: &Path, text: &str) -> std::io::Result<()> {
    match std::fs::hard_link(tmp, path) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?
                .write_all(text.as_bytes())
        }
        r => r,
    }
}

/// True when the lock file at `path` parses and names exactly
/// (`pane_id`, `pid`). Unreadable or corrupt reads as not ours.
pub(crate) fn lock_file_names(path: &Path, pane_id: &str, pid: u32) -> bool {
    std::fs::read_to_string(path).is_ok_and(|text| lock_text_names(&text, pane_id, pid))
}

/// True when lock-file `text` parses and names exactly (`pane_id`, `pid`).
fn lock_text_names(text: &str, pane_id: &str, pid: u32) -> bool {
    parse_lock_text(text)
        .is_ok_and(|(locked_pane, locked_pid)| locked_pane == pane_id && locked_pid == pid)
}

/// Outcome of a compare-and-delete of one lock file
/// ([`remove_lock_file_if_names`]).
#[derive(Debug)]
pub enum LockRemoval {
    /// Deleted while it named the expected pid (or already gone).
    Removed,
    /// It names someone else now (a newer claim): left in place.
    Changed,
    /// Reading or deleting it failed; nothing was removed.
    Failed(std::io::Error),
}

/// Delete the lock file at `path` only while it names (`pane_id`,
/// `pid`): compare-and-delete, re-reading the lock immediately before
/// the unlink. A sub-millisecond race remains — a claim landing between
/// that read and the unlink is deleted — and self-heals: the owning
/// monitor re-claims a missing lock on its next poll
/// ([`hold_monitor_lock`]).
fn remove_lock_file_if_names(path: &Path, pane_id: &str, pid: u32) -> LockRemoval {
    match std::fs::read_to_string(path) {
        Ok(text) if lock_text_names(&text, pane_id, pid) => {}
        Ok(_) => return LockRemoval::Changed,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return LockRemoval::Removed,
        Err(e) => return LockRemoval::Failed(e),
    }
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => LockRemoval::Failed(e),
        _ => LockRemoval::Removed,
    }
}

/// Verdict of [`hold_monitor_lock`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockHold {
    /// The lock names us.
    Ours,
    /// It was missing, stale or corrupt and is ours again.
    Reclaimed,
    /// It positively names this other pid, the live monitor for this
    /// pane: the only verdict that retires the caller.
    Lost(u32),
    /// Not ours and re-claiming failed (I/O error, ENOSPC, EACCES …)
    /// without any live rival owning it: keep running and retry.
    Failed,
}

/// Keep `pid`'s claim on the pane's monitor lock (the monitor's
/// per-poll check), from a single read of the lock: names us →
/// [`LockHold::Ours`]; names a different pid that is the live monitor
/// for this pane → [`LockHold::Lost`]; missing (deleted under a running
/// monitor), dead or foreign-pane pid, corrupt or unreadable → re-claimed
/// via [`write_monitor_lock_pid`]. A failed re-claim is `Lost` only when
/// a live rival now holds the lock (it won a reclaim race) or a live
/// pre-v0.1.x monitor runs for the pane, else [`LockHold::Failed`].
pub fn hold_monitor_lock(pane_id: &str, pid: u32) -> LockHold {
    if let Some((locked_pid, locked_pane)) = lock_record(pane_id) {
        if locked_pane == pane_id {
            if locked_pid == pid {
                return LockHold::Ours;
            }
            if pid_is_monitor_for_pane(locked_pid, pane_id) {
                return LockHold::Lost(locked_pid);
            }
        }
    }
    if write_monitor_lock_pid(pane_id, pid) {
        return LockHold::Reclaimed;
    }
    match live_monitor_pid(pane_id) {
        Some(rival) if rival != pid => LockHold::Lost(rival),
        _ => LockHold::Failed,
    }
}

/// A monitor's release of its lock: compare-and-delete. The lock file
/// is removed only while it still names (`pane_id`, `pid`), via
/// [`remove_lock_file_if_names`], so a lock another monitor has since
/// claimed is left alone. Best-effort; invalid ids touch nothing.
pub fn release_monitor_lock(pane_id: &str, pid: u32) {
    if !valid_pane_id(pane_id) {
        eprintln!("release_monitor_lock: rejected invalid pane id {pane_id:?}");
        return;
    }
    remove_lock_file_if_names(&monitor_lock_path(pane_id), pane_id, pid);
}

/// Parse one lock file's contents: `Ok((pane_id, pid))`, or `Err(kind)`
/// with a short machine-readable reason (`invalid-json`, `missing-pid`
/// (also for a pid outside `u32`), `missing-pane-id`).
fn parse_lock_text(text: &str) -> Result<(String, u32), &'static str> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|_| "invalid-json")?;
    let pid = v
        .get("pid")
        .and_then(|p| p.as_u64())
        .and_then(|p| u32::try_from(p).ok())
        .ok_or("missing-pid")?;
    let pane_id = v
        .get("pane_id")
        .and_then(|p| p.as_str())
        .map(str::to_string)
        .ok_or("missing-pane-id")?;
    Ok((pane_id, pid))
}

/// One usable monitor lock file from a scan of the monitors dir.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LockFile {
    pub pane_id: String,
    pub pid: u32,
    /// The lock's file name in the monitors dir.
    pub file: String,
}

impl LockFile {
    /// True when THIS record's pid is the live monitor for its pane.
    pub fn is_live(&self) -> bool {
        pid_is_monitor_for_pane(self.pid, &self.pane_id)
    }

    /// Remove exactly this lock file, and only while it still names this record's pane and pid
    /// ([`remove_lock_file_if_names`]), so a lock a new monitor claimed
    /// after the scan judged this one stale is kept
    /// ([`LockRemoval::Changed`]).
    pub fn remove(&self) -> LockRemoval {
        remove_lock_file_if_names(&monitors_dir().join(&self.file), &self.pane_id, self.pid)
    }

    /// This lock file as it reads now (e.g. after
    /// [`LockRemoval::Changed`]). None when gone, unreadable or corrupt.
    pub fn reread(&self) -> Option<LockFile> {
        let text = std::fs::read_to_string(monitors_dir().join(&self.file)).ok()?;
        let (pane_id, pid) = parse_lock_text(&text).ok()?;
        Some(LockFile {
            pane_id,
            pid,
            file: self.file.clone(),
        })
    }
}

/// One unusable file from a scan of the monitors dir.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct BadLock {
    /// The file name in the monitors dir.
    pub file: String,
    /// Short machine-readable reason (e.g. `invalid-json`).
    pub reason: String,
    /// The parsed record of a parseable `unrecognised-name` lock under
    /// its pane's pre-v0.1.x `:` -> `_` name: its monitor may still be
    /// running and honours `stop-<pid>`, so `stop` can signal or clear
    /// it and a claim can yield to it. None for every other reason, and
    /// for any other mismatched name (e.g. a hand-renamed copy).
    pub lock: Option<LockFile>,
}

/// Usable locks, and unusable files.
pub type LockScan = (Vec<LockFile>, Vec<BadLock>);

/// All recorded monitor locks (live or stale), plus the file names (not
/// paths) of lock files that exist but cannot be used (unreadable,
/// corrupt JSON, missing fields, or a name that is not its pane's
/// encoded lock name, e.g. a pre-v0.1.x `:` -> `_` lock, which is no
/// longer recognised but keeps its parsed record, [`BadLock::lock`])
/// with the reason, from one pass over
/// the monitors dir (both lists sorted). Only regular `*.json` files
/// are lock candidates; anything else (dotfiles, editor swap files,
/// subdirectories) is ignored. Silent: callers report the unusable
/// files themselves.
pub fn scan_monitor_locks() -> LockScan {
    let entries = match std::fs::read_dir(monitors_dir()) {
        Ok(e) => e,
        Err(_) => return (vec![], vec![]),
    };
    let mut locks = Vec::new();
    let mut bad = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.')
            || !name.ends_with(".json")
            || !entry.file_type().is_ok_and(|t| t.is_file())
        {
            continue;
        }
        let path = entry.path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match parse_lock_text(&text) {
                Ok((pane_id, pid)) if name != format!("{}.json", encode_pane_id(&pane_id)) => {
                    // Only the pane's own pre-v0.1.x name keeps its record:
                    // a hand-renamed copy is no monitor's lock.
                    let legacy = legacy_lock_name(&pane_id).is_some_and(|l| l == name);
                    bad.push(BadLock {
                        file: name.clone(),
                        reason: "unrecognised-name".to_string(),
                        lock: legacy.then_some(LockFile {
                            pane_id,
                            pid,
                            file: name,
                        }),
                    })
                }
                Ok((pane_id, pid)) => locks.push(LockFile {
                    pane_id,
                    pid,
                    file: name,
                }),
                Err(kind) => bad.push(BadLock {
                    file: name,
                    reason: kind.to_string(),
                    lock: None,
                }),
            },
            Err(e) => bad.push(BadLock {
                file: name,
                reason: format!("unreadable ({e})"),
                lock: None,
            }),
        }
    }
    locks.sort();
    bad.sort();
    (locks, bad)
}

/// The pane's monitor lock as `(pid, locked pane id)`. None when absent,
/// unreadable or corrupt.
fn lock_record(pane_id: &str) -> Option<(u32, String)> {
    if !valid_pane_id(pane_id) {
        eprintln!("live_monitor_pid: rejected invalid pane id {pane_id:?}");
        return None;
    }
    let path = monitor_lock_path(pane_id);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!("live_monitor_pid: unreadable {}: {e}", path.display());
            return None;
        }
    };
    match parse_lock_text(&text) {
        Ok((locked_pane, pid)) => Some((pid, locked_pane)),
        Err(kind) => {
            eprintln!("live_monitor_pid: corrupt {}: {kind}", path.display());
            None
        }
    }
}

/// PID of the live monitor for a pane, or `None` when no lock exists, the
/// lock is unreadable, the lock names a different pane, or the recorded
/// process is not the monitor for this pane (guards against PID reuse:
/// monitors are spawned as `auto-resume monitor <pane-id>`, so the pane
/// id must appear as an argv token in the process cmdline). Falls back
/// to a live pre-v0.1.x monitor ([`live_legacy_monitor_pid`]).
pub fn live_monitor_pid(pane_id: &str) -> Option<u32> {
    lock_record(pane_id)
        .and_then(|(pid, locked_pane)| {
            (locked_pane == pane_id && pid_is_monitor_for_pane(pid, pane_id)).then_some(pid)
        })
        .or_else(|| live_legacy_monitor_pid(pane_id))
}

/// PID of a live pre-v0.1.x monitor for a pane: one whose lock sits
/// under the old `:` -> `_` name ([`BadLock::lock`]), names this pane
/// and whose pid is its monitor. Reads that one file only.
fn live_legacy_monitor_pid(pane_id: &str) -> Option<u32> {
    let pid = legacy_lock_pid(pane_id)?;
    pid_is_monitor_for_pane(pid, pane_id).then_some(pid)
}

/// The pre-v0.1.x lock file name for a pane (`:` -> `_`), unless the
/// pane id is invalid or the name is its current encoded one.
fn legacy_lock_name(pane_id: &str) -> Option<String> {
    let name = format!("{}.json", pane_id.replace(':', "_"));
    (valid_pane_id(pane_id) && name != format!("{}.json", encode_pane_id(pane_id))).then_some(name)
}

/// The pid of the pane's pre-v0.1.x lock ([`legacy_lock_name`]) when
/// it parses and names this pane. Reads that one file only.
fn legacy_lock_pid(pane_id: &str) -> Option<u32> {
    let path = monitors_dir().join(legacy_lock_name(pane_id)?);
    let (locked_pane, pid) = parse_lock_text(&std::fs::read_to_string(path).ok()?).ok()?;
    (locked_pane == pane_id).then_some(pid)
}

/// After a claim: delete the pane's dead pre-v0.1.x lock (its pid is
/// not the pane's live monitor) by compare-and-delete
/// ([`remove_lock_file_if_names`]), so it does not linger until `stop`.
fn remove_dead_legacy_lock(pane_id: &str) {
    let Some(pid) = legacy_lock_pid(pane_id).filter(|&p| !pid_is_monitor_for_pane(p, pane_id))
    else {
        return;
    };
    if let Some(name) = legacy_lock_name(pane_id) {
        remove_lock_file_if_names(&monitors_dir().join(name), pane_id, pid);
    }
}

/// Our executable's file name, resolved once per process (liveness
/// checks run every poll).
pub(crate) fn our_binary_marker() -> &'static str {
    static MARKER: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    MARKER.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "auto-resume".to_string())
    })
}

/// True when a raw cmdline (NUL- or whitespace-separated, e.g. `ps`
/// output with its trailing newline) belongs to the
/// monitor for `pane_id`: it names our binary and carries BOTH
/// `monitor` and the pane id as exact argv tokens (exact, so `w7G:p1`
/// never matches `w7G:p10`; the `monitor` token keeps e.g. a
/// `hook-pane` invocation for the same pane from matching).
/// Pure for testability; `pid_is_monitor_for_pane` feeds it `/proc` data.
fn cmdline_is_monitor_for_pane(cmdline: &str, pane_id: &str) -> bool {
    if !cmdline.contains(our_binary_marker()) {
        return false;
    }
    let mut has_monitor = false;
    let mut has_pane = false;
    for tok in cmdline
        .split(|c: char| c == '\0' || c.is_whitespace())
        .filter(|t| !t.is_empty())
    {
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
#[cfg(any(test, not(target_os = "linux")))]
fn ps_output_is_monitor(output: &str, pane_id: &str) -> bool {
    cmdline_is_monitor_for_pane(output, pane_id)
}

pub(crate) fn pid_is_monitor_for_pane(pid: u32, pane_id: &str) -> bool {
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

/// Size cap for `log.txt`; past it the log rotates to `log.txt.1`.
const LOG_MAX_BYTES: u64 = 1024 * 1024;

/// Append a timestamped line to `log.txt`, rotating it (one generation,
/// `log.txt.1`, overwritten) once it exceeds [`LOG_MAX_BYTES`].
/// Best-effort.
pub fn append_log(line: &str) {
    let dir = config::state_dir();
    std::fs::create_dir_all(&dir).ok();
    let secs = unix_secs();
    append_log_capped(&log_path(), &format!("[{secs}] {line}\n"), LOG_MAX_BYTES);
}

/// `<path>.1`, the single rotated log generation.
pub(crate) fn rotated_log_path(path: &Path) -> PathBuf {
    let mut rotated = path.as_os_str().to_owned();
    rotated.push(".1");
    PathBuf::from(rotated)
}

/// Append `entry` to `path`, first renaming `path` to `<path>.1` when it
/// already exceeds `max_bytes`. The rotation is serialized under
/// `state.lock` and the size re-checked once held, so two monitors
/// racing past the cap rotate once (a second rename would clobber
/// `<path>.1` with the fresh, tiny log); only the under-cap fast path
/// skips the lock. Rotation is best-effort and never waits: the lock is
/// tried once, and while another process holds it rotation is skipped
/// (a later append rotates), so logging never blocks a caller such as
/// `hook-pane`. Inside a [`with_state_lock_within`] scope it runs directly;
/// where locking is unsupported, unlocked.
fn append_log_capped(path: &Path, entry: &str, max_bytes: u64) {
    let over = || std::fs::metadata(path).is_ok_and(|m| m.len() > max_bytes);
    if over() {
        let rotate = || {
            if over() {
                std::fs::rename(path, rotated_log_path(path)).ok();
            }
        };
        if IN_LOCK_SCOPE.with(std::cell::Cell::get) {
            rotate();
        } else {
            match open_state_lock(std::time::Duration::ZERO) {
                Ok(_handle) => rotate(),
                Err(StateLockFail::Unsupported(_)) => rotate(),
                Err(StateLockFail::Busy) => {}
            }
        }
    }
    use std::io::Write;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(entry.as_bytes()))
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        clear_monitor_lock, forget, lock_env, monitor_lock_owned_by, record_shell, remember,
        unique_temp_dir, RemoveDir,
    };
    use std::path::PathBuf;

    /// Run `f` with plugin dirs pinned at a fresh temp dir (CONFIG) and
    /// STATE_DIR removed, restoring both and removing the dir afterwards
    /// (drop guards, so a panic in `f` cleans up too). Pinning both makes
    /// state resolution deterministic regardless of ambient env or other
    /// modules' tests (all env mutation shares one crate-wide lock).
    fn with_temp_state_dir(f: impl FnOnce(&PathBuf)) {
        let _guard = lock_env();
        let dir = unique_temp_dir("state-test");
        let _cleanup = RemoveDir(dir.clone());
        let _restore = crate::test_support::EnvRestore::capture([
            "HERDR_PLUGIN_CONFIG_DIR",
            "HERDR_PLUGIN_STATE_DIR",
        ]);
        std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", &dir);
        std::env::remove_var("HERDR_PLUGIN_STATE_DIR");
        f(&dir);
    }

    /// `(pane_id, pid)` of every usable lock file.
    fn lock_pairs() -> Vec<(String, u32)> {
        scan_monitor_locks()
            .0
            .into_iter()
            .map(|l| (l.pane_id, l.pid))
            .collect()
    }

    #[test]
    fn forget_removes_entry_and_rewrites() {
        with_temp_state_dir(|dir| {
            remember(
                "w7G:p1",
                SessionRef {
                    agent: "kiro".into(),
                    value: "sess-1".into(),
                },
            );
            remember(
                "w7G:p2",
                SessionRef {
                    agent: "claude".into(),
                    value: "sess-2".into(),
                },
            );
            forget("w7G:p1");
            let reg = load_registry();
            assert!(!reg.contains_key("w7G:p1"));
            assert_eq!(reg["w7G:p2"].value, "sess-2");
            assert!(dir.join("registry.json").exists());
        });
    }

    #[test]
    fn profiles_round_trip_per_pane() {
        with_temp_state_dir(|dir| {
            assert_eq!(profile_for("w7G:p1"), None);
            record_profile("w7G:p1", "personal");
            record_profile("w7G:p2", "work");
            record_profile("w7G:p1", "work");
            assert_eq!(profile_for("w7G:p1").as_deref(), Some("work"), "overwrite");
            assert_eq!(profile_for("w7G:p2").as_deref(), Some("work"));
            std::fs::write(dir.join("profiles.json"), "not json").expect("corrupt");
            assert_eq!(profile_for("w7G:p1"), None, "corrupt file reads as empty");
        });
    }

    #[test]
    fn shell_records_round_trip_per_pane() {
        with_temp_state_dir(|dir| {
            assert_eq!(shell_record("w7G:p1"), None);
            let live = ShellRecord {
                shell: "100@b:7".into(),
                live: true,
                argv_seen: None,
            };
            record_shell("w7G:p1", live.clone());
            record_shell(
                "w7G:p2",
                ShellRecord {
                    shell: "200".into(),
                    live: false,
                    argv_seen: None,
                },
            );
            assert_eq!(shell_record("w7G:p1"), Some(live));
            assert_eq!(shell_record("w7G:p2").map(|r| r.live), Some(false));
            std::fs::write(dir.join("shells.json"), "not json").expect("corrupt");
            assert_eq!(shell_record("w7G:p1"), None, "corrupt file reads as empty");
        });
    }

    #[test]
    fn undeserializable_entry_survives_other_panes_write() {
        // A bad entry (e.g. from a newer release) for pane X is skipped
        // on read but written back unchanged when pane Y is recorded,
        // and each bad key's skip is logged.
        with_temp_state_dir(|dir| {
            let bad = serde_json::json!({"shell": 7, "live": "yes"});
            std::fs::write(
                dir.join("shells.json"),
                serde_json::json!({"w7G:pX": bad, "w7G:pZ": 1}).to_string(),
            )
            .expect("seed");
            assert_eq!(shell_record("w7G:pX"), None, "skipped on read");
            let y = ShellRecord {
                shell: "200".into(),
                live: true,
                argv_seen: None,
            };
            assert!(record_shell("w7G:pY", y.clone()));
            let on_disk: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(dir.join("shells.json")).expect("read"),
            )
            .expect("json");
            assert_eq!(on_disk["w7G:pX"], bad, "kept unchanged");
            assert_eq!(shell_record("w7G:pY"), Some(y));
            let log = std::fs::read_to_string(dir.join("log.txt")).unwrap_or_default();
            for key in ["w7G:pX", "w7G:pZ"] {
                assert!(
                    log.lines()
                        .any(|l| l.contains(&format!("skipping bad entry for {key:?}"))),
                    "{log}"
                );
            }
            // Recording X itself replaces the raw entry.
            let x = ShellRecord {
                shell: "300".into(),
                live: false,
                argv_seen: None,
            };
            assert!(record_shell("w7G:pX", x.clone()));
            assert_eq!(shell_record("w7G:pX"), Some(x));
        });
    }

    #[test]
    fn forget_noop_without_write() {
        with_temp_state_dir(|dir| {
            forget("w7G:p9");
            assert!(
                !dir.join("registry.json").exists(),
                "forget of absent entry must not create a registry"
            );
        });
    }

    #[test]
    fn registry_round_trips() {
        with_temp_state_dir(|_| {
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
    }

    #[test]
    fn remember_skips_write_when_value_unchanged() {
        with_temp_state_dir(|dir| {
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
    }

    #[test]
    fn monitor_lock_names_sanitize_colon() {
        with_temp_state_dir(|_| {
            let p = monitor_lock_path("w7G:p1");
            assert_eq!(p.file_name().unwrap().to_string_lossy(), "w7G__c__p1.json");
        });
    }

    #[test]
    fn live_monitor_pid_rejects_dead_pid() {
        with_temp_state_dir(|_| {
            // Record a PID that almost certainly does not exist.
            write_monitor_lock_pid("w7G:p1", 2147483647);
            assert_eq!(live_monitor_pid("w7G:p1"), None);
        });
    }

    #[test]
    fn monitor_lock_embeds_pane_id() {
        with_temp_state_dir(|dir| {
            write_monitor_lock_pid("w7G:p1", 12345);
            let text =
                std::fs::read_to_string(dir.join("monitors/w7G__c__p1.json")).expect("lock exists");
            let v: serde_json::Value = serde_json::from_str(&text).expect("valid json");
            assert_eq!(v["pane_id"], "w7G:p1");
        });
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
        assert!(!cmdline_is_monitor_for_pane(
            "other-bin\0w7G:p1\0",
            "w7G:p1"
        ));
    }

    #[test]
    fn live_monitor_pid_rejects_monitor_for_other_pane() {
        with_temp_state_dir(|_| {
            // Our own process is not a `monitor w7G:p1` invocation, so
            // even our live pid is rejected for that pane now.
            write_monitor_lock_pid("w7G:p1", std::process::id());
            assert_eq!(live_monitor_pid("w7G:p1"), None);
            clear_monitor_lock("w7G:p1");
            assert_eq!(live_monitor_pid("w7G:p1"), None);
        });
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
        with_temp_state_dir(|dir| {
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
    }

    #[test]
    fn colon_and_underscore_map_to_different_files() {
        with_temp_state_dir(|_| {
            let a = monitor_lock_path("a:b");
            let b = monitor_lock_path("a_b");
            assert_ne!(a, b);
            assert_eq!(a.file_name().unwrap().to_string_lossy(), "a__c__b.json");
            assert_eq!(b.file_name().unwrap().to_string_lossy(), "a__u__b.json");
        });
    }

    #[test]
    fn monitor_lock_claim_reclaims_stale() {
        with_temp_state_dir(|_| {
            // No existing lock: claim succeeds.
            assert!(write_monitor_lock_pid("w7G:p1", 2147483647));
            // Existing lock names a dead pid: stale, so reclaim succeeds
            // and the file now carries the new pid.
            assert!(write_monitor_lock_pid("w7G:p1", 2147483646));
            assert_eq!(lock_pairs(), vec![("w7G:p1".to_string(), 2147483646)]);
        });
    }

    #[test]
    fn monitor_locks_lists_written_locks() {
        with_temp_state_dir(|_| {
            write_monitor_lock_pid("w7G:p1", 111);
            write_monitor_lock_pid("w7G:p2", 222);
            assert_eq!(
                lock_pairs(),
                vec![("w7G:p1".to_string(), 111), ("w7G:p2".to_string(), 222),]
            );
        });
    }

    #[test]
    fn unreadable_locks_reports_corrupt_files() {
        with_temp_state_dir(|dir| {
            write_monitor_lock_pid("w7G:p1", 111);
            std::fs::write(dir.join("monitors/corrupt.json"), "{oops").expect("write corrupt lock");
            assert!(scan_monitor_locks()
                .1
                .iter()
                .any(|b| b.file == "corrupt.json"));
            // Corrupt entry is skipped; the good one is still listed.
            assert_eq!(lock_pairs(), vec![("w7G:p1".to_string(), 111)]);
        });
    }

    #[test]
    fn scan_ignores_non_lock_entries() {
        with_temp_state_dir(|dir| {
            write_monitor_lock_pid("w7G:p1", 111);
            let m = dir.join("monitors");
            std::fs::write(m.join(".hidden.json"), "{oops").expect("dotfile");
            std::fs::write(m.join("w7G__c__p1.json.swp"), "{oops").expect("swap");
            std::fs::write(m.join("notes.txt"), "{oops").expect("txt");
            std::fs::create_dir(m.join("sub.json")).expect("subdir");
            let (locks, bad) = scan_monitor_locks();
            assert_eq!(bad, vec![], "non-lock entries are never reported");
            assert_eq!(locks.len(), 1);
            assert_eq!(lock_pairs(), vec![("w7G:p1".to_string(), 111)]);
        });
    }

    #[test]
    fn stop_sentinel_round_trips() {
        with_temp_state_dir(|_| {
            let pid = 424242u32;
            assert!(!stop_requested(pid));
            std::fs::write(stop_sentinel_path(pid), b"stop").expect("sentinel");
            assert!(stop_requested(pid));
            clear_stop_sentinel(pid);
            assert!(!stop_requested(pid));
        });
    }

    #[test]
    fn load_registry_returns_empty_on_corrupt_json() {
        with_temp_state_dir(|dir| {
            std::fs::write(dir.join("registry.json"), "{oops").expect("corrupt");
            assert!(load_registry().is_empty());
            std::fs::write(dir.join("registry.json"), "[1,2]").expect("wrong shape");
            assert!(load_registry().is_empty());
        });
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
        // Real `ps -o command=` output ends in a newline; tabs and runs
        // of spaces separate tokens too.
        assert!(ps_output_is_monitor(
            &format!("{m} monitor w7G:p1\n"),
            "w7G:p1"
        ));
        assert!(ps_output_is_monitor(
            &format!("{m}\tmonitor   w7G:p1 \n"),
            "w7G:p1"
        ));
        assert!(!ps_output_is_monitor(
            &format!("{m} monitor w7G:p10\n"),
            "w7G:p1"
        ));
    }

    #[test]
    fn append_log_writes_timestamped_line() {
        with_temp_state_dir(|dir| {
            append_log("hello");
            let text = std::fs::read_to_string(dir.join("log.txt")).expect("log exists");
            assert!(text.contains("hello"));
            assert!(text.starts_with('['));
        });
    }

    #[test]
    fn registry_auto_migrates_from_config_dir() {
        let _guard = lock_env();
        let src_dir = unique_temp_dir("state-test");
        let dst_dir = unique_temp_dir("state-test");
        let _cleanup = (RemoveDir(src_dir.clone()), RemoveDir(dst_dir.clone()));
        let _restore = crate::test_support::EnvRestore::capture([
            "HERDR_PLUGIN_CONFIG_DIR",
            "HERDR_PLUGIN_STATE_DIR",
        ]);
        std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", &src_dir);
        std::env::set_var("HERDR_PLUGIN_STATE_DIR", &dst_dir);
        let src_reg = src_dir.join("registry.json");
        std::fs::write(&src_reg, r#"{"w7G:p1":{"agent":"kiro","value":"sess-1"}}"#)
            .expect("legacy registry");
        assert!(!dst_dir.join("registry.json").exists());
        let reg = load_registry();
        assert_eq!(reg["w7G:p1"].value, "sess-1");
        assert!(dst_dir.join("registry.json").exists());
        assert!(
            !has_stray_tmp(&dst_dir),
            "atomic migration must not leave tmp behind"
        );
        // Second load is a no-op (target now present).
        let reg2 = load_registry();
        assert_eq!(reg2["w7G:p1"].value, "sess-1");
    }

    #[test]
    fn load_registry_reads_legacy_while_migration_is_busy() {
        let _guard = lock_env();
        let src_dir = unique_temp_dir("state-test");
        let dst_dir = unique_temp_dir("state-test");
        let _cleanup = (RemoveDir(src_dir.clone()), RemoveDir(dst_dir.clone()));
        let _restore = crate::test_support::EnvRestore::capture([
            "HERDR_PLUGIN_CONFIG_DIR",
            "HERDR_PLUGIN_STATE_DIR",
        ]);
        std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", &src_dir);
        std::env::set_var("HERDR_PLUGIN_STATE_DIR", &dst_dir);
        std::fs::write(
            src_dir.join("registry.json"),
            r#"{"w7G:p1":{"agent":"kiro","value":"sess-1"}}"#,
        )
        .expect("legacy registry");
        let held = std::fs::File::create(state_lock_path()).expect("lock file");
        held.lock().expect("hold the lock");
        let reg = load_registry_within(std::time::Duration::from_millis(50));
        assert_eq!(reg["w7G:p1"].value, "sess-1", "legacy read in place");
        assert!(!dst_dir.join("registry.json").exists(), "not migrated");
        drop(held);
        assert_eq!(load_registry()["w7G:p1"].value, "sess-1");
        assert!(dst_dir.join("registry.json").exists(), "migrated once free");
    }

    #[test]
    fn registry_never_migrates_after_a_quarantine() {
        let _guard = lock_env();
        let _restore = crate::test_support::EnvRestore::capture([
            "HERDR_PLUGIN_CONFIG_DIR",
            "HERDR_PLUGIN_STATE_DIR",
        ]);
        let src_dir = unique_temp_dir("state-test");
        let dst_dir = unique_temp_dir("state-test");
        std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", &src_dir);
        std::env::set_var("HERDR_PLUGIN_STATE_DIR", &dst_dir);
        std::fs::write(
            src_dir.join("registry.json"),
            r#"{"w7G:p1":{"agent":"kiro","value":"stale"}}"#,
        )
        .expect("legacy registry");
        // A corrupt state-dir registry is quarantined by a writer: the
        // registry stays present, so the legacy file is never migrated.
        std::fs::write(dst_dir.join("registry.json"), "{oops").expect("corrupt");
        let wrote = remember(
            "w7G:p2",
            SessionRef {
                agent: "claude".into(),
                value: "sess-2".into(),
            },
        );
        let present = dst_dir.join("registry.json").exists();
        let reg = load_registry();
        let read = read_registry();
        std::fs::remove_dir_all(&src_dir).ok();
        std::fs::remove_dir_all(&dst_dir).ok();
        assert!(wrote);
        assert!(present, "quarantine keeps registry.json present");
        assert_eq!(reg.keys().collect::<Vec<_>>(), ["w7G:p2"], "{reg:?}");
        assert_eq!(read, reg);
    }

    #[test]
    fn load_registry_keeps_good_entries_skips_bad() {
        with_temp_state_dir(|dir| {
            std::fs::write(
                dir.join("registry.json"),
                r#"{"good":{"agent":"kiro","value":"sess-1"},"bad":123}"#,
            )
            .expect("mixed registry");
            let reg = load_registry();
            assert_eq!(reg["good"].value, "sess-1");
            assert!(!reg.contains_key("bad"));
        });
    }

    #[test]
    fn remember_refuses_unsafe_session_values() {
        with_temp_state_dir(|dir| {
            remember(
                "w7G:p1",
                SessionRef {
                    agent: "claude".into(),
                    value: "bad;value".into(),
                },
            );
            assert!(
                load_registry().is_empty(),
                "unsafe value must not be persisted"
            );
            assert!(
                !dir.join("registry.json").exists(),
                "no registry file for refused write"
            );
            remember(
                "w7G:p1",
                SessionRef {
                    agent: "claude".into(),
                    value: "".into(),
                },
            );
            assert!(load_registry().is_empty());
            // Safe values still persist.
            remember(
                "w7G:p1",
                SessionRef {
                    agent: "claude".into(),
                    value: "sess-1".into(),
                },
            );
            assert_eq!(load_registry()["w7G:p1"].value, "sess-1");
        });
    }

    #[test]
    fn unreadable_locks_reports_missing_fields() {
        with_temp_state_dir(|dir| {
            std::fs::create_dir_all(dir.join("monitors")).expect("monitors dir");
            std::fs::write(
                dir.join("monitors/missing-pid.json"),
                r#"{"pane_id":"w7G:p1"}"#,
            )
            .expect("missing pid");
            std::fs::write(dir.join("monitors/missing-pane.json"), r#"{"pid":123}"#)
                .expect("missing pane_id");
            let (locks, bad) = scan_monitor_locks();
            assert!(bad.iter().any(|b| b.file == "missing-pid.json"));
            assert!(bad.iter().any(|b| b.file == "missing-pane.json"));
            assert!(locks.is_empty());
            // Fail-closed: neither resolves to a live pid.
            assert_eq!(live_monitor_pid("w7G:p1"), None);
        });
    }

    #[cfg(unix)]
    #[test]
    fn monitor_lock_claim_rejects_live_rival() {
        with_temp_state_dir(|_| {
            let pane = "w7G:p1";
            let mut child = crate::test_support::spawn_fake_monitor(pane);
            let pid = child.id();
            assert!(write_monitor_lock_pid(pane, pid));
            let before = std::fs::read_to_string(monitor_lock_path(pane)).expect("lock exists");
            // Second claim loses to the live rival; file untouched.
            assert!(!write_monitor_lock_pid(pane, 2147483647));
            let after = std::fs::read_to_string(monitor_lock_path(pane)).expect("lock exists");
            assert_eq!(before, after);
            let v: serde_json::Value = serde_json::from_str(&after).expect("valid");
            assert_eq!(v["pid"].as_u64().unwrap() as u32, pid);
            child.kill().ok();
            child.wait().ok();
        });
    }

    #[test]
    fn load_registry_top_level_corruption_returns_empty() {
        with_temp_state_dir(|dir| {
            std::fs::write(dir.join("registry.json"), "not-json{").expect("corrupt registry");
            // Must not panic; corrupt top-level JSON yields an empty map.
            let reg = load_registry();
            assert!(reg.is_empty());
        });
    }

    #[test]
    fn shells_and_profiles_keep_good_entries_skip_bad() {
        with_temp_state_dir(|dir| {
            std::fs::write(
                dir.join("shells.json"),
                r#"{"good":{"shell":"100","live":true},"bad":123}"#,
            )
            .expect("mixed shells");
            assert_eq!(shell_record("good").map(|r| r.shell), Some("100".into()));
            assert!(!load_shells().contains_key("bad"));
            std::fs::write(dir.join("profiles.json"), r#"{"good":"work","bad":[1]}"#)
                .expect("mixed profiles");
            assert_eq!(profile_for("good").as_deref(), Some("work"));
            assert_eq!(profile_for("bad"), None);
            // A write keeps the surviving good entry.
            assert!(record_shell(
                "other",
                ShellRecord {
                    shell: "200".into(),
                    live: false,
                    argv_seen: None
                }
            ));
            assert!(load_shells().contains_key("good"));
        });
    }

    /// True when any `*.tmp` staging file is left in `dir`.
    fn has_stray_tmp(dir: &Path) -> bool {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            })
            .unwrap_or(false)
    }

    #[test]
    fn tmp_name_is_unique_locked_or_not() {
        with_temp_state_dir(|dir| {
            let a = tmp_path_for(&dir.join("shells.json"));
            let b =
                with_state_lock_within(STATE_LOCK_WAIT, || tmp_path_for(&dir.join("shells.json")))
                    .expect("locked");
            assert_ne!(a, b, "each staging name is unique within a process");
            let prefix = format!("shells.json.stage-{}.", std::process::id());
            for tmp in [&a, &b] {
                let name = tmp.file_name().unwrap().to_string_lossy().into_owned();
                let n = name
                    .strip_prefix(&prefix)
                    .and_then(|r| r.strip_suffix(".tmp"))
                    .expect("<name>.stage-<pid>.<n>.tmp");
                assert!(n.parse::<u64>().is_ok(), "{name}");
            }
            // A legacy fixed tmp (an older release's) is never written to.
            std::fs::write(dir.join("shells.json.tmp"), "half-written").expect("orphan");
            assert!(record_shell(
                "w7G:p1",
                ShellRecord {
                    shell: "1".into(),
                    live: true,
                    argv_seen: None
                }
            ));
            assert_eq!(
                std::fs::read_to_string(dir.join("shells.json.tmp")).ok(),
                Some("half-written".into())
            );
            assert_eq!(shell_record("w7G:p1").map(|r| r.shell), Some("1".into()));
        });
    }

    #[test]
    fn sweep_removes_old_orphans_and_spares_fresh_or_foreign_files() {
        with_temp_state_dir(|dir| {
            let old =
                std::time::SystemTime::now() - ORPHAN_AGE - std::time::Duration::from_secs(60);
            let orphans = [
                "registry.json.tmp",
                "shells.json.tmp",
                "profiles.json.tmp",
                "shells.json.stage-123.4.tmp",
                "profiles.json.stage-123.5.tmp",
                "monitor-w7G__c__p1.stage-123.6.tmp",
            ];
            for name in orphans {
                let f = std::fs::File::create(dir.join(name)).expect("orphan");
                f.set_modified(old).expect("mtime");
            }
            // Fresh staging files (a write in flight) and old files that
            // are not ours stay.
            std::fs::write(dir.join("registry.json.stage-123.8.tmp"), "").expect("fresh");
            for name in ["notes.tmp", "registry.json.123.9.tmp", "a.stage-1.json"] {
                let foreign = std::fs::File::create(dir.join(name)).expect("foreign");
                foreign.set_modified(old).expect("mtime");
            }
            assert_eq!(sweep_orphans(), orphans.len());
            for name in orphans {
                assert!(!dir.join(name).exists(), "{name} swept");
            }
            for name in [
                "registry.json.stage-123.8.tmp",
                "notes.tmp",
                "registry.json.123.9.tmp",
                "a.stage-1.json",
            ] {
                assert!(dir.join(name).exists(), "{name} kept");
            }
        });
    }

    #[test]
    fn pre_encoding_lock_name_is_unrecognised() {
        // A pre-v0.1.x `:` -> `_` lock is listed as unusable (with its
        // parsed record) and never resolved; with its pid dead the pane
        // can still be claimed, which deletes it.
        with_temp_state_dir(|dir| {
            let legacy = dir.join("monitors/w1_p1.json");
            std::fs::create_dir_all(legacy.parent().unwrap()).expect("monitors dir");
            std::fs::write(&legacy, r#"{"pid":2147483646,"pane_id":"w1:p1"}"#).expect("legacy");
            let (locks, bad) = scan_monitor_locks();
            assert!(locks.is_empty(), "{locks:?}");
            assert_eq!(
                bad,
                vec![BadLock {
                    file: "w1_p1.json".to_string(),
                    reason: "unrecognised-name".to_string(),
                    lock: Some(LockFile {
                        pane_id: "w1:p1".to_string(),
                        pid: 2147483646,
                        file: "w1_p1.json".to_string(),
                    }),
                }]
            );
            assert_eq!(lock_record("w1:p1"), None);
            assert!(write_monitor_lock_pid("w1:p1", 2147483647));
            assert!(!legacy.exists(), "dead legacy lock removed on claim");
            assert_eq!(lock_pairs(), vec![("w1:p1".to_string(), 2147483647)]);
        });
    }

    #[test]
    fn claim_keeps_a_legacy_name_naming_another_pane() {
        // Compare-and-delete: only a legacy file naming this pane goes.
        with_temp_state_dir(|dir| {
            let legacy = dir.join("monitors/w1_p1.json");
            std::fs::create_dir_all(legacy.parent().unwrap()).expect("monitors dir");
            std::fs::write(&legacy, r#"{"pid":2147483646,"pane_id":"w9:p9"}"#).expect("legacy");
            assert!(write_monitor_lock_pid("w1:p1", 2147483647));
            assert!(legacy.exists());
        });
    }

    #[test]
    fn renamed_lock_copy_is_not_legacy() {
        // A hand-renamed lock is unusable without a record: `stop`
        // leaves it alone.
        with_temp_state_dir(|dir| {
            let copy = dir.join("monitors/w1_p1-copy.json");
            std::fs::create_dir_all(copy.parent().unwrap()).expect("monitors dir");
            std::fs::write(&copy, r#"{"pid":2147483646,"pane_id":"w1:p1"}"#).expect("copy");
            let (locks, bad) = scan_monitor_locks();
            assert!(locks.is_empty(), "{locks:?}");
            assert_eq!(
                bad,
                vec![BadLock {
                    file: "w1_p1-copy.json".to_string(),
                    reason: "unrecognised-name".to_string(),
                    lock: None,
                }]
            );
        });
    }

    #[test]
    fn monitor_lock_claim_leaves_no_staging_tmp() {
        with_temp_state_dir(|dir| {
            let pane = "w1:p1";
            // Fresh claim (hard link) and stale reclaim (rename).
            assert!(write_monitor_lock_pid(pane, 2147483646));
            assert!(lock_file_names(&monitor_lock_path(pane), pane, 2147483646));
            assert!(write_monitor_lock_pid(pane, 2147483647), "stale reclaim");
            assert!(lock_file_names(&monitor_lock_path(pane), pane, 2147483647));
            let tmps: Vec<_> = std::fs::read_dir(dir)
                .expect("state dir")
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".tmp"))
                .collect();
            assert!(tmps.is_empty(), "{tmps:?}");
        });
    }

    #[test]
    fn lock_remove_skips_a_lock_whose_pid_changed_after_the_scan() {
        with_temp_state_dir(|_| {
            let pane = "w1:p1";
            assert!(write_monitor_lock_pid(pane, 2147483647));
            let locks = scan_monitor_locks().0;
            assert_eq!(locks.len(), 1);
            assert!(!locks[0].is_live());
            // A new monitor claims the pane between the scan and the
            // removal: the stale verdict no longer applies.
            assert!(write_monitor_lock_pid(pane, 2147483646));
            assert!(
                matches!(locks[0].remove(), LockRemoval::Changed),
                "changed lock kept"
            );
            assert!(monitor_lock_owned_by(pane, 2147483646));
            // Unchanged, it is removed.
            let locks = scan_monitor_locks().0;
            assert!(matches!(locks[0].remove(), LockRemoval::Removed));
            assert!(!monitor_lock_path(pane).exists());
        });
    }

    #[test]
    fn lock_remove_reports_failure_apart_from_change() {
        with_temp_state_dir(|dir| {
            // A "lock" that cannot be read (a directory) is a failure,
            // not a changed lock.
            std::fs::create_dir_all(dir.join("monitors/w1__c__p1.json")).expect("dir");
            let lock = LockFile {
                pane_id: "w1:p1".into(),
                pid: 2147483647,
                file: "w1__c__p1.json".into(),
            };
            assert!(matches!(lock.remove(), LockRemoval::Failed(_)));
            assert!(dir.join("monitors/w1__c__p1.json").exists());
            // Already gone counts as removed.
            std::fs::remove_dir(dir.join("monitors/w1__c__p1.json")).expect("rm");
            assert!(matches!(lock.remove(), LockRemoval::Removed));
        });
    }

    #[test]
    fn release_monitor_lock_deletes_only_our_own_lock() {
        with_temp_state_dir(|_| {
            let pane = "w1:p1";
            assert!(write_monitor_lock_pid(pane, 2147483647));
            release_monitor_lock(pane, 2147483646);
            assert!(
                monitor_lock_owned_by(pane, 2147483647),
                "other pid's lock kept"
            );
            release_monitor_lock(pane, 2147483647);
            assert!(!monitor_lock_path(pane).exists(), "own lock released");
        });
    }

    /// A live pre-v0.1.x monitor (lock under the old `:` -> `_` name)
    /// blocks a new claim of its pane, and retires an owner whose
    /// encoded lock went missing; other panes are unaffected.
    #[cfg(unix)]
    #[test]
    fn live_pre_encoding_monitor_blocks_claim() {
        with_temp_state_dir(|dir| {
            let mut child = crate::test_support::spawn_fake_monitor("w1:p1");
            let legacy = dir.join("monitors/w1_p1.json");
            std::fs::create_dir_all(legacy.parent().unwrap()).expect("monitors dir");
            std::fs::write(
                &legacy,
                format!(r#"{{"pid":{},"pane_id":"w1:p1"}}"#, child.id()),
            )
            .expect("legacy");
            assert_eq!(live_monitor_pid("w1:p1"), Some(child.id()));
            assert!(!write_monitor_lock_pid("w1:p1", 2147483647));
            assert!(!monitor_lock_path("w1:p1").exists());
            assert_eq!(
                hold_monitor_lock("w1:p1", 2147483647),
                LockHold::Lost(child.id())
            );
            assert!(write_monitor_lock_pid("w1:p2", 2147483647));
            child.kill().ok();
            child.wait().ok();
            assert!(
                write_monitor_lock_pid("w1:p1", 2147483647),
                "dead: claimable"
            );
        });
    }

    #[test]
    fn parse_lock_text_rejects_out_of_range_pid() {
        assert_eq!(
            parse_lock_text(r#"{"pid":4294967297,"pane_id":"w1:p1"}"#),
            Err("missing-pid")
        );
        assert_eq!(
            parse_lock_text(r#"{"pid":4294967295,"pane_id":"w1:p1"}"#),
            Ok(("w1:p1".to_string(), u32::MAX))
        );
    }

    #[cfg(unix)]
    #[test]
    fn hold_monitor_lock_reclaims_missing_or_stale_but_yields_to_live() {
        with_temp_state_dir(|_| {
            let pane = "w1:p1";
            let own = 2147483647;
            // Missing → re-claimed; then ours.
            assert_eq!(hold_monitor_lock(pane, own), LockHold::Reclaimed);
            assert_eq!(hold_monitor_lock(pane, own), LockHold::Ours);
            // Dead other pid, or corrupt → re-claimed.
            clear_monitor_lock(pane);
            assert!(write_monitor_lock_pid(pane, 2147483646));
            assert_eq!(hold_monitor_lock(pane, own), LockHold::Reclaimed);
            std::fs::write(monitor_lock_path(pane), "{oops").expect("corrupt");
            assert_eq!(hold_monitor_lock(pane, own), LockHold::Reclaimed);
            assert!(monitor_lock_owned_by(pane, own));
            // A live monitor for the pane keeps its lock.
            clear_monitor_lock(pane);
            let mut child = crate::test_support::spawn_fake_monitor(pane);
            assert!(write_monitor_lock_pid(pane, child.id()));
            assert_eq!(hold_monitor_lock(pane, own), LockHold::Lost(child.id()));
            assert!(monitor_lock_owned_by(pane, child.id()));
            // A failed re-claim with no live rival is no loss.
            clear_monitor_lock(pane);
            let monitors = monitor_lock_path(pane).parent().unwrap().to_path_buf();
            std::fs::remove_dir_all(&monitors).expect("rm monitors");
            std::fs::write(&monitors, "not a dir").expect("block monitors");
            assert_eq!(hold_monitor_lock(pane, own), LockHold::Failed);
            std::fs::remove_file(&monitors).ok();
            child.kill().ok();
            child.wait().ok();
        });
    }

    #[test]
    fn concurrent_writers_lose_no_updates() {
        with_temp_state_dir(|_| {
            const N: usize = 16;
            let handles: Vec<_> = (0..N)
                .map(|i| {
                    std::thread::spawn(move || {
                        let pane = format!("w7G:p{i}");
                        assert!(record_shell(
                            &pane,
                            ShellRecord {
                                shell: format!("{i}"),
                                live: true,
                                argv_seen: None
                            }
                        ));
                        assert!(record_profile(&pane, "work"));
                        assert!(remember(
                            &pane,
                            SessionRef {
                                agent: "claude".into(),
                                value: format!("sess-{i}")
                            }
                        ));
                    })
                })
                .collect();
            for h in handles {
                h.join().expect("writer thread");
            }
            let shells = load_shells();
            let reg = load_registry();
            for i in 0..N {
                let pane = format!("w7G:p{i}");
                assert_eq!(shells[&pane].shell, format!("{i}"));
                assert_eq!(profile_for(&pane).as_deref(), Some("work"));
                assert_eq!(reg[&pane].value, format!("sess-{i}"));
            }
        });
    }

    #[test]
    fn writers_report_failure() {
        with_temp_state_dir(|dir| {
            // A directory squatting on the target makes the rename fail.
            std::fs::create_dir_all(dir.join("shells.json")).expect("squat");
            assert!(!record_shell(
                "w7G:p1",
                ShellRecord {
                    shell: "1".into(),
                    live: true,
                    argv_seen: None
                }
            ));
            std::fs::create_dir_all(dir.join("profiles.json")).expect("squat");
            assert!(!record_profile("w7G:p1", "work"));
            std::fs::create_dir_all(dir.join("registry.json")).expect("squat");
            assert!(!remember(
                "w7G:p1",
                SessionRef {
                    agent: "kiro".into(),
                    value: "sess-1".into()
                }
            ));
            assert!(!has_stray_tmp(dir));
            let log = std::fs::read_to_string(dir.join("log.txt")).expect("logged");
            assert!(log.contains("record_shell"));
        });
    }

    /// The `<name>.corrupt-*` quarantine copies under `dir`.
    fn quarantined(dir: &Path, name: &str) -> Vec<PathBuf> {
        let prefix = format!("{name}.corrupt-");
        let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
            .expect("read dir")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix))
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn writers_quarantine_a_corrupt_map() {
        let session = |v: &str| SessionRef {
            agent: "claude".into(),
            value: v.into(),
        };
        let bads: [&[u8]; 3] = [b"{oops", b"[1,2]", b"{\"\xff\": 1}"];
        for bad in bads {
            with_temp_state_dir(|dir| {
                std::fs::write(dir.join("registry.json"), bad).expect("corrupt");
                assert!(remember("w7G:p1", session("sess-1")));
                let q = quarantined(dir, "registry.json");
                assert_eq!(q.len(), 1, "{q:?}");
                assert_eq!(std::fs::read(&q[0]).expect("quarantine"), bad);
                let reg = read_registry();
                assert_eq!(reg.len(), 1);
                assert_eq!(reg["w7G:p1"].value, "sess-1");
                // The new file is valid: no further quarantine.
                assert!(remember("w7G:p2", session("sess-2")));
                assert_eq!(quarantined(dir, "registry.json").len(), 1);
                assert_eq!(read_registry().len(), 2);
                let log = std::fs::read_to_string(dir.join("log.txt")).expect("logged");
                assert_eq!(log.matches("copied aside to").count(), 1, "{log}");
                assert!(log.contains(&q[0].display().to_string()), "{log}");
            });
        }
        with_temp_state_dir(|dir| {
            std::fs::write(dir.join("shells.json"), "{oops").expect("corrupt");
            let rec = ShellRecord {
                shell: "1".into(),
                live: true,
                argv_seen: None,
            };
            assert!(record_shell("w7G:p1", rec.clone()));
            assert_eq!(quarantined(dir, "shells.json").len(), 1);
            assert_eq!(shell_record("w7G:p1"), Some(rec.clone()));
            // A second quarantine within the same second gets its own
            // `<secs>.<pid>.<n>` name: the first is never overwritten.
            std::fs::write(dir.join("shells.json"), "{again").expect("corrupt");
            assert!(record_shell("w7G:p1", rec));
            let q = quarantined(dir, "shells.json");
            assert_eq!(q.len(), 2, "{q:?}");
            let mut bytes: Vec<Vec<u8>> = q.iter().map(|p| std::fs::read(p).expect("q")).collect();
            bytes.sort();
            assert_eq!(bytes, vec![b"{again".to_vec(), b"{oops".to_vec()]);
        });
    }

    #[test]
    fn writers_never_overwrite_an_unreadable_map() {
        with_temp_state_dir(|dir| {
            // A directory in the registry's place: an I/O error, not
            // corruption — never quarantined, never overwritten.
            std::fs::create_dir_all(dir.join("registry.json")).expect("dir");
            let session = SessionRef {
                agent: "claude".into(),
                value: "sess-1".into(),
            };
            assert!(!remember("w7G:p1", session.clone()));
            assert!(!remember("w7G:p1", session));
            assert!(dir.join("registry.json").is_dir());
            assert!(quarantined(dir, "registry.json").is_empty());
            let log = std::fs::read_to_string(dir.join("log.txt")).expect("logged");
            assert_eq!(log.matches("not writing").count(), 1, "throttled: {log}");
        });
    }

    #[test]
    fn state_lock_wait_is_bounded() {
        with_temp_state_dir(|_| {
            let path = state_lock_path();
            std::fs::create_dir_all(path.parent().unwrap()).expect("dir");
            let held = std::fs::File::create(&path).expect("lock file");
            held.lock().expect("hold the lock");
            let start = std::time::Instant::now();
            let ran = with_state_lock_within(std::time::Duration::from_millis(100), || ());
            assert_eq!(ran, None, "a held lock skips `f` once the wait elapses");
            assert!(start.elapsed() < std::time::Duration::from_secs(2));
            // Each skip reaches log.txt, at most once a minute.
            with_state_lock_within(std::time::Duration::from_millis(10), || ());
            let log = std::fs::read_to_string(log_path()).expect("logged");
            assert_eq!(log.matches("skipped write").count(), 1, "{log}");
        });
    }

    #[test]
    fn busy_lock_never_rotates_the_log() {
        // The busy skip is logged outside the lock scope: with the log
        // past its cap, rotation is skipped (the holder may be rotating)
        // and the line lands in the current log.
        with_temp_state_dir(|dir| {
            let held = std::fs::File::create(state_lock_path()).expect("lock file");
            held.lock().expect("hold the lock");
            let big = vec![b'x'; LOG_MAX_BYTES as usize + 1];
            std::fs::write(log_path(), &big).expect("seed log");
            let start = std::time::Instant::now();
            assert_eq!(
                with_state_lock_within(std::time::Duration::from_millis(10), || ()),
                None
            );
            assert!(start.elapsed() < std::time::Duration::from_secs(2));
            assert!(!dir.join("log.txt.1").exists(), "rotated while busy");
            let log = std::fs::read(log_path()).expect("log");
            assert!(log.len() > big.len(), "busy skip still logged");
            drop(held);
            append_log("free");
            assert!(dir.join("log.txt.1").exists(), "rotates once free");
        });
    }

    #[test]
    fn write_json_atomic_writes_nothing_on_serialize_error() {
        struct Unserializable;
        impl Serialize for Unserializable {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("nope"))
            }
        }
        with_temp_state_dir(|dir| {
            let path = dir.join("map.json");
            assert!(!write_json_atomic(&path, &Unserializable, "test"));
            assert!(!path.exists(), "nothing written");
            assert!(!has_stray_tmp(dir));
            let log = std::fs::read_to_string(log_path()).expect("logged");
            assert!(log.contains("serializing"), "{log}");
        });
    }

    #[test]
    fn unsupported_state_lock_runs_unlocked() {
        with_temp_state_dir(|_| {
            // A directory in the lock's place: the open fails outright
            // (as on a filesystem without locks), so `f` runs unlocked.
            std::fs::create_dir_all(state_lock_path()).expect("dir");
            let sess = SessionRef {
                agent: "opencode".into(),
                value: "s1".into(),
            };
            assert!(remember("w7G:p1", sess));
            assert_eq!(
                load_registry().get("w7G:p1").map(|s| s.value.as_str()),
                Some("s1")
            );
            let log = std::fs::read_to_string(log_path()).expect("logged");
            assert_eq!(log.matches("proceeding unlocked").count(), 1, "{log}");
        });
    }

    #[test]
    fn remember_within_skips_a_busy_lock() {
        with_temp_state_dir(|_| {
            let old = SessionRef {
                agent: "opencode".into(),
                value: "s0".into(),
            };
            assert!(remember("w7G:p1", old));
            let before = std::fs::read(registry_path()).expect("registry");
            let path = state_lock_path();
            let held = std::fs::File::create(&path).expect("lock file");
            held.lock().expect("hold the lock");
            let sess = SessionRef {
                agent: "opencode".into(),
                value: "s1".into(),
            };
            let start = std::time::Instant::now();
            let wait = std::time::Duration::from_millis(100);
            assert!(
                !remember_with_snapshot_within("w7G:p1", sess, &HashMap::new(), wait),
                "not persisted"
            );
            assert!(start.elapsed() < std::time::Duration::from_secs(2));
            assert_eq!(std::fs::read(registry_path()).expect("registry"), before);
        });
    }

    #[test]
    fn writers_return_true_on_noop() {
        with_temp_state_dir(|_| {
            assert!(forget("w7G:p9"), "absent entry is a successful no-op");
            let rec = ShellRecord {
                shell: "1".into(),
                live: true,
                argv_seen: None,
            };
            assert!(record_shell("w7G:p1", rec.clone()));
            assert!(record_shell("w7G:p1", rec));
            assert!(record_profile("w7G:p1", "work"));
            assert!(record_profile("w7G:p1", "work"));
        });
    }

    #[test]
    fn read_registry_reads_legacy_without_migrating() {
        let _guard = lock_env();
        let _restore = crate::test_support::EnvRestore::capture([
            "HERDR_PLUGIN_CONFIG_DIR",
            "HERDR_PLUGIN_STATE_DIR",
        ]);
        let src_dir = unique_temp_dir("state-test");
        let dst_dir = unique_temp_dir("state-test");
        std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", &src_dir);
        std::env::set_var("HERDR_PLUGIN_STATE_DIR", &dst_dir);
        std::fs::write(
            src_dir.join("registry.json"),
            r#"{"w7G:p1":{"agent":"kiro","value":"sess-1"}}"#,
        )
        .expect("legacy registry");
        assert_eq!(read_registry()["w7G:p1"].value, "sess-1");
        assert_eq!(
            std::fs::read_dir(&dst_dir).expect("state dir").count(),
            0,
            "read_registry must write nothing"
        );
        // Once the state-dir registry exists it wins over legacy.
        std::fs::write(
            dst_dir.join("registry.json"),
            r#"{"w7G:p2":{"agent":"claude","value":"sess-2"}}"#,
        )
        .expect("state registry");
        let reg = read_registry();
        assert_eq!(reg["w7G:p2"].value, "sess-2");
        assert!(!reg.contains_key("w7G:p1"));
        // Missing state dir entirely: still nothing created.
        std::fs::remove_dir_all(&dst_dir).expect("remove state dir");
        assert_eq!(read_registry()["w7G:p1"].value, "sess-1");
        assert!(!dst_dir.exists());
        std::fs::remove_dir_all(&src_dir).ok();
    }

    #[test]
    fn lock_reread_confirms_only_stored_pid() {
        with_temp_state_dir(|_| {
            let pane = "w7G:p1";
            assert!(write_monitor_lock_pid(pane, 2147483647));
            let path = monitor_lock_path(pane);
            assert!(lock_file_names(&path, pane, 2147483647));
            assert!(
                !lock_file_names(&path, pane, 2147483646),
                "foreign pid loses"
            );
            assert!(!lock_file_names(&path, "w7G:p2", 2147483647), "other pane");
            assert!(monitor_lock_owned_by(pane, 2147483647));
            assert!(!monitor_lock_owned_by(pane, 2147483646));
            // A stale reclaim hands ownership over; the old claimer's
            // per-poll re-check now fails.
            assert!(write_monitor_lock_pid(pane, 2147483646));
            assert!(monitor_lock_owned_by(pane, 2147483646));
            assert!(!monitor_lock_owned_by(pane, 2147483647));
            // Corrupt, missing, or invalid ids are never owned.
            std::fs::write(&path, "{oops").expect("corrupt");
            assert!(!monitor_lock_owned_by(pane, 2147483646));
            clear_monitor_lock(pane);
            assert!(!monitor_lock_owned_by(pane, 2147483646));
            assert!(!monitor_lock_owned_by("../../evil", 1));
        });
    }

    #[test]
    fn reclaim_leaves_no_tmp_in_monitors_dir() {
        with_temp_state_dir(|dir| {
            assert!(write_monitor_lock_pid("w7G:p1", 2147483647));
            assert!(write_monitor_lock_pid("w7G:p1", 2147483646));
            assert!(scan_monitor_locks().1.is_empty());
            assert!(!has_stray_tmp(dir), "reclaim tmp must be renamed away");
        });
    }

    #[test]
    fn concurrent_rotation_keeps_rotated_generation() {
        // Many writers cross the cap at once: exactly one rotation, so
        // `log.txt.1` keeps the old log instead of a clobbered stub. A
        // writer that finds the lock busy skips rotation, so its line
        // may land in either generation, but none is lost.
        with_temp_state_dir(|dir| {
            let path = dir.join("log.txt");
            let original = "old-line\n".repeat(64);
            std::fs::write(&path, &original).expect("seed log");
            let cap = original.len() as u64 - 1;
            let handles: Vec<_> = (0..16)
                .map(|i| {
                    let path = path.clone();
                    std::thread::spawn(move || {
                        append_log_capped(&path, &format!("new-{i}\n"), cap);
                    })
                })
                .collect();
            for h in handles {
                h.join().expect("writer");
            }
            let rotated = std::fs::read_to_string(dir.join("log.txt.1")).unwrap();
            assert!(rotated.starts_with(&original), "{rotated}");
            let fresh = std::fs::read_to_string(&path).unwrap();
            let new_lines = rotated.lines().count() - 64 + fresh.lines().count();
            assert_eq!(new_lines, 16, "{rotated}{fresh}");
            assert!(fresh.lines().all(|l| l.starts_with("new-")), "{fresh}");
        });
    }

    #[test]
    fn unchanged_writes_skip_state_lock() {
        // With `state.lock` held elsewhere, unchanged writes must still
        // return at once: the fast path never touches the lock.
        with_temp_state_dir(|dir| {
            let pane = "w7G:p1";
            let rec = ShellRecord {
                shell: "1".into(),
                live: true,
                argv_seen: None,
            };
            let sess = SessionRef {
                agent: "kiro".into(),
                value: "sess-1".into(),
            };
            assert!(record_shell(pane, rec.clone()));
            assert!(record_profile(pane, "work"));
            assert!(remember(pane, sess.clone()));
            let held = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(dir.join("state.lock"))
                .expect("open lock");
            held.lock().expect("hold lock");
            let state_dir = config::state_dir();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                // Same resolution as the caller (env is pinned by the
                // crate-wide lock the caller holds).
                assert_eq!(config::state_dir(), state_dir);
                let ok = record_shell(pane, rec)
                    && record_profile(pane, "work")
                    && remember(pane, sess)
                    && forget("w7G:p9");
                tx.send(ok).ok();
            });
            let got = rx.recv_timeout(std::time::Duration::from_secs(5));
            drop(held);
            assert_eq!(got, Ok(true), "unchanged writes blocked on state.lock");
        });
    }

    #[test]
    fn append_log_rotates_past_cap() {
        with_temp_state_dir(|dir| {
            let path = dir.join("log.txt");
            append_log_capped(&path, "first\n", 8);
            append_log_capped(&path, "second\n", 8);
            // 13 bytes > 8: the next append rotates.
            append_log_capped(&path, "third\n", 8);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "third\n");
            assert_eq!(
                std::fs::read_to_string(dir.join("log.txt.1")).unwrap(),
                "first\nsecond\n"
            );
            // Rotation overwrites the previous generation.
            append_log_capped(&path, "fourth-is-long\n", 8);
            append_log_capped(&path, "fifth\n", 8);
            assert_eq!(
                std::fs::read_to_string(dir.join("log.txt.1")).unwrap(),
                "third\nfourth-is-long\n"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "fifth\n");
        });
    }

    #[test]
    fn binary_marker_is_cached() {
        assert!(std::ptr::eq(our_binary_marker(), our_binary_marker()));
    }
}
