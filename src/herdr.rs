use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionRef {
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct Pane {
    #[serde(default)]
    pub pane_id: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub agent_status: Option<String>,
    #[serde(default)]
    pub agent_session: Option<SessionRef>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
}

fn panes_from_result(result: &Value) -> Vec<Pane> {
    result
        .get("panes")
        .and_then(|p| serde_json::from_value(p.clone()).ok())
        .unwrap_or_default()
}

fn parse_panes(raw: &str) -> Vec<Pane> {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|v| v.get("result").cloned())
        .map(|r| panes_from_result(&r))
        .unwrap_or_default()
}

/// Run `herdr <args...>`, scan stdout then stderr for the last JSON line
/// containing `"result"`, and return its parsed `result` object.
///
/// False-success guards: a non-zero exit status bails outright; lines
/// carrying `"error"` are skipped; `result: null` counts as absent.
pub fn invoke(args: &[&str]) -> Option<Value> {
    let bin = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string());
    let output = std::process::Command::new(&bin).args(args).output().ok()?;
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
        if line.contains("\"error\"") {
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
    invoke(&args).is_some()
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
}
