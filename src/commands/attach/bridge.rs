//! The remote end of `attach --host`, from a Unix or a Windows client:
//! `tendr _remote` with an attach request.
//!
//! The client keeps the terminal (raw mode, resizes, the detach escape) and
//! speaks the attach protocol on ssh's stdin and stdout. This end connects
//! those frames to the session's attach socket. It decides nothing about
//! control: the sidecar grants and retires controllers and records the run.
//! The bridge only reports what the session's event log already says, when a
//! takeover cut the connection before the sidecar could tell the client.
//!
//! Each direction has its own thread, so a client that stops reading its
//! output holds up only the output; its resizes and its detach still reach
//! the session, and a takeover by another client never waits for it.

use tendr::attach_proto::{ResumePoint, Version};
use tendr::model::ids::Namespace;

/// `tendr _remote` for an attach request: check the session as a local
/// attach does, read the client's hello from stdin, connect to the session's
/// attach socket with its mode, speaking `version`, then pass the sidecar's
/// answer to the client and bridge frames until either side ends.
///
/// `version` is the one the request agreed. The client's hello is always a
/// version 1 hello, so that a remote tendr older than the agreement takes it
/// too; this end speaks the agreed version to the sidecar, or version 1 to a
/// sidecar started by a tendr that predates it ([`super::connect`]). The
/// answer's type tells the client which.
///
/// A request with `resume` is followed by the same resume frame in place of
/// the hello, which this end passes on to the sidecar. It skips the
/// human-control check, since after a lost connection `meta.json` still
/// names the resuming client's own control, and the sidecar decides. A
/// sidecar that cannot resume refuses it, and this end exits 80 saying so:
/// it never attaches afresh instead.
///
/// A `read_only` request is a watcher's, which never holds or changes
/// control, so it skips the human-control check too. Its hello must be a
/// watch hello, answered `Watching`. Its resume must be a watch resume
/// ([`Frame::WatchResume`]), built from `resume`'s run and offset only: the
/// epoch the request carries is ignored, and a plain resume frame after a
/// read-only request is refused (80), so nothing a watcher sends can reach
/// the control rule. When the session closes a watcher's connection, the
/// run ended (0) or the session cut it off (84), as `meta.json` here says;
/// the client has no other way to tell.
///
/// Its exit code is that of a local attach: the checks' and the sidecar's
/// refusals keep their codes, and a client taken over exits 81 (after the
/// `MSG_RETIRED` that tells it so).
///
/// # Errors
/// The local attach's errors, a request not followed by its hello or resume
/// frame (80), a session that cannot resume (80), and a watcher cut off
/// (84).
pub fn cmd_attach_bridge(
    name: &str,
    namespace: &Namespace,
    takeover: bool,
    read_only: bool,
    version: Version,
    resume: Option<ResumePoint>,
) -> anyhow::Result<()> {
    let (session, meta) =
        super::attachable(name, namespace, takeover || read_only || resume.is_some())?;

    #[cfg(not(unix))]
    {
        let _ = (session, meta, version, resume);
        anyhow::bail!("attach is only supported on Unix");
    }

    #[cfg(unix)]
    {
        use super::Point;
        use tendr::attach_proto::{self, Frame, WatchPoint};
        use tendr::pty_exit::PtyExitCode;

        let first = attach_proto::read_frame(&mut std::io::stdin().lock());
        let resume = resume.map(|point| {
            if read_only {
                Point::Watch(WatchPoint {
                    run_id: point.run_id,
                    offset: point.offset,
                })
            } else {
                Point::Control(point)
            }
        });
        let (stream, answer, epoch) = match resume {
            None => {
                let mode = super::mode(takeover, read_only);
                match first {
                    Ok(Frame::Hello { mode: hello, .. }) if hello == mode => {}
                    other => {
                        return Err(PtyExitCode::Protocol.error(anyhow::anyhow!(
                            "the attach request must be followed by its {mode:?} hello, not {other:?}"
                        )));
                    }
                }
                let (stream, accept) = super::connect(session.path(), version, mode)?;
                (stream, accept.frame(), accept.epoch())
            }
            Some(point) => {
                match first {
                    Ok(asked) if asked == point.resume_frame() => {}
                    other => {
                        return Err(PtyExitCode::Protocol.error(anyhow::anyhow!(
                            "the resume request must be followed by its resume frame, not {other:?}"
                        )));
                    }
                }
                let (stream, resumed) = super::connect_resume(session.path(), point)?;
                let epoch = match point {
                    Point::Control(_) => Some(resumed.point().epoch),
                    Point::Watch(_) => None,
                };
                (stream, resumed.frame(), epoch)
            }
        };
        // A resumed client's control is its new epoch: a later takeover of
        // it grants the one after. A watcher has none to be taken over.
        let control = epoch.map(|epoch| super::Control {
            session_dir: session.path().to_path_buf(),
            run_id: meta.run_id(),
            epoch,
        });
        match unix::run(stream, answer, control) {
            // The client has been told, and says so itself.
            unix::End::TakenOver => std::process::exit(PtyExitCode::Control.code()),
            // The session ends a watcher's connection only at the run's end
            // or to cut it off; only this end can read which.
            unix::End::Closed if read_only => super::run_ended(&session, meta.run_id()),
            unix::End::Closed | unix::End::Left => Ok(()),
        }
    }
}

#[cfg(unix)]
mod unix {
    use std::io::{self, BufReader, BufWriter};
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    use tendr::attach_proto::{self, Frame, FrameError};

    use super::super::Control;
    use super::super::client::{DETACH_GRACE, Outbox, spawn_sender};

    /// How often the bridge checks whether the client has hung up while
    /// both directions are blocked.
    const HANGUP_POLL: Duration = Duration::from_millis(200);

    /// How the bridge ended.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum End {
        /// The client left: it detached, closed its input or hung up.
        Left,
        /// The session closed the connection: its run ended.
        Closed,
        /// Another client took this one over, and this one has been told.
        TakenOver,
    }

    /// Pass the sidecar's acceptance on to the client, then bridge until
    /// either side ends. `control` is `None` for a watcher, which holds no
    /// control, so no takeover check runs for it.
    pub(super) fn run(stream: UnixStream, accepted: Frame, control: Option<Control>) -> End {
        if attach_proto::write_frame(&mut io::stdout().lock(), &accepted).is_err() {
            return End::Left;
        }
        let (Ok(to_session), Ok(from_session)) = (stream.try_clone(), stream.try_clone()) else {
            return End::Left;
        };
        let (ends, ended) = mpsc::channel();
        let outbox = Arc::new(Outbox::default());
        // A failed write to the socket means the session closed it: the
        // output direction reports that end, a takeover included.
        spawn_sender(
            BufWriter::new(to_session),
            Arc::clone(&outbox),
            Arc::new(AtomicBool::new(false)),
        );
        {
            let (outbox, ends) = (Arc::clone(&outbox), ends.clone());
            std::thread::spawn(move || {
                if forward_input(&outbox) {
                    let _ = ends.send(End::Left);
                }
            });
        }
        std::thread::spawn(move || {
            let _ = ends.send(forward_output(from_session, control.as_ref()));
        });

        let end = wait_end(&ended);
        // The session treats a closed connection as a detach.
        let _ = stream.shutdown(std::net::Shutdown::Both);
        outbox.close();
        end
    }

    /// Client → session: input waits behind controls, and the client's
    /// detach is passed on before the connection closes. `true` if the client
    /// ended its input; `false` if the outbox closed under it because the
    /// session closed the connection, which the output direction reports.
    fn forward_input(outbox: &Outbox) -> bool {
        let mut input = io::stdin().lock();
        loop {
            match attach_proto::read_frame(&mut input) {
                Ok(Frame::Data(bytes)) => {
                    if !outbox.push_data_waiting(bytes) {
                        return false;
                    }
                }
                Ok(frame @ (Frame::Resize(_) | Frame::Repaint)) => outbox.push_control(frame),
                Ok(Frame::Detach) => {
                    outbox.push_control(Frame::Detach);
                    outbox.wait_controls_sent(DETACH_GRACE);
                    return true;
                }
                // Frames a client does not send, and unknown or malformed
                // ones, are ignored, as the sidecar ignores them.
                Ok(_) | Err(FrameError::Malformed(_) | FrameError::Unknown(_)) => {}
                Err(FrameError::Io(_)) => return true,
            }
        }
    }

    /// Session → client, one whole frame at a time, so a client that stops
    /// reading stalls this direction and, behind it, the sidecar's queue for
    /// this viewer, exactly as a local client would. A frame the sidecar cut
    /// off is not passed on. When the session ends the connection without
    /// `MSG_RETIRED` because this client was taken over, the event log says
    /// so, and the client is told as the sidecar would have told it.
    fn forward_output(socket: UnixStream, control: Option<&Control>) -> End {
        let mut socket = BufReader::new(socket);
        let mut retired = false;
        loop {
            match attach_proto::read_frame(&mut socket) {
                Ok(frame) => {
                    retired |= matches!(frame, Frame::Retired(_));
                    if attach_proto::write_frame(&mut io::stdout().lock(), &frame).is_err() {
                        return End::Left;
                    }
                }
                Err(FrameError::Malformed(_) | FrameError::Unknown(_)) => {}
                Err(FrameError::Io(_)) => break,
            }
        }
        if retired {
            return End::TakenOver;
        }
        match control.and_then(Control::taken_over_by) {
            Some(successor) => {
                if attach_proto::write_frame(&mut io::stdout().lock(), &Frame::Retired(successor))
                    .is_err()
                {
                    return End::Left;
                }
                End::TakenOver
            }
            None => End::Closed,
        }
    }

    /// The first end either direction reports. A client that hangs up while
    /// both directions are blocked (its input behind a full queue, its
    /// output waiting for the session) is noticed here; the input direction
    /// still gets a moment to pass on a detach that came just before.
    fn wait_end(ended: &mpsc::Receiver<End>) -> End {
        loop {
            match ended.recv_timeout(HANGUP_POLL) {
                Ok(end) => return end,
                Err(mpsc::RecvTimeoutError::Disconnected) => return End::Left,
                Err(mpsc::RecvTimeoutError::Timeout) if client_hung_up() => {
                    return ended.recv_timeout(DETACH_GRACE).unwrap_or(End::Left);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }

    /// Whether the client's end of stdin or stdout has closed, checked
    /// without blocking or reading.
    fn client_hung_up() -> bool {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        let (stdin, stdout) = (io::stdin(), io::stdout());
        let mut fds = [
            PollFd::new(&stdin, PollFlags::empty()),
            PollFd::new(&stdout, PollFlags::empty()),
        ];
        let now = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        matches!(poll(&mut fds, Some(&now)), Ok(n) if n > 0)
            && fds
                .iter()
                .any(|fd| fd.revents().intersects(PollFlags::HUP | PollFlags::ERR))
    }
}
