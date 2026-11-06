//! Asciicast exports from recordings written with the library encoder.

mod harness;
use harness::Home;

use std::path::{Path, PathBuf};

use harness::tendr;
use tendr::model::ids::RunId;
use tendr::recording::{
    Geometry, Record, RecordKind, ResizeCause, SegmentEncoder, SegmentHeader, Sequence, TermName,
};

/// When the fixtures' recordings started, in Unix nanoseconds.
const ORIGIN_NS: u64 = 1_700_000_000_123_456_789;

fn output(ms: u64, bytes: &[u8]) -> (u64, RecordKind) {
    (ms, RecordKind::Output(bytes.to_vec()))
}

fn input(ms: u64, bytes: &[u8]) -> (u64, RecordKind) {
    (ms, RecordKind::Input(bytes.to_vec()))
}

fn user(ms: u64, rows: u16, cols: u16) -> (u64, RecordKind) {
    (
        ms,
        RecordKind::Resize {
            geometry: Geometry::new(rows, cols).unwrap(),
            cause: ResizeCause::User,
        },
    )
}

fn repaint(ms: u64, rows: u16, cols: u16, correlation: u32) -> (u64, RecordKind) {
    (
        ms,
        RecordKind::Resize {
            geometry: Geometry::new(rows, cols).unwrap(),
            cause: ResizeCause::Repaint {
                correlation: std::num::NonZeroU32::new(correlation).unwrap(),
            },
        },
    )
}

/// A recording written to disk: its directory, segment files, and where
/// each segment's records start.
struct Written {
    dir: PathBuf,
    segments: Vec<PathBuf>,
    record_offsets: Vec<Vec<u64>>,
}

/// Write one run's recording under `dir`, starting at `rows`x`cols`, with
/// each slice of `segments` in a segment of its own.
fn write_recording(
    dir: &Path,
    (rows, cols): (u16, u16),
    input_recorded: bool,
    segments: &[&[(u64, RecordKind)]],
) -> Written {
    std::fs::create_dir_all(dir).unwrap();
    let mut encoder = SegmentEncoder::new(SegmentHeader {
        run_id: RunId::new(),
        segment_index: 0,
        first_sequence: Sequence::FIRST,
        origin_unix_ns: ORIGIN_NS,
        geometry: Geometry::new(rows, cols).unwrap(),
        input_recorded,
        term: TermName::new("xterm-256color").unwrap(),
    })
    .unwrap();
    let mut sequence = Sequence::FIRST;
    let mut written = Written {
        dir: dir.to_path_buf(),
        segments: Vec::new(),
        record_offsets: Vec::new(),
    };
    for (index, records) in segments.iter().enumerate() {
        if index > 0 {
            encoder = encoder.next_segment().unwrap();
        }
        let mut bytes = encoder.header_bytes();
        let mut offsets = Vec::new();
        for (ms, kind) in *records {
            offsets.push(bytes.len() as u64);
            let record = Record {
                sequence,
                elapsed_ns: ms * 1_000_000,
                kind: kind.clone(),
            };
            bytes.extend(encoder.encode(&record).unwrap());
            sequence = sequence.next().unwrap();
        }
        let path = dir.join(format!("seg-{index:08}.tndrrec"));
        std::fs::write(&path, bytes).unwrap();
        written.segments.push(path);
        written.record_offsets.push(offsets);
    }
    written
}

// --- export ---

/// UTF-8 split between two records, across a segment boundary and a resize,
/// comes out whole; every applied resize is an `r` event, repaints included;
/// input is an `i` event. Times are seconds since the recording's origin.
#[test]
fn export_joins_split_utf8_and_lists_every_resize() {
    let root = Home::new();
    let rec = write_recording(
        &root.path().join("rec"),
        (24, 80),
        true,
        &[
            &[output(0, b"h\xc3"), user(1000, 30, 100)],
            &[
                output(1500, b"\xa9!"),
                repaint(2000, 29, 100, 7),
                repaint(2000, 30, 100, 7),
                input(2250, b"q\xe2\x82"),
                input(2300, b"\xac"),
                output(2500, b"\x1b[0m"),
            ],
        ],
    );

    let out = tendr(&root)
        .arg("export")
        .arg(&rec.dir)
        .args(["--format", "asciicast"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        concat!(
            "{\"version\": 2, \"width\": 80, \"height\": 24, \"timestamp\": 1700000000, ",
            "\"env\": {\"TERM\": \"xterm-256color\"}}\n",
            "[0.000000, \"o\", \"h\"]\n",
            "[1.000000, \"r\", \"100x30\"]\n",
            "[1.500000, \"o\", \"é!\"]\n",
            "[2.000000, \"r\", \"100x29\"]\n",
            "[2.000000, \"r\", \"100x30\"]\n",
            "[2.250000, \"i\", \"q\"]\n",
            "[2.300000, \"i\", \"€\"]\n",
            "[2.500000, \"o\", \"\\u001b[0m\"]\n",
        )
    );
}

/// Invalid UTF-8 refuses the export, naming the record and byte, and writes
/// nothing to stdout.
#[test]
fn export_refuses_invalid_utf8() {
    let root = Home::new();
    let rec = write_recording(
        &root.path().join("rec"),
        (24, 80),
        false,
        &[&[output(0, b"ok"), output(1, b"a\xffb")]],
    );
    let out = tendr(&root).arg("export").arg(&rec.dir).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "{:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("invalid UTF-8 at byte 1 of record #2"),
        "{stderr}"
    );
}

/// A damaged interior record refuses the export before writing anything.
#[test]
fn a_corrupt_interior_segment_exports_nothing_and_names_the_offset() {
    let root = Home::new();
    let rec = write_recording(
        &root.path().join("rec"),
        (24, 80),
        false,
        &[
            &[output(0, b"one"), output(1, b"two")],
            &[output(2, b"three")],
        ],
    );
    let first = &rec.segments[0];
    let mut bytes = std::fs::read(first).unwrap();
    *bytes.last_mut().unwrap() ^= 0xff; // the second record's checksum
    std::fs::write(first, bytes).unwrap();

    let out = tendr(&root).arg("export").arg(&rec.dir).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    let expected = format!(
        "{}: corrupt: record checksum mismatch at byte offset {}; the last valid record \
         before it is #1. Nothing was exported",
        first.display(),
        rec.record_offsets[0][1]
    );
    assert!(stderr.contains(&expected), "{stderr}");
}

#[test]
fn truncated_tail_exports_complete_records_and_reports_cut() {
    let root = Home::new();
    let rec = write_recording(
        &root.path().join("rec"),
        (24, 80),
        false,
        &[&[output(0, b"complete"), output(1, b"cut")]],
    );
    let segment = &rec.segments[0];
    let mut bytes = std::fs::read(segment).unwrap();
    bytes.pop();
    std::fs::write(segment, bytes).unwrap();

    let out = tendr(&root).arg("export").arg(segment).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("[0.000000, \"o\", \"complete\"]"),
        "{stdout}"
    );
    assert!(!stdout.contains("cut"), "{stdout}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("ends inside a record"), "{stderr}");
    assert!(stderr.contains("last complete record"), "{stderr}");
}
