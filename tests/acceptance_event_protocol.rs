//! End-to-end acceptance tests for docs/architecture/07-event-protocol.md.
//! Companion coverage: cli_events_lifecycle.rs (WAL crash injection),
//! cli_events_reconcile.rs (healing), cli_emit.rs (exit codes, lost+found),
//! events_log.rs (2×1000 concurrent writers at the append layer).

mod harness;

#[cfg(unix)]
use harness::{Observation, poll_until};
use harness::{tendr, wait_running, wait_terminal};
#[cfg(unix)]
use std::time::Duration;
use tempfile::TempDir;

fn parse_ndjson(stdout: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("NDJSON line parses"))
        .collect()
}

/// Criterion 1: `kill -9` a supervised run; `tendr events` replays
/// `run.starting → run.started → run.sidecar_lost` with occurrence-time
/// timestamps and `provenance:"inferred"` on the last.
#[cfg(unix)]
#[test]
fn kill_nine_replays_full_lifecycle_via_events() {
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "s1", "--", "sleep", "60"])
        .assert()
        .success();
    wait_running(&root, "s1");

    let meta_path = root.path().join(".tendr/sessions/default/s1/meta.json");
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    let sc_pid = meta["sidecar"]["pid"].as_u64().unwrap() as i32;
    unsafe { libc::kill(sc_pid, libc::SIGKILL) };
    harness::poll_pid_dead(sc_pid, Duration::from_secs(5), Duration::from_millis(50))
        .unwrap_or_else(|e| panic!("sidecar never died: {e}"));

    // Reconciliation runs on the next status/wait touch ("after reboot" in
    // the criterion = any later CLI contact).
    let status_out = tendr(&root).args(["status", "s1"]).output().unwrap();
    let status: serde_json::Value = serde_json::from_slice(&status_out.stdout).unwrap();

    let output = tendr(&root)
        .args(["events", "--session", "default/s1"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let events = parse_ndjson(&output.stdout);
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["run.starting", "run.started", "run.sidecar_lost"]);

    // Occurrence-time stamps: fixed-width RFC 3339 µs Z, non-decreasing.
    let stamps: Vec<&str> = events.iter().map(|e| e["ts"].as_str().unwrap()).collect();
    assert!(stamps.iter().all(|t| t.len() == 27 && t.ends_with('Z')));
    assert!(stamps.windows(2).all(|w| w[0] <= w[1]));

    assert_eq!(events[2]["data"]["provenance"], "inferred");

    // Reconciliation killed the orphan: its evidence says so.
    assert!(
        status["transition_provenance"]["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e == "orphan_killed"),
        "{status}"
    );
}

/// Criterion 3 (process-level; the 2×1000 writer test lives in
/// events_log.rs): concurrent `tendr emit` processes interleave without
/// torn lines and every event survives.
#[test]
fn concurrent_emit_processes_no_torn_lines() {
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "s1", "--", "echo", "hi"])
        .assert()
        .success();
    wait_terminal(&root, "s1");

    let bin = assert_cmd::cargo::cargo_bin("tendr");
    let home = root.path().to_path_buf();
    std::thread::scope(|scope| {
        for t in 0..2 {
            let bin = bin.clone();
            let home = home.clone();
            scope.spawn(move || {
                for i in 0..50 {
                    let status = std::process::Command::new(&bin)
                        .env("HOME", &home)
                        .args([
                            "emit",
                            "--kind",
                            "stress.emit",
                            "--session",
                            "s1",
                            "--data",
                            &format!(r#"{{"t":{t},"i":{i}}}"#),
                        ])
                        .status()
                        .expect("emit process runs");
                    assert!(status.success());
                }
            });
        }
    });

    // Every line in every segment must parse — a torn line would not.
    let events_dir = root.path().join(".tendr/sessions/default/s1/events");
    let mut stress = 0;
    for entry in std::fs::read_dir(&events_dir)
        .unwrap()
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if path.extension().is_none_or(|x| x != "jsonl") {
            continue;
        }
        for line in std::fs::read_to_string(&path).unwrap().lines() {
            let event: serde_json::Value =
                serde_json::from_str(line).expect("no torn or interleaved lines");
            if event["kind"] == "stress.emit" {
                stress += 1;
            }
        }
    }
    assert_eq!(stress, 100, "all 100 emitted events present");
}

/// Criterion 4: an emit with 1 MiB data produces a blob + preview event with
/// `truncated:true` and a valid `data_ref.sha256`; identical payload emitted
/// twice stores one blob.
#[test]
fn one_mib_emit_spills_to_deduped_blob() {
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "s1", "--", "echo", "hi"])
        .assert()
        .success();
    wait_terminal(&root, "s1");

    let payload = format!(r#"{{"blob":"{}"}}"#, "x".repeat(1024 * 1024));
    let data_file = root.path().join("payload.json");
    std::fs::write(&data_file, &payload).unwrap();

    for _ in 0..2 {
        tendr(&root)
            .args([
                "emit",
                "--kind",
                "bulk.payload",
                "--session",
                "s1",
                "--data-file",
                data_file.to_str().unwrap(),
            ])
            .assert()
            .success();
    }

    let output = tendr(&root)
        .args(["events", "--kind", "bulk."])
        .output()
        .unwrap();
    let events = parse_ndjson(&output.stdout);
    assert_eq!(events.len(), 2);

    let session_dir = root.path().join(".tendr/sessions/default/s1");
    for event in &events {
        assert_eq!(event["truncated"], true);
        let data_ref = &event["data_ref"];
        let sha = data_ref["sha256"].as_str().unwrap();
        assert_eq!(sha.len(), 64);
        assert_eq!(
            data_ref["path"].as_str().unwrap(),
            format!("events/blobs/{sha}")
        );

        // The blob is the full payload, addressed by its hash.
        let blob_path = session_dir.join(data_ref["path"].as_str().unwrap());
        let blob = std::fs::read(&blob_path).unwrap();
        assert_eq!(blob.len() as u64, data_ref["bytes"].as_u64().unwrap());
        let full: serde_json::Value = serde_json::from_slice(&blob).unwrap();
        let original: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(full, original);

        // Preview is small and marked.
        let preview = serde_json::to_string(&event["data"]).unwrap();
        assert!(preview.len() <= 4 * 1024 + 64);
    }

    // Content-addressing dedupes the identical payload.
    let blob_count = std::fs::read_dir(session_dir.join("events/blobs"))
        .unwrap()
        .count();
    assert_eq!(blob_count, 1, "identical payloads store one blob");

    // Cross-check the hash with the system tool when available (macOS/Linux).
    let sha = events[0]["data_ref"]["sha256"].as_str().unwrap();
    let blob_path = session_dir.join(format!("events/blobs/{sha}"));
    if let Ok(out) = std::process::Command::new("shasum")
        .args(["-a", "256"])
        .arg(&blob_path)
        .output()
    {
        if out.status.success() {
            let system_sha = String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .next()
                .unwrap()
                .to_owned();
            assert_eq!(system_sha, sha, "sha256 matches the system tool");
        }
    }
}

/// Criterion 6: DuckDB reads the segments with zero schema wrangling.
/// Self-skips when duckdb is not installed.
#[test]
fn duckdb_reads_events_as_typed_rows() {
    if !harness::duckdb_or_skip() {
        return;
    }

    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "s1", "--", "echo", "hi"])
        .assert()
        .success();
    wait_terminal(&root, "s1");

    let glob = root
        .path()
        .join(".tendr/sessions/default/s1/events/*.jsonl");
    let query = format!(
        "SELECT kind, seq, run_id, ts FROM read_json('{}') ORDER BY seq",
        glob.display()
    );
    let out = std::process::Command::new("duckdb")
        .args(["-json", "-c", &query])
        .output()
        .expect("duckdb runs");
    assert!(
        out.status.success(),
        "duckdb failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(rows.len(), 3);
    let kinds: Vec<&str> = rows.iter().map(|r| r["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["run.starting", "run.started", "run.exited"]);
    assert_eq!(rows[0]["seq"], 1);
    assert!(rows[0]["run_id"].is_string());
}

// --- exec/wrap integration (docs/architecture/07-event-protocol.md §2, §6) ---

/// An exec payload running `tendr emit` lands in the exec block (frame
/// env propagation), and after the block the session shell is unpolluted —
/// probed via a raw `tendr push` (a follow-up exec exports its own var).
#[cfg(unix)]
#[test]
fn exec_payload_emit_chains_and_env_unsets() {
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "shell", "--stdin", "--", "bash"])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "shell");
    wait_running(&root, "shell");

    let bin = assert_cmd::cargo::cargo_bin("tendr");
    tendr(&root)
        .args([
            "exec",
            "shell",
            "--",
            "sh",
            "-c",
            &format!(
                "{} emit --kind build.step --data '{{}}' --best-effort",
                shell_words::quote(bin.to_str().unwrap())
            ),
        ])
        .assert()
        .success();

    let events = harness::read_events(&root, "shell");
    let started = events
        .iter()
        .find(|e| e["kind"] == "exec.started")
        .expect("exec.started");
    let step = events
        .iter()
        .find(|e| e["kind"] == "build.step")
        .expect("payload emit stored");
    assert_eq!(
        step["block_id"], started["block_id"],
        "payload emit lands in the exec block"
    );
    assert_eq!(
        step["parent_id"], started["block_id"],
        "block is the ambient parent"
    );

    // Probe the shell env after the block via raw push.
    tendr(&root)
        .args(["push", "shell"])
        .write_stdin("echo probe_${TENDR_BLOCK_ID:-unset}\n")
        .assert()
        .success();
    let log_path = root.path().join(".tendr/sessions/default/shell/output.log");
    poll_until(Duration::from_secs(5), Duration::from_millis(50), || {
        let content = std::fs::read_to_string(&log_path).unwrap_or_default();
        if content.contains("probe_unset") {
            return Observation::Ready(());
        }
        assert!(
            !content.contains("probe_01"),
            "TENDR_BLOCK_ID leaked past the block"
        );
        Observation::Pending(format!("{} log bytes, no probe_unset", content.len()))
    })
    .unwrap_or_else(|e| panic!("probe output never arrived: {e}"));
}

/// exec with ≥1 MiB stdout: the full data spills to a content-addressed
/// blob while the inline preview keeps exit_code/cwd_after queryable.
#[cfg(unix)]
#[test]
fn exec_one_mib_stdout_spills_with_structured_preview() {
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "shell", "--stdin", "--", "bash"])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "shell");
    wait_running(&root, "shell");

    let output = tendr(&root)
        .args([
            "exec",
            "shell",
            "--timeout",
            "60",
            "--",
            "sh",
            "-c",
            // The trailing echo matters: without a final newline the
            // sentinel would share the payload's line and never parse.
            "head -c 1048576 /dev/zero | tr '\\0' 'x'; echo",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let events = harness::read_events(&root, "shell");
    let result = events
        .iter()
        .find(|e| e["kind"] == "exec.result")
        .expect("exec.result");

    assert_eq!(result["truncated"], true);
    let data_ref = &result["data_ref"];
    let sha = data_ref["sha256"].as_str().expect("valid data_ref");
    assert_eq!(sha.len(), 64);

    // The blob holds the full result data.
    let blob_path = root
        .path()
        .join(".tendr/sessions/default/shell")
        .join(data_ref["path"].as_str().unwrap());
    let full: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&blob_path).unwrap()).unwrap();
    assert!(full["stdout"].as_str().unwrap().len() >= 1024 * 1024);
    assert_eq!(full["exit_code"], 0);

    // The inline preview is structured: exit_code/cwd_after queryable
    // without fetching the blob (spec example (d)).
    let preview = &result["data"];
    assert_eq!(preview["exit_code"], 0);
    assert!(preview["cwd_after"].is_string());
    assert_eq!(preview["timed_out"], false);
    assert_eq!(preview["truncated"], true);
    assert!(preview["stdout_preview"].as_str().unwrap().starts_with('x'));
    assert!(preview.get("stdout").is_none(), "preview renames the field");
}

/// The plan's validation scenario: a wrapped hook whose script emits — the
/// causal tree rebuilds in one DuckDB pass over the three foreign keys.
/// Self-skips when duckdb is not installed.
#[test]
fn wrap_hook_causal_tree_rebuilds_in_duckdb() {
    if !harness::duckdb_or_skip() {
        return;
    }

    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "agent", "--", "sleep", "60"])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "agent");
    wait_running(&root, "agent");

    let meta: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root.path().join(".tendr/sessions/default/agent/meta.json"))
            .unwrap(),
    )
    .unwrap();
    let run_id = meta["run_id"].as_str().unwrap();

    let bin = assert_cmd::cargo::cargo_bin("tendr");
    tendr(&root)
        .env("TENDR_RUN_ID", run_id)
        .env("TENDR_SESSION", "agent")
        .env("TENDR_NAMESPACE", "default")
        .args([
            "wrap",
            "--session",
            "agent",
            "--source",
            "claude.hook",
            "--event",
            "hook.post_tool_use",
            "--",
            "sh",
            "-c",
            &format!(
                "{} emit --kind hook.note --data '{{\"note\":1}}' --best-effort",
                shell_words::quote(bin.to_str().unwrap())
            ),
        ])
        .assert()
        .success();

    let glob = root
        .path()
        .join(".tendr/sessions/default/agent/events/*.jsonl");
    let query = format!(
        "SELECT parent.kind AS parent_kind, \
                child.block_id = parent.block_id AS same_block \
         FROM read_json('{g}') child JOIN read_json('{g}') parent \
           ON child.parent_id = parent.id \
         WHERE child.kind = 'hook.note'",
        g = glob.display()
    );
    let out = std::process::Command::new("duckdb")
        .args(["-json", "-c", &query])
        .output()
        .expect("duckdb runs");
    assert!(
        out.status.success(),
        "duckdb failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(rows.len(), 1, "one causal edge: note → hook event");
    assert_eq!(rows[0]["parent_kind"], "hook.post_tool_use");
    // duckdb -json renders booleans as strings.
    assert!(
        rows[0]["same_block"] == "true" || rows[0]["same_block"] == true,
        "note shares the hook's block, got: {}",
        rows[0]["same_block"]
    );
}
