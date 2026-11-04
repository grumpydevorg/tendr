//! `output.log` rotation end to end: a session writes past the rotation size
//! (lowered with the debug-only `TENDR_TEST_OUTPUT_LOG_BYTES`) and `log`,
//! `log --tail`, `log -f` and `events --include-logs` still return its
//! records, in order, from the rotated files; retention keeps the newest.

mod harness;
use harness::Home;

use harness::{
    DeadlineAssertExt, HANG, Observation, OpenGate, fake_program, poll_until, tendr, wait_terminal,
};
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A record is about 60 bytes, so this rotates every ~70 lines.
const ROTATE_BYTES: &str = "4096";

fn lines(range: std::ops::Range<u32>) -> String {
    range.map(|i| format!("line-{i:04}\\n")).collect()
}

fn names(range: std::ops::Range<u32>) -> Vec<String> {
    range.map(|i| format!("line-{i:04}")).collect()
}

fn contents(records: &[Value], key: &[&str]) -> Vec<String> {
    records
        .iter()
        .map(|r| key.iter().fold(r, |v, k| &v[*k]))
        .map(|c| c.as_str().unwrap().to_owned())
        .collect()
}

fn json_lines(out: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(out)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn archives(root: &Home) -> Vec<u64> {
    let dir = root.path().join(".tendr/sessions/default/rot");
    let mut found: Vec<u64> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| {
            e.unwrap()
                .file_name()
                .to_str()?
                .strip_prefix("output.log.")?
                .parse()
                .ok()
        })
        .collect();
    found.sort_unstable();
    found
}

fn log(root: &Home, args: &[&str]) -> Vec<String> {
    let out = tendr(root)
        .args(["log", "rot"])
        .args(args)
        .assert_within_deadline()
        .success();
    contents(&json_lines(&out.get_output().stdout), &["content"])
}

/// `tendr log rot -f --since 0`, its records collected as they come.
struct Follower {
    child: Child,
    records: Arc<Mutex<Vec<Value>>>,
}

impl Follower {
    fn spawn(root: &Home) -> Self {
        let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
            .args(["log", "rot", "-f", "--since", "0"])
            .env("HOME", root.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let records = Arc::new(Mutex::new(Vec::new()));
        let stdout = child.stdout.take().unwrap();
        let sink = Arc::clone(&records);
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                sink.lock()
                    .unwrap()
                    .push(serde_json::from_str(&line).unwrap());
            }
        });
        Self { child, records }
    }

    fn wait_for(&self, content: &str) -> Vec<String> {
        poll_until(HANG, Duration::from_millis(50), || {
            let seen = contents(&self.records.lock().unwrap(), &["content"]);
            if seen.last().is_some_and(|c| c == content) {
                Observation::Ready(seen)
            } else {
                Observation::Pending(format!("{} records", seen.len()))
            }
        })
        .unwrap_or_else(|e| panic!("log -f never showed {content}: {e}"))
    }
}

impl Drop for Follower {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn readers_keep_working_across_rotations() {
    let root = Home::new();
    let dir = fake_program("chatty", "");
    let (g1, g2) = (dir.path().join("g1"), dir.path().join("g2"));
    let _g1 = OpenGate(g1.clone());
    let _g2 = OpenGate(g2.clone());
    std::fs::write(
        dir.path().join("fake-ssh-actions"),
        format!(
            "out {}\nuntil {}\nout {}\nuntil {}\nout {}\n",
            lines(0..100),
            g1.display(),
            lines(100..250),
            g2.display(),
            lines(250..600),
        ),
    )
    .unwrap();
    let program = dir
        .path()
        .join(format!("chatty{}", std::env::consts::EXE_SUFFIX));
    tendr(&root)
        .env("TENDR_TEST_OUTPUT_LOG_BYTES", ROTATE_BYTES)
        .args(["start", "rot", "--", program.to_str().unwrap()])
        .assert_within_deadline()
        .success();

    // The first 100 lines span two files.
    let follower = Follower::spawn(&root);
    assert_eq!(follower.wait_for("line-0099"), names(0..100));
    assert_eq!(archives(&root), [1]);
    assert_eq!(log(&root, &[]), names(0..100));
    assert_eq!(log(&root, &["--tail", "80"]), names(20..100));

    // A follower keeps its place while the log rotates under it.
    std::fs::write(&g1, "").unwrap();
    assert_eq!(follower.wait_for("line-0249"), names(0..250));
    assert_eq!(archives(&root), [1, 2, 3]);
    let events = tendr(&root)
        .args([
            "events",
            "--session",
            "rot",
            "--include-logs",
            "--kind",
            "log.",
        ])
        .assert_within_deadline()
        .success();
    assert_eq!(
        contents(
            &json_lines(&events.get_output().stdout),
            &["data", "content"]
        ),
        names(0..250)
    );
    drop(follower);

    // Retention keeps the newest three archives: the log is the most recent
    // output, whole and in order, up to the last line.
    std::fs::write(&g2, "").unwrap();
    wait_terminal(&root, "rot");
    let kept = archives(&root);
    assert_eq!(kept.len(), 3, "{kept:?}");
    assert!(kept[0] > 3, "old archives deleted: {kept:?}");
    let all = log(&root, &[]);
    let first: u32 = all[0].strip_prefix("line-").unwrap().parse().unwrap();
    assert!(first > 0);
    assert_eq!(all, names(first..600));
    assert_eq!(log(&root, &["--tail", "3"]), names(597..600));
}
