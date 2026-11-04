mod harness;

use harness::{
    DeadlineAssertExt, HANG, SessionGuard, tendr, wait_log_tail, wait_running, wait_terminal,
};
use predicates::prelude::*;
use std::sync::Mutex;
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

/// Kill `session` once the sidecar records it, waiting for that up to the hang
/// deadline rather than kill's 11 s before it kills the child itself.
fn kill(root: &TempDir, session: &str) {
    tendr(root)
        .env("TENDR_TEST_KILL_WAIT_MS", HANG.as_millis().to_string())
        .args(["kill", session])
        .assert()
        .success();
}

fn tendr_with_stdin(root: &TempDir, args: &[&str], input: &[u8]) -> std::process::Output {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let bin = assert_cmd::cargo::cargo_bin("tendr");
    let mut child = Command::new(bin)
        .args(args)
        .env("HOME", root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn tendr");
    // A command expected to fail can exit before reading its stdin, so a broken
    // pipe here is a legitimate outcome; callers assert on the exit status.
    match child.stdin.take().unwrap().write_all(input) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
        other => other.unwrap(),
    }
    child.wait_with_output().unwrap()
}

#[test]
fn push_delivers_stdin_to_child() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "--stdin", "push-echo", "cat"])
        .assert()
        .success();
    wait_running(&root, "push-echo");

    let push_out = tendr_with_stdin(&root, &["push", "push-echo"], b"hello from push\n");
    assert!(push_out.status.success());

    // push returns once the bytes reach cat's stdin, not once cat has echoed
    // them: wait for the echo before the kill can cut it off.
    wait_log_tail(&root, "push-echo", "hello from push");
    kill(&root, "push-echo");
    wait_terminal(&root, "push-echo");

    tendr(&root)
        .args(["log", "push-echo"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello from push"));
}

#[test]
fn push_multiple_sequential() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "--stdin", "push-multi", "cat"])
        .assert()
        .success();
    wait_running(&root, "push-multi");

    let out1 = tendr_with_stdin(&root, &["push", "push-multi"], b"first line\n");
    assert!(out1.status.success());

    let out2 = tendr_with_stdin(&root, &["push", "push-multi"], b"second line\n");
    assert!(out2.status.success());

    // cat echoes in order: the second line logged means both were.
    wait_log_tail(&root, "push-multi", "second line");
    kill(&root, "push-multi");
    wait_terminal(&root, "push-multi");

    tendr(&root)
        .args(["log", "push-multi"])
        .assert()
        .success()
        .stdout(predicate::str::contains("first line"))
        .stdout(predicate::str::contains("second line"));
}

#[test]
fn push_to_session_without_stdin_fails() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "nostdin-job", "sleep", "3600"])
        .assert()
        .success();
    let _session = SessionGuard::new(&root, "nostdin-job");
    wait_running(&root, "nostdin-job");

    let push_out = tendr_with_stdin(&root, &["push", "nostdin-job"], b"nope\n");
    assert!(!push_out.status.success());
    let stderr = String::from_utf8_lossy(&push_out.stderr);
    assert!(stderr.contains("not started with --stdin"));

    kill(&root, "nostdin-job");
    wait_terminal(&root, "nostdin-job");
}

#[test]
fn push_to_nonexistent_session_fails() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let push_out = tendr_with_stdin(&root, &["push", "nope"], b"data\n");
    assert!(!push_out.status.success());
}

#[test]
fn push_to_terminal_session_fails() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "--stdin", "term-push", "true"])
        .assert()
        .success();
    wait_terminal(&root, "term-push");

    let push_out = tendr_with_stdin(&root, &["push", "term-push"], b"too late\n");
    assert!(!push_out.status.success());
    let stderr = String::from_utf8_lossy(&push_out.stderr);
    assert!(stderr.contains("not running"));
}

#[test]
fn push_fails_promptly_when_session_dies() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "--stdin", "push-die", "sleep", "1"])
        .assert()
        .success();
    wait_terminal(&root, "push-die");

    // push to a dead session must fail promptly, not hang on the dead stdin
    // pipe; the hang detector bounds it and `.failure()` proves the result.
    tendr(&root)
        .args(["push", "push-die"])
        .write_stdin(b"hello\n".to_vec())
        .assert_within_deadline()
        .failure();
}

#[test]
fn push_immediately_after_start() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "--stdin", "imm-push", "cat"])
        .assert()
        .success();

    let push_out = tendr_with_stdin(&root, &["push", "imm-push"], b"immediate data\n");
    assert!(push_out.status.success());

    wait_log_tail(&root, "imm-push", "immediate data");
    kill(&root, "imm-push");
    wait_terminal(&root, "imm-push");

    tendr(&root)
        .args(["log", "imm-push"])
        .assert()
        .success()
        .stdout(predicate::str::contains("immediate data"));
}
