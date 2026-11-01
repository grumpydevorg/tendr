//! Golden net for the local dispatch of the `--host`-capable commands (start,
//! status, list, log, push, kill, wait, watch, attach).
//!
//! These pin the stdout, stderr and exit codes that the typed
//! `RemoteOperation` layer must keep byte-for-byte: namespace routing per
//! command, the order in which arguments are validated, and every `start`
//! field reaching the launch spec. Other files pin each command's own
//! behaviour in depth.

mod harness;

use harness::{SessionGuard, tendr, wait_running_ns, wait_terminal_ns};
use std::sync::Mutex;
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

fn run(root: &TempDir, args: &[&str]) -> (i32, String, String) {
    let out = tendr(root)
        .args(args)
        .write_stdin("")
        .timeout(std::time::Duration::from_secs(60))
        .output()
        .expect("run tendr");
    (
        out.status.code().expect("exit code"),
        String::from_utf8(out.stdout).expect("utf-8 stdout"),
        String::from_utf8(out.stderr).expect("utf-8 stderr"),
    )
}

#[test]
fn missing_session_is_reported_per_command() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    for verb in ["status", "log", "push", "attach", "wait"] {
        let (code, stdout, stderr) = run(&root, &[verb, "ghost"]);
        assert_eq!(code, 1, "{verb}");
        assert_eq!(stdout, "", "{verb}");
        assert_eq!(stderr, "session not found: ghost\n", "{verb}");
    }

    // kill of a missing session is a successful, reported no-op.
    let (code, stdout, stderr) = run(&root, &["kill", "ghost"]);
    assert_eq!(code, 0);
    assert_eq!(stdout, "{\"result\":\"not_found\",\"session\":\"ghost\"}\n");
    assert_eq!(stderr, "");
}

#[test]
fn invalid_namespace_is_rejected_first_per_command() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // The namespace is checked before the session name or any other input.
    let cases: &[&[&str]] = &[
        &["status", "--namespace", "bad/ns", "bad/name"],
        &["list", "--namespace", "bad/ns"],
        &["log", "--namespace", "bad/ns", "bad/name"],
        &["push", "--namespace", "bad/ns", "bad/name"],
        &["kill", "--namespace", "bad/ns", "bad/name"],
        &["wait", "--namespace", "bad/ns", "bad/name"],
        &["watch", "--namespace", "bad/ns"],
        &["attach", "--namespace", "bad/ns", "bad/name"],
        &[
            "start",
            "--namespace",
            "bad/ns",
            "--env",
            "NOEQUALS",
            "bad/name",
            "--",
            "true",
        ],
    ];
    for args in cases {
        let (code, stdout, stderr) = run(&root, args);
        assert_eq!(code, 1, "{args:?}");
        assert_eq!(stdout, "", "{args:?}");
        assert_eq!(stderr, "namespace cannot contain '/'\n", "{args:?}");
    }
}

#[test]
fn start_rejects_malformed_env_before_launch() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let (code, stdout, stderr) = run(&root, &["start", "--env", "NOEQUALS", "x", "--", "true"]);
    assert_eq!(code, 1);
    assert_eq!(stdout, "");
    assert_eq!(
        stderr,
        "invalid --env format: expected KEY=VALUE, got: NOEQUALS\n"
    );
    let (code, _, stderr) = run(&root, &["status", "x"]);
    assert_eq!(code, 1, "no session was created");
    assert_eq!(stderr, "session not found: x\n");
}

#[test]
fn start_carries_every_launch_field_into_meta() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let cwd_str = cwd.path().to_str().unwrap();

    let (code, stdout, stderr) = run(
        &root,
        &[
            "start",
            "--namespace",
            "golden",
            "--stdin",
            "--exec-target",
            "python-repl",
            "--timeout",
            "60",
            "--cwd",
            cwd_str,
            "--env",
            "K=V=W",
            "--unset-env",
            "DROP_ME",
            "--on-exit",
            "true",
            "--boundary",
            "host:alpha",
            "--boundary-parent",
            "vm:beta",
            "--boundary-parent",
            "pod:gamma",
            "dep",
            "--",
            "sleep",
            "30",
        ],
    );
    let _dep = SessionGuard::new(&root, "dep");
    assert_eq!(code, 0, "{stderr}");
    let meta: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let spec = &meta["launch_spec"];
    assert_eq!(meta["session"], "dep");
    assert_eq!(spec["argv"], serde_json::json!(["sleep", "30"]));
    assert_eq!(spec["namespace"], "golden");
    assert_eq!(spec["stdin_mode"], "Pipe");
    assert_eq!(spec["io_mode"], "Pipe");
    assert_eq!(spec["exec_target"], "PythonRepl");
    assert_eq!(spec["timeout_s"], 60);
    assert_eq!(spec["cwd"], cwd_str);
    assert_eq!(spec["env"], serde_json::json!({"K": "V=W"}));
    assert_eq!(spec["unset_env"], serde_json::json!(["DROP_ME"]));
    assert_eq!(spec["on_exit"], serde_json::json!(["true"]));
    assert_eq!(
        spec["boundary"],
        serde_json::json!({
            "current": {"kind": "host", "label": "alpha"},
            "parents": [
                {"kind": "vm", "label": "beta"},
                {"kind": "pod", "label": "gamma"}
            ]
        })
    );
    wait_running_ns(&root, "golden", "dep");

    // --after binds the dependency's run id on this host; --any-exit rides along.
    let (code, stdout, stderr) = run(
        &root,
        &[
            "start",
            "--namespace",
            "golden",
            "--after",
            "dep",
            "--any-exit",
            "next",
            "--",
            "true",
        ],
    );
    let _next = SessionGuard::new(&root, "next");
    assert_eq!(code, 0, "{stderr}");
    let next: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        next["launch_spec"]["after"],
        serde_json::json!([{"session": "dep", "run_id": meta["run_id"]}])
    );
    assert_eq!(next["launch_spec"]["after_any_exit"], true);
    // exec target is inferred from argv[0] when not given.
    assert_eq!(next["launch_spec"]["exec_target"], "None");

    // --after resolves in the start's namespace, not the default one.
    let (code, stdout, stderr) = run(&root, &["start", "--after", "dep", "lost", "--", "true"]);
    assert_eq!(code, 1);
    assert_eq!(stdout, "");
    assert_eq!(stderr, "--after: session not found: dep\n");

    tendr(&root)
        .args(["kill", "--force", "--namespace", "golden", "dep"])
        .assert()
        .success();
    tendr(&root)
        .args(["kill", "--force", "--namespace", "golden", "next"])
        .assert()
        .success();
}

#[test]
fn namespace_routes_every_session_command() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
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

    // Without --namespace each command looks in `default` and misses.
    for verb in ["status", "log", "push", "attach", "wait"] {
        let (code, _, stderr) = run(&root, &[verb, "gold"]);
        assert_eq!(code, 1, "{verb}");
        assert_eq!(stderr, "session not found: gold\n", "{verb}");
    }

    let (code, stdout, _) = run(&root, &["status", "--namespace", "golden", "gold"]);
    assert_eq!(code, 0);
    let status: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(status["session"], "gold");
    assert_eq!(status["status"], "Exited");

    let (code, stdout, stderr) = run(&root, &["log", "--namespace", "golden", "--raw", "gold"]);
    assert_eq!(
        (code, stdout.as_str(), stderr.as_str()),
        (0, "golden-line\n", "")
    );
    let (code, stdout, _) = run(
        &root,
        &[
            "log",
            "--namespace",
            "golden",
            "--raw",
            "--tail",
            "1",
            "gold",
        ],
    );
    assert_eq!((code, stdout.as_str()), (0, "golden-line\n"));
    let (code, stdout, _) = run(
        &root,
        &[
            "log",
            "--namespace",
            "golden",
            "--since",
            "9999999999",
            "gold",
        ],
    );
    assert_eq!((code, stdout.as_str()), (0, ""));

    let (code, stdout, _) = run(&root, &["list", "--namespace", "golden"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("\"gold\""), "{stdout}");
    let (code, stdout, _) = run(&root, &["list", "--namespace", "elsewhere"]);
    assert_eq!(code, 0);
    assert!(!stdout.contains("\"gold\""), "{stdout}");

    let (code, stdout, _) = run(&root, &["wait", "--namespace", "golden", "--any", "gold"]);
    assert_eq!(code, 0);
    assert!(stdout.starts_with('['), "{stdout}");

    // A session that never had a PTY cannot be attached, wherever it lives.
    let (code, stdout, stderr) = run(&root, &["attach", "--namespace", "golden", "gold"]);
    assert_eq!(
        (code, stdout.as_str(), stderr.as_str()),
        (1, "", "session is not PTY-enabled\n")
    );

    // push to an ended session without stdin fails without side effect.
    let (code, stdout, _) = run(&root, &["push", "--namespace", "golden", "gold"]);
    assert_eq!((code, stdout.as_str()), (1, ""));
}
