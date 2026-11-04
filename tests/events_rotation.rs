//! Event segment rotation (spec §3.3, docs/architecture/07-event-protocol.md):
//! past the segment size a writer starts a new segment with `segment.opened`,
//! and every reader still returns every record, in order.

mod harness;
use harness::Home;

use std::collections::BTreeMap;
use std::path::PathBuf;

use harness::{ReadyFollower, tendr, wait_terminal};

/// Small enough that a few emits cross it, large enough that the three
/// lifecycle events of a short run fit in the first segment.
const SEGMENT_BYTES: &str = "4096";
const EMITS: usize = 24;

/// A `tendr` command whose event writers rotate at [`SEGMENT_BYTES`].
fn rotating(root: &Home) -> assert_cmd::Command {
    let mut cmd = tendr(root);
    cmd.env("TENDR_TEST_EVENT_SEGMENT_BYTES", SEGMENT_BYTES);
    cmd
}

fn parse_ndjson(stdout: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("NDJSON line parses"))
        .collect()
}

fn segments(root: &Home, session: &str) -> Vec<PathBuf> {
    let dir = root
        .path()
        .join(format!(".tendr/sessions/default/{session}/events"));
    let mut segs: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    segs.sort();
    segs
}

fn kinds(records: &[serde_json::Value]) -> Vec<String> {
    records
        .iter()
        .map(|r| r["kind"].as_str().unwrap().to_owned())
        .collect()
}

fn ids(records: &[serde_json::Value]) -> Vec<String> {
    records
        .iter()
        .filter(|r| r["kind"] != "cursor.bookmark")
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

fn test_kinds(records: &[serde_json::Value]) -> Vec<String> {
    kinds(records)
        .into_iter()
        .filter(|k| k.starts_with("test."))
        .collect()
}

fn expected_test_kinds() -> Vec<String> {
    (0..EMITS).map(|i| format!("test.n{i:03}")).collect()
}

/// Emit [`EMITS`] padded events into `s1`, one `tendr emit` process each.
fn emit_past_several_segments(root: &Home) {
    let pad = "x".repeat(300);
    for i in 0..EMITS {
        rotating(root)
            .args([
                "emit",
                "--kind",
                &format!("test.n{i:03}"),
                "--session",
                "s1",
                "--data",
                &format!(r#"{{"i":{i},"pad":"{pad}"}}"#),
            ])
            .assert()
            .success();
    }
}

fn start_finished_session(root: &Home) {
    rotating(root)
        .args(["start", "s1", "--", "echo", "hi"])
        .assert()
        .success();
    wait_terminal(root, "s1");
}

#[test]
fn replay_follow_and_a_cursor_from_before_rotation_return_every_record() {
    let root = Home::new();
    start_finished_session(&root);
    assert_eq!(
        segments(&root, "s1").len(),
        1,
        "the run's lifecycle events fit in one segment"
    );

    // A cursor taken before any rotation, and a follower already reading.
    let before = tendr(&root).args(["events", "--cursors"]).output().unwrap();
    assert!(before.status.success());
    let before = parse_ndjson(&before.stdout);
    let bookmark = before.last().unwrap();
    assert_eq!(bookmark["kind"], "cursor.bookmark");
    let token = bookmark["cursor"].as_str().unwrap().to_owned();
    let follower = ReadyFollower::spawn(&root, "events", &["--follow"]);

    emit_past_several_segments(&root);

    let segs = segments(&root, "s1");
    assert!(
        segs.len() >= 3,
        "{EMITS} padded events cross {SEGMENT_BYTES} bytes more than once: {segs:?}"
    );

    // On disk: each later segment starts with segment.opened naming the one
    // before it, and no segment was renamed away.
    for pair in segs.windows(2) {
        let first = std::fs::read_to_string(&pair[1]).unwrap();
        let opened: serde_json::Value =
            serde_json::from_str(first.lines().next().unwrap()).unwrap();
        assert_eq!(opened["kind"], "segment.opened", "{}", pair[1].display());
        let name = |p: &PathBuf| p.file_name().unwrap().to_str().unwrap().to_owned();
        assert_eq!(opened["data"]["previous"], name(&pair[0]));
        assert_eq!(opened["data"]["segment"], name(&pair[1]));
        assert!(opened["data"]["previous_bytes"].as_u64().unwrap() >= 4096);
    }

    // Replay returns every event once, in order, and the rotations.
    let replay = tendr(&root).arg("events").output().unwrap();
    assert!(replay.status.success());
    let replay = parse_ndjson(&replay.stdout);
    assert_eq!(test_kinds(&replay), expected_test_kinds());
    let opened = kinds(&replay)
        .iter()
        .filter(|k| *k == "segment.opened")
        .count();
    assert_eq!(opened, segs.len() - 1, "one segment.opened per new segment");

    // Each writer's seq stays contiguous from 1 across rotation, so a
    // consumer's gap detection sees no loss.
    let mut by_writer: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for record in &replay {
        by_writer
            .entry(record["writer"].as_str().unwrap().to_owned())
            .or_default()
            .push(record["seq"].as_u64().unwrap());
    }
    for (writer, mut seqs) in by_writer {
        seqs.sort_unstable();
        let contiguous: Vec<u64> = (1..=seqs.len() as u64).collect();
        assert_eq!(seqs, contiguous, "writer {writer}");
    }

    // The follower crossed every boundary and saw the same stream.
    follower.read_until(|r| r["kind"] == format!("test.n{:03}", EMITS - 1));
    assert_eq!(ids(&follower.records()), ids(&replay));
    follower.stop();

    // The pre-rotation cursor still resolves and resumes exactly after the
    // records it covered, through every later segment.
    let resumed = tendr(&root)
        .args(["events", "--from-cursor", &token])
        .output()
        .unwrap();
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    let covered = ids(&before).len();
    assert_eq!(ids(&parse_ndjson(&resumed.stdout)), ids(&replay)[covered..]);
}

#[test]
fn query_reads_every_rotated_segment() {
    if !harness::duckdb_or_skip() {
        return;
    }
    let root = Home::new();
    start_finished_session(&root);
    emit_past_several_segments(&root);
    let segs = segments(&root, "s1");
    assert!(segs.len() >= 3, "{segs:?}");

    let out = tendr(&root)
        .args([
            "query",
            "SELECT 'n=' || COUNT(*) || ';' FROM events WHERE kind LIKE 'test.%' \
             UNION ALL SELECT 'opened=' || COUNT(*) || ';' FROM events WHERE kind = 'segment.opened'",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(&format!("n={EMITS};")), "{stdout}");
    assert!(
        stdout.contains(&format!("opened={};", segs.len() - 1)),
        "{stdout}"
    );
}
