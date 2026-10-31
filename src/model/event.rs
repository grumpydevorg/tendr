//! Event envelope — spec §1 of docs/architecture/07-event-protocol.md.
//!
//! One JSON object per line in a session's `events/*.jsonl` segments.
//! Fields outside `data` are stamped by the tendr binary (trusted tier).

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

use super::ids::{Namespace, RunId, SessionName, Source};

/// Envelope version. Additive fields never bump it (spec §1).
pub const ENVELOPE_VERSION: u32 = 1;

/// Kind prefixes whose payload schemas tendr itself owns. Rejected at
/// argument validation of user-supplied kinds (`Kind::new_user`); tendr's
/// internal call sites are the only writers of reserved kinds. `hook.` is
/// deliberately unreserved.
pub const RESERVED_KIND_PREFIXES: [&str; 9] = [
    "run.",
    "log.",
    "exec.",
    "session.",
    "pty.",
    "callback.",
    "segment.",
    "cursor.",
    "tendr.",
];

/// A UUIDv7 event identifier (event id, writer id, block id, parent id).
/// Serialization is the transparent UUID string; deserialization enforces v7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Uuid7(uuid::Uuid);

impl Uuid7 {
    #[must_use]
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7())
    }

    #[must_use]
    pub fn as_uuid(&self) -> uuid::Uuid {
        self.0
    }
}

impl Default for Uuid7 {
    fn default() -> Self {
        Self::new()
    }
}

impl From<RunId> for Uuid7 {
    fn from(run_id: RunId) -> Self {
        // RunId is v7 by construction, so the invariant holds.
        Self(run_id.as_uuid())
    }
}

impl fmt::Display for Uuid7 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Error)]
pub enum Uuid7Error {
    #[error("invalid UUID: {0}")]
    Invalid(#[from] uuid::Error),
    #[error("expected UUID v7, got v{0}")]
    WrongVersion(usize),
}

impl FromStr for Uuid7 {
    type Err = Uuid7Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let uuid = uuid::Uuid::parse_str(s)?;
        if uuid.get_version_num() != 7 {
            return Err(Uuid7Error::WrongVersion(uuid.get_version_num()));
        }
        Ok(Self(uuid))
    }
}

impl Serialize for Uuid7 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Uuid7 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let uuid = uuid::Uuid::deserialize(deserializer)?;
        if uuid.get_version_num() != 7 {
            return Err(serde::de::Error::custom(format!(
                "expected UUID v7, got v{}",
                uuid.get_version_num()
            )));
        }
        Ok(Self(uuid))
    }
}

/// RFC 3339 UTC timestamp with exactly 6 fractional digits and `Z`.
/// Fixed-width (27 bytes), so lexicographic order is chronological.
/// Stamped at occurrence time by the writer (spec §1 `ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventTimestamp {
    secs: u64,
    micros: u32,
}

impl EventTimestamp {
    #[must_use]
    pub fn now() -> Self {
        let d = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Self {
            secs: d.as_secs(),
            micros: d.subsec_micros(),
        }
    }

    /// Construct from epoch seconds and microseconds (`micros < 1_000_000`).
    #[must_use]
    pub fn from_parts(secs: u64, micros: u32) -> Self {
        debug_assert!(micros < 1_000_000);
        Self { secs, micros }
    }

    #[must_use]
    pub fn epoch_secs(&self) -> u64 {
        self.secs
    }

    #[must_use]
    pub fn epoch_micros(&self) -> u64 {
        self.secs * 1_000_000 + u64::from(self.micros)
    }

    /// Convert `output.log`'s f64 epoch-seconds to the envelope timestamp
    /// (spec §5.1 log projection). Rounded to the nearest microsecond so
    /// f64 representation noise below 1 µs never shifts the value.
    #[must_use]
    pub fn from_epoch_secs_f64(secs: f64) -> Self {
        let total_micros = (secs * 1e6).round().max(0.0) as u64;
        Self {
            secs: total_micros / 1_000_000,
            micros: (total_micros % 1_000_000) as u32,
        }
    }

    /// Parse a user-supplied RFC 3339 UTC timestamp (`--since`): `Z` suffix
    /// required, 0–9 fractional digits accepted and normalized to the strict
    /// 6-digit envelope form (extra digits truncate).
    ///
    /// # Errors
    /// Returns `TimestampParseError` for non-UTC offsets or malformed input.
    pub fn parse_flexible(s: &str) -> Result<Self, TimestampParseError> {
        let err = || TimestampParseError(s.to_owned());
        let rest = s.strip_suffix('Z').ok_or_else(err)?;
        let (secs_part, fraction) = match rest.split_once('.') {
            Some((sp, f)) => (sp, f),
            None => (rest, ""),
        };
        if fraction.len() > 9 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
            return Err(err());
        }
        let mut micros = *b"000000";
        for (slot, b) in micros.iter_mut().zip(fraction.bytes()) {
            *slot = b;
        }
        let micros = std::str::from_utf8(&micros).expect("digits are ASCII");
        format!("{secs_part}.{micros}Z").parse()
    }
}

/// Days since 1970-01-01 → (year, month, day). Hinnant's civil_from_days.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (year, month, day) → days since 1970-01-01. Hinnant's days_from_civil.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let doy = (153 * u64::from(if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + u64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

impl fmt::Display for EventTimestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let days = (self.secs / 86_400) as i64;
        let rem = self.secs % 86_400;
        let (y, mo, d) = civil_from_days(days);
        write!(
            f,
            "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}.{:06}Z",
            rem / 3600,
            (rem % 3600) / 60,
            rem % 60,
            self.micros
        )
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("invalid event timestamp (expected YYYY-MM-DDTHH:MM:SS.ffffffZ): {0}")]
pub struct TimestampParseError(String);

impl FromStr for EventTimestamp {
    type Err = TimestampParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || TimestampParseError(s.to_owned());
        let b = s.as_bytes();
        if b.len() != 27
            || b[4] != b'-'
            || b[7] != b'-'
            || b[10] != b'T'
            || b[13] != b':'
            || b[16] != b':'
            || b[19] != b'.'
            || b[26] != b'Z'
        {
            return Err(err());
        }
        let num = |range: std::ops::Range<usize>| -> Result<u64, TimestampParseError> {
            let part = &s[range];
            if !part.bytes().all(|c| c.is_ascii_digit()) {
                return Err(err());
            }
            part.parse::<u64>().map_err(|_| err())
        };
        let year = num(0..4)? as i64;
        let month = num(5..7)? as u32;
        let day = num(8..10)? as u32;
        let hour = num(11..13)?;
        let min = num(14..16)?;
        let sec = num(17..19)?;
        let micros = num(20..26)? as u32;
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return Err(err());
        }
        if hour > 23 || min > 59 || sec > 59 {
            return Err(err());
        }
        let days = days_from_civil(year, month, day);
        // Reject impossible dates (e.g. Feb 30) by round-tripping.
        if civil_from_days(days) != (year, month, day) {
            return Err(err());
        }
        if days < 0 {
            return Err(err()); // pre-epoch timestamps are not representable
        }
        let secs = days as u64 * 86_400 + hour * 3600 + min * 60 + sec;
        Ok(Self { secs, micros })
    }
}

impl Serialize for EventTimestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for EventTimestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Validated event kind: routing + payload-schema id (spec §1).
/// Grammar = the shipped `Source` grammar plus `_` (the spec's own worked
/// examples use kinds like `hook.post_tool_use`). Open vocabulary — the
/// reserved-prefix check applies only to user-supplied kinds.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Kind(String);

#[derive(Debug, Error)]
pub enum KindError {
    #[error("kind cannot be empty")]
    Empty,
    #[error("kind must contain at least one '.'")]
    NoDot,
    #[error("kind contains invalid character '{0}' (allowed: ASCII alphanumeric, '.', '-', '_')")]
    InvalidChar(char),
    #[error("kind has empty segment (leading, trailing, or consecutive dots)")]
    EmptySegment,
    #[error("kind too long (max {MAX_KIND_LEN} bytes)")]
    TooLong,
    #[error("kind prefix '{0}' is reserved to tendr-owned schemas")]
    ReservedPrefix(&'static str),
}

const MAX_KIND_LEN: usize = 128;

impl Kind {
    /// Grammar-only validation — for tendr's internal call sites (which are
    /// the only writers of reserved kinds) and for deserialization.
    ///
    /// # Errors
    /// Returns `KindError` on grammar violations.
    pub fn new(s: &str) -> Result<Self, KindError> {
        Self::validate_grammar(s)?;
        Ok(Self(s.to_owned()))
    }

    /// Validate a user-supplied kind (`emit --kind`, `wrap --event`):
    /// grammar plus reserved-prefix rejection.
    ///
    /// # Errors
    /// Returns `KindError`, including `ReservedPrefix` for tendr-owned prefixes.
    pub fn new_user(s: &str) -> Result<Self, KindError> {
        Self::validate_grammar(s)?;
        if let Some(prefix) = RESERVED_KIND_PREFIXES
            .iter()
            .find(|prefix| s.starts_with(**prefix))
        {
            return Err(KindError::ReservedPrefix(prefix));
        }
        Ok(Self(s.to_owned()))
    }

    fn validate_grammar(s: &str) -> Result<(), KindError> {
        if s.is_empty() {
            return Err(KindError::Empty);
        }
        if s.len() > MAX_KIND_LEN {
            return Err(KindError::TooLong);
        }
        if let Some(c) = s
            .chars()
            .find(|c| !c.is_ascii_alphanumeric() && *c != '.' && *c != '-' && *c != '_')
        {
            return Err(KindError::InvalidChar(c));
        }
        if !s.contains('.') {
            return Err(KindError::NoDot);
        }
        if s.starts_with('.') || s.ends_with('.') || s.contains("..") {
            return Err(KindError::EmptySegment);
        }
        Ok(())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for Kind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Kind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::validate_grammar(&s).map_err(serde::de::Error::custom)?;
        Ok(Self(s))
    }
}

/// Reference to a spilled oversize payload (spec §3.4). When present, `data`
/// is a ≤4 KiB preview and the envelope's `truncated` is `true`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataRef {
    /// Session-dir-relative path, e.g. `events/blobs/<sha256>`.
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub media_type: String,
}

fn validate_v1<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
    let v = u32::deserialize(deserializer)?;
    if v != ENVELOPE_VERSION {
        return Err(serde::de::Error::custom(format!(
            "unsupported envelope version: expected {ENVELOPE_VERSION}, got {v}"
        )));
    }
    Ok(v)
}

/// One event: a single JSONL line in a session's `events/` segment.
/// Field order here is the serialization order and matches the spec's
/// worked examples. Consumers MUST ignore unknown fields (serde's default).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    #[serde(deserialize_with = "validate_v1")]
    pub v: u32,
    pub id: Uuid7,
    pub ts: EventTimestamp,
    pub kind: Kind,
    pub namespace: Namespace,
    pub session: SessionName,
    pub run_id: RunId,
    #[serde(rename = "gen", default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    pub writer: Uuid7,
    pub seq: u64,
    pub source: Source,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_id: Option<Uuid7>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<Uuid7>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_ref: Option<DataRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
}
