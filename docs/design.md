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
Git at commit `95dcf1d`, unless the entry names another commit. `95dcf1d` is
the last commit before the old plans and specs were removed.

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

Two consequences stand until a named user needs otherwise. Storage is per
session: large event payloads go to the session's own `events/blobs/`, and
deleting a session deletes them; there is no global blob store, reference
count or cache. Control between the CLI and the sidecar uses files, a FIFO
and the attach socket; there is no request/response protocol. A design for
each was written and set aside; both are in Git at `83f83ab`
(`docs/plans/backlog/content-addressable-storage.md` and
`sidecar-control-protocol.md`).

### Event logs rotate but keep every segment; `prune` owns retention

*2026-11-04.* An event segment is full at 64 MiB and the next append starts a
new one ([event protocol §3.3](architecture/07-event-protocol.md#33-segments)).
Segments are never renamed, because cursors name them, and they are kept until
the session is pruned: dropping old segments is the only operation that
would invalidate a live cursor or lose history the log is the authority for
(rule 4). So rotation bounds the size of each file, not of the session; a
session that must stay small is pruned or replaced. The spec's option to keep
only the newest N segments was not built. Source:
`docs/plans/specs/event-protocol.md` §3.3 and §3.7.

Any writer rotates, not only the sidecar as the spec proposed: a session's
sidecar writes only at lifecycle transitions, while hooks and `emit` may
append for weeks, so a sidecar-only rule would leave such a session in one
growing segment. 64 MiB matches the PTY recording's segments. No long-running
session data existed to measure. A synthetic one (`wrap --event hook.*` with
a 1.8 KB hook payload writes about 4 KB per event) puts an agent session with
pre- and post-tool hooks at about 29 MB per 10-hour day at one tool call
every 10 s, and 230 MB per day at one every 3 s around the clock: a segment
lasts from about 7 hours to a few days.

`prune --older-than` sweeps `~/.tendr/lost+found` by each event's own `ts`,
rewriting the one file under the lock its appenders take, rather than
rotating it into dated files: the path stays the documented one, removal
follows event time rather than file time, and nothing has to guess when a
dated file is no longer being written.

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

### Viewers are cursors into one output ring

*2026-11-02.* A PTY run keeps its latest 32 MiB of output in one ring, from
its first viewer on, and a viewer is a cursor into it, instead of a queue of
copies per viewer. One shared ring is what lets a viewer resume from an
offset later: a reconnecting viewer gets a cursor at the offset it reached,
so the bytes it missed and the live output join by construction, with no gap
and no duplicate. It also keeps capture from ever waiting on a viewer, and
bounds memory to one ring per run however many viewers it gets. The ring
holds bytes, not capture's chunks, so a cursor can stand at any offset and
tiny chunks cost no per-chunk overhead. Source: step 1 of the remote frame
transport plan (`docs/plans/active/01_remote-frame-transport.md`) at `d282062`.

### Viewers count output in bytes, and agree the version in the request

*2026-11-02.* A version 2 viewer's position is a byte offset of the run's PTY
output, not a recording sequence number. The recorder stops numbering when it
stops (at its size limit, after a storage failure or a full backlog), numbers
resizes as well as output, and may not run at all, so a sequence cannot name
every byte a viewer was sent. A byte offset can: it is assigned under the
same lock as the ring, the only place output is counted. A disk-backed resume
can still map offsets onto the recording by summing output payloads, up to
where recording stopped.

`attach --host` agrees the version in the remote frame's request, not in
the hello it sends over ssh, because both 0.5.0 ends, the remote tendr and
the sidecar, take only the exact version 1 hello, while the 0.5.0 remote
ignores an unknown request field. So a new client keeps working against an
old remote, and a new remote against a sidecar started by an old one; the
steps are in [the attach bridge](architecture/06-transport-boundaries.md#the-attach-bridge).
Source: step 1 of the remote frame transport plan
(`docs/plans/active/01_remote-frame-transport.md`) at `d282062`.

### A resume decides control and output on one connection

*2026-11-02.* A viewer that lost its connection resumes with one request that
the sidecar answers once: first whether it regains control, then whether the
output from its offset is still held. Splitting the two (ask for control,
then fetch bytes, or the other way round) would leave a window in which a
takeover, a push or eviction from the ring lands between them, and the
client would act on an answer that is no longer true: typing into a
terminal it no longer controls, or showing output with a hole in it. Under
the input writer and the sink lock, both answers hold for the same instant,
and the ring cursor installed with the answer joins the missed bytes to the
live ones.

The resume is granted a new epoch, never the one the client held, because
every ownership change advances the epoch by exactly one, and a resume is
one: in the common half-open case the old connection still holds that
epoch, and only a new one tells the two apart. The old connection's late
disconnect then releases nothing, and its queued input cannot be written.
The change is recorded with its own trigger, `resume`, so a client checking
the event log for a takeover of its epoch never mistakes a resume for one,
while a takeover of the resumed client still reads as a takeover. Source:
step 1 of the remote frame transport plan
(`docs/plans/active/01_remote-frame-transport.md`) at `d282062`.

### A dropped `attach --host` reconnects by itself, judged by ssh's exit

*2026-11-02.* When an `attach --host` connection ends from the far side,
ssh's own end says what happened, and nothing else is asked. The remote's
exit code (from its trailer) is its verdict, and the attach ends with it;
ssh's 255 without a trailer, ssh killed, or ssh still running 5 s after its
output ended is a lost link, and the client reconnects. A separate status
call before reconnecting would need the same link that just failed, and
could be stale by the time the resume lands. The resume itself is the end
signal: its refusal (the control rule, a run that has ended, a remote that
cannot resume) carries the code to exit with.

*2026-11-03.* That end counts only once the client has written everything
the old connection delivered to the terminal. A failed write to the session
(ssh's stdin already closed) is the same end arriving first, not the end
itself: acting on it dropped a controller's last output when its remote
answered and exited at once. The wait keeps the keyboard, so the escape and
the signals still leave at once. A reconnect's dial is judged as the end of a
connection is: on Windows, ssh.exe's 1 without a trailer is ssh.exe killed,
and the attempt is tried again; on Unix it is the remote's code, and the
attach ends with it.

It retries without a limit on the number of attempts (Rick's decision),
with pauses doubling from 1 s to at most 30 s, until it resumes, the remote
refuses, or the user leaves with the escape or a signal. A laptop that
sleeps or changes networks should come back to its session, and a person
who wants out has the escape.

A reconnect runs ssh with `BatchMode=yes`, so that call has a constant argv
of its own. The first connection keeps prompts: it runs before raw mode,
so a passphrase or host-key prompt reads a normal terminal. A reconnect
happens in raw mode, while the keyboard belongs to the attach; a prompt
would eat keystrokes, or wait unseen. Under `BatchMode` a login that needs a
prompt fails at once, and its stderr goes into the status line instead. A
login that stalls without a prompt (a lapsed Tailscale SSH check-mode
approval) is bounded by the connect deadline instead. Every framed call has
`ServerAliveInterval=5` and `ServerAliveCountMax=3`, so ssh notices a dead
link within about 15 to 20 s instead of waiting on TCP.

An attach always makes its own ssh connection (`-o ControlMaster=no -o
ControlPath=none`). A mux client sends no keepalives of its own: the
master's settings decide when a dead link is noticed. With Rick's config
(`ServerAliveInterval 60`, `ServerAliveCountMax 3`, `ControlPersist 600`),
an attach over an open master noticed an iptables drop on the VM after
240 s; on its own connection, after 20 s. The extra handshake (about 0.4 s
to the VM) is paid once per connection. Short framed calls keep sharing:
the master saves them about 0.4 s each, and over a dead master they still
end, at the 30 s connect deadline.

Input typed while reconnecting is discarded with one notice, never queued:
keystrokes sent seconds late to a program that has moved on can do harm, and
the user cannot see what they would land on until the screen is back.
Source: step 1 of the remote frame transport plan
(`docs/plans/active/01_remote-frame-transport.md`) at `d282062`.

### Watchers never touch control

*2026-11-03.* A read-only viewer (a watcher) never reaches the input writer:
it is not granted an epoch, records no `pty.control_changed`, is kept in
its own set that a takeover cannot reach, and its keystrokes, resizes and
repaints are dropped by the sidecar. So nothing a watcher does or loses can
move control: not its attach, its link dropping, its resume, nor its
detach. A human holding the session therefore does not keep a watcher out,
locally or over `--host`, and a takeover never retires one. Taking control
means attaching again with `--takeover` (Rick's decision), never upgrading
a watcher in place, which would put a second path into the control rule.
Source: step 1 of the remote frame transport plan
(`docs/plans/active/01_remote-frame-transport.md`) at `6612733`.

### A watch resume carries no epoch

*2026-11-03.* A watcher that lost its link resumes with its own frame,
`MSG_WATCH_RESUME` (run id and offset), not with `MSG_RESUME` and a dummy
epoch. With no epoch in the frame, no path can feed a watcher's point into
`InputArbiter::resume`, and a sidecar or remote tendr from before it refuses
it as an unknown frame instead of reading it as a controller's resume, so a
skewed peer can never grant control to a watcher. The bridge requires
exactly this frame after a read-only request and refuses a plain resume
there. The answers are the controller's `MSG_RESUMED` and `MSG_GONE` at
epoch 0, which names no control, since the watcher reads only their run and
offsets; the remote request still carries an `epoch` field, set to 0, only
because a remote from before watch resumes requires the field to refuse
the frame with 80 rather than reject the request with 2. Source: step 1 of
the remote frame transport plan
(`docs/plans/active/01_remote-frame-transport.md`) at `6612733`.

### The end of a viewer's output is a frame, in a new version

*2026-11-03.* A viewer cut off when the run ends (still behind when the
sidecar stops waiting for it) and one that had everything both see the
session close the connection after the run ended, so neither `meta.json`
nor the remote's reading of it can tell them apart: a cut-off watcher
exited 0, short. The sidecar now sends `MSG_OUTPUT_END`, with the final
offset, once a viewer's sender has sent the run's last byte, and a watcher
exits 0 only after it. The frame is attach protocol version 3, asked for in
the hello or the resume, rather than sent to every watcher, so no client
receives a type it never asked for. The cost is the step-down: a sidecar
from before version 3 refuses the hello, and the client says it again in
version 2, then goes by `meta.json` as before. Over `--host` the client
cannot learn the version the bridge spoke to the sidecar (version 3's
answers are version 2's), so the bridge, which can, judges: 84 without the
end in version 3, `meta.json` before it. Source:
`src/commands/attach/bridge.rs`, `src/sidecar.rs` (`run_viewer_sender`).

### Windows uses Job Objects and a hidden console

*2026-03 to 2026-04.* Windows has no SIGTERM. The sidecar runs the child in a
Job Object so that stopping it stops all its descendants. A stop first sends
CTRL_BREAK, which is best-effort, then ends the Job Object after the grace
period. For CTRL_BREAK to reach the child, the sidecar allocates a console and
hides its window. Source: `src/platform/windows.rs`.

### Windows clients attach over the bridge, through a console layer

*2026-11-03.* The attach client is one platform-neutral core (frames, the
control-first outbox, the escape, mode tracking, offsets, the status line,
drop classification and reconnect) over a small terminal layer (raw mode,
input, size, output, leave). Windows got its own layer instead of keeping
`ssh -t`, so a Windows client has the same contract as a Unix one: names
travel as data, the escape and terminal restore are local, and a dropped
link resumes. The `ssh -t` path and its plain-name rule were deleted with
the cutover rather than kept as a fallback. A console without
virtual-terminal support (before Windows 10 1809) is refused, because a
fallback would keep a second attach path alive for almost no users. Output
is written with `WriteFile` under the UTF-8 code page, since Rust's console
`Stdout` fails on bytes that are not valid UTF-8 and a PTY child can send
any. Source: `src/commands/attach/terminal/windows.rs`, PRs #177, #179, #181.

### Remote work runs the same tendr over SSH

*2026-04-01.* `tendr --host <host> <command>` runs the remote host's own
`tendr` over SSH. There is no broker and no second lifecycle model: a remote
session is a local session on that host. Upgrade `tendr` on both ends
together, because a release can change what the two ends send each other.

The request travels as typed data, never as a command line
(*2026-11-01*). Every `--host` call but `attach` runs one constant remote
command and sends the request as JSON on ssh stdin
([remote frame](architecture/06-transport-boundaries.md#the-remote-frame)).
Quoting arguments for the remote login shell cannot be made safe: POSIX
quoting means nothing to `cmd.exe`, so on a Windows host a session name like
`x&calc` ran `calc`, and no one quoting works in `cmd.exe`, PowerShell and
bash alike. The requests are their own types, not the clap command enum, so a
CLI change is not a wire change; the version sits on the outer frame. The
frame ends at the remote tendr CLI, which runs the command as if typed there:
no network listener, no RPC layer. Values cross verbatim, and tendr does not
translate workload syntax between shells.

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

*2026-07-09, revised 2026-10-31.* Tendr owns the PTY, input ownership and the
exact recording. It does not parse or render the screen, and it offers no
screen reads. Core does not link a terminal emulator such as libghostty, so it
gains no Zig or Ghostty build dependency. A caller that needs the rendered
screen replays the recording through a terminal of its own. Source:
`docs/plans/specs/ecosystem-landscape.md`, and the cloud PTY plan at
`95dcf1d`, which designed an optional `tendr-screen` extension that was
dropped on 2026-10-31.

### Detach is a fixed two-key escape

*2026-09-26.* `attach` detaches on `Ctrl-\ d`. It does not need Enter, which
could submit an unfinished prompt. It is not newline `~.`, because an outer
`ssh -t` takes that as its own disconnect escape. The prefix is fixed, with
only an on/off switch; a configurable prefix waits for a measured need.
`--escape` already takes a value (`ctrl-backslash` or `none`), so one can be
added later without changing the flag. Source: the cloud PTY plan at
`83f83ab`.

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

### The wire protocols are designed and tested here

*2026-11-01.* The formats tendr puts on the wire — the length-prefixed
request envelope on ssh stdin, the versioned request, the exit trailer, the
event protocol — are tendr's own designs. They are defined in this
repository, named in
[06-transport-boundaries](architecture/06-transport-boundaries.md), and
changed like any other code: review, tests, and a recorded decision. The
work is designing and validating this framing against controlled endpoints:
a fake ssh in unit tests, real OpenSSH on CI runners driven as a test
endpoint, and local sshd lanes. The injection-style strings in the remote
tests (`tests/cli_remote*.rs`) are adversarial data, not attack tooling:
they are sent as framed data to those endpoints, and marker-file regression
tests assert that a hostile session name creates nothing and runs nothing
remotely. The `ndjson` exec target extends the same framing with
request-id matched replies.
