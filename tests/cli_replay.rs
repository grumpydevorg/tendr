//! `tendr replay`: playback in a real terminal (a PTY the test drives, as a
//! person at the window would), and `--export asciicast`. The recordings are
//! written with the library encoder, so each test states exactly what was
//! recorded.

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
        .args(["replay", "--export", "asciicast"])
        .arg(&rec.dir)
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
    let out = tendr(&root)
        .args(["replay", "--export", "asciicast"])
        .arg(&rec.dir)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "{:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("invalid UTF-8 at byte 1 of record #2"),
        "{stderr}"
    );
}

/// A damaged record in a segment before the last stops replay before
/// anything is played, naming the file and byte offset. Decoding comes
/// before the terminal: this run has none.
#[test]
fn a_corrupt_interior_segment_plays_nothing_and_names_the_offset() {
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

    for export in [false, true] {
        let mut cmd = tendr(&root);
        cmd.arg("replay");
        if export {
            cmd.args(["--export", "asciicast"]);
        }
        let out = cmd.arg(&rec.dir).output().unwrap();
        assert_eq!(out.status.code(), Some(1), "export: {export}");
        assert!(out.stdout.is_empty(), "export: {export}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        let expected = format!(
            "{}: corrupt: record checksum mismatch at byte offset {}; the last valid record \
             before it is #1. Nothing was played",
            first.display(),
            rec.record_offsets[0][1]
        );
        assert!(stderr.contains(&expected), "export {export}: {stderr}");
    }
}

#[test]
fn replay_refuses_host() {
    let root = Home::new();
    let out = tendr(&root)
        .args(["--host", "box", "replay", "rec"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("'replay' is not supported over SSH"),
        "{stderr}"
    );
}

// --- playback ---

#[cfg(unix)]
mod playback {
    use super::*;
    use harness::terminal::{WrappedAttach, tendr_bin};
    use tendr::term_modes::TermModes;

    /// Run `tendr replay <args> <dir>` in the test's terminal, which starts at
    /// 24x80.
    fn replay(root: &Home, label: &str, args: &[&str], dir: &Path) -> WrappedAttach {
        let mut argv = vec![tendr_bin(), "replay".to_owned()];
        argv.extend(args.iter().map(|a| (*a).to_owned()));
        argv.push(dir.to_str().unwrap().to_owned());
        WrappedAttach::spawn_argv(root, label, &argv)
    }

    /// A final segment cut off inside a record (an active run's) plays, across
    /// the segment boundary, to its last complete record, and the cut is
    /// reported with the file and byte offset.
    #[test]
    fn a_truncated_final_segment_plays_to_its_last_complete_record() {
        let root = Home::new();
        let rec = write_recording(
            &root.path().join("rec"),
            (24, 80),
            false,
            &[
                &[output(0, b"<seg0-a>"), output(1, b"<seg0-b>")],
                &[output(2, b"<seg1-a>"), output(3, b"<seg1-cut>")],
            ],
        );
        let last = &rec.segments[1];
        let cut_at = rec.record_offsets[1][1];
        let bytes = std::fs::read(last).unwrap();
        std::fs::write(last, &bytes[..bytes.len() - 3]).unwrap();

        let wrapped = replay(&root, "cut", &["--no-delay"], &rec.dir);
        assert_eq!(wrapped.wait_exit(), 0, "{}", wrapped.cli.output());
        wrapped.assert_terminal_restored();
        let shown = wrapped.cli.output();
        let a = shown.find("<seg0-a>").expect(&shown);
        let b = shown.find("<seg0-b>").expect(&shown);
        let c = shown.find("<seg1-a>").expect(&shown);
        assert!(a < b && b < c, "{shown}");
        assert!(!shown.contains("<seg1-cut>"), "{shown}");
        assert!(
            shown.contains(&format!(
                "ends inside a record at {} byte offset {cut_at}",
                last.display()
            )),
            "{shown}"
        );
        assert!(
            shown.contains("up to #3, its last complete record"),
            "{shown}"
        );
    }

    /// The window is asked for once, before playback. Several attaches
    /// (repaint pairs) and resizes later, nothing prompts again, and the one
    /// mismatch is reported after playback.
    #[test]
    fn default_geometry_prompts_once_whatever_the_recording_does() {
        let root = Home::new();
        let rec = write_recording(
            &root.path().join("rec"),
            (30, 100),
            false,
            &[&[
                output(0, b"<start>"),
                user(1, 40, 120),
                repaint(2, 39, 120, 1),
                repaint(2, 40, 120, 1),
                output(3, b"<middle>"),
                user(4, 24, 80),
                repaint(5, 23, 80, 2),
                repaint(5, 24, 80, 2),
                user(6, 30, 100),
                output(7, b"<end>"),
            ]],
        );

        let mut wrapped = replay(&root, "prompt", &["--no-delay"], &rec.dir);
        wrapped.wait_output_contains("Size this window to 100x30 (now 80x24)");
        assert!(!wrapped.cli.output().contains("<start>"));
        wrapped.cli.type_bytes(b" ");
        assert_eq!(wrapped.wait_exit(), 0, "{}", wrapped.cli.output());
        wrapped.assert_terminal_restored();

        let shown = wrapped.cli.output();
        assert!(shown.contains("<end>"), "{shown}");
        assert_eq!(shown.matches("Size this window").count(), 1, "{shown}");
        assert!(!shown.contains("resizes to"), "{shown}");
        assert_eq!(
            shown.matches("the picture may differ").count(),
            1,
            "{shown}"
        );
        assert!(
            shown.contains("from the start the recording was 100x30 and this window 80x24"),
            "{shown}"
        );
    }

    /// `--strict-geometry` stops at each real size change until the window
    /// matches, and never for a repaint.
    #[test]
    fn strict_geometry_stops_at_each_real_size_change() {
        let root = Home::new();
        let rec = write_recording(
            &root.path().join("rec"),
            (24, 80),
            false,
            &[&[
                output(0, b"<A>"),
                user(1, 30, 100),
                output(2, b"<B>"),
                repaint(3, 29, 100, 1),
                repaint(3, 30, 100, 1),
                output(4, b"<C>"),
                user(5, 24, 80),
                output(6, b"<D>"),
            ]],
        );

        let wrapped = replay(
            &root,
            "strict",
            &["--strict-geometry", "--no-delay"],
            &rec.dir,
        );
        // The pause never ends by itself: until the window matches, what
        // follows the resize is not played.
        wrapped.wait_output_contains("the recording resizes to 100x30 here");
        let shown = wrapped.cli.output();
        assert!(shown.contains("<A>") && !shown.contains("<B>"), "{shown}");

        wrapped.cli.resize(30, 100);
        wrapped.wait_output_contains("the recording resizes to 80x24 here");
        let shown = wrapped.cli.output();
        assert!(shown.contains("<C>") && !shown.contains("<D>"), "{shown}");

        wrapped.cli.resize(24, 80);
        assert_eq!(wrapped.wait_exit(), 0, "{}", wrapped.cli.output());
        wrapped.assert_terminal_restored();
        let shown = wrapped.cli.output();
        assert!(shown.contains("<D>"), "{shown}");
        assert_eq!(
            shown.matches("the recording resizes to").count(),
            2,
            "{shown}"
        );
        assert!(!shown.contains("Size this window"), "{shown}");
    }

    /// The output a query is drained against: a query and a keyboard protocol
    /// in the first record, and a second record an hour later, which never
    /// plays within a test.
    fn query_then_an_hour(root: &Home) -> (Written, Vec<u8>) {
        let first = b"<Q>\x1b[>1u\x1b[6n";
        let rec = write_recording(
            &root.path().join("rec"),
            (24, 80),
            false,
            &[&[output(0, first), output(3_600_000, b"<LATE>")]],
        );
        let mut modes = TermModes::new();
        modes.feed(first);
        (rec, modes.reset())
    }

    /// The terminal's replies to a played query are read and dropped: replay
    /// writes nothing back and keeps playing. Ctrl-C, in the keyboard
    /// protocol the recording turned on, stops it; the protocol is turned off
    /// and the terminal restored.
    #[test]
    fn replies_are_drained_and_ctrl_c_stops_with_the_terminal_restored() {
        let root = Home::new();
        let (rec, reset) = query_then_an_hour(&root);
        let mut wrapped = replay(&root, "drain", &[], &rec.dir);
        wrapped.wait_output_contains("<Q>\x1b[>1u\x1b[6n");
        wrapped
            .cli
            .type_bytes(b"\x1b[24;1R\x1b]11;rgb:0000/0000/0000\x07q");
        wrapped.cli.type_bytes(b"\x1b[99;5u");
        assert_eq!(wrapped.wait_exit(), 0, "{}", wrapped.cli.output());
        wrapped.assert_terminal_restored();

        let shown = wrapped.cli.output();
        assert!(!shown.contains("<LATE>"), "{shown}");
        assert!(
            !shown.contains("24;1R") && !shown.contains("rgb:"),
            "{shown}"
        );
        let after_query = &shown[shown.find("\x1b[6n").unwrap()..];
        assert!(
            after_query.contains(std::str::from_utf8(&reset).unwrap()),
            "the keyboard protocol is turned off: {after_query:?}"
        );
        assert!(shown.contains("replay stopped after #1"), "{shown}");
    }

    /// A signal mid-play ends replay with 1, the terminal restored.
    #[test]
    fn a_signal_mid_play_restores_the_terminal() {
        let root = Home::new();
        let (rec, _) = query_then_an_hour(&root);
        let wrapped = replay(&root, "signal", &[], &rec.dir);
        wrapped.wait_output_contains("<Q>");
        wrapped.signal(libc::SIGTERM);
        assert_eq!(wrapped.wait_exit(), 1, "{}", wrapped.cli.output());
        wrapped.assert_terminal_restored();
        let shown = wrapped.cli.output();
        assert!(
            shown.contains("replay ended by SIGTERM after #1"),
            "{shown}"
        );
        assert!(!shown.contains("<LATE>"), "{shown}");
    }
}
