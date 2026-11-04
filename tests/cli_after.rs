mod harness;
use harness::Home;

use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// Argv for a child that exits 0 once `gate` exists, and never before: the
/// test decides when it ends, however slowly the test gets there.
fn until_exists(gate: &std::path::Path) -> [String; 5] {
    [
        "sh".into(),
        "-c".into(),
        "until [ -e \"$1\" ]; do sleep 0.05; done".into(),
        "sh".into(),
        gate.to_str().unwrap().into(),
    ]
}

/// --after resolves dependency run_id at bind time.
/// Verifies the launch_spec in meta.json contains the binding.
#[test]
fn after_bind_captures_run_id() {
    let _lock = lock();
    let root = Home::new();

    // Start job1 (short-lived)
    harness::tendr(&root)
        .args(["start", "job1", "--", "true"])
        .assert()
        .success();
    harness::wait_terminal(&root, "job1");

    // Read job1's run_id
    let job1_meta_path = root.path().join(".tendr/sessions/default/job1/meta.json");
    let job1_meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&job1_meta_path).unwrap()).unwrap();
    let job1_run_id = job1_meta["run_id"].as_str().unwrap();

    // Start job2 --after job1
    harness::tendr(&root)
        .args(["start", "job2", "--after", "job1", "--", "true"])
        .assert()
        .success();
    harness::wait_terminal(&root, "job2");

    // Verify job2's launch_spec.after contains job1's run_id
    let job2_meta_path = root.path().join(".tendr/sessions/default/job2/meta.json");
    let job2_meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&job2_meta_path).unwrap()).unwrap();
    let after = &job2_meta["launch_spec"]["after"];
    assert_eq!(after[0]["session"].as_str(), Some("job1"));
    assert_eq!(after[0]["run_id"].as_str(), Some(job1_run_id));
}

/// --after nonexistent session fails at bind time.
#[test]
fn after_nonexistent_session_fails_at_bind() {
    let _lock = lock();
    let root = Home::new();

    harness::tendr(&root)
        .args(["start", "job2", "--after", "nonexistent", "--", "true"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("session not found"));
}

/// Idempotent start on a Starting session with --after: same spec -> return existing.
/// With the sidecar wait loop, --after sessions stay in Starting while waiting.
/// This validates that the after bindings are part of the spec hash for idempotent matching.
#[test]
fn after_idempotent_on_running() {
    let _lock = lock();
    let root = Home::new();

    // Start job1 (never ends by itself)
    harness::tendr(&root)
        .args(["start", "job1", "--", "sleep", "3600"])
        .assert()
        .success();
    harness::wait_running(&root, "job1");

    // Start job2 --after job1 (stays in Starting, waiting for job1)
    harness::tendr(&root)
        .args(["start", "job2", "--after", "job1", "--", "sleep", "30"])
        .assert()
        .success();

    // No sleep: start returning proves the first scan completed — job2's metadata
    // is Starting and the sidecar holds the session lock.

    // Second start with identical args: should succeed (idempotent)
    harness::tendr(&root)
        .args(["start", "job2", "--after", "job1", "--", "sleep", "30"])
        .assert()
        .success();
}

/// Idempotent start on Starting session (waiting for deps): same spec -> return existing.
#[test]
fn after_idempotent_on_starting() {
    let _lock = lock();
    let root = Home::new();

    // Start job1 (never ends by itself, so job2 stays in Starting)
    harness::tendr(&root)
        .args(["start", "job1", "--", "sleep", "3600"])
        .assert()
        .success();
    harness::wait_running(&root, "job1");

    // Start job2 --after job1 (enters Starting, waits)
    harness::tendr(&root)
        .args(["start", "job2", "--after", "job1", "--", "echo", "done"])
        .assert()
        .success();

    // No sleep: start returning proves the first scan completed — job2's metadata
    // is Starting and the sidecar holds the session lock.

    // Second start with identical args: should succeed (idempotent)
    harness::tendr(&root)
        .args(["start", "job2", "--after", "job1", "--", "echo", "done"])
        .assert()
        .success();
}

/// Kill during dependency wait → DependencyFailed/Killed.
#[test]
fn kill_during_dependency_wait() {
    let _lock = lock();
    let root = Home::new();

    // Start job1 (never ends by itself)
    harness::tendr(&root)
        .args(["start", "job1", "--", "sleep", "3600"])
        .assert()
        .success();
    harness::wait_running(&root, "job1");

    // Start job2 --after job1 (enters wait loop)
    harness::tendr(&root)
        .args(["start", "job2", "--after", "job1", "--", "true"])
        .assert()
        .success();

    // No sleep: start returning proves the first scan completed. A kill request
    // written now persists on disk until the poll loop consumes it.

    // Kill job2
    harness::tendr(&root)
        // The sidecar must record the kill itself (dep_reason), so `kill` waits
        // for it up to the hang deadline instead of 8 s.
        .env(
            "TENDR_TEST_KILL_WAIT_MS",
            harness::HANG.as_millis().to_string(),
        )
        .args(["kill", "job2"])
        .assert()
        .success();

    let meta = harness::wait_terminal(&root, "job2");
    assert_eq!(meta["status"].as_str(), Some("DependencyFailed"));
    assert_eq!(meta["dep_reason"].as_str(), Some("Killed"));
}

/// Dependency exits non-zero → DependencyFailed/Failed.
#[test]
fn after_dependency_exits_nonzero() {
    let _lock = lock();
    let root = Home::new();

    harness::tendr(&root)
        .args(["start", "job1", "--", "false"])
        .assert()
        .success();
    harness::wait_terminal(&root, "job1");

    harness::tendr(&root)
        .args(["start", "job2", "--after", "job1", "--", "true"])
        .assert()
        .success();

    let meta = harness::wait_terminal(&root, "job2");
    assert_eq!(meta["status"].as_str(), Some("DependencyFailed"));
    assert_eq!(meta["dep_reason"].as_str(), Some("Failed"));
}

/// First-scan barrier: when a dependency is already terminally failed at the
/// first scan, `tendr start` returns the truthful DependencyFailed snapshot
/// (not an optimistic Starting one) — and remains a successful command.
#[test]
fn start_after_already_failed_dep_returns_dependency_failed_snapshot() {
    let _lock = lock();
    let root = Home::new();

    harness::tendr(&root)
        .args(["start", "job1", "--", "false"])
        .assert()
        .success();
    harness::wait_terminal(&root, "job1");

    let out = harness::tendr(&root)
        .args(["start", "job2", "--after", "job1", "--", "true"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "start remains a successful command even when the dependency already failed"
    );
    let snapshot: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        snapshot["status"].as_str(),
        Some("DependencyFailed"),
        "the returned snapshot must reflect the first-scan outcome, not optimistic Starting"
    );
    assert_eq!(snapshot["dep_reason"].as_str(), Some("Failed"));
}

/// --any-exit: dependency exits non-zero → job2 runs anyway.
#[test]
fn after_any_exit_proceeds_on_failure() {
    let _lock = lock();
    let root = Home::new();

    harness::tendr(&root)
        .args(["start", "job1", "--", "false"])
        .assert()
        .success();
    harness::wait_terminal(&root, "job1");

    harness::tendr(&root)
        .args([
            "start",
            "job2",
            "--after",
            "job1",
            "--any-exit",
            "--",
            "true",
        ])
        .assert()
        .success();

    let meta = harness::wait_terminal(&root, "job2");
    assert_eq!(meta["status"].as_str(), Some("Exited"));
    assert_eq!(meta["reason"].as_str(), Some("ExitedOk"));
}

/// Dependency replaced after bind → DependencyFailed/Failed.
#[test]
fn after_run_id_mismatch_fails() {
    let _lock = lock();
    let root = Home::new();

    harness::tendr(&root)
        .args(["start", "job1", "--", "sleep", "3600"])
        .assert()
        .success();
    // Never ends by itself: only the replace below ends this run.
    harness::wait_running(&root, "job1");

    // Start job2 --after job1 (captures run_id)
    harness::tendr(&root)
        .args(["start", "job2", "--after", "job1", "--", "true"])
        .assert()
        .success();

    // Replace job1 (new run_id)
    harness::tendr(&root)
        // Waits for the old run to end up to the hang deadline, not 10 s.
        .env(
            "TENDR_TEST_REPLACE_WAIT_MS",
            harness::HANG.as_millis().to_string(),
        )
        .args(["start", "job1", "--replace", "--", "true"])
        .assert()
        .success();
    harness::wait_terminal(&root, "job1");

    // job2 should fail (run_id mismatch)
    let meta = harness::wait_terminal(&root, "job2");
    assert_eq!(meta["status"].as_str(), Some("DependencyFailed"));
    assert_eq!(meta["dep_reason"].as_str(), Some("Failed"));
}

/// Timeout during dependency wait → DependencyFailed/TimedOut.
#[test]
fn after_timeout_during_wait() {
    let _lock = lock();
    let root = Home::new();

    harness::tendr(&root)
        .args(["start", "job1", "--", "sleep", "3600"])
        .assert()
        .success();
    // Never ends by itself: job2's dependency timeout is its only way out.
    harness::wait_running(&root, "job1");

    harness::tendr(&root)
        .args([
            "start",
            "job2",
            "--after",
            "job1",
            "--timeout",
            "2",
            "--",
            "true",
        ])
        .assert()
        .success();

    let meta = harness::wait_terminal(&root, "job2");
    assert_eq!(meta["status"].as_str(), Some("DependencyFailed"));
    assert_eq!(meta["dep_reason"].as_str(), Some("TimedOut"));
}

/// Multiple --after: waits for all deps.
#[test]
fn after_multiple_dependencies() {
    let _lock = lock();
    let root = Home::new();

    harness::tendr(&root)
        .args(["start", "job1", "--", "true"])
        .assert()
        .success();
    harness::tendr(&root)
        .args(["start", "job3", "--", "true"])
        .assert()
        .success();
    harness::wait_terminal(&root, "job1");
    harness::wait_terminal(&root, "job3");

    harness::tendr(&root)
        .args([
            "start", "job2", "--after", "job1", "--after", "job3", "--", "true",
        ])
        .assert()
        .success();

    let meta = harness::wait_terminal(&root, "job2");
    assert_eq!(meta["status"].as_str(), Some("Exited"));
    assert_eq!(meta["reason"].as_str(), Some("ExitedOk"));
}

/// Different --after deps on same session → conflict.
#[test]
fn after_idempotent_different_deps_conflicts() {
    let _lock = lock();
    let root = Home::new();

    harness::tendr(&root)
        .args(["start", "job1", "--", "sleep", "3600"])
        .assert()
        .success();
    harness::tendr(&root)
        .args(["start", "job3", "--", "sleep", "3600"])
        .assert()
        .success();
    harness::wait_running(&root, "job1");
    harness::wait_running(&root, "job3");

    harness::tendr(&root)
        .args(["start", "job2", "--after", "job1", "--", "sleep", "30"])
        .assert()
        .success();

    // Different dep → conflict. start returned ⇒ first scan completed, job2's
    // metadata is Starting, and the sidecar holds the session lock.
    harness::tendr(&root)
        .args(["start", "job2", "--after", "job3", "--", "sleep", "30"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("session conflict"));
}

/// `tendr wait` on DependencyFailed session exits with code 4.
#[test]
fn wait_dependency_failed_exits_4() {
    let _lock = lock();
    let root = Home::new();

    harness::tendr(&root)
        .args(["start", "job1", "--", "false"])
        .assert()
        .success();
    harness::wait_terminal(&root, "job1");

    harness::tendr(&root)
        .args(["start", "job2", "--after", "job1", "--", "true"])
        .assert()
        .success();
    harness::wait_terminal(&root, "job2");

    harness::tendr(&root)
        .args(["wait", "job2"])
        .assert()
        .code(4);
}

/// job2 waits while job1 is still running, then runs after job1 exits.
#[test]
fn after_waits_for_running_dependency() {
    let _lock = lock();
    let root = Home::new();

    // Start job1, which runs until the test opens its gate.
    let gate = root.path().join("job1-gate");
    harness::tendr(&root)
        .args(["start", "job1", "--"])
        .args(until_exists(&gate))
        .assert()
        .success();
    harness::wait_running(&root, "job1");

    // Start job2 --after job1
    harness::tendr(&root)
        .args(["start", "job2", "--after", "job1", "--", "true"])
        .assert()
        .success();

    // job2 should be Starting (waiting)
    let meta_path = root.path().join(".tendr/sessions/default/job2/meta.json");
    let content = std::fs::read_to_string(&meta_path).unwrap();
    let meta: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(meta["status"].as_str(), Some("Starting"));

    // Let job1 exit, then wait for both to finish
    std::fs::write(&gate, "").unwrap();
    harness::wait_terminal(&root, "job1");
    let meta2 = harness::wait_terminal(&root, "job2");
    assert_eq!(meta2["status"].as_str(), Some("Exited"));
    assert_eq!(meta2["reason"].as_str(), Some("ExitedOk"));
}

/// kill --force during dependency wait → DependencyFailed/KilledForced.
#[test]
fn kill_force_during_dependency_wait() {
    let _lock = lock();
    let root = Home::new();

    harness::tendr(&root)
        .args(["start", "job1", "--", "sleep", "3600"])
        .assert()
        .success();
    harness::wait_running(&root, "job1");

    harness::tendr(&root)
        .args(["start", "job2", "--after", "job1", "--", "true"])
        .assert()
        .success();

    // start returned ⇒ first scan completed; a kill request written now persists
    // on disk until the poll loop consumes it.
    harness::tendr(&root)
        .env(
            "TENDR_TEST_KILL_WAIT_MS",
            harness::HANG.as_millis().to_string(),
        )
        .args(["kill", "job2", "--force"])
        .assert()
        .success();

    let meta = harness::wait_terminal(&root, "job2");
    assert_eq!(meta["status"].as_str(), Some("DependencyFailed"));
    assert_eq!(meta["dep_reason"].as_str(), Some("KilledForced"));
}

/// Satisfied dep is latched: replacing it after satisfaction doesn't fail the waiter.
#[test]
fn after_satisfied_dep_not_invalidated_by_replace() {
    let _lock = lock();
    let root = Home::new();

    // job1 exits quickly, job3 runs until the test opens its gate
    harness::tendr(&root)
        .args(["start", "job1", "--", "true"])
        .assert()
        .success();
    harness::wait_terminal(&root, "job1");

    let gate = root.path().join("job3-gate");
    harness::tendr(&root)
        .args(["start", "job3", "--"])
        .args(until_exists(&gate))
        .assert()
        .success();
    harness::wait_running(&root, "job3");

    // job2 depends on both: job1 (already terminal) and job3 (still running)
    harness::tendr(&root)
        .args([
            "start", "job2", "--after", "job1", "--after", "job3", "--", "true",
        ])
        .assert()
        .success();

    // start returning proves the first scan ran: job1 (already terminal) is
    // latched at its bound run_id. No sleep — the replace below is deterministically
    // ordered after the latch.

    // Replace job1 — this would fail the waiter without latching
    harness::tendr(&root)
        .env(
            "TENDR_TEST_REPLACE_WAIT_MS",
            harness::HANG.as_millis().to_string(),
        )
        .args(["start", "job1", "--replace", "--", "sleep", "60"])
        .assert()
        .success();

    // job3 finishes, job2 should succeed (job1 was already latched)
    std::fs::write(&gate, "").unwrap();
    harness::wait_terminal(&root, "job3");
    let meta = harness::wait_terminal(&root, "job2");
    assert_eq!(meta["status"].as_str(), Some("Exited"));
    assert_eq!(meta["reason"].as_str(), Some("ExitedOk"));
}
