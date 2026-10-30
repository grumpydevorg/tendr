//! Remote transport — a thin SSH wrapper, not a second lifecycle model.
//!
//! `--host` forwards an allowlisted subset of commands ([`REMOTE_COMMANDS`]) to
//! a remote `tendr`, which runs the *same* local lifecycle on the far side.
//! Ordinary commands are POSIX-shell-quoted into an `ssh -T` argv; `exec`
//! instead rides the [`exec_frame`](crate::exec_frame) over SSH stdin so no
//! user value ever reaches the remote argv. Destinations are validated
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

/// Commands whose argv is forwarded verbatim over SSH transport.
///
/// `exec` is deliberately absent: it is remote-capable, but travels via
/// the frame transport (`exec_ssh_frame`) — the request rides the SSH
/// stdin channel as one JSON frame, never the remote argv. Every command
/// not in this list (and not `exec`) is local-only when `--host` is set —
/// e.g. `run`, `wrap`, `prune`, `query`, `guide`, `skill`, `emit`,
/// `events`. Of these, `wrap` relies on local process context, `run`
/// reads a local script file, and `prune` walks the local session root.
pub const REMOTE_COMMANDS: &[&str] = &[
    "start", "status", "list", "log", "push", "kill", "wait", "watch", "attach",
];

/// Check whether a subcommand is supported for remote execution.
pub fn is_remote_supported(subcommand: &str) -> bool {
    REMOTE_COMMANDS.contains(&subcommand)
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
    if !allocate_tty {
        cmd.arg("--hello");
    }
    for arg in tendr_args {
        cmd.arg(shell_words::quote(arg).into_owned());
    }

    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(if allocate_tty {
            Stdio::inherit()
        } else {
            Stdio::piped()
        });
    Ok(cmd)
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

    let status = if allocate_tty {
        child.wait().map_err(SshError::SpawnFailed)?
    } else {
        wait_after_hello(host, &mut child, connect_deadline())?
    };
    exit_code(status)
}

/// The remote argv for frame-transport exec — constant by construction:
/// nothing user-controlled rides in argv, so there is no shell-quoting
/// layer to traverse (2026-07-08-remote-exec-host-parity.md slice 2). The frame
/// itself travels on the SSH stdin channel as opaque bytes.
fn build_ssh_exec_frame_command(host: &str, inherit_stdin: bool) -> Result<Command, SshError> {
    validate_destination(host)?;
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-T",
        "-o",
        "ConnectTimeout=10",
        host,
        "tendr",
        "--hello",
        "exec",
        "--frame-from-stdin",
    ]);
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
/// A failed write to the remote's stdin is deliberately ignored: it
/// means the remote died early (EPIPE), and the remote's exit status is
/// the authoritative outcome either way.
///
/// # Errors
/// `SshError::TransportFailed` on ssh exit 255 (which shadows a remote
/// command genuinely exiting 255 — an inherent ssh limitation),
/// `SshError::ConnectDeadline`, `SshError::SpawnFailed`/`Abnormal` as for
/// `exec_ssh`.
pub fn exec_ssh_frame(host: &str, frame: Option<&[u8]>) -> Result<i32, SshError> {
    let mut child = build_ssh_exec_frame_command(host, frame.is_none())?
        .spawn()
        .map_err(SshError::SpawnFailed)?;

    // The frame is written on its own thread: a remote that is still
    // authenticating does not read it, and a large frame would block the
    // write, and with it the connect deadline, on a full pipe.
    let writer = frame.and_then(|bytes| {
        let bytes = bytes.to_vec();
        child.stdin.take().map(|mut stdin| {
            std::thread::spawn(move || {
                use std::io::Write;
                let _ = stdin.write_all(&bytes);
                // Dropping stdin closes the channel — the remote sees EOF.
            })
        })
    });

    let status = wait_after_hello(host, &mut child, connect_deadline())?;
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

/// The connect deadline: `TENDR_SSH_CONNECT_TIMEOUT` seconds, `0` for none,
/// otherwise [`DEFAULT_CONNECT_DEADLINE`]. An unparseable value is the default.
fn connect_deadline() -> Option<Duration> {
    match std::env::var("TENDR_SSH_CONNECT_TIMEOUT")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
    {
        Some(secs) if secs <= 0.0 => None,
        Some(secs) if secs.is_finite() => Some(Duration::from_secs_f64(secs)),
        _ => Some(DEFAULT_CONNECT_DEADLINE),
    }
}

/// Wait for `child` (an ssh to a remote `tendr --hello …`) to exit, killing it
/// if the remote tendr has not said hello within `deadline`.
///
/// ssh's own `ConnectTimeout` ends at key exchange; authentication can then
/// stall indefinitely (a lapsed Tailscale SSH check-mode approval waits about
/// 30 minutes, silently). The hello is the remote tendr's first stderr line,
/// so it arrives only once ssh is through. After it, the call may run as long
/// as it needs (`wait`, `log -f`). Everything on stderr except the hello is
/// passed through in order.
fn wait_after_hello(
    host: &str,
    child: &mut std::process::Child,
    deadline: Option<Duration>,
) -> Result<std::process::ExitStatus, SshError> {
    let stderr = child
        .stderr
        .take()
        .expect("stderr is piped for a --hello call");
    let (hello_tx, hello_rx) = mpsc::channel();
    let relay = std::thread::spawn(move || relay_stderr(stderr, hello_tx));

    let started = Instant::now();
    let mut hello = false;
    loop {
        if !hello {
            match hello_rx.try_recv() {
                Ok(()) => hello = true,
                // The relay ended without a hello: stderr closed, so ssh is
                // exiting; its status says why.
                Err(mpsc::TryRecvError::Disconnected) => hello = true,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(status) = child.try_wait().map_err(SshError::SpawnFailed)? {
            let saw_old_tendr = relay.join().unwrap_or(false);
            if saw_old_tendr {
                eprintln!(
                    "tendr on {host} does not know --hello, so it predates the \
                     connect deadline: upgrade tendr on {host}"
                );
            }
            return Ok(status);
        }
        if let (false, Some(limit)) = (hello, deadline) {
            if started.elapsed() >= limit {
                let _ = child.kill();
                let _ = child.wait();
                return Err(SshError::ConnectDeadline {
                    host: host.to_owned(),
                    seconds: limit.as_secs_f64(),
                });
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Copy a `--hello` call's stderr to ours, dropping the hello line and
/// signalling it. Returns whether the remote rejected `--hello` (a tendr too
/// old to know it).
fn relay_stderr(stderr: std::process::ChildStderr, hello: mpsc::Sender<()>) -> bool {
    use std::io::{BufRead, Write};

    let mut reader = io::BufReader::new(stderr);
    let mut out = io::stderr();
    let mut old_tendr = false;
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return old_tendr,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&line);
        if text.trim_end() == HELLO_LINE {
            let _ = hello.send(());
            break;
        }
        old_tendr |= text.contains("'--hello'");
        let _ = out.write_all(&line);
    }
    // After the hello the stream is the command's own: copy it as it comes.
    let mut out = out.lock();
    let _ = io::copy(&mut reader, &mut out);
    old_tendr
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

    #[test]
    fn remote_allowlist() {
        assert!(is_remote_supported("start"));
        assert!(is_remote_supported("status"));
        assert!(is_remote_supported("list"));
        assert!(is_remote_supported("log"));
        assert!(is_remote_supported("push"));
        assert!(is_remote_supported("kill"));
        assert!(is_remote_supported("wait"));
        assert!(is_remote_supported("watch"));
        assert!(is_remote_supported("attach"));

        assert!(!is_remote_supported("run"));
        assert!(!is_remote_supported("exec"));
        assert!(!is_remote_supported("wrap"));
        assert!(!is_remote_supported("prune"));
        assert!(!is_remote_supported("_sidecar"));
    }
}
