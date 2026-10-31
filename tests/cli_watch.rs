mod harness;

use harness::{tendr, wait_terminal, wait_terminal_ns};
use std::sync::Mutex;
use std::time::Duration;
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

#[test]
fn watch_emits_initial_state_snapshot() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Start a session and let it finish.
    tendr(&root)
        .args(["start", "snap-echo", "--", "echo", "hi"])
        .output()
        .unwrap();
    wait_terminal(&root, "snap-echo");

    // The initial snapshot is flushed before the follower signals readiness.
    let follower = harness::ReadyFollower::spawn(&root, "watch", &["--events"]);
    follower.read_until(Duration::from_secs(10), |r| {
        r["session"] == "snap-echo" && r["name"] == "run.exited"
    });
    let records = follower.records();
    follower.stop();

    assert!(!records.is_empty(), "watch should emit at least one line");
    for event in &records {
        assert_eq!(event["source"], "tendr.sidecar");
        assert_eq!(event["kind"], "run");
        assert!(
            event["ts"].is_f64() || event["ts"].is_u64(),
            "ts should be a number"
        );
        assert!(
            event["namespace"].is_string(),
            "namespace should be present"
        );
        assert!(event["session"].is_string(), "session should be present");
        assert!(event["run_id"].is_string(), "run_id should be present");
        assert!(event["name"].is_string(), "name should be present");
    }

    // The snapshot should be for snap-echo, a terminal session -> run.exited.
    let first = &records[0];
    assert_eq!(first["session"], "snap-echo");
    assert_eq!(first["namespace"], "default");
    assert_eq!(first["kind"], "run");
    assert_eq!(first["name"], "run.exited");
}

#[test]
fn watch_emits_log_events() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Start a session that produces output.
    tendr(&root)
        .args(["start", "log-echo", "--", "echo", "hello-watch"])
        .output()
        .unwrap();
    wait_terminal(&root, "log-echo");

    // Watch with --logs only; the log.stdout snapshot must surface.
    let follower = harness::ReadyFollower::spawn(&root, "watch", &["--logs"]);
    let rec = follower.read_until(Duration::from_secs(10), |r| {
        r["kind"] == "log"
            && r["name"] == "log.stdout"
            && r["data"]["content"]
                .as_str()
                .is_some_and(|c| c.contains("hello-watch"))
    });
    follower.stop();
    assert!(
        rec["data"]["content"]
            .as_str()
            .unwrap()
            .contains("hello-watch"),
        "should find a log.stdout event with 'hello-watch'"
    );
}

#[test]
fn watch_filters_by_namespace() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Create sessions in two namespaces.
    tendr(&root)
        .args([
            "start",
            "alpha",
            "--namespace",
            "ns-a",
            "--",
            "echo",
            "a-out",
        ])
        .output()
        .unwrap();
    tendr(&root)
        .args([
            "start",
            "beta",
            "--namespace",
            "ns-b",
            "--",
            "echo",
            "b-out",
        ])
        .output()
        .unwrap();
    wait_terminal_ns(&root, "ns-a", "alpha");
    wait_terminal_ns(&root, "ns-b", "beta");

    // Watch only ns-a.
    let follower =
        harness::ReadyFollower::spawn(&root, "watch", &["--namespace", "ns-a", "--events"]);
    follower.read_until(Duration::from_secs(10), |r| {
        r["session"] == "alpha" && r["kind"] == "run"
    });
    let records = follower.records();
    follower.stop();

    assert!(!records.is_empty(), "should emit events for ns-a");
    for event in &records {
        assert_eq!(
            event["namespace"], "ns-a",
            "all events should be from ns-a, got: {event}"
        );
    }
}

#[test]
fn watch_from_now_skips_existing() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Start and complete a session before running watch.
    tendr(&root)
        .args(["start", "old-session", "--", "echo", "old"])
        .output()
        .unwrap();
    wait_terminal(&root, "old-session");

    let follower = harness::ReadyFollower::spawn(&root, "watch", &["--from-now"]);

    // A sentinel started strictly after readiness: once its run event surfaces,
    // the stream has advanced past any (incorrect) emission for the pre-existing
    // session, so an absence assertion is now sound rather than a race.
    tendr(&root)
        .args(["start", "sentinel", "--", "echo", "sentinel-out"])
        .output()
        .unwrap();
    wait_terminal(&root, "sentinel");
    follower.read_until(Duration::from_secs(10), |r| {
        r["session"] == "sentinel" && r["kind"] == "run"
    });
    let records = follower.records();
    follower.stop();

    for event in &records {
        if event["kind"] == "run" {
            assert_ne!(
                event["session"], "old-session",
                "from-now must not emit run events for existing terminal sessions, got: {event}"
            );
        }
        // Log lines from existing content should also be skipped.
        if event["kind"] == "log" {
            let content = event["data"]["content"].as_str().unwrap_or("");
            assert!(
                !content.contains("old"),
                "from-now should skip existing log lines, got: {event}"
            );
        }
    }
}

#[test]
fn watch_both_events_and_logs_by_default() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Start a session that produces output.
    tendr(&root)
        .args(["start", "both-echo", "--", "echo", "dual-output"])
        .output()
        .unwrap();
    wait_terminal(&root, "both-echo");

    // No --events/--logs -> both. The run snapshot precedes the log snapshot,
    // so waiting for the log proves both are downstream.
    let follower = harness::ReadyFollower::spawn(&root, "watch", &[]);
    follower.read_until(Duration::from_secs(10), |r| {
        r["kind"] == "log" && r["session"] == "both-echo"
    });
    let records = follower.records();
    follower.stop();

    let mut has_run = false;
    let mut has_log = false;
    for event in &records {
        match event["kind"].as_str() {
            Some("run") => has_run = true,
            Some("log") => has_log = true,
            _ => {}
        }
    }
    assert!(
        has_run,
        "default watch should emit run events, got:\n{records:?}"
    );
    assert!(
        has_log,
        "default watch should emit log events, got:\n{records:?}"
    );
}

/// Readiness handshake for `watch`: the ready-file is published only after the
/// initial scan/snapshot, so a session started strictly afterwards is a genuine
/// post-baseline discovery and must surface.
#[test]
fn watch_from_now_surfaces_new_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let follower = harness::ReadyFollower::spawn(&root, "watch", &["--from-now"]);
    // Baseline established: a session started now is not skipped by --from-now.
    tendr(&root)
        .args(["start", "after-watch", "--", "echo", "post-watch-output"])
        .output()
        .unwrap();
    wait_terminal(&root, "after-watch");

    let rec = follower.read_until(Duration::from_secs(10), |r| {
        r["kind"] == "run" && r["session"] == "after-watch"
    });
    assert_eq!(rec["session"], "after-watch");
    follower.stop();
}

#[test]
fn watch_detects_replace_and_resets_log_offset() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Start a session that produces output.
    tendr(&root)
        .args(["start", "replace-watch", "--", "echo", "first-run-output"])
        .output()
        .unwrap();
    wait_terminal(&root, "replace-watch");

    // Watch's initial snapshot of the first run is flushed before readiness.
    let follower = harness::ReadyFollower::spawn(&root, "watch", &[]);

    // Replace with a new run (new run_id) that produces different output.
    tendr(&root)
        .args([
            "start",
            "replace-watch",
            "--replace",
            "--",
            "echo",
            "second-run-output",
        ])
        .output()
        .unwrap();
    wait_terminal(&root, "replace-watch");

    // The replaced run's log surfacing proves the offset was reset on run_id
    // change; read_until is the assertion.
    follower.read_until(Duration::from_secs(10), |r| {
        r["kind"] == "log"
            && r["data"]["content"]
                .as_str()
                .is_some_and(|s| s.contains("second-run-output"))
    });
    follower.stop();
}
