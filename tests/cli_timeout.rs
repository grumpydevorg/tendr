mod harness;

use harness::{tendr, wait_running, wait_terminal};
use std::sync::Mutex;
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

#[test]
fn timeout_kills_child() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "--timeout", "2", "timeout-job", "sleep", "60"])
        .assert()
        .success();
    wait_running(&root, "timeout-job");

    // `wait_terminal` has a 10-second deadline while the child sleeps for 60
    // seconds, so reaching terminal state proves that the timeout fired. Do
    // not include `tendr start` latency in a sub-5-second assertion: detached
    // process startup takes about three seconds on hosted Windows runners and
    // is outside the configured child-runtime timeout.
    let meta = wait_terminal(&root, "timeout-job");
    assert_eq!(meta["status"], "Exited");
    assert_eq!(meta["reason"], "TimedOut");
}

#[test]
fn timeout_not_triggered() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "--timeout", "60", "fast-job", "true"])
        .assert()
        .success();

    let meta = wait_terminal(&root, "fast-job");
    assert_eq!(meta["status"], "Exited");
    assert_eq!(meta["reason"], "ExitedOk");
}

/// Run `script` under `--timeout 1` and return its session guard, its
/// terminal meta and how long the run took from start to its terminal state.
/// The guard is created right after `start`, so a failed wait still stops
/// the sidecar.
#[cfg(unix)]
fn timed_out_run<'a>(
    root: &'a TempDir,
    session: &str,
    pty: bool,
    script: &str,
) -> (
    harness::SessionGuard<'a>,
    serde_json::Value,
    std::time::Duration,
) {
    let mut args = vec!["start", "--timeout", "1", session];
    if pty {
        args.push("--pty");
    }
    args.extend(["--", "sh", "-c", script]);
    let started = std::time::Instant::now();
    tendr(root).args(&args).assert().success();
    let guard = harness::SessionGuard::new(root, session);
    let meta = wait_terminal(root, session);
    (guard, meta, started.elapsed())
}

/// A timed-out pipe run gets SIGTERM before SIGKILL, so its cleanup runs.
#[cfg(unix)]
#[test]
fn timeout_lets_a_pipe_run_clean_up_on_sigterm() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let marker = root.path().join("cleaned");
    let script = format!(
        "trap 'echo term > {m}; exit 0' TERM; echo ready; while :; do sleep 0.1; done",
        m = marker.display()
    );
    let (_session, meta, _) = timed_out_run(&root, "pipe-trap", false, &script);
    assert_eq!(meta["reason"], "TimedOut", "{meta}");
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "term");
}

/// A timed-out PTY run is hung up first, so a SIGHUP handler runs.
#[cfg(unix)]
#[test]
fn timeout_hangs_up_a_pty_run_first() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let marker = root.path().join("cleaned");
    let script = format!(
        "trap 'echo hup > {m}; exit 0' HUP; trap 'echo term > {m}; exit 0' TERM; \
         while :; do sleep 0.1; done",
        m = marker.display()
    );
    let (_session, meta, _) = timed_out_run(&root, "pty-trap", true, &script);
    assert_eq!(meta["reason"], "TimedOut", "{meta}");
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "hup");
}

/// A child that ignores HUP and TERM is still killed by the escalation, and
/// the run ends TimedOut within the bound: 1 s timeout, up to 1 s hangup
/// grace and 5 s of SIGTERM grace, plus start-up slack.
///
/// This guards the escalation. It passes on the old code too, where the
/// timeout sent SIGKILL at once.
#[cfg(unix)]
#[test]
fn timeout_escalates_to_sigkill() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let script = "trap '' HUP TERM; while :; do sleep 0.1; done";
    let (_session, meta, elapsed) = timed_out_run(&root, "pty-stubborn", true, script);
    assert_eq!(meta["reason"], "TimedOut", "{meta}");
    assert!(elapsed < std::time::Duration::from_secs(10), "{elapsed:?}");
}
