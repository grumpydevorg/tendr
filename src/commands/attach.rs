use tendr::attach_escape::EscapeMode;
#[cfg(unix)]
use tendr::attach_proto::{self, Frame, FrameError, Mode};
use tendr::model::ids::{Namespace, SessionName};
use tendr::model::meta::Meta;
use tendr::model::pty::PtyControl;
use tendr::model::state::RunStatus;
use tendr::pty_exit::PtyExitCode;
use tendr::session::{self, SessionDir, SessionRoot};

mod bridge;

pub use bridge::cmd_attach_bridge;

pub fn cmd_attach(
    name: &str,
    namespace: &Namespace,
    takeover: bool,
    read_only: bool,
    escape: EscapeMode,
) -> anyhow::Result<()> {
    let (session, meta) = attachable(name, namespace, takeover || read_only)?;

    #[cfg(not(unix))]
    {
        let _ = (session, meta, escape);
        anyhow::bail!("attach is only supported on Unix");
    }

    #[cfg(unix)]
    {
        // Fail before any side effect — a takeover included — if there is no
        // terminal to drive.
        require_terminal()?;
        let mode = mode(takeover, read_only);
        // A local attach does not reconnect, so it has no use for offsets.
        let (stream, accept) = connect(session.path(), attach_proto::Version::V1, mode)?;
        // A watcher holds no control, so nothing can take it over.
        let control = accept.epoch().map(|epoch| Control {
            session_dir: session.path().to_path_buf(),
            run_id: meta.run_id(),
            epoch,
        });
        let recorded_takeover = || {
            control
                .as_ref()
                .is_some_and(|control| control.taken_over_by().is_some())
        };
        unix_relay::relay(
            unix_relay::Link::socket(stream)?,
            None,
            escape,
            accept.access(),
            unix_relay::Peer::Local {
                recorded_takeover: &recorded_takeover,
            },
        )
        .map(drop)
    }
}

/// The session `name` in `namespace`, if it can be attached: a PTY session
/// whose run is running, and not under human control unless the attach does
/// not contend for it: a takeover or a resume, which may supersede its
/// holder and where the sidecar decides (after a lost connection
/// `meta.json` still names the resuming client's own control), or a
/// read-only attach, which never holds control. Shared by a local attach
/// and the remote end of `attach --host`.
fn attachable(
    name: &str,
    namespace: &Namespace,
    human_control_ok: bool,
) -> anyhow::Result<(SessionDir, Meta)> {
    let session_name = SessionName::new(name)?;
    let root = SessionRoot::default_path()?;

    let session = session::open(&root, namespace, &session_name)?
        .ok_or_else(|| anyhow::anyhow!("session not found: {name}"))?;

    let meta = session::read_meta(&session)?;

    // First what can never succeed: a session without a PTY cannot be attached
    // whatever its state (1).
    let pty = meta
        .pty()
        .ok_or_else(|| anyhow::anyhow!("session is not PTY-enabled"))?;

    // A PTY run that has ended is a stale run (81), as the sidecar reports when
    // it ends while the attach connects.
    if !matches!(meta.status(), RunStatus::Running { .. }) {
        return Err(PtyExitCode::Control.error(anyhow::anyhow!("session is not running")));
    }

    // Informational pre-check for a clear message; the sidecar is the authority.
    if !human_control_ok && pty.control == PtyControl::HumanControl {
        return Err(PtyExitCode::Control.error(anyhow::anyhow!(
            "session is already under human control (use --takeover to take it over)"
        )));
    }
    Ok((session, meta))
}

/// The hello's mode: `read_only` and `takeover` never come together (the
/// command line refuses it).
#[cfg(unix)]
fn mode(takeover: bool, read_only: bool) -> Mode {
    match (read_only, takeover) {
        (true, _) => Mode::Watch,
        (false, true) => Mode::Takeover,
        (false, false) => Mode::Attach,
    }
}

/// An attach drives the terminal on stdin.
#[cfg(unix)]
fn require_terminal() -> anyhow::Result<()> {
    if rustix::termios::isatty(std::io::stdin()) {
        Ok(())
    } else {
        anyhow::bail!("attach requires an interactive terminal on stdin")
    }
}

/// How long `attach --host` waits for ssh to end once the attach has ended
/// on this side (a detach, or the keyboard closing) before stopping it. The
/// remote side ends as soon as it reads the end of its input; the session
/// treats a lost connection as a detach either way.
#[cfg(unix)]
const SSH_LEAVE_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

/// How long `attach --host` waits for ssh to end, for its exit code, once
/// the remote end has closed the connection. ssh still running after it
/// counts as a lost connection.
#[cfg(unix)]
const SSH_CLOSE_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// `attach --host` from a Unix client. `frame` is the encoded remote frame
/// of `request`; the hello follows it on ssh's stdin, and then the typed
/// attach frames, which the remote tendr bridges to the session's attach
/// socket ([`cmd_attach_bridge`]). This end keeps the terminal: raw mode,
/// resizes (SIGWINCH included) and the detach escape, exactly as a local
/// attach. The remote sidecar keeps the controller epochs and the recording.
///
/// The version is agreed in the request, whose `protocol` names the newest
/// this client speaks; the hello on ssh's stdin is always a version 1 hello,
/// the only one a 0.5.0 remote tendr takes, and that remote ignores
/// `protocol`. The answer's type says which version the session speaks: a
/// version 2 acceptance starts the offsets this client tracks, a version 1
/// one means plain output, as before.
///
/// A version 2 attach whose connection drops reconnects by itself and
/// resumes where its terminal stopped ([`resume_host`]), until it resumes,
/// the remote refuses, or the user leaves.
///
/// Returns the exit code. The remote side's own failures (no such session,
/// a refusal, an unreachable socket) arrive as its exit code, with its
/// message passed through on stderr, so the codes are those of a local
/// attach.
///
/// # Errors
/// A takeover of this attach (81), an answer or output outside the protocol
/// (80), a remote that ended without answering (84), and the ssh failures of
/// [`tendr::ssh::AttachSsh`].
#[cfg(unix)]
pub fn cmd_attach_host(
    host: &str,
    request: &crate::remote_op::AttachRequest,
    mut frame: Vec<u8>,
) -> anyhow::Result<i32> {
    use tendr::attach_proto::Version;

    // Before ssh starts: the request may take over.
    require_terminal()?;
    let mode = mode(request.takeover, request.read_only);
    attach_proto::write_frame(
        &mut frame,
        &Frame::Hello {
            version: Version::V1,
            mode,
        },
    )?;
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stderr = std::sync::Arc::new(unix_relay::StderrRoute::default());
    let dial = tendr::ssh::Dial {
        login: tendr::ssh::Login::Interactive,
        stderr: Box::new(unix_relay::SshStderr::new(std::sync::Arc::clone(&stderr))),
        cancel: std::sync::Arc::clone(&cancel),
    };
    // The remote side's answer is the sidecar's acceptance.
    let opened = open_host(host, &frame, dial, |reply| {
        hello_reply(reply, mode, Version::LATEST)
    })?;
    let (link, accept) = match opened {
        Opened::Answered(link, accept) => (link, accept),
        Opened::Ended(code) => return Ok(code),
    };
    let (link, ssh) = link.split();
    let end = HostEnd {
        host: host.to_owned(),
        request: request.clone(),
        ssh: Some(ssh),
        cancel,
        stderr,
    };
    unix_relay::relay(
        link,
        accept.resume_point(),
        request.escape,
        accept.access(),
        unix_relay::Peer::Host(Box::new(end)),
    )
}

/// The ssh end of an `attach --host`: the current connection's ssh, and what
/// a reconnect needs to open the next.
#[cfg(unix)]
pub(super) struct HostEnd {
    host: String,
    request: crate::remote_op::AttachRequest,
    /// The current connection's ssh, until it is finished.
    ssh: Option<tendr::ssh::AttachSsh>,
    /// Once set, every ssh of this attach stops, the one connecting
    /// included: the attach is leaving while it reconnects.
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Where every connection's ssh stderr goes once the relay owns the
    /// terminal.
    stderr: std::sync::Arc<unix_relay::StderrRoute>,
}

#[cfg(unix)]
impl HostEnd {
    /// Wait at most `grace` for the current connection's ssh to end, then
    /// stop it: the call's exit code, or `None` if ssh had to be stopped or
    /// was already finished.
    fn finish(&mut self, grace: std::time::Duration) -> Option<anyhow::Result<i32>> {
        self.ssh
            .take()?
            .finish(Some(grace))
            .map(|code| code.map_err(anyhow::Error::from))
    }

    /// One attempt to resume from `point` over a new ssh, which never
    /// prompts ([`tendr::ssh::Login::Batch`]) and whose stderr goes where
    /// every connection's does once the relay owns the terminal. On success
    /// the new ssh is this end's.
    ///
    /// # Errors
    /// As for [`resume_host`].
    fn resume(&mut self, point: attach_proto::ResumePoint) -> anyhow::Result<unix_relay::Attempt> {
        let dial = tendr::ssh::Dial {
            login: tendr::ssh::Login::Batch,
            stderr: Box::new(unix_relay::SshStderr::new(std::sync::Arc::clone(
                &self.stderr,
            ))),
            cancel: std::sync::Arc::clone(&self.cancel),
        };
        Ok(match resume_host(&self.host, &self.request, point, dial)? {
            Opened::Answered(link, resumption) => {
                let (link, ssh) = link.split();
                self.ssh = Some(ssh);
                unix_relay::Attempt::Resumed(link, resumption)
            }
            Opened::Ended(code) => unix_relay::Attempt::Ended(code),
        })
    }
}

/// An `attach --host` connection whose remote side has answered: ssh, its
/// stdin for the client's frames, and its stdout, past the answer, for the
/// session's.
#[cfg(unix)]
pub(super) struct HostLink {
    ssh: tendr::ssh::AttachSsh,
    stdin: std::process::ChildStdin,
    reader: std::io::BufReader<std::process::ChildStdout>,
}

#[cfg(unix)]
impl HostLink {
    /// The relay's link over ssh's stdin and stdout, and ssh. The sender
    /// closes ssh's stdin when the relay closes its outbox, and
    /// [`HostEnd::finish`] stops ssh if it does not end by itself.
    fn split(self) -> (unix_relay::Link, tendr::ssh::AttachSsh) {
        let link = unix_relay::Link {
            reader: Box::new(self.reader),
            writer: Box::new(std::io::BufWriter::new(self.stdin)),
            close: Box::new(|| {}),
        };
        (link, self.ssh)
    }
}

/// How opening an `attach --host` connection went.
#[cfg(unix)]
pub(super) enum Opened<A> {
    /// The session answered `A`; the attach goes on over the link.
    Answered(HostLink, A),
    /// The remote side ended without answering, with this exit code, its
    /// verdict: a refusal or a failure of its own (its message has been
    /// passed on to stderr).
    Ended(i32),
}

/// Start ssh with `request` (the remote frame and what replaces the hello),
/// read the session's answer, and let `answer` classify it. A remote side
/// that ends without answering is [`Opened::Ended`] with its exit code (84
/// if it claims success); an answer `answer` refuses stops ssh. Setting the
/// dial's `cancel` stops ssh at any point before the answer begins to
/// arrive.
///
/// # Errors
/// `answer`'s error, and the ssh failures of [`tendr::ssh::AttachSsh`].
#[cfg(unix)]
fn open_host<A>(
    host: &str,
    request: &[u8],
    dial: tendr::ssh::Dial,
    answer: impl FnOnce(Result<Frame, FrameError>) -> anyhow::Result<A>,
) -> anyhow::Result<Opened<A>> {
    use tendr::ssh::{AttachSsh, AttachStart, SshError};

    let cancel = std::sync::Arc::clone(&dial.cancel);
    let (ssh, stdin, stdout) = match AttachSsh::start(host, request, dial)? {
        AttachStart::Running { ssh, stdin, stdout } => (ssh, stdin, stdout),
        AttachStart::Ended(code) => return unanswered(code).map(Opened::Ended),
    };
    // A refusal or a failure of the remote side's own ends the connection,
    // and its exit code says which. The answer is waited for where a cancel
    // can stop it: the link may freeze after the hello, and without a
    // connect deadline there is no hello at all.
    let mut reader = std::io::BufReader::new(stdout);
    let reply = if readable_unless(reader.get_ref(), &cancel) {
        attach_proto::read_frame(&mut reader)
    } else {
        Err(FrameError::Io(std::io::ErrorKind::Interrupted.into()))
    };
    match reply {
        Err(FrameError::Io(_)) => {
            drop(stdin);
            match ssh.finish(None) {
                Some(code) => unanswered(code?).map(Opened::Ended),
                // Without a grace, only a cancel stops it.
                None => Err(SshError::Cancelled.into()),
            }
        }
        reply => match answer(reply) {
            Ok(answered) => Ok(Opened::Answered(HostLink { ssh, stdin, reader }, answered)),
            Err(e) => {
                drop(stdin);
                let _ = ssh.finish(Some(SSH_LEAVE_GRACE));
                Err(e)
            }
        },
    }
}

/// Wait until `stdout` has something to read, has closed, or cannot be
/// polled: `true`, and the read that follows says which; or until `cancel`
/// is set: `false`.
#[cfg(unix)]
fn readable_unless(
    stdout: &impl std::os::fd::AsFd,
    cancel: &std::sync::atomic::AtomicBool,
) -> bool {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let tick = Timespec {
        tv_sec: 0,
        tv_nsec: 20_000_000,
    };
    loop {
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }
        let mut fds = [PollFd::new(stdout, PollFlags::IN)];
        match poll(&mut fds, Some(&tick)) {
            Ok(0) | Err(rustix::io::Errno::INTR) => {}
            Ok(_) | Err(_) => return true,
        }
    }
}

/// Reconnect the `attach --host` of `request` after its connection was lost,
/// resuming from `point`: the run, the epoch it held and the offset just
/// past the last output byte written to the terminal (the version 2 resume
/// point the relay keeps). `dial` says how ssh connects.
///
/// The request carries `point`, and a resume frame takes the hello's place
/// on ssh's stdin ([`resume_request`]), so a remote tendr that cannot resume
/// refuses it rather than attach afresh. On [`Resumption::Resumed`] the relay
/// goes on from `point.offset`: the session's stamped output follows from
/// there, with no mode replay. On [`Resumption::Gone`] the output in between
/// is lost: the caller writes the reset of the modes it has tracked
/// (`TermModes::reset`) before relaying, since the mode replay that follows
/// only turns modes on, then goes on from the head, and sends the size and
/// the repaint nudge, as a fresh attach does.
///
/// # Errors
/// A refusal (81 when control has changed since, the run has ended, or an
/// agent's push holds the terminal; 80 when the session cannot resume), an
/// answer outside the protocol (80), and the ssh failures of
/// [`tendr::ssh::AttachSsh`].
#[cfg(unix)]
fn resume_host(
    host: &str,
    request: &crate::remote_op::AttachRequest,
    point: attach_proto::ResumePoint,
    dial: tendr::ssh::Dial,
) -> anyhow::Result<Opened<Resumption>> {
    let frame = resume_request(request, point)?;
    open_host(host, &frame, dial, |reply| resume_reply(reply, point))
}

/// What a resume from `point` writes to ssh's stdin: the remote frame of
/// `request` carrying `point`, never a takeover, then the resume frame where
/// the hello would go.
#[cfg(unix)]
fn resume_request(
    request: &crate::remote_op::AttachRequest,
    point: attach_proto::ResumePoint,
) -> anyhow::Result<Vec<u8>> {
    use crate::remote_op::{AttachRequest, RemoteOperation};

    let mut frame = crate::remote_frame::encode(&RemoteOperation::Attach(AttachRequest {
        takeover: false,
        resume: Some(point.into()),
        ..request.clone()
    }))?;
    attach_proto::write_frame(&mut frame, &Frame::Resume(point))?;
    Ok(frame)
}

/// The exit code of an `attach --host` whose remote side ended without
/// accepting: its own code, or 84 if it claims success.
#[cfg(unix)]
fn unanswered(code: i32) -> anyhow::Result<i32> {
    if code == 0 {
        Err(PtyExitCode::Runtime.error(anyhow::anyhow!(
            "the remote tendr ended without answering the attach"
        )))
    } else {
        Ok(code)
    }
}

/// The control this attach was granted: its run and controller epoch.
#[cfg(unix)]
struct Control {
    session_dir: std::path::PathBuf,
    run_id: tendr::model::ids::RunId,
    epoch: tendr::model::pty_control::ControllerEpoch,
}

#[cfg(unix)]
impl Control {
    /// The epoch of the client that took this control over, if one did: in
    /// this run, the event log records a takeover granting the epoch after
    /// this one, and no detach releasing this one. A client that had already
    /// let go (dropped for falling behind, say) is not taken over by a later
    /// `--takeover`, which also grants the next epoch.
    ///
    /// A client that stopped reading its output cannot read the `MSG_RETIRED`
    /// queued behind it, so the sidecar records the takeover before it ends the
    /// connection, and the client looks here when it sees the end. An
    /// unreadable log, or a sidecar that records no epochs, reads as no
    /// takeover.
    fn taken_over_by(&self) -> Option<tendr::model::pty_control::ControllerEpoch> {
        let successor = self.epoch.next().ok()?;
        let log = tendr::events::read_session_events(&self.session_dir).ok()?;
        let changes = || {
            log.events.iter().filter_map(|event| {
                (event.run_id == self.run_id && event.kind.as_str() == "pty.control_changed")
                    .then_some(event.data.as_ref())
                    .flatten()
            })
        };
        let released =
            changes().any(|data| data["trigger"] == "detach" && data["epoch"] == self.epoch.get());
        let took_over =
            changes().any(|data| data["trigger"] == "takeover" && data["epoch"] == successor.get());
        (!released && took_over).then_some(successor)
    }
}

/// How long a client waits for the sidecar's answer to its hello.
#[cfg(unix)]
const HELLO_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The sidecar's acceptance: version 1's controller epoch, or version 2's
/// point in the run, from which a viewer's stamped output starts; or, for a
/// read-only viewer, its point in the run with no epoch, since it holds no
/// control.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Accept {
    V1(tendr::model::pty_control::ControllerEpoch),
    V2(attach_proto::ResumePoint),
    Watch(attach_proto::WatchPoint),
}

#[cfg(unix)]
impl Accept {
    /// The controller epoch granted; `None` for a watcher, which holds
    /// none, so no takeover check applies to it.
    fn epoch(self) -> Option<tendr::model::pty_control::ControllerEpoch> {
        match self {
            Self::V1(epoch) => Some(epoch),
            Self::V2(point) => Some(point.epoch),
            Self::Watch(_) => None,
        }
    }

    /// Where a version 2 controller's output starts; `None` for version 1,
    /// whose output carries no offsets, and for a watcher, which does not
    /// resume.
    fn resume_point(self) -> Option<attach_proto::ResumePoint> {
        match self {
            Self::V1(_) | Self::Watch(_) => None,
            Self::V2(point) => Some(point),
        }
    }

    /// What the relay may send for this attach.
    fn access(self) -> unix_relay::Access {
        match self {
            Self::V1(_) | Self::V2(_) => unix_relay::Access::Control,
            Self::Watch(_) => unix_relay::Access::ReadOnly,
        }
    }

    /// The acceptance as the sidecar sent it.
    fn frame(self) -> Frame {
        match self {
            Self::V1(epoch) => Frame::Accepted(epoch),
            Self::V2(point) => Frame::AcceptedV2(point),
            Self::Watch(point) => Frame::Watching(point),
        }
    }
}

/// Connect to the session's attach socket, check that its listener is this
/// user's, and complete the hello in `mode`, speaking `version`; `attach`,
/// PTY `push` and the attach bridge all start here. Returns the connection
/// and the sidecar's acceptance.
///
/// A sidecar started by an older tendr may not speak `version`: it refuses
/// the hello with the protocol class. Then this connects once more and
/// speaks version 1, which every sidecar with a hello speaks. A watch hello
/// is never retried: the protocol class there means the sidecar predates
/// read-only viewers in every version, and the failure says so (80).
///
/// Every failure carries its PTY exit code ([`tendr::pty_exit`]): a socket
/// that is missing or cannot be reached is a runtime failure (84), a listener
/// of another user an identity failure (85), and the hello's reply is
/// classified by [`hello_reply`].
#[cfg(unix)]
pub(super) fn connect(
    session_dir: &std::path::Path,
    version: attach_proto::Version,
    mode: Mode,
) -> anyhow::Result<(std::os::unix::net::UnixStream, Accept)> {
    use attach_proto::{RejectClass, Version};
    let mut stream = open_socket(session_dir)?;
    let hello = |version| Frame::Hello { version, mode };
    let mut reply = exchange(&mut stream, &hello(version), verb(mode))?;
    let mut version = version;
    if let Ok(Frame::Rejected {
        class: RejectClass::Protocol,
        reason,
    }) = &reply
    {
        if mode == Mode::Watch {
            return Err(PtyExitCode::Protocol.error(anyhow::anyhow!(
                "cannot attach read-only: the session was started by a tendr that cannot serve read-only viewers ({reason})"
            )));
        }
        if version > Version::V1 {
            version = Version::V1;
            stream = open_socket(session_dir)?;
            reply = exchange(&mut stream, &hello(version), verb(mode))?;
        }
    }
    let accept = hello_reply(reply, mode, version)?;
    // From here an attach waits for output, and a push as long as the terminal
    // applies backpressure.
    stream.set_read_timeout(None).map_err(|e| {
        PtyExitCode::Runtime.error(anyhow::anyhow!("{} handshake failed: {e}", verb(mode)))
    })?;
    Ok((stream, accept))
}

/// What the sidecar answered a resume: control regained at a new epoch, and
/// where the output goes on from.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Resumption {
    /// The output follows from `point.offset`, the offset asked for, with
    /// no mode replay: the terminal already has the modes.
    Resumed(attach_proto::ResumePoint),
    /// The output from the offset asked for is gone: the session holds only
    /// `oldest` up to `point.offset`, its head. The mode replay follows,
    /// unstamped, then the output from the head.
    Gone {
        point: attach_proto::ResumePoint,
        oldest: u64,
    },
}

#[cfg(unix)]
impl Resumption {
    /// The new epoch, and the offset the stamped output starts at.
    pub(super) fn point(self) -> attach_proto::ResumePoint {
        match self {
            Self::Resumed(point) | Self::Gone { point, .. } => point,
        }
    }

    /// The answer as the sidecar sent it.
    fn frame(self) -> Frame {
        match self {
            Self::Resumed(point) => Frame::Resumed(point),
            Self::Gone { point, oldest } => Frame::Gone { point, oldest },
        }
    }
}

/// [`connect`] for a resume from `point`: the resume frame takes the hello's
/// place. A sidecar started by a tendr that cannot resume refuses it with
/// the protocol class; that is "cannot resume" (80), never a fresh attach.
#[cfg(unix)]
fn connect_resume(
    session_dir: &std::path::Path,
    point: attach_proto::ResumePoint,
) -> anyhow::Result<(std::os::unix::net::UnixStream, Resumption)> {
    let mut stream = open_socket(session_dir)?;
    let reply = exchange(&mut stream, &Frame::Resume(point), "resume")?;
    if let Ok(Frame::Rejected {
        class: attach_proto::RejectClass::Protocol,
        reason,
    }) = &reply
    {
        return Err(PtyExitCode::Protocol.error(anyhow::anyhow!(
            "cannot resume: the session was started by a tendr that cannot resume an attach ({reason}); attach again"
        )));
    }
    let resumed = resume_reply(reply, point)?;
    stream
        .set_read_timeout(None)
        .map_err(|e| PtyExitCode::Runtime.error(anyhow::anyhow!("resume handshake failed: {e}")))?;
    Ok((stream, resumed))
}

/// Connect to the session's attach socket and check that its listener is
/// this user's.
#[cfg(unix)]
fn open_socket(session_dir: &std::path::Path) -> anyhow::Result<std::os::unix::net::UnixStream> {
    let sock_path = attach_proto::read_sock_path(session_dir)
        .ok_or_else(|| PtyExitCode::Runtime.error(anyhow::anyhow!("attach socket not found")))?;
    let stream = std::os::unix::net::UnixStream::connect(&sock_path).map_err(|e| {
        PtyExitCode::Runtime.error(anyhow::anyhow!(
            "cannot connect to attach socket {}: {e}",
            sock_path.display()
        ))
    })?;
    // Keystrokes and pushed input go only to a listener run by this same user:
    // a spoofed socket owned by someone else is refused before the hello.
    tendr::attach_socket::verify_peer(&stream).map_err(|e| {
        PtyExitCode::of_peer_check(&e).error(anyhow::anyhow!(
            "refusing attach socket {}: {e}",
            sock_path.display()
        ))
    })?;
    Ok(stream)
}

/// Send `hello` (a hello, or the resume that replaces it), and read the
/// sidecar's reply under the reply deadline, for [`hello_reply`] or
/// [`resume_reply`] to classify.
///
/// The sidecar refuses a peer of another user, or a connection beyond its
/// limit, before reading the hello, and closes the connection. If that lands
/// first, the hello cannot be written, but the refusal is still waiting to be
/// read: it, not the failed write, decides the outcome. The write's error is
/// the error only when no frame can be read.
#[cfg(unix)]
fn exchange(
    stream: &mut std::os::unix::net::UnixStream,
    hello: &Frame,
    what: &str,
) -> anyhow::Result<Result<Frame, FrameError>> {
    let transport = |e: std::io::Error| {
        PtyExitCode::Runtime.error(anyhow::anyhow!("{what} handshake failed: {e}"))
    };
    // macOS refuses SO_RCVTIMEO (EINVAL) on a socket whose peer has already
    // shut down: exactly the refusal-before-hello case. Then read without
    // blocking, so a buffered refusal is still seen and a live peer that never
    // got a hello cannot hold the read forever.
    let timeout = stream.set_read_timeout(Some(HELLO_REPLY_TIMEOUT));
    let unbounded = timeout.is_err();
    if unbounded {
        stream.set_nonblocking(true).map_err(transport)?;
    }
    let sent = timeout.and_then(|()| attach_proto::write_frame(stream, hello));
    // After a failed write the peer has closed its end, so this read returns
    // at once; otherwise it is bounded by the reply timeout.
    let reply = attach_proto::read_frame(stream);
    if unbounded {
        let _ = stream.set_nonblocking(false);
    }
    if let Err(e) = sent {
        if matches!(reply, Err(FrameError::Io(_))) {
            return Err(transport(e));
        }
    }
    Ok(reply)
}

#[cfg(unix)]
fn verb(mode: Mode) -> &'static str {
    match mode {
        Mode::Attach | Mode::Takeover => "attach",
        Mode::Push => "push",
        Mode::Watch => "read-only attach",
    }
}

/// What the sidecar's reply to a hello in `mode` means for the client, which
/// speaks versions up to `upto`.
///
/// A refusal exits with its class's code. An acceptance of a newer version
/// than `upto`, an acceptance that does not fit the mode (control granted to
/// a watch hello, or a watcher's admission to any other), any other reply,
/// or none before the deadline (a sidecar from before the hello waits
/// silently), is a protocol failure (80); a connection that ends first is a
/// transport failure (84).
#[cfg(unix)]
fn hello_reply(
    reply: Result<Frame, FrameError>,
    mode: Mode,
    upto: attach_proto::Version,
) -> anyhow::Result<Accept> {
    let what = verb(mode);
    let protocol = |message: String| Err(PtyExitCode::Protocol.error(anyhow::anyhow!(message)));
    let watch = mode == Mode::Watch;
    match reply {
        Ok(Frame::Watching(point)) if watch => Ok(Accept::Watch(point)),
        Ok(answer @ (Frame::Accepted(_) | Frame::AcceptedV2(_))) if watch => protocol(format!(
            "the session answered the {what} with {answer:?}, granting control it was not asked for"
        )),
        Ok(Frame::Accepted(epoch)) => Ok(Accept::V1(epoch)),
        Ok(Frame::AcceptedV2(point)) if upto >= attach_proto::Version::V2 => Ok(Accept::V2(point)),
        Ok(Frame::AcceptedV2(_)) => protocol(format!(
            "the session accepted the {what} in attach protocol version 2, which this connection did not ask for"
        )),
        reply => reply_failure(reply, what),
    }
}

/// What the sidecar's reply to a resume from `asked` means for the client.
/// An answer must be for the same run, at a later epoch; a resumed one at
/// the offset asked for, a gone one naming a span that starts after it (an
/// offset past the head means this client's count is broken). Otherwise as
/// [`hello_reply`].
#[cfg(unix)]
fn resume_reply(
    reply: Result<Frame, FrameError>,
    asked: attach_proto::ResumePoint,
) -> anyhow::Result<Resumption> {
    let ours = |point: attach_proto::ResumePoint| {
        point.run_id == asked.run_id && point.epoch > asked.epoch
    };
    match reply {
        Ok(Frame::Resumed(point)) if ours(point) && point.offset == asked.offset => {
            Ok(Resumption::Resumed(point))
        }
        Ok(Frame::Gone { point, oldest }) if ours(point) && asked.offset < oldest => {
            Ok(Resumption::Gone { point, oldest })
        }
        Ok(answer @ (Frame::Resumed(_) | Frame::Gone { .. })) => {
            Err(PtyExitCode::Protocol.error(anyhow::anyhow!(
                "the session's answer {answer:?} does not fit the resume from {asked:?}"
            )))
        }
        reply => reply_failure(reply, "resume"),
    }
}

/// A reply that accepts nothing: a refusal exits with its class's code; any
/// other reply, or none before the deadline (a sidecar from before the hello
/// waits silently), is a protocol failure (80); a connection that ends first
/// is a transport failure (84).
#[cfg(unix)]
fn reply_failure<T>(reply: Result<Frame, FrameError>, what: &str) -> anyhow::Result<T> {
    let protocol = |message: String| Err(PtyExitCode::Protocol.error(anyhow::anyhow!(message)));
    match reply {
        Ok(Frame::Rejected { class, reason }) => {
            Err(PtyExitCode::from(class).error(anyhow::anyhow!("{what} refused: {reason}")))
        }
        Ok(other) => protocol(format!(
            "unexpected {what} reply {other:?}; the session may predate attach protocol v1"
        )),
        Err(FrameError::Unknown(msg_type)) => protocol(format!(
            "the session replied with attach message type {msg_type}, which this tendr does not know; it may be newer"
        )),
        Err(FrameError::Malformed(e)) => protocol(format!(
            "the session's reply to the {what} hello is malformed ({e}); it may be older or newer than this tendr"
        )),
        Err(FrameError::Io(e))
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            protocol(format!(
                "the session did not answer the {what} hello ({e}); it may predate attach protocol v1"
            ))
        }
        Err(FrameError::Io(e)) => Err(PtyExitCode::Runtime.error(anyhow::anyhow!(
            "the session closed the connection before answering the {what} hello ({e})"
        ))),
    }
}

/// The interactive relay between the user's terminal and the session, over
/// the attach socket or, for `attach --host`, over ssh to the remote bridge.
///
/// Three independent paths, so no blocked one can delay a detach:
///
/// - the **main thread** polls the keyboard, recognizes the escape, watches for
///   terminal size changes, and only ever queues messages;
/// - a **sender thread** writes queued messages to the session, controls
///   (resize, detach) before keystrokes; if the session stops accepting input,
///   only this thread waits;
/// - a **reader thread** writes session output to the terminal; if the terminal
///   stops reading, only this thread waits.
///
/// When an `attach --host` connection is lost, a fourth path, the
/// [`reconnect`](unix_relay::reconnect) thread, judges ssh's end and opens a
/// new connection, while the main thread keeps the keyboard.
///
/// Leaving never joins a blocked thread: the connection is closed, the
/// terminal is restored (without waiting for pending output to drain), and
/// the process exits.
///
/// SIGHUP, SIGTERM and SIGINT end the relay like a detach, and SIGWINCH brings
/// the next size check forward. Their handlers only wake the main thread's
/// poll through a self-pipe ([`signals`](unix_relay::signals)); the previous
/// dispositions are restored when the relay ends.
#[cfg(unix)]
mod unix_relay {
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
    use std::time::{Duration, Instant};

    use tendr::attach_escape::{Escape, EscapeMode, EscapeParser};
    use tendr::attach_proto::{self, Frame, FrameError};
    use tendr::pty_exit::PtyExitCode;
    use tendr::term_modes::TermModes;

    mod reconnect;

    pub(super) use reconnect::{Attempt, SshStderr, StderrRoute};

    /// Input that may wait for a session that is not accepting it. Beyond
    /// this, the attach client drops further keystrokes (and reports them)
    /// rather than buffering without bound, and the escape is still
    /// recognized; the remote bridge stops reading its input until there is
    /// room.
    const OUTBOX_DATA_LIMIT: usize = 1 << 20;
    /// How long the main thread waits on the keyboard before it checks again
    /// whether the session has closed.
    const POLL: Duration = Duration::from_millis(100);
    /// How often the terminal size is checked.
    const SIZE_POLL: Duration = Duration::from_millis(100);
    /// How long a detach waits for the detach message to be sent.
    pub(super) const DETACH_GRACE: Duration = Duration::from_millis(200);
    /// How long leaving on a signal waits for its report to be written to a
    /// terminal that may be gone or not reading.
    const REPORT_GRACE: Duration = Duration::from_millis(200);
    /// How long leaving waits to turn off the session's terminal modes in a
    /// terminal that is slow, gone or not reading. After a signal the shorter
    /// [`REPORT_GRACE`] applies: SIGHUP means the terminal is gone.
    const MODES_GRACE: Duration = Duration::from_secs(1);
    /// How long leaving during a reconnect waits for it to stop its ssh,
    /// which gets SIGTERM, then SIGKILL a second later.
    const RECONNECT_STOP_GRACE: Duration = Duration::from_secs(2);

    /// One attach connection as the relay sees it: frames in, frames out.
    pub(super) struct Link {
        pub(super) reader: Box<dyn Read + Send>,
        pub(super) writer: Box<dyn Write + Send>,
        /// Ends the connection when the relay leaves, unblocking the reader
        /// and the sender; the session treats a closed connection as a
        /// detach.
        pub(super) close: Box<dyn FnOnce() + Send>,
    }

    impl Link {
        /// A connection to the session's attach socket.
        pub(super) fn socket(stream: UnixStream) -> anyhow::Result<Self> {
            let socket = |e| PtyExitCode::Runtime.error(anyhow::anyhow!("attach socket: {e}"));
            let shutdown = stream.try_clone().map_err(socket)?;
            Ok(Self {
                reader: Box::new(stream.try_clone().map_err(socket)?),
                writer: Box::new(stream),
                close: Box::new(move || {
                    let _ = shutdown.shutdown(std::net::Shutdown::Both);
                }),
            })
        }
    }

    /// The far end of the relay.
    pub(super) enum Peer<'a> {
        /// The session's attach socket on this machine. `recorded_takeover`
        /// is asked when the session closes the connection without saying
        /// it was taken over.
        Local {
            recorded_takeover: &'a dyn Fn() -> bool,
        },
        /// The remote tendr, over ssh (`attach --host`). Its end of a
        /// connection says whether the remote ended it or the link was lost,
        /// and a version 2 attach whose link was lost reconnects through it.
        Host(Box<super::HostEnd>),
    }

    /// What an attach may send the session.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(in crate::commands::attach) enum Access {
        /// Keystrokes, resizes and the repaint nudge: it holds control.
        Control,
        /// Nothing but the detach: a watcher's terminal neither types into
        /// the session nor sizes it, so a program it attaches to shows
        /// nothing new until it next draws, at the controller's size.
        ReadOnly,
    }

    /// One connection's paths to the session: the sender, writing its
    /// outbox, and the reader.
    struct Connection {
        outbox: Arc<Outbox>,
        access: Access,
        /// Set once the connection has ended: by the reader at the end of
        /// the session's output, or by the sender on a failed write.
        closed: Arc<AtomicBool>,
        reader: std::thread::JoinHandle<()>,
        close: Box<dyn FnOnce() + Send>,
    }

    impl Connection {
        fn open(
            link: Link,
            access: Access,
            retired: &Arc<AtomicBool>,
            screen: &Arc<Screen>,
        ) -> Self {
            let closed = Arc::new(AtomicBool::new(false));
            let outbox = Arc::new(Outbox::default());
            spawn_sender(link.writer, Arc::clone(&outbox), Arc::clone(&closed));
            let reader = spawn_reader(
                link.reader,
                Arc::clone(&closed),
                Arc::clone(retired),
                Arc::clone(screen),
            );
            Self {
                outbox,
                access,
                closed,
                reader,
                close: link.close,
            }
        }

        fn closed(&self) -> bool {
            self.closed.load(Ordering::SeqCst)
        }

        /// Send the terminal's size, then the repaint nudge: an attach at
        /// the session's size makes the child draw nothing, so the sidecar
        /// nudges it into redrawing, which only a resize lets apply. A
        /// read-only attach sends neither.
        fn greet(&self, size: Option<tendr::recording::Geometry>) {
            if let Some(size) = size {
                if self.access == Access::Control {
                    self.outbox.push_control(Frame::Resize(size));
                    self.outbox.push_control(Frame::Repaint);
                }
            }
        }

        /// Send the terminal's new size, unless the attach is read-only.
        fn resize(&self, size: tendr::recording::Geometry) {
            if self.access == Access::Control {
                self.outbox.push_control(Frame::Resize(size));
            }
        }

        /// End the connection, which unblocks its sender and reader; the
        /// session treats a closed connection as a detach. Returns the
        /// reader, which may still be writing to the terminal.
        fn close(self) -> std::thread::JoinHandle<()> {
            (self.close)();
            self.outbox.close();
            self.reader
        }
    }

    /// Relay until the attach ends, and return its exit code: for `attach
    /// --host`, the remote's when the remote ended it, otherwise 0. Another
    /// client taking it over is an error (exit 81), and so is session output
    /// that breaks the protocol (exit 80). `resume` is where a version 2
    /// attach's stamped output starts; `None` for version 1.
    ///
    /// A read-only attach ([`Access::ReadOnly`]) sends no size, resize or
    /// repaint, and drops keystrokes, saying so once in the status line; the
    /// escape and the signals still leave.
    ///
    /// A version 2 `attach --host` whose connection the far side ends, without
    /// a takeover or a broken protocol, hands ssh's end to a
    /// [`reconnect::Job`]: the remote's exit code ends the attach with it, and
    /// a lost link is resumed. Meanwhile keystrokes are discarded, with one
    /// notice in the status line; the escape and the signals still leave, and
    /// stop the reconnect's ssh.
    pub(super) fn relay(
        link: Link,
        resume: Option<attach_proto::ResumePoint>,
        escape: EscapeMode,
        access: Access,
        peer: Peer<'_>,
    ) -> anyhow::Result<i32> {
        let (recorded_takeover, mut host) = match peer {
            Peer::Local { recorded_takeover } => (Some(recorded_takeover), None),
            Peer::Host(end) => (None, Some(*end)),
        };
        // Only a version 2 attach knows where to resume from.
        let resumable = resume.is_some();
        let retired = Arc::new(AtomicBool::new(false));
        let screen = Arc::new(Screen {
            shown: Mutex::new(Shown {
                modes: TermModes::default(),
                resume,
            }),
            done: AtomicBool::new(false),
            bracketed_paste: AtomicBool::new(false),
            broken: OnceLock::new(),
            terminal_gone: AtomicBool::new(false),
        });
        let status = Arc::new(reconnect::Status::default());

        // Installed before raw mode, so no cancelling signal can arrive between
        // the two and leave the terminal raw; restored when dropped, after the
        // terminal.
        let handlers = signals::Handlers::install()
            .map_err(|e| anyhow::anyhow!("failed to install attach signal handlers: {e}"))?;
        // Restores the terminal on every way out of this function, panics
        // included.
        let terminal = RawTerminal::enter()?;
        // From here on, what ssh says stays off the session's screen.
        if let Some(end) = &host {
            end.stderr.set(&status, &screen);
        }

        let mut size = terminal_size();
        let mut connection = Some(Connection::open(link, access, &retired, &screen));
        let mut reconnecting: Option<reconnect::Job> = None;
        if let Some(connection) = &connection {
            connection.greet(size);
        }
        // The remote's exit code, or its failure, when a reconnect ended the
        // attach.
        let mut verdict = None;

        let size_poll = size_poll_interval();
        let mut size_due = Instant::now() + size_poll;
        let mut parser = EscapeParser::new(escape);
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 4096];
        let mut dropped = 0usize;
        let ending = loop {
            if connection.as_ref().is_some_and(Connection::closed) {
                let lost = resumable
                    && !retired.load(Ordering::SeqCst)
                    && screen.broken.get().is_none()
                    && !screen.terminal_gone.load(Ordering::SeqCst);
                match (host.take(), connection.take()) {
                    (Some(end), Some(old)) if lost => {
                        reconnecting = Some(reconnect::Job::start(
                            end,
                            old.close(),
                            Arc::clone(&screen),
                            Arc::clone(&status),
                        ));
                    }
                    (end, old) => {
                        (host, connection) = (end, old);
                        break Ending::SessionClosed;
                    }
                }
            }
            if let Some((end, outcome)) = reconnecting.as_ref().and_then(reconnect::Job::outcome) {
                reconnecting = None;
                host = end;
                match outcome {
                    reconnect::Outcome::Resumed(link) => {
                        let resumed = Connection::open(link, access, &retired, &screen);
                        resumed.greet(size);
                        connection = Some(resumed);
                    }
                    reconnect::Outcome::Ended(result) => {
                        verdict = Some(result);
                        break Ending::SessionClosed;
                    }
                }
            }
            let mut timeout = size_due.saturating_duration_since(Instant::now()).min(POLL);
            if let Some(due) = parser.partial_deadline() {
                timeout = timeout.min(due.saturating_duration_since(Instant::now()));
            }
            let ready = wait_ready(handlers.wake(), timeout);
            if ready.signal {
                let received = handlers.take();
                if let Some(cancel) = received.cancel {
                    break Ending::Signalled(cancel);
                }
                if received.winch {
                    // Signals coalesce, so SIGWINCH only brings the check
                    // forward; the poll stays the source of truth.
                    size_due = Instant::now();
                }
            }
            if Instant::now() >= size_due {
                size_due = Instant::now() + size_poll;
                let now = terminal_size();
                if now.is_some() && now != size {
                    size = now;
                    // While reconnecting, the size goes with the next
                    // connection's greeting.
                    if let (Some(connection), Some(size)) = (&connection, now) {
                        connection.resize(size);
                    }
                }
            }
            if !ready.keyboard {
                // An escape sequence left unfinished by the last read (a lone
                // Esc key, say) is forwarded once it has waited long enough.
                let mut forward = Vec::new();
                parser.flush_expired(Instant::now(), &mut forward);
                send_keystrokes(
                    connection.as_ref(),
                    access,
                    &status,
                    &screen,
                    forward,
                    &mut dropped,
                );
                continue;
            }
            let n = match stdin.read(&mut buf) {
                Ok(0) => break Ending::KeyboardClosed,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break Ending::KeyboardClosed,
            };
            let mut forward = Vec::new();
            parser.set_bracketed_paste(screen.bracketed_paste.load(Ordering::SeqCst));
            let decision = parser.feed(&buf[..n], &mut forward);
            send_keystrokes(
                connection.as_ref(),
                access,
                &status,
                &screen,
                forward,
                &mut dropped,
            );
            if decision == Escape::Detach {
                break Ending::Detached;
            }
        };

        if let Some(connection) = &connection {
            if matches!(ending, Ending::Detached | Ending::Signalled(_)) {
                connection.outbox.push_control(Frame::Detach);
                connection.outbox.wait_controls_sent(DETACH_GRACE);
            }
        }
        // Unblocks the sender and reader, which are not waited for.
        drop(connection.map(Connection::close));
        // Leaving while reconnecting: stop it, and its ssh. A connection it
        // resumed just now ends as it is dropped, which closes ssh's stdin.
        if let Some(job) = reconnecting {
            if let Some((end, _)) = job.stop(RECONNECT_STOP_GRACE) {
                host = end;
            }
        }
        turn_off_modes(
            &screen,
            &status,
            if matches!(ending, Ending::Signalled(_)) {
                REPORT_GRACE
            } else {
                MODES_GRACE
            },
        );
        drop(terminal);
        drop(handlers);

        // Only report once the terminal is restored, and never after a user
        // detach, whose terminal may be the thing that is blocked. A client
        // that was not reading when it was taken over never got the message,
        // so at the end of its connection the event log decides.
        let taken_over = ending != Ending::Detached
            && (retired.load(Ordering::SeqCst)
                || (ending == Ending::SessionClosed && recorded_takeover.is_some_and(|f| f())));
        // What the remote said on the reconnect's last attempt, such as why
        // it refused to resume, which the status line could only glimpse.
        let mut notes = status.take_lines();
        if let Ending::Signalled(cancel) = ending {
            notes.push(format!("tendr: attach ended by {}", cancel.name()));
        }
        if ending != Ending::Detached {
            // Otherwise the takeover is the error returned below.
            if taken_over && matches!(ending, Ending::Signalled(_)) {
                notes.push("tendr: another client took over this session".to_owned());
            }
            if dropped > 0 {
                notes.push(format!(
                    "tendr: {dropped} typed bytes were dropped while the session was not accepting input"
                ));
            }
        }
        if matches!(ending, Ending::Signalled(_)) {
            exit_signalled(notes);
        }
        for note in notes {
            eprintln!("{note}");
        }
        let finished = host.as_mut().and_then(|end| {
            end.finish(if ending == Ending::SessionClosed {
                super::SSH_CLOSE_GRACE
            } else {
                super::SSH_LEAVE_GRACE
            })
        });
        if ending == Ending::SessionClosed {
            if let Some(why) = screen.broken.get() {
                return Err(PtyExitCode::Protocol.error(anyhow::anyhow!("tendr: {why}")));
            }
        }
        if taken_over {
            // This client lost control: a controller conflict, which a script
            // must be able to tell from a detach.
            return Err(PtyExitCode::Control.error(anyhow::anyhow!(
                "tendr: another client took over this session"
            )));
        }
        if ending != Ending::SessionClosed {
            return Ok(0);
        }
        // The far side ended the connection: the remote's code, or ssh's
        // failure (a lost version 1 connection) when there is no code.
        verdict.or(finished).unwrap_or(Ok(0))
    }

    /// Send keystrokes to the session, counting them in `dropped` if the
    /// outbox is full. With no connection, while reconnecting, they are
    /// discarded: never sent late to a program that has moved on. A
    /// read-only attach drops them all, and says so once.
    fn send_keystrokes(
        connection: Option<&Connection>,
        access: Access,
        status: &Arc<reconnect::Status>,
        screen: &Arc<Screen>,
        forward: Vec<u8>,
        dropped: &mut usize,
    ) {
        if forward.is_empty() {
            return;
        }
        if access == Access::ReadOnly {
            status.read_only(screen);
            return;
        }
        let Some(connection) = connection else {
            status.discard(screen);
            return;
        };
        let len = forward.len();
        if !connection.outbox.push_data(forward) {
            *dropped += len;
        }
    }

    /// How a relay ended.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Ending {
        Detached,
        SessionClosed,
        KeyboardClosed,
        Signalled(signals::Cancel),
    }

    /// Leave after a cancelling signal with exit code 1. The report is written
    /// without waiting on the terminal for long, and without panicking if it is
    /// gone: SIGHUP means it hung up, and a terminal that stopped reading is a
    /// reason to send SIGTERM.
    fn exit_signalled(notes: Vec<String>) -> ! {
        let (done, written) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut stderr = std::io::stderr().lock();
            for note in notes {
                let _ = writeln!(stderr, "{note}");
            }
            let _ = done.send(());
        });
        let _ = written.recv_timeout(REPORT_GRACE);
        std::process::exit(1);
    }

    /// What the session's output has done to the user's terminal.
    pub(super) struct Screen {
        /// What has been written to the terminal. Held while writing, so a
        /// reset cannot interleave with output, and so the resume point moves
        /// only past bytes the terminal took.
        shown: Mutex<Shown>,
        /// Set by the main thread when leaving: the reader writes nothing
        /// more, so at most the chunk it is writing precedes the reset.
        done: AtomicBool,
        /// The child has bracketed paste on. Read by the keyboard loop
        /// without taking `shown`, which a blocked write can hold.
        bracketed_paste: AtomicBool,
        /// Why the reader stopped on output outside the protocol, if it did.
        /// Set before it marks the connection closed.
        broken: OnceLock<String>,
        /// The terminal refused a write: the reader stopped for the
        /// terminal's sake, not the connection's, so there is nothing to
        /// reconnect for. Set before it marks the connection closed.
        terminal_gone: AtomicBool,
    }

    impl Screen {
        fn lock(&self) -> MutexGuard<'_, Shown> {
            self.shown.lock().unwrap_or_else(|e| e.into_inner())
        }
    }

    /// The terminal as the session's output has left it.
    struct Shown {
        /// The modes turned on by everything written so far.
        modes: TermModes,
        /// For a version 2 attach, the run and epoch it was accepted at, and
        /// the offset just past the last output byte written to the terminal:
        /// where a reconnect would resume. `None` for version 1.
        resume: Option<attach_proto::ResumePoint>,
    }

    /// Turn off the terminal modes the session turned on (the alternate
    /// screen, mouse reports, keyboard protocols), and erase a reconnect's
    /// status line, so the user's shell gets its terminal back. Waits for a
    /// write in progress, but not for longer than `grace`: a terminal that is
    /// not reading cannot hold the exit.
    fn turn_off_modes(screen: &Arc<Screen>, status: &Arc<reconnect::Status>, grace: Duration) {
        screen.done.store(true, Ordering::SeqCst);
        let (done, written) = std::sync::mpsc::channel();
        let (screen, status) = (Arc::clone(screen), Arc::clone(status));
        std::thread::spawn(move || {
            status.leave(&screen);
            let _ = done.send(());
        });
        let _ = written.recv_timeout(grace);
    }

    /// Write `bytes` to the terminal, if it takes them. The caller holds
    /// [`Screen::shown`], so nothing else writes in between.
    fn write_terminal(bytes: &[u8]) {
        if !bytes.is_empty() {
            let mut stdout = std::io::stdout().lock();
            let _ = stdout.write_all(bytes).and_then(|()| stdout.flush());
        }
    }

    /// Raw mode for the user's terminal, restored on drop.
    struct RawTerminal {
        original: libc::termios,
    }

    impl RawTerminal {
        fn enter() -> anyhow::Result<Self> {
            use std::os::unix::io::AsRawFd;
            let fd = std::io::stdin().as_raw_fd();
            // SAFETY: termios is plain old data; tcgetattr fills it completely.
            let mut original: libc::termios = unsafe { std::mem::zeroed() };
            // SAFETY: `fd` is this process's stdin; `original` is a valid out-pointer.
            if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
                anyhow::bail!("failed to get terminal attributes");
            }
            let mut raw = original;
            // SAFETY: `raw` is a valid termios.
            unsafe { libc::cfmakeraw(&mut raw) };
            // SAFETY: `fd` is stdin; `raw` is a valid termios.
            if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &raw) } != 0 {
                anyhow::bail!("failed to set raw mode");
            }
            Ok(Self { original })
        }
    }

    impl Drop for RawTerminal {
        fn drop(&mut self) {
            use std::os::unix::io::AsRawFd;
            let fd = std::io::stdin().as_raw_fd();
            // Restore at once, then discard unread typed-ahead input. Not
            // TCSAFLUSH/TCSADRAIN: those also wait for pending output to drain,
            // which never happens if the terminal has stopped reading. The input
            // flush must come after: re-entering canonical mode makes the BSD
            // tty driver set PENDIN, and flushing input is what clears it.
            // SAFETY: `fd` is stdin; `original` came from tcgetattr on this fd;
            // tcflush takes a queue selector.
            unsafe {
                libc::tcsetattr(fd, libc::TCSANOW, &self.original);
                libc::tcflush(fd, libc::TCIFLUSH);
            }
        }
    }

    /// Messages waiting for the sender thread. Controls go first, on a
    /// bounded path: at most one size waits (a newer one replaces it), and
    /// the others are sent once each. Input waits behind them, bounded by
    /// [`OUTBOX_DATA_LIMIT`].
    #[derive(Default)]
    pub(super) struct Outbox {
        state: Mutex<OutboxState>,
        changed: Condvar,
    }

    #[derive(Default)]
    struct OutboxState {
        controls: VecDeque<Frame>,
        data: VecDeque<Vec<u8>>,
        data_bytes: usize,
        sending_control: bool,
        closed: bool,
    }

    impl Outbox {
        fn lock(&self) -> MutexGuard<'_, OutboxState> {
            self.state.lock().unwrap_or_else(|e| e.into_inner())
        }

        pub(super) fn push_control(&self, frame: Frame) {
            let mut state = self.lock();
            let queued_size = matches!(frame, Frame::Resize(_))
                .then(|| {
                    state
                        .controls
                        .iter_mut()
                        .find(|queued| matches!(queued, Frame::Resize(_)))
                })
                .flatten();
            match queued_size {
                Some(queued) => *queued = frame,
                None => state.controls.push_back(frame),
            }
            drop(state);
            self.changed.notify_all();
        }

        /// Queue keystrokes; `false` if dropped because the backlog is full.
        fn push_data(&self, bytes: Vec<u8>) -> bool {
            let mut state = self.lock();
            if state.data_bytes + bytes.len() > OUTBOX_DATA_LIMIT {
                return false;
            }
            state.data_bytes += bytes.len();
            state.data.push_back(bytes);
            drop(state);
            self.changed.notify_all();
            true
        }

        /// Queue input, waiting while the backlog is full; `false` once
        /// closed.
        pub(super) fn push_data_waiting(&self, bytes: Vec<u8>) -> bool {
            let mut state = self.lock();
            while !state.closed
                && state.data_bytes > 0
                && state.data_bytes + bytes.len() > OUTBOX_DATA_LIMIT
            {
                state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            if state.closed {
                return false;
            }
            state.data_bytes += bytes.len();
            state.data.push_back(bytes);
            drop(state);
            self.changed.notify_all();
            true
        }

        /// The next message to send, controls first; `None` once closed.
        fn next(&self) -> Option<Frame> {
            let mut state = self.lock();
            loop {
                if state.closed {
                    return None;
                }
                if let Some(control) = state.controls.pop_front() {
                    state.sending_control = true;
                    return Some(control);
                }
                if let Some(data) = state.data.pop_front() {
                    state.data_bytes -= data.len();
                    // Room for input that waits for it.
                    self.changed.notify_all();
                    return Some(Frame::Data(data));
                }
                state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
            }
        }

        fn sent(&self) {
            self.lock().sending_control = false;
            self.changed.notify_all();
        }

        /// Wait up to `grace` for queued controls to be written.
        pub(super) fn wait_controls_sent(&self, grace: Duration) {
            let deadline = Instant::now() + grace;
            let mut state = self.lock();
            while !state.closed && (!state.controls.is_empty() || state.sending_control) {
                let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                    return;
                };
                state = self
                    .changed
                    .wait_timeout(state, left)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        }

        pub(super) fn close(&self) {
            self.lock().closed = true;
            self.changed.notify_all();
        }
    }

    /// Write the outbox's messages to `stream` until it closes. A failed
    /// write sets `closed` and closes the outbox. Dropping `stream` at the end
    /// closes it, which ends ssh's input for `attach --host`.
    pub(super) fn spawn_sender(
        mut stream: impl Write + Send + 'static,
        outbox: Arc<Outbox>,
        closed: Arc<AtomicBool>,
    ) {
        std::thread::spawn(move || {
            while let Some(frame) = outbox.next() {
                let written = attach_proto::write_frame(&mut stream, &frame);
                outbox.sent();
                if written.is_err() {
                    closed.store(true, Ordering::SeqCst);
                    outbox.close();
                    return;
                }
            }
        });
    }

    fn spawn_reader(
        mut stream: impl Read + Send + 'static,
        closed: Arc<AtomicBool>,
        retired: Arc<AtomicBool>,
        screen: Arc<Screen>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            loop {
                let shown = match attach_proto::read_frame(&mut stream) {
                    Ok(Frame::Data(payload)) => show(&screen, None, &payload),
                    Ok(Frame::Output { offset, bytes }) => show(&screen, Some(offset), &bytes),
                    Ok(Frame::Retired(_)) => {
                        retired.store(true, Ordering::SeqCst);
                        Ok(true)
                    }
                    Ok(_) | Err(FrameError::Malformed(_) | FrameError::Unknown(_)) => Ok(true),
                    Err(FrameError::Io(_)) => Ok(false),
                };
                match shown {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(why) => {
                        let _ = screen.broken.set(why);
                        break;
                    }
                }
            }
            closed.store(true, Ordering::SeqCst);
        })
    }

    /// Write the session's output to the terminal: `Ok(true)` to go on,
    /// `Ok(false)` once leaving or the terminal is gone. Stamped output
    /// (`offset`) must start exactly where the last ended, or it is refused
    /// unwritten, with why; it moves the resume point only once the terminal
    /// has taken all of it. Unstamped output, such as the mode replay, moves
    /// nothing.
    fn show(screen: &Screen, offset: Option<u64>, bytes: &[u8]) -> Result<bool, String> {
        let mut shown = screen.shown.lock().unwrap_or_else(|e| e.into_inner());
        if screen.done.load(Ordering::SeqCst) {
            return Ok(false);
        }
        if let Some(offset) = offset {
            match &shown.resume {
                None => return Err("the session sent offsets to a version 1 attach".to_owned()),
                Some(point) if point.offset != offset => {
                    return Err(format!(
                        "the session's output skipped from offset {} to {offset}",
                        point.offset
                    ));
                }
                Some(_) => {}
            }
        }
        // Track before writing: modes from a partial write are still turned
        // off when leaving.
        shown.modes.feed(bytes);
        screen
            .bracketed_paste
            .store(shown.modes.bracketed_paste(), Ordering::SeqCst);
        let mut stdout = std::io::stdout().lock();
        if stdout.write_all(bytes).is_err() || stdout.flush().is_err() {
            screen.terminal_gone.store(true, Ordering::SeqCst);
            return Ok(false);
        }
        if let (Some(_), Some(point)) = (offset, shown.resume.as_mut()) {
            point.offset += bytes.len() as u64;
        }
        Ok(true)
    }

    /// What woke the main thread's poll.
    #[derive(Debug, Default)]
    struct Ready {
        /// The keyboard has input, or has closed.
        keyboard: bool,
        /// A signal arrived.
        signal: bool,
    }

    /// Wait up to `timeout` for the keyboard or for a signal's wake-up byte.
    fn wait_ready(wake: std::os::fd::BorrowedFd<'_>, timeout: Duration) -> Ready {
        use rustix::event::{PollFd, PollFlags, poll};
        let stdin = std::io::stdin();
        let mut fds = [
            PollFd::new(&stdin, PollFlags::IN),
            PollFd::new(&wake, PollFlags::IN),
        ];
        let timespec = poll_timespec(timeout);
        match poll(&mut fds, Some(&timespec)) {
            Ok(n) if n > 0 => Ready {
                keyboard: !fds[0].revents().is_empty(),
                signal: !fds[1].revents().is_empty(),
            },
            // A timeout, or EINTR from a signal whose byte the next poll sees.
            _ => Ready::default(),
        }
    }

    /// How often the terminal size is checked. Debug builds accept
    /// `TENDR_TEST_ATTACH_SIZE_POLL_MS` so tests can tell a size change
    /// forwarded on SIGWINCH from one found by this poll; release builds ignore
    /// it.
    fn size_poll_interval() -> Duration {
        if cfg!(debug_assertions) {
            if let Some(ms) = std::env::var("TENDR_TEST_ATTACH_SIZE_POLL_MS")
                .ok()
                .and_then(|v| v.parse().ok())
            {
                return Duration::from_millis(ms);
            }
        }
        SIZE_POLL
    }

    /// The poll timeout for `timeout`, whole seconds included (saturating for a
    /// duration too long to represent).
    fn poll_timespec(timeout: Duration) -> rustix::event::Timespec {
        rustix::event::Timespec::try_from(timeout).unwrap_or(rustix::event::Timespec {
            tv_sec: i64::MAX,
            tv_nsec: 999_999_999,
        })
    }

    /// The terminal's size; `None` if unknown or if either dimension is zero,
    /// which the session could not apply.
    fn terminal_size() -> Option<tendr::recording::Geometry> {
        use std::os::unix::io::AsRawFd;
        let fd = std::io::stdout().as_raw_fd();
        // SAFETY: winsize is plain old data; TIOCGWINSZ fills it on success.
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: `fd` is stdout; TIOCGWINSZ takes a winsize out-pointer.
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0 {
            tendr::recording::Geometry::new(ws.ws_row, ws.ws_col)
        } else {
            None
        }
    }

    /// The attach client's signal handling: a self-pipe that the handlers
    /// write to, so the main thread's `poll()` wakes for a signal whichever
    /// thread the kernel delivered it to.
    pub(super) mod signals {
        use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
        use std::sync::OnceLock;
        use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

        /// A signal that ends the attach.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Cancel {
            Hangup,
            Terminate,
            Interrupt,
        }

        impl Cancel {
            const ALL: [Self; 3] = [Self::Hangup, Self::Terminate, Self::Interrupt];

            fn number(self) -> libc::c_int {
                match self {
                    Self::Hangup => libc::SIGHUP,
                    Self::Terminate => libc::SIGTERM,
                    Self::Interrupt => libc::SIGINT,
                }
            }

            pub fn name(self) -> &'static str {
                match self {
                    Self::Hangup => "SIGHUP",
                    Self::Terminate => "SIGTERM",
                    Self::Interrupt => "SIGINT",
                }
            }
        }

        /// The signals received since the last [`Handlers::take`].
        #[derive(Debug, Default)]
        pub struct Received {
            /// The first cancelling signal, in [`Cancel::ALL`] order.
            pub cancel: Option<Cancel>,
            pub winch: bool,
        }

        /// A bit per signal number; the pipe only wakes the poll, so a full
        /// pipe cannot lose a signal.
        static PENDING: AtomicU32 = AtomicU32::new(0);
        /// The wake pipe's write end, or -1 before the first install.
        static WAKE_WRITE: AtomicI32 = AtomicI32::new(-1);

        fn bit(signal: libc::c_int) -> u32 {
            1u32 << (signal as u32 % 32)
        }

        /// Async-signal-safe only: atomics, `write(2)`, and errno kept intact
        /// for the code it interrupted.
        extern "C" fn on_signal(signal: libc::c_int) {
            PENDING.fetch_or(bit(signal), Ordering::SeqCst);
            let fd = WAKE_WRITE.load(Ordering::SeqCst);
            if fd < 0 {
                return;
            }
            let saved = errno::get();
            let byte = 1u8;
            // SAFETY: write(2) is async-signal-safe; `fd` is the wake pipe's
            // write end, which is never closed. A full pipe (EAGAIN) already
            // holds a wake-up.
            unsafe { libc::write(fd, std::ptr::addr_of!(byte).cast(), 1) };
            errno::set(saved);
        }

        /// The process's wake pipe `(read, write)`, both ends nonblocking and
        /// close-on-exec. Created once and never closed, so a handler still
        /// running on another thread after the dispositions are restored
        /// cannot write to a reused descriptor.
        fn wake_pipe() -> std::io::Result<&'static (OwnedFd, OwnedFd)> {
            static PIPE: OnceLock<Result<(OwnedFd, OwnedFd), rustix::io::Errno>> = OnceLock::new();
            PIPE.get_or_init(|| {
                // pipe2 is not on every Unix; the attach client spawns no
                // processes, so setting close-on-exec afterwards cannot leak.
                let (read, write) = rustix::pipe::pipe()?;
                for end in [&read, &write] {
                    rustix::io::fcntl_setfd(end, rustix::io::FdFlags::CLOEXEC)?;
                    rustix::fs::fcntl_setfl(end, rustix::fs::OFlags::NONBLOCK)?;
                }
                Ok((read, write))
            })
            .as_ref()
            .map_err(|&e| e.into())
        }

        /// Handlers installed for the relay; the previous dispositions come
        /// back on drop.
        pub struct Handlers {
            wake: BorrowedFd<'static>,
            previous: Vec<(libc::c_int, libc::sigaction)>,
        }

        impl Handlers {
            /// Handle SIGHUP, SIGTERM, SIGINT and SIGWINCH. A cancelling
            /// signal the process inherited as ignored (`nohup`, or SIGINT in
            /// a shell's background job) stays ignored.
            pub fn install() -> std::io::Result<Self> {
                let (read, write) = wake_pipe()?;
                WAKE_WRITE.store(write.as_raw_fd(), Ordering::SeqCst);
                let mut handlers = Self {
                    wake: read.as_fd(),
                    previous: Vec::new(),
                };
                // Nothing from an earlier relay in this process carries over.
                handlers.take();
                let wanted = Cancel::ALL
                    .iter()
                    .map(|c| (c.number(), true))
                    .chain([(libc::SIGWINCH, false)]);
                for (signal, keep_ignored) in wanted {
                    // SAFETY: sigaction is plain old data; sigaction(2) fills
                    // `old`, and a null new action only queries.
                    let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
                    if unsafe { libc::sigaction(signal, std::ptr::null(), &mut old) } != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if keep_ignored && old.sa_sigaction == libc::SIG_IGN {
                        continue;
                    }
                    // SAFETY: as above; `new` is fully initialized, and the
                    // handler is async-signal-safe.
                    let mut new: libc::sigaction = unsafe { std::mem::zeroed() };
                    new.sa_sigaction =
                        on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
                    new.sa_flags = libc::SA_RESTART;
                    unsafe { libc::sigemptyset(&mut new.sa_mask) };
                    if unsafe { libc::sigaction(signal, &new, std::ptr::null_mut()) } != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    handlers.previous.push((signal, old));
                }
                Ok(handlers)
            }

            /// The descriptor that becomes readable when a signal arrives.
            pub fn wake(&self) -> BorrowedFd<'static> {
                self.wake
            }

            /// Empty the wake pipe and return what arrived.
            pub fn take(&self) -> Received {
                let mut sink = [0u8; 64];
                while matches!(rustix::io::read(self.wake, &mut sink), Ok(n) if n > 0) {}
                // After the drain: a signal landing now leaves a byte behind
                // for the next poll.
                let pending = PENDING.swap(0, Ordering::SeqCst);
                Received {
                    cancel: Cancel::ALL
                        .into_iter()
                        .find(|c| pending & bit(c.number()) != 0),
                    winch: pending & bit(libc::SIGWINCH) != 0,
                }
            }
        }

        impl Drop for Handlers {
            fn drop(&mut self) {
                for (signal, old) in self.previous.iter().rev() {
                    // SAFETY: `old` is the disposition sigaction(2) returned
                    // for `signal`.
                    unsafe { libc::sigaction(*signal, old, std::ptr::null_mut()) };
                }
            }
        }

        /// errno, saved and restored around the handler's `write(2)`.
        mod errno {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            fn location() -> *mut libc::c_int {
                // SAFETY: returns this thread's errno; always valid.
                unsafe { libc::__errno_location() }
            }

            #[cfg(any(
                target_vendor = "apple",
                target_os = "freebsd",
                target_os = "dragonfly"
            ))]
            fn location() -> *mut libc::c_int {
                // SAFETY: returns this thread's errno; always valid.
                unsafe { libc::__error() }
            }

            #[cfg(not(any(
                target_os = "linux",
                target_os = "android",
                target_vendor = "apple",
                target_os = "freebsd",
                target_os = "dragonfly"
            )))]
            fn location() -> *mut libc::c_int {
                std::ptr::null_mut()
            }

            pub fn get() -> libc::c_int {
                let p = location();
                // SAFETY: a non-null `p` is this thread's errno.
                if p.is_null() { 0 } else { unsafe { *p } }
            }

            pub fn set(value: libc::c_int) {
                let p = location();
                if !p.is_null() {
                    // SAFETY: a non-null `p` is this thread's errno.
                    unsafe { *p = value };
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn disposition(signal: libc::c_int) -> libc::sighandler_t {
            // SAFETY: a null new action only queries the current one.
            let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe { libc::sigaction(signal, std::ptr::null(), &mut current) },
                0
            );
            current.sa_sigaction
        }

        /// One test, because the handlers are process-wide.
        #[test]
        fn handlers_wake_on_signals_keep_inherited_ignores_and_restore_dispositions() {
            // SAFETY: SIGHUP ignored for the length of this test, then reset.
            let hup_before = unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };
            let winch_before = disposition(libc::SIGWINCH);

            let handlers = signals::Handlers::install().unwrap();
            assert_eq!(
                disposition(libc::SIGHUP),
                libc::SIG_IGN,
                "an inherited ignore is kept"
            );
            assert_ne!(disposition(libc::SIGWINCH), winch_before);

            // SAFETY: raise(3) delivers SIGWINCH to this thread, whose handler
            // is installed.
            unsafe { libc::raise(libc::SIGWINCH) };
            let ready = wait_ready(handlers.wake(), Duration::from_secs(5));
            assert!(ready.signal, "SIGWINCH wakes the poll");
            let received = handlers.take();
            assert!(received.winch);
            assert_eq!(received.cancel, None);
            let ready = wait_ready(handlers.wake(), Duration::ZERO);
            assert!(!ready.signal, "the wake pipe was drained");

            drop(handlers);
            assert_eq!(disposition(libc::SIGWINCH), winch_before);
            // SAFETY: restores SIGHUP's disposition from before the test.
            unsafe { libc::signal(libc::SIGHUP, hup_before) };
        }

        #[test]
        fn poll_timespec_carries_whole_seconds() {
            let ts = poll_timespec(Duration::from_millis(1500));
            assert_eq!((ts.tv_sec, ts.tv_nsec), (1, 500_000_000));
            let ts = poll_timespec(Duration::from_millis(100));
            assert_eq!((ts.tv_sec, ts.tv_nsec), (0, 100_000_000));
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{Accept, Resumption, exchange, hello_reply, resume_reply, resume_request};
    use std::io::{self, Write};
    use std::os::unix::net::UnixStream;
    use tendr::attach_proto::{
        Frame, FrameError, ProtocolError, RejectClass, ResumePoint, WatchPoint,
    };
    use tendr::attach_proto::{Mode, Version, write_frame};
    use tendr::model::ids::RunId;
    use tendr::model::pty_control::ControllerEpoch;
    use tendr::pty_exit::exit_code;

    fn code(reply: Result<Frame, FrameError>) -> Option<i32> {
        hello_reply(reply, Mode::Attach, Version::V1)
            .err()
            .map(|e| exit_code(&e))
    }

    /// A resume carries its point in the request, never takes over, and
    /// sends the resume frame where the hello goes. A 0.5.0 remote tendr,
    /// which ignores the request's unknown fields and requires the exact
    /// version 1 hello, refuses it rather than attach afresh.
    #[test]
    fn a_resume_sends_its_point_in_the_request_and_in_place_of_the_hello() {
        let request = crate::remote_op::AttachRequest {
            session: "s".to_owned(),
            namespace: None,
            takeover: true,
            read_only: false,
            escape: tendr::attach_escape::EscapeMode::CtrlBackslash,
            protocol: 2,
            resume: None,
        };
        let point = ResumePoint {
            run_id: RunId::new(),
            epoch: ControllerEpoch::new(4),
            offset: 99,
        };
        let sent = resume_request(&request, point).unwrap();
        let len = u32::from_be_bytes(sent[..4].try_into().unwrap()) as usize;
        let json: serde_json::Value = serde_json::from_slice(&sent[4..4 + len]).unwrap();
        assert_eq!(
            json["params"]["resume"],
            serde_json::json!({
                "run_id": point.run_id.as_uuid().to_string(),
                "epoch": 4,
                "offset": 99
            })
        );
        assert_eq!(json["params"]["takeover"], false);
        let mut rest = &sent[4 + len..];
        assert_eq!(
            tendr::attach_proto::read_frame(&mut rest).unwrap(),
            Frame::Resume(point)
        );
        assert!(rest.is_empty());
    }

    /// The hello and its reply, as a client speaking `version` sees them.
    fn hello(stream: &mut UnixStream, version: Version, mode: Mode) -> anyhow::Result<Accept> {
        hello_reply(
            exchange(stream, &Frame::Hello { version, mode }, "attach")?,
            mode,
            version,
        )
    }

    #[test]
    fn a_refusal_exits_with_its_class_code() {
        for (class, expected) in [
            (RejectClass::Protocol, 80),
            (RejectClass::Control, 81),
            (RejectClass::Runtime, 84),
            (RejectClass::Identity, 85),
        ] {
            let refusal = Frame::Rejected {
                class,
                reason: "why".to_owned(),
            };
            assert_eq!(code(Ok(refusal)), Some(expected), "{class:?}");
        }
        assert_eq!(code(Ok(Frame::Accepted(ControllerEpoch::new(1)))), None);
    }

    /// A version 2 acceptance is one only to a client that speaks version 2;
    /// such a client also takes a version 1 acceptance, from a session that
    /// speaks only version 1.
    #[test]
    fn a_v2_acceptance_is_accepted_only_by_a_v2_client() {
        let point = ResumePoint {
            run_id: RunId::new(),
            epoch: ControllerEpoch::new(3),
            offset: 42,
        };
        assert_eq!(code(Ok(Frame::AcceptedV2(point))), Some(80));
        assert_eq!(
            hello_reply(Ok(Frame::AcceptedV2(point)), Mode::Attach, Version::V2).unwrap(),
            Accept::V2(point)
        );
        let epoch = ControllerEpoch::new(4);
        assert_eq!(
            hello_reply(Ok(Frame::Accepted(epoch)), Mode::Attach, Version::V2).unwrap(),
            Accept::V1(epoch)
        );
    }

    /// A watch hello is answered only by a watcher's admission, which holds
    /// no epoch: control granted to it is a protocol failure, never an
    /// attach with control, and a watcher's admission answers no other hello.
    #[test]
    fn only_a_watch_hello_is_admitted_as_a_watcher_and_never_with_control() {
        let point = WatchPoint {
            run_id: RunId::new(),
            offset: 7,
        };
        let watch = |reply, version| {
            hello_reply(Ok(reply), Mode::Watch, version).map_err(|e| exit_code(&e))
        };
        for version in [Version::V1, Version::V2] {
            let accept = watch(Frame::Watching(point), version).unwrap();
            assert_eq!(accept, Accept::Watch(point));
            assert_eq!(accept.epoch(), None);
        }
        let granted = ResumePoint {
            run_id: point.run_id,
            epoch: ControllerEpoch::new(1),
            offset: 7,
        };
        assert_eq!(watch(Frame::Accepted(granted.epoch), Version::V1), Err(80));
        assert_eq!(watch(Frame::AcceptedV2(granted), Version::V2), Err(80));
        assert_eq!(code(Ok(Frame::Watching(point))), Some(80));
    }

    /// A resume's answer counts only if it is for the run and offset asked,
    /// at a later epoch; a gone answer must name a span that starts after
    /// that offset. Anything else, a fresh acceptance included, is a
    /// protocol failure, and a refusal exits with its class's code.
    #[test]
    fn a_resume_answer_must_fit_the_resume() {
        let asked = ResumePoint {
            run_id: RunId::new(),
            epoch: ControllerEpoch::new(3),
            offset: 100,
        };
        let code = |reply| resume_reply(Ok(reply), asked).err().map(|e| exit_code(&e));
        let resumed = ResumePoint {
            epoch: ControllerEpoch::new(5),
            ..asked
        };
        assert_eq!(
            resume_reply(Ok(Frame::Resumed(resumed)), asked).unwrap(),
            Resumption::Resumed(resumed)
        );
        let head = ResumePoint {
            offset: 500,
            ..resumed
        };
        let gone = Frame::Gone {
            point: head,
            oldest: 101,
        };
        assert_eq!(
            resume_reply(Ok(gone), asked).unwrap(),
            Resumption::Gone {
                point: head,
                oldest: 101
            }
        );

        for wrong in [
            Frame::Resumed(ResumePoint {
                run_id: RunId::new(),
                ..resumed
            }),
            Frame::Resumed(asked),
            Frame::Resumed(ResumePoint {
                offset: 99,
                ..resumed
            }),
            Frame::Gone {
                point: head,
                oldest: 100,
            },
            Frame::Gone {
                point: ResumePoint {
                    offset: 50,
                    ..resumed
                },
                oldest: 10,
            },
            Frame::Accepted(ControllerEpoch::new(4)),
            Frame::AcceptedV2(head),
        ] {
            assert_eq!(code(wrong.clone()), Some(80), "{wrong:?}");
        }
        let refusal = Frame::Rejected {
            class: RejectClass::Control,
            reason: "taken".to_owned(),
        };
        assert_eq!(code(refusal), Some(81));
    }

    /// Anything but a v1 reply, or no reply before the deadline, is a
    /// protocol failure; a connection that ends first is a transport failure.
    #[test]
    fn a_reply_outside_the_protocol_exits_80_and_a_lost_connection_84() {
        assert_eq!(code(Ok(Frame::Data(b"output".to_vec()))), Some(80));
        assert_eq!(code(Err(FrameError::Unknown(0x7f))), Some(80));
        assert_eq!(
            code(Err(FrameError::Malformed(
                ProtocolError::UnknownRejectClass(b'b')
            ))),
            Some(80)
        );
        for kind in [io::ErrorKind::WouldBlock, io::ErrorKind::TimedOut] {
            assert_eq!(code(Err(FrameError::Io(kind.into()))), Some(80), "{kind:?}");
        }
        for kind in [io::ErrorKind::UnexpectedEof, io::ErrorKind::ConnectionReset] {
            assert_eq!(code(Err(FrameError::Io(kind.into()))), Some(84), "{kind:?}");
        }
    }

    /// A sidecar whose end of the connection is already closed, after it wrote
    /// `sent` (nothing, or a frame), as the real one does when it refuses a
    /// connection before reading its hello.
    fn closed_peer(sent: Option<Frame>) -> UnixStream {
        let (client, mut sidecar) = UnixStream::pair().unwrap();
        if let Some(frame) = sent {
            write_frame(&mut sidecar, &frame).unwrap();
        }
        drop(sidecar);
        // Precondition: the hello cannot be written, so these tests exercise
        // the refusal that arrived before the hello.
        assert!(client.try_clone().unwrap().write_all(&[0]).is_err());
        client
    }

    /// The sidecar refuses an identity mismatch or a full listener before
    /// reading the hello; the refusal is still read and gives the code, even
    /// though the hello could not be written.
    #[test]
    fn a_refusal_sent_before_the_hello_is_read_keeps_its_code() {
        for (class, expected) in [(RejectClass::Identity, 85), (RejectClass::Runtime, 84)] {
            let mut client = closed_peer(Some(Frame::Rejected {
                class,
                reason: "refused first".to_owned(),
            }));
            let err = hello(&mut client, Version::V1, Mode::Push).unwrap_err();
            assert_eq!(exit_code(&err), expected, "{class:?}: {err:#}");
            assert!(format!("{err:#}").contains("refused first"), "{err:#}");
        }
    }

    /// With no frame to read, the failed write is the error: a transport
    /// failure.
    #[test]
    fn a_hello_that_cannot_be_written_and_gets_no_reply_exits_84() {
        let mut client = closed_peer(None);
        let err = hello(&mut client, Version::V2, Mode::Attach).unwrap_err();
        assert_eq!(exit_code(&err), 84, "{err:#}");
        assert!(format!("{err:#}").contains("handshake failed"), "{err:#}");
    }
}
