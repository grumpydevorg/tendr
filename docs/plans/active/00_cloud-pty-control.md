---
id: cloud-pty-control
depends_on: []
---

# Cloud PTY control

**Status:** steps 8 and 9 remain. Step 5's measurement (below) settled step
6's design, and step 6 shipped: attach restores the terminal's modes and
nudges a redraw. Step 10, `--unset-env`, shipped too. Step 8 needs any sshd
(`TENDR_TEST_SSH_HOST`); step 9 the VM.

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

- **Step 8. Nested `ssh -t` fixtures, opt-in** (sshd; `TENDR_TEST_SSH_HOST`).
   `tests/cli_attach_ssh.rs`: detach on `Ctrl-\ d`, `Ctrl-]` passthrough,
   paste with prefixes, terminal restore, takeover over a stopped first
   client.
- **Step 9. Remote acceptance on the Linux VM.**
   - Same Claude pid throughout. Attach from Ghostty; half-open the
     connection two ways, then take over; agent → human → agent.
   - A blocked child input and a stalled viewer during takeover (time it).
   - 32 idle half-open reconnects, each with `--takeover`; fd, thread and
     admission counts equal before and after.
   - Logout and relogin: log out the last session on a logind host with
     lingering off, check no hidden SSH session keeps the login count above
     zero, log in again and attach to the same run and pid. Record whether
     `KillUserProcesses` ends the sidecar; a socket in `~/.tendr/sockets`
     cannot prevent that.
   - Cross-user rejection. It needs two accounts; never mark it passed
     without them.
   - Dump the whole recording.
   - Run Claude Code, omp, a dev server and a shell with background jobs.
     For each, before and after `tendr kill`, and separately `--timeout`,
     snapshot `ps -eo pid,ppid,pgid,sid,comm` and diff. The sid is the run's
     child pid, and anything left with it (shells, MCP servers, background
     jobs) outlived the stop. `kill` hangs up a PTY session first;
     `--timeout` sends SIGKILL at once, so record what it leaves and decide
     whether it should hang up too.
     [Run lifecycle, Limits](../../architecture/03-run-lifecycle.md#limits)
     lists the known cases.

   If descendants survive, choose a fix from this data. The goal is that no
   process the run started outlives the stop on Linux; on macOS, tendr does
   the same or states plainly what it cannot reach. On Linux, cgroup v2
   `cgroup.kill` (5.14 or later) stops every descendant, including those that
   call `setsid`, but needs a delegated cgroup subtree and a fallback without
   one. `PR_SET_CHILD_SUBREAPER` works without cgroups but only catches
   orphans. macOS has neither: the sidecar would list processes before
   signalling them, which races with new forks, and `proc_pidinfo` already
   returns EPERM for a process in another session (`src/platform/unix.rs`).

Step 9 uses the VM harness in
[`scripts/pty-vm/`](../../../scripts/pty-vm/README.md).

## What step 9 must show

| Invariant | Verified by | Limit |
|-----------|-------------|-------|
| Old epochs cannot write after an acknowledged takeover | Half-open SSH, queued and partial writes, blocked child, old disconnect, stale run | Bytes already in the kernel cannot be recalled |
| Terminal bytes stay within the owner's account | Foreign-UID client and server, pre-created path, symlink, bind failure | Needs two test accounts |
| A surviving run stays attachable after logout | Real last-session logout and relogin, lingering on and off | A process-killing login policy needs its own launch setup |
| Reconnects do not accumulate stale clients | 32 idle half-open takeovers; fd, thread and admission counts | No reliance on the SSH front end's idle timeout |
| Detach leaves ordinary Claude input alone | `Ctrl-]`, partial prefix, literal prefix, paste, kitty and `modifyOtherKeys` keys over nested SSH | The prefix is documented, not universally unused |
| Viewers cannot block capture or control | Stalled reader with sustained output and a concurrent takeover | Storage failure stops the recording, visibly |
| Query handling supports the measured workflow | Pinned Claude on the VM, attached vs detached | No claim of general detached terminal support |

## Gates

CI's checks; red-first tests; never mark a VM row passed from local tests.

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
which needs root) is unmeasured. The deployed host logout policies (exe.dev,
NixOS) are not validated.

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
