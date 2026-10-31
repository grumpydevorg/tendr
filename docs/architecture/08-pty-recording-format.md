# PTY Recording Format, Version 1

This file defines the byte format of a PTY recording. The `tendr::recording`
rustdoc describes the Rust API and does not repeat the layout. The golden
fixture `tests/fixtures/recording/v1-segment0.hex` guards compatibility. A
change that alters its bytes is a format change and needs a new version
number.

A recording is the exact, ordered record of what the sidecar observed and
applied on one run's PTY: output bytes, geometry, and, if input recording is
on, accepted input bytes. It is separate from `output.log`, which is a lossy
readable transcript, and from the event log. Why both exist:
[../design.md](../design.md#outputlog-is-a-line-log-the-pty-recording-is-the-exact-record).

## Storage

The sidecar stores each run's recording in `<session>/recording/<run_id>/`
as `seg-<index:08>.tndrrec`, one file per segment (`src/recorder.rs`).

- On Unix the directories are `0700` and segment files `0600`.
- Each segment is first written as a temporary file holding its header. It
  is synced, then renamed into place, before any record is appended.
- The sidecar starts a new segment at 64 MiB and stops recording at 1 GiB
  in total ([04-pty-lane.md](04-pty-lane.md#recording)).
- The sidecar never records input, so `input_recorded` is always 0 in its
  segments. The format still defines `INPUT` records.

## Conventions

- All integers are **unsigned, big-endian**. Offsets and sizes are in bytes.
- **CRC** means CRC-32C (Castagnoli): reflected polynomial `0x82F63B78`, initial
  value `0xFFFFFFFF`, final XOR `0xFFFFFFFF`. Check value: `"123456789"` →
  `0xE3069283`. Implementations must also pass the RFC 3720 appendix B.4 vectors.
- A **recording** is an ordered list of **segments** for one run, numbered from 0.

## Segment header

| Offset | Size | Field | Rule |
|---:|---:|---|---|
| 0 | 8 | `magic` | `54 4E 44 52 52 45 43 00` (`"TNDRREC\0"`) |
| 8 | 2 | `version` | `1` |
| 10 | 2 | `header_len` | Total header size including `header_crc`. Must equal `60 + term_len`, so `60..=316` |
| 12 | 2 | `flags` | Bit 0 = `input_recorded`. Bits 1–15 must be zero |
| 14 | 16 | `run_id` | UUID bytes in RFC 9562 order; must be version 7 |
| 30 | 4 | `segment_index` | 0-based position in the recording |
| 34 | 8 | `first_sequence` | Sequence of this segment's first record; `>= 1`; segment 0 ⇒ `1` |
| 42 | 8 | `origin_unix_ns` | Wall clock (Unix epoch, ns) when the run's recording started; identical in every segment |
| 50 | 2 | `rows` | Geometry in effect at segment start; `>= 1` |
| 52 | 2 | `cols` | Geometry in effect at segment start; `>= 1` |
| 54 | 2 | `term_len` | `0..=256` |
| 56 | `term_len` | `term` | The child's `TERM`; every byte in `0x21..=0x7E` |
| 56 + `term_len` | 4 | `header_crc` | CRC over bytes `[0, 56 + term_len)` |

`header_crc` covers every preceding header byte, including `magic`, `version`,
and both framing lengths (`header_len`, `term_len`).

A segment is published only after its complete header is written and synced, so
**an incomplete header is corruption, never a truncated tail**.

### Header decoding order

1. Fewer than 60 bytes available → `Corrupt(HeaderIncomplete)`.
2. `magic` mismatch → `BadMagic`.
3. `version != 1` → `UnsupportedVersion`. Nothing after the version is
   interpreted for an unknown version.
4. `header_len` outside `60..=316` → `Corrupt(HeaderLength)`.
5. Fewer than `header_len` bytes available → `Corrupt(HeaderIncomplete)`.
6. `header_crc` mismatch → `Corrupt(HeaderChecksum)`.
7. `term_len != header_len - 60` → `Invalid(HeaderLengthMismatch)`.
8. Semantic rules (flags, UUID version, `first_sequence`, geometry, `term` bytes)
   → `Invalid(...)`.

## Record framing

Records follow the header immediately and continue to end of file.

| Offset | Size | Field | Rule |
|---:|---:|---|---|
| 0 | 4 | `body_len` | Body size; `17..=65553` (17 + max payload) |
| 4 | 4 | `body_len_check` | `body_len XOR 0xFFFFFFFF` |
| 8 | `body_len` | `body` | See below |
| 8 + `body_len` | 4 | `record_crc` | CRC over bytes `[0, 8 + body_len)`: both length words and the body |

`body_len_check` protects the framing length **before** the reader trusts it, so a
damaged length is reported as corruption instead of being mistaken for an
incomplete tail. `record_crc` additionally covers both length words.

### Body

| Offset | Size | Field | Rule |
|---:|---:|---|---|
| 0 | 1 | `kind` | See record kinds |
| 1 | 8 | `sequence` | Contiguous; see sequence rules |
| 9 | 8 | `elapsed_ns` | Nanoseconds since `origin_unix_ns`, from a monotonic clock; non-decreasing |
| 17 | `body_len - 17` | `payload` | Kind-specific |

### Record kinds

| `kind` | Name | Payload |
|---:|---|---|
| `0x01` | `OUTPUT` | `1..=65536` exact PTY output bytes. No encoding is implied; invalid UTF-8 and split escape sequences are preserved |
| `0x02` | `RESIZE` | Exactly 9 bytes: `rows` u16 `>= 1`, `cols` u16 `>= 1`, `cause` u8, `correlation` u32 |
| `0x03` | `INPUT` | `1..=65536` accepted input bytes. Allowed only when `input_recorded` is set |

Any other `kind` → `Invalid(UnknownKind)`.

`RESIZE.cause` is one of:

- `0x01` = `USER`: a client resize. `correlation` must be `0`.
- `0x02` = `REPAINT`: a repaint nudge, which shrinks the window and restores it.
  `correlation` must be nonzero, and both records of the pair share it.

Any other cause → `Invalid(UnknownResizeCause)`. A `RESIZE` records a geometry
that the owner actually applied. Repaint records are tagged, never omitted.

## Record decoding and end of segment

At each record position with `R` bytes remaining:

1. `R == 0` → **clean end**. A segment ending exactly at a record boundary is
   successful completion, not truncation.
2. `R < 8` → **truncated tail** (incomplete length prefix).
3. `body_len_check != body_len XOR 0xFFFFFFFF` → `Corrupt(LengthCheck)`.
4. `body_len` outside `17..=65553` → `Corrupt(LengthBounds)`.
5. `R < 8 + body_len + 4` → **truncated tail** (incomplete record).
6. `record_crc` mismatch → `Corrupt(RecordChecksum)`. This applies to the final
   record too: **a complete record with a bad checksum is corruption.** Only
   missing bytes make a truncated tail.
7. Semantic rules (kind, payload size, geometry, cause/correlation, input policy,
   sequence, elapsed) → `Invalid(...)`.

A truncated tail ends decoding. The records before it are valid; the tail is
reported with its byte offset and the last valid sequence, and is excluded.
`Corrupt` and `Invalid` are errors reported with the byte offset and the last
valid sequence; tooling may salvage the valid prefix only explicitly.

## Sequence, time, and geometry rules

Within a segment:

- The first record's `sequence` equals `first_sequence`.
- Each later record's `sequence` is exactly the previous plus one. A gap,
  duplicate, regression, or `u64` overflow → `Invalid(Sequence)`.
- `elapsed_ns` never decreases; equal values are allowed.
- The geometry in effect starts as the header `rows`/`cols` and becomes each
  `RESIZE` record's `rows`/`cols` in order.

Across the segments of one recording:

- `segment_index` runs `0, 1, 2, …` with no gaps.
- `run_id`, `origin_unix_ns`, `flags`, and `term` equal segment 0's values.
- `first_sequence` equals the previous segment's last `sequence` plus one.
- The first record's `elapsed_ns` is at least the previous segment's last
  `elapsed_ns`.
- The header `rows`/`cols` equal the geometry in effect at the end of the
  previous segment.
- A segment with no records is valid only as the final segment.
- Only the final segment may end in a truncated tail. A truncated non-final
  segment → `Corrupt(TruncatedInteriorSegment)`.

Violations → `Invalid(...)` unless stated otherwise.

**Check continuity before trusting a segment's records.** A decoder checks two
things before it accepts any record from a segment:

- the segment's header continuity (index, identity fields, `first_sequence`,
  geometry), right after the header decodes;
- elapsed-time continuity, on the segment's first record.

So the last valid sequence reported with a later error never includes a record
from a segment that breaks continuity, even if corruption follows in the same
segment.

## Golden fixture

`tests/fixtures/recording/v1-segment0.hex` is annotated hex. Lines starting with
`#` are comments. It holds segment 0 of a run with `TERM=xterm-ghostty`, 80×24
and input recording off. It has four records:

1. an `OUTPUT` ending in a split escape sequence;
2. an `OUTPUT` containing invalid UTF-8;
3. a `USER` resize to 100×30;
4. a `REPAINT` resize to 100×29 with an equal timestamp.

Its checksums come from an independent bitwise CRC-32C implementation, not from
the crate under test. Decoding the fixture must yield exactly those records.
Encoding them must reproduce the fixture byte for byte.
