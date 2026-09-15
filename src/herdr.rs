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

fn parse_panes(raw: &str) -> Vec<Pane> {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|v| v.get("result").cloned())
        .map(|r| panes_from_result(&r))
        .unwrap_or_default()
}

/// Run `herdr <args...>` with a 10s timeout, returning the raw output.
/// On timeout the child is killed and None is returned (fail-closed).
fn run_raw(args: &[&str]) -> Option<std::process::Output> {
    let bin = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string());
    let child = std::process::Command::new(&bin)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let slot = std::sync::Arc::new(std::sync::Mutex::new(Some(child)));
    let slot_clone = std::sync::Arc::clone(&slot);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let child_opt = slot_clone.lock().ok().and_then(|mut g| g.take());
        let Some(child) = child_opt else {
            let _ = tx.send(None);
            return;
        };
        let out = child.wait_with_output().ok();
        let _ = tx.send(out);
    });
    match rx.recv_timeout(std::time::Duration::from_secs(10)) {
        Ok(out) => out,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            if let Ok(mut g) = slot.lock() {
                if let Some(mut child) = g.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
            eprintln!("herdr: invoke timed out after 10s: {} {}", bin, args.join(" "));
            None
        }
        Err(_) => None,
    }
}

/// Run `herdr <args...>`, scan stdout then stderr for the last JSON line
/// containing `"result"`, and return its parsed `result` object.
///
/// False-success guards: a non-zero exit status bails outright;
/// top-level `"error"` envelopes are skipped; `result: null` counts as absent.
pub fn invoke(args: &[&str]) -> Option<Value> {
    let output = run_raw(args)?;
    if !output.status.success() {
        return None;
    }
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

pub fn pane_list() -> Vec<Pane> {
    invoke(&["pane", "list"]).map(|r| panes_from_result(&r)).unwrap_or_default()
}

pub fn pane_get(id: &str) -> Option<Pane> {
    let result = invoke(&["pane", "get", id])?;
    let pane = result.get("pane")?;
    serde_json::from_value(pane.clone()).ok()
}

pub fn pane_run(id: &str, argv: &[&str]) -> bool {
    let mut args = vec!["pane", "run", id];
    args.extend_from_slice(argv);
    run_raw(&args).map(|o| o.status.success()).unwrap_or(false)
}

pub fn process_info(id: &str) -> Option<Value> {
    invoke(&["pane", "process-info", "--pane", id])?
        .get("process_info")
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the invoke tests: they mutate the process-global
    /// `HERDR_BIN_PATH`, so they must never run concurrently.
    static HERDR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Run `f` with `HERDR_BIN_PATH` pointed at an executable shell script
    /// with `script_body`, restoring the previous value afterwards.
    fn with_fake_herdr(script_body: &str, f: impl FnOnce()) {
        let _guard = HERDR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _guard = HERDR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
            elapsed < std::time::Duration::from_secs(15),
            "invoke must return quickly on timeout, took {elapsed:?}"
        );
    }
}
