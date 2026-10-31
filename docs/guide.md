# Tendr Guide

Tendr keeps long-running processes alive and lets you drive them with one-shot
commands. You start a shell, REPL or job once as a session. Later commands run
inside it, read its output or wait for it, locally or over SSH.

See also the [README](../README.md) (install),
[Architecture](architecture/README.md) (internals) and
[Design](design.md) (rules and reasons). Coding agents start from the
[`using-tendr` skill](../src/embedded/SKILL.md).

## Terms

|Term|Meaning|
|---|---|
|session|A named, supervised child process: a shell, REPL, database client or script. It keeps its state between commands.|
|run|One launch of a session's child. `start --replace` begins a new run of the same session.|
|sidecar|The supervisor process of one run. It starts with the run, outlives the CLI call that started it, and exits after the run ends.|
|namespace|A group of sessions. The default is `default`. A session name is unique within its namespace.|
|pipe session|A session started with `--stdin`. `exec` and `push` write to the child's stdin.|
|PTY session|A session started with `--pty`. The child gets a terminal; you can `attach` to it.|
|exec target|How `exec` talks to the child: `posix-shell`, `powershell`, `python-repl`, `duckdb` or `none`.|
|event log|The structured JSONL record of every run, exec and annotation in a session.|

A session's state is on disk under `~/.tendr`: `meta.json` holds its state and
`output.log` holds its output history. Both survive a crash or a disconnect.

## Install and build

```bash
cargo install tendr
tendr --help
```

The [README](../README.md) lists other routes: release archives,
`cargo binstall` and Nix. To build from a clone:

```bash
cargo build --release --bin tendr
install -m 0755 target/release/tendr ~/.local/bin/
```

To build a static Linux binary on macOS for a remote host, use `cargo-zigbuild`:

```bash
brew install zig
cargo install cargo-zigbuild
cargo zigbuild --release --target aarch64-unknown-linux-musl --bin tendr
```

A musl build does not depend on the remote host's libc version. On Windows,
build the MSVC target on Windows itself. `tendr skill install` installs the
`using-tendr` agent skill. Upgrading from `tender` 0.2.1? Follow the rename
steps in the [CHANGELOG](../CHANGELOG.md).

## Start a session

```bash
tendr start --stdin dev -- bash
```

Everything after `--` is the child's argv. `start` and `tendr run` share most
flags:

|Flag|Effect|
|---|---|
|`--stdin`|Make a pipe session, so `exec` and `push` can write to it. Use it for shells and REPLs.|
|`--pty`|Make a PTY session, for programs you want to `attach` to (`start` only).|
|`--namespace <ns>`|Put the session in a namespace.|
|`--exec-target <target>`|Set the exec target instead of inferring it (`start` only).|
|`--replace`|Kill an existing session of the same name and start a new run.|
|`--timeout <sec>`|Kill the child after this many seconds.|
|`--cwd <dir>`, `--env KEY=VALUE`|Set the child's working directory or an environment variable.|
|`--unset-env NAME`|Remove an inherited variable from the child's environment. Repeatable. `--env` still sets it. Use it for a variable whose mere presence matters: an agent's shell exports `CI`, and Claude Code turns colour off for any `CI` value, even an empty one.|
|`--after <session>`|Wait for that session to exit before starting. Repeatable.|
|`--any-exit`|With `--after`, start even if the dependency exited non-zero.|
|`--on-exit <command>`|Run a command after the child exits. Repeatable.|

Tendr infers the exec target from argv[0]: `bash`, `sh`, `zsh`, `dash` or `ash`
→ `posix-shell`; `pwsh` or `powershell` → `powershell`; `python`, `python3`,
`ipython` or `py` → `python-repl`; `duckdb` → `duckdb`. Set it yourself when
inference fails:

```bash
tendr start --stdin py --exec-target python-repl -- ipython --no-banner --no-confirm-exit
```

## Drive it with `exec`

```bash
tendr exec dev -- cd repo
tendr exec dev -- . .venv/bin/activate
tendr exec dev -- pytest -x              # runs in repo/ with the venv active
```

**`exec` takes argv, not a shell snippet.** `tendr exec sh -- "cd /tmp && pwd"`
sends one argv element, not two shell statements. For several steps, use
separate `exec` calls or wrap them yourself:

```bash
tendr exec sh -- bash -c 'cd /tmp && pwd'
```

**Check the exit code, not the output text.** `exec` prints a JSON result and
exits with the command's own exit code:

```json
{"session":"dev","stdout":"…","stderr":"…","exit_code":0,"cwd_after":"repo","timed_out":false,"truncated":false}
```

```bash
tendr exec ddb -- "$SQL" | jq -e '.exit_code == 0' >/dev/null || { echo FAIL; exit 1; }
```

`--timeout <sec>` limits how long the client waits. It is client-side only: the
command may still be running in the session after a timeout. A timed-out
`exec` exits 124.

A session runs one `exec` at a time. A second concurrent `exec` fails with
"another exec is already running on this session". Start a second session
(`ddb2`, `py2`) for parallel work.

The JSON result always holds the full output. The matching `output.log` line
is shortened when it would be too large. At worst it becomes an
`exec_truncated` record with `stdout_len`, `stderr_len`, `stdout_sha256` and
`stderr_sha256`. Find these with `rg '"event":"exec_truncated"' output.log`.

## REPLs and databases

A REPL or database client works the same way as a shell: `exec` runs code in
it, and its in-memory state stays loaded between calls.

### DuckDB

`exec` returns rows as JSON:

```bash
tendr start --stdin ddb -- duckdb :memory:
tendr exec  ddb -- "CREATE TABLE t AS SELECT range AS id, range * 2 AS val FROM range(5);"
tendr exec  ddb -- "SELECT count(*), sum(val) FROM t;"     # → [{"count_star()":5,"sum(val)":"20"}]
```

### Python / IPython

Variables, imports and loaded data persist between calls:

```bash
tendr start --stdin py -- python3 -i                       # or: ipython --no-banner
tendr exec  py -- 'import pandas as pd; df = pd.read_csv("data.csv")'
tendr exec  py -- 'print(df.describe())'                   # df is still loaded
```

### PowerShell

`pwsh` and `powershell` return the same JSON result, with two differences:

- Each `exec` runs in a new scriptblock scope. Use `$global:` for variables
  that must persist (`$global:x = 42`). `Set-Location`, modules and
  dot-sourced functions persist without it.
- `Format-*` cmdlets fail inside `exec` because there is no interactive host.
  Use `ConvertTo-Json`, `ConvertTo-Csv` or `Out-String`, and format the result
  on the calling side.

## Answer prompts and attach

`tendr push <name>` sends its stdin to the child, for example to answer a
prompt: `printf 'y\n' | tendr push dev`. A pipe session needs `--stdin` for
this; a PTY session does not. On a PTY session, `push` holds the terminal while
it writes and succeeds only when every byte is written. It fails, reporting the
bytes written, if another client holds the terminal or takes it over. A second
concurrent `push` is refused, not interleaved.

`tendr attach <name>` connects your terminal to a PTY session:

- `Ctrl-\` then `d` detaches; the session keeps running. `Ctrl-\` twice sends
  one `Ctrl-\`; `Ctrl-\` then any other key sends both. `--escape none` turns
  the escape off. `Enter ~ .` is the outer `ssh` escape, not tendr's.
- `Ctrl-C` goes to the session; it does not stop `attach`.
- Window resizes follow your terminal, and your terminal settings are restored
  however the attach ends. The modes the session turned on, such as the
  alternate screen, mouse reports and keyboard protocols, are turned off when
  you leave and on again when you attach.
- `attach` is refused while another client holds the terminal. `--takeover`
  disconnects that client and drops any input it or an in-flight `push` had
  queued. Use it to reconnect after a dropped SSH connection.
- SIGTERM, SIGHUP or SIGINT (closing the window sends SIGHUP) also detach. It
  exits 1 with `tendr: attach ended by SIGTERM` (or the signal received).

The escape works with the legacy `Ctrl-\` byte, the kitty keyboard protocol
(which Claude Code can switch Ghostty, kitty, WezTerm and foot to) and xterm's
`modifyOtherKeys`. Key releases, auto-repeat and modifier-only keys do not
interrupt it. A doubled `Ctrl-\` is sent in the encoding you typed.

### PTY details

A PTY session starts at 24×80. Tendr records its output and every window size
under `recording/<run_id>/` in the session directory, but not typed input.
`tendr status` shows the recording under `pty.recording`. If recording stops
(size limit, disk failure, or storage falling behind), the session keeps
running. `status` then shows `Stopped` with the last recorded sequence, Tendr
logs a `recording.stopped` event, and the run ends with a warning that later
output was not recorded.

The child gets `TERM=xterm-256color` when `TERM` was unset, empty or `dumb` for
the `tendr start` that launched it, as in an agent's tool shell or `ssh -T`.
Otherwise `TERM` passes through, and `--env TERM=…` always wins. Other
variables pass through unchanged: remove an agent shell's `CI=true` or
`NO_COLOR=1` with, for example, `-- env -u CI -u NO_COLOR claude`.

## Observe long-running work

```bash
tendr status dev                 # current state
tendr list                       # every session; --namespace to filter
tendr log    dev --tail 50       # last 50 lines
tendr log    dev -f              # follow
tendr log    dev -s 5m           # since a time or duration (30s, 5m, 2h, 1d)
tendr log    dev --raw           # content only, without the JSONL envelope
tendr wait   dev --timeout 600   # block until it exits and the sidecar releases it
tendr watch --namespace nightly --events --logs
```

- `wait` takes one or more session names. It waits for all of them, or for the
  first with `--any`. See [Exit codes](#exit-codes) for what it returns.
- `watch` takes no session name. It streams NDJSON for every session, or for
  one namespace with `--namespace`. Choose streams with `--events` (run
  lifecycle), `--logs` (output) and `--annotations`. `--from-now` skips the
  initial snapshot.
- `tendr events` replays the event log as NDJSON. Filter with
  `--session <ns>/<name>` (exact), and `--kind` and `--source` (prefix matches).
  All three are repeatable. Use `--follow` to keep
  reading, `--since <RFC 3339>` or `--last <N>` to limit history, and
  `--cursors` with `--from-cursor` to resume where you stopped.
  `--include-logs` merges output lines in.

## Lifecycle, batches, and hooks

```bash
tendr kill  dev                  # stop a session
tendr prune dev                  # delete one finished session
tendr prune --older-than 7d      # delete finished sessions older than 7 days; --all for all
tendr run --detach ./job.sh      # run a script as a session
```

**Stopping.** `kill` sends SIGTERM to the session's process group, then SIGKILL
after 5 s. A PTY session first gets SIGHUP and up to 1 s to act on it, as when
a terminal window closes, so an interactive shell also hangs up its background
jobs. `kill --force` and `--timeout` send SIGKILL at once. A shell cannot pass
SIGKILL on, so its background jobs keep running.

`wait` and `kill` return once the sidecar has released the session, so a
`prune` or `start --replace` straight after them finds the session free. If the
sidecar still holds it after 2 s, they return anyway with a warning on stderr.

**Pruning.** `prune` deletes only finished sessions. Names resolve in
`--namespace`, or in `default`. `--dry-run` shows what it would delete. It also
removes stale attach sockets in `~/.tendr/sockets`.

**Hooks.** `--on-exit` splits its command into argv like a shell would, but
runs no shell: no variable expansion, pipes or `&&`. Put that logic in a
script. The command gets the [environment variables](#environment-variables),
including `TENDR_EXIT_REASON`. Tendr logs the results as a `callback.finished`
event.

**Scripts.** `tendr run <script> [args]` starts a script as a session. It
waits for the script to finish and exits with its code, unless you pass
`--detach`. The session name is the file name without its extension, with dots
replaced by hyphens. Tendr picks the interpreter in this order: `--shell`, an
executable file, the extension (`.py`, `.ps1`, …), then the shebang.

A script can set its own options in `#tendr:` comment lines at the top, after
an optional shebang:

```bash
#!/bin/bash
#tendr: session=nightly-build
#tendr: namespace=builds
#tendr: timeout=3600
#tendr: on-exit=notify-done
make all
```

The keys are `session=NAME`, `namespace`, `timeout`, `on-exit` (repeatable),
`stdin=pipe`, `cwd`, `env=KEY=VALUE` (repeatable), `replace` and `detach`.
Each matches the flag of the same name; `--foreground` overrides `detach`.
Command-line values win: `--on-exit` replaces every `on-exit` directive, and
`--env` entries apply after `env` directives. An unknown directive, or a second
copy of a non-repeatable one, is an error.

**Annotations.** Inside a session, `tendr wrap` runs a command and records it
in the session's event log:

```bash
tendr wrap --source claude.hook --event hook.post_tool_use -- ~/.claude/hooks/post.sh
```

`wrap` passes its stdin to the command. It records stdin, stdout, stderr and
the exit code as an event of kind `--event`, then exits with the command's
code. It finds the session from `TENDR_SESSION` and `TENDR_NAMESPACE`, and
fails outside a supervised process. `tendr emit --kind <kind> --data '<json>'`
appends an event without running anything. Kinds that start with `run.`,
`exec.` and other tendr-owned prefixes are reserved; both commands exit 6 if
you use one.

## Reach remote hosts with `--host`

Put `--host` on the tendr command and the same commands run over SSH:

```bash
tendr --host data-box start --stdin ddb -- duckdb /data/warehouse.duckdb
tendr --host data-box exec  ddb -- "SELECT count(*) FROM read_parquet('s3://bucket/*.parquet');"
tendr --host data-box log   ddb -f
tendr --host data-box wait  extract_all --timeout 3600
```

Start work under `--host`, disconnect, and come back later to `log`, `status`,
`wait` or `exec` against the same live session.

|Commands|With `--host`|
|---|---|
|`start`, `status`, `list`, `log`, `push`, `kill`, `wait`, `watch`, `attach`|Run on the remote host. Tendr rebuilds their argv for a POSIX remote login shell.|
|`exec`|Runs on the remote host. The request travels as one JSON frame on the SSH stdin, so no remote shell parses it.|
|`run`, `wrap`, `prune`, `query`, `guide`, `skill`|Refused with exit 2 and a ready-to-paste fallback, such as `try:  ssh data-box 'tendr run deploy.sh'`.|
|`events`, `emit`|Refused with exit 1.|

**Windows hosts.** Remote `exec` ignores the remote host's default ssh shell:
it always runs `tendr exec --frame-from-stdin` and sends the request on stdin.
The other remote commands assume a POSIX login shell. Do not use them against a
Windows host whose default shell is `cmd.exe` or PowerShell; arguments with
spaces or shell syntax are not protected there. If you wrap a local-only
command in `ssh` yourself, quote for the remote shell, for example with
`powershell -NoProfile -Command`.

**Connect deadline.** Every `--host` command except `attach` must authenticate
and start the remote `tendr` within 30 s, or tendr stops `ssh` and exits 82,
naming the host. Without it, a lapsed Tailscale SSH check-mode approval waits
about 30 minutes with no prompt.

- Once the remote `tendr` starts, the command runs as long as it needs, so
  `wait --timeout 3600` and `log -f` are unaffected.
- Time spent at an `ssh` prompt (passphrase, unknown host key) counts.
- `TENDR_SSH_CONNECT_TIMEOUT` sets the limit in seconds; `0` turns it off.
- The remote `tendr` must be this version or newer, or it rejects the
  `--hello` flag and the error says to upgrade it. With
  `TENDR_SSH_CONNECT_TIMEOUT=0` tendr sends no `--hello`, so an older remote
  works.
- If the deadline fires during authentication, a `ProxyCommand` that ignores
  its input can be left running.

### Scripting: `exec --frame-from-stdin`

You can use the same JSON frame locally. Pass the whole exec request on stdin,
so multi-line SQL or Python needs no argv quoting:

```bash
jq -cn --rawfile sql query.sql '{v:1, session:"ddb", cmd:[$sql], timeout:300}' \
  | tendr exec --frame-from-stdin
```

The frame carries `session`, `namespace`, `cmd` and `timeout`. A malformed frame
exits 2.

## Cap a session's memory

A runaway session can exhaust a host's memory. On a host with no swap, the
kernel then cannot `fork()`, so new `ssh` logins fail until the OOM killer
frees memory. You lose the access you need to kill the job. Put long-running
remote work under a memory limit, so a runaway is killed inside its limit and
the host stays reachable.

**Linux: cgroup v2.** Under systemd this needs no root if the `memory`
controller is delegated to your user slice. Check:

```bash
cat /sys/fs/cgroup/user.slice/user-$(id -u).slice/cgroup.controllers   # must list "memory"
```

One-time setup: enable lingering, so your user manager and its jobs survive
logout, and define a capped slice:

```bash
loginctl enable-linger
mkdir -p ~/.config/systemd/user
cat > ~/.config/systemd/user/tendr.slice <<'EOF'
[Slice]
MemoryHigh=2G      # soft: reclaim and throttle above this
MemoryMax=3G       # hard: OOM-kill inside the slice above this
MemorySwapMax=0
EOF
systemctl --user daemon-reload
```

Launch tendr inside the slice. The sidecar and its child inherit the cgroup:

```bash
systemd-run --user --slice=tendr.slice --scope --quiet \
  tendr run --detach --replace ./job.sh
```

- The child's own scope reports `memory.max = max`. This is expected: cgroup v2
  enforces the tightest limit of all ancestors, so `tendr.slice` applies.
- The limit is on the slice, so all sessions in it share one budget. Size it
  for their sum.
- Plain `systemd-run` needs root or polkit. `systemd-run --user`, as above,
  does not.
- Some locked-down NAS and appliance systems reset the linger setting or user
  units on reboot. Reapply them at boot on those hosts.

### Windows and macOS

**Windows.** A Job Object with `JOB_OBJECT_LIMIT_JOB_MEMORY` caps the committed
memory of every process in the job; an allocation over the limit fails. No
built-in command sets one. You need code or a helper that calls
`SetInformationJobObject` and then `AssignProcessToJobObject` on the child.
Under WSL2, use the Linux steps; WSL2 also has a VM-wide `memory=` limit under
`[wsl2]` in `%UserProfile%\.wslconfig`.

**macOS** has no dependable per-job limit. The `JetsamMemoryLimit` launchd key
is undocumented and plist-only. `ulimit -v` is unreliable and `ulimit -m` has
no effect. Dynamic swap makes a Mac slow down under memory pressure rather than
lock up. For a hard limit, use the cgroup steps inside a memory-capped Linux VM,
such as Apple's `container` tool (macOS 26+), OrbStack, Colima, Lima or Docker
Desktop.

## Record where a session runs (`--boundary`)

You can tag a session with the environment it runs in: a host, container, VM
or pod. `status` and the event log then tell a local session from one in a
container on a remote host. Tendr records boundaries; it does not create or
manage them.

```bash
tendr start job --boundary host:data-box -- make test
tendr start dev --boundary container:my-image:latest --boundary-parent host:data-box -- bash
```

- `--boundary KIND:LABEL` is the session's own boundary. `KIND` is `host`,
  `container`, `vm` or `pod`.
- `--boundary-parent KIND:LABEL` adds an enclosing boundary. It is repeatable,
  with the outermost last, and needs `--boundary`.
- `meta.json` holds the current boundary. The `run.starting` and `run.started`
  events hold a copy that never changes, for later analysis.

## Query the event log

`tendr query` runs DuckDB SQL over the event log of every session. Use it for
failure rates, slowest commands and causal chains. It needs the `duckdb` CLI
on your `PATH`. See the [analytics recipes](analytics-recipes.md).

```bash
tendr query "SELECT kind, count(*) FROM events GROUP BY kind"
```

## Tendr inside herdr

[herdr](https://herdr.dev) hosts interactive agents in panes and shows whether
each is working, blocked or idle. Run the agent in a herdr pane, and run its
builds, REPLs and remote jobs under `tendr start` and `tendr exec`, which return
exit codes instead of a screen to scrape.

Observed on macOS on 2026-09-25 with herdr 0.9.1, oh-my-pi 18.3.0 and `tender`
0.2.1 (the release before the rename to `tendr`):

- A session started from a pane kept running after `herdr pane close` and
  after `herdr session stop`. A `--stdin` shell kept its cwd and exported
  variables. The sidecar calls `setsid`, so it does not end with the pane.
- While an omp agent ran `tendr start` and an 8-second `tendr exec`, herdr
  showed the pane as `working`, then `idle`. The agent got the exec's exit
  code.
- Sessions started from a pane inherit `HERDR_ENV`, `HERDR_PANE_ID`,
  `HERDR_SOCKET_PATH`, `HERDR_SESSION`, `HERDR_BIN_PATH`, `HERDR_TAB_ID` and
  `HERDR_WORKSPACE_ID`. An `--on-exit` command uses them to reach the right
  herdr session. herdr accepts lifecycle reports only from the pane's own
  agent, so these variables do not confuse its agent state: an omp started
  with `tendr start --pty` from a pane's shell, and a hand-sent
  `pane.report_agent`, were both acknowledged and ignored.

To raise a herdr notification when a job ends, put the command in a script,
for example `~/bin/notify-herdr`, because `--on-exit` does not expand
variables:

```sh
#!/bin/sh
exec "$HERDR_BIN_PATH" notification show "tendr: $TENDR_SESSION $TENDR_EXIT_REASON" --sound done
```

```bash
tendr start --on-exit ~/bin/notify-herdr build -- make   # from inside a herdr pane
```

The notification arrives even if the starting pane has closed. Outside herdr,
`HERDR_BIN_PATH` is unset, the hook fails, and the `callback.finished` event
records the failure. Use `tendr watch --namespace <ns> --events` to follow
every job from one pane.

Limits:

- Read results from tendr, not from `herdr pane read`. A pane is a screen and
  has no exit code.
- Run agents in herdr panes, not under tendr. `tendr start --pty -- omp` works,
  but herdr cannot see that agent's state.

## Exit codes

Each command has its own codes, so one number can mean different things in
different commands. Every command exits 0 on success. It exits 1 on an error
with no code of its own below, such as session not found, session not running
or an I/O error.

|Command|Code|Meaning|
|---|---|---|
|any|2|Usage error from argument parsing|
|any, with `--host`|2|Invalid destination, or a local-only command (printed with an `ssh` fallback)|
|`events`, `emit` with `--host`|1|Not supported over SSH|
|any, with `--host`|the remote's|The remote `tendr`'s exit code; 255 is `ssh` failing to connect|
|any, with `--host` except `attach`|82|`ssh` did not authenticate and start the remote `tendr` within the connect deadline (`TENDR_SSH_CONNECT_TIMEOUT`, default 30 s)|
|`start`|2, 5|The child failed to spawn; the sidecar failed|
|`run`|the child's|The child's own code, or 2 spawn failed, 3 sidecar lost, 4 dependency failed, 5 sidecar failed, 124 timed out, 137 killed|
|`wait`|1|`--timeout` passed before the sessions ended|
|`wait`|2, 3, 4, 5|Spawn failed, sidecar lost, dependency failed, sidecar failed|
|`wait`|42|The child exited non-zero (any code)|
|`wait`|124, 137|A dependency timed out; killed while waiting on a dependency|
|`exec`|the command's|The command's own exit code; 124 if `--timeout` passed; 2 for a malformed `--frame-from-stdin` frame|
|`events`|44, 65|`--from-cursor` names history that is gone; `--strict` found unparseable lines|
|`emit`|2, 3, 5, 6|Invalid `--parent`; no session given and none in `TENDR_SESSION`, or an invalid `TENDR_NAMESPACE`/`TENDR_SESSION`; corrupt session; invalid kind or source|
|`wrap`|the child's|The wrapped command's code; 6 for an `--event` with a reserved prefix|
|`query`|DuckDB's|DuckDB's exit code|
|`guide`, `skill install`|2|Unknown topic; refusing to overwrite a modified skill file|
|`attach`, `push` to a PTY session|80|Protocol: the session answered the hello outside this attach protocol, or not within 10 s (an older or newer `tendr`)|
|`attach`, `push` to a PTY session|81|Control conflict: another client holds the terminal (a plain `attach` while a human is attached, or a `push` while anyone is), another client's `--takeover` took the attach, or the session's run has ended|
|`attach`, `push` to a PTY session|82, 83|Reserved, not used yet|
|`attach`, `push` to a PTY session|84|Runtime or transport: the attach socket is missing or unreachable, the connection fails, or a push ended revoked or stopped with bytes unwritten (stderr gives the byte counts)|
|`attach`, `push` to a PTY session|85|Identity: the socket's listener, or the connecting client, is another user|

With several sessions, `wait` returns the most severe code. A session that was
killed or timed out on request counts as success (0). A `push` whose every
byte reached the terminal exits 0, even if a takeover lands just after. An
`attach` ended by SIGHUP, SIGTERM or SIGINT exits 1. A `push` to a pipe session
exits 1 on failure.

## Environment variables

|Variable|Set by|Value|
|---|---|---|
|`TENDR_SESSION`|tendr, for the child and `--on-exit`|Session name|
|`TENDR_NAMESPACE`|tendr, for the child and `--on-exit`|Namespace|
|`TENDR_RUN_ID`|tendr, for the child and `--on-exit`|ID of the current run (UUIDv7)|
|`TENDR_GENERATION`|tendr, for the child and `--on-exit`|Run counter for the session; `--replace` increments it|
|`TENDR_SESSION_DIR`|tendr, for the child and `--on-exit`|Path of the session directory|
|`TENDR_EXIT_REASON`|tendr, for `--on-exit` only|`ExitedOk`, `ExitedError { code: N }`, `Killed`, `KilledForced`, `TimedOut` or `SidecarFailed`|
|`TENDR_BLOCK_ID`|`tendr wrap`, and `exec` in a POSIX shell, for the command they run|ID that links events emitted inside the command to its wrap or exec event|
|`TENDR_PARENT_EVENT_ID`|`tendr wrap`, for its command|ID of the wrap event|
|`TENDR_SSH_CONNECT_TIMEOUT`|you|Connect deadline for `--host` commands, in seconds. Default 30; `0` turns it off.|

`wrap` and `emit` read `TENDR_SESSION`, `TENDR_NAMESPACE`, `TENDR_RUN_ID` and
`TENDR_GENERATION` to find their session and run.

## See also

- [Architecture](architecture/README.md) · [Design](design.md) · [Plans](plans/README.md)
- [Analytics recipes](analytics-recipes.md)
- [`using-tendr` skill](../src/embedded/SKILL.md): the agent-facing entry point to `tendr guide`
