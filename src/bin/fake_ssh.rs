//! Test fixture: a portable stand-in for `ssh`, linked or copied into a temp
//! PATH dir as `ssh` (`ssh.exe` on Windows) by `harness::fake_ssh` in `tests/`.
//!
//! What it does is read from the file `fake-ssh-actions` beside it (the
//! `FAKE_SSH` environment variable, if set, takes precedence). It holds one
//! action per line, run in order:
//!
//! ```text
//! args [<path>]     print each argv entry as `ARG:<value>` on stdout, or
//!                   write those lines to <path>
//! out <text>        write <text> to stdout (`\n` is a newline, `\\` a backslash)
//! err <text>        write <text> to stderr (likewise)
//! stdin <path>      copy all of stdin to <path>
//! pidfile <path>    write this process's id to <path>, atomically
//! touch <path>      create an empty file at <path>
//! reject-hello      if `--hello` is in argv: clap's error on stderr, exit 2
//! leftover <secs> <exe>
//!                   start <exe> (this fixture's built binary) sleeping <secs>,
//!                   given our stderr and null stdin and stdout, as a
//!                   ProxyCommand that ignores the hangup is. On Windows it
//!                   also inherits every inheritable handle we hold, our
//!                   stdin and stdout among them, as the ProxyCommand of an
//!                   ssh without a handle allow-list does.
//! close-stderr      close our stderr
//! sleep <secs>      sleep
//! exit <code>       exit with <code>
//! forward           act as the remote login shell: see `forward`
//! ```
//!
//! Without an `exit` or `forward`, it exits 0 after the last action.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

const ENV: &str = "FAKE_SSH";

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let script = std::env::var(ENV).unwrap_or_else(|_| {
        let actions = std::env::current_exe()
            .expect("fake ssh: current_exe")
            .with_file_name("fake-ssh-actions");
        std::fs::read_to_string(&actions).unwrap_or_else(|e| {
            panic!(
                "fake ssh: no FAKE_SSH and cannot read {}: {e}",
                actions.display()
            )
        })
    });
    for line in script.lines().filter(|l| !l.trim().is_empty()) {
        let (action, value) = line.split_once(' ').unwrap_or((line, ""));
        match action {
            "args" => {
                let lines: String = argv.iter().map(|arg| format!("ARG:{arg}\n")).collect();
                if value.is_empty() {
                    let mut out = std::io::stdout().lock();
                    out.write_all(lines.as_bytes()).unwrap();
                    out.flush().unwrap();
                } else {
                    std::fs::write(value, lines).unwrap();
                }
            }
            "out" => {
                let mut out = std::io::stdout().lock();
                out.write_all(unescape(value).as_bytes()).unwrap();
                out.flush().unwrap();
            }
            "err" => {
                let mut err = std::io::stderr().lock();
                err.write_all(unescape(value).as_bytes()).unwrap();
                err.flush().unwrap();
            }
            "stdin" => {
                let mut file = std::fs::File::create(value).unwrap();
                std::io::copy(&mut std::io::stdin().lock(), &mut file).unwrap();
            }
            "pidfile" => {
                // Written then renamed, so a reader never sees a partial pid.
                let tmp = format!("{value}.tmp");
                std::fs::write(&tmp, std::process::id().to_string()).unwrap();
                std::fs::rename(&tmp, value).unwrap();
            }
            "touch" => std::fs::write(value, "").unwrap(),
            "reject-hello" => {
                if argv.iter().any(|a| a == "--hello") {
                    eprintln!("error: unexpected argument '--hello' found");
                    std::process::exit(2);
                }
            }
            "leftover" => {
                let (secs, exe) = value
                    .split_once(' ')
                    .expect("fake ssh: leftover <secs> <exe>");
                // Deliberately not waited for: it must outlive us, as the
                // background `sleep` of the shell fake it replaces did.
                #[allow(clippy::zombie_processes)]
                let _leftover = Command::new(exe)
                    .env(ENV, format!("sleep {secs}"))
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::inherit())
                    .spawn()
                    .unwrap();
            }
            "close-stderr" => close_stderr(),
            "sleep" => std::thread::sleep(Duration::from_secs_f64(value.parse().unwrap())),
            "exit" => std::process::exit(value.parse().unwrap()),
            "forward" => forward(&argv),
            other => panic!("fake ssh: unknown action {other:?}"),
        }
    }
}

/// `\n` becomes a newline and `\\` a backslash; any other backslash stays.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Emulate `ssh` handing the remote command to a POSIX login shell: drop the
/// ssh options and the destination, join the rest with spaces, split that as
/// the shell would, and run the result with stdin, stdout and stderr
/// inherited, exiting with its code. The remote `tendr` is the binary named
/// by `TENDR_TEST_BIN`.
fn forward(argv: &[String]) -> ! {
    let mut rest = argv.iter();
    // ssh options before the destination: `-o` takes a value, the others
    // tendr passes (`-T`, `-t`) do not.
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "-o" => {
                rest.next();
            }
            a if a.starts_with('-') => {}
            _ => break, // the destination
        }
    }
    let remote = rest.cloned().collect::<Vec<_>>().join(" ");
    let words = shell_words::split(&remote).expect("remote command is valid shell syntax");
    let (program, args) = words.split_first().expect("remote command is not empty");
    assert_eq!(program, "tendr", "the remote command runs tendr: {words:?}");
    let tendr = std::env::var_os("TENDR_TEST_BIN").expect("TENDR_TEST_BIN is set");
    let mut cmd = Command::new(tendr);
    cmd.args(args);
    exec(cmd)
}

#[cfg(unix)]
fn exec(mut cmd: Command) -> ! {
    use std::os::unix::process::CommandExt;
    let err = cmd.exec();
    panic!("fake ssh: exec failed: {err}");
}

#[cfg(windows)]
fn exec(mut cmd: Command) -> ! {
    let status = cmd.status().expect("fake ssh: spawn failed");
    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(unix)]
fn close_stderr() {
    // SAFETY: closing fd 2 only; nothing in this process writes to stderr
    // afterwards.
    unsafe { libc::close(2) };
}

#[cfg(windows)]
fn close_stderr() {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE};
    // SAFETY: closes this process's own stderr handle; nothing in this
    // process writes to stderr afterwards.
    unsafe { CloseHandle(GetStdHandle(STD_ERROR_HANDLE)) };
}
