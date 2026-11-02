//! Test fixture: reports the console state it sees, from inside a (pseudo)
//! console.
//!
//! ```text
//! conmode            print one report tagged `once`, then exit
//! conmode --serve    print a report tagged `start`, then one per line read
//!                    from stdin, tagged with that line; exit 0 on `quit` or EOF
//! conmode --run [--env <name>=<value>]... [--break-on <path>] -- <program> [<arg>...]
//!                    print a report tagged `before`, run <program> in this
//!                    console with the environment given, then print a report
//!                    tagged `after` and exit with its exit code. With
//!                    `--break-on`, raise CTRL_BREAK_EVENT in the console
//!                    once <path> exists, as a program sharing the console
//!                    would; conmode itself ignores it.
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

    let emit = |tag: &str| {
        let line = report(tag);
        // One write per report, flushed, so the line is drawn whole.
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(line.as_bytes());
        let _ = out.flush();
    };

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => emit("once"),
        Some("--serve") if args.len() == 1 => {
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
        Some("--run") => run(&args[1..], emit),
        Some(_) => {
            eprintln!("conmode: unknown arguments {args:?}; usage: see src/bin/conmode.rs");
            std::process::exit(2);
        }
    }
}

/// `conmode --run`: report, run the program, report again, exit with its
/// code.
#[cfg(windows)]
fn run(args: &[String], emit: impl Fn(&str)) -> ! {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler,
    };

    unsafe extern "system" fn ignore(_: u32) -> i32 {
        1
    }

    let usage = || -> ! {
        eprintln!(
            "conmode: usage: conmode --run [--env K=V]... [--break-on <path>] -- <program> [<arg>...]"
        );
        std::process::exit(2);
    };
    let mut command = None;
    let mut break_on = None;
    let mut env = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--env" => match rest.next().and_then(|kv| kv.split_once('=')) {
                Some((name, value)) => env.push((name.to_owned(), value.to_owned())),
                None => usage(),
            },
            "--break-on" => break_on = Some(rest.next().unwrap_or_else(|| usage()).clone()),
            "--" => {
                let program = rest.next().unwrap_or_else(|| usage());
                let mut cmd = std::process::Command::new(program);
                cmd.args(rest.by_ref())
                    .envs(env.iter().map(|(k, v)| (k, v)));
                command = Some(cmd);
            }
            _ => usage(),
        }
    }
    let Some(mut command) = command else { usage() };

    // A control event raised in the console reaches every program in it,
    // this one too: it outlives the program it runs, to report after it.
    // SAFETY: `ignore` has the handler signature; TRUE adds it.
    if unsafe { SetConsoleCtrlHandler(Some(ignore), 1) } == 0 {
        eprintln!(
            "conmode: SetConsoleCtrlHandler: {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(2);
    }
    emit("before");
    if let Some(path) = break_on {
        std::thread::spawn(move || {
            while !std::path::Path::new(&path).exists() {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            // SAFETY: plain FFI call; group 0 is every process in this console.
            if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, 0) } == 0 {
                eprintln!(
                    "conmode: GenerateConsoleCtrlEvent: {}",
                    std::io::Error::last_os_error()
                );
            }
        });
    }
    let code = match command.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("conmode: cannot run the program: {e}");
            2
        }
    };
    emit("after");
    std::process::exit(code);
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
