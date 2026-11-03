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
//! the process exits. The session ending a connection is the opposite: the
//! relay acts on it only once the reader is done, so the session's last
//! output is on the terminal first.
//!
//! The terminal layer's [`Leave`] (on Unix SIGHUP, SIGTERM and SIGINT; on
//! Windows the console's control events) ends the relay like a detach, and
//! its size notices (SIGWINCH) bring the next size check forward. A leave
//! wins over a session end it raced: the same signal or control event can
//! end ssh, which shares the terminal, and the user asked to leave.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
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

/// One connection's paths to the session: what goes out, and the reader.
struct Connection {
    outgoing: Outgoing,
    /// The reader thread, whose result is how the connection ended. A
    /// failed write by the sender ends nothing by itself: the session's
    /// last output may still be on its way to the terminal, and ends with
    /// the reader.
    reader: std::thread::JoinHandle<ReaderEnd>,
}

impl Connection {
    fn open(link: Link, access: Access, retired: &Arc<AtomicBool>, screen: &Arc<Screen>) -> Self {
        let outbox = Arc::new(Outbox::default());
        spawn_sender(link.writer, Arc::clone(&outbox));
        let reader = spawn_reader(link.reader, Arc::clone(retired), Arc::clone(screen));
        Self {
            outgoing: Outgoing {
                outbox,
                access,
                close: link.close,
            },
            reader,
        }
    }

    /// How the connection ended, once its reader is done, with what still
    /// goes out on it: every byte of output the reader delivered is on the
    /// terminal, or never will be. The connection comes back while the
    /// reader runs. A reader that panicked ends it with its panic.
    fn end(self) -> Result<(ReaderEnd, Outgoing), Self> {
        if !self.reader.is_finished() {
            return Err(self);
        }
        let end = self
            .reader
            .join()
            .unwrap_or_else(|panic| ReaderEnd::Panicked(Panic(panic)));
        Ok((end, self.outgoing))
    }
}

/// What a connection sends the session: the outbox its sender thread
/// writes, and how to end the connection.
struct Outgoing {
    outbox: Arc<Outbox>,
    access: Access,
    close: Box<dyn FnOnce() + Send>,
}

impl Outgoing {
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
    /// session treats a closed connection as a detach. The reader, which
    /// may still be writing to the terminal, is not waited for.
    fn close(self) {
        (self.close)();
        self.outbox.close();
    }
}

/// Relay until the attach ends, and return its exit code: for `attach
/// --host`, the remote's when the remote ended it, otherwise 0. Another
/// client taking it over is an error (exit 81), and so is session output
/// that breaks the protocol (exit 80). `resume` is where a version 2
/// attach's stamped output starts, a controller's or a watcher's; `None`
/// for version 1.
///
/// The relay is in one [`Phase`] at a time. A connection's end counts once
/// its reader is done ([`ReaderEnd`]), so whatever ends the attach, the
/// session's last output reaches the terminal first; [`after_reader`]
/// decides what that end means. A failed write to the session only means
/// that end is on its way; the keyboard stays live while the reader drains.
///
/// The session's end of output ([`Frame::OutputEnd`]) ends the relay as the
/// session closing the connection does, never as a lost link, and must be
/// at the offset the output has reached (else exit 80). A watcher whose
/// session promised it (`end`) but that never got it was cut off: exit 84,
/// saying how many bytes of the run's output it had.
///
/// A read-only attach ([`Access::ReadOnly`]) sends no size, resize or
/// repaint, and drops keystrokes, saying so in the status line from the
/// first one on; the escape and the signals still leave.
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
    let (recorded_takeover, host) = match peer {
        Peer::Local { recorded_takeover } => (Some(recorded_takeover), None),
        Peer::Host(end) => (None, Some(end)),
    };
    // Only a version 2 attach knows where to resume from.
    let resumable = if resume.is_some() {
        Resumable::Yes
    } else {
        Resumable::No
    };
    // Set by a reader the session told of a takeover. Not part of its
    // end: the session closes the connection after telling, and a leave
    // in between still reports the takeover.
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
    let connection = Connection::open(link, access, &retired, &screen);
    connection.outgoing.greet(size);
    let mut phase = Phase::Attached { connection, host };

    let size_poll = size_poll_interval();
    let mut size_due = Instant::now() + size_poll;
    let mut parser = EscapeParser::new(escape);
    let mut buf = [0u8; 4096];
    let mut dropped = 0usize;
    let (ending, held) = loop {
        phase = match phase {
            Phase::Attached { connection, host } => match connection.end() {
                Err(connection) => Phase::Attached { connection, host },
                Ok((end, outgoing)) => {
                    let takeover = if retired.load(Ordering::SeqCst) {
                        Takeover::Told
                    } else {
                        Takeover::None
                    };
                    match (after_reader(end, resumable, takeover), host) {
                        (None, Some(host)) => {
                            outgoing.close();
                            Phase::Reconnecting(reconnect::Job::start(
                                *host,
                                Arc::clone(&screen),
                                Arc::clone(&status),
                            ))
                        }
                        // A local socket has no link to lose: its close is
                        // the session's end.
                        (None, None) => {
                            let ending = settle(
                                Ending::Session(SessionEnd::Closed),
                                terminal.as_mut(),
                                &mut buf,
                            );
                            break (
                                ending,
                                Held::Outgoing {
                                    outgoing,
                                    host: None,
                                },
                            );
                        }
                        (Some(ending), host) => {
                            let ending = settle(ending, terminal.as_mut(), &mut buf);
                            let host = host.map(|host| *host);
                            break (ending, Held::Outgoing { outgoing, host });
                        }
                    }
                }
            },
            Phase::Reconnecting(job) => match job.outcome() {
                None => Phase::Reconnecting(job),
                Some((host, reconnect::Outcome::Resumed(link))) => {
                    let connection = Connection::open(link, access, &retired, &screen);
                    connection.outgoing.greet(size);
                    Phase::Attached {
                        connection,
                        host: host.map(Box::new),
                    }
                }
                Some((host, reconnect::Outcome::Ended(verdict))) => {
                    let ending = settle(
                        Ending::Session(SessionEnd::Remote(verdict)),
                        terminal.as_mut(),
                        &mut buf,
                    );
                    break (ending, Held::Host(host));
                }
            },
        };
        let mut timeout = size_due.saturating_duration_since(Instant::now()).min(POLL);
        if let Some(due) = parser.partial_deadline() {
            timeout = timeout.min(due.saturating_duration_since(Instant::now()));
        }
        let (typed, resized) = match terminal.input(&mut buf, timeout) {
            Input::Bytes(n) => (Some(n), false),
            Input::SizeDue => (None, true),
            Input::Wake => (None, false),
            Input::Leave(leave) => break (Ending::Left(leave), phase.into()),
            Input::Closed => break (Ending::KeyboardClosed, phase.into()),
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
                if let (Some(outgoing), Some(size)) = (phase.outgoing(), now) {
                    outgoing.resize(size);
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
                phase.outgoing(),
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
            phase.outgoing(),
            access,
            &status,
            &screen,
            forward,
            &mut dropped,
        );
        if decision == Escape::Detach {
            break (Ending::Detached, phase.into());
        }
    };

    let mut host = match held {
        Held::Outgoing { outgoing, host } => {
            if matches!(
                ending,
                Ending::Detached | Ending::Left(_) | Ending::OutputGone
            ) {
                outgoing.outbox.detach(DETACH_GRACE);
            }
            // Unblocks the sender and reader, which are not waited for.
            outgoing.close();
            host
        }
        // Leaving while reconnecting: stop it, and its ssh. A connection it
        // resumed just now ends as it is dropped, which closes ssh's stdin.
        Held::Reconnecting(job) => job.stop(RECONNECT_STOP_GRACE).and_then(|(host, _)| host),
        Held::Host(host) => host,
    };
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
    let taken_over = !matches!(ending, Ending::Detached)
        && (retired.load(Ordering::SeqCst)
            || (ending.by_session() && recorded_takeover.is_some_and(|f| f())));
    // What the remote said on the reconnect's last attempt, such as why
    // it refused to resume, which the status line could only glimpse.
    let mut notes = status.take_lines();
    if let Ending::Left(leave) = &ending {
        notes.push(format!("tendr: attach ended by {}", leave.name()));
    }
    if !matches!(ending, Ending::Detached) {
        // Otherwise the takeover is the error returned below.
        if taken_over && matches!(ending, Ending::Left(_)) {
            notes.push("tendr: another client took over this session".to_owned());
        }
        if dropped > 0 {
            notes.push(format!(
                "tendr: {dropped} typed bytes were dropped while the session was not accepting input"
            ));
        }
        // The reader's own report went to a raw terminal, and an alternate
        // screen took it away as the modes were turned off.
        if let Ending::Panicked(panic) = &ending {
            notes.push(format!(
                "tendr: the attach's output reader panicked: {}",
                panic.message()
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
        end.finish(if ending.by_session() {
            super::SSH_CLOSE_GRACE
        } else {
            super::SSH_LEAVE_GRACE
        })
    });
    let watched = (access == Access::ReadOnly).then(|| Watched {
        end,
        written: screen.written.load(Ordering::SeqCst),
    });
    let (code, note) = verdict(ending, taken_over, finished, watched);
    if let Some(note) = note {
        eprintln!("{note}");
    }
    code
}

/// Where the relay is: what it holds, and so what leaving must close.
enum Phase {
    /// Relaying over a connection. `host` is ssh's end for `attach --host`,
    /// boxed: the phase is moved on every turn of the relay.
    Attached {
        connection: Connection,
        host: Option<Box<super::HostEnd>>,
    },
    /// The link was lost; the reconnect holds ssh's end until it resumes
    /// or ends. Keystrokes are discarded.
    Reconnecting(reconnect::Job),
}

impl Phase {
    /// Where keystrokes and sizes go, if anywhere.
    fn outgoing(&self) -> Option<&Outgoing> {
        match self {
            Phase::Attached { connection, .. } => Some(&connection.outgoing),
            Phase::Reconnecting(_) => None,
        }
    }
}

/// What the relay still holds as it ends, for leaving to close.
enum Held {
    /// A connection's way out, its reader done or left running, and
    /// ssh's end.
    Outgoing {
        outgoing: Outgoing,
        host: Option<super::HostEnd>,
    },
    /// A reconnect, still running.
    Reconnecting(reconnect::Job),
    /// Only ssh's end: a reconnect ended without resuming.
    Host(Option<super::HostEnd>),
}

impl From<Phase> for Held {
    /// The relay ends in `phase`. A connection's reader is left running,
    /// not waited for.
    fn from(phase: Phase) -> Self {
        match phase {
            Phase::Attached { connection, host } => Held::Outgoing {
                outgoing: connection.outgoing,
                host: host.map(|host| *host),
            },
            Phase::Reconnecting(job) => Held::Reconnecting(job),
        }
    }
}

/// Whether a closed connection can be resumed: only a version 2 attach
/// knows the offset to resume from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resumable {
    Yes,
    No,
}

/// Whether the session told this attach another client took it over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Takeover {
    None,
    Told,
}

/// What a connection's end means for the relay: `None` to reconnect,
/// which only a connection that closed where it can be resumed, without a
/// takeover, may do. A complete run's output, a broken frame and a
/// takeover are the session's word, and a terminal that refused output
/// leaves nothing to reconnect for.
fn after_reader(end: ReaderEnd, resumable: Resumable, takeover: Takeover) -> Option<Ending> {
    match end {
        ReaderEnd::Panicked(panic) => Some(Ending::Panicked(panic)),
        ReaderEnd::TerminalGone => Some(Ending::OutputGone),
        ReaderEnd::Broken(why) => Some(Ending::Broken(why)),
        ReaderEnd::OutputComplete => Some(Ending::Session(SessionEnd::OutputComplete)),
        ReaderEnd::Closed if resumable == Resumable::Yes && takeover == Takeover::None => None,
        ReaderEnd::Closed => Some(Ending::Session(SessionEnd::Closed)),
    }
}

/// A watcher's account at the end: whether its session promised to say
/// where the run's output ends, and how many bytes of it were written.
struct Watched {
    end: EndNotice,
    written: u64,
}

/// The attach's exit code, and a note for stderr, from how the relay
/// ended, whether another client took this one over (never after a
/// detach), ssh's end (`finished`, for `--host`) and, for a watcher, its
/// account. The first that applies decides: a reader's panic, raised again
/// now that the attach has cleaned up after it, then output outside the
/// protocol (80), a takeover (81), output that could not be written (1),
/// the user's own end (0), then the session's: the remote's verdict, else
/// ssh's, else 0. A watcher promised the end of the output that did not get
/// it was cut off (84); one not promised it has the bytes it had reported
/// when the session's code is 84.
fn verdict(
    ending: Ending,
    taken_over: bool,
    finished: Option<anyhow::Result<i32>>,
    watched: Option<Watched>,
) -> (anyhow::Result<i32>, Option<String>) {
    debug_assert!(
        !(taken_over && matches!(ending, Ending::Detached)),
        "a detach is never a takeover"
    );
    let session = match ending {
        Ending::Panicked(panic) => std::panic::resume_unwind(panic.0),
        Ending::Broken(why) => {
            return (
                Err(PtyExitCode::Protocol.error(anyhow::anyhow!("tendr: {why}"))),
                None,
            );
        }
        // This client lost control: a controller conflict, which a script
        // must be able to tell from a detach.
        _ if taken_over => {
            return (
                Err(PtyExitCode::Control.error(anyhow::anyhow!(
                    "tendr: another client took over this session"
                ))),
                None,
            );
        }
        Ending::OutputGone => {
            return (
                Err(anyhow::anyhow!(
                    "tendr: the output closed; detached from the session"
                )),
                None,
            );
        }
        Ending::Detached | Ending::KeyboardClosed | Ending::Left(_) => return (Ok(0), None),
        Ending::Session(session) => session,
    };
    // The far side ended the connection: the remote's code, or ssh's
    // failure (a lost version 1 connection) when there is no code.
    let (code, complete) = match session {
        SessionEnd::Remote(verdict) => (verdict, false),
        SessionEnd::Closed => (finished.unwrap_or(Ok(0)), false),
        SessionEnd::OutputComplete => (finished.unwrap_or(Ok(0)), true),
    };
    let Some(Watched { end, written }) = watched.filter(|_| !complete) else {
        return (code, None);
    };
    match end {
        EndNotice::Promised => (
            Err(PtyExitCode::Runtime.error(anyhow::anyhow!(
                "tendr: the session cut this read-only attach off after {written} bytes of the run's output, before its end"
            ))),
            None,
        ),
        EndNotice::Unknown if matches!(code, Ok(code) if code == PtyExitCode::Runtime.code()) => (
            code,
            Some(format!(
                "tendr: this read-only attach had {written} bytes of the run's output"
            )),
        ),
        EndNotice::Unknown => (code, None),
    }
}

/// The relay's end, once a leave that arrived but was not yet read has
/// had its say: the session's end may have been caused by it (the same
/// signal can end ssh), and the user asked to leave.
fn settle(ending: Ending, terminal: &mut dyn Raw, buf: &mut [u8]) -> Ending {
    if !ending.by_session() {
        return ending;
    }
    pending_leave(terminal, buf).map_or(ending, Ending::Left)
}

/// Send keystrokes to the session, counting them in `dropped` if the
/// outbox is full. With no connection, while reconnecting, they are
/// discarded: never sent late to a program that has moved on. A
/// read-only attach drops them all, and says so once.
fn send_keystrokes(
    outgoing: Option<&Outgoing>,
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
    let Some(outgoing) = outgoing else {
        status.discard(screen);
        return;
    };
    let len = forward.len();
    if !outgoing.outbox.push_data(forward) {
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
#[derive(Debug)]
enum Ending {
    Detached,
    KeyboardClosed,
    /// The terminal, or stdout without one, refused the session's output.
    OutputGone,
    /// The user's side ended it from outside the keyboard.
    Left(Leave),
    /// The session's output broke the protocol, with why (exit 80).
    Broken(String),
    /// The session's side ended it.
    Session(SessionEnd),
    /// The connection's reader panicked: a bug, never a reason to
    /// reconnect. The attach leaves as on any end, the session's modes
    /// turned off and ssh finished, and then panics with it (exit 101).
    Panicked(Panic),
}

impl Ending {
    /// Whether the session's side ended the relay, not the user's.
    fn by_session(&self) -> bool {
        matches!(self, Ending::Broken(_) | Ending::Session(_))
    }
}

/// How the session's side ended a relay.
#[derive(Debug)]
enum SessionEnd {
    /// It closed the connection; over `--host`, ssh's end says why.
    Closed,
    /// It said the run's output ended where this attach's had reached
    /// ([`Frame::OutputEnd`]): the attach has had all of it.
    OutputComplete,
    /// A reconnect ended it, with the remote's exit code, or its failure.
    Remote(anyhow::Result<i32>),
}

/// Why a connection's reader stopped. It is the reader thread's result,
/// so the relay learns it only once the reader is done, with every byte
/// it will write on the terminal.
#[derive(Debug)]
enum ReaderEnd {
    /// The stream ended or failed: the session closed the connection, or
    /// the link was lost. Also how a reader stops as the relay leaves,
    /// which closes the connection.
    Closed,
    /// The session's end of output, at the offset this attach had reached.
    OutputComplete,
    /// Output outside the protocol, with why.
    Broken(String),
    /// The terminal, or stdout without one, refused a write.
    TerminalGone,
    /// The reader panicked.
    Panicked(Panic),
}

/// A thread's panic, carried until it is raised again.
struct Panic(Box<dyn std::any::Any + Send>);

impl std::fmt::Debug for Panic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Panic(..)")
    }
}

impl Panic {
    /// The panic's message, when it has one.
    fn message(&self) -> &str {
        self.0
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| self.0.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("(no message)")
    }
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
/// [`OUTBOX_DATA_LIMIT`]. A detach goes last, after the input queued
/// before it ([`Outbox::detach`]).
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
    /// A detach waits to be sent, once the input queued before it is.
    detach: bool,
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

    /// The next message to send: controls first, then input, then a
    /// detach; `None` once closed.
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
            if state.detach {
                state.detach = false;
                state.sending_control = true;
                return Some(Frame::Detach);
            }
            state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn sent(&self) {
        self.lock().sending_control = false;
        self.changed.notify_all();
    }

    /// Send a detach after the input queued before it, so keystrokes typed
    /// with the escape reach the session first (#111), and wait up to
    /// `grace` for it to be written. Input that is backed up (a session
    /// not reading) keeps it waiting past `grace`; the caller then closes
    /// the connection, which the session treats as a detach.
    pub(super) fn detach(&self, grace: Duration) {
        let deadline = Instant::now() + grace;
        let mut state = self.lock();
        state.detach = true;
        self.changed.notify_all();
        while !state.closed
            && (state.detach
                || !state.controls.is_empty()
                || !state.data.is_empty()
                || state.sending_control)
        {
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
/// write closes the outbox. Dropping `stream` at the end closes it, which
/// ends ssh's input for `attach --host`.
pub(super) fn spawn_sender(mut stream: impl Write + Send + 'static, outbox: Arc<Outbox>) {
    std::thread::spawn(move || {
        while let Some(frame) = outbox.next() {
            let written = attach_proto::write_frame(&mut stream, &frame);
            outbox.sent();
            if written.is_err() {
                outbox.close();
                return;
            }
        }
    });
}

/// Write the session's output to the terminal until the connection ends,
/// on a thread whose result is how it ended ([`Connection::end`]).
fn spawn_reader(
    mut stream: impl Read + Send + 'static,
    retired: Arc<AtomicBool>,
    screen: Arc<Screen>,
) -> std::thread::JoinHandle<ReaderEnd> {
    std::thread::spawn(move || {
        loop {
            let step = match attach_proto::read_frame(&mut stream) {
                Ok(Frame::Data(payload)) => show(&screen, None, &payload),
                Ok(Frame::Output { offset, bytes }) => show(&screen, Some(offset), &bytes),
                Ok(Frame::OutputEnd(offset)) => Err(end_output(&screen, offset)),
                Ok(Frame::Retired(_)) => {
                    retired.store(true, Ordering::SeqCst);
                    Ok(())
                }
                Ok(_) | Err(FrameError::Malformed(_) | FrameError::Unknown(_)) => Ok(()),
                Err(FrameError::Io(_)) => Err(ReaderEnd::Closed),
            };
            if let Err(end) = step {
                return end;
            }
        }
    })
}

/// Write the session's output to the terminal, or say why the reader
/// stops. Stamped output (`offset`) must start exactly where the last
/// ended, or it is refused unwritten, with why; it moves the resume point
/// only once the terminal has taken all of it. Unstamped output, such as
/// the mode replay, moves nothing.
fn show(screen: &Screen, offset: Option<u64>, bytes: &[u8]) -> Result<(), ReaderEnd> {
    let mut shown = screen.shown.lock().unwrap_or_else(|e| e.into_inner());
    if screen.done.load(Ordering::SeqCst) {
        return Err(ReaderEnd::Closed);
    }
    if let Some(offset) = offset {
        match &shown.resume {
            None => {
                return Err(ReaderEnd::Broken(
                    "the session sent offsets to a version 1 attach".to_owned(),
                ));
            }
            Some(point) if point.offset() != offset => {
                return Err(ReaderEnd::Broken(format!(
                    "the session's output skipped from offset {} to {offset}",
                    point.offset()
                )));
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
        return Err(ReaderEnd::TerminalGone);
    }
    if let (Some(_), Some(point)) = (offset, shown.resume.as_mut()) {
        point.advance(bytes.len() as u64);
        screen
            .written
            .fetch_add(bytes.len() as u64, Ordering::SeqCst);
    }
    Ok(())
}

/// The session's end of output, at `offset`: nothing more comes, so the
/// reader stops, the output complete. It must be where the stamped output
/// has reached, or it is refused, with why: output the session never
/// sent, or an end it claims before output it did send.
fn end_output(screen: &Screen, offset: u64) -> ReaderEnd {
    let shown = screen.lock();
    match &shown.resume {
        None => {
            ReaderEnd::Broken("the session sent an end of output to a version 1 attach".to_owned())
        }
        Some(point) if point.offset() != offset => ReaderEnd::Broken(format!(
            "the session's output ended at offset {offset}, but this attach has it up to {}",
            point.offset()
        )),
        Some(_) => ReaderEnd::OutputComplete,
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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use tendr::recording::Geometry;

    /// What the keyboard side puts in the outbox.
    #[derive(Debug, Clone)]
    enum Push {
        /// Keystrokes of this length.
        Data(usize),
        /// A size, with this many rows.
        Resize(u16),
        Repaint,
    }

    /// Chunk `i` of those pushed: its index, then one byte repeated. Each
    /// chunk differs from the others, so one sent twice, skipped or out of
    /// order cannot match the expected list.
    fn chunk(i: usize, len: usize) -> Vec<u8> {
        let mut bytes = vec![(i as u8).wrapping_mul(97); len];
        let index = (i as u64).to_le_bytes();
        let n = index.len().min(len);
        bytes[..n].copy_from_slice(&index[..n]);
        bytes
    }

    /// Nothing, a yield, or a short sleep, so the two threads interleave
    /// differently from case to case.
    fn pause(kind: u8) {
        match kind % 4 {
            0 => {}
            1 => std::thread::yield_now(),
            2 => std::thread::sleep(Duration::from_micros(50)),
            _ => std::thread::sleep(Duration::from_micros(500)),
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// With the sender draining concurrently, the Data frames it gets
        /// are exactly the accepted chunks, in order and once; every refused
        /// byte is counted as dropped; a chunk is refused only when it could
        /// not fit; and sizes queued in between neither reorder nor lose
        /// input, and the latest size is always sent.
        #[test]
        fn the_sender_gets_every_accepted_chunk_in_order_and_once(
            pushes in proptest::collection::vec(
                prop_oneof![
                    6 => (1..4096usize).prop_map(Push::Data),
                    2 => ((256usize << 10)..(1200 << 10)).prop_map(Push::Data),
                    2 => (1..100u16).prop_map(Push::Resize),
                    1 => Just(Push::Repaint),
                ],
                0..24,
            ),
            producer_pauses in proptest::collection::vec(any::<u8>(), 1..8),
            consumer_pauses in proptest::collection::vec(any::<u8>(), 1..8),
        ) {
            // Built ahead, so pushes come as fast as reads and fill the queue.
            let mut chunks = Vec::new();
            for push in &pushes {
                if let Push::Data(len) = *push {
                    chunks.push(chunk(chunks.len(), len));
                }
            }
            let pushed: usize = chunks.iter().map(Vec::len).sum();
            let mut chunks = chunks.into_iter();

            let outbox = Arc::new(Outbox::default());
            let sender = {
                let outbox = Arc::clone(&outbox);
                std::thread::spawn(move || {
                    let mut frames = Vec::new();
                    while let Some(frame) = outbox.next() {
                        pause(consumer_pauses[frames.len() % consumer_pauses.len()]);
                        frames.push(frame);
                        outbox.sent();
                    }
                    frames
                })
            };

            let mut accepted: Vec<Vec<u8>> = Vec::new();
            let mut accepted_bytes = 0;
            let mut dropped = 0;
            let mut sizes = Vec::new();
            let mut repaints = 0;
            for (i, push) in pushes.iter().enumerate() {
                pause(producer_pauses[i % producer_pauses.len()]);
                match *push {
                    Push::Data(len) => {
                        let bytes = chunks.next().unwrap();
                        // As `send_keystrokes` counts them.
                        if outbox.push_data(bytes.clone()) {
                            accepted_bytes += len;
                            accepted.push(bytes);
                        } else {
                            // Queued bytes are at most the accepted ones.
                            prop_assert!(
                                accepted_bytes + len > OUTBOX_DATA_LIMIT,
                                "{len} bytes refused with {accepted_bytes} accepted so far"
                            );
                            dropped += len;
                        }
                    }
                    Push::Resize(rows) => {
                        let size = Geometry::new(rows, 80).unwrap();
                        sizes.push(size);
                        outbox.push_control(Frame::Resize(size));
                    }
                    Push::Repaint => {
                        repaints += 1;
                        outbox.push_control(Frame::Repaint);
                    }
                }
            }
            // Once the queues are empty every frame is in the sender's hands.
            loop {
                let state = outbox.lock();
                if state.data.is_empty() && state.controls.is_empty() {
                    break;
                }
                drop(state);
                std::thread::sleep(Duration::from_micros(100));
            }
            outbox.close();
            let frames = sender.join().unwrap();

            let mut data = Vec::new();
            let mut sent_sizes = Vec::new();
            let mut sent_repaints = 0;
            for frame in frames {
                match frame {
                    Frame::Data(bytes) => data.push(bytes),
                    Frame::Resize(size) => sent_sizes.push(size),
                    Frame::Repaint => sent_repaints += 1,
                    other => prop_assert!(false, "unexpected {other:?}"),
                }
            }
            prop_assert!(data == accepted, "the sender got other chunks");
            prop_assert_eq!(data.iter().map(Vec::len).sum::<usize>() + dropped, pushed);
            prop_assert_eq!(sent_repaints, repaints);
            let mut later = sizes.iter();
            prop_assert!(
                sent_sizes.iter().all(|sent| later.any(|size| size == sent)),
                "sizes sent out of order: {sent_sizes:?} from {sizes:?}"
            );
            prop_assert_eq!(sent_sizes.last(), sizes.last());
        }
    }

    /// A finished reader's end decides the relay's next step: only a
    /// connection that simply closed, with a point to resume from and no
    /// takeover, reconnects. A complete run, a broken frame and a takeover
    /// end the attach, and a terminal that refused output ends it whatever
    /// else held.
    #[test]
    fn only_a_plain_close_with_a_resume_point_and_no_takeover_reconnects() {
        for resumable in [Resumable::No, Resumable::Yes] {
            for takeover in [Takeover::None, Takeover::Told] {
                let next = |end| after_reader(end, resumable, takeover);
                assert!(matches!(
                    next(ReaderEnd::TerminalGone),
                    Some(Ending::OutputGone)
                ));
                assert!(matches!(
                    next(ReaderEnd::Broken("why".to_owned())),
                    Some(Ending::Broken(why)) if why == "why"
                ));
                assert!(matches!(
                    next(ReaderEnd::OutputComplete),
                    Some(Ending::Session(SessionEnd::OutputComplete))
                ));
                assert!(matches!(
                    next(ReaderEnd::Panicked(Panic(Box::new("boom")))),
                    Some(Ending::Panicked(_))
                ));
                let closed = next(ReaderEnd::Closed);
                if resumable == Resumable::Yes && takeover == Takeover::None {
                    assert!(closed.is_none(), "{closed:?}");
                } else {
                    assert!(
                        matches!(closed, Some(Ending::Session(SessionEnd::Closed))),
                        "{resumable:?}, {takeover:?}: {closed:?}"
                    );
                }
            }
        }
    }

    /// The exit code's order of precedence: a broken protocol over a
    /// takeover, a takeover over output that could not be written, the
    /// user's own end next, then the session's, where the remote's verdict
    /// wins over ssh's end. A watcher promised the end of the output exits
    /// 84 short of it, whatever the session's code; one not promised it
    /// keeps the code, with a note of its bytes only when that code is 84.
    #[test]
    fn the_exit_code_follows_the_ends_precedence() {
        use tendr::pty_exit::exit_code;
        struct Case {
            name: &'static str,
            ending: Ending,
            taken_over: bool,
            finished: Option<anyhow::Result<i32>>,
            watched: Option<EndNotice>,
            code: i32,
            noted: bool,
        }
        let case = |name, ending, taken_over, finished, watched, code| Case {
            name,
            ending,
            taken_over,
            finished,
            watched,
            code,
            noted: false,
        };
        let closed = || Ending::Session(SessionEnd::Closed);
        let cases = [
            case(
                "broken over takeover",
                Ending::Broken("why".into()),
                true,
                Some(Ok(0)),
                None,
                80,
            ),
            case(
                "takeover over the session",
                closed(),
                true,
                Some(Ok(7)),
                None,
                81,
            ),
            case(
                "takeover over output gone",
                Ending::OutputGone,
                true,
                None,
                None,
                81,
            ),
            case(
                "output gone",
                Ending::OutputGone,
                false,
                Some(Ok(7)),
                None,
                1,
            ),
            case("detach", Ending::Detached, false, Some(Ok(7)), None, 0),
            case(
                "keyboard closed",
                Ending::KeyboardClosed,
                false,
                Some(Ok(7)),
                None,
                0,
            ),
            case("ssh's end", closed(), false, Some(Ok(7)), None, 7),
            case("no ssh", closed(), false, None, None, 0),
            case(
                "the remote's verdict over ssh's end",
                Ending::Session(SessionEnd::Remote(Ok(3))),
                false,
                Some(Ok(7)),
                None,
                3,
            ),
            case(
                "a controller at 84",
                closed(),
                false,
                Some(Ok(84)),
                None,
                84,
            ),
            case(
                "a promised watcher cut off",
                closed(),
                false,
                Some(Ok(0)),
                Some(EndNotice::Promised),
                84,
            ),
            case(
                "a promised watcher with all of it",
                Ending::Session(SessionEnd::OutputComplete),
                false,
                Some(Ok(0)),
                Some(EndNotice::Promised),
                0,
            ),
            case(
                "an unpromised watcher",
                closed(),
                false,
                Some(Ok(0)),
                Some(EndNotice::Unknown),
                0,
            ),
            Case {
                noted: true,
                ..case(
                    "an unpromised watcher at 84",
                    closed(),
                    false,
                    Some(Ok(84)),
                    Some(EndNotice::Unknown),
                    84,
                )
            },
        ];
        for c in cases {
            let watched = c.watched.map(|end| Watched { end, written: 42 });
            let (result, note) = verdict(c.ending, c.taken_over, c.finished, watched);
            let message = result.as_ref().err().map(|e| format!("{e:#}"));
            let code = result.unwrap_or_else(|e| exit_code(&e));
            assert_eq!(code, c.code, "{}", c.name);
            assert_eq!(note.is_some(), c.noted, "{}: {note:?}", c.name);
            if let Some(note) = note {
                assert!(note.contains("had 42 bytes"), "{}: {note}", c.name);
            }
            if c.code == 84 && c.watched == Some(EndNotice::Promised) {
                let message = message.unwrap_or_default();
                assert!(message.contains("after 42 bytes"), "{}: {message}", c.name);
            }
        }
    }

    /// A reader's panic is raised again, after the cleanup, with the
    /// reader's own payload, whatever else held.
    #[test]
    fn a_readers_panic_is_raised_again_as_it_was() {
        let raised = std::panic::catch_unwind(|| {
            verdict(
                Ending::Panicked(Panic(Box::new("the reader's own"))),
                true,
                Some(Ok(7)),
                Some(Watched {
                    end: EndNotice::Promised,
                    written: 0,
                }),
            )
        });
        let payload = raised.expect_err("the panic is raised again");
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"the reader's own"));
    }
}
