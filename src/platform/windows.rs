//! Windows implementation of the Platform trait.
//!
//! All Platform methods are implemented: process lifecycle (spawn, kill,
//! identity, status), sidecar spawn with STARTUPINFOEXW handle whitelisting,
//! readiness channel (CreatePipe), and stdin transport (named pipes).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::num::NonZeroU32;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::sync::Arc;

use crate::model::ids::ProcessIdentity;
use crate::platform::{Platform, ProcessStatus};

/// Windows implementation of the Platform trait.
pub struct WindowsPlatform;

/// Opaque supervised-child state for Windows.
/// Wraps a `std::process::Child` with its verified `ProcessIdentity`
/// and an Arc'd Job Object for tree kill.
pub struct SupervisedChild {
    child: std::process::Child,
    identity: ProcessIdentity,
    job_object: Arc<OwnedHandle>,
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

    fn spawn_sidecar(tendr_bin: &Path, session_dir: &Path, ready_writer: &File) -> io::Result<u32> {
        // Raw CreateProcessW with STARTUPINFOEXW + PROC_THREAD_ATTRIBUTE_HANDLE_LIST.
        // This is the only way to inherit exactly one handle (the ready pipe)
        // without leaking every other inheritable handle from the parent.
        spawn_sidecar_raw(tendr_bin, session_dir, ready_writer)
    }

    fn ready_channel() -> io::Result<(File, File)> {
        use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
        use windows_sys::Win32::System::Pipes::CreatePipe;

        let mut sa: SECURITY_ATTRIBUTES = unsafe { std::mem::zeroed() };
        sa.nLength = std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32;
        sa.bInheritHandle = 1; // TRUE — both handles inheritable initially

        let mut read_handle = std::ptr::null_mut();
        let mut write_handle = std::ptr::null_mut();

        // SAFETY: sa is valid, pointers are valid out params.
        let ret = unsafe { CreatePipe(&mut read_handle, &mut write_handle, &sa, 0) };
        if ret == 0 {
            return Err(io::Error::last_os_error());
        }

        // Make read end non-inheritable — only the parent reads.
        set_handle_inheritable(read_handle, false)?;

        // SAFETY: both handles are valid from CreatePipe.
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
        env: &BTreeMap<String, String>,
    ) -> io::Result<SupervisedChild> {
        use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;

        let mut cmd = Command::new(&argv[0]);
        if argv.len() > 1 {
            cmd.args(&argv[1..]);
        }
        if stdin_piped {
            cmd.stdin(std::process::Stdio::piped());
        } else {
            cmd.stdin(std::process::Stdio::null());
        }
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        if !env.is_empty() {
            cmd.envs(env);
        }

        // CREATE_NEW_PROCESS_GROUP: required for GenerateConsoleCtrlEvent targeting.
        // No CREATE_SUSPENDED — std::process::Child doesn't expose the thread
        // handle needed for ResumeThread, so the child can run briefly before
        // the Job Object assignment below. Accepted for now; the fix (raw
        // CreateProcessW, CREATE_SUSPENDED) is in
        // docs/plans/backlog/windows-parity.md, "Windows child containment".
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
        let mut child = cmd.spawn()?;

        let job = create_job_object()?;

        // Assign child to Job Object immediately after spawn.
        // Race window: the child may briefly run before assignment (see above).
        if let Err(e) = assign_process_to_job(
            job.as_raw_handle() as *mut _,
            child.as_raw_handle() as *mut _,
        ) {
            // Kill the child so it doesn't run unsupervised outside the job.
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }

        let pid = child.id();
        let identity = process_identity(pid)?;

        Ok(SupervisedChild {
            child,
            identity,
            job_object: Arc::new(job),
        })
    }

    fn spawn_child_pty(
        _argv: &[String],
        _cwd: Option<&Path>,
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
        child.child.wait()
    }

    fn child_try_wait(child: &mut SupervisedChild) -> io::Result<Option<ExitStatus>> {
        child.child.try_wait()
    }

    fn child_stdout(child: &mut SupervisedChild) -> Option<Box<dyn io::Read + Send>> {
        child
            .child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn io::Read + Send>)
    }

    fn child_stderr(child: &mut SupervisedChild) -> Option<Box<dyn io::Read + Send>> {
        child
            .child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn io::Read + Send>)
    }

    fn child_stdin(child: &mut SupervisedChild) -> Option<Box<dyn io::Write + Send>> {
        child
            .child
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

        // Graceful: best-effort CTRL_BREAK, then poll via WaitForSingleObject,
        // then escalate. Uses a real waitable handle instead of process_status
        // because Windows keeps the process object alive while any handle is
        // open (e.g. SupervisedChild's std::process::Child), making PID-based
        // liveness checks unreliable.
        send_ctrl_break(handle.identity.pid.get());

        if wait_for_process_exit(handle.identity.pid.get(), 5000) {
            return Ok(());
        }

        // Grace period expired — escalate to force.
        terminate_job(&handle.job_object)
    }

    fn kill_orphan(id: &ProcessIdentity, _force: bool) -> io::Result<()> {
        // Without a Job Object handle (sidecar crashed or CLI-initiated kill),
        // we can only kill the individual process by PID — no descendant tree
        // kill. This is a known degradation on Windows; orphan kill is a
        // recovery path, not the normal lifecycle.
        //
        // force=false degrades to force on Windows because there is no
        // reliable way to send a graceful stop signal to an orphaned process
        // without a shared console (which we don't have for orphans).
        match process_status(id) {
            ProcessStatus::Missing => return Ok(()),
            ProcessStatus::IdentityMismatch => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "PID was recycled — refusing to kill wrong process",
                ));
            }
            ProcessStatus::OsError(kind) => {
                return Err(io::Error::new(kind, "failed to probe process status"));
            }
            ProcessStatus::AliveVerified | ProcessStatus::Inaccessible => {}
        }

        terminate_process_by_pid(id.pid.get())
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
        // SAFETY: handle was inherited from the parent via CreatePipe with
        // bInheritHandle=TRUE. from_raw_handle takes ownership.
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

// --- Sidecar spawn (raw CreateProcessW) ---

/// Whether the calling process's job (if any) permits child breakaway.
///
/// Returns true when:
/// - The calling process is not in a job at all (breakaway flag is a no-op).
/// - The calling process is in a job whose limits include
///   `JOB_OBJECT_LIMIT_BREAKAWAY_OK`.
///
/// Returns false when the parent's job exists but forbids breakaway, or when
/// the kernel queries fail. False is the safe default: the sidecar will
/// inherit the parent's job (and lifetime) — a degradation, but better than
/// failing the spawn outright with ERROR_ACCESS_DENIED.
fn parent_job_allows_breakaway() -> bool {
    use windows_sys::Win32::System::JobObjects::{
        IsProcessInJob, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, QueryInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    // BOOL is defined as i32 in windows-sys.
    let mut in_job: i32 = 0;
    // SAFETY: GetCurrentProcess is a pseudo-handle (always valid); second
    // arg null = "the job containing the calling process"; out param points
    // to a valid stack i32.
    let ok = unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job) };
    if ok == 0 {
        return false;
    }
    if in_job == 0 {
        return true; // not in any job — breakaway flag is a no-op, harmless
    }

    // SAFETY: zeroed JOBOBJECT_EXTENDED_LIMIT_INFORMATION is the documented
    // initialization pattern before passing as an out-buffer.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    let mut returned: u32 = 0;
    // SAFETY: null hJob = "the job associated with the calling process";
    // the info pointer + advertised size match the JobObjectExtendedLimitInformation
    // class; the returned-bytes out param is a valid stack u32.
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
        return false;
    }
    (info.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_BREAKAWAY_OK) != 0
}

/// Spawn the sidecar with explicit handle whitelisting.
///
/// Uses STARTUPINFOEXW + PROC_THREAD_ATTRIBUTE_HANDLE_LIST so the sidecar
/// inherits ONLY the ready pipe write handle. Without this, CreateProcessW
/// with bInheritHandles=TRUE leaks every inheritable handle from the parent
/// (stdout/stderr pipes from test harnesses, SSH sessions, etc.).
///
/// Requires Windows 7+ (PROC_THREAD_ATTRIBUTE_HANDLE_LIST support).
fn spawn_sidecar_raw(tendr_bin: &Path, session_dir: &Path, ready_writer: &File) -> io::Result<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Threading::{
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_UNICODE_ENVIRONMENT,
        CreateProcessW, DETACHED_PROCESS, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, InitializeProcThreadAttributeList,
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, STARTUPINFOEXW,
        UpdateProcThreadAttribute,
    };

    let ready_handle: HANDLE = ready_writer.as_raw_handle() as *mut _;

    // --- Build command line ---
    // CreateProcessW wants a single mutable wide string for the command line.
    let cmdline = format!(
        "\"{}\" _sidecar --session-dir \"{}\"",
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
    // CREATE_BREAKAWAY_FROM_JOB lets the sidecar escape any job object the
    // parent (e.g. OpenSSH-Server's per-session job with KILL_ON_JOB_CLOSE)
    // assigned us to. Without it, DETACHED_PROCESS only severs the *console*,
    // not the *job* — and the sidecar dies on parent disconnect.
    //
    // Detect-then-spawn: ask the kernel whether the parent job allows
    // breakaway. If we set CREATE_BREAKAWAY_FROM_JOB on a job that doesn't
    // permit it, CreateProcessW fails with ERROR_ACCESS_DENIED — and we'd
    // have to retry with a buffer that CreateProcessW may have mutated
    // in-place. Detecting upfront avoids that.
    let mut creation_flags = CREATE_NEW_PROCESS_GROUP
        | DETACHED_PROCESS
        | EXTENDED_STARTUPINFO_PRESENT
        | CREATE_UNICODE_ENVIRONMENT;
    if parent_job_allows_breakaway() {
        creation_flags |= CREATE_BREAKAWAY_FROM_JOB;
    }

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    let ret = unsafe {
        CreateProcessW(
            std::ptr::null(),          // lpApplicationName (use cmdline)
            cmdline_wide.as_mut_ptr(), // lpCommandLine
            std::ptr::null(),          // lpProcessAttributes
            std::ptr::null(),          // lpThreadAttributes
            1,                         // bInheritHandles = TRUE
            creation_flags,            // dwCreationFlags
            env_block.as_ptr().cast(), // lpEnvironment
            std::ptr::null(),          // lpCurrentDirectory (inherit)
            &si_ex.StartupInfo,        // lpStartupInfo
            &mut pi,                   // lpProcessInformation
        )
    };

    unsafe { DeleteProcThreadAttributeList(attr_list) };

    if ret == 0 {
        return Err(io::Error::last_os_error());
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

/// Create a Job Object with KILL_ON_JOB_CLOSE (safety net for crashes).
fn create_job_object() -> io::Result<OwnedHandle> {
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };

    // SAFETY: null name = anonymous job object. Returns null on failure.
    let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }

    // Configure kill-on-close before returning, while we still have the raw handle.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

    let ret = unsafe {
        SetInformationJobObject(
            handle,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ret == 0 {
        // Close the job handle before returning the error.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
        return Err(io::Error::last_os_error());
    }

    // SAFETY: handle is a valid non-null HANDLE from CreateJobObjectW.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle as *mut _) })
}

/// Assign a process to a Job Object.
fn assign_process_to_job(
    job: windows_sys::Win32::Foundation::HANDLE,
    process: windows_sys::Win32::Foundation::HANDLE,
) -> io::Result<()> {
    use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

    // SAFETY: both handles are valid — job from create_job_object,
    // process from std::process::Child (which owns the process HANDLE).
    let ret = unsafe { AssignProcessToJobObject(job, process) };
    if ret == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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

/// Terminate a single process by PID. No tree kill — use only for orphans.
fn terminate_process_by_pid(pid: u32) -> io::Result<()> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if handle.is_null() {
        // Can't open — process likely already exited.
        return Ok(());
    }

    let ret = unsafe { TerminateProcess(handle, 1) };
    unsafe { CloseHandle(handle) };

    if ret == 0 {
        let err = io::Error::last_os_error();
        // Access denied may mean the process already exited between
        // OpenProcess and TerminateProcess — treat as success.
        if err.raw_os_error() != Some(windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED as i32) {
            return Err(err);
        }
    }
    Ok(())
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
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, GetProcessTimes, OpenProcess,
        PROCESS_QUERY_LIMITED_INFORMATION,
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

    let mut creation = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exit = creation;
    let mut kernel = creation;
    let mut user = creation;

    let ret = unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };

    // Close handle unless it's the pseudo-handle for current process
    if !is_self {
        unsafe { CloseHandle(handle) };
    }

    if ret == 0 {
        return Err(io::Error::last_os_error());
    }

    // Convert FILETIME (100ns intervals since 1601-01-01) to nanoseconds since Unix epoch.
    // Epoch offset: 11,644,473,600 seconds = 116,444,736,000,000,000 100ns intervals.
    let ticks_100ns = (creation.dwHighDateTime as u64) << 32 | creation.dwLowDateTime as u64;
    let unix_ticks = ticks_100ns.saturating_sub(116_444_736_000_000_000);
    let start_time_ns = unix_ticks * 100; // 100ns → ns

    Ok(ProcessIdentity {
        pid: pid_nz,
        start_time_ns,
    })
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
