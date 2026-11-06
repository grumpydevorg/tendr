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
|exec target|How `exec` talks to the child: `posix-shell`, `powershell`, `python-repl`, `duckdb`, `ndjson` or `none`.|
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

Everything after `--` is the child's argv. `start` returns a session record;
`run` uses the same launch options and streams output until the run ends:

|Flag|Effect|
|---|---|
|`--stdin`|Make a pipe session, so `exec` and `push` can write to it. Use it for shells and REPLs.|
|`--pty`|Make a PTY session, for programs you want to `attach` to.|
|`--record-input`, `--no-record`|With `--pty`: also record what is typed, or record nothing ([PTY details](#pty-details)).|
|`--namespace <ns>`|Put the session in a namespace.|
|`--exec-target <target>`|Set the exec target instead of inferring it.|
|`--replace`|Kill an existing session of the same name and start a new run. A run still starting is waited for first, as `kill` does.|
|`--timeout <sec>`|Stop the child after this many seconds, as `kill` does.|
|`--cwd <dir>`, `--env KEY=VALUE`|Set the child's working directory or an environment variable.|
|`--unset-env NAME`|Remove an inherited variable from the child's environment. Repeatable; `--env` of the same name still sets it.|
|`--env-mode inherit\|clean`|Inherit the caller's environment (default), or construct a clean sidecar environment before the worker starts.|
|`--pass-env NAME`|In clean mode, inherit this variable by name. Repeatable; a missing variable is a usage error.|
|`--after <session>`|Wait for that session to exit before starting. Repeatable.|
|`--any-exit`|With `--after`, start even if the dependency exited non-zero.|
|`--on-exit <command>`|Run a command after the child exits. Repeatable.|
|`--ready-when-output <regex>`, `--ready-when-port <port>`|Say when the run is ready: a line of its output matches, or a TCP port on its host accepts connections. See [Wait until a server is ready](#wait-until-a-server-is-ready).|
|`--ready-within <duration>`|Record a miss if the run is not ready this long after it starts, such as `30s` or `2m`.|
|`--after-ready <session>`|Wait for that session to be ready before starting. Repeatable.|

The sidecar and the child get no descriptor above stderr from the shell that
ran `start` or `run`. A descriptor left open there, such as a terminal leaked by a
VM's init, does not reach them. `wrap` runs inline, so its command keeps the
caller's descriptors.

Tendr infers the exec target from argv[0]: `bash`, `sh`, `zsh`, `dash` or `ash`
→ `posix-shell`; `pwsh` or `powershell` → `powershell`; `python`, `python3`,
`ipython` or `py` → `python-repl`; `duckdb` → `duckdb`. Set it yourself when
inference fails:

```bash
tendr start --stdin py --exec-target python-repl -- ipython --no-banner --no-confirm-exit
```

## Run in the foreground

```bash
tendr run build --cwd ./repo -- make test
tendr run script -- bash ./job.sh
```

`run <name> [launch options] -- <command> [args]` uses the same launch path
as `start`, follows the session's output, and exits with the child's code.
Choose `start` instead when the caller should return immediately with JSON
session metadata. The session name and executable are explicit: tendr does
not read launch directives from scripts or infer their interpreter.

`run` does not attach to a PTY or forward the caller's terminal input. For
interactive control, use `start --pty` followed by `attach`. Its launch
timeout stops the child, as `start --timeout` does; it is not a wait-only
timeout. `run` is local-only.

## Choose a launch environment

```bash
tendr start py --stdin --env-mode inherit -- python3 -i
tendr start clean-py --stdin --env-mode clean -- python3 -i
```

- `inherit` is the default and preserves the caller's environment.
- `clean` constructs an environment before the sidecar is created. The
  worker and exit callbacks inherit that environment; cleaning only the
  worker would leave its supervisor subject to inherited lifetime markers.
- `--pass-env NAME` deliberately copies a named variable into clean mode.
  Repeat it for several names. It is an error with inherit mode, an invalid
  name, or an unset variable; resolution happens before replacing or creating
  a session. Names are case-insensitive on Windows.
- On Unix the baseline retains these variables when set: `HOME`, `PATH`,
  `USER`, `LOGNAME`, `SHELL`, `LANG`, `LC_ALL`, `LC_CTYPE`, `TMPDIR`, `TZ`.
  Windows retains `PATH`, `PATHEXT`, `SystemRoot`, `WINDIR`, `COMSPEC`,
  `USERPROFILE`, `HOME`, `HOMEDRIVE`, `HOMEPATH`, `TEMP`, `TMP`, `APPDATA`,
  `LOCALAPPDATA`, `LANG`, `LC_ALL`, `LC_CTYPE`, `TZ`.
- Worker `--unset-env` removals and explicit `--env KEY=VALUE` overrides still
  apply; generated `TENDR_*` session identity remains authoritative.
- The launch spec records the mode and selected names, never the resolved
  `--pass-env` values. Those values travel only in the sidecar's process
  environment. Explicit `--env` values are persisted configuration, so do not
  use that option for secrets. Programs can still expose their own environment
  in output; tendr does not redact arbitrary program output.
  A passed `TERM` is also omitted from recording/export metadata unless an
  explicit persisted override or the normal PTY fallback replaces it.
- A repeated idempotent start does not refresh a running process's values.
  Replacing it resolves the current environment again. A changed mode or
  selected-name set is a different launch specification.
- With `--host`, names resolve on the remote host; local values are not
  forwarded. Environment policy requires remote frame version 3. Older
  remotes reject it before launching work instead of silently inheriting.

For example, when `HTTPS_PROXY` is set on the executing host:

```bash
tendr start worker --env-mode clean --pass-env HTTPS_PROXY -- ./worker
```

Clean launch is not a sandbox or a universal survival guarantee. It avoids
inherited environment-based tracking without knowing a terminal product's
variable names, while retaining platform detachment. Linux cgroups, Windows
Job Objects, containers, other cleanup policies and host lifetime still apply.
See [the hc scope observation](#observed-hc-cleanup-scope-limit).

## Wait until a server is ready

A server started with `tendr start` is `Running` as soon as it spawns, before
it listens. Tell `start` what ready means for it, and tendr checks it:

```bash
tendr start api --ready-when-output 'listening on :\d+' --ready-within 30s -- ./api
tendr start db --ready-when-port 5432 -- postgres -D data
tendr wait --ready api db           # 0 once both are ready
tendr start tests --after-ready api -- ./run-tests
```

- `--ready-when-output <regex>` holds once a line of the child's output,
  stdout or stderr, matches the regular expression (Rust `regex` syntax,
  unanchored). A pipe session's line is matched whole, even one
  `output.log` splits into 16 KiB records (a line over 64 KiB by its last
  64 KiB), and under `--log-delimiter newline-or-cr` an update ended by a
  carriage return counts as a line. A PTY
  session's output is matched as the terminal shows it, escape sequences
  included: a line ends at `\n` or `\r`, and the line still being written
  counts too, so a prompt such as `^ready> $` matches. So a `$`-anchored
  pattern can also match a line written in pieces before it is complete.
  A child that prints its ready line and exits at once was ready.
- `--ready-when-port <port>` holds once a TCP connection to that port on
  the session's host (`127.0.0.1` or `::1`) succeeds. Any listener counts,
  not only the child, so choose a port nothing else holds. The port is
  tried every 200 ms while the run lasts, so one open only briefly before
  the run ends may be missed.
- Give one condition per session. `--ready-within <duration>` sets a
  deadline counted from when the run is `Running`; without it, there is no
  deadline. Past it, tendr records the miss and leaves the child running.
  The condition is then no longer checked, so a session that misses its
  deadline never becomes ready.
- The sidecar checks the condition, so it works for detached and `--host`
  sessions alike. It records `run.ready` (or `run.ready_missed`) in the event
  log, and in `meta.json` `ready`: `{"state":"Ready","at":"<epoch s>"}` or
  `{"state":"Missed","at":…}`. `tendr events --follow --kind run.ready`
  streams it.
- `tendr wait --ready <session>…` returns 0 once every session is ready,
  and 43 if one missed its deadline or its run ended before it was ready.
  `--any` returns at the first session decided either way, and `--timeout`
  works as it does for exits. It fails with 1 for a session that has no
  ready condition.
- `start --after-ready <session>` starts the child once that session is
  ready, as `--after` does once a session exits. If the dependency misses
  its deadline or ends before it is ready, the waiting session ends as
  `DependencyFailed`, and `wait` on it exits 4. `--timeout` bounds the wait
  for both kinds of dependency. The dependency must have a ready condition.

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
`exec` prints its result and exits 124 at the deadline.

A session runs one `exec` at a time. A second concurrent `exec` fails with
"another exec is already running on this session". Start a second session
(`ddb2`, `py2`) for parallel work.

After a timeout, the session is still busy with that command. Until it
finishes, a later `exec` sends nothing and exits 1 with a message naming the
earlier exec's token, start time and command. Sending it would let the
running command read it as input, and mix the late output into the new
result. Wait for the command to finish (`tendr log dev -f` shows its output)
and retry, or stop the session with `tendr kill dev`. The same applies when
an `exec` client dies before its result. An NDJSON session is the
exception; see below.

The JSON result always holds the full output. The matching `output.log` line
is shortened when it would be too large. At worst it becomes an
`exec_truncated` record with `stdout_len`, `stderr_len`, `stdout_sha256` and
`stderr_sha256`. Find these with `rg '"event":"exec_truncated"' output.log*`.

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

`pwsh` and `powershell` return the same JSON result. `exec` runs the code in
the session's own scope, as if typed at its prompt, so variables, functions,
`Set-Location` and `$env:` changes persist between calls:

```bash
tendr start --stdin ps -- pwsh -NoProfile                  # or: powershell -NoProfile
tendr exec  ps -- '$x = 42; function f { 7 }'
tendr exec  ps -- '$x + (f)'                               # → 49
```

While the code runs, tendr keeps its own state in `$__tendr_frame`, which the
code cannot change. Over `--host` this needs a newer remote `tendr`; see
**Versions** under [Reach remote hosts](#reach-remote-hosts-with---host).

`Format-*` cmdlets fail inside `exec` because there is no interactive host.
Use `ConvertTo-Json`, `ConvertTo-Csv` or `Out-String`, and format the result
on the calling side.

On Windows, a `powershell` (Windows PowerShell 5.1) child does not get pwsh 7's
module directories. pwsh 7 adds its own to `PSModulePath`, and everything it
starts inherits them, `tendr start` under a pwsh 7 prompt or ssh login shell
included. Windows PowerShell would then load pwsh 7's modules in place of its
own; on ARM64 that swallowed the first `exec`. So tendr drops from the child's
`PSModulePath` what pwsh 7 drops when it starts `powershell.exe` itself:
`Documents\PowerShell\Modules`, `Program Files\PowerShell\Modules` and the
`Modules` directory of any pwsh installation. The rest keeps its order.
`--env PSModulePath=…` or `--unset-env PSModulePath` replaces this. A
`powershell` that the child starts itself, such as `cmd /c powershell`,
inherits whatever the child passes on.

Windows PowerShell can take tens of seconds over the first command it has to
find in a module it has not loaded, while it analyses the installed modules:
22–38 s on GitHub's windows-11-arm runner, under a second on x64. Give the
first `exec` in a new `powershell` session a `--timeout` that allows for it.

### Line-oriented JSON processes (NDJSON)

`--exec-target ndjson` drives a program that reads one JSON object per stdin
line and writes one JSON reply line per request on stdout, such as
`sio stdio`. Tendr never infers this target, and it needs `--stdin` without
`--pty`.

```bash
tendr start --stdin --exec-target ndjson dev -- sio stdio
tendr exec  dev -- '{"cmd":"read","bytes":4,"timeout_ms":1000}'
```

- The command is exactly one argument, a JSON object. Tendr adds a fresh
  string `id` and writes the object as one line. It re-serializes the object,
  so key order may change and numbers beyond 64-bit integer or double range
  lose precision. An object that already has an `id`, anything that is not an
  object, or more than one argument exits 2 and sends nothing.
- The program must copy the request's `id` into its reply. The first stdout
  line that is a JSON object with that `id` is the result's `stdout`,
  verbatim. `exit_code` is 1 if the reply has `"ok": false` or a non-null
  `error`, else 0. `cwd_after` is empty.
- Every other stdout line seen while waiting is kept, in order, in
  `unmatched`: a late reply to an `exec` that timed out, or output the program
  sent on its own. The program's stderr is `stderr`.
- `request_id` is the `id` tendr added to the request. It is present on every
  NDJSON result, including a timeout, so you can still address the request.

```json
{"session":"dev","stdout":"{\"id\":\"…\",\"ok\":true,…}","stderr":"","exit_code":0,"cwd_after":"","timed_out":false,"truncated":false,"unmatched":[],"request_id":"…"}
```

A timed-out `exec` exits 124 at once, unlike the other targets, and the next
`exec` can start while the program is still busy. It still gets its own
reply, because replies are matched by `id`. Lines the program writes while no
`exec` is waiting are only in the log (`tendr log dev`).

Tendr has no cancel of its own; to stop a timed-out request, send the
program's own cancel request naming `request_id`. For example, on a `sio
stdio` session named `serial`:

```bash
tendr exec serial -- '{"cmd":"cancel","target":"<request_id>"}'
```

This is an ordinary NDJSON request. What follows is sio's behaviour (sio
0.4.0, [SKILL.md, "Waiting and cancelling"](https://github.com/grumpydevorg/sio/blob/main/skills/sio/SKILL.md)),
not tendr's:

- A cancel skips sio's queue and stops the `read`, `readline` or `expect`
  running when it arrives, within about 100 ms. With `target`, it stops it
  only if the running request's id equals `target`.
- The stopped request replies with `end:"cancelled"` and
  `consumes_input:false`; its bytes stay buffered.
- The cancel's own reply is `{"ok":true,"result":{"cancelled":true|false}}`
  (sio main c5fc03e). A cancel that stops a wait replies `true` after that
  wait's `cancelled` reply, in request order, so requests queued before the
  cancel reply first and can delay it. Only a cancel that stops nothing
  (wrong or stale `target`, or nothing running) skips the queue: it replies
  `false` at once, ahead of the replies to running and queued requests.
- An untargeted `{"cmd":"cancel"}` is safe only when one caller owns the
  session.
- Requests queued behind a running one are capped at 16 MiB (sio main
  c1cdfe7); beyond that, a request gets an immediate, retryable
  `queue_full` reply and does nothing.

On tendr's side, the cancel `exec` exits 0 either way, and the stopped
request's reply, which arrives while it waits, lands in its `unmatched`. A
`queue_full` reply is an `ok:false` reply, so its `exec` exits 1.

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
- Detaching first writes what you typed before the escape, waiting at most
  about a second for a program that is not reading. Input still unwritten
  then is dropped, and so is any you typed but tendr had not yet written when
  you lose the connection or someone runs `--takeover`. Tendr logs it as
  `pty.input_revoked`. Input already written stays for the program to read.
- `Ctrl-C` goes to the session; it does not stop `attach`.
- Window resizes follow your terminal, and your terminal settings are restored
  however the attach ends. The modes the session turned on, such as the
  alternate screen, mouse reports and keyboard protocols, are turned off when
  you leave and on again when you attach. Attaching at the session's own size
  briefly shrinks it by one row and restores it, so a full-screen program
  such as Claude Code redraws; scrollback is not restored.
- `attach` is refused while another client holds the terminal. `--takeover`
  disconnects that client and drops any input it or an in-flight `push` had
  queued but tendr had not yet written to the terminal. Use it after a local
  connection was lost, or when an `attach --host` gave up reconnecting. The
  client taken over exits 81 with
  `tendr: another client took over this session`. If it was frozen at the
  time (a stopped terminal or SSH connection), it stays alive, without
  control, until it runs again, and then exits 81.
- `attach --read-only` watches without control: it shows the session's
  output beside whoever holds the terminal, even while a human does, and up
  to eight can watch at once (a ninth is refused with 84). Your keystrokes
  are dropped, with a note on the bottom line from the first one; leaving
  erases that line (once output has scrolled the note up, the line erased is
  the session's latest). `Ctrl-\ d`
  still detaches (0), so `--escape none` is refused with it. It sends no
  window size, so a full-screen program shows nothing new until it next
  redraws, and then at the controlling terminal's size. A takeover leaves
  watchers attached. To take control, attach again with `--takeover` (it
  cannot be combined with
  `--read-only`). A session started by a `tendr` without read-only viewers
  refuses it with 80. It works with `--host` too, and reconnects like any
  `attach --host` (below), when both hosts and the session are new enough
  ([versions](#reach-remote-hosts-with---host)).
- With no terminal (stdin or stdout redirected), `attach --read-only`
  streams the session's output to stdout, for an agent or script watching a
  PTY session live: `tendr attach --read-only build | grep -m1 'error'`. It
  never reads stdin, writes nothing but the session's bytes to stdout (from
  the moment it attaches, after the modes the program had on), and puts
  every notice on stderr. There is no escape; a signal ends it (1). It exits
  0 when the run ends and it has all of the run's output, 84 if it was cut
  off (it fell more than the session's output buffer behind, or, at the
  run's end, was not reading the last of it within 2 s, as with a stalled
  reader; stderr says how many bytes of the run's output it had), and 1
  when its stdout closes (the reader gone), which detaches it. A session
  started by an older `tendr` cannot say whether the last of the output
  arrived: then 0 means only that the run ended. The run's own record
  (`status`, its exit code) can land a moment after a 0; `tendr wait`
  waits for it. Over
  `--host` the remote host tells 0 from 84, and a dropped connection
  reconnects without anything reaching stdout: the reconnect's status lines
  and notices go to stderr. Without `--read-only`, attach still needs a
  terminal.
- SIGTERM, SIGHUP or SIGINT (closing the window sends SIGHUP) also detach. It
  exits 1 with `tendr: attach ended by SIGTERM` (or the signal received). On
  Windows, closing the console window, Ctrl-Break, or a Ctrl-C event that
  another program raises in the console, does the same
  (`tendr: attach ended by CTRL_BREAK_EVENT`); Ctrl-C typed at the keyboard
  goes to the session.
- `tendr --host <host> attach <name>` attaches to a session on another host.
  Your terminal stays on your machine: raw mode, window resizes and the
  detach escape work exactly as above, and the session on the remote host
  keeps the recording and decides who holds the terminal. `ssh` carries
  only the attach traffic, so any session name works, and the exit codes are
  those of a local attach. A client whose terminal or connection stops
  reading does not hold up its own detach, nor another client's
  `--takeover`. It works the same from Windows, in Windows Terminal or a
  conhost window on Windows 10 1809 or later. A console without virtual
  terminal support (an older Windows, or the console's "Use legacy console"
  setting) and mintty (Git Bash's terminal) are refused before ssh starts.

**When an `attach --host` connection drops**, tendr reconnects by itself
and carries on where your terminal stopped, with the screen as it was. That
needs a remote host and a session new enough to resume
([versions](#reach-remote-hosts-with---host)); otherwise the attach ends as
before, with ssh's exit.

- ssh checks the link every 5 s and gives up after three unanswered checks,
  so a dead network is noticed within about 15 to 20 s. Killing `ssh`, or a
  remote host that drops the connection, is noticed at once. The attach
  makes its own ssh connection even where your ssh config shares one
  (`ControlMaster`), because a shared connection's checks are the master's.
- While it reconnects, a line at the bottom of your terminal says so, and
  ssh's latest error, such as its timeout, goes on the last row below it.
  What ssh says while you are attached stays off the session's screen: the
  next reconnect shows it there, or the attach prints it when it ends. A
  reconnect never prompts (`ssh -o BatchMode=yes`): a login that needs a
  passphrase or a host-key answer fails at once and is tried again. Load the
  key into your agent and the next attempt gets through. A lapsed Tailscale
  SSH check-mode approval prints nothing; each attempt stops at the connect
  deadline and is tried again, so the attach resumes on the first attempt
  after the approval is renewed.
- Attempts follow each other with pauses of 1, 2, 4, 8 and 16 s, then every
  30 s, for as long as it takes. `Ctrl-\ d`, SIGTERM, SIGHUP and SIGINT still
  leave (0, or 1 for a signal), and so on Windows do Ctrl-Break, closing the
  console window, and a Ctrl-C event another program raises (1); each stops
  the attempt under way. A signal or Ctrl-Break that also ends ssh still
  leaves with 1: the attach does not reconnect for it. The old
  connection may still hold the session until the remote host notices it is
  gone; attach again with `--takeover` then.
- Output the session wrote meanwhile arrives in full, once, as if the
  connection had never dropped; then the program is asked to redraw at your
  terminal's size (not for `--read-only`, which never sizes the session).
  Input is never resent: what you typed before the drop and had not reached
  the session is lost, and what you type while it reconnects is discarded,
  with a note in the status line. Where both hosts and the session are new
  enough ([versions](#reach-remote-hosts-with---host)), tendr says as it
  reconnects how many of the bytes it sent before the drop reached the
  session and how many did not, and repeats it when the attach ends. It
  counts the bytes it sent after the detach escape: a doubled `Ctrl-\` is
  one byte, and a `Ctrl-\` still waiting for its next key was not sent.
- The session keeps the last 32 MiB of output. If more than that was written
  while you were away, tendr says how many bytes you missed and how many the
  session no longer held, turns off the terminal modes the program had set,
  and shows the session from then on, with its current modes and a redraw.
  A full-screen program covers that notice at once, so tendr repeats it when
  the attach ends.
- The reconnect is refused, ending the attach with 81 and the remote's
  reason, if someone took the terminal (an attach, a takeover, or an agent's
  `push` still running), or if the session's run has ended. A read-only
  attach never holds the terminal, so only the run's end refuses it. A
  session started by a `tendr` that cannot resume ends it with 80, and so
  does one that cannot resume a read-only attach. Exit 82 or 255 from ssh
  during a reconnect is not an end: it is tried again.
- When a run ends, everything it wrote reaches every attached client before
  the session closes the connections, even one that was behind; a client
  that has not taken it all within 2 s is cut off where it is. A client
  shows all it was sent before it exits, even if your terminal is slow to
  take it.
- On Windows, `ssh.exe` ended by Windows (an NTSTATUS exit such as
  `0xC000013A`, or a crash) or killed (Task Manager's exit 1) is a dropped
  connection too, and the attach reconnects: the remote `tendr` always
  reports its own exit code, so an exit without that report is ssh's own.

The escape works with the legacy `Ctrl-\` byte, the kitty keyboard protocol
(which Claude Code can switch Ghostty, kitty, WezTerm and foot to) and xterm's
`modifyOtherKeys`. Key releases, auto-repeat and modifier-only keys do not
interrupt it. A doubled `Ctrl-\` is sent in the encoding you typed. When the
program in the session has turned on bracketed paste, as shells and Claude
Code do, a paste is sent as pasted: a `Ctrl-\ d` inside it does not detach.
Otherwise a paste is typing, and the escape applies to it.

### PTY details

A PTY session starts at 24×80. Tendr records its output and every window size
under `recording/<run_id>/` in the session directory. `--record-input` also
records what is typed: every byte written to the terminal, from an attach or
a `push`, stored in clear. That includes passwords, tokens and anything else
typed or pushed, and `export --format asciicast` writes it out as `i` events;
use it only where that is acceptable. `--no-record` records nothing, and
`status` then shows no `pty.recording`. Otherwise `tendr status` shows the
recording there. If
recording stops (size limit, disk failure, or storage falling behind), the
session keeps running. `status` then shows `Stopped` with the last recorded
sequence, Tendr logs a `recording.stopped` event, and the run ends with a
warning that later output was not recorded. If the sidecar itself is lost,
the recording shows `Stopped` with reason `sidecar_lost`. If the sidecar died
after the run ended but before writing the recording's end, it shows reason
`end_unrecorded`; the recording on disk is complete. Recordings are kept
until you delete them ([Pruning](#lifecycle-batches-and-hooks)).

The child gets `TERM=xterm-256color` when `TERM` was missing, empty, `dumb`,
or removed with `--unset-env TERM` for the `tendr start` that launched it, as
in an agent's tool shell or `ssh -T`. Otherwise `TERM` passes through, and
`--env TERM=…` always wins. Other variables follow the selected
[launch environment](#choose-a-launch-environment), apart from `PSModulePath`
for Windows PowerShell ([PowerShell](#powershell)). With inherited mode,
`--unset-env CI --unset-env NO_COLOR` removes those application settings
without changing the sidecar's environment.

### Export a recording

```bash
tendr export ~/.tendr/sessions/default/dev --format asciicast > dev.cast
```

The path may be a session directory, its `recording/<run_id>/` directory,
or one segment file (`seg-00000000.tndrrec`). The default format is asciicast
v2. Use an asciicast-compatible player to view it; tendr exports evidence
without managing playback, terminal geometry or playback timing.

- Output is written as `o` events, recorded input (passwords and tokens
  included) as `i` events, and every applied resize as an `r` event, with
  times since the recording started. Split UTF-8 characters are joined per
  stream.
- Corrupt recordings and invalid UTF-8 are refused with exit 1 before any
  export is written. Diagnostics name the file and byte offset or record.
  The exact binary recording remains unchanged.
- A final segment cut inside its last record, as a running session's can
  be, exports to its last complete record and reports the cut.
- Export is local-only and needs no terminal. Copy remote recordings here
  first, for example `scp -r box:.tendr/sessions/default/dev/recording ./dev-rec`,
  then `tendr export ./dev-rec > dev.cast`.

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
  `--ready` waits until they are ready instead
  ([Wait until a server is ready](#wait-until-a-server-is-ready)).
- `watch` takes no session name. It streams NDJSON for every session, or for
  one namespace with `--namespace`. Choose streams with `--events` (run
  lifecycle), `--logs` (output) and `--annotations`. `--from-now` skips the
  initial snapshot.
  Lifecycle viewing requires usable persisted event history; sessions without
  it fail with an actionable error rather than synthesizing transitions from
  metadata. `--logs` or `--annotations` alone can still read their output.
- `tendr events` replays the event log as NDJSON. Filter with
  `--session <ns>/<name>` (exact), and `--kind` and `--source` (prefix matches).
  All three are repeatable. Use `--follow` to keep
  reading, `--since <RFC 3339>` or `--last <N>` to limit history, and
  `--cursors` with `--from-cursor` to resume where you stopped.
  `--include-logs` merges output lines in. `--data <key>=<value>` keeps events
  by a field of their data, and `--max-count <N>` and `--timeout` end a follow
  on a match or at a deadline
  ([Wait for a published event](#wait-for-a-published-event)).
- `wait` waits for processes to exit, not for application requests or agent
  turns to finish. A caller can wait for a published event, but owns its
  interpretation; interactive agent coordination belongs to hc and the harness.
- A session keeps its latest 256 MiB of output or so: `output.log` rotates at
  64 MiB and the newest four files are kept. `log`, `watch --logs` and
  `events --include-logs` read them as one log, and `log -f` keeps following
  through a rotation. Output older than that is gone; to keep everything,
  have the child write to its own file too.

### Partial lines and progress output

A pipe session logs a record when its child ends a line. Output with no
newline yet, such as a progress meter that redraws itself with carriage
returns (`rsync --info=progress2`, `curl`, `pv`), stays in the reader: `tendr
log` shows nothing of it, and a stale log looks the same as a hung child.
Check `tendr log <name>` within a minute of starting a long job. If it is
empty, `tendr status <name>` tells the two apart:

```json
"pipe_reader": {"pending_partial_bytes": 412000, "last_record_age_s": 1203.4, "live": true}
```

- `pending_partial_bytes` are bytes read from stdout and stderr that no record
  holds yet: a child writing progress without newlines. 0 with a growing
  `last_record_age_s` is a child that writes nothing at all.
- `last_record_age_s` is the time since the last record, or since capture
  started.
- The sidecar updates the values at most once a second. `live` is false once
  capture has ended or the sidecar is gone; the values then no longer change.
  PTY sessions have no `pipe_reader`.

To see progress as it happens, choose one of:

- `tendr start --log-delimiter newline-or-cr`, which ends a record at a
  carriage return too, at once. CRLF stays one record. The default is
  `newline`.
- A newline-framed mode of the child, such as `rsync --info=name2,stats2`.
- `--pty`, which logs each read as it comes but merges stdout and stderr and
  makes the child see a terminal. `--pty` takes no `--log-delimiter`.

Records ended by a carriage return carry `"terminator": "cr"`. Every update is
stored, but `tendr log` (with `--tail N`, counted after) and `tendr events
--include-logs` keep only the last update of each run of them in one stream:
stderr lines in between do not end the run. `--all-updates` keeps them all.
`log -f`, `watch --logs` and `events --follow` show every update, as they
cannot know which one is last.

No record holds more than 16 KiB, in either mode. A longer stretch without a
terminator becomes several records marked
`"terminator": "fragment"`, cut between characters, whose contents join to the
original output. A record the stream's end cut short is marked `"eof"`. Bytes
that are not UTF-8 are shown as U+FFFD.

Over `--host`, these need a newer remote `tendr`
([Versions](#reach-remote-hosts-with---host)).

## Lifecycle, batches, and hooks

```bash
tendr kill  dev                  # stop a session
tendr prune dev                  # delete one finished session
tendr prune --older-than 7d      # delete finished sessions older than 7 days; --all for all
tendr prune --recordings-older-than 7d --recordings-max 10GiB   # delete old PTY recordings only
tendr run job -- bash ./job.sh    # explicit foreground execution
```

**Stopping.** `kill` sends SIGTERM to the session's process group, then SIGKILL
after 5 s. A PTY session first gets SIGHUP and up to 1 s to act on it, as when
a terminal window closes, so an interactive shell also hangs up its background
jobs. `--timeout` stops the same way, so a timed-out child exits up to about
6 s after the deadline. `kill --force` sends SIGKILL at once.

A stop then ends every other process the run started, wherever it went: a
shell's background jobs, `nohup` and `disown` jobs, `setsid` processes, and
tool shells that Claude Code starts in sessions of their own. `kill` and
`--timeout` send each SIGTERM and give it up to 1 s to exit; then, and at
once for `kill --force`, each one left is stopped and killed. The run's end
is recorded after that, so `kill` returns with them gone. A session you start
with `tendr start` from inside the run is left running. A stop cannot reach
processes started through another manager, such as `systemd-run --user`,
`at` or an already running `tmux` server, nor end a setuid program; a run
warning names what it could not end. On macOS, where orphans go to launchd,
it also misses a process that left the session, lost its parent and holds
no terminal descriptor, such as `(setsid cmd >/dev/null 2>&1 &)` (run
lifecycle, [Limits](architecture/03-run-lifecycle.md#limits)).

A PTY run ends with its child, even if a process the child started still holds
the terminal, such as a `setsid` job. Once the child has exited, tendr captures
that process's output for up to 2 s more, then records the child's exit. On
Linux a run warning names the pids that still hold the terminal. The process
is left running; its later writes to the terminal fail once the sidecar has
exited. A pipe run still waits until every process holding its output pipes
has exited. A stop ends such processes too.

`kill` succeeds only when the run has ended. If the sidecar has not recorded
the end after about 20 s, `kill` exits 1. If the sidecar is gone, `kill` stops
the child and records the run as `SidecarLost`.

A run whose sidecar has not recorded it yet is still starting; this
happens when its `start` or `run` was interrupted while it waited for the
run to start. `kill` and `start --replace` say `<name> is starting; waiting
for its sidecar to record the run` on stderr, wait up to 11 s for the
record, and then stop or replace the run. If it is still not recorded, they
exit 1 with `<name> is still starting after 11 s`. A second `start` and
`status` exit 1 with `session <name> is starting` and leave the run alone.

`wait` and `kill` return once the sidecar has released the session, so a
`prune` or `start --replace` straight after them finds the session free. If the
sidecar still holds it after 2 s, they return anyway with a warning on stderr.

**Pruning.** `prune` deletes only finished sessions. Names resolve in
`--namespace`, or in `default`. `--dry-run` shows what it would delete. It also
removes stale attach sockets in `~/.tendr/sockets`. With `--older-than`, it
also removes events in `~/.tendr/lost+found` stamped longer ago than that, and
`--on-exit` results in `~/.tendr/callbacks` written longer ago; the summary
line counts them as `lost_found_removed` and `callbacks_removed`, and their
bytes in `bytes_reclaimed`. A session's event log starts a new segment file
every 64 MiB and keeps them all until the session is pruned
([session storage](architecture/02-session-storage.md#limits)).

PTY recordings are never deleted on their own. `prune
--recordings-older-than <duration>` deletes the recordings of runs that ended
longer ago than that, and `--recordings-max <size>` (`B`, `KiB`, `MiB`, `GiB`
or `TiB`) then deletes those of the runs that ended first until all
recordings together take at most that much. Give either or both, with
`--namespace` and `--dry-run` if you like; they cannot be combined with
session names, `--older-than` or `--all`. The sessions stay: `status` shows
the recording with `deleted_at`. A run still going keeps its recording, which
counts toward the total. Each recording deleted is a `"type":"recording"`
line with `"action":"delete"`, its session, `dir`, `bytes` and `reason`
(`older_than` or `over_max`); one that could not be deleted is the same line
with `"action":"error"` and an `error`; each one kept because its run is
going is a line with `"skip_reason":"running"`; and a `recording_summary`
line ends the output. Only pruning sessions sweeps stale attach sockets.

**Hooks.** `--on-exit` splits its command into argv like a shell would, but
runs no shell: no variable expansion, pipes or `&&`. Put that logic in a
script. The command gets the [environment variables](#environment-variables),
including `TENDR_EXIT_REASON`. Tendr logs the results as a `callback.finished`
event.

**Foreground work.** `tendr run job -- bash ./job.sh` uses the same explicit
launch specification as `start`, streams output and exits with the child's
code. For detached work, use `tendr start job -- bash ./job.sh`. Configure
namespace, working directory, environment and callbacks on the command line,
not inside the script. See [Run in the foreground](#run-in-the-foreground).

**Annotations.** Inside a session, `tendr wrap` runs a command and records it
in the session's event log:

```bash
tendr wrap --source worker.local --event work.step -- ./step.sh
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
|`start`, `status`, `list`, `log`, `push`, `kill`, `wait`, `watch`, `exec`|Run on the remote host. The request travels as data on the SSH stdin, so no remote shell parses it. Through a POSIX login shell, output and exit codes are byte for byte those of the command run there.|
|`attach`|The request travels the same way, from macOS, Linux or Windows, and your terminal stays local ([Answer prompts and attach](#answer-prompts-and-attach)).|
|`run`, `wrap`, `prune`, `query`, `guide`, `skill`, `export`|Refused with exit 2 and a ready-to-paste fallback for a POSIX shell, such as `try (POSIX shell):  ssh data-box 'tendr run deploy -- sh deploy.sh'`.|
|`events`, `emit`|Refused with exit 1.|

**Versions.** The remote `tendr` must be 0.5.0 or newer for every command,
`exec` included. An older one fails at once, and tendr says to upgrade it.
Some features need more:

- Reconnecting a dropped `attach --host` needs 0.6.0 or newer on the remote
  host, and a session started by 0.6.0 or newer. Otherwise the attach ends
  with ssh's exit.
- Watching (`attach --read-only`) needs 0.7.0 or newer on both hosts, and a
  session started by 0.7.0 or newer. An older remote refuses it with 80, or
  with 81 when a human is attached, and tendr adds a line saying the remote
  is too old. The 81's own hint, `--takeover`, would take the terminal from
  that human: do not follow it to watch. A session started by an older
  `tendr` refuses it with 80.
- PowerShell `exec` keeping variables and functions between calls (see
  [PowerShell](#powershell)) needs 0.8.0 or newer on the remote host. An
  older remote runs each `exec` in a scope of its own, where only `$global:`
  variables persist.
- Saying how much of what you typed before a drop reached the session needs
  0.8.0 or newer on both hosts, and a session started by 0.8.0 or newer.
  Otherwise the attach reconnects as before and says nothing of it.
- `start --record-input` and `start --no-record` need 0.8.0 or newer on the
  remote host. An older one would ignore them, so tendr sends such a request
  in a form it refuses instead (exit 2, `unsupported remote request
  version 2`).
- The ready check (`start --ready-when-output`, `--ready-when-port`,
  `--ready-within`, `--after-ready`, and `wait --ready`) needs 0.8.0 or
  newer on the remote host. An older remote refuses the request with exit 2
  and `unsupported remote request version 2`, before anything runs; a
  request without these flags still goes to it as before.
- A clean launch environment (`--env-mode clean` or `--pass-env`) requires a
  remote supporting frame version 3. Values resolve there and never travel
  in the frame; an older remote rejects version 3 before launching.
- `start --log-delimiter newline-or-cr` needs 0.8.0 or newer on the remote
  host ([Partial lines](#partial-lines-and-progress-output)). An older remote
  refuses the request with exit 2 (`unsupported remote request version 2`)
  rather than start the session without it. `--all-updates` and
  `status`'s `pipe_reader` need 0.8.0 too; an older remote shows every update
  and no `pipe_reader`.

An older client still works against a newer remote.

**Invalid requests.** The remote `tendr` checks a request before it does
anything. One it refuses, such as a bad session name or a malformed `--env`,
exits 2 with `tendr _remote:` and the reason, where the same command run
locally exits 1. A `--cwd` that is not valid UTF-8 cannot be sent, and exits 2
before `ssh` starts.

**Windows hosts.** Your values arrive as data whatever the remote host's
default ssh shell: `cmd.exe`, PowerShell and POSIX shells only ever run a fixed
`tendr` command, so they cannot act on a value. A PowerShell login shell
reports any failure as ssh exit 1, so the remote `tendr` also sends its exit
code as its last stderr line, which tendr reads and hides: exit codes come
through PowerShell too. CI checks values, output, exit codes and `exec`
against a real Windows OpenSSH server on x64 and ARM64, under `cmd.exe`,
Windows PowerShell 5.1 and pwsh 7 login shells.
Values are carried exactly, not translated, so a Windows session takes
Windows commands and paths. If you wrap a local-only command in `ssh`
yourself, quote for the remote shell, for example with
`powershell -NoProfile -Command`.

**Connect deadline.** Every `--host` command must authenticate and start the
remote `tendr` within 30 s, or tendr stops `ssh` and exits 82, naming the
host. Without it, a lapsed Tailscale SSH check-mode approval waits about 30
minutes with no prompt.

- Once the remote `tendr` starts, the command runs as long as it needs, so
  `wait --timeout 3600` and `log -f` are unaffected. `ssh` checks the link
  every 5 s (`ServerAliveInterval=5`, `ServerAliveCountMax=3`), so a dead
  network ends a command with ssh's 255 within about 15 to 20 s, instead of
  leaving it hanging. If your ssh config shares connections
  (`ControlMaster`), a command that rides an existing master gets the
  master's checks instead, and one that starts over a master whose network
  is gone ends at the connect deadline. `attach` always makes its own
  connection.
- Time spent at an `ssh` prompt (passphrase, unknown host key) counts.
- `TENDR_SSH_CONNECT_TIMEOUT` sets the limit in seconds; `0` turns it off.
- A remote `tendr` too old for this client fails on the first flag it does
  not know (`--hello`, or `--exit-trailer` with the deadline off), and the
  error says to upgrade it.
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
  tendr start job --replace -- bash ./job.sh
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

## Wait for a published event

A process can publish structured events with `tendr emit`, or run a command
and record its result with `tendr wrap`. A supervised child inherits the
`TENDR_*` session identity; a caller outside the run supplies `--session`.
Tendr stores these events and filters their fields. It does not interpret
application completion, agent turns or permission decisions.

For an existing session such as `analysis/py` started below, a worker can
publish an application-defined result:

```bash
tendr emit --session analysis/py --source worker.local --kind work.finished \
  --data '{"request_id":"r1","ok":true}'
```

The caller waits for the corresponding event, whether already stored or
published while it waits:

```bash
tendr events --session analysis/py --kind work.finished --data request_id=r1 \
  --follow --max-count 1 --timeout 10m
```

- `--max-count <N>` ends the follow with 0 once it has printed N matching
  events. That means an event matched, not that the application succeeded:
  the caller reads its payload. `--kind` and `--source` match prefixes.
- `--timeout <duration>` ends it with 124 if the deadline passes first,
  counted from the start of the command. Both options need `--follow`.
  A deadline means the outcome is unknown; it does not cancel the producer.
- `--data <key>=<value>` matches a top-level field, a string by its text or
  a number as JSON writes it. Repeat it to require several fields; it
  combines with `--session`, `--kind` and `--source`, with or without
  `--follow`. See [event protocol §5.1](architecture/07-event-protocol.md).
- Payloads over 16 KiB are stored as a preview without their fields, so
  `--data` cannot match them. Project the fields needed for matching into
  a small payload before emitting.
- Use a unique request identity or a stored cursor/time to exclude stale
  results. `--since <ts>` includes stored events from that time;
  `--from-now` excludes events present when the command starts, even if
  they are the result you need. Start a `--from-now` follower before the
  action and use `--ready-file` to know when it is subscribed.
- `--ready-file` is created after replay, even when replay already held N
  matches. With `--cursors`, a final bookmark follows the Nth event.
- `events` is local-only. For a remote producer, run it on that host, for
  example `ssh box 'tendr events … --follow --max-count 1 --timeout 10m'`.
  These bounded-follow options need tendr 0.8.0 or newer there.

Events are not authenticated: `source` is caller-supplied except for reserved
`tendr.*` names. Never grant permission on an event alone. `--best-effort`
can lose events silently; a failed producer or a replaced/pruned session can
also leave the caller waiting. Bound waits and treat missing results as
unknown, not as evidence that the work is still running or safe to repeat.

## Tendr alongside Herdr Core and a harness

[Herdr Core (`hc`)](https://github.com/herdrdev/herdr-core) runs persistent
interactive terminals. It is not the older `herdr pane`/`herdr session` CLI.
The [design decision](design.md#harness-herdr-core-and-tendr-have-separate-owners)
sets these responsibilities:

|Owner|Responsibility|
|---|---|
|Harness (omp, Claude Code, Codex)|Model/tool loop, context, permission policy, task completion, backend selection and work-retention/cancellation policy|
|hc|Hosting interactive harness/agent terminals, screen/history, attach, program-reported status, terminal messaging and terminal-child lifetimes|
|tendr|Supervised work processes, stateful command/results, logs/events, and generic pipe or PTY transport/recording/attach|

### Compose through the harness

An hc terminal may host a harness that invokes tendr as a tool. Tendr's
sidecar is the primary supervisor and owns its child's transport and runtime
record, not the caller's terminal. That does not remove enclosing lifetime
authority: hc cleanup metadata, OS jobs, cgroups, containers and host lifetime
can still end the run. Neither hc nor a harness is a tendr dependency.
Native harness tools remain the default when adequate.

For example, launch an interactive harness with hc:

```bash
hc run --bg --name workbench claude
```

In that harness's tool calls, use tendr for a stateful worker while the
hosting terminal lives; this is not a post-terminal survival guarantee:

```bash
tendr start py --namespace analysis --stdin -- python3 -i
tendr exec py --namespace analysis -- 'data = list(range(1000))'
tendr exec py --namespace analysis -- 'print(sum(data))'
```

The harness reads tendr's structured results. A person can attach to the hc
agent terminal without taking over that worker. Generic tendr PTY sessions
remain available for work that needs a terminal; shell `exec` requires the
pipe lane, not a PTY (the Python REPL target is the PTY exception).

Choose one primary supervisor, authoritative runtime record and transport
owner per work process; enclosing lifetime authority can still apply. Do not
wrap an hc-hosted job in tendr as if both supervised it:
`tendr start … -- hc run --bg …` tracks the hc launcher, whose
exit says nothing about the hosted program. A deliberate one-shot `hc exec`
is a separate process, not a command inside an existing terminal; `tendr exec`
addresses an existing session. Do not silently switch between them.

### Lifecycle and recovery

- Keep the backend's identity: an hc machine/terminal address, or a tendr
  host, namespace, session and run ID. Inherited environment may also carry
  backend-specific lifetime metadata; do not assume detachment removes it
  or strip it implicitly.
- An hc agent turn ending, a tendr command returning, a work process exiting
  and the harness task finishing are distinct facts. hc stores status
  reported through OSC 7501; it does not infer it from screen text.
  Reported status is neither permission authority nor proof of task success.
  Tendr's `events` transports published facts without adopting agent-turn
  meanings; the harness decides whether the task is complete.
- hc's child-terminal policy and its process-scope cleanup are different
  mechanisms. `hc run --outlive-parent` governs an hc child terminal; it is not a
  proven escape for detached tendr descendants. Work started inside an hc
  terminal can be swept when that terminal's program ends, despite `setsid`.
  The harness must explicitly stop task-scoped work and establish a separate,
  verified lifetime boundary for independently retained work. This composition
  supplies no such boundary and no guarantee of survival or complete cleanup.
- Stop waiting, interrupt an operation, and kill a process are different
  actions. A timed-out `tendr exec` can still be running; do not resend work
  until its outcome is known. hc one-shot `exec` has its own timeout/kill
  contract; these are not interchangeable cancellation APIs.
- Reconnect to known work and inspect its identity/status before relaunching.
  hc holders preserve programs across core restarts; `hc run --keep` recovery
  starts a fresh shell, not the original task or its in-memory state.
  A tendr process lost with its host is not restored from its logs.
- Namespaces, hc sessions and tendr's `--boundary` declarations are not
  sandboxes. The harness's permission policy is not isolation from other
  processes running with the same account's access.

### Observed hc cleanup-scope limit

On 2026-11-06, macOS 27, hc 0.1.18 and tendr's 0.7.2 development build
(runtime source base `9e69346a6370a4a8de8aed327ae1ffafe5af9510`), an isolated
hc session used scratch HOME/XDG directories. Its `hc run --no-parent
--no-notify` program was a shell that started `python3 -i` through
`tendr start --stdin`, then completed `tendr exec` with
`data = list(range(1000))`. The shell exited 0; a subsequent `tendr status`
reported `SidecarLost`, with inferred provenance, after the successful exec.
The private hc core remained alive.

In the control, the same tendr worker was started outside that private hc
terminal scope. An hc terminal acted only as its client; after that client
exited, the worker was still `Running` with the same run ID and returned
`499500` from `print(sum(data))`. This verifies that scope boundary on macOS,
not universal survival under every enclosing runtime or operating system.

The mechanism is explicit in hc:
[`holder::reap`](https://github.com/herdrdev/herdr-core/blob/c6b316f87c33e6f2ab1a1ceea3e3461e2b6bb61a/crates/herdr-core/src/holder.rs#L422-L437)
sweeps the terminal's scope before reporting its exit, and
[`sweep`](https://github.com/herdrdev/herdr-core/blob/c6b316f87c33e6f2ab1a1ceea3e3461e2b6bb61a/crates/herdr-core/src/sweep.rs)
finds inherited `HC_SCOPE` across Unix session detachment and signals matching
processes. Linux uses that token mechanism too; its runtime behavior was not
exercised here. Windows uses Job Objects instead; survival under its complete
job hierarchy was not verified. Do not silently unset hc's lifetime metadata
or impersonate its protected runtime to manufacture independence.
An explicit `--env-mode clean` launch instead removes arbitrary inherited
environment metadata before the sidecar starts. This is generic environment
selection, not an hc exception; it does not escape OS containment.

With this change on the same macOS/hc versions, a private `hc run` shell
started a Bash worker using `tendr start work --env-mode clean --stdin`,
then set `PROBE_MARK=retained` through `tendr exec` and exited 0. After hc
reported that terminal's exit, tendr still reported `Running`; another
`tendr exec` returned `retained`. This verifies explicit clean-mode
independence from that hc cleanup scope, not from enclosing cgroups,
Windows jobs or containers.

This is a composition contract, not an hc adapter, shared registry or
cancellation bridge. [hc's API](https://github.com/herdrdev/herdr-core/blob/c6b316f87c33e6f2ab1a1ceea3e3461e2b6bb61a/schema/README.md)
and [machine model](https://github.com/herdrdev/herdr-core/blob/c6b316f87c33e6f2ab1a1ceea3e3461e2b6bb61a/docs/machines.md)
own its behavior; tendr does not copy its screen or agent-status state.

## Exit codes

Each command has its own codes, so one number can mean different things in
different commands. Every command exits 0 on success. It exits 1 on an error
with no code of its own below, such as session not found, session not running
or an I/O error.

|Command|Code|Meaning|
|---|---|---|
|any|2|Usage error from argument parsing, or invalid environment policy/names/missing passed values before a launch|
|any, with `--host`|2|Invalid destination; a local-only command (printed with an `ssh` fallback); a `--cwd` that is not UTF-8; a request the remote `tendr` refused as invalid (`tendr _remote:` on stderr); a remote `tendr` too old for the request (1 when the remote login shell is PowerShell)|
|`events`, `emit` with `--host`|1|Not supported over SSH|
|any, with `--host`|the remote's|The remote `tendr`'s exit code; 255 without one from the remote is `ssh` failing to connect (`attach` reconnects instead, once it is attached)|
|any, with `--host`|82|`ssh` did not authenticate and start the remote `tendr` within the connect deadline (`TENDR_SSH_CONNECT_TIMEOUT`, default 30 s); while `attach --host` reconnects, it tries again instead|
|`start`|2, 5|The child failed to spawn; the sidecar failed|
|`run`|the child's|The child's own code, or 2 spawn failed, 3 sidecar lost, 4 dependency failed, 5 sidecar failed, 124 timed out, 137 killed|
|`wait`|1|`--timeout` passed before the sessions ended (with `--ready`, before they were ready or not); with `--ready`, a session has no ready condition|
|`wait`|2, 3, 4, 5|Spawn failed, sidecar lost, dependency failed, sidecar failed|
|`wait`|42|The child exited non-zero (any code)|
|`wait`|124, 137|A dependency timed out; killed while waiting on a dependency|
|`wait --ready`|43|A session will not be ready: it missed its `--ready-within` deadline, or its run ended before it was ready|
|`exec`|the command's|The command's own exit code (an NDJSON reply's is 1 for `"ok": false` or a non-null `error`); 124 if `--timeout` passed; 2 for a malformed `--frame-from-stdin` frame or an invalid NDJSON request|
|`events`|44, 65|`--from-cursor` names history that is gone; `--strict` found unparseable lines|
|`events`|124|`--timeout` passed (before `--max-count` events were printed, when given)|
|`emit`|2, 3, 5, 6|Invalid `--parent`; no session given and none in `TENDR_SESSION`, or an invalid `TENDR_NAMESPACE`/`TENDR_SESSION`; corrupt session; invalid kind or source|
|`wrap`|the child's|The wrapped command's code; 6 for an `--event` with a reserved prefix|
|`query`|DuckDB's|DuckDB's exit code|
|`guide`, `skill install`|2|Unknown topic; refusing to overwrite a modified skill file|
|`attach`, `push` to a PTY session|80|Protocol: the session answered the hello outside this attach protocol, or not at all from a session started by a `tendr` older than 0.4.0 (within 10 s; any later session is waited for until it answers or closes the connection), an `attach --host` got output that does not follow on from the last, or its reconnect reached a session that cannot resume or that claims more of its input written than it sent, or an `attach --read-only` reached a session started by a `tendr` without read-only viewers, or, over `--host`, a remote `tendr` older than 0.7.0 (tendr adds a line saying so)|
|`attach`, `push` to a PTY session|81|Control conflict: another client holds the terminal (a plain `attach` while a human is attached, or a `push` while anyone is), another client's `--takeover` took the attach (also when the attach had stopped reading its output and learns of it only later), or the session's run has ended. An `attach --host` whose reconnect is refused exits 81 too: someone took the terminal while it was disconnected (an attach, a takeover, or an agent's `push` still running), or the run ended meanwhile (`not running`). So does an `attach --read-only --host` whose remote `tendr`, older than 0.7.0, finds a human attached; tendr adds a line saying the remote is too old, and that the `--takeover` it suggests would take the terminal from that human|
|`attach`, `push` to a PTY session|82, 83|Reserved, not used yet|
|`attach`, `push` to a PTY session|84|Runtime or transport: the attach socket is missing or unreachable, the connection fails, the session already serves as many connections of that kind as it admits (eight read-only viewers, or eight attaches, takeovers, resumes and pushes), an `attach --read-only` was cut off short of the run's output (at the run's end too, when it was not reading its last bytes within 2 s; with a session started by an older `tendr`, only without a terminal or over `--host`, and only while the run went on), or a push ended revoked or stopped with bytes unwritten (stderr gives the byte counts)|
|`attach`, `push` to a PTY session|85|Identity: the socket's listener, or the connecting client, is another user|

With several sessions, `wait` returns the most severe code, and `wait --ready`
returns 43 if any session reported is not ready. A session that was
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
|`TENDR_BLOCK_ID`|`tendr wrap`, and `exec` in a POSIX shell or PowerShell, for the command they run|ID that links events emitted inside the command to its wrap or exec event|
|`TENDR_PARENT_EVENT_ID`|`tendr wrap`, for its command|ID of the wrap event|
|`PSModulePath`|tendr, on Windows, for a child or `tendr wrap` command that is Windows PowerShell (`powershell`)|The inherited value without pwsh 7's module directories; see [PowerShell](#powershell)|
|`TENDR_SSH_CONNECT_TIMEOUT`|you|Connect deadline for `--host` commands, in seconds. Default 30; `0` turns it off.|

`wrap` and `emit` read `TENDR_SESSION`, `TENDR_NAMESPACE`, `TENDR_RUN_ID` and
`TENDR_GENERATION` to find their session and run.

## See also

- [Architecture](architecture/README.md) · [Design](design.md) · [Plans](plans/README.md)
- [Analytics recipes](analytics-recipes.md)
- [`using-tendr` skill](../src/embedded/SKILL.md): the agent-facing entry point to `tendr guide`
