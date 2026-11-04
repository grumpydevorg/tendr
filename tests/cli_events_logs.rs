//! `tendr events --include-logs` — read-time projection of output.log
//! O/E lines as derived events (spec §5.1, slice 2 plan scope item 5).

mod harness;
use harness::Home;

use std::collections::BTreeMap;
use std::path::PathBuf;

use harness::{tendr, wait_terminal};

fn parse_ndjson(stdout: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("NDJSON line parses"))
        .collect()
}

fn segment_bytes(root: &Home, session: &str) -> BTreeMap<PathBuf, Vec<u8>> {
    let events_dir = root
        .path()
        .join(format!(".tendr/sessions/default/{session}/events"));
    std::fs::read_dir(&events_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .map(|p| {
            let bytes = std::fs::read(&p).unwrap();
            (p, bytes)
        })
        .collect()
}

#[test]
fn include_logs_interleaves_derived_records_in_timestamp_order() {
    let root = Home::new();
    tendr(&root)
        .args([
            "start",
            "s1",
            "--",
            "sh",
            "-c",
            "echo out-line; echo err-line 1>&2",
        ])
        .assert()
        .success();
    wait_terminal(&root, "s1");

    let before = segment_bytes(&root, "s1");

    let output = tendr(&root)
        .args(["events", "--include-logs"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let records = parse_ndjson(&output.stdout);

    // Derived stdout and stderr records are present with the projected shape.
    let stdout_rec = records
        .iter()
        .find(|r| r["kind"] == "log.stdout")
        .expect("a log.stdout derived record");
    assert_eq!(stdout_rec["derived"], true);
    assert_eq!(stdout_rec["data"]["content"], "out-line");
    assert_eq!(stdout_rec["namespace"], "default");
    assert_eq!(stdout_rec["session"], "s1");
    assert_eq!(stdout_rec["source"], "tendr.sidecar");
    assert!(stdout_rec["run_id"].is_string());
    // No stored identity on derived records.
    assert!(stdout_rec.get("id").is_none());
    assert!(stdout_rec.get("writer").is_none());
    assert!(stdout_rec.get("seq").is_none());

    let stderr_rec = records
        .iter()
        .find(|r| r["kind"] == "log.stderr")
        .expect("a log.stderr derived record");
    assert_eq!(stderr_rec["data"]["content"], "err-line");

    // Stored lifecycle events still replay alongside.
    assert!(records.iter().any(|r| r["kind"] == "run.exited"));

    // Single merged stream, ordered by ts (fixed-width strings sort
    // chronologically).
    let timestamps: Vec<&str> = records.iter().map(|r| r["ts"].as_str().unwrap()).collect();
    let mut sorted = timestamps.clone();
    sorted.sort_unstable();
    assert_eq!(timestamps, sorted, "merged stream is in timestamp order");

    // Projection is read-time only: stored segments byte-identical.
    let after = segment_bytes(&root, "s1");
    assert_eq!(before, after, "segments untouched by --include-logs");
}

#[test]
fn include_logs_respects_kind_prefix_filter() {
    let root = Home::new();
    tendr(&root)
        .args(["start", "s1", "--", "echo", "only-line"])
        .assert()
        .success();
    wait_terminal(&root, "s1");

    let output = tendr(&root)
        .args(["events", "--include-logs", "--kind", "log."])
        .output()
        .unwrap();
    assert!(output.status.success());
    let records = parse_ndjson(&output.stdout);
    assert!(!records.is_empty());
    assert!(
        records
            .iter()
            .all(|r| r["kind"].as_str().unwrap().starts_with("log.")),
        "kind filter applies to derived records too"
    );
}

#[test]
fn logs_are_not_projected_without_the_flag() {
    let root = Home::new();
    tendr(&root)
        .args(["start", "s1", "--", "echo", "quiet"])
        .assert()
        .success();
    wait_terminal(&root, "s1");

    let output = tendr(&root).args(["events"]).output().unwrap();
    assert!(output.status.success());
    let records = parse_ndjson(&output.stdout);
    assert!(
        records
            .iter()
            .all(|r| !r["kind"].as_str().unwrap().starts_with("log.")),
        "no derived log records without --include-logs"
    );
}

#[test]
fn include_logs_with_last_counts_the_merged_stream() {
    let root = Home::new();
    tendr(&root)
        .args(["start", "s1", "--", "echo", "tail-me"])
        .assert()
        .success();
    wait_terminal(&root, "s1");

    let all = parse_ndjson(
        &tendr(&root)
            .args(["events", "--include-logs"])
            .output()
            .unwrap()
            .stdout,
    );
    assert!(all.len() >= 4, "3 lifecycle + at least 1 log line");

    let tail = parse_ndjson(
        &tendr(&root)
            .args(["events", "--include-logs", "--last", "2"])
            .output()
            .unwrap()
            .stdout,
    );
    assert_eq!(tail.len(), 2);
    let expected: Vec<&serde_json::Value> = all.iter().skip(all.len() - 2).collect();
    let got: Vec<&serde_json::Value> = tail.iter().collect();
    assert_eq!(got, expected, "--last tails the merged stream");
}
