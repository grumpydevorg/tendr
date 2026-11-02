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
//! script            act as the remote tendr of an `attach --host`, playing
//!                   the lines after this one: see `play`
//! ```
//!
//! Without an `exit`, `forward`, `pump` or `script`, it exits 0 after the
//! last action.

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
    let mut lines = script.lines().filter(|l| !l.trim().is_empty());
    while let Some(line) = lines.next() {
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
            "script" => play(&argv, lines.collect()),
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

/// Act as the remote tendr of an `attach --host`, statelessly, for the
/// attach client's conformance tests, with no session behind it: say hello
/// on stderr if asked, skip the remote frame, read the client's first attach
/// frame, and play the steps of `lines` for it. Each line is `<first>
/// <step>`, where `<first>` names the first frame: `hello` (an attach or
/// takeover hello), `watch` (a watch hello) or `resume`. Frames are built
/// with `attach_proto`, so they are the protocol's own. The steps:
///
/// ```text
/// answer         accept it: `AcceptedV2` at epoch 1 and offset 0 of a new
///                run for a hello, `Watching` at offset 0 for a watch, and
///                `Resumed` at the next epoch and the offset asked for, for
///                a resume
/// gone <n>       answer a resume `Gone` at the next epoch: the head is <n>
///                (at least 1) bytes past the offset asked for, of which the
///                session still holds the last n - 1
/// output <text>  the session's output (`\n` is a newline): stamped from the
///                answer's offset under control, plain for a watcher
/// retire         `Retired` at the next epoch: another client took over
/// log <path>     from now on append the frames the client sent to <path>,
///                `{:?}` one per line, its first frame first
/// await <type>   read the client's frames until one of <type> (the name
///                of a `Frame` variant, such as `Repaint`) or the end of
///                its input: `await Repaint` waits until the client relays
/// drain          read the client's frames until its input ends
/// drop           exit 255 without an exit trailer: the link is lost
/// exit <code>    exit with <code>, after the exit trailer
/// ```
///
/// After the last step it exits 0, after the exit trailer.
fn play(argv: &[String], lines: Vec<&str>) -> ! {
    use std::io::Read;
    use tendr::attach_proto::{self, Frame, Mode, ResumePoint, WatchPoint};
    use tendr::model::ids::RunId;
    use tendr::model::pty_control::ControllerEpoch;

    let has = |flag: &str| argv.iter().any(|a| a == flag);
    if has("--hello") {
        eprintln!("{}", tendr::ssh::HELLO_LINE);
    }
    let trailer = has("--exit-trailer");
    let exit = |code: i32| -> ! {
        if trailer {
            eprintln!("{}{code}", tendr::ssh::EXIT_TRAILER);
        }
        std::process::exit(code)
    };
    let next = |epoch: ControllerEpoch| epoch.next().expect("fake ssh: the next epoch");

    let mut stdin = std::io::stdin().lock();
    let mut len = [0u8; 4];
    stdin
        .read_exact(&mut len)
        .expect("fake ssh: the remote frame's length");
    let mut header = vec![0; u32::from_be_bytes(len) as usize];
    stdin
        .read_exact(&mut header)
        .expect("fake ssh: the remote frame");
    let first = attach_proto::read_frame(&mut stdin).expect("fake ssh: the first attach frame");
    let fresh = ResumePoint {
        run_id: RunId::new(),
        epoch: ControllerEpoch::new(1),
        offset: 0,
    };
    let (section, mut point) = match &first {
        Frame::Hello {
            mode: Mode::Attach | Mode::Takeover,
            ..
        } => ("hello", fresh),
        Frame::Hello {
            mode: Mode::Watch, ..
        } => ("watch", fresh),
        Frame::Resume(asked) => ("resume", *asked),
        other => panic!("fake ssh: no script for a first frame {other:?}"),
    };

    let mut stdout = std::io::stdout().lock();
    let mut send = |frame: Frame| {
        // A client that has gone is the test's to notice.
        let _ = attach_proto::write_frame(&mut stdout, &frame).and_then(|()| stdout.flush());
    };
    let mut log: Option<std::fs::File> = None;
    for line in lines {
        let (first_name, step) = line
            .split_once(' ')
            .expect("fake ssh: script lines are <first> <step>");
        if first_name != section {
            continue;
        }
        let (step, value) = step.split_once(' ').unwrap_or((step, ""));
        match step {
            "answer" => send(match section {
                "hello" => Frame::AcceptedV2(point),
                "watch" => Frame::Watching(WatchPoint {
                    run_id: point.run_id,
                    offset: point.offset,
                }),
                _ => {
                    point.epoch = next(point.epoch);
                    Frame::Resumed(point)
                }
            }),
            "gone" => {
                let skipped: u64 = value.parse().expect("fake ssh: gone <n>");
                let asked = point.offset;
                point.epoch = next(point.epoch);
                point.offset = asked + skipped;
                send(Frame::Gone {
                    point,
                    oldest: asked + 1,
                });
            }
            "output" => {
                let bytes = unescape(value).into_bytes();
                if section == "watch" {
                    send(Frame::Data(bytes));
                } else {
                    let offset = point.offset;
                    point.offset += bytes.len() as u64;
                    send(Frame::Output { offset, bytes });
                }
            }
            "retire" => send(Frame::Retired(next(point.epoch))),
            "log" => {
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(value)
                    .unwrap();
                writeln!(file, "{first:?}").unwrap();
                log = Some(file);
            }
            "await" => {
                while let Some(frame) = receive(&mut stdin, log.as_mut()) {
                    if format!("{frame:?}").split(['(', ' ', '{']).next() == Some(value) {
                        break;
                    }
                }
            }
            "drain" => while receive(&mut stdin, log.as_mut()).is_some() {},
            "drop" => std::process::exit(255),
            "exit" => exit(value.parse().unwrap()),
            other => panic!("fake ssh: unknown script step {other:?}"),
        }
    }
    exit(0)
}

/// The client's next frame, appended to `log` if there is one; `None` once
/// its input ends.
fn receive(
    stdin: &mut impl std::io::Read,
    log: Option<&mut std::fs::File>,
) -> Option<tendr::attach_proto::Frame> {
    use tendr::attach_proto::{FrameError, read_frame};
    match read_frame(stdin) {
        Ok(frame) => {
            if let Some(log) = log {
                writeln!(log, "{frame:?}").unwrap();
            }
            Some(frame)
        }
        Err(FrameError::Io(_)) => None,
        Err(e) => panic!("fake ssh: the client sent {e:?}"),
    }
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
