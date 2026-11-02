//! The user's Windows console: raw input and virtual terminal output through
//! the console API, the keyboard read as input records, the size from the
//! visible window, and the console control events.
//!
//! The console is shared with ssh, which may change its modes and code page
//! (a password prompt, or an ssh stopped mid-prompt), so its state is taken
//! before ssh starts ([`WindowsTerminal::open`]) and put back on every way
//! out, whether or not the attach got as far as raw mode.
//!
//! - Input: `ENABLE_VIRTUAL_TERMINAL_INPUT` makes conhost encode keys, and
//!   mouse events once the session asks for them, as the byte sequences a
//!   Unix terminal sends. Line input, echo, processed input (so Ctrl-C is the
//!   byte 0x03 for the session, not a control event) and QuickEdit (which
//!   would take the mouse) are off. The records are read with
//!   `ReadConsoleInputW`, their UTF-16 turned into UTF-8.
//! - Output: `ENABLE_VIRTUAL_TERMINAL_PROCESSING`, and the bytes written as
//!   they are with `WriteFile` under the UTF-8 code page. Rust's `Stdout`
//!   refuses bytes that are not UTF-8 when it writes to a console, and the
//!   session's output need not be.
//! - A console that cannot take these modes (before Windows 10 1809, or with
//!   "Use legacy console" set) is refused before anything starts.
//! - `CTRL_C_EVENT` and `CTRL_BREAK_EVENT` (raised by another program, since
//!   the keyboard's Ctrl-C is a byte) leave ([`Input::Leave`]); so does
//!   `CTRL_CLOSE_EVENT`, whose handler waits for the console to be put back
//!   before Windows ends the process. `WINDOW_BUFFER_SIZE_EVENT` brings the
//!   next size check forward ([`Input::SizeDue`]).

use std::io;
use std::os::windows::io::AsRawHandle;
use std::sync::Arc;
use std::time::Duration;

use tendr::recording::Geometry;
use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::Storage::FileSystem::WriteFile;
use windows_sys::Win32::System::Console::{
    CONSOLE_SCREEN_BUFFER_INFO, DISABLE_NEWLINE_AUTO_RETURN, ENABLE_ECHO_INPUT,
    ENABLE_EXTENDED_FLAGS, ENABLE_LINE_INPUT, ENABLE_MOUSE_INPUT, ENABLE_PROCESSED_INPUT,
    ENABLE_QUICK_EDIT_MODE, ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
    ENABLE_WINDOW_INPUT, FlushConsoleInputBuffer, GetConsoleMode, GetConsoleOutputCP,
    GetConsoleScreenBufferInfo, GetNumberOfConsoleInputEvents, INPUT_RECORD, KEY_EVENT,
    ReadConsoleInputW, SetConsoleMode, SetConsoleOutputCP, WINDOW_BUFFER_SIZE_EVENT,
};
use windows_sys::Win32::System::Threading::WaitForMultipleObjects;

use super::{Input, Output, Raw, Terminal};

/// The UTF-8 code page.
const CP_UTF8: u32 = 65001;

/// What a console that cannot take virtual terminal modes is told.
const NO_VIRTUAL_TERMINAL: &str = "attach needs a console that understands virtual terminal \
     sequences: Windows 10 1809 or later, in Windows Terminal or a conhost window, with the \
     console's \"Use legacy console\" setting off";

/// What stdin that is a terminal but not a console (mintty's pipes) is told.
const NOT_A_CONSOLE: &str = "attach needs a Windows console on stdin: run it in Windows \
     Terminal or a conhost window (mintty, Git Bash's terminal, gives programs pipes, not a \
     console)";

/// The console's state before the attach: taken before ssh starts, and put
/// back when dropped.
pub(super) struct WindowsTerminal {
    snapshot: Snapshot,
}

impl WindowsTerminal {
    /// Take the console's state, and refuse a console without virtual
    /// terminal modes. Stdout may be redirected, as on Unix: then only the
    /// input is a console.
    pub(super) fn open() -> anyhow::Result<Self> {
        let snapshot = Snapshot::take()?;
        let mut probes = vec![Probe {
            console: Console::Input,
            current: snapshot.input,
            wanted: raw_input_mode(snapshot.input),
        }];
        if let Some(output) = snapshot.output {
            probes.push(Probe {
                console: Console::Output,
                current: output,
                wanted: vt_output_mode(output),
            });
        }
        probe_vt(&probes, |console, mode| set_mode(console.handle(), mode))?;
        Ok(Self { snapshot })
    }
}

impl Terminal for WindowsTerminal {
    fn enter(self: Box<Self>) -> anyhow::Result<Box<dyn Raw>> {
        // Installed before raw mode, so no control event can arrive between
        // the two and leave the console raw; released after the console is
        // put back (see `WindowsRaw`'s drop). On a failure below, the
        // snapshot is dropped first and puts the console back.
        let handler = ctrl::Handler::install()
            .map_err(|e| anyhow::anyhow!("failed to install the console control handler: {e}"))?;
        let Self { snapshot } = *self;
        if !set_mode(stdin(), raw_input_mode(snapshot.input)) {
            anyhow::bail!(NO_VIRTUAL_TERMINAL);
        }
        if let Some(output) = snapshot.output {
            if !set_mode(stdout(), vt_output_mode(output)) {
                anyhow::bail!(NO_VIRTUAL_TERMINAL);
            }
            // SAFETY: plain FFI call on this process's console. A console
            // that refuses keeps its code page: text in it still shows.
            unsafe { SetConsoleOutputCP(CP_UTF8) };
        }
        Ok(Box::new(WindowsRaw {
            snapshot: Some(snapshot),
            handler,
            decoder: Decoder::default(),
            typed: Vec::new(),
            size_due: false,
        }))
    }
}

/// The console's modes and output code page.
struct Snapshot {
    input: u32,
    /// Stdout's mode, if stdout is a console.
    output: Option<u32>,
    /// `None` if it could not be read.
    code_page: Option<u32>,
}

impl Snapshot {
    fn take() -> anyhow::Result<Self> {
        let input = console_mode(stdin()).ok_or_else(|| anyhow::anyhow!(NOT_A_CONSOLE))?;
        // SAFETY: plain FFI call; 0 means it failed.
        let code_page = match unsafe { GetConsoleOutputCP() } {
            0 => None,
            cp => Some(cp),
        };
        Ok(Self {
            input,
            output: console_mode(stdout()),
            code_page,
        })
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        set_mode(stdin(), self.input);
        if let Some(output) = self.output {
            set_mode(stdout(), output);
        }
        if let Some(cp) = self.code_page {
            // SAFETY: plain FFI call on this process's console.
            unsafe { SetConsoleOutputCP(cp) };
        }
    }
}

/// The console in raw mode. Dropping it puts the console back, discards
/// typed-ahead input, then gives back the control events.
struct WindowsRaw {
    /// `Some` until dropped.
    snapshot: Option<Snapshot>,
    /// Dropped after `Drop::drop` has put the console back.
    handler: ctrl::Handler,
    decoder: Decoder,
    /// Read from the console but not yet handed out.
    typed: Vec<u8>,
    /// A size change came with typing, which went first.
    size_due: bool,
}

impl Raw for WindowsRaw {
    fn input(&mut self, buf: &mut [u8], timeout: Duration) -> Input {
        if let Some(leave) = self.handler.leave() {
            return Input::Leave(leave);
        }
        if self.typed.is_empty() {
            if std::mem::take(&mut self.size_due) {
                return Input::SizeDue;
            }
            match wait_ready(self.handler.wake(), timeout) {
                Ready::Keyboard => match self.read() {
                    Ok(resized) => self.size_due |= resized,
                    Err(_) => return Input::Closed,
                },
                Ready::Wake => return self.handler.leave().map_or(Input::Wake, Input::Leave),
                Ready::Timeout => return Input::Wake,
                Ready::Failed => return Input::Closed,
            }
            // A batch of records need not hold typing: key releases, focus
            // and menu events, a size change.
            if self.typed.is_empty() {
                return if std::mem::take(&mut self.size_due) {
                    Input::SizeDue
                } else {
                    Input::Wake
                };
            }
        }
        let n = self.typed.len().min(buf.len());
        buf[..n].copy_from_slice(&self.typed[..n]);
        self.typed.drain(..n);
        Input::Bytes(n)
    }

    fn output(&self) -> Arc<dyn Output> {
        Arc::new(WindowsOutput)
    }
}

impl WindowsRaw {
    /// Read the waiting input records into `typed`: whether one was a size
    /// change.
    fn read(&mut self) -> io::Result<bool> {
        let mut waiting = 0u32;
        // SAFETY: `stdin()` is the console input; `waiting` is a valid
        // out-pointer.
        if unsafe { GetNumberOfConsoleInputEvents(stdin(), &mut waiting) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if waiting == 0 {
            return Ok(false);
        }
        // SAFETY: INPUT_RECORD is plain old data (integers and a union of
        // them), so zeroes are a valid value.
        let mut records: [INPUT_RECORD; 64] = unsafe { std::mem::zeroed() };
        let mut read = 0u32;
        // SAFETY: `records` has room for the length passed; `read` is a
        // valid out-pointer. Records are waiting, so this does not block.
        let ok = unsafe {
            ReadConsoleInputW(
                stdin(),
                records.as_mut_ptr(),
                records.len() as u32,
                &mut read,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut resized = false;
        for record in &records[..read as usize] {
            match u32::from(record.EventType) {
                KEY_EVENT => {
                    // SAFETY: the event type says the union holds a key event.
                    let key = unsafe { record.Event.KeyEvent };
                    self.decoder.key(
                        Key {
                            down: key.bKeyDown != 0,
                            virtual_key: key.wVirtualKeyCode,
                            // SAFETY: both union members are integers;
                            // ReadConsoleInputW fills the UTF-16 one.
                            unit: unsafe { key.uChar.UnicodeChar },
                            repeat: key.wRepeatCount,
                        },
                        &mut self.typed,
                    );
                }
                WINDOW_BUFFER_SIZE_EVENT => resized = true,
                // Mouse events reach the session as key sequences under
                // virtual terminal input; focus and menu events are conhost's.
                _ => {}
            }
        }
        Ok(resized)
    }
}

impl Drop for WindowsRaw {
    fn drop(&mut self) {
        // Put the console back at once, then discard unread typed-ahead
        // input, which the shell would otherwise read. The handler is
        // released after this, when a close event may stop waiting.
        drop(self.snapshot.take());
        // SAFETY: `stdin()` is the console input.
        unsafe { FlushConsoleInputBuffer(stdin()) };
    }
}

/// The console's screen: stdout.
struct WindowsOutput;

impl Output for WindowsOutput {
    fn write(&self, bytes: &[u8]) -> io::Result<()> {
        let handle = stdout();
        let mut rest = bytes;
        while !rest.is_empty() {
            let len = u32::try_from(rest.len()).unwrap_or(u32::MAX);
            let mut written = 0u32;
            // SAFETY: `rest` holds at least `len` bytes; `written` is a valid
            // out-pointer; the handle is not overlapped.
            let ok = unsafe {
                WriteFile(
                    handle,
                    rest.as_ptr(),
                    len,
                    &mut written,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            if written == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            rest = &rest[written as usize..];
        }
        Ok(())
    }

    fn size(&self) -> Option<Geometry> {
        // SAFETY: CONSOLE_SCREEN_BUFFER_INFO is plain old data.
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a valid out-pointer; a handle that is not a
        // console fails the call.
        if unsafe { GetConsoleScreenBufferInfo(stdout(), &mut info) } == 0 {
            return None;
        }
        // The visible window, not the scrollback buffer behind it.
        let window = info.srWindow;
        let cols = i32::from(window.Right) - i32::from(window.Left) + 1;
        let rows = i32::from(window.Bottom) - i32::from(window.Top) + 1;
        Geometry::new(u16::try_from(rows).ok()?, u16::try_from(cols).ok()?)
    }

    fn is_terminal(&self) -> bool {
        true
    }
}

/// Input mode for the attach: the console's own, with virtual terminal
/// input, window and mouse events, and without line input, echo, processed
/// input or QuickEdit (clearing which needs the extended flags).
fn raw_input_mode(current: u32) -> u32 {
    (current
        | ENABLE_VIRTUAL_TERMINAL_INPUT
        | ENABLE_WINDOW_INPUT
        | ENABLE_MOUSE_INPUT
        | ENABLE_EXTENDED_FLAGS)
        & !(ENABLE_QUICK_EDIT_MODE | ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT)
}

/// Output mode for the attach: the console's own, with virtual terminal
/// processing, and a line feed that does not also return the carriage, as
/// on a Unix terminal.
fn vt_output_mode(current: u32) -> u32 {
    current | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN
}

/// Which side of the console.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Console {
    Input,
    Output,
}

impl Console {
    fn handle(self) -> HANDLE {
        match self {
            Self::Input => stdin(),
            Self::Output => stdout(),
        }
    }
}

/// A mode the attach needs, and the one to put back.
#[derive(Debug, Clone, Copy)]
struct Probe {
    console: Console,
    current: u32,
    wanted: u32,
}

/// Check that the console takes each wanted mode, by setting it and putting
/// the current one back: a console without virtual terminal support refuses
/// them. Nothing has started yet, so the user is told before ssh runs.
fn probe_vt(probes: &[Probe], mut set: impl FnMut(Console, u32) -> bool) -> anyhow::Result<()> {
    for probe in probes {
        if !set(probe.console, probe.wanted) {
            anyhow::bail!(NO_VIRTUAL_TERMINAL);
        }
        set(probe.console, probe.current);
    }
    Ok(())
}

fn stdin() -> HANDLE {
    io::stdin().as_raw_handle()
}

fn stdout() -> HANDLE {
    io::stdout().as_raw_handle()
}

/// The console mode of `handle`; `None` if it is not a console.
fn console_mode(handle: HANDLE) -> Option<u32> {
    let mut mode = 0u32;
    // SAFETY: `mode` is a valid out-pointer; a handle that is not a console
    // fails the call.
    (unsafe { GetConsoleMode(handle, &mut mode) } != 0).then_some(mode)
}

fn set_mode(handle: HANDLE, mode: u32) -> bool {
    // SAFETY: plain FFI call; a handle that is not a console fails it.
    unsafe { SetConsoleMode(handle, mode) != 0 }
}

/// What woke the keyboard's wait.
enum Ready {
    /// Input records are waiting.
    Keyboard,
    /// The control handler's wake-up.
    Wake,
    Timeout,
    Failed,
}

/// Wait up to `timeout` for input records or for the control handler.
fn wait_ready(wake: HANDLE, timeout: Duration) -> Ready {
    let handles = [stdin(), wake];
    // Rounded up, so a short wait is not a busy one; below INFINITE.
    let ms = u32::try_from(timeout.as_micros().div_ceil(1000))
        .unwrap_or(u32::MAX - 1)
        .min(u32::MAX - 1);
    // SAFETY: both handles are open and waitable for the call.
    match unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, ms) } {
        WAIT_OBJECT_0 => Ready::Keyboard,
        r if r == WAIT_OBJECT_0 + 1 => Ready::Wake,
        WAIT_TIMEOUT => Ready::Timeout,
        _ => Ready::Failed,
    }
}

/// The parts of a key event that make typing.
#[derive(Debug, Clone, Copy)]
struct Key {
    down: bool,
    virtual_key: u16,
    /// One UTF-16 code unit; 0 for a key with no character (Shift, say).
    unit: u16,
    repeat: u16,
}

/// The Alt key's virtual-key code.
const VK_MENU: u16 = 0x12;

/// UTF-16 code units from key events → UTF-8. A character outside the
/// Basic Multilingual Plane comes as two events, or two reads, one code unit
/// each; a unit without its partner becomes U+FFFD.
#[derive(Debug, Default)]
struct Decoder {
    /// A high surrogate waiting for its low one.
    high: Option<u16>,
}

impl Decoder {
    /// Append what `key` types to `out`. A key press types its character
    /// `repeat` times; a release types nothing, except Alt's after an
    /// Alt-numpad entry, which carries the character entered.
    fn key(&mut self, key: Key, out: &mut Vec<u8>) {
        let types = key.down || key.virtual_key == VK_MENU;
        if !types || key.unit == 0 {
            return;
        }
        for _ in 0..key.repeat.max(1) {
            self.unit(key.unit, out);
        }
    }

    fn unit(&mut self, unit: u16, out: &mut Vec<u8>) {
        let mut push = |c: char| out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
        match unit {
            0xD800..=0xDBFF => {
                if self.high.replace(unit).is_some() {
                    push(char::REPLACEMENT_CHARACTER);
                }
            }
            0xDC00..=0xDFFF => match self.high.take() {
                Some(high) => {
                    let code =
                        0x10000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(unit) - 0xDC00);
                    push(char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER));
                }
                None => push(char::REPLACEMENT_CHARACTER),
            },
            _ => {
                if self.high.take().is_some() {
                    push(char::REPLACEMENT_CHARACTER);
                }
                push(char::from_u32(u32::from(unit)).unwrap_or(char::REPLACEMENT_CHARACTER));
            }
        }
    }
}

/// The attach client's console control handling: the handler records which
/// event arrived and sets an event the keyboard's wait watches, so the main
/// thread leaves whichever thread Windows ran the handler on.
mod ctrl {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Condvar, LazyLock, Mutex, PoisonError};
    use std::time::Duration;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, SetConsoleCtrlHandler,
    };
    use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent};

    use super::super::Leave;

    /// No event yet.
    const NONE: u32 = u32::MAX;

    /// How long a close event's handler waits for the console to be put
    /// back. Windows ends the process 5 s after the event; leaving takes at
    /// most about 2.4 s (the detach's grace, a reconnect's ssh stopping, the
    /// report's grace).
    const CLOSE_WAIT: Duration = Duration::from_millis(4500);

    /// The first control event since the handler was installed.
    static RECEIVED: AtomicU32 = AtomicU32::new(NONE);

    /// Whether the console has been put back, for a close event to wait on.
    static RESTORED: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());

    /// The wake-up the keyboard's wait watches: an auto-reset event, created
    /// once and never closed, so a handler still running on another thread
    /// after the handler is removed cannot set a reused handle.
    fn wake_event() -> std::io::Result<HANDLE> {
        static WAKE: LazyLock<Result<usize, i32>> = LazyLock::new(|| {
            // SAFETY: default security, auto-reset, initially unset, no name.
            let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
            if event.is_null() {
                Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
            } else {
                Ok(event as usize)
            }
        });
        match *WAKE {
            Ok(event) => Ok(event as HANDLE),
            Err(code) => Err(std::io::Error::from_raw_os_error(code)),
        }
    }

    fn name(event: u32) -> &'static str {
        match event {
            CTRL_C_EVENT => "CTRL_C_EVENT",
            CTRL_BREAK_EVENT => "CTRL_BREAK_EVENT",
            _ => "CTRL_CLOSE_EVENT",
        }
    }

    /// Ctrl-C and Ctrl-Break raised by another program, and the console
    /// window closing, leave. Logoff and shutdown reach services only.
    unsafe extern "system" fn on_event(event: u32) -> i32 {
        if !matches!(event, CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT) {
            return 0;
        }
        let _ = RECEIVED.compare_exchange(NONE, event, Ordering::SeqCst, Ordering::SeqCst);
        if let Ok(wake) = wake_event() {
            // SAFETY: `wake` is the process's wake event, never closed.
            unsafe { SetEvent(wake) };
        }
        if event == CTRL_CLOSE_EVENT {
            // Returning lets Windows end the process: first wait for the
            // main thread to put the console back.
            let (restored, changed) = &RESTORED;
            let guard = restored.lock().unwrap_or_else(PoisonError::into_inner);
            let _ = changed
                .wait_timeout_while(guard, CLOSE_WAIT, |restored| !*restored)
                .unwrap_or_else(PoisonError::into_inner);
        }
        1
    }

    /// The handler, installed for the relay; removed on drop, after the
    /// console has been put back.
    pub(super) struct Handler {
        wake: HANDLE,
    }

    impl Handler {
        pub(super) fn install() -> std::io::Result<Self> {
            let wake = wake_event()?;
            // Nothing from an earlier relay in this process carries over.
            RECEIVED.store(NONE, Ordering::SeqCst);
            *RESTORED.0.lock().unwrap_or_else(PoisonError::into_inner) = false;
            // SAFETY: `on_event` has the handler signature; TRUE adds it.
            if unsafe { SetConsoleCtrlHandler(Some(on_event), 1) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self { wake })
        }

        /// The event the keyboard's wait watches.
        pub(super) fn wake(&self) -> HANDLE {
            self.wake
        }

        /// The control event that ends the attach, once one has arrived.
        pub(super) fn leave(&self) -> Option<Leave> {
            match RECEIVED.load(Ordering::SeqCst) {
                NONE => None,
                event => Some(Leave(name(event))),
            }
        }
    }

    impl Drop for Handler {
        fn drop(&mut self) {
            let (restored, changed) = &RESTORED;
            *restored.lock().unwrap_or_else(PoisonError::into_inner) = true;
            changed.notify_all();
            // SAFETY: removes the handler `install` added.
            unsafe { SetConsoleCtrlHandler(Some(on_event), 0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(decoder: &mut Decoder, keys: &[Key]) -> Vec<u8> {
        let mut out = Vec::new();
        for &key in keys {
            decoder.key(key, &mut out);
        }
        out
    }

    fn press(unit: u16) -> Key {
        Key {
            down: true,
            virtual_key: 0x41,
            unit,
            repeat: 1,
        }
    }

    #[test]
    fn a_character_outside_the_bmp_is_joined_across_events_and_reads() {
        // U+1F600 is D83D DE00.
        let mut decoder = Decoder::default();
        let mut out = typed(&mut decoder, &[press(0xD83D)]);
        assert!(out.is_empty(), "the high half waits for its partner");
        out.extend(typed(
            &mut decoder,
            &[press(0xDE00), press(u16::from(b'x'))],
        ));
        assert_eq!(out, "\u{1F600}x".as_bytes());
    }

    #[test]
    fn a_surrogate_without_its_partner_is_a_replacement_character() {
        let mut decoder = Decoder::default();
        let out = typed(
            &mut decoder,
            &[
                press(0xDE00),
                press(0xD83D),
                press(u16::from(b'a')),
                press(0xD83D),
                press(0xD83D),
                press(0xDE00),
            ],
        );
        assert_eq!(out, "\u{FFFD}\u{FFFD}a\u{FFFD}\u{1F600}".as_bytes());
    }

    #[test]
    fn a_held_key_types_its_repeat_count() {
        let mut decoder = Decoder::default();
        let key = Key {
            repeat: 3,
            ..press(u16::from(b'z'))
        };
        assert_eq!(typed(&mut decoder, &[key]), b"zzz");
        let euro = Key {
            repeat: 2,
            ..press(0x20AC)
        };
        assert_eq!(typed(&mut decoder, &[euro]), "€€".as_bytes());
    }

    #[test]
    fn releases_type_nothing_but_alt_numpad() {
        let mut decoder = Decoder::default();
        let release = Key {
            down: false,
            ..press(u16::from(b'a'))
        };
        let shift = Key {
            virtual_key: 0x10,
            ..press(0)
        };
        assert!(typed(&mut decoder, &[release, shift]).is_empty());
        // Alt-numpad 0233: the character comes with Alt's release.
        let alt_release = Key {
            down: false,
            virtual_key: VK_MENU,
            unit: 0xE9,
            repeat: 1,
        };
        let plain_alt_release = Key {
            unit: 0,
            ..alt_release
        };
        assert_eq!(
            typed(&mut decoder, &[alt_release, plain_alt_release]),
            "é".as_bytes()
        );
    }

    #[test]
    fn ctrl_c_is_a_byte_for_the_session() {
        // With processed input off, conhost delivers Ctrl-C as a key event.
        let mut decoder = Decoder::default();
        assert_eq!(typed(&mut decoder, &[press(0x03)]), [0x03]);
        assert_eq!(
            raw_input_mode(ENABLE_PROCESSED_INPUT) & ENABLE_PROCESSED_INPUT,
            0
        );
    }

    #[test]
    fn raw_input_takes_vt_window_and_mouse_input_and_drops_cooking_and_quick_edit() {
        // A fresh conhost's default input mode.
        let cooked = 0x1f7;
        let raw = raw_input_mode(cooked);
        for on in [
            ENABLE_VIRTUAL_TERMINAL_INPUT,
            ENABLE_WINDOW_INPUT,
            ENABLE_MOUSE_INPUT,
            ENABLE_EXTENDED_FLAGS,
        ] {
            assert_eq!(raw & on, on, "{on:#x} in {raw:#x}");
        }
        for off in [
            ENABLE_QUICK_EDIT_MODE,
            ENABLE_LINE_INPUT,
            ENABLE_ECHO_INPUT,
            ENABLE_PROCESSED_INPUT,
        ] {
            assert_eq!(raw & off, 0, "{off:#x} in {raw:#x}");
        }
        let out = vt_output_mode(0x3);
        assert_eq!(out & 0x3, 0x3, "keeps processed output and wrapping");
        assert_ne!(out & ENABLE_VIRTUAL_TERMINAL_PROCESSING, 0);
        assert_ne!(out & DISABLE_NEWLINE_AUTO_RETURN, 0);
    }

    /// A console that refuses virtual terminal modes, as one before Windows
    /// 10 1809 or with "Use legacy console" set does.
    struct LegacyConsole {
        input: u32,
        output: u32,
    }

    impl LegacyConsole {
        fn set(&mut self, console: Console, mode: u32) -> bool {
            let vt = match console {
                Console::Input => ENABLE_VIRTUAL_TERMINAL_INPUT,
                Console::Output => ENABLE_VIRTUAL_TERMINAL_PROCESSING,
            };
            if mode & vt != 0 {
                return false;
            }
            match console {
                Console::Input => self.input = mode,
                Console::Output => self.output = mode,
            }
            true
        }
    }

    fn probes(input: u32, output: u32) -> [Probe; 2] {
        [
            Probe {
                console: Console::Input,
                current: input,
                wanted: raw_input_mode(input),
            },
            Probe {
                console: Console::Output,
                current: output,
                wanted: vt_output_mode(output),
            },
        ]
    }

    #[test]
    fn a_console_without_virtual_terminal_modes_is_refused_and_left_as_it_was() {
        let mut console = LegacyConsole {
            input: 0x1f7,
            output: 0x3,
        };
        let refused = probe_vt(&probes(0x1f7, 0x3), |c, mode| console.set(c, mode));
        assert!(refused.is_err());
        assert_eq!((console.input, console.output), (0x1f7, 0x3));
    }

    #[test]
    fn a_console_with_virtual_terminal_modes_passes_the_probe_unchanged() {
        let (mut input, mut output) = (0x1f7, 0x3);
        let mut calls = 0;
        let probed = probe_vt(&probes(0x1f7, 0x3), |c, mode| {
            calls += 1;
            match c {
                Console::Input => input = mode,
                Console::Output => output = mode,
            }
            true
        });
        assert!(probed.is_ok());
        assert_eq!(calls, 4, "each mode set, then put back");
        assert_eq!((input, output), (0x1f7, 0x3));
    }
}
