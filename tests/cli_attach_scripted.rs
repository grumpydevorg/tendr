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

use harness::{Observation, fake_ssh, poll_until, shim_path_env};
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
    /// The threads collecting them, done once both pipes are closed.
    collectors: Vec<std::thread::JoinHandle<()>>,
}

impl Client {
    fn spawn(root: &TempDir, path: &std::ffi::OsStr, args: &[&str]) -> Self {
        Self::spawn_on(root, path, args, true, None)
    }

    /// `tendr` with `args`, on the test terminal if `terminal`, or else with
    /// no terminal at all: stdin closed, stdout a pipe. With a `hold`, the
    /// test terminal takes no writes while that file exists.
    fn spawn_on(
        root: &TempDir,
        path: &std::ffi::OsStr,
        args: &[&str],
        terminal: bool,
        hold: Option<&Path>,
    ) -> Self {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("tendr"));
        command
            .args(args)
            .env("HOME", root.path())
            .env("PATH", path)
            .env_remove("TENDR_TEST_ATTACH_SCREEN_HOLD")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if terminal {
            command
                .env("TENDR_TEST_ATTACH_TERMINAL", format!("{ROWS}x{COLS}"))
                .stdin(Stdio::piped());
        } else {
            command
                .env_remove("TENDR_TEST_ATTACH_TERMINAL")
                .stdin(Stdio::null());
        }
        if let Some(hold) = hold {
            command.env("TENDR_TEST_ATTACH_SCREEN_HOLD", hold);
        }
        let mut child = command.spawn().unwrap();
        let mut collectors = Vec::new();
        let mut collect = |mut from: Box<dyn Read + Send>| {
            let into = Arc::new(Mutex::new(Vec::new()));
            let shared = Arc::clone(&into);
            collectors.push(std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n @ 1..) = from.read(&mut buf) {
                    shared.lock().unwrap().extend_from_slice(&buf[..n]);
                }
            }));
            into
        };
        let screen = collect(Box::new(child.stdout.take().unwrap()));
        let stderr = collect(Box::new(child.stderr.take().unwrap()));
        Self {
            keyboard: child.stdin.take(),
            child,
            screen,
            stderr,
            collectors,
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

    /// Wait for the client to exit, and for what it wrote before exiting
    /// to be collected: the pipes may still hold it when it has exited.
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
        poll_until(WAIT, Duration::from_millis(20), || {
            if self
                .collectors
                .iter()
                .all(std::thread::JoinHandle::is_finished)
            {
                Observation::Ready(())
            } else {
                Observation::Pending("its output pipes are still open".into())
            }
        })
        .unwrap_or_else(|e| panic!("the client's output never ended: {e}"));
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
        Self::after("", script)
    }

    /// [`Remote::new`], with the fake ssh's `actions` (each `{drop}`
    /// standing for the file [`Remote::drop_link`] creates) run before the
    /// script.
    fn after(actions: &str, script: &str) -> Self {
        let dir = TempDir::new().unwrap();
        let frames = dir.path().join("frames");
        let drop = dir.path().join("drop");
        let script = script.replace("{log}", frames.to_str().unwrap());
        let actions = actions.replace("{drop}", drop.to_str().unwrap());
        Self {
            ssh: fake_ssh(&format!("{actions}\nscript\n{script}")),
            _dir: dir,
            frames,
        }
    }

    /// Lose the link of a remote made [`Remote::after`] `give-up-if {drop}`:
    /// the call under way says ssh's keepalive timeout and exits 255.
    fn drop_link(&self) {
        std::fs::write(self.frames.with_file_name("drop"), b"").unwrap();
    }

    /// `tendr --host box attach s` against this remote.
    fn attach(&self, root: &TempDir) -> Client {
        Client::spawn(
            root,
            &shim_path_env(&self.ssh),
            &["--host", "box", "attach", "s"],
        )
    }

    /// [`Remote::attach`], on a screen that takes no writes while `hold`
    /// exists.
    fn attach_held(&self, root: &TempDir, hold: &Path) -> Client {
        Client::spawn_on(
            root,
            &shim_path_env(&self.ssh),
            &["--host", "box", "attach", "s"],
            true,
            Some(hold),
        )
    }

    /// `tendr --host box attach --read-only s` against this remote, on the
    /// test terminal if `terminal`.
    fn watch(&self, root: &TempDir, terminal: bool) -> Client {
        Client::spawn_on(
            root,
            &shim_path_env(&self.ssh),
            &["--host", "box", "attach", "--read-only", "s"],
            terminal,
            None,
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

/// Keystrokes read together with the escape go out before the detach,
/// never after it, where the session would no longer read them (#111).
#[test]
fn keystrokes_typed_with_the_escape_go_out_before_the_detach() {
    let root = TempDir::new().unwrap();
    let remote = Remote::new(
        "hello log {log}\n\
         hello answer\n\
         hello output scripted-output\n\
         hello drain",
    );
    let mut client = remote.attach(&root);
    client.wait_screen(&["scripted-output"]);
    let mut typed = b"typed-before".to_vec();
    typed.extend_from_slice(DETACH);
    client.type_bytes(&typed);
    assert_eq!(client.wait_exit(), 0, "{}", client.stderr());

    let frames = remote.frames();
    assert!(
        frames.ends_with(&debug(&[
            Frame::Data(b"typed-before".to_vec()),
            Frame::Detach
        ])),
        "{frames:?}"
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
        frames[0].starts_with("Resume {")
            && frames[0].contains("offset: 3 ")
            && frames[0].ends_with("version: V2 }"),
        "the resume, in version 2, asks for the offset past \"one\": {frames:?}"
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

/// A controller whose link drops, and whose reconnect's first dial then
/// ends with exit 1 before the remote says hello, with no exit trailer: the
/// remote, after `one`, waits for a typed `z`, then drops the link, and the
/// next ssh dies at once. Returns the client and the file whose removal
/// shows that dial ran.
fn reconnect_dial_exits_1(root: &TempDir, dir: &TempDir) -> (Client, Remote, PathBuf) {
    let dial_dies = dir.path().join("dial-dies");
    let remote = Remote::after(
        &format!("die-if {} 1", dial_dies.display()),
        "hello answer\n\
         hello await Repaint\n\
         hello output one\n\
         hello await-typed 7a\n\
         hello drop\n\
         resume answer\n\
         resume output two\n\
         resume drain",
    );
    let mut client = remote.attach(root);
    client.wait_screen(&["one"]);
    // Armed only now, so it is the reconnect's dial that dies.
    std::fs::write(&dial_dies, b"").unwrap();
    client.type_bytes(b"z");
    (client, remote, dial_dies)
}

/// On a Windows client, ssh.exe's exit 1 without the remote's trailer on a
/// reconnect's dial is ssh.exe's own end (`TerminateProcess` gives 1), as
/// at a connection's end: a lost link, so the attempt fails and the next
/// one resumes.
#[cfg(windows)]
#[test]
fn a_reconnect_dial_that_exits_1_is_tried_again_on_windows() {
    let (root, dir) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let (mut client, _remote, dial_dies) = reconnect_dial_exits_1(&root, &dir);
    client.wait_screen(&[
        "one",
        "tendr: connection lost, attempt 1 failed, next in 1 s",
        "two",
    ]);
    assert!(!dial_dies.exists(), "the reconnect's dial never ran");
    client.type_bytes(DETACH);
    assert_eq!(client.wait_exit(), 0, "{}", client.stderr());
}

/// On a Unix client, ssh's own failures are 255 and a killed ssh has no
/// code, so a reconnect's dial that exits 1 without a trailer gives the
/// remote side's code, as the first dial does: it ends the attach with 1.
#[cfg(not(windows))]
#[test]
fn a_reconnect_dial_that_exits_1_ends_the_attach_on_unix() {
    let (root, dir) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let (mut client, _remote, dial_dies) = reconnect_dial_exits_1(&root, &dir);
    assert_eq!(client.wait_exit(), 1, "{}", client.stderr());
    assert!(!dial_dies.exists(), "the reconnect never dialled");
    let screen = client.screen();
    assert!(
        screen.contains("reconnecting (attempt 1)") && !screen.contains("two"),
        "{screen:?}"
    );
}

/// A remote that sends the last of its output and ends at once can end the
/// connection while the terminal is still taking that output. Here the
/// screen is held, the remote closes its input and exits 3 behind it, and
/// the client's next write, a keystroke, fails. The attach still shows
/// every byte the remote sent before it exits with the remote's code.
#[test]
fn a_controller_shows_all_output_before_the_remote_s_exit() {
    let root = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    let (hold, hung_up) = (dir.path().join("hold"), dir.path().join("hung-up"));
    std::fs::write(&hold, b"").unwrap();
    let remote = Remote::new(&format!(
        "hello answer\n\
         hello await Repaint\n\
         hello output the-head\n\
         hello output the-tail\n\
         hello close-input\n\
         hello touch {}\n\
         hello exit 3",
        hung_up.display()
    ));
    let mut client = remote.attach_held(&root, &hold);
    poll_until(WAIT, Duration::from_millis(20), || {
        if hung_up.exists() {
            Observation::Ready(())
        } else {
            Observation::Pending("the remote has not closed its input".into())
        }
    })
    .unwrap();
    client.type_bytes(b"k");
    // Time for the failed write to end the attach early, as it would without
    // the drain: the relay looks for a connection's end every 100 ms.
    std::thread::sleep(Duration::from_millis(500));
    std::fs::remove_file(&hold).unwrap();
    assert_eq!(client.wait_exit(), 3, "{}", client.stderr());
    assert!(
        client.screen().ends_with("the-headthe-tail"),
        "{:?}",
        client.screen()
    );
}

/// The read-only note, drawn on the test terminal's last row.
const READ_ONLY_ROW: &str =
    "\x1b7\x1b[24;1H\x1b[0m\x1b[2Ktendr: read-only attach; typed input is dropped\x1b8";
/// The test terminal's last row, erased.
const LAST_ROW_ERASED: &str = "\x1b7\x1b[24;1H\x1b[0m\x1b[2K\x1b8";

/// A read-only attach drops what is typed and says so once on the screen,
/// through the terminal layer like every write, and sends the session
/// nothing but its detach: no size, no repaint, no keystrokes. The note
/// stays while attached, and leaving erases it with the rest of the status.
#[test]
fn a_read_only_attach_says_typing_is_dropped() {
    let root = TempDir::new().unwrap();
    let remote = Remote::new(
        "watch log {log}\n\
         watch answer\n\
         watch output watched\n\
         watch drain",
    );
    let mut client = remote.watch(&root, true);
    client.wait_screen(&["watched"]);
    client.type_bytes(b"x");
    client.wait_screen(&[READ_ONLY_ROW]);
    client.type_bytes(b"y");
    client.type_bytes(DETACH);
    assert_eq!(client.wait_exit(), 0, "{}", client.stderr());
    let screen = client.screen();
    assert_eq!(
        screen.matches("typed input is dropped").count(),
        1,
        "said once: {screen:?}"
    );
    assert!(
        screen.ends_with(&format!("{READ_ONLY_ROW}{LAST_ROW_ERASED}")),
        "the note is the last thing drawn, and leaving erases it: {screen:?}"
    );
    assert_eq!(
        remote.frames(),
        debug(&[
            Frame::Hello {
                version: Version::V1,
                mode: Mode::Watch,
            },
            Frame::Detach,
        ])
    );
}

/// A reconnect's status takes the read-only note's row while it runs, and
/// once the watcher has resumed the note is drawn again, since it is still
/// attached and still drops typing. Leaving erases it.
#[test]
fn a_watcher_s_read_only_note_returns_after_a_reconnect() {
    let root = TempDir::new().unwrap();
    let remote = Remote::after(
        "give-up-if {drop}",
        "watch answer\n\
         watch output one\n\
         watch drain\n\
         watch-resume answer\n\
         watch-resume output two\n\
         watch-resume drain",
    );
    let mut client = remote.watch(&root, true);
    client.wait_screen(&["one"]);
    client.type_bytes(b"x");
    client.wait_screen(&[READ_ONLY_ROW]);
    remote.drop_link();
    client.wait_screen(&[
        READ_ONLY_ROW,
        "tendr: connection lost, reconnecting (attempt 1)",
        READ_ONLY_ROW,
        "two",
    ]);
    client.type_bytes(DETACH);
    assert_eq!(client.wait_exit(), 0, "{}", client.stderr());
    let screen = client.screen();
    assert!(
        screen.ends_with(&format!("two{LAST_ROW_ERASED}")),
        "{screen:?}"
    );
}

/// A watcher whose link is lost resumes with its own frame, the watch
/// resume, which carries no epoch, from just past the last byte shown; the
/// output goes on from there, and the new connection, like the first, is
/// sent no size and no repaint.
#[test]
fn a_scripted_watcher_resumes_with_a_watch_resume_and_never_sizes() {
    let root = TempDir::new().unwrap();
    let remote = Remote::after(
        "give-up-if {drop}",
        "watch answer\n\
         watch output one\n\
         watch drain\n\
         watch-resume log {log}\n\
         watch-resume answer\n\
         watch-resume output two\n\
         watch-resume drain",
    );
    let mut client = remote.watch(&root, true);
    client.wait_screen(&["one"]);
    remote.drop_link();
    client.wait_screen(&["one", "two"]);
    client.type_bytes(DETACH);
    assert_eq!(client.wait_exit(), 0, "{}", client.stderr());

    let frames = remote.frames();
    assert_eq!(frames[1..], debug(&[Frame::Detach]), "{frames:?}");
    assert!(
        frames[0].starts_with("WatchResume {")
            && frames[0].contains("offset: 3 ")
            && frames[0].ends_with("version: V2 }"),
        "the watch resume, in version 2, asks for the offset past \"one\": {frames:?}"
    );
}

/// A watcher's resume the session can no longer serve resets the modes the
/// screen had, says how much output was missed, and goes on from the head,
/// still sending no size and no repaint.
#[test]
fn a_scripted_watcher_resume_that_is_gone_says_what_was_missed() {
    let root = TempDir::new().unwrap();
    let remote = Remote::after(
        "give-up-if {drop}",
        "watch answer\n\
         watch output-hex 1b5b3f32303034686f6e65\n\
         watch drain\n\
         watch-resume log {log}\n\
         watch-resume gone 5\n\
         watch-resume output two\n\
         watch-resume drain",
    );
    let mut client = remote.watch(&root, true);
    client.wait_screen(&["one"]);
    remote.drop_link();
    client.wait_screen(&[
        "one",
        "\x1b[?2004l",
        "reconnected; 5 bytes of output were written while disconnected, \
         and the session no longer held 1 of them",
        "two",
    ]);
    client.type_bytes(DETACH);
    assert_eq!(client.wait_exit(), 0, "{}", client.stderr());
    assert_eq!(remote.frames()[1..], debug(&[Frame::Detach]));
}

/// Without a terminal, a watcher's stdout gets the session's output and
/// nothing else across a lost link, a gone resume included: no mode resets,
/// no status line, no notice. Those go to stderr. Its exit code is the
/// remote's verdict on how the session ended it: 0 when the run ended, 84
/// when the session cut it off. Only Unix has a no-terminal layer.
#[cfg(unix)]
#[test]
fn a_scripted_watcher_without_a_terminal_keeps_stdout_to_the_output() {
    for verdict in [0, 84] {
        let root = TempDir::new().unwrap();
        let remote = Remote::after(
            "give-up-if {drop}",
            &format!(
                "watch answer\n\
                 watch output-hex 1b5b3f32303034686f6e65\n\
                 watch drain\n\
                 watch-resume gone 5\n\
                 watch-resume output two\n\
                 watch-resume exit {verdict}"
            ),
        );
        let mut client = remote.watch(&root, false);
        poll_until(WAIT, Duration::from_millis(20), || {
            if client.screen().ends_with("one") {
                Observation::Ready(())
            } else {
                Observation::Pending(client.screen())
            }
        })
        .unwrap();
        remote.drop_link();
        assert_eq!(client.wait_exit(), verdict, "{}", client.stderr());
        assert_eq!(client.screen(), "\x1b[?2004honetwo");
        let stderr = client.stderr();
        assert!(
            stderr.contains("tendr: connection lost, reconnecting")
                && stderr.contains("the session no longer held 1 of them"),
            "{stderr}"
        );
    }
}

/// The end of output ends a watcher's attach where the session closing it
/// would, though the remote holds the connection open: the client stops
/// reading, closes its side, and exits with the remote's verdict, never
/// reconnecting. An end at another offset than the output reached is
/// outside the protocol (80).
#[test]
fn a_scripted_end_of_output_ends_the_watch_at_its_offset() {
    for (end, code, said) in [("end", 0, ""), ("end 1", 80, "ended at offset 4")] {
        let root = TempDir::new().unwrap();
        let remote = Remote::new(&format!(
            "watch answer\n\
             watch output one\n\
             watch {end}\n\
             watch drain"
        ));
        let mut client = remote.watch(&root, true);
        assert_eq!(client.wait_exit(), code, "{end}: {}", client.stderr());
        assert!(
            client.screen().contains("one"),
            "{end}: {}",
            client.screen()
        );
        let stderr = client.stderr();
        assert!(stderr.contains(said), "{end}: {stderr}");
        assert!(!stderr.contains("reconnecting"), "{end}: {stderr}");
    }
}

/// A remote tendr from before 0.7.0 ignores `read_only` and refuses a
/// watcher before any answer, in its own frozen words (those of 0.5.0 and
/// 0.6.0): 80 for the watch hello it does not know, or 81 from its
/// human-control check, whose `--takeover` hint would take the terminal
/// from whoever holds it. The client keeps the remote's code and message,
/// and adds a line after it saying what the refusal means. A 0.7.0 remote's
/// own refusal, and a plain attach's 81, get no such line.
#[test]
fn a_watcher_refused_by_an_old_remote_says_it_needs_0_7_0() {
    const OLD_HELLO: &str = "the attach request must be followed by its Attach hello, \
                             not Err(Malformed(UnsupportedHello))";
    const OLD_CONTROL: &str =
        "session is already under human control (use --takeover to take it over)";
    const NEW_SESSION: &str = "cannot attach read-only: the session was started by a tendr \
                               that cannot serve read-only viewers (unsupported attach \
                               protocol version or mode)";
    const NEEDS: &str = "tendr: watching (attach --read-only) needs tendr 0.7.0 or newer on \
                         box, and a session started by it\n";
    const OLDER: &str = "tendr: the tendr on box is older than 0.7.0 and cannot serve \
                         watchers; --takeover would take the terminal from whoever holds it\n";
    let terminals: &[bool] = if cfg!(unix) { &[true, false] } else { &[true] };
    for &terminal in terminals {
        for (code, said, note) in [
            (80, OLD_HELLO, Some(NEEDS)),
            (81, OLD_CONTROL, Some(OLDER)),
            (80, NEW_SESSION, None),
        ] {
            let root = TempDir::new().unwrap();
            let remote = Remote::new(&format!("watch say {said}\nwatch exit {code}"));
            let mut client = remote.watch(&root, terminal);
            let case = format!("{code} {said:?}, terminal {terminal}");
            assert_eq!(client.wait_exit(), code, "{case}: {}", client.stderr());
            assert_eq!(client.screen(), "", "{case}");
            let stderr = client.stderr();
            let expected = format!("{said}\n{}", note.unwrap_or_default());
            assert_eq!(stderr, expected, "{case}");
        }
    }
    let root = TempDir::new().unwrap();
    let remote = Remote::new(&format!("hello say {OLD_CONTROL}\nhello exit 81"));
    let mut client = remote.attach(&root);
    assert_eq!(client.wait_exit(), 81, "{}", client.stderr());
    assert_eq!(client.stderr(), format!("{OLD_CONTROL}\n"));
}
