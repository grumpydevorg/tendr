//! A descriptor the caller leaked into `tendr start` must not reach the
//! sidecar or the child. exe.dev's init leaks two /dev/ptmx masters into every
//! SSH command; passed on, they keep those terminals open for the run's life.
#![cfg(unix)]

mod harness;

use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// An fd number nothing in the test or tendr opens by itself.
const LEAKED_FD: i32 = 47;

fn meta(root: &TempDir, session: &str) -> serde_json::Value {
    let path = root
        .path()
        .join(format!(".tendr/sessions/default/{session}/meta.json"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The files process `pid` has open, by path.
fn open_paths(pid: u64) -> Vec<PathBuf> {
    if cfg!(target_os = "linux") {
        std::fs::read_dir(format!("/proc/{pid}/fd"))
            .unwrap()
            .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
            .collect()
    } else {
        // -Fn prints one `n<name>` line per open file.
        let out = std::process::Command::new("lsof")
            .args(["-n", "-P", "-p", &pid.to_string(), "-Fn"])
            .output()
            .expect("lsof");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.strip_prefix('n'))
            .map(PathBuf::from)
            .collect()
    }
}

/// Start a run from a shell that holds a non-CLOEXEC fd on `marker` open, as
/// exe-init does with its terminals.
fn start_with_leaked_fd(root: &TempDir, session: &str, pty: bool, marker: &Path) {
    let mode = if pty { "--pty" } else { "" };
    let script =
        format!("exec {LEAKED_FD}<\"$MARKER\"; exec \"$TENDR\" start {session} {mode} -- sleep 60");
    // bash, not sh: dash cannot redirect an fd above 9.
    let status = std::process::Command::new("bash")
        .args(["-c", &script])
        .env("HOME", root.path())
        .env("TENDR", assert_cmd::cargo::cargo_bin("tendr"))
        .env("MARKER", marker)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "tendr start failed");
}

fn assert_fd_not_inherited(pty: bool) {
    let root = TempDir::new().unwrap();
    let session = if pty { "fd-pty" } else { "fd-pipe" };
    // Canonical, so it matches what /proc and lsof report (macOS's /var is a
    // symlink to /private/var).
    let marker = root.path().join("leaked-marker");
    std::fs::write(&marker, "").unwrap();
    let marker = marker.canonicalize().unwrap();

    start_with_leaked_fd(&root, session, pty, &marker);
    let _guard = harness::SessionGuard::new(&root, session);
    harness::wait_running(&root, session);

    let meta = meta(&root, session);
    let sidecar = meta["sidecar"]["pid"].as_u64().expect("sidecar.pid");
    let child = meta["child"]["pid"].as_u64().expect("child.pid");
    for (who, pid) in [("sidecar", sidecar), ("child", child)] {
        let paths = open_paths(pid);
        assert!(
            !paths.contains(&marker),
            "the {who} {pid} inherited the leaked fd on {}: {paths:?}",
            marker.display()
        );
    }
}

#[test]
fn leaked_fd_does_not_reach_sidecar_or_pipe_child() {
    assert_fd_not_inherited(false);
}

#[test]
fn leaked_fd_does_not_reach_sidecar_or_pty_child() {
    assert_fd_not_inherited(true);
}
