//! Stopping a run without the sidecar's Job Object handle
//! ([`super::WindowsPlatform::kill_orphan`]): the sidecar has crashed or
//! does not answer, and only the child's identity from `meta.json` is left.
//!
//! The run is found two ways, both safe against a recycled PID:
//!
//! - its Job Object, by the name the sidecar gave it ([`job_name`]), which
//!   holds the child's PID and creation time. It exists while the sidecar
//!   holds it (the job kills its processes when the sidecar's handle
//!   closes), and lists every process of the run, however it was started:
//!   none can leave it, so a session started inside the run is in it too
//!   and is stopped with the run. A `Local\` name is per Terminal Services
//!   session (every SSH login runs in session 0, a desktop's sign-in in its
//!   own), so a `tendr kill` from another one cannot open it.
//! - otherwise, the child's process tree: the processes whose parent is the
//!   child or a process already found, and which were created after that
//!   parent. Each one found is held open, so its PID cannot be reused while
//!   the search goes on. Another tendr sidecar, a session started inside
//!   the run, is left alone with its tree, as on Unix, though the job's own
//!   end (the run's sidecar exiting) still stops it. This misses a process
//!   whose parent had already exited when the search began.
//!
//! A graceful stop first sends CTRL_BREAK to the child's process group,
//! from a helper attached to the child's console, then waits up to
//! [`GRACE`] before terminating what is left.

use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::HANDLE;

use crate::model::ids::ProcessIdentity;
use crate::platform::ProcessStatus;

/// How long a graceful stop waits before terminating, as `kill_child`.
pub(super) const GRACE: Duration = Duration::from_secs(5);

/// Passes of "find, then terminate" before giving up on a tree whose
/// processes keep starting new ones.
const TERMINATE_PASSES: usize = 50;

// From winnt.h; tendr's windows-sys features do not include SystemServices.
const JOB_OBJECT_QUERY: u32 = 0x0004;
const JOB_OBJECT_TERMINATE: u32 = 0x0008;

/// The name of the Job Object of the run whose child is `id`: unique to that
/// process, PID and creation time, so no other run's job has it.
pub(super) fn job_name(id: &ProcessIdentity) -> String {
    format!(r"Local\tendr-run-{}-{}", id.pid, id.start_time_ns)
}

pub(super) fn kill_orphan(id: &ProcessIdentity, force: bool) -> io::Result<()> {
    let root = match super::process_status(id) {
        ProcessStatus::IdentityMismatch => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "PID was recycled — refusing to kill wrong process",
            ));
        }
        ProcessStatus::OsError(kind) => {
            return Err(io::Error::new(kind, "failed to probe process status"));
        }
        ProcessStatus::Missing => None,
        // Opened and checked again (creation time against `id`) below, and
        // held open from then on.
        ProcessStatus::AliveVerified | ProcessStatus::Inaccessible => open_member(id)?,
    };
    if let Some(job) = open_run_job(id, root.as_ref()) {
        if !force && root.is_some() {
            ctrl_break(id);
            let deadline = Instant::now() + GRACE;
            while active_processes(&job).is_some_and(|n| n > 0) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        return super::terminate_job(&job);
    }
    let Some(root) = root else {
        // Gone, and no job to find the rest of the run by.
        return Ok(());
    };
    let mut tree = Tree {
        members: vec![root],
    };
    tree.extend();
    if !force {
        ctrl_break(id);
        let deadline = Instant::now() + GRACE;
        for member in &tree.members {
            let left = deadline.saturating_duration_since(Instant::now());
            wait(&member.handle, left);
        }
    }
    for _ in 0..TERMINATE_PASSES {
        tree.extend();
        let live: Vec<&Member> = tree.members.iter().filter(|m| !m.exited()).collect();
        if live.is_empty() {
            return Ok(());
        }
        for member in live {
            member.terminate();
            // Wait, so the next pass's snapshot shows what it started last.
            wait(&member.handle, Duration::from_secs(1));
        }
    }
    let left: Vec<String> = tree
        .members
        .iter()
        .filter(|m| !m.exited())
        .map(|m| m.id.pid.to_string())
        .collect();
    if left.is_empty() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "could not stop processes {} of the run: they keep starting more",
        left.join(", ")
    )))
}

/// The run's Job Object, if it can be opened and holds the child (when the
/// child is still running).
fn open_run_job(id: &ProcessIdentity, root: Option<&Member>) -> Option<OwnedHandle> {
    use windows_sys::Win32::System::JobObjects::{IsProcessInJob, OpenJobObjectW};
    let name: Vec<u16> = job_name(id).encode_utf16().chain([0]).collect();
    // SAFETY: `name` is NUL-terminated.
    let raw = unsafe { OpenJobObjectW(JOB_OBJECT_QUERY | JOB_OBJECT_TERMINATE, 0, name.as_ptr()) };
    if raw.is_null() {
        return None;
    }
    // SAFETY: a non-null handle from OpenJobObjectW is ours to close.
    let job = unsafe { OwnedHandle::from_raw_handle(raw) };
    if let Some(root) = root {
        let mut inside = 0;
        // SAFETY: both handles are valid; `inside` is a valid out-pointer.
        let ok = unsafe { IsProcessInJob(raw_handle(&root.handle), raw, &mut inside) };
        if ok == 0 || inside == 0 {
            return None;
        }
    }
    Some(job)
}

/// How many processes are still in `job`; `None` when it cannot be asked.
fn active_processes(job: &OwnedHandle) -> Option<u32> {
    use windows_sys::Win32::System::JobObjects::{
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
        QueryInformationJobObject,
    };
    let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the buffer and its size match the information class.
    let ok = unsafe {
        QueryInformationJobObject(
            raw_handle(job),
            JobObjectBasicAccountingInformation,
            (&raw mut info).cast(),
            std::mem::size_of_val(&info) as u32,
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(info.ActiveProcesses)
}

/// A process of the run, held open so its PID is not reused while held.
struct Member {
    id: ProcessIdentity,
    handle: OwnedHandle,
}

impl Member {
    fn exited(&self) -> bool {
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        // SAFETY: the handle is valid; a zero timeout only polls.
        unsafe { WaitForSingleObject(raw_handle(&self.handle), 0) == WAIT_OBJECT_0 }
    }

    fn terminate(&self) {
        // SAFETY: the handle is valid and was opened with PROCESS_TERMINATE;
        // it is the process found, whatever has happened to its PID since.
        unsafe {
            windows_sys::Win32::System::Threading::TerminateProcess(raw_handle(&self.handle), 1)
        };
    }
}

/// Open the process `id` names, if it is still that process.
fn open_member(id: &ProcessIdentity) -> io::Result<Option<Member>> {
    let Some(handle) = open_process(id.pid.get()) else {
        return Ok(None);
    };
    match super::creation_time_ns(raw_handle(&handle)) {
        Ok(start) if start == id.start_time_ns => Ok(Some(Member { id: *id, handle })),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "PID was recycled — refusing to kill wrong process",
        )),
        Err(e) => Err(e),
    }
}

fn open_process(pid: u32) -> Option<OwnedHandle> {
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    };
    // SAFETY: plain call; a null result means the process cannot be opened.
    let raw = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
            0,
            pid,
        )
    };
    // SAFETY: a non-null handle from OpenProcess is ours to close.
    (!raw.is_null()).then(|| unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// The child's process tree, as far as it has been found.
struct Tree {
    members: Vec<Member>,
}

impl Tree {
    /// Add every running process whose parent is a member and which was
    /// created after it, until a pass adds none. The members are held open,
    /// so a parent PID that matches one is that member, not a reuse. A
    /// candidate is opened, then found again with the same parent in a
    /// second snapshot: held open, its PID cannot have changed hands since.
    fn extend(&mut self) {
        loop {
            let is_member = |pid: u32| self.members.iter().any(|m| m.id.pid.get() == pid);
            let opened: Vec<(u32, u32, OwnedHandle)> = snapshot()
                .into_iter()
                .filter(|&(pid, parent)| !is_member(pid) && is_member(parent))
                .filter_map(|(pid, parent)| open_process(pid).map(|h| (pid, parent, h)))
                .collect();
            if opened.is_empty() {
                return;
            }
            let after = snapshot();
            let mut added = false;
            for (pid, parent, handle) in opened {
                if !after.contains(&(pid, parent)) {
                    continue;
                }
                let Some(parent_start) = self
                    .members
                    .iter()
                    .find(|m| m.id.pid.get() == parent)
                    .map(|m| m.id.start_time_ns)
                else {
                    continue;
                };
                let (Some(pid), Ok(start)) = (
                    std::num::NonZeroU32::new(pid),
                    super::creation_time_ns(raw_handle(&handle)),
                ) else {
                    continue;
                };
                // A process created before the member was the child of an
                // earlier holder of that PID. A sidecar is another session's.
                if start >= parent_start && !is_sidecar(&handle) {
                    let id = ProcessIdentity {
                        pid,
                        start_time_ns: start,
                    };
                    self.members.push(Member { id, handle });
                    added = true;
                }
            }
            if !added {
                return;
            }
        }
    }
}

/// Whether the process is a tendr sidecar, by the first argument of its
/// command line ([`super::SIDECAR_ARG`]), as on Unix. False when the
/// command line cannot be read.
fn is_sidecar(process: &OwnedHandle) -> bool {
    use windows_sys::Wdk::System::Threading::{
        NtQueryInformationProcess, ProcessCommandLineInformation,
    };
    use windows_sys::Win32::Foundation::UNICODE_STRING;
    use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

    let mut len = 0u32;
    // SAFETY: a zero-length buffer asks for the size only.
    unsafe {
        NtQueryInformationProcess(
            raw_handle(process),
            ProcessCommandLineInformation,
            std::ptr::null_mut(),
            0,
            &mut len,
        )
    };
    if (len as usize) < std::mem::size_of::<UNICODE_STRING>() {
        return false;
    }
    // usize-aligned: the buffer starts with a UNICODE_STRING.
    let mut buf = vec![0usize; (len as usize).div_ceil(std::mem::size_of::<usize>())];
    // SAFETY: the buffer holds `len` bytes.
    let status = unsafe {
        NtQueryInformationProcess(
            raw_handle(process),
            ProcessCommandLineInformation,
            buf.as_mut_ptr().cast(),
            len,
            &mut len,
        )
    };
    if status < 0 {
        return false;
    }
    // SAFETY: on success the buffer starts with a UNICODE_STRING whose
    // Buffer points into `buf`, `Length` bytes long.
    let line: Vec<u16> = unsafe {
        let s = &*buf.as_ptr().cast::<UNICODE_STRING>();
        std::slice::from_raw_parts(s.Buffer, usize::from(s.Length) / 2)
    }
    .iter()
    .copied()
    .chain([0])
    .collect();
    let mut argc = 0;
    // SAFETY: `line` is NUL-terminated; the result is freed below.
    let argv = unsafe { CommandLineToArgvW(line.as_ptr(), &mut argc) };
    if argv.is_null() {
        return false;
    }
    let first_arg = (argc >= 2).then(|| {
        // SAFETY: argv holds `argc` NUL-terminated strings.
        unsafe {
            let arg = *argv.add(1);
            let len = (0..).take_while(|&i| *arg.add(i) != 0).count();
            String::from_utf16_lossy(std::slice::from_raw_parts(arg, len))
        }
    });
    // SAFETY: allocated by CommandLineToArgvW.
    unsafe { windows_sys::Win32::Foundation::LocalFree(argv.cast()) };
    first_arg.as_deref() == Some(super::SIDECAR_ARG)
}

/// Every process's PID and parent PID; empty when no snapshot can be taken.
fn snapshot() -> Vec<(u32, u32)> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    // SAFETY: plain call.
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if raw == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    // SAFETY: a valid snapshot handle is ours to close.
    let snap = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut out = Vec::new();
    // SAFETY: `entry` is a valid, sized out-buffer for each call.
    let mut more = unsafe { Process32FirstW(raw_handle(&snap), &mut entry) } != 0;
    while more {
        out.push((entry.th32ProcessID, entry.th32ParentProcessID));
        more = unsafe { Process32NextW(raw_handle(&snap), &mut entry) } != 0;
    }
    out
}

/// Wait up to `timeout` for the process to exit.
fn wait(handle: &OwnedHandle, timeout: Duration) {
    let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
    // SAFETY: the handle is valid and has SYNCHRONIZE access.
    unsafe { windows_sys::Win32::System::Threading::WaitForSingleObject(raw_handle(handle), ms) };
}

/// Send CTRL_BREAK to the process group of `id`, which leads it: tendr
/// starts every child as a group leader. Only a process attached to the
/// child's console can, and `tendr kill` has its own, so a helper
/// (`tendr _ctrl-break`) attaches to it and sends the event. Best effort:
/// what does not stop is terminated after the grace period.
fn ctrl_break(id: &ProcessIdentity) {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::DETACHED_PROCESS;
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.args([
        "_ctrl-break",
        &id.pid.to_string(),
        &id.start_time_ns.to_string(),
    ])
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    // No console of its own, so it can attach to the child's.
    .creation_flags(DETACHED_PROCESS);
    if let Ok(mut helper) = crate::platform::spawn_external(&mut cmd) {
        let _ = helper.wait();
    }
}

/// `tendr _ctrl-break <pid> <start_time_ns>`: attach to the console of that
/// process, if it is still the one named, and send CTRL_BREAK to its group.
/// Returns the exit code: 0 when sent.
pub fn ctrl_break_helper(pid: u32, start_time_ns: u64) -> i32 {
    use windows_sys::Win32::System::Console::{
        AttachConsole, CTRL_BREAK_EVENT, FreeConsole, GenerateConsoleCtrlEvent,
        SetConsoleCtrlHandler,
    };
    unsafe extern "system" fn ignore(_: u32) -> i32 {
        1
    }
    let Some(pid) = std::num::NonZeroU32::new(pid) else {
        return 2;
    };
    let id = ProcessIdentity { pid, start_time_ns };
    // Held open from the check to the send, so the PID cannot change hands
    // in between.
    let Ok(Some(_member)) = open_member(&id) else {
        return 1;
    };
    // SAFETY: plain calls on this process's own console state; the handler
    // is a valid `extern "system"` function that keeps this process alive
    // whatever event reaches it.
    unsafe {
        FreeConsole();
        if AttachConsole(pid.get()) == 0 {
            return 1;
        }
        SetConsoleCtrlHandler(Some(ignore), 1);
        let sent = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid.get());
        FreeConsole();
        if sent == 0 { 1 } else { 0 }
    }
}

fn raw_handle(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle().cast()
}
