//! `tendr query` — event-log analytics v1 (DuckDB over the JSONL event log).
//!
//! Analytical surface: locate the event segments in scope, register an `events`
//! view over them (ts as TIMESTAMP, data as JSON), run the user's SQL via the
//! external `duckdb` CLI. Read-only; no new write path.

mod harness;

use harness::tendr;
use predicates::prelude::*;
use tempfile::TempDir;

/// Write `lines` as one JSONL segment under `<ns>/<session>/events/`.
fn seed(root: &TempDir, ns: &str, session: &str, lines: &[&str]) {
    let dir = root
        .path()
        .join(format!(".tendr/sessions/{ns}/{session}/events"));
    std::fs::create_dir_all(&dir).unwrap();
    // Any *.jsonl name is a valid segment; the view discovers by extension.
    let seg = dir.join("00000000-0000-7000-8000-000000000001.jsonl");
    let mut body = lines.join("\n");
    body.push('\n');
    std::fs::write(&seg, body).unwrap();
}

/// Two exec.result events (exit 0 and 7) in default/s1, one hook event in agents/s2.
fn seed_mixed(root: &TempDir) {
    seed(
        root,
        "default",
        "s1",
        &[
            r#"{"v":1,"id":"a1","ts":"2026-07-09T10:00:00.000000Z","kind":"exec.result","namespace":"default","session":"s1","source":"tendr.exec","data":{"exit_code":0,"command":"echo hi"}}"#,
            r#"{"v":1,"id":"a2","ts":"2026-07-09T10:00:01.000000Z","kind":"exec.result","namespace":"default","session":"s1","source":"tendr.exec","data":{"exit_code":7,"command":"false"}}"#,
        ],
    );
    seed(
        root,
        "agents",
        "s2",
        &[
            r#"{"v":1,"id":"a3","ts":"2026-07-09T11:00:00.000000Z","kind":"hook.post_tool_use","namespace":"agents","session":"s2","source":"claude.hook","data":{"tool":"Bash"}}"#,
        ],
    );
}

#[test]
fn query_counts_all_events_across_namespaces() {
    if !harness::duckdb_or_skip() {
        return;
    }
    let root = TempDir::new().unwrap();
    seed_mixed(&root);

    tendr(&root)
        .args(["query", "SELECT COUNT(*) AS n FROM events"])
        .assert()
        .success()
        .stdout(predicate::str::contains("3"));
}

#[test]
fn query_group_by_kind_reads_kind_column() {
    if !harness::duckdb_or_skip() {
        return;
    }
    let root = TempDir::new().unwrap();
    seed_mixed(&root);

    tendr(&root)
        .args([
            "query",
            "SELECT kind, COUNT(*) AS n FROM events GROUP BY kind ORDER BY kind",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("exec.result"))
        .stdout(predicate::str::contains("hook.post_tool_use"));
}

#[test]
fn query_data_is_json_and_ts_is_timestamp() {
    if !harness::duckdb_or_skip() {
        return;
    }
    let root = TempDir::new().unwrap();
    seed_mixed(&root);

    // data->>'exit_code' proves `data` is JSON; the ts comparison proves ts is a
    // real TIMESTAMP. Exactly one exec.result has a non-zero exit code.
    tendr(&root)
        .args([
            "query",
            "SELECT COUNT(*) FILTER (WHERE (data->>'exit_code')::INT != 0) AS failures \
             FROM events \
             WHERE kind = 'exec.result' AND ts > TIMESTAMP '2026-07-09 09:00:00'",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("1"));
}

#[test]
fn query_namespace_scopes_the_view() {
    if !harness::duckdb_or_skip() {
        return;
    }
    let root = TempDir::new().unwrap();
    seed_mixed(&root);

    // Only the agents namespace is in scope: default/s1's events are excluded.
    tendr(&root)
        .args([
            "query",
            "--namespace",
            "agents",
            "SELECT DISTINCT namespace FROM events",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("agents"))
        .stdout(predicate::str::contains("default").not());
}

#[test]
fn query_from_file_runs_sql_from_disk() {
    if !harness::duckdb_or_skip() {
        return;
    }
    let root = TempDir::new().unwrap();
    seed_mixed(&root);

    let sql_path = root.path().join("q.sql");
    std::fs::write(&sql_path, "SELECT COUNT(*) AS n FROM events;\n").unwrap();

    tendr(&root)
        .args(["query", "--file"])
        .arg(&sql_path)
        .assert()
        .success()
        .stdout(predicate::str::contains("3"));
}

#[test]
fn query_empty_scope_returns_zero() {
    if !harness::duckdb_or_skip() {
        return;
    }
    let root = TempDir::new().unwrap();
    // No sessions seeded at all: the view exists but is empty.

    tendr(&root)
        .args(["query", "SELECT COUNT(*) AS n FROM events"])
        .assert()
        .success()
        .stdout(predicate::str::contains("0"));
}

#[test]
fn query_bad_sql_propagates_failure() {
    if !harness::duckdb_or_skip() {
        return;
    }
    let root = TempDir::new().unwrap();
    seed_mixed(&root);

    tendr(&root)
        .args(["query", "SELECT * FROM does_not_exist"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("does_not_exist"));
}

#[test]
fn query_missing_duckdb_errors_clearly() {
    let root = TempDir::new().unwrap();
    seed_mixed(&root);

    // Empty PATH: the `duckdb` binary cannot be found. The error must name the
    // tool and how to fix it, not leak a bare "No such file or directory".
    tendr(&root)
        .env("PATH", "")
        .args(["query", "SELECT COUNT(*) FROM events"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("duckdb"))
        .stderr(predicate::str::contains("PATH"));
}

#[test]
fn query_version_reports_duckdb() {
    if !harness::duckdb_or_skip() {
        return;
    }
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["query", "--version"])
        .assert()
        .success()
        .stdout(predicate::str::contains("DuckDB"));
}

#[test]
fn query_requires_sql_file_or_shell() {
    let root = TempDir::new().unwrap();

    // Bare `tendr query` with no SQL, no --file, no --shell, no --version is a
    // usage error.
    tendr(&root).args(["query"]).assert().failure();
}

#[test]
fn query_tolerates_a_malformed_line() {
    if !harness::duckdb_or_skip() {
        return;
    }
    let root = TempDir::new().unwrap();
    // A corrupt/torn line between two valid events must not kill the query: it
    // is skipped, and the two real events still count. (Unparseable JSON is not
    // rescued by TRY_CAST — the reader itself must tolerate it.)
    seed(
        &root,
        "default",
        "s1",
        &[
            r#"{"v":1,"id":"a1","ts":"2026-07-09T10:00:00.000000Z","kind":"exec.result","namespace":"default","session":"s1","data":{"exit_code":0}}"#,
            "not-json{",
            r#"{"v":1,"id":"a2","ts":"2026-07-09T10:00:01.000000Z","kind":"exec.result","namespace":"default","session":"s1","data":{"exit_code":7}}"#,
        ],
    );

    tendr(&root)
        .args(["query", "SELECT COUNT(*) AS n FROM events"])
        .assert()
        .success()
        .stdout(predicate::str::contains("2"));
}

/// What is left in `pipe`, read to its end, once every process that wrote to
/// it has exited; `None` if it has no end yet because some other process
/// still holds it open. Never blocks.
#[cfg(unix)]
fn read_to_closed_end(mut pipe: impl std::io::Read + std::os::fd::AsRawFd) -> Option<Vec<u8>> {
    let fd = pipe.as_raw_fd();
    // SAFETY: fcntl on a pipe fd owned by `pipe` for the whole call.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        assert!(flags >= 0, "F_GETFL: {}", std::io::Error::last_os_error());
        assert!(
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) >= 0,
            "F_SETFL: {}",
            std::io::Error::last_os_error()
        );
    }
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) => return Some(out),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return None,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => panic!("reading the pipe: {e}"),
        }
    }
}

/// What is left in `pipe`, read to its end, once every process that wrote to
/// it has exited; `None` if it has no end yet because some other process
/// still holds it open. Never blocks.
#[cfg(windows)]
fn read_to_closed_end(
    mut pipe: impl std::io::Read + std::os::windows::io::AsRawHandle,
) -> Option<Vec<u8>> {
    use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, GetLastError};
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;
    let mut out = Vec::new();
    loop {
        let mut available: u32 = 0;
        // SAFETY: the handle is owned by `pipe` for the whole call; only the
        // byte count is written.
        let ok = unsafe {
            PeekNamedPipe(
                pipe.as_raw_handle() as _,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            // SAFETY: plain FFI call.
            let error = unsafe { GetLastError() };
            assert_eq!(error, ERROR_BROKEN_PIPE, "PeekNamedPipe failed: {error}");
            return Some(out);
        }
        if available == 0 {
            return None;
        }
        let mut buf = vec![0u8; available as usize];
        let n = pipe.read(&mut buf).expect("reading the pipe");
        out.extend_from_slice(&buf[..n]);
    }
}

/// A duckdb that leaves something running (a process it started that does not
/// get its stdio) does not keep the caller's stdin, stdout or stderr open once
/// tendr exits. tendr gives duckdb a pipe for `--version`'s output and for the
/// SQL, not the caller's handles. On Windows, where a child is handed every
/// inheritable handle and not just the stdio it is given, that takes tendr
/// keeping its own stdio from being inherited.
///
/// Once tendr has exited, its stdout and stderr must be at their end at once,
/// while the orphan still runs: a pipe still held open has no end yet. No
/// clock decides it.
#[test]
fn query_is_not_held_open_by_what_duckdb_leaves_running() {
    let root = TempDir::new().unwrap();
    seed_mixed(&root);

    for args in [&["query", "--version"][..], &["query", "SELECT 1"][..]] {
        // The orphan runs from the built fake, not the linked `duckdb` copy,
        // so it does not keep that copy (and its temp dir) in use on Windows.
        let fake_dir = TempDir::new().unwrap();
        let tmp = harness::fake_program(
            "duckdb",
            &format!(
                "orphan 30 {}\nstdin {}\nout v1.4.0\\n",
                assert_cmd::cargo::cargo_bin("fake_ssh").display(),
                fake_dir.path().join("script").display()
            ),
        );
        let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("tendr"))
            .args(args)
            .env("HOME", root.path())
            .env("PATH", tmp.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let status = harness::poll_child_exit(
            &mut child,
            harness::HANG,
            std::time::Duration::from_millis(20),
        )
        .unwrap_or_else(|e| panic!("{args:?}: tendr did not exit: {e}"));
        let stderr = read_to_closed_end(stderr);
        let stderr_text = String::from_utf8_lossy(stderr.as_deref().unwrap_or_default());
        assert!(
            stderr.is_some(),
            "{args:?}: the caller's stderr is still held open"
        );
        assert!(
            read_to_closed_end(stdout).is_some(),
            "{args:?}: the caller's stdout is still held open: {stderr_text}"
        );
        assert!(status.success(), "{args:?}: {stderr_text}");
        // Nothing reads the caller's stdin any more: a write to it fails.
        use std::io::Write;
        assert!(
            stdin.write_all(b"x").and_then(|()| stdin.flush()).is_err(),
            "{args:?}: the caller's stdin is still open for reading"
        );
    }
}
