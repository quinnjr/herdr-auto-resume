use std::time::{Duration, Instant};

use crate::config::Config;
use crate::herdr::{self, Pane};

/// Foreground programs that count as an idle shell: a bare shell with no
/// script, `-c` command, or other arguments is a dead agent's pane.
const IDLE_SHELLS: &[&str] = &[
    "zsh", "bash", "sh", "dash", "ksh", "fish", "nu", "pwsh", "ash",
];

/// True when `argv` is a bare idle shell: exactly one element naming a
/// known shell (path prefix and login-shell `-` prefix stripped).
fn is_bare_idle_shell(argv: &[String]) -> bool {
    if argv.len() != 1 {
        return false;
    }
    let prog = &argv[0];
    let base = prog.rsplit('/').next().unwrap_or(prog);
    let base = base.strip_prefix('-').unwrap_or(base);
    IDLE_SHELLS.contains(&base)
}

/// Relaunch decision. True ONLY when every foreground process is a bare
/// idle shell (a live foreground process — agent or script — vetoes),
/// AND the pane looks agentless (status unknown/absent, or no agent
/// recorded), AND the post-relaunch cooldown has expired.
///
/// Load-bearing guard: never returns true with a live foreground process.
pub fn should_relaunch(
    pane: &Pane,
    proc_argv: &[Vec<String>],
    cooldown_until: Option<Instant>,
) -> bool {
    if let Some(until) = cooldown_until {
        if Instant::now() < until {
            return false;
        }
    }
    if proc_argv.is_empty() || !proc_argv.iter().all(|a| is_bare_idle_shell(a)) {
        return false;
    }
    match pane.agent_status.as_deref() {
        None => true,
        Some(s) if s.eq_ignore_ascii_case("unknown") => true,
        _ => pane.agent.is_none(),
    }
}

/// Extract foreground argv vectors from a `process-info` result object.
/// Best-effort across shapes (`foreground` / `processes` lists of
/// `{argv: [...]}` or bare argv arrays, or a single `argv`). Unknown
/// shape yields empty (which vetoes relaunch — fail closed).
fn foreground_argv(info: &serde_json::Value) -> Vec<Vec<String>> {
    for key in ["foreground", "processes", "process_list", "children"] {
        if let Some(v) = info.get(key) {
            let out = argv_list_from(v);
            if !out.is_empty() {
                return out;
            }
        }
    }
    if let Some(argv) = info.get("argv") {
        if let Ok(a) = serde_json::from_value::<Vec<String>>(argv.clone()) {
            if !a.is_empty() {
                return vec![a];
            }
        }
    }
    vec![]
}

fn argv_list_from(v: &serde_json::Value) -> Vec<Vec<String>> {
    let arr = match v.as_array() {
        Some(a) => a,
        None => return vec![],
    };
    let mut out = Vec::new();
    for item in arr {
        if let Some(argv) = item.get("argv") {
            if let Ok(a) = serde_json::from_value::<Vec<String>>(argv.clone()) {
                out.push(a);
                continue;
            }
        }
        if let Ok(a) = serde_json::from_value::<Vec<String>>(item.clone()) {
            out.push(a);
        }
    }
    out
}

/// One monitor poll: refresh the registry while the agent is alive
/// (threading the live config commands), then relaunch when
/// `should_relaunch` fires. Agent-less panes need a registry ref to know
/// what to relaunch; panes with a known agent but no session fall back
/// to a valueless/`<agent>-fallback` template when one exists.
fn poll_once(pane_id: &str, config: &Config, cooldown_until: &mut Option<Instant>) {
    let Some(pane) = herdr::pane_get(pane_id) else {
        return;
    };
    let proc_argv = herdr::process_info(pane_id)
        .map(|v| foreground_argv(&v))
        .unwrap_or_default();
    if pane.agent.is_some() {
        let reg = crate::state::load_registry();
        if let Some(sess) = crate::resume::resolve_session_with_commands(
            &pane,
            reg.get(pane_id),
            &proc_argv,
            &config.commands,
        ) {
            crate::state::remember(pane_id, sess);
        }
    }
    if !should_relaunch(&pane, &proc_argv, *cooldown_until) {
        return;
    }
    let reg = crate::state::load_registry();
    if pane.agent.is_none() && !reg.contains_key(pane_id) {
        return;
    }
    let registry_value = reg.get(pane_id);
    let session = crate::resume::resolve_session_with_commands(
        &pane,
        registry_value,
        &proc_argv,
        &config.commands,
    );
    let (agent, value) = match (&pane.agent, session) {
        (Some(a), Some(s)) => (
            a.clone(),
            if s.value.is_empty() {
                None
            } else {
                Some(s.value)
            },
        ),
        (Some(a), None) => (a.clone(), None),
        (None, Some(s)) => (
            s.agent.clone(),
            if s.value.is_empty() {
                None
            } else {
                Some(s.value)
            },
        ),
        (None, None) => return,
    };
    if agent.is_empty() {
        return;
    }
    let Some(argv) =
        crate::resume::resume_argv(&agent, value.as_deref(), &config.commands)
    else {
        return;
    };
    let args: Vec<&str> = argv.iter().map(String::as_str).collect();
    if herdr::pane_run(pane_id, &args) {
        *cooldown_until = Some(Instant::now() + Duration::from_secs(config.cooldown_seconds));
        crate::state::append_log(&format!("monitor {pane_id}: relaunched {agent}"));
    }
}

/// Monitor loop for one pane: sleep `connect_grace_seconds` (the pane
/// may still be connecting after restore), then poll every
/// `poll_seconds` until a `stop-<pid>` sentinel appears for our pid.
pub fn run(pane_id: &str) {
    let config = crate::config::load();
    std::thread::sleep(Duration::from_secs(config.connect_grace_seconds));
    crate::state::write_monitor_lock(pane_id);
    let own_pid = std::process::id();
    let mut cooldown_until: Option<Instant> = None;
    loop {
        if crate::state::stop_requested(own_pid) {
            crate::state::clear_monitor_lock(pane_id);
            crate::state::clear_stop_sentinel(own_pid);
            break;
        }
        poll_once(pane_id, &config, &mut cooldown_until);
        std::thread::sleep(Duration::from_secs(config.poll_seconds));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relaunches_dead_pane_at_idle_shell() {
        let pane = Pane { pane_id: "w1:p1".into(), agent: None,
            agent_status: Some("unknown".into()), ..Default::default() };
        let argv = vec![vec!["/usr/bin/zsh".into()]];
        assert!(should_relaunch(&pane, &argv, None));
    }

    #[test]
    fn never_relaunches_with_live_foreground() {
        let pane = Pane { pane_id: "w1:p1".into(), agent: Some("claude".into()),
            agent_status: Some("working".into()), ..Default::default() };
        let argv = vec![vec!["claude".into()]];
        assert!(!should_relaunch(&pane, &argv, None));
    }

    #[test]
    fn shell_running_a_script_is_not_idle() {
        let pane = Pane { pane_id: "w1:p1".into(), agent: None,
            agent_status: Some("unknown".into()), ..Default::default() };
        let argv = vec![vec!["/bin/sh".into(), "myscript.sh".into()]];
        assert!(!should_relaunch(&pane, &argv, None));
    }
}
