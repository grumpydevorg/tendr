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
use harness::{Observation, fake_ssh, poll_until, shim_path_env};
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
        let dir = TempDir::new().unwrap();
        let frames = dir.path().join("frames");
        let ssh = fake_ssh(&format!(
            "script\n{}",
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

/// CTRL_BREAK_EVENT raised in the console (it reaches ssh too, as it would a
/// real ssh) ends the attach with exit 1, and the console is put back.
#[test]
fn ctrl_break_leaves_and_puts_the_console_back() {
    let trigger = TempDir::new().unwrap();
    let path = trigger.path().join("break");
    let attach = Attach::start(
        "hello answer\n\
         hello output ready-for-break\n\
         hello drain",
        &["--break-on", path.to_str().unwrap()],
    );
    attach.wait_text("ready-for-break");
    std::fs::write(&path, b"").unwrap();
    assert_eq!(attach.finish(), 1);
}
