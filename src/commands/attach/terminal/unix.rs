//! The user's Unix terminal: termios raw mode, the keyboard polled on stdin,
//! the size from `TIOCGWINSZ` on stdout, and the signals.
//!
//! SIGHUP, SIGTERM and SIGINT leave ([`Input::Leave`]), and SIGWINCH brings
//! the next size check forward ([`Input::SizeDue`]). Their handlers only wake
//! the keyboard's poll through a self-pipe ([`signals`]); the previous
//! dispositions are restored when the terminal is.

use std::io::Write;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use std::time::Duration;

use tendr::recording::Geometry;

use super::{Input, Leave, Output, Raw, Terminal};

/// The terminal's state before the attach.
pub(super) struct UnixTerminal {
    original: libc::termios,
}

impl UnixTerminal {
    /// Take the terminal's state.
    pub(super) fn open() -> anyhow::Result<Self> {
        let fd = std::io::stdin().as_raw_fd();
        // SAFETY: termios is plain old data; tcgetattr fills it completely.
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: `fd` is this process's stdin; `original` is a valid out-pointer.
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            anyhow::bail!("failed to get terminal attributes");
        }
        Ok(Self { original })
    }
}

impl Terminal for UnixTerminal {
    fn enter(self: Box<Self>) -> anyhow::Result<Box<dyn Raw>> {
        // Installed before raw mode, so no cancelling signal can arrive
        // between the two and leave the terminal raw; restored after the
        // terminal (see the field order of `UnixRaw`).
        let handlers = signals::Handlers::install()
            .map_err(|e| anyhow::anyhow!("failed to install attach signal handlers: {e}"))?;
        let fd = std::io::stdin().as_raw_fd();
        let mut raw = self.original;
        // SAFETY: `raw` is a valid termios.
        unsafe { libc::cfmakeraw(&mut raw) };
        // SAFETY: `fd` is stdin; `raw` is a valid termios.
        if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &raw) } != 0 {
            anyhow::bail!("failed to set raw mode");
        }
        Ok(Box::new(UnixRaw {
            original: self.original,
            handlers,
        }))
    }
}

/// The terminal in raw mode. Dropping it restores the terminal, then the
/// signal dispositions.
struct UnixRaw {
    original: libc::termios,
    /// Dropped after `Drop::drop` has restored the terminal.
    handlers: signals::Handlers,
}

impl Raw for UnixRaw {
    fn input(&mut self, buf: &mut [u8], timeout: Duration) -> Input {
        let stdin = std::io::stdin();
        let ready = wait_ready(stdin.as_fd(), self.handlers.wake(), timeout);
        if ready.signal {
            let received = self.handlers.take();
            if let Some(cancel) = received.cancel {
                return Input::Leave(Leave(cancel.name()));
            }
            if received.winch {
                return Input::SizeDue;
            }
        }
        if !ready.keyboard {
            return Input::Wake;
        }
        read_keyboard(stdin.as_fd(), buf)
    }

    fn output(&self) -> Arc<dyn Output> {
        Arc::new(UnixOutput)
    }
}

impl Drop for UnixRaw {
    fn drop(&mut self) {
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

/// The terminal's screen: stdout.
struct UnixOutput;

impl Output for UnixOutput {
    fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(bytes).and_then(|()| stdout.flush())
    }

    fn size(&self) -> Option<Geometry> {
        let fd = std::io::stdout().as_raw_fd();
        // SAFETY: winsize is plain old data; TIOCGWINSZ fills it on success.
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: `fd` is stdout; TIOCGWINSZ takes a winsize out-pointer.
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0 {
            Geometry::new(ws.ws_row, ws.ws_col)
        } else {
            None
        }
    }

    fn is_terminal(&self) -> bool {
        true
    }
}

/// No terminal, for a read-only attach whose stdin or stdout is not one:
/// stdin is never read, so only the cancelling signals reach
/// [`Raw::input`], and stdout is a plain stream of the session's output.
pub(super) struct Absent;

impl Terminal for Absent {
    fn enter(self: Box<Self>) -> anyhow::Result<Box<dyn Raw>> {
        let handlers = signals::Handlers::install()
            .map_err(|e| anyhow::anyhow!("failed to install attach signal handlers: {e}"))?;
        Ok(Box::new(AbsentRaw { handlers }))
    }
}

/// [`Absent`] while attached; dropping it gives back the signal
/// dispositions.
struct AbsentRaw {
    handlers: signals::Handlers,
}

impl Raw for AbsentRaw {
    fn input(&mut self, _buf: &mut [u8], timeout: Duration) -> Input {
        use rustix::event::{PollFd, PollFlags, poll};
        let wake = self.handlers.wake();
        let mut fds = [PollFd::new(&wake, PollFlags::IN)];
        if !matches!(poll(&mut fds, Some(&poll_timespec(timeout))), Ok(n) if n > 0) {
            return Input::Wake;
        }
        match self.handlers.take().cancel {
            Some(cancel) => Input::Leave(Leave(cancel.name())),
            // No size to follow.
            None => Input::Wake,
        }
    }

    fn output(&self) -> Arc<dyn Output> {
        Arc::new(AbsentOutput)
    }
}

/// stdout, which may not be a terminal (or not the one being typed at): no
/// size, and modes turned off when leaving only if it is one.
struct AbsentOutput;

impl Output for AbsentOutput {
    fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
        UnixOutput.write(bytes)
    }

    fn size(&self) -> Option<Geometry> {
        None
    }

    /// A stdout that is a terminal (stdin piped) gets its modes turned off
    /// when leaving; a pipe or file gets nothing but the session's output.
    fn is_terminal(&self) -> bool {
        use std::io::IsTerminal;
        std::io::stdout().is_terminal()
    }
}

/// What woke the keyboard's poll.
#[derive(Debug, Default)]
struct Ready {
    /// The keyboard has input, or has closed.
    keyboard: bool,
    /// A signal arrived.
    signal: bool,
}

/// Wait up to `timeout` for the `keyboard` or for a signal's wake-up byte.
fn wait_ready(keyboard: BorrowedFd<'_>, wake: BorrowedFd<'_>, timeout: Duration) -> Ready {
    use rustix::event::{PollFd, PollFlags, poll};
    let mut fds = [
        PollFd::new(&keyboard, PollFlags::IN),
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

/// Read what the `keyboard` has, from the descriptor [`wait_ready`] polls.
/// Never through `std::io::Stdin`: its 8 KiB buffer could take more than
/// `buf` holds, and the rest would wait, unseen by the poll, for the next
/// keystroke (issue #192, a paste cut at 4,096 bytes).
fn read_keyboard(keyboard: BorrowedFd<'_>, buf: &mut [u8]) -> Input {
    match rustix::io::read(keyboard, buf) {
        Ok(0) => Input::Closed,
        Ok(n) => Input::Bytes(n),
        Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => Input::Wake,
        Err(_) => Input::Closed,
    }
}

/// The poll timeout for `timeout`, whole seconds included (saturating for a
/// duration too long to represent).
fn poll_timespec(timeout: Duration) -> rustix::event::Timespec {
    rustix::event::Timespec::try_from(timeout).unwrap_or(rustix::event::Timespec {
        tv_sec: i64::MAX,
        tv_nsec: 999_999_999,
    })
}

/// The attach client's signal handling: a self-pipe that the handlers
/// write to, so the main thread's `poll()` wakes for a signal whichever
/// thread the kernel delivered it to.
mod signals {
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
                new.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
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

    /// Input bigger than one read is read in full: what a read leaves, the
    /// next poll sees.
    #[test]
    fn a_read_leaves_nothing_the_poll_cannot_see() {
        let (keyboard, typing) = rustix::pipe::pipe().unwrap();
        let (wake, _wake_write) = rustix::pipe::pipe().unwrap();
        // Over one 4 KiB read and under a pipe's buffer, so one write.
        let typed: Vec<u8> = (0..6025u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(rustix::io::write(&typing, &typed).unwrap(), typed.len());
        let mut buf = [0u8; 4096];
        let mut got = Vec::new();
        while wait_ready(keyboard.as_fd(), wake.as_fd(), Duration::ZERO).keyboard {
            match read_keyboard(keyboard.as_fd(), &mut buf) {
                Input::Bytes(n) => got.extend_from_slice(&buf[..n]),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(got, typed);
    }

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
        let stdin = std::io::stdin();
        let ready = wait_ready(stdin.as_fd(), handlers.wake(), Duration::from_secs(5));
        assert!(ready.signal, "SIGWINCH wakes the poll");
        let received = handlers.take();
        assert!(received.winch);
        assert_eq!(received.cancel, None);
        let ready = wait_ready(stdin.as_fd(), handlers.wake(), Duration::ZERO);
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
