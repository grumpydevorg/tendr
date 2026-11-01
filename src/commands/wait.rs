use std::collections::BTreeMap;

use tendr::model::ids::{Namespace, SessionName};
use tendr::model::meta::Meta;
use tendr::model::state::{ExitReason, RunStatus};
use tendr::session::{self, SessionRoot};

/// Poll interval for the wait loop.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Wait for one or more sessions to reach terminal state.
///
/// - `any == false` (default): blocks until ALL sessions are terminal.
/// - `any == true`: blocks until ANY session is terminal, then returns only
///   the terminal sessions in the output array.
///
/// Exit codes follow the single-session convention extended to sets:
/// - 0: all reported sessions exited successfully
/// - 2: at least one spawn failure
/// - 3: at least one sidecar lost (the child may still be running)
/// - 5: at least one sidecar failure (the sidecar killed the child)
/// - 4: at least one dependency failure
/// - 42: at least one non-zero child exit
/// - 1: session error (not found, etc.) — handled by anyhow bail
///
/// Timeout is reported via anyhow::bail (exit code 1), consistent with
/// other Tendr commands that use anyhow for operational errors.
///
/// A session counts as done once its meta is terminal *and* its sidecar has
/// released the session lock, so `prune` or `start --replace` straight after
/// `wait` find it free (#91). The release gets [`session::RELEASE_GRACE`]; if it
/// has not happened by then, `wait` warns on stderr and reports the session
/// anyway. The exit code always reflects the session's outcome, and the grace
/// is not charged against `--timeout`.
pub fn cmd_wait(
    names: &[String],
    timeout: Option<u64>,
    any: bool,
    namespace: &Namespace,
) -> anyhow::Result<()> {
    let root = SessionRoot::default_path()?;

    // Deduplicate while preserving request order for output.
    let mut seen = std::collections::HashSet::new();
    let unique_names: Vec<&str> = names
        .iter()
        .filter_map(|n| {
            if seen.insert(n.as_str()) {
                Some(n.as_str())
            } else {
                None
            }
        })
        .collect();

    // Open all unique sessions upfront — fail fast if any don't exist.
    let sessions: BTreeMap<String, session::SessionDir> = unique_names
        .iter()
        .map(|&n| {
            let name = SessionName::new(n)?;
            let dir = session::open(&root, namespace, &name)?
                .ok_or_else(|| anyhow::anyhow!("session not found: {name}"));
            dir.map(|d| (n.to_string(), d))
        })
        .collect::<anyhow::Result<BTreeMap<_, _>>>()?;

    let deadline = timeout.map(|t| std::time::Instant::now() + std::time::Duration::from_secs(t));

    // Track which sessions have reached terminal state.
    let mut terminal_metas: BTreeMap<String, Meta> = BTreeMap::new();

    loop {
        for (name, session_dir) in &sessions {
            if terminal_metas.contains_key(name) {
                continue;
            }

            let mut meta = session::read_meta(session_dir)?;

            // Reconciliation: non-terminal + lock not held -> sidecar gone.
            // Heals from the event log when the sidecar's own terminal event
            // exists; infers SidecarLost otherwise (spec §3.6).
            tendr::reconcile::reconcile_sidecar_gone(session_dir, &mut meta)?;

            if meta.status().is_terminal() {
                await_release(session_dir, name, "wait");
                terminal_metas.insert(name.clone(), meta);
            }
        }

        // Check completion condition.
        let done = if any {
            !terminal_metas.is_empty()
        } else {
            terminal_metas.len() == sessions.len()
        };

        if done {
            // Collect metas in deduplicated request order, one entry per unique name.
            let results: Vec<&Meta> = unique_names
                .iter()
                .filter_map(|n| terminal_metas.get(*n))
                .collect();

            let json = serde_json::to_string_pretty(&results)?;
            outln!("{json}");

            // Derive exit code from the set.
            let exit_code = derive_exit_code(&results);
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            return Ok(());
        }

        // Check timeout.
        if let Some(dl) = deadline {
            if std::time::Instant::now() >= dl {
                let pending: Vec<&str> = unique_names
                    .iter()
                    .copied()
                    .filter(|n| !terminal_metas.contains_key(*n))
                    .collect();
                anyhow::bail!("timeout waiting for {}", pending.join(", "));
            }
        }

        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Derive the exit code from a set of terminal metas.
///
/// When multiple sessions have different failure modes, the most severe
/// failure wins. Severity order (highest to lowest):
///
/// - 2: spawn failure (process never started)
/// - 3: sidecar lost (supervision crashed; the child may still be running)
/// - 5: sidecar failed (supervision failed, killed the child, said so)
/// - 4/124/137: dependency failed (4=upstream non-zero, 124=upstream timeout, 137=killed during wait)
/// - 42: child exited non-zero (process ran but failed)
/// - 0: success
fn derive_exit_code(metas: &[&Meta]) -> i32 {
    let mut worst: i32 = 0;

    for meta in metas {
        let code = single_exit_code(meta.status());
        if code != 0 && (worst == 0 || severity(code) > severity(worst)) {
            worst = code;
        }
    }

    worst
}

/// Severity rank for exit code comparison. Higher = more severe.
fn severity(code: i32) -> u8 {
    match code {
        2 => 6,             // spawn failed
        3 => 5,             // sidecar lost
        5 => 4,             // sidecar failed
        4 | 124 | 137 => 3, // dependency failed (any sub-reason)
        42 => 1,            // non-zero exit
        _ => 0,
    }
}

/// Map a single terminal RunStatus to its exit code.
///
/// These codes match the documented Tendr exit code contract
/// (consistent with `tendr run`):
/// - 0: success (ExitedOk, Killed, KilledForced, TimedOut)
/// - 2: spawn failure
/// - 3: sidecar lost
/// - 4: dependency failed (upstream exited non-zero)
/// - 5: sidecar failed
/// - 42: child exited non-zero
/// - 124: dependency timed out
/// - 137: killed during dependency wait
fn single_exit_code(status: &RunStatus) -> i32 {
    match status {
        RunStatus::Exited { how, .. } => match how {
            ExitReason::ExitedOk => 0,
            ExitReason::ExitedError { .. } => 42,
            ExitReason::Killed | ExitReason::KilledForced | ExitReason::TimedOut => 0,
            ExitReason::SidecarFailed { .. } => 5,
        },
        RunStatus::SpawnFailed { .. } => 2,
        RunStatus::SidecarLost { .. } => 3,
        RunStatus::DependencyFailed { reason, .. } => {
            use tendr::model::dep_fail::DepFailReason;
            match reason {
                DepFailReason::Failed => 4,
                DepFailReason::TimedOut => 124,
                DepFailReason::Killed | DepFailReason::KilledForced => 137,
            }
        }
        // Non-terminal states shouldn't reach here, but return 0 if they do.
        _ => 0,
    }
}

/// Give a terminal session's sidecar [`session::RELEASE_GRACE`] to release the
/// session lock before reporting the session done. Warns on stderr if it has
/// not; the caller reports the session either way (#91).
pub(crate) fn await_release(session_dir: &session::SessionDir, name: &str, command: &str) {
    if let Ok(false) = session::wait_unlocked(session_dir, session::RELEASE_GRACE) {
        eprintln!(
            "tendr {command}: {name} is terminal but its sidecar still holds the session lock \
             after {}s; prune and start --replace may still find it locked",
            session::RELEASE_GRACE.as_secs()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tendr::model::ids::{EpochTimestamp, ProcessIdentity};
    use tendr::model::state::SidecarStep;

    fn sidecar_failed() -> RunStatus {
        RunStatus::Exited {
            child: ProcessIdentity {
                pid: std::num::NonZeroU32::new(7).unwrap(),
                start_time_ns: 1,
            },
            how: ExitReason::SidecarFailed {
                step: SidecarStep::OutputLog,
            },
            ended_at: EpochTimestamp::from_secs(1),
        }
    }

    #[test]
    fn sidecar_failed_exits_5() {
        assert_eq!(single_exit_code(&sidecar_failed()), 5);
    }

    /// 3 (the child may still be running) outranks 5 (the sidecar killed it),
    /// which outranks a dependency failure.
    #[test]
    fn sidecar_failed_ranks_between_lost_and_dependency_failure() {
        assert!(severity(2) > severity(3));
        assert!(severity(3) > severity(5));
        assert!(severity(5) > severity(4));
        assert!(severity(5) > severity(42));
    }
}
