//! A reader that exits early (`tendr status x | head -c1`) closes stdout's
//! pipe. The command must not panic on the broken pipe: it drops the rest of
//! its output quietly and keeps its exit code.

#![cfg(unix)]

mod harness;

use harness::{tendr, wait_terminal};
use std::os::fd::{FromRawFd, OwnedFd};
use std::process::{Command, Stdio};
use tempfile::TempDir;

/// Run `tendr <args>` with stdout a pipe whose reader has already closed.
fn run_into_closed_pipe(root: &TempDir, args: &[&str]) -> std::process::Output {
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let (reader, writer) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    drop(reader);
    Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args(args)
        .env("HOME", root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::from(writer))
        .stderr(Stdio::piped())
        .output()
        .unwrap()
}

#[test]
fn read_commands_survive_a_reader_that_exits_early() {
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "s", "--", "sh", "-c", "seq 1 2000"])
        .assert()
        .success();
    wait_terminal(&root, "s");

    for args in [
        &["status", "s"][..],
        &["list"],
        &["log", "s"],
        &["wait", "s"],
        &["guide"],
    ] {
        let output = run_into_closed_pipe(&root, args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stderr.is_empty(),
            "tendr {args:?} into a closed pipe: {:?}, stderr: {stderr}",
            output.status
        );
    }
}
