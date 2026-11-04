mod harness;

use harness::{HANG, SessionGuard, tendr, wait_running, wait_terminal};
use std::sync::Mutex;
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

/// `kill` waits this long (ms) for the sidecar to record the kill before it
/// kills the child itself: raised from 11 s to the hang deadline, so load
/// cannot take the reason out of the sidecar's hands.
fn kill_wait_ms() -> String {
    HANG.as_millis().to_string()
}

#[test]
fn force_kill_produces_killed_forced() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "fk-job", "sleep", "3600"])
        .assert()
        .success();
    let _session = SessionGuard::new(&root, "fk-job");
    wait_running(&root, "fk-job");

    tendr(&root)
        .env("TENDR_TEST_KILL_WAIT_MS", kill_wait_ms())
        .args(["kill", "--force", "fk-job"])
        .assert()
        .success();

    let meta = wait_terminal(&root, "fk-job");
    assert_eq!(meta["status"], "Exited");
    assert_eq!(meta["reason"], "KilledForced");
}

#[test]
fn graceful_kill_produces_killed() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "gk-job", "sleep", "3600"])
        .assert()
        .success();
    let _session = SessionGuard::new(&root, "gk-job");
    wait_running(&root, "gk-job");

    tendr(&root)
        .env("TENDR_TEST_KILL_WAIT_MS", kill_wait_ms())
        .args(["kill", "gk-job"])
        .assert()
        .success();

    let meta = wait_terminal(&root, "gk-job");
    assert_eq!(meta["status"], "Exited");
    assert_eq!(meta["reason"], "Killed");
}
