//! Per-session event log — append path (spec §3.2–3.5) and read path (§5)
//! of docs/architecture/07-event-protocol.md.
//!
//! Layout, per session dir:
//! ```text
//! events/
//!   <seg-uuidv7>.jsonl   # event segments — HISTORY authority
//!   blobs/<sha256>       # spilled payloads, content-addressed
//!   append.lock          # advisory lock file (POSIX only)
//! ```

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::log::LogLine;
use crate::model::boundary::BoundaryContext;
use crate::model::dep_fail::DepFailReason;
use crate::model::event::{DataRef, ENVELOPE_VERSION, Event, EventTimestamp, Kind, Uuid7};
use crate::model::ids::{Namespace, RunId, SessionName, Source};
use crate::model::state::{ExitReason, RunStatus};

/// Poll interval for every follow surface — `tendr events --follow`,
/// `tendr watch`, log follow. One constant, no configuration surface
/// (spec §5.1; the disk is the buffer).
pub const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Inline `data` cap; larger payloads spill to a blob (spec §1, §3.4).
pub const MAX_INLINE_DATA_BYTES: usize = 16 * 1024;
/// Preview size for spilled/degraded payloads (spec §1 `data_ref`).
pub const MAX_PREVIEW_BYTES: usize = 4 * 1024;

/// The fields a caller supplies; `EventWriter::append` stamps the rest
/// (`v`, `id` unless pre-minted, `ts`, `writer`, `seq`) and handles
/// oversize spill.
#[derive(Debug, Clone)]
pub struct EventDraft {
    /// Caller-supplied pre-minted event id; `None` = stamp at append.
    /// wrap pre-mints so `TENDR_PARENT_EVENT_ID` can name the event it
    /// will write (spec §2).
    pub id: Option<Uuid7>,
    pub kind: Kind,
    pub namespace: Namespace,
    pub session: SessionName,
    pub run_id: RunId,
    pub generation: Option<u64>,
    pub source: Source,
    pub block_id: Option<Uuid7>,
    pub parent_id: Option<Uuid7>,
    pub data: Option<serde_json::Value>,
    /// Caller-supplied structured preview, used instead of the generic
    /// head-of-JSON preview when `data` spills (spec §3.4). A preview
    /// that itself exceeds `MAX_PREVIEW_BYTES` degrades to generic.
    pub preview: Option<serde_json::Value>,
}

/// Appends events to one session's `events/` dir with a stable writer
/// identity and per-writer contiguous `seq` starting at 1.
///
/// `seq` is consumed by every write attempt, even one that fails: a gap in
/// the log is honest (readers detect loss); a reused `seq` would be a lie.
#[derive(Debug)]
pub struct EventWriter {
    session_dir: PathBuf,
    writer: Uuid7,
    next_seq: u64,
}

impl EventWriter {
    /// A writer with a freshly minted identity (CLI emitters).
    #[must_use]
    pub fn new(session_dir: &Path) -> Self {
        Self::with_writer(session_dir, Uuid7::new())
    }

    /// A writer with a caller-supplied identity (the sidecar uses its run id).
    #[must_use]
    pub fn with_writer(session_dir: &Path, writer: Uuid7) -> Self {
        Self {
            session_dir: session_dir.to_path_buf(),
            writer,
            next_seq: 1,
        }
    }

    #[must_use]
    pub fn writer_id(&self) -> Uuid7 {
        self.writer
    }

    /// Stamp a draft into a full envelope and append it as one line.
    /// `durable` forces the segment to stable storage before returning
    /// (`fdatasync`; `F_FULLFSYNC` on macOS via std).
    ///
    /// When the append starts a new segment (spec §3.3), this writer first
    /// writes the new segment's `segment.opened`, with the same `ts` and the
    /// previous `seq`, so the merge order keeps it first.
    ///
    /// # Errors
    /// Returns the underlying IO error; the payload is never silently dropped
    /// short of the append itself failing (blob failure degrades inline, §3.4).
    pub fn append(&mut self, draft: EventDraft, durable: bool) -> io::Result<Event> {
        let (data, data_ref, truncated) =
            prepare_data(&self.session_dir, draft.data, draft.preview);
        let events_dir = self.session_dir.join("events");
        std::fs::create_dir_all(&events_dir)?;

        // Picking, rotating and writing happen under one lock, so on Unix
        // exactly one writer starts each new segment.
        #[cfg(unix)]
        let _lock = AppendLock::acquire(&events_dir)?;
        let (mut file, rotation) = open_segment(&events_dir, segment_rotate_bytes())?;
        let ts = EventTimestamp::now();

        if let Some(rotation) = rotation {
            let opened = Event {
                v: ENVELOPE_VERSION,
                id: Uuid7::new(),
                ts,
                kind: Kind::new(SEGMENT_OPENED).expect("segment.opened is grammatical"),
                namespace: draft.namespace.clone(),
                session: draft.session.clone(),
                run_id: draft.run_id,
                generation: draft.generation,
                writer: self.writer,
                seq: self.take_seq(),
                source: Source::trusted(SEGMENT_SOURCE).expect("tendr.events is grammatical"),
                block_id: None,
                parent_id: None,
                data: Some(serde_json::json!({
                    "segment": rotation.segment,
                    "previous": rotation.previous,
                    "previous_bytes": rotation.previous_bytes,
                })),
                data_ref: None,
                truncated: None,
            };
            write_line(&mut file, &opened)?;
        }

        let event = Event {
            v: ENVELOPE_VERSION,
            id: draft.id.unwrap_or_default(), // Default mints a fresh v7
            ts,
            kind: draft.kind,
            namespace: draft.namespace,
            session: draft.session,
            run_id: draft.run_id,
            generation: draft.generation,
            writer: self.writer,
            seq: self.take_seq(),
            source: draft.source,
            block_id: draft.block_id,
            parent_id: draft.parent_id,
            data,
            data_ref,
            truncated,
        };
        write_line(&mut file, &event)?;
        if durable {
            file.sync_data()?;
        }
        Ok(event)
    }

    fn take_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
    }
}

/// Stamp a draft into a full envelope without a session dir — the lost+found
/// path (spec §7). Fresh writer identity, `seq` 1; oversize data is
/// inline-truncated because there is no blob store to spill to.
#[must_use]
pub fn stamp_orphan_event(draft: EventDraft) -> Event {
    let (data, truncated) = match draft.data {
        None => (None, None),
        Some(value) => match serde_json::to_vec(&value) {
            Ok(bytes) if bytes.len() > MAX_INLINE_DATA_BYTES => {
                (Some(effective_preview(draft.preview, &bytes)), Some(true))
            }
            _ => (Some(value), None),
        },
    };
    Event {
        v: ENVELOPE_VERSION,
        id: draft.id.unwrap_or_default(), // Default mints a fresh v7
        ts: EventTimestamp::now(),
        kind: draft.kind,
        namespace: draft.namespace,
        session: draft.session,
        run_id: draft.run_id,
        generation: draft.generation,
        writer: Uuid7::new(),
        seq: 1,
        source: draft.source,
        block_id: draft.block_id,
        parent_id: draft.parent_id,
        data,
        data_ref: None,
        truncated,
    }
}

/// Spill oversize `data` per spec §3.4. Returns `(data, data_ref, truncated)`:
/// - fits inline → unchanged
/// - oversize, blob written → preview + `data_ref` + `truncated:true`
/// - oversize, blob write failed → preview + `truncated:true` (never a drop)
fn prepare_data(
    session_dir: &Path,
    data: Option<serde_json::Value>,
    preview: Option<serde_json::Value>,
) -> (Option<serde_json::Value>, Option<DataRef>, Option<bool>) {
    let Some(data) = data else {
        return (None, None, None);
    };
    let serialized = match serde_json::to_vec(&data) {
        Ok(bytes) => bytes,
        Err(_) => return (Some(data), None, None), // Value serialization cannot fail in practice
    };
    if serialized.len() <= MAX_INLINE_DATA_BYTES {
        return (Some(data), None, None);
    }

    let preview = effective_preview(preview, &serialized);
    match write_blob(session_dir, &serialized) {
        Ok(data_ref) => (Some(preview), Some(data_ref), Some(true)),
        Err(_) => (Some(preview), None, Some(true)),
    }
}

/// A UUIDv7 from an ambient env var (the spec §2 chaining vars).
/// Malformed values warn on stderr and are ignored — a polluted
/// environment must never fail a producer.
#[must_use]
pub fn env_uuid7(var: &str) -> Option<Uuid7> {
    let value = std::env::var(var).ok()?;
    match value.parse::<Uuid7>() {
        Ok(id) => Some(id),
        Err(e) => {
            eprintln!("tendr: ignoring malformed {var}: {e}");
            None
        }
    }
}

/// The ambient causal parent (spec §2 chaining rule):
/// `TENDR_PARENT_EVENT_ID` > `TENDR_BLOCK_ID`.
#[must_use]
pub fn env_parent_chain() -> Option<Uuid7> {
    env_uuid7("TENDR_PARENT_EVENT_ID").or_else(|| env_uuid7("TENDR_BLOCK_ID"))
}

/// The caller's structured preview when it fits `MAX_PREVIEW_BYTES`,
/// else the generic head-of-JSON preview of the full payload.
fn effective_preview(supplied: Option<serde_json::Value>, serialized: &[u8]) -> serde_json::Value {
    supplied
        .filter(|p| serde_json::to_vec(p).is_ok_and(|bytes| bytes.len() <= MAX_PREVIEW_BYTES))
        .unwrap_or_else(|| preview_of(serialized))
}

/// A ≤4 KiB preview object for a spilled payload: the head of the
/// serialized JSON as an opaque string (arbitrary JSON cannot be truncated
/// to valid JSON in place).
fn preview_of(serialized: &[u8]) -> serde_json::Value {
    let text = String::from_utf8_lossy(serialized);
    let mut end = MAX_PREVIEW_BYTES.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    serde_json::json!({ "preview": &text[..end] })
}

/// Write a content-addressed blob under `events/blobs/<sha256>` via
/// temp + rename in the same directory. Identical payloads dedupe by key.
fn write_blob(session_dir: &Path, serialized: &[u8]) -> io::Result<DataRef> {
    let sha256 = hex(&Sha256::digest(serialized));
    let blobs_dir = session_dir.join("events").join("blobs");
    std::fs::create_dir_all(&blobs_dir)?;

    let final_path = blobs_dir.join(&sha256);
    if !final_path.exists() {
        let tmp_path = blobs_dir.join(format!(".tmp-{}", Uuid7::new()));
        let mut tmp = File::create(&tmp_path)?;
        if let Err(e) = tmp.write_all(serialized) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }
        drop(tmp);
        std::fs::rename(&tmp_path, &final_path)?;
    }

    Ok(DataRef {
        path: format!("events/blobs/{sha256}"),
        bytes: serialized.len() as u64,
        sha256,
        media_type: "application/json".to_owned(),
    })
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// A writer starts a new segment once the newest one has reached this many
/// bytes (spec §3.3). Every segment is kept until the session is pruned.
pub const SEGMENT_ROTATE_BYTES: u64 = 64 << 20;

/// The kind of the first record in every segment after the first (spec §3.3).
pub const SEGMENT_OPENED: &str = "segment.opened";

/// The source stamped on `segment.opened`: the event-log layer itself, not
/// the producer whose append happened to start the segment.
const SEGMENT_SOURCE: &str = "tendr.events";

/// [`SEGMENT_ROTATE_BYTES`]. Debug builds accept
/// `TENDR_TEST_EVENT_SEGMENT_BYTES`, so a test crosses the limit without
/// writing 64 MiB.
fn segment_rotate_bytes() -> u64 {
    crate::test_timer::bytes_or_env("TENDR_TEST_EVENT_SEGMENT_BYTES", SEGMENT_ROTATE_BYTES)
}

/// Serialize one event and write it with a single `write_all` (spec §3.2).
fn write_line(file: &mut File, event: &Event) -> io::Result<()> {
    let mut line = serde_json::to_string(event).map_err(io::Error::other)?;
    line.push('\n');
    file.write_all(line.as_bytes())
}

/// A segment this append started because the newest one was full.
struct Rotation {
    segment: String,
    previous: String,
    previous_bytes: u64,
}

/// Open the newest segment for appending, or create one: the first segment
/// when `events/` has none, or a new one when the newest has reached
/// `rotate_at` bytes. Segments are never renamed (spec §3.3). `create_new`
/// losers re-list and open the winner's segment.
fn open_segment(events_dir: &Path, rotate_at: u64) -> io::Result<(File, Option<Rotation>)> {
    for _ in 0..8 {
        let mut full: Option<(String, u64)> = None;
        if let Some(newest) = newest_segment(events_dir)? {
            match OpenOptions::new().append(true).open(&newest) {
                Ok(f) => {
                    // By path: on Windows an append-only handle lacks
                    // FILE_READ_ATTRIBUTES, which File::metadata needs.
                    let bytes = std::fs::metadata(&newest)?.len();
                    if bytes < rotate_at {
                        return Ok((f, None));
                    }
                    full = Some((file_name(&newest), bytes));
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue, // pruned mid-pick
                Err(e) => return Err(e),
            }
        }
        let name = segment_name_after(full.as_ref().map(|(previous, _)| previous.as_str()));
        let candidate = events_dir.join(name);
        match OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&candidate)
        {
            Ok(f) => {
                let rotation = full.map(|(previous, previous_bytes)| Rotation {
                    segment: file_name(&candidate),
                    previous,
                    previous_bytes,
                });
                return Ok((f, rotation));
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue, // race loser
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other(
        "could not open an event segment after repeated races",
    ))
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// A file name for a new segment that sorts after `full`, the segment it
/// replaces as the newest. Normally a fresh UUIDv7; after a backwards clock
/// step (NTP, a restored snapshot) a fresh one would sort first, so every
/// later append would find the full segment newest and rotate again. Then
/// the name is a UUIDv7 1 ms after `full`'s timestamp.
fn segment_name_after(full: Option<&str>) -> String {
    let now = uuid::Uuid::now_v7();
    let floor = full
        .and_then(|name| name.strip_suffix(".jsonl"))
        .and_then(|stem| uuid::Uuid::parse_str(stem).ok());
    let id = match floor.and_then(|floor| floor.get_timestamp().filter(|_| now <= floor)) {
        Some(ts) => {
            let (secs, nanos) = ts.to_unix();
            let millis = secs * 1_000 + u64::from(nanos / 1_000_000) + 1;
            uuid::Uuid::new_v7(uuid::Timestamp::from_unix(
                uuid::NoContext,
                millis / 1_000,
                (millis % 1_000) as u32 * 1_000_000,
            ))
        }
        None => now,
    };
    format!("{id}.jsonl")
}

/// The lexicographically greatest `events/*.jsonl` — UUIDv7 names sort by
/// creation time, so greatest = newest.
fn newest_segment(events_dir: &Path) -> io::Result<Option<PathBuf>> {
    let mut newest: Option<PathBuf> = None;
    for entry in std::fs::read_dir(events_dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|x| x == "jsonl")
            && newest.as_ref().is_none_or(|n| path > *n)
        {
            newest = Some(path);
        }
    }
    Ok(newest)
}

/// Exclusive lock on `<dir>/append.lock`, held until dropped. A dedicated
/// lock file, never the data file, so readers never take it and are never
/// blocked.
///
/// Session event appends take it on Unix only (spec §3.2): on Windows
/// `OpenOptions::append` strips `FILE_WRITE_DATA`, which gives a documented
/// per-`WriteFile` atomic-append contract across processes. lost+found takes
/// it everywhere, because `prune` rewrites that file (spec §7).
struct AppendLock {
    _file: File,
}

impl AppendLock {
    #[cfg(unix)]
    fn acquire(dir: &Path) -> io::Result<Self> {
        use std::os::unix::io::AsRawFd;

        let file = File::create(dir.join("append.lock"))?;
        // SAFETY: file is an open File, so as_raw_fd() returns a valid fd.
        // LOCK_EX is a valid blocking-exclusive flock operation.
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }
        // flock releases on close — dropping the File unlocks.
        Ok(Self { _file: file })
    }

    #[cfg(windows)]
    fn acquire(dir: &Path) -> io::Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{LOCKFILE_EXCLUSIVE_LOCK, LockFileEx};

        let file = File::create(dir.join("append.lock"))?;
        let handle = file.as_raw_handle() as _;
        // SAFETY: an all-zero OVERLAPPED is valid: it locks from offset 0.
        let mut overlapped = unsafe { std::mem::zeroed() };
        // SAFETY: handle is the open file's handle and overlapped outlives the
        // call; a blocking exclusive lock of one byte.
        let ret = unsafe { LockFileEx(handle, LOCKFILE_EXCLUSIVE_LOCK, 0, 1, 0, &mut overlapped) };
        if ret == 0 {
            return Err(io::Error::last_os_error());
        }
        // Closing the handle releases the lock.
        Ok(Self { _file: file })
    }
}

/// Lifecycle event kind for a run status — the shipped watch vocabulary,
/// reused verbatim (spec §1).
#[must_use]
pub fn lifecycle_kind(status: &RunStatus) -> Kind {
    let name = match status {
        RunStatus::Starting => "run.starting",
        RunStatus::Running { .. } => "run.started",
        RunStatus::SpawnFailed { .. } => "run.spawn_failed",
        RunStatus::Exited { how, .. } => match how {
            ExitReason::ExitedOk | ExitReason::ExitedError { .. } => "run.exited",
            ExitReason::Killed | ExitReason::KilledForced => "run.killed",
            ExitReason::TimedOut => "run.timed_out",
            ExitReason::SidecarFailed { .. } => "run.sidecar_failed",
        },
        RunStatus::SidecarLost { .. } => "run.sidecar_lost",
        RunStatus::DependencyFailed { .. } => "run.dependency_failed",
    };
    Kind::new(name).expect("lifecycle kinds satisfy the kind grammar")
}

/// Lifecycle event `data`: watch's data shape (kept intact — watch output is
/// a frozen compat surface) plus `provenance` per spec §1 example (a) and
/// the §4 one-provenance-vocabulary rule.
///
/// When `boundary` is present it is stamped — denormalized and immutable — into
/// the `run.starting` / `run.started` events only (the launch events). This is
/// a copy *derived from* `LaunchSpec.boundary`; historical analytics read it,
/// or join other events to the launch event on `run_id`. Terminal events do not
/// re-carry it (docs/architecture/07-event-protocol.md §1).
#[must_use]
pub fn lifecycle_data(
    status: &RunStatus,
    provenance: &str,
    boundary: Option<&BoundaryContext>,
) -> serde_json::Value {
    let mut data = match status {
        RunStatus::Starting => serde_json::json!({"status": "Starting"}),
        RunStatus::Running { .. } => serde_json::json!({"status": "Running"}),
        RunStatus::SpawnFailed { .. } => serde_json::json!({"status": "SpawnFailed"}),
        RunStatus::Exited { how, .. } => match how {
            ExitReason::ExitedOk => {
                serde_json::json!({"status": "Exited", "reason": "ExitedOk", "exit_code": 0})
            }
            ExitReason::ExitedError { code } => {
                serde_json::json!({"status": "Exited", "reason": "ExitedError", "exit_code": code.get()})
            }
            ExitReason::Killed => serde_json::json!({"status": "Exited", "reason": "Killed"}),
            ExitReason::KilledForced => {
                serde_json::json!({"status": "Exited", "reason": "KilledForced"})
            }
            ExitReason::TimedOut => {
                serde_json::json!({"status": "Exited", "reason": "TimedOut"})
            }
            ExitReason::SidecarFailed { step } => {
                serde_json::json!({"status": "Exited", "reason": "SidecarFailed", "step": step.as_str()})
            }
        },
        RunStatus::SidecarLost { .. } => serde_json::json!({"status": "SidecarLost"}),
        RunStatus::DependencyFailed { reason, .. } => {
            let reason_str = match reason {
                DepFailReason::Failed => "Failed",
                DepFailReason::TimedOut => "TimedOut",
                DepFailReason::Killed => "Killed",
                DepFailReason::KilledForced => "KilledForced",
            };
            serde_json::json!({"status": "DependencyFailed", "reason": reason_str})
        }
    };
    data["provenance"] = serde_json::Value::String(provenance.to_owned());
    // The boundary snapshot rides only the launch events; terminal/inferred
    // events join back to it on run_id.
    if let Some(boundary) = boundary {
        if matches!(status, RunStatus::Starting | RunStatus::Running { .. }) {
            data["boundary"] =
                serde_json::to_value(boundary).expect("BoundaryContext is always serializable");
        }
    }
    data
}

/// Result of reading a session's event log.
#[derive(Debug)]
pub struct ReadOutcome {
    /// Events merged by `(ts, writer, seq)` (spec §4 deterministic merge).
    pub events: Vec<Event>,
    /// Lines that failed to parse (torn writes, foreign fragments).
    /// JSONL is self-synchronizing: readers resync at the next newline.
    pub skipped: usize,
}

/// Deterministic cross-writer merge key (spec §4): `(ts, writer, seq)`.
/// UUID `u128` ordering matches the canonical string ordering.
#[must_use]
pub fn merge_key(event: &Event) -> (EventTimestamp, u128, u64) {
    (event.ts, event.writer.as_uuid().as_u128(), event.seq)
}

/// Read every segment of one session's event log, in segment-name order,
/// then merge by `(ts, writer, seq)`. A missing `events/` dir is an empty
/// log, not an error.
///
/// # Errors
/// Returns IO errors from directory listing or file reads; parse failures
/// are counted in `skipped`, never fatal.
pub fn read_session_events(session_dir: &Path) -> io::Result<ReadOutcome> {
    let events_dir = session_dir.join("events");
    let mut outcome = ReadOutcome {
        events: Vec::new(),
        skipped: 0,
    };
    if !events_dir.exists() {
        return Ok(outcome);
    }

    let mut segments: Vec<PathBuf> = std::fs::read_dir(&events_dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    segments.sort();

    for segment in segments {
        // Byte-level resync: JSONL is self-synchronizing (spec §3.2 defense
        // in depth). A non-UTF8 fragment is a counted skip like any other
        // unparseable line — never a fatal error for the whole replay.
        let content = std::fs::read(&segment)?;
        for raw_line in content.split(|b| *b == b'\n') {
            let raw_line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
            if raw_line.is_empty() {
                continue;
            }
            let parsed = std::str::from_utf8(raw_line)
                .ok()
                .and_then(|line| serde_json::from_str::<Event>(line).ok());
            match parsed {
                Some(event) => outcome.events.push(event),
                None => outcome.skipped += 1,
            }
        }
    }

    outcome.events.sort_by_key(merge_key);
    Ok(outcome)
}

/// Cursor token version (spec §5.2). Unknown versions are treated as
/// cursor-gone by the CLI, never silently accepted.
pub const CURSOR_VERSION: u32 = 1;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CursorError {
    #[error("cursor is not valid URL-safe base64")]
    Base64(#[from] base64::DecodeError),
    #[error("cursor payload is not valid JSON")]
    Json(#[from] serde_json::Error),
    #[error("unknown cursor version {0} (this tendr speaks v{CURSOR_VERSION})")]
    UnknownVersion(u32),
}

/// Wire shape of a cursor token: `{"v":1,"s":[["<relpath>", offset], …]}`
/// where relpaths are `<ns>/<session>/events/<seg>.jsonl` under the sessions
/// root and offsets are byte positions after the last fully-consumed line.
#[derive(Serialize, Deserialize)]
struct CursorToken {
    v: u32,
    s: Vec<(String, u64)>,
}

/// Encode per-segment read offsets as an opaque URL-safe base64 token
/// (spec §5.2). Deterministic: streams serialize in path order.
#[must_use]
pub fn encode_cursor(streams: &BTreeMap<String, u64>) -> String {
    let token = CursorToken {
        v: CURSOR_VERSION,
        s: streams.iter().map(|(k, v)| (k.clone(), *v)).collect(),
    };
    let json = serde_json::to_string(&token).expect("cursor token serialization cannot fail");
    URL_SAFE_NO_PAD.encode(json)
}

/// Decode a cursor token back to per-segment offsets.
///
/// # Errors
/// Returns `CursorError` for malformed base64/JSON or an unknown version —
/// all mapped to cursor-gone (exit 44) at the CLI layer.
pub fn decode_cursor(token: &str) -> Result<BTreeMap<String, u64>, CursorError> {
    let bytes = URL_SAFE_NO_PAD.decode(token)?;
    let parsed: CursorToken = serde_json::from_slice(&bytes)?;
    if parsed.v != CURSOR_VERSION {
        return Err(CursorError::UnknownVersion(parsed.v));
    }
    Ok(parsed.s.into_iter().collect())
}

/// One parsed event with the byte range of its line within its segment.
/// `end` is the offset just past the line's `\n` — the resume position
/// after consuming this record.
#[derive(Debug)]
pub struct SegmentRecord {
    pub event: Event,
    pub start: u64,
    pub end: u64,
}

/// Result of one offset-aware segment read.
#[derive(Debug)]
pub struct SegmentReadOutcome {
    pub records: Vec<SegmentRecord>,
    /// Complete lines that failed to parse (torn writes, foreign fragments).
    pub skipped: usize,
    /// Offset after the last fully-consumed (`\n`-terminated) line. An
    /// unterminated tail is a write in progress — never consumed, never
    /// counted as a skip; the next poll retries from here.
    pub consumed_to: u64,
}

/// Read complete lines of one segment starting at byte offset `from`
/// (spec §5.1 follow tailing). Only `\n`-terminated lines are consumed,
/// so a torn tail line under follow waits for its writer instead of being
/// mis-counted as corrupt.
///
/// # Errors
/// Returns IO errors from opening or reading the segment.
pub fn read_segment_records(path: &Path, from: u64) -> io::Result<SegmentReadOutcome> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(from))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;

    let mut outcome = SegmentReadOutcome {
        records: Vec::new(),
        skipped: 0,
        consumed_to: from,
    };
    let mut line_start = 0usize;
    while let Some(nl) = buf[line_start..].iter().position(|b| *b == b'\n') {
        let line_end = line_start + nl;
        let raw = &buf[line_start..line_end];
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        if !raw.is_empty() {
            let parsed = std::str::from_utf8(raw)
                .ok()
                .and_then(|line| serde_json::from_str::<Event>(line).ok());
            match parsed {
                Some(event) => outcome.records.push(SegmentRecord {
                    event,
                    start: from + line_start as u64,
                    end: from + line_end as u64 + 1,
                }),
                None => outcome.skipped += 1,
            }
        }
        outcome.consumed_to = from + line_end as u64 + 1;
        line_start = line_end + 1;
    }
    Ok(outcome)
}

/// Project one `output.log` O/E line as a derived event (spec §5.1
/// `--include-logs`): read-time only, no stored identity (`id`/`writer`/
/// `seq`), `derived:true`, f64 seconds converted to the envelope ts format.
/// Annotation (`A`) and unknown tags project to nothing.
#[must_use]
pub fn project_log_line(
    line: &LogLine,
    namespace: &str,
    session: &str,
    run_id: &str,
) -> Option<serde_json::Value> {
    let kind = match line.tag.as_str() {
        "O" => "log.stdout",
        "E" => "log.stderr",
        _ => return None,
    };
    Some(serde_json::json!({
        "kind": kind,
        "ts": EventTimestamp::from_epoch_secs_f64(line.ts).to_string(),
        "derived": true,
        "namespace": namespace,
        "session": session,
        "run_id": run_id,
        "source": "tendr.sidecar",
        "data": {"content": line.format_raw()},
    }))
}

/// Append a fully-addressed event to `<tendr-root>/lost+found/events.jsonl`
/// (spec §7): emits from a process whose session dir was pruned or replaced
/// keep their data without resurrecting the session dir.
///
/// The lock is taken before the file is opened, so a line never goes to a
/// file that [`sweep_lost_found`] has just replaced.
///
/// # Errors
/// Returns IO errors from creating or appending to the lost+found log.
pub fn append_lost_found(tendr_root: &Path, event: &Event) -> io::Result<()> {
    let dir = tendr_root.join("lost+found");
    std::fs::create_dir_all(&dir)?;

    let mut line = serde_json::to_string(event).map_err(io::Error::other)?;
    line.push('\n');

    let _lock = AppendLock::acquire(&dir)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(LOST_FOUND_FILE))?;
    file.write_all(line.as_bytes())
}

const LOST_FOUND_FILE: &str = "events.jsonl";

/// What [`sweep_lost_found`] removed, or in a dry run would remove.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LostFoundSweep {
    /// Events removed.
    pub events: u64,
    /// Bytes of the removed lines.
    pub bytes: u64,
}

/// The fields a sweep needs from a lost+found line; the rest is ignored.
#[derive(Deserialize)]
struct DatedLine {
    ts: EventTimestamp,
    namespace: String,
}

/// Remove the lost+found events stamped before `cutoff` (spec §7), only
/// those of `namespace` when it is given. A line the sweep cannot date or
/// attribute, and an unfinished last line, are kept.
///
/// The file is streamed twice under the lock appenders take before they
/// open it: once to count, then, when something expires, into a temporary
/// file that is renamed over the original. So an append either lands before
/// the sweep reads the file or goes to the new file, and memory stays at
/// one line. Nothing is written when nothing is removed or `dry_run` is set.
///
/// # Errors
/// Returns IO errors from locking, reading or replacing the file.
pub fn sweep_lost_found(
    tendr_root: &Path,
    cutoff: EventTimestamp,
    namespace: Option<&Namespace>,
    dry_run: bool,
) -> io::Result<LostFoundSweep> {
    let dir = tendr_root.join("lost+found");
    let path = dir.join(LOST_FOUND_FILE);
    if !path.is_file() {
        return Ok(LostFoundSweep::default());
    }
    let _lock = AppendLock::acquire(&dir)?;
    let expired = |line: &[u8]| {
        line.ends_with(b"\n")
            && serde_json::from_slice::<DatedLine>(line).is_ok_and(|dated| {
                dated.ts < cutoff
                    && namespace.is_none_or(|namespace| dated.namespace == namespace.as_str())
            })
    };

    let mut swept = LostFoundSweep::default();
    let mut kept_any = false;
    let counted = for_each_line(&path, |line| {
        if expired(line) {
            swept.events += 1;
            swept.bytes += line.len() as u64;
        } else {
            kept_any = true;
        }
        Ok(())
    });
    match counted {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(LostFoundSweep::default()),
        Err(e) => return Err(e),
    }

    if swept.events == 0 || dry_run {
        return Ok(swept);
    }
    if !kept_any {
        std::fs::remove_file(&path)?;
        return Ok(swept);
    }
    let tmp = dir.join(format!("{LOST_FOUND_FILE}.tmp"));
    let mut out = io::BufWriter::new(File::create(&tmp)?);
    for_each_line(&path, |line| {
        if expired(line) {
            Ok(())
        } else {
            out.write_all(line)
        }
    })?;
    let file = out.into_inner().map_err(io::IntoInnerError::into_error)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, &path)?;
    Ok(swept)
}

/// Call `f` with each line of `path`, its `\n` included; an unfinished last
/// line comes without one.
fn for_each_line(path: &Path, mut f: impl FnMut(&[u8]) -> io::Result<()>) -> io::Result<()> {
    use std::io::BufRead;

    let mut reader = io::BufReader::new(File::open(path)?);
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        f(&line)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// After a backwards clock step the full segment's name is in the future.
    /// The segment that replaces it must still sort after it, or every later
    /// append finds the full one newest and rotates again.
    #[test]
    fn rotation_after_a_backwards_clock_step_sorts_after_the_full_segment() {
        let tmp = tempfile::TempDir::new().unwrap();
        let events_dir = tmp.path();
        let day_ahead = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 86_400;
        let future = uuid::Uuid::new_v7(uuid::Timestamp::from_unix(uuid::NoContext, day_ahead, 0));
        let full = events_dir.join(format!("{future}.jsonl"));
        std::fs::write(&full, vec![b'x'; 1_000]).unwrap();

        let mut rotations = 0;
        for _ in 0..3 {
            let (mut file, rotation) = open_segment(events_dir, 1_000).unwrap();
            rotations += usize::from(rotation.is_some());
            file.write_all(b"{}\n").unwrap();
        }

        assert_eq!(
            rotations, 1,
            "one rotation, then appends to the new segment"
        );
        let newest = newest_segment(events_dir).unwrap().unwrap();
        assert!(
            newest > full,
            "{} sorts after {}",
            newest.display(),
            full.display()
        );
        assert_eq!(std::fs::read(&newest).unwrap(), b"{}\n{}\n{}\n");
    }
}
