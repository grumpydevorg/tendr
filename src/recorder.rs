//! The sidecar's exact PTY recorder: one writer thread per recorded run.
//!
//! Capture offers output bytes and the input writer offers applied window
//! sizes through a shared sequencer, which assigns each record its sequence and
//! elapsed time and places it in a byte-bounded backlog without waiting for
//! storage. The recorder thread encodes records with [`SegmentEncoder`], rotates
//! segments, publishes each segment atomically (header synced, then renamed),
//! and syncs at most [`SYNC_INTERVAL`] apart and when a segment closes.
//!
//! Recording ends explicitly, never silently. Reaching the size limit, a write
//! or sync failure, a full backlog, or storage still stalled at close stops it
//! at the last completed append: [`Stopped`] carries that sequence and the
//! reason, an append that completes after the stop is cut off again, and the
//! stop callback runs once, at the stop, so it has run by the time
//! [`RecorderThread::finish`] returns. The callback only hands the stop on
//! (the sidecar queues it for its effects thread), so a stalled disk cannot
//! hold up capture. The recorded process and its viewers are unaffected. Wire
//! format:
//! `docs/architecture/08-pty-recording-format.md`.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::recording::{
    Geometry, Record, RecordKind, ResizeCause, SegmentEncoder, SegmentHeader, Sequence,
};

/// Rotate to a new segment once the current one reaches this size.
pub const DEFAULT_SEGMENT_BYTES: u64 = 64 << 20;
/// Stop recording rather than let a run's segments exceed this.
pub const DEFAULT_MAX_BYTES: u64 = 1 << 30;
/// Stop recording if this many payload bytes are waiting for storage.
pub const DEFAULT_BACKLOG_BYTES: usize = 16 << 20;
/// How long closing waits for storage before declaring it stalled.
pub const DEFAULT_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest interval between syncs while records are being appended.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(1);

/// Limits for one run's recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecorderLimits {
    pub segment_bytes: u64,
    /// Every segment byte counts, headers included.
    pub max_bytes: u64,
    pub backlog_bytes: usize,
    pub close_timeout: Duration,
}

impl Default for RecorderLimits {
    fn default() -> Self {
        Self {
            segment_bytes: DEFAULT_SEGMENT_BYTES,
            max_bytes: DEFAULT_MAX_BYTES,
            backlog_bytes: DEFAULT_BACKLOG_BYTES,
            close_timeout: DEFAULT_CLOSE_TIMEOUT,
        }
    }
}

/// Why recording stopped before the run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The next record would have taken the run past [`RecorderLimits::max_bytes`].
    SizeLimit,
    /// Storage rejected a write, sync, or segment publication.
    WriteFailed(io::ErrorKind),
    /// Storage fell [`RecorderLimits::backlog_bytes`] behind.
    BacklogFull,
    /// Storage had not finished when the recording closed.
    Stalled,
}

impl StopReason {
    /// Stable name for events, metadata, and warnings.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SizeLimit => "size_limit",
            Self::WriteFailed(_) => "write_failed",
            Self::BacklogFull => "backlog_full",
            Self::Stalled => "stalled",
        }
    }
}

/// Recording ended early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stopped {
    /// The last record appended before the stop; nothing later is recorded.
    pub last_recorded: Option<Sequence>,
    pub reason: StopReason,
}

/// How a recording ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecorderSummary {
    /// The last record appended.
    pub last_recorded: Option<Sequence>,
    /// The last record known to be synced to stable storage.
    pub last_synced: Option<Sequence>,
    /// `Some` if recording stopped before the run ended.
    pub stopped: Option<Stopped>,
    /// Segments published.
    pub segments: u32,
}

/// Where segments are written.
pub trait SegmentStore: Send + 'static {
    /// Durably publish segment `index` holding `header`, and return a writer
    /// positioned after it.
    ///
    /// # Errors
    ///
    /// Storage failures.
    fn publish(&mut self, index: u32, header: &[u8]) -> io::Result<Box<dyn SegmentFile>>;
}

/// An open, published segment.
pub trait SegmentFile: Write + Send {
    /// Sync appended bytes to stable storage.
    ///
    /// # Errors
    ///
    /// Storage failures.
    fn sync(&mut self) -> io::Result<()>;

    /// Cut the segment back to `len` bytes, header included; later writes
    /// append after it.
    ///
    /// # Errors
    ///
    /// Storage failures.
    fn truncate(&mut self, len: u64) -> io::Result<()>;
}

/// Segments in one directory: `seg-<index:08>.tndrrec`. On Unix the directory
/// and its parent are owner-only (`0700`) and segment files `0600`. Each
/// segment is written as a temporary file holding its header, synced, and
/// renamed into place before any record is appended.
pub struct DirectoryStore {
    dir: PathBuf,
}

const SEGMENT_PREFIX: &str = "seg-";
const SEGMENT_SUFFIX: &str = ".tndrrec";

impl DirectoryStore {
    /// A store for `dir`, created when the first segment is published.
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The published segment files in `dir`, in recording order.
    ///
    /// # Errors
    ///
    /// Reading the directory.
    pub fn segments(dir: &Path) -> io::Result<Vec<PathBuf>> {
        let mut segments = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let name = entry?.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let index = name
                .strip_prefix(SEGMENT_PREFIX)
                .and_then(|rest| rest.strip_suffix(SEGMENT_SUFFIX));
            if index.is_some_and(|i| i.len() == 8 && i.bytes().all(|b| b.is_ascii_digit())) {
                segments.push(dir.join(name));
            }
        }
        segments.sort();
        Ok(segments)
    }

    fn create_dirs(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            let private = |dir: &Path| {
                crate::attach_socket::prepare_private_dir(dir).map_err(io::Error::other)
            };
            if let Some(parent) = self.dir.parent() {
                private(parent)?;
            }
            private(&self.dir)
        }
        #[cfg(not(unix))]
        {
            std::fs::create_dir_all(&self.dir)
        }
    }
}

impl SegmentStore for DirectoryStore {
    fn publish(&mut self, index: u32, header: &[u8]) -> io::Result<Box<dyn SegmentFile>> {
        if index == 0 {
            self.create_dirs()?;
        }
        let name = format!("{SEGMENT_PREFIX}{index:08}{SEGMENT_SUFFIX}");
        let path = self.dir.join(&name);
        let tmp = self.dir.join(format!("{name}.tmp"));

        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&tmp)?;
        let published = file
            .write_all(header)
            .and_then(|()| file.sync_all())
            .and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(e) = published {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        #[cfg(unix)]
        std::fs::File::open(&self.dir)?.sync_all()?;
        Ok(Box::new(DiskSegment { file }))
    }
}

struct DiskSegment {
    file: std::fs::File,
}

impl Write for DiskSegment {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl SegmentFile for DiskSegment {
    fn sync(&mut self) -> io::Result<()> {
        self.file.sync_data()
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        use std::io::Seek;
        self.file.set_len(len)?;
        self.file.seek(io::SeekFrom::Start(len)).map(|_| ())
    }
}

/// Producer side of a run's recorder. Cheap to clone; never waits for storage.
#[derive(Clone)]
pub struct Recorder {
    shared: Arc<Shared>,
}

struct Shared {
    limits: RecorderLimits,
    origin: Instant,
    state: Mutex<State>,
    /// Wakes the writer: a record, a stop, or close.
    work: Condvar,
    /// Wakes waiters on the outcome: a stop, or the writer finishing.
    settled: Condvar,
}

/// Told of the stop, once. See [`RecorderThread::start`].
type StopReport = Box<dyn FnOnce(Stopped) + Send>;

struct State {
    /// The sequence the next record takes; `None` once exhausted.
    next: Option<Sequence>,
    backlog: VecDeque<Record>,
    backlog_bytes: usize,
    accepting: bool,
    /// The last record whose append completed before any stop.
    appended: Option<Sequence>,
    synced: Option<Sequence>,
    segments: u32,
    stopped: Option<Stopped>,
    /// Taken by the stop that reports.
    on_stop: Option<StopReport>,
    writer_done: bool,
}

/// Backlog cost of a resize record, which carries no payload bytes.
const RESIZE_COST: usize = 16;

fn cost(record: &Record) -> usize {
    match &record.kind {
        RecordKind::Output(bytes) | RecordKind::Input(bytes) => bytes.len(),
        RecordKind::Resize { .. } => RESIZE_COST,
    }
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// End the recording at the last completed append and report it. Queued
    /// records are discarded; the first stop wins.
    fn stop(&self, state: &mut State, reason: StopReason) {
        if state.stopped.is_some() {
            return;
        }
        let stopped = Stopped {
            last_recorded: state.appended,
            reason,
        };
        state.stopped = Some(stopped);
        state.backlog.clear();
        state.backlog_bytes = 0;
        if let Some(report) = state.on_stop.take() {
            report(stopped);
        }
        self.work.notify_all();
        self.settled.notify_all();
    }

    /// Sequence `kind` and queue it for the writer, or stop if the backlog has
    /// no room for it.
    fn enqueue(&self, state: &mut State, kind: RecordKind) {
        if state.stopped.is_some() || !state.accepting {
            return;
        }
        let Some(sequence) = state.next else {
            self.stop(state, StopReason::SizeLimit);
            return;
        };
        let record = Record {
            sequence,
            elapsed_ns: u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX),
            kind,
        };
        let cost = cost(&record);
        if state.backlog_bytes.saturating_add(cost) > self.limits.backlog_bytes {
            self.stop(state, StopReason::BacklogFull);
            return;
        }
        state.next = sequence.next();
        state.backlog.push_back(record);
        state.backlog_bytes += cost;
        self.work.notify_one();
    }
}

/// A running recorder: [`Recorder`] handles for producers, and its threads.
pub struct RecorderThread {
    recorder: Recorder,
}

impl RecorderThread {
    /// Start recording under `header`, which must describe segment 0.
    ///
    /// `on_stop` runs once if recording stops early, at the stop: on the
    /// producer, the storage writer or [`RecorderThread::finish`], whichever
    /// stops it, with the recorder locked. So it must return promptly, must
    /// not touch storage that may stall, and must not call back into the
    /// recorder; the sidecar's only queues the stop. Once `finish` returns it
    /// has run, if it ever will.
    pub fn start<S: SegmentStore>(
        header: SegmentHeader,
        store: S,
        limits: RecorderLimits,
        on_stop: impl FnOnce(Stopped) + Send + 'static,
    ) -> Self {
        let shared = Arc::new(Shared {
            limits,
            origin: Instant::now(),
            state: Mutex::new(State {
                next: Some(header.first_sequence),
                backlog: VecDeque::new(),
                backlog_bytes: 0,
                accepting: true,
                appended: None,
                synced: None,
                segments: 0,
                stopped: None,
                on_stop: Some(Box::new(on_stop)),
                writer_done: false,
            }),
            work: Condvar::new(),
            settled: Condvar::new(),
        });

        let writer = Arc::clone(&shared);
        std::thread::spawn(move || {
            match SegmentEncoder::new(header) {
                Ok(encoder) => Writer::new(&writer, encoder, store).run(),
                Err(_) => {
                    let mut state = writer.lock();
                    writer.stop(
                        &mut state,
                        StopReason::WriteFailed(io::ErrorKind::InvalidInput),
                    );
                }
            }
            let mut state = writer.lock();
            state.writer_done = true;
            writer.settled.notify_all();
        });

        Self {
            recorder: Recorder { shared },
        }
    }

    #[must_use]
    pub fn recorder(&self) -> Recorder {
        self.recorder.clone()
    }

    /// Accept nothing further, append what is queued, sync, and return how the
    /// recording ended. Storage still busy after
    /// [`RecorderLimits::close_timeout`] stops the recording as
    /// [`StopReason::Stalled`]. Any stop has been reported when this returns.
    #[must_use]
    pub fn finish(self) -> RecorderSummary {
        let shared = &self.recorder.shared;
        let timeout = shared.limits.close_timeout;
        let mut state = shared.lock();
        state.accepting = false;
        shared.work.notify_all();

        let deadline = Instant::now() + timeout;
        state = wait_until(shared, state, deadline, |s| s.writer_done);
        if !state.writer_done {
            shared.stop(&mut state, StopReason::Stalled);
        }

        RecorderSummary {
            last_recorded: state.appended,
            last_synced: state.synced,
            stopped: state.stopped,
            segments: state.segments,
        }
    }
}

/// Wait on `settled` until `done` holds or `deadline` passes.
fn wait_until<'a>(
    shared: &'a Shared,
    mut state: std::sync::MutexGuard<'a, State>,
    deadline: Instant,
    done: impl Fn(&State) -> bool,
) -> std::sync::MutexGuard<'a, State> {
    while !done(&state) {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        state = shared
            .settled
            .wait_timeout(state, deadline - now)
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0;
    }
    state
}

impl Recorder {
    /// Record PTY output.
    pub fn output(&self, bytes: &[u8]) {
        let mut state = self.shared.lock();
        for chunk in bytes.chunks(crate::recording::MAX_PAYLOAD) {
            self.shared
                .enqueue(&mut state, RecordKind::Output(chunk.to_vec()));
        }
    }

    /// Apply a window size with `apply` and, if it succeeds, record it. No
    /// output is sequenced while `apply` runs, so output the child produces in
    /// response to the new size is always recorded after the resize.
    ///
    /// # Errors
    ///
    /// `apply`'s error; nothing is recorded.
    pub fn resize_applied(
        &self,
        geometry: Geometry,
        cause: ResizeCause,
        apply: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        let mut state = self.shared.lock();
        apply()?;
        self.shared
            .enqueue(&mut state, RecordKind::Resize { geometry, cause });
        Ok(())
    }

    /// The stop, once recording has stopped.
    #[must_use]
    pub fn stopped(&self) -> Option<Stopped> {
        self.shared.lock().stopped
    }
}

/// The recorder thread's storage side.
struct Writer<'a, S> {
    shared: &'a Shared,
    store: S,
    encoder: SegmentEncoder,
    file: Option<Box<dyn SegmentFile>>,
    segment_len: u64,
    total_len: u64,
    last_appended: Option<Sequence>,
    unsynced: bool,
    last_sync: Instant,
}

impl<'a, S: SegmentStore> Writer<'a, S> {
    fn new(shared: &'a Shared, encoder: SegmentEncoder, store: S) -> Self {
        Self {
            shared,
            store,
            encoder,
            file: None,
            segment_len: 0,
            total_len: 0,
            last_appended: None,
            unsynced: false,
            last_sync: Instant::now(),
        }
    }

    fn run(mut self) {
        if let Err(reason) = self.publish(self.encoder.header_bytes(), 0) {
            let mut state = self.shared.lock();
            self.shared.stop(&mut state, reason);
            return;
        }

        while let Some(record) = self.next_record() {
            let sequence = record.sequence;
            let prepared = self.prepare(&record);
            let before = self.segment_len;
            let attempted = prepared.is_ok();
            let result = prepared.and_then(|bytes| self.append(&bytes));

            let mut state = self.shared.lock();
            if state.stopped.is_some() {
                // Stopped while this append ran: it lies beyond the boundary.
                drop(state);
                if attempted {
                    self.cut_back(before);
                }
                break;
            }
            if let Err(reason) = result {
                self.shared.stop(&mut state, reason);
                drop(state);
                if attempted {
                    self.cut_back(before);
                }
                break;
            }
            state.appended = Some(sequence);
            drop(state);
            self.last_appended = Some(sequence);

            if self.last_sync.elapsed() >= SYNC_INTERVAL {
                if let Err(reason) = self.sync() {
                    let mut state = self.shared.lock();
                    self.shared.stop(&mut state, reason);
                    break;
                }
            }
        }

        if let Err(reason) = self.sync() {
            let mut state = self.shared.lock();
            self.shared.stop(&mut state, reason);
        }
    }

    /// The next queued record, syncing while idle; `None` once stopped or
    /// closed with nothing queued.
    fn next_record(&mut self) -> Option<Record> {
        let mut state = self.shared.lock();
        loop {
            if state.stopped.is_some() {
                return None;
            }
            if let Some(record) = state.backlog.pop_front() {
                state.backlog_bytes -= cost(&record);
                return Some(record);
            }
            if !state.accepting {
                return None;
            }
            if self.unsynced {
                let due = self.last_sync + SYNC_INTERVAL;
                let now = Instant::now();
                if now >= due {
                    drop(state);
                    if let Err(reason) = self.sync() {
                        let mut state = self.shared.lock();
                        self.shared.stop(&mut state, reason);
                        return None;
                    }
                    state = self.shared.lock();
                    continue;
                }
                state = self
                    .shared
                    .work
                    .wait_timeout(state, due - now)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
            } else {
                state = self
                    .shared
                    .work
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
    }

    /// Encode `record`, rotating to a new segment first if the current one is
    /// full, within the run's size limit.
    fn prepare(&mut self, record: &Record) -> Result<Vec<u8>, StopReason> {
        let limits = self.shared.limits;
        if self.segment_len >= limits.segment_bytes {
            if let Ok(mut next) = self.encoder.next_segment() {
                let bytes = next.encode(record).map_err(|_| invalid())?;
                let header = next.header_bytes();
                self.check_size(header.len() + bytes.len())?;
                self.sync()?;
                self.publish(header, next.header().segment_index)?;
                self.encoder = next;
                return Ok(bytes);
            }
        }
        let bytes = self.encoder.encode(record).map_err(|_| invalid())?;
        self.check_size(bytes.len())?;
        Ok(bytes)
    }

    fn check_size(&self, adding: usize) -> Result<(), StopReason> {
        let adding = u64::try_from(adding).unwrap_or(u64::MAX);
        if self.total_len.saturating_add(adding) > self.shared.limits.max_bytes {
            Err(StopReason::SizeLimit)
        } else {
            Ok(())
        }
    }

    fn publish(&mut self, header: Vec<u8>, index: u32) -> Result<(), StopReason> {
        self.check_size(header.len())?;
        let file = self.store.publish(index, &header).map_err(write_failed)?;
        let len = header.len() as u64;
        self.file = Some(file);
        self.segment_len = len;
        self.total_len += len;
        self.shared.lock().segments = index + 1;
        Ok(())
    }

    fn append(&mut self, bytes: &[u8]) -> Result<(), StopReason> {
        let file = self.file.as_mut().expect("a segment is open");
        file.write_all(bytes).map_err(write_failed)?;
        let len = bytes.len() as u64;
        self.segment_len += len;
        self.total_len += len;
        self.unsynced = true;
        Ok(())
    }

    /// Remove whatever an append beyond the recorded boundary left behind. Best
    /// effort: a segment that cannot be cut back ends in bytes past the declared
    /// boundary, which the stop report already excludes.
    fn cut_back(&mut self, len: u64) {
        if let Some(file) = self.file.as_mut() {
            if file.truncate(len).is_ok() {
                self.total_len -= self.segment_len.saturating_sub(len);
                self.segment_len = len;
            }
        }
    }

    fn sync(&mut self) -> Result<(), StopReason> {
        if !self.unsynced {
            return Ok(());
        }
        let file = self.file.as_mut().expect("a segment is open");
        file.sync().map_err(write_failed)?;
        self.unsynced = false;
        self.last_sync = Instant::now();
        self.shared.lock().synced = self.last_appended;
        Ok(())
    }
}

fn write_failed(e: io::Error) -> StopReason {
    StopReason::WriteFailed(e.kind())
}

/// A record the encoder refused; the sequencer never produces one.
fn invalid() -> StopReason {
    StopReason::WriteFailed(io::ErrorKind::InvalidData)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ids::RunId;
    use crate::recording::{DecodedRecording, SegmentEnd, TermName, decode_recording};

    fn header() -> SegmentHeader {
        SegmentHeader {
            run_id: RunId::new(),
            segment_index: 0,
            first_sequence: Sequence::FIRST,
            origin_unix_ns: 1,
            geometry: Geometry::new(24, 80).unwrap(),
            input_recorded: false,
            term: TermName::new("xterm-256color").unwrap(),
        }
    }

    type Gate = Arc<(Mutex<bool>, Condvar)>;

    fn open(gate: &Gate) {
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
    }

    /// An in-memory store whose writes can fail or wait on a gate.
    #[derive(Clone, Default)]
    struct MemoryStore {
        segments: Arc<Mutex<Vec<Vec<u8>>>>,
        fail_after_bytes: Option<usize>,
        gate: Option<Gate>,
        /// Writes that have reached the gate.
        at_gate: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl MemoryStore {
        fn total_bytes(&self) -> usize {
            self.segments.lock().unwrap().iter().map(Vec::len).sum()
        }

        fn decoded(&self) -> DecodedRecording {
            let segments = self.segments.lock().unwrap().clone();
            decode_recording(segments.iter().map(Vec::as_slice)).expect("recording decodes")
        }
    }

    struct MemoryFile {
        store: MemoryStore,
        index: usize,
    }

    impl SegmentStore for MemoryStore {
        fn publish(&mut self, index: u32, header: &[u8]) -> io::Result<Box<dyn SegmentFile>> {
            let mut segments = self.segments.lock().unwrap();
            assert_eq!(segments.len(), index as usize, "segments publish in order");
            segments.push(header.to_vec());
            Ok(Box::new(MemoryFile {
                store: self.clone(),
                index: index as usize,
            }))
        }
    }

    impl Write for MemoryFile {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if let Some(gate) = &self.store.gate {
                self.store
                    .at_gate
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let (open, cv) = &**gate;
                let mut open = open.lock().unwrap();
                while !*open {
                    open = cv.wait(open).unwrap();
                }
            }
            let written = self.store.total_bytes();
            if self
                .store
                .fail_after_bytes
                .is_some_and(|limit| written + buf.len() > limit)
            {
                return Err(io::ErrorKind::StorageFull.into());
            }
            self.store.segments.lock().unwrap()[self.index].extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl SegmentFile for MemoryFile {
        fn sync(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn truncate(&mut self, len: u64) -> io::Result<()> {
            let mut segments = self.store.segments.lock().unwrap();
            segments[self.index].truncate(usize::try_from(len).unwrap());
            Ok(())
        }
    }

    fn outputs(recording: &DecodedRecording) -> Vec<u8> {
        recording
            .records
            .iter()
            .filter_map(|r| match &r.kind {
                RecordKind::Output(bytes) => Some(bytes.as_slice()),
                _ => None,
            })
            .flatten()
            .copied()
            .collect()
    }

    fn last_sequence(recording: &DecodedRecording) -> Option<Sequence> {
        recording.records.last().map(|r| r.sequence)
    }

    #[test]
    fn output_and_resizes_are_recorded_exactly_and_in_order() {
        let store = MemoryStore::default();
        let thread =
            RecorderThread::start(header(), store.clone(), RecorderLimits::default(), |_| {
                panic!("no stop expected")
            });
        let recorder = thread.recorder();
        recorder.output(b"before\x1b[");
        recorder
            .resize_applied(
                Geometry::new(30, 100).unwrap(),
                ResizeCause::User,
                || Ok(()),
            )
            .unwrap();
        recorder.output(b"31m\xff\xfe");
        let summary = thread.finish();

        assert_eq!(summary.stopped, None);
        assert_eq!(summary.last_recorded.map(Sequence::get), Some(3));
        assert_eq!(
            summary.last_synced, summary.last_recorded,
            "synced at close"
        );
        let recording = store.decoded();
        assert_eq!(recording.end, SegmentEnd::Clean);
        assert_eq!(outputs(&recording), b"before\x1b[31m\xff\xfe");
        assert_eq!(
            recording.records[1].kind,
            RecordKind::Resize {
                geometry: Geometry::new(30, 100).unwrap(),
                cause: ResizeCause::User
            }
        );
    }

    #[test]
    fn a_failed_resize_is_not_recorded() {
        let store = MemoryStore::default();
        let thread =
            RecorderThread::start(header(), store.clone(), RecorderLimits::default(), |_| {});
        let recorder = thread.recorder();
        let err = recorder
            .resize_applied(Geometry::new(30, 100).unwrap(), ResizeCause::User, || {
                Err(io::ErrorKind::InvalidInput.into())
            })
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        recorder.output(b"x");
        let _ = thread.finish();
        let recording = store.decoded();
        assert_eq!(recording.records.len(), 1);
        assert_eq!(recording.records[0].kind, RecordKind::Output(b"x".to_vec()));
    }

    #[test]
    fn output_offered_while_a_resize_is_applied_is_recorded_after_it() {
        let store = MemoryStore::default();
        let thread =
            RecorderThread::start(header(), store.clone(), RecorderLimits::default(), |_| {});
        let recorder = thread.recorder();
        let mut responder = None;
        recorder
            .resize_applied(Geometry::new(50, 132).unwrap(), ResizeCause::User, || {
                // The child sees the new size as soon as it is applied and
                // redraws; capture offers that output before this call returns.
                let producer = recorder.clone();
                responder = Some(std::thread::spawn(move || producer.output(b"redraw")));
                std::thread::sleep(Duration::from_millis(100));
                Ok(())
            })
            .unwrap();
        responder.unwrap().join().unwrap();
        let _ = thread.finish();

        let kinds: Vec<_> = store
            .decoded()
            .records
            .into_iter()
            .map(|r| r.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                RecordKind::Resize {
                    geometry: Geometry::new(50, 132).unwrap(),
                    cause: ResizeCause::User
                },
                RecordKind::Output(b"redraw".to_vec()),
            ]
        );
    }

    #[test]
    fn output_larger_than_one_record_is_split_without_loss() {
        let store = MemoryStore::default();
        let thread =
            RecorderThread::start(header(), store.clone(), RecorderLimits::default(), |_| {});
        let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        thread.recorder().output(&big);
        let _ = thread.finish();
        let recording = store.decoded();
        assert!(recording.records.len() > 1);
        assert_eq!(outputs(&recording), big);
    }

    #[test]
    fn segments_rotate_into_one_contiguous_recording() {
        let store = MemoryStore::default();
        let limits = RecorderLimits {
            segment_bytes: 256,
            ..RecorderLimits::default()
        };
        let thread = RecorderThread::start(header(), store.clone(), limits, |_| {
            panic!("no stop expected")
        });
        let recorder = thread.recorder();
        let mut expected = Vec::new();
        for i in 0..50u8 {
            let chunk = vec![i; 40];
            recorder.output(&chunk);
            expected.extend(chunk);
        }
        let summary = thread.finish();

        assert!(summary.segments > 1, "rotated");
        let recording = store.decoded();
        assert_eq!(recording.segment_count, summary.segments);
        assert_eq!(outputs(&recording), expected);
    }

    #[test]
    fn reaching_the_size_limit_stops_at_the_exact_recorded_prefix() {
        let store = MemoryStore::default();
        let stops = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&stops);
        let limits = RecorderLimits {
            max_bytes: 1024,
            ..RecorderLimits::default()
        };
        let thread = RecorderThread::start(header(), store.clone(), limits, move |stop| {
            seen.lock().unwrap().push(stop);
        });
        let recorder = thread.recorder();
        for _ in 0..100 {
            recorder.output(&[b'x'; 100]);
        }
        let summary = thread.finish();

        let stop = summary.stopped.expect("recording stopped");
        assert_eq!(stop.reason, StopReason::SizeLimit);
        assert_eq!(recorder.stopped(), Some(stop));
        assert_eq!(*stops.lock().unwrap(), vec![stop], "reported exactly once");
        let recording = store.decoded();
        assert_eq!(
            last_sequence(&recording),
            stop.last_recorded,
            "the report names the recording's real last record"
        );
        assert!(stop.last_recorded.is_some(), "some output fit");
        let total = store.total_bytes();
        assert!(total <= 1024, "never beyond the limit ({total} bytes)");
    }

    #[test]
    fn a_storage_failure_stops_recording_with_its_reason() {
        let store = MemoryStore {
            fail_after_bytes: Some(400),
            ..MemoryStore::default()
        };
        let thread =
            RecorderThread::start(header(), store.clone(), RecorderLimits::default(), |_| {});
        let recorder = thread.recorder();
        for _ in 0..20 {
            recorder.output(&[b'y'; 50]);
        }
        let summary = thread.finish();

        let stop = summary.stopped.expect("recording stopped");
        assert_eq!(
            stop.reason,
            StopReason::WriteFailed(io::ErrorKind::StorageFull)
        );
        let recording = store.decoded();
        assert_eq!(recording.end, SegmentEnd::Clean);
        assert_eq!(last_sequence(&recording), stop.last_recorded);
    }

    #[test]
    fn stalled_storage_never_blocks_producers_and_a_full_backlog_ends_the_prefix() {
        let gate = Gate::default();
        let store = MemoryStore {
            gate: Some(Arc::clone(&gate)),
            ..MemoryStore::default()
        };
        let limits = RecorderLimits {
            backlog_bytes: 4096,
            ..RecorderLimits::default()
        };
        let stops = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&stops);
        let thread = RecorderThread::start(header(), store.clone(), limits, move |stop| {
            seen.lock().unwrap().push(stop);
        });
        let recorder = thread.recorder();

        // Hold the first record's write in flight at the gate.
        recorder.output(b"in flight");
        let deadline = Instant::now() + Duration::from_secs(5);
        while store.at_gate.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "the first write never started");
            std::thread::sleep(Duration::from_millis(5));
        }

        let started = Instant::now();
        for _ in 0..1000 {
            recorder.output(&[b'z'; 512]);
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "offering output must not wait for storage"
        );
        let stop = recorder
            .stopped()
            .expect("stopped while storage is stalled");
        assert_eq!(stop.reason, StopReason::BacklogFull);
        assert_eq!(stop.last_recorded, None, "nothing had been appended");
        let deadline = Instant::now() + Duration::from_secs(5);
        while stops.lock().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            *stops.lock().unwrap(),
            vec![stop],
            "reported without waiting for storage"
        );

        // The write that was in flight completes after the stop.
        open(&gate);
        let summary = thread.finish();
        assert_eq!(summary.stopped, Some(stop));
        let recording = store.decoded();
        assert_eq!(
            last_sequence(&recording),
            None,
            "a late append beyond the declared boundary is cut off"
        );
    }

    #[test]
    fn storage_still_stalled_at_close_is_declared_stopped() {
        let gate = Gate::default();
        let store = MemoryStore {
            gate: Some(Arc::clone(&gate)),
            ..MemoryStore::default()
        };
        let limits = RecorderLimits {
            close_timeout: Duration::from_millis(200),
            ..RecorderLimits::default()
        };
        let thread = RecorderThread::start(header(), store, limits, |_| {});
        thread.recorder().output(b"never lands");

        let started = Instant::now();
        let summary = thread.finish();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "close is bounded"
        );
        assert_eq!(
            summary.stopped,
            Some(Stopped {
                last_recorded: None,
                reason: StopReason::Stalled
            })
        );
        open(&gate);
    }

    /// The stop report has run by the time `finish` returns, even for a stop
    /// that `finish` itself declares: nothing reports a stop later.
    #[test]
    fn every_stop_is_reported_before_finish_returns() {
        let gate = Gate::default();
        let store = MemoryStore {
            gate: Some(Arc::clone(&gate)),
            ..MemoryStore::default()
        };
        let limits = RecorderLimits {
            close_timeout: Duration::from_millis(100),
            ..RecorderLimits::default()
        };
        let stops = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&stops);
        let thread = RecorderThread::start(header(), store, limits, move |stop| {
            seen.lock().unwrap().push(stop);
        });
        thread.recorder().output(b"never lands");

        let summary = thread.finish();
        let stop = summary.stopped.expect("stalled at close");
        assert_eq!(*stops.lock().unwrap(), vec![stop]);
        open(&gate);
    }

    #[cfg(unix)]
    #[test]
    fn directory_store_publishes_private_ordered_segments() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("recording");
        let dir = parent.join("run");
        let limits = RecorderLimits {
            segment_bytes: 256,
            ..RecorderLimits::default()
        };
        let thread =
            RecorderThread::start(header(), DirectoryStore::new(dir.clone()), limits, |_| {});
        for i in 0..20u8 {
            thread.recorder().output(&[i; 60]);
        }
        let summary = thread.finish();
        assert_eq!(summary.stopped, None);

        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&parent), 0o700);
        assert_eq!(mode(&dir), 0o700);
        let files = DirectoryStore::segments(&dir).unwrap();
        assert_eq!(files.len() as u32, summary.segments);
        assert!(files.len() > 1);
        for file in &files {
            assert_eq!(mode(file), 0o600);
        }
        assert!(
            std::fs::read_dir(&dir).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")),
            "no unpublished temporary segments remain"
        );
        let bytes: Vec<Vec<u8>> = files.iter().map(|f| std::fs::read(f).unwrap()).collect();
        let recording = decode_recording(bytes.iter().map(Vec::as_slice)).unwrap();
        assert_eq!(recording.records.len(), 20);
    }

    #[cfg(unix)]
    #[test]
    fn an_unwritable_recording_directory_stops_before_the_first_record() {
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("recording");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let stops = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&stops);
        let thread = RecorderThread::start(
            header(),
            DirectoryStore::new(blocker.join("run")),
            RecorderLimits::default(),
            move |stop| seen.lock().unwrap().push(stop),
        );
        thread.recorder().output(b"lost");
        let summary = thread.finish();
        let stop = summary.stopped.expect("recording stopped");
        assert!(matches!(stop.reason, StopReason::WriteFailed(_)));
        assert_eq!(stop.last_recorded, None);
        assert_eq!(summary.segments, 0);
        assert_eq!(*stops.lock().unwrap(), vec![stop]);
    }
}
