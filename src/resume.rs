use std::collections::HashMap;

use crate::herdr::{Pane, SessionRef};

pub const VALUE_PLACEHOLDER: &str = "{value}";
pub const MESSAGE_PLACEHOLDER: &str = "{message}";

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
/// leading `-` (flag-shaped), ASCII whitespace, control chars, quotes,
/// backslash, or shell metacharacters (`; & | < > ( ) $ ` ! * ? [ ] { } ~ #`).
pub(crate) fn is_safe_session_value(v: &str) -> bool {
    !v.is_empty()
        && !v.starts_with('-')
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
/// resume flag derived from `commands` (`flag <value>` and `flag=<value>`).
/// A value failing [`is_safe_session_value`] (flag-shaped, empty, shell
/// metacharacters) counts as no value. Unknown agent or no match → None.
/// Only the template's own flag yields a value: under the default kiro
/// templates kiro-cli's bare `--resume` is a boolean switch, so a token
/// after it (`kiro-cli chat --resume continue`) is no session id — kiro
/// values come from `--resume-id`. A custom template's own flag is
/// always read as valued (`--resume {value}` reads `continue`).
pub fn session_from_argv_with_commands<A: AsRef<[String]>>(
    agent: &str,
    proc_argv: &[A],
    commands: &HashMap<String, String>,
) -> Option<SessionRef> {
    session_for_flag(agent, &template_flag(agent, commands)?, proc_argv)
}

/// `agent`'s resume flag ([`flag_from_template`] of its template), for
/// callers scanning many argvs with one flag ([`session_for_flag`]).
pub(crate) fn template_flag(agent: &str, commands: &HashMap<String, String>) -> Option<String> {
    flag_from_template(commands.get(agent)?)
}

/// Whether a session `value` derived from argv via the agent's
/// configured `template` may override the registry on a live Refresh —
/// a property of the template's flag semantics, not the agent's name.
/// True for most templates: a hand-started `opencode --session NEW`
/// replaces a saved OLD. For a template running `claude` with
/// `--resume` or its short form `-r` (any agent key, e.g. an aliased
/// `claude-work`) only when `value` is a session UUID (8-4-4-4-12 hex,
/// any case): that flag also takes a picker search term
/// (`claude --resume auth`), which argv cannot tell from a session name
/// (`claude --resume my-feature`), but a UUID is never a search term. A
/// non-UUID value is still used when the registry holds none.
pub fn argv_overrides_registry(template: &str, value: &str) -> bool {
    let searchable = template_program(template).as_deref() == Some("claude")
        && matches!(
            flag_from_template(template).as_deref(),
            Some("--resume" | "-r")
        );
    !searchable || is_uuid(value)
}

/// `8-4-4-4-12` hex digits (either case), e.g. a claude session id.
fn is_uuid(value: &str) -> bool {
    let groups: Vec<&str> = value.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, n)| g.len() == n && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// First `flag <value>` / `flag=<value>` across `proc_argv` whose value
/// passes [`is_safe_session_value`]; unsafe values are skipped.
pub(crate) fn session_for_flag<A: AsRef<[String]>>(
    agent: &str,
    flag: &str,
    proc_argv: &[A],
) -> Option<SessionRef> {
    let eq_prefix = format!("{flag}=");
    for argv in proc_argv {
        let argv = argv.as_ref();
        for (i, tok) in argv.iter().enumerate() {
            let value = match tok.strip_prefix(&eq_prefix) {
                Some(rest) => Some(rest),
                None if tok == flag => argv.get(i + 1).map(String::as_str),
                None => None,
            };
            if let Some(v) = value.filter(|v| is_safe_session_value(v)) {
                return Some(SessionRef {
                    agent: agent.to_string(),
                    value: v.to_string(),
                });
            }
        }
    }
    None
}

/// `session_from_argv_with_commands` against the default command templates.
#[cfg(test)]
pub fn session_from_argv(agent: &str, proc_argv: &[Vec<String>]) -> Option<SessionRef> {
    session_from_argv_with_commands(agent, proc_argv, &crate::config::default_commands())
}

/// The `commands` key of the template a relaunch uses: `agent` when a
/// value is known and its template is usable; otherwise a usable
/// `<agent>-fallback` (valueless), else `agent` itself only when its
/// template is usable and valueless. An empty (or whitespace-only)
/// template is an opt-out of that template only ([`usable_template`]):
/// a blank primary leaves the fallback as the agent's only template, a
/// blank fallback disables the fallback. Unknown agent, no usable
/// template, or a valued template with no value → None. The single
/// source of template selection (`resume_command` builds from it).
fn resume_template_key(
    agent: &str,
    value_known: bool,
    commands: &HashMap<String, String>,
) -> Option<String> {
    let template = usable_template(agent, commands);
    if value_known {
        if template.is_some() {
            return Some(agent.to_string());
        }
        if !commands.contains_key(agent) {
            return None;
        }
    }
    let fallback_key = format!("{agent}-fallback");
    if usable_template(&fallback_key, commands).is_some_and(|fb| !fb.contains(VALUE_PLACEHOLDER)) {
        return Some(fallback_key);
    }
    if template?.contains(VALUE_PLACEHOLDER) {
        return None;
    }
    Some(agent.to_string())
}

/// The template under `key` when usable: present and not blank (a blank
/// one opts out of that template, [`resume_template_key`]).
pub(crate) fn usable_template<'a>(
    key: &str,
    commands: &'a HashMap<String, String>,
) -> Option<&'a str> {
    commands
        .get(key)
        .map(String::as_str)
        .filter(|t| !t.trim().is_empty())
}

/// A built resume command: the `commands` key of the template it used
/// (see [`resume_template_key`]), the program that template runs
/// ([`template_program`]) and the argv, so callers never re-derive the
/// template selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeCommand {
    pub key: String,
    pub program: String,
    pub argv: Vec<String>,
}

/// The program a command template runs: its first token's basename.
pub fn template_program(template: &str) -> Option<String> {
    let first = split_argv(template).into_iter().next()?;
    Some(first.rsplit('/').next().unwrap_or(&first).to_string())
}

/// Build the resume command for `agent` ([`ResumeCommand`]). A template containing `{value}` requires a value; a
/// template without it (e.g. `kiro-cli chat -r`) is a valueless fallback
/// usable without one. A value failing [`is_safe_session_value`] is
/// treated as unknown (ignored, never substituted). When no value is known, an explicit
/// `<agent>-fallback` entry (e.g. `kiro-fallback`) is preferred over a
/// valueless primary template. Unknown agent → None.
///
/// A template may also carry `{message}` (e.g. kiro's positional chat
/// input): it is substituted with `message` when set, otherwise the
/// token is dropped so unconfigured templates render exactly as before.
pub fn resume_command(
    agent: &str,
    value: Option<&str>,
    commands: &HashMap<String, String>,
    message: Option<&str>,
) -> Option<ResumeCommand> {
    fn fill(template: &str, value: Option<&str>, message: Option<&str>) -> Vec<String> {
        split_argv(template)
            .into_iter()
            .filter_map(|t| {
                let had_message = t.contains(MESSAGE_PLACEHOLDER);
                let mut t = t;
                if let Some(v) = value {
                    t = t.replace(VALUE_PLACEHOLDER, v);
                }
                t = t.replace(MESSAGE_PLACEHOLDER, message.unwrap_or_default());
                // An unconfigured `{message}` vanishes; an explicit empty
                // template arg (`""`) is preserved.
                if t.is_empty() && had_message {
                    None
                } else {
                    Some(t)
                }
            })
            .collect()
    }
    // An unsafe value counts as unknown: it never selects the valued
    // primary, so a usable valueless fallback still resumes.
    let value = value.filter(|v| is_safe_session_value(v));
    let key = resume_template_key(agent, value.is_some(), commands)?;
    let template = commands.get(&key)?;
    let value = value.filter(|_| template.contains(VALUE_PLACEHOLDER));
    // Split the template first, then substitute per token: the value is
    // never re-split, so it cannot inject extra flags.
    let argv = fill(template, value, message);
    let program = template_program(template)?;
    Some(ResumeCommand { key, program, argv })
}

/// Resolve the session for a dead pane's relaunch: live
/// `agent_session` → registry value → process-argv derivation. The
/// saved value outranks argv: a dead pane's foreground is an idle
/// shell, so argv is only a last resort. Needs the pane's `agent` for
/// the argv step. Live Refresh uses [`resolve_live_session`] instead.
pub fn resolve_session_with_commands<A: AsRef<[String]>>(
    pane: &Pane,
    registry_value: Option<&SessionRef>,
    proc_argv: &[A],
    commands: &HashMap<String, String>,
) -> Option<SessionRef> {
    if let Some(live) = &pane.agent_session {
        return Some(live.clone());
    }
    if let Some(reg) = registry_for_pane(pane, registry_value) {
        return Some(reg.clone());
    }
    let agent = pane.agent.as_deref()?;
    session_from_argv_with_commands(agent, proc_argv, commands)
}

/// Resolve the session of a LIVE agent (Refresh): live `agent_session`
/// (incl. a captured claude-profile session) → `argv_value`, the
/// template's own valued flag in the agent's argv → registry value.
/// `argv_value` must come from the agent's OWN process argv only
/// (never an MCP child's), so a newly started `opencode --session NEW`
/// replaces a saved OLD while a child's `--session X` or kiro's bare
/// `--resume continue` never overwrites it. A fresh `opencode` with no flag keeps the saved
/// value: there is no evidence of a new session.
///
/// The argv value outranks a registry value only when `argv_new`: the
/// monitor saw it appear under a shell it was already watching
/// ([`crate::state::SeenArgv::is_new`]). A flag stays in argv after the
/// agent moves to a newer session (reported once via `agent_session`,
/// then omitted), and a first sighting (new shell, record from an older
/// release) may be just such a stale flag, so otherwise the registry
/// wins. Never for a non-UUID value of a `claude --resume` template
/// either ([`argv_overrides_registry`]: `claude --resume auth` may be a
/// search term). Either way the argv value is still used when the
/// registry holds none.
pub fn resolve_live_session(
    pane: &Pane,
    registry_value: Option<&SessionRef>,
    argv_value: Option<&SessionRef>,
    commands: &HashMap<String, String>,
    argv_new: bool,
) -> Option<SessionRef> {
    if let Some(live) = &pane.agent_session {
        return Some(live.clone());
    }
    let registry = registry_for_pane(pane, registry_value);
    let Some(agent) = pane.agent.as_deref() else {
        return registry.cloned();
    };
    match (argv_value, registry) {
        (Some(argv), Some(reg))
            if !argv_new
                || commands
                    .get(agent)
                    .is_some_and(|t| !argv_overrides_registry(t, &argv.value)) =>
        {
            Some(reg.clone())
        }
        (Some(argv), _) => Some(argv.clone()),
        (None, reg) => reg.cloned(),
    }
}

/// The registry ref, unless it is stale: a cross-agent ref (pane
/// re-created for another agent) is accepted only when the pane names
/// no agent or the same one.
fn registry_for_pane<'a>(
    pane: &Pane,
    registry_value: Option<&'a SessionRef>,
) -> Option<&'a SessionRef> {
    registry_value
        .filter(|reg| pane.agent.is_none() || pane.agent.as_deref() == Some(reg.agent.as_str()))
}

/// `resolve_session_with_commands` against the default command templates.
#[cfg(test)]
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

    /// A claude session id as it appears in argv.
    const ARGV_UUID: &str = "0f3c9a2e-7b41-4d8e-9c55-1a2b3c4d5e6f";

    fn resume_cmd_argv(
        agent: &str,
        value: Option<&str>,
        commands: &HashMap<String, String>,
        message: Option<&str>,
    ) -> Option<Vec<String>> {
        resume_command(agent, value, commands, message).map(|c| c.argv)
    }

    #[test]
    fn kiro_falls_back_to_valueless_resume() {
        let cmds = HashMap::from([("kiro".into(), "kiro-cli chat -r".into())]);
        assert_eq!(
            resume_cmd_argv("kiro", None, &cmds, None).unwrap(),
            vec!["kiro-cli", "chat", "-r"]
        );
    }

    #[test]
    fn valued_template_requires_value() {
        let cmds = HashMap::from([("claude".into(), "claude --resume {value}".into())]);
        assert!(resume_cmd_argv("claude", None, &cmds, None).is_none());
        assert_eq!(
            resume_cmd_argv("claude", Some("abc-123"), &cmds, None).unwrap(),
            vec!["claude", "--resume", "abc-123"]
        );
    }

    #[test]
    fn resume_flag_derived_from_process_argv() {
        // pane ran `claude --resume <uuid>` → value recovered without agent_session
        let argv = vec![vec!["claude".into(), "--resume".into(), ARGV_UUID.into()]];
        let found = session_from_argv("claude", &argv);
        assert_eq!(found.unwrap().value, ARGV_UUID);
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
            resume_cmd_argv("kiro", None, &cmds, None).unwrap(),
            vec!["kiro-cli", "chat", "-r"]
        );
        // Known value → valued primary template.
        assert_eq!(
            resume_cmd_argv("kiro", Some("sess-1"), &cmds, None).unwrap(),
            vec!["kiro-cli", "chat", "--resume-id", "sess-1"]
        );
    }

    #[test]
    fn message_appended_when_template_carries_placeholder() {
        let cmds = HashMap::from([(
            "kiro".into(),
            "kiro-cli chat --resume-id {value} {message}".into(),
        )]);
        // Live-verified 2026-09-16: positional input after `--resume-id`
        // is delivered into the resumed session.
        assert_eq!(
            resume_cmd_argv("kiro", Some("sess-1"), &cmds, Some("continue")).unwrap(),
            vec!["kiro-cli", "chat", "--resume-id", "sess-1", "continue"]
        );
        // Multi-word messages stay one argv token (shell-quoted downstream).
        assert_eq!(
            resume_cmd_argv(
                "kiro",
                Some("sess-1"),
                &cmds,
                Some("pick up where you left off")
            )
            .unwrap(),
            vec![
                "kiro-cli",
                "chat",
                "--resume-id",
                "sess-1",
                "pick up where you left off"
            ]
        );
        // Unset message: placeholder vanishes, argv identical to before.
        assert_eq!(
            resume_cmd_argv("kiro", Some("sess-1"), &cmds, None).unwrap(),
            vec!["kiro-cli", "chat", "--resume-id", "sess-1"]
        );
        // Fallback path honors it too.
        let cmds = HashMap::from([
            (
                "kiro".into(),
                "kiro-cli chat --resume-id {value} {message}".into(),
            ),
            ("kiro-fallback".into(), "kiro-cli chat -r {message}".into()),
        ]);
        assert_eq!(
            resume_cmd_argv("kiro", None, &cmds, Some("continue")).unwrap(),
            vec!["kiro-cli", "chat", "-r", "continue"]
        );
        assert_eq!(
            resume_cmd_argv("kiro", None, &cmds, None).unwrap(),
            vec!["kiro-cli", "chat", "-r"]
        );
    }

    #[test]
    fn message_absent_without_placeholder() {
        // Templates without `{message}` ignore it entirely.
        let cmds = HashMap::from([("claude".into(), "claude --resume {value}".into())]);
        assert_eq!(
            resume_cmd_argv("claude", Some("abc"), &cmds, Some("continue")).unwrap(),
            vec!["claude", "--resume", "abc"]
        );
    }

    #[test]
    fn explicit_empty_template_arg_preserved() {
        // Only `{message}`-derived empties drop; an explicit `""` stays.
        let cmds = HashMap::from([("x".into(), "x \"\" {message}".into())]);
        assert_eq!(
            resume_cmd_argv("x", None, &cmds, None).unwrap(),
            vec!["x", ""]
        );
    }

    #[test]
    fn unknown_agent_returns_none() {
        let cmds = HashMap::from([("claude".into(), "claude --resume {value}".into())]);
        assert!(resume_cmd_argv("nope", Some("v"), &cmds, None).is_none());
        assert!(resume_cmd_argv("nope", None, &cmds, None).is_none());
    }

    #[test]
    fn flag_from_attached_template_matches_both_argv_forms() {
        assert_eq!(
            flag_from_template("x --session={value}").as_deref(),
            Some("--session")
        );
        let cmds = HashMap::from([("x".into(), "x --session={value}".into())]);
        for argv in [
            vec!["x".to_string(), "--session=abc".into()],
            vec!["x".to_string(), "--session".into(), "abc".into()],
        ] {
            assert_eq!(
                session_from_argv_with_commands("x", std::slice::from_ref(&argv), &cmds)
                    .map(|s| s.value),
                Some("abc".to_string()),
                "argv {argv:?}"
            );
        }
    }

    #[test]
    fn resume_command_reports_template_key() {
        let cmds = crate::config::default_commands();
        assert_eq!(
            resume_command("kiro", None, &cmds, None)
                .map(|c| c.key)
                .as_deref(),
            Some("kiro-fallback")
        );
        assert_eq!(
            resume_command("kiro", Some("s1"), &cmds, None)
                .map(|c| c.key)
                .as_deref(),
            Some("kiro")
        );
        assert!(resume_command("claude", None, &cmds, None).is_none());
    }

    #[test]
    fn session_from_argv_supports_equals_form_and_skips_flags() {
        let argv = vec![vec!["claude".into(), format!("--resume={ARGV_UUID}")]];
        assert_eq!(session_from_argv("claude", &argv).unwrap().value, ARGV_UUID);

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

    /// [`resolve_live_session`] against the default templates, for an
    /// argv flag the monitor saw appear (new evidence).
    fn resolve_live(pane: &Pane, reg: Option<&SessionRef>, own: &[Vec<String>]) -> SessionRef {
        resolve_with(pane, reg, own, true)
    }

    /// [`resolve_live_session`] with `argv_new` given.
    fn resolve_with(
        pane: &Pane,
        reg: Option<&SessionRef>,
        own: &[Vec<String>],
        argv_new: bool,
    ) -> SessionRef {
        let commands = crate::config::default_commands();
        let agent = pane.agent.as_deref().unwrap_or_default();
        let own = session_from_argv_with_commands(agent, own, &commands);
        resolve_live_session(pane, reg, own.as_ref(), &commands, argv_new)
            .expect("a session resolves")
    }

    fn argv(toks: &[&str]) -> Vec<String> {
        toks.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn refresh_prefers_agent_own_flag_over_registry() {
        // A hand-started `opencode --session NEW` replaces a saved OLD.
        let old = SessionRef {
            agent: "opencode".into(),
            value: "OLD".into(),
        };
        let pane = pane_with(Some("opencode"), None);
        let own = vec![argv(&["opencode", "--session", "NEW"])];
        assert_eq!(resolve_live(&pane, Some(&old), &own).value, "NEW");
        // A fresh `opencode` with no flag carries no evidence of a new
        // session: the saved one is kept.
        let own = vec![argv(&["opencode"])];
        assert_eq!(resolve_live(&pane, Some(&old), &own), old);
        // The live agent_session still outranks both.
        let live = SessionRef {
            agent: "opencode".into(),
            value: "LIVE".into(),
        };
        let pane = pane_with(Some("opencode"), Some(live.clone()));
        let own = vec![argv(&["opencode", "--session", "NEW"])];
        assert_eq!(resolve_live(&pane, Some(&old), &own), live);
    }

    #[test]
    fn refresh_keeps_registry_over_flag_not_seen_appearing() {
        // A `--session OLD` the monitor did not see appear (already seen,
        // first sighting in a new shell, record from an older release)
        // may be stale: the saved NEW is kept.
        let newer = SessionRef {
            agent: "opencode".into(),
            value: "NEW".into(),
        };
        let pane = pane_with(Some("opencode"), None);
        let own = vec![argv(&["opencode", "--session", "OLD"])];
        assert_eq!(resolve_with(&pane, Some(&newer), &own, false), newer);
        // With nothing saved, the argv value is still used.
        assert_eq!(resolve_with(&pane, None, &own, false).value, "OLD");
        // Seen appearing, it wins.
        assert_eq!(resolve_with(&pane, Some(&newer), &own, true).value, "OLD");
        // Same for a claude UUID.
        let claude = pane_with(Some("claude"), None);
        let newer = SessionRef {
            agent: "claude".into(),
            value: "1a2b3c4d-0000-4000-8000-000000000000".into(),
        };
        let own = vec![argv(&["claude", "--resume", ARGV_UUID])];
        assert_eq!(resolve_with(&claude, Some(&newer), &own, false), newer);
    }

    #[test]
    fn refresh_keeps_registry_over_claude_resume_argv() {
        // `claude --resume auth` may be a picker search term: a non-UUID
        // claude argv value never overrides a saved session on Refresh.
        let saved = SessionRef {
            agent: "claude".into(),
            value: "abc".into(),
        };
        let pane = pane_with(Some("claude"), None);
        for own in [
            vec![argv(&["claude", "--resume", "auth"])],
            vec![argv(&["claude", "--resume=auth"])],
        ] {
            assert_eq!(resolve_live(&pane, Some(&saved), &own), saved);
        }
        // A UUID is never a search term: it overrides (either case).
        let own = vec![argv(&["claude", "--resume", ARGV_UUID])];
        assert_eq!(resolve_live(&pane, Some(&saved), &own).value, ARGV_UUID);
        let upper = ARGV_UUID.to_ascii_uppercase();
        let own = vec![argv(&["claude", "--resume", &upper])];
        assert_eq!(resolve_live(&pane, Some(&saved), &own).value, upper);
        // With no saved claude value, the argv value — a search term or
        // named session too — is remembered.
        let own = vec![argv(&["claude", "--resume", "auth"])];
        assert_eq!(resolve_live(&pane, None, &own).value, "auth");
        let own = vec![argv(&["claude", "--resume", "my-feature"])];
        assert_eq!(resolve_live(&pane, None, &own).value, "my-feature");
        let opencode = SessionRef {
            agent: "opencode".into(),
            value: "OLD".into(),
        };
        assert_eq!(
            resolve_live(&pane, Some(&opencode), &own).value,
            "my-feature"
        );
        // Relaunch derivation accepts a named session the same way.
        let dead = vec![argv(&["claude", "--resume", "my-feature"])];
        assert_eq!(
            resolve_session(&pane, None, &dead).map(|s| s.value),
            Some("my-feature".into())
        );
    }

    #[test]
    fn argv_override_is_per_template() {
        let commands = crate::config::default_commands();
        let claude = commands["claude"].as_str();
        assert!(!argv_overrides_registry(claude, "auth"));
        assert!(argv_overrides_registry(claude, ARGV_UUID));
        for not_uuid in [
            "0f3c9a2e-7b41-4d8e-9c55-1a2b3c4d5e6",
            "0f3c9a2e-7b41-4d8e-9c55-1a2b3c4d5e6f-0",
            "0f3c9a2g-7b41-4d8e-9c55-1a2b3c4d5e6f",
            "0f3c9a2e7b414d8e9c551a2b3c4d5e6f",
        ] {
            assert!(!argv_overrides_registry(claude, not_uuid), "{not_uuid}");
        }
        for agent in ["opencode", "codex", "kiro", "pi", "hermes"] {
            assert!(argv_overrides_registry(&commands[agent], "auth"), "{agent}");
        }
        // Keyed on the template, not the agent name: an aliased key
        // running `claude --resume` gets the rule (full path included),
        // a claude template with another flag does not.
        assert!(!argv_overrides_registry(
            "/usr/bin/claude --resume={value}",
            "auth"
        ));
        assert!(argv_overrides_registry("claude -c {value}", "auth"));
        // Claude's short `-r` is the same searchable flag.
        assert!(!argv_overrides_registry("claude -r {value}", "auth"));
        assert!(argv_overrides_registry("claude -r {value}", ARGV_UUID));
        let mut aliased = commands.clone();
        aliased.insert("claude-work".into(), "claude --resume {value}".into());
        let pane = Pane {
            agent: Some("claude-work".into()),
            ..Default::default()
        };
        let saved = SessionRef {
            agent: "claude-work".into(),
            value: "saved".into(),
        };
        let live = vec![argv(&["claude", "--resume", "auth"])];
        assert_eq!(
            resolve_live_session(
                &pane,
                Some(&saved),
                session_from_argv_with_commands("claude-work", &live, &aliased).as_ref(),
                &aliased,
                true
            )
            .map(|s| s.value),
            Some("saved".into())
        );
        // A `claude -r {value}` template running `claude -r auth`: the
        // registry is kept too.
        aliased.insert("claude-work".into(), "claude -r {value}".into());
        let short = vec![argv(&["claude", "-r", "auth"])];
        assert_eq!(
            resolve_live_session(
                &pane,
                Some(&saved),
                session_from_argv_with_commands("claude-work", &short, &aliased).as_ref(),
                &aliased,
                true
            )
            .map(|s| s.value),
            Some("saved".into())
        );
        // Argv values still pass the substitution safety rule.
        let flagged = vec![argv(&["claude", "--resume", "--verbose"])];
        assert!(session_from_argv("claude", &flagged).is_none());
        let unsafe_value = vec![argv(&["claude", "--resume", "a;b"])];
        assert!(session_from_argv("claude", &unsafe_value).is_none());
    }

    #[test]
    fn refresh_keeps_registry_over_kiro_bare_resume() {
        // Kiro bare `--resume continue` is no session flag: the pinned
        // registry wins.
        let pinned = SessionRef {
            agent: "kiro".into(),
            value: "pinned".into(),
        };
        let kiro = pane_with(Some("kiro"), None);
        let own = vec![argv(&["kiro-cli", "chat", "--resume", "continue"])];
        assert_eq!(resolve_live(&kiro, Some(&pinned), &own), pinned);
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
        let argv = vec![vec!["claude".into(), "--resume".into(), ARGV_UUID.into()]];
        let pane = pane_with(Some("claude"), Some(live.clone()));
        assert_eq!(resolve_session(&pane, Some(&reg), &argv).unwrap(), live);

        let pane = pane_with(Some("claude"), None);
        assert_eq!(resolve_session(&pane, Some(&reg), &argv).unwrap(), reg);

        let pane = pane_with(Some("claude"), None);
        let found = resolve_session(&pane, None, &argv).unwrap();
        assert_eq!(found.value, ARGV_UUID);
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
        let argv = vec![vec!["claude".into(), "--resume".into(), ARGV_UUID.into()]];
        let found = resolve_session(&pane, Some(&reg), &argv).unwrap();
        assert_eq!(found.agent, "claude");
        assert_eq!(found.value, ARGV_UUID);

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
        let argv = vec![vec!["claude".into(), "--resume".into(), ARGV_UUID.into()]];
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
            resume_cmd_argv("claude", Some("abc-123"), &cmds, None).unwrap(),
            vec!["claude", "--resume", "abc-123"]
        );
        // Flag injection via embedded whitespace is rejected.
        assert!(resume_cmd_argv(
            "claude",
            Some("x --dangerously-skip-permissions"),
            &cmds,
            None
        )
        .is_none());
        // Shell metacharacters are rejected.
        assert!(resume_cmd_argv("claude", Some("a;b"), &cmds, None).is_none());
        assert!(resume_cmd_argv("claude", Some(""), &cmds, None).is_none());
        assert!(resume_cmd_argv("claude", Some("a\"b"), &cmds, None).is_none());
        assert!(resume_cmd_argv("claude", Some("a\\b"), &cmds, None).is_none());
    }

    #[test]
    fn kiro_argv_derivation_uses_only_resume_id() {
        let cmds = HashMap::from([("kiro".into(), "kiro-cli chat --resume-id {value}".into())]);
        let kiro = |rest: &[&str]| -> Vec<Vec<String>> {
            let mut a = vec!["kiro-cli".to_string(), "chat".to_string()];
            a.extend(rest.iter().map(|s| s.to_string()));
            vec![a]
        };
        // Template-derived spelling, separate and equals forms.
        let found =
            session_from_argv_with_commands("kiro", &kiro(&["--resume-id", "sess-1"]), &cmds)
                .unwrap();
        assert_eq!(found.agent, "kiro");
        assert_eq!(found.value, "sess-1");
        assert_eq!(
            session_from_argv_with_commands("kiro", &kiro(&["--resume-id=sess-2"]), &cmds)
                .unwrap()
                .value,
            "sess-2"
        );
        // kiro-cli's bare `--resume` is a switch: the next token (a chat
        // message such as `continue`) is never a session id.
        for rest in [
            &["--resume", "continue"][..],
            &["--resume=sess-9"][..],
            &["--resume"][..],
        ] {
            assert!(session_from_argv_with_commands("kiro", &kiro(rest), &cmds).is_none());
        }
    }

    #[test]
    fn valued_fallback_is_ignored_when_no_value_known() {
        let cmds = HashMap::from([
            ("kiro".into(), "kiro-cli chat --resume-id {value}".into()),
            (
                "kiro-fallback".into(),
                "kiro-cli chat --resume {value}".into(),
            ),
        ]);
        // Never emit a literal `{value}` token.
        assert!(resume_cmd_argv("kiro", None, &cmds, None).is_none());
    }

    #[test]
    fn rejects_flag_shaped_session_values() {
        assert!(!is_safe_session_value("--dangerously-skip-permissions"));
    }

    #[test]
    fn valueless_template_ignores_unused_value_safety() {
        let cmds = HashMap::from([("claude".into(), "claude --continue".into())]);
        assert_eq!(
            resume_cmd_argv("claude", Some("a;b"), &cmds, None).unwrap(),
            vec!["claude", "--continue"]
        );
        assert_eq!(
            resume_cmd_argv("claude", Some("--evil"), &cmds, None).unwrap(),
            vec!["claude", "--continue"]
        );
    }

    #[test]
    fn empty_primary_template_disables_resume() {
        // `"pi": ""` is an opt-out: never a Relaunch with an empty argv.
        let mut cmds = crate::config::default_commands();
        cmds.insert("pi".into(), "".into());
        assert!(resume_command("pi", Some("sess-1"), &cmds, None).is_none());
        assert!(resume_command("pi", None, &cmds, None).is_none());
        cmds.insert("pi".into(), "  ".into());
        assert!(resume_command("pi", Some("sess-1"), &cmds, None).is_none());
    }

    #[test]
    fn empty_primary_keeps_fallback() {
        // A blank primary opts out of the primary only: the fallback is
        // the agent's one usable template, value known or not.
        let cmds = HashMap::from([
            ("kiro".into(), "".into()),
            ("kiro-fallback".into(), "kiro-cli chat -r {message}".into()),
        ]);
        for value in [None, Some("sess-1")] {
            let cmd = resume_command("kiro", value, &cmds, Some("go")).expect("fallback");
            assert_eq!(cmd.key, "kiro-fallback");
            assert_eq!(cmd.argv, vec!["kiro-cli", "chat", "-r", "go"]);
        }
        // Both blank → no resume at all.
        let cmds = HashMap::from([
            ("kiro".into(), " ".into()),
            ("kiro-fallback".into(), "".into()),
        ]);
        assert!(resume_command("kiro", None, &cmds, None).is_none());
        assert!(resume_command("kiro", Some("sess-1"), &cmds, None).is_none());
    }

    #[test]
    fn unsafe_value_uses_valueless_fallback() {
        // An unsafe value is unknown: the valued primary is skipped, not
        // a dead end, so the valueless fallback still resumes.
        let cmds = HashMap::from([
            ("kiro".into(), "kiro-cli chat --resume-id {value}".into()),
            ("kiro-fallback".into(), "kiro-cli chat -r".into()),
        ]);
        let cmd = resume_command("kiro", Some("a;b"), &cmds, None).expect("fallback");
        assert_eq!(cmd.key, "kiro-fallback");
        assert_eq!(cmd.argv, vec!["kiro-cli", "chat", "-r"]);
    }

    #[test]
    fn empty_fallback_disables_fallback() {
        let cmds = HashMap::from([
            ("kiro".into(), "kiro-cli chat --resume-id {value}".into()),
            ("kiro-fallback".into(), "".into()),
        ]);
        assert!(resume_cmd_argv("kiro", None, &cmds, None).is_none());
    }
}
