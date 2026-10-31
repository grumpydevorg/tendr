//! Private, peer-verified attach sockets (Unix).
//!
//! A PTY session's attach endpoint must stay within the run owner's account.
//! The socket lives in `sockets/` under Tendr's persistent state root
//! (`~/.tendr/sockets`), an owner-only directory that survives logout, with a
//! short name derived from the session and run. Both ends verify the peer's
//! user id before any terminal bytes flow. Described in
//! `docs/architecture/04-pty-lane.md` ("The attach socket").

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Instant;

use thiserror::Error;

use crate::model::ids::RunId;

/// Directory, under the state root, that holds attach sockets.
pub const SOCKET_DIR: &str = "sockets";

#[derive(Debug, Error)]
pub enum SocketError {
    #[error("cannot locate the tendr state root for {0}")]
    NoStateRoot(PathBuf),
    #[error("socket directory {path} is not a private directory owned by this user: {reason}")]
    UnsafeDirectory { path: PathBuf, reason: &'static str },
    #[error("socket path {path} is {len} bytes; this platform allows at most {max}")]
    PathTooLong {
        path: PathBuf,
        len: usize,
        max: usize,
    },
    #[error("socket path {0} already exists; refusing to adopt or remove it")]
    PathExists(PathBuf),
    #[error("attach socket I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// A bound, private attach listener and where it lives.
pub struct BoundSocket {
    pub listener: UnixListener,
    pub path: PathBuf,
}

/// The state root (`…/.tendr`) for a session directory
/// (`…/.tendr/sessions/<namespace>/<name>`).
#[must_use]
pub fn state_root_for(session_dir: &Path) -> Option<PathBuf> {
    session_dir
        .ancestors()
        .find(|p| p.file_name().is_some_and(|name| name == "sessions"))
        .and_then(Path::parent)
        .map(Path::to_path_buf)
}

/// Create `dir` as an owner-only directory, or verify an existing one: not a
/// symlink, a directory, owned by the effective user. An existing directory we
/// own with looser permissions is tightened to `0700`.
///
/// # Errors
///
/// [`SocketError::UnsafeDirectory`] or [`SocketError::Io`].
pub fn prepare_private_dir(dir: &Path) -> Result<(), SocketError> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    let io_err = |source| SocketError::Io {
        path: dir.to_path_buf(),
        source,
    };
    let unsafe_dir = |reason| SocketError::UnsafeDirectory {
        path: dir.to_path_buf(),
        reason,
    };

    match std::fs::symlink_metadata(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = dir.parent() {
                std::fs::create_dir_all(parent).map_err(io_err)?;
            }
            if let Err(e) = std::fs::DirBuilder::new().mode(0o700).create(dir) {
                // A concurrent creator won; whatever exists is verified below.
                if e.kind() != io::ErrorKind::AlreadyExists {
                    return Err(io_err(e));
                }
            }
        }
        Err(e) => return Err(io_err(e)),
        Ok(_) => {}
    }

    let meta = std::fs::symlink_metadata(dir).map_err(io_err)?;
    if meta.file_type().is_symlink() {
        return Err(unsafe_dir("it is a symbolic link"));
    }
    if !meta.is_dir() {
        return Err(unsafe_dir("it is not a directory"));
    }
    if meta.uid() != rustix::process::geteuid().as_raw() {
        return Err(unsafe_dir("it is owned by another user"));
    }
    if meta.mode() & 0o777 != 0o700 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(io_err)?;
    }
    Ok(())
}

/// The socket path for a run, checked against the platform's `sun_path` limit.
///
/// # Errors
///
/// [`SocketError::PathTooLong`].
pub fn socket_path(
    state_root: &Path,
    session_dir: &Path,
    run_id: RunId,
) -> Result<PathBuf, SocketError> {
    use sha2::{Digest, Sha256};
    use std::os::unix::ffi::OsStrExt;

    let mut hasher = Sha256::new();
    hasher.update(session_dir.as_os_str().as_bytes());
    hasher.update(run_id.as_uuid().as_bytes());
    let name: String = hasher.finalize()[..6]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let path = state_root.join(SOCKET_DIR).join(format!("{name}.sock"));

    // `sun_path` must hold the path plus its NUL terminator.
    // SAFETY: sockaddr_un is plain old data; all-zero is a valid value.
    let addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let max = std::mem::size_of_val(&addr.sun_path) - 1;
    let len = path.as_os_str().as_bytes().len();
    if len > max {
        return Err(SocketError::PathTooLong { path, len, max });
    }
    Ok(path)
}

/// Bind the run's private attach socket and publish its breadcrumb atomically.
/// Nothing is advertised unless every step succeeds.
///
/// # Errors
///
/// Any [`SocketError`]; the caller must not start the PTY session.
pub fn bind_for_session(session_dir: &Path, run_id: RunId) -> Result<BoundSocket, SocketError> {
    use std::os::unix::fs::PermissionsExt;

    let root = state_root_for(session_dir)
        .ok_or_else(|| SocketError::NoStateRoot(session_dir.to_path_buf()))?;
    prepare_private_dir(&root.join(SOCKET_DIR))?;
    let path = socket_path(&root, session_dir, run_id)?;
    // Never adopt or remove what is already there. `bind` below also refuses an
    // existing path, which closes the race after this check.
    if std::fs::symlink_metadata(&path).is_ok() {
        return Err(SocketError::PathExists(path));
    }
    let io_err = |path: &Path, source| SocketError::Io {
        path: path.to_path_buf(),
        source,
    };
    let listener = UnixListener::bind(&path).map_err(|e| {
        if e.kind() == io::ErrorKind::AddrInUse {
            SocketError::PathExists(path.clone())
        } else {
            io_err(&path, e)
        }
    })?;

    let published = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| io_err(&path, e))
        .and_then(|()| publish_breadcrumb(session_dir, &path));
    if let Err(e) = published {
        let _ = std::fs::remove_file(&path); // ours: just bound
        return Err(e);
    }
    Ok(BoundSocket { listener, path })
}

/// Atomically point the session's breadcrumb at `socket`.
fn publish_breadcrumb(session_dir: &Path, socket: &Path) -> Result<(), SocketError> {
    use std::os::unix::ffi::OsStrExt;

    let breadcrumb = session_dir.join("a.sock.path");
    let tmp = session_dir.join("a.sock.path.tmp");
    std::fs::write(&tmp, socket.as_os_str().as_bytes())
        .and_then(|()| std::fs::rename(&tmp, &breadcrumb))
        .map_err(|source| SocketError::Io {
            path: breadcrumb,
            source,
        })
}

/// The effective user id of the process on the other end of `stream`.
///
/// # Errors
///
/// The credential query's OS error.
pub fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();

    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut cred = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = libc::socklen_t::try_from(std::mem::size_of::<libc::ucred>())
            .expect("ucred size fits socklen_t");
        // SAFETY: `fd` is an open socket owned by `stream` for this call; `cred`
        // and `len` describe a writable ucred buffer of the stated size.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut cred).cast(),
                &mut len,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(cred.uid)
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        // SAFETY: `fd` is an open socket owned by `stream` for this call; `uid`
        // and `gid` are valid out-pointers.
        let rc = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(uid)
    }
}

/// Whether the peer is this process's effective user.
///
/// # Errors
///
/// `PermissionDenied` for a different user, or the credential query's error.
pub fn verify_peer(stream: &UnixStream) -> io::Result<()> {
    let peer = peer_uid(stream)?;
    let me = rustix::process::geteuid().as_raw();
    if peer == me {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("attach peer is user {peer}, not {me}"),
        ))
    }
}

/// Whether the peer has closed its end, checked without blocking or consuming
/// data. Reports a closed peer even if its unread frames are still queued, so a
/// client that sent input and then vanished is noticed while the sidecar is busy
/// waiting on the PTY rather than reading.
#[must_use]
pub fn peer_hung_up(stream: &UnixStream) -> bool {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};

    let mut fds = [PollFd::new(stream, PollFlags::IN)];
    let now = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // A socket error counts as hung up: the connection is unusable either way.
    matches!(poll(&mut fds, Some(&now)), Ok(n) if n > 0)
        && fds[0].revents().intersects(PollFlags::HUP | PollFlags::ERR)
}

/// Reads from a socket under one overall deadline, so a client that trickles
/// bytes cannot extend a handshake indefinitely.
pub struct DeadlineReader<'a> {
    pub stream: &'a UnixStream,
    pub deadline: Instant,
}

impl io::Read for DeadlineReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.stream.set_read_timeout(Some(remaining))?;
        io::Read::read(&mut &*self.stream, buf)
    }
}

/// How old an unnamed socket must be before [`sweep_orphans`] removes it: far
/// longer than a bind takes to publish its breadcrumb.
pub const ORPHAN_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

/// Remove attach sockets that a sidecar killed outright left behind in
/// `state_root/sockets` (only a sidecar that ends normally removes its own), or
/// with `dry_run` only report them. Returns the orphans' paths.
///
/// A socket is removed only if every check passes, so a socket a live session
/// could still be using is never touched:
///
/// - it is a socket owned by the effective user, named like a run's socket;
/// - no session breadcrumb under `state_root/sessions` names it, live or not
///   (a crashed session keeps its socket until the session is pruned or
///   replaced);
/// - it is older than [`ORPHAN_GRACE`], which covers the moment between a
///   bind and its breadcrumb;
/// - connecting to it is refused: nothing listens on it.
///
/// # Errors
///
/// The socket directory is unsafe, or the sessions or a breadcrumb cannot be
/// read, so an unnamed socket cannot be told from a live one. Nothing is
/// removed then.
pub fn sweep_orphans(state_root: &Path, dry_run: bool) -> io::Result<Vec<PathBuf>> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let dir = state_root.join(SOCKET_DIR);
    let meta = match std::fs::symlink_metadata(&dir) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let euid = rustix::process::geteuid().as_raw();
    if !meta.is_dir() || meta.uid() != euid {
        return Err(io::Error::other(format!(
            "{} is not a directory owned by this user",
            dir.display()
        )));
    }
    let named = breadcrumb_names(&state_root.join("sessions"))?;

    let mut orphans = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if !is_run_socket_name(&name) || named.contains(&name) {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let old_enough = meta
            .modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age >= ORPHAN_GRACE);
        if !meta.file_type().is_socket() || meta.uid() != euid || !old_enough {
            continue;
        }
        let refused = matches!(
            UnixStream::connect(&path),
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused
        );
        if !refused {
            continue;
        }
        if dry_run || std::fs::remove_file(&path).is_ok() {
            orphans.push(path);
        }
    }
    Ok(orphans)
}

/// `<12 hex digits>.sock`, the shape [`socket_path`] gives a run's socket.
fn is_run_socket_name(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .and_then(|n| n.strip_suffix(".sock"))
        .is_some_and(|stem| stem.len() == 12 && stem.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The file names every session breadcrumb under `sessions` points at.
fn breadcrumb_names(sessions: &Path) -> io::Result<std::collections::HashSet<std::ffi::OsString>> {
    use std::os::unix::ffi::OsStrExt;

    let read_dir = |dir: &Path| match std::fs::read_dir(dir) {
        Ok(entries) => Ok(Some(entries)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    };
    let mut names = std::collections::HashSet::new();
    let Some(namespaces) = read_dir(sessions)? else {
        return Ok(names);
    };
    for namespace in namespaces {
        let namespace = namespace?.path();
        if !namespace.is_dir() {
            continue;
        }
        let Some(sessions) = read_dir(&namespace)? else {
            continue;
        };
        for session in sessions {
            let breadcrumb = session?.path().join("a.sock.path");
            match std::fs::read(&breadcrumb) {
                Ok(bytes) => {
                    if let Some(name) = Path::new(std::ffi::OsStr::from_bytes(&bytes)).file_name() {
                        names.insert(name.to_owned());
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::time::Duration;

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }

    fn fake_session(root: &Path) -> PathBuf {
        let dir = root.join(".tendr/sessions/default/work");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn state_root_is_the_parent_of_the_sessions_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let session = fake_session(tmp.path());
        assert_eq!(state_root_for(&session), Some(tmp.path().join(".tendr")));
        assert_eq!(state_root_for(Path::new("/no/session/dir/here")), None);
    }

    #[test]
    fn private_dir_is_created_owner_only() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sockets");
        prepare_private_dir(&dir).unwrap();
        let meta = std::fs::symlink_metadata(&dir).unwrap();
        assert!(meta.is_dir());
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(meta.uid(), rustix::process::geteuid().as_raw());
    }

    #[test]
    fn an_existing_loose_dir_we_own_is_tightened() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sockets");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        prepare_private_dir(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn a_symlinked_socket_dir_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let dir = tmp.path().join("sockets");
        std::os::unix::fs::symlink(&elsewhere, &dir).unwrap();
        assert!(matches!(
            prepare_private_dir(&dir),
            Err(SocketError::UnsafeDirectory { .. })
        ));
    }

    #[test]
    fn socket_path_is_short_stable_per_run_and_distinct_across_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let session = fake_session(tmp.path());
        let root = tmp.path().join(".tendr");
        let run = RunId::new();
        let a = socket_path(&root, &session, run).unwrap();
        assert_eq!(a, socket_path(&root, &session, run).unwrap());
        assert_ne!(a, socket_path(&root, &session, RunId::new()).unwrap());
        assert_eq!(a.parent().unwrap(), root.join(SOCKET_DIR));
        assert_eq!(a.extension().unwrap(), "sock");
    }

    #[test]
    fn an_overlong_parent_directory_is_an_error_not_a_truncation() {
        let tmp = tempfile::tempdir().unwrap();
        let session = fake_session(tmp.path());
        let root = tmp.path().join("x".repeat(120));
        assert!(matches!(
            socket_path(&root, &session, RunId::new()),
            Err(SocketError::PathTooLong { .. })
        ));
    }

    #[test]
    fn bind_publishes_a_private_socket_and_its_breadcrumb() {
        // Short root: tempdir paths on macOS already use most of `sun_path`.
        let tmp = tempfile::Builder::new()
            .prefix("ts")
            .tempdir_in("/tmp")
            .unwrap();
        let session = fake_session(tmp.path());
        let bound = bind_for_session(&session, RunId::new()).unwrap();

        assert_eq!(mode(bound.path.parent().unwrap()), 0o700);
        assert_eq!(mode(&bound.path), 0o600);
        let breadcrumb = std::fs::read_to_string(session.join("a.sock.path")).unwrap();
        assert_eq!(PathBuf::from(breadcrumb.trim()), bound.path);
        assert!(!session.join("a.sock.path.tmp").exists());
    }

    #[test]
    fn bind_refuses_to_adopt_an_existing_path() {
        let tmp = tempfile::Builder::new()
            .prefix("ts")
            .tempdir_in("/tmp")
            .unwrap();
        let session = fake_session(tmp.path());
        let run = RunId::new();
        let root = state_root_for(&session).unwrap();
        prepare_private_dir(&root.join(SOCKET_DIR)).unwrap();
        let path = socket_path(&root, &session, run).unwrap();
        std::fs::write(&path, b"foreign").unwrap();

        assert!(matches!(
            bind_for_session(&session, run),
            Err(SocketError::PathExists(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"foreign", "never removed");
    }

    #[test]
    fn peer_uid_of_a_local_pair_is_this_user() {
        let (a, _b) = UnixStream::pair().unwrap();
        assert_eq!(peer_uid(&a).unwrap(), rustix::process::geteuid().as_raw());
        verify_peer(&a).unwrap();
    }

    #[test]
    fn peer_hung_up_tracks_the_peer_without_consuming_data() {
        let (a, mut b) = UnixStream::pair().unwrap();
        assert!(!peer_hung_up(&a), "an idle live peer");

        b.write_all(b"queued").unwrap();
        assert!(!peer_hung_up(&a), "a live peer with unread data");

        drop(b);
        assert!(
            peer_hung_up(&a),
            "a closed peer, even with unread data queued"
        );

        let mut rest = Vec::new();
        (&a).read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"queued", "checking consumed nothing");
        assert!(peer_hung_up(&a), "a closed peer after draining");
    }

    #[test]
    fn deadline_reader_stops_a_trickling_peer_at_the_overall_deadline() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let trickle = std::thread::spawn(move || {
            for _ in 0..20 {
                if b.write_all(&[0]).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(40));
            }
        });
        let started = Instant::now();
        let mut reader = DeadlineReader {
            stream: &a,
            deadline: started + Duration::from_millis(200),
        };
        let mut buf = [0u8; 20];
        let err = reader.read_exact(&mut buf).unwrap_err();
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ),
            "unexpected error {err}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(600),
            "the deadline is overall, not per read: {:?}",
            started.elapsed()
        );
        drop(a);
        let _ = trickle.join();
    }
}
