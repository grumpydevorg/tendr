mod harness;
use harness::Home;

use harness::{tendr, wait_running, wait_terminal};
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

#[test]
fn timeout_kills_child() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    tendr(&root)
        .args(["start", "--timeout", "2", "timeout-job", "sleep", "3600"])
        .assert()
        .success();
    wait_running(&root, "timeout-job");

    // The child never ends by itself, so reaching terminal state proves that
    // the timeout fired; the reason says so too.
    let meta = wait_terminal(&root, "timeout-job");
    assert_eq!(meta["status"], "Exited");
    assert_eq!(meta["reason"], "TimedOut");
}

#[test]
fn timeout_not_triggered() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    // A timeout no load can reach before `true` exits.
    tendr(&root)
        .args(["start", "--timeout", "3600", "fast-job", "true"])
        .assert()
        .success();

    let meta = wait_terminal(&root, "fast-job");
    assert_eq!(meta["status"], "Exited");
    assert_eq!(meta["reason"], "ExitedOk");
}

/// Run `script` under `--timeout 1` and return its terminal meta.
#[cfg(unix)]
fn timed_out_run(root: &Home, session: &str, pty: bool, script: &str) -> serde_json::Value {
    let mut args = vec!["start", "--timeout", "1", session];
    if pty {
        args.push("--pty");
    }
    args.extend(["--", "sh", "-c", script]);
    tendr(root).args(&args).assert().success();
    wait_terminal(root, session)
}

/// [`timed_out_run`] for a script that sets its traps and then prints
/// `ready`, repeated until a run's timeout found the traps set: on a loaded
/// machine the 1 s timeout can fire before the shell gets that far, which
/// fails the premise, not the product. Returns that run's terminal meta.
#[cfg(unix)]
fn timed_out_trapped_run(root: &Home, session: &str, pty: bool, script: &str) -> serde_json::Value {
    let mut attempt = 0;
    harness::poll_until(harness::HANG, std::time::Duration::from_millis(10), || {
        attempt += 1;
        let name = format!("{session}-{attempt}");
        let meta = timed_out_run(root, &name, pty, script);
        if harness::logged_output(root, &name).contains("ready") {
            harness::Observation::Ready(meta)
        } else {
            harness::Observation::Pending(format!(
                "attempt {attempt}: timed out before the traps were set"
            ))
        }
    })
    .unwrap_or_else(|e| panic!("no run had its traps set when it timed out: {e}"))
}

/// A timed-out pipe run gets SIGTERM before SIGKILL, so its cleanup runs.
#[cfg(unix)]
#[test]
fn timeout_lets_a_pipe_run_clean_up_on_sigterm() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    let marker = root.path().join("cleaned");
    let script = format!(
        "trap 'echo term > {m}; exit 0' TERM; echo ready; while :; do sleep 0.1; done",
        m = marker.display()
    );
    let meta = timed_out_trapped_run(&root, "pipe-trap", false, &script);
    assert_eq!(meta["reason"], "TimedOut", "{meta}");
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "term");
}

/// A timed-out PTY run is hung up first, so a SIGHUP handler runs.
#[cfg(unix)]
#[test]
fn timeout_hangs_up_a_pty_run_first() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    let marker = root.path().join("cleaned");
    let script = format!(
        "trap 'echo hup > {m}; exit 0' HUP; trap 'echo term > {m}; exit 0' TERM; \
         echo ready; while :; do sleep 0.1; done",
        m = marker.display()
    );
    let meta = timed_out_trapped_run(&root, "pty-trap", true, &script);
    assert_eq!(meta["reason"], "TimedOut", "{meta}");
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "hup");
}

/// A child that ignores HUP and TERM, and never ends by itself, is still
/// killed by the escalation: the run reaching `TimedOut` at all is the proof.
///
/// This guards the escalation. It passes on the old code too, where the
/// timeout sent SIGKILL at once.
#[cfg(unix)]
#[test]
fn timeout_escalates_to_sigkill() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    let script = "trap '' HUP TERM; while :; do sleep 0.1; done";
    let meta = timed_out_run(&root, "pty-stubborn", true, script);
    assert_eq!(meta["reason"], "TimedOut", "{meta}");
}
