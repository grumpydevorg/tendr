#![cfg(unix)]

mod harness;

use harness::terminal::{CliAttach, WrappedAttach, tendr_bin};
use harness::{Observation, poll_until, tendr};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tendr::attach_proto::{
    MSG_ACCEPTED, MSG_DATA, MSG_DETACH, MSG_HELLO, MSG_INPUT_DONE, MSG_REJECTED, MSG_RESIZE,
    MSG_RETIRED, Mode, PROTOCOL_VERSION, read_msg, resize_payload,
};
use tendr::recording::Geometry;

static SERIAL: Mutex<()> = Mutex::new(());

/// Frame and send one attach message (`[type][u32 len][payload]`).
fn write_msg(stream: &mut UnixStream, msg_type: u8, payload: &[u8]) {
    stream.write_all(&[msg_type]).unwrap();
    stream
        .write_all(&(payload.len() as u32).to_be_bytes())
        .unwrap();
    stream.write_all(payload).unwrap();
    stream.flush().unwrap();
}

#[test]
fn start_pty_flag_sets_io_mode() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let output = tendr(&root)
        .args(["start", "pty-test", "--pty", "--", "echo", "hello"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let meta: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(meta["launch_spec"]["io_mode"], "Pty");
}

#[test]
fn start_pty_session_captures_output() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "pty-echo", "--pty", "--", "echo", "pty-hello"])
        .output()
        .unwrap();

    harness::wait_terminal(&root, "pty-echo");

    let output = tendr(&root)
        .args(["log", "pty-echo", "--raw"])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("pty-hello"),
        "PTY output should be captured in log: {stdout}"
    );
}

/// A short-lived child's output survives a PTY reader that is starved until
/// well after the child wrote and exited. On macOS the exiting child's close
/// of the terminal waits only briefly (about 0.65 s measured) for unread
/// output to be read before discarding it, unless something else still holds
/// the terminal open; the sidecar does, until it has read what the child
/// left. The debug-only `TENDR_TEST_PTY_READ_DELAY_MS` stalls the reader.
#[test]
fn pty_output_survives_a_reader_stalled_past_the_childs_exit() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .env("TENDR_TEST_PTY_READ_DELAY_MS", "2000")
        .args(["start", "pty-stall", "--pty", "--", "echo", "pty-hello"])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "pty-stall");
    let meta = harness::wait_terminal(&root, "pty-stall");
    assert_eq!(meta["reason"], "ExitedOk", "{meta}");

    let output = tendr(&root)
        .args(["log", "pty-stall", "--raw"])
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&output.stdout);
    assert!(log.contains("pty-hello"), "output.log lost it: {log:?}");
    let recorded = recorded_output(&decode_session_recording(&root, "pty-stall").records);
    assert!(
        contains(&recorded, b"pty-hello"),
        "the recording lost it: {:?}",
        String::from_utf8_lossy(&recorded)
    );
}

/// The PTY capture ends when the terminal does, or at most 2 s after the
/// child has exited and its output has been read, and the sidecar's own hold
/// on the terminal changes neither. A background job hung up by its session
/// leader's exit ends with it, so capture does not wait for the job's 30 s.
/// One that ignores the hangup and keeps the terminal is captured on Linux
/// while it writes within those 2 s; on macOS the leader's exit ends the
/// session's use of the terminal, so its later output is not captured.
#[test]
fn pty_capture_reads_a_held_terminal_for_up_to_2_s_after_the_child() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let run = |session: &str, script: &str| -> String {
        tendr(&root)
            .args(["start", session, "--pty", "--", "sh", "-c", script])
            .assert()
            .success();
        let _session = harness::SessionGuard::new(&root, session);
        let meta = harness::wait_terminal(&root, session);
        assert_eq!(meta["reason"], "ExitedOk", "{meta}");
        let output = tendr(&root)
            .args(["log", session, "--raw"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    };

    let log = run("pty-hungup-job", "sleep 30 & echo started");
    assert!(log.contains("started"), "{log:?}");

    let log = run(
        "pty-kept-job",
        "trap '' HUP; (sleep 1; echo late) & echo started",
    );
    assert!(log.contains("started"), "{log:?}");
    if cfg!(target_os = "macos") {
        assert!(!log.contains("late"), "{log:?}");
    } else {
        assert!(log.contains("late"), "{log:?}");
    }
}

/// Puts the program after it (its first argument is a pid file) in a new
/// session of its own, as `setsid` does (macOS has no `setsid` command), then
/// writes its pid to the pid file. The pid file therefore appears only once
/// the program is outside the child's session and process group, where no
/// hangup or group signal reaches it. It keeps the terminal descriptors it
/// inherited.
const IN_OWN_SESSION: &str = r#"perl -MPOSIX -e 'POSIX::setsid() or die "setsid: $!"; my $p = shift; open(my $f, ">", "$p.tmp") or die; print $f "$$\n"; close $f; rename("$p.tmp", $p) or die; exec @ARGV or die'"#;

/// Shell code that waits until the pid file `"$1"` exists.
const AWAIT_HOLDER: &str = r#"while [ ! -e "$1" ]; do sleep 0.05; done"#;

/// Kills the process whose pid a test's child wrote to the file, when
/// dropped, so a holder that outlives its run never outlives the test.
struct KillPidFile(std::path::PathBuf);

impl Drop for KillPidFile {
    fn drop(&mut self) {
        if let Some(pid) = read_pid(&self.0) {
            // SAFETY: the pid is the test's own holder process.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

fn read_pid(path: &std::path::Path) -> Option<libc::pid_t> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn epoch_secs(value: &serde_json::Value) -> u64 {
    value
        .as_str()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or_else(|| panic!("not epoch seconds: {value}"))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// On Linux, the run carries a warning naming `holder` among the processes
/// that still held its terminal. macOS revokes the terminal when the child,
/// its session leader, exits, so nothing holds it there.
fn assert_holder_named(meta: &serde_json::Value, holder: libc::pid_t) {
    if !cfg!(target_os = "linux") {
        return;
    }
    let pid = holder.to_string();
    let named = meta["warnings"].as_array().is_some_and(|warnings| {
        warnings.iter().filter_map(|w| w.as_str()).any(|w| {
            w.contains("terminal") && w.split(|c: char| !c.is_ascii_digit()).any(|n| n == pid)
        })
    });
    assert!(named, "no warning names holder {holder}: {meta}");
}

/// A process that keeps the terminal after the child has exited (here one
/// that left the child's session) does not keep the run going. Capture ends
/// about 2 s after the child's exit; the child is reaped and its own exit
/// recorded, with `ended_at` near that exit, not the holder's.
#[test]
fn a_terminal_holder_does_not_keep_an_exited_pty_run_going() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let holder = root.path().join("holder.pid");
    let exited = root.path().join("exited");
    let _holder = KillPidFile(holder.clone());

    let script =
        format!(r#"{IN_OWN_SESSION} "$1" sleep 30 & {AWAIT_HOLDER}; date +%s > "$2"; exit 3"#);
    tendr(&root)
        .args([
            "start", "pty-held", "--pty", "--", "sh", "-c", &script, "sh",
        ])
        .args([&holder, &exited])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "pty-held");
    let meta = harness::wait_terminal(&root, "pty-held");

    assert_eq!(meta["reason"], "ExitedError", "{meta}");
    assert_eq!(meta["code"], 3, "{meta}");
    let exited_at: u64 = std::fs::read_to_string(&exited)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let ended_at = epoch_secs(&meta["ended_at"]);
    assert!(
        ended_at <= exited_at + 4,
        "ended_at {ended_at} is long after the child's exit at {exited_at}: {meta}"
    );
    assert_holder_named(&meta, read_pid(&holder).expect("holder pid"));
}

/// `kill` of a PTY run whose terminal is still held by a process outside the
/// child's session ends the run once the child is gone, and reports that end:
/// `Killed`, promptly, with `ended_at` near the kill. The stop ends the
/// holder too, before the end is recorded, so the terminal closes and
/// capture ends with no holder warning.
#[test]
fn kill_ends_a_pty_run_whose_terminal_is_held() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let holder = root.path().join("holder.pid");
    let _holder = KillPidFile(holder.clone());

    let script = format!(r#"{IN_OWN_SESSION} "$1" sleep 30 & {AWAIT_HOLDER}; exec sleep 600"#);
    tendr(&root)
        .args([
            "start",
            "pty-held-kill",
            "--pty",
            "--",
            "sh",
            "-c",
            &script,
            "sh",
        ])
        .arg(&holder)
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "pty-held-kill");
    harness::wait_running(&root, "pty-held-kill");
    harness::poll_path_exists(&holder, Duration::from_secs(10), Duration::from_millis(20))
        .unwrap_or_else(|e| panic!("the holder never started: {e}"));

    let killed_at = now_secs();
    let started = Instant::now();
    let out = tendr(&root)
        .args(["kill", "pty-held-kill"])
        .output()
        .unwrap();
    let elapsed = started.elapsed();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let meta: serde_json::Value = serde_json::from_str(&stdout).expect("kill prints meta");
    assert_eq!(meta["status"], "Exited", "{meta}");
    assert_eq!(meta["reason"], "Killed", "{meta}");
    assert!(
        elapsed < Duration::from_secs(8),
        "kill took {elapsed:?}: {meta}"
    );
    let ended_at = epoch_secs(&meta["ended_at"]);
    assert!(
        ended_at <= killed_at + 5,
        "ended_at {ended_at} is long after the kill at {killed_at}: {meta}"
    );
    let holder = read_pid(&holder).expect("holder pid");
    // SAFETY: signal 0 only probes; the pid is the test's own holder.
    let alive = unsafe { libc::kill(holder, 0) } == 0 && !is_zombie(holder);
    assert!(!alive, "holder {holder} outlived the stop: {meta}");
    let warned = meta["warnings"].as_array().is_some_and(|w| {
        w.iter()
            .any(|w| w.as_str().is_some_and(|w| w.contains("terminal")))
    });
    assert!(
        !warned,
        "a holder warning after the stop ended the holder: {meta}"
    );
}

/// Whether `pid` is a zombie, per `ps`.
fn is_zombie(pid: libc::pid_t) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|out| {
            String::from_utf8_lossy(&out.stdout)
                .trim_start()
                .starts_with('Z')
        })
}

/// A holder that keeps writing to the terminal after the child has exited
/// cannot stretch the run either: capture ends about 2 s after the child's
/// exit, and the holder's later output is not captured.
#[test]
fn a_writing_terminal_holder_does_not_keep_an_exited_pty_run_going() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let holder = root.path().join("holder.pid");
    let _holder = KillPidFile(holder.clone());

    // Writes `tick N` every 100 ms for 10 s.
    let ticker = "sh -c 'i=0; while [ $i -lt 100 ]; do i=$((i+1)); echo tick $i; sleep 0.1; done'";
    let script = format!(r#"{IN_OWN_SESSION} "$1" {ticker} & {AWAIT_HOLDER}; exit 0"#);
    let started = Instant::now();
    tendr(&root)
        .args([
            "start",
            "pty-ticking",
            "--pty",
            "--",
            "sh",
            "-c",
            &script,
            "sh",
        ])
        .arg(&holder)
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "pty-ticking");
    let meta = harness::wait_terminal(&root, "pty-ticking");
    let elapsed = started.elapsed();

    assert_eq!(meta["reason"], "ExitedOk", "{meta}");
    assert!(
        elapsed < Duration::from_secs(5),
        "the run took {elapsed:?} to end: {meta}"
    );
    let output = tendr(&root)
        .args(["log", "pty-ticking", "--raw"])
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&output.stdout);
    let ticks: Vec<u32> = log
        .split("tick ")
        .skip(1)
        .filter_map(|rest| {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        })
        .collect();
    // Tick 45 comes 4.5 s after the holder started.
    assert!(
        ticks.iter().all(|&n| n < 45),
        "late ticks were captured: {ticks:?}"
    );
    if cfg!(target_os = "linux") {
        assert!(
            !ticks.is_empty(),
            "the holder's output within the grace was not captured: {log:?}"
        );
    }
    assert_holder_named(&meta, read_pid(&holder).expect("holder pid"));
}

/// A `^S` sent to a PTY session stops its output. On macOS a child that
/// writes and exits while it is stopped cannot finish exiting until the
/// output drains, which nothing can do while it is stopped; on Linux the
/// write itself blocks. Either way `kill` must end the run: every stop
/// restarts output, and a forced one also releases the sidecar's hold on the
/// terminal. A graceful stop keeps the hold, so on macOS capture still gets
/// what the child wrote. Both a child exec'd directly and a shell (which
/// opens `/dev/tty`, holding the terminal itself) are covered.
#[test]
fn kill_ends_a_pty_run_whose_output_is_stopped() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let direct = r#"$| = 1; select(undef, undef, undef, 0.05) until -e $ARGV[0];
        print "pending\n"; open(my $f, ">", $ARGV[1]) or die; close $f"#;
    let shell = r#"while [ ! -e "$1" ]; do sleep 0.05; done; echo pending; : > "$2""#;
    for (session, program, force) in [
        ("xoff-direct", ["perl", "-e", direct], true),
        ("xoff-shell", ["sh", "-c", shell], true),
        ("xoff-direct-graceful", ["perl", "-e", direct], false),
        ("xoff-shell-graceful", ["sh", "-c", shell], false),
    ] {
        let go = root.path().join(format!("{session}.go"));
        let written = root.path().join(format!("{session}.written"));
        let mut argv: Vec<&str> = program.to_vec();
        if program[0] == "sh" {
            argv.push("sh");
        }
        argv.extend([go.to_str().unwrap(), written.to_str().unwrap()]);
        tendr(&root)
            .args(["start", session, "--pty", "--"])
            .args(&argv)
            .assert()
            .success();
        let _session = harness::SessionGuard::new(&root, session);
        harness::wait_running(&root, session);

        tendr(&root)
            .args(["push", session])
            .write_stdin("\x13")
            .assert()
            .success();
        std::fs::write(&go, b"").unwrap();
        // macOS queues the write, so the child goes on to exit; Linux blocks
        // it until output restarts, so the child never gets past it there.
        if cfg!(target_os = "macos") {
            harness::poll_path_exists(&written, Duration::from_secs(10), Duration::from_millis(20))
                .unwrap_or_else(|e| panic!("{session}: the child never wrote: {e}"));
        }
        // Let the child reach its exit (macOS) or its write (Linux).
        std::thread::sleep(Duration::from_millis(300));

        // Already ended by itself is fine too; only a stuck run fails.
        let mut kill = tendr(&root);
        kill.args(["kill", session]);
        if force {
            kill.arg("--force");
        }
        let _ = kill.output();
        let meta = harness::poll_terminal_ns(&root, "default", session)
            .unwrap_or_else(|e| panic!("{session}: still not ended after kill: {e}"));
        assert_eq!(meta["status"], "Exited", "{session}: {meta}");
        if !force && cfg!(target_os = "macos") {
            let output = tendr(&root)
                .args(["log", session, "--raw"])
                .output()
                .unwrap();
            let log = String::from_utf8_lossy(&output.stdout);
            assert!(log.contains("pending"), "{session}: {log:?}");
        }
    }
}

/// A graceful `kill` keeps the sidecar's hold on the terminal while capture
/// is running, so what the child writes as it hangs up is still captured by
/// a reader that is stalled past the child's exit. (Only a forced stop, or a
/// stop with no reader running, gives the hold up at once.) The pause after
/// `Running` lets capture start, so the kill finds the reader in place.
#[test]
fn graceful_kill_keeps_a_stalled_readers_output() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .env("TENDR_TEST_PTY_READ_DELAY_MS", "3000")
        .args(["start", "pty-hangup", "--pty", "--", "perl", "-e"])
        .arg(r#"$| = 1; $SIG{HUP} = sub { print "final line\n"; exit 0 }; sleep 60"#)
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "pty-hangup");
    harness::wait_running(&root, "pty-hangup");
    std::thread::sleep(Duration::from_millis(300));
    tendr(&root).args(["kill", "pty-hangup"]).assert().success();
    harness::wait_terminal(&root, "pty-hangup");

    let output = tendr(&root)
        .args(["log", "pty-hangup", "--raw"])
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&output.stdout);
    assert!(log.contains("final line"), "output.log lost it: {log:?}");
}

/// The `TERM` a PTY child sees when started with `inherited` in the caller's
/// environment and `extra` arguments to `start`.
fn pty_child_term(
    root: &TempDir,
    session: &str,
    inherited: Option<&str>,
    extra: &[&str],
) -> String {
    let mut cmd = tendr(root);
    match inherited {
        Some(term) => cmd.env("TERM", term),
        None => cmd.env_remove("TERM"),
    };
    cmd.args(["start", session, "--pty"])
        .args(extra)
        .args(["--", "sh", "-c", "echo \"TERM=[${TERM-unset}]\""])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(root, session);
    harness::wait_terminal(root, session);
    let log = wait_output_contains(root, session, "TERM=[");
    let start = log.find("TERM=[").unwrap() + "TERM=[".len();
    let term = log[start..].split(']').next().unwrap().to_owned();
    assert_eq!(
        decode_session_recording(root, session).header.term.as_str(),
        term,
        "{session}: the recording names the TERM the child ran under"
    );
    term
}

/// A PTY is a real terminal, so a caller's `TERM=dumb` (an agent's tool
/// shell) or no `TERM` at all (`ssh -T`) would tell its child it cannot draw.
/// Such a session gets `xterm-256color`, as does one started with
/// `--unset-env TERM`; a real `TERM` is kept, and an explicit `--env TERM=…`
/// always wins.
#[test]
fn pty_child_gets_a_usable_term() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    assert_eq!(
        pty_child_term(&root, "term-dumb", Some("dumb"), &[]),
        "xterm-256color"
    );
    assert_eq!(
        pty_child_term(&root, "term-unset", None, &[]),
        "xterm-256color"
    );
    assert_eq!(
        pty_child_term(&root, "term-empty", Some(""), &[]),
        "xterm-256color"
    );
    assert_eq!(
        pty_child_term(&root, "term-real", Some("xterm-ghostty"), &[]),
        "xterm-ghostty"
    );
    assert_eq!(
        pty_child_term(
            &root,
            "term-explicit",
            Some("xterm-ghostty"),
            &["--env", "TERM=dumb"]
        ),
        "dumb"
    );
    assert_eq!(
        pty_child_term(
            &root,
            "term-unset-flag",
            Some("xterm-ghostty"),
            &["--unset-env", "TERM"]
        ),
        "xterm-256color"
    );
    assert_eq!(
        pty_child_term(
            &root,
            "term-unset-then-env",
            Some("xterm-ghostty"),
            &["--unset-env", "TERM", "--env", "TERM=dumb"]
        ),
        "dumb"
    );
}

#[test]
fn start_pty_session_shows_pty_metadata() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "pty-meta", "--pty", "--", "echo", "hi"])
        .output()
        .unwrap();

    harness::wait_terminal(&root, "pty-meta");

    let output = tendr(&root).args(["status", "pty-meta"]).output().unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let meta: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(meta["pty"]["enabled"], true);
    assert_eq!(meta["pty"]["control"], "AgentControl");
    assert_eq!(meta["launch_spec"]["io_mode"], "Pty");
}

#[test]
fn exec_rejected_on_pty_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args([
            "start",
            "pty-shell",
            "--pty",
            "--stdin",
            "--",
            "sleep",
            "60",
        ])
        .output()
        .unwrap();
    let _session = harness::SessionGuard::new(&root, "pty-shell");
    harness::wait_running(&root, "pty-shell");

    let output = tendr(&root)
        .args(["exec", "pty-shell", "--", "echo", "test"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not supported") || stderr.contains("PTY"),
        "should reject exec on PTY: {stderr}"
    );
}

#[test]
fn attach_to_non_pty_session_fails() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "pipe-session", "--", "sleep", "60"])
        .output()
        .unwrap();
    let _session = harness::SessionGuard::new(&root, "pipe-session");
    harness::wait_running(&root, "pipe-session");

    let output = tendr(&root)
        .args(["attach", "pipe-session"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("PTY") || stderr.contains("not PTY"),
        "should reject attach on non-PTY: {stderr}"
    );
}

#[test]
fn attach_socket_exists_for_pty_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "pty-attach", "--pty", "--", "sleep", "60"])
        .output()
        .unwrap();
    let _session = harness::SessionGuard::new(&root, "pty-attach");
    harness::wait_running(&root, "pty-attach");

    let breadcrumb = root
        .path()
        .join(".tendr/sessions/default/pty-attach/a.sock.path");

    // The attach listener thread may not have written the breadcrumb yet.
    harness::poll_path_exists(
        &breadcrumb,
        Duration::from_secs(5),
        Duration::from_millis(50),
    )
    .unwrap_or_else(|e| panic!("timed out waiting for a.sock.path breadcrumb to appear: {e}"));

    // The breadcrumb should point to an actual socket file
    let sock_path = std::fs::read_to_string(&breadcrumb).unwrap();
    let sock_path = sock_path.trim();
    assert!(
        std::path::Path::new(sock_path).exists(),
        "socket file should exist at {sock_path}"
    );
}

/// A PTY push travels over the attach socket, which every PTY session has, so
/// it needs no `--stdin` (that flag adds the FIFO `exec` uses).
#[test]
fn push_to_pty_session_delivers_input_without_stdin_flag() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "pty-push", "--pty", "--", "cat"])
        .output()
        .unwrap();
    let _session = harness::SessionGuard::new(&root, "pty-push");
    harness::wait_running(&root, "pty-push");

    tendr(&root)
        .args(["push", "pty-push"])
        .write_stdin(b"hello-from-push\n")
        .assert()
        .success();

    // Poll the log until the pushed input echoes through the PTY (no fixed sleep).
    poll_until(Duration::from_secs(10), Duration::from_millis(20), || {
        let output = tendr(&root)
            .args(["log", "pty-push", "--raw"])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if stdout.contains("hello-from-push") {
            Observation::Ready(())
        } else {
            Observation::Pending(stdout)
        }
    })
    .unwrap_or_else(|e| panic!("push input never appeared in PTY log: {e}"));
}

/// A PTY session still waiting on `--after` has no `pty` record yet, but it
/// will take pushes once it runs: a push now is refused as not running, not
/// for a missing `--stdin`.
#[test]
fn push_to_a_starting_pty_session_without_stdin_reports_not_running() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "pty-dep", "--", "sleep", "120"])
        .assert()
        .success();
    let _dep = harness::SessionGuard::new(&root, "pty-dep");
    tendr(&root)
        .args([
            "start",
            "pty-after",
            "--pty",
            "--after",
            "pty-dep",
            "--",
            "cat",
        ])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "pty-after");

    let output = tendr(&root)
        .args(["push", "pty-after"])
        .write_stdin(b"early\n")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a push before the run starts fails"
    );
    assert!(
        stderr.contains("not running"),
        "the refusal says the session is not running: {stderr}"
    );
}

/// Killing a PTY session hangs up its terminal first, as closing a terminal
/// window does, so an interactive shell's background jobs end with it. Those
/// jobs sit in their own process groups, outside the group kill, and a shell
/// that is SIGKILLed cannot pass a hangup on to them.
#[test]
fn kill_pty_shell_ends_its_background_jobs() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let pid_file = root.path().join("job.pid");

    tendr(&root)
        .args([
            "start",
            "pty-jobs",
            "--pty",
            "--stdin",
            "--",
            "bash",
            "--norc",
            "--noprofile",
            "-i",
        ])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "pty-jobs");
    harness::wait_running(&root, "pty-jobs");

    tendr(&root)
        .args(["push", "pty-jobs"])
        .write_stdin(format!("sleep 600 & echo $! > {}\n", pid_file.display()))
        .assert()
        .success();
    let job: libc::pid_t =
        harness::poll_until(Duration::from_secs(10), Duration::from_millis(20), || {
            match std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|s| s.trim().parse().ok())
            {
                Some(pid) => harness::Observation::Ready(pid),
                None => harness::Observation::Pending(format!("no pid in {}", pid_file.display())),
            }
        })
        .expect("the shell never reported its background job");

    tendr(&root).args(["kill", "pty-jobs"]).assert().success();

    // SAFETY: signal 0 only probes whether `job` exists.
    let alive = || unsafe { libc::kill(job, 0) } == 0;
    let gone = harness::poll_until(Duration::from_secs(10), Duration::from_millis(50), || {
        if alive() {
            harness::Observation::Pending(format!("job {job} still alive"))
        } else {
            harness::Observation::Ready(())
        }
    });
    if gone.is_err() {
        // SAFETY: `job` is the test's own background sleep.
        unsafe { libc::kill(job, libc::SIGKILL) };
        panic!("background job {job} outlived `tendr kill` of its PTY shell");
    }
}

/// Python REPL exec works on PTY sessions.
#[test]
fn exec_python_pty() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args([
            "start",
            "py-pty",
            "--stdin",
            "--pty",
            "--exec-target",
            "python-repl",
            "--",
            "python3",
        ])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "py-pty");
    harness::wait_running(&root, "py-pty");
    // No sleep: exec buffers the frame and waits for the result file, so the
    // REPL not being input-ready yet is a delay, not a lost command (PR #55).

    let output = tendr(&root)
        .args([
            "exec",
            "py-pty",
            "--timeout",
            "10",
            "--",
            "print('pty hello')",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"].as_i64(), Some(0));
    assert!(result["stdout"].as_str().unwrap().contains("pty hello"));
}

/// PTY exec is still rejected for shell targets.
#[test]
fn exec_pty_still_rejected_for_shells() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args([
            "start",
            "pty-shell",
            "--stdin",
            "--pty",
            "--exec-target",
            "posix-shell",
            "--",
            "bash",
        ])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "pty-shell");
    harness::wait_running(&root, "pty-shell");

    tendr(&root)
        .args(["exec", "pty-shell", "--", "echo", "test"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("not supported on PTY"));
}

/// Wait for the attach socket breadcrumb and return the socket path.
fn wait_for_attach_socket(root: &TempDir, session: &str) -> std::path::PathBuf {
    let breadcrumb = root
        .path()
        .join(format!(".tendr/sessions/default/{session}/a.sock.path"));
    poll_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        || match std::fs::read_to_string(&breadcrumb) {
            Ok(content) => {
                let p = std::path::PathBuf::from(content.trim());
                if p.exists() {
                    Observation::Ready(p)
                } else {
                    Observation::Pending(format!("socket {} absent", p.display()))
                }
            }
            Err(e) => Observation::Pending(format!("breadcrumb unreadable: {e}")),
        },
    )
    .unwrap_or_else(|e| panic!("timed out waiting for attach socket in {session}: {e}"))
}

/// Connect, send a v1 hello in `mode`, and return the stream with the sidecar's
/// reply (`(message type, payload)`), read under a deadline.
fn hello(sock_path: &std::path::Path, mode: Mode) -> (UnixStream, (u8, Vec<u8>)) {
    let mut stream = UnixStream::connect(sock_path).expect("failed to connect to attach socket");
    write_msg(&mut stream, MSG_HELLO, &[PROTOCOL_VERSION, mode as u8]);
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let reply = read_msg(&mut stream).expect("sidecar replies to hello");
    // Fails with EINVAL on macOS once the sidecar has shut a rejected socket down.
    let _ = stream.set_read_timeout(None);
    (stream, reply)
}

fn epoch_of(payload: &[u8]) -> u64 {
    u64::from_be_bytes(payload.try_into().expect("epoch payload is 8 bytes"))
}

/// Attach as a human through the v1 handshake and hold the connection.
/// Returns the stream so the caller can control when it disconnects.
fn attach_as_human(sock_path: &std::path::Path) -> UnixStream {
    let (stream, (msg_type, payload)) = hello(sock_path, Mode::Attach);
    assert_eq!(
        msg_type,
        MSG_ACCEPTED,
        "attach should be accepted: {}",
        String::from_utf8_lossy(&payload)
    );
    stream
}

/// Poll the PTY output until it contains `needle`, wherever capture split it,
/// and return the output. `tendr log --raw` prints each captured chunk as its
/// own line, so a needle that straddles two reads of the PTY never appears
/// whole there. For a child whose output has no newlines of its own, joining
/// the lines recovers the stream.
fn wait_output_contains(root: &TempDir, session: &str, needle: &str) -> String {
    poll_until(Duration::from_secs(15), Duration::from_millis(20), || {
        let output = tendr(root)
            .args(["log", session, "--raw"])
            .output()
            .unwrap();
        let joined: String = String::from_utf8_lossy(&output.stdout)
            .chars()
            .filter(|&c| c != '\n')
            .collect();
        if joined.contains(needle) {
            Observation::Ready(joined)
        } else {
            Observation::Pending(joined)
        }
    })
    .unwrap_or_else(|e| panic!("output of {session} never contained {needle:?}: {e}"))
}

/// Poll `tendr log --raw` until it contains `needle`.
fn wait_log_contains(root: &TempDir, session: &str, needle: &str) -> String {
    poll_until(Duration::from_secs(15), Duration::from_millis(20), || {
        let output = tendr(root)
            .args(["log", session, "--raw"])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if stdout.contains(needle) {
            Observation::Ready(stdout)
        } else {
            Observation::Pending(stdout)
        }
    })
    .unwrap_or_else(|e| panic!("log for {session} never contained {needle:?}: {e}"))
}

/// Read until the sidecar closes the stream, returning every message seen.
fn read_until_closed(stream: &mut UnixStream) -> Vec<(u8, Vec<u8>)> {
    // EINVAL on macOS means the socket is already shut down, and a read then
    // returns end of stream immediately, so ignoring it cannot hang.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut seen = Vec::new();
    loop {
        match read_msg(stream) {
            Ok(msg) => seen.push(msg),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
                ) =>
            {
                return seen;
            }
            Err(e) => panic!("expected the sidecar to close the stream, got {e}"),
        }
    }
}

/// Wait for meta.json PTY control to reach a specific state.
fn wait_for_pty_control(root: &TempDir, session: &str, expected: &str) {
    wait_for_pty_control_within(root, session, expected, Duration::from_secs(5));
}

/// [`wait_for_pty_control`], for a change that may take up to `within`.
fn wait_for_pty_control_within(root: &TempDir, session: &str, expected: &str, within: Duration) {
    let meta_path = root
        .path()
        .join(format!(".tendr/sessions/default/{session}/meta.json"));
    poll_until(within, Duration::from_millis(50), || {
        let control = std::fs::read_to_string(&meta_path)
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
            .map(|meta| meta["pty"]["control"].clone());
        match control {
            Some(c) if c.as_str() == Some(expected) => Observation::Ready(()),
            Some(c) => Observation::Pending(format!("pty.control={c}")),
            None => Observation::Pending("meta unreadable".into()),
        }
    })
    .unwrap_or_else(|e| panic!("timed out waiting for pty.control={expected} in {session}: {e}"))
}

#[test]
fn push_rejected_during_human_control() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Start a PTY session with stdin
    tendr(&root)
        .args(["start", "pty-hc", "--pty", "--stdin", "--", "cat"])
        .output()
        .unwrap();
    let _session = harness::SessionGuard::new(&root, "pty-hc");
    harness::wait_running(&root, "pty-hc");

    let sock_path = wait_for_attach_socket(&root, "pty-hc");

    // Simulate a human attaching
    let _human = attach_as_human(&sock_path);
    wait_for_pty_control(&root, "pty-hc", "HumanControl");

    // Push should be rejected
    let output = tendr(&root)
        .args(["push", "pty-hc"])
        .write_stdin(b"rejected\n")
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("human control"),
        "push should be rejected during human control: {stderr}"
    );

    // Drop the human connection (detach)
    drop(_human);
    wait_for_pty_control(&root, "pty-hc", "AgentControl");

    // Push should work again
    let output = tendr(&root)
        .args(["push", "pty-hc"])
        .write_stdin(b"accepted\n")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "push should succeed after detach: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The sidecar's own whole-meta writes must carry the live control owner, not
/// the `AgentControl` it started with. The gated `output_log` fault forces one
/// such write mid-run, after the human has taken control.
#[test]
fn sidecar_meta_write_keeps_human_control() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let gate = root.path().join("fault-gate");

    tendr(&root)
        .env("TENDR_TEST_FAIL", "output_log")
        .env("TENDR_TEST_FAULT_GATE", &gate)
        .args(["start", "pty-keep", "--pty", "--", "cat"])
        .output()
        .unwrap();
    let _session = harness::SessionGuard::new(&root, "pty-keep");
    harness::wait_running(&root, "pty-keep");

    let sock_path = wait_for_attach_socket(&root, "pty-keep");
    let _human = attach_as_human(&sock_path);
    wait_for_pty_control(&root, "pty-keep", "HumanControl");

    std::fs::write(&gate, b"").unwrap();
    let meta_path = root
        .path()
        .join(".tendr/sessions/default/pty-keep/meta.json");
    let meta = poll_until(Duration::from_secs(10), Duration::from_millis(50), || {
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
        let written = meta["warnings"].as_array().is_some_and(|w| {
            w.iter()
                .any(|w| w.as_str().is_some_and(|w| w.contains("output.log")))
        });
        if written {
            Observation::Ready(meta)
        } else {
            Observation::Pending(meta.to_string())
        }
    })
    .unwrap_or_else(|e| panic!("sidecar never rewrote meta: {e}"));

    assert_eq!(meta["status"], "Running");
    assert_eq!(meta["pty"]["control"], "HumanControl");
}

#[test]
fn attach_contention_rejected() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "pty-contend", "--pty", "--stdin", "--", "cat"])
        .output()
        .unwrap();
    let _session = harness::SessionGuard::new(&root, "pty-contend");
    harness::wait_running(&root, "pty-contend");

    let sock_path = wait_for_attach_socket(&root, "pty-contend");

    // First human attaches
    let _human = attach_as_human(&sock_path);
    wait_for_pty_control(&root, "pty-contend", "HumanControl");

    // Second attach via CLI should be rejected
    let output = tendr(&root)
        .args(["attach", "pty-contend"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("already under human control"),
        "second attach should be rejected: {stderr}"
    );

    drop(_human);
}

#[test]
fn resize_reaches_child_pty() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // An interactive shell so we can query the child's terminal size post-resize.
    tendr(&root)
        .args(["start", "pty-resize", "--pty", "--stdin", "--", "sh"])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-resize");

    let sock_path = wait_for_attach_socket(&root, "pty-resize");
    let mut stream = attach_as_human(&sock_path);
    wait_for_pty_control(&root, "pty-resize", "HumanControl");

    // Collect child output (MSG_DATA) on a reader thread so the observation below
    // is deadline-bounded, not a blocking framed read.
    let seen = Arc::new(Mutex::new(String::new()));
    let reader = {
        let mut r = stream.try_clone().unwrap();
        let seen = Arc::clone(&seen);
        std::thread::spawn(move || {
            while let Ok((msg_type, payload)) = read_msg(&mut r) {
                if msg_type == MSG_DATA {
                    seen.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push_str(&String::from_utf8_lossy(&payload));
                }
            }
        })
    };

    // 1) Resize to 40x120. 2) On the SAME ordered socket, ask the child its
    // terminal size. The listener processes messages sequentially, so `stty size`
    // runs only after apply_pty_resize returns — child-reported "40 120" proves
    // the resize actually reached the child's PTY, not merely that it parsed.
    //
    // The sentinel is split as `__RESIZE""_DONE__` to make it a *causal*
    // completion token: the shell's echo of the input line cannot contain the
    // bare `__RESIZE_DONE__` (the quotes are only stripped when the command
    // runs), so that token can appear only in the command's output — strictly
    // after `stty size` has printed the dimensions.
    write_msg(
        &mut stream,
        MSG_RESIZE,
        &resize_payload(Geometry::new(40, 120).unwrap()),
    );
    write_msg(
        &mut stream,
        MSG_DATA,
        b"stty size; echo __RESIZE\"\"_DONE__\n",
    );

    // Observe by *returning a result*, not asserting — so a timeout cannot bypass
    // the detach/join/kill cleanup below and leak the PTY session. Require BOTH
    // the child-observed dimensions (load-bearing) and the completion token
    // (proves `stty size` finished, so the dimensions are settled, not mid-write).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let observed: Result<(), String> = loop {
        let out = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if out.contains("40 120") && out.contains("__RESIZE_DONE__") {
            break Ok(());
        }
        if std::time::Instant::now() >= deadline {
            break Err(format!(
                "child never reported the resized dimensions + completion token; got: {out:?}"
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };

    // Secondary safety snapshot, taken while still under human control.
    let status = tendr(&root)
        .args(["status", "pty-resize"])
        .output()
        .unwrap();
    let meta: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&status.stdout)).unwrap();

    // Deterministic cleanup that runs regardless of the observation outcome:
    // MSG_DETACH makes the listener close the connection, so the reader receives
    // EOF and joins without depending on the kill; then the shell is reaped.
    write_msg(&mut stream, MSG_DETACH, &[]);
    drop(stream);
    let _ = reader.join();
    tendr(&root)
        .args(["kill", "pty-resize", "--force"])
        .assert()
        .success();

    // Assert only after cleanup has run.
    observed.expect("resize observation");
    assert_eq!(meta["status"], "Running", "session should still be running");
    assert_eq!(meta["pty"]["control"], "HumanControl");
}

// --- Slice 3: pty.control_changed events (plan scope 6) ---

/// Attach then detach emits two pty.control_changed events — the shipped
/// PtyControl vocabulary, exactly the three pinned data fields, appended
/// before the corresponding meta flip, from the attach thread's own writer.
#[test]
fn attach_detach_emit_control_changed_events() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "pty-ev", "--pty", "--stdin", "--", "cat"])
        .output()
        .unwrap();
    let _session = harness::SessionGuard::new(&root, "pty-ev");
    harness::wait_running(&root, "pty-ev");
    let sock_path = wait_for_attach_socket(&root, "pty-ev");

    let human = attach_as_human(&sock_path);
    wait_for_pty_control(&root, "pty-ev", "HumanControl");
    // WAL order: once meta shows the flip, the event must already be stored.
    let events = harness::read_events(&root, "pty-ev");
    assert!(
        events.iter().any(|e| e["kind"] == "pty.control_changed"),
        "event lands before the meta control write"
    );

    drop(human);
    wait_for_pty_control(&root, "pty-ev", "AgentControl");

    let events = harness::read_events(&root, "pty-ev");
    let changed: Vec<_> = events
        .iter()
        .filter(|e| e["kind"] == "pty.control_changed")
        .collect();
    assert_eq!(changed.len(), 2);
    assert_eq!(
        changed[0]["data"],
        serde_json::json!({"control": "HumanControl", "trigger": "attach", "epoch": 1}),
        "minimal by design — a control fact, not a screen event"
    );
    assert_eq!(
        changed[1]["data"],
        serde_json::json!({"control": "AgentControl", "trigger": "detach", "epoch": 1}),
        "a detach names the epoch it released"
    );
    for event in &changed {
        assert_eq!(event["source"], "tendr.sidecar");
    }

    // The attach thread owns its own writer (multi-writer by design).
    let started = events.iter().find(|e| e["kind"] == "run.started").unwrap();
    assert_ne!(
        changed[0]["writer"], started["writer"],
        "not the lifecycle writer"
    );
    assert_eq!(changed[0]["writer"], changed[1]["writer"]);
    assert_eq!(changed[0]["seq"], 1);
    assert_eq!(changed[1]["seq"], 2);
}

// --- Cloud PTY plan: sidecar-enforced input authority ---

#[test]
fn cli_escape_detaches_and_restores_the_terminal() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-escape");
    start_cat(&root, "pty-escape");

    let mut wrapped = WrappedAttach::spawn(&root, "escape", &["pty-escape"]);
    wrapped
        .cli
        .type_until_logged(&root, "pty-escape", "before-detach\n");
    wrapped.cli.type_bytes(b"\x1cd");

    assert_eq!(wrapped.wait_exit(Duration::from_secs(10)), 0);
    wrapped.assert_terminal_restored();
    wait_for_pty_control(&root, "pty-escape", "AgentControl");
    push(&root, "pty-escape", b"session-still-running\n");
    wait_log_contains(&root, "pty-escape", "session-still-running");
}

/// A PTY child that turns on the alternate screen, mouse reports and kitty
/// keyboard flags (1 on the main screen, 5 on the alternate one) once it reads
/// a byte, then prints `modes-on`.
fn start_mode_setter(root: &TempDir, session: &str) {
    tendr(root)
        .args(["start", session, "--pty", "--stdin", "--"])
        .args([
            "sh",
            "-c",
            r#"stty raw -echo; head -c 1 >/dev/null; printf '\033[>1u\033[?1049h\033[>5u\033[?1003h'; printf modes-on; exec cat"#,
        ])
        .output()
        .unwrap();
    harness::wait_running(root, session);
}

#[test]
fn detach_turns_off_the_modes_the_child_turned_on() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-modes-off");
    start_mode_setter(&root, "pty-modes-off");

    let mut wrapped = WrappedAttach::spawn(&root, "modes-off", &["pty-modes-off"]);
    wait_for_pty_control(&root, "pty-modes-off", "HumanControl");
    wrapped.cli.type_bytes(b"x");
    wrapped.wait_output_contains("modes-on");
    wrapped.cli.type_bytes(b"\x1cd");
    assert_eq!(wrapped.wait_exit(Duration::from_secs(10)), 0);

    let output = wrapped.cli.output();
    let after = output.rsplit("modes-on").next().unwrap();
    assert_eq!(
        after.replace('\x1b', "^["),
        "^[[<1u^[[?1049l^[[<1u^[[?1003l",
        "leaving must turn off, in reverse, exactly the modes the child turned on"
    );
    wrapped.assert_terminal_restored();
}

#[test]
fn attach_replays_the_modes_set_before_it() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-modes-replay");
    tendr(&root)
        .args(["start", "pty-modes-replay", "--pty", "--stdin", "--"])
        .args([
            "sh",
            "-c",
            r#"stty raw -echo; printf '\033[?1049h\033[?2004h\033[?25l'; echo set; exec cat"#,
        ])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-modes-replay");
    wait_log_contains(&root, "pty-modes-replay", "set");

    let mut wrapped = WrappedAttach::spawn(&root, "modes-replay", &["pty-modes-replay"]);
    wrapped
        .cli
        .type_until_logged(&root, "pty-modes-replay", "typed\n");
    wrapped.cli.type_bytes(b"\x1cd");
    assert_eq!(wrapped.wait_exit(Duration::from_secs(10)), 0);

    // Keystrokes typed before `attach` entered raw mode are echoed by the
    // terminal itself, so the session's output starts at the first escape.
    let output = wrapped.cli.output().replace('\x1b', "^[");
    assert!(
        from_first_escape(&output).starts_with("^[[?1049h^[[?2004h^[[?25l"),
        "the modes set before the attach come first; terminal saw: {output:?}"
    );
    assert!(
        output.ends_with("^[[?1049l^[[?2004l^[[?25h"),
        "and are turned off when it leaves; terminal saw: {output:?}"
    );
}

/// What a terminal showed from its first escape (written `^[`) on.
fn from_first_escape(output: &str) -> &str {
    output.find("^[").map_or("", |i| &output[i..])
}

#[test]
fn a_takeover_replays_the_modes_and_the_old_client_turns_them_off() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-modes-take");
    tendr(&root)
        .args(["start", "pty-modes-take", "--pty", "--stdin", "--"])
        .args([
            "sh",
            "-c",
            r#"stty raw -echo; printf '\033[?1049h\033[?1003h'; echo set; exec cat"#,
        ])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-modes-take");
    wait_log_contains(&root, "pty-modes-take", "set");

    let first = WrappedAttach::spawn(&root, "modes-take-1", &["pty-modes-take"]);
    wait_for_pty_control(&root, "pty-modes-take", "HumanControl");
    let mut second = WrappedAttach::spawn(&root, "modes-take-2", &["pty-modes-take", "--takeover"]);
    assert_ne!(first.wait_exit(Duration::from_secs(10)), 0);
    second
        .cli
        .type_until_logged(&root, "pty-modes-take", "second\n");

    let replay = "^[[?1049h^[[?1003h";
    let old = first.cli.output().replace('\x1b', "^[");
    let (old_terminal, old_note) = old.split_once("tendr: ").unwrap_or((&old, ""));
    assert!(
        old_terminal.starts_with(replay) && old_terminal.ends_with("^[[?1049l^[[?1003l"),
        "the retired client got the modes and turned them off before its note; it saw: {old:?}"
    );
    assert!(old_note.starts_with("another client took over"), "{old:?}");
    let new = second.cli.output().replace('\x1b', "^[");
    assert!(
        from_first_escape(&new).starts_with(replay),
        "the client that took over gets the modes first; it saw: {new:?}"
    );
    second.cli.type_bytes(b"\x1cd");
    assert_eq!(second.wait_exit(Duration::from_secs(10)), 0);
}

#[test]
fn nothing_from_the_session_follows_the_reset() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-modes-flood");
    tendr(&root)
        .args(["start", "pty-modes-flood", "--pty", "--stdin", "--"])
        .args([
            "sh",
            "-c",
            r#"stty raw -echo; printf '\033[?1003h'; while :; do printf flood-flood-flood-flood; done"#,
        ])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-modes-flood");

    let mut wrapped = WrappedAttach::spawn(&root, "modes-flood", &["pty-modes-flood"]);
    wrapped.wait_output_contains("flood");
    wrapped.cli.type_bytes(b"\x1cd");
    assert_eq!(wrapped.wait_exit(Duration::from_secs(10)), 0);

    let output = wrapped.cli.output().replace('\x1b', "^[");
    assert!(
        output.ends_with("^[[?1003l"),
        "the reset is written, and is the last thing written; the terminal's tail: {:?}",
        &output[output.len().saturating_sub(80)..]
    );
}

/// A PTY child that shows control characters: `Ctrl-\` prints as `^\`.
fn start_visible_cat(root: &TempDir, session: &str) {
    tendr(root)
        .args(["start", session, "--pty", "--stdin", "--"])
        .args(["sh", "-c", "stty raw -echo; exec cat -v"])
        .output()
        .unwrap();
    harness::wait_running(root, session);
}

#[test]
fn cli_doubled_escape_and_other_keys_reach_the_session_unchanged() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-literal");
    start_visible_cat(&root, "pty-literal");

    let mut wrapped = WrappedAttach::spawn(&root, "literal", &["pty-literal"]);
    wrapped
        .cli
        .type_until_logged(&root, "pty-literal", "literal-ready\n");
    // Doubled prefix → one literal; prefix + other key → both; plain keys as typed.
    wrapped.cli.type_bytes(b"A\x1c\x1cB\x1cxC\n");
    wait_log_contains(&root, "pty-literal", "A^\\B^\\xC");
    assert!(!wrapped.exited(), "no detach was requested");
}

#[test]
fn cli_escape_none_forwards_the_detach_sequence() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-noescape");
    start_visible_cat(&root, "pty-noescape");

    let mut wrapped =
        WrappedAttach::spawn(&root, "noescape", &["pty-noescape", "--escape", "none"]);
    wrapped
        .cli
        .type_until_logged(&root, "pty-noescape", "none-ready\n");
    wrapped.cli.type_bytes(b"N\x1cdM\n");
    wait_log_contains(&root, "pty-noescape", "N^\\dM");
    assert!(!wrapped.exited(), "--escape none must not detach");
}

#[test]
fn cli_prefix_held_across_a_read_boundary_then_detaches() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-split-prefix");
    start_visible_cat(&root, "pty-split-prefix");

    let mut wrapped = WrappedAttach::spawn(&root, "split-prefix", &["pty-split-prefix"]);
    wrapped
        .cli
        .type_until_logged(&root, "pty-split-prefix", "split-ready\n");

    // A single write is delivered to one `read` on both Linux and macOS, so
    // this lands the prefix in the same read as a marker. Waiting for the
    // marker in the log proves that read has already been processed — and
    // that the trailing prefix was held, not forwarded, since an unheld
    // prefix would show up as `^\` right after it. Only then is `d` sent, in
    // a second, later read the parser must still be holding the prefix for.
    wrapped.cli.type_bytes(b"split-marker\x1c");
    wait_output_contains(&root, "pty-split-prefix", "split-marker");
    wrapped.cli.type_bytes(b"d");

    assert_eq!(wrapped.wait_exit(Duration::from_secs(10)), 0);
    wrapped.assert_terminal_restored();

    wait_for_pty_control(&root, "pty-split-prefix", "AgentControl");
    push(&root, "pty-split-prefix", b"after-split\n");
    let after = wait_output_contains(&root, "pty-split-prefix", "after-split");
    assert_eq!(
        // `type_until_logged` retypes `split-ready` every 200 ms until it
        // sees its own marker, so an extra copy can land before this point;
        // `trim_start_matches` strips every leading copy, not just one.
        after.trim_start_matches("split-ready"),
        "split-markerafter-split",
        "the held prefix and the detach key must not reach the child"
    );
}

#[test]
fn cli_paste_with_embedded_prefixes_arrives_intact() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-paste");
    start_visible_cat(&root, "pty-paste");

    let mut cli = CliAttach::spawn(&root, &["pty-paste"]);
    cli.type_until_logged(&root, "pty-paste", "paste-ready\n");

    // One write, comfortably over the CLI's 4 KiB read buffer — the OS
    // splits it into several smaller reads regardless (each capped near
    // 1 KiB on macOS), so no single embedded sequence is placed to straddle
    // a particular one. What this proves is that a doubled prefix, a lone
    // Ctrl-], and a prefix followed by an ordinary key, each sitting inside
    // plain text as a real paste would deliver them, all survive that whole
    // read-then-parse pipeline intact regardless of where the splits land.
    let a = "A".repeat(2000);
    let b = "B".repeat(2000);
    let c = "C".repeat(2000);
    let mut paste = Vec::new();
    paste.extend_from_slice(b"PASTE-START");
    paste.extend_from_slice(a.as_bytes());
    paste.extend_from_slice(b"\x1c\x1c"); // doubled prefix -> one literal Ctrl-\
    paste.extend_from_slice(b.as_bytes());
    paste.push(0x1d); // lone Ctrl-] -> passes through unchanged
    paste.extend_from_slice(b"\x1cx"); // prefix + other key -> both forwarded
    paste.extend_from_slice(c.as_bytes());
    paste.extend_from_slice(b"PASTE-END\n");
    assert!(
        paste.len() > 4096,
        "paste must exceed the CLI's 4 KiB read buffer"
    );

    cli.type_bytes(&paste);

    let expected = format!("PASTE-START{a}^\\{b}^]^\\x{c}PASTE-END");
    wait_output_contains(&root, "pty-paste", &expected);
}

#[test]
fn cli_bracketed_paste_with_the_escape_inside_does_not_detach() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-bpaste");
    // The child asks for bracketed paste, as shells and Claude Code do.
    tendr(&root)
        .args(["start", "pty-bpaste", "--pty", "--stdin", "--"])
        .args([
            "sh",
            "-c",
            r"stty raw -echo; printf '\033[?2004h'; exec cat -v",
        ])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-bpaste");

    let mut cli = CliAttach::spawn(&root, &["pty-bpaste"]);
    cli.type_until_logged(&root, "pty-bpaste", "bpaste-ready\n");
    // The client has seen the child turn bracketed paste on.
    poll_until(Duration::from_secs(10), Duration::from_millis(20), || {
        if cli.output().contains("\x1b[?2004h") {
            Observation::Ready(())
        } else {
            Observation::Pending("no ?2004h yet".to_owned())
        }
    })
    .unwrap();

    // Over the CLI's 4 KiB read buffer, so the paste spans several reads.
    let a = "A".repeat(3000);
    let b = "B".repeat(3000);
    let mut paste = b"\x1b[200~".to_vec();
    paste.extend_from_slice(a.as_bytes());
    paste.extend_from_slice(b"\x1cd");
    paste.extend_from_slice(b.as_bytes());
    paste.extend_from_slice(b"\x1b[201~BPASTE-END\n");
    cli.type_bytes(&paste);

    let expected = format!("^[[200~{a}^\\d{b}^[[201~BPASTE-END");
    wait_output_contains(&root, "pty-bpaste", &expected);
    wait_for_pty_control(&root, "pty-bpaste", "HumanControl");
}

/// Detach with a `Ctrl-\` encoded as a child's keyboard mode makes the
/// terminal send it: send `prefix` in one read after a marker, wait until the
/// marker reached the child (so the prefix was held, not forwarded), then send
/// `d` in a later read. Afterwards the child must have received neither.
fn assert_encoded_prefix_detaches(label: &str, prefix: &[u8]) {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let session = format!("pty-{label}");
    let _kill = harness::SessionGuard::new(&root, &session);
    start_visible_cat(&root, &session);

    let mut wrapped = WrappedAttach::spawn(&root, label, &[&session]);
    wrapped
        .cli
        .type_until_logged(&root, &session, &format!("{label}-ready\n"));
    let marker = format!("{label}-marker");
    let mut typed = marker.clone().into_bytes();
    typed.extend_from_slice(prefix);
    wrapped.cli.type_bytes(&typed);
    wait_output_contains(&root, &session, &marker);
    wrapped.cli.type_bytes(b"d");

    assert_eq!(wrapped.wait_exit(Duration::from_secs(10)), 0);
    wrapped.assert_terminal_restored();

    wait_for_pty_control(&root, &session, "AgentControl");
    push(&root, &session, b"after-detach\n");
    let after = wait_output_contains(&root, &session, "after-detach");
    assert_eq!(
        after.trim_start_matches(&format!("{label}-ready")),
        format!("{marker}after-detach"),
        "neither the encoded prefix nor the detach key may reach the child"
    );
}

#[test]
fn cli_kitty_encoded_prefix_then_d_detaches() {
    assert_encoded_prefix_detaches("kitty-escape", b"\x1b[92;5u");
}

#[test]
fn cli_modify_other_keys_encoded_prefix_then_d_detaches() {
    assert_encoded_prefix_detaches("mok-escape", b"\x1b[27;5;92~");
}

#[test]
fn cli_doubled_kitty_prefix_forwards_one_in_the_same_encoding() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-kitty-literal");
    start_visible_cat(&root, "pty-kitty-literal");

    let mut wrapped = WrappedAttach::spawn(&root, "kitty-literal", &["pty-kitty-literal"]);
    wrapped
        .cli
        .type_until_logged(&root, "pty-kitty-literal", "kitty-literal-ready\n");
    wrapped.cli.type_bytes(b"A\x1b[92;5u\x1b[92;5uB");
    let output = wait_output_contains(&root, "pty-kitty-literal", "A^[[92;5uB");
    assert_eq!(
        output.matches("^[[92;5u").count(),
        1,
        "exactly one Ctrl-\\ reaches the child; got: {output}"
    );
    assert!(!wrapped.exited(), "no detach was requested");
}

#[test]
fn cli_lone_esc_reaches_the_child_without_another_key() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-lone-esc");
    start_visible_cat(&root, "pty-lone-esc");

    let mut wrapped = WrappedAttach::spawn(&root, "lone-esc", &["pty-lone-esc"]);
    wrapped
        .cli
        .type_until_logged(&root, "pty-lone-esc", "lone-esc-ready\n");
    // A read ending in ESC could be the start of an encoded Ctrl-\, so it is
    // held briefly; with nothing typed after it, it must still arrive.
    wrapped.cli.type_bytes(b"esc-marker\x1b");
    wait_output_contains(&root, "pty-lone-esc", "esc-marker^[");
    assert!(!wrapped.exited(), "no detach was requested");
}

#[test]
fn cli_forwards_later_terminal_resizes() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-winch");
    tendr(&root)
        .args(["start", "pty-winch", "--pty", "--stdin", "--", "sh"])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-winch");

    let mut cli = CliAttach::spawn(&root, &["pty-winch"]);
    cli.type_until_logged(&root, "pty-winch", "echo winch-ready\n");

    // Resized after the attach started: only continuous forwarding delivers it.
    cli.resize(33, 111);
    poll_until(Duration::from_secs(15), Duration::from_millis(200), || {
        cli.type_bytes(b"stty size\n");
        let log = tendr(&root)
            .args(["log", "pty-winch", "--raw"])
            .output()
            .unwrap();
        if String::from_utf8_lossy(&log.stdout).contains("33 111") {
            Observation::Ready(())
        } else {
            Observation::Pending("log lacks 33 111".into())
        }
    })
    .unwrap_or_else(|e| {
        panic!(
            "the session never saw the new size ({e}); cli output: {}",
            cli.output()
        )
    });
}

// --- Attach client signals: cancellation, and SIGWINCH as an early size check ---

/// A signal-ended attach: the terminal restored, exit 1 with the signal named,
/// control back with the agent, and the session still running.
fn assert_signal_ends_the_attach(signal: libc::c_int, name: &str) {
    let root = TempDir::new().unwrap();
    let session = format!("pty-{}", name.to_ascii_lowercase());
    let _kill = harness::SessionGuard::new(&root, &session);
    start_cat(&root, &session);

    let mut wrapped = WrappedAttach::spawn(&root, name, &[&session]);
    wrapped
        .cli
        .type_until_logged(&root, &session, "before-signal\n");
    wait_for_pty_control(&root, &session, "HumanControl");
    wrapped.signal(signal);

    assert_eq!(
        wrapped.wait_exit(Duration::from_secs(10)),
        1,
        "cli output: {}",
        wrapped.cli.output()
    );
    wrapped.assert_terminal_restored();
    wrapped.wait_output_contains(&format!("tendr: attach ended by {name}"));
    wait_for_pty_control(&root, &session, "AgentControl");
    push(&root, &session, b"session-still-running\n");
    wait_log_contains(&root, &session, "session-still-running");
}

#[test]
fn cli_sigterm_restores_the_terminal_and_releases_control() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    assert_signal_ends_the_attach(libc::SIGTERM, "SIGTERM");
}

#[test]
fn cli_sighup_detaches_cleanly() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    assert_signal_ends_the_attach(libc::SIGHUP, "SIGHUP");
}

/// SIGTERM is how a user gets out of an attach whose terminal has stopped
/// reading, so leaving must not wait on that terminal to report it.
#[test]
fn cli_sigterm_exits_while_terminal_output_is_blocked() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-termblock");
    tendr(&root)
        .args(["start", "pty-termblock", "--pty", "--stdin", "--"])
        .args(["sh", "-c", "stty raw -echo; yes OUTPUT-FLOOD"])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-termblock");

    let wrapped = WrappedAttach::spawn(&root, "termblock", &["pty-termblock"]);
    let deadline = Instant::now() + Duration::from_secs(15);
    while !wrapped.cli.output().contains("OUTPUT-FLOOD") {
        assert!(Instant::now() < deadline, "attach never relayed output");
        std::thread::sleep(Duration::from_millis(20));
    }

    // As in `cli_detach_is_responsive_while_terminal_output_is_blocked`: the
    // blocked state cannot be observed directly, so give the flood a moment.
    wrapped.cli.pause_output();
    std::thread::sleep(Duration::from_millis(500));
    wrapped.signal(libc::SIGTERM);

    let code = wrapped.wait_exit(Duration::from_secs(15));
    wrapped.cli.resume_output();
    assert_eq!(code, 1);
    wrapped.assert_terminal_restored();
    wait_for_pty_control(&root, "pty-termblock", "AgentControl");
}

/// With the size poll stretched far beyond the test's deadline, only SIGWINCH
/// can forward the new size in time. Keystrokes do not trigger a size check,
/// so typing `stty size` cannot deliver it either.
#[test]
fn cli_sigwinch_forwards_the_size_before_the_next_poll() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-sigwinch");
    tendr(&root)
        .args(["start", "pty-sigwinch", "--pty", "--stdin", "--", "sh"])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-sigwinch");

    let argv = [tendr_bin(), "attach".to_owned(), "pty-sigwinch".to_owned()];
    let mut cli = CliAttach::spawn_argv_env(
        &root,
        &argv,
        &[("TENDR_TEST_ATTACH_SIZE_POLL_MS", "600000")],
    );
    cli.type_until_logged(&root, "pty-sigwinch", "echo sigwinch-ready\n");

    cli.resize(37, 97);
    poll_until(Duration::from_secs(15), Duration::from_millis(200), || {
        cli.type_bytes(b"stty size\n");
        let log = tendr(&root)
            .args(["log", "pty-sigwinch", "--raw"])
            .output()
            .unwrap();
        if String::from_utf8_lossy(&log.stdout).contains("37 97") {
            Observation::Ready(())
        } else {
            Observation::Pending("log lacks 37 97".into())
        }
    })
    .unwrap_or_else(|e| {
        panic!(
            "the session never saw the new size ({e}); cli output: {}",
            cli.output()
        )
    });
}

#[test]
fn cli_takeover_by_another_client_restores_the_terminal() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-taken");
    let sock_path = start_cat(&root, "pty-taken");

    let mut wrapped = WrappedAttach::spawn(&root, "taken", &["pty-taken"]);
    wrapped
        .cli
        .type_until_logged(&root, "pty-taken", "before-takeover\n");
    let (_other, (msg_type, _)) = hello(&sock_path, Mode::Takeover);
    assert_eq!(msg_type, MSG_ACCEPTED);

    wrapped.wait_exit(Duration::from_secs(10));
    wrapped.assert_terminal_restored();
    assert!(
        wrapped.cli.output().contains("took over"),
        "the user is told why the attach ended: {}",
        wrapped.cli.output()
    );
}

#[test]
fn cli_session_end_restores_the_terminal() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-ended");
    start_cat(&root, "pty-ended");

    let mut wrapped = WrappedAttach::spawn(&root, "ended", &["pty-ended"]);
    wrapped
        .cli
        .type_until_logged(&root, "pty-ended", "before-end\n");
    tendr(&root)
        .args(["kill", "pty-ended", "--force"])
        .output()
        .unwrap();

    wrapped.wait_exit(Duration::from_secs(10));
    wrapped.assert_terminal_restored();
}

#[test]
fn cli_detach_is_responsive_while_session_input_is_blocked() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-inblock");
    // The child never reads its input, so everything typed backs up.
    tendr(&root)
        .args(["start", "pty-inblock", "--pty", "--stdin", "--"])
        .args([
            "sh",
            "-c",
            "stty raw -echo; while true; do printf READY; sleep 0.2; done",
        ])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-inblock");

    let mut wrapped = WrappedAttach::spawn(&root, "inblock", &["pty-inblock"]);
    // Relayed output proves the CLI is in raw mode, so Ctrl-\ is not SIGQUIT.
    let deadline = Instant::now() + Duration::from_secs(15);
    while !wrapped.cli.output().contains("READY") {
        assert!(Instant::now() < deadline, "attach never relayed output");
        std::thread::sleep(Duration::from_millis(20));
    }

    let mut keyboard = wrapped.cli.take_input();
    let typist = std::thread::spawn(move || {
        let _ = keyboard.write_all(&vec![b'x'; 256 * 1024]);
        let _ = keyboard.write_all(b"\x1cd");
        keyboard
    });

    assert_eq!(wrapped.wait_exit(Duration::from_secs(15)), 0);
    wrapped.assert_terminal_restored();
    wait_for_pty_control(&root, "pty-inblock", "AgentControl");
    drop(typist.join());
}

#[test]
fn cli_detach_is_responsive_while_terminal_output_is_blocked() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-outblock");
    tendr(&root)
        .args(["start", "pty-outblock", "--pty", "--stdin", "--"])
        .args(["sh", "-c", "stty raw -echo; yes OUTPUT-FLOOD"])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-outblock");

    let mut wrapped = WrappedAttach::spawn(&root, "outblock", &["pty-outblock"]);
    let deadline = Instant::now() + Duration::from_secs(15);
    while !wrapped.cli.output().contains("OUTPUT-FLOOD") {
        assert!(Instant::now() < deadline, "attach never relayed output");
        std::thread::sleep(Duration::from_millis(20));
    }

    // The terminal stops reading; the flood fills its output buffer and the
    // CLI's writes to it block. That state cannot be observed directly, so give
    // the flood a moment to fill it.
    wrapped.cli.pause_output();
    std::thread::sleep(Duration::from_millis(500));
    wrapped.cli.type_bytes(b"\x1cd");

    let code = wrapped.wait_exit(Duration::from_secs(15));
    wrapped.cli.resume_output();
    assert_eq!(code, 0);
    wrapped.assert_terminal_restored();
}

#[test]
fn cli_attach_delivers_typed_input_through_the_handshake() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-cli");
    start_cat(&root, "pty-cli");

    let mut cli = CliAttach::spawn(&root, &["pty-cli"]);
    wait_for_pty_control(&root, "pty-cli", "HumanControl");
    cli.type_until_logged(&root, "pty-cli", "typed-through-cli\n");
    assert!(!cli.output().contains("error"), "cli: {}", cli.output());
}

#[test]
fn cli_attach_takeover_retires_the_current_controller() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-cli-take");
    let sock_path = start_cat(&root, "pty-cli-take");
    let mut old = attach_as_human(&sock_path);

    let mut cli = CliAttach::spawn(&root, &["pty-cli-take", "--takeover"]);
    let seen = read_until_closed(&mut old);
    assert!(
        seen.iter().any(|(t, _)| *t == MSG_RETIRED),
        "the raw controller is retired by the CLI takeover: {seen:?}; cli: {}",
        cli.output()
    );
    cli.type_until_logged(&root, "pty-cli-take", "cli-took-over\n");
}

/// Start `cat` under a PTY with `--stdin` and return its attach socket.
fn start_cat(root: &TempDir, session: &str) -> std::path::PathBuf {
    tendr(root)
        .args(["start", session, "--pty", "--stdin", "--", "cat"])
        .output()
        .unwrap();
    harness::wait_running(root, session);
    wait_for_attach_socket(root, session)
}

fn push(root: &TempDir, session: &str, bytes: &[u8]) {
    let output = tendr(root)
        .args(["push", session])
        .write_stdin(bytes.to_vec())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "push failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn connection_without_hello_gains_no_control_and_is_closed() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-nohello");
    let sock_path = start_cat(&root, "pty-nohello");

    let mut raw = UnixStream::connect(&sock_path).unwrap();
    write_msg(&mut raw, MSG_DATA, b"no-hello-marker\n");
    let seen = read_until_closed(&mut raw);
    assert!(
        seen.iter().all(|(t, _)| *t != MSG_ACCEPTED),
        "a connection without hello must never be accepted: {seen:?}"
    );

    // Causal barrier: input that arrives later through push is visible, so the
    // earlier marker would be visible too had it reached the PTY.
    push(&root, "pty-nohello", b"control-marker\n");
    let log = wait_log_contains(&root, "pty-nohello", "control-marker");
    assert!(!log.contains("no-hello-marker"), "log: {log}");

    let status = tendr(&root)
        .args(["status", "pty-nohello"])
        .output()
        .unwrap();
    let meta: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(meta["pty"]["control"], "AgentControl");
}

#[test]
fn attach_is_accepted_with_an_epoch_and_a_second_attach_is_rejected() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-epoch");
    let sock_path = start_cat(&root, "pty-epoch");

    let (mut first, (msg_type, payload)) = hello(&sock_path, Mode::Attach);
    assert_eq!(msg_type, MSG_ACCEPTED);
    assert_eq!(epoch_of(&payload), 1);
    wait_for_pty_control(&root, "pty-epoch", "HumanControl");

    let (mut second, (msg_type, payload)) = hello(&sock_path, Mode::Attach);
    assert_eq!(
        msg_type,
        MSG_REJECTED,
        "a plain attach must not supersede: {}",
        String::from_utf8_lossy(&payload)
    );
    let _ = read_until_closed(&mut second);

    // The first controller is undisturbed.
    write_msg(&mut first, MSG_DATA, b"first-still-owns\n");
    wait_log_contains(&root, "pty-epoch", "first-still-owns");

    drop(first);
}

#[test]
fn takeover_retires_the_previous_human_and_rejects_its_later_input() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-takeover");
    let sock_path = start_cat(&root, "pty-takeover");

    let mut old = attach_as_human(&sock_path);
    let (mut new, (msg_type, payload)) = hello(&sock_path, Mode::Takeover);
    assert_eq!(msg_type, MSG_ACCEPTED);
    assert_eq!(epoch_of(&payload), 2);

    let seen = read_until_closed(&mut old);
    let retired: Vec<_> = seen.iter().filter(|(t, _)| *t == MSG_RETIRED).collect();
    assert_eq!(retired.len(), 1, "old controller is told it was retired");
    assert_eq!(epoch_of(&retired[0].1), 2);

    // The retired connection is shut down; anything it still manages to send is
    // rejected. Its write may fail outright, which is equally correct.
    let _ = old.write_all(&[MSG_DATA, 0, 0, 0, 13]);
    let _ = old.write_all(b"old-after-rt\n");

    write_msg(&mut new, MSG_DATA, b"new-owns-now\n");
    let log = wait_log_contains(&root, "pty-takeover", "new-owns-now");
    assert!(!log.contains("old-after-rt"), "log: {log}");

    // Control changes reach events.jsonl through the run's effects thread,
    // not the output path, so the new owner's output can be logged first.
    harness::poll_until(Duration::from_secs(10), Duration::from_millis(10), || {
        let takeover = harness::read_events(&root, "pty-takeover")
            .into_iter()
            .find(|e| e["kind"] == "pty.control_changed" && e["data"]["trigger"] == "takeover");
        match takeover {
            Some(event) => harness::Observation::Ready(event),
            None => harness::Observation::Pending("no takeover control_changed event yet".into()),
        }
    })
    .expect("takeover is recorded as a fact");

    drop(new);
}

#[test]
fn takeover_revokes_queued_agent_input() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-revoke");
    let go = root.path().join("go");

    // The child consumes exactly 1 KiB of agent input, reports READY, then stops
    // reading until the go file exists. Everything the agent pushes after that
    // stays queued behind a full PTY input buffer.
    let script = "stty raw -echo; head -c 1024 >/dev/null; printf READY; \
                  while [ ! -f \"$GO\" ]; do sleep 0.05; done; exec cat";
    tendr(&root)
        .args(["start", "pty-revoke", "--pty", "--stdin"])
        .arg("--env")
        .arg(format!("GO={}", go.display()))
        .args(["--", "sh", "-c", script])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-revoke");
    let sock_path = wait_for_attach_socket(&root, "pty-revoke");

    let mut payload = vec![b'a'; 2 << 20];
    payload.extend_from_slice(b"AGENT-TAIL\n");
    let mut agent = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args(["push", "pty-revoke"])
        .env("HOME", root.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut agent_stdin = agent.stdin.take().unwrap();
    let feeder = std::thread::spawn(move || {
        // The write fails with a broken pipe if the sidecar stops reading; either
        // outcome is fine for the feeder.
        let _ = agent_stdin.write_all(&payload);
    });

    // Barrier: agent input is flowing and the rest of it is queued.
    wait_log_contains(&root, "pty-revoke", "READY");
    assert!(
        agent.try_wait().unwrap().is_none(),
        "setup invariant: the agent push must still be in flight at takeover"
    );

    let (mut human, (msg_type, _)) = hello(&sock_path, Mode::Takeover);
    assert_eq!(msg_type, MSG_ACCEPTED);
    write_msg(&mut human, MSG_DATA, b"HUMAN-LINE\n");
    std::fs::write(&go, b"").unwrap();

    // The human's line can reach the PTY in pieces as the child drains the
    // agent's buffered prefix, so it may straddle two captured chunks.
    let log = wait_output_contains(&root, "pty-revoke", "HUMAN-LINE");
    assert!(
        !log.contains("AGENT-TAIL"),
        "queued agent input was written after takeover"
    );
    let a_count = log.bytes().filter(|&b| b == b'a').count();
    assert!(
        a_count < 1 << 20,
        "at most the kernel-buffered prefix may precede the takeover; saw {a_count} bytes"
    );
    let revoked = harness::wait_event_kind(&root, "pty-revoke", "pty.input_revoked");
    assert_eq!(revoked["data"]["kind"], "Agent");

    // The revoked push itself reports it: no false success.
    drop(human);
    feeder.join().unwrap();
    let pushed = agent.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&pushed.stderr);
    assert!(
        !pushed.status.success(),
        "a push revoked by takeover must not exit 0; stderr: {stderr}"
    );
    assert!(
        stderr.contains("revoked"),
        "the failure says the push was revoked: {stderr}"
    );
    tendr(&root)
        .args(["kill", "pty-revoke", "--force"])
        .output()
        .ok();
}

#[test]
fn takeover_after_a_silent_connection_reaches_the_same_running_process() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-reconnect");

    tendr(&root)
        .args(["start", "pty-reconnect", "--pty", "--stdin", "--", "sh"])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-reconnect");
    let sock_path = wait_for_attach_socket(&root, "pty-reconnect");
    let status = tendr(&root)
        .args(["status", "pty-reconnect"])
        .output()
        .unwrap();
    let meta: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    let child_pid = meta["child"]["pid"].as_u64().unwrap();

    // A controller that goes silent without closing, like a half-open SSH session.
    let _silent = attach_as_human(&sock_path);

    let (mut new, (msg_type, _)) = hello(&sock_path, Mode::Takeover);
    assert_eq!(msg_type, MSG_ACCEPTED);
    write_msg(&mut new, MSG_DATA, b"echo \"pid=\"$$\n");

    let expected = format!("pid={child_pid}");
    wait_log_contains(&root, "pty-reconnect", &expected);

    drop(new);
    tendr(&root)
        .args(["kill", "pty-reconnect", "--force"])
        .output()
        .ok();
}

#[test]
fn stalled_viewer_cannot_block_output_capture() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-stalled");
    let go = root.path().join("go");

    // After the go file appears, the child writes ~10 MiB — more than one
    // viewer's queue budget — then a marker, then stays alive.
    let script = "while [ ! -f \"$GO\" ]; do sleep 0.05; done; \
                  yes 0123456789abcdef0123456789abcdef | head -c 10485760; \
                  echo; echo ALL-OUTPUT-DONE; sleep 60";
    tendr(&root)
        .args(["start", "pty-stalled", "--pty"])
        .arg("--env")
        .arg(format!("GO={}", go.display()))
        .args(["--", "sh", "-c", script])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-stalled");
    let sock_path = wait_for_attach_socket(&root, "pty-stalled");

    // A viewer that attaches and then never reads.
    let mut stalled = attach_as_human(&sock_path);
    std::fs::write(&go, b"").unwrap();

    // Capture must keep up regardless of the stalled viewer. Read output.log
    // directly: `tendr log` would re-parse ~10 MiB on every poll.
    let log_path = root
        .path()
        .join(".tendr/sessions/default/pty-stalled/output.log");
    poll_until(Duration::from_secs(60), Duration::from_millis(200), || {
        let log = std::fs::read(&log_path).unwrap_or_default();
        if log.windows(15).any(|w| w == b"ALL-OUTPUT-DONE") {
            Observation::Ready(())
        } else {
            Observation::Pending(format!("{} log bytes", log.len()))
        }
    })
    .unwrap_or_else(|e| panic!("output capture stalled behind a viewer that never reads: {e}"));

    // The stalled viewer was disconnected rather than buffered without bound,
    // and with it went its control.
    let _ = read_until_closed(&mut stalled);
    wait_for_pty_control(&root, "pty-stalled", "AgentControl");
}

#[test]
fn a_second_push_is_refused_while_one_holds_the_terminal() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-twopush");
    let go = root.path().join("go");
    // The child consumes exactly 1 KiB of the first push, reports READY, then
    // reads nothing until the go file exists: the first push stays in flight.
    let script = "stty raw -echo; head -c 1024 >/dev/null; printf READY; \
                  while [ ! -f \"$GO\" ]; do sleep 0.05; done; exec cat";
    tendr(&root)
        .args(["start", "pty-twopush", "--pty", "--stdin"])
        .arg("--env")
        .arg(format!("GO={}", go.display()))
        .args(["--", "sh", "-c", script])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-twopush");

    let mut first = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args(["push", "pty-twopush"])
        .env("HOME", root.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut first_stdin = first.stdin.take().unwrap();
    let feeder = std::thread::spawn(move || {
        let _ = first_stdin.write_all(&vec![b'a'; 1 << 20]);
    });
    // Barrier: the first push holds the terminal and its input is flowing.
    wait_log_contains(&root, "pty-twopush", "READY");

    let second = tendr(&root)
        .args(["push", "pty-twopush"])
        .write_stdin(b"SECOND-PUSH\n".to_vec())
        .timeout(Duration::from_secs(20))
        .output()
        .expect("a second push must return promptly, not block behind the first");
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        !second.status.success(),
        "a concurrent push must be refused, not interleaved: {stderr}"
    );
    assert!(
        stderr.contains("owned"),
        "the refusal names the owner: {stderr}"
    );

    std::fs::write(&go, b"").unwrap();
    feeder.join().unwrap();
    let first = first.wait_with_output().unwrap();
    assert!(
        first.status.success(),
        "the first push completes: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let log = wait_log_contains(&root, "pty-twopush", "aaaa");
    assert!(!log.contains("SECOND-PUSH"), "refused bytes were written");
}

#[test]
fn attach_socket_is_private_and_under_the_state_root() {
    use std::os::unix::fs::PermissionsExt;
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-private");
    let sock_path = start_cat(&root, "pty-private");

    let sockets = root.path().join(".tendr").join("sockets");
    assert_eq!(
        sock_path
            .parent()
            .map(std::fs::canonicalize)
            .transpose()
            .unwrap(),
        Some(std::fs::canonicalize(&sockets).unwrap()),
        "socket lives in the state root's private directory, not shared temp"
    );
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&sockets), 0o700);
    assert_eq!(mode(&sock_path), 0o600);
}

#[test]
fn oversized_attach_frame_closes_the_connection_and_releases_control() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-bigframe");
    let sock_path = start_cat(&root, "pty-bigframe");

    let mut human = attach_as_human(&sock_path);
    wait_for_pty_control(&root, "pty-bigframe", "HumanControl");
    // Declare a 1 GiB payload; the sidecar must not wait for or allocate it.
    human
        .write_all(&[MSG_DATA, 0x40, 0x00, 0x00, 0x00])
        .unwrap();
    let _ = read_until_closed(&mut human);
    wait_for_pty_control(&root, "pty-bigframe", "AgentControl");

    // The session is unaffected.
    push(&root, "pty-bigframe", b"still-running\n");
    wait_log_contains(&root, "pty-bigframe", "still-running");
}

#[test]
fn a_trickled_hello_is_cut_off_at_the_overall_deadline() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-slowhello");
    let sock_path = start_cat(&root, "pty-slowhello");

    // A well-formed hello sent one byte per second: every individual read is
    // quick, but the whole handshake takes 7 s, past the 5 s deadline.
    let mut slow = UnixStream::connect(&sock_path).unwrap();
    let hello = [MSG_HELLO, 0, 0, 0, 2, PROTOCOL_VERSION, Mode::Attach as u8];
    let started = Instant::now();
    for byte in hello {
        if slow.write_all(&[byte]).is_err() {
            break; // already closed by the sidecar
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    let seen = read_until_closed(&mut slow);
    assert!(
        seen.iter().all(|(t, _)| *t != MSG_ACCEPTED),
        "a hello past its deadline must not be accepted: {seen:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(12),
        "closed promptly after the deadline"
    );
    let status = tendr(&root)
        .args(["status", "pty-slowhello"])
        .output()
        .unwrap();
    let meta: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(meta["pty"]["control"], "AgentControl");
}

#[test]
fn an_unsafe_socket_directory_fails_start_loudly() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-unsafe");
    let elsewhere = root.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::create_dir_all(root.path().join(".tendr")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, root.path().join(".tendr/sockets")).unwrap();

    let output = tendr(&root)
        .args(["start", "pty-unsafe", "--pty", "--stdin", "--", "cat"])
        .output()
        .unwrap();
    let meta = harness::wait_terminal(&root, "pty-unsafe");
    assert_eq!(
        meta["status"],
        "SpawnFailed",
        "a PTY session must not run without its private listener; start said: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let warnings = meta["warnings"].to_string();
    assert!(
        warnings.contains("socket"),
        "the failure names the socket: {warnings}"
    );
    assert!(
        std::fs::read_dir(&elsewhere).unwrap().next().is_none(),
        "nothing was created through the symlink"
    );
    // Nothing was spawned: the evidence names the socket, not a spawn syscall.
    let evidence = meta["transition_provenance"]["evidence"].to_string();
    assert!(
        evidence.contains("attach_socket_failed") && !evidence.contains("spawn_failed_syscall"),
        "evidence: {evidence}"
    );
}

#[test]
fn detach_releases_control_even_when_pty_input_is_full() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-full-detach");
    // The child never reads its input, so the PTY input buffer fills.
    tendr(&root)
        .args(["start", "pty-full-detach", "--pty", "--stdin", "--"])
        .args(["sh", "-c", "stty raw -echo; printf READY; sleep 60"])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-full-detach");
    wait_log_contains(&root, "pty-full-detach", "READY");
    let sock_path = wait_for_attach_socket(&root, "pty-full-detach");

    let mut human = attach_as_human(&sock_path);
    human
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    for _ in 0..16 {
        write_msg(&mut human, MSG_DATA, &vec![b'x'; 65536]);
    }
    write_msg(&mut human, MSG_DETACH, &[]);
    let _ = read_until_closed(&mut human);

    // The detached client must not keep control behind its unwritable input.
    wait_for_pty_control(&root, "pty-full-detach", "AgentControl");
    let (_next, (msg_type, reason)) = hello(&sock_path, Mode::Attach);
    assert_eq!(
        msg_type,
        MSG_ACCEPTED,
        "a detached client still holds control: {}",
        String::from_utf8_lossy(&reason)
    );
}

#[test]
fn a_disconnected_push_releases_control_behind_a_full_pty() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-push-gone");
    tendr(&root)
        .args(["start", "pty-push-gone", "--pty", "--stdin", "--"])
        .args([
            "sh",
            "-c",
            "stty raw -echo; head -c 1024 >/dev/null; printf READY; sleep 60",
        ])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-push-gone");
    let sock_path = wait_for_attach_socket(&root, "pty-push-gone");

    // A push client that sends more than the child will read, then vanishes
    // while the sidecar is still waiting for the PTY to accept it.
    let (mut agent, (msg_type, _)) = hello(&sock_path, Mode::Push);
    assert_eq!(msg_type, MSG_ACCEPTED);
    agent
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write_msg(&mut agent, MSG_DATA, &vec![b'x'; 65536]);
    wait_log_contains(&root, "pty-push-gone", "READY");
    agent.shutdown(std::net::Shutdown::Both).unwrap();
    drop(agent);

    poll_until(Duration::from_secs(5), Duration::from_millis(50), || {
        let (_next, (msg_type, reason)) = hello(&sock_path, Mode::Attach);
        if msg_type == MSG_ACCEPTED {
            Observation::Ready(())
        } else {
            Observation::Pending(String::from_utf8_lossy(&reason).into_owned())
        }
    })
    .unwrap_or_else(|e| panic!("a disconnected push still holds the terminal: {e}"));
}

#[test]
fn idle_pushes_retired_by_takeover_do_not_exhaust_connection_slots() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-push-slots");
    let sock_path = start_cat(&root, "pty-push-slots");

    // More rounds than there are connection slots. Each push stays connected and
    // idle; the client never closes it.
    let mut idle_pushes = Vec::new();
    for round in 0..9 {
        let (mut push, (msg_type, reason)) = hello(&sock_path, Mode::Push);
        assert_eq!(
            msg_type,
            MSG_ACCEPTED,
            "push {round}: {}",
            String::from_utf8_lossy(&reason)
        );
        let (mut human, (msg_type, reason)) = hello(&sock_path, Mode::Takeover);
        assert_eq!(
            msg_type,
            MSG_ACCEPTED,
            "takeover {round}: {}",
            String::from_utf8_lossy(&reason)
        );

        // The retired push is told, and its connection is closed server-side.
        let seen = read_until_closed(&mut push);
        let done = seen
            .iter()
            .find(|(t, _)| *t == MSG_INPUT_DONE)
            .unwrap_or_else(|| panic!("push {round} got no outcome: {seen:?}"));
        assert!(
            matches!(
                tendr::attach_proto::Frame::decode(done.0, done.1.clone()),
                Ok(tendr::attach_proto::Frame::InputDone(
                    tendr::attach_proto::PushOutcome::Revoked(_)
                ))
            ),
            "push {round}: {done:?}"
        );
        idle_pushes.push(push);

        write_msg(&mut human, MSG_DETACH, &[]);
        let _ = read_until_closed(&mut human);
        wait_for_pty_control(&root, "pty-push-slots", "AgentControl");
    }
}

/// Count entries under a `/proc/<pid>` subdirectory, or `0` if it cannot be
/// read (e.g. the process is gone).
#[cfg(target_os = "linux")]
fn proc_count(pid: u64, subdir: &str) -> usize {
    std::fs::read_dir(format!("/proc/{pid}/{subdir}"))
        .map(|entries| entries.count())
        .unwrap_or(0)
}

/// Sample the sidecar's fd and thread counts until three consecutive
/// readings, 50 ms apart, agree, bounded by a deadline. Right after
/// `wait_running` the sidecar can still be spinning up its own kill-watcher
/// and output-capture threads, so a single early reading can undercount by a
/// thread or two; settling first keeps that startup noise out of the
/// baseline this test compares against.
#[cfg(target_os = "linux")]
fn stable_proc_counts(pid: u64) -> (usize, usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last = (proc_count(pid, "fd"), proc_count(pid, "task"));
    let mut streak = 1;
    loop {
        std::thread::sleep(Duration::from_millis(50));
        let current = (proc_count(pid, "fd"), proc_count(pid, "task"));
        if current == last {
            streak += 1;
            if streak >= 3 {
                return current;
            }
        } else {
            streak = 1;
            last = current;
        }
        assert!(
            Instant::now() < deadline,
            "sidecar {pid}'s fd/thread counts never settled: last {current:?}"
        );
    }
}

#[test]
fn thirty_two_half_open_takeovers_do_not_grow_connections_or_threads() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-32-takeovers");
    let sock_path = start_cat(&root, "pty-32-takeovers");

    // Establish the first controller: a plain attach that then goes silent.
    // Its resources belong in the baseline below, since the loop also ends
    // with exactly one live holder.
    let (mut previous, (msg_type, reason)) = hello(&sock_path, Mode::Attach);
    assert_eq!(
        msg_type,
        MSG_ACCEPTED,
        "initial attach: {}",
        String::from_utf8_lossy(&reason)
    );

    #[cfg(target_os = "linux")]
    let sidecar_pid = session_meta(&root, "pty-32-takeovers")["sidecar"]["pid"]
        .as_u64()
        .expect("meta.json has sidecar.pid");
    #[cfg(target_os = "linux")]
    let baseline = stable_proc_counts(sidecar_pid);

    // 32 rounds: a takeover is admitted, evicting the silent holder ahead of
    // it. Neither side ever detaches, sends data, or closes cooperatively --
    // the only route from "controlling" to "gone" is the sidecar's own
    // retirement, triggered purely by the next takeover.
    let mut retired = Vec::new();
    for round in 0..32 {
        let (taker, (msg_type, reason)) = hello(&sock_path, Mode::Takeover);
        assert_eq!(
            msg_type,
            MSG_ACCEPTED,
            "takeover {round}: {}",
            String::from_utf8_lossy(&reason)
        );

        // The evicted holder is retired: told over its own connection, then
        // the sidecar closes its side. The test reads that here to confirm
        // it; the holder itself never reads, writes, or detaches on its own
        // while it holds control.
        let seen = read_until_closed(&mut previous);
        assert!(
            seen.iter().any(|(t, _)| *t == MSG_RETIRED),
            "round {round}: previous holder was not retired: {seen:?}"
        );

        // Kept, not dropped: an early client-side close must not be what
        // frees the sidecar's fd or thread for this connection.
        retired.push(previous);
        previous = taker;
    }
    // The last round's holder was never displaced by anyone. Keep it live,
    // like every retired socket, for the check below.
    retired.push(previous);

    #[cfg(target_os = "linux")]
    {
        // No fixed sleep: poll the sidecar's own fd/thread counts down to the
        // pre-loop baseline, bounded by a deadline. This runs while all 32
        // retired sockets, plus the last round's holder, are still open on
        // the client side (nothing below has dropped `retired` yet): the
        // sidecar's own retirement must be what frees its fd and thread, not
        // a client eventually closing its end.
        poll_until(Duration::from_secs(10), Duration::from_millis(50), || {
            let fds = proc_count(sidecar_pid, "fd");
            let threads = proc_count(sidecar_pid, "task");
            if fds <= baseline.0 && threads <= baseline.1 {
                Observation::Ready(())
            } else {
                Observation::Pending(format!(
                    "fds {fds} (baseline {}), threads {threads} (baseline {})",
                    baseline.0, baseline.1
                ))
            }
        })
        .unwrap_or_else(|e| {
            panic!(
                "sidecar {sidecar_pid} did not release its resources while \
                 every retired socket was still open on the client side: {e}"
            )
        });
    }

    // Only now: nothing above depended on the client side ever closing.
    drop(retired);

    // The last live controller's disconnect is served through the same
    // control queue as the next claim, and on macOS in particular it can lag
    // the fd/thread check above. Wait for it to land, as the sibling tests
    // do, before treating the PTY as unowned.
    wait_for_pty_control(&root, "pty-32-takeovers", "AgentControl");

    // Admission still works: the pool of 8 (hellos included) has room again.
    // Hold eight connections open without ever sending a hello, so none of
    // them can contend for control or be rejected for any reason but a full
    // pool, and confirm the sidecar still lets every one of them past the
    // connection-count gate. A ninth must then be turned away with exactly
    // the rejection that would show a leaked admission slot.
    let mut pending = Vec::new();
    for i in 0..8 {
        let mut conn =
            UnixStream::connect(&sock_path).unwrap_or_else(|e| panic!("connection {i}: {e}"));
        conn.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        match read_msg(&mut conn) {
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => panic!("connection {i}: unexpected error waiting on it: {e}"),
            Ok((msg_type, payload)) => panic!(
                "connection {i} was already answered before it ever sent a hello: \
                 {msg_type} {payload:?}"
            ),
        }
        pending.push(conn);
    }
    let mut ninth = UnixStream::connect(&sock_path).expect("connect a ninth, hello-less too");
    ninth
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let (msg_type, reason) = read_msg(&mut ninth).expect("the ninth gets a reply");
    let reason_text = String::from_utf8_lossy(&reason);
    assert_eq!(
        msg_type, MSG_REJECTED,
        "the ninth should find the pool full: {reason_text}"
    );
    assert!(
        reason_text.contains("too many attach connections"),
        "unexpected rejection reason for the ninth: {reason_text}"
    );
}

#[test]
fn an_old_runs_cleanup_cannot_remove_its_replacements_breadcrumb() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-replaced");
    let callback = root.path().join("callback.sh");
    let started = root.path().join("callback-started");
    let finish = root.path().join("callback-finish");
    std::fs::write(
        &callback,
        b"touch \"$1\"; i=0; while [ ! -f \"$2\" ] && [ \"$i\" -lt 400 ]; do sleep 0.05; i=$((i+1)); done\n",
    )
    .unwrap();
    let on_exit = format!(
        "sh {} {} {}",
        callback.display(),
        started.display(),
        finish.display()
    );
    let first = tendr(&root)
        .args(["start", "pty-replaced", "--pty", "--stdin", "--on-exit"])
        .arg(&on_exit)
        .args(["--", "cat"])
        .output()
        .unwrap();
    let old_run: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    let old_run_id = old_run["run_id"].as_str().unwrap().to_owned();
    harness::wait_running(&root, "pty-replaced");
    let old_socket = wait_for_attach_socket(&root, "pty-replaced");

    // End the old run; its exit callback runs after the session lock is released.
    tendr(&root)
        .args(["kill", "pty-replaced", "--force"])
        .output()
        .unwrap();
    harness::poll_path_exists(&started, Duration::from_secs(10), Duration::from_millis(20))
        .unwrap_or_else(|e| panic!("exit callback never started: {e}"));

    // A replacement starts while the old sidecar is still in its callback.
    tendr(&root)
        .args([
            "start",
            "pty-replaced",
            "--replace",
            "--pty",
            "--stdin",
            "--",
            "cat",
        ])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-replaced");
    let new_socket = wait_for_attach_socket(&root, "pty-replaced");
    assert_ne!(old_socket, new_socket);

    // Let the old sidecar finish: its callback record is written afterwards.
    std::fs::write(&finish, b"").unwrap();
    let record = root
        .path()
        .join(format!(".tendr/callbacks/{old_run_id}.json"));
    harness::poll_path_exists(&record, Duration::from_secs(10), Duration::from_millis(20))
        .unwrap_or_else(|e| panic!("old sidecar never finished: {e}"));

    let session = root.path().join(".tendr/sessions/default/pty-replaced");
    assert!(!old_socket.exists(), "the old run's socket is gone");
    assert_eq!(
        tendr::attach_proto::read_sock_path(&session),
        Some(new_socket),
        "the old run's cleanup removed the replacement's breadcrumb"
    );
}

// --- Cloud PTY plan: exact recording ---

fn session_meta(root: &TempDir, session: &str) -> serde_json::Value {
    let path = root
        .path()
        .join(format!(".tendr/sessions/default/{session}/meta.json"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The run's recording directory, as named by its metadata.
fn recording_dir(root: &TempDir, session: &str) -> std::path::PathBuf {
    let meta = session_meta(root, session);
    let dir = meta["pty"]["recording"]["dir"]
        .as_str()
        .unwrap_or_else(|| panic!("no recording in metadata: {meta}"));
    assert_eq!(
        dir,
        format!("recording/{}", meta["run_id"].as_str().unwrap()),
        "one directory per run"
    );
    root.path()
        .join(format!(".tendr/sessions/default/{session}"))
        .join(dir)
}

fn decode_session_recording(root: &TempDir, session: &str) -> tendr::recording::DecodedRecording {
    let dir = recording_dir(root, session);
    let files = tendr::recorder::DirectoryStore::segments(&dir).unwrap();
    let bytes: Vec<Vec<u8>> = files.iter().map(|f| std::fs::read(f).unwrap()).collect();
    tendr::recording::decode_recording(bytes.iter().map(Vec::as_slice)).expect("recording decodes")
}

fn recorded_output(records: &[tendr::recording::Record]) -> Vec<u8> {
    records
        .iter()
        .filter_map(|r| match &r.kind {
            tendr::recording::RecordKind::Output(bytes) => Some(bytes.as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn pty_output_is_recorded_exactly_with_the_initial_geometry() {
    use std::os::unix::fs::PermissionsExt;
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Escape sequences, bytes that are not UTF-8, and no trailing newline.
    // Started from a caller with `TERM=dumb`, which the child (and so the
    // recording) gets as `xterm-256color`.
    tendr(&root)
        .env("TERM", "dumb")
        .args([
            "start",
            "pty-rec",
            "--pty",
            "--",
            "sh",
            "-c",
            "stty size; printf '\\033[31mred\\377\\376tail'",
        ])
        .output()
        .unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-rec");
    harness::wait_terminal(&root, "pty-rec");

    let recording = decode_session_recording(&root, "pty-rec");
    assert_eq!(
        recorded_output(&recording.records),
        b"24 80\r\n\x1b[31mred\xff\xfetail",
        "the child starts at 24x80 and every output byte is recorded as written"
    );
    assert_eq!(recording.end, tendr::recording::SegmentEnd::Clean);

    let meta = session_meta(&root, "pty-rec");
    let header = &recording.header;
    assert_eq!(
        header.run_id.as_uuid().to_string(),
        meta["run_id"].as_str().unwrap()
    );
    assert_eq!((header.geometry.rows(), header.geometry.cols()), (24, 80));
    assert!(!header.input_recorded, "input is not recorded by default");
    assert_eq!(
        header.term.as_str(),
        "xterm-256color",
        "the recording names the TERM the child ran under"
    );

    let last = recording.records.last().unwrap().sequence.get();
    let state = &meta["pty"]["recording"];
    assert_eq!(state["state"], "Complete", "{meta}");
    assert_eq!(state["input_recorded"], false);
    assert_eq!(state["last_recorded_sequence"], last);
    assert_eq!(state["last_synced_sequence"], last);

    let dir = recording_dir(&root, "pty-rec");
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(dir.parent().unwrap()), 0o700);
    assert_eq!(mode(&dir), 0o700);
}

#[test]
fn an_applied_resize_is_recorded_between_the_output_around_it() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let sock = start_cat(&root, "pty-rec-resize");
    let _kill = harness::SessionGuard::new(&root, "pty-rec-resize");

    let mut human = attach_as_human(&sock);
    write_msg(&mut human, MSG_DATA, b"one\n");
    wait_log_contains(&root, "pty-rec-resize", "one");
    write_msg(
        &mut human,
        MSG_RESIZE,
        &resize_payload(Geometry::new(30, 100).unwrap()),
    );
    write_msg(&mut human, MSG_DATA, b"two\n");
    wait_log_contains(&root, "pty-rec-resize", "two");
    write_msg(&mut human, MSG_DETACH, &[]);
    drop(human);
    tendr(&root)
        .args(["kill", "pty-rec-resize", "--force"])
        .output()
        .unwrap();
    harness::wait_terminal(&root, "pty-rec-resize");

    let recording = decode_session_recording(&root, "pty-rec-resize");
    let resizes: Vec<usize> = recording
        .records
        .iter()
        .enumerate()
        .filter(|(_, r)| matches!(r.kind, tendr::recording::RecordKind::Resize { .. }))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(resizes.len(), 1, "{:?}", recording.records);
    let at = resizes[0];
    assert_eq!(
        recording.records[at].kind,
        tendr::recording::RecordKind::Resize {
            geometry: tendr::recording::Geometry::new(30, 100).unwrap(),
            cause: tendr::recording::ResizeCause::User,
        }
    );
    let before = recorded_output(&recording.records[..at]);
    let after = recorded_output(&recording.records[at + 1..]);
    assert!(contains(&before, b"one"), "before: {before:?}");
    assert!(!contains(&before, b"two"), "before: {before:?}");
    assert!(contains(&after, b"two"), "after: {after:?}");
}

#[test]
fn a_resize_with_a_zero_dimension_is_ignored() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "pty-zero-size", "--pty", "--stdin", "--", "sh"])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-zero-size");
    let _kill = harness::SessionGuard::new(&root, "pty-zero-size");
    let sock = wait_for_attach_socket(&root, "pty-zero-size");

    let mut human = attach_as_human(&sock);
    // Built by hand: `resize_payload` cannot encode a zero dimension.
    write_msg(&mut human, MSG_RESIZE, &[0, 0, 0, 100]);
    write_msg(&mut human, MSG_DATA, b"stty size\n");
    wait_log_contains(&root, "pty-zero-size", "24 80");
    write_msg(&mut human, MSG_DETACH, &[]);
    drop(human);
    tendr(&root)
        .args(["kill", "pty-zero-size", "--force"])
        .output()
        .unwrap();
    harness::wait_terminal(&root, "pty-zero-size");

    let recording = decode_session_recording(&root, "pty-zero-size");
    assert!(
        !recording
            .records
            .iter()
            .any(|r| matches!(r.kind, tendr::recording::RecordKind::Resize { .. })),
        "{:?}",
        recording.records
    );
}

/// The window-size records of a session's recording, in order.
fn recorded_resizes(
    recording: &tendr::recording::DecodedRecording,
) -> Vec<(Geometry, tendr::recording::ResizeCause)> {
    recording
        .records
        .iter()
        .filter_map(|r| match r.kind {
            tendr::recording::RecordKind::Resize { geometry, cause } => Some((geometry, cause)),
            _ => None,
        })
        .collect()
}

#[test]
fn reattach_at_the_same_size_records_a_tagged_repaint_pair() {
    use tendr::recording::ResizeCause;
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-repaint");
    start_cat(&root, "pty-repaint");

    // The CLI's terminal is 24x80, the size the session starts at.
    let mut cli = CliAttach::spawn(&root, &["pty-repaint"]);
    cli.type_until_logged(&root, "pty-repaint", "attached\n");
    let resizes = poll_until(Duration::from_secs(5), Duration::from_millis(50), || {
        let resizes = recorded_resizes(&decode_session_recording(&root, "pty-repaint"));
        if resizes.len() >= 3 {
            Observation::Ready(resizes)
        } else {
            Observation::Pending(format!("resizes so far: {resizes:?}"))
        }
    })
    .unwrap_or_else(|e| panic!("no repaint pair recorded: {e}"));

    let size = |rows, cols| Geometry::new(rows, cols).unwrap();
    let [
        (attached, ResizeCause::User),
        (shrunk, first),
        (restored, second),
    ] = resizes[..]
    else {
        panic!("expected the attach's size then a repaint pair: {resizes:?}");
    };
    assert_eq!(
        (attached, shrunk, restored),
        (size(24, 80), size(23, 80), size(24, 80))
    );
    assert!(
        matches!(first, ResizeCause::Repaint { .. }) && first == second,
        "both repaint records share one correlation: {resizes:?}"
    );
}

#[test]
fn reattach_at_a_new_size_records_only_a_user_resize() {
    use tendr::attach_proto::MSG_REPAINT;
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let sock = start_cat(&root, "pty-no-repaint");
    let _kill = harness::SessionGuard::new(&root, "pty-no-repaint");

    let mut human = attach_as_human(&sock);
    write_msg(
        &mut human,
        MSG_RESIZE,
        &resize_payload(Geometry::new(30, 100).unwrap()),
    );
    write_msg(&mut human, MSG_REPAINT, &[]);
    // The sidecar decides on the repaint before it reads this data, and the
    // shrink would be recorded before the data's echo.
    write_msg(&mut human, MSG_DATA, b"after-repaint\n");
    wait_log_contains(&root, "pty-no-repaint", "after-repaint");
    write_msg(&mut human, MSG_DETACH, &[]);
    drop(human);
    tendr(&root)
        .args(["kill", "pty-no-repaint", "--force"])
        .output()
        .unwrap();
    harness::wait_terminal(&root, "pty-no-repaint");

    assert_eq!(
        recorded_resizes(&decode_session_recording(&root, "pty-no-repaint")),
        [(
            Geometry::new(30, 100).unwrap(),
            tendr::recording::ResizeCause::User
        )]
    );
}

#[test]
fn a_recording_size_limit_stops_recording_but_not_the_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tendr(&root)
        .env("TENDR_TEST_RECORDING_MAX_BYTES", "2048")
        .args(["start", "pty-rec-limit", "--pty", "--stdin", "--", "cat"])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-rec-limit");
    let _kill = harness::SessionGuard::new(&root, "pty-rec-limit");

    // A small record first: how the PTY coalesces the flood into reads (and so
    // whether any of it fits under the limit) differs between platforms.
    push(&root, "pty-rec-limit", b"first\n");
    wait_log_contains(&root, "pty-rec-limit", "first");
    let line = format!("{}\n", "x".repeat(59));
    push(&root, "pty-rec-limit", line.repeat(50).as_bytes());

    let meta = poll_until(Duration::from_secs(10), Duration::from_millis(20), || {
        let meta = session_meta(&root, "pty-rec-limit");
        if meta["pty"]["recording"]["state"] == "Stopped" {
            Observation::Ready(meta)
        } else {
            Observation::Pending(meta.to_string())
        }
    })
    .unwrap_or_else(|e| panic!("recording never stopped: {e}"));
    assert_eq!(meta["status"], "Running", "the session keeps running");
    assert_eq!(meta["pty"]["recording"]["reason"], "size_limit");
    let last = meta["pty"]["recording"]["last_recorded_sequence"]
        .as_u64()
        .unwrap_or_else(|| panic!("the stop names the recorded prefix: {meta}"));

    // The event precedes the metadata flip.
    let events = harness::read_events(&root, "pty-rec-limit");
    let event = events
        .iter()
        .find(|e| e["kind"] == "recording.stopped")
        .expect("recording.stopped is logged before status shows it");
    assert_eq!(
        event["data"],
        serde_json::json!({"last_recorded_sequence": last, "reason": "size_limit"})
    );

    // Capture continues past the unrecorded suffix.
    push(&root, "pty-rec-limit", b"after-stop\n");
    wait_log_contains(&root, "pty-rec-limit", "after-stop");

    tendr(&root)
        .args(["kill", "pty-rec-limit", "--force"])
        .output()
        .unwrap();
    let meta = harness::wait_terminal(&root, "pty-rec-limit");
    assert_eq!(meta["pty"]["recording"]["state"], "Stopped", "{meta}");
    assert_eq!(meta["pty"]["recording"]["last_recorded_sequence"], last);
    assert!(
        meta["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("recording stopped")),
        "{meta}"
    );

    let recording = decode_session_recording(&root, "pty-rec-limit");
    assert_eq!(recording.records.last().unwrap().sequence.get(), last);
    assert!(!contains(
        &recorded_output(&recording.records),
        b"after-stop"
    ));
    let dir = recording_dir(&root, "pty-rec-limit");
    let total: u64 = tendr::recorder::DirectoryStore::segments(&dir)
        .unwrap()
        .iter()
        .map(|f| std::fs::metadata(f).unwrap().len())
        .sum();
    assert!(total <= 2048, "{total} bytes recorded");
}

/// A run's end invalidates its attach handles (PR #68 review, finding 1). The
/// attached client gets end of stream while the run's on-exit hook is still
/// running, and it can't touch the replacement run: before the fix, its late
/// detach rewrote the new run's `meta.json` back to `AgentControl` while a human
/// held the new run.
#[test]
fn a_finished_runs_attach_client_cannot_touch_its_replacement() {
    use std::io::Read;

    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let started = root.path().join("hook-started");
    let gate = root.path().join("hook-gate");
    // Holds run A's sidecar in its on-exit hook until the gate appears, or 20 s.
    let hook = format!(
        "sh -c {} {} {}",
        shell_words::quote(
            r#"touch "$0"; i=0; while [ ! -e "$1" ] && [ $i -lt 400 ]; do sleep 0.05; i=$((i+1)); done"#
        ),
        shell_words::quote(started.to_str().unwrap()),
        shell_words::quote(gate.to_str().unwrap()),
    );

    tendr(&root)
        .args([
            "start",
            "pty-end",
            "--pty",
            "--stdin",
            "--on-exit",
            &hook,
            "--",
            "cat",
        ])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "pty-end");
    let sock_a = wait_for_attach_socket(&root, "pty-end");
    let mut stale = attach_as_human(&sock_a);
    wait_for_pty_control(&root, "pty-end", "HumanControl");

    tendr(&root)
        .args(["kill", "--force", "pty-end"])
        .assert()
        .success();
    harness::poll_until(Duration::from_secs(10), Duration::from_millis(20), || {
        if started.exists() {
            harness::Observation::Ready(())
        } else {
            harness::Observation::Pending("run A's on-exit hook has not started".into())
        }
    })
    .unwrap();

    // 1. The run is over: its client sees end of stream now, not when the
    //    sidecar process finally exits after the hook.
    // EINVAL on macOS means the socket is already shut down; the read below
    // then returns end of stream at once (as in `read_until_closed`).
    let _ = stale.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 4096];
    loop {
        match stale.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => continue,
            Err(e) => panic!("run A's client should see end of stream, got {e:?}"),
        }
    }

    // 2. Replace the run and give the new one a human controller.
    tendr(&root)
        .args([
            "start",
            "pty-end",
            "--pty",
            "--stdin",
            "--replace",
            "--",
            "cat",
        ])
        .assert()
        .success();
    let sock_b = wait_for_attach_socket(&root, "pty-end");
    assert_ne!(sock_a, sock_b, "the replacement binds its own socket");
    let _human_b = attach_as_human(&sock_b);
    wait_for_pty_control(&root, "pty-end", "HumanControl");

    // 3. Run A's stale client goes away while A's sidecar is still alive.
    drop(stale);
    std::thread::sleep(Duration::from_millis(1500));
    let meta: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            root.path()
                .join(".tendr/sessions/default/pty-end/meta.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        meta["pty"]["control"], "HumanControl",
        "run A must not rewrite run B's control owner: {meta}"
    );

    std::fs::write(&gate, b"").unwrap();
}

/// Input arriving on the stdin FIFO (how `exec` reaches a PTY python-repl)
/// while an agent `push` holds the PTY input waits its turn instead of being
/// dropped (PR #68 review, finding 2).
#[test]
fn fifo_input_waits_behind_an_agent_push() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-fifo");
    let go = root.path().join("go");

    // Consumes 1 KiB, reports READY, then stops reading until the go file
    // exists, so the push below stays in flight holding the input.
    let script = "stty raw -echo; head -c 1024 >/dev/null; printf READY; \
                  while [ ! -f \"$GO\" ]; do sleep 0.05; done; exec cat";
    tendr(&root)
        .args(["start", "pty-fifo", "--pty", "--stdin"])
        .arg("--env")
        .arg(format!("GO={}", go.display()))
        .args(["--", "sh", "-c", script])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-fifo");

    let mut payload = vec![b'a'; 1 << 20];
    payload.extend_from_slice(b"\nPUSH-TAIL\n");
    let mut agent = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args(["push", "pty-fifo"])
        .env("HOME", root.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut agent_stdin = agent.stdin.take().unwrap();
    let feeder = std::thread::spawn(move || {
        let _ = agent_stdin.write_all(&payload);
    });
    wait_log_contains(&root, "pty-fifo", "READY");
    assert!(
        agent.try_wait().unwrap().is_none(),
        "setup invariant: the push must still hold the input"
    );

    // An exec-style frame through the FIFO while the push holds the input.
    let fifo = root
        .path()
        .join(".tendr/sessions/default/pty-fifo/stdin.pipe");
    let writer = std::thread::spawn(move || {
        let mut f = std::fs::OpenOptions::new().write(true).open(fifo).unwrap();
        f.write_all(b"FIFO-LINE\n").unwrap();
    });

    std::fs::write(&go, b"").unwrap();
    feeder.join().unwrap();
    let status = agent.wait().unwrap();
    assert!(status.success(), "the push itself completes");
    writer.join().unwrap();

    let log = wait_log_contains(&root, "pty-fifo", "FIFO-LINE");
    assert!(log.contains("PUSH-TAIL"), "the push was written in full");
    assert!(
        log.find("PUSH-TAIL") < log.find("FIFO-LINE"),
        "the FIFO input waits for the push rather than interleaving"
    );
}

/// A FIFO writer (an `exec` frame) closes as soon as its bytes are in the pipe,
/// so its hang-up is how a complete frame ends, not a sign it was abandoned.
/// Bytes still waiting on a full PTY after the writer has gone must be
/// delivered once the child reads again, and the agent claim then released
/// (PR #68 review, finding 6: not changed, because cancelling on hang-up would
/// drop them).
///
/// How much a PTY and a pipe buffer differs by platform (a Linux PTY absorbs
/// tens of KiB), so the test assumes no size. It writes until the FIFO itself
/// stays full, which can only happen once the forwarder is stuck on a full
/// PTY, and closes the writer there.
#[test]
fn fifo_input_is_delivered_after_its_writer_closes() {
    use std::os::unix::fs::OpenOptionsExt;

    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-fifo-gone");
    let go = root.path().join("go");
    let out = root.path().join("received");

    // Reads nothing until the go file exists, then copies its input to a file.
    let script = "stty raw -echo; printf READY; \
                  while [ ! -f \"$GO\" ]; do sleep 0.05; done; exec cat > \"$OUT\"";
    tendr(&root)
        .args(["start", "pty-fifo-gone", "--pty", "--stdin"])
        .arg("--env")
        .arg(format!("GO={}", go.display()))
        .arg("--env")
        .arg(format!("OUT={}", out.display()))
        .args(["--", "sh", "-c", script])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-fifo-gone");
    wait_log_contains(&root, "pty-fifo-gone", "READY");

    // Nonblocking, so a full FIFO is observed instead of waited on. The open
    // is refused (ENXIO) until the forwarder has the FIFO open for reading.
    let fifo_path = root
        .path()
        .join(".tendr/sessions/default/pty-fifo-gone/stdin.pipe");
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut fifo = loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&fifo_path)
        {
            Ok(f) => break f,
            Err(e) => assert!(Instant::now() < deadline, "open stdin.pipe: {e}"),
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // Fill the FIFO until it has stayed full for a whole second. The forwarder
    // drains it as fast as the PTY takes input, so a FIFO that stays full means
    // the PTY is full and the forwarder is stuck mid-frame.
    let chunk = [b'a'; 1024];
    let mut written = 0usize;
    let mut last_progress = Instant::now();
    while last_progress.elapsed() < Duration::from_secs(1) {
        assert!(Instant::now() < deadline, "the FIFO never stayed full");
        match fifo.write(&chunk) {
            Ok(n) => {
                written += n;
                last_progress = Instant::now();
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("write stdin.pipe: {e}"),
        }
    }
    drop(fifo); // the writer is gone; its frame is not all delivered

    let early = tendr(&root)
        .args(["push", "pty-fifo-gone"])
        .write_stdin("EARLY\n")
        .output()
        .unwrap();
    let early_err = String::from_utf8_lossy(&early.stderr);
    assert!(
        !early.status.success() && early_err.contains("(Agent)"),
        "the waiting frame still holds the input after its writer closed: {early_err}"
    );

    // Once the child reads again, every byte of the frame arrives, and then
    // the frame no longer holds the input.
    std::fs::write(&go, b"").unwrap();
    let received = || std::fs::metadata(&out).map_or(0, |m| m.len() as usize);
    while received() < written {
        assert!(
            Instant::now() < deadline,
            "{} of {written} bytes delivered",
            received()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let after = tendr(&root)
        .args(["push", "pty-fifo-gone"])
        .write_stdin("AFTER\n")
        .output()
        .unwrap();
    assert!(
        after.status.success(),
        "push after the frame: {}",
        String::from_utf8_lossy(&after.stderr)
    );
    while received() < written + 6 {
        assert!(Instant::now() < deadline, "the later push never arrived");
        std::thread::sleep(Duration::from_millis(20));
    }
    let bytes = std::fs::read(&out).unwrap();
    assert_eq!(bytes.len(), written + 6);
    assert!(bytes[..written].iter().all(|&b| b == b'a'));
    assert_eq!(&bytes[written..], b"AFTER\n");
}

// Exit codes of `attach` and PTY `push` (exit-code table, #107): 80 protocol,
// 81 control conflict, 84 runtime/transport or a partial write, 85 identity.
// The helpers and tests below are one block.

/// A `tendr push` that holds the session's terminal: its stdin stays open, so
/// it keeps control until [`HeldPush::finish`] closes it.
struct HeldPush {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
}

impl HeldPush {
    /// Start a push into `session` and wait until its first line has reached
    /// the terminal, so it holds control from then on.
    fn start(root: &TempDir, session: &str) -> Self {
        let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
            .args(["push", session])
            .env("HOME", root.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"PUSH-HOLDS\n").unwrap();
        stdin.flush().unwrap();
        wait_log_contains(root, session, "PUSH-HOLDS");
        assert!(
            child.try_wait().unwrap().is_none(),
            "setup invariant: the push must still hold the terminal"
        );
        Self {
            child,
            stdin: Some(stdin),
        }
    }

    /// Send `bytes`, close stdin, and return how the push ended.
    fn finish(mut self, bytes: &[u8]) -> std::process::Output {
        let mut stdin = self.stdin.take().unwrap();
        // A push already ended by the session may have stopped reading.
        let _ = stdin.write_all(bytes);
        drop(stdin);
        self.child.wait_with_output().unwrap()
    }
}

/// When a [`fake_sidecar`] replies.
#[derive(Clone, Copy)]
enum FakeReply {
    /// After reading the hello, as a sidecar answering it does.
    AfterHello,
    /// On accepting, then closing without reading anything, as the real
    /// sidecar refuses a peer of another user or a connection over its limit.
    BeforeHello,
}

/// Point `session`'s breadcrumb at a fake sidecar listening in `root`, which
/// answers each connection with `reply`, raw frame bytes, as a sidecar speaking
/// another protocol would. The real sidecar keeps running.
fn fake_sidecar(root: &TempDir, session: &str, label: &str, when: FakeReply, reply: Vec<u8>) {
    let sock = root.path().join(format!("{label}.sock"));
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().map_while(Result::ok) {
            match when {
                FakeReply::AfterHello => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                    if read_msg(&mut stream).is_ok() {
                        let _ = stream.write_all(&reply);
                    }
                }
                FakeReply::BeforeHello => {
                    let _ = stream.write_all(&reply);
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
            }
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

/// One frame's bytes.
fn frame_bytes(msg_type: u8, payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    tendr::attach_proto::write_msg(&mut bytes, msg_type, payload).unwrap();
    bytes
}

fn push_output(root: &TempDir, session: &str, bytes: &[u8]) -> std::process::Output {
    tendr(root)
        .args(["push", session])
        .write_stdin(bytes.to_vec())
        .timeout(Duration::from_secs(20))
        .output()
        .expect("push returns")
}

/// Assert `output` exited with `code`, showing its stderr if not.
fn assert_exit(output: &std::process::Output, code: i32, what: &str) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "{what}; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The CLI's own check refuses a plain attach while a human holds the
/// terminal: a controller conflict.
#[test]
fn cli_attach_refused_while_human_holds_exits_81() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-x81-human");
    let sock_path = start_cat(&root, "pty-x81-human");
    let _human = attach_as_human(&sock_path);
    wait_for_pty_control(&root, "pty-x81-human", "HumanControl");

    let output = tendr(&root)
        .args(["attach", "pty-x81-human"])
        .output()
        .unwrap();
    assert_exit(&output, 81, "attach while a human holds the terminal");
}

/// The sidecar refuses a plain attach while a push holds the terminal (the
/// CLI's own check passes: `meta.json` says `AgentControl`); its refusal's
/// class, not its text, gives the code.
#[test]
fn cli_attach_refused_by_the_sidecar_exits_81() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-x81-side");
    start_cat(&root, "pty-x81-side");
    let held = HeldPush::start(&root, "pty-x81-side");

    let wrapped = WrappedAttach::spawn(&root, "x81-side", &["pty-x81-side"]);
    assert_eq!(
        wrapped.wait_exit(Duration::from_secs(20)),
        81,
        "attach refused by the sidecar; cli output: {}",
        wrapped.cli.output()
    );
    wrapped.assert_terminal_restored();

    let pushed = held.finish(b"PUSH-ENDS\n");
    assert_exit(&pushed, 0, "the holding push completes");
}

/// An attach retired by another client's `--takeover` lost control, so it
/// exits 81, not 0: a script can tell it from a detach.
#[test]
fn cli_attach_retired_by_a_takeover_exits_81() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-x81-retired");
    let sock_path = start_cat(&root, "pty-x81-retired");

    let mut wrapped = WrappedAttach::spawn(&root, "x81-retired", &["pty-x81-retired"]);
    wrapped
        .cli
        .type_until_logged(&root, "pty-x81-retired", "before-takeover\n");
    let (_other, (msg_type, _)) = hello(&sock_path, Mode::Takeover);
    assert_eq!(msg_type, MSG_ACCEPTED);

    assert_eq!(wrapped.wait_exit(Duration::from_secs(10)), 81);
    wrapped.assert_terminal_restored();
    wrapped.wait_output_contains("took over");
}

/// An attach whose terminal stopped reading during a flood cannot read the
/// takeover notice queued behind its output. Once its terminal reads again,
/// it still learns it was taken over and exits 81, even after the client that
/// took over has detached.
#[test]
fn cli_attach_taken_over_while_stalled_exits_81() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-x81-stalled");
    // A paced flood of 8000 bytes per 10 ms tick: at most 800 KB/s, so at
    // most 3.2 MB in the 4 s stall, well inside the sidecar's 8 MiB viewer
    // queue, and the client is never disconnected for falling behind. What
    // must fill for the sidecar's write to block is well under 1 MB: the
    // terminal's PTY buffer (tens of KB), the socket's buffers (16 KB on
    // macOS, about 2 × 208 KB on Linux) and the one output frame (at most
    // 64 KB) the client's reader holds. Even with `sleep` taking twice its
    // 10 ms, the flood brings 1.6 MB in 4 s, so the stall is certain.
    tendr(&root)
        .args(["start", "pty-x81-stalled", "--pty", "--stdin", "--"])
        .args([
            "sh",
            "-c",
            "stty raw -echo; line=$(printf '%01000d' 0); \
             while :; do i=0; while [ $i -lt 8 ]; do printf '%s' \"$line\"; i=$((i+1)); done; sleep 0.01; done",
        ])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-x81-stalled");
    let sock_path = wait_for_attach_socket(&root, "pty-x81-stalled");

    let wrapped = WrappedAttach::spawn(&root, "x81-stalled", &["pty-x81-stalled"]);
    wrapped.wait_output_contains("0000000000");
    wait_for_pty_control(&root, "pty-x81-stalled", "HumanControl");
    // The terminal stops reading; the flood fills its buffer, the CLI's socket
    // and the sidecar's write to it.
    wrapped.cli.pause_output();
    std::thread::sleep(Duration::from_secs(4));

    let (other, (msg_type, _)) = hello(&sock_path, Mode::Takeover);
    assert_eq!(msg_type, MSG_ACCEPTED);
    std::thread::sleep(Duration::from_secs(1));
    drop(other);
    wait_for_pty_control(&root, "pty-x81-stalled", "AgentControl");
    assert!(
        !wrapped.exited(),
        "a client that is not reading stays blocked"
    );

    wrapped.cli.resume_output();
    assert_eq!(
        wrapped.wait_exit(Duration::from_secs(20)),
        81,
        "a stalled client that was taken over; cli output tail: {:?}",
        tail(&wrapped.cli.output())
    );
    wrapped.assert_terminal_restored();
    wrapped.wait_output_contains("took over");
}

/// A client dropped for falling behind has already let go of control, so a
/// later `--takeover`, which grants the next epoch, did not take it over: it
/// exits 0 once its terminal reads again.
#[test]
fn cli_attach_dropped_for_lag_then_taken_over_exits_0() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-x0-lagged");
    // Unpaced: far past the 8 MiB viewer queue within a few seconds.
    tendr(&root)
        .args(["start", "pty-x0-lagged", "--pty", "--stdin", "--"])
        .args(["sh", "-c", "stty raw -echo; yes 0000000000"])
        .output()
        .unwrap();
    harness::wait_running(&root, "pty-x0-lagged");
    let sock_path = wait_for_attach_socket(&root, "pty-x0-lagged");

    let wrapped = WrappedAttach::spawn(&root, "x0-lagged", &["pty-x0-lagged"]);
    wrapped.wait_output_contains("0000000000");
    wait_for_pty_control(&root, "pty-x0-lagged", "HumanControl");
    wrapped.cli.pause_output();
    // The sidecar drops the client once its queue overflows, releasing control.
    // Under a loaded test run the flood can take a while to pass 8 MiB.
    wait_for_pty_control_within(
        &root,
        "pty-x0-lagged",
        "AgentControl",
        Duration::from_secs(60),
    );

    let (other, (msg_type, _)) = hello(&sock_path, Mode::Takeover);
    assert_eq!(msg_type, MSG_ACCEPTED);
    drop(other);
    wait_for_pty_control(&root, "pty-x0-lagged", "AgentControl");

    wrapped.cli.resume_output();
    assert_eq!(
        wrapped.wait_exit(Duration::from_secs(20)),
        0,
        "a client dropped before the takeover; cli output tail: {:?}",
        tail(&wrapped.cli.output())
    );
    assert!(!wrapped.cli.output().contains("took over"));
}

fn tail(output: &str) -> &str {
    let start = output.len().saturating_sub(200);
    let start = (start..output.len())
        .find(|&i| output.is_char_boundary(i))
        .unwrap_or(output.len());
    &output[start..]
}

/// A sidecar that does not speak this protocol: an unreleased v1 sidecar's
/// free-text refusal (its first byte is no known class), and a reply of a
/// message type this version does not know.
#[test]
fn cli_attach_to_an_older_sidecar_exits_80() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-x80-attach");
    start_cat(&root, "pty-x80-attach");

    for (label, reply) in [
        ("old", frame_bytes(MSG_REJECTED, b"busy")),
        ("new", frame_bytes(0x7f, b"hello from the future")),
    ] {
        fake_sidecar(&root, "pty-x80-attach", label, FakeReply::AfterHello, reply);
        let wrapped = WrappedAttach::spawn(&root, &format!("x80-{label}"), &["pty-x80-attach"]);
        assert_eq!(
            wrapped.wait_exit(Duration::from_secs(20)),
            80,
            "{label} sidecar; cli output: {}",
            wrapped.cli.output()
        );
        wrapped.assert_terminal_restored();
    }
}

#[test]
fn cli_push_to_an_older_sidecar_exits_80() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-x80-push");
    start_cat(&root, "pty-x80-push");

    for (label, reply) in [
        ("old", frame_bytes(MSG_REJECTED, b"busy")),
        ("new", frame_bytes(0x7f, b"hello from the future")),
    ] {
        fake_sidecar(&root, "pty-x80-push", label, FakeReply::AfterHello, reply);
        let output = push_output(&root, "pty-x80-push", b"never-written\n");
        assert_exit(&output, 80, &format!("push to a {label} sidecar"));
    }
}

#[test]
fn cli_push_refused_while_human_holds_exits_81() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-x81-push-human");
    let sock_path = start_cat(&root, "pty-x81-push-human");
    let _human = attach_as_human(&sock_path);
    wait_for_pty_control(&root, "pty-x81-push-human", "HumanControl");

    let output = push_output(&root, "pty-x81-push-human", b"refused\n");
    assert_exit(&output, 81, "push while a human holds the terminal");
}

/// The sidecar refuses a push while another holds the terminal.
#[test]
fn cli_push_refused_while_another_push_holds_exits_81() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-x81-push-push");
    start_cat(&root, "pty-x81-push-push");
    let held = HeldPush::start(&root, "pty-x81-push-push");

    let output = push_output(&root, "pty-x81-push-push", b"SECOND-PUSH\n");
    assert_exit(&output, 81, "a second push while one holds the terminal");

    let pushed = held.finish(b"PUSH-ENDS\n");
    assert_exit(&pushed, 0, "the holding push completes");
    let log = wait_log_contains(&root, "pty-x81-push-push", "PUSH-ENDS");
    assert!(!log.contains("SECOND-PUSH"), "refused bytes were written");
}

/// A push revoked by a takeover with bytes still to send ends with bytes
/// unwritten: a partial write. Its later bytes are sent only after the
/// takeover, so this holds whatever the terminal buffers.
#[test]
fn cli_push_revoked_by_takeover_exits_84() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-x84-revoked");
    let sock_path = start_cat(&root, "pty-x84-revoked");
    let held = HeldPush::start(&root, "pty-x84-revoked");

    let (_human, (msg_type, _)) = hello(&sock_path, Mode::Takeover);
    assert_eq!(msg_type, MSG_ACCEPTED);
    let pushed = held.finish(b"AFTER-TAKEOVER\n");
    assert_exit(&pushed, 84, "a push revoked with bytes unwritten");
    assert!(
        String::from_utf8_lossy(&pushed.stderr).contains("revoked"),
        "the failure says the push was revoked"
    );
}

/// A breadcrumb naming a socket nobody listens on: a transport failure.
#[test]
fn cli_push_to_a_socket_with_no_listener_exits_84() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-x84-dead");
    start_cat(&root, "pty-x84-dead");
    let dead = root.path().join("dead.sock");
    drop(std::os::unix::net::UnixListener::bind(&dead).unwrap());
    point_breadcrumb_at(&root, "pty-x84-dead", &dead);

    let output = push_output(&root, "pty-x84-dead", b"nowhere\n");
    assert_exit(&output, 84, "push to a socket with no listener");
}

/// The sidecar's peer check cannot fail for a test run by one user, so a fake
/// sidecar sends the refusal it would send, in the same order: on accepting,
/// before reading the hello, then closing. The client-side check's mapping is
/// unit-tested in `pty_exit`, and the hello that cannot be written because the
/// refusal closed the connection first in `commands::attach`.
#[test]
fn cli_attach_and_push_refused_for_peer_identity_exit_85() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-x85");
    start_cat(&root, "pty-x85");
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
        "pty-x85",
        "identity",
        FakeReply::BeforeHello,
        refusal,
    );

    // The refusal can close the connection before or after the hello is
    // written; either way the push exits 85.
    for _ in 0..20 {
        let output = push_output(&root, "pty-x85", b"refused\n");
        assert_exit(&output, 85, "push refused for peer identity");
    }
    let wrapped = WrappedAttach::spawn(&root, "x85", &["pty-x85"]);
    assert_eq!(
        wrapped.wait_exit(Duration::from_secs(20)),
        85,
        "attach refused for peer identity; cli output: {}",
        wrapped.cli.output()
    );
}

/// A session whose run has ended is a stale run for `attach`: the CLI's own
/// check exits 81, as the sidecar's "session ended" refusal does when the run
/// ends as the attach connects.
#[test]
fn cli_attach_to_a_finished_session_exits_81() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pty-x81-ended");
    tendr(&root)
        .args(["start", "pty-x81-ended", "--pty", "--", "true"])
        .output()
        .unwrap();
    harness::wait_terminal(&root, "pty-x81-ended");

    let output = tendr(&root)
        .args(["attach", "pty-x81-ended"])
        .output()
        .unwrap();
    assert_exit(&output, 81, "attach to a finished session");
}

/// A pipe session can never be attached, so a finished one is refused as not
/// PTY-enabled (1), not as a stale run.
#[test]
fn cli_attach_to_a_finished_pipe_session_exits_1() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = harness::SessionGuard::new(&root, "pipe-attach-ended");
    tendr(&root)
        .args(["start", "pipe-attach-ended", "--", "true"])
        .output()
        .unwrap();
    harness::wait_terminal(&root, "pipe-attach-ended");

    let output = tendr(&root)
        .args(["attach", "pipe-attach-ended"])
        .output()
        .unwrap();
    assert_exit(&output, 1, "attach to a finished pipe session");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not PTY-enabled"),
        "a pipe session is refused as not PTY-enabled: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The same for a push to a PTY session; a pipe session keeps exit 1.
#[test]
fn cli_push_to_a_finished_session_exits_81_for_a_pty_and_1_for_a_pipe() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _pty = harness::SessionGuard::new(&root, "pty-x81-push-ended");
    let _pipe = harness::SessionGuard::new(&root, "pipe-push-ended");
    tendr(&root)
        .args([
            "start",
            "pty-x81-push-ended",
            "--pty",
            "--stdin",
            "--",
            "true",
        ])
        .output()
        .unwrap();
    tendr(&root)
        .args(["start", "pipe-push-ended", "--stdin", "--", "true"])
        .output()
        .unwrap();
    harness::wait_terminal(&root, "pty-x81-push-ended");
    harness::wait_terminal(&root, "pipe-push-ended");

    let output = push_output(&root, "pty-x81-push-ended", b"too late\n");
    assert_exit(&output, 81, "push to a finished PTY session");
    let output = push_output(&root, "pipe-push-ended", b"too late\n");
    assert_exit(&output, 1, "push to a finished pipe session");
}
