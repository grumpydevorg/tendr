//! Process-supervision backends — one [`Platform`] trait with a Unix and a
//! Windows implementation, selected at compile time as [`Current`].
//!
//! The trait hides every raw OS handle (PIDs, fds, Windows `HANDLE`s) behind
//! opaque associated types, so callers supervise children — spawn, contain,
//! observe, tree-kill, handshake readiness — without `cfg`-branching. Process
//! *observation* is the typed [`ProcessStatus`], never a boolean; lifecycle
//! *state* is owned by the sidecar, not decided here.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io;
use std::path::Path;
use std::process::ExitStatus;

use crate::model::ids::ProcessIdentity;

#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;

#[cfg(unix)]
pub type Current = unix::UnixPlatform;
#[cfg(windows)]
pub type Current = windows::WindowsPlatform;

/// Rows a PTY child starts with, until a client sets its size.
pub const INITIAL_PTY_ROWS: u16 = 24;
/// Columns a PTY child starts with, until a client sets its size.
pub const INITIAL_PTY_COLS: u16 = 80;

/// Result of probing a process by identity.
/// Lifecycle state comes from the sidecar; process observation comes from
/// this typed OS result -- never a boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum ProcessStatus {
    /// PID exists and identity matches -- safe to signal.
    AliveVerified,
    /// PID does not exist (ESRCH).
    Missing,
    /// PID exists but identity differs -- PID was recycled.
    IdentityMismatch,
    /// PID exists but the OS denied reading it (EPERM): another user's
    /// process. A same-user process in another session is readable
    /// (`proc_pidinfo` and `getsid` on macOS 27, `/proc` on Linux). Signals
    /// go through only with the privilege to send them; PID reuse safety is
    /// degraded.
    Inaccessible,
    /// Unexpected OS error.
    OsError(std::io::ErrorKind),
}

/// Platform-specific process supervision operations.
///
/// The key abstraction is `SupervisedChild`: an opaque bundle of
/// backend state for a child process under supervision. On Unix this
/// wraps std::process::Child + ProcessIdentity. On Windows this wraps
/// a process HANDLE, Job Object HANDLE, graceful-stop event HANDLE,
/// and stdout/stderr pipe HANDLEs.
///
/// Callers never see raw HANDLEs, PIDs, or fds -- only this type.
pub trait Platform {
    /// Opaque supervised-child state. Dropped when supervision ends.
    type SupervisedChild;
    /// Lightweight, cloneable kill handle extracted from SupervisedChild.
    /// Carries enough backend state to kill the child tree from another thread
    /// (timeout thread) without needing the full SupervisedChild.
    ///
    /// On Unix: wraps ProcessIdentity (group kill via -pgid).
    /// On Windows: wraps Job Object HANDLE + stop event HANDLE (tree kill).
    type ChildKillHandle: Send + Clone;
    type StdinTransport: Send;
    type ReadyReader;
    type ReadyWriter;

    // --- Sidecar spawn (CLI side) ---

    /// Spawn the sidecar as a detached process.
    /// Returns the sidecar PID (for meta.json).
    fn spawn_sidecar(
        tendr_bin: &Path,
        session_dir: &Path,
        ready_writer: &Self::ReadyWriter,
    ) -> io::Result<u32>;

    // --- Readiness channel ---

    /// Create a readiness channel (anonymous pipe).
    fn ready_channel() -> io::Result<(Self::ReadyReader, Self::ReadyWriter)>;

    /// Block until the sidecar writes a readiness message.
    fn read_ready_signal(reader: Self::ReadyReader) -> io::Result<String>;

    /// Write a readiness message from the sidecar side.
    fn write_ready_signal(writer: Self::ReadyWriter, message: &str) -> io::Result<()>;

    // --- Child spawn (sidecar side) ---

    /// Spawn a child process under supervision.
    ///
    /// Sets up platform-specific process grouping/containment:
    /// - Unix: setpgid(0,0) to create new process group
    /// - Windows: CREATE_SUSPENDED -> Job Object -> ResumeThread
    ///
    /// `env_remove` names are removed from the inherited environment first;
    /// `env` entries are applied after, so they win.
    ///
    /// Returns an opaque SupervisedChild that owns all backend state.
    fn spawn_child(
        argv: &[String],
        stdin_piped: bool,
        cwd: Option<&Path>,
        env_remove: &BTreeSet<String>,
        env: &BTreeMap<String, String>,
    ) -> io::Result<Self::SupervisedChild>;

    /// Spawn a child process under a pseudo-terminal.
    /// child_stdout() returns PTY master read half.
    /// child_stdin() returns PTY master write half.
    /// child_stderr() returns None (merged output).
    /// `env_remove` and `env` apply as for `spawn_child`.
    ///
    /// Unix only in slice one. Windows returns Err.
    fn spawn_child_pty(
        argv: &[String],
        cwd: Option<&Path>,
        env_remove: &BTreeSet<String>,
        env: &BTreeMap<String, String>,
    ) -> io::Result<Self::SupervisedChild>;

    /// Get the ProcessIdentity of the supervised child, captured at spawn
    /// (a spawn that cannot identify its child fails instead).
    /// Cheap, callable any time -- borrows only.
    fn child_identity(child: &Self::SupervisedChild) -> ProcessIdentity;

    /// Wait for the child to exit. Blocks until the process terminates.
    /// Does NOT consume the child -- handles stay open for cleanup.
    fn child_wait(child: &mut Self::SupervisedChild) -> io::Result<ExitStatus>;

    /// Poll for child exit without blocking.
    /// Returns `Ok(Some(status))` if the child has exited, `Ok(None)` if still
    /// running, or `Err` on OS failure.
    fn child_try_wait(child: &mut Self::SupervisedChild) -> io::Result<Option<ExitStatus>>;

    /// Take the child's stdout stream for capture.
    /// Returns None if already taken. Moves ownership to the caller
    /// (typically a capture thread).
    fn child_stdout(child: &mut Self::SupervisedChild) -> Option<Box<dyn io::Read + Send>>;

    /// Take the child's stderr stream for capture.
    /// Same take-once semantics as child_stdout.
    fn child_stderr(child: &mut Self::SupervisedChild) -> Option<Box<dyn io::Read + Send>>;

    /// Take the child's stdin for forwarding (if stdin_piped was true).
    /// Same take-once semantics.
    fn child_stdin(child: &mut Self::SupervisedChild) -> Option<Box<dyn io::Write + Send>>;

    /// Get a dup'd file descriptor for applying PTY resize (TIOCSWINSZ).
    /// Must be called before `child_stdin` takes the write half.
    /// Returns None for non-PTY children or on platforms without PTY support.
    fn pty_resize_fd(child: &Self::SupervisedChild) -> Option<File>;

    // --- Kill ---

    /// Extract a lightweight kill handle from the supervised child.
    /// The handle is Send + Clone and can be moved to a timeout thread.
    /// Must be called before child_wait (which takes &mut self).
    fn child_kill_handle(child: &Self::SupervisedChild) -> Self::ChildKillHandle;

    /// Kill a supervised child via its kill handle.
    ///
    /// Contract (what implementations must provide):
    /// - `force = false`: request graceful stop (SIGTERM on Unix,
    ///   best-effort CTRL_BREAK on Windows); waits up to 5s then
    ///   escalates to force termination. A Unix PTY child gets SIGHUP and
    ///   up to 1s to act on it first, as when its terminal closes.
    /// - `force = true`: immediate termination (SIGKILL / TerminateJobObject).
    ///
    /// Does not reap the child. Callers must still call `child_wait`
    /// or `child_try_wait` afterward.
    fn kill_child(handle: &Self::ChildKillHandle, force: bool) -> io::Result<()>;

    /// Make this process the parent of every process its children orphan,
    /// so that [`Platform::stop_run`] can still find them: on Linux a
    /// child subreaper (`PR_SET_CHILD_SUBREAPER`), refused with an error
    /// where the kernel has no children lists to find them by. Call it
    /// before spawning the child. Returns `false` where the platform has no
    /// such mechanism; the caller then has no orphans to reap.
    fn adopt_orphans() -> io::Result<bool>;

    /// Reap the orphans this process adopted (see
    /// [`Platform::adopt_orphans`]) that have exited. Waits on each by pid,
    /// never on any child, and leaves `child`, the supervised child, to
    /// `child_wait`. Skips the pass while a stop is searching. The caller
    /// must not run it while this process has other children of its own,
    /// which their spawner reaps.
    fn reap_orphans(child: &ProcessIdentity);

    /// Stop the whole run: the child by [`Platform::kill_child`] (a child
    /// already gone counts as stopped), then every other process the run
    /// started that is still running, wherever it went (another process
    /// group, its own session, orphaned). Unless `force`, each first gets
    /// SIGTERM, SIGCONT and up to a second to exit. Those left are stopped
    /// before they are killed, so they cannot start more, and the search
    /// repeats until it finds none. Another tendr sidecar, a session started
    /// inside the run, is left alone with its tree. The caller must not run
    /// it while this process has children other than the run's. Errors name
    /// the processes it could not end.
    ///
    /// - Linux: the run is every descendant of this process, a child
    ///   subreaper.
    /// - macOS: the run is the same user's processes in the child's session
    ///   or this one, this process's descendants, the holders of the run's
    ///   terminal, and their descendants, listed before the child is
    ///   signalled and again after. It misses a process that left the
    ///   session, lost its parent and holds no terminal descriptor.
    /// - Windows: only `kill_child`. The kill-on-close Job Object, which
    ///   only the sidecar holds, ends the rest of the tree when the sidecar
    ///   exits.
    fn stop_run(handle: &Self::ChildKillHandle, force: bool) -> io::Result<()>;

    /// Kill an orphaned process by persisted identity (no live handle).
    ///
    /// Used when the sidecar has crashed and we only have ProcessIdentity
    /// from meta.json or the child_pid breadcrumb.
    fn kill_orphan(id: &ProcessIdentity, force: bool) -> io::Result<()>;

    // --- Process identity ---

    /// Get the identity of the current process (for sidecar meta).
    fn self_identity() -> io::Result<ProcessIdentity>;

    /// Get the identity of any process by PID (for orphan recovery).
    fn process_identity(pid: u32) -> io::Result<ProcessIdentity>;

    /// Probe whether a process is alive and matches the given identity.
    fn process_status(id: &ProcessIdentity) -> ProcessStatus;

    // --- stdin transport ---

    /// Create the stdin transport (sidecar side).
    /// Unix: mkfifo. Windows: CreateNamedPipe.
    fn create_stdin_transport(session_dir: &Path) -> io::Result<Self::StdinTransport>;

    /// Wait for a writer to connect to the stdin transport and return a reader.
    /// Blocks until a writer connects. Returns None if the transport is
    /// closed/removed (e.g., session cleanup).
    ///
    /// Called in a loop by the forwarding thread:
    /// - Unix: opens the FIFO for reading (blocks until a writer connects)
    /// - Windows: calls ConnectNamedPipe on the server handle
    fn accept_stdin_connection(
        transport: &Self::StdinTransport,
        session_dir: &Path,
    ) -> Option<Box<dyn io::Read + Send>>;

    /// Open the stdin transport for writing (push command side).
    /// Returns immediately if no reader (ConnectionRefused).
    fn open_stdin_writer(session_dir: &Path) -> io::Result<File>;

    /// Remove the stdin transport on cleanup.
    fn remove_stdin_transport(session_dir: &Path);

    // --- Ready fd inheritance (sidecar side) ---

    /// Construct a ReadyWriter from the OS-specific sidecar entry point.
    /// Unix: converts TENDR_READY_FD (RawFd from env var) to File.
    /// Windows: converts TENDR_READY_HANDLE to a HANDLE-based writer.
    fn ready_writer_from_env() -> io::Result<Self::ReadyWriter>;

    /// Prevent the ready channel from leaking to the child process.
    /// Unix: set CLOEXEC on the ready fd (returns the same file).
    /// Windows: replace the inheritable HANDLE with a non-inheritable duplicate.
    fn seal_ready_fd(writer: Self::ReadyWriter) -> io::Result<Self::ReadyWriter>;
}
