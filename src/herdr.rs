use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionRef {
    pub agent: String,
    #[serde(alias = "session_id", alias = "sessionId", alias = "id")]
    pub value: String,
}

fn lenient_session<'de, D>(d: D) -> Result<Option<SessionRef>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v: Option<Value> = Option::deserialize(d)?;
    match v {
        None => Ok(None),
        Some(v) if v.is_null() => Ok(None),
        Some(v) => match serde_json::from_value::<SessionRef>(v) {
            Ok(s) => Ok(Some(s)),
            Err(e) => {
                eprintln!("herdr: skipping unparsable agent_session: {e}");
                Ok(None)
            }
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct Pane {
    #[serde(default)]
    pub pane_id: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub agent_status: Option<String>,
    #[serde(default, deserialize_with = "lenient_session")]
    pub agent_session: Option<SessionRef>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
}

fn panes_from_result(result: &Value) -> Vec<Pane> {
    let Some(arr) = result.get("panes").and_then(|p| p.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(arr.len());
    for p in arr {
        match serde_json::from_value::<Pane>(p.clone()) {
            Ok(pane) => out.push(pane),
            Err(e) => eprintln!("herdr: skipping unparsable pane: {e}"),
        }
    }
    out
}

#[cfg(test)]
fn parse_panes(raw: &str) -> Vec<Pane> {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|v| v.get("result").cloned())
        .map(|r| panes_from_result(&r))
        .unwrap_or_default()
}

/// Run `herdr <args...>` with a 10s timeout, returning the raw output.
/// On timeout the child is killed and Err("timeout") is returned (fail-closed).
///
/// Pipes are drained concurrently by reader threads so a child that fills
/// stdout/stderr cannot deadlock against the `try_wait` poll loop.
fn run_raw(args: &[&str]) -> Result<std::process::Output, String> {
    let bin = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string());
    let mut child = std::process::Command::new(&bin)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            eprintln!("herdr: spawn failed: {} {}: {e}", bin, args.join(" "));
            format!("spawn: {e}")
        })?;
    // Take the pipes so reader threads own them; the child can keep writing
    // while we poll try_wait below.
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stdout_pipe {
            use std::io::Read;
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stderr_pipe {
            use std::io::Read;
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Child has exited, but a grandchild may still inherit the
                // pipe write ends (same shape the timeout path guards
                // against), so bound the reader join: collect what arrived
                // within grace, detach the rest.
                let grace = std::time::Instant::now() + std::time::Duration::from_secs(2);
                while !(stdout_reader.is_finished() && stderr_reader.is_finished()) {
                    if std::time::Instant::now() >= grace {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                let stdout = stdout_reader
                    .is_finished()
                    .then(|| stdout_reader.join().unwrap_or_default())
                    .unwrap_or_default();
                let stderr = stderr_reader
                    .is_finished()
                    .then(|| stderr_reader.join().unwrap_or_default())
                    .unwrap_or_default();
                return Ok(std::process::Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    if let Err(e) = child.kill() {
                        eprintln!(
                            "herdr: kill failed: {} {}: {e}",
                            bin,
                            args.join(" ")
                        );
                    }
                    if let Err(e) = child.wait() {
                        eprintln!(
                            "herdr: wait after kill failed: {} {}: {e}",
                            bin,
                            args.join(" ")
                        );
                    }
                    // Pipes are closed after kill+wait, so readers terminate.
                    // Do NOT join them here: an orphaned grandchild (e.g. the
                    // `sleep 30` in the timeout test) can inherit the pipe
                    // write ends and hold them open past the deadline, which
                    // would turn the 10s timeout into a 30s hang. Detach the
                    // readers (drop the JoinHandles); they finish on their own
                    // once every writer exits.
                    std::mem::drop(stdout_reader);
                    std::mem::drop(stderr_reader);
                    eprintln!("herdr: invoke timed out after 10s: {} {}", bin, args.join(" "));
                    return Err("timeout".to_string());
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("herdr: wait failed: {} {}: {e}", bin, args.join(" "));
                let _ = child.kill();
                let _ = child.wait();
                // Detach readers (see timeout path): joining could block on
                // grandchild-inherited pipes; we return Err either way.
                std::mem::drop(stdout_reader);
                std::mem::drop(stderr_reader);
                return Err(format!("wait: {e}"));
            }
        }
    }
}

fn stderr_tail_200(stderr: &str) -> String {
    let chars: Vec<char> = stderr.chars().collect();
    if chars.len() > 200 {
        chars[chars.len() - 200..].iter().collect()
    } else {
        stderr.to_string()
    }
}

/// Shared `run_raw` + non-zero-exit check + tailed stderr log.
/// Returns `Some(output)` only when the child exited successfully;
/// spawn/timeout/wait failures and non-zero exits log and yield `None`.
fn run_checked(args: &[&str], ctx: &str) -> Option<std::process::Output> {
    match run_raw(args) {
        Err(e) => {
            eprintln!("herdr: {ctx} failed for {}: {e}", args.join(" "));
            None
        }
        Ok(o) => {
            if !o.status.success() {
                let stderr = String::from_utf8_lossy(&o.stderr);
                let tail = stderr_tail_200(stderr.trim());
                eprintln!(
                    "herdr: {ctx} non-zero exit {} for {}: {tail}",
                    o.status,
                    args.join(" ")
                );
                None
            } else {
                Some(o)
            }
        }
    }
}

fn extract_result(output: &std::process::Output) -> Option<Value> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut found = None;
    for line in stdout.lines().chain(stderr.lines()) {
        let line = line.trim();
        if !line.contains("\"result\"") {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            if v.get("error").is_some() {
                continue;
            }
            match v.get("result") {
                Some(result) if !result.is_null() => {
                    found = Some(result.clone());
                }
                _ => {}
            }
        }
    }
    found
}

/// Run `herdr <args...>`, scan stdout then stderr for the last JSON line
/// containing `"result"`, and return its parsed `result` object.
///
/// False-success guards: a non-zero exit status bails outright;
/// top-level `"error"` envelopes are skipped; `result: null` counts as absent.
pub fn invoke(args: &[&str]) -> Option<Value> {
    let output = run_checked(args, "invoke")?;
    extract_result(&output)
}

pub fn pane_list() -> Vec<Pane> {
    invoke(&["pane", "list"]).map(|r| panes_from_result(&r)).unwrap_or_default()
}

pub fn pane_list_checked() -> Option<Vec<Pane>> {
    let output = run_checked(&["pane", "list"], "pane list")?;
    match extract_result(&output) {
        Some(r) => Some(panes_from_result(&r)),
        None => {
            eprintln!("herdr: pane list returned no result envelope");
            None
        }
    }
}

pub fn pane_get(id: &str) -> Option<Pane> {
    let result = invoke(&["pane", "get", id])?;
    let pane = result.get("pane")?;
    match serde_json::from_value(pane.clone()) {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("herdr: pane_get unparsable pane, falling back to bare pane: {e}");
            let fallback_id = pane
                .get("pane_id")
                .and_then(|v| v.as_str())
                .unwrap_or(id)
                .to_string();
            Some(Pane {
                pane_id: fallback_id,
                ..Default::default()
            })
        }
    }
}

/// Bytes (beyond ASCII alphanumerics) that pass through unquoted in a
/// joined command line: none is a shell metacharacter in trailing-arg
/// position, so bare tokens are inert under POSIX `sh` quoting rules.
const BARE_TOKEN_BYTES: &[u8] = b"-_./:,=+@%";

/// True when `tok` needs no quoting in a joined shell line. Leading
/// `=` is excluded: zsh performs `=cmd` expansion on word-initial `=`
/// even in trailing-arg position.
fn is_bare_token(tok: &str) -> bool {
    !tok.is_empty()
        && !tok.starts_with('=')
        && tok
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || BARE_TOKEN_BYTES.contains(&b))
}

/// Join argv into a single shell line for delivery as one `pane run`
/// `COMMAND` value: bare tokens pass through, anything else is
/// single-quoted (embedded `'` → `'\''`, per POSIX §2.2.2; POSIX shells
/// only). Typical valued resume templates never quote — session values
/// are pre-screened by `is_safe_session_value` and session-ID shapes
/// stay in the bare set (`^`/non-ASCII values still quote, correctly).
fn join_command(argv: &[&str]) -> String {
    argv.iter()
        .map(|tok| {
            if is_bare_token(tok) {
                (*tok).to_string()
            } else {
                format!("'{}'", tok.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn pane_run(id: &str, argv: &[&str]) -> bool {
    // The joined line goes as ONE `COMMAND` value (never re-split), so
    // herdr cannot mistake embedded agent flags for its own options —
    // a single value starting with the program name is never parsed as
    // a flag. No `--` separator: herdr types it literally into the pane
    // (`zsh: command not found: --`, observed live on herdr 0.8.2), and
    // without it multi-token argv lets herdr consume its own globals
    // (notably `--session` → `server_not_running`).
    // TODO(herdr>0.8.2): re-probe `pane run <id> -- <cmd>`; if herdr
    // strips the separator again, the join can go back to plain argv.
    if argv.is_empty() {
        eprintln!("herdr: pane_run called with empty argv for {id}");
        return false;
    }
    let line = join_command(argv);
    run_checked(&["pane", "run", id, &line], "pane_run").is_some()
}

/// Suffix a joined command line with an exit-code marker so a later
/// `pane_read` can tell a clean exit (`:0`) from a crash. The marker
/// always prints: `;` runs it for any exit reason short of the shell
/// itself dying (which surfaces as pane misses instead).
fn wrap_with_exit_marker(line: &str) -> String {
    format!("{line}; printf '@@AUTORESUME-EXIT:%s@@\\n' \"$?\"")
}

/// Relaunch with the exit marker wrapped (see `wrap_with_exit_marker`).
/// Same verbatim single-`COMMAND` delivery as `pane_run`.
pub fn pane_run_wrapped(id: &str, argv: &[&str]) -> bool {
    if argv.is_empty() {
        eprintln!("herdr: pane_run_wrapped called with empty argv for {id}");
        return false;
    }
    let line = wrap_with_exit_marker(&join_command(argv));
    run_checked(&["pane", "run", id, &line], "pane_run_wrapped").is_some()
}

/// POSIX shells for exit-marker wrapping (`;` + `$?` + `printf`).
/// `fish` (`$status`), `nu`, and `pwsh` need different syntax and stay
/// on the legacy unwrapped path.
const POSIX_SHELLS: &[&str] = &["zsh", "bash", "sh", "dash", "ksh", "ash"];

/// True when the foreground is a single bare POSIX shell (basename
/// match, path and login-`-` prefixes stripped) — the only shape we
/// wrap with an exit marker. Called on the idle-shell foreground that
/// `should_relaunch` already vetted, so arity is 1×1 in practice.
pub(crate) fn foreground_shell_is_posix(proc_argv: &[Vec<String>]) -> bool {
    if proc_argv.len() != 1 || proc_argv[0].len() != 1 {
        return false;
    }
    let prog = &proc_argv[0][0];
    let base = prog.rsplit('/').next().unwrap_or(prog);
    let base = base.strip_prefix('-').unwrap_or(base);
    POSIX_SHELLS.contains(&base)
}

/// Type literal text into a pane and submit it: the two-step half of a
/// relaunch for agents whose template carries no `{message}` (e.g.
/// opencode — verified live: typed input into a resumed TUI is answered,
/// while `--prompt` neither submits nor visibly pre-fills). TEXT goes as
/// one `send-text` value so embedded flags stay inert; `Enter` submits.
pub fn send_text_enter(id: &str, text: &str) -> bool {
    run_checked(&["pane", "send-text", id, text], "pane_send_text").is_some()
        && run_checked(&["pane", "send-keys", id, "Enter"], "pane_send_keys").is_some()
}

/// Read a pane's recent scrollback as text (bounded tail for marker
/// scans). Raw terminal output, not a result envelope: any exit-0
/// stdout is taken verbatim. None on spawn/timeout/non-zero exit.
pub fn pane_read(id: &str) -> Option<String> {
    run_checked(&["pane", "read", id, "--lines", "50", "--format", "text"], "pane_read")
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}

const EXIT_MARKER_PREFIX: &str = "@@AUTORESUME-EXIT:";

/// The last wrapped-run exit code in scrollback, if any. Malformed
/// tails are skipped by scanning backwards; anything non-numeric or
/// unterminated never matches.
pub(crate) fn parse_exit_marker(text: &str) -> Option<i32> {
    let mut search = text;
    while let Some(idx) = search.rfind(EXIT_MARKER_PREFIX) {
        let rest = &search[idx + EXIT_MARKER_PREFIX.len()..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() && rest[digits.len()..].starts_with("@@") {
            if let Ok(code) = digits.parse::<i32>() {
                return Some(code);
            }
        }
        search = &search[..idx];
    }
    None
}

pub fn process_info(id: &str) -> Option<Value> {
    invoke(&["pane", "process-info", "--pane", id])?
        .get("process_info")
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::lock_env;
    use crate::test_support::{herdr_calls, run_with_fake_herdr};

    #[test]
    fn invoke_returns_none_on_nonzero_exit() {
        run_with_fake_herdr(
            "#!/bin/sh\necho '{\"id\":\"cli:pane:list\",\"result\":{\"panes\":[]}}'\nexit 1\n",
            &[],
            |_| {
                assert_eq!(invoke(&["pane", "list"]), None);
            },
        );
    }

    #[test]
    fn invoke_skips_envelope_carrying_error() {
        run_with_fake_herdr(
            "#!/bin/sh\necho '{\"id\":\"cli:pane:list\",\"error\":\"boom\",\"result\":{\"panes\":[]}}'\n",
            &[],
            |_| {
                assert_eq!(invoke(&["pane", "list"]), None);
            },
        );
    }

    #[test]
    fn invoke_treats_null_result_as_absent() {
        run_with_fake_herdr(
            "#!/bin/sh\necho '{\"id\":\"cli:pane:list\",\"result\":null}'\n",
            &[],
            |_| {
                assert_eq!(invoke(&["pane", "list"]), None);
            },
        );
    }

    #[test]
    fn invoke_returns_last_result_envelope() {
        run_with_fake_herdr(
            "#!/bin/sh\necho 'starting up'\necho '{\"id\":\"cli:pane:list\",\"result\":{\"panes\":[{\"pane_id\":\"w1:p1\"}]}}'\n",
            &[],
            |_| {
                let v = invoke(&["pane", "list"]).expect("valid envelope parses");
                assert_eq!(
                    v.get("panes").and_then(|p| p.as_array()).map(|a| a.len()),
                    Some(1)
                );
            },
        );
    }

    #[test]
    fn parses_pane_list_envelope() {
        let raw = r#"{"id":"cli:pane:list","result":{"panes":[{"pane_id":"w7G:p1","agent":"kiro","agent_status":"working"}]}}"#;
        let panes = parse_panes(raw);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane_id, "w7G:p1");
        assert_eq!(panes[0].agent.as_deref(), Some("kiro"));
    }

    #[test]
    fn unknown_status_pane_without_agent_key_parses() {
        // Pane object copied verbatim from live `herdr pane list` output
        // (2026-09-15): plain shells report agent_status "unknown" with no
        // `agent` key at all.
        let raw = r#"{"id":"cli:pane:list","result":{"panes":[{"agent_status":"unknown","cwd":"/home/joseph/Projects/icedtea","focused":false,"foreground_cwd":"/home/joseph/Projects/icedtea","pane_id":"w78:p1","revision":0,"scroll":{"max_offset_from_bottom":70,"offset_from_bottom":0,"viewport_rows":67},"tab_id":"w78:t1","terminal_id":"term_65b899f5c112e1","workspace_id":"w78"}]}}"#;
        let panes = parse_panes(raw);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane_id, "w78:p1");
        assert_eq!(panes[0].agent, None);
        assert_eq!(panes[0].agent_status.as_deref(), Some("unknown"));
        assert_eq!(panes[0].cwd.as_deref(), Some("/home/joseph/Projects/icedtea"));
        assert_eq!(panes[0].workspace_id.as_deref(), Some("w78"));
    }

    #[test]
    fn invoke_parses_envelope_with_nested_error_value() {
        run_with_fake_herdr(
            "#!/bin/sh\necho '{\"id\":\"cli:pane:list\",\"result\":{\"panes\":[{\"pane_id\":\"w1:p1\",\"detail\":{\"error\":\"nested boom\"}}]}}'\n",
            &[],
            |_| {
                let v = invoke(&["pane", "list"]).expect("nested error value must still parse");
                assert_eq!(
                    v.get("panes").and_then(|p| p.as_array()).map(|a| a.len()),
                    Some(1)
                );
            },
        );
    }

    #[test]
    fn pane_run_true_on_exit_zero_with_null_result() {
        run_with_fake_herdr(
            "#!/bin/sh\necho '{\"id\":\"cli:pane:run\",\"result\":null}'\nexit 0\n",
            &[],
            |_| {
                assert!(pane_run("w1:p1", &["echo", "hi"]));
            },
        );
    }

    #[test]
    fn pane_run_false_on_nonzero_exit() {
        run_with_fake_herdr(
            "#!/bin/sh\necho '{\"id\":\"cli:pane:run\",\"result\":{\"ok\":true}}'\nexit 1\n",
            &[],
            |_| {
                assert!(!pane_run("w1:p1", &["echo", "hi"]));
            },
        );
    }

    #[test]
    fn pane_list_returns_panes_from_envelope() {
        run_with_fake_herdr(
            "#!/bin/sh\necho '{\"id\":\"cli:pane:list\",\"result\":{\"panes\":[{\"pane_id\":\"w7G:p1\",\"agent\":\"kiro\",\"agent_status\":\"working\",\"cwd\":\"/tmp\",\"workspace_id\":\"w7G\"}]}}'\n",
            &[],
            |_| {
                let panes = pane_list();
                assert_eq!(panes.len(), 1);
                assert_eq!(panes[0].pane_id, "w7G:p1");
                assert_eq!(panes[0].agent.as_deref(), Some("kiro"));
                assert_eq!(panes[0].agent_status.as_deref(), Some("working"));
                assert_eq!(panes[0].cwd.as_deref(), Some("/tmp"));
                assert_eq!(panes[0].workspace_id.as_deref(), Some("w7G"));
            },
        );
    }

    #[test]
    fn pane_get_returns_pane_object() {
        run_with_fake_herdr(
            "#!/bin/sh\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w7G:p1\",\"agent\":\"kiro\",\"agent_status\":\"working\",\"cwd\":\"/tmp\",\"workspace_id\":\"w7G\"}}}'\n",
            &[],
            |_| {
                let pane = pane_get("w7G:p1").expect("pane_get parses pane object");
                assert_eq!(pane.pane_id, "w7G:p1");
                assert_eq!(pane.agent.as_deref(), Some("kiro"));
                assert_eq!(pane.agent_status.as_deref(), Some("working"));
                assert_eq!(pane.cwd.as_deref(), Some("/tmp"));
                assert_eq!(pane.workspace_id.as_deref(), Some("w7G"));
            },
        );
    }

    #[test]
    fn process_info_returns_process_info_object() {
        run_with_fake_herdr(
            "#!/bin/sh\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"pid\":123,\"command\":\"claude\"}}}'\n",
            &[],
            |_| {
                let info = process_info("w7G:p1").expect("process_info parses object");
                assert_eq!(info.get("pid").and_then(|v| v.as_u64()), Some(123));
                assert_eq!(
                    info.get("command").and_then(|v| v.as_str()),
                    Some("claude")
                );
            },
        );
    }

    #[test]
    fn invoke_returns_none_on_spawn_failure() {
        let _guard = lock_env();
        let prev = std::env::var_os("HERDR_BIN_PATH");
        std::env::set_var(
            "HERDR_BIN_PATH",
            "/nonexistent-herdr-bin-xyz-12345/herdr",
        );
        let result = invoke(&["pane", "list"]);
        match prev {
            Some(v) => std::env::set_var("HERDR_BIN_PATH", v),
            None => std::env::remove_var("HERDR_BIN_PATH"),
        }
        assert_eq!(result, None);
    }

    #[test]
    fn parse_panes_returns_empty_on_garbage() {
        assert!(parse_panes("not json").is_empty());
        assert!(parse_panes(r#"{"id":"x"}"#).is_empty());
        assert!(parse_panes(r#"{"id":"x","result":{"panes":"nope"}}"#).is_empty());
        assert!(parse_panes(r#"{"id":"x","result":null}"#).is_empty());
    }

    #[test]
    fn pane_with_bad_agent_session_parses_with_none() {
        let raw = r#"{"id":"cli:pane:list","result":{"panes":[{"pane_id":"w1:p1","agent_session":{"wrong_key":"x"}}]}}"#;
        let panes = parse_panes(raw);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane_id, "w1:p1");
        assert_eq!(panes[0].agent_session, None);
    }

    #[test]
    fn session_ref_aliases_parse_into_value() {
        let a: SessionRef =
            serde_json::from_value(serde_json::json!({"agent":"kiro","session_id":"s1"}))
                .expect("session_id alias parses");
        assert_eq!(a.value, "s1");
        let b: SessionRef =
            serde_json::from_value(serde_json::json!({"agent":"kiro","sessionId":"s2"}))
                .expect("sessionId alias parses");
        assert_eq!(b.value, "s2");
        let c: SessionRef =
            serde_json::from_value(serde_json::json!({"agent":"kiro","id":"s3"}))
                .expect("id alias parses");
        assert_eq!(c.value, "s3");
    }

    #[test]
    fn invoke_times_out_and_returns_none() {
        let start = std::time::Instant::now();
        run_with_fake_herdr("#!/bin/sh\nsleep 30\n", &[], |_| {
            assert_eq!(invoke(&["pane", "list"]), None);
        });
        let elapsed = start.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_secs(10),
            "invoke must wait out the 10s timeout, took {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(30),
            "invoke must return quickly on timeout, took {elapsed:?}"
        );
    }

    #[test]
    fn invoke_returns_despite_grandchild_holding_pipes() {
        // Child exits 0 immediately but leaves a backgrounded grandchild
        // holding the pipe write ends: success-path join must not hang.
        let start = std::time::Instant::now();
        run_with_fake_herdr(
            "#!/bin/sh\nsleep 30 &\necho '{\"id\":\"x\",\"result\":{\"panes\":[]}}'\n",
            &[],
            |_| {
                let panes = pane_list();
                assert!(panes.is_empty());
            },
        );
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(8),
            "invoke must not block on inherited pipes, took {elapsed:?}"
        );
    }

    #[test]
    fn panes_from_result_skips_bad_entry_keeps_good() {
        let result = serde_json::json!({
            "panes": [
                {"pane_id": "w1:p9", "agent": 5},
                {"pane_id": "w1:p1", "agent": "kiro"}
            ]
        });
        let panes = panes_from_result(&result);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane_id, "w1:p1");
    }

    #[test]
    fn pane_get_falls_back_to_bare_pane_on_schema_drift() {
        run_with_fake_herdr(
            "#!/bin/sh\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w1:p9\",\"agent\":5}}}'\n",
            &[],
            |_| {
                let pane = pane_get("w1:p9").expect("schema drift degrades to bare pane");
                assert_eq!(pane.pane_id, "w1:p9");
            },
        );
    }

    #[test]
    fn pane_run_sends_single_command_value() {
        // Delivery contract (see `pane_run` docs): the joined line goes
        // as ONE `COMMAND` value.
        run_with_fake_herdr(
            "#!/bin/sh\necho \"$#:$@\" >> \"@CALL_LOG@\"\nexit 0\n",
            &[],
            |dir| {
                assert!(pane_run("w1:p1", &["opencode", "--session", "ses-1"]));
                let lines: Vec<String> = herdr_calls(dir)
                    .lines()
                    .map(str::to_string)
                    .collect();
                assert_eq!(
                    lines,
                    vec!["4:pane run w1:p1 opencode --session ses-1"],
                    "one pane run call, command as a single value"
                );
            },
        );
    }

    #[test]
    fn pane_run_quotes_text_as_single_token() {
        // End-to-end wiring into `join_command` (arity stays 4).
        run_with_fake_herdr(
            "#!/bin/sh\necho \"$#:$@\" >> \"@CALL_LOG@\"\nexit 0\n",
            &[],
            |dir| {
                assert!(pane_run("w1:p1", &["echo", "a b"]));
                assert_eq!(
                    herdr_calls(dir).lines().collect::<Vec<_>>(),
                    vec!["4:pane run w1:p1 echo 'a b'"]
                );
            },
        );
    }

    #[test]
    fn join_command_quotes_only_when_needed() {
        assert_eq!(join_command(&["opencode", "--session", "ses-1"]), "opencode --session ses-1");
        assert_eq!(
            join_command(&["kiro-cli", "chat", "--resume-id", "7d204a41-9bac-4530-a190-c4bbe12461f5"]),
            "kiro-cli chat --resume-id 7d204a41-9bac-4530-a190-c4bbe12461f5"
        );
        assert_eq!(join_command(&["echo", "a b"]), "echo 'a b'");
        assert_eq!(join_command(&["echo", "a'b"]), "echo 'a'\\''b'");
        assert_eq!(join_command(&[] as &[&str]), "");
    }

    #[test]
    fn join_command_quotes_shell_metachars_and_empty() {
        // Every shell-active byte must take the quoted branch; bare-set
        // members must stay unquoted (over-quoting regressions fail).
        assert_eq!(join_command(&["echo", ""]), "echo ''");
        assert_eq!(join_command(&["echo", "a$b"]), "echo 'a$b'");
        assert_eq!(join_command(&["echo", "a`b`"]), "echo 'a`b`'");
        assert_eq!(join_command(&["echo", "a\"b"]), "echo 'a\"b'");
        assert_eq!(join_command(&["echo", "a\\b"]), "echo 'a\\b'");
        assert_eq!(join_command(&["echo", "a;b"]), "echo 'a;b'");
        assert_eq!(join_command(&["echo", "a|b"]), "echo 'a|b'");
        assert_eq!(join_command(&["echo", "a&b"]), "echo 'a&b'");
        assert_eq!(join_command(&["echo", "caf\u{e9}"]), "echo 'caf\u{e9}'");
        assert_eq!(join_command(&["echo", "=foo"]), "echo '=foo'");
        assert_eq!(
            join_command(&["echo", "a*b?c#d~e!f(g)h[i]j{k}l<m>n"]),
            "echo 'a*b?c#d~e!f(g)h[i]j{k}l<m>n'"
        );
        assert_eq!(join_command(&["echo", "a\nb\tc"]), "echo 'a\nb\tc'");
        assert_eq!(join_command(&["prog", "a-b_c.d=e+f@g%h/i:j,k"]), "prog a-b_c.d=e+f@g%h/i:j,k");
    }

    #[test]
    fn pane_run_false_on_empty_argv() {
        run_with_fake_herdr("#!/bin/sh\nexit 0\n", &[], |_| {
            assert!(!pane_run("w1:p1", &[]));
        });
    }

    #[test]
    fn pane_run_wrapped_appends_exit_marker() {
        // Single COMMAND value: joined line + `;` + marker suffix.
        run_with_fake_herdr(
            "#!/bin/sh\necho \"$#:$@\" >> \"@CALL_LOG@\"\nexit 0\n",
            &[],
            |dir| {
                assert!(pane_run_wrapped("w1:p1", &["kiro-cli", "chat", "-r"]));
                assert_eq!(
                    crate::test_support::herdr_calls(dir).lines().collect::<Vec<_>>(),
                    vec!["4:pane run w1:p1 kiro-cli chat -r; printf '@@AUTORESUME-EXIT:%s@@\\n' \"$?\""]
                );
            },
        );
    }

    #[test]
    fn pane_run_wrapped_false_on_empty_argv() {
        run_with_fake_herdr("#!/bin/sh\nexit 0\n", &[], |_| {
            assert!(!pane_run_wrapped("w1:p1", &[]));
        });
    }

    #[test]
    fn foreground_shell_is_posix_matches_bare_shells() {
        let zsh = vec![vec!["/usr/bin/zsh".to_string()]];
        assert!(foreground_shell_is_posix(&zsh));
        let login = vec![vec!["-bash".to_string()]];
        assert!(foreground_shell_is_posix(&login));
        for shell in ["fish", "nu", "pwsh"] {
            let argv = vec![vec![shell.to_string()]];
            assert!(!foreground_shell_is_posix(&argv), "{shell} must stay unwrapped");
        }
        assert!(!foreground_shell_is_posix(&[]));
        assert!(!foreground_shell_is_posix(&[vec!["/usr/bin/zsh".into(), "-c".into()]]));
        assert!(!foreground_shell_is_posix(&[vec!["/usr/bin/zsh".into()], vec!["/usr/bin/zsh".into()]]));
    }

    #[test]
    fn parse_exit_marker_takes_last_well_formed() {
        assert_eq!(parse_exit_marker("@@AUTORESUME-EXIT:0@@"), Some(0));
        assert_eq!(parse_exit_marker("@@AUTORESUME-EXIT:137@@"), Some(137));
        assert_eq!(
            parse_exit_marker("old @@AUTORESUME-EXIT:1@@ prompt @@AUTORESUME-EXIT:0@@ » "),
            Some(0),
            "last marker wins"
        );
        assert_eq!(parse_exit_marker("no marker here"), None);
        assert_eq!(parse_exit_marker("@@AUTORESUME-EXIT:@@"), None);
        assert_eq!(parse_exit_marker("@@AUTORESUME-EXIT:4x@@"), None);
        assert_eq!(parse_exit_marker("@@AUTORESUME-EXIT:4"), None, "unterminated");
        assert_eq!(
            parse_exit_marker("@@AUTORESUME-EXIT:@@ tail @@AUTORESUME-EXIT:3@@"),
            Some(3),
            "malformed earlier marker skipped"
        );
    }

    #[test]
    fn pane_read_returns_scrollback_text() {
        run_with_fake_herdr(
            "#!/bin/sh\necho 'scrollback with @@AUTORESUME-EXIT:0@@'\nexit 0\n",
            &[],
            |_| {
                assert_eq!(
                    pane_read("w1:p1").as_deref(),
                    Some("scrollback with @@AUTORESUME-EXIT:0@@\n")
                );
            },
        );
    }

    #[test]
    fn pane_read_none_on_failure() {
        run_with_fake_herdr("#!/bin/sh\nexit 1\n", &[], |_| {
            assert_eq!(pane_read("w1:p1"), None);
        });
    }

    #[test]
    fn send_text_enter_sends_text_then_enter() {
        run_with_fake_herdr(
            "#!/bin/sh\necho \"$#:$@\" >> \"@CALL_LOG@\"\nexit 0\n",
            &[],
            |dir| {
                assert!(send_text_enter("w1:p1", "continue"));
                assert_eq!(
                    crate::test_support::herdr_calls(dir).lines().collect::<Vec<_>>(),
                    vec![
                        "4:pane send-text w1:p1 continue",
                        "4:pane send-keys w1:p1 Enter",
                    ]
                );
            },
        );
    }

    #[test]
    fn send_text_enter_false_when_keys_fail() {
        run_with_fake_herdr(
            "#!/bin/sh\nif [ \"$2\" = \"send-keys\" ]; then exit 1; fi\nexit 0\n",
            &[],
            |_| {
                assert!(!send_text_enter("w1:p1", "continue"));
            },
        );
    }

    #[test]
    fn pane_run_false_on_spawn_failure() {
        let _guard = lock_env();
        let prev = std::env::var_os("HERDR_BIN_PATH");
        std::env::set_var("HERDR_BIN_PATH", "/nonexistent-herdr-bin-xyz-12345/herdr");
        let result = pane_run("w1:p1", &["echo", "hi"]);
        match prev {
            Some(v) => std::env::set_var("HERDR_BIN_PATH", v),
            None => std::env::remove_var("HERDR_BIN_PATH"),
        }
        assert!(!result);
    }

    #[test]
    fn pane_list_checked_some_empty_on_success_empty() {
        run_with_fake_herdr(
            "#!/bin/sh\necho '{\"id\":\"cli:pane:list\",\"result\":{\"panes\":[]}}'\n",
            &[],
            |_| {
                let v = pane_list_checked().expect("success with empty panes is Some");
                assert!(v.is_empty());
            },
        );
    }

    #[test]
    fn pane_list_checked_none_on_spawn_failure() {
        let _guard = lock_env();
        let prev = std::env::var_os("HERDR_BIN_PATH");
        std::env::set_var("HERDR_BIN_PATH", "/nonexistent-herdr-bin-xyz-12345/herdr");
        let result = pane_list_checked();
        match prev {
            Some(v) => std::env::set_var("HERDR_BIN_PATH", v),
            None => std::env::remove_var("HERDR_BIN_PATH"),
        }
        assert_eq!(result, None);
    }

    #[test]
    fn pane_list_checked_none_on_nonzero_exit() {
        run_with_fake_herdr(
            "#!/bin/sh\necho '{\"id\":\"cli:pane:list\",\"result\":{\"panes\":[]}}'\nexit 1\n",
            &[],
            |_| {
                assert_eq!(pane_list_checked(), None);
            },
        );
    }

    #[test]
    fn pane_list_checked_none_on_success_without_envelope() {
        run_with_fake_herdr("#!/bin/sh\necho 'garbage no json here'\nexit 0\n", &[], |_| {
            assert_eq!(pane_list_checked(), None);
        });
    }
}
