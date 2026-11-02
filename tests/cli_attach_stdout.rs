//! `attach --read-only` with no terminal, against a real sidecar: stdout
//! carries the run's output and nothing else, stdin is never read, and the
//! exit code says how the attach ended.
#![cfg(unix)]

mod harness;

use harness::{Observation, SessionGuard, poll_until, tendr};
use std::io::{Read, Seek, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;
use tempfile::TempDir;
use tendr::attach_proto::{Frame, Mode, Version, read_frame, write_frame};

static SERIAL: Mutex<()> = Mutex::new(());

fn session_dir(root: &TempDir, session: &str) -> PathBuf {
    root.path()
        .join(format!(".tendr/sessions/default/{session}"))
}

/// Start a PTY session running `script` under `sh -c`, with `env` set for
/// the sidecar, and wait for its attach socket.
fn start(root: &TempDir, session: &str, script: &str, env: &[(&str, &str)]) {
    let mut start = tendr(root);
    for (key, value) in env {
        start.env(key, value);
    }
    let output = start
        .args(["start", session, "--pty", "--", "sh", "-c", script])
        .output()
        .unwrap();
    assert!(output.status.success(), "start {session}: {output:?}");
    harness::wait_running(root, session);
    socket(root, session);
}

fn socket(root: &TempDir, session: &str) -> PathBuf {
    let dir = session_dir(root, session);
    poll_until(Duration::from_secs(10), Duration::from_millis(50), || {
        match tendr::attach_proto::read_sock_path(&dir) {
            Some(path) => Observation::Ready(path),
            None => Observation::Pending("no attach socket yet".to_owned()),
        }
    })
    .unwrap()
}

fn touch(path: &Path) {
    std::fs::write(path, b"").unwrap();
}

/// `tendr attach --read-only <session>` with `stdin`, stdout and stderr
/// piped, and `env` set.
fn attach(root: &TempDir, session: &str, stdin: Stdio, env: &[(&str, &str)]) -> Child {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin("tendr"));
    cmd.env("HOME", root.path())
        .env_remove("TENDR_TEST_ATTACH_TERMINAL")
        .args(["attach", "--read-only", session])
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd.spawn().unwrap()
}

/// Read `stdout` until what was read contains `marker`.
fn read_until(stdout: &mut ChildStdout, marker: &str) -> Vec<u8> {
    let mut read = Vec::new();
    let mut buf = [0u8; 4096];
    while !String::from_utf8_lossy(&read).contains(marker) {
        let n = stdout.read(&mut buf).unwrap();
        assert!(n > 0, "stdout ended before {marker:?}: {read:?}");
        read.extend_from_slice(&buf[..n]);
    }
    read
}

/// Wait for `child` to exit, killing it after a minute, and return its code
/// and stderr.
fn finish(mut child: Child) -> (Option<i32>, String) {
    let status = poll_until(
        Duration::from_secs(60),
        Duration::from_millis(20),
        || match child.try_wait().unwrap() {
            Some(status) => Observation::Ready(status),
            None => Observation::Pending("attach still running".to_owned()),
        },
    )
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

/// A child that turns no terminal modes on: ticks until `go` exists, then
/// prints a payload and ends.
fn ticking_then(go: &Path, then: &str) -> String {
    format!(
        "while [ ! -f '{go}' ]; do echo tick; sleep 0.05; done; {then}",
        go = go.display()
    )
}

#[test]
fn stdout_is_exactly_the_runs_output_and_stdin_is_never_read() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "ro-out");
    let go = root.path().join("go");
    start(
        &root,
        "ro-out",
        &ticking_then(
            &go,
            "i=0; while [ $i -lt 200 ]; do echo line-$i; i=$((i+1)); done; echo ALL-DONE",
        ),
        &[],
    );
    // Typed-ahead data a reader of stdin would consume: its shared file
    // offset shows whether anything read it.
    let mut stdin = tempfile::tempfile().unwrap();
    stdin.write_all(b"would be typed\x1c\x64").unwrap();
    stdin.rewind().unwrap();

    let mut child = attach(
        &root,
        "ro-out",
        Stdio::from(stdin.try_clone().unwrap()),
        &[],
    );
    let mut stdout = child.stdout.take().unwrap();
    let mut out = read_until(&mut stdout, "tick");
    touch(&go);
    stdout.read_to_end(&mut out).unwrap();
    let (code, stderr) = finish(child);

    assert_eq!(code, Some(0), "the run ended; stderr: {stderr}");
    let recording = harness::recorded_output(&root, "ro-out");
    assert!(
        recording.ends_with(&out),
        "stdout is not the recording from the attach on:\n{}",
        String::from_utf8_lossy(&out)
    );
    assert!(String::from_utf8_lossy(&out).contains("line-199\r\nALL-DONE\r\n"));
    assert_eq!(stdin.stream_position().unwrap(), 0, "stdin was read");
}

#[test]
fn stdout_gets_the_mode_replay_but_no_resets_when_leaving() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "ro-modes");
    let go = root.path().join("go");
    // Bracketed paste on, then ticks, then the end.
    let script = format!(
        "printf '\\033[?2004h'; {}",
        ticking_then(&go, "echo ALL-DONE")
    );
    start(&root, "ro-modes", &script, &[]);

    let mut child = attach(&root, "ro-modes", Stdio::null(), &[]);
    let mut stdout = child.stdout.take().unwrap();
    let mut out = read_until(&mut stdout, "tick");
    touch(&go);
    stdout.read_to_end(&mut out).unwrap();
    let (code, stderr) = finish(child);

    assert_eq!(code, Some(0), "the run ended; stderr: {stderr}");
    assert!(
        out.starts_with(b"\x1b[?2004h"),
        "no mode replay first: {:?}",
        String::from_utf8_lossy(&out)
    );
    assert!(
        out.ends_with(b"ALL-DONE\r\n"),
        "stdout does not end with the run's last output: {:?}",
        String::from_utf8_lossy(&out)
    );
}

#[test]
fn a_watcher_cut_off_while_its_consumer_stalls_exits_84() {
    // Far smaller than the flood, so the stalled attach falls behind it.
    const RING: &str = "1048576";
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "ro-cut");
    let (go, end) = (root.path().join("go"), root.path().join("end"));
    let line = "0123456789abcdef".repeat(62);
    let flood = format!(
        "i=0; while [ $i -lt 300 ]; do yes {line} | head -n 10; i=$((i+1)); done; \
         echo ALL-DONE; while [ ! -f '{end}' ]; do sleep 0.05; done",
        end = end.display()
    );
    start(
        &root,
        "ro-cut",
        &ticking_then(&go, &flood),
        &[("TENDR_TEST_OUTPUT_RING_BYTES", RING)],
    );

    let mut child = attach(&root, "ro-cut", Stdio::null(), &[]);
    let mut stdout = child.stdout.take().unwrap();
    read_until(&mut stdout, "tick");
    // Stall: read nothing while the run floods past the ring.
    touch(&go);
    poll_until(Duration::from_secs(60), Duration::from_millis(100), || {
        if harness::logged_output(&root, "ro-cut").contains("ALL-DONE") {
            Observation::Ready(())
        } else {
            Observation::Pending("no ALL-DONE yet".to_owned())
        }
    })
    .unwrap();
    let mut out = Vec::new();
    stdout.read_to_end(&mut out).unwrap();
    let (code, stderr) = finish(child);
    touch(&end);
    harness::wait_terminal(&root, "ro-cut");

    assert_eq!(
        code,
        Some(84),
        "cut off while the run went on; stderr: {stderr}"
    );
    assert!(!stderr.is_empty(), "the cut-off is reported on stderr");
    assert!(
        !String::from_utf8_lossy(&out).contains("tendr:"),
        "a notice reached stdout"
    );
}

/// Connect a raw watcher: the connection, if the sidecar admitted it.
fn watcher_admitted(root: &TempDir, session: &str) -> Option<UnixStream> {
    let mut stream = UnixStream::connect(socket(root, session)).unwrap();
    write_frame(
        &mut stream,
        &Frame::Hello {
            version: Version::V2,
            mode: Mode::Watch,
        },
    )
    .unwrap();
    match read_frame(&mut stream).unwrap() {
        Frame::Watching(_) => Some(stream),
        _ => None,
    }
}

#[test]
fn a_consumer_that_closes_early_detaches_the_watcher_and_exits_1() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "ro-pipe");
    let end = root.path().join("end");
    start(&root, "ro-pipe", &ticking_then(&end, "true"), &[]);

    // Seven watchers and the attach fill the run's watcher pool.
    let others: Vec<UnixStream> = (0..7)
        .map(|i| watcher_admitted(&root, "ro-pipe").unwrap_or_else(|| panic!("watcher {i}")))
        .collect();
    let mut child = attach(&root, "ro-pipe", Stdio::null(), &[]);
    let mut stdout = child.stdout.take().unwrap();
    read_until(&mut stdout, "tick");
    assert!(
        watcher_admitted(&root, "ro-pipe").is_none(),
        "the pool is full"
    );
    drop(stdout);
    let (code, stderr) = finish(child);

    assert_eq!(code, Some(1), "stderr: {stderr}");
    // The sidecar let the attach go: its slot takes a new watcher.
    let freed =
        poll_until(
            Duration::from_secs(5),
            Duration::from_millis(50),
            || match watcher_admitted(&root, "ro-pipe") {
                Some(stream) => Observation::Ready(stream),
                None => Observation::Pending("the attach's slot is still held".to_owned()),
            },
        );
    touch(&end);
    harness::wait_terminal(&root, "ro-pipe");
    drop(others);
    freed.unwrap();
}

#[test]
fn sigterm_ends_the_attach_with_1_while_stdin_would_block() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "ro-term");
    let end = root.path().join("end");
    start(&root, "ro-term", &ticking_then(&end, "true"), &[]);

    // A stdin that blocks any read, held open for the whole attach.
    let (stdin_read, stdin_write) = rustix::pipe::pipe().unwrap();
    let mut child = attach(&root, "ro-term", Stdio::from(stdin_read), &[]);
    let mut stdout = child.stdout.take().unwrap();
    read_until(&mut stdout, "tick");
    // SAFETY: kill(2) on the child's pid, which has not been reaped.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    let reading = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        out
    });
    let (code, stderr) = finish(child);
    let out = reading.join().unwrap();
    drop(stdin_write);
    touch(&end);
    harness::wait_terminal(&root, "ro-term");

    assert_eq!(code, Some(1), "stderr: {stderr}");
    assert!(stderr.contains("SIGTERM"), "stderr: {stderr}");
    assert!(
        !String::from_utf8_lossy(&out).contains("SIGTERM"),
        "the notice reached stdout"
    );
}

/// Every viewer that is behind when the run ends gets the run's last bytes:
/// the session sends each, the controller and the watchers, everything up to
/// the run's final offset before it closes their connections, waiting a
/// moment for one slow to read.
#[test]
fn every_viewer_behind_at_the_runs_end_gets_its_last_bytes() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "ro-flush");
    let go = root.path().join("go");
    start(
        &root,
        "ro-flush",
        &ticking_then(
            &go,
            &format!("yes {} | head -n 40000; printf LAST-BYTES", "y".repeat(62)),
        ),
        &[],
    );
    // A version 2 controller that reads nothing until the run has ended.
    let mut controller = UnixStream::connect(socket(&root, "ro-flush")).unwrap();
    write_frame(
        &mut controller,
        &Frame::Hello {
            version: Version::V2,
            mode: Mode::Attach,
        },
    )
    .unwrap();
    assert!(matches!(
        read_frame(&mut controller),
        Ok(Frame::AcceptedV2(_))
    ));

    let mut child = attach(&root, "ro-flush", Stdio::null(), &[]);
    let mut stdout = child.stdout.take().unwrap();
    let mut out = read_until(&mut stdout, "tick");
    // Behind at the end: nothing read until the run has written its last
    // bytes and ended, then a moment more, well within the session's wait.
    touch(&go);
    harness::wait_log_tail(&root, "ro-flush", "LAST-BYTES");
    std::thread::sleep(Duration::from_millis(500));
    // Both read at once, as two clients would.
    let controller = std::thread::spawn(move || {
        let mut controlled = Vec::new();
        while let Ok(frame) = read_frame(&mut controller) {
            if let Frame::Output { bytes, .. } = frame {
                controlled.extend(bytes);
            }
        }
        controlled
    });
    stdout.read_to_end(&mut out).unwrap();
    let (code, stderr) = finish(child);
    let controlled = controller.join().unwrap();

    assert_eq!(code, Some(0), "the run ended; stderr: {stderr}");
    let recording = harness::recorded_output(&root, "ro-flush");
    for (who, got) in [("the watcher", &out), ("the controller", &controlled)] {
        assert!(
            got.ends_with(b"LAST-BYTES"),
            "{who} ends short of the run's last bytes: {:?}",
            String::from_utf8_lossy(&got[got.len().saturating_sub(80)..])
        );
        assert!(
            recording.ends_with(got),
            "{who} did not get the recording's end"
        );
    }
}
