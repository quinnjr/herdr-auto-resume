use std::collections::HashMap;

use crate::herdr::{Pane, SessionRef};

pub const VALUE_PLACEHOLDER: &str = "{value}";

/// Split a command template into argv words, honouring single/double
/// quotes and backslash escapes (outside single quotes).
pub fn split_argv(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_tok = false;
    let mut quote: Option<char> = None;
    let mut chars = cmd.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some('\'') => {
                if c == '\'' {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            Some(q) => {
                if c == q {
                    quote = None;
                } else if c == '\\' {
                    if let Some(n) = chars.next() {
                        cur.push(n);
                    }
                } else {
                    cur.push(c);
                }
            }
            None => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                    in_tok = true;
                } else if c == '\\' {
                    if let Some(n) = chars.next() {
                        cur.push(n);
                    }
                    in_tok = true;
                } else if c.is_whitespace() {
                    if in_tok {
                        out.push(std::mem::take(&mut cur));
                        in_tok = false;
                    }
                } else {
                    cur.push(c);
                    in_tok = true;
                }
            }
        }
    }
    if in_tok {
        out.push(cur);
    }
    out
}

/// Derive the resume flag token from a command template: the token before
/// `{value}` (`--resume {value}` → `--resume`; bare `resume {value}` →
/// `resume`), or the token carrying it with the placeholder stripped
/// (`--resume={value}` → `--resume`). None when the template carries no
/// value or leaves no flag behind.
fn flag_from_template(template: &str) -> Option<String> {
    let toks = split_argv(template);
    let idx = toks.iter().position(|t| t.contains(VALUE_PLACEHOLDER))?;
    let attached = toks[idx].replace(VALUE_PLACEHOLDER, "");
    let attached = attached.strip_suffix('=').unwrap_or(&attached);
    if !attached.is_empty() {
        return Some(attached.to_string());
    }
    if idx == 0 {
        return None;
    }
    Some(toks[idx - 1].clone())
}

/// Defense-in-depth for value substitution: the value lands in a single
/// argv token (never re-split), but reject values that could act as flag
/// or shell injection if ever logged/replayed through a shell — empty,
/// ASCII whitespace, control chars, quotes, backslash, or shell
/// metacharacters (`; & | < > ( ) $ ` ! * ? [ ] { } ~ #`).
fn is_safe_session_value(v: &str) -> bool {
    !v.is_empty()
        && !v.chars().any(|c| {
            c.is_ascii_whitespace()
                || c.is_ascii_control()
                || matches!(
                    c,
                    '"' | '\''
                        | '\\'
                        | ';'
                        | '&'
                        | '|'
                        | '<'
                        | '>'
                        | '('
                        | ')'
                        | '$'
                        | '`'
                        | '!'
                        | '*'
                        | '?'
                        | '['
                        | ']'
                        | '{'
                        | '}'
                        | '~'
                        | '#'
                )
        })
}

/// Recover the session value for `agent` by scanning process argv for the
/// resume flag derived from `commands` (`flag <value>` and `flag=<value>`;
/// values starting with `-` are skipped). Unknown agent or no match → None.
/// `kiro` also accepts the live-CLI spelling `--resume` alongside the
/// template-derived `--resume-id` (template spelling tried first).
pub fn session_from_argv_with_commands(
    agent: &str,
    proc_argv: &[Vec<String>],
    commands: &HashMap<String, String>,
) -> Option<SessionRef> {
    let template = commands.get(agent)?;
    let primary = flag_from_template(template)?;
    let mut flags = vec![primary.clone()];
    if agent == "kiro" {
        for alt in ["--resume-id", "--resume"] {
            if alt != primary && !flags.iter().any(|f| f == alt) {
                flags.push(alt.to_string());
            }
        }
    }
    for flag in &flags {
        let eq_prefix = format!("{flag}=");
        for argv in proc_argv {
            for (i, tok) in argv.iter().enumerate() {
                if let Some(rest) = tok.strip_prefix(&eq_prefix) {
                    if !rest.is_empty() && !rest.starts_with('-') {
                        return Some(SessionRef {
                            agent: agent.to_string(),
                            value: rest.to_string(),
                        });
                    }
                    continue;
                }
                if *tok == *flag {
                    if let Some(next) = argv.get(i + 1) {
                        if !next.is_empty() && !next.starts_with('-') {
                            return Some(SessionRef {
                                agent: agent.to_string(),
                                value: next.clone(),
                            });
                        }
                    }
                }
            }
        }
    }
    None
}

/// `session_from_argv_with_commands` against the default command templates.
pub fn session_from_argv(agent: &str, proc_argv: &[Vec<String>]) -> Option<SessionRef> {
    session_from_argv_with_commands(agent, proc_argv, &crate::config::default_commands())
}

/// Build the resume argv for `agent`. A template containing `{value}`
/// requires a value; a template without it (e.g. `kiro-cli chat -r`) is a
/// valueless fallback usable without one. When no value is known, an
/// explicit `<agent>-fallback` entry (e.g. `kiro-fallback`) is preferred
/// over a valueless primary template. Unknown agent → None.
pub fn resume_argv(
    agent: &str,
    value: Option<&str>,
    commands: &HashMap<String, String>,
) -> Option<Vec<String>> {
    match value {
        Some(v) => {
            if !is_safe_session_value(v) {
                return None;
            }
            let template = commands.get(agent)?;
            if template.contains(VALUE_PLACEHOLDER) {
                // Split the template first, then substitute per token: the
                // value is never re-split, so it cannot inject extra flags.
                Some(
                    split_argv(template)
                        .into_iter()
                        .map(|t| t.replace(VALUE_PLACEHOLDER, v))
                        .collect(),
                )
            } else {
                Some(split_argv(template))
            }
        }
        None => {
            let fallback_key = format!("{agent}-fallback");
            if let Some(fb) = commands.get(&fallback_key) {
                if !fb.contains(VALUE_PLACEHOLDER) {
                    return Some(split_argv(fb));
                }
            }
            let template = commands.get(agent)?;
            if template.contains(VALUE_PLACEHOLDER) {
                return None;
            }
            Some(split_argv(template))
        }
    }
}

/// Resolve the session for a pane: live `agent_session` → registry value →
/// process-argv derivation. Needs the pane's `agent` for the argv step.
pub fn resolve_session_with_commands(
    pane: &Pane,
    registry_value: Option<&SessionRef>,
    proc_argv: &[Vec<String>],
    commands: &HashMap<String, String>,
) -> Option<SessionRef> {
    if let Some(live) = &pane.agent_session {
        return Some(live.clone());
    }
    // A cross-agent registry ref is stale (pane re-created for another
    // agent): accept it only when the pane names no agent or the same one.
    if let Some(reg) = registry_value {
        if pane.agent.is_none() || pane.agent.as_deref() == Some(reg.agent.as_str()) {
            return Some(reg.clone());
        }
    }
    let agent = pane.agent.as_deref()?;
    session_from_argv_with_commands(agent, proc_argv, commands)
}

/// `resolve_session_with_commands` against the default command templates.
pub fn resolve_session(
    pane: &Pane,
    registry_value: Option<&SessionRef>,
    proc_argv: &[Vec<String>],
) -> Option<SessionRef> {
    resolve_session_with_commands(
        pane,
        registry_value,
        proc_argv,
        &crate::config::default_commands(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kiro_falls_back_to_valueless_resume() {
        let cmds = HashMap::from([("kiro".into(), "kiro-cli chat -r".into())]);
        assert_eq!(
            resume_argv("kiro", None, &cmds).unwrap(),
            vec!["kiro-cli", "chat", "-r"]
        );
    }

    #[test]
    fn valued_template_requires_value() {
        let cmds = HashMap::from([("claude".into(), "claude --resume {value}".into())]);
        assert!(resume_argv("claude", None, &cmds).is_none());
        assert_eq!(
            resume_argv("claude", Some("abc-123"), &cmds).unwrap(),
            vec!["claude", "--resume", "abc-123"]
        );
    }

    #[test]
    fn resume_flag_derived_from_process_argv() {
        // pane ran `claude --resume abc-123` → value recovered without agent_session
        let argv = vec![vec!["claude".into(), "--resume".into(), "abc-123".into()]];
        let found = session_from_argv("claude", &argv);
        assert_eq!(found.unwrap().value, "abc-123");
    }

    #[test]
    fn split_argv_handles_quotes() {
        assert_eq!(
            split_argv("claude --resume \"a b\""),
            vec!["claude", "--resume", "a b"]
        );
    }

    #[test]
    fn kiro_fallback_key_used_when_no_value_known() {
        let cmds = HashMap::from([
            ("kiro".into(), "kiro-cli chat --resume-id {value}".into()),
            ("kiro-fallback".into(), "kiro-cli chat -r".into()),
        ]);
        // No value → valueless fallback template.
        assert_eq!(
            resume_argv("kiro", None, &cmds).unwrap(),
            vec!["kiro-cli", "chat", "-r"]
        );
        // Known value → valued primary template.
        assert_eq!(
            resume_argv("kiro", Some("sess-1"), &cmds).unwrap(),
            vec!["kiro-cli", "chat", "--resume-id", "sess-1"]
        );
    }

    #[test]
    fn unknown_agent_returns_none() {
        let cmds = HashMap::from([("claude".into(), "claude --resume {value}".into())]);
        assert!(resume_argv("nope", Some("v"), &cmds).is_none());
        assert!(resume_argv("nope", None, &cmds).is_none());
    }

    #[test]
    fn session_from_argv_supports_equals_form_and_skips_flags() {
        let argv = vec![vec!["claude".into(), "--resume=abc-123".into()]];
        assert_eq!(session_from_argv("claude", &argv).unwrap().value, "abc-123");

        // A flag-like token after the flag is not a value.
        let argv = vec![vec!["claude".into(), "--resume".into(), "--verbose".into()]];
        assert!(session_from_argv("claude", &argv).is_none());
    }

    fn pane_with(agent: Option<&str>, session: Option<SessionRef>) -> Pane {
        Pane {
            pane_id: "w1:p1".into(),
            agent: agent.map(str::to_string),
            agent_status: None,
            agent_session: session,
            cwd: None,
            workspace_id: None,
        }
    }

    #[test]
    fn resolve_session_prefers_live_over_registry_over_argv() {
        let live = SessionRef {
            agent: "claude".into(),
            value: "live".into(),
        };
        let reg = SessionRef {
            agent: "claude".into(),
            value: "reg".into(),
        };
        let argv = vec![vec!["claude".into(), "--resume".into(), "from-argv".into()]];
        let pane = pane_with(Some("claude"), Some(live.clone()));
        assert_eq!(resolve_session(&pane, Some(&reg), &argv).unwrap(), live);

        let pane = pane_with(Some("claude"), None);
        assert_eq!(resolve_session(&pane, Some(&reg), &argv).unwrap(), reg);

        let pane = pane_with(Some("claude"), None);
        let found = resolve_session(&pane, None, &argv).unwrap();
        assert_eq!(found.value, "from-argv");
        assert_eq!(found.agent, "claude");
    }

    #[test]
    fn resolve_session_ignores_registry_on_agent_mismatch() {
        // pane.agent=Some("claude") + registry{agent:kiro} → registry
        // ignored: falls through to argv derivation, or None.
        let reg = SessionRef {
            agent: "kiro".into(),
            value: "reg-sess".into(),
        };
        let pane = pane_with(Some("claude"), None);
        let argv = vec![vec!["claude".into(), "--resume".into(), "from-argv".into()]];
        let found = resolve_session(&pane, Some(&reg), &argv).unwrap();
        assert_eq!(found.agent, "claude");
        assert_eq!(found.value, "from-argv");

        let argv_none: Vec<Vec<String>> = vec![vec!["claude".into()]];
        assert!(resolve_session(&pane, Some(&reg), &argv_none).is_none());
    }

    #[test]
    fn resolve_session_accepts_registry_when_pane_agent_none() {
        let reg = SessionRef {
            agent: "kiro".into(),
            value: "reg-sess".into(),
        };
        let pane = pane_with(None, None);
        assert_eq!(resolve_session(&pane, Some(&reg), &[]).unwrap(), reg);
    }

    #[test]
    fn resolve_session_accepts_registry_on_agent_match() {
        let reg = SessionRef {
            agent: "claude".into(),
            value: "reg-sess".into(),
        };
        let pane = pane_with(Some("claude"), None);
        let argv = vec![vec!["claude".into(), "--resume".into(), "from-argv".into()]];
        assert_eq!(resolve_session(&pane, Some(&reg), &argv).unwrap(), reg);
    }

    #[test]
    fn resolve_session_returns_none_when_nothing_known() {
        let pane = pane_with(Some("claude"), None);
        let argv: Vec<Vec<String>> = vec![vec!["claude".into()]];
        assert!(resolve_session(&pane, None, &argv).is_none());

        let pane = pane_with(None, None);
        let argv = vec![vec!["claude".into(), "--resume".into(), "x".into()]];
        assert!(resolve_session(&pane, None, &argv).is_none());
    }

    #[test]
    fn resume_argv_rejects_unsafe_session_values() {
        let cmds = HashMap::from([("claude".into(), "claude --resume {value}".into())]);
        // Ordinary ids substitute as a single token.
        assert_eq!(
            resume_argv("claude", Some("abc-123"), &cmds).unwrap(),
            vec!["claude", "--resume", "abc-123"]
        );
        // Flag injection via embedded whitespace is rejected.
        assert!(
            resume_argv("claude", Some("x --dangerously-skip-permissions"), &cmds).is_none()
        );
        // Shell metacharacters are rejected.
        assert!(resume_argv("claude", Some("a;b"), &cmds).is_none());
        assert!(resume_argv("claude", Some(""), &cmds).is_none());
        assert!(resume_argv("claude", Some("a\"b"), &cmds).is_none());
        assert!(resume_argv("claude", Some("a\\b"), &cmds).is_none());
    }

    #[test]
    fn kiro_argv_derivation_accepts_both_resume_spellings() {
        let cmds = HashMap::from([("kiro".into(), "kiro-cli chat --resume-id {value}".into())]);
        // Template-derived spelling.
        let argv = vec![vec![
            "kiro-cli".into(),
            "chat".into(),
            "--resume-id".into(),
            "sess-1".into(),
        ]];
        assert_eq!(
            session_from_argv_with_commands("kiro", &argv, &cmds)
                .unwrap()
                .value,
            "sess-1"
        );
        // Live kiro CLIs also spell the flag `--resume`.
        let argv = vec![vec![
            "kiro-cli".into(),
            "chat".into(),
            "--resume".into(),
            "sess-9".into(),
        ]];
        let found = session_from_argv_with_commands("kiro", &argv, &cmds).unwrap();
        assert_eq!(found.agent, "kiro");
        assert_eq!(found.value, "sess-9");
        // Equals form works for the alternate spelling too.
        let argv = vec![vec!["kiro-cli".into(), "chat".into(), "--resume=sess-9".into()]];
        assert_eq!(
            session_from_argv_with_commands("kiro", &argv, &cmds)
                .unwrap()
                .value,
            "sess-9"
        );
        // Bare `--resume` with no following value carries no id.
        let argv = vec![vec!["kiro-cli".into(), "chat".into(), "--resume".into()]];
        assert!(session_from_argv_with_commands("kiro", &argv, &cmds).is_none());
    }

    #[test]
    fn valued_fallback_is_ignored_when_no_value_known() {
        let cmds = HashMap::from([
            ("kiro".into(), "kiro-cli chat --resume-id {value}".into()),
            ("kiro-fallback".into(), "kiro-cli chat --resume {value}".into()),
        ]);
        // Never emit a literal `{value}` token.
        assert!(resume_argv("kiro", None, &cmds).is_none());
    }
}
