//! When a long-running child is ready to use: the condition `start` was given
//! (`--ready-when-output`, `--ready-when-port`, `--ready-within`), and what
//! the sidecar observed (`meta.json` `ready`). Tendr does not decide what
//! "ready" means for any program: the caller names the condition.

use serde::{Deserialize, Serialize};

use super::ids::EpochTimestamp;

/// A ready condition and its optional deadline, as the launch spec holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyCheck {
    pub when: ReadyWhen,
    /// Milliseconds after the run is `Running` by which the condition must
    /// hold; past it the miss is recorded and the child keeps running. No
    /// deadline when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within_ms: Option<u64>,
}

/// What makes the run ready.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadyWhen {
    /// A line of the child's output matches this regular expression.
    Output(String),
    /// A TCP connection to this port on the session's host succeeds.
    Port(u16),
}

/// Why a ready condition is refused.
#[derive(Debug, thiserror::Error)]
pub enum ReadyCheckError {
    #[error("invalid --ready-when-output regex: {0}")]
    Regex(#[from] regex::Error),
    #[error("--ready-when-port must be 1-65535")]
    Port,
}

impl ReadyWhen {
    /// Check the condition as `start` and the remote frame accept it.
    ///
    /// # Errors
    /// [`ReadyCheckError`] for a regex that does not compile or port 0.
    pub fn validate(&self) -> Result<(), ReadyCheckError> {
        match self {
            Self::Output(pattern) => output_regex(pattern).map(drop),
            Self::Port(0) => Err(ReadyCheckError::Port),
            Self::Port(_) => Ok(()),
        }
    }
}

/// The regex `--ready-when-output` matches output lines with. Lines are
/// bytes: output that is not UTF-8 still matches what it contains.
///
/// # Errors
/// [`ReadyCheckError::Regex`] when `pattern` does not compile.
pub fn output_regex(pattern: &str) -> Result<regex::bytes::Regex, ReadyCheckError> {
    Ok(regex::bytes::Regex::new(pattern)?)
}

/// What the sidecar recorded about a run's ready condition, in `meta.json`
/// as `ready`. Absent until one of these happens, and never changes after.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state")]
pub enum ReadyState {
    /// The condition held at `at` (`run.ready`).
    Ready { at: EpochTimestamp },
    /// The deadline passed at `at` first (`run.ready_missed`). The run kept
    /// going, and the condition is no longer checked.
    Missed { at: EpochTimestamp },
}
