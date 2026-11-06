//! Export a PTY recording as asciicast v2. Recordings are decoded before
//! output, so corruption is reported without writing a partial export.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use tendr::recorder::DirectoryStore;
use tendr::recording::{
    Corruption, DecodeError, DecodeErrorKind, DecodedRecording, Geometry, RecordKind, SegmentEnd,
    Sequence, decode_recording, decode_segment,
};

/// Recording export formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ExportFormat {
    /// asciicast v2 (asciinema)
    Asciicast,
}

pub struct ExportOptions {
    pub path: PathBuf,
    pub format: ExportFormat,
}

pub fn cmd_export(options: &ExportOptions) -> anyhow::Result<()> {
    let loaded = load(&options.path)?;
    let cast = match options.format {
        ExportFormat::Asciicast => asciicast(&loaded.recording)?,
    };
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(&cast)
        .and_then(|()| stdout.flush())
        .context("writing the export to stdout")?;
    if let Some(note) = loaded.cut_note() {
        eprintln!("tendr: {note}");
    }
    Ok(())
}

/// A decoded recording and the segment files it came from.
struct Loaded {
    recording: DecodedRecording,
    segments: Vec<PathBuf>,
}

impl Loaded {
    /// What to say about a final segment cut off inside a record.
    fn cut_note(&self) -> Option<String> {
        let SegmentEnd::Truncated { offset } = self.recording.end else {
            return None;
        };
        let file = self.segments.last().expect("a recording has a segment");
        let played = self.recording.records.last().map_or_else(
            || "no record".to_owned(),
            |r| format!("#{}", r.sequence.get()),
        );
        Some(format!(
            "the recording ends inside a record at {} byte offset {offset} \
             (an active run's recording ends this way): this is up to {played}, \
             its last complete record",
            file.display()
        ))
    }
}

/// Read and decode the recording at `path`: a segment file, a run's
/// recording directory, or a directory holding exactly one run's recording
/// (a session's `recording/`, or the session directory).
fn load(path: &Path) -> anyhow::Result<Loaded> {
    if path.is_file() {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let segments = vec![path.to_path_buf()];
        let segment = decode_segment(&bytes).map_err(|e| decode_failure(&e, &segments))?;
        return Ok(Loaded {
            recording: DecodedRecording {
                header: segment.header,
                segment_count: 1,
                records: segment.records,
                end: segment.end,
            },
            segments,
        });
    }
    let mut runs = DirectoryStore::runs(path)
        .with_context(|| format!("listing recordings at {}", path.display()))?;
    let segments = match runs.len() {
        0 => anyhow::bail!(
            "no recording at {}: give a segment file (seg-*.tndrrec), a run's recording \
             directory, or a session directory",
            path.display()
        ),
        1 => runs.pop().expect("one run").1,
        n => {
            let mut message = format!(
                "{} holds {n} runs' recordings; export one of them:",
                path.display()
            );
            for (dir, _) in &runs {
                let _ = write!(message, "\n  {}", dir.display());
            }
            anyhow::bail!(message);
        }
    };
    let data = segments
        .iter()
        .map(|p| std::fs::read(p).with_context(|| format!("reading {}", p.display())))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let recording = decode_recording(data.iter().map(Vec::as_slice))
        .map_err(|e| decode_failure(&e, &segments))?;
    Ok(Loaded {
        recording,
        segments,
    })
}

/// A decoding failure, naming the file and byte offset at fault.
fn decode_failure(error: &DecodeError, segments: &[PathBuf]) -> anyhow::Error {
    let file = error
        .segment
        .and_then(|i| segments.get(i as usize))
        .or(segments.first())
        .map_or_else(String::new, |p| p.display().to_string());
    let what = match error.kind {
        DecodeErrorKind::BadMagic => "not a tendr recording".to_owned(),
        DecodeErrorKind::UnsupportedVersion(v) => {
            format!("recording format version {v}, which this tendr cannot read")
        }
        DecodeErrorKind::Corrupt(c) => match c {
            Corruption::HeaderIncomplete => "corrupt: the segment header is incomplete",
            Corruption::HeaderLength => "corrupt: bad segment header length",
            Corruption::HeaderChecksum => "corrupt: segment header checksum mismatch",
            Corruption::LengthCheck => "corrupt: damaged record length",
            Corruption::LengthBounds => "corrupt: record length out of bounds",
            Corruption::RecordChecksum => "corrupt: record checksum mismatch",
            Corruption::TruncatedInteriorSegment => {
                "corrupt: a segment before the last ends inside a record"
            }
        }
        .to_owned(),
        DecodeErrorKind::Invalid(v) => format!("breaks the recording format ({v:?})"),
    };
    let last = error.last_valid_sequence.map_or_else(
        || "no record before it is valid".to_owned(),
        |s| format!("the last valid record before it is #{}", s.get()),
    );
    anyhow::anyhow!(
        "{file}: {what} at byte offset {}; {last}. Nothing was exported",
        error.offset
    )
}

/// A size as `<cols>x<rows>`, the way terminals name it.
fn size(geometry: Geometry) -> String {
    format!("{}x{}", geometry.cols(), geometry.rows())
}

/// The recording as asciicast v2: a header line, then one event per output,
/// input and resize record, with its time since the recording's origin.
/// Output and input are decoded as UTF-8 across records, each stream on its
/// own, since a character can be split between two; invalid UTF-8 refuses
/// the export. Every applied resize is an `r` event, repaints included.
fn asciicast(recording: &DecodedRecording) -> anyhow::Result<Vec<u8>> {
    let header = &recording.header;
    let mut out = format!(
        "{{\"version\": 2, \"width\": {}, \"height\": {}, \"timestamp\": {}",
        header.geometry.cols(),
        header.geometry.rows(),
        header.origin_unix_ns / 1_000_000_000
    );
    if !header.term.as_str().is_empty() {
        let term = serde_json::to_string(header.term.as_str()).expect("a string serializes");
        let _ = write!(out, ", \"env\": {{\"TERM\": {term}}}");
    }
    out.push_str("}\n");

    let mut output = Utf8Stream::default();
    let mut input = Utf8Stream::default();
    for record in &recording.records {
        let time = Seconds(record.elapsed_ns);
        let (code, data) = match &record.kind {
            RecordKind::Output(bytes) => ("o", output.decode(record.sequence, bytes)?),
            RecordKind::Input(bytes) => ("i", input.decode(record.sequence, bytes)?),
            RecordKind::Resize { geometry, .. } => ("r", size(*geometry)),
        };
        if data.is_empty() {
            continue;
        }
        let data = serde_json::to_string(&data).expect("a string serializes");
        let _ = writeln!(out, "[{time}, \"{code}\", {data}]");
    }
    for (stream, what) in [(&output, "output"), (&input, "input")] {
        if let Some(from) = stream.pending_from {
            match recording.end {
                // The rest of the character was cut off with the record.
                SegmentEnd::Truncated { .. } => {}
                SegmentEnd::Clean => anyhow::bail!(
                    "the recording's {what} ends inside a UTF-8 character begun in record \
                     #{}; asciicast carries text only",
                    from.get()
                ),
            }
        }
    }
    Ok(out.into_bytes())
}

/// Nanoseconds as seconds with six decimals, as asciicast writes time.
struct Seconds(u64);

impl std::fmt::Display for Seconds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}.{:06}",
            self.0 / 1_000_000_000,
            self.0 % 1_000_000_000 / 1_000
        )
    }
}

/// Incremental UTF-8 decoding of one byte stream fed record by record.
#[derive(Default)]
struct Utf8Stream {
    /// The start of a character the last record ended inside.
    pending: Vec<u8>,
    /// The record that began it.
    pending_from: Option<Sequence>,
}

impl Utf8Stream {
    /// The text `bytes` complete, holding back a character they end inside.
    fn decode(&mut self, sequence: Sequence, bytes: &[u8]) -> anyhow::Result<String> {
        let carried = self.pending.len();
        let mut joined = std::mem::take(&mut self.pending);
        joined.extend_from_slice(bytes);
        let from = self.pending_from.take().unwrap_or(sequence);
        match std::str::from_utf8(&joined) {
            Ok(text) => Ok(text.to_owned()),
            Err(e) if e.error_len().is_none() => {
                let valid = e.valid_up_to();
                self.pending = joined[valid..].to_vec();
                self.pending_from = Some(if valid < carried { from } else { sequence });
                Ok(String::from_utf8(joined[..valid].to_vec()).expect("validated prefix"))
            }
            Err(e) => {
                let valid = e.valid_up_to();
                let place = if valid < carried {
                    format!(
                        "the end of record #{}, continued in #{}",
                        from.get(),
                        sequence.get()
                    )
                } else {
                    format!("byte {} of record #{}", valid - carried, sequence.get())
                };
                anyhow::bail!(
                    "invalid UTF-8 at {place}: asciicast carries text only (the recording \
                     itself stays the exact source)"
                )
            }
        }
    }
}
