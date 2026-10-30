# Tendr Guide

How to use Tendr day to day — starting sessions, driving shells and REPLs,
observing long-running work, and reaching remote hosts. For the one-page pitch
see the [project README](../README.md); for how it works inside, see
[Architecture](architecture/README.md).

> Using Tendr *from a coding agent?* The [`using-tendr` skill](../.claude/skills/using-tendr/SKILL.md)
> is a thin router that tells the agent the first rules to obey, then points it
> back to `tendr guide` for version-matched detail.

## The model

A **session** is a long-lived child process — a shell, a REPL, a database
client, a script — that Tendr supervises. You start it once; after that every
`tendr exec` is a thin, one-shot client against that still-running process. The
process keeps its live state (cwd, env, activated venv, imported modules, loaded
tables, open connections) between calls; the transcript of what each call
returned is yours to keep, but the *process* is what Tendr keeps alive.

Each session has a stable **name** and lives under a **namespace** (default:
`default`). Its durable truth is on disk — `meta.json` (state) and `output.log`
(append-only history) — so it survives a crash, a disconnect, or your agent's
context resetting. A per-session **sidecar** is the actual supervisor; the CLI
just talks to it.

## Install and build

The crate and the installed binary are both `tendr`:

```bash
cargo install tendr
tendr --help
```

Releases up to 0.2.1 shipped as the crate `agenttender` with a `tender` binary,
state under `~/.tender`, and `TENDER_*` environment variables. `tendr` does not
read the old state root. To keep existing session history, cut over once: stop
every session the old binary supervises (or let them finish), then move the
state root:

```bash
tender list                # every session the old binary knows about
tender status <session>    # each must be terminal; `tender kill <session>` if not
mv ~/.tender ~/.tendr
```

Do not move the directory while old sidecars are alive: they hold absolute
paths into `~/.tender`. Scripts that read `TENDER_*` variables or carry
`#tender:` directives need the `TENDR_*` and `#tendr:` spellings.

For unpublished local builds, build the `tendr` binary from the repository:

```bash
cargo build --release --bin tendr
install -m 0755 target/release/tendr ~/.local/bin/
```

For Linux hosts from macOS, `cargo-zigbuild` is the simplest static-musl path:

```bash
brew install zig
cargo install cargo-zigbuild
cargo zigbuild --release --target aarch64-unknown-linux-musl --bin tendr
```

Musl builds avoid libc-version drift across remote hosts. Windows builds should
prefer the MSVC target on Windows itself; cross-compiling GNU Windows targets from
macOS/Linux may need an external binutils toolchain. For long remote builds, run
the build under Tendr too, then follow with `tendr log -f` and `tendr wait`.

## Start a session

```bash
tendr start --stdin dev -- bash          # a durable, supervised shell
```

- `--stdin` opens the pipe lane so `exec` can frame commands into the session. You almost always want it for interactive shells and REPLs.
- `-- <cmd> …` is the process to supervise. Everything after `--` is the child's argv.
- `--namespace <ns>` groups related sessions so `watch` can follow them together.
- `--replace` kills and restarts an existing session of the same name.
- `--timeout <sec>` kills the child if it overruns.

The child's kind (the **exec target**) is auto-inferred from the command —
`bash`/`sh` → shell, `duckdb` → DuckDB, `python3 -i`/`ipython` → Python REPL,
`powershell`/`pwsh` → PowerShell. Force it when inference is unclear:

```bash
tendr start --stdin py --exec-target python-repl -- ipython --no-banner --no-confirm-exit
```

## Drive it with `exec`

```bash
tendr exec dev -- cd repo
tendr exec dev -- . .venv/bin/activate
tendr exec dev -- pytest -x              # cwd + venv still active
```

Two rules that matter:

**`exec` takes argv, not a shell snippet.** `tendr exec sh -- "cd /tmp && pwd"`
sends one argv element, not two shell statements. For multi-step shell work, use
separate `exec` calls or wrap explicitly:

```bash
tendr exec sh -- bash -c 'cd /tmp && pwd'
```

**Gate success on the exit code, not on grepping stdout.** `exec` returns a JSON
envelope and the inner exit code propagates to `$?`:

```json
{"session":"dev","stdout":"…","stderr":"…","exit_code":0,"cwd_after":"repo","timed_out":false,"truncated":false}
```

```bash
tendr exec ddb -- "$SQL" | jq -e '.exit_code == 0' >/dev/null || { echo FAIL; exit 1; }
```

Only one `exec` can be in flight per session — a second concurrent `exec` against
the same session fails with *"another exec is already running."* Start a second
session (`ddb2`, `py2`) for parallel inspection.

Large exec results are quiet. The JSON result still contains the command's
stdout/stderr and exit code, but the annotation line in `output.log` may degrade
when it would be too large. In that case Tendr writes a compact
`exec_truncated` breadcrumb with `stdout_len`, `stderr_len`, `stdout_sha256`, and
`stderr_sha256` instead of raw payload. Find them with:

```bash
rg '"event":"exec_truncated"' output.log
```

## The REPL and database lanes

The same start/exec model turns any REPL into a durable session. In-memory
state — imported modules, loaded DataFrames, DuckDB tables, open connections —
survives across every `exec`.

### DuckDB

Structured JSON rows, ready to parse:

```bash
tendr start --stdin ddb -- duckdb :memory:
tendr exec  ddb -- "CREATE TABLE t AS SELECT range AS id, range * 2 AS val FROM range(5);"
tendr exec  ddb -- "SELECT count(*), sum(val) FROM t;"     # → [{"count_star()":5,"sum(val)":"20"}]
```

### Python / IPython

The namespace persists:

```bash
tendr start --stdin py -- python3 -i                       # or: ipython --no-banner
tendr exec  py -- 'import pandas as pd; df = pd.read_csv("data.csv")'
tendr exec  py -- 'print(df.describe())'                   # df still loaded
```

### PowerShell

`powershell` or `pwsh` — same clean-capture envelope, with two quirks worth
knowing:

- Each `exec` runs inside a fresh scriptblock scope, so variables need `$global:`
  to persist across calls (`$global:x = 42`). `Set-Location`, modules, and
  dot-sourced functions persist automatically.
- `Format-*` cmdlets throw inside the frame (no interactive host). Use
  `ConvertTo-Json` / `ConvertTo-Csv` / `Out-String` and pretty-print on the
  calling side.

## Answer prompts and attach

- **`tendr push <name>`** feeds stdin to a session waiting on an interactive
  prompt: `printf 'y\n' | tendr push dev`.
- **`tendr attach <name>`** connects your terminal to the live session for
  hands-on interaction. Press **`Ctrl-\` then `d`** to detach; the session keeps
  running. `Ctrl-\` twice sends one `Ctrl-\`, and `Ctrl-\` followed by any other
  key sends both. A trailing `Ctrl-\` is held until the next key decides it,
  `Ctrl-]` and every other key pass through unchanged, and `Enter ~ .` is the
  outer `ssh`'s own escape, not tendr's. The escape works however the session's
  program has set your terminal to encode keys: the legacy `Ctrl-\` byte, the
  kitty keyboard protocol (which Claude Code can switch Ghostty, kitty, WezTerm
  and foot to), and xterm's `modifyOtherKeys`. Key releases, auto-repeat and
  modifier keys those modes report do not interrupt it, and a doubled `Ctrl-\` is sent in the
  encoding you typed. `--escape none` turns the escape off
  so every key reaches the session. Window resizes follow you, and your
  terminal settings are restored however the attach ends. It is refused while
  another client holds the terminal.
- **`tendr attach <name> --takeover`** takes the terminal anyway: the previous
  client is disconnected and any input it (or an in-flight `push`) had queued is
  dropped, never written. Use it to reconnect after a dropped SSH session.
- Killing `tendr attach` with SIGTERM, SIGHUP or SIGINT (a closed terminal
  window sends SIGHUP) detaches it the same way, restores your terminal, and
  exits 1 with `tendr: attach ended by SIGTERM` (or the signal received). In
  the attached terminal `Ctrl-C` is a key for the session, not a signal.
- On a PTY session, `push` holds the terminal for its duration and succeeds only
  once every byte has been written to it. It fails, reporting how many bytes
  were written, if another client holds the terminal or takes it over mid-push;
  a second concurrent push is refused rather than interleaved.
- A PTY session starts at 24×80 and records its exact output and every applied
  window size under `recording/<run_id>/` in the session directory (typed input
  is not recorded). `tendr status` shows the recording under `pty.recording`.
  If recording has to stop (size limit, disk failure, or storage falling behind),
  the session keeps running: status shows `Stopped` with the last recorded
  sequence, a `recording.stopped` event is logged, and the run ends with a
  warning that later output was not recorded.

## Observe long-running work

No `ssh` + `tail` + `sleep` loops — Tendr owns the read side:

```bash
tendr status dev                 # current state
tendr log    dev --tail 50       # last N lines (on disk, survives crashes)
tendr log    dev -f              # follow
tendr log    dev -s 5m           # since a time window
tendr wait   dev --timeout 600   # block until it exits and is released (propagates its code)
tendr watch --namespace nightly --events --logs   # follow a whole namespace
```

`watch` takes a namespace, not a session name — it follows every visible session
(optionally filtered by `--namespace`).

## Lifecycle, batches, and hooks

```bash
tendr kill  dev                  # stop a session
tendr prune dev                  # remove one finished session by name (local-only)
tendr prune --older-than 7d      # or every finished session past an age; --all for all
tendr run --detach ./job.sh      # one-shot convenience over `start` for scripts
```

`wait` and `kill` return once the session's sidecar has released it, so `prune` or
`start --replace` straight after them find the session free. If the sidecar still
holds it after 2 s, they return anyway with a warning on stderr.

`kill` stops the session's process group with SIGTERM, then SIGKILL after 5 s.
A PTY session first gets SIGHUP and up to a second to act on it, as when a
terminal window closes, so an interactive shell hangs up its background jobs
too. `kill --force` and `--timeout` send SIGKILL at once, which a shell cannot
pass on: its background jobs keep running.

Useful `start` / `run` flags for batch work:

- `--detach` — return immediately, leave it running.
- `--after <session>` — wait for other sessions to exit first (dependencies).
- `--on-exit <command>` — fire a hook when the child exits.
- `--replace` — restart an existing session of the same name.
- `--timeout <sec>` — kill on overrun.

## Reach remote hosts with `--host`

Put `--host` on the Tendr command itself and the *same* commands run over SSH:

```bash
tendr --host data-box start --stdin ddb -- duckdb /data/warehouse.duckdb
tendr --host data-box exec  ddb -- "SELECT count(*) FROM read_parquet('s3://bucket/*.parquet');"
tendr --host data-box log   ddb -f
tendr --host data-box wait  extract_all --timeout 3600
```

`--host` carries `start`, `status`, `list`, `log`, `push`, `kill`, `wait`,
`watch`, `attach`, and `exec`. Remote `exec` ships the payload as one JSON frame
over the ssh stdin channel — it never traverses a remote shell, so there is no
nested-quoting layer to escape.

> **Remote-shell scope (read before `--host` to Windows).** Only `exec` uses the
> constant-argv frame transport, so it is safe against any remote shell. The other
> `--host` commands still reconstruct argv for a **POSIX** remote login shell.
> So today: **local Windows and remote `exec` are supported; general `--host`
> command forwarding remains POSIX-shell-only** — do not point general `--host`
> commands at a Windows host (cmd.exe / PowerShell) until the
> [remote frame transport](plans/active/01_remote-frame-transport.md) lands.
>
> *(A 2026-07-10 ARM-Windows smoke ran `start`/`kill`/`exec` with simple
> arguments — happy-path evidence that the mechanism runs, **not** proof of
> general Windows `--host` safety. Hostile or space-containing arguments were not
> exercised and remain exposed on the reconstructed-argv commands.)*

**`run`, `wrap`, `prune`, `query`, `guide`, and `skill`** are local-only.
Naming `--host` on them exits `2` with a ready-to-paste fallback:

```text
$ tendr --host data-box run deploy.sh
error: 'run' is local-only and does not support --host
try:  ssh data-box 'tendr run deploy.sh'
```

This is the workflow behind "leave long-running work on a remote box and come
back to it": start it under `--host`, disconnect, and reconnect later to `log`,
`status`, `wait`, or `exec` against the same live session.

On Windows hosts, the default ssh shell only matters when you write your own
`ssh host 'tendr ...'` wrapper. `tendr --host ... exec ...` uses a constant
remote argv (`tendr exec --frame-from-stdin`) and sends the payload over stdin,
so the payload does not care whether the remote default shell is `cmd.exe` or
PowerShell. If you manually ssh-wrap local-only commands, quote for that remote
shell explicitly; for PowerShell-default hosts, `powershell -NoProfile -Command`
is often clearer than relying on implicit quoting.

This path was validated end to end on 2026-07-10 against a Parallels ARM Windows
VM over SSH, using the released Tendr 0.2.0 x64 binary under Windows emulation.
Remote `exec` preserved PowerShell session state across frames, returned clean
structured JSON, and composed correctly with remote `start` and `kill`.

### Scripting: `exec --frame-from-stdin`

The transport `--host` uses is independently useful locally — pass the whole exec
request as one JSON frame on stdin so multi-line SQL/Python never fights argv
quoting:

```bash
jq -cn --rawfile sql query.sql '{v:1, session:"ddb", cmd:[$sql], timeout:300}' \
  | tendr exec --frame-from-stdin
```

## Cap a session's memory

A supervised session that runs away — a listing that accumulates in RAM, a load
that balloons — can take the whole host down, not just itself. It is worst on a
box with **no swap**: once memory is exhausted the kernel can't even `fork()`, so
new `ssh` logins start failing and you lose the very access you'd use to kill the
job, until the OOM killer reaps something. When you leave long-running work on a
remote box, bound it so a runaway dies *inside its own limit* and the host stays
reachable.

**Linux — cgroup v2 (the real thing).** With systemd this needs **no root** when
the `memory` controller is delegated to your user slice — check with:

```bash
cat /sys/fs/cgroup/user.slice/user-$(id -u).slice/cgroup.controllers   # must list "memory"
```

One-time setup: enable lingering (so the user manager and its jobs survive
logout) and define a capped slice:

```bash
loginctl enable-linger
mkdir -p ~/.config/systemd/user
cat > ~/.config/systemd/user/tendr.slice <<'EOF'
[Slice]
MemoryHigh=2G      # soft — reclaim/throttle before the wall
MemoryMax=3G       # hard — OOM-kill inside the slice past this
MemorySwapMax=0
EOF
systemctl --user daemon-reload
```

Launch Tendr inside the slice; the detached sidecar and its child inherit the
cgroup:

```bash
systemd-run --user --slice=tendr.slice --scope --quiet \
  tendr run --detach --replace ./job.sh
```

The child's *own* scope reports `memory.max = max` — that's expected. cgroup v2
enforces the tightest limit among **all** ancestors, so `tendr.slice` bounds the
aggregate of every session in it; a single runaway is OOM-killed within the slice
and the host is untouched. Three things to know: the cap is on the *slice*, so
concurrent sessions share its budget (size for the sum); system-level
`systemd-run` (without `--user`) needs root/polkit — the `--user` path above does
not; and persistence across a full reboot is host-dependent (some locked-down
NAS/appliance OSes reset the linger marker or user units — reapply on boot if so).

### Windows and macOS

**Windows** has a true equivalent: a **Job Object** with
`JOB_OBJECT_LIMIT_JOB_MEMORY` caps the committed memory of every process in the
job, and an over-limit allocation simply fails. There is no clean built-in CLI —
today it takes native code / P-Invoke (or a helper) to call
`SetInformationJobObject` then `AssignProcessToJobObject` on the child. Under
**WSL2** you get the Linux path above instead, under a VM-wide ceiling set in
`%UserProfile%\.wslconfig` (`[wsl2]` → `memory=`).

**macOS** has no host-level cgroup, but it isn't empty-handed. A native
per-process kill limit *does* exist — **jetsam** (the `memorystatus` framework
shared with iOS): the undocumented `JetsamMemoryLimit` launchd key makes the
kernel kill a process that overruns N MB. It's private, launchd-plist-only, and
barely exercised on the desktop, so it's not something to hang a job on. The
portable knobs are weak too — `ulimit -v` (address space) is coarse and
unreliable, `ulimit -m` (RSS) a no-op on modern kernels. The saving grace is
dynamic swap: a Mac *degrades* (thrash, slow) rather than hard-deadlocking like a
swapless Linux box. For a dependable hard bound, run the job in a memory-capped
Linux VM and use the cgroup path inside it — via Apple's own **`container`** tool
(macOS 26+, a lightweight VM per container) or OrbStack / Colima / Lima / Docker
Desktop.

## Record where a session runs — `--boundary`

Optionally tag a session with the environment it runs in (host, container, VM,
pod) so `status` and analytics can tell a local session from one inside a
container on a remote box. Tendr *describes* boundaries; it never manages them.

```bash
tendr start job --boundary host:data-box -- make test
tendr start dev --boundary container:my-image:latest --boundary-parent host:data-box -- bash
```

The boundary is authoritative in `meta.json` and is stamped, immutably, into the
run's lifecycle events for historical analytics. See
[the boundary plan](plans/completed/2026-07-10-boundary-metadata.md).

## Query the event log

Every supervised run emits a structured JSONL event stream. Point DuckDB at it
with `tendr query` to answer questions across sessions — failure rates, longest
blocks, causal chains. See the [analytics recipes](analytics-recipes.md).

## Tendr and Boo

Tendr owns the **process**; [Boo](https://github.com/coder/boo) owns the
**screen**. They compose as a stack — supervise a Boo session with Tendr for a
durable, accountable process while Boo drives and reads the live TUI. Tendr does
rendered-screen reads for nobody and deliberately never will.

## Tendr inside herdr

[herdr](https://herdr.dev) owns the **agent pane**: it hosts interactive
agents and shows whether each is working, blocked or idle, from the agent's own
lifecycle hooks. Tendr owns the **processes those agents start**. The agent
runs in a herdr pane; its builds, REPLs and remote jobs run under
`tendr start`/`exec`, where it gets exit codes rather than a screen to scrape.

Measured together on macOS on 2026-09-25 (herdr 0.9.1, oh-my-pi 18.3.0, and
`tender` 0.2.1, before the rename to `tendr`):

- **Tendr outlives the pane and the server.** A session started from a pane
  kept running after `herdr pane close`, finished `ExitedOk` with its output
  logged, and a `--stdin` shell kept its cwd and exported variables across the
  pane closing *and* `herdr session stop`. The sidecar `setsid`s, so it is
  reparented to launchd rather than dying with the pane.
- **Agent state stays right.** An omp agent in a pane ran `tendr start` and
  an 8-second `tendr exec` through its bash tool; herdr showed the pane
  `working` for the turn and `idle` after, the agent reported the exec's exit
  code, and the session was still `Running` for the next turn.
- **Sessions inherit the pane's identity.** Anything Tendr starts from a pane
  carries `HERDR_ENV`, `HERDR_PANE_ID`, `HERDR_SOCKET_PATH`, `HERDR_SESSION`,
  `HERDR_BIN_PATH`, `HERDR_TAB_ID` and `HERDR_WORKSPACE_ID`. That is what lets an
  `--on-exit` hook reach the right herdr session, and it is harmless: herdr
  applied lifecycle reports only from the pane's own agent session. An omp
  started under `tendr start --pty` from a pane's shell, and a hand-sent
  `pane.report_agent`, were both acknowledged and ignored. omp also marks its
  shells `OMPCODE=1`, which its herdr extension treats as nested and silences.

Raise a herdr notification when a job ends. `--on-exit` runs its command as
argv, not through a shell, so put the expansion in a small script, say
`~/bin/notify-herdr`:

```sh
#!/bin/sh
exec "$HERDR_BIN_PATH" notification show "tendr: $TENDR_SESSION $TENDR_EXIT_REASON" --sound done
```

```bash
tendr start --on-exit ~/bin/notify-herdr build -- make   # from inside a herdr pane
```

The notification arrived whether or not the pane that started the job still
existed. Outside herdr `HERDR_BIN_PATH` is unset and the hook fails, which the
run's `callback.finished` event records.

Watch every job from one pane with `tendr watch --namespace <ns> --events`,
and `tendr attach <name>` to take one over by hand.

Limits:

- Read results from Tendr, not from `herdr pane read`: a pane is a screen,
  with no exit code.
- Run agents in herdr panes, not under Tendr: `tendr start --pty -- omp`
  works, but herdr cannot see that agent's state.

## Exit codes

Exit codes are command-scoped: the same number can mean different things to
different commands, and older codes keep their historical meanings. Every
command exits 0 on success and 1 on an error without a code of its own below
(session not found, session not running, an I/O error, …).

| Command | Code | Meaning |
|---|---|---|
| any | 2 | Usage error from argument parsing |
| any, with `--host` | 2 | Invalid destination, or a local-only command (printed with an `ssh` fallback) |
| any, with `--host` | the remote's | The remote `tendr`'s exit code; 255 is `ssh` failing to connect |
| `start` | 2, 5 | The child failed to spawn; the sidecar failed |
| `run` | the child's | The child's own code, or 2 spawn failed, 3 sidecar lost, 4 dependency failed, 5 sidecar failed, 124 timed out, 137 killed |
| `wait` | 1 | `--timeout` passed before the sessions ended |
| `wait` | 2, 3, 4, 5 | Spawn failed, sidecar lost, dependency failed, sidecar failed |
| `wait` | 42, 124, 137 | The child exited non-zero; a dependency timed out; killed while waiting on a dependency. With several sessions, the most severe code wins; a session killed or timed out on request is success |
| `exec` | the command's | The command's own exit code; 124 if `--timeout` passed; 2 for a malformed `--frame-from-stdin` frame |
| `events` | 44, 65 | `--from-cursor` names history that is gone; `--strict` found unparseable lines |
| `emit` | 2, 5, 6 | Invalid `--parent`; corrupt session; invalid kind or source |
| `wrap` | the child's | The wrapped command's code; 6 for an `--event` with a reserved prefix |
| `query` | DuckDB's | DuckDB's exit code |
| `guide`, `skill install` | 2 | Unknown topic; refusing to overwrite a modified skill file |
| `attach`, `push` to a PTY session | 80 | Protocol: the session answered the hello outside this attach protocol, or not within 10 s (an older or newer `tendr`) |
| `attach`, `push` to a PTY session | 81 | Control conflict: another client holds the terminal (a plain `attach` while a human is attached, or a `push` while anyone is), the attach was taken over by another client's `--takeover`, or the session is not running (its run has ended) |
| `attach`, `push` to a PTY session | 82 | Reserved: operation deadline exceeded |
| `attach`, `push` to a PTY session | 83 | Reserved: missing or incompatible screen extension |
| `attach`, `push` to a PTY session | 84 | Runtime or transport: the attach socket is missing or unreachable, the connection fails, or a push ended revoked or stopped with bytes unwritten (stderr gives the byte counts) |
| `attach`, `push` to a PTY session | 85 | Identity: the socket's listener, or the connecting client, is another user |

A push whose every byte reached the terminal exits 0, even if a takeover lands
just after. An `attach` ended by SIGHUP, SIGTERM or SIGINT exits 1. A `push` to
a pipe session keeps exit 1 for its failures.

## See also

- [Architecture](architecture/README.md) · [Design principles](design-principles.md) · [Roadmap](ROADMAP.md)
- [`using-tendr` skill](../.claude/skills/using-tendr/SKILL.md) — the thin agent-facing router to `tendr guide`
