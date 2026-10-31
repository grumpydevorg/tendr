---
id: partial-line-telemetry
depends_on: []
---

# Partial-line telemetry: make a silent session explain itself

**Status:** not started. Ready, except for one open question: how the sidecar
passes the counters to the CLI.

Expose how much output a session has buffered but not yet written as a record,
and how long since the last record. An empty `output.log` then reports its own
cause. Split from [log-delimiter-cr-framing](log-delimiter-cr-framing.md).

## Why

A 251 GiB `rsync` ran 75 minutes under a pipe session with a 0-byte `output.log`.
The log was empty because the child's progress meter is `\r`-framed and pipe capture
emits on `\n` — correct per the log contract, invisible to the operator.

[log-delimiter-cr-framing](log-delimiter-cr-framing.md) makes that *fixable* by
adding `--log-delimiter newline-or-cr`. It does not make it *diagnosable*: the
default stays `newline`, so the next person starts the same session, sees the same
empty log, and has nothing to go on. They cannot distinguish:

- a child buffering a partial line (progress meter, no newline yet),
- a child that has genuinely produced nothing,
- a wedged or deadlocked child,
- a broken capture path.

All four look identical from outside: an empty file. One counter separates them.

This is deliberately generic — it names no program and encodes no progress-meter
policy. It reports a property of the stream.

## What

Two values per session:

- `pending_partial_bytes` — bytes received since the last emitted record.
- `last_record_age_s` — age of the most recent record (or of session start).

Surfaced in `tendr status`. A session reporting 400 KB pending with no record for
20 minutes has just explained itself, and `tendr guide` can point at exactly that
reading.

## The hard part: the sidecar is not the CLI

The counter lives in the sidecar's reader thread. `tendr status` is a **different
process** that reconstructs state by reading `meta.json` off disk
(`src/commands/status.rs` → `session::read_meta`). An in-memory counter is invisible
to it. This is the whole design problem, and it is why this is a separate plan
rather than a bullet in the framing one.

Three options:

1. **Sidecar-owned telemetry file.** A small `progress.json` (or fixed-width
   record) the sidecar rewrites at a bounded cadence; `status` reads it
   best-effort and reports staleness rather than blocking. Fits the existing
   file-based IPC pattern (`kill_request`, lease files, push rejects). Needs a
   write-cadence decision — chatty enough to be useful, quiet enough not to be a
   disk-churn regression on a busy session.
2. **Derive it at read time from `output.log` mtime.** Gets
   `last_completed_record_at` with **zero** new IPC — and nothing else.
   **It cannot separate "wedged" from "working," and the motivating incident is the
   proof.** At 16:19, 63 minutes into a transfer moving ~33 MB/s, the session
   directory read:

   ```
   -rw-r--r--  1 rick rick    0 Jul 27 15:16 output.log
   ```

   Zero bytes, mtime still the creation time, while the child was emitting
   continuously. A hung child, a child doing legitimate silent work, and a child
   flooding a partial line are all identical under mtime. Use it for
   `last_completed_record_at` and nothing more.
3. **Sidecar control protocol.**
   The [sidecar control protocol](sidecar-control-protocol.md) design is not
   scheduled. Its own trigger is a feature that needs portable correlated
   request/response. This is a one-way push of a scalar. **It does not meet that
   trigger** and must not be used to justify building the protocol.

**Do not put these counters in `meta.json`.** That file is the durable lifecycle
record, rewritten on state transitions and read by reconciliation; rewriting it
every few hundred milliseconds would churn the state machine's own storage, race
readers, and bury real transitions in noise. The lifecycle record must stay
low-frequency.

Recommendation: **(1)**, a small rate-limited sidecar-owned telemetry file.
`pending_partial_bytes` is the value that does the work here, and it exists only
inside the sidecar's reader thread — no read-time derivation can recover it. Take
`last_completed_record_at` from mtime alongside, as a cheap cross-check, but not as
the feature.

## Tests

- A child writing `10%\r` repeatedly with no newline reports rising
  `pending_partial_bytes` and a growing `last_record_age_s`.
- A child emitting normal lines keeps `pending_partial_bytes` at ~0.
- A child that has exited leaves both values final and stable, not drifting.
- A wedged child (no output at all) is distinguishable from a buffering one:
  `last_completed_record_at` is equally stale in both, and only
  `pending_partial_bytes` separates them. This test is the whole point of the
  feature — if it passes with mtime alone, the implementation is wrong.
- `status` on a session whose sidecar died reports the values as stale rather than
  asserting them as live.
