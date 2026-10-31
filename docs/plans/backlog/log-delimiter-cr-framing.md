---
id: log-delimiter-cr-framing
depends_on: []
---

# CR-aware pipe framing: live progress without a PTY

**Status:** not started. Ready.

Give pipe sessions a typed, generic way to split records on a lone carriage
return. A `\r`-framed progress meter then shows in `output.log` while the child
runs, without a PTY and without command-specific rules in Tendr.

## Why

A 251 GiB `rsync` ran for 75 minutes under a pipe session with
`--info=progress2,stats1` and a **0-byte `output.log`** the whole time. The
operator had no progress signal at all and fell back to measuring the destination
filesystem by hand.

The cause is `capture_stream` (`src/sidecar.rs`), which reads with
`BufReader::lines()` — that yields only on `\n`. `--info=progress2` terminates each
update with `\r`. The bytes arrive and the record does eventually land, but not
until a newline appears or the child exits.

**This is the documented contract, not a defect.** The log is a line-oriented
observability log, not a byte-exact replay stream, and it buffers partial lines
until a newline (see [design](../../design.md#outputlog-is-a-line-log-the-pty-recording-is-the-exact-record)). Nothing is lost —
the operator is blind. Those are different failures and the second one is the one
that hurt.

Measured on tender 0.2.1 / rsync 3.4.4:

| Setup | Bytes in log mid-run |
|---|---|
| pipe + `--info=progress2` | **0** at 25 s |
| pipe + `--info=progress2 --outbuf=N` | **0** at 25 s |
| pipe + `--info=name2,stats2` | 143 records, streaming |
| `--pty` + `--info=progress2` | 3,863 at 20 s |
| plain shell `> file` + `progress2` | 874 at 20 s |

`--outbuf=N` changing nothing is the tell: this is framing in the reader, not
buffering in the child.

### What the log looked like afterwards

The transfer finished cleanly (`ExitedOk`, 268,136,296,402 bytes sent, 36.8 MB/s,
~2h01m). At the moment the child closed stdout, `output.log` went from **0 bytes to
18,010,442 bytes**. Measured structure of that file:

```
records: 4
by tag:  {'O': 4}
largest single record: 17,773,603 chars
```

**Not thousands of records — one record of 17.8 MB.** `BufRead::lines()` yields on
`\n`, so a long CR-only span is accumulated into a *single* logical record and
emitted whole, interleaved with whatever genuine LF-delimited lines exist (here, the
three closing `stats1` lines). This has three consequences the plan must address:

1. **Unbounded in-memory growth.** That 17.8 MB String lived in the sidecar's reader
   thread for two hours, growing the whole time. Nothing bounds it. A longer job, or
   a chattier meter, grows it further — a multi-day transfer is an OOM waiting to
   happen. This is a latent robustness bug in the *current* implementation,
   independent of whether anyone wants CR framing, and **it is not fixed by adding
   an opt-in flag** — see "Bounded records in every mode" below.
2. **The record is unusable even after it lands.** A single 17.8 MB line is not
   something `--tail`, `--grep`, or a JSON consumer can work with. The information
   arrives and is still not legible.
3. **It sets the scale for collapse.** At ~60 chars per rsync progress update, 17.8
   MB is roughly **296,000 updates** — about 41/second. That is the flood
   `newline-or-cr` would produce without the derived-view collapse, and the concrete
   number behind the `--tail` argument below.

## Why not the alternatives

**`--pty`.** Works, and is the correct answer for interactive children. Wrong
default for a non-interactive batch job: it merges stdout with stderr, flips the
child's `isatty()` (inviting colour and control sequences into the durable log),
records a best-effort transcript split at arbitrary 4096-byte read boundaries, and
is **not implemented on Windows** (`src/platform/windows.rs:184` — "PTY not
supported on Windows yet"). A Windows user has no workaround at all today.

**Recognising programs.** Tendr must not learn that `rsync --info=progress2`
behaves one way and `curl` another. That list is unbounded, drifts with every tool
release, and puts policy in the wrong layer. The delimiter is the actual
abstraction.

**Telling users to pick newline-framed output.** Good advice (`--info=name2,stats2`
is genuinely better for a many-file transfer — countable as well as visible) but it
is not always available, and it does not help someone who already started the job.

## Design

A typed field on `LaunchSpec`, surfaced as a `start` flag:

```
--log-delimiter newline          # current behaviour, default
--log-delimiter newline-or-cr    # progress-meter mode
```

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum LogDelimiter {
    #[default]
    Newline,
    NewlineOrCr,
}
```

PTY capture is unaffected (already chunk-based).

### Bounded records in every mode

The memory bug is orthogonal to the framing choice and **must not be fixed by the
flag**. `newline` stays the default; a session that omits `--log-delimiter` still
accumulates an indefinitely growing String, and "you should have passed the flag" is
not a memory-safety story. The cap is a property of the reader, not of a mode.

Acceptance criteria:

- **Every framing mode has a maximum buffered record size.** `newline` included —
  especially `newline`, since it is what the unaware session gets.
- **Crossing the cap emits a durable record** carrying the bytes so far, tagged
  `terminator: Fragment`. Nothing is dropped and nothing is silently truncated; the
  span simply continues in the next record.
- **Collapse applies only to `Cr`.** A `Fragment` is real content that happened to
  be cut, not a superseded progress frame — collapsing it would destroy output.
- The cap is a documented number with a rationale, not an arbitrary constant, and
  is the same in every mode.

Under `newline-or-cr` the cap should almost never fire, because CR framing already
bounds records at one update. It exists for the child that emits neither delimiter
— which is exactly the case nothing currently protects against.

### Emit on CR immediately — never hold it

The obvious implementation is wrong: do **not** buffer a trailing CR while waiting
to see whether the next byte is LF. A child that writes one progress update and
then pauses would have that update held until its next write — recreating the exact
visibility delay this plan exists to remove, just with a shorter fuse.

The algorithm is emit-then-suppress:

- On CR, emit the record immediately.
- Set `last_was_cr`.
- If the next byte is LF, swallow it — including when it arrives in a *later read*,
  so the flag must survive across chunk boundaries.
- Any other byte clears the flag.

CRLF therefore still yields one record, with no empty second record and no byte
ever held back. Same outcome as lookahead, none of the latency.

Preserved: separate stdout/stderr, normal non-TTY child behaviour, `--host`
parity, Windows parity, zero command-specific policy.

### Constraint: spec hashing

`LaunchSpec::canonical_hash()` (`src/model/spec.rs:118-122`) SHA-256s the whole
serialized struct. The new field **must** carry
`#[serde(default, skip_serializing_if = "is_default")]` so specs that don't set it
serialize byte-identically to pre-feature specs and their hashes do not move.
`boundary` is the established precedent and carries a comment saying exactly this.
A regression test should assert hash stability for a default-valued spec.

### Framing provenance, and collapse as a derived view

A `\r` meter updates several times per second, so a two-hour transfer becomes tens
of thousands of near-identical records. Left raw, `tendr log --tail 50` returns
fifty renderings of the same progress line and buries any real stdout or stderr —
the command fails at exactly the moment you reach for it.

But collapse cannot be implemented on today's record. `LogLine { ts, tag, content }`
(`src/log.rs:19-26`) keeps no framing information: once written, nothing
distinguishes a record that ended in CR from one that ended in LF or at EOF. A
consumer cannot tell a progress frame from a real line, so it cannot safely collapse
anything.

So the record must carry provenance:

```rust
pub struct LogLine {
    pub ts: f64,
    pub tag: String,
    pub content: serde_json::Value,
    /// How this record was framed.
    ///
    /// `None` means LF-framed **or** written before this field existed — the two
    /// are deliberately conflated, because neither ever collapses and the
    /// distinction has no consumer. The rest are stored explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminator: Option<Terminator>,
}

pub enum Terminator {
    Cr,
    Eof,
    /// Not terminated by the child at all — cut by the size cap. See
    /// "Bounded records in every mode".
    Fragment,
}
```

There is no `Lf` variant: the default case is absence. That keeps every existing
LF-framed record byte-identical on the wire and makes "collapsible" exactly
`terminator == Some(Cr)` — which deliberately excludes `Fragment`, since a
size-forced cut carries real content that must never be discarded as a superseded
progress frame.

Preferred over a new `P` tag: the tag axis means *stream* (`O`/`E`/`A`), and framing
is an orthogonal property. Overloading it would make a CR-framed stderr record
unrepresentable.

**The durable record stays complete.** Every update is written. Collapse is a
*derived view* applied at read time.

#### Historical collapses; live does not

Collapse **cannot** behave identically on live and historical surfaces, and
pretending otherwise would be a lie in the spec. A historical reader has the whole
run in hand and can discard superseded CR records before emitting anything. A
follower cannot: it does not know which CR update will turn out to be the last, and
it cannot retract a record it has already emitted.

The rejected alternatives are worth recording:

- **Delay CR output until LF/EOF** to learn which one is last — recreates precisely
  the blindness this plan exists to remove.
- **Keyed replacement/update events**, so a follower can supersede a record it
  already sent — coherent, but a protocol change well beyond this plan.
- **Terminal overwrite** (re-render in place on `-f`) — conflicts with
  machine-readable pipes and crosses Tendr's "not a renderer" boundary. Tendr
  owns the process and the record; screens belong to Boo.

Accept the honest asymmetry:

| Surface | Behaviour | Opt-out |
|---|---|---|
| `tendr log` (historical) | collapse each CR run to its last record | `--all-updates` |
| `tendr log --tail N` | collapse **first**, then take the last N | `--all-updates` |
| event replay (`events --include-logs`, not following) | collapse | `--all-updates` |
| `tendr log -f` | every update, no collapse — cannot know the last | n/a |
| `watch --logs` (`src/commands/watch.rs:196`) | every update, no collapse | n/a |
| followed events | every update, no collapse | n/a |

`--tail N` ordering matters: N records *after* collapsing, or the tail is still N
renderings of one progress line.

#### CR runs are per stream tag

A run of CR records is defined **within a tag**, not across the log. A child writing
a `\r` meter on stdout while stderr emits an unrelated line must not have the stderr
record terminate the stdout run — otherwise interleaving silently defeats collapse,
and the more the child says on stderr the worse the stdout view gets.

### The flag alone does not prevent recurrence

Worth stating plainly: `newline` stays the default, so anyone who doesn't already
know the rule starts the same session, sees the same empty log, and learns nothing.
This plan makes the fix *possible*; it does not make the failure *self-evident*.

Closing that loop needs a signal rather than a flag — buffered-byte and
last-record-age telemetry, generic enough to catch a wedged child too. That is a
separate feature with a separate problem (the sidecar and the CLI are different
processes, so an in-memory counter is invisible to `tendr status`), and it is
tracked as [partial-line-telemetry](partial-line-telemetry.md). Neither blocks the
other; shipping both is what actually closes the loop.

## Tests

Framing:

- `10%\r` appears in `output.log` **before** the child exits, under
  `newline-or-cr`.
- **A CR at the very end of a read is emitted on that read**, not held. Write
  `10%\r`, pause past any plausible buffering window, assert the record is already
  on disk. This is the test that pins the emit-then-suppress rule; a lookahead
  implementation fails it.
- `\r\n` produces one record, not a record plus an empty one.
- `\r\n` **split across reads** (chunk ends `...10%\r`, next begins `\n20%\r`)
  produces two records — `last_was_cr` survived the boundary and swallowed the LF —
  not three, and not one.
- A lone `\r` split across a read boundary (chunk ends `...10%\r`, next begins
  `20%\r`) produces two records.
- A CR followed by any non-LF byte clears the flag: `\rX` is not swallowed.

Bounded records — **these are the desired assertions**, in every mode:

- Under `newline`, a CR-only span exceeding the cap yields multiple records, each
  `terminator: Fragment`, whose contents concatenate back to the original bytes
  exactly. Nothing dropped, nothing truncated.
- The same holds under `newline-or-cr` for a child emitting neither delimiter.
- Peak reader memory for an arbitrarily long unterminated span stays bounded by the
  cap — assert the bound, not merely that output appears.
- A `Fragment` record is **never** collapsed, on any surface, including within a run
  of `Cr` records.

> **Characterization only, not a contract.** The observed 2026-07-27 behaviour — a
> 17.8 MB span arriving as a *single* record — describes the implementation that had
> the bug. Do not write it as a permanent assertion; the bounded-record criteria
> above deliberately change it. It is recorded here so the change is understood as
> intentional rather than a regression someone later "fixes" back.

Framing, under `newline`:

- Embedded `\r` bytes within a record are preserved, not split on.
- CR-only output interleaved with genuine LF lines produces the LF records
  separately.

Compatibility:

- Default (`newline`) behaviour is byte-identical to today — regression.
- `canonical_hash()` of a default-valued spec is unchanged from pre-feature.
- A `LogLine` written before `terminator` existed still deserializes.
- LF-framed records omit `terminator` on the wire (no log-size regression for
  every existing session).

Projections:

- Historical `log` and non-following `events --include-logs` render identically for
  the same session.
- `--tail N` over a session of 10,000 CR updates plus 3 real lines returns the
  collapsed tail, not 
  N frames of the meter — i.e. collapse runs before the tail is taken.
- `log -f` over the same session emits **every** update; asserting parity with the
  historical rendering is wrong and the test should say so.
- A session interleaving stdout CR updates with stderr lines collapses the stdout
  run intact — the stderr records do not split it.
- `--all-updates` on the historical surfaces returns every CR record.

Platform:

- Windows pipe capture honours the delimiter (the platform with no PTY escape).

## Documentation

The failure was reachable only by reading the architecture doc or the source.
`tendr guide` has **zero** mentions of PTY, TTY, or progress framing; so does
`README.md`; so does the embedded `using-tendr` skill. `--pty` appears only in
`tendr start --help`, described as "Interactive pseudo-terminal mode" — which
reads as *for humans*, not *required to see progress*.

- Add a `tendr guide pty` topic: the two lanes, what each captures, and how to
  choose for a progress-emitting child.
- Add the partial-line rule to `docs/guide.md` where long-running work is
  discussed — the log holds partial lines until a delimiter.
- Add one line to `src/embedded/SKILL.md`: check `tendr log <name>`
  within 60 s of starting a long session; an empty log means you are blind.

## Rollout

There is a stopgap outside this repo, in one user's Claude Code install
(`~/.claude/hooks/tender-pty-guard.py`, a `PreToolUse`/`Bash` hook). Recorded here
because Tendr's own docs are where someone will look for it, and the file itself
is not visible from this repo:

- **Denies** a Bash call whose command segment starts a tendr session
  (`tendr` … `start`), requests an rsync `\r` meter (`--progress` or
  `--info=*progress*`), and has no `--pty`.
- **Segment-scoped**, not whole-command: `--pty` belonging to a different command in
  a compound line does not count. It is a matcher, not a shell parser — quoting,
  nesting and here-docs can fool it either way.
- **Measured cases only.** rsync alone. curl, wget, pip, dd, pv and docker pull are
  suspected to behave the same but were never measured, and several change
  behaviour off a TTY, so they are deliberately out of scope.
- **Escape hatch:** `# pty-guard: ok` in the command.
- The denial message explains the framing cause and offers both fixes
  (`--info=name2,stats2` first, `--pty` second).

Scope is narrow and worth stating plainly: it covers Claude Code tool calls on one
machine and nothing else — not a human at a terminal, not other agents, not scripts
or cron, not `--host` starts issued any other way.

**Shipping this plan is not sufficient to remove it.** The two solve different
problems:

- This plan solves **capability** — with `--log-delimiter newline-or-cr` there is
  now a correct thing to do.
- The hook solves **omission** — it catches the case where nobody does it.

`newline` stays the default, so after this ships an agent can still start exactly
the session that caused the incident, still get an empty log, and still learn
nothing. An opt-in mechanism does not retire an enforcement mechanism.

Remove the hook when **any one** of these is true:

1. CR-aware framing becomes the default for new sessions, so omission is no longer
   possible.
2. Tendr detects the condition itself and surfaces it reliably — e.g.
   [partial-line-telemetry](partial-line-telemetry.md) landing somewhere an operator
   or agent actually reads, not merely being queryable.
3. A cross-agent enforcement mechanism replaces it — something every caller
   resolves, not one agent's config.

Until then the narrow rsync hook stays alongside the documented flag. Its pattern
list must not migrate into Tendr in any case: per-command knowledge belongs in
neither.
