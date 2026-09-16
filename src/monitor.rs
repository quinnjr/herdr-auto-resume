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
    let Some(argv) = crate::resume::resume_argv(
        &agent,
        value.as_deref(),
        &config.commands,
        config.resume_message.as_deref(),
    ) else {
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
/// stay quiet), the warn-once marker for repetitive failure logs,
/// whether a valueless fallback relaunch was already spent, and whether
/// our last delivery was exit-marker-wrapped (only a pending wrapped
/// run's marker may classify a death — older markers are stale).
#[derive(Debug, Default)]
struct MonitorState {
    cooldown_until: Option<Instant>,
    last_noresume: Option<(Option<String>, bool)>,
    last_warn: Option<WarnKind>,
    fallback_used: bool,
    wrapped_pending: bool,
    /// Two-step continue: (agent, program, message) awaiting TUI boot,
    /// plus polls remaining before it is submitted (agents whose
    /// template carries no `{message}`, e.g. opencode — kiro inlines
    /// instead). The program gates delivery: only the relaunched agent
    /// itself may receive the text, never vim/less/tmux opened meanwhile.
    pending_continue: Option<(String, String, String)>,
    continue_in: u8,
}

/// Polls between a relaunch and its two-step continue submission:
/// long enough for typical TUI boot, short enough to stay relevant.
const CONTINUE_AFTER_POLLS: u8 = 3;

/// Repetitive failure logs, warned once until a successful action
/// clears them (same spam philosophy as the `NoResume` throttle).
/// Single slot by design: interleaved kinds re-log, which is accepted
/// (alternating failures are themselves new information).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WarnKind {
    RunFailed,
    ProcInfoFailed,
    FallbackSpent,
    ReadFailed,
    CleanExit,
    ContinueFailed,
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

/// True when some foreground entry runs `program` (argv[0] basename
/// match): the sufficient half of send-readiness. A pending continue
/// belongs to the relaunched agent — never to vim/less/tmux the user
/// opened in the pane meanwhile, which `foreground_looks_live` alone
/// would accept.
fn foreground_matches_program(proc_argv: &[Vec<String>], program: &str) -> bool {
    proc_argv.iter().any(|a| {
        a.first().is_some_and(|prog| {
            prog.rsplit('/').next().unwrap_or(prog) == program
        })
    })
}

/// The resume template actually used for this relaunch (valued primary
/// vs valueless fallback), mirroring `resume_argv`'s selection.
fn used_template<'a>(
    agent: &str,
    valued: bool,
    commands: &'a HashMap<String, String>,
) -> Option<&'a String> {
    if valued {
        commands.get(agent)
    } else {
        let fallback_key = format!("{agent}-fallback");
        match commands.get(&fallback_key) {
            Some(fb)
                if !fb.is_empty()
                    && !fb.contains(crate::resume::VALUE_PLACEHOLDER) =>
            {
                Some(fb)
            }
            _ => commands.get(agent),
        }
    }
}

/// True when the resume template actually used for this relaunch
/// carries `{message}` (inline delivery, e.g. kiro).
fn template_inlines_message(
    agent: &str,
    valued: bool,
    commands: &HashMap<String, String>,
) -> bool {
    used_template(agent, valued, commands)
        .is_some_and(|t| t.contains(crate::resume::MESSAGE_PLACEHOLDER))
}

/// True when the foreground looks like a live agent (never a shell in
/// any form, never unparseable): necessary but not sufficient — see
/// `foreground_matches_program` for the sufficient half.
fn foreground_looks_live(proc_argv: &[Vec<String>]) -> bool {
    if proc_argv.is_empty() {
        return false;
    }
    proc_argv.iter().all(|a| {
        if a.iter().any(|t| t == UNPARSEABLE) {
            return false;
        }
        let Some(prog) = a.first() else {
            return false;
        };
        let base = prog.rsplit('/').next().unwrap_or(prog);
        let base = base.strip_prefix('-').unwrap_or(base);
        !IDLE_SHELLS.contains(&base)
    })
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
    // Skips the countdown tick below when this poll armed it, so the
    // message goes out three full polls after the relaunch, not two.
    let mut skip_tick = false;
    match decide_poll(&pane, &proc_argv, &reg, config, &st.cooldown_until) {
        PollAction::Refresh(sess) => {
            // A live agent re-arms everything: registry (valued path for
            // the next crash) and the fallback + warn state below. The
            // fallback flag re-arms only on a valued session: a valueless
            // Refresh is refused by `remember` and must not re-arm either.
            // A live observation does NOT retire a pending exit marker:
            // the running agent may still be our wrapped run, whose
            // eventual death marker is exactly what classifies it.
            if !sess.value.is_empty() {
                st.fallback_used = false;
            }
            // A live agent of a DIFFERENT name retires a pending
            // continue: the message belongs to the relaunched agent,
            // never to whatever the user started manually afterwards.
            if let Some((pending_agent, _, _)) = &st.pending_continue {
                if pending_agent != &sess.agent {
                    st.pending_continue = None;
                }
            }
            st.clear_warns();
            crate::state::remember(pane_id, sess);
        }
        PollAction::Relaunch { agent, argv } => {
            // Exit-gated resurrection (POSIX idle shells only): when our
            // last delivery was exit-marker-wrapped, scrollback tells a
            // clean exit from a crash. A clean exit spends everything and
            // stays dead — no comebacks. Anything else (crash marker, no
            // marker yet, non-POSIX shell) falls through to the one-shot
            // rules below; an unreadable scrollback skips fail-safe
            // toward user peace (a transient blip self-heals next poll).
            let posix = crate::herdr::foreground_shell_is_posix(&proc_argv);
            if posix && st.wrapped_pending {
                match crate::herdr::pane_read(pane_id)
                    .as_deref()
                    .map(crate::herdr::parse_exit_marker)
                {
                    Some(Some(0)) => {
                        crate::state::forget(pane_id);
                        st.fallback_used = true;
                        st.wrapped_pending = false;
                        // The agent that would have received a pending
                        // continue is gone by its own clean hand: disarm.
                        st.pending_continue = None;
                        st.continue_in = 0;
                        st.warn_once(
                            WarnKind::CleanExit,
                            format!(
                                "monitor {pane_id}: clean exit for '{agent}'; staying dead"
                            ),
                        );
                        return true;
                    }
                    Some(_) => {}
                    None => {
                        st.warn_once(
                            WarnKind::ReadFailed,
                            format!(
                                "monitor {pane_id}: pane read failed; staying dead until scrollback is readable"
                            ),
                        );
                        return true;
                    }
                }
            }
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
                // No relaunch is forthcoming on this dead pane: a pending
                // continue belongs to a gone agent, so disarm (a future
                // manual agent must not receive stale text).
                st.pending_continue = None;
                st.continue_in = 0;
                st.warn_once(
                    WarnKind::FallbackSpent,
                    format!(
                        "monitor {pane_id}: fallback relaunch already spent for '{agent}'; staying dead until a valued session is known"
                    ),
                );
                return true;
            }
            let args: Vec<&str> = argv.iter().map(String::as_str).collect();
            let sent = if posix {
                herdr::pane_run_wrapped(pane_id, &args)
            } else {
                herdr::pane_run(pane_id, &args)
            };
            if sent {
                st.cooldown_until =
                    Some(Instant::now() + Duration::from_secs(config.cooldown_seconds));
                // One-shot resurrection: the relaunch spends the saved
                // session (failed delivery keeps it and retries).
                // (Fallback relaunches spend the in-memory flag instead:
                // there is no registry entry to consume.)
                crate::state::forget(pane_id);
                // Any successful relaunch — valued or fallback — spends
                // the fallback one-shot too: otherwise a valued comeback
                // followed by another death would fallback-resurrect on
                // top. Only a later live Refresh re-arms.
                st.fallback_used = true;
                // A POSIX wrapped delivery arms exit classification for
                // the next death; anything else leaves no marker behind.
                st.wrapped_pending = posix;
                // Two-step continue: a configured message the template
                // did not inline (e.g. opencode) is submitted after a few
                // polls once the TUI is plausibly booted.
                if let Some(message) = config.resume_message.clone() {
                    if !template_inlines_message(&agent, valued, &config.commands) {
                        let program = used_template(&agent, valued, &config.commands)
                            .and_then(|t| crate::resume::split_argv(t).into_iter().next())
                            .map(|tok| {
                                tok.rsplit('/').next().unwrap_or(&tok).to_string()
                            })
                            .unwrap_or_else(|| agent.clone());
                        st.pending_continue = Some((agent.clone(), program, message));
                        st.continue_in = CONTINUE_AFTER_POLLS;
                        skip_tick = true;
                    }
                }
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
            // Same dead-end as the fallback skip above: disarm first.
            st.pending_continue = None;
            st.continue_in = 0;
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
        // Nothing also covers cooling-down idle panes; either way a
        // pending exit marker stands — only a clean classification or
        // a monitor restart retires it.
        PollAction::Nothing => {}
    }
    // Two-step continue tick: submit once the countdown lapses, the
    // foreground positively looks like a live agent, AND it runs the
    // relaunched program (never vim/less/tmux opened meanwhile, never
    // chat text into a shell). Anything else waits silently; a gone
    // pane exits via the miss path above, dropping the pending message.
    if st.pending_continue.is_some() && !skip_tick {
        if st.continue_in > 0 {
            st.continue_in -= 1;
        }
        if st.continue_in == 0
            && foreground_looks_live(&proc_argv)
            && st.pending_continue.as_ref().is_some_and(|(_, program, _)| {
                foreground_matches_program(&proc_argv, program)
            })
        {
            if let Some((agent, _, message)) = st.pending_continue.take() {
                if herdr::send_text_enter(pane_id, &message) {
                    st.clear_warns();
                    crate::state::append_log(&format!(
                        "monitor {pane_id}: submitted continue to {agent}"
                    ));
                } else {
                    st.warn_once(
                        WarnKind::ContinueFailed,
                        format!("monitor {pane_id}: continue submit failed for {agent}"),
                    );
                }
            }
        }
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
    fn decide_poll_relaunch_carries_configured_message() {
        // `resume_message` flows from config into the relaunch argv when
        // the template carries `{message}` (default kiro templates do).
        let config = Config {
            resume_message: Some("continue".into()),
            ..Config::default()
        };
        let pane = Pane { pane_id: "w9M:p1".into(), agent: Some("kiro".into()),
            agent_status: Some("idle".into()), ..Default::default() };
        let argv = vec![vec!["/usr/bin/zsh".into()]];
        let reg = HashMap::from([("w9M:p1".into(), SessionRef {
            agent: "kiro".into(), value: "sess-9".into(),
        })]);
        assert_eq!(
            decide_poll(&pane, &argv, &reg, &config, &None),
            PollAction::Relaunch {
                agent: "kiro".into(),
                argv: vec![
                    "kiro-cli".into(), "chat".into(), "--resume-id".into(),
                    "sess-9".into(), "continue".into(),
                ],
            }
        );
        // Without the setting the argv is identical to before.
        assert_eq!(
            decide_poll(&pane, &argv, &reg, &Config::default(), &None),
            PollAction::Relaunch {
                agent: "kiro".into(),
                argv: vec![
                    "kiro-cli".into(), "chat".into(), "--resume-id".into(),
                    "sess-9".into(),
                ],
            }
        );
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
        let script = "#!/bin/sh\necho \"$#:$@\" >> \"@CALL_LOG@\"\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w9M:p1\",\"agent\":\"kiro\",\"agent_status\":\"unknown\"}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"/usr/bin/zsh\"]}]}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"read\" ]; then\necho '@@AUTORESUME-EXIT:137@@'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"run\" ]; then\necho '{\"id\":\"cli:pane:run\",\"result\":{\"ok\":true}}'\nexit 0\nfi\nexit 1\n";
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
        let script = "#!/bin/sh\nD=\"$(dirname \"@CALL_LOG@\")\"\nstep() {\nF=\"$D/$1\"\nn=$(cat \"$F\" 2>/dev/null || echo 0)\necho $((n + 1)) > \"$F\"\necho \"$n\"\n}\necho \"$#:$@\" >> \"@CALL_LOG@\"\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\ncase \"$(step get)\" in\n0) echo '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w9M:p1\",\"agent\":\"kiro\",\"agent_status\":\"unknown\"}}}' ;;\n1) echo '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w9M:p1\",\"agent\":\"kiro\",\"agent_status\":\"working\",\"agent_session\":{\"agent\":\"kiro\",\"value\":\"sess-9\"}}}}' ;;\n*) echo '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w9M:p1\",\"agent\":\"kiro\",\"agent_status\":\"unknown\"}}}' ;;\nesac\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\ncase \"$(step info)\" in\n0) echo '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"/usr/bin/zsh\"]}]}}}' ;;\n1) echo '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"kiro-cli\",\"chat\",\"--resume-id\",\"sess-9\"]}]}}}' ;;\n*) echo '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"/usr/bin/zsh\"]}]}}}' ;;\nesac\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"read\" ]; then\necho '@@AUTORESUME-EXIT:137@@'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"run\" ]; then\necho '{\"id\":\"cli:pane:run\",\"result\":{\"ok\":true}}'\nexit 0\nfi\nexit 1\n";
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
                    "4:pane run w9M:p1 kiro-cli chat -r; printf '@@AUTORESUME-EXIT:%s@@\\n' \"$?\"",
                    "4:pane run w9M:p1 kiro-cli chat --resume-id sess-9; printf '@@AUTORESUME-EXIT:%s@@\\n' \"$?\"",
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
    fn poll_once_clean_exit_stays_dead() {
        // Our wrapped run exited 0: spend everything, no comeback, log
        // once. Poll 1 (no pending marker yet) relaunches wrapped and
        // arms classification; polls 2+ read the clean marker and stop.
        let script = "#!/bin/sh\necho \"$#:$@\" >> \"@CALL_LOG@\"\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w9M:p1\",\"agent\":\"kiro\",\"agent_status\":\"unknown\"}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"/usr/bin/zsh\"]}]}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"read\" ]; then\necho 'prompt @@AUTORESUME-EXIT:0@@ » '\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"run\" ]; then\necho '{\"id\":\"cli:pane:run\",\"result\":{\"ok\":true}}'\nexit 0\nfi\nexit 1\n";
        run_with_fake_herdr(script, &[], |state_dir| {
            crate::state::remember(
                "w9M:p1",
                SessionRef {
                    agent: "kiro".into(),
                    value: "sess-9".into(),
                },
            );
            let config = Config::default();
            let mut st = MonitorState::default();
            assert!(poll_once("w9M:p1", &config, &mut st)); // valued wrapped relaunch
            assert!(st.wrapped_pending);
            st.cooldown_until = None; // simulate expiry
            assert!(poll_once("w9M:p1", &config, &mut st)); // clean: stay dead
            assert!(poll_once("w9M:p1", &config, &mut st)); // clean: stay dead
            let calls = crate::test_support::herdr_calls(state_dir);
            assert_eq!(
                calls.lines().filter(|l| l.contains("pane run")).count(),
                1,
                "clean exit must allow zero comebacks, got: {calls}"
            );
            assert!(
                !crate::state::load_registry().contains_key("w9M:p1"),
                "clean exit spends the entry"
            );
            let log = std::fs::read_to_string(state_dir.join("log.txt")).unwrap_or_default();
            assert_eq!(
                log.lines().filter(|l| l.contains("clean exit")).count(),
                1,
                "clean exit must log once, got: {log}"
            );
        });
    }

    #[test]
    fn poll_once_read_failure_skips_relaunch() {
        // Unreadable scrollback cannot prove a clean exit, but relaunching
        // blind would resurrect intentional exits: skip fail-safe toward
        // user peace (a transient blip self-heals next poll).
        let script = "#!/bin/sh\necho \"$#:$@\" >> \"@CALL_LOG@\"\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w9M:p1\",\"agent\":\"kiro\",\"agent_status\":\"unknown\"}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"/usr/bin/zsh\"]}]}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"read\" ]; then\nexit 1\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"run\" ]; then\necho '{\"id\":\"cli:pane:run\",\"result\":{\"ok\":true}}'\nexit 0\nfi\nexit 1\n";
        run_with_fake_herdr(script, &[], |state_dir| {
            crate::state::remember(
                "w9M:p1",
                SessionRef {
                    agent: "kiro".into(),
                    value: "sess-9".into(),
                },
            );
            let config = Config::default();
            let mut st = MonitorState::default();
            assert!(poll_once("w9M:p1", &config, &mut st)); // valued relaunch
            st.cooldown_until = None; // simulate expiry
            assert!(poll_once("w9M:p1", &config, &mut st)); // read fails: skip
            assert!(poll_once("w9M:p1", &config, &mut st)); // read fails: skip
            let calls = crate::test_support::herdr_calls(state_dir);
            assert_eq!(
                calls.lines().filter(|l| l.contains("pane run")).count(),
                1,
                "unreadable scrollback must not relaunch, got: {calls}"
            );
            let log = std::fs::read_to_string(state_dir.join("log.txt")).unwrap_or_default();
            assert_eq!(
                log.lines().filter(|l| l.contains("pane read failed")).count(),
                1,
                "read failure must log once, got: {log}"
            );
        });
    }

    #[test]
    fn poll_once_nonposix_stays_unwrapped() {
        // fish has no `$?`-suffix syntax: legacy one-shot unwrapped
        // delivery, and scrollback is never read for classification.
        let script = "#!/bin/sh\necho \"$#:$@\" >> \"@CALL_LOG@\"\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w1:p1\",\"agent\":\"claude\",\"agent_status\":\"unknown\"}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"/usr/bin/fish\"]}]}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"run\" ]; then\necho '{\"id\":\"cli:pane:run\",\"result\":{\"ok\":true}}'\nexit 0\nfi\nexit 1\n";
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
            let calls = crate::test_support::herdr_calls(state_dir);
            assert_eq!(
                calls.lines().collect::<Vec<_>>(),
                vec![
                    "3:pane get w1:p1",
                    "4:pane process-info --pane w1:p1",
                    "4:pane run w1:p1 claude --resume abc-123",
                ],
                "unwrapped single-value delivery, no read, got: {calls}"
            );
        });
    }

    #[test]
    fn foreground_looks_live_rejects_shells_and_garbage() {
        assert!(foreground_looks_live(&[vec!["opencode".into()]]));
        assert!(foreground_looks_live(&[vec![
            "kiro-cli".into(),
            "chat".into(),
            "--resume-id".into(),
            "s".into()
        ]]));
        assert!(!foreground_looks_live(&[]));
        assert!(!foreground_looks_live(&[vec!["/usr/bin/zsh".into()]]));
        assert!(!foreground_looks_live(&[vec!["/bin/sh".into(), "job.sh".into()]]));
        assert!(!foreground_looks_live(&[vec!["-fish".into()]]));
        assert!(!foreground_looks_live(&[vec!["<unparseable>".into()]]));
        assert!(!foreground_looks_live(&[Vec::<String>::new()]));
        assert!(!foreground_looks_live(&[
            vec!["opencode".into()],
            vec!["/usr/bin/zsh".into()],
        ]));
    }

    #[test]
    fn poll_once_continue_sent_three_polls_after_relaunch() {
        // Opencode's template carries no `{message}`, so the configured
        // message goes two-step: relaunch, then submit once the TUI had
        // a few polls to boot (live foreground required).
        let script = "#!/bin/sh\necho \"$#:$@\" >> \"@CALL_LOG@\"\nD=\"$(dirname \"@CALL_LOG@\")\"\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"wAA:p1\",\"agent\":\"opencode\",\"agent_status\":\"unknown\"}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\nif [ -f \"$D/booted\" ]; then\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"opencode\"]}]}}}'\nelse\ntouch \"$D/booted\"\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"/usr/bin/zsh\"]}]}}}'\nfi\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"run\" ]; then\necho '{\"id\":\"cli:pane:run\",\"result\":{\"ok\":true}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"send-text\" ]; then\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"send-keys\" ]; then\nexit 0\nfi\nexit 1\n";
        run_with_fake_herdr(script, &[], |state_dir| {
            crate::state::remember(
                "wAA:p1",
                SessionRef {
                    agent: "opencode".into(),
                    value: "ses-x".into(),
                },
            );
            let config = Config {
                resume_message: Some("continue".into()),
                ..Config::default()
            };
            let mut st = MonitorState::default();
            assert!(poll_once("wAA:p1", &config, &mut st)); // relaunch
            assert!(poll_once("wAA:p1", &config, &mut st)); // booting
            assert!(poll_once("wAA:p1", &config, &mut st)); // booting
            assert!(poll_once("wAA:p1", &config, &mut st)); // submit here
            assert!(poll_once("wAA:p1", &config, &mut st)); // nothing more
            let calls = crate::test_support::herdr_calls(state_dir);
            assert_eq!(
                calls.lines().filter(|l| l.contains("pane send-text")).count(),
                1,
                "continue submitted exactly once, got: {calls}"
            );
            assert_eq!(
                calls.lines().filter(|l| l.contains("pane send-keys")).count(),
                1,
                "Enter submitted exactly once, got: {calls}"
            );
            assert!(
                calls.contains("pane send-text wAA:p1 continue"),
                "message text delivered verbatim, got: {calls}"
            );
            let log = std::fs::read_to_string(state_dir.join("log.txt")).unwrap_or_default();
            assert!(log.contains("submitted continue to opencode"), "log names submit: {log}");
        });
    }

    #[test]
    fn poll_once_continue_skipped_when_idle() {
        // The agent died (or never booted) before the countdown lapsed:
        // chat text must never land in a shell prompt — wait silently.
        let script = "#!/bin/sh\necho \"$#:$@\" >> \"@CALL_LOG@\"\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"wAA:p1\",\"agent\":\"opencode\",\"agent_status\":\"unknown\"}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"/usr/bin/zsh\"]}]}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"run\" ]; then\necho '{\"id\":\"cli:pane:run\",\"result\":{\"ok\":true}}'\nexit 0\nfi\nexit 1\n";
        run_with_fake_herdr(script, &[], |state_dir| {
            crate::state::remember(
                "wAA:p1",
                SessionRef {
                    agent: "opencode".into(),
                    value: "ses-x".into(),
                },
            );
            let config = Config {
                resume_message: Some("continue".into()),
                ..Config::default()
            };
            let mut st = MonitorState::default();
            st.cooldown_until = None;
            assert!(poll_once("wAA:p1", &config, &mut st)); // relaunch
            st.cooldown_until = None;
            for _ in 0..4 {
                assert!(poll_once("wAA:p1", &config, &mut st)); // idle: wait
            }
            let calls = crate::test_support::herdr_calls(state_dir);
            assert!(
                !calls.contains("send-text"),
                "idle shell must never receive chat text, got: {calls}"
            );
            assert!(
                st.pending_continue.is_none(),
                "dead-end poll disarms the pending message"
            );
        });
    }

    #[test]
    fn poll_once_kiro_message_inline_no_twostep() {
        // Kiro's template carries `{message}`: delivered inline with the
        // relaunch, so no post-boot submit may follow.
        let script = "#!/bin/sh\necho \"$#:$@\" >> \"@CALL_LOG@\"\nD=\"$(dirname \"@CALL_LOG@\")\"\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w9M:p1\",\"agent\":\"kiro\",\"agent_status\":\"unknown\"}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\nif [ -f \"$D/booted\" ]; then\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"kiro-cli\",\"chat\",\"--resume-id\",\"sess-9\"]}]}}}'\nelse\ntouch \"$D/booted\"\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"/usr/bin/zsh\"]}]}}}'\nfi\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"run\" ]; then\necho '{\"id\":\"cli:pane:run\",\"result\":{\"ok\":true}}'\nexit 0\nfi\nexit 1\n";
        run_with_fake_herdr(script, &[], |state_dir| {
            crate::state::remember(
                "w9M:p1",
                SessionRef {
                    agent: "kiro".into(),
                    value: "sess-9".into(),
                },
            );
            let config = Config {
                resume_message: Some("continue".into()),
                ..Config::default()
            };
            let mut st = MonitorState::default();
            assert!(poll_once("w9M:p1", &config, &mut st)); // relaunch
            for _ in 0..3 {
                assert!(poll_once("w9M:p1", &config, &mut st)); // live
            }
            let calls = crate::test_support::herdr_calls(state_dir);
            assert!(
                calls.contains("sess-9 continue; printf"),
                "message inline in relaunch, got: {calls}"
            );
            assert!(
                !calls.contains("send-text"),
                "inline delivery must not two-step, got: {calls}"
            );
        });
    }

    #[test]
    fn poll_once_continue_never_into_shell_or_garbage() {
        // A script-running shell, an unparseable foreground, and
        // interactive non-agents (vim/less/tmux) are not the relaunched
        // agent: the pending message must wait, not send. Poll 1
        // relaunches from an idle shell (arming the pending message);
        // later polls switch to the hostile foreground.
        for fg in [
            r#"[{"argv":["/bin/sh","job.sh"]}]"#,
            r#"[{"foo":1}]"#,
            r#"[{"argv":["vim","notes.md"]}]"#,
            r#"[{"argv":["less"]}]"#,
            r#"[{"argv":["tmux","attach"]}]"#,
        ] {
            let script = format!("#!/bin/sh\necho \"$#:$@\" >> \"@CALL_LOG@\"\nD=\"$(dirname \"@CALL_LOG@\")\"\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\necho '{{\"id\":\"cli:pane:get\",\"result\":{{\"pane\":{{\"pane_id\":\"wAA:p1\",\"agent\":\"opencode\",\"agent_status\":\"unknown\"}}}}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\nif [ -f \"$D/booted\" ]; then\necho '{{\"id\":\"cli:pane:process-info\",\"result\":{{\"process_info\":{{\"foreground_processes\":{fg}}}}}}}'\nelse\ntouch \"$D/booted\"\necho '{{\"id\":\"cli:pane:process-info\",\"result\":{{\"process_info\":{{\"foreground_processes\":[{{\"argv\":[\"/usr/bin/zsh\"]}}]}}}}}}'\nfi\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"run\" ]; then\necho '{{\"id\":\"cli:pane:run\",\"result\":{{\"ok\":true}}}}'\nexit 0\nfi\nexit 1\n");
            run_with_fake_herdr(&script, &[], |state_dir| {
                crate::state::remember(
                    "wAA:p1",
                    SessionRef {
                        agent: "opencode".into(),
                        value: "ses-x".into(),
                    },
                );
                let config = Config {
                    resume_message: Some("continue".into()),
                    ..Config::default()
                };
                let mut st = MonitorState::default();
                assert!(poll_once("wAA:p1", &config, &mut st)); // relaunch
                for _ in 0..4 {
                    assert!(poll_once("wAA:p1", &config, &mut st)); // hostile fg
                }
                let calls = crate::test_support::herdr_calls(state_dir);
                assert!(
                    !calls.contains("send-text"),
                    "shell/garbage foreground must never receive chat text, fg={fg}, got: {calls}"
                );
            });
        }
    }

    #[test]
    fn poll_once_continue_dropped_on_agent_switch() {
        // Poll 1 relaunches opencode (pending armed); poll 2 finds claude
        // alive instead (user started something else) → pending dropped,
        // never delivered to the wrong agent.
        let script = "#!/bin/sh\necho \"$#:$@\" >> \"@CALL_LOG@\"\nD=\"$(dirname \"@CALL_LOG@\")\"\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\nif [ -f \"$D/live\" ]; then\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"wAA:p1\",\"agent\":\"claude\",\"agent_status\":\"working\",\"agent_session\":{\"agent\":\"claude\",\"value\":\"sess-c\"}}}}'\nelse\ntouch \"$D/live\"\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"wAA:p1\",\"agent\":\"opencode\",\"agent_status\":\"unknown\"}}}'\nfi\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\nif [ -f \"$D/booted\" ]; then\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"claude\"]}]}}}'\nelse\ntouch \"$D/booted\"\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"/usr/bin/zsh\"]}]}}}'\nfi\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"run\" ]; then\necho '{\"id\":\"cli:pane:run\",\"result\":{\"ok\":true}}'\nexit 0\nfi\nexit 1\n";
        run_with_fake_herdr(script, &[], |state_dir| {
            crate::state::remember(
                "wAA:p1",
                SessionRef {
                    agent: "opencode".into(),
                    value: "ses-x".into(),
                },
            );
            let config = Config {
                resume_message: Some("continue".into()),
                ..Config::default()
            };
            let mut st = MonitorState::default();
            assert!(poll_once("wAA:p1", &config, &mut st)); // relaunch opencode
            assert!(st.pending_continue.is_some());
            assert!(poll_once("wAA:p1", &config, &mut st)); // claude alive
            assert!(
                st.pending_continue.is_none(),
                "agent switch must drop the pending message"
            );
            for _ in 0..3 {
                assert!(poll_once("wAA:p1", &config, &mut st));
            }
            let calls = crate::test_support::herdr_calls(state_dir);
            assert!(
                !calls.contains("send-text"),
                "dropped message must never send, got: {calls}"
            );
        });
    }

    #[test]
    fn poll_once_clean_exit_disarms_pending_continue() {
        // Custom agent whose templates carry no `{message}`: poll 1
        // relaunches wrapped and arms the two-step pending message; the
        // wrapped run then exits cleanly, which must disarm it (a future
        // manual agent must not receive stale text).
        let mut commands = HashMap::new();
        commands.insert("x".into(), "x --resume {value}".into());
        commands.insert("x-fallback".into(), "x -r".into());
        let config = Config {
            commands,
            resume_message: Some("continue".into()),
            ..Config::default()
        };
        let script = "#!/bin/sh\necho \"$#:$@\" >> \"@CALL_LOG@\"\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w1:p1\",\"agent\":\"x\",\"agent_status\":\"unknown\"}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"/usr/bin/zsh\"]}]}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"read\" ]; then\necho 'prompt @@AUTORESUME-EXIT:0@@ » '\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"run\" ]; then\necho '{\"id\":\"cli:pane:run\",\"result\":{\"ok\":true}}'\nexit 0\nfi\nexit 1\n";
        run_with_fake_herdr(script, &[], |state_dir| {
            crate::state::remember(
                "w1:p1",
                SessionRef {
                    agent: "x".into(),
                    value: "sess-x".into(),
                },
            );
            let mut st = MonitorState::default();
            assert!(poll_once("w1:p1", &config, &mut st)); // valued relaunch
            assert!(st.pending_continue.is_some(), "relaunch arms pending");
            st.cooldown_until = None; // simulate expiry
            assert!(poll_once("w1:p1", &config, &mut st)); // clean: disarm
            assert!(
                st.pending_continue.is_none(),
                "clean exit must disarm the pending message"
            );
            let calls = crate::test_support::herdr_calls(state_dir);
            assert!(
                !calls.contains("send-text"),
                "disarmed message must never send, got: {calls}"
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
                    "4:pane run w1:p1 claude --resume abc-123; printf '@@AUTORESUME-EXIT:%s@@\\n' \"$?\"",
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
