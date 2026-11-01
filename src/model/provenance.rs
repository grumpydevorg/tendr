use serde::{Deserialize, Serialize};

/// Compact typed evidence for a lifecycle transition.
///
/// Variants are added as new transition paths emerge. Existing variants
/// are stable wire identifiers; do not rename without a schema bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Evidence {
    /// The sidecar wrote this state from its supervisory loop.
    SidecarWrite,
    /// The child process was successfully spawned.
    ChildSpawned,
    /// The sidecar observed the child exit (wait/waitpid returned).
    ChildExitObserved,
    /// The child failed to spawn (syscall or platform error).
    SpawnFailedSyscall,
    /// A PTY session's private attach socket could not be bound, so the child
    /// was never spawned.
    AttachSocketFailed,
    /// A declared `--after` dependency did not satisfy.
    DependencyFailed,
    /// Reconciliation: the session lock was released without a terminal write.
    LockReleased,
    /// Reconciliation: meta.json shows a non-terminal status with no live writer.
    NonTerminalMeta,
    /// Reconciliation: the sidecar's own terminal event was found in the
    /// event log; meta was healed from it (spec §3.6).
    EventLogTerminal,
    /// The sidecar failed while supervising, killed the child, and recorded
    /// `SidecarFailed` itself.
    SupervisionFailed,
    /// Reconciliation: the child a gone sidecar left behind (a lost sidecar's,
    /// or one whose healed `SidecarFailed` record names it) was still alive,
    /// its identity verified, and it was killed.
    OrphanKilled,
}

/// Provenance of a lifecycle transition.
///
/// `Direct` means an authoritative writer (the sidecar) recorded an
/// observation. `Inferred` means a reconciliation path concluded the
/// transition from indirect evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TransitionProvenance {
    Direct { evidence: Vec<Evidence> },
    Inferred { evidence: Vec<Evidence> },
}

impl TransitionProvenance {
    #[must_use]
    pub fn direct(evidence: &[Evidence]) -> Self {
        Self::Direct {
            evidence: evidence.to_vec(),
        }
    }

    #[must_use]
    pub fn inferred(evidence: &[Evidence]) -> Self {
        Self::Inferred {
            evidence: evidence.to_vec(),
        }
    }

    #[must_use]
    pub fn is_direct(&self) -> bool {
        matches!(self, Self::Direct { .. })
    }

    #[must_use]
    pub fn is_inferred(&self) -> bool {
        matches!(self, Self::Inferred { .. })
    }
}
