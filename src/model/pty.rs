use serde::{Deserialize, Serialize};

/// Who currently controls input to a PTY session.
///
/// Slice one rules (no agent lease):
/// - start --pty → AgentControl
/// - attach → HumanControl (steals from AgentControl)
/// - human detach → AgentControl (always, no lease check)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PtyControl {
    /// Agent owns input — push is accepted.
    AgentControl,
    /// Human is attached — push is rejected, terminal relay is active.
    HumanControl,
}

/// PTY session metadata. Present only for PTY-enabled sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtyMeta {
    pub enabled: bool,
    pub control: PtyControl,
    /// The run's exact recording, when one was started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<PtyRecording>,
}

impl PtyMeta {
    pub fn new() -> Self {
        Self {
            enabled: true,
            control: PtyControl::AgentControl,
            recording: None,
        }
    }
}

/// A PTY run's recording policy and state, as the sidecar last reported them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtyRecording {
    /// The segment directory, relative to the session directory.
    pub dir: String,
    /// Whether input bytes are recorded. Output and geometry always are.
    pub input_recorded: bool,
    #[serde(flatten)]
    pub state: RecordingState,
}

/// Where a recording stands. Sequences are recording sequence numbers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state")]
pub enum RecordingState {
    /// Records are being appended.
    Recording,
    /// The run ended with everything it produced recorded.
    Complete {
        last_recorded_sequence: Option<u64>,
        last_synced_sequence: Option<u64>,
    },
    /// Recording ended before the run did. Nothing after
    /// `last_recorded_sequence` is recorded.
    Stopped {
        last_recorded_sequence: Option<u64>,
        /// Known once the run has ended.
        last_synced_sequence: Option<u64>,
        /// `reason`, and for a storage failure its `error`.
        #[serde(flatten)]
        reason: RecordingStopReason,
    },
}

/// Why a recording stopped early. On the wire, the `reason` field names the
/// variant and only [`RecordingStopReason::WriteFailed`] carries an `error`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum RecordingStopReason {
    /// The run reached its recording size limit.
    SizeLimit,
    /// Storage rejected a write, sync, or segment publication.
    WriteFailed {
        /// The storage error.
        error: String,
    },
    /// Storage fell too far behind the output.
    BacklogFull,
    /// Storage had not caught up when the run ended.
    Stalled,
    /// The sidecar was lost: reconciliation ended the recording, not the
    /// recorder. The last recorded sequence is not known.
    SidecarLost,
}

impl Default for PtyMeta {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stopped(reason: RecordingStopReason) -> PtyRecording {
        PtyRecording {
            dir: "recording/r".to_owned(),
            input_recorded: false,
            state: RecordingState::Stopped {
                last_recorded_sequence: Some(7),
                last_synced_sequence: None,
                reason,
            },
        }
    }

    /// The wire shape is unchanged: a flat object whose `reason` names the stop
    /// and whose `error` appears only for a storage failure.
    #[test]
    fn a_stopped_recording_keeps_its_flat_wire_shape() {
        let failed = stopped(RecordingStopReason::WriteFailed {
            error: "StorageFull".to_owned(),
        });
        let json = serde_json::to_value(&failed).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "dir": "recording/r",
                "input_recorded": false,
                "state": "Stopped",
                "last_recorded_sequence": 7,
                "last_synced_sequence": null,
                "reason": "write_failed",
                "error": "StorageFull",
            })
        );
        assert_eq!(
            serde_json::from_value::<PtyRecording>(json).unwrap(),
            failed
        );

        let limit = stopped(RecordingStopReason::SizeLimit);
        let json = serde_json::to_value(&limit).unwrap();
        assert_eq!(json["reason"], "size_limit");
        assert!(json.get("error").is_none(), "{json}");
        assert_eq!(serde_json::from_value::<PtyRecording>(json).unwrap(), limit);
    }

    /// A storage failure without its error, and an unknown reason, no longer
    /// decode.
    #[test]
    fn a_write_failure_without_its_error_does_not_decode() {
        let mut json = serde_json::to_value(stopped(RecordingStopReason::WriteFailed {
            error: "StorageFull".to_owned(),
        }))
        .unwrap();
        json.as_object_mut().unwrap().remove("error");
        assert!(serde_json::from_value::<PtyRecording>(json).is_err());

        let mut json = serde_json::to_value(stopped(RecordingStopReason::Stalled)).unwrap();
        json["reason"] = "gone_fishing".into();
        assert!(serde_json::from_value::<PtyRecording>(json).is_err());
    }
}
