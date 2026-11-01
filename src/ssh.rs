//! Remote transport — a thin SSH wrapper, not a second lifecycle model.
//!
//! `--host` forwards the remote-capable commands (the binary's
//! `RemoteOperation`) to a remote `tendr`, which runs the *same* local
//! lifecycle on the far side. Ordinary commands are POSIX-shell-quoted into an
//! `ssh -T` argv; `exec` instead rides the [`exec_frame`](crate::exec_frame)
//! over SSH stdin so no user value ever reaches the remote argv. Destinations
//! are validated ([`validate_destination`]) to fail closed against
//! option-shaped hosts.

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
/// Enforced at the CLI boundary (exit 2) AND re-checked inside `exec_ssh` /
/// `exec_ssh_frame`, so no non-CLI caller can bypass the guard.
pub fn validate_destination(host: &str) -> Result<(), SshError> {
    if host.is_empty() || host.starts_with('-') {
        return Err(SshError::InvalidDestination(host.to_owned()));
    }
    Ok(())
}

/// Build the SSH command line for remote tendr invocation.
///
/// SSH sends everything after the destination as a single string to the
/// remote login shell. To preserve argument boundaries (spaces, quotes,
/// shell metacharacters), each tendr argument is individually
/// shell-quoted using `shell_words::quote()` (POSIX quoting).
///
/// The resulting local argv is:
///
/// ```text
/// ssh -T -o ConnectTimeout=10 <host> tendr --hello 'arg1' 'arg2' ...
/// ```
///
/// SSH concatenates "tendr", "'arg1'", "'arg2'" with spaces and sends
/// the result to the remote login shell, which re-splits it into argv.
/// `--hello` asks the remote tendr to announce itself on stderr first
/// ([`HELLO_LINE`]), which ends the connect deadline. An `attach` (with a
/// TTY, `allocate_tty`) has no hello: its stderr shares the terminal.
///
/// No `--` separator is used between the host and the remote command.
/// SSH does not treat `--` as an option terminator — it would be sent
/// literally to the remote shell as part of the command string.
///
/// **Scope:** POSIX remote login shells only (bash, zsh, sh, etc.).
/// Windows remote hosts (PowerShell, cmd.exe) need a different quoting
/// strategy and are deferred to a follow-on slice.
pub fn build_ssh_command(
    host: &str,
    tendr_args: &[String],
    allocate_tty: bool,
) -> Result<Command, SshError> {
    // Fail closed: the builder validates the destination, so no caller can
    // obtain a spawnable Command for an option-shaped/empty host — not just the
    // CLI path. See `validate_destination`.
    validate_destination(host)?;
    let mut cmd = Command::new("ssh");
    let tty_flag = if allocate_tty { "-t" } else { "-T" };
    cmd.args([tty_flag, "-o", "ConnectTimeout=10", host]);

    // Each tendr arg becomes a separate ssh argv entry, individually
    // quoted for the remote shell. SSH concatenates all args after the
    // host with spaces to form the remote command string.
    cmd.arg("tendr");
    let hello = sends_hello(allocate_tty);
    if hello {
        cmd.arg("--hello");
    }
    for arg in tendr_args {
        cmd.arg(shell_words::quote(arg).into_owned());
    }

    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(if hello {
            Stdio::piped()
        } else {
            Stdio::inherit()
        });
    Ok(cmd)
}

/// Whether a call carries `--hello` and so runs under the connect deadline:
/// not for `attach` (its stderr shares the TTY), and not with the deadline
/// off (`TENDR_SSH_CONNECT_TIMEOUT=0`), which is how an older remote tendr
/// that does not know `--hello` is still reached.
fn sends_hello(allocate_tty: bool) -> bool {
    !allocate_tty && connect_deadline().is_some()
}

/// Execute a remote tendr command via SSH, streaming stdout/stderr
/// directly to the local stdout/stderr.
///
/// Returns the remote tendr exit code on success.
/// Returns `SshError::TransportFailed` for SSH connection failures (exit 255),
/// `SshError::ConnectDeadline` if the remote tendr never said hello in time,
/// or `SshError::InvalidDestination` for an option-shaped/empty host.
pub fn exec_ssh(host: &str, tendr_args: &[String], allocate_tty: bool) -> Result<i32, SshError> {
    let mut child = build_ssh_command(host, tendr_args, allocate_tty)?
        .spawn()
        .map_err(SshError::SpawnFailed)?;

    let status = match connect_deadline().filter(|_| sends_hello(allocate_tty)) {
        Some(deadline) => wait_after_hello(host, &mut child, deadline)?,
        None => child.wait().map_err(SshError::SpawnFailed)?,
    };
    exit_code(status)
}

/// The remote argv for frame-transport exec — constant by construction:
/// nothing user-controlled rides in argv, so there is no shell-quoting
/// layer to traverse. The frame
/// itself travels on the SSH stdin channel as opaque bytes.
fn build_ssh_exec_frame_command(host: &str, inherit_stdin: bool) -> Result<Command, SshError> {
    validate_destination(host)?;
    let hello = sends_hello(false);
    let mut cmd = Command::new("ssh");
    cmd.args(["-T", "-o", "ConnectTimeout=10", host, "tendr"]);
    if hello {
        cmd.arg("--hello");
    }
    cmd.args(["exec", "--frame-from-stdin"]);
    cmd.stdin(if inherit_stdin {
        Stdio::inherit()
    } else {
        Stdio::piped()
    })
    .stdout(Stdio::inherit())
    .stderr(if hello {
        Stdio::piped()
    } else {
        Stdio::inherit()
    });
    Ok(cmd)
}

/// Execute a remote exec by shipping the frame over the SSH stdin
/// channel. `frame` = `None` inherits local stdin (the
/// `--frame-from-stdin` passthrough form — the caller's stdin already
/// is the frame).
///
/// A failed write to the remote's stdin is deliberately ignored: it
/// means the remote died early (EPIPE), and the remote's exit status is
/// the authoritative outcome either way.
///
/// # Errors
/// `SshError::TransportFailed` on ssh exit 255 (which shadows a remote
/// command genuinely exiting 255 — an inherent ssh limitation),
/// `SshError::ConnectDeadline`, `SshError::SpawnFailed`/`Abnormal` as for
/// `exec_ssh`.
pub fn exec_ssh_frame(host: &str, frame: Option<Vec<u8>>) -> Result<i32, SshError> {
    let mut child = build_ssh_exec_frame_command(host, frame.is_none())?
        .spawn()
        .map_err(SshError::SpawnFailed)?;

    // The frame is written on its own thread: a remote that is still
    // authenticating does not read it, and a large frame would block the
    // write, and with it the connect deadline, on a full pipe. Once ssh is
    // gone the write fails with EPIPE, so the join never hangs.
    let writer = frame.and_then(|bytes| {
        child.stdin.take().map(|mut stdin| {
            std::thread::spawn(move || {
                use std::io::Write;
                let _ = stdin.write_all(&bytes);
                // Dropping stdin closes the channel — the remote sees EOF.
            })
        })
    });

    let status = match connect_deadline().filter(|_| sends_hello(false)) {
        Some(deadline) => wait_after_hello(host, &mut child, deadline)?,
        None => child.wait().map_err(SshError::SpawnFailed)?,
    };
    if let Some(writer) = writer {
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

/// Wait for `child` (an ssh to a remote `tendr --hello …`) to exit, stopping
/// it if the remote tendr has not said hello within `deadline`.
///
/// ssh's own `ConnectTimeout` ends at key exchange; authentication can then
/// stall indefinitely (a lapsed Tailscale SSH check-mode approval waits about
/// 30 minutes, silently). The hello is the remote tendr's first stderr line,
/// so it arrives only once ssh is through. After it, the call may run as long
/// as it needs (`wait`, `log -f`). Everything on stderr except the hello is
/// passed through in order; stdout stays inherited, so the order *between*
/// the two streams is no longer exact.
///
/// Time a human spends at an ssh prompt (passphrase, host key) counts against
/// the deadline: those prompts use the terminal, not stderr.
fn wait_after_hello(
    host: &str,
    child: &mut std::process::Child,
    deadline: Duration,
) -> Result<std::process::ExitStatus, SshError> {
    let stderr = child
        .stderr
        .take()
        .expect("stderr is piped for a --hello call");
    let (events_tx, events) = mpsc::channel();
    std::thread::spawn(move || relay_stderr(stderr, events_tx));

    let started = Instant::now();
    let mut hello = false;
    let mut old_tendr = None;
    loop {
        // Only the hello proves ssh got through. An end of stream before it
        // proves nothing: a process can close its stderr and keep running.
        while let Ok(event) = events.try_recv() {
            match event {
                Relay::Hello => hello = true,
                Relay::Done { old_tendr: seen } => old_tendr = Some(seen),
            }
        }
        if let Some(status) = child.try_wait().map_err(SshError::SpawnFailed)? {
            // The relay reads stderr to its end, so it waits for every holder
            // of it, not just ssh. After the hello that is the command's own
            // output, kept as long as it lasts (a ProxyCommand that ignores
            // ssh's SIGHUP holds the call open, as it held the terminal
            // before). Without a hello, a leftover holder must not hold the
            // call open: wait briefly for the rest, then report the status.
            if old_tendr.is_none() {
                old_tendr = if hello {
                    wait_relay(&events, None)
                } else {
                    wait_relay(&events, Some(Duration::from_secs(1)))
                };
            }
            if old_tendr == Some(true) {
                eprintln!(
                    "tendr on {host} does not know --hello, so it predates the \
                     connect deadline: upgrade tendr on {host}, or set \
                     TENDR_SSH_CONNECT_TIMEOUT=0 to reach it without one"
                );
            }
            return Ok(status);
        }
        if !hello && started.elapsed() >= deadline {
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

/// What the stderr relay reports.
enum Relay {
    /// The remote tendr's hello arrived.
    Hello,
    /// Stderr ended; whether the remote rejected `--hello` (a tendr too old
    /// to know it).
    Done { old_tendr: bool },
}

/// Wait for the relay to finish, at most `limit` (forever with `None`).
/// Returns its verdict on an old remote tendr, or `None` if it did not finish.
fn wait_relay(events: &mpsc::Receiver<Relay>, limit: Option<Duration>) -> Option<bool> {
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

/// Copy a `--hello` call's stderr to ours, dropping the hello and reporting
/// it. The hello ends a line but need not start one: anything a login shell's
/// rc file wrote without a newline just before tendr ran is glued in front,
/// and is passed through. Reports [`Relay::Done`] when stderr ends.
fn relay_stderr(stderr: std::process::ChildStderr, events: mpsc::Sender<Relay>) {
    use std::io::{BufRead, Write};

    let mut reader = io::BufReader::new(stderr);
    let mut out = io::stderr();
    let mut old_tendr = false;
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
        old_tendr |= String::from_utf8_lossy(&line).contains("'--hello'");
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

    /// The frame-transport argv is constant: the payload never appears.
    #[test]
    fn exec_frame_argv_is_constant() {
        let cmd = build_ssh_exec_frame_command("user@box", false).unwrap();
        let argv: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            argv,
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
        // Neither builder can hand out a spawnable Command for an option-shaped
        // or empty host — so no non-CLI caller can bypass the guard.
        let no_args: Vec<String> = vec![];
        assert!(build_ssh_command("-oProxyCommand=x", &no_args, false).is_err());
        assert!(build_ssh_command("", &no_args, false).is_err());
        assert!(build_ssh_exec_frame_command("-oProxyCommand=x", false).is_err());
        assert!(build_ssh_exec_frame_command("", false).is_err());
        // Valid destinations still build.
        assert!(build_ssh_command("user@box", &no_args, false).is_ok());
        assert!(build_ssh_exec_frame_command("user@box", false).is_ok());
    }

    /// Helper: extract the args that `build_ssh_command` would pass to the ssh binary.
    fn ssh_argv(host: &str, tendr_args: &[&str]) -> Vec<String> {
        let args: Vec<String> = tendr_args.iter().map(|s| s.to_string()).collect();
        let cmd = build_ssh_command(host, &args, false).unwrap();
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn simple_command() {
        let args = ssh_argv("user@box", &["status", "my-session"]);
        assert_eq!(args[0], "-T");
        assert_eq!(args[1], "-o");
        assert_eq!(args[2], "ConnectTimeout=10");
        assert_eq!(args[3], "user@box");
        assert_eq!(args[4], "tendr");
        assert_eq!(args[5], "--hello");
        assert_eq!(args[6], "status");
        assert_eq!(args[7], "my-session");
        assert!(
            !args.contains(&"--".to_string()),
            "no -- in ssh argv: {args:?}"
        );
    }

    #[test]
    fn args_with_spaces_are_quoted() {
        let args: Vec<String> = vec![
            "start".to_string(),
            "job".to_string(),
            "--".to_string(),
            "echo".to_string(),
            "hello world".to_string(),
        ];
        let cmd = build_ssh_command("user@box", &args, false).unwrap();
        let argv: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let remote_args = &argv[4..];
        let remote_cmd = remote_args.join(" ");
        let parsed = shell_words::split(&remote_cmd).unwrap();
        assert_eq!(
            parsed,
            vec![
                "tendr",
                "--hello",
                "start",
                "job",
                "--",
                "echo",
                "hello world"
            ]
        );
    }

    #[test]
    fn args_with_shell_metacharacters_are_quoted() {
        let args: Vec<String> = vec![
            "start".to_string(),
            "job".to_string(),
            "--".to_string(),
            "bash".to_string(),
            "-c".to_string(),
            "echo $HOME && ls".to_string(),
        ];
        let cmd = build_ssh_command("user@box", &args, false).unwrap();
        let argv: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let remote_args = &argv[4..];
        let remote_cmd = remote_args.join(" ");
        let parsed = shell_words::split(&remote_cmd).unwrap();
        assert_eq!(parsed[7], "echo $HOME && ls");
    }
}
