---
id: log-retention
depends_on: []
---

# Log retention

**Status:** only `output.log` is left: it has no rotation and no size cap.
Event segments, lost+found and `--on-exit` results shipped in PR #211; their
limits are in
[session storage](../../architecture/02-session-storage.md#limits). The
`output.log` part is in progress in its own PR.

## Goal

A session that runs for weeks keeps bounded disk use, and nothing under
`~/.tendr` grows for ever.

## Open questions

- `output.log`: rotate by size and keep the last N files, or cap and drop the
  oldest lines? `tendr log --since` and `--tail` must keep working across
  rotated files.

## Acceptance

- A test writes past the size limit and shows that `tendr log` still returns
  the expected records.
- The `output.log` limit is documented in
  `docs/architecture/02-session-storage.md`.
