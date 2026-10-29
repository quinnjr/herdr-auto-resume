mod claude_profile;
mod config;
mod herdr;
mod kiro;
mod monitor;
mod resume;
mod state;
#[cfg(test)]
mod test_support;

use std::collections::HashMap;
use std::process::Stdio;

/// A pane is worth a monitor when it has a live agent, or when the
/// registry already holds a session ref for it (a dead agent's pane that
/// we know how to relaunch). Plain shells with no history are skipped.
fn wants_monitor(pane: &herdr::Pane, registry: &HashMap<String, herdr::SessionRef>) -> bool {
    if pane.agent.is_some() {
        return true;
    }
    registry.contains_key(&pane.pane_id)
}

/// Spawn `auto-resume monitor <pane-id>` detached: stdio to /dev/null so
/// the child outlives the plugin invocation. NOTE: no `setsid`/double
/// fork — monitors die with the user session. Acceptable v1 (same as
/// prior art); a restart re-spawns them via `startup`.
/// Returns true on successful spawn, false on failure (logged).
fn spawn_monitor(pane_id: &str) -> bool {
    if !state::valid_pane_id(pane_id) {
        eprintln!("auto-resume: spawn monitor for {pane_id:?} refused: invalid pane id");
        return false;
    }
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("auto-resume: spawn monitor for {pane_id} failed: current_exe: {e}");
            return false;
        }
    };
    match std::process::Command::new(exe)
        .arg("monitor")
        .arg(pane_id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(_) => true,
        Err(e) => {
            eprintln!("auto-resume: spawn monitor for {pane_id} failed: {e}");
            false
        }
    }
}

/// Ensure exactly one live monitor for `pane_id`: skip when one is
/// already live (checked via lock + /proc argv), else spawn.
/// Returns true when a live monitor exists or spawn succeeded.
fn ensure_monitor(pane_id: &str) -> bool {
    if !state::valid_pane_id(pane_id) {
        eprintln!("auto-resume: ensure monitor for {pane_id:?} refused: invalid pane id");
        return false;
    }
    if state::live_monitor_pid(pane_id).is_some() {
        return true;
    }
    spawn_monitor(pane_id)
}

/// Scan all panes and ensure a monitor for each supervisable one.
/// Spawns polling monitors only — no `pane run`, no pane mutation, so
/// this is safe to run with live agents doing real work. First sweeps
/// orphaned staging files from the state dir ([`state::sweep_orphans`]).
/// Returns the count of failed spawns.
fn supervise_all() -> u32 {
    state::sweep_orphans();
    let reg = state::load_registry();
    let mut failed = 0u32;
    let panes = match herdr::pane_list_checked() {
        Some(panes) => panes,
        None => {
            eprintln!("auto-resume: supervise-all: pane list failed");
            println!("auto-resume: supervise-all done (1 failed: pane list unavailable)");
            return 1;
        }
    };
    for pane in panes {
        if !state::valid_pane_id(&pane.pane_id) {
            eprintln!(
                "auto-resume: supervise-all: skipping invalid pane id {:?}",
                pane.pane_id
            );
            continue;
        }
        if !wants_monitor(&pane, &reg) {
            continue;
        }
        if !ensure_monitor(&pane.pane_id) {
            failed += 1;
        }
    }
    println!("auto-resume: supervise-all done ({failed} failed)");
    failed
}

/// Extract a pane id from `HERDR_PLUGIN_EVENT_JSON` (`{"data":{"pane_id":...}}`).
/// Pure helper so the shape is unit-testable; returns None on any drift.
fn event_pane_id(raw: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    // data.pane_id wins when it is a non-empty string; a present-but-wrong
    // type (e.g. number) or an empty string falls through to the top-level
    // pane_id instead of shadowing it with None. Empty ids are rejected.
    let data_id = v
        .get("data")
        .and_then(|d| d.get("pane_id"))
        .and_then(|id| id.as_str())
        .filter(|s| !s.is_empty());
    let top_id = v
        .get("pane_id")
        .and_then(|id| id.as_str())
        .filter(|s| !s.is_empty());
    data_id.or(top_id).map(str::to_string)
}

/// `hook-pane`: refresh one pane (pane id is best-effort: argv[1], then
/// `HERDR_PANE_ID`, then `HERDR_PLUGIN_EVENT_JSON`, else fall back to a
/// full scan). Records a live `agent_session` into the registry and
/// ensures a monitor ([`wants_monitor`]).
/// Returns the count of failed spawns (0 on the single-pane path unless
/// ensure_monitor failed).
fn hook_pane(arg: Option<&str>) -> u32 {
    // Empty values count as unset at every source, so an exported-but-
    // empty HERDR_PANE_ID never shadows the event JSON pane id.
    let pane_id = arg
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .or_else(|| config::env_string("HERDR_PANE_ID"))
        .or_else(|| {
            config::env_string("HERDR_PLUGIN_EVENT_JSON").and_then(|raw| event_pane_id(&raw))
        })
        .unwrap_or_default();
    if pane_id.is_empty() {
        eprintln!("auto-resume: hook-pane without pane id; full scan");
        return supervise_all();
    }
    if !state::valid_pane_id(&pane_id) {
        eprintln!("auto-resume: hook-pane: rejected invalid pane id {pane_id:?}");
        return 0;
    }
    if let Some(pane) = herdr::pane_get(&pane_id) {
        // Pure read: this snapshot never migrates (and so never waits
        // out the state lock for it). The remember write below may
        // perform the one-shot migration, under the lock within its
        // bounded wait (below).
        let mut reg = state::read_registry();
        if let Some(sess) = pane.agent_session.clone() {
            // Short lock wait: herdr runs this hook synchronously. A
            // still-busy lock skips the write (false); an agent pane's
            // monitor records the session on its next poll. An agentless
            // pane's monitor never does, and only the recorded session
            // earns it a monitor, so that rare case retries once with
            // the normal (~5s) wait rather than lose the session.
            let wait = std::time::Duration::from_millis(250);
            let saved = state::remember_with_snapshot_within(&pane_id, sess.clone(), &reg, wait)
                || (pane.agent.is_none()
                    && resume::is_safe_session_value(&sess.value)
                    && state::remember_with_snapshot(&pane_id, sess.clone(), &reg));
            if saved {
                reg.insert(pane_id.clone(), sess);
            }
        }
        if wants_monitor(&pane, &reg) && !ensure_monitor(&pane_id) {
            eprintln!("auto-resume: hook-pane {pane_id}: monitor spawn failed");
            println!("auto-resume: hook-pane {pane_id} done (1 failed)");
            return 1;
        }
        println!("auto-resume: hook-pane {pane_id} ok");
        0
    } else {
        eprintln!("auto-resume: hook-pane: unknown pane {pane_id}");
        0
    }
}

/// `status`: read-only. Panes with agent/status, registry presence, and
/// monitor liveness. Touches nothing (`read_registry` never migrates).
fn status() {
    for line in status_lines() {
        println!("{line}");
    }
}

/// The lines `status` prints, built without side effects.
fn status_lines() -> Vec<String> {
    let panes = herdr::pane_list();
    let reg = state::read_registry();
    // Liveness is probed once per lock file and reused for both the
    // per-pane rows and the stale-lock listing. A pane with no usable lock has no live monitor.
    // One silent scan: unusable lock files are listed below on stdout.
    let (usable, unreadable) = state::scan_monitor_locks();
    let locks: Vec<(state::LockFile, bool)> = usable
        .into_iter()
        .map(|lock| {
            let live = lock.is_live();
            (lock, live)
        })
        .collect();
    // A live pre-v0.1.x monitor (its lock under the old name, listed
    // as unreadable below) still is the pane's monitor.
    let legacy: Vec<state::LockFile> = unreadable
        .iter()
        .filter_map(|b| b.lock.clone())
        .filter(state::LockFile::is_live)
        .collect();
    let live_pid = |pane_id: &str| {
        locks
            .iter()
            .find(|(lock, live)| *live && lock.pane_id == pane_id)
            .map(|(lock, _)| lock)
            .or_else(|| legacy.iter().find(|lock| lock.pane_id == pane_id))
            .map(|lock| lock.pid)
    };
    let mut out = Vec::new();
    let mut live = 0u32;
    for pane in &panes {
        let agent = pane.agent.as_deref().unwrap_or("-");
        let st = pane.agent_status.as_deref().unwrap_or("-");
        let known = if reg.contains_key(&pane.pane_id) {
            "session=known"
        } else {
            "session=-"
        };
        let mon = match live_pid(&pane.pane_id) {
            Some(pid) => {
                live += 1;
                format!("monitor=live({pid})")
            }
            None => "monitor=-".to_string(),
        };
        out.push(format!(
            "pane {} agent={agent} status={st} {known} {mon}",
            pane.pane_id
        ));
    }
    // Stale locks (recorded pid no longer a monitor) and unreadable or
    // corrupt lock files are listed so drift is visible without extra
    // probing.
    let mut stale = 0usize;
    for (lock, live) in &locks {
        if !live {
            stale += 1;
            out.push(format!("stale lock pane {} pid={}", lock.pane_id, lock.pid));
        }
    }
    for bad in &unreadable {
        out.push(format!("unreadable lock {}", bad.file));
    }
    out.push(format!(
        "auto-resume: {} panes, {live} live monitors, {stale} stale locks, {} unreadable locks",
        panes.len(),
        unreadable.len()
    ));
    out
}

/// `stop`: write a `stop-<pid>` sentinel per live monitor lock. The
/// monitor loop sees it on its next poll, clears its lock + sentinel,
/// and exits. Stale lock files (their own pid is no live monitor for
/// the pane) are removed instead of signalled — only that file, never
/// the same pane's live lock, and only while it still names the stale
/// pid ([`state::LockFile::remove`]); a live monitor that reclaimed it
/// meanwhile is signalled instead ([`stop_lock`]). Never kills, closes, or touches
/// panes. Returns the count of failures (sentinel writes, and stale
/// locks that could not be removed). A pre-v0.1.x lock under its old
/// `:` -> `_` name ([`state::BadLock::lock`]) is handled the same way:
/// its live monitor is signalled (old monitors honour `stop-<pid>`
/// too), a dead one's file removed. Other unreadable/corrupt lock files
/// are skipped and left in place — no sentinel can be addressed to
/// them — so they are reported, not counted as failures.
fn stop() -> u32 {
    let (mut locks, bad) = state::scan_monitor_locks();
    let mut skipped = 0;
    for b in bad {
        match b.lock {
            Some(legacy) => locks.push(legacy),
            None => {
                skipped += 1;
                eprintln!(
                    "stop: skipping {}: {}",
                    state::monitors_dir().join(&b.file).display(),
                    b.reason
                );
            }
        }
    }
    if locks.is_empty() && skipped == 0 {
        println!("auto-resume: no monitors recorded");
        return 0;
    }
    let failed = locks.iter().map(stop_lock).sum();
    println!("{}", stop_summary(failed, skipped));
    failed
}

/// `stop` for one scanned lock; returns its failure count. A lock a
/// new monitor claimed after the scan judged it stale
/// ([`state::LockRemoval::Changed`]) is re-read: a live monitor for the
/// same pane is stopped too, anything else is kept untouched.
fn stop_lock(lock: &state::LockFile) -> u32 {
    let (pane_id, pid) = (&lock.pane_id, &lock.pid);
    if lock.is_live() {
        return request_stop(pane_id, *pid);
    }
    match lock.remove() {
        state::LockRemoval::Removed => {
            println!("auto-resume: cleared stale lock for {pane_id} (pid {pid})");
            0
        }
        state::LockRemoval::Changed => match lock
            .reread()
            .filter(|new| &new.pane_id == pane_id && new.is_live())
        {
            Some(new) => request_stop(pane_id, new.pid),
            None => {
                println!("auto-resume: kept lock for {pane_id}: no longer pid {pid}");
                0
            }
        },
        state::LockRemoval::Failed(e) => {
            eprintln!("auto-resume: clearing stale lock for {pane_id} (pid {pid}) failed: {e}");
            1
        }
    }
}

/// Write the stop sentinel for monitor `pid`; returns its failure count.
fn request_stop(pane_id: &str, pid: u32) -> u32 {
    let path = state::stop_sentinel_path(pid);
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("auto-resume: stop failed for {pane_id} (pid {pid}): create_dir_all: {e}");
            return 1;
        }
    }
    match std::fs::write(&path, "") {
        Ok(()) => {
            println!("auto-resume: stop requested for {pane_id} (pid {pid})");
            0
        }
        Err(e) => {
            eprintln!("auto-resume: stop failed for {pane_id} (pid {pid}): {e}");
            1
        }
    }
}

/// `stop`'s closing line: the failure count, plus any unreadable lock
/// files it left in place.
fn stop_summary(failed: u32, unreadable: usize) -> String {
    let mut line = format!("auto-resume: stop done ({failed} failed)");
    if unreadable > 0 {
        line.push_str(&format!(
            "; {unreadable} unreadable lock files left in place (see status)"
        ));
    }
    line
}

/// Default `logs` line count.
const DEFAULT_LOG_LINES: usize = 50;

/// `logs [n]` line count: `n` when it parses, else the default.
fn log_line_count(arg: Option<&str>) -> usize {
    arg.and_then(|a| a.parse().ok())
        .unwrap_or(DEFAULT_LOG_LINES)
}

/// Last `n` lines of the file at `path` (empty when missing). Reads a
/// bounded trailing chunk, doubling it until it holds `n` complete lines
/// or reaches the start, so a large log is never read whole.
fn tail_lines(path: &std::path::Path, n: usize) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    if n == 0 {
        return Vec::new();
    }
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let len = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return Vec::new(),
    };
    let mut chunk: u64 = 8 * 1024;
    loop {
        let size = chunk.min(len);
        let start = len - size;
        let mut buf = Vec::with_capacity(size as usize);
        if file.seek(SeekFrom::Start(start)).is_err()
            || file.by_ref().take(size).read_to_end(&mut buf).is_err()
        {
            return Vec::new();
        }
        let text = String::from_utf8_lossy(&buf);
        let mut lines: Vec<&str> = text.lines().collect();
        if start > 0 && !lines.is_empty() {
            // The first line may be cut mid-way by the chunk boundary.
            lines.remove(0);
        }
        if lines.len() >= n || start == 0 {
            let skip = lines.len().saturating_sub(n);
            return lines[skip..].iter().map(|l| l.to_string()).collect();
        }
        chunk = chunk.saturating_mul(2);
    }
}

/// Last `n` log lines across rotation: the tail of `path`, topped up
/// from the front with the tail of its rotated generation
/// (`<path>.1`) when `path` alone holds fewer than `n`.
fn log_tail(path: &std::path::Path, n: usize) -> Vec<String> {
    let mut lines = tail_lines(path, n);
    if lines.len() < n {
        let mut older = tail_lines(&state::rotated_log_path(path), n - lines.len());
        older.append(&mut lines);
        lines = older;
    }
    lines
}

/// `logs`: print the tail of `log.txt` (default 50 lines), reaching
/// into `log.txt.1` when a fresh rotation left too few lines.
fn logs(arg: Option<&str>) {
    let path = config::state_dir().join("log.txt");
    for line in log_tail(&path, log_line_count(arg)) {
        println!("{line}");
    }
}

/// Report `config.json` problems once per `startup` / `supervise-all`:
/// on stderr (seen when run by hand) and in `log.txt`. Monitors and
/// `hook-pane` stay silent ([`config::load`]).
fn report_config_warnings() {
    for w in config::load().1 {
        eprintln!("auto-resume: {w}");
        state::append_log(&format!("WARNING {w}"));
    }
}

/// Fail closed before a known command touches state: everything `run`
/// dispatches relies on `config::state_dir()`, which panics when the dir
/// is unresolvable (`logs`/`status` fail too: nothing to read). `run`
/// checks this for every recognized command, after the usage check.
fn check_state_dir() -> Result<(), &'static str> {
    if config::try_state_dir().is_none() {
        return Err(
            "auto-resume: cannot resolve state dir (HOME unset); set HERDR_PLUGIN_STATE_DIR",
        );
    }
    Ok(())
}

/// Dispatch one CLI invocation (`args` excludes argv[0]) and return the
/// process exit code: 0 success, 1 failed spawns/sentinel writes, 2
/// usage errors (an unresolvable state dir included).
fn run(args: &[String]) -> i32 {
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    // Select the handler first: an unknown command is a usage error;
    // every recognized one then fails closed on the state dir alike.
    let handler: fn(&[String]) -> i32 = match cmd {
        // `monitor <pane-id>` carries the pane id as an argv token: the
        // liveness check in state.rs matches it against /proc cmdlines.
        "monitor" => |args| match args.get(1) {
            Some(id) if state::valid_pane_id(id) => {
                monitor::run(id);
                0
            }
            Some(id) => {
                eprintln!("auto-resume: monitor: rejected invalid pane id {id:?}");
                2
            }
            None => {
                eprintln!("usage: auto-resume monitor <pane-id>");
                2
            }
        },
        "startup" | "supervise-all" => |_| {
            report_config_warnings();
            i32::from(supervise_all() > 0)
        },
        "hook-pane" => |args| i32::from(hook_pane(args.get(1).map(String::as_str)) > 0),
        "status" => |_| {
            status();
            0
        },
        "stop" => |_| i32::from(stop() > 0),
        "logs" => |args| {
            logs(args.get(1).map(String::as_str));
            0
        },
        _ => {
            eprintln!(
                "usage: auto-resume <startup|hook-pane|supervise-all|status|stop|logs|monitor>"
            );
            return 2;
        }
    };
    if let Err(msg) = check_state_dir() {
        eprintln!("{msg}");
        return 2;
    }
    handler(args)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = run(&args);
    if code != 0 {
        std::process::exit(code);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sess() -> herdr::SessionRef {
        herdr::SessionRef {
            agent: "kiro".into(),
            value: "sess-1".into(),
        }
    }

    #[test]
    fn wants_monitor_true_when_agent_present() {
        let pane = herdr::Pane {
            pane_id: "w1:p1".into(),
            agent: Some("kiro".into()),
            ..Default::default()
        };
        let reg: HashMap<String, herdr::SessionRef> = HashMap::new();
        assert!(wants_monitor(&pane, &reg));
    }

    #[test]
    fn wants_monitor_true_when_agentless_with_registry_ref() {
        let pane = herdr::Pane {
            pane_id: "w1:p2".into(),
            agent: None,
            ..Default::default()
        };
        let mut reg: HashMap<String, herdr::SessionRef> = HashMap::new();
        reg.insert("w1:p2".into(), sess());
        assert!(wants_monitor(&pane, &reg));
    }

    #[test]
    fn wants_monitor_false_when_agentless_session_unrecorded() {
        // The monitor of an agentless pane never records its session:
        // only the recorded ref (hook-pane's write) earns a monitor.
        let pane = herdr::Pane {
            pane_id: "w1:p4".into(),
            agent: None,
            agent_session: Some(sess()),
            ..Default::default()
        };
        let reg: HashMap<String, herdr::SessionRef> = HashMap::new();
        assert!(!wants_monitor(&pane, &reg));
    }

    #[test]
    fn wants_monitor_false_for_plain_shell_without_ref() {
        let pane = herdr::Pane {
            pane_id: "w1:p3".into(),
            agent: None,
            ..Default::default()
        };
        let reg: HashMap<String, herdr::SessionRef> = HashMap::new();
        assert!(!wants_monitor(&pane, &reg));
    }

    #[test]
    fn event_pane_id_parses_live_event_json() {
        let raw = r#"{"event":"pane_agent_status_changed","data":{"type":"pane_agent_status_changed","pane_id":"w7G:p1","workspace_id":"w7G","agent_status":"done","agent":"kiro"}}"#;
        assert_eq!(event_pane_id(raw).as_deref(), Some("w7G:p1"));
    }

    #[test]
    fn event_pane_id_rejects_garbage() {
        assert_eq!(event_pane_id("not json"), None);
        assert_eq!(event_pane_id(r#"{"event":"x"}"#), None);
    }

    // ---- review-fix guards (hermetic: fake herdr + temp state dir) ----
    use crate::test_support::{herdr_calls, run_with_fake_herdr};

    /// Fake `herdr`: logs argv to @CALL_LOG@, serves agentless panes for
    /// w1:argv / w1:evt, a live-agent pane with session for w9:live, an
    /// agentless pane carrying a session for w9:sess, null-result
    /// (unknown pane) otherwise, and an empty pane list.
    const FAKE_HERDR: &str = r#"#!/bin/sh
echo "$@" >> "@CALL_LOG@"
if [ "$1" = "pane" ]; then
  if [ "$2" = "get" ]; then
    case "$3" in
      w1:argv) echo '{"id":"x","result":{"pane":{"pane_id":"w1:argv"}}}' ;;
      w1:evt) echo '{"id":"x","result":{"pane":{"pane_id":"w1:evt"}}}' ;;
      w9:live) echo '{"id":"x","result":{"pane":{"pane_id":"w9:live","agent":"kiro","agent_status":"working","agent_session":{"agent":"kiro","value":"sess-live"}}}}' ;;
      w9:sess) echo '{"id":"x","result":{"pane":{"pane_id":"w9:sess","agent_session":{"agent":"claude","value":"sess-only"}}}}' ;;
      *) echo '{"id":"x","result":null}' ;;
    esac
  elif [ "$2" = "list" ]; then
    echo '{"id":"x","result":{"panes":[]}}'
  else
    echo '{"id":"x","result":null}'
  fi
else
  echo '{"id":"x","result":null}'
fi
exit 0
"#;

    /// Run `f` with HERDR_BIN_PATH faked (default `FAKE_HERDR` script),
    /// state/config dirs isolated to a fresh temp dir, and `extra` env
    /// vars set (`Some`) or removed (`None`); everything is restored
    /// afterwards. Serialized on the crate-wide `lock_env`.
    fn run_isolated(extra: &[(&str, Option<&str>)], f: impl FnOnce(&std::path::Path)) {
        run_with_fake_herdr(FAKE_HERDR, extra, f);
    }

    #[test]
    fn hook_pane_prefers_argv_over_env() {
        run_isolated(
            &[
                ("HERDR_PANE_ID", Some("w1:unknown")),
                ("HERDR_PLUGIN_EVENT_JSON", None),
            ],
            |dir| {
                assert_eq!(hook_pane(Some("w1:argv")), 0);
                let calls = herdr_calls(dir);
                assert!(
                    calls.contains("pane get w1:argv"),
                    "argv id must be fetched, got: {calls}"
                );
                assert!(
                    !calls.contains("w1:unknown"),
                    "env id must not be fetched, got: {calls}"
                );
            },
        );
    }

    #[test]
    fn hook_pane_reads_event_json() {
        run_isolated(
            &[
                ("HERDR_PANE_ID", None),
                (
                    "HERDR_PLUGIN_EVENT_JSON",
                    Some(r#"{"data":{"pane_id":"w1:evt"}}"#),
                ),
            ],
            |dir| {
                assert_eq!(hook_pane(None), 0);
                let calls = herdr_calls(dir);
                assert!(
                    calls.contains("pane get w1:evt"),
                    "event pane id must be fetched, got: {calls}"
                );
            },
        );
    }

    #[test]
    fn hook_pane_full_scans_on_empty_id() {
        run_isolated(
            &[("HERDR_PANE_ID", None), ("HERDR_PLUGIN_EVENT_JSON", None)],
            |dir| {
                assert_eq!(hook_pane(None), 0);
                let calls = herdr_calls(dir);
                assert!(
                    calls.contains("pane list"),
                    "empty id must fall back to full scan, got: {calls}"
                );
            },
        );
    }

    #[test]
    fn hook_pane_remembers_live_session() {
        run_isolated(
            &[("HERDR_PANE_ID", None), ("HERDR_PLUGIN_EVENT_JSON", None)],
            |_| {
                // w9:live carries agent + agent_session, so hook_pane takes
                // the spawn path. The child is a detached copy of this test
                // binary (libtest treats its args as filters and exits on
                // its own); spawn itself is not waited on, so this is fast.
                assert_eq!(hook_pane(Some("w9:live")), 0);
                let reg = state::load_registry();
                assert_eq!(
                    reg.get("w9:live").map(|s| s.value.as_str()),
                    Some("sess-live")
                );
            },
        );
    }

    #[test]
    fn hook_pane_busy_lock_retries_agentless_session() {
        run_isolated(
            &[("HERDR_PANE_ID", None), ("HERDR_PLUGIN_EVENT_JSON", None)],
            |dir| {
                // state.lock held past the 250ms wait: the agentless
                // pane's session is retried with the normal wait, so it
                // is recorded once the holder lets go, and wants a monitor.
                let held = std::fs::File::create(dir.join("state.lock")).expect("lock file");
                held.lock().expect("hold the lock");
                let holder = std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(600));
                    drop(held);
                });
                assert_eq!(hook_pane(Some("w9:sess")), 0);
                holder.join().expect("holder");
                let reg = state::read_registry();
                assert!(reg.contains_key("w9:sess"), "retried write: {reg:?}");
                let pane = herdr::pane_get("w9:sess").expect("pane");
                assert_eq!(pane.agent, None);
                assert!(wants_monitor(&pane, &reg));
            },
        );
    }

    #[test]
    fn hook_pane_busy_lock_skips_agent_pane_write_quickly() {
        run_isolated(
            &[("HERDR_PANE_ID", None), ("HERDR_PLUGIN_EVENT_JSON", None)],
            |dir| {
                // An agent pane's monitor records the session itself, so
                // a busy lock costs the hook only the 250ms wait.
                let held = std::fs::File::create(dir.join("state.lock")).expect("lock file");
                held.lock().expect("hold the lock");
                let start = std::time::Instant::now();
                assert_eq!(hook_pane(Some("w9:live")), 0);
                assert!(start.elapsed() < std::time::Duration::from_secs(2));
                assert!(!state::read_registry().contains_key("w9:live"));
            },
        );
    }

    #[test]
    fn stop_zero_when_no_locks() {
        run_isolated(&[], |_| {
            assert_eq!(stop(), 0);
        });
    }

    #[test]
    fn stop_reports_unreadable_locks_without_failing() {
        run_isolated(&[], |dir| {
            let monitors = dir.join("monitors");
            std::fs::create_dir_all(&monitors).expect("monitors dir");
            let corrupt = monitors.join("w1__c__p1.json");
            std::fs::write(&corrupt, "not json").expect("corrupt lock");
            assert_eq!(state::scan_monitor_locks().1.len(), 1);
            assert_eq!(stop(), 0, "an unreadable lock is not a stop failure");
            assert!(corrupt.exists(), "left in place for status");
        });
        assert_eq!(stop_summary(0, 0), "auto-resume: stop done (0 failed)");
        assert_eq!(
            stop_summary(1, 2),
            "auto-resume: stop done (1 failed); 2 unreadable lock files left in place (see status)"
        );
    }

    #[test]
    fn stop_clears_stale_lock_without_sentinel() {
        run_isolated(&[], |_| {
            state::write_monitor_lock_pid("w1:p1", 2147483647);
            assert_eq!(stop(), 0);
            assert!(
                state::scan_monitor_locks().0.is_empty(),
                "stale lock must be cleared"
            );
            assert!(
                !state::stop_sentinel_path(2147483647).exists(),
                "no sentinel for a stale lock"
            );
        });
    }

    /// A stale lock that cannot be removed (its dir is read-only) is a
    /// failure, not a "kept lock … no longer pid N".
    #[cfg(unix)]
    #[test]
    fn stop_counts_a_failed_stale_lock_removal() {
        use std::os::unix::fs::PermissionsExt;
        run_isolated(&[], |dir| {
            assert!(state::write_monitor_lock_pid("w1:p1", 2147483647));
            let monitors = dir.join("monitors");
            std::fs::set_permissions(&monitors, std::fs::Permissions::from_mode(0o555))
                .expect("chmod");
            // Root ignores directory permissions: nothing to observe.
            let enforced = std::fs::write(monitors.join("probe"), "").is_err();
            let code = stop();
            std::fs::set_permissions(&monitors, std::fs::Permissions::from_mode(0o755))
                .expect("chmod back");
            if enforced {
                assert_eq!(code, 1);
                assert!(state::monitor_lock_path("w1:p1").exists());
            }
        });
    }

    #[test]
    fn supervise_all_logs_bad_config_fields_once() {
        // A skipped config field reaches log.txt from `supervise-all`
        // (once), never from `hook-pane`.
        run_isolated(
            &[("HERDR_PANE_ID", None), ("HERDR_PLUGIN_EVENT_JSON", None)],
            |dir| {
                std::fs::write(dir.join("config.json"), r#"{"poll_seconds":"10"}"#)
                    .expect("config");
                let warned = || {
                    std::fs::read_to_string(dir.join("log.txt"))
                        .unwrap_or_default()
                        .lines()
                        .filter(|l| l.contains("WARNING") && l.contains("ignoring poll_seconds"))
                        .count()
                };
                assert_eq!(hook_pane(None), 0);
                assert_eq!(warned(), 0, "hook-pane is silent");
                assert_eq!(run(&["supervise-all".to_string()]), 0);
                assert_eq!(warned(), 1);
            },
        );
    }

    #[test]
    fn supervise_all_zero_panes_returns_zero() {
        run_isolated(&[], |dir| {
            assert_eq!(supervise_all(), 0);
            assert!(herdr_calls(dir).contains("pane list"));
        });
    }

    /// Fake `herdr` that fails `pane list` (non-zero exit): supervise_all
    /// must report failure instead of a false-success zero.
    const FAILING_HERDR: &str = r#"#!/bin/sh
echo "$@" >> "@CALL_LOG@"
echo '{"id":"x","error":"boom"}'
exit 1
"#;

    #[test]
    fn supervise_all_reports_herdr_failure() {
        run_with_fake_herdr(FAILING_HERDR, &[], |dir| {
            assert_eq!(supervise_all(), 1);
            assert!(herdr_calls(dir).contains("pane list"));
        });
    }

    /// Fake `herdr` serving an agentless pane list containing one invalid
    /// id: supervise_all must skip it without ever passing it to herdr,
    /// and the agentless valid pane (no registry ref) is skipped too.
    const EVIL_LIST_HERDR: &str = r#"#!/bin/sh
echo "$@" >> "@CALL_LOG@"
if [ "$1" = "pane" ] && [ "$2" = "list" ]; then
  echo '{"id":"x","result":{"panes":[{"pane_id":"../../evil"},{"pane_id":"w1:p1"}]}}'
else
  echo '{"id":"x","result":null}'
fi
exit 0
"#;

    #[test]
    fn supervise_all_skips_invalid_pane_id_without_touching_it() {
        run_with_fake_herdr(EVIL_LIST_HERDR, &[], |dir| {
            assert_eq!(supervise_all(), 0);
            let calls = herdr_calls(dir);
            assert!(calls.contains("pane list"), "must list panes, got: {calls}");
            assert!(
                !calls.contains("../../evil"),
                "invalid id must never reach herdr, got: {calls}"
            );
        });
    }

    #[test]
    fn hook_pane_rejects_invalid_pane_id() {
        run_isolated(&[], |dir| {
            assert_eq!(hook_pane(Some("../../evil")), 0);
            assert!(
                !dir.join("calls.log").exists(),
                "invalid id must not touch herdr"
            );
        });
    }

    #[test]
    fn invalid_id_spawn_refused() {
        run_isolated(&[], |dir| {
            assert!(!spawn_monitor("../../evil"));
            assert!(
                !dir.join("calls.log").exists(),
                "invalid id must not touch herdr"
            );
        });
    }

    #[test]
    fn invalid_id_ensure_refused() {
        run_isolated(&[], |_| {
            assert!(!ensure_monitor("../../evil"));
        });
    }

    #[test]
    fn event_pane_id_numeric_data_falls_back_to_top_level() {
        assert_eq!(
            event_pane_id(r#"{"data":{"pane_id":123},"pane_id":"w1:p9"}"#).as_deref(),
            Some("w1:p9")
        );
    }

    #[test]
    fn event_pane_id_rejects_empty_ids() {
        assert_eq!(event_pane_id(r#"{"data":{"pane_id":""}}"#), None);
        assert_eq!(event_pane_id(r#"{"pane_id":""}"#), None);
        assert_eq!(
            event_pane_id(r#"{"data":{"pane_id":""},"pane_id":""}"#),
            None
        );
    }

    #[test]
    fn event_pane_id_top_level_happy_path() {
        assert_eq!(
            event_pane_id(r#"{"pane_id":"w1:top"}"#).as_deref(),
            Some("w1:top")
        );
    }

    #[test]
    fn event_pane_id_data_empty_falls_back_to_top_level() {
        assert_eq!(
            event_pane_id(r#"{"data":{"pane_id":""},"pane_id":"w1:p9"}"#).as_deref(),
            Some("w1:p9")
        );
    }

    #[test]
    fn hook_pane_empty_env_id_falls_through_to_event_json() {
        run_isolated(
            &[
                ("HERDR_PANE_ID", Some("")),
                (
                    "HERDR_PLUGIN_EVENT_JSON",
                    Some(r#"{"data":{"pane_id":"w1:evt"}}"#),
                ),
            ],
            |dir| {
                assert_eq!(hook_pane(None), 0);
                let calls = herdr_calls(dir);
                assert!(
                    calls.contains("pane get w1:evt"),
                    "empty env id must not shadow event JSON, got: {calls}"
                );
                assert!(!calls.contains("pane list"), "no full scan, got: {calls}");
            },
        );
    }

    #[test]
    fn status_reads_legacy_registry_without_writing_state() {
        let state_dir = crate::test_support::unique_temp_dir("status-test");
        let state_str = state_dir.to_string_lossy().into_owned();
        run_isolated(
            &[("HERDR_PLUGIN_STATE_DIR", Some(&state_str))],
            |config_dir| {
                std::fs::write(
                    config_dir.join("registry.json"),
                    r#"{"w1:p1":{"agent":"kiro","value":"sess-1"},"w1:p2":42}"#,
                )
                .expect("write legacy registry");
                // The bad entry is skipped without logging to disk.
                let reg = state::read_registry();
                assert!(reg.contains_key("w1:p1") && !reg.contains_key("w1:p2"));
                let lines = status_lines();
                assert!(lines.last().is_some_and(|l| l.contains("0 panes")));
                let written: Vec<_> = std::fs::read_dir(&state_dir)
                    .expect("read state dir")
                    .flatten()
                    .map(|e| e.file_name())
                    .collect();
                assert!(
                    written.is_empty(),
                    "status must not write, got: {written:?}"
                );
            },
        );
        std::fs::remove_dir_all(&state_dir).ok();
    }

    /// A dead pre-v0.1.x `:` -> `_` lock beside the pane's live lock:
    /// listed as unreadable; `stop` removes it and signals the live pid
    /// only.
    #[cfg(unix)]
    #[test]
    fn pre_encoding_lock_beside_live_lock_is_listed_unreadable() {
        run_isolated(&[], |dir| {
            let mut child = crate::test_support::spawn_fake_monitor("w1:p1");
            let pid = child.id();
            assert!(state::write_monitor_lock_pid("w1:p1", pid));
            let legacy = dir.join("monitors/w1_p1.json");
            std::fs::write(&legacy, r#"{"pid":2147483647,"pane_id":"w1:p1"}"#).expect("legacy");
            let lines = status_lines();
            assert!(
                lines.iter().any(|l| l == "unreadable lock w1_p1.json"),
                "{lines:?}"
            );
            assert_eq!(stop(), 0);
            assert!(!legacy.exists(), "dead legacy lock removed");
            assert!(state::monitor_lock_path("w1:p1").exists(), "live lock kept");
            assert!(
                state::stop_sentinel_path(pid).exists(),
                "live pid signalled"
            );
            assert!(!state::stop_sentinel_path(2147483647).exists());
            child.kill().ok();
            child.wait().ok();
        });
    }

    /// A live pre-v0.1.x monitor (lock under the old name only) is
    /// signalled by `stop`; its lock is left for it to release.
    #[cfg(unix)]
    #[test]
    fn stop_signals_live_pre_encoding_monitor() {
        run_isolated(&[], |dir| {
            let mut child = crate::test_support::spawn_fake_monitor("w1:p1");
            let pid = child.id();
            let legacy = dir.join("monitors/w1_p1.json");
            std::fs::create_dir_all(legacy.parent().unwrap()).expect("monitors dir");
            std::fs::write(&legacy, format!(r#"{{"pid":{pid},"pane_id":"w1:p1"}}"#))
                .expect("legacy");
            assert_eq!(stop(), 0);
            child.kill().ok();
            child.wait().ok();
            assert!(legacy.exists(), "live legacy lock kept");
            assert!(
                state::stop_sentinel_path(pid).exists(),
                "legacy pid signalled"
            );
        });
    }

    /// A live pre-v0.1.x monitor (lock under the old name only) is the
    /// pane's monitor: `ensure_monitor` keeps it, `status` shows it.
    #[cfg(unix)]
    #[test]
    fn live_pre_encoding_monitor_is_the_panes_monitor() {
        run_with_fake_herdr(EVIL_LIST_HERDR, &[], |dir| {
            let mut child = crate::test_support::spawn_fake_monitor("w1:p1");
            let pid = child.id();
            let legacy = dir.join("monitors/w1_p1.json");
            std::fs::create_dir_all(legacy.parent().unwrap()).expect("monitors dir");
            std::fs::write(&legacy, format!(r#"{{"pid":{pid},"pane_id":"w1:p1"}}"#))
                .expect("legacy");
            assert_eq!(state::live_monitor_pid("w1:p1"), Some(pid));
            assert!(ensure_monitor("w1:p1"));
            let lines = status_lines();
            child.kill().ok();
            child.wait().ok();
            assert!(
                lines.contains(&format!(
                    "pane w1:p1 agent=- status=- session=- monitor=live({pid})"
                )),
                "got: {lines:?}"
            );
            assert!(!state::monitor_lock_path("w1:p1").exists());
        });
    }

    #[test]
    fn status_lists_stale_and_unreadable_locks() {
        run_isolated(&[], |dir| {
            state::write_monitor_lock_pid("w1:p1", 2147483647);
            std::fs::write(dir.join("monitors/junk.json"), "not json").expect("write junk");
            let lines = status_lines();
            assert!(lines
                .iter()
                .any(|l| l == "stale lock pane w1:p1 pid=2147483647"));
            assert!(lines.iter().any(|l| l == "unreadable lock junk.json"));
            assert!(lines
                .last()
                .is_some_and(|l| l.contains("1 stale locks, 1 unreadable locks")));
        });
    }

    #[test]
    fn log_tail_fills_from_rotated_generation() {
        let dir = crate::test_support::unique_temp_dir("log-tail-test");
        let path = dir.join("log.txt");
        // No rotated file: just the current tail.
        std::fs::write(&path, "c\nd\n").expect("write log");
        assert_eq!(log_tail(&path, 5), vec!["c", "d"]);
        std::fs::write(dir.join("log.txt.1"), "x\na\nb\n").expect("write rotated");
        // Older lines first, then the current log, capped at n.
        assert_eq!(log_tail(&path, 3), vec!["b", "c", "d"]);
        assert_eq!(log_tail(&path, 9), vec!["x", "a", "b", "c", "d"]);
        // Enough lines in log.txt: log.txt.1 is not consulted.
        assert_eq!(log_tail(&path, 2), vec!["c", "d"]);
        // Just rotated (log.txt missing): all from log.txt.1.
        std::fs::remove_file(&path).expect("rm log");
        assert_eq!(log_tail(&path, 2), vec!["a", "b"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tail_lines_returns_last_n() {
        let dir = crate::test_support::unique_temp_dir("tail-test");
        let path = dir.join("log.txt");
        // Larger than the initial 8 KiB chunk so the growth path runs.
        let text: String = (0..2000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, &text).expect("write log");
        let tail = tail_lines(&path, 1500);
        assert_eq!(tail.len(), 1500);
        assert_eq!(tail[0], "line 500");
        assert_eq!(tail[1499], "line 1999");
        assert_eq!(
            tail_lines(&path, 3),
            vec!["line 1997", "line 1998", "line 1999"]
        );
        // Fewer lines than n: everything, from the first line.
        std::fs::write(&path, "a\nb\n").expect("write log");
        assert_eq!(tail_lines(&path, 50), vec!["a", "b"]);
        assert!(tail_lines(&dir.join("missing.txt"), 50).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn log_line_count_defaults_to_50() {
        assert_eq!(log_line_count(None), 50);
        assert_eq!(log_line_count(Some("abc")), 50);
        assert_eq!(log_line_count(Some("7")), 7);
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn run_exit_codes_for_usage_errors() {
        assert_eq!(run(&args(&["bogus"])), 2);
        assert_eq!(run(&args(&[])), 2);
        assert_eq!(run(&args(&["monitor"])), 2);
        assert_eq!(run(&args(&["monitor", "../../evil"])), 2);
    }

    #[test]
    fn run_fails_closed_without_state_dir() {
        let unset = [
            ("HOME", None),
            ("HERDR_PLUGIN_STATE_DIR", None),
            ("HERDR_PLUGIN_CONFIG_DIR", None),
        ];
        run_isolated(&unset, |dir| {
            let msg =
                "auto-resume: cannot resolve state dir (HOME unset); set HERDR_PLUGIN_STATE_DIR";
            for cmd in [
                vec!["logs"],
                vec!["status"],
                vec!["stop"],
                vec!["startup"],
                vec!["supervise-all"],
                vec!["hook-pane", "w1:a"],
                vec!["monitor", "w1:a"],
            ] {
                assert_eq!(run(&args(&cmd)), 2, "{cmd:?}");
            }
            assert_eq!(check_state_dir(), Err(msg));
            assert_eq!(run(&args(&["bogus"])), 2, "usage error");
            assert_eq!(herdr_calls(dir), "", "no command may reach herdr");
        });
    }

    #[test]
    fn run_exit_codes_for_read_only_and_success() {
        run_isolated(&[], |_| {
            assert_eq!(run(&args(&["logs"])), 0);
            assert_eq!(run(&args(&["logs", "abc"])), 0);
            assert_eq!(run(&args(&["status"])), 0);
            assert_eq!(run(&args(&["stop"])), 0);
            assert_eq!(run(&args(&["supervise-all"])), 0);
        });
        run_with_fake_herdr(FAILING_HERDR, &[], |_| {
            assert_eq!(run(&args(&["startup"])), 1);
        });
    }

    /// A lock judged stale by the scan but reclaimed by a live monitor
    /// before removal is kept, and that new monitor is stopped too.
    #[cfg(unix)]
    #[test]
    fn stop_signals_a_monitor_that_reclaimed_a_stale_lock() {
        run_isolated(&[], |_| {
            state::write_monitor_lock_pid("w1:p1", 2147483647);
            let stale = state::scan_monitor_locks().0.remove(0);
            let mut child = crate::test_support::spawn_fake_monitor("w1:p1");
            let pid = child.id();
            assert!(state::write_monitor_lock_pid("w1:p1", pid), "reclaim");
            let code = stop_lock(&stale);
            child.kill().ok();
            child.wait().ok();
            assert_eq!(code, 0);
            assert!(state::monitor_lock_path("w1:p1").exists(), "kept");
            assert!(
                state::stop_sentinel_path(pid).exists(),
                "new monitor stopped"
            );
            // A reclaim by a non-monitor pid is only kept.
            state::write_monitor_lock_pid("w1:p1", 2147483646);
            assert_eq!(stop_lock(&stale), 0);
            assert!(!state::stop_sentinel_path(2147483646).exists());
            assert!(state::monitor_lock_path("w1:p1").exists());
        });
    }

    /// A live "monitor" ([`crate::test_support::spawn_fake_monitor`])
    /// whose sentinel path is blocked by a directory: `stop` must count
    /// the failed write and exit 1.
    #[cfg(unix)]
    #[test]
    fn run_stop_returns_1_when_sentinel_write_fails() {
        run_isolated(&[], |_| {
            let mut child = crate::test_support::spawn_fake_monitor("w1:stop");
            let pid = child.id();
            state::write_monitor_lock_pid("w1:stop", pid);
            assert_eq!(state::live_monitor_pid("w1:stop"), Some(pid));
            std::fs::create_dir_all(state::stop_sentinel_path(pid)).expect("block sentinel");
            let code = run(&args(&["stop"]));
            child.kill().ok();
            child.wait().ok();
            assert_eq!(code, 1);
        });
    }
}
