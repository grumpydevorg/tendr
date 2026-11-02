//! A terminal on piped stdio, for tests: stdin is the keyboard and stdout
//! the screen, of a fixed size. Debug builds select it with
//! `TENDR_TEST_ATTACH_TERMINAL=<rows>x<cols>`, in place of the user's
//! terminal, which then need not exist; release builds ignore it. There is
//! no raw mode to enter or restore, and nothing leaves but the keyboard
//! closing or the escape.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use tendr::recording::Geometry;

use super::{Input, Output, Raw, Terminal};

const ENV: &str = "TENDR_TEST_ATTACH_TERMINAL";

/// The test terminal, if this is a debug build and [`ENV`] selects it.
///
/// # Errors
/// When [`ENV`] is set but is not `<rows>x<cols>` with neither zero.
pub(super) fn from_env() -> anyhow::Result<Option<Pipe>> {
    if !cfg!(debug_assertions) {
        return Ok(None);
    }
    let Some(value) = std::env::var_os(ENV) else {
        return Ok(None);
    };
    let size = value
        .to_str()
        .and_then(|v| v.split_once('x'))
        .and_then(|(rows, cols)| Geometry::new(rows.parse().ok()?, cols.parse().ok()?))
        .ok_or_else(|| anyhow::anyhow!("{ENV} must be <rows>x<cols>, not {value:?}"))?;
    Ok(Some(Pipe { size }))
}

/// The test terminal before the attach.
pub(super) struct Pipe {
    size: Geometry,
}

impl Terminal for Pipe {
    fn enter(self: Box<Self>) -> anyhow::Result<Box<dyn Raw>> {
        Ok(Box::new(PipeRaw {
            keyboard: keyboard(),
            typed: Vec::new(),
            output: Arc::new(PipeOutput { size: self.size }),
        }))
    }
}

/// Read stdin on a thread of its own, chunk by chunk, until it ends: the
/// channel then disconnects. The thread is left to the process exit.
fn keyboard() -> mpsc::Receiver<Vec<u8>> {
    let (send, keys) = mpsc::channel();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    if send.send(buf[..n].to_vec()).is_err() {
                        return;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    });
    keys
}

struct PipeRaw {
    keyboard: mpsc::Receiver<Vec<u8>>,
    /// Read from the keyboard but not yet handed out.
    typed: Vec<u8>,
    output: Arc<PipeOutput>,
}

impl Raw for PipeRaw {
    fn input(&mut self, buf: &mut [u8], timeout: Duration) -> Input {
        if self.typed.is_empty() {
            match self.keyboard.recv_timeout(timeout) {
                Ok(chunk) => self.typed = chunk,
                Err(RecvTimeoutError::Timeout) => return Input::Wake,
                Err(RecvTimeoutError::Disconnected) => return Input::Closed,
            }
        }
        let n = self.typed.len().min(buf.len());
        buf[..n].copy_from_slice(&self.typed[..n]);
        self.typed.drain(..n);
        Input::Bytes(n)
    }

    fn output(&self) -> Arc<dyn Output> {
        Arc::clone(&self.output) as Arc<dyn Output>
    }
}

/// stdout, at the size [`ENV`] gave.
struct PipeOutput {
    size: Geometry,
}

impl Output for PipeOutput {
    fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(bytes).and_then(|()| stdout.flush())
    }

    fn size(&self) -> Option<Geometry> {
        Some(self.size)
    }

    fn is_terminal(&self) -> bool {
        true
    }
}
