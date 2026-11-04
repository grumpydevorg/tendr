---
id: windows-parity
depends_on: []
---

# Windows parity

**Status:** not started. Left: the Windows 10 1809 conhost check. Starts
when a Windows user hits it.
General `--host` use against a Windows host already travels as
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
A `--pty` session on a Windows host runs under ConPTY and is attached,
resized, detached and reattached on both lanes (`tests/windows_pty.rs`).

## Gaps a user hits today

### Attach client loose ends

- The in-box conhost of Windows 10 1809 may show U+FFFD where a multibyte
  character is split across two writes. Current conhost and Windows
  Terminal do not; untested on 1809.

## Acceptance

On Windows 10 1809's in-box conhost, a multibyte character split across
two writes shows as itself, not U+FFFD.
