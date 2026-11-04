//! Private, peer-verified attach pipes (Windows): the carrier for a PTY
//! session's attach frames, as `attach_socket` is on Unix.
//!
//! A run's endpoint is a named pipe, `\\.\pipe\tendr-attach-<hash>`, named
//! from the session directory and run id as the Unix socket is, and
//! published in the same breadcrumb (`a.sock.path`). Its security
//! descriptor admits only the run owner's account, it refuses remote
//! clients, and the sidecar creates its first instance exclusively
//! (`FILE_FLAG_FIRST_PIPE_INSTANCE`), so no other process can serve the
//! name first. Both ends still check the other's account before any
//! terminal bytes flow. The frames are those of `attach_proto`, carried
//! unchanged: [`PipeStream`] offers the part of `UnixStream`'s interface
//! the attach code uses (reads and writes with timeouts, clones, shutdown).
//!
//! Every handle is overlapped, so a timeout or a shutdown cancels the one
//! operation it ends. A shutdown wakes this side's blocked reads and writes;
//! the peer sees the end of the stream when the last handle to this end
//! closes, as the threads it woke drop theirs. Unlike a socket, a pipe whose
//! peer has gone but still holds unread bytes is not reported as hung up
//! ([`peer_hung_up`]) until they are read.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_NO_DATA,
    ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED,
    HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

use crate::model::ids::RunId;
use crate::session::SessionRoot;

/// The attach stream of this platform.
pub type Stream = PipeStream;
/// The attach listener of this platform.
pub type Listener = PipeListener;

/// Bytes each direction of a pipe instance buffers.
const BUFFER: u32 = 64 * 1024;

/// A run's bound attach pipe: the listener and its name. Nothing on disk is
/// tightened on Windows, so `tightened_root` is always `None`.
pub struct BoundSocket {
    pub listener: PipeListener,
    pub path: PathBuf,
    pub tightened_root: Option<std::convert::Infallible>,
}

/// The pipe name of run `run_id` in `session_dir`.
#[must_use]
pub fn pipe_name(session_dir: &Path, run_id: RunId) -> PathBuf {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(session_dir.as_os_str().as_encoded_bytes());
    hasher.update(run_id.as_uuid().as_bytes());
    let name: String = hasher.finalize()[..6]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    PathBuf::from(format!(r"\\.\pipe\tendr-attach-{name}"))
}

/// Create the run's attach pipe and publish its breadcrumb. Nothing is
/// advertised unless both succeed.
///
/// # Errors
///
/// The pipe's creation (one of that name already exists, say) or the
/// breadcrumb's write; the caller must not start the PTY session.
pub fn bind_for_session(
    _sessions: &SessionRoot,
    session_dir: &Path,
    run_id: RunId,
) -> io::Result<BoundSocket> {
    let path = pipe_name(session_dir, run_id);
    let listener = PipeListener::bind(&path)?;
    let breadcrumb = session_dir.join("a.sock.path");
    let tmp = session_dir.join("a.sock.path.tmp");
    std::fs::write(&tmp, path.as_os_str().as_encoded_bytes())
        .and_then(|()| std::fs::rename(&tmp, &breadcrumb))?;
    Ok(BoundSocket {
        listener,
        path,
        tightened_root: None,
    })
}

/// Whether a pipe named `path` is being served. Asked without connecting,
/// which would take one of the server's instances.
#[must_use]
pub fn exists(path: &Path) -> bool {
    use windows_sys::Win32::System::Pipes::WaitNamedPipeW;
    let Ok(name) = wide(path) else {
        return false;
    };
    // SAFETY: `name` is NUL-terminated. A 1 ms wait only asks.
    if unsafe { WaitNamedPipeW(name.as_ptr(), 1) } != 0 {
        return true;
    }
    // All instances busy still means it exists.
    io::Error::last_os_error().raw_os_error() != Some(ERROR_FILE_NOT_FOUND as i32)
}

/// Nothing to remove: a pipe goes once its last handle closes, as the
/// listener's do when the sidecar exits.
pub fn remove(_path: &Path) {}

/// How long [`connect`] waits for a busy pipe's instance: the server makes
/// the next as soon as one is taken, so only a server that has stopped
/// accepting keeps it busy this long.
const BUSY_WAIT: Duration = Duration::from_secs(10);

/// Connect to the attach pipe `path`, waiting while every instance is busy
/// (the server makes a new one as each connects), for up to [`BUSY_WAIT`].
///
/// # Errors
///
/// `NotFound` when no such pipe is served, `ERROR_PIPE_BUSY` when no
/// instance came free, or the OS error.
pub fn connect(path: &Path) -> io::Result<PipeStream> {
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, SECURITY_IDENTIFICATION,
        SECURITY_SQOS_PRESENT,
    };
    use windows_sys::Win32::System::Pipes::WaitNamedPipeW;

    let name = wide(path)?;
    let deadline = Instant::now() + BUSY_WAIT;
    loop {
        // SAFETY: `name` is NUL-terminated. Identification only: the server
        // may learn who we are, never act as us.
        let raw = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                std::ptr::null_mut(),
            )
        };
        if raw != INVALID_HANDLE_VALUE {
            // SAFETY: a valid handle from CreateFileW is ours.
            return Ok(PipeStream::new(
                unsafe { OwnedHandle::from_raw_handle(raw) },
                false,
            ));
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(ERROR_PIPE_BUSY as i32) || Instant::now() >= deadline {
            return Err(err);
        }
        // SAFETY: `name` is NUL-terminated.
        if unsafe { WaitNamedPipeW(name.as_ptr(), 1000) } == 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) {
                return Err(err);
            }
        }
    }
}

/// Whether the process at the other end of `stream` runs as this process's
/// user.
///
/// # Errors
///
/// `PermissionDenied` for another account, or the OS error of finding out
/// (which also refuses the peer).
pub fn verify_peer(stream: &PipeStream) -> io::Result<()> {
    use windows_sys::Win32::System::Pipes::{
        GetNamedPipeClientProcessId, GetNamedPipeServerProcessId,
    };
    let mut pid = 0;
    // SAFETY: a valid (or null, refused) handle and an out-pointer.
    let ok = stream.with_raw(|handle| unsafe {
        if stream.0.server {
            GetNamedPipeClientProcessId(handle, &mut pid)
        } else {
            GetNamedPipeServerProcessId(handle, &mut pid)
        }
    });
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let peer = process_user(Some(pid))?;
    let me = process_user(None)?;
    if peer == me {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("attach peer (process {pid}) runs as another user"),
        ))
    }
}

/// Whether the peer has closed its end, checked without blocking or
/// consuming data. Bytes it sent and nobody has read yet keep it open.
#[must_use]
pub fn peer_hung_up(stream: &PipeStream) -> bool {
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;
    let mut available = 0;
    // SAFETY: a valid (or null, refused) handle; only the byte count is
    // asked for.
    let ok = stream.with_raw(|handle| unsafe {
        PeekNamedPipe(
            handle,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    });
    ok == 0
}

/// Reads from a pipe under one overall deadline, so a client that trickles
/// bytes cannot extend a handshake indefinitely.
pub struct DeadlineReader<'a> {
    pub stream: &'a PipeStream,
    pub deadline: Instant,
}

impl Read for DeadlineReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.stream.set_read_timeout(Some(remaining))?;
        Read::read(&mut &*self.stream, buf)
    }
}

/// The server end of a named pipe, accepting one client per instance.
pub struct PipeListener {
    name: Vec<u16>,
    /// The security descriptor every instance is created with.
    security: SecurityDescriptor,
    /// The instance the next client connects to.
    next: Mutex<Option<OwnedHandle>>,
}

// SAFETY: the raw security descriptor is immutable after creation and only
// read by CreateNamedPipeW; the rest is Send + Sync already.
unsafe impl Send for PipeListener {}
unsafe impl Sync for PipeListener {}

impl PipeListener {
    /// Create the pipe `path`, failing if a pipe of that name exists.
    ///
    /// # Errors
    ///
    /// The OS error (`ERROR_ACCESS_DENIED` when the name is taken).
    pub fn bind(path: &Path) -> io::Result<Self> {
        let name = wide(path)?;
        let security = SecurityDescriptor::owner_only()?;
        let first = create_instance(&name, &security, true)?;
        Ok(Self {
            name,
            security,
            next: Mutex::new(Some(first)),
        })
    }

    /// Wait for the next client.
    ///
    /// # Errors
    ///
    /// The OS error of creating or connecting an instance.
    pub fn accept(&self) -> io::Result<(PipeStream, ())> {
        use windows_sys::Win32::System::Pipes::ConnectNamedPipe;
        let instance = match self.next.lock().unwrap_or_else(|e| e.into_inner()).take() {
            Some(instance) => instance,
            None => create_instance(&self.name, &self.security, false)?,
        };
        let raw: HANDLE = instance.as_raw_handle().cast();
        let result = with_event(|event| {
            let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
            ov.hEvent = event;
            // SAFETY: valid handle; `ov` outlives the operation, which is
            // waited for below before `ov` goes out of scope.
            if unsafe { ConnectNamedPipe(raw, &mut ov) } != 0 {
                return Ok(());
            }
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(code) if code == ERROR_PIPE_CONNECTED as i32 => Ok(()),
                Some(code) if code == ERROR_IO_PENDING as i32 => {
                    let mut n = 0;
                    // SAFETY: waits for the operation `ov` describes.
                    if unsafe { GetOverlappedResult(raw, &ov, &mut n, 1) } == 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                }
                _ => Err(err),
            }
        });
        // Ready for the next client before this one is handed over.
        if let Ok(next) = create_instance(&self.name, &self.security, false) {
            *self.next.lock().unwrap_or_else(|e| e.into_inner()) = Some(next);
        }
        result?;
        Ok((PipeStream::new(instance, true), ()))
    }

    /// Accepted clients, for ever: each item is one [`Self::accept`].
    pub fn incoming(&self) -> impl Iterator<Item = io::Result<PipeStream>> + '_ {
        std::iter::repeat_with(|| self.accept().map(|(stream, ())| stream))
    }
}

fn create_instance(
    name: &[u16],
    security: &SecurityDescriptor,
    first: bool,
) -> io::Result<OwnedHandle> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX,
    };
    use windows_sys::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
        PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    let mut open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    let attributes = security.attributes();
    // SAFETY: `name` is NUL-terminated; `attributes` points at a valid
    // security descriptor for the call.
    let raw = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            BUFFER,
            BUFFER,
            0,
            &attributes,
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a valid handle from CreateNamedPipeW is ours.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// One end of an attach pipe. Clones share the handle, as a `UnixStream`'s
/// share its socket, and with it the timeouts and the shutdown.
pub struct PipeStream(Arc<Inner>);

struct Inner {
    /// `None` once both directions are shut down: closing it is how the peer
    /// learns of the end, as a socket's shutdown tells it.
    handle: RwLock<Option<OwnedHandle>>,
    server: bool,
    read_timeout: Mutex<Option<Duration>>,
    write_timeout: Mutex<Option<Duration>>,
    read_shut: AtomicBool,
    write_shut: AtomicBool,
    /// The OVERLAPPED of every operation in flight, by address, with
    /// whether it reads, so a shutdown can cancel exactly those it ends.
    in_flight: Mutex<Vec<(usize, bool)>>,
}

// SAFETY: the handle is a kernel object usable from any thread; the rest is
// synchronized.
unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

impl PipeStream {
    fn new(handle: OwnedHandle, server: bool) -> Self {
        Self(Arc::new(Inner {
            handle: RwLock::new(Some(handle)),
            server,
            read_timeout: Mutex::new(None),
            write_timeout: Mutex::new(None),
            read_shut: AtomicBool::new(false),
            write_shut: AtomicBool::new(false),
            in_flight: Mutex::new(Vec::new()),
        }))
    }

    /// Run `op` on the handle, which stays open until it returns; a closed
    /// one is null, which every call refuses.
    fn with_raw<T>(&self, op: impl FnOnce(HANDLE) -> T) -> T {
        let handle = self.0.handle.read().unwrap_or_else(|e| e.into_inner());
        op(handle
            .as_ref()
            .map_or(std::ptr::null_mut(), |h| h.as_raw_handle().cast()))
    }

    /// Another handle to the same end.
    ///
    /// # Errors
    ///
    /// Never; `Result` as `UnixStream::try_clone`.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self(Arc::clone(&self.0)))
    }

    /// Bound each later read; `None` waits for ever. A read that times out
    /// fails with `WouldBlock`, as a socket's does on Unix.
    ///
    /// # Errors
    ///
    /// `InvalidInput` for a zero duration, as std's sockets.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        *lock(&self.0.read_timeout) = nonzero(timeout)?;
        Ok(())
    }

    /// Bound each later write; as [`Self::set_read_timeout`].
    ///
    /// # Errors
    ///
    /// `InvalidInput` for a zero duration.
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        *lock(&self.0.write_timeout) = nonzero(timeout)?;
        Ok(())
    }

    /// End reads, writes or both on every clone: a read then returns end of
    /// stream, a write `BrokenPipe`, and ones blocked now are woken. Once
    /// both are ended the handle closes, so the peer reads what was sent and
    /// then the end, as from a socket shut down both ways.
    ///
    /// # Errors
    ///
    /// Never; `Result` as `UnixStream::shutdown`.
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        let (reads, writes) = match how {
            Shutdown::Read => (true, false),
            Shutdown::Write => (false, true),
            Shutdown::Both => (true, true),
        };
        if reads {
            self.0.read_shut.store(true, Ordering::SeqCst);
        }
        if writes {
            self.0.write_shut.store(true, Ordering::SeqCst);
        }
        // The handle's guard before the list's, in the order `transfer`
        // takes them.
        self.with_raw(|raw| {
            for &(ov, is_read) in lock(&self.0.in_flight).iter() {
                if (is_read && reads) || (!is_read && writes) {
                    // SAFETY: the OVERLAPPED is registered only while its
                    // operation is in flight, and the lock is held, so it
                    // is live; cancelling a finished operation does nothing.
                    unsafe { CancelIoEx(raw, ov as *const OVERLAPPED) };
                }
            }
        });
        if self.0.read_shut.load(Ordering::SeqCst) && self.0.write_shut.load(Ordering::SeqCst) {
            // Waits out the operations just cancelled.
            self.0
                .handle
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .take();
        }
        Ok(())
    }

    fn transfer(&self, buf: *mut u8, len: usize, is_read: bool) -> io::Result<usize> {
        use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
        use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
        use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};

        let shut = if is_read {
            &self.0.read_shut
        } else {
            &self.0.write_shut
        };
        let ended = || {
            if is_read {
                Ok(0)
            } else {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
        };
        let timeout = *lock(if is_read {
            &self.0.read_timeout
        } else {
            &self.0.write_timeout
        });
        let len = u32::try_from(len).unwrap_or(u32::MAX);
        // Held until the operation is over, so the handle cannot close
        // under it.
        let handle = self.0.handle.read().unwrap_or_else(|e| e.into_inner());
        let Some(raw) = handle
            .as_ref()
            .map(|h| h.as_raw_handle().cast::<core::ffi::c_void>())
        else {
            return ended();
        };
        if shut.load(Ordering::SeqCst) {
            return ended();
        }
        with_event(|event| {
            let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
            ov.hEvent = event;
            let key = (&raw const ov) as usize;
            lock(&self.0.in_flight).push((key, is_read));
            let _registered = Unregister(&self.0.in_flight, key);
            // SAFETY: `buf` holds `len` bytes for the operation, which is
            // waited for below before `buf` or `ov` can go away.
            let started = unsafe {
                if is_read {
                    ReadFile(raw, buf, len, std::ptr::null_mut(), &mut ov)
                } else {
                    WriteFile(raw, buf, len, std::ptr::null_mut(), &mut ov)
                }
            };
            let mut timed_out = false;
            if started == 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                    return finish_error(err, is_read, shut, false);
                }
                // A shutdown between the check above and the start missed
                // this operation: cancel it here.
                if shut.load(Ordering::SeqCst) {
                    // SAFETY: `ov` is this pending operation's.
                    unsafe { CancelIoEx(raw, &ov) };
                }
                let ms = timeout.map_or(INFINITE, |t| {
                    u32::try_from(t.as_millis()).unwrap_or(INFINITE - 1).max(1)
                });
                // SAFETY: the event is valid.
                if unsafe { WaitForSingleObject(event, ms) } == WAIT_TIMEOUT {
                    timed_out = true;
                    // SAFETY: `ov` is this pending operation's.
                    unsafe { CancelIoEx(raw, &ov) };
                }
            }
            let mut n = 0;
            // SAFETY: waits for the operation `ov` describes to finish.
            if unsafe { GetOverlappedResult(raw, &ov, &mut n, 1) } == 0 {
                if n > 0 {
                    return Ok(n as usize);
                }
                return finish_error(io::Error::last_os_error(), is_read, shut, timed_out);
            }
            Ok(n as usize)
        })
    }
}

/// Removes an operation from [`Inner::in_flight`] once it is over.
struct Unregister<'a>(&'a Mutex<Vec<(usize, bool)>>, usize);

impl Drop for Unregister<'_> {
    fn drop(&mut self) {
        lock(self.0).retain(|&(ov, _)| ov != self.1);
    }
}

/// What a failed read or write reports: a pipe whose peer has gone is the
/// end of the stream to a read and `BrokenPipe` to a write, as on a socket;
/// a cancelled one is a timeout (`WouldBlock`) or the shutdown's end.
fn finish_error(
    err: io::Error,
    is_read: bool,
    shut: &AtomicBool,
    timed_out: bool,
) -> io::Result<usize> {
    let code = err.raw_os_error().map(|c| c as u32);
    let gone = matches!(
        code,
        Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_NO_DATA)
    );
    if gone || (code == Some(ERROR_OPERATION_ABORTED) && shut.load(Ordering::SeqCst)) {
        return if is_read {
            Ok(0)
        } else {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        };
    }
    if code == Some(ERROR_OPERATION_ABORTED) && timed_out {
        return Err(io::Error::from(io::ErrorKind::WouldBlock));
    }
    Err(err)
}

impl Read for &PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.transfer(buf.as_mut_ptr(), buf.len(), true)
    }
}

impl Write for &PipeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // WriteFile only reads the buffer.
        self.transfer(buf.as_ptr().cast_mut(), buf.len(), false)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Read for PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self).read(buf)
    }
}

impl Write for PipeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&*self).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl std::fmt::Debug for PipeStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipeStream")
            .field("server", &self.0.server)
            .finish_non_exhaustive()
    }
}

thread_local! {
    /// This thread's event for overlapped operations, which run one at a
    /// time on a thread. Each operation resets it as it starts.
    static EVENT: Option<OwnedHandle> = {
        use windows_sys::Win32::System::Threading::CreateEventW;
        // SAFETY: plain call: a manual-reset, unnamed, unsignalled event.
        let raw = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        // SAFETY: a non-null handle from CreateEventW is ours.
        (!raw.is_null()).then(|| unsafe { OwnedHandle::from_raw_handle(raw) })
    };
}

fn with_event<T>(op: impl FnOnce(HANDLE) -> io::Result<T>) -> io::Result<T> {
    EVENT.with(|event| match event {
        Some(event) => op(event.as_raw_handle().cast()),
        None => Err(io::Error::other("no event for overlapped I/O")),
    })
}

/// A security descriptor that grants only this process's user, and
/// nothing to anyone else: not Everyone's read that a pipe's default
/// descriptor grants.
struct SecurityDescriptor(*mut core::ffi::c_void);

impl SecurityDescriptor {
    fn owner_only() -> io::Result<Self> {
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        let sddl: Vec<u16> = format!("D:P(A;;GA;;;{})", process_user(None)?)
            .encode_utf16()
            .chain([0])
            .collect();
        let mut sd = std::ptr::null_mut();
        // SAFETY: `sddl` is NUL-terminated; the result is freed on drop.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(sd))
    }

    fn attributes(&self) -> windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>()
                as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { windows_sys::Win32::Foundation::LocalFree(self.0) };
    }
}

/// The user SID of process `pid` (this one for `None`) in its string form
/// (`S-1-5-21-…`).
fn process_user(pid: Option<u32>) -> io::Result<String> {
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    let user = token_user(pid)?;
    let sid = user
        .as_ptr()
        .cast::<windows_sys::Win32::Security::TOKEN_USER>();
    let mut text = std::ptr::null_mut();
    // SAFETY: `user` holds a TOKEN_USER whose SID points into it.
    if unsafe { ConvertSidToStringSidW((*sid).User.Sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: on success `text` is a NUL-terminated string to LocalFree.
    Ok(unsafe {
        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        windows_sys::Win32::Foundation::LocalFree(text.cast());
        s
    })
}

/// The `TOKEN_USER` of process `pid` (this one for `None`), in a buffer
/// aligned for it.
fn token_user(pid: Option<u32>) -> io::Result<Vec<usize>> {
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TokenUser};
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let process = match pid {
        None => None,
        Some(pid) => {
            // SAFETY: plain call; the result is checked.
            let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
            if raw.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a non-null handle from OpenProcess is ours.
            Some(unsafe { OwnedHandle::from_raw_handle(raw) })
        }
    };
    let process_handle = process.as_ref().map_or_else(
        || unsafe { GetCurrentProcess() },
        |p| p.as_raw_handle().cast(),
    );
    let mut token = std::ptr::null_mut();
    // SAFETY: valid process handle and out-pointer.
    if unsafe { OpenProcessToken(process_handle, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a token handle from OpenProcessToken is ours.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut len = 0;
    // SAFETY: a null buffer asks for the size.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle().cast(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut len,
        )
    };
    let mut buf = vec![0usize; (len as usize).div_ceil(std::mem::size_of::<usize>()).max(1)];
    // SAFETY: the buffer holds `len` bytes.
    let ok = unsafe {
        GetTokenInformation(
            token.as_raw_handle().cast(),
            TokenUser,
            buf.as_mut_ptr().cast(),
            len,
            &mut len,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(buf)
}

fn nonzero(timeout: Option<Duration>) -> io::Result<Option<Duration>> {
    if timeout == Some(Duration::ZERO) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot set a 0 duration timeout",
        ));
    }
    Ok(timeout)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn wide(path: &Path) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    let s = path.as_os_str();
    if s.encode_wide().any(|u| u == 0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "nul in pipe name",
        ));
    }
    Ok(s.encode_wide().chain([0]).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(name: &str) -> (PipeStream, PipeStream) {
        let path = PathBuf::from(format!(
            r"\\.\pipe\tendr-test-{name}-{}",
            std::process::id()
        ));
        let listener = PipeListener::bind(&path).unwrap();
        let client = std::thread::spawn(move || connect(&path).unwrap());
        let (server, ()) = listener.accept().unwrap();
        (server, client.join().unwrap())
    }

    #[test]
    fn bytes_cross_both_ways_and_a_close_is_the_end_of_the_stream() {
        let (mut server, mut client) = pair("cross");
        client.write_all(b"hello").unwrap();
        let mut buf = [0u8; 5];
        server.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hello");
        server.write_all(b"back").unwrap();
        drop(server);
        let mut rest = Vec::new();
        client.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"back", "bytes written before the close still arrive");
        assert_eq!(
            client.write(b"x").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn a_read_times_out_with_would_block() {
        let (server, _client) = pair("timeout");
        server
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let mut buf = [0u8; 1];
        assert_eq!(
            (&server).read(&mut buf).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    /// A read blocked for ever (no timeout, a peer that never writes) is
    /// woken by a shutdown from another thread, and sees the end.
    #[test]
    fn a_shutdown_wakes_a_blocked_read() {
        let (server, _client) = pair("shutdown");
        let reader = server.try_clone().unwrap();
        let blocked = std::thread::spawn(move || {
            let mut buf = [0u8; 1];
            (&reader).read(&mut buf)
        });
        // Whether the read has started yet or not, it ends.
        server.shutdown(Shutdown::Read).unwrap();
        assert_eq!(blocked.join().unwrap().unwrap(), 0);
    }

    /// A shutdown both ways ends the stream for the peer, after what was
    /// sent, though a clone of this end is still held.
    #[test]
    fn a_shutdown_both_ways_is_the_end_for_the_peer() {
        let (mut server, mut client) = pair("shutboth");
        let _held = server.try_clone().unwrap();
        server.write_all(b"last").unwrap();
        server.shutdown(Shutdown::Both).unwrap();
        let mut rest = Vec::new();
        client.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"last");
        assert_eq!(
            server.write(b"x").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn the_peer_is_this_user_on_both_ends() {
        let (server, client) = pair("peer");
        verify_peer(&server).unwrap();
        verify_peer(&client).unwrap();
    }

    #[test]
    fn a_name_is_served_once_and_seen_without_taking_an_instance() {
        let path = PathBuf::from(format!(r"\\.\pipe\tendr-test-once-{}", std::process::id()));
        assert!(!exists(&path));
        let listener = PipeListener::bind(&path).unwrap();
        assert!(exists(&path));
        assert!(
            PipeListener::bind(&path).is_err(),
            "a second server is refused"
        );
        // The check took no instance: a client still connects to the first.
        let client = std::thread::spawn({
            let path = path.clone();
            move || connect(&path).unwrap()
        });
        let (_server, ()) = listener.accept().unwrap();
        let _client = client.join().unwrap();
    }

    #[test]
    fn a_missing_pipe_is_not_found() {
        let path = PathBuf::from(format!(r"\\.\pipe\tendr-test-none-{}", std::process::id()));
        assert_eq!(connect(&path).unwrap_err().kind(), io::ErrorKind::NotFound);
    }
}
