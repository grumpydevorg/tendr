# Decisions

Why tendr is built the way it is. Each entry is a decision that is hard to
reverse and came from a real choice between alternatives. How things work
today is in the [architecture](architecture/README.md); the rules for judging
new features are in the [design principles](design-principles.md).

Where an entry names a source, the full text is in Git at commit `95dcf1d`,
the last commit before the old plans and specs were removed.

## Tendr supervises processes; it is not an agent framework

*2026-04-03.* This is the "process sitter" decision. Tendr starts, watches,
chains and stops processes. It does not learn LLM protocols, count tokens,
retry API calls or choose models. An agent runs under tendr as its own
command-line program, for example
`tendr start task -- claude -p "…"`. Adding LLM knowledge would turn tendr into
an AI framework, with a different scope, users and maintenance cost. Source:
`docs/plans/specs/decision-process-sitter-not-framework.md`.

## Processes may publish events; tendr does not interpret them

*2026-05-23, refined 2026-07-06.* A supervised process can add structured
events to its session's event log with `tendr emit` or `tendr wrap`. Tendr
stores and orders these events. It does not decide what they mean: "this hook
event means the agent's turn ended" is the consumer's policy. This lets any
tool with lifecycle hooks report its state without tendr carrying
tool-specific code. Source: `docs/plans/specs/tender-as-block-runtime.md`.

## No daemon: one sidecar per run, and files are the truth

*2026-03, confirmed for the event log 2026-07-06.* Each run has its own
sidecar process. It is born with the run and exits after it. All state is in
files: `meta.json` holds the current state of a session, and `events/` holds
its history. Consumers read the files directly. A per-host daemon with an
in-memory index was designed and then rejected in a comparison of three
designs: append-only files met the event log's requirements without adding a
resident process. Source: `docs/plans/specs/persistence-architecture.md` (the
rejected design) and `docs/plans/specs/event-protocol.md` §0.

## No platform supervisor

*2026-03.* Tendr does not use systemd, launchd, Windows services or Task
Scheduler. The sidecar is the supervisor on every platform, so commands
behave the same on Linux, macOS and Windows, inside containers and in CI.
Each platform supervisor exists on one platform only and would need a second
code path. Source: `docs/plans/specs/tender-agent-process-sitter.md`,
"Platform Supervisor Landscape".

## Commands print JSON, and exit codes are a contract

*2026-03.* Commands print JSON by default because their main readers are
programs and agents. Exit codes have fixed meanings (see `tendr guide` and its
exit code table), so a caller branches on an integer, not on message text.
Changing an exit code's meaning is a breaking change.

## `start` is idempotent on the whole launch spec

*2026-03.* Starting a session that is already running with the same launch
spec returns the existing run. The launch spec is the command, its arguments
and the start options, such as working directory, environment, timeout,
dependencies and exit hooks. A different spec under the same name is a session
conflict error. Replacing a run needs `--replace`. Retrying a command is
therefore always safe, and a conflicting request is never silently merged.

## `--after` waits for a run, not a name

*2026-03.* Each run has a `run_id` (a UUIDv7). `--after job1` records the
`run_id` of `job1` when the dependency is bound. If `job1` is replaced while
the dependent waits, the dependent run ends as dependency-failed instead of
following a different run. The generation number is a counter for people and
is not used for this check.

## `output.log` is a line log; the PTY recording is the exact record

*2026-03, recording added 2026-09.* `output.log` stores lines with a timestamp
and a stream tag. It is for reading and searching, and it does not reproduce
bytes exactly. A PTY session also writes an exact recording of its output and
window sizes under `recording/<run_id>/`. A caller that needs exact bytes from
a pipe session redirects the child's output to its own file.

## Windows uses Job Objects and a hidden console

*2026-03 to 2026-04.* Windows has no SIGTERM. The sidecar runs the child in a
Job Object so that stopping it stops all its descendants. A stop first sends
CTRL_BREAK, which is best-effort, then ends the Job Object after the grace
period. For CTRL_BREAK to reach the child, the sidecar allocates a console and
hides its window. Source: `src/platform/windows.rs`.

## Remote work runs the same tendr over SSH

*2026-04-01.* `tendr --host <host> <command>` runs the remote host's own
`tendr` over SSH. There is no broker and no second lifecycle model: a remote
session is a local session on that host. Upgrade `tendr` on both ends
together: a release can change what the two ends send each other.

## No terminal emulator in core

*2026-07-09, revised 2026-09-14.* Tendr owns the PTY, input ownership and the
exact recording. It does not parse or render the screen. Screen reads come
from outside: an optional `tendr-screen` extension that tendr calls through a
versioned command-line contract, or Boo. Core does not link a terminal
emulator such as libghostty, so it gains no Zig or Ghostty build dependency.
Source: [cloud PTY plan](plans/active/00_cloud-pty-control.md) and
`docs/plans/specs/ecosystem-landscape.md`.

## CI stays on native GitHub Actions; Dagger was not adopted

*2026-09-25.* A pilot ran the Linux checks under Dagger beside `ci.yml`.
Coverage and failure reporting were equal, but Dagger added 38–70 s of setup
to every cold hosted job (limit 60 s; msrv was within it in 3 of 6 attempts).
With its engine state cached through `actions/cache`, a one-line edit cost
+47 to +95 s per job against a 15 s limit. Hosted runners keep no engine
between jobs, and the safe ways to keep one need a self-hosted or paid engine.
Source: `docs/plans/completed/2026-09-25-dagger-ci.md` and its data folder;
closed PR #78.
