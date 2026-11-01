//! A terminal driver for tests: runs a command in a PTY, types into it and
//! captures what it writes, as a person at a terminal would.

use super::{Observation, poll_path_exists, poll_until, tendr};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tendr::platform::{Current, Platform};

/// Path to the `tendr` binary under test.
#[allow(dead_code)]
pub fn tendr_bin() -> String {
    assert_cmd::cargo::cargo_bin("tendr")
        .to_string_lossy()
        .into_owned()
}

/// Run the real `tendr attach` CLI inside a PTY, as a terminal user would.
#[allow(dead_code)]
pub struct CliAttach {
    child: <Current as Platform>::SupervisedChild,
    input: Option<Box<dyn Write + Send>>,
    output: Arc<Mutex<Vec<u8>>>,
    /// While set, the terminal stops reading the CLI's output, as a stalled
    /// terminal or SSH connection would.
    output_paused: Arc<AtomicBool>,
    resize_fd: Option<std::fs::File>,
}

#[allow(dead_code)]
impl CliAttach {
    pub fn spawn(root: &TempDir, args: &[&str]) -> Self {
        let mut argv = vec![tendr_bin(), "attach".to_owned()];
        argv.extend(args.iter().map(|a| (*a).to_owned()));
        Self::spawn_argv(root, &argv)
    }

    pub fn spawn_argv(root: &TempDir, argv: &[String]) -> Self {
        Self::spawn_argv_env(root, argv, &[])
    }

    pub fn spawn_argv_env(root: &TempDir, argv: &[String], extra_env: &[(&str, &str)]) -> Self {
        let mut env = std::collections::BTreeMap::new();
        env.insert(
            "HOME".to_owned(),
            root.path().to_string_lossy().into_owned(),
        );
        for (key, value) in extra_env {
            env.insert((*key).to_owned(), (*value).to_owned());
        }
        let mut child =
            Current::spawn_child_pty(argv, None, &std::collections::BTreeSet::new(), &env).unwrap();
        // Take the resize handle before stdin takes the master's write half.
        let resize_fd = Current::pty_resize_fd(&child);
        let input = Current::child_stdin(&mut child).unwrap();
        let mut reader = Current::child_stdout(&mut child).unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let output_paused = Arc::new(AtomicBool::new(false));
        let (sink, paused) = (Arc::clone(&output), Arc::clone(&output_paused));
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                if paused.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                match std::io::Read::read(&mut reader, &mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sink.lock().unwrap().extend_from_slice(&chunk[..n]),
                }
            }
        });
        Self {
            child,
            input: Some(input),
            output,
            output_paused,
            resize_fd,
        }
    }

    pub fn type_bytes(&mut self, bytes: &[u8]) {
        self.input
            .as_mut()
            .expect("input still owned")
            .write_all(bytes)
            .unwrap();
    }

    /// Hand the terminal's keyboard to another thread (for writes that block).
    pub fn take_input(&mut self) -> Box<dyn Write + Send> {
        self.input.take().expect("input still owned")
    }

    pub fn pause_output(&self) {
        self.output_paused.store(true, Ordering::SeqCst);
    }

    pub fn resume_output(&self) {
        self.output_paused.store(false, Ordering::SeqCst);
    }

    /// Resize the CLI's terminal, as a user resizing the Ghostty window would.
    pub fn resize(&self, rows: u16, cols: u16) {
        use std::os::unix::io::AsRawFd;
        let fd = self.resize_fd.as_ref().expect("resize handle").as_raw_fd();
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: `fd` is an open PTY master; TIOCSWINSZ takes a winsize pointer.
        let rc = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) };
        assert_eq!(rc, 0, "resize the CLI's terminal");
    }

    /// Type `line` until the session log shows it. Keystrokes typed before the CLI
    /// enters raw mode are discarded (`TCSAFLUSH`), so a single write races the
    /// handshake; the marker is idempotent, so retyping is safe.
    pub fn type_until_logged(&mut self, root: &TempDir, session: &str, line: &str) {
        poll_until(Duration::from_secs(15), Duration::from_millis(200), || {
            self.type_bytes(line.as_bytes());
            let output = tendr(root)
                .args(["log", session, "--raw"])
                .output()
                .unwrap();
            if String::from_utf8_lossy(&output.stdout).contains(line.trim_end()) {
                Observation::Ready(())
            } else {
                Observation::Pending(format!("{session} log lacks {line:?}"))
            }
        })
        .unwrap_or_else(|e| {
            panic!(
                "typed {line:?} never reached {session} ({e}); cli output: {}",
                self.output()
            )
        });
    }

    pub fn output(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }
}

impl Drop for CliAttach {
    fn drop(&mut self) {
        self.resume_output();
        let kill = Current::child_kill_handle(&self.child);
        let _ = Current::kill_child(&kill, true);
        let _ = Current::child_wait(&mut self.child);
    }
}

/// A command run by a shell in the CLI's terminal, recording the terminal
/// settings (`stty -g`) before and after it, its pid and its exit code, so
/// tests can signal it and prove the terminal was restored however it ended.
/// [`spawn`](Self::spawn) wraps `tendr attach`; [`spawn_argv`](Self::spawn_argv)
/// wraps any command, such as `ssh -t <host> tendr attach`.
///
/// The shell runs it as a background job so that its pid is known, with the
/// terminal passed explicitly as its stdin (a background job's stdin is
/// otherwise `/dev/null`). Without job control the job stays in the terminal's
/// foreground process group, so it still reads the keyboard and receives
/// SIGWINCH. It starts with SIGINT and SIGQUIT ignored, as every background
/// job of a non-interactive shell does.
#[allow(dead_code)]
pub struct WrappedAttach {
    pub cli: CliAttach,
    before: PathBuf,
    after: PathBuf,
    pid: PathBuf,
    code: PathBuf,
}

#[allow(dead_code)]
impl WrappedAttach {
    /// Wrap `tendr attach <args>`.
    pub fn spawn(root: &TempDir, label: &str, args: &[&str]) -> Self {
        let argv: Vec<String> = [tendr_bin(), "attach".to_owned()]
            .into_iter()
            .chain(args.iter().map(|a| (*a).to_owned()))
            .collect();
        Self::spawn_argv(root, label, &argv)
    }

    /// Wrap an arbitrary command. `label` names its record files under `root`.
    pub fn spawn_argv(root: &TempDir, label: &str, argv: &[String]) -> Self {
        Self::spawn_argv_env(root, label, argv, &[])
    }

    /// [`spawn_argv`](Self::spawn_argv) with `extra_env` set in the terminal,
    /// such as a `PATH` that finds a fake `ssh` first.
    pub fn spawn_argv_env(
        root: &TempDir,
        label: &str,
        argv: &[String],
        extra_env: &[(&str, &str)],
    ) -> Self {
        let dir = root.path().join(format!("wrap-{label}"));
        std::fs::create_dir_all(&dir).unwrap();
        let q = |p: &Path| shell_words::quote(p.to_str().unwrap()).into_owned();
        let (before, after, pid, code) = (
            dir.join("before"),
            dir.join("after"),
            dir.join("pid"),
            dir.join("code"),
        );
        let script = format!(
            "stty -g > {b}.tmp && mv {b}.tmp {b}; exec 3<&0; \
             {cmd} <&3 3<&- & echo $! > {p}.tmp; mv {p}.tmp {p}; \
             wait $!; echo $? > {c}.tmp; \
             stty -g > {a}.tmp; mv {a}.tmp {a}; mv {c}.tmp {c}; sleep 60",
            b = q(&before),
            a = q(&after),
            p = q(&pid),
            c = q(&code),
            cmd = shell_words::join(argv),
        );
        let cli =
            CliAttach::spawn_argv_env(root, &["sh".to_owned(), "-c".to_owned(), script], extra_env);
        poll_path_exists(&before, Duration::from_secs(10), Duration::from_millis(20))
            .unwrap_or_else(|e| panic!("wrapper never started: {e}"));
        Self {
            cli,
            before,
            after,
            pid,
            code,
        }
    }

    /// Send `signal` to the wrapped command itself, not to the shell running it.
    pub fn signal(&self, signal: libc::c_int) {
        let pid: libc::pid_t =
            poll_until(Duration::from_secs(10), Duration::from_millis(20), || {
                match std::fs::read_to_string(&self.pid)
                    .ok()
                    .and_then(|p| p.trim().parse().ok())
                {
                    Some(pid) => Observation::Ready(pid),
                    None => Observation::Pending(format!("{} has no pid", self.pid.display())),
                }
            })
            .unwrap_or_else(|e| panic!("wrapped command's pid never recorded: {e}"));
        // SAFETY: kill(2) takes a pid and a signal number.
        let rc = unsafe { libc::kill(pid, signal) };
        assert_eq!(rc, 0, "signal the wrapped command (pid {pid})");
    }

    /// Wait until what the CLI wrote to its terminal contains `needle`.
    pub fn wait_output_contains(&self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.cli.output().contains(needle) {
            assert!(
                Instant::now() < deadline,
                "the terminal never showed {needle:?}; cli output: {}",
                self.cli.output()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn exited(&self) -> bool {
        self.code.exists()
    }

    /// Wait for the wrapped command to exit and return its exit code.
    pub fn wait_exit(&self, within: Duration) -> i32 {
        poll_path_exists(&self.code, within, Duration::from_millis(20)).unwrap_or_else(|e| {
            panic!(
                "the wrapped command did not exit within {within:?} ({e}); cli output: {}",
                self.cli.output()
            )
        });
        std::fs::read_to_string(&self.code)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    pub fn assert_terminal_restored(&self) {
        let before = std::fs::read_to_string(&self.before).unwrap();
        let after = std::fs::read_to_string(&self.after).unwrap();
        assert_eq!(
            before, after,
            "terminal settings after the command differ from before"
        );
    }

    /// [`assert_terminal_restored`](Self::assert_terminal_restored), except
    /// for PENDIN. The BSD tty driver sets PENDIN whenever canonical mode is
    /// turned back on, and clears it on the next read. `tendr attach` flushes
    /// input to clear it, but `ssh` does not, so a test of the terminal ssh
    /// restores ignores it. Linux never sets it this way.
    pub fn assert_terminal_restored_but_pendin(&self) {
        let without_pendin = |settings: String| -> String {
            let lflag = |field: &str| {
                let hex = field.strip_prefix("lflag=")?;
                libc::tcflag_t::from_str_radix(hex, 16).ok()
            };
            settings
                .trim_end()
                .split(':')
                .map(|field| match lflag(field) {
                    Some(flags) => format!("lflag={:x}", flags & !libc::PENDIN),
                    None => field.to_owned(),
                })
                .collect::<Vec<_>>()
                .join(":")
        };
        let before = without_pendin(std::fs::read_to_string(&self.before).unwrap());
        let after = without_pendin(std::fs::read_to_string(&self.after).unwrap());
        assert_eq!(
            before, after,
            "terminal settings after the command differ from before, PENDIN aside"
        );
    }
}
