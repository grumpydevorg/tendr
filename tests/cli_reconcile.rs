#![cfg(unix)]

mod harness;

use harness::{tendr, wait_pid_dead, wait_running};
use predicates::prelude::*;
use std::sync::Mutex;
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

/// Read meta.json from disk and return parsed JSON.
fn read_meta_json(root: &TempDir, session: &str) -> serde_json::Value {
    let path = root
        .path()
        .join(format!(".tendr/sessions/default/{session}/meta.json"));
    let content = std::fs::read_to_string(&path).expect("failed to read meta.json");
    serde_json::from_str(&content).expect("failed to parse meta.json")
}

/// Get the sidecar PID from meta JSON.
fn sidecar_pid(meta: &serde_json::Value) -> i32 {
    meta["sidecar"]["pid"]
        .as_u64()
        .expect("sidecar.pid not found") as i32
}

#[test]
fn status_reconciles_crashed_sidecar() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "crashed-sc", "sleep", "60"])
        .assert()
        .success();
    wait_running(&root, "crashed-sc");

    let meta = read_meta_json(&root, "crashed-sc");
    let sc_pid = sidecar_pid(&meta);
    unsafe { libc::kill(sc_pid, libc::SIGKILL) };
    wait_pid_dead(sc_pid);

    let output = tendr(&root)
        .args(["status", "crashed-sc"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let result: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("output is not JSON");
    assert_eq!(result["status"], "SidecarLost");

    // Reconciliation killed the orphan it found, and recorded doing so.
    assert!(
        result["transition_provenance"]["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e == "orphan_killed"),
        "{result}"
    );
    wait_pid_dead(result["child"]["pid"].as_u64().expect("child pid") as i32);
}

#[test]
fn status_does_not_reconcile_running_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "still-running", "sleep", "60"])
        .assert()
        .success();
    wait_running(&root, "still-running");

    tendr(&root)
        .args(["status", "still-running"])
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""status": "Running"#));

    tendr(&root)
        .args(["kill", "--force", "still-running"])
        .assert()
        .success();
}

#[test]
fn wait_reconciles_crashed_sidecar() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "wait-crashed", "sleep", "60"])
        .assert()
        .success();
    wait_running(&root, "wait-crashed");

    let meta = read_meta_json(&root, "wait-crashed");
    let sc_pid = sidecar_pid(&meta);
    unsafe { libc::kill(sc_pid, libc::SIGKILL) };
    wait_pid_dead(sc_pid);

    let output = tendr(&root)
        .args(["wait", "--timeout", "5", "wait-crashed"])
        .assert()
        .code(3)
        .stdout(predicate::str::contains(r#""status": "SidecarLost"#));

    // `wait` reconciles too: the orphan it found is dead.
    let results: serde_json::Value = serde_json::from_slice(&output.get_output().stdout).unwrap();
    wait_pid_dead(results[0]["child"]["pid"].as_u64().expect("child pid") as i32);
}
