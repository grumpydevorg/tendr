//! The user's terminal as the attach client drives it: the platform layer
//! under the client core ([`super::client`]). The core touches no file
//! descriptor, signal or console API; everything it needs from the terminal
//! goes through these traits.
//!
//! - [`Terminal`]: the terminal's state, taken by [`open`] before anything
//!   that may change it starts (ssh shares the terminal), and turned into
//!   raw mode by [`Terminal::enter`];
//! - [`Raw`]: the terminal in raw mode, until dropped, which restores the
//!   state taken and discards typed-ahead input, on every way out, panics
//!   included. Only the relay's main thread reads its keyboard
//!   ([`Raw::input`]);
//! - [`Output`]: its screen and size, shared by the relay's threads, which
//!   write only while holding the relay's screen lock.
//!
//! The layers: the user's Unix terminal ([`unix`]), and in debug builds a
//! test terminal on piped stdio ([`pipe`]).

use std::sync::Arc;
use std::time::Duration;

use tendr::recording::Geometry;

mod pipe;
mod unix;

/// What one wait for the keyboard got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Input {
    /// This many bytes were typed, at the start of the buffer.
    Bytes(usize),
    /// The terminal may have changed size: check it now rather than at the
    /// next poll. Such notices coalesce, so the poll stays the source of
    /// truth; this only brings the check forward.
    SizeDue,
    /// The user's side ended the attach from outside the keyboard.
    Leave(Leave),
    /// Nothing typed before the timeout, or a wake-up for nothing the relay
    /// acts on.
    Wake,
    /// The keyboard closed or failed.
    Closed,
}

/// Why the user's side ended the attach from outside the keyboard (on Unix,
/// a cancelling signal). The relay leaves as for a detach, then exits 1,
/// saying which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Leave(&'static str);

impl Leave {
    /// What the user sees: `tendr: attach ended by <name>`.
    pub(super) fn name(self) -> &'static str {
        self.0
    }
}

/// The terminal's screen, shared by the relay's threads.
pub(super) trait Output: Send + Sync {
    /// Write all of `bytes` and flush them. An error means the terminal is
    /// gone or refuses them.
    fn write(&self, bytes: &[u8]) -> std::io::Result<()>;

    /// The terminal's size; `None` if unknown or if either dimension is
    /// zero, which the session could not apply.
    fn size(&self) -> Option<Geometry>;
}

/// The terminal's state, taken before anything that may change it starts.
pub(super) trait Terminal {
    /// Raw mode, with the user's ways of leaving from outside the keyboard
    /// taken over first, so none can arrive in between and leave the
    /// terminal raw.
    ///
    /// # Errors
    /// When the terminal cannot be put in raw mode, or leaving cannot be
    /// taken over.
    fn enter(self: Box<Self>) -> anyhow::Result<Box<dyn Raw>>;
}

/// The terminal in raw mode. Dropping it restores the state taken, then
/// discards typed-ahead input, and gives back the ways of leaving it took
/// over. The restore never waits for pending output to drain: a terminal
/// that stopped reading cannot hold the exit.
pub(super) trait Raw {
    /// Wait at most `timeout` for the keyboard, reading what was typed into
    /// `buf`.
    fn input(&mut self, buf: &mut [u8], timeout: Duration) -> Input;

    /// The terminal's screen.
    fn output(&self) -> Arc<dyn Output>;
}

/// The terminal an attach drives, its state taken now: the user's, whose
/// stdin must be a terminal, or in debug builds the test terminal that
/// `TENDR_TEST_ATTACH_TERMINAL` selects ([`pipe`]). Called before anything
/// with a side effect, a takeover or ssh included.
///
/// # Errors
/// When stdin is not a terminal, or the terminal's state cannot be read.
pub(super) fn open() -> anyhow::Result<Box<dyn Terminal>> {
    if let Some(pipe) = pipe::from_env()? {
        return Ok(Box::new(pipe));
    }
    require_terminal()?;
    Ok(Box::new(unix::UnixTerminal::open()?))
}

/// An attach drives the terminal on stdin.
fn require_terminal() -> anyhow::Result<()> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        Ok(())
    } else {
        anyhow::bail!("attach requires an interactive terminal on stdin")
    }
}
