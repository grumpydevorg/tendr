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

    for _ in 0..kill_wait_polls(poll_iterations) {
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

/// How many 100 ms polls [`signal_kill_request`] and [`await_record`] make:
/// `default`, or in debug builds `TENDR_TEST_KILL_WAIT_MS` worth, so tests
/// whose sidecar must record the kill itself are not sent to the direct kill
/// by a loaded machine; release builds ignore it.
fn kill_wait_polls(default: u32) -> u32 {
    if cfg!(debug_assertions) {
        if let Some(ms) = std::env::var("TENDR_TEST_KILL_WAIT_MS")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
        {
            return ms.div_ceil(100);
        }
    }
    default
}

/// Whether a sidecar holds `session` but has not recorded its run yet: a
/// sidecar takes the session lock first and writes `meta.json` only once the
/// child is spawned (or at `Starting`, for a run with `--after`). Such a
/// session is starting, not orphaned: its directory is the sidecar's. A lock
/// that cannot be probed counts as held, so an error never lets the orphan
/// cleanup remove a live sidecar's directory.
pub(crate) fn is_unrecorded(session: &SessionDir) -> bool {
    !session.path().join("meta.json").exists() && session::is_locked(session).unwrap_or(true)
}

/// The error for a session [`is_unrecorded`].
pub(crate) fn starting_error(name: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "session {name} is starting: its sidecar holds the session but has not recorded the run yet"
    )
}

/// Wait while `session` [`is_unrecorded`], for a `command` that must act on
/// the run: one landing there (the `start` client interrupted while waiting
/// for readiness) would find no run to act on, and the run would start after
/// it. Bounded like the wait for a kill request.
///
/// # Errors
/// The sidecar has still not recorded the run when the bound passes.
pub(crate) fn await_record(session: &SessionDir, name: &str, command: &str) -> anyhow::Result<()> {
    if !is_unrecorded(session) {
        return Ok(());
    }
    eprintln!("tendr {command}: {name} is starting; waiting for its sidecar to record the run");
    let polls = kill_wait_polls(110);
    for _ in 0..polls {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if !is_unrecorded(session) {
            return Ok(());
        }
    }
    anyhow::bail!(
        "{name} is still starting after {} s: its sidecar holds the session but has not recorded the run",
        polls.div_ceil(10)
    )
}

pub fn cmd_kill(name: &str, force: bool, namespace: &Namespace) -> anyhow::Result<()> {
    let session_name = SessionName::new(name)?;
    let root = SessionRoot::default_path()?;

    if let Ok(dir) = session::open_raw(&root, namespace, &session_name) {
        await_record(&dir, name, "kill")?;
    }

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
            // No sidecar: record the end as `status` would, stopping a child
            // the lost sidecar may have spawned.
            let mut m = meta;
            tendr::reconcile::reconcile_sidecar_gone(&session, &mut m)?;
            if !m.status().is_terminal() {
                return Err(not_ended(name));
            }
            let json = serde_json::to_string_pretty(&m)?;
            outln!("{json}");
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
        // 11 s total: up to 1 s for a PTY child's hangup, 5 s grace for a
        // graceful stop, up to 2 s for a PTY's holder grace (capture ends at
        // most 2 s after the child exits, while another process holds the
        // terminal), and 3 s buffer for the terminal state write.
        if let Some(m) = signal_kill_request(&session, &meta, force, 110)? {
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
