use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::Path;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Query parameters for filtering log output.
#[derive(Default)]
pub struct LogQuery {
    pub tail: Option<usize>,
    pub since_us: Option<u64>,
    pub raw: bool,
}

/// One JSONL line from `output.log`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogLine {
    /// Epoch seconds with sub-second precision.
    pub ts: f64,
    /// `"O"` stdout, `"E"` stderr, `"A"` annotation.
    pub tag: String,
    /// String content for O/E, structured JSON for A.
    pub content: serde_json::Value,
}

impl LogLine {
    #[must_use]
    pub fn format_raw(&self) -> String {
        match &self.content {
            serde_json::Value::String(s) => s.clone(),
            other => serde_json::to_string(other).unwrap_or_default(),
        }
    }

    #[must_use]
    pub fn content_text(&self) -> Option<&str> {
        self.content.as_str()
    }

    #[must_use]
    pub fn timestamp_us(&self) -> u64 {
        (self.ts * 1_000_000.0) as u64
    }
}

/// Return current time as epoch seconds with microsecond-ish precision.
#[must_use]
pub fn timestamp_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_secs_f64()
}

/// Read and filter a log file, writing matching lines to `out`.
///
/// Returns the number of lines written. Malformed lines are silently skipped.
pub fn query_log(path: &Path, query: &LogQuery, out: &mut dyn Write) -> io::Result<usize> {
    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);

    let tail_n = query.tail;
    let mut ring: VecDeque<LogLine> = VecDeque::with_capacity(tail_n.unwrap_or(0));
    let mut count = 0usize;

    for raw_line in reader.lines() {
        let raw_line = raw_line?;
        let Some(parsed) = serde_json::from_str::<LogLine>(&raw_line).ok() else {
            continue;
        };

        if !matches_query(&parsed, query) {
            continue;
        }

        if let Some(n) = tail_n {
            if n > 0 {
                if ring.len() == n {
                    ring.pop_front();
                }
                ring.push_back(parsed);
            }
        } else {
            write_line(out, &parsed, query.raw)?;
            count += 1;
        }
    }

    for line in &ring {
        write_line(out, line, query.raw)?;
        count += 1;
    }

    Ok(count)
}

fn matches_query(line: &LogLine, query: &LogQuery) -> bool {
    if let Some(threshold) = query.since_us {
        if line.timestamp_us() < threshold {
            return false;
        }
    }
    true
}

fn write_line(out: &mut dyn Write, line: &LogLine, raw: bool) -> io::Result<()> {
    if raw {
        writeln!(out, "{}", line.format_raw())
    } else {
        writeln!(
            out,
            "{}",
            serde_json::to_string(line).expect("JSON serialization cannot fail")
        )
    }
}

/// Follow a log file, writing new lines as they appear.
///
/// Waits for the file to exist, then tails it. Applies since/raw filters
/// from the query. If neither `tail` nor `since_us` is set, seeks to end
/// of file (only showing new lines). Polls every 100ms and returns `Ok(())`
/// when `should_stop` returns true.
pub fn follow_log<F>(
    path: &Path,
    query: &LogQuery,
    out: &mut dyn Write,
    should_stop: F,
) -> io::Result<()>
where
    F: Fn() -> bool,
{
    while !path.exists() {
        if should_stop() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }

    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file);

    if query.tail.is_none() && query.since_us.is_none() {
        reader.seek(SeekFrom::End(0))?;
    }

    if let Some(n) = query.tail {
        let mut ring: VecDeque<LogLine> = VecDeque::with_capacity(n);
        let mut buf = String::new();
        loop {
            buf.clear();
            let bytes = reader.read_line(&mut buf)?;
            if bytes == 0 {
                break;
            }
            let trimmed = buf.trim_end_matches('\n').trim_end_matches('\r');
            let Some(parsed) = serde_json::from_str::<LogLine>(trimmed).ok() else {
                continue;
            };
            if !matches_query(&parsed, query) {
                continue;
            }
            if n > 0 {
                if ring.len() == n {
                    ring.pop_front();
                }
                ring.push_back(parsed);
            }
        }
        for line in &ring {
            write_line(out, line, query.raw)?;
            out.flush()?;
        }
    }

    let mut buf = String::new();
    loop {
        buf.clear();
        let bytes = reader.read_line(&mut buf)?;
        if bytes == 0 {
            if should_stop() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(100));
            continue;
        }

        let trimmed = buf.trim_end_matches('\n').trim_end_matches('\r');
        let Some(parsed) = serde_json::from_str::<LogLine>(trimmed).ok() else {
            continue;
        };

        if !matches_query(&parsed, query) {
            continue;
        }

        write_line(out, &parsed, query.raw)?;
        out.flush()?;
    }
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_micros() as u64
}

/// Parse a "since" value into a microsecond epoch threshold.
///
/// Accepts:
/// - Duration suffixes: `"30s"`, `"5m"`, `"2h"`, `"1d"` (relative to now)
/// - Plain epoch seconds: `"1000000"` (converted to microseconds)
pub fn parse_since(value: &str) -> Result<u64, String> {
    if value.is_empty() {
        return Err("empty value".to_owned());
    }

    let last = value.as_bytes()[value.len() - 1];
    let multiplier = match last {
        b's' => Some(1_000_000u64),
        b'm' => Some(60_000_000u64),
        b'h' => Some(3_600_000_000u64),
        b'd' => Some(86_400_000_000u64),
        _ => None,
    };

    if let Some(mult) = multiplier {
        let num_str = &value[..value.len() - 1];
        let n: u64 = num_str
            .parse()
            .map_err(|_| format!("invalid duration number: {num_str:?}"))?;
        let duration_us = n
            .checked_mul(mult)
            .ok_or_else(|| format!("duration overflow: {value:?}"))?;
        return Ok(now_us().saturating_sub(duration_us));
    }

    let secs: u64 = value
        .parse()
        .map_err(|_| format!("invalid since value: {value:?}"))?;
    secs.checked_mul(1_000_000)
        .ok_or_else(|| format!("epoch overflow: {value:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::HANG;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct SignallingWriter {
        bytes: Vec<u8>,
        needle: &'static [u8],
        seen: Arc<AtomicBool>,
    }

    impl SignallingWriter {
        fn new(needle: &'static [u8], seen: Arc<AtomicBool>) -> Self {
            Self {
                bytes: Vec::new(),
                needle,
                seen,
            }
        }
    }

    impl std::io::Write for SignallingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.bytes.extend_from_slice(buf);
            if self
                .bytes
                .windows(self.needle.len())
                .any(|window| window == self.needle)
            {
                self.seen.store(true, Ordering::Release);
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn write_test_log(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("output.log");
        let mut f = std::fs::File::create(&path).unwrap();
        for line in [
            serde_json::json!({"ts":1000000.0,"tag":"O","content":"line one"}),
            serde_json::json!({"ts":1000001.0,"tag":"O","content":"line two"}),
            serde_json::json!({"ts":1000002.0,"tag":"E","content":"error here"}),
            serde_json::json!({"ts":1000003.0,"tag":"O","content":"line four"}),
            serde_json::json!({"ts":1000004.0,"tag":"A","content":{"source":"test.src","event":"evt","data":{"msg":"line five with error word"}}}),
        ] {
            writeln!(f, "{}", serde_json::to_string(&line).unwrap()).unwrap();
        }
        path
    }

    #[test]
    fn format_raw_returns_string_content() {
        let line: LogLine = serde_json::from_value(serde_json::json!({
            "ts": 1773653954.012345,
            "tag": "O",
            "content": "hello world"
        }))
        .unwrap();
        assert_eq!(line.format_raw(), "hello world");
    }

    #[test]
    fn format_raw_stringifies_annotation_content() {
        let line: LogLine = serde_json::from_value(serde_json::json!({
            "ts": 1773653954.012345,
            "tag": "A",
            "content": {"source":"cmux.hook","event":"pre-tool-use"}
        }))
        .unwrap();
        assert_eq!(
            line.format_raw(),
            r#"{"event":"pre-tool-use","source":"cmux.hook"}"#
        );
    }

    #[test]
    fn query_full_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_log(dir.path());
        let mut buf = Vec::new();
        let query = LogQuery::default();
        let count = query_log(&path, &query, &mut buf).unwrap();
        assert_eq!(count, 5);
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("\"content\":\"line one\""));
        assert!(output.contains("\"tag\":\"A\""));
    }

    #[test]
    fn query_tail_2() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_log(dir.path());
        let mut buf = Vec::new();
        let query = LogQuery {
            tail: Some(2),
            ..Default::default()
        };
        let count = query_log(&path, &query, &mut buf).unwrap();
        assert_eq!(count, 2);
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("\"content\":\"line four\""));
        assert!(output.contains("\"tag\":\"A\""));
        assert!(!output.contains("\"content\":\"line one\""));
    }

    #[test]
    fn query_tail_0_returns_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_log(dir.path());
        let mut buf = Vec::new();
        let query = LogQuery {
            tail: Some(0),
            ..Default::default()
        };
        let count = query_log(&path, &query, &mut buf).unwrap();
        assert_eq!(count, 0);
        assert!(buf.is_empty());
    }

    #[test]
    fn query_since() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_log(dir.path());
        let mut buf = Vec::new();
        let query = LogQuery {
            since_us: Some(1_000_002_000_000),
            ..Default::default()
        };
        let count = query_log(&path, &query, &mut buf).unwrap();
        assert_eq!(count, 3);
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("\"content\":\"error here\""));
        assert!(output.contains("\"content\":\"line four\""));
        assert!(output.contains("\"msg\":\"line five with error word\""));
        assert!(!output.contains("\"content\":\"line one\""));
    }

    #[test]
    fn query_raw() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_log(dir.path());
        let mut buf = Vec::new();
        let query = LogQuery {
            raw: true,
            ..Default::default()
        };
        let count = query_log(&path, &query, &mut buf).unwrap();
        assert_eq!(count, 5);
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("line one"));
        let annotation = output
            .lines()
            .find(|line| line.contains("line five with error word"))
            .unwrap();
        let annotation: serde_json::Value = serde_json::from_str(annotation).unwrap();
        assert_eq!(annotation["source"], "test.src");
        assert_eq!(annotation["event"], "evt");
        assert_eq!(annotation["data"]["msg"], "line five with error word");
        assert!(!output.contains("\"tag\""));
    }

    #[test]
    fn query_missing_file_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.log");
        let mut buf = Vec::new();
        let query = LogQuery::default();
        let result = query_log(&path, &query, &mut buf);
        assert!(result.is_err());
    }

    #[test]
    fn follow_stops_on_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_log(dir.path());
        let mut buf = Vec::new();
        let query = LogQuery {
            tail: Some(100),
            ..Default::default()
        };
        follow_log(&path, &query, &mut buf, || true).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("line one"));
        assert!(output.contains("line five with error word"));
    }

    #[test]
    fn follow_picks_up_new_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.log");

        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(
                f,
                "{}",
                serde_json::json!({"ts":1000000.0,"tag":"O","content":"initial line"})
            )
            .unwrap();
        }

        let seen = Arc::new(AtomicBool::new(false));
        let seen_for_stop = Arc::clone(&seen);
        // Set once the follower has written the tail and polls for more.
        let following = Arc::new(AtomicBool::new(false));
        let following_for_stop = Arc::clone(&following);
        let path_clone = path.clone();

        let appender = thread::spawn(move || {
            while !following.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(10));
            }
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path_clone)
                .unwrap();
            writeln!(
                f,
                "{}",
                serde_json::json!({"ts":1000001.0,"tag":"O","content":"appended line"})
            )
            .unwrap();
        });

        let query = LogQuery {
            tail: Some(100),
            ..Default::default()
        };
        let deadline = std::time::Instant::now() + HANG;
        let mut writer = SignallingWriter::new(b"appended line", seen);
        follow_log(&path, &query, &mut writer, || {
            following_for_stop.store(true, Ordering::Release);
            seen_for_stop.load(Ordering::Acquire) || std::time::Instant::now() >= deadline
        })
        .unwrap();
        appender.join().unwrap();

        let output = String::from_utf8(writer.bytes).unwrap();
        assert!(output.contains("initial line"));
        assert!(output.contains("appended line"));
    }

    #[test]
    fn follow_waits_for_file_creation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("delayed.log");

        let seen = Arc::new(AtomicBool::new(false));
        let seen_for_stop = Arc::clone(&seen);
        // Set once the follower has looked for the file and is waiting.
        let waiting = Arc::new(AtomicBool::new(false));
        let waiting_for_stop = Arc::clone(&waiting);
        let path_clone = path.clone();
        let temp_path = dir.path().join("delayed.log.tmp");

        let creator = thread::spawn(move || {
            while !waiting.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(10));
            }
            {
                let mut f = std::fs::File::create(&temp_path).unwrap();
                writeln!(
                    f,
                    "{}",
                    serde_json::json!({"ts":1000000.0,"tag":"O","content":"delayed line"})
                )
                .unwrap();
            }
            std::fs::rename(temp_path, path_clone).unwrap();
        });

        let query = LogQuery {
            tail: Some(100),
            ..Default::default()
        };
        let deadline = std::time::Instant::now() + HANG;
        let mut writer = SignallingWriter::new(b"delayed line", seen);
        follow_log(&path, &query, &mut writer, || {
            waiting_for_stop.store(true, Ordering::Release);
            seen_for_stop.load(Ordering::Acquire) || std::time::Instant::now() >= deadline
        })
        .unwrap();
        creator.join().unwrap();

        let output = String::from_utf8(writer.bytes).unwrap();
        assert!(output.contains("delayed line"));
    }

    #[test]
    fn parse_since_epoch_seconds() {
        let result = parse_since("1000000").unwrap();
        assert_eq!(result, 1_000_000_000_000);
    }

    #[test]
    fn parse_since_seconds_duration() {
        let before = now_us();
        let result = parse_since("30s").unwrap();
        let after = now_us();
        let expected_low = before - 30_000_000;
        let expected_high = after - 30_000_000;
        assert!(
            result >= expected_low.saturating_sub(1000) && result <= expected_high + 1000,
            "result {result} not in expected range [{expected_low}, {expected_high}]"
        );
    }

    #[test]
    fn parse_since_minutes_duration() {
        let before = now_us();
        let result = parse_since("5m").unwrap();
        let after = now_us();
        let expected_low = before - 300_000_000;
        let expected_high = after - 300_000_000;
        assert!(
            result >= expected_low.saturating_sub(1000) && result <= expected_high + 1000,
            "result {result} not in expected range [{expected_low}, {expected_high}]"
        );
    }

    #[test]
    fn parse_since_invalid() {
        assert!(parse_since("abc").is_err());
        assert!(parse_since("5x").is_err());
        assert!(parse_since("").is_err());
    }
}
