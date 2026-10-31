mod harness;

use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// Returns the Python interpreter argv for the current platform.
/// POSIX: `["python3", "-i"]`, Windows: `["py", "-3", "-i"]`.
fn python_repl_argv() -> &'static [&'static str] {
    if cfg!(windows) {
        &["py", "-3", "-i"]
    } else {
        &["python3", "-i"]
    }
}

/// Build tendr start args for a Python REPL session.
fn python_start_args(session: &str) -> Vec<String> {
    let mut args: Vec<String> = [
        "start",
        session,
        "--stdin",
        "--exec-target",
        "python-repl",
        "--",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.extend(python_repl_argv().iter().map(|s| s.to_string()));
    args
}

/// Build tendr start args for a Python session without --exec-target (for inference tests).
fn python_start_args_no_target(session: &str) -> Vec<String> {
    let mut args: Vec<String> = ["start", session, "--stdin", "--"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    args.extend(python_repl_argv().iter().map(|s| s.to_string()));
    args
}

// ---------------------------------------------------------------------------
// Native-shell fixture (WS3). The OS-neutral exec_* tests below drive a native
// interactive shell — bash + PosixShell on Unix, powershell + PowerShell on
// Windows — instead of hard-coding bash. The Windows path reaches the very same
// backend the #[cfg(windows)] exec_powershell_* tests already prove works.
//
// PowerShell exec joins a multi-element argv with "\n", so every PowerShell
// command below is a single argument (single-string commands), matching the
// exec_powershell_* pattern.
// ---------------------------------------------------------------------------

/// `tendr start` args for a native interactive shell session.
#[cfg(unix)]
fn native_shell_start_args(session: &str) -> Vec<String> {
    ["start", session, "--stdin", "--", "bash"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

#[cfg(windows)]
fn native_shell_start_args(session: &str) -> Vec<String> {
    [
        "start",
        session,
        "--stdin",
        "--exec-target",
        "powershell",
        "--",
        "powershell",
        "-NoProfile",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// The exec_target the native shell infers/declares, as it appears in meta.json
/// and in exec events.
#[cfg(unix)]
fn expected_shell_target() -> &'static str {
    "PosixShell"
}
#[cfg(windows)]
fn expected_shell_target() -> &'static str {
    "PowerShell"
}

/// A command that prints `text` verbatim on its own line.
#[cfg(unix)]
fn native_echo(text: &str) -> Vec<String> {
    vec!["echo".into(), text.into()]
}
#[cfg(windows)]
fn native_echo(text: &str) -> Vec<String> {
    // Single-quote the literal; PowerShell escapes an embedded quote by doubling.
    vec![format!("Write-Output '{}'", text.replace('\'', "''"))]
}

/// A command that exits non-zero (code 1) WITHOUT killing the REPL.
#[cfg(unix)]
fn native_failure() -> Vec<String> {
    vec!["false".into()]
}
#[cfg(windows)]
fn native_failure() -> Vec<String> {
    // `cmd /c exit 1` sets $LASTEXITCODE=1, which the PowerShell frame reads.
    vec!["cmd /c exit 1".into()]
}

/// A command that sleeps for `secs` seconds.
#[cfg(unix)]
fn native_sleep(secs: u64) -> Vec<String> {
    vec!["sleep".into(), secs.to_string()]
}
#[cfg(windows)]
fn native_sleep(secs: u64) -> Vec<String> {
    vec![format!("Start-Sleep {secs}")]
}

/// A command that changes the session's working directory to `path`.
#[cfg(unix)]
fn native_cd(path: &std::path::Path) -> Vec<String> {
    vec!["cd".into(), path.display().to_string()]
}
#[cfg(windows)]
fn native_cd(path: &std::path::Path) -> Vec<String> {
    vec![format!("Set-Location '{}'", path.display())]
}

/// `tendr exec <session> -- <cmd...>` argv for the native fixture.
fn exec_argv(session: &str, cmd: Vec<String>) -> Vec<String> {
    let mut args = vec!["exec".to_string(), session.to_string(), "--".to_string()];
    args.extend(cmd);
    args
}

/// exec fails if session doesn't exist.
#[test]
fn exec_session_not_found() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();
    harness::tendr(&root)
        .args(["exec", "nonexistent", "--", "pwd"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("session not found"));
}

/// exec fails if session is not running (terminal state).
#[test]
fn exec_session_not_running() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();
    harness::tendr(&root)
        .args(["start", "job1", "--", "true"])
        .assert()
        .success();
    harness::wait_terminal(&root, "job1");
    harness::tendr(&root)
        .args(["exec", "job1", "--", "pwd"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("not running"));
}

/// Basic exec: run echo in the native shell, get structured output.
#[test]
fn exec_basic_command() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    // Start the native shell with --stdin
    harness::tendr(&root)
        .args(native_shell_start_args("shell"))
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "shell");
    harness::wait_running(&root, "shell");

    // Exec a command
    let output = harness::tendr(&root)
        .args(exec_argv("shell", native_echo("hello world")))
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"].as_i64(), Some(0));
    assert!(result["stdout"].as_str().unwrap().contains("hello world"));
    assert!(!result["timed_out"].as_bool().unwrap());
    let cwd = result["cwd_after"].as_str().unwrap();
    // cwd_after is absolute: a drive path from the Windows PowerShell frame, a
    // /-rooted path from a POSIX shell (starts_with('/') also covers MSYS-style
    // paths that Path::is_absolute() misses on Windows).
    assert!(
        std::path::Path::new(cwd).is_absolute() || cwd.starts_with('/'),
        "cwd_after should be absolute, got: {cwd}"
    );
}

/// exec fails if session lacks --stdin.
#[test]
fn exec_session_no_stdin() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();
    harness::tendr(&root)
        .args(["start", "job1", "--", "sleep", "30"])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "job1");
    harness::wait_running(&root, "job1");
    harness::tendr(&root)
        .args(["exec", "job1", "--", "pwd"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--stdin"));
}

/// exec propagates non-zero exit code; shell stays alive.
#[test]
fn exec_nonzero_exit() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args(native_shell_start_args("shell"))
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "shell");
    harness::wait_running(&root, "shell");

    let output = harness::tendr(&root)
        .args(exec_argv("shell", native_failure()))
        .output()
        .unwrap();

    assert!(!output.status.success());
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"].as_i64(), Some(1));

    // Shell still running after failed command
    let status_output = harness::tendr(&root)
        .args(["status", "shell"])
        .output()
        .unwrap();
    let status: serde_json::Value = serde_json::from_slice(&status_output.stdout).unwrap();
    assert_eq!(status["status"].as_str(), Some("Running"));
}

/// Shell state (cwd) persists across exec calls.
#[test]
fn exec_cwd_persists() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args(native_shell_start_args("shell"))
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "shell");
    harness::wait_running(&root, "shell");

    // A test-chosen directory (distinct from HOME) so the assertion is exact
    // rather than a fuzzy "tmp" substring. Canonicalize both sides to absorb
    // macOS /var→/private/var symlinks and Windows path normalization.
    let target = tempfile::TempDir::new().unwrap();
    let canon_target = std::fs::canonicalize(target.path()).unwrap();

    // cd into the target directory
    let output1 = harness::tendr(&root)
        .args(exec_argv("shell", native_cd(target.path())))
        .output()
        .unwrap();
    let result1: serde_json::Value = serde_json::from_slice(&output1.stdout).unwrap();
    let cwd1 = result1["cwd_after"].as_str().unwrap();
    assert_eq!(
        std::fs::canonicalize(cwd1).unwrap(),
        canon_target,
        "cwd_after should be the directory we changed into, got: {cwd1}"
    );

    // The next exec must still observe the target dir — state persisted.
    let output2 = harness::tendr(&root)
        .args(exec_argv("shell", native_echo("persist")))
        .output()
        .unwrap();
    let result2: serde_json::Value = serde_json::from_slice(&output2.stdout).unwrap();
    let cwd2 = result2["cwd_after"].as_str().unwrap();
    assert_eq!(cwd1, cwd2, "cwd persists across exec calls");
    assert_eq!(
        std::fs::canonicalize(cwd2).unwrap(),
        canon_target,
        "persisted cwd_after should be the target dir, got: {cwd2}"
    );
}

/// Annotation event is written to output.log after exec.
#[test]
fn exec_writes_annotation() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args(native_shell_start_args("shell"))
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "shell");
    harness::wait_running(&root, "shell");

    harness::tendr(&root)
        .args(exec_argv("shell", native_echo("annotated")))
        .assert()
        .success();

    let log_path = root.path().join(".tendr/sessions/default/shell/output.log");
    let content = std::fs::read_to_string(&log_path).unwrap();
    let ann_line: serde_json::Value = content
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|line| line["tag"] == "A" && line["content"]["source"] == "agent.exec")
        .expect("annotation line should exist in output.log");
    let ann = &ann_line["content"];
    assert_eq!(ann["source"].as_str(), Some("agent.exec"));
    assert_eq!(ann["event"].as_str(), Some("exec"));
    assert_eq!(ann["data"]["hook_exit_code"].as_i64(), Some(0));
    assert!(ann["data"]["command"].is_array());
}

/// Oversized exec output must be quiet — no "annotation too large" warning on
/// stderr — and must still leave a compact `exec_truncated` breadcrumb in
/// output.log instead of dropping the record silently.
/// (exec-annotation-ergonomics)
#[test]
fn exec_oversized_output_is_quiet_and_leaves_breadcrumb() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args(native_shell_start_args("shell"))
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "shell");
    harness::wait_running(&root, "shell");

    // Both streams multi-KB so even the field-truncated annotation overflows
    // the cap (MAX_LINE 4096 / MAX_FIELD_BYTES 3000), forcing the breadcrumb.
    // POSIX writes to fd 2 without erroring (exit 0). PowerShell's side-channel
    // frame records stderr only from the error stream, and any captured error
    // forces exit 1 — so a large *captured* stderr and exit 0 are mutually
    // exclusive there. We take the large-stderr path (Write-Error, the same
    // mechanism exec_powershell_stderr_separated relies on) and let exec mirror
    // the inner exit 1.
    let big_cmd: Vec<String> = if cfg!(windows) {
        vec![
            "1..2000 | ForEach-Object { $_ }; 1..2000 | ForEach-Object { Write-Error $_ }"
                .to_string(),
        ]
    } else {
        vec![
            "bash".to_string(),
            "-c".to_string(),
            "seq 1 2000; seq 1 2000 >&2".to_string(),
        ]
    };
    let output = harness::tendr(&root)
        .args(exec_argv("shell", big_cmd))
        .output()
        .unwrap();
    if cfg!(windows) {
        assert_eq!(output.status.code(), Some(1));
    } else {
        assert!(output.status.success());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("annotation too large"),
        "annotation overflow must be silent, got stderr: {stderr:?}"
    );

    let log_path = root.path().join(".tendr/sessions/default/shell/output.log");
    let content = std::fs::read_to_string(&log_path).unwrap();
    let ann = content
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|line| line["tag"] == "A" && line["content"]["event"] == "exec_truncated")
        .expect("an exec_truncated breadcrumb should exist in output.log");
    let data = &ann["content"]["data"];
    assert!(
        data["stdout_len"].as_u64().unwrap() > 3000,
        "breadcrumb records the true stdout length"
    );
    assert!(data["stderr_len"].as_u64().unwrap() > 3000);
    assert!(data["stdout_sha256"].is_string());
    assert!(data["stderr_sha256"].is_string());
    assert_eq!(data["truncated"], true);
}

/// exec --timeout: returns timeout error, shell stays alive.
#[test]
fn exec_timeout() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args(native_shell_start_args("shell"))
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "shell");
    harness::wait_running(&root, "shell");

    let mut args = vec![
        "exec".to_string(),
        "shell".to_string(),
        "--timeout".to_string(),
        "1".to_string(),
        "--".to_string(),
    ];
    args.extend(native_sleep(4));
    let output = harness::tendr(&root).args(args).output().unwrap();

    assert_eq!(output.status.code(), Some(124));
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(result["timed_out"].as_bool().unwrap());

    // Shell should still be running
    let status_output = harness::tendr(&root)
        .args(["status", "shell"])
        .output()
        .unwrap();
    let status: serde_json::Value = serde_json::from_slice(&status_output.stdout).unwrap();
    assert_eq!(status["status"].as_str(), Some("Running"));
}

/// Second concurrent exec fails with busy error.
#[test]
fn exec_concurrent_busy() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args(native_shell_start_args("shell"))
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "shell");
    harness::wait_running(&root, "shell");

    // Start a long exec in the background (holds the exec lock while it sleeps)
    let mut long_exec = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
        .env("HOME", root.path())
        .args(exec_argv("shell", native_sleep(30)))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    // exec.started is appended only after the background command owns the
    // exec lock, so it is the observable precondition for the busy probe.
    harness::wait_event_kind(&root, "shell", "exec.started");

    // Second exec should fail with busy
    harness::tendr(&root)
        .args(exec_argv("shell", native_echo("hello")))
        .assert()
        .failure()
        .stderr(predicates::str::contains("another exec"));

    // Clean up
    let _ = long_exec.kill();
    let _ = long_exec.wait();
}

/// exec with explicit --exec-target posix-shell.
// POSIX-shell contract; Windows parity is the exec_powershell_* tests, not this.
#[cfg(unix)]
#[test]
fn exec_explicit_posix_target() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();
    harness::tendr(&root)
        .args([
            "start",
            "shell",
            "--stdin",
            "--exec-target",
            "posix-shell",
            "--",
            "bash",
        ])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "shell");
    harness::wait_running(&root, "shell");

    let output = harness::tendr(&root)
        .args(["exec", "shell", "--", "echo", "explicit"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"].as_i64(), Some(0));
    assert!(result["stdout"].as_str().unwrap().contains("explicit"));
}

/// exec on a session with no exec target fails with clear message.
#[test]
fn exec_none_target_rejected() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();
    // sleep is not a shell — infers ExecTarget::None
    harness::tendr(&root)
        .args(["start", "sleeper", "--stdin", "--", "sleep", "60"])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "sleeper");
    harness::wait_running(&root, "sleeper");

    harness::tendr(&root)
        .args(["exec", "sleeper", "--", "echo", "hello"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no exec target"));
}

/// bash infers PosixShell, exec works without --exec-target.
// POSIX-shell contract; Windows parity is the exec_powershell_* tests, not this.
#[cfg(unix)]
#[test]
fn exec_infers_posix_from_bash() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();
    harness::tendr(&root)
        .args(["start", "shell", "--stdin", "--", "bash"])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "shell");
    harness::wait_running(&root, "shell");

    let output = harness::tendr(&root)
        .args(["exec", "shell", "--", "echo", "inferred"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(result["stdout"].as_str().unwrap().contains("inferred"));
}

/// Invalid --exec-target value fails at start (clap rejects it).
#[test]
fn start_invalid_exec_target() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();
    harness::tendr(&root)
        .args([
            "start",
            "shell",
            "--stdin",
            "--exec-target",
            "fish",
            "--",
            "bash",
        ])
        .assert()
        .failure();
}

/// Different --exec-target creates a session conflict (different spec hash).
#[test]
fn exec_target_changes_session_identity() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    // Start the native shell; its exec-target is expected_shell_target().
    harness::tendr(&root)
        .args(native_shell_start_args("shell"))
        .assert()
        .success();
    harness::wait_running(&root, "shell");

    // Re-start the same name with the *opposite* exec-target. A different
    // exec-target changes the spec hash, so identity conflicts on either OS.
    let (other_target, interp): (&str, &[&str]) = if expected_shell_target() == "PosixShell" {
        ("powershell", &["bash"])
    } else {
        ("posix-shell", &["powershell", "-NoProfile"])
    };
    let mut conflict_args = vec![
        "start",
        "shell",
        "--stdin",
        "--exec-target",
        other_target,
        "--",
    ];
    let _session = harness::SessionGuard::new(&root, "shell");
    conflict_args.extend_from_slice(interp);
    harness::tendr(&root)
        .args(conflict_args)
        .assert()
        .failure()
        .stderr(predicates::str::contains("session conflict"));
}

/// Python REPL exec: basic print statement.
#[test]
fn exec_python_repl_basic() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args(python_start_args("py"))
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "py");
    harness::wait_running(&root, "py");

    let output = harness::tendr(&root)
        .args([
            "exec",
            "py",
            "--timeout",
            "10",
            "--",
            "print('hello from python')",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"].as_i64(), Some(0));
    assert!(
        result["stdout"]
            .as_str()
            .unwrap()
            .contains("hello from python")
    );
    let cwd = result["cwd_after"].as_str().unwrap();
    assert!(
        std::path::Path::new(cwd).is_absolute(),
        "cwd_after should be absolute, got: {cwd}"
    );
}

/// Python REPL exec: exception produces non-zero exit and traceback in stderr.
#[test]
fn exec_python_repl_exception() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args(python_start_args("py"))
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "py");
    harness::wait_running(&root, "py");

    let output = harness::tendr(&root)
        .args([
            "exec",
            "py",
            "--timeout",
            "10",
            "--",
            "raise ValueError('boom')",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"].as_i64(), Some(1));
    assert!(result["stderr"].as_str().unwrap().contains("ValueError"));
    assert!(result["stderr"].as_str().unwrap().contains("boom"));
}

/// Python REPL exec: cwd changes are tracked.
#[test]
fn exec_python_repl_cwd() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args(python_start_args("py"))
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "py");
    harness::wait_running(&root, "py");

    let tmp = std::env::temp_dir();
    let tmp_str = tmp.to_str().expect("temp dir should be valid UTF-8");
    // Use forward slashes — valid on Windows and avoids raw string edge cases
    let chdir_code = format!("import os; os.chdir('{}')", tmp_str.replace('\\', "/"));
    let output = harness::tendr(&root)
        .args(["exec", "py", "--timeout", "10", "--", &chdir_code])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "exec failed:\nstderr: {}\nstdout: {}\ncode: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
        chdir_code,
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"].as_i64(), Some(0));
    let cwd = result["cwd_after"].as_str().unwrap();
    assert!(
        std::path::Path::new(cwd).is_absolute(),
        "cwd should be absolute after chdir, got: {cwd}"
    );
}

/// python/python3/ipython (and Windows `py`) infer PythonRepl when started
/// as an interactive Python session.
#[test]
fn exec_python_inferred() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args(python_start_args_no_target("py"))
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "py");
    harness::wait_running(&root, "py");

    let output = harness::tendr(&root)
        .args(["exec", "py", "--", "print(1+1)"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "exec failed:\nstderr: {}\nstdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"].as_i64(), Some(0));
    assert_eq!(result["stdout"].as_str().unwrap().trim(), "2");
}

/// DuckDB inferred from argv[0].
#[test]
fn exec_infers_duckdb() {
    if !harness::duckdb_or_skip() {
        return;
    }
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args(["start", "db", "--stdin", "--", "duckdb"])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "db");
    harness::wait_running(&root, "db");

    // Verify the session was created with DuckDb exec target
    let status_output = harness::tendr(&root)
        .args(["status", "db"])
        .output()
        .unwrap();
    let status: serde_json::Value = serde_json::from_slice(&status_output.stdout).unwrap();
    assert_eq!(
        status["launch_spec"]["exec_target"].as_str(),
        Some("DuckDb")
    );
}

/// DuckDB exec: basic SELECT query returns structured JSON in stdout.
#[test]
fn exec_duckdb_basic_select() {
    if !harness::duckdb_or_skip() {
        return;
    }
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args([
            "start",
            "db",
            "--stdin",
            "--exec-target",
            "duckdb",
            "--",
            "duckdb",
        ])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "db");
    harness::wait_running(&root, "db");

    let output = harness::tendr(&root)
        .args([
            "exec",
            "db",
            "--timeout",
            "10",
            "--",
            "SELECT 42 as answer;",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"].as_i64(), Some(0));
    assert!(!result["timed_out"].as_bool().unwrap());
    assert_eq!(result["cwd_after"].as_str(), Some("."));

    // Query results flow through stdout (captured from the output log)
    let stdout = result["stdout"].as_str().expect("stdout should be present");
    assert!(
        stdout.contains("42"),
        "stdout should contain query result with 42, got: {stdout}"
    );

    // No exec-results directory — DuckDB doesn't use side-channel files
    let results_dir = root.path().join(".tendr/sessions/default/db/exec-results");
    assert!(
        !results_dir.exists(),
        "exec-results/ should not exist for DuckDB"
    );
}

/// DuckDB exec: SQL error reports exit_code 1 but keeps the session alive.
#[test]
fn exec_duckdb_sql_error() {
    if !harness::duckdb_or_skip() {
        return;
    }
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args([
            "start",
            "db",
            "--stdin",
            "--exec-target",
            "duckdb",
            "--",
            "duckdb",
        ])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "db");
    harness::wait_running(&root, "db");

    // Invalid SQL — error goes to stderr, sentinel still fires, exit_code = 1.
    let output = harness::tendr(&root)
        .args([
            "exec",
            "db",
            "--timeout",
            "10",
            "--",
            "SELECT * FROM nonexistent_table_xyz;",
        ])
        .output()
        .unwrap();

    // tendr exec mirrors the inner exit code — exits 1 on SQL error.
    // JSON result is still printed to stdout.
    assert_eq!(
        output.status.code(),
        Some(1),
        "tendr exec should exit 1 (mirroring SQL error)"
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("stdout should contain JSON result even on error");
    assert_eq!(result["exit_code"].as_i64(), Some(1));
    assert!(!result["timed_out"].as_bool().unwrap());
    let stderr = result["stderr"].as_str().unwrap_or("");
    assert!(
        stderr.contains("nonexistent_table_xyz"),
        "stderr should contain the error table name, got: {stderr}"
    );

    // Session should still be running — errors don't kill DuckDB
    harness::tendr(&root)
        .args(["status", "db"])
        .assert()
        .success();

    // Verify the session can still handle another query after error
    let output2 = harness::tendr(&root)
        .args([
            "exec",
            "db",
            "--timeout",
            "10",
            "--",
            "SELECT 'recovered' as status;",
        ])
        .output()
        .unwrap();
    assert!(output2.status.success());
    let result2: serde_json::Value = serde_json::from_slice(&output2.stdout).unwrap();
    assert_eq!(result2["exit_code"].as_i64(), Some(0));
    let stdout2 = result2["stdout"].as_str().unwrap_or("");
    assert!(
        stdout2.contains("recovered"),
        "second query should succeed after error: {stdout2}"
    );
}

/// DuckDB exec: multiple statements produce concatenated results.
#[test]
fn exec_duckdb_multi_statement() {
    if !harness::duckdb_or_skip() {
        return;
    }
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args([
            "start",
            "db",
            "--stdin",
            "--exec-target",
            "duckdb",
            "--",
            "duckdb",
        ])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "db");
    harness::wait_running(&root, "db");

    let output = harness::tendr(&root)
        .args([
            "exec",
            "db",
            "--timeout",
            "10",
            "--",
            "SELECT 1 as a;\nSELECT 2 as b;",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"].as_i64(), Some(0));

    // Both query results should be in stdout
    let stdout = result["stdout"].as_str().expect("stdout should be present");
    assert!(
        stdout.contains('1'),
        "stdout should contain first query result"
    );
    assert!(
        stdout.contains('2'),
        "stdout should contain second query result"
    );
}

/// DuckDB exec with explicit --exec-target duckdb.
#[test]
fn exec_duckdb_explicit_target() {
    if !harness::duckdb_or_skip() {
        return;
    }
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args([
            "start",
            "db",
            "--stdin",
            "--exec-target",
            "duckdb",
            "--",
            "duckdb",
        ])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "db");
    harness::wait_running(&root, "db");

    let output = harness::tendr(&root)
        .args([
            "exec",
            "db",
            "--timeout",
            "10",
            "--",
            "SELECT 'hello' as greeting;",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"].as_i64(), Some(0));
}

/// DuckDB exec: mixed success — first statement succeeds, second fails.
/// Must report exit_code 1 even though stdout has partial results.
#[test]
fn exec_duckdb_mixed_success_reports_error() {
    if !harness::duckdb_or_skip() {
        return;
    }
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args([
            "start",
            "db",
            "--stdin",
            "--exec-target",
            "duckdb",
            "--",
            "duckdb",
        ])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "db");
    harness::wait_running(&root, "db");

    let output = harness::tendr(&root)
        .args([
            "exec",
            "db",
            "--timeout",
            "10",
            "--",
            "SELECT 1 as ok;\nSELECT * FROM nonexistent_table_xyz;",
        ])
        .output()
        .unwrap();

    let result: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should contain JSON result");
    assert_eq!(
        result["exit_code"].as_i64(),
        Some(1),
        "mixed success should report exit_code 1"
    );
    // stdout should still have the first query's results
    let stdout = result["stdout"].as_str().unwrap_or("");
    assert!(
        stdout.contains('1'),
        "stdout should contain first query result: {stdout}"
    );
    // stderr should have the error from the second query
    let stderr = result["stderr"].as_str().unwrap_or("");
    assert!(
        stderr.contains("nonexistent_table_xyz"),
        "stderr should contain the error: {stderr}"
    );

    // Session should still be alive
    harness::tendr(&root)
        .args(["status", "db"])
        .assert()
        .success();
}

/// DuckDB exec: paths with spaces work correctly (no .output path escaping needed).
#[test]
fn exec_duckdb_path_with_spaces() {
    if !harness::duckdb_or_skip() {
        return;
    }
    let _lock = lock();
    // Create a temp dir whose path includes spaces
    let parent = tempfile::TempDir::new().unwrap();
    let spaced_dir = parent.path().join("path with spaces");
    std::fs::create_dir_all(&spaced_dir).unwrap();

    // Use the spaced directory as HOME so session paths contain spaces
    let mut cmd = assert_cmd::Command::cargo_bin("tendr").unwrap();
    cmd.env("HOME", &spaced_dir);
    cmd.args([
        "start",
        "db",
        "--stdin",
        "--exec-target",
        "duckdb",
        "--",
        "duckdb",
    ]);
    cmd.assert().success();

    // Wait for running
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let mut status_cmd = assert_cmd::Command::cargo_bin("tendr").unwrap();
        status_cmd.env("HOME", &spaced_dir);
        status_cmd.args(["status", "db"]);
        let out = status_cmd.output().unwrap();
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            if s.contains("Running") {
                break;
            }
        }
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for db to start");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let mut exec_cmd = assert_cmd::Command::cargo_bin("tendr").unwrap();
    exec_cmd.env("HOME", &spaced_dir);
    exec_cmd.args([
        "exec",
        "db",
        "--timeout",
        "10",
        "--",
        "SELECT 42 as answer;",
    ]);
    let output = exec_cmd.output().unwrap();

    assert!(
        output.status.success(),
        "exec with spaces in path failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"].as_i64(), Some(0));
    let stdout = result["stdout"].as_str().unwrap_or("");
    assert!(
        stdout.contains("42"),
        "stdout should contain query result: {stdout}"
    );

    let mut kill_cmd = assert_cmd::Command::cargo_bin("tendr").unwrap();
    kill_cmd.env("HOME", &spaced_dir);
    kill_cmd.args(["kill", "db", "--force"]);
    let _ = kill_cmd.output();
}

// ---------------------------------------------------------------------------
// PowerShell side-channel exec — Windows-gated.
// ---------------------------------------------------------------------------
//
// These tests start a `powershell -NoProfile` session with --exec-target
// powershell and verify the side-channel result file path: stdout in the
// envelope is clean (no prompt, no echoed framing), stderr is partitioned
// from stdout, exit codes propagate, state persists across exec calls, and
// cwd_after tracks Set-Location.

#[cfg(windows)]
fn powershell_start_args(session: &str) -> Vec<String> {
    // Plain `powershell -NoProfile` enters interactive REPL mode and reads
    // commands from the persistent stdin pipe. `-Command -` would buffer
    // until EOF and only execute once — incompatible with multiple execs.
    [
        "start",
        session,
        "--stdin",
        "--exec-target",
        "powershell",
        "--",
        "powershell",
        "-NoProfile",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// PowerShell exec: simple echo produces clean stdout — no prompt prefix,
/// no echoed framing, just the user's output.
///
/// Multi-element argv joins with `\n` (matching Python REPL semantics),
/// so we pass the cmdlet+args as a single argument to keep it on one line.
#[cfg(windows)]
#[test]
fn exec_powershell_clean_stdout() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args(powershell_start_args("ps"))
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "ps");
    harness::wait_running(&root, "ps");

    let output = harness::tendr(&root)
        .args(["exec", "ps", "--timeout", "15", "--", "echo hello-world"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"].as_i64(), Some(0));
    let stdout = result["stdout"].as_str().unwrap();
    assert!(
        !stdout.contains("PS "),
        "stdout must not contain prompt: {stdout:?}"
    );
    assert!(
        !stdout.contains("__TENDR_EXEC__"),
        "stdout must not contain framing"
    );
    assert!(
        !stdout.contains("FromBase64String"),
        "stdout must not contain frame source"
    );
    assert_eq!(stdout.trim(), "hello-world");
}

/// PowerShell exec: arbitrary expression — `$x = 1; $x + 1` → stdout `2`.
#[cfg(windows)]
#[test]
fn exec_powershell_arbitrary_expression() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args(powershell_start_args("ps"))
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "ps");
    harness::wait_running(&root, "ps");

    let output = harness::tendr(&root)
        .args(["exec", "ps", "--timeout", "15", "--", "$x = 1; $x + 1"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"].as_i64(), Some(0));
    assert_eq!(result["stdout"].as_str().unwrap().trim(), "2");
}

/// PowerShell exec: pipeline emits each item on its own line.
#[cfg(windows)]
#[test]
fn exec_powershell_pipeline() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args(powershell_start_args("ps"))
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "ps");
    harness::wait_running(&root, "ps");

    let output = harness::tendr(&root)
        .args([
            "exec",
            "ps",
            "--timeout",
            "15",
            "--",
            "1..3 | ForEach-Object { $_ * 10 }",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"].as_i64(), Some(0));
    let stdout = result["stdout"].as_str().unwrap();
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines,
        vec!["10", "20", "30"],
        "pipeline output unexpected: {stdout:?}"
    );
}

/// PowerShell exec: variables persist across exec calls (same REPL session).
#[cfg(windows)]
#[test]
fn exec_powershell_state_persists_across_calls() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args(powershell_start_args("ps"))
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "ps");
    harness::wait_running(&root, "ps");

    // Set a variable
    harness::tendr(&root)
        .args([
            "exec",
            "ps",
            "--timeout",
            "15",
            "--",
            "$global:tendr_test_var = 42",
        ])
        .assert()
        .success();

    // Read it back in a separate exec
    let output = harness::tendr(&root)
        .args([
            "exec",
            "ps",
            "--timeout",
            "15",
            "--",
            "$global:tendr_test_var",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["exit_code"].as_i64(), Some(0));
    assert_eq!(result["stdout"].as_str().unwrap().trim(), "42");
}

/// PowerShell exec: Write-Error goes to stderr field, not stdout.
#[cfg(windows)]
#[test]
fn exec_powershell_stderr_separated() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args(powershell_start_args("ps"))
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "ps");
    harness::wait_running(&root, "ps");

    let output = harness::tendr(&root)
        .args(["exec", "ps", "--timeout", "15", "--", "Write-Error 'oops'"])
        .output()
        .unwrap();

    // Write-Error sets $? = false, so frame reports exit 1; CLI propagates.
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        result["stderr"].as_str().unwrap().contains("oops"),
        "stderr should contain error: {:?}",
        result["stderr"]
    );
    assert!(
        !result["stdout"].as_str().unwrap().contains("oops"),
        "stderr must not leak into stdout: {:?}",
        result["stdout"]
    );
}

/// PowerShell exec: Set-Location is reflected in cwd_after on next call.
#[cfg(windows)]
#[test]
fn exec_powershell_cwd_after() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args(powershell_start_args("ps"))
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "ps");
    harness::wait_running(&root, "ps");

    // Change directory to C:\
    let output = harness::tendr(&root)
        .args(["exec", "ps", "--timeout", "15", "--", "Set-Location C:\\"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let cwd = result["cwd_after"].as_str().unwrap();
    assert!(
        cwd.eq_ignore_ascii_case("C:\\") || cwd.eq_ignore_ascii_case("C:/"),
        "cwd_after should reflect Set-Location, got: {cwd:?}"
    );
}

/// Side-channel result file is cleaned up after exec.
#[test]
fn exec_python_result_file_cleaned() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args(python_start_args("py"))
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "py");
    harness::wait_running(&root, "py");

    harness::tendr(&root)
        .args([
            "exec",
            "py",
            "--timeout",
            "10",
            "--",
            "print('cleanup test')",
        ])
        .assert()
        .success();

    // exec-results/ dir should exist but be empty (result file was cleaned up)
    let results_dir = root.path().join(".tendr/sessions/default/py/exec-results");
    if results_dir.exists() {
        let entries: Vec<_> = std::fs::read_dir(&results_dir).unwrap().collect();
        assert!(
            entries.is_empty(),
            "result files should be cleaned up, found: {entries:?}"
        );
    }
}

// --- Slice 3: exec.started / exec.result events (plan scope 1) ---

/// exec emits exec.started + exec.result sharing a block_id, one writer,
/// contiguous seq, source tendr.exec, with the pinned data shapes.
#[test]
fn exec_emits_started_and_result_events() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args(native_shell_start_args("shell"))
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "shell");
    harness::wait_running(&root, "shell");

    let echo_cmd = native_echo("event brigade");
    let output = harness::tendr(&root)
        .args(exec_argv("shell", echo_cmd.clone()))
        .output()
        .unwrap();
    assert!(output.status.success());

    // The JSON stdout envelope is frozen: exactly the shipped fields
    // (serde_json yields keys alphabetically).
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let keys: Vec<&str> = envelope
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "cwd_after",
            "exit_code",
            "session",
            "stderr",
            "stdout",
            "timed_out",
            "truncated"
        ],
        "exec JSON envelope field set is frozen"
    );

    let events = harness::read_events(&root, "shell");
    let started = events
        .iter()
        .find(|e| e["kind"] == "exec.started")
        .expect("exec.started");
    let result = events
        .iter()
        .find(|e| e["kind"] == "exec.result")
        .expect("exec.result");

    assert_eq!(started["source"], "tendr.exec");
    assert_eq!(result["source"], "tendr.exec");
    assert_eq!(started["data"]["command"], serde_json::json!(echo_cmd));
    assert_eq!(started["data"]["exec_target"], expected_shell_target());
    assert!(
        started["data"].get("timeout_ms").is_none(),
        "no timeout flag → no field"
    );
    assert_eq!(result["data"]["exit_code"], 0);
    assert!(
        result["data"]["stdout"]
            .as_str()
            .unwrap()
            .contains("event brigade")
    );
    assert_eq!(result["data"]["timed_out"], false);
    assert_eq!(result["data"]["truncated"], false);
    assert!(!result["data"]["cwd_after"].as_str().unwrap().is_empty());

    let block = started["block_id"].as_str().expect("started has block_id");
    assert_eq!(
        result["block_id"].as_str().unwrap(),
        block,
        "one block, both events"
    );
    assert_eq!(
        started["writer"], result["writer"],
        "one writer for the pair"
    );
    assert_eq!(
        started["seq"].as_u64().unwrap() + 1,
        result["seq"].as_u64().unwrap(),
        "contiguous seq"
    );
    assert_eq!(started["gen"], 1);
    assert!(
        started.get("parent_id").is_none(),
        "no ambient chain → no parent"
    );
}

/// A tendr exec running inside an outer block chains upward: parent_id
/// from the exec process's own env, block_id freshly minted.
#[test]
fn exec_events_inherit_parent_from_env_chain() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args(native_shell_start_args("shell"))
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "shell");
    harness::wait_running(&root, "shell");

    let outer = uuid::Uuid::now_v7().to_string();
    harness::tendr(&root)
        .env("TENDR_BLOCK_ID", &outer)
        .args(exec_argv("shell", native_echo("chain")))
        .assert()
        .success();

    let events = harness::read_events(&root, "shell");
    let started = events.iter().find(|e| e["kind"] == "exec.started").unwrap();
    let result = events.iter().find(|e| e["kind"] == "exec.result").unwrap();
    for event in [started, result] {
        assert_eq!(event["parent_id"].as_str().unwrap(), outer);
        assert_ne!(
            event["block_id"].as_str().unwrap(),
            outer,
            "fresh block per exec"
        );
    }
}

/// The exec A-line carries additive event_id (= exec.result id) and
/// block_id — same linkage contract as wrap (spec §0).
#[test]
fn exec_aline_links_event_id_and_block_id() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args(native_shell_start_args("shell"))
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "shell");
    harness::wait_running(&root, "shell");

    harness::tendr(&root)
        .args(exec_argv("shell", native_echo("linked")))
        .assert()
        .success();

    let events = harness::read_events(&root, "shell");
    let result = events.iter().find(|e| e["kind"] == "exec.result").unwrap();

    let log_path = root.path().join(".tendr/sessions/default/shell/output.log");
    let content = std::fs::read_to_string(&log_path).unwrap();
    let ann_line: serde_json::Value = content
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|line| line["tag"] == "A" && line["content"]["source"] == "agent.exec")
        .expect("annotation line exists");
    let ann = &ann_line["content"];
    assert_eq!(
        ann["event_id"], result["id"],
        "A-line links the exec.result event"
    );
    assert_eq!(ann["block_id"], result["block_id"]);
}

/// A timed-out exec still records its exec.result (timed_out true) and the
/// process exit code stays 124.
#[test]
fn exec_timeout_still_emits_result_event() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args(native_shell_start_args("shell"))
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "shell");
    harness::wait_running(&root, "shell");

    let mut args = vec![
        "exec".to_string(),
        "shell".to_string(),
        "--timeout".to_string(),
        "1".to_string(),
        "--".to_string(),
    ];
    args.extend(native_sleep(3));
    let output = harness::tendr(&root).args(args).output().unwrap();
    assert_eq!(output.status.code(), Some(124));

    let events = harness::read_events(&root, "shell");
    let started = events.iter().find(|e| e["kind"] == "exec.started").unwrap();
    let result = events.iter().find(|e| e["kind"] == "exec.result").unwrap();
    assert_eq!(started["data"]["timeout_ms"], 1000);
    assert_eq!(result["data"]["timed_out"], true);
    assert_eq!(result["data"]["exit_code"], -1);
}

/// Event emission is best-effort: an unwritable events dir warns but never
/// changes exec's output or exit code.
#[cfg(unix)]
#[test]
fn exec_event_append_is_best_effort() {
    use std::os::unix::fs::PermissionsExt;

    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args(["start", "shell", "--stdin", "--", "bash"])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "shell");
    harness::wait_running(&root, "shell");

    let events_dir = root.path().join(".tendr/sessions/default/shell/events");
    std::fs::set_permissions(&events_dir, std::fs::Permissions::from_mode(0o000)).unwrap();

    let output = harness::tendr(&root)
        .args(["exec", "shell", "--", "echo", "still fine"])
        .output()
        .unwrap();

    std::fs::set_permissions(&events_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(output.status.success(), "append failure never fails exec");
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(envelope["stdout"].as_str().unwrap().contains("still fine"));
}

/// The PosixShell frame exports TENDR_BLOCK_ID for the payload's duration:
/// the payload sees exactly the block_id its exec events carry.
#[cfg_attr(
    windows,
    ignore = "PowerShell TENDR_BLOCK_ID env propagation — windows-parity Phase 3"
)]
#[test]
fn exec_payload_sees_block_id_env() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args(["start", "shell", "--stdin", "--", "bash"])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "shell");
    harness::wait_running(&root, "shell");

    let output = harness::tendr(&root)
        .args(["exec", "shell", "--", "printenv", "TENDR_BLOCK_ID"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let seen = envelope["stdout"].as_str().unwrap().trim().to_owned();

    let events = harness::read_events(&root, "shell");
    let result = events.iter().find(|e| e["kind"] == "exec.result").unwrap();
    assert_eq!(seen, result["block_id"].as_str().unwrap());
}

// --- exec --frame-from-stdin ---

/// A valid v1 frame on stdin runs exactly like the argv form: same
/// envelope, same exit code, session named by the frame.
#[test]
fn exec_frame_from_stdin_runs_payload() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args(native_shell_start_args("shell"))
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "shell");
    harness::wait_running(&root, "shell");

    let frame = serde_json::json!({
        "v": 1,
        "session": "shell",
        "cmd": native_echo("framed hello"),
    });
    let output = harness::tendr(&root)
        .args(["exec", "--frame-from-stdin"])
        .write_stdin(frame.to_string())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "frame exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["exit_code"], 0);
    assert!(
        envelope["stdout"]
            .as_str()
            .unwrap()
            .contains("framed hello")
    );
    assert_eq!(envelope["session"], "shell");
}

/// The frame carries argv as a JSON array, so quoting-hostile payloads
/// never touch a shell on the way in: the payload byte-for-byte matches
/// what the in-session command receives.
// POSIX-shell contract (single-quote torture); Windows parity is the
// exec_powershell_* tests, not this.
#[cfg(unix)]
#[test]
fn exec_frame_payload_survives_quoting_torture() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args(["start", "shell", "--stdin", "--", "bash"])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "shell");
    harness::wait_running(&root, "shell");

    // printf's FORMAT string interprets \n (the payload arg does not),
    // giving a trailing newline without a newline byte in argv — a
    // payload that ends without one trips the pre-existing sentinel
    // merge quirk and hangs exec. timeout guards regressions.
    let torture = r#"it's a "test" with $VAR and back\slash"#;
    let frame = serde_json::json!({
        "v": 1,
        "session": "shell",
        "cmd": ["printf", "%s\\n", torture],
        "timeout": 30,
    });
    let output = harness::tendr(&root)
        .args(["exec", "--frame-from-stdin"])
        .write_stdin(frame.to_string())
        .output()
        .unwrap();

    assert!(output.status.success());
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        envelope["stdout"].as_str().unwrap().trim_end_matches('\n'),
        torture,
        "payload survives byte-exact"
    );
}

/// Frame timeout behaves like --timeout: exit 124, timed_out true.
#[test]
fn exec_frame_timeout_exits_124() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    harness::tendr(&root)
        .args(native_shell_start_args("shell"))
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "shell");
    harness::wait_running(&root, "shell");

    let frame = serde_json::json!({
        "v": 1,
        "session": "shell",
        "cmd": native_sleep(3),
        "timeout": 1,
    });
    let output = harness::tendr(&root)
        .args(["exec", "--frame-from-stdin"])
        .write_stdin(frame.to_string())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(124));
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["timed_out"], true);
}

/// Malformed frames are a usage error before any side effect: exit 2
/// with a message that names the frame, not a clap error.
#[test]
fn exec_frame_bad_json_exits_2() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    let output = harness::tendr(&root)
        .args(["exec", "--frame-from-stdin"])
        .write_stdin("this is not json")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid exec frame"),
        "frame parse error names the frame: {stderr}"
    );
}

/// An unsupported frame version is rejected loudly (exit 2, names the
/// version) so schema evolution stays honest.
#[test]
fn exec_frame_unsupported_version_exits_2() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    let frame = r#"{"v":2,"session":"shell","cmd":["true"]}"#;
    let output = harness::tendr(&root)
        .args(["exec", "--frame-from-stdin"])
        .write_stdin(frame)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("version"),
        "version rejection names the problem: {stderr}"
    );
}

/// --frame-from-stdin conflicts with the argv surface: the frame is the
/// whole request, nothing rides in argv beside it.
#[test]
fn exec_frame_conflicts_with_positional_args() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    let output = harness::tendr(&root)
        .args(["exec", "shell", "--frame-from-stdin", "--", "ls"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot be used with") || stderr.contains("conflict"),
        "clap reports the conflict: {stderr}"
    );
}

/// A frame naming a structurally invalid session is an invalid frame:
/// exit 2 before any session lookup or lock (review finding on PR #13).
#[test]
fn exec_frame_invalid_session_exits_2() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    let output = harness::tendr(&root)
        .args(["exec", "--frame-from-stdin"])
        .write_stdin(r#"{"v":1,"session":"bad/name","cmd":["true"]}"#)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid exec frame"),
        "invalid session is a frame error: {stderr}"
    );
}

/// An empty cmd is an invalid frame: exit 2 before the exec lock, not a
/// runtime "no command specified" after session lookup.
#[test]
fn exec_frame_empty_cmd_exits_2() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();

    let output = harness::tendr(&root)
        .args(["exec", "--frame-from-stdin"])
        .write_stdin(r#"{"v":1,"session":"shell","cmd":[]}"#)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid exec frame"),
        "empty cmd is a frame error: {stderr}"
    );
}

/// Drift guard: the ONLY exec tests permitted to carry a Windows `#[ignore]`.
/// Each entry is a deliberate, documented parity gap; adding another
/// Windows-ignored exec test without listing it here fails the suite, so the
/// ignored set can never grow silently and every name stays tracked.
#[test]
fn windows_ignored_exec_set_is_exactly_tracked() {
    // Tracked parity gaps (see the attribute above each named test):
    //   exec_payload_sees_block_id_env — PowerShell has no TENDR_BLOCK_ID env
    //     propagation yet (windows-parity Phase 3).
    const EXPECTED: &[&str] = &["exec_payload_sees_block_id_env"];

    let src = include_str!("cli_exec.rs");
    // Scan only the source *before* this guard, so the guard's own body can
    // never masquerade as a windows-ignore attribute (self-match immunity).
    let guard_at = src
        .find("fn windows_ignored_exec_set_is_exactly_tracked")
        .expect("guard fn present");
    let scan = &src[..guard_at];

    // A windows-ignore is a `cfg_attr(...)` whose body names both `windows` and
    // `ignore`, then a `fn <name>(`. Inspecting the attribute body (not a single
    // literal) is tolerant of rustfmt wrapping the attribute across lines.
    let mut found: Vec<String> = Vec::new();
    let mut rest = scan;
    while let Some(pos) = rest.find("cfg_attr(") {
        let after = &rest[pos + "cfg_attr(".len()..];
        let (body, post) = after.split_once(")]").unwrap_or((after, ""));
        if body.contains("windows") && body.contains("ignore") {
            let name = post
                .split_once("fn ")
                .and_then(|(_, tail)| tail.split('(').next())
                .map(|n| n.trim().to_string())
                .expect("a fn follows the windows-ignore attribute");
            found.push(name);
        }
        rest = after;
    }
    found.sort();

    let mut expected: Vec<String> = EXPECTED.iter().map(|s| s.to_string()).collect();
    expected.sort();
    assert_eq!(
        found, expected,
        "windows-ignored exec test set drifted; update EXPECTED and justify \
         each parity gap"
    );
}

// ── exec framing markers (#92, #95) ─────────────────────────────────────

#[cfg(unix)]
use harness::DeadlineAssertExt;

/// Start a POSIX shell session and return a guard that stops it.
#[cfg(unix)]
fn posix_shell<'a>(root: &'a tempfile::TempDir, name: &str) -> harness::SessionGuard<'a> {
    harness::tendr(root)
        .args(["start", name, "--stdin", "--", "bash"])
        .assert()
        .success();
    let guard = harness::SessionGuard::new(root, name);
    harness::wait_running(root, name);
    guard
}

/// Output without a trailing newline used to glue the sentinel onto the last
/// line, so exec never saw it and hung past --timeout (#95).
#[test]
#[cfg(unix)]
fn exec_stdout_without_trailing_newline_returns() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();
    let _session = posix_shell(&root, "shell");

    let output = harness::tendr(&root)
        .args(["exec", "shell", "--timeout", "5", "--", "printf", "foo"])
        .assert_within_deadline();
    let output = output.get_output();
    assert!(
        output.status.success(),
        "exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["stdout"], "foo", "{envelope}");
    assert_eq!(envelope["timed_out"], false, "{envelope}");
}

/// The stderr end marker lands in the log but never in exec's stderr (#92).
#[test]
#[cfg(unix)]
fn exec_stderr_marker_is_stripped() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();
    let _session = posix_shell(&root, "shell");

    let output = harness::tendr(&root)
        .args([
            "exec",
            "shell",
            "--",
            "sh",
            "-c",
            "echo x >&2; printf y >&2",
        ])
        .assert_within_deadline();
    let output = output.get_output();
    assert!(output.status.success());
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["stderr"], "x\ny", "{envelope}");

    let log = std::fs::read_to_string(root.path().join(".tendr/sessions/default/shell/output.log"))
        .unwrap();
    let marker_lines = log
        .lines()
        .filter(|l| l.contains(r#""tag":"E""#) && l.contains("__TENDR_EXEC_ERR__"))
        .count();
    assert_eq!(
        marker_lines, 1,
        "exactly one stderr end marker in the log:\n{log}"
    );
}

/// If the session shell's stderr no longer reaches the sidecar, the marker
/// never arrives; exec still returns, and says stderr may be incomplete.
#[test]
#[cfg(unix)]
fn exec_survives_redirected_shell_stderr() {
    let _lock = lock();
    let root = tempfile::TempDir::new().unwrap();
    let _session = posix_shell(&root, "shell");

    harness::tendr(&root)
        .args(["push", "shell"])
        .write_stdin("exec 2>/dev/null\n")
        .assert()
        .success();

    let output = harness::tendr(&root)
        .args(["exec", "shell", "--", "echo", "hi"])
        .assert_within_deadline();
    let output = output.get_output();
    assert!(output.status.success());
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["stdout"], "hi", "{envelope}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("stderr end marker not seen"),
        "the CLI must warn that stderr may be incomplete: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
