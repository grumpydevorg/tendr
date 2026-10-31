//! Contract tests for the PTY recording codec (`tendr::recording`).
//! Wire-format authority: `docs/architecture/08-pty-recording-format.md`.

use std::num::NonZeroU32;

use proptest::prelude::*;
use tendr::model::ids::RunId;
use tendr::recording::{
    Corruption, DecodeErrorKind, EncodeError, Geometry, MAX_PAYLOAD, Record, RecordKind,
    ResizeCause, SegmentEncoder, SegmentEnd, SegmentHeader, Sequence, TermName, TermNameError,
    Violation, decode_recording, decode_segment, dump,
};

// ---------------------------------------------------------------------------
// Fixture and raw builders
// ---------------------------------------------------------------------------

const FIXTURE: &str = include_str!("fixtures/recording/v1-segment0.hex");
const RUN_BYTES: [u8; 16] = [
    0x01, 0x90, 0xf1, 0xb2, 0x3c, 0x4d, 0x7e, 0x5f, 0x8a, 0x6b, 0x7c, 0x8d, 0x9e, 0x0f, 0xa1, 0xb2,
];
const ORIGIN: u64 = 1_789_000_000_000_000_000;
/// Record boundaries in the fixture: header end, then each record's end.
const BOUNDARIES: [usize; 5] = [73, 106, 140, 178, 216];

const OUTPUT: u8 = 0x01;
const RESIZE: u8 = 0x02;
const INPUT: u8 = 0x03;

fn fixture_bytes() -> Vec<u8> {
    FIXTURE
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .flat_map(str::split_whitespace)
        .map(|tok| u8::from_str_radix(tok, 16).expect("fixture token is hex"))
        .collect()
}

/// The fixture run id with its version nibble changed to 4.
fn v4_bytes() -> [u8; 16] {
    let mut bytes = RUN_BYTES;
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes
}

fn run_id() -> RunId {
    RunId::from_v7(uuid::Uuid::from_bytes(RUN_BYTES)).expect("fixture run id is v7")
}

fn seq(n: u64) -> Sequence {
    Sequence::new(n).expect("nonzero sequence")
}

fn geo(rows: u16, cols: u16) -> Geometry {
    Geometry::new(rows, cols).expect("nonzero geometry")
}

fn fixture_header() -> SegmentHeader {
    SegmentHeader {
        run_id: run_id(),
        segment_index: 0,
        first_sequence: Sequence::FIRST,
        origin_unix_ns: ORIGIN,
        geometry: geo(24, 80),
        input_recorded: false,
        term: TermName::new("xterm-ghostty").unwrap(),
    }
}

fn fixture_records() -> Vec<Record> {
    vec![
        Record {
            sequence: seq(1),
            elapsed_ns: 1_000_000,
            kind: RecordKind::Output(b"hi\x1b[".to_vec()),
        },
        Record {
            sequence: seq(2),
            elapsed_ns: 1_500_000,
            kind: RecordKind::Output(b"31m\xff\xfe".to_vec()),
        },
        Record {
            sequence: seq(3),
            elapsed_ns: 2_000_000,
            kind: RecordKind::Resize {
                geometry: geo(30, 100),
                cause: ResizeCause::User,
            },
        },
        Record {
            sequence: seq(4),
            elapsed_ns: 2_000_000,
            kind: RecordKind::Resize {
                geometry: geo(29, 100),
                cause: ResizeCause::Repaint {
                    correlation: NonZeroU32::new(1).unwrap(),
                },
            },
        },
    ]
}

/// Header fields with no validation, for building invalid headers.
#[derive(Clone)]
struct RawHeader {
    magic: [u8; 8],
    version: u16,
    header_len: Option<u16>,
    flags: u16,
    run: [u8; 16],
    index: u32,
    first_sequence: u64,
    origin: u64,
    rows: u16,
    cols: u16,
    term: Vec<u8>,
    padding: usize,
}

impl Default for RawHeader {
    fn default() -> Self {
        Self {
            magic: *b"TNDRREC\0",
            version: 1,
            header_len: None,
            flags: 0,
            run: RUN_BYTES,
            index: 0,
            first_sequence: 1,
            origin: ORIGIN,
            rows: 24,
            cols: 80,
            term: b"xterm-ghostty".to_vec(),
            padding: 0,
        }
    }
}

impl RawHeader {
    fn bytes(&self) -> Vec<u8> {
        let term_len = u16::try_from(self.term.len()).unwrap();
        let header_len = self
            .header_len
            .unwrap_or(60 + term_len + u16::try_from(self.padding).unwrap());
        let mut out = Vec::new();
        out.extend_from_slice(&self.magic);
        out.extend_from_slice(&self.version.to_be_bytes());
        out.extend_from_slice(&header_len.to_be_bytes());
        out.extend_from_slice(&self.flags.to_be_bytes());
        out.extend_from_slice(&self.run);
        out.extend_from_slice(&self.index.to_be_bytes());
        out.extend_from_slice(&self.first_sequence.to_be_bytes());
        out.extend_from_slice(&self.origin.to_be_bytes());
        out.extend_from_slice(&self.rows.to_be_bytes());
        out.extend_from_slice(&self.cols.to_be_bytes());
        out.extend_from_slice(&term_len.to_be_bytes());
        out.extend_from_slice(&self.term);
        out.extend(std::iter::repeat_n(0u8, self.padding));
        let crc = crc32c::crc32c(&out);
        out.extend_from_slice(&crc.to_be_bytes());
        out
    }
}

fn raw_record(kind: u8, sequence: u64, elapsed_ns: u64, payload: &[u8]) -> Vec<u8> {
    let mut body = vec![kind];
    body.extend_from_slice(&sequence.to_be_bytes());
    body.extend_from_slice(&elapsed_ns.to_be_bytes());
    body.extend_from_slice(payload);
    raw_framed(u32::try_from(body.len()).unwrap(), &body)
}

fn raw_framed(body_len: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&body_len.to_be_bytes());
    out.extend_from_slice(&(body_len ^ 0xFFFF_FFFF).to_be_bytes());
    out.extend_from_slice(body);
    let crc = crc32c::crc32c(&out);
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

fn resize_payload(rows: u16, cols: u16, cause: u8, correlation: u32) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&rows.to_be_bytes());
    p.extend_from_slice(&cols.to_be_bytes());
    p.push(cause);
    p.extend_from_slice(&correlation.to_be_bytes());
    p
}

fn concat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

fn assert_segment_error(bytes: &[u8], kind: DecodeErrorKind, offset: u64, last_valid: Option<u64>) {
    let err = decode_segment(bytes).expect_err("segment must be rejected");
    assert_eq!(err.kind, kind, "error kind");
    assert_eq!(err.offset, offset, "error offset");
    assert_eq!(
        err.last_valid_sequence.map(Sequence::get),
        last_valid,
        "last valid sequence"
    );
    assert_eq!(err.segment, None);
}

fn encode_segment(encoder: &mut SegmentEncoder, records: &[Record]) -> Vec<u8> {
    let mut out = encoder.header_bytes();
    for record in records {
        out.extend(encoder.encode(record).expect("valid record"));
    }
    out
}

fn output(sequence: u64, elapsed_ns: u64, bytes: &[u8]) -> Record {
    Record {
        sequence: seq(sequence),
        elapsed_ns,
        kind: RecordKind::Output(bytes.to_vec()),
    }
}

// ---------------------------------------------------------------------------
// CRC-32C: independent vectors, so the dependency is checked, not trusted
// ---------------------------------------------------------------------------

#[test]
fn crc32c_matches_check_value_and_rfc3720_vectors() {
    let ascending: Vec<u8> = (0u8..32).collect();
    let descending: Vec<u8> = (0u8..32).rev().collect();
    assert_eq!(crc32c::crc32c(b"123456789"), 0xE306_9283);
    assert_eq!(crc32c::crc32c(&[0u8; 32]), 0x8A91_36AA);
    assert_eq!(crc32c::crc32c(&[0xFFu8; 32]), 0x62A8_AB43);
    assert_eq!(crc32c::crc32c(&ascending), 0x46DD_794E);
    assert_eq!(crc32c::crc32c(&descending), 0x113F_DB5C);
}

// ---------------------------------------------------------------------------
// Value types
// ---------------------------------------------------------------------------

#[test]
fn run_id_from_v7_rejects_other_versions() {
    assert!(RunId::from_v7(uuid::Uuid::from_bytes(RUN_BYTES)).is_some());
    assert!(RunId::from_v7(uuid::Uuid::from_bytes(v4_bytes())).is_none());
    assert!(RunId::from_v7(uuid::Uuid::nil()).is_none());
}

#[test]
fn geometry_and_sequence_reject_zero() {
    assert!(Geometry::new(0, 80).is_none());
    assert!(Geometry::new(24, 0).is_none());
    assert_eq!(geo(24, 80).rows(), 24);
    assert_eq!(geo(24, 80).cols(), 80);
    assert!(Sequence::new(0).is_none());
    assert_eq!(Sequence::FIRST.get(), 1);
    assert_eq!(seq(1).next(), Some(seq(2)));
    assert_eq!(seq(u64::MAX).next(), None);
}

#[test]
fn term_name_enforces_length_and_printable_ascii() {
    assert!(TermName::new("").is_ok());
    assert!(TermName::new("x".repeat(256)).is_ok());
    assert_eq!(TermName::new("x".repeat(257)), Err(TermNameError::TooLong));
    assert_eq!(TermName::new("xterm 256"), Err(TermNameError::InvalidByte));
    assert_eq!(
        TermName::new("xterm\u{e9}"),
        Err(TermNameError::InvalidByte)
    );
    assert_eq!(
        TermName::new("tmux-256color").unwrap().as_str(),
        "tmux-256color"
    );
}

// ---------------------------------------------------------------------------
// Golden fixture
// ---------------------------------------------------------------------------

#[test]
fn golden_fixture_decodes_to_the_documented_records() {
    let decoded = decode_segment(&fixture_bytes()).unwrap();
    assert_eq!(decoded.header, fixture_header());
    assert_eq!(decoded.records, fixture_records());
    assert_eq!(decoded.end, SegmentEnd::Clean);
}

#[test]
fn encoding_the_documented_records_reproduces_the_golden_fixture() {
    let mut encoder = SegmentEncoder::new(fixture_header()).unwrap();
    let bytes = encode_segment(&mut encoder, &fixture_records());
    assert_eq!(bytes, fixture_bytes());
    assert_eq!(encoder.header_bytes().len(), BOUNDARIES[0]);
}

// ---------------------------------------------------------------------------
// Dump (development aid; `cargo run --example dump-recording`)
// ---------------------------------------------------------------------------

fn dump_text(segments: &[&[u8]]) -> String {
    let decoded = decode_recording(segments.iter().copied()).unwrap();
    let mut out = Vec::new();
    dump(&decoded, &mut out).unwrap();
    String::from_utf8(out).expect("the dump is text")
}

#[test]
fn dump_of_the_golden_fixture_is_stable() {
    let expected = r#"format 1
run 0190f1b2-3c4d-7e5f-8a6b-7c8d9e0fa1b2
segments 1 (first index 0, first sequence 1)
origin_unix_ns 1789000000000000000
geometry rows=24 cols=80
term xterm-ghostty
input not recorded
#1 1.000ms output len=4 "hi\x1b["
#2 1.500ms output len=5 "31m\xff\xfe"
#3 2.000ms resize rows=30 cols=100 user
#4 2.000ms resize rows=29 cols=100 repaint correlation=1
end clean
"#;
    assert_eq!(dump_text(&[&fixture_bytes()]), expected);
}

#[test]
fn dump_reports_an_incomplete_final_record() {
    let bytes = fixture_bytes();
    let text = dump_text(&[&bytes[..BOUNDARIES[4] - 1]]);
    assert!(
        text.ends_with(
            "#3 2.000ms resize rows=30 cols=100 user\n\
             end truncated at segment 0 offset 178 (incomplete final record excluded)\n"
        ),
        "{text}"
    );
}

#[test]
fn dump_flags_queries_across_records_and_segments() {
    let header = SegmentHeader {
        input_recorded: true,
        ..fixture_header()
    };
    let mut first = SegmentEncoder::new(header).unwrap();
    let segment0 = encode_segment(
        &mut first,
        &[
            output(1, 1_000_000, b"ok\r\n\x1b"),
            Record {
                sequence: seq(2),
                elapsed_ns: 2_000_000,
                kind: RecordKind::Resize {
                    geometry: geo(30, 100),
                    cause: ResizeCause::User,
                },
            },
        ],
    );
    let mut second = first.next_segment().unwrap();
    let mut segment1 = encode_segment(
        &mut second,
        &[
            output(3, 3_000_000, b"[6n"),
            Record {
                sequence: seq(4),
                elapsed_ns: 4_500_000,
                kind: RecordKind::Input(b"\x1b[1;1R".to_vec()),
            },
            output(5, 5_000_000, b"\x1b[?1049h\x1b[?2004h\\"),
            output(6, 6_000_000, b"\x1b]11;?"),
        ],
    );
    let tail = segment1.len();
    segment1.extend(&second.encode(&output(7, 7_000_000, b"\x07")).unwrap()[..10]);

    let expected = format!(
        r#"format 1
run 0190f1b2-3c4d-7e5f-8a6b-7c8d9e0fa1b2
segments 2 (first index 0, first sequence 1)
origin_unix_ns 1789000000000000000
geometry rows=24 cols=80
term xterm-ghostty
input recorded
#1 1.000ms output len=5 "ok\r\n\x1b"
#2 2.000ms resize rows=30 cols=100 user
#3 3.000ms output len=3 "[6n"
  ^ query:dsr-cpr "\x1b[6n" from #1
#4 4.500ms input len=6 "\x1b[1;1R"
#5 5.000ms output len=17 "\x1b[?1049h\x1b[?2004h\\"
  ^ mode:alt-screen-on "\x1b[?1049h"
  ^ mode:bracketed-paste-on "\x1b[?2004h"
#6 6.000ms output len=6 "\x1b]11;?"
unfinished escape sequence from #6: "\x1b]11;?"
end truncated at segment 1 offset {tail} (incomplete final record excluded)
"#
    );
    assert_eq!(dump_text(&[&segment0, &segment1]), expected);
}

// ---------------------------------------------------------------------------
// End of segment: clean, truncated, corrupt
// ---------------------------------------------------------------------------

#[test]
fn header_only_segment_ends_clean() {
    let bytes = &fixture_bytes()[..BOUNDARIES[0]];
    let decoded = decode_segment(bytes).unwrap();
    assert!(decoded.records.is_empty());
    assert_eq!(decoded.end, SegmentEnd::Clean);
}

#[test]
fn every_cut_point_is_clean_at_boundaries_and_truncated_elsewhere() {
    let bytes = fixture_bytes();
    for cut in BOUNDARIES[0]..=BOUNDARIES[4] {
        let decoded = decode_segment(&bytes[..cut])
            .unwrap_or_else(|e| panic!("cut {cut}: unexpected error {e:?}"));
        let complete = BOUNDARIES[1..].iter().filter(|&&end| end <= cut).count();
        assert_eq!(decoded.records, fixture_records()[..complete], "cut {cut}");
        if BOUNDARIES.contains(&cut) {
            assert_eq!(decoded.end, SegmentEnd::Clean, "cut {cut}");
        } else {
            let start = BOUNDARIES[complete] as u64;
            assert_eq!(
                decoded.end,
                SegmentEnd::Truncated { offset: start },
                "cut {cut}"
            );
        }
    }
}

#[test]
fn bad_checksum_on_the_final_complete_record_is_corruption_not_truncation() {
    let mut bytes = fixture_bytes();
    bytes[BOUNDARIES[4] - 1] ^= 0x01;
    assert_segment_error(
        &bytes,
        DecodeErrorKind::Corrupt(Corruption::RecordChecksum),
        BOUNDARIES[3] as u64,
        Some(3),
    );
}

#[test]
fn flipped_payload_bit_in_final_record_is_corruption() {
    let mut bytes = fixture_bytes();
    bytes[BOUNDARIES[3] + 8 + 17] ^= 0x80;
    assert_segment_error(
        &bytes,
        DecodeErrorKind::Corrupt(Corruption::RecordChecksum),
        BOUNDARIES[3] as u64,
        Some(3),
    );
}

#[test]
fn bad_checksum_on_an_interior_record_reports_its_offset() {
    let mut bytes = fixture_bytes();
    bytes[BOUNDARIES[1] + 8 + 17] ^= 0x01;
    assert_segment_error(
        &bytes,
        DecodeErrorKind::Corrupt(Corruption::RecordChecksum),
        BOUNDARIES[1] as u64,
        Some(1),
    );
}

#[test]
fn damaged_length_that_would_look_truncated_is_caught_by_the_check_word() {
    let bytes = fixture_bytes();
    // Growing body_len by one would make the final record look one byte short.
    let mut grown = bytes.clone();
    grown[BOUNDARIES[3] + 3] = grown[BOUNDARIES[3] + 3].wrapping_add(1);
    assert_segment_error(
        &grown,
        DecodeErrorKind::Corrupt(Corruption::LengthCheck),
        BOUNDARIES[3] as u64,
        Some(3),
    );
    // A huge length would look like a long incomplete tail.
    let mut huge = bytes;
    huge[BOUNDARIES[3]] = 0x7F;
    assert_segment_error(
        &huge,
        DecodeErrorKind::Corrupt(Corruption::LengthCheck),
        BOUNDARIES[3] as u64,
        Some(3),
    );
}

#[test]
fn consistent_but_out_of_bounds_length_is_corruption() {
    let header = RawHeader::default().bytes();
    let too_small = raw_framed(16, &[OUTPUT; 16]);
    assert_segment_error(
        &concat(&[&header, &too_small]),
        DecodeErrorKind::Corrupt(Corruption::LengthBounds),
        73,
        None,
    );
    let mut body = vec![OUTPUT];
    body.extend_from_slice(&1u64.to_be_bytes());
    body.extend_from_slice(&0u64.to_be_bytes());
    body.extend(std::iter::repeat_n(b'x', MAX_PAYLOAD + 1));
    let too_large = raw_framed(u32::try_from(body.len()).unwrap(), &body);
    assert_segment_error(
        &concat(&[&header, &too_large]),
        DecodeErrorKind::Corrupt(Corruption::LengthBounds),
        73,
        None,
    );
}

#[test]
fn largest_payload_round_trips() {
    let header = fixture_header();
    let mut encoder = SegmentEncoder::new(header).unwrap();
    let big = output(1, 0, &vec![0xAB; MAX_PAYLOAD]);
    let bytes = encode_segment(&mut encoder, std::slice::from_ref(&big));
    assert_eq!(decode_segment(&bytes).unwrap().records, vec![big]);
}

// ---------------------------------------------------------------------------
// Header decoding order and rules
// ---------------------------------------------------------------------------

#[test]
fn incomplete_header_is_corruption_never_truncation() {
    let bytes = fixture_bytes();
    for cut in [0, 12, 59, 60, 72] {
        assert_segment_error(
            &bytes[..cut],
            DecodeErrorKind::Corrupt(Corruption::HeaderIncomplete),
            0,
            None,
        );
    }
}

#[test]
fn wrong_magic_is_rejected() {
    let raw = RawHeader {
        magic: *b"TNDRREX\0",
        ..RawHeader::default()
    };
    assert_segment_error(&raw.bytes(), DecodeErrorKind::BadMagic, 0, None);
}

#[test]
fn unknown_version_is_rejected_before_anything_else_is_interpreted() {
    let mut bytes = fixture_bytes();
    bytes[9] = 2; // version 2; the header checksum is now also wrong
    bytes[10] = 0xFF; // and header_len (0xFF49) is out of range
    assert_segment_error(&bytes, DecodeErrorKind::UnsupportedVersion(2), 0, None);
}

#[test]
fn header_length_outside_bounds_is_corruption() {
    for header_len in [59u16, 317] {
        let raw = RawHeader {
            header_len: Some(header_len),
            ..RawHeader::default()
        };
        let mut bytes = raw.bytes();
        bytes.resize(400, 0);
        assert_segment_error(
            &bytes,
            DecodeErrorKind::Corrupt(Corruption::HeaderLength),
            0,
            None,
        );
    }
}

#[test]
fn header_checksum_covers_the_term_bytes() {
    let mut bytes = fixture_bytes();
    bytes[60] ^= 0x01;
    assert_segment_error(
        &bytes,
        DecodeErrorKind::Corrupt(Corruption::HeaderChecksum),
        0,
        None,
    );
}

#[test]
fn checksummed_header_with_inconsistent_term_length_is_invalid() {
    let raw = RawHeader {
        padding: 1,
        ..RawHeader::default()
    };
    assert_segment_error(
        &raw.bytes(),
        DecodeErrorKind::Invalid(Violation::HeaderLengthMismatch),
        0,
        None,
    );
}

#[test]
fn header_semantic_rules_are_enforced() {
    let v4 = v4_bytes();
    let cases = [
        (
            RawHeader {
                flags: 0b10,
                ..RawHeader::default()
            },
            Violation::ReservedFlags,
        ),
        (
            RawHeader {
                run: v4,
                ..RawHeader::default()
            },
            Violation::RunIdVersion,
        ),
        (
            RawHeader {
                first_sequence: 2,
                ..RawHeader::default()
            },
            Violation::FirstSequence,
        ),
        (
            RawHeader {
                index: 3,
                first_sequence: 0,
                ..RawHeader::default()
            },
            Violation::FirstSequence,
        ),
        (
            RawHeader {
                rows: 0,
                ..RawHeader::default()
            },
            Violation::ZeroGeometry,
        ),
        (
            RawHeader {
                cols: 0,
                ..RawHeader::default()
            },
            Violation::ZeroGeometry,
        ),
        (
            RawHeader {
                term: b"xterm ghostty".to_vec(),
                ..RawHeader::default()
            },
            Violation::TermByte,
        ),
    ];
    for (raw, violation) in cases {
        assert_segment_error(&raw.bytes(), DecodeErrorKind::Invalid(violation), 0, None);
    }
}

#[test]
fn nonzero_segment_index_may_start_at_a_later_sequence() {
    let raw = RawHeader {
        index: 1,
        first_sequence: 42,
        ..RawHeader::default()
    };
    let bytes = concat(&[&raw.bytes(), &raw_record(OUTPUT, 42, 7, b"x")]);
    let decoded = decode_segment(&bytes).unwrap();
    assert_eq!(decoded.header.first_sequence, seq(42));
    assert_eq!(decoded.records, vec![output(42, 7, b"x")]);
}

// ---------------------------------------------------------------------------
// Record rules
// ---------------------------------------------------------------------------

fn header73() -> Vec<u8> {
    RawHeader::default().bytes()
}

#[test]
fn record_kind_and_payload_rules_are_enforced() {
    let cases: Vec<(Vec<u8>, Violation)> = vec![
        (raw_record(0x04, 1, 0, b"x"), Violation::UnknownKind(0x04)),
        (raw_record(0x00, 1, 0, b"x"), Violation::UnknownKind(0x00)),
        (raw_record(OUTPUT, 1, 0, b""), Violation::PayloadSize),
        (
            raw_record(RESIZE, 1, 0, &resize_payload(30, 100, 1, 0)[..8]),
            Violation::PayloadSize,
        ),
        (
            raw_record(
                RESIZE,
                1,
                0,
                &[resize_payload(30, 100, 1, 0), vec![0]].concat(),
            ),
            Violation::PayloadSize,
        ),
        (
            raw_record(RESIZE, 1, 0, &resize_payload(0, 100, 1, 0)),
            Violation::ZeroGeometry,
        ),
        (
            raw_record(RESIZE, 1, 0, &resize_payload(30, 100, 3, 0)),
            Violation::UnknownResizeCause(3),
        ),
        (
            raw_record(RESIZE, 1, 0, &resize_payload(30, 100, 1, 7)),
            Violation::Correlation,
        ),
        (
            raw_record(RESIZE, 1, 0, &resize_payload(30, 100, 2, 0)),
            Violation::Correlation,
        ),
        (
            raw_record(INPUT, 1, 0, b"ls\r"),
            Violation::InputNotRecorded,
        ),
    ];
    for (record, violation) in cases {
        assert_segment_error(
            &concat(&[&header73(), &record]),
            DecodeErrorKind::Invalid(violation),
            73,
            None,
        );
    }
}

#[test]
fn input_records_are_accepted_when_input_recording_is_enabled() {
    let raw = RawHeader {
        flags: 1,
        ..RawHeader::default()
    };
    let bytes = concat(&[&raw.bytes(), &raw_record(INPUT, 1, 0, b"ls\r")]);
    let decoded = decode_segment(&bytes).unwrap();
    assert!(decoded.header.input_recorded);
    assert_eq!(decoded.records[0].kind, RecordKind::Input(b"ls\r".to_vec()));
    // An empty input payload is still invalid.
    assert_segment_error(
        &concat(&[&raw.bytes(), &raw_record(INPUT, 1, 0, b"")]),
        DecodeErrorKind::Invalid(Violation::PayloadSize),
        73,
        None,
    );
}

#[test]
fn sequence_must_start_at_first_sequence_and_stay_contiguous() {
    let first = raw_record(OUTPUT, 1, 0, b"a");
    let cases: Vec<(Vec<u8>, u64, Option<u64>)> = vec![
        (raw_record(OUTPUT, 2, 0, b"a"), 73, None),
        (
            concat(&[&first, &raw_record(OUTPUT, 3, 0, b"b")]),
            73 + 30,
            Some(1),
        ),
        (
            concat(&[&first, &raw_record(OUTPUT, 1, 0, b"b")]),
            73 + 30,
            Some(1),
        ),
    ];
    for (records, offset, last_valid) in cases {
        assert_segment_error(
            &concat(&[&header73(), &records]),
            DecodeErrorKind::Invalid(Violation::Sequence),
            offset,
            last_valid,
        );
    }
}

#[test]
fn elapsed_time_may_repeat_but_never_regress() {
    let equal = concat(&[
        &header73(),
        &raw_record(OUTPUT, 1, 5, b"a"),
        &raw_record(OUTPUT, 2, 5, b"b"),
    ]);
    assert_eq!(decode_segment(&equal).unwrap().records.len(), 2);

    let regress = concat(&[
        &header73(),
        &raw_record(OUTPUT, 1, 5, b"a"),
        &raw_record(OUTPUT, 2, 4, b"b"),
    ]);
    assert_segment_error(
        &regress,
        DecodeErrorKind::Invalid(Violation::ElapsedRegressed),
        73 + 30,
        Some(1),
    );
}

#[test]
fn sequence_overflow_is_invalid() {
    let raw = RawHeader {
        index: 1,
        first_sequence: u64::MAX,
        ..RawHeader::default()
    };
    let bytes = concat(&[
        &raw.bytes(),
        &raw_record(OUTPUT, u64::MAX, 0, b"a"),
        &raw_record(OUTPUT, 0, 0, b"b"),
    ]);
    assert_segment_error(
        &bytes,
        DecodeErrorKind::Invalid(Violation::Sequence),
        73 + 30,
        Some(u64::MAX),
    );
}

// ---------------------------------------------------------------------------
// Encoder rules
// ---------------------------------------------------------------------------

#[test]
fn encoder_requires_segment_zero_to_start_at_one() {
    let header = SegmentHeader {
        first_sequence: seq(2),
        ..fixture_header()
    };
    assert_eq!(
        SegmentEncoder::new(header).err(),
        Some(EncodeError::FirstSequence)
    );
}

#[test]
fn encoder_rejects_rule_breaking_records_without_changing_state() {
    let mut encoder = SegmentEncoder::new(fixture_header()).unwrap();
    assert_eq!(
        encoder.encode(&output(2, 0, b"a")),
        Err(EncodeError::Sequence {
            expected: 1,
            got: 2
        })
    );
    assert_eq!(
        encoder.encode(&output(1, 0, b"")),
        Err(EncodeError::EmptyPayload)
    );
    assert_eq!(
        encoder.encode(&output(1, 0, &vec![0; MAX_PAYLOAD + 1])),
        Err(EncodeError::PayloadTooLarge {
            len: MAX_PAYLOAD + 1
        })
    );
    assert_eq!(
        encoder.encode(&Record {
            sequence: seq(1),
            elapsed_ns: 0,
            kind: RecordKind::Input(b"x".to_vec()),
        }),
        Err(EncodeError::InputNotRecorded)
    );
    encoder.encode(&output(1, 10, b"a")).unwrap();
    assert_eq!(
        encoder.encode(&output(2, 9, b"b")),
        Err(EncodeError::ElapsedRegressed {
            previous: 10,
            got: 9
        })
    );
    encoder.encode(&output(2, 10, b"b")).unwrap();
}

#[test]
fn encoder_tracks_geometry_through_resizes() {
    let mut encoder = SegmentEncoder::new(fixture_header()).unwrap();
    assert_eq!(encoder.geometry(), geo(24, 80));
    for record in fixture_records() {
        encoder.encode(&record).unwrap();
    }
    assert_eq!(encoder.geometry(), geo(29, 100));
}

#[test]
fn rotation_continues_sequence_time_and_geometry() {
    let mut first = SegmentEncoder::new(fixture_header()).unwrap();
    assert_eq!(
        first.next_segment().err(),
        Some(EncodeError::EmptySegmentRotation)
    );
    for record in fixture_records() {
        first.encode(&record).unwrap();
    }

    let mut second = first.next_segment().unwrap();
    let header = second.header().clone();
    assert_eq!(header.segment_index, 1);
    assert_eq!(header.first_sequence, seq(5));
    assert_eq!(header.geometry, geo(29, 100));
    assert_eq!(header.run_id, run_id());
    assert_eq!(header.origin_unix_ns, ORIGIN);
    assert_eq!(header.term, fixture_header().term);
    assert!(!header.input_recorded);
    assert_eq!(
        second.encode(&output(5, 1_999_999, b"late")),
        Err(EncodeError::ElapsedRegressed {
            previous: 2_000_000,
            got: 1_999_999
        })
    );
    second.encode(&output(5, 2_000_000, b"ok")).unwrap();
}

// ---------------------------------------------------------------------------
// Cross-segment rules
// ---------------------------------------------------------------------------

fn two_segments() -> (Vec<u8>, SegmentEncoder) {
    let mut first = SegmentEncoder::new(fixture_header()).unwrap();
    let bytes = encode_segment(&mut first, &fixture_records());
    (bytes, first)
}

fn segment_with_header(header: SegmentHeader, records: &[Record]) -> Vec<u8> {
    let mut encoder = SegmentEncoder::new(header).unwrap();
    encode_segment(&mut encoder, records)
}

fn assert_recording_error(segments: &[&[u8]], kind: DecodeErrorKind, segment: u32) {
    let err = decode_recording(segments.iter().copied()).expect_err("recording must be rejected");
    assert_eq!(err.kind, kind);
    assert_eq!(err.segment, Some(segment));
}

#[test]
fn rotated_segments_decode_as_one_contiguous_recording() {
    let (seg0, first) = two_segments();
    let mut second = first.next_segment().unwrap();
    let seg1 = encode_segment(&mut second, &[output(5, 3_000_000, b"more")]);

    let decoded = decode_recording([seg0.as_slice(), seg1.as_slice()]).unwrap();

    let mut expected = fixture_records();
    expected.push(output(5, 3_000_000, b"more"));
    assert_eq!(decoded.header, fixture_header());
    assert_eq!(decoded.segment_count, 2);
    assert_eq!(decoded.records, expected);
    assert_eq!(decoded.end, SegmentEnd::Clean);
}

#[test]
fn an_empty_recording_is_invalid() {
    let err = decode_recording(std::iter::empty::<&[u8]>()).unwrap_err();
    assert_eq!(err.kind, DecodeErrorKind::Invalid(Violation::NoSegments));
}

#[test]
fn cross_segment_identity_and_continuity_rules_are_enforced() {
    let (seg0, first) = two_segments();
    let good = first.next_segment().unwrap().header().clone();
    let rec = |n| output(n, 3_000_000, b"x");
    let other_run = RunId::new();

    let cases: Vec<(SegmentHeader, Vec<Record>, Violation)> = vec![
        (
            SegmentHeader {
                segment_index: 2,
                ..good.clone()
            },
            vec![rec(5)],
            Violation::SegmentIndex,
        ),
        (
            SegmentHeader {
                run_id: other_run,
                ..good.clone()
            },
            vec![rec(5)],
            Violation::SegmentRun,
        ),
        (
            SegmentHeader {
                origin_unix_ns: ORIGIN + 1,
                ..good.clone()
            },
            vec![rec(5)],
            Violation::SegmentOrigin,
        ),
        (
            SegmentHeader {
                input_recorded: true,
                ..good.clone()
            },
            vec![rec(5)],
            Violation::SegmentFlags,
        ),
        (
            SegmentHeader {
                term: TermName::new("xterm-256color").unwrap(),
                ..good.clone()
            },
            vec![rec(5)],
            Violation::SegmentTerm,
        ),
        (
            SegmentHeader {
                first_sequence: seq(6),
                ..good.clone()
            },
            vec![rec(6)],
            Violation::SegmentFirstSequence,
        ),
        (
            SegmentHeader {
                geometry: geo(24, 80),
                ..good.clone()
            },
            vec![rec(5)],
            Violation::SegmentGeometry,
        ),
        (
            good.clone(),
            vec![output(5, 1_999_999, b"x")],
            Violation::SegmentElapsed,
        ),
    ];
    for (header, records, violation) in cases {
        let seg1 = segment_with_header(header, &records);
        assert_recording_error(
            &[seg0.as_slice(), seg1.as_slice()],
            DecodeErrorKind::Invalid(violation),
            1,
        );
    }
}

#[test]
fn empty_segment_is_valid_only_at_the_end() {
    let (seg0, first) = two_segments();
    let empty_header = first.next_segment().unwrap().header().clone();
    let empty = segment_with_header(empty_header.clone(), &[]);

    let decoded = decode_recording([seg0.as_slice(), empty.as_slice()]).unwrap();
    assert_eq!(decoded.segment_count, 2);
    assert_eq!(decoded.records, fixture_records());

    let after = segment_with_header(
        SegmentHeader {
            segment_index: 2,
            ..empty_header
        },
        &[output(5, 3_000_000, b"x")],
    );
    assert_recording_error(
        &[seg0.as_slice(), empty.as_slice(), after.as_slice()],
        DecodeErrorKind::Invalid(Violation::EmptyInteriorSegment),
        1,
    );
}

#[test]
fn only_the_final_segment_may_be_truncated() {
    let (seg0, first) = two_segments();
    let mut second = first.next_segment().unwrap();
    let seg1 = encode_segment(&mut second, &[output(5, 3_000_000, b"tail")]);
    let truncated0 = &seg0[..seg0.len() - 1];
    let truncated1 = &seg1[..seg1.len() - 1];

    assert_recording_error(
        &[truncated0, seg1.as_slice()],
        DecodeErrorKind::Corrupt(Corruption::TruncatedInteriorSegment),
        0,
    );

    let decoded = decode_recording([seg0.as_slice(), truncated1]).unwrap();
    assert_eq!(decoded.records, fixture_records());
    // seg1 is a 73-byte header plus one record; the cut record starts at 73.
    assert_eq!(decoded.end, SegmentEnd::Truncated { offset: 73 });
}

// ---------------------------------------------------------------------------
// Recovery boundary: last_valid_sequence never includes records that break
// cross-segment rules, even when a later record in that segment is corrupt.
// ---------------------------------------------------------------------------

/// Segment 1 with a valid-looking first record, then a record whose checksum is
/// broken. `header` is adjusted by `mutate`.
fn seg1_then_bad_checksum(
    first: &SegmentEncoder,
    mutate: impl FnOnce(&mut SegmentHeader),
    sequence: u64,
    elapsed_ns: u64,
) -> Vec<u8> {
    let mut header = first.next_segment().unwrap().header().clone();
    mutate(&mut header);
    let mut bytes = segment_with_header(
        header,
        &[
            output(sequence, elapsed_ns, b"not-a-valid-prefix"),
            output(sequence + 1, elapsed_ns + 1, b"bad-checksum"),
        ],
    );
    *bytes.last_mut().unwrap() ^= 0x01;
    bytes
}

fn assert_recovery_boundary(seg1: &[u8], kind: DecodeErrorKind, last_valid: u64) {
    let (seg0, _) = two_segments();
    let err = decode_recording([seg0.as_slice(), seg1]).unwrap_err();
    assert_eq!(err.kind, kind, "the continuity violation is reported first");
    assert_eq!(err.segment, Some(1));
    assert_eq!(
        err.last_valid_sequence.map(Sequence::get),
        Some(last_valid),
        "recovery boundary"
    );
}

#[test]
fn foreign_run_segment_does_not_advance_the_recovery_boundary() {
    let (_, first) = two_segments();
    let seg1 = seg1_then_bad_checksum(&first, |h| h.run_id = RunId::new(), 5, 3_000_000);
    assert_recovery_boundary(&seg1, DecodeErrorKind::Invalid(Violation::SegmentRun), 4);
}

#[test]
fn sequence_gap_segment_does_not_advance_the_recovery_boundary() {
    let (_, first) = two_segments();
    let seg1 = seg1_then_bad_checksum(&first, |h| h.first_sequence = seq(100), 100, 3_000_000);
    assert_recovery_boundary(
        &seg1,
        DecodeErrorKind::Invalid(Violation::SegmentFirstSequence),
        4,
    );
}

#[test]
fn time_regressing_segment_does_not_advance_the_recovery_boundary() {
    let (_, first) = two_segments();
    let seg1 = seg1_then_bad_checksum(&first, |_| {}, 5, 0);
    assert_recovery_boundary(
        &seg1,
        DecodeErrorKind::Invalid(Violation::SegmentElapsed),
        4,
    );
}

#[test]
fn continuing_segment_with_a_later_bad_checksum_keeps_its_valid_records() {
    let (_, first) = two_segments();
    let seg1 = seg1_then_bad_checksum(&first, |_| {}, 5, 3_000_000);
    assert_recovery_boundary(
        &seg1,
        DecodeErrorKind::Corrupt(Corruption::RecordChecksum),
        5,
    );
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Spec {
    Output(Vec<u8>),
    Input(Vec<u8>),
    Resize(u16, u16, Option<u32>),
}

fn spec_strategy() -> impl Strategy<Value = Spec> {
    prop_oneof![
        4 => proptest::collection::vec(any::<u8>(), 1..200).prop_map(Spec::Output),
        1 => proptest::collection::vec(any::<u8>(), 1..40).prop_map(Spec::Input),
        1 => (1u16..=500, 1u16..=500, proptest::option::of(1u32..)).prop_map(|(r, c, k)| Spec::Resize(r, c, k)),
    ]
}

proptest! {
    #[test]
    fn valid_segments_round_trip(
        input_recorded in any::<bool>(),
        rows in 1u16..=500,
        cols in 1u16..=500,
        origin in any::<u64>(),
        term in "[!-~]{0,40}",
        specs in proptest::collection::vec((spec_strategy(), 0u64..1_000_000), 0..40),
    ) {
        let header = SegmentHeader {
            run_id: RunId::new(),
            segment_index: 0,
            first_sequence: Sequence::FIRST,
            origin_unix_ns: origin,
            geometry: geo(rows, cols),
            input_recorded,
            term: TermName::new(term).unwrap(),
        };
        let mut encoder = SegmentEncoder::new(header.clone()).unwrap();
        let mut bytes = encoder.header_bytes();
        let mut records = Vec::new();
        let mut elapsed = 0u64;
        let mut next = Sequence::FIRST;
        for (spec, delta) in specs {
            let kind = match spec {
                Spec::Output(b) => RecordKind::Output(b),
                Spec::Input(b) if input_recorded => RecordKind::Input(b),
                Spec::Input(b) => RecordKind::Output(b),
                Spec::Resize(r, c, correlation) => RecordKind::Resize {
                    geometry: geo(r, c),
                    cause: correlation
                        .and_then(NonZeroU32::new)
                        .map_or(ResizeCause::User, |correlation| ResizeCause::Repaint { correlation }),
                },
            };
            elapsed += delta;
            let record = Record { sequence: next, elapsed_ns: elapsed, kind };
            bytes.extend(encoder.encode(&record).unwrap());
            records.push(record);
            next = next.next().unwrap();
        }

        let decoded = decode_segment(&bytes).unwrap();
        prop_assert_eq!(decoded.header, header);
        prop_assert_eq!(decoded.records, records);
        prop_assert_eq!(decoded.end, SegmentEnd::Clean);
    }

    #[test]
    fn output_bytes_are_preserved_exactly(chunks in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 1..64), 0..60)) {
        let mut encoder = SegmentEncoder::new(fixture_header()).unwrap();
        let mut bytes = encoder.header_bytes();
        for (i, chunk) in chunks.iter().enumerate() {
            let record = output(i as u64 + 1, i as u64, chunk);
            bytes.extend(encoder.encode(&record).unwrap());
        }
        let replayed: Vec<u8> = decode_segment(&bytes)
            .unwrap()
            .records
            .into_iter()
            .flat_map(|r| match r.kind {
                RecordKind::Output(b) => b,
                other => panic!("unexpected record {other:?}"),
            })
            .collect();
        prop_assert_eq!(replayed, chunks.concat());
    }
}
