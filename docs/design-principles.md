# Tendr Design Principles

These are the rules a reviewer applies to a new feature, review or refactor.
The [architecture docs](architecture/README.md) describe the system as it
is. The reasons for past choices are in [decisions.md](decisions.md).

Tendr is a runtime for supervised sessions. It is not a workflow engine or a
domain-specific scheduler.

## The 5-Layer Stack

1. **Runtime**: sidecar, child process, PTY and pipe transport, logs,
   process identity, kill and wait.
2. **Session control**: `start`, `exec`, `push`, `attach`, `status`,
   `wait`, `watch`, and the typed state machines behind them.
3. **Composition primitives**: `--after`, `--on-exit`, namespaces, and
   read-only projections over session data.
4. **Workflow policy**: scheduling, retries, health rules, orchestration,
   "what should be running".
5. **Domain tools**: compilers, extractors, CI coordinators, deployment
   tools, application control loops.

Tendr owns layers 1 to 3. Add layer-4 behavior only with care. Never absorb
layer 5.

## The 7 Themes

### 1. Mechanism Over Policy

Tendr gives callers runtime guarantees and small primitives. It does not
make workflow judgments for them.

Example: `wait`, `watch`, `--after` and `--on-exit` are mechanisms. None of
them decides what "healthy" means, when to retry, or how to deploy.

In review:

- Ask: "Does this give the caller a primitive, or does Tendr decide policy
  for the caller?"
- A feature that defines schedules, retry rules or health rules probably
  belongs above Tendr.

### 2. One Authority Per Fact

Every durable fact has one owner and one write path.

Examples:

- The sidecar is the normal writer of lifecycle state
  ([03-run-lifecycle.md](architecture/03-run-lifecycle.md)).
- CLI reconciliation is the only other lifecycle writer. It either heals
  meta from the sidecar's terminal event or infers `SidecarLost`, and it
  records provenance for both.
- PTY control changes go through one typed path in the sidecar, not through
  `meta.json` edits from several places.

In review:

- Ask: "Does this add a second writer for an existing fact?"
- If yes, redesign until there is one explicit owner.

### 3. State Machine First, Protocol Second

For a control feature, define the typed states and legal transitions first.
Then define CLI flags, files, sockets and JSON payloads.

Examples:

- The run lifecycle is a state machine
  ([03-run-lifecycle.md](architecture/03-run-lifecycle.md)).
- PTY control is a typed `InputArbiter` with controller epochs
  (`src/model/pty_control.rs`). The attach protocol carries its decisions.

In review:

- Ask: "If the transport disappeared, would the state model still be clear
  and testable?"
- If not, the protocol is standing in for the specification.

### 4. Durable Truth, Derived Views

Tendr stores authoritative facts and derives views from them. It does not
create new stored state just to support a view.

Examples:

- `meta.json` is the current run snapshot, `output.log` is the child-output
  record, and the event log is lifecycle and provenance history.
- `status`, `list`, `log`, `watch`, `events` and `query` read those files.
  They store nothing of their own.

In review:

- Ask: "Is this a view over existing facts, or a new store that copies
  them?"
- If a view needs its own on-disk store, it may not belong in Tendr.

### 5. Separate Control Plane From Work Plane

Control operations and payload bytes stay on separate paths.

- Control plane: `start`, `status`, `wait`, `watch`, `kill`, `attach`.
- Work plane: child stdin, stdout and stderr; PTY bytes; framed `exec`
  payloads.
- `watch` reports lifecycle and log events. It never writes to child stdin.

In review:

- Ask: "Does this feature carry control meaning through the child's payload
  channel?"
- Do not pass workflow state through stdin, transcript parsing or shell
  conventions when a control field or state transition is the real model.

### 6. Inference Must Be Labeled

Tendr knows some facts directly and infers others. The two must look
different.

- Direct: the sidecar writes a terminal state after it observes the child
  exit, or `SidecarFailed` after it stopped the child because its own
  supervision failed.
- Inferred: reconciliation concludes `SidecarLost` from a released lock and
  a missing terminal state.
- `transition_provenance` in `meta.json`, and `data.provenance` in lifecycle
  events, record which kind each transition is and the evidence for it.

In review:

- Ask: "Is this an observation or an inference? Can downstream tools tell
  which?"
- Prefer an added provenance field over an overloaded status name.

### 7. Composition Should Stay Shallow

Tendr supports shallow composition of supervised runs. It is not an
orchestration engine.

Example: `--after`, `--on-exit`, `wait`, `watch` and namespaces compose
runs without a retry engine, a scheduler or a planner.

In review:

- Ask: "Is this still structural composition, or a scheduler?"
- A feature that needs DAG semantics, health rules, retries or planning
  probably belongs in a tool above Tendr.

## Litmus Questions For New Features

- Does this add a new authority for an existing fact?
- Does the protocol come before the state machine?
- Can a reader tell inferences from observations?
- Is this a view over stored facts, or a new store?
- Does it cross from the control plane into the work plane?
- Does Tendr provide a primitive, or decide policy?
- Is the composition still shallow?

A feature that fails more than one of these checks probably belongs
elsewhere, or needs to be simpler.

## Rejected Anti-Patterns

### Ownership recorded but not enforced

Rejected: PTY control state that says an agent has exclusive input, while
`push` still accepts anonymous writes.

Why: the label and the behavior disagree. The transport must enforce
authority, not only record it in metadata.

### `meta.json` edits from many call sites

Rejected: patching `meta.json` as untyped JSON from unrelated code paths.

Why: it creates several writers for one fact. It bypasses typed invariants
and adds races.

### Protocol-first control features

Rejected: starting a control design with request files and CLI flags before
the state machine is defined.

Why: invalid states stay hidden until review, and the transport ends up as
the specification.

### Policy flags before checking the mechanism

Rejected: a new `exec` strictness flag, proposed before checking whether
`tendr exec` already passes on the inner exit code. It does.

Why: it adds user-facing policy to solve a problem the mechanism may
already solve. Check the current behavior before adding features.

## What a Good Feature Looks Like

- It keeps one authority per fact.
- It starts from a typed state model.
- It writes each fact once.
- It offers views without a second store.
- It keeps control out of the work channel.
- It gives callers primitives, not policy.

A proposal that needs more than this usually belongs above Tendr.
