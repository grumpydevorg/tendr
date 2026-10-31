# Design

The rules for judging a change to tendr, and why tendr is built the way it is.
How the code works today is in the [architecture](architecture/README.md);
open work is in [plans](plans/README.md).

## Scope

Tendr is a runtime for supervised sessions. It owns three layers:

1. **Runtime**: sidecar, child process, PTY and pipe transport, logs, process
   identity, kill and wait.
2. **Session control**: `start`, `exec`, `push`, `attach`, `status`, `wait`,
   `watch`, and the typed state machines behind them.
3. **Composition primitives**: `--after`, `--on-exit`, namespaces, and
   read-only views over session data.

Above these sit workflow policy (scheduling, retries, health rules, what
should be running) and domain tools (compilers, CI, deployment, agents). Add
workflow policy only with care; never absorb a domain tool.

These stay outside tendr:

- terminal emulation and screen rendering (see
  [No terminal emulator in core](#no-terminal-emulator-in-core));
- block-terminal interfaces, completion, and routing between shell and AI;
- agent logic (see
  [Tendr supervises processes](#tendr-supervises-processes-it-is-not-an-agent-framework));
- container and Kubernetes lifecycle management.

## Rules

A change that breaks more than one of these probably belongs above tendr, or
needs to be simpler.

### 1. Mechanism over policy

Tendr gives callers runtime guarantees and small primitives. It does not make
workflow judgments for them. `wait`, `watch`, `--after` and `--on-exit` are
mechanisms; none of them decides what "healthy" means, when to retry, or how
to deploy. Ask: does this give the caller a primitive, or decide for them?

Rejected: a new `exec` strictness flag, proposed before anyone checked
that `tendr exec` already passes on the inner exit code. Check what the
mechanism already does before adding a flag.

### 2. One authority per fact

Every durable fact has one owner and one write path. The sidecar is the normal
writer of lifecycle state; CLI reconciliation is the only other, and it records
provenance for what it writes ([run lifecycle](architecture/03-run-lifecycle.md)).
PTY control changes go through one typed path in the sidecar. Ask: does this
add a second writer for an existing fact?

Rejected: patching `meta.json` as untyped JSON from unrelated code paths. It
creates several writers, bypasses typed invariants and adds races. Also
rejected: PTY control state that said an agent had exclusive input while
`push` still accepted anonymous writes. The transport must enforce authority,
not only record it.

### 3. State machine first, protocol second

For a control feature, define the typed states and legal transitions first,
then the flags, files, sockets and payloads. The run lifecycle is a state
machine, and PTY control is a typed `InputArbiter` with controller epochs
(`src/model/pty_control.rs`). Ask: if the transport disappeared, would the
state model still be clear and testable? If not, the protocol is standing in
for the specification.

Rejected: a control design that started with request files and CLI flags
before its state machine. Invalid states stayed hidden until review, and the
transport became the specification.

### 4. Durable truth, derived views

Tendr stores authoritative facts and derives views from them; it adds no
stored state just to support a view. `meta.json` is the current run,
`output.log` the child's output, and the event log the history. `status`,
`list`, `log`, `watch`, `events` and `query` read those files and store
nothing of their own. Ask: is this a view over existing facts, or a new store
that copies them?

### 5. Separate control and work planes

Control operations (`start`, `status`, `wait`, `watch`, `kill`, `attach`) and
payload bytes (child stdin and output, PTY bytes, framed `exec` payloads) stay
on separate paths. `watch` never writes to child stdin. Do not pass workflow
state through stdin, transcript parsing or shell conventions when a control
field or state transition is the real model.

### 6. Labelled inference

Tendr observes some facts and infers others, and the two must look different.
The sidecar writes a terminal state after it sees the child exit; reconciliation
infers `SidecarLost` from a released lock and a missing terminal state.
`transition_provenance` in `meta.json` and `data.provenance` in lifecycle
events say which kind each transition is. Prefer a provenance field over an
overloaded status name.

### 7. Shallow composition

Tendr supports shallow composition of supervised runs; it is not an
orchestration engine. `--after`, `--on-exit`, `wait`, `watch` and namespaces
compose runs without a retry engine, scheduler or planner. A feature that needs
DAG semantics, health rules, retries or planning belongs in a tool above tendr.

## Decisions

Choices that are hard to reverse and came from a real choice between
alternatives. Where an entry names a file as its source, the full text is in
Git at commit `95dcf1d`, the last commit before the old plans and specs were
removed.

### Tendr supervises processes; it is not an agent framework

*2026-04-03, amended 2026-10-31.* This is the "process sitter" decision.
Tendr starts, watches, chains and stops processes. It does not learn LLM
protocols, count tokens, retry API calls or choose models. An agent runs under
tendr as its own command-line program, for example
`tendr start task -- claude -p "…"`. Adding LLM knowledge would turn tendr into
an AI framework, with a different scope, users and maintenance cost. Source:
`docs/plans/specs/decision-process-sitter-not-framework.md`.

The amendment corrects a premise. The 2026-04 decision assumed agent programs
run once and exit. Many now stay up: Claude Code's interactive-only features
and background mode, and omp's RPC mode. Tendr supervises them like any other
long-running process, and the rejection of LLM knowledge stands. It also sets
one more boundary: tendr does not decide when an agent's turn is done. An agent
can report its turn as events (see the next entry); what those events mean is
the caller's policy (rule 1). A review on 2026-10-08 (two models,
independently) rejected a turn contract in core because "done", "blocked" and
"settled" are vendor terms that change between releases. Source: PR #123.

### Processes may publish events; tendr does not interpret them

*2026-05-23, refined 2026-07-06.* A supervised process can add structured
events to its session's event log with `tendr emit` or `tendr wrap`. Tendr
stores and orders these events. It does not decide what they mean (rule 1):
"this hook event means the agent's turn ended" is the consumer's policy. Any
tool with lifecycle hooks can report its state without tendr carrying
tool-specific code. Source: `docs/plans/specs/tender-as-block-runtime.md`.

### No daemon: one sidecar per run, and files are the truth

*2026-03, confirmed for the event log 2026-07-06.* Each run has its own
sidecar process. It is born with the run and exits after it. All state is in
files, and consumers read them directly (rule 4). A per-host daemon with an
in-memory index was designed and rejected: in a comparison of three designs,
plain append-only files met the event log's needs without a resident process.
Source: `docs/plans/specs/persistence-architecture.md` (the rejected design)
and `docs/plans/specs/event-protocol.md` §0.

### No platform supervisor

*2026-03.* Tendr does not use systemd, launchd, Windows services or Task
Scheduler. The sidecar is the supervisor on every platform, so commands behave
the same on Linux, macOS and Windows, inside containers and in CI. Each
platform supervisor exists on one platform only and would need a second code
path. Source: `docs/plans/specs/tender-agent-process-sitter.md`, "Platform
Supervisor Landscape".

### Commands print JSON, and exit codes are a contract

*2026-03.* Commands print JSON by default because their main readers are
programs and agents. Exit codes have fixed meanings (see the exit code table in
`tendr guide`), so a caller branches on an integer, not on message text.
Changing an exit code's meaning is a breaking change.

### `start` is idempotent on the whole launch spec

*2026-03.* Starting a running session with the same launch spec returns the
existing run. The launch spec is the command, its arguments and the start
options: working directory, environment, timeout, dependencies and exit hooks.
A different spec under the same name is a session conflict error, and
replacing a run needs `--replace`. Retrying a command is therefore always safe,
and a conflicting request is never silently merged.

### `--after` waits for a run, not a name

*2026-03.* Each run has a `run_id` (a UUIDv7). `--after job1` records the
`run_id` of `job1` when the dependency is bound. If `job1` is replaced while
the dependent waits, the dependent run ends as dependency-failed instead of
following a different run. The generation number is a counter for people and
is not used for this check.

### `output.log` is a line log; the PTY recording is the exact record

*2026-03, recording added 2026-09.* `output.log` stores lines with a timestamp
and a stream tag. It is for reading and searching, and it does not reproduce
bytes exactly. A PTY session also writes an exact recording of its output and
window sizes under `recording/<run_id>/`. A caller that needs exact bytes from
a pipe session redirects the child's output to its own file.

### Windows uses Job Objects and a hidden console

*2026-03 to 2026-04.* Windows has no SIGTERM. The sidecar runs the child in a
Job Object so that stopping it stops all its descendants. A stop first sends
CTRL_BREAK, which is best-effort, then ends the Job Object after the grace
period. For CTRL_BREAK to reach the child, the sidecar allocates a console and
hides its window. Source: `src/platform/windows.rs`.

### Remote work runs the same tendr over SSH

*2026-04-01.* `tendr --host <host> <command>` runs the remote host's own
`tendr` over SSH. There is no broker and no second lifecycle model: a remote
session is a local session on that host. Upgrade `tendr` on both ends
together, because a release can change what the two ends send each other.

### No automatic restart

*2026-03.* Tendr never restarts a child that exits (rules 1 and 7). Restarting
an operating system process is riskier than restarting an in-memory actor: the
working directory, environment, pipes and any half-done external work may all
be in an unknown state. Deciding when a restart is safe is the caller's
policy. A restart policy (never, on-failure, always, with bounded retries) was
designed and not built. Source: `docs/plans/specs/tender-agent-process-sitter.md`,
"Restart Policy".

### Analytics is `tendr query`, run by the `duckdb` program

*2026-07-09.* `tendr query` runs SQL over the event logs (rule 4). It is a
separate command, not an option of `tendr events`: `events` replays and
follows the log with a fixed output format, and SQL analysis is a different
job. It runs the external `duckdb` program found on `PATH`, the same program
the `duckdb` exec target drives, so tendr has no DuckDB library dependency.
`query` therefore needs `duckdb` installed. Source:
`docs/plans/completed/2026-07-09-event-log-analytics-v1.md`.

### No terminal emulator in core

*2026-07-09, revised 2026-09-14.* Tendr owns the PTY, input ownership and the
exact recording. It does not parse or render the screen. Screen reads come
from outside tendr: from Boo today, or from an optional `tendr-screen`
extension that tendr would call through a versioned command-line contract.
That extension is planned and not built. Core does not link a terminal
emulator such as libghostty, so it gains no Zig or Ghostty build dependency.
Source: [cloud PTY plan](plans/active/00_cloud-pty-control.md) and
`docs/plans/specs/ecosystem-landscape.md`.

### CI stays on native GitHub Actions; Dagger was not adopted

*2026-09-25.* A pilot ran the Linux checks under Dagger beside `ci.yml`.
Coverage and failure reporting were equal. But Dagger added 38–70 s of setup to
every cold hosted job, against a limit of 60 s. The msrv job stayed within that
limit in only 3 of 6 attempts. With the engine state cached through
`actions/cache`, a one-line edit cost 47–95 s more per job, against a limit of
15 s. Hosted runners keep no engine between jobs, and the safe ways to keep one
need a self-hosted runner or a paid engine. Source:
`docs/plans/completed/2026-09-25-dagger-ci.md` and its data folder; closed
PR #78.
