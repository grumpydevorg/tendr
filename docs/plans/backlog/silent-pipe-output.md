---
id: silent-pipe-output
depends_on: []
---

# Silent pipe output: CR framing and partial-line status

**Status:** not started. Ready, except for the open questions below. Change 1
(the reader) can ship without change 2 (status).

A pipe session whose child writes progress with carriage returns shows nothing
in `output.log` until the child writes a newline or exits. The operator cannot
tell that from a hung child. Give the pipe reader an optional CR delimiter and
bounded records, and make `tendr status` report bytes waiting in the reader.

## Evidence

On 2026-07-27 a 251 GiB `rsync --info=progress2,stats1` ran about two hours in a
pipe session (tender 0.2.1, rsync 3.4.4). 63 minutes in, while it moved about
33 MB/s, `output.log` was 0 bytes with its creation mtime. When the child closed
stdout the file went to 18,010,442 bytes in 4 records; the largest record was
17,773,603 characters, about 296,000 progress updates.

| Setup | Bytes in log mid-run |
|---|---|
| pipe, `--info=progress2` | 0 at 25 s |
| pipe, `--info=progress2 --outbuf=N` | 0 at 25 s |
| pipe, `--info=name2,stats2` | 143 records, streaming |
| `--pty`, `--info=progress2` | 3,863 at 20 s |

`--outbuf` changing nothing shows the delay is in Tendr's reader, not the child.

## Cause

`capture_stream` (`src/sidecar.rs:1491`) reads with `BufReader::lines()`, which
yields only on `\n`. rsync ends each update with `\r`, so the whole run of
updates collects in one `String`. This is the documented contract
([design](../../design.md#outputlog-is-a-line-log-the-pty-recording-is-the-exact-record)),
but it has three costs:

- The operator sees nothing until a newline.
- The `String` has no size limit. A multi-day job can exhaust the sidecar's
  memory.
- The record that finally lands is too large for `--tail`, `--grep` or a JSON
  reader.

A related fault in the same loop: `lines()` also fails on a line that is not
valid UTF-8, and `Err(_) => break` (`src/sidecar.rs:1500`) treats that as a
closed pipe. One bad byte ends capture for the rest of the run. The new reader
should decode lossily, as the PTY path already does
(`capture_stream_with_tee`, `String::from_utf8_lossy`).

`--pty` avoids the problem but merges stdout and stderr and makes the child see
a terminal. On Windows there is no PTY (`src/platform/windows.rs:183`), so no
workaround exists there.

## Change 1: a byte reader with bounded records and optional CR framing

- **Bounded records in every mode.** The reader emits a record when it reaches a
  documented size cap, marked as a fragment. Nothing is dropped or truncated;
  the content continues in the next record. This applies to the default mode
  too, since that is the one an unaware user gets.
- **`tendr start --log-delimiter newline|newline-or-cr`**, default `newline`. A
  `LogDelimiter` field on `LaunchSpec` with
  `#[serde(default, skip_serializing_if = …)]`, so a default spec serialises as
  today and `canonical_hash()` (`src/model/spec.rs:118`) does not move. The
  `boundary` field (`src/model/spec.rs:78-82`) is the precedent.
- **Emit on CR at once.** Do not hold a CR to see whether LF follows; that
  delays the update until the next write. Emit, set `last_was_cr`, and swallow
  a following LF even when it arrives in the next read. Any other byte clears
  the flag. CRLF stays one record.
- **Record how each record ended.** `LogLine` (`src/log.rs:19-26`) has no
  framing field. Add an optional `terminator`: `cr`, `eof` or `fragment`;
  absent means LF or an old record, so LF records stay byte-identical.
- **Collapse is a read-time view.** Every update is stored. Historical
  `tendr log`, `log --tail N` (collapse first, then take N) and non-following
  `events --include-logs` keep only the last record of each run of `cr` records
  within one stream tag. `--all-updates` turns this off. Following readers
  (`log -f`, `watch --logs`, `events --follow`) show every update, because they
  cannot know which update is last. `fragment` records never collapse.

PTY capture is unchanged; it already writes per read chunk.

## Change 2: `tendr status` explains an empty log

Report two values per pipe session:

- `pending_partial_bytes`: bytes read since the last record was written;
- `last_record_age_s`: time since the last record, or since start.

400 KB pending with no record for 20 minutes then explains itself. A hung child
and a buffering child have the same stale `output.log` mtime; only the pending
count separates them, and it exists only in the sidecar's reader thread.
`tendr status` is a separate process that reads `meta.json`
(`src/commands/status.rs:27`), so the sidecar must publish the values. Do not
put them in `meta.json`: it is the lifecycle record, written on state changes,
and frequent rewrites would bury those changes. The likely carrier is a small
sidecar-owned file rewritten at a bounded rate, read best-effort by `status`.

## Open questions

- The record size cap, with a stated reason.
- Change 2: the telemetry file name, format and write rate, and how `status`
  shows values from a dead sidecar as stale.
- Whether `newline-or-cr` should later become the default.

## Documentation

`docs/guide.md` does not say that the log holds a partial line until a newline.
Add that rule to "Observe long-running work", with the choice between
`--log-delimiter newline-or-cr`, a newline-framed mode of the child (for rsync,
`--info=name2,stats2`) and `--pty`. Add one line to `src/embedded/SKILL.md`:
check `tendr log <name>` within a minute of starting a long job.

One user's Claude Code install has a stopgap hook outside this repo
(`~/.claude/hooks/tendr-pty-guard.py`). It refuses a `tendr start` of rsync with
a progress meter and no `--pty`. It can go once `status` reports pending bytes
where agents look, or CR framing becomes the default. Its rsync rule must not
move into Tendr.

## Acceptance

- Under `newline-or-cr`, `10%\r` is in `output.log` before the child exits, and
  a CR at the end of a read is on disk without waiting for the next write.
- `\r\n` gives one record, also when split across reads; `\r` split across reads
  gives two; `\rX` does not swallow `X`.
- In both modes, an unterminated span longer than the cap gives `fragment`
  records that concatenate to the original bytes, and reader memory stays
  within the cap.
- Invalid UTF-8 from the child does not end capture.
- A default spec's `canonical_hash()` is unchanged; old `LogLine` records still
  parse; LF records carry no `terminator`.
- `log --tail N` over 10,000 CR updates and 3 real lines returns the collapsed
  tail; stderr lines between stdout CR updates do not split the run; `log -f`
  shows every update; `--all-updates` returns every record.
- Windows pipe capture honours the delimiter.
- Change 2: a child writing `10%\r` repeatedly shows rising
  `pending_partial_bytes` and growing `last_record_age_s`; a silent child shows
  0 pending with the same age; after exit both values stop changing.
