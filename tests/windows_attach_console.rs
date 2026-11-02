//! The Windows console layer of `attach --host` under a pseudo console: the
//! client runs in a ConPTY (tests/harness/conpty.rs), as it would in Windows
//! Terminal, against the fake ssh's scripted remote (`play` in
//! `src/bin/fake_ssh.rs`). The frames the client sent come from the script's
//! log; what it drew is conhost's rendering, so only its text is asserted.
//!
//! `conmode --run` runs the client and reports the console's modes and
//! output code page before and after it, so every test also checks that the
//! console is put back.
#![cfg(windows)]

mod harness;

use std::path::PathBuf;
use std::time::Duration;

use harness::conpty::{ConPty, ConmodeReport, conmode_bin};
use harness::{Observation, fake_ssh, poll_until, process_alive, shim_path_env};
use tempfile::TempDir;
use tendr::attach_proto::{Frame, Mode, Version};
use tendr::recording::Geometry;

/// The pseudo console's size.
const COLS: i16 = 80;
const ROWS: i16 = 25;
/// The default detach escape: `Ctrl-\` then `d`.
const DETACH: &[u8] = b"\x1cd";
/// Hang detector for one observation. Each normally takes milliseconds.
const WAIT: Duration = Duration::from_secs(20);

/// `tendr --host box attach s` in a pseudo console against a scripted
/// remote, run by `conmode --run`.
struct Attach {
    pty: ConPty,
    frames: PathBuf,
    _dir: TempDir,
    _ssh: TempDir,
}

impl Attach {
    /// Start it: `script` with `{log}` standing for the frames log, and
    /// `conmode` options before the `--`.
    fn start(script: &str, conmode: &[&str]) -> Self {
        Self::start_after("", script, conmode)
    }

    /// Start it as [`Self::start`] does, with the fake ssh's `actions` (one
    /// a line) run by every ssh before it plays the script.
    fn start_after(actions: &str, script: &str, conmode: &[&str]) -> Self {
        let dir = TempDir::new().unwrap();
        let frames = dir.path().join("frames");
        let ssh = fake_ssh(&format!(
            "{actions}\nscript\n{}",
            script.replace("{log}", frames.to_str().unwrap())
        ));
        let path = format!("PATH={}", shim_path_env(&ssh).to_str().unwrap());
        let conmode_bin = conmode_bin();
        let mut argv = vec![conmode_bin.as_str(), "--run", "--env", &path];
        argv.extend(conmode);
        argv.extend([
            "--",
            env!("CARGO_BIN_EXE_tendr"),
            "--host",
            "box",
            "attach",
            "s",
        ]);
        let pty = ConPty::spawn(&argv, COLS, ROWS);
        let attach = Self {
            pty,
            frames,
            _dir: dir,
            _ssh: ssh,
        };
        attach.report("before");
        attach
    }

    fn report(&self, tag: &str) -> ConmodeReport {
        self.pty
            .wait_for(&format!("conmode report tagged {tag:?}"), WAIT, |text| {
                ConmodeReport::find(text, tag)
            })
    }

    /// Wait until the client has drawn `needle`.
    fn wait_text(&self, needle: &str) {
        self.pty
            .wait_for(needle, WAIT, |text| text.contains(needle).then_some(()));
    }

    /// The frames the client sent so far, `{:?}` one per line.
    fn frames(&self) -> Vec<String> {
        std::fs::read_to_string(&self.frames)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Wait until the frames sent satisfy `done`.
    fn wait_frames(&self, what: &str, done: impl Fn(&[String]) -> bool) {
        poll_until(WAIT, Duration::from_millis(20), || {
            let frames = self.frames();
            if done(&frames) {
                Observation::Ready(())
            } else {
                Observation::Pending(format!("frames: {frames:?}"))
            }
        })
        .unwrap_or_else(|e| panic!("never sent {what}: {e}\ndrawn: {:?}", self.pty.text()));
    }

    /// Wait for the client to exit, check that the console is as it was
    /// before, and return its exit code.
    fn finish(self) -> u32 {
        let code = self
            .pty
            .wait_exit(WAIT)
            .unwrap_or_else(|| panic!("the client never exited\ndrawn: {:?}", self.pty.text()));
        let (before, after) = (self.report("before"), self.report("after"));
        assert_eq!(
            (&before.conin, &before.conout, &before.cp),
            (&after.conin, &after.conout, &after.cp),
            "the console's modes and code page are put back\ndrawn: {:?}",
            self.pty.text()
        );
        self.pty.close();
        code
    }
}

/// The bytes the client sent as keystrokes, in order.
fn typed(frames: &[String]) -> Vec<u8> {
    frames
        .iter()
        .filter_map(|f| f.strip_prefix("Data([")?.strip_suffix("])"))
        .flat_map(|list| list.split(", ").filter(|b| !b.is_empty()))
        .map(|b| b.parse::<u8>().expect("a byte"))
        .collect()
}

fn resize(rows: u16, cols: u16) -> String {
    format!("{:?}", Frame::Resize(Geometry::new(rows, cols).unwrap()))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Typing (Ctrl-C included, as the byte for the session), output and a
/// resize cross the console, the escape detaches with exit 0, and the
/// console is put back.
#[test]
fn a_console_attach_types_shows_resizes_and_detaches() {
    let mut attach = Attach::start(
        "hello log {log}\n\
         hello answer\n\
         hello output scripted-output\n\
         hello drain",
        &[],
    );
    attach.wait_text("scripted-output");

    attach.pty.write(b"ab\x03c");
    attach.wait_frames("ab, Ctrl-C and c", |frames| {
        contains(&typed(frames), b"ab\x03c")
    });

    // conhost applies a resize asynchronously; the client's poll finds it.
    attach.pty.resize(100, 30);
    let wider = resize(30, 100);
    attach.wait_frames(&wider, |frames| frames.contains(&wider));

    attach.pty.write(DETACH);
    attach.wait_frames("the detach", |frames| {
        frames.last().map(String::as_str) == Some("Detach")
    });
    let frames = attach.frames();
    assert_eq!(attach.finish(), 0);

    assert_eq!(
        frames[..3],
        [
            format!(
                "{:?}",
                Frame::Hello {
                    version: Version::V1,
                    mode: Mode::Attach,
                }
            ),
            resize(ROWS as u16, COLS as u16),
            format!("{:?}", Frame::Repaint),
        ],
        "the hello, then the console's size and the repaint nudge"
    );
    assert!(
        !typed(&frames).contains(&0x1c),
        "the escape is not sent: {frames:?}"
    );
}

/// Told another client took over, the client exits 81 and puts the console
/// back.
#[test]
fn a_console_attach_taken_over_exits_81() {
    let attach = Attach::start(
        "hello answer\n\
         hello await Repaint\n\
         hello output before-takeover\n\
         hello retire\n\
         hello exit 81",
        &[],
    );
    attach.wait_text("before-takeover");
    assert_eq!(attach.finish(), 81);
}

/// Output that is not UTF-8 is written as it is: the attach goes on.
#[test]
fn output_that_is_not_utf8_does_not_end_the_attach() {
    let mut attach = Attach::start(
        "hello log {log}\n\
         hello answer\n\
         hello output before-invalid\n\
         hello output-hex fffec328\n\
         hello output after-invalid\n\
         hello drain",
        &[],
    );
    attach.wait_text("after-invalid");
    attach.pty.write(b"x");
    attach.wait_frames("typing after the invalid output", |frames| {
        typed(frames).contains(&b'x')
    });
    attach.pty.write(DETACH);
    assert_eq!(attach.finish(), 0);
}

/// CTRL_BREAK_EVENT raised in the console ends the attach with exit 1, and
/// the console is put back. It reaches ssh too, as it would a real ssh, and
/// ends it with `STATUS_CONTROL_C_EXIT`: a lost link, whose reconnect the
/// remote here answers with an end (exit 0). The leave wins over that end,
/// whichever the client sees first.
#[test]
fn ctrl_break_leaves_and_puts_the_console_back() {
    let trigger = TempDir::new().unwrap();
    let path = trigger.path().join("break");
    let attach = Attach::start(
        "hello answer\n\
         hello output ready-for-break\n\
         hello drain\n\
         resume exit 0",
        &["--break-on", path.to_str().unwrap()],
    );
    attach.wait_text("ready-for-break");
    std::fs::write(&path, b"").unwrap();
    assert_eq!(attach.finish(), 1);
}

/// Whether the first frame logged is a resume from `offset`: just past the
/// output shown before the link dropped, in version 2's frame, the one the
/// client always sends.
fn resumed_from(frames: &[String], offset: usize) -> bool {
    frames.first().is_some_and(|first| {
        first.starts_with("Resume {")
            && first.contains(&format!("offset: {offset} "))
            && first.ends_with("version: V2 }")
    })
}

/// While a lost link reconnects, the console's last rows say so: the
/// attempt, ssh's latest line, and after a failed attempt the pause before
/// the next, growing from 1 s to 2 s; and, once something is typed, that
/// typing is discarded. What is typed in the gap never reaches the session,
/// and the attach resumes once a login gets through.
#[test]
fn a_reconnect_shows_its_status_and_backoff_and_discards_typing() {
    let dir = TempDir::new().unwrap();
    let prompt = dir.path().join("prompt");
    let mut attach = Attach::start_after(
        &format!("needs-prompt {}", prompt.to_str().unwrap()),
        "hello answer\n\
         hello await Repaint\n\
         hello output before-drop\n\
         hello await Data\n\
         hello drop\n\
         resume log {log}\n\
         resume answer\n\
         resume output after-resume\n\
         resume drain",
        &[],
    );
    attach.wait_text("before-drop");
    // From now on a login needs a prompt, which a reconnect never answers:
    // its attempts fail at once, with an approval URL on ssh's stderr.
    std::fs::write(&prompt, b"").unwrap();
    attach.pty.write(b"x");
    attach.wait_text("tendr: connection lost, attempt 1 failed, next in 1 s");
    attach.wait_text("https://approve.invalid/a/1");
    attach.wait_text("tendr: connection lost, attempt 2 failed, next in 2 s");
    attach.pty.write(b"typed-in-the-gap");
    attach.wait_text("; typed input is discarded");
    std::fs::remove_file(&prompt).unwrap();
    attach.wait_text("after-resume");

    attach.pty.write(DETACH);
    attach.wait_frames("the detach", |frames| {
        frames.last().map(String::as_str) == Some("Detach")
    });
    let frames = attach.frames();
    assert_eq!(attach.finish(), 0);
    assert!(
        resumed_from(&frames, "before-drop".len()),
        "the resume asks for the offset past what was shown: {frames:?}"
    );
    assert!(
        !contains(&typed(&frames), b"typed"),
        "typing in the gap is never sent: {frames:?}"
    );
}

/// ssh.exe ended by Windows (an NTSTATUS exit, here `STATUS_CONTROL_C_EXIT`)
/// without the remote's verdict is a lost link: the attach resumes from
/// just past what it showed, greets the new connection as a fresh one, and
/// shows what follows.
#[test]
fn an_ntstatus_drop_resumes_where_the_console_stopped() {
    let mut attach = Attach::start(
        "hello answer\n\
         hello await Repaint\n\
         hello output before-drop\n\
         hello drop 3221225786\n\
         resume log {log}\n\
         resume answer\n\
         resume output after-resume\n\
         resume drain",
        &[],
    );
    attach.wait_text("before-drop");
    attach.wait_text("after-resume");
    attach.pty.write(DETACH);
    attach.wait_frames("the detach", |frames| {
        frames.last().map(String::as_str) == Some("Detach")
    });
    let frames = attach.frames();
    assert_eq!(attach.finish(), 0);
    assert!(
        resumed_from(&frames, "before-drop".len()),
        "the resume asks for the offset past what was shown: {frames:?}"
    );
    assert_eq!(
        frames[1..],
        [
            resize(ROWS as u16, COLS as u16),
            format!("{:?}", Frame::Repaint),
            "Detach".to_owned(),
        ],
        "the new connection is greeted, and nothing else is sent"
    );
}

/// ssh.exe's exit 1 without the remote's trailer is a lost link too. A
/// resume the session can no longer serve says what was missed, turns off
/// the modes the session had set, and shows the session from then on. The
/// mode here is bracketed paste, which the client itself acts on: inside a
/// paste the escape is typing, so it detaches only once the mode is off.
#[test]
fn a_gone_resume_says_what_was_missed_and_resets_the_modes() {
    const PASTE_START: &[u8] = b"\x1b[200~";
    let mut attach = Attach::start(
        "hello log {log}\n\
         hello answer\n\
         hello await Repaint\n\
         hello output-hex 1b5b3f3230303468\n\
         hello output bracketed-paste-on\n\
         hello await-typed 1c64\n\
         hello drop 1\n\
         resume log {log}\n\
         resume gone 5\n\
         resume output after-gone\n\
         resume drain",
        &[],
    );
    attach.wait_text("bracketed-paste-on");
    attach
        .pty
        .write(&[PASTE_START, DETACH, b"\x1b[201~"].concat());
    attach.wait_frames("the pasted escape, as typing", |frames| {
        contains(&typed(frames), DETACH)
    });
    attach.wait_text("tendr: reconnected; 5 bytes of output were written");
    attach.wait_text("after-gone");
    // With the mode off a paste's start is a key like any other, and the
    // escape after it detaches. The detach, a control, may overtake the
    // key.
    attach.pty.write(&[PASTE_START, DETACH].concat());
    attach.wait_frames("the detach", |frames| {
        frames.iter().any(|frame| frame == "Detach")
    });
    assert_eq!(attach.finish(), 0);
}

/// The escape while a reconnect's ssh is still connecting detaches with
/// exit 0 and stops that ssh.
#[test]
fn the_escape_during_a_reconnect_exits_0_and_stops_ssh() {
    let dir = TempDir::new().unwrap();
    let (pidfile, stall) = (dir.path().join("pid"), dir.path().join("stall"));
    let mut attach = Attach::start_after(
        &format!(
            "pidfile {}\nstall-if {}",
            pidfile.to_str().unwrap(),
            stall.to_str().unwrap()
        ),
        "hello answer\n\
         hello await Repaint\n\
         hello output before-drop\n\
         hello await Data\n\
         hello drop",
        &[],
    );
    attach.wait_text("before-drop");
    let pid = || -> Option<u32> { std::fs::read_to_string(&pidfile).ok()?.trim().parse().ok() };
    let first = pid().expect("the first ssh's pid");
    // From now on ssh stalls before the remote tendr starts, as a login
    // waiting on a lapsed approval does.
    std::fs::write(&stall, b"").unwrap();
    attach.pty.write(b"x");
    attach.wait_text("tendr: connection lost, reconnecting (attempt 1)");
    let attempt = poll_until(WAIT, Duration::from_millis(20), || match pid() {
        Some(pid) if pid != first => Observation::Ready(pid),
        other => Observation::Pending(format!("pid file holds {other:?}")),
    })
    .expect("the reconnect's ssh starts");

    attach.pty.write(DETACH);
    assert_eq!(attach.finish(), 0);
    poll_until(WAIT, Duration::from_millis(20), || {
        if process_alive(attempt) {
            Observation::Pending(format!("the reconnect's ssh (pid {attempt}) runs"))
        } else {
            Observation::Ready(())
        }
    })
    .expect("the reconnect's ssh is stopped");
}
