//! The interactive relay between the user's terminal and the session, over
//! the attach socket or, for `attach --host`, over ssh to the remote bridge:
//! the platform-neutral client core. Everything it does to the terminal
//! goes through the terminal layer ([`super::terminal`]).
//!
//! Three independent paths, so no blocked one can delay a detach:
//!
//! - the **main thread** waits on the keyboard, recognizes the escape,
//!   watches for terminal size changes, and only ever queues messages;
//! - a **sender thread** writes queued messages to the session, controls
//!   (resize, detach) before keystrokes; if the session stops accepting
//!   input, only this thread waits;
//! - a **reader thread** writes session output to the terminal; if the
//!   terminal stops reading, only this thread waits.
//!
//! When an `attach --host` connection is lost, a fourth path, the
//! [`reconnect`] thread, judges ssh's end and opens a new connection, while
//! the main thread keeps the keyboard.
//!
//! Leaving never joins a blocked thread: the connection is closed, the
//! terminal is restored (without waiting for pending output to drain), and
//! the process exits.
//!
//! The terminal layer's [`Leave`] (on Unix SIGHUP, SIGTERM and SIGINT; on
//! Windows the console's control events) ends the relay like a detach, and
//! its size notices (SIGWINCH) bring the next size check forward. A leave
//! wins over a session end it raced: the same signal or control event can
//! end ssh, which shares the terminal, and the user asked to leave.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use tendr::attach_escape::{Escape, EscapeMode, EscapeParser};
use tendr::attach_proto::{self, Frame, FrameError};
use tendr::pty_exit::PtyExitCode;
use tendr::term_modes::TermModes;

use super::terminal::{Input, Leave, Output, Raw, Terminal};

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

/// The far end of the relay.
pub(super) enum Peer<'a> {
    /// The session's attach socket on this machine. `recorded_takeover`
    /// is asked when the session closes the connection without saying
    /// it was taken over. Unix only: a Windows session has no attach socket.
    #[cfg_attr(windows, allow(dead_code))]
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

/// Whether the session has promised to tell this attach where the run's
/// output ends ([`Frame::OutputEnd`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::commands::attach) enum EndNotice {
    /// It speaks version 3, so it ends a viewer's output with the notice
    /// once it has sent all of it: a watcher whose connection ends without
    /// one was cut off short (84), whatever the run did. A local attach's
    /// only: Windows has no attach socket.
    #[cfg_attr(windows, allow(dead_code))]
    Promised,
    /// It may not: a sidecar from before version 3, or, over `--host`, a
    /// remote whose own verdict decides, since only the remote knows the
    /// version its sidecar speaks. A notice that comes all the same still
    /// ends the output and must match it.
    Unknown,
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
    fn open(link: Link, access: Access, retired: &Arc<AtomicBool>, screen: &Arc<Screen>) -> Self {
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
/// attach's stamped output starts, a controller's or a watcher's; `None`
/// for version 1.
///
/// The session's end of output ([`Frame::OutputEnd`]) ends the relay as the
/// session closing the connection does, never as a lost link, and must be
/// at the offset the output has reached (else exit 80). A watcher whose
/// session promised it (`end`) but that never got it was cut off: exit 84,
/// saying how many bytes of the run's output it had.
///
/// A read-only attach ([`Access::ReadOnly`]) sends no size, resize or
/// repaint, and drops keystrokes, saying so once in the status line; the
/// escape and the signals still leave.
///
/// A version 2 `attach --host` whose connection the far side ends, without
/// a takeover or a broken protocol, hands ssh's end to a
/// [`reconnect::Job`]: the remote's exit code ends the attach with it, and
/// a lost link is resumed, a watcher's too. Meanwhile keystrokes are
/// discarded, with one notice in the status line; the escape and the
/// signals still leave, and stop the reconnect's ssh.
pub(super) fn relay(
    link: Link,
    resume: Option<super::Point>,
    escape: EscapeMode,
    access: Access,
    peer: Peer<'_>,
    terminal: Box<dyn Terminal>,
    end: EndNotice,
) -> anyhow::Result<i32> {
    let (recorded_takeover, mut host) = match peer {
        Peer::Local { recorded_takeover } => (Some(recorded_takeover), None),
        Peer::Host(end) => (None, Some(*end)),
    };
    // Only a version 2 attach knows where to resume from.
    let resumable = resume.is_some();
    let retired = Arc::new(AtomicBool::new(false));
    let status = Arc::new(reconnect::Status::default());

    // Restores the terminal on every way out of this function, panics
    // included.
    let mut terminal = terminal.enter()?;
    let screen = Arc::new(Screen {
        shown: Mutex::new(Shown {
            modes: TermModes::default(),
            resume,
        }),
        done: AtomicBool::new(false),
        bracketed_paste: AtomicBool::new(false),
        broken: OnceLock::new(),
        terminal_gone: AtomicBool::new(false),
        ended: AtomicBool::new(false),
        written: AtomicU64::new(0),
        output: terminal.output(),
    });
    // From here on, what ssh says stays off the session's screen. With no
    // status rows to show it on (no terminal), it goes to stderr as it
    // comes, where the session's output is not.
    if let Some(end) = &host {
        if screen.has_status_rows() {
            end.stderr.set(&status, &screen);
        }
    }

    let mut size = screen.size();
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
    let mut buf = [0u8; 4096];
    let mut dropped = 0usize;
    let ending = loop {
        if connection.as_ref().is_some_and(Connection::closed) {
            // The terminal, or stdout without one, refused the output (a
            // pipeline's consumer gone): nothing to reconnect for, and the
            // session is still there to tell.
            if screen.terminal_gone.load(Ordering::SeqCst) {
                break Ending::OutputGone;
            }
            // The end of the output is the session's end, not a lost link.
            let lost = resumable
                && !retired.load(Ordering::SeqCst)
                && screen.broken.get().is_none()
                && !screen.ended.load(Ordering::SeqCst);
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
                    break pending_leave(terminal.as_mut(), &mut buf)
                        .map_or(Ending::SessionClosed, Ending::Left);
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
                    if let Some(leave) = pending_leave(terminal.as_mut(), &mut buf) {
                        break Ending::Left(leave);
                    }
                    verdict = Some(result);
                    break Ending::SessionClosed;
                }
            }
        }
        let mut timeout = size_due.saturating_duration_since(Instant::now()).min(POLL);
        if let Some(due) = parser.partial_deadline() {
            timeout = timeout.min(due.saturating_duration_since(Instant::now()));
        }
        let (typed, resized) = match terminal.input(&mut buf, timeout) {
            Input::Bytes(n) => (Some(n), false),
            Input::SizeDue => (None, true),
            Input::Wake => (None, false),
            Input::Leave(leave) => break Ending::Left(leave),
            Input::Closed => break Ending::KeyboardClosed,
        };
        if resized {
            // Size notices coalesce, so this only brings the check forward;
            // the poll stays the source of truth.
            size_due = Instant::now();
        }
        if Instant::now() >= size_due {
            size_due = Instant::now() + size_poll;
            let now = screen.size();
            if now.is_some() && now != size {
                size = now;
                // While reconnecting, the size goes with the next
                // connection's greeting.
                if let (Some(connection), Some(size)) = (&connection, now) {
                    connection.resize(size);
                }
            }
        }
        let Some(n) = typed else {
            if resized {
                // Typing may be waiting behind the notice: the next wait
                // reads it, so an unfinished escape sequence meets it before
                // it is flushed, or flushes at once if nothing is waiting.
                continue;
            }
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
        if matches!(
            ending,
            Ending::Detached | Ending::Left(_) | Ending::OutputGone
        ) {
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
        if matches!(ending, Ending::Left(_)) {
            REPORT_GRACE
        } else {
            MODES_GRACE
        },
    );
    drop(terminal);

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
    if let Ending::Left(leave) = ending {
        notes.push(format!("tendr: attach ended by {}", leave.name()));
    }
    if ending != Ending::Detached {
        // Otherwise the takeover is the error returned below.
        if taken_over && matches!(ending, Ending::Left(_)) {
            notes.push("tendr: another client took over this session".to_owned());
        }
        if dropped > 0 {
            notes.push(format!(
                "tendr: {dropped} typed bytes were dropped while the session was not accepting input"
            ));
        }
    }
    if matches!(ending, Ending::Left(_)) {
        exit_left(notes);
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
    if ending == Ending::OutputGone {
        return Err(anyhow::anyhow!(
            "tendr: the output closed; detached from the session"
        ));
    }
    if ending != Ending::SessionClosed {
        return Ok(0);
    }
    // The far side ended the connection: the remote's code, or ssh's
    // failure (a lost version 1 connection) when there is no code.
    let code = verdict.or(finished).unwrap_or(Ok(0));
    if access == Access::ReadOnly && !screen.ended.load(Ordering::SeqCst) {
        let written = screen.written.load(Ordering::SeqCst);
        match end {
            EndNotice::Promised => {
                return Err(PtyExitCode::Runtime.error(anyhow::anyhow!(
                    "tendr: the session cut this read-only attach off after {written} bytes of the run's output, before its end"
                )));
            }
            EndNotice::Unknown if matches!(code, Ok(code) if code == PtyExitCode::Runtime.code()) =>
            {
                eprintln!("tendr: this read-only attach had {written} bytes of the run's output");
            }
            EndNotice::Unknown => {}
        }
    }
    code
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

/// A leave that has arrived but not been read, checked without waiting
/// when the session's end may have been caused by it. Anything typed with
/// it is dropped: the attach is ending either way.
fn pending_leave(terminal: &mut dyn Raw, buf: &mut [u8]) -> Option<Leave> {
    match terminal.input(buf, Duration::ZERO) {
        Input::Leave(leave) => Some(leave),
        _ => None,
    }
}

/// How a relay ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    Detached,
    SessionClosed,
    KeyboardClosed,
    /// The terminal, or stdout without one, refused the session's output.
    OutputGone,
    /// The user's side ended it from outside the keyboard.
    Left(Leave),
}

/// Leave with exit code 1 after the user's side ended the attach
/// ([`Leave`]). The report is written without waiting on the terminal for
/// long, and without panicking if it is gone: SIGHUP means it hung up, and a
/// terminal that stopped reading is a reason to send SIGTERM.
fn exit_left(notes: Vec<String>) -> ! {
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
    /// The session said the run's output ended where this attach's had
    /// reached ([`Frame::OutputEnd`]): it has had all of it. Set before the
    /// reader marks the connection closed.
    ended: AtomicBool,
    /// Bytes of the run's own output (stamped) written to the terminal,
    /// over every connection, for the report when a watcher is cut off.
    written: AtomicU64,
    /// The terminal's screen, which every write to it goes through.
    output: Arc<dyn Output>,
}

impl Screen {
    fn lock(&self) -> MutexGuard<'_, Shown> {
        self.shown.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Write `bytes` to the terminal, if it takes them. The caller holds
    /// [`Screen::shown`], so nothing else writes in between.
    fn write_terminal(&self, bytes: &[u8]) {
        if !bytes.is_empty() {
            let _ = self.output.write(bytes);
        }
    }

    /// The terminal's size, as [`Output::size`].
    fn size(&self) -> Option<tendr::recording::Geometry> {
        self.output.size()
    }

    /// Whether the output is a terminal, as [`Output::is_terminal`].
    pub(super) fn is_terminal(&self) -> bool {
        self.output.is_terminal()
    }

    /// Whether a reconnect's status can be drawn on the terminal's last
    /// rows: there is a terminal of a known size. Without one (stdout not a
    /// terminal, or a terminal stdout while stdin is not one, which leaves
    /// the attach without a terminal layer) the status goes to stderr.
    fn has_status_rows(&self) -> bool {
        self.size().is_some()
    }
}

/// The terminal as the session's output has left it.
struct Shown {
    /// The modes turned on by everything written so far.
    modes: TermModes,
    /// For a version 2 attach, its point in the run: the offset just past
    /// the last output byte written to the terminal, where a reconnect
    /// would resume, and a controller's epoch. `None` for version 1.
    resume: Option<super::Point>,
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
    /// closed. The remote bridge's, which runs on Unix only.
    #[cfg(unix)]
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
                Ok(Frame::OutputEnd(offset)) => end_output(&screen, offset),
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
            Some(point) if point.offset() != offset => {
                return Err(format!(
                    "the session's output skipped from offset {} to {offset}",
                    point.offset()
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
    if screen.output.write(bytes).is_err() {
        screen.terminal_gone.store(true, Ordering::SeqCst);
        return Ok(false);
    }
    if let (Some(_), Some(point)) = (offset, shown.resume.as_mut()) {
        point.advance(bytes.len() as u64);
        screen
            .written
            .fetch_add(bytes.len() as u64, Ordering::SeqCst);
    }
    Ok(true)
}

/// The session's end of output, at `offset`: nothing more comes, so the
/// reader stops (`Ok(false)`), the output complete. It must be where the
/// stamped output has reached, or it is refused, with why: output the
/// session never sent, or an end it claims before output it did send.
fn end_output(screen: &Screen, offset: u64) -> Result<bool, String> {
    let shown = screen.lock();
    match &shown.resume {
        None => Err("the session sent an end of output to a version 1 attach".to_owned()),
        Some(point) if point.offset() != offset => Err(format!(
            "the session's output ended at offset {offset}, but this attach has it up to {}",
            point.offset()
        )),
        Some(_) => {
            screen.ended.store(true, Ordering::SeqCst);
            Ok(false)
        }
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
