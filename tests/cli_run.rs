mod harness;

use harness::{DeadlineAssertExt, Home};
use predicates::prelude::*;

#[cfg(unix)]
const PYTHON: &str = "python3";
#[cfg(windows)]
const PYTHON: &str = "py";

#[test]
fn run_streams_output_and_propagates_exit_status() {
    let root = Home::new();
    harness::tendr(&root)
        .args([
            "run",
            "failure",
            "--",
            PYTHON,
            "-c",
            "print('run-output', flush=True); raise SystemExit(7)",
        ])
        .assert_within_deadline()
        .code(7)
        .stdout(predicate::str::contains("run-output"));
    harness::tendr(&root)
        .args(["status", "failure"])
        .assert_within_deadline()
        .success()
        .stdout(predicate::str::contains("failure"));
}

#[test]
fn run_passes_explicit_arguments_cwd_and_environment() {
    let root = Home::new();
    let cwd = root.path().join("working directory");
    std::fs::create_dir(&cwd).unwrap();
    harness::tendr(&root)
        .env("TENDR_TEST_DROP", "inherited")
        .args(["run", "args", "--cwd", cwd.to_str().unwrap(), "--env", "TENDR_TEST_SET=explicit", "--unset-env", "TENDR_TEST_DROP", "--", PYTHON, "-c", "import os,sys; print(repr(sys.argv[1:])); print(os.getcwd()); print(os.getenv('TENDR_TEST_SET')); print(os.getenv('TENDR_TEST_DROP', 'absent'))", "space here", "--child-flag"])
        .assert_within_deadline()
        .success()
        .stdout(predicate::str::contains("['space here', '--child-flag']"))
        .stdout(predicate::str::contains("working directory"))
        .stdout(predicate::str::contains("explicit"))
        .stdout(predicate::str::contains("absent"));
}

#[test]
fn run_clean_environment_passes_only_requested_variable() {
    let root = Home::new();
    harness::tendr(&root)
        .env("TENDR_TEST_PASS", "passed-value")
        .env("TENDR_TEST_OMIT", "hidden-value")
        .args(["run", "clean", "--env-mode", "clean", "--pass-env", "TENDR_TEST_PASS", "--", PYTHON, "-c", "import os; print(os.getenv('TENDR_TEST_PASS')); print(os.getenv('TENDR_TEST_OMIT', 'absent'))"])
        .assert_within_deadline()
        .success()
        .stdout(predicate::str::contains("passed-value"))
        .stdout(predicate::str::contains("absent"));
}

#[test]
fn run_replace_reruns_explicit_command() {
    let root = Home::new();
    for options in [vec![], vec!["--replace"]] {
        let mut cmd = harness::tendr(&root);
        cmd.args(["run", "repeat"])
            .args(options)
            .args(["--", PYTHON, "-c", "print('repeated')"]);
        cmd.assert_within_deadline()
            .success()
            .stdout(predicate::str::contains("repeated"));
    }
}

#[test]
fn run_requires_name_and_command_and_rejects_removed_flags() {
    let root = Home::new();
    for args in [
        vec!["run"],
        vec!["run", "name"],
        vec!["run", "name", "--detach", "--", PYTHON],
        vec!["run", "name", "--shell", "sh", "--", PYTHON],
    ] {
        harness::tendr(&root)
            .args(args)
            .assert_within_deadline()
            .code(2);
    }
}

#[test]
fn run_timeout_preserves_stop_exit_status() {
    let root = Home::new();
    harness::tendr(&root)
        .args([
            "run",
            "timeout",
            "--timeout",
            "1",
            "--",
            PYTHON,
            "-c",
            "import time; time.sleep(3600)",
        ])
        .assert_within_deadline()
        .code(124);
}

#[test]
fn remote_run_fallback_preserves_options_and_child_argv() {
    let root = Home::new();
    let output = harness::tendr(&root)
        .args([
            "--host",
            "example",
            "run",
            "quoted",
            "--env-mode",
            "clean",
            "--pass-env",
            "TOKEN",
            "--cwd",
            "/path with spaces",
            "--ready-when-output",
            "ready.*",
            "--",
            "program",
            "argument with spaces",
            "--child-flag",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    let fallback = stderr.split("try (POSIX shell):  ").nth(1).unwrap().trim();
    let ssh = shell_words::split(fallback).unwrap();
    assert_eq!(&ssh[..2], ["ssh", "example"]);
    let command = shell_words::split(&ssh[2]).unwrap();
    assert_eq!(
        command,
        [
            "tendr",
            "run",
            "quoted",
            "--cwd",
            "/path with spaces",
            "--pass-env",
            "TOKEN",
            "--env-mode",
            "clean",
            "--ready-when-output",
            "ready.*",
            "--",
            "program",
            "argument with spaces",
            "--child-flag",
        ]
    );
}
