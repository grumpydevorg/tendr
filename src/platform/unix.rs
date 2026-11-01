use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read, Write};
use std::num::NonZeroU32;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitStatus};

use rustix::fs::{self as rfs, Mode, OFlags};

use crate::model::ids::ProcessIdentity;
use crate::platform::{Platform, ProcessStatus};

/// Unix implementation of the Platform trait.
pub struct UnixPlatform;

/// Opaque supervised-child state for Unix.
/// Wraps a `std::process::Child` with its verified `ProcessIdentity`.
pub struct SupervisedChild {
    child: std::process::Child,
    identity: ProcessIdentity,
    /// Whether this child was spawned under a PTY.
    is_pty: bool,
    /// PTY output reader (master read half, holding the slave until drained).
    /// None for pipe sessions.
    pty_reader: Option<PtyReader>,
    /// The reader's hold on the PTY slave, which a stop releases. None for
    /// pipe sessions.
    pty_hold: Option<SlaveHold>,
    /// PTY master write half. None for pipe sessions.
    pty_master_write: Option<File>,
}

impl SupervisedChild {
    /// Take the PTY master's write half for the sidecar's single input writer.
    /// `None` for pipe sessions or once taken.
    pub fn take_pty_writer(&mut self) -> Option<PtyWriter> {
        if self.is_pty {
            self.pty_master_write.take().map(PtyWriter::new)
        } else {
            None
        }
    }
}

/// Lightweight kill handle for Unix. Carries the ProcessIdentity
/// needed for identity-verified group kill, and for a child that leads a PTY
/// session, whose graceful stop starts with a hangup, the sidecar's hold on
/// its terminal, which a stop releases. Send + Clone so it can be moved to a
/// timeout thread.
#[derive(Clone)]
pub struct ChildKillHandle {
    identity: ProcessIdentity,
    pty_hold: Option<SlaveHold>,
}

impl Platform for UnixPlatform {
    type SupervisedChild = SupervisedChild;
    type ChildKillHandle = ChildKillHandle;
    type StdinTransport = ();
    type ReadyReader = File;
    type ReadyWriter = File;

    fn spawn_sidecar(
        tendr_bin: &Path,
        session_dir: &Path,
        ready_write_fd: &File,
    ) -> io::Result<u32> {
        spawn_sidecar(tendr_bin, session_dir, ready_write_fd)
    }

    fn ready_channel() -> io::Result<(File, File)> {
        pipe()
    }

    fn read_ready_signal(reader: File) -> io::Result<String> {
        read_ready_signal(reader)
    }

    fn write_ready_signal(writer: File, message: &str) -> io::Result<()> {
        write_ready_signal_file(writer, message)
    }

    fn spawn_child(
        argv: &[String],
        stdin_piped: bool,
        cwd: Option<&Path>,
        env_remove: &BTreeSet<String>,
        env: &BTreeMap<String, String>,
    ) -> io::Result<SupervisedChild> {
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
        for name in env_remove {
            cmd.env_remove(name);
        }
        if !env.is_empty() {
            cmd.envs(env);
        }

        // Make child its own process group leader so kill(-pgid) kills the whole tree.
        // SAFETY: pre_exec runs after fork() in the child process, before exec().
        // setpgid(0,0) is async-signal-safe and only affects the forked child.
        // No shared mutable state is accessed in the closure.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let child = cmd.spawn()?;
        let pid = child.id();
        // A child that exits between spawn() and identity capture is a
        // successful short-lived run, not a spawn failure -- capture_spawned_identity
        // returns an already_exited placeholder rather than erroring.
        let identity = capture_spawned_identity(pid)?;

        Ok(SupervisedChild {
            child,
            identity,
            is_pty: false,
            pty_reader: None,
            pty_hold: None,
            pty_master_write: None,
        })
    }

    fn spawn_child_pty(
        argv: &[String],
        cwd: Option<&Path>,
        env_remove: &BTreeSet<String>,
        env: &BTreeMap<String, String>,
    ) -> io::Result<SupervisedChild> {
        // 1. Create PTY pair at the initial size (a zero size breaks TUIs and
        // cannot be recorded).
        let mut master_fd: libc::c_int = 0;
        let mut slave_fd: libc::c_int = 0;
        let mut size = libc::winsize {
            ws_row: super::INITIAL_PTY_ROWS,
            ws_col: super::INITIAL_PTY_COLS,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: openpty writes valid fds into master_fd/slave_fd on success.
        // Null pointers for name/termios are explicitly allowed; `size` is a
        // valid winsize that outlives the call. A raw pointer fits both the
        // const (glibc) and mut (BSD) declarations.
        let ret = unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut size,
            )
        };
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: openpty returned 0, so master_fd and slave_fd are valid open fds.
        let master = unsafe { OwnedFd::from_raw_fd(master_fd) };
        let slave = unsafe { OwnedFd::from_raw_fd(slave_fd) };
        // openpty sets neither descriptor close-on-exec. The master is the
        // sidecar's alone; the sidecar keeps this slave descriptor until it has
        // read what the child left (see `PtyReader`). No process the sidecar
        // spawns may inherit either: the child's own slave copies are dup2'd
        // below and do not carry the flag.
        rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC)?;
        rustix::io::fcntl_setfd(&slave, rustix::io::FdFlags::CLOEXEC)?;
        // Watches for the child's exit before the child exists, so a failure to
        // start the thread cannot leave a spawned child behind.
        let (exit_watch, exited) = ExitWatch::start()?;

        let mut cmd = Command::new(&argv[0]);
        if argv.len() > 1 {
            cmd.args(&argv[1..]);
        }

        let slave_raw = slave.as_raw_fd();
        // SAFETY: pre_exec runs after fork() in the child process, before exec().
        // All called functions (setsid, ioctl, dup2, close, setpgid) are
        // async-signal-safe. slave_raw is a Copy integer captured by value.
        unsafe {
            cmd.pre_exec(move || {
                // Create new session so child becomes session leader
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                // Set the slave as the controlling terminal
                #[cfg(target_os = "macos")]
                {
                    if libc::ioctl(slave_raw, libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                }
                #[cfg(not(target_os = "macos"))]
                {
                    if libc::ioctl(slave_raw, libc::TIOCSCTTY, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                }
                // Wire slave to stdin/stdout/stderr
                if libc::dup2(slave_raw, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::dup2(slave_raw, 1) == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::dup2(slave_raw, 2) == -1 {
                    return Err(io::Error::last_os_error());
                }
                if slave_raw > 2 {
                    libc::close(slave_raw);
                }
                // setsid() already created a new process group (PGID == PID).
                // Calling setpgid(0, 0) on a session leader returns EPERM,
                // so we skip it — tree kill via kill(-pgid) still works.
                Ok(())
            });
        }

        // Child inherits slave via pre_exec. Rust's side gets null stdio.
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        for name in env_remove {
            cmd.env_remove(name);
        }
        if !env.is_empty() {
            cmd.envs(env);
        }

        let child = cmd.spawn()?;
        exit_watch.watch(&child);

        // Nonblocking before duplicating: both halves share the description.
        set_nonblocking(&master)?;
        // Dup master into read and write halves
        let master_read = File::from(master.try_clone()?);
        let master_write = File::from(master);

        let pid = child.id();
        let identity = capture_spawned_identity(pid)?;

        let hold = SlaveHold::new(slave);
        Ok(SupervisedChild {
            child,
            identity,
            is_pty: true,
            pty_reader: Some(PtyReader::holding_slave(master_read, hold.clone(), exited)),
            pty_hold: Some(hold),
            pty_master_write: Some(master_write),
        })
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
        if child.is_pty {
            child.pty_reader.take().map(|r| {
                r.mark_running();
                Box::new(r) as Box<dyn io::Read + Send>
            })
        } else {
            child
                .child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn io::Read + Send>)
        }
    }

    fn child_stderr(child: &mut SupervisedChild) -> Option<Box<dyn io::Read + Send>> {
        if child.is_pty {
            None // Merged into PTY master
        } else {
            child
                .child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn io::Read + Send>)
        }
    }

    fn child_stdin(child: &mut SupervisedChild) -> Option<Box<dyn io::Write + Send>> {
        if child.is_pty {
            child
                .pty_master_write
                .take()
                .map(|f| Box::new(PtyWriter::new(f)) as Box<dyn io::Write + Send>)
        } else {
            child
                .child
                .stdin
                .take()
                .map(|s| Box::new(s) as Box<dyn io::Write + Send>)
        }
    }

    fn pty_resize_fd(child: &SupervisedChild) -> Option<File> {
        child
            .pty_master_write
            .as_ref()
            .and_then(|f| f.try_clone().ok())
    }

    fn child_kill_handle(child: &SupervisedChild) -> ChildKillHandle {
        ChildKillHandle {
            identity: child.identity,
            pty_hold: child.pty_hold.clone(),
        }
    }

    fn kill_child(handle: &ChildKillHandle, force: bool) -> io::Result<()> {
        // Before signalling, so the child's exit never waits on output nothing
        // will read: restart output stopped by ^S, and give the terminal up if
        // no reader is running (a failure path) or the stop is forced.
        if let Some(hold) = &handle.pty_hold {
            hold.release_for_stop(force);
        }
        kill_process(&handle.identity, force, handle.pty_hold.is_some())
    }

    fn kill_orphan(id: &ProcessIdentity, force: bool) -> io::Result<()> {
        // Without its sidecar an orphan's terminal is usually already closed,
        // so the kernel has delivered whatever hangup it was going to.
        kill_process(id, force, false)
    }

    fn self_identity() -> io::Result<ProcessIdentity> {
        process_identity(std::process::id())
    }

    fn process_identity(pid: u32) -> io::Result<ProcessIdentity> {
        process_identity(pid)
    }

    fn process_status(id: &ProcessIdentity) -> ProcessStatus {
        process_status(id)
    }

    fn create_stdin_transport(session_dir: &Path) -> io::Result<()> {
        let fifo_path = session_dir.join("stdin.pipe");
        mkfifo(&fifo_path)
    }

    fn accept_stdin_connection(
        _transport: &(),
        session_dir: &Path,
    ) -> Option<Box<dyn io::Read + Send>> {
        // On Unix, opening a FIFO for reading blocks until a writer connects.
        // Returns None if the FIFO has been removed (session cleanup).
        let fifo_path = session_dir.join("stdin.pipe");
        File::open(&fifo_path)
            .ok()
            .map(|f| Box::new(f) as Box<dyn io::Read + Send>)
    }

    fn open_stdin_writer(session_dir: &Path) -> io::Result<File> {
        let fifo_path = session_dir.join("stdin.pipe");
        open_fifo_write_nonblock(&fifo_path).map_err(|e| {
            // Map ENXIO (no reader connected) to ConnectionRefused so callers
            // don't need platform-specific imports to detect this condition.
            if e.raw_os_error() == Some(libc::ENXIO) {
                io::Error::new(io::ErrorKind::ConnectionRefused, e)
            } else {
                e
            }
        })
    }

    fn remove_stdin_transport(session_dir: &Path) {
        let _ = std::fs::remove_file(session_dir.join("stdin.pipe"));
    }

    fn ready_writer_from_env() -> io::Result<File> {
        let fd_str = std::env::var("TENDR_READY_FD")
            .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "TENDR_READY_FD not set"))?;
        let fd: RawFd = fd_str.parse().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "TENDR_READY_FD is not a valid fd",
            )
        })?;
        // SAFETY: fd is the TENDR_READY_FD passed by spawn_sidecar, which was a
        // valid open fd at exec time (close-on-exec was cleared in pre_exec).
        // from_raw_fd takes ownership — the fd is closed when the File is dropped.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn seal_ready_fd(writer: File) -> io::Result<File> {
        let fd = writer.as_raw_fd();
        // SAFETY: fd is a valid open file descriptor from the ready pipe.
        // F_SETFD with FD_CLOEXEC is a valid fcntl operation.
        // This prevents the child from inheriting the ready pipe.
        let ret = unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        if ret == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(writer)
    }
}

// --- Standalone functions (not trait methods) ---

/// Create a named pipe (FIFO) at `path` with mode 0600.
/// Note: rustix doesn't provide mkfifo on macOS, so this stays as raw libc.
fn mkfifo(path: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains null byte"))?;
    // SAFETY: c_path is a valid null-terminated C string (CString guarantees this).
    // 0o600 is a valid mode. mkfifo may fail but won't cause UB.
    let ret = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Create a pipe. Returns (read_fd, write_fd) as owned Files.
/// Both fds have close-on-exec set.
fn pipe() -> io::Result<(File, File)> {
    #[cfg(target_os = "linux")]
    let (read_fd, write_fd) = { rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)? };

    #[cfg(not(target_os = "linux"))]
    let (read_fd, write_fd) = {
        // macOS doesn't have pipe2 -- use pipe() + set CLOEXEC
        let (r, w) = rustix::pipe::pipe()?;
        rustix::io::fcntl_setfd(&r, rustix::io::FdFlags::CLOEXEC)?;
        rustix::io::fcntl_setfd(&w, rustix::io::FdFlags::CLOEXEC)?;
        (r, w)
    };

    Ok((File::from(read_fd), File::from(write_fd)))
}

/// Spawn the sidecar as a detached process.
/// Returns the sidecar's PID. The sidecar will signal readiness via ready_fd.
///
/// The sidecar inherits the write end of the readiness pipe and is
/// responsible for writing a result and closing it.
fn spawn_sidecar(tendr_bin: &Path, session_dir: &Path, ready_write_fd: &File) -> io::Result<u32> {
    let write_fd_raw = ready_write_fd.as_raw_fd();

    let mut cmd = Command::new(tendr_bin);
    cmd.arg("_sidecar")
        .arg("--session-dir")
        .arg(session_dir)
        .env("TENDR_READY_FD", write_fd_raw.to_string());

    // Redirect stdin/stdout/stderr to /dev/null for detachment
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    // Descriptors our caller leaked to us (exe.dev's init leaks /dev/ptmx
    // masters into every SSH command) must not reach the sidecar, nor through
    // it the child. Found before the fork, since listing allocates.
    let inherited = InheritedFds::find();

    // SAFETY: pre_exec runs after fork() in the child process, before exec().
    // The closure captures write_fd_raw (a Copy integer) and reads the
    // `inherited` set found before the fork; it does not allocate.
    // close_range, fcntl and setsid are async-signal-safe.
    // Every inherited fd above 2 is marked close-on-exec rather than closed,
    // so std's own exec-error pipe keeps working; then close-on-exec is
    // cleared on the ready fd so it survives exec. setsid detaches the
    // sidecar from the parent's session.
    unsafe {
        cmd.pre_exec(move || {
            if !cloexec_from_3() {
                inherited.mark_cloexec();
            }
            // Clear close-on-exec so the ready fd survives exec
            if libc::fcntl(write_fd_raw, libc::F_SETFD, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            // Detach into new session
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd.spawn()?;
    Ok(child.id())
}

/// The descriptors to mark close-on-exec where `close_range` cannot.
enum InheritedFds {
    /// The fds listed in `/dev/fd` before the fork.
    Listed(Vec<RawFd>),
    /// `/dev/fd` could not be read: every fd from 3 below this bound, the
    /// soft `RLIMIT_NOFILE` (capped, as it may be unlimited).
    Below(RawFd),
}

impl InheritedFds {
    /// Highest bound for the `Below` sweep: Linux's default `nr_open`.
    const SWEEP_CAP: RawFd = 1 << 20;

    fn find() -> Self {
        open_fds().map_or_else(|| Self::Below(fd_limit()), Self::Listed)
    }

    /// Mark each fd above 2 close-on-exec. Async-signal-safe: no allocation.
    fn mark_cloexec(&self) {
        let mark = |fd: RawFd| {
            // SAFETY: fcntl only changes a descriptor flag; EBADF (no such
            // fd, or one closed since the listing) is harmless.
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        };
        match self {
            Self::Listed(fds) => fds.iter().copied().filter(|&fd| fd > 2).for_each(mark),
            Self::Below(limit) => (3..*limit).for_each(mark),
        }
    }
}

/// The soft `RLIMIT_NOFILE`, capped at `InheritedFds::SWEEP_CAP`.
fn fd_limit() -> RawFd {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes into the rlimit we own.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return InheritedFds::SWEEP_CAP;
    }
    RawFd::try_from(lim.rlim_cur)
        .map_or(InheritedFds::SWEEP_CAP, |n| n.min(InheritedFds::SWEEP_CAP))
}

/// The calling process's open descriptors, from `/dev/fd`, or `None` if it
/// cannot be read.
fn open_fds() -> Option<Vec<RawFd>> {
    let dir = std::fs::read_dir("/dev/fd").ok()?;
    Some(
        dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
            .collect(),
    )
}

/// Mark every fd from 3 up close-on-exec with one `close_range` call.
/// Returns false where that is unavailable: not Linux, a kernel without
/// `CLOSE_RANGE_CLOEXEC` (added in 5.11), or a seccomp filter denying the
/// call. Async-signal-safe.
fn cloexec_from_3() -> bool {
    #[cfg(target_os = "linux")]
    {
        const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
        // SAFETY: close_range only changes descriptor flags in this process.
        let ret = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                3 as libc::c_uint,
                libc::c_uint::MAX,
                CLOSE_RANGE_CLOEXEC,
            )
        };
        ret == 0
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Read the readiness result from the pipe.
/// Returns Ok(message) on success, or Err if the pipe closed without a message
/// (sidecar died before signaling).
fn read_ready_signal(mut read_end: File) -> io::Result<String> {
    let mut buf = String::new();
    read_end.read_to_string(&mut buf)?;
    if buf.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "sidecar died without signaling readiness",
        ));
    }
    Ok(buf)
}

/// Open a FIFO for writing without blocking.
/// Returns ENXIO immediately if no reader is connected.
/// On success, clears O_NONBLOCK so subsequent writes block normally.
fn open_fifo_write_nonblock(path: &Path) -> io::Result<File> {
    let fd: OwnedFd = rfs::open(path, OFlags::WRONLY | OFlags::NONBLOCK, Mode::empty())?;

    let flags = rfs::fcntl_getfl(&fd)?;
    rfs::fcntl_setfl(&fd, flags & !OFlags::NONBLOCK)?;

    Ok(File::from(fd))
}

/// Write a readiness signal to the pipe via a File.
fn write_ready_signal_file(mut file: File, message: &str) -> io::Result<()> {
    file.write_all(message.as_bytes())?;
    // file is dropped here, closing the fd
    Ok(())
}

/// Write a readiness signal to the pipe via a raw fd number.
/// Used by the sidecar entry point which receives the fd from the environment.
pub fn write_ready_signal(fd_num: RawFd, message: &str) -> io::Result<()> {
    // SAFETY: fd_num is the TENDR_READY_FD passed by spawn_sidecar, which was a
    // valid open fd at exec time (close-on-exec was cleared in pre_exec).
    // from_raw_fd takes ownership -- the fd is closed when file is dropped at
    // the end of this function, ensuring exactly one close.
    let file = unsafe { File::from_raw_fd(fd_num) };
    write_ready_signal_file(file, message)
}

/// Get the ProcessIdentity of the current process.
/// Get the ProcessIdentity of a process by PID.
fn process_identity(pid: u32) -> io::Result<ProcessIdentity> {
    let pid = NonZeroU32::new(pid).ok_or_else(|| io::Error::other("pid is zero"))?;
    let start_time_ns = process_start_time(pid.get())?;
    Ok(ProcessIdentity { pid, start_time_ns })
}

/// True if the error means the target process no longer exists. Linux surfaces
/// a gone process as `ENOENT` (reading `/proc/<pid>/stat`); macOS as `ESRCH`.
fn is_process_gone(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ESRCH) | Some(libc::ENOENT))
}

/// Capture the identity of a just-spawned child. If the child has already
/// exited before its start time could be read (a fast-exit race: `spawn()`
/// succeeded but the process is gone by the time we probe), this is a
/// successful short-lived run — return an `already_exited` placeholder rather
/// than an error. Linux masks this race because a zombie's `/proc/<pid>/stat`
/// is still readable; macOS `proc_pidinfo` fails on the zombie, which is why
/// the collapse-into-SpawnFailed bug only surfaced there.
fn capture_spawned_identity(pid: u32) -> io::Result<ProcessIdentity> {
    match process_identity(pid) {
        Ok(id) => Ok(id),
        Err(e) if is_process_gone(&e) => {
            let pid = NonZeroU32::new(pid).ok_or_else(|| io::Error::other("pid is zero"))?;
            Ok(ProcessIdentity::already_exited(pid))
        }
        // A real, unexpected identity-capture failure (e.g. EPERM on a foreign
        // pid). Give it distinct context so it is not confused with a spawn()
        // failure upstream.
        Err(e) => Err(io::Error::new(
            e.kind(),
            format!("identity capture failed after spawn (pid {pid}): {e}"),
        )),
    }
}

/// Get process start time. Platform-specific.
#[cfg(target_os = "linux")]
fn process_start_time(pid: u32) -> io::Result<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    // Field 22 (1-indexed) is starttime in clock ticks.
    // Fields are space-separated, but field 2 (comm) can contain spaces and parens.
    // Find the last ')' to skip past comm.
    let after_comm = stat
        .rfind(')')
        .map(|i| &stat[i + 2..])
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed /proc/pid/stat"))?;
    // After comm, fields are: state(3), ppid(4), ..., starttime(22)
    // starttime is field 20 (0-indexed from after comm, since we skipped fields 1-2)
    let field = after_comm
        .split_whitespace()
        .nth(19) // 0-indexed: field 22 - field 3 = index 19
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "starttime field not found"))?;
    let ticks: u64 = field
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("bad starttime: {e}")))?;
    // Convert ticks to nanoseconds
    let ticks_per_sec = rustix::param::clock_ticks_per_second() as u64;
    Ok(ticks * (1_000_000_000 / ticks_per_sec))
}

#[cfg(target_os = "macos")]
fn process_start_time(pid: u32) -> io::Result<u64> {
    use std::mem;

    // SAFETY: proc_bsdinfo is a POD type (plain data, no pointers that need
    // initialization). Zeroing it produces a valid struct that proc_pidinfo
    // will overwrite with actual process data.
    let mut info: libc::proc_bsdinfo = unsafe { mem::zeroed() };
    let size = mem::size_of::<libc::proc_bsdinfo>() as i32;

    // SAFETY: info is a properly sized and aligned proc_bsdinfo buffer.
    // pid is cast to i32 which is safe because macOS PIDs fit in i32.
    // PROC_PIDTBSDINFO is the correct flavor for proc_bsdinfo.
    // size matches the buffer -- proc_pidinfo won't write out of bounds.
    let ret = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut _,
            size,
        )
    };

    if ret <= 0 {
        // proc_pidinfo does not guarantee a freshly-set errno when it returns
        // <= 0 (it can leave a stale errno from an earlier syscall — the source
        // of phantom ENOENT reports on a gone process). Reprobe with kill(pid, 0),
        // which sets errno reliably, to classify the failure deterministically.
        // SAFETY: kill with signal 0 performs existence/permission checks only
        // and sends no signal.
        let signalable = unsafe { libc::kill(pid as i32, 0) } == 0;
        let kill_err = io::Error::last_os_error();
        if signalable {
            // The process is signalable but has no readable bsdinfo. For a
            // same-uid process that means an unreaped zombie: it has exited.
            // (A live same-uid process — even in another session — has readable
            // bsdinfo on modern macOS.) Report it as gone.
            return Err(io::Error::from_raw_os_error(libc::ESRCH));
        }
        // kill failed: errno is now freshly ESRCH (gone) or EPERM (foreign,
        // inaccessible) — both meaningful for process_status.
        return Err(kill_err);
    }

    // pbi_start_tvsec and pbi_start_tvusec give the process start time
    let secs = info.pbi_start_tvsec;
    let usecs = info.pbi_start_tvusec;
    Ok(secs * 1_000_000_000 + usecs * 1_000)
}

/// Probe a process by identity. Returns a typed status instead of a boolean
/// so callers can make informed decisions (especially around EPERM).
fn process_status(id: &ProcessIdentity) -> ProcessStatus {
    match process_identity(id.pid.get()) {
        Ok(current) => {
            if current == *id {
                ProcessStatus::AliveVerified
            } else {
                ProcessStatus::IdentityMismatch
            }
        }
        Err(e) => match e.raw_os_error() {
            Some(libc::ESRCH) => ProcessStatus::Missing,
            // Linux reads /proc/<pid>/stat; a gone process yields ENOENT, not
            // ESRCH. Without this a dead PID is misclassified as OsError, which
            // breaks the idempotent-kill contract (kill_process returns Err
            // instead of Ok for a process that is simply gone).
            Some(libc::ENOENT) => ProcessStatus::Missing,
            Some(libc::EPERM) => ProcessStatus::Inaccessible,
            _ => {
                // On macOS, proc_pidinfo returns 0 for missing processes
                // and sets errno to ESRCH. But if errno wasn't set (kind == Other),
                // the process is likely gone.
                if e.kind() == std::io::ErrorKind::Other {
                    ProcessStatus::Missing
                } else {
                    ProcessStatus::OsError(e.kind())
                }
            }
        },
    }
}

/// How long a PTY child gets to act on its hangup before SIGTERM. A shell
/// passes the hangup on to its jobs as it exits; a SIGTERM that ended it first
/// would leave them running.
const HANGUP_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

/// Kill a process. Sends SIGTERM first, waits briefly, then SIGKILL. A PTY
/// session leader (`hang_up_first`) gets SIGHUP before that, as when its
/// terminal window closes, so an interactive shell hangs up its jobs, which
/// sit in their own process groups outside the group kill.
/// Tries process group kill first (kill(-pgid)), falls back to direct kill.
/// Verifies process identity before signaling to prevent killing recycled PIDs.
/// Returns Ok(()) if the process is already dead (idempotent).
fn kill_process(id: &ProcessIdentity, force: bool, hang_up_first: bool) -> io::Result<()> {
    match process_status(id) {
        ProcessStatus::Missing => return Ok(()),
        ProcessStatus::IdentityMismatch => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "PID was recycled -- refusing to kill wrong process",
            ));
        }
        ProcessStatus::OsError(kind) => {
            return Err(io::Error::new(kind, "failed to probe process status"));
        }
        // AliveVerified: identity confirmed, signal normally
        // Inaccessible: can't verify identity (EPERM from proc_pidinfo on macOS
        // when child is in a different session), but kill(2) will still work.
        // Degraded PID reuse safety -- acceptable because the sidecar wrote the
        // identity moments ago and PID reuse within that window is negligible.
        ProcessStatus::AliveVerified | ProcessStatus::Inaccessible => {}
    }

    let pid = id.pid.get() as i32;
    // Try process group first (kills descendants), fall back to direct
    let signal_tree =
        |signal| send_signal_group(pid, signal).or_else(|_| send_signal_direct(pid, signal));

    if force {
        return signal_tree(rustix::process::Signal::KILL);
    }

    if hang_up_first {
        signal_tree(rustix::process::Signal::HUP)?;
        match wait_for_exit(id, HANGUP_GRACE) {
            Some(ProcessStatus::Missing) => {
                // The group outlives its leader while members remain, and one
                // that ignores the hangup must still be stopped. Only the
                // group: the leader's own PID may already belong to another
                // process. The result is ignored: with the leader gone,
                // ESRCH means the group is empty, and EPERM that its members
                // are all unreaped zombies (macOS refuses such a group,
                // including the leader until its parent reaps it).
                let _ = send_signal_group(pid, rustix::process::Signal::TERM);
                return Ok(());
            }
            // The leader's PID belongs to another process, so its group,
            // which held that number, has no members left.
            Some(_) => return Ok(()),
            None => {}
        }
    }

    signal_tree(rustix::process::Signal::TERM)?;

    // Wait up to 5 seconds for graceful exit
    if wait_for_exit(id, std::time::Duration::from_secs(5)).is_some() {
        return Ok(());
    }

    // Still alive -- escalate to SIGKILL
    signal_tree(rustix::process::Signal::KILL)
}

/// Poll until `id` has exited or `within` passes. Returns how it ended:
/// `Missing`, or `IdentityMismatch` (its PID recycled, so the original is
/// dead); `None` if it is still alive.
fn wait_for_exit(id: &ProcessIdentity, within: std::time::Duration) -> Option<ProcessStatus> {
    let deadline = std::time::Instant::now() + within;
    loop {
        if let status @ (ProcessStatus::Missing | ProcessStatus::IdentityMismatch) =
            process_status(id)
        {
            return Some(status);
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(100).min(deadline - now));
    }
}

/// Send a signal to a process group (kill(-pgid, sig)).
fn send_signal_group(pid: i32, signal: rustix::process::Signal) -> io::Result<()> {
    // rustix::process::Pid is a NonZero type representing a positive PID.
    // For process group kill we need the group leader's PID.
    let rpid = rustix::process::Pid::from_raw(pid)
        .ok_or_else(|| io::Error::other("invalid pid for process group signal"))?;
    match rustix::process::kill_process_group(rpid, signal) {
        Ok(()) => Ok(()),
        Err(e) if e == rustix::io::Errno::SRCH => Ok(()), // already dead
        Err(e) => Err(e.into()),
    }
}

/// Send a signal directly to a process (kill(pid, sig)).
fn send_signal_direct(pid: i32, signal: rustix::process::Signal) -> io::Result<()> {
    let rpid = rustix::process::Pid::from_raw(pid)
        .ok_or_else(|| io::Error::other("invalid pid for direct signal"))?;
    match rustix::process::kill_process(rpid, signal) {
        Ok(()) => Ok(()),
        Err(e) if e == rustix::io::Errno::SRCH => Ok(()), // already dead
        Err(e) => Err(e.into()),
    }
}

/// Put `fd`'s open file description into nonblocking mode.
///
/// `O_NONBLOCK` is a file *status* flag: it belongs to the open file
/// description, so every descriptor duplicated from `fd` (including a
/// `try_clone`) observes the change. Set it once, at setup, before handing
/// duplicates to different threads.
pub fn set_nonblocking(fd: &impl rustix::fd::AsFd) -> io::Result<()> {
    let flags = rfs::fcntl_getfl(fd)?;
    rfs::fcntl_setfl(fd, flags | OFlags::NONBLOCK)?;
    Ok(())
}

/// Reads from a nonblocking PTY master as if it were blocking.
///
/// The PTY master's read and write halves share one open file description, so
/// the nonblocking mode the input writer needs also applies to the reader. This
/// reader waits for readability with `poll(2)` when a read would block, and
/// retries on `Interrupted`; neither is end of stream.
///
/// A reader made by `PtyReader::holding_slave` also keeps a descriptor for
/// the PTY's slave side open until the child has exited and the master has
/// been read until it would block. On macOS, when an exiting child's close is
/// the last close of the slave, the kernel waits only briefly (about 0.65 s
/// measured) for the master to read what the child wrote, then discards it;
/// a reader starved of CPU for longer lost a short-lived child's output.
/// Holding the slave means the child's close is never the last one, so on
/// macOS the child's exit waits in the kernel, without a time limit, until
/// this reader has drained the master. A stop (`kill_child`) therefore
/// restarts output stopped by `^S`, and gives the hold up at once when it is
/// forced or no reader is running, so no exit waits on a reader that cannot
/// drain. A graceful stop with this reader running keeps the hold, so what
/// the child writes as it stops is captured. Once the slave is
/// released, end of stream comes as before: when every other holder
/// (descendants that inherited the terminal) has closed it.
pub struct PtyReader {
    file: File,
    drain: Option<DrainWatch>,
}

/// What a holding [`PtyReader`] needs to release its hold once drained.
/// Dropping it releases the hold, so a reader that ends early (an error, a
/// panic) never leaves the child's exit waiting.
struct DrainWatch {
    hold: SlaveHold,
    /// Read end of a pipe whose write end closes once the child has exited.
    exited: File,
    /// The exit has been seen: release the slave once the master would block.
    exit_seen: bool,
}

impl Drop for DrainWatch {
    fn drop(&mut self) {
        self.hold.release();
        self.hold
            .0
            .reader_running
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// The sidecar's descriptor for a PTY's slave side, shared by the reader,
/// which releases it once drained, and the stop path, which may release it
/// at once. Whichever comes first closes it.
#[derive(Clone)]
struct SlaveHold(std::sync::Arc<HoldState>);

struct HoldState {
    slave: std::sync::Mutex<Option<OwnedFd>>,
    /// Capture is reading the master, so it will drain what the child leaves.
    reader_running: std::sync::atomic::AtomicBool,
}

impl SlaveHold {
    fn new(slave: OwnedFd) -> Self {
        Self(std::sync::Arc::new(HoldState {
            slave: std::sync::Mutex::new(Some(slave)),
            reader_running: std::sync::atomic::AtomicBool::new(false),
        }))
    }

    fn slave(&self) -> std::sync::MutexGuard<'_, Option<OwnedFd>> {
        self.0
            .slave
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Close the slave descriptor, if still held.
    fn release(&self) {
        drop(self.slave().take());
    }

    /// Prepare for a stop. Restart output stopped by `^S` (`TCOON`), which
    /// nothing can drain while stopped; then, if the stop is forced or no
    /// reader is running, close the descriptor so the child's exit does not
    /// wait for a drain. A no-op once released: the child has then exited
    /// and been drained.
    fn release_for_stop(&self, force: bool) {
        let mut slave = self.slave();
        let Some(fd) = slave.as_ref() else {
            return;
        };
        // Best effort: the stop goes ahead either way.
        let _ = rustix::termios::tcflow(fd, rustix::termios::Action::OOn);
        if force
            || !self
                .0
                .reader_running
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            drop(slave.take());
        }
    }
}

impl PtyReader {
    #[must_use]
    pub fn new(file: File) -> Self {
        Self { file, drain: None }
    }

    /// A reader that holds `hold` until the pipe read by `exited` reports
    /// the child's exit and the master is then drained.
    fn holding_slave(file: File, hold: SlaveHold, exited: File) -> Self {
        Self {
            file,
            drain: Some(DrainWatch {
                hold,
                exited,
                exit_seen: false,
            }),
        }
    }

    /// Capture has taken this reader: from now on a graceful stop keeps the
    /// hold, since this reader will drain what the child leaves.
    fn mark_running(&self) {
        if let Some(drain) = &self.drain {
            drain
                .hold
                .0
                .reader_running
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Called when a read would block: wait until the master may be readable,
    /// or, while the slave is held, until the child exits. A read that would
    /// block after the exit was seen means the master is drained, so the slave
    /// is released instead and the next read sees the end of stream once no
    /// other process holds the terminal.
    fn wait_readable(&mut self) -> io::Result<()> {
        use rustix::event::{PollFd, PollFlags, poll};

        let Some(drain) = &mut self.drain else {
            wait_for(&self.file, PollFlags::IN, None)?;
            return Ok(());
        };
        if drain.exit_seen {
            // Dropping the watch releases the hold.
            self.drain = None;
            return Ok(());
        }
        let mut fds = [
            PollFd::new(&self.file, PollFlags::IN),
            PollFd::new(&drain.exited, PollFlags::IN),
        ];
        loop {
            match poll(&mut fds, None) {
                Ok(_) => break,
                Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
        }
        drain.exit_seen = !fds[1].revents().is_empty();
        Ok(())
    }
}

impl Read for PtyReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.file.read(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.wait_readable()?,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                other => return other,
            }
        }
    }
}

/// Observes a child's exit without reaping it, so `child_wait` still collects
/// and classifies its status.
struct ExitWatch(std::sync::mpsc::SyncSender<rustix::process::Pid>);

impl ExitWatch {
    /// Start the watcher thread, before the child exists. Returns it and the
    /// read end of a pipe that reaches end of file once the watched child has
    /// exited, or at once if no child is ever watched.
    fn start() -> io::Result<(Self, File)> {
        use rustix::process::{WaitId, WaitIdOptions, waitid};

        let (exited, exit_signal) = pipe()?;
        let (pid_tx, pid_rx) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("pty-exit-watch".to_owned())
            .spawn(move || {
                if let Ok(pid) = pid_rx.recv() {
                    // WNOWAIT leaves the child waitable. Any other error
                    // (ECHILD: it was already reaped) also means it is gone.
                    // Residual: if a failure path reaps the child, and its pid
                    // is reused by another child of the sidecar (an on-exit
                    // hook) before this call, it waits on that one instead;
                    // the hold is then released by the stop, not by this.
                    while let Err(rustix::io::Errno::INTR) = waitid(
                        WaitId::Pid(pid),
                        WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
                    ) {}
                }
                drop(exit_signal);
            })?;
        Ok((Self(pid_tx), exited))
    }

    /// Watch `child`, which must be this process's own unreaped child.
    fn watch(self, child: &std::process::Child) {
        // The watcher holds the receiver until a pid arrives, so this cannot fail.
        let _ = self.0.send(rustix::process::Pid::from_child(child));
    }
}

/// Wait until `file` is ready for `flags`, or `timeout` elapses.
///
/// Returns `Ok(true)` when poll reports any event on the descriptor, including
/// hangup or error, which the following read or write then reports.
fn wait_for(
    file: &File,
    flags: rustix::event::PollFlags,
    timeout: Option<std::time::Duration>,
) -> io::Result<bool> {
    use rustix::event::{PollFd, Timespec, poll};

    let timespec = timeout.map(|d| Timespec {
        tv_sec: i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: i64::from(d.subsec_nanos()),
    });
    let mut fds = [PollFd::new(file, flags)];
    loop {
        match poll(&mut fds, timespec.as_ref()) {
            Ok(0) => return Ok(false),
            Ok(_) => return Ok(true),
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

/// Writes to a nonblocking PTY master.
///
/// [`PtyWriter::try_write`] makes exactly one nonblocking attempt, so a child
/// that is not reading its input cannot stall the caller. The [`Write`]
/// implementation waits for writability between attempts, for callers that
/// need blocking semantics.
pub struct PtyWriter {
    file: File,
}

impl PtyWriter {
    #[must_use]
    pub fn new(file: File) -> Self {
        Self { file }
    }

    /// One nonblocking write attempt. `WouldBlock` means the PTY input buffer is
    /// full; `Interrupted` means try again.
    pub fn try_write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.file.write(bytes)
    }

    /// Wait up to `timeout` for the PTY to accept input. `Ok(false)` means the
    /// timeout elapsed; `Ok(true)` means a write may now progress or will report
    /// the descriptor's error or hangup.
    pub fn wait_writable(&self, timeout: std::time::Duration) -> io::Result<bool> {
        wait_for(&self.file, rustix::event::PollFlags::OUT, Some(timeout))
    }
}

impl Write for PtyWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            match self.file.write(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    wait_for(&self.file, rustix::event::PollFlags::OUT, None)?;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                other => return other,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;
    use std::time::{Duration, Instant};

    /// The sweep used when `/dev/fd` cannot be read must reach a leaked fd
    /// high in the table, as exe-init's are.
    #[test]
    fn rlimit_sweep_marks_a_high_leaked_fd_cloexec() {
        let file = File::open("/dev/null").unwrap();
        // SAFETY: F_DUPFD makes a new fd, at 200 or above, that this test
        // owns; unlike F_DUPFD_CLOEXEC it starts without close-on-exec.
        let high = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, 200) };
        assert!(high >= 200);
        // SAFETY: F_GETFD only reads a descriptor flag.
        let flags = |fd| unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_eq!(flags(high) & libc::FD_CLOEXEC, 0);

        InheritedFds::Below(fd_limit()).mark_cloexec();

        let marked = flags(high) & libc::FD_CLOEXEC != 0;
        // SAFETY: `high` is ours, from F_DUPFD above.
        unsafe { libc::close(high) };
        assert!(marked, "fd {high} below the limit was not marked");
    }

    // --- Shared-description nonblocking PTY I/O ---

    fn nonblocking_pipe() -> (File, File) {
        let (r, w) = rustix::pipe::pipe().unwrap();
        set_nonblocking(&r).unwrap();
        (File::from(r), File::from(w))
    }

    #[test]
    fn nonblocking_flag_is_shared_by_duplicated_descriptors() {
        let (r, _w) = rustix::pipe::pipe().unwrap();
        let dup = r.try_clone().unwrap();
        set_nonblocking(&r).unwrap();
        assert!(
            rfs::fcntl_getfl(&dup).unwrap().contains(OFlags::NONBLOCK),
            "a duplicate observes O_NONBLOCK set through the original"
        );
    }

    #[test]
    fn pty_reader_waits_for_late_output_instead_of_ending() {
        let (r, mut w) = nonblocking_pipe();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            w.write_all(b"late").unwrap();
            // Dropping `w` closes the pipe: end of stream after the data.
        });
        let mut out = Vec::new();
        PtyReader::new(r).read_to_end(&mut out).unwrap();
        writer.join().unwrap();
        assert_eq!(out, b"late");
    }

    #[test]
    fn pty_writer_blocking_write_completes_while_a_slow_reader_drains() {
        let (mut r, w) = rustix::pipe::pipe()
            .map(|(r, w)| (File::from(r), File::from(w)))
            .unwrap();
        set_nonblocking(&w).unwrap();
        let payload = vec![0x5A_u8; 1 << 20];
        let reader = std::thread::spawn(move || {
            let mut got = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                match r.read(&mut chunk) {
                    Ok(0) => break got,
                    Ok(n) => got.extend_from_slice(&chunk[..n]),
                    Err(e) => panic!("reader failed: {e}"),
                }
                std::thread::sleep(Duration::from_micros(200));
            }
        });
        let mut writer = PtyWriter::new(w);
        writer.write_all(&payload).unwrap();
        drop(writer);
        assert_eq!(reader.join().unwrap(), payload);
    }

    #[test]
    fn pty_writer_try_write_reports_would_block_when_full_and_wait_times_out() {
        let (_r, w) = rustix::pipe::pipe().unwrap();
        set_nonblocking(&w).unwrap();
        let mut writer = PtyWriter::new(File::from(w));
        let chunk = [0u8; 4096];
        let full = (0..10_000).find_map(|_| writer.try_write(&chunk).err());
        let err = full.expect("a pipe buffer fills within 40 MiB");
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);

        let started = Instant::now();
        assert!(!writer.wait_writable(Duration::from_millis(50)).unwrap());
        assert!(started.elapsed() >= Duration::from_millis(40));
    }

    #[test]
    fn pty_output_after_an_idle_period_reaches_the_reader() {
        let argv = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "sleep 0.2; printf late-output".to_owned(),
        ];
        let mut child =
            UnixPlatform::spawn_child_pty(&argv, None, &BTreeSet::new(), &BTreeMap::new()).unwrap();
        let mut reader = UnixPlatform::child_stdout(&mut child).unwrap();
        let mut out = Vec::new();
        let mut chunk = [0u8; 1024];
        let deadline = Instant::now() + Duration::from_secs(10);
        // A PTY master reports hangup as EIO on Linux and as EOF on macOS.
        while Instant::now() < deadline {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => out.extend_from_slice(&chunk[..n]),
            }
        }
        // Reap deterministically even if reading stopped early: a session leader
        // whose PTY output is never drained may otherwise block on exit.
        let kill = UnixPlatform::child_kill_handle(&child);
        let _ = UnixPlatform::kill_child(&kill, true);
        let _ = UnixPlatform::child_wait(&mut child);
        assert!(
            String::from_utf8_lossy(&out).contains("late-output"),
            "idle-then-output was lost: {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    fn fake_identity(pid: u32, start_time_ns: u64) -> ProcessIdentity {
        ProcessIdentity {
            pid: NonZeroU32::new(pid).unwrap(),
            start_time_ns,
        }
    }

    #[test]
    fn process_status_self_is_alive_verified() {
        let self_id = UnixPlatform::self_identity().unwrap();
        assert_eq!(process_status(&self_id), ProcessStatus::AliveVerified);
    }

    #[test]
    fn process_status_missing_pid_returns_missing() {
        // PID 4_000_000 is safely above any real PID
        let id = fake_identity(4_000_000, 0);
        assert_eq!(process_status(&id), ProcessStatus::Missing);
    }

    #[test]
    fn is_process_gone_classifies_esrch_and_enoent() {
        assert!(is_process_gone(&io::Error::from_raw_os_error(libc::ESRCH)));
        assert!(is_process_gone(&io::Error::from_raw_os_error(libc::ENOENT)));
        assert!(!is_process_gone(&io::Error::from_raw_os_error(libc::EPERM)));
        assert!(!is_process_gone(&io::Error::from_raw_os_error(
            libc::EACCES
        )));
    }

    #[test]
    fn capture_spawned_identity_of_gone_pid_is_already_exited() {
        // A pid that never existed models a child that exited before identity
        // capture (the fast-exit race). Capture must yield the already-exited
        // placeholder, NOT an error -- otherwise a successful short-lived run is
        // misreported as SpawnFailed (the macOS-only regression this fixes).
        let id = capture_spawned_identity(4_000_000)
            .expect("a gone child is a short-lived run, not a spawn error");
        assert_eq!(id.pid.get(), 4_000_000);
        assert_eq!(
            id.start_time_ns, 0,
            "already-exited placeholder carries the sentinel start time"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn process_start_time_of_missing_pid_is_deterministic_esrch() {
        // proc_pidinfo leaves errno stale on failure; the kill(pid, 0) reprobe
        // must normalize a gone pid to ESRCH so callers classify it reliably
        // (this is what stops the phantom ENOENT "No such file or directory"
        // reports for a process that has simply exited).
        let err = process_start_time(4_000_000).expect_err("pid 4_000_000 never exists");
        assert_eq!(err.raw_os_error(), Some(libc::ESRCH));
    }

    #[test]
    fn process_status_wrong_start_time_returns_identity_mismatch() {
        // Use our own PID but with a bogus start time
        let self_id = UnixPlatform::self_identity().unwrap();
        let wrong = fake_identity(self_id.pid.get(), self_id.start_time_ns.wrapping_add(1));
        assert_eq!(process_status(&wrong), ProcessStatus::IdentityMismatch);
    }

    #[test]
    fn process_status_inaccessible_on_cross_session_child() {
        // Spawn a child in a new session (setsid), then probe from parent.
        // On macOS, proc_pidinfo returns EPERM for processes in other sessions.
        // On Linux, /proc/<pid>/stat is world-readable so this returns AliveVerified.
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        let mut cmd = Command::new("sleep");
        cmd.arg("10");
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: pre_exec runs after fork() in the child, before exec().
        // setsid is async-signal-safe. No shared mutable state is accessed.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn().unwrap();
        let child_pid = child.id();

        // Small delay for process to be queryable
        std::thread::sleep(std::time::Duration::from_millis(50));

        let child_id = match process_identity(child_pid) {
            Ok(id) => id,
            Err(_) => {
                // On macOS, proc_pidinfo may already EPERM here.
                // Verify that process_status on a fabricated identity returns Inaccessible.
                let fabricated = fake_identity(child_pid, 0);
                let status = process_status(&fabricated);
                assert!(
                    status == ProcessStatus::Inaccessible || status == ProcessStatus::Missing,
                    "expected Inaccessible or Missing for cross-session child, got {status:?}"
                );
                // Clean up
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        };

        let status = process_status(&child_id);

        // macOS: Inaccessible (EPERM from proc_pidinfo across sessions)
        // Linux: AliveVerified (/proc/<pid>/stat is readable)
        assert!(
            status == ProcessStatus::AliveVerified || status == ProcessStatus::Inaccessible,
            "expected AliveVerified or Inaccessible, got {status:?}"
        );

        // Clean up
        let _ = child.kill();
        let _ = child.wait();
    }

    /// A PTY leader that exits on its hangup stays a zombie until its parent
    /// reaps it, and macOS refuses a signal to a group of zombies with EPERM.
    /// The kill still succeeds: there is nothing left in the group to stop.
    /// macOS only: elsewhere a zombie still reads as alive, so the kill takes
    /// the SIGTERM/SIGKILL path and never reaches the zombie group.
    #[cfg(target_os = "macos")]
    #[test]
    fn hang_up_kill_of_a_leader_left_unreaped_succeeds() {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        let mut cmd = Command::new("sleep");
        cmd.arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: pre_exec runs after fork() in the child, before exec().
        // setsid is async-signal-safe. No shared mutable state is accessed.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn().unwrap();
        let identity = capture_spawned_identity(child.id()).unwrap();

        // `sleep` dies on SIGHUP; nothing reaps it until the end of the test.
        let result = kill_process(&identity, false, true);
        let _ = child.kill();
        let _ = child.wait();
        result.expect("killing a leader that exited on its hangup");
    }
}
