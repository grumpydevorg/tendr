//! Append + read path tests — spec §3.2–3.5, §5 (docs/architecture/07-event-protocol.md).

use std::path::Path;

use tempfile::TempDir;
use tendr::events::{EventDraft, EventWriter, read_session_events};
use tendr::model::event::{Event, Kind, Uuid7};
use tendr::model::ids::{Namespace, RunId, SessionName, Source};

fn draft(kind: &str, data: serde_json::Value) -> EventDraft {
    EventDraft {
        id: None,
        kind: Kind::new(kind).unwrap(),
        namespace: Namespace::new("default").unwrap(),
        session: SessionName::new("s1").unwrap(),
        run_id: RunId::new(),
        generation: Some(1),
        source: Source::trusted("tendr.sidecar").unwrap(),
        block_id: None,
        parent_id: None,
        data: Some(data),
        preview: None,
    }
}

fn segment_files(session_dir: &Path) -> Vec<std::path::PathBuf> {
    let events_dir = session_dir.join("events");
    let mut segs: Vec<_> = std::fs::read_dir(&events_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    segs.sort();
    segs
}

fn read_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

// --- Basic append ---

#[test]
fn append_creates_segment_and_writes_one_line() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("s1");
    std::fs::create_dir_all(&session_dir).unwrap();

    let mut writer = EventWriter::new(&session_dir);
    let event = writer
        .append(
            draft("build.finished", serde_json::json!({"ok": true})),
            false,
        )
        .unwrap();

    let segs = segment_files(&session_dir);
    assert_eq!(segs.len(), 1, "exactly one segment created");
    // Segment name is a UUIDv7 (time-sortable identity).
    let stem = segs[0].file_stem().unwrap().to_str().unwrap();
    assert!(
        stem.parse::<Uuid7>().is_ok(),
        "segment name is uuidv7: {stem}"
    );

    let lines = read_lines(&segs[0]);
    assert_eq!(lines.len(), 1);
    let parsed: Event = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(parsed.id, event.id);
    assert_eq!(parsed.seq, 1);
    assert_eq!(parsed.v, 1);
    assert_eq!(parsed.kind.as_str(), "build.finished");
}

#[test]
fn append_picks_lexicographically_greatest_segment() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("s1");
    let events_dir = session_dir.join("events");
    std::fs::create_dir_all(&events_dir).unwrap();

    // Two pre-existing segments; uuidv7 names sort by creation time.
    let older = events_dir.join("01981f00-0000-7000-8000-000000000000.jsonl");
    let newer = events_dir.join("01981fff-0000-7000-8000-000000000000.jsonl");
    std::fs::write(&older, "").unwrap();
    std::fs::write(&newer, "").unwrap();

    let mut writer = EventWriter::new(&session_dir);
    writer
        .append(draft("build.finished", serde_json::json!({})), false)
        .unwrap();

    assert_eq!(read_lines(&older).len(), 0, "older segment untouched");
    assert_eq!(read_lines(&newer).len(), 1, "append goes to newest segment");
}

#[test]
fn writer_seq_is_contiguous_from_one() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("s1");
    std::fs::create_dir_all(&session_dir).unwrap();

    let mut writer = EventWriter::new(&session_dir);
    for expected_seq in 1..=3u64 {
        let event = writer
            .append(draft("build.finished", serde_json::json!({})), false)
            .unwrap();
        assert_eq!(event.seq, expected_seq);
        assert_eq!(event.writer, writer.writer_id());
    }
}

// --- Oversize spill (spec §3.4) ---

#[test]
fn oversize_data_spills_to_content_addressed_blob() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("s1");
    std::fs::create_dir_all(&session_dir).unwrap();

    let big = serde_json::json!({"payload": "x".repeat(1024 * 1024)}); // ~1 MiB
    let mut writer = EventWriter::new(&session_dir);
    let event = writer
        .append(draft("build.log", big.clone()), false)
        .unwrap();

    let data_ref = event.data_ref.as_ref().expect("data_ref present");
    assert_eq!(event.truncated, Some(true));
    assert_eq!(data_ref.media_type, "application/json");
    assert_eq!(data_ref.path, format!("events/blobs/{}", data_ref.sha256));

    // Blob holds the full serialized data, keyed by its sha256.
    let blob_path = session_dir.join(&data_ref.path);
    let blob = std::fs::read(&blob_path).unwrap();
    assert_eq!(blob.len() as u64, data_ref.bytes);
    let full: serde_json::Value = serde_json::from_slice(&blob).unwrap();
    assert_eq!(full, big);

    // Inline data is a ≤4 KiB preview.
    let preview = serde_json::to_string(event.data.as_ref().unwrap()).unwrap();
    assert!(
        preview.len() <= 4 * 1024 + 64,
        "preview is small: {}",
        preview.len()
    );

    // The event line itself parses and stays small: well under 32 KiB.
    let segs = segment_files(&session_dir);
    let lines = read_lines(&segs[0]);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].len() <= 32 * 1024);
}

#[test]
fn identical_oversize_payload_stores_one_blob() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("s1");
    std::fs::create_dir_all(&session_dir).unwrap();

    let big = serde_json::json!({"payload": "y".repeat(64 * 1024)});
    let mut writer = EventWriter::new(&session_dir);
    let first = writer
        .append(draft("build.log", big.clone()), false)
        .unwrap();
    let second = writer.append(draft("build.log", big), false).unwrap();

    assert_eq!(
        first.data_ref.as_ref().unwrap().sha256,
        second.data_ref.as_ref().unwrap().sha256
    );
    let blobs_dir = session_dir.join("events").join("blobs");
    let blob_count = std::fs::read_dir(&blobs_dir).unwrap().count();
    assert_eq!(blob_count, 1, "identical payloads dedupe to one blob");
}

#[test]
fn blob_failure_degrades_to_inline_truncation() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("s1");
    let events_dir = session_dir.join("events");
    std::fs::create_dir_all(&events_dir).unwrap();
    // A regular file where the blobs dir must go makes blob writes fail.
    std::fs::write(events_dir.join("blobs"), "not a dir").unwrap();

    let big = serde_json::json!({"payload": "z".repeat(64 * 1024)});
    let mut writer = EventWriter::new(&session_dir);
    let event = writer.append(draft("build.log", big), false).unwrap();

    assert!(event.data_ref.is_none(), "no data_ref on blob failure");
    assert_eq!(event.truncated, Some(true));
    let inline = serde_json::to_string(event.data.as_ref().unwrap()).unwrap();
    assert!(
        inline.len() <= 4 * 1024 + 64,
        "degraded inline is a preview"
    );
}

#[test]
fn small_data_stays_inline_untouched() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("s1");
    std::fs::create_dir_all(&session_dir).unwrap();

    let data = serde_json::json!({"ok": true, "artifacts": 3});
    let mut writer = EventWriter::new(&session_dir);
    let event = writer
        .append(draft("build.finished", data.clone()), false)
        .unwrap();

    assert_eq!(event.data.as_ref().unwrap(), &data);
    assert!(event.data_ref.is_none());
    assert!(event.truncated.is_none());
    assert!(!session_dir.join("events").join("blobs").exists());
}

// --- Slice 3: caller-supplied identity + structured spill preview ---

#[test]
fn append_honors_pre_minted_id() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("s1");
    std::fs::create_dir_all(&session_dir).unwrap();

    let minted = Uuid7::new();
    let mut d = draft("hook.post_tool_use", serde_json::json!({"ok": true}));
    d.id = Some(minted);
    let mut writer = EventWriter::new(&session_dir);
    let event = writer.append(d, false).unwrap();

    assert_eq!(event.id, minted, "caller-supplied id is stamped verbatim");
    let segs = segment_files(&session_dir);
    let parsed: Event = serde_json::from_str(&read_lines(&segs[0])[0]).unwrap();
    assert_eq!(parsed.id, minted, "pre-minted id round-trips the segment");
}

#[test]
fn orphan_stamp_honors_pre_minted_id() {
    let minted = Uuid7::new();
    let mut d = draft("hook.post_tool_use", serde_json::json!({}));
    d.id = Some(minted);
    let event = tendr::events::stamp_orphan_event(d);
    assert_eq!(event.id, minted);
}

#[test]
fn oversize_spill_uses_caller_supplied_structured_preview() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("s1");
    std::fs::create_dir_all(&session_dir).unwrap();

    let big = serde_json::json!({
        "exit_code": 0,
        "cwd_after": "/data",
        "timed_out": false,
        "stderr": "",
        "stdout": "x".repeat(1024 * 1024),
        "truncated": false,
    });
    let structured = serde_json::json!({
        "exit_code": 0,
        "cwd_after": "/data",
        "timed_out": false,
        "stderr": "",
        "stdout_preview": "x".repeat(512),
        "truncated": true,
    });
    let mut d = draft("exec.result", big.clone());
    d.preview = Some(structured.clone());
    let mut writer = EventWriter::new(&session_dir);
    let event = writer.append(d, false).unwrap();

    assert_eq!(
        event.data.as_ref().unwrap(),
        &structured,
        "inline data is the caller's structured preview"
    );
    assert_eq!(event.truncated, Some(true));
    let data_ref = event.data_ref.as_ref().expect("full data spilled to blob");
    let blob = std::fs::read(session_dir.join(&data_ref.path)).unwrap();
    let full: serde_json::Value = serde_json::from_slice(&blob).unwrap();
    assert_eq!(full, big, "blob holds the full untruncated data");
}

#[test]
fn oversize_caller_preview_degrades_to_generic() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("s1");
    std::fs::create_dir_all(&session_dir).unwrap();

    let big = serde_json::json!({"stdout": "y".repeat(64 * 1024)});
    let mut d = draft("exec.result", big);
    // Supplied preview itself busts MAX_PREVIEW_BYTES — fall back to generic.
    d.preview = Some(serde_json::json!({"stdout_preview": "y".repeat(8 * 1024)}));
    let mut writer = EventWriter::new(&session_dir);
    let event = writer.append(d, false).unwrap();

    assert_eq!(event.truncated, Some(true));
    let inline = event.data.as_ref().unwrap();
    assert!(
        inline.get("preview").is_some(),
        "generic head-of-JSON preview shape used, got: {inline}"
    );
    let serialized = serde_json::to_string(inline).unwrap();
    assert!(serialized.len() <= 4 * 1024 + 64);
}

#[test]
fn preview_ignored_when_data_fits_inline() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("s1");
    std::fs::create_dir_all(&session_dir).unwrap();

    let data = serde_json::json!({"exit_code": 0, "stdout": "small"});
    let mut d = draft("exec.result", data.clone());
    d.preview = Some(serde_json::json!({"never": "used"}));
    let mut writer = EventWriter::new(&session_dir);
    let event = writer.append(d, false).unwrap();

    assert_eq!(event.data.as_ref().unwrap(), &data, "inline data untouched");
    assert!(event.data_ref.is_none());
    assert!(event.truncated.is_none());
}

// --- Concurrency (acceptance criterion: 2 writers × 1000, zero torn lines) ---

#[test]
fn two_concurrent_writers_thousand_events_each_no_torn_lines() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("s1");
    std::fs::create_dir_all(&session_dir).unwrap();

    let run_id = RunId::new();
    std::thread::scope(|scope| {
        for t in 0..2 {
            let dir = session_dir.clone();
            scope.spawn(move || {
                let mut writer = EventWriter::new(&dir);
                for i in 0..1000 {
                    let mut d = draft("stress.event", serde_json::json!({"t": t, "i": i}));
                    d.run_id = run_id;
                    writer.append(d, false).unwrap();
                }
            });
        }
    });

    let outcome = read_session_events(&session_dir).unwrap();
    assert_eq!(outcome.skipped, 0, "zero torn or interleaved lines");
    assert_eq!(outcome.events.len(), 2000, "all 2000 events present");

    // Per-writer seq contiguous from 1.
    let mut by_writer: std::collections::HashMap<Uuid7, Vec<u64>> =
        std::collections::HashMap::new();
    for event in &outcome.events {
        by_writer.entry(event.writer).or_default().push(event.seq);
    }
    assert_eq!(by_writer.len(), 2);
    for (writer, mut seqs) in by_writer {
        seqs.sort_unstable();
        let expected: Vec<u64> = (1..=1000).collect();
        assert_eq!(seqs, expected, "writer {writer} seq contiguous");
    }
}

// --- Read path (spec §5.1 replay mechanics) ---

#[test]
fn read_resyncs_past_non_utf8_fragments() {
    // JSONL is self-synchronizing at the byte level (spec §3.2 defense in
    // depth): a corrupt non-UTF8 fragment is a counted skip, never a fatal
    // read error for the whole replay.
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("s1");
    std::fs::create_dir_all(&session_dir).unwrap();

    let mut writer = EventWriter::new(&session_dir);
    for i in 0..2 {
        writer
            .append(draft("build.step", serde_json::json!({"i": i})), false)
            .unwrap();
    }

    let segs = segment_files(&session_dir);
    let mut content = std::fs::read(&segs[0]).unwrap();
    content.extend_from_slice(&[0xff, 0xfe, 0x00, b'\n']); // foreign binary fragment
    std::fs::write(&segs[0], &content).unwrap();

    let outcome = read_session_events(&session_dir).expect("non-UTF8 line is not fatal");
    assert_eq!(outcome.events.len(), 2, "valid events survive");
    assert_eq!(outcome.skipped, 1, "binary fragment counted as a skip");
}

#[test]
fn read_merges_segments_and_counts_parse_skips() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("s1");
    std::fs::create_dir_all(&session_dir).unwrap();

    let mut writer = EventWriter::new(&session_dir);
    for i in 0..3 {
        writer
            .append(draft("build.step", serde_json::json!({"i": i})), false)
            .unwrap();
    }

    // Inject a torn line and a foreign fragment into the segment.
    let segs = segment_files(&session_dir);
    let mut content = std::fs::read_to_string(&segs[0]).unwrap();
    content.push_str("{\"v\":1,\"truncated-mid-obj\":\n");
    content.push_str("not json at all\n");
    std::fs::write(&segs[0], content).unwrap();

    let outcome = read_session_events(&session_dir).unwrap();
    assert_eq!(outcome.events.len(), 3, "valid events survive");
    assert_eq!(outcome.skipped, 2, "torn/foreign lines counted");

    // Events come back merged by (ts, writer, seq) — here one writer, so seq order.
    let seqs: Vec<u64> = outcome.events.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, vec![1, 2, 3]);
}

#[test]
fn read_missing_events_dir_is_empty_not_error() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("s1");
    std::fs::create_dir_all(&session_dir).unwrap();

    let outcome = read_session_events(&session_dir).unwrap();
    assert!(outcome.events.is_empty());
    assert_eq!(outcome.skipped, 0);
}

// --- lost+found (spec §7) ---

#[test]
fn lost_found_append_writes_fully_addressed_event() {
    let tmp = TempDir::new().unwrap();
    let tendr_root = tmp.path().join(".tendr");
    std::fs::create_dir_all(&tendr_root).unwrap();

    // Stamp an event without a session dir (orphan emitter).
    let session_dir = tmp.path().join("gone");
    std::fs::create_dir_all(&session_dir).unwrap();
    let mut writer = EventWriter::new(&session_dir);
    let event = writer
        .append(
            draft("hook.post_tool_use", serde_json::json!({"ok": 1})),
            false,
        )
        .unwrap();

    tendr::events::append_lost_found(&tendr_root, &event).unwrap();

    let lf = tendr_root.join("lost+found").join("events.jsonl");
    let lines = read_lines(&lf);
    assert_eq!(lines.len(), 1);
    let parsed: Event = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(parsed.id, event.id);
    assert_eq!(parsed.session.as_str(), "s1");
}
