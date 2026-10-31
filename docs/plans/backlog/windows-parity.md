---
id: windows-parity
depends_on: [remote-frame-transport]
---

# Windows parity

**Status:** native Windows CI is in place. Windows lifecycle hardening
(phase 4), ConPTY and attach (phase 5) and PowerShell state (phase 6) have not
started. General `--host` framing belongs to
[remote frame transport](../active/01_remote-frame-transport.md).

Goal: a Windows machine, as host or client, presents the same observable
contracts as POSIX for every capability, and required native CI blocks
regressions. Parity covers contracts, not identical OS mechanisms. Tendr does
not translate Bash to PowerShell or Unix paths to Windows paths.

Observable-contract parity means:

- the same commands and state transitions;
- the same stdout, stderr, exit codes and JSON/NDJSON shapes;
- the same persistence across disconnects;
- the same child-tree ownership and termination guarantees;
- the same pipe and PTY capabilities;
- the same remote behavior whatever the login shell is.

Already at parity: distribution (native x64 and ARM64 binaries, checksums,
attestations), remote `exec` (constant-argv frame), and required PR CI (lint
plus native Ubuntu, macOS, Windows x64 and Windows ARM64 test lanes). Local
supervision uses Job Objects and Ctrl-Break but has the lifecycle gaps below.
PowerShell `exec` works, but an ordinary `$x` assignment does not persist.

## Gap inventory

| Track | Current | Parity gap |
|---|---|---|
| General `--host` | only `exec` framed | other ops traverse POSIX shell quoting |
| PR CI | `ci.yml`: lint + Ubuntu/macOS/Windows x64/Windows ARM64 | none; all five checks are required on `main` |
| Windows child containment | Job Objects work | child briefly runs **before** Job assignment (`windows.rs:145`) |
| SSH durability | breakaway works when permitted | forbidden-breakaway **silently** accepts reduced lifetime (`windows.rs:480`) — a "successful" remote start can die on SSH disconnect |
| Orphan recovery | root PID killable | tree-kill/graceful weaker without the live Job handle |
| PTY | Unix impl | no ConPTY child backend |
| Attach target | Unix socket | no Windows named-pipe carrier |
| Attach client | Unix raw mode | no Windows console frontend |
| Remote attach | `ssh -t` + remote frontend | needs `ssh -T` framed bridge without regressing Windows-client→Unix |
| PowerShell state | `$global:x` persists | ordinary `$x = …` runs in the child scriptblock scope |
| Windows event concurrency | atomic-append contract | no native multi-process stress gate |
| Remote tests | shell-script fake ssh, whole file `cfg(unix)` | can't exercise client behavior on Windows |

## Dependency plan

```
Required PR CI (in place)
    ├── remote-frame-transport: fake-SSH harness → typed IR → framed endpoint → migrate ops
    └── Raw suspended Windows launcher (P4)
            → ConPTY backend → named-pipe carrier → console frontend → ssh -T bridge (P5)
PowerShell scope polish (P6) + final parity matrix → declare parity
```

Remote framing and Windows PTY can proceed independently. ConPTY follows the
launcher hardening, because both need raw `CreateProcessW` and
`STARTUPINFOEXW`.

## CI follow-up: release gating

Build and test to temporary Actions artifacts. Only when all lanes pass, create
and publish the release, upload all assets together, then publish to crates.io.
This removes the window in which a release is visible but incomplete.

## General `--host`: owned by remote frame transport

The portable fake-SSH harness, the typed `RemoteOperation` IR, the framed
endpoint, migration of every operation, deletion of reconstructed argv, and
real Windows OpenSSH tests under cmd.exe and PowerShell are steps of
[remote frame transport](../active/01_remote-frame-transport.md). When they
ship, general `--host` reaches Windows parity.

## Phase 4 — Harden Windows lifecycle guarantees

Before ConPTY, replace `std::process::Command` child launch with a raw launcher:
`CreateProcessW(CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP)` → create/configure
Job → assign the suspended process to the Job → resume the primary thread → own
process/thread/pipe handles safely. Closes the escape race where a fast child
spawns descendants before Job assignment.

**SSH breakaway contract.** Silent inheritance of a parent Job when breakaway is
forbidden means a "successful" remote start can die on SSH disconnect. Parity
requires **either** a guaranteed-independent launch via a supported OS mechanism
**or** failing `start` loudly when durable lifetime can't be guaranteed. Silent
degradation must not survive the parity milestone.

Native tests: SSH disconnect doesn't kill a started session; sidecar crash kills
the whole child tree; timeout + forced kill terminate descendants; cooperative
`CTRL_BREAK` exits gracefully; PID reuse never kills the wrong process; repeated
named-pipe connects work; concurrent event writers produce valid complete JSONL.

## Phase 5 — Windows PTY and attach

- **5A ConPTY target**: `CreatePseudoConsole`, I/O pipes,
  `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`, `ResizePseudoConsole`,
  `ClosePseudoConsole`, Job assignment before resume, merged-transcript semantics
  identical to Unix. Tests: interactive pwsh/cmd, Unicode, resize,
  detach/reconnect, timeout+tree-kill, PTY Python exec exception, push-rejection
  while human control is active.
- **5B carrier abstraction**: small duplex trait — Unix domain socket / Windows
  named pipe. Keep the existing `MSG_DATA`/`MSG_RESIZE`/`MSG_DETACH` protocol
  carrier-agnostic.
- **5C Windows client frontend**: console mode save/restore (RAII), VT input/output
  flags, raw keyboard, resize events, Ctrl-C + abnormal-exit restoration. Needed
  even for Windows-client→Unix-host.
- **5D remote attach bridge**: move to `ssh -T` (local frontend ↕ attach messages
  ↕ SSH bytes ↕ remote bridge ↕ socket/pipe). The Unix bridge is
  [cloud PTY control](../active/00_cloud-pty-control.md) slice 3; this step
  adds the Windows carrier and client. Keep `ssh -t` during migration for
  older remotes; remove it once capability/version handling prevents
  regressions. **→ PTY/attach reaches Windows parity.**

## Phase 6 — PowerShell behavioral parity

The frame runs user code via `& ([scriptblock]::Create($_code))` → the surprising
child scope. Rework so ordinary assignments persist while Tendr's internal
capture variables are cleaned afterward. Acceptance (PS 5.1 **and** 7): `$x = 42`
then later `$x` → 42; `function f {7}` then `f` → 7; `Set-Location`/`$env:` persist.
Preserve: clean stdout/stderr, `$LASTEXITCODE`, terminating + non-terminating
errors, cwd reporting, native exit codes, `exit` terminates the session. Polish
today, but a full-parity exit criterion (POSIX assignments persist naturally).

## Final parity qualification

Do not declare full parity until this matrix is green (✓ = native integration
test or explicit platform-independence proof), for every user-facing command:

| Client | Host | Pipe/exec | General control | PTY/attach |
|---|---|---|---|---|
| POSIX | Windows x64 | ✓ | ✓ | ✓ |
| POSIX | Windows ARM64 | ✓ | ✓ | ✓ |
| Windows x64 | POSIX | ✓ | ✓ | ✓ |
| Windows ARM64 | POSIX | ✓ | ✓ | ✓ |
| Windows | Windows | ✓ | ✓ | ✓ |
| POSIX | POSIX | ✓ | ✓ | ✓ |

## Recommended order

1. General `--host` framing ([remote frame transport](../active/01_remote-frame-transport.md))
2. In parallel: raw suspended Windows launcher (phase 4)
3. ConPTY and attach on that launcher (phase 5)
4. Ordinary PowerShell state persistence (phase 6)
5. Final cross-client/host qualification, then update the public support claim

Every step is regression-gated by the required CI from its first commit.
