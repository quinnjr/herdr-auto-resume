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

/// Kiro session pinning (thin I/O wrapper around [`crate::kiro`]).
///
/// Live kiro CLIs run with a bare `--resume` (no id on the cmdline) and
/// herdr often reports no `agent_session`, so the registry would stay
/// empty and a later crash would fall back to `chat -r` — wrong with
/// several same-folder sessions. While the pane is supervised and the
/// registry lacks it, pin the freshest `chat -l` id for the pane cwd.
/// The live primary re-saves on every turn, so it outranks stale
/// subagent runs. Best-effort: any failure leaves the registry
/// untouched and the existing decide path applies. `decide_poll` stays
/// pure; with the registry populated its Refresh/Relaunch arms just work.
fn maybe_pin_kiro_session(pane: &Pane) {
    if pane.agent.as_deref() != Some("kiro") {
        return;
    }
    if crate::state::load_registry().contains_key(&pane.pane_id) {
        return;
    }
    let Some(cwd) = pane.cwd.as_deref() else {
        return;
    };
    if cwd.is_empty() {
        return;
    }
    let Some(id) = crate::kiro::latest_session_for_cwd(cwd) else {
        return;
    };
    crate::state::remember(
        &pane.pane_id,
        SessionRef {
            agent: "kiro".into(),
            value: id,
        },
    );
}

/// Per-monitor mutable poll state: the post-relaunch cooldown, the
/// last-logged `NoResume` signature (repeat identical no-resume polls
/// stay quiet), the warn-once marker for repetitive failure logs, and
/// whether a valueless fallback relaunch was already spent.
#[derive(Debug, Default)]
struct MonitorState {
    cooldown_until: Option<Instant>,
    last_noresume: Option<(Option<String>, bool)>,
    last_warn: Option<WarnKind>,
    fallback_used: bool,
}

/// Repetitive failure logs, warned once until a successful action
/// clears them (same spam philosophy as the `NoResume` throttle).
/// Single slot by design: interleaved kinds re-log, which is accepted
/// (alternating failures are themselves new information).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WarnKind {
    RunFailed,
    ProcInfoFailed,
    FallbackSpent,
}

impl MonitorState {
    /// Log `line` unless it is the same warning as last time.
    fn warn_once(&mut self, kind: WarnKind, line: String) {
        if self.last_warn != Some(kind) {
            self.last_warn = Some(kind);
            crate::state::append_log(&line);
        }
    }

    /// A successful Refresh or relaunch clears all warn state: the world
    /// changed, so the next failure is new information again.
    fn clear_warns(&mut self) {
        self.last_warn = None;
        self.last_noresume = None;
    }
}

/// One monitor poll. Returns false when the pane no longer exists
/// (`pane_get` → None) so the caller can count consecutive misses.
/// Thin I/O shell around the pure `decide_poll`.
fn poll_once(pane_id: &str, config: &Config, st: &mut MonitorState) -> bool {
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
                // Warn-once: a persistently broken shape must not spam
                // every poll (same philosophy as the NoResume throttle).
                let keys = v
                    .as_object()
                    .map(|o| {
                        o.keys()
                            .map(String::as_str)
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_else(|| "<non-object>".to_string());
                st.warn_once(
                    WarnKind::ProcInfoFailed,
                    format!(
                        "monitor {pane_id}: empty/unparseable foreground parse; keys={keys}"
                    ),
                );
            }
            argv
        }
        None => {
            st.warn_once(
                WarnKind::ProcInfoFailed,
                format!("monitor {pane_id}: process-info failed; vetoing relaunch"),
            );
            vec![]
        }
    };
    // Kiro session pinning (best-effort discovery; registry reloaded after).
    maybe_pin_kiro_session(&pane);
    let reg = crate::state::load_registry();
    match decide_poll(&pane, &proc_argv, &reg, config, &st.cooldown_until) {
        PollAction::Refresh(sess) => {
            // A live agent re-arms everything: registry (valued path for
            // the next crash) and the fallback + warn state below. The
            // fallback flag re-arms only on a valued session: a valueless
            // Refresh is refused by `remember` and must not re-arm either.
            if !sess.value.is_empty() {
                st.fallback_used = false;
            }
            st.clear_warns();
            crate::state::remember(pane_id, sess);
        }
        PollAction::Relaunch { agent, argv } => {
            // Valueless fallback relaunches (e.g. kiro `chat -r`) carry
            // no session to spend, so the registry consume below cannot
            // stop them: gate them one-shot in memory instead. A later
            // Refresh (valued session known again) re-arms.
            let valued = crate::resume::resolve_session_with_commands(
                &pane,
                reg.get(&pane.pane_id),
                &proc_argv,
                &config.commands,
            )
            .is_some_and(|s| !s.value.is_empty());
            if !valued && st.fallback_used {
                st.warn_once(
                    WarnKind::FallbackSpent,
                    format!(
                        "monitor {pane_id}: fallback relaunch already spent for '{agent}'; staying dead until a valued session is known"
                    ),
                );
                return true;
            }
            let args: Vec<&str> = argv.iter().map(String::as_str).collect();
            if herdr::pane_run(pane_id, &args) {
                st.cooldown_until =
                    Some(Instant::now() + Duration::from_secs(config.cooldown_seconds));
                // One-shot resurrection: the relaunch spends the saved
                // session. A live agent re-arms via Refresh; an
                // intentionally-exited one stays dead after this comeback.
                // Spending only on success keeps retrying failed delivery.
                // (Fallback relaunches spend the in-memory flag instead:
                // there is no registry entry to consume.)
                crate::state::forget(pane_id);
                // Any successful relaunch — valued or fallback — spends
                // the fallback one-shot too: otherwise a valued comeback
                // followed by another death would fallback-resurrect on
                // top, violating one-shot. Only a later live Refresh
                // re-arms.
                st.fallback_used = true;
                st.clear_warns();
                crate::state::append_log(&format!("monitor {pane_id}: relaunched {agent}"));
            } else {
                st.warn_once(
                    WarnKind::RunFailed,
                    format!("monitor {pane_id}: pane run failed for {agent}"),
                );
            }
        }
        PollAction::NoResume { agent, value_known } => {
            let key = (agent.clone(), value_known);
            if st.last_noresume.as_ref() != Some(&key) {
                st.last_noresume = Some(key);
                let agent_name = agent.unwrap_or_default();
                crate::state::append_log(&format!(
                    "monitor {pane_id}: no resume argv for '{agent_name}' (value known: {value_known})"
                ));
            }
        }
        // Deliberately retains the throttle: transient liveness between
        // identical NoResume polls is a blip, not new information.
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
    let mut st = MonitorState::default();
    // A pane that stays gone (closed/deleted) must not spin a monitor
    // forever: exit and clear the lock after 10 consecutive misses.
    let mut missing = 0u32;
    loop {
        if crate::state::stop_requested(own_pid) {
            finish_monitor(pane_id, own_pid);
            break;
        }
        if poll_once(pane_id, &config, &mut st) {
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

    use crate::test_support::{herdr_calls, run_with_fake_herdr};

    #[test]
    fn poll_once_refresh_remembers_session() {
        let script = "#!/bin/sh\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w1:p1\",\"agent\":\"claude\",\"agent_status\":\"working\",\"agent_session\":{\"agent\":\"claude\",\"value\":\"live-1\"}}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"claude\",\"--resume\",\"live-1\"]}]}}}'\nexit 0\nfi\nexit 1\n";
        run_with_fake_herdr(script, &[], |_| {
            let config = Config::default();
            let mut st = MonitorState::default();
            assert!(poll_once("w1:p1", &config, &mut st));
            assert_eq!(st.cooldown_until, None);
            let reg = crate::state::load_registry();
            assert_eq!(reg.get("w1:p1").expect("session remembered").value, "live-1");
        });
    }

    #[test]
    fn poll_once_relaunch_consumes_registry_entry() {
        // One-shot resurrection: a successful relaunch spends the saved
        // session, so an intentionally-exited agent stays dead after one
        // comeback (a live agent re-arms via Refresh).
        let script = "#!/bin/sh\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w1:p1\",\"agent\":\"claude\",\"agent_status\":\"unknown\"}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"/usr/bin/zsh\"]}]}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"run\" ]; then\necho '{\"id\":\"cli:pane:run\",\"result\":{\"ok\":true}}'\nexit 0\nfi\nexit 1\n";
        run_with_fake_herdr(script, &[], |_| {
            crate::state::remember(
                "w1:p1",
                SessionRef {
                    agent: "claude".into(),
                    value: "abc-123".into(),
                },
            );
            let config = Config::default();
            let mut st = MonitorState::default();
            assert!(poll_once("w1:p1", &config, &mut st));
            assert!(st.cooldown_until.is_some());
            assert!(
                !crate::state::load_registry().contains_key("w1:p1"),
                "successful relaunch must spend the registry entry"
            );
        });
    }

    #[test]
    fn poll_once_failed_relaunch_keeps_registry() {
        // Delivery failure must NOT spend the entry: the next cooldown
        // expiry retries with the session still known.
        let script = "#!/bin/sh\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w1:p1\",\"agent\":\"claude\",\"agent_status\":\"unknown\"}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"/usr/bin/zsh\"]}]}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"run\" ]; then\nexit 1\nfi\nexit 1\n";
        run_with_fake_herdr(script, &[], |state_dir| {
            crate::state::remember(
                "w1:p1",
                SessionRef {
                    agent: "claude".into(),
                    value: "abc-123".into(),
                },
            );
            let config = Config::default();
            let mut st = MonitorState::default();
            assert!(poll_once("w1:p1", &config, &mut st));
            assert_eq!(st.cooldown_until, None);
            assert_eq!(
                crate::state::load_registry().get("w1:p1").map(|s| s.value.as_str()),
                Some("abc-123"),
                "failed delivery must keep the registry entry"
            );
            assert!(poll_once("w1:p1", &config, &mut st)); // retry fails too
            let log = std::fs::read_to_string(state_dir.join("log.txt")).unwrap_or_default();
            assert_eq!(
                log.lines().filter(|l| l.contains("pane run failed")).count(),
                1,
                "repeat delivery failures must log once, got: {log}"
            );
        });
    }

    #[test]
    fn poll_once_noresume_logs_once_until_state_changes() {
        // A spent/unknown session logs NoResume once; identical polls stay
        // quiet, while an intervening Refresh re-arms the log (new info).
        // Separate get/info phase flags so poll 1 is a true Refresh
        // (live agent + live argv) and polls 2+ are dead-idle NoResume.
        // The live agent is `zed` (mismatched with the later `claude`
        // pane) so its remembered session never resolves: polls 2+
        // stay NoResume instead of becoming valued relaunches.
        let script = "#!/bin/sh\nD=\"$(dirname \"@CALL_LOG@\")\"\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\nif [ -f \"$D/get2\" ]; then\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w1:p1\",\"agent\":\"claude\",\"agent_status\":\"unknown\"}}}'\nelse\ntouch \"$D/get2\"\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w1:p1\",\"agent\":\"zed\",\"agent_status\":\"working\",\"agent_session\":{\"agent\":\"zed\",\"value\":\"s1\"}}}}'\nfi\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\nif [ -f \"$D/info2\" ]; then\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"/usr/bin/zsh\"]}]}}}'\nelse\ntouch \"$D/info2\"\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"zed\"]}]}}}'\nfi\nexit 0\nfi\nexit 1\n";
        run_with_fake_herdr(script, &[], |state_dir| {
            let config = Config::default();
            let mut st = MonitorState::default();
            assert!(poll_once("w1:p1", &config, &mut st)); // Refresh (live)
            assert_eq!(
                crate::state::load_registry().get("w1:p1").map(|s| s.value.as_str()),
                Some("s1"),
                "poll 1 must Refresh the live session"
            );
            assert!(poll_once("w1:p1", &config, &mut st)); // NoResume (logs)
            assert!(poll_once("w1:p1", &config, &mut st)); // NoResume (quiet)
            assert!(poll_once("w1:p1", &config, &mut st)); // NoResume (quiet)
            let log = std::fs::read_to_string(state_dir.join("log.txt")).unwrap_or_default();
            assert_eq!(
                log.lines().filter(|l| l.contains("no resume argv")).count(),
                1,
                "identical NoResume polls must log once, got: {log}"
            );
        });
    }

    #[test]
    fn poll_once_fallback_relaunch_is_one_shot() {
        // Valueless fallback relaunches (kiro `chat -r`) carry no session
        // to spend: the in-memory flag stops the loop instead. Expire the
        // cooldown manually between polls to simulate time passing.
        let script = "#!/bin/sh\necho \"$#:$@\" >> \"@CALL_LOG@\"\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w9M:p1\",\"agent\":\"kiro\",\"agent_status\":\"unknown\"}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"/usr/bin/zsh\"]}]}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"run\" ]; then\necho '{\"id\":\"cli:pane:run\",\"result\":{\"ok\":true}}'\nexit 0\nfi\nexit 1\n";
        run_with_fake_herdr(script, &[], |state_dir| {
            let config = Config::default();
            let mut st = MonitorState::default();
            assert!(poll_once("w9M:p1", &config, &mut st)); // fallback relaunch
            assert!(st.cooldown_until.is_some());
            st.cooldown_until = None; // simulate expiry
            assert!(poll_once("w9M:p1", &config, &mut st)); // spent: skip
            assert!(poll_once("w9M:p1", &config, &mut st)); // spent: skip
            let calls = crate::test_support::herdr_calls(state_dir);
            assert_eq!(
                calls.lines().filter(|l| l.contains("pane run")).count(),
                1,
                "fallback must run exactly once, got: {calls}"
            );
            let log = std::fs::read_to_string(state_dir.join("log.txt")).unwrap_or_default();
            assert_eq!(
                log.lines().filter(|l| l.contains("fallback relaunch already spent")).count(),
                1,
                "spent fallback must log once, got: {log}"
            );
        });
    }

    #[test]
    fn poll_once_refresh_rearms_fallback() {
        // Fallback spent in poll 1, Refresh re-arms in poll 2, valued
        // relaunch proceeds in poll 3. Per-call counters (separate get /
        // info files beside the calls log) stage the three phases.
        let script = "#!/bin/sh\nD=\"$(dirname \"@CALL_LOG@\")\"\nstep() {\nF=\"$D/$1\"\nn=$(cat \"$F\" 2>/dev/null || echo 0)\necho $((n + 1)) > \"$F\"\necho \"$n\"\n}\necho \"$#:$@\" >> \"@CALL_LOG@\"\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\ncase \"$(step get)\" in\n0) echo '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w9M:p1\",\"agent\":\"kiro\",\"agent_status\":\"unknown\"}}}' ;;\n1) echo '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w9M:p1\",\"agent\":\"kiro\",\"agent_status\":\"working\",\"agent_session\":{\"agent\":\"kiro\",\"value\":\"sess-9\"}}}}' ;;\n*) echo '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w9M:p1\",\"agent\":\"kiro\",\"agent_status\":\"unknown\"}}}' ;;\nesac\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\ncase \"$(step info)\" in\n0) echo '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"/usr/bin/zsh\"]}]}}}' ;;\n1) echo '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"kiro-cli\",\"chat\",\"--resume-id\",\"sess-9\"]}]}}}' ;;\n*) echo '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"/usr/bin/zsh\"]}]}}}' ;;\nesac\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"run\" ]; then\necho '{\"id\":\"cli:pane:run\",\"result\":{\"ok\":true}}'\nexit 0\nfi\nexit 1\n";
        run_with_fake_herdr(script, &[], |state_dir| {
            let config = Config::default();
            let mut st = MonitorState::default();
            assert!(poll_once("w9M:p1", &config, &mut st)); // fallback relaunch
            st.cooldown_until = None;
            assert!(poll_once("w9M:p1", &config, &mut st)); // Refresh re-arms
            assert!(
                !st.fallback_used,
                "Refresh with a valued session must re-arm the fallback"
            );
            assert_eq!(
                crate::state::load_registry().get("w9M:p1").map(|s| s.value.as_str()),
                Some("sess-9")
            );
            st.cooldown_until = None;
            assert!(poll_once("w9M:p1", &config, &mut st)); // valued relaunch
            assert!(
                st.fallback_used,
                "valued relaunch must spend the fallback one-shot too"
            );
            let calls = crate::test_support::herdr_calls(state_dir);
            assert_eq!(
                calls.lines().filter(|l| l.contains("pane run")).collect::<Vec<_>>(),
                vec![
                    "4:pane run w9M:p1 kiro-cli chat -r",
                    "4:pane run w9M:p1 kiro-cli chat --resume-id sess-9",
                ],
                "fallback then valued relaunch, got: {calls}"
            );
            assert!(
                !crate::state::load_registry().contains_key("w9M:p1"),
                "valued relaunch spends the entry"
            );
        });
    }

    #[test]
    fn poll_once_relaunch_sets_cooldown() {
        let script = "#!/bin/sh\necho \"$#:$@\" >> \"@CALL_LOG@\"\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w1:p1\",\"agent_status\":\"unknown\"}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"/usr/bin/zsh\"]}]}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"run\" ]; then\necho '{\"id\":\"cli:pane:run\",\"result\":{\"ok\":true}}'\nexit 0\nfi\nexit 1\n";
        run_with_fake_herdr(script, &[], |state_dir| {
            crate::state::remember(
                "w1:p1",
                SessionRef {
                    agent: "claude".into(),
                    value: "abc-123".into(),
                },
            );
            let config = Config::default();
            let mut st = MonitorState::default();
            assert!(poll_once("w1:p1", &config, &mut st));
            assert!(st.cooldown_until.is_some(), "successful relaunch sets cooldown");
            let log = std::fs::read_to_string(state_dir.join("log.txt"))
                .expect("log exists");
            assert!(log.contains("relaunched claude"), "log names agent: {log}");
            // Delivery proof (see `pane_run` docs): the full call sequence
            // is exactly get + process-info + one joined `pane run`.
            let calls = herdr_calls(state_dir);
            assert_eq!(
                calls.lines().collect::<Vec<_>>(),
                vec![
                    "3:pane get w1:p1",
                    "4:pane process-info --pane w1:p1",
                    "4:pane run w1:p1 claude --resume abc-123",
                ],
                "exact delivery sequence, got: {calls}"
            );
        });
    }

    #[test]
    fn poll_once_process_info_failure_vetoes_relaunch() {
        let script = "#!/bin/sh\necho \"$1 $2 $3\" >> \"@CALL_LOG@\"\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w1:p1\",\"agent_status\":\"unknown\"}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\nexit 1\nfi\nexit 1\n";
        run_with_fake_herdr(script, &[], |state_dir| {
            crate::state::remember(
                "w1:p1",
                SessionRef {
                    agent: "claude".into(),
                    value: "abc-123".into(),
                },
            );
            let config = Config::default();
            let mut st = MonitorState::default();
            assert!(poll_once("w1:p1", &config, &mut st));
            assert_eq!(st.cooldown_until, None, "vetoed poll must not set cooldown");
            assert!(poll_once("w1:p1", &config, &mut st)); // still vetoed
            let calls = herdr_calls(state_dir);
            assert!(calls.contains("pane get"), "fake must have seen pane get: {calls}");
            assert!(calls.contains("pane process-info"), "fake must have seen process-info: {calls}");
            assert!(!calls.contains("pane run"), "no pane run may be invoked: {calls}");
            let log = std::fs::read_to_string(state_dir.join("log.txt"))
                .expect("log exists");
            assert_eq!(
                log.lines().filter(|l| l.contains("vetoing relaunch")).count(),
                1,
                "repeat process-info failures must log once, got: {log}"
            );
        });
    }

    #[test]
    fn poll_once_pins_kiro_session_from_discovery() {
        // Live kiro pane (bare `--resume` argv carries no id, herdr
        // reports no agent_session): the poll must pin the freshest
        // `chat -l` id for the pane cwd into the registry.
        let script = "#!/bin/sh\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w9M:p1\",\"agent\":\"kiro\",\"agent_status\":\"working\",\"cwd\":\"/\"}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"kiro-cli\",\"--resume\"]}]}}}'\nexit 0\nfi\nexit 1\n";
        run_with_fake_herdr(script, &[], |state_dir| {
            let list = r#"[{"cwd":"/","sessions":[
                {"sessionId":"sess-old-9","source":"v2","title":"old","updatedAt":"2026-09-15T20:00:00.000Z","messageCount":12},
                {"sessionId":"sess-pinned-1","source":"v2","title":"live work","updatedAt":"2026-09-15T22:32:18.685Z","messageCount":44}
            ]}]"#;
            std::fs::write(state_dir.join("list.json"), list).expect("fixture");
            let fake = state_dir.join("kiro-cli");
            std::fs::write(
                &fake,
                format!("#!/bin/sh\ncat \"{}\"\n", state_dir.join("list.json").display()),
            )
            .expect("fake kiro");
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
            let prev = std::env::var_os("KIRO_BIN_PATH");
            std::env::set_var("KIRO_BIN_PATH", &fake);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let config = Config::default();
                let mut st = MonitorState::default();
                assert!(poll_once("w9M:p1", &config, &mut st));
                let reg = crate::state::load_registry();
                assert_eq!(
                    reg.get("w9M:p1")
                        .map(|s| (s.agent.as_str(), s.value.as_str())),
                    Some(("kiro", "sess-pinned-1"))
                );
            }));
            match prev {
                Some(v) => std::env::set_var("KIRO_BIN_PATH", v),
                None => std::env::remove_var("KIRO_BIN_PATH"),
            }
            assert!(result.is_ok());
        });
    }

    #[test]
    fn finish_monitor_clears_lock_and_sentinel() {
        run_with_fake_herdr("#!/bin/sh\nexit 1\n", &[], |_| {
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
