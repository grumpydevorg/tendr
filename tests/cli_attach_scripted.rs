//! The attach client against a scripted remote: the conformance oracle for
//! the client side of the attach protocol. The fake ssh's `script` mode
//! (`play` in `src/bin/fake_ssh.rs`) answers the client's first frame and
//! plays the session's side from a script, its frames built with
//! `attach_proto`, with no remote tendr or sidecar behind it. The client
//! runs on the debug builds' test terminal (`TENDR_TEST_ATTACH_TERMINAL`):
//! piped stdin is its keyboard and piped stdout its screen.
//!
//! The real-remote tests (`cli_remote_attach.rs`) stay the oracle for the
//! client and the sidecar agreeing; they need a Unix remote, while these run
//! on every client platform. The Windows console layer under them has its
//! own tests under a pseudo console (`windows_attach_console.rs`).

mod harness;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use harness::{Observation, SessionGuard, fake_ssh, poll_until, shim_path_env};
use tempfile::TempDir;
use tendr::attach_proto::{Frame, Mode, Version};
use tendr::recording::Geometry;

/// The test terminal's size.
const ROWS: u16 = 24;
const COLS: u16 = 80;
/// The default detach escape: `Ctrl-\` then `d`.
const DETACH: &[u8] = b"\x1cd";
const WAIT: Duration = Duration::from_secs(15);

/// `tendr` with `args` on the test terminal, its screen and stderr
/// collected as they arrive.
struct Client {
    child: Child,
    keyboard: Option<ChildStdin>,
    screen: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
}

impl Client {
    fn spawn(root: &TempDir, path: &std::ffi::OsStr, args: &[&str]) -> Self {
        let mut child = Command::new(assert_cmd::cargo::cargo_bin("tendr"))
            .args(args)
            .env("HOME", root.path())
            .env("PATH", path)
            .env("TENDR_TEST_ATTACH_TERMINAL", format!("{ROWS}x{COLS}"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let collect = |mut from: Box<dyn Read + Send>| {
            let into = Arc::new(Mutex::new(Vec::new()));
            let shared = Arc::clone(&into);
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n @ 1..) = from.read(&mut buf) {
                    shared.lock().unwrap().extend_from_slice(&buf[..n]);
                }
            });
            into
        };
        let screen = collect(Box::new(child.stdout.take().unwrap()));
        let stderr = collect(Box::new(child.stderr.take().unwrap()));
        Self {
            keyboard: child.stdin.take(),
            child,
            screen,
            stderr,
        }
    }

    fn screen(&self) -> String {
        String::from_utf8_lossy(&self.screen.lock().unwrap()).into_owned()
    }

    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().unwrap()).into_owned()
    }

    fn type_bytes(&mut self, bytes: &[u8]) {
        let keyboard = self.keyboard.as_mut().unwrap();
        keyboard.write_all(bytes).unwrap();
        keyboard.flush().unwrap();
    }

    /// Wait until the screen shows each of `needles`, in order.
    fn wait_screen(&self, needles: &[&str]) {
        poll_until(WAIT, Duration::from_millis(20), || {
            let screen = self.screen();
            let mut rest = screen.as_str();
            for needle in needles {
                match rest.find(needle) {
                    Some(at) => rest = &rest[at + needle.len()..],
                    None => return Observation::Pending(format!("screen: {screen:?}")),
                }
            }
            Observation::Ready(())
        })
        .unwrap_or_else(|e| panic!("never showed {needles:?}: {e}"));
    }

    fn wait_exit(&mut self) -> i32 {
        let status = poll_until(WAIT, Duration::from_millis(20), || {
            match self.child.try_wait().unwrap() {
                Some(status) => Observation::Ready(status),
                None => Observation::Pending("still running".into()),
            }
        })
        .unwrap_or_else(|e| {
            let _ = self.child.kill();
            panic!("the client never exited: {e}; stderr: {}", self.stderr())
        });
        status.code().expect("the client exited with a code")
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A scripted remote, and the log its `log` steps keep of the frames the
/// client sent.
struct Remote {
    ssh: TempDir,
    _dir: TempDir,
    frames: PathBuf,
}

impl Remote {
    /// `script` with `{log}` standing for the frames log.
    fn new(script: &str) -> Self {
        let dir = TempDir::new().unwrap();
        let frames = dir.path().join("frames");
        let script = script.replace("{log}", frames.to_str().unwrap());
        Self {
            ssh: fake_ssh(&format!("script\n{script}")),
            _dir: dir,
            frames,
        }
    }

    /// `tendr --host box attach s` against this remote.
    fn attach(&self, root: &TempDir) -> Client {
        Client::spawn(
            root,
            &shim_path_env(&self.ssh),
            &["--host", "box", "attach", "s"],
        )
    }

    /// The frames the client sent so far, one per line.
    fn frames(&self) -> Vec<String> {
        read_lines(&self.frames)
    }

    /// Wait until the client has sent `frame`.
    fn wait_frame(&self, frame: &Frame) {
        let line = format!("{frame:?}");
        poll_until(WAIT, Duration::from_millis(20), || {
            let frames = self.frames();
            if frames.contains(&line) {
                Observation::Ready(())
            } else {
                Observation::Pending(format!("frames: {frames:?}"))
            }
        })
        .unwrap_or_else(|e| panic!("never sent {line}: {e}"));
    }
}

fn read_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn debug(frames: &[Frame]) -> Vec<String> {
    frames.iter().map(|f| format!("{f:?}")).collect()
}

fn greeting() -> [Frame; 2] {
    [
        Frame::Resize(Geometry::new(ROWS, COLS).unwrap()),
        Frame::Repaint,
    ]
}

/// The client says hello, greets with its size and the repaint nudge,
/// shows the session's output, sends what is typed, and detaches on the
/// escape with exit 0.
#[test]
fn a_scripted_attach_greets_shows_output_types_and_detaches() {
    let root = TempDir::new().unwrap();
    let remote = Remote::new(
        "hello log {log}\n\
         hello answer\n\
         hello output scripted-output\n\
         hello drain",
    );
    let mut client = remote.attach(&root);
    client.wait_screen(&["scripted-output"]);
    client.type_bytes(b"ab");
    remote.wait_frame(&Frame::Data(b"ab".to_vec()));
    client.type_bytes(DETACH);
    assert_eq!(client.wait_exit(), 0, "{}", client.stderr());

    let [resize, repaint] = greeting();
    assert_eq!(
        remote.frames(),
        debug(&[
            Frame::Hello {
                version: Version::V1,
                mode: Mode::Attach,
            },
            resize,
            repaint,
            Frame::Data(b"ab".to_vec()),
            Frame::Detach,
        ])
    );
}

/// Told another client took over, the client exits 81 and says so.
#[test]
fn a_scripted_takeover_exits_81() {
    let root = TempDir::new().unwrap();
    let remote = Remote::new(
        "hello answer\n\
         hello await Repaint\n\
         hello output before-takeover\n\
         hello retire\n\
         hello exit 81",
    );
    let mut client = remote.attach(&root);
    assert_eq!(client.wait_exit(), 81, "{}", client.stderr());
    assert!(client.screen().contains("before-takeover"));
    assert!(
        client
            .stderr()
            .contains("another client took over this session"),
        "{}",
        client.stderr()
    );
}

/// A link lost without the remote's verdict is resumed from just past the
/// last byte shown, over a new ssh; the output goes on from there, and the
/// new connection is greeted as a fresh one is.
#[test]
fn a_scripted_drop_resumes_where_the_screen_stopped() {
    let root = TempDir::new().unwrap();
    let remote = Remote::new(
        "hello answer\n\
         hello await Repaint\n\
         hello output one\n\
         hello drop\n\
         resume log {log}\n\
         resume answer\n\
         resume output two\n\
         resume drain",
    );
    let mut client = remote.attach(&root);
    client.wait_screen(&["one", "two"]);
    client.type_bytes(DETACH);
    assert_eq!(client.wait_exit(), 0, "{}", client.stderr());

    let frames = remote.frames();
    let [resize, repaint] = greeting();
    assert_eq!(frames[1..], debug(&[resize, repaint, Frame::Detach]));
    assert!(
        frames[0].starts_with("Resume(") && frames[0].contains("offset: 3 "),
        "the resume asks for the offset past \"one\": {frames:?}"
    );
}

/// A resume the session can no longer serve from where the screen stopped
/// says how much output was missed and how much was lost, then goes on.
#[test]
fn a_scripted_resume_that_is_gone_says_what_was_missed() {
    let root = TempDir::new().unwrap();
    let remote = Remote::new(
        "hello answer\n\
         hello await Repaint\n\
         hello output one\n\
         hello drop\n\
         resume log {log}\n\
         resume gone 5\n\
         resume output two\n\
         resume drain",
    );
    let mut client = remote.attach(&root);
    client.wait_screen(&[
        "one",
        "reconnected; 5 bytes of output were written while disconnected, \
         and the session no longer held 1 of them",
        "two",
    ]);
    client.type_bytes(DETACH);
    assert_eq!(client.wait_exit(), 0, "{}", client.stderr());
}

/// A read-only attach drops what is typed and says so once on the screen,
/// through the terminal layer like every write. `attach --host` refuses
/// `--read-only` before ssh for now, so this one is local, against a real
/// session.
#[test]
fn a_read_only_attach_says_typing_is_dropped() {
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "ro");
    let started = harness::tendr(&root)
        .args(["start", "ro", "--pty", "--stdin", "--", "sh"])
        .output()
        .unwrap();
    assert!(started.status.success(), "{started:?}");
    harness::wait_running(&root, "ro");

    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut client = Client::spawn(&root, &path, &["attach", "--read-only", "ro"]);
    client.type_bytes(b"x");
    client.wait_screen(&["tendr: read-only attach; typed input is dropped"]);
    client.type_bytes(DETACH);
    assert_eq!(client.wait_exit(), 0, "{}", client.stderr());
}
