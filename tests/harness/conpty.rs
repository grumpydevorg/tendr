//! A Windows console driver for tests: runs a command inside a pseudo console
//! (`CreatePseudoConsole`), types into it, resizes it and captures what it
//! draws, as Windows Terminal would.
//!
//! The output is conhost's rendering of the screen, not the bytes the child
//! wrote: conhost repaints, moves the cursor and wraps as it likes. Assert on
//! [`strip_escapes`] text, never on escape bytes.
//!
//! Cleanup follows the order `ClosePseudoConsole` documents: end the client,
//! keep draining the output (conhost may block writing a final frame), close
//! the pseudo console, then join the reader once the output pipe breaks.

use super::{Observation, poll_until};
use std::ffi::c_void;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;
use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetExitCodeProcess, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

/// Bound on each cleanup step in [`ConPty::close`]. A hang detector, not a
/// performance claim: every step normally takes milliseconds.
const CLOSE_STEP: Duration = Duration::from_secs(10);

/// Path to the `conmode` helper (src/bin/conmode.rs).
#[allow(dead_code)]
pub fn conmode_bin() -> String {
    env!("CARGO_BIN_EXE_conmode").to_owned()
}

/// A command running inside a pseudo console.
#[allow(dead_code)]
pub struct ConPty {
    /// `None` once closed.
    hpc: Option<HPCON>,
    /// The keyboard: conhost reads this as console input.
    input: Option<File>,
    process: OwnedHandle,
    output: Arc<Mutex<Vec<u8>>>,
    reader: Option<JoinHandle<()>>,
    reader_done: mpsc::Receiver<()>,
}

/// What [`ConPty::close`] observed.
#[allow(dead_code)]
#[derive(Debug)]
pub struct Closed {
    /// The client's exit code; `1` if `close` had to terminate it.
    pub exit_code: u32,
    /// Everything conhost drew, raw.
    pub output: Vec<u8>,
}

#[allow(dead_code)]
impl Closed {
    pub fn text(&self) -> String {
        strip_escapes(&self.output)
    }
}

#[allow(dead_code)]
impl ConPty {
    /// Run `argv` in a new `cols`×`rows` pseudo console. The child inherits no
    /// handles: its stdio are the pseudo console's.
    pub fn spawn(argv: &[&str], cols: i16, rows: i16) -> Self {
        Self::try_spawn(argv, cols, rows)
            .unwrap_or_else(|e| panic!("spawn {argv:?} in a pseudo console: {e}"))
    }

    fn try_spawn(argv: &[&str], cols: i16, rows: i16) -> io::Result<Self> {
        // conhost reads `input_read` and draws into `output_write`.
        let (input_read, input_write) = pipe()?;
        let (output_read, output_write) = pipe()?;
        let mut hpc: HPCON = 0;
        // SAFETY: both handles are open pipe ends; `hpc` is a valid out-pointer.
        // No flags: INHERIT_CURSOR would make conhost wait for a cursor report.
        let hr = unsafe {
            CreatePseudoConsole(
                COORD { X: cols, Y: rows },
                input_read.as_raw_handle(),
                output_write.as_raw_handle(),
                0,
                &mut hpc,
            )
        };
        if hr < 0 {
            return Err(io::Error::other(format!(
                "CreatePseudoConsole: HRESULT {hr:#010x}"
            )));
        }
        // conhost holds its own duplicates. Dropping ours lets the output pipe
        // break once conhost exits, which is how the reader learns of the end.
        drop(input_read);
        drop(output_write);

        let process = match spawn_attached(argv, hpc) {
            Ok(process) => process,
            Err(e) => {
                // SAFETY: `hpc` came from CreatePseudoConsole and is closed once.
                // No client is attached, so nothing is left to draw.
                unsafe { ClosePseudoConsole(hpc) };
                return Err(e);
            }
        };

        let output = Arc::new(Mutex::new(Vec::new()));
        let (done_tx, reader_done) = mpsc::channel();
        let sink = Arc::clone(&output);
        let mut output_read = File::from(output_read);
        let reader = std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                match output_read.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sink.lock().unwrap().extend_from_slice(&chunk[..n]),
                }
            }
            let _ = done_tx.send(());
        });

        Ok(Self {
            hpc: Some(hpc),
            input: Some(File::from(input_write)),
            process,
            output,
            reader: Some(reader),
            reader_done,
        })
    }

    /// Type `bytes`. A carriage return is the Enter key.
    pub fn write(&mut self, bytes: &[u8]) {
        let input = self.input.as_mut().expect("pseudo console still open");
        input.write_all(bytes).expect("write pseudo console input");
        input.flush().expect("flush pseudo console input");
    }

    /// Resize the pseudo console, as a user resizing the terminal window would.
    /// conhost applies it asynchronously: observe the new size, don't assume it.
    pub fn resize(&self, cols: i16, rows: i16) {
        let hpc = self.hpc.expect("pseudo console still open");
        // SAFETY: `hpc` is an open pseudo console.
        let hr = unsafe { ResizePseudoConsole(hpc, COORD { X: cols, Y: rows }) };
        assert!(hr >= 0, "ResizePseudoConsole: HRESULT {hr:#010x}");
    }

    /// Everything conhost has drawn so far, raw.
    pub fn raw(&self) -> Vec<u8> {
        self.output.lock().unwrap().clone()
    }

    /// Everything conhost has drawn so far, with escape sequences removed.
    pub fn text(&self) -> String {
        strip_escapes(&self.output.lock().unwrap())
    }

    /// Probe the drawn text until `probe` finds what it wants, or panic after
    /// `timeout` with `what` and the raw output.
    pub fn wait_for<T>(
        &self,
        what: &str,
        timeout: Duration,
        mut probe: impl FnMut(&str) -> Option<T>,
    ) -> T {
        poll_until(timeout, Duration::from_millis(20), || {
            match probe(&self.text()) {
                Some(found) => Observation::Ready(found),
                None => Observation::Pending(format!("no {what} yet")),
            }
        })
        .unwrap_or_else(|e| panic!("waiting for {what}: {e}\n{}", self.describe_output()))
    }

    /// Wait until the drawn text contains `needle`.
    pub fn wait_contains(&self, needle: &str, timeout: Duration) {
        self.wait_for(&format!("{needle:?}"), timeout, |text| {
            text.contains(needle).then_some(())
        });
    }

    /// The client's exit code if it exits within `timeout`.
    pub fn wait_exit(&self, timeout: Duration) -> Option<u32> {
        // Below INFINITE (u32::MAX), which would never time out.
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        // SAFETY: `process` is an open process handle.
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), ms) } {
            WAIT_OBJECT_0 => {
                let mut code = 0u32;
                // SAFETY: the process has exited; `code` is a valid out-pointer.
                let ok = unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &mut code) };
                assert_ne!(ok, 0, "GetExitCodeProcess: {}", io::Error::last_os_error());
                Some(code)
            }
            WAIT_TIMEOUT => None,
            other => panic!(
                "WaitForSingleObject returned {other}: {}",
                io::Error::last_os_error()
            ),
        }
    }

    /// Tear down in the documented order, failing if any step hangs: end the
    /// client (terminating it if still running), keep draining, close the
    /// pseudo console, then join the reader at end of output.
    pub fn close(mut self) -> Closed {
        let exit_code = match self.wait_exit(Duration::ZERO) {
            Some(code) => code,
            None => {
                self.terminate();
                self.wait_exit(CLOSE_STEP).unwrap_or_else(|| {
                    panic!(
                        "client still running {CLOSE_STEP:?} after TerminateProcess\n{}",
                        self.describe_output()
                    )
                })
            }
        };

        // The reader keeps draining throughout: ClosePseudoConsole may wait for
        // conhost to write a final frame. Close on a helper thread so a hang
        // fails the test instead of stalling it.
        let hpc = self.hpc.take().expect("pseudo console still open");
        let (closed_tx, closed_rx) = mpsc::channel();
        std::thread::spawn(move || {
            // SAFETY: `hpc` came from CreatePseudoConsole; `take` above ensures
            // it is closed once.
            unsafe { ClosePseudoConsole(hpc) };
            let _ = closed_tx.send(());
        });
        if closed_rx.recv_timeout(CLOSE_STEP).is_err() {
            panic!(
                "ClosePseudoConsole did not return within {CLOSE_STEP:?}\n{}",
                self.describe_output()
            );
        }

        drop(self.input.take());
        if self.reader_done.recv_timeout(CLOSE_STEP).is_err() {
            panic!(
                "output pipe still open {CLOSE_STEP:?} after ClosePseudoConsole\n{}",
                self.describe_output()
            );
        }
        self.reader
            .take()
            .expect("reader joined once")
            .join()
            .expect("reader thread");

        Closed {
            exit_code,
            output: std::mem::take(&mut *self.output.lock().unwrap()),
        }
    }

    fn terminate(&self) {
        // SAFETY: `process` is an open process handle. Failure means it has
        // already exited, which the caller's wait observes.
        unsafe { TerminateProcess(self.process.as_raw_handle(), 1) };
    }

    /// The drawn text and the raw bytes (escaped, last 4 KiB) for a failure
    /// message.
    fn describe_output(&self) -> String {
        let raw = self.raw();
        let tail = &raw[raw.len().saturating_sub(4096)..];
        format!(
            "drawn text: {:?}\nraw tail ({} of {} bytes): {}",
            strip_escapes(&raw),
            tail.len(),
            raw.len(),
            tail.escape_ascii()
        )
    }
}

impl Drop for ConPty {
    /// A test that panicked before `close`: end the client and close the
    /// pseudo console without waiting, so a stuck conhost can't hang the run.
    fn drop(&mut self) {
        if let Some(hpc) = self.hpc.take() {
            self.terminate();
            std::thread::spawn(move || {
                // SAFETY: `hpc` came from CreatePseudoConsole; `take` above
                // ensures it is closed once.
                unsafe { ClosePseudoConsole(hpc) };
            });
        }
    }
}

/// Start `argv` attached to the pseudo console `hpc`.
fn spawn_attached(argv: &[&str], hpc: HPCON) -> io::Result<OwnedHandle> {
    let mut cmdline = String::new();
    for arg in argv {
        push_quoted_arg(&mut cmdline, arg);
    }
    let mut cmdline: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();

    let mut attrs = AttributeList::with_pseudo_console(hpc)?;
    // SAFETY: zeroed is a valid STARTUPINFOEXW (integers and null pointers).
    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = u32::try_from(std::mem::size_of::<STARTUPINFOEXW>()).unwrap();
    // Null std handles under STARTF_USESTDHANDLES: the child gets the pseudo
    // console's handles, not this process's redirected ones (cargo test pipes
    // them), which it would otherwise be handed and write to instead.
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.lpAttributeList = attrs.as_mut_ptr();

    // SAFETY: zeroed is a valid PROCESS_INFORMATION (handles and integers).
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `cmdline` is a mutable NUL-terminated UTF-16 buffer; `si` and
    // its attribute list outlive the call; no handles are inherited.
    let ok = unsafe {
        CreateProcessW(
            std::ptr::null(),
            cmdline.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            EXTENDED_STARTUPINFO_PRESENT,
            std::ptr::null(),
            std::ptr::null(),
            &si.StartupInfo,
            &mut pi,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateProcessW returned these handles to us, each owned once.
    let (process, thread) = unsafe {
        (
            OwnedHandle::from_raw_handle(pi.hProcess),
            OwnedHandle::from_raw_handle(pi.hThread),
        )
    };
    drop(thread);
    Ok(process)
}

/// An initialized one-entry `PROC_THREAD_ATTRIBUTE_LIST` holding a pseudo
/// console, deleted on drop.
struct AttributeList(Box<[u64]>);

impl AttributeList {
    fn as_mut_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.0.as_mut_ptr().cast()
    }

    fn with_pseudo_console(hpc: HPCON) -> io::Result<Self> {
        let mut size = 0usize;
        // SAFETY: a null list asks for the required size; this call reports
        // ERROR_INSUFFICIENT_BUFFER by design.
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) };
        // u64 storage keeps the opaque list pointer-aligned.
        let mut buf = vec![0u64; size.div_ceil(8)].into_boxed_slice();
        let list: LPPROC_THREAD_ATTRIBUTE_LIST = buf.as_mut_ptr().cast();
        // SAFETY: `list` points to `size` writable bytes.
        if unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let attrs = Self(buf);
        // SAFETY: `list` is initialized; the pseudo console attribute takes the
        // HPCON value itself (not a pointer to it) with its size.
        let ok = unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                hpc as *const c_void,
                std::mem::size_of::<HPCON>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(attrs)
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        let list = self.as_mut_ptr();
        // SAFETY: the list was initialized by InitializeProcThreadAttributeList.
        unsafe { DeleteProcThreadAttributeList(list) };
    }
}

/// An anonymous pipe: (read end, write end), neither inheritable.
fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut read: HANDLE = std::ptr::null_mut();
    let mut write: HANDLE = std::ptr::null_mut();
    // SAFETY: valid out-pointers; null attributes make the handles
    // non-inheritable; 0 selects the default buffer size.
    if unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreatePipe returned two open handles, each owned once.
    Ok(unsafe {
        (
            OwnedHandle::from_raw_handle(read),
            OwnedHandle::from_raw_handle(write),
        )
    })
}

/// Append `arg` to a command line, quoted so `CommandLineToArgvW` and the MSVC
/// runtime split it back into exactly `arg`.
fn push_quoted_arg(cmdline: &mut String, arg: &str) {
    if !cmdline.is_empty() {
        cmdline.push(' ');
    }
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        cmdline.push_str(arg);
        return;
    }
    cmdline.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
            continue;
        }
        // Backslashes are literal unless they precede a quote.
        let escaped = if c == '"' {
            backslashes * 2 + 1
        } else {
            backslashes
        };
        cmdline.extend(std::iter::repeat_n('\\', escaped));
        cmdline.push(c);
        backslashes = 0;
    }
    cmdline.extend(std::iter::repeat_n('\\', backslashes * 2));
    cmdline.push('"');
}

/// The text of conhost's output: CSI, OSC and other escape sequences and BEL
/// removed, everything else (including CR and LF) kept.
#[allow(dead_code)]
pub fn strip_escapes(raw: &[u8]) -> String {
    let raw = String::from_utf8_lossy(raw);
    let mut text = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // CSI: parameter and intermediate bytes, then one final byte.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC, DCS, SOS, PM, APC: a string ended by BEL or ST (ESC \).
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                // nF (e.g. charset designation): intermediates, then a final byte.
                Some('\x20'..='\x2f') => {
                    for c in chars.by_ref() {
                        if ('\x30'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                }
                // Two-byte sequences (ESC 7, ESC =, ...), or a trailing ESC.
                Some(_) | None => {}
            },
            '\x07' => {}
            c => text.push(c),
        }
    }
    text
}

/// One line printed by `conmode` (src/bin/conmode.rs).
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConmodeReport {
    pub tag: String,
    /// `GetConsoleMode(CONIN$)`, as `0x....` or `err<code>`.
    pub conin: String,
    /// `GetConsoleMode(CONOUT$)`, as `0x....` or `err<code>`.
    pub conout: String,
    /// `GetConsoleOutputCP()`.
    pub cp: String,
    /// The console window, `<cols>x<rows>`.
    pub window: String,
}

#[allow(dead_code)]
impl ConmodeReport {
    /// Every complete report in `text`, in drawn order. A repaint draws old
    /// reports again, so match on a unique tag rather than position.
    pub fn parse_all(text: &str) -> Vec<Self> {
        text.split("conmode;")
            .skip(1)
            .filter_map(|segment| Self::parse(segment.split_once(";end")?.0))
            .collect()
    }

    /// The last complete report tagged `tag` in `text`.
    pub fn find(text: &str, tag: &str) -> Option<Self> {
        Self::parse_all(text).into_iter().rfind(|r| r.tag == tag)
    }

    fn parse(fields: &str) -> Option<Self> {
        let mut report = Self {
            tag: String::new(),
            conin: String::new(),
            conout: String::new(),
            cp: String::new(),
            window: String::new(),
        };
        for field in fields.split(';') {
            let (key, value) = field.split_once('=')?;
            let slot = match key {
                "tag" => &mut report.tag,
                "in" => &mut report.conin,
                "out" => &mut report.conout,
                "cp" => &mut report.cp,
                "window" => &mut report.window,
                _ => return None,
            };
            *slot = value.to_owned();
        }
        Some(report)
    }

    /// A console mode field as a number, if it is not an error.
    pub fn mode(field: &str) -> Option<u32> {
        u32::from_str_radix(field.strip_prefix("0x")?, 16).ok()
    }
}
