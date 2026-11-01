//! The sidecar's PTY input authority: one writer thread per PTY run.
//!
//! The thread exclusively owns the run's [`InputArbiter`] and the PTY master's
//! write half. Every other thread — attach connections, the push forwarder —
//! talks to it through [`InputWriter`], which carries two bounded queues:
//!
//! - **controls** (claim, takeover, release, resize, repaint) are always served
//!   before the next input step, so a full PTY input buffer or a long queued
//!   push can never delay a takeover;
//! - **inputs** (write requests and end-of-input markers) are served in order,
//!   one nonblocking write attempt at a time, and apply backpressure to their
//!   senders when full.
//!
//! Authorization and each write happen inside [`InputArbiter::write_step`] on
//! this thread, so no takeover can land between the check and the write. A
//! request queued by a controller that has since been superseded writes nothing
//! further. Contract: [`crate::model::pty_control`].

use std::collections::VecDeque;
use std::io;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use crate::model::ids::RunId;
use crate::model::pty_control::{
    ControlError, ControllerEpoch, ControllerHandle, ControllerKind, HandleKey, HolderId,
    IncompleteReason, InputArbiter, InputOutcome, PendingInput, PtyInputSink, RequestId, Takeover,
    WriteStep,
};
use crate::recording::{Geometry, ResizeCause};

/// Maximum queued control requests before senders wait.
const CONTROL_CAPACITY: usize = 16;
/// Maximum queued input items before senders wait.
const INPUT_CAPACITY: usize = 32;
/// How long the writer waits for writability before rechecking controls.
const WRITABLE_POLL: Duration = Duration::from_millis(10);

/// A PTY input the writer thread can drive without blocking.
pub trait WritablePty: PtyInputSink + Send + 'static {
    /// Wait up to `timeout` for the PTY to accept input.
    ///
    /// # Errors
    ///
    /// The descriptor's poll error.
    fn wait_writable(&self, timeout: Duration) -> io::Result<bool>;

    /// Apply a window size to the PTY, recorded with `cause`.
    fn resize(&self, size: Geometry, cause: ResizeCause);

    /// The PTY's window size, if it can be read.
    fn size(&self) -> Option<Geometry> {
        None
    }
}

/// How a human took control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// A plain attach, while nobody held control.
    Attach,
    /// A takeover, retiring any current controller.
    Takeover,
}

/// Side effects of ownership changes, run on the writer thread. Implementations
/// must return promptly; anything that can block belongs on another thread.
pub trait ControlHooks: Send + 'static {
    /// A human took control at `epoch`.
    fn human_control(&mut self, trigger: Trigger, epoch: ControllerEpoch);
    /// No human holds control any more; `epoch` is the one released.
    fn human_released(&mut self, epoch: ControllerEpoch);
    /// `holder` was superseded by a takeover at `epoch` and must be retired.
    fn retire(&mut self, holder: HolderId, kind: ControllerKind, epoch: ControllerEpoch);
    /// A request from a superseded controller was stopped.
    fn input_revoked(&mut self, kind: ControllerKind, accepted: usize, total: usize);
}

enum Control {
    Claim {
        holder: HolderId,
        kind: ControllerKind,
        reply: SyncSender<Result<ControllerHandle, ControlError>>,
    },
    Takeover {
        holder: HolderId,
        reply: SyncSender<Result<Takeover, ControlError>>,
    },
    Resize {
        key: HandleKey,
        size: Geometry,
    },
    /// Shrink the PTY by one row so its child redraws; replies whether it
    /// did.
    RepaintShrink {
        key: HandleKey,
        correlation: NonZeroU32,
        reply: SyncSender<bool>,
    },
    /// Restore the size the shrink with `correlation` left, if nothing has
    /// resized the PTY since.
    RepaintRestore {
        correlation: NonZeroU32,
    },
    /// Release now and cancel everything the holder still has queued.
    Disconnect {
        handle: ControllerHandle,
    },
}

enum InputItem {
    Write {
        request: PendingInput,
        reply: Option<SyncSender<InputOutcome>>,
    },
    End {
        handle: ControllerHandle,
        /// Told once the end marker has been served.
        done: Option<SyncSender<()>>,
    },
}

#[derive(Default)]
struct Queues {
    controls: VecDeque<Control>,
    inputs: VecDeque<InputItem>,
}

struct Shared {
    queues: Mutex<Queues>,
    /// Signals the writer that work arrived.
    work: Condvar,
    /// Signals senders that queue space was freed.
    space: Condvar,
    next_holder: AtomicU64,
    next_request: AtomicU64,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Queues> {
        self.queues.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Why a control request did not take effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RequestError {
    #[error(transparent)]
    Control(#[from] ControlError),
    #[error("the PTY input writer is no longer running")]
    WriterGone,
}

/// Cloneable sender side of a PTY run's input authority.
#[derive(Clone)]
pub struct InputWriter {
    shared: Arc<Shared>,
}

impl InputWriter {
    /// Start the writer thread for `run_id`, owning `pty`.
    pub fn spawn<P: WritablePty, H: ControlHooks>(run_id: RunId, pty: P, hooks: H) -> Self {
        let shared = Arc::new(Shared {
            queues: Mutex::new(Queues::default()),
            work: Condvar::new(),
            space: Condvar::new(),
            next_holder: AtomicU64::new(1),
            next_request: AtomicU64::new(1),
        });
        let thread_shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            writer_loop(&thread_shared, InputArbiter::new(run_id), pty, hooks)
        });
        Self { shared }
    }

    /// A fresh holder identity for a connection or claim.
    #[must_use]
    pub fn next_holder(&self) -> HolderId {
        HolderId::new(self.shared.next_holder.fetch_add(1, Ordering::Relaxed))
    }

    /// Take control of an unowned PTY.
    ///
    /// # Errors
    ///
    /// [`ControlError::Busy`] if another controller holds it;
    /// [`RequestError::WriterGone`] if the writer thread has stopped.
    pub fn claim(
        &self,
        holder: HolderId,
        kind: ControllerKind,
    ) -> Result<ControllerHandle, RequestError> {
        let (reply, rx) = sync_channel(1);
        self.push_control(Control::Claim {
            holder,
            kind,
            reply,
        });
        Ok(rx.recv().map_err(|_| RequestError::WriterGone)??)
    }

    /// Human takeover, retiring any current controller.
    ///
    /// # Errors
    ///
    /// [`ControlError::EpochExhausted`]; [`RequestError::WriterGone`].
    pub fn takeover(&self, holder: HolderId) -> Result<Takeover, RequestError> {
        let (reply, rx) = sync_channel(1);
        self.push_control(Control::Takeover { holder, reply });
        Ok(rx.recv().map_err(|_| RequestError::WriterGone)??)
    }

    /// Apply a window size if `handle` still authorizes.
    pub fn resize(&self, handle: &ControllerHandle, size: Geometry) {
        self.push_control(Control::Resize {
            key: handle.key(),
            size,
        });
    }

    /// Start a repaint: shrink the PTY by one row, recorded as
    /// [`ResizeCause::Repaint`], if `handle` authorizes and its last resize
    /// left the size unchanged (a new size already makes the child redraw).
    /// At most once per such resize. Returns whether it shrank the PTY; if
    /// so, [`InputWriter::repaint_restore`] must follow.
    pub fn repaint_shrink(&self, handle: &ControllerHandle, correlation: NonZeroU32) -> bool {
        let (reply, rx) = sync_channel(1);
        self.push_control(Control::RepaintShrink {
            key: handle.key(),
            correlation,
            reply,
        });
        rx.recv().unwrap_or(false)
    }

    /// Finish the repaint started with `correlation`: put back the size it
    /// shrank, unless a resize has applied since. Not tied to control: the
    /// client may have left, but the shrink must not outlive it.
    pub fn repaint_restore(&self, correlation: NonZeroU32) {
        self.push_control(Control::RepaintRestore { correlation });
    }

    /// Queue `bytes` and wait for the outcome. Empty input is a no-op.
    pub fn write(&self, handle: &ControllerHandle, bytes: Vec<u8>) -> Option<InputOutcome> {
        self.submit(handle, bytes)?.recv().ok()
    }

    /// Queue `bytes` (waiting only for queue space) and return where the outcome
    /// will arrive, so the caller can watch other events while it waits. `None`
    /// for empty input. A closed receiver means the writer stopped.
    pub fn submit(
        &self,
        handle: &ControllerHandle,
        bytes: Vec<u8>,
    ) -> Option<std::sync::mpsc::Receiver<InputOutcome>> {
        let request = self.request(handle, bytes)?;
        let (reply, rx) = sync_channel(1);
        self.push_input(InputItem::Write {
            request,
            reply: Some(reply),
        });
        Some(rx)
    }

    /// Queue `bytes` without waiting for the outcome (waits only for queue space).
    pub fn write_nowait(&self, handle: &ControllerHandle, bytes: Vec<u8>) {
        if let Some(request) = self.request(handle, bytes) {
            self.push_input(InputItem::Write {
                request,
                reply: None,
            });
        }
    }

    /// Like [`InputWriter::write_nowait`], but give up waiting for queue space
    /// as soon as `abandon` returns `true` (checked at least every 50 ms), so a
    /// sender whose client has gone is not stuck behind a full queue. Returns
    /// `false` if abandoned; nothing was queued then.
    pub fn write_nowait_unless(
        &self,
        handle: &ControllerHandle,
        bytes: Vec<u8>,
        abandon: impl Fn() -> bool,
    ) -> bool {
        let Some(request) = self.request(handle, bytes) else {
            return true;
        };
        let mut queues = self.shared.lock();
        while queues.inputs.len() >= INPUT_CAPACITY {
            if abandon() {
                return false;
            }
            queues = self
                .shared
                .space
                .wait_timeout(queues, Duration::from_millis(50))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        queues.inputs.push_back(InputItem::Write {
            request,
            reply: None,
        });
        drop(queues);
        self.shared.work.notify_one();
        true
    }

    /// Release `handle` once every input it queued before this call is done.
    /// A handle that no longer authorizes releases nothing. Consumes the
    /// handle: its holder can queue nothing after its end marker.
    pub fn end_of_input(&self, handle: ControllerHandle) {
        self.push_input(InputItem::End { handle, done: None });
    }

    /// The controller behind `handle` disconnected: release it now, ahead of any
    /// queued input, and cancel everything it still has queued or in flight.
    /// Unlike [`InputWriter::end_of_input`], nothing it queued is written after
    /// this is served, so a full PTY cannot keep a vanished client in control.
    /// Consumes the handle, so a holder disconnects at most once.
    pub fn disconnect(&self, handle: ControllerHandle) {
        self.push_control(Control::Disconnect { handle });
    }

    /// Like [`InputWriter::end_of_input`], but wait until the end marker is
    /// served: every input queued before it is done, and the handle released
    /// if it still held control.
    pub fn end_of_input_and_wait(&self, handle: ControllerHandle) {
        let (done, rx) = sync_channel(1);
        self.push_input(InputItem::End {
            handle,
            done: Some(done),
        });
        let _ = rx.recv();
    }

    fn request(&self, handle: &ControllerHandle, bytes: Vec<u8>) -> Option<PendingInput> {
        let id = RequestId::new(self.shared.next_request.fetch_add(1, Ordering::Relaxed));
        PendingInput::new(id, handle, bytes).ok()
    }

    fn push_control(&self, control: Control) {
        let mut queues = self.shared.lock();
        while queues.controls.len() >= CONTROL_CAPACITY {
            queues = self
                .shared
                .space
                .wait(queues)
                .unwrap_or_else(|e| e.into_inner());
        }
        queues.controls.push_back(control);
        drop(queues);
        self.shared.work.notify_one();
    }

    fn push_input(&self, item: InputItem) {
        let mut queues = self.shared.lock();
        while queues.inputs.len() >= INPUT_CAPACITY {
            queues = self
                .shared
                .space
                .wait(queues)
                .unwrap_or_else(|e| e.into_inner());
        }
        queues.inputs.push_back(item);
        drop(queues);
        self.shared.work.notify_one();
    }
}

/// The request being written, with where to report its outcome.
struct Current {
    request: PendingInput,
    reply: Option<SyncSender<InputOutcome>>,
}

fn writer_loop<P: WritablePty, H: ControlHooks>(
    shared: &Shared,
    mut arbiter: InputArbiter,
    mut pty: P,
    mut hooks: H,
) {
    let mut current: Option<Current> = None;
    let mut repaint = RepaintState::default();

    loop {
        // Take all controls, and the next input if nothing is being written.
        let (controls, next_input) = {
            let mut queues = shared.lock();
            while queues.controls.is_empty() && queues.inputs.is_empty() && current.is_none() {
                queues = shared.work.wait(queues).unwrap_or_else(|e| e.into_inner());
            }
            let controls: Vec<Control> = queues.controls.drain(..).collect();
            let next_input = if current.is_none() {
                queues.inputs.pop_front()
            } else {
                None
            };
            (controls, next_input)
        };
        shared.space.notify_all();

        // Stage the dequeued item first, so a disconnect served below can cancel
        // it along with everything else its holder queued.
        let mut next_end = None;
        match next_input {
            Some(InputItem::Write { request, reply }) => current = Some(Current { request, reply }),
            Some(end @ InputItem::End { .. }) => next_end = Some(end),
            None => {}
        }

        for control in controls {
            if let Control::Disconnect { handle } = control {
                disconnect(
                    handle,
                    shared,
                    &mut arbiter,
                    &mut pty,
                    &mut hooks,
                    &mut current,
                    &mut next_end,
                );
            } else {
                apply_control(control, &mut arbiter, &pty, &mut hooks, &mut repaint);
            }
        }

        if let Some(InputItem::End { handle, done }) = next_end {
            release(&mut arbiter, handle, &mut hooks);
            if let Some(done) = done {
                let _ = done.send(());
            }
            continue;
        }

        let Some(mut writing) = current.take() else {
            continue;
        };
        let before = writing.request.accepted();
        match arbiter.write_step(&mut writing.request, &mut pty) {
            WriteStep::Done(outcome) => {
                if let InputOutcome::Incomplete {
                    accepted,
                    total,
                    reason: IncompleteReason::NotAuthorized(_),
                    ..
                } = outcome
                {
                    hooks.input_revoked(writing.request.kind(), accepted, total);
                }
                if let Some(reply) = writing.reply {
                    let _ = reply.send(outcome);
                }
            }
            WriteStep::Pending => {
                if writing.request.accepted() == before {
                    // No progress: the PTY input buffer is full. Wait briefly for
                    // room, then loop to serve any control that arrived.
                    let _ = pty.wait_writable(WRITABLE_POLL);
                }
                current = Some(writing);
            }
        }
    }
}

/// Serve a disconnect ahead of all input: release `handle` if it is current,
/// then cancel every input its holder still has staged, queued, or in flight.
///
/// Each cancelled write is finished through [`InputArbiter::write_step`], which
/// cannot reach the PTY once the handle no longer authorizes, so callers waiting
/// on an outcome receive `NotAuthorized` with the exact accepted count. The
/// holder's revoked bytes are reported once, in aggregate.
#[allow(clippy::too_many_arguments)]
fn disconnect<P: WritablePty, H: ControlHooks>(
    handle: ControllerHandle,
    shared: &Shared,
    arbiter: &mut InputArbiter,
    pty: &mut P,
    hooks: &mut H,
    current: &mut Option<Current>,
    next_end: &mut Option<InputItem>,
) {
    let (holder, kind) = (handle.holder(), handle.kind());
    release(arbiter, handle, hooks);

    let belongs = |item: &InputItem| match item {
        InputItem::Write { request, .. } => request.key().holder() == holder,
        InputItem::End { handle, .. } => handle.holder() == holder,
    };
    let mut cancelled: Vec<InputItem> = {
        let mut queues = shared.lock();
        let (theirs, others): (VecDeque<_>, VecDeque<_>) =
            queues.inputs.drain(..).partition(|item| belongs(item));
        queues.inputs = others;
        theirs.into_iter().collect()
    };
    shared.space.notify_all();
    if next_end.as_ref().is_some_and(belongs) {
        cancelled.extend(next_end.take());
    }
    if current
        .as_ref()
        .is_some_and(|c| c.request.key().holder() == holder)
    {
        if let Some(Current { request, reply }) = current.take() {
            cancelled.push(InputItem::Write { request, reply });
        }
    }

    let (mut accepted_total, mut bytes_total) = (0, 0);
    for item in cancelled {
        match item {
            InputItem::Write { mut request, reply } => {
                if let WriteStep::Done(outcome) = arbiter.write_step(&mut request, pty) {
                    if let InputOutcome::Incomplete {
                        accepted, total, ..
                    } = outcome
                    {
                        accepted_total += accepted;
                        bytes_total += total;
                    }
                    if let Some(reply) = reply {
                        let _ = reply.send(outcome);
                    }
                }
            }
            InputItem::End { done, .. } => {
                // Cancelled by a disconnect, which has already released.
                if let Some(done) = done {
                    let _ = done.send(());
                }
            }
        }
    }
    if bytes_total > 0 {
        hooks.input_revoked(kind, accepted_total, bytes_total);
    }
}

/// The writer's view of the repaint nudge.
#[derive(Default)]
struct RepaintState {
    /// The controller whose last resize left the size unchanged, until it
    /// repaints.
    due: Option<HandleKey>,
    /// The size a shrink took away, and its correlation, until restored or
    /// overtaken by a resize.
    pending: Option<(Geometry, NonZeroU32)>,
}

fn apply_control<P: WritablePty, H: ControlHooks>(
    control: Control,
    arbiter: &mut InputArbiter,
    pty: &P,
    hooks: &mut H,
    repaint: &mut RepaintState,
) {
    match control {
        Control::Claim {
            holder,
            kind,
            reply,
        } => {
            let result = arbiter.claim(holder, kind);
            if let Ok(handle) = &result {
                if handle.kind() == ControllerKind::Human {
                    hooks.human_control(Trigger::Attach, handle.epoch());
                }
            }
            let _ = reply.send(result);
        }
        Control::Takeover { holder, reply } => {
            let result = arbiter.takeover(holder);
            if let Ok(takeover) = &result {
                hooks.human_control(Trigger::Takeover, takeover.handle.epoch());
                if let Some((retired, kind)) = takeover.retired {
                    hooks.retire(retired, kind, takeover.handle.epoch());
                }
            }
            let _ = reply.send(result);
        }
        Control::Resize { key, size } => {
            if arbiter.authorize(key).is_ok() {
                let unchanged = pty.size() == Some(size);
                pty.resize(size, ResizeCause::User);
                repaint.due = unchanged.then_some(key);
                repaint.pending = None;
            }
        }
        Control::RepaintShrink {
            key,
            correlation,
            reply,
        } => {
            let mut shrunk = false;
            if arbiter.authorize(key).is_ok() && repaint.due == Some(key) {
                repaint.due = None;
                if let Some(base) = pty.size() {
                    if let Some(smaller) = Geometry::new(base.rows() - 1, base.cols()) {
                        pty.resize(smaller, ResizeCause::Repaint { correlation });
                        repaint.pending = Some((base, correlation));
                        shrunk = true;
                    }
                }
            }
            let _ = reply.send(shrunk);
        }
        Control::RepaintRestore { correlation } => {
            if let Some((base, pending)) = repaint.pending {
                if pending == correlation {
                    repaint.pending = None;
                    pty.resize(base, ResizeCause::Repaint { correlation });
                }
            }
        }
        Control::Disconnect { .. } => {
            debug_assert!(
                false,
                "disconnect needs the queues; the writer loop serves it"
            );
        }
    }
}

/// End of a holder's input: release if it still authorizes. The handle is
/// consumed, so its holder can queue nothing after this.
fn release<H: ControlHooks>(arbiter: &mut InputArbiter, handle: ControllerHandle, hooks: &mut H) {
    if arbiter.release(&handle).is_ok() && handle.kind() == ControllerKind::Human {
        hooks.human_released(handle.epoch());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// A PTY whose input buffer is permanently full: the child never reads.
    struct FullPty;

    impl PtyInputSink for FullPty {
        fn try_write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::WouldBlock.into())
        }
    }

    impl WritablePty for FullPty {
        fn wait_writable(&self, timeout: Duration) -> io::Result<bool> {
            std::thread::sleep(timeout.min(Duration::from_millis(1)));
            Ok(false)
        }

        fn resize(&self, _size: Geometry, _cause: ResizeCause) {}
    }

    /// A PTY that accepts one byte per attempt and records everything written.
    #[derive(Clone, Default)]
    struct SlowPty(Arc<Mutex<Vec<u8>>>);

    impl PtyInputSink for SlowPty {
        fn try_write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().push(bytes[0]);
            Ok(1)
        }
    }

    impl WritablePty for SlowPty {
        fn wait_writable(&self, _timeout: Duration) -> io::Result<bool> {
            Ok(true)
        }

        fn resize(&self, _size: Geometry, _cause: ResizeCause) {}
    }

    /// Records hook calls in order.
    #[derive(Clone, Default)]
    struct Recorded(Arc<Mutex<Vec<String>>>);

    impl Recorded {
        fn calls(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    impl ControlHooks for Recorded {
        fn human_control(&mut self, trigger: Trigger, _epoch: ControllerEpoch) {
            self.0
                .lock()
                .unwrap()
                .push(format!("human_control:{trigger:?}"));
        }

        fn human_released(&mut self, _epoch: ControllerEpoch) {
            self.0.lock().unwrap().push("human_released".to_owned());
        }

        fn retire(&mut self, holder: HolderId, kind: ControllerKind, epoch: ControllerEpoch) {
            self.0
                .lock()
                .unwrap()
                .push(format!("retire:{}:{kind:?}:{epoch}", holder.get()));
        }

        fn input_revoked(&mut self, kind: ControllerKind, accepted: usize, total: usize) {
            self.0
                .lock()
                .unwrap()
                .push(format!("input_revoked:{kind:?}:{accepted}:{total}"));
        }
    }

    #[test]
    fn disconnect_releases_ahead_of_input_stuck_behind_a_full_pty() {
        let hooks = Recorded::default();
        let writer = InputWriter::spawn(RunId::new(), FullPty, hooks.clone());
        let human = writer
            .claim(writer.next_holder(), ControllerKind::Human)
            .unwrap();
        for _ in 0..4 {
            writer.write_nowait(&human, vec![b'x'; 1024]);
        }
        let stuck = writer
            .submit(&human, b"never-written".to_vec())
            .expect("non-empty input");
        let human_epoch = human.epoch();

        writer.disconnect(human);

        // Control is free again even though the PTY never accepts input.
        let deadline = Instant::now() + Duration::from_secs(5);
        let next = loop {
            match writer.claim(writer.next_holder(), ControllerKind::Human) {
                Ok(handle) => break handle,
                Err(e) => assert!(
                    Instant::now() < deadline,
                    "a disconnected client still holds control: {e}"
                ),
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(next.epoch() > human_epoch);

        // The disconnected client's in-flight write is cancelled, not written.
        match stuck.recv().ok() {
            Some(InputOutcome::Incomplete {
                accepted: 0,
                reason: IncompleteReason::NotAuthorized(_),
                ..
            }) => {}
            other => panic!("the stuck write must be revoked, got {other:?}"),
        }
        assert!(hooks.calls().contains(&"human_released".to_owned()));
    }

    #[test]
    fn end_of_input_still_drains_before_release() {
        let pty = SlowPty::default();
        let writer = InputWriter::spawn(RunId::new(), pty.clone(), Recorded::default());
        let agent = writer
            .claim(writer.next_holder(), ControllerKind::Agent)
            .unwrap();
        writer.write_nowait(&agent, b"first".to_vec());
        writer.write_nowait(&agent, b"second".to_vec());

        writer.end_of_input_and_wait(agent);

        assert_eq!(pty.0.lock().unwrap().as_slice(), b"firstsecond");
        assert!(
            writer
                .claim(writer.next_holder(), ControllerKind::Agent)
                .is_ok(),
            "released once its input was written"
        );
    }

    /// Records applied window sizes and their causes; starts at 24×80 and
    /// accepts all input.
    #[derive(Clone, Default)]
    struct SizedPty(Arc<Mutex<Vec<(Geometry, ResizeCause)>>>);

    impl SizedPty {
        fn sizes(&self) -> Vec<(Geometry, ResizeCause)> {
            self.0.lock().unwrap().clone()
        }
    }

    impl PtyInputSink for SizedPty {
        fn try_write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }
    }

    impl WritablePty for SizedPty {
        fn wait_writable(&self, _timeout: Duration) -> io::Result<bool> {
            Ok(true)
        }

        fn resize(&self, size: Geometry, cause: ResizeCause) {
            self.0.lock().unwrap().push((size, cause));
        }

        fn size(&self) -> Option<Geometry> {
            Some(
                self.0
                    .lock()
                    .unwrap()
                    .last()
                    .map_or(Geometry::new(24, 80).unwrap(), |(size, _)| *size),
            )
        }
    }

    fn geometry(rows: u16, cols: u16) -> Geometry {
        Geometry::new(rows, cols).unwrap()
    }

    fn correlation(n: u32) -> std::num::NonZeroU32 {
        std::num::NonZeroU32::new(n).unwrap()
    }

    /// Controls are served in order: a claim round-trip proves everything
    /// queued before it was processed.
    fn served(writer: &InputWriter) {
        let _ = writer.claim(writer.next_holder(), ControllerKind::Agent);
    }

    #[test]
    fn only_the_current_controller_can_resize() {
        let pty = SizedPty::default();
        let writer = InputWriter::spawn(RunId::new(), pty.clone(), Recorded::default());
        let old = writer
            .claim(writer.next_holder(), ControllerKind::Human)
            .unwrap();
        let new = writer.takeover(writer.next_holder()).unwrap().handle;

        writer.resize(&old, geometry(10, 20));
        writer.resize(&new, geometry(30, 40));
        served(&writer);

        assert_eq!(pty.sizes(), [(geometry(30, 40), ResizeCause::User)]);
    }

    #[test]
    fn a_repaint_is_authorized_like_input() {
        let pty = SizedPty::default();
        let writer = InputWriter::spawn(RunId::new(), pty.clone(), Recorded::default());
        let old = writer
            .claim(writer.next_holder(), ControllerKind::Human)
            .unwrap();
        writer.resize(&old, geometry(24, 80));
        let new = writer.takeover(writer.next_holder()).unwrap().handle;

        assert!(
            !writer.repaint_shrink(&old, correlation(1)),
            "a superseded client cannot shrink the PTY"
        );
        writer.resize(&new, geometry(24, 80));
        assert!(
            writer.repaint_shrink(&new, correlation(2)),
            "an attach at the PTY's size repaints"
        );
        writer.repaint_restore(correlation(1));
        writer.repaint_restore(correlation(2));
        served(&writer);

        let repaint = ResizeCause::Repaint {
            correlation: correlation(2),
        };
        assert_eq!(
            pty.sizes(),
            [
                (geometry(24, 80), ResizeCause::User),
                (geometry(24, 80), ResizeCause::User),
                (geometry(23, 80), repaint),
                (geometry(24, 80), repaint),
            ]
        );
    }

    #[test]
    fn only_a_resize_that_kept_the_size_is_followed_by_a_repaint_once() {
        let pty = SizedPty::default();
        let writer = InputWriter::spawn(RunId::new(), pty.clone(), Recorded::default());
        let human = writer
            .claim(writer.next_holder(), ControllerKind::Human)
            .unwrap();

        writer.resize(&human, geometry(30, 100));
        assert!(
            !writer.repaint_shrink(&human, correlation(1)),
            "a new size already redraws"
        );
        writer.resize(&human, geometry(30, 100));
        assert!(writer.repaint_shrink(&human, correlation(2)));
        assert!(
            !writer.repaint_shrink(&human, correlation(3)),
            "one repaint per resize that kept the size"
        );
    }

    #[test]
    fn a_one_row_terminal_is_not_shrunk() {
        let pty = SizedPty::default();
        let writer = InputWriter::spawn(RunId::new(), pty.clone(), Recorded::default());
        let human = writer
            .claim(writer.next_holder(), ControllerKind::Human)
            .unwrap();
        writer.resize(&human, geometry(1, 80));
        writer.resize(&human, geometry(1, 80));
        assert!(!writer.repaint_shrink(&human, correlation(1)));
        assert_eq!(pty.sizes().len(), 2);
    }

    #[test]
    fn a_client_that_leaves_mid_repaint_still_gets_its_size_restored() {
        let pty = SizedPty::default();
        let writer = InputWriter::spawn(RunId::new(), pty.clone(), Recorded::default());
        let human = writer
            .claim(writer.next_holder(), ControllerKind::Human)
            .unwrap();
        writer.resize(&human, geometry(24, 80));
        assert!(writer.repaint_shrink(&human, correlation(1)));
        writer.disconnect(human);
        writer.repaint_restore(correlation(1));
        served(&writer);

        assert_eq!(
            pty.sizes().last(),
            Some(&(
                geometry(24, 80),
                ResizeCause::Repaint {
                    correlation: correlation(1)
                }
            )),
            "the shrink does not outlive the client"
        );
    }

    #[test]
    fn a_resize_between_shrink_and_restore_cancels_the_restore() {
        let pty = SizedPty::default();
        let writer = InputWriter::spawn(RunId::new(), pty.clone(), Recorded::default());
        let human = writer
            .claim(writer.next_holder(), ControllerKind::Human)
            .unwrap();
        writer.resize(&human, geometry(24, 80));
        assert!(writer.repaint_shrink(&human, correlation(1)));
        writer.resize(&human, geometry(40, 120));
        writer.repaint_restore(correlation(1));
        served(&writer);

        assert_eq!(
            pty.sizes().last(),
            Some(&(geometry(40, 120), ResizeCause::User)),
            "the user's size stands"
        );
    }

    #[test]
    fn a_superseded_holders_end_of_input_releases_nothing() {
        let hooks = Recorded::default();
        let writer = InputWriter::spawn(RunId::new(), SlowPty::default(), hooks.clone());
        let agent = writer
            .claim(writer.next_holder(), ControllerKind::Agent)
            .unwrap();
        let human = writer.takeover(writer.next_holder()).unwrap().handle;

        writer.end_of_input_and_wait(agent);

        assert!(
            matches!(
                writer.claim(writer.next_holder(), ControllerKind::Agent),
                Err(RequestError::Control(ControlError::Busy {
                    kind: ControllerKind::Human,
                    ..
                }))
            ),
            "the human who took over still holds control"
        );
        assert!(!hooks.calls().contains(&"human_released".to_owned()));
        writer.disconnect(human);
    }

    /// The kind a revocation is reported under is the one the handle was
    /// granted with, whichever way its input ends.
    #[test]
    fn revoked_input_is_reported_under_its_holders_kind() {
        let hooks = Recorded::default();
        let writer = InputWriter::spawn(RunId::new(), FullPty, hooks.clone());
        let human = writer
            .claim(writer.next_holder(), ControllerKind::Human)
            .unwrap();
        writer.write_nowait(&human, b"abc".to_vec());
        writer.disconnect(human);

        let agent = writer
            .claim(writer.next_holder(), ControllerKind::Agent)
            .unwrap();
        let queued = writer.submit(&agent, b"de".to_vec()).unwrap();
        writer.takeover(writer.next_holder()).unwrap();
        let _ = queued.recv();

        let revoked: Vec<String> = hooks
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("input_revoked"))
            .collect();
        assert_eq!(
            revoked,
            ["input_revoked:Human:0:3", "input_revoked:Agent:0:2"]
        );
    }
}
