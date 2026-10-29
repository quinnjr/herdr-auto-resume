# auto-resume (quinnjr.auto-resume)

Herdr plugin that relaunches dead agent panes into their previous
sessions. Rust, std + `serde`/`serde_json` only.

## How it works

Each supervised agent pane gets a polling **monitor**
(`auto-resume monitor <pane-id>`). Every `poll_seconds` the monitor:

1. Refreshes the durable pane-id → session registry while the agent is
   alive. A live Refresh resolves the session as: live `agent_session`
   (a claude session captured from a managed claude-profile counts as
   one) → the template's own session flag in the argv of the agent's
   **own** foreground process (`opencode --session NEW`) → registry.
   Only the agent's process counts and only via its template flag, so
   an MCP child's `--session X` or (under the default kiro template)
   kiro's bare `--resume continue` never overwrites a saved session, while a session started by hand
   with its flag replaces it. A template running `claude --resume`
   (or `-r`; whatever its agent key) is the exception: its argv
   replaces a saved session only when the value is a session UUID
   (`claude --resume auth` may be a picker search term,
   indistinguishable from a named session); any other claude argv value
   is used only when nothing is saved (`claude --resume my-feature` then
   remembers `my-feature` — so a picker search term is persisted when
   nothing else is saved, as before). A flag lingers in argv after the
   agent moves on to a newer session (herdr may report the new one
   once, then omit it), so argv replaces a saved session only when the
   monitor saw it appear in a shell it was already watching: a new
   agent process (new pid, e.g. started by hand after a user exit —
   even when the exited one ran without a flag, like a bare `opencode`
   or kiro's `chat -r`), or the same process's flag value changing. A
   first sighting (a new
   shell after a host restart, a record written before this rule, an
   entry herdr reports without a pid) or the value the monitor itself
   relaunched keeps the saved session; argv is then used only when
   nothing is saved. The last seen process (pid, and flag value if any)
   is persisted on
   the pane's `shells.json` record (kept while the same shell lives,
   across a user exit; cleared for a different agent), so the guard
   survives a monitor restart. A managed claude-profile capture applies
   only when the pane's agent runs `claude` (its template's program, so
   an aliased key like `claude-work` with a `claude --resume {value}`
   template counts too), so a `claude -p` subprocess in an opencode or
   codex pane never takes over that pane's session.
   Residual case: a fresh agent started
   without a session flag (a bare `opencode`) while herdr reports no
   `agent_session` keeps the saved session — there is no evidence of
   a new one.
   Kiro panes get one more source: live kiro CLIs run with a bare
   `--resume` (no id), so for any agent key whose template runs
   `kiro-cli` (`kiro`, or an alias like `kiro-work`; only while its
   primary template is valued, never one blank or valueless), while
   `kiro-cli` is actually in the pane foreground, the registry holds no ref of that agent for it (a stale ref
   left by another agent is overwritten), and its argv names no
   `--resume-id <id>` on `kiro-cli` itself (that id is remembered
   directly, no lookup), the monitor pins the freshest `kiro-cli chat -l -f json` session id
   listed for the pane cwd (only envelopes naming exactly that cwd,
   after trailing-slash normalisation or symlink resolution — an
   envelope naming the pane cwd's canonical path always matches; an
   envelope's own path is resolved only when it shares the pane cwd's
   final component, so an envelope naming an alias under a different
   name is missed; a relative envelope cwd, a parent,
   subfolder or unrelated folder's never — with none matching nothing
   is pinned and a relaunch uses `kiro-fallback`, kiro's own cwd
   resolution, because a parent folder's session pinned to a subfolder
   pane would stick there permanently)
   (zero-message stubs skipped; a failed lookup, or a found id that
   could not be saved, is retried every 6th poll).
   The live primary re-saves on every turn, so it outranks stale
   subagent runs; two live primaries sharing one folder can still
   cross-pin (same ambiguity as `chat -r`, frozen at pin time instead
   of drifting). Once pinned the ref is never looked up again: a
   session switched inside a running kiro is not picked up (unless
   herdr reports it) until a relaunch spends the pin.
2. Relaunches via `herdr pane run <pane> <command>` **only** when every
   pane foreground process is a bare idle shell and the post-relaunch
   cooldown has expired. Recorded agent/status are ignored (after a
   crash they are stale); resolving the resume command still requires a
   known session (live `agent_session` → registry → process-argv
   derivation, else a valueless `<agent>-fallback` template), otherwise
   the poll logs `NoResume` and launches nothing. Any live foreground
   process vetoes — failing closed (so does a shell reported by name
   only, with no readable argv: it may be running a script). The
   command goes as a single `COMMAND` value (embedded agent flags stay inert inside it — no
   `--` separator, which herdr types literally into the pane; see
   `pane_run` in `src/herdr.rs` for the verified herdr 0.8.2 rationale).
   Resurrection is **host-restart-gated**: an idle shell looks the same
   after a Ctrl-C and after the session host crashed, but the shell
   itself does not. While the agent runs, the monitor records the
   pane's shell identity (`shell_pid` from `process-info`, plus boot id
   and process start time on Linux so a recycled pid or a reboot never
   matches). When the pane goes idle:
   - **different shell** than the one the agent was seen in → the host
     crashed or restarted → resume;
   - **same shell** → the agent exited inside it (Ctrl-C, `/exit`, a
     clean quit, or the agent itself dying) → stay dead, and the record
     is retired so a later host restart does not resurrect it either;
   - **no record** (never seen running) or **no shell id** → stay dead
     (fail closed).
   A relaunch pins the current shell, so the resumed agent's next
   in-place death is a user exit again: one comeback per host restart.
   A successful relaunch also spends the saved session (failed delivery
   keeps it and retries); a later live Refresh re-arms it.
3. When `resume_message` is set, the restored session also receives it
   as a chat message: templates carrying `{message}` (kiro) deliver it
   inline as chat input on resume; other templates get it two-step —
   typed and submitted after a few polls once the TUI is plausibly
   booted (opencode verified live; its `--prompt` neither submits nor
   pre-fills on resume; it also reaches name-only `claude` and
   node-launched CLIs). The submit is skipped while the foreground is
   idle (never type chat text into a shell) and attempted once; a
   pending message not delivered within 10 polls past boot is dropped
   with a log line.

Monitors exit on a `stop-<pid>` sentinel (`stop` action), or when they
find their pane's lock positively naming another live monitor for the
pane (checked every grace second and every poll, from one read of the
lock). A lock that vanished under a running monitor, or names a dead
pid, or is corrupt, is re-claimed instead; a re-claim that fails on
I/O is logged once and retried next poll, never treated as a loss. On
exit a monitor deletes its lock only while it still names its own pid
(compare-and-delete: the lock is re-read immediately before the
unlink). A sub-millisecond race remains — a claim landing between that
read and the unlink is deleted — and self-heals, because the owning
monitor re-claims a missing lock on its next poll. They are
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
| `hook-pane [pane-id]` | `pane.created`, `pane.agent_detected`, `pane.agent_status_changed`, `pane.exited` | records live session, ensures a monitor for that pane — any pane with an agent, or an agentless one whose session is recorded (a busy lock retries that write once with the normal ~5s wait) (full scan when no id) |
| `supervise-all` | action | spawns a polling monitor per agent pane; never runs `pane run` itself |
| `status` | action | read-only: panes + session + monitor liveness |
| `stop` | action | writes `stop-<pid>` sentinels; monitors exit on next poll |
| `logs [n]` | action | tail of `log.txt` (default 50 lines), topped up from `log.txt.1` right after a rotation |
| `monitor <pane-id>` | spawned internally | the poll loop; carries the pane id as an argv token for the liveness check |

`status` is read-only; `supervise-all` + `stop` only spawn/stop polling
monitors. Live verification never relaunches a real agent (covered by
unit tests instead).

After `stop`, wait one poll and confirm via `status` before
`supervise-all` (monitors exit on their next poll, not instantly).

Monitor liveness verifies the recorded pid is really
`auto-resume monitor <pane-id>` (argv reuse-guard): via
`/proc/<pid>/cmdline` on Linux, via `/bin/ps -p <pid> -o command=` on
macOS. `status` also lists stale locks (pid no longer a monitor) and
unreadable/corrupt lock files. Each lock file is judged on its own
pid, and `stop` removes just the stale file — with the same
compare-and-delete as a monitor's exit (re-read the lock, unlink only
while it still names the stale pid), so a lock a new monitor claimed
since the scan is kept (and, when that new claimant is a live
monitor for the pane, sent a stop sentinel too); a claim landing in the sub-millisecond gap
between that re-read and the unlink is lost but self-heals, as its
owner re-claims a missing lock on its next poll. A stale lock that cannot be removed counts as a
`stop` failure. Unreadable/corrupt lock files are skipped and left in
place (not failures: no sentinel can reach them); `stop` reports how
many in its summary line.

### Exit codes

`0` success (including read-only `status`/`logs`); `1` when one or more
spawns (`supervise-all`, `hook-pane`) or stop sentinel writes and
stale-lock removals (`stop`) fail — the failure count is printed; `2` for usage errors (unknown
command, `monitor` without a pane id).

## claude-profile

When [claude-profile](https://github.com/quinnjr/claude-code-profiles) is
installed (`$XDG_DATA_HOME/claude-profiles`, else
`~/.local/share/claude-profiles`), Claude panes resume under the profile
they ran in (any agent whose template runs `claude`, e.g. an aliased
`claude-work` key, is treated the same):

- While Claude runs, the monitor reads its `CLAUDE_CONFIG_DIR` from
  `/proc/<pid>/environ` and records which profile it is. Herdr only sees
  the sessions of the config dir it was started with (normally the default
  profile), so for other profiles the session id is read from
  `<profile>/sessions/<pid>.json` instead.
- On a host restart, a non-default profile resumes as
  `claude --profile <name> --resume <id>` (the flag goes right after
  argv[0], which must be the `claude` binary; a launcher template such
  as `env FOO=1 claude ...` relaunches without the profile and logs a
  warning — use a direct-binary template). This uses the `claude` wrapper's
  own flag, typed into the pane's shell, so the wrapper must be loaded
  there. The default profile resumes as plain `claude --resume <id>`.
- If the pane was never seen running, the nearest `.claude-profile`
  from the pane's cwd upwards names the profile.

Claude running under a config dir that is not a managed profile gets no
session capture: a resume could not reach its sessions.

## config.json reference

Location: `herdr plugin config-dir quinnjr.auto-resume`
(`~/.config/herdr/plugins/config/quinnjr.auto-resume/config.json`);
`$HERDR_PLUGIN_CONFIG_DIR/config.json` overrides it when the env var is
set. Every field optional; `commands` merge over the defaults. Missing
file or invalid JSON → defaults. A wrongly typed field (or `commands`
entry, e.g. `5`) is skipped on its own (reported once per `startup` /
`supervise-all`, on stderr and in `log.txt`); the other
overrides still apply. JSON `null` (for any field or `commands` entry)
reads as absent and silently keeps the default; `""` stays a
`commands` opt-out.

```json
{
  "poll_seconds": 10,
  "cooldown_seconds": 300,
  "connect_grace_seconds": 120,
  "resume_message": "continue",
  "commands": {
    "claude": "claude --resume {value}",
    "opencode": "opencode --session {value}",
    "codex": "codex resume {value}",
    "pi": "pi --session {value}",
    "hermes": "hermes --resume {value}",
    "kiro": "kiro-cli chat --resume-id {value} {message}",
    "kiro-fallback": "kiro-cli chat -r {message}"
  }
}
```

- `{value}` is the session id. Templates **with** `{value}` require a
  known session; templates **without** it are valueless fallbacks usable
  with none. `<agent>-fallback` (e.g. `kiro-fallback`) is preferred over
  a valueless primary template when no value is known. A known value
  that fails the safety check counts as unknown: it is never
  substituted, and a valueless fallback is used instead.
- `{message}` is the `resume_message` setting, submitted as a chat
  message with the relaunch (kiro delivers positional chat input into
  the resumed session, verified live). Unset by default: the token is
  dropped and templates render exactly as before. An empty or
  whitespace-only message counts as unset (no bare Enter is sent). Multi-word messages
  stay a single argv token (shell-quoted on delivery).
- Session values are recovered from process argv via the resume flag
  derived from each template (`--resume {value}` → `--resume`, both
  `flag value` and `flag=value` forms). Only that flag counts: for the
  default kiro templates it is `--resume-id`, and kiro-cli's bare
  `--resume` is a switch, so `kiro-cli chat --resume continue` yields
  no session. A custom template's own flag is always read as valued
  (a kiro template with `--resume {value}` reads `continue`). Recovered values
  pass the same safety check as substitution; a non-UUID claude value
  never overrides a saved one on a live refresh (see above).
- An agent's process is recognised by its template's program (the
  primary's, or `<agent>-fallback`'s when the primary is blank): the
  process name, argv[0]'s basename (exact or minus one extension, e.g.
  `kiro-cli.AppImage`), or an interpreter's entry script (`node
  .../codex.js`, `python -m hermes`, `deno run --config f codex.ts`).
  An agent started through a launcher (`npx`, `pnpm exec`, `uvx`,
  `uv run`, `pipx run`, `env VAR=val`, …) is not recognised as running,
  so configure a template that runs the agent's binary directly.
  Matching is positive-only (herdr's agent label can be stale), so an
  unrecognised form reads as not running.
- `kiro-fallback` ships a valueless default (`kiro-cli chat -r`) used when
  no session value is known. Opt out with `"kiro-fallback": ""`, which
  disables fallback relaunches for kiro (valued `--resume-id` resumes are
  unaffected).
- An empty (or whitespace-only) primary template (`"pi": ""`) opts out
  of that template only: with no usable `<agent>-fallback` its panes log
  `NoResume` once and are never relaunched; a non-blank valueless
  fallback stays the agent's only template and is used whether or not a
  value is known (e.g. `{"kiro": "", "kiro-fallback": "kiro-cli chat -r
  {message}"}` always resumes with `chat -r`). Kiro session pinning runs
  only while the agent's primary template is valued (has `{value}`).

## State

Under the plugin state dir: `registry.json` (pane → session),
`shells.json` (pane → shell the agent was last seen in, whether it
is still live, and the agent process last seen there with its session
flag, if any), `profiles.json` (pane → claude-profile it ran under),
`monitors/<pane>.json` (monitor locks; the pane id is encoded
injectively: `_` → `__u__`, `:` → `__c__`, any other byte outside
`[A-Za-z0-9-]` → `__xHH__`, so `w7G:p1` → `w7G__c__p1.json`; only
regular non-dot `*.json` files there are read as locks, anything else
is ignored),
`stop-<pid>` sentinels, `log.txt` (rotated to `log.txt.1` past ~1 MiB,
one generation kept; the rotation is serialized under `state.lock`
with the size re-checked, so racing monitors rotate once; it never
waits for the lock: while another process holds it rotation is skipped
until a later append), and
`state.lock` (an exclusive lock serializing the registry/shells/profiles
load-modify-save across monitor processes; an unchanged write is
decided on an unlocked read and skips the lock; writes stage through
a unique `<name>.stage-<pid>.<n>.tmp` + rename, `n` a per-process counter, so
no two writers — including an older release's monitors mid-upgrade —
share a tmp; a failed write or rename removes its tmp). The lock is
taken with a bounded wait (about 5s; 250ms on the `hook-pane` path,
retried once at about 5s for an agentless pane's session);
a lock still held by another process past it skips the write (logged
at most once a minute; the monitor retries next poll) rather than
hanging the monitor or racing the holder's write, while a filesystem
that cannot lock at all writes unlocked (also logged). A map file that is not a UTF-8 JSON object is quarantined
under the lock — its bytes copied to `<name>.corrupt-<unix-secs>.<pid>.<n>`
(logged with the copy's name) — and the write proceeds on an empty map
written atomically over `<name>`, so one bad file cannot block every
later write and the file is never absent. A single entry that does not
parse (e.g. written by a newer release) is skipped on read (reported on
stderr; writers also log it to `log.txt` once per file and entry per
process, read-only callers never touch disk) but written back unchanged by every later
write, so another pane's write never deletes it. A map file that exists but cannot
be read (EACCES, EIO …) is never overwritten: the write is skipped
(returns failure, logged at most once a minute per file) so other
panes' entries survive. Read-only callers see either as empty.
`startup`/`supervise-all` sweep (without `state.lock`: only files no
live writer can still be using) staging files a
crash orphaned (any `*.stage-*.tmp` — map tmps and monitor-lock
claim tmps — plus the legacy fixed `registry.json.tmp`,
`shells.json.tmp` and `profiles.json.tmp`) once untouched for
10 minutes. Override the dir
with `HERDR_PLUGIN_STATE_DIR` / `HERDR_PLUGIN_CONFIG_DIR` (tests, dev);
`HERDR_BIN_PATH` points at a fake `herdr` binary (`KIRO_BIN_PATH` at a
fake `kiro-cli`). An env override set to the empty string counts as
unset.

Mutable state lives under `state_dir()`: `HERDR_PLUGIN_STATE_DIR` is
preferred, `HERDR_PLUGIN_CONFIG_DIR` is the legacy fallback, else the
default config-tree path. With neither env var set and `HOME` unset or
empty, every command fails closed (exit 2, `auto-resume: cannot resolve
state dir (HOME unset); set HERDR_PLUGIN_STATE_DIR`) instead of writing
state under the working directory. Legacy state is migrated one-shot on first
use: when `HERDR_PLUGIN_STATE_DIR` points elsewhere and its
`registry.json` is absent (a quarantine never removes it),
`$HERDR_PLUGIN_CONFIG_DIR/registry.json` is copied into the state dir
(by monitors and `startup`/`supervise-all`; until it is, e.g. while
`state.lock` is busy, the legacy registry is read in place).
`status` never migrates and writes nothing; `hook-pane`'s snapshot read
never migrates either (both read the legacy registry in place), but a
session write `hook-pane` makes may perform the one-shot migration under
`state.lock` (its lock wait is bounded at 250 ms, or about 5s on the
agentless-pane retry). Lock files from releases before v0.1.x (the old
`:` → `_` filename) are no longer recognised as locks; they show as
unreadable in `status`. `stop` cleans them up: a still-running old
monitor gets its `stop-<pid>` sentinel, a dead one's file is deleted
(as it also is when a new monitor claims its pane). While such an old
monitor runs, a new monitor for its pane refuses to claim the pane.
Only a file under its own pane's old name counts: a parseable lock
under any other mismatched name (e.g. a hand-renamed copy) is reported
unreadable and left in place.
