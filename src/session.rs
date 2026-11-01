//! The on-disk session directory — Tendr's durable record.
//!
//! A session lives at `~/.tendr/sessions/<namespace>/<session>/`, holding the
//! [`Meta`] authority file and the append-only
//! `output.log`. This module creates, resolves, and lists those directories;
//! it does not own the lifecycle — that is the [`sidecar`](crate::sidecar).

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::model::ids::{Namespace, SessionName};
use crate::model::meta::Meta;

/// Errors from session directory operations.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("session already exists: {0}")]
    AlreadyExists(String),
    #[error("session not found: {0}")]
    NotFound(String),
    #[error("corrupt session {session}: {reason}")]
    Corrupt { session: String, reason: String },
    #[error("session is locked by another process: {0}")]
    Locked(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Root directory for all sessions. Overridable for tests.
#[derive(Debug, Clone)]
pub struct SessionRoot(PathBuf);

impl SessionRoot {
    /// Default: ~/.tendr/sessions/
    ///
    /// # Errors
    /// Returns an error if `HOME` is not set.
    pub fn default_path() -> anyhow::Result<Self> {
        let home = std::env::var("HOME").map_err(|_| anyhow::anyhow!("HOME not set"))?;
        Ok(Self(PathBuf::from(home).join(".tendr").join("sessions")))
    }

    /// Explicit path (for tests or custom deployments).
    pub fn new(path: PathBuf) -> Self {
        Self(path)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// The state root that holds this sessions directory (`~/.tendr`), by
    /// structure: its parent, whatever the directories are named.
    #[must_use]
    pub fn state_root(&self) -> Option<&Path> {
        self.0.parent()
    }
}

/// Handle to an existing session directory.
#[derive(Debug)]
pub struct SessionDir {
    path: PathBuf,
    name: SessionName,
}

impl SessionDir {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn name(&self) -> &SessionName {
        &self.name
    }

    pub fn meta_path(&self) -> PathBuf {
        self.path.join("meta.json")
    }

    fn meta_tmp_path(&self) -> PathBuf {
        self.path.join("meta.json.tmp")
    }

    pub fn lock_path(&self) -> PathBuf {
        self.path.join("lock")
    }
}

/// Create a new session directory. Fails if it already exists.
/// Uses atomic mkdir to avoid TOCTOU race between exists() and create().
/// Path: `root/<namespace>/<session>/`
pub fn create(
    root: &SessionRoot,
    namespace: &Namespace,
    name: &SessionName,
) -> Result<SessionDir, SessionError> {
    let ns_path = root.path().join(namespace.as_str());
    let path = ns_path.join(name.as_str());
    // Ensure namespace directory exists
    fs::create_dir_all(&ns_path)?;
    // Atomic: create_dir fails if already exists, no race window
    match fs::create_dir(&path) {
        Ok(()) => Ok(SessionDir {
            path,
            name: name.clone(),
        }),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(SessionError::AlreadyExists(name.to_string()))
        }
        Err(e) => Err(SessionError::Io(e)),
    }
}

/// Open an existing session directory. Returns None if it doesn't exist.
/// Returns Corrupt if the path exists but is not a directory, or if
/// the directory exists but has no meta.json (newly created dirs
/// that haven't had meta written yet are not valid open targets).
/// Path: `root/<namespace>/<session>/`
pub fn open(
    root: &SessionRoot,
    namespace: &Namespace,
    name: &SessionName,
) -> Result<Option<SessionDir>, SessionError> {
    let path = root.path().join(namespace.as_str()).join(name.as_str());
    if !path.exists() {
        return Ok(None);
    }
    if !path.is_dir() {
        return Err(SessionError::Corrupt {
            session: name.to_string(),
            reason: "not a directory".into(),
        });
    }
    let meta_path = path.join("meta.json");
    if !meta_path.exists() {
        return Err(SessionError::Corrupt {
            session: name.to_string(),
            reason: "meta.json not found".into(),
        });
    }
    Ok(Some(SessionDir {
        path,
        name: name.clone(),
    }))
}

/// Open a session directory without requiring meta.json.
/// Used internally by the sidecar before it writes meta.
/// Returns NotFound if the directory doesn't exist.
/// Path: `root/<namespace>/<session>/`
pub fn open_raw(
    root: &SessionRoot,
    namespace: &Namespace,
    name: &SessionName,
) -> Result<SessionDir, SessionError> {
    let path = root.path().join(namespace.as_str()).join(name.as_str());
    if !path.exists() {
        return Err(SessionError::NotFound(name.to_string()));
    }
    if !path.is_dir() {
        return Err(SessionError::Corrupt {
            session: name.to_string(),
            reason: "not a directory".into(),
        });
    }
    Ok(SessionDir {
        path,
        name: name.clone(),
    })
}

/// List session directory names, optionally filtered by namespace.
///
/// - `Some(ns)`: list sessions in `root/<ns>/`
/// - `None`: iterate all namespace dirs under root, then sessions within each
///
/// Returns only directories with valid session names.
/// Non-directory entries and invalid names (hidden files, underscore-prefixed)
/// are silently skipped — these are not sessions.
/// Directories with valid names but missing/corrupt meta.json ARE included —
/// use `open()` or `read_meta()` to distinguish healthy from corrupt sessions.
pub fn list(
    root: &SessionRoot,
    namespace: Option<&Namespace>,
) -> Result<Vec<(Namespace, SessionName)>, SessionError> {
    let root_path = root.path();
    if !root_path.exists() {
        return Ok(vec![]);
    }

    let namespaces: Vec<Namespace> = match namespace {
        Some(ns) => vec![ns.clone()],
        None => {
            let mut ns_list = Vec::new();
            for entry in fs::read_dir(root_path)? {
                let entry = entry?;
                if !entry.path().is_dir() {
                    continue;
                }
                if let Some(name_str) = entry.file_name().to_str() {
                    if let Ok(ns) = Namespace::new(name_str) {
                        ns_list.push(ns);
                    }
                }
            }
            ns_list.sort_by(|a, b| a.as_str().cmp(b.as_str()));
            ns_list
        }
    };

    let mut sessions = Vec::new();
    for ns in &namespaces {
        let ns_path = root_path.join(ns.as_str());
        if !ns_path.exists() {
            continue;
        }
        for entry in fs::read_dir(&ns_path)? {
            let entry = entry?;
            if !entry.path().is_dir() {
                continue;
            }
            if let Some(name_str) = entry.file_name().to_str() {
                if let Ok(name) = SessionName::new(name_str) {
                    sessions.push((ns.clone(), name));
                }
            }
        }
    }
    sessions.sort_by(|a, b| (&a.0.as_str(), a.1.as_str()).cmp(&(&b.0.as_str(), b.1.as_str())));
    Ok(sessions)
}

/// Read meta.json from a session directory.
/// Returns Corrupt if meta.json is missing or contains invalid JSON.
pub fn read_meta(session: &SessionDir) -> Result<Meta, SessionError> {
    let meta_path = session.meta_path();
    if !meta_path.exists() {
        return Err(SessionError::Corrupt {
            session: session.name().to_string(),
            reason: "meta.json not found".into(),
        });
    }
    let content = fs::read_to_string(&meta_path)?;
    let meta: Meta = serde_json::from_str(&content).map_err(|e| SessionError::Corrupt {
        session: session.name().to_string(),
        reason: format!("invalid meta.json: {e}"),
    })?;
    Ok(meta)
}

/// Write meta.json atomically via temp file + rename.
/// Crash-safe: write tmp → fsync tmp → rename → fsync parent dir.
/// Never leaves partial JSON behind.
pub fn write_meta_atomic(session: &SessionDir, meta: &Meta) -> Result<(), SessionError> {
    let tmp_path = session.meta_tmp_path();
    let meta_path = session.meta_path();

    let json = serde_json::to_string_pretty(meta)?;

    // Write and fsync the temp file
    let mut file = File::create(&tmp_path)?;
    file.write_all(json.as_bytes())?;
    file.sync_all()?;
    drop(file);

    // Atomic rename
    fs::rename(&tmp_path, &meta_path)?;

    // Fsync the parent directory to ensure the directory entry update is durable.
    // Without this, the rename could be lost on power failure.
    // On Windows, FlushFileBuffers on a directory handle returns ACCESS_DENIED,
    // and NTFS metadata updates from rename are journal-durable without explicit fsync.
    #[cfg(unix)]
    {
        let dir = File::open(session.path())?;
        dir.sync_all()?;
    }

    Ok(())
}

/// Check if the session lock is currently held by another process.
/// Returns true if locked, false if available. Does not acquire.
pub fn is_locked(session: &SessionDir) -> Result<bool, SessionError> {
    let lock_path = session.lock_path();
    if !lock_path.exists() {
        return Ok(false);
    }
    match LockGuard::try_acquire(session) {
        Ok(_guard) => Ok(false), // We got it -> was not locked. Drop releases.
        Err(SessionError::Locked(_)) => Ok(true),
        Err(e) => Err(e),
    }
}

/// How long commands that report a session as finished (`wait`, `kill`,
/// `start --replace`) wait for its sidecar to release the session lock after
/// terminal metadata appears (#91). The gap is normally milliseconds; the bound
/// keeps a stuck sidecar from turning those commands into hangs.
pub const RELEASE_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Poll until nothing holds the session lock, or `bound` elapses. Returns
/// `Ok(true)` once the lock is free, `Ok(false)` if `bound` passed first.
///
/// The sidecar writes terminal metadata just before it releases the lock, so a
/// session can be terminal and still locked for a moment. Callers that already
/// know the session is terminal use this before reporting it done.
pub fn wait_unlocked(
    session: &SessionDir,
    bound: std::time::Duration,
) -> Result<bool, SessionError> {
    let deadline = std::time::Instant::now() + bound;
    loop {
        if !is_locked(session)? {
            return Ok(true);
        }
        if std::time::Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Exclusive file lock on a session. Drop releases the lock.
/// Uses flock — exclusive across processes, not just threads.
#[cfg(unix)]
#[derive(Debug)]
pub struct LockGuard {
    _file: File,
}

#[cfg(unix)]
impl LockGuard {
    /// Try to acquire an exclusive lock. Returns Locked if held by another process.
    pub fn try_acquire(session: &SessionDir) -> Result<Self, SessionError> {
        use std::os::unix::io::AsRawFd;

        let lock_path = session.lock_path();
        let file = File::create(&lock_path)?;

        // SAFETY: file is an open File, so as_raw_fd() returns a valid fd.
        // LOCK_EX | LOCK_NB is a valid flock operation (non-blocking exclusive).
        // flock may fail (EWOULDBLOCK) but won't cause UB.
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                return Err(SessionError::Locked(session.name().to_string()));
            }
            return Err(SessionError::Io(err));
        }

        Ok(Self { _file: file })
    }

    /// Blocking acquire — waits until lock is available.
    pub fn acquire(session: &SessionDir) -> Result<Self, SessionError> {
        use std::os::unix::io::AsRawFd;

        let lock_path = session.lock_path();
        let file = File::create(&lock_path)?;

        // SAFETY: file is an open File, so as_raw_fd() returns a valid fd.
        // LOCK_EX is a valid flock operation (blocking exclusive lock).
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if ret != 0 {
            return Err(SessionError::Io(std::io::Error::last_os_error()));
        }

        Ok(Self { _file: file })
    }
}

/// Exclusive file lock on a session (Windows).
/// Uses LockFileEx for cross-process exclusion.
#[cfg(windows)]
#[derive(Debug)]
pub struct LockGuard {
    _file: File,
}

#[cfg(windows)]
impl LockGuard {
    /// Try to acquire an exclusive lock. Returns Locked if held by another process.
    pub fn try_acquire(session: &SessionDir) -> Result<Self, SessionError> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION;
        use windows_sys::Win32::Storage::FileSystem::{
            LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
        };

        let lock_path = session.lock_path();
        let file = File::create(&lock_path)?;
        let handle = file.as_raw_handle() as _;

        let mut overlapped = unsafe { std::mem::zeroed() };
        let ret = unsafe {
            LockFileEx(
                handle,
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                1,
                0,
                &mut overlapped,
            )
        };
        if ret == 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
                return Err(SessionError::Locked(session.name().to_string()));
            }
            return Err(SessionError::Io(err));
        }
        Ok(Self { _file: file })
    }

    /// Blocking acquire — waits until lock is available.
    pub fn acquire(session: &SessionDir) -> Result<Self, SessionError> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{LOCKFILE_EXCLUSIVE_LOCK, LockFileEx};

        let lock_path = session.lock_path();
        let file = File::create(&lock_path)?;
        let handle = file.as_raw_handle() as _;

        let mut overlapped = unsafe { std::mem::zeroed() };
        let ret = unsafe { LockFileEx(handle, LOCKFILE_EXCLUSIVE_LOCK, 0, 1, 0, &mut overlapped) };
        if ret == 0 {
            return Err(SessionError::Io(std::io::Error::last_os_error()));
        }
        Ok(Self { _file: file })
    }
}
