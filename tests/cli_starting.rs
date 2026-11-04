//! A sidecar holds its session before it records the run: it takes the
//! session lock first and writes `meta.json` only once the child is spawned.
//! A session in that window is starting, not orphaned. `kill` and
//! `start --replace` wait for the record and then act on the run; a second
//! `start` and `status` say it is starting. None of them deletes the
//! directory under its live sidecar.
//!
//! The window is held open by the breadcrumb fault, whose gate the sidecar
//! waits on after the spawn and before `meta.json`; the fault itself is only
//! a warning. No timing races: a command that waits says so on stderr before
//! the gate opens.

mod harness;
use harness::Home;

use harness::{HANG, Observation, OpenGate, poll_child_exit, poll_until, tendr, wait_running};
use std::ffi::OsStr;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::Mutex;
use std::time::Duration;

static SERIAL: Mutex<()> = Mutex::new(());

/// The session every test holds in the window.
const SESSION: &str = "early";

fn session_dir(root: &Home) -> PathBuf {
    root.path()
        .join(format!(".tendr/sessions/default/{SESSION}"))
}

/// `tendr <args>` as a background process, for a client the test acts
/// around while it runs.
fn spawn_tendr(root: &Home, args: &[&str], env: &[(&str, &OsStr)]) -> Child {
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"));
    cmd.args(args)
        .env("HOME", root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        cmd.env(key, value);
    }
    // Mirror harness::tendr: Git-for-Windows coreutils for `sleep`.
    #[cfg(windows)]
    {
        let git_usr_bin = std::path::Path::new(r"C:\Program Files\Git\usr\bin");
        if git_usr_bin.exists() {
            let path = std::env::var("PATH").unwrap_or_default();
            cmd.env("PATH", format!("{};{path}", git_usr_bin.display()));
        }
    }
    cmd.spawn().expect("spawn tendr")
}

/// A `start` whose sidecar is held after the spawn and before it records
/// the run, until `gate` exists. Opens the gate when dropped.
struct Held {
    start: Child,
    gate: OpenGate,
}

impl Held {
    fn new(root: &Home) -> Self {
        let gate = OpenGate(root.path().join("record-gate"));
        let start = spawn_tendr(
            root,
            &["start", SESSION, "sleep", "3600"],
            &[
                ("TENDR_TEST_FAIL", "breadcrumb".as_ref()),
                ("TENDR_TEST_FAULT_GATE", gate.0.as_os_str()),
                // The fault waits for the gate however long the test takes.
                ("TENDR_TEST_GATE_BOUND_MS", "3600000".as_ref()),
            ],
        );
        // The client writes launch_spec.json before it spawns the sidecar,
        // and the sidecar removes it once it holds the lock.
        let dir = session_dir(root);
        poll_until(HANG, Duration::from_millis(10), || {
            if dir.join("lock").exists() && !dir.join("launch_spec.json").exists() {
                Observation::Ready(())
            } else {
                Observation::Pending("the sidecar has not taken its spec".into())
            }
        })
        .unwrap_or_else(|e| panic!("timed out waiting for the sidecar to start: {e}"));
        assert_unrecorded(root);
        Self { start, gate }
    }

    /// Let the sidecar record the run, and wait for the `start` client.
    fn release(mut self) -> std::process::Output {
        std::fs::write(&self.gate.0, b"").expect("open the record gate");
        poll_child_exit(&mut self.start, HANG, Duration::from_millis(10))
            .unwrap_or_else(|e| panic!("start did not return: {e}"));
        self.start.wait_with_output().expect("collect start output")
    }
}

/// The session is still in the window: its directory is there and nothing
/// has recorded (or removed) the run.
fn assert_unrecorded(root: &Home) {
    let dir = session_dir(root);
    assert!(dir.join("lock").exists(), "the session directory is gone");
    assert!(
        !dir.join("meta.json").exists(),
        "the sidecar recorded the run with its gate shut"
    );
}

/// The first stderr line of a background client that should wait, within
/// [`HANG`]. The rest of its stderr is drained, so the client never blocks
/// on (or fails writing to) a full or closed pipe.
fn first_note(client: &mut Child) -> String {
    let stderr = client.stderr.take().expect("client stderr");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut note = String::new();
        let _ = reader.read_line(&mut note);
        let _ = tx.send(note);
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
    });
    rx.recv_timeout(HANG)
        .expect("the client wrote nothing to stderr")
}

/// Reap a background client, asserting it succeeded, and parse the meta it
/// printed.
fn finish_ok(mut client: Child, what: &str) -> serde_json::Value {
    poll_child_exit(&mut client, HANG, Duration::from_millis(10))
        .unwrap_or_else(|e| panic!("{what} did not return: {e}"));
    let out = client.wait_with_output().expect("collect output");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{what} failed: {stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{what} prints meta ({e}): {stdout}"))
}

/// `kill` waits for the record and stops the run, instead of failing on the
/// missing `meta.json` while the run starts after it.
#[test]
fn kill_waits_for_a_starting_run_and_stops_it() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    let held = Held::new(&root);

    let wait = HANG.as_millis().to_string();
    let mut kill = spawn_tendr(
        &root,
        &["kill", "--force", SESSION],
        &[("TENDR_TEST_KILL_WAIT_MS", wait.as_ref())],
    );
    let note = first_note(&mut kill);
    assert!(
        note.contains("tendr kill: early is starting; waiting for its sidecar"),
        "kill must wait for the run's record: {note}"
    );
    held.release();

    let meta = finish_ok(kill, "kill");
    assert_eq!(meta["status"], "Exited", "{meta}");
}

/// `start --replace` waits for the record, then replaces the run as it
/// would any other.
#[test]
fn replace_waits_for_a_starting_run_and_replaces_it() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    let held = Held::new(&root);

    let wait = HANG.as_millis().to_string();
    let mut replace = spawn_tendr(
        &root,
        &["start", "--replace", SESSION, "sleep", "3600"],
        &[
            ("TENDR_TEST_KILL_WAIT_MS", wait.as_ref()),
            ("TENDR_TEST_REPLACE_WAIT_MS", wait.as_ref()),
        ],
    );
    let note = first_note(&mut replace);
    assert!(
        note.contains("tendr start --replace: early is starting; waiting for its sidecar"),
        "start --replace must wait for the run's record: {note}"
    );
    held.release();

    let meta = finish_ok(replace, "start --replace");
    assert_eq!(meta["status"], "Running", "{meta}");
    assert_eq!(
        meta["generation"], 2,
        "the starting run was replaced: {meta}"
    );
}

/// A second `start` says the session is starting and leaves it to its
/// sidecar, which goes on to record the run.
#[test]
fn a_second_start_of_a_starting_run_is_refused() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    let held = Held::new(&root);

    let out = tendr(&root)
        .args(["start", SESSION, "sleep", "3600"])
        .timeout(HANG)
        .output()
        .expect("run the second start");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "the second start succeeded: {stderr}"
    );
    assert!(
        stderr.contains("session early is starting"),
        "the second start must say the session is starting: {stderr}"
    );
    assert_unrecorded(&root);

    let first = held.release();
    assert!(
        first.status.success(),
        "the first start failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    wait_running(&root, SESSION);
}

/// `status` says the session is starting, not orphaned, and leaves it to
/// its sidecar.
#[test]
fn status_of_a_starting_run_says_it_is_starting() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    let held = Held::new(&root);

    let out = tendr(&root)
        .args(["status", SESSION])
        .timeout(HANG)
        .output()
        .expect("run status");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "status succeeded: {stderr}");
    assert!(
        stderr.contains("session early is starting") && !stderr.contains("orphaned"),
        "status must say the session is starting: {stderr}"
    );
    assert_unrecorded(&root);

    let first = held.release();
    assert!(
        first.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    wait_running(&root, SESSION);
}
