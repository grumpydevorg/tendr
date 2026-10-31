mod harness;

use harness::{tendr, wait_running_ns, wait_terminal};
// Command/Stdio remain only for the cfg(unix) SIGTERM test below.
#[cfg(unix)]
use std::process::{Command, Stdio};
use std::sync::Mutex;
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

/// Start a session that runs `sleep 60`, wait for Running state.
fn create_running_session(root: &TempDir, name: &str, namespace: &str) {
    let out = tendr(root)
        .args(["start", name, "--namespace", namespace, "--", "sleep", "60"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    wait_running_ns(root, namespace, name);
}

/// Read the run_id from a session's meta.json.
fn read_run_id(root: &TempDir, namespace: &str, session: &str) -> String {
    let path = root
        .path()
        .join(format!(".tendr/sessions/{namespace}/{session}/meta.json"));
    let content = std::fs::read_to_string(&path).unwrap();
    let meta: serde_json::Value = serde_json::from_str(&content).unwrap();
    meta["run_id"].as_str().unwrap().to_owned()
}

/// Read output.log and return annotation payloads.
fn read_annotation_lines(root: &TempDir, namespace: &str, session: &str) -> Vec<serde_json::Value> {
    let path = root
        .path()
        .join(format!(".tendr/sessions/{namespace}/{session}/output.log"));
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    content
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|line| {
            if line["tag"] == "A" {
                Some(line["content"].clone())
            } else {
                None
            }
        })
        .collect()
}

#[test]
fn wrap_child_inherits_tendr_env() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Start a session that prints its env
    let out = tendr(&root)
        .args([
            "start",
            "wrap-env-test",
            "--namespace",
            "default",
            "--",
            "env",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    wait_terminal(&root, "wrap-env-test");

    let log_path = root
        .path()
        .join(".tendr/sessions/default/wrap-env-test/output.log");
    let content = std::fs::read_to_string(&log_path).unwrap();

    assert!(
        content.contains("TENDR_RUN_ID="),
        "child should see TENDR_RUN_ID"
    );
    assert!(
        content.contains("TENDR_SESSION=wrap-env-test"),
        "child should see TENDR_SESSION"
    );
    assert!(
        content.contains("TENDR_NAMESPACE=default"),
        "child should see TENDR_NAMESPACE"
    );
}

#[test]
fn wrap_preserves_child_exit_code() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_running_session(&root, "wrap-exit", "default");
    let run_id = read_run_id(&root, "default", "wrap-exit");

    let out = tendr(&root)
        .env("TENDR_RUN_ID", &run_id)
        .args([
            "wrap",
            "--session",
            "wrap-exit",
            "--source",
            "test.src",
            "--event",
            "test-event",
            "--",
            "sh",
            "-c",
            "exit 42",
        ])
        .output()
        .unwrap();

    assert_eq!(
        out.status.code(),
        Some(42),
        "wrap should exit with child's exit code"
    );

    // Cleanup
    tendr(&root)
        .args(["kill", "--force", "wrap-exit"])
        .output()
        .unwrap();
    wait_terminal(&root, "wrap-exit");
}

#[test]
fn wrap_captures_and_replays_stdout() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_running_session(&root, "wrap-stdout", "default");
    let run_id = read_run_id(&root, "default", "wrap-stdout");

    let out = tendr(&root)
        .env("TENDR_RUN_ID", &run_id)
        .args([
            "wrap",
            "--session",
            "wrap-stdout",
            "--source",
            "test.src",
            "--event",
            "test-event",
            "--",
            "echo",
            "hello-from-wrap",
        ])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("hello-from-wrap"),
        "wrap stdout should contain child output, got: {stdout}"
    );

    // Also check annotation was written
    let ann_lines = read_annotation_lines(&root, "default", "wrap-stdout");
    assert_eq!(ann_lines.len(), 1, "should have one annotation line");
    let ann = &ann_lines[0];
    assert_eq!(ann["source"], "test.src");
    assert_eq!(ann["event"], "test-event");
    assert_eq!(ann["data"]["hook_exit_code"], 0);

    // Cleanup
    tendr(&root)
        .args(["kill", "--force", "wrap-stdout"])
        .output()
        .unwrap();
    wait_terminal(&root, "wrap-stdout");
}

#[test]
fn wrap_writes_annotation_line() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_running_session(&root, "wrap-ann", "default");
    let run_id = read_run_id(&root, "default", "wrap-ann");

    let out = tendr(&root)
        .env("TENDR_RUN_ID", &run_id)
        .args([
            "wrap",
            "--session",
            "wrap-ann",
            "--source",
            "cmux.claude-hook",
            "--event",
            "pre-tool-use",
            "--",
            "echo",
            "ok",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());

    let ann_lines = read_annotation_lines(&root, "default", "wrap-ann");
    assert_eq!(ann_lines.len(), 1);

    let ann = &ann_lines[0];
    assert_eq!(ann["source"], "cmux.claude-hook");
    assert_eq!(ann["event"], "pre-tool-use");
    assert_eq!(ann["run_id"], run_id);
    assert_eq!(ann["data"]["truncated"], false);

    // Cleanup
    tendr(&root)
        .args(["kill", "--force", "wrap-ann"])
        .output()
        .unwrap();
    wait_terminal(&root, "wrap-ann");
}

#[test]
fn wrap_fails_without_run_id() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Don't set TENDR_RUN_ID
    let out = tendr(&root)
        .env_remove("TENDR_RUN_ID")
        .args([
            "wrap",
            "--session",
            "nonexistent",
            "--source",
            "test.src",
            "--event",
            "test",
            "--",
            "echo",
            "hi",
        ])
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "wrap should fail without TENDR_RUN_ID"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("TENDR_RUN_ID"),
        "error should mention TENDR_RUN_ID, got: {stderr}"
    );
}

#[test]
fn wrap_fails_for_nonexistent_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let out = tendr(&root)
        .env("TENDR_RUN_ID", "fake-run-id")
        .args([
            "wrap",
            "--session",
            "no-such-session",
            "--source",
            "test.src",
            "--event",
            "test",
            "--",
            "echo",
            "hi",
        ])
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "wrap should fail for nonexistent session"
    );
}

#[test]
fn source_rejects_tendr_prefix() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let out = tendr(&root)
        .env("TENDR_RUN_ID", "fake-run-id")
        .args([
            "wrap",
            "--session",
            "any",
            "--source",
            "tendr.sidecar",
            "--event",
            "test",
            "--",
            "echo",
            "hi",
        ])
        .output()
        .unwrap();

    assert!(!out.status.success(), "wrap should reject tendr.* source");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("tendr."),
        "error should mention reserved prefix, got: {stderr}"
    );
}

#[test]
fn wrap_defaults_session_from_env() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_running_session(&root, "wrap-env-default", "myns");
    let run_id = read_run_id(&root, "myns", "wrap-env-default");

    // Don't pass --session or --namespace, rely on env vars
    let out = tendr(&root)
        .env("TENDR_RUN_ID", &run_id)
        .env("TENDR_SESSION", "wrap-env-default")
        .env("TENDR_NAMESPACE", "myns")
        .args([
            "wrap",
            "--source",
            "test.src",
            "--event",
            "env-default",
            "--",
            "echo",
            "from-env",
        ])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "wrap should succeed with env defaults: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let ann_lines = read_annotation_lines(&root, "myns", "wrap-env-default");
    assert_eq!(ann_lines.len(), 1, "annotation should exist");

    // Cleanup
    tendr(&root)
        .args(["kill", "--force", "wrap-env-default", "--namespace", "myns"])
        .output()
        .unwrap();
    let _ = harness::poll_terminal_ns(&root, "myns", "wrap-env-default");
}

#[test]
fn wrap_truncates_large_payload() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_running_session(&root, "wrap-trunc", "default");
    let run_id = read_run_id(&root, "default", "wrap-trunc");

    // Generate a large stdin payload (>4096 bytes)
    let large_input = "x".repeat(5000);

    let out = tendr(&root)
        .env("TENDR_RUN_ID", &run_id)
        .args([
            "wrap",
            "--session",
            "wrap-trunc",
            "--source",
            "test.src",
            "--event",
            "big-event",
            "--",
            "cat",
        ])
        .write_stdin(large_input.as_bytes())
        .output()
        .unwrap();
    assert!(out.status.success());

    let ann_lines = read_annotation_lines(&root, "default", "wrap-trunc");
    assert_eq!(ann_lines.len(), 1);

    // Verify line is within limit
    let ann = &ann_lines[0];
    let wrapped = serde_json::json!({
        "ts": 0.0,
        "tag": "A",
        "content": ann.clone(),
    });
    let line = serde_json::to_string(&wrapped).unwrap();
    assert!(
        line.len() < 4096,
        "annotation line should fit within the line cap, got {}",
        line.len()
    );
    assert_eq!(ann["data"]["truncated"], true, "should be marked truncated");

    // Cleanup
    tendr(&root)
        .args(["kill", "--force", "wrap-trunc"])
        .output()
        .unwrap();
    wait_terminal(&root, "wrap-trunc");
}

#[test]
fn wrap_passes_stdin_to_child() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_running_session(&root, "wrap-stdin", "default");
    let run_id = read_run_id(&root, "default", "wrap-stdin");

    let input = r#"{"tool":"bash","input":"ls"}"#;

    let out = tendr(&root)
        .env("TENDR_RUN_ID", &run_id)
        .args([
            "wrap",
            "--session",
            "wrap-stdin",
            "--source",
            "test.src",
            "--event",
            "stdin-test",
            "--",
            "cat",
        ])
        .write_stdin(input.as_bytes())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.trim(),
        input,
        "child should receive and echo the stdin blob"
    );

    // Verify annotation recorded the stdin
    let ann_lines = read_annotation_lines(&root, "default", "wrap-stdin");
    assert_eq!(ann_lines.len(), 1);
    let ann = &ann_lines[0];
    // hook_stdin should be parsed as JSON since input is valid JSON
    assert_eq!(ann["data"]["hook_stdin"]["tool"], "bash");

    // Cleanup
    tendr(&root)
        .args(["kill", "--force", "wrap-stdin"])
        .output()
        .unwrap();
    wait_terminal(&root, "wrap-stdin");
}

#[test]
fn wrap_annotation_visible_in_watch() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_running_session(&root, "wrap-watch", "default");
    let run_id = read_run_id(&root, "default", "wrap-watch");

    // Wait for the watcher's baseline before mutating, then read until the exact
    // annotation surfaces — replaces the fixed 300 ms baseline + 1 s delivery
    // sleeps with the ReadyFollower handshake.
    let follower = harness::ReadyFollower::spawn(
        &root,
        "watch",
        &["--namespace", "default", "--annotations", "--from-now"],
    );

    let out = tendr(&root)
        .env("TENDR_RUN_ID", &run_id)
        .args([
            "wrap",
            "--session",
            "wrap-watch",
            "--source",
            "cmux.claude-hook",
            "--event",
            "pre-tool-use",
            "--",
            "echo",
            "watch-visible",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());

    follower.read_until(std::time::Duration::from_secs(10), |event| {
        event["kind"] == "annotation"
            && event["session"] == "wrap-watch"
            && event["source"] == "cmux.claude-hook"
            && event["name"] == "annotation.pre-tool-use"
            && event["data"]["event"] == "pre-tool-use"
    });
    follower.stop();

    tendr(&root)
        .args(["kill", "--force", "wrap-watch"])
        .output()
        .unwrap();
    wait_terminal(&root, "wrap-watch");
}

#[cfg(unix)]
#[test]
fn wrap_forwards_sigterm_and_writes_annotation() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_running_session(&root, "wrap-sigterm", "default");
    let run_id = read_run_id(&root, "default", "wrap-sigterm");

    // The shell installs its TERM trap and *then* publishes an out-of-band
    // ready-file, so the file's existence proves a SIGTERM sent from now on will
    // be caught. It cannot signal readiness on stdout: `wrap` read_to_end's the
    // child's output and only replays it after the child exits, so a READY line
    // would not be observable until the very exit we are trying to trigger.
    let ready = root.path().join("sigterm-trap-ready");
    let script = format!(
        "trap 'exit 0' TERM; : > '{}'; while :; do sleep 1; done",
        ready.display()
    );

    let bin = assert_cmd::cargo::cargo_bin("tendr");
    let mut wrap_child = Command::new(bin)
        .env("HOME", root.path())
        .env("TENDR_RUN_ID", &run_id)
        .args([
            "wrap",
            "--session",
            "wrap-sigterm",
            "--source",
            "test.src",
            "--event",
            "sigterm-test",
            "--",
            "sh",
            "-c",
            script.as_str(),
        ])
        // `wrap` reads its stdin to EOF before spawning the child, so an
        // inherited stdin that stays open (a pipe or socket from the shell
        // running `cargo test`) would hold the child back past the deadline.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn tendr wrap");

    // No sleep: the trap is proven installed once the ready-file appears.
    harness::wait_ready_file(&ready, std::time::Duration::from_secs(10));

    unsafe {
        libc::kill(wrap_child.id() as i32, libc::SIGTERM);
    }

    let status = harness::poll_child_exit(
        &mut wrap_child,
        std::time::Duration::from_secs(8),
        std::time::Duration::from_millis(50),
    )
    .unwrap_or_else(|e| {
        let _ = wrap_child.kill();
        panic!("wrap did not exit after SIGTERM: {e}");
    });
    assert!(
        status.success(),
        "wrap should exit cleanly after forwarding SIGTERM, got: {status:?}"
    );

    let ann_lines = read_annotation_lines(&root, "default", "wrap-sigterm");
    assert_eq!(ann_lines.len(), 1, "should write an annotation on SIGTERM");
    let ann = &ann_lines[0];
    assert_eq!(ann["event"], "sigterm-test");
    assert_eq!(ann["data"]["hook_exit_code"], 0);

    tendr(&root)
        .args(["kill", "--force", "wrap-sigterm"])
        .output()
        .unwrap();
    wait_terminal(&root, "wrap-sigterm");
}

// --- Slice 3: dual-write — stored event + A-line linked by event_id (plan scope 3) ---

/// wrap with a valid event kind dual-writes: the stored event (spec
/// example (b) data shape) and an A-line carrying event_id/block_id.
#[test]
fn wrap_dual_writes_event_and_linked_aline() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_running_session(&root, "wrap-dual", "default");
    let run_id = read_run_id(&root, "default", "wrap-dual");

    let out = tendr(&root)
        .env("TENDR_RUN_ID", &run_id)
        .env("TENDR_GENERATION", "1")
        .args([
            "wrap",
            "--session",
            "wrap-dual",
            "--source",
            "claude.hook",
            "--event",
            "hook.post_tool_use",
            "--",
            "echo",
            "ok",
        ])
        .write_stdin(r#"{"tool_name":"Bash"}"#)
        .output()
        .unwrap();
    assert!(out.status.success());

    let events = harness::read_events(&root, "wrap-dual");
    let hook = events
        .iter()
        .find(|e| e["kind"] == "hook.post_tool_use")
        .expect("stored hook event");
    assert_eq!(hook["source"], "claude.hook");
    assert_eq!(hook["run_id"].as_str().unwrap(), run_id);
    assert_eq!(hook["gen"], 1);
    assert_eq!(hook["data"]["hook_stdin"]["tool_name"], "Bash");
    assert_eq!(hook["data"]["hook_stdout"], "ok\n");
    assert_eq!(hook["data"]["hook_exit_code"], 0);
    assert_eq!(hook["data"]["command"], serde_json::json!(["echo", "ok"]));
    assert_eq!(hook["data"]["truncated"], false);
    let block = hook["block_id"].as_str().expect("event carries its block");

    let ann_lines = read_annotation_lines(&root, "default", "wrap-dual");
    assert_eq!(ann_lines.len(), 1);
    let ann = &ann_lines[0];
    assert_eq!(ann["event_id"], hook["id"], "A-line links the stored event");
    assert_eq!(ann["block_id"].as_str().unwrap(), block);
    assert_eq!(ann["source"], "claude.hook", "legacy A-line shape intact");
    assert_eq!(ann["data"]["hook_exit_code"], 0);

    tendr(&root)
        .args(["kill", "--force", "wrap-dual"])
        .output()
        .unwrap();
    wait_terminal(&root, "wrap-dual");
}

/// The child sees TENDR_BLOCK_ID (wrap's block) and TENDR_PARENT_EVENT_ID
/// — the id of the event wrap WILL write (pre-minted, spec §2).
#[test]
fn wrap_sets_child_env_chain() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_running_session(&root, "wrap-env", "default");
    let run_id = read_run_id(&root, "default", "wrap-env");

    let out = tendr(&root)
        .env("TENDR_RUN_ID", &run_id)
        .args([
            "wrap",
            "--session",
            "wrap-env",
            "--source",
            "test.src",
            "--event",
            "hook.env_probe",
            "--",
            "sh",
            "-c",
            "printenv TENDR_BLOCK_ID; printenv TENDR_PARENT_EVENT_ID",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());

    let events = harness::read_events(&root, "wrap-env");
    let hook = events
        .iter()
        .find(|e| e["kind"] == "hook.env_probe")
        .expect("stored event");
    let seen: Vec<&str> = hook["data"]["hook_stdout"]
        .as_str()
        .unwrap()
        .lines()
        .collect();
    assert_eq!(seen.len(), 2, "child saw both vars");
    assert_eq!(seen[0], hook["block_id"].as_str().unwrap());
    assert_eq!(
        seen[1],
        hook["id"].as_str().unwrap(),
        "TENDR_PARENT_EVENT_ID names the event wrap wrote"
    );

    tendr(&root)
        .args(["kill", "--force", "wrap-env"])
        .output()
        .unwrap();
    wait_terminal(&root, "wrap-env");
}

/// A hook-spawned emit chains to the wrap event automatically — the causal
/// tree needs no flags anywhere (spec §2).
#[test]
fn wrap_child_emit_chains_to_hook_event() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_running_session(&root, "wrap-chain", "default");
    let run_id = read_run_id(&root, "default", "wrap-chain");

    let tendr_bin = assert_cmd::cargo::cargo_bin("tendr");
    let out = tendr(&root)
        .env("TENDR_RUN_ID", &run_id)
        .env("TENDR_SESSION", "wrap-chain")
        .env("TENDR_NAMESPACE", "default")
        .args([
            "wrap",
            "--session",
            "wrap-chain",
            "--source",
            "claude.hook",
            "--event",
            "hook.post_tool_use",
            "--",
            "sh",
            "-c",
            &format!(
                "{} emit --kind hook.note --data '{{\"note\":1}}' --best-effort",
                shell_words::quote(tendr_bin.to_str().unwrap())
            ),
        ])
        .output()
        .unwrap();
    assert!(out.status.success());

    let events = harness::read_events(&root, "wrap-chain");
    let hook = events
        .iter()
        .find(|e| e["kind"] == "hook.post_tool_use")
        .expect("hook event");
    let note = events
        .iter()
        .find(|e| e["kind"] == "hook.note")
        .expect("child emit stored");
    assert_eq!(
        note["parent_id"], hook["id"],
        "child event chains to the hook event"
    );
    assert_eq!(
        note["block_id"], hook["block_id"],
        "child event lands in wrap's block"
    );

    tendr(&root)
        .args(["kill", "--force", "wrap-chain"])
        .output()
        .unwrap();
    wait_terminal(&root, "wrap-chain");
}

/// A reserved --event is argument validation: exit 6 before any side
/// effect — child not spawned, nothing written anywhere.
#[test]
fn wrap_reserved_event_exits_6_without_side_effects() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_running_session(&root, "wrap-res", "default");
    let run_id = read_run_id(&root, "default", "wrap-res");

    let marker = root.path().join("side-effect-marker");
    let out = tendr(&root)
        .env("TENDR_RUN_ID", &run_id)
        .args([
            "wrap",
            "--session",
            "wrap-res",
            "--source",
            "test.src",
            "--event",
            "run.hijack",
            "--",
            "touch",
            marker.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(6));
    assert!(!marker.exists(), "child must not have run");
    assert!(
        read_annotation_lines(&root, "default", "wrap-res").is_empty(),
        "no A-line"
    );
    let events = harness::read_events(&root, "wrap-res");
    assert!(
        !events.iter().any(|e| e["kind"] == "run.hijack"),
        "no stored event"
    );

    tendr(&root)
        .args(["kill", "--force", "wrap-res"])
        .output()
        .unwrap();
    wait_terminal(&root, "wrap-res");
}

/// A legacy dotless --event keeps the exact pre-slice-3 behavior: A-line
/// only, no stored event, no env chain for the child, child exit intact.
#[test]
fn wrap_legacy_dotless_event_keeps_aline_only() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_running_session(&root, "wrap-legacy", "default");
    let run_id = read_run_id(&root, "default", "wrap-legacy");

    let out = tendr(&root)
        .env("TENDR_RUN_ID", &run_id)
        .args([
            "wrap",
            "--session",
            "wrap-legacy",
            "--source",
            "test.src",
            "--event",
            "pre-tool-use",
            "--",
            "sh",
            "-c",
            "printenv TENDR_BLOCK_ID || echo no-block-env",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("annotation only"),
        "legacy path says what it does"
    );

    let ann_lines = read_annotation_lines(&root, "default", "wrap-legacy");
    assert_eq!(ann_lines.len(), 1);
    let ann = &ann_lines[0];
    assert_eq!(ann["event"], "pre-tool-use");
    assert!(ann.get("event_id").is_none(), "no linkage without an event");
    assert!(ann.get("block_id").is_none());
    assert!(
        ann["data"]["hook_stdout"]
            .as_str()
            .unwrap()
            .contains("no-block-env"),
        "legacy path sets no env chain"
    );

    let events = harness::read_events(&root, "wrap-legacy");
    assert!(
        events.iter().all(|e| e["source"] != "test.src"),
        "no stored event on the legacy path"
    );

    tendr(&root)
        .args(["kill", "--force", "wrap-legacy"])
        .output()
        .unwrap();
    wait_terminal(&root, "wrap-legacy");
}

/// wrap inside an outer block chains its event upward via parent_id.
#[test]
fn wrap_chains_parent_from_ambient_env() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_running_session(&root, "wrap-outer", "default");
    let run_id = read_run_id(&root, "default", "wrap-outer");

    let outer = uuid::Uuid::now_v7().to_string();
    tendr(&root)
        .env("TENDR_RUN_ID", &run_id)
        .env("TENDR_BLOCK_ID", &outer)
        .args([
            "wrap",
            "--session",
            "wrap-outer",
            "--source",
            "test.src",
            "--event",
            "hook.nested",
            "--",
            "true",
        ])
        .output()
        .unwrap();

    let events = harness::read_events(&root, "wrap-outer");
    let hook = events.iter().find(|e| e["kind"] == "hook.nested").unwrap();
    assert_eq!(hook["parent_id"].as_str().unwrap(), outer);
    assert_ne!(hook["block_id"].as_str().unwrap(), outer, "fresh block");

    tendr(&root)
        .args(["kill", "--force", "wrap-outer"])
        .output()
        .unwrap();
    wait_terminal(&root, "wrap-outer");
}

/// Event append failure is best-effort: the child exit code passes through
/// and the A-line is still written — without an event_id link.
#[cfg(unix)]
#[test]
fn wrap_event_append_failure_is_best_effort() {
    use std::os::unix::fs::PermissionsExt;

    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_running_session(&root, "wrap-ro", "default");
    let run_id = read_run_id(&root, "default", "wrap-ro");

    let events_dir = root.path().join(".tendr/sessions/default/wrap-ro/events");
    std::fs::set_permissions(&events_dir, std::fs::Permissions::from_mode(0o000)).unwrap();

    let out = tendr(&root)
        .env("TENDR_RUN_ID", &run_id)
        .args([
            "wrap",
            "--session",
            "wrap-ro",
            "--source",
            "test.src",
            "--event",
            "hook.roevents",
            "--",
            "sh",
            "-c",
            "exit 7",
        ])
        .output()
        .unwrap();

    std::fs::set_permissions(&events_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(out.status.code(), Some(7), "child exit passes through");
    let ann_lines = read_annotation_lines(&root, "default", "wrap-ro");
    assert_eq!(ann_lines.len(), 1, "A-line still written");
    assert!(
        ann_lines[0].get("event_id").is_none(),
        "no event_id when the event never landed"
    );
    assert!(
        ann_lines[0].get("block_id").is_some(),
        "block context still recorded"
    );

    tendr(&root)
        .args(["kill", "--force", "wrap-ro"])
        .output()
        .unwrap();
    wait_terminal(&root, "wrap-ro");
}
