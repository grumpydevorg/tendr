//! Remote transport — a thin SSH wrapper, not a second lifecycle model.
//!
//! `--host` forwards the remote-capable commands (the binary's
//! `RemoteOperation`) to a remote `tendr`, which runs the *same* local
//! lifecycle on the far side. The remote command line is constant: every
//! operation runs `tendr [--hello] --exit-trailer _remote` and sends the
//! request on ssh's stdin ([`exec_ssh_remote`]; `AttachSsh` for `attach`,
//! whose attach protocol frames then follow on the same stdin and stdout),
//! and `exec` runs `tendr [--hello] --exit-trailer exec --frame-from-stdin`
//! with its own [`exec_frame`](crate::exec_frame) ([`exec_ssh_frame`]). So no
//! user value reaches the remote login shell, whatever shell that is, from a
//! Unix client or a Windows one. Destinations are validated
//! ([`validate_destination`]) to fail closed against option-shaped hosts.

use std::io;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::platform::spawn_external;

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

    /// The ssh process ended without the remote's verdict other than by
    /// failing to connect: killed by a signal (Unix), ended by Windows with
    /// an NTSTATUS code, or, for an attach link already up or reconnecting
    /// on Windows, exit 1 without an exit trailer (see `classify`).
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

    /// An `attach --host` connection was stopped on purpose: the attach is
    /// leaving while it reconnects.
    #[error("ssh was stopped: the attach is leaving")]
    Cancelled,
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

/// The remote tendr entry point of a framed call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    /// `exec --frame-from-stdin`: the exec frame.
    Exec,
    /// `_remote`: every other operation, `attach` included.
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

/// Which of tendr's streams ssh gets for a framed call. Its stderr is always
/// a pipe tendr relays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Streams {
    /// Tendr's own stdin (which already is the frame) and stdout.
    Inherit,
    /// A pipe tendr writes the request to, and tendr's stdout.
    Request,
    /// A pipe each way: after the request, the attach protocol runs over them.
    Attach(Login),
}

/// Whether ssh may prompt on the terminal for an `attach --host` connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Login {
    /// ssh may ask for a passphrase or about a host key: the first
    /// connection, made before the terminal is in raw mode.
    Interactive,
    /// `BatchMode=yes`: ssh never prompts, so a login that needs a prompt
    /// fails at once. A reconnect runs this way, because the terminal is in
    /// raw mode and its keystrokes are the user's, not ssh's.
    Batch,
}

/// The ssh options of every framed call. The keepalives make ssh notice a
/// dead link within about 15 to 20 s and exit 255, instead of waiting on
/// it for as long as TCP does.
const FRAME_OPTIONS: [&str; 6] = [
    "-o",
    "ConnectTimeout=10",
    "-o",
    "ServerAliveInterval=5",
    "-o",
    "ServerAliveCountMax=3",
];

/// An attach's own connection, never a channel of a shared one. A mux
/// client sends no keepalives: the master's settings decide when a dead
/// link is noticed, and with `ServerAliveInterval 60` that took 240 s on the
/// VM instead of 20. A connection over a master whose link is dead waits
/// out the connect deadline rather than failing at `ConnectTimeout`. Other
/// framed calls keep sharing: they are short, and a shared connection
/// saves about 0.4 s each.
const ATTACH_OPTIONS: [&str; 4] = ["-o", "ControlMaster=no", "-o", "ControlPath=none"];

/// The argv of a framed call — constant by construction:
/// `ssh -T -o ConnectTimeout=10 -o ServerAliveInterval=5 -o
/// ServerAliveCountMax=3 <host> tendr [--hello] --exit-trailer <entry>`.
/// An attach adds `-o ControlMaster=no -o ControlPath=none` before the host
/// ([`ATTACH_OPTIONS`]), and its reconnect `-o BatchMode=yes` after them
/// ([`Login::Batch`]). Nothing user-controlled rides in it, so there is no
/// shell-quoting layer to traverse; the request travels on ssh's stdin as
/// opaque bytes.
///
/// - `--hello` asks the remote tendr to announce itself on stderr first
///   ([`HELLO_LINE`]), which ends the connect deadline; it is left out when
///   the deadline is off (`TENDR_SSH_CONNECT_TIMEOUT=0`).
/// - `--exit-trailer` asks it to end stderr with `tendr-exit <code>`
///   ([`EXIT_TRAILER`]): a PowerShell login shell reports any failure as ssh
///   exit 1, so ssh's own status cannot carry the code.
///
/// Stderr is always relayed, to strip both lines and to explain an old remote
/// tendr's parse error ([`Watch`]).
///
/// No `--` separator is used between the host and the remote command: ssh
/// does not treat `--` after the destination as an option terminator.
fn build_ssh_frame_command(
    host: &str,
    entry: Entry,
    streams: Streams,
) -> Result<Command, SshError> {
    validate_destination(host)?;
    let mut cmd = Command::new("ssh");
    cmd.arg("-T").args(FRAME_OPTIONS);
    if let Streams::Attach(login) = streams {
        cmd.args(ATTACH_OPTIONS);
        if login == Login::Batch {
            cmd.args(["-o", "BatchMode=yes"]);
        }
    }
    cmd.args([host, "tendr"]);
    if connect_deadline().is_some() {
        cmd.arg("--hello");
    }
    cmd.arg("--exit-trailer").args(entry.args());
    let (stdin, stdout) = match streams {
        Streams::Inherit => (Stdio::inherit(), Stdio::inherit()),
        Streams::Request => (Stdio::piped(), Stdio::inherit()),
        Streams::Attach(_) => (Stdio::piped(), Stdio::piped()),
    };
    cmd.stdin(stdin).stdout(stdout).stderr(Stdio::piped());
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
/// stdout and stderr stream through; returns the remote exit code, from the
/// remote's exit trailer when there is one, otherwise from ssh.
///
/// A failed write to the remote's stdin is deliberately ignored: it means the
/// remote died early (EPIPE), and the remote's exit status is the
/// authoritative outcome either way.
///
/// # Errors
/// `SshError::InvalidDestination` before ssh starts; `SshError::TransportFailed`
/// on ssh exit 255 without an exit trailer; `SshError::ConnectDeadline` if the
/// remote tendr never said hello in time; `SshError::SpawnFailed`/`Abnormal`.
pub fn exec_ssh_remote(host: &str, frame: Vec<u8>, body: bool) -> Result<i32, SshError> {
    let input = if body {
        Input::FrameThenStdin(frame)
    } else {
        Input::Frame(frame)
    };
    run_framed(host, Entry::Remote, input)
}

fn run_framed(host: &str, entry: Entry, input: Input) -> Result<i32, SshError> {
    let streams = match input {
        Input::Inherit => Streams::Inherit,
        Input::Frame(_) | Input::FrameThenStdin(_) => Streams::Request,
    };
    let mut cmd = build_ssh_frame_command(host, entry, streams)?;
    let mut child = spawn_external(&mut cmd).map_err(SshError::SpawnFailed)?;

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

    let mut watch = Watch::start(&mut child, connect_deadline(), Box::new(io::stderr()));
    let (status, trailer) = match watch.wait(host, &mut child, Until::Exit)? {
        Progress::Exited(status, trailer) => (status, trailer),
        Progress::Connected | Progress::Limit | Progress::Cancelled => {
            unreachable!("waited for the exit, with nothing to cancel it")
        }
    };
    // Once ssh is gone a frame write fails with EPIPE, so that join never
    // hangs. A body copy can be blocked reading local stdin, which ssh's exit
    // does not end: it is left to the process exit.
    if let Some(writer) = writer.filter(|_| !body) {
        let _ = writer.join();
    }
    exit_code(status.code(), trailer, Call::Operation)
}

/// `attach --host`: ssh to the remote tendr's `_remote` entry point, with
/// the same constant command line as every other operation. The request
/// (the remote frame, then the attach hello) goes on ssh's stdin; after it,
/// ssh's stdin and stdout carry the attach protocol, which the remote tendr
/// bridges to the session's attach socket. Raw mode, resizes and the detach
/// escape stay with the caller.
pub struct AttachSsh {
    host: String,
    child: std::process::Child,
    watch: Watch,
}

/// How [`AttachSsh::start`] went.
pub enum AttachStart {
    /// The remote tendr started (it said hello, or the connect deadline is
    /// off), and may have ended since. Its answer to the attach, if it gave
    /// one, comes next on `stdout`.
    Running {
        ssh: AttachSsh,
        stdin: std::process::ChildStdin,
        stdout: std::process::ChildStdout,
    },
    /// ssh ended first, with this exit code (the remote's, from its exit
    /// trailer, when it wrote one). Its stderr has been passed on.
    Ended(i32),
}

/// How [`AttachSsh::start`] connects, and what may stop it.
pub struct Dial {
    pub login: Login,
    /// Where ssh's stderr goes, the hello and the exit trailer taken out.
    pub stderr: Box<dyn io::Write + Send>,
    /// Set from another thread to stop ssh: a start or finish under way
    /// stops it within 20 ms and returns.
    pub cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl AttachSsh {
    /// Start ssh, write `request` to its stdin, and wait under the connect
    /// deadline until the remote tendr has said hello.
    ///
    /// A failed write of the request is ignored: the remote ended early, and
    /// its exit status says why.
    ///
    /// # Errors
    /// As for [`exec_ssh_remote`], and `SshError::Cancelled` (ssh stopped)
    /// once `dial.cancel` is set.
    pub fn start(host: &str, request: &[u8], dial: Dial) -> Result<AttachStart, SshError> {
        use std::io::Write;

        // The first dial keeps a 1 without a trailer: the remote's shell
        // may give it, before any link was up. A reconnect's dial reads it as
        // a lost link, as the end of a connection does.
        let call = match dial.login {
            Login::Interactive => Call::Operation,
            Login::Batch => Call::Attach,
        };
        let mut cmd = build_ssh_frame_command(host, Entry::Remote, Streams::Attach(dial.login))?;
        let mut child = spawn_external(&mut cmd).map_err(SshError::SpawnFailed)?;
        let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            unreachable!("both are piped for an attach");
        };
        // A request of a few hundred bytes fits in any pipe's buffer, so this
        // cannot block on a remote that is still authenticating.
        let _ = stdin.write_all(request).and_then(|()| stdin.flush());
        let mut watch = Watch::start(&mut child, connect_deadline(), dial.stderr);
        watch.cancel = Some(dial.cancel);
        match watch.wait(host, &mut child, Until::Connected)? {
            Progress::Connected => Ok(AttachStart::Running {
                ssh: Self {
                    host: host.to_owned(),
                    child,
                    watch,
                },
                stdin,
                stdout,
            }),
            Progress::Exited(status, trailer) => {
                Ok(AttachStart::Ended(exit_code(status.code(), trailer, call)?))
            }
            Progress::Cancelled => Err(SshError::Cancelled),
            Progress::Limit => unreachable!("no time limit"),
        }
    }

    /// Wait for ssh to end, and return the call's exit code as
    /// [`exec_ssh_remote`] does. With a `grace`, ssh is stopped once it has
    /// passed, and this returns `None`; so it does once the dial's `cancel`
    /// is set.
    ///
    /// # Errors
    /// `SshError::TransportFailed` on ssh exit 255 without an exit trailer;
    /// `SshError::SpawnFailed`/`Abnormal`.
    pub fn finish(mut self, grace: Option<Duration>) -> Option<Result<i32, SshError>> {
        let until = grace.map_or(Until::Exit, |grace| Until::Limit(Instant::now() + grace));
        match self.watch.wait(&self.host, &mut self.child, until) {
            Ok(Progress::Exited(status, trailer)) => {
                Some(exit_code(status.code(), trailer, Call::Attach))
            }
            Ok(Progress::Limit) => {
                stop(&mut self.child);
                None
            }
            Ok(Progress::Cancelled) => None,
            Ok(Progress::Connected) => unreachable!("waited for the exit"),
            Err(e) => Some(Err(e)),
        }
    }
}

/// Which call's exit [`exit_code`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    /// A one-shot operation (`exec`, `ls`, `push` and the rest), or the
    /// first dial of an `attach --host`.
    Operation,
    /// An `attach --host` link once a connection has been up: the end of a
    /// connection, and a reconnect's dial.
    Attach,
}

/// The call's exit code: the remote's exit trailer if it wrote one, else
/// ssh's own `code`, read as [`classify`] reads it on this client.
fn exit_code(code: Option<i32>, trailer: Option<i32>, call: Call) -> Result<i32, SshError> {
    classify(code, trailer, call, cfg!(windows))
}

/// The call's exit code from ssh's `code` and the remote's `trailer`. With
/// a trailer, it is the remote's code, 255 and 1 included. Without one, it
/// is ssh's own end:
/// - 255 is ssh failing to connect or to stay connected;
/// - no code (Unix: a signal) is ssh killed;
/// - on a `windows` client, an NTSTATUS code (the top two, severity, bits
///   not both clear, such as `0xC000013A` after Ctrl-C or Ctrl-Break) is
///   ssh.exe ended by Windows, as a signal ends it on Unix;
/// - on a `windows` client, 1 for an attach link ([`Call::Attach`]) is
///   ssh.exe's own end, a lost link: `TerminateProcess`, as Task Manager
///   and `Child::kill` use it, gives 1, and the Unix remote an attach runs
///   on never ends that way without a trailer: tendr's wrapper writes one
///   for every code (06-transport-boundaries.md, "The exit trailer"), parse
///   errors exit 2, and a shell that cannot find tendr exits 127. Other
///   calls keep the 1, which a PowerShell login shell gives for a tendr
///   that fails before its wrapper runs, with the remote's message;
/// - any other code is passed on.
fn classify(
    code: Option<i32>,
    trailer: Option<i32>,
    call: Call,
    windows: bool,
) -> Result<i32, SshError> {
    let ntstatus = |code: i32| (code as u32) >> 30 != 0;
    match (trailer, code) {
        (Some(code), _) => Ok(code),
        (None, Some(255)) => Err(SshError::TransportFailed),
        (None, Some(code)) if windows && ntstatus(code) => Err(SshError::Abnormal),
        (None, Some(1)) if windows && call == Call::Attach => Err(SshError::Abnormal),
        (None, Some(code)) => Ok(code),
        (None, None) => Err(SshError::Abnormal),
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

/// What [`Watch::wait`] waits for.
#[derive(Debug, Clone, Copy)]
enum Until {
    /// ssh's exit.
    Exit,
    /// The remote tendr's hello, or ssh's exit if that comes first. Without
    /// a deadline there is no hello to wait for, so this returns at once.
    Connected,
    /// ssh's exit, but not past this instant.
    Limit(Instant),
}

/// Where [`Watch::wait`] stopped.
enum Progress {
    /// The remote tendr started (it said hello), and may have ended since.
    Connected,
    /// ssh exited: its status, and the remote's exit trailer if it wrote one.
    Exited(std::process::ExitStatus, Option<i32>),
    /// The limit passed with ssh still running.
    Limit,
    /// The watch's `cancel` was set, and ssh has been stopped.
    Cancelled,
}

/// Watches `child`, an ssh to a remote `tendr [--hello] --exit-trailer
/// <entry>`: relays its stderr and stops it if a `deadline` is set and the
/// remote tendr has not said hello within it.
///
/// ssh's own `ConnectTimeout` ends at key exchange; authentication can then
/// stall indefinitely (a lapsed Tailscale SSH check-mode approval waits about
/// 30 minutes, silently). The hello is the remote tendr's first stderr line,
/// so it arrives only once ssh is through. After it, the call may run as long
/// as it needs (`wait`, `log -f`). Everything on stderr except the hello and
/// the trailer is passed through in order; stdout stays inherited, so the
/// order *between* the two streams is no longer exact.
///
/// A remote tendr too old for the call fails with clap's parse error, which
/// is passed on with advice on what to do.
///
/// Time a human spends at an ssh prompt (passphrase, host key) counts against
/// the deadline: those prompts use the terminal, not stderr.
struct Watch {
    events: mpsc::Receiver<Relay>,
    started: Instant,
    deadline: Option<Duration>,
    hello: bool,
    /// `Some` once stderr has ended.
    ended: Option<Ended>,
    /// Once set, a wait stops ssh and returns [`Progress::Cancelled`].
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl Watch {
    /// Start relaying `child`'s stderr to `out`; the deadline counts from
    /// now.
    fn start(
        child: &mut std::process::Child,
        deadline: Option<Duration>,
        out: Box<dyn io::Write + Send>,
    ) -> Self {
        let stderr = child
            .stderr
            .take()
            .expect("stderr is piped for a framed call");
        let (events_tx, events) = mpsc::channel();
        std::thread::spawn(move || relay_stderr(stderr, events_tx, out));
        Self {
            events,
            started: Instant::now(),
            deadline,
            hello: false,
            ended: None,
            cancel: None,
        }
    }

    /// Whether the watch's `cancel` has been set.
    fn cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|cancel| cancel.load(std::sync::atomic::Ordering::SeqCst))
    }

    /// Wait for what `until` names.
    ///
    /// # Errors
    /// `SshError::ConnectDeadline` (ssh stopped) if the remote tendr has not
    /// said hello within the deadline; `SshError::SpawnFailed` if ssh cannot
    /// be waited for.
    fn wait(
        &mut self,
        host: &str,
        child: &mut std::process::Child,
        until: Until,
    ) -> Result<Progress, SshError> {
        let limit = match until {
            Until::Limit(limit) => Some(limit),
            Until::Exit | Until::Connected => None,
        };
        let left = |poll: Duration| {
            limit.map_or(poll, |limit| {
                poll.min(limit.saturating_duration_since(Instant::now()))
            })
        };
        // A cancellable wait looks at its flag often, and never blocks on
        // ssh's exit.
        let cancellable = self.cancel.is_some();
        loop {
            if self.cancelled() {
                stop(child);
                return Ok(Progress::Cancelled);
            }
            // Only the hello proves ssh got through. An end of stream before it
            // proves nothing: a process can close its stderr and keep running.
            let awaiting_hello = self.deadline.is_some() && !self.hello;
            if matches!(until, Until::Connected) && !awaiting_hello {
                return Ok(Progress::Connected);
            }
            if limit.is_some_and(|limit| Instant::now() >= limit) {
                return Ok(Progress::Limit);
            }
            match self.ended {
                // Stderr has ended and no deadline is pending: only the exit
                // is left to wait for.
                Some(ended) if !awaiting_hello && limit.is_none() && !cancellable => {
                    let status = child.wait().map_err(SshError::SpawnFailed)?;
                    return Ok(self.exited(host, status, ended));
                }
                Some(_) => std::thread::sleep(left(PENDING_POLL)),
                // Wake for the relay, or to look at ssh and the deadline:
                // often while a deadline is pending, rarely during a long
                // call.
                None => match self
                    .events
                    .recv_timeout(left(if awaiting_hello || cancellable {
                        PENDING_POLL
                    } else {
                        RUNNING_POLL
                    })) {
                    // The remote tendr is running, and what it answered is
                    // on stdout even if ssh has exited since: report the
                    // connection before looking at the exit.
                    Ok(Relay::Hello) => {
                        self.hello = true;
                        continue;
                    }
                    Ok(Relay::Done(done)) => self.ended = Some(done),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        self.ended = Some(Ended::default());
                    }
                },
            }
            if let Some(status) = child.try_wait().map_err(SshError::SpawnFailed)? {
                // The relay reads stderr to its end, so it waits for every
                // holder of it, not just ssh. After the hello that is the
                // command's own output, kept as long as it lasts (a
                // ProxyCommand that ignores ssh's SIGHUP holds the call open,
                // as it held the terminal before), or until the limit.
                // Without a hello, a leftover holder must not hold the call
                // open: wait briefly for the rest, then report the status.
                let ended = match self.ended {
                    Some(ended) => Some(ended),
                    None if self.hello => wait_relay(
                        &self.events,
                        &mut self.hello,
                        limit.map(|limit| limit.saturating_duration_since(Instant::now())),
                    ),
                    None => wait_relay(&self.events, &mut self.hello, Some(Duration::from_secs(1))),
                };
                // A hello relayed only after ssh's exit still proves the
                // remote tendr ran, and its answer, if it wrote one, is on
                // stdout to read.
                if matches!(until, Until::Connected) && self.hello {
                    self.ended = Some(ended.unwrap_or_default());
                    return Ok(Progress::Connected);
                }
                return Ok(self.exited(host, status, ended.unwrap_or_default()));
            }
            if let Some(deadline) = self
                .deadline
                .filter(|d| !self.hello && self.started.elapsed() >= *d)
            {
                stop(child);
                // The relay is not waited for: a lingering ProxyCommand may
                // hold stderr open, and only an unfinished pre-hello line is
                // lost.
                return Err(SshError::ConnectDeadline {
                    host: host.to_owned(),
                    seconds: deadline.as_secs_f64(),
                });
            }
        }
    }

    /// ssh exited with `status` after stderr `ended`.
    fn exited(&self, host: &str, status: std::process::ExitStatus, ended: Ended) -> Progress {
        // An old tendr writes no trailer; its parse error fails the call
        // (exit 2, or 1 under a PowerShell login shell). Every remote from
        // REMOTE_FLOOR on takes the frame and both flags.
        if ended.old_tendr && ended.trailer.is_none() && status.code() != Some(0) {
            eprintln!(
                "tendr on {host} is too old for this client: upgrade it to {REMOTE_FLOOR} or later"
            );
        }
        Progress::Exited(status, ended.trailer)
    }
}

/// The oldest remote tendr a `--host` call works with: the first with the
/// remote frame (`_remote`), `--hello` and `--exit-trailer`. Reconnecting
/// and watching need newer ones (`docs/guide.md`, "Reach remote hosts with
/// `--host`").
const REMOTE_FLOOR: &str = "0.5.0";

/// How often a framed call looks at ssh while the connect deadline is pending.
const PENDING_POLL: Duration = Duration::from_millis(20);

/// How often a framed call looks at ssh once no deadline is pending: it wakes
/// at once for stderr ending, which is how a call normally ends.
const RUNNING_POLL: Duration = Duration::from_secs(1);

/// Whether a stderr line is clap's parse error from a remote tendr too old
/// for the call: one that does not know `_remote`, `--hello` or
/// `--exit-trailer`. The whole of clap's phrase is matched, so a newer
/// tendr's own error that quotes a user value such as `'--hello'` is not
/// mistaken for one.
fn is_old_tendr_error(line: &[u8]) -> bool {
    let line = String::from_utf8_lossy(line);
    [
        "error: unrecognized subcommand '_remote'",
        "error: unexpected argument '--hello' found",
        "error: unexpected argument '--exit-trailer' found",
    ]
    .iter()
    .any(|phrase| line.contains(phrase))
}

/// The last stderr line of a remote `tendr --exit-trailer …` is this, then
/// the exit code and a newline.
pub const EXIT_TRAILER: &str = "tendr-exit ";

/// The exit code in `line` if it is exactly an exit trailer:
/// `tendr-exit <code>`, then `\n` or `\r\n`.
fn trailer_code(line: &[u8]) -> Option<i32> {
    let line = line.strip_suffix(b"\n")?;
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let code = line.strip_prefix(EXIT_TRAILER.as_bytes())?;
    let digits = code.strip_prefix(b"-").unwrap_or(code);
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(code).ok()?.parse().ok()
}

/// Whether `tail`, the unfinished last line of what has arrived so far,
/// could still turn out to be an exit trailer.
fn could_be_trailer(tail: &[u8]) -> bool {
    let prefix = EXIT_TRAILER.as_bytes();
    if tail.len() <= prefix.len() {
        return prefix.starts_with(tail);
    }
    let Some(code) = tail.strip_prefix(prefix) else {
        return false;
    };
    let code = code.strip_suffix(b"\r").unwrap_or(code);
    let digits = code.strip_prefix(b"-").unwrap_or(code);
    digits.len() <= 11 && digits.iter().all(u8::is_ascii_digit)
}

/// Passes stderr through to `out`, holding back only what may be the exit
/// trailer: the last line, while it reads `tendr-exit <code>` so far. A
/// trailer-shaped line counts only if it is the last line of the stream; one
/// with anything after it is passed through unchanged. A partial line that
/// cannot be a trailer goes out at once, so progress output still streams.
struct TrailerFilter<W> {
    out: W,
    held: Vec<u8>,
}

impl<W: io::Write> TrailerFilter<W> {
    fn new(out: W) -> Self {
        Self {
            out,
            held: Vec::new(),
        }
    }

    /// Pass on `data`, keeping back a possible trailer at its end.
    fn write(&mut self, data: &[u8]) {
        self.held.extend_from_slice(data);
        // The last line: after the last newline, or, if what has arrived ends
        // with a newline, the complete line that newline ends.
        let body = self.held.strip_suffix(b"\n").unwrap_or(&self.held);
        let start = body.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        let last = &self.held[start..];
        let keep = if last.ends_with(b"\n") {
            trailer_code(last).is_some()
        } else {
            could_be_trailer(last)
        };
        let send = if keep { start } else { self.held.len() };
        if send > 0 {
            let _ = self.out.write_all(&self.held[..send]);
            let _ = self.out.flush();
            self.held.drain(..send);
        }
    }

    /// The stream has ended: strip a held trailer and return its code, or
    /// pass on what was held.
    fn finish(mut self) -> (W, Option<i32>) {
        let code = trailer_code(&self.held);
        if code.is_none() && !self.held.is_empty() {
            let _ = self.out.write_all(&self.held);
            let _ = self.out.flush();
        }
        (self.out, code)
    }
}

/// What the stderr relay reports.
enum Relay {
    /// The remote tendr's hello arrived.
    Hello,
    /// Stderr ended.
    Done(Ended),
}

/// How a framed call's stderr ended.
#[derive(Debug, Clone, Copy, Default)]
struct Ended {
    /// Before any hello, the remote rejected the call as a tendr too old for
    /// it.
    old_tendr: bool,
    /// The remote's exit trailer.
    trailer: Option<i32>,
}

/// Wait for the relay to finish, at most `limit` (forever with `None`),
/// noting a `hello` relayed meanwhile. Returns how stderr ended, or `None`
/// if it did not finish.
fn wait_relay(
    events: &mpsc::Receiver<Relay>,
    hello: &mut bool,
    limit: Option<Duration>,
) -> Option<Ended> {
    let until = limit.map(|limit| Instant::now() + limit);
    loop {
        let event = match until {
            None => events.recv().ok()?,
            Some(until) => events
                .recv_timeout(until.saturating_duration_since(Instant::now()))
                .ok()?,
        };
        match event {
            Relay::Hello => *hello = true,
            Relay::Done(ended) => return Some(ended),
        }
    }
}

/// Stop an ssh that missed the connect deadline: SIGTERM first, so ssh can
/// restore a terminal it left mid-prompt and hang up its ProxyCommand, then
/// SIGKILL if it is still there after a second. Windows has no SIGTERM: ssh
/// is terminated at once, and an attach client restores the console it
/// shared with ssh itself.
fn stop(child: &mut std::process::Child) {
    // An ssh already reaped (a Watch that saw it exit after its hello) has
    // nothing to stop, and its pid may have been reused.
    if let Ok(Some(_)) = child.try_wait() {
        return;
    }
    #[cfg(unix)]
    {
        if let Ok(pid) = i32::try_from(child.id()) {
            // SAFETY: `pid` is our own child, unreaped as just checked, so it
            // cannot have been reused; SIGTERM has no memory-safety
            // implications.
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

/// Copy a framed call's stderr to `out` through a [`TrailerFilter`],
/// dropping the hello and reporting it. The hello ends a line but need not
/// start one: anything a login shell's rc file wrote without a newline just
/// before tendr ran is glued in front, and is passed through. Reports
/// [`Relay::Done`] when stderr ends.
fn relay_stderr(
    stderr: std::process::ChildStderr,
    events: mpsc::Sender<Relay>,
    out: Box<dyn io::Write + Send>,
) {
    use std::io::BufRead;

    let mut reader = io::BufReader::new(stderr);
    let mut out = TrailerFilter::new(out);
    let mut old_tendr = false;
    let mut line = Vec::new();
    // Before the hello, line by line: the hello and an old tendr's parse
    // error are whole lines. Without a deadline no hello comes, and the whole
    // stream is read this way.
    let hello = loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break false,
            Ok(_) => {}
        }
        let body = line.trim_ascii_end();
        if body.ends_with(HELLO_LINE.as_bytes()) {
            let prefix = &body[..body.len() - HELLO_LINE.len()];
            if !prefix.is_empty() {
                out.write(prefix);
                out.write(b"\n");
            }
            let _ = events.send(Relay::Hello);
            break true;
        }
        old_tendr |= is_old_tendr_error(&line);
        out.write(&line);
    };
    // After the hello the stream is the command's own: pass it on as it
    // comes.
    if hello {
        loop {
            let n = match reader.fill_buf() {
                Ok([]) | Err(_) => break,
                Ok(chunk) => {
                    out.write(chunk);
                    chunk.len()
                }
            };
            reader.consume(n);
        }
    }
    let (_, trailer) = out.finish();
    let _ = events.send(Relay::Done(Ended { old_tendr, trailer }));
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
        let cmd = build_ssh_frame_command("user@box", Entry::Exec, Streams::Inherit).unwrap();
        assert_eq!(
            argv(&cmd),
            [
                "-T",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=5",
                "-o",
                "ServerAliveCountMax=3",
                "user@box",
                "tendr",
                "--hello",
                "--exit-trailer",
                "exec",
                "--frame-from-stdin"
            ]
        );
    }

    /// Every operation but exec runs this one argv; the request is not an
    /// input to it. A Unix client's `attach` adds only its own connection
    /// (no ControlMaster), and its reconnect `BatchMode=yes`, so it never
    /// prompts.
    #[test]
    fn remote_frame_argv_is_constant() {
        let remote = [
            "-T",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ServerAliveInterval=5",
            "-o",
            "ServerAliveCountMax=3",
            "user@box",
            "tendr",
            "--hello",
            "--exit-trailer",
            "_remote",
        ];
        let cmd = build_ssh_frame_command("user@box", Entry::Remote, Streams::Request).unwrap();
        assert_eq!(argv(&cmd), remote);
        let mut attach = remote.to_vec();
        attach.splice(7..7, ["-o", "ControlMaster=no", "-o", "ControlPath=none"]);
        let cmd = build_ssh_frame_command(
            "user@box",
            Entry::Remote,
            Streams::Attach(Login::Interactive),
        )
        .unwrap();
        assert_eq!(argv(&cmd), attach, "an attach");
        let mut batch = attach.clone();
        batch.splice(11..11, ["-o", "BatchMode=yes"]);
        let cmd = build_ssh_frame_command("user@box", Entry::Remote, Streams::Attach(Login::Batch))
            .unwrap();
        assert_eq!(argv(&cmd), batch, "a reconnect");
    }

    #[test]
    fn old_tendr_is_recognised_by_its_parse_error() {
        for line in [
            "error: unrecognized subcommand '_remote'\n",
            "error: unexpected argument '--hello' found\n",
            "error: unexpected argument '--exit-trailer' found\n",
        ] {
            assert!(is_old_tendr_error(line.as_bytes()), "{line}");
        }
        for line in [
            "session not found: x\n",
            "session not found: '--hello'\n",
            "invalid --since: '--hello'\n",
            "invalid --since: '--exit-trailer'\n",
            "session not found: unrecognized subcommand '_remote'\n",
        ] {
            assert!(!is_old_tendr_error(line.as_bytes()), "{line}");
        }
    }

    /// Feed `chunks` through a trailer filter; what it wrote, and the code of
    /// the trailer it stripped.
    fn filtered(chunks: &[&[u8]]) -> (String, Option<i32>) {
        let mut filter = TrailerFilter::new(Vec::new());
        for chunk in chunks {
            filter.write(chunk);
        }
        let (out, code) = filter.finish();
        (String::from_utf8(out).unwrap(), code)
    }

    #[test]
    fn the_last_line_trailer_is_stripped_and_read() {
        assert_eq!(
            filtered(&[b"out\n", b"tendr-exit 42\n"]),
            ("out\n".to_owned(), Some(42))
        );
        assert_eq!(
            filtered(&[b"out\ntendr-ex", b"it 4", b"2\n"]),
            ("out\n".to_owned(), Some(42)),
            "split across reads"
        );
        assert_eq!(
            filtered(&[b"tendr-exit -1073741819\r\n"]),
            (String::new(), Some(-1_073_741_819)),
            "a Windows status, CRLF"
        );
        assert_eq!(filtered(&[b"tendr-exit 0\n"]), (String::new(), Some(0)));
    }

    #[test]
    fn a_trailer_shaped_line_mid_stream_is_passed_through() {
        assert_eq!(
            filtered(&[b"tendr-exit 3\n", b"later\n"]),
            ("tendr-exit 3\nlater\n".to_owned(), None)
        );
        assert_eq!(
            filtered(&[b"tendr-exit 3\ntendr-exit 4\n"]),
            ("tendr-exit 3\n".to_owned(), Some(4)),
            "only the last one counts"
        );
        assert_eq!(
            filtered(&[b"tendr-exit 3\n", b"partial"]),
            ("tendr-exit 3\npartial".to_owned(), None)
        );
    }

    #[test]
    fn without_a_trailer_everything_is_passed_through() {
        for (chunks, expected) in [
            (&[&b"a"[..], b"b\n"][..], "ab\n"),
            (&[b"tendr-exit 42"], "tendr-exit 42"),
            (&[b"tendr-exit 42 x\n"], "tendr-exit 42 x\n"),
            (&[b"tendr-exit \n"], "tendr-exit \n"),
            (&[b"tendr-exit 99999999999\n"], "tendr-exit 99999999999\n"),
            (&[b"xtendr-exit 1\n"], "xtendr-exit 1\n"),
            (&[b""], ""),
        ] {
            assert_eq!(filtered(chunks), (expected.to_owned(), None), "{chunks:?}");
        }
    }

    /// A partial line goes out as soon as it cannot be a trailer, so progress
    /// output without a newline still streams.
    #[test]
    fn a_partial_line_is_not_held_back() {
        let mut filter = TrailerFilter::new(Vec::new());
        filter.write(b"progress 50%");
        assert_eq!(filter.out, b"progress 50%");
        filter.write(b"\ntendr-");
        assert_eq!(filter.out, b"progress 50%\n", "a possible trailer waits");
        filter.write(b"x");
        assert_eq!(filter.out, b"progress 50%\ntendr-x");
    }

    #[test]
    fn the_trailer_decides_the_exit_code() {
        for windows in [false, true] {
            for call in [Call::Operation, Call::Attach] {
                let read = |code, trailer| classify(code, trailer, call, windows);
                assert_eq!(read(Some(1), Some(42)).unwrap(), 42);
                assert_eq!(read(Some(1), Some(1)).unwrap(), 1, "a remote 1");
                assert_eq!(read(Some(255), Some(255)).unwrap(), 255, "a remote 255");
                assert!(matches!(
                    read(Some(255), None),
                    Err(SshError::TransportFailed)
                ));
                assert_eq!(read(Some(3), None).unwrap(), 3);
                assert!(matches!(read(None, None), Err(SshError::Abnormal)));
            }
        }
    }

    /// An NTSTATUS exit (the severity bits set) is ssh.exe ended by Windows,
    /// as a signal ends it on Unix: Ctrl-C or Ctrl-Break, a crash, a debugger.
    #[test]
    fn an_ntstatus_exit_on_windows_is_abnormal() {
        for status in [0xC000_013A_u32, 0xC000_0005, 0x8000_0003, 0x4001_0004] {
            let code = status as i32;
            for call in [Call::Operation, Call::Attach] {
                assert!(
                    matches!(
                        classify(Some(code), None, call, true),
                        Err(SshError::Abnormal)
                    ),
                    "{status:#x}"
                );
                assert_eq!(
                    classify(Some(code), Some(7), call, true).unwrap(),
                    7,
                    "the trailer still decides"
                );
            }
        }
    }

    /// A remote tendr always writes a trailer, so ssh.exe's 1 without one is
    /// ssh's own end (`TerminateProcess` gives 1): for an attach, a lost
    /// link. Other calls, and Unix clients, keep the code.
    #[test]
    fn exit_1_without_a_trailer_is_a_lost_attach_on_windows_only() {
        assert!(matches!(
            classify(Some(1), None, Call::Attach, true),
            Err(SshError::Abnormal)
        ));
        assert_eq!(classify(Some(1), None, Call::Operation, true).unwrap(), 1);
        assert_eq!(classify(Some(1), None, Call::Attach, false).unwrap(), 1);
        assert_eq!(classify(Some(1), None, Call::Operation, false).unwrap(), 1);
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
        for entry in [Entry::Exec, Entry::Remote] {
            for host in ["-oProxyCommand=x", ""] {
                assert!(build_ssh_frame_command(host, entry, Streams::Request).is_err());
            }
            assert!(build_ssh_frame_command("user@box", entry, Streams::Request).is_ok());
        }
    }
}
