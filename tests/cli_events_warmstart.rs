//! `tendr events` warm-start flags — `--since`, `--last`, `--from-now`,
//! and their mutual exclusion (slice 2 plan, scope item 2).

mod harness;
use harness::Home;

use harness::{tendr, wait_terminal};

fn finished_session(root: &Home, name: &str) {
    tendr(root)
        .args(["start", name, "--", "echo", "hi"])
        .assert()
        .success();
    wait_terminal(root, name);
}

fn emit(root: &Home, session: &str, kind: &str) {
    tendr(root)
        .args(["emit", "--kind", kind, "--session", session])
        .assert()
        .success();
}

fn parse_ndjson(stdout: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("NDJSON line parses"))
        .collect()
}

fn replay(root: &Home, args: &[&str]) -> Vec<serde_json::Value> {
    let output = tendr(root)
        .arg("events")
        .args(args)
        .output()
        .expect("events runs");
    assert!(output.status.success(), "events exits 0");
    parse_ndjson(&output.stdout)
}

#[test]
fn last_returns_exactly_the_last_n_by_merge_order() {
    let root = Home::new();
    finished_session(&root, "s1");
    for i in 0..4 {
        emit(&root, "s1", &format!("test.e{i}"));
    }

    let all = replay(&root, &[]);
    assert_eq!(all.len(), 7, "3 lifecycle + 4 emitted");

    let tail = replay(&root, &["--last", "5"]);
    assert_eq!(tail.len(), 5);
    let expected: Vec<&serde_json::Value> = all.iter().skip(2).collect();
    let got: Vec<&serde_json::Value> = tail.iter().collect();
    assert_eq!(got, expected, "--last 5 is the merge-order tail");

    // N larger than the log returns everything.
    let generous = replay(&root, &["--last", "100"]);
    assert_eq!(generous.len(), 7);
}

#[test]
fn since_excludes_earlier_events() {
    let root = Home::new();
    finished_session(&root, "s1");
    emit(&root, "s1", "test.late");

    let all = replay(&root, &[]);
    let since = all[2]["ts"].as_str().unwrap().to_owned();

    let filtered = replay(&root, &["--since", &since]);
    // ts is fixed-width RFC 3339, so string compare is chronological.
    let expected: Vec<&str> = all
        .iter()
        .filter(|e| e["ts"].as_str().unwrap() >= since.as_str())
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    let got: Vec<&str> = filtered.iter().map(|e| e["id"].as_str().unwrap()).collect();
    assert_eq!(got, expected);
    assert!(filtered.len() < all.len(), "--since excluded something");
}

#[test]
fn since_accepts_second_precision_utc() {
    let root = Home::new();
    finished_session(&root, "s1");

    // A whole-second timestamp far in the future excludes everything.
    let none = replay(&root, &["--since", "2100-01-01T00:00:00Z"]);
    assert!(none.is_empty());

    // And one far in the past includes everything.
    let all = replay(&root, &["--since", "2000-01-01T00:00:00Z"]);
    assert_eq!(all.len(), 3);
}

#[test]
fn since_rejects_malformed_timestamps_as_usage_error() {
    let root = Home::new();
    tendr(&root)
        .args(["events", "--since", "yesterday"])
        .assert()
        .code(2);
}

#[test]
fn from_now_without_follow_prints_nothing() {
    let root = Home::new();
    finished_session(&root, "s1");

    let output = tendr(&root)
        .args(["events", "--from-now"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        output.stdout.is_empty(),
        "--from-now skips all existing history"
    );
}

#[test]
fn warm_start_flags_are_mutually_exclusive() {
    let root = Home::new();
    let combos: &[&[&str]] = &[
        &["--from-now", "--since", "2026-01-01T00:00:00Z"],
        &["--from-now", "--last", "5"],
        &["--since", "2026-01-01T00:00:00Z", "--last", "5"],
        &["--from-cursor", "abc", "--from-now"],
        &["--from-cursor", "abc", "--since", "2026-01-01T00:00:00Z"],
        &["--from-cursor", "abc", "--last", "5"],
    ];
    for combo in combos {
        tendr(&root).arg("events").args(*combo).assert().code(2);
    }
}
