//! `--host` end to end through the frame transport. The fake ssh
//! (`src/bin/fake_ssh.rs`) writes down its argv, then acts as the remote
//! login shell and runs the real tendr, whose `_remote` entry point reads the
//! request from stdin. Both sides share one HOME, so a test checks what the
//! remote side recorded, and that the ssh argv was the constant one: no
//! value could have reached a shell.
//!
//! The `_remote` tests at the end drive the entry point directly with
//! crafted stdin.

mod harness;

use harness::{SessionGuard, fake_ssh, shim_path_env, tendr, wait_running, wait_terminal_ns};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

/// The ssh argv of every framed `--host` call.
const REMOTE_ARGV: [&str; 8] = [
    "-T",
    "-o",
    "ConnectTimeout=10",
    "box",
    "tendr",
    "--hello",
    "--exit-trailer",
    "_remote",
];

/// Data that a POSIX shell, cmd.exe or PowerShell would act on, with CR/LF,
/// Unicode, spaces and a Windows path.
const HOSTILE: &str = "& | $ ; ( ) \" ' ` \r\n \u{e9}\u{4e2d} C:\\Program Files\\x y";

/// A session or namespace name with shell metacharacters that is still a
/// valid directory name on every platform.
const HOSTILE_NAME: &str = "x&calc;$(id)`'\u{e9}";

/// How long a streamed line may take to arrive.
const STREAM_DEADLINE: Duration = Duration::from_secs(20);

/// A fake ssh that records its argv, then forwards to the real tendr.
struct Remote {
    ssh: TempDir,
    _argv_dir: TempDir,
    argv: PathBuf,
}

impl Remote {
    fn new() -> Self {
        let argv_dir = TempDir::new().unwrap();
        let argv = argv_dir.path().join("argv");
        let ssh = fake_ssh(&format!("args {}\nforward", argv.display()));
        Self {
            ssh,
            _argv_dir: argv_dir,
            argv,
        }
    }

    /// `tendr --host box`, through this remote, with `root` as HOME on both
    /// sides.
    fn command(&self, root: &TempDir) -> std::process::Command {
        let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"));
        cmd.args(["--host", "box"])
            .env("HOME", root.path())
            .env("PATH", shim_path_env(&self.ssh))
            .env("TENDR_TEST_BIN", assert_cmd::cargo::cargo_bin("tendr"))
            .env_remove("TENDR_SSH_CONNECT_TIMEOUT");
        cmd
    }

    /// [`Self::command`], run to completion.
    fn tendr(&self, root: &TempDir) -> assert_cmd::Command {
        let mut cmd = assert_cmd::Command::from_std(self.command(root));
        cmd.timeout(Duration::from_secs(60));
        cmd
    }

    /// [`Self::command`] with `args`, read while it runs.
    fn spawn(&self, root: &TempDir, args: &[&str], stdin: Stdio) -> Child {
        self.command(root)
            .args(args)
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    /// The last call through this remote ran the constant ssh argv.
    fn assert_constant_argv(&self) {
        let recorded = std::fs::read_to_string(&self.argv).unwrap();
        let argv: Vec<&str> = recorded
            .lines()
            .filter_map(|l| l.strip_prefix("ARG:"))
            .collect();
        assert_eq!(argv, REMOTE_ARGV);
    }
}

/// Exit code, stdout and stderr, as compared between local and `--host`.
fn outcome(output: &Output) -> (Option<i32>, String, String) {
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Force-kills a session in `namespace` when dropped.
struct NsGuard<'a> {
    root: &'a TempDir,
    namespace: String,
    name: String,
}

impl<'a> NsGuard<'a> {
    fn new(root: &'a TempDir, namespace: &str, name: &str) -> Self {
        Self {
            root,
            namespace: namespace.to_owned(),
            name: name.to_owned(),
        }
    }
}

impl Drop for NsGuard<'_> {
    fn drop(&mut self) {
        let _ = tendr(self.root)
            .args([
                "kill",
                "--force",
                "--namespace",
                &self.namespace,
                &self.name,
            ])
            .timeout(harness::CMD_DEADLINE)
            .output();
    }
}

fn meta_path(root: &TempDir, namespace: &str, session: &str) -> PathBuf {
    root.path()
        .join(".tendr")
        .join("sessions")
        .join(namespace)
        .join(session)
        .join("meta.json")
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

/// Poll until `check` holds, panicking with `what` after `deadline`.
fn wait_for(deadline: Duration, what: &str, mut check: impl FnMut() -> bool) {
    let until = Instant::now() + deadline;
    while !check() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Lines of `child`'s stdout, as they arrive. The reader lets go of the pipe
/// after `limit` lines, so a writer still running fails its next write.
fn stdout_lines(child: &mut Child, limit: usize) -> mpsc::Receiver<String> {
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .take(limit)
        {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    rx
}

/// Wait for `child` to exit, killing it and failing after `deadline`.
fn wait_exit(child: &mut Child, deadline: Duration, what: &str) -> std::process::ExitStatus {
    let until = Instant::now() + deadline;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= until {
            let _ = child.kill();
            panic!("{what} did not exit within {deadline:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// -- start: every field and hostile values survive the wire --

/// A `--host` start carries every field, with hostile values in each, into
/// the remote meta.json exactly. The start waits on `--after`, so the
/// hostile argv is recorded but never run.
#[test]
fn host_start_records_every_field_and_hostile_value_exactly() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let ns = format!("ns-{HOSTILE_NAME}");
    let name = HOSTILE_NAME;
    let dep = format!("dep-{HOSTILE_NAME}");
    let cwd = root.path().join("cwd a b&c;$(id)`'\u{e9}(x)");
    std::fs::create_dir(&cwd).unwrap();
    let cwd_str = cwd.to_str().unwrap();

    // The dependency, and a run of the session for --replace to replace.
    let dep_meta = tendr(&root)
        .args(["start", "--namespace", &ns, &dep, "--", "sleep", "30"])
        .output()
        .unwrap();
    let _dep = NsGuard::new(&root, &ns, &dep);
    assert!(dep_meta.status.success(), "{:?}", outcome(&dep_meta));
    let dep_meta: serde_json::Value = serde_json::from_slice(&dep_meta.stdout).unwrap();
    tendr(&root)
        .args(["start", "--namespace", &ns, name, "--", "sleep", "30"])
        .assert()
        .success();
    let _session = NsGuard::new(&root, &ns, name);

    let env_a = format!("A={HOSTILE}");
    let boundary = format!("container:{HOSTILE}");
    let parent = format!("vm:{HOSTILE}");
    let argv = [
        "prog&calc",
        HOSTILE,
        "C:\\Windows\\System32\\cmd.exe",
        "/c",
        "echo %PATH% & calc",
        "--host",
        "other",
    ];
    let mut args = vec![
        "start",
        "--namespace",
        &ns,
        "--stdin",
        "--replace",
        "--exec-target",
        "powershell",
        "--timeout",
        "600",
        "--cwd",
        cwd_str,
        "--env",
        &env_a,
        "--env",
        "B=x=y;z|w",
        "--unset-env",
        "UN&SET$(x)",
        "--on-exit",
        HOSTILE,
        "--after",
        &dep,
        "--any-exit",
        "--boundary",
        &boundary,
        "--boundary-parent",
        &parent,
        "--boundary-parent",
        "pod:C:\\x",
    ];
    if cfg!(unix) {
        args.push("--pty");
    }
    args.extend([name, "--"]);
    args.extend(argv);

    let remote = Remote::new();
    let output = remote.tendr(&root).args(&args).output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", outcome(&output));
    remote.assert_constant_argv();

    let printed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let meta = read_json(&meta_path(&root, &ns, name));
    assert_eq!(printed["launch_spec"], meta["launch_spec"]);
    assert_eq!(meta["session"], name);
    assert_eq!(meta["generation"], 2, "--replace replaced the first run");
    let spec = &meta["launch_spec"];
    assert_eq!(spec["argv"], serde_json::json!(argv));
    assert_eq!(spec["namespace"], ns);
    assert_eq!(spec["stdin_mode"], "Pipe");
    assert_eq!(spec["io_mode"], if cfg!(unix) { "Pty" } else { "Pipe" });
    assert_eq!(spec["exec_target"], "PowerShell");
    assert_eq!(spec["timeout_s"], 600);
    assert_eq!(spec["cwd"], cwd_str);
    assert_eq!(
        spec["env"],
        serde_json::json!({"A": HOSTILE, "B": "x=y;z|w"})
    );
    assert_eq!(spec["unset_env"], serde_json::json!(["UN&SET$(x)"]));
    assert_eq!(spec["on_exit"], serde_json::json!([HOSTILE]));
    assert_eq!(
        spec["after"],
        serde_json::json!([{"session": dep, "run_id": dep_meta["run_id"]}])
    );
    assert_eq!(spec["after_any_exit"], true);
    assert_eq!(
        spec["boundary"],
        serde_json::json!({
            "current": {"kind": "container", "label": HOSTILE},
            "parents": [
                {"kind": "vm", "label": HOSTILE},
                {"kind": "pod", "label": "C:\\x"}
            ]
        })
    );
}

// -- byte compatibility with local --

/// Every framed operation prints what the same command prints locally, byte
/// for byte, with the same exit code: JSON, JSONL, raw logs and errors.
/// `log --since` with shell syntax arrives as data and is rejected as data.
#[test]
fn host_operations_match_local_byte_for_byte() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args([
            "start",
            "--namespace",
            "golden",
            "gold",
            "--",
            "echo",
            "golden-line",
        ])
        .assert()
        .success();
    wait_terminal_ns(&root, "golden", "gold");

    let marker = root.path().join("PWNED");
    let since = format!(
        "$(touch {m})`touch {m}`&type nul > {m}|x;y'z\"\r\n",
        m = marker.display()
    );
    let ghost = format!("ghost-{HOSTILE_NAME}");
    let cases: Vec<Vec<&str>> = vec![
        vec!["status", "--namespace", "golden", "gold"],
        vec!["status", &ghost],
        vec!["list", "--namespace", "golden"],
        vec!["list"],
        vec!["log", "--namespace", "golden", "gold"],
        vec![
            "log",
            "--namespace",
            "golden",
            "--raw",
            "--tail",
            "1",
            "gold",
        ],
        vec![
            "log",
            "--namespace",
            "golden",
            "--since",
            "9999999999",
            "gold",
        ],
        vec!["log", "--namespace", "golden", "--since", &since, "gold"],
        vec!["log", &ghost],
        vec!["kill", &ghost],
        vec!["wait", "--namespace", "golden", "--any", "gold"],
        vec!["wait", "--namespace", "golden", "--timeout", "5", "gold"],
        vec!["wait", &ghost],
        vec!["push", &ghost],
        vec!["push", "--namespace", "golden", "gold"],
        vec!["start", "--after", &ghost, "lost", "--", "true"],
        vec!["kill", "--namespace", "golden", "gold"],
    ];
    let remote = Remote::new();
    for args in &cases {
        let local = tendr(&root)
            .args(args)
            .write_stdin("")
            .timeout(Duration::from_secs(60))
            .output()
            .unwrap();
        let over_host = remote
            .tendr(&root)
            .args(args)
            .write_stdin("")
            .output()
            .unwrap();
        remote.assert_constant_argv();
        assert_eq!(outcome(&over_host), outcome(&local), "{args:?}");
    }
    assert!(!marker.exists(), "--since was executed");

    // watch streams NDJSON: its snapshot matches line for line.
    let snapshot = |child: &mut Child| -> Vec<String> {
        let lines = stdout_lines(child, 2);
        (0..2)
            .map(|_| lines.recv_timeout(STREAM_DEADLINE).expect("snapshot line"))
            .collect()
    };
    let mut local = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args(["watch", "--namespace", "golden"])
        .env("HOME", root.path())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let local_snapshot = snapshot(&mut local);
    let _ = local.kill();
    let _ = local.wait();
    let mut over_host = remote.spawn(&root, &["watch", "--namespace", "golden"], Stdio::null());
    assert_eq!(snapshot(&mut over_host), local_snapshot);
    // A new record makes the remote watch write to the pipe the reader let
    // go of, so it fails and exits, and the call with it.
    tendr(&root)
        .args([
            "start",
            "--namespace",
            "golden",
            "poke",
            "--",
            "echo",
            "poke",
        ])
        .assert()
        .success();
    wait_exit(&mut over_host, STREAM_DEADLINE, "watch");
}

// -- streaming --

/// `log --follow` over `--host` streams: the first line arrives while the
/// remote command still runs, and the call ends when the session does.
#[test]
fn host_log_follow_streams() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "--stdin", "follow", "--", "cat"])
        .assert()
        .success();
    let _session = SessionGuard::new(&root, "follow");
    wait_running(&root, "follow");
    tendr(&root)
        .args(["push", "follow"])
        .write_stdin("first-line\n")
        .assert()
        .success();

    let remote = Remote::new();
    let mut child = remote.spawn(
        &root,
        &["log", "--follow", "--tail", "10", "--raw", "follow"],
        Stdio::null(),
    );
    let lines = stdout_lines(&mut child, usize::MAX);
    let first = lines.recv_timeout(STREAM_DEADLINE).expect("first line");
    assert_eq!(first, "first-line");
    assert!(child.try_wait().unwrap().is_none(), "still following");
    remote.assert_constant_argv();

    tendr(&root).args(["kill", "follow"]).assert().success();
    let status = wait_exit(&mut child, STREAM_DEADLINE, "log --follow");
    assert_eq!(status.code(), Some(0));
}

/// `watch` over `--host` streams: the first record arrives while the remote
/// watch still runs.
#[test]
fn host_watch_streams() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "--namespace", "watched", "w", "--", "sleep", "30"])
        .assert()
        .success();
    let _session = NsGuard::new(&root, "watched", "w");

    let remote = Remote::new();
    let mut child = remote.spawn(
        &root,
        &["watch", "--namespace", "watched", "--events"],
        Stdio::null(),
    );
    // The reader lets go after one line, so the remote watch fails its next
    // write once the session ends, and exits.
    let lines = stdout_lines(&mut child, 1);
    let first = lines.recv_timeout(STREAM_DEADLINE).expect("first record");
    let record: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(record["session"], "w");
    assert!(child.try_wait().unwrap().is_none(), "still watching");
    remote.assert_constant_argv();

    tendr(&root)
        .args(["kill", "--namespace", "watched", "w"])
        .assert()
        .success();
    wait_exit(&mut child, STREAM_DEADLINE, "watch");
}

// -- push: the body follows the header --

/// Start `name` with a child that copies its stdin to a file; returns the
/// file.
fn start_copier<'a>(root: &'a TempDir, name: &str) -> (SessionGuard<'a>, PathBuf) {
    let out = root.path().join(format!("{name}.bin"));
    tendr(root)
        .args(["start", "--stdin", name, "--"])
        .arg(harness::test_callback_bin())
        .arg("copy-stdin")
        .arg(&out)
        .assert()
        .success();
    let guard = SessionGuard::new(root, name);
    wait_running(root, name);
    (guard, out)
}

/// Every byte value, and more than a MiB of them, reach the session exactly.
#[test]
fn host_push_preserves_binary_bytes() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let (_session, out) = start_copier(&root, "binary");
    let body: Vec<u8> = (0..=255u8).cycle().take((1 << 20) + 4321).collect();

    let remote = Remote::new();
    let output = remote
        .tendr(&root)
        .args(["push", "binary"])
        .write_stdin(body.clone())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", outcome(&output));
    remote.assert_constant_argv();

    wait_for(STREAM_DEADLINE, "every pushed byte", || {
        std::fs::metadata(&out).is_ok_and(|m| m.len() >= body.len() as u64)
    });
    assert!(std::fs::read(&out).unwrap() == body, "bytes differ");
}

/// push streams: what is written reaches the session while local stdin is
/// still open.
#[test]
fn host_push_streams_before_stdin_ends() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let (_session, out) = start_copier(&root, "streamed");

    let remote = Remote::new();
    let mut child = remote.spawn(&root, &["push", "streamed"], Stdio::piped());
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"chunk-1\n").unwrap();
    stdin.flush().unwrap();
    wait_for(STREAM_DEADLINE, "the first chunk", || {
        std::fs::read(&out).is_ok_and(|b| b == b"chunk-1\n")
    });
    assert!(child.try_wait().unwrap().is_none(), "push still running");

    stdin.write_all(b"chunk-2\n").unwrap();
    drop(stdin);
    let status = wait_exit(&mut child, STREAM_DEADLINE, "push");
    assert_eq!(status.code(), Some(0));
    remote.assert_constant_argv();
    wait_for(STREAM_DEADLINE, "both chunks", || {
        std::fs::read(&out).is_ok_and(|b| b == b"chunk-1\nchunk-2\n")
    });
}

/// A PTY session takes the pushed body over its attach socket.
#[cfg(unix)]
#[test]
fn host_push_reaches_a_pty_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "--pty", "pty", "--", "cat"])
        .assert()
        .success();
    let _session = SessionGuard::new(&root, "pty");
    wait_running(&root, "pty");

    let remote = Remote::new();
    let output = remote
        .tendr(&root)
        .args(["push", "pty"])
        .write_stdin("pushed-over-host\n")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", outcome(&output));
    remote.assert_constant_argv();
    wait_for(STREAM_DEADLINE, "the pushed line in the log", || {
        let log = tendr(&root).args(["log", "--raw", "pty"]).output().unwrap();
        String::from_utf8_lossy(&log.stdout).contains("pushed-over-host")
    });
}

// -- exec: the NDJSON target over `--host` --

/// `start --exec-target ndjson` reaches the remote meta.json, and `exec`
/// through the exec frame gets the reply to its own id; a request with its
/// own `id` is refused with exit 2 as it is locally.
#[test]
fn host_ndjson_exec_matches_its_reply() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let remote = Remote::new();
    let output = remote
        .tendr(&root)
        .args(["start", "--stdin", "--exec-target", "ndjson", "dev", "--"])
        .arg(harness::test_callback_bin())
        .arg("ndjson-echo")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", outcome(&output));
    remote.assert_constant_argv();
    let _session = SessionGuard::new(&root, "dev");
    wait_running(&root, "dev");
    let meta = read_json(&meta_path(&root, "default", "dev"));
    assert_eq!(meta["launch_spec"]["exec_target"], "Ndjson");

    let output = remote
        .tendr(&root)
        .args(["exec", "dev", "--timeout", "20", "--", r#"{"cmd":"read"}"#])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", outcome(&output));
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let reply: serde_json::Value =
        serde_json::from_str(result["stdout"].as_str().unwrap()).unwrap();
    assert_eq!(reply["cmd"], "read", "{result}");
    assert_eq!(result["unmatched"], serde_json::json!([]), "{result}");

    let output = remote
        .tendr(&root)
        .args(["exec", "dev", "--", r#"{"cmd":"read","id":"mine"}"#])
        .output()
        .unwrap();
    let (code, _, stderr) = outcome(&output);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("must not have an \"id\""), "{stderr}");
}

// -- `tendr _remote`: bad requests fail with exit 2 and change nothing --

/// A frame: the 4-byte big-endian length, then `json`.
fn frame(json: &[u8]) -> Vec<u8> {
    let mut bytes = u32::try_from(json.len()).unwrap().to_be_bytes().to_vec();
    bytes.extend_from_slice(json);
    bytes
}

/// A complete start request, with `changes` merged into its params.
fn start_frame(changes: serde_json::Value) -> Vec<u8> {
    let mut params = serde_json::json!({
        "session": "victim", "namespace": null, "argv": ["true"], "cwd": null,
        "env": [], "unset_env": [], "stdin": false, "timeout": null,
        "exec_target": null, "pty": false, "replace": true, "on_exit": [],
        "after": [], "any_exit": false, "boundary": null, "boundary_parent": []
    });
    for (key, value) in changes.as_object().unwrap() {
        params[key] = value.clone();
    }
    frame(
        &serde_json::to_vec(&serde_json::json!({"v": 1, "op": "start", "params": params})).unwrap(),
    )
}

fn run_remote_entry(root: &TempDir, stdin: Vec<u8>) -> Output {
    tendr(root)
        .arg("_remote")
        .write_stdin(stdin)
        .timeout(Duration::from_secs(60))
        .output()
        .unwrap()
}

/// The sessions on disk, by namespace and name.
fn sessions_on_disk(root: &TempDir) -> Vec<String> {
    let sessions = root.path().join(".tendr").join("sessions");
    let mut found = Vec::new();
    for ns in std::fs::read_dir(&sessions).unwrap() {
        let ns = ns.unwrap();
        for session in std::fs::read_dir(ns.path()).unwrap() {
            found.push(format!(
                "{}/{}",
                ns.file_name().to_string_lossy(),
                session.unwrap().file_name().to_string_lossy()
            ));
        }
    }
    found.sort();
    found
}

/// Malformed, truncated, oversized, unsupported and semantically invalid
/// requests are refused with exit 2 and a clear message before anything
/// happens: the running `victim` that a bad `--replace` start names keeps
/// running, and no session appears.
#[test]
fn remote_entry_refuses_bad_requests_without_side_effects() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let started = tendr(&root)
        .args(["start", "victim", "--", "sleep", "30"])
        .output()
        .unwrap();
    let _victim = SessionGuard::new(&root, "victim");
    assert!(started.status.success(), "{:?}", outcome(&started));
    let run_id =
        serde_json::from_slice::<serde_json::Value>(&started.stdout).unwrap()["run_id"].clone();
    wait_running(&root, "victim");

    let oversized = ((1u32 << 20) + 1).to_be_bytes().to_vec();
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("empty stdin", vec![], "ended before"),
        ("short length", vec![0, 0], "ended before"),
        ("short header", {
            let mut b = 100u32.to_be_bytes().to_vec();
            b.extend_from_slice(br#"{"v":1"#);
            b
        }, "ended before"),
        ("oversized header", oversized, "exceeds"),
        ("malformed JSON", frame(b"{not json"), "invalid remote request"),
        ("not an object", frame(b"[1,2]"), "invalid remote request"),
        (
            "unknown version",
            frame(br#"{"v":2,"op":"list","params":{"namespace":null}}"#),
            "unsupported remote request version 2",
        ),
        (
            "unknown operation",
            frame(br#"{"v":1,"op":"explode","params":{}}"#),
            "unknown remote operation 'explode'",
        ),
        (
            "missing field",
            frame(br#"{"v":1,"op":"kill","params":{"namespace":null,"force":true}}"#),
            "missing field `session`",
        ),
        (
            "invalid session name",
            start_frame(serde_json::json!({"session": "bad/name"})),
            "session name",
        ),
        (
            "invalid namespace",
            frame(br#"{"v":1,"op":"kill","params":{"session":"victim","namespace":"bad/ns","force":true}}"#),
            "namespace",
        ),
        (
            "invalid kill target",
            frame(br#"{"v":1,"op":"kill","params":{"session":"bad.name","namespace":null,"force":true}}"#),
            "session name",
        ),
        (
            "malformed env",
            start_frame(serde_json::json!({"env": ["NOEQUALS"]})),
            "KEY=VALUE",
        ),
        (
            "empty env key",
            start_frame(serde_json::json!({"env": ["=v"]})),
            "key cannot be empty",
        ),
        (
            "malformed unset-env",
            start_frame(serde_json::json!({"unset_env": ["A=B"]})),
            "--unset-env",
        ),
        (
            "empty argv",
            start_frame(serde_json::json!({"argv": []})),
            "no command",
        ),
        (
            "invalid after",
            start_frame(serde_json::json!({"after": ["bad/dep"]})),
            "session name",
        ),
        (
            "any_exit without after",
            start_frame(serde_json::json!({"any_exit": true})),
            "--any-exit",
        ),
        (
            "empty boundary label",
            start_frame(serde_json::json!({"boundary": {"kind": "host", "label": ""}})),
            "boundary label",
        ),
        (
            "boundary parent without boundary",
            start_frame(serde_json::json!({"boundary_parent": [{"kind": "vm", "label": "v"}]})),
            "--boundary-parent",
        ),
        (
            "wait without sessions",
            frame(br#"{"v":1,"op":"wait","params":{"sessions":[],"namespace":null,"timeout":null,"any":false}}"#),
            "session",
        ),
    ];
    for (what, stdin, expected) in cases {
        let output = run_remote_entry(&root, stdin);
        let (code, stdout, stderr) = outcome(&output);
        assert_eq!(code, Some(2), "{what}: {stderr}");
        assert_eq!(stdout, "", "{what}");
        assert!(
            stderr.starts_with("tendr _remote: ") && stderr.contains(expected),
            "{what}: expected {expected:?} in {stderr:?}"
        );
        assert_eq!(sessions_on_disk(&root), ["default/victim"], "{what}");
    }
    let meta = read_json(&meta_path(&root, "default", "victim"));
    assert_eq!(meta["status"], "Running", "victim untouched");
    assert_eq!(meta["run_id"], run_id, "victim not replaced");
}

/// Unknown fields, in the envelope or the params, are ignored: a request
/// from a newer client still runs, with the local command's output.
#[test]
fn remote_entry_ignores_unknown_fields() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let output = run_remote_entry(
        &root,
        frame(
            br#"{"v":1,"op":"kill","params":{"session":"ghost","namespace":null,"force":false,"future":1},"extra":{"x":[1]}}"#,
        ),
    );
    let local = tendr(&root).args(["kill", "ghost"]).output().unwrap();
    assert_eq!(outcome(&output), outcome(&local));
    assert_eq!(output.status.code(), Some(0));
}

// -- `--exit-trailer`: the remote end of the exit trailer --

/// Run `tendr --exit-trailer <args>` here and return its outcome.
fn with_trailer(root: &TempDir, args: &[&str]) -> (Option<i32>, String, String) {
    let output = tendr(root)
        .arg("--exit-trailer")
        .args(args)
        .write_stdin("")
        .timeout(Duration::from_secs(60))
        .output()
        .unwrap();
    outcome(&output)
}

/// The command runs as without the flag, then its exit code is written as
/// the last stderr line, and the process exits with it too.
#[test]
fn exit_trailer_is_the_last_stderr_line_and_matches_the_exit_code() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "failed", "--", "false"])
        .assert()
        .success();
    let _failed = SessionGuard::new(&root, "failed");
    tendr(&root)
        .args(["start", "running", "--", "sleep", "30"])
        .assert()
        .success();
    let _running = SessionGuard::new(&root, "running");

    let cases: [(&[&str], i32); 4] = [
        (&["wait", "failed"], 42),
        (&["wait", "running", "--timeout", "1"], 1),
        (&["start", "--after", "ghost", "lost", "--", "true"], 1),
        (&["status", "ghost"], 1),
    ];
    for (args, code) in cases {
        let local = tendr(&root)
            .args(args)
            .write_stdin("")
            .timeout(Duration::from_secs(60))
            .output()
            .unwrap();
        let (got, stdout, stderr) = with_trailer(&root, args);
        assert_eq!(got, Some(code), "{args:?}: {stderr}");
        assert_eq!(local.status.code(), Some(code), "{args:?}");
        assert_eq!(
            stderr,
            format!(
                "{}tendr-exit {code}\n",
                String::from_utf8_lossy(&local.stderr)
            ),
            "{args:?}"
        );
        if args[0] != "wait" {
            assert_eq!(stdout, String::from_utf8_lossy(&local.stdout), "{args:?}");
        }
    }
}

/// With `--hello` too, the hello is written once, first, and the trailer
/// last.
#[test]
fn exit_trailer_keeps_the_hello_first_and_single() {
    let root = TempDir::new().unwrap();
    let output = tendr(&root)
        .args(["--hello", "--exit-trailer", "status", "missing"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert_eq!(
        stderr,
        format!(
            "{}\nsession not found: missing\ntendr-exit 1\n",
            tendr::ssh::HELLO_LINE
        )
    );
}

/// The wrapper leaves stdin to the command: a push body reaches the session
/// whole.
#[test]
fn exit_trailer_leaves_stdin_to_the_command() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let (_session, out) = start_copier(&root, "wrapped");
    let body: Vec<u8> = (0..=255u8).cycle().take(200_000).collect();

    let output = tendr(&root)
        .args(["--exit-trailer", "push", "wrapped"])
        .write_stdin(body.clone())
        .timeout(Duration::from_secs(60))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", outcome(&output));
    assert_eq!(String::from_utf8_lossy(&output.stderr), "tendr-exit 0\n");
    wait_for(STREAM_DEADLINE, "every pushed byte", || {
        std::fs::metadata(&out).is_ok_and(|m| m.len() >= body.len() as u64)
    });
    assert!(std::fs::read(&out).unwrap() == body, "bytes differ");
}
