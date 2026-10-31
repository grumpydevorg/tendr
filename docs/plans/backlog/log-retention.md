---
id: log-retention
depends_on: []
---

# Log retention

**Status:** not started. Needs a measurement first: how large do the logs of a
long-running session get in practice?

Three files grow without limit while a session exists, and one grows for
ever:

| File | Grows until | Today |
|---|---|---|
| `output.log` | the session is pruned or replaced | no rotation and no size limit |
| `events/` segment | the session is pruned or replaced | writers append to the newest segment and create one only when `events/` is empty, so a session has one segment |
| `recording/<run_id>/` | 1 GiB per run | split into 64 MiB segments; recording stops at 1 GiB and the session keeps running |
| `~/.tendr/lost+found/events.jsonl` | never removed | `prune` does not touch it |

The event-protocol design (in Git at `95dcf1d`,
`docs/plans/specs/event-protocol.md`) described rotation of event segments at
64 MiB, with a `segment.opened` event, and a `prune` that sweeps lost+found.
Neither was built. `segment.opened` is only a reserved event-kind prefix.

## Goal

A session that runs for weeks keeps bounded disk use, and nothing under
`~/.tendr` grows for ever.

## Open questions

- Measure first: the size of `output.log` and `events/` for real long-running
  sessions, for example a dev server or a build host over a week.
- `output.log`: rotate by size, keep the last N files, or cap and drop the
  oldest lines? `tendr log --since` and `--tail` must keep working across
  rotated files.
- Event segments: rotate at a size and keep cursors (`docs/architecture/07-event-protocol.md`
  §5.2) valid across segments. Cursors already name segments by file, which
  is why segments are never renamed.
- lost+found: should `prune --older-than` remove old lost+found lines?

## Acceptance

- A test writes past the size limit and shows that `tendr log`, `tendr events`
  and cursors still return the expected records.
- The limits are documented in `docs/architecture/02-session-storage.md`.
