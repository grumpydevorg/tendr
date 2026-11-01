//! A descriptor the caller leaked into `tendr start` must not reach the
//! sidecar or the child. exe.dev's init leaks two /dev/ptmx masters into every
//! SSH command; passed on, they keep those terminals open for the run's life.
#![cfg(unix)]

mod harness;

use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;

/// An fd number nothing in the test or tendr opens by itself.
const LEAKED_FD: i32 = 47;

fn sidecar_pid(root: &TempDir, session: &str) -> i32 {
    let path = root
        .path()
        .join(format!(".tendr/sessions/default/{session}/meta.json"));
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    meta["sidecar"]["pid"].as_u64().expect("sidecar.pid") as i32
}

/// Whether process `pid` has `fd` open.
fn has_fd(pid: i32, fd: i32) -> bool {
    if cfg!(target_os = "linux") {
        Path::new(&format!("/proc/{pid}/fd/{fd}")).exists()
    } else {
        let out = std::process::Command::new("lsof")
            .args(["-a", "-p", &pid.to_string(), "-d", &fd.to_string()])
            .output()
            .expect("lsof");
        !out.stdout.is_empty()
    }
}

/// Start a run from a shell that holds a non-CLOEXEC fd open, as exe-init does.
fn start_with_leaked_fd(root: &TempDir, session: &str, pty: bool) -> std::path::PathBuf {
    let report = root.path().join(format!("{session}.fd"));
    let child = format!(
        "if [ -e /dev/fd/{LEAKED_FD} ]; then echo leaked; else echo clean; fi > '{}'; sleep 60",
        report.display()
    );
    let mode = if pty { "--pty" } else { "" };
    let script = format!(
        "exec {LEAKED_FD}</dev/null; exec \"$TENDR\" start {session} {mode} -- sh -c \"$CHILD\""
    );
    let tendr_bin = assert_cmd::cargo::cargo_bin("tendr");
    // bash, not sh: dash cannot redirect an fd above 9.
    let status = std::process::Command::new("bash")
        .args(["-c", &script])
        .env("HOME", root.path())
        .env("TENDR", tendr_bin)
        .env("CHILD", child)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "tendr start failed");
    report
}

fn assert_fd_not_inherited(pty: bool) {
    let root = TempDir::new().unwrap();
    let session = if pty { "fd-pty" } else { "fd-pipe" };
    let report = start_with_leaked_fd(&root, session, pty);
    let _guard = harness::SessionGuard::new(&root, session);

    let seen = harness::poll_until(Duration::from_secs(10), Duration::from_millis(50), || {
        match std::fs::read_to_string(&report) {
            Ok(s) if s.ends_with('\n') => harness::Observation::Ready(s),
            _ => harness::Observation::Pending("no report yet".into()),
        }
    })
    .expect("child never reported its descriptors");
    let pid = sidecar_pid(&root, session);
    let sidecar_has = has_fd(pid, LEAKED_FD);
    assert!(
        seen.trim() == "clean" && !sidecar_has,
        "fd {LEAKED_FD} leaked: child reported {:?}, sidecar {pid} has it: {sidecar_has}",
        seen.trim()
    );
}

#[test]
fn leaked_fd_does_not_reach_sidecar_or_pipe_child() {
    assert_fd_not_inherited(false);
}

#[test]
fn leaked_fd_does_not_reach_sidecar_or_pty_child() {
    assert_fd_not_inherited(true);
}
