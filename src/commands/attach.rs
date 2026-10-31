use tendr::attach_escape::EscapeMode;
#[cfg(unix)]
use tendr::attach_proto::{self, Frame, FrameError, Mode};
use tendr::model::ids::{Namespace, SessionName};
use tendr::model::pty::PtyControl;
use tendr::model::state::RunStatus;
use tendr::pty_exit::PtyExitCode;
use tendr::session::{self, SessionRoot};

pub fn cmd_attach(
    name: &str,
    namespace: &Namespace,
    takeover: bool,
    escape: EscapeMode,
) -> anyhow::Result<()> {
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
    if !takeover && pty.control == PtyControl::HumanControl {
        return Err(PtyExitCode::Control.error(anyhow::anyhow!(
            "session is already under human control (use --takeover to take it over)"
        )));
    }

    #[cfg(not(unix))]
    {
        let _ = (takeover, escape);
        anyhow::bail!("attach is only supported on Unix");
    }

    #[cfg(unix)]
    {
        // Fail before any side effect — a takeover included — if there is no
        // terminal to drive.
        if !rustix::termios::isatty(std::io::stdin()) {
            anyhow::bail!("attach requires an interactive terminal on stdin");
        }

        let mode = if takeover {
            Mode::Takeover
        } else {
            Mode::Attach
        };
        let stream = connect(session.path(), mode)?;
        unix_relay::relay(stream, escape)
    }
}

/// How long a client waits for the sidecar's answer to its hello.
#[cfg(unix)]
const HELLO_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Connect to the session's attach socket, check that its listener is this
/// user's, and complete the v1 hello in `mode`; `attach` and PTY `push` both
/// start here.
///
/// Every failure carries its PTY exit code ([`tendr::pty_exit`]): a socket
/// that is missing or cannot be reached is a runtime failure (84), a listener
/// of another user an identity failure (85), and the hello's reply is
/// classified by [`hello_reply`].
#[cfg(unix)]
pub(super) fn connect(
    session_dir: &std::path::Path,
    mode: Mode,
) -> anyhow::Result<std::os::unix::net::UnixStream> {
    let sock_path = attach_proto::read_sock_path(session_dir)
        .ok_or_else(|| PtyExitCode::Runtime.error(anyhow::anyhow!("attach socket not found")))?;
    let mut stream = std::os::unix::net::UnixStream::connect(&sock_path).map_err(|e| {
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

    hello(&mut stream, mode)?;
    Ok(stream)
}

/// Send the v1 hello in `mode` and require the sidecar's acceptance.
///
/// The sidecar refuses a peer of another user, or a connection beyond its
/// limit, before reading the hello, and closes the connection. If that lands
/// first, the hello cannot be written, but the refusal is still waiting to be
/// read: it, not the failed write, decides the outcome. The write's error is
/// reported only when no frame can be read.
#[cfg(unix)]
fn hello(stream: &mut std::os::unix::net::UnixStream, mode: Mode) -> anyhow::Result<()> {
    let what = verb(mode);
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
    let sent = timeout.and_then(|()| attach_proto::write_frame(stream, &Frame::Hello(mode)));
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
    hello_reply(reply, what)?;
    // From here an attach waits for output, and a push as long as the terminal
    // applies backpressure.
    stream.set_read_timeout(None).map_err(transport)?;
    Ok(())
}

#[cfg(unix)]
fn verb(mode: Mode) -> &'static str {
    match mode {
        Mode::Attach | Mode::Takeover => "attach",
        Mode::Push => "push",
    }
}

/// What the sidecar's reply to a hello means for the client.
///
/// A refusal exits with its class's code. A reply that is not a v1 reply, or
/// none before the deadline (a sidecar from before the hello waits silently),
/// is a protocol failure (80); a connection that ends first is a transport
/// failure (84).
#[cfg(unix)]
fn hello_reply(reply: Result<Frame, FrameError>, what: &str) -> anyhow::Result<()> {
    let protocol = |message: String| Err(PtyExitCode::Protocol.error(anyhow::anyhow!(message)));
    match reply {
        Ok(Frame::Accepted(_)) => Ok(()),
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

/// The interactive relay between the user's terminal and the session.
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
/// Leaving never joins a blocked thread: the socket is shut down, the terminal
/// is restored (without waiting for pending output to drain), and the process
/// exits.
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
    use std::sync::{Arc, Condvar, Mutex, MutexGuard};
    use std::time::{Duration, Instant};

    use tendr::attach_escape::{Escape, EscapeMode, EscapeParser};
    use tendr::attach_proto::{self, Frame, FrameError};
    use tendr::pty_exit::PtyExitCode;
    use tendr::term_modes::TermModes;

    /// Keystrokes that may wait for a session that is not accepting input.
    /// Beyond this, further keystrokes are dropped (and reported) rather than
    /// buffered without bound; the escape is still recognized.
    const OUTBOX_DATA_LIMIT: usize = 1 << 20;
    /// How long the main thread waits on the keyboard before it checks again
    /// whether the session has closed.
    const POLL: Duration = Duration::from_millis(100);
    /// How often the terminal size is checked.
    const SIZE_POLL: Duration = Duration::from_millis(100);
    /// How long a detach waits for the detach message to be sent.
    const DETACH_GRACE: Duration = Duration::from_millis(200);
    /// How long leaving on a signal waits for its report to be written to a
    /// terminal that may be gone or not reading.
    const REPORT_GRACE: Duration = Duration::from_millis(200);
    /// How long leaving waits to turn off the session's terminal modes in a
    /// terminal that is slow, gone or not reading. After a signal the shorter
    /// [`REPORT_GRACE`] applies: SIGHUP means the terminal is gone.
    const MODES_GRACE: Duration = Duration::from_secs(1);

    pub fn relay(stream: UnixStream, escape: EscapeMode) -> anyhow::Result<()> {
        let socket = |e| PtyExitCode::Runtime.error(anyhow::anyhow!("attach socket: {e}"));
        let shutdown = stream.try_clone().map_err(socket)?;
        let reader_stream = stream.try_clone().map_err(socket)?;
        let closed = Arc::new(AtomicBool::new(false));
        let retired = Arc::new(AtomicBool::new(false));
        let outbox = Arc::new(Outbox::default());
        let screen = Arc::new(Screen::default());

        // Installed before raw mode, so no cancelling signal can arrive between
        // the two and leave the terminal raw; restored when dropped, after the
        // terminal.
        let handlers = signals::Handlers::install()
            .map_err(|e| anyhow::anyhow!("failed to install attach signal handlers: {e}"))?;
        // Restores the terminal on every way out of this function, panics
        // included.
        let terminal = RawTerminal::enter()?;

        spawn_sender(stream, Arc::clone(&outbox), Arc::clone(&closed));
        spawn_reader(
            reader_stream,
            Arc::clone(&closed),
            Arc::clone(&retired),
            Arc::clone(&screen),
        );

        let mut size = terminal_size();
        if let Some(size) = size {
            outbox.push_control(Frame::Resize(size));
        }

        let size_poll = size_poll_interval();
        let mut size_due = Instant::now() + size_poll;
        let mut parser = EscapeParser::new(escape);
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 4096];
        let mut dropped = 0usize;
        let ending = loop {
            if closed.load(Ordering::SeqCst) {
                break Ending::SessionClosed;
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
                    if let Some(size) = now {
                        outbox.push_control(Frame::Resize(size));
                    }
                }
            }
            if !ready.keyboard {
                // An escape sequence left unfinished by the last read (a lone
                // Esc key, say) is forwarded once it has waited long enough.
                let mut forward = Vec::new();
                parser.flush_expired(Instant::now(), &mut forward);
                queue_keystrokes(&outbox, forward, &mut dropped);
                continue;
            }
            let n = match stdin.read(&mut buf) {
                Ok(0) => break Ending::KeyboardClosed,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break Ending::KeyboardClosed,
            };
            let mut forward = Vec::new();
            let decision = parser.feed(&buf[..n], &mut forward);
            queue_keystrokes(&outbox, forward, &mut dropped);
            if decision == Escape::Detach {
                break Ending::Detached;
            }
        };

        if matches!(ending, Ending::Detached | Ending::Signalled(_)) {
            outbox.push_control(Frame::Detach);
            outbox.wait_controls_sent(DETACH_GRACE);
        }
        // Unblocks the sender and reader on the socket; the session treats a
        // closed connection as a detach.
        let _ = shutdown.shutdown(std::net::Shutdown::Both);
        outbox.close();
        turn_off_modes(
            &screen,
            if matches!(ending, Ending::Signalled(_)) {
                REPORT_GRACE
            } else {
                MODES_GRACE
            },
        );
        drop(terminal);
        drop(handlers);

        // Only report once the terminal is restored, and never after a user
        // detach, whose terminal may be the thing that is blocked.
        let taken_over = ending != Ending::Detached && retired.load(Ordering::SeqCst);
        let mut notes = Vec::new();
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
        if taken_over {
            // This client lost control: a controller conflict, which a script
            // must be able to tell from a detach.
            return Err(PtyExitCode::Control.error(anyhow::anyhow!(
                "tendr: another client took over this session"
            )));
        }
        Ok(())
    }

    /// Queue keystrokes for the session, counting them in `dropped` if the
    /// outbox is full.
    fn queue_keystrokes(outbox: &Outbox, forward: Vec<u8>, dropped: &mut usize) {
        if !forward.is_empty() {
            let len = forward.len();
            if !outbox.push_data(forward) {
                *dropped += len;
            }
        }
    }

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
    #[derive(Default)]
    struct Screen {
        /// The modes turned on by everything written so far. Held while
        /// writing, so a reset cannot interleave with output.
        modes: Mutex<TermModes>,
        /// Set by the main thread when leaving: the reader writes nothing
        /// more, so at most the chunk it is writing precedes the reset.
        done: AtomicBool,
    }

    /// Turn off the terminal modes the session turned on (the alternate
    /// screen, mouse reports, keyboard protocols), so the user's shell gets its
    /// terminal back. Waits for a write in progress, but not for longer than
    /// `grace`: a terminal that is not reading cannot hold the exit.
    fn turn_off_modes(screen: &Arc<Screen>, grace: Duration) {
        screen.done.store(true, Ordering::SeqCst);
        let (done, written) = std::sync::mpsc::channel();
        let screen = Arc::clone(screen);
        std::thread::spawn(move || {
            let modes = screen.modes.lock().unwrap_or_else(|e| e.into_inner());
            let reset = modes.reset();
            if !reset.is_empty() {
                let mut stdout = std::io::stdout().lock();
                let _ = stdout.write_all(&reset).and_then(|()| stdout.flush());
            }
            drop(modes);
            let _ = done.send(());
        });
        let _ = written.recv_timeout(grace);
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

    /// Messages waiting for the sender thread; controls go first.
    #[derive(Default)]
    struct Outbox {
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

        fn push_control(&self, frame: Frame) {
            self.lock().controls.push_back(frame);
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
        fn wait_controls_sent(&self, grace: Duration) {
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

        fn close(&self) {
            self.lock().closed = true;
            self.changed.notify_all();
        }
    }

    fn spawn_sender(mut stream: UnixStream, outbox: Arc<Outbox>, closed: Arc<AtomicBool>) {
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
        mut stream: UnixStream,
        closed: Arc<AtomicBool>,
        retired: Arc<AtomicBool>,
        screen: Arc<Screen>,
    ) {
        std::thread::spawn(move || {
            loop {
                match attach_proto::read_frame(&mut stream) {
                    Ok(Frame::Data(payload)) => {
                        let mut modes = screen.modes.lock().unwrap_or_else(|e| e.into_inner());
                        if screen.done.load(Ordering::SeqCst) {
                            break;
                        }
                        // Track before writing: modes from a partial write are
                        // still turned off when leaving.
                        modes.feed(&payload);
                        let mut stdout = std::io::stdout().lock();
                        if stdout.write_all(&payload).is_err() || stdout.flush().is_err() {
                            break;
                        }
                    }
                    Ok(Frame::Retired(_)) => retired.store(true, Ordering::SeqCst),
                    Ok(_) | Err(FrameError::Malformed(_) | FrameError::Unknown(_)) => {}
                    Err(FrameError::Io(_)) => break,
                }
            }
            closed.store(true, Ordering::SeqCst);
        });
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
    use super::{hello, hello_reply};
    use std::io::{self, Write};
    use std::os::unix::net::UnixStream;
    use tendr::attach_proto::{Frame, FrameError, ProtocolError, RejectClass};
    use tendr::attach_proto::{Mode, write_frame};
    use tendr::model::pty_control::ControllerEpoch;
    use tendr::pty_exit::exit_code;

    fn code(reply: Result<Frame, FrameError>) -> Option<i32> {
        hello_reply(reply, "attach").err().map(|e| exit_code(&e))
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
            let err = hello(&mut client, Mode::Push).unwrap_err();
            assert_eq!(exit_code(&err), expected, "{class:?}: {err:#}");
            assert!(format!("{err:#}").contains("refused first"), "{err:#}");
        }
    }

    /// With no frame to read, the failed write is the error: a transport
    /// failure.
    #[test]
    fn a_hello_that_cannot_be_written_and_gets_no_reply_exits_84() {
        let mut client = closed_peer(None);
        let err = hello(&mut client, Mode::Attach).unwrap_err();
        assert_eq!(exit_code(&err), 84, "{err:#}");
        assert!(format!("{err:#}").contains("handshake failed"), "{err:#}");
    }
}
