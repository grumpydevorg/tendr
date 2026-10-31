//! `tendr events` — replay-only read surface (spec §5.1, slice 1 scope).

mod harness;

use harness::{tendr, wait_terminal, wait_terminal_ns};
use tempfile::TempDir;

fn finished_session(root: &TempDir, name: &str) {
    tendr(root)
        .args(["start", name, "--", "echo", "hi"])
        .assert()
        .success();
    wait_terminal(root, name);
}

fn parse_ndjson(stdout: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("NDJSON line parses"))
        .collect()
}

#[test]
fn events_replays_session_lifecycle_as_ndjson() {
    let root = TempDir::new().unwrap();
    finished_session(&root, "s1");

    let output = tendr(&root)
        .args(["events", "--session", "default/s1"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let events = parse_ndjson(&output.stdout);
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["run.starting", "run.started", "run.exited"]);
    // Envelope NDJSON: full events, not a projection.
    assert_eq!(events[0]["v"], 1);
    assert!(events[0]["id"].is_string());
    assert_eq!(events[2]["data"]["exit_code"], 0);
}

#[test]
fn events_merges_all_sessions_by_timestamp() {
    let root = TempDir::new().unwrap();
    finished_session(&root, "a");
    finished_session(&root, "b");

    let output = tendr(&root).args(["events"]).output().unwrap();
    assert!(output.status.success());

    let events = parse_ndjson(&output.stdout);
    let sessions: std::collections::HashSet<&str> = events
        .iter()
        .map(|e| e["session"].as_str().unwrap())
        .collect();
    assert_eq!(sessions, ["a", "b"].into_iter().collect());

    // Deterministic merge: (ts, writer, seq) ascending across sessions.
    let keys: Vec<(String, String, u64)> = events
        .iter()
        .map(|e| {
            (
                e["ts"].as_str().unwrap().to_owned(),
                e["writer"].as_str().unwrap().to_owned(),
                e["seq"].as_u64().unwrap(),
            )
        })
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
}

#[test]
fn events_kind_prefix_filter() {
    let root = TempDir::new().unwrap();
    finished_session(&root, "s1");
    tendr(&root)
        .args(["emit", "--kind", "hook.post_tool_use", "--session", "s1"])
        .assert()
        .success();

    // Prefix filter: the plan's canonical validation is --kind hook.
    let output = tendr(&root)
        .args(["events", "--kind", "hook."])
        .output()
        .unwrap();
    let events = parse_ndjson(&output.stdout);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["kind"], "hook.post_tool_use");
    assert_eq!(events[0]["source"], "user.emit");

    // Exact kind is a prefix of itself.
    let output = tendr(&root)
        .args(["events", "--kind", "run.exited"])
        .output()
        .unwrap();
    let events = parse_ndjson(&output.stdout);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["kind"], "run.exited");
}

#[test]
fn events_source_prefix_filter() {
    let root = TempDir::new().unwrap();
    finished_session(&root, "s1");
    tendr(&root)
        .args([
            "emit",
            "--kind",
            "hook.pre_tool_use",
            "--source",
            "claude.hook",
            "--session",
            "s1",
        ])
        .assert()
        .success();

    let output = tendr(&root)
        .args(["events", "--source", "claude."])
        .output()
        .unwrap();
    let events = parse_ndjson(&output.stdout);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["source"], "claude.hook");
}

#[test]
fn events_namespace_filter() {
    let root = TempDir::new().unwrap();
    finished_session(&root, "s1");
    tendr(&root)
        .args(["start", "s2", "--namespace", "other", "--", "echo", "hi"])
        .assert()
        .success();
    wait_terminal_ns(&root, "other", "s2");

    let output = tendr(&root)
        .args(["events", "--namespace", "other"])
        .output()
        .unwrap();
    let events = parse_ndjson(&output.stdout);
    assert!(!events.is_empty());
    assert!(events.iter().all(|e| e["namespace"] == "other"));
}

#[test]
fn events_session_filter_is_repeatable() {
    let root = TempDir::new().unwrap();
    finished_session(&root, "a");
    finished_session(&root, "b");
    finished_session(&root, "c");

    let output = tendr(&root)
        .args(["events", "--session", "a", "--session", "default/b"])
        .output()
        .unwrap();
    let events = parse_ndjson(&output.stdout);
    let sessions: std::collections::HashSet<&str> = events
        .iter()
        .map(|e| e["session"].as_str().unwrap())
        .collect();
    assert_eq!(sessions, ["a", "b"].into_iter().collect());
}

#[test]
fn events_strict_exits_65_on_parse_skips() {
    let root = TempDir::new().unwrap();
    finished_session(&root, "s1");

    // Corrupt the log with a torn line.
    let events_dir = root.path().join(".tendr/sessions/default/s1/events");
    let seg = std::fs::read_dir(&events_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .unwrap();
    let mut content = std::fs::read_to_string(&seg).unwrap();
    content.push_str("{\"v\":1,\"torn\n");
    std::fs::write(&seg, content).unwrap();

    // Default: parse-skips tolerated, valid events still replay.
    let output = tendr(&root)
        .args(["events", "--session", "s1"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(parse_ndjson(&output.stdout).len(), 3);

    // --strict: parse-skips ⇒ exit 65.
    tendr(&root)
        .args(["events", "--session", "s1", "--strict"])
        .assert()
        .code(65);
}

#[test]
fn events_empty_log_succeeds_with_no_output() {
    let root = TempDir::new().unwrap();
    let output = tendr(&root).args(["events"]).output().unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
}
