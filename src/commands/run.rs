use tendr::log::{LogQuery, follow_log};
use tendr::model::state::{ExitReason, RunStatus};
use tendr::session;

use crate::remote_op::StartRequest;

pub fn cmd_run(request: StartRequest) -> anyhow::Result<()> {
    let (meta, session) = super::start::launch_session(request)?;
    if matches!(meta.status(), RunStatus::SpawnFailed { .. }) {
        eprintln!("error: child failed to spawn");
        std::process::exit(2);
    }
    foreground_wait(&session)
}

/// Follow output through the terminal record, then propagate the child's code.
/// The log reader drains output before observing the stop condition.
fn foreground_wait(session: &session::SessionDir) -> anyhow::Result<()> {
    let meta_path = session.path().join("meta.json");
    let query = LogQuery {
        tail: None,
        since_us: Some(0),
        raw: true,
        all_updates: true,
    };
    let mut stdout = std::io::stdout().lock();
    follow_log(session.path(), &query, &mut stdout, || {
        std::fs::read_to_string(&meta_path)
            .ok()
            .and_then(|content| serde_json::from_str::<tendr::model::meta::Meta>(&content).ok())
            .is_some_and(|meta| meta.status().is_terminal())
    })?;
    drop(stdout);

    // The reader observed terminal state only after draining the output.
    let content = std::fs::read_to_string(&meta_path)?;
    let mut meta: tendr::model::meta::Meta = serde_json::from_str(&content)?;

    // Reconciliation: if the sidecar crashed between the follow thread's last
    // check and now (unlikely but possible). Best-effort — heals from the
    // event log or infers SidecarLost (spec §3.6).
    let _ = tendr::reconcile::reconcile_sidecar_gone(session, &mut meta);

    // Propagate exit code.
    match meta.status() {
        RunStatus::Exited { how, .. } => match how {
            ExitReason::ExitedOk => Ok(()),
            ExitReason::ExitedError { code } => std::process::exit(code.get()),
            ExitReason::Killed | ExitReason::KilledForced => std::process::exit(137),
            ExitReason::TimedOut => std::process::exit(124),
            ExitReason::SidecarFailed { .. } => std::process::exit(5),
        },
        RunStatus::SpawnFailed { .. } => std::process::exit(2),
        RunStatus::SidecarLost { .. } => std::process::exit(3),
        RunStatus::DependencyFailed { reason, .. } => {
            use tendr::model::dep_fail::DepFailReason;
            match reason {
                DepFailReason::Failed => std::process::exit(4),
                DepFailReason::TimedOut => std::process::exit(124),
                DepFailReason::Killed | DepFailReason::KilledForced => std::process::exit(137),
            }
        }
        _ => Ok(()),
    }
}
