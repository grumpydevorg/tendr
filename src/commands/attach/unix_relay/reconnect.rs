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
//! before any output follows.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::time::Duration;

use tendr::attach_proto::ResumePoint;
use tendr::ssh::SshError;
use tendr::term_modes::TermModes;

use super::super::{HostEnd, Resumption, SSH_CLOSE_GRACE};
use super::{Link, Screen, write_terminal};

/// The longest pause between two failed attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// How many of an attempt's stderr lines are kept for the report when the
/// remote ends the attach, and how long each may be.
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
            // Only leaving cancels, and leaving uses `stop`.
            Ok((end, None)) => Some((Some(end), Outcome::Ended(Ok(0)))),
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
        let stderr = Box::new(StatusSink {
            status: Arc::clone(status),
            screen: Arc::clone(screen),
            partial: Vec::new(),
        });
        let attempt = end.resume(asked, stderr);
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

/// The reconnect's status, on the terminal's last rows, and what an
/// attempt's ssh said.
#[derive(Default)]
pub(in crate::commands::attach) struct Status {
    /// Held while drawing, so draws and erases do not interleave.
    state: Mutex<State>,
    /// The current attempt's stderr lines, for the report if the remote ends
    /// the attach. Never held while writing, so the main thread can take
    /// them when leaving whatever the terminal does.
    lines: Mutex<Vec<String>>,
    /// Typed input was discarded during this reconnect. Set by the main
    /// thread, which never waits on `state`.
    discarded: AtomicBool,
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

    /// A line of ssh's stderr, while a reconnect is under way; `false`
    /// otherwise, for the caller to pass it on.
    fn ssh_said(&self, line: &[u8], screen: &Screen) -> bool {
        let mut state = self.lock();
        if !state.active {
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
        if let Resumption::Gone {
            point: head,
            oldest,
        } = resumption
        {
            out.extend(shown.modes.reset());
            shown.modes = TermModes::default();
            screen.bracketed_paste.store(false, Ordering::SeqCst);
            out.extend(gone_notice(asked.offset, oldest, head.offset).as_bytes());
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
        let drawn = std::mem::take(&mut state.drawn);
        if drawn == 0 {
            return Vec::new();
        }
        let blank = vec![String::new(); usize::from(drawn)];
        super::terminal_size().map_or_else(Vec::new, |size| {
            status_rows(&blank, size.rows(), size.cols())
        })
    }

    /// The last attempt's stderr lines, which the remote's refusal is among.
    pub(in crate::commands::attach) fn take_lines(&self) -> Vec<String> {
        std::mem::take(&mut self.lines())
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

/// An attempt's ssh stderr: each line goes to the status line while the
/// reconnect is under way, and to stderr once the connection is up.
struct StatusSink {
    status: Arc<Status>,
    screen: Arc<Screen>,
    partial: Vec<u8>,
}

impl StatusSink {
    fn line(&mut self, line: &[u8]) {
        if !self.status.ssh_said(line, &self.screen) {
            let mut stderr = std::io::stderr().lock();
            let _ = stderr.write_all(line).and_then(|()| stderr.flush());
        }
    }
}

impl Write for StatusSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
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

impl Drop for StatusSink {
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
