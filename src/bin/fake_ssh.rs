//! Test fixture: a portable stand-in for `ssh`, linked or copied into a temp
//! PATH dir as `ssh` (`ssh.exe` on Windows) by `harness::fake_ssh` in `tests/`,
//! or under another name, such as `duckdb`, by `harness::fake_program`.
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
//! orphan <secs> <exe>
//!                   start <exe> (this fixture's built binary) sleeping <secs>,
//!                   given none of our stdio: our stdin, stdout and stderr
//!                   are made non-inheritable first. So on Windows it holds
//!                   only the handles we were handed but not given as stdio,
//!                   which are our parent's to keep from us.
//! close-stderr      close our stderr
//! sleep <secs>      sleep
//! exit <code>       exit with <code>
//! append <path>     append a line to <path>: the time in milliseconds since
//!                   the epoch, then the argv joined by spaces
//! needs-prompt <path>
//!                   while <path> exists, act as an ssh whose login needs a
//!                   prompt: write a line with an approval URL to stderr,
//!                   then exit 255 at once under `-o BatchMode=yes`, or wait
//!                   at the prompt (sleep) without it
//! stall-if <path>   while <path> exists, stall before the remote command
//!                   starts (sleep), as a lapsed check-mode approval does
//! hold <pid> <secs> sleep until process <pid> is gone, at most <secs>
//! give-up-if <path> from now on, once <path> exists: remove it, write
//!                   `Timeout, server box not responding.` to stderr and
//!                   exit 255, as ssh does when its keepalives go unanswered
//! forward           act as the remote login shell: see `forward`
//! pump              act as the remote login shell, as real ssh does (Unix
//!                   only): see `pump`
//! ```
//!
//! Without an `exit`, `forward` or `pump`, it exits 0 after the last action.

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
            "orphan" => {
                let (secs, exe) = value
                    .split_once(' ')
                    .expect("fake ssh: orphan <secs> <exe>");
                #[cfg(windows)]
                std_handles_not_inheritable();
                // Deliberately not waited for: it must outlive us.
                #[allow(clippy::zombie_processes)]
                let _orphan = Command::new(exe)
                    .env(ENV, format!("sleep {secs}"))
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap();
            }
            "close-stderr" => close_stderr(),
            "sleep" => std::thread::sleep(Duration::from_secs_f64(value.parse().unwrap())),
            "exit" => std::process::exit(value.parse().unwrap()),
            "append" => {
                let millis = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis();
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(value)
                    .unwrap();
                // One write, so a reader never sees half a line.
                let line = format!("{millis} {}\n", argv.join(" "));
                file.write_all(line.as_bytes()).unwrap();
            }
            "needs-prompt" => {
                if std::path::Path::new(value).exists() {
                    eprintln!("fake ssh: login needs approval: visit https://approve.invalid/a/1");
                    if argv.iter().any(|a| a == "BatchMode=yes") {
                        std::process::exit(255);
                    }
                    std::thread::sleep(Duration::from_secs(3600));
                }
            }
            "stall-if" => {
                if std::path::Path::new(value).exists() {
                    std::thread::sleep(Duration::from_secs(3600));
                }
            }
            "hold" => {
                let (pid, secs) = value.split_once(' ').expect("fake ssh: hold <pid> <secs>");
                hold(
                    pid.parse().unwrap(),
                    Duration::from_secs(secs.parse().unwrap()),
                );
            }
            "give-up-if" => give_up_if(value.to_owned()),
            "forward" => forward(&argv),
            "pump" => pump(&argv),
            other => panic!("fake ssh: unknown action {other:?}"),
        }
    }
}

/// Watch for `path` on a thread of its own: once it exists, remove it, say
/// what ssh says when its keepalives go unanswered, and exit as it does.
fn give_up_if(path: String) {
    std::thread::spawn(move || {
        loop {
            if std::fs::remove_file(&path).is_ok() {
                let mut err = std::io::stderr().lock();
                let _ = err.write_all(b"Timeout, server box not responding.\r\n");
                let _ = err.flush();
                std::process::exit(255);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    });
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

/// The remote command of an ssh `argv`, as a POSIX login shell splits it:
/// drop the ssh options and the destination, join the rest with spaces, and
/// split that as the shell would. It must run tendr, which is the binary
/// named by `TENDR_TEST_BIN`.
fn remote_tendr(argv: &[String]) -> Command {
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
    cmd
}

/// Emulate `ssh` handing the remote command to a POSIX login shell
/// ([`remote_tendr`]), and run it with stdin, stdout and stderr inherited,
/// exiting with its code.
fn forward(argv: &[String]) -> ! {
    exec(remote_tendr(argv))
}

/// Emulate `ssh` as it carries the remote command's streams: run the remote
/// tendr ([`remote_tendr`]) on pipes and copy between them and ours, then
/// exit with its code. Killing this process then closes our caller's pipes,
/// while the remote side's stay open, as sshd keeps them when the network
/// drops: a `hold` process keeps the other end of each, without reading,
/// until the remote exits (or a minute passes). The end of our stdin is
/// passed on as an end of input, as ssh's EOF is.
#[cfg(unix)]
fn pump(argv: &[String]) -> ! {
    use std::os::fd::OwnedFd;

    let pipe = || -> (OwnedFd, OwnedFd) {
        let (read, write) = rustix::pipe::pipe().expect("fake ssh: pipe");
        for end in [&read, &write] {
            rustix::io::fcntl_setfd(end, rustix::io::FdFlags::CLOEXEC).unwrap();
        }
        (read, write)
    };
    let (in_read, in_write) = pipe();
    let (out_read, out_write) = pipe();
    let (err_read, err_write) = pipe();
    let mut remote = remote_tendr(argv)
        .stdin(Stdio::from(in_read))
        .stdout(Stdio::from(out_write))
        .stderr(Stdio::from(err_write))
        .spawn()
        .expect("fake ssh: spawn the remote tendr");
    let holder = Command::new(std::env::current_exe().unwrap())
        .env(ENV, format!("hold {} 60", remote.id()))
        .stdin(Stdio::from(in_write.try_clone().unwrap()))
        .stdout(Stdio::from(out_read.try_clone().unwrap()))
        .stderr(Stdio::from(err_read.try_clone().unwrap()))
        .spawn()
        .expect("fake ssh: spawn the holder");
    let holder = std::sync::Arc::new(std::sync::Mutex::new(holder));

    let copy = |from: OwnedFd, to: Box<dyn std::io::Write + Send>| {
        std::thread::spawn(move || {
            let mut from = std::fs::File::from(from);
            let mut to = to;
            let mut buf = [0u8; 8192];
            while let Ok(n @ 1..) = std::io::Read::read(&mut from, &mut buf) {
                if to.write_all(&buf[..n]).and_then(|()| to.flush()).is_err() {
                    break;
                }
            }
        })
    };
    let stdout = copy(out_read, Box::new(std::io::stdout()));
    let stderr = copy(err_read, Box::new(std::io::stderr()));
    let held = std::sync::Arc::clone(&holder);
    std::thread::spawn(move || {
        let _ = std::io::copy(
            &mut std::io::stdin().lock(),
            &mut std::fs::File::from(in_write),
        );
        // Our input ended (the copy dropped its end): the remote's ends with
        // it once the holder lets go of its own.
        let mut holder = held.lock().unwrap();
        let _ = holder.kill();
        let _ = holder.wait();
    });

    let status = remote.wait().expect("fake ssh: wait for the remote tendr");
    let _ = stdout.join();
    let _ = stderr.join();
    let mut holder = holder.lock().unwrap();
    let _ = holder.kill();
    let _ = holder.wait();
    std::process::exit(status.code().unwrap_or(255));
}

#[cfg(windows)]
fn pump(_: &[String]) -> ! {
    panic!("fake ssh: pump is Unix-only");
}

/// Sleep until process `pid` is gone, at most `limit`.
fn hold(pid: u32, limit: Duration) {
    let until = std::time::Instant::now() + limit;
    while std::time::Instant::now() < until && alive(pid) {
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: kill(2) with signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(windows)]
fn alive(_: u32) -> bool {
    false
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

/// Keep our own stdin, stdout and stderr from being inherited by what we
/// start, so it holds only the other inheritable handles we were handed.
#[cfg(windows)]
fn std_handles_not_inheritable() {
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: only clears a flag on this process's own std handle; a
        // missing one makes the call fail, harmlessly.
        unsafe { SetHandleInformation(GetStdHandle(which), HANDLE_FLAG_INHERIT, 0) };
    }
}
