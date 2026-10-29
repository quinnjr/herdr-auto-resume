use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::herdr::{self, Pane, SessionRef};
use crate::state::{SeenArgv, ShellRecord};

/// Foreground programs that count as an idle shell: a bare shell with no
/// script, `-c` command, or other arguments is a dead agent's pane.
const IDLE_SHELLS: &[&str] = &[
    "zsh", "bash", "sh", "dash", "ksh", "fish", "nu", "pwsh", "ash",
];

/// True when `argv` is a bare idle shell: exactly one element naming a
/// known shell (path prefix and login-shell `-` prefix stripped).
fn is_bare_idle_shell(argv: &[String]) -> bool {
    argv.len() == 1 && is_shell_name(&argv[0])
}

/// True when `prog` names a known shell (path prefix and login-shell
/// `-` prefix stripped).
fn is_shell_name(prog: &str) -> bool {
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
fn should_relaunch(entries: &[ProcEntry], cooldown_until: Option<Instant>) -> bool {
    if let Some(until) = cooldown_until {
        if Instant::now() < until {
            return false;
        }
    }
    !entries.is_empty() && entries.iter().all(|e| is_bare_idle_shell(&e.argv))
}

/// One foreground process, normalised from every `process-info` shape
/// herdr reports (see [`foreground_entries`]). The single source of
/// shape handling: the relaunch veto, the program matcher
/// ([`entry_runs`]) and [`claude_pid`] all read these.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcEntry {
    /// herdr's `name` for the process, when reported.
    name: Option<String>,
    /// The process argv: `[name]` for a name-only entry, `[UNPARSEABLE]`
    /// for the fail-closed poison sentinel.
    argv: Vec<String>,
    pid: Option<u32>,
}

impl ProcEntry {
    fn from_argv(argv: Vec<String>) -> Self {
        ProcEntry {
            name: None,
            argv,
            pid: None,
        }
    }
}

/// Normalise a `process-info` result object into foreground entries.
/// Only the live shape is honored (`foreground_processes` lists of
/// `{argv: [...]}` objects, name-only `{name}` objects, or bare argv
/// arrays; else a single top-level `argv`); anything else yields empty
/// (which vetoes relaunch — fail closed; the caller logs the shape for
/// diagnosability).
fn foreground_entries(info: &serde_json::Value) -> Vec<ProcEntry> {
    if let Some(v) = info.get("foreground_processes") {
        let out = entries_from_list(v);
        if !out.is_empty() {
            return out;
        }
    }
    if let Some(argv) = info.get("argv") {
        if let Ok(a) = serde_json::from_value::<Vec<String>>(argv.clone()) {
            if !a.is_empty() {
                return vec![ProcEntry::from_argv(a)];
            }
        }
    }
    vec![]
}

/// Sentinel for a foreground list with zero parseable entries. A bare
/// sentinel never matches `IDLE_SHELLS`, so `should_relaunch` vetoes
/// fail-closed instead of treating silent drops as an idle shell.
const UNPARSEABLE: &str = "<unparseable>";

fn entries_from_list(v: &serde_json::Value) -> Vec<ProcEntry> {
    let arr = match v.as_array() {
        Some(a) => a,
        None => return vec![],
    };
    let mut out = Vec::new();
    let mut dropped_any = false;
    for item in arr {
        let name = item
            .get("name")
            .and_then(serde_json::Value::as_str)
            .filter(|n| !n.is_empty());
        let pid = item
            .get("pid")
            .and_then(serde_json::Value::as_u64)
            .and_then(|p| u32::try_from(p).ok());
        let argv = item.get("argv").filter(|a| !a.is_null());
        if let Some(argv) = argv {
            if let Ok(a) = serde_json::from_value::<Vec<String>>(argv.clone()) {
                if !a.is_empty() {
                    out.push(ProcEntry {
                        name: name.map(str::to_string),
                        argv: a,
                        pid,
                    });
                    continue;
                }
            }
        }
        // Name-only entry (live claude, herdr 0.9.5: `{name, pid, cwd}`
        // with no argv): the process is `[name]` — `claude` stays a live
        // agent. A shell name is NOT taken as `[shell]`: herdr may simply
        // have failed to read the cmdline of `bash long-job.sh`, and a
        // bare-shell reading would type into a live script, so it falls
        // through to the fail-closed sentinel. A present-but-malformed
        // argv is still corruption.
        if argv.is_none_or(|a| a.as_array().is_some_and(Vec::is_empty)) {
            if let Some(name) = name.filter(|n| !is_shell_name(n)) {
                out.push(ProcEntry {
                    name: Some(name.to_string()),
                    argv: vec![name.to_string()],
                    pid,
                });
                continue;
            }
        }
        // Bare argv array entry (`[["opencode", "--session", "s"]]`).
        if let Ok(a) = serde_json::from_value::<Vec<String>>(item.clone()) {
            out.push(ProcEntry::from_argv(a));
        } else {
            // Fail-closed on partial corruption: a dropped entry could be
            // a live agent, so poison the whole parse with the sentinel
            // (never a bare idle shell) instead of treating the survivors
            // as the full foreground.
            dropped_any = true;
        }
    }
    if dropped_any {
        out.push(ProcEntry::from_argv(vec![UNPARSEABLE.to_string()]));
    }
    out
}

/// An entry reads as its argv wherever argv vectors are taken (session
/// flag lookup), so no copy is made.
impl AsRef<[String]> for ProcEntry {
    fn as_ref(&self) -> &[String] {
        &self.argv
    }
}

/// Foreground argv vectors straight from a `process-info` result.
#[cfg(test)]
fn foreground_argv(info: &serde_json::Value) -> Vec<Vec<String>> {
    foreground_entries(info)
        .into_iter()
        .map(|e| e.argv)
        .collect()
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

/// Script interpreters whose entry point is the real program
/// (node-launched CLIs such as `node .../codex.js`).
#[derive(Clone, Copy)]
enum Interpreter {
    Node,
    Bun,
    Deno,
    Python,
}

fn interpreter(base: &str) -> Option<Interpreter> {
    match base {
        "node" => Some(Interpreter::Node),
        "bun" => Some(Interpreter::Bun),
        "deno" => Some(Interpreter::Deno),
        "python" | "python3" => Some(Interpreter::Python),
        _ => base
            .strip_prefix("python3.")
            .filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
            .map(|_| Interpreter::Python),
    }
}

/// A path token's basename with one extension dropped: any extension
/// when `exts` is None (`/usr/lib/.../codex.js` → `codex`), else only
/// one of `exts` (`codex.py` stays `codex.py` for node's list).
fn stem<'a>(tok: &'a str, exts: Option<&[&str]>) -> &'a str {
    let base = tok.rsplit('/').next().unwrap_or(tok);
    match base.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && exts.is_none_or(|e| e.contains(&ext)) => stem,
        _ => base,
    }
}

/// True when the foreground process `entry` IS `program`: its `name`; or
/// its argv[0] basename, exactly or with one extension dropped
/// (`kiro-cli.AppImage`, `codex.sh`); or — only when argv[0] is a script
/// interpreter (node, bun, deno, python, python3, python3.x) — its entry
/// point (`node -r dotenv/config .../codex.js`, `python -m hermes`; see
/// [`interpreted_program_is`]). Other argv tokens never count: `vim
/// opencode.json`, `less claude.log` and `man claude` do not run the
/// agent. An agent started through a package/env launcher (`npx`,
/// `pnpm exec`, `uvx`, `uv run`, `pipx run`, `env FOO=1`, …) is not
/// recognised as running: configure a template that runs the agent's
/// binary directly. Live claude entries carry
/// `name` only. The single program matcher: the continue gate, the kiro
/// pin gate, shell liveness, [`own_flagged`] and [`claude_pid`] all use
/// it.
///
/// Positive match only, by design: herdr's agent label can be stale (it
/// outlives the agent over vim/less, or after a crash), so an
/// unrecognised form reads as "not running" — it costs a missed
/// refresh, never a false liveness signal.
fn entry_runs(entry: &ProcEntry, program: &str) -> bool {
    if entry.name.as_deref() == Some(program) {
        return true;
    }
    let Some(first) = entry.argv.first() else {
        return false;
    };
    let base = first.rsplit('/').next().unwrap_or(first);
    if base == program || stem(base, None) == program {
        return true;
    }
    interpreter(base).is_some_and(|kind| interpreted_program_is(kind, &entry.argv[1..], program))
}

/// True when some foreground entry runs `program` ([`entry_runs`]).
fn entries_run(entries: &[ProcEntry], program: &str) -> bool {
    entries.iter().any(|e| entry_runs(e, program))
}

/// [`entries_run`] straight from a `process-info` result.
#[cfg(test)]
fn foreground_runs(info: &serde_json::Value, program: &str) -> bool {
    entries_run(&foreground_entries(info), program)
}

/// The agent's own flagged process: among the foreground entries that
/// ARE `program` (so an MCP child's `--session X` never counts), those
/// carrying `agent`'s template `flag` ([`crate::resume::template_flag`])
/// with a safe value; the lowest pid wins, an entry without a pid only
/// when none has one, whatever order herdr reports entries in. Its pid
/// and session value. The one selection behind both [`own_session`] and
/// [`own_sighting`], so a live Refresh, the kiro pin and the sighting
/// always name the same process.
fn own_flagged(
    entries: &[ProcEntry],
    program: &str,
    agent: &str,
    flag: &str,
) -> Option<(Option<u32>, SessionRef)> {
    entries
        .iter()
        .filter(|e| entry_runs(e, program))
        .filter_map(|e| {
            crate::resume::session_for_flag(agent, flag, std::slice::from_ref(e))
                .map(|s| (e.pid, s))
        })
        .min_by_key(|(pid, _)| (pid.is_none(), *pid))
}

/// The agent's own process as [`SeenArgv::is_new`] compares it: the
/// [`own_flagged`] entry with its pid (`flagged`, computed once per poll
/// by [`own_session`]), else an own-process entry
/// without a value (a bare `opencode` is still a sighting, so a later
/// flagged process with another pid is new; lowest pid again, an entry
/// without a pid only when none has one). None when no entry runs
/// `program`.
fn own_sighting(
    entries: &[ProcEntry],
    program: &str,
    agent: &str,
    flagged: Option<(Option<u32>, SessionRef)>,
) -> Option<SeenArgv> {
    flagged
        .map(|(pid, s)| SeenArgv {
            agent: s.agent,
            value: Some(s.value),
            pid,
        })
        .or_else(|| {
            entries
                .iter()
                .filter(|e| entry_runs(e, program))
                .map(|e| SeenArgv {
                    agent: agent.to_string(),
                    value: None,
                    pid: e.pid,
                })
                .min_by_key(|s| (s.pid.is_none(), s.pid))
        })
}

/// Node/bun/deno flags whose value is the next token.
const NODE_VALUE_FLAGS: &[&str] = &[
    "-r",
    "--require",
    "--import",
    "--loader",
    "--experimental-loader",
    "-C",
    "--conditions",
    "--env-file",
    "--title",
    "--inspect-port",
];

/// Deno-only flags whose value is the next token (node's `-c` is the
/// standalone `--check`, so these are not in [`NODE_VALUE_FLAGS`]).
const DENO_VALUE_FLAGS: &[&str] = &["--config", "-c", "--import-map"];

/// node/bun flags that run no script.
const NODE_NO_SCRIPT: &[&str] = &["-e", "--eval", "-p", "--print"];

/// Python flags whose value is the next token.
const PYTHON_VALUE_FLAGS: &[&str] = &["-W", "-X", "-Q"];

/// The interpreter-argv parse behind [`entry_runs`]: find the entry
/// point the way the interpreter would and compare only that, so the
/// script's own arguments never count (`node tool.js codex`, `python3 -m
/// pytest -k claude`).
///
/// - node/bun/deno: flags are skipped, [`NODE_VALUE_FLAGS`] (plus
///   [`DENO_VALUE_FLAGS`] for deno) with their separate value
///   (`--require=x` is one token); [`NODE_NO_SCRIPT`] flags run no script; a leading `run` subcommand (deno, bun) is
///   skipped; `--` ends flags. The first remaining token is the script,
///   compared by basename with `.js/.mjs/.cjs/.ts` dropped; an `index`
///   script is named by its directory (`.../opencode/index.ts`).
/// - python: [`PYTHON_VALUE_FLAGS`] take a separate value; `-c` runs no
///   script; `-m MOD` matches when MOD or its first dotted segment is
///   `program`; otherwise the first non-flag token is the script,
///   compared by basename with `.py` dropped. Any other `-` token is a
///   standalone flag.
fn interpreted_program_is(kind: Interpreter, args: &[String], program: &str) -> bool {
    let (value_flags, no_script, exts): (&[&str], &[&str], &[&str]) = match kind {
        Interpreter::Node | Interpreter::Bun | Interpreter::Deno => (
            NODE_VALUE_FLAGS,
            NODE_NO_SCRIPT,
            &["js", "mjs", "cjs", "ts"],
        ),
        Interpreter::Python => (PYTHON_VALUE_FLAGS, &["-c"], &["py"]),
    };
    let extra_value_flags: &[&str] = match kind {
        Interpreter::Deno => DENO_VALUE_FLAGS,
        _ => &[],
    };
    let mut args = args.iter().map(String::as_str);
    if matches!(kind, Interpreter::Deno | Interpreter::Bun) {
        let mut peek = args.clone();
        if peek.next() == Some("run") {
            args = peek;
        }
    }
    let script_is = |tok: &str| match stem(tok, Some(exts)) {
        // A package entry point (`.../opencode/index.ts`) is named by
        // its directory, as module resolution names it.
        "index" if !matches!(kind, Interpreter::Python) => {
            tok.rsplit('/').nth(1).is_some_and(|dir| dir == program)
        }
        s => s == program,
    };
    while let Some(tok) = args.next() {
        if tok == "--" {
            return args.next().is_some_and(script_is);
        }
        if !tok.starts_with('-') || tok == "-" {
            return script_is(tok);
        }
        let flag = tok.split_once('=').map_or(tok, |(f, _)| f);
        if no_script.contains(&flag) {
            return false;
        }
        if matches!(kind, Interpreter::Python) && tok == "-m" {
            return args
                .next()
                .is_some_and(|m| m == program || m.split('.').next() == Some(program));
        }
        if !tok.contains('=') && (value_flags.contains(&tok) || extra_value_flags.contains(&tok)) {
            args.next();
        }
    }
    false
}

/// The program an agent runs as: its usable primary template's program
/// ([`crate::resume::template_program`]), else its usable
/// `<agent>-fallback`'s (a blank primary opts out of the primary only),
/// else the agent name itself.
fn agent_program(agent: &str, commands: &HashMap<String, String>) -> String {
    let program = |key: &str| {
        crate::resume::usable_template(key, commands).and_then(crate::resume::template_program)
    };
    program(agent)
        .or_else(|| program(&format!("{agent}-fallback")))
        .unwrap_or_else(|| agent.to_string())
}

/// The pane agent's own flagged process ([`own_flagged`] of `program`
/// under the agent's [`crate::resume::template_flag`]): its pid and the
/// session value it carries, derived once per poll for the kiro pin and
/// a live Refresh (the value) and the sighting ([`own_sighting`] takes
/// it whole). None without a pane agent.
fn own_session(
    agent: Option<&str>,
    program: Option<&str>,
    entries: &[ProcEntry],
    commands: &HashMap<String, String>,
) -> Option<(Option<u32>, SessionRef)> {
    let agent = agent?;
    let flag = crate::resume::template_flag(agent, commands)?;
    own_flagged(entries, program?, agent, &flag)
}

/// Pure poll decision: refresh the registry while the agent is alive,
/// relaunch when `should_relaunch` fires, otherwise do nothing.
/// `should_relaunch` remains the veto core (live-foreground veto stays
/// fail-closed: empty/unknown/sentinel argv never relaunches).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollAction {
    Refresh(SessionRef),
    /// `cmd.key` names the `commands` entry the argv was built from
    /// (valued primary or `<agent>-fallback`), so the caller never
    /// re-derives the template selection; `cmd.program` is the program
    /// that template runs (gates `--profile` and the two-step continue).
    Relaunch {
        agent: String,
        cmd: crate::resume::ResumeCommand,
    },
    /// Veto passed but no resume argv could be built (computed once in
    /// `decide_poll`): `agent` is the best-known name (None when unknown),
    /// `value_known` tracks whether a session value was known.
    NoResume {
        agent: Option<String>,
        value_known: bool,
    },
    Nothing,
}

/// Decide one poll. `own_value` is the pane agent's own process's
/// session value ([`own_session`] of its [`agent_program`]), computed
/// once per poll by the caller; `argv_new` says the agent's
/// own session flag is new evidence ([`SeenArgv::is_new`],
/// [`crate::resume::resolve_live_session`]).
fn decide_poll(
    pane: &Pane,
    entries: &[ProcEntry],
    own_value: Option<&SessionRef>,
    registry: &HashMap<String, SessionRef>,
    config: &Config,
    cooldown_until: &Option<Instant>,
    argv_new: bool,
) -> PollAction {
    // Dead pane first: an idle-shell foreground means the agent process is
    // gone even when the pane still records an agent (stale crash
    // metadata). Refreshing the registry here would just re-record the
    // stale session and skip the relaunch.
    if should_relaunch(entries, *cooldown_until) {
        return relaunch_for_dead_pane(pane, entries, registry, config);
    }
    if pane.agent.is_some() {
        // Only the agent's own process argv may name a new session: an
        // MCP child's `--session X` is never the agent's.
        if let Some(sess) = crate::resume::resolve_live_session(
            pane,
            registry.get(&pane.pane_id),
            own_value,
            &config.commands,
            argv_new,
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
    entries: &[ProcEntry],
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
        entries,
        &config.commands,
    );
    let value = session
        .as_ref()
        .and_then(|s| Some(s.value.clone()).filter(|v| !v.is_empty()));
    let agent = pane
        .agent
        .clone()
        .or_else(|| session.map(|s| s.agent))
        .unwrap_or_default();
    if agent.is_empty() {
        return PollAction::NoResume {
            agent: None,
            value_known: value.is_some(),
        };
    }
    let Some(cmd) = crate::resume::resume_command(
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
    PollAction::Relaunch { agent, cmd }
}

/// Kiro session pinning (thin I/O wrapper around [`crate::kiro`]).
///
/// Live kiro CLIs run with a bare `--resume` (no id on the cmdline) and
/// herdr often reports no `agent_session`, so the registry would stay
/// empty and a later crash would fall back to `chat -r` — wrong with
/// several same-folder sessions. While the pane is supervised and the
/// registry holds no kiro ref for it (a stale foreign-agent ref is
/// overwritten), pin the freshest `chat -l` id for the pane cwd.
/// The live primary re-saves on every turn, so it outranks stale
/// subagent runs. Best-effort: any failure leaves the registry
/// untouched and the existing decide path applies. `decide_poll` stays
/// pure; with the registry populated its Refresh/Relaunch arms just work.
///
/// Only while kiro positively runs in the foreground (a dead pane with a
/// stale kiro label must not pin whatever ran last in its folder), and a
/// failed lookup (a `chat -l` spawn of up to 10s) — or a found id that
/// could not be persisted — is retried only every [`KIRO_RETRY_POLLS`]
/// polls. A pin lands in `reg` too (this poll's
/// registry snapshot), so the caller need not reload it.
///
/// A live kiro argv that already names the session (`own_value`: the
/// template's own flag, e.g. `--resume-id X`, on kiro's own process —
/// see [`own_session`]) needs no lookup: the Refresh arm remembers X,
/// so no `chat -l` is spawned.
///
/// Acts for any pane agent whose template runs kiro (`program`, its
/// [`agent_program`], is `kiro-cli` or e.g. `kiro-cli.AppImage`), so an
/// aliased key like `kiro-work` is pinned under that key, and only
/// while that agent has a valued primary template (one with `{value}`,
/// [`crate::resume::usable_template`]): a pin could never be used
/// otherwise, so a blank or valueless primary never spawns `chat -l`.
fn maybe_pin_kiro_session(
    pane: &Pane,
    procs: &[ProcEntry],
    program: Option<&str>,
    own_value: Option<&SessionRef>,
    reg: &mut HashMap<String, SessionRef>,
    config: &Config,
    st: &mut MonitorState,
) {
    let Some(agent) = pane.agent.as_deref() else {
        return;
    };
    // A registry ref of this agent is already pinned; a foreign agent's
    // ref is stale (resolve rejects it) and gets overwritten by the pin.
    if !crate::resume::usable_template(agent, &config.commands)
        .is_some_and(|t| t.contains(crate::resume::VALUE_PLACEHOLDER))
        || reg.get(&pane.pane_id).is_some_and(|s| s.agent == agent)
    {
        return;
    }
    let runs_kiro = program.is_some_and(|p| stem(p, None) == "kiro-cli" && entries_run(procs, p));
    if !runs_kiro || own_value.is_some() {
        return;
    }
    let Some(cwd) = pane.cwd.as_deref().filter(|c| !c.is_empty()) else {
        return;
    };
    if st.kiro_retry_in > 0 {
        st.kiro_retry_in -= 1;
        return;
    }
    let Some(id) = crate::kiro::latest_session_for_cwd(cwd) else {
        // The failed poll is poll 0: skip the next KIRO_RETRY_POLLS - 1
        // polls and retry on poll KIRO_RETRY_POLLS.
        st.kiro_retry_in = KIRO_RETRY_POLLS - 1;
        return;
    };
    let sess = SessionRef {
        agent: agent.to_string(),
        value: id,
    };
    if crate::state::remember_with_snapshot(&pane.pane_id, sess.clone(), reg) {
        reg.insert(pane.pane_id.clone(), sess);
    } else {
        // Found but not persisted: the next poll would look it up
        // again, so back off exactly as for a failed lookup.
        st.kiro_retry_in = KIRO_RETRY_POLLS - 1;
    }
}

/// A failed kiro session lookup is retried every this many polls.
const KIRO_RETRY_POLLS: u8 = 6;

/// Pid of the foreground Claude process: the lowest pid among the
/// entries that run `program` (Claude's [`agent_program`], per
/// [`entry_runs`]) and report one, whatever order herdr reports them in
/// (as [`own_flagged`] picks).
fn claude_pid(procs: &[ProcEntry], program: &str) -> Option<u32> {
    procs
        .iter()
        .filter(|e| entry_runs(e, program))
        .filter_map(|e| e.pid)
        .min()
}

/// claude-profile capture (thin I/O wrapper around
/// [`crate::claude_profile`]): while Claude runs under a managed
/// profile, remember that profile, and — since herdr only sees the
/// default profile's sessions — its session id when herdr reports none.
/// A config dir that is not a managed profile is left alone: a resume
/// could not reach its sessions.
///
/// Only when the pane's agent (`agent`: herdr's label, else the
/// registry's) runs claude — its `program` ([`agent_program`]) is
/// `claude`, so an aliased key like `claude-work` with a `claude
/// --resume {value}` template counts: a `claude -p` tool subprocess
/// inside an opencode or codex pane must never hijack that pane's
/// session. The capture is recorded under `agent`.
///
/// Returns the captured session: it is the running process's own, so
/// the caller treats it as the live `agent_session` (outranking a stale
/// registry value and a stale `--resume OLD` in argv). A remembered
/// capture also lands in `reg`.
fn maybe_capture_claude(
    pane: &Pane,
    agent: Option<&str>,
    program: Option<&str>,
    procs: &[ProcEntry],
    reg: &mut HashMap<String, SessionRef>,
) -> Option<SessionRef> {
    let agent = agent?;
    let program = program.filter(|p| *p == "claude")?;
    let data = crate::claude_profile::data_dir()?;
    let pid = claude_pid(procs, program)?;
    let config = crate::claude_profile::config_dir_of(pid)?;
    let profile = crate::claude_profile::profile_of_config_dir(&data, &config)?;
    crate::state::record_profile(&pane.pane_id, &profile);
    if pane.agent_session.is_some() {
        return None;
    }
    let id = crate::claude_profile::session_of(&config, pid)?;
    let sess = SessionRef {
        agent: agent.to_string(),
        value: id,
    };
    if crate::state::remember_with_snapshot(&pane.pane_id, sess.clone(), reg) {
        reg.insert(pane.pane_id.clone(), sess.clone());
    }
    Some(sess)
}

/// Whether a relaunch resumes under a claude-profile: the `claude`
/// agent, or any agent whose template `program` (the
/// [`PollAction::Relaunch`] one, argv[0]'s basename) is `claude` (an
/// aliased key like `claude-work`).
fn claude_profile_applies(agent: &str, program: &str) -> bool {
    agent == "claude" || program == "claude"
}

/// `claude --profile <name> ...` when the session ran under a profile
/// other than the default (the `claude` wrapper's own flag, inserted
/// right after argv[0]); otherwise the argv unchanged, so
/// default-profile resumes stay plain. `profile` is passed only when
/// [`claude_profile_applies`]; only a template whose `program` is
/// `claude` takes the flag: a launcher prefix (`env FOO=1 claude`, `env
/// -C /home/me/claude claude`) is not parsed, consistent with launcher
/// templates being unsupported. The flag is `false` when a needed
/// profile could not be applied: the argv is returned without it and
/// the caller logs a warning.
fn with_claude_profile(
    program: &str,
    mut argv: Vec<String>,
    profile: Option<&str>,
    default: Option<&str>,
) -> (Vec<String>, bool) {
    let Some(p) = profile.filter(|p| Some(*p) != default) else {
        return (argv, true);
    };
    if program != "claude" {
        return (argv, false);
    }
    argv.splice(1..1, ["--profile".to_string(), p.to_string()]);
    (argv, true)
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
/// stay quiet), the warn-once marker for repetitive failure logs, and the
/// kiro lookup backoff. Crash-vs-exit evidence is durable
/// (`shells.json`), not in here, so a respawned monitor judges the pane
/// the same way.
#[derive(Debug, Default)]
struct MonitorState {
    cooldown_until: Option<Instant>,
    last_noresume: Option<(Option<String>, bool)>,
    last_warn: Option<WarnKind>,
    /// Two-step continue: (agent, program, message) awaiting TUI boot
    /// (agents whose template carries no `{message}`, e.g. opencode —
    /// kiro inlines instead); its timing counts up in
    /// `polls_since_relaunch`, due from [`CONTINUE_AFTER_POLLS`] and
    /// dropped after [`CONTINUE_GIVE_UP_POLLS`] due polls. The program gates delivery: only the relaunched agent
    /// itself may receive the text, never vim/less/tmux opened meanwhile.
    pending_continue: Option<(String, String, String)>,
    /// Continue ticks since the relaunch that armed `pending_continue`
    /// (the relaunch poll itself is not ticked): submission is due from
    /// [`CONTINUE_AFTER_POLLS`], and a continue still undelivered after
    /// [`CONTINUE_GIVE_UP_POLLS`] due polls is dropped (and logged).
    polls_since_relaunch: u8,
    /// Polls left before a failed kiro session lookup is retried.
    kiro_retry_in: u8,
}

/// Polls between a relaunch and its two-step continue submission:
/// long enough for typical TUI boot, short enough to stay relevant.
const CONTINUE_AFTER_POLLS: u8 = 3;

/// Polls past [`CONTINUE_AFTER_POLLS`] a continue may wait for the
/// relaunched program before it is dropped as stale.
const CONTINUE_GIVE_UP_POLLS: u8 = 10;

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

    /// A successful relaunch or continue clears all warn state: the world
    /// changed, so the next failure is new information again.
    fn clear_warns(&mut self) {
        self.last_warn = None;
        self.last_noresume = None;
    }

    /// A Refresh clears warn state except a `process-info` failure: a
    /// live `agent_session` refreshes every poll even while the
    /// foreground is unreadable, which would otherwise re-log it each
    /// time. That warning re-arms only once a poll parses cleanly.
    fn clear_warns_on_refresh(&mut self) {
        if self.last_warn != Some(WarnKind::ProcInfoFailed) {
            self.last_warn = None;
        }
        self.last_noresume = None;
    }

    /// Disarm a pending two-step continue, logging once why it will
    /// never be delivered (no-op when none is pending).
    fn drop_continue(&mut self, pane_id: &str, reason: &str) {
        if let Some((agent, _, _)) = self.pending_continue.take() {
            crate::state::append_log(&format!(
                "monitor {pane_id}: dropped pending continue for {agent}: {reason}"
            ));
        }
        self.polls_since_relaunch = 0;
    }
}

/// True when the foreground looks like a live agent (never a shell in
/// any form, never unparseable): necessary but not sufficient — the
/// continue gate also requires `entries_run` of the relaunched
/// program.
fn foreground_looks_live(entries: &[ProcEntry]) -> bool {
    if entries.is_empty() {
        return false;
    }
    entries.iter().all(|e| {
        let a = &e.argv;
        if a.iter().any(|t| t == UNPARSEABLE) {
            return false;
        }
        a.first().is_some_and(|prog| !is_shell_name(prog))
    })
}

/// One monitor poll. Returns false when the pane no longer exists
/// (`pane_get` → None) so the caller can count consecutive misses.
/// Thin I/O shell around the pure `decide_poll`.
fn poll_once(pane_id: &str, config: &Config, st: &mut MonitorState) -> bool {
    let Some(mut pane) = herdr::pane_get(pane_id) else {
        return false;
    };
    let info = herdr::process_info(pane_id);
    let procs = match &info {
        Some(v) => {
            let procs = foreground_entries(v);
            if procs.is_empty()
                || procs
                    .iter()
                    .any(|e| e.argv.iter().any(|t| t == UNPARSEABLE))
            {
                // Fail-closed AND diagnosable: record the shape we saw
                // so future `process-info` drift shows up in the log.
                // Warn-once: a persistently broken shape must not spam
                // every poll (same philosophy as the NoResume throttle).
                let keys = v
                    .as_object()
                    .map(|o| o.keys().map(String::as_str).collect::<Vec<_>>().join(","))
                    .unwrap_or_else(|| "<non-object>".to_string());
                st.warn_once(
                    WarnKind::ProcInfoFailed,
                    format!("monitor {pane_id}: empty/unparseable foreground parse; keys={keys}"),
                );
            } else if st.last_warn == Some(WarnKind::ProcInfoFailed) {
                // Recovered: a later failure is new information.
                st.last_warn = None;
            }
            procs
        }
        None => {
            st.warn_once(
                WarnKind::ProcInfoFailed,
                format!("monitor {pane_id}: process-info failed; vetoing relaunch"),
            );
            vec![]
        }
    };
    // One registry read per poll: the kiro pin and claude capture below
    // persist through `state` and update this snapshot in step.
    let mut reg = crate::state::load_registry();
    // The pane's agent — herdr's label, else the registry's — and its
    // program, resolved once per poll. Neither step below changes it:
    // the kiro pin acts only for a kiro-cli agent, the claude capture
    // only for a claude one. The agent's template flag and own-process
    // session value are derived once too ([`own_session`]).
    let agent = pane
        .agent
        .clone()
        .or_else(|| reg.get(pane_id).map(|s| s.agent.clone()));
    let program = agent.as_deref().map(|a| agent_program(a, &config.commands));
    let own = own_session(
        pane.agent.as_deref(),
        program.as_deref(),
        &procs,
        &config.commands,
    );
    let own_value = own.as_ref().map(|(_, s)| s.clone());
    // One shell-record read per poll, kept in step with the writes below
    // (each passes it as the unchanged-check snapshot).
    // A flag value seen for one agent is no evidence for a different
    // agent: a changed agent clears it.
    let mut record = crate::state::shell_record(pane_id);
    if let Some(r) = record.clone().filter(|r| {
        agent.is_some()
            && r.argv_seen
                .as_ref()
                .is_some_and(|s| Some(&s.agent) != agent.as_ref())
    }) {
        let cleared = ShellRecord {
            argv_seen: None,
            ..r
        };
        crate::state::record_shell_with_snapshot(pane_id, cleared.clone(), record.as_ref());
        record = Some(cleared);
    }
    maybe_pin_kiro_session(
        &pane,
        &procs,
        program.as_deref(),
        own_value.as_ref(),
        &mut reg,
        config,
        st,
    );
    // The running claude's own session is live evidence: it outranks
    // a stale registry value and a `--resume OLD` left in its argv.
    if let Some(sess) = maybe_capture_claude(
        &pane,
        agent.as_deref(),
        program.as_deref(),
        &procs,
        &mut reg,
    ) {
        pane.agent_session.get_or_insert(sess);
    }
    let shell = info.as_ref().and_then(shell_pid).and_then(shell_identity);
    // Liveness evidence for `classify_idle`: the pane's agent is running
    // under this shell right now. Positive program match only — a stale
    // agent label over vim/less must not re-arm a retired pane.
    // Whether the agent's own session flag is new evidence: seen appear
    // under a shell already watched ([`SeenArgv::is_new`]). A first
    // sighting — no record, a new shell, a record without `argv_seen` —
    // never is: the registry wins.
    let mut argv_new = false;
    if let Some(shell) = &shell {
        if let Some(program) = program.as_deref() {
            if entries_run(&procs, program) {
                // The same shell keeps its last sighting (updated to
                // this poll's process, flagged or not, bar a flag the
                // same process was already seen with: [`SeenArgv::merge`]);
                // a new one never inherits it.
                let seen = record
                    .as_ref()
                    .filter(|r| &r.shell == shell)
                    .and_then(|r| r.argv_seen.clone());
                let current = pane
                    .agent
                    .as_deref()
                    .and_then(|a| own_sighting(&procs, program, a, own.clone()));
                argv_new = current
                    .as_ref()
                    .is_some_and(|c| SeenArgv::is_new(seen.as_ref(), c));
                let live = ShellRecord {
                    shell: shell.clone(),
                    live: true,
                    argv_seen: SeenArgv::merge(seen, current),
                };
                crate::state::record_shell_with_snapshot(pane_id, live.clone(), record.as_ref());
                record = Some(live);
            }
        }
    }
    // Skips the countdown tick below when this poll armed it, so the
    // message goes out three full polls after the relaunch, not two.
    let mut skip_tick = false;
    match decide_poll(
        &pane,
        &procs,
        own_value.as_ref(),
        &reg,
        config,
        &st.cooldown_until,
        argv_new,
    ) {
        PollAction::Refresh(sess) => {
            // A live agent re-arms the registry (valued path for the
            // next crash) and the warn state below.
            // A live agent of a DIFFERENT name retires a pending
            // continue: the message belongs to the relaunched agent,
            // never to whatever the user started manually afterwards.
            if st
                .pending_continue
                .as_ref()
                .is_some_and(|(pending_agent, _, _)| pending_agent != &sess.agent)
            {
                st.drop_continue(pane_id, &format!("'{}' is running instead", sess.agent));
            }
            st.clear_warns_on_refresh();
            crate::state::remember_with_snapshot(pane_id, sess, &reg);
        }
        PollAction::Relaunch {
            agent,
            cmd:
                crate::resume::ResumeCommand {
                    key: template_key,
                    program,
                    argv,
                },
        } => {
            // Crash vs. user exit: only a session-host crash/restart
            // (the agent's shell was replaced) resumes. Ctrl-C, `/exit`
            // or any death inside the still-running shell stays dead,
            // and so does a pane we never saw the agent run in.
            match classify_idle(record.as_ref(), shell.as_deref()) {
                IdleCause::HostRestart => {}
                cause => {
                    // No relaunch is forthcoming: a pending continue
                    // belongs to a gone agent, so disarm (a future manual
                    // agent must not receive stale text).
                    st.drop_continue(pane_id, "the relaunched agent exited");
                    if cause == IdleCause::UserExit {
                        // Retire the record so a later host restart does
                        // not resurrect what the user quit. The last
                        // sighting stays: a process started here next has
                        // a new pid, so its flag is evidence.
                        if let Some(r) = record.as_ref().filter(|r| r.live) {
                            let retired = ShellRecord {
                                shell: r.shell.clone(),
                                live: false,
                                argv_seen: r.argv_seen.clone(),
                            };
                            crate::state::record_shell_with_snapshot(pane_id, retired, Some(r));
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
            let (profile, default) = if claude_profile_applies(&agent, &program) {
                resume_profile(&pane)
            } else {
                (None, None)
            };
            let (argv, applied) =
                with_claude_profile(&program, argv, profile.as_deref(), default.as_deref());
            if !applied {
                crate::state::append_log(&format!(
                    "monitor {pane_id}: WARNING resume command does not start with `claude` (use a direct-binary template); relaunching without --profile {}",
                    profile.as_deref().unwrap_or_default()
                ));
            }
            let args: Vec<&str> = argv.iter().map(String::as_str).collect();
            if herdr::pane_run(pane_id, &args) {
                st.cooldown_until =
                    Some(Instant::now() + Duration::from_secs(config.cooldown_seconds));
                // One-shot resurrection: the relaunch spends the saved
                // session (failed delivery keeps it and retries).
                if !crate::state::forget_with_snapshot(pane_id, &reg) {
                    crate::state::append_log(&format!(
                        "monitor {pane_id}: WARNING relaunched {agent} but could not spend its saved session; a later idle shell may resume it again"
                    ));
                }
                // The resumed agent now lives in this shell: its next
                // death here is a user exit, not another crash. (Only a
                // further host restart earns another comeback.) It
                // also pins the value as the next poll's argv read will
                // see it (None for a valueless template), with no pid
                // yet: our own process showing that value is no evidence.
                if let Some(shell) = &shell {
                    if !crate::state::record_shell_with_snapshot(
                        pane_id,
                        ShellRecord {
                            shell: shell.clone(),
                            live: true,
                            argv_seen: Some(SeenArgv {
                                agent: agent.clone(),
                                value: crate::resume::session_from_argv_with_commands(
                                    &agent,
                                    std::slice::from_ref(&argv),
                                    &config.commands,
                                )
                                .map(|s| s.value),
                                pid: None,
                            }),
                        },
                        record.as_ref(),
                    ) {
                        crate::state::append_log(&format!(
                            "monitor {pane_id}: WARNING relaunched {agent} but could not pin its shell; its next in-place exit may be misread as a host restart"
                        ));
                    }
                }
                // Two-step continue: a configured message the template
                // did not inline (e.g. opencode) is submitted after a few
                // polls once the TUI is plausibly booted.
                if let Some(message) = config.resume_message.clone() {
                    let template = config.commands.get(&template_key);
                    if !template.is_some_and(|t| t.contains(crate::resume::MESSAGE_PLACEHOLDER)) {
                        st.pending_continue = Some((agent.clone(), program, message));
                        st.polls_since_relaunch = 0;
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
            st.drop_continue(pane_id, "no resume is possible");
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
    // relaunched program (`entries_run`: name-only claude and
    // node-launched CLIs count; vim/less/tmux opened meanwhile, or a
    // shell, never do). Otherwise it waits; after
    // `CONTINUE_GIVE_UP_POLLS` more polls it is dropped with a log line.
    // A gone pane exits via the miss path above, dropping the message.
    if st.pending_continue.is_some() && !skip_tick {
        st.polls_since_relaunch = st.polls_since_relaunch.saturating_add(1);
        let due = st.polls_since_relaunch >= CONTINUE_AFTER_POLLS;
        let program_live = st
            .pending_continue
            .as_ref()
            .is_some_and(|(_, program, _)| entries_run(&procs, program));
        if due && foreground_looks_live(&procs) && program_live {
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
        } else if due {
            // Due polls so far, this one included: the first due poll
            // is the first of CONTINUE_GIVE_UP_POLLS attempts.
            let attempts = st.polls_since_relaunch - CONTINUE_AFTER_POLLS + 1;
            if attempts >= CONTINUE_GIVE_UP_POLLS {
                let program = st
                    .pending_continue
                    .as_ref()
                    .map(|(_, p, _)| p.clone())
                    .unwrap_or_default();
                st.drop_continue(
                    pane_id,
                    &format!(
                        "'{program}' never took the foreground within {CONTINUE_GIVE_UP_POLLS} polls of boot"
                    ),
                );
            }
        }
    }
    true
}

/// Shared monitor exit path: release the monitor lock — only while it
/// still names us (compare-and-delete, so a successor's lock survives) —
/// and clear our stop sentinel.
fn finish_monitor(pane_id: &str, own_pid: u32) {
    crate::state::release_monitor_lock(pane_id, own_pid);
    crate::state::clear_stop_sentinel(own_pid);
}

/// The ownership check run each grace second and each poll
/// ([`crate::state::hold_monitor_lock`], one read of the lock): false
/// only when the lock positively names a different pid that is the live
/// monitor for this pane — its lock is not ours to clear, so just our
/// own stop sentinel is cleaned up. A lock deleted under us, or left by
/// a dead pid, is re-claimed (logged) and the monitor keeps running; a
/// re-claim that fails on I/O (ENOSPC, EACCES …) is logged once per
/// failure streak (`reclaim_failing`) and retried next poll.
fn keep_lock(pane_id: &str, own_pid: u32, reclaim_failing: &mut bool) -> bool {
    use crate::state::LockHold;
    match crate::state::hold_monitor_lock(pane_id, own_pid) {
        LockHold::Ours => {}
        LockHold::Reclaimed => crate::state::append_log(&format!(
            "monitor {pane_id}: lock missing or stale, re-claimed (pid {own_pid})"
        )),
        LockHold::Failed => {
            if !std::mem::replace(reclaim_failing, true) {
                crate::state::append_log(&format!(
                    "monitor {pane_id}: lock missing or stale but re-claim failed (pid {own_pid}); retrying next poll"
                ));
            }
            return true;
        }
        LockHold::Lost(rival) => {
            crate::state::append_log(&format!(
                "monitor {pane_id}: lock no longer ours (pid {own_pid}; live monitor {rival} holds it), exiting"
            ));
            crate::state::clear_stop_sentinel(own_pid);
            return false;
        }
    }
    *reclaim_failing = false;
    true
}

/// Monitor loop for one pane: claim the monitor lock FIRST, then sleep
/// `connect_grace_seconds` (the pane may still be connecting after
/// restore), then poll every `poll_seconds` until a `stop-<pid>` sentinel
/// appears for our pid, another live monitor for the pane holds the lock
/// (checked every grace second and every poll; a missing or stale lock
/// is re-claimed instead, and a failed re-claim retried, see
/// [`keep_lock`]), or the pane stays gone for
/// 10 straight polls (closed/deleted panes must not spin a monitor
/// forever).
///
/// Lock-then-sleep (not sleep-then-lock): the lock is visible during the
/// long grace sleep, so a racing second monitor — and `supervise-all` —
/// sees it via `live_monitor_pid` instead of double-spawning. A
/// simultaneous-start race (both check before either writes) remains but
/// is millisecond-narrow instead of grace-seconds-wide; the per-poll
/// ownership check retires the loser.
pub fn run(pane_id: &str) {
    run_with_sleep(pane_id, std::thread::sleep);
}

/// [`run`] with the sleep injected, so tests drive the loop instantly.
fn run_with_sleep(pane_id: &str, sleep: impl Fn(Duration)) {
    // Config warnings are `startup`'s to report ([`crate::config::load`]).
    let (config, _) = crate::config::load();
    if crate::state::live_monitor_pid(pane_id).is_some() {
        return;
    }
    if !crate::state::write_monitor_lock(pane_id) {
        return;
    }
    // Grace sleep in 1s slices so `stop` takes effect in ~1s instead of
    // after the full `connect_grace_seconds`.
    let own_pid = std::process::id();
    let mut reclaim_failing = false;
    for _ in 0..config.connect_grace_seconds {
        if crate::state::stop_requested(own_pid) {
            finish_monitor(pane_id, own_pid);
            return;
        }
        if !keep_lock(pane_id, own_pid, &mut reclaim_failing) {
            return;
        }
        sleep(Duration::from_secs(1));
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
        if !keep_lock(pane_id, own_pid, &mut reclaim_failing) {
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
                crate::state::append_log(&format!("monitor {pane_id}: pane gone 10x, exiting"));
                finish_monitor(pane_id, own_pid);
                break;
            }
        }
        sleep(Duration::from_secs(config.poll_seconds));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`decide_poll`] with its per-poll inputs derived as `poll_once`
    /// derives them.
    fn decide(
        pane: &Pane,
        procs: &[ProcEntry],
        registry: &HashMap<String, SessionRef>,
        config: &Config,
        cooldown_until: &Option<Instant>,
    ) -> PollAction {
        decide_with(pane, procs, registry, config, cooldown_until, true)
    }

    /// [`decide`] with `argv_new` given.
    fn decide_with(
        pane: &Pane,
        procs: &[ProcEntry],
        registry: &HashMap<String, SessionRef>,
        config: &Config,
        cooldown_until: &Option<Instant>,
        argv_new: bool,
    ) -> PollAction {
        let program = pane
            .agent
            .clone()
            .or_else(|| registry.get(&pane.pane_id).map(|s| s.agent.clone()))
            .map(|a| agent_program(&a, &config.commands));
        let own_value = own_session(
            pane.agent.as_deref(),
            program.as_deref(),
            procs,
            &config.commands,
        )
        .map(|(_, s)| s);
        decide_poll(
            pane,
            procs,
            own_value.as_ref(),
            registry,
            config,
            cooldown_until,
            argv_new,
        )
    }
    use crate::herdr::SessionRef;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    /// Argv-only foreground entries (no `name`/`pid`), as a bare-array
    /// `process-info` list would parse.
    fn procs(argv: &[Vec<String>]) -> Vec<ProcEntry> {
        argv.iter().cloned().map(ProcEntry::from_argv).collect()
    }

    fn rec(shell: &str, live: bool) -> ShellRecord {
        ShellRecord {
            shell: shell.into(),
            live,
            argv_seen: None,
        }
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
        assert_eq!(
            classify_idle(Some(&rec("s1", true)), None),
            IdleCause::Unknown
        );
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
    fn foreground_runs_ignores_program_named_arguments() {
        let fg = |argv: &[&str]| {
            serde_json::json!({"foreground_processes": [
                {"argv": argv, "name": argv[0].rsplit('/').next()}
            ]})
        };
        assert!(!foreground_runs(&fg(&["vim", "opencode.json"]), "opencode"));
        assert!(!foreground_runs(&fg(&["less", "claude.log"]), "claude"));
        assert!(!foreground_runs(&fg(&["man", "claude"]), "claude"));
        assert!(!foreground_runs(
            &fg(&["node", "--inspect", "tool.js", "codex"]),
            "codex"
        ));
        assert!(foreground_runs(&fg(&["opencode"]), "opencode"));
        assert!(foreground_runs(
            &fg(&["/usr/local/bin/opencode", "--session", "s"]),
            "opencode"
        ));
        assert!(foreground_runs(
            &fg(&[
                "node",
                "/usr/lib/node_modules/@openai/codex/bin/codex.js",
                "resume",
                "x"
            ]),
            "codex"
        ));
        assert!(foreground_runs(
            &fg(&["/usr/bin/python3.12", "-u", "/opt/hermes.py"]),
            "hermes"
        ));
        assert!(foreground_runs(
            &fg(&["node", "-r", "dotenv/config", "/usr/local/bin/codex"]),
            "codex"
        ));
        assert!(foreground_runs(
            &fg(&["python3", "-X", "utf8", "/opt/hermes.py"]),
            "hermes"
        ));
        // The script ends the scan: its own arguments never count.
        assert!(!foreground_runs(
            &fg(&["node", "tool.js", "codex"]),
            "codex"
        ));
        assert!(!foreground_runs(
            &fg(&["node", "lib/tool", "codex"]),
            "codex"
        ));
        let name_only = serde_json::json!({"foreground_processes": [{"name": "claude", "pid": 1}]});
        assert!(foreground_runs(&name_only, "claude"));
    }

    #[test]
    fn foreground_runs_does_not_see_through_launchers() {
        let fg = |argv: &[&str]| {
            serde_json::json!({"foreground_processes": [
                {"argv": argv, "name": argv[0].rsplit('/').next()}
            ]})
        };
        // Launcher-started agents are not recognised (configure a
        // direct-binary template instead).
        assert!(!foreground_runs(
            &fg(&["npx", "-y", "@openai/codex"]),
            "codex"
        ));
        assert!(!foreground_runs(&fg(&["uv", "run", "hermes"]), "hermes"));
        assert!(!foreground_runs(&fg(&["env", "FOO=1", "codex"]), "codex"));
        // Interpreter flags with values, around deno's `run`.
        assert!(foreground_runs(
            &fg(&["deno", "run", "--config", "f", "codex.ts"]),
            "codex"
        ));
        assert!(foreground_runs(
            &fg(&[
                "deno",
                "run",
                "-c",
                "deno.json",
                "--import-map",
                "m.json",
                "/opt/codex.ts"
            ]),
            "codex"
        ));
        assert!(!foreground_runs(
            &fg(&["deno", "run", "--config", "codex.ts", "tool.ts"]),
            "codex"
        ));
        assert!(!foreground_runs(&fg(&["vim", "opencode.json"]), "opencode"));
    }

    #[test]
    fn foreground_runs_matches_dotted_argv0_exactly_or_by_stem() {
        let fg = |argv: &[&str]| serde_json::json!({"foreground_processes": [{"argv": argv}]});
        assert!(foreground_runs(
            &fg(&["/opt/kiro-cli.AppImage", "chat"]),
            "kiro-cli.AppImage"
        ));
        assert!(foreground_runs(
            &fg(&["/opt/kiro-cli.AppImage"]),
            "kiro-cli"
        ));
        assert!(foreground_runs(&fg(&["./codex.sh"]), "codex.sh"));
        assert!(foreground_runs(&fg(&["./codex.sh"]), "codex"));
    }

    #[test]
    fn foreground_runs_parses_interpreter_entry_points() {
        let fg = |argv: &[&str]| serde_json::json!({"foreground_processes": [{"argv": argv}]});
        // Only the entry point counts, never the script's own arguments.
        assert!(!foreground_runs(
            &fg(&["python3", "-m", "pytest", "-k", "claude"]),
            "claude"
        ));
        assert!(!foreground_runs(
            &fg(&["python", "-m", "pip", "install", "claude"]),
            "claude"
        ));
        assert!(!foreground_runs(
            &fg(&["python3", "-c", "import hermes", "hermes"]),
            "hermes"
        ));
        assert!(!foreground_runs(
            &fg(&["node", "-e", "require('codex')", "codex"]),
            "codex"
        ));
        assert!(!foreground_runs(
            &fg(&["node", "--print=1", "codex"]),
            "codex"
        ));
        // A flag's separate value is never taken for the script.
        assert!(foreground_runs(
            &fg(&[
                "node",
                "-r",
                "./otel.js",
                "/usr/lib/node_modules/@openai/codex/bin/codex.js"
            ]),
            "codex"
        ));
        assert!(foreground_runs(
            &fg(&["node", "--require=./otel.js", "--inspect=9229", "codex.mjs"]),
            "codex"
        ));
        assert!(foreground_runs(&fg(&["node", "--", "codex.cjs"]), "codex"));
        assert!(foreground_runs(
            &fg(&["deno", "run", "--allow-all", "/opt/codex.ts"]),
            "codex"
        ));
        // bun takes `run` as a subcommand too, never as the script; a
        // package `index` entry point is named by its directory.
        assert!(foreground_runs(
            &fg(&["bun", "run", "/opt/opencode/index.ts"]),
            "opencode"
        ));
        assert!(foreground_runs(
            &fg(&["bun", "run", "/opt/opencode.ts"]),
            "opencode"
        ));
        assert!(!foreground_runs(&fg(&["bun", "run", "tool.ts"]), "run"));
        assert!(!foreground_runs(
            &fg(&["bun", "run", "index.ts"]),
            "opencode"
        ));
        assert!(!foreground_runs(
            &fg(&["python3", "/opt/hermes/index.py"]),
            "hermes"
        ));
        assert!(foreground_runs(&fg(&["python", "-m", "hermes"]), "hermes"));
        assert!(foreground_runs(
            &fg(&["python3.12", "-u", "-m", "hermes.cli", "--resume", "x"]),
            "hermes"
        ));
        assert!(!foreground_runs(
            &fg(&["python3", "-W", "ignore", "/opt/tool.py", "hermes"]),
            "hermes"
        ));
        // Only listed script extensions are dropped.
        assert!(!foreground_runs(&fg(&["node", "codex.py"]), "codex"));
    }

    #[test]
    fn foreground_runs_reads_top_level_argv_shape() {
        // The same entries `foreground_argv` derives: a single top-level
        // `argv` with no `foreground_processes`.
        let info = serde_json::json!({"argv": ["kiro-cli", "chat"]});
        assert_eq!(foreground_argv(&info), vec![vec!["kiro-cli", "chat"]]);
        assert!(foreground_runs(&info, "kiro-cli"));
        let node = serde_json::json!({"argv": ["node", "/opt/codex.js"]});
        assert!(foreground_runs(&node, "codex"));
        assert!(!foreground_runs(&info, "claude"));
        // `foreground_processes` still wins when present.
        let both = serde_json::json!({"foreground_processes": [{"argv": ["vim"]}],
            "argv": ["kiro-cli"]});
        assert!(!foreground_runs(&both, "kiro-cli"));
    }

    #[test]
    fn bare_argv_array_entries_feed_every_matcher() {
        // `foreground_processes: [[argv...]]`: the same entries for the
        // veto, the program matcher and the own-process argv.
        let info = serde_json::json!({"foreground_processes": [
            ["opencode", "--session", "s"],
            ["node", "/opt/mcp.js", "--session", "x"]
        ]});
        assert_eq!(
            foreground_argv(&info),
            vec![
                vec!["opencode".to_string(), "--session".into(), "s".into()],
                vec![
                    "node".into(),
                    "/opt/mcp.js".into(),
                    "--session".into(),
                    "x".into()
                ]
            ]
        );
        assert!(foreground_runs(&info, "opencode"));
        assert!(!foreground_runs(&info, "claude"));
        let entries = foreground_entries(&info);
        assert_eq!(
            own_flagged(&entries, "opencode", "opencode", "--session").map(|(_, s)| s.value),
            Some("s".to_string())
        );
        // A bare `claude` array carries no pid.
        let claude = serde_json::json!({"foreground_processes": [["claude"]]});
        assert!(foreground_runs(&claude, "claude"));
        assert_eq!(claude_pid(&foreground_entries(&claude), "claude"), None);
    }

    #[test]
    fn name_only_entries_parse_as_their_program() {
        // Live claude shape (herdr 0.9.5): `name` + `pid`, no argv.
        let info = serde_json::json!({"foreground_processes": [
            {"cwd": "/home/joseph", "name": "claude", "pid": 31533},
            {"argv": ["agentmail", "mcp"], "name": "agentmail", "pid": 31682}
        ]});
        let argv = foreground_argv(&info);
        assert_eq!(
            argv,
            vec![
                vec!["claude".to_string()],
                vec!["agentmail".into(), "mcp".into()]
            ]
        );
        assert!(
            !should_relaunch(&procs(&argv), None),
            "a live claude vetoes"
        );
        // A name-only shell may be `bash long-job.sh` with an unreadable
        // cmdline: fail closed, never a bare idle shell.
        for shell in [
            serde_json::json!({"foreground_processes": [{"name": "bash", "pid": 7}]}),
            serde_json::json!({"foreground_processes": [{"name": "zsh", "argv": []}]}),
            serde_json::json!({"foreground_processes": [{"name": "-zsh", "argv": null}]}),
        ] {
            let argv = foreground_argv(&shell);
            assert_eq!(argv, vec![vec![UNPARSEABLE.to_string()]], "{shell}");
            assert!(!should_relaunch(&procs(&argv), None), "{shell}");
        }
        // Mixed with a parseable entry the poison still lands.
        let mixed = serde_json::json!({"foreground_processes": [
            {"argv": ["/usr/bin/zsh"], "name": "zsh"},
            {"name": "bash", "pid": 8}
        ]});
        assert!(!should_relaunch(&foreground_entries(&mixed), None));
        // A present-but-malformed argv is still corruption.
        let bad = serde_json::json!({"foreground_processes": [{"name": "zsh", "argv": [1]}]});
        assert_eq!(foreground_argv(&bad), vec![vec![UNPARSEABLE.to_string()]]);
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
        assert_eq!(
            claude_pid(&foreground_entries(&info), "claude"),
            Some(17572)
        );
        let argv_only = serde_json::json!({"foreground_processes": [
            {"argv": ["/home/joseph/.local/bin/claude", "--resume", "x"], "pid": 9}
        ]});
        assert_eq!(
            claude_pid(&foreground_entries(&argv_only), "claude"),
            Some(9)
        );
        let other = serde_json::json!({"foreground_processes": [
            {"argv": ["vim"], "name": "vim", "pid": 3}
        ]});
        assert_eq!(claude_pid(&foreground_entries(&other), "claude"), None);
        // A child claude (e.g. a `claude -p` subprocess) listed first:
        // the lowest pid, the parent, wins.
        let child_first = serde_json::json!({"foreground_processes": [
            {"argv": ["claude", "-p", "x"], "name": "claude", "pid": 900},
            {"argv": ["claude"], "pid": null},
            {"argv": ["claude", "--resume", "s"], "name": "claude", "pid": 400}
        ]});
        assert_eq!(
            claude_pid(&foreground_entries(&child_first), "claude"),
            Some(400)
        );
    }

    #[test]
    fn own_sighting_ignores_report_order() {
        // Two own processes (flagged and bare, or both flagged) reported
        // in swapped order on the next poll are the same sighting.
        let entry = |argv: &[&str], pid| ProcEntry {
            name: None,
            argv: argv.iter().map(|t| t.to_string()).collect(),
            pid: Some(pid),
        };
        let commands = Config::default().commands;
        let sighting = |entries: &[ProcEntry]| {
            let own = own_session(Some("opencode"), Some("opencode"), entries, &commands);
            own_sighting(entries, "opencode", "opencode", own)
        };
        for (a, b) in [
            (
                entry(&["opencode", "--session", "s1"], 20),
                entry(&["opencode", "--session", "s2"], 10),
            ),
            (entry(&["opencode"], 30), entry(&["opencode"], 40)),
        ] {
            let seen = sighting(&[a.clone(), b.clone()]);
            let current = sighting(&[b, a]);
            assert_eq!(seen, current);
            assert!(!SeenArgv::is_new(
                seen.as_ref(),
                &current.expect("sighting")
            ));
        }
        // The flagged entry wins over a lower-pid bare one; a pid-less
        // entry only when none has a pid.
        let mixed = [
            entry(&["opencode"], 5),
            entry(&["opencode", "--session", "s1"], 9),
        ];
        let got = sighting(&mixed).expect("sighting");
        assert_eq!((got.value.as_deref(), got.pid), (Some("s1"), Some(9)));
        let pidless = [
            ProcEntry::from_argv(vec!["opencode".into()]),
            entry(&["opencode"], 7),
        ];
        let got = sighting(&pidless).expect("sighting");
        assert_eq!(got.pid, Some(7));
    }

    #[test]
    fn own_session_value_is_the_sightings_process() {
        // Two flagged own processes in either report order: the session
        // value and the sighting name the same (lowest-pid) process.
        let entry = |value: &str, pid| ProcEntry {
            name: None,
            argv: vec!["opencode".into(), "--session".into(), value.into()],
            pid: Some(pid),
        };
        let commands = Config::default().commands;
        let (a, b) = (entry("s1", 20), entry("s2", 10));
        for entries in [[a.clone(), b.clone()], [b, a]] {
            let own = own_session(Some("opencode"), Some("opencode"), &entries, &commands);
            let value = own.as_ref().map(|(_, s)| s.value.clone());
            let seen = own_sighting(&entries, "opencode", "opencode", own).expect("sighting");
            assert_eq!(value.as_deref(), Some("s2"));
            assert_eq!((seen.value.as_deref(), seen.pid), (Some("s2"), Some(10)));
        }
    }

    #[test]
    fn with_claude_profile_flags_only_non_default_profiles() {
        let argv = || vec!["claude".to_string(), "--resume".into(), "s1".into()];
        let flagged = vec!["claude", "--profile", "personal", "--resume", "s1"];
        // `program` as the relaunch derives it from the template.
        let wp = |agent, argv: Vec<String>, p: Option<&str>, d| {
            let program = crate::resume::template_program(&argv.join(" ")).unwrap_or_default();
            let p = p.filter(|_| claude_profile_applies(agent, &program));
            with_claude_profile(&program, argv, p, d)
        };
        assert_eq!(
            wp("claude", argv(), Some("personal"), Some("work")),
            (flagged.iter().map(|t| t.to_string()).collect(), true)
        );
        assert_eq!(
            wp("claude", argv(), Some("work"), Some("work")),
            (argv(), true),
            "default profile: plain resume"
        );
        assert_eq!(wp("claude", argv(), None, Some("work")), (argv(), true));
        assert_eq!(
            wp("claude", argv(), Some("personal"), None).0,
            flagged,
            "no default set: any profile is explicit"
        );
        let kiro = vec!["kiro-cli".to_string(), "chat".into()];
        assert_eq!(
            wp("kiro", kiro.clone(), Some("personal"), Some("work")),
            (kiro, true),
            "only claude takes --profile"
        );
        // An aliased key whose template runs claude takes it too.
        assert_eq!(
            wp("claude-work", argv(), Some("personal"), Some("work")).0,
            flagged,
            "aliased claude key"
        );
        // A path argv[0] takes the flag too.
        let path = crate::resume::split_argv("/usr/bin/claude --resume X");
        assert_eq!(
            wp("claude", path, Some("work"), None).0,
            vec!["/usr/bin/claude", "--profile", "work", "--resume", "X"]
        );
        // A launcher prefix is not parsed: unflagged, reported as not
        // applied — even past a path argument named `claude`.
        for launcher in [
            "env FOO=1 claude --resume X",
            "env -C /home/me/claude claude --resume X",
        ] {
            let argv = crate::resume::split_argv(launcher);
            assert_eq!(
                wp("claude", argv.clone(), Some("work"), None),
                (argv, false),
                "{launcher}"
            );
        }
        // No `claude` argv[0]: relaunch unflagged, reported as not applied.
        let wrapped = crate::resume::split_argv("my-claude --resume X");
        assert_eq!(
            wp("claude", wrapped.clone(), Some("work"), None),
            (wrapped, false)
        );
    }

    #[test]
    fn agent_program_comes_from_resume_template() {
        let commands = Config::default().commands;
        assert_eq!(agent_program("kiro", &commands), "kiro-cli");
        assert_eq!(agent_program("claude", &commands), "claude");
        assert_eq!(
            agent_program("zed", &commands),
            "zed",
            "unknown agent: own name"
        );
        let commands = HashMap::from([(
            "kiro".to_string(),
            "/opt/kiro-cli.AppImage chat -r".to_string(),
        )]);
        assert_eq!(agent_program("kiro", &commands), "kiro-cli.AppImage");
        let commands = HashMap::from([
            ("kiro".to_string(), " ".to_string()),
            ("kiro-fallback".to_string(), "kiro-cli chat -r".to_string()),
        ]);
        assert_eq!(
            agent_program("kiro", &commands),
            "kiro-cli",
            "blank primary"
        );
    }

    #[test]
    fn relaunches_dead_pane_at_idle_shell() {
        let argv = vec![vec!["/usr/bin/zsh".into()]];
        assert!(should_relaunch(&procs(&argv), None));
    }

    #[test]
    fn decide_poll_relaunch_carries_configured_message() {
        // `resume_message` flows from config into the relaunch argv when
        // the template carries `{message}` (default kiro templates do).
        let config = Config {
            resume_message: Some("continue".into()),
            ..Config::default()
        };
        let pane = Pane {
            pane_id: "w9M:p1".into(),
            agent: Some("kiro".into()),
            agent_status: Some("idle".into()),
            ..Default::default()
        };
        let argv = vec![vec!["/usr/bin/zsh".into()]];
        let reg = HashMap::from([(
            "w9M:p1".into(),
            SessionRef {
                agent: "kiro".into(),
                value: "sess-9".into(),
            },
        )]);
        assert_eq!(
            decide(&pane, &procs(&argv), &reg, &config, &None),
            PollAction::Relaunch {
                agent: "kiro".into(),
                cmd: crate::resume::ResumeCommand {
                    key: "kiro".into(),
                    program: "kiro-cli".into(),
                    argv: vec![
                        "kiro-cli".into(),
                        "chat".into(),
                        "--resume-id".into(),
                        "sess-9".into(),
                        "continue".into(),
                    ],
                },
            }
        );
        // Without the setting the argv is identical to before.
        assert_eq!(
            decide(&pane, &procs(&argv), &reg, &Config::default(), &None),
            PollAction::Relaunch {
                agent: "kiro".into(),
                cmd: crate::resume::ResumeCommand {
                    key: "kiro".into(),
                    program: "kiro-cli".into(),
                    argv: vec![
                        "kiro-cli".into(),
                        "chat".into(),
                        "--resume-id".into(),
                        "sess-9".into(),
                    ],
                },
            }
        );
    }

    #[test]
    fn decide_poll_relaunches_dead_kiro_pane_via_fallback() {
        // Crash shape (live 2026-09-15, kiro w9M:p1): stale agent label +
        // idle status, bare `--resume` foreground carried no session id, so
        // the registry is empty. Must relaunch via the valueless
        // `kiro-fallback` template, not silently do nothing.
        let pane = Pane {
            pane_id: "w9M:p1".into(),
            agent: Some("kiro".into()),
            agent_status: Some("idle".into()),
            ..Default::default()
        };
        let argv = vec![vec!["/usr/bin/zsh".into()]];
        assert_eq!(
            decide(
                &pane,
                &procs(&argv),
                &HashMap::new(),
                &Config::default(),
                &None
            ),
            PollAction::Relaunch {
                agent: "kiro".into(),
                cmd: crate::resume::ResumeCommand {
                    key: "kiro-fallback".into(),
                    program: "kiro-cli".into(),
                    argv: vec!["kiro-cli".into(), "chat".into(), "-r".into()],
                },
            }
        );
    }

    #[test]
    fn decide_poll_relaunches_dead_pane_with_recorded_agent() {
        // Crash shape (live 2026-09-15, opencode wAA:p1): agent still
        // recorded, status done, foreground idle shell, registry holds the
        // session. Must relaunch, not merely refresh the registry.
        let pane = Pane {
            pane_id: "wAA:p1".into(),
            agent: Some("opencode".into()),
            agent_status: Some("done".into()),
            ..Default::default()
        };
        let argv = vec![vec!["/usr/bin/zsh".into()]];
        let reg = HashMap::from([(
            "wAA:p1".into(),
            SessionRef {
                agent: "opencode".into(),
                value: "ses_abc".into(),
            },
        )]);
        assert_eq!(
            decide(&pane, &procs(&argv), &reg, &Config::default(), &None),
            PollAction::Relaunch {
                agent: "opencode".into(),
                cmd: crate::resume::ResumeCommand {
                    key: "opencode".into(),
                    program: "opencode".into(),
                    argv: vec!["opencode".into(), "--session".into(), "ses_abc".into()],
                },
            }
        );
    }

    #[test]
    fn never_relaunches_with_live_foreground() {
        let argv = vec![vec!["claude".into()]];
        assert!(!should_relaunch(&procs(&argv), None));
    }

    #[test]
    fn shell_running_a_script_is_not_idle() {
        let argv = vec![vec!["/bin/sh".into(), "myscript.sh".into()]];
        assert!(!should_relaunch(&procs(&argv), None));
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
        assert!(!should_relaunch(&procs(&argv), None));
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
        assert!(should_relaunch(&procs(&argv), None));
    }

    #[test]
    fn unparseable_foreground_list_vetoes_relaunch() {
        // Non-empty list, zero parseable entries → sentinel (not a bare
        // shell name) so the veto holds fail-closed.
        let info: serde_json::Value =
            serde_json::from_str(r#"{"foreground_processes":[{"foo":1},{"bar":"x"}]}"#)
                .expect("fixture parses");
        let argv = foreground_argv(&info);
        assert_eq!(argv, vec![vec!["<unparseable>".to_string()]]);
        assert!(!should_relaunch(&procs(&argv), None));
    }

    #[test]
    fn cooldown_vetoes_otherwise_idle_pane() {
        let argv = vec![vec!["/usr/bin/zsh".into()]];
        let future = Some(Instant::now() + Duration::from_secs(300));
        assert!(!should_relaunch(&procs(&argv), future));
        let expired = Some(Instant::now() - Duration::from_secs(1));
        assert!(should_relaunch(&procs(&argv), expired));
    }

    #[test]
    fn empty_foreground_vetoes_relaunch() {
        let argv: Vec<Vec<String>> = vec![];
        assert!(!should_relaunch(&procs(&argv), None));
    }

    #[test]
    fn foreground_argv_returns_empty_on_unknown_shape() {
        // No `foreground_processes` / `argv` keys → fallthrough empty
        // (vetoes relaunch fail-closed).
        let info: serde_json::Value =
            serde_json::from_str(r#"{"frobnicate":[{"argv":["x"]}],"argv_count":1}"#)
                .expect("fixture parses");
        assert_eq!(foreground_argv(&info), Vec::<Vec<String>>::new());
    }

    #[test]
    fn decide_poll_refreshes_live_session() {
        let live = SessionRef {
            agent: "claude".into(),
            value: "live-1".into(),
        };
        let pane = Pane {
            pane_id: "w1:p1".into(),
            agent: Some("claude".into()),
            agent_status: Some("working".into()),
            agent_session: Some(live.clone()),
            ..Default::default()
        };
        let argv = vec![vec!["claude".into(), "--resume".into(), "live-1".into()]];
        assert_eq!(
            decide(
                &pane,
                &procs(&argv),
                &HashMap::new(),
                &Config::default(),
                &None
            ),
            PollAction::Refresh(live)
        );
    }

    /// One live-opencode Refresh decision for `fg` over a registry
    /// holding OLD.
    fn opencode_refresh(fg: &[&[&str]]) -> PollAction {
        let pane = Pane {
            pane_id: "w1:p1".into(),
            agent: Some("opencode".into()),
            agent_status: Some("working".into()),
            ..Default::default()
        };
        let reg = HashMap::from([(
            "w1:p1".into(),
            SessionRef {
                agent: "opencode".into(),
                value: "OLD".into(),
            },
        )]);
        let argv: Vec<Vec<String>> = fg
            .iter()
            .map(|a| a.iter().map(|t| t.to_string()).collect())
            .collect();
        decide(&pane, &procs(&argv), &reg, &Config::default(), &None)
    }

    fn opencode_ref(value: &str) -> PollAction {
        PollAction::Refresh(SessionRef {
            agent: "opencode".into(),
            value: value.into(),
        })
    }

    #[test]
    fn decide_poll_refresh_takes_agent_own_session_flag() {
        // A hand-started `opencode --session NEW` replaces the saved OLD.
        assert_eq!(
            opencode_refresh(&[&["opencode", "--session", "NEW"]]),
            opencode_ref("NEW")
        );
        // A non-agent child (MCP server) never names the agent's session.
        assert_eq!(
            opencode_refresh(&[&["opencode"], &["node", "mcp.js", "--session", "X"]]),
            opencode_ref("OLD")
        );
        // A fresh `opencode` carries no evidence of a new session.
        assert_eq!(opencode_refresh(&[&["opencode"]]), opencode_ref("OLD"));
    }

    #[test]
    fn decide_poll_refresh_argv_needs_new_evidence() {
        let pane = Pane {
            pane_id: "w1:p1".into(),
            agent: Some("opencode".into()),
            ..Default::default()
        };
        let reg = HashMap::from([(
            "w1:p1".into(),
            SessionRef {
                agent: "opencode".into(),
                value: "NEW".into(),
            },
        )]);
        let fg = |v: &str| procs(&[vec!["opencode".into(), "--session".into(), v.into()]]);
        let config = Config::default();
        // A flag not seen appearing keeps the newer saved session.
        assert_eq!(
            decide_with(&pane, &fg("OLD"), &reg, &config, &None, false),
            opencode_ref("NEW")
        );
        // One seen appearing (a hand-started process) wins.
        assert_eq!(
            decide_with(&pane, &fg("NEW2"), &reg, &config, &None, true),
            opencode_ref("NEW2")
        );
    }

    #[test]
    fn decide_poll_refresh_keeps_kiro_registry_over_bare_resume() {
        let pane = Pane {
            pane_id: "w1:p1".into(),
            agent: Some("kiro".into()),
            agent_status: Some("working".into()),
            ..Default::default()
        };
        let pinned = SessionRef {
            agent: "kiro".into(),
            value: "pinned".into(),
        };
        let reg = HashMap::from([("w1:p1".into(), pinned.clone())]);
        let argv = vec![vec![
            "kiro-cli".to_string(),
            "chat".into(),
            "--resume".into(),
            "continue".into(),
        ]];
        assert_eq!(
            decide(&pane, &procs(&argv), &reg, &Config::default(), &None),
            PollAction::Refresh(pinned)
        );
    }

    #[test]
    fn decide_poll_opted_out_agent_is_noresume() {
        // `"pi": ""` opts pi out: a saved session must not yield a
        // Relaunch with an empty argv.
        let mut config = Config::default();
        config.commands.insert("pi".into(), String::new());
        let pane = Pane {
            pane_id: "w1:p1".into(),
            agent: Some("pi".into()),
            agent_status: Some("idle".into()),
            ..Default::default()
        };
        let reg = HashMap::from([(
            "w1:p1".into(),
            SessionRef {
                agent: "pi".into(),
                value: "sess-1".into(),
            },
        )]);
        let idle = vec![vec!["/usr/bin/zsh".to_string()]];
        assert_eq!(
            decide(&pane, &procs(&idle), &reg, &config, &None),
            PollAction::NoResume {
                agent: Some("pi".into()),
                value_known: true,
            }
        );
    }

    #[test]
    fn decide_poll_relaunches_idle_shell_with_registry_ref() {
        let pane = Pane {
            pane_id: "w1:p1".into(),
            agent: None,
            agent_status: Some("unknown".into()),
            ..Default::default()
        };
        let argv = vec![vec!["/usr/bin/zsh".into()]];
        let reg = HashMap::from([(
            "w1:p1".into(),
            SessionRef {
                agent: "claude".into(),
                value: "abc-123".into(),
            },
        )]);
        let config = Config::default();
        assert_eq!(
            decide(&pane, &procs(&argv), &reg, &config, &None),
            PollAction::Relaunch {
                agent: "claude".into(),
                cmd: crate::resume::ResumeCommand {
                    key: "claude".into(),
                    program: "claude".into(),
                    argv: vec!["claude".into(), "--resume".into(), "abc-123".into()],
                },
            }
        );
        // A follow-up decide with the post-relaunch cooldown set → Nothing.
        let cooldown = Some(Instant::now() + Duration::from_secs(config.cooldown_seconds));
        assert_eq!(
            decide(&pane, &procs(&argv), &reg, &config, &cooldown),
            PollAction::Nothing
        );
    }

    #[test]
    fn decide_poll_live_agent_is_nothing() {
        let pane = Pane {
            pane_id: "w1:p1".into(),
            agent: Some("claude".into()),
            agent_status: Some("working".into()),
            ..Default::default()
        };
        let argv = vec![vec!["claude".into()]];
        assert_eq!(
            decide(
                &pane,
                &procs(&argv),
                &HashMap::new(),
                &Config::default(),
                &None
            ),
            PollAction::Nothing
        );
    }

    #[test]
    fn decide_poll_nothing_when_resume_unresolvable() {
        let config = Config::default();
        let idle = vec![vec!["/usr/bin/zsh".into()]];
        // (None, None): agentless idle pane, empty registry.
        let pane = Pane {
            pane_id: "w1:p1".into(),
            agent: None,
            agent_status: Some("unknown".into()),
            ..Default::default()
        };
        assert_eq!(
            decide(&pane, &procs(&idle), &HashMap::new(), &config, &None),
            PollAction::NoResume {
                agent: None,
                value_known: false,
            }
        );
        // resume_argv None: known agent, idle shell, valued template, no
        // value anywhere.
        let pane = Pane {
            pane_id: "w1:p1".into(),
            agent: Some("claude".into()),
            agent_status: None,
            ..Default::default()
        };
        assert_eq!(
            decide(&pane, &procs(&idle), &HashMap::new(), &config, &None),
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
        assert!(!should_relaunch(&procs(&argv), None));
    }

    #[test]
    fn legacy_processes_key_is_rejected() {
        // Deliberate narrowing: only `foreground_processes` / `argv` are
        // honored. A legacy `processes` key yields empty (vetoes relaunch).
        let info: serde_json::Value =
            serde_json::from_str(r#"{"processes":[{"argv":["/usr/bin/zsh"]}]}"#)
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
        commands.insert("kiro-fallback".to_string(), "kiro-cli chat -r".to_string());
        let config = Config {
            commands,
            ..Config::default()
        };
        let pane = Pane {
            pane_id: "w1:p1".into(),
            agent: Some("kiro".into()),
            agent_status: Some("unknown".into()),
            ..Default::default()
        };
        let idle = vec![vec!["/usr/bin/zsh".into()]];
        assert_eq!(
            decide(&pane, &procs(&idle), &HashMap::new(), &config, &None),
            PollAction::Relaunch {
                agent: "kiro".into(),
                cmd: crate::resume::ResumeCommand {
                    key: "kiro-fallback".into(),
                    program: "kiro-cli".into(),
                    argv: vec!["kiro-cli".into(), "chat".into(), "-r".into()],
                },
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
        crate::test_support::record_shell(pane_id, rec("prior-host", true));
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
            assert_eq!(
                reg.get("w1:p1").expect("session remembered").value,
                "live-1"
            );
        });
    }

    #[test]
    fn poll_once_relaunch_consumes_registry_entry() {
        // One-shot resurrection: a successful relaunch spends the saved
        // session, so an intentionally-exited agent stays dead after one
        // comeback (a live agent re-arms via Refresh).
        let script = "#!/bin/sh\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w1:p1\",\"agent\":\"claude\",\"agent_status\":\"unknown\"}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"/usr/bin/zsh\"]}]}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"run\" ]; then\necho '{\"id\":\"cli:pane:run\",\"result\":{\"ok\":true}}'\nexit 0\nfi\nexit 1\n";
        run_with_fake_herdr(&with_shell_pid(script), &[], |_| {
            crate::test_support::remember(
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
    fn poll_once_own_relaunch_flag_never_overrides_newer_session() {
        // Poll 1 relaunches `opencode --session OLD`; the monitor then
        // restarts; poll 2 herdr reports NEW (saved); poll 3 herdr omits
        // it while the same process's argv still carries our own OLD —
        // NEW must be kept.
        let get = |extra: &str| {
            format!("echo '{{\"id\":\"cli:pane:get\",\"result\":{{\"pane\":{{\"pane_id\":\"w1:p1\",\"agent\":\"opencode\",\"agent_status\":\"unknown\"{extra}}}}}}}'")
        };
        let info = |argv: &str| {
            format!("echo '{{\"id\":\"cli:pane:process-info\",\"result\":{{\"process_info\":{{\"foreground_processes\":[{{\"argv\":{argv},\"pid\":4242}}]}}}}}}'")
        };
        let script = format!(
            "#!/bin/sh\nD=\"$(dirname \"@CALL_LOG@\")\"\nN=$(cat \"$D/n\" 2>/dev/null || echo 0)\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\nN=$((N+1)); echo $N > \"$D/n\"\ncase $N in\n1) {};;\n2) {};;\n*) {};;\nesac\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\ncase $N in\n1) {};;\n*) {};;\nesac\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"run\" ]; then\necho '{{\"id\":\"cli:pane:run\",\"result\":{{\"ok\":true}}}}'\nexit 0\nfi\nexit 1\n",
            get(""),
            get(",\"agent_session\":{\"agent\":\"opencode\",\"value\":\"NEW\"}"),
            get(""),
            info("[\"/usr/bin/zsh\"]"),
            info("[\"opencode\",\"--session\",\"OLD\"]"),
        );
        run_with_fake_herdr(&with_shell_pid(&script), &[], |_| {
            crate::test_support::remember(
                "w1:p1",
                SessionRef {
                    agent: "opencode".into(),
                    value: "OLD".into(),
                },
            );
            seed_host_restart("w1:p1");
            let config = Config::default();
            let mut st = MonitorState::default();
            let saved = || {
                crate::state::load_registry()
                    .get("w1:p1")
                    .map(|s| s.value.clone())
            };
            assert!(poll_once("w1:p1", &config, &mut st)); // Relaunch OLD
            assert_eq!(
                crate::state::shell_record("w1:p1")
                    .and_then(|r| r.argv_seen)
                    .and_then(|s| s.value)
                    .as_deref(),
                Some("OLD")
            );
            // Simulated monitor restart: the seen value survives on
            // disk, not in MonitorState.
            let mut st = MonitorState::default();
            assert!(poll_once("w1:p1", &config, &mut st)); // Refresh NEW
            assert_eq!(saved().as_deref(), Some("NEW"));
            assert!(poll_once("w1:p1", &config, &mut st)); // argv OLD only
            assert_eq!(
                saved().as_deref(),
                Some("NEW"),
                "own relaunch flag is no evidence"
            );
        });
    }

    #[test]
    fn poll_once_failed_relaunch_keeps_registry() {
        // Delivery failure must NOT spend the entry: the next cooldown
        // expiry retries with the session still known.
        let script = "#!/bin/sh\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w1:p1\",\"agent\":\"claude\",\"agent_status\":\"unknown\"}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"/usr/bin/zsh\"]}]}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"run\" ]; then\nexit 1\nfi\nexit 1\n";
        run_with_fake_herdr(&with_shell_pid(script), &[], |state_dir| {
            crate::test_support::remember(
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
                crate::state::load_registry()
                    .get("w1:p1")
                    .map(|s| s.value.as_str()),
                Some("abc-123"),
                "failed delivery must keep the registry entry"
            );
            assert!(poll_once("w1:p1", &config, &mut st)); // retry fails too
            let log = std::fs::read_to_string(state_dir.join("log.txt")).unwrap_or_default();
            assert_eq!(
                log.lines()
                    .filter(|l| l.contains("pane run failed"))
                    .count(),
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
                crate::state::load_registry()
                    .get("w1:p1")
                    .map(|s| s.value.as_str()),
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
    const STAGED_SCRIPT: &str = "#!/bin/sh\necho \"$#:$@\" >> \"@CALL_LOG@\"\nD=\"$(dirname \"@CALL_LOG@\")\"\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\ncat \"$D/get.json\"\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\ncat \"$D/info.json\"\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"run\" ]; then\necho '{\"id\":\"cli:pane:run\",\"result\":{\"ok\":true}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && { [ \"$2\" = \"send-text\" ] || [ \"$2\" = \"send-keys\" ]; }; then\nexit 0\nfi\nexit 1\n";

    /// Stage one poll from raw `pane` / `process_info` objects.
    fn stage_raw(dir: &std::path::Path, pane: serde_json::Value, info: serde_json::Value) {
        let get = serde_json::json!({"id": "cli:pane:get", "result": {"pane": pane}});
        std::fs::write(dir.join("get.json"), get.to_string()).expect("stage get");
        let reply = serde_json::json!({"id": "cli:pane:process-info",
            "result": {"process_info": info}});
        std::fs::write(dir.join("info.json"), reply.to_string()).expect("stage info");
    }

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
        herdr_calls(state_dir)
            .lines()
            .filter(|l| l.contains("pane run"))
            .count()
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
            assert_eq!(
                crate::state::shell_record("wAA:p1").map(|r| r.live),
                Some(false)
            );
            let log = std::fs::read_to_string(dir.join("log.txt")).unwrap_or_default();
            assert_eq!(
                log.lines().filter(|l| l.contains("user exit")).count(),
                1,
                "{log}"
            );
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
            stage(
                dir,
                Some(std::os::unix::process::parent_id()),
                &["opencode"],
            );
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
    fn poll_once_opted_out_agent_never_runs() {
        run_with_fake_herdr(STAGED_SCRIPT, &[], |dir| {
            std::fs::write(dir.join("config.json"), r#"{"commands": {"pi": ""}}"#).expect("config");
            let (config, _) = crate::config::load();
            assert_eq!(config.commands["pi"], "");
            crate::test_support::remember(
                "w1:p1",
                SessionRef {
                    agent: "pi".into(),
                    value: "sess-1".into(),
                },
            );
            seed_host_restart("w1:p1");
            stage_raw(
                dir,
                serde_json::json!({"pane_id": "w1:p1", "agent": "pi", "agent_status": "idle"}),
                serde_json::json!({"shell_pid": std::process::id(),
                    "foreground_processes": [{"argv": ["/usr/bin/zsh"], "name": "zsh"}]}),
            );
            let mut st = MonitorState::default();
            for _ in 0..3 {
                assert!(poll_once("w1:p1", &config, &mut st));
            }
            assert_eq!(pane_runs(dir), 0, "calls: {}", herdr_calls(dir));
            let log = log_text(dir);
            assert!(!log.contains("pane run failed"), "{log}");
            assert_eq!(
                log.lines()
                    .filter(|l| l.contains("no resume argv for 'pi'"))
                    .count(),
                1,
                "{log}"
            );
        });
    }

    #[test]
    fn poll_once_blank_primary_takes_program_from_fallback() {
        // `"kiro": ""` opts out of the primary only: the running
        // kiro-cli (the fallback's program) still marks the pane live.
        run_with_fake_herdr(STAGED_SCRIPT, &[], |dir| {
            std::fs::write(
                dir.join("config.json"),
                r#"{"commands": {"kiro": "", "kiro-fallback": "kiro-cli chat -r {message}"}}"#,
            )
            .expect("config");
            let (config, _) = crate::config::load();
            stage_raw(
                dir,
                serde_json::json!({"pane_id": "w1:p1", "agent": "kiro", "agent_status": "working"}),
                serde_json::json!({"shell_pid": std::process::id(),
                    "foreground_processes": [{"argv": ["kiro-cli", "chat"], "name": "kiro-cli"}]}),
            );
            let mut st = MonitorState::default();
            assert!(poll_once("w1:p1", &config, &mut st));
            assert_eq!(
                crate::state::shell_record("w1:p1").map(|r| r.live),
                Some(true)
            );
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

    /// Stage one opencode poll under shell `shell`: herdr reports
    /// `session` (None = omitted), the foreground runs `argv` as process
    /// `pid` (None = herdr reports no pid). Returns the saved value.
    fn poll_opencode(
        dir: &std::path::Path,
        st: &mut MonitorState,
        shell: u32,
        session: Option<&str>,
        argv: &[&str],
        pid: Option<u32>,
    ) -> Option<String> {
        poll_agent(dir, st, "opencode", shell, session, argv, pid)
    }

    /// [`poll_opencode`] for an `agent` pane.
    fn poll_agent(
        dir: &std::path::Path,
        st: &mut MonitorState,
        agent: &str,
        shell: u32,
        session: Option<&str>,
        argv: &[&str],
        pid: Option<u32>,
    ) -> Option<String> {
        let mut pane = serde_json::json!({"pane_id": "wAA:p1",
            "agent": agent, "agent_status": "working"});
        if let Some(v) = session {
            pane["agent_session"] = serde_json::json!({"agent": agent, "value": v});
        }
        let mut entry = serde_json::json!({"argv": argv, "name": "x"});
        if let Some(pid) = pid {
            entry["pid"] = pid.into();
        }
        stage_raw(
            dir,
            pane,
            serde_json::json!({"shell_pid": shell, "foreground_processes": [entry]}),
        );
        assert!(poll_once("wAA:p1", &Config::default(), st));
        crate::state::load_registry()
            .get("wAA:p1")
            .map(|s| s.value.clone())
    }

    fn remember_opencode(value: &str) {
        crate::test_support::remember(
            "wAA:p1",
            SessionRef {
                agent: "opencode".into(),
                value: value.into(),
            },
        );
    }

    #[test]
    fn poll_once_hand_start_after_user_exit_is_evidence() {
        // Relaunched with `--session ses-x`, moved on to NEW, then quit
        // by the user: the retired record keeps the exited process, so a
        // hand-started `opencode --session NEW2` (a new pid) in the same
        // shell is seen appearing and outranks NEW.
        run_with_fake_herdr(STAGED_SCRIPT, &[], |dir| {
            let config = Config::default();
            let mut st = MonitorState::default();
            stage(
                dir,
                Some(std::os::unix::process::parent_id()),
                &["opencode"],
            );
            assert!(poll_once("wAA:p1", &config, &mut st)); // live, old host
            let new_shell = std::process::id();
            stage(dir, Some(new_shell), &["/usr/bin/zsh"]);
            assert!(poll_once("wAA:p1", &config, &mut st)); // relaunch ses-x
            assert_eq!(pane_runs(dir), 1, "calls: {}", herdr_calls(dir));
            let resumed = ["opencode", "--session", "ses-x"];
            let saved = poll_opencode(dir, &mut st, new_shell, Some("NEW"), &resumed, Some(100));
            assert_eq!(saved.as_deref(), Some("NEW"));
            st.cooldown_until = None;
            poll_opencode(dir, &mut st, new_shell, None, &["/usr/bin/zsh"], None); // user exit
            let retired = crate::state::shell_record("wAA:p1").expect("record");
            assert!(!retired.live);
            assert_eq!(
                retired.argv_seen.and_then(|s| s.pid),
                Some(100),
                "retired with the exited process"
            );
            let by_hand = ["opencode", "--session", "NEW2"];
            let saved = poll_opencode(dir, &mut st, new_shell, None, &by_hand, Some(200));
            assert_eq!(saved.as_deref(), Some("NEW2"));
            assert_eq!(pane_runs(dir), 1, "calls: {}", herdr_calls(dir));
        });
    }

    #[test]
    fn poll_once_flagged_start_after_bare_agent_is_evidence() {
        // A flagless agent process is still a sighting: after a bare
        // `opencode` (pid A, saved S1) quits, `opencode --session S2`
        // (pid B) in the same shell is a new process, so S2 wins. Same
        // for kiro's valueless `chat -r` then `--resume-id Q`.
        run_with_fake_herdr(STAGED_SCRIPT, &[], |dir| {
            let mut st = MonitorState::default();
            let shell = std::process::id();
            remember_opencode("S1");
            let saved = poll_opencode(dir, &mut st, shell, None, &["opencode"], Some(600));
            assert_eq!(saved.as_deref(), Some("S1"));
            assert_eq!(
                crate::state::shell_record("wAA:p1").and_then(|r| r.argv_seen),
                Some(SeenArgv {
                    agent: "opencode".into(),
                    value: None,
                    pid: Some(600),
                }),
                "bare process recorded"
            );
            poll_opencode(dir, &mut st, shell, None, &["/usr/bin/zsh"], None); // user exit
            let flagged = ["opencode", "--session", "S2"];
            let saved = poll_opencode(dir, &mut st, shell, None, &flagged, Some(601));
            assert_eq!(saved.as_deref(), Some("S2"));

            crate::test_support::remember(
                "wAA:p1",
                SessionRef {
                    agent: "kiro".into(),
                    value: "K1".into(),
                },
            );
            let mut kiro =
                |argv: &[&str], pid| poll_agent(dir, &mut st, "kiro", shell, None, argv, pid);
            // The agent change clears the opencode sighting; `chat -r`
            // is the first (flagless) kiro sighting.
            assert_eq!(
                kiro(&["kiro-cli", "chat", "-r"], Some(700)).as_deref(),
                Some("K1")
            );
            kiro(&["/usr/bin/zsh"], None); // user exit
            let resumed = ["kiro-cli", "chat", "--resume-id", "Q"];
            assert_eq!(kiro(&resumed, Some(701)).as_deref(), Some("Q"));
            assert_eq!(pane_runs(dir), 0);
        });
    }

    #[test]
    fn poll_once_hand_started_flag_is_evidence_only_once() {
        // Hand-started `opencode --session A` (no relaunch): herdr then
        // reports B and omits it — the unchanged A in argv of the same
        // process is stale, so B is kept; that process's flag changing to
        // C is new evidence. A pid-less entry is never evidence.
        run_with_fake_herdr(STAGED_SCRIPT, &[], |dir| {
            let mut st = MonitorState::default();
            let shell = std::process::id();
            let mut poll = |session: Option<&str>, flag: &str, pid: Option<u32>| {
                poll_opencode(
                    dir,
                    &mut st,
                    shell,
                    session,
                    &["opencode", "--session", flag],
                    pid,
                )
            };
            assert_eq!(poll(None, "A", Some(300)).as_deref(), Some("A"));
            assert_eq!(
                crate::state::shell_record("wAA:p1").and_then(|r| r.argv_seen),
                Some(SeenArgv {
                    agent: "opencode".into(),
                    value: Some("A".into()),
                    pid: Some(300),
                })
            );
            assert_eq!(poll(Some("B"), "A", Some(300)).as_deref(), Some("B"));
            assert_eq!(
                poll(None, "A", Some(300)).as_deref(),
                Some("B"),
                "seen A is stale"
            );
            assert_eq!(
                poll(None, "C", Some(300)).as_deref(),
                Some("C"),
                "changed flag wins"
            );
            assert_eq!(
                poll(None, "D", None).as_deref(),
                Some("C"),
                "no pid, no evidence"
            );
            assert_eq!(pane_runs(dir), 0);
        });
    }

    #[test]
    fn poll_once_same_process_without_flag_keeps_known_flag() {
        // OLD seen first (NEW kept), then the same process reported
        // without its flag, then OLD again: a re-read, never new.
        run_with_fake_herdr(STAGED_SCRIPT, &[], |dir| {
            let mut st = MonitorState::default();
            let shell = std::process::id();
            remember_opencode("NEW");
            let stale = ["opencode", "--session", "OLD"];
            for argv in [&stale[..], &["opencode"], &stale] {
                let saved = poll_opencode(dir, &mut st, shell, None, argv, Some(450));
                assert_eq!(saved.as_deref(), Some("NEW"), "argv {argv:?}");
            }
            assert_eq!(
                crate::state::shell_record("wAA:p1")
                    .and_then(|r| r.argv_seen)
                    .and_then(|s| s.value),
                Some("OLD".into())
            );
            assert_eq!(pane_runs(dir), 0);
        });
    }

    #[test]
    fn poll_once_first_sighting_keeps_newer_registry() {
        // A record from v0.6.0 (no `argv_seen`) for this very shell, or
        // a new shell after a host restart: a stale `--session OLD` seen
        // for the first time never overwrites the saved NEW.
        run_with_fake_herdr(STAGED_SCRIPT, &[], |dir| {
            let shell = std::process::id();
            let identity = shell_identity(shell).expect("own shell identity");
            std::fs::write(
                dir.join("shells.json"),
                serde_json::json!({"wAA:p1": {"shell": identity, "live": true}}).to_string(),
            )
            .expect("v0.6.0 record");
            remember_opencode("NEW");
            let mut st = MonitorState::default();
            let stale = ["opencode", "--session", "OLD"];
            let saved = poll_opencode(dir, &mut st, shell, None, &stale, Some(400));
            assert_eq!(saved.as_deref(), Some("NEW"), "upgrade");
            // A new shell (no record for it): the same stale flag again.
            let other = std::os::unix::process::parent_id();
            let saved = poll_opencode(dir, &mut st, other, None, &stale, Some(500));
            assert_eq!(saved.as_deref(), Some("NEW"), "new shell");
            assert_eq!(pane_runs(dir), 0);
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
        stage_claude_as(dir, cwd, shell, fg, "claude");
    }

    /// [`stage_claude`] with the pane labelled `agent`.
    fn stage_claude_as(
        dir: &std::path::Path,
        cwd: &std::path::Path,
        shell: u32,
        fg: serde_json::Value,
        agent: &str,
    ) {
        let get = serde_json::json!({"id": "cli:pane:get", "result": {"pane": {
            "pane_id": "w2A:p1", "agent": agent, "agent_status": "working",
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
        claude_restart_relaunch_as(profile, "claude", Config::default())
    }

    /// [`claude_restart_relaunch`] for a pane labelled `agent`.
    fn claude_restart_relaunch_as(profile: &str, agent: &str, config: Config) -> String {
        let xdg = crate::test_support::unique_temp_dir("xdg");
        let data = claude_profiles(&xdg);
        let xdg_s = xdg.to_string_lossy().into_owned();
        let mut line = String::new();
        run_with_fake_herdr(STAGED_SCRIPT, &[("XDG_DATA_HOME", Some(&xdg_s))], |dir| {
            let mut claude = fake_claude(&data.join(profile), "sess-p-1");
            let mut st = MonitorState::default();
            stage_claude_as(
                dir,
                &xdg,
                std::os::unix::process::parent_id(),
                serde_json::json!([{"name": "claude", "pid": claude.id()}]),
                agent,
            );
            assert!(poll_once("w2A:p1", &config, &mut st)); // live
            claude.kill().ok();
            claude.wait().ok();
            assert_eq!(
                crate::state::load_registry()
                    .get("w2A:p1")
                    .map(|s| s.value.clone()),
                Some("sess-p-1".to_string()),
                "session captured from {profile}/sessions/<pid>.json"
            );
            assert_eq!(
                crate::state::profile_for("w2A:p1").as_deref(),
                Some(profile)
            );
            stage_claude_as(dir, &xdg, std::process::id(), idle_zsh(), agent);
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
    fn poll_once_never_captures_a_claude_child_of_another_agent() {
        // A `claude -p` tool subprocess (under a managed profile, with a
        // session file) inside an opencode pane: opencode's saved session
        // is kept and no claude profile or session is captured.
        let xdg = crate::test_support::unique_temp_dir("xdg");
        let data = claude_profiles(&xdg);
        let xdg_s = xdg.to_string_lossy().into_owned();
        run_with_fake_herdr(STAGED_SCRIPT, &[("XDG_DATA_HOME", Some(&xdg_s))], |dir| {
            let saved = SessionRef {
                agent: "opencode".into(),
                value: "ses-o".into(),
            };
            crate::test_support::remember("w2A:p1", saved.clone());
            let mut claude = fake_claude(&data.join("personal"), "sess-child");
            let get = serde_json::json!({"id": "cli:pane:get", "result": {"pane": {
                "pane_id": "w2A:p1", "agent": "opencode", "agent_status": "working",
                "cwd": xdg}}});
            std::fs::write(dir.join("get.json"), get.to_string()).expect("stage get");
            let fg = serde_json::json!([
                {"argv": ["opencode"], "name": "opencode"},
                {"name": "claude", "pid": claude.id()}
            ]);
            let info = serde_json::json!({"id": "cli:pane:process-info", "result": {
                "process_info": {"shell_pid": std::os::unix::process::parent_id(),
                "foreground_processes": fg}}});
            std::fs::write(dir.join("info.json"), info.to_string()).expect("stage info");
            let mut st = MonitorState::default();
            assert!(poll_once("w2A:p1", &Config::default(), &mut st));
            claude.kill().ok();
            claude.wait().ok();
            assert_eq!(crate::state::load_registry().get("w2A:p1"), Some(&saved));
            assert_eq!(crate::state::profile_for("w2A:p1"), None);
        });
        std::fs::remove_dir_all(&xdg).ok();
    }

    #[test]
    fn poll_once_resumes_non_default_claude_profile() {
        assert_eq!(
            claude_restart_relaunch("personal"),
            "4:pane run w2A:p1 claude --profile personal --resume sess-p-1"
        );
    }

    /// An aliased key whose template runs claude (`claude-work`) is
    /// captured and resumed under its profile like `claude`.
    #[test]
    fn poll_once_resumes_aliased_claude_key_under_profile() {
        let mut config = Config::default();
        config
            .commands
            .insert("claude-work".into(), "claude --resume {value}".into());
        assert_eq!(
            claude_restart_relaunch_as("personal", "claude-work", config),
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
            crate::test_support::remember(
                "w2A:p1",
                SessionRef {
                    agent: "claude".into(),
                    value: "sess-d-1".into(),
                },
            );
            seed_host_restart("w2A:p1");
            stage_claude(dir, &proj, std::process::id(), idle_zsh());
            assert!(poll_once(
                "w2A:p1",
                &Config::default(),
                &mut MonitorState::default()
            ));
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
            crate::test_support::record_shell("wAA:p1", rec("prior-host", true));
            stage(dir, None, &["/usr/bin/zsh"]);
            assert!(poll_once("wAA:p1", &config, &mut st)); // no shell pid
            assert_eq!(pane_runs(dir), 0, "calls: {}", herdr_calls(dir));
        });
    }

    #[test]
    fn foreground_looks_live_rejects_shells_and_garbage() {
        assert!(foreground_looks_live(&procs(&[vec!["opencode".into()]])));
        assert!(foreground_looks_live(&procs(&[vec![
            "kiro-cli".into(),
            "chat".into(),
            "--resume-id".into(),
            "s".into()
        ]])));
        assert!(!foreground_looks_live(&procs(&[])));
        assert!(!foreground_looks_live(&procs(&[vec![
            "/usr/bin/zsh".into()
        ]])));
        assert!(!foreground_looks_live(&procs(&[vec![
            "/bin/sh".into(),
            "job.sh".into()
        ]])));
        assert!(!foreground_looks_live(&procs(&[vec!["-fish".into()]])));
        assert!(!foreground_looks_live(&procs(&[vec![
            "<unparseable>".into()
        ]])));
        assert!(!foreground_looks_live(&procs(&[Vec::<String>::new()])));
        assert!(!foreground_looks_live(&procs(&[
            vec!["opencode".into()],
            vec!["/usr/bin/zsh".into()],
        ])));
    }

    #[test]
    fn poll_once_continue_sent_three_polls_after_relaunch() {
        // Opencode's template carries no `{message}`, so the configured
        // message goes two-step: relaunch, then submit once the TUI had
        // a few polls to boot (live foreground required).
        let script = "#!/bin/sh\necho \"$#:$@\" >> \"@CALL_LOG@\"\nD=\"$(dirname \"@CALL_LOG@\")\"\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"wAA:p1\",\"agent\":\"opencode\",\"agent_status\":\"unknown\"}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\nif [ -f \"$D/booted\" ]; then\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"opencode\"]}]}}}'\nelse\ntouch \"$D/booted\"\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"/usr/bin/zsh\"]}]}}}'\nfi\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"run\" ]; then\necho '{\"id\":\"cli:pane:run\",\"result\":{\"ok\":true}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"send-text\" ]; then\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"send-keys\" ]; then\nexit 0\nfi\nexit 1\n";
        run_with_fake_herdr(&with_shell_pid(script), &[], |state_dir| {
            crate::test_support::remember(
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
                calls
                    .lines()
                    .filter(|l| l.contains("pane send-text"))
                    .count(),
                1,
                "continue submitted exactly once, got: {calls}"
            );
            assert_eq!(
                calls
                    .lines()
                    .filter(|l| l.contains("pane send-keys"))
                    .count(),
                1,
                "Enter submitted exactly once, got: {calls}"
            );
            assert!(
                calls.contains("pane send-text wAA:p1 continue"),
                "message text delivered verbatim, got: {calls}"
            );
            let log = std::fs::read_to_string(state_dir.join("log.txt")).unwrap_or_default();
            assert!(
                log.contains("submitted continue to opencode"),
                "log names submit: {log}"
            );
        });
    }

    #[test]
    fn poll_once_continue_skipped_when_idle() {
        // The agent died (or never booted) before the countdown lapsed:
        // chat text must never land in a shell prompt — wait silently.
        let script = "#!/bin/sh\necho \"$#:$@\" >> \"@CALL_LOG@\"\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"get\" ]; then\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"wAA:p1\",\"agent\":\"opencode\",\"agent_status\":\"unknown\"}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"process-info\" ]; then\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"foreground_processes\":[{\"argv\":[\"/usr/bin/zsh\"]}]}}}'\nexit 0\nfi\nif [ \"$1\" = \"pane\" ] && [ \"$2\" = \"run\" ]; then\necho '{\"id\":\"cli:pane:run\",\"result\":{\"ok\":true}}'\nexit 0\nfi\nexit 1\n";
        run_with_fake_herdr(&with_shell_pid(script), &[], |state_dir| {
            crate::test_support::remember(
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
            crate::test_support::remember(
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
                crate::test_support::remember(
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
            crate::test_support::remember(
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
            crate::test_support::remember(
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
            assert!(
                st.cooldown_until.is_some(),
                "successful relaunch sets cooldown"
            );
            let log = std::fs::read_to_string(state_dir.join("log.txt")).expect("log exists");
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
            crate::test_support::remember(
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
            assert!(
                calls.contains("pane get"),
                "fake must have seen pane get: {calls}"
            );
            assert!(
                calls.contains("pane process-info"),
                "fake must have seen process-info: {calls}"
            );
            assert!(
                !calls.contains("pane run"),
                "no pane run may be invoked: {calls}"
            );
            let log = std::fs::read_to_string(state_dir.join("log.txt")).expect("log exists");
            assert_eq!(
                log.lines()
                    .filter(|l| l.contains("vetoing relaunch"))
                    .count(),
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
                format!(
                    "#!/bin/sh\ncat \"{}\"\n",
                    state_dir.join("list.json").display()
                ),
            )
            .expect("fake kiro");
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).expect("chmod");
            let _restore = crate::test_support::EnvRestore::capture(["KIRO_BIN_PATH"]);
            std::env::set_var("KIRO_BIN_PATH", &fake);
            let config = Config::default();
            let mut st = MonitorState::default();
            assert!(poll_once("w9M:p1", &config, &mut st));
            let reg = crate::state::load_registry();
            assert_eq!(
                reg.get("w9M:p1")
                    .map(|s| (s.agent.as_str(), s.value.as_str())),
                Some(("kiro", "sess-pinned-1"))
            );
        });
    }

    fn log_text(dir: &std::path::Path) -> String {
        std::fs::read_to_string(dir.join("log.txt")).unwrap_or_default()
    }

    #[test]
    fn poll_once_name_only_claude_is_not_a_proc_info_failure() {
        run_with_fake_herdr(STAGED_SCRIPT, &[], |dir| {
            stage_raw(
                dir,
                serde_json::json!({"pane_id": "w1:p1", "agent": "claude",
                    "agent_status": "working",
                    "agent_session": {"agent": "claude", "value": "live-1"}}),
                serde_json::json!({"foreground_processes": [
                    {"cwd": "/", "name": "claude"},
                    {"argv": ["agentmail", "mcp"], "name": "agentmail"}
                ]}),
            );
            let config = Config::default();
            let mut st = MonitorState::default();
            assert!(poll_once("w1:p1", &config, &mut st));
            assert!(poll_once("w1:p1", &config, &mut st));
            let log = log_text(dir);
            assert!(!log.contains("unparseable"), "{log}");
        });
    }

    #[test]
    fn poll_once_refresh_does_not_rearm_proc_info_warning() {
        // Unreadable foreground + live agent_session: every poll is a
        // Refresh, which must not reset the warn-once slot.
        run_with_fake_herdr(STAGED_SCRIPT, &[], |dir| {
            stage_raw(
                dir,
                serde_json::json!({"pane_id": "w1:p1", "agent": "claude",
                    "agent_status": "working",
                    "agent_session": {"agent": "claude", "value": "live-1"}}),
                serde_json::json!({"foreground_processes": [{"foo": 1}]}),
            );
            let config = Config::default();
            let mut st = MonitorState::default();
            for _ in 0..3 {
                assert!(poll_once("w1:p1", &config, &mut st));
            }
            let log = log_text(dir);
            assert_eq!(
                log.lines().filter(|l| l.contains("unparseable")).count(),
                1,
                "{log}"
            );
        });
    }

    /// Relaunch `agent` (saved session `value`) after a host restart with
    /// `resume_message` set, then let `live_fg` take the foreground for
    /// `polls` polls; returns the herdr calls.
    fn continue_after_relaunch(
        agent: &str,
        value: &str,
        live_fg: serde_json::Value,
        polls: usize,
    ) -> (String, String) {
        continue_after_relaunch_info(
            agent,
            value,
            serde_json::json!({"shell_pid": std::process::id(),
                "foreground_processes": live_fg}),
            polls,
        )
    }

    /// [`continue_after_relaunch`] with the whole live `process_info`.
    fn continue_after_relaunch_info(
        agent: &str,
        value: &str,
        live_info: serde_json::Value,
        polls: usize,
    ) -> (String, String) {
        let mut out = (String::new(), String::new());
        run_with_fake_herdr(STAGED_SCRIPT, &[], |dir| {
            let pane = serde_json::json!({"pane_id": "wC:p1", "agent": agent,
                "agent_status": "unknown"});
            let shell = std::process::id();
            crate::test_support::remember(
                "wC:p1",
                SessionRef {
                    agent: agent.into(),
                    value: value.into(),
                },
            );
            seed_host_restart("wC:p1");
            stage_raw(
                dir,
                pane.clone(),
                serde_json::json!({"shell_pid": shell,
                "foreground_processes": [{"argv": ["/usr/bin/zsh"], "name": "zsh"}]}),
            );
            let config = Config {
                resume_message: Some("continue".into()),
                ..Config::default()
            };
            let mut st = MonitorState::default();
            assert!(poll_once("wC:p1", &config, &mut st)); // relaunch
            assert_eq!(pane_runs(dir), 1, "calls: {}", herdr_calls(dir));
            stage_raw(dir, pane, live_info);
            for _ in 0..polls {
                assert!(poll_once("wC:p1", &config, &mut st));
            }
            out = (herdr_calls(dir), log_text(dir));
        });
        out
    }

    #[test]
    fn poll_once_continue_reaches_name_only_claude() {
        let (calls, _) = continue_after_relaunch(
            "claude",
            "sess-c",
            serde_json::json!([{"cwd": "/", "name": "claude"}]),
            3,
        );
        assert!(calls.contains("pane send-text wC:p1 continue"), "{calls}");
    }

    #[test]
    fn poll_once_continue_reaches_node_launched_codex() {
        let (calls, _) = continue_after_relaunch(
            "codex",
            "sess-x",
            serde_json::json!([{"argv": ["node",
                "/usr/lib/node_modules/@openai/codex/bin/codex.js", "resume", "sess-x"],
                "name": "node"}]),
            3,
        );
        assert!(calls.contains("pane send-text wC:p1 continue"), "{calls}");
    }

    #[test]
    fn poll_once_continue_reaches_bare_argv_array_agent() {
        let (calls, _) = continue_after_relaunch(
            "opencode",
            "ses-x",
            serde_json::json!([["opencode", "--session", "ses-x"]]),
            3,
        );
        assert!(calls.contains("pane send-text wC:p1 continue"), "{calls}");
    }

    #[test]
    fn poll_once_continue_reaches_top_level_argv_agent() {
        let (calls, _) = continue_after_relaunch_info(
            "opencode",
            "ses-x",
            serde_json::json!({"shell_pid": std::process::id(),
                "argv": ["opencode", "--session", "ses-x"]}),
            3,
        );
        assert!(calls.contains("pane send-text wC:p1 continue"), "{calls}");
    }

    #[test]
    fn poll_once_undeliverable_continue_is_dropped_with_one_log() {
        // The relaunched opencode never takes the foreground (vim does):
        // the message is never sent, and its expiry is logged once.
        let (calls, log) = continue_after_relaunch(
            "opencode",
            "ses-x",
            serde_json::json!([{"argv": ["vim", "opencode.json"], "name": "vim"}]),
            usize::from(CONTINUE_AFTER_POLLS + CONTINUE_GIVE_UP_POLLS) + 3,
        );
        assert!(!calls.contains("send-text"), "{calls}");
        assert_eq!(
            log.lines()
                .filter(|l| l.contains("dropped pending continue for opencode"))
                .count(),
            1,
            "{log}"
        );
    }

    /// A fake `kiro-cli` that logs each call and replies with `body`
    /// (exit `code`); returns (its dir, its path).
    fn fake_kiro(body: &str, code: i32) -> (std::path::PathBuf, String) {
        let dir = crate::test_support::unique_temp_dir("kiro-fake");
        let bin = dir.join("kiro-cli");
        std::fs::write(dir.join("list.json"), body).expect("fixture");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\necho \"$@\" >> \"{}\"\ncat \"{}\"\nexit {code}\n",
                dir.join("kiro-calls.log").display(),
                dir.join("list.json").display()
            ),
        )
        .expect("fake kiro");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let path = bin.to_string_lossy().into_owned();
        (dir, path)
    }

    fn kiro_calls(dir: &std::path::Path) -> usize {
        std::fs::read_to_string(dir.join("kiro-calls.log"))
            .unwrap_or_default()
            .lines()
            .count()
    }

    #[test]
    fn poll_once_never_pins_kiro_on_dead_pane() {
        // Stale kiro label over an idle shell: no `chat -l` spawn.
        let (kdir, kbin) = fake_kiro("[]", 0);
        run_with_fake_herdr(STAGED_SCRIPT, &[("KIRO_BIN_PATH", Some(&kbin))], |dir| {
            stage_raw(
                dir,
                serde_json::json!({"pane_id": "w9M:p1", "agent": "kiro",
                    "agent_status": "idle", "cwd": "/"}),
                serde_json::json!({"foreground_processes": [{"argv": ["/usr/bin/zsh"]}]}),
            );
            assert!(poll_once(
                "w9M:p1",
                &Config::default(),
                &mut MonitorState::default()
            ));
        });
        assert_eq!(kiro_calls(&kdir), 0);
        std::fs::remove_dir_all(&kdir).ok();
    }

    #[test]
    fn poll_once_backs_off_after_failed_kiro_lookup() {
        let (kdir, kbin) = fake_kiro("", 1);
        run_with_fake_herdr(STAGED_SCRIPT, &[("KIRO_BIN_PATH", Some(&kbin))], |dir| {
            stage_raw(
                dir,
                serde_json::json!({"pane_id": "w9M:p1", "agent": "kiro",
                    "agent_status": "working", "cwd": "/"}),
                serde_json::json!({"foreground_processes": [{"argv": ["kiro-cli", "--resume"]}]}),
            );
            let config = Config::default();
            let mut st = MonitorState::default();
            assert!(poll_once("w9M:p1", &config, &mut st));
            assert_eq!(kiro_calls(&kdir), 1);
            // The failed poll is poll 0: polls 1..KIRO_RETRY_POLLS-1 skip.
            for _ in 1..KIRO_RETRY_POLLS {
                assert!(poll_once("w9M:p1", &config, &mut st));
            }
            assert_eq!(kiro_calls(&kdir), 1, "no retry inside the backoff");
            assert!(poll_once("w9M:p1", &config, &mut st));
            assert_eq!(
                kiro_calls(&kdir),
                2,
                "retried on poll {KIRO_RETRY_POLLS} after the failure"
            );
            for _ in 1..KIRO_RETRY_POLLS {
                assert!(poll_once("w9M:p1", &config, &mut st));
            }
            assert_eq!(kiro_calls(&kdir), 2);
            assert!(poll_once("w9M:p1", &config, &mut st));
            assert_eq!(kiro_calls(&kdir), 3, "every {KIRO_RETRY_POLLS} polls");
        });
        std::fs::remove_dir_all(&kdir).ok();
    }

    #[test]
    fn poll_once_backs_off_when_kiro_pin_cannot_persist() {
        let list = r#"[{"cwd":"/","sessions":[
            {"sessionId":"sess-1","updatedAt":"2026-09-15T22:32:18.685Z","messageCount":4}
        ]}]"#;
        let (kdir, kbin) = fake_kiro(list, 0);
        run_with_fake_herdr(STAGED_SCRIPT, &[("KIRO_BIN_PATH", Some(&kbin))], |dir| {
            stage_raw(
                dir,
                serde_json::json!({"pane_id": "w9M:p1", "agent": "kiro",
                    "agent_status": "working", "cwd": "/"}),
                serde_json::json!({"foreground_processes": [{"argv": ["kiro-cli", "--resume"]}]}),
            );
            // A non-empty directory at the registry path: every registry
            // write fails, so the found id is never persisted.
            std::fs::create_dir_all(dir.join("registry.json").join("blocker"))
                .expect("registry blocker");
            let config = Config::default();
            let mut st = MonitorState::default();
            assert!(poll_once("w9M:p1", &config, &mut st));
            assert_eq!(kiro_calls(&kdir), 1);
            for _ in 1..KIRO_RETRY_POLLS {
                assert!(poll_once("w9M:p1", &config, &mut st));
            }
            assert_eq!(kiro_calls(&kdir), 1, "no respawn inside the backoff");
            assert!(poll_once("w9M:p1", &config, &mut st));
            assert_eq!(kiro_calls(&kdir), 2, "retried on poll {KIRO_RETRY_POLLS}");
        });
        std::fs::remove_dir_all(&kdir).ok();
    }

    #[test]
    fn poll_once_kiro_argv_session_skips_lookup() {
        // `--resume-id X` already names the session: no `chat -l`
        // spawn, and the Refresh arm remembers kiro's own argv value.
        let (kdir, kbin) = fake_kiro("[]", 0);
        run_with_fake_herdr(STAGED_SCRIPT, &[("KIRO_BIN_PATH", Some(&kbin))], |dir| {
            stage_raw(
                dir,
                serde_json::json!({"pane_id": "w9M:p1", "agent": "kiro",
                    "agent_status": "working", "cwd": "/"}),
                serde_json::json!({"foreground_processes": [
                    {"argv": ["kiro-cli", "chat", "--resume-id", "sess-argv-7"]}
                ]}),
            );
            assert!(poll_once(
                "w9M:p1",
                &Config::default(),
                &mut MonitorState::default()
            ));
            let reg = crate::state::load_registry();
            assert_eq!(
                reg.get("w9M:p1")
                    .map(|s| (s.agent.as_str(), s.value.as_str())),
                Some(("kiro", "sess-argv-7"))
            );
        });
        assert_eq!(kiro_calls(&kdir), 0);
        std::fs::remove_dir_all(&kdir).ok();
    }

    const KIRO_LIST: &str = r#"[{"cwd":"/","sessions":[
        {"sessionId":"sess-pinned-1","source":"v2","title":"live","updatedAt":"2026-09-15T22:32:18.685Z","messageCount":44}
    ]}]"#;

    /// One poll of live kiro pane `w9M:p1` with `info` as its
    /// process-info, after `seed` ran; returns the registry entry and
    /// the `chat -l` spawn count.
    fn kiro_poll(
        body: &str,
        code: i32,
        seed: impl FnOnce(),
        info: serde_json::Value,
    ) -> (Option<SessionRef>, usize) {
        kiro_poll_as("kiro", &Config::default(), body, code, seed, info)
    }

    /// [`kiro_poll`] for a pane labelled `agent` under `config`.
    fn kiro_poll_as(
        agent: &str,
        config: &Config,
        body: &str,
        code: i32,
        seed: impl FnOnce(),
        info: serde_json::Value,
    ) -> (Option<SessionRef>, usize) {
        let (kdir, kbin) = fake_kiro(body, code);
        let mut entry = None;
        run_with_fake_herdr(STAGED_SCRIPT, &[("KIRO_BIN_PATH", Some(&kbin))], |dir| {
            seed();
            stage_raw(
                dir,
                serde_json::json!({"pane_id": "w9M:p1", "agent": agent,
                    "agent_status": "working", "cwd": "/"}),
                info,
            );
            assert!(poll_once("w9M:p1", config, &mut MonitorState::default()));
            entry = crate::state::load_registry().remove("w9M:p1");
        });
        let calls = kiro_calls(&kdir);
        std::fs::remove_dir_all(&kdir).ok();
        (entry, calls)
    }

    #[test]
    fn poll_once_kiro_bare_resume_token_is_no_session() {
        // `kiro-cli chat --resume continue`: `continue` is a chat
        // message, never a session id — nothing is remembered from argv.
        let info = || {
            serde_json::json!({"foreground_processes": [
                {"argv": ["kiro-cli", "chat", "--resume", "continue"]}
            ]})
        };
        let (entry, calls) = kiro_poll("", 1, || {}, info());
        assert_eq!(entry, None);
        assert_eq!(calls, 1, "the pin path runs instead");
        // With a listed session the pin lands instead.
        let (entry, _) = kiro_poll(KIRO_LIST, 0, || {}, info());
        assert_eq!(entry.map(|s| s.value), Some("sess-pinned-1".into()));
        // `--resume-id X` is still remembered directly.
        let (entry, calls) = kiro_poll(
            KIRO_LIST,
            0,
            || {},
            serde_json::json!({"foreground_processes": [
                {"argv": ["kiro-cli", "chat", "--resume-id", "sess-argv-7"]}
            ]}),
        );
        assert_eq!(entry.map(|s| s.value), Some("sess-argv-7".into()));
        assert_eq!(calls, 0);
    }

    #[test]
    fn poll_once_pins_aliased_kiro_cli_agent() {
        // Any key whose template runs kiro-cli is pinned, under that key.
        let mut config = Config::default();
        config.commands.insert(
            "kiro-work".into(),
            "kiro-cli chat --resume-id {value}".into(),
        );
        let info =
            || serde_json::json!({"foreground_processes": [{"argv": ["kiro-cli", "--resume"]}]});
        let (entry, calls) = kiro_poll_as("kiro-work", &config, KIRO_LIST, 0, || {}, info());
        assert_eq!(calls, 1);
        assert_eq!(
            entry.map(|s| (s.agent, s.value)),
            Some(("kiro-work".into(), "sess-pinned-1".into()))
        );
        // An opted-out kiro (blank template) never spawns `chat -l`.
        config.commands.insert("kiro".into(), String::new());
        let (entry, calls) = kiro_poll_as("kiro", &config, KIRO_LIST, 0, || {}, info());
        assert_eq!((entry, calls), (None, 0));
        // Even where the blank template's fallback program (the agent
        // name) is `kiro-cli` itself.
        config.commands.insert("kiro-cli".into(), String::new());
        let (entry, calls) = kiro_poll_as("kiro-cli", &config, KIRO_LIST, 0, || {}, info());
        assert_eq!((entry, calls), (None, 0));
        // A valueless primary could never use a pin: no lookup either.
        config
            .commands
            .insert("kiro-work".into(), "kiro-cli chat -r".into());
        let (entry, calls) = kiro_poll_as("kiro-work", &config, KIRO_LIST, 0, || {}, info());
        assert_eq!((entry, calls), (None, 0));
    }

    #[test]
    fn poll_once_kiro_pin_overwrites_foreign_registry_ref() {
        // A stale claude ref for the pane (resolve rejects it) must not
        // block the kiro pin.
        let (entry, calls) = kiro_poll(
            KIRO_LIST,
            0,
            || {
                crate::test_support::remember(
                    "w9M:p1",
                    SessionRef {
                        agent: "claude".into(),
                        value: "old-claude".into(),
                    },
                );
            },
            serde_json::json!({"foreground_processes": [{"argv": ["kiro-cli", "--resume"]}]}),
        );
        assert_eq!(calls, 1);
        assert_eq!(
            entry.map(|s| (s.agent, s.value)),
            Some(("kiro".into(), "sess-pinned-1".into()))
        );
        // An existing kiro ref is left alone: no lookup.
        let (entry, calls) = kiro_poll(
            KIRO_LIST,
            0,
            || {
                crate::test_support::remember(
                    "w9M:p1",
                    SessionRef {
                        agent: "kiro".into(),
                        value: "sess-kept".into(),
                    },
                );
            },
            serde_json::json!({"foreground_processes": [{"argv": ["kiro-cli", "--resume"]}]}),
        );
        assert_eq!(calls, 0);
        assert_eq!(entry.map(|s| s.value), Some("sess-kept".into()));
    }

    #[test]
    fn poll_once_pins_kiro_with_bare_argv_array_shape() {
        let (entry, calls) = kiro_poll(
            KIRO_LIST,
            0,
            || {},
            serde_json::json!({"foreground_processes": [["kiro-cli", "chat", "--resume"]]}),
        );
        assert_eq!(calls, 1);
        assert_eq!(entry.map(|s| s.value), Some("sess-pinned-1".into()));
    }

    #[test]
    fn poll_once_kiro_child_resume_id_is_not_kiros() {
        // A child process's `--resume-id X` is not kiro's own: the pin
        // lookup still runs and X is never remembered.
        let (entry, calls) = kiro_poll(
            KIRO_LIST,
            0,
            || {},
            serde_json::json!({"foreground_processes": [
                {"argv": ["kiro-cli", "chat", "--resume"]},
                {"argv": ["node", "/opt/mcp.js", "--resume-id", "child-x"]}
            ]}),
        );
        assert_eq!(calls, 1);
        assert_eq!(entry.map(|s| s.value), Some("sess-pinned-1".into()));
    }

    #[test]
    fn poll_once_pins_kiro_with_top_level_argv_shape() {
        let (entry, calls) = kiro_poll(
            KIRO_LIST,
            0,
            || {},
            serde_json::json!({"argv": ["kiro-cli", "chat", "--resume"]}),
        );
        assert_eq!(calls, 1);
        assert_eq!(entry.map(|s| s.value), Some("sess-pinned-1".into()));
    }

    #[test]
    fn run_exits_on_stop_sentinel_and_clears_lock() {
        let own = std::process::id();
        let mut lock_gone = false;
        run_with_fake_herdr("#!/bin/sh\nexit 1\n", &[], |dir| {
            std::fs::write(dir.join("config.json"), r#"{"connect_grace_seconds":0}"#)
                .expect("config");
            std::fs::write(crate::state::stop_sentinel_path(own), b"stop").expect("sentinel");
            run_with_sleep("w1:p1", |_| {});
            lock_gone = !crate::state::monitor_lock_path("w1:p1").exists();
            assert!(
                !crate::state::stop_sentinel_path(own).exists(),
                "sentinel cleared"
            );
            let log = log_text(dir);
            assert!(
                !log.contains("pane fetch failed"),
                "stopped before polling: {log}"
            );
        });
        assert!(lock_gone, "lock cleared on exit");
    }

    #[test]
    fn run_exits_after_ten_pane_misses_and_clears_lock() {
        let mut lock_gone = false;
        run_with_fake_herdr("#!/bin/sh\nexit 1\n", &[], |dir| {
            std::fs::write(dir.join("config.json"), r#"{"connect_grace_seconds":0}"#)
                .expect("config");
            run_with_sleep("w1:p1", |_| {});
            lock_gone = !crate::state::monitor_lock_path("w1:p1").exists();
            let log = log_text(dir);
            assert_eq!(
                log.lines()
                    .filter(|l| l.contains("pane fetch failed"))
                    .count(),
                10,
                "{log}"
            );
            assert!(log.contains("pane gone 10x, exiting"), "{log}");
        });
        assert!(lock_gone, "lock cleared on exit");
    }

    #[cfg(unix)]
    #[test]
    fn run_exits_without_clearing_a_lock_it_lost() {
        // Another live monitor claimed the lock mid-run: exit, leaving
        // its lock in place.
        let mut rival = crate::test_support::spawn_fake_monitor("w1:p1");
        let rival_pid = rival.id();
        run_with_fake_herdr("#!/bin/sh\nexit 1\n", &[], |dir| {
            std::fs::write(dir.join("config.json"), r#"{"connect_grace_seconds":0}"#)
                .expect("config");
            run_with_sleep("w1:p1", |_| {
                std::fs::remove_file(crate::state::monitor_lock_path("w1:p1")).ok();
                assert!(crate::state::write_monitor_lock_pid("w1:p1", rival_pid));
            });
            assert!(
                crate::test_support::monitor_lock_owned_by("w1:p1", rival_pid),
                "rival's lock kept"
            );
            let log = log_text(dir);
            assert!(log.contains("lock no longer ours"), "{log}");
            assert_eq!(
                log.lines()
                    .filter(|l| l.contains("pane fetch failed"))
                    .count(),
                1,
                "{log}"
            );
        });
        rival.kill().ok();
        rival.wait().ok();
    }

    #[test]
    fn run_reclaims_a_lock_deleted_or_left_stale_under_it() {
        // The lock vanishes (or names a dead pid) mid-run: the monitor
        // re-claims it and keeps polling until its exit path, which then
        // releases its own lock.
        let own = std::process::id();
        for stale in [None, Some(2147483646u32)] {
            run_with_fake_herdr("#!/bin/sh\nexit 1\n", &[], |dir| {
                std::fs::write(dir.join("config.json"), r#"{"connect_grace_seconds":2}"#)
                    .expect("config");
                let reclaimed = std::cell::Cell::new(0u32);
                run_with_sleep("w1:p1", |_| {
                    let path = crate::state::monitor_lock_path("w1:p1");
                    if crate::test_support::monitor_lock_owned_by("w1:p1", own) {
                        reclaimed.set(reclaimed.get() + 1);
                    }
                    std::fs::remove_file(&path).ok();
                    if let Some(pid) = stale {
                        assert!(crate::state::write_monitor_lock_pid("w1:p1", pid));
                    }
                });
                let log = log_text(dir);
                assert!(!log.contains("lock no longer ours"), "{log}");
                assert!(
                    log.contains("pane gone 10x, exiting"),
                    "kept polling: {log}"
                );
                // Every sleep found the lock ours again: 2 grace slices
                // plus the 9 sleeps between the 10 polls.
                assert_eq!(reclaimed.get(), 11, "{log}");
                assert!(
                    !crate::state::monitor_lock_path("w1:p1").exists(),
                    "own (re-claimed) lock released on exit"
                );
            });
        }
    }

    #[test]
    fn run_keeps_running_when_a_reclaim_write_fails() {
        // The lock vanishes and the monitors dir becomes unwritable (a
        // file in its place): the failed re-claim is logged once and the
        // monitor keeps polling instead of retiring.
        run_with_fake_herdr("#!/bin/sh\nexit 1\n", &[], |dir| {
            std::fs::write(dir.join("config.json"), r#"{"connect_grace_seconds":1}"#)
                .expect("config");
            let monitors = crate::state::monitor_lock_path("w1:p1")
                .parent()
                .expect("monitors dir")
                .to_path_buf();
            let blocked = std::cell::Cell::new(false);
            run_with_sleep("w1:p1", |_| {
                if !blocked.replace(true) {
                    std::fs::remove_dir_all(&monitors).expect("rm monitors");
                    std::fs::write(&monitors, "not a dir").expect("block monitors");
                }
            });
            std::fs::remove_file(&monitors).ok();
            let log = log_text(dir);
            assert!(!log.contains("lock no longer ours"), "{log}");
            assert_eq!(
                log.lines()
                    .filter(|l| l.contains("re-claim failed"))
                    .count(),
                1,
                "{log}"
            );
            assert!(
                log.contains("pane gone 10x, exiting"),
                "kept polling: {log}"
            );
        });
    }

    #[test]
    fn finish_monitor_leaves_a_lock_owned_by_another_pid() {
        run_with_fake_herdr("#!/bin/sh\nexit 1\n", &[], |_| {
            let pane_id = "w1:p1";
            let own_pid = 424243u32;
            assert!(crate::state::write_monitor_lock_pid(pane_id, 424244));
            std::fs::write(crate::state::stop_sentinel_path(own_pid), b"stop").expect("sentinel");
            finish_monitor(pane_id, own_pid);
            assert!(
                crate::test_support::monitor_lock_owned_by(pane_id, 424244),
                "successor's lock kept"
            );
            assert!(
                !crate::state::stop_sentinel_path(own_pid).exists(),
                "own sentinel still cleared"
            );
        });
    }

    #[test]
    fn finish_monitor_clears_lock_and_sentinel() {
        run_with_fake_herdr("#!/bin/sh\nexit 1\n", &[], |_| {
            let pane_id = "w1:p1";
            let own_pid = 424243u32;
            assert!(crate::state::write_monitor_lock_pid(pane_id, own_pid));
            std::fs::write(crate::state::stop_sentinel_path(own_pid), b"stop").expect("sentinel");
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
