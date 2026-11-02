//! `attach --host` from a Unix client, end to end through the attach bridge.
//! The fake ssh (`src/bin/fake_ssh.rs`) writes down its argv, then acts as
//! the remote login shell and runs the real tendr, whose `_remote` entry
//! point bridges the attach protocol to the session's socket. Both sides
//! share one HOME. The local tendr runs in a terminal, as a person's would:
//! raw mode, resizes and the detach escape are all on its side.
#![cfg(unix)]

mod harness;

use harness::terminal::{WrappedAttach, tendr_bin};
use harness::{Observation, SessionGuard, fake_ssh, poll_until, shim_path_env, tendr};
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
    fn attach(&self, root: &TempDir, label: &str, args: &[&str]) -> WrappedAttach {
        attach_through(&self.ssh, "30", root, label, args, &[])
    }
}

/// `tendr --host box attach <args>` in a fresh terminal, through the fake ssh
/// in `ssh`, with a connect deadline of `deadline` seconds and `env` added.
fn attach_through(
    ssh: &TempDir,
    deadline: &str,
    root: &TempDir,
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

fn start_pty(root: &TempDir, session: &str, argv: &[&str]) {
    let output = tendr(root)
        .args(["start", session, "--pty", "--stdin", "--"])
        .args(argv)
        .output()
        .unwrap();
    assert!(output.status.success(), "start {session}: {output:?}");
    harness::wait_running(root, session);
}

fn control(root: &TempDir, session: &str) -> Option<String> {
    let meta = root
        .path()
        .join(format!(".tendr/sessions/default/{session}/meta.json"));
    let meta: serde_json::Value = serde_json::from_slice(&std::fs::read(meta).ok()?).ok()?;
    meta["pty"]["control"].as_str().map(str::to_owned)
}

fn wait_control(root: &TempDir, session: &str, expected: &str) {
    poll_until(
        Duration::from_secs(15),
        Duration::from_millis(50),
        || match control(root, session) {
            Some(c) if c == expected => Observation::Ready(()),
            other => Observation::Pending(format!("pty.control={other:?}")),
        },
    )
    .unwrap_or_else(|e| panic!("{session} never reached pty.control={expected}: {e}"));
}

/// Type into the session and see its output, resize it, then detach with the
/// escape: the terminal comes back as it was and the session keeps running
/// under agent control. The session's name would not survive a remote shell.
#[test]
fn host_attach_types_sees_output_resizes_and_detaches() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, HOSTILE_NAME);
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
    poll_until(Duration::from_secs(15), Duration::from_millis(300), || {
        wrapped.cli.type_bytes(b"stty size\n");
        if wrapped.cli.output().contains("33 111") {
            Observation::Ready(())
        } else {
            Observation::Pending("the session has not seen 33 111".into())
        }
    })
    .unwrap_or_else(|e| panic!("the resize never arrived ({e}): {}", wrapped.cli.output()));

    wrapped.cli.type_bytes(b"\x1cd");
    assert_eq!(
        wrapped.wait_exit(Duration::from_secs(15)),
        0,
        "{}",
        wrapped.cli.output()
    );
    wrapped.assert_terminal_restored();
    wait_control(&root, HOSTILE_NAME, "AgentControl");
}

/// A paced flood: at most 800 KB/s, so a few seconds of stall stay well
/// inside the sidecar's 32 MiB output ring and the client is never dropped
/// for falling behind, yet fill every buffer between the sidecar and a
/// terminal that stopped reading.
const PACED_FLOOD: &str = "stty raw -echo; line=$(printf '%01000d' 0); \
     while :; do i=0; while [ $i -lt 8 ]; do printf '%s' \"$line\"; i=$((i+1)); done; sleep 0.01; done";

/// A client whose terminal stopped reading still detaches at once: its
/// detach does not wait behind the output it has not read.
#[test]
fn host_detach_is_not_held_up_by_unread_output() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "host-stall-detach");
    start_pty(&root, "host-stall-detach", &["sh", "-c", PACED_FLOOD]);
    let remote = Remote::new();

    let mut wrapped = remote.attach(&root, "stall-detach", &["host-stall-detach"]);
    wrapped.wait_output_contains("0000000000");
    wait_control(&root, "host-stall-detach", "HumanControl");
    wrapped.cli.pause_output();
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        control(&root, "host-stall-detach").as_deref(),
        Some("HumanControl"),
        "the stalled client still holds control"
    );

    wrapped.cli.type_bytes(b"\x1cd");
    wait_control(&root, "host-stall-detach", "AgentControl");
    let code = wrapped.wait_exit(Duration::from_secs(15));
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
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "host-take");
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
    std::thread::sleep(Duration::from_secs(3));
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
    assert_eq!(
        first.wait_exit(Duration::from_secs(30)),
        81,
        "the stalled client was taken over"
    );
    first.assert_terminal_restored();
    first.wait_output_contains("another client took over this session");

    second.cli.type_bytes(b"\x1cd");
    assert_eq!(second.wait_exit(Duration::from_secs(15)), 0);
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
fn fake_sidecar(root: &TempDir, session: &str, label: &str, when: FakeReply, reply: Vec<u8>) {
    use std::io::Write;
    let sock = root.path().join(format!("{label}.sock"));
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().map_while(Result::ok) {
            if matches!(when, FakeReply::AfterHello) {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
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

fn point_breadcrumb_at(root: &TempDir, session: &str, sock: &std::path::Path) {
    let dir = root
        .path()
        .join(format!(".tendr/sessions/default/{session}"));
    let tmp = dir.join("a.sock.path.test");
    std::fs::write(&tmp, sock.to_str().unwrap()).unwrap();
    std::fs::rename(&tmp, dir.join("a.sock.path")).unwrap();
}

/// Attach to `args` through the bridge and return its exit code and what its
/// terminal showed; the terminal is checked restored.
fn host_attach_exit(root: &TempDir, remote: &Remote, label: &str, args: &[&str]) -> (i32, String) {
    let wrapped = remote.attach(root, label, args);
    let code = wrapped.wait_exit(Duration::from_secs(30));
    wrapped.assert_terminal_restored();
    (code, wrapped.cli.output())
}

/// The remote side's checks and the sidecar's refusals keep the exit codes
/// of a local attach (80, 81, 84, 85, and 1 for a session that cannot be
/// attached at all), with the remote's message on stderr.
#[test]
fn host_attach_keeps_the_exit_codes() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let remote = Remote::new();

    let (code, out) = host_attach_exit(&root, &remote, "missing", &["no-such"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("session not found"), "{out}");
    remote.assert_constant_argv();

    let _pipe = SessionGuard::new(&root, "host-pipe");
    tendr(&root)
        .args(["start", "host-pipe", "--", "sleep", "30"])
        .output()
        .unwrap();
    harness::wait_running(&root, "host-pipe");
    let (code, out) = host_attach_exit(&root, &remote, "pipe", &["host-pipe"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("not PTY-enabled"), "{out}");

    let _ended = SessionGuard::new(&root, "host-ended");
    tendr(&root)
        .args(["start", "host-ended", "--pty", "--", "true"])
        .output()
        .unwrap();
    harness::wait_terminal(&root, "host-ended");
    let (code, out) = host_attach_exit(&root, &remote, "ended", &["host-ended"]);
    assert_eq!(code, 81, "{out}");

    // Held by a human: the remote check refuses a plain attach.
    let _held = SessionGuard::new(&root, "host-held");
    start_pty(&root, "host-held", &["cat"]);
    let holder = remote.attach(&root, "holder", &["host-held"]);
    wait_control(&root, "host-held", "HumanControl");
    let (code, out) = host_attach_exit(&root, &remote, "held", &["host-held"]);
    assert_eq!(code, 81, "{out}");
    assert!(out.contains("human control"), "{out}");
    // A signal ends a bridged attach as it ends a local one: exit 1.
    holder.signal(libc::SIGTERM);
    assert_eq!(holder.wait_exit(Duration::from_secs(15)), 1);
    holder.assert_terminal_restored();
    wait_control(&root, "host-held", "AgentControl");

    // A breadcrumb naming a socket nobody listens on.
    let _dead = SessionGuard::new(&root, "host-dead");
    start_pty(&root, "host-dead", &["cat"]);
    let dead = root.path().join("dead.sock");
    drop(std::os::unix::net::UnixListener::bind(&dead).unwrap());
    point_breadcrumb_at(&root, "host-dead", &dead);
    let (code, out) = host_attach_exit(&root, &remote, "dead", &["host-dead"]);
    assert_eq!(code, 84, "{out}");

    // A sidecar that speaks another protocol, and one that refuses the peer.
    let _fake = SessionGuard::new(&root, "host-fake");
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
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "host-hello");
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
            .timeout(Duration::from_secs(30))
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
    let root = TempDir::new().unwrap();

    let silent = fake_ssh("sleep 30");
    let wrapped = attach_through(&silent, "1", &root, "deadline", &["s"], &[]);
    assert_eq!(wrapped.wait_exit(Duration::from_secs(20)), 82);
    wrapped.assert_terminal_restored();
    wrapped.wait_output_contains("did not reach the remote tendr");

    let lost = fake_ssh(&format!("err {}\\n\nexit 255", tendr::ssh::HELLO_LINE));
    let wrapped = attach_through(&lost, "30", &root, "lost", &["s"], &[]);
    assert_eq!(wrapped.wait_exit(Duration::from_secs(20)), 1);
    wrapped.assert_terminal_restored();
    wrapped.wait_output_contains("ssh transport failed");
}

/// A client that vanishes without detaching (its tendr killed outright)
/// gives the terminal back to the agent: the remote side sees its input end
/// and closes the connection.
#[test]
fn host_attach_client_killed_gives_control_back() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "host-killed");
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
    root: &TempDir,
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
            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
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
fn v1_only_sidecar(root: &TempDir, session: &str) -> Hellos {
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
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "host-old-sidecar");
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
    assert_eq!(
        wrapped.wait_exit(Duration::from_secs(15)),
        0,
        "{}",
        wrapped.cli.output()
    );
    wrapped.assert_terminal_restored();
    wait_control(&root, "host-old-sidecar", "AgentControl");
    assert_eq!(
        *hellos.lock().unwrap(),
        [vec![2, 1], vec![1, 1]],
        "version 2 first, then version 1 once"
    );
}

/// The bridge speaks version 2 to the sidecar and passes its acceptance and
/// stamped output to the client, which writes output that follows on with no
/// gap and refuses output that skips: a protocol failure (80), after which
/// the terminal is restored.
#[test]
fn host_attach_tracks_offsets_and_refuses_output_that_skips() {
    use tendr::attach_proto::{Frame, ResumePoint, write_frame};
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "host-skip");
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
    assert_eq!(wrapped.wait_exit(Duration::from_secs(15)), 80);
    wrapped.assert_terminal_restored();
    let output = wrapped.cli.output();
    assert!(output.contains("first-second-"), "{output}");
    assert!(!output.contains("after-the-gap"), "{output}");
    // The offset it expected: 100, then 6 and 7 bytes written.
    assert!(output.contains("113") && output.contains("200"), "{output}");
    assert_eq!(*hellos.lock().unwrap(), [vec![2, 1]]);
}

/// `tendr _remote` run by a client that speaks the attach protocol itself,
/// at the frame level: `json` is its request, a version 1 hello follows.
struct FrameClient {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: std::process::ChildStdout,
}

impl FrameClient {
    fn start(root: &TempDir, json: &str) -> Self {
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

    /// [`FrameClient::start`], with `first` in place of the hello.
    fn start_with(root: &TempDir, json: &str, first: &tendr::attach_proto::Frame) -> Self {
        use std::io::Write;
        use std::process::Stdio;
        let mut child = std::process::Command::new(tendr_bin())
            .arg("_remote")
            .env("HOME", root.path())
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
fn start_marker(root: &TempDir, session: &str, go: &std::path::Path, marker: &str) {
    let script = format!("while [ ! -f \"$GO\" ]; do sleep 0.05; done; printf {marker}; sleep 60");
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
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "host-v2");
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
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "host-v1");
    let go = root.path().join("go");
    start_marker(&root, "host-v1", &go, "v1-marker");

    let mut client = FrameClient::start(
        &root,
        r#"{"v":1,"op":"attach","params":{"session":"host-v1","namespace":null,"takeover":false,"escape":"ctrl-backslash"}}"#,
    );
    assert_eq!(client.read(), (MSG_ACCEPTED, 1u64.to_be_bytes().to_vec()));
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
    let root = TempDir::new().unwrap();
    let sent = root.path().join("sent");
    let ssh = fake_ssh(&format!("stdin {}", sent.display()));
    let wrapped = attach_through(&ssh, "1", &root, "old-remote", &["s", "--takeover"], &[]);
    assert_eq!(wrapped.wait_exit(Duration::from_secs(20)), 82);
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
    let resume = resume.map_or(String::new(), |p| {
        format!(
            r#","resume":{{"run_id":"{}","epoch":{},"offset":{}}}"#,
            p.run_id.as_uuid(),
            p.epoch.get(),
            p.offset
        )
    });
    format!(
        r#"{{"v":1,"op":"attach","params":{{"session":"{session}","namespace":null,"takeover":false,"escape":"ctrl-backslash","protocol":2{resume}}}}}"#
    )
}

/// A version 2 attach through `_remote`, and the point it was accepted at.
fn attach_v2(root: &TempDir, session: &str) -> (FrameClient, tendr::attach_proto::ResumePoint) {
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
    use tendr::attach_proto::{Frame, ResumePoint};
    use tendr::model::pty_control::ControllerEpoch;
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "host-resume");
    start_pty(&root, "host-resume", &["cat"]);

    let (mut lost, accepted) = attach_v2(&root, "host-resume");
    wait_control(&root, "host-resume", "HumanControl");

    let mut resumed = FrameClient::start_with(
        &root,
        &attach_json("host-resume", Some(accepted)),
        &Frame::Resume(accepted),
    );
    assert_eq!(
        resumed.frame(),
        Frame::Resumed(ResumePoint {
            epoch: ControllerEpoch::new(2),
            ..accepted
        })
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
    use tendr::attach_proto::{Frame, RejectClass, ResumePoint, write_frame};
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "host-old-resume");
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
    write_frame(&mut stdin, &Frame::Resume(point)).unwrap();

    let output = tendr(&root)
        .arg("_remote")
        .write_stdin(stdin)
        .timeout(Duration::from_secs(30))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(80), "{output:?}");
    assert!(output.stdout.is_empty(), "no frame is sent");
    let mut resume = Vec::new();
    write_frame(&mut resume, &Frame::Resume(point)).unwrap();
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
    fn attach(&self, root: &TempDir, label: &str, args: &[&str]) -> WrappedAttach {
        self.attach_with_deadline("30", root, label, args)
    }

    /// [`Flaky::attach`] with a connect deadline of `deadline` seconds; `0`
    /// turns it off, so no hello marks the remote tendr's start.
    fn attach_with_deadline(
        &self,
        deadline: &str,
        root: &TempDir,
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
        poll_until(Duration::from_secs(60), Duration::from_millis(50), || {
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
        poll_until(Duration::from_secs(30), Duration::from_millis(20), || {
            match std::fs::read_to_string(self.files.path().join("pid"))
                .ok()
                .and_then(|p| p.trim().parse().ok())
            {
                Some(pid) if pid != old => Observation::Ready(pid),
                other => Observation::Pending(format!("pid {other:?}")),
            }
        })
        .unwrap_or_else(|e| panic!("no ssh call after {old}: {e}"))
    }
}

/// Start a PTY session running `script` under `sh -c`. Each of `files` is
/// set in its environment to that name's path in `root`, for the script to
/// wait on; `env` goes to the sidecar.
fn start_script(root: &TempDir, session: &str, script: &str, files: &[&str], env: &[(&str, &str)]) {
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

fn touch(root: &TempDir, name: &str) {
    std::fs::write(root.path().join(name), b"").unwrap();
}

/// Wait until the run has granted control to `n` resumes.
fn wait_resumes(root: &TempDir, session: &str, n: usize) {
    poll_until(Duration::from_secs(60), Duration::from_millis(50), || {
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
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "host-cut");
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
    poll_until(Duration::from_secs(10), Duration::from_millis(50), || {
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
    assert_eq!(wrapped.wait_exit(Duration::from_secs(15)), 0);
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
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "host-quiet");
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
    assert_eq!(wrapped.wait_exit(Duration::from_secs(15)), 0);

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
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "host-backoff");
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
    assert_eq!(wrapped.wait_exit(Duration::from_secs(15)), 0);
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
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "host-gap-take");
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
    assert_eq!(wrapped.wait_exit(Duration::from_secs(30)), 81);
    wrapped.assert_terminal_restored();
    let said = without_status_line(&wrapped.cli.output()[before..]);
    assert!(!said.trim().is_empty(), "the refusal says why");

    taker.cli.type_bytes(b"\x1cd");
    assert_eq!(taker.wait_exit(Duration::from_secs(15)), 0);
    taker.assert_terminal_restored();
}

/// The run ends while the attach is reconnecting: the reconnect finds it
/// not running, and the attach exits 81.
#[test]
fn host_reconnect_after_the_run_ended_exits_81() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "host-gap-end");
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
    assert_eq!(wrapped.wait_exit(Duration::from_secs(30)), 81);
    wrapped.assert_terminal_restored();
}

/// The escape and a signal still leave while the attach is reconnecting,
/// and stop the ssh of the attempt under way: one waiting for the remote's
/// hello, and one waiting for its answer with no hello to wait for (the
/// connect deadline off).
#[test]
fn host_escape_or_signal_while_reconnecting_leaves_and_stops_ssh() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "host-gap-leave");
    start_pty(&root, "host-gap-leave", &["cat"]);
    let link = Flaky::new();

    // The escape: exit 0.
    let mut wrapped = link.attach_with_deadline("0", &root, "gap-escape", &["host-gap-leave"]);
    wrapped
        .cli
        .type_until_logged(&root, "host-gap-leave", "escape-client\n");
    link.set("stall", true);
    let cut = link.cut();
    let stalled = link.wait_new_pid(cut);
    wrapped.cli.type_bytes(b"\x1cd");
    assert_eq!(wrapped.wait_exit(Duration::from_secs(15)), 0);
    wrapped.assert_terminal_restored();
    harness::wait_pid_dead(stalled);
    link.set("stall", false);

    // SIGTERM: exit 1. The old remote side, still open, holds the session,
    // so this attach takes it over.
    let mut wrapped = link.attach(&root, "gap-term", &["host-gap-leave", "--takeover"]);
    wrapped
        .cli
        .type_until_logged(&root, "host-gap-leave", "term-client\n");
    link.set("stall", true);
    let cut = link.cut();
    let stalled = link.wait_new_pid(cut);
    wrapped.signal(libc::SIGTERM);
    assert_eq!(wrapped.wait_exit(Duration::from_secs(15)), 1);
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
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "host-gone");
    // About 2 MB of `x` lines, far past a 64 KiB ring, then a marker. The
    // pause keeps the flood out of the marker's chunk: capture logs a chunk
    // just before it adds it to the ring, so only that chunk can lag.
    let script = format!(
        "while [ ! -f \"$GO1\" ]; do sleep 0.05; done; printf '\\033[?2004h'; printf first-part; \
         while [ ! -f \"$GO2\" ]; do sleep 0.05; done; yes {} | head -n 20000; \
         sleep 0.5; echo FLOOD-END; \
         while [ ! -f \"$GO3\" ]; do sleep 0.05; done; printf after-the-gap; sleep 60",
        "x".repeat(100)
    );
    start_script(
        &root,
        "host-gone",
        &script,
        &["GO1", "GO2", "GO3"],
        &[("TENDR_TEST_OUTPUT_RING_BYTES", "65536")],
    );
    let link = Flaky::new();

    let mut wrapped = link.attach(&root, "gone", &["host-gone"]);
    wait_control(&root, "host-gone", "HumanControl");
    touch(&root, "GO1");
    wrapped.wait_output_contains("first-part");
    hold_in_reconnect(&link);
    touch(&root, "GO2");
    // The flood has all been captured: its end marker is in the tail of
    // output.log, whose lines follow capture's reads, so they are joined.
    let log = root
        .path()
        .join(".tendr/sessions/default/host-gone/output.log");
    poll_until(Duration::from_secs(60), Duration::from_millis(100), || {
        use std::io::{Read, Seek, SeekFrom};
        let mut tail = Vec::new();
        if let Ok(mut file) = std::fs::File::open(&log) {
            let _ = file.seek(SeekFrom::End(-4096));
            let _ = file.read_to_end(&mut tail);
        }
        let text: String = tail
            .split(|&b| b == b'\n')
            .filter_map(|l| serde_json::from_slice::<serde_json::Value>(l).ok())
            .filter_map(|l| l["content"].as_str().map(str::to_owned))
            .collect();
        if text.contains("FLOOD-END") {
            Observation::Ready(())
        } else {
            Observation::Pending(text)
        }
    })
    .unwrap();

    link.set("prompt", false);
    wait_resumes(&root, "host-gone", 1);
    touch(&root, "GO3");
    wrapped.wait_output_contains("after-the-gap");
    wrapped.cli.type_bytes(b"\x1cd");
    assert_eq!(wrapped.wait_exit(Duration::from_secs(15)), 0);
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
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "host-resume-take");
    let child = format!("({PACED_FLOOD}) & exec cat -v");
    start_pty(&root, "host-resume-take", &["sh", "-c", &child]);
    let link = Flaky::new();

    let first = link.attach(&root, "take-1", &["host-resume-take"]);
    first.wait_output_contains("0000000000");
    link.cut();
    wait_resumes(&root, "host-resume-take", 1);
    first.cli.pause_output();
    std::thread::sleep(Duration::from_secs(3));

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
        first.wait_exit(Duration::from_secs(30)),
        81,
        "the stalled reconnected client was taken over"
    );
    first.assert_terminal_restored();
    first.wait_output_contains("another client took over this session");

    second.cli.type_bytes(b"\x1cd");
    assert_eq!(second.wait_exit(Duration::from_secs(15)), 0);
    second.assert_terminal_restored();
}
