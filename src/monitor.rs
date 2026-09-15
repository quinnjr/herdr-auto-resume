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
/// Best-effort across shapes (`foreground_processes` — the live shape on
/// this host — plus `foreground` / `processes` / `process_list` /
/// `children` lists of `{argv: [...]}` or bare argv arrays, or a single
/// `argv`). Unknown shape yields empty (which vetoes relaunch — fail
/// closed; the caller logs the shape for diagnosability).
fn foreground_argv(info: &serde_json::Value) -> Vec<Vec<String>> {
    for key in [
        "foreground_processes",
        "foreground",
        "processes",
        "process_list",
        "children",
    ] {
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
/// One monitor poll. Returns false when the pane no longer exists
/// (`pane_get` → None) so the caller can count consecutive misses.
fn poll_once(pane_id: &str, config: &Config, cooldown_until: &mut Option<Instant>) -> bool {
    let Some(pane) = herdr::pane_get(pane_id) else {
        return false;
    };
    let proc_argv = match herdr::process_info(pane_id) {
        Some(v) => {
            let argv = foreground_argv(&v);
            if argv.is_empty() {
                // Fail-closed AND diagnosable: record the shape we saw
                // so future `process-info` drift shows up in the log.
                let keys = v
                    .as_object()
                    .map(|o| {
                        o.keys()
                            .map(String::as_str)
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_else(|| "<non-object>".to_string());
                crate::state::append_log(&format!(
                    "monitor {pane_id}: empty foreground parse; keys={keys}"
                ));
            }
            argv
        }
        None => vec![],
    };
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
        return true;
    }
    let reg = crate::state::load_registry();
    if pane.agent.is_none() && !reg.contains_key(pane_id) {
        return true;
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
        (None, None) => return true,
    };
    if agent.is_empty() {
        return true;
    }
    let Some(argv) =
        crate::resume::resume_argv(&agent, value.as_deref(), &config.commands)
    else {
        return true;
    };
    let args: Vec<&str> = argv.iter().map(String::as_str).collect();
    if herdr::pane_run(pane_id, &args) {
        *cooldown_until = Some(Instant::now() + Duration::from_secs(config.cooldown_seconds));
        crate::state::append_log(&format!("monitor {pane_id}: relaunched {agent}"));
    } else {
        crate::state::append_log(&format!("monitor {pane_id}: pane run failed for {agent}"));
    }
    true
}

/// Monitor loop for one pane: claim the monitor lock FIRST, then sleep
/// `connect_grace_seconds` (the pane may still be connecting after
/// restore), then poll every `poll_seconds` until a `stop-<pid>` sentinel
/// appears for our pid — or the pane stays gone for 10 straight polls
/// (closed/deleted panes must not spin a monitor forever).
///
/// Lock-then-sleep (not sleep-then-lock): the lock is visible during the
/// long grace sleep, so a racing second monitor — and `supervise-all` —
/// sees it via `live_monitor_pid` instead of double-spawning. A
/// simultaneous-start race (both check before either writes) remains but
/// is millisecond-narrow instead of grace-seconds-wide.
pub fn run(pane_id: &str) {
    let config = crate::config::load();
    if crate::state::live_monitor_pid(pane_id).is_some() {
        return;
    }
    crate::state::write_monitor_lock(pane_id);
    // Grace sleep in 1s slices so `stop` takes effect in ~1s instead of
    // after the full `connect_grace_seconds`.
    let own_pid = std::process::id();
    for _ in 0..config.connect_grace_seconds {
        if crate::state::stop_requested(own_pid) {
            crate::state::clear_monitor_lock(pane_id);
            crate::state::clear_stop_sentinel(own_pid);
            return;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    let mut cooldown_until: Option<Instant> = None;
    // A pane that stays gone (closed/deleted) must not spin a monitor
    // forever: exit and clear the lock after 10 consecutive misses.
    let mut missing = 0u32;
    loop {
        if crate::state::stop_requested(own_pid) {
            crate::state::clear_monitor_lock(pane_id);
            crate::state::clear_stop_sentinel(own_pid);
            break;
        }
        if poll_once(pane_id, &config, &mut cooldown_until) {
            missing = 0;
        } else {
            missing += 1;
            if missing >= 10 {
                crate::state::clear_monitor_lock(pane_id);
                crate::state::clear_stop_sentinel(own_pid);
                break;
            }
        }
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

    /// Live `herdr pane process-info` shape captured on this machine
    /// (2026-09-15, kiro agent pane `w7G:p1`): the result's
    /// `process_info` object carries `foreground_processes: [{argv, ...}]`.
    /// This is the primary session/decision source — the parser must
    /// handle this exact shape.
    #[test]
    fn foreground_argv_parses_live_process_info_shape() {
        let info: serde_json::Value = serde_json::from_str(
            r#"{"foreground_process_group_id":51024,"foreground_processes":[{"argv":["kiro-cli","--resume"],"cmdline":"kiro-cli --resume","cwd":"/home/joseph/Projects/Lexmata/lexmata-litify-integration","name":"kiro-cli","pid":51024},{"argv":["/home/joseph/.local/bin/kiro-cli-chat","chat","--resume"],"cmdline":"/home/joseph/.local/bin/kiro-cli-chat chat --resume","cwd":"/home/joseph/Projects/Lexmata/lexmata-litify-integration","name":"kiro-cli-chat","pid":51033},{"argv":["/home/joseph/.local/share/kiro-cli/bun","/home/joseph/.local/share/kiro-cli/tui.js","chat","--resume"],"cmdline":"/home/joseph/.local/share/kiro-cli/bun /home/joseph/.local/share/kiro-cli/tui.js chat --resume","cwd":"/home/joseph/Projects/Lexmata/lexmata-litify-integration","name":"bun","pid":51122}],"pane_id":"w7G:p1","shell_pid":20518}"#,
        )
        .expect("fixture parses");
        let argv = foreground_argv(&info);
        assert_eq!(argv.len(), 3);
        assert_eq!(argv[0], vec!["kiro-cli", "--resume"]);
        // A live agent foreground must veto relaunch.
        let pane = Pane { pane_id: "w7G:p1".into(), agent: Some("kiro".into()),
            agent_status: Some("working".into()), ..Default::default() };
        assert!(!should_relaunch(&pane, &argv, None));
    }

    /// Live idle-shell shape (`w78:p1`, 2026-09-15): single bare zsh in
    /// `foreground_processes` → idle, relaunchable when agentless.
    #[test]
    fn foreground_argv_parses_live_idle_shell_shape() {
        let info: serde_json::Value = serde_json::from_str(
            r#"{"foreground_process_group_id":20508,"foreground_processes":[{"argv":["/usr/bin/zsh"],"cmdline":"/usr/bin/zsh","cwd":"/home/joseph/Projects/icedtea","name":"zsh","pid":20508}],"pane_id":"w78:p1","shell_pid":20508}"#,
        )
        .expect("fixture parses");
        let argv = foreground_argv(&info);
        assert_eq!(argv, vec![vec!["/usr/bin/zsh".to_string()]]);
        let pane = Pane { pane_id: "w78:p1".into(), agent: None,
            agent_status: Some("unknown".into()), ..Default::default() };
        assert!(should_relaunch(&pane, &argv, None));
    }
}
