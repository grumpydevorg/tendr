#![cfg(windows)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read};
use tendr::platform::Platform;
use tendr::platform::windows::WindowsPlatform;

/// Spawn a child, verify it gets a valid identity.
#[test]
fn windows_spawn_child_identity() {
    let argv = vec!["cmd".into(), "/C".into(), "echo hello".into()];
    let mut child =
        WindowsPlatform::spawn_child(&argv, false, None, &BTreeSet::new(), &BTreeMap::new())
            .expect("spawn_child should succeed");

    let id = WindowsPlatform::child_identity(&child);
    assert!(id.pid.get() > 0);
    assert!(id.start_time_ns > 0);

    let status = WindowsPlatform::child_wait(&mut child).expect("wait should succeed");
    assert!(status.success());
}

/// Spawn, verify stdout is captured.
#[test]
fn windows_spawn_captures_stdout() {
    let argv = vec!["cmd".into(), "/C".into(), "echo hello-windows".into()];
    let mut child =
        WindowsPlatform::spawn_child(&argv, false, None, &BTreeSet::new(), &BTreeMap::new())
            .expect("spawn_child should succeed");

    let mut stdout = WindowsPlatform::child_stdout(&mut child).expect("stdout should be available");

    let status = WindowsPlatform::child_wait(&mut child).expect("wait should succeed");
    assert!(status.success());

    let mut buf = String::new();
    stdout.read_to_string(&mut buf).unwrap();
    assert!(
        buf.contains("hello-windows"),
        "stdout should contain output, got: {buf}"
    );
}

/// try_wait returns None while child is running, Some after exit.
#[test]
fn windows_try_wait_while_running() {
    let argv = vec!["cmd".into(), "/C".into(), "timeout /t 10 /nobreak".into()];
    let mut child =
        WindowsPlatform::spawn_child(&argv, false, None, &BTreeSet::new(), &BTreeMap::new())
            .expect("spawn_child should succeed");

    // Child should still be running
    let poll = WindowsPlatform::child_try_wait(&mut child).expect("try_wait should not error");
    assert!(poll.is_none(), "child should still be running");

    // Force kill it
    let handle = WindowsPlatform::child_kill_handle(&child);
    WindowsPlatform::kill_child(&handle, true).expect("force kill should succeed");

    // Wait for exit
    let status = WindowsPlatform::child_wait(&mut child).expect("wait should succeed");
    assert!(
        !status.success(),
        "force-killed child should have non-zero exit"
    );
}

/// Force kill terminates the child immediately.
#[test]
fn windows_force_kill() {
    let argv = vec!["cmd".into(), "/C".into(), "timeout /t 60 /nobreak".into()];
    let mut child =
        WindowsPlatform::spawn_child(&argv, false, None, &BTreeSet::new(), &BTreeMap::new())
            .expect("spawn_child should succeed");

    let handle = WindowsPlatform::child_kill_handle(&child);
    WindowsPlatform::kill_child(&handle, true).expect("force kill should succeed");

    let status = WindowsPlatform::child_wait(&mut child).expect("wait should succeed");
    assert!(!status.success());
}

/// Force kill is idempotent — killing an already-dead process is Ok.
#[test]
fn windows_force_kill_idempotent() {
    let argv = vec!["cmd".into(), "/C".into(), "echo done".into()];
    let mut child =
        WindowsPlatform::spawn_child(&argv, false, None, &BTreeSet::new(), &BTreeMap::new())
            .expect("spawn_child should succeed");

    let handle = WindowsPlatform::child_kill_handle(&child);
    WindowsPlatform::child_wait(&mut child).expect("wait should succeed");

    // Kill again — child is already dead, should not error.
    WindowsPlatform::kill_child(&handle, true).expect("double kill should be idempotent");
}

/// Graceful kill sends CTRL_BREAK, escalates to force after timeout.
#[test]
fn windows_kill_graceful_escalates() {
    let argv = vec!["cmd".into(), "/C".into(), "timeout /t 60 /nobreak".into()];
    let mut child =
        WindowsPlatform::spawn_child(&argv, false, None, &BTreeSet::new(), &BTreeMap::new())
            .expect("spawn_child should succeed");

    let handle = WindowsPlatform::child_kill_handle(&child);

    // Graceful kill — timeout /t 60 /nobreak ignores CTRL_BREAK,
    // so this should escalate to TerminateJobObject after ~5s.
    WindowsPlatform::kill_child(&handle, false).expect("graceful kill should succeed");

    let status = WindowsPlatform::child_wait(&mut child).expect("wait should succeed");
    assert!(
        !status.success(),
        "escalated kill should produce non-zero exit"
    );
}

/// Stdin piping works.
#[test]
fn windows_spawn_with_stdin() {
    use std::io::Write;

    // `findstr .` reads stdin and echoes lines that match "." (i.e., everything)
    let argv = vec!["findstr".into(), ".".into()];
    let mut child =
        WindowsPlatform::spawn_child(&argv, true, None, &BTreeSet::new(), &BTreeMap::new())
            .expect("spawn_child should succeed");

    let mut stdin = WindowsPlatform::child_stdin(&mut child).expect("stdin should be available");
    stdin.write_all(b"hello from stdin\n").unwrap();
    drop(stdin); // close pipe — child sees EOF

    let mut stdout = WindowsPlatform::child_stdout(&mut child).expect("stdout should be available");

    let status = WindowsPlatform::child_wait(&mut child).expect("wait should succeed");
    assert!(status.success());

    let mut buf = String::new();
    stdout.read_to_string(&mut buf).unwrap();
    assert!(
        buf.contains("hello from stdin"),
        "stdout should echo stdin, got: {buf}"
    );
}

/// Environment variables are passed to child.
#[test]
fn windows_spawn_with_env() {
    let mut env = BTreeMap::new();
    env.insert("TENDR_TEST_VAR".into(), "hello-env".into());

    let argv = vec!["cmd".into(), "/C".into(), "echo %TENDR_TEST_VAR%".into()];
    let mut child = WindowsPlatform::spawn_child(&argv, false, None, &BTreeSet::new(), &env)
        .expect("spawn_child should succeed");

    let mut stdout = WindowsPlatform::child_stdout(&mut child).expect("stdout should be available");

    let status = WindowsPlatform::child_wait(&mut child).expect("wait should succeed");
    assert!(status.success());

    let mut buf = String::new();
    stdout.read_to_string(&mut buf).unwrap();
    assert!(
        buf.contains("hello-env"),
        "env var should be visible, got: {buf}"
    );
}

/// Cooperative CTRL_BREAK: child handles the signal and exits cleanly with code 42.
/// Proves graceful kill works without escalating to TerminateJobObject.
#[test]
fn windows_kill_graceful_cooperative() {
    let helper = env!("CARGO_BIN_EXE_ctrl_break_responder");
    let argv = vec![helper.to_string()];
    let mut child =
        WindowsPlatform::spawn_child(&argv, false, None, &BTreeSet::new(), &BTreeMap::new())
            .expect("spawn_child should succeed");

    // The helper prints READY once its console control handler is installed, so a
    // CTRL_BREAK sent after this line is guaranteed to be caught — replacing a
    // fixed sleep. Read it on a thread with a deadline so a silent helper fails
    // fast with a clear message instead of hanging on a blocking read.
    let stdout = WindowsPlatform::child_stdout(&mut child).expect("stdout should be available");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(line);
    });
    // The responder exits(1) if SetConsoleCtrlHandler fails, so READY is only ever
    // published by a process whose handler is genuinely installed.
    let ready = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("ctrl_break_responder never printed READY (control handler not installed?)");
    assert_eq!(
        ready.trim(),
        "READY",
        "expected exactly READY from ctrl_break_responder, got: {ready:?}"
    );

    let handle = WindowsPlatform::child_kill_handle(&child);

    WindowsPlatform::kill_child(&handle, false).expect("graceful kill should succeed");

    let status = WindowsPlatform::child_wait(&mut child).expect("wait should succeed");

    // Exit code 42 proves the child handled CTRL_BREAK cooperatively and exited
    // on its own. Forced escalation (TerminateJobObject after the 5s grace)
    // would instead produce exit code 1 — so the cooperative exit code alone
    // proves no escalation occurred, with no fragile sub-escalation wall-clock bound.
    assert_eq!(
        status.code(),
        Some(42),
        "child should exit with code 42 from cooperative CTRL_BREAK handler, got: {status:?}"
    );
}
