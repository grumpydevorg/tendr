//! A stop ends every process the run started, wherever it went: out of the
//! child's process group, into a session of its own, or orphaned. Each kind
//! is one that survived a stop in the cloud PTY plan's step 9. On macOS,
//! where nothing adopts orphans, each kind here is one a stop can still
//! find: in the child's session, below a live parent, or holding the
//! terminal.
#![cfg(any(target_os = "linux", target_os = "macos"))]

mod harness;

use harness::{Observation, SessionGuard, poll_until, tendr};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

/// Writes its own pid to the file `$1` (atomically, so a reader never sees
/// it empty), then becomes `sleep 300`, keeping the pid.
const HOLD: &str = r#"echo $$ > "$1.tmp" && mv "$1.tmp" "$1" && exec sleep 300"#;

/// Runs its arguments in a session of their own, as util-linux `setsid`
/// does (macOS has no `setsid` command): a process group leader, which may
/// not call `setsid`, forks first and exits, leaving the new session
/// orphaned.
const SETSID: &str = r#"exec perl -MPOSIX -e 'if (getpgrp() == $$) { fork and exit } POSIX::setsid() or die "setsid: $!"; exec @ARGV or die' "$@""#;

/// A session leader whose parent stays alive, with a child of its own, as
/// Claude Code starts each Bash tool shell: `hold.sh` as its child, then its
/// own pid to `$1`.
const LEADER: &str =
    r#"sh "$(dirname "$0")/hold.sh" "$1-child" & echo $$ > "$1.tmp" && mv "$1.tmp" "$1"; wait"#;

/// Every way out of the child's process group that step 9 measured, each
/// named by the pid file its process writes. `session` is started before job
/// control is on, so it is not a group leader and gets its own session
/// without forking; the rest are jobs of a shell with job control, each in a
/// process group of its own, as in an interactive shell. `setsid` is
/// orphaned in a session of its own, and holds the terminal.
const PTY_RUN: &str = r#"
d=$1
sh "$d/setsid.sh" sh "$d/leader.sh" "$d/session" &
set -m
sh "$d/hold.sh" "$d/plain" &
sh "$d/setsid.sh" sh "$d/hold.sh" "$d/setsid" &
nohup sh "$d/hold.sh" "$d/nohup" >/dev/null 2>&1 &
(sh "$d/hold.sh" "$d/subshell" &)
sh "$d/hold.sh" "$d/disown" & disown
for k in plain setsid nohup subshell disown session session-child; do
  while [ ! -e "$d/$k" ]; do sleep 0.05; done
done
touch "$d/ready"
exec sleep 600
"#;

/// The pid files [`PTY_RUN`] writes.
const KINDS: [&str; 7] = [
    "plain",
    "setsid",
    "nohup",
    "subshell",
    "disown",
    "session",
    "session-child",
];

#[derive(Clone, Copy)]
enum Stop {
    Kill,
    Force,
    Timeout,
}

/// A directory of helper scripts and the pid files they write. Dropping it
/// kills every process still named there, so nothing outlives the test.
struct Kinds(PathBuf);

impl Kinds {
    fn new(root: &TempDir) -> Self {
        let dir = root.path().join("kinds");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("hold.sh"), HOLD).unwrap();
        std::fs::write(dir.join("leader.sh"), LEADER).unwrap();
        std::fs::write(dir.join("setsid.sh"), SETSID).unwrap();
        Self(dir)
    }

    fn pid(&self, kind: &str) -> Option<i32> {
        std::fs::read_to_string(self.0.join(kind))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// The kinds whose process is still running, with their pids.
    fn running(&self, kinds: &[&str]) -> Vec<String> {
        kinds
            .iter()
            .filter_map(|kind| {
                let pid = self.pid(kind)?;
                running(pid).then(|| format!("{kind} (pid {pid})"))
            })
            .collect()
    }

    fn await_file(&self, name: &str) {
        let path = self.0.join(name);
        harness::poll_path_exists(&path, Duration::from_secs(10), Duration::from_millis(20))
            .unwrap_or_else(|e| panic!("{name} never appeared: {e}"));
    }
}

impl Drop for Kinds {
    /// Kills the process named by every pid file in the directory.
    fn drop(&mut self) {
        let Ok(files) = std::fs::read_dir(&self.0) else {
            return;
        };
        for file in files.filter_map(Result::ok) {
            let pid = file.file_name().to_str().and_then(|name| self.pid(name));
            if let Some(pid) = pid.filter(|&pid| running(pid)) {
                // SAFETY: the pid is one of the test's own processes.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
    }
}

/// Whether `pid` is a live process: present and not a zombie, per `ps`.
fn running(pid: i32) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|out| {
            let stat = String::from_utf8_lossy(&out.stdout);
            let stat = stat.trim();
            !stat.is_empty() && !stat.starts_with('Z')
        })
}

/// A session's meta as it is on disk, if readable.
fn read_meta(root: &TempDir, namespace: &str, session: &str) -> Option<serde_json::Value> {
    let path = root
        .path()
        .join(format!(".tendr/sessions/{namespace}/{session}/meta.json"));
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Stop a run with `stop` and return its terminal meta. `kill` returns once
/// the run's end is recorded; a timed-out run is waited for.
fn stop_run(root: &TempDir, session: &str, stop: Stop) -> serde_json::Value {
    let args: &[&str] =
        match stop {
            Stop::Kill => &["kill", session],
            Stop::Force => &["kill", "--force", session],
            Stop::Timeout => {
                return poll_until(Duration::from_secs(20), Duration::from_millis(50), || {
                    match read_meta(root, "default", session) {
                        Some(meta)
                            if !matches!(meta["status"].as_str(), Some("Starting" | "Running")) =>
                        {
                            Observation::Ready(meta)
                        }
                        other => Observation::Pending(format!("{other:?}")),
                    }
                })
                .unwrap_or_else(|e| panic!("{session} did not time out: {e}"));
            }
        };
    let out = tendr(root).args(args).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(&stdout).expect("kill prints meta")
}

/// Start [`PTY_RUN`], wait until every kind has started, stop the run with
/// `stop`, and check that no kind's process outlived the run's end.
fn every_kind_ends_with_the_run(stop: Stop, session: &str, reason: &str) {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let kinds = Kinds::new(&root);

    let mut start = tendr(&root);
    start.args(["start", session, "--pty"]);
    if let Stop::Timeout = stop {
        start.args(["--timeout", "8"]);
    }
    start
        .args(["--", "bash", "-c", PTY_RUN, "bash"])
        .arg(&kinds.0)
        .assert()
        .success();
    let _session = SessionGuard::new(&root, session);
    kinds.await_file("ready");

    let meta = stop_run(&root, session, stop);
    assert_eq!(meta["status"], "Exited", "{meta}");
    assert_eq!(meta["reason"], reason, "{meta}");
    let survivors = kinds.running(&KINDS);
    assert!(
        survivors.is_empty(),
        "still running once the run's end was recorded: {survivors:?}: {meta}"
    );
    // Everything was ended, so the stop has nothing to warn about.
    let warned = meta["warnings"].as_array().is_some_and(|warnings| {
        warnings.iter().any(|w| {
            w.as_str()
                .is_some_and(|w| w.starts_with("stopping the run"))
        })
    });
    assert!(!warned, "a stop that ended everything warned: {meta}");
}

#[test]
fn kill_ends_every_process_the_run_started() {
    every_kind_ends_with_the_run(Stop::Kill, "tree-kill", "Killed");
}

#[test]
fn kill_force_ends_every_process_the_run_started() {
    every_kind_ends_with_the_run(Stop::Force, "tree-force", "KilledForced");
}

#[test]
fn timeout_ends_every_process_the_run_started() {
    every_kind_ends_with_the_run(Stop::Timeout, "tree-timeout", "TimedOut");
}

/// A process in a session of its own that holds a pipe run's output kept the
/// run going after a stop until it exited, so `kill` failed. The stop now
/// ends it, and with it the run.
#[test]
fn kill_ends_a_pipe_run_whose_output_a_setsid_process_holds() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let kinds = Kinds::new(&root);

    let script = r#"sh "$1/setsid.sh" sh "$1/hold.sh" "$1/pipe-holder" &
while [ ! -e "$1/pipe-holder" ]; do sleep 0.05; done
exec sleep 600"#;
    tendr(&root)
        .args(["start", "pipe-held", "--", "sh", "-c", script, "sh"])
        .arg(&kinds.0)
        .assert()
        .success();
    let _session = SessionGuard::new(&root, "pipe-held");
    kinds.await_file("pipe-holder");

    let meta = stop_run(&root, "pipe-held", Stop::Kill);
    assert_eq!(meta["reason"], "Killed", "{meta}");
    let survivors = kinds.running(&["pipe-holder"]);
    assert!(survivors.is_empty(), "{survivors:?}: {meta}");
}

/// Traps SIGTERM with a cleanup that takes 0.3 s and then writes `$2`;
/// writes its pid to `$1` once the trap is set.
const CLEANS_UP: &str = r#"trap 'sleep 0.3; echo cleaned > "$2"; exit 0' TERM
echo $$ > "$1.tmp" && mv "$1.tmp" "$1"
while :; do sleep 0.1; done"#;

/// A graceful stop gives every process the run started SIGTERM and time to
/// act on it before any SIGKILL: a member of the child's process group,
/// which outlives its leader, and a process in a session of its own, which
/// the group's SIGTERM does not reach.
#[test]
fn kill_lets_every_process_clean_up_on_sigterm_first() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let kinds = Kinds::new(&root);
    std::fs::write(kinds.0.join("cleans-up.sh"), CLEANS_UP).unwrap();

    // The leader `sh` dies on SIGTERM at once, while the member is still
    // cleaning up.
    let script = r#"sh "$1/cleans-up.sh" "$1/member" "$1/member.cleaned" &
sh "$1/setsid.sh" sh "$1/cleans-up.sh" "$1/escapee" "$1/escapee.cleaned" &
while [ ! -e "$1/member" ] || [ ! -e "$1/escapee" ]; do sleep 0.05; done
wait"#;
    tendr(&root)
        .args(["start", "cleans-up", "--", "sh", "-c", script, "sh"])
        .arg(&kinds.0)
        .assert()
        .success();
    let _session = SessionGuard::new(&root, "cleans-up");
    kinds.await_file("member");
    kinds.await_file("escapee");

    let meta = stop_run(&root, "cleans-up", Stop::Kill);
    assert_eq!(meta["reason"], "Killed", "{meta}");
    for kind in ["member", "escapee"] {
        assert!(
            kinds.0.join(format!("{kind}.cleaned")).exists(),
            "the {kind} was not let clean up: {meta}"
        );
    }
    let survivors = kinds.running(&["member", "escapee"]);
    assert!(survivors.is_empty(), "{survivors:?}: {meta}");
}

/// Force-kills the session `nest/inner` when dropped.
struct InnerSession<'a>(&'a TempDir);

impl Drop for InnerSession<'_> {
    fn drop(&mut self) {
        let _ = tendr(self.0)
            .args(["kill", "--force", "--namespace", "nest", "inner"])
            .timeout(harness::CMD_DEADLINE)
            .output();
    }
}

/// A tendr session started from inside a run is a session of its own. Its
/// sidecar is orphaned once `tendr start` returns, so the outer sidecar
/// adopts it, but stopping the outer run must leave it and its child
/// running, as it would have before adoption, when init took it.
#[test]
fn a_stop_leaves_a_session_started_inside_the_run_running() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let started = root.path().join("inner-started");

    let script = r#""$1" start --namespace nest inner -- sleep 300 >/dev/null && touch "$2"
exec sleep 600"#;
    tendr(&root)
        .args(["start", "outer", "--pty", "--", "sh", "-c", script, "sh"])
        .arg(assert_cmd::cargo::cargo_bin("tendr"))
        .arg(&started)
        .assert()
        .success();
    let _outer = SessionGuard::new(&root, "outer");
    let _inner = InnerSession(&root);
    harness::poll_path_exists(&started, Duration::from_secs(10), Duration::from_millis(20))
        .unwrap_or_else(|e| panic!("the inner session never started: {e}"));
    harness::wait_running_ns(&root, "nest", "inner");
    let before = read_meta(&root, "nest", "inner").expect("inner meta");
    let inner_child = before["child"]["pid"].as_i64().expect("inner child pid") as i32;

    let meta = stop_run(&root, "outer", Stop::Kill);
    assert_eq!(meta["reason"], "Killed", "{meta}");
    // Long enough for a dying sidecar to be noticed.
    std::thread::sleep(Duration::from_millis(500));
    let out = tendr(&root)
        .args(["status", "--namespace", "nest", "inner"])
        .output()
        .unwrap();
    let after: serde_json::Value = serde_json::from_slice(&out.stdout).expect("status prints meta");
    assert_eq!(after["status"], "Running", "{after}");
    assert_eq!(after["child"]["pid"], before["child"]["pid"], "{after}");
    assert!(
        running(inner_child),
        "inner child {inner_child} died: {after}"
    );
}

/// The sidecar adopts what the run orphans, so it must also reap it while
/// the run lasts: an orphan that exits on its own must not stay a zombie.
#[cfg(target_os = "linux")]
#[test]
fn orphans_that_exit_during_the_run_are_reaped() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let dir = root.path();

    // Each `( … &)` leaves an orphan that exits 0.1 s later, its pid in
    // `orphan.N`; `orphaned` appears once all three pids are written.
    let script = r#"for i in 1 2 3; do (sleep 0.1 & echo $! > "$1/orphan.$i"); done
touch "$1/orphaned"; exec sleep 600"#;
    tendr(&root)
        .args(["start", "orphans", "--", "sh", "-c", script, "sh"])
        .arg(dir)
        .assert()
        .success();
    let _session = SessionGuard::new(&root, "orphans");
    harness::poll_path_exists(
        &dir.join("orphaned"),
        Duration::from_secs(10),
        Duration::from_millis(20),
    )
    .unwrap();
    let orphans: Vec<i32> = (1..=3)
        .map(|i| {
            let pid = std::fs::read_to_string(dir.join(format!("orphan.{i}"))).unwrap();
            pid.trim().parse().unwrap()
        })
        .collect();

    // Reaped means gone from /proc; an unreaped zombie stays listed.
    poll_until(Duration::from_secs(5), Duration::from_millis(100), || {
        let left: Vec<String> = orphans
            .iter()
            .filter(|pid| std::path::Path::new(&format!("/proc/{pid}")).exists())
            .map(|pid| {
                format!(
                    "{pid} ({})",
                    if running(*pid) { "running" } else { "zombie" }
                )
            })
            .collect();
        if left.is_empty() {
            Observation::Ready(())
        } else {
            Observation::Pending(format!("orphans not reaped: {left:?}"))
        }
    })
    .unwrap_or_else(|e| panic!("{e}"));
}
