use tendr::model::ids::{Namespace, SessionName};
use tendr::model::pty::PtyControl;
use tendr::model::spec::StdinMode;
use tendr::model::state::RunStatus;
use tendr::platform::{Current, Platform};
use tendr::pty_exit::PtyExitCode;
use tendr::session::{self, SessionRoot};

pub fn cmd_push(name: &str, namespace: &Namespace) -> anyhow::Result<()> {
    let session_name = SessionName::new(name)?;
    let root = SessionRoot::default_path()?;

    let session = session::open(&root, namespace, &session_name)?
        .ok_or_else(|| anyhow::anyhow!("session not found: {name}"))?;

    let meta = session::read_meta(&session)?;

    // First what can never succeed, whatever the session is doing. A pipe
    // session's push needs the stdin FIFO; a PTY push goes over the attach
    // socket, which every PTY session has.
    if meta.pty().is_none() && meta.launch_spec().stdin_mode != StdinMode::Pipe {
        anyhow::bail!("session was not started with --stdin");
    }

    // Push requires Running state explicitly. For a PTY session an ended run
    // is a stale run (81), as the sidecar reports when it ends while the push
    // connects; a pipe session keeps exit 1.
    if !matches!(meta.status(), RunStatus::Running { .. }) {
        let not_running = anyhow::anyhow!("session is not running");
        return Err(if meta.pty().is_some() {
            PtyExitCode::Control.error(not_running)
        } else {
            not_running
        });
    }

    // Reject push while a human is attached to a PTY session
    if let Some(pty) = meta.pty() {
        if pty.control == PtyControl::HumanControl {
            return Err(
                PtyExitCode::Control.error(anyhow::anyhow!("session is under human control"))
            );
        }
    }

    // PTY sessions take pushes over the attach socket, which reports exactly
    // what reached the terminal; pipe sessions keep the stdin FIFO.
    #[cfg(unix)]
    if meta.pty().is_some() {
        return push_over_attach_socket(session.path());
    }

    let mut fifo = loop {
        match Current::open_stdin_writer(session.path()) {
            Ok(f) => break f,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                // No reader connected -- check if session is still running
                let current = session::read_meta(&session)?;
                if !matches!(current.status(), RunStatus::Running { .. }) {
                    anyhow::bail!("session exited before push could connect");
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                return Err(anyhow::anyhow!("failed to open stdin pipe: {e}"));
            }
        }
    };

    let mut stdin = std::io::stdin().lock();
    std::io::copy(&mut stdin, &mut fifo)?;

    Ok(())
}

/// Push stdin to a PTY session as an acknowledged agent input stream.
///
/// Succeeds only if every byte of stdin was written to the terminal
/// ([`push_outcome`]). A push refused because another client holds the
/// terminal fails with exit 81; one revoked by a takeover or stopped by the
/// session part-way fails with exit 84 and how many bytes were written.
#[cfg(unix)]
fn push_over_attach_socket(session_dir: &std::path::Path) -> anyhow::Result<()> {
    use std::io::Read;
    use tendr::attach_proto::{self, Frame, FrameError, Mode, read_frame, write_frame};

    let mut stream = super::attach::connect(session_dir, Mode::Push)?;

    let mut stdin = std::io::stdin().lock();
    let mut buf = vec![0u8; attach_proto::MAX_FRAME_PAYLOAD];
    let mut sent: u64 = 0;
    let mut all_sent = false;
    loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) => {
                all_sent = true;
                break;
            }
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        if write_frame(&mut stream, &Frame::Data(buf[..n].to_vec())).is_err() {
            // The sidecar stopped reading: its outcome explains why.
            break;
        }
        sent += n as u64;
    }
    let _ = write_frame(&mut stream, &Frame::Detach);

    loop {
        match read_frame(&mut stream) {
            Ok(Frame::InputDone(outcome)) => return push_outcome(outcome, sent, all_sent),
            Err(FrameError::Malformed(e)) => {
                return Err(PtyExitCode::Protocol.error(anyhow::anyhow!(
                    "malformed push outcome from the session: {e}"
                )));
            }
            Ok(_) | Err(FrameError::Unknown(_)) => {}
            Err(FrameError::Io(e)) => {
                return Err(PtyExitCode::Runtime.error(anyhow::anyhow!(
                    "the session closed the push without reporting an outcome after {sent} bytes ({e})"
                )));
            }
        }
    }
}

/// The push's result from the session's `outcome`, given that `sent` bytes
/// were sent and `all_sent` is whether stdin was sent to its end.
///
/// Success means every byte was written. A push stopped after its last byte was
/// written (a takeover while its end marker was still on its way) wrote them
/// all, so it succeeds whatever the outcome (PR #68 review, finding 4). One
/// stopped with bytes unwritten is a partial write: exit 84, revoked or closed.
#[cfg(unix)]
fn push_outcome(
    outcome: tendr::attach_proto::PushOutcome,
    sent: u64,
    all_sent: bool,
) -> anyhow::Result<()> {
    use tendr::attach_proto::PushOutcome;

    let every_byte_written = |accepted: u64| all_sent && accepted == sent;
    match outcome {
        PushOutcome::Written { .. } => Ok(()),
        PushOutcome::Revoked(p) | PushOutcome::Closed(p) if every_byte_written(p.accepted) => {
            Ok(())
        }
        PushOutcome::Revoked(p) => Err(PtyExitCode::Runtime.error(anyhow::anyhow!(
            "push revoked after {} of {sent} bytes: another client took control of the terminal",
            p.accepted
        ))),
        PushOutcome::Closed(p) => Err(PtyExitCode::Runtime.error(anyhow::anyhow!(
            "push stopped after {} of {sent} bytes: the session stopped accepting input",
            p.accepted
        ))),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::push_outcome;
    use tendr::attach_proto::{Progress, PushOutcome};

    fn stopped(accepted: u64, received: u64) -> Progress {
        Progress {
            accepted,
            unwritten: received - accepted,
        }
    }

    /// A takeover that lands after the last byte was written, while the end
    /// marker is still on its way, ends the push `revoked` with nothing left
    /// unwritten. The push succeeded (PR #68 review, finding 4).
    #[test]
    fn a_push_whose_every_byte_was_written_succeeds_whatever_the_outcome() {
        assert!(push_outcome(PushOutcome::Written { bytes: 10 }, 10, true).is_ok());
        assert!(push_outcome(PushOutcome::Revoked(stopped(10, 10)), 10, true).is_ok());
        assert!(push_outcome(PushOutcome::Closed(stopped(10, 10)), 10, true).is_ok());
    }

    /// A partial write exits 84 whether it was revoked or closed.
    #[test]
    fn a_push_with_bytes_unwritten_fails_with_exit_84() {
        let code = |outcome, all_sent| {
            tendr::pty_exit::exit_code(&push_outcome(outcome, 10, all_sent).unwrap_err())
        };
        assert_eq!(code(PushOutcome::Revoked(stopped(4, 10)), true), 84);
        assert_eq!(code(PushOutcome::Closed(stopped(4, 10)), true), 84);
        // Stdin was not sent to its end: bytes the session never saw are unwritten.
        assert_eq!(code(PushOutcome::Revoked(stopped(10, 10)), false), 84);
    }
}
