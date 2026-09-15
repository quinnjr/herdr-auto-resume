use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::herdr::{self, Pane, SessionRef};

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
/// AND the post-relaunch cooldown has expired.
///
/// Foreground-only by design: after a crash the pane keeps a stale agent
/// label + status while the process is gone, so recorded metadata must
/// not veto. Session resolution in `decide_poll` still requires a known
/// session (live, registry, or argv-derived); without one the poll
/// degrades to a logged `NoResume`, never a blind launch.
///
/// Load-bearing guard: never returns true with a live foreground process.
pub fn should_relaunch(
    proc_argv: &[Vec<String>],
    cooldown_until: Option<Instant>,
) -> bool {
    if let Some(until) = cooldown_until {
        if Instant::now() < until {
            return false;
        }
    }
    !proc_argv.is_empty() && proc_argv.iter().all(|a| is_bare_idle_shell(a))
}

/// Extract foreground argv vectors from a `process-info` result object.
/// Only the live shape is honored (`foreground_processes` lists of
/// `{argv: [...]}` or bare argv arrays, or a single `argv`); anything else
/// yields empty (which vetoes relaunch — fail closed; the caller logs the
/// shape for diagnosability).
fn foreground_argv(info: &serde_json::Value) -> Vec<Vec<String>> {
    if let Some(v) = info.get("foreground_processes") {
        let out = argv_list_from(v);
        if !out.is_empty() {
            return out;
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

/// Sentinel for a foreground list with zero parseable entries. A bare
/// sentinel never matches `IDLE_SHELLS`, so `should_relaunch` vetoes
/// fail-closed instead of treating silent drops as an idle shell.
const UNPARSEABLE: &str = "<unparseable>";

fn argv_list_from(v: &serde_json::Value) -> Vec<Vec<String>> {
    let arr = match v.as_array() {
        Some(a) => a,
        None => return vec![],
    };
    let mut out = Vec::new();
    let mut dropped_any = false;
    for item in arr {
        if let Some(argv) = item.get("argv") {
            if let Ok(a) = serde_json::from_value::<Vec<String>>(argv.clone()) {
                out.push(a);
                continue;
            }
        }
        if let Ok(a) = serde_json::from_value::<Vec<String>>(item.clone()) {
            out.push(a);
        } else {
            // Fail-closed on partial corruption: a dropped entry could be
            // a live agent, so poison the whole parse with the sentinel
            // (never a bare idle shell) instead of treating the survivors
            // as the full foreground.
            dropped_any = true;
        }
    }
    if dropped_any {
        out.push(vec![UNPARSEABLE.to_string()]);
    }
    if out.is_empty() && !arr.is_empty() {
        return vec![vec![UNPARSEABLE.to_string()]];
    }
    out
}

/// Pure poll decision: refresh the registry while the agent is alive,
/// relaunch when `should_relaunch` fires, otherwise do nothing.
/// `should_relaunch` remains the veto core (live-foreground veto stays
/// fail-closed: empty/unknown/sentinel argv never relaunches).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollAction {
    Refresh(SessionRef),
    Relaunch { agent: String, argv: Vec<String> },
    /// Veto passed but no resume argv could be built (computed once in
    /// `decide_poll`): `agent` is the best-known name (None when unknown),
    /// `value_known` tracks whether a session value was known.
    NoResume {
        agent: Option<String>,
        value_known: bool,
    },
    Nothing,
}

pub fn decide_poll(
    pane: &Pane,
    proc_argv: &[Vec<String>],
    registry: &HashMap<String, SessionRef>,
    config: &Config,
    cooldown_until: &Option<Instant>,
) -> PollAction {
    // Dead pane first: an idle-shell foreground means the agent process is
    // gone even when the pane still records an agent (stale crash
    // metadata). Refreshing the registry here would just re-record the
    // stale session and skip the relaunch.
    if should_relaunch(proc_argv, *cooldown_until) {
        return relaunch_for_dead_pane(pane, proc_argv, registry, config);
    }
    if pane.agent.is_some() {
        if let Some(sess) = crate::resume::resolve_session_with_commands(
            pane,
            registry.get(&pane.pane_id),
            proc_argv,
            &config.commands,
        ) {
            return PollAction::Refresh(sess);
        }
    }
    PollAction::Nothing
}

/// Resolve the relaunch for a dead (idle-shell foreground) pane: the
/// recorded agent (or registry ref when the pane names none) plus the
/// best-known session value feeds the resume template; unknown agent or
/// valueless template degrades to a logged `NoResume`.
fn relaunch_for_dead_pane(
    pane: &Pane,
    proc_argv: &[Vec<String>],
    registry: &HashMap<String, SessionRef>,
    config: &Config,
) -> PollAction {
    let registry_value = registry.get(&pane.pane_id);
    if pane.agent.is_none() && registry_value.is_none() {
        return PollAction::NoResume {
            agent: None,
            value_known: false,
        };
    }
    let session = crate::resume::resolve_session_with_commands(
        pane,
        registry_value,
        proc_argv,
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
        (None, None) => {
            return PollAction::NoResume {
                agent: None,
                value_known: false,
            }
        }
    };
    if agent.is_empty() {
        return PollAction::NoResume {
            agent: None,
            value_known: value.is_some(),
        };
    }
    let Some(argv) =
        crate::resume::resume_argv(&agent, value.as_deref(), &config.commands)
    else {
        return PollAction::NoResume {
            agent: Some(agent),
            value_known: value.is_some(),
        };
    };
    PollAction::Relaunch { agent, argv }
}

/// One monitor poll. Returns false when the pane no longer exists
/// (`pane_get` → None) so the caller can count consecutive misses.
/// Thin I/O shell around the pure `decide_poll`.
fn poll_once(pane_id: &str, config: &Config, cooldown_until: &mut Option<Instant>) -> bool {
    let Some(pane) = herdr::pane_get(pane_id) else {
        return false;
    };
    let proc_argv = match herdr::process_info(pane_id) {
        Some(v) => {
            let argv = foreground_argv(&v);
            if argv.is_empty()
                || argv
                    .iter()
                    .any(|a| a.iter().any(|t| t == UNPARSEABLE))
            {
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
                    "monitor {pane_id}: empty/unparseable foreground parse; keys={keys}"
                ));
            }
            argv
        }
        None => {
            crate::state::append_log(&format!(
                "monitor {pane_id}: process-info failed; vetoing relaunch"
            ));
            vec![]
        }
    };
    let reg = crate::state::load_registry();
    match decide_poll(&pane, &proc_argv, &reg, config, cooldown_until) {
        PollAction::Refresh(sess) => {
            crate::state::remember(pane_id, sess);
        }
        PollAction::Relaunch { agent, argv } => {
            let args: Vec<&str> = argv.iter().map(String::as_str).collect();
            if herdr::pane_run(pane_id, &args) {
                *cooldown_until =
                    Some(Instant::now() + Duration::from_secs(config.cooldown_seconds));
                crate::state::append_log(&format!("monitor {pane_id}: relaunched {agent}"));
            } else {
                crate::state::append_log(&format!(
                    "monitor {pane_id}: pane run failed for {agent}"
                ));
            }
        }
        PollAction::NoResume { agent, value_known } => {
            let agent_name = agent.unwrap_or_default();
            crate::state::append_log(&format!(
                "monitor {pane_id}: no resume argv for '{agent_name}' (value known: {value_known})"
            ));
        }
        PollAction::Nothing => {}
    }
    true
}

/// Shared monitor exit path: release the monitor lock and clear our stop
/// sentinel.
fn finish_monitor(pane_id: &str, own_pid: u32) {
    crate::state::clear_monitor_lock(pane_id);
    crate::state::clear_stop_sentinel(own_pid);
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
    if !crate::state::write_monitor_lock(pane_id) {
        return;
    }
    // Grace sleep in 1s slices so `stop` takes effect in ~1s instead of
    // after the full `connect_grace_seconds`.
    let own_pid = std::process::id();
    for _ in 0..config.connect_grace_seconds {
        if crate::state::stop_requested(own_pid) {
            finish_monitor(pane_id, own_pid);
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
            finish_monitor(pane_id, own_pid);
            break;
        }
        if poll_once(pane_id, &config, &mut cooldown_until) {
            missing = 0;
        } else {
            missing += 1;
            crate::state::append_log(&format!(
                "monitor {pane_id}: pane fetch failed (miss {missing}/10)"
            ));
            if missing >= 10 {
                crate::state::append_log(&format!(
                    "monitor {pane_id}: pane gone 10x, exiting"
                ));
                finish_monitor(pane_id, own_pid);
                break;
            }
        }
        std::thread::sleep(Duration::from_secs(config.poll_seconds));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::SessionRef;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    #[test]
    fn relaunches_dead_pane_at_idle_shell() {
        let argv = vec![vec!["/usr/bin/zsh".into()]];
        assert!(should_relaunch(&argv, None));
    }

    #[test]
    fn decide_poll_relaunches_dead_kiro_pane_via_fallback() {
        // Crash shape (live 2026-09-15, kiro w9M:p1): stale agent label +
        // idle status, bare `--resume` foreground carried no session id, so
        // the registry is empty. Must relaunch via the valueless
        // `kiro-fallback` template, not silently do nothing.
        let pane = Pane { pane_id: "w9M:p1".into(), agent: Some("kiro".into()),
            agent_status: Some("idle".into()), ..Default::default() };
        let argv = vec![vec!["/usr/bin/zsh".into()]];
        assert_eq!(
            decide_poll(&pane, &argv, &HashMap::new(), &Config::default(), &None),
            PollAction::Relaunch {
                agent: "kiro".into(),
                argv: vec!["kiro-cli".into(), "chat".into(), "-r".into()],
            }
        );
    }

    #[test]
    fn decide_poll_relaunches_dead_pane_with_recorded_agent() {
        // Crash shape (live 2026-09-15, opencode wAA:p1): agent still
        // recorded, status done, foreground idle shell, registry holds the
        // session. Must relaunch, not merely refresh the registry.
        let pane = Pane { pane_id: "wAA:p1".into(), agent: Some("opencode".into()),
            agent_status: Some("done".into()), ..Default::default() };
        let argv = vec![vec!["/usr/bin/zsh".into()]];
        let reg = HashMap::from([("wAA:p1".into(), SessionRef {
            agent: "opencode".into(), value: "ses_abc".into(),
        })]);
        assert_eq!(
            decide_poll(&pane, &argv, &reg, &Config::default(), &None),
            PollAction::Relaunch {
                agent: "opencode".into(),
                argv: vec!["opencode".into(), "--session".into(), "ses_abc".into()],
            }
        );
    }

    #[test]
    fn never_relaunches_with_live_foreground() {
        let argv = vec![vec!["claude".into()]];
        assert!(!should_relaunch(&argv, None));
    }

    #[test]
    fn shell_running_a_script_is_not_idle() {
        let argv = vec![vec!["/bin/sh".into(), "myscript.sh".into()]];
        assert!(!should_relaunch(&argv, None));
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
        assert!(!should_relaunch(&argv, None));
    }

    /// Live idle-shell shape (`w78:p1`, 2026-09-15): single bare zsh in
    /// `foreground_processes` → idle, relaunchable (recorded agent/status
    /// no longer veto: after a crash they are stale).
    #[test]
    fn foreground_argv_parses_live_idle_shell_shape() {
        let info: serde_json::Value = serde_json::from_str(
            r#"{"foreground_process_group_id":20508,"foreground_processes":[{"argv":["/usr/bin/zsh"],"cmdline":"/usr/bin/zsh","cwd":"/home/joseph/Projects/icedtea","name":"zsh","pid":20508}],"pane_id":"w78:p1","shell_pid":20508}"#,
        )
        .expect("fixture parses");
        let argv = foreground_argv(&info);
        assert_eq!(argv, vec![vec!["/usr/bin/zsh".to_string()]]);
        assert!(should_relaunch(&argv, None));
    }

    #[test]
    fn unparseable_foreground_list_vetoes_relaunch() {
        // Non-empty list, zero parseable entries → sentinel (not a bare
        // shell name) so the veto holds fail-closed.
        let info: serde_json::Value = serde_json::from_str(
            r#"{"foreground_processes":[{"foo":1},{"bar":"x"}]}"#,
        )
        .expect("fixture parses");
        let argv = foreground_argv(&info);
        assert_eq!(argv, vec![vec!["<unparseable>".to_string()]]);
        assert!(!should_relaunch(&argv, None));
    }

    #[test]
    fn cooldown_vetoes_otherwise_idle_pane() {
        let argv = vec![vec!["/usr/bin/zsh".into()]];
        let future = Some(Instant::now() + Duration::from_secs(300));
        assert!(!should_relaunch(&argv, future));
        let expired = Some(Instant::now() - Duration::from_secs(1));
        assert!(should_relaunch(&argv, expired));
    }

    #[test]
    fn empty_foreground_vetoes_relaunch() {
        let argv: Vec<Vec<String>> = vec![];
        assert!(!should_relaunch(&argv, None));
    }

    #[test]
    fn foreground_argv_returns_empty_on_unknown_shape() {
        // No `foreground_processes` / `argv` keys → fallthrough empty
        // (vetoes relaunch fail-closed).
        let info: serde_json::Value = serde_json::from_str(
            r#"{"frobnicate":[{"argv":["x"]}],"argv_count":1}"#,
        )
        .expect("fixture parses");
        assert_eq!(foreground_argv(&info), Vec::<Vec<String>>::new());
    }

    #[test]
    fn decide_poll_refreshes_live_session() {
        let live = SessionRef { agent: "claude".into(), value: "live-1".into() };
        let pane = Pane { pane_id: "w1:p1".into(), agent: Some("claude".into()),
            agent_status: Some("working".into()), agent_session: Some(live.clone()),
            ..Default::default() };
        let argv = vec![vec!["claude".into(), "--resume".into(), "live-1".into()]];
        assert_eq!(
            decide_poll(&pane, &argv, &HashMap::new(), &Config::default(), &None),
            PollAction::Refresh(live)
        );
    }

    #[test]
    fn decide_poll_relaunches_idle_shell_with_registry_ref() {
        let pane = Pane { pane_id: "w1:p1".into(), agent: None,
            agent_status: Some("unknown".into()), ..Default::default() };
        let argv = vec![vec!["/usr/bin/zsh".into()]];
        let reg = HashMap::from([("w1:p1".into(), SessionRef {
            agent: "claude".into(), value: "abc-123".into(),
        })]);
        let config = Config::default();
        assert_eq!(
            decide_poll(&pane, &argv, &reg, &config, &None),
            PollAction::Relaunch {
                agent: "claude".into(),
                argv: vec!["claude".into(), "--resume".into(), "abc-123".into()],
            }
        );
        // A follow-up decide with the post-relaunch cooldown set → Nothing.
        let cooldown = Some(Instant::now() + Duration::from_secs(config.cooldown_seconds));
        assert_eq!(
            decide_poll(&pane, &argv, &reg, &config, &cooldown),
            PollAction::Nothing
        );
    }

    #[test]
    fn decide_poll_live_agent_is_nothing() {
        let pane = Pane { pane_id: "w1:p1".into(), agent: Some("claude".into()),
            agent_status: Some("working".into()), ..Default::default() };
        let argv = vec![vec!["claude".into()]];
        assert_eq!(
            decide_poll(&pane, &argv, &HashMap::new(), &Config::default(), &None),
            PollAction::Nothing
        );
    }

    #[test]
    fn decide_poll_nothing_when_resume_unresolvable() {
        let config = Config::default();
        let idle = vec![vec!["/usr/bin/zsh".into()]];
        // (None, None): agentless idle pane, empty registry.
        let pane = Pane { pane_id: "w1:p1".into(), agent: None,
            agent_status: Some("unknown".into()), ..Default::default() };
        assert_eq!(
            decide_poll(&pane, &idle, &HashMap::new(), &config, &None),
            PollAction::NoResume {
                agent: None,
                value_known: false,
            }
        );
        // resume_argv None: known agent, idle shell, valued template, no
        // value anywhere.
        let pane = Pane { pane_id: "w1:p1".into(), agent: Some("claude".into()),
            agent_status: None, ..Default::default() };
        assert_eq!(
            decide_poll(&pane, &idle, &HashMap::new(), &config, &None),
            PollAction::NoResume {
                agent: Some("claude".into()),
                value_known: false,
            }
        );
    }

    #[test]
    fn partial_corruption_poisons_whole_parse() {
        // One parseable idle shell + one dropped entry → sentinel pushed,
        // so the whole parse vetoes (a dropped entry could be a live agent).
        let info: serde_json::Value = serde_json::from_str(
            r#"{"foreground_processes":[{"argv":["/usr/bin/zsh"]}, {"foo":1}]}"#,
        )
        .expect("fixture parses");
        let argv = foreground_argv(&info);
        assert!(argv.contains(&vec!["<unparseable>".to_string()]));
        assert!(!should_relaunch(&argv, None));
    }

    #[test]
    fn legacy_processes_key_is_rejected() {
        // Deliberate narrowing: only `foreground_processes` / `argv` are
        // honored. A legacy `processes` key yields empty (vetoes relaunch).
        let info: serde_json::Value = serde_json::from_str(
            r#"{"processes":[{"argv":["/usr/bin/zsh"]}]}"#,
        )
        .expect("fixture parses");
        assert_eq!(foreground_argv(&info), Vec::<Vec<String>>::new());
    }

    #[test]
    fn decide_poll_falls_back_to_valueless_kiro_template() {
        // kiro + idle shell + empty registry + valued primary + valueless
        // fallback → relaunch via the fallback template.
        let mut commands = HashMap::new();
        commands.insert(
            "kiro".to_string(),
            "kiro-cli chat --resume-id {value}".to_string(),
        );
        commands.insert(
            "kiro-fallback".to_string(),
            "kiro-cli chat -r".to_string(),
        );
        let config = Config {
            commands,
            ..Config::default()
        };
        let pane = Pane { pane_id: "w1:p1".into(), agent: Some("kiro".into()),
            agent_status: Some("unknown".into()), ..Default::default() };
        let idle = vec![vec!["/usr/bin/zsh".into()]];
        assert_eq!(
            decide_poll(&pane, &idle, &HashMap::new(), &config, &None),
            PollAction::Relaunch {
                agent: "kiro".into(),
                argv: vec!["kiro-cli".into(), "chat".into(), "-r".into()],
            }
        );
    }

    use crate::test_support::run_with_fake_herdr;

    /// Hermetic harness: fake `HERDR_BIN_PATH` + temp state dir (both
    /// `HERDR_PLUGIN_STATE_DIR` and `HERDR_PLUGIN_CONFIG_DIR` point at it).
    /// Thin wrapper over `crate::test_support::run_with_fake_herdr` (kept
    /// so existing tests are untouched).
    fn with_poll_harness(script_body: &str, f: impl FnOnce(&std::path::PathBuf)) {
        run_with_fake_herdr(script_body, &[], |dir| f(&dir.to_path_buf()));
    }

    #[test]
    fn poll_once_refresh_remembers_session() {
        let script = "#!/bin/sh\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w1:p1\",\"agent\":\"claude\",\"agent_status\":\"working\",\"agent_session\":{\"agent\":\"claude\",\"value\":\"live-1\"}}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"claude\",\"--resume\",\"live-1\"]}]}}}'\nexit 0\nfi\nexit 1\n";
        with_poll_harness(script, |_| {
            let config = Config::default();
            let mut cooldown: Option<Instant> = None;
            assert!(poll_once("w1:p1", &config, &mut cooldown));
            assert_eq!(cooldown, None);
            let reg = crate::state::load_registry();
            assert_eq!(reg.get("w1:p1").expect("session remembered").value, "live-1");
        });
    }

    #[test]
    fn poll_once_relaunch_sets_cooldown() {
        let script = "#!/bin/sh\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w1:p1\",\"agent_status\":\"unknown\"}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"/usr/bin/zsh\"]}]}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"run\" ]; then\necho '{\"id\":\"cli:pane:run\",\"result\":{\"ok\":true}}'\nexit 0\nfi\nexit 1\n";
        with_poll_harness(script, |state_dir| {
            crate::state::remember(
                "w1:p1",
                SessionRef {
                    agent: "claude".into(),
                    value: "abc-123".into(),
                },
            );
            let config = Config::default();
            let mut cooldown: Option<Instant> = None;
            assert!(poll_once("w1:p1", &config, &mut cooldown));
            assert!(cooldown.is_some(), "successful relaunch sets cooldown");
            let log = std::fs::read_to_string(state_dir.join("log.txt"))
                .expect("log exists");
            assert!(log.contains("relaunched claude"), "log names agent: {log}");
        });
    }

    #[test]
    fn poll_once_process_info_failure_vetoes_relaunch() {
        let script = "#!/bin/sh\necho \"$1 $2 $3\" >> \"@CALL_LOG@\"\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w1:p1\",\"agent_status\":\"unknown\"}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\nexit 1\nfi\nexit 1\n";
        with_poll_harness(script, |state_dir| {
            crate::state::remember(
                "w1:p1",
                SessionRef {
                    agent: "claude".into(),
                    value: "abc-123".into(),
                },
            );
            let config = Config::default();
            let mut cooldown: Option<Instant> = None;
            assert!(poll_once("w1:p1", &config, &mut cooldown));
            assert_eq!(cooldown, None, "vetoed poll must not set cooldown");
            let calls_path = state_dir.join("calls.log");
            let calls = std::fs::read_to_string(&calls_path).expect("calls log exists");
            assert!(calls.contains("pane get"), "fake must have seen pane get: {calls}");
            assert!(calls.contains("pane process-info"), "fake must have seen process-info: {calls}");
            assert!(!calls.contains("pane run"), "no pane run may be invoked: {calls}");
            let log = std::fs::read_to_string(state_dir.join("log.txt"))
                .expect("log exists");
            assert!(log.contains("vetoing relaunch"), "log names veto: {log}");
        });
    }

    #[test]
    fn finish_monitor_clears_lock_and_sentinel() {
        with_poll_harness("#!/bin/sh\nexit 1\n", |_| {
            let pane_id = "w1:p1";
            let own_pid = 424243u32;
            assert!(crate::state::write_monitor_lock_pid(pane_id, own_pid));
            std::fs::write(crate::state::stop_sentinel_path(own_pid), b"stop")
                .expect("sentinel");
            finish_monitor(pane_id, own_pid);
            assert!(
                !crate::state::monitor_lock_path(pane_id).exists(),
                "lock cleared"
            );
            assert!(
                !crate::state::stop_sentinel_path(own_pid).exists(),
                "sentinel cleared"
            );
        });
    }
}
