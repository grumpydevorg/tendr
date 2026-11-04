use tendr::attach_escape::EscapeMode;
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
        // terminal to drive. A read-only attach needs none: without one it
        // streams the session's output to stdout.
        let no_terminal = read_only && terminal::absent()?;
        let terminal = if no_terminal {
            terminal::none()
        } else {
            terminal::open()?
        };
        let mode = mode(takeover, read_only);
        // A local controller does not reconnect, so it has no use for
        // offsets. A watcher speaks the newest version it needs, to be told
        // where the run's output ends, and so whether it was cut off.
        let version = if read_only {
            attach_proto::Version::WATCH_LATEST
        } else {
            attach_proto::Version::V1
        };
        let (stream, accept, version) = connect(session.path(), version, mode)?;
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
        let end = if version.reports_end() {
            client::EndNotice::Promised
        } else {
            client::EndNotice::Unknown
        };
        // A local relay's code is always 0: its failures are errors.
        client::relay(
            client::Link::socket(stream)?,
            accept.resume_point(),
            escape,
            accept.access(),
            client::Peer::Local {
                recorded_takeover: &recorded_takeover,
            },
            terminal,
            end,
        )?;
        // Without a terminal nothing typed ends the attach: the session
        // ended it, at the end of the run or by cutting this watcher off. A
        // session that reports the end of its output has said which; one
        // started by a tendr from before that leaves `meta.json` to tell.
        if no_terminal && end == client::EndNotice::Unknown {
            run_ended(&session, meta.run_id())?;
        }
        Ok(())
    }
}

/// How long a no-terminal read-only attach whose connection the session
/// closed waits for the run to record that it ended, while the session
/// still publishes its attach socket, before counting the close as a
/// cut-off.
#[cfg(unix)]
const RUN_END_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// [`RUN_END_GRACE`]. Debug builds accept `TENDR_TEST_RUN_END_GRACE_MS` so
/// tests are not cut off by a sidecar a loaded machine slows.
#[cfg(unix)]
fn run_end_grace() -> std::time::Duration {
    tendr::test_timer::or_env("TENDR_TEST_RUN_END_GRACE_MS", RUN_END_GRACE)
}

/// How long it waits once the socket is gone: the sidecar removes it as the
/// run ends, before the durable writes that record the end, which a busy
/// disk can hold past [`RUN_END_GRACE`]. Beyond this the sidecar is taken to
/// have died before recording it.
#[cfg(unix)]
const RUN_END_RECORD_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

/// After the session closed a no-terminal read-only attach whose sidecar
/// does not report the end of its output (one started by a tendr from
/// before version 3): `Ok` if the run `run_id` ended, waiting for it to say
/// so ([`RUN_END_GRACE`], [`RUN_END_RECORD_GRACE`]); otherwise the attach
/// was cut off (84), having fallen too far behind or been shut by the
/// sidecar. It cannot tell a watcher cut off at the run's end, when the
/// sidecar stops waiting for a slow one, from one that had everything: both
/// read as the run ended.
#[cfg(unix)]
fn run_ended(session: &SessionDir, run_id: tendr::model::ids::RunId) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    loop {
        let ended = session::read_meta(session).is_ok_and(|meta| {
            meta.run_id() != run_id || !matches!(meta.status(), RunStatus::Running { .. })
        });
        if ended {
            return Ok(());
        }
        let grace = if attach_proto::read_sock_path(session.path()).is_some() {
            run_end_grace()
        } else {
            RUN_END_RECORD_GRACE
        };
        if start.elapsed() >= grace {
            return Err(PtyExitCode::Runtime.error(anyhow::anyhow!(
                "tendr: the session cut this read-only attach off while the run was still running"
            )));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
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
fn mode(takeover: bool, read_only: bool) -> Mode {
    match (read_only, takeover) {
        (true, _) => Mode::Watch,
        (false, true) => Mode::Takeover,
        (false, false) => Mode::Attach,
    }
}

/// How long `attach --host` waits for ssh to end once the attach has ended
/// on this side (a detach, or the keyboard closing) before stopping it. The
/// remote side ends as soon as it reads the end of its input; a session
/// whose connection is lost releases control either way.
const SSH_LEAVE_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

/// How long `attach --host` waits for ssh to end, for its exit code, once
/// the remote end has closed the connection. ssh still running after it
/// counts as a lost connection.
const SSH_CLOSE_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// [`SSH_CLOSE_GRACE`]. Debug builds accept `TENDR_TEST_SSH_CLOSE_GRACE_MS`
/// so tests can wait for ssh's exit code however slow the machine.
fn ssh_close_grace() -> std::time::Duration {
    tendr::test_timer::or_env("TENDR_TEST_SSH_CLOSE_GRACE_MS", SSH_CLOSE_GRACE)
}

/// `attach --host`. `frame` is the encoded remote frame
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
/// the remote refuses, or the user leaves; a read-only one too, as a
/// watcher, which never touches control.
///
/// A read-only attach without a terminal streams the session's output to
/// stdout, as a local one does, and its exit code is the remote's verdict:
/// 0 when the run ended, 84 when the session cut it off
/// ([`cmd_attach_bridge`]). Only the remote knows the version its sidecar
/// speaks, so it judges: from version 3 by the session's end of output,
/// which it passes on, and which, when it comes, ends the output here too.
///
/// Returns the exit code. The remote side's own failures (no such session,
/// a refusal, an unreachable socket) arrive as its exit code, with its
/// message passed through on stderr, so the codes are those of a local
/// attach. A watcher refused by a remote too old to serve it gets a line
/// after that message saying so ([`old_remote_watch_note`]).
///
/// # Errors
/// A takeover of this attach (81), an answer or output outside the protocol
/// (80), a remote that ended without answering (84), and the ssh failures of
/// [`tendr::ssh::AttachSsh`].
pub fn cmd_attach_host(
    host: &str,
    request: &crate::remote_op::AttachRequest,
    mut frame: Vec<u8>,
) -> anyhow::Result<i32> {
    use tendr::attach_proto::Version;

    // Before ssh starts: the request may take over, and ssh shares the
    // terminal, whose state is taken now. On Unix a read-only attach needs
    // none: without one it streams the session's output to stdout.
    #[cfg(unix)]
    let terminal = if request.read_only && terminal::absent()? {
        terminal::none()
    } else {
        terminal::open()?
    };
    #[cfg(not(unix))]
    let terminal = terminal::open()?;
    let mode = mode(request.takeover, request.read_only);
    attach_proto::write_frame(
        &mut frame,
        &Frame::Hello {
            version: Version::V1,
            mode,
        },
    )?;
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stderr = std::sync::Arc::new(client::StderrRoute::default());
    let ssh_stderr: Box<dyn std::io::Write + Send> =
        Box::new(client::SshStderr::new(std::sync::Arc::clone(&stderr)));
    // A watcher keeps the end of what the remote says, to explain a
    // refusal by a remote too old to serve it.
    let (ssh_stderr, heard) = if request.read_only {
        let heard = std::sync::Arc::default();
        let tail = Overheard {
            out: ssh_stderr,
            tail: std::sync::Arc::clone(&heard),
        };
        (
            Box::new(tail) as Box<dyn std::io::Write + Send>,
            Some(heard),
        )
    } else {
        (ssh_stderr, None)
    };
    let dial = tendr::ssh::Dial {
        login: tendr::ssh::Login::Interactive,
        stderr: ssh_stderr,
        cancel: std::sync::Arc::clone(&cancel),
    };
    // The remote side's answer is the sidecar's acceptance.
    let opened = open_host(host, &frame, dial, |reply| {
        hello_reply(reply, mode, Version::LATEST)
    })?;
    let (link, accept) = match opened {
        Opened::Answered(link, accept) => (link, accept),
        Opened::Ended(code) => {
            // Every byte the remote said has been passed on by now: ssh's
            // stderr ends before its exit code is known.
            let note = heard.and_then(|heard| {
                let heard = heard
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                old_remote_watch_note(host, code, &heard)
            });
            if let Some(note) = note {
                eprintln!("{note}");
            }
            return Ok(code);
        }
    };
    let (link, ssh) = link.split();
    let end = HostEnd {
        host: host.to_owned(),
        request: request.clone(),
        ssh: Some(ssh),
        cancel,
        stderr,
    };
    client::relay(
        link,
        accept.resume_point(),
        request.escape,
        accept.access(),
        client::Peer::Host(Box::new(end)),
        terminal,
        // The remote judges, and passes the end of output on when it comes.
        client::EndNotice::Unknown,
    )
}

/// The most of the remote's stderr a watcher keeps, from its end: the
/// refusal it looks for is the remote's last line.
const OVERHEARD_BYTES: usize = 4096;

/// An `attach --host` connection's ssh stderr, passed on to `out` as it
/// comes, whose last [`OVERHEARD_BYTES`] are kept in `tail`.
struct Overheard {
    out: Box<dyn std::io::Write + Send>,
    tail: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

impl std::io::Write for Overheard {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.out.write(buf)?;
        let mut tail = self
            .tail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tail.extend_from_slice(&buf[..written]);
        let excess = tail.len().saturating_sub(OVERHEARD_BYTES);
        tail.drain(..excess);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

/// The oldest `tendr` that serves watchers over `--host`.
const WATCH_FLOOR: &str = "0.7.0";

/// What a remote tendr from before [`WATCH_FLOOR`] says when it refuses a
/// watcher. It ignores the request's `read_only`, so it wants an attach
/// hello and refuses the watch hello (80), unless its human-control check,
/// which a newer remote skips for a watcher, refuses first (81). This is
/// the frozen text of the released 0.5.0 and 0.6.0, matched as it is; no
/// remote from 0.7.0 on says either to a watcher: it wants a `Watch` hello,
/// and never suggests `--takeover` to one.
const OLD_REMOTE_HELLO_REFUSAL: &str = "the attach request must be followed by its Attach hello";
const OLD_REMOTE_HUMAN_CONTROL: &str =
    "session is already under human control (use --takeover to take it over)";

/// The line an `attach --read-only --host` adds after the remote's own
/// message when a remote too old to serve watchers refused it before any
/// answer, with exit `code` and the stderr whose end is `heard`; `None` for
/// any other refusal.
fn old_remote_watch_note(host: &str, code: i32, heard: &[u8]) -> Option<String> {
    let heard = String::from_utf8_lossy(heard);
    if code == PtyExitCode::Protocol.code() && heard.contains(OLD_REMOTE_HELLO_REFUSAL) {
        Some(format!(
            "tendr: watching (attach --read-only) needs tendr {WATCH_FLOOR} or newer on {host}, \
             and a session started by it"
        ))
    } else if code == PtyExitCode::Control.code() && heard.contains(OLD_REMOTE_HUMAN_CONTROL) {
        Some(format!(
            "tendr: the tendr on {host} is older than {WATCH_FLOOR} and cannot serve watchers; \
             --takeover would take the terminal from whoever holds it"
        ))
    } else {
        None
    }
}

/// The ssh end of an `attach --host`: the current connection's ssh, and what
/// a reconnect needs to open the next.
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
    stderr: std::sync::Arc<client::StderrRoute>,
}

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
    fn resume(&mut self, point: Point) -> anyhow::Result<client::Attempt> {
        let dial = tendr::ssh::Dial {
            login: tendr::ssh::Login::Batch,
            stderr: Box::new(client::SshStderr::new(std::sync::Arc::clone(&self.stderr))),
            cancel: std::sync::Arc::clone(&self.cancel),
        };
        Ok(match resume_host(&self.host, &self.request, point, dial)? {
            Opened::Answered(link, resumption) => {
                let (link, ssh) = link.split();
                self.ssh = Some(ssh);
                client::Attempt::Resumed(link, resumption)
            }
            Opened::Ended(code) => client::Attempt::Ended(code),
        })
    }
}

/// Where a version 2 attach over `--host` stands in its run: the offset of
/// the next output byte it has not written, from which it resumes after a
/// lost connection. A controller's point carries the epoch it held, which
/// the control rule checks on a resume. A watcher's carries none, since it
/// holds no control, and it resumes with its own frame
/// ([`Frame::WatchResume`]), so no path can feed its point into the control
/// rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Point {
    Control(attach_proto::ResumePoint),
    Watch(attach_proto::WatchPoint),
}

impl Point {
    /// The offset of the next output byte not yet written.
    pub(super) fn offset(self) -> u64 {
        match self {
            Self::Control(point) => point.offset,
            Self::Watch(point) => point.offset,
        }
    }

    /// Move the offset past `written` more bytes.
    pub(super) fn advance(&mut self, written: u64) {
        match self {
            Self::Control(point) => point.offset += written,
            Self::Watch(point) => point.offset += written,
        }
    }

    /// The frame that asks to resume from here in `version` (2 or later),
    /// in place of the hello.
    fn resume_frame(self, version: attach_proto::Version) -> Frame {
        match self {
            Self::Control(point) => Frame::Resume { point, version },
            Self::Watch(point) => Frame::WatchResume { point, version },
        }
    }

    /// The point once the session answered a resume from here with
    /// `answer`: a controller's new epoch and offset; a watcher's offset.
    pub(super) fn resumed(self, answer: attach_proto::ResumePoint) -> Self {
        match self {
            Self::Control(_) => Self::Control(answer),
            Self::Watch(point) => Self::Watch(attach_proto::WatchPoint {
                offset: answer.offset,
                ..point
            }),
        }
    }
}

/// An `attach --host` connection whose remote side has answered: ssh, its
/// stdin for the client's frames, and its stdout, past the answer, for the
/// session's.
pub(super) struct HostLink {
    ssh: tendr::ssh::AttachSsh,
    stdin: std::process::ChildStdin,
    reader: std::io::BufReader<std::process::ChildStdout>,
}

impl HostLink {
    /// The relay's link over ssh's stdin and stdout, and ssh. The sender
    /// closes ssh's stdin when the relay closes its outbox, and
    /// [`HostEnd::finish`] stops ssh if it does not end by itself.
    fn split(self) -> (client::Link, tendr::ssh::AttachSsh) {
        let link = client::Link {
            reader: Box::new(self.reader),
            writer: Box::new(std::io::BufWriter::new(self.stdin)),
            close: Box::new(|| {}),
        };
        (link, self.ssh)
    }
}

/// How opening an `attach --host` connection went.
pub(super) enum Opened<A> {
    /// The session answered `A`; the attach goes on over the link.
    Answered(Box<HostLink>, A),
    /// The remote side ended without answering, with this exit code, its
    /// verdict: a refusal or a failure of its own (its message has been
    /// passed on to stderr).
    Ended(i32),
}

/// Start ssh with `request` (the remote frame and what replaces the hello),
/// read the session's answer, and let `answer` classify it. A remote side
/// that ends without answering is [`Opened::Ended`] with its exit code (84
/// if it claims success); an answer `answer` refuses stops ssh. Setting the
/// dial's `cancel` stops ssh at any point before the answer has arrived.
///
/// # Errors
/// `answer`'s error, and the ssh failures of [`tendr::ssh::AttachSsh`].
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
    match read_answer(stdout, &cancel) {
        Some((reader, reply)) if !matches!(reply, Err(FrameError::Io(_))) => match answer(reply) {
            Ok(answered) => Ok(Opened::Answered(
                Box::new(HostLink { ssh, stdin, reader }),
                answered,
            )),
            Err(e) => {
                drop(stdin);
                let _ = ssh.finish(Some(SSH_LEAVE_GRACE));
                Err(e)
            }
        },
        _ => {
            drop(stdin);
            match ssh.finish(None) {
                Some(code) => unanswered(code?).map(Opened::Ended),
                // Without a grace, only a cancel stops it.
                None => Err(SshError::Cancelled.into()),
            }
        }
    }
}

/// How often waiting for the session's answer looks at the cancel flag.
const ANSWER_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// Read the session's answer from `stdout` on a thread of its own, and wait
/// for it where `cancel` can stop the wait: the reader and the answer, or
/// `None` once `cancel` is set (or the thread died). The caller then ends
/// ssh, which ends the read, and the thread with it.
fn read_answer(
    stdout: std::process::ChildStdout,
    cancel: &std::sync::atomic::AtomicBool,
) -> Option<(
    std::io::BufReader<std::process::ChildStdout>,
    Result<Frame, FrameError>,
)> {
    use std::sync::mpsc::RecvTimeoutError;

    let (send, answered) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        let reply = attach_proto::read_frame(&mut reader);
        let _ = send.send((reader, reply));
    });
    loop {
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            return None;
        }
        match answered.recv_timeout(ANSWER_POLL) {
            Ok(answer) => return Some(answer),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

/// Reconnect the `attach --host` of `request` after its connection was lost,
/// resuming from `point`: the run, the offset just past the last output
/// byte written to the terminal (the version 2 point the relay keeps) and,
/// for a controller, the epoch it held. `dial` says how ssh connects.
///
/// The request carries `point`, and a resume frame takes the hello's place
/// on ssh's stdin ([`resume_request`]), so a remote tendr that cannot resume
/// refuses it rather than attach afresh. On [`Resumption::Resumed`] the relay
/// goes on from `point`'s offset: the session's stamped output follows from
/// there, with no mode replay. On [`Resumption::Gone`] the output in between
/// is lost: the caller writes the reset of the modes it has tracked
/// (`TermModes::reset`) before relaying, since the mode replay that follows
/// only turns modes on, then goes on from the head, and a controller sends
/// the size and the repaint nudge, as a fresh attach does.
///
/// # Errors
/// A refusal (81 when control has changed since, the run has ended, or an
/// agent's push holds the terminal; 80 when the session cannot resume), an
/// answer outside the protocol (80), and the ssh failures of
/// [`tendr::ssh::AttachSsh`].
fn resume_host(
    host: &str,
    request: &crate::remote_op::AttachRequest,
    point: Point,
    dial: tendr::ssh::Dial,
) -> anyhow::Result<Opened<Resumption>> {
    let frame = resume_request(request, point)?;
    open_host(host, &frame, dial, |reply| resume_reply(reply, point))
}

/// What a resume from `point` writes to ssh's stdin: the remote frame of
/// `request` carrying `point`, never a takeover, then the resume frame where
/// the hello would go. A watcher's request is still read-only, and the
/// epoch it carries is zero: the remote reads only its run and offset, into
/// the watch resume it requires ([`cmd_attach_bridge`]). The field stays
/// because a remote from before watch resumes requires it, and then refuses
/// the frame (80) rather than the request (2). The frame is always version
/// 2's, the only one such a remote takes; the request's `protocol` says
/// which the remote may speak to the session.
fn resume_request(
    request: &crate::remote_op::AttachRequest,
    point: Point,
) -> anyhow::Result<Vec<u8>> {
    use crate::remote_op::{AttachRequest, RemoteOperation, ResumeRequest};

    let resume = match point {
        Point::Control(point) => point.into(),
        Point::Watch(point) => ResumeRequest {
            run_id: point.run_id,
            epoch: tendr::model::pty_control::ControllerEpoch::ZERO.get(),
            offset: point.offset,
        },
    };
    let mut frame = crate::remote_frame::encode(&RemoteOperation::Attach(AttachRequest {
        takeover: false,
        resume: Some(resume),
        ..request.clone()
    }))?;
    attach_proto::write_frame(&mut frame, &point.resume_frame(attach_proto::Version::V2))?;
    Ok(frame)
}

/// The exit code of an `attach --host` whose remote side ended without
/// accepting: its own code, or 84 if it claims success.
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

/// How long a client waits for the answer to its hello from a sidecar that
/// may predate the hello (see [`reply_deadline`]). Every later sidecar
/// answers, so its answer is waited for however long it takes.
#[cfg(unix)]
const PRE_HELLO_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The deadline for the answer to a hello in session `session_dir`, if any.
///
/// A sidecar from before the hello (tendr 0.3 and older) never answers one:
/// it serves the connection as attached and is silent until the child
/// writes. Every sidecar since answers every hello, or closes the
/// connection; and every one of them, from 0.4.0 on, records its PTY
/// recording in `meta.json` (`pty.recording`) in each write while the run is
/// running, which no earlier one did. So only a session whose `meta.json`
/// lacks it, or cannot be read, gets [`PRE_HELLO_REPLY_TIMEOUT`]: an answer
/// from any other is waited for, so a loaded machine slows the attach but
/// cannot fail it. A debug build's `TENDR_TEST_PRE_HELLO_REPLY_MS` shortens
/// the timeout, so a test can show it is not applied.
#[cfg(unix)]
fn reply_deadline(session_dir: &std::path::Path) -> Option<std::time::Duration> {
    let answers = std::fs::read(session_dir.join("meta.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<tendr::model::meta::Meta>(&bytes).ok())
        .is_some_and(|meta| meta.pty().is_some_and(|pty| pty.recording.is_some()));
    (!answers).then(|| {
        tendr::test_timer::or_env("TENDR_TEST_PRE_HELLO_REPLY_MS", PRE_HELLO_REPLY_TIMEOUT)
    })
}

/// The sidecar's acceptance: version 1's controller epoch, or version 2's
/// point in the run, from which a viewer's stamped output starts; or, for a
/// read-only viewer, its point in the run with no epoch, since it holds no
/// control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Accept {
    V1(tendr::model::pty_control::ControllerEpoch),
    V2(attach_proto::ResumePoint),
    Watch(attach_proto::WatchPoint),
}

impl Accept {
    /// The controller epoch granted; `None` for a watcher, which holds
    /// none, so no takeover check applies to it.
    #[cfg(unix)]
    fn epoch(self) -> Option<tendr::model::pty_control::ControllerEpoch> {
        match self {
            Self::V1(epoch) => Some(epoch),
            Self::V2(point) => Some(point.epoch),
            Self::Watch(_) => None,
        }
    }

    /// Where an attach's stamped output starts: a version 2 controller's
    /// point, or a watcher's (a watcher speaks version 2 or later); `None`
    /// for version 1, whose output carries no offsets. Over `--host` it is
    /// where a lost connection resumes from; locally, where the session's
    /// end of output must find a watcher's output.
    fn resume_point(self) -> Option<Point> {
        match self {
            Self::V1(_) => None,
            Self::V2(point) => Some(Point::Control(point)),
            Self::Watch(point) => Some(Point::Watch(point)),
        }
    }

    /// What the relay may send for this attach.
    fn access(self) -> client::Access {
        match self {
            Self::V1(_) | Self::V2(_) => client::Access::Control,
            Self::Watch(_) => client::Access::ReadOnly,
        }
    }

    /// The acceptance as the sidecar sent it.
    #[cfg(unix)]
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
/// PTY `push` and the attach bridge all start here. Returns the connection,
/// the sidecar's acceptance, and the version it accepted.
///
/// A sidecar started by an older tendr may not speak `version`: it refuses
/// the hello with the protocol class. Then this connects once more and says
/// it in the version before, until one is accepted or version 1, which
/// every sidecar with a hello speaks, is refused too. A watch hello stops
/// at version 2: every sidecar that serves read-only viewers speaks it, so
/// the protocol class there means the sidecar predates them, and the
/// failure says so (80).
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
) -> anyhow::Result<(
    std::os::unix::net::UnixStream,
    Accept,
    attach_proto::Version,
)> {
    use attach_proto::{RejectClass, Version};
    let mut stream = open_socket(session_dir)?;
    let deadline = reply_deadline(session_dir);
    let hello = |version| Frame::Hello { version, mode };
    let mut reply = exchange(&mut stream, &hello(version), verb(mode), deadline)?;
    let mut version = version;
    while let Ok(Frame::Rejected {
        class: RejectClass::Protocol,
        reason,
    }) = &reply
    {
        match version.older() {
            Some(older) if mode != Mode::Watch || older >= Version::V2 => version = older,
            _ if mode == Mode::Watch => {
                return Err(PtyExitCode::Protocol.error(anyhow::anyhow!(
                    "cannot attach read-only: the session was started by a tendr that cannot serve read-only viewers ({reason})"
                )));
            }
            _ => break,
        }
        stream = open_socket(session_dir)?;
        reply = exchange(&mut stream, &hello(version), verb(mode), deadline)?;
    }
    let accept = hello_reply(reply, mode, version)?;
    // From here an attach waits for output, and a push as long as the terminal
    // applies backpressure.
    stream.set_read_timeout(None).map_err(|e| {
        PtyExitCode::Runtime.error(anyhow::anyhow!("{} handshake failed: {e}", verb(mode)))
    })?;
    Ok((stream, accept, version))
}

/// What the sidecar answered a resume: a controller's control regained at a
/// new epoch (a watcher's answer is at epoch zero, which names no control),
/// and where the output goes on from.
///
/// From version 4 a controller's answer also says how many bytes of input
/// the connection it resumes wrote to the session's terminal
/// (`input_written`); `None` from a session or remote before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Resumption {
    /// The output follows from `point.offset`, the offset asked for, with
    /// no mode replay: the terminal already has the modes.
    Resumed {
        point: attach_proto::ResumePoint,
        input_written: Option<u64>,
    },
    /// The output from the offset asked for is gone: the session holds only
    /// `oldest` up to `point.offset`, its head. The mode replay follows,
    /// unstamped, then the output from the head.
    Gone {
        point: attach_proto::ResumePoint,
        oldest: u64,
        input_written: Option<u64>,
    },
}

impl Resumption {
    /// The epoch, and the offset the stamped output starts at.
    pub(super) fn point(self) -> attach_proto::ResumePoint {
        match self {
            Self::Resumed { point, .. } | Self::Gone { point, .. } => point,
        }
    }

    /// How many bytes of the lost connection's input the session wrote, if
    /// it said (version 4).
    pub(super) fn input_written(self) -> Option<u64> {
        match self {
            Self::Resumed { input_written, .. } | Self::Gone { input_written, .. } => input_written,
        }
    }

    /// The answer as the sidecar sent it.
    #[cfg(unix)]
    fn frame(self) -> Frame {
        match self {
            Self::Resumed {
                point,
                input_written,
            } => Frame::Resumed {
                point,
                input_written,
            },
            Self::Gone {
                point,
                oldest,
                input_written,
            } => Frame::Gone {
                point,
                oldest,
                input_written,
            },
        }
    }
}

/// [`connect`] for a resume from `point`, speaking `version` (2 or later):
/// the resume frame, a controller's or a watcher's, takes the hello's place.
/// A sidecar started by a tendr from before `version` refuses it with the
/// protocol class, and this says it again in the version before, down to
/// version 2. One that cannot resume it at all refuses that too; that is
/// "cannot resume" (80), never a fresh attach. Its answer is waited for
/// however long it takes: a session that can be resumed answered a hello
/// before. Returns the connection, the answer, and the version it was given
/// in.
#[cfg(unix)]
fn connect_resume(
    session_dir: &std::path::Path,
    point: Point,
    version: attach_proto::Version,
) -> anyhow::Result<(
    std::os::unix::net::UnixStream,
    Resumption,
    attach_proto::Version,
)> {
    let mut stream = open_socket(session_dir)?;
    let mut reply = exchange(&mut stream, &point.resume_frame(version), "resume", None)?;
    let mut version = version;
    while let Ok(Frame::Rejected {
        class: attach_proto::RejectClass::Protocol,
        reason,
    }) = &reply
    {
        match version.older() {
            Some(older) if older >= attach_proto::Version::V2 => version = older,
            _ => {
                let what = match point {
                    Point::Control(_) => "an attach",
                    Point::Watch(_) => "a read-only attach",
                };
                return Err(PtyExitCode::Protocol.error(anyhow::anyhow!(
                    "cannot resume: the session was started by a tendr that cannot resume {what} ({reason}); attach again"
                )));
            }
        }
        stream = open_socket(session_dir)?;
        reply = exchange(&mut stream, &point.resume_frame(version), "resume", None)?;
    }
    let resumed = resume_reply(reply, point)?;
    stream
        .set_read_timeout(None)
        .map_err(|e| PtyExitCode::Runtime.error(anyhow::anyhow!("resume handshake failed: {e}")))?;
    Ok((stream, resumed, version))
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
/// sidecar's reply, within `deadline` if there is one ([`reply_deadline`]),
/// for [`hello_reply`] or [`resume_reply`] to classify.
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
    deadline: Option<std::time::Duration>,
) -> anyhow::Result<Result<Frame, FrameError>> {
    let transport = |e: std::io::Error| {
        PtyExitCode::Runtime.error(anyhow::anyhow!("{what} handshake failed: {e}"))
    };
    // macOS refuses SO_RCVTIMEO (EINVAL) on a socket whose peer has already
    // shut down: exactly the refusal-before-hello case. Then read without
    // blocking, so a buffered refusal is still seen and a live peer that never
    // got a hello cannot hold the read forever.
    let timeout = deadline.map_or(Ok(()), |deadline| stream.set_read_timeout(Some(deadline)));
    let unbounded = timeout.is_err();
    if unbounded {
        stream.set_nonblocking(true).map_err(transport)?;
    }
    let sent = timeout.and_then(|()| attach_proto::write_frame(stream, hello));
    // After a failed write the peer has closed its end, so this read returns
    // at once; otherwise it waits for the answer, within the deadline if any.
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
/// An answer must be for the same run, and a controller's at a later epoch;
/// a watcher's names no control, so only its run and offsets count. A
/// resumed one must be at the offset asked for, a gone one name a span that
/// starts after it (an offset past the head means this client's count is
/// broken). A watcher sent no input, so its answer must report none.
/// Otherwise as [`hello_reply`].
fn resume_reply(reply: Result<Frame, FrameError>, asked: Point) -> anyhow::Result<Resumption> {
    let ours = |point: attach_proto::ResumePoint, input_written: Option<u64>| match asked {
        Point::Control(asked) => point.run_id == asked.run_id && point.epoch > asked.epoch,
        Point::Watch(asked) => point.run_id == asked.run_id && input_written.is_none(),
    };
    match reply {
        Ok(Frame::Resumed {
            point,
            input_written,
        }) if ours(point, input_written) && point.offset == asked.offset() => {
            Ok(Resumption::Resumed {
                point,
                input_written,
            })
        }
        Ok(Frame::Gone {
            point,
            oldest,
            input_written,
        }) if ours(point, input_written) && asked.offset() < oldest => Ok(Resumption::Gone {
            point,
            oldest,
            input_written,
        }),
        Ok(answer @ (Frame::Resumed { .. } | Frame::Gone { .. })) => Err(PtyExitCode::Protocol
            .error(anyhow::anyhow!(
                "the session's answer {answer:?} does not fit the resume from {asked:?}"
            ))),
        reply => reply_failure(reply, "resume"),
    }
}

/// A reply that accepts nothing: a refusal exits with its class's code; any
/// other reply, or none before the deadline (a sidecar from before the hello
/// waits silently), is a protocol failure (80); a connection that ends first
/// is a transport failure (84).
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

mod client;
mod terminal;

#[cfg(unix)]
impl client::Link {
    /// A connection to the session's attach socket.
    fn socket(stream: std::os::unix::net::UnixStream) -> anyhow::Result<Self> {
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

#[cfg(all(test, unix))]
mod tests {
    use super::{
        Accept, PRE_HELLO_REPLY_TIMEOUT, Point, Resumption, exchange, hello_reply, reply_deadline,
        resume_reply, resume_request,
    };
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
            protocol: 3,
            resume: None,
        };
        let point = ResumePoint {
            run_id: RunId::new(),
            epoch: ControllerEpoch::new(4),
            offset: 99,
        };
        let sent = resume_request(&request, Point::Control(point)).unwrap();
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
        // Version 2's frame, whatever the request agrees: the remote speaks
        // its version to the session.
        let mut rest = &sent[4 + len..];
        assert_eq!(
            tendr::attach_proto::read_frame(&mut rest).unwrap(),
            Frame::Resume {
                point,
                version: Version::V2
            }
        );
        assert!(rest.is_empty());
    }

    /// A watcher's resume stays read-only, carries its run and offset in the
    /// request with epoch zero (the field a remote from before watch resumes
    /// requires), and sends the watch resume, with no epoch, in place of the
    /// hello.
    #[test]
    fn a_watchers_resume_sends_a_watch_resume_with_no_epoch() {
        let request = crate::remote_op::AttachRequest {
            session: "s".to_owned(),
            namespace: None,
            takeover: false,
            read_only: true,
            escape: tendr::attach_escape::EscapeMode::CtrlBackslash,
            protocol: 3,
            resume: None,
        };
        let point = WatchPoint {
            run_id: RunId::new(),
            offset: 99,
        };
        let sent = resume_request(&request, Point::Watch(point)).unwrap();
        let len = u32::from_be_bytes(sent[..4].try_into().unwrap()) as usize;
        let json: serde_json::Value = serde_json::from_slice(&sent[4..4 + len]).unwrap();
        assert_eq!(json["params"]["read_only"], true);
        assert_eq!(json["params"]["takeover"], false);
        assert_eq!(
            json["params"]["resume"],
            serde_json::json!({
                "run_id": point.run_id.as_uuid().to_string(),
                "epoch": 0,
                "offset": 99
            })
        );
        let mut rest = &sent[4 + len..];
        assert_eq!(
            tendr::attach_proto::read_frame(&mut rest).unwrap(),
            Frame::WatchResume {
                point,
                version: Version::V2
            }
        );
        assert!(rest.is_empty());
    }

    /// The hello and its reply, as a client speaking `version` sees them,
    /// under `deadline` ([`reply_deadline`]).
    fn hello(
        stream: &mut UnixStream,
        version: Version,
        mode: Mode,
        deadline: Option<std::time::Duration>,
    ) -> anyhow::Result<Accept> {
        hello_reply(
            exchange(stream, &Frame::Hello { version, mode }, "attach", deadline)?,
            mode,
            version,
        )
    }

    /// Only a session whose `meta.json` cannot show a sidecar with a hello
    /// (no `pty.recording`, or no readable meta) gets a reply deadline.
    #[test]
    fn only_a_session_that_may_predate_the_hello_gets_a_reply_deadline() {
        use std::num::NonZeroU32;
        use tendr::model::ids::{EpochTimestamp, Generation, ProcessIdentity, RunId, SessionName};
        use tendr::model::meta::Meta;
        use tendr::model::pty::{PtyMeta, PtyRecording, RecordingState};
        use tendr::model::spec::LaunchSpec;

        let dir = tempfile::tempdir().unwrap();
        assert!(reply_deadline(dir.path()).is_some(), "no meta.json");

        let run_id = RunId::new();
        let mut meta = Meta::new_starting(
            SessionName::new("s").unwrap(),
            run_id,
            Generation::first(),
            LaunchSpec::new(vec!["bash".into()]).unwrap(),
            ProcessIdentity {
                pid: NonZeroU32::new(100).unwrap(),
                start_time_ns: 1000,
            },
            EpochTimestamp::now(),
        );
        meta.set_pty(PtyMeta::new());
        let write = |meta: &Meta| {
            std::fs::write(
                dir.path().join("meta.json"),
                serde_json::to_vec(meta).unwrap(),
            )
            .unwrap();
        };
        write(&meta);
        assert!(reply_deadline(dir.path()).is_some(), "a 0.3 sidecar's meta");

        meta.set_pty_recording(PtyRecording {
            dir: format!("recording/{run_id}"),
            input_recorded: false,
            state: RecordingState::Recording,
        });
        write(&meta);
        assert_eq!(reply_deadline(dir.path()), None, "a sidecar with a hello");
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
        let code = |reply| {
            resume_reply(Ok(reply), Point::Control(asked))
                .err()
                .map(|e| exit_code(&e))
        };
        let resumed = ResumePoint {
            epoch: ControllerEpoch::new(5),
            ..asked
        };
        for input_written in [None, Some(7)] {
            assert_eq!(
                resume_reply(
                    Ok(Frame::Resumed {
                        point: resumed,
                        input_written
                    }),
                    Point::Control(asked)
                )
                .unwrap(),
                Resumption::Resumed {
                    point: resumed,
                    input_written
                }
            );
        }
        let head = ResumePoint {
            offset: 500,
            ..resumed
        };
        let gone = Frame::Gone {
            point: head,
            oldest: 101,
            input_written: Some(3),
        };
        assert_eq!(
            resume_reply(Ok(gone), Point::Control(asked)).unwrap(),
            Resumption::Gone {
                point: head,
                oldest: 101,
                input_written: Some(3)
            }
        );

        let resumed_at = |point| Frame::Resumed {
            point,
            input_written: None,
        };
        for wrong in [
            resumed_at(ResumePoint {
                run_id: RunId::new(),
                ..resumed
            }),
            resumed_at(asked),
            resumed_at(ResumePoint {
                offset: 99,
                ..resumed
            }),
            Frame::Gone {
                point: head,
                oldest: 100,
                input_written: None,
            },
            Frame::Gone {
                point: ResumePoint {
                    offset: 50,
                    ..resumed
                },
                oldest: 10,
                input_written: None,
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

    /// A watcher's resume answer names no control: its run and offsets are
    /// checked as a controller's are, its epoch is not. Anything else, a
    /// grant of control included, is a protocol failure. The point it then
    /// goes on from is still a watcher's.
    #[test]
    fn a_watchers_resume_answer_counts_only_its_run_and_offsets() {
        let asked = WatchPoint {
            run_id: RunId::new(),
            offset: 100,
        };
        let reply = |frame| resume_reply(Ok(frame), Point::Watch(asked));
        let resumed = ResumePoint {
            run_id: asked.run_id,
            epoch: ControllerEpoch::ZERO,
            offset: 100,
        };
        let resumed_at = |point| Frame::Resumed {
            point,
            input_written: None,
        };
        assert_eq!(
            reply(resumed_at(resumed)).unwrap(),
            Resumption::Resumed {
                point: resumed,
                input_written: None
            }
        );
        let head = ResumePoint {
            offset: 500,
            ..resumed
        };
        let gone = reply(Frame::Gone {
            point: head,
            oldest: 101,
            input_written: None,
        })
        .unwrap();
        assert_eq!(
            Point::Watch(asked).resumed(gone.point()),
            Point::Watch(WatchPoint {
                offset: 500,
                ..asked
            })
        );

        for wrong in [
            resumed_at(ResumePoint {
                run_id: RunId::new(),
                ..resumed
            }),
            resumed_at(ResumePoint {
                offset: 99,
                ..resumed
            }),
            // It sent no input, so none can have been written.
            Frame::Resumed {
                point: resumed,
                input_written: Some(0),
            },
            Frame::Gone {
                point: head,
                oldest: 100,
                input_written: None,
            },
            Frame::Watching(asked),
            Frame::Accepted(ControllerEpoch::new(4)),
            Frame::AcceptedV2(head),
        ] {
            let code = reply(wrong.clone()).err().map(|e| exit_code(&e));
            assert_eq!(code, Some(80), "{wrong:?}");
        }
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
    /// though the hello could not be written: with a reply deadline (which
    /// macOS refuses on such a socket) or without.
    #[test]
    fn a_refusal_sent_before_the_hello_is_read_keeps_its_code() {
        for deadline in [None, Some(PRE_HELLO_REPLY_TIMEOUT)] {
            for (class, expected) in [(RejectClass::Identity, 85), (RejectClass::Runtime, 84)] {
                let mut client = closed_peer(Some(Frame::Rejected {
                    class,
                    reason: "refused first".to_owned(),
                }));
                let err = hello(&mut client, Version::V1, Mode::Push, deadline).unwrap_err();
                assert_eq!(exit_code(&err), expected, "{class:?} {deadline:?}: {err:#}");
                assert!(format!("{err:#}").contains("refused first"), "{err:#}");
            }
        }
    }

    /// With no frame to read, the failed write is the error: a transport
    /// failure.
    #[test]
    fn a_hello_that_cannot_be_written_and_gets_no_reply_exits_84() {
        for deadline in [None, Some(PRE_HELLO_REPLY_TIMEOUT)] {
            let mut client = closed_peer(None);
            let err = hello(&mut client, Version::V2, Mode::Attach, deadline).unwrap_err();
            assert_eq!(exit_code(&err), 84, "{deadline:?}: {err:#}");
            assert!(format!("{err:#}").contains("handshake failed"), "{err:#}");
        }
    }
}
