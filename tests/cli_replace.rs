mod harness;

use harness::{HANG, SessionGuard, tendr, wait_running, wait_terminal};
use std::sync::Mutex;
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

/// `start --replace`, waiting for the old run to end and release its
/// session up to the hang deadline rather than the product's 10 s.
fn replace(root: &TempDir, name: &str) -> std::process::Output {
    tendr(root)
        .env("TENDR_TEST_REPLACE_WAIT_MS", HANG.as_millis().to_string())
        .args(["start", "--replace", name, "sleep", "3600"])
        .output()
        .unwrap()
}

/// `kill --force`, waiting for the sidecar to record it up to the hang
/// deadline rather than kill's 11 s.
fn kill_force(root: &TempDir, name: &str) {
    tendr(root)
        .env("TENDR_TEST_KILL_WAIT_MS", HANG.as_millis().to_string())
        .args(["kill", "--force", name])
        .assert()
        .success();
}

#[test]
fn replace_running_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let out1 = tendr(&root)
        .args(["start", "repl-run", "sleep", "3600"])
        .output()
        .unwrap();
    assert!(out1.status.success());
    let _session = SessionGuard::new(&root, "repl-run");
    wait_running(&root, "repl-run");

    let meta1: serde_json::Value = serde_json::from_slice(&out1.stdout).unwrap();
    let run_id1 = meta1["run_id"].as_str().unwrap().to_string();

    let out2 = replace(&root, "repl-run");
    assert!(out2.status.success());

    let meta2: serde_json::Value = serde_json::from_slice(&out2.stdout).unwrap();
    let run_id2 = meta2["run_id"].as_str().unwrap();

    assert_ne!(run_id1, run_id2, "replace should create a new run_id");

    kill_force(&root, "repl-run");
}

#[test]
fn replace_terminal_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "repl-term", "true"])
        .assert()
        .success();
    wait_terminal(&root, "repl-term");

    let out2 = replace(&root, "repl-term");
    let _session = SessionGuard::new(&root, "repl-term");
    assert!(out2.status.success());

    let meta2: serde_json::Value = serde_json::from_slice(&out2.stdout).unwrap();
    assert!(meta2["run_id"].as_str().is_some());

    kill_force(&root, "repl-term");
}

#[test]
fn replace_nonexistent_is_noop() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let out = tendr(&root)
        .args(["start", "--replace", "repl-nope", "true"])
        .output()
        .unwrap();
    assert!(out.status.success());

    let meta: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(meta["run_id"].as_str().is_some());
}

#[test]
fn replace_increments_generation() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let out1 = tendr(&root)
        .args(["start", "gen-test", "sleep", "3600"])
        .output()
        .unwrap();
    assert!(out1.status.success());
    let _session = SessionGuard::new(&root, "gen-test");
    let meta1: serde_json::Value = serde_json::from_slice(&out1.stdout).unwrap();
    assert_eq!(meta1["generation"], 1);

    let out2 = replace(&root, "gen-test");
    assert!(out2.status.success());
    let meta2: serde_json::Value = serde_json::from_slice(&out2.stdout).unwrap();
    assert_eq!(meta2["generation"], 2);

    kill_force(&root, "gen-test");
}
