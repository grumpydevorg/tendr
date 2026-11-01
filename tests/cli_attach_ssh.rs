//! `tendr attach` through a nested `ssh -t`, as a person attaches from a
//! laptop: a local terminal runs `ssh -t <host> tendr attach <session>`.
//!
//! The tests need an sshd and are opt-in. `TENDR_TEST_SSH_HOST` names the ssh
//! destination; it must accept `BatchMode` logins, for example with a key in
//! the agent. Without it each test prints one line saying it was skipped.
//! `TENDR_TEST_SSH_TENDR` is the path of tendr on that host. It defaults to
//! this build, which is right for `localhost`:
//!
//! ```text
//! TENDR_TEST_SSH_HOST=localhost cargo test --test cli_attach_ssh -- --test-threads=1
//! ```
//!
//! Each test keeps its remote state in a fresh `mktemp -d` directory, used as
//! the remote tendr's HOME, and removes it when it ends. Here `ssh`, not
//! tendr, restores the local terminal; on macOS it leaves PENDIN set, which
//! the terminal checks ignore.
#![cfg(unix)]

mod harness;

use harness::terminal::{WrappedAttach, tendr_bin};
use harness::{CMD_DEADLINE, Observation, poll_until};
use std::process::Output;
use std::sync::Mutex;
use std::time::Duration;
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

/// What the session's child turns on, replayed to every client that attaches.
const MODES_ON: &str = "\x1b[?1049h\x1b[?1003h";
/// What a client writes to its terminal to turn those modes off when it leaves.
const MODES_OFF: &str = "\x1b[?1049l\x1b[?1003l";
/// A child that turns the modes on, says `set`, then echoes its input visibly.
const MODES_CAT: &str = r#"stty raw -echo; printf '\033[?1049h\033[?1003h'; echo set; exec cat -v"#;

/// Options for every ssh: no prompts, and no shared connection, so each ssh
/// is its own connection and stopping one stops only that one.
const SSH_OPTIONS: [&str; 6] = [
    "-o",
    "BatchMode=yes",
    "-o",
    "ControlMaster=no",
    "-o",
    "ControlPath=none",
];

/// Run `command` on `host` through the remote login shell, without a terminal.
fn ssh_batch(host: &str, command: &str) -> Output {
    assert_cmd::Command::new("ssh")
        .arg("-T")
        .args(SSH_OPTIONS)
        .args([host, command])
        .timeout(CMD_DEADLINE)
        .output()
        .unwrap()
}

/// The ssh host and a private remote HOME for one test. Dropping it kills the
/// sessions the test started there and removes the directory.
struct Remote {
    host: String,
    tendr: String,
    home: String,
    sessions: Vec<String>,
}

impl Remote {
    /// `None`, after printing a skip note, when `TENDR_TEST_SSH_HOST` is unset.
    fn connect(test: &str) -> Option<Self> {
        let nonempty = |name| std::env::var(name).ok().filter(|v: &String| !v.is_empty());
        let Some(host) = nonempty("TENDR_TEST_SSH_HOST") else {
            eprintln!("skipped {test}: TENDR_TEST_SSH_HOST is not set");
            return None;
        };
        let tendr = nonempty("TENDR_TEST_SSH_TENDR").unwrap_or_else(tendr_bin);
        let output = ssh_batch(&host, "mktemp -d");
        assert!(
            output.status.success(),
            "ssh {host} mktemp -d failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let home = String::from_utf8(output.stdout).unwrap().trim().to_owned();
        assert!(
            home.starts_with('/') && home.len() > 1 && !home.contains('\n'),
            "mktemp -d on {host} printed {home:?}"
        );
        Some(Self {
            host,
            tendr,
            home,
            sessions: Vec::new(),
        })
    }

    /// The remote command line that runs tendr with the private HOME, quoted
    /// for the remote login shell.
    fn tendr_command(&self, args: &[&str]) -> String {
        let home = format!("HOME={}", self.home);
        let argv = ["env", &home, &self.tendr]
            .into_iter()
            .chain(args.iter().copied());
        shell_words::join(argv)
    }

    fn tendr(&self, args: &[&str]) -> Output {
        ssh_batch(&self.host, &self.tendr_command(args))
    }

    /// Start a PTY session running [`MODES_CAT`] and wait until it has set its
    /// modes.
    fn start_modes_cat(&mut self, session: &str) {
        self.sessions.push(session.to_owned());
        let start = ["start", session, "--pty", "--stdin", "--", "sh", "-c"];
        let output = self.tendr(&[&start[..], &[MODES_CAT]].concat());
        assert!(
            output.status.success(),
            "remote start {session} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        self.wait_log_contains(session, "set");
    }

    /// The argv that attaches to `session` through `ssh -t`.
    fn attach_argv(&self, session: &str, extra: &[&str]) -> Vec<String> {
        let attach = [&["attach", session][..], extra].concat();
        ["ssh", "-t"]
            .into_iter()
            .chain(SSH_OPTIONS)
            .map(str::to_owned)
            .chain([self.host.clone(), self.tendr_command(&attach)])
            .collect()
    }

    fn wait_log_contains(&self, session: &str, needle: &str) -> String {
        poll_until(Duration::from_secs(15), Duration::from_millis(100), || {
            let output = self.tendr(&["log", session, "--raw"]);
            let log = String::from_utf8_lossy(&output.stdout).into_owned();
            if log.contains(needle) {
                Observation::Ready(log)
            } else {
                Observation::Pending(log)
            }
        })
        .unwrap_or_else(|e| panic!("remote log of {session} never contained {needle:?}: {e}"))
    }

    /// Wait until the remote `status` shows `pty.control == expected`.
    fn wait_control(&self, session: &str, expected: &str) {
        poll_until(Duration::from_secs(15), Duration::from_millis(100), || {
            let output = self.tendr(&["status", session]);
            let control = serde_json::from_slice::<serde_json::Value>(&output.stdout)
                .map(|meta| meta["pty"]["control"].clone());
            match control {
                Ok(c) if c.as_str() == Some(expected) => Observation::Ready(()),
                Ok(c) => Observation::Pending(format!("pty.control={c}")),
                Err(_) => Observation::Pending(String::from_utf8_lossy(&output.stderr).into()),
            }
        })
        .unwrap_or_else(|e| panic!("remote {session} never reached pty.control={expected}: {e}"))
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        let mut steps: Vec<String> = self
            .sessions
            .iter()
            .map(|s| self.tendr_command(&["kill", "--force", s]))
            .collect();
        steps.push(shell_words::join(["rm", "-rf", "--", &self.home]));
        // Ignored: a session may already be gone, and a panic while unwinding
        // would abort the test binary.
        let _ = ssh_batch(&self.host, &steps.join("; "));
    }
}

/// What a terminal showed after the first replay of the session's modes.
fn after_modes_on(output: &str) -> &str {
    output
        .split_once(MODES_ON)
        .map(|(_, after)| after)
        .unwrap_or_else(|| panic!("the terminal never got the modes; it saw: {output:?}"))
}

/// Drop ssh's own `Connection to <host> closed.` line from the end of what a
/// terminal showed.
fn without_ssh_note(output: &str) -> &str {
    output
        .rfind("Connection to ")
        .map_or(output, |i| &output[..i])
        .trim_end()
}

/// Attach through `ssh -t` and wait until the remote client is relaying: it
/// replays the modes only after it has put its terminal in raw mode.
fn attach(root: &TempDir, remote: &Remote, label: &str, session: &str) -> WrappedAttach {
    let wrapped = WrappedAttach::spawn_argv(root, label, &remote.attach_argv(session, &[]));
    wrapped.wait_output_contains(MODES_ON);
    remote.wait_control(session, "HumanControl");
    wrapped
}

#[test]
fn ssh_detach_restores_the_terminal_and_turns_the_modes_off() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut remote) =
        Remote::connect("ssh_detach_restores_the_terminal_and_turns_the_modes_off")
    else {
        return;
    };
    let root = TempDir::new().unwrap();
    remote.start_modes_cat("ssh-detach");

    let mut wrapped = attach(&root, &remote, "detach", "ssh-detach");
    wrapped.cli.type_bytes(b"\x1cd");

    assert_eq!(
        wrapped.wait_exit(Duration::from_secs(15)),
        0,
        "{:?}",
        wrapped.cli.output()
    );
    remote.wait_control("ssh-detach", "AgentControl");
    wrapped.assert_terminal_restored_but_pendin();
    let output = wrapped.cli.output();
    assert!(
        without_ssh_note(after_modes_on(&output)).ends_with(MODES_OFF),
        "leaving turns off the modes the replay turned on; the terminal saw: {output:?}"
    );
}

#[test]
fn ssh_ctrl_right_bracket_passes_through() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut remote) = Remote::connect("ssh_ctrl_right_bracket_passes_through") else {
        return;
    };
    let root = TempDir::new().unwrap();
    remote.start_modes_cat("ssh-ctrl-bracket");

    let mut wrapped = attach(&root, &remote, "ctrl-bracket", "ssh-ctrl-bracket");
    wrapped.cli.type_bytes(b"\x1d");
    wrapped.wait_output_contains("^]");
    remote.wait_log_contains("ssh-ctrl-bracket", "^]");
    assert!(!wrapped.exited(), "Ctrl-] must not end the attach");

    wrapped.cli.type_bytes(b"\x1cd");
    assert_eq!(wrapped.wait_exit(Duration::from_secs(15)), 0);
    wrapped.assert_terminal_restored_but_pendin();
}

#[test]
fn ssh_paste_with_prefixes_reaches_the_child() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut remote) = Remote::connect("ssh_paste_with_prefixes_reaches_the_child") else {
        return;
    };
    let root = TempDir::new().unwrap();
    remote.start_modes_cat("ssh-paste");
    let mut wrapped = attach(&root, &remote, "paste", "ssh-paste");

    // A prefix followed by another key forwards both.
    wrapped.cli.type_bytes(b"\x1b[200~a\x1cb\x1b[201~");
    wrapped.wait_output_contains("^[[200~a^\\b^[[201~");
    // A doubled prefix forwards one.
    wrapped.cli.type_bytes(b"\x1b[200~c\x1c\x1ce\x1b[201~");
    wrapped.wait_output_contains("^[[200~c^\\e^[[201~");
    assert!(!wrapped.exited(), "neither paste detaches");
    remote.wait_control("ssh-paste", "HumanControl");

    // A paste holding the detach sequence. Recorded, not asserted: the escape
    // parser does not know about bracketed paste.
    wrapped.cli.type_bytes(b"\x1b[200~f\x1cdg\x1b[201~");
    let outcome = poll_until(Duration::from_secs(10), Duration::from_millis(20), || {
        if wrapped.exited() {
            Observation::Ready("detached")
        } else if wrapped.cli.output().contains("f^\\dg") {
            Observation::Ready("forwarded the paste intact")
        } else {
            Observation::Pending("neither detached nor forwarded".to_owned())
        }
    })
    .unwrap_or_else(|e| panic!("a paste holding Ctrl-\\ d did nothing visible: {e}"));

    if outcome == "detached" {
        assert_eq!(wrapped.wait_exit(Duration::from_secs(15)), 0);
        remote.wait_control("ssh-paste", "AgentControl");
    } else {
        wrapped.cli.type_bytes(b"\x1cd");
        assert_eq!(wrapped.wait_exit(Duration::from_secs(15)), 0);
    }
    wrapped.assert_terminal_restored_but_pendin();
    let log =
        String::from_utf8_lossy(&remote.tendr(&["log", "ssh-paste", "--raw"]).stdout).into_owned();
    let third = log.rsplit("e^[[201~").next().unwrap_or_default();
    eprintln!(
        "finding: a bracketed paste holding Ctrl-\\ d {outcome}; after the second paste the child got {third:?}"
    );
}

#[test]
fn ssh_takeover_over_a_stopped_first_client() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut remote) = Remote::connect("ssh_takeover_over_a_stopped_first_client") else {
        return;
    };
    let root = TempDir::new().unwrap();
    remote.start_modes_cat("ssh-takeover");

    let first = attach(&root, &remote, "take-1", "ssh-takeover");
    // The wrapped command is the local ssh: stopping it stalls the whole
    // connection, as a suspended laptop would.
    first.signal(libc::SIGSTOP);

    let takeover = remote.attach_argv("ssh-takeover", &["--takeover"]);
    let mut second = WrappedAttach::spawn_argv(&root, "take-2", &takeover);
    second.wait_output_contains(MODES_ON);
    remote.wait_control("ssh-takeover", "HumanControl");
    second.cli.type_bytes(b"second");
    second.wait_output_contains("second");
    remote.wait_log_contains("ssh-takeover", "second");

    first.signal(libc::SIGCONT);
    assert_eq!(
        first.wait_exit(Duration::from_secs(15)),
        81,
        "{:?}",
        first.cli.output()
    );
    first.assert_terminal_restored_but_pendin();
    let old = first.cli.output();
    let (old_terminal, old_note) = after_modes_on(&old)
        .split_once("tendr: ")
        .unwrap_or_else(|| panic!("the old client printed no note; it saw: {old:?}"));
    assert!(
        old_terminal.ends_with(MODES_OFF),
        "the old client turned the modes off before its note; it saw: {old:?}"
    );
    assert!(
        old_note.starts_with("another client took over this session"),
        "{old:?}"
    );

    second.cli.type_bytes(b"\x1cd");
    assert_eq!(second.wait_exit(Duration::from_secs(15)), 0);
    second.assert_terminal_restored_but_pendin();
}
