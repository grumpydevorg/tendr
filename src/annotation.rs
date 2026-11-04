use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;

/// Maximum annotation line length (timestamp + tag + json + newline).
/// Sized to stay within common local-FS single-write atomicity assumptions.
pub const MAX_LINE: usize = 4096;

/// Maximum size for individual payload fields before truncation.
pub const MAX_FIELD_BYTES: usize = 3000;

/// Truncate a string to at most `max_bytes`, respecting char boundaries.
pub fn truncate_string(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

/// Write one JSONL annotation line to the log, enforcing the size cap.
/// Returns Ok(true) if written, Ok(false) if the line was too large and dropped.
pub fn write_annotation_line(log_path: &Path, payload: &serde_json::Value) -> io::Result<bool> {
    let line = serde_json::to_string(&crate::log::LogLine {
        ts: crate::log::timestamp_secs(),
        tag: "A".to_owned(),
        content: payload.clone(),
        terminator: None,
    })
    .expect("JSON serialization cannot fail")
        + "\n";

    if line.len() > MAX_LINE {
        return Ok(false);
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    file.write_all(line.as_bytes())?;
    Ok(true)
}
