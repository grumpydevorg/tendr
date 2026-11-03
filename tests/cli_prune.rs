mod harness;

use harness::{tendr, wait_running};
use std::sync::Mutex;
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

/// Start a session that exits immediately, wait for terminal state.
fn create_terminal_session(root: &TempDir, name: &str, namespace: &str) {
    let out = tendr(root)
        .args([
            "start",
            name,
            "--namespace",
            namespace,
            "--",
            "echo",
            "done",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Terminal metadata is written just before the sidecar releases its lock, and
    // prune skips a locked session (#86). Wait until the lock can be taken, then
    // release it so the test's prune can acquire it.
    drop(harness::wait_terminal_quiescent_ns(root, namespace, name));
}

/// Parse NDJSON stdout into a Vec of serde_json::Value.
fn parse_ndjson(stdout: &[u8]) -> Vec<serde_json::Value> {
    let text = String::from_utf8_lossy(stdout);
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).expect("each line should be valid JSON"))
        .collect()
}

/// Backdate ended_at in a session's meta.json by rewriting the file.
fn backdate_ended_at(root: &TempDir, namespace: &str, session: &str, age_secs: u64) {
    let meta_path = root
        .path()
        .join(format!(".tendr/sessions/{namespace}/{session}/meta.json"));
    let content = std::fs::read_to_string(&meta_path).unwrap();
    let mut meta: serde_json::Value = serde_json::from_str(&content).unwrap();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let old_ts = now - age_secs;
    meta["ended_at"] = serde_json::Value::from(old_ts);

    std::fs::write(&meta_path, serde_json::to_string_pretty(&meta).unwrap()).unwrap();
}

#[test]
fn prune_deletes_terminal_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_terminal_session(&root, "prune-del", "default");

    let session_dir = root.path().join(".tendr/sessions/default/prune-del");
    assert!(
        session_dir.exists(),
        "session dir should exist before prune"
    );

    let out = tendr(&root).args(["prune", "--all"]).output().unwrap();
    assert!(
        out.status.success(),
        "prune failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        !session_dir.exists(),
        "session dir should be removed after prune"
    );

    let lines = parse_ndjson(&out.stdout);
    assert!(
        lines.len() >= 2,
        "expected at least session + summary lines"
    );

    let session_line = &lines[0];
    assert_eq!(session_line["type"], "session");
    assert_eq!(session_line["action"], "delete");
    assert_eq!(session_line["session"], "prune-del");
}

#[test]
fn prune_skips_running_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let out = tendr(&root)
        .args(["start", "prune-running", "--", "sleep", "60"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let _session = harness::SessionGuard::new(&root, "prune-running");
    wait_running(&root, "prune-running");

    let out = tendr(&root).args(["prune", "--all"]).output().unwrap();
    assert!(out.status.success());

    let lines = parse_ndjson(&out.stdout);
    let session_line = lines.iter().find(|l| l["type"] == "session").unwrap();
    assert_eq!(session_line["action"], "skip");
    // Running sessions have the sidecar lock held, so they're skipped as "locked"
    // (lock check comes before meta read per invariant table)
    assert_eq!(session_line["skip_reason"], "locked");

    let session_dir = root.path().join(".tendr/sessions/default/prune-running");
    assert!(
        session_dir.exists(),
        "running session dir should still exist"
    );
}

#[test]
fn prune_respects_older_than() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_terminal_session(&root, "prune-old", "default");
    create_terminal_session(&root, "prune-recent", "default");

    // Backdate "prune-old" to 8 days ago
    backdate_ended_at(&root, "default", "prune-old", 8 * 24 * 3600);

    let out = tendr(&root)
        .args(["prune", "--older-than", "7d"])
        .output()
        .unwrap();
    assert!(out.status.success());

    let old_dir = root.path().join(".tendr/sessions/default/prune-old");
    let recent_dir = root.path().join(".tendr/sessions/default/prune-recent");
    assert!(!old_dir.exists(), "old session should be deleted");
    assert!(recent_dir.exists(), "recent session should be kept");

    let lines = parse_ndjson(&out.stdout);
    let actions: Vec<&str> = lines
        .iter()
        .filter(|l| l["type"] == "session")
        .map(|l| l["action"].as_str().unwrap())
        .collect();
    assert!(actions.contains(&"delete"), "should have a delete action");
    assert!(actions.contains(&"skip"), "should have a skip action");

    // Verify too_recent skip includes ended_at
    let skip_line = lines
        .iter()
        .find(|l| l["type"] == "session" && l["action"] == "skip")
        .unwrap();
    assert_eq!(skip_line["skip_reason"], "too_recent");
    assert!(
        skip_line.get("ended_at").is_some() && !skip_line["ended_at"].is_null(),
        "too_recent skip should include ended_at"
    );
}

#[test]
fn prune_dry_run_preserves_sessions() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_terminal_session(&root, "prune-dry", "default");

    let session_dir = root.path().join(".tendr/sessions/default/prune-dry");

    let out = tendr(&root)
        .args(["prune", "--all", "--dry-run"])
        .output()
        .unwrap();
    assert!(out.status.success());

    assert!(
        session_dir.exists(),
        "session dir should still exist after dry-run"
    );

    let lines = parse_ndjson(&out.stdout);
    let session_line = lines.iter().find(|l| l["type"] == "session").unwrap();
    assert_eq!(session_line["action"], "delete");

    let summary = lines.iter().find(|l| l["type"] == "summary").unwrap();
    assert_eq!(summary["dry_run"], true);
    assert_eq!(summary["deleted"], 1);
}

#[test]
fn prune_without_filter_fails() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    let out = tendr(&root).args(["prune"]).output().unwrap();
    assert!(!out.status.success(), "prune without filter should fail");
}

#[test]
fn prune_skips_corrupt_meta() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Create a session dir with garbage meta.json
    let session_dir = root.path().join(".tendr/sessions/default/prune-corrupt");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(session_dir.join("meta.json"), "not valid json {{{").unwrap();

    let out = tendr(&root).args(["prune", "--all"]).output().unwrap();
    assert!(out.status.success());

    assert!(
        session_dir.exists(),
        "corrupt session dir should still exist"
    );

    let lines = parse_ndjson(&out.stdout);
    let session_line = lines.iter().find(|l| l["type"] == "session").unwrap();
    assert_eq!(session_line["action"], "skip");
    assert_eq!(session_line["skip_reason"], "corrupt_meta");
}

#[test]
fn prune_skips_missing_meta() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Create a session dir with no meta.json
    let session_dir = root.path().join(".tendr/sessions/default/prune-nometa");
    std::fs::create_dir_all(&session_dir).unwrap();

    let out = tendr(&root).args(["prune", "--all"]).output().unwrap();
    assert!(out.status.success());

    assert!(
        session_dir.exists(),
        "missing-meta session dir should still exist"
    );

    let lines = parse_ndjson(&out.stdout);
    let session_line = lines.iter().find(|l| l["type"] == "session").unwrap();
    assert_eq!(session_line["action"], "skip");
    assert_eq!(session_line["skip_reason"], "missing_meta");
}

#[test]
fn prune_respects_namespace() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_terminal_session(&root, "prune-ns", "ns-a");
    create_terminal_session(&root, "prune-ns", "ns-b");

    let out = tendr(&root)
        .args(["prune", "--all", "--namespace", "ns-a"])
        .output()
        .unwrap();
    assert!(out.status.success());

    let dir_a = root.path().join(".tendr/sessions/ns-a/prune-ns");
    let dir_b = root.path().join(".tendr/sessions/ns-b/prune-ns");
    assert!(!dir_a.exists(), "ns-a session should be deleted");
    assert!(dir_b.exists(), "ns-b session should be untouched");

    let lines = parse_ndjson(&out.stdout);
    let summary = lines.iter().find(|l| l["type"] == "summary").unwrap();
    assert_eq!(summary["namespace"], "ns-a");
    assert_eq!(summary["deleted"], 1);
}

#[test]
fn prune_output_format() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_terminal_session(&root, "prune-fmt", "default");

    let out = tendr(&root)
        .args(["prune", "--all", "--dry-run"])
        .output()
        .unwrap();
    assert!(out.status.success());

    let lines = parse_ndjson(&out.stdout);
    assert!(lines.len() >= 2, "need at least session + summary lines");

    // Every line has a type field
    for line in &lines {
        assert!(
            line.get("type").is_some(),
            "every line must have a type field: {line}"
        );
    }

    // Last line is summary
    let last = lines.last().unwrap();
    assert_eq!(last["type"], "summary");

    // Non-last lines are sessions
    for line in &lines[..lines.len() - 1] {
        assert_eq!(line["type"], "session");
    }
}

#[test]
fn prune_summary_counts_match_mixed_outcomes() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // 1. Deletable terminal session
    create_terminal_session(&root, "prune-mix-ok", "default");

    // 2. Running session (will be skipped)
    let out = tendr(&root)
        .args(["start", "prune-mix-run", "--", "sleep", "60"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let _session = harness::SessionGuard::new(&root, "prune-mix-run");
    wait_running(&root, "prune-mix-run");

    // 3. Corrupt meta session (will be skipped)
    let corrupt_dir = root.path().join(".tendr/sessions/default/prune-mix-bad");
    std::fs::create_dir_all(&corrupt_dir).unwrap();
    std::fs::write(corrupt_dir.join("meta.json"), "garbage").unwrap();

    let out = tendr(&root).args(["prune", "--all"]).output().unwrap();
    assert!(out.status.success());

    let lines = parse_ndjson(&out.stdout);
    let summary = lines.iter().find(|l| l["type"] == "summary").unwrap();

    let deleted = summary["deleted"].as_u64().unwrap();
    let skipped = summary["skipped"].as_u64().unwrap();
    let failed = summary["failed"].as_u64().unwrap();

    assert_eq!(deleted, 1, "one terminal session should be deleted");
    assert_eq!(skipped, 2, "running + corrupt should be skipped");
    assert_eq!(failed, 0, "no failures expected");

    let session_lines: Vec<_> = lines.iter().filter(|l| l["type"] == "session").collect();
    assert_eq!(
        session_lines.len() as u64,
        deleted + skipped + failed,
        "session line count must match summary totals"
    );
}

#[test]
#[cfg(unix)]
fn prune_delete_failure_reports_error_and_continues() {
    use std::os::unix::fs::PermissionsExt;

    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    // Create two deletable terminal sessions
    create_terminal_session(&root, "prune-err-a", "default");
    create_terminal_session(&root, "prune-err-b", "default");

    // Make one session's directory unremovable by removing write permission on parent
    // Actually, make the session dir itself unreadable so remove_dir_all fails
    let dir_a = root.path().join(".tendr/sessions/default/prune-err-a");
    // Create a subdirectory and make it unremovable
    let blocker = dir_a.join("blocker");
    std::fs::create_dir(&blocker).unwrap();
    std::fs::write(blocker.join("file"), "data").unwrap();
    // Remove write+execute on the blocker dir so its contents can't be removed
    std::fs::set_permissions(&blocker, std::fs::Permissions::from_mode(0o000)).unwrap();

    let out = tendr(&root).args(["prune", "--all"]).output().unwrap();
    assert!(out.status.success(), "prune should succeed overall");

    let lines = parse_ndjson(&out.stdout);
    let summary = lines.iter().find(|l| l["type"] == "summary").unwrap();

    // One should fail (prune-err-a), one should succeed (prune-err-b)
    let failed_count = summary["failed"].as_u64().unwrap();
    let deleted_count = summary["deleted"].as_u64().unwrap();
    assert_eq!(failed_count, 1, "one deletion should fail");
    assert_eq!(deleted_count, 1, "one deletion should succeed");

    // Verify the error line exists
    let error_line = lines
        .iter()
        .find(|l| l["type"] == "session" && l["action"] == "error")
        .expect("should have an error action line");
    assert!(
        error_line.get("error").is_some() && !error_line["error"].is_null(),
        "error line should have error message"
    );

    // Cleanup: restore permissions so TempDir can clean up
    std::fs::set_permissions(&blocker, std::fs::Permissions::from_mode(0o755)).unwrap();
}

// ── prune by name (#70) ─────────────────────────────────────────────────

fn session_dir(root: &TempDir, namespace: &str, name: &str) -> std::path::PathBuf {
    root.path()
        .join(format!(".tendr/sessions/{namespace}/{name}"))
}

#[test]
fn prune_by_name_deletes_only_that_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_terminal_session(&root, "prune-one", "default");
    create_terminal_session(&root, "prune-other", "default");

    let out = tendr(&root).args(["prune", "prune-one"]).output().unwrap();
    assert!(
        out.status.success(),
        "prune NAME failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(!session_dir(&root, "default", "prune-one").exists());
    assert!(
        session_dir(&root, "default", "prune-other").exists(),
        "an unnamed session in the same namespace must be untouched"
    );

    let lines = parse_ndjson(&out.stdout);
    let sessions: Vec<_> = lines.iter().filter(|l| l["type"] == "session").collect();
    assert_eq!(
        sessions.len(),
        1,
        "only the named session is considered: {lines:?}"
    );
    assert_eq!(sessions[0]["session"], "prune-one");
    assert_eq!(sessions[0]["action"], "delete");
}

#[test]
fn prune_by_name_skips_a_running_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["start", "prune-live", "--", "sleep", "60"])
        .assert()
        .success();
    let _session = harness::SessionGuard::new(&root, "prune-live");
    wait_running(&root, "prune-live");

    let out = tendr(&root).args(["prune", "prune-live"]).output().unwrap();
    assert!(out.status.success(), "a skip is not an error");

    let lines = parse_ndjson(&out.stdout);
    let line = lines.iter().find(|l| l["type"] == "session").unwrap();
    assert_eq!(line["action"], "skip");
    assert_eq!(line["skip_reason"], "locked");
    assert!(session_dir(&root, "default", "prune-live").exists());
}

#[test]
fn prune_by_name_reports_an_unknown_name_and_fails() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_terminal_session(&root, "prune-real", "default");

    let out = tendr(&root)
        .args(["prune", "prune-real", "prune-typo"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "an unknown name must exit non-zero");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("prune-typo"),
        "stderr must name the missing session: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let lines = parse_ndjson(&out.stdout);
    let typo = lines
        .iter()
        .find(|l| l["type"] == "session" && l["session"] == "prune-typo")
        .expect("the unknown name gets its own line");
    assert_eq!(typo["action"], "skip");
    assert_eq!(typo["skip_reason"], "not_found");
    assert!(
        !session_dir(&root, "default", "prune-real").exists(),
        "the names that do exist are still pruned"
    );
}

#[test]
fn prune_by_name_dry_run_removes_nothing() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_terminal_session(&root, "prune-dry-one", "default");

    let out = tendr(&root)
        .args(["prune", "prune-dry-one", "--dry-run"])
        .output()
        .unwrap();
    assert!(out.status.success());

    let lines = parse_ndjson(&out.stdout);
    let line = lines.iter().find(|l| l["type"] == "session").unwrap();
    assert_eq!(line["action"], "delete");
    assert!(session_dir(&root, "default", "prune-dry-one").exists());
}

#[test]
fn prune_by_name_resolves_in_the_given_namespace() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();

    create_terminal_session(&root, "prune-same", "ns-a");
    create_terminal_session(&root, "prune-same", "ns-b");

    let out = tendr(&root)
        .args(["prune", "prune-same", "--namespace", "ns-a"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(!session_dir(&root, "ns-a", "prune-same").exists());
    assert!(session_dir(&root, "ns-b", "prune-same").exists());
}

#[test]
fn prune_names_conflict_with_all_and_older_than() {
    let root = TempDir::new().unwrap();
    for flag in [&["--all"][..], &["--older-than", "1d"][..]] {
        let mut args = vec!["prune", "some-session"];
        args.extend_from_slice(flag);
        let out = tendr(&root).args(&args).output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(2),
            "names with {flag:?} must be a usage error: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

// ── orphaned attach sockets (PR #68 review, finding 5) ─────────────────

/// Make an attach-socket file at `path` with no listener, as a SIGKILLed
/// sidecar leaves it. Binding creates the file; dropping the listener closes
/// the socket but leaves the path behind.
#[cfg(unix)]
fn dead_socket(path: &std::path::Path) {
    drop(std::os::unix::net::UnixListener::bind(path).unwrap());
}

/// Set `path`'s modification time an hour back, without following symlinks.
#[cfg(unix)]
fn backdate(path: &std::path::Path) {
    use std::os::unix::ffi::OsStrExt;
    let then = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 3600;
    let ts = libc::timespec {
        tv_sec: libc::time_t::try_from(then).unwrap(),
        tv_nsec: 0,
    };
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: `c` is a valid NUL-terminated path; `times` points at two timespecs.
    let rc = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            c.as_ptr(),
            [ts, ts].as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    assert_eq!(rc, 0, "utimensat: {}", std::io::Error::last_os_error());
}

#[cfg(unix)]
fn socket_dir(root: &TempDir) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = root.path().join(".tendr/sockets");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

/// A sidecar killed outright leaves its socket behind. prune removes such a
/// socket once no session's breadcrumb names it, nothing listens on it, and it
/// is older than any bind takes to publish its breadcrumb. Everything else in
/// the directory is left alone.
#[cfg(unix)]
#[test]
fn prune_sweeps_orphaned_attach_sockets_only() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let sockets = socket_dir(&root);

    // A session prune keeps (too recent), whose breadcrumb names a dead socket.
    create_terminal_session(&root, "kept", "default");
    let named = sockets.join("aaaaaaaaaaaa.sock");
    dead_socket(&named);
    std::fs::write(
        session_dir(&root, "default", "kept").join("a.sock.path"),
        named.as_os_str().as_encoded_bytes(),
    )
    .unwrap();
    let orphan = sockets.join("bbbbbbbbbbbb.sock");
    dead_socket(&orphan);
    let fresh = sockets.join("cccccccccccc.sock");
    dead_socket(&fresh);
    let live = sockets.join("dddddddddddd.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&live).unwrap();
    let not_a_socket = sockets.join("eeeeeeeeeeee.sock");
    std::fs::write(&not_a_socket, b"").unwrap();
    for path in [&named, &orphan, &live, &not_a_socket] {
        backdate(path);
    }

    let dry = tendr(&root)
        .args(["prune", "--older-than", "1h", "--dry-run"])
        .output()
        .unwrap();
    assert!(dry.status.success());
    let lines = parse_ndjson(&dry.stdout);
    let summary = lines.iter().find(|l| l["type"] == "summary").unwrap();
    assert_eq!(summary["sockets_removed"], 1, "{summary}");
    assert!(orphan.exists(), "a dry run removes nothing");

    let out = tendr(&root)
        .args(["prune", "--older-than", "1h"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lines = parse_ndjson(&out.stdout);
    let summary = lines.iter().find(|l| l["type"] == "summary").unwrap();
    assert_eq!(summary["sockets_removed"], 1, "{summary}");
    assert!(!orphan.exists(), "the orphaned socket was not swept");
    assert!(named.exists(), "a socket a session names is kept");
    assert!(
        fresh.exists(),
        "a socket younger than the grace period is kept"
    );
    assert!(live.exists(), "a socket something listens on is kept");
    assert!(not_a_socket.exists(), "only sockets are swept");
}

/// prune keeps a live listener whose backlog is full, and does not wait for
/// it: on Linux a blocking connect to one waits until it accepts, which a
/// listener no session names may never do (#102).
#[cfg(target_os = "linux")]
#[test]
fn prune_keeps_a_listener_with_a_full_backlog_without_waiting_on_it() {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};

    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let live = socket_dir(&root).join("dddddddddddd.sock");
    let addr = SocketAddrUnix::new(&live).unwrap();
    let socket = || {
        rustix::net::socket_with(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
            None,
        )
        .unwrap()
    };
    let listener = socket();
    rustix::net::bind(&listener, &addr).unwrap();
    rustix::net::listen(&listener, 0).unwrap();
    // Connect, never accepting, until the backlog refuses more.
    let mut queued = Vec::new();
    loop {
        let client = socket();
        match rustix::net::connect(&client, &addr) {
            Ok(()) => queued.push(client),
            Err(rustix::io::Errno::AGAIN) => break,
            Err(e) => panic!("connect: {e}"),
        }
        assert!(queued.len() < 64, "the backlog never filled");
    }
    backdate(&live);

    // Killed at the deadline if it waits on the listener.
    let out = tendr(&root)
        .args(["prune", "--older-than", "1h"])
        .timeout(std::time::Duration::from_secs(30))
        .output()
        .expect("run prune");
    assert!(
        out.status.success(),
        "prune must not wait on a listener with a full backlog: {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let lines = parse_ndjson(&out.stdout);
    let summary = lines.iter().find(|l| l["type"] == "summary").unwrap();
    assert_eq!(summary["sockets_removed"], 0, "{summary}");
    assert!(live.exists(), "a socket something listens on is kept");
    drop((queued, listener));
}

/// Pruning a session whose sidecar died with it removes the session's socket
/// in the same run: the breadcrumb that protected it is gone with the session.
#[cfg(unix)]
#[test]
fn prune_removes_a_pruned_sessions_dead_socket() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = TempDir::new().unwrap();
    let sockets = socket_dir(&root);

    create_terminal_session(&root, "crashed", "default");
    let socket = sockets.join("ffffffffffff.sock");
    dead_socket(&socket);
    backdate(&socket);
    std::fs::write(
        session_dir(&root, "default", "crashed").join("a.sock.path"),
        socket.as_os_str().as_encoded_bytes(),
    )
    .unwrap();

    let out = tendr(&root).args(["prune", "--all"]).output().unwrap();
    assert!(out.status.success());
    assert!(!session_dir(&root, "default", "crashed").exists());
    assert!(
        !socket.exists(),
        "the pruned session's socket was left behind"
    );
}
