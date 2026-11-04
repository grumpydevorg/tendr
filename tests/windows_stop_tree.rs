#![cfg(windows)]

//! A stop on Windows ends every process the run started: the child starts
//! suspended and is in the run's Job Object before it runs, and a stop
//! without the sidecar's job handle (`kill_orphan`) still finds the whole
//! tree, stops it gracefully first, and never stops a process that only
//! reuses the child's PID.
//!
//! The tree is `ctrl_break_responder --child`: a responder that starts a
//! second one at once and prints `CHILD <pid>`. Each exits 42 on
//! CTRL_BREAK; a termination exits 1.

mod harness;

use harness::{HANG, Home, Observation, poll_until, tendr, wait_log_tail, wait_orphaned_running};
use std::io::{BufRead, BufReader};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::time::Duration;
use tendr::platform::{Current, Platform};
use windows_sys::Win32::Foundation::{STILL_ACTIVE, WAIT_OBJECT_0};
use windows_sys::Win32::System::JobObjects::{IsProcessInJob, OpenJobObjectW};
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, GetExitCodeProcess, OpenProcess,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess,
    WaitForSingleObject,
};

const RESPONDER: &str = env!("CARGO_BIN_EXE_ctrl_break_responder");

/// A process held open by the test, so its PID is not reused and its exit
/// code can be read however it ends. Terminated on drop if still running.
struct Held(OwnedHandle);

impl Held {
    fn open(pid: u32) -> Self {
        // SAFETY: plain call; the result is checked.
        let raw = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
                0,
                pid,
            )
        };
        assert!(
            !raw.is_null(),
            "open pid {pid}: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: a non-null handle from OpenProcess is ours.
        Self(unsafe { OwnedHandle::from_raw_handle(raw) })
    }

    /// The exit code, or `None` while it runs.
    fn exit_code(&self) -> Option<u32> {
        let mut code = 0;
        // SAFETY: valid handle and out-pointer.
        let ok = unsafe { GetExitCodeProcess(self.0.as_raw_handle().cast(), &mut code) };
        assert!(
            ok != 0,
            "GetExitCodeProcess: {}",
            std::io::Error::last_os_error()
        );
        (code != STILL_ACTIVE as u32).then_some(code)
    }

    /// Wait for it to exit (up to [`HANG`]) and return its exit code.
    fn wait(&self) -> u32 {
        // SAFETY: valid handle with SYNCHRONIZE.
        let waited =
            unsafe { WaitForSingleObject(self.0.as_raw_handle().cast(), HANG.as_millis() as u32) };
        assert_eq!(
            waited, WAIT_OBJECT_0,
            "process still running after {HANG:?}"
        );
        self.exit_code().expect("exited")
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        // SAFETY: valid handle with PROCESS_TERMINATE; harmless if exited.
        unsafe { TerminateProcess(self.0.as_raw_handle().cast(), 1) };
    }
}

fn meta_path(home: &Home, session: &str) -> std::path::PathBuf {
    home.path()
        .join(format!(".tendr/sessions/default/{session}/meta.json"))
}

fn read_meta(home: &Home, session: &str) -> serde_json::Value {
    let text = std::fs::read_to_string(meta_path(home, session)).expect("meta.json");
    serde_json::from_str(&text).expect("meta.json is JSON")
}

/// The run's Job Object, opened for query by the name the sidecar gives it
/// (`Local\tendr-run-<child pid>-<child creation ns>`), which `kill` opens
/// without the sidecar.
fn open_run_job(home: &Home, session: &str) -> OwnedHandle {
    // JOB_OBJECT_QUERY, from winnt.h (not in tendr's windows-sys features).
    const JOB_OBJECT_QUERY: u32 = 0x0004;
    let meta = read_meta(home, session);
    let name = format!(
        r"Local\tendr-run-{}-{}",
        meta["child"]["pid"].as_u64().expect("child pid"),
        meta["child"]["start_time_ns"]
            .as_u64()
            .expect("child start_time_ns"),
    );
    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    // SAFETY: NUL-terminated name, valid for the call; the result is checked.
    let raw = unsafe { OpenJobObjectW(JOB_OBJECT_QUERY, 0, wide.as_ptr()) };
    assert!(
        !raw.is_null(),
        "open job {name}: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: a non-null handle from OpenJobObjectW is ours.
    unsafe { OwnedHandle::from_raw_handle(raw) }
}

/// Whether `process` (opened with at least QUERY_LIMITED_INFORMATION) is
/// in `job`.
fn in_job(process: &OwnedHandle, job: &OwnedHandle) -> bool {
    let mut result = 0;
    // SAFETY: valid process and job (QUERY) handles, and a valid
    // out-pointer.
    let ok = unsafe {
        IsProcessInJob(
            process.as_raw_handle().cast(),
            job.as_raw_handle().cast(),
            &mut result,
        )
    };
    assert!(
        ok != 0,
        "IsProcessInJob: {}",
        std::io::Error::last_os_error()
    );
    result != 0
}

/// Process `pid`, opened only to query it: unlike [`Held`], dropping it
/// leaves the process alone.
fn open_for_query(pid: u32) -> OwnedHandle {
    // SAFETY: plain call; the result is checked.
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    assert!(
        !raw.is_null(),
        "open pid {pid}: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: a non-null handle from OpenProcess is ours.
    unsafe { OwnedHandle::from_raw_handle(raw) }
}

/// The pid of the run's child, once it is running.
fn child_pid(home: &Home, session: &str) -> u32 {
    poll_until(HANG, Duration::from_millis(50), || {
        let meta = std::fs::read_to_string(meta_path(home, session))
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
        match meta.as_ref().and_then(|m| m["child"]["pid"].as_u64()) {
            Some(pid) => Observation::Ready(pid as u32),
            None => Observation::Pending(format!("meta {meta:?}")),
        }
    })
    .expect("child running")
}

/// The `CHILD <pid>` the responder tree logged (the log as read joins its
/// lines).
fn logged_grandchild(home: &Home, session: &str) -> u32 {
    wait_log_tail(home, session, "READY");
    let log = harness::logged_output(home, session);
    log.split("CHILD ")
        .nth(1)
        .map(|rest| {
            rest.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .and_then(|pid| pid.parse().ok())
        .unwrap_or_else(|| panic!("no CHILD line in {log:?}"))
}

/// Start `session` running the responder tree and return the child and
/// its child, held.
fn start_tree(home: &Home, session: &str) -> (Held, Held) {
    tendr(home)
        .args(["start", session, "--", RESPONDER, "--child"])
        .assert()
        .success();
    let child = Held::open(child_pid(home, session));
    let grandchild = Held::open(logged_grandchild(home, session));
    (child, grandchild)
}

/// A child that starts a process as soon as it runs has it in the run's
/// job: a forced stop, which is the job's termination, ends it too.
#[test]
fn a_forced_kill_stops_what_the_child_started_at_once() {
    let home = Home::new();
    let (child, grandchild) = start_tree(&home, "forks");
    tendr(&home)
        .args(["kill", "forks", "--force"])
        .assert()
        .success();
    assert_eq!(child.wait(), 1, "the child was terminated");
    assert_eq!(
        grandchild.wait(),
        1,
        "its child was terminated with the job"
    );
}

/// A session started inside a run stays in the run's job, unlike on Unix:
/// the job allows no breakaway, since Cygwin and MSYS programs leave any
/// job that allows it. Its sidecar and child are in the outer run's job, so
/// a stop of the run reaches them.
#[test]
fn a_session_started_inside_a_run_stays_in_its_job() {
    let home = Home::new();
    // The outer run starts inner, then keeps running.
    let script = home.path().join("outer.cmd");
    std::fs::write(
        &script,
        format!(
            "@\"{}\" start inner -- ping -n 3600 127.0.0.1\r\n@ping -n 3600 127.0.0.1 >nul\r\n",
            env!("CARGO_BIN_EXE_tendr")
        ),
    )
    .unwrap();
    tendr(&home)
        // Over SSH the nested start would be refused instead.
        .env_remove("SSH_CONNECTION")
        .args(["start", "outer", "--"])
        .arg(&script)
        .assert()
        .success();
    let outer = Held::open(child_pid(&home, "outer"));
    let inner = Held::open(child_pid(&home, "inner"));
    let inner_sidecar = open_for_query(
        read_meta(&home, "inner")["sidecar"]["pid"]
            .as_u64()
            .expect("inner sidecar pid") as u32,
    );

    let job = open_run_job(&home, "outer");
    assert_eq!(outer.exit_code(), None, "the outer run is still running");
    assert!(
        in_job(&outer.0, &job),
        "the outer child is in its run's job"
    );
    assert!(
        in_job(&inner_sidecar, &job),
        "inner's sidecar is in the outer run's job"
    );
    assert!(
        in_job(&inner.0, &job),
        "inner's child is in the outer run's job"
    );
    tendr(&home)
        .args(["kill", "--force", "inner"])
        .assert()
        .success();
    tendr(&home)
        .args(["kill", "--force", "outer"])
        .assert()
        .success();
}

/// Kill a session's sidecar and leave the session `Running` without one,
/// its meta naming `identity` as the child: the orphan a crashed sidecar
/// leaves. (A real crash closes the run's job, which ends the run, so the
/// tree here is the test's own, outside any tendr job.)
fn orphan_session(home: &Home, session: &str, identity: tendr::model::ids::ProcessIdentity) {
    tendr(home)
        .args(["start", session, "--", "ping", "-n", "3600", "127.0.0.1"])
        .assert()
        .success();
    let old = child_pid(home, session);
    let meta = read_meta(home, session);
    let sidecar = Held::open(meta["sidecar"]["pid"].as_u64().expect("sidecar pid") as u32);
    drop(sidecar); // terminates it
    let orphaned = wait_orphaned_running(home, session);
    let mut meta = read_meta(home, session);
    let child = &mut meta["child"];
    assert_eq!(child["pid"].as_u64(), Some(u64::from(old)));
    child["pid"] = identity.pid.get().into();
    child["start_time_ns"] = identity.start_time_ns.into();
    std::fs::write(meta_path(home, session), meta.to_string()).expect("write meta.json");
    drop(orphaned);
}

/// The responder tree, started by the test in a console of its own, as a
/// tendr child is: the child and its child.
fn own_tree() -> (std::process::Child, Held) {
    let mut root = std::process::Command::new(RESPONDER)
        .arg("--child")
        .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the responder tree");
    let mut lines = BufReader::new(root.stdout.take().expect("stdout")).lines();
    let child_line = lines.next().expect("a CHILD line").expect("read");
    let pid: u32 = child_line
        .strip_prefix("CHILD ")
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("not a CHILD line: {child_line:?}"));
    let grandchild = Held::open(pid);
    assert_eq!(lines.next().expect("READY").expect("read"), "READY");
    (root, grandchild)
}

/// Without a sidecar, `kill` stops the child's whole tree, and asks it to
/// stop first: both processes exit by their CTRL_BREAK handler.
#[test]
fn an_orphan_kill_stops_the_tree_gracefully() {
    let home = Home::new();
    let (mut root, grandchild) = own_tree();
    let identity = Current::process_identity(root.id()).expect("identity");
    orphan_session(&home, "orphan", identity);

    tendr(&home).args(["kill", "orphan"]).assert().success();
    let code = root.wait().expect("wait").code();
    assert_eq!(code, Some(42), "the child stopped on CTRL_BREAK");
    assert_eq!(grandchild.wait(), 42, "its child stopped on CTRL_BREAK");
}

/// A meta whose child identity has the right PID but another creation time
/// names a process that has reused the PID: `kill` refuses it, and it and
/// its child keep running.
#[test]
fn an_orphan_kill_never_stops_a_process_that_reused_the_pid() {
    let home = Home::new();
    let (mut root, grandchild) = own_tree();
    let mut identity = Current::process_identity(root.id()).expect("identity");
    identity.start_time_ns -= 1;
    orphan_session(&home, "reused", identity);

    tendr(&home)
        .args(["kill", "reused"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("recycled"));
    // `kill` has returned, and nothing else stops them.
    assert_eq!(
        root.try_wait().expect("try_wait"),
        None,
        "the process is untouched"
    );
    assert_eq!(grandchild.exit_code(), None, "its child is untouched");
    let _ = root.kill();
}

/// Suspend every thread of process `pid`: a sidecar that does not answer.
/// Termination still works on it.
fn suspend(pid: u32) {
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, SuspendThread, THREAD_SUSPEND_RESUME};
    // SAFETY: plain calls with valid out-buffers; handles are closed by
    // OwnedHandle.
    unsafe {
        let snap = OwnedHandle::from_raw_handle(CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0));
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut suspended = 0;
        let mut more = Thread32First(snap.as_raw_handle().cast(), &mut entry) != 0;
        while more {
            if entry.th32OwnerProcessID == pid {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if !thread.is_null() {
                    let thread = OwnedHandle::from_raw_handle(thread);
                    if SuspendThread(thread.as_raw_handle().cast()) != u32::MAX {
                        suspended += 1;
                    }
                }
            }
            more = Thread32Next(snap.as_raw_handle().cast(), &mut entry) != 0;
        }
        assert!(suspended > 0, "suspended no thread of {pid}");
    }
}

/// A sidecar that does not answer still holds the run's job. `kill` finds
/// the job by its name, asks the tree to stop, then ends it.
#[test]
fn a_kill_past_a_stuck_sidecar_stops_the_tree_gracefully() {
    let home = Home::new();
    let (child, grandchild) = start_tree(&home, "stuck");
    let sidecar_pid = read_meta(&home, "stuck")["sidecar"]["pid"]
        .as_u64()
        .expect("sidecar pid") as u32;
    // Terminated when the test ends, which ends the job and the run.
    let _sidecar = Held::open(sidecar_pid);
    suspend(sidecar_pid);
    // The name `kill` opens the job by, and the child in it.
    let job = open_run_job(&home, "stuck");
    assert!(in_job(&child.0, &job), "the child is in the named job");
    drop(job);

    // The sidecar cannot record the end, so `kill` reports it has not.
    tendr(&home)
        .env("TENDR_TEST_KILL_WAIT_MS", "100")
        .args(["kill", "stuck"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("has not ended"));
    assert_eq!(child.wait(), 42, "the child stopped on CTRL_BREAK");
    assert_eq!(grandchild.wait(), 42, "its child stopped on CTRL_BREAK");
}
