//! What a pipe session's reader holds, published for `tendr status`.
//!
//! A child that buffers progress without a record terminator and a hung
//! child leave the same stale `output.log`; only the bytes waiting in the
//! sidecar's reader separate them. The sidecar publishes them in
//! `reader.json` in the session dir, not in `meta.json`, the lifecycle
//! record: it is rewritten (temp file, then rename) at most once per
//! [`PUBLISH_INTERVAL`] while the values change, once when capture starts and
//! once when it ends. `status` reads it best-effort.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The file's name in the session dir.
pub const FILE_NAME: &str = "reader.json";

/// The most often the sidecar rewrites the file.
pub const PUBLISH_INTERVAL: Duration = Duration::from_secs(1);

/// The file's contents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Bytes read from stdout and stderr that no record holds yet.
    pub pending_partial_bytes: u64,
    /// When the last record was written, or capture started (epoch seconds).
    pub last_record_at: f64,
    /// When these values were taken (epoch seconds).
    pub as_of: f64,
    /// Capture has ended: the values are final.
    #[serde(default)]
    pub ended: bool,
}

/// What `status` shows: [`Snapshot`] turned into ages.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub pending_partial_bytes: u64,
    pub last_record_age_s: f64,
    /// False once the sidecar no longer updates the values: capture ended or
    /// the sidecar is gone. The age is then measured at `as_of`, so neither
    /// value changes any more.
    pub live: bool,
}

impl Snapshot {
    /// The report for a reader whose sidecar is (`sidecar_live`) or is not
    /// still running, at `now`.
    #[must_use]
    pub fn report(&self, sidecar_live: bool, now: f64) -> Report {
        let live = sidecar_live && !self.ended;
        let at = if live {
            now.max(self.as_of)
        } else {
            self.as_of
        };
        Report {
            pending_partial_bytes: self.pending_partial_bytes,
            last_record_age_s: ((at - self.last_record_at).max(0.0) * 1000.0).round() / 1000.0,
            live,
        }
    }
}

/// Read a session's snapshot; `None` when absent or unreadable.
#[must_use]
pub fn read(session_dir: &Path) -> Option<Snapshot> {
    let bytes = std::fs::read(session_dir.join(FILE_NAME)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The values the capture threads update, shared with the publisher.
pub struct Telemetry {
    /// Pending bytes per stream: stdout, stderr.
    pending: [AtomicU64; 2],
    /// `last_record_at` in microseconds.
    last_record_us: AtomicU64,
    /// The last snapshot written, to skip unchanged rewrites.
    written: Mutex<Option<(u64, u64)>>,
}

impl Telemetry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: [AtomicU64::new(0), AtomicU64::new(0)],
            last_record_us: AtomicU64::new(now_us()),
            written: Mutex::new(None),
        }
    }

    /// A stream's reader now holds `bytes`.
    pub fn set_pending(&self, stream: usize, bytes: usize) {
        self.pending[stream].store(bytes as u64, Ordering::Relaxed);
    }

    /// A record was written.
    pub fn record_written(&self) {
        self.last_record_us.store(now_us(), Ordering::Relaxed);
    }

    /// Write the file if its values changed since the last write, or
    /// always when `ended`. Best effort: a failed write is tried again at
    /// the next call.
    pub fn publish(&self, session_dir: &Path, ended: bool) {
        let pending = self.pending.iter().map(|p| p.load(Ordering::Relaxed)).sum();
        let last = self.last_record_us.load(Ordering::Relaxed);
        let mut written = self
            .written
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !ended && *written == Some((pending, last)) {
            return;
        }
        let snapshot = Snapshot {
            pending_partial_bytes: pending,
            last_record_at: last as f64 / 1e6,
            as_of: now_us() as f64 / 1e6,
            ended,
        };
        let json = serde_json::to_vec(&snapshot).expect("Snapshot is always serializable");
        let tmp = session_dir.join(format!("{FILE_NAME}.tmp"));
        let ok = std::fs::write(&tmp, json)
            .and_then(|()| std::fs::rename(&tmp, session_dir.join(FILE_NAME)))
            .is_ok();
        if ok {
            *written = Some((pending, last));
        }
    }
}

impl Default for Telemetry {
    fn default() -> Self {
        Self::new()
    }
}

fn now_us() -> u64 {
    (crate::log::timestamp_secs() * 1e6) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(ended: bool) -> Snapshot {
        Snapshot {
            pending_partial_bytes: 7,
            last_record_at: 100.0,
            as_of: 110.0,
            ended,
        }
    }

    #[test]
    fn a_live_reader_ages_with_the_clock_and_a_dead_one_stops_at_as_of() {
        assert_eq!(snapshot(false).report(true, 130.0).last_record_age_s, 30.0);
        for (ended, sidecar) in [(true, true), (false, false), (true, false)] {
            let report = snapshot(ended).report(sidecar, 130.0);
            assert_eq!((report.last_record_age_s, report.live), (10.0, false));
        }
    }
}
