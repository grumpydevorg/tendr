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

Everything after `--` is the child's argv. `start` and `tendr run` share most
flags:

|Flag|Effect|
|---|---|
|`--stdin`|Make a pipe session, so `exec` and `push` can write to it. Use it for shells and REPLs.|
|`--pty`|Make a PTY session, for programs you want to `attach` to (`start` only).|
|`--record-input`, `--no-record`|With `--pty`: also record what is typed, or record nothing ([PTY details](#pty-details); `start` only).|
|`--namespace <ns>`|Put the session in a namespace.|
|`--exec-target <target>`|Set the exec target instead of inferring it (`start` only).|
|`--replace`|Kill an existing session of the same name and start a new run. A run still starting is waited for first, as `kill` does.|
|`--timeout <sec>`|Stop the child after this many seconds, as `kill` does.|
|`--cwd <dir>`, `--env KEY=VALUE`|Set the child's working directory or an environment variable.|
|`--unset-env NAME`|Remove an inherited variable from the child's environment. Repeatable; `--env` of the same name still sets it.|
|`--after <session>`|Wait for that session to exit before starting. Repeatable.|
|`--any-exit`|With `--after`, start even if the dependency exited non-zero.|
|`--on-exit <command>`|Run a command after the child exits. Repeatable.|
|`--ready-when-output <regex>`, `--ready-when-port <port>`|Say when the run is ready: a line of its output matches, or a TCP port on its host accepts connections (`start` only). See [Wait until a server is ready](#wait-until-a-server-is-ready).|
|`--ready-within <duration>`|Record a miss if the run is not ready this long after it starts, such as `30s` or `2m` (`start` only).|
|`--after-ready <session>`|Wait for that session to be ready before starting. Repeatable (`start` only).|

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
typed or pushed, and `replay --export asciicast` writes it out as `i` events;
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

On a Windows host the child runs under a pseudoconsole (ConPTY), so what
`attach` shows and the recording holds is conhost's rendering of the
screen, not the child's own bytes: conhost may repaint or coalesce output,
as it does in any Windows terminal. Ctrl-C reaches the child as a console
Ctrl-C. When the child exits, tendr closes the pseudoconsole, which ends
every other process still attached to that console.

The child gets `TERM=xterm-256color` when `TERM` was missing, empty, `dumb`,
or removed with `--unset-env TERM` for the `tendr start` that launched it, as
in an agent's tool shell or `ssh -T`. Otherwise `TERM` passes through, and
`--env TERM=…` always wins. Other variables pass through unchanged, apart from
`PSModulePath` for Windows PowerShell ([PowerShell](#powershell)). An agent's
shell exports `CI=true` and `NO_COLOR=1`, and Claude Code turns colour off for
any `CI` value, even an empty one, so `--env CI=` cannot undo it. Remove them
with `--unset-env CI --unset-env NO_COLOR`.

### Replay a recording

```bash
tendr replay ~/.tendr/sessions/default/dev          # play it in this terminal
tendr replay --speed 4 <path>                       # four times as fast; --no-delay skips the pauses
tendr replay --export asciicast <path> > dev.cast   # asciicast v2, for asciinema and other players
```

`<path>` is a session directory, its `recording/<run_id>/` directory, or one
segment file (`seg-00000000.tndrrec`). Replay writes the recorded output to
your terminal at the recorded pace, and nothing else: it never connects to a
session, and typed input in a recording is not played. Watch it in a real
terminal such as Ghostty; tendr has no screen of its own.

- Replay cannot resize your window. Before it plays, it shows the size the
  recording starts at and its largest size, and asks you to size the window
  and press a key, unless the window already matches. The bytes are always
  played exactly; if the window did not match the recording, replay says so
  once, after it ends, since the picture may differ. `--strict-geometry`
  instead pauses at each recorded size change, with a note on the bottom
  line, until the window matches. The resizes tendr makes itself so a
  program redraws on attach never ask anything.
- The terminal is in raw mode while replay runs. What it sends back, such as
  answers to the queries in the recording, is read and dropped, and so is
  every key but Ctrl-C, which stops replay (0). SIGHUP, SIGTERM and SIGINT
  stop it too (1), and on Windows so do Ctrl-Break and closing the console.
  However it ends, the modes the recording turned on are turned off and the
  terminal is restored.
- A damaged recording plays nothing: replay exits 1 naming the file and byte
  offset. A recording cut off inside its last record, as a running session's
  can be, plays to its last complete record, and replay says where it was
  cut.
- `--export asciicast` writes asciicast v2 to stdout: output as `o` events,
  recorded input (passwords and tokens included) as `i` events, and every
  window size change as an `r` event, with times since the recording
  started. A character split between records
  is joined. A recording whose output or input is not valid UTF-8 is refused
  (1), naming the record; the recording itself stays the exact copy.
- Replay is local-only. Copy a recording from another host first, such as
  `scp -r box:.tendr/sessions/default/dev/recording ./dev-rec`, then
  `tendr replay ./dev-rec`. On Windows it plays in Windows Terminal or a
  conhost window on Windows 10 1809 or later, as `attach` does.

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
- `tendr events` replays the event log as NDJSON. Filter with
  `--session <ns>/<name>` (exact), and `--kind` and `--source` (prefix matches).
  All three are repeatable. Use `--follow` to keep
  reading, `--since <RFC 3339>` or `--last <N>` to limit history, and
  `--cursors` with `--from-cursor` to resume where you stopped.
  `--include-logs` merges output lines in. `--data <key>=<value>` keeps events
  by a field of their data, and `--max-count <N>` and `--timeout` end a follow
  on a match or at a deadline
  ([Drive an agent turn through its hooks](#drive-an-agent-turn-through-its-hooks)).
- `wait` waits for processes to exit. An interactive agent keeps running
  between turns, so wait for a turn with `tendr events`, not `wait`.
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
tendr run --detach ./job.sh      # run a script as a session
```

**Stopping.** `kill` sends SIGTERM to the session's process group, then SIGKILL
after 5 s. A PTY session first gets SIGHUP and up to 1 s to act on it, as when
a terminal window closes, so an interactive shell also hangs up its background
jobs. On Windows a PTY session's console is closed instead, which its
processes get as `CTRL_CLOSE_EVENT`, and the job ends after 5 s. `--timeout`
stops the same way, so a timed-out child exits up to about 6 s after the
deadline. `kill --force` sends SIGKILL at once.

A stop then ends every other process the run started, wherever it went: a
shell's background jobs, `nohup` and `disown` jobs, `setsid` processes, and
tool shells that Claude Code starts in sessions of their own. `kill` and
`--timeout` send each SIGTERM and give it up to 1 s to exit; then, and at
once for `kill --force`, each one left is stopped and killed. The run's end
is recorded after that, so `kill` returns with them gone. A session you start
with `tendr start` from inside the run is left running (not on Windows, see
below). A stop cannot reach processes started through another manager,
such as `systemd-run --user`, `at` or an already running `tmux` server,
nor end a setuid program; a run
warning names what it could not end. On macOS, where orphans go to launchd,
it also misses a process that left the session, lost its parent and holds
no terminal descriptor, such as `(setsid cmd >/dev/null 2>&1 &)` (run
lifecycle, [Limits](architecture/03-run-lifecycle.md#limits)).

**On Windows** the child starts suspended, is put in a Job Object, and only
then runs, so whatever it starts, at once or later, is in the job. `kill`
and `--timeout` send CTRL_BREAK to the child's process group and end the
job after 5 s; `kill --force` ends it at once. No process can leave the
job: Cygwin and MSYS programs such as Git Bash's `sh` would otherwise let
their children escape it. So, unlike on Unix, a session started with
`tendr start` from inside the run is in the run's job too and ends with the
run; start it from outside the run to keep it. When the
sidecar is gone or does not answer, `kill` finds the run by its Job Object,
or, from another Windows session, by the child's process tree: the job's
name is per Terminal Services session, and every SSH login runs in session
0, apart from a desktop's sign-in. It sends CTRL_BREAK, waits up to 5 s,
then ends what is left. A PID that now names another process is never
stopped.

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
Command-line values win: `--on-exit` replaces every `on-exit` directive,
`--env` entries apply after `env` directives, and `--unset-env NAME` drops an
`env` directive for that name. An unknown directive, or a second copy of a
non-repeatable one, is an error.

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
|`start`, `status`, `list`, `log`, `push`, `kill`, `wait`, `watch`, `exec`|Run on the remote host. The request travels as data on the SSH stdin, so no remote shell parses it. Through a POSIX login shell, output and exit codes are byte for byte those of the command run there.|
|`attach`|The request travels the same way, from macOS, Linux or Windows, and your terminal stays local ([Answer prompts and attach](#answer-prompts-and-attach)).|
|`run`, `wrap`, `prune`, `query`, `guide`, `skill`|Refused with exit 2 and a ready-to-paste fallback for a POSIX shell, such as `try (POSIX shell):  ssh data-box 'tendr run deploy.sh'`.|
|`events`, `emit`, `replay`|Refused with exit 1.|

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
- `start --log-delimiter newline-or-cr` needs 0.8.0 or newer on the remote
  host ([Partial lines](#partial-lines-and-progress-output)). An older remote
  refuses the request with exit 2 (`unsupported remote request version 2`)
  rather than start the session without it. `--all-updates` and
  `status`'s `pipe_reader` need 0.8.0 too; an older remote shows every update
  and no `pipe_reader`.
- A PTY session (`start --pty`) and `attach` on a Windows host need 0.8.0
  or newer there. An older one fails `start --pty`.

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

A session started over SSH on a Windows host outlives the connection: its
sidecar leaves the job Windows OpenSSH runs the connection's processes in,
which can end with the connection. CI checks, against a real Windows
OpenSSH server on x64 and ARM64, that a session is still running after the
connection that started it has closed. If the job does not let its
processes leave (no `JOB_OBJECT_LIMIT_BREAKAWAY_OK`), `start` (and
`run --detach`) exits 1 and says the session could not outlive this SSH
connection, rather than start one that may die with it. tendr knows it
runs for SSH by `SSH_CONNECTION`, which every process started from an SSH
login inherits, so a `tendr start` in a program you launched from an SSH
shell is checked too, and one with the variable removed is not. A
foreground `tendr run` is never refused: it lasts as long as its caller
anyway. A `tendr start` inside a tendr run on Windows is in the run's job,
which nothing leaves; inside a run started over SSH (whose processes have
`SSH_CONNECTION`) it is refused, and the message says the session would
end with the run. Locally, a session started inside such a job (Cargo's,
an IDE's, a run's) stays in it and lasts as long as the job, as one in a
cgroup does on Linux. A remote `tendr` older than 0.8.0 starts the session
inside such a job over SSH too.

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

## Drive an agent turn through its hooks

Some agent features exist only in an interactive session. A program can still
drive one under tendr: it pushes a prompt into the agent's terminal, and the
agent's lifecycle hooks write events that say when the prompt was taken, when
the turn waits for a permission and when it ended. The caller waits on those
events instead of reading the screen.

`tendr wait` waits for a process to exit, not for a turn. An interactive agent
keeps running between turns, so wait for a turn with
`tendr events --follow --max-count`.

Tendr stores these events and does not interpret them: what `hook.stop` means
is the caller's decision. Consumers such as UIs, analytics or `tendr query`
read the `hook.*` events; tendr does not route or render them. For a local
caller that needs one Claude Code turn, `claude --bg` and `claude --bg --resume`
are simpler. Use tendr when the record must be queryable, the session must
survive an SSH loss, or the agent has no background mode.

**The pattern.** A hook runs `tendr emit --kind hook.<name>` to record an
event, or `tendr wrap --event hook.<name> -- <cmd>` to run a command and
record it with its output. Hooks are the agent's children, so they inherit the
session's `TENDR_*` variables and need no `--session`. `hook.` is open
vocabulary, not reserved.

**Wait for one event.** One `tendr events` call blocks until the event arrives
or a deadline passes, with no follower in the background:

```bash
tendr events --session agents/cc --kind hook.stop --data prompt_id=$PROMPT_ID \
  --since "$SUBMIT_TS" --follow --max-count 1 --timeout 10m
```

- `--max-count <N>` ends the follow with 0 once it has printed N events.
- `--timeout <duration>` ends it with 124 if the duration passes first,
  counted from the start of the command. Both need `--follow`.
- `--data <key>=<value>` keeps an event only when its `data` has the top-level
  field `<key>` equal to `<value>`, a string by its text or a number as JSON
  writes it ([event protocol](architecture/07-event-protocol.md) §5.1 has the
  full rule). Repeat it to require several fields. It combines with
  `--session`, `--kind` and `--source`, and works without `--follow` too.
- **A payload over 16 KiB loses its fields** (event protocol §3.4), so
  `--data` cannot match it. Project payloads with `jq` before emitting them,
  as the hook script below does.
- `--from-now` skips what the sessions hold when the command starts, so an
  event written earlier never matches. To wait for a turn's end after its
  submit event, use `--since` with the submit event's `ts` instead: the end may
  already be written, and `--since` replays it.
- With `--ready-file`, the file is created once the replay is printed, even
  when the replay already held N events. With `--cursors`, a last bookmark
  follows the Nth event.
- `events` runs only where the sessions are (it is refused with `--host`).
  For a remote agent, run it there, for example
  `ssh box 'tendr events … --max-count 1 --timeout 10m'`, with tendr 0.8.0 or
  newer on that host.

**Claude Code.** `tendr-hook.sh` gets the hook name as its argument. It keeps
the payload under the 16 KiB cap and writes nothing to stdout, which Claude
Code reads as a hook decision:

```sh
#!/bin/sh
jq -c '{hook_event_name, session_id, prompt_id, transcript_path,
        notification_type, message, tool_name, error,
        last_assistant_message: ((.last_assistant_message // "") | .[0:4000])}' \
  | tendr emit --best-effort --source claude.hook --kind "hook.$1" --data-stdin >/dev/null
```

The settings, given to `claude --settings` so they apply to this run only:

```json
{"hooks":{
  "SessionStart":[{"hooks":[{"type":"command","command":"/abs/tendr-hook.sh session_start"}]}],
  "UserPromptSubmit":[{"hooks":[{"type":"command","command":"/abs/tendr-hook.sh user_prompt_submit"}]}],
  "PermissionRequest":[{"hooks":[{"type":"command","command":"/abs/tendr-hook.sh permission_request"}]}],
  "Notification":[{"hooks":[{"type":"command","command":"/abs/tendr-hook.sh notification"}]}],
  "Stop":[{"hooks":[{"type":"command","command":"/abs/tendr-hook.sh stop"}]}],
  "StopFailure":[{"hooks":[{"type":"command","command":"/abs/tendr-hook.sh stop_failure"}]}]
}}
```

Start each wait before the action it waits for:

```bash
ev() { tendr events --session agents/cc "$@"; }

# 1. Wait for the session start, then launch the resolved binary:
#    tendr start takes argv, so shell aliases do not apply.
rm -f ./ready
ev --kind hook.session_start --follow --from-now --max-count 1 --timeout 60s \
  --ready-file ./ready > ./start.ndjson &
until [ -f ./ready ]; do sleep 0.1; done
CLAUDE=$(readlink -f "$(command -v claude)")
tendr start cc --namespace agents --pty --cwd "$PWD" \
  --unset-env CI --unset-env NO_COLOR \
  -- "$CLAUDE" --settings ./tendr-hooks.json
wait $!
sleep 3   # give the input box a moment

# 2. Submit: text, a pause, then a lone carriage return.
rm -f ./ready
ev --kind hook.user_prompt_submit --follow --from-now --max-count 1 --timeout 30s \
  --ready-file ./ready > ./submit.ndjson &
until [ -f ./ready ]; do sleep 0.1; done
printf 'Summarise README.md in one line.' | tendr push cc --namespace agents
sleep 0.5
printf '\r' | tendr push cc --namespace agents
wait $! || echo "not submitted"   # 124: the prompt was not taken
PROMPT_ID=$(jq -r .data.prompt_id ./submit.ndjson)
SUBMIT_TS=$(jq -r .ts ./submit.ndjson)

# 3. The turn's end: hook.stop, or hook.stop_failure on an API error
#    (--kind is a prefix match).
ev --kind hook.stop --data "prompt_id=$PROMPT_ID" --since "$SUBMIT_TS" \
  --follow --max-count 1 --timeout 10m > ./end.ndjson
# 124: the outcome is unknown, not "still working".
```

While the turn runs, `hook.permission_request`, or a `hook.notification`
with `notification_type` `permission_prompt`, means it waits for a
permission. Decide by your own policy, and answer with `push`, for example
`printf 1 | tendr push cc --namespace agents`.

**Read the result** from the stop event's `last_assistant_message`, or from
the transcript at its `transcript_path`. Scope a transcript query to the turn
by `promptId`: it is on the user entries (the prompt and each tool result),
not on the assistant's. This one finds the URLs of the turn's `Artifact`
calls:

```bash
jq -rs --arg p "$PROMPT_ID" '
  ([.[] | select(.type=="user" and .promptId==$p)
        | .message.content[]? | select(.type=="tool_result") | .tool_use_id]) as $ids
  | [.[] | select(.type=="assistant") | .message.content[]?
         | select(.type=="tool_use" and .name=="Artifact" and (.id as $i | $ids | index($i)))
         | .input.url // empty] | unique[]' "$TRANSCRIPT"
```

On 2026-10-08 it returned nothing for a plain-text turn and the URL for a
`/design` turn of the same session. Claude Code writes the transcript
asynchronously, so it can lag behind the hook: retry a missing result briefly
before treating it as absent. A person can watch or take over with
`tendr attach cc --namespace agents`.

**Measured** on 2026-11-04 on an exe.dev VM (Ubuntu 24.04, x86_64) with Claude
Code 2.1.316 through exe.dev's LLM gateway and tendr 0.7.2 with this section's
`events` options, running the recipe above, unless a point says otherwise:

- Hooks inherit the session: `tendr emit` without `--session` wrote to the
  right session and run. `hook.session_start` reached a `--from-now` waiter
  started before the session existed, 0.4 s after `tendr start`.
- Text, a 0.5 s pause, then `\r` was taken first time, in both turns. A long
  prompt ending in its own `\r` is taken as a paste and the CR is removed
  (2026-10-08, macOS 26.6.2, tendr 0.3.0, Claude Code 2.1.293). Only
  `hook.user_prompt_submit` confirms that a prompt was taken.
- `hook.user_prompt_submit` and `hook.stop` carry the same `prompt_id`;
  the stop also carries `last_assistant_message` and `transcript_path`. In
  the transcript, `promptId` was on the turn's user entries (the prompt and
  its tool result) and on no assistant entry.
- With the API endpoint unreachable, the turn ended with `hook.stop_failure`,
  `error` `server_error`, after 180 s of Claude Code's retries. The
  `--kind hook.stop --data prompt_id=…` wait above caught it.
- A `hook.notification` with `notification_type` `idle_prompt` came after
  each turn had ended. A wait on `hook.notification` must check the type.
- Permission prompts were not re-measured: on that VM Claude Code asked for
  none, even for a `Write` outside its working directory. On 2026-10-08 a
  `hook.notification` with `permission_prompt` arrived 9 s after the prompt
  was taken, and `printf 1 | tendr push` answered it. Claude Code's docs say
  `PermissionRequest` fires at once; that hook has not been seen live.
- A PTY child gets `TERM=xterm-256color` when the caller's `TERM` is `dumb`,
  as in agent shells, which also set `CI=true` and `NO_COLOR=1` (2026-10-08);
  the launch line removes those two.
- A raw 40 KB `Stop` payload lost its `prompt_id` to the spill (2026-10-08);
  the tests check that `--data` does not match such an event.

**Trust and failure.**

- Hook events are not authenticated. Only `tendr.*` sources are reserved, and
  `source` is whatever the caller passes
  ([event protocol](architecture/07-event-protocol.md) §1). Any process in the
  session can emit a `hook.stop` with a guessed `prompt_id`. Never grant a
  permission on the strength of an event alone.
- `--best-effort` loses events silently. A missing hook script, a full disk,
  or a session pruned or replaced mid-turn (its emits go to `lost+found`) all
  look like a slow turn. A missed deadline means the outcome is unknown.
- A user interrupt fires neither `Stop` nor `StopFailure`, so give every wait
  a deadline.
- The payload shapes are Claude Code's and can change with any release. Name
  the fields in the projection, so a rename shows as a missing value.

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
- Run an agent that a person watches in a herdr pane, not under tendr:
  `tendr start --pty -- omp` works, but herdr cannot see that agent's state.
  An agent that a program drives can run under tendr instead
  ([Drive an agent turn through its hooks](#drive-an-agent-turn-through-its-hooks)).

## Exit codes

Each command has its own codes, so one number can mean different things in
different commands. Every command exits 0 on success. It exits 1 on an error
with no code of its own below, such as session not found, session not running
or an I/O error.

|Command|Code|Meaning|
|---|---|---|
|any|2|Usage error from argument parsing|
|any, with `--host`|2|Invalid destination; a local-only command (printed with an `ssh` fallback); a `--cwd` that is not UTF-8; a request the remote `tendr` refused as invalid (`tendr _remote:` on stderr); a remote `tendr` too old for the request (1 when the remote login shell is PowerShell)|
|`events`, `emit`, `replay` with `--host`|1|Not supported over SSH|
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
