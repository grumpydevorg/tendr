#![cfg(windows)]

//! Verify the sidecar survives termination of its parent's Job Object.
//!
//! This is the SSH-disconnect survival case: OpenSSH-Server on Windows wraps
//! each session's spawned processes in a Job Object with KILL_ON_JOB_CLOSE
//! so it can guarantee cleanup on disconnect. Without
//! CREATE_BREAKAWAY_FROM_JOB on the sidecar's CreateProcessW call, the
//! sidecar inherits that job — `DETACHED_PROCESS` only severs the console,
//! not the job — and dies on disconnect.
//!
//! The test reproduces the kill chain locally:
//!   1. Create a named Job Object with KILL_ON_JOB_CLOSE | BREAKAWAY_OK
//!   2. Spawn helper which assigns self to job and then spawns `tendr start`
//!   3. After helper exits, all in-job processes have either inherited the
//!      job (`tendr start` did, sidecar would without breakaway) or broken
//!      away (sidecar should, with the fix)
//!   4. TerminateJobObject — kills anything still in the job
//!   5. Assert sidecar PID still alive

mod harness;

use harness::{Observation, poll_until};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr;
use std::time::Duration;

use windows_sys::Win32::Foundation::STILL_ACTIVE;
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// Create a named Job Object with the given limit flags. Returns an
/// `OwnedHandle` so the kernel handle is closed via `Drop` even if the
/// caller panics before explicit cleanup.
fn create_named_job_with_limits(name: &str, limit_flags: u32) -> OwnedHandle {
    let name_w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();

    // SAFETY: name pointer is valid (and NUL-terminated) for the duration
    // of the call; first arg null = default security; non-null return is
    // a valid kernel handle that we own.
    let job = unsafe { CreateJobObjectW(ptr::null(), name_w.as_ptr()) };
    assert!(
        !job.is_null(),
        "CreateJobObjectW failed: {}",
        std::io::Error::last_os_error()
    );

    // SAFETY: zeroed JOBOBJECT_EXTENDED_LIMIT_INFORMATION is the documented
    // way to initialize the struct before populating fields we care about.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = limit_flags;

    // SAFETY: `job` is a valid handle from the CreateJobObjectW above; the
    // info pointer + size match the JobObjectExtendedLimitInformation class.
    let ret = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    assert!(
        ret != 0,
        "SetInformationJobObject failed: {}",
        std::io::Error::last_os_error()
    );

    // SAFETY: `job` is a valid kernel HANDLE owned by this scope; transferring
    // ownership to OwnedHandle so Drop calls CloseHandle exactly once.
    unsafe { OwnedHandle::from_raw_handle(job as _) }
}

/// Returns true iff the process with `pid` is currently alive.
fn process_alive(pid: u32) -> bool {
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
        windows_sys::Win32::Foundation::CloseHandle(h);
        ok != 0 && code == STILL_ACTIVE as u32
    }
}

/// Wait up to `timeout` for `pid` to die. Returns `true` if it died within
/// the window, `false` if it remained alive throughout. Polling is cheaper
/// than a hard sleep and surfaces a regression as soon as the kill happens
/// rather than waiting the full timeout.
fn process_dies_within(pid: u32, timeout: Duration) -> bool {
    poll_until(timeout, Duration::from_millis(25), || {
        if process_alive(pid) {
            Observation::Pending(format!("pid {pid} alive"))
        } else {
            Observation::Ready(())
        }
    })
    .is_ok()
}

/// Resolve a binary path: prefer runtime env var (so the test can be moved to
/// another machine alongside the binaries), fall back to compile-time
/// CARGO_BIN_EXE for local cargo-test runs.
fn resolve_bin(env_key: &str, compile_time: &str) -> String {
    std::env::var(env_key).unwrap_or_else(|_| compile_time.to_string())
}

/// Force-kill a tendr session, ignoring errors. Used in test teardown.
fn force_kill_session(tendr_bin: &str, home: &std::path::Path, session: &str) {
    let _ = std::process::Command::new(tendr_bin)
        .env("HOME", home)
        .args(["kill", session, "--force"])
        .status();
}

#[test]
fn sidecar_survives_parent_job_kill() {
    let tendr_bin = resolve_bin("TENDR_TEST_BIN", env!("CARGO_BIN_EXE_tendr"));
    let helper_bin = resolve_bin(
        "TENDR_TEST_HELPER_BIN",
        env!("CARGO_BIN_EXE_test_breakaway_parent"),
    );
    let home = tempfile::tempdir().expect("tempdir");
    let session = format!("breakaway-{}", std::process::id());
    let sidecar_pid_out = home.path().join("sidecar_pid.txt");
    let job_name = format!("tendr-test-breakaway-{}", std::process::id());

    let job = create_named_job_with_limits(
        &job_name,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    );

    let status = std::process::Command::new(&helper_bin)
        .arg(&tendr_bin)
        .arg(home.path())
        .arg(&session)
        .arg(&sidecar_pid_out)
        .env("TEST_JOB_NAME", &job_name)
        .status()
        .expect("spawn helper");
    assert!(status.success(), "helper failed: {status:?}");

    let pid_str =
        std::fs::read_to_string(&sidecar_pid_out).expect("read sidecar_pid_out from helper");
    let sidecar_pid: u32 = pid_str.trim().parse().expect("parse sidecar pid");
    assert!(
        process_alive(sidecar_pid),
        "sidecar pid {sidecar_pid} should be alive immediately after helper exit"
    );

    // The kill: terminate the parent job. Without CREATE_BREAKAWAY_FROM_JOB,
    // the sidecar is in this job and dies here.
    // SAFETY: `job` is a valid OwnedHandle; exit code 1 is arbitrary.
    let ret = unsafe { TerminateJobObject(job.as_raw_handle() as _, 1) };
    assert!(
        ret != 0,
        "TerminateJobObject failed: {}",
        std::io::Error::last_os_error()
    );

    // Poll for kernel termination propagation; surfaces a regression early.
    let died = process_dies_within(sidecar_pid, Duration::from_secs(2));

    // Cleanup before asserting (so a failure still tears the session down).
    force_kill_session(&tendr_bin, home.path(), &session);
    drop(job); // close job handle explicitly for clarity (Drop would do this anyway)

    assert!(
        !died,
        "sidecar pid {sidecar_pid} was killed by parent job termination — \
         CREATE_BREAKAWAY_FROM_JOB likely missing from sidecar spawn flags"
    );
}

/// Fallback path: when the parent's job forbids breakaway, `tendr start`
/// must still succeed. Without the fallback, CreateProcessW returns
/// ERROR_ACCESS_DENIED and `tendr start` fails outright — strictly worse
/// than the degraded case where the sidecar inherits the parent's lifetime.
#[test]
fn sidecar_spawn_succeeds_when_parent_job_forbids_breakaway() {
    let tendr_bin = resolve_bin("TENDR_TEST_BIN", env!("CARGO_BIN_EXE_tendr"));
    let helper_bin = resolve_bin(
        "TENDR_TEST_HELPER_BIN",
        env!("CARGO_BIN_EXE_test_breakaway_parent"),
    );
    let home = tempfile::tempdir().expect("tempdir");
    let session = format!("no-breakaway-{}", std::process::id());
    let sidecar_pid_out = home.path().join("sidecar_pid.txt");
    let job_name = format!("tendr-test-no-breakaway-{}", std::process::id());

    // KILL_ON_JOB_CLOSE only — explicitly NO BREAKAWAY_OK.
    let job = create_named_job_with_limits(&job_name, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE);

    let status = std::process::Command::new(&helper_bin)
        .arg(&tendr_bin)
        .arg(home.path())
        .arg(&session)
        .arg(&sidecar_pid_out)
        .env("TEST_JOB_NAME", &job_name)
        .status()
        .expect("spawn helper");

    let succeeded = status.success();

    // Cleanup before asserting.
    force_kill_session(&tendr_bin, home.path(), &session);
    // SAFETY: `job` is a valid OwnedHandle; exit code 1 is arbitrary.
    unsafe { TerminateJobObject(job.as_raw_handle() as _, 1) };
    drop(job); // close job handle (Drop would do this anyway).

    assert!(
        succeeded,
        "tendr start should fall back to non-breakaway spawn when the \
         parent's job forbids breakaway. Helper exit: {status:?}"
    );
}
