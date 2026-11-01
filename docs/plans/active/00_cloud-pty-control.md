---
id: cloud-pty-control
depends_on: []
---

# Cloud PTY control

**Status:** steps 11–15 remain: fixes that step 9's acceptance run on the VM
called for. Step 12 needs the VM to verify; the others are local.

## Goal

Ghostty on the laptop → `ssh -t` → tendr on an exe.dev Linux VM → Claude Code
in a tendr-owned PTY. A person can lose the network, attach again with
`--takeover`, resize and detach, and an agent can drive the same PTY. Claude
stays the same process throughout, and its output is recorded exactly. A lost
SSH connection does not end the run; a dead sidecar or a VM reboot does.

What shipped is described in [PTY lane](../../architecture/04-pty-lane.md) and
[PTY recording format](../../architecture/08-pty-recording-format.md),
including how attach turns the session's terminal modes on and off again
([output and viewers](../../architecture/04-pty-lane.md#output-and-viewers))
and the repaint nudge ([recording](../../architecture/04-pty-lane.md#recording)).
Exit codes are in the [guide](../../guide.md#exit-codes). The typed `ssh -T`
attach bridge and resumable viewers are step 5 of
[remote frame transport](01_remote-frame-transport.md). Playing a recording
back is [PTY replay](../backlog/pty-replay.md).

## Remaining steps (one PR each)

- **Step 11. A dead child ends its run** (local). After the child exits,
  capture waits for end of output, which waits for every process holding the
  terminal. A `setsid`, `(cmd &)` or `disown` job, or a tool process omp
  starts (it leaks the terminal on fds 28 and 30), then keeps the run
  `Running` with the child an unreaped zombie; `tendr kill` waits about 20 s
  and prints `kill_sent` with exit 0, and `ended_at` records the holder's
  exit. Repro: `tendr start x --pty -- sh -c "setsid sleep 45 & exec sleep
  600"`, then `tendr kill x`. Fix: once the child has exited and its output
  drained, end capture after a bounded wait (2 s), reap, and add a warning
  naming the pids that still hold the terminal. `kill` does not report
  success for a run that did not end. (`supervise` in
  `src/sidecar/supervised_run.rs`; `PtyReader` in `src/platform/unix.rs`;
  `src/commands/kill.rs`.)
- **Step 12. Stop the whole run** (local; verify on the VM). Decision, from
  the step 9 data below:
   - Linux: the sidecar is a child subreaper (`PR_SET_CHILD_SUBREAPER`), so
     every process the run starts stays its descendant. At a stop, after the
     existing graceful sequence (or at once for `--force`), walk
     `/proc/<sidecar>/task/*/children`, SIGSTOP then SIGKILL each process
     but the sidecar's own helpers, and repeat until a walk finds none.
     Reap adopted pids by pid, never `waitpid(-1)`. This needs no cgroup
     delegation: exe.dev starts sidecars in root-owned `/init.scope`, and a
     logind session scope is root-owned too. A delegated cgroup with
     `cgroup.kill` (measured: 6 escapees gone in 5 ms) is worth adding only
     where the sidecar already sits in one; moving it into the user manager
     ties the run to the user manager's life, which ends at the last logout
     without lingering.
   - macOS: at a stop, list same-user processes whose session is the
     child's, the child's descendants, and processes holding the run's
     terminal; SIGSTOP then SIGKILL, repeated. It cannot reach a process
     that called `setsid`, was orphaned and holds no terminal fd; the docs
     say so. `proc_pidinfo` and `getsid` worked for same-user processes in
     another session on macOS 27, so the EPERM noted in
     `src/platform/unix.rs` needs rechecking.
   - `--timeout` hangs up first, as `kill` does: Claude Code and a shell
     cleaned up their children after a hangup but not after SIGKILL.
   - Tests: each escape kind of the step 9 table ends with the stop, under
     `kill` and `--timeout`; the VM run repeats the table.
- **Step 13. Close inherited descriptors** (local). exe.dev's `exe-init`
  leaks two `/dev/ptmx` masters (fds 23 and 25) into every SSH command, and
  tendr passed them on to the sidecar and the child. `spawn_sidecar` closes
  every inherited fd above 2 except the ready pipe.
- **Step 14. A client taken over while stalled exits 81** (local). An old
  client that had stopped reading during a flood, was taken over, and resumed
  after the new client had detached exited 0 with no "another client took
  over" note (3 of 3 runs). With the new client still attached it exited 81.
  Repro without SSH: start `flood`, attach A on a PTY nobody reads, after 6 s
  take over with B, detach B, then drain A. Also: a frozen old client stays
  alive on the VM after the takeover until it is resumed.
- **Step 15. Small fixes found in step 9** (local, one PR):
   - After `SidecarLost`, `meta.json` still says the recording is
     `Recording`, with no `recording.stopped` event.
   - A socket bind failure is recorded as `spawn_failed_syscall` though
     nothing was spawned.
   - `tendr status` panics with "Broken pipe" when its reader exits early.
   - Only `~/.tendr/sockets` is checked, not its parent: another user who can
     write the parent can make the owner's attach and next start fail
     (denial of service only; no bytes reached them).
   - Keys typed in the same read as `Ctrl-\ d` are usually dropped: detach
     goes ahead of queued input, which the sidecar cancels on release. State
     this in the PTY lane's attach rules, or deliver input queued before the
     detach first, bounded.

## Gates

CI's checks; red-first tests; never mark a VM row passed from local tests.

## Step 9 results (2026-11-01)

On the step 5 VM, with tendr main `a04f061` (modes, repaint, `--unset-env`,
bracketed-paste-safe escape), Claude Code 2.1.316, omp 18.14.0 and Ghostty
1.4.0.

| Invariant | Result |
|-----------|--------|
| Same Claude pid throughout | Pass: pid 17435 through agent push, human typing in Ghostty, agent push, an exe.dev takeover and two reattaches |
| Old epochs cannot write after an acknowledged takeover | Pass on both half-open paths (SIGSTOP of the Mac's ssh; `iptables` drop on the VM's own sshd): bytes typed into the frozen client never reached the child; the new client had control in about 0.5 s; the old one exited 81 after resuming or healing. Exception: step 14 |
| Viewers cannot block capture or control | Pass: with `noread` input full and a stalled viewer during a 250–320 KB/s flood, takeover took 0.071 s on the VM (0.79 s from the Mac), and the recording had no sequence or line gaps. Old input still queued at takeover (50 KB) was dropped |
| Reconnects do not accumulate stale clients | Pass: 32 idle half-open takeovers took the sidecar from 13 fds, 8 threads, 0 clients to 17/10/1 and back to 13/8/0, 3 s after the last takeover, with no SSH timeout involved |
| A surviving run stays attachable after logout | Pass with `KillUserProcesses=no`, lingering off and on: same run and pid after relogin. With `KillUserProcesses=yes` logind kills the sidecar about 9 ms after the last logout; only a lingering user's `systemd-run --user --scope tendr start …` survives |
| Terminal bytes stay within the owner's account | Pass with two accounts: every attempt by `intruder` on `acc`'s socket was refused by permissions; with the socket made reachable, the sidecar refused it before the hello (exit 85). Symlinked or foreign `sockets` directories and every bind failure tested (file, symlink, foreign directory, over-long path, EPERM, EROFS) failed the start with no child. Gaps in step 15 |
| Detach leaves ordinary Claude input alone | Pass from Ghostty in legacy, kitty and `modifyOtherKeys` modes: `Ctrl-]` passes, prefix then a key forwards both, a doubled prefix forwards one, modes come back on reattach. Ghostty turns a pasted `Ctrl-\` into a space, so the paste case ran over nested `ssh -t`, where it arrived intact. The four `tests/cli_attach_ssh.rs` fixtures pass against the VM |
| Query handling supports the measured workflow | Pass: no stall started detached; modes replayed on attach; each same-size reattach recorded a repaint pair with a 1.7–1.9 KB redraw about 16 ms later. The full recording: 134 output and 12 resize records, sequence 1–146, no gaps, clean end |

What survived a stop, counted 30 s after it began:

| Child | After `tendr kill` | After `--timeout` |
|---|---|---|
| Dev server: shell, `http.server`, a forking server with 2 workers, a watcher | 0 of 6 | 0 of 6 |
| Interactive bash with `sleep &`, `setsid`, `nohup`, `(cmd &)`, `disown` jobs | 4 of 6 (all but the plain job) | 5 of 6 |
| Claude Code: tool shell, background task, a `setsid` tail | 1 of 4 (the `setsid` tail) | 3 of 4 |
| omp: a `setsid` tail | 1 of 2 | 1 of 2 |

Claude Code starts each Bash tool shell as a session leader, outside the run's
process group and session. After a hangup it cleaned up its own tasks; after
SIGKILL it could not. A `nohup` job survives a graceful kill (it ignores the
hangup and has its own group), which the run lifecycle's Limits do not list.

## Measurement (step 5, 2026-11-01)

On an exe.dev VM: Ubuntu 24.04.5, kernel 6.12.113, x86_64, 2 vCPU. tendr
0.4.1; Claude Code 2.1.316 through exe.dev's LLM gateway. The scripted
terminal was tmux 3.4 (`launch.sh`, `reattach.sh`, `resize.sh`); the real one
was Ghostty 1.4.0 on macOS, attached over `ssh -t <vm>.exe.xyz` and driven by
`scripts/pty-vm/gt`.

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
  fully. Step 6b uses the shrink-and-restore pair. Checked on the VM after
  6b, reattaching from Ghostty at 47×175: the shrink to 46 rows and the
  restore 91 ms later each drew a full 1.9 KB frame; the first attach, at a
  new size, got no repaint.
- **Detach leaves the terminal in Claude's modes.** After `Ctrl-\ d` the
  alternate screen, mouse tracking (1000, 1002, 1003, 1006), focus events,
  bracketed paste, kitty keyboard flags and `modifyOtherKeys` stay on. In
  Ghostty, mouse moves then typed `ESC[<35;11;5M` into the shell, and `Ctrl-C`
  arrived as `ESC[99;5u` and did not interrupt `cat`. A redraw after reattach
  sends only the mouse modes again. Step 6a fixed both directions.
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
off with any value, empty included, so `--env` cannot undo it; step 10 added
`--unset-env` for that.

## Open measurements

A half-open loss without ACKs (dropping the Mac's packets with a `pf` rule,
which needs root) is unmeasured. exe.dev's VM runs logind with
`KillUserProcesses=no` (step 9); NixOS hosts' logout policy is not checked.

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
