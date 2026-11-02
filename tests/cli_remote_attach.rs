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

/// The ssh argv of every `--host` operation, attach included.
const REMOTE_ARGV: [&str; 8] = [
    "-T",
    "-o",
    "ConnectTimeout=10",
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
        attach_through(&self.ssh, "30", root, label, args)
    }
}

/// `tendr --host box attach <args>` in a fresh terminal, through the fake ssh
/// in `ssh`, with a connect deadline of `deadline` seconds.
fn attach_through(
    ssh: &TempDir,
    deadline: &str,
    root: &TempDir,
    label: &str,
    args: &[&str],
) -> WrappedAttach {
    let argv: Vec<String> = [tendr_bin(), "--host".into(), "box".into(), "attach".into()]
        .into_iter()
        .chain(args.iter().map(|a| (*a).to_owned()))
        .collect();
    let path = shim_path_env(ssh);
    let tendr = tendr_bin();
    WrappedAttach::spawn_argv_env(
        root,
        label,
        &argv,
        &[
            ("PATH", path.to_str().unwrap()),
            ("TENDR_TEST_BIN", &tendr),
            ("TENDR_SSH_CONNECT_TIMEOUT", deadline),
        ],
    )
}

impl Remote {
    /// The last call through this remote ran the constant ssh argv.
    fn assert_constant_argv(&self) {
        let recorded = std::fs::read_to_string(&self.argv).unwrap();
        let argv: Vec<&str> = recorded
            .lines()
            .filter_map(|l| l.strip_prefix("ARG:"))
            .collect();
        assert_eq!(argv, REMOTE_ARGV);
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
    let wrapped = attach_through(&silent, "1", &root, "deadline", &["s"]);
    assert_eq!(wrapped.wait_exit(Duration::from_secs(20)), 82);
    wrapped.assert_terminal_restored();
    wrapped.wait_output_contains("did not reach the remote tendr");

    let lost = fake_ssh(&format!("err {}\\n\nexit 255", tendr::ssh::HELLO_LINE));
    let wrapped = attach_through(&lost, "30", &root, "lost", &["s"]);
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
        use std::io::Write;
        use std::process::Stdio;
        use tendr::attach_proto::{Frame, Mode, Version, write_frame};
        let mut child = std::process::Command::new(tendr_bin())
            .arg("_remote")
            .env("HOME", root.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut request = u32::try_from(json.len()).unwrap().to_be_bytes().to_vec();
        request.extend_from_slice(json.as_bytes());
        write_frame(
            &mut request,
            &Frame::Hello {
                version: Version::V1,
                mode: Mode::Attach,
            },
        )
        .unwrap();
        stdin.write_all(&request).unwrap();
        let stdout = child.stdout.take().unwrap();
        Self {
            child,
            stdin: Some(stdin),
            stdout,
        }
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
    let wrapped = attach_through(&ssh, "1", &root, "old-remote", &["s", "--takeover"]);
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
