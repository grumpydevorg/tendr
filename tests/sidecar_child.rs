// These tests spawn real processes (CLI -> sidecar -> child).
mod harness;
use harness::Home;

use harness::{tendr, wait_terminal, wait_terminal_quiescent};
use predicates::prelude::*;
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

fn read_log(root: &Home, session: &str) -> String {
    wait_terminal(root, session);
    let path = root
        .path()
        .join(format!(".tendr/sessions/default/{session}/output.log"));
    std::fs::read_to_string(&path).unwrap_or_default()
}

fn parse_log_lines(log: &str) -> Vec<serde_json::Value> {
    log.lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect()
}

#[test]
fn start_returns_running_with_child() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    let output = tendr(&root)
        .args(["start", "echo-job", "echo", "hello"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let meta: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(meta["status"], "Running");
    assert!(meta["child"]["pid"].is_number());
    assert!(meta["child"]["start_time_ns"].is_number());
}

#[test]
fn child_exit_ok_produces_exited_ok() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    tendr(&root)
        .args(["start", "ok-job", "true"])
        .assert()
        .success();

    let meta = wait_terminal(&root, "ok-job");
    assert_eq!(meta["status"], "Exited");
    assert_eq!(meta["reason"], "ExitedOk");
    assert!(meta["ended_at"].is_string());
    assert!(meta["child"]["pid"].is_number());
}

#[test]
fn child_exit_error_produces_exited_error() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    tendr(&root)
        .args(["start", "err-job", "sh", "-c", "exit 42"])
        .assert()
        .success();

    let meta = wait_terminal(&root, "err-job");
    assert_eq!(meta["status"], "Exited");
    assert_eq!(meta["reason"], "ExitedError");
    assert_eq!(meta["code"], 42);
}

#[test]
fn stdout_captured_to_output_log() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    tendr(&root)
        .args(["start", "stdout-job", "echo", "hello world"])
        .assert()
        .success();

    let log = read_log(&root, "stdout-job");
    assert!(log.contains("\"content\":\"hello world\""), "log: {log}");

    for line in parse_log_lines(&log) {
        assert!(line["ts"].is_number(), "timestamp missing: {line}");
        assert!(
            line["tag"] == "O" || line["tag"] == "E",
            "bad tag: {}",
            line["tag"]
        );
        assert!(
            line["content"].is_string(),
            "content should be string: {line}"
        );
    }
}

#[test]
fn stderr_captured_to_output_log() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    tendr(&root)
        .args(["start", "stderr-job", "sh", "-c", "echo error >&2"])
        .assert()
        .success();

    let log = read_log(&root, "stderr-job");
    assert!(log.contains("\"tag\":\"E\""), "log: {log}");
    assert!(log.contains("\"content\":\"error\""), "log: {log}");
}

#[test]
fn interleaved_stdout_stderr() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    tendr(&root)
        .args([
            "start",
            "interleave-job",
            "sh",
            "-c",
            "echo out1; echo err1 >&2; echo out2",
        ])
        .assert()
        .success();

    let log = read_log(&root, "interleave-job");
    assert!(
        log.contains("\"content\":\"out1\""),
        "missing out1 in: {log}"
    );
    assert!(
        log.contains("\"content\":\"err1\""),
        "missing err1 in: {log}"
    );
    assert!(
        log.contains("\"content\":\"out2\""),
        "missing out2 in: {log}"
    );
}

#[test]
fn spawn_failure_produces_spawn_failed() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    let output = tendr(&root)
        .args(["start", "bad-cmd", "nonexistent-command-xyz-12345"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));

    let meta: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(meta["status"], "SpawnFailed");
}

#[test]
fn child_identity_preserved_in_terminal_state() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    let output = tendr(&root)
        .args(["start", "preserve-job", "echo", "hi"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let start_meta: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let start_pid = start_meta["child"]["pid"].as_u64().unwrap();

    let final_meta = wait_terminal(&root, "preserve-job");
    let final_pid = final_meta["child"]["pid"].as_u64().unwrap();
    assert_eq!(start_pid, final_pid);
}

#[test]
fn lock_released_after_child_exits() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    tendr(&root)
        .args(["start", "lock-job", "echo", "hi"])
        .assert()
        .success();
    let _terminal = wait_terminal_quiescent(&root, "lock-job");
}

#[test]
fn status_shows_terminal_after_child_exits() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    tendr(&root)
        .args(["start", "status-job", "echo", "hi"])
        .assert()
        .success();
    wait_terminal(&root, "status-job");

    tendr(&root)
        .args(["status", "status-job"])
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""status": "Exited"#))
        .stdout(predicate::str::contains(r#""reason": "ExitedOk"#));
}
