---
id: content-addressable-storage
depends_on: []
---

# Content-addressable storage extensions

**Status:** not started. Waiting for a consumer that meets the trigger below.

Oversized event payloads already spill into per-session
`events/blobs/<sha256>` and appear as `data_ref`. `prune` owns session-local
retention. Do not build a global store, reference counts, a replay cache or a
new `block` record vocabulary until a consumer needs them.

## Reconsideration trigger

Reopen this work only when a named consumer requires at least one capability
that the shipped per-session blob store cannot provide:

- a reproducible export/crash bundle with verified payload hashes;
- a new captured-artifact class that must use `data_ref`;
- measured storage pressure where cross-session deduplication has material
  value; or
- retention/garbage-collection behavior that `prune` cannot express.

The consumer must supply representative data and an ownership model for
retention. Cross-session deduplication is not a local layout tweak: it breaks
the current rule that deleting a session deletes all of its events and blobs.

## Constraints

- The event envelope and `data_ref` schema remain owned by the
  [event protocol](../../architecture/07-event-protocol.md).
- Blob keys are SHA-256 of the exact serialized payload bytes.
- Writes remain temp-file plus atomic rename; readers verify size/hash when
  integrity matters.
- Existing session directories and event readers remain compatible.
- No daemon, global block index, hidden database, or cache-driven execution.
- Until a consumer proves otherwise, session-local blobs plus `prune` are the
  complete design.

Earlier global-store sketches are in Git history.
