mod harness;

use harness::{echo_env_cmd, tendr, touch_cmd, wait_running, wait_terminal};
use std::sync::Mutex;
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

/// Wait for terminal state, then wait for callback completion.
/// Callbacks run after the lock is released, so we poll for the callback record file.
fn wait_for_callbacks(root: &TempDir, session: &str) {
    wait_terminal(root, session);

    // Callbacks run after lock release. Poll for the callback record file.
    // Read meta to get run_id, then check for callbacks/<run_id>.json
    let meta_path = root
        .path()
        .join(format!(".tendr/sessions/default/{session}/meta.json"));
    let content = std::fs::read_to_string(&meta_path).expect("meta.json should exist");
    let meta: serde_json::Value = serde_json::from_str(&content).unwrap();
    let run_id = meta["run_id"].as_str().unwrap();

    let callbacks_path = root.path().join(format!(".tendr/callbacks/{run_id}.json"));
    // Callbacks may have no failures and still write the record, so not finding
    // the file just means we timed out waiting — tests that need it will assert.
    let _ = harness::poll_path_exists(
        &callbacks_path,
        std::time::Duration::from_secs(5),
        std::time::Duration::from_millis(50),
    );
}

/// Read the callback record for a session (by run_id from meta.json).
fn read_callback_record(root: &TempDir, session: &str) -> Option<serde_json::Value> {
    let meta_path = root
        .path()
        .join(format!(".tendr/sessions/default/{session}/meta.json"));
    let content = std::fs::read_to_string(&meta_path).ok()?;
    let meta: serde_json::Value = serde_json::from_str(&content).ok()?;
    let run_id = meta["run_id"].as_str()?;

    let callbacks_path = root.path().join(format!(".tendr/callbacks/{run_id}.json"));
    let cb_content = std::fs::read_to_string(&callbacks_path).ok()?;
    serde_json::from_str(&cb_content).ok()
}

#[test]
fn on_exit_callback_runs_after_normal_exit() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let marker = root.path().join("normal_exit_marker");

    let out = tendr(&root)
        .args([
            "start",
            "on-exit-normal",
            "--on-exit",
            &touch_cmd(&marker),
            "--",
            "echo",
            "done",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    wait_for_callbacks(&root, "on-exit-normal");

    assert!(
        marker.exists(),
        "marker file should exist after normal exit callback: {marker:?}"
    );
}

#[test]
fn on_exit_callback_runs_after_forced_kill() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let marker = root.path().join("kill_exit_marker");

    let out = tendr(&root)
        .args([
            "start",
            "on-exit-kill",
            "--on-exit",
            &touch_cmd(&marker),
            "--",
            "sleep",
            "60",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    wait_running(&root, "on-exit-kill");

    tendr(&root)
        .args(["kill", "--force", "on-exit-kill"])
        .assert()
        .success();

    wait_for_callbacks(&root, "on-exit-kill");

    assert!(
        marker.exists(),
        "marker file should exist after forced kill callback: {marker:?}"
    );
}

#[test]
fn on_exit_callback_sees_env_vars() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let output_file = root.path().join("env_output.txt");

    let out = tendr(&root)
        .args([
            "start",
            "on-exit-env",
            "--on-exit",
            &echo_env_cmd(&output_file),
            "--",
            "echo",
            "done",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    wait_for_callbacks(&root, "on-exit-env");

    assert!(
        output_file.exists(),
        "env output file should exist: {output_file:?}"
    );

    let content = std::fs::read_to_string(&output_file).unwrap();
    let content = content.trim();

    assert!(
        content.contains("on-exit-env"),
        "should contain session name 'on-exit-env', got: {content}"
    );
    assert!(
        content.contains("default"),
        "should contain namespace 'default', got: {content}"
    );
    assert!(
        content.contains("ExitedOk"),
        "should contain exit reason 'ExitedOk', got: {content}"
    );
}

#[test]
fn on_exit_callback_failure_recorded_without_state_change() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let out = tendr(&root)
        .args([
            "start",
            "on-exit-fail",
            "--on-exit",
            "/nonexistent/binary",
            "--",
            "echo",
            "done",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    wait_for_callbacks(&root, "on-exit-fail");

    // Meta should still show correct terminal state — callbacks don't modify it
    let meta_path = root
        .path()
        .join(".tendr/sessions/default/on-exit-fail/meta.json");
    let meta_content = std::fs::read_to_string(&meta_path).unwrap();
    let meta: serde_json::Value = serde_json::from_str(&meta_content).unwrap();
    assert_eq!(meta["status"].as_str(), Some("Exited"));

    // Callback failure should be in the callback record, not meta.json warnings
    let record = read_callback_record(&root, "on-exit-fail").expect("callback record should exist");
    let callbacks = record["callbacks"].as_array().expect("callbacks array");
    assert_eq!(callbacks.len(), 1);
    assert_eq!(callbacks[0]["status"].as_str(), Some("spawn_failed"));
}

#[test]
fn on_exit_multiple_callbacks_both_run() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let marker_a = root.path().join("marker_a");
    let marker_b = root.path().join("marker_b");

    let out = tendr(&root)
        .args([
            "start",
            "on-exit-multi",
            "--on-exit",
            &touch_cmd(&marker_a),
            "--on-exit",
            &touch_cmd(&marker_b),
            "--",
            "echo",
            "done",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    wait_for_callbacks(&root, "on-exit-multi");

    assert!(
        marker_a.exists(),
        "first marker file should exist: {marker_a:?}"
    );
    assert!(
        marker_b.exists(),
        "second marker file should exist: {marker_b:?}"
    );
}

// --- Slice 3: callback.finished events (plan scope 5) ---

/// Each on-exit callback lands one callback.finished event through the
/// sidecar's lifecycle writer: same writer id as the run events, seq
/// contiguous after the terminal transition, status vocabulary verbatim.
#[test]
fn on_exit_callbacks_emit_finished_events() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let marker = root.path().join("cb_event_marker");
    let out = tendr(&root)
        .args([
            "start",
            "on-exit-events",
            "--on-exit",
            &touch_cmd(&marker),
            "--on-exit",
            "/nonexistent/binary",
            "--",
            "echo",
            "done",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());

    wait_for_callbacks(&root, "on-exit-events");

    let events = harness::read_events(&root, "on-exit-events");
    let exited = events
        .iter()
        .find(|e| e["kind"] == "run.exited")
        .expect("terminal lifecycle event");
    let finished: Vec<_> = events
        .iter()
        .filter(|e| e["kind"] == "callback.finished")
        .collect();
    assert_eq!(finished.len(), 2, "one event per configured callback");

    assert_eq!(finished[0]["data"]["index"], 0);
    assert_eq!(finished[0]["data"]["status"], "ok");
    assert_eq!(finished[1]["data"]["index"], 1);
    assert_eq!(finished[1]["data"]["status"], "spawn_failed");
    assert!(finished[1]["data"]["error"].is_string());

    for (offset, event) in finished.iter().enumerate() {
        assert_eq!(event["source"], "tendr.sidecar");
        assert_eq!(
            event["writer"], exited["writer"],
            "lifecycle writer identity reused"
        );
        assert_eq!(
            event["seq"].as_u64().unwrap(),
            exited["seq"].as_u64().unwrap() + 1 + offset as u64,
            "seq contiguous after the terminal transition"
        );
    }

    // The batch record file is unchanged in shape.
    let record = read_callback_record(&root, "on-exit-events").expect("batch record");
    assert_eq!(record["callbacks"].as_array().unwrap().len(), 2);
}
