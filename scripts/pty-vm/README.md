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
The last results are [below](#results-2026-11-01); a re-run starts there.

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
| `halfopen.sh`, `heal.sh` | VM | Drop one SSH client's packets with `iptables` for N seconds and time when the server notices; the rule is removed four separate ways |
| `halfopen-exe.sh` | Mac | Freeze the local `ssh` of an attached client with SIGSTOP (exe.dev's own SSH path) |
| `halfopen-loopback.sh` | Mac | Attach as `acc` over the VM's OpenSSH and run `halfopen.sh` against that connection |
| `nested-ssh.sh` | Mac | Resize over `ssh -t <vm> tendr attach` from a PTY with `TERM=xterm-ghostty`. [`tests/cli_attach_ssh.rs`](../../tests/cli_attach_ssh.rs) covers the rest: detach, `Ctrl-]`, paste, terminal restore, takeover |
| `ptydrive.py` | both | Runs a command in a fresh PTY from a step file (`expect`, `quiet`, `send`, `resize`) with bounded waits |
| `rep_exit.py <session>` | VM | Repro for a client taken over while stalled: attach a client that never reads, take over, detach the new client, then drain the old one and report its exit code (81 expected) |
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

References:

- [OpenSSH escape characters](https://man.openbsd.org/ssh#ESCAPE_CHARACTERS)
- [Ghostty terminal stream handler](https://github.com/ghostty-org/ghostty/blob/main/src/terminal/stream.zig)
- [pam_systemd logout contract](https://github.com/systemd/systemd/blob/main/man/pam_systemd.xml)
