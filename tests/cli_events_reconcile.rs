//! Reconciliation against the event log — spec §3.6: heal meta from the
//! sidecar's own terminal event; otherwise infer and log run.sidecar_lost.

#![cfg(unix)]

mod harness;

use harness::{Observation, poll_until, tendr, wait_pid_dead, wait_running};
use std::sync::Mutex;
use std::time::Duration;
use tempfile::TempDir;

/// Sidecar-killing and crash-injection tests are timing-sensitive under
/// parallel load — serialize within this binary (same pattern as
/// cli_reconcile.rs).
static SERIAL: Mutex<()> = Mutex::new(());

/// Read all events for a session, merged by (ts, writer, seq).
fn read_events(root: &TempDir, session: &str) -> Vec<serde_json::Value> {
    let events_dir = root
        .path()
        .join(format!(".tendr/sessions/default/{session}/events"));
    let mut segments: Vec<_> = std::fs::read_dir(&events_dir)
        .expect("events dir exists")
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

fn read_meta_json(root: &TempDir, session: &str) -> serde_json::Value {
    let path = root
        .path()
        .join(format!(".tendr/sessions/default/{session}/meta.json"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

fn sidecar_pid(meta: &serde_json::Value) -> i32 {
    meta["sidecar"]["pid"].as_u64().expect("sidecar.pid") as i32
}

/// Poll `tendr status` until it reports a terminal state, or panic after
/// 10 s. Needed after crash injection: the aborting sidecar releases the
/// session lock only when the process finishes dying (macOS crash reporting
/// can delay that well past the fsync'd event append), and reconciliation
/// deliberately no-ops while the lock is held.
fn poll_status_until_terminal(root: &TempDir, session: &str) -> serde_json::Value {
    poll_until(Duration::from_secs(10), Duration::from_millis(50), || {
        let output = tendr(root).args(["status", session]).output().unwrap();
        assert!(output.status.success());
        let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        match status["status"].as_str() {
            Some(s) if s != "Starting" && s != "Running" => Observation::Ready(status),
            other => Observation::Pending(format!("status={}", other.unwrap_or("<missing>"))),
        }
    })
    .unwrap_or_else(|e| panic!("waiting for {session} to reconcile to a terminal state: {e}"))
}

/// Poll until the session's event log contains `kind`, or panic after 10 s.
fn wait_for_event_kind(root: &TempDir, session: &str, kind: &str) {
    let events_dir = root
        .path()
        .join(format!(".tendr/sessions/default/{session}/events"));
    poll_until(Duration::from_secs(10), Duration::from_millis(50), || {
        if events_dir.exists() && read_events(root, session).iter().any(|e| e["kind"] == kind) {
            Observation::Ready(())
        } else {
            Observation::Pending(format!("no event kind {kind} yet"))
        }
    })
    .unwrap_or_else(|e| panic!("waiting for event kind {kind}: {e}"))
}

/// Acceptance criterion 1 (plan): kill -9 the sidecar; reconciliation appends
/// the inferred run.sidecar_lost so replay shows the full lifecycle.
#[test]
fn killed_sidecar_appends_inferred_sidecar_lost_event() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "s1", "--", "sleep", "60"])
        .assert()
        .success();
    wait_running(&root, "s1");

    let meta = read_meta_json(&root, "s1");
    let sc_pid = sidecar_pid(&meta);
    unsafe { libc::kill(sc_pid, libc::SIGKILL) };
    wait_pid_dead(sc_pid);

    // status triggers reconciliation.
    let output = tendr(&root).args(["status", "s1"]).output().unwrap();
    assert!(output.status.success());
    let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["status"], "SidecarLost");

    let events = read_events(&root, "s1");
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["run.starting", "run.started", "run.sidecar_lost"]);

    let lost = events.last().unwrap();
    assert_eq!(lost["data"]["provenance"], "inferred");
    assert_eq!(lost["source"], "tendr.cli");
    assert_eq!(lost["run_id"], meta["run_id"]);
    // CLI emitter: freshly minted writer (not the sidecar's), seq from 1.
    assert_ne!(lost["writer"], meta["run_id"]);
    assert_eq!(lost["seq"], 1);

    // Reconciliation killed the orphan: its evidence says so.
    assert!(
        status["transition_provenance"]["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e == "orphan_killed"),
        "{status}"
    );
}

/// Acceptance criterion 2 (plan): a terminal event durably logged before the
/// meta write heals meta after a crash between the two writes — instead of
/// a false SidecarLost.
#[test]
fn crash_between_event_and_meta_heals_meta_from_event_log() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .env("TENDR_TEST_ABORT", "before_terminal_meta")
        .args(["start", "s1", "--", "false"])
        .assert()
        .success();
    wait_for_event_kind(&root, "s1", "run.exited");

    let status = poll_status_until_terminal(&root, "s1");

    // Healed to the sidecar's recorded outcome, not inferred as lost.
    // (meta.json spells the exit code "code"; the event data spells it
    // "exit_code" — two shipped schemas, both unchanged.)
    assert_eq!(status["status"], "Exited");
    assert_eq!(status["reason"], "ExitedError");
    assert_eq!(status["code"], 1);
    assert_eq!(status["transition_provenance"]["kind"], "direct");
    assert_eq!(
        status["transition_provenance"]["evidence"][0],
        "event_log_terminal"
    );

    // No spurious run.sidecar_lost was appended.
    let events = read_events(&root, "s1");
    assert!(
        !events.iter().any(|e| e["kind"] == "run.sidecar_lost"),
        "healed run must not gain a sidecar_lost event"
    );
}

/// wait derives its exit code from the healed state.
#[test]
fn wait_exit_code_reflects_healed_state() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .env("TENDR_TEST_ABORT", "before_terminal_meta")
        .args(["start", "s1", "--", "false"])
        .assert()
        .success();
    wait_for_event_kind(&root, "s1", "run.exited");

    // ExitedError → 42 (wait's non-zero-child-exit code), not 3 (sidecar
    // lost). wait polls internally, so it tolerates the lock-release lag;
    // the generous timeout covers slow crash teardown under load.
    tendr(&root)
        .args(["wait", "--timeout", "10", "s1"])
        .assert()
        .code(42);
}
