# Event Protocol v1

This is the reference for tendr's structured event log: the envelope, the
files, the append rules, ordering, and the read and write commands. Code
comments cite its sections as "spec §N", so the section numbers are stable.
The reasons behind the design are in [../design.md](../design.md#decisions).

## 0. Summary

| Property | Rule |
|---|---|
| Transport | Files. No daemon and no socket; consumers read the files. |
| Identity | UUIDv7 for every ID (`id`, `run_id`, `writer`, `block_id`, `parent_id`). |
| Timestamp | RFC 3339 UTC with exactly 6 fractional digits and `Z`, stamped by the writer when the event happens. `output.log` keeps its own f64 seconds. |
| Oversize payloads | Spilled to a blob, never rejected (§3.4). |
| Retention | The event log lives in the session dir; `prune` deletes it with the session (§3.7). |
| `kind` | An open vocabulary, not a closed enum. Some prefixes are reserved (§1). |
| `watch` | Output shape frozen; read from the event log when one exists (§5.3). `tendr events` is the full protocol surface. |
| `wrap`, `exec` | Write the event, then an `output.log` `A` line linked to it by `event_id` (§6). |

## 1. Envelope

One JSON object per line, written with one `write()` and ended with `\n`.
The tendr binary stamps every field outside `data`; none is copied from user
input. `data` is at most 16 KiB inline; larger payloads spill (§3.4).

| Field | Type | Required | Notes |
|---|---|---|---|
| `v` | int `1` | yes | Envelope version. Additive fields never change it. Consumers must ignore unknown fields and accept unknown `kind`s. |
| `id` | UUIDv7 | yes | Event identity and dedupe key. |
| `ts` | RFC 3339 µs `Z` | yes | When the event happened, stamped by the writer. Never the time a poller noticed it. |
| `kind` | string ≤128 bytes | yes | Routing key and payload-schema ID. See the grammar below. A payload-breaking change needs a new kind (for example `exec.result.v2`), not a new `v`. |
| `namespace`, `session` | strings | yes | The session's namespace and name. |
| `run_id` | UUIDv7 | yes | The supervised run: the emitter's `TENDR_RUN_ID`, or the sidecar's own run. After `--replace` it may name an earlier generation. |
| `gen` | u64 | no | Generation, when known. |
| `writer` | UUIDv7 | yes | The emitting process. The sidecar uses its `run_id`; each CLI writer mints one. |
| `seq` | u64 | yes | Starts at 1 and is contiguous per writer. Detects gaps and breaks ties when merging. |
| `source` | string | yes | The semantic emitter, such as `tendr.sidecar`, `tendr.exec`, `tendr.cli` or `claude.hook`. ASCII letters, digits, `.` and `-`; at least one dot; ≤128 bytes. `tendr.*` is reserved. |
| `block_id` | UUIDv7 | no | The command block (one `exec` or `wrap` call) this event belongs to. Like an OTel span ID. |
| `parent_id` | UUIDv7 | no | The immediate causal parent (§2, §6). |
| `data` | object | no | Payload, ≤16 KiB serialized; otherwise a preview of at most 4 KiB. |
| `data_ref` | `{path, bytes, sha256, media_type}` | no | Spill reference (§3.4). When present, `data` is a preview and `truncated` is `true`. |
| `truncated` | bool | no | `data` is a preview or truncation. |

**Kind grammar.** ASCII letters, digits, `.`, `-` and `_`. At least one
dot, no empty segments, at most 128 bytes.

**Reserved kind prefixes.** `run.`, `log.`, `exec.`, `session.`, `pty.`,
`callback.`, `segment.`, `cursor.`, `tendr.` (`RESERVED_KIND_PREFIXES` in
`src/model/event.rs`).

- `emit --kind` and `wrap --event` exit 6 on a reserved prefix.
- Only tendr's internal call sites write reserved kinds. The append layer
  itself does not filter kinds.
- `hook.` is not reserved. It is the conventional prefix for external
  lifecycle-hook events (Claude Code, CI), written with `wrap --event hook.*`
  or `emit`.

**Kinds tendr writes.**

| Kind | Writer | Notes |
|---|---|---|
| `run.starting`, `run.started`, `run.spawn_failed`, `run.exited`, `run.killed`, `run.timed_out`, `run.dependency_failed` | sidecar | One per lifecycle transition. `data` has `status`, `reason` and `exit_code` where they apply, and `provenance` (`"direct"`). |
| `run.sidecar_failed` | sidecar | `data`: `{"status":"Exited","reason":"SidecarFailed","step":"<step>"}`. The sidecar's direct record that it failed and stopped the child. |
| `run.sidecar_lost` | CLI reconciliation (`source: tendr.cli`) | `data.provenance` is `"inferred"`. |
| `callback.finished` | sidecar | One per `--on-exit` hook. |
| `pty.control_changed`, `pty.input_revoked`, `recording.stopped` | sidecar | PTY sessions ([04-pty-lane.md](04-pty-lane.md)). CLI reconciliation also logs `recording.stopped` (reason `sidecar_lost`) for a lost sidecar. |
| `exec.started`, `exec.result` | `exec` (`source: tendr.exec`) | `exec.result` holds the `ExecResult` fields. |
| `log.stdout`, `log.stderr` | `events --include-logs` | Derived at read time, never stored (§5.1). |
| `cursor.bookmark` | `events --cursors` | Derived at read time, never stored (§5.2). |

`run.starting` and `run.started` also carry the session's `boundary`, when
one was set at `start`. Terminal events do not repeat it.

### 1.1 Examples

**(a) Lifecycle transition**, written by the sidecar before it writes
`meta.json`:

```json
{"v":1,"id":"01981f2e-9a3b-7c1d-8e4f-0a1b2c3d4e5f","ts":"2026-07-06T03:14:15.926535Z","kind":"run.exited","namespace":"default","session":"build","run_id":"01981f2d-1111-7abc-9def-556677889900","gen":3,"writer":"01981f2d-1111-7abc-9def-556677889900","seq":7,"source":"tendr.sidecar","data":{"status":"Exited","reason":"ExitedError","exit_code":3,"provenance":"direct"}}
```

**(b) Hook event via wrap**: `tendr wrap --source claude.hook --event
hook.post_tool_use -- ~/.claude/hooks/post.sh`

```json
{"v":1,"id":"01981f30-2222-7abc-8def-aabbccddeeff","ts":"2026-07-06T03:14:16.101833Z","kind":"hook.post_tool_use","namespace":"default","session":"agent","run_id":"01981f2d-1111-7abc-9def-556677889900","gen":3,"writer":"01981f30-2221-7abc-8def-001122334455","seq":1,"source":"claude.hook","parent_id":"01981f2f-3333-7abc-8def-667788990011","data":{"hook_stdin":{"tool_name":"Bash"},"hook_stdout":"","hook_stderr":"","hook_exit_code":0,"command":["/Users/rick/.claude/hooks/post.sh"],"truncated":false}}
```

**(c) Arbitrary emit**: `tendr emit --kind build.finished --source ci.local
--data '{"ok":true,"artifacts":3}'`

```json
{"v":1,"id":"01981f31-4444-7abc-8def-102030405060","ts":"2026-07-06T03:15:02.007412Z","kind":"build.finished","namespace":"default","session":"build","run_id":"01981f2d-1111-7abc-9def-556677889900","gen":3,"writer":"01981f31-4443-7abc-8def-605040302010","seq":1,"source":"ci.local","data":{"ok":true,"artifacts":3}}
```

**(d) Exec result with a spilled payload**: 1 MiB of stdout, stored as a blob
keyed by its SHA-256:

```json
{"v":1,"id":"01981f32-5555-7abc-8def-abcdefabcdef","ts":"2026-07-06T03:16:40.550021Z","kind":"exec.result","namespace":"default","session":"duck","run_id":"01981f2d-1111-7abc-9def-556677889900","writer":"01981f32-5554-7abc-8def-fedcbafedcba","seq":2,"source":"tendr.exec","block_id":"01981f32-5550-7abc-8def-111122223333","data":{"exit_code":0,"cwd_after":"/data","timed_out":false,"stderr":"","stdout_preview":"┌─────────┐…","truncated":true},"data_ref":{"path":"events/blobs/9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08","bytes":1048576,"sha256":"9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08","media_type":"application/json"}}
```

## 2. Identity and causality

The scope chain is `(namespace, session)` → `run_id` → `block_id` → `id`,
plus `parent_id` edges. A consumer can rebuild the tree from three foreign
keys. OTel export maps `run_id` to trace ID, `block_id` to span ID and
`parent_id` to parent span ID.

Two environment variables chain events across processes:

| Variable | Set by | Value |
|---|---|---|
| `TENDR_BLOCK_ID` | `exec` (POSIX shell target only, for the command's duration); `wrap` (valid `--event` kind only) | The call's fresh `block_id`. |
| `TENDR_PARENT_EVENT_ID` | `wrap` (valid `--event` kind only) | The `id` of the event `wrap` will write after the child exits. |

So a `tendr emit` run from a hook or an exec command chains to that block.
Python REPL, PowerShell, DuckDB and NDJSON exec frames do not set
`TENDR_BLOCK_ID`.
A malformed value in either variable prints a warning on stderr and is
ignored; it never fails the producer.

## 3. Storage and write path

### 3.1 Layout

Events are stored per session only:

```
~/.tendr/sessions/<ns>/<session>/
  meta.json            # current state
  output.log           # raw output and A-line annotations
  events/
    <uuidv7>.jsonl     # event segments: the history
    blobs/<sha256>     # spilled payloads
    append.lock        # advisory lock file (Unix only, §3.2)
```

There is no namespace-level event file. A namespace view is merged at read
time from each session's segments.

### 3.2 Append protocol

Every writer appends the same way (`append_line` in `src/events.rs`):

1. Pick the greatest `events/*.jsonl` by name. UUIDv7 names sort by creation
   time, so that is the newest. If there is none, create one with
   `create_new`; a writer that loses the race opens the winner's file.
2. Open it with `append(true)`.
3. On Unix, take `flock(LOCK_EX)` on `events/append.lock`, a separate lock
   file. Readers never take it. On Windows, take no lock.
4. Serialize the whole line and write it with one `write_all`. Sync it if
   the write is durable (§3.5). Release the lock.

Why each platform is safe:

- POSIX makes `O_APPEND` positioning atomic, but does not promise that
  concurrent writes to a regular file never interleave. `PIPE_BUF` applies
  only to pipes and FIFOs. macOS gives no such promise either. The `flock`
  makes it a rule among tendr's own writers, and readers are never blocked.
- On Windows, Rust's `OpenOptions::append` removes `FILE_WRITE_DATA`. That
  gives a documented atomic append per `WriteFile` across processes on NTFS.
  `LockFileEx` would be mandatory and would block readers, so tendr does not
  take it.
- Session roots on NFS or SMB are not supported. `O_APPEND` is a documented
  race on NFS. The same applies to `output.log`.

Two further checks: JSONL resynchronizes at the next `\n`, so a torn or
foreign fragment fails to parse and is counted. The per-writer `seq` turns
silent loss into a visible gap.

### 3.3 Segments

Segment files are never renamed. A segment's name is its permanent identity,
so cursors (§5.2) need no inode or rename handling. Writers append to the
newest segment and create one only when `events/` has none.

### 3.4 Oversize spill

When serialized `data` is over 16 KiB:

1. The full `data` JSON is written to `events/blobs/<sha256>` (temporary
   file in the same directory, then rename). Identical payloads share one
   blob.
2. The event is appended with a preview of at most 4 KiB in `data`,
   `truncated: true`, and `data_ref`. A writer may supply its own structured
   preview (`exec.result` keeps `exit_code` and `cwd_after`); otherwise the
   preview is `{"preview": "<first 4 KiB of the JSON>"}`.

If the blob write fails, the event is still appended with the preview and
`truncated: true`, without `data_ref`. The event is never dropped.

### 3.5 fsync

By default no event is synced. Page-cache durability survives an emitter
crash; a power loss risks only the writeback window. These are synced with
`fdatasync` (`F_FULLFSYNC` on macOS):

- `tendr emit --durable`
- every sidecar terminal transition, and `DependencyFailed`
- reconciliation's `run.sidecar_lost`

### 3.6 Write-ahead ordering

The sidecar appends each lifecycle event **before** it writes `meta.json`. It
syncs the segment before any *terminal* meta write.

- This orders writes against a crash. A sidecar that dies between the two
  writes leaves the event without terminal meta, never the reverse.
- It does not protect against I/O failure. If the append itself fails, the
  sidecar still writes terminal meta (meta stays the current-state
  authority). It adds a meta warning and saves the fully-addressed event to
  `~/.tendr/lost+found/events.jsonl`.

Before reconciliation infers `run.sidecar_lost`, it reads the event log. If
the sidecar's own terminal event is there, it heals meta from that event
with `event_log_terminal` evidence
([03-run-lifecycle.md](03-run-lifecycle.md)). Reconciliation's own
`run.sidecar_lost` is also appended before its meta write.

### 3.7 Retention

`prune` deletes the session dir, and the events and blobs go with it.
`--replace` also replaces the session dir, so a new generation starts a new
event log. `prune` does not touch `~/.tendr/lost+found/` or
`~/.tendr/callbacks/`.

## 4. Ordering and timestamps

Guaranteed:

- total order per writer, by `seq`, with gaps detectable
- declared causal order, through `parent_id` and `block_id`
- a deterministic merge across writers, by `(ts, writer, seq)`
- `ts` is when the event happened; an inferred transition says so with
  `data.provenance: "inferred"`

Lifecycle events use the same provenance vocabulary as `meta.json`.
`data.provenance` is `"direct"` or `"inferred"`, matching
`transition_provenance.kind` (`src/model/provenance.rs`). Meta holds the
latest transition only; the event log holds the history.

Not guaranteed:

- a total order across writers, sessions or hosts
- monotonic wall-clock time: clocks can step, and `seq` is the per-writer
  truth

## 5. Read surface

### 5.1 `tendr events`

```sh
tendr events [--namespace ns] [--session ns/name]... [--kind prefix]... \
  [--source prefix]... [--follow] [--from-now | --from-cursor <c> | --since <rfc3339>] \
  [--last N] [--cursors] [--include-logs] [--strict] [--ready-file <path>]
```

- **Replay** reads every segment of the matching sessions in name order,
  merges by `(ts, writer, seq)`, and writes NDJSON to stdout.
- **`--follow`** then polls every 100 ms (`POLL_INTERVAL`). Only complete,
  `\n`-terminated lines are read; an unfinished last line waits for its
  writer.
- **`--last N`** replays only the last N events by merge order.
- **`--include-logs`** merges `output.log` `O` and `E` lines in as
  `log.stdout` and `log.stderr` records. They are derived at read time:
  `"derived": true`, no `id`, `writer` or `seq`.
- **`--strict`** exits 65 if any unparseable line was skipped.
- **`--ready-file`** (with `--follow`) creates the file once the baseline is
  set and the initial replay is flushed.

### 5.2 Cursors

A cursor is an opaque URL-safe base64 token. Inside it is
`{"v":1,"s":[["<ns>/<session>/events/<seg>.jsonl", offset], …]}`: a byte
offset after the last fully read line of each segment. Segments are never
renamed, so a cursor stays valid as long as its files exist.

- `--cursors` interleaves `cursor.bookmark` records into the output, every
  100 events or after 5 s idle.
- `--from-cursor <token>` resumes exactly from that point.
- If a segment named in the cursor is gone, or the token is malformed or of
  an unknown version, `events` exits **44** and prints on stderr:
  `{"error":"cursor_gone","gone":[…],"recover":"replay without --from-cursor, or use --since <ts>"}`.
  It never silently restarts from zero.

### 5.3 `tendr watch`

`watch` keeps its output shape: f64 `ts`, the kind/name split, and its event
names. For a session with an `events/` directory, `watch` reads the event
log, so it reports true timestamps, every transition, and real sources. For
a session without one, it derives events from changes in `meta.json`.

## 6. Write surface

```sh
tendr emit --kind <kind> [--data <json> | --data-file <p> | --data-stdin] \
  [--source <source>] [--session <ns>/<name>] [--parent <uuid>] \
  [--durable] [--best-effort]
```

`data` must be a JSON object. `--source` defaults to `user.emit`; a hook
that wants attribution passes it (`--source claude.hook`). Without
`--session`, the target comes from the supervised-run environment
(`TENDR_NAMESPACE`, `TENDR_SESSION`, `TENDR_RUN_ID`).

Ambient chain:

- `block_id` comes from `TENDR_BLOCK_ID`.
- `parent_id` comes from `--parent`, else `TENDR_PARENT_EVENT_ID`, else
  `TENDR_BLOCK_ID`.
- A `--parent` that does not parse exits 2. A malformed environment value
  prints a warning and is ignored.

| Exit | Meaning |
|---:|---|
| 0 | appended, or preserved in lost+found (§7) |
| 1 | I/O failure |
| 2 | usage: bad `--parent`, bad `--session`, or `data` not a JSON object |
| 3 | no session given and none in the environment, or an invalid `TENDR_NAMESPACE`/`TENDR_SESSION` |
| 5 | session not found or corrupt |
| 6 | invalid kind or source, including a reserved prefix |

`--best-effort` turns every failure into exit 0, so a hook never fails its
host tool.

**`exec`** writes `exec.started` and `exec.result` from its own call site.
That is allowed because the kind is stamped by tendr, not given by the user.
Its `output.log` `A` line carries `event_id` and `block_id`
([05-key-flows.md](05-key-flows.md#events-and-annotation)).

**`wrap --event`** takes one of three paths, decided before any side
effect:

| `--event` value | Result |
|---|---|
| a valid kind, not reserved | Writes the event and an `A` line linked by `event_id` and `block_id`. Sets `TENDR_BLOCK_ID` and `TENDR_PARENT_EVENT_ID` for the child (§2). |
| a reserved kind | Exits 6. The child never runs and nothing is written. |
| not a valid kind (for example a dotless name) | Writes only the `A` line: no event, no environment chain. Prints a notice on stderr every time. |

A name that fails the grammar is not a kind, so it can never produce a
stored event and cannot reach the reserved prefixes.

`wrap` mints its event `id` and puts it in the child's
`TENDR_PARENT_EVENT_ID` before it spawns the child. If `wrap`'s best-effort
append then fails, events the child emitted name a parent that was never
written. A `parent_id → id` join drops those edges. The `A` line still
records `block_id`, without `event_id`.

## 7. Orphan emitters: lost+found

`emit` from a process whose session dir no longer exists (pruned) appends
the fully-addressed event to `~/.tendr/lost+found/events.jsonl` and exits 0.
This needs the session and run from the environment. The session dir is
never recreated.

After `--replace`, an `emit` from the old generation appends to the new
session dir's log, with the old `run_id` from its environment. So it stays
attributable.

The sidecar also uses lost+found when its own event log is unwritable
(§3.6).

## 8. Remote and Windows

- `events` and `emit` are local-only. `--host` rejects them. Merging several
  hosts' streams by `ts` is up to the consumer and is best-effort.
- `emit`, `wrap` and `exec` run on the session's host, because the children
  run that host's tendr binary.
- Windows uses the append contract in §3.2: `CREATE_NEW` races, no renames,
  no lock. A PowerShell exec still uses its side-channel file; the CLI
  writes `exec.result` after reading it.

## 9. Failure modes

| Failure | Behavior |
|---|---|
| Torn line | Skipped and counted; `--strict` exits 65. The writer's `seq` shows a gap. |
| Sidecar crash between event and meta write | Terminal transitions: the synced event survives and reconciliation heals meta (§3.6). A non-terminal event may exist without its meta update. |
| Reader during a new segment | No race: segments are never renamed. |
| Clock step | `seq` keeps per-writer order. |
| Slow consumer | No backpressure: the disk is the buffer, and writers never wait for readers. |
| Follower whose segment is gone | Exit 44 with recovery advice (§5.2). |
