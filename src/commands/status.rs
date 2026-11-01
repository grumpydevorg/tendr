use tendr::model::ids::{Namespace, ProcessIdentity, SessionName};
use tendr::session::{self, SessionError, SessionRoot};

pub fn cmd_status(name: &str, namespace: &Namespace) -> anyhow::Result<()> {
    let session_name = SessionName::new(name)?;
    let root = SessionRoot::default_path()?;

    // Try normal open first
    let session = match session::open(&root, namespace, &session_name) {
        Ok(Some(s)) => s,
        Ok(None) => anyhow::bail!("session not found: {name}"),
        Err(SessionError::Corrupt { .. }) => {
            // Check for orphan dir (child_pid but no meta.json)
            let orphan_dir = root
                .path()
                .join(namespace.as_str())
                .join(session_name.as_str());
            if orphan_dir.exists() {
                cleanup_orphan_dir(&orphan_dir);
                anyhow::bail!("session {name} was orphaned (cleaned up)");
            }
            anyhow::bail!("session not found: {name}");
        }
        Err(e) => return Err(e.into()),
    };

    let mut meta = session::read_meta(&session)?;

    // Reconciliation: non-terminal + lock not held -> sidecar gone.
    // Heals from the event log when the sidecar's own terminal event
    // exists; infers SidecarLost otherwise (spec §3.6).
    tendr::reconcile::reconcile_sidecar_gone(&session, &mut meta)?;

    let json = serde_json::to_string_pretty(&meta)?;
    outln!("{json}");
    Ok(())
}

/// Clean up an orphaned session dir that has child_pid but no meta.json.
/// The child_pid breadcrumb contains a JSON-serialized ProcessIdentity; the
/// child is killed only by the shared identity-verified rule
/// ([`tendr::reconcile::kill_verified_orphan`]), never on a bare PID.
pub(crate) fn cleanup_orphan_dir(dir: &std::path::Path) {
    let child_pid_path = dir.join("child_pid");
    if let Ok(content) = std::fs::read_to_string(&child_pid_path) {
        if let Ok(identity) = serde_json::from_str::<ProcessIdentity>(&content) {
            let _ = tendr::reconcile::kill_verified_orphan(&identity);
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}
