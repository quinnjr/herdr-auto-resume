# Auto-Resume Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `quinnjr.auto-resume`, a Herdr plugin in Rust that automatically relaunches dead agent panes (claude, opencode, kiro-cli, …) into their previous sessions, so a crash/BSOD/power-loss never means hand-typing resume commands again.

**Architecture:** Single Rust binary + `herdr-plugin.toml`. `[[startup]]` hook and pane/agent event hooks only ensure a detached per-pane monitor process exists and refresh a durable resume registry; the monitor is the single relauncher (polls `pane get` / `process-info`, relaunches via `pane run` when the agent is dead and the pane is back to an idle shell). Resume commands come from a per-agent table overridable via `config.json`, with valueless-fallback support so `kiro-cli` can resume via `chat -r` when no session id is known.

**Tech Stack:** Rust 1.98 (stable), `serde` + `serde_json` only (parse Herdr CLI JSON envelopes). No HTTP, no async runtime — blocking `std::process::Command` calls to `HERDR_BIN_PATH`.

**Spec:** This plan IS the spec (small project, owner-directed). Prior art studied: `terafin/herdr-restart-always` (Python; requires session value, no kiro entry — our Rust rewrite fixes both).

## Global Constraints

- Plugin id is `quinnjr.auto-resume`; repo root is `~/Projects/herdr-auto-resume`.
- `min_herdr_version = "0.7.5"` (event names `pane.created`, `pane.agent_detected`, `pane.agent_status_changed`, `pane.exited` all exist there).
- `platforms = ["linux", "macos"]` (owner runs Linux; macOS kept for parity, no Windows in v1).
- Herdr CLI is the only API: `pane list|get|process-info|run|split`, `agent list`, `workspace list|create`. Never touch Herdr state files directly.
- NEVER relaunch into a pane that has a live foreground process (anti-double-launch guard is load-bearing).
- TDD: no production code without a failing test first (`cargo test`). Watch each test fail, then pass.

---

### Task 1: Scaffold binary + manifest + config model

**Files:**
- Create: `Cargo.toml`
- Create: `src/main.rs` (CLI dispatch skeleton)
- Create: `src/config.rs` (config load + resume template table)
- Create: `herdr-plugin.toml`
- Test: `src/config.rs` unit tests (`cargo test`)

**Interfaces:**
- Consumes: nothing.
- Produces: `config::Config { poll_seconds: u64, cooldown_seconds: u64, connect_grace_seconds: u64, commands: HashMap<String, String> }`, `config::load() -> Config`, `config::default_commands() -> HashMap<String, String>`, `config::state_dir() -> PathBuf`.

- [ ] **Step 1: Write the failing test** — default command table contains claude, opencode, kiro entries:

```rust
#[test]
fn default_commands_cover_owner_agents() {
    let cmds = default_commands();
    assert_eq!(cmds["claude"], "claude --resume {value}");
    assert_eq!(cmds["opencode"], "opencode --session {value}");
    assert_eq!(cmds["kiro"], "kiro-cli chat --resume-id {value}");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test default_commands_cover_owner_agents`
Expected: FAIL — `default_commands` not defined.

- [ ] **Step 3: Write minimal implementation** — `src/config.rs`:

```rust
use std::collections::HashMap;

pub fn default_commands() -> HashMap<String, String> {
    HashMap::from([
        ("claude".into(), "claude --resume {value}".into()),
        ("opencode".into(), "opencode --session {value}".into()),
        ("codex".into(), "codex resume {value}".into()),
        ("pi".into(), "pi --session {value}".into()),
        ("hermes".into(), "hermes --resume {value}".into()),
        ("kiro".into(), "kiro-cli chat --resume-id {value}".into()),
    ])
}
```

plus `Config` struct and `load()` merging `config.json` `commands` over defaults, and `state_dir()` honoring `HERDR_PLUGIN_CONFIG_DIR`, then `HERDR_PLUGIN_STATE_DIR`, else `~/.config/herdr/plugins/config/quinnjr.auto-resume`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Write `herdr-plugin.toml`** (no test — manifest, allowed exception):

```toml
id = "quinnjr.auto-resume"
name = "Auto Resume"
version = "0.1.0"
min_herdr_version = "0.7.5"
description = "Relaunch dead agent panes into their previous sessions (claude, opencode, kiro-cli, ...). Rust."
platforms = ["linux", "macos"]

[[startup]]
command = ["auto-resume", "startup"]

[[events]]
on = "pane.created"
command = ["auto-resume", "hook-pane"]

[[events]]
on = "pane.agent_detected"
command = ["auto-resume", "hook-pane"]

[[events]]
on = "pane.agent_status_changed"
command = ["auto-resume", "hook-pane"]

[[events]]
on = "pane.exited"
command = ["auto-resume", "hook-pane"]

[[actions]]
id = "supervise-all"
title = "Auto Resume: scan all panes and start monitors"
contexts = ["global"]
command = ["auto-resume", "supervise-all"]

[[actions]]
id = "status"
title = "Auto Resume: status"
contexts = ["global"]
command = ["auto-resume", "status"]

[[actions]]
id = "stop"
title = "Auto Resume: stop monitoring"
contexts = ["global"]
command = ["auto-resume", "stop"]

[[actions]]
id = "logs"
title = "Auto Resume: logs"
contexts = ["global"]
command = ["auto-resume", "logs"]
```

NOTE: `command` argv runs with the plugin dir as cwd; ship the built binary at repo root as `auto-resume` OR use an absolute install path. During dev, `herdr plugin link ~/Projects/herdr-auto-resume` + `cargo build --release` + `cp target/release/auto-resume .` (gitignore the binary). Decide in this task and document in README.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml src/config.rs src/main.rs herdr-plugin.toml .gitignore
git commit -m "feat: scaffold auto-resume plugin with config model"
```

### Task 2: Herdr CLI client (envelope parsing + pane/agent queries)

**Files:**
- Create: `src/herdr.rs`
- Test: `src/herdr.rs` unit tests with fixture JSON copied verbatim from live `herdr pane list` output.

**Interfaces:**
- Consumes: `config::state_dir` (no), env `HERDR_BIN_PATH`.
- Produces: `herdr::invoke(args: &[&str]) -> Option<serde_json::Value>` (returns parsed `result` object), `herdr::pane_list() -> Vec<Pane>`, `herdr::pane_get(id) -> Option<Pane>`, `herdr::pane_run(id, argv) -> bool`, `herdr::process_info(id) -> Option<Value>`, struct `Pane { pane_id: String, agent: Option<String>, agent_status: Option<String>, agent_session: Option<SessionRef>, cwd: Option<String>, workspace_id: Option<String> }`, struct `SessionRef { agent: String, value: String }`.

- [ ] **Step 1: Write the failing test** — envelope with `{"id":"cli:pane:list","result":{"panes":[...]}}` parses to pane list:

```rust
#[test]
fn parses_pane_list_envelope() {
    let raw = r#"{"id":"cli:pane:list","result":{"panes":[{"pane_id":"w7G:p1","agent":"kiro","agent_status":"working"}]}}"#;
    let panes = parse_panes(raw);
    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0].pane_id, "w7G:p1");
    assert_eq!(panes[0].agent.as_deref(), Some("kiro"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test parses_pane_list_envelope`
Expected: FAIL — `parse_panes` not defined.

- [ ] **Step 3: Write minimal implementation** — `invoke()` runs `HERDR_BIN_PATH` (fallback `"herdr"`), scans stdout then stderr for the last JSON line containing `"result"`, returns it. `Pane`/`SessionRef` structs with `#[serde(default)]` so missing `agent`/`agent_session` (e.g. `unknown` shells) parse instead of erroring. `pane_run` returns true when a `result` envelope was received.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test`
Expected: PASS, including a second test with a real captured `unknown`-status pane (no `agent` key) asserting `agent == None`.

- [ ] **Step 5: Commit**

```bash
git add src/herdr.rs
git commit -m "feat: herdr CLI client with envelope parsing"
```

### Task 3: Session-value resolution + resume argv (incl. kiro valueless fallback)

**Files:**
- Create: `src/resume.rs`
- Test: `src/resume.rs` unit tests.

**Interfaces:**
- Consumes: `herdr::Pane`, `config::Config`.
- Produces: `resume::resolve_session(pane, registry_value: Option<&SessionRef>, proc_argv: &[Vec<String>]) -> Option<SessionRef>` with priority live `agent_session` → registry → process argv; `resume::resume_argv(agent, value: Option<&str>, commands) -> Option<Vec<String>>` — if template contains `{value}` a value is required; if it does NOT contain `{value}` (e.g. `"kiro-cli chat -r"`), value is optional (valueless fallback); unknown agent → None.

- [ ] **Step 1: Write the failing tests**

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test resume`
Expected: FAIL — `resume_argv`/`session_from_argv` not defined.

- [ ] **Step 3: Write minimal implementation** — flag derivation: token before `{value}` in the template (`--resume {value}` → `--resume`; `--resume={value}` → `--resume`; bare `resume {value}` → `resume`); scan argv for `flag <value>` and `flag=<value>`; skip values starting with `-`. `resume_argv` uses simple `{value}` string replacement then shell-word splitting (implement a small `split_argv` handling quotes; unit-test it with `"claude --resume \"a b\""`).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/resume.rs
git commit -m "feat: session resolution with kiro valueless fallback"
```

### Task 4: Registry, monitor locks, log

**Files:**
- Create: `src/state.rs`
- Test: `src/state.rs` unit tests using `tempfile`-less temp dirs (`std::env::temp_dir` + unique subdir; no new deps).

**Interfaces:**
- Consumes: `config::state_dir()`.
- Produces: `state::load_registry() -> HashMap<String, SessionRef>`, `state::save_registry`, `state::remember(pane_id, SessionRef)`, `state::monitor_lock_path(pane_id)`, `state::live_monitor_pid(pane_id) -> Option<u32>` (kill(pid,0) via `libc`? NO new deps — read `/proc/<pid>/cmdline` on Linux and match our binary+monitor marker; on macOS fall back to `kill -0` via `std::process::Command`), `state::append_log(line)`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn registry_round_trips() {
    let dir = unique_temp_dir();
    std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", &dir);
    remember("w7G:p1", SessionRef { agent: "kiro".into(), value: "sess-1".into() });
    let reg = load_registry();
    assert_eq!(reg["w7G:p1"].value, "sess-1");
}
```

(Careful: env var mutation is process-global — run state tests single-threaded or save/restore the var inside the test. Save/restore inside the test.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test registry_round_trips`
Expected: FAIL — `remember` not defined.

- [ ] **Step 3: Write minimal implementation** — atomic write via `registry.json.tmp` + rename; `monitors/<pane_id with : → _>.json` lock files `{pid}`; log appends to `log.txt` with timestamp.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/state.rs
git commit -m "feat: durable registry, monitor locks, log"
```

### Task 5: Relaunch decision + monitor loop

**Files:**
- Create: `src/monitor.rs`
- Test: `src/monitor.rs` unit tests on pure decision function with fakes.

**Interfaces:**
- Consumes: `herdr`, `resume`, `state`, `config`.
- Produces: `monitor::should_relaunch(pane: &Pane, proc_argv: &[Vec<String>], cooldown_until: Option<Instant>) -> bool` — true ONLY when: pane foreground is a bare idle shell (every argv is a single-element shell from `{zsh,bash,sh,dash,ksh,fish,nu,pwsh,ash}`), AND (agent_status is unknown/absent OR pane has no agent but registry holds a ref for it), AND cooldown expired. `monitor::run(pane_id)` — the loop: poll every `poll_seconds`, refresh registry while agent alive, relaunch via `pane run` when `should_relaunch`, enforce `cooldown_seconds` after each relaunch, exit on stop-sentinel file.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn relaunches_dead_pane_at_idle_shell() {
    let pane = Pane { pane_id: "w1:p1".into(), agent: None,
        agent_status: Some("unknown".into()), ..Default::default() };
    let argv = vec![vec!["/usr/bin/zsh".into()]];
    assert!(should_relaunch(&pane, &argv, None));
}

#[test]
fn never_relaunches_with_live_foreground() {
    let pane = Pane { pane_id: "w1:p1".into(), agent: Some("claude".into()),
        agent_status: Some("working".into()), ..Default::default() };
    let argv = vec![vec!["claude".into()]];
    assert!(!should_relaunch(&pane, &argv, None));
}

#[test]
fn shell_running_a_script_is_not_idle() {
    let pane = Pane { pane_id: "w1:p1".into(), agent: None,
        agent_status: Some("unknown".into()), ..Default::default() };
    let argv = vec![vec!["/bin/sh".into(), "myscript.sh".into()]];
    assert!(!should_relaunch(&pane, &argv, None));
}
```

(`Pane` needs `Default` — add `#[derive(Default)]` in Task 2's struct; do it in this task if missed.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test monitor`
Expected: FAIL — `should_relaunch` not defined.

- [ ] **Step 3: Write minimal implementation** — pure `should_relaunch` + `run()` loop with stop-sentinel (`stop-<pid>` file check each poll) and `connect_grace_seconds` sleep at start (pane may still be connecting after restore).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS, full suite green.

- [ ] **Step 5: Commit**

```bash
git add src/monitor.rs
git commit -m "feat: relaunch decision and monitor loop"
```

### Task 6: CLI entrypoints + live verification (no relaunch of live agents)

**Files:**
- Modify: `src/main.rs` (dispatch: `startup|hook-pane|supervise-all|status|stop|logs|monitor <pane-id>`)
- Create: `README.md` (install, config.json reference with owner flags, commands table incl. kiro)

**Interfaces:**
- Consumes: all modules.
- Produces: working binary; `status` prints supervised panes + monitor liveness; `supervise-all` spawns monitors (detached via `setsid`-less double-fork: `Command::new(self_exe).arg("monitor")...spawn()` + `stdout/stderr` to `/dev/null`; document that monitors die with the user session — acceptable v1, same as prior art).

- [ ] **Step 1: Build and link**

Run: `cargo build --release && cp target/release/auto-resume . && herdr plugin link ~/Projects/herdr-auto-resume`
Expected: plugin links (may warn about binary — fine).

- [ ] **Step 2: Dry-run status against the LIVE session (read-only)**

Run: `herdr plugin action invoke quinnjr.auto-resume.status`
Expected: lists panes, zero monitors, exit 0. This touches nothing — safe with 12 live agents running.

- [ ] **Step 3: End-to-end on a SCRATCH pane only** — create scratch workspace, open a shell pane, kill its shell so it sits idle-unknown with a registry ref injected? Registry injection is test-only: add `monitor` test-mode? NO — keep it honest: e2e = `supervise-all` then confirm monitors spawn for agent panes and `status` shows them live, then `stop`. Relaunch path is covered by unit tests in Task 5; live relaunch of a real agent is explicitly OUT of scope for verification (owner's agents are doing real work).

- [ ] **Step 4: Commit**

```bash
git add src/main.rs README.md
git commit -m "feat: CLI entrypoints and live status verification"
```

### Task 7: Owner config + handoff

**Files:**
- Modify: `~/.config/herdr/config.toml` (`[session] resume_agents_on_restore = false` — prevents native restore racing the plugin)
- Create: `~/.config/herdr/plugins/config/quinnjr.auto-resume/config.json` with owner commands:

```json
{
  "poll_seconds": 5,
  "cooldown_seconds": 15,
  "connect_grace_seconds": 10,
  "commands": {
    "claude": "claude --resume {value}",
    "opencode": "opencode --session {value}",
    "kiro": "kiro-cli chat --resume-id {value}",
    "kiro-fallback": "kiro-cli chat -r"
  }
}
```

NOTE: owner's `claude --yolo` is a personal wrapper/alias (no `--yolo` in `claude --help`); if they want it on resume, edit the `claude` template to `"claude --yolo --resume {value}"`. `kiro-fallback` key name TBD in implementation — if template has no `{value}`, Task 3 logic already handles it, so the kiro entry itself can just be the fallback when no value is known (implement: try valued template first, then valueless template `<agent>-fallback`? Simplest honest design: `commands["kiro"]` valued, and if value is None use `commands["kiro-fallback"]`. Lock this in during Task 3 implementation and update this task's config accordingly.)

- [ ] **Step 1: Install claude + opencode integrations** (`herdr integration install claude`, `herdr integration install opencode`) — gives native session refs the registry feeds on. Kiro has no integration; detection is native (verified live: `agent:"kiro"`).
- [ ] **Step 2: Write config files, run `supervise-all`, confirm `status` shows monitors for the live kiro/claude/opencode panes.**
- [ ] **Step 3: Update discussion #4207** with a comment pointing at the new repo (owner's call — ask before posting).

## Self-Review

- Spec coverage: auto-resume claude/opencode/kiro (Tasks 1–6), valueless kiro fallback (Task 3), no-touch-live-agents verification (Task 6), owner config (Task 7). OS autostart explicitly out of scope per owner ("don't need herdr itself to auto restart").
- No placeholders: every step names files, exact code/commands, expected results.
- Type consistency: `Pane`, `SessionRef`, `Config` defined once (Tasks 1–2), reused by name in Tasks 3–6.

---

Plan complete and saved to `~/Projects/herdr-auto-resume/docs/superpowers/plans/2026-09-15-auto-resume.md`. Two execution options:

**1. Subagent-Driven (recommended)** — fresh subagent per task, I review between tasks.

**2. Inline Execution** — I execute tasks here with checkpoints.

Which approach?
