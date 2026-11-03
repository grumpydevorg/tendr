# PTY acceptance scripts for a Linux VM

Scripts that measure how Claude Code behaves in a tendr PTY session on a VM,
and check attach, takeover, half-open SSH connections and what survives a
stop. The workflow they test: Ghostty on a laptop, `ssh -t` to tendr on an
exe.dev Linux VM, Claude Code in a tendr-owned PTY. A person can lose the
network, attach again with `--takeover`, resize and detach, and an agent can
drive the same PTY, with Claude the same process throughout and its output
recorded exactly. What shipped for it is described in the
[PTY lane](../../docs/architecture/04-pty-lane.md) and the
[PTY recording format](../../docs/architecture/08-pty-recording-format.md).
The last results are [below](#results-2026-11-03); a re-run starts there.

They were written on an exe.dev VM on 2026-09-26 and first used on
2026-11-01, with tendr 0.4.1, Claude Code 2.1.316 and Ghostty 1.4.0.
The scripts the 2026-11-01 runs did not use (`smoke.sh`, the half-open pair
on the VM's own sshd, `nested-ssh.sh`) have not run since attach moved to
exit codes 80–85.

## Set up the VM

The VM needs Debian or Ubuntu with systemd, `python3` and passwordless `sudo`.

1. On the Mac: `VM_HOST=<vm> ./sync-vm.sh` copies these scripts to `~/slice1`
   on the VM.
2. On the VM, from `~/slice1`:
   1. `sudo ./provision-root.sh '<ssh public key>'`: adds the test users `acc`
      and `intruder` and an OpenSSH server with PAM and logind on
      `127.0.0.1:2222`. Save its last line as `known_hosts.acc` next to these
      scripts on the Mac.
   2. Install tendr into `/usr/local/bin`.
   3. `./install-claude.sh`: installs Claude Code, stops if its checksum does
      not match, and points the Claude of `acc` and of the login user at
      exe.dev's LLM gateway, so no credential is stored on the VM. It also
      settles Claude's first-run screens for `~/slice1/work` and `~/work`.
   4. `./install-helpers.sh`: installs the test children, `tsnap`, `iptables`,
      `jq` and `tmux`.
   5. For `treekill/run-agent.sh … omp`: install omp 18.14.0 with `curl -fsSL
      -o omp-linux-x64 https://github.com/can1357/oh-my-pi/releases/download/v18.14.0/omp-linux-x64
      && install -m755 omp-linux-x64 ~/.local/bin/omp`, then start
      `omp --profile s9k` once in a terminal and settle its setup screens.

Run `sync-vm.sh` again after changing a script.

## Scripts

| Script | Runs on | Does |
|---|---|---|
| `gt` | Mac | Drives a Ghostty window through Ghostty's AppleScript dictionary: open one running a command, send keys, paste text, move the mouse, close it |
| `vmx` | Mac | Runs a command on the VM without a terminal, with a timeout |
| `sync-vm.sh` | Mac | Copies the scripts to `~/slice1` on the VM |
| `launch.sh` | VM | Starts Claude (or a stand-in) in a PTY session, watches it detached, then attaches from a tmux pane and sends one prompt |
| `reattach.sh` | VM | Detaches, waits, reattaches at the same size, and measures what the child redraws |
| `resize.sh` | VM | Compares a same-size resize, `kill -WINCH`, and rows−1 then restore |
| `smoke.sh` | VM | Start, push, attach, detach, refused second attach, takeover, kill, prune |
| `halfopen.sh`, `heal.sh` | VM | Drop one SSH client's packets with `iptables` for N seconds and time when the server notices; the rule is removed four separate ways. Without `--client-port` it drops every client's packets to the port, new connections included; `--both` also drops sshd's replies |
| `halfopen-exe.sh` | Mac | Freeze the local `ssh` of an attached client with SIGSTOP (exe.dev's own SSH path) |
| `halfopen-loopback.sh` | Mac | Attach as `acc` over the VM's OpenSSH and run `halfopen.sh` against that connection |
| `nested-ssh.sh` | Mac | Resize over `ssh -t <vm> tendr attach` from a PTY with `TERM=xterm-ghostty`. [`tests/cli_attach_ssh.rs`](../../tests/cli_attach_ssh.rs) covers the rest: detach, `Ctrl-]`, paste, terminal restore, takeover |
| `ptydrive.py` | both | Runs a command in a fresh PTY from a step file (`expect`, `quiet`, `send`, `resize`) with bounded waits |
| `rep_exit.py <session>` | VM | Repro for a client taken over while stalled: attach a client that never reads, take over, detach the new client, then drain the old one and report its exit code (81 expected) |
| `resume.py drop\|takeover\|gone` | Mac | Attaches with `tendr --host <alias> attach` from a fresh PTY, drops the link with `halfopen.sh` and checks the resume: the terminal's bytes, status rows taken out, against the recording; the reconnect's exit 81 after a takeover; the gone notice, mode reset and redraw after more than 32 MiB. Options: `--child claude\|sh`, `--drop N`, `--both yes`, `--master open\|none`, `--prompt TEXT`. Needs `VM_HOST`, `TENDR_BIN` and an `ssh` on `PATH` that resolves the alias (below) |
| `recout.py <session> output\|len\|resizes` | VM | Reads a session's recording: its output bytes, their count, or its resize records with their output offsets |
| `inject.py <session> <tag> <bytes> [<bytes_per_s>]` | VM | Writes numbered fields to the session's terminal (its PTY's slave side), which the sidecar captures as the child's output: known bytes in flight beside Claude Code |
| `readonly.py admit\|stall\|resume\|takeover\|pipe` | Mac | Read-only viewers over `tendr --host <alias>` against a `keyprobe` session, each viewer's terminal checked against the recording: eight watchers and a controller with a ninth refused, then a takeover while watchers type (`admit`); watchers and controllers admitted beside four stalled hellos, refused beside five (`stall`); a watcher resuming after its own connection is dropped beside an untouched controller (`resume`); a takeover while the controller and a watcher are both stalled (`takeover`); no-terminal `attach --read-only` into a pipe, to the run's end and cut off (`pipe`). Option `--drop N`. Needs what `resume.py` needs |
| `stall.py <session> <n> <seconds>` | VM | Holds n connections to the session's attach socket that never send a hello, reopening each the sidecar closes |
| `treekill/treekill.py NAME MODE SETTLE [--timeout N] [--push DELAY:TEXT]… -- CMD…` | VM | Starts `CMD` in a PTY session tagged `S9K_TAG=NAME`, snapshots its process tree, stops it (`kill`, `force` or `timeout`) and reports, from 0 to 30 s after, which processes of the run (tagged, same session or group, or descendants) are still alive; writes `NAME.survivors` |
| `treekill/run-shell.sh`, `treekill/run-agent.sh`, `treekill/devwrap.sh` | VM | The process-tree children of the [survivor table](#what-survives-a-stop): an interactive bash with `&`, `setsid`, `nohup`, `(cmd &)` and `disown` jobs; Claude Code or omp told to start a background and a `setsid` task; a dev server with forked workers |
| `treekill/reap.py NAME…` | VM | Kills the survivors `treekill.py` recorded, if their pid and start time still match |
| `lib.sh` | VM | Shared settings and helpers; sourced, not run |

The test children installed by `install-helpers.sh`:

- `keyprobe` prints every byte it receives and every SIGWINCH;
- `noread` never reads its input;
- `flood` writes about 256 KiB/s;
- `slowread` reads nothing for N seconds, then reports what arrived.

`tsnap <user> <session>` prints the sidecar's open files and threads, the attach
clients and the login sessions, for before-and-after comparisons.

The Mac-side scripts need `VM_HOST` and GNU `timeout` as `gtimeout`. All but
`vmx` and `sync-vm.sh` also need `SSH_BIN`, an absolute path to `ssh`; those two
use it when set and `ssh` from `PATH` otherwise. `halfopen-loopback.sh` also
needs `ACC_KEY`, the private half of the key given to `provision-root.sh`.

`resume.py` reaches `acc` through tendr's own `ssh <host>`, so the alias has to
resolve in `ssh` on `PATH` without touching `~/.ssh/config`. The 2026-11-02
run used a throwaway key added to `acc`'s `authorized_keys`, and a wrapper
`/tmp/tvm/bin/ssh` (`exec /usr/bin/ssh -F /tmp/tvm/config "$@"`) first on
`PATH`, with this config, so Rick's own settings (ControlMaster, keepalives)
applied:

```text
Include /Users/rick/.ssh/config
Host tvm
    HostName 127.0.0.1
    Port 2222
    User acc
    IdentityFile /tmp/tvm/key
    HostKeyAlias slice1-acc
    UserKnownHostsFile /tmp/tvm/known_hosts.acc
    StrictHostKeyChecking yes
    ProxyJump <vm>.exe.xyz
```

## Results (2026-11-03)

### Read-only viewers over `attach --host`

The acceptance of read-only viewers, the last step of the remote frame
transport plan (`docs/plans/active/01_remote-frame-transport.md`), which this
run closed and retired. On `tendr-pty.exe.xyz`: Ubuntu 24.04.5, kernel
6.12.113, x86_64, 2 vCPU, OpenSSH 9.6p1 (`sshd` on `127.0.0.1:2222`). The
client: macOS 27.0.1 (arm64), OpenSSH 10.3p1, `ssh` resolving `tvm` as
[above](#scripts). tendr: main `0da4bf2` (0.6.0), release builds, on both
ends.

Every run: `PATH=/tmp/tvm/bin:$PATH VM_HOST=tendr-pty.exe.xyz
TENDR_BIN=<checkout>/target/release/tendr readonly.py <scenario>`. The child
is `keyprobe`, so every byte that reaches it is in the recording. Each
viewer's terminal bytes, status rows taken out, are compared with the
recording from the offset taken just before it attached. A watcher is
admitted when its terminal gets the mode replay. A drop is `halfopen.sh
--both --client-port` on one viewer's connection, singled out with `ss`
before and after it attached, so only that connection is cut and the
reconnect's new one is not; ssh's keepalives (5 s × 3) notice it.

| Item | Result |
|------|--------|
| Eight watchers and a controller; a ninth refused (`admit`, two runs) | Pass. Each watcher was admitted 0.48–0.68 s after it started; sshd then held 8 connections, 9 with the controller. A ninth watcher exited 84 with `read-only attach refused: too many attach connections`, both before the controller attached and after. The controller got control 0.83–0.85 s after it started, with eight watchers attached |
| Four stalled hellos (`stall`) | Pass. `stall.py` held connections to the session's socket that never sent a hello; the sidecar closed each after 5.12–5.24 s (`attach requires a protocol hello`) and `stall.py` reopened it within 0.5 ms. With five held, a watcher and a controller both exited 84 (`too many attach connections`). With four held, 5 of 5 rounds admitted a watcher (0.48–0.62 s) and a controller (control after 0.79–0.90 s), the controller's keystrokes reached the watcher, and both detached with 0; round 3 ran 0.3 s after the four were reopened |
| A takeover with watchers attached; watchers' keystrokes (`admit`, two runs) | Pass. With eight watchers attached, `attach --takeover` over `--host` took control, and the first controller exited 81 (`another client took over this session`) 0.59–0.67 s after the takeover started. All eight watchers kept running and got both controllers' echoed keystrokes and 40 KB written after the takeover. Each showed the recording from its attach to the end (41,296 bytes) exactly and once, between the 8-byte mode replay and reset. Every watcher typed before and after the takeover; the child received only `@C1@@C2@`. Each watcher's status line said `tendr: read-only attach; typed input is dropped`. The event log's `pty.control_changed`: attach, takeover, detach, all the controllers' |
| A watcher resumes after a real drop; the controller untouched (`resume --drop 30`, two runs; `inject.py` at 40 KB/s throughout) | Pass. The watcher noticed the drop after 19.25 s and 19.42 s and resumed 0.51 s and 0.61 s later, on a new connection while the old one was still dropped. It showed the recording from its attach on (3,000,053 bytes) exactly and once, with no injected field outside it. The controller showed its 3,000,086 bytes exactly, never saw a loss, kept control, and its keystrokes after the resume reached the child. The only `pty.control_changed`, before the drop and after the resume, was the controller's attach |
| A takeover while the controller and a watcher are stalled (`takeover --drop 30`: both connections dropped, then 4 MiB written so both senders block) | Pass. The takeover over `--host` was admitted 0.57 s after it started and its keystrokes reached the child 1.17 s after it started. The watcher noticed the drop after 18.68 s, resumed 0.49 s later and showed its 4,196,431 bytes exactly and once. The old controller's reconnect was refused (`resume refused: cannot resume: control changed while disconnected; attach again`), exit 81. `pty.control_changed`: attach, takeover, detach |
| No terminal (`pipe`, two runs) | Pass. `tendr --host tvm attach --read-only rp 2>err \| cat`, against a child that writes 4,000 lines and then `LAST-BYTES-OF-THE-RUN` and exits at once: exit 0 after 5.09–5.19 s, stdout the recording exactly (180,021 bytes, ending with the last bytes), stderr empty. Cut off: `\| (sleep 40; cat)` while 96 MiB were written in 2.7 s: exit 84 after 42.2 s with `tendr: the session cut this read-only attach off while the run was still running` on stderr, the run still running, and stdout exactly the recording's first 2,357,248 and 2,375,671 bytes (sha256) |

### Windows console client over `attach --host`

The first run of the Windows console client (PRs #177, #179, #181) on a real
Windows desktop, done by hand rather than by these scripts. Client: Windows 11
10.0.26200.9457, ARM64, a Parallels VM; Windows Terminal 1.24.12741.0 and the
in-box conhost; OpenSSH_for_Windows 9.5p1. tendr 0.7.0, the released
`tendr-aarch64-pc-windows-msvc` asset (sha256 checked). Remote: the VM above,
tendr `v0.7.0` (`1ea81d3`), user `acc` on its `sshd` at `127.0.0.1:2222`,
reached through `ssh -L 10.211.55.2:2222:127.0.0.1:2222 tendr-pty.exe.xyz` on
the Mac. Keys came from the VM's virtual keyboard (`prlctl send-key-event -j`);
each result was read off a screenshot (`prlctl capture`). A drop was killing
that forward, which closes the TCP connection: a half-open loss on Windows is
unmeasured.

| Item | Result |
|------|--------|
| Keystrokes reach the child | Pass. All 94 printable ASCII characters, typed three times into `cat` under `stty -echo`, reached the remote file exactly. Arrivals were 26–68 ms apart, the keyboard's pace |
| Output | Pass in both terminals. `✓ café 中文 😀` rendered correctly; `\xff\xfe` showed as replacement characters and the attach went on |
| Resize | Pass. Maximising Windows Terminal took `stty size` from `30 120` to `46 155`; `top` then filled 46×155 |
| Ctrl-C | Pass. It ended `sleep 100` in the session; the attach stayed up |
| Reconnect while idle, and while output flows | Pass. The last rows read `tendr: connection lost, reconnecting (attempt 1)` over ssh's last line (`Connection to 10.211.55.2 closed by remote host.`); once the forward was back it resumed by itself. Cut at `tick-10` of 30 written 0.5 s apart, the screen then showed `tick-11` to `tick-30` and `TICKS-DONE` in order, once each |
| A watcher beside the controller | Pass. A second window with `--read-only` showed `tendr: read-only attach; typed input is dropped`; a command typed there never ran, and it showed the controller's command and output. `pty.control` stayed `HumanControl`, with no `pty.control_changed` from the watcher |
| `Ctrl-\ d` | Pass. Watcher and controller each exited 0, the controller after the child had turned on bracketed paste and SGR mouse reporting; cmd's prompt then read and echoed typing normally |
| Closing the conhost window (`WM_CLOSE`, as its close button sends) | Pass. tendr and ssh were gone 0.62 s later; the run kept running, `pty.control` went to `AgentControl`, and no `_remote` was left on the VM |

What it showed beyond the tests:

- In conhost, Alt+F4 and Alt+Space reach the session (`^[[1;3S`, `^[ `)
  instead of closing the window or opening its menu. `ssh -t` in conhost does
  the same: it is the console's virtual terminal input, which tendr needs. The
  close button still works, and Windows Terminal keeps both keys.
- The read-only note stayed on the window's last row after the watcher left
  (fixed in #189: leaving now erases it).
- A reconnect erases the rows its status took, and a shell does not redraw
  them: the line of earlier output there stays blank.

## Results (2026-11-02)

### Resuming `attach --host`

The acceptance of resumable viewers (step 1 of the remote frame transport
plan at `d282062`, closed by this run),
on `tendr-pty.exe.xyz`: Ubuntu 24.04.5, kernel 6.12.113, x86_64, 2 vCPU,
OpenSSH 9.6p1 (`sshd` on `127.0.0.1:2222`), Claude Code 2.1.316 through
exe.dev's LLM gateway. The client: macOS 27.0.1 (arm64), OpenSSH 10.3p1,
`ssh` resolving `tvm` as [above](#scripts), so Rick's `Host *` settings
applied: `ControlMaster auto`, `ControlPath ~/.ssh/sockets/%r@%h-%p`,
`ControlPersist 600`, `ServerAliveInterval 60`, `ServerAliveCountMax 3`.
tendr: main `4c4df4a` on both ends before the fix, the branch of #168 for the
runs after it, and its merge `d282062` on both ends for the final runs.

Every run: `PATH=/tmp/tvm/bin:$PATH VM_HOST=tendr-pty.exe.xyz
TENDR_BIN=<checkout>/target/release/tendr resume.py <scenario> …`, Claude Code
in the session at 40×120, `TERM=xterm-256color` (it uses the alternate
screen there). The drop is `iptables` on the VM, so detection is ssh's
keepalives: 5 s × 3 when they apply. Times are from the Mac.

| Item | Result |
|------|--------|
| No gap, no duplicate, redraw (`drop --drop 30 --prompt 'Count from 1 to 30, one number per line.'`, `inject.py` writing 40 KB/s throughout) | Pass. Before the fix, no master: noticed 20.2 s after the drop, resumed 0.9 s after the heal; the terminal got the recording from the attach on, 3,005,682 bytes, exactly and once, plus only the 54-byte mode replay before it and the 54-byte reset after it. After the resume a repaint pair was recorded and Claude cleared and redrew its screen on both halves (1.3 KB and 1.5 KB, Claude's answer and prompt). After the fix, master open: noticed 19.9 s after the drop, resumed 0.6 s after the heal, 3,005,807 bytes exact. On `d282062`, master open: 3,005,902 bytes exact; the Mac slept for 238 s in the middle of that reconnect (a maintenance sleep, from `pmset -g log`), and it resumed exactly after the wake |
| A takeover while stalled exits 81 (`takeover --drop 45 --both yes`: a `tendr attach --takeover` on the VM, then detach, 5 s into the drop) | Pass. The reconnect noticed the drop after 18.3 s (17.5 s on `d282062`), failed while the drop held, and the first attempt after the heal was refused: `resume refused: cannot resume: control changed while disconnected; attach again`, exit 81, 5.3 s (5.1 s) after the heal, with the reason printed again once the terminal was restored. With only the client's packets dropped (`takeover --drop 40`), sshd's still arrived, the client never saw the link as lost, and it exited 81 with `tendr: another client took over this session` 5.1 s after the heal |
| The gone path (`gone --drop 120 --both yes`: 40 MiB written by `inject.py` during the outage, in 1.1 s on the VM; not `TENDR_TEST_OUTPUT_RING_BYTES`, which the release build ignores) | Pass, twice (the second on `d282062` with a master open). The notice: 41,943,052 bytes missed, 8,388,620 no longer held. The terminal got the recording exactly up to its offset, then the mode reset (`?1049l ?1003l ?1004l ?1006l ?2004l ?2031l ?25h`), the notice, the mode replay, and the recording from the head exactly; none of the 3,813,004 injected fields. A repaint pair followed, and the final screen, rendered with `pyte`, was Claude's clean prompt. The notice was printed again after the exit. It came 19.4 s after the heal, at the next attempt in the 30 s backoff |
| Keepalives with ControlMaster (`drop --child sh --master open\|none`) | Fail on `4c4df4a`, fixed in #168. With a master open (`ssh tvm true` first), tendr's ssh was a mux client: it sends no keepalives, and the master's 60 s × 3 decided. The drop was noticed after **239.8 s**. With no master, tendr's own ssh became a ControlPersist master, forked with its `-o` options, and noticed it after 20.2 s and 19.7 s. Reconnect attempts did not ride the dead master: nothing noticed the drop until the master exited, removing its socket, and attempt 1 made a new connection. A new connection started over a master whose link is dead does ride it: `tendr --host tvm list` ended with 82 after 30.06 s (the connect deadline), against 10.04 s (`ConnectTimeout`) with no sharing. Sharing saves 0.40 s a call (0.10 against 0.50 s through the jump host; 0.08 against 0.40 s direct to the VM), so #168 opts only `attach` out (`-o ControlMaster=no -o ControlPath=none`, both argvs) and documents the master's keepalives for other calls. After it, with a master open: 19.35 s (bash) and 19.9 s (Claude) |
| Tailscale approval URL in the status line | Withdrawn: a lapsed Tailscale SSH check-mode approval prints no URL, prompt or error; ssh just stalls (about 30 minutes). A reconnect attempt into it stops at the 30 s connect deadline and is tried again, so there is no URL to show and nothing tailnet-specific to check |

Found on the way and fixed in #168: with its own connection, ssh's
`Timeout, server 127.0.0.1 not responding.` went into the session's screen,
133,412 bytes into a bash run's output. Once the attach owns the terminal,
ssh's stderr now goes to the status line (the timeout shows under
`tendr: connection lost`), or is printed after the attach ends.

The status line during a drop: `tendr: connection lost, reconnecting
(attempt N)` or `…, attempt N failed, next in S s`, and below it ssh's
latest line: `Timeout, server 127.0.0.1 not responding.`, then the jump
host's `Connection timed out during banner exchange` while the drop held.

## Results (2026-11-01)

A row passes only when it was run on a VM; a local test does not stand in
for it.

### Attach, takeover and recording

On the exe.dev VM below, with tendr main `a04f061` (modes, repaint,
`--unset-env`, bracketed-paste-safe escape), Claude Code 2.1.316, omp 18.14.0
and Ghostty 1.4.0.

| Invariant | Result |
|-----------|--------|
| Same Claude pid throughout | Pass: pid 17435 through agent push, human typing in Ghostty, agent push, an exe.dev takeover and two reattaches |
| Old epochs cannot write after an acknowledged takeover | Pass on both half-open paths (SIGSTOP of the Mac's ssh; `iptables` drop on the VM's own sshd): bytes typed into the frozen client never reached the child; the new client had control in 0.5–0.7 s; the old one exited 81 after resuming or healing. A client taken over while stalled, resumed after the new client detached, exited 0 on `a04f061`; with #141's build on the VM, its two CLI tests passed 3 of 3 runs and `rep_exit.py` printed the takeover note 3 of 3 |
| Viewers cannot block capture or control | Pass: with input full and a stalled viewer during a 250–320 KB/s flood, takeover took 0.071 s on the VM (0.79 s from the Mac), and the recording had no sequence or line gaps. Input still queued at takeover was dropped (50 KB in the `slowread` runs) |
| Reconnects do not accumulate stale clients | Pass: through 32 idle half-open takeovers the sidecar went from 13 fds, 8 threads, 0 clients to 17/10/1 (one live client) 3 s after the last takeover, unchanged after the heal, and back to 13/8/0 once that client left; no SSH timeout involved |
| A surviving run stays attachable after logout | Pass with `KillUserProcesses=no`, lingering off and on: same run and pid after relogin. With `KillUserProcesses=yes` logind removed the session 9 ms after the disconnect, and the sidecar and child were gone at the first check 1.4 s later; only a lingering user's `systemd-run --user --scope tendr start …` survived |
| Terminal bytes stay within the owner's account | Pass with two accounts: every attempt by `intruder` on `acc`'s socket was refused by permissions; with the socket made reachable, the sidecar answered `REJECTED` before reading the hello, and the CLI exits 85 on a foreign listener. Symlinked or foreign `sockets` directories and every bind failure tested (file, symlink, foreign directory, over-long path, EPERM, EROFS) failed the start with no child. Not reachable on the VM, so covered only by the unit test in `src/attach_socket.rs`: an existing file at the computed socket path. A writable parent of `sockets/` let another user break the owner's attach (denial of service only) |
| Detach leaves ordinary Claude input alone | Pass from Ghostty in legacy, kitty and `modifyOtherKeys` modes: `Ctrl-]` passes, prefix then a key forwards both, a doubled prefix forwards one, modes come back on reattach. Ghostty turns a pasted `Ctrl-\` into a space, so the paste case ran over nested `ssh -t`, where it arrived intact. `TENDR_TEST_SSH_HOST=tendr-pty.exe.xyz TENDR_TEST_SSH_TENDR=/usr/local/bin/tendr cargo test --test cli_attach_ssh` (tests at `b2a4e3d`): 4 ran, 4 passed, 12 s |
| Query handling supports the measured workflow | Pass: no stall started detached; modes replayed on attach; each same-size reattach recorded a repaint pair with a 1.7–1.9 KB redraw about 16 ms later. The full recording: 134 output and 12 resize records, sequence 1–146, no gaps, clean end |

### What survives a stop

Measured with `treekill/`, counting processes alive 30 s after the stop
began. The first two columns are tendr `a04f061`, which stopped only the
child's process group; the last is the build of #145, which stops the whole
run on Linux.

| Child | After `tendr kill` | After `--timeout` | With #145 (Linux): `kill`, `kill --force`, `--timeout` |
|---|---|---|---|
| Dev server: shell, `http.server`, a forking server with 2 workers, a watcher | 0 of 6 | 0 of 6 | 0, not run, 0 |
| Interactive bash with `sleep &`, `setsid`, `nohup`, `(cmd &)`, `disown` jobs | 4 of 6 (every job but the plain one; bash itself exited) | 5 of 6 | 0, 0 of 7, 0 |
| Claude Code: tool shell, background task, a `setsid` tail | 1 of 4 (the `setsid` tail) | 3 of 4 | 0, not run, 0 |
| omp: a `setsid` tail | 1 of 2 | 1 of 2 | 0, not run, 0 |

Claude Code starts each Bash tool shell as a session leader, outside the run's
process group and session. After a hangup its tasks were gone within 0.5 s;
after SIGKILL they survived. A `nohup` job survives a graceful kill: it
ignores the hangup and has its own group. In a delegated cgroup (only
`user@1000.service` on the VM), `cgroup.kill` ended a tree of six processes,
including `setsid -f` with HUP and TERM ignored, in 5 ms; a subreaper
adopted all five escapees of an exiting shell.

### Claude Code in a tendr PTY

On an exe.dev VM: Ubuntu 24.04.5, kernel 6.12.113, x86_64, 2 vCPU. tendr
0.4.1; Claude Code 2.1.316 through exe.dev's LLM gateway. The scripted
terminal was tmux 3.4 (`launch.sh`, `reattach.sh`, `resize.sh`); the real one
was Ghostty 1.4.0 on macOS, attached over `ssh -t <vm>.exe.xyz` and driven by
`gt`.

- **Detached start does not stall.** About 0.4 s after start Claude sends
  XTVERSION, a kitty keyboard query, `OSC 7501` and DA1. Nobody answers while
  detached. Claude draws its screen anyway, and answers a prompt after a later
  attach. No responder is needed.
- **`TERM` decides the keyboard modes and synchronized output, not the
  replies.** Under `TERM=xterm-ghostty` Claude pushes kitty keyboard flags 5,
  sets `modifyOtherKeys` 2 and draws inside synchronized output before any
  reply could arrive; under `xterm-256color` it does none of these. Started
  with Ghostty attached, the replies lead to more queries (DECRQM 2026 and
  1016, kitty graphics, `CSI 16 t`, DA1).
- **Same-size reattach draws nothing.** Neither does a same-size
  `TIOCSWINSZ` (the kernel sends no SIGWINCH) nor `kill -WINCH` at an
  unchanged size. Rows−1 and then restore each give a full redraw (about
  2 KB, 19 ms and 9 ms after the resize). Attaching at a new size also redraws
  fully. Attach's repaint nudge uses the shrink-and-restore pair. Checked on
  the VM after it shipped, reattaching from Ghostty at 47×175: the shrink to
  46 rows and the restore 91 ms later each drew a full 1.9 KB frame; the
  first attach, at a new size, got no repaint.
- **Detach left the terminal in Claude's modes** before attach restored
  them. After `Ctrl-\ d` the alternate screen, mouse tracking (1000, 1002,
  1003, 1006), focus events, bracketed paste, kitty keyboard flags and
  `modifyOtherKeys` stayed on. In Ghostty, mouse moves then typed
  `ESC[<35;11;5M` into the shell, and `Ctrl-C` arrived as `ESC[99;5u` and
  did not interrupt `cat`. A redraw after reattach sends only the mouse
  modes again. Attach now turns the modes off on leaving and on again on
  attaching.
- **The escape works through Ghostty → `ssh -t` → attach** in both
  encodings. Legacy: `Ctrl-\ d` detaches, `Ctrl-]` reaches the child as
  `0x1d`, a doubled `Ctrl-\` as one `0x1c`. Kitty: `ESC[92;5u` then `d`
  detaches, `Ctrl-]` reaches the child as `ESC[93;5u`, the prefix then `x`
  forwards both, a doubled prefix forwards one.
- **exe.dev half-open:** an attached `ssh -t` frozen with SIGSTOP (its kernel
  still ACKs) for 600 s. The VM kept the attach client and human control the
  whole time; after SIGCONT the same connection worked and detached cleanly.
  exe.dev's front end does not end a silent connection within 10 minutes.
  `--takeover` is the recovery, so this is not a correctness dependency.

Launch paths. `TERM` is repaired when missing, empty or `dumb`; everything
else is inherited from the caller:

| Launched from | `TERM` the child sees | Inherited extras | Claude renders |
|---|---|---|---|
| Login shell, Ghostty → `ssh -t` | `xterm-ghostty` (terminfo in exe.dev's image at `/etc/terminfo`, not from an Ubuntu package) | none; `COLORTERM` is not forwarded | truecolor, kitty keyboard |
| An agent's bash tool on the VM (omp's environment) | `dumb`, repaired to `xterm-256color` | `CI=true`, `NO_COLOR=1`, `EDITOR`, `PAGER` and others | no colour |
| `tendr --host` from the laptop (`ssh -T`) | none, repaired to `xterm-256color` | none | 256 colours |

`NO_COLOR=1` turns colour off and `NO_COLOR=` (empty) does not. `CI` turns it
off with any value, empty included, so `--env` cannot undo it; `--unset-env`
exists for that.

## Not yet measured

A half-open loss without ACKs (dropping the Mac's packets with a `pf` rule,
which needs root) is unmeasured. On exe.dev, the SSH relay creates no logind
session and runs land in `/init.scope`, so a logout cannot touch them; logind
(`KillUserProcesses=no` there) applies only to PAM logins such as the VM's
own sshd. NixOS hosts' logout policy is not checked. The macOS stop (#147)
has not been measured with `treekill/`, which reads `/proc` and runs only
on Linux.

## exe.dev notes

Found while building the harness on an exe.dev VM (2026-09-26):

- `ssh <vm>.exe.xyz` does not reach the VM's sshd over TCP. exe.dev ends SSH
  at its front end and relays to `/exe.dev/bin/exe-init guestd` over vsock
  port 22, so `$SSH_CLIENT` is empty and no VM-side firewall rule can cut the
  connection.
- To half-open that path, break the client end: stopping the local `ssh` with
  SIGSTOP leaves its kernel ACKing TCP, so only the front end's SSH keepalives,
  whose policy is unknown, can notice. Dropping the packets needs a `pf` rule
  on the Mac, which needs root.
- For a VM-side half-open test, run a real OpenSSH server on the VM (the
  harness used `127.0.0.1:2222`, reached with `ssh -J <vm>.exe.xyz -p 2222`)
  and drop that connection's packets with `iptables`.
- Turn SSH connection sharing off for every attach a test breaks or kills
  (`-o ControlMaster=no -o ControlPath=none`). Through a shared connection,
  killing the local `ssh -t` left the VM's `tendr attach` running until a
  `--takeover` cleared it.

References:

- [OpenSSH escape characters](https://man.openbsd.org/ssh#ESCAPE_CHARACTERS)
- [Ghostty terminal stream handler](https://github.com/ghostty-org/ghostty/blob/main/src/terminal/stream.zig)
- [pam_systemd logout contract](https://github.com/systemd/systemd/blob/main/man/pam_systemd.xml)
