mod harness;

use harness::{tendr, wait_running, wait_terminal};
use predicates::prelude::*;
use std::sync::Mutex;
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

#[test]
fn kill_running_process() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "kill-job", "sleep", "60"])
        .assert()
        .success();
    wait_running(&root, "kill-job");

    tendr(&root)
        .args(["kill", "kill-job"])
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""status": "Exited"#))
        .stdout(predicate::str::contains(r#""reason": "Killed"#));
}

#[test]
fn kill_force() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "force-job", "sleep", "60"])
        .assert()
        .success();
    wait_running(&root, "force-job");

    tendr(&root)
        .args(["kill", "--force", "force-job"])
        .assert()
        .success();

    let meta = wait_terminal(&root, "force-job");
    assert_eq!(meta["status"], "Exited");
}

#[test]
fn kill_already_dead_is_idempotent() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "dead-job", "true"])
        .assert()
        .success();
    wait_terminal(&root, "dead-job");

    tendr(&root)
        .args(["kill", "dead-job"])
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""status": "Exited"#))
        .stdout(predicate::str::contains(r#""reason": "ExitedOk"#));
}

#[test]
fn kill_nonexistent_session_is_idempotent() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["kill", "nope"])
        .assert()
        .success()
        .stdout(predicate::str::contains("not_found"));
}

#[test]
fn kill_preserves_child_identity() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let start_out = tendr(&root)
        .args(["start", "preserve-kill", "sleep", "60"])
        .output()
        .unwrap();
    let start_meta: serde_json::Value =
        serde_json::from_slice(&start_out.stdout).expect("not JSON");
    let start_pid = start_meta["child"]["pid"].as_u64().unwrap();
    wait_running(&root, "preserve-kill");

    let kill_out = tendr(&root)
        .args(["kill", "preserve-kill"])
        .output()
        .unwrap();
    let kill_meta: serde_json::Value = serde_json::from_slice(&kill_out.stdout).expect("not JSON");
    let kill_pid = kill_meta["child"]["pid"].as_u64().unwrap();

    assert_eq!(start_pid, kill_pid);
}

#[test]
fn status_shows_killed_after_kill() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "status-kill", "sleep", "60"])
        .assert()
        .success();
    wait_running(&root, "status-kill");

    tendr(&root)
        .args(["kill", "status-kill"])
        .assert()
        .success();

    tendr(&root)
        .args(["status", "status-kill"])
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""status": "Exited"#))
        .stdout(predicate::str::contains(r#""reason": "Killed"#));
}

/// Start `sleep 60` as `name` and return the pids of its sidecar and child.
#[cfg(unix)]
fn start_sleeper(root: &TempDir, name: &str) -> (libc::pid_t, libc::pid_t) {
    let out = tendr(root)
        .args(["start", name, "sleep", "60"])
        .output()
        .unwrap();
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout).expect("not JSON");
    wait_running(root, name);
    let pid = |v: &serde_json::Value| v.as_i64().expect("pid") as libc::pid_t;
    (pid(&meta["sidecar"]["pid"]), pid(&meta["child"]["pid"]))
}

/// Resumes a stopped sidecar when dropped, pass or panic, so the session can
/// still be stopped and cleaned up.
#[cfg(unix)]
struct ResumeOnDrop(libc::pid_t);

#[cfg(unix)]
impl Drop for ResumeOnDrop {
    fn drop(&mut self) {
        // SAFETY: the pid is the test's own stopped sidecar.
        unsafe { libc::kill(self.0, libc::SIGCONT) };
    }
}

/// `kill` does not report success for a run that did not end. A sidecar that
/// never records the end (here, stopped) leaves the run `Running`, so `kill`
/// fails, though it did signal the child itself.
#[cfg(unix)]
#[test]
fn kill_fails_when_the_run_does_not_end() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let (sidecar, _) = start_sleeper(&root, "stuck-kill");
    let _session = harness::SessionGuard::new(&root, "stuck-kill");
    // SAFETY: the pid is the test's own sidecar.
    unsafe { libc::kill(sidecar, libc::SIGSTOP) };
    let _resume = ResumeOnDrop(sidecar);

    let out = tendr(&root).args(["kill", "stuck-kill"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "kill succeeded for a run still recorded as Running: {stdout}{stderr}"
    );
    assert!(stderr.contains("has not ended"), "{stderr}");
}

/// A run whose sidecar is gone cannot record its own end, so `kill` records
/// it as `SidecarLost` after stopping the child, as `status` would.
#[cfg(unix)]
#[test]
fn kill_records_the_end_of_a_run_whose_sidecar_is_gone() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let (sidecar, child) = start_sleeper(&root, "lost-kill");
    let _session = harness::SessionGuard::new(&root, "lost-kill");
    // SAFETY: the pid is the test's own sidecar.
    unsafe { libc::kill(sidecar, libc::SIGKILL) };
    harness::wait_pid_dead(sidecar);

    let out = tendr(&root).args(["kill", "lost-kill"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let meta: serde_json::Value = serde_json::from_str(&stdout).expect("kill prints meta");
    assert_eq!(meta["status"], "SidecarLost", "{meta}");
    harness::wait_pid_dead(child);
}

/// The same for a run still `Starting` (waiting on `--after`) when its
/// sidecar died: `kill` records `SidecarLost` instead of leaving it live.
#[cfg(unix)]
#[test]
fn kill_records_the_end_of_a_starting_run_whose_sidecar_is_gone() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    start_sleeper(&root, "lost-dep");
    let _dep = harness::SessionGuard::new(&root, "lost-dep");
    tendr(&root)
        .args([
            "start",
            "lost-starting",
            "--after",
            "lost-dep",
            "sleep",
            "60",
        ])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "lost-starting");
    let meta_path = root
        .path()
        .join(".tendr/sessions/default/lost-starting/meta.json");
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    assert_eq!(meta["status"], "Starting", "{meta}");
    let sidecar = meta["sidecar"]["pid"].as_i64().expect("sidecar pid") as libc::pid_t;
    // SAFETY: the pid is the test's own sidecar.
    unsafe { libc::kill(sidecar, libc::SIGKILL) };
    harness::wait_pid_dead(sidecar);

    let out = tendr(&root)
        .args(["kill", "lost-starting"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let meta: serde_json::Value = serde_json::from_str(&stdout).expect("kill prints meta");
    assert_eq!(meta["status"], "SidecarLost", "{meta}");
}
