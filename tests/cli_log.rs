mod harness;
use harness::Home;

use harness::{DeadlineAssertExt, tendr, wait_terminal};
use predicates::prelude::*;
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

#[test]
fn log_shows_child_output() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    tendr(&root)
        .args(["start", "log-echo", "echo", "hello from child"])
        .assert()
        .success();
    wait_terminal(&root, "log-echo");

    tendr(&root)
        .args(["log", "log-echo"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello from child"));
}

#[test]
fn log_tail() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    tendr(&root)
        .args([
            "start",
            "log-tail",
            "sh",
            "-c",
            "echo line1; echo line2; echo line3",
        ])
        .assert()
        .success();
    wait_terminal(&root, "log-tail");

    tendr(&root)
        .args(["log", "--tail", "1", "log-tail"])
        .assert()
        .success()
        .stdout(predicate::str::contains("line3"))
        .stdout(predicate::str::contains("line1").not());
}

#[test]
fn log_raw_strips_prefix() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    tendr(&root)
        .args(["start", "log-raw", "echo", "just content"])
        .assert()
        .success();
    wait_terminal(&root, "log-raw");

    let output = tendr(&root)
        .args(["log", "--raw", "log-raw"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("just content"));
    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        assert!(
            !line.contains("\"tag\""),
            "raw line should not contain JSON envelope: {line}"
        );
    }
}

#[test]
fn log_nonexistent_session_fails() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    tendr(&root).args(["log", "nope"]).assert().failure();
}

#[test]
fn log_no_output_file_returns_empty() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    tendr(&root)
        .args(["start", "nolog-test", "/nonexistent/binary"])
        .assert()
        .code(2);

    wait_terminal(&root, "nolog-test");

    tendr(&root)
        .args(["log", "nolog-test"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn log_stderr_captured() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    tendr(&root)
        .args(["start", "log-stderr", "sh", "-c", "echo err >&2"])
        .assert()
        .success();
    wait_terminal(&root, "log-stderr");

    tendr(&root)
        .args(["log", "log-stderr"])
        .assert()
        .success()
        .stdout(predicate::str::contains("err"))
        .stdout(predicate::str::contains("\"tag\":\"E\""));
}

#[test]
fn log_since_filters_by_time() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    tendr(&root)
        .args([
            "start",
            "log-since",
            "sh",
            "-c",
            "echo early; sleep 1; echo late",
        ])
        .assert()
        .success();
    wait_terminal(&root, "log-since");

    // Full log has both lines
    tendr(&root)
        .args(["log", "log-since"])
        .assert()
        .success()
        .stdout(predicate::str::contains("early"))
        .stdout(predicate::str::contains("late"));

    // Epoch far in the future — should return 0 lines
    tendr(&root)
        .args(["log", "--since", "9999999999", "log-since"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn log_follow_stops_on_terminal_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    tendr(&root)
        .args(["start", "log-follow", "echo", "follow me"])
        .assert()
        .success();
    wait_terminal(&root, "log-follow");

    // `--follow` on an already-terminal session must stop rather than block
    // forever; the hang detector guards that, and the stdout assertion proves
    // the tail content was emitted.
    tendr(&root)
        .args(["log", "--follow", "--tail", "10", "log-follow"])
        .assert_within_deadline()
        .success()
        .stdout(predicate::str::contains("follow me"));
}
