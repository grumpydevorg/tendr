---
id: log-retention
depends_on: []
---

# Log retention

**Status:** not started. Starts when a long-running session's logs use enough
disk to matter. Needs a measurement first: how large do those logs get in
practice?

Two parts of a session grow without limit while it exists, and two files under
`~/.tendr` are never removed. `--replace` and `prune` delete the whole session
directory (`src/commands/start.rs:219`, `src/commands/prune.rs:159`).

| File | Limit today |
|---|---|
| `output.log` | none; no rotation and no size cap |
| `events/*.jsonl` and `events/blobs/` | none; writers append to the newest segment and create one only when `events/` has none (`open_segment`, `src/events.rs:296`), so a session has one segment |
| `recording/<run_id>/` | bounded: 64 MiB segments, recording stops at 1 GiB and the session keeps running (`src/recorder.rs:32-34`) |
| `~/.tendr/lost+found/events.jsonl` | never removed; written by `emit` and by the sidecar when an event has no session dir or log to go to |
| `~/.tendr/callbacks/<run_id>.json` | never removed; one file per run with `--on-exit` (`src/sidecar.rs:1361-1376`) |

The event-protocol spec (in Git at `95dcf1d`,
`docs/plans/specs/event-protocol.md`) described event segments rotating at
64 MiB with a `segment.opened` event, and a `prune` that sweeps lost+found.
Neither was built: `segment.` is a reserved kind prefix
(`src/model/event.rs:28`) and nothing writes `segment.opened`.

## Goal

A session that runs for weeks keeps bounded disk use, and nothing under
`~/.tendr` grows for ever.

## Open questions

- Measure first: the size of `output.log` and `events/` for real long-running
  sessions, for example a dev server or a build host over a week.
- `output.log`: rotate by size and keep the last N files, or cap and drop the
  oldest lines? `tendr log --since` and `--tail` must keep working across
  rotated files.
- Event segments: rotate at a size and keep cursors
  ([event protocol](../../architecture/07-event-protocol.md) §5.2) valid. Cursors
  name segments by file, which is why segments are never renamed.
- lost+found and callbacks: should `prune --older-than` remove old entries?

## Acceptance

- A test writes past the size limit and shows that `tendr log`, `tendr events`
  and cursors still return the expected records.
- The limits are documented in `docs/architecture/02-session-storage.md`.
