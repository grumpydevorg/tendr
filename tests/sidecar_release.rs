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
use harness::Home;

use harness::{DeadlineAssertExt, HANG, tendr, wait_running, wait_terminal};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tendr::model::ids::{Namespace, SessionName};
use tendr::session::{self, SessionRoot};

static SERIAL: Mutex<()> = Mutex::new(());

/// A product timer lengthened past anything load can do, for a test where it
/// must not fire.
const FOREVER_MS: &str = "3600000";

/// Start `name` with the unlock gate armed at `gate`. The sidecar holds the
/// lock until the test opens the gate: the gate's own bound is put off.
fn start_gated(root: &Home, name: &str, gate: &Path, argv: &[&str]) {
    tendr(root)
        .env("TENDR_TEST_UNLOCK_GATE", gate)
        .env("TENDR_TEST_GATE_BOUND_MS", FOREVER_MS)
        .args(["start", name, "--"])
        .args(argv)
        .assert()
        .success();
}

fn locked(root: &Home, name: &str) -> bool {
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
/// as `harness::tendr` (temp HOME; on Windows, Git's coreutils on PATH). Its
/// release grace, `--replace` wait and kill wait are put off, so it waits for
/// the sidecar however long the test takes to open the gate.
fn spawn_tendr(root: &Home, args: &[&str]) -> KillOnDrop {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin("tendr"));
    cmd.args(args)
        .env("HOME", root.path())
        .env("TENDR_TEST_RELEASE_GRACE_MS", FOREVER_MS)
        .env("TENDR_TEST_REPLACE_WAIT_MS", FOREVER_MS)
        .env("TENDR_TEST_KILL_WAIT_MS", FOREVER_MS)
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
    KillOnDrop(Some(cmd.spawn().unwrap()))
}

/// A background `tendr` killed and reaped on drop, so a failing test does not
/// leave it waiting out its put-off bounds.
struct KillOnDrop(Option<Child>);

impl KillOnDrop {
    fn child(&mut self) -> &mut Child {
        self.0.as_mut().unwrap()
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Wait for a background `tendr` to exit, bounded by [`HANG`].
fn finish(mut bg: KillOnDrop) -> std::process::Output {
    let child = bg.child();
    harness::poll_child_exit(child, HANG, Duration::from_millis(20))
        .unwrap_or_else(|e| panic!("tendr did not exit: {e}"));
    bg.0.take().unwrap().wait_with_output().unwrap()
}

/// The background command must still be waiting: the session is terminal but
/// its sidecar holds the lock. The sleep waits out a command that would
/// return without waiting for the release; under load it can only pass
/// wrongly, never fail wrongly.
fn assert_still_waiting(bg: &mut KillOnDrop, what: &str) {
    let child = bg.child();
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        child.try_wait().unwrap().is_none(),
        "{what} returned while the sidecar still held the session lock"
    );
}

#[test]
fn wait_returns_only_after_the_sidecar_releases_the_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    let gate = root.path().join("gate");
    let _open = harness::OpenGate(gate.clone());
    start_gated(&root, "rel-wait", &gate, &["true"]);

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
    let root = Home::new();
    let gate = root.path().join("gate");
    let _open = harness::OpenGate(gate.clone());
    start_gated(&root, "rel-grace", &gate, &["true"]);
    wait_terminal(&root, "rel-grace");

    // The sidecar holds the lock until the gate opens, after the assertions,
    // so the grace (default, 2 s) is the only way out. `--timeout` only
    // bounds a hang: the session is already terminal.
    let hang = HANG.as_secs().to_string();
    let started = Instant::now();
    let out = tendr(&root)
        .args(["wait", "rel-grace", "--timeout", &hang])
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
    let root = Home::new();
    let gate = root.path().join("gate");
    let _open = harness::OpenGate(gate.clone());
    start_gated(&root, "rel-replace", &gate, &["true"]);
    wait_terminal(&root, "rel-replace");

    let mut replace = spawn_tendr(
        &root,
        &["start", "rel-replace", "--replace", "--", "sleep", "3600"],
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
    let root = Home::new();
    let gate = root.path().join("gate");
    let _open = harness::OpenGate(gate.clone());
    start_gated(&root, "rel-kill", &gate, &["sleep", "3600"]);
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
