//! Test fixture: reports the console state it sees, from inside a (pseudo)
//! console.
//!
//! ```text
//! conmode            print one report tagged `once`, then exit
//! conmode --serve    print a report tagged `start`, then one per line read
//!                    from stdin, tagged with that line; exit 0 on `quit` or EOF
//! ```
//!
//! A report is one line with no spaces, because ConPTY renders what conhost
//! drew and may replace runs of spaces with cursor movement:
//!
//! ```text
//! conmode;tag=<tag>;in=0x<mode>;out=0x<mode>;cp=<output code page>;window=<cols>x<rows>;end
//! ```
//!
//! The modes come from `GetConsoleMode` on `CONIN$` and `CONOUT$`, opened by
//! name rather than through stdio, so the report describes the console even
//! when stdio is redirected. A query that fails prints `err<code>` in place of
//! its value. Used by the ConPTY harness (tests/harness/conpty.rs).

#[cfg(windows)]
fn main() {
    use std::io::{BufRead, Write};

    let serve = match std::env::args().nth(1).as_deref() {
        None => false,
        Some("--serve") => true,
        Some(other) => {
            eprintln!("conmode: unknown argument {other:?}; usage: conmode [--serve]");
            std::process::exit(2);
        }
    };

    let mut out = std::io::stdout().lock();
    let mut emit = |tag: &str| {
        let line = report(tag);
        // One write per report, flushed, so the line is drawn whole.
        let _ = out.write_all(line.as_bytes());
        let _ = out.flush();
    };

    if !serve {
        emit("once");
        return;
    }
    emit("start");
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let tag = line.trim();
        if tag == "quit" {
            break;
        }
        emit(tag);
    }
}

/// One report line, newline-terminated.
#[cfg(windows)]
fn report(tag: &str) -> String {
    use windows_sys::Win32::System::Console::{
        CONSOLE_SCREEN_BUFFER_INFO, GetConsoleMode, GetConsoleOutputCP, GetConsoleScreenBufferInfo,
    };

    let conin = Console::open("CONIN$");
    let conout = Console::open("CONOUT$");

    let mode = |console: &Result<Console, u32>| -> String {
        let handle = match console {
            Ok(console) => console.0,
            Err(code) => return format!("err{code}"),
        };
        let mut mode = 0u32;
        // SAFETY: `handle` is an open console handle owned by `Console`;
        // `mode` is a valid out-pointer.
        if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
            return format!("err{}", last_error());
        }
        format!("0x{mode:04x}")
    };

    // SAFETY: no arguments; reads the calling process's console state.
    let cp = match unsafe { GetConsoleOutputCP() } {
        0 => format!("err{}", last_error()),
        cp => cp.to_string(),
    };

    let window = match &conout {
        Err(code) => format!("err{code}"),
        Ok(console) => {
            // SAFETY: zeroed is a valid CONSOLE_SCREEN_BUFFER_INFO (plain integers).
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
            // SAFETY: `console.0` is an open CONOUT$ handle; `info` is a valid out-pointer.
            if unsafe { GetConsoleScreenBufferInfo(console.0, &mut info) } == 0 {
                format!("err{}", last_error())
            } else {
                let w = info.srWindow;
                format!(
                    "{}x{}",
                    i32::from(w.Right) - i32::from(w.Left) + 1,
                    i32::from(w.Bottom) - i32::from(w.Top) + 1
                )
            }
        }
    };

    format!(
        "conmode;tag={tag};in={};out={};cp={cp};window={window};end\n",
        mode(&conin),
        mode(&conout),
    )
}

/// A console handle opened by name, closed on drop.
#[cfg(windows)]
struct Console(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Console {
    /// Open `CONIN$` or `CONOUT$`; the error is the Win32 error code.
    fn open(name: &str) -> Result<Self, u32> {
        use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };

        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the
        // call; null security attributes and template are permitted.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(last_error());
        }
        Ok(Self(handle))
    }
}

#[cfg(windows)]
impl Drop for Console {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by `Console::open` and is closed once.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

#[cfg(windows)]
fn last_error() -> u32 {
    // SAFETY: no arguments; reads the calling thread's last-error value.
    unsafe { windows_sys::Win32::Foundation::GetLastError() }
}

#[cfg(not(windows))]
fn main() {
    // Stub for non-Windows builds.
}
