//! The per-session sidecar — the lifecycle authority.
//!
//! Spawned detached by `start`, the sidecar holds the session lock, spawns the
//! supervised child via the [`platform`](crate::platform) backend, writes run
//! [`state`](crate::model::state) transitions into
//! [`Meta`], captures the child's output into the
//! append-only log, watches for kill requests, and classifies the exit. The
//! CLI normally only *asks*; after the sidecar is gone, the narrowly scoped
//! [`reconcile`](crate::reconcile) path may heal or infer terminal state.

use std::io;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;

mod supervised_run;
use supervised_run::SupervisedRun;

#[cfg(unix)]
use crate::attach_proto::RejectClass;
use crate::events::{self, EventDraft, EventWriter};
use crate::model::dep_fail::DepFailReason;
use crate::model::event::{Kind, Uuid7};
use crate::model::ids::{EpochTimestamp, Generation, Namespace, RunId, SessionName, Source};
use crate::model::meta::Meta;
#[cfg(unix)]
use crate::model::pty::PtyControl;
#[cfg(all(test, unix))]
use crate::model::pty::PtyMeta;
use crate::model::pty::{PtyRecording, RecordingState, RecordingStopReason};
use crate::model::spec::{DependencyBinding, IoMode, LaunchSpec};
use crate::model::state::{ExitReason, RunStatus, SidecarStep};
use crate::platform::{Current, Platform};
use crate::recorder::{Recorder, RecorderSummary, RecorderThread, StopReason};
use crate::session::{self, LockGuard, SessionDir, SessionRoot};

/// Type alias for the platform's ReadyWriter to avoid verbose turbofish.
type ReadyWriter = <Current as Platform>::ReadyWriter;

/// The human viewer PTY output is teed to, tagged with its controller identity
/// so a stale connection can never clear or replace its successor's viewer.
///
/// Output capture never writes to the viewer's socket. It offers each chunk to
/// the viewer's bounded [`ViewerQueue`], which the viewer's own sender thread
/// drains; a viewer that falls a full budget behind is disconnected instead of
/// stalling capture (and, through capture, the child).
struct AttachViewer {
    #[cfg(unix)]
    holder: crate::model::pty_control::HolderId,
    queue: Arc<ViewerQueue>,
    /// Shut the viewer's socket down (used when it overflows its queue).
    disconnect: Box<dyn Fn() + Send>,
}

/// Queued output bytes one viewer may fall behind before it is disconnected.
const VIEWER_QUEUE_BYTES: usize = 8 << 20;

/// A byte-bounded queue of output chunks for one viewer.
#[derive(Default)]
struct ViewerQueue {
    state: Mutex<ViewerQueueState>,
    ready: std::sync::Condvar,
}

#[derive(Default)]
struct ViewerQueueState {
    chunks: std::collections::VecDeque<Vec<u8>>,
    bytes: usize,
    closed: bool,
}

impl ViewerQueue {
    /// Queue `chunk` without blocking. `false` means the viewer is closed or has
    /// just overflowed its budget; either way it must be dropped.
    fn offer(&self, chunk: &[u8]) -> bool {
        let mut state = lock(&self.state);
        if state.closed {
            return false;
        }
        if state.bytes + chunk.len() > VIEWER_QUEUE_BYTES {
            state.closed = true;
            state.chunks.clear();
            state.bytes = 0;
            drop(state);
            self.ready.notify_all();
            return false;
        }
        state.bytes += chunk.len();
        state.chunks.push_back(chunk.to_vec());
        drop(state);
        self.ready.notify_all();
        true
    }

    /// Wait for the next chunk; `None` once closed.
    #[cfg(unix)]
    fn next(&self) -> Option<Vec<u8>> {
        let mut state = lock(&self.state);
        loop {
            if state.closed {
                return None;
            }
            if let Some(chunk) = state.chunks.pop_front() {
                state.bytes -= chunk.len();
                return Some(chunk);
            }
            state = self.ready.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    #[cfg(unix)]
    fn close(&self) {
        lock(&self.state).closed = true;
        self.ready.notify_all();
    }
}

/// Drain a viewer's queue into its connection until either closes.
#[cfg(unix)]
fn run_viewer_sender(queue: &ViewerQueue, conn: &Mutex<Box<dyn Write + Send>>) {
    use crate::attach_proto::{Frame, write_frame};
    while let Some(chunk) = queue.next() {
        if write_frame(&mut *lock(conn), &Frame::Data(chunk)).is_err() {
            queue.close();
            return;
        }
    }
}

/// Tees PTY output to the attached client, and tracks the terminal modes that
/// output has turned on, so that a client accepted later gets them first.
#[derive(Default)]
struct AttachTee {
    viewer: Option<AttachViewer>,
    modes: crate::term_modes::TermModes,
}

/// Shared sink for teeing PTY output to the attached client.
type AttachSink = Arc<Mutex<AttachTee>>;

/// Lock a mutex, recovering the data if another thread panicked while holding it.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// The sidecar's PTY input authority (Unix only; PTY sessions are unsupported
/// elsewhere, so the type is uninhabited there).
#[cfg(unix)]
struct PtyInput {
    writer: crate::pty_input::InputWriter,
}
#[cfg(not(unix))]
type PtyInput = std::convert::Infallible;

/// Removes a run's attach socket, and its breadcrumb while it still names that
/// socket, when dropped. The run drops it before releasing the session lock, and
/// on every early exit. The socket is this run's own: its name derives from the
/// run identity and binding refused any pre-existing path.
#[cfg(unix)]
struct AttachSocketCleanup {
    socket: PathBuf,
    session_dir: PathBuf,
}
#[cfg(not(unix))]
type AttachSocketCleanup = std::convert::Infallible;

#[cfg(unix)]
impl Drop for AttachSocketCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
        if crate::attach_proto::read_breadcrumb(&self.session_dir).as_ref() == Some(&self.socket) {
            let _ = std::fs::remove_file(self.session_dir.join("a.sock.path"));
        }
    }
}

/// A PTY run's private attach socket, bound and published before the child is
/// spawned (Unix only; the type is uninhabited elsewhere). The lifecycle guard
/// owns it after spawn: starting the attach side moves the socket to the
/// listener thread and keeps the cleanup, which removes the socket before the
/// guard releases the session lock.
#[cfg(unix)]
struct AttachEndpoint {
    socket: crate::attach_socket::BoundSocket,
    cleanup: AttachSocketCleanup,
}
#[cfg(not(unix))]
type AttachEndpoint = std::convert::Infallible;

/// Run the sidecar process. Called from the `_sidecar` subcommand.
///
/// Contract:
/// - Acquire session lock
/// - Read launch spec from session dir
/// - Spawn child process (write child_pid breadcrumb immediately)
/// - Write meta.json with Running state (or SpawnFailed)
/// - Send meta JSON snapshot over ready pipe (no race with disk state)
/// - Capture child stdout/stderr to output.log with timestamps
/// - Write terminal state when child exits
/// - Release lock and exit
pub fn run(session_dir: PathBuf, ready_writer: ReadyWriter) -> anyhow::Result<()> {
    remember_panics();

    // Wrap so we can track whether it's been consumed.
    // write_ready_signal takes ownership -- Option prevents double-use.
    // After spawn the lifecycle guard owns it, and answers the client itself.
    let mut ready = Some(ready_writer);

    let result = run_inner(&session_dir, &mut ready);

    if let Err(ref e) = result {
        // Only signal error if the file hasn't been consumed yet
        if let Some(file) = ready.take() {
            let _ = Current::write_ready_signal(file, &format!("ERROR:{e}\n"));
        }
    }

    result
}

/// The most recent panic message in this process, for the lifecycle guard's
/// record: a guard dropped by an unwinding panic cannot see the payload.
static LAST_PANIC: Mutex<Option<String>> = Mutex::new(None);

/// Keep each panic's message (then run the default hook; sidecar stderr is
/// /dev/null, so without this the reason would be lost).
fn remember_panics() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Ok(mut slot) = LAST_PANIC.lock() {
            *slot = Some(info.to_string());
        }
        default_hook(info);
    }));
}

fn last_panic_message() -> String {
    LAST_PANIC
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .unwrap_or_else(|| "unknown panic".to_owned())
}

/// Create the stdin transport and spawn a forwarding thread.
/// The transport is moved into the forwarding thread (it needs the server-side
/// handle on Windows). Cleanup is handled by `remove_stdin_transport`.
fn setup_stdin_forwarding(
    session_dir: &Path,
    child_stdin: Box<dyn Write + Send>,
    stdin_errors: &Arc<Mutex<Vec<String>>>,
) -> io::Result<()> {
    // StdinTransport is () on Unix — clippy flags the let-binding but
    // forward_stdin needs the value on Windows where the type is non-unit.
    #[allow(clippy::let_unit_value)]
    let transport = Current::create_stdin_transport(session_dir)?;

    // Spawn forwarding thread (detached -- not joined).
    // Thread owns the transport and exits when child stdin breaks or transport is removed.
    let session_dir_clone = session_dir.to_path_buf();
    let errors_clone = Arc::clone(stdin_errors);
    std::thread::spawn(move || {
        forward_stdin(transport, session_dir_clone, child_stdin, errors_clone)
    });

    Ok(())
}

/// The terminal type assumed when a PTY child would otherwise inherit none.
const DEFAULT_PTY_TERM: &str = "xterm-256color";

/// The `TERM` a PTY child runs under, given its launch `--env` overrides.
///
/// An explicit `--env TERM=…` wins. Otherwise the inherited `TERM` is kept
/// unless it is unset, empty or `dumb`: those come from callers that are not
/// terminals (an agent's tool shell, `ssh -T`), and would tell a child that
/// has a real terminal that it cannot draw.
fn pty_term(launch_env: &std::collections::BTreeMap<String, String>) -> String {
    if let Some(term) = launch_env.get("TERM") {
        return term.clone();
    }
    match std::env::var("TERM") {
        Ok(term) if !term.is_empty() && term != "dumb" => term,
        _ => DEFAULT_PTY_TERM.to_owned(),
    }
}

/// Spawn a timeout thread that kills the child after `timeout_s` seconds.
/// Returns the `timed_out` flag. The caller passes a `cancel` flag to prevent the kill
/// after the child exits naturally.
///
/// Takes a `ChildKillHandle` (lightweight, Send + Clone) extracted from the
/// SupervisedChild, so the timeout thread uses the live backend context
/// (Job Object on Windows, process group on Unix) rather than degrading
/// to orphan-kill semantics.
fn setup_timeout(
    kill_handle: <Current as Platform>::ChildKillHandle,
    timeout_s: u64,
    cancel: Arc<AtomicBool>,
) -> Arc<AtomicBool> {
    let timed_out = Arc::new(AtomicBool::new(false));
    let timed_out_clone = Arc::clone(&timed_out);
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_s);
        loop {
            if cancel.load(Ordering::Relaxed) {
                return; // Child exited before timeout -- don't kill
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if cancel.load(Ordering::Relaxed) {
            return; // Final check after deadline
        }
        timed_out_clone.store(true, Ordering::Relaxed);
        // Use kill_child with the live kill handle context.
        // On Windows this uses the Job Object for full tree kill;
        // on Unix this uses process group kill.
        let _ = Current::kill_child(&kill_handle, true);
    });
    timed_out
}

/// Spawn a thread that watches for a `kill_request` file from the CLI.
/// When found, validates the run_id matches (preventing stale requests from
/// killing a replacement run), then calls kill_child with the live
/// ChildKillHandle for tree-aware kill.
fn setup_kill_watcher(
    session_dir: &Path,
    kill_handle: <Current as Platform>::ChildKillHandle,
    run_id: RunId,
    cancel: Arc<AtomicBool>,
) {
    let kill_request_path = session_dir.join("kill_request");
    let kill_acted_path = session_dir.join("kill_acted");
    let run_id_str = run_id.to_string();
    std::thread::spawn(move || {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            if kill_request_path.exists() {
                // Parse the request. If unreadable or malformed, discard it
                // rather than defaulting to force (avoids partial-read upgrades).
                let parsed = std::fs::read_to_string(&kill_request_path)
                    .ok()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());

                // Always clean up the request file.
                let _ = std::fs::remove_file(&kill_request_path);

                let request = match parsed {
                    Some(v) => v,
                    None => continue, // Malformed — ignore, CLI will retry or fall back
                };

                // Validate run_id — reject stale requests from a previous run.
                if request["run_id"].as_str() != Some(&run_id_str) {
                    continue; // Wrong run — ignore
                }

                let force = request["force"].as_bool().unwrap_or(false);

                // Leave a breadcrumb so exit classification knows this was
                // a sidecar-mediated kill (not a spontaneous child exit).
                // The kill_forced marker handles force=true classification;
                // this breadcrumb handles force=false → Killed.
                if !force {
                    let _ = std::fs::write(&kill_acted_path, "");
                }

                // Use the live kill handle for tree-aware kill.
                let _ = Current::kill_child(&kill_handle, force);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    });
}

/// Collect stdin forwarding errors into a warning list.
fn collect_warnings(stdin_errors: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    lock(stdin_errors).clone()
}

/// Outcome of the dependency wait phase.
enum DepWaitOutcome {
    /// All dependencies satisfied — proceed to spawn.
    Satisfied,
    /// A dependency failed (non-zero exit, not found, replaced).
    Failed(String),
    /// Timeout expired during the wait.
    TimedOut(String),
    /// Graceful kill request received during the wait.
    Killed(String),
    /// Force kill request received during the wait.
    KilledForced(String),
}

/// A `--after` dependency after the first scan. Only `Pending` and `Latched`
/// coexist with continued polling; failure/timeout/kill are phase-terminal
/// ([`FirstScanOutcome::Terminal`]) and are never represented here.
enum ScannedDependency {
    Pending(DependencyBinding),
    /// Satisfied at its bound run_id and never re-examined — the binding is not
    /// retained because a latched dependency is never inspected again.
    Latched,
}

/// The dependency set produced by [`first_dependency_scan`]. Its private field
/// makes that scan the only producer, so accepting a `ScannedDependencies` (as
/// [`poll_dependencies`] does) is a static proof that a first scan has run.
struct ScannedDependencies(Vec<ScannedDependency>);

/// Exhaustive outcome of the first complete dependency scan.
enum FirstScanOutcome {
    /// Every dependency already satisfied — spawn immediately.
    ReadyToSpawn,
    /// Some pending, none failed — publish `Starting` and enter the poll loop.
    Waiting(ScannedDependencies),
    /// The scan produced, or was pre-empted by, a phase-terminal outcome.
    Terminal {
        reason: DepFailReason,
        warning: String,
    },
}

/// What `run_inner` does after the single post-scan readiness publication.
enum DepAction {
    Spawn,
    Wait(ScannedDependencies),
    ReturnTerminal,
}

/// Result of one dependency-scan iteration over the not-yet-latched set.
enum ScanStep {
    AllLatched,
    StillPending,
    Terminal(DepFailReason, String),
}

/// One dependency-scan iteration: honour a targeted kill request, then the
/// timeout, then inspect every not-yet-latched dependency — latching a
/// newly-satisfied one, or reporting the first phase-terminal condition. Shared
/// by the first scan and the poll loop so their evaluation is identical.
fn scan_iteration(
    scanned: &mut ScannedDependencies,
    session_root: &SessionRoot,
    namespace: &Namespace,
    kill_request_path: &Path,
    run_id_str: &str,
    deadline: Option<std::time::Instant>,
    after_any_exit: bool,
) -> ScanStep {
    // A kill request targeted at this run pre-empts the wait.
    if kill_request_path.exists() {
        if let Ok(content) = std::fs::read_to_string(kill_request_path) {
            if let Ok(req) = serde_json::from_str::<serde_json::Value>(&content) {
                if req["run_id"].as_str() == Some(run_id_str) {
                    let _ = std::fs::remove_file(kill_request_path);
                    let force = req["force"].as_bool().unwrap_or(false);
                    return if force {
                        ScanStep::Terminal(
                            DepFailReason::KilledForced,
                            "force-killed during dependency wait".into(),
                        )
                    } else {
                        ScanStep::Terminal(
                            DepFailReason::Killed,
                            "killed during dependency wait".into(),
                        )
                    };
                }
            }
        }
        // Wrong run_id or malformed — remove and ignore.
        let _ = std::fs::remove_file(kill_request_path);
    }

    if let Some(dl) = deadline {
        if std::time::Instant::now() >= dl {
            return ScanStep::Terminal(
                DepFailReason::TimedOut,
                "timeout expired during dependency wait".into(),
            );
        }
    }

    let mut all_latched = true;
    for slot in scanned.0.iter_mut() {
        let ScannedDependency::Pending(binding) = slot else {
            continue; // already latched — never re-polled
        };

        let dep_session = match session::open(session_root, namespace, &binding.session) {
            Ok(Some(s)) => s,
            Ok(None) => {
                return ScanStep::Terminal(
                    DepFailReason::Failed,
                    format!("dependency session not found: {}", binding.session),
                );
            }
            Err(e) => {
                return ScanStep::Terminal(
                    DepFailReason::Failed,
                    format!("failed to open dependency {}: {e}", binding.session),
                );
            }
        };
        let dep_meta = match session::read_meta(&dep_session) {
            Ok(m) => m,
            Err(e) => {
                return ScanStep::Terminal(
                    DepFailReason::Failed,
                    format!("failed to read dependency {}: {e}", binding.session),
                );
            }
        };

        // Bound to run_id: replacing a still-pending dependency invalidates it.
        if dep_meta.run_id() != binding.run_id {
            return ScanStep::Terminal(
                DepFailReason::Failed,
                format!(
                    "dependency {} was replaced (bound run_id {}, found {})",
                    binding.session,
                    binding.run_id,
                    dep_meta.run_id()
                ),
            );
        }

        if dep_meta.status().is_terminal() {
            if !after_any_exit {
                use crate::model::state::{ExitReason as ER, RunStatus};
                match dep_meta.status() {
                    RunStatus::Exited {
                        how: ER::ExitedOk, ..
                    } => {} // satisfied
                    _ => {
                        return ScanStep::Terminal(
                            DepFailReason::Failed,
                            format!(
                                "dependency {} exited with non-success state",
                                binding.session
                            ),
                        );
                    }
                }
            }
            *slot = ScannedDependency::Latched;
        } else {
            all_latched = false;
        }
    }

    if all_latched {
        ScanStep::AllLatched
    } else {
        ScanStep::StillPending
    }
}

/// The first complete dependency scan — run once, before readiness is signalled.
/// Every already-satisfied dependency is latched to its observed run_id.
fn first_dependency_scan(
    session_root: &SessionRoot,
    namespace: &Namespace,
    spec: &LaunchSpec,
    deadline: Option<std::time::Instant>,
    session_dir: &Path,
    run_id: &RunId,
) -> FirstScanOutcome {
    let kill_request_path = session_dir.join("kill_request");
    let run_id_str = run_id.to_string();
    let mut scanned = ScannedDependencies(
        spec.after
            .iter()
            .cloned()
            .map(ScannedDependency::Pending)
            .collect(),
    );
    match scan_iteration(
        &mut scanned,
        session_root,
        namespace,
        &kill_request_path,
        &run_id_str,
        deadline,
        spec.after_any_exit,
    ) {
        ScanStep::AllLatched => FirstScanOutcome::ReadyToSpawn,
        ScanStep::StillPending => FirstScanOutcome::Waiting(scanned),
        ScanStep::Terminal(reason, warning) => FirstScanOutcome::Terminal { reason, warning },
    }
}

/// Continue polling an already-scanned dependency set until it resolves. Taking
/// a `ScannedDependencies` (only [`first_dependency_scan`] can build one) makes
/// polling uninspected dependencies unrepresentable.
fn poll_dependencies(
    mut scanned: ScannedDependencies,
    session_root: &SessionRoot,
    namespace: &Namespace,
    deadline: Option<std::time::Instant>,
    session_dir: &Path,
    run_id: &RunId,
    after_any_exit: bool,
) -> DepWaitOutcome {
    let kill_request_path = session_dir.join("kill_request");
    let run_id_str = run_id.to_string();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        match scan_iteration(
            &mut scanned,
            session_root,
            namespace,
            &kill_request_path,
            &run_id_str,
            deadline,
            after_any_exit,
        ) {
            ScanStep::AllLatched => return DepWaitOutcome::Satisfied,
            ScanStep::StillPending => continue,
            ScanStep::Terminal(reason, msg) => {
                return match reason {
                    DepFailReason::Failed => DepWaitOutcome::Failed(msg),
                    DepFailReason::TimedOut => DepWaitOutcome::TimedOut(msg),
                    DepFailReason::Killed => DepWaitOutcome::Killed(msg),
                    DepFailReason::KilledForced => DepWaitOutcome::KilledForced(msg),
                };
            }
        }
    }
}

/// Prepare the single readiness publication. Leaves `meta` as `Starting` for
/// `ReadyToSpawn`/`Waiting`; for `Terminal`, transitions, emits, and persists
/// `DependencyFailed` before returning, so the snapshot signalled next is
/// truthful. Takes `session` because the terminal arm writes meta to disk.
fn apply_first_scan_outcome(
    session: &SessionDir,
    meta: &mut Meta,
    lifecycle: &mut LifecycleEvents,
    outcome: FirstScanOutcome,
) -> anyhow::Result<DepAction> {
    match outcome {
        FirstScanOutcome::ReadyToSpawn => Ok(DepAction::Spawn),
        FirstScanOutcome::Waiting(scanned) => Ok(DepAction::Wait(scanned)),
        FirstScanOutcome::Terminal { reason, warning } => {
            meta.add_warning(warning);
            meta.transition_dependency_failed(EpochTimestamp::now(), reason)?;
            lifecycle.emit(meta, true);
            session::write_meta_atomic(session, meta)?;
            Ok(DepAction::ReturnTerminal)
        }
    }
}

/// Appends the sidecar's lifecycle events to the session event log.
/// Writer identity is the run id; `seq` is contiguous across the run.
///
/// WAL order (spec §3.6): call `emit` after the meta transition but BEFORE
/// `write_meta_atomic`, with `durable: true` for terminal transitions. This
/// is an ORDERING guarantee against the crash window — a sidecar that dies
/// between the two writes leaves the event, never the meta. It is not an
/// IO-failure guarantee: if the append itself fails, supervision continues
/// (meta stays the current-state authority), the failure is recorded as a
/// meta warning, and the record is salvaged to lost+found.
struct LifecycleEvents {
    session_dir: PathBuf,
    writer: EventWriter,
    namespace: Namespace,
    session: SessionName,
    run_id: RunId,
    generation: Generation,
}

impl LifecycleEvents {
    fn new(
        session_dir: &Path,
        namespace: &Namespace,
        session: &SessionName,
        run_id: RunId,
        generation: Generation,
    ) -> Self {
        Self {
            session_dir: session_dir.to_path_buf(),
            writer: EventWriter::with_writer(session_dir, Uuid7::from(run_id)),
            namespace: namespace.clone(),
            session: session.clone(),
            run_id,
            generation,
        }
    }

    /// Same session and run, freshly minted writer identity — for a PTY run's
    /// effects thread, which appends concurrently with the lifecycle writer.
    /// The protocol is multi-writer by design: each writer keeps its own
    /// contiguous `seq` chain (spec §1).
    #[cfg(unix)]
    fn with_fresh_writer(&self) -> Self {
        Self {
            session_dir: self.session_dir.clone(),
            writer: EventWriter::new(&self.session_dir),
            namespace: self.namespace.clone(),
            session: self.session.clone(),
            run_id: self.run_id,
            generation: self.generation,
        }
    }

    /// The lifecycle event for meta's CURRENT status.
    fn lifecycle_draft(&self, meta: &Meta) -> EventDraft {
        EventDraft {
            id: None,
            kind: events::lifecycle_kind(meta.status()),
            namespace: self.namespace.clone(),
            session: self.session.clone(),
            run_id: self.run_id,
            generation: Some(self.generation.as_u64()),
            source: Source::trusted("tendr.sidecar").expect("tendr.sidecar is grammatical"),
            block_id: None,
            parent_id: None,
            data: Some(events::lifecycle_data(
                meta.status(),
                "direct",
                meta.launch_spec().boundary.as_ref(),
            )),
            preview: None,
        }
    }

    /// Append the lifecycle event for meta's CURRENT status. Never fails the
    /// run: an append failure becomes a meta warning and the record is
    /// salvaged to lost+found — supervision must not die, and the history
    /// record must not silently vanish, because the event log is unwritable.
    fn emit(&mut self, meta: &mut Meta, durable: bool) {
        let draft = self.lifecycle_draft(meta);
        if let Err(e) = self.writer.append(draft.clone(), durable) {
            meta.add_warning(format!("event log append failed: {e}"));
            self.salvage_to_lost_found(draft);
        }
    }

    /// Meta could not be written after a terminal transition: keep a copy of
    /// the terminal record, with the write error, outside the session dir.
    fn salvage_unrecorded(&self, meta: &Meta, record_error: &str) {
        let mut draft = self.lifecycle_draft(meta);
        if let Some(data) = draft.data.as_mut() {
            data["meta_write_error"] = serde_json::Value::String(record_error.to_owned());
            data["warnings"] = serde_json::json!(meta.warnings());
        }
        self.salvage_to_lost_found(draft);
    }

    /// Append a non-lifecycle sidecar fact (`callback.finished`, spec §1)
    /// through the same writer, so `seq` stays contiguous after the
    /// terminal transition. Best-effort by design: no
    /// salvage, no meta warning — these are not terminal transitions.
    fn append_fact(&mut self, fact: Fact, data: &impl serde::Serialize) {
        let (Ok(kind), Ok(data)) = (Kind::new(fact.kind()), serde_json::to_value(data)) else {
            return; // unreachable: every Fact kind is grammatical (tested)
        };
        let draft = EventDraft {
            id: None,
            kind,
            namespace: self.namespace.clone(),
            session: self.session.clone(),
            run_id: self.run_id,
            generation: Some(self.generation.as_u64()),
            source: Source::trusted("tendr.sidecar").expect("tendr.sidecar is grammatical"),
            block_id: None,
            parent_id: None,
            data: Some(data),
            preview: None,
        };
        let _ = self.writer.append(draft, false);
    }

    /// Last-resort preservation when the session's event log is unwritable:
    /// the fully-addressed record lands in `~/.tendr/lost+found/events.jsonl`
    /// (spec §7 machinery) instead of vanishing. Best-effort by design.
    fn salvage_to_lost_found(&self, draft: EventDraft) {
        let Some(tendr_root) = self
            .session_dir
            .ancestors()
            .find(|p| p.ends_with("sessions"))
            .and_then(Path::parent)
        else {
            return;
        };
        let event = events::stamp_orphan_event(draft);
        let _ = events::append_lost_found(tendr_root, &event);
    }
}

/// The non-lifecycle facts the sidecar appends. Only PTY runs (Unix) record
/// the last three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(unix), allow(dead_code))]
enum Fact {
    CallbackFinished,
    RecordingStopped,
    ControlChanged,
    InputRevoked,
}

impl Fact {
    /// The event kind.
    fn kind(self) -> &'static str {
        match self {
            Self::CallbackFinished => "callback.finished",
            Self::RecordingStopped => "recording.stopped",
            Self::ControlChanged => "pty.control_changed",
            Self::InputRevoked => "pty.input_revoked",
        }
    }
}

/// Test-only crash injection: `TENDR_TEST_ABORT=<point>` aborts the sidecar
/// there, a true crash that skips the lifecycle guard (WAL ordering, orphan
/// recovery). Points: `after_spawn`, `after_running`, `before_terminal_event`,
/// `before_terminal_meta`. Compiled into debug builds only; release sidecars
/// ignore the variable entirely.
fn test_abort_point(point: &str) {
    if cfg!(debug_assertions) && std::env::var("TENDR_TEST_ABORT").as_deref() == Ok(point) {
        std::process::abort();
    }
}

/// Test-only fault injection at a named sidecar step. `TENDR_TEST_FAIL` is a
/// comma-separated list of points that return an injected error;
/// `TENDR_TEST_PANIC` names points that panic. The points are the
/// [`SidecarStep`] wire names plus `ready_rewrite`, `terminal_meta`,
/// `failure_record` and `output_log_write` (every write to `output.log`). If
/// `TENDR_TEST_FAULT_GATE` names a file, a named point
/// first waits for it (bounded), so a test can let the child reach a known
/// state before the fault fires. Compiled into debug builds only; release
/// sidecars ignore all three variables entirely.
fn test_fault(point: &str) -> io::Result<()> {
    if !cfg!(debug_assertions) {
        return Ok(());
    }
    let names = |var: &str| {
        std::env::var(var)
            .map(|v| v.split(',').any(|p| p.trim() == point))
            .unwrap_or(false)
    };
    let panics = names("TENDR_TEST_PANIC");
    let fails = names("TENDR_TEST_FAIL");
    if panics || fails {
        wait_for_gate_file("TENDR_TEST_FAULT_GATE");
    }
    if panics {
        panic!("injected panic at {point}");
    }
    if fails {
        return Err(io::Error::other(format!("injected fault at {point}")));
    }
    Ok(())
}

/// Wait until the file named by env var `var` exists, if it is set. Bounded,
/// so a failed test cannot strand the sidecar.
fn wait_for_gate_file(var: &str) {
    let Some(gate) = std::env::var_os(var) else {
        return;
    };
    let gate = PathBuf::from(gate);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !gate.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Test-only gate before the readiness write, so a test can kill the `start`
/// client inside the spawn-to-readiness window without a timing race: the
/// sidecar waits until the file named by `TENDR_TEST_READY_GATE` exists
/// (bounded, so a failed test cannot strand it). Compiled into debug builds
/// only; release sidecars ignore the variable entirely.
fn test_ready_gate() {
    if cfg!(debug_assertions) {
        wait_for_gate_file("TENDR_TEST_READY_GATE");
    }
}

/// Test-only stall of a PTY session's output reader: before its first read it
/// sleeps for `TENDR_TEST_PTY_READ_DELAY_MS` milliseconds, as a reader starved
/// of CPU would, so a test can let a short-lived child write and exit before
/// anything reads the PTY. Compiled into debug builds only; release sidecars
/// ignore the variable entirely.
fn test_pty_read_delay() {
    if !cfg!(debug_assertions) {
        return;
    }
    if let Some(ms) = std::env::var("TENDR_TEST_PTY_READ_DELAY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

/// Forward pushed stdin for a PTY session through the single input writer.
#[cfg(unix)]
fn setup_pty_stdin_forwarding(
    session_dir: &Path,
    input: &PtyInput,
    errors: &Arc<Mutex<Vec<String>>>,
) -> io::Result<()> {
    #[allow(clippy::let_unit_value)]
    let transport = Current::create_stdin_transport(session_dir)?;
    let session_dir = session_dir.to_path_buf();
    let writer = input.writer.clone();
    let errors = Arc::clone(errors);
    std::thread::spawn(move || forward_pty_stdin(transport, &session_dir, &writer, &errors));
    Ok(())
}

/// Test-only gate between the terminal meta write and the lock release, so a
/// test can observe a session that is terminal but still locked (#91). Waits
/// for the file named by `TENDR_TEST_UNLOCK_GATE` (bounded). Debug builds only.
fn test_unlock_gate() {
    if cfg!(debug_assertions) {
        wait_for_gate_file("TENDR_TEST_UNLOCK_GATE");
    }
}

#[cfg(not(unix))]
fn setup_pty_stdin_forwarding(
    _session_dir: &Path,
    input: &PtyInput,
    _errors: &Arc<Mutex<Vec<String>>>,
) -> io::Result<()> {
    match *input {}
}

/// Each push connection claims the PTY as an agent for its duration and writes
/// through the input writer, one acknowledged chunk at a time.
///
/// A push that finds another agent holding the input waits its turn
/// ([`claim_for_fifo`]). One that cannot claim (a human holds control) or whose
/// claim is revoked by a takeover has its remaining bytes drained and discarded,
/// never written to the PTY. The FIFO is drained rather than closed because reopening it would
/// hand the still-connected push writer to the next accept. The legacy push CLI
/// therefore cannot observe the rejection; revocation is recorded as a
/// `pty.input_revoked` event and the rejection as a run warning.
///
/// Unlike an attach-socket push, a FIFO writer's hang-up is not watched for:
/// `exec` closes the FIFO as soon as its frame is in the pipe, so hang-up is
/// how a complete frame ends. A frame waiting on a full PTY keeps the claim
/// until the child reads it, or a takeover revokes it (PR #68 review,
/// finding 6).
#[cfg(unix)]
fn forward_pty_stdin(
    transport: <Current as Platform>::StdinTransport,
    session_dir: &Path,
    writer: &crate::pty_input::InputWriter,
    errors: &Mutex<Vec<String>>,
) {
    use crate::model::pty_control::{IncompleteReason, InputOutcome};

    let mut buf = [0u8; 8192];
    loop {
        let Some(mut reader) = Current::accept_stdin_connection(&transport, session_dir) else {
            return;
        };
        let handle = claim_for_fifo(writer, errors);
        // Cleared once a takeover revokes the claim.
        let mut authorized = handle.is_some();
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    lock(errors).push(format!("stdin read failed: {e}"));
                    // Give the claim up with the frame: nothing more of it
                    // can arrive, and a held claim would refuse every agent.
                    if let Some(handle) = handle {
                        writer.disconnect(handle);
                    }
                    return;
                }
            };
            let Some(current) = handle.as_ref().filter(|_| authorized) else {
                continue; // rejected or revoked: drain and discard
            };
            match writer.write(current, buf[..n].to_vec()) {
                Some(InputOutcome::Accepted { .. }) => {}
                Some(InputOutcome::Incomplete {
                    reason: IncompleteReason::NotAuthorized(_),
                    ..
                }) => authorized = false,
                Some(InputOutcome::Incomplete { .. }) | None => {
                    lock(errors).push("stdin forwarding: child stdin closed".to_owned());
                    if let Some(handle) = handle {
                        writer.disconnect(handle);
                    }
                    return;
                }
            }
        }
        if let Some(handle) = handle {
            // Release only after this push's input is done, so the next push can
            // claim; a revoked handle releases nothing.
            writer.end_of_input_and_wait(handle);
        }
    }
}

/// Claim the PTY input for one FIFO writer (an `exec` frame).
///
/// Another agent's push is waited out: agents take the input one at a time,
/// so the frame is queued behind it rather than dropped (PR #68 review,
/// finding 2). A human at the terminal is never typed into, so a human holder
/// refuses the frame, which is then discarded and reported as a run warning.
#[cfg(unix)]
fn claim_for_fifo(
    writer: &crate::pty_input::InputWriter,
    errors: &Mutex<Vec<String>>,
) -> Option<crate::model::pty_control::ControllerHandle> {
    use crate::model::pty_control::{ControlError, ControllerKind};
    use crate::pty_input::RequestError;

    let holder = writer.next_holder();
    loop {
        match writer.claim(holder, ControllerKind::Agent) {
            Ok(handle) => return Some(handle),
            Err(RequestError::Control(ControlError::Busy {
                kind: ControllerKind::Agent,
                ..
            })) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(e) => {
                lock(errors).push(format!("push rejected: {e}"));
                return None;
            }
        }
    }
}

fn run_inner(session_dir: &Path, ready: &mut Option<ReadyWriter>) -> anyhow::Result<()> {
    // --- Setup: lock, read spec, create meta ---
    let sidecar_identity = Current::self_identity()?;

    let session_name_str = session_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid session directory"))?;
    let session_name = SessionName::new(session_name_str)?;

    // Path structure: root/<namespace>/<session>/
    let ns_dir = session_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("session dir has no parent (namespace)"))?;
    let namespace_str = ns_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid namespace directory"))?;
    let namespace = Namespace::new(namespace_str)?;

    let root = ns_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("namespace dir has no parent (root)"))?;
    let session_root = SessionRoot::new(root.to_path_buf());
    let session = session::open_raw(&session_root, &namespace, &session_name)?;

    let lock = LockGuard::try_acquire(&session)?;

    // Read launch spec
    let spec_path = session_dir.join("launch_spec.json");
    let spec_json =
        std::fs::read_to_string(&spec_path).context("failed to read launch_spec.json")?;
    let launch_spec: LaunchSpec =
        serde_json::from_str(&spec_json).context("invalid launch_spec.json")?;
    let _ = std::fs::remove_file(&spec_path);

    let run_id = RunId::new();
    let generation = {
        let gen_path = session_dir.join("generation");
        if let Ok(content) = std::fs::read_to_string(&gen_path) {
            let _ = std::fs::remove_file(&gen_path); // consumed
            content
                .trim()
                .parse::<u64>()
                .ok()
                .map(Generation::from_u64)
                .unwrap_or_else(Generation::first)
        } else {
            Generation::first()
        }
    };

    let mut meta = Meta::new_starting(
        session_name.clone(),
        run_id,
        generation,
        launch_spec,
        sidecar_identity,
        EpochTimestamp::now(),
    );

    // The event log records history from the very first state, even though
    // meta only hits disk at Starting for dependency waits (WAL: event first).
    let mut lifecycle =
        LifecycleEvents::new(session_dir, &namespace, &session_name, run_id, generation);
    lifecycle.emit(&mut meta, false);

    // Re-set CLOEXEC on the ready fd before spawning the child.
    // The sidecar inherited this fd with CLOEXEC cleared (so it survived the sidecar's exec),
    // but the child must NOT hold the pipe open -- otherwise the CLI's read_to_string blocks
    // until the child exits, defeating the readiness handshake.
    if let Some(writer) = ready.take() {
        let sealed = Current::seal_ready_fd(writer)
            .map_err(|e| anyhow::anyhow!("failed to seal ready fd: {e}"))?;
        *ready = Some(sealed);
    }

    // Build effective env: user-supplied first, then TENDR_* overlay (authoritative).
    // The spawn removes `unset_env` names from the inherited env before applying
    // this, so both still win over an unset.
    let mut effective_env = meta.launch_spec().env.clone();
    effective_env.insert(
        "TENDR_SESSION".to_owned(),
        meta.session().as_str().to_owned(),
    );
    effective_env.insert("TENDR_NAMESPACE".to_owned(), namespace.as_str().to_owned());
    effective_env.insert("TENDR_RUN_ID".to_owned(), run_id.to_string());
    effective_env.insert("TENDR_GENERATION".to_owned(), generation.to_string());
    effective_env.insert(
        "TENDR_SESSION_DIR".to_owned(),
        session_dir.to_str().unwrap_or("").to_owned(),
    );
    // A PTY is a real terminal: don't hand its child a `TERM` that says it
    // cannot draw.
    if meta.launch_spec().io_mode == IoMode::Pty {
        effective_env.insert("TERM".to_owned(), pty_term(&meta.launch_spec().env));
    }

    // --- Wait for --after dependencies ---
    let has_deps = !meta.launch_spec().after.is_empty();
    if has_deps {
        // Persist Starting so the session is observable on disk during the scan.
        session::write_meta_atomic(&session, &meta)?;

        // One deadline for the whole wait, fixed before the first scan so that
        // splitting scan-from-poll cannot silently extend the timeout.
        let deadline = meta
            .launch_spec()
            .timeout_s
            .map(|t| std::time::Instant::now() + std::time::Duration::from_secs(t));
        let after_any_exit = meta.launch_spec().after_any_exit;

        // Readiness fires only AFTER the first complete scan, through one funnel.
        // So `start` returning proves every dependency was inspected once (and
        // already-satisfied ones latched), or a terminal condition was persisted
        // and signalled first.
        let outcome = first_dependency_scan(
            &session_root,
            &namespace,
            meta.launch_spec(),
            deadline,
            session_dir,
            &run_id,
        );
        let action = apply_first_scan_outcome(&session, &mut meta, &mut lifecycle, outcome)?;
        signal_readiness(&session, ready, &mut meta);

        match action {
            DepAction::Spawn => {} // every dependency already satisfied — proceed
            DepAction::ReturnTerminal => return Ok(()),
            DepAction::Wait(scanned) => match poll_dependencies(
                scanned,
                &session_root,
                &namespace,
                deadline,
                session_dir,
                &run_id,
                after_any_exit,
            ) {
                DepWaitOutcome::Satisfied => {} // proceed to spawn
                DepWaitOutcome::Failed(msg) => {
                    meta.add_warning(msg);
                    meta.transition_dependency_failed(
                        EpochTimestamp::now(),
                        DepFailReason::Failed,
                    )?;
                    lifecycle.emit(&mut meta, true);
                    session::write_meta_atomic(&session, &meta)?;
                    return Ok(());
                }
                DepWaitOutcome::TimedOut(msg) => {
                    meta.add_warning(msg);
                    meta.transition_dependency_failed(
                        EpochTimestamp::now(),
                        DepFailReason::TimedOut,
                    )?;
                    lifecycle.emit(&mut meta, true);
                    session::write_meta_atomic(&session, &meta)?;
                    return Ok(());
                }
                DepWaitOutcome::Killed(msg) => {
                    meta.add_warning(msg);
                    meta.transition_dependency_failed(
                        EpochTimestamp::now(),
                        DepFailReason::Killed,
                    )?;
                    lifecycle.emit(&mut meta, true);
                    session::write_meta_atomic(&session, &meta)?;
                    return Ok(());
                }
                DepWaitOutcome::KilledForced(msg) => {
                    meta.add_warning(msg);
                    meta.transition_dependency_failed(
                        EpochTimestamp::now(),
                        DepFailReason::KilledForced,
                    )?;
                    lifecycle.emit(&mut meta, true);
                    session::write_meta_atomic(&session, &meta)?;
                    return Ok(());
                }
            },
        }
    }

    // --- Spawn child (with SpawnFailed handling inline) ---
    // A PTY session's private attach socket is bound and published before the
    // child exists, so a PTY session never runs without its listener. A bind
    // failure is a spawn failure: there is no child yet. Dropping the endpoint
    // removes the socket and its breadcrumb: here if the spawn fails, otherwise
    // in the lifecycle guard before it releases the session lock.
    #[cfg(unix)]
    let attach = if meta.launch_spec().io_mode == IoMode::Pty {
        match crate::attach_socket::bind_for_session(session_dir, run_id) {
            Ok(bound) => Some(AttachEndpoint {
                cleanup: AttachSocketCleanup {
                    socket: bound.path.clone(),
                    session_dir: session_dir.to_path_buf(),
                },
                socket: bound,
            }),
            Err(e) => {
                meta.add_warning(format!("attach socket unavailable: {e}"));
                meta.transition_spawn_failed(EpochTimestamp::now())?;
                lifecycle.emit(&mut meta, true);
                session::write_meta_atomic(&session, &meta)?;
                signal_readiness(&session, ready, &mut meta);
                return Ok(());
            }
        }
    } else {
        None
    };
    #[cfg(not(unix))]
    let attach: Option<AttachEndpoint> = None;

    let spawned = if meta.launch_spec().io_mode == IoMode::Pty {
        Current::spawn_child_pty(
            meta.launch_spec().argv(),
            meta.launch_spec().cwd.as_deref(),
            &meta.launch_spec().unset_env,
            &effective_env,
        )
    } else {
        Current::spawn_child(
            meta.launch_spec().argv(),
            meta.launch_spec().stdin_mode == crate::model::spec::StdinMode::Pipe,
            meta.launch_spec().cwd.as_deref(),
            &meta.launch_spec().unset_env,
            &effective_env,
        )
    };
    let child = match spawned {
        Ok(child) => child,
        Err(e) => {
            meta.add_warning(format!("spawn failed: {e}"));
            meta.transition_spawn_failed(EpochTimestamp::now())?;
            lifecycle.emit(&mut meta, true);
            session::write_meta_atomic(&session, &meta)?;
            signal_readiness(&session, ready, &mut meta);
            return Ok(());
        }
    };

    // From here the guard owns the child: no exit below can leave it
    // unsupervised (see supervised_run.rs).
    let run = SupervisedRun::adopt(child, session, lock, meta, lifecycle, ready.take(), attach);
    let mut run = run.publish_running()?;
    let how = run.supervise()?;
    run.finish(how)
}

/// Run the `--on-exit` hooks for a terminal run, unlocked and separate from
/// the run lifecycle. Each outcome is a best-effort `callback.finished` fact,
/// and the batch is written under `callbacks/<run_id>.json` outside the
/// session dir, so it survives `--replace`.
fn run_on_exit_hooks(meta: &Meta, session_dir: &Path, lifecycle: &mut LifecycleEvents) {
    let on_exit_callbacks = meta.launch_spec().on_exit.clone();
    if on_exit_callbacks.is_empty() {
        return;
    }
    let RunStatus::Exited { how, .. } = meta.status() else {
        return;
    };
    let exit_reason = exit_reason_env(how);
    let run_id = meta.run_id().to_string();
    let session_name = meta.session().as_str().to_string();
    let namespace = meta
        .launch_spec()
        .namespace
        .as_deref()
        .unwrap_or("default")
        .to_string();
    let generation = meta.generation().to_string();
    let session_dir_str = session_dir.to_str().unwrap_or("").to_string();

    let mut callback_results: Vec<serde_json::Value> = Vec::new();

    for (i, callback_cmd) in on_exit_callbacks.iter().enumerate() {
        let argv = shell_words::split(callback_cmd).unwrap_or_else(|_| vec![callback_cmd.clone()]);
        if argv.is_empty() {
            continue;
        }
        let result = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .env("TENDR_SESSION", &session_name)
            .env("TENDR_NAMESPACE", &namespace)
            .env("TENDR_RUN_ID", &run_id)
            .env("TENDR_GENERATION", &generation)
            .env("TENDR_EXIT_REASON", &exit_reason)
            .env("TENDR_SESSION_DIR", &session_dir_str)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output();

        let record = match result {
            Ok(output) if output.status.success() => {
                serde_json::json!({"index": i, "command": callback_cmd, "status": "ok"})
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                serde_json::json!({
                    "index": i,
                    "command": callback_cmd,
                    "status": "failed",
                    "exit_code": output.status.code(),
                    "stderr": stderr.trim()
                })
            }
            Err(e) => {
                serde_json::json!({
                    "index": i,
                    "command": callback_cmd,
                    "status": "spawn_failed",
                    "error": e.to_string()
                })
            }
        };
        // The durable per-callback fact: emitted as each
        // callback finishes, same record shape as the batch file.
        lifecycle.append_fact(Fact::CallbackFinished, &record);
        callback_results.push(record);
    }

    // Write callback results keyed by run_id, outside the session dir
    // This survives --replace (which removes the session dir)
    let callbacks_dir = session_dir
        .ancestors()
        .find(|p| p.ends_with("sessions"))
        .and_then(|p| p.parent())
        .map(|tendr_root| tendr_root.join("callbacks"));

    if let Some(dir) = callbacks_dir {
        let _ = std::fs::create_dir_all(&dir);
        let record = serde_json::json!({
            "run_id": run_id,
            "session": session_name,
            "namespace": namespace,
            "callbacks": callback_results
        });
        let _ = std::fs::write(dir.join(format!("{run_id}.json")), record.to_string());
    }
}

/// `TENDR_EXIT_REASON` for `--on-exit` hooks. `SidecarFailed` is the bare
/// reason name, so a hook can match it exactly; the failing step is in meta.
/// Every other reason keeps the Debug form hooks have always received.
fn exit_reason_env(how: &ExitReason) -> String {
    match how {
        ExitReason::SidecarFailed { .. } => "SidecarFailed".to_owned(),
        other => format!("{other:?}"),
    }
}

/// Forward data from the stdin transport to the child's stdin pipe.
/// Accepts connections in a loop to support multiple pushes.
/// Exits when: child stdin write fails (child exited) or transport is removed.
fn forward_stdin(
    transport: <Current as Platform>::StdinTransport,
    session_dir: PathBuf,
    mut child_stdin: Box<dyn Write + Send>,
    errors: Arc<Mutex<Vec<String>>>,
) {
    use std::io::Read;
    let mut buf = [0u8; 8192];
    loop {
        // Block until a writer connects (returns None if transport removed)
        let mut reader = match Current::accept_stdin_connection(&transport, &session_dir) {
            Some(r) => r,
            None => return, // transport closed/removed
        };
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break, // writer disconnected
                Ok(n) => n,
                Err(e) => {
                    if let Ok(mut errs) = errors.lock() {
                        errs.push(format!("stdin read failed: {e}"));
                    }
                    return;
                }
            };
            if child_stdin.write_all(&buf[..n]).is_err() {
                if let Ok(mut errs) = errors.lock() {
                    errs.push("stdin forwarding: child stdin closed".to_owned());
                }
                return;
            }
        }
    }
}

/// Outcome of offering the meta snapshot to the `start` client. Deliberately
/// not a `Result`: readiness is a courtesy to the client that asked, never a
/// condition of the run (#71), so it must not be `?`-able.
#[must_use = "an undelivered readiness must be recorded as a session warning"]
enum ReadyDelivery {
    Delivered,
    /// The client has gone (killed mid-handshake, its pane closed).
    ClientGone(io::Error),
    /// Readiness went out earlier (the `--after` path signals after its
    /// first dependency scan).
    AlreadySent,
}

impl ReadyDelivery {
    /// Record a lost delivery in meta. Returns whether meta changed.
    fn record(self, meta: &mut Meta) -> bool {
        match self {
            Self::Delivered | Self::AlreadySent => false,
            Self::ClientGone(e) => {
                meta.add_warning(format!("readiness not delivered: start client gone ({e})"));
                true
            }
        }
    }
}

/// Send the meta snapshot over the readiness channel, consuming the writer.
/// The CLI reads this snapshot directly -- no race with subsequent disk writes.
fn deliver_ready(ready: &mut Option<ReadyWriter>, meta: &Meta) -> ReadyDelivery {
    let Some(writer) = ready.take() else {
        return ReadyDelivery::AlreadySent;
    };
    let message = match serde_json::to_string(meta) {
        Ok(json) => format!("OK:{json}\n"),
        Err(e) => format!("ERROR:meta snapshot not serializable: {e}\n"),
    };
    test_ready_gate();
    if let Err(e) = test_fault(SidecarStep::Readiness.as_str()) {
        drop(writer);
        return ReadyDelivery::ClientGone(e);
    }
    match Current::write_ready_signal(writer, &message) {
        Ok(()) => ReadyDelivery::Delivered,
        Err(e) => ReadyDelivery::ClientGone(e),
    }
}

/// Deliver readiness if it is still owed, and persist a lost delivery as a
/// session warning. Infallible by design: the sidecar then carries on exactly
/// as it would have (supervising, waiting on dependencies, or finishing a
/// terminal path). Meta was already written before every call site, so the
/// rewrite only adds the warning; if the rewrite fails too, the warning stays
/// in memory and the next meta write carries it.
fn signal_readiness(session: &SessionDir, ready: &mut Option<ReadyWriter>, meta: &mut Meta) {
    if deliver_ready(ready, meta).record(meta) {
        let _ = test_fault("ready_rewrite")
            .map_err(|e| e.to_string())
            .and_then(|()| session::write_meta_atomic(session, meta).map_err(|e| e.to_string()));
    }
}

/// Write one line of a stream to the shared log file.
fn write_log_line(log: &Mutex<Box<dyn Write + Send>>, tag: char, line: String) -> io::Result<()> {
    let formatted = serde_json::to_string(&crate::log::LogLine {
        ts: crate::log::timestamp_secs(),
        tag: tag.to_string(),
        content: serde_json::Value::String(line),
    })
    .expect("JSON serialization cannot fail")
        + "\n";
    test_fault("output_log_write").and_then(|()| lock(log).write_all(formatted.as_bytes()))
}

/// The warning for a stream whose `output.log` write failed.
fn log_write_warning(tag: char, e: &io::Error) -> String {
    let stream = if tag == 'E' { "stderr" } else { "stdout" };
    format!("{stream} capture: output.log write failed; later output not logged: {e}")
}

/// Read lines from a stream and write them to the shared log file. Invalid
/// UTF-8 is logged lossily. After the first failed write, send its warning to
/// `failed`, then drain the rest unparsed so the child never blocks.
fn capture_stream(
    stream: Box<dyn std::io::Read + Send>,
    tag: char,
    log: &Mutex<Box<dyn Write + Send>>,
    failed: &std::sync::mpsc::Sender<String>,
) {
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return, // pipe closed
            Ok(_) => {}
        }
        if line.last() == Some(&b'\n') {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
        }
        let text = String::from_utf8_lossy(&line).into_owned();
        if let Err(e) = write_log_line(log, tag, text) {
            let _ = failed.send(log_write_warning(tag, &e));
            let _ = io::copy(&mut reader, &mut io::sink());
            return;
        }
    }
}

/// Read raw bytes from a stream, record them, write them to the log, and tee
/// them to the attach sink. Used for PTY sessions where a human may be attached.
fn capture_stream_with_tee(
    mut stream: Box<dyn std::io::Read + Send>,
    tag: char,
    log: &Mutex<Box<dyn Write + Send>>,
    failed: &std::sync::mpsc::Sender<String>,
    attach_sink: &AttachSink,
    recorder: Option<&Recorder>,
) {
    let mut buf = [0u8; 4096];
    let mut logging = true;
    loop {
        let n = match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };

        // Record first: sequencing never waits for storage.
        if let Some(recorder) = recorder {
            recorder.output(&buf[..n]);
        }

        // Write to log (best-effort chunk-based transcript). After the first
        // failed write, skip the log but keep recording and teeing.
        if logging {
            let text = String::from_utf8_lossy(&buf[..n]);
            for line in text.lines() {
                if let Err(e) = write_log_line(log, tag, line.to_owned()) {
                    let _ = failed.send(log_write_warning(tag, &e));
                    logging = false;
                    break;
                }
            }
        }

        // Offer raw bytes to the attached viewer's bounded queue; never block.
        let mut tee = lock(attach_sink);
        tee.modes.feed(&buf[..n]);
        if let Some(viewer) = tee.viewer.as_ref() {
            if !viewer.queue.offer(&buf[..n]) {
                (viewer.disconnect)();
                tee.viewer = None; // closed, or fell a full budget behind
            }
        }
    }
}

/// The PTY input the single writer drives: the master's write half, a dup used
/// for window-size changes, and the recorder that records applied sizes.
#[cfg(unix)]
struct UnixPtyInput {
    writer: crate::platform::unix::PtyWriter,
    resize: Option<std::fs::File>,
    recorder: Option<Recorder>,
}

#[cfg(unix)]
impl crate::model::pty_control::PtyInputSink for UnixPtyInput {
    fn try_write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.writer.try_write(bytes)
    }
}

#[cfg(unix)]
impl crate::pty_input::WritablePty for UnixPtyInput {
    fn wait_writable(&self, timeout: std::time::Duration) -> io::Result<bool> {
        self.writer.wait_writable(timeout)
    }

    fn resize(&self, size: crate::recording::Geometry) {
        let Some(fd) = &self.resize else {
            return;
        };
        let apply = || apply_pty_resize(fd, size);
        let _ = match &self.recorder {
            Some(recorder) => {
                recorder.resize_applied(size, crate::recording::ResizeCause::User, apply)
            }
            None => apply(),
        };
    }
}

/// A live attach-socket connection: a descriptor used only to shut the socket
/// down, and how to retire it when a takeover supersedes it.
#[cfg(unix)]
struct Registered {
    control: std::os::unix::net::UnixStream,
    retirement: Retirement,
}

/// How a superseded connection is retired.
#[cfg(unix)]
enum Retirement {
    /// An attach client: told with `MSG_RETIRED` through its framed writer
    /// (shared with its output sender), then shut down.
    Viewer {
        conn: Arc<Mutex<Box<dyn Write + Send>>>,
    },
    /// A push: flagged revoked and its read side shut down, which wakes a handler
    /// waiting for the next frame; the handler then reports the outcome itself.
    Push { revoked: Arc<AtomicBool> },
}

/// Live attach connections by controller identity, so a takeover can retire
/// exactly the superseded connection.
#[cfg(unix)]
#[derive(Clone, Default)]
struct ConnectionRegistry(Arc<Mutex<RegistryState>>);

#[cfg(unix)]
#[derive(Default)]
struct RegistryState {
    live: std::collections::HashMap<crate::model::pty_control::HolderId, Registered>,
    /// Set when the run ends; nothing registers after that.
    closed: bool,
}

#[cfg(unix)]
impl ConnectionRegistry {
    /// Register a connection. Returns false, dropping `entry`, once the run has
    /// ended: the caller must reject the connection.
    #[must_use]
    fn insert(&self, holder: crate::model::pty_control::HolderId, entry: Registered) -> bool {
        let mut state = lock(&self.0);
        if state.closed {
            return false;
        }
        state.live.insert(holder, entry);
        true
    }

    fn remove(&self, holder: crate::model::pty_control::HolderId) -> Option<Registered> {
        lock(&self.0).live.remove(&holder)
    }

    fn contains(&self, holder: crate::model::pty_control::HolderId) -> bool {
        lock(&self.0).live.contains_key(&holder)
    }

    /// Refuse further registrations and shut every live connection down, so
    /// each client sees end of stream and each handler thread exits. The same
    /// lock as `insert`, so no connection can slip in after the sweep.
    fn close(&self) {
        let entries: Vec<Registered> = {
            let mut state = lock(&self.0);
            state.closed = true;
            state.live.drain().map(|(_, entry)| entry).collect()
        };
        for entry in entries {
            if let Retirement::Push { revoked } = &entry.retirement {
                revoked.store(true, Ordering::SeqCst);
            }
            let _ = entry.control.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// Ends a PTY session's attach side when its run ends, before the terminal
/// record is written: every connection is shut down (PR #68 review, finding 1),
/// and the run's side effects already decided are recorded, so they precede the
/// terminal event and meta write. The effects thread then stops, so nothing
/// reaches the session paths after the terminal record, when they may belong
/// to a replacement run (finding 3; invalid-states review, findings 1 and 5).
#[cfg(unix)]
struct PtyTeardown {
    registry: ConnectionRegistry,
    effects: RunEffects,
}

#[cfg(unix)]
impl PtyTeardown {
    fn run(&self) {
        self.registry.close();
        self.effects.flush_and_stop();
    }
}
#[cfg(not(unix))]
type PtyTeardown = std::convert::Infallible;

/// A change of who owns the PTY's input.
#[cfg(unix)]
#[derive(Debug, Clone, Copy)]
enum ControlChange {
    /// A human took control.
    Human(crate::pty_input::Trigger),
    /// The human released it; it falls back to the agent.
    Released,
}

/// The `pty.control_changed` event's `trigger`.
#[cfg(unix)]
#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum ChangeTrigger {
    Attach,
    Takeover,
    Detach,
}

#[cfg(unix)]
impl ControlChange {
    fn owner(self) -> PtyControl {
        match self {
            Self::Human(_) => PtyControl::HumanControl,
            Self::Released => PtyControl::AgentControl,
        }
    }

    fn trigger(self) -> ChangeTrigger {
        use crate::pty_input::Trigger;
        match self {
            Self::Human(Trigger::Attach) => ChangeTrigger::Attach,
            Self::Human(Trigger::Takeover) => ChangeTrigger::Takeover,
            Self::Released => ChangeTrigger::Detach,
        }
    }
}

/// A side effect of a PTY run, for the session's event log and `meta.json`.
/// The run's effects thread ([`RunEffects`]) is the only thread that writes
/// them, so the teardown's flush orders every one before the terminal record.
#[cfg(unix)]
enum RunEffect {
    /// `pty.control_changed`, then the new owner published to the guard and
    /// patched into `meta.json`.
    ControlChanged(ControlChange),
    /// `pty.input_revoked`: `accepted` of `total` bytes a superseded or
    /// disconnected controller queued were written.
    InputRevoked {
        kind: crate::model::pty_control::ControllerKind,
        accepted: usize,
        total: usize,
    },
    /// `recording.stopped`, then the stop patched into `meta.json`.
    RecordingStopped(crate::recorder::Stopped),
    /// Everything sent earlier is recorded: acknowledge and stop.
    Flush(std::sync::mpsc::SyncSender<()>),
}

#[cfg(unix)]
#[derive(serde::Serialize)]
struct ControlChangedData {
    control: PtyControl,
    trigger: ChangeTrigger,
}

#[cfg(unix)]
#[derive(serde::Serialize)]
struct InputRevokedData {
    kind: crate::model::pty_control::ControllerKind,
    accepted: usize,
    total: usize,
}

#[cfg(unix)]
#[derive(serde::Serialize)]
struct RecordingStoppedData {
    last_recorded_sequence: Option<u64>,
    #[serde(flatten)]
    reason: RecordingStopReason,
}

/// Records a PTY run's side effects on their own thread, in the order they
/// were decided, so a slow or stalled state root never delays a takeover's
/// acknowledgement, any PTY input step, or capture (PR #68 review, finding 3).
#[cfg(unix)]
struct RunEffects {
    tx: std::sync::mpsc::Sender<RunEffect>,
}

#[cfg(unix)]
impl RunEffects {
    /// Start the thread. `control` is the live owner the guard folds into its
    /// whole-meta writes; this thread updates it.
    fn spawn(
        session_dir: PathBuf,
        mut facts: LifecycleEvents,
        control: Arc<Mutex<PtyControl>>,
    ) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // WAL order (spec §3.6): each fact is appended before any meta
            // write can carry what it reports. The append itself is
            // best-effort, not fsynced. Meta patches are under META_WRITE, so
            // a guard write either precedes the patch or folds its state in.
            for effect in rx {
                match effect {
                    RunEffect::ControlChanged(change) => {
                        let owner = change.owner();
                        facts.append_fact(
                            Fact::ControlChanged,
                            &ControlChangedData {
                                control: owner.clone(),
                                trigger: change.trigger(),
                            },
                        );
                        let _serialized = lock(&META_WRITE);
                        *lock(&control) = owner.clone();
                        patch_meta_file(&session_dir, |meta| meta.set_pty_control(owner));
                    }
                    RunEffect::InputRevoked {
                        kind,
                        accepted,
                        total,
                    } => facts.append_fact(
                        Fact::InputRevoked,
                        &InputRevokedData {
                            kind,
                            accepted,
                            total,
                        },
                    ),
                    RunEffect::RecordingStopped(stopped) => {
                        facts.append_fact(
                            Fact::RecordingStopped,
                            &RecordingStoppedData {
                                last_recorded_sequence: stopped
                                    .last_recorded
                                    .map(crate::recording::Sequence::get),
                                reason: stop_reason(stopped.reason),
                            },
                        );
                        let _serialized = lock(&META_WRITE);
                        patch_meta_file(&session_dir, |meta| {
                            if let Some(mut recording) =
                                meta.pty().and_then(|p| p.recording.clone())
                            {
                                recording.state = stopped_state(stopped, None);
                                meta.set_pty_recording(recording);
                            }
                        });
                    }
                    RunEffect::Flush(done) => {
                        let _ = done.send(());
                        return;
                    }
                }
            }
        });
        Self { tx }
    }

    fn sender(&self) -> std::sync::mpsc::Sender<RunEffect> {
        self.tx.clone()
    }

    /// Wait until every effect sent so far is recorded, then stop: an effect
    /// sent later queues behind the flush and is dropped with the channel,
    /// never recorded.
    fn flush_and_stop(&self) {
        let (done, flushed) = std::sync::mpsc::sync_channel(1);
        if self.tx.send(RunEffect::Flush(done)).is_ok() {
            let _ = flushed.recv();
        }
    }
}

/// Maximum simultaneous attach connections, including ones awaiting a hello.
#[cfg(unix)]
const MAX_ATTACH_CONNECTIONS: usize = 8;

/// Ownership side effects, decided on the input writer thread. Only the
/// connection registry is touched there; the event log and `meta.json` are
/// written by [`RunEffects`]. Nothing here checks whether the run has ended:
/// an effect sent after the teardown's flush is never recorded.
#[cfg(unix)]
struct SidecarControlHooks {
    effects: std::sync::mpsc::Sender<RunEffect>,
    registry: ConnectionRegistry,
    attach_sink: AttachSink,
}

#[cfg(unix)]
impl SidecarControlHooks {
    /// Queue an effect. Never blocks: the queue is unbounded, and once the
    /// effects thread has stopped the send fails and the effect is dropped.
    fn record(&self, effect: RunEffect) {
        let _ = self.effects.send(effect);
    }
}

#[cfg(unix)]
impl crate::pty_input::ControlHooks for SidecarControlHooks {
    fn human_control(&mut self, trigger: crate::pty_input::Trigger) {
        // Minimal by design: who owns the PTY's input, nothing else.
        self.record(RunEffect::ControlChanged(ControlChange::Human(trigger)));
    }

    fn human_released(&mut self) {
        self.record(RunEffect::ControlChanged(ControlChange::Released));
    }

    fn retire(
        &mut self,
        holder: crate::model::pty_control::HolderId,
        kind: crate::model::pty_control::ControllerKind,
        epoch: crate::model::pty_control::ControllerEpoch,
    ) {
        // Forget the connection now, on the writer thread, before the successor
        // is granted control: from here on it can never install a viewer. An
        // unregistered holder (a FIFO exec forwarder) learns of revocation from
        // its write outcomes.
        let _ = kind;
        let Some(entry) = self.registry.remove(holder) else {
            return;
        };
        match entry.retirement {
            Retirement::Push { revoked } => {
                // Nonblocking: interrupt a handler idling on its next frame. A
                // handler waiting on a PTY write gets a NotAuthorized outcome.
                revoked.store(true, Ordering::SeqCst);
                let _ = entry.control.shutdown(std::net::Shutdown::Read);
            }
            Retirement::Viewer { conn } => {
                // Notifying and shutting down may wait on a stalled connection;
                // never block the writer with that.
                let sink = Arc::clone(&self.attach_sink);
                let control = entry.control;
                std::thread::spawn(move || {
                    retire_connection(holder, epoch, &conn, &control, &sink)
                });
            }
        }
    }

    fn input_revoked(
        &mut self,
        kind: crate::model::pty_control::ControllerKind,
        accepted: usize,
        total: usize,
    ) {
        self.record(RunEffect::InputRevoked {
            kind,
            accepted,
            total,
        });
    }
}

/// Finish retiring a connection the takeover hook has already removed from the
/// registry: tell it best-effort, shut the socket down (which also unblocks any
/// output write stuck on it), and drop its viewer if it had installed one.
///
/// `MSG_RETIRED` is best-effort by design. It shares the connection's framed
/// writer with the viewer's sender, which holds the lock for one output frame
/// at a time, so a client that reads its output gets the message within the
/// 200 ms spin. The lock stays held only while an output write is stalled,
/// that is, while the client is not reading. Such a client could not read a
/// message queued behind that output either, so it sees only end of stream.
/// Nothing depends on delivery: the takeover has already been decided, and
/// the shutdown follows either way (PR #68 review, open question a).
#[cfg(unix)]
fn retire_connection(
    holder: crate::model::pty_control::HolderId,
    epoch: crate::model::pty_control::ControllerEpoch,
    conn: &Mutex<Box<dyn Write + Send>>,
    control: &std::os::unix::net::UnixStream,
    sink: &AttachSink,
) {
    use crate::attach_proto::{Frame, write_frame};

    let _ = control.set_write_timeout(Some(std::time::Duration::from_millis(200)));
    for _ in 0..20 {
        if let Ok(mut conn) = conn.try_lock() {
            let _ = write_frame(&mut *conn, &Frame::Retired(epoch));
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let _ = control.shutdown(std::net::Shutdown::Both);
    let mut tee = lock(sink);
    if tee.viewer.as_ref().is_some_and(|v| v.holder == holder) {
        if let Some(v) = tee.viewer.take() {
            v.queue.close();
        }
    }
}

/// One acknowledged agent push: claim control if nobody holds it, write each
/// received frame before reading the next (so the client feels the PTY's
/// backpressure), and report exactly what was written.
///
/// The claim is released only after all input is written. Neither of the
/// handler's two waits can hold control hostage:
///
/// - **waiting for the next frame:** a takeover retires the registered push,
///   which flags it revoked and shuts its read side down; the handler wakes and
///   reports `revoked`;
/// - **waiting for a PTY write:** a takeover makes the write outcome
///   `NotAuthorized`; a client that vanishes is noticed by polling for hang-up,
///   and its input is cancelled.
///
/// A push that stops early reports how far it got. A vanished client gets no
/// report; its connection is simply gone.
#[cfg(unix)]
fn handle_push_connection(
    mut stream: std::os::unix::net::UnixStream,
    writer: &crate::pty_input::InputWriter,
    registry: &ConnectionRegistry,
) {
    use crate::attach_proto::{Frame, FrameError, Progress, PushOutcome, read_frame, write_frame};
    use crate::model::pty_control::{ControllerKind, IncompleteReason, InputOutcome};

    let holder = writer.next_holder();
    let revoked = Arc::new(AtomicBool::new(false));
    let Ok(control) = stream.try_clone() else {
        return;
    };
    // Register before claiming, so a takeover right after the grant retires it.
    let registered = registry.insert(
        holder,
        Registered {
            control,
            retirement: Retirement::Push {
                revoked: Arc::clone(&revoked),
            },
        },
    );
    if !registered {
        return reject_connection(&mut stream, RejectClass::Control, "session ended");
    }
    let handle = match writer.claim(holder, ControllerKind::Agent) {
        Ok(handle) => handle,
        Err(e) => {
            registry.remove(holder);
            return reject_request(&mut stream, &e);
        }
    };
    // The client vanished or broke the protocol: cancel its input.
    let gone = |stream: &std::os::unix::net::UnixStream, handle| {
        registry.remove(holder);
        let _ = stream.shutdown(std::net::Shutdown::Both);
        writer.disconnect(handle);
    };
    if write_frame(&mut stream, &Frame::Accepted(handle.epoch())).is_err() {
        return gone(&stream, handle);
    }

    // Bytes of the frames written so far: every byte of each, since a frame
    // not written in full ends the push. The outcome is built from this and
    // the frame that stopped it, so it can only report what happened.
    let mut written = 0u64;
    let outcome = loop {
        match read_frame(&mut stream) {
            Ok(Frame::Data(payload)) => {
                let len = payload.len() as u64;
                let Some(outcome) = writer.submit(&handle, payload) else {
                    continue; // an empty frame writes nothing
                };
                // The bytes of this frame that were not written.
                let stopped = |accepted: u64| Progress {
                    accepted: written + accepted,
                    unwritten: len - accepted,
                };
                match await_push_write(&outcome, &stream, &revoked) {
                    PushWrite::Done(InputOutcome::Accepted { .. }) => written += len,
                    PushWrite::Done(InputOutcome::Incomplete {
                        accepted, reason, ..
                    }) => {
                        let progress = stopped(accepted as u64);
                        break if matches!(reason, IncompleteReason::NotAuthorized(_)) {
                            PushOutcome::Revoked(progress)
                        } else {
                            PushOutcome::Closed(progress)
                        };
                    }
                    PushWrite::WriterGone => break PushOutcome::Closed(stopped(0)),
                    PushWrite::ClientGone => return gone(&stream, handle),
                }
            }
            Ok(Frame::Detach) => {
                // Every frame was written before the next was read, and any
                // frame that was not stopped the loop: all of this push is
                // written. A takeover that lands before the end marker is
                // served supersedes nothing still to write, so the push is
                // `written` either way (PR #68 review, finding 4).
                break PushOutcome::Written { bytes: written };
            }
            // Other frames, and unknown or malformed ones, are ignored.
            Ok(_) | Err(FrameError::Malformed(_) | FrameError::Unknown(_)) => {}
            Err(FrameError::Io(_)) if revoked.load(Ordering::SeqCst) => {
                break PushOutcome::Revoked(Progress {
                    accepted: written,
                    unwritten: 0,
                });
            }
            Err(FrameError::Io(_)) => return gone(&stream, handle),
        }
    };
    if let PushOutcome::Written { .. } = outcome {
        // The end marker releases control if it is still held.
        writer.end_of_input_and_wait(handle);
    } else {
        writer.disconnect(handle); // release if still held; nothing else is queued
    }
    registry.remove(holder);
    let _ = write_frame(&mut stream, &Frame::InputDone(outcome));
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

/// How waiting for one push write ended.
#[cfg(unix)]
enum PushWrite {
    Done(crate::model::pty_control::InputOutcome),
    WriterGone,
    ClientGone,
}

/// Wait for a push write's outcome, watching for the client to vanish. On
/// [`PushWrite::ClientGone`] the caller disconnects the handle, which cancels
/// the write, so the wait cannot outlast the PTY accepting nothing.
#[cfg(unix)]
fn await_push_write(
    outcome: &std::sync::mpsc::Receiver<crate::model::pty_control::InputOutcome>,
    stream: &std::os::unix::net::UnixStream,
    revoked: &AtomicBool,
) -> PushWrite {
    use std::sync::mpsc::RecvTimeoutError;

    loop {
        match outcome.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(done) => return PushWrite::Done(done),
            Err(RecvTimeoutError::Disconnected) => return PushWrite::WriterGone,
            // A retired push shut its own read side down; its write outcome
            // (NotAuthorized) is what ends the wait, not a hang-up.
            Err(RecvTimeoutError::Timeout)
                if !revoked.load(Ordering::SeqCst)
                    && crate::attach_socket::peer_hung_up(stream) =>
            {
                return PushWrite::ClientGone;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

/// Install `viewer` unless its connection has been retired.
///
/// The registry check and the install happen under the sink lock. Combined
/// with retirement removing the holder from the registry synchronously (on the
/// writer thread, before the successor is even granted control), a retired
/// connection can never install its viewer over its successor's.
#[cfg(unix)]
fn install_viewer(registry: &ConnectionRegistry, sink: &AttachSink, viewer: AttachViewer) -> bool {
    let mut tee = lock(sink);
    if !registry.contains(viewer.holder) {
        return false;
    }
    // Under the tee's lock, so no output can land between the modes it has
    // seen and the output that follows them.
    let replay = tee.modes.replay();
    if !replay.is_empty() && !viewer.queue.offer(&replay) {
        return false;
    }
    tee.viewer = Some(viewer);
    true
}

#[cfg(unix)]
fn run_attach_listener(
    listener: &std::os::unix::net::UnixListener,
    writer: &crate::pty_input::InputWriter,
    registry: &ConnectionRegistry,
    attach_sink: &AttachSink,
) {
    use std::sync::atomic::AtomicUsize;

    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else {
            continue;
        };
        // Only the run owner's own processes may attach. The socket directory
        // is already owner-only; this checks the connecting process itself.
        if crate::attach_socket::verify_peer(&stream).is_err() {
            reject_connection(&mut stream, RejectClass::Identity, "peer identity rejected");
            continue;
        }
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_ATTACH_CONNECTIONS {
            active.fetch_sub(1, Ordering::SeqCst);
            reject_connection(
                &mut stream,
                RejectClass::Runtime,
                "too many attach connections",
            );
            continue;
        }
        let (writer, registry, sink, active) = (
            writer.clone(),
            registry.clone(),
            Arc::clone(attach_sink),
            Arc::clone(&active),
        );
        std::thread::spawn(move || {
            handle_attach_connection(stream, &writer, &registry, &sink);
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

#[cfg(unix)]
fn reject_connection(
    stream: &mut std::os::unix::net::UnixStream,
    class: RejectClass,
    reason: &str,
) {
    use crate::attach_proto::{Frame, write_frame};
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_millis(200)));
    let refusal = Frame::Rejected {
        class,
        reason: reason.to_owned(),
    };
    let _ = write_frame(stream, &refusal);
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

/// Refuse a connection whose claim or takeover the input writer turned down.
#[cfg(unix)]
fn reject_request(
    stream: &mut std::os::unix::net::UnixStream,
    error: &crate::pty_input::RequestError,
) {
    reject_connection(stream, request_reject_class(error), &error.to_string());
}

/// A refused claim or takeover's class: every control refusal (a holder, a
/// stale handle, an exhausted epoch) is a control conflict; a writer that has
/// stopped is a runtime failure.
#[cfg(unix)]
fn request_reject_class(error: &crate::pty_input::RequestError) -> RejectClass {
    use crate::pty_input::RequestError;
    match error {
        RequestError::Control(_) => RejectClass::Control,
        RequestError::WriterGone => RejectClass::Runtime,
    }
}

/// One attach connection: v1 hello, claim or takeover through the input writer,
/// then relay input and resizes until detach, disconnect, or retirement.
#[cfg(unix)]
fn handle_attach_connection(
    mut stream: std::os::unix::net::UnixStream,
    writer: &crate::pty_input::InputWriter,
    registry: &ConnectionRegistry,
    attach_sink: &AttachSink,
) {
    use crate::attach_proto::{
        self, Frame, FrameError, Mode, ProtocolError, read_frame, write_frame,
    };
    use crate::model::pty_control::ControllerKind;

    // One overall deadline for the whole hello: a client trickling bytes cannot
    // hold a connection slot open by keeping each individual read short.
    let hello = read_frame(&mut crate::attach_socket::DeadlineReader {
        stream: &stream,
        deadline: std::time::Instant::now() + attach_proto::HELLO_TIMEOUT,
    });
    let mode = match hello {
        Ok(Frame::Hello(mode)) => mode,
        Err(FrameError::Malformed(e @ ProtocolError::UnsupportedHello)) => {
            return reject_connection(&mut stream, RejectClass::Protocol, &e.to_string());
        }
        _ => {
            return reject_connection(
                &mut stream,
                RejectClass::Protocol,
                "attach requires a protocol hello",
            );
        }
    };
    let _ = stream.set_read_timeout(None);
    if mode == Mode::Push {
        return handle_push_connection(stream, writer, registry);
    }

    let (Ok(conn_stream), Ok(control)) = (stream.try_clone(), stream.try_clone()) else {
        return;
    };
    let conn: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(Box::new(conn_stream)));
    let holder = writer.next_holder();
    // Register before asking for control, so a takeover that lands immediately
    // after the grant can still find and retire this connection.
    let registered = registry.insert(
        holder,
        Registered {
            control,
            retirement: Retirement::Viewer {
                conn: Arc::clone(&conn),
            },
        },
    );
    if !registered {
        return reject_connection(&mut stream, RejectClass::Control, "session ended");
    }
    let granted = if mode == Mode::Takeover {
        writer.takeover(holder).map(|t| t.handle)
    } else {
        writer.claim(holder, ControllerKind::Human)
    };
    let handle = match granted {
        Ok(handle) => handle,
        Err(e) => {
            registry.remove(holder);
            return reject_request(&mut stream, &e);
        }
    };

    // Reply before installing the viewer, so tee output never precedes it.
    let accepted = write_frame(&mut *lock(&conn), &Frame::Accepted(handle.epoch()));
    let queue = Arc::new(ViewerQueue::default());
    if accepted.is_ok() {
        let sender = {
            let (queue, conn) = (Arc::clone(&queue), Arc::clone(&conn));
            std::thread::spawn(move || run_viewer_sender(&queue, &conn))
        };
        if let Ok(shutdown) = stream.try_clone() {
            // Refused if this connection was retired in the meantime.
            install_viewer(
                registry,
                attach_sink,
                AttachViewer {
                    holder,
                    queue: Arc::clone(&queue),
                    disconnect: Box::new(move || {
                        let _ = shutdown.shutdown(std::net::Shutdown::Both);
                    }),
                },
            );
        }
        drop(sender); // detached: exits when the queue or connection closes

        loop {
            match read_frame(&mut stream) {
                Ok(Frame::Data(payload)) => {
                    // A full input queue must not hide a client that has left.
                    let queued = writer.write_nowait_unless(&handle, payload, || {
                        crate::attach_socket::peer_hung_up(&stream)
                    });
                    if !queued {
                        break;
                    }
                }
                Ok(Frame::Resize(size)) => writer.resize(&handle, size),
                Ok(Frame::Detach) | Err(FrameError::Io(_)) => break,
                // Other frames, unknown ones, and malformed ones such as a
                // resize with a zero dimension, are ignored.
                Ok(_) | Err(FrameError::Malformed(_) | FrameError::Unknown(_)) => {}
            }
        }
    }

    // Detach, disconnect, retirement, or a protocol violation such as an
    // oversized frame: close the connection, release ahead of queued input,
    // and cancel what this client still has queued.
    let _ = stream.shutdown(std::net::Shutdown::Both);
    writer.disconnect(handle);
    registry.remove(holder);
    let mut tee = lock(attach_sink);
    if tee.viewer.as_ref().is_some_and(|v| v.holder == holder) {
        tee.viewer = None;
    }
    drop(tee);
    queue.close();
}

#[cfg(unix)]
fn apply_pty_resize(fd: &std::fs::File, size: crate::recording::Geometry) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let ws = libc::winsize {
        ws_row: size.rows(),
        ws_col: size.cols(),
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: fd is a valid PTY master fd. TIOCSWINSZ with a valid winsize
    // pointer is safe on any terminal fd.
    if unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ, &ws) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Serializes the sidecar's `meta.json` writes: the patches the effects thread
/// makes ([`RunEffects`]: control changes and recording stops), and the
/// lifecycle guard's writes, which fold in the live owner and the recording
/// state they read under this lock.
static META_WRITE: Mutex<()> = Mutex::new(());

/// Best-effort: patch the session's `meta.json` through a typed `Meta`
/// round-trip (read → `patch` → write); the caller holds [`META_WRITE`]. Keeps meta.json
/// a valid typed `Meta` — consistent with how the sidecar serializes meta
/// everywhere else — instead of hand-patching an untyped JSON value.
/// Persistence is best-effort tmp+rename (no fsync): the durable lifecycle
/// writes go through `write_meta_atomic` elsewhere.
#[cfg(unix)]
fn patch_meta_file(session_dir: &Path, patch: impl FnOnce(&mut Meta)) {
    let meta_path = session_dir.join("meta.json");
    let Ok(content) = std::fs::read_to_string(&meta_path) else {
        return;
    };
    let Ok(mut meta) = serde_json::from_str::<Meta>(&content) else {
        return;
    };
    patch(&mut meta);
    let Ok(json) = serde_json::to_string_pretty(&meta) else {
        return;
    };
    let tmp = session_dir.join("meta.json.tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, &meta_path);
    }
}

/// A PTY run's recording: the recorder and where its segments live.
struct PtyRecordingRun {
    thread: RecorderThread,
    /// Relative to the session directory.
    dir: String,
}

impl PtyRecordingRun {
    fn meta(&self, state: RecordingState) -> PtyRecording {
        PtyRecording {
            dir: self.dir.clone(),
            input_recorded: false,
            state,
        }
    }
}

/// Start recording a PTY run: output and applied geometry, not input. The child
/// starts at the platform's initial PTY size, under its `TERM` ([`pty_term`];
/// recorded empty if not recordable). A stop is queued on the run's effects channel,
/// which appends it as a `recording.stopped` event and writes it to
/// `meta.json`; the recorder never touches the session paths itself.
#[cfg(unix)]
fn start_pty_recording(
    session_dir: &Path,
    run_id: RunId,
    launch_env: &std::collections::BTreeMap<String, String>,
    limits: crate::recorder::RecorderLimits,
    effects: std::sync::mpsc::Sender<RunEffect>,
) -> PtyRecordingRun {
    use crate::recording::{Geometry, SegmentHeader, Sequence, TermName};

    let dir = format!("recording/{run_id}");
    let term = TermName::new(pty_term(launch_env))
        .unwrap_or_else(|_| TermName::new("").expect("an empty TERM is recordable"));
    let origin_unix_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
    let header = SegmentHeader {
        run_id,
        segment_index: 0,
        first_sequence: Sequence::FIRST,
        origin_unix_ns,
        geometry: Geometry::new(
            crate::platform::INITIAL_PTY_ROWS,
            crate::platform::INITIAL_PTY_COLS,
        )
        .expect("the initial PTY size is nonzero"),
        input_recorded: false,
        term,
    };

    let store = crate::recorder::DirectoryStore::new(session_dir.join(&dir));
    // Runs at the stop, with the recorder locked: only queue it.
    let thread = RecorderThread::start(header, store, limits, move |stopped| {
        let _ = effects.send(RunEffect::RecordingStopped(stopped));
    });
    PtyRecordingRun { thread, dir }
}

/// Recording limits. Debug builds accept `TENDR_TEST_RECORDING_MAX_BYTES` so
/// tests can reach the size limit; release sidecars ignore it.
#[cfg(unix)]
fn recorder_limits() -> crate::recorder::RecorderLimits {
    let mut limits = crate::recorder::RecorderLimits::default();
    if cfg!(debug_assertions) {
        if let Some(max) = std::env::var("TENDR_TEST_RECORDING_MAX_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            limits.max_bytes = max;
        }
    }
    limits
}

/// The recorder's stop reason as metadata and events carry it.
fn stop_reason(reason: StopReason) -> RecordingStopReason {
    match reason {
        StopReason::SizeLimit => RecordingStopReason::SizeLimit,
        StopReason::WriteFailed(kind) => RecordingStopReason::WriteFailed {
            error: format!("{kind:?}"),
        },
        StopReason::BacklogFull => RecordingStopReason::BacklogFull,
        StopReason::Stalled => RecordingStopReason::Stalled,
    }
}

fn stopped_state(
    stopped: crate::recorder::Stopped,
    last_synced: Option<crate::recording::Sequence>,
) -> RecordingState {
    RecordingState::Stopped {
        last_recorded_sequence: stopped.last_recorded.map(crate::recording::Sequence::get),
        last_synced_sequence: last_synced.map(crate::recording::Sequence::get),
        reason: stop_reason(stopped.reason),
    }
}

/// The recording's final state, and a run warning if it stopped early.
fn finished_recording(summary: RecorderSummary) -> (RecordingState, Option<String>) {
    let Some(stopped) = summary.stopped else {
        return (
            RecordingState::Complete {
                last_recorded_sequence: summary.last_recorded.map(crate::recording::Sequence::get),
                last_synced_sequence: summary.last_synced.map(crate::recording::Sequence::get),
            },
            None,
        );
    };
    let at = stopped.last_recorded.map_or_else(
        || "before its first record".to_owned(),
        |s| format!("at sequence {}", s.get()),
    );
    let cause = match stopped.reason {
        StopReason::WriteFailed(kind) => format!("write_failed: {kind:?}"),
        reason => reason.as_str().to_owned(),
    };
    (
        stopped_state(stopped, summary.last_synced),
        Some(format!(
            "recording stopped {at} ({cause}); later output was not recorded"
        )),
    )
}

// Every test here exercises the Unix-only PTY control path.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::model::ids::{EpochTimestamp, Generation, ProcessIdentity, RunId, SessionName};
    use crate::model::pty::PtyControl;
    use crate::model::spec::LaunchSpec;
    use crate::recorder::RecorderLimits;
    use std::num::NonZeroU32;

    /// A refused claim or takeover goes out with the class the client maps to
    /// an exit code: any holder or stale handle is a control conflict.
    #[test]
    fn a_refused_request_is_classed_by_why() {
        use crate::model::pty_control::{
            ControlError, ControllerEpoch, ControllerKind, HolderId, StaleReason,
        };
        use crate::pty_input::RequestError;

        let busy = ControlError::Busy {
            holder: HolderId::new(1),
            kind: ControllerKind::Agent,
            epoch: ControllerEpoch::new(3),
        };
        let stale = ControlError::Stale {
            reason: StaleReason::EpochMismatch,
        };
        for control in [busy, stale, ControlError::EpochExhausted] {
            assert_eq!(
                request_reject_class(&RequestError::Control(control)),
                RejectClass::Control,
                "{control:?}"
            );
        }
        assert_eq!(
            request_reject_class(&RequestError::WriterGone),
            RejectClass::Runtime
        );
    }

    #[test]
    fn takeover_retirement_refuses_the_old_viewer_before_the_hook_returns() {
        use crate::model::pty_control::{ControllerEpoch, ControllerKind, HolderId};
        use crate::pty_input::ControlHooks;
        use std::os::unix::net::UnixStream;

        let dir = tempfile::tempdir().unwrap();
        let registry = ConnectionRegistry::default();
        let sink: AttachSink = Arc::default();
        let (old_conn, _peer) = UnixStream::pair().unwrap();
        let old = HolderId::new(1);
        assert!(registry.insert(
            old,
            Registered {
                retirement: Retirement::Viewer {
                    conn: Arc::new(Mutex::new(Box::new(old_conn.try_clone().unwrap()))),
                },
                control: old_conn,
            },
        ));
        let (mut hooks, _control, _teardown) = hooks_for(dir.path());
        hooks.registry = registry.clone();
        hooks.attach_sink = Arc::clone(&sink);

        hooks.retire(old, ControllerKind::Human, ControllerEpoch::new(2));

        // No waiting: the old connection may try to install at any instant after
        // the takeover is decided, including before any notification thread runs.
        let installed = install_viewer(
            &registry,
            &sink,
            AttachViewer {
                holder: old,
                queue: Arc::new(ViewerQueue::default()),
                disconnect: Box::new(|| {}),
            },
        );
        assert!(!installed, "a retired connection installed its viewer");
        assert!(lock(&sink).viewer.is_none());
    }

    /// Control hooks for a session at `dir`, the owner they publish, and the
    /// teardown that ends them.
    fn hooks_for(dir: &Path) -> (SidecarControlHooks, Arc<Mutex<PtyControl>>, PtyTeardown) {
        let control = Arc::new(Mutex::new(PtyControl::AgentControl));
        let effects = RunEffects::spawn(
            dir.to_path_buf(),
            LifecycleEvents::new(
                dir,
                &Namespace::new("default").unwrap(),
                &SessionName::new("effects").unwrap(),
                RunId::new(),
                Generation::first(),
            ),
            Arc::clone(&control),
        );
        let registry = ConnectionRegistry::default();
        let hooks = SidecarControlHooks {
            effects: effects.sender(),
            registry: registry.clone(),
            attach_sink: Arc::default(),
        };
        let teardown = PtyTeardown { registry, effects };
        (hooks, control, teardown)
    }

    /// Make `dir/meta.json` a FIFO: reading it blocks until a writer opens it,
    /// standing in for a stalled state root.
    fn stall_meta_json(dir: &Path) {
        use std::os::unix::ffi::OsStrExt;
        let path = dir.join("meta.json");
        let _ = std::fs::remove_file(&path);
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    }

    /// Feed the stalled `meta.json` FIFO one valid meta, unblocking its reader.
    fn unstall_meta_json(dir: &Path, meta: &Meta) {
        let mut fifo = std::fs::OpenOptions::new()
            .write(true)
            .open(dir.join("meta.json"))
            .unwrap();
        fifo.write_all(serde_json::to_string_pretty(meta).unwrap().as_bytes())
            .unwrap();
    }

    /// A takeover is acknowledged as soon as the owner is updated in memory: the
    /// control hooks, which run on the input writer thread, must not wait for
    /// the event log or `meta.json` (PR #68 review, finding 3).
    #[test]
    fn control_hooks_do_not_wait_for_a_stalled_meta_json() {
        use crate::pty_input::ControlHooks;

        let dir = tempfile::tempdir().unwrap();
        let meta = write_pty_meta(dir.path());
        stall_meta_json(dir.path());
        let (mut hooks, control, teardown) = hooks_for(dir.path());

        let (done, returned) = std::sync::mpsc::channel();
        let writer_thread = std::thread::spawn(move || {
            hooks.human_control(crate::pty_input::Trigger::Takeover);
            let _ = done.send(());
            hooks
        });
        let prompt = returned
            .recv_timeout(std::time::Duration::from_secs(2))
            .is_ok();
        // Unblock whoever reads meta.json, so neither outcome hangs the test.
        unstall_meta_json(dir.path(), &meta);
        let _hooks = writer_thread.join().unwrap();
        assert!(prompt, "human_control waited for meta.json");

        // The change is still recorded, in order, once the state root recovers.
        teardown.run();
        assert_eq!(*lock(&control), PtyControl::HumanControl);
        let on_disk: Meta =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("meta.json")).unwrap())
                .unwrap();
        assert_eq!(on_disk.pty().unwrap().control, PtyControl::HumanControl);
        assert_eq!(control_facts(dir.path()), ["pty.control_changed"]);
    }

    /// The control and revocation facts in `dir`'s event log, in order.
    fn control_facts(dir: &Path) -> Vec<String> {
        crate::events::read_session_events(dir)
            .unwrap()
            .events
            .iter()
            .map(|e| e.kind.as_str().to_owned())
            .filter(|k| k.starts_with("pty."))
            .collect()
    }

    /// A run's end records every control effect already decided, before the
    /// terminal record, and nothing decided after it (PR #68 review, findings 1
    /// and 3): the session paths may belong to a replacement run by then.
    #[test]
    fn a_run_end_records_queued_control_effects_and_nothing_later() {
        use crate::model::pty_control::ControllerKind;
        use crate::pty_input::ControlHooks;

        let dir = tempfile::tempdir().unwrap();
        write_pty_meta(dir.path());
        let (mut hooks, _control, teardown) = hooks_for(dir.path());

        hooks.human_control(crate::pty_input::Trigger::Attach);
        hooks.input_revoked(ControllerKind::Agent, 1, 2);
        teardown.run();
        // Recorded by the time the teardown returns, without polling.
        assert_eq!(
            control_facts(dir.path()),
            ["pty.control_changed", "pty.input_revoked"]
        );

        hooks.human_released();
        hooks.input_revoked(ControllerKind::Human, 0, 1);
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(control_facts(dir.path()).len(), 2, "nothing after the end");
        let on_disk: Meta =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("meta.json")).unwrap())
                .unwrap();
        assert_eq!(on_disk.pty().unwrap().control, PtyControl::HumanControl);
    }

    /// A PTY meta.json for run `run_id` whose recording is still `Recording`.
    fn recording_meta(run_id: RunId) -> Meta {
        let mut meta = Meta::new_starting(
            SessionName::new("ptytest").unwrap(),
            run_id,
            Generation::first(),
            LaunchSpec::new(vec!["bash".into()]).unwrap(),
            ProcessIdentity {
                pid: NonZeroU32::new(100).unwrap(),
                start_time_ns: 1000,
            },
            EpochTimestamp::now(),
        );
        meta.set_pty(PtyMeta::new());
        meta.set_pty_recording(PtyRecording {
            dir: format!("recording/{run_id}"),
            input_recorded: false,
            state: RecordingState::Recording,
        });
        meta
    }

    fn read_meta(dir: &Path) -> Meta {
        serde_json::from_str(&std::fs::read_to_string(dir.join("meta.json")).unwrap()).unwrap()
    }

    fn event_kinds(dir: &Path) -> Vec<String> {
        crate::events::read_session_events(dir)
            .unwrap()
            .events
            .iter()
            .map(|e| e.kind.as_str().to_owned())
            .collect()
    }

    /// A recording's stop report is written before the run's terminal record
    /// or not at all, never after the run has ended (invalid-states review of
    /// PR #68, finding 1). After the end the session paths may belong to a
    /// replacement run, whose meta.json a late report would patch with this
    /// run's stop.
    ///
    /// The recording stops at once (its directory cannot be created) while
    /// meta.json is stalled, so the report cannot finish. The run then ends:
    /// the recording is finished and the attach side torn down. If that
    /// returns while the report is still pending, the replacement's meta is
    /// what the report finds when the state root recovers. So the end must not
    /// return until the report is written.
    #[test]
    fn a_stalled_recording_stop_report_never_lands_after_the_run_ends() {
        let dir = tempfile::tempdir().unwrap();
        let run_a = RunId::new();
        let meta_a = recording_meta(run_a);
        std::fs::write(dir.path().join("recording"), b"not a directory").unwrap();
        stall_meta_json(dir.path());
        let (hooks, _control, teardown) = hooks_for(dir.path());
        let limits = RecorderLimits {
            close_timeout: std::time::Duration::from_millis(50),
            ..RecorderLimits::default()
        };
        let recording = start_pty_recording(
            dir.path(),
            run_a,
            &std::collections::BTreeMap::new(),
            limits,
            hooks.effects.clone(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while recording.thread.recorder().stopped().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "recording never stopped"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        // The run ends, as SupervisedRun::finish does before its terminal record.
        let (ended_tx, ended) = std::sync::mpsc::channel();
        let run_end = std::thread::spawn(move || {
            let (state, _warning) = finished_recording(recording.thread.finish());
            teardown.run();
            let _ = ended_tx.send(());
            state
        });

        // The run's end must wait for the report. A 50 ms close timeout
        // against a 2 s window leaves a 40x margin, so a run end that returns
        // early fails here rather than slipping past on a slow runner.
        assert!(
            ended
                .recv_timeout(std::time::Duration::from_secs(2))
                .is_err(),
            "the run ended while its recording's stop report was still pending"
        );
        // Let the state root recover with this run's own meta; the end completes.
        unstall_meta_json(dir.path(), &meta_a);
        ended
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the run ends once the state root recovers");
        let final_state = run_end.join().unwrap();
        assert!(matches!(final_state, RecordingState::Stopped { .. }));

        // Recorded before the end returned, so before the terminal record.
        let on_disk = read_meta(dir.path());
        assert_eq!(on_disk.run_id(), run_a);
        assert!(
            matches!(
                on_disk.pty().unwrap().recording.as_ref().unwrap().state,
                RecordingState::Stopped { .. }
            ),
            "the stop is in meta.json before the run's end returns"
        );
        assert!(
            event_kinds(dir.path()).contains(&"recording.stopped".to_owned()),
            "the stop is in the event log before the run's end returns"
        );
        // The replacement run takes over the session paths.
        let meta_b = recording_meta(RunId::new());
        std::fs::write(
            dir.path().join("meta.json"),
            serde_json::to_string_pretty(&meta_b).unwrap(),
        )
        .unwrap();
        let events_at_end = event_kinds(dir.path());

        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(
            serde_json::to_value(read_meta(dir.path())).unwrap(),
            serde_json::to_value(&meta_b).unwrap(),
            "run A's stop report patched the replacement run's meta.json"
        );
        assert_eq!(
            event_kinds(dir.path()),
            events_at_end,
            "nothing after the end"
        );
    }

    /// Every fact kind is grammatical, so `append_fact` never drops one for
    /// its kind.
    #[test]
    fn every_fact_kind_is_grammatical() {
        for fact in [
            Fact::CallbackFinished,
            Fact::RecordingStopped,
            Fact::ControlChanged,
            Fact::InputRevoked,
        ] {
            assert!(Kind::new(fact.kind()).is_ok(), "{fact:?}");
        }
    }

    /// The typed effects keep the events' v1 data shapes.
    #[test]
    fn run_effects_keep_their_event_data() {
        use crate::model::pty_control::ControllerKind;
        use crate::pty_input::{ControlHooks, Trigger};

        let dir = tempfile::tempdir().unwrap();
        write_pty_meta(dir.path());
        let (mut hooks, _control, teardown) = hooks_for(dir.path());
        hooks.human_control(Trigger::Takeover);
        hooks.human_released();
        hooks.input_revoked(ControllerKind::Agent, 1, 2);
        let _ = hooks
            .effects
            .send(RunEffect::RecordingStopped(crate::recorder::Stopped {
                last_recorded: crate::recording::Sequence::new(4),
                reason: StopReason::WriteFailed(io::ErrorKind::StorageFull),
            }));
        teardown.run();

        let data: Vec<serde_json::Value> = crate::events::read_session_events(dir.path())
            .unwrap()
            .events
            .iter()
            .map(|e| serde_json::json!({"kind": e.kind.as_str(), "data": e.data}))
            .collect();
        assert_eq!(
            data,
            [
                serde_json::json!({"kind": "pty.control_changed",
                    "data": {"control": "HumanControl", "trigger": "takeover"}}),
                serde_json::json!({"kind": "pty.control_changed",
                    "data": {"control": "AgentControl", "trigger": "detach"}}),
                serde_json::json!({"kind": "pty.input_revoked",
                    "data": {"kind": "Agent", "accepted": 1, "total": 2}}),
                serde_json::json!({"kind": "recording.stopped",
                    "data": {"last_recorded_sequence": 4, "reason": "write_failed",
                        "error": "StorageFull"}}),
            ]
        );
    }

    /// A PTY that accepts everything and records it.
    #[derive(Clone, Default)]
    struct RecordingPty(Arc<Mutex<Vec<u8>>>);

    impl crate::model::pty_control::PtyInputSink for RecordingPty {
        fn try_write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            lock(&self.0).extend_from_slice(bytes);
            Ok(bytes.len())
        }
    }

    impl crate::pty_input::WritablePty for RecordingPty {
        fn wait_writable(&self, _timeout: std::time::Duration) -> io::Result<bool> {
            Ok(true)
        }

        fn resize(&self, _size: crate::recording::Geometry) {}
    }

    /// Hooks with no side effects: a takeover supersedes the push without
    /// interrupting its connection, as when its end marker is already on the way.
    struct InertHooks;

    impl crate::pty_input::ControlHooks for InertHooks {
        fn human_control(&mut self, _trigger: crate::pty_input::Trigger) {}
        fn human_released(&mut self) {}
        fn retire(
            &mut self,
            _holder: crate::model::pty_control::HolderId,
            _kind: crate::model::pty_control::ControllerKind,
            _epoch: crate::model::pty_control::ControllerEpoch,
        ) {
        }
        fn input_revoked(
            &mut self,
            _kind: crate::model::pty_control::ControllerKind,
            _accepted: usize,
            _total: usize,
        ) {
        }
    }

    /// A takeover that lands after a push's last frame was written, but before
    /// its end marker is served, leaves nothing of the push unwritten: the push
    /// reports `written` (PR #68 review, finding 4).
    #[test]
    fn a_push_taken_over_after_its_last_frame_reports_written() {
        use crate::attach_proto::{Frame, PushOutcome, read_frame, write_frame};
        use std::os::unix::net::UnixStream;

        let pty = RecordingPty::default();
        let writer = crate::pty_input::InputWriter::spawn(RunId::new(), pty.clone(), InertHooks);
        let registry = ConnectionRegistry::default();
        let (server, mut client) = UnixStream::pair().unwrap();
        let handler = {
            let (writer, registry) = (writer.clone(), registry.clone());
            std::thread::spawn(move || handle_push_connection(server, &writer, &registry))
        };
        assert!(matches!(
            read_frame(&mut client).unwrap(),
            Frame::Accepted(_)
        ));

        write_frame(&mut client, &Frame::Data(b"abc".to_vec())).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while lock(&pty.0).len() < 3 {
            assert!(
                std::time::Instant::now() < deadline,
                "the frame was never written"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // The frame is written, so its Accepted outcome is already on its way;
        // the takeover is served before the end marker below.
        writer.takeover(writer.next_holder()).unwrap();
        write_frame(&mut client, &Frame::Detach).unwrap();

        assert_eq!(
            read_frame(&mut client).unwrap(),
            Frame::InputDone(PushOutcome::Written { bytes: 3 }),
            "every byte was written, so the push must not report revoked"
        );
        handler.join().unwrap();
    }

    /// Write a PTY-enabled meta.json (AgentControl by default) into `dir`.
    fn write_pty_meta(dir: &Path) -> Meta {
        let mut meta = Meta::new_starting(
            SessionName::new("ptytest").unwrap(),
            RunId::new(),
            Generation::first(),
            LaunchSpec::new(vec!["bash".into()]).unwrap(),
            ProcessIdentity {
                pid: NonZeroU32::new(100).unwrap(),
                start_time_ns: 1000,
            },
            EpochTimestamp::now(),
        );
        meta.set_pty(PtyMeta::new());
        std::fs::write(
            dir.join("meta.json"),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();
        meta
    }

    /// attach -> HumanControl and detach -> AgentControl each leave meta.json a
    /// valid typed Meta carrying the expected pty.control, other fields intact.
    #[test]
    fn set_pty_control_round_trips_as_typed_meta() {
        let dir = tempfile::tempdir().unwrap();
        let original = write_pty_meta(dir.path());
        assert_eq!(original.pty().unwrap().control, PtyControl::AgentControl);

        // attach
        patch_meta_file(dir.path(), |m| m.set_pty_control(PtyControl::HumanControl));
        let after_attach: Meta =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("meta.json")).unwrap())
                .expect("meta.json still deserializes as typed Meta after attach");
        assert_eq!(
            after_attach.pty().unwrap().control,
            PtyControl::HumanControl
        );
        assert_eq!(
            after_attach.run_id().to_string(),
            original.run_id().to_string(),
            "a control flip must not disturb other meta fields"
        );

        // detach
        patch_meta_file(dir.path(), |m| m.set_pty_control(PtyControl::AgentControl));
        let after_detach: Meta =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("meta.json")).unwrap())
                .unwrap();
        assert_eq!(
            after_detach.pty().unwrap().control,
            PtyControl::AgentControl
        );
    }
}
