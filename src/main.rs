mod config;
mod herdr;
mod monitor;
mod resume;
mod state;

use std::process::Stdio;

/// A pane is worth a monitor when it has a live agent, or when the
/// registry already holds a session ref for it (a dead agent's pane that
/// we know how to relaunch). Plain shells with no history are skipped.
fn wants_monitor(
    pane: &herdr::Pane,
    reg: &std::collections::HashMap<String, herdr::SessionRef>,
) -> bool {
    if pane.agent.is_some() {
        return true;
    }
    reg.contains_key(&pane.pane_id)
}

/// Spawn `auto-resume monitor <pane-id>` detached: stdio to /dev/null so
/// the child outlives the plugin invocation. NOTE: no `setsid`/double
/// fork — monitors die with the user session. Acceptable v1 (same as
/// prior art); a restart re-spawns them via `startup`.
/// Returns true on successful spawn, false on failure (logged).
fn spawn_monitor(pane_id: &str) -> bool {
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
    for pane in herdr::pane_list() {
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
    v.get("data")
        .and_then(|d| d.get("pane_id"))
        .or_else(|| v.get("pane_id"))
        .and_then(|id| id.as_str())
        .map(str::to_string)
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
            Some(id) => monitor::run(id),
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
    use std::collections::HashMap;

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
}
