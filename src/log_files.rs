//! `output.log` and its rotated archives.
//!
//! The sidecar appends to `output.log`. Once it holds [`ROTATE_BYTES`], the
//! sidecar renames it to `output.log.<n>`, `n` counting up from 1 for the
//! session's life, starts a new `output.log`, and deletes archives older
//! than the newest [`KEEP_ARCHIVES`]. So a session keeps at most
//! `(KEEP_ARCHIVES + 1) * ROTATE_BYTES` of output (256 MiB), and rotation
//! falls between two records, never inside one.
//!
//! Archives are never renamed again, so a position is a [`LogCursor`]: the
//! archive count when it was taken, and an offset into the file that was
//! `output.log` then. [`LogReader`] reads lines from a cursor on, across
//! rotations, so readers and followers keep their place while the log
//! rotates under them.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The live log's name in the session dir.
pub const LOG_FILE: &str = "output.log";

/// The size at which `output.log` rotates.
pub const ROTATE_BYTES: u64 = 64 << 20;

/// How many rotated files a session keeps besides `output.log`.
pub const KEEP_ARCHIVES: u64 = 3;

/// The rotation size, which a debug build's tests may lower.
fn rotate_bytes() -> u64 {
    crate::test_timer::bytes_or_env("TENDR_TEST_OUTPUT_LOG_BYTES", ROTATE_BYTES)
}

/// `output.log.<n>`.
#[must_use]
pub fn archive_path(dir: &Path, n: u64) -> PathBuf {
    dir.join(format!("{LOG_FILE}.{n}"))
}

/// The numbers of the archives in `dir`, oldest first.
fn archives(dir: &Path) -> io::Result<Vec<u64>> {
    let prefix = format!("{LOG_FILE}.");
    let mut numbers: Vec<u64> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter_map(|e| {
                e.file_name()
                    .to_str()?
                    .strip_prefix(&prefix)?
                    .parse::<u64>()
                    .ok()
            })
            .filter(|&n| n > 0)
            .collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    numbers.sort_unstable();
    Ok(numbers)
}

fn newest_archive(dir: &Path) -> io::Result<u64> {
    Ok(archives(dir)?.last().copied().unwrap_or(0))
}

/// Whether the session has any log yet.
#[must_use]
pub fn exists(dir: &Path) -> bool {
    dir.join(LOG_FILE).exists() || newest_archive(dir).is_ok_and(|n| n > 0)
}

/// A position in a session's log: `offset` bytes into the file that was
/// `output.log` while `archive` archives had been made. The default is the
/// start of the oldest retained file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogCursor {
    pub archive: u64,
    pub offset: u64,
}

impl LogCursor {
    /// Whether retention deleted the file this position is in.
    pub fn is_gone(&self, dir: &Path) -> io::Result<bool> {
        Ok(self.archive < newest_archive(dir)? && !archive_path(dir, self.archive + 1).exists())
    }

    /// The end of the log: where the next record will go.
    pub fn end(dir: &Path) -> io::Result<Self> {
        loop {
            let archive = newest_archive(dir)?;
            let offset = match std::fs::metadata(dir.join(LOG_FILE)) {
                Ok(m) => m.len(),
                Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
                Err(e) => return Err(e),
            };
            // A rotation in between would pair the new count with the old
            // file's length.
            if !archive_path(dir, archive + 1).exists() {
                return Ok(Self { archive, offset });
            }
        }
    }
}

/// Reads complete lines of a session's log from a cursor on, across
/// rotations. A line still being written is left for a later call.
pub struct LogReader {
    dir: PathBuf,
    cursor: LogCursor,
    /// The open file, and whether it was `output.log` when opened.
    open: Option<(BufReader<File>, bool)>,
}

impl LogReader {
    #[must_use]
    pub fn new(dir: &Path, cursor: LogCursor) -> Self {
        Self {
            dir: dir.to_path_buf(),
            cursor,
            open: None,
        }
    }

    /// The position after the last line returned.
    #[must_use]
    pub fn cursor(&self) -> LogCursor {
        self.cursor
    }

    /// Read the next complete line into `line`, without its `\n` (or `\r\n`).
    /// `false` when there is none yet.
    pub fn next_line(&mut self, line: &mut Vec<u8>) -> io::Result<bool> {
        loop {
            if self.open.is_none() && !self.open_next()? {
                return Ok(false);
            }
            let (reader, current) = self.open.as_mut().expect("opened above");
            line.clear();
            let n = reader.read_until(b'\n', line)?;
            if line.last() == Some(&b'\n') {
                self.cursor.offset += n as u64;
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(true);
            }
            if *current {
                if n > 0 {
                    // A line being written: read it whole next time.
                    reader.seek_relative(-(n as i64))?;
                }
                // Rotated when an archive was made after our file's: the
                // next one, or (once retention deleted it) a later one.
                if newest_archive(&self.dir)? <= self.cursor.archive {
                    return Ok(false);
                }
                // It rotated. The sidecar writes nothing to a file after
                // renaming it, so read it to its end once more as an archive.
                *current = false;
                continue;
            }
            // An archive is complete: move on to the next file.
            self.cursor = LogCursor {
                archive: self.cursor.archive + 1,
                offset: 0,
            };
            self.open = None;
        }
    }

    /// Open the cursor's file at its offset. `false` when it does not exist
    /// yet (no log, or `output.log` between a rotation's rename and create).
    fn open_next(&mut self) -> io::Result<bool> {
        loop {
            let newest = newest_archive(&self.dir)?;
            let (file, current) = if self.cursor.archive < newest {
                match File::open(archive_path(&self.dir, self.cursor.archive + 1)) {
                    Ok(file) => (file, false),
                    // Deleted by retention before it was read: skip it. On
                    // Windows a file pending deletion refuses to open, which
                    // counts only for an archive retention has dropped.
                    Err(e)
                        if e.kind() == io::ErrorKind::NotFound
                            || (e.kind() == io::ErrorKind::PermissionDenied
                                && self.cursor.archive + KEEP_ARCHIVES < newest) =>
                    {
                        self.cursor = LogCursor {
                            archive: self.cursor.archive + 1,
                            offset: 0,
                        };
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            } else {
                let opened = File::open(self.dir.join(LOG_FILE));
                // Opened before any rotation since `newest` was read, so it
                // is the cursor's file; otherwise resolve again.
                if newest_archive(&self.dir)? != newest {
                    continue;
                }
                match opened {
                    Ok(file) => (file, true),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
                    Err(e) => return Err(e),
                }
            };
            let mut reader = BufReader::new(file);
            reader.seek(SeekFrom::Start(self.cursor.offset))?;
            self.open = Some((reader, current));
            return Ok(true);
        }
    }
}

/// Where the sidecar writes records: a [`LogWriter`], or a sink when
/// `output.log` cannot be opened.
pub trait LogSink: Write + Send {
    /// A warning for the run's meta, raised since the last call.
    fn take_warning(&mut self) -> Option<String> {
        None
    }
}

impl LogSink for io::Sink {}

/// The sidecar's writer: appends to `output.log` and rotates it. Each
/// `write` is one record and is written whole, so rotation falls between
/// records.
pub struct LogWriter {
    dir: PathBuf,
    /// `None` between a rotation's rename and the new file's creation; the
    /// next record creates it, or fails with why.
    file: Option<File>,
    /// The file length at which to rotate: `limit`, or after a failed
    /// rotation another `limit` past where it failed.
    rotate_at: u64,
    limit: u64,
    archives: u64,
    /// A rotation has failed; warned once.
    rotate_failed: bool,
    warning: Option<String>,
}

impl LogWriter {
    /// Open (or create) `output.log` in `dir` for appending.
    pub fn open(dir: &Path) -> io::Result<Self> {
        let file = append(dir)?;
        let limit = rotate_bytes();
        Ok(Self {
            dir: dir.to_path_buf(),
            file: Some(file),
            rotate_at: limit,
            limit,
            archives: newest_archive(dir)?,
            rotate_failed: false,
            warning: None,
        })
    }

    fn rotate(&mut self) -> io::Result<()> {
        let n = self.archives + 1;
        std::fs::rename(self.dir.join(LOG_FILE), archive_path(&self.dir, n))?;
        self.archives = n;
        self.file = None;
        self.rotate_at = self.limit;
        if let Some(old) = n.checked_sub(KEEP_ARCHIVES) {
            for stale in archives(&self.dir)?.into_iter().filter(|&a| a <= old) {
                let _ = std::fs::remove_file(archive_path(&self.dir, stale));
            }
        }
        Ok(())
    }
}

fn append(dir: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(LOG_FILE))
}

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let file = match &mut self.file {
            Some(file) => file,
            None => self.file.insert(append(&self.dir)?),
        };
        file.write_all(buf)?;
        // The file's length, not the bytes written here: `wrap` and `exec`
        // append annotations through their own handles, and they count too.
        let size = file.metadata()?.len();
        if size >= self.rotate_at {
            if let Err(e) = self.rotate() {
                // Keep appending; try again after another limit's worth.
                self.rotate_at = size + self.limit;
                if !std::mem::replace(&mut self.rotate_failed, true) {
                    self.warning = Some(format!(
                        "output.log rotation failed, so the log grows past its limit: {e}"
                    ));
                }
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.as_mut().map_or(Ok(()), File::flush)
    }
}

impl LogSink for LogWriter {
    fn take_warning(&mut self) -> Option<String> {
        self.warning.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn writer(dir: &Path, limit: u64) -> LogWriter {
        let mut w = LogWriter::open(dir).unwrap();
        w.limit = limit;
        w.rotate_at = limit;
        w
    }

    fn lines(reader: &mut LogReader) -> Vec<String> {
        let mut out = Vec::new();
        let mut line = Vec::new();
        while reader.next_line(&mut line).unwrap() {
            out.push(String::from_utf8(line.clone()).unwrap());
        }
        out
    }

    #[test]
    fn a_reader_keeps_its_place_across_rotations_and_retention() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = writer(dir.path(), 10);
        let mut follower = LogReader::new(dir.path(), LogCursor::default());
        assert!(lines(&mut follower).is_empty());

        w.write_all(b"record-0\n").unwrap(); // 9 bytes: no rotation
        assert_eq!(lines(&mut follower), ["record-0"]);
        w.write_all(b"record-1\n").unwrap(); // 18: rotates to .1
        w.write_all(b"rec").unwrap(); // a record being written
        assert_eq!(lines(&mut follower), ["record-1"]);
        w.write_all(b"ord-2\n").unwrap();
        assert_eq!(lines(&mut follower), ["record-2"]);
        let end = LogCursor::end(dir.path()).unwrap();
        assert_eq!(
            end,
            LogCursor {
                archive: 1,
                offset: 9
            }
        );

        for i in 3..9 {
            w.write_all(format!("record-{i}\n").as_bytes()).unwrap(); // rotates
            w.write_all(b"x\n").unwrap();
        }
        // Six rotations later only the newest three archives are kept; the
        // follower finishes the file it holds, then skips what is gone.
        assert_eq!(archives(dir.path()).unwrap(), [5, 6, 7]);
        let kept = ["x", "record-6", "x", "record-7", "x", "record-8", "x"];
        let tail = lines(&mut follower);
        assert_eq!(tail[0], "record-3");
        assert_eq!(tail[1..], kept);
        // A fresh reader starts at the oldest retained file, as does one
        // whose file is gone.
        let fresh = lines(&mut LogReader::new(dir.path(), LogCursor::default()));
        assert_eq!(fresh, kept);
        assert_eq!(lines(&mut LogReader::new(dir.path(), end)), kept);
    }

    #[test]
    fn annotations_appended_by_others_count_toward_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = writer(dir.path(), 20);
        w.write_all(b"record-0\n").unwrap();
        append(dir.path())
            .unwrap()
            .write_all(b"annotation\n")
            .unwrap();
        w.write_all(b"record-1\n").unwrap(); // 9 + 11 + 9 bytes: rotates
        assert_eq!(archives(dir.path()).unwrap(), [1]);
    }
}
