//! The ready check: `start --ready-when-output`, `--ready-when-port`,
//! `--ready-within`, `wait --ready` and `start --after-ready`.

mod harness;
use harness::Home;

use harness::{DeadlineAssertExt, OpenGate, read_events, tendr, wait_terminal};
use std::path::Path;

/// Argv for a child that runs `script` (POSIX sh, the gate path is `$1`).
fn sh(script: &str, gate: &Path) -> Vec<String> {
    vec![
        "sh".into(),
        "-c".into(),
        script.into(),
        "sh".into(),
        gate.to_str().unwrap().into(),
    ]
}

/// Waits for `$1` to exist: a child that never ends by itself.
const UNTIL_GATE: &str = "until [ -e \"$1\" ]; do sleep 0.05; done";

fn start(root: &Home, args: &[&str], argv: &[String]) {
    tendr(root)
        .arg("start")
        .args(args)
        .arg("--")
        .args(argv)
        .assert_within_deadline()
        .success();
}

fn meta(root: &Home, session: &str) -> serde_json::Value {
    let path = root
        .path()
        .join(format!(".tendr/sessions/default/{session}/meta.json"));
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn kinds(root: &Home, session: &str) -> Vec<String> {
    read_events(root, session)
        .into_iter()
        .map(|e| e["kind"].as_str().unwrap().to_owned())
        .collect()
}

/// Whether one of the session's meta warnings contains `text`.
fn warned(root: &Home, session: &str, text: &str) -> bool {
    meta(root, session)["warnings"].as_array().is_some_and(|w| {
        w.iter()
            .any(|w| w.as_str().is_some_and(|w| w.contains(text)))
    })
}

/// A pipe session is ready once a line of its output, here on stderr,
/// matches: `wait --ready` returns 0 while the child keeps running, meta
/// records the time, and `run.ready` follows `run.started` with the line,
/// where `events --kind` finds it.
#[test]
fn a_matching_output_line_makes_a_pipe_session_ready() {
    let root = Home::new();
    let gate = OpenGate(root.path().join("gate"));
    let script = format!("echo booting; echo 'listening on 8080' >&2; {UNTIL_GATE}");
    start(
        &root,
        &["srv", "--ready-when-output", r"^listening on \d+$"],
        &sh(&script, &gate.0),
    );

    let out = tendr(&root)
        .args(["wait", "--ready", "srv"])
        .assert_within_deadline()
        .success();
    let reported: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(reported[0]["ready"]["state"], "Ready", "{reported}");
    assert_eq!(reported[0]["status"], "Running", "the child keeps running");
    assert_eq!(meta(&root, "srv")["ready"]["state"], "Ready");

    let events = read_events(&root, "srv");
    let ready = events
        .iter()
        .position(|e| e["kind"] == "run.ready")
        .expect("a run.ready event");
    let started = events
        .iter()
        .position(|e| e["kind"] == "run.started")
        .unwrap();
    assert!(started < ready, "{events:?}");
    assert_eq!(events[ready]["data"]["line"], "listening on 8080");
    assert_eq!(events[ready]["source"], "tendr.sidecar");

    let replay = tendr(&root)
        .args(["events", "--session", "default/srv", "--kind", "run.ready"])
        .assert_within_deadline()
        .success();
    let lines: Vec<serde_json::Value> = String::from_utf8_lossy(&replay.get_output().stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["kind"], "run.ready");
}

/// A PTY session's output is matched as the terminal shows it: a prompt that
/// never ends its line makes the session ready.
#[cfg(unix)]
#[test]
fn a_pty_prompt_makes_a_pty_session_ready() {
    let root = Home::new();
    let gate = OpenGate(root.path().join("gate"));
    let script = format!("printf 'loading\\nready> '; {UNTIL_GATE}");
    start(
        &root,
        &["tty", "--pty", "--ready-when-output", "^ready> $"],
        &sh(&script, &gate.0),
    );
    tendr(&root)
        .args(["wait", "--ready", "tty"])
        .assert_within_deadline()
        .success();
    let event = harness::wait_event_kind(&root, "tty", "run.ready");
    assert_eq!(event["data"]["line"], "ready> ");
}

/// A port condition holds once something accepts connections on that port
/// of the session's host, here a listener opened after the start.
#[test]
fn an_open_port_makes_the_session_ready() {
    let root = Home::new();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    start(
        &root,
        &["web", "--ready-when-port", &port.to_string()],
        &["sleep".into(), "3600".into()],
    );
    let _listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
    tendr(&root)
        .args(["wait", "--ready", "web"])
        .assert_within_deadline()
        .success();
    let event = harness::wait_event_kind(&root, "web", "run.ready");
    assert_eq!(event["data"]["port"], port);
}

/// Past `--ready-within`, the miss is recorded and the child keeps running.
/// The condition is no longer checked: a matching line after the miss
/// changes nothing.
#[test]
fn a_missed_deadline_is_recorded_and_the_child_keeps_running() {
    let root = Home::new();
    let gate = OpenGate(root.path().join("gate"));
    let done = OpenGate(root.path().join("done"));
    let script = format!("{UNTIL_GATE}; echo up; until [ -e \"$2\" ]; do sleep 0.05; done",);
    let mut argv = sh(&script, &gate.0);
    argv.push(done.0.to_str().unwrap().into());
    start(
        &root,
        &[
            "slow",
            "--ready-when-output",
            "^up$",
            "--ready-within",
            "1s",
        ],
        &argv,
    );

    let out = tendr(&root)
        .args(["wait", "--ready", "slow"])
        .assert_within_deadline()
        .code(43);
    let reported: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(reported[0]["ready"]["state"], "Missed", "{reported}");
    assert_eq!(reported[0]["status"], "Running");
    let missed = harness::wait_event_kind(&root, "slow", "run.ready_missed");
    assert_eq!(missed["data"]["within_ms"], 1000);

    drop(gate);
    harness::wait_log_tail(&root, "slow", "up");
    assert_eq!(meta(&root, "slow")["ready"]["state"], "Missed");
    assert_eq!(meta(&root, "slow")["status"], "Running");
    drop(done);
    wait_terminal(&root, "slow");
    assert!(!kinds(&root, "slow").contains(&"run.ready".to_owned()));
}

/// A child whose last output is its ready line, and that exits at once, was
/// ready: the match is recorded before the run's end, pipe and PTY alike.
#[test]
fn a_ready_line_printed_just_before_exit_counts() {
    let root = Home::new();
    let mut sessions = vec![("pipe", vec![])];
    if cfg!(unix) {
        sessions.push(("tty", vec!["--pty"]));
    }
    for (name, extra) in sessions {
        let mut args = vec![name, "--ready-when-output", "^migrated$"];
        args.extend(extra);
        start(
            &root,
            &args,
            &["sh".into(), "-c".into(), "echo migrated".into()],
        );
        tendr(&root)
            .args(["wait", "--ready", name])
            .assert_within_deadline()
            .success();
        wait_terminal(&root, name);
        assert_eq!(meta(&root, name)["ready"]["state"], "Ready", "{name}");
        assert!(
            kinds(&root, name).contains(&"run.ready".to_owned()),
            "{name}"
        );
    }
}

/// `wait --ready` exits 43 for a run that ended before it was ready, 1 when
/// its `--timeout` passes first, and 1 for a session without a condition.
#[test]
fn wait_ready_says_why_a_session_is_not_ready() {
    let root = Home::new();
    start(
        &root,
        &["quits", "--ready-when-output", "never"],
        &["true".into()],
    );
    let out = tendr(&root)
        .args(["wait", "--ready", "quits"])
        .assert_within_deadline()
        .code(43);
    let reported: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(reported[0]["status"], "Exited");
    assert!(reported[0].get("ready").is_none(), "{reported}");

    start(
        &root,
        &["pending", "--ready-when-output", "never"],
        &["sleep".into(), "3600".into()],
    );
    tendr(&root)
        .args(["wait", "--ready", "--timeout", "1", "pending"])
        .assert_within_deadline()
        .code(1)
        .stderr(predicates::str::contains("timeout waiting for pending"));

    start(&root, &["plain"], &["sleep".into(), "3600".into()]);
    tendr(&root)
        .args(["wait", "--ready", "plain"])
        .assert_within_deadline()
        .code(1)
        .stderr(predicates::str::contains("no ready condition"));
}

/// `--after-ready` holds the child in `Starting` until the dependency is
/// ready, then starts it while the dependency keeps running.
#[test]
fn after_ready_starts_once_the_dependency_is_ready() {
    let root = Home::new();
    let gate = OpenGate(root.path().join("gate"));
    let script = format!("{UNTIL_GATE}; echo ready; sleep 3600");
    start(
        &root,
        &["db", "--ready-when-output", "^ready$"],
        &sh(&script, &gate.0),
    );
    start(&root, &["app", "--after-ready", "db"], &["true".into()]);
    // `start` returns after the first scan: db cannot be ready yet.
    assert_eq!(meta(&root, "app")["status"], "Starting");

    drop(gate);
    tendr(&root)
        .args(["wait", "app"])
        .assert_within_deadline()
        .success();
    assert_eq!(meta(&root, "db")["status"], "Running");
    assert_eq!(meta(&root, "db")["ready"]["state"], "Ready");
}

/// A dependency that ends before it is ready, or misses its deadline, fails
/// the waiter as a failed dependency (4); one without a ready condition is
/// refused at `start`.
#[test]
fn after_ready_fails_when_the_dependency_will_not_be_ready() {
    let root = Home::new();
    let gate = OpenGate(root.path().join("gate"));
    start(
        &root,
        &["ends", "--ready-when-output", "never"],
        &sh(UNTIL_GATE, &gate.0),
    );
    start(&root, &["app1", "--after-ready", "ends"], &["true".into()]);
    drop(gate);
    tendr(&root)
        .args(["wait", "app1"])
        .assert_within_deadline()
        .code(4);
    assert_eq!(meta(&root, "app1")["status"], "DependencyFailed");
    assert!(warned(&root, "app1", "ended before it was ready"));

    start(
        &root,
        &[
            "late",
            "--ready-when-output",
            "never",
            "--ready-within",
            "1s",
        ],
        &["sleep".into(), "3600".into()],
    );
    start(&root, &["app2", "--after-ready", "late"], &["true".into()]);
    tendr(&root)
        .args(["wait", "app2"])
        .assert_within_deadline()
        .code(4);
    assert!(warned(&root, "app2", "missed its ready deadline"));
    assert_eq!(meta(&root, "late")["status"], "Running");

    start(&root, &["plain"], &["sleep".into(), "3600".into()]);
    tendr(&root)
        .args(["start", "app3", "--after-ready", "plain", "--", "true"])
        .assert_within_deadline()
        .code(1)
        .stderr(predicates::str::contains("has no ready condition"));
}
