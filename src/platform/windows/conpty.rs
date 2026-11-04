//! A PTY session's terminal on Windows: a pseudoconsole
//! (`CreatePseudoConsole`), which conhost runs on two pipes. tendr writes
//! the keyboard into one and reads conhost's rendering of the screen from
//! the other.
//!
//! The output pipe ends only when conhost exits, which it does when the
//! pseudoconsole is closed, not when the child exits. So a watcher closes it
//! as soon as the child has exited ([`close_on_exit`]): conhost writes its
//! last frame and exits, ending the output, and ends whatever else is still
//! attached to the console. Closing it is also a PTY child's hangup
//! (`CTRL_CLOSE_EVENT`), as a graceful stop starts with on Unix.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Write};
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};

use crate::recording::Geometry;

/// An open pseudoconsole and the size it was last given.
pub(super) struct PseudoConsole {
    /// `None` once closed. Held across a resize, so a close never frees it
    /// under one.
    hpc: Mutex<Option<HPCON>>,
    size: Mutex<Geometry>,
    /// The read end of its output until capture takes it. A stop drops it
    /// if still here: nothing is reading it, and conhost could not finish.
    output: Mutex<Option<File>>,
}

impl PseudoConsole {
    /// A pseudoconsole of `size`, and the write end of its input. The read
    /// end of its output is [`Self::take_output`]'s.
    ///
    /// # Errors
    ///
    /// Creating a pipe or the pseudoconsole failed.
    pub(super) fn create(size: Geometry) -> io::Result<(Arc<Self>, File)> {
        let (input_read, input_write) = super::pipe()?;
        let (output_read, output_write) = super::pipe()?;
        let mut hpc: HPCON = 0;
        // SAFETY: both handles are open pipe ends; `hpc` is a valid
        // out-pointer. No flags: INHERIT_CURSOR would make conhost wait for
        // a cursor report.
        let hr = unsafe {
            CreatePseudoConsole(
                coord(size),
                input_read.as_raw_handle().cast(),
                output_write.as_raw_handle().cast(),
                0,
                &mut hpc,
            )
        };
        if hr < 0 {
            return Err(io::Error::other(format!(
                "CreatePseudoConsole failed: HRESULT {hr:#010x}"
            )));
        }
        // conhost holds its own duplicates; ours must close, so the output
        // ends when conhost exits.
        drop((input_read, output_write));
        Ok((
            Arc::new(Self {
                hpc: Mutex::new(Some(hpc)),
                size: Mutex::new(size),
                output: Mutex::new(Some(output_read)),
            }),
            input_write,
        ))
    }

    /// The read end of its output, once, for capture.
    pub(super) fn take_output(&self) -> Option<File> {
        lock(&self.output).take()
    }

    /// The pseudoconsole, while it is open, for a child to attach to.
    pub(super) fn handle(&self) -> Option<HPCON> {
        *lock(&self.hpc)
    }

    /// Close it, once: conhost ends what is still attached, writes its last
    /// frame and exits, which ends the output. Returns once conhost has
    /// exited, which on older Windows waits for the output to be read.
    pub(super) fn close(&self) {
        // Taken under the lock, so a resize holding it finishes first and
        // later ones find it closed; closed outside it. Before Windows 11
        // 24H2, ClosePseudoConsole blocks until conhost's output is drained,
        // and a resize waiting on this lock holds the recorder's lock, under
        // which capture records: holding it here would stop the very reads
        // the close waits for. CI's Windows (build 26100) does not block.
        let open = lock(&self.hpc).take();
        if let Some(open) = open {
            // SAFETY: `open` came from CreatePseudoConsole and `take` makes
            // this its only close.
            unsafe { ClosePseudoConsole(open) };
        }
    }

    /// Close it for a stop. Output capture has not taken by now will not
    /// be read (a start that failed before capture began, as on Unix a
    /// stop gives the terminal up when no reader runs): it is given up
    /// first, so conhost's last writes fail rather than wait for a reader.
    /// A child that exits by itself keeps its output for capture, however
    /// late capture takes it.
    pub(super) fn close_for_stop(&self) {
        drop(self.take_output());
        self.close();
    }
}

impl Drop for PseudoConsole {
    fn drop(&mut self) {
        self.close();
    }
}

/// Close `console` once `process`, its child, has exited, on a thread of
/// its own.
///
/// # Errors
///
/// No thread could be started.
pub(super) fn close_on_exit(process: OwnedHandle, console: Arc<PseudoConsole>) -> io::Result<()> {
    std::thread::Builder::new()
        .name("tendr-conpty-close".to_owned())
        .spawn(move || {
            use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};
            // SAFETY: the handle is a process handle with SYNCHRONIZE. A
            // failed wait (never expected) closes at once rather than never.
            unsafe { WaitForSingleObject(process.as_raw_handle().cast(), INFINITE) };
            console.close();
        })
        .map(drop)
}

/// The COORD of `size`, as `CreatePseudoConsole` and `ResizePseudoConsole`
/// take it.
fn coord(size: Geometry) -> COORD {
    let clamp = |n: u16| i16::try_from(n).unwrap_or(i16::MAX);
    COORD {
        X: clamp(size.cols()),
        Y: clamp(size.rows()),
    }
}

/// Sets and reads a pseudoconsole's window size.
pub struct PtyResize(pub(super) Arc<PseudoConsole>);

impl PtyResize {
    /// Apply `size` (`ResizePseudoConsole`); conhost then tells the child.
    ///
    /// # Errors
    ///
    /// `BrokenPipe` once the pseudoconsole is closed, or the call's error.
    pub fn apply(&self, size: Geometry) -> io::Result<()> {
        let hpc = lock(&self.0.hpc);
        let Some(open) = *hpc else {
            return Err(io::ErrorKind::BrokenPipe.into());
        };
        // SAFETY: `open` is an open pseudoconsole; the lock keeps it so.
        let hr = unsafe { ResizePseudoConsole(open, coord(size)) };
        if hr < 0 {
            return Err(io::Error::other(format!(
                "ResizePseudoConsole failed: HRESULT {hr:#010x}"
            )));
        }
        *lock(&self.0.size) = size;
        Ok(())
    }

    /// The size last applied: a pseudoconsole cannot be asked for it.
    #[must_use]
    pub fn size(&self) -> Option<Geometry> {
        Some(*lock(&self.0.size))
    }
}

/// Bytes of input [`PtyWriter`] holds for conhost: what a PTY's input
/// queue holds on Unix.
const INPUT_BUFFER: usize = 64 * 1024;
/// The most the input thread writes into the pipe at once.
const INPUT_CHUNK: usize = 4096;

/// Writes to a pseudoconsole's input without blocking the caller.
///
/// An anonymous pipe has no nonblocking write or readiness wait, so input
/// goes through a bounded buffer that a thread of its own writes into the
/// pipe. [`PtyWriter::try_write`] takes what fits and reports a full buffer
/// as `WouldBlock`; [`PtyWriter::wait_writable`] waits for room. Once the
/// pipe fails (conhost has exited), every write reports it.
pub struct PtyWriter(Arc<InputBuffer>);

struct InputBuffer {
    state: Mutex<InputState>,
    changed: Condvar,
}

#[derive(Default)]
struct InputState {
    pending: VecDeque<u8>,
    /// The pipe's write error, once it failed.
    failed: Option<io::ErrorKind>,
    /// The writer was dropped: the thread writes what is pending and ends.
    dropped: bool,
}

impl PtyWriter {
    /// Start the thread that writes into `pipe`.
    ///
    /// # Errors
    ///
    /// No thread could be started.
    pub(super) fn spawn(mut pipe: File) -> io::Result<Self> {
        let buffer = Arc::new(InputBuffer {
            state: Mutex::new(InputState::default()),
            changed: Condvar::new(),
        });
        let shared = Arc::clone(&buffer);
        std::thread::Builder::new()
            .name("tendr-conpty-input".to_owned())
            .spawn(move || {
                loop {
                    let chunk: Vec<u8> = {
                        let mut state = lock(&shared.state);
                        while state.pending.is_empty() && !state.dropped {
                            state = shared
                                .changed
                                .wait(state)
                                .unwrap_or_else(|e| e.into_inner());
                        }
                        if state.pending.is_empty() {
                            return;
                        }
                        let n = state.pending.len().min(INPUT_CHUNK);
                        state.pending.drain(..n).collect()
                    };
                    shared.changed.notify_all();
                    if let Err(e) = pipe.write_all(&chunk) {
                        lock(&shared.state).failed = Some(e.kind());
                        shared.changed.notify_all();
                        return;
                    }
                }
            })?;
        Ok(Self(buffer))
    }

    /// One attempt that never blocks: takes what fits in the buffer.
    /// `WouldBlock` means it is full.
    ///
    /// # Errors
    ///
    /// `WouldBlock`, or the pipe's error once it failed.
    pub fn try_write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut state = lock(&self.0.state);
        if let Some(kind) = state.failed {
            return Err(kind.into());
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        let room = INPUT_BUFFER.saturating_sub(state.pending.len());
        if room == 0 {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let n = room.min(bytes.len());
        state.pending.extend(&bytes[..n]);
        drop(state);
        self.0.changed.notify_all();
        Ok(n)
    }

    /// Wait up to `timeout` for room in the buffer. `Ok(true)` means a
    /// write may now progress or will report the pipe's error.
    ///
    /// # Errors
    ///
    /// Never; `Result` as on Unix.
    pub fn wait_writable(&self, timeout: Duration) -> io::Result<bool> {
        let full = |s: &mut InputState| s.pending.len() >= INPUT_BUFFER && s.failed.is_none();
        let state = lock(&self.0.state);
        let (mut state, _) = self
            .0
            .changed
            .wait_timeout_while(state, timeout, full)
            .unwrap_or_else(|e| e.into_inner());
        Ok(!full(&mut state))
    }
}

impl Write for PtyWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            match self.try_write(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.wait_writable(Duration::from_secs(1))?;
                }
                other => return other,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for PtyWriter {
    fn drop(&mut self) {
        lock(&self.0.state).dropped = true;
        self.0.changed.notify_all();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// The shared hang detector's bound (`tests/harness`): a wait that
    /// normally takes milliseconds.
    const HANG: Duration = Duration::from_secs(120);

    /// Input written without blocking reaches the pipe in order, and a full
    /// buffer is `WouldBlock` until the reader drains it.
    #[test]
    fn input_reaches_the_pipe_and_a_full_buffer_would_block() {
        let (mut read, write) = super::super::pipe().unwrap();
        let mut writer = PtyWriter::spawn(write).unwrap();
        assert_eq!(writer.try_write(b"abc").unwrap(), 3);
        let mut buf = [0u8; 3];
        read.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"abc");

        // Nothing reads now: the pipe fills, then the buffer.
        let block = vec![b'x'; INPUT_BUFFER];
        let mut written = 0;
        loop {
            match writer.try_write(&block) {
                Ok(n) => written += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("unexpected error: {e}"),
            }
            if written > 64 * INPUT_BUFFER {
                panic!("the buffer never filled");
            }
        }
        // Reading makes room.
        let drained = std::thread::spawn(move || {
            let mut sink = vec![0u8; written];
            read.read_exact(&mut sink).unwrap();
            read
        });
        assert!(writer.wait_writable(HANG).unwrap());
        drop(drained.join().unwrap());
    }

    /// Once the pipe's reader is gone, writes report it.
    #[test]
    fn a_closed_pipe_fails_later_writes() {
        let (read, write) = super::super::pipe().unwrap();
        let mut writer = PtyWriter::spawn(write).unwrap();
        drop(read);
        writer.try_write(b"x").unwrap();
        let block = [b'y'; 4096];
        let failed = loop {
            match writer.try_write(&block) {
                Ok(_) => {}
                // Full before the thread's write failed: the failure wakes it.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    assert!(writer.wait_writable(HANG).unwrap());
                }
                Err(e) => break e.kind(),
            }
        };
        assert_eq!(failed, io::ErrorKind::BrokenPipe);
    }
}
