mod harness;

use harness::{tendr, wait_running_ns, wait_terminal, wait_terminal_ns};
use std::sync::Mutex;
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

#[test]
fn start_in_explicit_namespace() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let out = tendr(&root)
        .args([
            "start",
            "ns-echo",
            "--namespace",
            "myns",
            "--",
            "echo",
            "hi",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    wait_terminal_ns(&root, "myns", "ns-echo");

    let meta_path = root.path().join(".tendr/sessions/myns/ns-echo/meta.json");
    assert!(
        meta_path.exists(),
        "meta.json should exist at namespace path: {meta_path:?}"
    );
}

#[test]
fn same_name_different_namespace() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Start "job" in ns-a
    let out_a = tendr(&root)
        .args(["start", "job", "--namespace", "ns-a", "--", "sleep", "60"])
        .output()
        .unwrap();
    assert!(
        out_a.status.success(),
        "start in ns-a failed: {}",
        String::from_utf8_lossy(&out_a.stderr)
    );
    wait_running_ns(&root, "ns-a", "job");

    // Start "job" in ns-b — should succeed because different namespace
    let out_b = tendr(&root)
        .args(["start", "job", "--namespace", "ns-b", "--", "sleep", "60"])
        .output()
        .unwrap();
    assert!(
        out_b.status.success(),
        "start in ns-b failed: {}",
        String::from_utf8_lossy(&out_b.stderr)
    );
    wait_running_ns(&root, "ns-b", "job");

    // Verify status works for each
    let status_a = tendr(&root)
        .args(["status", "job", "--namespace", "ns-a"])
        .output()
        .unwrap();
    assert!(status_a.status.success(), "status for ns-a/job failed");

    let status_b = tendr(&root)
        .args(["status", "job", "--namespace", "ns-b"])
        .output()
        .unwrap();
    assert!(status_b.status.success(), "status for ns-b/job failed");

    // Cleanup both
    tendr(&root)
        .args(["kill", "--force", "job", "--namespace", "ns-a"])
        .output()
        .unwrap();
    tendr(&root)
        .args(["kill", "--force", "job", "--namespace", "ns-b"])
        .output()
        .unwrap();
    wait_terminal_ns(&root, "ns-a", "job");
    wait_terminal_ns(&root, "ns-b", "job");
}

#[test]
fn list_with_namespace_filters() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Create sessions in two namespaces
    tendr(&root)
        .args(["start", "alpha", "--namespace", "ns-a", "--", "sleep", "60"])
        .assert()
        .success();
    wait_running_ns(&root, "ns-a", "alpha");

    tendr(&root)
        .args(["start", "beta", "--namespace", "ns-b", "--", "sleep", "60"])
        .assert()
        .success();
    wait_running_ns(&root, "ns-b", "beta");

    // list --namespace ns-a should only show ns-a sessions
    let list_out = tendr(&root)
        .args(["list", "--namespace", "ns-a"])
        .output()
        .unwrap();
    assert!(list_out.status.success(), "list --namespace ns-a failed");

    let entries: Vec<serde_json::Value> =
        serde_json::from_slice(&list_out.stdout).expect("list output not JSON");

    assert_eq!(entries.len(), 1, "should have exactly 1 session in ns-a");
    assert_eq!(entries[0]["namespace"], "ns-a");
    assert_eq!(entries[0]["name"], "alpha");

    // Cleanup
    tendr(&root)
        .args(["kill", "--force", "alpha", "--namespace", "ns-a"])
        .output()
        .unwrap();
    tendr(&root)
        .args(["kill", "--force", "beta", "--namespace", "ns-b"])
        .output()
        .unwrap();
    wait_terminal_ns(&root, "ns-a", "alpha");
    wait_terminal_ns(&root, "ns-b", "beta");
}

#[test]
fn list_without_namespace_returns_all() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Create sessions in two namespaces
    tendr(&root)
        .args(["start", "one", "--namespace", "ns-a", "--", "sleep", "60"])
        .assert()
        .success();
    wait_running_ns(&root, "ns-a", "one");

    tendr(&root)
        .args(["start", "two", "--namespace", "ns-b", "--", "sleep", "60"])
        .assert()
        .success();
    wait_running_ns(&root, "ns-b", "two");

    // list without --namespace should return all
    let list_out = tendr(&root).args(["list"]).output().unwrap();
    assert!(list_out.status.success(), "list (all) failed");

    let entries: Vec<serde_json::Value> =
        serde_json::from_slice(&list_out.stdout).expect("list output not JSON");

    assert!(
        entries.len() >= 2,
        "should have at least 2 sessions, got {}",
        entries.len()
    );

    // Verify namespace field is present on all entries
    for entry in &entries {
        assert!(
            entry["namespace"].is_string(),
            "each entry should have a namespace field"
        );
    }

    // Verify both namespaces are represented
    let namespaces: Vec<&str> = entries
        .iter()
        .filter_map(|e| e["namespace"].as_str())
        .collect();
    assert!(
        namespaces.contains(&"ns-a"),
        "should contain ns-a, got: {namespaces:?}"
    );
    assert!(
        namespaces.contains(&"ns-b"),
        "should contain ns-b, got: {namespaces:?}"
    );

    // Cleanup
    tendr(&root)
        .args(["kill", "--force", "one", "--namespace", "ns-a"])
        .output()
        .unwrap();
    tendr(&root)
        .args(["kill", "--force", "two", "--namespace", "ns-b"])
        .output()
        .unwrap();
    wait_terminal_ns(&root, "ns-a", "one");
    wait_terminal_ns(&root, "ns-b", "two");
}

#[test]
fn start_with_namespace_idempotent() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let out1 = tendr(&root)
        .args([
            "start",
            "idem-ns",
            "--namespace",
            "myns",
            "--",
            "sleep",
            "60",
        ])
        .output()
        .unwrap();
    assert!(
        out1.status.success(),
        "first start failed: {}",
        String::from_utf8_lossy(&out1.stderr)
    );
    wait_running_ns(&root, "myns", "idem-ns");

    let meta1: serde_json::Value =
        serde_json::from_slice(&out1.stdout).expect("first output not JSON");
    let run_id1 = meta1["run_id"].as_str().expect("no run_id in first output");

    // Same name + same spec + same namespace = idempotent
    let out2 = tendr(&root)
        .args([
            "start",
            "idem-ns",
            "--namespace",
            "myns",
            "--",
            "sleep",
            "60",
        ])
        .output()
        .unwrap();
    assert!(
        out2.status.success(),
        "second start should succeed (idempotent): {}",
        String::from_utf8_lossy(&out2.stderr)
    );

    let meta2: serde_json::Value =
        serde_json::from_slice(&out2.stdout).expect("second output not JSON");
    let run_id2 = meta2["run_id"]
        .as_str()
        .expect("no run_id in second output");

    assert_eq!(
        run_id1, run_id2,
        "idempotent start should return same run_id"
    );

    // Cleanup
    tendr(&root)
        .args(["kill", "--force", "idem-ns", "--namespace", "myns"])
        .output()
        .unwrap();
    wait_terminal_ns(&root, "myns", "idem-ns");
}

#[test]
fn default_namespace_used_when_omitted() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let out = tendr(&root)
        .args(["start", "no-ns", "--", "echo", "hi"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Use the default-namespace harness helper
    wait_terminal(&root, "no-ns");

    // Verify session created under .tendr/sessions/default/
    let meta_path = root.path().join(".tendr/sessions/default/no-ns/meta.json");
    assert!(
        meta_path.exists(),
        "session without --namespace should be under 'default': {meta_path:?}"
    );
}

#[test]
fn push_resolves_session_in_namespace() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Start a session with stdin in a non-default namespace
    tendr(&root)
        .args([
            "start",
            "push-ns",
            "--namespace",
            "ns-push",
            "--stdin",
            "--",
            "cat",
        ])
        .assert()
        .success();
    wait_running_ns(&root, "ns-push", "push-ns");

    // Push data via the correct namespace
    let push_out = tendr(&root)
        .args(["push", "push-ns", "--namespace", "ns-push"])
        .write_stdin("hello from push\n")
        .output()
        .unwrap();
    assert!(
        push_out.status.success(),
        "push should succeed in non-default namespace: {}",
        String::from_utf8_lossy(&push_out.stderr)
    );

    // `push` returns once the bytes are handed to the child's stdin, not once
    // `cat` has echoed them into the log. Wait for the echo before killing, or a
    // force-kill can land while the line is still in flight.
    harness::poll_until(
        std::time::Duration::from_secs(10),
        std::time::Duration::from_millis(50),
        || {
            let log_out = tendr(&root)
                .args(["log", "push-ns", "--namespace", "ns-push", "--raw"])
                .output()
                .unwrap();
            let log = String::from_utf8_lossy(&log_out.stdout).into_owned();
            if log.contains("hello from push") {
                harness::Observation::Ready(())
            } else {
                harness::Observation::Pending(format!("log so far: {log:?}"))
            }
        },
    )
    .unwrap_or_else(|e| panic!("pushed data should appear in log: {e}"));

    tendr(&root)
        .args(["kill", "--force", "push-ns", "--namespace", "ns-push"])
        .output()
        .unwrap();
    wait_terminal_ns(&root, "ns-push", "push-ns");
}

#[test]
fn log_resolves_session_in_namespace() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args([
            "start",
            "log-ns",
            "--namespace",
            "ns-log",
            "--",
            "echo",
            "namespace-log-output",
        ])
        .assert()
        .success();
    wait_terminal_ns(&root, "ns-log", "log-ns");

    let log_out = tendr(&root)
        .args(["log", "log-ns", "--namespace", "ns-log", "--raw"])
        .output()
        .unwrap();
    assert!(
        log_out.status.success(),
        "log should succeed in non-default namespace: {}",
        String::from_utf8_lossy(&log_out.stderr)
    );
    let log = String::from_utf8_lossy(&log_out.stdout);
    assert!(
        log.contains("namespace-log-output"),
        "log should contain child output, got: {log}"
    );
}

#[test]
fn wait_resolves_session_in_namespace() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args([
            "start",
            "wait-ns",
            "--namespace",
            "ns-wait",
            "--",
            "echo",
            "done",
        ])
        .assert()
        .success();

    let wait_out = tendr(&root)
        .args([
            "wait",
            "wait-ns",
            "--namespace",
            "ns-wait",
            "--timeout",
            "5",
        ])
        .output()
        .unwrap();
    assert!(
        wait_out.status.success(),
        "wait should succeed in non-default namespace: {}",
        String::from_utf8_lossy(&wait_out.stderr)
    );
}
