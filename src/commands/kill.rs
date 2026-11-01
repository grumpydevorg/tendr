use tendr::model::ids::{Namespace, SessionName};
use tendr::model::meta::Meta;
use tendr::platform::{Current, Platform};
use tendr::session::{self, SessionDir, SessionRoot};

/// Write a kill_request file and wait for the sidecar to write terminal state.
/// Returns `Some(meta)` if the sidecar responded, `None` if it didn't within the timeout.
fn signal_kill_request(
    session: &SessionDir,
    meta: &Meta,
    force: bool,
    poll_iterations: u32,
) -> anyhow::Result<Option<Meta>> {
    let run_id = meta.run_id().to_string();
    let request = serde_json::json!({ "force": force, "run_id": run_id });
    let kill_request_path = session.path().join("kill_request");
    let kill_request_tmp = session.path().join("kill_request.tmp");
    std::fs::write(&kill_request_tmp, request.to_string())?;
    std::fs::rename(&kill_request_tmp, &kill_request_path)?;

    for _ in 0..poll_iterations {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if let Ok(m) = session::read_meta(session) {
            if m.status().is_terminal() {
                super::wait::await_release(session, m.session().as_str(), "kill");
                return Ok(Some(m));
            }
        }
    }
    Ok(None)
}

pub fn cmd_kill(name: &str, force: bool, namespace: &Namespace) -> anyhow::Result<()> {
    let session_name = SessionName::new(name)?;
    let root = SessionRoot::default_path()?;

    // Session doesn't exist -- idempotent success
    let session = match session::open(&root, namespace, &session_name)? {
        Some(s) => s,
        None => {
            outln!(
                "{}",
                serde_json::json!({"session": name, "result": "not_found"})
            );
            return Ok(());
        }
    };

    let meta = session::read_meta(&session)?;

    // Already terminal -- idempotent success
    if meta.status().is_terminal() {
        let json = serde_json::to_string_pretty(&meta)?;
        outln!("{json}");
        return Ok(());
    }

    // Get child identity from Running state
    let child = match meta.status().child() {
        Some(c) => *c,
        None => {
            // Starting state with no child — sidecar may be in dependency wait.
            // If sidecar is alive, signal it via kill_request.
            let sidecar_alive = session::is_locked(&session).unwrap_or(false);
            if sidecar_alive {
                if let Some(m) = signal_kill_request(&session, &meta, force, 80)? {
                    let json = serde_json::to_string_pretty(&m)?;
                    outln!("{json}");
                    return Ok(());
                }
                return Err(not_ended(name));
            }
            outln!(
                "{}",
                serde_json::json!({"session": name, "result": "no_child"})
            );
            return Ok(());
        }
    };

    // Write kill_forced marker before killing so sidecar can detect it
    let kill_forced_path = if force {
        let p = session.path().join("kill_forced");
        let _ = std::fs::write(&p, "");
        Some(p)
    } else {
        None
    };

    // Choose kill strategy based on whether the sidecar is alive.
    // If locked, the sidecar holds the session lock and has the live Job Object
    // (Windows) or process group context. Signal it via a control file so it
    // can perform tree-aware kill_child. Fall back to kill_orphan if the sidecar
    // is gone or doesn't respond in time.
    let sidecar_alive = session::is_locked(&session).unwrap_or(false);

    if sidecar_alive {
        // 9s total: up to 1s for a PTY child's hangup, 5s grace for graceful
        // stop, and 3s buffer for terminal state write.
        if let Some(m) = signal_kill_request(&session, &meta, force, 90)? {
            let json = serde_json::to_string_pretty(&m)?;
            outln!("{json}");
            return Ok(());
        }
        // Sidecar didn't act in time -- fall through to direct kill.
    }

    // Direct kill: sidecar is gone or unresponsive.
    if let Err(e) = Current::kill_orphan(&child, force) {
        // Clean up markers on error
        if let Some(ref p) = kill_forced_path {
            let _ = std::fs::remove_file(p);
        }
        let _ = std::fs::remove_file(session.path().join("kill_request"));
        return Err(e.into());
    }

    // Wait up to 5 s for the end to be recorded: by the sidecar, or, once no
    // sidecar holds the session, by reconciliation, as `status` would.
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if let Ok(mut m) = session::read_meta(&session) {
            tendr::reconcile::reconcile_sidecar_gone(&session, &mut m)?;
            if m.status().is_terminal() {
                super::wait::await_release(&session, name, "kill");
                let json = serde_json::to_string_pretty(&m)?;
                outln!("{json}");
                return Ok(());
            }
        }
    }

    Err(not_ended(name))
}

/// The run is still recorded as live after every wait: the kill was sent,
/// but that is not success.
fn not_ended(name: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "kill sent, but session {name} has not ended: its sidecar did not record the end"
    )
}
