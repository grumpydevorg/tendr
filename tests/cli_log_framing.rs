//! Pipe capture framing (`--log-delimiter`, bounded records) and its
//! read-time collapse, end to end through a real sidecar. The child is the
//! portable fake program, so these run on Windows too.

mod harness;
use harness::Home;

use harness::{
    DeadlineAssertExt, HANG, Observation, OpenGate, fake_program, poll_until, tendr, wait_terminal,
};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

/// A fake child running `actions`, and the gates it waits on. Keep it alive
/// while the session runs.
struct Child {
    dir: tempfile::TempDir,
}

impl Child {
    fn new(actions: &str) -> Self {
        Self {
            dir: fake_program("progress", actions),
        }
    }

    fn program(&self) -> String {
        let exe = format!("progress{}", std::env::consts::EXE_SUFFIX);
        self.dir.path().join(exe).to_str().unwrap().to_owned()
    }

    fn gate(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

fn log_path(root: &Home, session: &str) -> PathBuf {
    root.path()
        .join(format!(".tendr/sessions/default/{session}/output.log"))
}

/// Every record in a session's `output.log` so far, as stored; none while
/// the file does not exist yet.
fn records(root: &Home, session: &str) -> Vec<Value> {
    std::fs::read_to_string(log_path(root, session))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Wait for a session's run to end, which must be a clean exit: anything
/// else means the child did not run all its actions.
fn wait_exited_ok(root: &Home, session: &str) {
    let meta = wait_terminal(root, session);
    assert_eq!(meta["reason"], "ExitedOk", "{session}: {meta:#}");
}

/// Every record in the `output.log` of a session that has ended: the file
/// is complete, so it must exist, parse, and hold a record.
fn logged(root: &Home, session: &str) -> Vec<Value> {
    let log = log_path(root, session);
    let dir = log.parent().unwrap();
    let listing = || -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                format!("{:?} {:?}", e.file_name(), e.metadata().map(|m| m.len()))
            })
            .collect()
    };
    let text = std::fs::read_to_string(&log)
        .unwrap_or_else(|e| panic!("{}: {e}; session dir: {:?}", log.display(), listing()));
    let records: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l:?}")))
        .collect();
    assert!(
        !records.is_empty(),
        "{} holds no record: {:?}; session dir: {:?}",
        log.display(),
        std::fs::metadata(&log),
        listing()
    );
    records
}

/// (content, terminator) of each record; `None` for LF.
fn framing(records: &[Value]) -> Vec<(String, Option<String>)> {
    records
        .iter()
        .map(|r| {
            (
                r["content"].as_str().unwrap().to_owned(),
                r["terminator"].as_str().map(str::to_owned),
            )
        })
        .collect()
}

fn wait_records(root: &Home, session: &str, done: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    poll_until(HANG, Duration::from_millis(50), || {
        let records = records(root, session);
        if done(&records) {
            Observation::Ready(records)
        } else {
            Observation::Pending(format!("{} records", records.len()))
        }
    })
    .unwrap_or_else(|e| panic!("{session}: {e}"))
}

fn has(records: &[Value], content: &str) -> bool {
    records.iter().any(|r| r["content"] == content)
}

fn start(root: &Home, session: &str, child: &Child, delimiter: Option<&str>) {
    let mut args = vec!["start", session];
    if let Some(d) = delimiter {
        args.extend(["--log-delimiter", d]);
    }
    let program = child.program();
    args.extend(["--", &program]);
    tendr(root).args(&args).assert_within_deadline().success();
}

fn lines(out: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(out)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn a_cr_update_is_logged_while_the_child_runs() {
    let root = Home::new();
    let child = Child::new("");
    let (g1, g2) = (child.gate("g1"), child.gate("g2"));
    std::fs::write(
        child.dir.path().join("fake-ssh-actions"),
        format!(
            "out 10%\\r\nuntil {}\nout \\na\\r\\rX\\n\nuntil {}\n",
            g1.display(),
            g2.display()
        ),
    )
    .unwrap();
    let _release = OpenGate(g2.clone());
    start(&root, "cr", &child, Some("newline-or-cr"));

    // The child blocks after its CR: the update is there before it exits,
    // and before its next write.
    let seen = wait_records(&root, "cr", |r| has(r, "10%"));
    assert_eq!(framing(&seen), [("10%".into(), Some("cr".into()))]);

    std::fs::write(&g1, "").unwrap();
    let seen = wait_records(&root, "cr", |r| has(r, "X"));
    // The LF after "10%\r" came in a later write and is swallowed with it;
    // "\r\r" is two records; the byte after a CR is kept.
    assert_eq!(
        framing(&seen),
        [
            ("10%".into(), Some("cr".into())),
            ("a".into(), Some("cr".into())),
            (String::new(), Some("cr".into())),
            ("X".into(), None),
        ]
    );
    std::fs::write(&g2, "").unwrap();
    wait_exited_ok(&root, "cr");
}

#[test]
fn newline_mode_keeps_crs_in_the_line() {
    let root = Home::new();
    let child = Child::new("out 1%\\r2%\\r\\n\nout tail");
    start(&root, "nl", &child, None);
    wait_exited_ok(&root, "nl");
    assert_eq!(
        framing(&logged(&root, "nl")),
        [("1%\r2%".into(), None), ("tail".into(), Some("eof".into()))]
    );
}

#[test]
fn a_long_span_is_logged_as_fragments_in_both_modes() {
    let span = "aé€😀".repeat(10_000); // 100,000 bytes, no terminator
    for delimiter in [None, Some("newline-or-cr")] {
        let root = Home::new();
        let child = Child::new(&format!("out {span}\nout-hex ff410a\nout ok\\n"));
        start(&root, "frag", &child, delimiter);
        wait_exited_ok(&root, "frag");
        let framed = framing(&logged(&root, "frag"));
        let (ok, rest) = framed.split_last().unwrap();
        assert_eq!(
            *ok,
            ("ok".to_owned(), None),
            "{delimiter:?}: capture went on"
        );
        let (line, fragments) = rest.split_last().unwrap();
        assert_eq!(line.1, None);
        // Invalid UTF-8 is decoded lossily.
        let rest_of_span = line.0.strip_suffix("\u{FFFD}A").unwrap();
        assert!(fragments.len() > 1, "{delimiter:?}: {framed:?}");
        for (content, t) in fragments {
            assert_eq!(t.as_deref(), Some("fragment"));
            assert!(content.len() <= 16 * 1024);
        }
        let joined: String = fragments.iter().map(|(c, _)| c.as_str()).collect();
        assert_eq!(joined + rest_of_span, span, "{delimiter:?}");
    }
}

/// 10,000 CR updates with a stderr line in the middle of the run, then
/// three real lines.
fn ten_thousand_updates(root: &Home) -> Child {
    let updates = |range: std::ops::RangeInclusive<u32>| -> String {
        range.map(|i| format!("{i}%\\r")).collect()
    };
    let child = Child::new("");
    let (g1, g2) = (child.gate("g1"), child.gate("g2"));
    std::fs::write(
        child.dir.path().join("fake-ssh-actions"),
        format!(
            "out {}\nuntil {}\nerr warn\\n\nuntil {}\nout {}line1\\nline2\\nline3\\n\n",
            updates(1..=5000),
            g1.display(),
            g2.display(),
            updates(5001..=10_000),
        ),
    )
    .unwrap();
    let _g1 = OpenGate(g1.clone());
    let _g2 = OpenGate(g2.clone());
    start(root, "rsync", &child, Some("newline-or-cr"));
    // Order the stderr line inside the stdout run.
    wait_records(root, "rsync", |r| has(r, "5000%"));
    std::fs::write(&g1, "").unwrap();
    wait_records(root, "rsync", |r| has(r, "warn"));
    std::fs::write(&g2, "").unwrap();
    wait_exited_ok(root, "rsync");
    child
}

#[test]
fn a_historical_read_collapses_cr_runs_and_a_follower_sees_every_update() {
    let root = Home::new();
    let _child = ten_thousand_updates(&root);
    let read = |args: &[&str]| {
        let out = tendr(&root)
            .args(["log", "rsync"])
            .args(args)
            .assert_within_deadline()
            .success();
        lines(&out.get_output().stdout)
    };
    let content = |records: &[Value]| -> Vec<String> {
        records
            .iter()
            .map(|r| r["content"].as_str().unwrap().to_owned())
            .collect()
    };

    // The stderr line does not split the run: one update survives.
    let collapsed = read(&[]);
    assert_eq!(
        content(&collapsed),
        ["warn", "10000%", "line1", "line2", "line3"]
    );
    assert_eq!(
        content(&read(&["--tail", "4"])),
        ["10000%", "line1", "line2", "line3"]
    );

    let every = read(&["--all-updates"]);
    assert_eq!(every.len(), 10_000 + 4);
    // A follower cannot know which update is last, so it shows them all.
    assert_eq!(read(&["-f", "--since", "0"]), every);

    let events = tendr(&root)
        .args([
            "events",
            "--session",
            "rsync",
            "--include-logs",
            "--kind",
            "log.",
        ])
        .assert_within_deadline()
        .success();
    let events = lines(&events.get_output().stdout);
    let contents: Vec<&str> = events
        .iter()
        .map(|e| e["data"]["content"].as_str().unwrap())
        .collect();
    assert_eq!(contents, ["warn", "10000%", "line1", "line2", "line3"]);
    assert_eq!(events[1]["data"]["terminator"], "cr");
}

/// `tendr status`'s `pipe_reader`, polled until `done`.
fn wait_reader(root: &Home, session: &str, done: impl Fn(&Value) -> bool) -> Value {
    poll_until(HANG, Duration::from_millis(100), || {
        let out = tendr(root)
            .args(["status", session])
            .assert_within_deadline()
            .success();
        let status: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
        let reader = status["pipe_reader"].clone();
        if done(&reader) {
            Observation::Ready(reader)
        } else {
            Observation::Pending(reader.to_string())
        }
    })
    .unwrap_or_else(|e| panic!("{session}: {e}"))
}

fn pending(reader: &Value) -> u64 {
    reader["pending_partial_bytes"].as_u64().unwrap_or(u64::MAX)
}

fn age(reader: &Value) -> f64 {
    reader["last_record_age_s"].as_f64().unwrap_or(-1.0)
}

#[test]
fn status_shows_bytes_waiting_for_a_newline() {
    let root = Home::new();
    let child = Child::new("");
    let (g1, g2) = (child.gate("g1"), child.gate("g2"));
    std::fs::write(
        child.dir.path().join("fake-ssh-actions"),
        format!(
            "out 10%\\r\nuntil {}\nout 10%\\r\nuntil {}\n",
            g1.display(),
            g2.display()
        ),
    )
    .unwrap();
    let _g1 = OpenGate(g1.clone());
    let _g2 = OpenGate(g2.clone());
    start(&root, "buffered", &child, None);

    let first = wait_reader(&root, "buffered", |r| pending(r) == 4);
    assert_eq!(first["live"], true);
    wait_reader(&root, "buffered", |r| age(r) > age(&first));
    std::fs::write(&g1, "").unwrap();
    let second = wait_reader(&root, "buffered", |r| pending(r) == 8);
    wait_reader(&root, "buffered", |r| age(r) > age(&second));
    assert!(records(&root, "buffered").is_empty(), "no record yet");

    std::fs::write(&g2, "").unwrap();
    wait_exited_ok(&root, "buffered");
    // The 8 bytes became an eof record; the values are final.
    let ended = wait_reader(&root, "buffered", |r| r["live"] == false);
    assert_eq!(pending(&ended), 0);
    assert_eq!(wait_reader(&root, "buffered", |_| true), ended);
}

#[test]
fn status_shows_a_silent_child_as_nothing_pending_and_ageing() {
    let root = Home::new();
    let child = Child::new("");
    let gate = child.gate("g");
    std::fs::write(
        child.dir.path().join("fake-ssh-actions"),
        format!("until {}\n", gate.display()),
    )
    .unwrap();
    let _gate = OpenGate(gate.clone());
    start(&root, "silent", &child, None);

    let first = wait_reader(&root, "silent", |r| r.is_object());
    assert_eq!(pending(&first), 0);
    let later = wait_reader(&root, "silent", |r| age(r) > age(&first));
    assert_eq!(pending(&later), 0);

    std::fs::write(&gate, "").unwrap();
    wait_exited_ok(&root, "silent");
    let ended = wait_reader(&root, "silent", |r| r["live"] == false);
    assert_eq!(wait_reader(&root, "silent", |_| true), ended);
}
