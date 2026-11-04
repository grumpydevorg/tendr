//! Resuming a version 2 viewer at the attach socket, against a real sidecar:
//! the control rule (`InputArbiter::resume`) and the bytes (the output ring)
//! decided on one connection.
#![cfg(unix)]

mod harness;
use harness::Home;

use harness::{HANG, Observation, poll_until, read_events, tendr};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tendr::attach_proto::{
    Frame, Mode, RejectClass, ResumePoint, Version, read_frame, write_frame,
};
use tendr::model::pty_control::ControllerEpoch;

static SERIAL: Mutex<()> = Mutex::new(());

/// About 1 KB per line once the terminal adds CRs; long lines keep
/// output.log's per-line cost down.
fn line() -> String {
    "0123456789abcdef".repeat(62)
}

fn session_dir(root: &Home, session: &str) -> PathBuf {
    root.path()
        .join(format!(".tendr/sessions/default/{session}"))
}

/// Start a PTY session running `script` under `sh -c`, with `env` set for
/// the sidecar (and so the run), and wait for its attach socket.
fn start(root: &Home, session: &str, script: &str, env: &[(&str, &str)]) {
    let mut start = tendr(root);
    for (key, value) in env {
        start.env(key, value);
    }
    let go = root.path().join("go");
    let end = root.path().join("end");
    let output = start
        .args(["start", session, "--pty"])
        .arg("--env")
        .arg(format!("GO={}", go.display()))
        .arg("--env")
        .arg(format!("END={}", end.display()))
        .args(["--", "sh", "-c", script])
        .output()
        .unwrap();
    assert!(output.status.success(), "start {session}: {output:?}");
    harness::wait_running(root, session);
    socket(root, session);
}

/// Waits for the go file, then runs `body`, then waits for the end file.
fn gated(body: &str) -> String {
    format!(
        "while [ ! -f \"$GO\" ]; do sleep 0.05; done; {body}; \
         while [ ! -f \"$END\" ]; do sleep 0.05; done"
    )
}

fn touch(root: &Home, name: &str) {
    std::fs::write(root.path().join(name), b"").unwrap();
}

fn socket(root: &Home, session: &str) -> PathBuf {
    let dir = session_dir(root, session);
    poll_until(
        HANG,
        Duration::from_millis(50),
        || match tendr::attach_proto::read_sock_path(&dir) {
            Some(path) => Observation::Ready(path),
            None => Observation::Pending("no attach socket yet".to_owned()),
        },
    )
    .unwrap()
}

/// Connect and send `first` (a hello or a resume); the sidecar's reply.
fn open(root: &Home, session: &str, first: &Frame) -> (UnixStream, Frame) {
    let mut stream = UnixStream::connect(socket(root, session)).unwrap();
    write_frame(&mut stream, first).unwrap();
    stream.set_read_timeout(Some(HANG)).unwrap();
    let reply = read_frame(&mut stream).expect("the sidecar answers");
    (stream, reply)
}

fn hello(root: &Home, session: &str, version: Version, mode: Mode) -> (UnixStream, Frame) {
    open(root, session, &Frame::Hello { version, mode })
}

/// A version 2 attach, and the point it was accepted at.
fn attach_v2(root: &Home, session: &str) -> (UnixStream, ResumePoint) {
    match hello(root, session, Version::V2, Mode::Attach) {
        (stream, Frame::AcceptedV2(point)) => (stream, point),
        (_, other) => panic!("attach refused: {other:?}"),
    }
}

fn resume(root: &Home, session: &str, point: ResumePoint) -> (UnixStream, Frame) {
    open(
        root,
        session,
        &Frame::Resume {
            point,
            version: Version::V2,
        },
    )
}

fn epoch(n: u64) -> ControllerEpoch {
    ControllerEpoch::new(n)
}

fn assert_refused(reply: &Frame, class: RejectClass) {
    assert!(
        matches!(reply, Frame::Rejected { class: c, .. } if *c == class),
        "expected a {class:?} refusal, got {reply:?}"
    );
}

/// Read stamped output from `from` until it ends with `marker` (or, with an
/// empty marker, `len` bytes), checking each frame starts where the last
/// ended. Any unstamped frame fails: only the run's output may follow.
fn read_stamped(stream: &mut UnixStream, from: u64, marker: &[u8], len: usize) -> Vec<u8> {
    let mut out = Vec::new();
    while if marker.is_empty() {
        out.len() < len
    } else {
        !out.ends_with(marker)
    } {
        match read_frame(stream).expect("the viewer gets its output") {
            Frame::Output { offset, bytes } => {
                assert_eq!(
                    offset,
                    from + out.len() as u64,
                    "a frame skipped or repeated"
                );
                out.extend(bytes);
            }
            other => panic!("only stamped output follows, not {other:?}"),
        }
    }
    out
}

/// Read until the sidecar closes the connection.
fn drain(stream: &mut UnixStream) {
    let _ = stream.set_read_timeout(Some(HANG));
    while read_frame(stream).is_ok() {}
}

fn control(root: &Home, session: &str) -> Option<String> {
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(session_dir(root, session).join("meta.json")).ok()?)
            .ok()?;
    meta["pty"]["control"].as_str().map(str::to_owned)
}

fn wait_control(root: &Home, session: &str, expected: &str) {
    poll_until(HANG, Duration::from_millis(50), || {
        match control(root, session) {
            Some(c) if c == expected => Observation::Ready(()),
            other => Observation::Pending(format!("pty.control={other:?}")),
        }
    })
    .unwrap_or_else(|e| panic!("{session} never reached {expected}: {e}"));
}

/// Wait until the run's `pty.control_changed` events, `(trigger, epoch)` in
/// order, are `expected`: the sidecar records them on their own thread.
fn assert_control_changes(root: &Home, session: &str, expected: &[(&str, u64)]) {
    poll_until(HANG, Duration::from_millis(50), || {
        let changes: Vec<(String, u64)> = read_events(root, session)
            .into_iter()
            .filter(|e| e["kind"] == "pty.control_changed")
            .map(|e| {
                (
                    e["data"]["trigger"].as_str().unwrap().to_owned(),
                    e["data"]["epoch"].as_u64().unwrap(),
                )
            })
            .collect();
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

/// The text at the end of output.log, with the lines a marker may straddle
/// joined.
fn log_tail(root: &Home, session: &str) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let mut tail = Vec::new();
    if let Ok(mut file) = std::fs::File::open(session_dir(root, session).join("output.log")) {
        let _ = file.seek(SeekFrom::End(-16384));
        let _ = file.read_to_end(&mut tail);
    }
    tail.split(|&b| b == b'\n')
        .filter_map(|l| serde_json::from_slice::<serde_json::Value>(l).ok())
        .filter_map(|l| l["content"].as_str().map(str::to_owned))
        .collect()
}

/// Wait until capture has written `marker` to output.log.
fn wait_captured(root: &Home, session: &str, marker: &str) {
    poll_until(HANG, Duration::from_millis(100), || {
        let text = log_tail(root, session);
        if text.contains(marker) {
            Observation::Ready(())
        } else {
            Observation::Pending(text)
        }
    })
    .unwrap_or_else(|e| panic!("{marker} never captured: {e}"));
}

/// End the run and return its whole output, as recorded.
fn finish(root: &Home, session: &str) -> Vec<u8> {
    touch(root, "end");
    harness::wait_terminal(root, session);
    harness::recorded_output(root, session)
}

fn run_id(root: &Home, session: &str) -> String {
    let meta: serde_json::Value = serde_json::from_slice(
        &std::fs::read(session_dir(root, session).join("meta.json")).unwrap(),
    )
    .unwrap();
    meta["run_id"].as_str().unwrap().to_owned()
}

/// The half-open case: a viewer stops reading mid-flood with its connection
/// still open, and a new connection resumes from the last byte it read. The
/// two together got every byte of the run's output once, in order, and the
/// old connection's control passed to the new one at the next epoch, as a
/// resume, not a takeover, with no release in between.
#[test]
fn a_half_open_resume_under_a_running_flood_has_no_gap_and_no_duplicate() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    // About 3 MB over at least 3 s. Half way it ticks until the test has
    // resumed, so output is still being appended when the resume is
    // answered however long load makes the test take to get there.
    let resumed = root.path().join("resumed");
    let flood = format!(
        "i=0; while [ $i -lt 300 ]; do yes {} | head -n 10; sleep 0.01; i=$((i+1)); \
         if [ $i -eq 150 ]; then while [ ! -f '{}' ]; do echo tick; sleep 0.05; done; fi; \
         done; echo FLOOD-DONE",
        line(),
        resumed.display()
    );
    start(&root, "resume-flood", &gated(&flood), &[]);

    let (mut old, accepted) = attach_v2(&root, "resume-flood");
    assert_eq!(accepted.epoch, epoch(1));
    assert_eq!(accepted.run_id.to_string(), run_id(&root, "resume-flood"));
    touch(&root, "go");
    let before = read_stamped(&mut old, accepted.offset, b"", 300_000);
    // The old connection stays open, unread, as after a dropped link.
    let reached = accepted.offset + before.len() as u64;

    let (mut new, reply) = resume(
        &root,
        "resume-flood",
        ResumePoint {
            offset: reached,
            ..accepted
        },
    );
    assert_eq!(
        reply,
        Frame::Resumed(ResumePoint {
            run_id: accepted.run_id,
            epoch: epoch(2),
            offset: reached,
        })
    );
    assert!(
        !log_tail(&root, "resume-flood").contains("FLOOD-DONE"),
        "the flood was still running when the resume was answered"
    );
    touch(&root, "resumed");
    let after = read_stamped(&mut new, reached, b"FLOOD-DONE\r\n", 0);
    // The old connection was retired.
    drain(&mut old);
    // meta.json shows AgentControl before the attach is recorded too.
    wait_control(&root, "resume-flood", "HumanControl");
    write_frame(&mut new, &Frame::Detach).unwrap();
    wait_control(&root, "resume-flood", "AgentControl");

    let recording = finish(&root, "resume-flood");
    let joined = [before, after].concat();
    assert!(
        joined == recording[accepted.offset as usize..],
        "the two connections got {} bytes, not the {} recorded",
        joined.len(),
        recording.len() as u64 - accepted.offset
    );
    // The old connection's late disconnect released nothing.
    assert_control_changes(
        &root,
        "resume-flood",
        &[("attach", 1), ("resume", 2), ("detach", 2)],
    );
}

/// A viewer that resumes from an offset the ring no longer holds is told
/// exactly what it holds, gets the mode replay and the output from the
/// head; from the ring's oldest byte it resumes, and one byte earlier it is
/// gone again.
#[test]
fn gone_at_the_ring_edge_names_the_exact_span() {
    const RING: u64 = 64 * 1024;
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    // Bracketed paste on, then about 2 MB: far past the ring and any socket
    // buffer, so a viewer that never reads is evicted.
    let script = format!(
        "printf '\\033[?2004h'; {}",
        gated(&format!("yes {} | head -n 2000; echo ALL-DONE", line()))
    );
    start(
        &root,
        "resume-gone",
        &script,
        &[("TENDR_TEST_OUTPUT_RING_BYTES", &RING.to_string())],
    );
    let (_evicted, accepted) = attach_v2(&root, "resume-gone");
    wait_control(&root, "resume-gone", "HumanControl");
    touch(&root, "go");
    wait_captured(&root, "resume-gone", "ALL-DONE");
    // Evicted and disconnected, it lost control.
    wait_control(&root, "resume-gone", "AgentControl");

    // Capture logs a chunk just before it appends it to the ring, so the
    // ring can lag the log for a moment: resume, each time from the last
    // answer, until the head is the whole output.
    let total = (b"\x1b[?2004h".len() + 2000 * (line().len() + 2) + b"ALL-DONE\r\n".len()) as u64;
    let mut asked = accepted;
    let (mut gone, point, oldest) = poll_until(HANG, Duration::from_millis(20), || {
        let (stream, reply) = resume(&root, "resume-gone", asked);
        let Frame::Gone { point, oldest } = reply else {
            panic!("expected gone, got {reply:?}");
        };
        assert_eq!(
            (point.run_id, point.epoch),
            (asked.run_id, asked.epoch.next().unwrap())
        );
        if point.offset == total {
            return Observation::Ready((stream, point, oldest));
        }
        asked = ResumePoint { offset: 0, ..point };
        Observation::Pending(format!("the head at {} of {total}", point.offset))
    })
    .unwrap_or_else(|e| panic!("the ring never held the whole output: {e}"));
    let head = point.offset;
    assert_eq!(head - oldest, RING, "a full ring");
    assert_eq!(
        read_frame(&mut gone).unwrap(),
        Frame::Data(b"\x1b[?2004h".to_vec()),
        "the mode replay, unstamped"
    );

    let (mut edge, reply) = resume(
        &root,
        "resume-gone",
        ResumePoint {
            offset: oldest,
            ..point
        },
    );
    assert_eq!(
        reply,
        Frame::Resumed(ResumePoint {
            epoch: point.epoch.next().unwrap(),
            offset: oldest,
            ..point
        })
    );
    let held = read_stamped(&mut edge, oldest, b"", RING as usize);

    let (_, reply) = resume(
        &root,
        "resume-gone",
        ResumePoint {
            epoch: point.epoch.next().unwrap(),
            offset: oldest - 1,
            ..point
        },
    );
    assert_eq!(
        reply,
        Frame::Gone {
            point: ResumePoint {
                epoch: point.epoch.next().unwrap().next().unwrap(),
                ..point
            },
            oldest
        }
    );

    let recording = finish(&root, "resume-gone");
    assert_eq!(head, recording.len() as u64, "the head is all the output");
    assert!(held == recording[oldest as usize..], "the ring's bytes");
}

/// A human took control after the lost connection's epoch: the resume is
/// refused (81), and the taker keeps control.
#[test]
fn a_resume_after_a_human_took_control_is_refused() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start(&root, "resume-taken", "cat", &[]);

    let (_lost, accepted) = attach_v2(&root, "resume-taken");
    let (_taker, reply) = hello(&root, "resume-taken", Version::V2, Mode::Takeover);
    assert!(matches!(reply, Frame::AcceptedV2(p) if p.epoch == epoch(2)));

    let (_, reply) = resume(&root, "resume-taken", accepted);
    assert_refused(&reply, RejectClass::Control);
    assert_control_changes(&root, "resume-taken", &[("attach", 1), ("takeover", 2)]);
}

/// An agent push that came and went while the connection was lost does not
/// block the resume; one still running does (81).
#[test]
fn a_finished_push_lets_the_resume_through_and_a_running_one_does_not() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start(&root, "resume-push", "cat", &[]);

    // The connection is lost and its end noticed: control is released.
    // meta.json shows AgentControl before the attach is recorded too, so the
    // release is only seen once the attach has been.
    let (mut lost, accepted) = attach_v2(&root, "resume-push");
    wait_control(&root, "resume-push", "HumanControl");
    write_frame(&mut lost, &Frame::Detach).unwrap();
    wait_control(&root, "resume-push", "AgentControl");

    // A push that is still running holds the terminal.
    let (mut push, reply) = hello(&root, "resume-push", Version::V1, Mode::Push);
    assert_eq!(reply, Frame::Accepted(epoch(2)));
    let (_, reply) = resume(&root, "resume-push", accepted);
    assert_refused(&reply, RejectClass::Control);

    // Once it has finished, the resume goes through, after its epoch.
    write_frame(&mut push, &Frame::Data(b"pushed\n".to_vec())).unwrap();
    write_frame(&mut push, &Frame::Detach).unwrap();
    assert!(matches!(
        read_frame(&mut push).unwrap(),
        Frame::InputDone(_)
    ));
    let (mut resumed, reply) = resume(&root, "resume-push", accepted);
    assert!(
        matches!(reply, Frame::Resumed(p) if p.epoch == epoch(3)),
        "{reply:?}"
    );
    wait_control(&root, "resume-push", "HumanControl");
    write_frame(&mut resumed, &Frame::Detach).unwrap();
}

/// A resume names its run; another run's is refused (81), and the holder
/// keeps control.
#[test]
fn a_resume_from_another_run_is_refused() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start(&root, "resume-stale", "cat", &[]);

    let (_holder, accepted) = attach_v2(&root, "resume-stale");
    let (_, reply) = resume(
        &root,
        "resume-stale",
        ResumePoint {
            run_id: tendr::model::ids::RunId::new(),
            ..accepted
        },
    );
    assert_refused(&reply, RejectClass::Control);
    assert_control_changes(&root, "resume-stale", &[("attach", 1)]);
}

/// A resumed client is an ordinary controller: a later takeover retires it,
/// and the event log records that as a takeover of the resumed epoch, which
/// is what a client cut off without `MSG_RETIRED` looks for.
#[test]
fn a_resumed_client_taken_over_is_retired_as_by_any_takeover() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    start(&root, "resume-retire", "cat", &[]);

    let (_lost, accepted) = attach_v2(&root, "resume-retire");
    let (mut resumed, reply) = resume(&root, "resume-retire", accepted);
    assert!(matches!(reply, Frame::Resumed(p) if p.epoch == epoch(2)));
    let (_taker, reply) = hello(&root, "resume-retire", Version::V2, Mode::Takeover);
    assert!(matches!(reply, Frame::AcceptedV2(p) if p.epoch == epoch(3)));

    assert_eq!(read_frame(&mut resumed).unwrap(), Frame::Retired(epoch(3)));
    assert_control_changes(
        &root,
        "resume-retire",
        &[("attach", 1), ("resume", 2), ("takeover", 3)],
    );
}
