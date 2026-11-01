//! `--host` against a real Windows OpenSSH server, under whichever login
//! shell it is configured with (cmd.exe by default, or PowerShell through
//! `HKLM:\SOFTWARE\OpenSSH\DefaultShell`). Every value a user types must reach
//! the remote tendr verbatim and must never be run by that shell.
//!
//! The tests are opt-in; CI's `windows-openssh` job sets them up on the
//! Windows x64 and ARM64 lanes. Without `TENDR_TEST_WIN_SSH_HOST` each test
//! passes at once, printing that it was skipped. The setup they need:
//!
//! - `TENDR_TEST_WIN_SSH_HOST`: an ssh destination on THIS machine that logs
//!   in without prompting (an ssh config alias, for example). Its user has
//!   `tendr` and `test_callback` (this build) on its PATH.
//! - `TENDR_TEST_WIN_SSH_SCRATCH`: a directory both users can write. An
//!   injected command would create a file in its `m` subdirectory, which the
//!   tests check from this side.
//! - `TENDR_TEST_WIN_SSH_SHELL`: a label for the configured shell, used in
//!   namespaces and messages so both shells can share the remote user's state.
//!
//! Each test reports every value that failed, not only the first.
#![cfg(windows)]

mod harness;

use harness::{Observation, poll_until};
use serde_json::Value;
use std::path::PathBuf;
use std::process::Output;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static SERIAL: Mutex<()> = Mutex::new(());

/// Bound on one `tendr --host` call: ssh password login plus a remote tendr
/// on a loaded runner, with room to spare. A hang fails the test.
const CALL_DEADLINE: Duration = Duration::from_secs(90);

/// The remote host, the scratch directory, and this test's namespace.
struct Remote {
    host: String,
    shell: String,
    namespace: String,
    scratch: PathBuf,
}

impl Remote {
    /// `None`, after printing a skip note, when `TENDR_TEST_WIN_SSH_HOST` is
    /// unset. Empties the marker directory.
    fn connect(test: &str) -> Option<Self> {
        let nonempty = |name| std::env::var(name).ok().filter(|v: &String| !v.is_empty());
        let Some(host) = nonempty("TENDR_TEST_WIN_SSH_HOST") else {
            eprintln!("skipped {test}: TENDR_TEST_WIN_SSH_HOST is not set");
            return None;
        };
        let scratch = PathBuf::from(
            nonempty("TENDR_TEST_WIN_SSH_SCRATCH")
                .expect("TENDR_TEST_WIN_SSH_SCRATCH must be set with TENDR_TEST_WIN_SSH_HOST"),
        );
        let shell = nonempty("TENDR_TEST_WIN_SSH_SHELL").unwrap_or_else(|| "shell".to_owned());
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let remote = Self {
            host,
            namespace: format!("{shell}-{nanos}"),
            shell,
            scratch,
        };
        let markers = remote.markers();
        if markers.exists() {
            std::fs::remove_dir_all(&markers).expect("empty the marker directory");
        }
        std::fs::create_dir_all(&markers).expect("create the marker directory");
        Some(remote)
    }

    /// Where an injected command in these tests writes its file.
    fn markers(&self) -> PathBuf {
        self.scratch.join("m")
    }

    /// The marker directory as text, for building payloads.
    fn m(&self) -> String {
        self.markers().display().to_string()
    }

    /// Files an injected command created, removed so the next check starts
    /// clean.
    fn take_markers(&self) -> Vec<String> {
        let found: Vec<String> = std::fs::read_dir(self.markers())
            .expect("read the marker directory")
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        for name in &found {
            let _ = std::fs::remove_file(self.markers().join(name));
        }
        found
    }

    /// Run `tendr --host <host> <args>` here, feeding `stdin`.
    fn tendr_with_stdin(&self, args: &[&str], stdin: &[u8]) -> Output {
        assert_cmd::Command::cargo_bin("tendr")
            .unwrap()
            .args(["--host", &self.host])
            .args(args)
            .write_stdin(stdin)
            .timeout(CALL_DEADLINE)
            .output()
            .unwrap_or_else(|e| panic!("tendr --host {} {args:?} did not run: {e}", self.host))
    }

    fn tendr(&self, args: &[&str]) -> Output {
        self.tendr_with_stdin(args, b"")
    }

    /// `status` of `session` in this namespace, parsed; `Err` describes a
    /// failed call or unparseable output.
    fn status(&self, session: &str) -> Result<Value, String> {
        let output = self.tendr(&["status", session, "--namespace", &self.namespace]);
        if !output.status.success() {
            return Err(describe(&output));
        }
        serde_json::from_slice(&output.stdout)
            .map_err(|e| format!("status output is not JSON ({e}): {}", describe(&output)))
    }

    /// Start `session` in this namespace with `flags`, running `argv`.
    fn start(&self, session: &str, flags: &[&str], argv: &[&str]) -> Output {
        let mut args = vec!["start", session, "--namespace", &self.namespace];
        args.extend_from_slice(flags);
        args.push("--");
        args.extend_from_slice(argv);
        self.tendr(&args)
    }

    /// Wait for `session` to reach `status` (`Running`, `Exited`, …).
    fn await_status(&self, session: &str, status: &str) -> Result<Value, String> {
        poll_until(
            Duration::from_secs(60),
            Duration::from_millis(500),
            || match self.status(session) {
                Ok(meta) if meta["status"] == status => Observation::Ready(meta),
                Ok(meta) => Observation::Pending(meta.to_string()),
                Err(e) => Observation::Pending(e),
            },
        )
        .map_err(|e| format!("{session} never reached {status}: {e}"))
    }

    fn kill(&self, session: &str) {
        let _ = self.tendr(&["kill", session, "--namespace", &self.namespace, "--force"]);
    }
}

fn describe(output: &Output) -> String {
    format!(
        "exit {:?}; stdout {:?}; stderr {:?}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Every failure a test found, reported together at the end.
#[derive(Default)]
struct Failures(Vec<String>);

impl Failures {
    fn check(&mut self, ok: bool, what: impl FnOnce() -> String) {
        if !ok {
            self.0.push(what());
        }
    }

    /// Record any file an injected command created while doing `what`.
    fn check_markers(&mut self, remote: &Remote, what: &str) {
        let found = remote.take_markers();
        self.check(found.is_empty(), || {
            format!("INJECTED: {what} made the remote shell create {found:?}")
        });
    }

    fn assert_none(self, remote: &Remote, test: &str) {
        assert!(
            self.0.is_empty(),
            "{test} under the {} login shell: {} failure(s):\n- {}",
            remote.shell,
            self.0.len(),
            self.0.join("\n- ")
        );
    }
}

/// Values every shell treats as syntax somewhere, none of them whitespace,
/// so they are also valid session names. Each one that could run a command
/// writes into the marker directory if it does.
fn hostile_values(m: &str) -> Vec<String> {
    vec![
        format!("a&echo>{m}\\amp"),
        format!("a|echo>{m}\\pipe"),
        format!("a>{m}\\gt"),
        format!("a'&echo>{m}\\quote"),
        format!("a\"&echo>{m}\\dquote"),
        format!("a';$null>{m}\\ps;'"),
        format!("a';ni(\"{m}\\psni\");'"),
        format!("a$(ni(\"{m}\\subexpr\"))"),
        "a^&b^|c".to_owned(),
        "a%USERNAME%b".to_owned(),
        "a%PATH:~0,1%b".to_owned(),
        "a;b,c=d".to_owned(),
        "a$b$env:USERNAME".to_owned(),
        "a'b".to_owned(),
        "a\"b".to_owned(),
        "a`b".to_owned(),
        "a(b)c".to_owned(),
        "a!b!c".to_owned(),
        "a\\b".to_owned(),
        "é-ü-日本".to_owned(),
    ]
}

/// A name the remote tendr looks up and does not find comes back in the
/// error exactly as typed, whatever characters it holds (Windows file names
/// may not hold some of them, so nothing is created).
#[test]
fn session_names_arrive_as_data() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(remote) = Remote::connect("session_names_arrive_as_data") else {
        return;
    };
    let mut failures = Failures::default();
    for name in hostile_values(&remote.m()) {
        let output = remote.tendr(&["status", &name, "--namespace", &remote.namespace]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        failures.check(
            output.status.code() == Some(1)
                && stderr.contains(&format!("session not found: {name}")),
            || format!("status {name:?}: {}", describe(&output)),
        );
        failures.check_markers(&remote, &format!("status {name:?}"));
    }
    failures.assert_none(&remote, "session_names_arrive_as_data");
}

/// Hostile names that are valid Windows file names: `start` creates the
/// session under exactly that name, and `status` and `list` find it.
#[test]
fn started_sessions_keep_their_names() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(remote) = Remote::connect("started_sessions_keep_their_names") else {
        return;
    };
    let names = [
        "a&b",
        "a^b",
        "a%USERNAME%b",
        "a;b,c=d",
        "a$b",
        "a'b",
        "a`b",
        "a(b)c",
        "a!b!c",
        "é-ü",
    ];
    let mut failures = Failures::default();
    for name in names {
        let output = remote.start(name, &[], &["test_callback", "print-cwd"]);
        failures.check(output.status.success(), || {
            format!("start {name:?}: {}", describe(&output))
        });
        match remote.status(name) {
            Ok(meta) => failures.check(meta["session"] == name, || {
                format!("status {name:?} found session {}", meta["session"])
            }),
            Err(e) => failures.0.push(format!("status {name:?}: {e}")),
        }
        failures.check_markers(&remote, &format!("start/status {name:?}"));
    }
    let output = remote.tendr(&["list", "--namespace", &remote.namespace]);
    match serde_json::from_slice::<Value>(&output.stdout) {
        Ok(Value::Array(entries)) => {
            let mut listed: Vec<&str> = entries.iter().filter_map(|e| e["name"].as_str()).collect();
            listed.sort_unstable();
            let mut expected = names.to_vec();
            expected.sort_unstable();
            failures.check(listed == expected, || {
                format!("list shows {listed:?}, expected {expected:?}")
            });
        }
        _ => failures.0.push(format!("list: {}", describe(&output))),
    }
    failures.assert_none(&remote, "started_sessions_keep_their_names");
}

/// The child's argv, its working directory (spaces and shell syntax) and its
/// environment values reach the remote run as typed, and the child runs in
/// that directory.
#[test]
fn start_argv_cwd_and_env_arrive_verbatim() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(remote) = Remote::connect("start_argv_cwd_and_env_arrive_verbatim") else {
        return;
    };
    let m = remote.m();
    let cwd = remote
        .scratch
        .join(format!("{} cwd & (x) 'y' %USERNAME% $z", remote.shell));
    std::fs::create_dir_all(&cwd).expect("create the cwd");
    let cwd_text = cwd.display().to_string();
    let env = [
        ("TENDR_T_AMP", format!("a&echo>{m}\\envamp")),
        ("TENDR_T_PS", format!("x';$null>{m}\\envps;'")),
        (
            "TENDR_T_MIX",
            "%USERNAME% $HOME \"q\" `b` a|b ^c".to_owned(),
        ),
    ];
    let env_flags: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let mut flags = vec!["--cwd", cwd_text.as_str()];
    for flag in &env_flags {
        flags.extend(["--env", flag.as_str()]);
    }
    let mut argv = vec!["test_callback".to_owned(), "print-cwd".to_owned()];
    argv.extend(hostile_values(&m));
    argv.extend([
        "two words".to_owned(),
        String::new(),
        "trailing\\".to_owned(),
    ]);
    let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();

    let mut failures = Failures::default();
    let session = "argv";
    let output = remote.start(session, &flags, &argv_refs);
    failures.check(output.status.success(), || {
        format!("start: {}", describe(&output))
    });
    match remote.await_status(session, "Exited") {
        Ok(meta) => {
            let spec = &meta["launch_spec"];
            failures.check(spec["argv"] == serde_json::json!(argv), || {
                format!("argv arrived as {}", spec["argv"])
            });
            failures.check(spec["cwd"] == cwd_text.as_str(), || {
                format!("cwd arrived as {}, sent {cwd_text:?}", spec["cwd"])
            });
            for (key, value) in &env {
                failures.check(spec["env"][key] == value.as_str(), || {
                    format!("env {key} arrived as {}, sent {value:?}", spec["env"][key])
                });
            }
            failures.check(meta["reason"] == "ExitedOk", || {
                format!("the child did not exit 0: {meta}")
            });
        }
        Err(e) => failures.0.push(e),
    }
    let output = remote.tendr(&["log", session, "--namespace", &remote.namespace, "--raw"]);
    failures.check(
        String::from_utf8_lossy(&output.stdout).contains(&cwd_text),
        || {
            format!(
                "the child did not print its cwd {cwd_text:?}: {}",
                describe(&output)
            )
        },
    );
    failures.check_markers(&remote, "start with hostile argv, cwd and env");
    failures.assert_none(&remote, "start_argv_cwd_and_env_arrive_verbatim");
}

/// `log --since` is parsed by the remote tendr, never by the shell: an
/// invalid value comes back in tendr's own error, as typed.
#[test]
fn log_since_arrives_as_data() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(remote) = Remote::connect("log_since_arrives_as_data") else {
        return;
    };
    let m = remote.m();
    let mut failures = Failures::default();
    let session = "since";
    let output = remote.start(session, &[], &["test_callback", "print-cwd"]);
    failures.check(output.status.success(), || {
        format!("start: {}", describe(&output))
    });
    if let Err(e) = remote.await_status(session, "Exited") {
        failures.0.push(e);
    }
    let output = remote.tendr(&[
        "log",
        session,
        "--namespace",
        &remote.namespace,
        "--since",
        "1h",
    ]);
    failures.check(output.status.success() && !output.stdout.is_empty(), || {
        format!("log --since 1h: {}", describe(&output))
    });
    let values = [
        format!("1&echo>{m}\\sinceamp"),
        format!("1|echo>{m}\\sincepipe"),
        format!("1';$null>{m}\\sinceps;'"),
        format!("1 & echo>{m}\\sincespace"),
        "%USERNAME%".to_owned(),
        "1\"2".to_owned(),
    ];
    for since in values {
        let output = remote.tendr(&[
            "log",
            session,
            "--namespace",
            &remote.namespace,
            "--since",
            &since,
        ]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        failures.check(
            output.status.code() == Some(1)
                && stderr.contains(&format!("invalid since value: {since:?}")),
            || format!("log --since {since:?}: {}", describe(&output)),
        );
        failures.check_markers(&remote, &format!("log --since {since:?}"));
    }
    failures.assert_none(&remote, "log_since_arrives_as_data");
}

/// `push` bytes reach the child's stdin, `wait` and `status` report how
/// runs ended, `kill` stops one, and exit codes come back through ssh.
#[test]
fn push_wait_kill_and_exit_codes_round_trip() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(remote) = Remote::connect("push_wait_kill_and_exit_codes_round_trip") else {
        return;
    };
    let ns = remote.namespace.clone();
    let mut failures = Failures::default();

    // push: the child prints the first line it reads.
    let line = "pushed & | % $ ' \" ^ ` é";
    let output = remote.start(
        "echo",
        &["--stdin"],
        &[
            "powershell",
            "-NoProfile",
            "-Command",
            "[Console]::In.ReadLine()",
        ],
    );
    failures.check(output.status.success(), || {
        format!("start echo: {}", describe(&output))
    });
    match remote.await_status("echo", "Running") {
        Ok(_) => {
            let output = remote.tendr_with_stdin(
                &["push", "echo", "--namespace", &ns],
                format!("{line}\r\n").as_bytes(),
            );
            failures.check(output.status.success(), || {
                format!("push: {}", describe(&output))
            });
            let output = remote.tendr(&["wait", "echo", "--namespace", &ns, "--timeout", "60"]);
            failures.check(output.status.code() == Some(0), || {
                format!("wait echo: {}", describe(&output))
            });
            let output = remote.tendr(&["log", "echo", "--namespace", &ns, "--raw"]);
            failures.check(
                String::from_utf8_lossy(&output.stdout).contains(line),
                || {
                    format!(
                        "the pushed line {line:?} is not in the log: {}",
                        describe(&output)
                    )
                },
            );
        }
        Err(e) => failures.0.push(e),
    }

    // A child's non-zero exit: `wait` exits 42 and `status` holds the code.
    let output = remote.start("code", &[], &["cmd", "/d", "/c", "exit", "7"]);
    failures.check(output.status.success(), || {
        format!("start code: {}", describe(&output))
    });
    let output = remote.tendr(&["wait", "code", "--namespace", &ns, "--timeout", "60"]);
    failures.check(output.status.code() == Some(42), || {
        format!(
            "wait for a child that exits 7 should exit 42: {}",
            describe(&output)
        )
    });
    match remote.status("code") {
        Ok(meta) => failures.check(meta["reason"] == "ExitedError" && meta["code"] == 7, || {
            format!("status of a child that exits 7: {meta}")
        }),
        Err(e) => failures.0.push(format!("status code: {e}")),
    }

    // kill a long run; wait then sees it ended.
    let output = remote.start("long", &[], &["ping", "-n", "300", "127.0.0.1"]);
    failures.check(output.status.success(), || {
        format!("start long: {}", describe(&output))
    });
    match remote.await_status("long", "Running") {
        Ok(_) => {
            let output = remote.tendr(&["kill", "long", "--namespace", &ns]);
            failures.check(output.status.success(), || {
                format!("kill long: {}", describe(&output))
            });
            let output = remote.tendr(&["wait", "long", "--namespace", &ns, "--timeout", "60"]);
            failures.check(output.status.code().is_some_and(|c| c != 1), || {
                format!("wait after kill: {}", describe(&output))
            });
            match remote.status("long") {
                Ok(meta) => failures.check(meta["status"] == "Exited", || {
                    format!("status after kill: {meta}")
                }),
                Err(e) => failures.0.push(format!("status long: {e}")),
            }
        }
        Err(e) => {
            failures.0.push(e);
            remote.kill("long");
        }
    }

    // A remote error exits 1 here.
    let output = remote.tendr(&["status", "missing", "--namespace", &ns]);
    failures.check(output.status.code() == Some(1), || {
        format!("status of a missing session: {}", describe(&output))
    });
    failures.check_markers(&remote, "the round trip");
    failures.assert_none(&remote, "push_wait_kill_and_exit_codes_round_trip");
}

/// A run started over one ssh connection is still running after that
/// connection has closed: Windows OpenSSH must not take the sidecar or the
/// child down with the session.
#[test]
fn run_outlives_the_ssh_session_that_started_it() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(remote) = Remote::connect("run_outlives_the_ssh_session_that_started_it") else {
        return;
    };
    let session = "outlives";
    let output = remote.start(session, &[], &["ping", "-n", "300", "127.0.0.1"]);
    assert!(output.status.success(), "start: {}", describe(&output));
    std::thread::sleep(Duration::from_secs(10));
    let meta = remote.status(session);
    remote.kill(session);
    let meta = meta.unwrap_or_else(|e| panic!("status after the ssh session ended: {e}"));
    assert_eq!(
        meta["status"], "Running",
        "under the {} login shell the run did not survive the end of the ssh \
         session that started it: {meta}",
        remote.shell
    );
    eprintln!(
        "{}: run still Running 10 s after its ssh session ended",
        remote.shell
    );
}
