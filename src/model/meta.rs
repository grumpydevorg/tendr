use serde::{Deserialize, Deserializer, Serialize};

use super::ids::{EpochTimestamp, Generation, ProcessIdentity, RunId, SessionName};
use super::provenance::TransitionProvenance;
use super::pty::{PtyControl, PtyMeta, PtyRecording, RecordingState, RecordingStopReason};
use super::spec::LaunchSpec;
use super::state::RunStatus;

/// Persisted session metadata. Schema v1.
/// Written atomically to meta.json via temp file + rename.
///
/// Fields are private — construction only through `new_starting`,
/// mutation only through transition methods in `transition.rs`.
#[derive(Debug, Clone, Serialize)]
pub struct Meta {
    schema_version: u32,
    session: SessionName,
    run_id: RunId,
    generation: Generation,
    launch_spec: LaunchSpec,
    sidecar: ProcessIdentity,
    #[serde(flatten)]
    status: RunStatus,
    started_at: EpochTimestamp,
    restart_count: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pty: Option<PtyMeta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transition_provenance: Option<TransitionProvenance>,
}

impl Meta {
    pub const SCHEMA_VERSION: u32 = 1;

    /// Create initial meta for a new run in Starting state.
    pub fn new_starting(
        session: SessionName,
        run_id: RunId,
        generation: Generation,
        launch_spec: LaunchSpec,
        sidecar: ProcessIdentity,
        started_at: EpochTimestamp,
    ) -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            session,
            run_id,
            generation,
            launch_spec,
            sidecar,
            status: RunStatus::Starting,
            started_at,
            restart_count: 0,
            warnings: vec![],
            pty: None,
            transition_provenance: None,
        }
    }

    // --- Accessors ---

    #[must_use]
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    #[must_use]
    pub fn session(&self) -> &SessionName {
        &self.session
    }

    #[must_use]
    pub fn run_id(&self) -> RunId {
        self.run_id
    }

    #[must_use]
    pub fn generation(&self) -> Generation {
        self.generation
    }

    /// Compute canonical hash on demand. Never stale.
    #[must_use]
    pub fn launch_spec_hash(&self) -> String {
        self.launch_spec.canonical_hash()
    }

    #[must_use]
    pub fn launch_spec(&self) -> &LaunchSpec {
        &self.launch_spec
    }

    #[must_use]
    pub fn sidecar(&self) -> &ProcessIdentity {
        &self.sidecar
    }

    #[must_use]
    pub fn status(&self) -> &RunStatus {
        &self.status
    }

    #[must_use]
    pub fn started_at(&self) -> &EpochTimestamp {
        &self.started_at
    }

    #[must_use]
    pub fn restart_count(&self) -> u32 {
        self.restart_count
    }

    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn add_warning(&mut self, msg: String) {
        self.warnings.push(msg);
    }

    #[must_use]
    pub fn pty(&self) -> Option<&PtyMeta> {
        self.pty.as_ref()
    }

    pub fn set_pty(&mut self, pty_meta: PtyMeta) {
        self.pty = Some(pty_meta);
    }

    pub fn set_pty_control(&mut self, control: PtyControl) {
        if let Some(ref mut p) = self.pty {
            p.control = control;
        }
    }

    pub fn set_pty_recording(&mut self, recording: PtyRecording) {
        if let Some(ref mut p) = self.pty {
            p.recording = Some(recording);
        }
    }

    /// Mark a recording that is still `Recording` as `Stopped` for `reason`,
    /// with no known last sequence. For a recorder that can no longer report
    /// its own end. Returns whether it changed anything.
    pub fn stop_live_recording(&mut self, reason: RecordingStopReason) -> bool {
        let Some(recording) = self.pty.as_mut().and_then(|p| p.recording.as_mut()) else {
            return false;
        };
        if recording.state != RecordingState::Recording {
            return false;
        }
        recording.state = RecordingState::Stopped {
            last_recorded_sequence: None,
            last_synced_sequence: None,
            reason,
        };
        true
    }

    /// Provenance of the most recent lifecycle transition, if recorded.
    /// Older meta files written before this field existed return `None`.
    #[must_use]
    pub fn transition_provenance(&self) -> Option<&TransitionProvenance> {
        self.transition_provenance.as_ref()
    }

    // --- Mutable access for transition module only ---

    pub(super) fn status_mut(&mut self) -> &mut RunStatus {
        &mut self.status
    }

    pub(super) fn set_transition_provenance(&mut self, prov: TransitionProvenance) {
        self.transition_provenance = Some(prov);
    }
}

impl<'de> Deserialize<'de> for Meta {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            schema_version: u32,
            session: SessionName,
            run_id: RunId,
            generation: Generation,
            launch_spec: LaunchSpec,
            sidecar: ProcessIdentity,
            #[serde(flatten)]
            status: RunStatus,
            started_at: EpochTimestamp,
            restart_count: u32,
            #[serde(default)]
            warnings: Vec<String>,
            #[serde(default)]
            pty: Option<PtyMeta>,
            #[serde(default)]
            transition_provenance: Option<TransitionProvenance>,
        }

        let raw = Raw::deserialize(deserializer)?;
        if raw.schema_version != Meta::SCHEMA_VERSION {
            return Err(serde::de::Error::custom(format!(
                "unsupported schema version: expected {}, got {}",
                Meta::SCHEMA_VERSION,
                raw.schema_version
            )));
        }
        Ok(Meta {
            schema_version: raw.schema_version,
            session: raw.session,
            run_id: raw.run_id,
            generation: raw.generation,
            launch_spec: raw.launch_spec,
            sidecar: raw.sidecar,
            status: raw.status,
            started_at: raw.started_at,
            restart_count: raw.restart_count,
            warnings: raw.warnings,
            pty: raw.pty,
            transition_provenance: raw.transition_provenance,
        })
    }
}
