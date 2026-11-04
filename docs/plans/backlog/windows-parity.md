---
id: windows-parity
depends_on: []
---

# Windows parity

**Status:** not started. Starts when a Windows user hits a gap below. Each gap
is a separate change; none waits for
another except ConPTY, which reuses the suspended launcher. General `--host` use
against a Windows host already travels as
[remote frames](../../architecture/06-transport-boundaries.md#the-remote-frame)
and is not part of this plan.

Goal: a Windows client or host keeps the same observable contracts as POSIX:
commands, output shapes, exit codes, survival across disconnects, and
child-tree ownership. Tendr does not translate Bash to PowerShell or Unix paths
to Windows paths.

Native Windows x64 and ARM64 test lanes already run in required CI. Remote
`exec` into a PowerShell session on a Windows host is tested on both lanes
through a real OpenSSH server (`tests/cli_remote_windows_ssh.rs`). A Windows
client attaches to a session on a Unix host over the attach bridge, from
Windows Terminal or conhost, and reconnects after a dropped connection
(`tests/windows_attach_console.rs`); a hand run on a Windows 11 ARM64 desktop
checked it end to end ([results](../../../scripts/pty-vm/README.md#windows-console-client-over-attach---host)).

## Gaps a user hits today

### Windows child containment

`spawn_child` starts the child with `std::process::Command` and assigns it to
the Job Object afterwards (`src/platform/windows.rs:143-164`). A child that
starts descendants at once can place them outside the job, and a stop then
misses them. Fix: launch with raw `CreateProcessW(CREATE_SUSPENDED |
CREATE_NEW_PROCESS_GROUP)`, assign the suspended process to the job, then
resume its thread.

### Remote start can die with the SSH session

The sidecar breaks away from its parent's job only when that job allows it
(`parent_job_allows_breakaway`, `src/platform/windows.rs:490`; flag set at
`:636-638`). When breakaway is forbidden, the sidecar silently inherits the
job, so a `start` that succeeded over SSH can die when the connection closes.
Fix: launch independently by a supported mechanism, or make `start` fail with
a clear error when it cannot guarantee the session outlives the caller.

Measured on GitHub's Windows runners (2026-11-01; cmd.exe and powershell.exe
5.1 login shells; x64 with the inbox `OpenSSH_9.5p2 for Windows`, ARM64 with
the portable `OpenSSH_10.4p1 for Windows`, release `10.4.0.0p1-Preview`): a
run started over OpenSSH was still `Running` 10 s after the session closed
(`tests/cli_remote_windows_ssh.rs`,
[run 41292804852](https://github.com/grumpydevorg/tendr/actions/runs/41292804852)).

### Orphan kill stops only the root process

Without the live Job handle (the sidecar crashed), `kill_orphan` terminates one
PID: no descendants and no graceful stop (`src/platform/windows.rs:254-278`).

### No PTY or attach on a Windows host

`--pty` fails with "PTY not supported on Windows yet"
(`src/platform/windows.rs:183`), and `attach` to a session on a Windows host
fails with "attach is only supported on Unix" (`src/commands/attach.rs`, and
as the attach bridge's remote end in `src/commands/attach/bridge.rs`).
Needed:

- a ConPTY child backend (`CreatePseudoConsole`, `ResizePseudoConsole`), on the
  suspended launcher above;
- a named-pipe carrier for the existing attach frames.

### Attach client loose ends

- The in-box conhost of Windows 10 1809 may show U+FFFD where a multibyte
  character is split across two writes. Current conhost and Windows
  Terminal do not; untested on 1809.

## Acceptance

Native Windows tests show:

- a session started over SSH survives disconnect, or `start` fails clearly;
- stopping a session stops a child that forks at once, and an orphan kill
  stops the whole tree;
- a recycled PID is never killed in place of the session's process;
- repeated connects to the stdin named pipe work;
- several processes appending events at once leave complete, valid JSONL (a
  native multi-process stress test);
- a `--pty` session can be attached, resized, detached and reattached.

The guide's PowerShell and platform notes are updated to match.
