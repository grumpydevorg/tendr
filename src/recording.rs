//! Exact PTY recording codec, format version 1.
//!
//! A recording preserves exactly what a PTY owner observed and applied for one
//! run: output bytes, applied geometry, and — only when enabled at launch —
//! accepted input bytes. It is not `output.log`, which is a lossy readable
//! transcript.
//!
//! **The wire format is specified in
//! [`docs/architecture/08-pty-recording-format.md`](https://github.com/grumpydevorg/tendr/blob/main/docs/architecture/08-pty-recording-format.md).**
//! That document is the authority for byte layout, checksums, and decoding
//! order; this module does not restate it. The golden fixture
//! `tests/fixtures/recording/v1-segment0.hex` guards compatibility.
//!
//! # API
//!
//! - [`SegmentEncoder`] writes one segment and enforces the format's sequence,
//!   time, payload, and input-policy rules, so a writer cannot emit a segment the
//!   decoder would reject. [`SegmentEncoder::next_segment`] rotates to the next
//!   segment with the continuity fields filled in.
//! - [`decode_segment`] decodes one segment. A segment that ends exactly at a
//!   record boundary ends [`SegmentEnd::Clean`]; one whose final record is missing
//!   bytes ends [`SegmentEnd::Truncated`] and excludes that record. A complete
//!   record with a bad checksum is always an error, never a truncated tail.
//! - [`decode_recording`] decodes an ordered list of segments and enforces the
//!   cross-segment rules.
//! - [`dump`] writes a decoded recording as plain text, one line per record,
//!   and flags the terminal queries and mode changes in its output. It is a
//!   development aid (`cargo run --example dump-recording`), not a stable
//!   format.

use std::num::{NonZeroU16, NonZeroU32, NonZeroU64};

use thiserror::Error;

use crate::model::ids::RunId;

mod dump;

pub use dump::dump;

/// The format version this module reads and writes.
pub const FORMAT_VERSION: u16 = 1;
/// Maximum `OUTPUT`/`INPUT` payload size in bytes.
pub const MAX_PAYLOAD: usize = 65_536;
/// Maximum `TERM` length in bytes.
pub const MAX_TERM_LEN: usize = 256;

// Layout constants. The spec is the authority; these mirror it.
const MAGIC: [u8; 8] = *b"TNDRREC\0";
const HEADER_FIXED_LEN: usize = 56;
const CRC_LEN: usize = 4;
const HEADER_MIN_LEN: usize = HEADER_FIXED_LEN + CRC_LEN;
const HEADER_MAX_LEN: usize = HEADER_MIN_LEN + MAX_TERM_LEN;
const FLAG_INPUT_RECORDED: u16 = 1;
const PREFIX_LEN: usize = 8;
const BODY_FIXED_LEN: usize = 17;
const BODY_MAX_LEN: usize = BODY_FIXED_LEN + MAX_PAYLOAD;
const RESIZE_PAYLOAD_LEN: usize = 9;
const KIND_OUTPUT: u8 = 0x01;
const KIND_RESIZE: u8 = 0x02;
const KIND_INPUT: u8 = 0x03;
const CAUSE_USER: u8 = 0x01;
const CAUSE_REPAINT: u8 = 0x02;

/// Terminal dimensions; both are at least one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Geometry {
    rows: NonZeroU16,
    cols: NonZeroU16,
}

impl Geometry {
    /// `None` if either dimension is zero.
    #[must_use]
    pub fn new(rows: u16, cols: u16) -> Option<Self> {
        Some(Self {
            rows: NonZeroU16::new(rows)?,
            cols: NonZeroU16::new(cols)?,
        })
    }

    #[must_use]
    pub fn rows(self) -> u16 {
        self.rows.get()
    }

    #[must_use]
    pub fn cols(self) -> u16 {
        self.cols.get()
    }
}

/// A record's position in the run. Starts at one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sequence(NonZeroU64);

impl Sequence {
    /// The first sequence of every run.
    pub const FIRST: Self = Self(NonZeroU64::MIN);

    /// `None` for zero.
    #[must_use]
    pub fn new(value: u64) -> Option<Self> {
        NonZeroU64::new(value).map(Self)
    }

    #[must_use]
    pub fn get(self) -> u64 {
        self.0.get()
    }

    /// The following sequence; `None` at `u64::MAX`.
    #[must_use]
    pub fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

/// The child's `TERM`: at most [`MAX_TERM_LEN`] bytes, each in `0x21..=0x7E`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TermName(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TermNameError {
    #[error("TERM is longer than {MAX_TERM_LEN} bytes")]
    TooLong,
    #[error("TERM contains a byte outside 0x21..=0x7E")]
    InvalidByte,
}

impl TermName {
    /// # Errors
    ///
    /// [`TermNameError`] if the value breaks the length or byte rules.
    pub fn new(value: impl Into<String>) -> Result<Self, TermNameError> {
        let value = value.into();
        if value.len() > MAX_TERM_LEN {
            return Err(TermNameError::TooLong);
        }
        if !value.bytes().all(|b| (0x21..=0x7E).contains(&b)) {
            return Err(TermNameError::InvalidByte);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a geometry change was applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResizeCause {
    /// A client resize.
    User,
    /// A repaint nudge; both records of a shrink/restore pair share `correlation`.
    Repaint { correlation: NonZeroU32 },
}

/// A record's content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordKind {
    /// Exact PTY output bytes.
    Output(Vec<u8>),
    /// Geometry the owner applied.
    Resize {
        geometry: Geometry,
        cause: ResizeCause,
    },
    /// Accepted input bytes (only in segments with input recording enabled).
    Input(Vec<u8>),
}

/// One recorded observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub sequence: Sequence,
    /// Nanoseconds since the recording origin, from a monotonic clock.
    pub elapsed_ns: u64,
    pub kind: RecordKind,
}

/// Segment header fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentHeader {
    pub run_id: RunId,
    pub segment_index: u32,
    pub first_sequence: Sequence,
    /// Wall clock (Unix epoch, ns) when the run's recording started.
    pub origin_unix_ns: u64,
    /// Geometry in effect at segment start.
    pub geometry: Geometry,
    pub input_recorded: bool,
    pub term: TermName,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum EncodeError {
    #[error("segment 0 must start at sequence 1")]
    FirstSequence,
    #[error("expected sequence {expected}, got {got}")]
    Sequence { expected: u64, got: u64 },
    #[error("sequence space exhausted")]
    SequenceExhausted,
    #[error("elapsed time went backwards: {previous} ns then {got} ns")]
    ElapsedRegressed { previous: u64, got: u64 },
    #[error("record payload is empty")]
    EmptyPayload,
    #[error("record payload is {len} bytes; the maximum is {MAX_PAYLOAD}")]
    PayloadTooLarge { len: usize },
    #[error("input record in a segment without input recording")]
    InputNotRecorded,
    #[error("cannot rotate away from a segment with no records")]
    EmptySegmentRotation,
    #[error("segment index space exhausted")]
    SegmentIndexExhausted,
}

/// Writes one segment, enforcing the format rules on every record.
#[derive(Debug, Clone)]
pub struct SegmentEncoder {
    header: SegmentHeader,
    next_sequence: Option<Sequence>,
    last_elapsed_ns: Option<u64>,
    geometry: Geometry,
    records: u64,
}

impl SegmentEncoder {
    /// # Errors
    ///
    /// [`EncodeError::FirstSequence`] if segment 0 does not start at sequence 1.
    pub fn new(header: SegmentHeader) -> Result<Self, EncodeError> {
        if header.segment_index == 0 && header.first_sequence != Sequence::FIRST {
            return Err(EncodeError::FirstSequence);
        }
        Ok(Self {
            next_sequence: Some(header.first_sequence),
            last_elapsed_ns: None,
            geometry: header.geometry,
            records: 0,
            header,
        })
    }

    #[must_use]
    pub fn header(&self) -> &SegmentHeader {
        &self.header
    }

    /// The encoded header, including its checksum.
    #[must_use]
    pub fn header_bytes(&self) -> Vec<u8> {
        let h = &self.header;
        let term = h.term.as_str().as_bytes();
        let term_len = u16::try_from(term.len()).expect("TermName is at most 256 bytes");
        let header_len =
            u16::try_from(HEADER_MIN_LEN + term.len()).expect("header is at most 316 bytes");
        let flags = if h.input_recorded {
            FLAG_INPUT_RECORDED
        } else {
            0
        };

        let mut out = Vec::with_capacity(usize::from(header_len));
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_be_bytes());
        out.extend_from_slice(&header_len.to_be_bytes());
        out.extend_from_slice(&flags.to_be_bytes());
        out.extend_from_slice(h.run_id.as_uuid().as_bytes());
        out.extend_from_slice(&h.segment_index.to_be_bytes());
        out.extend_from_slice(&h.first_sequence.get().to_be_bytes());
        out.extend_from_slice(&h.origin_unix_ns.to_be_bytes());
        out.extend_from_slice(&h.geometry.rows().to_be_bytes());
        out.extend_from_slice(&h.geometry.cols().to_be_bytes());
        out.extend_from_slice(&term_len.to_be_bytes());
        out.extend_from_slice(term);
        let crc = crc32c::crc32c(&out);
        out.extend_from_slice(&crc.to_be_bytes());
        out
    }

    /// Validate `record` against the segment so far and encode it.
    ///
    /// # Errors
    ///
    /// [`EncodeError`] for a sequence, time, payload, or input-policy violation;
    /// the encoder state is then unchanged.
    pub fn encode(&mut self, record: &Record) -> Result<Vec<u8>, EncodeError> {
        let expected = self.next_sequence.ok_or(EncodeError::SequenceExhausted)?;
        if record.sequence != expected {
            return Err(EncodeError::Sequence {
                expected: expected.get(),
                got: record.sequence.get(),
            });
        }
        if let Some(previous) = self.last_elapsed_ns {
            if record.elapsed_ns < previous {
                return Err(EncodeError::ElapsedRegressed {
                    previous,
                    got: record.elapsed_ns,
                });
            }
        }

        let (kind, payload) = match &record.kind {
            RecordKind::Output(bytes) => (KIND_OUTPUT, checked_payload(bytes)?.to_vec()),
            RecordKind::Input(bytes) => {
                let bytes = checked_payload(bytes)?;
                if !self.header.input_recorded {
                    return Err(EncodeError::InputNotRecorded);
                }
                (KIND_INPUT, bytes.to_vec())
            }
            RecordKind::Resize { geometry, cause } => {
                let (cause, correlation) = match cause {
                    ResizeCause::User => (CAUSE_USER, 0),
                    ResizeCause::Repaint { correlation } => (CAUSE_REPAINT, correlation.get()),
                };
                let mut payload = Vec::with_capacity(RESIZE_PAYLOAD_LEN);
                payload.extend_from_slice(&geometry.rows().to_be_bytes());
                payload.extend_from_slice(&geometry.cols().to_be_bytes());
                payload.push(cause);
                payload.extend_from_slice(&correlation.to_be_bytes());
                (KIND_RESIZE, payload)
            }
        };

        let body_len = BODY_FIXED_LEN + payload.len();
        let body_len_word = u32::try_from(body_len).expect("body is at most 65553 bytes");
        let mut out = Vec::with_capacity(PREFIX_LEN + body_len + CRC_LEN);
        out.extend_from_slice(&body_len_word.to_be_bytes());
        out.extend_from_slice(&(!body_len_word).to_be_bytes());
        out.push(kind);
        out.extend_from_slice(&record.sequence.get().to_be_bytes());
        out.extend_from_slice(&record.elapsed_ns.to_be_bytes());
        out.extend_from_slice(&payload);
        let crc = crc32c::crc32c(&out);
        out.extend_from_slice(&crc.to_be_bytes());

        self.next_sequence = expected.next();
        self.last_elapsed_ns = Some(record.elapsed_ns);
        if let RecordKind::Resize { geometry, .. } = record.kind {
            self.geometry = geometry;
        }
        self.records += 1;
        Ok(out)
    }

    /// The geometry in effect after the records encoded so far.
    #[must_use]
    pub fn geometry(&self) -> Geometry {
        self.geometry
    }

    /// An encoder for the following segment, continuing sequence, time, and
    /// geometry.
    ///
    /// # Errors
    ///
    /// [`EncodeError::EmptySegmentRotation`] if this segment has no records;
    /// [`EncodeError::SequenceExhausted`] or
    /// [`EncodeError::SegmentIndexExhausted`] at the numeric limits.
    pub fn next_segment(&self) -> Result<Self, EncodeError> {
        if self.records == 0 {
            return Err(EncodeError::EmptySegmentRotation);
        }
        let first_sequence = self.next_sequence.ok_or(EncodeError::SequenceExhausted)?;
        let segment_index = self
            .header
            .segment_index
            .checked_add(1)
            .ok_or(EncodeError::SegmentIndexExhausted)?;
        let header = SegmentHeader {
            segment_index,
            first_sequence,
            geometry: self.geometry,
            ..self.header.clone()
        };
        Ok(Self {
            next_sequence: Some(first_sequence),
            last_elapsed_ns: self.last_elapsed_ns,
            geometry: self.geometry,
            records: 0,
            header,
        })
    }
}

/// An `OUTPUT`/`INPUT` payload within the size bounds.
fn checked_payload(bytes: &[u8]) -> Result<&[u8], EncodeError> {
    if bytes.is_empty() {
        Err(EncodeError::EmptyPayload)
    } else if bytes.len() > MAX_PAYLOAD {
        Err(EncodeError::PayloadTooLarge { len: bytes.len() })
    } else {
        Ok(bytes)
    }
}

/// How a decoded segment ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentEnd {
    /// The data ended exactly at a record boundary.
    Clean,
    /// The final record starting at byte `offset` was incomplete and is excluded.
    Truncated { offset: u64 },
}

/// A decoded segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedSegment {
    pub header: SegmentHeader,
    pub records: Vec<Record>,
    pub end: SegmentEnd,
}

/// A decoded recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedRecording {
    /// Segment 0's header.
    pub header: SegmentHeader,
    pub segment_count: u32,
    pub records: Vec<Record>,
    /// How the final segment ended.
    pub end: SegmentEnd,
}

/// Damage that checksums or framing checks detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Corruption {
    HeaderIncomplete,
    HeaderLength,
    HeaderChecksum,
    LengthCheck,
    LengthBounds,
    RecordChecksum,
    TruncatedInteriorSegment,
}

/// Checksummed data that breaks a format rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Violation {
    HeaderLengthMismatch,
    ReservedFlags,
    RunIdVersion,
    FirstSequence,
    ZeroGeometry,
    TermByte,
    UnknownKind(u8),
    PayloadSize,
    UnknownResizeCause(u8),
    Correlation,
    InputNotRecorded,
    Sequence,
    ElapsedRegressed,
    NoSegments,
    SegmentIndex,
    SegmentRun,
    SegmentOrigin,
    SegmentFlags,
    SegmentTerm,
    SegmentFirstSequence,
    SegmentGeometry,
    SegmentElapsed,
    EmptyInteriorSegment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeErrorKind {
    BadMagic,
    UnsupportedVersion(u16),
    Corrupt(Corruption),
    Invalid(Violation),
}

/// A decoding failure, located precisely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("recording {kind:?} at segment {segment:?} offset {offset}")]
pub struct DecodeError {
    pub kind: DecodeErrorKind,
    /// Position in the list given to [`decode_recording`]; `None` from
    /// [`decode_segment`].
    pub segment: Option<u32>,
    /// Byte offset within the segment of the header or record at fault.
    pub offset: u64,
    /// The last record that decoded validly before the failure.
    pub last_valid_sequence: Option<Sequence>,
}

/// Decode one segment.
///
/// # Errors
///
/// [`DecodeError`] for any corruption or rule violation.
pub fn decode_segment(bytes: &[u8]) -> Result<DecodedSegment, DecodeError> {
    decode_segment_with_summary(bytes, None).map(|(segment, _)| segment)
}

/// What cross-segment validation needs from a decoded segment.
#[derive(Debug, Clone, Copy)]
struct SegmentSummary {
    header_len: usize,
    /// The sequence the next record must carry; `None` after `u64::MAX`.
    next_sequence: Option<Sequence>,
    last_sequence: Option<Sequence>,
    last_elapsed_ns: Option<u64>,
    final_geometry: Geometry,
}

/// Where a segment sits in a recording and what it must continue from.
///
/// Continuity is checked against the header before any record is read, and
/// against the first record before it is accepted. A segment that breaks
/// continuity therefore contributes no records to a failure's
/// `last_valid_sequence`, whatever corruption follows it.
struct Continuity<'a> {
    position: u32,
    /// Segment 0's header and the previous segment's summary; `None` for the
    /// first segment.
    previous: Option<(&'a SegmentHeader, SegmentSummary)>,
}

fn check_header_continuity(header: &SegmentHeader, continuity: &Continuity) -> Option<Violation> {
    if header.segment_index != continuity.position {
        return Some(Violation::SegmentIndex);
    }
    let (first, prev) = continuity.previous?;
    if header.run_id != first.run_id {
        Some(Violation::SegmentRun)
    } else if header.origin_unix_ns != first.origin_unix_ns {
        Some(Violation::SegmentOrigin)
    } else if header.input_recorded != first.input_recorded {
        Some(Violation::SegmentFlags)
    } else if header.term != first.term {
        Some(Violation::SegmentTerm)
    } else if prev.next_sequence != Some(header.first_sequence) {
        Some(Violation::SegmentFirstSequence)
    } else if header.geometry != prev.final_geometry {
        Some(Violation::SegmentGeometry)
    } else {
        None
    }
}

fn error_at(kind: DecodeErrorKind, offset: usize, last: Option<Sequence>) -> DecodeError {
    DecodeError {
        kind,
        segment: None,
        // usize is at most 64 bits on every supported target.
        offset: offset as u64,
        last_valid_sequence: last,
    }
}

fn be_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([bytes[at], bytes[at + 1]])
}

fn be_u32(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0u8; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_be_bytes(word)
}

fn be_u64(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_be_bytes(word)
}

/// Decode and validate the header in the spec's decoding order.
fn decode_header(bytes: &[u8]) -> Result<(SegmentHeader, usize), DecodeErrorKind> {
    use DecodeErrorKind::{BadMagic, Corrupt, Invalid, UnsupportedVersion};

    if bytes.len() < HEADER_MIN_LEN {
        return Err(Corrupt(Corruption::HeaderIncomplete));
    }
    if bytes[..8] != MAGIC {
        return Err(BadMagic);
    }
    let version = be_u16(bytes, 8);
    if version != FORMAT_VERSION {
        return Err(UnsupportedVersion(version));
    }
    let header_len = usize::from(be_u16(bytes, 10));
    if !(HEADER_MIN_LEN..=HEADER_MAX_LEN).contains(&header_len) {
        return Err(Corrupt(Corruption::HeaderLength));
    }
    if bytes.len() < header_len {
        return Err(Corrupt(Corruption::HeaderIncomplete));
    }
    let crc_at = header_len - CRC_LEN;
    if crc32c::crc32c(&bytes[..crc_at]) != be_u32(bytes, crc_at) {
        return Err(Corrupt(Corruption::HeaderChecksum));
    }
    let term_len = usize::from(be_u16(bytes, 54));
    if HEADER_MIN_LEN + term_len != header_len {
        return Err(Invalid(Violation::HeaderLengthMismatch));
    }

    let flags = be_u16(bytes, 12);
    if flags & !FLAG_INPUT_RECORDED != 0 {
        return Err(Invalid(Violation::ReservedFlags));
    }
    let mut run = [0u8; 16];
    run.copy_from_slice(&bytes[14..30]);
    let run_id =
        RunId::from_v7(uuid::Uuid::from_bytes(run)).ok_or(Invalid(Violation::RunIdVersion))?;
    let segment_index = be_u32(bytes, 30);
    let first_sequence =
        Sequence::new(be_u64(bytes, 34)).ok_or(Invalid(Violation::FirstSequence))?;
    if segment_index == 0 && first_sequence != Sequence::FIRST {
        return Err(Invalid(Violation::FirstSequence));
    }
    let geometry = Geometry::new(be_u16(bytes, 50), be_u16(bytes, 52))
        .ok_or(Invalid(Violation::ZeroGeometry))?;
    let term_bytes = &bytes[HEADER_FIXED_LEN..HEADER_FIXED_LEN + term_len];
    let term = std::str::from_utf8(term_bytes)
        .ok()
        .and_then(|s| TermName::new(s).ok())
        .ok_or(Invalid(Violation::TermByte))?;

    let header = SegmentHeader {
        run_id,
        segment_index,
        first_sequence,
        origin_unix_ns: be_u64(bytes, 42),
        geometry,
        input_recorded: flags & FLAG_INPUT_RECORDED != 0,
        term,
    };
    Ok((header, header_len))
}

/// Decode a checksummed record body and apply the record rules.
fn decode_body(
    body: &[u8],
    header: &SegmentHeader,
    summary: &SegmentSummary,
) -> Result<Record, Violation> {
    let kind = body[0];
    let sequence_raw = be_u64(body, 1);
    let elapsed_ns = be_u64(body, 9);
    let payload = &body[BODY_FIXED_LEN..];

    let kind = match kind {
        KIND_OUTPUT => {
            if payload.is_empty() {
                return Err(Violation::PayloadSize);
            }
            RecordKind::Output(payload.to_vec())
        }
        KIND_INPUT => {
            if payload.is_empty() {
                return Err(Violation::PayloadSize);
            }
            if !header.input_recorded {
                return Err(Violation::InputNotRecorded);
            }
            RecordKind::Input(payload.to_vec())
        }
        KIND_RESIZE => {
            if payload.len() != RESIZE_PAYLOAD_LEN {
                return Err(Violation::PayloadSize);
            }
            let geometry = Geometry::new(be_u16(payload, 0), be_u16(payload, 2))
                .ok_or(Violation::ZeroGeometry)?;
            let correlation = be_u32(payload, 5);
            let cause = match payload[4] {
                CAUSE_USER if correlation == 0 => ResizeCause::User,
                CAUSE_REPAINT => ResizeCause::Repaint {
                    correlation: NonZeroU32::new(correlation).ok_or(Violation::Correlation)?,
                },
                CAUSE_USER => return Err(Violation::Correlation),
                other => return Err(Violation::UnknownResizeCause(other)),
            };
            RecordKind::Resize { geometry, cause }
        }
        other => return Err(Violation::UnknownKind(other)),
    };

    let sequence = match summary.next_sequence {
        Some(expected) if expected.get() == sequence_raw => expected,
        _ => return Err(Violation::Sequence),
    };
    if summary
        .last_elapsed_ns
        .is_some_and(|previous| elapsed_ns < previous)
    {
        return Err(Violation::ElapsedRegressed);
    }
    Ok(Record {
        sequence,
        elapsed_ns,
        kind,
    })
}

fn decode_segment_with_summary(
    bytes: &[u8],
    continuity: Option<&Continuity>,
) -> Result<(DecodedSegment, SegmentSummary), DecodeError> {
    let (header, header_len) = decode_header(bytes).map_err(|kind| error_at(kind, 0, None))?;
    if let Some(violation) = continuity.and_then(|c| check_header_continuity(&header, c)) {
        return Err(error_at(DecodeErrorKind::Invalid(violation), 0, None));
    }
    let previous_elapsed_ns = continuity
        .and_then(|c| c.previous)
        .and_then(|(_, prev)| prev.last_elapsed_ns);
    let mut summary = SegmentSummary {
        header_len,
        next_sequence: Some(header.first_sequence),
        last_sequence: None,
        last_elapsed_ns: None,
        final_geometry: header.geometry,
    };
    let mut records = Vec::new();
    let mut pos = header_len;

    let end = loop {
        let remaining = bytes.len() - pos;
        if remaining == 0 {
            break SegmentEnd::Clean;
        }
        if remaining < PREFIX_LEN {
            break SegmentEnd::Truncated { offset: pos as u64 };
        }
        let corrupt = |c| error_at(DecodeErrorKind::Corrupt(c), pos, summary.last_sequence);
        let body_len_word = be_u32(bytes, pos);
        if be_u32(bytes, pos + 4) != !body_len_word {
            return Err(corrupt(Corruption::LengthCheck));
        }
        let body_len = usize::try_from(body_len_word).unwrap_or(usize::MAX);
        if !(BODY_FIXED_LEN..=BODY_MAX_LEN).contains(&body_len) {
            return Err(corrupt(Corruption::LengthBounds));
        }
        let record_len = PREFIX_LEN + body_len + CRC_LEN;
        if remaining < record_len {
            break SegmentEnd::Truncated { offset: pos as u64 };
        }
        let crc_at = pos + PREFIX_LEN + body_len;
        if crc32c::crc32c(&bytes[pos..crc_at]) != be_u32(bytes, crc_at) {
            return Err(corrupt(Corruption::RecordChecksum));
        }
        let record = decode_body(&bytes[pos + PREFIX_LEN..crc_at], &header, &summary)
            .map_err(|v| error_at(DecodeErrorKind::Invalid(v), pos, summary.last_sequence))?;
        if records.is_empty() && previous_elapsed_ns.is_some_and(|prev| record.elapsed_ns < prev) {
            return Err(error_at(
                DecodeErrorKind::Invalid(Violation::SegmentElapsed),
                pos,
                None,
            ));
        }

        summary.next_sequence = record.sequence.next();
        summary.last_sequence = Some(record.sequence);
        summary.last_elapsed_ns = Some(record.elapsed_ns);
        if let RecordKind::Resize { geometry, .. } = record.kind {
            summary.final_geometry = geometry;
        }
        records.push(record);
        pos += record_len;
    };

    Ok((
        DecodedSegment {
            header,
            records,
            end,
        },
        summary,
    ))
}

/// Decode an ordered list of segments as one recording.
///
/// # Errors
///
/// [`DecodeError`] for any per-segment or cross-segment failure.
pub fn decode_recording<'a, I>(segments: I) -> Result<DecodedRecording, DecodeError>
where
    I: IntoIterator<Item = &'a [u8]>,
{
    let mut segments = segments.into_iter().peekable();
    if segments.peek().is_none() {
        return Err(error_at(
            DecodeErrorKind::Invalid(Violation::NoSegments),
            0,
            None,
        ));
    }

    let mut first: Option<SegmentHeader> = None;
    let mut previous: Option<SegmentSummary> = None;
    let mut records = Vec::new();
    let mut end = SegmentEnd::Clean;
    let mut position: u32 = 0;

    while let Some(bytes) = segments.next() {
        let at = |kind, offset, last| DecodeError {
            segment: Some(position),
            ..error_at(kind, offset, last)
        };
        let last_so_far = previous.and_then(|p| p.last_sequence);
        let continuity = Continuity {
            position,
            previous: first.as_ref().zip(previous),
        };
        let (segment, summary) =
            decode_segment_with_summary(bytes, Some(&continuity)).map_err(|e| DecodeError {
                segment: Some(position),
                last_valid_sequence: e.last_valid_sequence.or(last_so_far),
                ..e
            })?;
        let is_final = segments.peek().is_none();

        if let (SegmentEnd::Truncated { offset }, false) = (segment.end, is_final) {
            return Err(DecodeError {
                offset,
                ..at(
                    DecodeErrorKind::Corrupt(Corruption::TruncatedInteriorSegment),
                    0,
                    summary.last_sequence.or(last_so_far),
                )
            });
        }

        let invalid = |v, offset| at(DecodeErrorKind::Invalid(v), offset, last_so_far);
        if segment.records.is_empty() && !is_final {
            return Err(invalid(Violation::EmptyInteriorSegment, summary.header_len));
        }

        end = segment.end;
        records.extend(segment.records);
        if first.is_none() {
            first = Some(segment.header);
        }
        // An empty segment is final (checked above), so no later segment reads
        // its summary.
        previous = Some(summary);
        let Some(next_position) = position.checked_add(1) else {
            return Err(invalid(Violation::SegmentIndex, 0));
        };
        position = next_position;
    }

    Ok(DecodedRecording {
        header: first.expect("at least one segment was decoded"),
        segment_count: position,
        records,
        end,
    })
}
