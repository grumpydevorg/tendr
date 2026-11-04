//! `tendr watch` re-backed by the event log (spec §5.3, slice 2 plan
//! scope item 7). Output shape frozen — these tests assert the *gains*
//! (un-collapsed transitions, true timestamps, provenance stripped) and
//! the legacy fallback; cli_watch.rs continues to pin the frozen shape.

mod harness;

use std::sync::Mutex;
use std::time::{Duration, Instant};

use harness::{HANG, tendr, wait_terminal};
use tempfile::TempDir;
use tendr::model::event::EventTimestamp;

static SERIAL: Mutex<()> = Mutex::new(());

fn run_names(records: &[serde_json::Value], session: &str) -> Vec<String> {
    records
        .iter()
        .filter(|r| r["kind"] == "run" && r["session"] == session)
        .map(|r| r["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn fast_exit_session_shows_all_three_transitions_uncollapsed() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Watch starts (baseline established) BEFORE the session exists.
    let follower = harness::ReadyFollower::spawn(&root, "watch", &["--events"]);

    // A session that exits faster than any meta-diff poll could observe.
    tendr(&root)
        .args(["start", "fast", "--", "echo", "hi"])
        .assert()
        .success();
    wait_terminal(&root, "fast");
    follower.read_until(|r| r["session"] == "fast" && r["name"] == "run.exited");
    let records = follower.records();
    follower.stop();

    assert_eq!(
        run_names(&records, "fast"),
        ["run.starting", "run.started", "run.exited"],
        "event-log backing un-collapses fast transitions, got: {records:?}"
    );

    // Frozen shape, real payloads: f64 ts, kind/name split, legacy data —
    // and the event's provenance field stripped at projection.
    for record in records.iter().filter(|r| r["kind"] == "run") {
        assert!(record["ts"].is_f64() || record["ts"].is_u64());
        assert_eq!(record["source"], "tendr.sidecar");
        assert!(record["data"]["status"].is_string());
        assert!(
            record["data"].get("provenance").is_none(),
            "provenance is event-log detail, not watch surface: {record}"
        );
    }
}

#[test]
fn preexisting_session_still_gets_current_state_snapshot_only() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "done", "--", "echo", "hi"])
        .assert()
        .success();
    wait_terminal(&root, "done");

    let follower = harness::ReadyFollower::spawn(&root, "watch", &["--events"]);
    follower.read_until(|r| r["session"] == "done" && r["name"] == "run.exited");
    let records = follower.records();
    follower.stop();

    assert_eq!(
        run_names(&records, "done"),
        ["run.exited"],
        "pre-existing sessions keep watch's snapshot contract, got: {records:?}"
    );
}

#[test]
fn rebacked_snapshot_carries_the_true_timestamp() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "stamped", "--", "echo", "hi"])
        .assert()
        .success();
    wait_terminal(&root, "stamped");

    // The stored run.exited event's occurrence time.
    let replay = tendr(&root)
        .args(["events", "--session", "stamped", "--kind", "run.exited"])
        .output()
        .unwrap();
    let stored: serde_json::Value = serde_json::from_str(
        String::from_utf8_lossy(&replay.stdout)
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    let stored_micros = stored["ts"]
        .as_str()
        .unwrap()
        .parse::<EventTimestamp>()
        .unwrap()
        .epoch_micros();

    // Watch starts later; its snapshot must carry the occurrence time, not
    // poll-detection time.
    let follower = harness::ReadyFollower::spawn(&root, "watch", &["--events"]);
    let snapshot = follower.read_until(|r| r["kind"] == "run" && r["session"] == "stamped");
    follower.stop();

    let watch_micros = (snapshot["ts"].as_f64().unwrap() * 1e6).round() as u64;
    let drift = watch_micros.abs_diff(stored_micros);
    assert!(
        drift <= 2,
        "watch ts must be the event's occurrence time (drift {drift}µs)"
    );
}

#[test]
fn session_without_events_dir_keeps_legacy_meta_diff_synthesis() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "legacy", "--", "echo", "hi"])
        .assert()
        .success();
    wait_terminal(&root, "legacy");

    // A pre-slice-1 layout: meta.json + output.log, no events dir.
    let events_dir = root.path().join(".tendr/sessions/default/legacy/events");
    std::fs::remove_dir_all(&events_dir).unwrap();

    let follower = harness::ReadyFollower::spawn(&root, "watch", &["--events"]);
    follower.read_until(|r| r["session"] == "legacy" && r["name"] == "run.exited");
    let records = follower.records();
    follower.stop();

    assert_eq!(
        run_names(&records, "legacy"),
        ["run.exited"],
        "meta-diff snapshot for sessions without an event log, got: {records:?}"
    );
    let snapshot = &records[0];
    assert_eq!(snapshot["kind"], "run");
    assert_eq!(snapshot["source"], "tendr.sidecar");
    assert!(snapshot["ts"].is_f64() || snapshot["ts"].is_u64());
    assert_eq!(snapshot["data"]["status"], "Exited");
}

#[test]
fn from_now_skips_existing_but_streams_new_sessions_uncollapsed() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "old", "--", "echo", "hi"])
        .assert()
        .success();
    wait_terminal(&root, "old");

    let follower = harness::ReadyFollower::spawn(&root, "watch", &["--events", "--from-now"]);

    tendr(&root)
        .args(["start", "fresh", "--", "echo", "hi"])
        .assert()
        .success();
    wait_terminal(&root, "fresh");
    follower.read_until(|r| r["session"] == "fresh" && r["name"] == "run.exited");
    let records = follower.records();
    follower.stop();

    assert!(
        run_names(&records, "old").is_empty(),
        "--from-now skips pre-existing sessions, got: {records:?}"
    );
    assert_eq!(
        run_names(&records, "fresh"),
        ["run.starting", "run.started", "run.exited"],
        "sessions started after watch replay their full history"
    );
}

#[test]
fn replaced_session_streams_the_new_generation_uncollapsed() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "swap", "--", "echo", "first"])
        .assert()
        .success();
    wait_terminal(&root, "swap");

    // gen-1's snapshot (run.exited) is flushed before readiness.
    let follower = harness::ReadyFollower::spawn(&root, "watch", &["--events"]);

    // Waits for the old run to release its session up to the hang deadline.
    tendr(&root)
        .env("TENDR_TEST_REPLACE_WAIT_MS", HANG.as_millis().to_string())
        .args(["start", "swap", "--replace", "--", "echo", "second"])
        .assert()
        .success();
    wait_terminal(&root, "swap");

    // Two run.exited events occur (gen-1 snapshot, gen-2 exit), so wait on the
    // full count rather than a single terminal marker.
    let deadline = Instant::now() + HANG;
    let names = loop {
        let names = run_names(&follower.records(), "swap");
        if names.len() >= 4 {
            break names;
        }
        assert!(
            Instant::now() < deadline,
            "expected gen-1 snapshot + gen-2 lifecycle for swap, got: {names:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    follower.stop();

    // Snapshot of gen 1, then gen 2's full lifecycle from its fresh log.
    assert_eq!(
        names,
        ["run.exited", "run.starting", "run.started", "run.exited"],
        "got: {names:?}"
    );
}
