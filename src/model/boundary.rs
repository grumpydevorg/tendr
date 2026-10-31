//! Boundary descriptor — where a session runs, for legibility only.
//!
//! Tendr *describes* boundaries (host, container, VM, pod); it never manages
//! them. The boundary is authoritative in `LaunchSpec` / `meta.json`; lifecycle
//! events carry a denormalized immutable snapshot for historical analytics
//! (see docs/architecture/07-event-protocol.md §1).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The kind of execution boundary a session is declared to run within.
/// Open vocabulary — new kinds may be added without breaking callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BoundaryKind {
    Host,
    Container,
    Vm,
    Pod,
}

impl BoundaryKind {
    /// The wire/CLI token for this kind — matches the snake_case serde form,
    /// so `as_str()`, `Display`, and serialization all agree.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            BoundaryKind::Host => "host",
            BoundaryKind::Container => "container",
            BoundaryKind::Vm => "vm",
            BoundaryKind::Pod => "pod",
        }
    }
}

impl fmt::Display for BoundaryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single boundary: its kind and a user-supplied label (e.g. host name,
/// image tag, VM id). The label is opaque to Tendr.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Boundary {
    pub kind: BoundaryKind,
    pub label: String,
}

impl fmt::Display for Boundary {
    /// Reconstructs the `kind:label` CLI form — the inverse of `FromStr`, so a
    /// parsed `Boundary` round-trips (used by `--host` arg reconstruction).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind, self.label)
    }
}

/// A boundary and its ancestry. Flat vector, not recursive — simpler serde,
/// diffs, and partial updates. `parents[0]` is the immediate parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundaryContext {
    pub current: Boundary,
    pub parents: Vec<Boundary>,
}

/// Failure parsing a `kind:label` boundary token.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum BoundaryParseError {
    #[error("boundary must be KIND:LABEL (missing ':'), got: {0:?}")]
    MissingColon(String),
    #[error("boundary label cannot be empty, got: {0:?}")]
    EmptyLabel(String),
    #[error("unknown boundary kind {0:?} (expected host, container, vm, or pod)")]
    UnknownKind(String),
}

impl FromStr for BoundaryKind {
    type Err = BoundaryParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "host" => Ok(BoundaryKind::Host),
            "container" => Ok(BoundaryKind::Container),
            "vm" => Ok(BoundaryKind::Vm),
            "pod" => Ok(BoundaryKind::Pod),
            other => Err(BoundaryParseError::UnknownKind(other.to_owned())),
        }
    }
}

impl FromStr for Boundary {
    type Err = BoundaryParseError;

    /// Parse `kind:label`. Splits on the *first* colon only, so labels may
    /// themselves contain colons (e.g. `container:my-image:latest`).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (kind, label) = s
            .split_once(':')
            .ok_or_else(|| BoundaryParseError::MissingColon(s.to_owned()))?;
        if label.is_empty() {
            return Err(BoundaryParseError::EmptyLabel(s.to_owned()));
        }
        Ok(Boundary {
            kind: kind.parse()?,
            label: label.to_owned(),
        })
    }
}
