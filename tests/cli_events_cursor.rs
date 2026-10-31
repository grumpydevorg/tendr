//! `tendr events --cursors` / `--from-cursor` — Kubernetes semantics on
//! files (spec §5.2, slice 2 plan scope items 3–4, 6).

mod harness;

use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine as _;
use harness::{tendr, wait_running, wait_terminal};
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

fn parse_ndjson(stdout: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("NDJSON line parses"))
        .collect()
}

fn events_dir(root: &TempDir, session: &str) -> std::path::PathBuf {
    root.path()
        .join(format!(".tendr/sessions/default/{session}/events"))
}

fn segments(root: &TempDir, session: &str) -> Vec<std::path::PathBuf> {
    let mut segs: Vec<_> = std::fs::read_dir(events_dir(root, session))
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    segs.sort();
    segs
}

/// Split the session's single segment after `keep` lines: the tail moves to
/// a new, lexicographically-later segment (tendr does not rotate segments yet,
/// but multi-segment logs are legal — names are permanent identities).
fn split_segment(root: &TempDir, session: &str, keep: usize) {
    let segs = segments(root, session);
    assert_eq!(segs.len(), 1, "expected a single segment to split");
    let content = std::fs::read_to_string(&segs[0]).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert!(keep < lines.len(), "split point inside the segment");
    let head = lines[..keep].join("\n") + "\n";
    let tail = lines[keep..].join("\n") + "\n";
    std::fs::write(&segs[0], head).unwrap();
    std::fs::write(segs[0].with_file_name("zzzz-second.jsonl"), tail).unwrap();
}

fn decode_token(token: &str) -> serde_json::Value {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token)
        .expect("cursor token is URL-safe base64");
    serde_json::from_slice(&bytes).expect("cursor token payload is JSON")
}

fn ids(records: &[serde_json::Value]) -> Vec<String> {
    records
        .iter()
        .filter(|r| r["kind"] != "cursor.bookmark")
        .map(|r| {
            r["id"]
                .as_str()
                .expect("stored records carry ids")
                .to_owned()
        })
        .collect()
}

#[test]
fn batch_mode_emits_final_bookmark_and_resume_is_empty_then_exact() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "s1", "--", "echo", "hi"])
        .assert()
        .success();
    wait_terminal(&root, "s1");

    let output = tendr(&root).args(["events", "--cursors"]).output().unwrap();
    assert!(output.status.success());
    let records = parse_ndjson(&output.stdout);

    let bookmark = records.last().expect("some output");
    assert_eq!(
        bookmark["kind"], "cursor.bookmark",
        "batch mode ends with a resumable bookmark"
    );
    assert_eq!(bookmark["derived"], true);
    // Read-time record: exactly kind/ts/cursor/derived, no stored identity.
    let keys: Vec<&str> = bookmark
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["cursor", "derived", "kind", "ts"]);

    // Resuming from the final bookmark: nothing left.
    let token = bookmark["cursor"].as_str().unwrap().to_owned();
    let resumed = tendr(&root)
        .args(["events", "--from-cursor", &token])
        .output()
        .unwrap();
    assert!(resumed.status.success());
    assert!(
        resumed.stdout.is_empty(),
        "fully-consumed cursor resumes to nothing"
    );

    // New events after the bookmark: resume yields exactly those.
    tendr(&root)
        .args(["emit", "--kind", "test.post1", "--session", "s1"])
        .assert()
        .success();
    tendr(&root)
        .args(["emit", "--kind", "test.post2", "--session", "s1"])
        .assert()
        .success();
    let resumed = tendr(&root)
        .args(["events", "--from-cursor", &token])
        .output()
        .unwrap();
    let resumed = parse_ndjson(&resumed.stdout);
    let kinds: Vec<&str> = resumed
        .iter()
        .map(|r| r["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["test.post1", "test.post2"]);
}

#[test]
fn mid_stream_bookmark_resumes_remainder_exactly_across_segments() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "s1", "--", "echo", "hi"])
        .assert()
        .success();
    wait_terminal(&root, "s1");
    // 3 lifecycle + 104 emits = 107 stored records.
    for i in 0..104 {
        tendr(&root)
            .args([
                "emit",
                "--kind",
                &format!("test.n{i:03}"),
                "--session",
                "s1",
            ])
            .assert()
            .success();
    }
    // Two segments, split before the 100-record bookmark point so the
    // cursor spans both files.
    split_segment(&root, "s1", 53);

    let output = tendr(&root).args(["events", "--cursors"]).output().unwrap();
    assert!(output.status.success());
    let records = parse_ndjson(&output.stdout);

    // A bookmark interleaves after every 100 records, plus the final one.
    let bookmark_positions: Vec<usize> = records
        .iter()
        .enumerate()
        .filter(|(_, r)| r["kind"] == "cursor.bookmark")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        bookmark_positions.len(),
        2,
        "one cadence bookmark + one final bookmark, got: {bookmark_positions:?}"
    );
    assert_eq!(
        bookmark_positions[0], 100,
        "cadence bookmark lands after the 100th record"
    );

    // The mid-stream cursor names both segments.
    let mid = &records[bookmark_positions[0]];
    let token = mid["cursor"].as_str().unwrap().to_owned();
    let payload = decode_token(&token);
    assert_eq!(payload["v"], 1);
    let streams: Vec<&str> = payload["s"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s[0].as_str().unwrap())
        .collect();
    assert_eq!(streams.len(), 2, "cursor covers both segments: {streams:?}");

    // Exactness: resume replays precisely the records after the bookmark —
    // nothing twice, nothing dropped.
    let expected = ids(&records[bookmark_positions[0] + 1..]);
    assert_eq!(expected.len(), 7);
    let resumed = tendr(&root)
        .args(["events", "--from-cursor", &token])
        .output()
        .unwrap();
    assert!(resumed.status.success());
    let got = ids(&parse_ndjson(&resumed.stdout));
    assert_eq!(got, expected);
}

#[test]
fn follow_cursors_bookmarks_within_5s_of_idle() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "idle", "--", "sleep", "8"])
        .assert()
        .success();
    wait_running(&root, "idle");

    let bin = assert_cmd::cargo::cargo_bin("tendr");
    let mut child = Command::new(bin)
        .args(["events", "--follow", "--from-now", "--cursors"])
        .env("HOME", root.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The idle follower emits a cursor.bookmark after ~5s of idleness. Read its
    // stdout until that bookmark arrives (with a generous outer deadline) rather
    // than sleeping a fixed window — a loaded runner can take longer than a fixed
    // nap to fire the wall-clock idle timer.
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut bookmark: Option<serde_json::Value> = None;
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                    if v["kind"] == "cursor.bookmark" {
                        bookmark = Some(v);
                        break;
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();

    let bookmark =
        bookmark.expect("an idle follower should emit a cursor.bookmark within the deadline");
    assert!(bookmark["derived"] == true);
    assert!(bookmark.get("id").is_none());
}

#[test]
fn cursor_gone_exits_44_with_structured_stderr() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "s1", "--", "echo", "hi"])
        .assert()
        .success();
    wait_terminal(&root, "s1");

    let output = tendr(&root).args(["events", "--cursors"]).output().unwrap();
    let records = parse_ndjson(&output.stdout);
    let token = records.last().unwrap()["cursor"]
        .as_str()
        .unwrap()
        .to_owned();

    // The cursor's segment file disappears (e.g. pruned).
    let segs = segments(&root, "s1");
    std::fs::remove_file(&segs[0]).unwrap();

    let output = tendr(&root)
        .args(["events", "--from-cursor", &token])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(44));
    assert!(output.stdout.is_empty(), "never a silent restart from zero");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let err: serde_json::Value = serde_json::from_str(stderr.trim()).expect("structured stderr");
    assert_eq!(err["error"], "cursor_gone");
    let gone: Vec<&str> = err["gone"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g.as_str().unwrap())
        .collect();
    assert!(!gone.is_empty());
    assert!(
        gone.iter().all(|g| g.starts_with("default/s1/events/")),
        "gone names segment relpaths: {gone:?}"
    );
    assert!(err["recover"].is_string());
}

#[test]
fn unparseable_or_future_version_tokens_are_cursor_gone() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    for token in [
        "definitely-not-a-cursor",
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"v":2,"s":[["default/s1/events/a.jsonl",0]]}"#),
    ] {
        let output = tendr(&root)
            .args(["events", "--from-cursor", token])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(44), "token: {token}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        let err: serde_json::Value =
            serde_json::from_str(stderr.trim()).expect("structured stderr");
        assert_eq!(err["error"], "cursor_gone");
        assert_eq!(err["gone"][0].as_str().unwrap(), token);
    }
}

#[test]
fn cursors_never_cover_output_log() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "s1", "--", "echo", "logged-line"])
        .assert()
        .success();
    wait_terminal(&root, "s1");

    let output = tendr(&root)
        .args(["events", "--include-logs", "--cursors"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let records = parse_ndjson(&output.stdout);
    assert!(
        records.iter().any(|r| r["kind"] == "log.stdout"),
        "log projection ran"
    );
    let token = records.last().unwrap()["cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let payload = decode_token(&token);
    assert!(
        payload["s"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s[0].as_str().unwrap().contains("/events/")),
        "cursors cover event segments only: {payload}"
    );

    // Resume + --include-logs: log projection restarts at the resume
    // wall-clock — historical log lines are not replayed.
    let resumed = tendr(&root)
        .args(["events", "--from-cursor", &token, "--include-logs"])
        .output()
        .unwrap();
    assert!(resumed.status.success());
    let resumed = parse_ndjson(&resumed.stdout);
    assert!(
        resumed
            .iter()
            .all(|r| !r["kind"].as_str().unwrap().starts_with("log.")),
        "no historical log replay after --from-cursor: {resumed:?}"
    );
}
