//! `tendr events` — the protocol read surface (spec §5.1–5.2): replay all
//! segments of matching sessions merged by (ts, writer, seq) as envelope
//! NDJSON, with warm starts (`--since`, `--last`, `--from-now`,
//! `--from-cursor`), read-time output.log projection (`--include-logs`),
//! resumable cursor bookmarks (`--cursors`), and poll-based live tailing
//! (`--follow` at the shipped 100 ms constant — the disk is the buffer).

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tendr::events::{
    POLL_INTERVAL, decode_cursor, encode_cursor, merge_key, project_log_line, read_segment_records,
};
use tendr::log::LogLine;
use tendr::model::event::{Event, EventTimestamp};
use tendr::model::ids::{Namespace, SessionName};
use tendr::session::{self, SessionRoot};

/// Bookmark cadence: every 100 emitted records or
/// 5 s idle, counters reset on each bookmark.
const BOOKMARK_EVERY_RECORDS: usize = 100;
const BOOKMARK_IDLE: Duration = Duration::from_secs(5);

pub struct EventsOptions {
    pub namespace: Option<String>,
    pub sessions: Vec<String>,
    pub kinds: Vec<String>,
    pub sources: Vec<String>,
    pub follow: bool,
    pub from_now: bool,
    pub from_cursor: Option<String>,
    pub since: Option<EventTimestamp>,
    pub last: Option<usize>,
    pub cursors: bool,
    pub include_logs: bool,
    pub strict: bool,
    /// Follow-only: atomically publish this file once the baseline is
    /// established and initial replay is flushed. See [`ready_file`](tendr::ready_file).
    pub ready_file: Option<PathBuf>,
}

/// Kind/source/since filters, applied uniformly to stored and derived
/// records at read time. Offsets still advance past filtered records —
/// a cursor tracks read position, not printed position.
struct Filters {
    kinds: Vec<String>,
    sources: Vec<String>,
    since: Option<EventTimestamp>,
}

impl Filters {
    fn passes(&self, kind: &str, source: &str, ts: EventTimestamp) -> bool {
        (self.kinds.is_empty() || self.kinds.iter().any(|prefix| kind.starts_with(prefix)))
            && (self.sources.is_empty()
                || self.sources.iter().any(|prefix| source.starts_with(prefix)))
            && self.since.is_none_or(|since| ts >= since)
    }
}

/// One record of the merged output stream: a stored envelope event (with
/// its segment identity for cursor bookkeeping) or a read-time derived
/// record (log projection). Derived records sort by timestamp with a zero
/// writer, before stored events at the same ts — arrival interleaving is
/// best-effort by contract (spec §4).
enum OutRecord {
    Stored {
        // Boxed: an Event is ~370 bytes and batches can hold a whole
        // replay; Derived stays small.
        event: Box<Event>,
        rel: String,
        start: u64,
    },
    Derived {
        ts: EventTimestamp,
        tie: u64,
        value: serde_json::Value,
    },
}

impl OutRecord {
    fn sort_key(&self) -> (EventTimestamp, u128, u64) {
        match self {
            Self::Stored { event, .. } => merge_key(event),
            Self::Derived { ts, tie, .. } => (*ts, 0, *tie),
        }
    }

    fn to_line(&self) -> serde_json::Result<String> {
        match self {
            Self::Stored { event, .. } => serde_json::to_string(event),
            Self::Derived { value, .. } => serde_json::to_string(value),
        }
    }
}

/// Everything one poll pass produced across all matched sessions
/// (post-filter records; skips are counted pre-filter).
#[derive(Default)]
struct Batch {
    records: Vec<OutRecord>,
    skipped: usize,
}

/// Exact-resume cursor state (spec §5.2). `read_to` is how far each
/// segment has been read; `pending` holds the start offsets of records
/// read but not yet printed. A stream's resumable offset is the earliest
/// unprinted record, or `read_to` when everything read was printed (or
/// filtered) — so a bookmark emitted mid-stream never duplicates and never
/// drops.
#[derive(Default)]
struct CursorTracker {
    read_to: BTreeMap<String, u64>,
    pending: BTreeMap<String, BTreeSet<u64>>,
}

impl CursorTracker {
    fn seed(&mut self, streams: BTreeMap<String, u64>) {
        self.read_to = streams;
    }

    fn note_read_to(&mut self, rel: &str, consumed_to: u64) {
        self.read_to.insert(rel.to_owned(), consumed_to);
    }

    fn note_pending(&mut self, rel: &str, start: u64) {
        self.pending
            .entry(rel.to_owned())
            .or_default()
            .insert(start);
    }

    fn note_consumed(&mut self, rel: &str, start: u64) {
        if let Some(starts) = self.pending.get_mut(rel) {
            starts.remove(&start);
            if starts.is_empty() {
                self.pending.remove(rel);
            }
        }
    }

    fn token(&self) -> String {
        let mut streams = self.read_to.clone();
        for (rel, starts) in &self.pending {
            if let Some(min) = starts.first() {
                streams.insert(rel.clone(), *min);
            }
        }
        encode_cursor(&streams)
    }
}

/// Bookmark cadence counters — reset on each bookmark.
struct BookmarkState {
    since_count: usize,
    last_mark: Instant,
}

/// Read complete `output.log` lines from a byte offset — the same
/// only-`\n`-terminated discipline as event segments, so a line mid-write
/// waits instead of being lost. Unparseable lines are consumed silently:
/// output.log is a separate contract and never feeds `--strict`.
fn read_log_lines(path: &Path, from: u64) -> io::Result<(Vec<LogLine>, u64)> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(from))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;

    let mut lines = Vec::new();
    let mut consumed_to = from;
    let mut line_start = 0usize;
    while let Some(nl) = buf[line_start..].iter().position(|b| *b == b'\n') {
        let line_end = line_start + nl;
        let raw = &buf[line_start..line_end];
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        if let Some(parsed) = std::str::from_utf8(raw)
            .ok()
            .and_then(|line| serde_json::from_str::<LogLine>(line).ok())
        {
            lines.push(parsed);
        }
        consumed_to = from + line_end as u64 + 1;
        line_start = line_end + 1;
    }
    Ok((lines, consumed_to))
}

/// Incremental reader over the event segments (and optionally output.log)
/// of all matched sessions. Every `poll` re-discovers sessions and
/// segments, so sessions and segments that appear mid-follow are picked up
/// and replay from their start.
struct StreamReader {
    root: SessionRoot,
    namespace_filter: Option<Namespace>,
    /// `Some` when `--session` was given: a fixed target list (the dirs may
    /// appear later — follow waits for them). `None` = all sessions.
    explicit: Option<Vec<(Namespace, SessionName)>>,
    filters: Filters,
    include_logs: bool,
    /// After `--from-cursor`, log projection restarts at the resume
    /// wall-clock — cursors never cover output.log.
    logs_from_end: bool,
    tracker: CursorTracker,
    /// `<ns>/<session>/events/<seg>.jsonl` → offset after last consumed line.
    seg_offsets: BTreeMap<String, u64>,
    /// `<ns>/<session>` → output.log offset after last consumed line.
    log_offsets: BTreeMap<String, u64>,
    /// Monotonic tiebreak so derived records keep arrival order at equal ts.
    derived_tie: u64,
}

impl StreamReader {
    fn poll(&mut self) -> anyhow::Result<Batch> {
        let targets: Vec<(Namespace, SessionName)> = match &self.explicit {
            Some(list) => list.clone(),
            None => session::list(&self.root, self.namespace_filter.as_ref())?,
        };

        let mut batch = Batch::default();
        for (namespace, name) in &targets {
            let session_dir = self
                .root
                .path()
                .join(namespace.as_str())
                .join(name.as_str());
            self.poll_segments(&session_dir, namespace, name, &mut batch)?;
            if self.include_logs {
                self.poll_log(&session_dir, namespace, name, &mut batch);
            }
        }
        Ok(batch)
    }

    fn poll_segments(
        &mut self,
        session_dir: &Path,
        namespace: &Namespace,
        name: &SessionName,
        batch: &mut Batch,
    ) -> anyhow::Result<()> {
        // A session with no events dir (pruned, pre-events, or never
        // started) contributes an empty log — reads are total over what
        // exists.
        let events_dir = session_dir.join("events");
        let Ok(read_dir) = std::fs::read_dir(&events_dir) else {
            return Ok(());
        };
        let mut segments: Vec<PathBuf> = read_dir
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .collect();
        segments.sort();

        for segment in segments {
            let Some(file_name) = segment.file_name().and_then(|f| f.to_str()) else {
                continue;
            };
            let rel = format!(
                "{}/{}/events/{file_name}",
                namespace.as_str(),
                name.as_str()
            );
            let from = self.seg_offsets.get(&rel).copied().unwrap_or(0);
            let outcome = match read_segment_records(&segment, from) {
                Ok(outcome) => outcome,
                // Pruned between listing and reading — the session is gone,
                // not corrupt.
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            batch.skipped += outcome.skipped;
            self.seg_offsets.insert(rel.clone(), outcome.consumed_to);
            self.tracker.note_read_to(&rel, outcome.consumed_to);
            for record in outcome.records {
                if self.filters.passes(
                    record.event.kind.as_str(),
                    record.event.source.as_str(),
                    record.event.ts,
                ) {
                    self.tracker.note_pending(&rel, record.start);
                    batch.records.push(OutRecord::Stored {
                        event: Box::new(record.event),
                        rel: rel.clone(),
                        start: record.start,
                    });
                }
            }
        }
        Ok(())
    }

    fn poll_log(
        &mut self,
        session_dir: &Path,
        namespace: &Namespace,
        name: &SessionName,
        batch: &mut Batch,
    ) {
        let key = format!("{}/{}", namespace.as_str(), name.as_str());
        let log_path = session_dir.join("output.log");
        let from = match self.log_offsets.get(&key) {
            Some(offset) => *offset,
            // First sight after a cursor resume: start at the wall-clock
            // EOF, documented best-effort (cursors never cover output.log).
            None if self.logs_from_end => std::fs::metadata(&log_path)
                .map(|m| m.len())
                .unwrap_or_default(),
            None => 0,
        };
        let Ok((lines, consumed_to)) = read_log_lines(&log_path, from) else {
            return;
        };
        self.log_offsets.insert(key, consumed_to);
        if lines.is_empty() {
            return;
        }

        // output.log lines carry no run identity; attribute them to the
        // session's current run like watch does.
        let run_id = std::fs::read_to_string(session_dir.join("meta.json"))
            .ok()
            .and_then(|meta| serde_json::from_str::<serde_json::Value>(&meta).ok())
            .and_then(|meta| meta["run_id"].as_str().map(str::to_owned))
            .unwrap_or_default();

        for line in lines {
            let Some(value) = project_log_line(&line, namespace.as_str(), name.as_str(), &run_id)
            else {
                continue;
            };
            let ts = EventTimestamp::from_epoch_secs_f64(line.ts);
            let kind = if line.tag == "O" {
                "log.stdout"
            } else {
                "log.stderr"
            };
            if self.filters.passes(kind, "tendr.sidecar", ts) {
                batch.records.push(OutRecord::Derived {
                    ts,
                    tie: self.derived_tie,
                    value,
                });
                self.derived_tie += 1;
            }
        }
    }
}

/// Cursor-gone (spec §5.2): defined staleness, defined recovery, never a
/// silent restart from zero.
fn cursor_gone(gone: Vec<String>) -> ! {
    let message = serde_json::json!({
        "error": "cursor_gone",
        "gone": gone,
        "recover": "replay without --from-cursor, or use --since <ts>",
    });
    eprintln!("{message}");
    std::process::exit(44);
}

/// Emit one `cursor.bookmark` record: read-time only, no stored identity
/// (spec §5.2). Returns false when stdout's pipe closed.
fn write_bookmark(
    out: &mut impl Write,
    tracker: &CursorTracker,
    mark: &mut BookmarkState,
) -> io::Result<bool> {
    let record = serde_json::json!({
        "kind": "cursor.bookmark",
        "ts": EventTimestamp::now().to_string(),
        "cursor": tracker.token(),
        "derived": true,
    });
    if writeln!(out, "{record}").is_err() || out.flush().is_err() {
        return Ok(false);
    }
    mark.since_count = 0;
    mark.last_mark = Instant::now();
    Ok(true)
}

/// Sort and print one batch, maintaining cursor exactness and bookmark
/// cadence. Returns false when stdout's pipe closed — the consumer went
/// away, which is a normal exit, not an error.
fn emit_batch(
    out: &mut impl Write,
    opts: &EventsOptions,
    tracker: &mut CursorTracker,
    mark: &mut BookmarkState,
    mut records: Vec<OutRecord>,
    limit_last: Option<usize>,
) -> anyhow::Result<bool> {
    records.sort_by_key(OutRecord::sort_key);
    if let Some(n) = limit_last {
        if records.len() > n {
            // Tail-N: the head is consumed-by-request, not left pending —
            // a later bookmark must not pin the cursor before it.
            for dropped in records.drain(..records.len() - n) {
                if let OutRecord::Stored { rel, start, .. } = dropped {
                    tracker.note_consumed(&rel, start);
                }
            }
        }
    }
    for record in records {
        let line = record.to_line()?;
        if writeln!(out, "{line}").is_err() {
            return Ok(false);
        }
        if let OutRecord::Stored { rel, start, .. } = &record {
            tracker.note_consumed(rel, *start);
        }
        mark.since_count += 1;
        mark.last_mark = Instant::now();
        if opts.cursors
            && mark.since_count >= BOOKMARK_EVERY_RECORDS
            && !write_bookmark(out, tracker, mark)?
        {
            return Ok(false);
        }
    }
    if out.flush().is_err() {
        return Ok(false);
    }
    Ok(true)
}

/// `--strict`: unparseable segment lines are a defect wherever observed —
/// after a full replay batch or mid-follow. Valid
/// records of the offending batch have already been printed.
fn strict_check(opts: &EventsOptions, skipped: usize) {
    if opts.strict && skipped > 0 {
        eprintln!("tendr events: {skipped} unparseable line(s) skipped");
        std::process::exit(65);
    }
}

pub fn cmd_events(opts: EventsOptions) -> anyhow::Result<()> {
    // clap enforces this via the warm_start ArgGroup; restated here so the
    // command layer carries the invariant too.
    anyhow::ensure!(
        [
            opts.from_now,
            opts.from_cursor.is_some(),
            opts.since.is_some(),
            opts.last.is_some(),
        ]
        .iter()
        .filter(|set| **set)
        .count()
            <= 1,
        "--from-now, --from-cursor, --since, and --last are mutually exclusive"
    );

    let root = SessionRoot::default_path()?;
    let namespace_filter = opts.namespace.as_deref().map(Namespace::new).transpose()?;

    // The explicit --session list, else every session (spec §5.1).
    let explicit: Option<Vec<(Namespace, SessionName)>> = if opts.sessions.is_empty() {
        None
    } else {
        let mut targets = Vec::new();
        for spec in &opts.sessions {
            let (ns, name) = match spec.split_once('/') {
                Some((ns, name)) => (ns, name),
                None => ("default", spec.as_str()),
            };
            let namespace = Namespace::new(ns)?;
            if namespace_filter
                .as_ref()
                .is_some_and(|filter| filter != &namespace)
            {
                continue;
            }
            targets.push((namespace, SessionName::new(name)?));
        }
        Some(targets)
    };

    // --from-cursor: resolve before reading anything. A token naming a
    // segment that no longer exists — or an unparseable/unknown-version
    // token — is cursor-gone, exit 44 (spec §5.2).
    let seeded_offsets: BTreeMap<String, u64> = match &opts.from_cursor {
        None => BTreeMap::new(),
        Some(token) => match decode_cursor(token) {
            Err(_) => cursor_gone(vec![token.clone()]),
            Ok(streams) => {
                let gone: Vec<String> = streams
                    .keys()
                    .filter(|rel| {
                        // Tokens are opaque user input: never let a stream
                        // path escape the sessions root.
                        rel.starts_with('/')
                            || rel.split('/').any(|part| part == "..")
                            || !root.path().join(rel).is_file()
                    })
                    .cloned()
                    .collect();
                if !gone.is_empty() {
                    cursor_gone(gone);
                }
                streams
            }
        },
    };

    let mut reader = StreamReader {
        root,
        namespace_filter,
        explicit,
        filters: Filters {
            kinds: opts.kinds.clone(),
            sources: opts.sources.clone(),
            since: opts.since,
        },
        include_logs: opts.include_logs,
        logs_from_end: opts.from_cursor.is_some(),
        tracker: CursorTracker::default(),
        seg_offsets: seeded_offsets.clone(),
        log_offsets: BTreeMap::new(),
        derived_tie: 0,
    };
    reader.tracker.seed(seeded_offsets);

    // --from-now: consume the history of sessions existing at invocation
    // without printing it. Skips in discarded history are not "observed" —
    // reading starts now by request.
    if opts.from_now {
        let discarded = reader.poll()?;
        for record in discarded.records {
            if let OutRecord::Stored { rel, start, .. } = record {
                reader.tracker.note_consumed(&rel, start);
            }
        }
    }

    let stdout = io::stdout().lock();
    let mut out = io::BufWriter::new(stdout);
    let mut mark = BookmarkState {
        since_count: 0,
        last_mark: Instant::now(),
    };

    // Replay: everything between the warm-start position and now.
    let batch = reader.poll()?;
    let skipped = batch.skipped;
    if !emit_batch(
        &mut out,
        &opts,
        &mut reader.tracker,
        &mut mark,
        batch.records,
        opts.last,
    )? {
        return Ok(());
    }
    strict_check(&opts, skipped);

    if !opts.follow {
        // A final bookmark so batch consumers can resume past everything
        // they were handed, including a partial last hundred.
        if opts.cursors {
            let _ = write_bookmark(&mut out, &reader.tracker, &mut mark)?;
        }
        return Ok(());
    }

    // Baseline captured and initial replay flushed: publish the out-of-band
    // readiness signal (never on stdout) so callers know it is safe to perform
    // live mutations before we enter the poll loop.
    if let Some(ready_path) = &opts.ready_file {
        // A dead consumer stops the follow, same as emit_batch's flush handling;
        // the readiness signal only makes sense while someone is reading.
        if out.flush().is_err() {
            return Ok(());
        }
        tendr::ready_file::create_ready_file(ready_path).map_err(|e| {
            anyhow::anyhow!("failed to create ready-file {}: {e}", ready_path.display())
        })?;
    }

    loop {
        std::thread::sleep(POLL_INTERVAL);
        let batch = reader.poll()?;
        let skipped = batch.skipped;
        if !emit_batch(
            &mut out,
            &opts,
            &mut reader.tracker,
            &mut mark,
            batch.records,
            None,
        )? {
            return Ok(());
        }
        strict_check(&opts, skipped);
        if opts.cursors
            && mark.last_mark.elapsed() >= BOOKMARK_IDLE
            && !write_bookmark(&mut out, &reader.tracker, &mut mark)?
        {
            return Ok(());
        }
    }
}
