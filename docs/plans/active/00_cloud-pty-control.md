---
id: cloud-pty-control
depends_on: []
---

# Cloud PTY control

**Status:** mostly shipped in v0.4.0. Steps 5, 6, 8 and 9 remain. Step 5
waits for a VM; steps 6 and 8 need none.

## Goal

Ghostty on the laptop → `ssh -t` → tendr on an exe.dev Linux VM → Claude Code
in a tendr-owned PTY. A person can lose the network, attach again with
`--takeover`, resize and detach, and an agent can drive the same PTY. Claude
stays the same process throughout, and its output is recorded exactly. A lost
SSH connection does not end the run; a dead sidecar or a VM reboot does.

What shipped is described in [PTY lane](../../architecture/04-pty-lane.md) and
[PTY recording format](../../architecture/08-pty-recording-format.md). Exit
codes are in the [guide](../../guide.md#exit-codes). The typed `ssh -T`
attach bridge and resumable viewers are step 5 of
[remote frame transport](01_remote-frame-transport.md). Playing a recording
back is [PTY replay](../backlog/pty-replay.md).

## Remaining steps (one PR each)

- **Step 5. Query and repaint measurement on the VM** (waits for a VM; docs only).
   - Detached vs attached Claude; same-size reattach redraw; same-size resize
     vs `kill -WINCH` vs rows−1 and restore; `Ctrl-\ d` and `Ctrl-]` through
     Ghostty → `ssh -t`; exe.dev half-open timing.
   - Launch Claude each way the workflow does: from the SSH login shell, from
     an agent's bash tool on the VM, and with `tendr --host` from the laptop
     (`ssh -T`, so sshd sets no `TERM`). A PTY child inherits the caller's
     environment except `TERM`
     ([PTY lane](../../architecture/04-pty-lane.md#output-and-viewers)). omp's
     non-PTY bash tool sets `TERM=dumb`, `CI=true`, `NO_COLOR=1`,
     `EDITOR`/`VISUAL`/`GIT_EDITOR=true`, `PAGER`/`GIT_PAGER=cat` and
     `GIT_TERMINAL_PROMPT=0` (measured locally, 2026-10-30). Record the full
     `env | sort` diff between launch paths and how Claude renders and behaves
     under each; `CI` and `NO_COLOR` are the ones expected to change
     rendering.
   - Use a bounded capture deadline so a query stall shows up. Do not log real
     credentials. `examples/dump-recording.rs` flags the query sequences to
     inspect.
   - Output: a "Measurement" subsection in this plan with the Claude and
     Ghostty versions, the decision for step 6, and whether `start --pty`
     should offer an env unset.
   - If Claude stalls while detached, that blocks this plan. The fix is a
     small documented responder for the measured queries only. It has one
     responder per attach epoch, writes through the input writer, and stays
     silent while a human is attached. Do not invent cursor-position replies.
- **Step 6. Best-effort repaint nudge after (re)attach** (local; design fixed by
   step 5). New `MSG_REPAINT`, recorded as `ResizeCause::Repaint
   { correlation }`; the codec supports it and nothing emits it yet. Prefer a
   same-size repaint if step 5 shows Claude redraws on one; use a tagged
   shrink-and-restore pair only if it does not. A repaint does not restore
   history. Tests:
   `reattach_at_the_same_size_records_a_tagged_repaint_pair`,
   `reattach_at_a_new_size_records_only_a_user_resize`,
   `a_repaint_is_authorized_like_input`.
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
   - Before and after `tendr kill`, and separately `--timeout`, snapshot
     `ps -eo pid,ppid,pgid,sid,comm` and diff. The sid is the Claude pid, and
     anything left with it (shells, MCP servers, background jobs) outlived the
     stop. `kill` hangs up a PTY session first; `--timeout` sends SIGKILL at
     once, so record what it leaves and decide whether it should hang up too.
     [Run lifecycle, Limits](../../architecture/03-run-lifecycle.md#limits)
     lists the known cases.

   If descendants survive, choose a fix from this data. On Linux, cgroup v2
   `cgroup.kill` (5.14 or later) stops every descendant, including those that
   call `setsid`, but needs a delegated cgroup subtree and a fallback without
   one. `PR_SET_CHILD_SUBREAPER` works without cgroups but only catches
   orphans. macOS has neither: the sidecar would list processes before
   signalling them, which races with new forks, and `proc_pidinfo` already
   returns EPERM for a process in another session (`src/platform/unix.rs`).

Steps 5 and 9 have a VM harness in [`scripts/pty-vm/`](../../../scripts/pty-vm/README.md):
VM setup, test children, half-open timers and a PTY driver. It was written for
a development build of #68 and has not run on 0.4.0.

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

Every step: `cargo fmt --all --check`, `cargo clippy --all-targets --locked
-- -D warnings`, `cargo test --locked --all-targets --no-fail-fast`, `cargo
test --locked --doc`, `cargo +1.85.0 check --locked --all-targets`, `cargo doc
--no-deps` with `-D warnings`. Write each new test first and see it fail.
Never mark a VM row passed from local tests. Record versions, commands,
results and untested limits in the PR.

## Open measurements

exe.dev's half-open SSH timeout is unmeasured and is not a correctness
dependency. Claude's same-size repaint and its handling of the detach prefix
through Ghostty are step-5 measurements. The deployed host logout policies
(exe.dev, NixOS) are not validated.

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
