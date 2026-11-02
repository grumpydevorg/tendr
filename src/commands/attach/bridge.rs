//! The remote end of `attach --host` from a Unix client: `tendr _remote`
//! with an attach request.
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

use tendr::model::ids::Namespace;

/// `tendr _remote` for an attach request: check the session as a local
/// attach does, read the client's hello from stdin, connect to the session's
/// attach socket with it, then bridge frames until either side ends.
///
/// Its exit code is that of a local attach: the checks' and the sidecar's
/// refusals keep their codes, and a client taken over exits 81 (after the
/// `MSG_RETIRED` that tells it so).
///
/// # Errors
/// The local attach's errors, and a request not followed by its hello (80).
pub fn cmd_attach_bridge(name: &str, namespace: &Namespace, takeover: bool) -> anyhow::Result<()> {
    let (session, meta) = super::attachable(name, namespace, takeover)?;

    #[cfg(not(unix))]
    {
        let _ = (session, meta);
        anyhow::bail!("attach is only supported on Unix");
    }

    #[cfg(unix)]
    {
        use tendr::attach_proto::{self, Frame};
        use tendr::pty_exit::PtyExitCode;

        let mode = super::mode(takeover);
        match attach_proto::read_frame(&mut std::io::stdin().lock()) {
            Ok(Frame::Hello(hello)) if hello == mode => {}
            other => {
                return Err(PtyExitCode::Protocol.error(anyhow::anyhow!(
                    "the attach request must be followed by its {mode:?} hello, not {other:?}"
                )));
            }
        }
        let (stream, epoch) = super::connect(session.path(), mode)?;
        let control = super::Control {
            session_dir: session.path().to_path_buf(),
            run_id: meta.run_id(),
            epoch,
        };
        if unix::run(stream, control) == unix::End::TakenOver {
            // The client has been told, and says so itself.
            std::process::exit(PtyExitCode::Control.code());
        }
        Ok(())
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
    use super::super::unix_relay::{DETACH_GRACE, Outbox, spawn_sender};

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

    /// Tell the client it has control, then bridge until either side ends.
    pub(super) fn run(stream: UnixStream, control: Control) -> End {
        if attach_proto::write_frame(&mut io::stdout().lock(), &Frame::Accepted(control.epoch))
            .is_err()
        {
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
            let _ = ends.send(forward_output(from_session, &control));
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
    fn forward_output(socket: UnixStream, control: &Control) -> End {
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
        match control.taken_over_by() {
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
