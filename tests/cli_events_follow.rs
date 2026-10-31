//! `tendr events --follow` — poll-based live tailing with warm starts
//! (spec §5.1, slice 2 plan scope items 1–2).

mod harness;

use std::io::Write as _;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use harness::{tendr, wait_running, wait_terminal};
use predicates::prelude::*;
use tempfile::TempDir;

/// `--ready-file` is a follow-only readiness signal. Accepting it on a one-shot
/// replay would advertise a readiness contract the command never fulfils, so it
/// is rejected at CLI validation.
#[test]
fn ready_file_requires_follow() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let ready = root.path().join("ready");
    tendr(&root)
        .args(["events", "--ready-file", ready.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--follow"));
    assert!(
        !ready.exists(),
        "no readiness file when the invocation is rejected"
    );
}

/// The readiness handshake end-to-end: the follower publishes its ready-file
/// only after its `--from-now` baseline is established, so an event emitted
/// strictly after readiness is guaranteed live (never classified as history).
#[test]
fn follow_from_now_ready_then_surfaces_live_event() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "live", "--", "sleep", "5"])
        .assert()
        .success();
    wait_running(&root, "live");

    let follower = harness::ReadyFollower::spawn(&root, "events", &["--follow", "--from-now"]);
    // Baseline proven established: a live emit must now surface downstream.
    tendr(&root)
        .args(["emit", "--kind", "hook.live_probe", "--session", "live"])
        .assert()
        .success();

    let rec = follower.read_until(Duration::from_secs(10), |r| r["kind"] == "hook.live_probe");
    assert_eq!(rec["session"], "live");
    // --from-now still skips the pre-existing lifecycle history.
    assert!(
        follower
            .records()
            .iter()
            .all(|r| r["kind"] != "run.starting"),
        "history must be skipped under --from-now"
    );
    follower.stop();
}

/// Follower children + sleeper sessions are timing-sensitive; serialize
/// like cli_watch.rs does so parallel load can't blow the poll budgets.
static SERIAL: Mutex<()> = Mutex::new(());

fn spawn_events(root: &TempDir, args: &[&str]) -> Child {
    let bin = assert_cmd::cargo::cargo_bin("tendr");
    Command::new(bin)
        .arg("events")
        .args(args)
        .env("HOME", root.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn tendr events")
}

fn newest_segment(root: &TempDir, session: &str) -> std::path::PathBuf {
    let events_dir = root
        .path()
        .join(format!(".tendr/sessions/default/{session}/events"));
    let mut segs: Vec<_> = std::fs::read_dir(&events_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    segs.sort();
    segs.pop().expect("at least one segment")
}

#[test]
fn follow_from_now_replays_later_discovered_sessions_from_start() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "old", "--", "echo", "old-hi"])
        .assert()
        .success();
    wait_terminal(&root, "old");

    let follower = harness::ReadyFollower::spawn(&root, "events", &["--follow", "--from-now"]);

    tendr(&root)
        .args(["start", "fresh", "--", "echo", "fresh-hi"])
        .assert()
        .success();
    wait_terminal(&root, "fresh");
    // The last of fresh's lifecycle events proves the whole replay arrived.
    follower.read_until(Duration::from_secs(10), |r| {
        r["session"] == "fresh" && r["kind"] == "run.exited"
    });

    let records = follower.records();
    assert!(
        records.iter().all(|r| r["session"] != "old"),
        "pre-existing session history must be skipped, got: {records:?}"
    );
    let fresh_kinds: Vec<&str> = records
        .iter()
        .filter(|r| r["session"] == "fresh")
        .map(|r| r["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        fresh_kinds,
        ["run.starting", "run.started", "run.exited"],
        "later-discovered sessions replay from their start"
    );
    follower.stop();
}

#[test]
fn follow_replays_history_then_streams_new_events() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "s1", "--", "sleep", "5"])
        .assert()
        .success();
    wait_running(&root, "s1");

    // --follow (no --from-now): the ready-file is published only after the
    // history replay is flushed, so the replay is already downstream once
    // spawn returns.
    let follower = harness::ReadyFollower::spawn(&root, "events", &["--follow"]);

    tendr(&root)
        .args(["emit", "--kind", "test.after_replay", "--session", "s1"])
        .assert()
        .success();
    follower.read_until(Duration::from_secs(10), |r| {
        r["kind"] == "test.after_replay"
    });

    let records = follower.records();
    let kinds: Vec<&str> = records
        .iter()
        .map(|r| r["kind"].as_str().unwrap())
        .collect();
    assert!(
        kinds.starts_with(&["run.starting", "run.started"]),
        "history replays first, got: {kinds:?}"
    );
    assert!(
        kinds.contains(&"test.after_replay"),
        "live events stream after replay, got: {kinds:?}"
    );
    follower.stop();
}

#[test]
fn follow_output_is_merge_ordered_by_ts_writer_seq() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "s1", "--", "sleep", "5"])
        .assert()
        .success();
    wait_running(&root, "s1");
    // A burst of events from distinct writers, all before the follower's
    // next poll — they arrive in one batch and must come out merge-ordered.
    for i in 0..5 {
        tendr(&root)
            .args([
                "emit",
                "--kind",
                &format!("test.burst{i}"),
                "--session",
                "s1",
            ])
            .assert()
            .success();
    }

    // The burst was emitted before follow began, so it is replayed as history;
    // the last burst event proves the whole batch is downstream.
    let follower = harness::ReadyFollower::spawn(&root, "events", &["--follow"]);
    follower.read_until(Duration::from_secs(10), |r| r["kind"] == "test.burst4");
    let records = follower.records();
    follower.stop();

    assert!(records.len() >= 7, "lifecycle + burst, got: {records:?}");
    let keys: Vec<(String, String, u64)> = records
        .iter()
        .map(|r| {
            (
                r["ts"].as_str().unwrap().to_owned(),
                r["writer"].as_str().unwrap().to_owned(),
                r["seq"].as_u64().unwrap(),
            )
        })
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted, "batch output is (ts, writer, seq)-ordered");
}

#[test]
fn follow_picks_up_new_segments() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "s1", "--", "sleep", "5"])
        .assert()
        .success();
    wait_running(&root, "s1");

    let follower = harness::ReadyFollower::spawn(&root, "events", &["--follow"]);

    // A new, lexicographically-later segment appears (tendr does not rotate
    // segments yet, but multi-segment logs are legal). Its events must stream.
    let seg = newest_segment(&root, "s1");
    let first_line = std::fs::read_to_string(&seg)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_owned();
    let mut event: serde_json::Value = serde_json::from_str(&first_line).unwrap();
    event["kind"] = "test.from_new_segment".into();
    let new_seg = seg.with_file_name("zzzz-pickup.jsonl");
    let mut f = std::fs::File::create(&new_seg).unwrap();
    writeln!(f, "{}", serde_json::to_string(&event).unwrap()).unwrap();
    drop(f);

    follower.read_until(Duration::from_secs(10), |r| {
        r["kind"] == "test.from_new_segment"
    });
    follower.stop();
}

#[test]
fn follow_strict_exits_65_on_first_observed_parse_skip() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "s1", "--", "sleep", "5"])
        .assert()
        .success();
    wait_running(&root, "s1");

    // This follower is expected to exit on its own (code 65), so it keeps a raw
    // child handle rather than ReadyFollower; the ready-file still replaces the
    // baseline sleep.
    let ready = root.path().join("strict-ready");
    let mut child = spawn_events(
        &root,
        &[
            "--follow",
            "--strict",
            "--ready-file",
            ready.to_str().unwrap(),
        ],
    );
    harness::wait_ready_file(&ready, Duration::from_secs(10));

    // A complete-but-unparseable line lands in the newest segment.
    let seg = newest_segment(&root, "s1");
    let mut f = std::fs::OpenOptions::new().append(true).open(&seg).unwrap();
    f.write_all(b"{\"v\":1,\"torn\n").unwrap();
    drop(f);

    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "--strict follower must exit on the parse skip"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(status.code(), Some(65));
}

/// Regression: production flushes the initial replay *before* creating the
/// ready-file, so `ReadyFollower` must drain stdout throughout the readiness
/// wait. A replay larger than the OS pipe buffer would otherwise deadlock — the
/// follower blocks flushing stdout, never creates the ready-file, and spawn
/// waits until its timeout.
#[test]
fn ready_follower_survives_replay_larger_than_pipe_buffer() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "big", "--", "sleep", "30"])
        .assert()
        .success();
    wait_running(&root, "big");

    // ~300 KB of history: 20 events x ~15 KB inline data (under the 16 KB cap),
    // well past any OS pipe buffer (64 KB on Linux, smaller elsewhere).
    let payload = format!("{{\"blob\":\"{}\"}}", "x".repeat(15 * 1024));
    for i in 0..20 {
        tendr(&root)
            .args([
                "emit",
                "--kind",
                &format!("test.bulk{i}"),
                "--session",
                "big",
                "--data",
                &payload,
            ])
            .assert()
            .success();
    }

    // The whole ~300 KB replay is flushed before the ready-file; spawn must
    // drain stdout while waiting, or this deadlocks. The pre-readiness replay
    // is preserved, so the last bulk event is still observable via read_until.
    let follower = harness::ReadyFollower::spawn(&root, "events", &["--follow"]);
    follower.read_until(Duration::from_secs(10), |r| r["kind"] == "test.bulk19");
    follower.stop();
}
