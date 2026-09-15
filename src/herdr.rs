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
                // Child has exited; pipes hit EOF so readers finish promptly.
                let stdout = stdout_reader.join().unwrap_or_default();
                let stderr = stderr_reader.join().unwrap_or_default();
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

pub fn pane_run(id: &str, argv: &[&str]) -> bool {
    let mut args = vec!["pane", "run", id];
    args.extend_from_slice(argv);
    run_checked(&args, "pane_run").is_some()
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

    /// Run `f` with `HERDR_BIN_PATH` pointed at an executable shell script
    /// with `script_body`, restoring the previous value afterwards.
    fn with_fake_herdr(script_body: &str, f: impl FnOnce()) {
        let _guard = lock_env();
        let prev = std::env::var_os("HERDR_BIN_PATH");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "auto-resume-herdr-test-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("herdr");
        std::fs::write(&path, script_body).expect("write fake herdr");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake herdr");
        std::env::set_var("HERDR_BIN_PATH", &path);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        match prev {
            Some(v) => std::env::set_var("HERDR_BIN_PATH", v),
            None => std::env::remove_var("HERDR_BIN_PATH"),
        }
        std::fs::remove_dir_all(&dir).ok();
        assert!(result.is_ok());
    }

    #[test]
    fn invoke_returns_none_on_nonzero_exit() {
        with_fake_herdr(
            "#!/bin/sh\necho '{\"id\":\"cli:pane:list\",\"result\":{\"panes\":[]}}'\nexit 1\n",
            || {
                assert_eq!(invoke(&["pane", "list"]), None);
            },
        );
    }

    #[test]
    fn invoke_skips_envelope_carrying_error() {
        with_fake_herdr(
            "#!/bin/sh\necho '{\"id\":\"cli:pane:list\",\"error\":\"boom\",\"result\":{\"panes\":[]}}'\n",
            || {
                assert_eq!(invoke(&["pane", "list"]), None);
            },
        );
    }

    #[test]
    fn invoke_treats_null_result_as_absent() {
        with_fake_herdr(
            "#!/bin/sh\necho '{\"id\":\"cli:pane:list\",\"result\":null}'\n",
            || {
                assert_eq!(invoke(&["pane", "list"]), None);
            },
        );
    }

    #[test]
    fn invoke_returns_last_result_envelope() {
        with_fake_herdr(
            "#!/bin/sh\necho 'starting up'\necho '{\"id\":\"cli:pane:list\",\"result\":{\"panes\":[{\"pane_id\":\"w1:p1\"}]}}'\n",
            || {
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
        with_fake_herdr(
            "#!/bin/sh\necho '{\"id\":\"cli:pane:list\",\"result\":{\"panes\":[{\"pane_id\":\"w1:p1\",\"detail\":{\"error\":\"nested boom\"}}]}}'\n",
            || {
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
        with_fake_herdr(
            "#!/bin/sh\necho '{\"id\":\"cli:pane:run\",\"result\":null}'\nexit 0\n",
            || {
                assert!(pane_run("w1:p1", &["echo", "hi"]));
            },
        );
    }

    #[test]
    fn pane_run_false_on_nonzero_exit() {
        with_fake_herdr(
            "#!/bin/sh\necho '{\"id\":\"cli:pane:run\",\"result\":{\"ok\":true}}'\nexit 1\n",
            || {
                assert!(!pane_run("w1:p1", &["echo", "hi"]));
            },
        );
    }

    #[test]
    fn pane_list_returns_panes_from_envelope() {
        with_fake_herdr(
            "#!/bin/sh\necho '{\"id\":\"cli:pane:list\",\"result\":{\"panes\":[{\"pane_id\":\"w7G:p1\",\"agent\":\"kiro\",\"agent_status\":\"working\",\"cwd\":\"/tmp\",\"workspace_id\":\"w7G\"}]}}'\n",
            || {
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
        with_fake_herdr(
            "#!/bin/sh\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w7G:p1\",\"agent\":\"kiro\",\"agent_status\":\"working\",\"cwd\":\"/tmp\",\"workspace_id\":\"w7G\"}}}'\n",
            || {
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
        with_fake_herdr(
            "#!/bin/sh\necho '{\"id\":\"cli:pane:process-info\",\"result\":{\"process_info\":{\"pid\":123,\"command\":\"claude\"}}}'\n",
            || {
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
        with_fake_herdr("#!/bin/sh\nsleep 30\n", || {
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
        with_fake_herdr(
            "#!/bin/sh\necho '{\"id\":\"cli:pane:get\",\"result\":{\"pane\":{\"pane_id\":\"w1:p9\",\"agent\":5}}}'\n",
            || {
                let pane = pane_get("w1:p9").expect("schema drift degrades to bare pane");
                assert_eq!(pane.pane_id, "w1:p9");
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
        with_fake_herdr(
            "#!/bin/sh\necho '{\"id\":\"cli:pane:list\",\"result\":{\"panes\":[]}}'\n",
            || {
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
        with_fake_herdr(
            "#!/bin/sh\necho '{\"id\":\"cli:pane:list\",\"result\":{\"panes\":[]}}'\nexit 1\n",
            || {
                assert_eq!(pane_list_checked(), None);
            },
        );
    }

    #[test]
    fn pane_list_checked_none_on_success_without_envelope() {
        with_fake_herdr("#!/bin/sh\necho 'garbage no json here'\nexit 0\n", || {
            assert_eq!(pane_list_checked(), None);
        });
    }
}
