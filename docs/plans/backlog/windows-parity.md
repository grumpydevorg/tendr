---
id: windows-parity
depends_on: [remote-frame-transport]
---

# Windows parity

**Status:** not started. Each gap below is a separate change; none waits for
another except ConPTY, which reuses the suspended launcher. General `--host` use
against a Windows host is fixed by
[remote frame transport](../active/01_remote-frame-transport.md), not here.

Goal: a Windows client or host keeps the same observable contracts as POSIX:
commands, output shapes, exit codes, survival across disconnects, and
child-tree ownership. Tendr does not translate Bash to PowerShell or Unix paths
to Windows paths.

Native Windows x64 and ARM64 test lanes already run in required CI. Remote
`exec` against a Windows host was checked once by hand (2026-07-10, Tendr 0.2.0
on an ARM Windows VM over SSH); no test covers it.

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

### Orphan kill stops only the root process

Without the live Job handle (the sidecar crashed), `kill_orphan` terminates one
PID: no descendants and no graceful stop (`src/platform/windows.rs:254-278`).

### PowerShell state

`exec` runs code with `& ([scriptblock]::Create($_code))`
(`src/exec_frame.rs:80`), so `$x = 42` is lost after the call. The guide's
workaround is `$global:x`. The PowerShell frame also does not set
`TENDR_BLOCK_ID` for the payload; the test is ignored on Windows
(`tests/cli_exec.rs:1715-1718`). Fix: keep ordinary assignments and functions
between calls, clean up Tendr's own frame variables, and export
`TENDR_BLOCK_ID`. Check under PowerShell 5.1 and 7.

### No PTY or attach

`--pty` fails with "PTY not supported on Windows yet"
(`src/platform/windows.rs:183`), and `attach` fails with "attach is only
supported on Unix" (`src/commands/attach.rs:43-46`). Needed:

- a ConPTY child backend (`CreatePseudoConsole`, `ResizePseudoConsole`), on the
  suspended launcher above;
- a named-pipe carrier for the existing attach frames;
- a Windows console frontend (raw mode, VT flags, resize, restore on exit).
  Remote attach from a Windows client to a Unix host needs it once remote attach
  moves to `ssh -T`, which remote frame transport owns.

### Remote tests do not run on Windows

`tests/cli_remote.rs` is `#![cfg(unix)]` because its fake `ssh` is a shell
script, so the Windows lanes never test `--host` as a client. The portable
fake-SSH harness is part of remote frame transport.

## Acceptance

Native Windows tests show: a session started over SSH survives disconnect, or
`start` fails clearly; stopping a session stops a child that forks at once;
an orphan kill stops the whole tree; `$x = 42` then `$x` returns 42 under
PowerShell 5.1 and 7; a `--pty` session can be attached, resized, detached and
reattached. The guide's PowerShell and platform notes are updated to match.
