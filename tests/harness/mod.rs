#[allow(unused_imports)]
use assert_cmd::Command;
use assert_cmd::assert::{Assert, OutputAssertExt};
#[allow(unused_imports)]
use std::io::{BufRead, BufReader, Read};
#[allow(unused_imports)]
use std::path::{Path, PathBuf};
#[allow(unused_imports)]
use std::process::{Child, Stdio};
#[allow(unused_imports)]
use std::sync::atomic::{AtomicU64, Ordering};
#[allow(unused_imports)]
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
#[allow(unused_imports)]
use tempfile::TempDir;
use tendr::model::ids::{Namespace, SessionName};
use tendr::model::meta::Meta;
use tendr::model::state::RunStatus;
use tendr::session::{self, LockGuard, SessionRoot};

/// Hang-detector deadline for a single `tendr` CLI invocation in tests.
///
/// This is NOT a performance assertion. A normal detached start completes in
/// ~20 ms (warm) to ~570 ms (cold); this bound exists only to fail a genuine
/// hang (deadlock) rather than let it stall CI forever. It is deliberately
/// generous: the old hard-coded 5 s per-command deadline false-fired on a
/// loaded Windows CI runner — `assert_cmd` killed the still-starting `tendr`
/// process, which surfaced as exit 1 with empty output. 30 s keeps substantial
/// headroom over observed individual starts while still surfacing a true hang
/// promptly. Fixed and shared — raise this one value only if a real environment
/// proves it insufficient.
#[allow(dead_code)]
pub const CMD_DEADLINE: Duration = Duration::from_secs(30);

/// Extension trait so call sites read `cmd.args(..).assert_within_deadline()` —
/// a drop-in for the fluent `.assert()` that runs the command under the shared
/// [`CMD_DEADLINE`] hang detector with explicit timeout diagnostics.
#[allow(dead_code)]
pub trait DeadlineAssertExt {
    /// Assert the command under the shared [`CMD_DEADLINE`] hang detector.
    fn assert_within_deadline(&mut self) -> Assert;
}

impl DeadlineAssertExt for Command {
    fn assert_within_deadline(&mut self) -> Assert {
        assert_within(self, CMD_DEADLINE)
    }
}

/// Like [`assert_within_deadline`] but with an explicit deadline (used by the
/// deadline's own regression test). If the invocation returns within 1 ms of
/// `deadline` or later, this panics with an explicit message naming the
/// command and the deadline. `assert_cmd` normally enforces that bound by killing an overrun,
/// but the diagnostic intentionally states only the wall-clock fact we can
/// observe rather than inferring how the process ended.
#[allow(dead_code)]
pub fn assert_within(cmd: &mut Command, deadline: Duration) -> Assert {
    let desc = format!("{cmd:?}");
    let start = Instant::now();
    let outcome = cmd.timeout(deadline).output();
    let elapsed = start.elapsed();
    // assert_cmd enforces the deadline through wait-timeout, whose poll()
    // truncates the remaining time to whole milliseconds: a killed overrun
    // can return up to 1 ms before `deadline`. Round the same way.
    if elapsed + Duration::from_millis(1) > deadline {
        panic!(
            "HARNESS TIMEOUT: command reached the {:.1}s harness deadline; the invocation \
             returned after {:.1}s.\n  command: {desc}\n  This is the harness hang-detector \
             firing — on a loaded runner it may mean the process was starved, not a product failure. \
             Raise harness::CMD_DEADLINE only after ruling out a real hang.",
            deadline.as_secs_f64(),
            elapsed.as_secs_f64(),
        );
    }
    outcome
        .expect("failed to launch command (non-timeout spawn error)")
        .assert()
}

/// Create a `tendr` command rooted in a temp HOME.
#[allow(dead_code)]
pub fn tendr(root: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("tendr").expect("tendr binary not found");
    cmd.env("HOME", root.path());
    // On Windows, ensure Git-for-Windows coreutils (echo, sleep, true, cat)
    // are on PATH so tests can spawn Unix-style commands.
    #[cfg(windows)]
    {
        let git_usr_bin = std::path::Path::new(r"C:\Program Files\Git\usr\bin");
        if git_usr_bin.exists() {
            let path = std::env::var("PATH").unwrap_or_default();
            cmd.env("PATH", format!("{};{path}", git_usr_bin.display()));
        }
    }
    cmd
}

/// Force-kills a session when dropped, whether the test passed or panicked.
///
/// A test that stops its session only on its last line leaks a live sidecar
/// whenever an earlier assertion panics. The `TempDir` is then removed while
/// unwinding, and nothing can reach the orphan (#87). Create the guard right
/// after `start`, and after the `TempDir`: locals drop in reverse order, so the
/// sidecar is stopped before its state directory disappears.
#[allow(dead_code)]
pub struct SessionGuard<'a> {
    root: &'a TempDir,
    name: String,
}

#[allow(dead_code)]
impl<'a> SessionGuard<'a> {
    pub fn new(root: &'a TempDir, name: &str) -> Self {
        Self {
            root,
            name: name.to_owned(),
        }
    }
}

impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        // The result is ignored: the session may already be terminal, and a
        // panic here while unwinding would abort the whole test binary.
        let _ = tendr(self.root)
            .args(["kill", "--force", &self.name])
            .timeout(CMD_DEADLINE)
            .output();
    }
}

/// Path to the `test_callback` fixture binary (built by cargo as a sibling of the test binary).
#[allow(dead_code)]
pub fn test_callback_bin() -> String {
    let bin = assert_cmd::cargo::cargo_bin("test_callback");
    bin.to_str()
        .expect("test_callback path is valid UTF-8")
        .to_owned()
}

/// `test_callback_bin()` quoted for embedding in on-exit command strings
/// that will be parsed by `shell_words::split`.
#[allow(dead_code)]
fn test_callback_bin_quoted() -> String {
    shell_words::quote(&test_callback_bin()).into_owned()
}

/// Return an on-exit command string that creates `path` as an empty marker file.
/// Parsed by `shell_words::split` in the sidecar, then executed directly — no shell involved.
#[allow(dead_code)]
pub fn touch_cmd(path: &Path) -> String {
    let quoted = shell_words::quote(path.to_str().expect("path is valid UTF-8"));
    format!("{} touch {quoted}", test_callback_bin_quoted())
}

/// Return an on-exit command string that writes TENDR_SESSION, TENDR_NAMESPACE,
/// and TENDR_EXIT_REASON to the given file.
/// Parsed by `shell_words::split` in the sidecar, then executed directly — no shell involved.
#[allow(dead_code)]
pub fn echo_env_cmd(path: &Path) -> String {
    let quoted = shell_words::quote(path.to_str().expect("path is valid UTF-8"));
    format!("{} echo-env {quoted}", test_callback_bin_quoted())
}

/// Read all stored events for a default-namespace session, merged by
/// (ts, writer, seq) — the spec §4 merge rule.
#[allow(dead_code)]
pub fn read_events(root: &TempDir, session: &str) -> Vec<serde_json::Value> {
    let events_dir = root
        .path()
        .join(format!(".tendr/sessions/default/{session}/events"));
    let mut segments: Vec<_> = std::fs::read_dir(&events_dir)
        .expect("events dir exists")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    segments.sort();

    let mut events: Vec<serde_json::Value> = Vec::new();
    for seg in segments {
        for line in std::fs::read_to_string(&seg).unwrap().lines() {
            if !line.is_empty() {
                events.push(serde_json::from_str(line).expect("event line parses"));
            }
        }
    }
    events.sort_by_key(|e| {
        (
            e["ts"].as_str().unwrap().to_owned(),
            e["writer"].as_str().unwrap().to_owned(),
            e["seq"].as_u64().unwrap(),
        )
    });
    events
}

// Polling. A wait on the filesystem or a process (meta.json, event logs, a
// path, PID liveness, `try_wait`, `try_acquire`, `tendr log` or `status`
// output) goes through `poll_until` or a wrapper below. The rules:
//
// - Probe at once: never sleep before the first probe.
// - One `Instant` deadline per wait, computed on entry.
// - A wait keeps its own timeout and interval. On a timeout it panics,
//   returns `false`, returns the error so cleanup runs first, or ignores it
//   for best-effort cleanup. `poll_*` wrappers return the `Result`; `wait_*`
//   wrappers panic.
// - `Pending` describes the last state for the timeout message. Keep it short,
//   or move in output the probe already owns; never clone a large buffer.
// - Add a wrapper only when three or more call sites share a probe.
//
// Some loops stay hand-written. Waits on in-process producer threads
// (`ReadyFollower` records, PTY reader buffers, mpsc channels) do not use
// `poll_until`: their replacement, if one is ever needed, is a `Condvar` or
// channel, and no failure has called for it. Loops that are not "probe until
// true or deadline" (settling streaks, one deadline shared by several phases,
// fixed pacing) also stay as they are.

/// One probe result: ready with a value, or not-yet with a cheap description of
/// the current state (retained only for the timeout message).
#[allow(dead_code)]
pub enum Observation<T> {
    Ready(T),
    Pending(String),
}

/// The failure of a [`poll_until`] wait: how long it waited and the last state
/// it observed. One uniform diagnostic across every wait.
#[allow(dead_code)]
#[derive(Debug)]
pub struct WaitTimeout {
    pub elapsed: Duration,
    pub last: String,
}

impl std::fmt::Display for WaitTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "timed out after {:?}; last: {}", self.elapsed, self.last)
    }
}

/// Probe a condition immediately, then every `interval`, until it is `Ready` or
/// the deadline passes. The single shared polling primitive.
///
/// - Probes immediately — no initial sleep (an already-true condition returns at
///   once).
/// - `Instant`-based deadline computed once; the same `now` drives the deadline
///   check and the remaining-time calculation.
/// - The final wait is clamped to the remaining time, so a wait never overshoots
///   its deadline.
/// - Returns `Result` so callers can run cleanup before asserting; the
///   `WaitTimeout` records the last observed state and the elapsed time.
#[allow(dead_code)]
pub fn poll_until<T>(
    timeout: Duration,
    interval: Duration,
    mut probe: impl FnMut() -> Observation<T>,
) -> Result<T, WaitTimeout> {
    let start = Instant::now();
    let deadline = start + timeout;
    loop {
        match probe() {
            Observation::Ready(value) => return Ok(value),
            Observation::Pending(last) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(WaitTimeout {
                        elapsed: now - start,
                        last,
                    });
                }
                let remaining = deadline.saturating_duration_since(now);
                std::thread::sleep(interval.min(remaining));
            }
        }
    }
}

/// Wait for a session event of `kind`, returning the observed record.
#[allow(dead_code)]
pub fn wait_event_kind(root: &TempDir, session: &str, kind: &str) -> serde_json::Value {
    poll_until(
        Duration::from_secs(10),
        Duration::from_millis(10),
        || match read_events(root, session)
            .into_iter()
            .find(|event| event["kind"] == kind)
        {
            Some(event) => Observation::Ready(event),
            None => Observation::Pending(format!("no event kind {kind} in {session} yet")),
        },
    )
    .unwrap_or_else(|e| panic!("waiting for event kind {kind} in {session}: {e}"))
}

/// Read the `status` field of a session's meta.json, if it is readable.
#[allow(dead_code)]
fn meta_status(session_meta: &Path) -> Option<String> {
    std::fs::read_to_string(session_meta)
        .ok()
        .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
        .and_then(|m| m["status"].as_str().map(str::to_owned))
}

/// Wait for meta.json to show Running state on disk.
#[allow(dead_code)]
pub fn wait_running(root: &TempDir, session: &str) {
    wait_running_ns(root, "default", session)
}

/// [`wait_running`] for a session in `namespace`.
#[allow(dead_code)]
pub fn wait_running_ns(root: &TempDir, namespace: &str, session: &str) {
    let path = root
        .path()
        .join(format!(".tendr/sessions/{namespace}/{session}/meta.json"));
    poll_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        || match meta_status(&path).as_deref() {
            Some("Running") => Observation::Ready(()),
            other => Observation::Pending(format!("status={}", other.unwrap_or("<unreadable>"))),
        },
    )
    .unwrap_or_else(|e| panic!("waiting for Running state in {namespace}/{session}: {e}"))
}

/// Wait for meta.json to reach any terminal state on disk.
#[allow(dead_code)]
pub fn wait_terminal(root: &TempDir, session: &str) -> serde_json::Value {
    wait_terminal_ns(root, "default", session)
}

/// [`wait_terminal`] for a session in `namespace`.
#[allow(dead_code)]
pub fn wait_terminal_ns(root: &TempDir, namespace: &str, session: &str) -> serde_json::Value {
    poll_terminal_ns(root, namespace, session)
        .unwrap_or_else(|e| panic!("waiting for terminal state in {namespace}/{session}: {e}"))
}

/// [`wait_terminal_ns`] that returns the timeout instead of panicking.
#[allow(dead_code)]
pub fn poll_terminal_ns(
    root: &TempDir,
    namespace: &str,
    session: &str,
) -> Result<serde_json::Value, WaitTimeout> {
    let path = root
        .path()
        .join(format!(".tendr/sessions/{namespace}/{session}/meta.json"));
    poll_until(Duration::from_secs(10), Duration::from_millis(50), || {
        let meta = std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok());
        match meta.as_ref().and_then(|m| m["status"].as_str()) {
            Some(status) if status != "Starting" && status != "Running" => {
                Observation::Ready(meta.unwrap())
            }
            other => Observation::Pending(format!("status={}", other.unwrap_or("<unreadable>"))),
        }
    })
}

/// Wait until `path` exists, probing every `interval`.
#[allow(dead_code)]
pub fn poll_path_exists(
    path: &Path,
    timeout: Duration,
    interval: Duration,
) -> Result<(), WaitTimeout> {
    poll_until(timeout, interval, || {
        if path.exists() {
            Observation::Ready(())
        } else {
            Observation::Pending(format!("{} absent", path.display()))
        }
    })
}

/// Wait until `child` exits, probing `try_wait` every `interval`. On a timeout
/// the child is still running: the caller decides whether to kill it.
#[allow(dead_code)]
pub fn poll_child_exit(
    child: &mut Child,
    timeout: Duration,
    interval: Duration,
) -> Result<std::process::ExitStatus, WaitTimeout> {
    poll_until(timeout, interval, || {
        match child.try_wait().expect("try_wait") {
            Some(status) => Observation::Ready(status),
            None => Observation::Pending(format!("pid {} still running", child.id())),
        }
    })
}

/// Wait until `kill(pid, 0)` fails, probing every `interval`.
#[cfg(unix)]
#[allow(dead_code)]
pub fn poll_pid_dead(pid: i32, timeout: Duration, interval: Duration) -> Result<(), WaitTimeout> {
    poll_until(timeout, interval, || {
        // SAFETY: signal 0 only probes for existence.
        if unsafe { libc::kill(pid, 0) } != 0 {
            Observation::Ready(())
        } else {
            Observation::Pending(format!("pid {pid} alive"))
        }
    })
}

/// Wait up to 5 s for `pid` to die (not just be signalled).
#[cfg(unix)]
#[allow(dead_code)]
pub fn wait_pid_dead(pid: i32) {
    poll_pid_dead(pid, Duration::from_secs(5), Duration::from_millis(50))
        .unwrap_or_else(|e| panic!("waiting for pid {pid} to die: {e}"))
}

/// Proof that a session is terminal and its sidecar has released ownership.
///
/// Construction is private: callers can only obtain this guard through
/// [`wait_terminal_quiescent`], which observes terminal metadata and then
/// acquires the session lock. Keeping the lock guard alive prevents another
/// owner from invalidating that observation while the caller asserts against
/// the quiescent session.
#[allow(dead_code)]
pub struct QuiescentTerminal {
    _lock: LockGuard,
}

/// Wait until terminal metadata is durable and the sidecar lock is available.
///
/// Terminal metadata alone does not prove that the sidecar has exited: the
/// sidecar writes its final state immediately before releasing the lock. This
/// condition handshake acquires the lock instead of relying on a timing gap.
#[allow(dead_code)]
pub fn wait_terminal_quiescent(root: &TempDir, session_name: &str) -> QuiescentTerminal {
    wait_terminal_quiescent_ns(root, "default", session_name)
}

/// [`wait_terminal_quiescent`] for a session in `namespace`.
#[allow(dead_code)]
pub fn wait_terminal_quiescent_ns(
    root: &TempDir,
    namespace: &str,
    session_name: &str,
) -> QuiescentTerminal {
    let session_root = SessionRoot::new(root.path().join(".tendr/sessions"));
    let namespace = Namespace::new(namespace).expect("test namespace is valid");
    let session_name = SessionName::new(session_name).expect("test session name is valid");

    poll_until(Duration::from_secs(10), Duration::from_millis(10), || {
        if let Ok(Some(session_dir)) = session::open(&session_root, &namespace, &session_name) {
            if session::read_meta(&session_dir).is_ok_and(|meta| meta.status().is_terminal()) {
                match LockGuard::try_acquire(&session_dir) {
                    Ok(lock) => return Observation::Ready(QuiescentTerminal { _lock: lock }),
                    Err(session::SessionError::Locked(_)) => {}
                    Err(error) => panic!("failed to acquire terminal session lock: {error}"),
                }
            }
        }
        Observation::Pending(format!("{session_name} not yet terminal + quiescent"))
    })
    .unwrap_or_else(|e| {
        panic!("waiting for terminal session {session_name} to become quiescent: {e}")
    })
}

/// Proof that a session is the residue of an intentionally-crashed sidecar:
/// `Running + Unlocked`. The sidecar is gone (its lock was acquired) and the
/// metadata it left behind is still non-terminal `Running`.
///
/// Construction is private: callers can only obtain this through
/// [`wait_orphaned_running`], which acquires the session lock — proving the
/// aborted sidecar exited — and then verifies the status. Both fields are
/// retained to make the proof-carrying state explicit even though the caller
/// needs neither; holding the guard keeps a new sidecar from taking the session
/// while the caller inspects meta and events.
#[allow(dead_code)]
pub struct OrphanedRunning {
    _meta: Meta,
    _lock: LockGuard,
}

/// Wait until an intentionally-crashed sidecar's residue is observable —
/// acquiring the session lock is the proof of death, so no fixed sleep.
///
/// Only lock *contention* is retried (the sidecar has not exited yet). Every
/// other condition fails immediately, so a real defect surfaces as a clear
/// message rather than a timeout: a missing or unreadable session, any non-
/// contention lock error, or a terminal status once the lock is held —
/// terminal-plus-acquired-lock is a stable contradiction for this injected crash
/// point, not a transient state worth polling further.
#[allow(dead_code)]
pub fn wait_orphaned_running(root: &TempDir, session_name: &str) -> OrphanedRunning {
    let session_root = SessionRoot::new(root.path().join(".tendr/sessions"));
    let namespace = Namespace::new("default").expect("default namespace is valid");
    let session_name = SessionName::new(session_name).expect("test session name is valid");

    poll_until(Duration::from_secs(10), Duration::from_millis(10), || {
        // `start` has returned, so the session directory and meta must exist:
        // anything else is a defect, not a state to wait out.
        let session_dir = match session::open(&session_root, &namespace, &session_name) {
            Ok(Some(dir)) => dir,
            Ok(None) => panic!("crashed session {session_name} does not exist"),
            Err(error) => panic!("failed to open crashed session {session_name}: {error}"),
        };

        match LockGuard::try_acquire(&session_dir) {
            Ok(lock) => {
                // The lock is ours, so the aborted sidecar is gone. What it left
                // behind must still be Running: a terminal status here means the
                // crash did not abort before finalization.
                let meta = session::read_meta(&session_dir)
                    .expect("crashed session must have readable meta");
                assert!(
                    matches!(meta.status(), RunStatus::Running { .. }),
                    "invariant violation: sidecar lock released but status is {:?}, expected \
                     Running — the injected crash did not leave a Running + Unlocked orphan",
                    meta.status()
                );
                Observation::Ready(OrphanedRunning {
                    _meta: meta,
                    _lock: lock,
                })
            }
            // The only retryable condition: the sidecar has not exited yet.
            Err(session::SessionError::Locked(_)) => {
                Observation::Pending(format!("{session_name} still locked by the sidecar"))
            }
            Err(error) => panic!("failed to acquire crashed session lock: {error}"),
        }
    })
    .unwrap_or_else(|e| {
        panic!("waiting for the crashed sidecar to release the lock on {session_name}: {e}")
    })
}

/// Gate a test on the external `duckdb` CLI. Returns `true` if the test should
/// proceed. When duckdb is absent:
///   - panics if `TENDR_REQUIRE_DUCKDB_TESTS` is set (CI installs duckdb, so a
///     broken install must fail loudly rather than silently pass as green);
///   - otherwise prints a skip notice and returns `false` so the caller returns
///     early (friendly local runs without duckdb installed).
#[allow(dead_code)]
pub fn duckdb_or_skip() -> bool {
    let available = std::process::Command::new("duckdb")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !available {
        if std::env::var_os("TENDR_REQUIRE_DUCKDB_TESTS").is_some() {
            panic!(
                "duckdb not found on PATH but TENDR_REQUIRE_DUCKDB_TESTS is set \
                 -- CI must install the DuckDB CLI"
            );
        }
        eprintln!("skipped: duckdb not on PATH (set TENDR_REQUIRE_DUCKDB_TESTS to enforce)");
    }
    available
}

/// Distinguishes ready-file paths when a test spawns several followers.
#[allow(dead_code)]
static FOLLOWER_SEQ: AtomicU64 = AtomicU64::new(0);

/// Poll until `path` exists or `deadline` elapses (panicking). The follower
/// publishes its `--ready-file` only after its baseline is established, so this
/// is the deterministic "safe to mutate now" barrier that replaces a fixed
/// post-spawn sleep.
#[allow(dead_code)]
pub fn wait_ready_file(path: &Path, deadline: Duration) {
    poll_path_exists(path, deadline, Duration::from_millis(10))
        .unwrap_or_else(|e| panic!("waiting for ready-file {}: {e}", path.display()))
}

/// An opaque handle to a running follower (`tendr events --follow` or
/// `tendr watch`) whose initial baseline is *proven* established: [`spawn`]
/// returns only after the follower publishes its `--ready-file`. Holding one
/// means "safe to perform live mutations now."
///
/// [`read_until`](Self::read_until) waits for a specific streamed record with a
/// deadline; [`stop`](Self::stop) consumes the handle and reaps the child; and
/// `Drop` reaps the child even on panic, so a failing test never leaks a
/// follower process.
///
/// [`spawn`]: Self::spawn
#[allow(dead_code)]
pub struct ReadyFollower {
    child: Child,
    label: String,
    records: Arc<Mutex<Vec<serde_json::Value>>>,
    stdout_reader: Option<std::thread::JoinHandle<()>>,
    stderr_reader: Option<std::thread::JoinHandle<()>>,
}

#[allow(dead_code)]
impl ReadyFollower {
    /// Spawn `tendr <subcommand> <args...> --ready-file <temp>` and block until
    /// the follower signals readiness. Panics — after killing/reaping the child
    /// — if the follower exits before readiness or the 10 s barrier elapses,
    /// surfacing the follower's captured stderr rather than a bare timeout.
    pub fn spawn(root: &TempDir, subcommand: &str, args: &[&str]) -> Self {
        let bin = assert_cmd::cargo::cargo_bin("tendr");
        let seq = FOLLOWER_SEQ.fetch_add(1, Ordering::Relaxed);
        let ready_path = root.path().join(format!("follower-ready-{seq}"));
        let label = format!("tendr {subcommand} {}", args.join(" "));

        let mut child = std::process::Command::new(bin)
            .arg(subcommand)
            .args(args)
            .arg("--ready-file")
            .arg(&ready_path)
            .env("HOME", root.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn follower `{label}`: {e}"));

        // Drain stdout AND stderr immediately — before waiting for readiness.
        // Production flushes the initial replay/snapshot *before* creating the
        // ready-file, so a replay larger than the OS pipe buffer would deadlock
        // if we waited first: the follower blocks flushing stdout and never
        // reaches the ready-file. Records accumulated here (the replay) are
        // preserved for later read_until calls.
        let records = Arc::new(Mutex::new(Vec::new()));
        let mut stdout_reader = {
            let records = Arc::clone(&records);
            let stdout = child.stdout.take().expect("piped stdout");
            Some(std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                        records.lock().unwrap().push(v);
                    }
                }
            }))
        };
        // stderr is drained so a premature exit is reported with its diagnostics
        // and a chatty stderr can't wedge on a full pipe.
        let stderr_buf = Arc::new(Mutex::new(String::new()));
        let mut stderr_reader = {
            let buf = Arc::clone(&stderr_buf);
            let mut err = child.stderr.take().expect("piped stderr");
            Some(std::thread::spawn(move || {
                let mut s = String::new();
                let _ = err.read_to_string(&mut s);
                *buf.lock().unwrap() = s;
            }))
        };

        // `Ready(Some(status))` is an exit before readiness.
        let ready = poll_until(Duration::from_secs(10), Duration::from_millis(10), || {
            if ready_path.exists() {
                return Observation::Ready(None);
            }
            match child.try_wait() {
                Ok(Some(status)) => Observation::Ready(Some(status)),
                _ => Observation::Pending(format!("ready-file {} absent", ready_path.display())),
            }
        });
        let failure = match ready {
            Ok(None) => None,
            Ok(Some(status)) => Some(format!("exited before readiness ({status})")),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                Some(format!("never published its ready-file within 10s ({e})"))
            }
        };
        // On failure, join both readers so the captured stderr is complete (the
        // pipes EOF once the child is gone), instead of sampling it after a
        // fixed sleep.
        if let Some(failure) = failure {
            if let Some(h) = stdout_reader.take() {
                let _ = h.join();
            }
            if let Some(h) = stderr_reader.take() {
                let _ = h.join();
            }
            let stderr = stderr_buf.lock().unwrap().clone();
            panic!("follower `{label}` {failure}; stderr:\n{stderr}");
        }

        ReadyFollower {
            child,
            label,
            records,
            stdout_reader,
            stderr_reader,
        }
    }

    /// Block until a streamed record satisfies `pred`, returning it. Panics on
    /// `deadline`. Rescans every record seen so far each poll, so a record that
    /// arrived just before the call is never missed — this is the delivery
    /// barrier that replaces a fixed pre-kill sleep.
    pub fn read_until(
        &self,
        deadline: Duration,
        pred: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let end = Instant::now() + deadline;
        loop {
            if let Some(found) = self
                .records
                .lock()
                .unwrap()
                .iter()
                .find(|r| pred(r))
                .cloned()
            {
                return found;
            }
            assert!(
                Instant::now() < end,
                "follower `{}` produced no matching record within {deadline:?}",
                self.label
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Snapshot of every record streamed so far (for negative assertions such as
    /// "history was skipped"). Take it only after a `read_until` has proven the
    /// stream reached the point you care about.
    pub fn records(&self) -> Vec<serde_json::Value> {
        self.records.lock().unwrap().clone()
    }

    /// Kill and reap the follower, consuming the handle.
    pub fn stop(mut self) {
        self.terminate();
    }

    fn terminate(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(handle) = self.stdout_reader.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_reader.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ReadyFollower {
    fn drop(&mut self) {
        self.terminate();
    }
}
