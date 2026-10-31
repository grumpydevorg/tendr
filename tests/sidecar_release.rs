//! `wait`, `kill` and `start --replace` treat a session as done only once its
//! sidecar has released the session lock, not as soon as terminal metadata is
//! written (#91). The sidecar writes its final state just before releasing the
//! lock; a command run straight after `wait` (prune, `start --replace`) could
//! otherwise still find the session locked.
//!
//! The window is held open deterministically by the debug-only
//! `TENDR_TEST_UNLOCK_GATE` hook: the sidecar waits for the gate file between
//! its terminal meta write and releasing the lock.

mod harness;

use harness::{DeadlineAssertExt, tendr, wait_running, wait_terminal};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tendr::model::ids::{Namespace, SessionName};
use tendr::session::{self, SessionRoot};

static SERIAL: Mutex<()> = Mutex::new(());

/// Start `name` with the unlock gate armed at `gate`.
fn start_gated(root: &TempDir, name: &str, gate: &Path, argv: &[&str]) {
    tendr(root)
        .env("TENDR_TEST_UNLOCK_GATE", gate)
        .args(["start", name, "--"])
        .args(argv)
        .assert()
        .success();
}

fn locked(root: &TempDir, name: &str) -> bool {
    let dir = session::open(
        &SessionRoot::new(root.path().join(".tendr/sessions")),
        &Namespace::new("default").unwrap(),
        &SessionName::new(name).unwrap(),
    )
    .unwrap()
    .expect("session exists");
    session::is_locked(&dir).unwrap()
}

/// A `tendr` invocation running in the background, with the same environment
/// as `harness::tendr` (temp HOME; on Windows, Git's coreutils on PATH).
fn spawn_tendr(root: &TempDir, args: &[&str]) -> Child {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin("tendr"));
    cmd.args(args)
        .env("HOME", root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        let git_usr_bin = Path::new(r"C:\Program Files\Git\usr\bin");
        if git_usr_bin.exists() {
            let path = std::env::var("PATH").unwrap_or_default();
            cmd.env("PATH", format!("{};{path}", git_usr_bin.display()));
        }
    }
    cmd.spawn().unwrap()
}

/// Wait for a background `tendr` to exit, bounded.
fn finish(mut child: Child) -> std::process::Output {
    harness::poll_child_exit(
        &mut child,
        Duration::from_secs(15),
        Duration::from_millis(20),
    )
    .unwrap_or_else(|e| panic!("tendr did not exit: {e}"));
    child.wait_with_output().unwrap()
}

/// The background command must still be waiting: the session is terminal but
/// its sidecar holds the lock.
fn assert_still_waiting(child: &mut Child, what: &str) {
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        child.try_wait().unwrap().is_none(),
        "{what} returned while the sidecar still held the session lock"
    );
}

#[test]
fn wait_returns_only_after_the_sidecar_releases_the_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let gate = root.path().join("gate");
    start_gated(&root, "rel-wait", &gate, &["true"]);
    let _session = harness::SessionGuard::new(&root, "rel-wait");

    wait_terminal(&root, "rel-wait");
    assert!(locked(&root, "rel-wait"), "setup: the gate holds the lock");

    let mut waiter = spawn_tendr(&root, &["wait", "rel-wait"]);
    assert_still_waiting(&mut waiter, "wait");

    std::fs::write(&gate, b"").unwrap();
    let out = finish(waiter);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("Exited"));
    assert!(!locked(&root, "rel-wait"), "released once wait returns");
}

#[test]
fn wait_warns_and_returns_when_the_sidecar_keeps_the_lock() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let gate = root.path().join("gate");
    start_gated(&root, "rel-grace", &gate, &["true"]);
    let _session = harness::SessionGuard::new(&root, "rel-grace");
    wait_terminal(&root, "rel-grace");

    let started = Instant::now();
    let out = tendr(&root)
        .args(["wait", "rel-grace", "--timeout", "10"])
        .assert_within_deadline();
    let out = out.get_output();
    assert!(
        out.status.success(),
        "the outcome decides the exit code: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("still holds the session lock"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        started.elapsed() >= Duration::from_millis(1900),
        "wait gives the sidecar its grace first ({:?})",
        started.elapsed()
    );

    std::fs::write(&gate, b"").unwrap();
}

#[test]
fn replace_waits_for_a_finished_sidecar_to_release() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let gate = root.path().join("gate");
    start_gated(&root, "rel-replace", &gate, &["true"]);
    let _session = harness::SessionGuard::new(&root, "rel-replace");
    wait_terminal(&root, "rel-replace");

    let mut replace = spawn_tendr(
        &root,
        &["start", "rel-replace", "--replace", "--", "sleep", "30"],
    );
    assert_still_waiting(&mut replace, "start --replace");

    std::fs::write(&gate, b"").unwrap();
    let out = finish(replace);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    wait_running(&root, "rel-replace");
}

#[test]
fn kill_returns_only_after_the_sidecar_releases_the_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let gate = root.path().join("gate");
    start_gated(&root, "rel-kill", &gate, &["sleep", "60"]);
    let _session = harness::SessionGuard::new(&root, "rel-kill");
    wait_running(&root, "rel-kill");

    let mut kill = spawn_tendr(&root, &["kill", "rel-kill"]);
    wait_terminal(&root, "rel-kill");
    assert_still_waiting(&mut kill, "kill");

    std::fs::write(&gate, b"").unwrap();
    let out = finish(kill);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!locked(&root, "rel-kill"));
}
