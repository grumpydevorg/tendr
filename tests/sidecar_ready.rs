mod harness;
use harness::Home;

use harness::{DeadlineAssertExt, tendr, wait_terminal, wait_terminal_quiescent};
use predicates::prelude::*;
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

#[test]
fn start_returns_promptly_not_blocked_by_child() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    // Returning at all — under the shared hang deadline, which the child
    // outlives — proves `start` did not block on the child, i.e. the ready
    // pipe fd was not leaked into it. No fragile wall-clock upper bound needed.
    tendr(&root)
        .args(["start", "prompt-test", "sleep", "3600"])
        .assert_within_deadline()
        .success();

    tendr(&root)
        .env(
            "TENDR_TEST_KILL_WAIT_MS",
            harness::HANG.as_millis().to_string(),
        )
        .args(["kill", "--force", "prompt-test"])
        .assert()
        .success();
}

#[test]
fn start_creates_session_and_returns_json() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    let output = tendr(&root)
        .args(["start", "test-job", "echo", "hello"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let meta: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(meta["session"], "test-job");
    assert_eq!(meta["schema_version"], 1);
    assert_eq!(meta["status"], "Running");
    assert!(meta["run_id"].is_string());
    assert!(meta["sidecar"]["pid"].is_number());
    assert!(meta["child"]["pid"].is_number());
    assert_eq!(meta["launch_spec"]["argv"][0], "echo");
    assert_eq!(meta["launch_spec"]["argv"][1], "hello");
}

#[test]
fn start_writes_durable_meta_json() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    tendr(&root)
        .args(["start", "durable-test", "echo", "hi"])
        .assert()
        .success();

    let meta_path = root
        .path()
        .join(".tendr/sessions/default/durable-test/meta.json");
    assert!(meta_path.exists(), "meta.json not written to disk");

    let content = std::fs::read_to_string(&meta_path).unwrap();
    let meta: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(meta["session"], "durable-test");
}

#[test]
fn start_same_name_after_completed_fails_already_exists() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    tendr(&root)
        .args(["start", "dup-test", "echo", "a"])
        .assert()
        .success();

    tendr(&root)
        .args(["start", "dup-test", "echo", "b"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("terminal state")
                .or(predicate::str::contains("session conflict")),
        );
}

#[test]
fn status_reads_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    tendr(&root)
        .args(["start", "status-test", "echo", "hi"])
        .assert()
        .success();

    tendr(&root)
        .args(["status", "status-test"])
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""session": "status-test"#));
}

#[test]
fn status_nonexistent_fails() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    tendr(&root).args(["status", "nope"]).assert().failure();
}

#[test]
fn list_shows_sessions() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    // Empty list
    let output = tendr(&root).args(["list"]).output().unwrap();
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert!(entries.is_empty());

    // Create sessions
    tendr(&root)
        .args(["start", "bravo", "echo", "b"])
        .assert()
        .success();
    tendr(&root)
        .args(["start", "alpha", "echo", "a"])
        .assert()
        .success();

    let output = tendr(&root).args(["list"]).output().unwrap();
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
    let names: Vec<&str> = entries
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["alpha", "bravo"]);
}

#[test]
fn launch_spec_json_cleaned_up() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    tendr(&root)
        .args(["start", "cleanup-test", "echo", "hi"])
        .assert()
        .success();

    let spec_path = root
        .path()
        .join(".tendr/sessions/default/cleanup-test/launch_spec.json");
    // Wait for sidecar to clean up
    wait_terminal(&root, "cleanup-test");
    assert!(!spec_path.exists(), "launch_spec.json should be cleaned up");
}

#[test]
fn lock_released_after_sidecar_exits() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    tendr(&root)
        .args(["start", "lock-test", "echo", "hi"])
        .assert()
        .success();
    let _terminal = wait_terminal_quiescent(&root, "lock-test");
}
