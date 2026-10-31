//! CLI-side reconciliation of sessions whose sidecar is gone — the shared
//! path behind `wait`/`status`/`run` (docs/architecture/07-event-protocol.md §3.6).
//!
//! Before inferring `run.sidecar_lost`, read the event-log tail: if the
//! sidecar's own terminal event exists (it died in the WAL crash window
//! between the event append and the meta write), heal meta from that record
//! instead of inferring loss.
//!
//! When loss is inferred, the lost sidecar's child may still be running
//! (SIGKILL, OOM or abort skip the sidecar's own guard). Reconciliation finds
//! it (meta's `Running` child, or the `child_pid` breadcrumb while meta is
//! `Starting`), kills it only if its identity is verified alive, and records
//! the outcome as evidence and a warning. A healed `SidecarFailed` gets the
//! same treatment: the sidecar records it even when its kill did not take.

use std::num::NonZeroI32;
use std::path::Path;

use crate::events::{self, EventDraft, EventWriter, read_session_events};
use crate::model::dep_fail::DepFailReason;
use crate::model::event::{Event, Uuid7};
use crate::model::ids::{EpochTimestamp, Namespace, ProcessIdentity, Source};
use crate::model::meta::Meta;
use crate::model::state::{ExitReason, RunStatus, SidecarStep};
use crate::model::transition::{HealedTerminal, Orphan};
use crate::platform::{Current, Platform, ProcessStatus};
use crate::session::{self, LockGuard, SessionDir, SessionError};

/// What reconciliation did to meta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reconciled {
    /// Meta was already terminal, or a live sidecar holds the lock.
    Untouched,
    /// Meta healed from the sidecar's own terminal event
    /// (Direct provenance, EventLogTerminal evidence, plus OrphanKilled when
    /// a healed `SidecarFailed`'s child was still alive and was killed).
    Healed,
    /// No terminal event found: SidecarLost inferred, and the inferred
    /// `run.sidecar_lost` event appended to the log.
    Inferred,
}

/// Reconcile a session whose meta is non-terminal while nothing holds its
/// lock. Writes meta atomically when it changes it; a no-op otherwise.
///
/// The session lock is held for the whole reconciliation and `meta` is
/// re-read under it, so a sidecar that wrote its terminal record and released
/// the lock after the caller read `meta` always wins: its record is returned
/// in `meta`, untouched.
///
/// # Errors
/// Returns lock, meta-read, meta-write, or transition errors. Event-log reads
/// and the inferred-event append are best-effort — they never fail
/// reconciliation.
pub fn reconcile_sidecar_gone(session: &SessionDir, meta: &mut Meta) -> anyhow::Result<Reconciled> {
    if meta.status().is_terminal() {
        return Ok(Reconciled::Untouched);
    }
    let _lock = match LockGuard::try_acquire(session) {
        Ok(lock) => lock,
        Err(SessionError::Locked(_)) => return Ok(Reconciled::Untouched),
        Err(e) => return Err(e.into()),
    };
    *meta = session::read_meta(session)?;
    if meta.status().is_terminal() {
        return Ok(Reconciled::Untouched);
    }

    let spawned_child = left_behind_child(session, meta);
    if let Some(event) = find_sidecar_terminal_event(session.path(), meta) {
        if let Some(healed) = event.data.as_ref().and_then(healed_terminal_of) {
            let ended_at = EpochTimestamp::from_secs(event.ts.epoch_secs());
            if meta
                .heal_terminal_from_event(healed, ended_at, spawned_child)
                .is_ok()
            {
                stop_healed_orphan(meta)?;
                session::write_meta_atomic(session, meta)?;
                return Ok(Reconciled::Healed);
            }
        }
    }

    let orphan = spawned_child.map(|child| Orphan {
        child,
        killed: stop_orphan(&child, meta, "its sidecar was lost"),
    });
    meta.reconcile_sidecar_lost(EpochTimestamp::now(), orphan)?;
    // WAL discipline holds for the inferred record too: event before meta.
    append_sidecar_lost_event(session, meta);
    session::write_meta_atomic(session, meta)?;
    Ok(Reconciled::Inferred)
}

/// The child a lost sidecar may have left running: meta's `Running` child,
/// or, while meta is still `Starting`, the `child_pid` breadcrumb the sidecar
/// writes right after spawning.
fn left_behind_child(session: &SessionDir, meta: &Meta) -> Option<ProcessIdentity> {
    match meta.status() {
        RunStatus::Running { child } => Some(*child),
        RunStatus::Starting => {
            let content = std::fs::read_to_string(session.path().join("child_pid")).ok()?;
            serde_json::from_str(&content).ok()
        }
        _ => None,
    }
}

/// What [`kill_verified_orphan`] found and did.
#[derive(Debug)]
pub enum OrphanStop {
    /// Verified alive and killed.
    Killed,
    /// Verified alive; the kill failed.
    KillFailed(std::io::Error),
    /// A process with that PID exists but its identity cannot be read, so
    /// it may not be ours: left alone.
    Unverifiable,
    /// The probe itself failed: left alone.
    CheckFailed(std::io::ErrorKind),
    /// Already gone, or the PID now belongs to another process.
    Gone,
}

/// Kill a lost sidecar's child if, and only if, it is verifiably the same
/// process: PID reuse must never kill a stranger. Force-kills its process
/// group, so a caller like `status` never blocks on a grace period. The one
/// orphan-kill rule, shared by reconciliation and the orphan-directory
/// cleanup.
pub fn kill_verified_orphan(child: &ProcessIdentity) -> OrphanStop {
    match Current::process_status(child) {
        ProcessStatus::AliveVerified => match Current::kill_orphan(child, true) {
            Ok(()) => OrphanStop::Killed,
            Err(e) => OrphanStop::KillFailed(e),
        },
        ProcessStatus::Inaccessible => OrphanStop::Unverifiable,
        ProcessStatus::OsError(kind) => OrphanStop::CheckFailed(kind),
        ProcessStatus::Missing | ProcessStatus::IdentityMismatch => OrphanStop::Gone,
    }
}

/// A healed `SidecarFailed` is the one healed record whose child may still be
/// running: the sidecar records it even when its forced kill did not take.
/// Stop that child by the same rule as a lost sidecar's, and add
/// `OrphanKilled` evidence when it was killed. Every other healed record needs
/// no probe: an observed exit (`Exited`, `Killed*`, `TimedOut`) means the
/// sidecar reaped the child, and `SpawnFailed` / `DependencyFailed` never had
/// one.
fn stop_healed_orphan(meta: &mut Meta) -> anyhow::Result<()> {
    let RunStatus::Exited {
        child,
        how: ExitReason::SidecarFailed { .. },
        ..
    } = meta.status()
    else {
        return Ok(());
    };
    let child = *child;
    if stop_orphan(&child, meta, "its sidecar failed") {
        meta.record_healed_orphan_killed()?;
    }
    Ok(())
}

/// Stop the child a gone sidecar left behind by the shared rule and record
/// every outcome but "already gone" as a warning naming `after` (how the
/// sidecar went). Returns whether it was killed.
fn stop_orphan(child: &ProcessIdentity, meta: &mut Meta, after: &str) -> bool {
    let pid = child.pid;
    let note = match kill_verified_orphan(child) {
        OrphanStop::Killed => {
            meta.add_warning(format!(
                "child pid {pid} was still running after {after}; killed it"
            ));
            return true;
        }
        OrphanStop::KillFailed(e) => {
            format!("child pid {pid} is still running after {after}; killing it failed: {e}")
        }
        OrphanStop::Unverifiable => format!(
            "child pid {pid} may still be running after {after}; \
             its identity could not be verified, so it was not killed"
        ),
        OrphanStop::CheckFailed(kind) => {
            format!("child pid {pid} could not be checked after {after}: {kind}")
        }
        OrphanStop::Gone => return false,
    };
    meta.add_warning(note);
    false
}

/// The sidecar's own most recent terminal event for meta's run, if any.
/// Best-effort: unreadable logs mean "no evidence", never an error.
fn find_sidecar_terminal_event(session_dir: &Path, meta: &Meta) -> Option<Event> {
    let outcome = read_session_events(session_dir).ok()?;
    let run_id = meta.run_id();
    let sidecar_writer = Uuid7::from(run_id);
    outcome.events.into_iter().rev().find(|event| {
        event.run_id == run_id
            && event.writer == sidecar_writer
            && event.source.as_str() == "tendr.sidecar"
            && matches!(
                event.kind.as_str(),
                "run.exited"
                    | "run.killed"
                    | "run.timed_out"
                    | "run.sidecar_failed"
                    | "run.spawn_failed"
                    | "run.dependency_failed"
            )
    })
}

/// Parse a lifecycle event's `data` back into a terminal outcome.
/// Returns `None` on any shape surprise — the caller then infers loss.
fn healed_terminal_of(data: &serde_json::Value) -> Option<HealedTerminal> {
    match data["status"].as_str()? {
        "Exited" => {
            let how = match data["reason"].as_str()? {
                "ExitedOk" => ExitReason::ExitedOk,
                "ExitedError" => ExitReason::ExitedError {
                    code: NonZeroI32::new(i32::try_from(data["exit_code"].as_i64()?).ok()?)?,
                },
                "Killed" => ExitReason::Killed,
                "KilledForced" => ExitReason::KilledForced,
                "TimedOut" => ExitReason::TimedOut,
                "SidecarFailed" => ExitReason::SidecarFailed {
                    step: SidecarStep::from_wire(data["step"].as_str()?)?,
                },
                _ => return None,
            };
            Some(HealedTerminal::Exited(how))
        }
        "SpawnFailed" => Some(HealedTerminal::SpawnFailed),
        "DependencyFailed" => {
            let reason = match data["reason"].as_str()? {
                "Failed" => DepFailReason::Failed,
                "TimedOut" => DepFailReason::TimedOut,
                "Killed" => DepFailReason::Killed,
                "KilledForced" => DepFailReason::KilledForced,
                _ => return None,
            };
            Some(HealedTerminal::DependencyFailed(reason))
        }
        _ => None,
    }
}

/// Append the inferred `run.sidecar_lost` event (`data.provenance:
/// "inferred"`, source `tendr.cli`, fresh CLI writer identity). Best-effort:
/// reconciliation must not fail because the history log is unwritable.
fn append_sidecar_lost_event(session: &SessionDir, meta: &Meta) {
    let Some(namespace) = namespace_of(session) else {
        return;
    };
    let Ok(source) = Source::trusted("tendr.cli") else {
        return;
    };
    let draft = EventDraft {
        id: None,
        kind: events::lifecycle_kind(meta.status()),
        namespace,
        session: meta.session().clone(),
        run_id: meta.run_id(),
        generation: Some(meta.generation().as_u64()),
        source,
        block_id: None,
        parent_id: None,
        data: Some(events::lifecycle_data(
            meta.status(),
            "inferred",
            meta.launch_spec().boundary.as_ref(),
        )),
        preview: None,
    };
    let mut writer = EventWriter::new(session.path());
    let _ = writer.append(draft, true);
}

/// Namespace from the session dir's structure: `root/<namespace>/<session>/`.
fn namespace_of(session: &SessionDir) -> Option<Namespace> {
    let name = session.path().parent()?.file_name()?.to_str()?;
    Namespace::new(name).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ids::{Generation, RunId, SessionName};
    use crate::model::spec::LaunchSpec;
    use std::num::NonZeroU32;

    fn running_meta(child: ProcessIdentity) -> Meta {
        let mut meta = Meta::new_starting(
            SessionName::new("t").unwrap(),
            RunId::new(),
            Generation::first(),
            LaunchSpec::new(vec!["true".to_owned()]).unwrap(),
            child,
            EpochTimestamp::from_secs(0),
        );
        meta.transition_running(child).unwrap();
        meta
    }

    /// A reused PID must never be killed: the probe sees a different start
    /// time and reconciliation leaves the process alone, silently.
    #[test]
    fn stop_orphan_never_kills_a_reused_pid() {
        let me = Current::self_identity().unwrap();
        let stale = ProcessIdentity {
            pid: me.pid,
            start_time_ns: me.start_time_ns.wrapping_add(1),
        };
        let mut meta = running_meta(stale);
        assert!(!stop_orphan(&stale, &mut meta, "its sidecar was lost"));
        assert!(meta.warnings().is_empty(), "{:?}", meta.warnings());
        assert_eq!(Current::process_status(&me), ProcessStatus::AliveVerified);
    }

    #[test]
    fn stop_orphan_ignores_a_child_that_is_already_gone() {
        let mut child = std::process::Command::new(if cfg!(windows) { "cmd" } else { "true" })
            .args(if cfg!(windows) {
                &["/C", "exit"][..]
            } else {
                &[][..]
            })
            .spawn()
            .unwrap();
        let id = Current::process_identity(child.id()).unwrap_or(ProcessIdentity {
            pid: NonZeroU32::new(child.id()).unwrap(),
            start_time_ns: 0,
        });
        child.wait().unwrap();
        // `child` still holds the process handle: on Windows the exited
        // process object stays alive, and must still read as gone.
        let mut meta = running_meta(id);
        assert!(!stop_orphan(&id, &mut meta, "its sidecar was lost"));
        assert!(meta.warnings().is_empty(), "{:?}", meta.warnings());
    }

    /// A long-lived child, killed and reaped on drop so a failing test leaks
    /// nothing. On Unix it leads its own process group, as the sidecar's
    /// child does, so the reconciler's group kill reaches it and nothing else.
    struct LiveChild(std::process::Child);

    impl LiveChild {
        fn spawn() -> Self {
            #[cfg(unix)]
            let child = {
                use std::os::unix::process::CommandExt;
                std::process::Command::new("sleep")
                    .arg("60")
                    .process_group(0)
                    .spawn()
            };
            #[cfg(windows)]
            let child = std::process::Command::new("ping")
                .args(["-n", "60", "127.0.0.1"])
                .stdout(std::process::Stdio::null())
                .spawn();
            Self(child.unwrap())
        }

        /// Whether it exits within `timeout`. Reaping it, rather than
        /// probing, is the proof: a Linux zombie still reads `AliveVerified`.
        fn exits_within(&mut self, timeout: std::time::Duration) -> bool {
            let deadline = std::time::Instant::now() + timeout;
            while std::time::Instant::now() < deadline {
                if self.0.try_wait().unwrap().is_some() {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            false
        }
    }

    impl Drop for LiveChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// The sidecar records `SidecarFailed` even when its forced kill did not
    /// take; if its meta write then failed, only the durable event and the
    /// breadcrumb remain. Healing from that event must kill the still-alive,
    /// verified child and record it, or `status` says terminal while the child
    /// runs on and every later reconciliation is a no-op.
    #[test]
    fn a_healed_sidecar_failed_kills_the_child_its_sidecar_could_not_stop() {
        let root = tempfile::TempDir::new().unwrap();
        let session = session::create(
            &session::SessionRoot::new(root.path().to_path_buf()),
            &Namespace::new("default").unwrap(),
            &SessionName::new("t").unwrap(),
        )
        .unwrap();
        let mut child = LiveChild::spawn();
        let id = Current::process_identity(child.0.id()).unwrap();

        // Meta still `Starting` (the `--after` path), the breadcrumb, and the
        // sidecar's durable `run.sidecar_failed`.
        let run_id = RunId::new();
        let meta = Meta::new_starting(
            SessionName::new("t").unwrap(),
            run_id,
            Generation::first(),
            LaunchSpec::new(vec!["sleep".to_owned()]).unwrap(),
            Current::self_identity().unwrap(),
            EpochTimestamp::from_secs(0),
        );
        session::write_meta_atomic(&session, &meta).unwrap();
        std::fs::write(
            session.path().join("child_pid"),
            serde_json::to_string(&id).unwrap(),
        )
        .unwrap();
        let failed = RunStatus::Exited {
            child: id,
            how: ExitReason::SidecarFailed {
                step: SidecarStep::StdinTransport,
            },
            ended_at: EpochTimestamp::from_secs(1),
        };
        EventWriter::with_writer(session.path(), Uuid7::from(run_id))
            .append(
                EventDraft {
                    id: None,
                    kind: events::lifecycle_kind(&failed),
                    namespace: Namespace::new("default").unwrap(),
                    session: SessionName::new("t").unwrap(),
                    run_id,
                    generation: Some(1),
                    source: Source::trusted("tendr.sidecar").unwrap(),
                    block_id: None,
                    parent_id: None,
                    data: Some(events::lifecycle_data(&failed, "direct", None)),
                    preview: None,
                },
                true,
            )
            .unwrap();

        let mut meta = session::read_meta(&session).unwrap();
        assert_eq!(
            reconcile_sidecar_gone(&session, &mut meta).unwrap(),
            Reconciled::Healed
        );
        assert!(
            matches!(
                meta.status(),
                RunStatus::Exited { child, how: ExitReason::SidecarFailed { .. }, .. } if *child == id
            ),
            "{:?}",
            meta.status()
        );
        assert_eq!(
            meta.transition_provenance(),
            Some(&crate::model::provenance::TransitionProvenance::direct(&[
                crate::model::provenance::Evidence::EventLogTerminal,
                crate::model::provenance::Evidence::OrphanKilled,
            ]))
        );
        assert!(
            meta.warnings()
                .iter()
                .any(|w| w.ends_with("after its sidecar failed; killed it")),
            "{:?}",
            meta.warnings()
        );
        assert_eq!(
            session::read_meta(&session)
                .unwrap()
                .transition_provenance(),
            meta.transition_provenance(),
            "the kill is in the record on disk"
        );
        assert!(
            child.exits_within(std::time::Duration::from_secs(10)),
            "the reconciler killed the child"
        );
    }

    /// Every terminal `Exited` shape the sidecar writes must parse back, or a
    /// WAL-window crash would be mislabelled `SidecarLost`.
    #[test]
    fn every_exit_reason_heals_from_its_own_lifecycle_data() {
        let child = ProcessIdentity {
            pid: NonZeroU32::new(7).unwrap(),
            start_time_ns: 1,
        };
        for how in [
            ExitReason::ExitedOk,
            ExitReason::ExitedError {
                code: NonZeroI32::new(3).unwrap(),
            },
            ExitReason::Killed,
            ExitReason::KilledForced,
            ExitReason::TimedOut,
            ExitReason::SidecarFailed {
                step: SidecarStep::ChildWait,
            },
        ] {
            let status = RunStatus::Exited {
                child,
                how: how.clone(),
                ended_at: EpochTimestamp::from_secs(1),
            };
            let data = events::lifecycle_data(&status, "direct", None);
            match healed_terminal_of(&data) {
                Some(HealedTerminal::Exited(healed)) => assert_eq!(healed, how),
                other => panic!("{how:?} did not heal from {data}: {other:?}"),
            }
        }
    }

    #[test]
    fn sidecar_failed_without_a_known_step_does_not_heal() {
        let data =
            serde_json::json!({"status": "Exited", "reason": "SidecarFailed", "step": "nope"});
        assert!(healed_terminal_of(&data).is_none());
    }
}
