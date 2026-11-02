//! Reconnecting a lost `attach --host` connection, on its own thread, while
//! the relay's main thread keeps the keyboard.
//!
//! The far side ended the connection. ssh's end says whose doing that was:
//! the remote's exit code is its verdict, and the attach ends with it. ssh
//! failing (255 without an exit trailer), killed by a signal, or still
//! running [`SSH_CLOSE_GRACE`](super::super::SSH_CLOSE_GRACE) after its
//! output ended is a lost link. Then the old reader is joined, so the resume
//! point is final, and attempts follow, each a new ssh that never prompts,
//! with a growing pause between failed ones, until one resumes, the remote
//! refuses (its exit code ends the attach), or the user leaves.
//!
//! While it runs, a status line on the terminal's last row says so, with a
//! note once typed input has been discarded, and ssh's latest stderr line
//! goes on a row of its own below it, so an approval URL is visible. They
//! are drawn with the cursor saved and restored (DECSC, DECRC), and erased
//! before any output follows. What ssh says while a connection is up is
//! kept off the session's screen ([`SshStderr`]).

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, mpsc};
use std::time::Duration;

use tendr::attach_proto::ResumePoint;
use tendr::ssh::SshError;
use tendr::term_modes::TermModes;

use super::super::{HostEnd, Resumption, SSH_CLOSE_GRACE};
use super::{Link, Screen, write_terminal};

/// The longest pause between two failed attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// How many of ssh's stderr lines are kept for the report when the attach
/// ends, and how long each may be.
const KEPT_LINES: usize = 20;
const KEPT_LINE_BYTES: usize = 1024;

/// How often a pause between attempts looks at the cancel flag.
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// What one attempt to resume got.
pub(in crate::commands::attach) enum Attempt {
    /// The session answered: the link, relaying from the resume point on.
    Resumed(Link, Resumption),
    /// The remote ended without answering, with this exit code: its verdict.
    Ended(i32),
}

/// How a reconnect ended.
pub(in crate::commands::attach) enum Outcome {
    /// A new connection, its status line erased and, after a gone answer,
    /// the tracked modes reset: relay over it.
    Resumed(Link),
    /// The remote ended the attach, with its exit code, or it failed.
    Ended(anyhow::Result<i32>),
}

/// A reconnect running on its own thread.
pub(in crate::commands::attach) struct Job {
    done: mpsc::Receiver<(HostEnd, Option<Outcome>)>,
    cancel: Arc<AtomicBool>,
}

impl Job {
    /// Reconnect through `end` after its connection ended, once `reader`, the
    /// old connection's reader, is done.
    pub(in crate::commands::attach) fn start(
        end: HostEnd,
        reader: std::thread::JoinHandle<()>,
        screen: Arc<Screen>,
        status: Arc<Status>,
    ) -> Self {
        let cancel = Arc::clone(&end.cancel);
        let (send, done) = mpsc::channel();
        std::thread::spawn(move || {
            let mut end = end;
            let outcome = reconnect(&mut end, reader, &screen, &status);
            let _ = send.send((end, outcome));
        });
        Self { done, cancel }
    }

    /// How the reconnect ended, once it has, and the ssh end back.
    pub(in crate::commands::attach) fn outcome(&self) -> Option<(Option<HostEnd>, Outcome)> {
        match self.done.try_recv() {
            Ok((end, Some(outcome))) => Some((Some(end), outcome)),
            // Only leaving cancels, and leaving uses `stop`; otherwise there
            // was no resume point to start from.
            Ok((end, None)) => Some((
                Some(end),
                Outcome::Ended(Err(anyhow::anyhow!(
                    "tendr: the connection was lost, and there is no point to resume from"
                ))),
            )),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some((
                None,
                Outcome::Ended(Err(anyhow::anyhow!(
                    "tendr: the reconnect stopped unexpectedly"
                ))),
            )),
        }
    }

    /// Stop the reconnect, and its ssh, as the attach leaves: wait at most
    /// `grace` for it, and return the ssh end if it came back.
    pub(in crate::commands::attach) fn stop(
        self,
        grace: Duration,
    ) -> Option<(Option<HostEnd>, Option<Outcome>)> {
        self.cancel.store(true, Ordering::SeqCst);
        self.done
            .recv_timeout(grace)
            .ok()
            .map(|(end, outcome)| (Some(end), outcome))
    }
}

/// The reconnect's thread: `None` once cancelled.
fn reconnect(
    end: &mut HostEnd,
    reader: std::thread::JoinHandle<()>,
    screen: &Arc<Screen>,
    status: &Arc<Status>,
) -> Option<Outcome> {
    match end.finish(SSH_CLOSE_GRACE) {
        Some(Ok(code)) => return Some(Outcome::Ended(Ok(code))),
        Some(Err(e)) if !lost_link(&e) => return Some(Outcome::Ended(Err(e))),
        _ => {}
    }
    // Every byte the old connection delivered has been written once its
    // reader is done, so the offset is final.
    let _ = reader.join();
    let cancel = Arc::clone(&end.cancel);
    let cancelled = || cancel.load(Ordering::SeqCst);
    let asked = screen.lock().resume?;
    let mut failures = 0;
    loop {
        if cancelled() {
            return None;
        }
        status.attempt(failures + 1, screen);
        let attempt = end.resume(asked);
        if cancelled() {
            return None;
        }
        match attempt {
            Ok(Attempt::Resumed(link, resumption)) => {
                status.resumed(screen, asked, resumption);
                return Some(Outcome::Resumed(link));
            }
            Ok(Attempt::Ended(code)) => {
                status.end(screen);
                return Some(Outcome::Ended(Ok(code)));
            }
            Err(e) if lost_link(&e) => {
                failures += 1;
                let pause = backoff(failures);
                status.failed(pause, screen);
                if pause_unless(&cancel, pause) {
                    return None;
                }
            }
            Err(e) => {
                status.end(screen);
                return Some(Outcome::Ended(Err(e)));
            }
        }
    }
}

/// Whether `error` is the link failing rather than the remote ending: ssh
/// failing to connect or stay connected (255 without an exit trailer),
/// killed by a signal, or not getting through to the remote tendr in time.
fn lost_link(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<SshError>(),
        Some(SshError::TransportFailed | SshError::Abnormal | SshError::ConnectDeadline { .. })
    )
}

/// The pause after `failures` failed attempts in a row: 1 s, doubling, at
/// most [`MAX_BACKOFF`].
fn backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(5);
    Duration::from_secs(1 << doublings).min(MAX_BACKOFF)
}

/// Pause for `pause`, unless `cancel` is set first; whether it was.
fn pause_unless(cancel: &AtomicBool, pause: Duration) -> bool {
    let until = std::time::Instant::now() + pause;
    loop {
        if cancel.load(Ordering::SeqCst) {
            return true;
        }
        let left = until.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return false;
        }
        std::thread::sleep(left.min(CANCEL_POLL));
    }
}

/// The reconnect's status, on the terminal's last rows, and what ssh said
/// since the current attempt began or the connection came up.
#[derive(Default)]
pub(in crate::commands::attach) struct Status {
    /// Held while drawing, so draws and erases do not interleave.
    state: Mutex<State>,
    /// ssh's stderr lines since the current attempt began, or since the
    /// connection came up, for the report when the attach ends: a remote's
    /// reason for ending it is among them. Like `notices`, never held while
    /// writing, so the main thread can take them when leaving whatever the
    /// terminal does.
    lines: Mutex<Vec<String>>,
    /// The gone notices, said again once the terminal is restored, since a
    /// program on the alternate screen hides them.
    notices: Mutex<Vec<String>>,
    /// Typed input was discarded during this reconnect. Set by the main
    /// thread, which never waits on `state`.
    discarded: AtomicBool,
    /// A read-only attach has said that it drops typed input.
    read_only_said: AtomicBool,
}

#[derive(Default)]
struct State {
    /// A reconnect is under way and its status is on the terminal.
    active: bool,
    /// How many of the last rows it has drawn on, to erase.
    drawn: u16,
    attempt: u32,
    /// After a failed attempt, the pause before the next.
    pause: Option<Duration>,
    /// ssh's latest non-blank stderr line.
    ssh_line: Option<String>,
}

impl Status {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn notices(&self) -> MutexGuard<'_, Vec<String>> {
        self.notices.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lines(&self) -> MutexGuard<'_, Vec<String>> {
        self.lines.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Note that typed input was discarded: drawn once, on a thread of its
    /// own, since the main thread never waits on the terminal.
    pub(in crate::commands::attach) fn discard(self: &Arc<Self>, screen: &Arc<Screen>) {
        if !self.discarded.swap(true, Ordering::SeqCst) {
            let (status, screen) = (Arc::clone(self), Arc::clone(screen));
            std::thread::spawn(move || status.draw(&mut status.lock(), &screen));
        }
    }

    /// Note, once, that a read-only attach drops typed input: on the last
    /// row, on a thread of its own, as [`Self::discard`] does. The session's
    /// output may draw over it; it is a notice, not a status kept on screen.
    pub(in crate::commands::attach) fn read_only(self: &Arc<Self>, screen: &Arc<Screen>) {
        if !self.read_only_said.swap(true, Ordering::SeqCst) {
            let (status, screen) = (Arc::clone(self), Arc::clone(screen));
            std::thread::spawn(move || {
                let _state = status.lock();
                let _shown = screen.lock();
                if screen.done.load(Ordering::SeqCst) {
                    return;
                }
                if let Some(size) = super::terminal_size() {
                    let notice = ["tendr: read-only attach; typed input is dropped".to_owned()];
                    write_terminal(&status_rows(&notice, size.rows(), size.cols()));
                }
            });
        }
    }

    /// Attempt `attempt` begins.
    fn attempt(&self, attempt: u32, screen: &Screen) {
        self.lines().clear();
        let mut state = self.lock();
        state.active = true;
        state.attempt = attempt;
        state.pause = None;
        self.draw(&mut state, screen);
    }

    /// The attempt failed; the next follows after `pause`.
    fn failed(&self, pause: Duration, screen: &Screen) {
        let mut state = self.lock();
        state.pause = Some(pause);
        self.draw(&mut state, screen);
    }

    /// A line of ssh's stderr. While a reconnect is under way it goes on the
    /// status line. While a connection is up it is kept: ssh says why it
    /// gave up just before it exits, which the next reconnect's status line
    /// then shows, and the report at the end of the attach says the rest.
    /// `false` once the attach is leaving, for the caller to write it to
    /// stderr itself.
    fn ssh_said(&self, line: &[u8], screen: &Screen) -> bool {
        let mut state = self.lock();
        if screen.done.load(Ordering::SeqCst) {
            return false;
        }
        let text = String::from_utf8_lossy(line);
        let text = text.trim_end_matches(['\r', '\n']);
        {
            let mut lines = self.lines();
            if lines.len() == KEPT_LINES {
                lines.remove(0);
            }
            lines.push(text.to_owned());
        }
        if !text.trim().is_empty() {
            state.ssh_line = Some(text.trim().to_owned());
        }
        self.draw(&mut state, screen);
        true
    }

    /// Draw the status as it stands, unless no reconnect is under way or
    /// the attach is leaving: tendr's line, and below it ssh's latest line,
    /// on a row of its own so that an approval URL fits.
    fn draw(&self, state: &mut State, screen: &Screen) {
        if !state.active {
            return;
        }
        let mut text = vec![state.text(self.discarded.load(Ordering::SeqCst))];
        text.extend(state.ssh_line.clone());
        let _shown = screen.lock();
        if screen.done.load(Ordering::SeqCst) {
            return;
        }
        if let Some(size) = super::terminal_size() {
            write_terminal(&status_rows(&text, size.rows(), size.cols()));
            state.drawn = state
                .drawn
                .max(u16::try_from(text.len()).unwrap_or(u16::MAX));
        }
    }

    /// The reconnect ended without a connection: erase the status.
    fn end(&self, screen: &Screen) {
        let mut state = self.lock();
        let _shown = screen.lock();
        write_terminal(&self.erase(&mut state));
    }

    /// The session answered the resume from `asked`: erase the status, and
    /// make the terminal ready for what follows. After a gone answer, that
    /// is the reset of the modes tracked so far (the replay that follows only
    /// turns modes on) and a notice of what was lost. Then the resume point
    /// becomes the new connection's.
    fn resumed(&self, screen: &Screen, asked: ResumePoint, resumption: Resumption) {
        let mut state = self.lock();
        let mut shown = screen.lock();
        let mut out = self.erase(&mut state);
        // The connection is up: what its attempts said is no longer news.
        self.lines().clear();
        if let Resumption::Gone {
            point: head,
            oldest,
        } = resumption
        {
            out.extend(shown.modes.reset());
            shown.modes = TermModes::default();
            screen.bracketed_paste.store(false, Ordering::SeqCst);
            let notice = gone_notice(asked.offset, oldest, head.offset);
            out.extend(notice.as_bytes());
            self.notices().push(notice.trim().to_owned());
        }
        shown.resume = Some(resumption.point());
        write_terminal(&out);
    }

    /// The attach is leaving: erase the status, and turn off the modes the
    /// session turned on.
    pub(in crate::commands::attach) fn leave(&self, screen: &Screen) {
        let mut state = self.lock();
        let shown = screen.lock();
        let mut out = self.erase(&mut state);
        out.extend(shown.modes.reset());
        write_terminal(&out);
    }

    /// End the reconnect's display: the bytes that erase its rows, if
    /// drawn.
    fn erase(&self, state: &mut State) -> Vec<u8> {
        self.discarded.store(false, Ordering::SeqCst);
        state.active = false;
        state.ssh_line = None;
        let drawn = std::mem::take(&mut state.drawn);
        if drawn == 0 {
            return Vec::new();
        }
        let blank = vec![String::new(); usize::from(drawn)];
        super::terminal_size().map_or_else(Vec::new, |size| {
            status_rows(&blank, size.rows(), size.cols())
        })
    }

    /// What to say once the terminal is restored: the gone notices, then the
    /// stderr of the attempt whose remote ended the attach.
    pub(in crate::commands::attach) fn take_lines(&self) -> Vec<String> {
        let mut said = std::mem::take(&mut *self.notices());
        said.append(&mut self.lines());
        said
    }
}

impl State {
    fn text(&self, discarded: bool) -> String {
        let mut text = match self.pause {
            None => format!(
                "tendr: connection lost, reconnecting (attempt {})",
                self.attempt
            ),
            Some(pause) => format!(
                "tendr: connection lost, attempt {} failed, next in {} s",
                self.attempt,
                pause.as_secs()
            ),
        };
        if discarded {
            text.push_str("; typed input is discarded");
        }
        text
    }
}

/// `lines` on the last rows of a terminal `rows` high and `cols` wide, with
/// the cursor and its attributes saved and restored around them. Only
/// printable ASCII is written, so text from the remote cannot drive the
/// terminal, and at most `cols - 1` characters a row, so no row wraps and
/// scrolls the screen.
fn status_rows(lines: &[String], rows: u16, cols: u16) -> Vec<u8> {
    let first = rows.saturating_sub(u16::try_from(lines.len()).unwrap_or(rows)) + 1;
    let mut out = String::from("\x1b7");
    for (row, text) in (first..=rows).zip(lines) {
        let line: String = text
            .chars()
            .map(|c| {
                if c == ' ' || c.is_ascii_graphic() {
                    c
                } else {
                    '?'
                }
            })
            .take(usize::from(cols).saturating_sub(1))
            .collect();
        out.push_str(&format!("\x1b[{row};1H\x1b[0m\x1b[2K{line}"));
    }
    out.push_str("\x1b8");
    out.into_bytes()
}

/// The notice for a resume from `asked` answered gone: the session holds
/// only `oldest` up to `head`, and goes on from `head`.
fn gone_notice(asked: u64, oldest: u64, head: u64) -> String {
    format!(
        "\r\ntendr: reconnected; {} bytes of output were written while disconnected, \
         and the session no longer held {} of them, so none are shown\r\n",
        head - asked,
        oldest - asked
    )
}

/// Where an `attach --host` connection's ssh stderr goes, once the relay owns
/// the terminal: see [`SshStderr`].
#[derive(Default)]
pub(in crate::commands::attach) struct StderrRoute(OnceLock<(Arc<Status>, Arc<Screen>)>);

impl StderrRoute {
    /// From now on, ssh's stderr lines go to `status`.
    pub(in crate::commands::attach) fn set(&self, status: &Arc<Status>, screen: &Arc<Screen>) {
        let _ = self.0.set((Arc::clone(status), Arc::clone(screen)));
    }
}

/// An `attach --host` connection's ssh stderr, line by line: to tendr's
/// stderr until the relay owns the terminal and once it is leaving, and
/// otherwise to [`Status::ssh_said`], so that nothing ssh says, such as its
/// `Timeout, server not responding.` as a link dies, lands in the session's
/// screen.
pub(in crate::commands::attach) struct SshStderr {
    route: Arc<StderrRoute>,
    partial: Vec<u8>,
}

impl SshStderr {
    pub(in crate::commands::attach) fn new(route: Arc<StderrRoute>) -> Self {
        Self {
            route,
            partial: Vec::new(),
        }
    }

    fn line(&mut self, line: &[u8]) {
        let said = self
            .route
            .0
            .get()
            .is_some_and(|(status, screen)| status.ssh_said(line, screen));
        if !said {
            let mut stderr = std::io::stderr().lock();
            let _ = stderr.write_all(line).and_then(|()| stderr.flush());
        }
    }
}

impl Write for SshStderr {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.route.0.get().is_none() {
            // Not routed yet: straight to tendr's stderr, as for any call.
            let mut stderr = std::io::stderr().lock();
            let _ = stderr.write_all(buf).and_then(|()| stderr.flush());
            return Ok(buf.len());
        }
        self.partial.extend_from_slice(buf);
        while let Some(end) = self.partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=end).collect();
            self.line(&line);
        }
        if self.partial.len() > KEPT_LINE_BYTES {
            let line = std::mem::take(&mut self.partial);
            self.line(&line);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for SshStderr {
    fn drop(&mut self) {
        if !self.partial.is_empty() {
            let line = std::mem::take(&mut self.partial);
            self.line(&line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pause_doubles_from_a_second_up_to_thirty() {
        let pauses: Vec<u64> = (1..=8).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(pauses, [1, 2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(backoff(u32::MAX), MAX_BACKOFF);
    }

    /// Each row stays on its row: it never reaches the last column, where
    /// the terminal would wrap and scroll, and carries no control bytes from
    /// the remote's text. The rows are the terminal's last.
    #[test]
    fn the_status_rows_are_printable_and_never_wrap() {
        let lines = [
            "tendr".to_owned(),
            "ssh: \x1b]0;evil\x07 visit https://x.invalid/a".to_owned(),
        ];
        let text = String::from_utf8(status_rows(&lines, 24, 20)).unwrap();
        assert_eq!(
            text,
            "\x1b7\x1b[23;1H\x1b[0m\x1b[2Ktendr\x1b[24;1H\x1b[0m\x1b[2Kssh: ?]0;evil? visi\x1b8"
        );
    }

    #[test]
    fn the_gone_notice_names_what_was_missed_and_what_was_lost() {
        let notice = gone_notice(100, 400, 1000);
        assert!(notice.contains("900"), "{notice}");
        assert!(notice.contains("300"), "{notice}");
    }
}
