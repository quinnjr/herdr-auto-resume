# auto-resume (quinnjr.auto-resume)

Herdr plugin that relaunches dead agent panes into their previous
sessions. Rust, std + `serde`/`serde_json` only.

## How it works

Each supervised agent pane gets a polling **monitor**
(`auto-resume monitor <pane-id>`). Every `poll_seconds` the monitor:

1. Refreshes the durable pane-id → session registry while the agent is
   alive (live `agent_session` → registry → process-argv derivation).
   Kiro panes get one more source: live kiro CLIs run with a bare
   `--resume` (no id), so while a kiro pane is supervised and the
   registry lacks it, the monitor pins the freshest `kiro-cli chat -l
   -f json` session id for the pane cwd (zero-message stubs skipped).
   The live primary re-saves on every turn, so it outranks stale
   subagent runs; two live primaries sharing one folder can still
   cross-pin (same ambiguity as `chat -r`, frozen at pin time instead
   of drifting).
2. Relaunches via `herdr pane run <pane> <command>` **only** when every
   pane foreground process is a bare idle shell and the post-relaunch
   cooldown has expired. Recorded agent/status are ignored (after a
   crash they are stale); resolving the resume command still requires a
   known session (live `agent_session`, registry, or process-argv
   derivation, else a valueless `<agent>-fallback` template), otherwise
   the poll logs `NoResume` and launches nothing. Any live foreground
   process vetoes — failing closed. The command goes as a single
   `COMMAND` value (embedded agent flags stay inert inside it — no
   `--` separator, which herdr types literally into the pane; see
   `pane_run` in `src/herdr.rs` for the verified herdr 0.8.2 rationale).
   Resurrection is one-shot: a successful relaunch spends the saved
   session, so an intentionally-exited agent stays dead after one
   comeback — while a live agent re-saves every poll and re-arms the
   next crash recovery. Failed delivery keeps the entry and retries.
   Valueless fallback relaunches (e.g. `kiro-cli chat -r`) carry no
   session to spend, so they are gated one-shot per monitor instead: a
   later live Refresh (valued session known again) re-arms them. Any
   successful relaunch, valued or fallback, spends the fallback
   one-shot — otherwise a valued comeback plus another death would
   resurrect twice.
   Re-arming needs an observable live session (`agent_session`,
   registry, or argv); an agent that never exposes one is not
   re-armed and stays dead after its single fallback comeback.

Monitors exit on a `stop-<pid>` sentinel (`stop` action). They are
spawned detached but **without `setsid`/double-fork**, so they die with
the user session; `startup` re-spawns them on restart. Same as prior art,
acceptable v1.

## Install

From the marketplace (builds from source — requires `cargo` + Rust stable):

```bash
herdr plugin install quinnjr/herdr-auto-resume
herdr plugin action invoke quinnjr.auto-resume.supervise-all
```

Or develop locally:

```bash
cargo build --release && cp target/release/auto-resume .
herdr plugin link ~/Projects/herdr-auto-resume
```

Plugin commands resolve `./auto-resume` relative to the plugin directory
(Herdr runs them with the plugin dir as cwd), so no PATH wiring is needed.
The binary lives at the repo root (`./auto-resume`) and is gitignored.
Do not commit it; do commit `Cargo.lock` (binary crate).

## Commands

| Command | Trigger | Effect |
|---|---|---|
| `startup` | plugin start | same as `supervise-all` |
| `hook-pane [pane-id]` | `pane.created`, `pane.agent_detected`, `pane.agent_status_changed`, `pane.exited` | records live session, ensures a monitor for that pane (full scan when no id) |
| `supervise-all` | action | spawns a polling monitor per agent pane; never runs `pane run` itself |
| `status` | action | read-only: panes + session + monitor liveness |
| `stop` | action | writes `stop-<pid>` sentinels; monitors exit on next poll |
| `logs [n]` | action | tail of `log.txt` (default 50 lines) |
| `monitor <pane-id>` | spawned internally | the poll loop; carries the pane id as an argv token for the liveness check |

`status` is read-only; `supervise-all` + `stop` only spawn/stop polling
monitors. Live verification never relaunches a real agent (covered by
unit tests instead).

After `stop`, wait one poll and confirm via `status` before
`supervise-all` (monitors exit on their next poll, not instantly).

Monitor liveness on Linux verifies the recorded pid is really
`auto-resume monitor <pane-id>` (argv reuse-guard); on macOS it is
existence-only (no `/proc` argv check).

### Exit codes

`0` success (including read-only `status`/`logs`); `1` when one or more
spawns (`supervise-all`, `hook-pane`) or stop sentinel writes (`stop`)
fail — the failure count is printed; `2` for usage errors (unknown
command, `monitor` without a pane id).

## config.json reference

Location: `herdr plugin config-dir quinnjr.auto-resume`
(`~/.config/herdr/plugins/config/quinnjr.auto-resume/config.json`).
Every field optional; `commands` merge over the defaults. Missing file
or invalid JSON → defaults.

```json
{
  "poll_seconds": 10,
  "cooldown_seconds": 300,
  "connect_grace_seconds": 120,
  "commands": {
    "claude": "claude --resume {value}",
    "opencode": "opencode --session {value}",
    "codex": "codex resume {value}",
    "pi": "pi --session {value}",
    "hermes": "hermes --resume {value}",
    "kiro": "kiro-cli chat --resume-id {value}",
    "kiro-fallback": "kiro-cli chat -r"
  }
}
```

- `{value}` is the session id. Templates **with** `{value}` require a
  known session; templates **without** it are valueless fallbacks usable
  with none. `<agent>-fallback` (e.g. `kiro-fallback`) is preferred over
  a valueless primary template when no value is known.
- Session values are recovered from process argv via the resume flag
  derived from each template (`--resume {value}` → `--resume`, both
  `flag value` and `flag=value` forms).
- `kiro-fallback` ships a valueless default (`kiro-cli chat -r`) used when
  no session value is known. Opt out with `"kiro-fallback": ""`, which
  disables fallback relaunches for kiro (valued `--resume-id` resumes are
  unaffected).

## State

Under the plugin config/state dir: `registry.json` (pane → session),
`monitors/<pane>.json` (monitor locks, `:` → `_`), `stop-<pid>`
sentinels, `log.txt`. Override dir with `HERDR_PLUGIN_CONFIG_DIR`
(tests, dev) or `HERDR_BIN_PATH` (fake `herdr` binary).

Mutable state lives under `state_dir()`: `HERDR_PLUGIN_STATE_DIR` is
preferred, `HERDR_PLUGIN_CONFIG_DIR` is the legacy fallback, else the
default config-tree path. There is no migration step — when only the
config dir is set, state resolves into that same directory, so an
existing `registry.json` keeps working in place.
