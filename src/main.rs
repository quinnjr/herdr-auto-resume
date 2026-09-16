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
fn wants_monitor(
    pane: &herdr::Pane,
    registry: &HashMap<String, herdr::SessionRef>,
) -> bool {
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
/// this is safe to run with live agents doing real work.
/// Returns the count of failed spawns.
fn supervise_all() -> u32 {
    let reg = state::load_registry();
    let mut failed = 0u32;
    let panes = match herdr::pane_list_checked() {
        Some(panes) => panes,
        None => {
            eprintln!("auto-resume: supervise-all: pane list failed");
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
/// ensures a monitor.
/// Returns the count of failed spawns (0 on the single-pane path unless
/// ensure_monitor failed).
fn hook_pane(arg: Option<&str>) -> u32 {
    let pane_id = arg
        .map(str::to_string)
        .or_else(|| std::env::var("HERDR_PANE_ID").ok())
        .or_else(|| {
            std::env::var("HERDR_PLUGIN_EVENT_JSON")
                .ok()
                .and_then(|raw| event_pane_id(&raw))
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
        if let Some(sess) = pane.agent_session.clone() {
            state::remember(&pane_id, sess);
        }
        let reg = state::load_registry();
        if wants_monitor(&pane, &reg) && !ensure_monitor(&pane_id) {
            eprintln!("auto-resume: hook-pane {pane_id}: monitor spawn failed");
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
/// monitor liveness. Touches nothing.
fn status() {
    let panes = herdr::pane_list();
    let reg = state::load_registry();
    let mut live = 0u32;
    for pane in &panes {
        let agent = pane.agent.as_deref().unwrap_or("-");
        let st = pane.agent_status.as_deref().unwrap_or("-");
        let known = if reg.contains_key(&pane.pane_id) {
            "session=known"
        } else {
            "session=-"
        };
        let mon = match state::live_monitor_pid(&pane.pane_id) {
            Some(pid) => {
                live += 1;
                format!("monitor=live({pid})")
            }
            None => "monitor=-".to_string(),
        };
        println!("pane {} agent={agent} status={st} {known} {mon}", pane.pane_id);
    }
    // Stale locks (recorded pid no longer a monitor) are listed so drift
    // is visible without extra probing.
    let stale: Vec<_> = state::monitor_locks()
        .into_iter()
        .filter(|(id, _)| state::live_monitor_pid(id).is_none())
        .collect();
    for (id, pid) in &stale {
        println!("stale lock pane {id} pid={pid}");
    }
    println!(
        "auto-resume: {} panes, {live} live monitors, {} stale locks",
        panes.len(),
        stale.len()
    );
}

/// `stop`: write a `stop-<pid>` sentinel per live monitor lock. The
/// monitor loop sees it on its next poll, clears its lock + sentinel,
/// and exits. Stale locks (no live monitor behind them) are cleared
/// instead of signalled. Never kills, closes, or touches panes.
/// Returns the count of failed sentinel writes.
fn stop() -> u32 {
    let locks = state::monitor_locks();
    if locks.is_empty() {
        println!("auto-resume: no monitors recorded");
        return 0;
    }
    let mut failed = 0u32;
    for (pane_id, pid) in &locks {
        if state::live_monitor_pid(pane_id).is_none() {
            state::clear_monitor_lock(pane_id);
            println!("auto-resume: cleared stale lock for {pane_id} (pid {pid})");
            continue;
        }
        let path = state::stop_sentinel_path(*pid);
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!(
                    "auto-resume: stop failed for {pane_id} (pid {pid}): create_dir_all: {e}"
                );
                failed += 1;
                continue;
            }
        }
        match std::fs::write(&path, "") {
            Ok(()) => println!("auto-resume: stop requested for {pane_id} (pid {pid})"),
            Err(e) => {
                eprintln!("auto-resume: stop failed for {pane_id} (pid {pid}): {e}");
                failed += 1;
            }
        }
    }
    failed
}

/// `logs`: print the tail of `log.txt` (default 50 lines).
fn logs(arg: Option<&str>) {
    let n: usize = arg.and_then(|a| a.parse().ok()).unwrap_or(50);
    let path = config::state_dir().join("log.txt");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    for line in &lines[start..] {
        println!("{line}");
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    match cmd {
        // `monitor <pane-id>` carries the pane id as an argv token: the
        // liveness check in state.rs matches it against /proc cmdlines.
        "monitor" => match args.get(1) {
            Some(id) if state::valid_pane_id(id) => monitor::run(id),
            Some(id) => {
                eprintln!("auto-resume: monitor: rejected invalid pane id {id:?}");
                std::process::exit(2);
            }
            None => {
                eprintln!("usage: auto-resume monitor <pane-id>");
                std::process::exit(2);
            }
        },
        "startup" | "supervise-all" => {
            let failed = supervise_all();
            if failed > 0 {
                std::process::exit(1);
            }
        }
        "hook-pane" => {
            let failed = hook_pane(args.get(1).map(String::as_str));
            if failed > 0 {
                std::process::exit(1);
            }
        }
        "status" => status(),
        "stop" => {
            let failed = stop();
            if failed > 0 {
                std::process::exit(1);
            }
        }
        "logs" => logs(args.get(1).map(String::as_str)),
        _ => {
            eprintln!(
                "usage: auto-resume <startup|hook-pane|supervise-all|status|stop|logs|monitor>"
            );
            std::process::exit(2);
        }
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
    /// w1:argv / w1:evt, a live-agent pane with session for w9:live,
    /// null-result (unknown pane) otherwise, and an empty pane list.
    const FAKE_HERDR: &str = r#"#!/bin/sh
echo "$@" >> "@CALL_LOG@"
if [ "$1" = "pane" ]; then
  if [ "$2" = "get" ]; then
    case "$3" in
      w1:argv) echo '{"id":"x","result":{"pane":{"pane_id":"w1:argv"}}}' ;;
      w1:evt) echo '{"id":"x","result":{"pane":{"pane_id":"w1:evt"}}}' ;;
      w9:live) echo '{"id":"x","result":{"pane":{"pane_id":"w9:live","agent":"kiro","agent_status":"working","agent_session":{"agent":"kiro","value":"sess-live"}}}}' ;;
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
    fn stop_zero_when_no_locks() {
        run_isolated(&[], |_| {
            assert_eq!(stop(), 0);
        });
    }

    #[test]
    fn stop_clears_stale_lock_without_sentinel() {
        run_isolated(&[], |_| {
            state::write_monitor_lock_pid("w1:p1", 2147483647);
            assert_eq!(stop(), 0);
            assert!(
                state::monitor_locks().is_empty(),
                "stale lock must be cleared"
            );
            assert!(
                !state::stop_sentinel_path(2147483647).exists(),
                "no sentinel for a stale lock"
            );
        });
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
            assert!(
                calls.contains("pane list"),
                "must list panes, got: {calls}"
            );
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
}
