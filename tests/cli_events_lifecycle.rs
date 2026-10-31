//! Sidecar lifecycle events with WAL ordering — spec §3.6, plan scope item 3.

mod harness;

use harness::{Observation, poll_until, tendr, wait_running, wait_terminal};
use std::time::Duration;
use tempfile::TempDir;

/// Read all events for a session, merged by (ts, writer, seq).
fn read_events(root: &TempDir, session: &str) -> Vec<serde_json::Value> {
    let events_dir = root
        .path()
        .join(format!(".tendr/sessions/default/{session}/events"));
    let mut segments: Vec<_> = std::fs::read_dir(&events_dir)
        .unwrap_or_else(|_| panic!("events dir missing for {session}"))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    segments.sort();

    let mut events: Vec<serde_json::Value> = Vec::new();
    for seg in segments {
        for line in std::fs::read_to_string(&seg).unwrap().lines() {
            if !line.is_empty() {
                events.push(serde_json::from_str(line).expect("event line parses"));
            }
        }
    }
    events.sort_by_key(|e| {
        (
            e["ts"].as_str().unwrap().to_owned(),
            e["writer"].as_str().unwrap().to_owned(),
            e["seq"].as_u64().unwrap(),
        )
    });
    events
}

fn kinds(events: &[serde_json::Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_owned())
        .collect()
}

/// Poll until the session's event log contains `kind`, or panic after 10 s.
fn wait_for_event_kind(root: &TempDir, session: &str, kind: &str) -> Vec<serde_json::Value> {
    let events_dir = root
        .path()
        .join(format!(".tendr/sessions/default/{session}/events"));
    poll_until(Duration::from_secs(10), Duration::from_millis(50), || {
        if !events_dir.exists() {
            return Observation::Pending("no events dir".into());
        }
        let events = read_events(root, session);
        if kinds(&events).iter().any(|k| k == kind) {
            Observation::Ready(events)
        } else {
            Observation::Pending(format!("{} events, none of kind {kind}", events.len()))
        }
    })
    .unwrap_or_else(|e| panic!("waiting for event kind {kind} in {session}: {e}"))
}

#[test]
fn normal_run_logs_starting_started_exited() {
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "s1", "--", "echo", "hi"])
        .assert()
        .success();
    let meta = wait_terminal(&root, "s1");
    let events = wait_for_event_kind(&root, "s1", "run.exited");

    assert_eq!(
        kinds(&events),
        ["run.starting", "run.started", "run.exited"]
    );

    // Envelope stamping: sidecar's writer is its run_id, seq contiguous from 1.
    let run_id = meta["run_id"].as_str().unwrap();
    for (i, event) in events.iter().enumerate() {
        assert_eq!(event["v"], 1);
        assert_eq!(event["run_id"].as_str().unwrap(), run_id);
        assert_eq!(event["writer"].as_str().unwrap(), run_id);
        assert_eq!(event["seq"].as_u64().unwrap(), i as u64 + 1);
        assert_eq!(event["source"], "tendr.sidecar");
        assert_eq!(event["namespace"], "default");
        assert_eq!(event["session"], "s1");
        assert_eq!(event["gen"], 1);
        // Occurrence-time RFC 3339 µs Z timestamps.
        let ts = event["ts"].as_str().unwrap();
        assert_eq!(ts.len(), 27, "fixed-width ts: {ts}");
        assert!(ts.ends_with('Z'));
    }

    // Watch-vocabulary data shapes plus provenance (spec §1 example (a)).
    assert_eq!(events[0]["data"]["status"], "Starting");
    assert_eq!(events[1]["data"]["status"], "Running");
    assert_eq!(events[2]["data"]["status"], "Exited");
    assert_eq!(events[2]["data"]["reason"], "ExitedOk");
    assert_eq!(events[2]["data"]["exit_code"], 0);
    for event in &events {
        assert_eq!(event["data"]["provenance"], "direct");
    }
}

#[test]
fn nonzero_exit_logs_exit_code() {
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "s1", "--", "false"])
        .assert()
        .success();
    wait_terminal(&root, "s1");
    let events = wait_for_event_kind(&root, "s1", "run.exited");

    let exited = events.last().unwrap();
    assert_eq!(exited["data"]["status"], "Exited");
    assert_eq!(exited["data"]["reason"], "ExitedError");
    assert_eq!(exited["data"]["exit_code"], 1);
}

#[test]
fn spawn_failure_logs_spawn_failed() {
    let root = TempDir::new().unwrap();
    // Spawn failure is a normal terminal outcome for start; ignore its exit.
    let _ = tendr(&root)
        .args(["start", "s1", "--", "/nonexistent-cmd-tendr-test"])
        .assert();
    wait_terminal(&root, "s1");
    let events = wait_for_event_kind(&root, "s1", "run.spawn_failed");

    assert_eq!(kinds(&events), ["run.starting", "run.spawn_failed"]);
    assert_eq!(events[1]["data"]["status"], "SpawnFailed");
    assert_eq!(events[1]["data"]["provenance"], "direct");
}

#[test]
fn graceful_kill_logs_run_killed() {
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "s1", "--", "sleep", "30"])
        .assert()
        .success();
    wait_running(&root, "s1");
    tendr(&root).args(["kill", "s1"]).assert().success();
    wait_terminal(&root, "s1");
    let events = wait_for_event_kind(&root, "s1", "run.killed");

    let killed = events.last().unwrap();
    assert_eq!(killed["kind"], "run.killed");
    assert_eq!(killed["data"]["status"], "Exited");
    assert_eq!(killed["data"]["provenance"], "direct");
}

#[test]
fn failed_dependency_logs_dependency_failed() {
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "dep", "--", "false"])
        .assert()
        .success();
    wait_terminal(&root, "dep");

    tendr(&root)
        .args(["start", "s1", "--after", "dep", "--", "echo", "hi"])
        .assert()
        .success();
    wait_terminal(&root, "s1");
    let events = wait_for_event_kind(&root, "s1", "run.dependency_failed");

    assert_eq!(kinds(&events), ["run.starting", "run.dependency_failed"]);
    assert_eq!(events[1]["data"]["status"], "DependencyFailed");
    assert_eq!(events[1]["data"]["reason"], "Failed");
    assert_eq!(events[1]["data"]["provenance"], "direct");
}

// --- WAL ordering: event append precedes meta write (spec §3.6) ---
// Crash injection via TENDR_TEST_ABORT is compiled into debug sidecars only.

#[test]
fn crash_after_terminal_event_leaves_event_without_meta() {
    let root = TempDir::new().unwrap();
    tendr(&root)
        .env("TENDR_TEST_ABORT", "before_terminal_meta")
        .args(["start", "s1", "--", "echo", "hi"])
        .assert()
        .success();

    // The sidecar aborts after appending+syncing run.exited, before meta.
    let events = wait_for_event_kind(&root, "s1", "run.exited");
    assert!(kinds(&events).contains(&"run.exited".to_owned()));

    let meta_path = root.path().join(".tendr/sessions/default/s1/meta.json");
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    assert_eq!(
        meta["status"], "Running",
        "meta must still be non-terminal: the event log leads, meta lags"
    );
}

#[test]
fn crash_before_terminal_event_leaves_neither() {
    let root = TempDir::new().unwrap();
    tendr(&root)
        .env("TENDR_TEST_ABORT", "before_terminal_event")
        .args(["start", "s1", "--", "echo", "hi"])
        .assert()
        .success();

    // No sleep: acquiring the session lock proves the aborted sidecar is gone, and
    // the guard is held while we read the log, so we cannot observe a transient
    // finalization window. Construction already proves Running + Unlocked;
    // re-asserting the status here would merely duplicate the constructor's
    // invariant.
    let _orphan = harness::wait_orphaned_running(&root, "s1");

    let events = read_events(&root, "s1");
    assert!(
        !kinds(&events).iter().any(|k| k == "run.exited"),
        "no terminal event was appended"
    );
}

/// An unwritable event log must not eat the terminal record: the append
/// failure is salvaged to ~/.tendr/lost+found/events.jsonl and recorded as
/// a meta warning, while supervision still writes terminal meta.
#[test]
fn unwritable_event_log_salvages_terminal_event_to_lost_found() {
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "s1", "--", "sleep", "2"])
        .assert()
        .success();
    wait_running(&root, "s1");

    // Sabotage: replace the events dir with a regular file so every further
    // append fails while meta.json stays writable.
    let session_dir = root.path().join(".tendr/sessions/default/s1");
    std::fs::remove_dir_all(session_dir.join("events")).unwrap();
    std::fs::write(session_dir.join("events"), "not a dir").unwrap();

    let meta = wait_terminal(&root, "s1");
    assert_eq!(meta["status"], "Exited");
    assert_eq!(meta["reason"], "ExitedOk");
    let warnings = meta["warnings"].as_array().expect("append failure warned");
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap_or("").contains("event log append failed")),
        "warnings: {warnings:?}"
    );

    // The terminal record survived in lost+found, fully addressed.
    let lf = root.path().join(".tendr/lost+found/events.jsonl");
    let content = std::fs::read_to_string(&lf).expect("lost+found log exists");
    let salvaged: Vec<serde_json::Value> = content
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let exited = salvaged
        .iter()
        .find(|e| e["kind"] == "run.exited")
        .expect("terminal event salvaged");
    assert_eq!(exited["session"], "s1");
    assert_eq!(exited["run_id"], meta["run_id"]);
    assert_eq!(exited["data"]["reason"], "ExitedOk");
    assert_eq!(exited["source"], "tendr.sidecar");
}
