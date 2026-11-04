//! Windows implementation of the Platform trait.
//!
//! All Platform methods are implemented: process lifecycle (spawn suspended
//! into a Job Object, kill, identity, status), sidecar spawn with
//! STARTUPINFOEXW handle whitelisting, readiness channel (CreatePipe), and
//! stdin transport (named pipes).

mod launch;
mod orphan;

pub use orphan::ctrl_break_helper;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read, Write};
use std::num::NonZeroU32;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Arc;

use crate::model::ids::ProcessIdentity;
use crate::platform::{Platform, ProcessStatus};

/// Windows implementation of the Platform trait.
pub struct WindowsPlatform;

/// How long a graceful stop waits for the child to exit on CTRL_BREAK before
/// terminating its Job Object.
pub const GRACEFUL_STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

impl WindowsPlatform {
    /// A graceful [`Platform::kill_child`] that waits up to `grace`, not
    /// [`GRACEFUL_STOP_GRACE`], before escalating: a test whose child must
    /// exit on CTRL_BREAK passes a grace load cannot outlast, without
    /// changing it for any other kill in the process.
    ///
    /// # Errors
    /// Terminating the Job Object failed.
    pub fn stop_gracefully(handle: &ChildKillHandle, grace: std::time::Duration) -> io::Result<()> {
        // Best-effort CTRL_BREAK, then wait via WaitForSingleObject, then
        // escalate. Uses a real waitable handle instead of process_status
        // because Windows keeps the process object alive while any handle is
        // open (e.g. SupervisedChild's own), making PID-based liveness
        // checks unreliable.
        send_ctrl_break(handle.identity.pid.get());

        let grace_ms = u32::try_from(grace.as_millis()).unwrap_or(u32::MAX - 1);
        if wait_for_process_exit(handle.identity.pid.get(), grace_ms) {
            return Ok(());
        }

        // Grace period expired — escalate to force.
        terminate_job(&handle.job_object)
    }
}

/// Opaque supervised-child state for Windows: the process, its verified
/// `ProcessIdentity`, an Arc'd Job Object for tree kill, and the parent
/// ends of its standard streams until they are taken.
pub struct SupervisedChild {
    process: OwnedHandle,
    identity: ProcessIdentity,
    job_object: Arc<OwnedHandle>,
    stdin: Option<File>,
    stdout: Option<File>,
    stderr: Option<File>,
}

/// Lightweight kill handle for Windows.
/// Carries an Arc'd Job Object HANDLE for tree kill and ProcessIdentity
/// for status checks and GenerateConsoleCtrlEvent targeting.
#[derive(Clone)]
pub struct ChildKillHandle {
    identity: ProcessIdentity,
    job_object: Arc<OwnedHandle>,
}

/// Stdin transport for Windows.
/// Holds the server-side named pipe HANDLE for stdin forwarding.
///
/// `has_connected` tracks whether any client has ever connected. This prevents
/// a race where `DisconnectNamedPipe` on the first `accept_stdin_connection`
/// call would sever a client that connected between `CreateNamedPipeW` and
/// `ConnectNamedPipe` (the `ERROR_PIPE_CONNECTED` case).
pub struct StdinTransport {
    pipe_handle: OwnedHandle,
    #[allow(dead_code)] // name used conceptually; cleanup is via handle drop
    pipe_name: String,
    has_connected: std::sync::atomic::AtomicBool,
}

// SAFETY: StdinTransport holds an OwnedHandle (kernel object) and an AtomicBool, safe to send.
unsafe impl Send for StdinTransport {}

impl Platform for WindowsPlatform {
    type SupervisedChild = SupervisedChild;
    type ChildKillHandle = ChildKillHandle;
    type StdinTransport = StdinTransport;
    type ReadyReader = File;
    type ReadyWriter = File;

    fn spawn_sidecar(
        tendr_bin: &Path,
        session_dir: &Path,
        ready_writer: &File,
        lifetime: super::SessionLifetime,
    ) -> io::Result<u32> {
        // Raw CreateProcessW with STARTUPINFOEXW + PROC_THREAD_ATTRIBUTE_HANDLE_LIST.
        // This is the only way to inherit exactly one handle (the ready pipe)
        // without leaking every other inheritable handle from the parent.
        spawn_sidecar_raw(tendr_bin, session_dir, ready_writer, lifetime)
    }

    fn ready_channel() -> io::Result<(File, File)> {
        use windows_sys::Win32::System::Pipes::CreatePipe;

        let mut read_handle = std::ptr::null_mut();
        let mut write_handle = std::ptr::null_mut();

        // Both ends are created non-inheritable (null security attributes),
        // never made inheritable and fixed up after: any process another
        // thread starts in between would inherit them, and a stray copy of
        // the read end keeps a write from failing once we drop ours, a
        // stray write end keeps `read_ready_signal` from seeing EOF.
        // `spawn_sidecar_raw` hands the sidecar an inheritable duplicate.
        // SAFETY: pointers are valid out params; null attributes are allowed.
        let ret = unsafe { CreatePipe(&mut read_handle, &mut write_handle, std::ptr::null(), 0) };
        if ret == 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: both handles are valid from CreatePipe and owned by us.
        let read_file = unsafe { File::from_raw_handle(read_handle as *mut _) };
        let write_file = unsafe { File::from_raw_handle(write_handle as *mut _) };

        Ok((read_file, write_file))
    }

    fn read_ready_signal(mut reader: File) -> io::Result<String> {
        let mut buf = String::new();
        reader.read_to_string(&mut buf)?;
        if buf.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "sidecar died without signaling readiness",
            ));
        }
        Ok(buf)
    }

    fn write_ready_signal(mut writer: File, message: &str) -> io::Result<()> {
        writer.write_all(message.as_bytes())?;
        // writer dropped here — closes HANDLE, reader sees EOF
        Ok(())
    }

    fn spawn_child(
        argv: &[String],
        stdin_piped: bool,
        cwd: Option<&Path>,
        env_remove: &BTreeSet<String>,
        env: &BTreeMap<String, String>,
    ) -> io::Result<SupervisedChild> {
        let (stdin_child, stdin) = if stdin_piped {
            let (read, write) = pipe()?;
            (read, Some(write))
        } else {
            (null_device()?, None)
        };
        let (stdout, stdout_child) = pipe()?;
        let (stderr, stderr_child) = pipe()?;
        let child_ends = [&stdin_child, &stdout_child, &stderr_child];
        for end in child_ends {
            set_handle_inheritable(end.as_raw_handle().cast(), true)?;
        }
        let handles = child_ends.map(|end| end.as_raw_handle().cast());
        let (process, identity, job) = start_contained(
            argv,
            cwd,
            env_remove,
            env,
            &launch::Streams::Handles(handles),
        )?;
        // The child's ends are the child's now; ours must close, so that
        // stdout reads end when the child's tree closes its copies.
        drop((stdin_child, stdout_child, stderr_child));
        Ok(SupervisedChild {
            process,
            identity,
            job_object: Arc::new(job),
            stdin,
            stdout: Some(stdout),
            stderr: Some(stderr),
        })
    }

    fn spawn_child_pty(
        _argv: &[String],
        _cwd: Option<&Path>,
        _env_remove: &BTreeSet<String>,
        _env: &BTreeMap<String, String>,
    ) -> io::Result<SupervisedChild> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "PTY not supported on Windows yet",
        ))
    }

    fn child_identity(child: &SupervisedChild) -> ProcessIdentity {
        child.identity
    }

    fn child_wait(child: &mut SupervisedChild) -> io::Result<ExitStatus> {
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};
        // SAFETY: the process handle is valid and has SYNCHRONIZE access.
        if unsafe { WaitForSingleObject(child.process.as_raw_handle().cast(), INFINITE) }
            != WAIT_OBJECT_0
        {
            return Err(io::Error::last_os_error());
        }
        exit_status(&child.process)
    }

    fn child_try_wait(child: &mut SupervisedChild) -> io::Result<Option<ExitStatus>> {
        use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        // SAFETY: the process handle is valid; a zero timeout only polls.
        match unsafe { WaitForSingleObject(child.process.as_raw_handle().cast(), 0) } {
            WAIT_OBJECT_0 => exit_status(&child.process).map(Some),
            WAIT_TIMEOUT => Ok(None),
            _ => Err(io::Error::last_os_error()),
        }
    }

    fn child_stdout(child: &mut SupervisedChild) -> Option<Box<dyn io::Read + Send>> {
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn io::Read + Send>)
    }

    fn child_stderr(child: &mut SupervisedChild) -> Option<Box<dyn io::Read + Send>> {
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn io::Read + Send>)
    }

    fn child_stdin(child: &mut SupervisedChild) -> Option<Box<dyn io::Write + Send>> {
        child
            .stdin
            .take()
            .map(|s| Box::new(s) as Box<dyn io::Write + Send>)
    }

    fn pty_resize_fd(_child: &SupervisedChild) -> Option<File> {
        None // No PTY support on Windows in slice one
    }

    fn child_kill_handle(child: &SupervisedChild) -> ChildKillHandle {
        ChildKillHandle {
            identity: child.identity,
            job_object: child.job_object.clone(),
        }
    }

    fn kill_child(handle: &ChildKillHandle, force: bool) -> io::Result<()> {
        if force {
            return terminate_job(&handle.job_object);
        }

        Self::stop_gracefully(handle, GRACEFUL_STOP_GRACE)
    }

    /// The Job Object keeps every process the run starts, orphaned or not.
    fn adopt_orphans() -> io::Result<bool> {
        Ok(false)
    }

    fn reap_orphans(_child: &ProcessIdentity) {}

    /// Only `kill_child`: the Job Object is kill-on-close and only the
    /// sidecar holds it, so the rest of the tree ends when the sidecar exits
    /// after the stop.
    fn stop_run(handle: &ChildKillHandle, force: bool) -> io::Result<()> {
        Self::kill_child(handle, force)
    }

    /// Without the sidecar's Job Object handle: see [`orphan`]. The tree is
    /// found by the run's named job, or else by parent links from the
    /// child; unless `force`, it gets CTRL_BREAK and [`orphan::GRACE`]
    /// first. A PID that no longer names the child is refused.
    fn kill_orphan(id: &ProcessIdentity, force: bool) -> io::Result<()> {
        orphan::kill_orphan(id, force)
    }

    fn self_identity() -> io::Result<ProcessIdentity> {
        let pid = std::process::id();
        process_identity(pid)
    }

    fn process_identity(pid: u32) -> io::Result<ProcessIdentity> {
        process_identity(pid)
    }

    fn process_status(id: &ProcessIdentity) -> ProcessStatus {
        process_status(id)
    }

    fn create_stdin_transport(session_dir: &Path) -> io::Result<StdinTransport> {
        let pipe_name = stdin_pipe_name(session_dir);
        let pipe_handle = create_named_pipe_server(&pipe_name)?;
        Ok(StdinTransport {
            pipe_handle,
            pipe_name,
            has_connected: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn accept_stdin_connection(
        transport: &StdinTransport,
        _session_dir: &Path,
    ) -> Option<Box<dyn io::Read + Send>> {
        use std::sync::atomic::Ordering;
        use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, DisconnectNamedPipe};

        let handle = transport.pipe_handle.as_raw_handle() as *mut _;

        // Only disconnect after the first successful connection. On the first
        // call, a client may have already connected between CreateNamedPipeW
        // and this ConnectNamedPipe — DisconnectNamedPipe would sever that
        // connection and lose input.
        if transport.has_connected.load(Ordering::Relaxed) {
            unsafe { DisconnectNamedPipe(handle) };
        }

        // Block until a new client connects.
        let ret = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };
        if ret == 0 {
            let err = io::Error::last_os_error();
            // ERROR_PIPE_CONNECTED = client connected before ConnectNamedPipe
            // was called — that's fine, proceed with the read.
            if err.raw_os_error()
                != Some(windows_sys::Win32::Foundation::ERROR_PIPE_CONNECTED as i32)
            {
                return None; // pipe broken or closed
            }
        }

        transport.has_connected.store(true, Ordering::Relaxed);
        Some(Box::new(PipeReader { handle }))
    }

    fn open_stdin_writer(session_dir: &Path) -> io::Result<File> {
        use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING,
        };

        let pipe_name = stdin_pipe_name(session_dir);
        let wide_name: Vec<u16> = pipe_name.encode_utf16().chain(std::iter::once(0)).collect();

        let handle = unsafe {
            CreateFileW(
                wide_name.as_ptr(),
                windows_sys::Win32::Foundation::GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };

        if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                || err.kind() == io::ErrorKind::NotFound
            {
                return Err(io::Error::new(io::ErrorKind::ConnectionRefused, err));
            }
            return Err(err);
        }

        Ok(unsafe { File::from_raw_handle(handle as *mut _) })
    }

    fn remove_stdin_transport(_session_dir: &Path) {
        // Named pipes are kernel objects cleaned up when all handles close.
        // StdinTransport's OwnedHandle drop handles this automatically.
        //
        // Platform divergence: on Unix, removing the FIFO file unblocks
        // accept_stdin_connection (File::open returns Err). On Windows, a
        // thread blocked in ConnectNamedPipe cannot be woken by this no-op;
        // it unblocks only when the process exits (dropping the pipe handle)
        // or a client connects. This is acceptable because the sidecar's
        // forwarding thread is the only caller, and sidecar shutdown drops
        // the StdinTransport which closes the pipe handle.
    }

    fn ready_writer_from_env() -> io::Result<File> {
        let handle_str = std::env::var("TENDR_READY_HANDLE")
            .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "TENDR_READY_HANDLE not set"))?;
        let handle: usize = handle_str.parse().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "TENDR_READY_HANDLE is not a valid handle",
            )
        })?;
        // SAFETY: handle is the inheritable duplicate the parent listed in
        // PROC_THREAD_ATTRIBUTE_HANDLE_LIST. from_raw_handle takes ownership.
        Ok(unsafe { File::from_raw_handle(handle as *mut _) })
    }

    fn seal_ready_fd(writer: File) -> io::Result<File> {
        // Replace the inheritable HANDLE with a non-inheritable duplicate.
        // Takes ownership of the old File (closing the inheritable handle)
        // and returns a new File wrapping a non-inheritable copy.
        use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;

        let old_handle = writer.into_raw_handle() as *mut _;
        let current_process = unsafe { GetCurrentProcess() };
        let mut new_handle: *mut core::ffi::c_void = std::ptr::null_mut();

        let ret = unsafe {
            DuplicateHandle(
                current_process,
                old_handle,
                current_process,
                &mut new_handle,
                0, // ignored with DUPLICATE_SAME_ACCESS
                0, // bInheritHandle = FALSE
                DUPLICATE_SAME_ACCESS,
            )
        };

        // Close the original inheritable handle.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(old_handle) };

        if ret == 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: new_handle is a valid, non-inheritable duplicate.
        Ok(unsafe { File::from_raw_handle(new_handle) })
    }
}

// --- Sidecar runtime ---

/// Allocate a hidden console for the sidecar process.
///
/// The sidecar spawns with DETACHED_PROCESS (no console). Before spawning
/// children, it must acquire a console so that:
/// - Children inherit it via CREATE_NEW_PROCESS_GROUP
/// - GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) can reach them
///
/// The ShowWindow(SW_HIDE) step is best-effort — AllocConsole is the
/// important part. If the window can't be hidden, graceful stop still works.
pub fn prepare_sidecar_console() {
    use windows_sys::Win32::System::Console::{AllocConsole, GetConsoleWindow};
    use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, ShowWindow};

    // SAFETY: AllocConsole is safe to call. Returns FALSE if the process
    // already has a console — harmless.
    unsafe { AllocConsole() };

    // Best-effort: hide the console window.
    let hwnd = unsafe { GetConsoleWindow() };
    if !hwnd.is_null() {
        unsafe { ShowWindow(hwnd, SW_HIDE) };
    }
}

// --- Handle helpers ---

/// Set or clear the inheritable flag on a HANDLE.
fn set_handle_inheritable(
    handle: windows_sys::Win32::Foundation::HANDLE,
    inheritable: bool,
) -> io::Result<()> {
    use windows_sys::Win32::Foundation::HANDLE_FLAG_INHERIT;
    use windows_sys::Win32::Foundation::SetHandleInformation;

    let flags = if inheritable { HANDLE_FLAG_INHERIT } else { 0 };
    let ret = unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, flags) };
    if ret == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// An inheritable duplicate of `file`'s handle, created inheritable in one
/// call rather than flagged after.
fn inheritable_duplicate(file: &File) -> io::Result<OwnedHandle> {
    use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let mut dup = std::ptr::null_mut();
    // SAFETY: the source handle is valid for the borrow of `file`; `dup` is
    // a valid out param.
    let ret = unsafe {
        let process = GetCurrentProcess();
        DuplicateHandle(
            process,
            file.as_raw_handle() as _,
            process,
            &mut dup,
            0, // ignored with DUPLICATE_SAME_ACCESS
            1, // bInheritHandle = TRUE
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ret == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: DuplicateHandle succeeded; we own the new handle.
    Ok(unsafe { OwnedHandle::from_raw_handle(dup) })
}

/// Clear the inheritable flag on this process's stdin, stdout and stderr, for
/// [`super::spawn_external`]. A missing (null) handle makes the call fail,
/// which is harmless: there is nothing to inherit.
pub(super) fn std_handles_not_inheritable() {
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: GetStdHandle only reads this process's own std handle.
        let _ = set_handle_inheritable(unsafe { GetStdHandle(which) }, false);
    }
}

// --- Sidecar spawn (raw CreateProcessW) ---

/// How the sidecar leaves the job tendr runs in, if any, so that it
/// outlives its caller: an SSH session's processes, for one, are in a job
/// that is closed, killing them, when the connection ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Breakaway {
    /// Not in a job, or in one whose children leave it by themselves
    /// (`JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK`): nothing to do.
    NotNeeded,
    /// Ask for it with `CREATE_BREAKAWAY_FROM_JOB`: the job allows it
    /// (`JOB_OBJECT_LIMIT_BREAKAWAY_OK`), or it could not be asked.
    Request,
    /// The job allows neither: the sidecar stays in it and lasts as long as
    /// the job lets it.
    Forbidden,
}

/// Whether, and how, the sidecar must break away from this process's job.
fn breakaway() -> Breakaway {
    use windows_sys::Win32::System::JobObjects::{
        IsProcessInJob, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        QueryInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    // BOOL is defined as i32 in windows-sys.
    let mut in_job: i32 = 0;
    // SAFETY: GetCurrentProcess is a pseudo-handle (always valid); second
    // arg null = "the job containing the calling process"; out param points
    // to a valid stack i32.
    let ok = unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job) };
    if ok == 0 {
        return Breakaway::Request;
    }
    if in_job == 0 {
        return Breakaway::NotNeeded;
    }

    // SAFETY: zeroed JOBOBJECT_EXTENDED_LIMIT_INFORMATION is the documented
    // initialization pattern before passing as an out-buffer.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    let mut returned: u32 = 0;
    // SAFETY: null hJob = "the job associated with the calling process" (the
    // innermost, when jobs are nested); the info pointer + advertised size
    // match the JobObjectExtendedLimitInformation class; the returned-bytes
    // out param is a valid stack u32.
    let ok = unsafe {
        QueryInformationJobObject(
            std::ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            &mut info as *mut _ as *mut _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            &mut returned,
        )
    };
    if ok == 0 {
        return Breakaway::Request;
    }
    let flags = info.BasicLimitInformation.LimitFlags;
    if flags & JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK != 0 {
        Breakaway::NotNeeded
    } else if flags & JOB_OBJECT_LIMIT_BREAKAWAY_OK != 0 {
        Breakaway::Request
    } else {
        Breakaway::Forbidden
    }
}

/// Whether tendr runs for an SSH connection, whose job Windows OpenSSH (or
/// another server) may close when the connection ends. Known by
/// `SSH_CONNECTION`, which every process started from an SSH login
/// inherits. Outside one, a job that keeps its processes is its owner's
/// choice (an IDE, a CI runner), as a cgroup is on Linux, and the session
/// lives as long as it.
fn over_ssh() -> bool {
    std::env::var_os("SSH_CONNECTION").is_some()
}

/// The error `start` fails with over SSH when the session could not
/// outlive the connection. Kept rather than starting a session that dies
/// with the connection's job. Inside a tendr run (`TENDR_RUN_ID`, which a
/// run gives its child, is set) the job is most likely the run's, which no
/// process leaves: the message says so, since the SSH connection is then
/// not what the session would end with.
fn breakaway_forbidden() -> io::Error {
    let message = if std::env::var_os("TENDR_RUN_ID").is_some() {
        "the session could not outlive the tendr run it is started from: on \
         Windows a session started inside a run stays in the run's job, \
         which no process leaves, and ends with the run; start it from \
         outside the run"
    } else {
        "the session could not outlive this SSH connection: tendr runs in a \
         Windows job that does not let its processes break away, so the \
         session would end with the connection"
    };
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

/// Spawn the sidecar with explicit handle whitelisting.
///
/// Uses STARTUPINFOEXW + PROC_THREAD_ATTRIBUTE_HANDLE_LIST so the sidecar
/// inherits ONLY the ready pipe write handle. Without this, CreateProcessW
/// with bInheritHandles=TRUE leaks every inheritable handle from the parent
/// (stdout/stderr pipes from test harnesses, SSH sessions, etc.).
///
/// `ready_writer` itself is not inheritable (see `ready_channel`), and the
/// list may only name inheritable handles, so the sidecar is given an
/// inheritable duplicate that is closed here once CreateProcessW returns.
/// A process another thread starts while it exists can still inherit it,
/// as with std's own stdio pipes (std serialises its spawns with a private
/// lock we cannot share). That copy is the write end, so the cost is a
/// readiness read that waits for that process too, never a lost failure.
///
/// Requires Windows 7+ (PROC_THREAD_ATTRIBUTE_HANDLE_LIST support).
fn spawn_sidecar_raw(
    tendr_bin: &Path,
    session_dir: &Path,
    ready_writer: &File,
    lifetime: super::SessionLifetime,
) -> io::Result<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, HANDLE};
    use windows_sys::Win32::System::Threading::{
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_UNICODE_ENVIRONMENT,
        CreateProcessW, DETACHED_PROCESS, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, InitializeProcThreadAttributeList,
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, STARTUPINFOEXW,
        UpdateProcThreadAttribute,
    };

    // CREATE_BREAKAWAY_FROM_JOB lets the sidecar escape any job object the
    // parent (e.g. OpenSSH-Server's per-session job with KILL_ON_JOB_CLOSE)
    // assigned us to. Without it, DETACHED_PROCESS only severs the *console*,
    // not the *job*, and the sidecar dies on parent disconnect. Over SSH, a
    // job that forbids it fails a detached start instead: a session that
    // may die with the connection is not one tendr reports as started.
    // Elsewhere, and for a foreground run, which lasts only as long as its
    // caller anyway, the sidecar stays in the job.
    let breakaway = breakaway();
    let refuse = over_ssh() && lifetime == super::SessionLifetime::Detached;
    if breakaway == Breakaway::Forbidden && refuse {
        return Err(breakaway_forbidden());
    }

    let ready_dup = inheritable_duplicate(ready_writer)?;
    let ready_handle: HANDLE = ready_dup.as_raw_handle() as *mut _;

    // --- Build command line ---
    // CreateProcessW wants a single mutable wide string for the command line.
    let cmdline = format!(
        "\"{}\" {SIDECAR_ARG} --session-dir \"{}\"",
        tendr_bin.display(),
        session_dir.display(),
    );
    let mut cmdline_wide: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();

    // --- Build environment block ---
    // Unicode environment block: sorted KEY=VALUE\0 pairs, terminated by \0.
    let mut env_vars: Vec<(String, String)> = std::env::vars().collect();
    // Add/override the ready handle env var.
    let handle_value = ready_handle as usize;
    let ready_key = "TENDR_READY_HANDLE";
    if let Some(entry) = env_vars.iter_mut().find(|(k, _)| k == ready_key) {
        entry.1 = handle_value.to_string();
    } else {
        env_vars.push((ready_key.to_string(), handle_value.to_string()));
    }
    // Sort case-insensitively (Windows requirement for environment blocks).
    env_vars.sort_by_cached_key(|(k, _)| k.to_lowercase());
    let mut env_block: Vec<u16> = Vec::new();
    for (k, v) in &env_vars {
        let entry = format!("{k}={v}");
        env_block.extend(entry.encode_utf16());
        env_block.push(0);
    }
    env_block.push(0); // double-null terminator

    // --- Initialize proc thread attribute list ---
    // First call: query required size.
    let mut attr_size: usize = 0;
    unsafe {
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut attr_size);
    }
    // attr_size now holds the required allocation size.
    let mut attr_buf: Vec<u8> = vec![0u8; attr_size];
    let attr_list = attr_buf.as_mut_ptr() as *mut _;

    let ret = unsafe { InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_size) };
    if ret == 0 {
        return Err(io::Error::last_os_error());
    }

    // Set the handle list: only the ready pipe write handle.
    let mut handles: [HANDLE; 1] = [ready_handle];
    let ret = unsafe {
        UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_mut_ptr().cast(),
            std::mem::size_of_val(&handles),
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    };
    if ret == 0 {
        let err = io::Error::last_os_error();
        unsafe { DeleteProcThreadAttributeList(attr_list) };
        return Err(err);
    }

    // --- Build STARTUPINFOEXW ---
    let mut si_ex: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si_ex.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    si_ex.lpAttributeList = attr_list;

    // --- Create process ---
    let creation_flags = CREATE_NEW_PROCESS_GROUP
        | DETACHED_PROCESS
        | EXTENDED_STARTUPINFO_PRESENT
        | CREATE_UNICODE_ENVIRONMENT;

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let mut create = |flags| unsafe {
        CreateProcessW(
            std::ptr::null(),          // lpApplicationName (use cmdline)
            cmdline_wide.as_mut_ptr(), // lpCommandLine
            std::ptr::null(),          // lpProcessAttributes
            std::ptr::null(),          // lpThreadAttributes
            1,                         // bInheritHandles = TRUE
            flags,                     // dwCreationFlags
            env_block.as_ptr().cast(), // lpEnvironment
            std::ptr::null(),          // lpCurrentDirectory (inherit)
            &si_ex.StartupInfo,        // lpStartupInfo
            &mut pi,                   // lpProcessInformation
        )
    };
    let access_denied =
        || io::Error::last_os_error().raw_os_error() == Some(ERROR_ACCESS_DENIED as i32);

    let mut result = if breakaway == Breakaway::Request {
        create(creation_flags | CREATE_BREAKAWAY_FROM_JOB)
    } else {
        create(creation_flags)
    };
    // An outer job of nested ones can forbid what the inner allows.
    let outer_forbids = result == 0 && breakaway == Breakaway::Request && access_denied();
    if outer_forbids && !refuse {
        result = create(creation_flags);
    }
    let err = io::Error::last_os_error();

    unsafe { DeleteProcThreadAttributeList(attr_list) };

    if result == 0 {
        if outer_forbids && refuse {
            return Err(breakaway_forbidden());
        }
        return Err(err);
    }

    let pid = pi.dwProcessId;

    // Close process and thread handles — we don't need them.
    // The sidecar runs independently; we track it by PID.
    unsafe {
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
    }

    Ok(pid)
}

// --- Job Object helpers ---

/// Start `argv` suspended, put it in a new kill-on-close Job Object named
/// for it ([`orphan::job_name`]), then let it run: every process it starts
/// is in the job from its first instruction. Returns the process, its
/// identity and the job. On failure nothing is left running.
fn start_contained(
    argv: &[String],
    cwd: Option<&Path>,
    env_remove: &BTreeSet<String>,
    env: &BTreeMap<String, String>,
    streams: &launch::Streams,
) -> io::Result<(OwnedHandle, ProcessIdentity, OwnedHandle)> {
    use windows_sys::Win32::System::Threading::{ResumeThread, TerminateProcess};

    let mut environment = launch::Environment::inherited(env_remove, env);
    let module_path = argv
        .first()
        .and_then(|program| windows_powershell_module_path(program, env_remove, env));
    if let Some(module_path) = module_path {
        environment.set(PS_MODULE_PATH, module_path.into());
    }
    let child = launch::spawn_suspended(argv, cwd, &environment, streams)?;
    let contained = (|| {
        let identity = ProcessIdentity {
            pid: NonZeroU32::new(child.pid).ok_or_else(|| io::Error::other("pid is zero"))?,
            start_time_ns: creation_time_ns(child.process.as_raw_handle().cast())?,
        };
        let job = create_job_object(&orphan::job_name(&identity))?;
        assign_process_to_job(
            job.as_raw_handle().cast(),
            child.process.as_raw_handle().cast(),
        )?;
        // SAFETY: the thread handle is valid and has THREAD_SUSPEND_RESUME.
        if unsafe { ResumeThread(child.thread.as_raw_handle().cast()) } == u32::MAX {
            return Err(io::Error::last_os_error());
        }
        Ok((identity, job))
    })();
    match contained {
        Ok((identity, job)) => Ok((child.process, identity, job)),
        Err(e) => {
            // Still suspended (or in the job, which dropping closes): it has
            // started nothing.
            // SAFETY: the process handle is valid and has PROCESS_TERMINATE.
            unsafe { TerminateProcess(child.process.as_raw_handle().cast(), 1) };
            Err(e)
        }
    }
}

/// The hidden subcommand that runs a sidecar (`main.rs`), its first
/// argument; an orphan kill finds other sessions' sidecars by it.
const SIDECAR_ARG: &str = "_sidecar";

/// Create a Job Object named `name` with KILL_ON_JOB_CLOSE (safety net for
/// crashes) and no breakaway: nothing the run starts can leave it, so a
/// stop reaches all of it. Cygwin and MSYS programs (Git Bash's `sh`, say)
/// start every child with `CREATE_BREAKAWAY_FROM_JOB` when their job allows
/// it, so a job that allowed breakaway would lose them. A session started
/// inside the run stays in it too and ends with the run. If an object of
/// that name already exists, which a run's own name never should, the job
/// is anonymous instead, so it is never one someone else made.
fn create_job_object(name: &str) -> io::Result<OwnedHandle> {
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };

    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    // SAFETY: `wide` is NUL-terminated. Returns null on failure.
    let mut handle = unsafe { CreateJobObjectW(std::ptr::null(), wide.as_ptr()) };
    if !handle.is_null() && unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        // SAFETY: the handle was just returned to us.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
        // SAFETY: null name = anonymous job object.
        handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    }
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: handle is a valid non-null HANDLE from CreateJobObjectW.
    let job = unsafe { OwnedHandle::from_raw_handle(handle) };

    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: the buffer and its size match the information class.
    let ret = unsafe {
        SetInformationJobObject(
            handle,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ret == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}

/// Assign a process to a Job Object.
fn assign_process_to_job(
    job: windows_sys::Win32::Foundation::HANDLE,
    process: windows_sys::Win32::Foundation::HANDLE,
) -> io::Result<()> {
    use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

    // SAFETY: both handles are valid: the job from create_job_object, the
    // process from launch::spawn_suspended.
    let ret = unsafe { AssignProcessToJobObject(job, process) };
    if ret == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The exit status of an exited process.
fn exit_status(process: &OwnedHandle) -> io::Result<ExitStatus> {
    use std::os::windows::process::ExitStatusExt;
    use windows_sys::Win32::System::Threading::GetExitCodeProcess;
    let mut code = 0;
    // SAFETY: the handle is valid; `code` is a valid out-pointer.
    if unsafe { GetExitCodeProcess(process.as_raw_handle().cast(), &mut code) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ExitStatus::from_raw(code))
}

/// An anonymous pipe, `(read, write)`, neither end inheritable.
fn pipe() -> io::Result<(File, File)> {
    use windows_sys::Win32::System::Pipes::CreatePipe;
    let mut read = std::ptr::null_mut();
    let mut write = std::ptr::null_mut();
    // SAFETY: valid out-pointers; null attributes = not inheritable.
    if unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both handles are valid and ours.
    Ok(unsafe { (File::from_raw_handle(read), File::from_raw_handle(write)) })
}

/// The `NUL` device for reading, as a child's stdin when none is piped.
fn null_device() -> io::Result<File> {
    std::fs::OpenOptions::new().read(true).open("NUL")
}

/// Terminate all processes in a Job Object. Idempotent — already-dead is Ok.
fn terminate_job(job: &OwnedHandle) -> io::Result<()> {
    use windows_sys::Win32::System::JobObjects::TerminateJobObject;

    let ret = unsafe { TerminateJobObject(job.as_raw_handle() as *mut _, 1) };
    if ret == 0 {
        let err = io::Error::last_os_error();
        // ERROR_ACCESS_DENIED can mean the job is already terminated.
        if err.raw_os_error() != Some(windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED as i32) {
            return Err(err);
        }
    }
    Ok(())
}

/// Whether the process with this PID has already exited (its object signalled).
/// False when it is running, or when it cannot be opened for waiting.
fn has_exited(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    // SAFETY: OpenProcess with SYNCHRONIZE is the minimum right for waiting.
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return false;
    }
    // SAFETY: handle is valid from OpenProcess; a zero timeout only polls.
    let result = unsafe { WaitForSingleObject(handle, 0) };
    unsafe { CloseHandle(handle) };
    result == WAIT_OBJECT_0
}

/// Wait for a process to exit using WaitForSingleObject.
/// Opens a SYNCHRONIZE handle to the process by PID, waits up to `timeout_ms`.
/// Returns true if the process exited within the timeout, false otherwise.
/// Returns true (treat as exited) if the process handle cannot be opened
/// (process already gone or access denied).
fn wait_for_process_exit(pid: u32, timeout_ms: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    // SAFETY: OpenProcess with SYNCHRONIZE is the minimum right for waiting.
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        // Can't open — process likely already exited.
        return true;
    }

    // SAFETY: handle is valid from OpenProcess. WaitForSingleObject blocks
    // until the process exits or the timeout expires.
    let result = unsafe { WaitForSingleObject(handle, timeout_ms) };
    unsafe { CloseHandle(handle) };

    result == WAIT_OBJECT_0
}

/// Best-effort CTRL_BREAK to a process group. No-op if the child has no console.
fn send_ctrl_break(pid: u32) {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
    // The child was created with CREATE_NEW_PROCESS_GROUP, so pid == group id.
    // Failure is silently ignored — this is a best-effort graceful stop.
    unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
}

// --- Stdin transport helpers ---

/// Derive a named pipe path from the session directory.
/// Uses first 16 hex chars of SHA256 to stay under the 256-char pipe name limit.
fn stdin_pipe_name(session_dir: &Path) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(session_dir.as_os_str().as_encoded_bytes());
    let hex: String = hash[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!(r"\\.\pipe\tendr-stdin-{hex}")
}

/// Create a named pipe server for inbound byte-mode reads.
fn create_named_pipe_server(name: &str) -> io::Result<OwnedHandle> {
    use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_INBOUND;
    use windows_sys::Win32::System::Pipes::{CreateNamedPipeW, PIPE_TYPE_BYTE, PIPE_WAIT};

    let wide_name: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();

    let handle = unsafe {
        CreateNamedPipeW(
            wide_name.as_ptr(),
            PIPE_ACCESS_INBOUND,
            PIPE_TYPE_BYTE | PIPE_WAIT,
            1,    // max instances
            0,    // out buffer
            8192, // in buffer
            0,    // default timeout
            std::ptr::null(),
        )
    };

    if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }

    Ok(unsafe { OwnedHandle::from_raw_handle(handle as *mut _) })
}

/// Non-owning reader for a named pipe handle.
/// Maps ERROR_BROKEN_PIPE to EOF (Ok(0)) so the forwarding loop
/// treats client disconnect as a clean end-of-input.
struct PipeReader {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

// SAFETY: The handle is a kernel object, safe to send.
unsafe impl Send for PipeReader {}

impl io::Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use windows_sys::Win32::Storage::FileSystem::ReadFile;

        let mut bytes_read: u32 = 0;
        let ret = unsafe {
            ReadFile(
                self.handle,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
                &mut bytes_read,
                std::ptr::null_mut(),
            )
        };
        if ret == 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE as i32)
            {
                return Ok(0); // EOF — client disconnected
            }
            return Err(err);
        }
        Ok(bytes_read as usize)
    }
}

// --- Process identity implementation ---

/// Get ProcessIdentity for a process by PID.
///
/// Uses `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` + `GetProcessTimes`.
/// `PROCESS_QUERY_LIMITED_INFORMATION` (0x1000) is the minimum access right
/// needed for `GetProcessTimes` — it works even for processes running as
/// other users, unlike `PROCESS_QUERY_INFORMATION` which requires elevated
/// privileges. This matches the Unix approach where `/proc/pid/stat` is
/// world-readable.
///
/// Error mapping:
/// - `ERROR_INVALID_PARAMETER` (87): PID does not exist
/// - `ERROR_ACCESS_DENIED` (5): process exists but we can't query it
///   (e.g., protected process, system process)
fn process_identity(pid: u32) -> io::Result<ProcessIdentity> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let pid_nz = NonZeroU32::new(pid).ok_or_else(|| io::Error::other("pid is zero"))?;

    let is_self = pid == unsafe { GetCurrentProcessId() };
    let handle = if is_self {
        // GetCurrentProcess returns a pseudo-handle (-1) that is always valid
        // for the current process and does not need CloseHandle.
        unsafe { GetCurrentProcess() }
    } else {
        // PROCESS_QUERY_LIMITED_INFORMATION is the minimum right for GetProcessTimes.
        // bInheritHandle = FALSE (0) — we don't need child inheritance.
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if h.is_null() {
            return Err(io::Error::last_os_error());
        }
        h
    };

    let start_time_ns = creation_time_ns(handle);

    // Close handle unless it's the pseudo-handle for current process
    if !is_self {
        unsafe { CloseHandle(handle) };
    }

    Ok(ProcessIdentity {
        pid: pid_nz,
        start_time_ns: start_time_ns?,
    })
}

/// When the process `handle` refers to was created, in nanoseconds since
/// the Unix epoch: the start time of its [`ProcessIdentity`].
fn creation_time_ns(handle: windows_sys::Win32::Foundation::HANDLE) -> io::Result<u64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;

    let mut creation = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exit = creation;
    let mut kernel = creation;
    let mut user = creation;

    // SAFETY: the handle is valid with at least
    // PROCESS_QUERY_LIMITED_INFORMATION; the out-pointers are valid.
    let ret = unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
    if ret == 0 {
        return Err(io::Error::last_os_error());
    }

    // Convert FILETIME (100ns intervals since 1601-01-01) to nanoseconds since Unix epoch.
    // Epoch offset: 11,644,473,600 seconds = 116,444,736,000,000,000 100ns intervals.
    let ticks_100ns = (creation.dwHighDateTime as u64) << 32 | creation.dwLowDateTime as u64;
    let unix_ticks = ticks_100ns.saturating_sub(116_444_736_000_000_000);
    Ok(unix_ticks * 100) // 100ns → ns
}

/// Probe a process by identity on Windows.
///
/// Error classification from `OpenProcess`:
///
/// | Win32 error | Code | Meaning | Maps to |
/// |---|---|---|---|
/// | `ERROR_INVALID_PARAMETER` | 87 | PID does not exist (never existed or fully reaped) | `Missing` |
/// | `ERROR_ACCESS_DENIED` | 5 | Process exists but we can't query (protected/system) | `Inaccessible` |
/// | Other | — | Unexpected failure | `OsError` |
///
/// Note: Unlike Unix where PIDs are recycled immediately after exit,
/// Windows keeps the process object alive as long as any handle is open.
/// So `ERROR_INVALID_PARAMETER` reliably means "no process with this PID"
/// rather than "PID was recycled." The identity check (creation time)
/// catches the recycled-PID case when `OpenProcess` succeeds. Because the
/// object outlives the process, a matching identity is also checked for
/// exit: an exited process whose object is still held open (by us, a
/// console host, a scanner) is `Missing`, as an unreaped zombie is on macOS.
/// (On Linux a zombie's `/proc/<pid>/stat` stays readable, so it reads
/// `AliveVerified` until it is reaped.)
fn process_status(id: &ProcessIdentity) -> ProcessStatus {
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER};

    match process_identity(id.pid.get()) {
        Ok(current) => {
            if current != *id {
                ProcessStatus::IdentityMismatch
            } else if has_exited(id.pid.get()) {
                ProcessStatus::Missing
            } else {
                ProcessStatus::AliveVerified
            }
        }
        Err(e) => match e.raw_os_error() {
            Some(code) if code == ERROR_INVALID_PARAMETER as i32 => ProcessStatus::Missing,
            Some(code) if code == ERROR_ACCESS_DENIED as i32 => ProcessStatus::Inaccessible,
            _ => ProcessStatus::OsError(e.kind()),
        },
    }
}

// --- Windows PowerShell module path ---

const PS_MODULE_PATH: &str = "PSModulePath";

/// The `PSModulePath` to give a Windows PowerShell (`powershell.exe`, 5.1)
/// child: tendr's own value without PowerShell 7's module directories.
///
/// pwsh 7 puts its own directories on `PSModulePath` (`$PSHOME\Modules`,
/// `Documents\PowerShell\Modules`, `Program Files\PowerShell\Modules`) and
/// every process it starts inherits them. Windows PowerShell finds pwsh 7's
/// modules there before its own and loads them. On windows-11-arm, pwsh 7's
/// PSReadLine then usually fails to load at startup and the console
/// swallows the first line of stdin unrun, so a session's first `exec`
/// never finishes. pwsh 7 removes these directories when it starts
/// `powershell.exe` itself (`ModuleIntrinsics.GetWindowsPowerShellModulePath`),
/// but a `powershell.exe` started by tendr is not its direct child, so tendr
/// applies pwsh's rule.
///
/// `None` leaves the inherited value alone: the child is not Windows
/// PowerShell, the launch spec sets or unsets `PSModulePath` itself, tendr
/// has none, or it holds no pwsh 7 directory.
fn windows_powershell_module_path(
    program: &str,
    env_remove: &BTreeSet<String>,
    env: &BTreeMap<String, String>,
) -> Option<String> {
    if !is_windows_powershell(program) {
        return None;
    }
    let is_module_path = |name: &String| name.eq_ignore_ascii_case(PS_MODULE_PATH);
    if env.keys().any(is_module_path) || env_remove.iter().any(is_module_path) {
        return None;
    }
    let current = std::env::var(PS_MODULE_PATH).ok()?;
    let pwsh_dirs: Vec<PathBuf> = [
        known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_Documents),
        known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_ProgramFiles),
    ]
    .into_iter()
    .flatten()
    .map(|dir| dir.join("PowerShell").join("Modules"))
    .collect();
    strip_pwsh_module_dirs(&current, &pwsh_dirs, |dir| dir.join("pwsh.dll").is_file())
}

/// Whether `program` (a session's `argv[0]`) starts Windows PowerShell:
/// `powershell` or `powershell.exe`, bare or with a directory, in any case.
fn is_windows_powershell(program: &str) -> bool {
    let name = program.rsplit(['\\', '/']).next().unwrap_or(program);
    name.eq_ignore_ascii_case("powershell") || name.eq_ignore_ascii_case("powershell.exe")
}

/// `module_path` without pwsh 7's entries, by pwsh's own rule: an entry that
/// is one of `pwsh_dirs` (compared without case or trailing separators), or
/// whose parent directory is a pwsh installation (`is_pwsh_home`), is
/// dropped. The rest keep their order; entries are trimmed and empty ones
/// dropped. `None` when no entry is pwsh 7's.
fn strip_pwsh_module_dirs(
    module_path: &str,
    pwsh_dirs: &[PathBuf],
    is_pwsh_home: impl Fn(&Path) -> bool,
) -> Option<String> {
    let key = |path: &str| path.trim_end_matches(['\\', '/']).to_lowercase();
    let pwsh_keys: Vec<String> = pwsh_dirs
        .iter()
        .filter_map(|dir| dir.to_str())
        .map(key)
        .collect();
    let is_pwsh_entry = |entry: &str| {
        pwsh_keys.contains(&key(entry))
            || Path::new(entry)
                .parent()
                .is_some_and(|dir| !dir.as_os_str().is_empty() && is_pwsh_home(dir))
    };
    let entries: Vec<&str> = module_path
        .split(';')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .collect();
    let kept: Vec<&str> = entries
        .iter()
        .copied()
        .filter(|entry| !is_pwsh_entry(entry))
        .collect();
    (kept.len() < entries.len()).then(|| kept.join(";"))
}

/// A known folder's path for the current user (`SHGetKnownFolderPath`), as
/// recorded and without checking that it exists (pwsh's `DoNotVerify`), or
/// `None` if the shell cannot resolve it.
fn known_folder(id: &windows_sys::core::GUID) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{KF_FLAG_DONT_VERIFY, SHGetKnownFolderPath};

    let mut raw: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: `id` points to a valid GUID; KF_FLAG_DONT_VERIFY and a null
    // token ask for the current user's folder without touching it (so an
    // offline redirected Documents cannot stall a spawn); `raw` is a valid
    // out-pointer.
    let hr = unsafe {
        SHGetKnownFolderPath(
            id,
            KF_FLAG_DONT_VERIFY as u32,
            std::ptr::null_mut(),
            &mut raw,
        )
    };
    let path = (hr >= 0 && !raw.is_null()).then(|| {
        // SAFETY: on success `raw` is a NUL-terminated UTF-16 string.
        let len = (0..).take_while(|&i| unsafe { *raw.add(i) } != 0).count();
        // SAFETY: the `len` units before the NUL were just read.
        let wide = unsafe { std::slice::from_raw_parts(raw, len) };
        PathBuf::from(std::ffi::OsString::from_wide(wide))
    });
    // SAFETY: the buffer is CoTaskMem-allocated and must be freed whether or
    // not the call succeeded; CoTaskMemFree accepts null.
    unsafe { CoTaskMemFree(raw.cast_const().cast()) };
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_powershell_is_recognised_by_file_name() {
        for program in [
            "powershell",
            "PowerShell.EXE",
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
            "C:/Windows/SysWOW64/WindowsPowerShell/v1.0/powershell",
        ] {
            assert!(is_windows_powershell(program), "{program}");
        }
        for program in [
            "pwsh",
            r"C:\Program Files\PowerShell\7\pwsh.exe",
            "powershell_ise.exe",
            r"C:\powershell\cmd.exe",
            "powershell.cmd",
        ] {
            assert!(!is_windows_powershell(program), "{program}");
        }
    }

    /// The value pwsh 7.6 hands its children on a GitHub windows runner, and
    /// what Windows PowerShell must see instead.
    #[test]
    fn pwsh_module_dirs_are_removed_and_the_rest_kept_in_order() {
        let inherited = r"C:\Users\u\Documents\PowerShell\Modules;C:\Program Files\PowerShell\Modules;c:\program files\powershell\7\Modules;C:\\Modules\az_15.6.1;C:\Users\packer\Documents\WindowsPowerShell\Modules;C:\Program Files\WindowsPowerShell\Modules;C:\Windows\system32\WindowsPowerShell\v1.0\Modules;C:\Program Files\Microsoft SQL Server\130\Tools\PowerShell\Modules\";
        let pwsh_dirs = [
            PathBuf::from(r"C:\Users\u\Documents\PowerShell\Modules"),
            PathBuf::from(r"C:\Program Files\PowerShell\Modules"),
        ];
        let is_pwsh_home = |dir: &Path| {
            dir.as_os_str()
                .eq_ignore_ascii_case(r"c:\program files\powershell\7")
        };
        assert_eq!(
            strip_pwsh_module_dirs(inherited, &pwsh_dirs, is_pwsh_home).as_deref(),
            Some(
                r"C:\\Modules\az_15.6.1;C:\Users\packer\Documents\WindowsPowerShell\Modules;C:\Program Files\WindowsPowerShell\Modules;C:\Windows\system32\WindowsPowerShell\v1.0\Modules;C:\Program Files\Microsoft SQL Server\130\Tools\PowerShell\Modules\"
            )
        );
    }

    #[test]
    fn pwsh_dirs_match_without_case_or_trailing_separator() {
        let pwsh_dirs = [PathBuf::from(r"C:\Users\Ünï\Documents\PowerShell\Modules")];
        assert_eq!(
            strip_pwsh_module_dirs(
                r" c:\users\üNÏ\documents\powershell\modules\ ;;D:\mods",
                &pwsh_dirs,
                |_| false
            )
            .as_deref(),
            Some(r"D:\mods")
        );
    }

    #[test]
    fn a_drive_root_entry_is_kept() {
        assert_eq!(
            strip_pwsh_module_dirs(r"C:\;D:\pwsh\Modules;E:\", &[], |_| true).as_deref(),
            Some(r"C:\;E:\")
        );
    }

    #[test]
    fn a_value_without_pwsh_dirs_is_left_alone() {
        let windows_powershell = r"C:\Users\u\Documents\WindowsPowerShell\Modules;;C:\Program Files\WindowsPowerShell\Modules;C:\Windows\system32\WindowsPowerShell\v1.0\Modules";
        assert_eq!(
            strip_pwsh_module_dirs(windows_powershell, &[], |_| false),
            None
        );
    }

    /// An explicit `--env` or `--unset-env` for PSModulePath, in any case,
    /// wins; so does a child that is not Windows PowerShell.
    #[test]
    fn the_launch_spec_and_other_programs_are_left_alone() {
        let set = BTreeMap::from([("psmodulepath".to_owned(), r"D:\mine".to_owned())]);
        let unset = BTreeSet::from(["PSMODULEPATH".to_owned()]);
        assert_eq!(
            windows_powershell_module_path("powershell", &BTreeSet::new(), &set),
            None
        );
        assert_eq!(
            windows_powershell_module_path("powershell", &unset, &BTreeMap::new()),
            None
        );
        assert_eq!(
            windows_powershell_module_path("pwsh", &BTreeSet::new(), &BTreeMap::new()),
            None
        );
    }

    #[test]
    fn known_folders_resolve() {
        let docs = known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_Documents)
            .expect("the Documents folder");
        assert!(docs.is_absolute(), "{}", docs.display());
        let program_files = known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_ProgramFiles)
            .expect("the Program Files folder");
        assert_eq!(
            Some(program_files.as_os_str()),
            std::env::var_os("ProgramFiles").as_deref()
        );
    }
}
