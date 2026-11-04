//! Smoke tests for the ConPTY harness (tests/harness/conpty.rs): a command
//! runs inside a pseudo console, reads typed input, sees resizes and is closed
//! without a hang. `conmode` reports the console state from inside. No tendr
//! code is involved; later tests drive `tendr attach` the same way.
#![cfg(windows)]

mod harness;

use harness::conpty::{ConPty, ConmodeReport, conmode_bin, strip_escapes};
use harness::{HANG, Observation, poll_until};
use std::time::Duration;

fn report_tagged(pty: &ConPty, tag: &str) -> ConmodeReport {
    pty.wait_for(&format!("conmode report tagged {tag:?}"), HANG, |text| {
        ConmodeReport::find(text, tag)
    })
}

#[test]
fn conmode_reports_the_pseudo_console_it_runs_in() {
    let pty = ConPty::spawn(&[&conmode_bin()], 80, 25);
    let report = report_tagged(&pty, "once");

    assert_eq!(report.window, "80x25", "{report:?}");
    // A fresh console starts cooked: processed, line and echo input.
    let conin = ConmodeReport::mode(&report.conin).expect("CONIN$ mode");
    assert_eq!(conin & 0x7, 0x7, "cooked input bits in {report:?}");
    // ENABLE_PROCESSED_OUTPUT.
    let conout = ConmodeReport::mode(&report.conout).expect("CONOUT$ mode");
    assert_eq!(conout & 0x1, 0x1, "processed output in {report:?}");
    let cp: u32 = report.cp.parse().expect("output code page");
    assert_ne!(cp, 0, "{report:?}");

    // The report is drawn before conmode has finished exiting; wait for the
    // exit so close() doesn't terminate it.
    assert_eq!(pty.wait_exit(HANG), Some(0), "{}", pty.text());
    let closed = pty.close();
    assert_eq!(closed.exit_code, 0, "{}", closed.text());
}

#[test]
fn typed_line_is_echoed_by_the_child() {
    let mut pty = ConPty::spawn(&[&conmode_bin(), "--serve"], 80, 25);
    report_tagged(&pty, "start");

    pty.write(b"hello-conpty\r");
    let echoed = report_tagged(&pty, "hello-conpty");
    assert_eq!(echoed.window, "80x25", "{echoed:?}");

    pty.write(b"quit\r");
    assert_eq!(pty.wait_exit(HANG), Some(0), "{}", pty.text());
    let closed = pty.close();
    assert_eq!(closed.exit_code, 0);
}

#[test]
fn resize_is_seen_by_the_child() {
    let mut pty = ConPty::spawn(&[&conmode_bin(), "--serve"], 80, 25);
    report_tagged(&pty, "start");

    // Grow, then shrink. A report is at most 65 columns (`tag=r9999`,
    // `cp=65001`, `window=100x30`), so no width used here can wrap one.
    let mut asked = 0;
    for (cols, rows) in [(100, 30), (72, 20)] {
        pty.resize(cols, rows);
        let want = format!("{cols}x{rows}");
        // conhost applies a resize asynchronously, so ask until a report shows
        // it. Each request waits for its own tagged answer.
        let first = asked + 1;
        poll_until(HANG, Duration::from_millis(50), || {
            asked += 1;
            let tag = format!("r{asked}");
            pty.write(format!("{tag}\r").as_bytes());
            let report = report_tagged(&pty, &tag);
            if report.window == want {
                Observation::Ready(())
            } else {
                Observation::Pending(format!("{report:?}"))
            }
        })
        .unwrap_or_else(|e| panic!("resize to {want} never seen: {e}"));
        eprintln!(
            "resize to {want} seen after {} request(s)",
            asked - first + 1
        );
    }

    pty.write(b"quit\r");
    assert_eq!(pty.wait_exit(HANG), Some(0), "{}", pty.text());
    pty.close();
}

#[test]
fn close_ends_a_running_client_without_hanging() {
    let pty = ConPty::spawn(&[&conmode_bin(), "--serve"], 80, 25);
    report_tagged(&pty, "start");

    // close() bounds every step and terminates the client still reading input.
    let closed = pty.close();
    assert_eq!(closed.exit_code, 1, "terminated by close");
    assert!(
        ConmodeReport::find(&closed.text(), "start").is_some(),
        "{}",
        closed.text()
    );
}

#[test]
fn strip_escapes_keeps_text_and_drops_sequences() {
    let raw =
        b"\x1b[?25l\x1b[2J\x1b[1;1Hab\x1b[0mc\x1b]0;title\x07d\x1b]8;;x\x1b\\e\x1b(Bf\x1b7g\r\n";
    assert_eq!(strip_escapes(raw), "abcdefg\r\n");
}
