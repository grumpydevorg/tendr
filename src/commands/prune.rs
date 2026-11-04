use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde::Serialize;

use tendr::events::{self, LostFoundSweep};
use tendr::model::event::EventTimestamp;
use tendr::model::ids::{Namespace, SessionName};
use tendr::model::state::{ExitReason, RunStatus};
use tendr::session::{self, SessionRoot};

pub fn cmd_prune(
    older_than: Option<Duration>,
    all: bool,
    namespace: Option<&Namespace>,
    dry_run: bool,
) -> anyhow::Result<()> {
    if older_than.is_none() && !all {
        anyhow::bail!("session names, --older-than or --all is required");
    }

    let root = SessionRoot::default_path()?;
    let sessions = session::list(&root, namespace)?;
    prune_sessions(&root, &sessions, older_than, namespace, dry_run)?;
    Ok(())
}

/// Prune the named sessions in `namespace`. Each one goes through the same checks
/// as `--all`, so a live session is never deleted. A name that doesn't exist is
/// reported as `not_found` and fails the command, so a typo isn't a silent no-op.
pub fn cmd_prune_named(
    names: &[String],
    namespace: &Namespace,
    dry_run: bool,
) -> anyhow::Result<()> {
    let root = SessionRoot::default_path()?;
    let mut sessions: Vec<(Namespace, SessionName)> = Vec::new();
    for name in names {
        let name = SessionName::new(name)?;
        if !sessions.iter().any(|(_, seen)| seen == &name) {
            sessions.push((namespace.clone(), name));
        }
    }

    let not_found = prune_sessions(&root, &sessions, None, Some(namespace), dry_run)?;
    if !not_found.is_empty() {
        anyhow::bail!(
            "no such session in namespace {}: {}",
            namespace.as_str(),
            not_found.join(", ")
        );
    }
    Ok(())
}

/// A finished run's recording that `--recordings-…` may delete.
struct Recording {
    ns: Namespace,
    name: SessionName,
    session_dir: session::SessionDir,
    /// Relative to the session directory, as `meta.json` names it.
    dir: String,
    ended_at: u64,
    bytes: u64,
}

/// Delete the PTY recordings of finished runs, never their sessions: first
/// those of runs that ended more than `older_than` ago, then, while all
/// recordings together take more than `max` bytes, those of the runs that
/// ended first. A recording whose run may still be going (its session is
/// locked, or not in a terminal state) is never deleted; it counts toward
/// the total. `meta.json` keeps the recording's entry, with `deleted_at`.
/// Prints one NDJSON line per recording deleted or kept running, and a
/// summary line.
pub fn cmd_prune_recordings(
    older_than: Option<Duration>,
    max: Option<u64>,
    namespace: Option<&Namespace>,
    dry_run: bool,
) -> anyhow::Result<()> {
    let root = SessionRoot::default_path()?;
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut finished = Vec::new();
    let mut total: u64 = 0;
    let mut kept_running: u64 = 0;
    for (ns, name) in session::list(&root, namespace)? {
        let Ok(session_dir) = session::open_raw(&root, &ns, &name) else {
            continue;
        };
        let Ok(meta) = session::read_meta(&session_dir) else {
            continue;
        };
        let Some(recording) = meta.pty().and_then(|pty| pty.recording.as_ref()) else {
            continue;
        };
        if recording.deleted_at.is_some() {
            continue;
        }
        let Some(bytes) = dir_size_bytes(&session_dir.path().join(&recording.dir)) else {
            continue; // nothing on disk
        };
        total += bytes;
        let live =
            !matches!(session::is_locked(&session_dir), Ok(false)) || !meta.status().is_terminal();
        match meta.status().ended_at().filter(|_| !live) {
            Some(ended_at) => finished.push(Recording {
                ns,
                name,
                session_dir,
                dir: recording.dir.clone(),
                ended_at: ended_at.as_secs(),
                bytes,
            }),
            None => {
                kept_running += 1;
                emit_recording(&ns, &name, "skip", &recording.dir, None, bytes, None, None);
            }
        }
    }

    // Oldest first, so a cap deletes the runs that ended first.
    finished.sort_by_key(|r| r.ended_at);
    let mut deleted: u64 = 0;
    let mut failed: u64 = 0;
    let mut bytes_reclaimed: u64 = 0;
    for recording in finished {
        let expired =
            older_than.is_some_and(|d| now_secs.saturating_sub(recording.ended_at) >= d.as_secs());
        let over_cap = max.is_some_and(|max| total > max);
        if !expired && !over_cap {
            continue;
        }
        let reason = if expired { "older_than" } else { "over_max" };
        let result = if dry_run {
            Ok(())
        } else {
            delete_recording(&recording)
        };
        match result {
            Ok(()) => {
                deleted += 1;
                bytes_reclaimed += recording.bytes;
                total -= recording.bytes;
                emit_recording(
                    &recording.ns,
                    &recording.name,
                    "delete",
                    &recording.dir,
                    Some(recording.ended_at),
                    recording.bytes,
                    Some(reason),
                    None,
                );
            }
            Err(e) => {
                failed += 1;
                emit_recording(
                    &recording.ns,
                    &recording.name,
                    "error",
                    &recording.dir,
                    Some(recording.ended_at),
                    recording.bytes,
                    Some(reason),
                    Some(&e.to_string()),
                );
            }
        }
    }

    let summary = PruneOutput::RecordingSummary {
        recordings_deleted: deleted,
        recordings_running: kept_running,
        failed,
        bytes_reclaimed,
        bytes_kept: total,
        dry_run,
        namespace: namespace.map(|ns| ns.as_str().to_owned()),
    };
    outln!("{}", serde_json::to_string(&summary).unwrap());
    Ok(())
}

/// Delete a finished run's segments and mark its entry in `meta.json`,
/// holding the session's lock, so no sidecar or `start --replace` acts on the
/// session meanwhile. A session taken since it was listed, or holding
/// another run by now, keeps its recording.
fn delete_recording(recording: &Recording) -> anyhow::Result<()> {
    let kept = || anyhow::anyhow!("the session changed while pruning; its recording is kept");
    let _lock = session::LockGuard::try_acquire(&recording.session_dir).map_err(|_| kept())?;
    let mut meta = session::read_meta(&recording.session_dir)?;
    let mut entry = meta
        .pty()
        .and_then(|pty| pty.recording.clone())
        .filter(|entry| entry.dir == recording.dir && meta.status().is_terminal())
        .ok_or_else(kept)?;
    fs::remove_dir_all(recording.session_dir.path().join(&recording.dir))?;
    entry.deleted_at = Some(tendr::model::ids::EpochTimestamp::now());
    meta.set_pty_recording(entry);
    session::write_meta_atomic(&recording.session_dir, &meta)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)] // one NDJSON line's fields
fn emit_recording(
    ns: &Namespace,
    name: &SessionName,
    action: &'static str,
    dir: &str,
    ended_at: Option<u64>,
    bytes: u64,
    reason: Option<&str>,
    error: Option<&str>,
) {
    let line = PruneOutput::Recording {
        action,
        namespace: ns.as_str().to_owned(),
        session: name.as_str().to_owned(),
        dir: dir.to_owned(),
        ended_at,
        bytes,
        reason: reason.map(str::to_owned),
        skip_reason: (action == "skip").then(|| "running".to_owned()),
        error: error.map(str::to_owned),
    };
    outln!("{}", serde_json::to_string(&line).unwrap());
}

/// Apply prune's checks to each session, delete the eligible ones (unless
/// `dry_run`), print one NDJSON line per session and a summary line, and return
/// the names that did not exist. With `older_than`, also sweep lost+found
/// events and `--on-exit` results older than that.
fn prune_sessions(
    root: &SessionRoot,
    sessions: &[(Namespace, SessionName)],
    older_than: Option<Duration>,
    namespace: Option<&Namespace>,
    dry_run: bool,
) -> anyhow::Result<Vec<String>> {
    let now = SystemTime::now();
    let now_secs = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let threshold_secs = older_than.map(|d| d.as_secs());

    let mut deleted: u64 = 0;
    let mut skipped: u64 = 0;
    let mut failed: u64 = 0;
    let mut bytes_reclaimed: u64 = 0;
    let mut not_found: Vec<String> = Vec::new();

    for (ns, name) in sessions {
        let session_dir = match session::open_raw(root, ns, name) {
            Ok(dir) => dir,
            Err(_) => {
                emit_skip(ns, name, "not_found", None);
                skipped += 1;
                not_found.push(name.as_str().to_owned());
                continue;
            }
        };

        // Check lock FIRST, before reading meta
        match session::is_locked(&session_dir) {
            Ok(true) => {
                emit_skip(ns, name, "locked", None);
                skipped += 1;
                continue;
            }
            Ok(false) => {}
            Err(_) => {
                emit_skip(ns, name, "locked", None);
                skipped += 1;
                continue;
            }
        }

        // Structural meta classification — no string matching
        if !session_dir.meta_path().exists() {
            emit_skip(ns, name, "missing_meta", None);
            skipped += 1;
            continue;
        }

        let meta = match session::read_meta(&session_dir) {
            Ok(m) => m,
            Err(_) => {
                emit_skip(ns, name, "corrupt_meta", None);
                skipped += 1;
                continue;
            }
        };

        if !meta.status().is_terminal() {
            let skip_reason = match meta.status() {
                RunStatus::Running { .. } => "running",
                RunStatus::Starting => "starting",
                _ => "running", // unreachable given is_terminal check
            };
            emit_skip(ns, name, skip_reason, None);
            skipped += 1;
            continue;
        }

        let ended_at = match meta.status().ended_at() {
            Some(ts) => ts.as_secs(),
            None => {
                emit_skip(ns, name, "missing_ended_at", None);
                skipped += 1;
                continue;
            }
        };

        if let Some(threshold) = threshold_secs {
            let age = now_secs.saturating_sub(ended_at);
            if age < threshold {
                emit_skip(ns, name, "too_recent", Some(ended_at));
                skipped += 1;
                continue;
            }
        }

        // Session is eligible for deletion
        let exit_reason = exit_reason_label(meta.status());
        let bytes = dir_size_bytes(session_dir.path());

        if dry_run {
            emit_delete(ns, name, ended_at, &exit_reason, bytes);
            deleted += 1;
            if let Some(b) = bytes {
                bytes_reclaimed += b;
            }
        } else {
            match fs::remove_dir_all(session_dir.path()) {
                Ok(()) => {
                    emit_delete(ns, name, ended_at, &exit_reason, bytes);
                    deleted += 1;
                    if let Some(b) = bytes {
                        bytes_reclaimed += b;
                    }
                }
                Err(e) => {
                    emit_error(ns, name, &e.to_string());
                    failed += 1;
                }
            }
        }
    }

    // After the sessions: a pruned session's breadcrumb no longer protects its
    // socket, so a dead sidecar's socket goes in the same run.
    let sockets_removed = sweep_attach_sockets(root, dry_run);

    let mut lost_found = LostFoundSweep::default();
    let mut callbacks = FileSweep::default();
    if let (Some(older_than), Some(state_root)) = (older_than, root.state_root()) {
        let cutoff = now.checked_sub(older_than).unwrap_or(std::time::UNIX_EPOCH);
        lost_found = sweep_lost_found(state_root, cutoff, namespace, dry_run);
        callbacks = sweep_callbacks(&state_root.join("callbacks"), cutoff, namespace, dry_run);
    }
    bytes_reclaimed += lost_found.bytes + callbacks.bytes;

    let summary = PruneOutput::Summary {
        deleted,
        skipped,
        failed,
        bytes_reclaimed,
        sockets_removed,
        lost_found_removed: lost_found.events,
        callbacks_removed: callbacks.files,
        dry_run,
        namespace: namespace.map(|ns| ns.as_str().to_owned()),
    };
    outln!("{}", serde_json::to_string(&summary).unwrap());

    Ok(not_found)
}

/// Remove (or with `dry_run`, count) lost+found events stamped before
/// `cutoff`. A sweep that fails removes nothing and warns on stderr.
fn sweep_lost_found(
    state_root: &Path,
    cutoff: SystemTime,
    namespace: Option<&Namespace>,
    dry_run: bool,
) -> LostFoundSweep {
    let since_epoch = cutoff
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let cutoff = EventTimestamp::from_parts(since_epoch.as_secs(), since_epoch.subsec_micros());
    events::sweep_lost_found(state_root, cutoff, namespace, dry_run).unwrap_or_else(|e| {
        eprintln!("tendr: lost+found not swept: {e}");
        LostFoundSweep::default()
    })
}

/// What [`sweep_callbacks`] removed, or in a dry run would remove.
#[derive(Default)]
struct FileSweep {
    files: u64,
    bytes: u64,
}

/// Remove (or with `dry_run`, count) the `--on-exit` results in `dir` last
/// written before `cutoff`, only those of `namespace` when it is given. The
/// sidecar writes each file once, after the run's hooks, so its age is the
/// age of the result. A file that cannot be removed is left and named on
/// stderr.
fn sweep_callbacks(
    dir: &Path,
    cutoff: SystemTime,
    namespace: Option<&Namespace>,
    dry_run: bool,
) -> FileSweep {
    let mut swept = FileSweep::default();
    let Ok(entries) = fs::read_dir(dir) else {
        return swept;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() || !metadata.modified().is_ok_and(|m| m < cutoff) {
            continue;
        }
        if let Some(namespace) = namespace {
            let file_namespace = fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .and_then(|record| record["namespace"].as_str().map(str::to_owned));
            if file_namespace.as_deref() != Some(namespace.as_str()) {
                continue;
            }
        }
        if !dry_run {
            if let Err(e) = fs::remove_file(&path) {
                eprintln!("tendr: {} not removed: {e}", path.display());
                continue;
            }
        }
        swept.files += 1;
        swept.bytes += metadata.len();
    }
    swept
}

/// Remove (or with `dry_run`, count) attach sockets that sidecars killed
/// outright left behind (PR #68 review, finding 5). Best-effort: a sweep that
/// cannot prove a socket unused removes nothing and warns on stderr.
#[cfg(unix)]
fn sweep_attach_sockets(root: &SessionRoot, dry_run: bool) -> u64 {
    let Some(state_root) = root.state_root() else {
        return 0;
    };
    match tendr::attach_socket::sweep_orphans(state_root, dry_run) {
        Ok(orphans) => orphans.len() as u64,
        Err(e) => {
            eprintln!("tendr: orphaned attach sockets not swept: {e}");
            0
        }
    }
}

#[cfg(not(unix))]
fn sweep_attach_sockets(_root: &SessionRoot, _dry_run: bool) -> u64 {
    0
}

fn emit_skip(
    ns: &Namespace,
    name: &tendr::model::ids::SessionName,
    reason: &str,
    ended_at: Option<u64>,
) {
    let line = PruneOutput::Session {
        action: "skip",
        namespace: ns.as_str().to_owned(),
        session: name.as_str().to_owned(),
        ended_at,
        reason: None,
        skip_reason: Some(reason.to_owned()),
        error: None,
        bytes: None,
    };
    outln!("{}", serde_json::to_string(&line).unwrap());
}

fn emit_delete(
    ns: &Namespace,
    name: &tendr::model::ids::SessionName,
    ended_at: u64,
    reason: &str,
    bytes: Option<u64>,
) {
    let line = PruneOutput::Session {
        action: "delete",
        namespace: ns.as_str().to_owned(),
        session: name.as_str().to_owned(),
        ended_at: Some(ended_at),
        reason: Some(reason.to_owned()),
        skip_reason: None,
        error: None,
        bytes,
    };
    outln!("{}", serde_json::to_string(&line).unwrap());
}

fn emit_error(ns: &Namespace, name: &tendr::model::ids::SessionName, error: &str) {
    let line = PruneOutput::Session {
        action: "error",
        namespace: ns.as_str().to_owned(),
        session: name.as_str().to_owned(),
        ended_at: None,
        reason: None,
        skip_reason: None,
        error: Some(error.to_owned()),
        bytes: None,
    };
    outln!("{}", serde_json::to_string(&line).unwrap());
}

fn exit_reason_label(status: &RunStatus) -> String {
    match status {
        RunStatus::Exited { how, .. } => match how {
            ExitReason::ExitedOk => "ExitedOk".to_owned(),
            ExitReason::ExitedError { code } => format!("ExitedError({})", code.get()),
            ExitReason::Killed => "Killed".to_owned(),
            ExitReason::KilledForced => "KilledForced".to_owned(),
            ExitReason::TimedOut => "TimedOut".to_owned(),
            ExitReason::SidecarFailed { step } => format!("SidecarFailed({step})"),
        },
        RunStatus::SpawnFailed { .. } => "SpawnFailed".to_owned(),
        RunStatus::SidecarLost { .. } => "SidecarLost".to_owned(),
        _ => "Unknown".to_owned(),
    }
}

/// Best-effort recursive directory size in bytes.
/// Returns None only if the top-level read_dir fails.
/// Individual file stat failures are silently skipped.
fn dir_size_bytes(path: &Path) -> Option<u64> {
    let mut total: u64 = 0;
    let mut dirs = vec![path.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) if dir == path => return None,
            Err(_) => continue,
        };
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                dirs.push(entry.path());
            } else {
                total += metadata.len();
            }
        }
    }
    Some(total)
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PruneOutput {
    Session {
        action: &'static str,
        namespace: String,
        session: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        ended_at: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        skip_reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        bytes: Option<u64>,
    },
    Summary {
        deleted: u64,
        skipped: u64,
        failed: u64,
        /// Bytes of the deleted sessions, lost+found events and `--on-exit`
        /// results.
        bytes_reclaimed: u64,
        /// Orphaned attach sockets removed (or, in a dry run, found).
        sockets_removed: u64,
        /// lost+found events removed (only with `--older-than`).
        lost_found_removed: u64,
        /// `--on-exit` result files removed (only with `--older-than`).
        callbacks_removed: u64,
        dry_run: bool,
        namespace: Option<String>,
    },
    /// A recording `--recordings-…` deleted (`delete`), failed to delete
    /// (`error`), or kept because its run may still be going (`skip`).
    Recording {
        action: &'static str,
        namespace: String,
        session: String,
        /// Relative to the session directory.
        dir: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        ended_at: Option<u64>,
        bytes: u64,
        /// `older_than` or `over_max`.
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        skip_reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    RecordingSummary {
        recordings_deleted: u64,
        /// Recordings kept because their run may still be going.
        recordings_running: u64,
        failed: u64,
        bytes_reclaimed: u64,
        /// What every recording left, running ones included, takes.
        bytes_kept: u64,
        dry_run: bool,
        namespace: Option<String>,
    },
}
