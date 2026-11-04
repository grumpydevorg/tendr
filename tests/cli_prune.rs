mod harness;
use harness::Home;

use harness::{tendr, wait_running};
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

/// Start a session that exits immediately, wait for terminal state.
fn create_terminal_session(root: &Home, name: &str, namespace: &str) {
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
fn backdate_ended_at(root: &Home, namespace: &str, session: &str, age_secs: u64) {
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
    let root = Home::new();

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
    let root = Home::new();

    let out = tendr(&root)
        .args(["start", "prune-running", "--", "sleep", "3600"])
        .output()
        .unwrap();
    assert!(out.status.success());
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
    let root = Home::new();

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
    let root = Home::new();

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
    let root = Home::new();

    let out = tendr(&root).args(["prune"]).output().unwrap();
    assert!(!out.status.success(), "prune without filter should fail");
}

#[test]
fn prune_skips_corrupt_meta() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

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
    let root = Home::new();

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
    let root = Home::new();

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
    let root = Home::new();

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
    let root = Home::new();

    // 1. Deletable terminal session
    create_terminal_session(&root, "prune-mix-ok", "default");

    // 2. Running session (will be skipped)
    let out = tendr(&root)
        .args(["start", "prune-mix-run", "--", "sleep", "3600"])
        .output()
        .unwrap();
    assert!(out.status.success());
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
    let root = Home::new();

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

    // Cleanup: restore permissions so the Home can clean up
    std::fs::set_permissions(&blocker, std::fs::Permissions::from_mode(0o755)).unwrap();
}

// ── prune by name (#70) ─────────────────────────────────────────────────

fn session_dir(root: &Home, namespace: &str, name: &str) -> std::path::PathBuf {
    root.path()
        .join(format!(".tendr/sessions/{namespace}/{name}"))
}

#[test]
fn prune_by_name_deletes_only_that_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();

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
    let root = Home::new();

    tendr(&root)
        .args(["start", "prune-live", "--", "sleep", "3600"])
        .assert()
        .success();
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
    let root = Home::new();

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
    let root = Home::new();

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
    let root = Home::new();

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
    let root = Home::new();
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

// ── PTY recordings (`--recordings-older-than`, `--recordings-max`) ──────

/// A PTY session's recording directory, from its metadata.
#[cfg(unix)]
fn recording_dir(root: &Home, name: &str) -> std::path::PathBuf {
    let dir = session_dir(root, "default", name);
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("meta.json")).unwrap()).unwrap();
    dir.join(meta["pty"]["recording"]["dir"].as_str().unwrap())
}

/// Finished runs lose their recordings, those past `--recordings-older-than`
/// first and then, oldest first, those over `--recordings-max`; their
/// sessions stay, and metadata says when the recording went. A running
/// run's recording is never deleted, though it counts toward the cap.
#[cfg(unix)]
#[test]
fn prune_recordings_deletes_only_finished_runs_recordings() {
    use harness::{HANG, Observation, poll_until};
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    for name in ["rec-old", "rec-new"] {
        let out = tendr(&root)
            .args(["start", name, "--pty", "--", "sh", "-c", "printf finished"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        drop(harness::wait_terminal_quiescent_ns(&root, "default", name));
    }
    backdate_ended_at(&root, "default", "rec-old", 10 * 86_400);
    backdate_ended_at(&root, "default", "rec-new", 3_600);
    tendr(&root)
        .args([
            "start",
            "rec-live",
            "--pty",
            "--",
            "sh",
            "-c",
            "printf live; exec cat",
        ])
        .output()
        .unwrap();
    wait_running(&root, "rec-live");
    let live = poll_until(HANG, std::time::Duration::from_millis(20), || {
        let dir = session_dir(&root, "default", "rec-live").join("recording");
        match std::fs::read_dir(&dir).ok().and_then(|mut d| d.next()) {
            Some(_) => Observation::Ready(recording_dir(&root, "rec-live")),
            None => Observation::Pending(format!("{} is empty", dir.display())),
        }
    })
    .unwrap();
    let (old, new) = (
        recording_dir(&root, "rec-old"),
        recording_dir(&root, "rec-new"),
    );

    let prune = |args: &[&str]| {
        let out = tendr(&root).arg("prune").args(args).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        parse_ndjson(&out.stdout)
    };
    let deleted = |lines: &[serde_json::Value]| -> Vec<(String, String)> {
        lines
            .iter()
            .filter(|l| l["type"] == "recording" && l["action"] == "delete")
            .map(|l| {
                (
                    l["session"].as_str().unwrap().to_owned(),
                    l["reason"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    };
    let running = |lines: &[serde_json::Value]| {
        lines
            .iter()
            .any(|l| l["session"] == "rec-live" && l["skip_reason"] == "running")
    };

    let dry = prune(&["--recordings-older-than", "7d", "--dry-run"]);
    assert_eq!(
        deleted(&dry),
        [("rec-old".to_owned(), "older_than".to_owned())]
    );
    assert!(old.exists(), "a dry run deletes nothing");

    let lines = prune(&["--recordings-older-than", "7d"]);
    assert_eq!(
        deleted(&lines),
        [("rec-old".to_owned(), "older_than".to_owned())]
    );
    assert!(running(&lines), "{lines:?}");
    assert!(!old.exists() && new.exists() && live.exists());
    let status = tendr(&root).args(["status", "rec-old"]).output().unwrap();
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    // Unix seconds, written like `ended_at`.
    let deleted_at = status["pty"]["recording"]["deleted_at"]
        .as_str()
        .unwrap_or_default();
    assert!(deleted_at.parse::<u64>().is_ok(), "{status}");

    let lines = prune(&["--recordings-max", "0"]);
    assert_eq!(
        deleted(&lines),
        [("rec-new".to_owned(), "over_max".to_owned())]
    );
    assert!(running(&lines), "{lines:?}");
    assert!(!new.exists() && live.exists());
    let summary = lines.last().unwrap();
    assert_eq!(summary["type"], "recording_summary");
    assert!(summary["bytes_kept"].as_u64().unwrap() > 0, "{summary}");
    for name in ["rec-old", "rec-new"] {
        assert!(
            session_dir(&root, "default", name)
                .join("meta.json")
                .exists()
        );
    }
    tendr(&root)
        .args(["kill", "rec-live", "--force"])
        .output()
        .unwrap();
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
fn socket_dir(root: &Home) -> std::path::PathBuf {
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
    let root = Home::new();
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

    // `fresh` must stay younger than the orphan grace however slowly the two
    // prunes come round: lengthened from a minute to ten, still well short of
    // the backdated sockets' hour.
    const ORPHAN_GRACE_MS: &str = "600000";
    let dry = tendr(&root)
        .env("TENDR_TEST_ORPHAN_GRACE_MS", ORPHAN_GRACE_MS)
        .args(["prune", "--older-than", "1h", "--dry-run"])
        .output()
        .unwrap();
    assert!(dry.status.success());
    let lines = parse_ndjson(&dry.stdout);
    let summary = lines.iter().find(|l| l["type"] == "summary").unwrap();
    assert_eq!(summary["sockets_removed"], 1, "{summary}");
    assert!(orphan.exists(), "a dry run removes nothing");

    let out = tendr(&root)
        .env("TENDR_TEST_ORPHAN_GRACE_MS", ORPHAN_GRACE_MS)
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
    let root = Home::new();
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

    // Killed at the hang deadline if it waits on the listener, which never
    // accepts: returning at all is the proof.
    let out = tendr(&root)
        .args(["prune", "--older-than", "1h"])
        .timeout(harness::HANG)
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
    let root = Home::new();
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

// ── lost+found and --on-exit results (log retention) ───────────────────

const DAY: u64 = 86_400;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Append a lost+found event for `namespace`, stamped `age_secs` ago, the way
/// emit and the sidecar do.
fn lost_found_event(root: &Home, namespace: &str, kind: &str, age_secs: u64) {
    use tendr::events::EventDraft;
    use tendr::model::event::{EventTimestamp, Kind};
    use tendr::model::ids::{Namespace, RunId, SessionName, Source};

    let mut event = tendr::events::stamp_orphan_event(EventDraft {
        id: None,
        kind: Kind::new(kind).unwrap(),
        namespace: Namespace::new(namespace).unwrap(),
        session: SessionName::new("gone").unwrap(),
        run_id: RunId::new(),
        generation: None,
        source: Source::new("test.prune").unwrap(),
        block_id: None,
        parent_id: None,
        data: None,
        preview: None,
    });
    event.ts = EventTimestamp::from_parts(now_secs() - age_secs, 0);
    tendr::events::append_lost_found(&root.path().join(".tendr"), &event).unwrap();
}

fn lost_found_kinds(root: &Home) -> Vec<String> {
    let path = root.path().join(".tendr/lost+found/events.jsonl");
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .map(|line| {
            let event: serde_json::Value = serde_json::from_str(line).unwrap();
            event["kind"].as_str().unwrap().to_owned()
        })
        .collect()
}

/// Write `callbacks/<name>.json` for `namespace`, last modified `age_secs` ago.
fn callback_result(root: &Home, name: &str, namespace: &str, age_secs: u64) -> std::path::PathBuf {
    let dir = root.path().join(".tendr/callbacks");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.json"));
    let record = serde_json::json!({"run_id": name, "session": "s", "namespace": namespace, "callbacks": []});
    std::fs::write(&path, record.to_string()).unwrap();
    let then = std::time::SystemTime::now() - std::time::Duration::from_secs(age_secs);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(then)
        .unwrap();
    path
}

fn prune_summary(root: &Home, args: &[&str]) -> serde_json::Value {
    let out = tendr(root).arg("prune").args(args).output().unwrap();
    assert!(
        out.status.success(),
        "prune {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    parse_ndjson(&out.stdout)
        .into_iter()
        .find(|line| line["type"] == "summary")
        .expect("a summary line")
}

#[test]
fn prune_older_than_removes_old_lost_found_events_and_callback_results() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    lost_found_event(&root, "default", "test.old", 10 * DAY);
    lost_found_event(&root, "default", "test.new", 0);
    let old_callback = callback_result(&root, "old", "default", 10 * DAY);
    let new_callback = callback_result(&root, "new", "default", 0);

    // Another namespace's prune leaves them all.
    let summary = prune_summary(&root, &["--older-than", "7d", "--namespace", "ci"]);
    assert_eq!(summary["lost_found_removed"], 0);
    assert_eq!(summary["callbacks_removed"], 0);

    // A dry run reports what would go and removes nothing.
    let summary = prune_summary(&root, &["--older-than", "7d", "--dry-run"]);
    assert_eq!(summary["lost_found_removed"], 1);
    assert_eq!(summary["callbacks_removed"], 1);
    assert_eq!(lost_found_kinds(&root), ["test.old", "test.new"]);
    assert!(old_callback.exists());

    let summary = prune_summary(&root, &["--older-than", "7d"]);
    assert_eq!(summary["lost_found_removed"], 1);
    assert_eq!(summary["callbacks_removed"], 1);
    assert!(summary["bytes_reclaimed"].as_u64().unwrap() > 0);
    assert_eq!(lost_found_kinds(&root), ["test.new"]);
    assert!(
        !old_callback.exists(),
        "the old --on-exit result is removed"
    );
    assert!(new_callback.exists(), "a recent --on-exit result is kept");
}

/// prune rewrites lost+found while emits from pruned sessions keep appending
/// to it: every emitted event must survive every sweep.
#[test]
fn prune_sweep_loses_no_concurrent_lost_found_emit() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = Home::new();
    let home = root.path().to_path_buf();
    let sweeping = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));

    let emitter = {
        let sweeping = std::sync::Arc::clone(&sweeping);
        let run_id = uuid::Uuid::now_v7().to_string();
        std::thread::spawn(move || {
            let mut emitted = 0u64;
            // At least a few emits, then as many as overlap the sweeps.
            while emitted < 5 || sweeping.load(std::sync::atomic::Ordering::SeqCst) {
                let out = assert_cmd::Command::cargo_bin("tendr")
                    .unwrap()
                    .env("HOME", &home)
                    .env("TENDR_SESSION", "gone")
                    .env("TENDR_NAMESPACE", "default")
                    .env("TENDR_RUN_ID", &run_id)
                    .args(["emit", "--kind", "test.live", "--data"])
                    .arg(format!(r#"{{"i":{emitted}}}"#))
                    .timeout(harness::HANG)
                    .output()
                    .unwrap();
                assert!(
                    out.status.success(),
                    "{}",
                    String::from_utf8_lossy(&out.stderr)
                );
                emitted += 1;
            }
            emitted
        })
    };

    // Each round gives the sweep an old event to remove, so every prune
    // rewrites the file while the emitter appends.
    for _ in 0..20 {
        lost_found_event(&root, "default", "test.old", 10 * DAY);
        let summary = prune_summary(&root, &["--older-than", "1d"]);
        assert_eq!(summary["lost_found_removed"], 1);
    }
    sweeping.store(false, std::sync::atomic::Ordering::SeqCst);
    let emitted = emitter.join().unwrap();

    let path = root.path().join(".tendr/lost+found/events.jsonl");
    let mut seen: Vec<u64> = std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .inspect(|event| assert_eq!(event["kind"], "test.live", "only live events remain"))
        .map(|event| event["data"]["i"].as_u64().unwrap())
        .collect();
    seen.sort_unstable();
    assert_eq!(seen, (0..emitted).collect::<Vec<_>>(), "no emit was lost");
}
