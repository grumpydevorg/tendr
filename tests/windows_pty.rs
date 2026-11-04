//! `--pty` sessions on a Windows host: the child runs on a pseudoconsole
//! (ConPTY) the sidecar owns, and `tendr attach`, itself in a pseudo console
//! as in Windows Terminal (tests/harness/conpty.rs), reaches it through the
//! session's named pipe.
//!
//! The session's child is `conmode --serve` (src/bin/conmode.rs), which
//! reports the size of the console it runs in for each line typed into it.
//! What the attach client drew is conhost's rendering of the session's
//! conhost's rendering, so only report lines are asserted.
#![cfg(windows)]

mod harness;

use harness::conpty::{ConPty, ConmodeReport, conmode_bin};
use harness::{
    HANG, Home, logged_output, recorded_output, tendr, wait_log_tail, wait_running, wait_terminal,
};
use std::time::Instant;

/// The default detach escape: `Ctrl-\` then `d`.
const DETACH: &[u8] = b"\x1cd";

/// Start `name` as a `--pty` session running `argv`.
fn start_pty(home: &Home, name: &str, argv: &[&str]) {
    let out = tendr(home)
        .args(["start", name, "--pty", "--"])
        .args(argv)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "start {name}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    wait_running(home, name);
}

/// `tendr attach <name>` in a `cols`×`rows` pseudo console, run by `conmode
/// --run`, which also reports the console before and after it.
fn attach(home: &Home, name: &str, cols: i16, rows: i16) -> ConPty {
    let home_env = format!("HOME={}", home.path().display());
    let conmode = conmode_bin();
    let client = ConPty::spawn(
        &[
            &conmode,
            "--run",
            "--env",
            &home_env,
            "--",
            env!("CARGO_BIN_EXE_tendr"),
            "attach",
            name,
        ],
        cols,
        rows,
    );
    client.wait_for("conmode report tagged \"before\"", HANG, |text| {
        ConmodeReport::find(text, "before")
    });
    client
}

/// Type lines tagged `prefix0`, `prefix1`, … into the session through
/// `client` until the session's report shows a console of `size`
/// (`<cols>x<rows>`), and return that report. A resize reaches the session
/// asynchronously (conhost applies it, the client finds it by polling), so
/// a report from before it may still show the old size.
fn report_at_size(client: &mut ConPty, prefix: &str, size: &str) -> ConmodeReport {
    let start = Instant::now();
    for n in 0.. {
        let tag = format!("{prefix}{n}");
        client.write(format!("{tag}\r").as_bytes());
        let report = client.wait_for(&format!("the session's report {tag:?}"), HANG, |text| {
            ConmodeReport::find(text, &tag)
        });
        if report.window == size {
            return report;
        }
        assert!(
            start.elapsed() < HANG,
            "the session never reported a {size} console; last {report:?}\ndrawn: {:?}",
            client.text()
        );
    }
    unreachable!()
}

/// A client attached to a `--pty` session types into it and its size
/// reaches the session; a resize of the client's console reaches it too.
/// The escape detaches (exit 0) and leaves the session running; a second
/// client attaches with its own size, and the session's end ends it.
#[test]
fn a_pty_session_attaches_resizes_detaches_and_reattaches() {
    let home = Home::new();
    let conmode = conmode_bin();
    start_pty(&home, "s", &[&conmode, "--serve"]);

    let mut first = attach(&home, "s", 80, 25);
    report_at_size(&mut first, "one", "80x25");
    // As a user resizing the window.
    first.resize(100, 30);
    report_at_size(&mut first, "two", "100x30");

    first.write(DETACH);
    assert_eq!(first.wait_exit(HANG), Some(0), "{}", first.text());
    first.close();
    let meta = wait_running_meta(&home);
    assert_eq!(
        meta["status"], "Running",
        "detaching leaves it running: {meta}"
    );

    let mut second = attach(&home, "s", 90, 28);
    report_at_size(&mut second, "three", "90x28");
    // conmode --serve exits on `quit`, which ends the run, and the attach.
    second.write(b"quit\r");
    assert_eq!(second.wait_exit(HANG), Some(0), "{}", second.text());
    second.close();
    let meta = wait_terminal(&home, "s");
    assert_eq!(meta["reason"], "ExitedOk", "{meta}");
}

/// The session's meta while it runs.
fn wait_running_meta(home: &Home) -> serde_json::Value {
    let out = tendr(home).args(["status", "s"]).output().unwrap();
    serde_json::from_slice(&out.stdout).expect("status prints meta")
}

/// Ctrl-C pushed into a session reaches its child as Ctrl-C does typed
/// into a console: the session's processes are not started ignoring it.
/// ping ends with STATUS_CONTROL_C_EXIT, and cmd, which waits out a Ctrl-C
/// while a command runs, takes the next line.
#[test]
fn ctrl_c_pushed_into_a_pty_session_reaches_its_child() {
    let home = Home::new();
    start_pty(&home, "c", &["cmd.exe", "/q", "/k"]);

    push(&home, "c", b"ping -n 1000 127.0.0.1\r");
    // ping's own line, not the echo of the typed command.
    wait_log_tail(&home, "c", "Pinging");
    push(&home, "c", b"\x03");
    // ping says so when Ctrl-C stops it.
    wait_log_tail(&home, "c", "Control-C");
    push(&home, "c", b"echo rc=%errorlevel%\r");
    // 0xC000013A, STATUS_CONTROL_C_EXIT. The typed line echoes as
    // `%errorlevel%`, so only cmd's expansion matches.
    wait_log_tail(&home, "c", "rc=-1073741510");

    push(&home, "c", b"exit 0\r");
    let meta = wait_terminal(&home, "c");
    assert_eq!(meta["reason"], "ExitedOk", "{meta}");
}

fn push(home: &Home, name: &str, bytes: &[u8]) {
    let out = tendr(home)
        .args(["push", name])
        .write_stdin(bytes)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "push {bytes:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A child that exits by itself ends the run with all its output, though
/// capture is stalled past the exit: the pseudoconsole is closed once the
/// child has exited, and conhost's last frame is still read.
#[test]
fn a_pty_child_that_exits_ends_the_run_with_its_output() {
    let home = Home::new();
    let out = tendr(&home)
        .env("TENDR_TEST_PTY_READ_DELAY_MS", "2000")
        .args([
            "start",
            "e",
            "--pty",
            "--",
            "cmd.exe",
            "/c",
            "echo pty-hello",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let meta = wait_terminal(&home, "e");
    assert_eq!(meta["reason"], "ExitedOk", "{meta}");
    let log = logged_output(&home, "e");
    assert!(log.contains("pty-hello"), "{log:?}");
    // The recording is complete and holds the output, as on Unix, and
    // `tendr replay` reads it.
    assert_eq!(meta["pty"]["recording"]["state"], "Complete", "{meta}");
    let recorded = String::from_utf8_lossy(&recorded_output(&home, "e")).into_owned();
    assert!(recorded.contains("pty-hello"), "{recorded:?}");
    let out = tendr(&home)
        .args(["replay", "--export", "asciicast"])
        .arg(home.path().join(".tendr/sessions/default/e"))
        .output()
        .unwrap();
    let cast = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "replay: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(cast.starts_with("{\"version\": 2,"), "{cast}");
    assert!(cast.contains("pty-hello"), "{cast}");
}

/// A graceful kill closes the child's pseudoconsole, which ends it as
/// closing its window would.
#[test]
fn kill_ends_a_pty_session() {
    let home = Home::new();
    let conmode = conmode_bin();
    start_pty(&home, "k", &[&conmode, "--serve"]);
    wait_log_tail(&home, "k", "tag=start");

    let out = tendr(&home).args(["kill", "k"]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let meta = wait_terminal(&home, "k");
    assert_eq!(meta["reason"], "Killed", "{meta}");
}
