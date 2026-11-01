use std::fs;
use std::path::Path;
use std::time::Duration;

use serde::Serialize;

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
    let threshold_secs = older_than.map(|d| d.as_secs());
    prune_sessions(&root, &sessions, threshold_secs, namespace, dry_run)?;
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

/// Apply prune's checks to each session, delete the eligible ones (unless
/// `dry_run`), print one NDJSON line per session and a summary line, and return
/// the names that did not exist.
fn prune_sessions(
    root: &SessionRoot,
    sessions: &[(Namespace, SessionName)],
    threshold_secs: Option<u64>,
    namespace: Option<&Namespace>,
    dry_run: bool,
) -> anyhow::Result<Vec<String>> {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

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

    let summary = PruneOutput::Summary {
        deleted,
        skipped,
        failed,
        bytes_reclaimed,
        sockets_removed,
        dry_run,
        namespace: namespace.map(|ns| ns.as_str().to_owned()),
    };
    outln!("{}", serde_json::to_string(&summary).unwrap());

    Ok(not_found)
}

/// Remove (or with `dry_run`, count) attach sockets that sidecars killed
/// outright left behind (PR #68 review, finding 5). Best-effort: a sweep that
/// cannot prove a socket unused removes nothing and warns on stderr.
#[cfg(unix)]
fn sweep_attach_sockets(root: &SessionRoot, dry_run: bool) -> u64 {
    let Some(state_root) = root.path().parent() else {
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
        bytes_reclaimed: u64,
        /// Orphaned attach sockets removed (or, in a dry run, found).
        sockets_removed: u64,
        dry_run: bool,
        namespace: Option<String>,
    },
}
