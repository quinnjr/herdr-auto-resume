use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::herdr::{self, Pane, SessionRef};
use crate::state::ShellRecord;

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

/// Why a dead pane's agent is gone, judged by the identity of the shell
/// it ran under. An idle-shell foreground looks the same after Ctrl-C
/// and after a session-host crash; the shell does not: the user's exit
/// leaves the agent's own shell in charge, a host crash/restart
/// replaces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleCause {
    /// The shell that hosted the live agent is gone: resume.
    HostRestart,
    /// The agent exited inside its still-running shell, or did so
    /// earlier and was retired: stay dead.
    UserExit,
    /// Never seen running under a known shell, or the current shell is
    /// unidentifiable: no proof of a crash, stay dead (fail closed).
    Unknown,
}

pub fn classify_idle(record: Option<&ShellRecord>, current: Option<&str>) -> IdleCause {
    let Some(current) = current else {
        return IdleCause::Unknown;
    };
    match record {
        None => IdleCause::Unknown,
        Some(r) if !r.live || r.shell == current => IdleCause::UserExit,
        Some(_) => IdleCause::HostRestart,
    }
}

/// The pane's shell pid from a `process-info` result (live herdr 0.8+
/// shape: top-level `shell_pid`).
fn shell_pid(info: &serde_json::Value) -> Option<u32> {
    info.get("shell_pid")
        .and_then(serde_json::Value::as_u64)
        .and_then(|p| u32::try_from(p).ok())
}

/// A reuse-proof identity for a shell process. Linux: pid + boot id +
/// start time, so neither a recycled pid nor the same pid after a
/// reboot reads as the old shell. Elsewhere (no `/proc`): the bare pid.
/// None when `/proc` exists but cannot be read — a transient read
/// failure must not look like a replaced shell.
fn shell_identity(pid: u32) -> Option<String> {
    if !std::path::Path::new("/proc/self/stat").exists() {
        return Some(pid.to_string());
    }
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesised comm start at field 3 (`state`);
    // `starttime` is field 22.
    let start = stat.rsplit_once(')')?.1.split_whitespace().nth(19)?;
    Some(format!("{pid}@{}:{start}", boot.trim()))
}

/// True when some foreground process is `program`: its `name`, or any
/// argv token's basename with the extension dropped (node-launched
/// CLIs such as `.../codex.js`). Live claude entries carry `name` only.
fn foreground_runs(info: &serde_json::Value, program: &str) -> bool {
    let Some(procs) = info
        .get("foreground_processes")
        .and_then(serde_json::Value::as_array)
    else {
        return false;
    };
    procs.iter().any(|p| {
        p.get("name").and_then(serde_json::Value::as_str) == Some(program)
            || p.get("argv")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|argv| {
                    argv.iter().filter_map(serde_json::Value::as_str).any(|tok| {
                        let base = tok.rsplit('/').next().unwrap_or(tok);
                        base.split('.').next() == Some(program)
                    })
                })
    })
}

/// The program an agent runs as: its resume template's first token
/// (basename), else the agent name itself.
fn agent_program(agent: &str, commands: &HashMap<String, String>) -> String {
    commands
        .get(agent)
        .and_then(|t| crate::resume::split_argv(t).into_iter().next())
        .map(|tok| tok.rsplit('/').next().unwrap_or(&tok).to_string())
        .unwrap_or_else(|| agent.to_string())
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

/// Pid of the foreground Claude process (`name`, or argv[0] basename),
/// as herdr's `process-info` reports it.
fn claude_pid(info: &serde_json::Value) -> Option<u32> {
    info.get("foreground_processes")?
        .as_array()?
        .iter()
        .find(|p| {
            p.get("name").and_then(serde_json::Value::as_str) == Some("claude")
                || p.get("argv")
                    .and_then(|a| a.get(0))
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|a0| a0.rsplit('/').next() == Some("claude"))
        })?
        .get("pid")?
        .as_u64()
        .and_then(|p| u32::try_from(p).ok())
}

/// claude-profile capture (thin I/O wrapper around
/// [`crate::claude_profile`]): while Claude runs under a managed
/// profile, remember that profile, and — since herdr only sees the
/// default profile's sessions — its session id when herdr reports none.
/// A config dir that is not a managed profile is left alone: a resume
/// could not reach its sessions.
fn maybe_capture_claude(pane: &Pane, info: &serde_json::Value) {
    let Some(data) = crate::claude_profile::data_dir() else {
        return;
    };
    let Some(pid) = claude_pid(info) else {
        return;
    };
    let Some(config) = crate::claude_profile::config_dir_of(pid) else {
        return;
    };
    let Some(profile) = crate::claude_profile::profile_of_config_dir(&data, &config) else {
        return;
    };
    crate::state::record_profile(&pane.pane_id, &profile);
    if pane.agent_session.is_none() {
        if let Some(id) = crate::claude_profile::session_of(&config, pid) {
            crate::state::remember(
                &pane.pane_id,
                SessionRef { agent: "claude".into(), value: id },
            );
        }
    }
}

/// `claude --profile <name> ...` when the session ran under a profile
/// other than the default (the `claude` wrapper's own flag); otherwise
/// the argv unchanged, so default-profile resumes stay plain.
fn with_claude_profile(
    agent: &str,
    mut argv: Vec<String>,
    profile: Option<&str>,
    default: Option<&str>,
) -> Vec<String> {
    if let Some(p) = profile {
        if agent == "claude" && Some(p) != default && !argv.is_empty() {
            argv.splice(1..1, ["--profile".to_string(), p.to_string()]);
        }
    }
    argv
}

/// The profile to resume a pane's Claude under: the one it was seen
/// running under, else the nearest `.claude-profile` from the pane cwd;
/// with the default, for [`with_claude_profile`]. Nothing without
/// claude-profile installed.
fn resume_profile(pane: &Pane) -> (Option<String>, Option<String>) {
    let Some(data) = crate::claude_profile::data_dir() else {
        return (None, None);
    };
    let profile = crate::state::profile_for(&pane.pane_id).or_else(|| {
        pane.cwd
            .as_deref()
            .filter(|c| !c.is_empty())
            .and_then(|c| crate::claude_profile::dotfile_profile(&data, std::path::Path::new(c)))
    });
    (profile, crate::claude_profile::default_profile(&data))
}

/// Per-monitor mutable poll state: the post-relaunch cooldown, the
/// last-logged `NoResume` signature (repeat identical no-resume polls
/// stay quiet), and the warn-once marker for repetitive failure logs.
/// Crash-vs-exit evidence is durable (`shells.json`), not in here, so a
/// respawned monitor judges the pane the same way.
#[derive(Debug, Default)]
struct MonitorState {
    cooldown_until: Option<Instant>,
    last_noresume: Option<(Option<String>, bool)>,
    last_warn: Option<WarnKind>,
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
    UserExit,
    NoShellEvidence,
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
    let info = herdr::process_info(pane_id);
    let proc_argv = match &info {
        Some(v) => {
            let argv = foreground_argv(v);
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
    if let Some(info) = &info {
        maybe_capture_claude(&pane, info);
    }
    let reg = crate::state::load_registry();
    let shell = info.as_ref().and_then(shell_pid).and_then(shell_identity);
    // Liveness evidence for `classify_idle`: the pane's agent is running
    // under this shell right now. Positive program match only — a stale
    // agent label over vim/less must not re-arm a retired pane.
    if let (Some(info), Some(shell)) = (&info, &shell) {
        let agent = pane
            .agent
            .clone()
            .or_else(|| reg.get(pane_id).map(|s| s.agent.clone()));
        if let Some(agent) = agent {
            if foreground_runs(info, &agent_program(&agent, &config.commands)) {
                crate::state::record_shell(
                    pane_id,
                    ShellRecord { shell: shell.clone(), live: true },
                );
            }
        }
    }
    // Skips the countdown tick below when this poll armed it, so the
    // message goes out three full polls after the relaunch, not two.
    let mut skip_tick = false;
    match decide_poll(&pane, &proc_argv, &reg, config, &st.cooldown_until) {
        PollAction::Refresh(sess) => {
            // A live agent re-arms the registry (valued path for the
            // next crash) and the warn state below.
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
            // Crash vs. user exit: only a session-host crash/restart
            // (the agent's shell was replaced) resumes. Ctrl-C, `/exit`
            // or any death inside the still-running shell stays dead,
            // and so does a pane we never saw the agent run in.
            let record = crate::state::shell_record(pane_id);
            match classify_idle(record.as_ref(), shell.as_deref()) {
                IdleCause::HostRestart => {}
                cause => {
                    // No relaunch is forthcoming: a pending continue
                    // belongs to a gone agent, so disarm (a future manual
                    // agent must not receive stale text).
                    st.pending_continue = None;
                    st.continue_in = 0;
                    if cause == IdleCause::UserExit {
                        // Retire the record so a later host restart does
                        // not resurrect what the user quit.
                        if let Some(r) = record.filter(|r| r.live) {
                            crate::state::record_shell(
                                pane_id,
                                ShellRecord { live: false, ..r },
                            );
                        }
                        st.warn_once(
                            WarnKind::UserExit,
                            format!(
                                "monitor {pane_id}: '{agent}' exited inside its running shell (user exit); staying dead"
                            ),
                        );
                    } else {
                        st.warn_once(
                            WarnKind::NoShellEvidence,
                            format!(
                                "monitor {pane_id}: no evidence '{agent}' was lost to a host restart (never seen running, or shell unidentifiable); staying dead"
                            ),
                        );
                    }
                    return true;
                }
            }
            let valued = crate::resume::resolve_session_with_commands(
                &pane,
                reg.get(&pane.pane_id),
                &proc_argv,
                &config.commands,
            )
            .is_some_and(|s| !s.value.is_empty());
            let (profile, default) = if agent == "claude" {
                resume_profile(&pane)
            } else {
                (None, None)
            };
            let argv = with_claude_profile(&agent, argv, profile.as_deref(), default.as_deref());
            let args: Vec<&str> = argv.iter().map(String::as_str).collect();
            if herdr::pane_run(pane_id, &args) {
                st.cooldown_until =
                    Some(Instant::now() + Duration::from_secs(config.cooldown_seconds));
                // One-shot resurrection: the relaunch spends the saved
                // session (failed delivery keeps it and retries).
                crate::state::forget(pane_id);
                // The resumed agent now lives in this shell: its next
                // death here is a user exit, not another crash. (Only a
                // further host restart earns another comeback.)
                if let Some(shell) = &shell {
                    crate::state::record_shell(
                        pane_id,
                        ShellRecord { shell: shell.clone(), live: true },
                    );
                }
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
        // Nothing also covers cooling-down idle panes.
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

    fn rec(shell: &str, live: bool) -> ShellRecord {
        ShellRecord { shell: shell.into(), live }
    }

    #[test]
    fn classify_idle_replaced_shell_is_host_restart() {
        assert_eq!(
            classify_idle(Some(&rec("old", true)), Some("new")),
            IdleCause::HostRestart
        );
    }

    #[test]
    fn classify_idle_same_shell_is_user_exit() {
        // Ctrl-C / `/exit`: the agent's own shell is still the pane's.
        assert_eq!(
            classify_idle(Some(&rec("s1", true)), Some("s1")),
            IdleCause::UserExit
        );
    }

    #[test]
    fn classify_idle_retired_record_stays_user_exit_across_restarts() {
        // The user quit; a later host restart must not resurrect it.
        assert_eq!(
            classify_idle(Some(&rec("s1", false)), Some("s2")),
            IdleCause::UserExit
        );
    }

    #[test]
    fn classify_idle_without_evidence_is_unknown() {
        assert_eq!(classify_idle(None, Some("s1")), IdleCause::Unknown);
        assert_eq!(classify_idle(Some(&rec("s1", true)), None), IdleCause::Unknown);
    }

    #[test]
    fn shell_pid_reads_live_process_info_field() {
        let info = serde_json::json!({"foreground_processes": [], "shell_pid": 2838});
        assert_eq!(shell_pid(&info), Some(2838));
        assert_eq!(shell_pid(&serde_json::json!({})), None);
        assert_eq!(shell_pid(&serde_json::json!({"shell_pid": "x"})), None);
    }

    #[test]
    fn shell_identity_is_stable_and_pid_specific() {
        let own = std::process::id();
        let a = shell_identity(own).expect("own pid readable");
        assert_eq!(shell_identity(own), Some(a.clone()), "stable across reads");
        assert!(a.starts_with(&format!("{own}")));
        let parent = std::os::unix::process::parent_id();
        assert_ne!(shell_identity(parent), Some(a));
    }

    #[test]
    fn foreground_runs_matches_name_or_argv_basename() {
        // Live claude shape (herdr 0.9.5): no argv on the agent entry,
        // only `name`.
        let claude = serde_json::json!({"foreground_processes": [
            {"cwd": "/home/joseph", "name": "claude", "pid": 31533},
            {"argv": ["agentmail", "mcp"], "name": "agentmail", "pid": 31682}
        ]});
        assert!(foreground_runs(&claude, "claude"));
        let node = serde_json::json!({"foreground_processes": [
            {"argv": ["node", "/usr/lib/node_modules/@openai/codex/bin/codex.js"], "name": "node"}
        ]});
        assert!(foreground_runs(&node, "codex"));
        let vim = serde_json::json!({"foreground_processes": [
            {"argv": ["vim", "notes.md"], "name": "vim"}
        ]});
        assert!(!foreground_runs(&vim, "claude"));
        assert!(!foreground_runs(&serde_json::json!({}), "claude"));
    }

    #[test]
    fn claude_pid_finds_live_claude_entry() {
        // Live claude shape (herdr 0.9.5): `name` + `pid`, no argv.
        let info = serde_json::json!({"foreground_processes": [
            {"cwd": "/home/joseph", "name": "claude", "pid": 17572},
            {"argv": ["agentmail", "mcp"], "name": "agentmail", "pid": 17600}
        ]});
        assert_eq!(claude_pid(&info), Some(17572));
        let argv_only = serde_json::json!({"foreground_processes": [
            {"argv": ["/home/joseph/.local/bin/claude", "--resume", "x"], "pid": 9}
        ]});
        assert_eq!(claude_pid(&argv_only), Some(9));
        let other = serde_json::json!({"foreground_processes": [
            {"argv": ["vim"], "name": "vim", "pid": 3}
        ]});
        assert_eq!(claude_pid(&other), None);
    }

    #[test]
    fn with_claude_profile_flags_only_non_default_profiles() {
        let argv = || vec!["claude".to_string(), "--resume".into(), "s1".into()];
        assert_eq!(
            with_claude_profile("claude", argv(), Some("personal"), Some("work")),
            vec!["claude", "--profile", "personal", "--resume", "s1"]
        );
        assert_eq!(
            with_claude_profile("claude", argv(), Some("work"), Some("work")),
            argv(),
            "default profile: plain resume"
        );
        assert_eq!(with_claude_profile("claude", argv(), None, Some("work")), argv());
        assert_eq!(
            with_claude_profile("claude", argv(), Some("personal"), None),
            vec!["claude", "--profile", "personal", "--resume", "s1"],
            "no default set: any profile is explicit"
        );
        let kiro = vec!["kiro-cli".to_string(), "chat".into()];
        assert_eq!(
            with_claude_profile("kiro", kiro.clone(), Some("personal"), Some("work")),
            kiro,
            "only claude takes --profile"
        );
    }

    #[test]
    fn agent_program_comes_from_resume_template() {
        let commands = Config::default().commands;
        assert_eq!(agent_program("kiro", &commands), "kiro-cli");
        assert_eq!(agent_program("claude", &commands), "claude");
        assert_eq!(agent_program("zed", &commands), "zed", "unknown agent: own name");
    }

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

    /// Give a fake's `process-info` replies a real `shell_pid` (our own
    /// pid, so the `/proc` identity read succeeds).
    fn with_shell_pid(script: &str) -> String {
        script.replace(
            "\"process_info\":{",
            &format!("\"process_info\":{{\"shell_pid\":{},", std::process::id()),
        )
    }

    /// The agent was last seen live under a now-gone shell: the current
    /// idle shell reads as a session-host restart, licensing a relaunch.
    fn seed_host_restart(pane_id: &str) {
        crate::state::record_shell(pane_id, rec("prior-host", true));
    }

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
        run_with_fake_herdr(&with_shell_pid(script), &[], |_| {
            crate::state::remember(
                "w1:p1",
                SessionRef {
                    agent: "claude".into(),
                    value: "abc-123".into(),
                },
            );
            seed_host_restart("w1:p1");
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
        run_with_fake_herdr(&with_shell_pid(script), &[], |state_dir| {
            crate::state::remember(
                "w1:p1",
                SessionRef {
                    agent: "claude".into(),
                    value: "abc-123".into(),
                },
            );
            seed_host_restart("w1:p1");
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

    /// Fake herdr whose `pane get` / `process-info` replies are the
    /// current contents of `get.json` / `info.json` beside the calls
    /// log, so a test stages each poll by rewriting them.
    const STAGED_SCRIPT: &str = "#!/bin/sh\necho \"$#:$@\" >> \"@CALL_LOG@\"\nD=\"$(dirname \"@CALL_LOG@\")\"\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\ncat \"$D/get.json\"\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\ncat \"$D/info.json\"\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"run\" ]; then\necho '{\"id\":\"cli:pane:run\",\"result\":{\"ok\":true}}'\nexit 0\nfi\nexit 1\n";

    /// Stage one poll: opencode pane `wAA:p1` whose shell is `shell`
    /// (None = herdr reports no shell pid), foreground `fg` argv.
    fn stage(dir: &std::path::Path, shell: Option<u32>, fg: &[&str]) {
        let get = r#"{"id":"cli:pane:get","result":{"pane":{"pane_id":"wAA:p1","agent":"opencode","agent_status":"working","agent_session":{"agent":"opencode","value":"ses-x"}}}}"#;
        std::fs::write(dir.join("get.json"), get).expect("stage get");
        let mut info = serde_json::json!({
            "foreground_processes": [{"argv": fg, "name": fg[0].rsplit('/').next()}],
        });
        if let Some(pid) = shell {
            info["shell_pid"] = pid.into();
        }
        let reply = serde_json::json!({"id": "cli:pane:process-info",
            "result": {"process_info": info}});
        std::fs::write(dir.join("info.json"), reply.to_string()).expect("stage info");
    }

    fn pane_runs(state_dir: &std::path::Path) -> usize {
        herdr_calls(state_dir).lines().filter(|l| l.contains("pane run")).count()
    }

    #[test]
    fn poll_once_ctrl_c_in_live_shell_stays_dead() {
        // The reported bug: the user Ctrl-Cs the agent, the same shell
        // takes the foreground back — that must never relaunch.
        run_with_fake_herdr(STAGED_SCRIPT, &[], |dir| {
            let shell = std::process::id();
            let config = Config::default();
            let mut st = MonitorState::default();
            stage(dir, Some(shell), &["opencode"]);
            assert!(poll_once("wAA:p1", &config, &mut st)); // live
            stage(dir, Some(shell), &["/usr/bin/zsh"]);
            for _ in 0..3 {
                assert!(poll_once("wAA:p1", &config, &mut st)); // Ctrl-C'd
            }
            assert_eq!(pane_runs(dir), 0, "calls: {}", herdr_calls(dir));
            assert_eq!(crate::state::shell_record("wAA:p1").map(|r| r.live), Some(false));
            let log = std::fs::read_to_string(dir.join("log.txt")).unwrap_or_default();
            assert_eq!(log.lines().filter(|l| l.contains("user exit")).count(), 1, "{log}");
        });
    }

    #[test]
    fn poll_once_host_restart_relaunches_once() {
        // Session host crashed/restarted: the agent was live under one
        // shell, the pane now idles in a different one → resume. A
        // later exit inside the resumed shell is a user exit again.
        run_with_fake_herdr(STAGED_SCRIPT, &[], |dir| {
            let config = Config::default();
            let mut st = MonitorState::default();
            stage(dir, Some(std::os::unix::process::parent_id()), &["opencode"]);
            assert!(poll_once("wAA:p1", &config, &mut st)); // live, old host
            let new_shell = std::process::id();
            stage(dir, Some(new_shell), &["/usr/bin/zsh"]);
            assert!(poll_once("wAA:p1", &config, &mut st)); // restarted
            assert_eq!(pane_runs(dir), 1, "calls: {}", herdr_calls(dir));
            assert!(
                herdr_calls(dir).contains("pane run wAA:p1 opencode --session ses-x"),
                "unwrapped resume, got: {}",
                herdr_calls(dir)
            );
            st.cooldown_until = None;
            assert!(poll_once("wAA:p1", &config, &mut st)); // exited in place
            assert_eq!(pane_runs(dir), 1, "no second comeback");
        });
    }

    #[test]
    fn poll_once_user_exit_survives_later_host_restart() {
        // Quit by the user, then the host restarts: still dead.
        run_with_fake_herdr(STAGED_SCRIPT, &[], |dir| {
            let config = Config::default();
            let mut st = MonitorState::default();
            let old = std::os::unix::process::parent_id();
            stage(dir, Some(old), &["opencode"]);
            assert!(poll_once("wAA:p1", &config, &mut st));
            stage(dir, Some(old), &["/usr/bin/zsh"]);
            assert!(poll_once("wAA:p1", &config, &mut st)); // user exit
            let mut fresh = MonitorState::default(); // monitor respawned
            stage(dir, Some(std::process::id()), &["/usr/bin/zsh"]);
            assert!(poll_once("wAA:p1", &config, &mut fresh));
            assert_eq!(pane_runs(dir), 0, "calls: {}", herdr_calls(dir));
        });
    }

    /// claude-profile install under `xdg`: `work` (default) and
    /// `personal`; returns the data dir.
    fn claude_profiles(xdg: &std::path::Path) -> std::path::PathBuf {
        let data = xdg.join("claude-profiles");
        for p in ["work", "personal"] {
            std::fs::create_dir_all(data.join(p).join("sessions")).expect("profile");
        }
        std::fs::write(data.join(".default"), "work").expect("default");
        data
    }

    /// A stand-in Claude process running under `config` (its environ is
    /// what the monitor reads), with `sessions/<pid>.json` naming `sid`.
    fn fake_claude(config: &std::path::Path, sid: &str) -> std::process::Child {
        let child = std::process::Command::new("sleep")
            .arg("60")
            .env("CLAUDE_CONFIG_DIR", config)
            .spawn()
            .expect("spawn fake claude");
        let pid = child.id();
        std::fs::write(
            config.join(format!("sessions/{pid}.json")),
            format!(r#"{{"pid":{pid},"sessionId":"{sid}"}}"#),
        )
        .expect("session file");
        for _ in 0..200 {
            if crate::claude_profile::config_dir_of(pid).as_deref() == Some(config) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        child
    }

    /// Stage one poll of claude pane `w2A:p1` (cwd `cwd`, no herdr
    /// `agent_session`) whose shell is `shell`, foreground `fg`.
    fn stage_claude(
        dir: &std::path::Path,
        cwd: &std::path::Path,
        shell: u32,
        fg: serde_json::Value,
    ) {
        let get = serde_json::json!({"id": "cli:pane:get", "result": {"pane": {
            "pane_id": "w2A:p1", "agent": "claude", "agent_status": "working",
            "cwd": cwd}}});
        std::fs::write(dir.join("get.json"), get.to_string()).expect("stage get");
        let info = serde_json::json!({"id": "cli:pane:process-info", "result": {
            "process_info": {"shell_pid": shell, "foreground_processes": fg}}});
        std::fs::write(dir.join("info.json"), info.to_string()).expect("stage info");
    }

    fn idle_zsh() -> serde_json::Value {
        serde_json::json!([{"argv": ["/usr/bin/zsh"], "name": "zsh"}])
    }

    /// Live claude (agent session invisible to herdr) under `profile`,
    /// then a host restart; returns the relaunch line typed.
    fn claude_restart_relaunch(profile: &str) -> String {
        let xdg = crate::test_support::unique_temp_dir("xdg");
        let data = claude_profiles(&xdg);
        let xdg_s = xdg.to_string_lossy().into_owned();
        let mut line = String::new();
        run_with_fake_herdr(STAGED_SCRIPT, &[("XDG_DATA_HOME", Some(&xdg_s))], |dir| {
            let mut claude = fake_claude(&data.join(profile), "sess-p-1");
            let config = Config::default();
            let mut st = MonitorState::default();
            stage_claude(
                dir,
                &xdg,
                std::os::unix::process::parent_id(),
                serde_json::json!([{"name": "claude", "pid": claude.id()}]),
            );
            assert!(poll_once("w2A:p1", &config, &mut st)); // live
            claude.kill().ok();
            claude.wait().ok();
            assert_eq!(
                crate::state::load_registry().get("w2A:p1").map(|s| s.value.clone()),
                Some("sess-p-1".to_string()),
                "session captured from {profile}/sessions/<pid>.json"
            );
            assert_eq!(crate::state::profile_for("w2A:p1").as_deref(), Some(profile));
            stage_claude(dir, &xdg, std::process::id(), idle_zsh());
            assert!(poll_once("w2A:p1", &config, &mut st)); // host restarted
            line = herdr_calls(dir)
                .lines()
                .find(|l| l.contains("pane run"))
                .unwrap_or_default()
                .to_string();
        });
        std::fs::remove_dir_all(&xdg).ok();
        line
    }

    #[test]
    fn poll_once_resumes_non_default_claude_profile() {
        assert_eq!(
            claude_restart_relaunch("personal"),
            "4:pane run w2A:p1 claude --profile personal --resume sess-p-1"
        );
    }

    #[test]
    fn poll_once_resumes_default_claude_profile_plainly() {
        assert_eq!(
            claude_restart_relaunch("work"),
            "4:pane run w2A:p1 claude --resume sess-p-1"
        );
    }

    /// Never seen live (no recorded profile): a host restart of a pane
    /// whose cwd holds `.claude-profile` = personal; returns the
    /// relaunch line. `installed` = claude-profile data dir present.
    fn dotfile_restart_relaunch(installed: bool) -> String {
        let xdg = crate::test_support::unique_temp_dir("xdg");
        if installed {
            claude_profiles(&xdg);
        }
        let proj = xdg.join("proj/sub");
        std::fs::create_dir_all(&proj).expect("proj");
        std::fs::write(xdg.join("proj/.claude-profile"), "personal\n").expect("dotfile");
        let xdg_s = xdg.to_string_lossy().into_owned();
        let mut line = String::new();
        run_with_fake_herdr(STAGED_SCRIPT, &[("XDG_DATA_HOME", Some(&xdg_s))], |dir| {
            crate::state::remember(
                "w2A:p1",
                SessionRef { agent: "claude".into(), value: "sess-d-1".into() },
            );
            seed_host_restart("w2A:p1");
            stage_claude(dir, &proj, std::process::id(), idle_zsh());
            assert!(poll_once("w2A:p1", &Config::default(), &mut MonitorState::default()));
            line = herdr_calls(dir)
                .lines()
                .find(|l| l.contains("pane run"))
                .unwrap_or_default()
                .to_string();
        });
        std::fs::remove_dir_all(&xdg).ok();
        line
    }

    #[test]
    fn poll_once_falls_back_to_local_claude_profile_file() {
        assert_eq!(
            dotfile_restart_relaunch(true),
            "4:pane run w2A:p1 claude --profile personal --resume sess-d-1"
        );
    }

    #[test]
    fn poll_once_ignores_profiles_without_claude_profile_installed() {
        assert_eq!(
            dotfile_restart_relaunch(false),
            "4:pane run w2A:p1 claude --resume sess-d-1"
        );
    }

    #[test]
    fn poll_once_without_shell_evidence_stays_dead() {
        // Never seen running (no record), or no shell pid reported:
        // nothing proves a crash, so fail closed.
        run_with_fake_herdr(STAGED_SCRIPT, &[], |dir| {
            let config = Config::default();
            let mut st = MonitorState::default();
            stage(dir, Some(std::process::id()), &["/usr/bin/zsh"]);
            assert!(poll_once("wAA:p1", &config, &mut st)); // no record
            crate::state::record_shell("wAA:p1", rec("prior-host", true));
            stage(dir, None, &["/usr/bin/zsh"]);
            assert!(poll_once("wAA:p1", &config, &mut st)); // no shell pid
            assert_eq!(pane_runs(dir), 0, "calls: {}", herdr_calls(dir));
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
        run_with_fake_herdr(&with_shell_pid(script), &[], |state_dir| {
            crate::state::remember(
                "wAA:p1",
                SessionRef {
                    agent: "opencode".into(),
                    value: "ses-x".into(),
                },
            );
            seed_host_restart("wAA:p1");
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
        run_with_fake_herdr(&with_shell_pid(script), &[], |state_dir| {
            crate::state::remember(
                "wAA:p1",
                SessionRef {
                    agent: "opencode".into(),
                    value: "ses-x".into(),
                },
            );
            seed_host_restart("wAA:p1");
            let config = Config {
                resume_message: Some("continue".into()),
                ..Config::default()
            };
            let mut st = MonitorState::default();
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
        run_with_fake_herdr(&with_shell_pid(script), &[], |state_dir| {
            crate::state::remember(
                "w9M:p1",
                SessionRef {
                    agent: "kiro".into(),
                    value: "sess-9".into(),
                },
            );
            seed_host_restart("w9M:p1");
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
                calls.contains("sess-9 continue"),
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
            run_with_fake_herdr(&with_shell_pid(&script), &[], |state_dir| {
                crate::state::remember(
                    "wAA:p1",
                    SessionRef {
                        agent: "opencode".into(),
                        value: "ses-x".into(),
                    },
                );
                seed_host_restart("wAA:p1");
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
        run_with_fake_herdr(&with_shell_pid(script), &[], |state_dir| {
            crate::state::remember(
                "wAA:p1",
                SessionRef {
                    agent: "opencode".into(),
                    value: "ses-x".into(),
                },
            );
            seed_host_restart("wAA:p1");
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
    fn poll_once_relaunch_sets_cooldown() {
        let script = "#!/bin/sh\necho \"$#:$@\" >> \"@CALL_LOG@\"\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w1:p1\",\"agent_status\":\"unknown\"}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"/usr/bin/zsh\"]}]}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"run\" ]; then\necho '{\"id\":\"cli:pane:run\",\"result\":{\"ok\":true}}'\nexit 0\nfi\nexit 1\n";
        run_with_fake_herdr(&with_shell_pid(script), &[], |state_dir| {
            crate::state::remember(
                "w1:p1",
                SessionRef {
                    agent: "claude".into(),
                    value: "abc-123".into(),
                },
            );
            seed_host_restart("w1:p1");
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
