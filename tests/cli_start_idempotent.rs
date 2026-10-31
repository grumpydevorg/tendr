mod harness;

use harness::{tendr, test_callback_bin, wait_running, wait_terminal};
use predicates::prelude::*;
use std::sync::Mutex;
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

#[test]
fn start_same_spec_is_idempotent() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let out1 = tendr(&root)
        .args(["start", "idem-same", "sleep", "60"])
        .output()
        .unwrap();
    assert!(out1.status.success(), "first start failed");
    wait_running(&root, "idem-same");

    let meta1: serde_json::Value =
        serde_json::from_slice(&out1.stdout).expect("first output not JSON");
    let run_id1 = meta1["run_id"].as_str().expect("no run_id");

    let out2 = tendr(&root)
        .args(["start", "idem-same", "sleep", "60"])
        .output()
        .unwrap();
    assert!(
        out2.status.success(),
        "second start should succeed (idempotent)"
    );

    let meta2: serde_json::Value =
        serde_json::from_slice(&out2.stdout).expect("second output not JSON");
    let run_id2 = meta2["run_id"]
        .as_str()
        .expect("no run_id in second output");

    assert_eq!(
        run_id1, run_id2,
        "idempotent start should return same run_id"
    );

    tendr(&root)
        .args(["kill", "--force", "idem-same"])
        .assert()
        .success();
}

#[test]
fn start_different_spec_is_conflict() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "idem-diff", "sleep", "60"])
        .assert()
        .success();
    wait_running(&root, "idem-diff");

    tendr(&root)
        .args(["start", "idem-diff", "echo", "hi"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("session conflict"));

    tendr(&root)
        .args(["kill", "--force", "idem-diff"])
        .assert()
        .success();
}

#[test]
fn start_after_terminal_is_error() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "idem-term", "true"])
        .assert()
        .success();
    wait_terminal(&root, "idem-term");

    tendr(&root)
        .args(["start", "idem-term", "true"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("terminal state"));
}

#[test]
fn start_with_cwd_child_runs_in_requested_directory() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let work_dir = root.path().join("myworkdir");
    std::fs::create_dir_all(&work_dir).unwrap();

    let out = tendr(&root)
        .args([
            "start",
            "cwd-test",
            "--cwd",
            work_dir.to_str().unwrap(),
            "--",
            &test_callback_bin(),
            "print-cwd",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    wait_terminal(&root, "cwd-test");

    let log_out = tendr(&root)
        .args(["log", "cwd-test", "--raw"])
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&log_out.stdout);
    assert!(
        log.contains(work_dir.to_str().unwrap()),
        "child should run in {work_dir:?}, got log: {log}"
    );
}

#[test]
fn start_with_env_child_sees_overridden_vars() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let out = tendr(&root)
        .args([
            "start",
            "env-test",
            "--env",
            "TENDR_TEST_VAR=hello_from_tendr",
            "--",
            "sh",
            "-c",
            "echo $TENDR_TEST_VAR",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    wait_terminal(&root, "env-test");

    let log_out = tendr(&root)
        .args(["log", "env-test", "--raw"])
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&log_out.stdout);
    assert!(
        log.contains("hello_from_tendr"),
        "child should see TENDR_TEST_VAR, got log: {log}"
    );
}

#[test]
fn start_with_different_cwd_is_spec_conflict() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let dir_a = root.path().join("dir_a");
    let dir_b = root.path().join("dir_b");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();

    let out1 = tendr(&root)
        .args([
            "start",
            "cwd-conflict",
            "--cwd",
            dir_a.to_str().unwrap(),
            "--",
            "sleep",
            "60",
        ])
        .output()
        .unwrap();
    assert!(out1.status.success());

    let out2 = tendr(&root)
        .args([
            "start",
            "cwd-conflict",
            "--cwd",
            dir_b.to_str().unwrap(),
            "--",
            "sleep",
            "60",
        ])
        .output()
        .unwrap();
    assert!(
        !out2.status.success(),
        "different cwd should be a spec conflict"
    );
    let stderr = String::from_utf8_lossy(&out2.stderr);
    assert!(
        stderr.contains("session conflict"),
        "expected conflict error, got: {stderr}"
    );

    // Cleanup: kill the running session
    tendr(&root)
        .args(["kill", "cwd-conflict", "--force"])
        .output()
        .unwrap();
    wait_terminal(&root, "cwd-conflict");
}

#[test]
fn start_with_different_env_is_spec_conflict() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let out1 = tendr(&root)
        .args([
            "start",
            "env-conflict",
            "--env",
            "FOO=bar",
            "--",
            "sleep",
            "60",
        ])
        .output()
        .unwrap();
    assert!(out1.status.success());

    let out2 = tendr(&root)
        .args([
            "start",
            "env-conflict",
            "--env",
            "FOO=baz",
            "--",
            "sleep",
            "60",
        ])
        .output()
        .unwrap();
    assert!(
        !out2.status.success(),
        "different env should be a spec conflict"
    );
    let stderr = String::from_utf8_lossy(&out2.stderr);
    assert!(
        stderr.contains("session conflict"),
        "expected conflict error, got: {stderr}"
    );

    // Cleanup: kill the running session
    tendr(&root)
        .args(["kill", "env-conflict", "--force"])
        .output()
        .unwrap();
    wait_terminal(&root, "env-conflict");
}

#[test]
fn start_with_invalid_env_format_fails() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let out = tendr(&root)
        .args(["start", "bad-env", "--env", "NO_EQUALS_SIGN", "--", "true"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "malformed --env should fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("KEY=VALUE"),
        "error should mention expected format, got: {stderr}"
    );
}

#[test]
fn start_with_empty_env_key_fails() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let out = tendr(&root)
        .args(["start", "empty-key", "--env", "=foo", "--", "true"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "--env '=foo' should fail (empty key)"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("key cannot be empty"),
        "error should mention empty key, got: {stderr}"
    );
}

/// Start `sh -c 'echo CI=${CI-unset}'` with `CI=true` in tendr's own
/// environment, plus `extra` tendr flags. Returns the child's output.
fn start_ci_probe(root: &TempDir, name: &str, extra: &[&str]) -> String {
    let out = tendr(root)
        .env("CI", "true")
        .args(["start", name])
        .args(extra)
        .args(["--", "sh", "-c", "echo CI=${CI-unset}"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    wait_terminal(root, name);
    let log_out = tendr(root).args(["log", name, "--raw"]).output().unwrap();
    String::from_utf8_lossy(&log_out.stdout).into_owned()
}

#[test]
fn start_with_unset_env_child_does_not_see_inherited_var() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Control: without the flag the child inherits CI from tendr.
    let log = start_ci_probe(&root, "ci-inherit", &[]);
    assert!(log.contains("CI=true"), "child should inherit CI: {log}");

    let log = start_ci_probe(&root, "ci-unset", &["--unset-env", "CI"]);
    assert!(
        log.contains("CI=unset"),
        "--unset-env CI should remove CI from the child, got log: {log}"
    );
}

#[test]
fn start_with_unset_env_and_env_of_same_name_sets_it() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let log = start_ci_probe(
        &root,
        "ci-override",
        &["--unset-env", "CI", "--env", "CI=forced"],
    );
    assert!(
        log.contains("CI=forced"),
        "--env should win over --unset-env, got log: {log}"
    );
}

#[test]
fn start_with_invalid_unset_env_name_fails() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    for (name, bad, expected) in [
        ("unset-eq", "CI=true", "cannot contain '='"),
        ("unset-empty", "", "name cannot be empty"),
    ] {
        let out = tendr(&root)
            .args(["start", name, "--unset-env", bad, "--", "true"])
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "--unset-env {bad:?} should be refused"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("invalid --unset-env") && stderr.contains(expected),
            "--unset-env {bad:?} error should say {expected:?}, got: {stderr}"
        );
    }
}

#[test]
fn start_with_env_preserves_inherited_environment() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let out = tendr(&root)
        .args([
            "start",
            "env-inherit",
            "--env",
            "TENDR_EXTRA=added",
            "--",
            "sh",
            "-c",
            "echo PATH=$PATH TENDR_EXTRA=$TENDR_EXTRA",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    wait_terminal(&root, "env-inherit");

    let log_out = tendr(&root)
        .args(["log", "env-inherit", "--raw"])
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&log_out.stdout);
    assert!(
        log.contains("PATH="),
        "child should inherit PATH from parent"
    );
    assert!(
        log.contains("TENDR_EXTRA=added"),
        "child should see override"
    );
}

#[test]
fn start_with_same_cwd_and_env_is_idempotent() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let work_dir = root.path().join("samedir");
    std::fs::create_dir_all(&work_dir).unwrap();

    let out1 = tendr(&root)
        .args([
            "start",
            "idem-cwd-env",
            "--cwd",
            work_dir.to_str().unwrap(),
            "--env",
            "FOO=bar",
            "--",
            "sleep",
            "60",
        ])
        .output()
        .unwrap();
    assert!(out1.status.success());
    let meta1: serde_json::Value = serde_json::from_slice(&out1.stdout).unwrap();
    let run_id1 = meta1["run_id"].as_str().unwrap().to_string();

    let out2 = tendr(&root)
        .args([
            "start",
            "idem-cwd-env",
            "--cwd",
            work_dir.to_str().unwrap(),
            "--env",
            "FOO=bar",
            "--",
            "sleep",
            "60",
        ])
        .output()
        .unwrap();
    assert!(out2.status.success());
    let meta2: serde_json::Value = serde_json::from_slice(&out2.stdout).unwrap();
    let run_id2 = meta2["run_id"].as_str().unwrap().to_string();

    assert_eq!(
        run_id1, run_id2,
        "same spec with cwd+env should be idempotent"
    );

    // Cleanup: kill the running session
    tendr(&root)
        .args(["kill", "idem-cwd-env", "--force"])
        .output()
        .unwrap();
    wait_terminal(&root, "idem-cwd-env");
}
