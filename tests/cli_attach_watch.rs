//! Read-only viewers ("watchers") at the attach socket, against a real
//! sidecar: admitted from a pool of their own beside the controller, sent
//! the run's output, never holding or changing control.
#![cfg(unix)]

mod harness;

use harness::{Observation, SessionGuard, poll_until, read_events, tendr};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tempfile::TempDir;
use tendr::attach_proto::{
    Frame, FrameError, Mode, RejectClass, ResumePoint, Version, WatchPoint, read_frame, write_frame,
};
use tendr::recording::Geometry;

static SERIAL: Mutex<()> = Mutex::new(());

/// The most watchers a run admits.
const WATCHERS: usize = 8;

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

/// Connect, say hello in `version` and `mode`, and return the reply.
fn hello(root: &TempDir, session: &str, version: Version, mode: Mode) -> (UnixStream, Frame) {
    let mut stream = UnixStream::connect(socket(root, session)).unwrap();
    write_frame(&mut stream, &Frame::Hello { version, mode }).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let reply = read_frame(&mut stream).expect("the sidecar answers");
    (stream, reply)
}

/// A version 2 watcher, and where its output starts.
fn watch(root: &TempDir, session: &str) -> (UnixStream, WatchPoint) {
    match hello(root, session, Version::V2, Mode::Watch) {
        (stream, Frame::Watching(point)) => (stream, point),
        (_, other) => panic!("watch refused: {other:?}"),
    }
}

/// A version 2 controller, and the point it was accepted at.
fn control(root: &TempDir, session: &str, mode: Mode) -> (UnixStream, ResumePoint) {
    match hello(root, session, Version::V2, mode) {
        (stream, Frame::AcceptedV2(point)) => (stream, point),
        (_, other) => panic!("{mode:?} refused: {other:?}"),
    }
}

/// Read output frames until what was read contains `marker`; the mode
/// replay (unstamped) is read past.
fn read_until(stream: &mut UnixStream, marker: &str) -> String {
    let mut out = Vec::new();
    while !String::from_utf8_lossy(&out).contains(marker) {
        match read_frame(stream) {
            Ok(Frame::Output { bytes, .. } | Frame::Data(bytes)) => out.extend(bytes),
            other => panic!(
                "waiting for {marker:?} after {:?}, got {other:?}",
                String::from_utf8_lossy(&out)
            ),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Read `len` bytes of stamped output from `from`, checking each frame
/// starts where the last ended; the mode replay (unstamped) is read past.
fn read_stamped(mut stream: UnixStream, from: u64, len: u64) -> (UnixStream, Vec<u8>) {
    let mut out = Vec::new();
    while (out.len() as u64) < len {
        match read_frame(&mut stream) {
            Ok(Frame::Output { offset, bytes }) => {
                assert_eq!(
                    offset,
                    from + out.len() as u64,
                    "a frame skipped or repeated"
                );
                out.extend(bytes);
            }
            Ok(Frame::Data(_)) => {}
            other => panic!("after {} bytes: {other:?}", out.len()),
        }
    }
    (stream, out)
}

/// Read until the sidecar ends the connection, and fail if it does not: a
/// read that times out (under the deadline [`hello`] set) is not an end.
fn assert_ended(stream: &mut UnixStream, who: &str) {
    loop {
        match read_frame(stream) {
            Ok(_) | Err(FrameError::Malformed(_) | FrameError::Unknown(_)) => {}
            Err(FrameError::Io(e)) => {
                assert!(
                    !matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ),
                    "{who} was never closed: {e}"
                );
                return;
            }
        }
    }
}

/// Wait until `meta.json` names `expected` as the PTY's controller.
fn wait_control(root: &TempDir, session: &str, expected: &str) {
    let control = || {
        let meta: serde_json::Value = serde_json::from_slice(
            &std::fs::read(session_dir(root, session).join("meta.json")).ok()?,
        )
        .ok()?;
        meta["pty"]["control"].as_str().map(str::to_owned)
    };
    poll_until(
        Duration::from_secs(15),
        Duration::from_millis(50),
        || match control() {
            Some(c) if c == expected => Observation::Ready(()),
            other => Observation::Pending(format!("pty.control={other:?}")),
        },
    )
    .unwrap_or_else(|e| panic!("{session} never reached {expected}: {e}"));
}

/// The run's `pty.control_changed` events, `(trigger, epoch)` in order.
fn control_changes(root: &TempDir, session: &str) -> Vec<(String, u64)> {
    read_events(root, session)
        .into_iter()
        .filter(|e| e["kind"] == "pty.control_changed")
        .map(|e| {
            (
                e["data"]["trigger"].as_str().unwrap().to_owned(),
                e["data"]["epoch"].as_u64().unwrap(),
            )
        })
        .collect()
}

/// Wait until the run's control changes are `expected`: the sidecar records
/// them on their own thread.
fn assert_control_changes(root: &TempDir, session: &str, expected: &[(&str, u64)]) {
    poll_until(Duration::from_secs(15), Duration::from_millis(50), || {
        let changes = control_changes(root, session);
        let matches = changes.len() == expected.len()
            && changes
                .iter()
                .zip(expected)
                .all(|(c, e)| c.0 == e.0 && c.1 == e.1);
        if matches {
            Observation::Ready(())
        } else {
            Observation::Pending(format!("{changes:?}"))
        }
    })
    .unwrap_or_else(|e| panic!("control changes never became {expected:?}: {e}"));
}

/// Eight watchers and a controller share one run, each watcher sent the
/// controller's echoed input. A ninth watcher is refused as a full pool
/// (84) while the controller is still admitted, and the watchers leave no
/// control events.
#[test]
fn eight_watchers_and_a_controller_share_a_run_and_a_ninth_watcher_is_refused() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "watch-full");
    start(&root, "watch-full", "cat", &[]);

    let mut watchers: Vec<UnixStream> = (0..WATCHERS)
        .map(|_| watch(&root, "watch-full").0)
        .collect();
    let (_ninth, reply) = hello(&root, "watch-full", Version::V2, Mode::Watch);
    assert!(
        matches!(
            reply,
            Frame::Rejected {
                class: RejectClass::Runtime,
                ..
            }
        ),
        "a ninth watcher: {reply:?}"
    );

    let (mut controller, accepted) = control(&root, "watch-full", Mode::Attach);
    assert_eq!(accepted.epoch.get(), 1);
    write_frame(&mut controller, &Frame::Data(b"seen-by-all\n".to_vec())).unwrap();
    for watcher in &mut watchers {
        read_until(watcher, "seen-by-all");
    }
    wait_control(&root, "watch-full", "HumanControl");
    assert_control_changes(&root, "watch-full", &[("attach", 1)]);
}

/// A takeover retires the controller it supersedes and no watcher: each
/// is still attached and sent the new controller's output.
#[test]
fn a_takeover_leaves_every_watcher_attached_and_receiving() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "watch-takeover");
    start(&root, "watch-takeover", "cat", &[]);

    let mut watchers: Vec<UnixStream> = (0..3).map(|_| watch(&root, "watch-takeover").0).collect();
    let (mut first, _) = control(&root, "watch-takeover", Mode::Attach);
    let (mut taker, accepted) = control(&root, "watch-takeover", Mode::Takeover);
    assert_eq!(accepted.epoch.get(), 2);
    loop {
        match read_frame(&mut first).expect("the retired controller is told") {
            Frame::Retired(epoch) => {
                assert_eq!(epoch.get(), 2);
                break;
            }
            Frame::Output { .. } | Frame::Data(_) => {}
            other => panic!("unexpected {other:?}"),
        }
    }

    write_frame(&mut taker, &Frame::Data(b"after-takeover\n".to_vec())).unwrap();
    for watcher in &mut watchers {
        read_until(watcher, "after-takeover");
    }
    assert_control_changes(&root, "watch-takeover", &[("attach", 1), ("takeover", 2)]);
}

/// A watcher's keystrokes, resize and repaint are dropped by the sidecar:
/// the child, which logs every line it reads and answers `size` with its
/// terminal's size, gets only the controller's input, at the size the
/// watcher did not change.
#[test]
fn a_watchers_input_resize_and_repaint_never_reach_the_child() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "watch-input");
    let input = root.path().join("input.log");
    let script = format!(
        "stty -echo; while IFS= read -r line; do \
         if [ \"$line\" = size ]; then echo \"size:$(stty size)\"; \
         else printf '%s\\n' \"$line\" >> '{}'; fi; done",
        input.display()
    );
    start(&root, "watch-input", &script, &[]);

    let (mut watcher, _) = watch(&root, "watch-input");
    for frame in [
        Frame::Data(b"typed-by-watcher\n".to_vec()),
        Frame::Resize(Geometry::new(11, 33).unwrap()),
        Frame::Repaint,
        Frame::Data(b"size\n".to_vec()),
        Frame::Detach,
    ] {
        write_frame(&mut watcher, &frame).unwrap();
    }
    // The sidecar reads a connection's frames in order: by the end of this
    // one, it has handled all of them.
    assert_ended(&mut watcher, "the detached watcher");

    let (mut controller, _) = control(&root, "watch-input", Mode::Attach);
    write_frame(
        &mut controller,
        &Frame::Data(b"typed-by-controller\nsize\n".to_vec()),
    )
    .unwrap();
    let shown = read_until(&mut controller, "size:");
    let shown = read_until_line_end(&mut controller, shown);
    assert!(
        !shown.contains("11 33"),
        "the watcher resized the PTY: {shown:?}"
    );
    assert_eq!(
        shown.matches("size:").count(),
        1,
        "the controller's size request was answered once: {shown:?}"
    );
    let logged = poll_until(Duration::from_secs(10), Duration::from_millis(50), || {
        let logged = std::fs::read_to_string(&input).unwrap_or_default();
        if logged.contains("typed-by-controller") {
            Observation::Ready(logged)
        } else {
            Observation::Pending(logged)
        }
    })
    .unwrap();
    assert_eq!(logged, "typed-by-controller\n");
    assert_control_changes(&root, "watch-input", &[("attach", 1)]);
}

/// Read on until `shown` holds a whole line after its last `size:`.
fn read_until_line_end(stream: &mut UnixStream, mut shown: String) -> String {
    while !shown.rsplit("size:").next().unwrap_or("").contains('\n') {
        shown.push_str(&read_until(stream, "\n"));
    }
    shown
}

/// A watcher that stops reading falls a ring behind and is cut off; capture
/// records every byte, another watcher gets every byte, and the controller
/// keeps control and its output.
#[test]
fn a_watcher_a_ring_behind_is_cut_off_and_nothing_else_is() {
    // Far smaller than the flood, so the stalled watcher falls behind it,
    // and roomy enough that a reader briefly descheduled does not.
    const RING: u64 = 1024 * 1024;
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "watch-evict");
    let (go, end) = (root.path().join("go"), root.path().join("end"));
    let line = "0123456789abcdef".repeat(62);
    // About 3 MB, paced so a reader that keeps reading stays within the ring.
    let script = format!(
        "while [ ! -f '{go}' ]; do sleep 0.05; done; \
         i=0; while [ $i -lt 300 ]; do yes {line} | head -n 10; sleep 0.01; i=$((i+1)); done; \
         echo ALL-DONE; while [ ! -f '{end}' ]; do sleep 0.05; done",
        go = go.display(),
        end = end.display()
    );
    start(
        &root,
        "watch-evict",
        &script,
        &[("TENDR_TEST_OUTPUT_RING_BYTES", &RING.to_string())],
    );

    let (mut stalled, _) = watch(&root, "watch-evict");
    let (reader, reader_at) = watch(&root, "watch-evict");
    let (controller, accepted) = control(&root, "watch-evict", Mode::Attach);
    let total = (300 * 10 * (line.len() + 2) + "ALL-DONE\r\n".len()) as u64;
    let reading = std::thread::spawn(move || {
        read_stamped(reader, reader_at.offset, total - reader_at.offset)
    });
    let controlling = std::thread::spawn(move || {
        read_stamped(controller, accepted.offset, total - accepted.offset)
    });
    touch(&go);

    poll_until(Duration::from_secs(60), Duration::from_millis(100), || {
        if harness::logged_output(&root, "watch-evict").contains("ALL-DONE") {
            Observation::Ready(())
        } else {
            Observation::Pending("no ALL-DONE yet".to_owned())
        }
    })
    .unwrap();
    // Cut off while the run goes on: a connection the sidecar ended, not
    // one still waiting for output.
    assert_ended(&mut stalled, "the stalled watcher");
    let (_reader, read) = reading.join().unwrap();
    let (mut controller, controlled) = controlling.join().unwrap();
    wait_control(&root, "watch-evict", "HumanControl");
    write_frame(&mut controller, &Frame::Detach).unwrap();
    touch(&end);
    harness::wait_terminal(&root, "watch-evict");
    let recording = harness::recorded_output(&root, "watch-evict");

    assert_eq!(recording.len() as u64, total, "capture recorded every byte");
    assert!(
        read == recording[reader_at.offset as usize..],
        "the reading watcher's bytes are not the recording's"
    );
    assert!(
        controlled == recording[accepted.offset as usize..],
        "the controller's bytes are not the recording's"
    );
    assert_control_changes(&root, "watch-evict", &[("attach", 1), ("detach", 1)]);
}

/// The run's end shuts every watcher's connection, as it does the
/// controller's.
#[test]
fn the_runs_end_closes_every_watcher() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let _kill = SessionGuard::new(&root, "watch-end");
    let end = root.path().join("end");
    start(
        &root,
        "watch-end",
        &format!("while [ ! -f '{}' ]; do sleep 0.05; done", end.display()),
        &[],
    );

    let mut watchers: Vec<UnixStream> = (0..3).map(|_| watch(&root, "watch-end").0).collect();
    let (mut controller, _) = control(&root, "watch-end", Mode::Attach);
    touch(&end);
    for (i, watcher) in watchers.iter_mut().enumerate() {
        assert_ended(watcher, &format!("watcher {i}"));
    }
    assert_ended(&mut controller, "the controller");
    harness::wait_terminal(&root, "watch-end");
}
