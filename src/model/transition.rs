use thiserror::Error;

use super::dep_fail::DepFailReason;
use super::ids::{EpochTimestamp, ProcessIdentity};
use super::meta::Meta;
use super::provenance::{Evidence, TransitionProvenance};
use super::state::{ExitReason, RunStatus, SidecarStep};

#[derive(Debug, Error)]
pub enum TransitionError {
    #[error("cannot transition from {from} — already terminal")]
    AlreadyTerminal { from: &'static str },
    #[error("illegal transition from {from} to {to}")]
    Illegal {
        from: &'static str,
        to: &'static str,
    },
}

fn status_name(status: &RunStatus) -> &'static str {
    match status {
        RunStatus::Starting => "Starting",
        RunStatus::Running { .. } => "Running",
        RunStatus::SpawnFailed { .. } => "SpawnFailed",
        RunStatus::Exited { how, .. } => match how {
            ExitReason::ExitedOk => "ExitedOk",
            ExitReason::ExitedError { .. } => "ExitedError",
            ExitReason::Killed => "Killed",
            ExitReason::KilledForced => "KilledForced",
            ExitReason::TimedOut => "TimedOut",
            ExitReason::SidecarFailed { .. } => "SidecarFailed",
        },
        RunStatus::SidecarLost { .. } => "SidecarLost",
        RunStatus::DependencyFailed { .. } => "DependencyFailed",
    }
}

impl Meta {
    /// Transition Starting → Running. Requires child identity.
    pub fn transition_running(&mut self, child: ProcessIdentity) -> Result<(), TransitionError> {
        match self.status() {
            RunStatus::Starting => {
                *self.status_mut() = RunStatus::Running { child };
                self.set_transition_provenance(TransitionProvenance::direct(&[
                    Evidence::SidecarWrite,
                    Evidence::ChildSpawned,
                ]));
                Ok(())
            }
            RunStatus::Running { .. } => Err(TransitionError::Illegal {
                from: "Running",
                to: "Running",
            }),
            _ => Err(TransitionError::AlreadyTerminal {
                from: status_name(self.status()),
            }),
        }
    }

    /// Transition Starting → SpawnFailed. Child never started.
    /// Only valid from Starting — cannot reach SpawnFailed from Running.
    pub fn transition_spawn_failed(
        &mut self,
        ended_at: EpochTimestamp,
    ) -> Result<(), TransitionError> {
        self.spawn_failed(ended_at, Evidence::SpawnFailedSyscall)
    }

    /// Transition Starting → SpawnFailed because a PTY session's attach
    /// socket could not be bound: the child was never spawned.
    pub fn transition_attach_socket_failed(
        &mut self,
        ended_at: EpochTimestamp,
    ) -> Result<(), TransitionError> {
        self.spawn_failed(ended_at, Evidence::AttachSocketFailed)
    }

    fn spawn_failed(
        &mut self,
        ended_at: EpochTimestamp,
        cause: Evidence,
    ) -> Result<(), TransitionError> {
        match self.status() {
            RunStatus::Starting => {
                *self.status_mut() = RunStatus::SpawnFailed { ended_at };
                self.set_transition_provenance(TransitionProvenance::direct(&[
                    Evidence::SidecarWrite,
                    cause,
                ]));
                Ok(())
            }
            RunStatus::Running { .. } => Err(TransitionError::Illegal {
                from: "Running",
                to: "SpawnFailed",
            }),
            _ => Err(TransitionError::AlreadyTerminal {
                from: status_name(self.status()),
            }),
        }
    }

    /// Transition Running → Exited. Only valid from Running.
    /// ExitReason cannot include SpawnFailed — that's a separate type.
    /// Child identity is carried from Running into the Exited state.
    /// `SidecarFailed` is not an observed exit: it goes through
    /// [`Meta::transition_sidecar_failed`], which carries its own evidence.
    pub fn transition_exited(
        &mut self,
        how: ExitReason,
        ended_at: EpochTimestamp,
    ) -> Result<(), TransitionError> {
        if matches!(how, ExitReason::SidecarFailed { .. }) {
            return Err(TransitionError::Illegal {
                from: status_name(self.status()),
                to: "SidecarFailed (use transition_sidecar_failed)",
            });
        }
        match self.status() {
            RunStatus::Running { child } => {
                let child = *child;
                *self.status_mut() = RunStatus::Exited {
                    child,
                    how,
                    ended_at,
                };
                self.set_transition_provenance(TransitionProvenance::direct(&[
                    Evidence::SidecarWrite,
                    Evidence::ChildExitObserved,
                ]));
                Ok(())
            }
            RunStatus::Starting => Err(TransitionError::Illegal {
                from: "Starting",
                to: "Exited",
            }),
            _ => Err(TransitionError::AlreadyTerminal {
                from: status_name(self.status()),
            }),
        }
    }

    /// Transition Starting | Running → `Exited { SidecarFailed { step } }`:
    /// the sidecar failed after spawning `child`, killed it, and records that
    /// itself (Direct provenance). Valid from `Starting` because the failure
    /// can precede publishing `Running` (the child existed; `Running` was
    /// never persisted). From `Running`, the recorded child is kept.
    pub fn transition_sidecar_failed(
        &mut self,
        child: ProcessIdentity,
        step: SidecarStep,
        ended_at: EpochTimestamp,
    ) -> Result<(), TransitionError> {
        let child = match self.status() {
            RunStatus::Starting => child,
            RunStatus::Running { child: running } => *running,
            _ => {
                return Err(TransitionError::AlreadyTerminal {
                    from: status_name(self.status()),
                });
            }
        };
        *self.status_mut() = RunStatus::Exited {
            child,
            how: ExitReason::SidecarFailed { step },
            ended_at,
        };
        self.set_transition_provenance(TransitionProvenance::direct(&[
            Evidence::SidecarWrite,
            Evidence::SupervisionFailed,
        ]));
        Ok(())
    }

    /// Transition Starting → DependencyFailed.
    pub fn transition_dependency_failed(
        &mut self,
        ended_at: EpochTimestamp,
        reason: DepFailReason,
    ) -> Result<(), TransitionError> {
        match self.status() {
            RunStatus::Starting => {
                *self.status_mut() = RunStatus::DependencyFailed { ended_at, reason };
                self.set_transition_provenance(TransitionProvenance::direct(&[
                    Evidence::SidecarWrite,
                    Evidence::DependencyFailed,
                ]));
                Ok(())
            }
            RunStatus::Running { .. } => Err(TransitionError::Illegal {
                from: "Running",
                to: "DependencyFailed",
            }),
            _ => Err(TransitionError::AlreadyTerminal {
                from: status_name(self.status()),
            }),
        }
    }

    /// Reconciliation: mark as SidecarLost. The ONLY case where
    /// something other than the sidecar writes lifecycle state.
    /// Valid from Starting or Running. `orphan` is the child the lost sidecar
    /// left behind, if reconciliation found one: from `Starting` it is the
    /// only record of that child (the `child_pid` breadcrumb); from `Running`
    /// meta's child is kept. A killed orphan adds `OrphanKilled` evidence.
    pub fn reconcile_sidecar_lost(
        &mut self,
        ended_at: EpochTimestamp,
        orphan: Option<Orphan>,
    ) -> Result<(), TransitionError> {
        let child = match self.status() {
            RunStatus::Starting => orphan.map(|o| o.child),
            RunStatus::Running { child } => Some(*child),
            _ => {
                return Err(TransitionError::AlreadyTerminal {
                    from: status_name(self.status()),
                });
            }
        };
        *self.status_mut() = RunStatus::SidecarLost { child, ended_at };
        let mut evidence = vec![Evidence::LockReleased, Evidence::NonTerminalMeta];
        if orphan.is_some_and(|o| o.killed) {
            evidence.push(Evidence::OrphanKilled);
        }
        self.set_transition_provenance(TransitionProvenance::inferred(&evidence));
        Ok(())
    }

    /// Reconciliation healing: apply a terminal state recorded by the
    /// sidecar's own event-log record (spec §3.6). The observation was the
    /// sidecar's — the CLI only replays it into meta — so provenance is
    /// Direct with EventLogTerminal evidence, not Inferred.
    ///
    /// `spawned_child` is the child the sidecar spawned before `Running` was
    /// published (its `child_pid` breadcrumb). It lets a `SidecarFailed`
    /// recorded before `Running` (the `--after` path) heal from `Starting`.
    pub fn heal_terminal_from_event(
        &mut self,
        healed: HealedTerminal,
        ended_at: EpochTimestamp,
        spawned_child: Option<ProcessIdentity>,
    ) -> Result<(), TransitionError> {
        match (self.status(), healed, spawned_child) {
            (RunStatus::Running { child }, HealedTerminal::Exited(how), _) => {
                let child = *child;
                *self.status_mut() = RunStatus::Exited {
                    child,
                    how,
                    ended_at,
                };
            }
            (
                RunStatus::Starting,
                HealedTerminal::Exited(how @ ExitReason::SidecarFailed { .. }),
                Some(child),
            ) => {
                *self.status_mut() = RunStatus::Exited {
                    child,
                    how,
                    ended_at,
                };
            }
            (RunStatus::Starting, HealedTerminal::SpawnFailed, _) => {
                *self.status_mut() = RunStatus::SpawnFailed { ended_at };
            }
            (RunStatus::Starting, HealedTerminal::DependencyFailed(reason), _) => {
                *self.status_mut() = RunStatus::DependencyFailed { ended_at, reason };
            }
            (status, _, _) if status.is_terminal() => {
                return Err(TransitionError::AlreadyTerminal {
                    from: status_name(status),
                });
            }
            (status, _, _) => {
                // Shape mismatch (e.g. meta Starting but event says Exited):
                // the caller falls back to inferring SidecarLost.
                return Err(TransitionError::Illegal {
                    from: status_name(status),
                    to: "healed terminal",
                });
            }
        }
        self.set_transition_provenance(TransitionProvenance::direct(&[Evidence::EventLogTerminal]));
        Ok(())
    }

    /// Reconciliation killed the child that a healed `SidecarFailed` record
    /// names: the sidecar records that failure even when its forced kill did
    /// not take, so the record alone does not prove the child dead. Adds
    /// `OrphanKilled` evidence to the healed record. Valid only right after
    /// [`Meta::heal_terminal_from_event`] healed a `SidecarFailed`.
    pub fn record_healed_orphan_killed(&mut self) -> Result<(), TransitionError> {
        let healed_sidecar_failed = matches!(
            self.status(),
            RunStatus::Exited {
                how: ExitReason::SidecarFailed { .. },
                ..
            }
        ) && self.transition_provenance()
            == Some(&TransitionProvenance::direct(&[Evidence::EventLogTerminal]));
        if !healed_sidecar_failed {
            return Err(TransitionError::Illegal {
                from: status_name(self.status()),
                to: "healed SidecarFailed with a killed orphan",
            });
        }
        self.set_transition_provenance(TransitionProvenance::direct(&[
            Evidence::EventLogTerminal,
            Evidence::OrphanKilled,
        ]));
        Ok(())
    }
}

/// A child a lost sidecar left behind, as reconciliation found it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Orphan {
    pub child: ProcessIdentity,
    /// Reconciliation verified it alive and killed it.
    pub killed: bool,
}

/// A terminal outcome parsed from the sidecar's own event-log record, used
/// to heal meta when the sidecar died between the event append and the meta
/// write (the WAL crash window, spec §3.6).
#[derive(Debug, Clone)]
pub enum HealedTerminal {
    Exited(ExitReason),
    SpawnFailed,
    DependencyFailed(DepFailReason),
}

#[cfg(test)]
mod provenance_tests {
    use super::*;
    use crate::model::ids::{EpochTimestamp, Generation, ProcessIdentity, RunId, SessionName};
    use crate::model::provenance::Evidence;
    use crate::model::spec::LaunchSpec;

    fn fresh_meta() -> Meta {
        Meta::new_starting(
            SessionName::new("t").unwrap(),
            RunId::new(),
            Generation::first(),
            LaunchSpec::new(vec!["bash".to_owned()]).unwrap(),
            ProcessIdentity {
                pid: std::num::NonZero::new(1).unwrap(),
                start_time_ns: 0,
            },
            EpochTimestamp::from_secs(0),
        )
    }

    fn child() -> ProcessIdentity {
        ProcessIdentity {
            pid: std::num::NonZero::new(2).unwrap(),
            start_time_ns: 0,
        }
    }

    #[test]
    fn running_is_direct_with_child_spawned() {
        let mut m = fresh_meta();
        m.transition_running(child()).unwrap();
        let p = m.transition_provenance().unwrap();
        assert!(p.is_direct());
        let TransitionProvenance::Direct { evidence } = p else {
            unreachable!()
        };
        assert!(evidence.contains(&Evidence::SidecarWrite));
        assert!(evidence.contains(&Evidence::ChildSpawned));
    }

    #[test]
    fn spawn_failed_is_direct_with_syscall_evidence() {
        let mut m = fresh_meta();
        m.transition_spawn_failed(EpochTimestamp::from_secs(1))
            .unwrap();
        let p = m.transition_provenance().unwrap();
        assert!(p.is_direct());
        let TransitionProvenance::Direct { evidence } = p else {
            unreachable!()
        };
        assert!(evidence.contains(&Evidence::SpawnFailedSyscall));
    }

    #[test]
    fn exited_is_direct_with_child_exit_observed() {
        let mut m = fresh_meta();
        m.transition_running(child()).unwrap();
        m.transition_exited(ExitReason::ExitedOk, EpochTimestamp::from_secs(2))
            .unwrap();
        let p = m.transition_provenance().unwrap();
        assert!(p.is_direct());
        let TransitionProvenance::Direct { evidence } = p else {
            unreachable!()
        };
        assert!(evidence.contains(&Evidence::ChildExitObserved));
    }

    #[test]
    fn sidecar_failed_from_running_keeps_child_and_is_direct() {
        let mut m = fresh_meta();
        m.transition_running(child()).unwrap();
        let other = ProcessIdentity {
            pid: std::num::NonZero::new(99).unwrap(),
            start_time_ns: 7,
        };
        m.transition_sidecar_failed(other, SidecarStep::OutputLog, EpochTimestamp::from_secs(2))
            .unwrap();
        let RunStatus::Exited { child: c, how, .. } = m.status() else {
            panic!("expected Exited, got {:?}", m.status());
        };
        assert_eq!(*c, child(), "Running's recorded child wins");
        assert_eq!(
            *how,
            ExitReason::SidecarFailed {
                step: SidecarStep::OutputLog
            }
        );
        let TransitionProvenance::Direct { evidence } = m.transition_provenance().unwrap() else {
            panic!("SidecarFailed is a direct sidecar write");
        };
        assert!(evidence.contains(&Evidence::SidecarWrite));
        assert!(evidence.contains(&Evidence::SupervisionFailed));
    }

    #[test]
    fn sidecar_failed_heals_from_starting_only_with_the_spawned_child() {
        let how = ExitReason::SidecarFailed {
            step: SidecarStep::StdinTransport,
        };
        let mut without = fresh_meta();
        assert!(
            without
                .heal_terminal_from_event(
                    HealedTerminal::Exited(how.clone()),
                    EpochTimestamp::from_secs(1),
                    None
                )
                .is_err(),
            "no child to record: shape mismatch"
        );
        let mut observed = fresh_meta();
        assert!(
            observed
                .heal_terminal_from_event(
                    HealedTerminal::Exited(ExitReason::ExitedOk),
                    EpochTimestamp::from_secs(1),
                    Some(child())
                )
                .is_err(),
            "an observed exit never comes from Starting"
        );
        let mut m = fresh_meta();
        m.heal_terminal_from_event(
            HealedTerminal::Exited(how.clone()),
            EpochTimestamp::from_secs(1),
            Some(child()),
        )
        .unwrap();
        assert_eq!(m.status().child(), Some(&child()));
        assert!(matches!(m.status(), RunStatus::Exited { how: h, .. } if *h == how));
    }

    #[test]
    fn orphan_killed_is_recorded_only_on_a_healed_sidecar_failed() {
        let failed = HealedTerminal::Exited(ExitReason::SidecarFailed {
            step: SidecarStep::ChildWait,
        });
        let mut direct = fresh_meta();
        direct.transition_running(child()).unwrap();
        direct
            .transition_sidecar_failed(
                child(),
                SidecarStep::ChildWait,
                EpochTimestamp::from_secs(1),
            )
            .unwrap();
        assert!(
            direct.record_healed_orphan_killed().is_err(),
            "the sidecar's own record was not healed"
        );
        let mut observed = fresh_meta();
        observed.transition_running(child()).unwrap();
        observed
            .heal_terminal_from_event(
                HealedTerminal::Exited(ExitReason::ExitedOk),
                EpochTimestamp::from_secs(1),
                None,
            )
            .unwrap();
        assert!(
            observed.record_healed_orphan_killed().is_err(),
            "an observed exit has no orphan"
        );
        let mut m = fresh_meta();
        m.heal_terminal_from_event(failed, EpochTimestamp::from_secs(1), Some(child()))
            .unwrap();
        m.record_healed_orphan_killed().unwrap();
        assert_eq!(
            m.transition_provenance(),
            Some(&TransitionProvenance::direct(&[
                Evidence::EventLogTerminal,
                Evidence::OrphanKilled
            ]))
        );
    }

    #[test]
    fn sidecar_failed_from_starting_records_the_spawned_child() {
        let mut m = fresh_meta();
        m.transition_sidecar_failed(
            child(),
            SidecarStep::StdinTransport,
            EpochTimestamp::from_secs(1),
        )
        .unwrap();
        assert_eq!(m.status().child(), Some(&child()));
        assert!(m.status().is_terminal());
    }

    #[test]
    fn sidecar_failed_is_rejected_once_terminal() {
        let mut m = fresh_meta();
        m.transition_running(child()).unwrap();
        m.transition_exited(ExitReason::ExitedOk, EpochTimestamp::from_secs(2))
            .unwrap();
        assert!(
            m.transition_sidecar_failed(
                child(),
                SidecarStep::ChildWait,
                EpochTimestamp::from_secs(3)
            )
            .is_err()
        );
    }

    #[test]
    fn transition_exited_refuses_sidecar_failed() {
        let mut m = fresh_meta();
        m.transition_running(child()).unwrap();
        let how = ExitReason::SidecarFailed {
            step: SidecarStep::ChildWait,
        };
        assert!(
            m.transition_exited(how, EpochTimestamp::from_secs(2))
                .is_err()
        );
        assert!(matches!(m.status(), RunStatus::Running { .. }));
    }

    #[test]
    fn dependency_failed_is_direct_with_dependency_evidence() {
        let mut m = fresh_meta();
        m.transition_dependency_failed(EpochTimestamp::from_secs(1), DepFailReason::Failed)
            .unwrap();
        let p = m.transition_provenance().unwrap();
        assert!(p.is_direct());
        let TransitionProvenance::Direct { evidence } = p else {
            unreachable!()
        };
        assert!(evidence.contains(&Evidence::DependencyFailed));
    }

    #[test]
    fn sidecar_lost_is_inferred_with_lock_and_non_terminal_evidence() {
        let mut m = fresh_meta();
        m.transition_running(child()).unwrap();
        m.reconcile_sidecar_lost(EpochTimestamp::from_secs(3), None)
            .unwrap();
        let p = m.transition_provenance().unwrap();
        assert!(p.is_inferred());
        let TransitionProvenance::Inferred { evidence } = p else {
            unreachable!()
        };
        assert!(evidence.contains(&Evidence::LockReleased));
        assert!(evidence.contains(&Evidence::NonTerminalMeta));
    }

    #[test]
    fn sidecar_lost_from_starting_records_the_breadcrumb_orphan() {
        let mut m = fresh_meta();
        let orphan = Orphan {
            child: child(),
            killed: true,
        };
        m.reconcile_sidecar_lost(EpochTimestamp::from_secs(1), Some(orphan))
            .unwrap();
        assert_eq!(m.status().child(), Some(&child()));
        let TransitionProvenance::Inferred { evidence } = m.transition_provenance().unwrap() else {
            panic!("SidecarLost is inferred");
        };
        assert!(evidence.contains(&Evidence::OrphanKilled));
    }

    #[test]
    fn sidecar_lost_without_a_kill_has_no_orphan_killed_evidence() {
        let mut m = fresh_meta();
        m.transition_running(child()).unwrap();
        let orphan = Orphan {
            child: child(),
            killed: false,
        };
        m.reconcile_sidecar_lost(EpochTimestamp::from_secs(1), Some(orphan))
            .unwrap();
        let TransitionProvenance::Inferred { evidence } = m.transition_provenance().unwrap() else {
            panic!("SidecarLost is inferred");
        };
        assert!(!evidence.contains(&Evidence::OrphanKilled));
    }

    #[test]
    fn sidecar_lost_from_starting_also_inferred() {
        let mut m = fresh_meta();
        m.reconcile_sidecar_lost(EpochTimestamp::from_secs(1), None)
            .unwrap();
        assert!(m.transition_provenance().unwrap().is_inferred());
    }
}
