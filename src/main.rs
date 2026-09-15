mod config;
mod herdr;
mod monitor;
mod resume;
mod state;

use std::process::Stdio;

/// A pane is worth a monitor when it has a live agent, or when the
/// registry already holds a session ref for it (a dead agent's pane that
/// we know how to relaunch). Plain shells with no history are skipped.
fn wants_monitor(pane: &herdr::Pane) -> bool {
    if pane.agent.is_some() {
        return true;
    }
    state::load_registry().contains_key(&pane.pane_id)
}

/// Spawn `auto-resume monitor <pane-id>` detached: stdio to /dev/null so
/// the child outlives the plugin invocation. NOTE: no `setsid`/double
/// fork — monitors die with the user session. Acceptable v1 (same as
/// prior art); a restart re-spawns them via `startup`.
fn spawn_monitor(pane_id: &str) {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    std::process::Command::new(exe)
        .arg("monitor")
        .arg(pane_id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok();
}

/// Ensure exactly one live monitor for `pane_id`: skip when one is
/// already live (checked via lock + /proc argv), else spawn.
fn ensure_monitor(pane_id: &str) {
    if state::live_monitor_pid(pane_id).is_some() {
        return;
    }
    spawn_monitor(pane_id);
}

/// Scan all panes and ensure a monitor for each supervisable one.
/// Spawns polling monitors only — no `pane run`, no pane mutation, so
/// this is safe to run with live agents doing real work.
fn supervise_all() {
    for pane in herdr::pane_list() {
        if !wants_monitor(&pane) {
            continue;
        }
        ensure_monitor(&pane.pane_id);
    }
    println!("auto-resume: supervise-all done");
}

/// `hook-pane`: refresh one pane (event payload shape is not
/// contractual, so the pane id is best-effort: argv[1], then
/// `HERDR_PANE_ID`, else fall back to a full scan). Records a live
/// `agent_session` into the registry and ensures a monitor.
fn hook_pane(arg: Option<&str>) {
    let pane_id = arg
        .map(str::to_string)
        .or_else(|| std::env::var("HERDR_PANE_ID").ok())
        .unwrap_or_default();
    if pane_id.is_empty() {
        supervise_all();
        return;
    }
    if let Some(pane) = herdr::pane_get(&pane_id) {
        if let Some(sess) = pane.agent_session.clone() {
            state::remember(&pane_id, sess);
        }
        if wants_monitor(&pane) {
            ensure_monitor(&pane_id);
        }
        println!("auto-resume: hook-pane {pane_id} ok");
    } else {
        eprintln!("auto-resume: hook-pane: unknown pane {pane_id}");
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
fn stop() {
    let locks = state::monitor_locks();
    if locks.is_empty() {
        println!("auto-resume: no monitors recorded");
        return;
    }
    for (pane_id, pid) in &locks {
        if state::live_monitor_pid(pane_id).is_none() {
            state::clear_monitor_lock(pane_id);
            println!("auto-resume: cleared stale lock for {pane_id} (pid {pid})");
            continue;
        }
        let path = state::stop_sentinel_path(*pid);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&path, "").ok();
        println!("auto-resume: stop requested for {pane_id} (pid {pid})");
    }
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
        "startup" | "supervise-all" => supervise_all(),
        "hook-pane" => hook_pane(args.get(1).map(String::as_str)),
        "status" => status(),
        "stop" => stop(),
        "logs" => logs(args.get(1).map(String::as_str)),
        _ => {
            eprintln!(
                "usage: auto-resume <startup|hook-pane|supervise-all|status|stop|logs|monitor>"
            );
            std::process::exit(2);
        }
    }
}
