mod harness;

use harness::{DeadlineAssertExt, HANG, tendr, wait_running, wait_terminal};
use predicates::prelude::*;
use std::sync::Mutex;
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

/// `tendr kill --force <session>`, waiting for its sidecar to record the end
/// however long that takes rather than falling back to a direct kill.
fn kill_forced(root: &TempDir, session: &str) {
    tendr(root)
        .env("TENDR_TEST_KILL_WAIT_MS", "3600000")
        .args(["kill", "--force", session])
        .assert()
        .success();
}

/// Kills a background child on drop, so a failed assertion does not leak it.
struct KillOnDrop(Option<std::process::Child>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn wait_returns_terminal_state() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "wait-ok", "true"])
        .assert()
        .success();
    wait_terminal(&root, "wait-ok");

    // Output is now a JSON array (even for single session).
    tendr(&root)
        .args(["wait", "wait-ok"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with("["))
        .stdout(predicate::str::contains(r#""status": "Exited"#))
        .stdout(predicate::str::contains(r#""reason": "ExitedOk"#));
}

#[test]
fn wait_blocks_until_exit() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // The child runs until the test opens the gate.
    let gate = root.path().join("exit-gate");
    let _open = harness::OpenGate(gate.clone());
    let script = format!(
        "while [ ! -e {} ]; do sleep 0.05; done",
        shell_words::quote(gate.to_str().unwrap())
    );
    tendr(&root)
        .args(["start", "wait-block", "--", "sh", "-c", &script])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "wait-block");
    wait_running(&root, "wait-block");

    let mut waiter = KillOnDrop(Some(
        std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
            .args(["wait", "wait-block"])
            .env("HOME", root.path())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    let child = waiter.0.as_mut().unwrap();
    // `wait` blocks while the child runs. The sleep waits out a `wait` that
    // would return at once; under load it can only pass wrongly.
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        child.try_wait().unwrap().is_none(),
        "wait returned while the child still ran"
    );

    std::fs::write(&gate, b"").unwrap();
    harness::poll_child_exit(child, HANG, std::time::Duration::from_millis(20))
        .unwrap_or_else(|e| panic!("wait did not return once the child exited: {e}"));
    let out = waiter.0.take().unwrap().wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("ExitedOk"));
}

#[test]
fn wait_timeout_expires() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "wait-timeout", "sleep", "3600"])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "wait-timeout");
    wait_running(&root, "wait-timeout");

    // The failure + exit 1 + "timeout" stderr prove the product's --timeout
    // fired; the hang detector bounds the call instead of a fragile 3s cap.
    tendr(&root)
        .args(["wait", "--timeout", "1", "wait-timeout"])
        .assert_within_deadline()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("timeout"));

    kill_forced(&root, "wait-timeout");
}

#[test]
fn wait_nonexistent_session_fails() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root).args(["wait", "nope"]).assert().failure();
}

#[test]
fn wait_exit_code_42_for_nonzero_child() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "wait-err", "sh", "-c", "exit 3"])
        .assert()
        .success();
    wait_terminal(&root, "wait-err");

    tendr(&root).args(["wait", "wait-err"]).assert().code(42);
}

#[test]
fn wait_exit_code_2_for_spawn_failed() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "wait-spawn", "/nonexistent/binary"])
        .output()
        .unwrap(); // exit 2 from start is expected
    wait_terminal(&root, "wait-spawn");

    tendr(&root).args(["wait", "wait-spawn"]).assert().code(2);
}

// --- New multi-session tests ---

#[test]
fn wait_all_multiple_sessions() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "multi-a", "true"])
        .assert()
        .success();
    tendr(&root)
        .args(["start", "multi-b", "true"])
        .assert()
        .success();
    wait_terminal(&root, "multi-a");
    wait_terminal(&root, "multi-b");

    let output = tendr(&root)
        .args(["wait", "multi-a", "multi-b"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let arr: Vec<serde_json::Value> = serde_json::from_str(&stdout).unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["session"], "multi-a");
    assert_eq!(arr[1]["session"], "multi-b");
}

#[test]
fn wait_any_returns_on_first_terminal() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // One fast, one slow.
    tendr(&root)
        .args(["start", "any-fast", "true"])
        .assert()
        .success();
    tendr(&root)
        .args(["start", "any-slow", "sleep", "3600"])
        .assert()
        .success();
    let _any_slow = harness::SessionGuard::new(&root, "any-slow");
    wait_terminal(&root, "any-fast");
    wait_running(&root, "any-slow");

    let assert = tendr(&root)
        .args(["wait", "--any", "any-fast", "any-slow"])
        .assert_within_deadline()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let arr: Vec<serde_json::Value> = serde_json::from_str(&stdout).unwrap();
    // Only the terminal session is in the output — proves `--any` returned on
    // the terminal session without blocking on the sibling, which never ends
    // by itself, so no wall-clock upper bound is needed.
    assert!(!arr.is_empty());
    assert!(arr.iter().any(|m| m["session"] == "any-fast"));

    // Cleanup the slow session.
    kill_forced(&root, "any-slow");
}

#[test]
fn wait_mixed_exit_codes() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // One success, one failure.
    tendr(&root)
        .args(["start", "mix-ok", "true"])
        .assert()
        .success();
    tendr(&root)
        .args(["start", "mix-fail", "sh", "-c", "exit 1"])
        .assert()
        .success();
    wait_terminal(&root, "mix-ok");
    wait_terminal(&root, "mix-fail");

    tendr(&root)
        .args(["wait", "mix-ok", "mix-fail"])
        .assert()
        .code(42);
}

#[test]
fn wait_single_session_emits_array_of_one() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "single-arr", "true"])
        .assert()
        .success();
    wait_terminal(&root, "single-arr");

    let output = tendr(&root).args(["wait", "single-arr"]).output().unwrap();
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let arr: Vec<serde_json::Value> = serde_json::from_str(&stdout).unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["session"], "single-arr");
}

#[test]
fn wait_not_found_among_multiple() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "exists-ok", "true"])
        .assert()
        .success();
    wait_terminal(&root, "exists-ok");

    // One valid, one nonexistent -- should fail immediately.
    tendr(&root)
        .args(["wait", "exists-ok", "does-not-exist"])
        .assert()
        .failure()
        .code(1);
}

#[test]
fn wait_multiple_timeout() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "mt-slow1", "sleep", "3600"])
        .assert()
        .success();
    let _mt_slow1 = harness::SessionGuard::new(&root, "mt-slow1");
    tendr(&root)
        .args(["start", "mt-slow2", "sleep", "3600"])
        .assert()
        .success();
    let _mt_slow2 = harness::SessionGuard::new(&root, "mt-slow2");
    wait_running(&root, "mt-slow1");
    wait_running(&root, "mt-slow2");

    // As above: the failure + exit 1 + "timeout" stderr prove the product
    // timeout fired; the hang detector bounds the call, not a fragile 3s cap.
    tendr(&root)
        .args(["wait", "--timeout", "1", "mt-slow1", "mt-slow2"])
        .assert_within_deadline()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("timeout"));

    // Cleanup.
    kill_forced(&root, "mt-slow1");
    kill_forced(&root, "mt-slow2");
}

#[test]
fn wait_dep_timed_out_exits_124() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // job1 never ends by itself; job2 depends on it with a 2s timeout, which
    // is its only way out.
    tendr(&root)
        .args(["start", "dep-slow", "sleep", "3600"])
        .assert()
        .success();
    let _dep_slow = harness::SessionGuard::new(&root, "dep-slow");
    wait_running(&root, "dep-slow");

    tendr(&root)
        .args([
            "start",
            "dep-waiter",
            "--after",
            "dep-slow",
            "--timeout",
            "2",
            "--",
            "true",
        ])
        .assert()
        .success();
    let _dep_waiter = harness::SessionGuard::new(&root, "dep-waiter");
    wait_terminal(&root, "dep-waiter");

    tendr(&root).args(["wait", "dep-waiter"]).assert().code(124);

    kill_forced(&root, "dep-slow");
}

#[test]
fn wait_killed_during_dep_wait_exits_137() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // job1 never ends by itself; job2 depends on it.
    tendr(&root)
        .args(["start", "dep-block", "sleep", "3600"])
        .assert()
        .success();
    let _dep_block = harness::SessionGuard::new(&root, "dep-block");
    wait_running(&root, "dep-block");

    tendr(&root)
        .args(["start", "dep-victim", "--after", "dep-block", "--", "true"])
        .assert()
        .success();
    let _dep_victim = harness::SessionGuard::new(&root, "dep-victim");

    // No sleep: start returning proves the first scan completed. The kill
    // request written now persists on disk until the poll loop consumes it.

    // Kill the waiting session. The kill waits for its sidecar to record the
    // end however long that takes, not falling back to a direct kill.
    tendr(&root)
        .env("TENDR_TEST_KILL_WAIT_MS", "3600000")
        .args(["kill", "dep-victim"])
        .assert()
        .success();
    wait_terminal(&root, "dep-victim");

    tendr(&root).args(["wait", "dep-victim"]).assert().code(137);

    kill_forced(&root, "dep-block");
}

#[test]
fn wait_dep_failed_beats_nonzero_in_multi() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // One non-zero exit, one dependency failure.
    tendr(&root)
        .args(["start", "dep-upstream", "false"])
        .assert()
        .success();
    wait_terminal(&root, "dep-upstream");

    tendr(&root)
        .args([
            "start",
            "dep-downstream",
            "--after",
            "dep-upstream",
            "--",
            "true",
        ])
        .assert()
        .success();
    wait_terminal(&root, "dep-downstream");

    tendr(&root)
        .args(["start", "plain-fail", "sh", "-c", "exit 1"])
        .assert()
        .success();
    wait_terminal(&root, "plain-fail");

    // Dep failed (4) is more severe than non-zero exit (42).
    tendr(&root)
        .args(["wait", "plain-fail", "dep-downstream"])
        .assert()
        .code(4);
}

#[test]
fn wait_duplicate_session_names() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "dup-test", "true"])
        .assert()
        .success();
    wait_terminal(&root, "dup-test");

    // Passing the same name twice should emit one entry, not two.
    let output = tendr(&root)
        .args(["wait", "dup-test", "dup-test"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let arr: Vec<serde_json::Value> = serde_json::from_str(&stdout).unwrap();
    assert_eq!(arr.len(), 1);
}

#[test]
fn wait_spawn_failed_beats_nonzero_exit() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // One non-zero exit, one spawn failure.
    tendr(&root)
        .args(["start", "sev-exit", "sh", "-c", "exit 1"])
        .assert()
        .success();
    tendr(&root)
        .args(["start", "sev-spawn", "/nonexistent/binary"])
        .output()
        .unwrap();
    wait_terminal(&root, "sev-exit");
    wait_terminal(&root, "sev-spawn");

    // Spawn failure (2) is more severe than non-zero exit (42).
    tendr(&root)
        .args(["wait", "sev-exit", "sev-spawn"])
        .assert()
        .code(2);
}
