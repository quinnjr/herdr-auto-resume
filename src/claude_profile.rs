//! claude-profile integration (https://github.com/quinnjr/claude-code-profiles).
//!
//! claude-profile runs each Claude Code session under a per-profile
//! config dir (`<data>/<name>`, exported as `CLAUDE_CONFIG_DIR`). Herdr
//! only knows the sessions of the config dir it was started with (the
//! default profile), so a pane running another profile reports no
//! `agent_session` and could never resume. While such a pane is alive we
//! read its session id from `<config>/sessions/<pid>.json` (written by
//! Claude Code per process) and remember which profile it ran under; the
//! relaunch then passes `--profile <name>` to the `claude` wrapper when
//! that profile is not the default.

use std::path::{Path, PathBuf};

/// The directory-local profile file claude-profile reads on `cd`.
const DOTFILE: &str = ".claude-profile";

/// claude-profile's data dir (`$XDG_DATA_HOME/claude-profiles`, else
/// `~/.local/share/claude-profiles`), when it is installed (the dir
/// exists).
pub fn data_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    let data = base.join("claude-profiles");
    data.is_dir().then_some(data)
}

/// Profile names claude-profile accepts; also keeps names inert when
/// typed into a shell. `skills` is the shared pool, never a profile.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "skills"
        && !name.starts_with(['.', '-'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// An existing profile named `name` (trimmed), else None.
fn existing(data: &Path, name: &str) -> Option<String> {
    let name = name.trim();
    (valid_name(name) && data.join(name).is_dir()).then(|| name.to_string())
}

/// The default profile name (`<data>/.default`), if set.
pub fn default_profile(data: &Path) -> Option<String> {
    existing(data, &std::fs::read_to_string(data.join(".default")).ok()?)
}

/// Profile name for a config dir that is a managed profile
/// (`<data>/<name>`); None for any other dir.
pub fn profile_of_config_dir(data: &Path, dir: &Path) -> Option<String> {
    if dir.parent()? != data {
        return None;
    }
    existing(data, dir.file_name()?.to_str()?)
}

/// The nearest `.claude-profile` from `cwd` upwards (claude-profile's
/// own lookup), when it names an existing profile.
pub fn dotfile_profile(data: &Path, cwd: &Path) -> Option<String> {
    let text = cwd
        .ancestors()
        .find_map(|d| std::fs::read_to_string(d.join(DOTFILE)).ok())?;
    existing(data, text.lines().next().unwrap_or(""))
}

/// `CLAUDE_CONFIG_DIR` from a live process's environment.
pub fn config_dir_of(pid: u32) -> Option<PathBuf> {
    let environ = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    environ
        .split(|b| *b == 0)
        .find_map(|kv| kv.strip_prefix(b"CLAUDE_CONFIG_DIR="))
        .filter(|v| !v.is_empty())
        .map(|v| PathBuf::from(String::from_utf8_lossy(v).into_owned()))
}

/// The session id Claude Code recorded for `pid` under `config`
/// (`sessions/<pid>.json`); unsafe ids are refused like any session.
pub fn session_of(config: &Path, pid: u32) -> Option<String> {
    let text = std::fs::read_to_string(config.join(format!("sessions/{pid}.json"))).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let id = v.get("sessionId")?.as_str()?;
    crate::resume::is_safe_session_value(id).then(|| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::unique_temp_dir;

    /// A claude-profile data dir with `work` (default) and `personal`.
    fn data_fixture() -> PathBuf {
        let data = unique_temp_dir("claude-profiles");
        for p in ["work", "personal", "skills"] {
            std::fs::create_dir_all(data.join(p)).expect("profile dir");
        }
        // claude-profile writes `.default` without a trailing newline.
        std::fs::write(data.join(".default"), "work").expect("default");
        data
    }

    #[test]
    fn data_dir_follows_xdg_data_home_and_requires_install() {
        let _guard = crate::test_support::lock_env();
        let prev = std::env::var_os("XDG_DATA_HOME");
        let xdg = unique_temp_dir("xdg");
        std::env::set_var("XDG_DATA_HOME", &xdg);
        let missing = data_dir();
        std::fs::create_dir_all(xdg.join("claude-profiles")).expect("data dir");
        let present = data_dir();
        match prev {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        std::fs::remove_dir_all(&xdg).ok();
        assert_eq!(missing, None, "not installed: no data dir");
        assert_eq!(present, Some(xdg.join("claude-profiles")));
    }

    #[test]
    fn default_profile_reads_trimmed_name() {
        let data = data_fixture();
        assert_eq!(default_profile(&data).as_deref(), Some("work"));
        std::fs::write(data.join(".default"), "personal\n").expect("default");
        assert_eq!(default_profile(&data).as_deref(), Some("personal"));
        std::fs::write(data.join(".default"), "").expect("default");
        assert_eq!(default_profile(&data), None);
        std::fs::remove_dir_all(&data).ok();
    }

    #[test]
    fn profile_of_config_dir_accepts_only_managed_profiles() {
        let data = data_fixture();
        assert_eq!(
            profile_of_config_dir(&data, &data.join("personal")).as_deref(),
            Some("personal")
        );
        assert_eq!(
            profile_of_config_dir(&data, &data.join("work/")).as_deref(),
            Some("work"),
            "trailing slash"
        );
        assert_eq!(profile_of_config_dir(&data, &data.join("skills")), None);
        assert_eq!(profile_of_config_dir(&data, &data.join("gone")), None);
        assert_eq!(profile_of_config_dir(&data, Path::new("/home/x/.claude")), None);
        assert_eq!(profile_of_config_dir(&data, &data.join("work/nested")), None);
        std::fs::remove_dir_all(&data).ok();
    }

    #[test]
    fn dotfile_profile_walks_up_to_nearest_existing_profile() {
        let data = data_fixture();
        let tree = unique_temp_dir("tree");
        let deep = tree.join("a/b/c");
        std::fs::create_dir_all(&deep).expect("tree");
        assert_eq!(dotfile_profile(&data, &deep), None);
        std::fs::write(tree.join(DOTFILE), "personal\n").expect("dotfile");
        assert_eq!(dotfile_profile(&data, &deep).as_deref(), Some("personal"));
        std::fs::write(tree.join("a").join(DOTFILE), "work").expect("nearer");
        assert_eq!(dotfile_profile(&data, &deep).as_deref(), Some("work"), "nearest wins");
        std::fs::write(tree.join("a").join(DOTFILE), "nope").expect("bad");
        assert_eq!(dotfile_profile(&data, &deep), None, "unknown profile ignored");
        std::fs::remove_dir_all(&tree).ok();
        std::fs::remove_dir_all(&data).ok();
    }

    #[test]
    fn config_dir_and_session_read_from_live_process() {
        let data = data_fixture();
        let personal = data.join("personal");
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .env("CLAUDE_CONFIG_DIR", &personal)
            .spawn()
            .expect("spawn");
        let pid = child.id();
        // The environ of a just-forked child can still be ours.
        let mut dir = None;
        for _ in 0..200 {
            dir = config_dir_of(pid);
            if dir.as_deref() == Some(personal.as_path()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        child.kill().ok();
        child.wait().ok();
        assert_eq!(dir.as_deref(), Some(personal.as_path()));

        std::fs::create_dir_all(personal.join("sessions")).expect("sessions");
        std::fs::write(
            personal.join("sessions/4242.json"),
            r#"{"pid":4242,"sessionId":"2293c83f-e2ec-4e1b-8114-ba35559fa53f","cwd":"/"}"#,
        )
        .expect("session file");
        assert_eq!(
            session_of(&personal, 4242).as_deref(),
            Some("2293c83f-e2ec-4e1b-8114-ba35559fa53f")
        );
        assert_eq!(session_of(&personal, 4243), None, "no file");
        std::fs::write(personal.join("sessions/4244.json"), r#"{"sessionId":"--evil"}"#)
            .expect("unsafe");
        assert_eq!(session_of(&personal, 4244), None, "unsafe id refused");
        std::fs::remove_dir_all(&data).ok();
    }
}
