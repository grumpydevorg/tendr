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

#[cfg(windows)]
pub mod conpty;
#[cfg(unix)]
pub mod terminal;

/// The one hang detector: how long any test waits for something that is
/// expected to happen (a command to return, a state, an event, a file, a
/// process to end) before calling it hung.
///
/// It is not a performance bound and never decides a passing test: a
/// passing wait returns as soon as its condition holds, so the size costs
/// nothing then. It is sized for the slowest machine the suite runs on,
/// a Mac at a load average of 300 under other builds, where 10 s and 30 s
/// deadlines fired on correct behaviour (#200). A real hang takes this long
/// to report.
#[allow(dead_code)]
pub const HANG: Duration = Duration::from_secs(120);

/// Extension trait so call sites read `cmd.args(..).assert_within_deadline()` —
/// a drop-in for the fluent `.assert()` that runs the command under the shared
/// [`HANG`] hang detector with explicit timeout diagnostics.
#[allow(dead_code)]
pub trait DeadlineAssertExt {
    /// Assert the command under the shared [`HANG`] hang detector.
    fn assert_within_deadline(&mut self) -> Assert;
}

impl DeadlineAssertExt for Command {
    fn assert_within_deadline(&mut self) -> Assert {
        assert_within(self, HANG)
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
             firing: the command hung, or the machine starved it for that long.",
            deadline.as_secs_f64(),
            elapsed.as_secs_f64(),
        );
    }
    outcome
        .expect("failed to launch command (non-timeout spawn error)")
        .assert()
}

/// A test's HOME: a temp directory that, when dropped, force-kills every
/// session in it and waits for each sidecar to let go of its session before
/// deleting the directory.
///
/// Removing the directory under a live sidecar loses the test's sessions to
/// nothing that can reach them (#87), and a sidecar that finishes afterwards
/// recreates its session directory, or salvages its exit record into
/// `.tendr/lost+found`, under the deleted path, so every such test leaked a
/// directory into the system temp dir. Every helper that runs `tendr` takes a
/// `Home`, and a `Home` is declared before anything that uses it, so it drops
/// last, after the test's followers, gates and locks.
#[allow(dead_code)]
pub struct Home(TempDir);

#[allow(dead_code)]
impl Home {
    pub fn new() -> Self {
        Self(TempDir::new().expect("create a temp HOME"))
    }

    pub fn path(&self) -> &Path {
        self.0.path()
    }

    /// Wait until no sidecar holds `namespace/name`, force-killing it first
    /// if one does. Returns the last observation if it is still held after
    /// [`HANG`].
    fn release(&self, root: &SessionRoot, namespace: &str, name: &str) -> Result<(), WaitTimeout> {
        let (Ok(ns), Ok(session_name)) = (Namespace::new(namespace), SessionName::new(name)) else {
            return Ok(());
        };
        let Ok(Some(dir)) = session::open(root, &ns, &session_name) else {
            return Ok(());
        };
        let mut killed = false;
        poll_until(HANG, Duration::from_millis(10), || {
            match LockGuard::try_acquire(&dir) {
                Err(session::SessionError::Locked(_)) => {}
                // Free, or no longer a session a sidecar could hold.
                _ => return Observation::Ready(()),
            }
            if !killed {
                killed = true;
                // Ignored: the session may already be terminal, with its
                // sidecar finishing; the lock decides either way.
                let _ = tendr(self)
                    .args(["kill", "--force", "--namespace", namespace, name])
                    .timeout(HANG)
                    .output();
            }
            Observation::Pending(format!("{namespace}/{name} is still held by its sidecar"))
        })
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let sessions = self.path().join(".tendr/sessions");
        let root = SessionRoot::new(sessions.clone());
        let Ok(namespaces) = std::fs::read_dir(&sessions) else {
            return;
        };
        for namespace in namespaces.flatten() {
            let Ok(names) = std::fs::read_dir(namespace.path()) else {
                continue;
            };
            let namespace = namespace.file_name().to_string_lossy().into_owned();
            for name in names.flatten() {
                let name = name.file_name().to_string_lossy().into_owned();
                if let Err(e) = self.release(&root, &namespace, &name) {
                    let message =
                        format!("test HOME: sidecar of {namespace}/{name} outlived the test: {e}");
                    // A second panic while unwinding would abort the binary.
                    if std::thread::panicking() {
                        eprintln!("{message}");
                    } else {
                        panic!("{message}");
                    }
                }
            }
        }
    }
}

/// Create a `tendr` command rooted in a temp HOME.
#[allow(dead_code)]
pub fn tendr(root: &Home) -> Command {
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

/// Creates the gate file a test's child or sidecar waits on, when dropped,
/// so a party held at the gate never outlives a failing test. Create it
/// right after the gate's path, before anything that can panic.
#[allow(dead_code)]
pub struct OpenGate(pub PathBuf);

impl Drop for OpenGate {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, b"");
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

/// A PATH dir holding only a fake `ssh` that runs `actions` (one per line;
/// the list is in `src/bin/fake_ssh.rs`). Keep the TempDir alive while the
/// PATH is in use.
#[allow(dead_code)]
pub fn fake_ssh(actions: &str) -> TempDir {
    fake_program("ssh", actions)
}

/// A PATH dir holding only a fake `name` that runs `actions`, as `fake_ssh`
/// does for ssh. Keep the TempDir alive while the PATH is in use.
#[allow(dead_code)]
pub fn fake_program(name: &str, actions: &str) -> TempDir {
    let tmp = TempDir::new().unwrap();
    let program = tmp
        .path()
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    let bin = assert_cmd::cargo::cargo_bin("fake_ssh");
    // A hard link never holds the executable open for writing, so a fork in
    // a parallel test cannot make exec fail with ETXTBSY; copy across devices.
    if std::fs::hard_link(&bin, &program).is_err() {
        std::fs::copy(&bin, &program).unwrap();
    }
    std::fs::write(tmp.path().join("fake-ssh-actions"), actions).unwrap();
    tmp
}

/// The fake ssh's dir in front of the inherited PATH.
#[allow(dead_code)]
pub fn shim_path_env(tmp: &TempDir) -> std::ffi::OsString {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let dirs = std::iter::once(tmp.path().to_path_buf()).chain(std::env::split_paths(&inherited));
    std::env::join_paths(dirs).unwrap()
}

/// Read all stored events for a default-namespace session, merged by
/// (ts, writer, seq) — the spec §4 merge rule.
#[allow(dead_code)]
pub fn read_events(root: &Home, session: &str) -> Vec<serde_json::Value> {
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

/// The PTY output of a default-namespace session's current run, as its
/// recording holds it. Read it once the run has ended, so the recording is
/// complete.
#[allow(dead_code)]
pub fn recorded_output(root: &Home, session: &str) -> Vec<u8> {
    let session_dir = root
        .path()
        .join(format!(".tendr/sessions/default/{session}"));
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(session_dir.join("meta.json")).unwrap()).unwrap();
    let dir = session_dir.join(meta["pty"]["recording"]["dir"].as_str().unwrap());
    let files = tendr::recorder::DirectoryStore::segments(&dir).unwrap();
    let bytes: Vec<Vec<u8>> = files.iter().map(|f| std::fs::read(f).unwrap()).collect();
    tendr::recording::decode_recording(bytes.iter().map(Vec::as_slice))
        .expect("recording decodes")
        .records
        .iter()
        .filter_map(|r| match &r.kind {
            tendr::recording::RecordKind::Output(bytes) => Some(bytes.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

/// A burst for a child to write as it ends: 200 000 numbered lines (2.8 MB),
/// then the marker `BURST-END`, which ends the run's output. Well past every
/// buffer between a session and a stalled reader, and well inside the
/// session's output ring.
#[allow(dead_code)]
pub const BURST: &str =
    "awk 'BEGIN { for (i = 0; i < 200000; i++) printf \"burst-%07d\\n\", i }'; printf BURST-END";

/// `out`, what a viewer of a run that wrote [`BURST`] got before it was cut
/// off, is the recording's output up to some point of the burst: from the
/// burst's first line on it is the recording's, and the marker never came.
#[allow(dead_code)]
pub fn assert_prefix_of_the_burst(recording: &[u8], out: &[u8]) {
    let first = |bytes: &[u8]| {
        bytes
            .windows(13)
            .position(|w| w == b"burst-0000000")
            .expect("the burst's first line")
    };
    let (rec, got) = (&recording[first(recording)..], &out[first(out)..]);
    assert!(
        got.len() < rec.len() && rec.starts_with(got),
        "the output is not a prefix of the recording's burst ({} of {} bytes)",
        got.len(),
        rec.len()
    );
    assert!(!out.ends_with(b"BURST-END"), "the marker came after all");
}

/// The PTY output of a session as `tendr log --raw` prints it, with every line
/// break removed. Capture logs each read of the PTY as lines of its own, so text
/// that straddles two reads is split there: a shell that echoes a typed line one
/// keystroke at a time, as macOS's `/bin/sh` (bash 3.2) does, is often read
/// mid-line. Joining the lines recovers such text, at the cost of the output's
/// own line breaks.
#[allow(dead_code)]
pub fn logged_output(root: &Home, session: &str) -> String {
    let output = tendr(root)
        .args(["log", session, "--raw"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .chars()
        .filter(|&c| c != '\n')
        .collect()
}

/// Wait until the tail of a default-namespace session's `output.log` holds
/// `marker`: capture logs each read just before it adds it to the output
/// ring, so the output up to the marker has then all been captured. The
/// log's lines follow capture's reads, so they are joined. Reads only the
/// tail, so a large log costs nothing more.
#[allow(dead_code)]
pub fn wait_log_tail(root: &Home, session: &str, marker: &str) {
    let log = root
        .path()
        .join(format!(".tendr/sessions/default/{session}/output.log"));
    poll_until(HANG, Duration::from_millis(100), || {
        use std::io::{Read, Seek, SeekFrom};
        let mut tail = Vec::new();
        if let Ok(mut file) = std::fs::File::open(&log) {
            let _ = file.seek(SeekFrom::End(-4096));
            let _ = file.read_to_end(&mut tail);
        }
        let text: String = tail
            .split(|&b| b == b'\n')
            .filter_map(|l| serde_json::from_slice::<serde_json::Value>(l).ok())
            .filter_map(|l| l["content"].as_str().map(str::to_owned))
            .collect();
        if text.contains(marker) {
            Observation::Ready(())
        } else {
            Observation::Pending(format!("{} bytes of log tail", tail.len()))
        }
    })
    .unwrap_or_else(|e| panic!("{session} never logged {marker}: {e}"));
}

// Time. A test passes or fails by what it observes, never by how long that
// took, so that a loaded machine only makes it slower:
//
// - A wait for something expected to happen gives up only at [`HANG`].
// - A test that something does not wait (for a stalled child, a full pipe,
//   a timer) makes that thing wait for ever, or until the test releases it
//   after the assertion, so "it returned" is the proof, not "it returned
//   within N".
// - A test that a timer fires makes the timer the only way out: the party
//   it times out never makes progress by itself. A lower bound ("waited at
//   least the grace") is sound, since load only lengthens it.
// - A test that a timer does not fire needs the timer to outlast anything
//   load can do to the scenario; a product timer too short for that is
//   lengthened for the test (debug-only `TENDR_TEST_*` variable) or the
//   scenario is changed so the timer is not in its path.
// - A timing policy (quiet windows, bounds, backoff) is a pure function of
//   instants, tested with chosen instants, not with sleeping threads.
// - A wait that a thing does NOT happen (sleep, then assert absence) can
//   only pass wrongly under load, never fail wrongly; keep it short and say
//   what it waits out.
// - `thread::sleep` otherwise only paces a producer; it never orders two
//   things. Wait for the event that means the first one happened.
//
// `scripts/load-test.sh` checks a test against these on Linux: it runs the
// test binary frozen for most of each second, as a loaded machine freezes a
// starved process, beside CPU hogs sharing its quota.
//
// Polling. A wait on the filesystem or a process (meta.json, event logs, a
// path, PID liveness, `try_wait`, `try_acquire`, `tendr log` or `status`
// output) goes through `poll_until` or a wrapper below. The rules:
//
// - Probe at once: never sleep before the first probe.
// - One `Instant` deadline per wait, computed on entry.
// - A wait keeps its own interval. On a timeout it panics, returns `false`,
//   returns the error so cleanup runs first, or ignores it for best-effort
//   cleanup. `poll_*` wrappers return the `Result`; `wait_*` wrappers panic.
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
pub fn wait_event_kind(root: &Home, session: &str, kind: &str) -> serde_json::Value {
    poll_until(HANG, Duration::from_millis(10), || {
        match read_events(root, session)
            .into_iter()
            .find(|event| event["kind"] == kind)
        {
            Some(event) => Observation::Ready(event),
            None => Observation::Pending(format!("no event kind {kind} in {session} yet")),
        }
    })
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
pub fn wait_running(root: &Home, session: &str) {
    wait_running_ns(root, "default", session)
}

/// [`wait_running`] for a session in `namespace`.
#[allow(dead_code)]
pub fn wait_running_ns(root: &Home, namespace: &str, session: &str) {
    let path = root
        .path()
        .join(format!(".tendr/sessions/{namespace}/{session}/meta.json"));
    poll_until(HANG, Duration::from_millis(50), || {
        match meta_status(&path).as_deref() {
            Some("Running") => Observation::Ready(()),
            other => Observation::Pending(format!("status={}", other.unwrap_or("<unreadable>"))),
        }
    })
    .unwrap_or_else(|e| panic!("waiting for Running state in {namespace}/{session}: {e}"))
}

/// Wait for meta.json to reach any terminal state on disk.
#[allow(dead_code)]
pub fn wait_terminal(root: &Home, session: &str) -> serde_json::Value {
    wait_terminal_ns(root, "default", session)
}

/// [`wait_terminal`] for a session in `namespace`.
#[allow(dead_code)]
pub fn wait_terminal_ns(root: &Home, namespace: &str, session: &str) -> serde_json::Value {
    poll_terminal_ns(root, namespace, session)
        .unwrap_or_else(|e| panic!("waiting for terminal state in {namespace}/{session}: {e}"))
}

/// [`wait_terminal_ns`] that returns the timeout instead of panicking.
#[allow(dead_code)]
pub fn poll_terminal_ns(
    root: &Home,
    namespace: &str,
    session: &str,
) -> Result<serde_json::Value, WaitTimeout> {
    let path = root
        .path()
        .join(format!(".tendr/sessions/{namespace}/{session}/meta.json"));
    poll_until(HANG, Duration::from_millis(50), || {
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

/// Wait up to [`HANG`] for `pid` to die (not just be signalled).
#[cfg(unix)]
#[allow(dead_code)]
pub fn wait_pid_dead(pid: i32) {
    poll_pid_dead(pid, HANG, Duration::from_millis(50))
        .unwrap_or_else(|e| panic!("waiting for pid {pid} to die: {e}"))
}

/// Whether `pid` is a live process: it exists and is not a zombie. Read from
/// the kernel rather than `ps`: a `ps` without macOS's setuid bit (a Nix
/// build's) prints no state letter, so its output cannot tell a live process
/// from a gone one.
#[cfg(unix)]
#[allow(dead_code)]
pub fn process_running(pid: i32) -> bool {
    // SAFETY: signal 0 only probes for existence.
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        // The state follows the parenthesised command, which may hold spaces.
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next())
                .is_some_and(|state| state != "Z")
        })
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: proc_bsdinfo is plain data; proc_pidinfo writes at most
        // `size` bytes into it and returns how many it wrote.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let wrote = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size)
        };
        // proc_pidinfo fails on a zombie, which kill(pid, 0) still finds.
        wrote == size && info.pbi_status != libc::SZOMB
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        true
    }
}

/// Whether the process with `pid` is still running.
#[cfg(windows)]
#[allow(dead_code)]
pub fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: OpenProcess/GetExitCodeProcess/CloseHandle are safe to call
    // with these args; `h` is checked for null before further use; the
    // handle is closed exactly once on the (single) success path.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        ok != 0 && code == STILL_ACTIVE as u32
    }
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
pub fn wait_terminal_quiescent(root: &Home, session_name: &str) -> QuiescentTerminal {
    wait_terminal_quiescent_ns(root, "default", session_name)
}

/// [`wait_terminal_quiescent`] for a session in `namespace`.
#[allow(dead_code)]
pub fn wait_terminal_quiescent_ns(
    root: &Home,
    namespace: &str,
    session_name: &str,
) -> QuiescentTerminal {
    let session_root = SessionRoot::new(root.path().join(".tendr/sessions"));
    let namespace = Namespace::new(namespace).expect("test namespace is valid");
    let session_name = SessionName::new(session_name).expect("test session name is valid");

    poll_until(HANG, Duration::from_millis(10), || {
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
pub fn wait_orphaned_running(root: &Home, session_name: &str) -> OrphanedRunning {
    let session_root = SessionRoot::new(root.path().join(".tendr/sessions"));
    let namespace = Namespace::new("default").expect("default namespace is valid");
    let session_name = SessionName::new(session_name).expect("test session name is valid");

    poll_until(HANG, Duration::from_millis(10), || {
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

/// Poll until `path` exists, panicking at [`HANG`]. The follower
/// publishes its `--ready-file` only after its baseline is established, so this
/// is the deterministic "safe to mutate now" barrier that replaces a fixed
/// post-spawn sleep.
#[allow(dead_code)]
pub fn wait_ready_file(path: &Path) {
    poll_path_exists(path, HANG, Duration::from_millis(10))
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
    /// — if the follower exits before readiness or [`HANG`] elapses,
    /// surfacing the follower's captured stderr rather than a bare timeout.
    pub fn spawn(root: &Home, subcommand: &str, args: &[&str]) -> Self {
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
        let ready = poll_until(HANG, Duration::from_millis(10), || {
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
                Some(format!("never published its ready-file ({e})"))
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

    /// Block until a streamed record satisfies `pred`, returning it. Panics at
    /// [`HANG`]. Rescans every record seen so far each poll, so a record that
    /// arrived just before the call is never missed — this is the delivery
    /// barrier that replaces a fixed pre-kill sleep.
    pub fn read_until(&self, pred: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        let end = Instant::now() + HANG;
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
                "follower `{}` produced no matching record within {HANG:?}",
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
