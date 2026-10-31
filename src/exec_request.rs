//! The exec request frame — the whole exec request as one versioned
//! JSON document, read from stdin (`exec --frame-from-stdin`).
//!
//! Local and remote exec share this schema: `--host` serializes the
//! frame and ships it over the SSH stdin channel, so the payload never
//! traverses a shell and the remote argv contains nothing
//! user-controlled (docs/architecture/06-transport-boundaries.md). The shape
//! is a self-contained params object, independent of how it is carried.

use serde::{Deserialize, Serialize};

use crate::model::ids::SessionName;

/// The frame version this binary reads and writes.
pub const EXEC_FRAME_VERSION: u32 = 1;

/// One exec request. `cmd` is argv — never a shell string. Unknown
/// fields are tolerated (consumers-ignore-unknown doctrine); unknown
/// versions are not (`FrameError::Version`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecRequestFrame {
    /// Schema version — must equal `EXEC_FRAME_VERSION`.
    pub v: u32,
    pub session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// The command as argv.
    pub cmd: Vec<String>,
    /// Timeout in seconds, enforced by the executing side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

/// Frame decode failures — all are usage errors (exit 2 at the CLI):
/// the frame is rejected before any side effect.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FrameError {
    #[error("invalid exec frame: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("unsupported exec frame version {0} (this tendr supports {EXEC_FRAME_VERSION})")]
    Version(u32),
    /// A structurally-decoded frame that fails a semantic invariant
    /// (bad session name, empty cmd) — the frame path is a public
    /// scripting surface, so these are caught here, not at runtime.
    #[error("invalid exec frame: {0}")]
    Invalid(String),
}

impl ExecRequestFrame {
    /// Decode and version-check one frame.
    ///
    /// # Errors
    /// `FrameError::Parse` on malformed JSON or missing required fields;
    /// `FrameError::Version` on a version this binary doesn't speak.
    pub fn from_json(bytes: &[u8]) -> Result<Self, FrameError> {
        let frame: Self = serde_json::from_slice(bytes)?;
        if frame.v != EXEC_FRAME_VERSION {
            return Err(FrameError::Version(frame.v));
        }
        // Semantic invariants, so an invalid session or empty cmd is a
        // frame error (exit 2, no side effect) rather than a runtime
        // failure after session lookup and lock.
        SessionName::new(&frame.session).map_err(|e| FrameError::Invalid(e.to_string()))?;
        if frame.cmd.is_empty() {
            return Err(FrameError::Invalid("no command specified".to_owned()));
        }
        Ok(frame)
    }

    /// Serialize for the wire.
    #[must_use]
    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("frame is plain data, always serializes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_hostile_bytes() {
        let frame = ExecRequestFrame {
            v: EXEC_FRAME_VERSION,
            session: "s1".to_owned(),
            namespace: None,
            cmd: vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "printf '%s' \"a\nb\" && echo 'it'\\''s' \\$HOME".to_owned(),
            ],
            timeout: Some(30),
        };
        let parsed = ExecRequestFrame::from_json(&frame.to_json()).unwrap();
        assert_eq!(parsed.cmd, frame.cmd, "argv bytes survive exactly");
        assert_eq!(parsed.session, "s1");
        assert_eq!(parsed.timeout, Some(30));
    }

    #[test]
    fn unknown_fields_are_tolerated() {
        let json = br#"{"v":1,"session":"s","cmd":["true"],"future_field":42}"#;
        let frame = ExecRequestFrame::from_json(json).unwrap();
        assert_eq!(frame.cmd, ["true"]);
    }

    #[test]
    fn wrong_version_is_rejected() {
        let json = br#"{"v":2,"session":"s","cmd":["true"]}"#;
        match ExecRequestFrame::from_json(json) {
            Err(FrameError::Version(2)) => {}
            other => panic!("expected version error, got {other:?}"),
        }
    }

    #[test]
    fn missing_required_field_is_parse_error() {
        let json = br#"{"v":1,"session":"s"}"#;
        assert!(matches!(
            ExecRequestFrame::from_json(json),
            Err(FrameError::Parse(_))
        ));
    }

    #[test]
    fn invalid_session_name_is_invalid_frame() {
        let json = br#"{"v":1,"session":"bad/name","cmd":["true"]}"#;
        match ExecRequestFrame::from_json(json) {
            Err(FrameError::Invalid(msg)) => assert!(msg.contains("session name")),
            other => panic!("expected invalid frame, got {other:?}"),
        }
    }

    #[test]
    fn empty_cmd_is_invalid_frame() {
        let json = br#"{"v":1,"session":"s","cmd":[]}"#;
        match ExecRequestFrame::from_json(json) {
            Err(FrameError::Invalid(msg)) => assert!(msg.contains("command")),
            other => panic!("expected invalid frame, got {other:?}"),
        }
    }
}
