//! Remote transport — a thin SSH wrapper, not a second lifecycle model.
//!
//! `--host` forwards the remote-capable commands (the binary's
//! `RemoteOperation`) to a remote `tendr`, which runs the *same* local
//! lifecycle on the far side. The remote command line is constant: every
//! operation but `attach` runs `tendr [--hello] _remote` and sends the request
//! on ssh's stdin ([`exec_ssh_remote`]), and `exec` runs
//! `tendr [--hello] exec --frame-from-stdin` with its own
//! [`exec_frame`](crate::exec_frame) ([`exec_ssh_frame`]). So no user value
//! reaches the remote login shell, whatever shell that is. `attach` still runs
//! under `ssh -t` with a rebuilt command line, limited to plain names
//! ([`exec_ssh_attach`]). Destinations are validated
//! ([`validate_destination`]) to fail closed against option-shaped hosts.

use std::io;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The first stderr line of a remote `tendr --hello …`: proof that ssh got
/// through authentication and started the remote tendr.
pub const HELLO_LINE: &str = "tendr-hello 1";

/// Errors from the SSH transport layer.
#[derive(Debug, thiserror::Error)]
pub enum SshError {
    /// Failed to spawn the ssh process itself.
    #[error("failed to spawn ssh: {0}")]
    SpawnFailed(io::Error),

    /// SSH process exited with a connection-level failure (exit code 255).
    #[error("ssh transport failed: connection or authentication failure (exit 255)")]
    TransportFailed,

    /// SSH process was killed by a signal or returned an unexpected OS error.
    #[error("ssh process terminated abnormally")]
    Abnormal,

    /// The SSH destination is empty or option-shaped (begins with `-`). It is
    /// passed to the local `ssh` as a bare positional argument, so a value like
    /// `-oProxyCommand=<cmd>` would be parsed by the local ssh as an option —
    /// local command execution. Rejected before ssh is ever spawned.
    #[error("invalid --host destination {0:?}: must not be empty or begin with '-'")]
    InvalidDestination(String),

    /// An `attach` argument that is neither a plain name ([`is_plain_name`])
    /// nor one of attach's flags: the remote login shell could act on it.
    /// Rejected before ssh is ever spawned.
    #[error("refusing to send {0:?} to the remote shell: attach takes plain names only")]
    UnsafeAttachArgument(String),

    /// ssh did not get through authentication and start the remote tendr
    /// (no [`HELLO_LINE`]) within the connect deadline, and was killed.
    #[error(
        "ssh to {host} did not reach the remote tendr within {seconds} s and was \
         stopped: authentication stalled (a Tailscale SSH check-mode approval may \
         have lapsed: approve it in the browser, then retry) or the host is slow \
         to start tendr. Set TENDR_SSH_CONNECT_TIMEOUT to change the limit (0 for none)."
    )]
    ConnectDeadline { host: String, seconds: f64 },
}

impl SshError {
    /// The CLI exit code: 82 (operation deadline exceeded, as for PTY
    /// operations) for a missed connect deadline, 1 otherwise.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::ConnectDeadline { .. } => 82,
            _ => 1,
        }
    }
}

/// Validate an SSH destination before it is handed to the local `ssh` binary.
///
/// The destination is a *bare* positional argument to `ssh`. An empty or
/// option-shaped value (one beginning with `-`, e.g. `-oProxyCommand=<cmd>`)
/// would be parsed by the local ssh as an *option*, enabling local command
/// execution. Valid destinations — `user@host`, ssh-config aliases, IPv4, and
/// bracketed IPv6 (`[::1]`) — never begin with `-`, so a single non-empty /
/// leading-dash check preserves every legitimate form while closing the vector.
///
/// Enforced at the CLI boundary (exit 2) AND re-checked by every command
/// builder here, so no non-CLI caller can bypass the guard.
pub fn validate_destination(host: &str) -> Result<(), SshError> {
    if host.is_empty() || host.starts_with('-') {
        return Err(SshError::InvalidDestination(host.to_owned()));
    }
    Ok(())
}

/// Whether `value` is a plain name: `[A-Za-z0-9][A-Za-z0-9_-]{0,254}`. No
/// POSIX shell, cmd.exe or PowerShell treats any of these characters
/// specially, so a plain name can go on a remote command line as it is.
pub fn is_plain_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
        && value.len() <= 255
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The flags an `attach` command line may carry besides plain names.
const ATTACH_FLAGS: [&str; 3] = ["--namespace", "--takeover", "--escape"];

/// Build `ssh -t -o ConnectTimeout=10 <host> tendr <args>` for `attach`,
/// which needs a remote terminal and so cannot take its request on stdin
/// until the attach bridge lands. Every arg must be a plain name or an attach
/// flag; anything else is refused, so the remote shell sees no
/// metacharacter. No hello: the remote stderr shares the terminal.
fn build_ssh_attach_command(host: &str, args: &[String]) -> Result<Command, SshError> {
    validate_destination(host)?;
    if let Some(unsafe_arg) = args
        .iter()
        .find(|a| !is_plain_name(a) && !ATTACH_FLAGS.contains(&a.as_str()))
    {
        return Err(SshError::UnsafeAttachArgument(unsafe_arg.clone()));
    }
    let mut cmd = Command::new("ssh");
    cmd.args(["-t", "-o", "ConnectTimeout=10", host, "tendr"])
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    Ok(cmd)
}

/// Run `tendr <args>` (an `attach`) on `host` under `ssh -t`, see
/// [`build_ssh_attach_command`]. Returns the remote exit code.
///
/// # Errors
/// `SshError::InvalidDestination`, `SshError::UnsafeAttachArgument` before
/// ssh starts; `SshError::TransportFailed` on ssh exit 255;
/// `SshError::SpawnFailed`/`Abnormal`.
pub fn exec_ssh_attach(host: &str, args: &[String]) -> Result<i32, SshError> {
    let status = build_ssh_attach_command(host, args)?
        .spawn()
        .and_then(|mut child| child.wait())
        .map_err(SshError::SpawnFailed)?;
    exit_code(status)
}

/// The remote tendr entry point of a framed call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    /// `exec --frame-from-stdin`: the exec frame.
    Exec,
    /// `_remote`: every other operation but attach.
    Remote,
}

impl Entry {
    fn args(self) -> &'static [&'static str] {
        match self {
            Self::Exec => &["exec", "--frame-from-stdin"],
            Self::Remote => &["_remote"],
        }
    }
}

/// What a framed call writes to ssh's stdin.
enum Input {
    /// Local stdin itself, which already is the frame.
    Inherit,
    /// These bytes, then end of input.
    Frame(Vec<u8>),
    /// These bytes, then local stdin until it ends (push).
    FrameThenStdin(Vec<u8>),
}

/// The argv of a framed call — constant by construction:
/// `ssh -T -o ConnectTimeout=10 <host> tendr [--hello] <entry>`. Nothing
/// user-controlled rides in it, so there is no shell-quoting layer to
/// traverse; the request travels on ssh's stdin as opaque bytes. `--hello`
/// asks the remote tendr to announce itself on stderr first ([`HELLO_LINE`]),
/// which ends the connect deadline; it is left out when the deadline is off
/// (`TENDR_SSH_CONNECT_TIMEOUT=0`), which is how a remote exec too old to
/// know it is still reached. Stderr is always relayed, so an old remote
/// tendr's parse error can be explained ([`wait_framed`]).
///
/// No `--` separator is used between the host and the remote command: ssh
/// does not treat `--` after the destination as an option terminator.
fn build_ssh_frame_command(
    host: &str,
    entry: Entry,
    inherit_stdin: bool,
) -> Result<Command, SshError> {
    validate_destination(host)?;
    let mut cmd = Command::new("ssh");
    cmd.args(["-T", "-o", "ConnectTimeout=10", host, "tendr"]);
    if connect_deadline().is_some() {
        cmd.arg("--hello");
    }
    cmd.args(entry.args());
    cmd.stdin(if inherit_stdin {
        Stdio::inherit()
    } else {
        Stdio::piped()
    })
    .stdout(Stdio::inherit())
    .stderr(Stdio::piped());
    Ok(cmd)
}

/// Execute a remote exec by shipping the frame over the SSH stdin
/// channel. `frame` = `None` inherits local stdin (the
/// `--frame-from-stdin` passthrough form — the caller's stdin already
/// is the frame).
///
/// # Errors
/// As for [`exec_ssh_remote`].
pub fn exec_ssh_frame(host: &str, frame: Option<Vec<u8>>) -> Result<i32, SshError> {
    let input = frame.map_or(Input::Inherit, Input::Frame);
    run_framed(host, Entry::Exec, input)
}

/// Run one request on `host`'s tendr through its `_remote` entry point:
/// write `frame` (the encoded request) to ssh's stdin, then, with `body`,
/// copy local stdin after it as it arrives, until it ends (push). Remote
/// stdout and stderr stream through; returns the remote exit code.
///
/// A failed write to the remote's stdin is deliberately ignored: it means the
/// remote died early (EPIPE), and the remote's exit status is the
/// authoritative outcome either way.
///
/// # Errors
/// `SshError::InvalidDestination` before ssh starts; `SshError::TransportFailed`
/// on ssh exit 255 (which shadows a remote command genuinely exiting 255 — an
/// inherent ssh limitation); `SshError::ConnectDeadline` if the remote tendr
/// never said hello in time; `SshError::SpawnFailed`/`Abnormal`.
pub fn exec_ssh_remote(host: &str, frame: Vec<u8>, body: bool) -> Result<i32, SshError> {
    let input = if body {
        Input::FrameThenStdin(frame)
    } else {
        Input::Frame(frame)
    };
    run_framed(host, Entry::Remote, input)
}

fn run_framed(host: &str, entry: Entry, input: Input) -> Result<i32, SshError> {
    let mut child = build_ssh_frame_command(host, entry, matches!(input, Input::Inherit))?
        .spawn()
        .map_err(SshError::SpawnFailed)?;

    // The input is written on its own thread: a remote that is still
    // authenticating does not read it, and a large frame (or a push body)
    // would block the write, and with it the connect deadline, on a full
    // pipe. Dropping ssh's stdin at the end closes the channel: the remote
    // sees EOF.
    let (frame, body) = match input {
        Input::Inherit => (None, false),
        Input::Frame(frame) => (Some(frame), false),
        Input::FrameThenStdin(frame) => (Some(frame), true),
    };
    let writer = frame.and_then(|frame| {
        child.stdin.take().map(|mut stdin| {
            std::thread::spawn(move || {
                use std::io::Write;
                if stdin.write_all(&frame).is_ok() && body {
                    let _ = io::copy(&mut io::stdin().lock(), &mut stdin);
                }
            })
        })
    });

    let status = wait_framed(host, entry, &mut child, connect_deadline())?;
    // Once ssh is gone a frame write fails with EPIPE, so that join never
    // hangs. A body copy can be blocked reading local stdin, which ssh's exit
    // does not end: it is left to the process exit.
    if let Some(writer) = writer.filter(|_| !body) {
        let _ = writer.join();
    }
    exit_code(status)
}

fn exit_code(status: std::process::ExitStatus) -> Result<i32, SshError> {
    match status.code() {
        Some(255) => Err(SshError::TransportFailed),
        Some(code) => Ok(code),
        None => Err(SshError::Abnormal),
    }
}

/// The default time for ssh to get through authentication and start the
/// remote tendr.
const DEFAULT_CONNECT_DEADLINE: Duration = Duration::from_secs(30);

/// The connect deadline: `TENDR_SSH_CONNECT_TIMEOUT` seconds, `0` (or less)
/// for none, otherwise [`DEFAULT_CONNECT_DEADLINE`]. A value that does not
/// parse, or is too large to represent, is the default.
fn connect_deadline() -> Option<Duration> {
    match std::env::var("TENDR_SSH_CONNECT_TIMEOUT")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
    {
        Some(secs) if secs <= 0.0 => None,
        Some(secs) => Some(Duration::try_from_secs_f64(secs).unwrap_or(DEFAULT_CONNECT_DEADLINE)),
        None => Some(DEFAULT_CONNECT_DEADLINE),
    }
}

/// Wait for `child` (an ssh to a remote `tendr [--hello] <entry>`) to exit,
/// stopping it if a `deadline` is set and the remote tendr has not said hello
/// within it.
///
/// ssh's own `ConnectTimeout` ends at key exchange; authentication can then
/// stall indefinitely (a lapsed Tailscale SSH check-mode approval waits about
/// 30 minutes, silently). The hello is the remote tendr's first stderr line,
/// so it arrives only once ssh is through. After it, the call may run as long
/// as it needs (`wait`, `log -f`). Everything on stderr except the hello is
/// passed through in order; stdout stays inherited, so the order *between*
/// the two streams is no longer exact.
///
/// A remote tendr too old for the call fails with clap's parse error (exit
/// 2), which is passed on with advice on what to do.
///
/// Time a human spends at an ssh prompt (passphrase, host key) counts against
/// the deadline: those prompts use the terminal, not stderr.
fn wait_framed(
    host: &str,
    entry: Entry,
    child: &mut std::process::Child,
    deadline: Option<Duration>,
) -> Result<std::process::ExitStatus, SshError> {
    let stderr = child
        .stderr
        .take()
        .expect("stderr is piped for a framed call");
    let (events_tx, events) = mpsc::channel();
    std::thread::spawn(move || relay_stderr(stderr, events_tx));

    let started = Instant::now();
    let mut hello = false;
    let mut verdict = None;
    loop {
        // Only the hello proves ssh got through. An end of stream before it
        // proves nothing: a process can close its stderr and keep running.
        while let Ok(event) = events.try_recv() {
            match event {
                Relay::Hello => hello = true,
                Relay::Done { old_tendr } => verdict = Some(old_tendr),
            }
        }
        if let Some(status) = child.try_wait().map_err(SshError::SpawnFailed)? {
            // The relay reads stderr to its end, so it waits for every holder
            // of it, not just ssh. After the hello that is the command's own
            // output, kept as long as it lasts (a ProxyCommand that ignores
            // ssh's SIGHUP holds the call open, as it held the terminal
            // before). Without a hello, a leftover holder must not hold the
            // call open: wait briefly for the rest, then report the status.
            if verdict.is_none() {
                verdict = if hello {
                    wait_relay(&events, None)
                } else {
                    wait_relay(&events, Some(Duration::from_secs(1)))
                };
            }
            if let Some(Some(old)) = verdict.filter(|_| status.code() == Some(2)) {
                eprintln!("{}", old.advice(host, entry));
            }
            return Ok(status);
        }
        if let Some(deadline) = deadline.filter(|d| !hello && started.elapsed() >= *d) {
            stop(child);
            // The relay is not waited for: a lingering ProxyCommand may hold
            // stderr open, and only an unfinished pre-hello line is lost.
            return Err(SshError::ConnectDeadline {
                host: host.to_owned(),
                seconds: deadline.as_secs_f64(),
            });
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// How a remote tendr showed it is too old for the call, by its parse error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OldTendr {
    /// It does not know `--hello` (before the connect deadline).
    NoHello,
    /// It does not know `_remote` (before the frame transport).
    NoRemote,
}

impl OldTendr {
    /// The parse error in a stderr line, if it is one of these.
    fn detect(line: &[u8]) -> Option<Self> {
        let line = String::from_utf8_lossy(line);
        if line.contains("unrecognized subcommand '_remote'") {
            Some(Self::NoRemote)
        } else if line.contains("'--hello'") {
            Some(Self::NoHello)
        } else {
            None
        }
    }

    /// What the caller should do. An exec reaches a tendr without `--hello`
    /// once the deadline is off; every other operation needs a tendr that
    /// knows `_remote`.
    fn advice(self, host: &str, entry: Entry) -> String {
        match (self, entry) {
            (Self::NoHello, Entry::Exec) => format!(
                "tendr on {host} does not know --hello, so it predates the \
                 connect deadline: upgrade tendr on {host}, or set \
                 TENDR_SSH_CONNECT_TIMEOUT=0 to reach it without one"
            ),
            _ => format!(
                "tendr on {host} is too old for this client: upgrade it to {} or later",
                env!("CARGO_PKG_VERSION")
            ),
        }
    }
}

/// What the stderr relay reports.
enum Relay {
    /// The remote tendr's hello arrived.
    Hello,
    /// Stderr ended; whether, before any hello, the remote rejected the call
    /// as a tendr too old for it.
    Done { old_tendr: Option<OldTendr> },
}

/// Wait for the relay to finish, at most `limit` (forever with `None`).
/// Returns its verdict on an old remote tendr, or `None` if it did not finish.
fn wait_relay(events: &mpsc::Receiver<Relay>, limit: Option<Duration>) -> Option<Option<OldTendr>> {
    let until = limit.map(|limit| Instant::now() + limit);
    loop {
        let event = match until {
            None => events.recv().ok()?,
            Some(until) => events
                .recv_timeout(until.saturating_duration_since(Instant::now()))
                .ok()?,
        };
        if let Relay::Done { old_tendr } = event {
            return Some(old_tendr);
        }
    }
}

/// Stop an ssh that missed the connect deadline: SIGTERM first, so ssh can
/// restore a terminal it left mid-prompt and hang up its ProxyCommand, then
/// SIGKILL if it is still there after a second.
fn stop(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        if let Ok(pid) = i32::try_from(child.id()) {
            // SAFETY: `pid` is our own unreaped child, so it cannot have been
            // reused; SIGTERM has no memory-safety implications.
            unsafe { libc::kill(pid, libc::SIGTERM) };
            let until = Instant::now() + Duration::from_secs(1);
            while Instant::now() < until {
                if let Ok(Some(_)) = child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Copy a framed call's stderr to ours, dropping the hello and reporting
/// it. The hello ends a line but need not start one: anything a login shell's
/// rc file wrote without a newline just before tendr ran is glued in front,
/// and is passed through. Reports [`Relay::Done`] when stderr ends.
fn relay_stderr(stderr: std::process::ChildStderr, events: mpsc::Sender<Relay>) {
    use std::io::{BufRead, Write};

    let mut reader = io::BufReader::new(stderr);
    let mut out = io::stderr();
    let mut old_tendr = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => {
                let _ = events.send(Relay::Done { old_tendr });
                return;
            }
            Ok(_) => {}
        }
        let body = line.trim_ascii_end();
        if body.ends_with(HELLO_LINE.as_bytes()) {
            let prefix = &body[..body.len() - HELLO_LINE.len()];
            if !prefix.is_empty() {
                let _ = out.write_all(prefix);
                let _ = out.write_all(b"\n");
            }
            let _ = events.send(Relay::Hello);
            break;
        }
        old_tendr = old_tendr.or_else(|| OldTendr::detect(&line));
        let _ = out.write_all(&line);
    }
    // After the hello the stream is the command's own: copy it as it comes.
    {
        let mut out = out.lock();
        let _ = io::copy(&mut reader, &mut out);
    }
    let _ = events.send(Relay::Done { old_tendr });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// The frame-transport argv is constant: the payload never appears.
    #[test]
    fn exec_frame_argv_is_constant() {
        let cmd = build_ssh_frame_command("user@box", Entry::Exec, false).unwrap();
        assert_eq!(
            argv(&cmd),
            [
                "-T",
                "-o",
                "ConnectTimeout=10",
                "user@box",
                "tendr",
                "--hello",
                "exec",
                "--frame-from-stdin"
            ]
        );
    }

    /// Every operation but exec and attach runs this one argv; the request
    /// is not an input to it.
    #[test]
    fn remote_frame_argv_is_constant() {
        let cmd = build_ssh_frame_command("user@box", Entry::Remote, false).unwrap();
        assert_eq!(
            argv(&cmd),
            [
                "-T",
                "-o",
                "ConnectTimeout=10",
                "user@box",
                "tendr",
                "--hello",
                "_remote"
            ]
        );
    }

    #[test]
    fn attach_argv_is_the_plain_args_under_a_tty() {
        let args: Vec<String> = ["attach", "s-1", "--namespace", "n_2", "--takeover"]
            .map(str::to_owned)
            .to_vec();
        let cmd = build_ssh_attach_command("user@box", &args).unwrap();
        assert_eq!(
            argv(&cmd),
            [
                "-t",
                "-o",
                "ConnectTimeout=10",
                "user@box",
                "tendr",
                "attach",
                "s-1",
                "--namespace",
                "n_2",
                "--takeover"
            ]
        );
    }

    #[test]
    fn attach_refuses_anything_but_plain_args() {
        for bad in [
            "x&calc",
            "a b",
            "$(id)",
            "'q'",
            "-x",
            "--hello",
            "caf\u{e9}",
            "",
        ] {
            let args = vec!["attach".to_owned(), bad.to_owned()];
            assert!(
                matches!(
                    build_ssh_attach_command("user@box", &args),
                    Err(SshError::UnsafeAttachArgument(a)) if a == bad
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn plain_names_are_alphanumeric_dash_and_underscore() {
        let long = "a".repeat(255);
        for ok in ["a", "Z9", "build-1", "a_b-C", long.as_str()] {
            assert!(is_plain_name(ok), "{ok:?}");
        }
        let too_long = "a".repeat(256);
        for bad in [
            "",
            "-a",
            "_a",
            "a.b",
            "a/b",
            "a b",
            "a&b",
            "a|b",
            "a;b",
            "a$b",
            "a`b",
            "a'b",
            "a\"b",
            "a%b",
            "a^b",
            "\u{e9}",
            too_long.as_str(),
        ] {
            assert!(!is_plain_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn old_tendr_is_recognised_by_its_parse_error() {
        assert_eq!(
            OldTendr::detect(b"error: unrecognized subcommand '_remote'\n"),
            Some(OldTendr::NoRemote)
        );
        assert_eq!(
            OldTendr::detect(b"error: unexpected argument '--hello' found\n"),
            Some(OldTendr::NoHello)
        );
        assert_eq!(OldTendr::detect(b"session not found: x\n"), None);
    }

    #[test]
    fn validate_destination_rejects_empty_and_option_shaped() {
        for bad in [
            "",
            "-",
            "-t",
            "--foo",
            "-oProxyCommand=calc",
            "-oProxyCommand=x",
        ] {
            assert!(
                validate_destination(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn validate_destination_preserves_valid_forms() {
        for ok in [
            "box",
            "rick-windows",
            "user@host",
            "user@10.0.0.1",
            "10.0.0.1",
            "host.example.com",
            "[::1]",
            "user@[fe80::1]",
        ] {
            assert!(
                validate_destination(ok).is_ok(),
                "expected {ok:?} to be accepted"
            );
        }
    }

    #[test]
    fn builders_fail_closed_on_invalid_destination() {
        // No builder hands out a spawnable Command for an option-shaped or
        // empty host — so no non-CLI caller can bypass the guard.
        let attach = vec!["attach".to_owned(), "s".to_owned()];
        for entry in [Entry::Exec, Entry::Remote] {
            assert!(build_ssh_frame_command("-oProxyCommand=x", entry, false).is_err());
            assert!(build_ssh_frame_command("", entry, false).is_err());
            assert!(build_ssh_frame_command("user@box", entry, false).is_ok());
        }
        assert!(build_ssh_attach_command("-oProxyCommand=x", &attach).is_err());
        assert!(build_ssh_attach_command("", &attach).is_err());
        assert!(build_ssh_attach_command("user@box", &attach).is_ok());
    }
}
