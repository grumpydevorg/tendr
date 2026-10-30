// Remote SSH transport tests — POSIX only.
// All tests rely on fake shell scripts as ssh shims.
#![cfg(unix)]

mod harness;

use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

/// Helper: create a fake ssh script that dumps args to stdout.
/// Returns the TempDir (must be kept alive for PATH to stay valid).
fn fake_ssh_echo() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let fake_ssh = tmp.path().join("ssh");
    std::fs::write(
        &fake_ssh,
        "#!/bin/sh\nfor arg in \"$@\"; do echo \"ARG:$arg\"; done\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake_ssh, PermissionsExt::from_mode(0o755)).unwrap();
    tmp
}

/// Helper: create a fake ssh that exits 0 immediately (no-op).
fn fake_ssh_noop() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let fake_ssh = tmp.path().join("ssh");
    std::fs::write(&fake_ssh, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&fake_ssh, PermissionsExt::from_mode(0o755)).unwrap();
    tmp
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

#[test]
fn host_flag_invokes_ssh_with_correct_remote_command() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = fake_ssh_echo();

    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args(["--host", "user@box", "list"])
        .env("PATH", tmp.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let args: Vec<&str> = stdout
        .lines()
        .filter_map(|l| l.strip_prefix("ARG:"))
        .collect();
    // Should see: -T, -o, ConnectTimeout=10, user@box, tendr, --hello, list
    let remote: Vec<&str> = args
        .iter()
        .skip_while(|a| **a != "tendr")
        .copied()
        .collect();
    assert_eq!(
        remote,
        ["tendr", "--hello", "list"],
        "the remote tendr is asked to announce itself first: {args:?}"
    );
    // No "--" should be passed to ssh
    assert!(
        !args.contains(&"--"),
        "should not contain -- in ssh args: {args:?}"
    );
}

/// Helper: extract the remote command from the fake ssh output.
///
/// `fake_ssh_echo()` prints each arg as `ARG:<value>`. The remote
/// command args are everything after the host (arg index 4+, since
/// the first 4 are: -T, -o, ConnectTimeout=10, <host>).
///
/// SSH concatenates these with spaces to form the remote command string.
/// We do the same and then `shell_words::split()` to simulate what the
/// remote POSIX shell would produce as argv.
fn parse_remote_argv(stdout: &str) -> Vec<String> {
    let args: Vec<&str> = stdout
        .lines()
        .filter_map(|l| l.strip_prefix("ARG:"))
        .collect();
    // Skip: -T, -o, ConnectTimeout=10, <host>
    let remote_parts = &args[4..];
    let remote_cmd = remote_parts.join(" ");
    shell_words::split(&remote_cmd).expect("remote command should be valid POSIX shell syntax")
}

// -- Connect deadline: a --host call that never reaches the remote tendr --

/// A fake ssh running `script` (a POSIX `sh` body), on a PATH holding only it.
fn fake_ssh(script: &str) -> TempDir {
    let tmp = TempDir::new().unwrap();
    let fake_ssh = tmp.path().join("ssh");
    std::fs::write(&fake_ssh, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&fake_ssh, PermissionsExt::from_mode(0o755)).unwrap();
    tmp
}

/// Run `tendr --host box <args>` against `tmp`'s fake ssh with a connect
/// deadline of `deadline` (the `TENDR_SSH_CONNECT_TIMEOUT` value).
fn host_call(tmp: &TempDir, deadline: &str, args: &[&str]) -> (std::process::Output, Duration) {
    let started = Instant::now();
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args(["--host", "box"])
        .args(args)
        .env("PATH", tmp.path())
        .env("TENDR_SSH_CONNECT_TIMEOUT", deadline)
        .env("PIDF", tmp.path().join("ssh.pid"))
        .output()
        .unwrap();
    (output, started.elapsed())
}

/// An ssh that never reaches the remote tendr (a Tailscale SSH check-mode
/// approval that lapsed stalls like this for about 30 minutes) is killed at
/// the deadline with exit 82 and a message naming the host, for ordinary
/// commands and for exec's frame transport alike.
#[test]
fn host_call_that_never_reaches_the_remote_tendr_stops_at_the_deadline() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    for args in [&["status", "job"][..], &["exec", "shell", "--", "true"][..]] {
        let tmp = fake_ssh("echo $$ > \"$PIDF\"\nexec /bin/sleep 30");
        let (output, took) = host_call(&tmp, "1", args);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(output.status.code(), Some(82), "{args:?}: {stderr}");
        assert!(took < Duration::from_secs(10), "{args:?} took {took:?}");
        assert!(
            stderr.contains("box") && stderr.contains("did not reach"),
            "{args:?}: the error names the host: {stderr}"
        );
        let pid: i32 = std::fs::read_to_string(tmp.path().join("ssh.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: signal 0 only probes whether the fake ssh still exists.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "{args:?}: the stalled ssh was killed");
    }
}

/// Once the remote tendr has said hello the deadline no longer applies: a
/// call that runs on long after it (`wait`, `log -f`) is left alone. The hello
/// itself is not shown; everything else on stderr is, in order.
#[test]
fn host_call_runs_past_the_deadline_once_the_remote_tendr_says_hello() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = fake_ssh(&format!(
        "echo 'ssh: before hello' >&2\n\
         echo '{}' >&2\n\
         /bin/sleep 2\n\
         echo 'remote result'\n\
         echo 'remote: after hello' >&2\n\
         exit 0",
        tendr::ssh::HELLO_LINE
    ));
    let (output, _) = host_call(&tmp, "1", &["wait", "job"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "remote result\n");
    assert_eq!(stderr, "ssh: before hello\nremote: after hello\n");
}

/// `TENDR_SSH_CONNECT_TIMEOUT=0` turns the deadline off.
#[test]
fn host_connect_deadline_of_zero_waits_as_long_as_ssh_takes() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = fake_ssh("/bin/sleep 2\necho 'slow but fine'\nexit 0");
    let (output, _) = host_call(&tmp, "0", &["list"]);

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "slow but fine\n");
}

/// A remote tendr too old to know `--hello` fails fast with clap's parse
/// error (exit 2); the caller is told why and what to do.
#[test]
fn host_call_to_a_tendr_without_hello_says_to_upgrade_it() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = fake_ssh("echo \"error: unexpected argument '--hello' found\" >&2\nexit 2");
    let (output, _) = host_call(&tmp, "5", &["list"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("unexpected argument '--hello'"), "{stderr}");
    assert!(stderr.contains("upgrade tendr on box"), "{stderr}");
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

    let tmp = tempfile::TempDir::new().unwrap();
    let fake_ssh = tmp.path().join("ssh");
    std::fs::write(&fake_ssh, "#!/bin/sh\nexit 255\n").unwrap();
    std::fs::set_permissions(&fake_ssh, PermissionsExt::from_mode(0o755)).unwrap();

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

    let tmp = tempfile::TempDir::new().unwrap();
    let fake_ssh = tmp.path().join("ssh");
    std::fs::write(&fake_ssh, "#!/bin/sh\nexit 42\n").unwrap();
    std::fs::set_permissions(&fake_ssh, PermissionsExt::from_mode(0o755)).unwrap();

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

    let tmp = tempfile::TempDir::new().unwrap();
    let fake_ssh = tmp.path().join("ssh");
    let script = "#!/bin/sh\nprintf '{\"schema_version\":1,\"session\":\"remote-job\",\"status\":\"Running\"}\\n'\nexit 0\n";
    std::fs::write(&fake_ssh, script).unwrap();
    std::fs::set_permissions(&fake_ssh, PermissionsExt::from_mode(0o755)).unwrap();

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

    let tmp = tempfile::TempDir::new().unwrap();
    let fake_ssh = tmp.path().join("ssh");
    let script = r#"#!/bin/sh
echo '{"ts":1.0,"namespace":"default","session":"s1","run_id":"abc","source":"tendr.sidecar","kind":"run","name":"run.started","data":{"status":"Running"}}'
echo '{"ts":2.0,"namespace":"default","session":"s1","run_id":"abc","source":"tendr.sidecar","kind":"run","name":"run.exited","data":{"status":"Exited","reason":"ExitedOk","exit_code":0}}'
exit 0
"#;
    std::fs::write(&fake_ssh, script).unwrap();
    std::fs::set_permissions(&fake_ssh, PermissionsExt::from_mode(0o755)).unwrap();

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

// -- Task 8: Namespace forwarding --

#[test]
fn host_flag_forwards_namespace_and_strips_host() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = fake_ssh_echo();

    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args([
            "--host",
            "user@box",
            "status",
            "my-session",
            "--namespace",
            "prod",
        ])
        .env("PATH", tmp.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed = parse_remote_argv(&stdout);
    assert!(
        parsed.contains(&"--namespace".to_string()),
        "should forward --namespace: {parsed:?}"
    );
    assert!(
        parsed.contains(&"prod".to_string()),
        "should forward prod: {parsed:?}"
    );
    assert!(
        !parsed.contains(&"--host".to_string()),
        "--host should NOT be in remote command: {parsed:?}"
    );
}

// -- Task 9: Stderr passthrough --

#[test]
fn host_flag_passes_through_stderr() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = tempfile::TempDir::new().unwrap();
    let fake_ssh = tmp.path().join("ssh");
    let script = "#!/bin/sh\necho 'session not found: oops' >&2\nexit 1\n";
    std::fs::write(&fake_ssh, script).unwrap();
    std::fs::set_permissions(&fake_ssh, PermissionsExt::from_mode(0o755)).unwrap();

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

// -- Task 10: Trailing args, quoting, child --host --

#[test]
fn host_flag_forwards_start_with_trailing_args() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = fake_ssh_echo();

    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args([
            "--host",
            "user@box",
            "start",
            "job",
            "--timeout",
            "30",
            "--",
            "sleep",
            "60",
        ])
        .env("PATH", tmp.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed = parse_remote_argv(&stdout);

    assert_eq!(parsed[0], "tendr");
    assert!(parsed.contains(&"start".to_string()), "parsed: {parsed:?}");
    assert!(parsed.contains(&"job".to_string()), "parsed: {parsed:?}");
    assert!(
        parsed.contains(&"--timeout".to_string()),
        "parsed: {parsed:?}"
    );
    assert!(parsed.contains(&"30".to_string()), "parsed: {parsed:?}");
    assert!(parsed.contains(&"sleep".to_string()), "parsed: {parsed:?}");
    assert!(parsed.contains(&"60".to_string()), "parsed: {parsed:?}");
    assert!(
        !parsed.contains(&"--host".to_string()),
        "parsed: {parsed:?}"
    );
}

#[test]
fn host_flag_quotes_child_args_with_spaces() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = fake_ssh_echo();

    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args([
            "--host",
            "user@box",
            "start",
            "job",
            "--",
            "echo",
            "hello world",
            "foo;bar",
            "$HOME",
        ])
        .env("PATH", tmp.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed = parse_remote_argv(&stdout);

    assert_eq!(parsed[0], "tendr");
    assert!(
        parsed.contains(&"hello world".to_string()),
        "space-containing arg must survive round-trip: {parsed:?}"
    );
    assert!(
        parsed.contains(&"foo;bar".to_string()),
        "semicolon-containing arg must survive: {parsed:?}"
    );
    assert!(
        parsed.contains(&"$HOME".to_string()),
        "dollar sign must survive (not expanded): {parsed:?}"
    );
}

#[test]
fn host_flag_does_not_eat_child_host_arg() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = fake_ssh_echo();

    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args([
            "--host",
            "user@box",
            "start",
            "job",
            "--",
            "myprog",
            "--host",
            "other-host",
        ])
        .env("PATH", tmp.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed = parse_remote_argv(&stdout);

    let host_indices: Vec<usize> = parsed
        .iter()
        .enumerate()
        .filter(|(_, a)| a.as_str() == "--host")
        .map(|(i, _)| i)
        .collect();
    assert!(
        !host_indices.is_empty(),
        "child's --host must be preserved in remote command: {parsed:?}"
    );
    for &i in &host_indices {
        if i + 1 < parsed.len() && parsed[i + 1] == "other-host" {
            return;
        }
    }
    panic!("child's --host other-host must be preserved: {parsed:?}");
}

// -- Task 9 (PTY): SSH -t for attach --

#[test]
fn host_flag_attach_uses_tty_allocation() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = fake_ssh_echo();

    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args(["--host", "user@box", "attach", "my-session"])
        .env("PATH", tmp.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let args: Vec<&str> = stdout
        .lines()
        .filter_map(|l| l.strip_prefix("ARG:"))
        .collect();
    assert!(
        args.contains(&"-t"),
        "attach should use -t for TTY: {args:?}"
    );
    assert!(!args.contains(&"-T"), "attach should not use -T: {args:?}");
}

// -- Task 6 (exec-target): --exec-target forwarding --

#[test]
fn host_flag_forwards_exec_target() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = fake_ssh_echo();

    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args([
            "--host",
            "user@box",
            "start",
            "shell",
            "--stdin",
            "--exec-target",
            "posix-shell",
            "--",
            "bash",
        ])
        .env("PATH", tmp.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed = parse_remote_argv(&stdout);

    assert_eq!(parsed[0], "tendr");
    assert!(parsed.contains(&"start".to_string()), "parsed: {parsed:?}");
    assert!(parsed.contains(&"shell".to_string()), "parsed: {parsed:?}");
    assert!(
        parsed.contains(&"--stdin".to_string()),
        "parsed: {parsed:?}"
    );
    assert!(
        parsed.contains(&"--exec-target".to_string()),
        "--exec-target should be forwarded: {parsed:?}"
    );
    assert!(
        parsed.contains(&"posix-shell".to_string()),
        "posix-shell value should be forwarded: {parsed:?}"
    );
    assert!(parsed.contains(&"bash".to_string()), "parsed: {parsed:?}");
    assert!(
        !parsed.contains(&"--host".to_string()),
        "parsed: {parsed:?}"
    );
}

// -- Slice 1 (2026-07-08-remote-exec-host-parity.md): local-only verbs exit 2
//    with a pre-filled ssh fallback, before any connection --

/// A poison ssh shim: running it is the failure the slice exists to
/// prevent, so it exits loudly. Returns the PATH dir.
fn poison_ssh() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let fake_ssh = tmp.path().join("ssh");
    std::fs::write(&fake_ssh, "#!/bin/sh\necho SSH_MUST_NOT_RUN >&2\nexit 99\n").unwrap();
    std::fs::set_permissions(&fake_ssh, PermissionsExt::from_mode(0o755)).unwrap();
    tmp
}

/// Extract the `try:` line's fallback and re-split it twice — once as the
/// local shell would (yielding `ssh <host> <remote-string>`), once as the
/// remote login shell would — returning the remote tendr argv. This pins
/// that the printed fallback is copy-paste correct, not just plausible.
fn parse_fallback_argv(stderr: &str, host: &str) -> Vec<String> {
    let try_line = stderr
        .lines()
        .find(|l| l.trim_start().starts_with("try:"))
        .unwrap_or_else(|| panic!("no try: line in stderr: {stderr}"));
    let cmd = try_line.trim_start().strip_prefix("try:").unwrap().trim();
    let local = shell_words::split(cmd).expect("try: line parses as shell words");
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

    let tmp = TempDir::new().unwrap();
    let marker = tmp.path().join("ssh-was-run");
    let fake_ssh = tmp.path().join("ssh");
    std::fs::write(
        &fake_ssh,
        format!("#!/bin/sh\ntouch {}\nexit 0\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&fake_ssh, PermissionsExt::from_mode(0o755)).unwrap();

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

// -- Slice 2 (2026-07-08-remote-exec-host-parity.md): remote exec via frame-from-stdin --

/// A fake ssh that drops everything up to and including the "tendr"
/// argv token, then executes the real tendr binary with the remainder —
/// the frame arrives on real stdin, the session root rides in on HOME.
/// This exercises the full frame path end-to-end on one machine.
fn fake_ssh_frame_shim() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let fake_ssh = tmp.path().join("ssh");
    std::fs::write(
        &fake_ssh,
        "#!/bin/sh\n\
         while [ $# -gt 0 ]; do a=\"$1\"; shift; [ \"$a\" = tendr ] && break; done\n\
         exec \"$TENDR_TEST_BIN\" \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake_ssh, PermissionsExt::from_mode(0o755)).unwrap();
    tmp
}

fn shim_path_env(tmp: &TempDir) -> String {
    format!("{}:/usr/bin:/bin", tmp.path().display())
}

/// `tendr --host h exec …` works end-to-end: the envelope comes back on
/// stdout and the exec ran against the (shim-local) remote session.
#[test]
fn host_exec_frame_end_to_end() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    harness::tendr(&root)
        .args(["start", "shell", "--stdin", "--", "bash"])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "shell");
    harness::wait_running(&root, "shell");

    let tmp = fake_ssh_frame_shim();
    let output = harness::tendr(&root)
        .args(["--host", "box", "exec", "shell", "--", "echo", "remote hi"])
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

    harness::tendr(&root)
        .args(["start", "shell", "--stdin", "--", "bash"])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "shell");
    harness::wait_running(&root, "shell");

    let tmp = fake_ssh_frame_shim();
    let output = harness::tendr(&root)
        .args(["--host", "box", "exec", "shell", "--", "sh", "-c", "exit 7"])
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

    harness::tendr(&root)
        .args(["start", "shell", "--stdin", "--", "bash"])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "shell");
    harness::wait_running(&root, "shell");

    let tmp = fake_ssh_frame_shim();
    let output = harness::tendr(&root)
        .args([
            "--host",
            "box",
            "exec",
            "shell",
            "--timeout",
            "1",
            "--",
            "sleep",
            "3",
        ])
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

    harness::tendr(&root)
        .args(["start", "shell", "--stdin", "--", "bash"])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "shell");
    harness::wait_running(&root, "shell");

    // %s\n in printf's FORMAT: trailing newline without a newline byte
    // in argv (a newline-less payload trips the pre-existing sentinel
    // merge quirk and hangs exec); --timeout guards regressions.
    let torture = r#"it's a "test" $VAR `tick` back\slash ;&|"#;
    let argv = ["printf", "%s\\n", torture];

    let local = harness::tendr(&root)
        .args(["exec", "shell", "--timeout", "30", "--"])
        .args(argv)
        .output()
        .unwrap();
    assert!(local.status.success());

    let tmp = fake_ssh_frame_shim();
    let remote = harness::tendr(&root)
        .args(["--host", "box", "exec", "shell", "--timeout", "30", "--"])
        .args(argv)
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
        local_env["stdout"].as_str().unwrap().trim_end_matches('\n'),
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

    let stdout = String::from_utf8_lossy(&output.stdout);
    let args = parse_remote_argv(&stdout);
    assert_eq!(
        args,
        vec!["tendr", "--hello", "exec", "--frame-from-stdin"],
        "remote argv is constant — nothing user-controlled"
    );
    assert!(
        !stdout.contains(payload),
        "payload must not appear in argv: {stdout}"
    );
}

/// --host composes with --frame-from-stdin: the local frame passes
/// through to the remote verbatim (scripting-friendly).
#[test]
fn host_exec_frame_stdin_passes_through() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    harness::tendr(&root)
        .args(["start", "shell", "--stdin", "--", "bash"])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "shell");
    harness::wait_running(&root, "shell");

    let tmp = fake_ssh_frame_shim();
    let frame = r#"{"v":1,"session":"shell","cmd":["echo","piped frame"]}"#;
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

#[test]
fn start_boundary_is_reconstructed_for_ssh() {
    // --boundary / --boundary-parent must survive the local→remote arg
    // reconstruction so a `--host` start declares the same boundary remotely.
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = fake_ssh_echo();

    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .args([
            "--host",
            "user@box",
            "start",
            "job",
            "--boundary",
            "container:img:latest",
            "--boundary-parent",
            "host:data-box",
            "--",
            "echo",
            "hi",
        ])
        .env("PATH", tmp.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let args: Vec<&str> = stdout
        .lines()
        .filter_map(|l| l.strip_prefix("ARG:"))
        .collect();

    assert!(args.contains(&"--boundary"), "missing --boundary: {args:?}");
    assert!(
        args.contains(&"container:img:latest"),
        "missing boundary value: {args:?}"
    );
    assert!(
        args.contains(&"--boundary-parent"),
        "missing --boundary-parent: {args:?}"
    );
    assert!(
        args.contains(&"host:data-box"),
        "missing boundary-parent value: {args:?}"
    );
    // The trailing command must still be present and after the boundary flags.
    assert!(args.contains(&"echo"), "missing trailing cmd: {args:?}");
}

// --- host destination hardening (v0.2.1) ---

#[test]
fn host_rejects_option_shaped_destination_general() {
    // A `-`-prefixed --host destination must be rejected at the CLI boundary
    // (exit 2) BEFORE ssh is spawned, so the fake ssh (which prints ARG: lines)
    // is never invoked. Guards the general reconstructed-argv path.
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
