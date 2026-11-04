//! `attach --host` from a Unix client, end to end through the attach bridge.
//! The fake ssh (`src/bin/fake_ssh.rs`) writes down its argv, then acts as
//! the remote login shell and runs the real tendr, whose `_remote` entry
//! point bridges the attach protocol to the session's socket. Both sides
//! share one HOME. The local tendr runs in a terminal, as a person's would:
//! raw mode, resizes and the detach escape are all on its side.
#![cfg(unix)]

mod harness;
use harness::Home;

use harness::terminal::{WrappedAttach, tendr_bin};
use harness::{HANG, Observation, fake_ssh, poll_until, shim_path_env, tendr};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

/// The ssh argv of an attach: every `--host` operation's, plus a connection
/// of its own (no ControlMaster sharing).
const ATTACH_ARGV: [&str; 16] = [
    "-T",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "ServerAliveInterval=5",
    "-o",
    "ServerAliveCountMax=3",
    "-o",
    "ControlMaster=no",
    "-o",
    "ControlPath=none",
    "box",
    "tendr",
    "--hello",
    "--exit-trailer",
    "_remote",
];

/// A session name with shell metacharacters: no plain-name rule applies.
const HOSTILE_NAME: &str = "x&calc;$(id)`'\u{e9}";

/// The connect deadline (`TENDR_SSH_CONNECT_TIMEOUT`, seconds) of a call
/// that is expected to connect: far past anything load can do to a remote
/// tendr's start, so it never fires in a test that is not about it.
const CONNECTS: &str = "600";

/// A product grace, in milliseconds, that must not fire in the tests that
/// set it: far past anything load can do to the party it waits for.
const NEVER_MS: &str = "600000";

/// For a run whose end must not cut a reading viewer off: the sidecar waits
/// for its viewers to be sent the last of its output (`RUN_END_FLUSH`) for
/// longer than load can hold up a viewer that is reading.
const FLUSH_ALL: (&str, &str) = ("TENDR_TEST_RUN_END_FLUSH_MS", NEVER_MS);

/// For a watcher whose remote goes by `meta.json` once the session closed
/// its connection (a sidecar from before the end of output): the remote
/// waits for the run to record its end (`RUN_END_GRACE`) for longer than
/// load can delay that record.
const RECORDED_END: (&str, &str) = ("TENDR_TEST_RUN_END_GRACE_MS", NEVER_MS);

/// For every `--host` attach client: once the session has closed the
/// connection, ssh's exit (the remote's verdict) is waited for
/// (`SSH_CLOSE_GRACE`) longer than load can delay it, so a slow remote is
/// never taken for a lost link and reconnected.
const SSH_CLOSES: (&str, &str) = ("TENDR_TEST_SSH_CLOSE_GRACE_MS", NEVER_MS);

/// A fake ssh that records its argv, then forwards to the real tendr.
struct Remote {
    ssh: TempDir,
    _argv_dir: TempDir,
    argv: PathBuf,
}

impl Remote {
    fn new() -> Self {
        let argv_dir = TempDir::new().unwrap();
        let argv = argv_dir.path().join("argv");
        let ssh = fake_ssh(&format!("args {}\nforward", argv.display()));
        Self {
            ssh,
            _argv_dir: argv_dir,
            argv,
        }
    }

    /// `tendr --host box attach <args>` in a fresh terminal.
    fn attach(&self, root: &Home, label: &str, args: &[&str]) -> WrappedAttach {
        attach_through(&self.ssh, CONNECTS, root, label, args, &[])
    }
}

/// `tendr --host box attach <args>` in a fresh terminal, through the fake ssh
/// in `ssh`, with a connect deadline of `deadline` seconds and `env` added.
fn attach_through(
    ssh: &TempDir,
    deadline: &str,
    root: &Home,
    label: &str,
    args: &[&str],
    env: &[(&str, &str)],
) -> WrappedAttach {
    let argv: Vec<String> = [tendr_bin(), "--host".into(), "box".into(), "attach".into()]
        .into_iter()
        .chain(args.iter().map(|a| (*a).to_owned()))
        .collect();
    let path = shim_path_env(ssh);
    let tendr = tendr_bin();
    let mut all = vec![
        ("PATH", path.to_str().unwrap()),
        ("TENDR_TEST_BIN", tendr.as_str()),
        ("TENDR_SSH_CONNECT_TIMEOUT", deadline),
        SSH_CLOSES,
    ];
    all.extend_from_slice(env);
    WrappedAttach::spawn_argv_env(root, label, &argv, &all)
}

impl Remote {
    /// The last call through this remote ran the constant ssh argv.
    fn assert_constant_argv(&self) {
        let recorded = std::fs::read_to_string(&self.argv).unwrap();
        let argv: Vec<&str> = recorded
            .lines()
            .filter_map(|l| l.strip_prefix("ARG:"))
            .collect();
        assert_eq!(argv, ATTACH_ARGV);
    }
}

fn start_pty(root: &Home, session: &str, argv: &[&str]) {
    let output = tendr(root)
        .args(["start", session, "--pty", "--stdin", "--"])
        .args(argv)
        .output()
        .unwrap();
    assert!(output.status.success(), "start {session}: {output:?}");
    harness::wait_running(root, session);
}

fn control(root: &Home, session: &str) -> Option<String> {
    let meta = root
        .path()
        .join(format!(".tendr/sessions/default/{session}/meta.json"));
    let meta: serde_json::Value = serde_json::from_slice(&std::fs::read(meta).ok()?).ok()?;
    meta["pty"]["control"].as_str().map(str::to_owned)
}

fn wait_control(root: &Home, session: &str, expected: &str) {
    poll_until(HANG, Duration::from_millis(50), || {
        match control(root, session) {
            Some(c) if c == expected => Observation::Ready(()),
            other => Observation::Pending(format!("pty.control={other:?}")),
        }
    })
    .unwrap_or_else(|e| panic!("{session} never reached pty.control={expected}: {e}"));
}

/// Wait until `session`'s recording (`output.log`) has grown by `bytes`
/// from now. Beside a terminal that stopped reading, output that far past
/// what it was sent has filled every buffer on the way (the terminal's,
/// ssh's pipe, the remote side's socket) and waits in the sidecar's ring,
/// so the sidecar's write to this client is blocked. Waiting on the bytes,
/// not a time, keeps that true however slowly the flood runs.
fn wait_recorded_past(root: &Home, session: &str, bytes: u64) {
    let log = root
        .path()
        .join(format!(".tendr/sessions/default/{session}/output.log"));
    let size = || std::fs::metadata(&log).map_or(0, |m| m.len());
    let from = size();
    poll_until(HANG, Duration::from_millis(50), || {
        let now = size();
        if now >= from + bytes {
            Observation::Ready(())
        } else {
            Observation::Pending(format!(
                "{} of {bytes} bytes recorded",
                now.saturating_sub(from)
            ))
        }
    })
    .unwrap_or_else(|e| panic!("{session} never recorded {bytes} more bytes: {e}"));
}

/// Output past every buffer between the sidecar and a stalled terminal: a
/// few hundred KiB at most, and far inside the sidecar's 32 MiB ring.
const BACKLOG: u64 = 2 << 20;

/// Type into the session and see its output, resize it, then detach with the
/// escape: the terminal comes back as it was and the session keeps running
/// under agent control. The session's name would not survive a remote shell.
#[test]
fn host_attach_types_sees_output_resizes_and_detaches() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start_pty(&root, HOSTILE_NAME, &["sh"]);
    let remote = Remote::new();

    let mut wrapped = remote.attach(&root, "basic", &[HOSTILE_NAME]);
    wrapped
        .cli
        .type_until_logged(&root, HOSTILE_NAME, "echo host-$((40+2))\n");
    wrapped.wait_output_contains("host-42");
    wait_control(&root, HOSTILE_NAME, "HumanControl");
    remote.assert_constant_argv();

    // SIGWINCH on this side becomes a resize frame on the remote side.
    wrapped.cli.resize(33, 111);
    poll_until(HANG, Duration::from_millis(300), || {
        wrapped.cli.type_bytes(b"stty size\n");
        if wrapped.cli.output().contains("33 111") {
            Observation::Ready(())
        } else {
            Observation::Pending("the session has not seen 33 111".into())
        }
    })
    .unwrap_or_else(|e| panic!("the resize never arrived ({e}): {}", wrapped.cli.output()));

    wrapped.cli.type_bytes(b"\x1cd");
    assert_eq!(wrapped.wait_exit(), 0, "{}", wrapped.cli.output());
    wrapped.assert_terminal_restored();
    wait_control(&root, HOSTILE_NAME, "AgentControl");
}

/// A paced flood: at most 800 KB/s, so a stall's [`BACKLOG`] stays well
/// inside the sidecar's 32 MiB output ring and the client is never dropped
/// for falling behind, yet fills every buffer between the sidecar and a
/// terminal that stopped reading.
const PACED_FLOOD: &str = "stty raw -echo; line=$(printf '%01000d' 0); \
     while :; do i=0; while [ $i -lt 8 ]; do printf '%s' \"$line\"; i=$((i+1)); done; sleep 0.01; done";

/// A client whose terminal stopped reading still detaches at once: its
/// detach does not wait behind the output it has not read.
#[test]
fn host_detach_is_not_held_up_by_unread_output() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start_pty(&root, "host-stall-detach", &["sh", "-c", PACED_FLOOD]);
    let remote = Remote::new();

    let mut wrapped = remote.attach(&root, "stall-detach", &["host-stall-detach"]);
    wrapped.wait_output_contains("0000000000");
    wait_control(&root, "host-stall-detach", "HumanControl");
    wrapped.cli.pause_output();
    wait_recorded_past(&root, "host-stall-detach", BACKLOG);
    assert_eq!(
        control(&root, "host-stall-detach").as_deref(),
        Some("HumanControl"),
        "the stalled client still holds control"
    );

    wrapped.cli.type_bytes(b"\x1cd");
    wait_control(&root, "host-stall-detach", "AgentControl");
    let code = wrapped.wait_exit();
    wrapped.cli.resume_output();
    assert_eq!(code, 0);
    wrapped.assert_terminal_restored();
}

/// A second client takes over while the first has stopped reading with
/// output queued for it: the takeover goes through at once, and the first
/// learns of it, exiting 81, only when its terminal reads again. Its output
/// is backed up all the way to the sidecar, so the sidecar cannot send it
/// `MSG_RETIRED`; the remote side reads the takeover from the event log.
#[test]
fn host_takeover_while_the_first_client_is_stalled() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    // The paced flood in the background, and an echo of what is typed.
    let child = format!("({PACED_FLOOD}) & exec cat -v");
    start_pty(&root, "host-take", &["sh", "-c", &child]);
    let remote = Remote::new();

    let first = remote.attach(&root, "take-1", &["host-take"]);
    first.wait_output_contains("0000000000");
    wait_control(&root, "host-take", "HumanControl");
    // The terminal stops reading; the flood fills its buffer, ssh's pipe,
    // the remote side's socket and the sidecar's write to it.
    first.cli.pause_output();
    wait_recorded_past(&root, "host-take", BACKLOG);
    assert_eq!(
        control(&root, "host-take").as_deref(),
        Some("HumanControl"),
        "the stalled client still holds control"
    );

    let mut second = remote.attach(&root, "take-2", &["host-take", "--takeover"]);
    second
        .cli
        .type_until_logged(&root, "host-take", "second-client");
    assert!(
        !first.exited(),
        "a client that is not reading stays blocked"
    );

    first.cli.resume_output();
    assert_eq!(first.wait_exit(), 81, "the stalled client was taken over");
    first.assert_terminal_restored();
    first.wait_output_contains("another client took over this session");

    second.cli.type_bytes(b"\x1cd");
    assert_eq!(second.wait_exit(), 0);
    second.assert_terminal_restored();
    wait_control(&root, "host-take", "AgentControl");
}

/// When a [`fake_sidecar`] replies.
#[derive(Clone, Copy)]
enum FakeReply {
    /// After reading the hello.
    AfterHello,
    /// On accepting, before reading anything, then closing.
    BeforeHello,
}

/// Point `session`'s breadcrumb at a fake sidecar in `root` that answers
/// every connection with `reply`.
fn fake_sidecar(root: &Home, session: &str, label: &str, when: FakeReply, reply: Vec<u8>) {
    use std::io::Write;
    let sock = root.path().join(format!("{label}.sock"));
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().map_while(Result::ok) {
            if matches!(when, FakeReply::AfterHello) {
                let _ = stream.set_read_timeout(Some(HANG));
                if tendr::attach_proto::read_msg(&mut stream).is_err() {
                    continue;
                }
            }
            let _ = stream.write_all(&reply);
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    });
    point_breadcrumb_at(root, session, &sock);
}

fn point_breadcrumb_at(root: &Home, session: &str, sock: &std::path::Path) {
    let dir = root
        .path()
        .join(format!(".tendr/sessions/default/{session}"));
    let tmp = dir.join("a.sock.path.test");
    std::fs::write(&tmp, sock.to_str().unwrap()).unwrap();
    std::fs::rename(&tmp, dir.join("a.sock.path")).unwrap();
}

/// Attach to `args` through the bridge and return its exit code and what its
/// terminal showed; the terminal is checked restored.
fn host_attach_exit(root: &Home, remote: &Remote, label: &str, args: &[&str]) -> (i32, String) {
    let wrapped = remote.attach(root, label, args);
    let code = wrapped.wait_exit();
    wrapped.assert_terminal_restored();
    (code, wrapped.cli.output())
}

/// The remote side's checks and the sidecar's refusals keep the exit codes
/// of a local attach (80, 81, 84, 85, and 1 for a session that cannot be
/// attached at all), with the remote's message on stderr.
#[test]
fn host_attach_keeps_the_exit_codes() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    let remote = Remote::new();

    let (code, out) = host_attach_exit(&root, &remote, "missing", &["no-such"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("session not found"), "{out}");
    remote.assert_constant_argv();

    tendr(&root)
        .args(["start", "host-pipe", "--", "sleep", "3600"])
        .output()
        .unwrap();
    harness::wait_running(&root, "host-pipe");
    let (code, out) = host_attach_exit(&root, &remote, "pipe", &["host-pipe"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("not PTY-enabled"), "{out}");

    tendr(&root)
        .args(["start", "host-ended", "--pty", "--", "true"])
        .output()
        .unwrap();
    harness::wait_terminal(&root, "host-ended");
    let (code, out) = host_attach_exit(&root, &remote, "ended", &["host-ended"]);
    assert_eq!(code, 81, "{out}");

    // Held by a human: the remote check refuses a plain attach.
    start_pty(&root, "host-held", &["cat"]);
    let holder = remote.attach(&root, "holder", &["host-held"]);
    wait_control(&root, "host-held", "HumanControl");
    let (code, out) = host_attach_exit(&root, &remote, "held", &["host-held"]);
    assert_eq!(code, 81, "{out}");
    assert!(out.contains("human control"), "{out}");
    // A signal ends a bridged attach as it ends a local one: exit 1.
    holder.signal(libc::SIGTERM);
    assert_eq!(holder.wait_exit(), 1);
    holder.assert_terminal_restored();
    wait_control(&root, "host-held", "AgentControl");

    // A breadcrumb naming a socket nobody listens on.
    start_pty(&root, "host-dead", &["cat"]);
    let dead = root.path().join("dead.sock");
    drop(std::os::unix::net::UnixListener::bind(&dead).unwrap());
    point_breadcrumb_at(&root, "host-dead", &dead);
    let (code, out) = host_attach_exit(&root, &remote, "dead", &["host-dead"]);
    assert_eq!(code, 84, "{out}");

    // A sidecar that speaks another protocol, and one that refuses the peer.
    start_pty(&root, "host-fake", &["cat"]);
    let mut unknown = Vec::new();
    tendr::attach_proto::write_msg(&mut unknown, 0x7f, b"from the future").unwrap();
    fake_sidecar(&root, "host-fake", "future", FakeReply::AfterHello, unknown);
    let (code, out) = host_attach_exit(&root, &remote, "future", &["host-fake"]);
    assert_eq!(code, 80, "{out}");

    let mut refusal = Vec::new();
    tendr::attach_proto::write_frame(
        &mut refusal,
        &tendr::attach_proto::Frame::Rejected {
            class: tendr::attach_proto::RejectClass::Identity,
            reason: "peer identity rejected".to_owned(),
        },
    )
    .unwrap();
    fake_sidecar(
        &root,
        "host-fake",
        "identity",
        FakeReply::BeforeHello,
        refusal,
    );
    let (code, out) = host_attach_exit(&root, &remote, "identity", &["host-fake"]);
    assert_eq!(code, 85, "{out}");
}

/// The remote side takes the attach protocol only after the hello its
/// request names; anything else is a protocol failure, and the session keeps
/// its controller.
#[test]
fn remote_attach_requires_its_hello() {
    use tendr::attach_proto::{Frame, Mode, Version, write_frame};
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start_pty(&root, "host-hello", &["cat"]);

    let json = br#"{"v":1,"op":"attach","params":{"session":"host-hello","namespace":null,"takeover":false,"escape":"ctrl-backslash"}}"#;
    let mut request = u32::try_from(json.len()).unwrap().to_be_bytes().to_vec();
    request.extend_from_slice(json);
    let mut wrong_mode = request.clone();
    write_frame(
        &mut wrong_mode,
        &Frame::Hello {
            version: Version::V1,
            mode: Mode::Takeover,
        },
    )
    .unwrap();
    let mut not_a_hello = request.clone();
    write_frame(&mut not_a_hello, &Frame::Data(b"x".to_vec())).unwrap();
    for (what, stdin) in [
        ("no hello", request),
        ("another mode", wrong_mode),
        ("not a hello", not_a_hello),
    ] {
        let output = tendr(&root)
            .arg("_remote")
            .write_stdin(stdin)
            .timeout(HANG)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(80), "{what}: {output:?}");
        assert!(output.stdout.is_empty(), "{what}: no frame is sent");
    }
    assert_eq!(
        control(&root, "host-hello").as_deref(),
        Some("AgentControl")
    );
}

/// The ssh failures keep the codes of every other `--host` command: a remote
/// tendr that never says hello is stopped at the connect deadline (82), and
/// ssh's own 255 without an exit trailer is a connection failure (1).
#[test]
fn host_attach_ssh_failures_keep_their_codes() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

    // The fake ssh never says hello by itself: only the deadline ends it.
    let silent = fake_ssh("sleep 3600");
    let wrapped = attach_through(&silent, "1", &root, "deadline", &["s"], &[]);
    assert_eq!(wrapped.wait_exit(), 82);
    wrapped.assert_terminal_restored();
    wrapped.wait_output_contains("did not reach the remote tendr");

    let lost = fake_ssh(&format!("err {}\\n\nexit 255", tendr::ssh::HELLO_LINE));
    let wrapped = attach_through(&lost, CONNECTS, &root, "lost", &["s"], &[]);
    assert_eq!(wrapped.wait_exit(), 1);
    wrapped.assert_terminal_restored();
    wrapped.wait_output_contains("ssh transport failed");
}

/// A client that vanishes without detaching (its tendr killed outright)
/// gives the terminal back to the agent: the remote side sees its input end
/// and closes the connection.
#[test]
fn host_attach_client_killed_gives_control_back() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start_pty(&root, "host-killed", &["cat"]);
    let remote = Remote::new();

    let mut wrapped = remote.attach(&root, "killed", &["host-killed"]);
    wrapped
        .cli
        .type_until_logged(&root, "host-killed", "before-kill");
    wait_control(&root, "host-killed", "HumanControl");
    wrapped.signal(libc::SIGKILL);
    wait_control(&root, "host-killed", "AgentControl");
}

/// The hello payloads (`[version, mode]`) a fake sidecar was sent, in order.
type Hellos = std::sync::Arc<Mutex<Vec<Vec<u8>>>>;

/// Point `session`'s breadcrumb at a socket in `root` whose every connection,
/// after its hello (recorded in the returned list), goes to `serve`.
fn fake_listener(
    root: &Home,
    session: &str,
    label: &str,
    serve: impl Fn(std::os::unix::net::UnixStream, &[u8]) + Send + 'static,
) -> Hellos {
    let sock = root.path().join(format!("{label}.sock"));
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let hellos = Hellos::default();
    let seen = std::sync::Arc::clone(&hellos);
    std::thread::spawn(move || {
        for mut stream in listener.incoming().map_while(Result::ok) {
            let _ = stream.set_read_timeout(Some(HANG));
            let Ok((_, hello)) = tendr::attach_proto::read_msg(&mut stream) else {
                continue;
            };
            let _ = stream.set_read_timeout(None);
            seen.lock().unwrap().push(hello.clone());
            serve(stream, &hello);
        }
    });
    point_breadcrumb_at(root, session, &sock);
    hellos
}

/// A sidecar started by tendr 0.5.0, emulated at the frame level: it refuses
/// any hello but `[1, mode]` with the protocol class and the message 0.5.0
/// sends, and serves a version 1 hello as the real sidecar at `real` does,
/// by passing the connection through to it.
fn v1_only_sidecar(root: &Home, session: &str) -> Hellos {
    use std::io::Write;
    use tendr::attach_proto::{Frame, RejectClass, write_frame};
    let real = tendr::attach_proto::read_sock_path(
        &root
            .path()
            .join(format!(".tendr/sessions/default/{session}")),
    )
    .unwrap();
    fake_listener(root, session, "v1-only", move |mut client, hello| {
        if hello[0] != 1 {
            let refusal = Frame::Rejected {
                class: RejectClass::Protocol,
                reason: "unsupported attach protocol version or mode".to_owned(),
            };
            let _ = write_frame(&mut client, &refusal);
            let _ = client.shutdown(std::net::Shutdown::Both);
            return;
        }
        let mut sidecar = std::os::unix::net::UnixStream::connect(&real).unwrap();
        tendr::attach_proto::write_msg(&mut sidecar, tendr::attach_proto::MSG_HELLO, hello)
            .unwrap();
        let (mut from_client, mut to_sidecar) =
            (client.try_clone().unwrap(), sidecar.try_clone().unwrap());
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut from_client, &mut to_sidecar);
            let _ = to_sidecar.shutdown(std::net::Shutdown::Both);
        });
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut sidecar, &mut client);
            let _ = client.flush();
            let _ = client.shutdown(std::net::Shutdown::Both);
        });
    })
}

/// A new remote tendr whose session's sidecar was started by 0.5.0, so it
/// speaks only version 1: the bridge's version 2 hello is refused, it says
/// hello again in version 1, and the attach works as it did, plain output
/// and all.
#[test]
fn host_attach_falls_back_to_v1_for_an_old_sidecar() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start_pty(&root, "host-old-sidecar", &["sh"]);
    let hellos = v1_only_sidecar(&root, "host-old-sidecar");
    let remote = Remote::new();

    let mut wrapped = remote.attach(&root, "old-sidecar", &["host-old-sidecar"]);
    wrapped
        .cli
        .type_until_logged(&root, "host-old-sidecar", "echo old-$((40+2))\n");
    wrapped.wait_output_contains("old-42");
    wait_control(&root, "host-old-sidecar", "HumanControl");
    wrapped.cli.type_bytes(b"\x1cd");
    assert_eq!(wrapped.wait_exit(), 0, "{}", wrapped.cli.output());
    wrapped.assert_terminal_restored();
    wait_control(&root, "host-old-sidecar", "AgentControl");
    assert_eq!(
        *hellos.lock().unwrap(),
        [vec![4, 1], vec![3, 1], vec![2, 1], vec![1, 1]],
        "version 4 first, then each older version once"
    );
}

/// The bridge speaks version 4 to the sidecar and passes its acceptance and
/// stamped output to the client, which writes output that follows on with no
/// gap and refuses output that skips: a protocol failure (80), after which
/// the terminal is restored.
#[test]
fn host_attach_tracks_offsets_and_refuses_output_that_skips() {
    use tendr::attach_proto::{Frame, ResumePoint, write_frame};
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start_pty(&root, "host-skip", &["cat"]);
    let hellos = fake_listener(&root, "host-skip", "skip", |mut client, _| {
        let accept = Frame::AcceptedV2(ResumePoint {
            run_id: tendr::model::ids::RunId::new(),
            epoch: tendr::model::pty_control::ControllerEpoch::new(1),
            offset: 100,
        });
        let output = |offset, bytes: &[u8]| Frame::Output {
            offset,
            bytes: bytes.to_vec(),
        };
        for frame in [
            accept,
            output(100, b"first-"),
            output(106, b"second-"),
            output(200, b"after-the-gap"),
        ] {
            write_frame(&mut client, &frame).unwrap();
        }
        // Held open: only the skip ends the attach.
        let _ = std::io::copy(&mut client, &mut std::io::sink());
    });
    let remote = Remote::new();

    let wrapped = remote.attach(&root, "skip", &["host-skip"]);
    assert_eq!(wrapped.wait_exit(), 80);
    wrapped.assert_terminal_restored();
    let output = wrapped.cli.output();
    assert!(output.contains("first-second-"), "{output}");
    assert!(!output.contains("after-the-gap"), "{output}");
    // The offset it expected: 100, then 6 and 7 bytes written.
    assert!(output.contains("113") && output.contains("200"), "{output}");
    assert_eq!(*hellos.lock().unwrap(), [vec![4, 1]]);
}

/// `tendr _remote` run by a client that speaks the attach protocol itself,
/// at the frame level: `json` is its request, a version 1 hello follows.
struct FrameClient {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: std::process::ChildStdout,
}

impl FrameClient {
    fn start(root: &Home, json: &str) -> Self {
        use tendr::attach_proto::{Frame, Mode, Version};
        Self::start_with(
            root,
            json,
            &Frame::Hello {
                version: Version::V1,
                mode: Mode::Attach,
            },
        )
    }

    /// [`FrameClient::start`], with `first` in place of the hello. A
    /// watcher's remote that goes by `meta.json` waits for the run's end
    /// longer than load can delay it ([`RECORDED_END`]); no test here
    /// cuts a watcher off through this client.
    fn start_with(root: &Home, json: &str, first: &tendr::attach_proto::Frame) -> Self {
        use std::io::Write;
        use std::process::Stdio;
        let mut child = std::process::Command::new(tendr_bin())
            .arg("_remote")
            .env("HOME", root.path())
            .env(RECORDED_END.0, RECORDED_END.1)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut request = remote_request(json);
        tendr::attach_proto::write_frame(&mut request, first).unwrap();
        stdin.write_all(&request).unwrap();
        let stdout = child.stdout.take().unwrap();
        Self {
            child,
            stdin: Some(stdin),
            stdout,
        }
    }

    /// The next frame from the remote side, decoded.
    fn frame(&mut self) -> tendr::attach_proto::Frame {
        let (msg_type, payload) = self.read();
        tendr::attach_proto::Frame::decode(msg_type, payload).expect("a known frame")
    }

    fn read(&mut self) -> (u8, Vec<u8>) {
        tendr::attach_proto::read_msg(&mut self.stdout).expect("the remote side sends a frame")
    }

    /// Detach, and the remote side's exit code.
    fn detach(mut self) -> Option<i32> {
        use tendr::attach_proto::{Frame, write_frame};
        let mut stdin = self.stdin.take().unwrap();
        write_frame(&mut stdin, &Frame::Detach).unwrap();
        drop(stdin);
        self.child.wait().unwrap().code()
    }
}

/// A PTY session that prints `marker` once `go` exists, then waits.
fn start_marker(root: &Home, session: &str, go: &std::path::Path, marker: &str) {
    let script =
        format!("while [ ! -f \"$GO\" ]; do sleep 0.05; done; printf {marker}; sleep 3600");
    let output = tendr(root)
        .args(["start", session, "--pty"])
        .arg("--env")
        .arg(format!("GO={}", go.display()))
        .args(["--", "sh", "-c", &script])
        .output()
        .unwrap();
    assert!(output.status.success(), "start {session}: {output:?}");
    harness::wait_running(root, session);
}

/// A client asking for version 2 gets the sidecar's version 2 acceptance,
/// for this run, and its output stamped from the head offset there.
#[test]
fn remote_attach_passes_v2_through() {
    use tendr::attach_proto::{MSG_ACCEPTED_V2, MSG_OUTPUT};
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    let go = root.path().join("go");
    start_marker(&root, "host-v2", &go, "v2-marker");

    let mut client = FrameClient::start(
        &root,
        r#"{"v":1,"op":"attach","params":{"session":"host-v2","namespace":null,"takeover":false,"escape":"ctrl-backslash","protocol":2}}"#,
    );
    let (msg_type, accept) = client.read();
    assert_eq!(msg_type, MSG_ACCEPTED_V2, "{accept:?}");
    let meta: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            root.path()
                .join(".tendr/sessions/default/host-v2/meta.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let run_id = uuid::Uuid::from_slice(&accept[8..24]).unwrap();
    assert_eq!(run_id.to_string(), meta["run_id"].as_str().unwrap());
    let head = u64::from_be_bytes(accept[24..32].try_into().unwrap());
    assert_eq!(head, 0, "nothing was written before the attach");
    wait_control(&root, "host-v2", "HumanControl");

    std::fs::write(&go, b"").unwrap();
    let mut next = head;
    let mut shown = Vec::new();
    while !shown.ends_with(b"v2-marker") {
        let (msg_type, payload) = client.read();
        assert_eq!(msg_type, MSG_OUTPUT);
        let offset = u64::from_be_bytes(payload[..8].try_into().unwrap());
        assert_eq!(offset, next);
        next += (payload.len() - 8) as u64;
        shown.extend_from_slice(&payload[8..]);
    }
    assert_eq!(shown, b"v2-marker");
    assert_eq!(client.detach(), Some(0));
    wait_control(&root, "host-v2", "AgentControl");
}

/// A 0.5.0 client's request (no `protocol`) and version 1 hello get what
/// 0.5.0's remote tendr sent: the version 1 acceptance with the epoch, then
/// plain output.
#[test]
fn remote_attach_answers_an_old_client_in_v1() {
    use tendr::attach_proto::{MSG_ACCEPTED, MSG_DATA};
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    let go = root.path().join("go");
    start_marker(&root, "host-v1", &go, "v1-marker");

    let mut client = FrameClient::start(
        &root,
        r#"{"v":1,"op":"attach","params":{"session":"host-v1","namespace":null,"takeover":false,"escape":"ctrl-backslash"}}"#,
    );
    assert_eq!(client.read(), (MSG_ACCEPTED, 1u64.to_be_bytes().to_vec()));
    wait_control(&root, "host-v1", "HumanControl");
    std::fs::write(&go, b"").unwrap();
    let mut shown = Vec::new();
    while !shown.ends_with(b"v1-marker") {
        let (msg_type, payload) = client.read();
        assert_eq!(msg_type, MSG_DATA);
        shown.extend_from_slice(&payload);
    }
    assert_eq!(shown, b"v1-marker");
    assert_eq!(client.detach(), Some(0));
    wait_control(&root, "host-v1", "AgentControl");
}

/// What this client sends a 0.5.0 remote tendr is what that remote reads:
/// the request under frame version 1, whose attach fields 0.5.0 knows (it
/// ignores the rest), then the exact version 1 hello 0.5.0 requires. The
/// remote's answers, a version 1 acceptance and plain output, are what
/// [`host_attach_falls_back_to_v1_for_an_old_sidecar`] gives the client.
#[test]
fn host_attach_sends_what_an_old_remote_reads() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    let sent = root.path().join("sent");
    let pid = root.path().join("ssh-pid");
    let ssh = fake_ssh(&format!(
        "pidfile {}\nstdin {}",
        pid.display(),
        sent.display()
    ));
    let wrapped = attach_through(
        &ssh,
        CONNECTS,
        &root,
        "old-remote",
        &["s", "--takeover"],
        &[],
    );
    // The fake ssh never answers. Once it has written down the request and
    // the hello that follows it (7 bytes), the test ends it, not a deadline
    // that could stop it before it had read them: ssh killed before any
    // hello is a failed connection (1).
    poll_until(HANG, Duration::from_millis(20), || {
        let got = std::fs::read(&sent).unwrap_or_default();
        let want = got
            .get(..4)
            .map(|len| 4 + u32::from_be_bytes(len.try_into().unwrap()) as usize + 7);
        match want {
            Some(want) if got.len() >= want => Observation::Ready(()),
            _ => Observation::Pending(format!("{} bytes sent", got.len())),
        }
    })
    .unwrap_or_else(|e| panic!("the request and hello never reached ssh: {e}"));
    let pid: libc::pid_t = std::fs::read_to_string(&pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY: kill(2) takes a pid and a signal number.
    assert_eq!(
        unsafe { libc::kill(pid, libc::SIGKILL) },
        0,
        "kill ssh {pid}"
    );
    assert_eq!(wrapped.wait_exit(), 1);
    wrapped.assert_terminal_restored();

    /// 0.5.0's attach request, field for field.
    #[derive(serde::Deserialize, Debug, PartialEq)]
    struct AttachRequest050 {
        session: String,
        namespace: Option<String>,
        takeover: bool,
        escape: String,
    }
    #[derive(serde::Deserialize)]
    struct Frame050 {
        v: u32,
        op: String,
        params: AttachRequest050,
    }
    let sent = std::fs::read(&sent).unwrap();
    let len = u32::from_be_bytes(sent[..4].try_into().unwrap()) as usize;
    let frame: Frame050 = serde_json::from_slice(&sent[4..4 + len]).unwrap();
    assert_eq!((frame.v, frame.op.as_str()), (1, "attach"));
    assert_eq!(
        frame.params,
        AttachRequest050 {
            session: "s".to_owned(),
            namespace: None,
            takeover: true,
            escape: "ctrl-backslash".to_owned(),
        }
    );
    assert_eq!(
        &sent[4 + len..],
        [tendr::attach_proto::MSG_HELLO, 0, 0, 0, 2, 1, 2],
        "a version 1 takeover hello"
    );
}

/// The remote frame of `json`: its length, then the JSON.
fn remote_request(json: &str) -> Vec<u8> {
    let mut request = u32::try_from(json.len()).unwrap().to_be_bytes().to_vec();
    request.extend_from_slice(json.as_bytes());
    request
}

/// This client's attach request for `session`, resuming from `resume`.
fn attach_json(session: &str, resume: Option<tendr::attach_proto::ResumePoint>) -> String {
    attach_json_in(2, session, resume)
}

/// [`attach_json`], naming attach protocol version `protocol`.
fn attach_json_in(
    protocol: u8,
    session: &str,
    resume: Option<tendr::attach_proto::ResumePoint>,
) -> String {
    let resume = resume.map_or(String::new(), |p| {
        format!(
            r#","resume":{{"run_id":"{}","epoch":{},"offset":{}}}"#,
            p.run_id.as_uuid(),
            p.epoch.get(),
            p.offset
        )
    });
    format!(
        r#"{{"v":1,"op":"attach","params":{{"session":"{session}","namespace":null,"takeover":false,"escape":"ctrl-backslash","protocol":{protocol}{resume}}}}}"#
    )
}

/// A version 2 attach through `_remote`, and the point it was accepted at.
fn attach_v2(root: &Home, session: &str) -> (FrameClient, tendr::attach_proto::ResumePoint) {
    let mut client = FrameClient::start(root, &attach_json(session, None));
    match client.frame() {
        tendr::attach_proto::Frame::AcceptedV2(point) => (client, point),
        other => panic!("attach not accepted: {other:?}"),
    }
}

/// After a lost link, sshd keeps the old remote side and its socket open,
/// so `meta.json` still says a human holds the session. A resume is not
/// refused for that: the sidecar decides, resumes, and retires the old
/// connection, whose remote side reports it taken over.
#[test]
fn remote_resume_is_not_refused_by_the_human_control_check() {
    use tendr::attach_proto::{Frame, ResumePoint, Version};
    use tendr::model::pty_control::ControllerEpoch;
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start_pty(&root, "host-resume", &["cat"]);

    let (mut lost, accepted) = attach_v2(&root, "host-resume");
    wait_control(&root, "host-resume", "HumanControl");

    let mut resumed = FrameClient::start_with(
        &root,
        &attach_json("host-resume", Some(accepted)),
        &Frame::Resume {
            point: accepted,
            version: Version::V2,
        },
    );
    assert_eq!(
        resumed.frame(),
        Frame::Resumed {
            point: ResumePoint {
                epoch: ControllerEpoch::new(2),
                ..accepted
            },
            input_written: None,
        }
    );
    assert_eq!(lost.frame(), Frame::Retired(ControllerEpoch::new(2)));
    assert_eq!(lost.child.wait().unwrap().code(), Some(81));
    assert_eq!(resumed.detach(), Some(0));
    wait_control(&root, "host-resume", "AgentControl");
}

/// A sidecar started by a tendr that cannot resume refuses the resume as it
/// refuses anything but a hello. The remote side answers that it cannot
/// resume (80), after that one connection: it neither attaches afresh nor
/// falls back to version 1.
#[test]
fn remote_resume_with_a_sidecar_that_cannot_resume_is_refused() {
    use tendr::attach_proto::{Frame, RejectClass, ResumePoint, Version, write_frame};
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start_pty(&root, "host-old-resume", &["cat"]);
    let seen = fake_listener(&root, "host-old-resume", "pre-resume", |mut client, _| {
        let refusal = Frame::Rejected {
            class: RejectClass::Protocol,
            reason: "attach requires a protocol hello".to_owned(),
        };
        let _ = write_frame(&mut client, &refusal);
        let _ = client.shutdown(std::net::Shutdown::Both);
    });
    let point = ResumePoint {
        run_id: tendr::model::ids::RunId::new(),
        epoch: tendr::model::pty_control::ControllerEpoch::new(1),
        offset: 7,
    };
    let mut stdin = remote_request(&attach_json("host-old-resume", Some(point)));
    let resume_frame = Frame::Resume {
        point,
        version: Version::V2,
    };
    write_frame(&mut stdin, &resume_frame).unwrap();

    let output = tendr(&root)
        .arg("_remote")
        .write_stdin(stdin)
        .timeout(HANG)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(80), "{output:?}");
    assert!(output.stdout.is_empty(), "no frame is sent");
    let mut resume = Vec::new();
    write_frame(&mut resume, &resume_frame).unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        [resume[5..].to_vec()],
        "the resume, once, and nothing else"
    );
    assert_eq!(
        control(&root, "host-old-resume").as_deref(),
        Some("AgentControl")
    );
}

/// A link that can be cut: a fake ssh that carries the remote tendr's
/// streams on pipes, as real ssh does (`pump`), so killing it closes this
/// side's pipes and leaves the remote side open, as a dropped network does.
/// Every call is logged with its start time and argv. While the `prompt`
/// file exists, a call is a login that needs a prompt: it writes an
/// approval URL to stderr and, under `BatchMode=yes`, fails at once. While
/// the `stall` file exists, a call stalls before the remote tendr starts.
/// Once the `give-up` file exists, the call under way says ssh's keepalive
/// timeout on stderr and exits 255.
struct Flaky {
    ssh: TempDir,
    files: TempDir,
}

/// The URL a login that needs a prompt writes to stderr.
const APPROVAL_URL: &str = "https://approve.invalid/a/1";

impl Flaky {
    fn new() -> Self {
        let files = TempDir::new().unwrap();
        let path = |name: &str| files.path().join(name).display().to_string();
        let ssh = fake_ssh(&format!(
            "append {}\npidfile {}\nneeds-prompt {}\nstall-if {}\ngive-up-if {}\npump",
            path("calls"),
            path("pid"),
            path("prompt"),
            path("stall"),
            path("give-up")
        ));
        Self { ssh, files }
    }

    /// `tendr --host box attach <args>` in a fresh terminal.
    fn attach(&self, root: &Home, label: &str, args: &[&str]) -> WrappedAttach {
        self.attach_with_deadline(CONNECTS, root, label, args)
    }

    /// [`Flaky::attach`] with a connect deadline of `deadline` seconds; `0`
    /// turns it off, so no hello marks the remote tendr's start.
    fn attach_with_deadline(
        &self,
        deadline: &str,
        root: &Home,
        label: &str,
        args: &[&str],
    ) -> WrappedAttach {
        attach_through(&self.ssh, deadline, root, label, args, &[])
    }

    /// Turn the `prompt` or `stall` behaviour on or off for later calls.
    fn set(&self, name: &str, on: bool) {
        let path = self.files.path().join(name);
        if on {
            std::fs::write(path, b"").unwrap();
        } else {
            std::fs::remove_file(path).unwrap();
        }
    }

    /// The pid of the latest call's ssh.
    fn pid(&self) -> libc::pid_t {
        std::fs::read_to_string(self.files.path().join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    /// Cut the link: kill the current call's ssh outright.
    fn cut(&self) -> libc::pid_t {
        let pid = self.pid();
        // SAFETY: kill(2) takes a pid and a signal number.
        assert_eq!(
            unsafe { libc::kill(pid, libc::SIGKILL) },
            0,
            "kill ssh {pid}"
        );
        pid
    }

    /// Lose the link as ssh's keepalives do: the current call says it timed
    /// out and exits 255.
    fn give_up(&self) {
        std::fs::write(self.files.path().join("give-up"), b"").unwrap();
    }

    /// The calls so far: when each started (ms since the epoch), and its argv.
    fn calls(&self) -> Vec<(u128, String)> {
        std::fs::read_to_string(self.files.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(|line| {
                let (millis, argv) = line.split_once(' ').unwrap();
                (millis.parse().unwrap(), argv.to_owned())
            })
            .collect()
    }

    /// Wait until `n` calls have started.
    fn wait_calls(&self, n: usize) -> Vec<(u128, String)> {
        poll_until(HANG, Duration::from_millis(50), || {
            let calls = self.calls();
            if calls.len() >= n {
                Observation::Ready(calls)
            } else {
                Observation::Pending(format!("{} calls", calls.len()))
            }
        })
        .unwrap_or_else(|e| panic!("never {n} ssh calls: {e}"))
    }

    /// Wait for a call whose ssh is not `old`, and return its pid.
    fn wait_new_pid(&self, old: libc::pid_t) -> libc::pid_t {
        poll_until(
            HANG,
            Duration::from_millis(20),
            || match std::fs::read_to_string(self.files.path().join("pid"))
                .ok()
                .and_then(|p| p.trim().parse().ok())
            {
                Some(pid) if pid != old => Observation::Ready(pid),
                other => Observation::Pending(format!("pid {other:?}")),
            },
        )
        .unwrap_or_else(|e| panic!("no ssh call after {old}: {e}"))
    }
}

/// Start a PTY session running `script` under `sh -c`. Each of `files` is
/// set in its environment to that name's path in `root`, for the script to
/// wait on; `env` goes to the sidecar.
fn start_script(root: &Home, session: &str, script: &str, files: &[&str], env: &[(&str, &str)]) {
    let mut start = tendr(root);
    for (key, value) in env {
        start.env(key, value);
    }
    start.args(["start", session, "--pty"]);
    for name in files {
        start
            .arg("--env")
            .arg(format!("{name}={}", root.path().join(name).display()));
    }
    let output = start.args(["--", "sh", "-c", script]).output().unwrap();
    assert!(output.status.success(), "start {session}: {output:?}");
    harness::wait_running(root, session);
}

fn touch(root: &Home, name: &str) {
    std::fs::write(root.path().join(name), b"").unwrap();
}

/// Wait until the run has granted control to `n` resumes.
fn wait_resumes(root: &Home, session: &str, n: usize) {
    poll_until(HANG, Duration::from_millis(50), || {
        let resumes = harness::read_events(root, session)
            .into_iter()
            .filter(|e| e["kind"] == "pty.control_changed" && e["data"]["trigger"] == "resume")
            .count();
        if resumes >= n {
            Observation::Ready(())
        } else {
            Observation::Pending(format!("{resumes} resumes"))
        }
    })
    .unwrap_or_else(|e| panic!("{session} was never resumed {n} times: {e}"));
}

/// What the terminal showed with the reconnect's status line taken out:
/// every write of it is wrapped in a cursor save (`ESC 7`) and restore
/// (`ESC 8`).
fn without_status_line(shown: &str) -> String {
    let mut out = String::new();
    let mut rest = shown;
    while let Some(start) = rest.find("\x1b7") {
        out.push_str(&rest[..start]);
        let end = rest[start..].find("\x1b8").expect("a status line ends") + start;
        rest = &rest[end + 2..];
    }
    out.push_str(rest);
    out
}

/// Numbered fields, about 40 KB/s, once `GO` exists and until `STOP` does,
/// then a marker; the run ends once `END` exists.
const NUMBERED_FLOOD: &str = "while [ ! -f \"$GO\" ]; do sleep 0.05; done; i=0; \
     while [ ! -f \"$STOP\" ]; do j=0; while [ $j -lt 50 ]; do printf '%09d ' $i; \
     i=$((i+1)); j=$((j+1)); done; sleep 0.01; done; printf FLOOD-END; \
     while [ ! -f \"$END\" ]; do sleep 0.05; done";

/// ssh dies mid-flood, closing this side's pipes while the old remote side
/// stays open. The client reconnects by itself, without prompts, and the
/// terminal gets every byte of the run's output exactly once, in order: the
/// recording.
#[test]
fn host_attach_reconnects_when_ssh_dies_mid_flood_with_no_gap_or_duplicate() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start_script(
        &root,
        "host-cut",
        NUMBERED_FLOOD,
        &["GO", "STOP", "END"],
        &[],
    );
    let link = Flaky::new();

    let mut wrapped = link.attach(&root, "cut", &["host-cut"]);
    wait_control(&root, "host-cut", "HumanControl");
    touch(&root, "GO");
    wrapped.wait_output_contains("000000500 ");
    link.cut();
    wait_resumes(&root, "host-cut", 1);
    // The flood goes on over the new connection.
    let resumed = wrapped.cli.output().len();
    poll_until(HANG, Duration::from_millis(50), || {
        if wrapped.cli.output().len() > resumed + 20_000 {
            Observation::Ready(())
        } else {
            Observation::Pending("no output since the resume".into())
        }
    })
    .unwrap();
    touch(&root, "STOP");
    wrapped.wait_output_contains("FLOOD-END");
    wrapped.cli.type_bytes(b"\x1cd");
    assert_eq!(wrapped.wait_exit(), 0);
    wrapped.assert_terminal_restored();

    touch(&root, "END");
    harness::wait_terminal(&root, "host-cut");
    let recording = String::from_utf8(harness::recorded_output(&root, "host-cut")).unwrap();
    let shown = without_status_line(&wrapped.cli.output());
    if shown != recording {
        let at = shown
            .bytes()
            .zip(recording.bytes())
            .position(|(a, b)| a != b)
            .unwrap_or(shown.len().min(recording.len()));
        let near = |s: &str| s[at.saturating_sub(40)..(at + 40).min(s.len())].to_owned();
        panic!(
            "the terminal showed {} bytes, the run wrote {}; first difference at {at}: \
             shown {:?}, recorded {:?}",
            shown.len(),
            recording.len(),
            near(&shown),
            near(&recording)
        );
    }
    let calls = link.calls();
    assert!(calls.len() >= 2, "{calls:?}");
    assert!(
        !calls[0].1.contains("BatchMode"),
        "the first connection may prompt"
    );
    assert!(
        calls[1..]
            .iter()
            .all(|(_, argv)| argv.contains("BatchMode=yes")),
        "a reconnect never prompts: {calls:?}"
    );
}

/// ssh's last words as its keepalives give up go to the status line, not
/// into the session's screen: the terminal gets the run's output exactly,
/// and the timeout is shown below the reconnect's line.
#[test]
fn host_attach_keeps_what_ssh_says_off_the_sessions_screen() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start_script(
        &root,
        "host-quiet",
        NUMBERED_FLOOD,
        &["GO", "STOP", "END"],
        &[],
    );
    let link = Flaky::new();

    let mut wrapped = link.attach(&root, "quiet", &["host-quiet"]);
    wait_control(&root, "host-quiet", "HumanControl");
    touch(&root, "GO");
    wrapped.wait_output_contains("000000500 ");
    link.give_up();
    wait_resumes(&root, "host-quiet", 1);
    touch(&root, "STOP");
    wrapped.wait_output_contains("FLOOD-END");
    wrapped.cli.type_bytes(b"\x1cd");
    assert_eq!(wrapped.wait_exit(), 0);

    touch(&root, "END");
    harness::wait_terminal(&root, "host-quiet");
    let output = wrapped.cli.output();
    assert!(
        output.contains("\x1b[2KTimeout, server box not responding."),
        "the timeout is on the status line: {output:?}"
    );
    let recording = String::from_utf8(harness::recorded_output(&root, "host-quiet")).unwrap();
    let shown = without_status_line(&output);
    assert!(
        shown == recording,
        "the terminal showed {} bytes, the run wrote {}; ssh's line on the screen: {}",
        shown.len(),
        recording.len(),
        shown.contains("Timeout")
    );
}

/// While logging in needs a prompt, every reconnect fails at once under
/// BatchMode, rather than wait at a prompt it cannot show, and the pauses
/// between attempts grow. ssh's stderr, with its approval URL, is on the
/// screen. What is typed meanwhile is discarded, never sent late; once the
/// login works again the attach resumes.
#[test]
fn host_reconnect_fails_fast_backs_off_and_discards_typing() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start_pty(&root, "host-backoff", &["cat"]);
    let link = Flaky::new();

    let mut wrapped = link.attach(&root, "backoff", &["host-backoff"]);
    wrapped
        .cli
        .type_until_logged(&root, "host-backoff", "before-the-drop\n");
    link.set("prompt", true);
    link.cut();
    let calls = link.wait_calls(4);
    // Between two starts: the failed attempt, then its pause. The pauses are
    // at least 1 s, then 2 s: lower bounds, so a slow attempt cannot fail
    // it.
    let gaps: Vec<u128> = calls[1..4].windows(2).map(|w| w[1].0 - w[0].0).collect();
    assert!(
        gaps[0] >= 950 && gaps[1] >= 1950,
        "the pauses between attempts grow: {gaps:?} ms"
    );
    wrapped.wait_output_contains(APPROVAL_URL);
    wrapped.cli.type_bytes(b"typed-in-the-gap\r");
    // The status line says the typing was discarded once the attach has
    // read it: only then may the login work again, or the resume could
    // come before the keystrokes are read. Matched short of its end, which
    // 80 columns cut off once a slow test reaches attempt 10.
    wrapped.wait_output_contains("; typed input is discard");

    link.set("prompt", false);
    wait_resumes(&root, "host-backoff", 1);
    wrapped
        .cli
        .type_until_logged(&root, "host-backoff", "after-the-gap\n");
    assert!(
        !harness::logged_output(&root, "host-backoff").contains("typed-in-the-gap"),
        "input typed while reconnecting reached the session"
    );
    wrapped.cli.type_bytes(b"\x1cd");
    assert_eq!(wrapped.wait_exit(), 0);
    wrapped.assert_terminal_restored();
    let calls = link.calls();
    assert!(
        calls[1..]
            .iter()
            .all(|(_, argv)| argv.contains("BatchMode=yes")),
        "{calls:?}"
    );
}

/// Hold a dropped attach in its reconnect: cut the link while logging in
/// needs a prompt, and wait until it has tried once.
fn hold_in_reconnect(link: &Flaky) {
    link.set("prompt", true);
    let calls = link.calls().len();
    link.cut();
    link.wait_calls(calls + 1);
}

/// Another client takes the session over while the attach is reconnecting:
/// the reconnect is refused, and the attach exits 81 with the remote's
/// reason on its terminal.
#[test]
fn host_reconnect_after_a_takeover_in_the_gap_exits_81() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start_pty(&root, "host-gap-take", &["cat"]);
    let link = Flaky::new();

    let mut wrapped = link.attach(&root, "gap-take", &["host-gap-take"]);
    wrapped
        .cli
        .type_until_logged(&root, "host-gap-take", "first-client\n");
    hold_in_reconnect(&link);
    let mut taker = WrappedAttach::spawn(&root, "taker", &["host-gap-take", "--takeover"]);
    taker
        .cli
        .type_until_logged(&root, "host-gap-take", "second-client\n");

    let before = wrapped.cli.output().len();
    link.set("prompt", false);
    assert_eq!(wrapped.wait_exit(), 81);
    wrapped.assert_terminal_restored();
    let said = without_status_line(&wrapped.cli.output()[before..]);
    assert!(!said.trim().is_empty(), "the refusal says why");

    taker.cli.type_bytes(b"\x1cd");
    assert_eq!(taker.wait_exit(), 0);
    taker.assert_terminal_restored();
}

/// The run ends while the attach is reconnecting: the reconnect finds it
/// not running, and the attach exits 81.
#[test]
fn host_reconnect_after_the_run_ended_exits_81() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start_pty(&root, "host-gap-end", &["cat"]);
    let link = Flaky::new();

    let mut wrapped = link.attach(&root, "gap-end", &["host-gap-end"]);
    wrapped
        .cli
        .type_until_logged(&root, "host-gap-end", "before-the-end\n");
    hold_in_reconnect(&link);
    let killed = tendr(&root)
        .args(["kill", "host-gap-end", "--force"])
        .output()
        .unwrap();
    assert!(killed.status.success(), "{killed:?}");
    harness::wait_terminal(&root, "host-gap-end");

    link.set("prompt", false);
    assert_eq!(wrapped.wait_exit(), 81);
    wrapped.assert_terminal_restored();
}

/// The escape and a signal still leave while the attach is reconnecting,
/// and stop the ssh of the attempt under way: one waiting for the remote's
/// hello, and one waiting for its answer with no hello to wait for (the
/// connect deadline off).
#[test]
fn host_escape_or_signal_while_reconnecting_leaves_and_stops_ssh() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start_pty(&root, "host-gap-leave", &["cat"]);
    let link = Flaky::new();
    // Leaving waits for the reconnect to stop its ssh
    // (`RECONNECT_STOP_GRACE`) longer than load can delay that.
    let stops = [("TENDR_TEST_RECONNECT_STOP_GRACE_MS", NEVER_MS)];

    // The escape: exit 0.
    let mut wrapped = attach_through(
        &link.ssh,
        "0",
        &root,
        "gap-escape",
        &["host-gap-leave"],
        &stops,
    );
    wrapped
        .cli
        .type_until_logged(&root, "host-gap-leave", "escape-client\n");
    link.set("stall", true);
    let cut = link.cut();
    let stalled = link.wait_new_pid(cut);
    wrapped.cli.type_bytes(b"\x1cd");
    assert_eq!(wrapped.wait_exit(), 0);
    wrapped.assert_terminal_restored();
    harness::wait_pid_dead(stalled);
    link.set("stall", false);

    // SIGTERM: exit 1. The old remote side, still open, holds the session,
    // so this attach takes it over.
    let mut wrapped = attach_through(
        &link.ssh,
        CONNECTS,
        &root,
        "gap-term",
        &["host-gap-leave", "--takeover"],
        &stops,
    );
    wrapped
        .cli
        .type_until_logged(&root, "host-gap-leave", "term-client\n");
    link.set("stall", true);
    let cut = link.cut();
    let stalled = link.wait_new_pid(cut);
    wrapped.signal(libc::SIGTERM);
    assert_eq!(wrapped.wait_exit(), 1);
    wrapped.assert_terminal_restored();
    harness::wait_pid_dead(stalled);
}

/// The output missed while reconnecting is no longer in the session's ring:
/// the terminal gets a notice naming how much was missed, the reset of the
/// modes it had, the session's modes again, then the live output, and none
/// of what was missed.
#[test]
fn host_reconnect_past_the_ring_resets_the_modes_and_says_what_was_missed() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    // About 2 MB of `x` lines, far past a 64 KiB ring, then markers. Capture
    // logs a chunk just before it adds it to the ring, so only the last
    // chunk logged can lag the ring: FLOOD-END is written only once
    // FLOOD-DONE, and with it the flood's tail, has been logged, so the
    // flood is in a chunk of its own, whatever the chunks' timing.
    let script = format!(
        "while [ ! -f \"$GO1\" ]; do sleep 0.05; done; printf '\\033[?2004h'; printf first-part; \
         while [ ! -f \"$GO2\" ]; do sleep 0.05; done; yes {} | head -n 20000; \
         echo FLOOD-DONE; while [ ! -f \"$FLOODED\" ]; do sleep 0.05; done; echo FLOOD-END; \
         while [ ! -f \"$GO3\" ]; do sleep 0.05; done; printf after-the-gap; sleep 3600",
        "x".repeat(100)
    );
    start_script(
        &root,
        "host-gone",
        &script,
        &["GO1", "GO2", "FLOODED", "GO3"],
        &[("TENDR_TEST_OUTPUT_RING_BYTES", "65536")],
    );
    let link = Flaky::new();

    let mut wrapped = link.attach(&root, "gone", &["host-gone"]);
    wait_control(&root, "host-gone", "HumanControl");
    touch(&root, "GO1");
    wrapped.wait_output_contains("first-part");
    hold_in_reconnect(&link);
    touch(&root, "GO2");
    // The flood has all been captured.
    harness::wait_log_tail(&root, "host-gone", "FLOOD-DONE");
    touch(&root, "FLOODED");
    harness::wait_log_tail(&root, "host-gone", "FLOOD-END");

    link.set("prompt", false);
    wait_resumes(&root, "host-gone", 1);
    touch(&root, "GO3");
    wrapped.wait_output_contains("after-the-gap");
    wrapped.cli.type_bytes(b"\x1cd");
    assert_eq!(wrapped.wait_exit(), 0);
    wrapped.assert_terminal_restored();

    let killed = tendr(&root)
        .args(["kill", "host-gone", "--force"])
        .output()
        .unwrap();
    assert!(killed.status.success(), "{killed:?}");
    harness::wait_terminal(&root, "host-gone");
    let recording = harness::recorded_output(&root, "host-gone");
    // What the attach had shown before the drop, and the head it went on
    // from: everything but the last marker.
    let asked = "\x1b[?2004hfirst-part".len();
    let head = recording.len() - "after-the-gap".len();

    let out = wrapped.cli.output();
    let after = &out[out.find("first-part").unwrap() + "first-part".len()..];
    let reset = after
        .find("\x1b[?2004l")
        .expect("the tracked modes are reset");
    let replay = reset
        + after[reset..]
            .find("\x1b[?2004h")
            .expect("the session's modes are replayed");
    let live = after.find("after-the-gap").unwrap();
    assert!(replay < live, "{after:?}");
    assert!(
        after[reset..replay].contains(&(head - asked).to_string()),
        "the notice names the {} bytes missed: {:?}",
        head - asked,
        &after[reset..replay]
    );
    assert!(
        !after.contains("xxxxxxxxxx"),
        "output from before the head was shown"
    );
}

/// A reconnected `attach --host` that stops reading is taken over like any
/// other: the takeover cuts it off without `MSG_RETIRED`, and its remote
/// side reads the takeover of its resumed epoch from the event log, so it
/// exits 81 rather than reconnect.
#[test]
fn host_reconnected_attach_taken_over_while_stalled_exits_81() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    let child = format!("({PACED_FLOOD}) & exec cat -v");
    start_pty(&root, "host-resume-take", &["sh", "-c", &child]);
    let link = Flaky::new();

    let first = link.attach(&root, "take-1", &["host-resume-take"]);
    first.wait_output_contains("0000000000");
    link.cut();
    wait_resumes(&root, "host-resume-take", 1);
    first.cli.pause_output();
    wait_recorded_past(&root, "host-resume-take", BACKLOG);

    let remote = Remote::new();
    let mut second = remote.attach(&root, "take-2", &["host-resume-take", "--takeover"]);
    second
        .cli
        .type_until_logged(&root, "host-resume-take", "second-client");
    assert!(
        !first.exited(),
        "a client that is not reading stays blocked"
    );

    first.cli.resume_output();
    assert_eq!(
        first.wait_exit(),
        81,
        "the stalled reconnected client was taken over"
    );
    first.assert_terminal_restored();
    first.wait_output_contains("another client took over this session");

    second.cli.type_bytes(b"\x1cd");
    assert_eq!(second.wait_exit(), 0);
    second.assert_terminal_restored();
}

/// The `pty.control_changed` events of `session` so far, as `(trigger,
/// epoch)`.
fn control_changes(root: &Home, session: &str) -> Vec<(String, u64)> {
    harness::read_events(root, session)
        .into_iter()
        .filter(|e| e["kind"] == "pty.control_changed")
        .map(|e| {
            (
                e["data"]["trigger"].as_str().unwrap_or_default().to_owned(),
                e["data"]["epoch"].as_u64().unwrap_or_default(),
            )
        })
        .collect()
}

/// A read-only `attach --host` beside a controller holding the session: no
/// human-control check refuses it, and when ssh dies mid-flood it reconnects
/// by itself, without prompts, and resumes as a watcher. Its terminal gets
/// the run's output exactly once, in order, from where it attached; the
/// controller is never touched: no control change but its own attach, and
/// it detaches as it would have.
#[test]
fn host_read_only_attach_resumes_beside_a_controller_with_no_gap_or_duplicate() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start_script(
        &root,
        "host-ro-cut",
        NUMBERED_FLOOD,
        &["GO", "STOP", "END"],
        &[],
    );
    let mut controller = WrappedAttach::spawn(&root, "ro-controller", &["host-ro-cut"]);
    wait_control(&root, "host-ro-cut", "HumanControl");
    let link = Flaky::new();

    let mut watcher = link.attach(&root, "ro-cut", &["host-ro-cut", "--read-only"]);
    touch(&root, "GO");
    watcher.wait_output_contains("000000500 ");
    link.cut();
    link.wait_calls(2);
    let resumed = watcher.cli.output().len();
    poll_until(HANG, Duration::from_millis(50), || {
        if watcher.cli.output().len() > resumed + 20_000 {
            Observation::Ready(())
        } else {
            Observation::Pending("no output since the resume".into())
        }
    })
    .unwrap();
    touch(&root, "STOP");
    watcher.wait_output_contains("FLOOD-END");
    watcher.cli.type_bytes(b"\x1cd");
    assert_eq!(watcher.wait_exit(), 0);
    watcher.assert_terminal_restored();

    assert_eq!(
        control(&root, "host-ro-cut").as_deref(),
        Some("HumanControl")
    );
    assert_eq!(
        control_changes(&root, "host-ro-cut"),
        [("attach".to_owned(), 1)],
        "the watcher changed control"
    );
    controller.cli.type_bytes(b"\x1cd");
    assert_eq!(controller.wait_exit(), 0);
    controller.assert_terminal_restored();
    wait_control(&root, "host-ro-cut", "AgentControl");

    touch(&root, "END");
    harness::wait_terminal(&root, "host-ro-cut");
    let recording = String::from_utf8(harness::recorded_output(&root, "host-ro-cut")).unwrap();
    let shown = without_status_line(&watcher.cli.output());
    assert!(
        shown.len() > 20_000 && recording.ends_with(&shown),
        "the watcher showed {} bytes that are not the end of the run's {}",
        shown.len(),
        recording.len()
    );
    let calls = link.calls();
    assert!(
        calls[1..]
            .iter()
            .all(|(_, argv)| argv.contains("BatchMode=yes")),
        "a reconnect never prompts: {calls:?}"
    );
}

/// This client's read-only attach request for `session`, resuming from
/// `resume`: a watcher's resume carries epoch zero.
fn watch_json(session: &str, resume: Option<tendr::attach_proto::WatchPoint>) -> String {
    let resume = resume.map_or(String::new(), |p| {
        format!(
            r#","resume":{{"run_id":"{}","epoch":0,"offset":{}}}"#,
            p.run_id.as_uuid(),
            p.offset
        )
    });
    format!(
        r#"{{"v":1,"op":"attach","params":{{"session":"{session}","namespace":null,"takeover":false,"read_only":true,"escape":"ctrl-backslash","protocol":2{resume}}}}}"#
    )
}

/// `_remote` run for a watcher's resume from `point`, with `first` in place
/// of the hello.
fn watch_resume(
    root: &Home,
    session: &str,
    point: tendr::attach_proto::WatchPoint,
    first: &tendr::attach_proto::Frame,
) -> FrameClient {
    FrameClient::start_with(root, &watch_json(session, Some(point)), first)
}

/// The remote side admits a read-only request beside a controller, and
/// answers a watcher's resume as the sidecar does, at epoch zero, which
/// names no control: resumed at the offset asked for while the ring holds
/// it, gone with what it holds once it does not. A resume for another run
/// is refused (81), and so is a plain resume frame after a read-only
/// request (80), before any answer. The controller is untouched throughout.
#[test]
fn remote_watch_resume_is_answered_at_epoch_zero_and_never_touches_control() {
    use tendr::attach_proto::{Frame, Mode, ResumePoint, Version, WatchPoint};
    use tendr::model::pty_control::ControllerEpoch;
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    // The flood passes the 4 KiB ring in 1200-byte steps (ONLCR). Each step
    // is let out only once the controller has been sent the one before, so
    // it is never a whole ring behind and never evicted, however slowly
    // load runs its sender.
    start_script(
        &root,
        "host-ro-resume",
        "while [ ! -f \"$GO1\" ]; do sleep 0.05; done; printf abc; \
         i=0; while [ $i -lt 6 ]; do while [ ! -f \"$GO2.$i\" ]; do sleep 0.05; done; \
         yes xxxxxxxxxx | head -n 100; i=$((i+1)); done; \
         printf FLOOD-END; sleep 3600",
        &["GO1", "GO2"],
        &[("TENDR_TEST_OUTPUT_RING_BYTES", "4096")],
    );
    let (mut controller, _) = attach_v2(&root, "host-ro-resume");
    wait_control(&root, "host-ro-resume", "HumanControl");

    let mut watcher = FrameClient::start_with(
        &root,
        &watch_json("host-ro-resume", None),
        &Frame::Hello {
            version: Version::V1,
            mode: Mode::Watch,
        },
    );
    let Frame::Watching(admitted) = watcher.frame() else {
        panic!("the watcher was not admitted");
    };
    assert_eq!(admitted.offset, 0);
    touch(&root, "GO1");
    assert_eq!(
        watcher.frame(),
        Frame::Output {
            offset: 0,
            bytes: b"abc".to_vec()
        }
    );
    assert_eq!(watcher.detach(), Some(0));

    let asked = WatchPoint {
        offset: 3,
        ..admitted
    };
    let watch_resume_frame = Frame::WatchResume {
        point: asked,
        version: Version::V2,
    };
    let mut resumed = watch_resume(&root, "host-ro-resume", asked, &watch_resume_frame);
    assert_eq!(
        resumed.frame(),
        Frame::Resumed {
            point: ResumePoint {
                run_id: admitted.run_id,
                epoch: ControllerEpoch::ZERO,
                offset: 3,
            },
            input_written: None,
        }
    );
    assert_eq!(resumed.detach(), Some(0));

    let mut to_controller = Vec::new();
    for step in 0..6 {
        touch(&root, &format!("GO2.{step}"));
        let lines = (step + 1) * 100;
        while String::from_utf8_lossy(&to_controller)
            .matches("xxxxxxxxxx")
            .count()
            < lines
        {
            if let Frame::Output { bytes, .. } = controller.frame() {
                to_controller.extend_from_slice(&bytes);
            }
        }
    }
    harness::wait_log_tail(&root, "host-ro-resume", "FLOOD-END");
    let mut gone = watch_resume(&root, "host-ro-resume", asked, &watch_resume_frame);
    match gone.frame() {
        Frame::Gone {
            point,
            oldest,
            input_written: None,
        } => {
            assert_eq!(
                (point.run_id, point.epoch),
                (admitted.run_id, ControllerEpoch::ZERO)
            );
            assert!(
                oldest > 3 && point.offset - oldest <= 4096,
                "{point:?} {oldest}"
            );
        }
        other => panic!("not gone: {other:?}"),
    }
    assert_eq!(gone.detach(), Some(0));

    let elsewhere = WatchPoint {
        run_id: tendr::model::ids::RunId::new(),
        ..asked
    };
    let plain = Frame::Resume {
        point: ResumePoint {
            run_id: asked.run_id,
            epoch: ControllerEpoch::new(1),
            offset: 3,
        },
        version: Version::V2,
    };
    for (what, mut refused, code) in [
        (
            "another run",
            watch_resume(
                &root,
                "host-ro-resume",
                elsewhere,
                &Frame::WatchResume {
                    point: elsewhere,
                    version: Version::V2,
                },
            ),
            81,
        ),
        (
            "a plain resume",
            watch_resume(&root, "host-ro-resume", asked, &plain),
            80,
        ),
    ] {
        let mut rest = Vec::new();
        std::io::Read::read_to_end(&mut refused.stdout, &mut rest).unwrap();
        assert!(rest.is_empty(), "{what}: no frame is sent");
        assert_eq!(refused.child.wait().unwrap().code(), Some(code), "{what}");
    }

    assert_eq!(
        control_changes(&root, "host-ro-resume"),
        [("attach".to_owned(), 1)],
        "a watcher changed control"
    );
    assert_eq!(controller.detach(), Some(0));
    wait_control(&root, "host-ro-resume", "AgentControl");
}

/// Watchers resumed through `_remote` in version 3 (the request's
/// `protocol`) and in version 2, side by side: at the run's end the version
/// 3 one is sent the rest of the output, then the end of output at the
/// final offset, and its remote exits 0; the version 2 one gets the output
/// and nothing it does not know, and its remote goes by `meta.json` (0).
#[test]
fn remote_watch_resume_in_version_3_is_told_where_the_output_ends() {
    use tendr::attach_proto::{Frame, Mode, Version, WatchPoint};
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start_script(
        &root,
        "host-ro-end",
        "while [ ! -f \"$GO\" ]; do sleep 0.05; done; printf xyz",
        &["GO"],
        &[FLUSH_ALL],
    );
    let json = |protocol: u8, resume: Option<WatchPoint>| {
        watch_json("host-ro-end", resume)
            .replace("\"protocol\":2", &format!("\"protocol\":{protocol}"))
    };
    let mut watcher = FrameClient::start_with(
        &root,
        &json(3, None),
        &Frame::Hello {
            version: Version::V1,
            mode: Mode::Watch,
        },
    );
    let Frame::Watching(admitted) = watcher.frame() else {
        panic!("the watcher was not admitted");
    };
    assert_eq!(watcher.detach(), Some(0));

    // The client's frame is version 2's either way; the request says which
    // the remote may speak.
    let first = Frame::WatchResume {
        point: admitted,
        version: Version::V2,
    };
    let mut resumed: Vec<FrameClient> = [3, 2]
        .into_iter()
        .map(|protocol| FrameClient::start_with(&root, &json(protocol, Some(admitted)), &first))
        .collect();
    for client in &mut resumed {
        assert!(
            matches!(client.frame(), Frame::Resumed { point, .. } if point.offset == admitted.offset)
        );
    }
    touch(&root, "GO");
    let ends: Vec<(Vec<Frame>, Option<i32>)> = resumed
        .into_iter()
        .map(|mut client| {
            let mut frames = Vec::new();
            while let Ok((msg_type, payload)) = tendr::attach_proto::read_msg(&mut client.stdout) {
                frames.push(Frame::decode(msg_type, payload).unwrap());
            }
            drop(client.stdin.take());
            (frames, client.child.wait().unwrap().code())
        })
        .collect();

    let output = Frame::Output {
        offset: admitted.offset,
        bytes: b"xyz".to_vec(),
    };
    assert_eq!(
        ends[0],
        (
            vec![output.clone(), Frame::OutputEnd(admitted.offset + 3)],
            Some(0)
        ),
        "version 3"
    );
    assert_eq!(ends[1], (vec![output], Some(0)), "version 2");
}

/// `tendr --host box attach --read-only <session>` through the fake ssh in
/// `ssh`, with no terminal: stdin closed, stdout and stderr piped, and `env`
/// added (the fake ssh passes it on to the remote tendr).
fn watch_without_terminal(
    ssh: &TempDir,
    root: &Home,
    session: &str,
    env: &[(&str, &str)],
) -> std::process::Child {
    use std::process::Stdio;
    std::process::Command::new(tendr_bin())
        .args(["--host", "box", "attach", "--read-only", session])
        .env("HOME", root.path())
        .env("PATH", shim_path_env(ssh))
        .env("TENDR_TEST_BIN", tendr_bin())
        .env("TENDR_SSH_CONNECT_TIMEOUT", CONNECTS)
        .env(SSH_CLOSES.0, SSH_CLOSES.1)
        .envs(env.iter().copied())
        .env_remove("TENDR_TEST_ATTACH_TERMINAL")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// Read `stdout` until what was read contains `marker`.
fn read_until(stdout: &mut std::process::ChildStdout, marker: &str) -> Vec<u8> {
    use std::io::Read;
    let mut read = Vec::new();
    let mut buf = [0u8; 4096];
    while !String::from_utf8_lossy(&read).contains(marker) {
        let n = stdout.read(&mut buf).unwrap();
        assert!(n > 0, "stdout ended before {marker:?}: {read:?}");
        read.extend_from_slice(&buf[..n]);
    }
    read
}

/// Wait for `child` to exit, killing it if it hangs ([`HANG`]), and return
/// its code and stderr.
fn finish(mut child: std::process::Child) -> (Option<i32>, String) {
    use std::io::Read;
    let status = poll_until(HANG, Duration::from_millis(20), || {
        match child.try_wait().unwrap() {
            Some(status) => Observation::Ready(status),
            None => Observation::Pending("attach still running".to_owned()),
        }
    })
    .unwrap_or_else(|e| {
        let _ = child.kill();
        panic!("attach did not exit: {e}");
    });
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    (status.code(), stderr)
}

/// A child that turns no terminal modes on: ticks until `GO` exists, then
/// runs `then`.
fn ticking_then(then: &str) -> String {
    format!("while [ ! -f \"$GO\" ]; do echo tick; sleep 0.05; done; {then}")
}

/// Without a terminal, a read-only `attach --host` writes exactly the run's
/// output from where it attached to stdout, and exits 0 when the run ends:
/// the remote's verdict, from its own `meta.json`.
#[test]
fn host_read_only_without_a_terminal_writes_the_runs_output_and_exits_0() {
    use std::io::Read;
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start_script(
        &root,
        "host-ro-out",
        &ticking_then(
            "i=0; while [ $i -lt 200 ]; do echo line-$i; i=$((i+1)); done; echo ALL-DONE",
        ),
        &["GO"],
        &[FLUSH_ALL],
    );
    let remote = Remote::new();

    let mut child = watch_without_terminal(&remote.ssh, &root, "host-ro-out", &[]);
    let mut stdout = child.stdout.take().unwrap();
    let mut out = read_until(&mut stdout, "tick");
    touch(&root, "GO");
    stdout.read_to_end(&mut out).unwrap();
    let (code, stderr) = finish(child);

    assert_eq!(code, Some(0), "the run ended; stderr: {stderr}");
    let recording = harness::recorded_output(&root, "host-ro-out");
    assert!(
        recording.ends_with(&out),
        "stdout is not the recording from the attach on:\n{}",
        String::from_utf8_lossy(&out)
    );
    assert!(String::from_utf8_lossy(&out).contains("line-199\r\nALL-DONE\r\n"));
}

/// Without a terminal, a read-only `attach --host` whose consumer stalls
/// while the run floods past the ring is cut off by the session; the remote
/// reads that the run is still running and exits 84, which the client
/// exits with, its notice on stderr and none on stdout.
#[test]
fn host_read_only_without_a_terminal_cut_off_exits_84() {
    use std::io::Read;
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    let line = "0123456789abcdef".repeat(62);
    start_script(
        &root,
        "host-ro-cutoff",
        &ticking_then(&format!(
            "i=0; while [ $i -lt 300 ]; do yes {line} | head -n 10; i=$((i+1)); done; \
             printf ALL-DONE; while [ ! -f \"$END\" ]; do sleep 0.05; done"
        )),
        &["GO", "END"],
        &[("TENDR_TEST_OUTPUT_RING_BYTES", "1048576")],
    );
    let remote = Remote::new();

    // The run does not end before the verdict (it waits for END), so the
    // cut-off, however late load makes it, is the only end the attach sees.
    let mut child = watch_without_terminal(&remote.ssh, &root, "host-ro-cutoff", &[]);
    let mut stdout = child.stdout.take().unwrap();
    read_until(&mut stdout, "tick");
    // Stall: read nothing while the run floods past the ring.
    touch(&root, "GO");
    harness::wait_log_tail(&root, "host-ro-cutoff", "ALL-DONE");
    let mut out = Vec::new();
    stdout.read_to_end(&mut out).unwrap();
    let (code, stderr) = finish(child);
    touch(&root, "END");
    harness::wait_terminal(&root, "host-ro-cutoff");

    assert_eq!(
        code,
        Some(84),
        "cut off while the run went on; stderr: {stderr}"
    );
    assert!(stderr.contains("cut this read-only attach off"), "{stderr}");
    assert!(
        !String::from_utf8_lossy(&out).contains("tendr:"),
        "a notice reached stdout"
    );
}

/// The run's last bytes reach a watcher over `--host` that was behind when
/// the run ended: the session sends a viewer everything up to the run's
/// final offset before it closes the connection, waiting for one that is
/// slow to read (here for longer than load can hold it up).
#[test]
fn host_read_only_gets_the_runs_last_bytes_at_its_end() {
    use std::io::Read;
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start_script(
        &root,
        "host-ro-flush",
        &ticking_then(&format!(
            "yes {} | head -n 40000; printf LAST-BYTES",
            "y".repeat(62)
        )),
        &["GO"],
        &[FLUSH_ALL],
    );
    let remote = Remote::new();

    let mut child = watch_without_terminal(&remote.ssh, &root, "host-ro-flush", &[]);
    let mut stdout = child.stdout.take().unwrap();
    let mut out = read_until(&mut stdout, "tick");
    // Behind at the end: nothing read until the run has written its last
    // bytes, then a moment more for it to end. This sleep only waits out
    // the run's exit; under load the watcher may start reading before the
    // run has ended, which can only weaken the test, never fail it.
    touch(&root, "GO");
    harness::wait_log_tail(&root, "host-ro-flush", "LAST-BYTES");
    std::thread::sleep(Duration::from_millis(500));
    stdout.read_to_end(&mut out).unwrap();
    let (code, stderr) = finish(child);

    assert_eq!(code, Some(0), "the run ended; stderr: {stderr}");
    assert!(
        out.ends_with(b"LAST-BYTES"),
        "stdout ends short of the run's last bytes: {:?}",
        String::from_utf8_lossy(&out[out.len().saturating_sub(80)..])
    );
    let recording = harness::recorded_output(&root, "host-ro-flush");
    assert!(
        recording.ends_with(&out),
        "stdout is not the recording's end"
    );
}

/// A child that writes a burst and exits at once: a watcher over `--host`,
/// through ssh's pipes, reading all along, gets the whole burst through the
/// marker that ends it, and exits 0.
#[test]
fn host_read_only_gets_a_burst_as_the_run_ends_through_its_marker() {
    use std::io::Read;
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start_script(
        &root,
        "host-ro-burst",
        &ticking_then(harness::BURST),
        &["GO"],
        &[FLUSH_ALL],
    );
    let ssh = fake_ssh("pump");

    let mut child = watch_without_terminal(&ssh, &root, "host-ro-burst", &[]);
    let mut stdout = child.stdout.take().unwrap();
    let mut out = read_until(&mut stdout, "tick");
    touch(&root, "GO");
    stdout.read_to_end(&mut out).unwrap();
    let (code, stderr) = finish(child);

    assert_eq!(code, Some(0), "the run ended; stderr: {stderr}");
    assert!(
        out.ends_with(b"burst-0199999\r\nBURST-END"),
        "stdout ends short of the marker: {:?}",
        String::from_utf8_lossy(&out[out.len().saturating_sub(80)..])
    );
    let recording = harness::recorded_output(&root, "host-ro-burst");
    assert!(
        recording.ends_with(&out),
        "stdout is not the recording's end"
    );
}

/// A watcher over `--host` whose consumer reads nothing from the burst at
/// the run's end until well past the session's wait for its viewers is cut
/// off then: 84, never the remote's reading of `meta.json` that the run
/// ended, and stdout is the recording up to where it stopped.
#[test]
fn host_read_only_stalled_past_the_run_end_flush_exits_84() {
    use std::io::Read;
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start_script(
        &root,
        "host-ro-stall",
        &ticking_then(harness::BURST),
        &["GO"],
        &[],
    );
    let ssh = fake_ssh("pump");

    // The session's run-end flush (`RUN_END_FLUSH`) is the only way out:
    // this consumer reads nothing until the run's end is recorded.
    let mut child = watch_without_terminal(&ssh, &root, "host-ro-stall", &[]);
    let mut stdout = child.stdout.take().unwrap();
    let mut out = read_until(&mut stdout, "tick");
    touch(&root, "GO");
    // The run's end is recorded only once the session has waited for its
    // viewers and shut their connections.
    harness::wait_terminal(&root, "host-ro-stall");
    stdout.read_to_end(&mut out).unwrap();
    let (code, stderr) = finish(child);

    assert_eq!(code, Some(84), "cut off at the run's end; stderr: {stderr}");
    assert!(stderr.contains("cut this read-only attach off"), "{stderr}");
    assert!(stderr.contains("bytes of the run's output"), "{stderr}");
    assert!(
        !String::from_utf8_lossy(&out).contains("tendr:"),
        "a notice reached stdout"
    );
    harness::assert_prefix_of_the_burst(&harness::recorded_output(&root, "host-ro-stall"), &out);
}

/// A session over `--host` whose sidecar cannot report the end of its
/// output refuses the bridge's version 3 watch hello; the bridge says it
/// again in version 2, and at the run's end its verdict goes by `meta.json`
/// as before: all the output, and exit 0.
#[test]
fn host_read_only_with_a_sidecar_from_before_the_end_of_output_exits_0() {
    use std::io::Read;
    use tendr::attach_proto::MSG_HELLO;
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start_script(
        &root,
        "host-ro-old-end",
        &ticking_then("echo ALL-DONE"),
        &["GO"],
        &[FLUSH_ALL],
    );
    let seen = old_sidecar(
        &root,
        "host-ro-old-end",
        "pre-end",
        decoded_before_output_end,
    );
    let ssh = fake_ssh("pump");

    let mut child = watch_without_terminal(&ssh, &root, "host-ro-old-end", &[RECORDED_END]);
    let mut stdout = child.stdout.take().unwrap();
    let mut out = read_until(&mut stdout, "tick");
    touch(&root, "GO");
    stdout.read_to_end(&mut out).unwrap();
    let (code, stderr) = finish(child);

    assert_eq!(code, Some(0), "the run ended; stderr: {stderr}");
    assert!(
        out.ends_with(b"ALL-DONE\r\n"),
        "{:?}",
        String::from_utf8_lossy(&out)
    );
    assert_eq!(
        *seen.lock().unwrap(),
        [(MSG_HELLO, vec![3, 4]), (MSG_HELLO, vec![2, 4])]
    );
}

/// The first frames, type and payload, an emulated old sidecar was sent.
type FirstFrames = std::sync::Arc<Mutex<Vec<(u8, Vec<u8>)>>>;

/// A sidecar started by an older tendr, emulated at the frame level in
/// front of `session`'s real one: a connection whose first frame `decodes`
/// takes is passed through to the real sidecar, which serves it as the old
/// one would; any other is refused with the protocol class, as the old
/// decoder refuses it.
fn old_sidecar(
    root: &Home,
    session: &str,
    label: &str,
    decodes: fn(u8, &[u8]) -> bool,
) -> FirstFrames {
    use std::io::Write;
    use tendr::attach_proto::{Frame, RejectClass, read_msg, write_frame, write_msg};
    let real = tendr::attach_proto::read_sock_path(
        &root
            .path()
            .join(format!(".tendr/sessions/default/{session}")),
    )
    .unwrap();
    let sock = root.path().join(format!("{label}.sock"));
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let seen = FirstFrames::default();
    let frames = std::sync::Arc::clone(&seen);
    std::thread::spawn(move || {
        for mut client in listener.incoming().map_while(Result::ok) {
            let _ = client.set_read_timeout(Some(HANG));
            let Ok((msg_type, payload)) = read_msg(&mut client) else {
                continue;
            };
            let _ = client.set_read_timeout(None);
            frames.lock().unwrap().push((msg_type, payload.clone()));
            if !decodes(msg_type, &payload) {
                let reason = if msg_type == tendr::attach_proto::MSG_HELLO {
                    "unsupported attach protocol version or mode"
                } else {
                    "attach requires a protocol hello"
                };
                let refusal = Frame::Rejected {
                    class: RejectClass::Protocol,
                    reason: reason.to_owned(),
                };
                let _ = write_frame(&mut client, &refusal);
                let _ = client.shutdown(std::net::Shutdown::Both);
                continue;
            }
            let mut sidecar = std::os::unix::net::UnixStream::connect(&real).unwrap();
            write_msg(&mut sidecar, msg_type, &payload).unwrap();
            let (mut from_client, mut to_sidecar) =
                (client.try_clone().unwrap(), sidecar.try_clone().unwrap());
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut from_client, &mut to_sidecar);
                let _ = to_sidecar.shutdown(std::net::Shutdown::Both);
            });
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut sidecar, &mut client);
                let _ = client.flush();
                let _ = client.shutdown(std::net::Shutdown::Both);
            });
        }
    });
    point_breadcrumb_at(root, session, &sock);
    seen
}

/// The first frames a sidecar from before read-only viewers decodes: a
/// hello of version 1 or 2 in modes 1 to 3 (attach, takeover, push), or a
/// resume.
fn decoded_before_watchers(msg_type: u8, payload: &[u8]) -> bool {
    use tendr::attach_proto::{MSG_HELLO, MSG_RESUME};
    match (msg_type, payload) {
        (MSG_HELLO, [version, mode]) => (1..=2).contains(version) && (1..=3).contains(mode),
        (MSG_RESUME, point) => point.len() == 32,
        _ => false,
    }
}

/// The first frames a sidecar from before watch resumes decodes: those of
/// [`decoded_before_watchers`], and a watch hello.
fn decoded_before_watch_resumes(msg_type: u8, payload: &[u8]) -> bool {
    decoded_before_watchers(msg_type, payload)
        || (msg_type == tendr::attach_proto::MSG_HELLO && matches!(payload, [1 | 2, 4]))
}

/// The first frames a sidecar from before the end of output decodes: those
/// of [`decoded_before_watch_resumes`], and a watch resume without a
/// version.
fn decoded_before_output_end(msg_type: u8, payload: &[u8]) -> bool {
    decoded_before_watch_resumes(msg_type, payload)
        || (msg_type == tendr::attach_proto::MSG_WATCH_RESUME && payload.len() == 24)
}

/// A watcher over `--host` of a session whose sidecar knows watch resumes
/// but not the end of output loses its link: the bridge's version 3 watch
/// resume is refused, it says it again in version 2, which the sidecar
/// resumes, and at the run's end the remote's verdict goes by `meta.json`
/// as before (0).
#[test]
fn host_watch_resume_to_a_sidecar_from_before_the_end_of_output_says_it_in_version_2() {
    use tendr::attach_proto::{MSG_HELLO, MSG_WATCH_RESUME};
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start_script(
        &root,
        "host-ro-old-end-resume",
        &ticking_then("echo ALL-DONE"),
        &["GO"],
        &[FLUSH_ALL],
    );
    let seen = old_sidecar(
        &root,
        "host-ro-old-end-resume",
        "pre-end-resume",
        decoded_before_output_end,
    );
    let link = Flaky::new();

    let watcher = attach_through(
        &link.ssh,
        CONNECTS,
        &root,
        "old-end-resume",
        &["host-ro-old-end-resume", "--read-only"],
        &[RECORDED_END],
    );
    watcher.wait_output_contains("tick");
    link.cut();
    poll_until(HANG, Duration::from_millis(50), || {
        let seen = seen.lock().unwrap().clone();
        if seen.len() >= 4 {
            Observation::Ready(())
        } else {
            Observation::Pending(format!("{seen:?}"))
        }
    })
    .unwrap();
    touch(&root, "GO");
    watcher.wait_output_contains("ALL-DONE");
    assert_eq!(watcher.wait_exit(), 0);
    watcher.assert_terminal_restored();
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 4, "{seen:?}");
    assert_eq!(seen[0], (MSG_HELLO, vec![3, 4]));
    assert_eq!(seen[1], (MSG_HELLO, vec![2, 4]));
    assert_eq!((seen[2].0, seen[2].1.len()), (MSG_WATCH_RESUME, 25));
    assert_eq!((seen[3].0, seen[3].1.len()), (MSG_WATCH_RESUME, 24));
}

/// A watch hello to a sidecar started before read-only viewers is refused
/// (80), said in version 3 and then once in version 2, which every sidecar
/// with watchers speaks: never in version 1, and nothing attaches. The
/// controller holding the session is untouched.
#[test]
fn host_watch_hello_to_a_sidecar_from_before_watchers_exits_80() {
    use tendr::attach_proto::MSG_HELLO;
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start_pty(&root, "host-ro-old", &["cat"]);
    let mut controller = WrappedAttach::spawn(&root, "old-controller", &["host-ro-old"]);
    wait_control(&root, "host-ro-old", "HumanControl");
    let seen = old_sidecar(&root, "host-ro-old", "pre-watch", decoded_before_watchers);
    let remote = Remote::new();

    let (code, out) =
        host_attach_exit(&root, &remote, "old-watch", &["host-ro-old", "--read-only"]);
    assert_eq!(code, 80, "{out}");
    assert!(out.contains("cannot serve read-only viewers"), "{out}");
    assert_eq!(
        *seen.lock().unwrap(),
        [(MSG_HELLO, vec![3, 4]), (MSG_HELLO, vec![2, 4])]
    );
    assert_eq!(
        control_changes(&root, "host-ro-old"),
        [("attach".to_owned(), 1)]
    );

    controller.cli.type_bytes(b"\x1cd");
    assert_eq!(controller.wait_exit(), 0);
    wait_control(&root, "host-ro-old", "AgentControl");
}

/// A watcher admitted by a sidecar started before watch resumes loses its
/// link: its watch resume is refused (80), in version 3 and then once in
/// version 2, and it neither attaches afresh nor sends a controller's
/// resume. The controller is untouched.
#[test]
fn host_watch_resume_to_a_sidecar_from_before_it_exits_80() {
    use tendr::attach_proto::{MSG_HELLO, MSG_WATCH_RESUME};
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start_pty(
        &root,
        "host-ro-old-resume",
        &["sh", "-c", "while :; do echo tick; sleep 0.2; done"],
    );
    let mut controller =
        WrappedAttach::spawn(&root, "old-resume-controller", &["host-ro-old-resume"]);
    wait_control(&root, "host-ro-old-resume", "HumanControl");
    let seen = old_sidecar(
        &root,
        "host-ro-old-resume",
        "pre-watch-resume",
        decoded_before_watch_resumes,
    );
    let link = Flaky::new();

    let watcher = link.attach(&root, "old-resume", &["host-ro-old-resume", "--read-only"]);
    watcher.wait_output_contains("tick");
    link.cut();
    assert_eq!(watcher.wait_exit(), 80);
    watcher.assert_terminal_restored();
    watcher.wait_output_contains("cannot resume a read-only attach");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 4, "{seen:?}");
    assert_eq!(seen[0], (MSG_HELLO, vec![3, 4]));
    assert_eq!(seen[1], (MSG_HELLO, vec![2, 4]));
    assert_eq!((seen[2].0, seen[2].1.len()), (MSG_WATCH_RESUME, 25));
    assert_eq!((seen[3].0, seen[3].1.len()), (MSG_WATCH_RESUME, 24));
    assert_eq!(
        control_changes(&root, "host-ro-old-resume"),
        [("attach".to_owned(), 1)]
    );

    controller.cli.type_bytes(b"\x1cd");
    assert_eq!(controller.wait_exit(), 0);
    wait_control(&root, "host-ro-old-resume", "AgentControl");
}

/// A remote tendr from before read-only `--host`, emulated from its bridge:
/// its exit code for `sent` (a request, then its first attach frame) while
/// `human_control` holds the session, or `None` if it would go on to attach.
/// `before_this` is this repository's bridge just before read-only `--host`
/// (it knows `resume` and decodes every frame up to the watch accept);
/// otherwise 0.5.0's, which knows no `resume` and takes only version 1
/// hellos in modes 1 to 3. Both refuse a session under human control (81)
/// before reading the frame, unless the request takes over or, to one that
/// knows it, resumes; then the frame must be exactly the hello the request
/// names, or after a resume its resume frame, or it is refused (80). Neither
/// reads `read_only`.
fn old_remote(sent: &[u8], human_control: bool, before_this: bool) -> Option<i32> {
    use tendr::attach_proto::{MSG_HELLO, MSG_RESUME, read_msg};
    let len = u32::from_be_bytes(sent[..4].try_into().unwrap()) as usize;
    let request: serde_json::Value = serde_json::from_slice(&sent[4..4 + len]).unwrap();
    let params = &request["params"];
    let takeover = params["takeover"].as_bool().unwrap();
    let resume = Some(&params["resume"]).filter(|r| before_this && !r.is_null());
    if human_control && !takeover && resume.is_none() {
        return Some(81);
    }
    let (msg_type, payload) = read_msg(&mut &sent[4 + len..]).unwrap();
    let attaches = match resume {
        None => {
            let (versions, modes) = if before_this {
                (1..=2, 1..=4)
            } else {
                (1..=1, 1..=3)
            };
            let mode = if takeover { 2 } else { 1 };
            msg_type == MSG_HELLO
                && matches!(payload.as_slice(), [v, m]
                    if versions.contains(v) && modes.contains(m) && *m == mode)
        }
        Some(point) => {
            let mut asked = point["epoch"].as_u64().unwrap().to_be_bytes().to_vec();
            let run_id = uuid::Uuid::parse_str(point["run_id"].as_str().unwrap()).unwrap();
            asked.extend_from_slice(run_id.as_bytes());
            asked.extend_from_slice(&point["offset"].as_u64().unwrap().to_be_bytes());
            msg_type == MSG_RESUME && payload == asked
        }
    };
    (!attaches).then_some(80)
}

/// What a read-only `attach --host` sends a remote tendr, its watch hello
/// and its watch resume, is refused by one from before read-only `--host`:
/// 0.5.0's, or this repository's just before it. Each refuses with 81 when
/// its own human-control check comes first, otherwise 80, and never goes on
/// to attach, so never takes or changes control.
#[test]
fn a_watch_hello_and_watch_resume_are_refused_by_an_old_remote() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    let files = TempDir::new().unwrap();
    let path = |name: &str| files.path().join(name);
    let ssh = fake_ssh(&format!(
        "give-up-if {drop}\nscript\n\
         watch request {fresh}\n\
         watch answer\n\
         watch output one\n\
         watch drain\n\
         watch-resume request {resumed}\n\
         watch-resume exit 80",
        drop = path("drop").display(),
        fresh = path("fresh").display(),
        resumed = path("resumed").display(),
    ));
    let watcher = attach_through(
        &ssh,
        CONNECTS,
        &root,
        "old-remote-ro",
        &["s", "--read-only"],
        &[],
    );
    watcher.wait_output_contains("one");
    std::fs::write(path("drop"), b"").unwrap();
    assert_eq!(watcher.wait_exit(), 80);
    watcher.assert_terminal_restored();

    let fresh = std::fs::read(path("fresh")).unwrap();
    let resumed = std::fs::read(path("resumed")).unwrap();
    for before_this in [true, false] {
        for human_control in [false, true] {
            let at = format!("before_this={before_this} human_control={human_control}");
            let checked = if human_control { 81 } else { 80 };
            assert_eq!(
                old_remote(&fresh, human_control, before_this),
                Some(checked),
                "hello, {at}"
            );
            // Only a remote that knows `resume` skips its check for one.
            let resume = if before_this { 80 } else { checked };
            assert_eq!(
                old_remote(&resumed, human_control, before_this),
                Some(resume),
                "resume, {at}"
            );
        }
    }
}

/// Keystrokes far past what a PTY's input buffer holds, so that most of
/// them wait in the session's queue, the bridge or the pipes between.
const FLOOD: usize = 256 * 1024;

/// A version 4 attach through `_remote`, and the point it was accepted at.
fn attach_v4(root: &Home, session: &str) -> (FrameClient, tendr::attach_proto::ResumePoint) {
    let mut client = FrameClient::start(root, &attach_json_in(4, session, None));
    match client.frame() {
        tendr::attach_proto::Frame::AcceptedV2(point) => (client, point),
        other => panic!("attach not accepted: {other:?}"),
    }
}

/// A version 4 resume through `_remote` from `point`, as the client sends
/// it after a lost link, and how many bytes of the lost connection's input
/// its answer says were written.
fn resume_v4(
    root: &Home,
    session: &str,
    point: tendr::attach_proto::ResumePoint,
) -> (FrameClient, u64) {
    use tendr::attach_proto::{Frame, Version};
    let mut resumed = FrameClient::start_with(
        root,
        &attach_json_in(4, session, Some(point)),
        &Frame::Resume {
            point,
            version: Version::V2,
        },
    );
    match resumed.frame() {
        Frame::Resumed {
            point: answer,
            input_written: Some(written),
        } if answer.epoch > point.epoch => (resumed, written),
        other => panic!("not resumed with an input report: {other:?}"),
    }
}

/// Send `bytes` as `client`'s keystrokes, in frames of 16 KiB, on a thread
/// that returns its stdin, still open: the client has lost its link, not
/// left, so the remote side goes on as sshd leaves it.
fn type_frames(
    client: &mut FrameClient,
    bytes: Vec<u8>,
) -> std::thread::JoinHandle<std::process::ChildStdin> {
    use tendr::attach_proto::{Frame, write_frame};
    let mut stdin = client.stdin.take().unwrap();
    std::thread::spawn(move || {
        for chunk in bytes.chunks(16 * 1024) {
            // A remote side retired meanwhile stops reading: the rest was
            // never sent.
            if write_frame(&mut stdin, &Frame::Data(chunk.to_vec())).is_err() {
                break;
            }
        }
        stdin
    })
}

/// Wait until `session`'s output log, its lines joined, holds `needle`.
fn wait_logged(root: &Home, session: &str, needle: &str) {
    wait_logged_times(root, session, needle, 1);
}

/// [`wait_logged`], `times` times over.
fn wait_logged_times(root: &Home, session: &str, needle: &str, times: usize) {
    poll_until(HANG, Duration::from_millis(50), || {
        let found = harness::logged_output(root, session)
            .matches(needle)
            .count();
        if found >= times {
            Observation::Ready(())
        } else {
            Observation::Pending(format!("{needle:?} logged {found} times"))
        }
    })
    .unwrap_or_else(|e| panic!("{session} never logged {needle:?} {times} times: {e}"));
}

/// A resume reports exactly the input the lost connection had written, and
/// none of what it had queued behind a child that was not reading, which
/// the resume revokes: the child, reading at last, finds exactly that many
/// bytes of it, then the first byte the resumed client sends.
#[test]
fn remote_resume_reports_the_input_written_and_not_the_input_revoked() {
    use tendr::attach_proto::{Frame, write_frame};
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    let session = "host-input-revoked";
    // The sidecar creates it once a write finds the PTY full: from then on
    // input waits in its queue, unwritten.
    let pty_full = root.path().join("pty-full");
    start_script(
        &root,
        session,
        "stty raw echo; printf READY; while [ ! -f \"$GO\" ]; do sleep 0.05; done; \
         head -c \"$(cat \"$N\")\" > \"$GOT\"; printf DONE; sleep 3600",
        &["GO", "N", "GOT"],
        &[("TENDR_TEST_PTY_FULL_FILE", pty_full.to_str().unwrap())],
    );
    wait_logged(&root, session, "READY");
    let (mut lost, accepted) = attach_v4(&root, session);
    let typing = type_frames(&mut lost, vec![b'a'; FLOOD]);
    // Some of it has been written: the terminal echoes what it takes in.
    wait_logged(&root, session, "a");
    // And some waits behind the full PTY, for the resume to revoke.
    harness::poll_path_exists(&pty_full, HANG, Duration::from_millis(20))
        .unwrap_or_else(|e| panic!("the PTY never filled: {e}"));

    let (mut resumed, written) = resume_v4(&root, session, accepted);
    assert!(
        written > 0 && written < FLOOD as u64,
        "{written} of {FLOOD} bytes written"
    );
    // Its echoes come first.
    while !matches!(lost.frame(), Frame::Retired(_)) {}
    assert_eq!(lost.child.wait().unwrap().code(), Some(81));
    drop(typing.join().unwrap());
    poll_until(HANG, Duration::from_millis(50), || {
        if harness::read_events(&root, session)
            .iter()
            .any(|e| e["kind"] == "pty.input_revoked")
        {
            Observation::Ready(())
        } else {
            Observation::Pending("no pty.input_revoked".into())
        }
    })
    .unwrap_or_else(|e| panic!("the queued rest was never revoked: {e}"));

    std::fs::write(root.path().join("N"), (written + 1).to_string()).unwrap();
    write_frame(resumed.stdin.as_mut().unwrap(), &Frame::Data(b"Z".to_vec())).unwrap();
    touch(&root, "GO");
    wait_logged(&root, session, "DONE");
    let got = std::fs::read(root.path().join("GOT")).unwrap();
    let mut expected = vec![b'a'; usize::try_from(written).unwrap()];
    expected.push(b'Z');
    assert!(
        got == expected,
        "the child read {} bytes, ending {:?}; {written} were reported written",
        got.len(),
        String::from_utf8_lossy(&got[got.len().saturating_sub(8)..])
    );
    assert_eq!(resumed.detach(), Some(0));
}

/// After a lost link, sshd keeps the old remote side open, and it goes on
/// passing the keystrokes it already had to the session. Here all of them
/// wait behind a child that is not reading until the link is lost; then
/// the child reads every one, and the resume reports every one written.
#[test]
fn remote_resume_counts_what_the_half_open_old_connection_wrote_after_the_drop() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    let session = "host-input-half-open";
    start_script(
        &root,
        session,
        &format!(
            "stty raw -echo; printf READY; while [ ! -f \"$GO\" ]; do sleep 0.05; done; \
             head -c {FLOOD} > \"$GOT\"; printf DONE; sleep 3600"
        ),
        &["GO", "GOT"],
        &[],
    );
    wait_logged(&root, session, "READY");
    let (mut lost, accepted) = attach_v4(&root, session);
    // Every byte is handed to the remote side, which the PTY cannot take
    // in yet; then the link is lost, with the remote side left open.
    let _half_open = type_frames(&mut lost, vec![b'a'; FLOOD]).join().unwrap();
    touch(&root, "GO");
    wait_logged(&root, session, "DONE");
    assert_eq!(std::fs::read(root.path().join("GOT")).unwrap().len(), FLOOD);

    let (resumed, written) = resume_v4(&root, session, accepted);
    assert_eq!(written, FLOOD as u64);
    assert_eq!(lost.child.wait().unwrap().code(), Some(81));
    assert_eq!(resumed.detach(), Some(0));
}

/// A PTY session that prints `READY` once `GO` exists, then echoes lines.
fn start_ready_cat(root: &Home, session: &str) {
    start_script(
        root,
        session,
        "while [ ! -f \"$GO\" ]; do sleep 0.05; done; printf READY; exec cat",
        &["GO"],
        &[],
    );
}

/// The keystrokes typed once before the link is lost in these tests, and
/// how many bytes the client sends for them.
const BEFORE_THE_DROP: &[u8] = b"before-the-drop\r";

/// `attach --host` through `link` to a [`start_ready_cat`] session, with
/// [`BEFORE_THE_DROP`] typed once and written, then the link cut.
fn type_then_cut(root: &Home, link: &Flaky, session: &str, label: &str) -> WrappedAttach {
    let mut wrapped = link.attach(root, label, &[session]);
    wait_control(root, session, "HumanControl");
    touch(root, "GO");
    wrapped.wait_output_contains("READY");
    wrapped.cli.type_bytes(BEFORE_THE_DROP);
    // The echo, then cat's copy: the line's end was written too.
    wait_logged_times(root, session, "before-the-drop", 2);
    link.cut();
    wrapped
}

/// A reconnected attach says how many of the keystroke bytes sent before
/// the link was lost reached the session, as it resumes and again on
/// stderr once the terminal is restored.
#[test]
fn host_reconnect_says_how_much_input_reached_the_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start_ready_cat(&root, "host-input-said");
    let link = Flaky::new();
    let mut wrapped = type_then_cut(&root, &link, "host-input-said", "input-said");
    let sent = BEFORE_THE_DROP.len();
    let notice = format!(
        "tendr: reconnected; of the {sent} bytes of input sent before the connection was lost, \
         {sent} reached the session and 0 did not"
    );
    wrapped.wait_output_contains(&notice);
    wrapped.cli.type_bytes(b"\x1cd");
    assert_eq!(wrapped.wait_exit(), 0);
    wrapped.assert_terminal_restored();
    assert_eq!(
        wrapped.cli.output().matches(&notice).count(),
        2,
        "on the screen, then on stderr"
    );
}

/// The first frames a sidecar from before the input report decodes: a
/// hello of versions 1 to 3, and a resume or a watch resume of version 2
/// or 3.
fn decoded_before_input_report(msg_type: u8, payload: &[u8]) -> bool {
    use tendr::attach_proto::{MSG_HELLO, MSG_RESUME, MSG_WATCH_RESUME};
    match (msg_type, payload) {
        (MSG_HELLO, [version, mode]) => (1..=3).contains(version) && (1..=4).contains(mode),
        (MSG_RESUME, point) => point.len() == 32 || (point.len() == 33 && point[32] == 3),
        (MSG_WATCH_RESUME, point) => point.len() == 24 || (point.len() == 25 && point[24] == 3),
        _ => false,
    }
}

/// A session whose sidecar predates the input report refuses the bridge's
/// version 4 hello and resume; it says each again in version 3, and the
/// attach reconnects as before, saying nothing of the input.
#[test]
fn host_reconnect_to_a_session_from_before_the_input_report_steps_down() {
    use tendr::attach_proto::{MSG_HELLO, MSG_RESUME};
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    let session = "host-input-old";
    start_ready_cat(&root, session);
    let seen = old_sidecar(&root, session, "pre-input", decoded_before_input_report);
    let link = Flaky::new();
    let mut wrapped = type_then_cut(&root, &link, session, "input-old");
    wait_resumes(&root, session, 1);
    wrapped
        .cli
        .type_until_logged(&root, session, "after-the-drop\n");
    wrapped.cli.type_bytes(b"\x1cd");
    assert_eq!(wrapped.wait_exit(), 0);
    wrapped.assert_terminal_restored();
    assert!(
        !wrapped.cli.output().contains("bytes of input"),
        "{}",
        wrapped.cli.output()
    );
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 4, "{seen:?}");
    let resume = |version: u8| {
        let mut payload = seen[2].1[..32].to_vec();
        payload.push(version);
        (MSG_RESUME, payload)
    };
    assert_eq!(
        seen,
        [
            (MSG_HELLO, vec![4, 1]),
            (MSG_HELLO, vec![3, 1]),
            resume(4),
            resume(3),
        ]
    );
}
