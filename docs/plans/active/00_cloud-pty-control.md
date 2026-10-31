---
id: cloud-pty-control
depends_on: []
---

# Cloud PTY control and replay

**Status:** slice 1 is partly shipped (v0.4.0). Steps 5, 6, 8 and 9 of slice 1
remain. Slices 2–4 have not started.

## Goal

The target workflow is Ghostty on the laptop → SSH → Tendr on an exe.dev Linux
VM → Claude Code in a Tendr-owned PTY. A person can reconnect after losing the
network, take input ownership at once, resize and detach. An agent can drive the
same PTY. Output is recorded exactly and can be played back through Ghostty
without a screen extension.

Claude must be launched through Tendr. Adopting an unrelated existing terminal
is out of scope. A lost SSH connection does not end the run. A dead sidecar or
VM reboot does: recovering a recording does not resume a process or a
conversation.

This plan owns the early Unix PTY bridge, taken from
[remote frame transport](01_remote-frame-transport.md) step 5. It needs no
global daemon, second PTY owner, generic RPC framework or async runtime.

## Decisions

| Concern | Decision |
|---------|----------|
| Core owner | Existing Tendr sidecar owns child lifecycle, PTY, input arbitration, output sequencing, recording, and viewer transport. |
| First transport | Existing `ssh -t` remote attach, hardened for explicit takeover. Typed `ssh -T` follows in slice 3, before the general remote-frame migration. |
| Recording | Exact output and geometry are recorded for the new PTY workflow by default. Input payload recording is opt-in per run. |
| Playback | Core `tendr replay` writes recorded output locally. It neither requires a VT engine nor sends historical input to a running child. |
| Screen authority | Optional, separately packaged Rust `tendr-screen` executable on PATH; no Ghostty/Zig linkage in the core package. |
| Public surface | Core implements byte/control operations; explicitly recognized screen commands use a versioned external extension contract. Unknown commands are errors. |
| First platforms | macOS client and Linux target, including exe.dev. Test local Unix behavior too. ConPTY and native Windows attach remain separate work. |
| Session identity | Reuse Tendr namespace/session/run identity. Every stateful handle also carries its run ID; replacement invalidates old handles. |

The external screen dispatch revises the earlier Boo-only screen decision. See
[design](../../design.md#no-terminal-emulator-in-core) and [boo-integration](../backlog/boo-integration.md).

## Rust implementation boundaries

Start with modules in the existing crate, extracting only the external screen
package when slice 4 needs it. Suggested responsibilities: controller state,
bounded PTY messages, recording codec/store, viewer delivery, and replay client.
Keep command handlers thin; do not serialize Clap enums as wire DTOs.

- Use validated `RunId`, `ControllerEpoch`, `Sequence`, and nonzero terminal-size
  types. Use `TryFrom` for frame lengths/dimensions; reject overflow and zero sizes.
- Use data-carrying enums for controller and recording state; runtime transitions
  fit persisted/remote state better than a large typestate hierarchy.
- Return typed domain errors (`thiserror`), adding context at the CLI boundary.
  Decode/version-check/validate before side effects. Keep PTY payloads as bytes.
- Retain the sync-threaded architecture. Use bounded queues and cancellable,
  nonblocking I/O with deadlines; never hold a shared lock across a viewer write.
  Avoid `Arc<Mutex<...>>` as a substitute for defining one state owner.
- PTY master clones share the open file description and `O_NONBLOCK`. Set its
  mode deliberately at setup and migrate **both** read and write loops together.
  Use nonblocking reads/writes with readiness waits and a cancellation wakeup;
  `WouldBlock` waits/retries, `Interrupted` retries after cancellation checks,
  and neither is EOF. Preserve/drain output on hangup and classify platform PTY
  close errors explicitly. Do not toggle `F_SETFL` independently in one half.
  Polling `POLLOUT` before a blocking `write_all` is not a deadline guarantee.
- Prefer `OwnedFd` and existing `rustix` facilities. Localize necessary platform
  `unsafe` with safety comments and explicit descriptor lifetimes/CLOEXEC rules.
- Terminal restoration uses an RAII guard plus cancellation/signal handling.
  EOF from either relay direction must wake the other direction and permit exit.

## Controller state and the single input writer

Model `Unowned { epoch }` and `Owned { epoch, holder, kind }`, where kind is
human or agent and holder identifies a connection or explicit claim. The sidecar
alone transitions state. Epoch increments are checked; exhaustion is an error.

| Action | Transition and result |
|--------|-----------------------|
| Claim an unowned PTY | Advance epoch, bind holder, return a run-bound controller handle. |
| Claim an owned PTY | Deny unless this is an explicit human takeover. |
| Human `attach --takeover` | Advance epoch and replace holder immediately, even if the previous socket still looks connected. Revoke queued old input. |
| Send / resize | Validate run, holder, epoch, and payload, then enqueue a bounded request. Revalidate at each nonblocking write/resize boundary. |
| Detach / detected EOF | Release only if the disconnecting handle is still current. An old connection closing cannot release its successor. |
| Agent resumes after human detach | Explicit fresh claim. Never silently restore the preempted handle or its queued prompt. |
| Run ends / is replaced | Invalidate all controller and viewer handles for that run. |

One writer owns PTY writes and resize application. Both legacy PTY `push` and
attach use it; pipe-mode `push` retains its existing behavior. A legacy PTY push
can obtain a temporary claim while unowned, but must not bypass arbitration.
Concurrent claims receive a typed busy result. Do not port the historical
one-header-then-unchecked-stream lease design.

Takeover is serialized with writer progress: after the takeover acknowledgement,
no further write from an old epoch may enter the kernel. Already-written bytes
cannot be recalled. A partially written request reports the accepted byte count;
only its sidecar-queued remainder can be cancelled. Writes have deadlines and do
not block ownership changes while the child is not reading input.

An input acknowledgement means **bytes were written to the PTY master**. It does
not mean Claude consumed them or completed a turn. A lost acknowledgement is an
unknown outcome; clients do not automatically resend. Request IDs correlate
outcomes; they do not imply durable exactly-once execution.

The first slice depends on explicit takeover, not timely TCP failure detection.
A remote attach process remaining alive across a network partition does not
prove that Ghostty is reachable. End-to-end heartbeat/expiry can be added to the
typed bridge if needed; a heartbeat from that remote process alone is insufficient.

Takeover also retires the previous controller's connection and output subscription:
shut down both socket directions, wake blocked tasks, drop queued output/input,
and release its client slot. Do not merely demote it into a passive viewer. The
old attach process must cancel its stdout relay even when the SSH peer is stalled.
Cleanup has a one-second deadline and cannot delay acknowledging the new epoch;
epoch validation protects the writer while old tasks exit.

## Local socket identity and readiness — slice 1

The local terminal endpoint must stay within the run owner's account. Replace
the predictable socket directly under shared `/tmp` with a short socket path in
an owner-only directory (`0700`). Default to `sockets/` under Tendr's persistent
state root, with a short name derived from run identity and collision checking.
The state root must remain mounted/available after logout. Verify ownership/mode
and reject symlink substitutions; never adopt or remove a foreign pre-existing
directory/socket. Keep the complete path within the platform limit: hashing a
basename cannot shorten an overlong parent directory.

Socket placement must preserve the run's lifetime, not just its permissions:

- Do not prefer `XDG_RUNTIME_DIR` merely because it exists. A runtime-directory
  override is eligible only with a verified lifetime guarantee, such as confirmed
  lingering for the owning UID on the deployed logind host. Unknown/failed checks
  select persistent storage. Do not silently enable lingering or change login policy.
- If the persistent path is too long, permit an explicitly configured short,
  owner-only persistent socket root. An atomically created private temporary
  directory is the last fallback, only with verified protection from the host's
  age-based cleanup for the lifetime of the run. For systemd-tmpfiles, acquire and
  retain a BSD lock on the directory FD before binding/publishing the socket, and
  test that the deployed cleanup implementation honors it. A lock file inside the
  directory is not equivalent. Other cleanup systems need their own verified
  exclusion; do not assume that a socket being in use prevents deletion.
- If neither a short persistent path nor a protected temporary path is available,
  fail before PTY readiness with an actionable socket-root error. Report the chosen
  root/lifetime policy in diagnostics. Cleanup follows run termination, not detach.

Slice-1 acceptance includes an actual last-session logout on a disposable
logind-managed Linux host with lingering disabled, then a fresh login and attach
to the same run/PID through the persistent socket. Ensure no hidden SSH session
keeps the login count nonzero. Test a permitted runtime override with lingering
enabled separately. Record process/cgroup survival policy: a persistent socket
cannot prevent `KillUserProcesses` from terminating a login-scoped sidecar.
Validate age-based cleanup against an aged, idle temporary directory in an isolated
test root while its directory lock is held and again after release. Do not run
destructive global cleanup on the user's machine or equate `rm` with aging policy.

Verify peer UID on **both** sides before any terminal bytes are sent: the sidecar
checks connecting clients and the attach client checks the server against the
run owner, using Linux `SO_PEERCRED` or macOS `getpeereid` behind the platform
boundary. Checking only the client UID at the server does not stop a spoofed
server from receiving the user's keystrokes. Apply this to legacy paths too;
there is no unverified shared-temp fallback.

Bind/listen and establish permissions before atomically publishing the breadcrumb
and advertising PTY readiness. Bind, credential, or publication failure is a loud
startup/attach error with cleanup of resources owned by this run; it must not
return a usable Running PTY with an absent listener. Confirm run identity in the
handshake. Missing capabilities fail closed before raw mode or input forwarding.
Normal shutdown removes only this run's breadcrumb/socket/private directory;
stale cleanup validates ownership and active-run identity before removal.

## Output, sequence, and viewer contract

The output reader never writes to a viewer. The per-run output coordinator
reserves the next sequence, appends the complete record containing that sequence,
then publishes it to bounded per-viewer queues. This is the precise meaning of
**record → publish sequence → fan out**: sequence assignment cannot literally
happen after persisting a record that must contain it.

The recording orders observed output, initial geometry, and applied resizes.
It describes what the PTY owner observed/applied; it does not infer the child's
internal timing of output around a resize. Input payloads, when enabled, are
separate typed records, never terminal-output bytes.

Initial limits: 64 KiB data frames and 8 MiB queued bytes per viewer, configurable
within validated bounds. Control messages have a smaller separately bounded
budget and cannot sit behind output backlog. Allow one controller per run; in
slice 1 its own connection is the only viewer, and one pool of eight
connections, hellos included, admits it, each hello bounded by a five-second
deadline. Read-only viewers (at most eight) and a separate bounded admission
path for up to four pending handshakes arrive with the typed bridge in slice 3,
so a full viewer set never rejects a replacement controller. Authenticate
before admission; retire the old controller rather than count it as another
viewer on takeover.
These are starting operational defaults to validate, not performance claims.

A slow viewer is disconnected with its last acknowledged/rendered cursor where
available. A typed viewer reconnects with `(run_id, after_sequence)` and receives
the retained contiguous suffix. Missing history returns `CursorGone` with the
available range; it must never silently skip forward. Before the typed bridge,
the recovery is explicit takeover plus redraw/repaint, without a byte-resume claim.

## Recording format, failure, and retention

Use a versioned Tendr binary recording, separate from structured lifecycle
events and `output.log`. A segment header records format version, run identity,
initial dimensions/terminal profile, and wall-clock origin. Length-delimited
records contain sequence, monotonic elapsed time, kind, byte length, and exact
payload. A bounded decoder rejects unknown versions, oversized frames, and
interior corruption; an incomplete final record after a crash is reported and
excluded. Define the concrete byte layout and golden fixtures in slice 1.

Within a run, sequences start at one and are contiguous across segment boundaries;
reject duplicates, regressions, missing interior sequences, mismatched run IDs,
and decreasing elapsed timestamps (equal timestamps are allowed). A retained
suffix declares its first available sequence and checkpoint, when present; it
does not authorize a gap inside that suffix. Validate indexes against the records,
including segment transitions and the last complete-record boundary. Cursor-based
observation counts every record kind; filtering output does not reset numbering.

Segment indexes are derived and rebuildable from records. Publish segment files
and manifests atomically. Keep one recording writer. Record references and state
changes can appear in Tendr's existing event schema without embedding every
PTY chunk into that lifecycle stream or inventing another run model.

Defaults for this workflow:

- Output/geometry recording on. An explicit recording-off mode and `--record-input`
  are slice-2 work alongside retention; until then input bytes are never recorded.
  Store the policy in run metadata. Do not store text/paste payloads in diagnostics
  or audit events when input recording is off. Echoed input can still appear in
  output recordings.
- Private session/recording directories (`0700`) and files (`0600`) on Unix.
  No claim of reliable automatic secret redaction. Recordings share the run's
  local-user trust boundary; they are not a multi-user security sandbox.
- Rotate at 64 MiB; cap retained recording data at 1 GiB per run and 10 GiB per
  recording store, including indexes/checkpoints. Default expiry is seven days
  after a run ends. Expiry/global cleanup removes completed recordings first;
  report deletions and resulting cursor loss. Limits and expiry are configurable.
- Before checkpoints, do not delete a live recording's prefix. On quota/storage
  failure, close the exact recorded prefix and transition to
  `Stopped { last_recorded_sequence, reason }`. Keep the process and live viewers
  running, explicitly report the unrecorded suffix in status/events/attach, and
  reject replay/catch-up requests that need it. This applies to disk-write failure
  and exhausted recorder backlog as well as size limits; no unbounded RAM spool.
- Use a bounded recorder backlog so storage cannot indefinitely block PTY draining.
  Normal publication follows successful append. On stalled storage, the coordinator
  ends the contiguous recorded prefix; any late append beyond its declared boundary
  is excluded from the valid recording. Live-only publication is an explicit degraded
  state, not silent success. Do not start a second writer after a stuck one.
- Append completion is not fsync durability. Batch sync at most one second apart
  under normal storage operation and sync on segment close; expose the last synced
  sequence. A crash may lose the unsynced tail. Disk failures cannot promise exact
  recording, bounded resources, and uninterrupted execution simultaneously.

Replay writes only output records and applies recorded timing. It drains and
discards terminal replies from its input and never connects to the live PTY.
Restore local terminal state on cancellation/error. Input records are inspectable
only when present; they are not re-executed.

Core cannot emulate a differently sized terminal and must not depend on Ghostty
honoring `CSI 8 ; rows ; cols t`. Before playback, show the initial/maximum recorded
dimensions and ask the operator to size the window once if needed. Default playback
does not pause for later resize/repaint events: emit one geometry warning and
continue at the recorded pace. A larger viewport prevents some clipping but does
not reproduce wrapping, bottom-relative operations, or every historical layout.
Label this visual playback best-effort when geometry differs; the stored bytes
remain exact. Optional `--strict-geometry` checks every actual dimension change
and pauses until the local size matches, or can be cancelled. Add both modes to
help; uninterrupted strict playback of variable geometry needs the later VT layer.

Record repaint requests separately from ordinary user resize, with reason, applied
dimensions, and a correlation ID for any temporary shrink/restore pair. Default
playback skips repaint actions/geometry prompts, **not** their output bytes. If a
nudge really changed dimensions, retain those changes for strict replay and VT
reconstruction; discarding them would make checkpoints incorrect.

With no screen engine, seeking requires replay from the retained beginning. Export
to asciicast uses an incremental UTF-8 decoder across output chunks; reject invalid
UTF-8 by default, with an explicitly lossy export option if later needed. Export
actual resize effects even when tagged as repaint. The original binary recording
remains the exact source.

## Measure terminal-query behavior before choosing a responder

Slice 1 records a controlled Claude launch, idle period, prompt/response,
detach, repaint, and resize on the target VM, with versions and TERM/dimensions.
Use a bounded capture deadline so a query stall is itself observable. Inspect
queries and any delays offline; compare with an attached terminal. Do not log
real credentials to conduct this measurement.

Measure unchanged-size `TIOCSWINSZ` and an explicit SIGWINCH separately: the first
may not generate a signal. Check whether Claude repaints at unchanged dimensions.
Prefer a same-size repaint when verified; only use a tagged temporary-size nudge
if measurement shows it is necessary. Record the tested Claude/Ghostty versions.

Without the extension, the attached terminal answers queries. Detached core has
no VT engine and does not claim general query support. If the measured Claude
workflow stalls, fixing that is a slice-1 acceptance blocker: either implement a
small documented responder for the measured subset with honest capabilities and
exclusive reply ownership, or bring forward the necessary screen-extension work.
Do not invent cursor-position replies without a model, or claim an idle timer
proves Claude completed a turn. Structured completion comes from hooks.

The final authority contract has one responder per attach epoch. A companion
responds only when designated; responses pass through the same serialized PTY
writer via a restricted protocol-reply path, not an agent input bypass. Ownership
transitions must suppress duplicate/late replies. Offline playback/checkpoint
rebuild discards all write-back effects. Test these rules before enabling replies.

## Slice 1 — usable cloud Claude through existing SSH

Implement controller transitions, one PTY writer, exact output recording with
limits, verified private sockets, shared-FD-aware I/O, bounded viewer delivery,
initial dimensions, and the query/repaint measurement. Harden attach with explicit
takeover, retirement of the old connection, ongoing resize forwarding (a
`TIOCGWINSZ` poll, woken early by SIGWINCH), EOF handling and signal-driven
cancellation (SIGHUP/SIGTERM/SIGINT), and a best-effort repaint nudge.

Use a two-key escape with a fixed prefix, **Ctrl-\\ then d**, which `--escape none`
turns off. **Ctrl-\\ then Ctrl-\\** forwards one literal prefix; other prefix
combinations forward unchanged. The prefix and `d` are recognised in the legacy
byte encoding, the kitty keyboard protocol and xterm `modifyOtherKeys` (#109).
Document the prefix buffering behavior and exercise it across read boundaries.
Ctrl-] alone passes through to Claude. Do not require Enter to detach: it may
submit an unfinished prompt. Do not use newline-tilde-dot as the default either:
the outer `ssh -t` client consumes it as its own disconnect escape. Test the actual
nested SSH path, literal keys, paste, and cancellation as well as a local PTY.
A repaint does not restore history and must not masquerade as a screen snapshot.

A fixed prefix with an on/off switch avoids the complexity of a configurable
prefix before there is a measured need for one; `--escape` already takes a value
(`ctrl-backslash` or `none`), so a configurable prefix can be added later without
a flag-shape change (decided 2026-09-26).

Acceptance is remote: launch via Tendr on a Linux VM, attach from Ghostty over
`ssh -t`, leave the old connection half-open, attach again with takeover, and
prove the new controller works while old input/resize is rejected. Also cover
agent → human → explicit agent reacquisition. A blocked child input or stalled
viewer must not prevent takeover. Claude remains the same running process.
Repeat at least 32 idle half-open reconnects with the viewer set full and verify
that descriptors, tasks, and admission slots do not grow with reconnect count.

## Slice 2 — replay in Ghostty without the extension

Implement `tendr replay` for a local recording/bundle and asciicast export.
Retrieve closed remote segments and their manifest through existing SSH/file
transfer; replay on the laptop. Do not execute the replay in the live Claude
session. Publish recordings using a consistent high-water boundary if exporting
an active run. Validate geometry, invalid UTF-8, incomplete tails, rotation,
quota handling, permissions, and cancellation. A recording with repeated attaches
and window resizes must play without repeated prompts in default mode; strict
mode tests separately establish its geometry checks. No PNG/WebM pipeline is needed.

## Slice 3 — typed SSH bridge and resumable observation

Move local terminal handling to the laptop. Use `ssh -T` to a constant remote
Tendr entrypoint, carrying a bounded versioned handshake and subsequent typed
PTY frames. No user payload is reconstructed into remote shell argv. Share the
codec/validation with [remote frame transport](01_remote-frame-transport.md);
do not wait for its general operation migration or Windows work.

Separate bounded control requests from bulk work traffic so takeover is never
queued behind a stalled output stream. Define connection setup, deadlines, EOF,
protocol version mismatch, and remote errors. The remote sidecar owns controller
epochs and recordings; the SSH bridge is a replaceable client with no lifecycle
authority. Reconnect explicitly supplies run identity and the last consumed
sequence. It never resends unacknowledged user input automatically.

Expose byte send, resize, claim/release, and observe through a typed CLI contract.
JSON results include run ID, request ID, epoch where relevant, accepted byte count,
and output cursor; stderr contains diagnostics. Reserve a new operation-specific
range rather than giving legacy `wait` status codes 1–4 new meanings:

| New PTY operation exit | Meaning |
|------------------------|---------|
| 0 | Success |
| 44 | Cursor/history unavailable; reuse the existing event-cursor meaning |
| 80 | Request validation or protocol-version failure after CLI parsing |
| 81 | Controller conflict, stale epoch, or stale run |
| 82 | Operation deadline exceeded |
| 83 | Missing/incompatible screen extension |
| 84 | Runtime/transport/storage failure, including a partial input write |
| 85 | Peer identity/permission rejection |

Provide stable error kinds within those categories and accepted byte counts for
partial writes. Clap's existing parse failure remains 2; intercepted signals and
pre-dispatch failures retain their documented CLI behavior. Reserve/check these
codes when implementation starts. Publish one command-scoped table in `docs/guide.md`
before shipping slice 1 (and extend it with later slices), covering legacy
`wait`/`run`/`exec`/`events`, parser/dispatch errors, and the new PTY operations.
Historical codes are already command-dependent: for example `wait` timeout is 1
and spawn failure is 2. Do not claim global uniformity or silently renumber them.
Reads are retryable, bounded writes are not automatically retried, observation
is explicitly streaming, and every wait has a finite default deadline.

## Slice 4 — external Rust screen extension

Add an independently packaged `tendr-screen` executable. A small core dispatch
table delegates only recognized operations such as `tendr pty snapshot`, screen
waits, and mode-aware keys/paste. Call it with `Command` and structured stdin;
never use shell interpolation or implicit installation. Require a protocol/version
and capability handshake. Missing/incompatible extension returns an actionable
typed error; core launch/attach/record/replay continue to work.

For `--host`, resolve and invoke the extension on the target host through the
typed bridge. Maintain one VT state worker per opted-in run beside its sidecar;
it observes Tendr's sequence stream and never owns another child PTY. Tendr
binds its lifecycle to the run. A worker crash leaves the child alive, marks screen
operations unavailable/stale, and permits reconstruction from retained history.
If designated query answering is lost, report that capability loss explicitly.

Pin the Rust bindings, Ghostty source, and required Zig version together; isolate
FFI and suppress callbacks during replay. Build the extension separately from the
core crate. Nix packaging must prefetch native sources/dependencies and build
without network access; core artifacts/Windows CI remain independent of Zig.
Reference: [libghostty-rs build contract](https://github.com/Uzaaft/libghostty-rs).

A snapshot/checkpoint carries run ID, last included sequence, dimensions, terminal
profile, and engine/format version. Persistent checkpoints need full restorable
terminal/parser state, including pending partial sequences; a text grid is not
sufficient. Validate the pinned engine's snapshot facility; if incomplete, retain
the required raw prefix instead of claiming bounded-cost restoration.

Publish a checkpoint atomically before deleting any prefix it replaces. Budget
checkpoint storage within retention limits. Reattach restores the snapshot then
streams strictly after its sequence; output arriving during snapshot creation is
retained and spliced without duplication/gaps. Resize/epoch changes invalidate
incompatible snapshots. An incompatible/missing checkpoint uses retained raw
history or returns a precise unavailable-history error.

Screen waits return the observed sequence and accept an output-cursor lower bound
to avoid matching stale screens. This does not prove that a prompt completed;
key encoding and bracketed paste use current terminal modes, and mutation still
requires the current controller handle. Ghostty window layout stays client policy.

## Per-slice invariant and verification table

| Slice | Invariant | Enforced by | Verified by | Limit |
|-------|-----------|-------------|-------------|-------|
| 1 | Old epochs cannot write after acknowledged takeover | Single writer and per-write validation | Half-open SSH, queued/partial write, blocked child, old disconnect, stale run tests | Kernel-buffered bytes cannot be recalled |
| 1 | Terminal bytes stay within the owner's account | Private socket directory, bidirectional peer UID checks, bind-before-readiness | Foreign-UID spoofed server/client, pre-created path, symlink, bind failure, short-path tests | Cross-UID Linux test requires two test accounts; do not mark passed if unavailable |
| 1 | A surviving run remains attachable after logout and aging cleanup | Persistent socket root by default; verified lifetime for overrides; directory lock where supported | Real last-session logout/relogin, lingering on/off, overlong path, isolated age-cleanup while idle | Process-killing login policy and unmounted home/state roots require a separate supported launch/storage configuration |
| 1 | Nonblocking input does not terminate output capture | Shared mode setup and readiness-aware read/write loops | Idle then output, EAGAIN/EINTR injection, full input buffer plus output, hangup drain on Linux/macOS | PTY close errors differ by platform |
| 1 | Reconnect does not accumulate stale clients | Old connection retirement and reserved bounded admission | 32 idle half-open takeovers with full viewer slots; descriptor/task accounting | No dependency on SSH front-end idle timeout |
| 1 | Detach preserves ordinary Claude input | Two-key parser, literal-prefix and disabled modes | Ctrl-], partial prefix, literal prefix, paste, kitty/modifyOtherKeys encodings and nested SSH fixtures | Chosen prefix is documented, not universally unused |
| 1 | Viewers cannot block capture/control | Byte-bounded queues, cancellation, independent control path | Stalled reader plus sustained child output and concurrent takeover | Storage failure becomes explicit recording loss |
| 1 | Recorded bytes are exact and geometry is ordered | Byte codec, single sequencer, atomic segment metadata | Split UTF-8/VT property tests, resize ordering, crash-tail fixtures | Appended vs synced boundary is reported |
| 1 | Query handling supports the measured Claude workflow | Measured capability/reply policy | Real pinned Claude on target, attached/detached comparison | No claim of general detached terminal support |
| 2 | Playback cannot send input to a child or prompt at each default-mode resize | Output-only replay, reply draining, explicit geometry modes | Query responses, cancellation, repeated repaint/resize, strict-size checks | Default mismatched geometry is best-effort; no input re-execution |
| 2 | Retention and privacy are explicit | Recording policy and bounded store | Rotation, quota, input-off, private modes, disk fault and expiry tests | Output may contain echoed secrets |
| 3 | Reconnect yields a contiguous known suffix | Run-bound handshake and cross-segment decoder validation | Network interruption, duplicate/lost/regressing sequence, pruned cursor, replaced run, equal timestamps | Missing history is an error |
| 1–4 | New errors do not overload legacy status codes | New-operation range, typed errors, command-scoped guide table | CLI/bridge/extension exit-code tests including parser errors and partial writes | Legacy codes retain historical command-specific meanings |
| 4 | Snapshot plus suffix equals continuous processing | Full checkpoint and sequence barrier | Alternate screen, split escape, Unicode, resize, concurrent output, checkpoint rebuild | Pinned engine capabilities bound fidelity |
| 4 | Exactly one live query responder, none during replay | Designated reply authority and epoch checks | Attach transition/query race, worker failure, offline rebuild | No duplicate reply fallback |
| 4 | Core stays independent of native screen dependencies | External process/package boundary | Core build without Zig/extension; missing/version-mismatch tests; Nix extension build | Optional native package has its own support matrix |

Before each implementation slice is complete: run the relevant behavior and
property tests, `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
and the repository-required checks. Review readiness and cleanup paths, frame
bounds, cancellation, permissions, and error contracts. Use disposable test runs
for network/disk faults. Record versions, commands, results, and untested limits
in the implementation PR; never infer cloud acceptance from local unit tests.

## Remaining work

- [ ] Slice 1: steps 5, 6, 8 and 9 below.
- [ ] Slice 2: core playback and export with bounded recording storage.
- [ ] Slice 3: typed bridge and cursor-based observation.
- [ ] Slice 4: independent screen extension and validated full checkpoints.

The query measurement and the pinned checkpoint API validation are
implementation gates, with the fallback behavior described above. Neither has
passed yet. Packaging and acceptance target an SSH-accessible Linux VM. The
exe.dev management API is not a PTY carrier, and the core protocol needs no VM
provisioning.

### Slice 1: remaining steps (one PR each)

Shipped in v0.4.0: input authority, takeover, private sockets and exact
recording (#68); signal cancellation and SIGWINCH (#105); escape fixtures
(#104); reconnect accounting (#108); the recording dump tool
`examples/dump-recording.rs` (#103); extended-key escape (#109); the exit-code
table (#107); `kill` hangs up a PTY session first (#114); PTY `push` without
`--stdin` (#115); `TERM` repair (#117); the `--host` connect deadline (#119).

Slice-1 admission design: one pool of eight connections, pending hellos
included (`MAX_ATTACH_CONNECTIONS`). Eight connections that never send a hello
refuse a takeover for up to `HELLO_TIMEOUT` (5 s). The socket is owner-only and
peer-verified, so only the owner can cause this. A separate pending-hello pool
comes with slice 3's viewers.

5. **Query and repaint measurement on the VM** (VM, docs only). Detached vs attached Claude; same-size reattach redraw; same-size resize vs `kill -WINCH` vs rows−1/restore; `Ctrl-\ d` and `Ctrl-]` through Ghostty → `ssh -t`; exe.dev half-open timing. Launch environment in each way the workflow starts Claude: from the SSH login shell, by an agent's bash tool on the VM, and by `tendr --host` from the laptop (`ssh -T`, so no `TERM` from sshd). A PTY child inherits the caller's environment except `TERM`, which is `xterm-256color` when the caller's is unset, empty or `dumb`; omp's non-PTY bash tool sets `TERM=dumb`, `CI=true`, `NO_COLOR=1` and also `EDITOR`/`VISUAL`/`GIT_EDITOR=true`, `PAGER`/`GIT_PAGER=cat`, `GIT_TERMINAL_PROMPT=0` (measured locally, 2026-10-30). Record the full `env | sort` diff between the launch paths and how Claude renders and behaves under each; `CI` and `NO_COLOR` are the ones still expected to change rendering. Output: a "Measurement" subsection plus the decision for step 6, and whether `start --pty` should offer an env unset. If Claude stalls detached, that is a slice-1 blocker. `examples/dump-recording.rs` flags the query sequences to inspect.
6. **Best-effort repaint nudge after (re)attach** (local, design fixed by step 5). New `MSG_REPAINT`; recorded as `ResizeCause::Repaint { correlation }` (the codec supports it; nothing emits it yet). Tests: `reattach_at_the_same_size_records_a_tagged_repaint_pair`, `reattach_at_a_new_size_records_only_a_user_resize`, `a_repaint_is_authorized_like_input`.
8. **Nested `ssh -t` fixtures, opt-in** (sshd; `TENDR_TEST_SSH_HOST`). `tests/cli_attach_ssh.rs`: detach on `Ctrl-\ d`, `Ctrl-]` passthrough, paste with prefixes, terminal restore, takeover over a stopped first client.
9. **Remote acceptance on the Linux VM.** Same Claude pid throughout; attach from Ghostty; half-open two ways plus takeover; agent → human → agent; blocked child input and stalled viewer during takeover (time it); 32 idle half-open reconnects with fd/task/admission counts equal before and after; logout/relogin; cross-user rejection; dump the whole recording. Before and after `tendr kill`, and separately `--timeout`, snapshot `ps -eo pid,ppid,pgid,sid,comm` and diff: the sid is the Claude pid, and anything left with it (shells, MCP servers, background jobs) outlived the stop. `kill` hangs up a PTY session first; `--timeout` SIGKILLs at once, so record what it leaves and decide whether it should hang up too.

Steps 5 and 9 have a VM harness in [`scripts/pty-vm/`](../../../scripts/pty-vm/README.md):
VM setup, test children, half-open timers and a PTY driver. It was written for
a development build of #68 and has not run on 0.4.0.

Gates for every step: `cargo fmt --all --check`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo test --locked --all-targets --no-fail-fast`, `cargo test --locked --doc`, `cargo +1.85.0 check --locked --all-targets`, `cargo doc --no-deps` with `-D warnings`. Red-first for each new test. Never mark a VM row passed from local tests.

### Open measurements and references

exe.dev's half-open SSH timeout is unmeasured and is not a correctness
dependency. Claude's same-size repaint and its handling of the detach prefix
through Ghostty are step-5 measurements. The deployed host policies (exe.dev,
NixOS) for logout and temporary-file aging are not validated.

Found while building that harness on an exe.dev VM (2026-09-26):

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

- [Linux dup: shared file status flags](https://man7.org/linux/man-pages/man2/dup.2.html)
- [Linux poll: writable readiness and blocking writes](https://man7.org/linux/man-pages/man2/poll.2.html)
- [OpenSSH escape characters](https://man.openbsd.org/ssh#ESCAPE_CHARACTERS)
- [Ghostty terminal stream handler](https://github.com/ghostty-org/ghostty/blob/main/src/terminal/stream.zig)
- [pam_systemd logout contract](https://github.com/systemd/systemd/blob/main/man/pam_systemd.xml)
- [tmpfiles aging contract](https://github.com/systemd/systemd/blob/main/man/tmpfiles.d.xml)
