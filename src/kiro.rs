//! Kiro session discovery.
//!
//! Live kiro CLIs run with a bare `--resume` (no id on the cmdline), so
//! process-argv derivation never yields a session and the registry stays
//! empty — relaunch then falls back to `kiro-cli chat -r` ("most recent
//! from this directory"), which is wrong with several same-folder
//! sessions. While a kiro pane is alive we instead pin its actual id
//! from `kiro-cli chat -l -f json` (the only source of truth: the v2
//! session store lives outside any on-disk file we may read) and persist
//! it via the registry, so a later crash relaunches the valued
//! `--resume-id` template.
//!
//! The list envelope carries no subagent flag; entries explicitly
//! reporting zero messages (unresumable stubs) are skipped — a missing
//! count (v2 shape) is kept, not treated as zero — and the most recently updated session
//! wins — a live primary re-saves on every turn, so it outranks stale
//! subagent runs while its pane is alive.

use std::time::{Duration, Instant};

/// One resumable kiro session listed for a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KiroSession {
    pub id: String,
    /// `updatedAt` as unix millis.
    pub updated_at_ms: i64,
    pub message_count: u64,
}

fn kiro_bin() -> String {
    crate::config::env_string("KIRO_BIN_PATH").unwrap_or_else(|| "kiro-cli".to_string())
}

/// Parse `2026-09-15T22:32:18.685Z` (millis optional) to unix millis.
/// Strict UTC-only shape; None on any drift (fail closed).
fn parse_updated_at(s: &str) -> Option<i64> {
    let (datetime, _) = s.strip_suffix('Z').map(|d| (d, true))?;
    let (date, time) = datetime.split_once('T')?;
    let mut date_it = date.split('-');
    let (y, m, d): (i64, i64, i64) = (
        date_it.next()?.parse().ok()?,
        date_it.next()?.parse().ok()?,
        date_it.next()?.parse().ok()?,
    );
    if date_it.next().is_some() {
        return None;
    }
    let (hms, millis): (&str, i64) = match time.split_once('.') {
        Some((h, frac)) => {
            // ASCII digits only: a sign or multi-byte char must fail
            // closed, never parse or panic on the byte slice below.
            if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let ms: i64 = format!("{frac:0<3}")[..3].parse().ok()?;
            (h, ms)
        }
        None => (time, 0),
    };
    let mut hms_it = hms.split(':');
    let (hh, mm, ss): (i64, i64, i64) = (
        hms_it.next()?.parse().ok()?,
        hms_it.next()?.parse().ok()?,
        hms_it.next()?.parse().ok()?,
    );
    if hms_it.next().is_some() {
        return None;
    }
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    if hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    Some(days_from_civil(y, m, d) * 86_400_000 + hh * 3_600_000 + mm * 60_000 + ss * 1000 + millis)
}

/// Days since the unix epoch (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Normalise a directory path for envelope matching: trailing `/`
/// trimmed (the root stays `/`).
fn normalize_dir(p: &str) -> &str {
    let t = p.trim_end_matches('/');
    if t.is_empty() && p.starts_with('/') {
        "/"
    } else {
        t
    }
}

/// True when an envelope's `cwd` names the pane's directory: equal
/// after trailing-slash normalisation, equal to the pane cwd's canonical
/// path (kiro reports the canonical cwd of a symlinked pane cwd, whatever
/// its basename), or equal after canonicalising the envelope too.
/// Exact only — an ancestor or descendant folder's sessions are never
/// the pane's. Only envelopes sharing the pane cwd's final component are
/// canonicalised (one stat chain per candidate, not per envelope), so an
/// envelope naming a symlinked alias with a different basename is an
/// accepted miss. A relative envelope cwd never matches (it would
/// resolve against this process's cwd, not kiro's).
fn same_dir(envelope: &str, pane: &str, pane_canonical: Option<&std::path::Path>) -> bool {
    if !envelope.starts_with('/') {
        return false;
    }
    let (envelope, pane) = (normalize_dir(envelope), normalize_dir(pane));
    fn basename(p: &str) -> &str {
        p.rsplit('/').next().unwrap_or_default()
    }
    envelope == pane
        || pane_canonical.is_some_and(|p| std::path::Path::new(envelope) == p)
        || (basename(envelope) == basename(pane)
            && pane_canonical
                .is_some_and(|p| std::fs::canonicalize(envelope).is_ok_and(|e| e == p)))
}

/// Parse one `chat -l -f json` envelope (array of `{cwd, sessions}`) into
/// resumable sessions for `cwd`. Only envelopes naming the pane's
/// directory exactly ([`same_dir`]) are used; a parent, subfolder or
/// unrelated folder's, or one without `cwd`, never is, so with none
/// matching this returns nothing (fail closed: no pin, and relaunch
/// falls back to kiro's own cwd resolution). Entries explicitly
/// reporting zero messages
/// (unresumable stubs), unsafe ids, and unparsable timestamps are
/// skipped (fail closed, survivors kept). A missing `messageCount`
/// (the v2 store omits it entirely) is NOT a stub signal: the entry is
/// kept and ranked by recency like the rest.
pub fn sessions_from_list_output(raw: &str, cwd: &str) -> Vec<KiroSession> {
    let v: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let Some(arr) = v.as_array() else {
        return vec![];
    };
    let pane_canonical = std::fs::canonicalize(cwd).ok();
    let envelopes = arr.iter().filter(|env| {
        env.get("cwd")
            .and_then(|c| c.as_str())
            .is_some_and(|c| same_dir(c, cwd, pane_canonical.as_deref()))
    });
    let mut out = Vec::new();
    for env in envelopes {
        let Some(sessions) = env.get("sessions").and_then(|s| s.as_array()) else {
            continue;
        };
        for s in sessions {
            let Some(id) = s.get("sessionId").and_then(|v| v.as_str()) else {
                continue;
            };
            if !crate::resume::is_safe_session_value(id) {
                continue;
            }
            // Explicit zero means an unresumable stub; a missing count
            // (v2 shape) means unknown, not empty — keep it.
            let count = s.get("messageCount").and_then(|v| v.as_u64());
            if count == Some(0) {
                continue;
            }
            let Some(updated) = s.get("updatedAt").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(updated_at_ms) = parse_updated_at(updated) else {
                continue;
            };
            out.push(KiroSession {
                id: id.to_string(),
                updated_at_ms,
                message_count: count.unwrap_or(0),
            });
        }
    }
    out
}

/// Run `kiro-cli chat -l -f json` in `cwd` with a 10s timeout. No shell
/// is involved, so the cwd cannot inject flags. None on spawn failure,
/// timeout, non-zero exit, or non-UTF8 output.
fn run_list(cwd: &str) -> Option<String> {
    let mut child = std::process::Command::new(kiro_bin())
        .args(["chat", "-l", "-f", "json"])
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let stdout_pipe = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stdout_pipe {
            use std::io::Read;
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let buf = reader.join().unwrap_or_default();
                if !status.success() {
                    return None;
                }
                return String::from_utf8(buf).ok();
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Newest resumable session id for `cwd`, or None when discovery yields
/// nothing usable. Pure selection over `sessions_from_list_output`.
pub fn latest_session_for_cwd(cwd: &str) -> Option<String> {
    let raw = run_list(cwd)?;
    sessions_from_list_output(&raw, cwd)
        .into_iter()
        .max_by_key(|s| s.updated_at_ms)
        .map(|s| s.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mixed live `chat -l -f json` shape (2026-09-15, condukt): a stale
    /// subagent-ish run, zero-message stubs, and the live primary.
    const MIXED_LIST: &str = r#"[{"cwd":"/home/joseph/Projects/Lexmata/condukt","sessions":[
        {"sessionId":"old-subagent-run","source":"v2","title":"review helper","updatedAt":"2026-09-15T20:00:00.000Z","messageCount":30},
        {"sessionId":"stub-no-messages","source":"v2","title":"(no title)","updatedAt":"2026-09-15T22:30:00.000Z","messageCount":0},
        {"sessionId":"live-primary-aaa","source":"v2","title":"real work","updatedAt":"2026-09-15T22:32:18.685Z","messageCount":1729,"status":"in_progress"},
        {"sessionId":"bad id with spaces","source":"v2","title":"evil","updatedAt":"2026-09-15T22:33:00.000Z","messageCount":5}
    ]}]"#;

    const CONDUKT: &str = "/home/joseph/Projects/Lexmata/condukt";

    #[test]
    fn skips_stubs_and_picks_freshest() {
        let sessions = sessions_from_list_output(MIXED_LIST, CONDUKT);
        assert_eq!(sessions.len(), 2);
        let newest = sessions.iter().max_by_key(|s| s.updated_at_ms).unwrap();
        assert_eq!(newest.id, "live-primary-aaa");
        assert_eq!(newest.message_count, 1729);
    }

    #[test]
    fn keeps_v2_entries_without_message_count() {
        // Live v2 shape (2026-09-16): the store omits `messageCount`
        // entirely — all 600+ real sessions parsed from `chat -l`
        // carry only sessionId/source/title/updatedAt. Missing count
        // must not read as a zero-message stub.
        const V2_LIST: &str = r#"[{"cwd":"/home/joseph/Projects/Lexmata/cardozo-ai","sessions":[
            {"sessionId":"7d204a41-9bac-4530-a190-c4bbe12461f5","source":"v2","title":"rework this project to use DeepSeek4.1-flash as the mother model","updatedAt":"2026-09-16T01:02:32.859Z"},
            {"sessionId":"cc417b43-2220-4bc6-b915-d42964ea450c","source":"v2","title":"older review","updatedAt":"2026-09-14T23:33:25.449Z"},
            {"sessionId":"stub-explicit-zero","source":"v2","title":"(no title)","updatedAt":"2026-09-16T02:00:00.000Z","messageCount":0}
        ]}]"#;
        let sessions =
            sessions_from_list_output(V2_LIST, "/home/joseph/Projects/Lexmata/cardozo-ai/");
        assert_eq!(
            sessions.len(),
            2,
            "both uncounted sessions kept: {sessions:?}"
        );
        let newest = sessions.iter().max_by_key(|s| s.updated_at_ms).unwrap();
        assert_eq!(newest.id, "7d204a41-9bac-4530-a190-c4bbe12461f5");
        assert_eq!(newest.message_count, 0, "unreported count defaults to 0");
    }

    #[test]
    fn rejects_garbage_envelopes() {
        assert!(sessions_from_list_output("not json", "x").is_empty());
        assert!(sessions_from_list_output(r#"{"sessions":[]}"#, "x").is_empty());
        assert!(sessions_from_list_output(r#"[{"cwd":"x"}]"#, "x").is_empty());
        assert!(sessions_from_list_output(r#"[{"cwd":"x","sessions":[{"sessionId":"ok-id","updatedAt":"yesterday","messageCount":3}]}]"#, "x").is_empty());
    }

    #[test]
    fn ignores_newer_session_from_another_cwd() {
        // Two envelopes: the newest session lives in a different folder
        // and must never be pinned to this pane.
        const TWO: &str = r#"[
            {"cwd":"/work/a","sessions":[{"sessionId":"a-older","updatedAt":"2026-09-15T20:00:00.000Z","messageCount":3}]},
            {"cwd":"/work/b","sessions":[{"sessionId":"b-newer","updatedAt":"2026-09-15T23:00:00.000Z","messageCount":9}]}
        ]"#;
        let sessions = sessions_from_list_output(TWO, "/work/a/");
        assert_eq!(
            sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["a-older"]
        );
    }

    #[test]
    fn uses_only_envelopes_naming_cwd_exactly() {
        const LIST: &str = r#"[
            {"cwd":"/repo","sessions":[{"sessionId":"repo","updatedAt":"2026-09-15T20:00:00.000Z","messageCount":3}]},
            {"cwd":"/","sessions":[{"sessionId":"root","updatedAt":"2026-09-15T21:00:00.000Z","messageCount":3}]},
            {"cwd":"/repo/sub/vendor","sessions":[{"sessionId":"below","updatedAt":"2026-09-15T22:00:00.000Z","messageCount":3}]},
            {"sessions":[{"sessionId":"no-cwd","updatedAt":"2026-09-15T23:00:00.000Z","messageCount":9}]}
        ]"#;
        let ids = |raw: &str, cwd: &str| {
            sessions_from_list_output(raw, cwd)
                .into_iter()
                .map(|s| s.id)
                .collect::<Vec<_>>()
        };
        // Exact match (trailing slash normalised) is used.
        assert_eq!(ids(LIST, "/repo/"), vec!["repo"]);
        // A parent (`/repo`, `/`) or subfolder envelope never stands in
        // for a `/repo/sub` pane: nothing, so no pin.
        assert!(ids(LIST, "/repo/sub").is_empty());
        // `/repo` and `/repository` are unrelated.
        assert!(ids(LIST, "/repository").is_empty());
    }

    #[test]
    fn matches_cwd_through_symlink() {
        let dir = crate::test_support::unique_temp_dir("kiro-symlink");
        let real = dir.join("real/proj");
        std::fs::create_dir_all(&real).expect("mkdir");
        std::fs::create_dir_all(dir.join("alias")).expect("mkdir");
        let link = dir.join("alias/proj");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        // A differently named alias is an accepted miss (never canonicalised).
        let renamed = dir.join("renamed");
        std::os::unix::fs::symlink(&real, &renamed).expect("symlink");
        let list = format!(
            r#"[{{"cwd":"{}","sessions":[{{"sessionId":"via-link","updatedAt":"2026-09-15T20:00:00.000Z","messageCount":3}}]}},{{"cwd":"{}","sessions":[{{"sessionId":"via-renamed","updatedAt":"2026-09-15T20:00:00.000Z","messageCount":3}}]}}]"#,
            link.display(),
            renamed.display()
        );
        let sessions = sessions_from_list_output(&list, &real.to_string_lossy());
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(
            sessions.into_iter().map(|s| s.id).collect::<Vec<_>>(),
            vec!["via-link"]
        );
    }

    #[test]
    fn relative_envelope_cwd_never_matches() {
        let dir = crate::test_support::unique_temp_dir("kiro-relative");
        let real = dir.join("proj");
        std::fs::create_dir_all(&real).expect("mkdir");
        let canonical = std::fs::canonicalize(&real).ok();
        let pane = real.to_string_lossy().into_owned();
        let hit = same_dir("proj", &pane, canonical.as_deref()) || same_dir("proj", "proj", None);
        std::fs::remove_dir_all(&dir).ok();
        assert!(!hit);
    }

    #[test]
    fn symlinked_pane_cwd_matches_canonical_envelope() {
        // Pane cwd is a symlink whose basename differs from its target;
        // kiro reports the canonical cwd.
        let dir = crate::test_support::unique_temp_dir("kiro-symlink-pane");
        let real = dir.join("real/proj");
        std::fs::create_dir_all(&real).expect("mkdir");
        let pane = dir.join("work");
        std::os::unix::fs::symlink(&real, &pane).expect("symlink");
        let canonical = std::fs::canonicalize(&real).expect("canonicalize");
        let list = format!(
            r#"[{{"cwd":"{}","sessions":[{{"sessionId":"canon","updatedAt":"2026-09-15T20:00:00.000Z","messageCount":3}}]}}]"#,
            canonical.display()
        );
        let sessions = sessions_from_list_output(&list, &pane.to_string_lossy());
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(
            sessions.into_iter().map(|s| s.id).collect::<Vec<_>>(),
            vec!["canon"]
        );
    }

    #[test]
    fn updated_at_rejects_non_digit_fractions() {
        assert!(parse_updated_at("2026-09-15T22:32:18.\u{e9}\u{e9}Z").is_none());
        assert!(parse_updated_at("2026-09-15T22:32:18.-5Z").is_none());
        assert!(parse_updated_at("2026-09-15T22:32:18.Z").is_none());
    }

    #[test]
    fn updated_at_parses_live_shape() {
        // 2026-09-15T22:32:18.685Z
        let ms = parse_updated_at("2026-09-15T22:32:18.685Z").expect("parses");
        assert_eq!(ms % 1000, 685);
        // No-millis form works too.
        let whole = parse_updated_at("2026-09-15T22:32:18Z").expect("parses");
        assert_eq!(whole, ms - 685);
        assert!(parse_updated_at("yesterday").is_none());
        assert!(parse_updated_at("2026-09-15 22:32:18").is_none());
        assert!(parse_updated_at("2026-13-01T00:00:00.000Z").is_none());
    }

    #[test]
    fn latest_session_uses_fake_kiro_bin() {
        use crate::test_support::lock_env;
        let _guard = lock_env();
        let _restore = crate::test_support::EnvRestore::capture(["KIRO_BIN_PATH"]);
        let dir = crate::test_support::unique_temp_dir("kiro-test");
        let fake = dir.join("kiro-cli");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\necho \"$@\" >> \"{log}\"\ncat \"{fixture}\"\n",
                log = dir.join("calls.log").display(),
                fixture = dir.join("list.json").display()
            ),
        )
        .expect("write fake");
        std::fs::write(dir.join("list.json"), MIXED_LIST.replace(CONDUKT, "/tmp"))
            .expect("fixture");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        std::env::set_var("KIRO_BIN_PATH", &fake);
        let result = latest_session_for_cwd("/tmp");
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(result.as_deref(), Some("live-primary-aaa"));
    }

    #[test]
    fn latest_session_none_when_bin_missing() {
        use crate::test_support::lock_env;
        let _guard = lock_env();
        let _restore = crate::test_support::EnvRestore::capture(["KIRO_BIN_PATH"]);
        std::env::set_var("KIRO_BIN_PATH", "/nonexistent-kiro-bin-xyz/kiro-cli");
        assert_eq!(latest_session_for_cwd("/tmp"), None);
    }
}
