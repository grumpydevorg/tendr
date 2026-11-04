use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use super::boundary::BoundaryContext;
use super::ids::{RunId, SessionName};
use super::ready::ReadyCheck;

fn is_false(b: &bool) -> bool {
    !b
}

/// How stdin is provided to the child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StdinMode {
    /// Named pipe created for push.
    Pipe,
    /// No stdin — /dev/null.
    None,
}

/// How the session's child I/O is wired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum IoMode {
    /// Pipes: stdout/stderr captured separately, stdin via FIFO if enabled.
    #[default]
    Pipe,
    /// Pseudo-terminal: merged I/O, interactive terminal.
    Pty,
}

/// What a PTY run's exact recording holds
/// (`docs/architecture/08-pty-recording-format.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RecordingPolicy {
    /// Output bytes and applied window sizes.
    #[default]
    Output,
    /// Output, window sizes, and the input bytes written to the terminal
    /// (`start --record-input`).
    OutputAndInput,
    /// No recording (`start --no-record`).
    Off,
}

impl RecordingPolicy {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// What ends a pipe session's `output.log` record besides the size cap.
/// PTY sessions ignore it: their capture writes per read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum LogDelimiter {
    /// A line feed (CRLF counts as one).
    #[default]
    Newline,
    /// A line feed or a carriage return, so progress updates arrive at once.
    NewlineOrCr,
}

impl LogDelimiter {
    #[must_use]
    pub fn is_newline(&self) -> bool {
        *self == Self::Newline
    }
}

/// What exec protocol the session speaks. Determined at start time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecTarget {
    /// Exec not supported on this session.
    None,
    /// POSIX shell (bash, sh, zsh). Uses unix_frame.
    PosixShell,
    /// PowerShell (pwsh, powershell.exe). Uses powershell_frame.
    PowerShell,
    /// Python REPL. Uses side-channel result files, supports PTY.
    PythonRepl,
    /// DuckDB SQL. JSON results via stdout, sentinel completion, pipe transport.
    DuckDb,
    /// A line-oriented JSON process: one JSON object per stdin line, one JSON
    /// reply per request on stdout, matched by the `id` tendr adds. Pipe
    /// transport only; never inferred.
    Ndjson,
}

/// A dependency on another session's specific execution.
/// Binds to run_id, not session name — safe against --replace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyBinding {
    pub session: SessionName,
    pub run_id: RunId,
}

/// The full identity of a run. Hashed for idempotent matching.
/// Argv must be non-empty (validated on construction and deserialization).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LaunchSpec {
    argv: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub env: BTreeMap<String, String>,
    /// Names removed from the inherited environment before `env` is applied.
    /// Omitted when empty so existing specs keep their canonical hash.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub unset_env: BTreeSet<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub after: Vec<DependencyBinding>,
    #[serde(skip_serializing_if = "is_false", default)]
    pub after_any_exit: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub on_exit: Vec<String>,
    pub stdin_mode: StdinMode,
    #[serde(default)]
    pub io_mode: IoMode,
    pub exec_target: ExecTarget,
    /// Declared execution boundary (host/container/vm/pod). User-supplied,
    /// authoritative here and in `meta.json`. Omitted when absent so specs
    /// without a boundary hash identically to pre-feature specs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary: Option<BoundaryContext>,
    /// What a PTY run records. Omitted at the default so specs that record
    /// output only hash identically to pre-feature specs.
    #[serde(default, skip_serializing_if = "RecordingPolicy::is_default")]
    pub recording: RecordingPolicy,
    /// `--ready-when-*`: when the run counts as ready. Omitted when absent so
    /// specs without one hash as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready: Option<ReadyCheck>,
    /// `--after-ready`: dependencies to be ready before the child starts,
    /// bound to run ids as `after` is. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after_ready: Vec<DependencyBinding>,
    /// What ends a pipe session's log record. Omitted at the default so
    /// existing specs keep their canonical hash.
    #[serde(default, skip_serializing_if = "LogDelimiter::is_newline")]
    pub log_delimiter: LogDelimiter,
}

#[derive(Debug, thiserror::Error)]
#[error("argv cannot be empty")]
pub struct EmptyArgvError;

impl LaunchSpec {
    pub fn new(argv: Vec<String>) -> Result<Self, EmptyArgvError> {
        if argv.is_empty() {
            return Err(EmptyArgvError);
        }
        Ok(Self {
            argv,
            cwd: None,
            env: BTreeMap::new(),
            unset_env: BTreeSet::new(),
            timeout_s: None,
            after: vec![],
            after_any_exit: false,
            namespace: None,
            on_exit: vec![],
            stdin_mode: StdinMode::None,
            io_mode: IoMode::Pipe,
            exec_target: ExecTarget::None,
            boundary: None,
            recording: RecordingPolicy::Output,
            ready: None,
            after_ready: vec![],
            log_delimiter: LogDelimiter::Newline,
        })
    }

    #[must_use]
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    /// Canonical SHA-256 hash of the launch spec.
    /// Uses sorted JSON serialization for determinism (BTreeMap is already sorted).
    #[must_use]
    pub fn canonical_hash(&self) -> String {
        let json = serde_json::to_string(self).expect("LaunchSpec is always serializable");
        let hash = Sha256::digest(json.as_bytes());
        format!("sha256:{hash:x}")
    }
}

impl<'de> Deserialize<'de> for LaunchSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            argv: Vec<String>,
            cwd: Option<PathBuf>,
            #[serde(default)]
            env: BTreeMap<String, String>,
            #[serde(default)]
            unset_env: BTreeSet<String>,
            timeout_s: Option<u64>,
            #[serde(default)]
            after: Vec<DependencyBinding>,
            #[serde(default)]
            after_any_exit: bool,
            namespace: Option<String>,
            #[serde(default)]
            on_exit: Vec<String>,
            stdin_mode: StdinMode,
            #[serde(default)]
            io_mode: IoMode,
            exec_target: ExecTarget,
            #[serde(default)]
            boundary: Option<BoundaryContext>,
            #[serde(default)]
            recording: RecordingPolicy,
            ready: Option<ReadyCheck>,
            #[serde(default)]
            after_ready: Vec<DependencyBinding>,
            #[serde(default)]
            log_delimiter: LogDelimiter,
        }

        let raw = Raw::deserialize(deserializer)?;
        if raw.argv.is_empty() {
            return Err(serde::de::Error::custom("argv cannot be empty"));
        }
        Ok(Self {
            argv: raw.argv,
            cwd: raw.cwd,
            env: raw.env,
            unset_env: raw.unset_env,
            timeout_s: raw.timeout_s,
            after: raw.after,
            after_any_exit: raw.after_any_exit,
            namespace: raw.namespace,
            on_exit: raw.on_exit,
            stdin_mode: raw.stdin_mode,
            io_mode: raw.io_mode,
            exec_target: raw.exec_target,
            boundary: raw.boundary,
            recording: raw.recording,
            ready: raw.ready,
            after_ready: raw.after_ready,
            log_delimiter: raw.log_delimiter,
        })
    }
}
