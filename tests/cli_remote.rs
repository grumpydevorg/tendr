// Remote SSH transport tests. The `ssh` they reach is the portable fake in
// `src/bin/fake_ssh.rs`, put on a temp PATH by `harness::fake_ssh`. The
// end-to-end frame tests, through the real `tendr _remote`, are in
// `cli_remote_frame.rs`.

mod harness;

use harness::{fake_ssh, shim_path_env};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

/// A fake ssh that prints each of its args as `ARG:<value>` on stdout.
fn fake_ssh_echo() -> TempDir {
    fake_ssh("args")
}

/// A fake ssh that exits 0 immediately (no-op).
fn fake_ssh_noop() -> TempDir {
    fake_ssh("exit 0")
}

/// A running process, watched from while it is known to be alive so a later
/// check cannot be fooled by its pid being reused.
#[cfg(unix)]
struct ProcessWatch(i32);

#[cfg(unix)]
impl ProcessWatch {
    fn new(pid: u32) -> Self {
        Self(i32::try_from(pid).unwrap())
    }

    /// Whether the process is still running.
    fn alive(&self) -> bool {
        // SAFETY: signal 0 only probes whether the process still exists.
        // Pids are reused only after the counter wraps, which a test lasting
        // a few seconds does not reach in practice.
        unsafe { libc::kill(self.0, 0) == 0 }
    }
}

/// A running process, watched from while it is known to be alive so a later
/// check cannot be fooled by its pid being reused.
#[cfg(windows)]
struct ProcessWatch(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl ProcessWatch {
    fn new(pid: u32) -> Self {
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: plain FFI call; the result is checked.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        assert!(!handle.is_null(), "open process {pid}");
        Self(handle)
    }

    /// Whether the process is still running. The open handle pins the
    /// process object, so this is the same process even if the pid is reused.
    fn alive(&self) -> bool {
        use windows_sys::Win32::Foundation::STILL_ACTIVE;
        use windows_sys::Win32::System::Threading::GetExitCodeProcess;
        let mut code = 0u32;
        // SAFETY: `self.0` is a valid process handle until drop.
        let ok = unsafe { GetExitCodeProcess(self.0, &mut code) } != 0;
        assert!(ok, "GetExitCodeProcess");
        code == STILL_ACTIVE as u32
    }
}

#[cfg(windows)]
impl Drop for ProcessWatch {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from OpenProcess and is closed once.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

#[test]
fn host_flag_is_accepted_by_parser() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // Use a fake ssh shim so we don't hit real SSH (ConnectTimeout).
    let tmp = fake_ssh_noop();

    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args(["--host", "user@example.com", "list"])
        .env("PATH", tmp.path())
        .output()
        .unwrap();

    // Should not fail with "unexpected argument '--host'"
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("unexpected argument"),
        "parser should accept --host, got: {stderr}"
    );
}

/// The ssh argv of every `--host` operation but exec (and a Windows client's
/// attach): constant, so nothing the user typed reaches the remote login
/// shell.
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

/// A value that a POSIX shell, cmd.exe or PowerShell would act on.
const HOSTILE: &str = "x&calc|whoami;$(id)`id`'\"(a) b\r\n\u{e9} C:\\Users\\o'neil";

/// The ssh argv a fake ssh with the `args` action printed.
fn printed_args(stdout: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|l| l.strip_prefix("ARG:"))
        .map(str::to_owned)
        .collect()
}

/// Every operation, with a hostile value in every place a value can go, runs
/// the same ssh argv: the request itself travels on ssh's stdin.
#[test]
fn every_host_operation_sends_the_same_constant_argv() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = fake_ssh("args");
    let h = HOSTILE;
    let env = format!("K={h}");
    let boundary = format!("host:{h}");
    let cases: [&[&str]; 8] = [
        &[
            "start",
            "--namespace",
            h,
            "--stdin",
            "--pty",
            "--replace",
            "--timeout",
            "5",
            "--cwd",
            h,
            "--env",
            &env,
            "--unset-env",
            h,
            "--on-exit",
            h,
            "--after",
            h,
            "--any-exit",
            "--exec-target",
            "powershell",
            "--boundary",
            &boundary,
            "--boundary-parent",
            &boundary,
            h,
            "--",
            h,
            h,
        ],
        &["status", "--namespace", h, h],
        &["list", "--namespace", h],
        &[
            "log",
            "--namespace",
            h,
            "--tail",
            "3",
            "--follow",
            "--since",
            h,
            "--raw",
            h,
        ],
        &["push", "--namespace", h, h],
        &["kill", "--namespace", h, "--force", h],
        &["wait", "--namespace", h, "--timeout", "5", "--any", h, h],
        &[
            "watch",
            "--namespace",
            h,
            "--events",
            "--logs",
            "--annotations",
            "--from-now",
        ],
    ];
    for args in cases {
        let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
            .args(["--host", "box"])
            .args(args)
            .env("PATH", tmp.path())
            .env_remove("TENDR_SSH_CONNECT_TIMEOUT")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert_eq!(
            printed_args(&output.stdout),
            REMOTE_ARGV,
            "{}: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // attach from a Unix client runs the same argv. It needs a terminal on
    // stdin before ssh starts, so it runs in one, and its ssh stdout carries
    // the attach protocol, so the argv goes to a file.
    #[cfg(unix)]
    {
        use harness::{Observation, poll_until};
        let recorded = TempDir::new().unwrap();
        let argv_file = recorded.path().join("argv");
        let tmp = fake_ssh(&format!("args {}\nexit 3", argv_file.display()));
        let root = TempDir::new().unwrap();
        let argv: Vec<String> = [
            harness::terminal::tendr_bin().as_str(),
            "--host",
            "box",
            "attach",
            "--namespace",
            h,
            "--takeover",
            "--escape",
            "none",
            h,
        ]
        .map(str::to_owned)
        .to_vec();
        let path = shim_path_env(&tmp);
        let _cli = harness::terminal::CliAttach::spawn_argv_env(
            &root,
            &argv,
            &[
                ("PATH", path.to_str().unwrap()),
                ("TENDR_SSH_CONNECT_TIMEOUT", "30"),
            ],
        );
        let printed =
            poll_until(
                Duration::from_secs(15),
                Duration::from_millis(20),
                || match std::fs::read(&argv_file) {
                    Ok(bytes)
                        if printed_args(&bytes).last().map(String::as_str) == Some("_remote") =>
                    {
                        Observation::Ready(printed_args(&bytes))
                    }
                    _ => Observation::Pending("no argv yet".into()),
                },
            )
            .expect("attach ran ssh");
        assert_eq!(printed, REMOTE_ARGV, "attach");
    }
}

/// What crosses ssh's stdin: a 4-byte big-endian length, that many bytes of
/// JSON (`v`, `op`, `params`), and for push the body, byte for byte.
#[test]
fn host_request_travels_on_stdin_as_a_length_prefixed_frame() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let captured = TempDir::new().unwrap();
    let captured = captured.path().join("stdin");
    let tmp = fake_ssh(&format!("stdin {}\nexit 0", captured.display()));
    let body: Vec<u8> = (0..=255u8).cycle().take(3000).collect();
    let cases: [(&[&str], serde_json::Value, &[u8]); 3] = [
        (
            &["status", "--namespace", "n", "x&calc"],
            serde_json::json!({
                "v": 1, "op": "status",
                "params": {"session": "x&calc", "namespace": "n"}
            }),
            b"",
        ),
        (
            &["log", "--since", "$(id)", "s"],
            serde_json::json!({
                "v": 1, "op": "log",
                "params": {
                    "session": "s", "namespace": null, "tail": null,
                    "follow": false, "since": "$(id)", "raw": false
                }
            }),
            b"",
        ),
        (
            &["push", "s"],
            serde_json::json!({
                "v": 1, "op": "push",
                "params": {"session": "s", "namespace": null}
            }),
            &body,
        ),
    ];
    for (args, header, body) in cases {
        let output = assert_cmd::Command::cargo_bin("tendr")
            .unwrap()
            .args(["--host", "box"])
            .args(args)
            .env("PATH", tmp.path())
            .write_stdin(body)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(0),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let sent = std::fs::read(&captured).unwrap();
        let len = u32::from_be_bytes(sent[..4].try_into().unwrap()) as usize;
        let json: serde_json::Value = serde_json::from_slice(&sent[4..4 + len]).unwrap();
        assert_eq!(json, header, "{args:?}");
        assert_eq!(
            &sent[4 + len..],
            body,
            "{args:?}: the body follows the header"
        );
    }
}

/// A cwd that is not UTF-8 cannot travel in JSON. It is refused before ssh
/// starts, rather than sent altered.
#[cfg(unix)]
#[test]
fn host_start_with_a_non_utf8_cwd_is_refused_before_ssh() {
    use std::os::unix::ffi::OsStrExt;
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = poison_ssh();
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args(["--host", "box", "start", "--cwd"])
        .arg(std::ffi::OsStr::from_bytes(b"/tmp/not-utf8-\xff"))
        .args(["job", "--", "true"])
        .env("PATH", tmp.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(!stderr.contains("SSH_MUST_NOT_RUN"), "{stderr}");
    assert!(
        stderr.contains("--cwd") && stderr.contains("UTF-8"),
        "{stderr}"
    );
}

// -- Connect deadline: a --host call that never reaches the remote tendr --

/// Run `tendr --host box <args>` against `tmp`'s fake ssh with a connect
/// deadline of `deadline` (the `TENDR_SSH_CONNECT_TIMEOUT` value).
fn host_call(tmp: &TempDir, deadline: &str, args: &[&str]) -> std::process::Output {
    std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args(["--host", "box"])
        .args(args)
        .env("PATH", tmp.path())
        .env("TENDR_SSH_CONNECT_TIMEOUT", deadline)
        .output()
        .unwrap()
}

/// An ssh that never reaches the remote tendr (a Tailscale SSH check-mode
/// approval that lapsed stalls like this for about 30 minutes) is killed at
/// the deadline with exit 82 and a message naming the host, for ordinary
/// commands and for exec's frame transport alike.
#[test]
fn host_call_that_never_reaches_the_remote_tendr_stops_at_the_deadline() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // The last stall closes its stderr first: end of stream before a hello
    // proves nothing about authentication.
    for (args, close_stderr) in [
        (&["status", "job"][..], false),
        (&["exec", "shell", "--", "true"][..], false),
        (&["status", "job"][..], true),
    ] {
        let pid_file = TempDir::new().unwrap();
        let pid_file = pid_file.path().join("ssh.pid");
        let close = if close_stderr { "close-stderr\n" } else { "" };
        let tmp = fake_ssh(&format!("pidfile {}\n{close}sleep 30", pid_file.display()));
        let started = Instant::now();
        let child = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
            .args(["--host", "box"])
            .args(args)
            .env("PATH", tmp.path())
            .env("TENDR_SSH_CONNECT_TIMEOUT", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // The fake ssh writes its pidfile atomically before it stalls.
        let ssh = loop {
            if let Ok(pid) = std::fs::read_to_string(&pid_file) {
                break ProcessWatch::new(pid.trim().parse().unwrap());
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "{args:?}: the fake ssh never started"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let output = child.wait_with_output().unwrap();
        let took = started.elapsed();
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(output.status.code(), Some(82), "{args:?}: {stderr}");
        assert!(took < Duration::from_secs(10), "{args:?} took {took:?}");
        assert!(
            stderr.contains("box") && stderr.contains("did not reach"),
            "{args:?}: the error names the host: {stderr}"
        );
        assert!(!ssh.alive(), "{args:?}: the stalled ssh was killed");
    }
}

/// An ssh that exits without a hello while something it started lingers (a
/// ProxyCommand that ignores the hangup) does not keep the call open past the
/// deadline: the exit status is reported, and the caller's stderr reaches end
/// of file when tendr exits. tendr does not give ssh its stderr, so nothing
/// ssh starts may hold it; on Windows, where a child is handed every
/// inheritable handle and not just the stdio it is given, that takes tendr
/// keeping its own stdio from being inherited.
#[test]
fn host_call_that_exits_without_hello_is_not_held_open_by_a_leftover_process() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    for args in [&["status", "job"][..], &["exec", "shell", "--", "true"][..]] {
        // The leftover is given ssh's stderr but not its stdout, as a
        // ProxyCommand is (its stdout is the ssh connection, not ours).
        // The leftover runs from the built fake, not the linked `ssh` copy, so
        // it does not keep that copy (and its temp dir) in use on Windows.
        let tmp = fake_ssh(&format!(
            "leftover 30 {}\nerr ssh: connection closed\\n\nexit 255",
            assert_cmd::cargo::cargo_bin("fake_ssh").display()
        ));
        // tendr gives ssh its stdout, for remote output to stream straight
        // through, and the fake ssh, like an ssh without a handle allow-list,
        // lets the leftover inherit it: stdout goes to a file. Stderr is
        // piped, so `took` is when the last holder of the caller's stderr
        // lets go.
        let out_dir = TempDir::new().unwrap();
        let started = Instant::now();
        let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
            .args(["--host", "box"])
            .args(args)
            .env("PATH", tmp.path())
            .env("TENDR_SSH_CONNECT_TIMEOUT", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::fs::File::create(out_dir.path().join("stdout")).unwrap())
            .stderr(std::process::Stdio::piped())
            .output()
            .unwrap();
        let took = started.elapsed();
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(
            took < Duration::from_secs(10),
            "{args:?} took {took:?}: {stderr}"
        );
        assert_eq!(output.status.code(), Some(1), "{args:?}: {stderr}");
        assert!(
            stderr.contains("ssh: connection closed"),
            "{args:?}: {stderr}"
        );
        assert!(stderr.contains("exit 255"), "{args:?}: {stderr}");
    }
}

/// Once the remote tendr has said hello the deadline no longer applies: a
/// call that runs on long after it (`wait`, `log -f`) is left alone. The hello
/// itself is not shown; everything else on stderr is, in order, including a
/// fragment without a newline that a login shell's rc file wrote just before
/// tendr started, which the hello is glued onto.
#[test]
fn host_call_runs_past_the_deadline_once_the_remote_tendr_says_hello() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = fake_ssh(&format!(
        "err ssh: before hello\\n\n\
         err rc says hi \n\
         err {}\\n\n\
         sleep 3\n\
         out remote result\\n\n\
         err remote: after hello\\n\n\
         exit 0",
        tendr::ssh::HELLO_LINE
    ));
    let output = host_call(&tmp, "2", &["wait", "job"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "remote result\n");
    assert_eq!(
        stderr,
        "ssh: before hello\nrc says hi \nremote: after hello\n"
    );
}

/// `TENDR_SSH_CONNECT_TIMEOUT=0` turns the deadline off and with it the
/// `--hello` flag: the call waits as long as ssh takes.
#[test]
fn host_connect_deadline_of_zero_waits_without_hello() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = fake_ssh("reject-hello\nsleep 2\nout slow but fine\\n\nexit 0");
    for args in [&["list"][..], &["exec", "shell", "--", "true"][..]] {
        let output = host_call(&tmp, "0", args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(0), "{args:?}: {stderr}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "slow but fine\n");
    }
}

/// A deadline too large to represent is the default, not a panic.
#[test]
fn host_connect_deadline_that_cannot_be_represented_falls_back() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = fake_ssh(&format!(
        "err {}\\n\nout ok\\n\nexit 0",
        tendr::ssh::HELLO_LINE
    ));
    let output = host_call(&tmp, "1e30", &["list"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// What a call to a remote tendr too old for it is told to do.
const UPGRADE: &str = concat!(
    "tendr on box is too old for this client: upgrade it to ",
    env!("CARGO_PKG_VERSION"),
    " or later"
);

/// A remote tendr too old to know `--hello` or `--exit-trailer` fails fast
/// with clap's parse error; the caller is told to upgrade it, for exec too.
/// Under a PowerShell login shell ssh reports that as exit 1, not 2.
#[test]
fn host_call_to_a_tendr_without_hello_or_trailer_says_to_upgrade_it() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    for (flag, ssh_exit) in [("--hello", 2), ("--exit-trailer", 2), ("--exit-trailer", 1)] {
        let tmp = fake_ssh(&format!(
            "err error: unexpected argument '{flag}' found\\n\nexit {ssh_exit}"
        ));
        for args in [&["exec", "shell", "--", "true"][..], &["list"][..]] {
            for deadline in ["5", "0"] {
                let output = host_call(&tmp, deadline, args);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let what = format!("{flag} {ssh_exit} {args:?} {deadline}");

                assert_eq!(output.status.code(), Some(ssh_exit), "{what}: {stderr}");
                assert!(
                    stderr.contains(&format!("unexpected argument '{flag}'")),
                    "{what}: {stderr}"
                );
                assert!(stderr.contains(UPGRADE), "{what}: {stderr}");
            }
        }
    }
}

// -- The exit trailer: a remote exit code that survives a PowerShell shell --

/// A PowerShell login shell reports any failure as ssh exit 1. The remote
/// tendr's last stderr line, `tendr-exit <code>`, carries the real code:
/// the call exits with it and the line is not shown, with the deadline on or
/// off, for a framed operation and for exec.
#[test]
fn host_call_exits_with_the_trailer_code_and_hides_it() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    for deadline in ["5", "0"] {
        let hello = if deadline == "0" {
            String::new()
        } else {
            format!("err {}\\n\n", tendr::ssh::HELLO_LINE)
        };
        let tmp = fake_ssh(&format!(
            "{hello}err remote said\\n\nout result\\n\nerr tendr-exit 42\\n\nexit 1"
        ));
        for args in [&["wait", "job"][..], &["exec", "shell", "--", "true"][..]] {
            let output = host_call(&tmp, deadline, args);
            let stderr = String::from_utf8_lossy(&output.stderr);

            assert_eq!(
                output.status.code(),
                Some(42),
                "{deadline} {args:?}: {stderr}"
            );
            assert_eq!(stderr, "remote said\n", "{deadline} {args:?}");
            assert_eq!(String::from_utf8_lossy(&output.stdout), "result\n");
        }
    }
}

/// Only the last line counts: a `tendr-exit` line with more stderr after it
/// is the remote's own output, passed on, and ssh's status stands.
#[test]
fn host_call_passes_on_a_trailer_shaped_line_mid_stream() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = fake_ssh(&format!(
        "err {}\\n\nerr tendr-exit 3\\n\nerr later\\n\nexit 7",
        tendr::ssh::HELLO_LINE
    ));
    let output = host_call(&tmp, "5", &["status", "job"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(7), "{stderr}");
    assert_eq!(stderr, "tendr-exit 3\nlater\n");
}

/// A trailer of 255 is the remote's exit code, not ssh failing to connect;
/// ssh's 255 without a trailer still is.
#[test]
fn host_call_trailer_255_is_a_remote_exit() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = fake_ssh(&format!(
        "err {}\\n\nerr tendr-exit 255\\n\nexit 255",
        tendr::ssh::HELLO_LINE
    ));
    let output = host_call(&tmp, "5", &["status", "job"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(255), "{stderr}");
    assert!(!stderr.contains("transport"), "{stderr}");

    let tmp = fake_ssh(&format!("err {}\\n\nexit 255", tendr::ssh::HELLO_LINE));
    let output = host_call(&tmp, "5", &["status", "job"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("transport"), "{stderr}");
}

/// What a tendr from before the frame transport prints for `_remote`.
const NO_REMOTE_ENTRY: &str = "error: unrecognized subcommand '_remote'\\n\\n  \
     tip: a similar subcommand exists: 'emit'\\n\\nUsage: tendr [OPTIONS] <COMMAND>\\n\\n\
     For more information, try '--help'.\\n";

/// A remote tendr that knows `--hello` but not the frame entry point: clap's
/// error is passed on, and the caller is told to upgrade it, with the
/// deadline on or off.
#[test]
fn host_call_to_a_tendr_without_the_frame_entry_says_to_upgrade_it() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = fake_ssh(&format!("err {NO_REMOTE_ENTRY}\nexit 2"));
    for deadline in ["5", "0"] {
        let output = host_call(&tmp, deadline, &["status", "job"]);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(output.status.code(), Some(2), "{deadline}: {stderr}");
        assert!(
            stderr.contains("unrecognized subcommand '_remote'"),
            "{deadline}: {stderr}"
        );
        assert!(
            stderr.contains(concat!(
                "tendr on box is too old for this client: upgrade it to ",
                env!("CARGO_PKG_VERSION"),
                " or later"
            )),
            "{deadline}: {stderr}"
        );
    }
}

/// A current remote's own exit-2 refusal that quotes a user value such as
/// `'--hello'` or `'_remote'` is passed on as it is, not mistaken for an old
/// tendr, with the deadline on or off.
#[test]
fn host_refusal_quoting_hello_is_not_upgrade_advice() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = fake_ssh(
        "err tendr _remote: invalid remote request: session name '--hello' and \
         unrecognized subcommand '_remote'\\n\nexit 2",
    );
    for deadline in ["0", "5"] {
        let output = host_call(&tmp, deadline, &["status", "job"]);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(output.status.code(), Some(2), "{deadline}: {stderr}");
        assert!(stderr.starts_with("tendr _remote:"), "{deadline}: {stderr}");
        assert!(!stderr.contains("upgrade"), "{deadline}: {stderr}");
    }
}

/// What the remote end does: `tendr --hello …` writes the hello as its very
/// first stderr line, before the command's own output.
#[test]
fn hello_flag_writes_the_hello_line_first() {
    let root = TempDir::new().unwrap();
    let output = harness::tendr(&root)
        .args(["--hello", "status", "missing"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr.lines().next(),
        Some(tendr::ssh::HELLO_LINE),
        "{stderr}"
    );
}

// -- Task 4: Transport error classification --

#[test]
fn host_flag_exit_255_is_transport_error() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = fake_ssh("exit 255");

    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args(["--host", "user@box", "list"])
        .env("PATH", tmp.path())
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(1),
        "transport error should exit 1"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("transport"),
        "stderr should mention transport failure, got: {stderr}"
    );
}

#[test]
fn host_flag_preserves_remote_exit_code() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = fake_ssh("exit 42");

    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args(["--host", "user@box", "wait", "my-session"])
        .env("PATH", tmp.path())
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(42),
        "should preserve remote exit code 42"
    );
}

// -- Task 5: JSON output passthrough --

#[test]
fn host_flag_passes_through_json_stdout() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = fake_ssh(
        "out {\"schema_version\":1,\"session\":\"remote-job\",\"status\":\"Running\"}\\n\nexit 0",
    );

    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args(["--host", "user@box", "status", "remote-job"])
        .env("PATH", tmp.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|_| panic!("stdout should be valid JSON, got: {stdout}"));
    assert_eq!(parsed["session"], "remote-job");
    assert_eq!(parsed["status"], "Running");
    assert!(output.status.success());
}

// -- Task 6: NDJSON streaming passthrough --

#[test]
fn host_flag_passes_through_ndjson_stream() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = fake_ssh(
        r#"out {"ts":1.0,"namespace":"default","session":"s1","run_id":"abc","source":"tendr.sidecar","kind":"run","name":"run.started","data":{"status":"Running"}}\n
out {"ts":2.0,"namespace":"default","session":"s1","run_id":"abc","source":"tendr.sidecar","kind":"run","name":"run.exited","data":{"status":"Exited","reason":"ExitedOk","exit_code":0}}\n
exit 0"#,
    );

    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args(["--host", "user@box", "watch", "--events"])
        .env("PATH", tmp.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "should have 2 NDJSON lines, got: {stdout}");

    for line in &lines {
        let event: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|_| panic!("each line should be valid JSON: {line}"));
        assert_eq!(event["source"], "tendr.sidecar");
    }

    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(first["name"], "run.started");
    let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(second["name"], "run.exited");
}

// -- Task 7: SSH spawn failure --

#[test]
fn host_flag_ssh_not_found_gives_clear_error() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = tempfile::TempDir::new().unwrap();
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args(["--host", "user@box", "list"])
        .env("PATH", tmp.path())
        .output()
        .unwrap();

    assert!(!output.status.success(), "should fail when ssh not found");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ssh") || stderr.contains("spawn"),
        "error should mention ssh, got: {stderr}"
    );
}

// -- Task 9: Stderr passthrough --

#[test]
fn host_flag_passes_through_stderr() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = fake_ssh("err session not found: oops\\n\nexit 1");

    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args(["--host", "user@box", "status", "oops"])
        .env("PATH", tmp.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("session not found"),
        "remote stderr should pass through, got: {stderr}"
    );
    assert_eq!(output.status.code(), Some(1));
}

// -- attach from a Windows client: still `ssh -t` with a rebuilt argv, so
//    plain names only --

#[cfg(windows)]
#[test]
fn host_flag_attach_uses_tty_allocation() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = fake_ssh_echo();

    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args([
            "--host",
            "user@box",
            "attach",
            "--namespace",
            "ns_1",
            "--takeover",
            "--escape",
            "none",
            "My-session_2",
        ])
        .env("PATH", tmp.path())
        .output()
        .unwrap();

    assert_eq!(
        printed_args(&output.stdout),
        [
            "-t",
            "-o",
            "ConnectTimeout=10",
            "user@box",
            "tendr",
            "attach",
            "My-session_2",
            "--namespace",
            "ns_1",
            "--takeover",
            "--escape",
            "none"
        ]
    );
}

/// A Windows client's attach still rebuilds its remote command, so it takes
/// only names that no shell acts on, and refuses the rest before ssh starts.
#[cfg(windows)]
#[test]
fn host_attach_takes_only_plain_names() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = poison_ssh();
    let long = "a".repeat(256);
    for args in [
        &["attach", "x&calc"][..],
        &["attach", "x;id"],
        &["attach", "x$(id)"],
        &["attach", "it's"],
        &["attach", "caf\u{e9}"],
        &["attach", "_lead"],
        &["attach", "0x1F"],
        &["attach", "1kb"],
        &["attach", "1e3"],
        &["attach", "--namespace", "9", "s"],
        &["attach", &long],
        &["attach", "--namespace", "n&s", "s"],
        &["attach", "--namespace", "n|s", "s"],
    ] {
        let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
            .args(["--host", "box"])
            .args(args)
            .env("PATH", tmp.path())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(output.status.code(), Some(2), "{args:?}: {stderr}");
        assert!(!stderr.contains("SSH_MUST_NOT_RUN"), "{args:?}: {stderr}");
        assert!(
            stderr.contains("attach over --host from Windows takes plain names"),
            "{args:?}: {stderr}"
        );
    }
}

// -- Local-only verbs exit 2 with a pre-filled ssh fallback, before any
//    connection --

/// A poison ssh shim: running it is the failure the slice exists to
/// prevent, so it exits loudly. Returns the PATH dir.
fn poison_ssh() -> TempDir {
    fake_ssh("err SSH_MUST_NOT_RUN\\n\nexit 99")
}

/// What introduces the copy-paste fallback: POSIX-quoted text, labelled so.
const FALLBACK_LABEL: &str = "try (POSIX shell):";

/// The fallback after `try (POSIX shell):`, re-split twice — once as the
/// local shell would (yielding `ssh <host> <remote-string>`), once as the
/// remote login shell would — returning the remote tendr argv. This pins
/// that the printed fallback is copy-paste correct, not just plausible.
fn parse_fallback_argv(stderr: &str, host: &str) -> Vec<String> {
    let try_line = stderr
        .lines()
        .find(|l| l.trim_start().starts_with(FALLBACK_LABEL))
        .unwrap_or_else(|| panic!("no fallback line in stderr: {stderr}"));
    let cmd = try_line
        .trim_start()
        .strip_prefix(FALLBACK_LABEL)
        .unwrap()
        .trim();
    let local = shell_words::split(cmd).expect("the fallback parses as shell words");
    assert_eq!(local[0], "ssh", "fallback is an ssh command: {cmd}");
    assert_eq!(local[1], host, "fallback targets the requested host");
    assert_eq!(
        local.len(),
        3,
        "ssh + host + one remote command string: {local:?}"
    );
    shell_words::split(&local[2]).expect("remote command string parses")
}

/// run/wrap/prune stay local-only permanently: exit 2, verb named,
/// local-only stated, fallback line present.
#[test]
fn host_run_wrap_prune_exit_2_and_say_local_only() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = poison_ssh();
    for (verb, cmd_args) in [
        (
            "run",
            vec!["--host", "user@box", "run", "deploy.sh", "--stdin"],
        ),
        (
            "wrap",
            vec![
                "--host",
                "user@box",
                "wrap",
                "--source",
                "test.hook",
                "--event",
                "pre-tool-use",
                "--",
                "true",
            ],
        ),
        (
            "prune",
            vec!["--host", "user@box", "prune", "--all", "--dry-run"],
        ),
    ] {
        let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
            .args(&cmd_args)
            .env("PATH", tmp.path())
            .output()
            .unwrap();

        assert_eq!(
            output.status.code(),
            Some(2),
            "{verb} --host is a usage error"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("'{verb}' is local-only")),
            "{verb} names itself and says local-only: {stderr}"
        );
        let remote = parse_fallback_argv(&stderr, "user@box");
        assert_eq!(remote[0], "tendr");
        assert_eq!(remote[1], verb, "fallback reconstructs the verb");
    }
}

/// run's fallback keeps the `--` separator before script args: a script
/// arg like --stdin must not re-parse as tendr's own flag when the
/// fallback is pasted (review finding on PR #12). clap consumes the
/// first `--` but keeps any later one in the captured args, so exactly
/// one separator is re-inserted.
#[test]
fn host_run_fallback_keeps_separator_before_script_args() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = poison_ssh();
    for (cli_args, expected_remote) in [
        (
            vec!["--host", "user@box", "run", "deploy.sh", "--", "--stdin"],
            vec!["tendr", "run", "deploy.sh", "--", "--stdin"],
        ),
        (
            // A second `--` inside script args survives in place.
            vec!["--host", "user@box", "run", "d.sh", "--", "a", "--", "b"],
            vec!["tendr", "run", "d.sh", "--", "a", "--", "b"],
        ),
    ] {
        let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
            .args(&cli_args)
            .env("PATH", tmp.path())
            .output()
            .unwrap();

        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8_lossy(&output.stderr);
        let remote = parse_fallback_argv(&stderr, "user@box");
        assert_eq!(
            remote, expected_remote,
            "script args re-parse as script args, not tendr flags"
        );
    }
}

/// The rejection happens before any connection or side effect: a PATH'd
/// ssh shim that records invocation is never run.
#[test]
fn host_local_only_rejection_spawns_no_ssh() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let marker = TempDir::new().unwrap();
    let marker = marker.path().join("ssh-was-run");
    let tmp = fake_ssh(&format!("touch {}\nexit 0", marker.display()));

    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args(["--host", "user@box", "prune", "--all"])
        .env("PATH", tmp.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(!marker.exists(), "ssh must not be spawned on rejection");
}

/// query is local-only in v1: it reads the local session tree and shells to a
/// local duckdb, so --host is a usage error (exit 2) with a copy-paste ssh
/// fallback — the same treatment as run/wrap/prune, not the generic rejection
/// (which would print a false "local-only: run, wrap, prune" list omitting it).
#[test]
fn host_query_is_local_only_with_fallback() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = poison_ssh();
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args(["--host", "user@box", "query", "SELECT 1"])
        .env("PATH", tmp.path())
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(2),
        "query --host is a usage error"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("'query' is local-only"),
        "query names itself and says local-only: {stderr}"
    );
    let remote = parse_fallback_argv(&stderr, "user@box");
    assert_eq!(remote[0], "tendr");
    assert_eq!(remote[1], "query", "fallback reconstructs the verb");
    assert!(
        remote.contains(&"SELECT 1".to_string()),
        "fallback carries the SQL: {remote:?}"
    );
}

/// `guide` and `skill` print/write local files, so `--host` is meaningless:
/// exit 2 with a copy-paste ssh fallback, the same treatment as run/wrap/prune/
/// query rather than the generic rejection. The skill case also proves the
/// subcommand + its flags round-trip into the fallback argv.
#[test]
fn host_guide_and_skill_are_local_only_with_fallback() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = poison_ssh();
    for (verb, cmd_args, expected_tail) in [
        (
            "guide",
            vec!["--host", "user@box", "guide", "exec"],
            vec!["guide", "exec"],
        ),
        (
            "skill",
            vec![
                "--host", "user@box", "skill", "install", "--global", "--force",
            ],
            vec!["skill", "install", "--global", "--force"],
        ),
    ] {
        let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
            .args(&cmd_args)
            .env("PATH", tmp.path())
            .output()
            .unwrap();

        assert_eq!(
            output.status.code(),
            Some(2),
            "{verb} --host is a usage error"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("'{verb}' is local-only")),
            "{verb} names itself and says local-only: {stderr}"
        );
        let remote = parse_fallback_argv(&stderr, "user@box");
        assert_eq!(remote[0], "tendr");
        assert_eq!(
            &remote[1..],
            expected_tail.as_slice(),
            "fallback round-trips"
        );
    }
}

/// Commands outside the plan's four verbs (`events`, `emit`) keep the
/// generic unsupported-over-SSH rejection untouched — spec §8 defers
/// their local-only help text to slice 5.
#[test]
fn host_events_keeps_generic_rejection() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = poison_ssh();
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args(["--host", "user@box", "events"])
        .env("PATH", tmp.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1), "generic path unchanged");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not supported over SSH"),
        "generic message unchanged: {stderr}"
    );
}

// -- Remote exec via frame-from-stdin --

/// A fake ssh that acts as the remote login shell: it drops the ssh options
/// and destination, re-splits the remote command as a POSIX shell would, and
/// runs the real tendr binary (`TENDR_TEST_BIN`) with it — the frame arrives
/// on real stdin, the session root rides in on HOME. This exercises the full
/// frame path end-to-end on one machine.
fn fake_ssh_frame_shim() -> TempDir {
    fake_ssh("forward")
}

// The exec sessions behind the fake ssh are each platform's native shell, as
// in `cli_exec.rs`: bash on Unix, PowerShell on Windows.

/// Start the native-shell session "shell" and wait until it runs.
fn start_native_shell(root: &TempDir) -> harness::SessionGuard<'_> {
    let shell: &[&str] = if cfg!(windows) {
        &[
            "--exec-target",
            "powershell",
            "--",
            "powershell",
            "-NoProfile",
        ]
    } else {
        &["--", "bash"]
    };
    harness::tendr(root)
        .args(["start", "shell", "--stdin"])
        .args(shell)
        .assert()
        .success();
    let guard = harness::SessionGuard::new(root, "shell");
    harness::wait_running(root, "shell");
    guard
}

/// A command that prints `text` verbatim and a newline. On Unix the newline
/// comes from printf's format, not argv: a newline-less payload trips the
/// sentinel merge quirk and hangs exec.
fn native_echo(text: &str) -> Vec<String> {
    if cfg!(windows) {
        // PowerShell takes a single-quoted literal; a quote inside doubles.
        vec![format!("Write-Output '{}'", text.replace('\'', "''"))]
    } else {
        vec!["printf".into(), "%s\\n".into(), text.into()]
    }
}

/// A command that exits with `code` without ending the session.
fn native_exit(code: u8) -> Vec<String> {
    if cfg!(windows) {
        vec![format!("cmd /c exit {code}")]
    } else {
        vec!["sh".into(), "-c".into(), format!("exit {code}")]
    }
}

/// A command that sleeps for `secs` seconds.
fn native_sleep(secs: u64) -> Vec<String> {
    if cfg!(windows) {
        vec![format!("Start-Sleep {secs}")]
    } else {
        vec!["sleep".into(), secs.to_string()]
    }
}

/// `tendr --host h exec …` works end-to-end: the envelope comes back on
/// stdout and the exec ran against the (shim-local) remote session.
#[test]
fn host_exec_frame_end_to_end() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let _session = start_native_shell(&root);

    let tmp = fake_ssh_frame_shim();
    let output = harness::tendr(&root)
        .args(["--host", "box", "exec", "shell", "--"])
        .args(native_echo("remote hi"))
        .env("PATH", shim_path_env(&tmp))
        .env(
            "TENDR_TEST_BIN",
            assert_cmd::cargo::cargo_bin("tendr").to_str().unwrap(),
        )
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "remote exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["exit_code"], 0);
    assert!(envelope["stdout"].as_str().unwrap().contains("remote hi"));
}

/// Non-zero inner exit codes propagate through ssh natively.
#[test]
fn host_exec_inner_exit_code_propagates() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let _session = start_native_shell(&root);

    let tmp = fake_ssh_frame_shim();
    let output = harness::tendr(&root)
        .args(["--host", "box", "exec", "shell", "--"])
        .args(native_exit(7))
        .env("PATH", shim_path_env(&tmp))
        .env(
            "TENDR_TEST_BIN",
            assert_cmd::cargo::cargo_bin("tendr").to_str().unwrap(),
        )
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(7), "inner exit code survives");
}

/// Timeout propagates: remote exits 124, envelope says timed_out.
#[test]
fn host_exec_timeout_propagates() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let _session = start_native_shell(&root);

    let tmp = fake_ssh_frame_shim();
    let output = harness::tendr(&root)
        .args(["--host", "box", "exec", "shell", "--timeout", "1", "--"])
        .args(native_sleep(3))
        .env("PATH", shim_path_env(&tmp))
        .env(
            "TENDR_TEST_BIN",
            assert_cmd::cargo::cargo_bin("tendr").to_str().unwrap(),
        )
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(124));
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["timed_out"], true);
}

/// The acceptance fixture: a payload full of quoting hazards produces
/// byte-identical results run locally and run through the remote frame
/// path — zero shell-quoting traversal, both directions.
#[test]
fn host_exec_torture_payload_matches_local() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let _session = start_native_shell(&root);

    let torture = r#"it's a "test" $VAR `tick` back\slash ;&|"#;
    let argv = native_echo(torture);

    let local = harness::tendr(&root)
        .args(["exec", "shell", "--timeout", "30", "--"])
        .args(&argv)
        .output()
        .unwrap();
    assert!(local.status.success());

    let tmp = fake_ssh_frame_shim();
    let remote = harness::tendr(&root)
        .args(["--host", "box", "exec", "shell", "--timeout", "30", "--"])
        .args(&argv)
        .env("PATH", shim_path_env(&tmp))
        .env(
            "TENDR_TEST_BIN",
            assert_cmd::cargo::cargo_bin("tendr").to_str().unwrap(),
        )
        .output()
        .unwrap();
    assert!(
        remote.status.success(),
        "remote: {}",
        String::from_utf8_lossy(&remote.stderr)
    );

    let local_env: serde_json::Value = serde_json::from_slice(&local.stdout).unwrap();
    let remote_env: serde_json::Value = serde_json::from_slice(&remote.stdout).unwrap();
    assert_eq!(
        local_env["stdout"], remote_env["stdout"],
        "local and remote exec agree byte-exact"
    );
    assert_eq!(
        local_env["stdout"]
            .as_str()
            .unwrap()
            .trim_end_matches(['\r', '\n']),
        torture
    );
}

/// The remote argv contains nothing user-controlled: the payload rides
/// only on stdin. (The echo shim also exits without reading stdin, so
/// this pins that a dead remote never panics the local writer.)
#[test]
fn host_exec_remote_argv_carries_no_payload() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let tmp = fake_ssh_echo();
    let payload = "SELECT count(*) FROM t;";
    let output = harness::tendr(&root)
        .args(["--host", "box", "exec", "ddb", "--", payload])
        .env("PATH", shim_path_env(&tmp))
        .output()
        .unwrap();

    assert_eq!(
        printed_args(&output.stdout),
        [
            "-T",
            "-o",
            "ConnectTimeout=10",
            "box",
            "tendr",
            "--hello",
            "--exit-trailer",
            "exec",
            "--frame-from-stdin"
        ],
        "remote argv is constant — nothing user-controlled"
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains(payload),
        "payload must not appear in argv"
    );
}

/// --host composes with --frame-from-stdin: the local frame passes
/// through to the remote verbatim (scripting-friendly).
#[test]
fn host_exec_frame_stdin_passes_through() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let _session = start_native_shell(&root);

    let tmp = fake_ssh_frame_shim();
    let frame = serde_json::json!({"v": 1, "session": "shell", "cmd": native_echo("piped frame")})
        .to_string();
    let output = harness::tendr(&root)
        .args(["--host", "box", "exec", "--frame-from-stdin"])
        .env("PATH", shim_path_env(&tmp))
        .env(
            "TENDR_TEST_BIN",
            assert_cmd::cargo::cargo_bin("tendr").to_str().unwrap(),
        )
        .write_stdin(frame)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "passthrough failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(envelope["stdout"].as_str().unwrap().contains("piped frame"));
}

/// The frame reaches ssh's stdin byte for byte: nothing is added, dropped or
/// re-encoded on the way.
#[test]
fn host_exec_frame_stdin_reaches_ssh_unchanged() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let captured = TempDir::new().unwrap();
    let captured = captured.path().join("stdin");
    let tmp = fake_ssh(&format!("stdin {}\nexit 0", captured.display()));
    let frame = "{\"v\":1,\"session\":\"shell\",\"cmd\":[\"echo\",\"it's \\\"q\\\" \u{e9}\"]}\n";
    let output = harness::tendr(&root)
        .args(["--host", "box", "exec", "--frame-from-stdin"])
        .env("PATH", shim_path_env(&tmp))
        .write_stdin(frame)
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(&captured).unwrap(), frame.as_bytes());
}

#[test]
fn host_run_fallback_keeps_unset_env() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = poison_ssh();
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args([
            "--host",
            "user@box",
            "run",
            "--unset-env",
            "CI",
            "deploy.sh",
        ])
        .env("PATH", tmp.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    let remote = parse_fallback_argv(&stderr, "user@box");
    assert_eq!(
        remote,
        ["tendr", "run", "--unset-env", "CI", "deploy.sh"],
        "fallback passes --unset-env on"
    );
}

// --- host destination hardening (v0.2.1) ---

#[test]
fn host_rejects_option_shaped_destination_general() {
    // A `-`-prefixed --host destination must be rejected at the CLI boundary
    // (exit 2) BEFORE ssh is spawned, so the fake ssh (which prints ARG: lines)
    // is never invoked. Guards the frame path.
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = fake_ssh_echo();
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args(["--host", "-oProxyCommand=marker", "status", "job"])
        .env("PATH", tmp.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "must exit 2");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("ARG:"),
        "fake ssh must NOT be spawned for an option-shaped --host: {stdout}"
    );
}

#[test]
fn host_rejects_option_shaped_destination_framed_exec() {
    // The framed exec path has a constant remote argv, but the destination still
    // precedes it — so the same guard applies, rejected before any ssh spawn.
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = fake_ssh_echo();
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args([
            "--host",
            "-oProxyCommand=marker",
            "exec",
            "job",
            "--",
            "echo",
            "hi",
        ])
        .env("PATH", tmp.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "must exit 2");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("ARG:"),
        "fake ssh must NOT be spawned for an option-shaped --host: {stdout}"
    );
}
