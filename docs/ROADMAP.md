# Roadmap

The planned work, as an index of the open plans. Each line links to its plan;
the plan holds the detail and its current status. Shipped work is in the
[changelog](../CHANGELOG.md).

A test checks that this list matches `docs/plans/` exactly. The rules for plans
are in the [plans README](plans/README.md).

## Now

In queue order.

1. [Cloud PTY control and replay](plans/active/00_cloud-pty-control.md): a
   recorded Claude Code PTY session on a cloud VM that a person can reconnect
   to. Slice 1 is finishing; slices 2–4 (replay, typed SSH bridge, screen
   reads) have not started.
2. [Remote frame transport](plans/active/01_remote-frame-transport.md): send
   every `--host` command as a typed frame over SSH stdin, which also closes
   the Windows remote-shell quoting gap.
3. [Polling consolidation](plans/active/02_polling-consolidation.md): move the
   test suite's polling loops onto the shared `poll_until` helpers, with one
   deadline policy.

## Backlog

Not in order. Each plan says what would start it.

- [Agent hook routing](plans/backlog/agent-hook-routing.md): a guide topic on
  sending an agent's lifecycle hooks to the event log with `tendr emit` or
  `tendr wrap`.
- [Interactive agent turns](plans/backlog/interactive-agent-turns.md): drive a
  live agent's turns in a PTY session through its hook events, and let
  `tendr events --follow` exit on its first match.
- [Capture stop drain](plans/backlog/capture-stop-drain.md): keep reading a
  child's output after a log write fails, so the run cannot hang.
- [Log delimiter: CR framing](plans/backlog/log-delimiter-cr-framing.md):
  optionally split log records on a lone carriage return, so pipe sessions show
  progress meters as they happen.
- [Partial-line telemetry](plans/backlog/partial-line-telemetry.md): report
  buffered partial-line bytes and the time since the last log record, so a
  silent session can be explained.
- [Log retention](plans/backlog/log-retention.md): keep `output.log`, event
  segments and lost+found from growing without limit in long-running sessions.
- [Windows parity](plans/backlog/windows-parity.md): the same observable
  behavior on Windows: a Job-safe launcher, ConPTY and attach, PowerShell state
  and a final parity check.
- [Boo integration](plans/backlog/boo-integration.md): check the
  tendr-supervises-Boo recipe against a pinned Boo build.
- [Content-addressable storage](plans/backlog/content-addressable-storage.md):
  extend the per-session blob store, only when a named consumer needs it.
- [Sidecar control protocol](plans/backlog/sidecar-control-protocol.md): a
  typed request–response channel between the CLI and the sidecar, built only
  when a feature needs one.

## Not in core

These stay outside tendr.

- Terminal emulation and screen rendering: an optional external
  `tendr-screen` extension or Boo (see [decisions](decisions.md)).
- Block-terminal user interfaces, completion, and routing between shell and AI.
- Workflow scheduling and agent logic (see [decisions](decisions.md)).
- Container and Kubernetes lifecycle management.
