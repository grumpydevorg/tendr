//! The lifecycle guard: from the moment the child is spawned until a terminal
//! state is recorded, a [`SupervisedRun`] owns it.
//!
//! The invariant: after spawn, either the sidecar is alive and owns the child,
//! or meta is terminal and the child is dead, or the sidecar died by a true
//! crash (SIGKILL, OOM, abort) and reconciliation closes the gap. The guard
//! makes the second case the only way out of the first:
//!
//! - [`SupervisedRun<Spawned>::publish_running`] is the only way to `Running`,
//!   so the resources `Running` promises (stdin transport; for a PTY session,
//!   its recorder, input writer and attach listener) are in place before it is
//!   published. A PTY session's attach socket is bound even earlier, before the
//!   child is spawned, and the guard owns it from adoption.
//! - [`SupervisedRun<Running>::finish`] is the only normal exit.
//! - Any other exit (an error, an early return, a panic unwind) kills the child
//!   through the platform kill path and records
//!   `Exited { reason: SidecarFailed { step } }`, then runs the hooks.
//!
//! Typestate stops at the process boundary: persisted meta stays
//! runtime-validated (`transition.rs`), because a type parameter does not
//! survive serialization.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::marker::PhantomData;
use std::num::NonZeroI32;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{
    AttachEndpoint, AttachSink, AttachSocketCleanup, LifecycleEvents, META_WRITE, PtyInput,
    PtyRecordingRun, PtyTeardown, ReadyWriter, capture_stream, capture_stream_with_tee,
    collect_warnings, deliver_ready, finished_recording, lock, run_on_exit_hooks,
    setup_kill_watcher, setup_pty_stdin_forwarding, setup_stdin_forwarding, setup_timeout,
    stopped_state, test_abort_point, test_fault, test_unlock_gate,
};
#[cfg(unix)]
use super::{
    ConnectionRegistry, RunEffects, SidecarControlHooks, UnixPtyInput, recorder_limits,
    run_attach_listener, start_pty_recording,
};
use crate::model::ids::{EpochTimestamp, ProcessIdentity};
use crate::model::meta::Meta;
use crate::model::pty::{PtyControl, PtyMeta, PtyRecording, RecordingState};
use crate::model::spec::StdinMode;
use crate::model::state::{ExitReason, SidecarStep};
use crate::platform::{Current, Platform, ProcessStatus};
use crate::session::{self, LockGuard, SessionDir};

type SupervisedChild = <Current as Platform>::SupervisedChild;
type ChildKillHandle = <Current as Platform>::ChildKillHandle;

/// The child is spawned; `Running` is not yet published.
pub(super) struct Spawned;
/// `Running` is published; supervision is under way.
pub(super) struct Running;

/// Owner of a spawned child. See the module docs for the contract.
#[must_use = "dropping a SupervisedRun kills the child and records SidecarFailed"]
pub(super) struct SupervisedRun<Phase> {
    core: RunCore,
    _phase: PhantomData<Phase>,
}

/// The guard has already killed the child and recorded the failure; the
/// sidecar only has to exit.
#[derive(Debug)]
pub(super) struct SidecarFailure {
    step: SidecarStep,
}

impl std::fmt::Display for SidecarFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "sidecar failed at {}; the child was stopped and the failure recorded",
            self.step
        )
    }
}

impl std::error::Error for SidecarFailure {}

/// Everything the guard owns. `Drop` is here, not on [`SupervisedRun`], so a
/// phase change can move the core into a new wrapper without running it.
struct RunCore {
    child: SupervisedChild,
    kill_handle: ChildKillHandle,
    identity: ProcessIdentity,
    meta: Meta,
    lifecycle: LifecycleEvents,
    session: SessionDir,
    /// The `start` client's readiness channel while it is still unsent.
    ready: Option<ReadyWriter>,
    stdin_errors: Arc<Mutex<Vec<String>>>,
    /// A PTY session's attach side; `None` for a pipe session, decided at
    /// adoption. Dropped, removing the socket and its breadcrumb, before the
    /// lock is released.
    pty: Option<PtySide>,
    /// Stops the timeout and kill-request watchers once the run is ending.
    watch_cancel: Arc<AtomicBool>,
    timed_out: Arc<AtomicBool>,
    /// What the sidecar is doing now; recorded if the guard is dropped.
    step: SidecarStep,
    /// Set once the run has an outcome; `Drop` is then a no-op.
    ended: bool,
    /// Released after the terminal record is written, before the hooks run.
    lock: Option<LockGuard>,
}

/// A PTY run's attach side, in two phases, so what exists together is
/// constructed together.
// Uninhabited off Unix, where AttachEndpoint and PtyTeardown are, so Started
// is never built there.
#[cfg_attr(not(unix), allow(dead_code))]
enum PtySide {
    /// The attach socket is bound and published; nothing is running yet.
    Bound(AttachEndpoint),
    /// The effects thread, recorder, input writer and listener are running.
    Started(StartedPty),
}

#[cfg_attr(not(unix), allow(dead_code))]
struct StartedPty {
    /// Held for its `Drop`, which removes the socket and its breadcrumb.
    _socket: AttachSocketCleanup,
    /// Tee for PTY output to an attached client.
    sink: AttachSink,
    /// The exact recording of output and applied geometry, until it is
    /// finished into meta (before the teardown).
    recording: Option<PtyRecordingRun>,
    /// The input owner, set by the effects thread once the change is logged.
    /// `meta` never tracks it, so every write folds it in.
    control: Arc<Mutex<PtyControl>>,
    /// Ends the attach side with the run.
    teardown: PtyTeardown,
}

/// End a started attach side before the terminal record: shut its
/// connections, record its pending effects, and fold the final owner into
/// `meta`. Dropping it then removes the socket.
#[cfg(unix)]
fn end_attach_side(pty: StartedPty, meta: &mut Meta) {
    pty.teardown.run();
    // The effects thread has stopped: nothing changes the owner any more.
    meta.set_pty_control(lock(&pty.control).clone());
}

#[cfg(not(unix))]
fn end_attach_side(pty: StartedPty, _meta: &mut Meta) {
    match pty.teardown {}
}

impl SupervisedRun<Spawned> {
    /// Take ownership of a freshly spawned child. `ready` is the readiness
    /// channel if it has not been used yet (no `--after` dependencies);
    /// `attach` is a PTY session's attach socket, bound before spawn.
    pub(super) fn adopt(
        child: SupervisedChild,
        session: SessionDir,
        lock: LockGuard,
        meta: Meta,
        lifecycle: LifecycleEvents,
        ready: Option<ReadyWriter>,
        attach: Option<AttachEndpoint>,
    ) -> Self {
        let identity = Current::child_identity(&child);
        let kill_handle = Current::child_kill_handle(&child);
        Self {
            core: RunCore {
                child,
                kill_handle,
                identity,
                meta,
                lifecycle,
                session,
                ready,
                stdin_errors: Arc::new(Mutex::new(Vec::new())),
                pty: attach.map(PtySide::Bound),
                watch_cancel: Arc::new(AtomicBool::new(false)),
                timed_out: Arc::new(AtomicBool::new(false)),
                step: SidecarStep::Breadcrumb,
                ended: false,
                lock: Some(lock),
            },
            _phase: PhantomData,
        }
    }

    /// Set up what `Running` promises, publish it, deliver readiness, and
    /// start the timeout and kill watchers. The only way to `Running`.
    ///
    /// # Errors
    /// [`SidecarFailure`] when a step that ends the run failed (stdin
    /// transport, persisting `Running`). The child is already stopped and the
    /// failure recorded.
    pub(super) fn publish_running(mut self) -> Result<SupervisedRun<Running>, SidecarFailure> {
        self.core.publish_running()?;
        Ok(SupervisedRun {
            core: self.core,
            _phase: PhantomData,
        })
    }
}

impl SupervisedRun<Running> {
    /// Capture output until the child closes it, then observe its exit.
    ///
    /// # Errors
    /// [`SidecarFailure`] when the exit cannot be observed. The child is
    /// already stopped and the failure recorded.
    pub(super) fn supervise(&mut self) -> Result<ExitReason, SidecarFailure> {
        self.core.supervise()
    }

    /// The only normal exit: classify, record the terminal state, release the
    /// lock and run the hooks.
    ///
    /// # Errors
    /// When the terminal meta could not be written. The child has exited,
    /// the durable terminal event lets reconciliation heal meta, a copy is
    /// salvaged to lost+found, and the hooks still run.
    pub(super) fn finish(self, how: ExitReason) -> anyhow::Result<()> {
        let mut core = self.core;
        core.finish(how)
    }
}

impl RunCore {
    fn session_dir(&self) -> &Path {
        self.session.path()
    }

    /// End the run at `step`: stop the child, record `SidecarFailed`, and
    /// return the marker for the caller to propagate.
    fn fail(&mut self, step: SidecarStep, error: impl std::fmt::Display) -> SidecarFailure {
        self.end_failed(step, &error.to_string());
        SidecarFailure { step }
    }

    fn publish_running(&mut self) -> Result<(), SidecarFailure> {
        let dir = self.session_dir().to_path_buf();

        // Recover: the breadcrumb only matters to crash recovery.
        self.step = SidecarStep::Breadcrumb;
        let breadcrumb = serde_json::to_string(&self.identity)
            .map_err(io::Error::other)
            .and_then(|json| {
                test_fault(SidecarStep::Breadcrumb.as_str())?;
                std::fs::write(dir.join("child_pid"), json)
            });
        if let Err(e) = breadcrumb {
            self.meta
                .add_warning(format!("orphan breadcrumb not written: {e}"));
        }
        test_abort_point("after_spawn");

        // End: a PTY session never runs without its listener, and the listener
        // reaches the PTY only through the input writer.
        // A failed start drops the endpoint, removing the socket.
        let pty_input = match self.pty.take() {
            Some(PtySide::Bound(endpoint)) => {
                self.step = SidecarStep::AttachBind;
                match self.start_pty(endpoint) {
                    Ok((started, input)) => {
                        self.pty = Some(PtySide::Started(started));
                        Some(input)
                    }
                    Err(e) => return Err(self.fail(SidecarStep::AttachBind, e)),
                }
            }
            // A pipe session.
            None => None,
            // Only SupervisedRun<Spawned>::publish_running reaches here, and it
            // consumes the run, so the side is started at most once. A panic
            // is recorded by the guard as SidecarFailed.
            Some(PtySide::Started(_)) => {
                unreachable!("a PTY attach side is started only once, when Running is published")
            }
        };

        // End: a --stdin run whose input can never arrive is a failed run.
        if self.meta.launch_spec().stdin_mode == StdinMode::Pipe {
            self.step = SidecarStep::StdinTransport;
            let transport =
                test_fault(SidecarStep::StdinTransport.as_str()).and_then(|()| match &pty_input {
                    Some(input) => setup_pty_stdin_forwarding(&dir, input, &self.stdin_errors),
                    None => match Current::child_stdin(&mut self.child) {
                        Some(writer) => setup_stdin_forwarding(&dir, writer, &self.stdin_errors),
                        None => Err(io::Error::other("child stdin not piped")),
                    },
                });
            if let Err(e) = transport {
                return Err(self.fail(SidecarStep::StdinTransport, e));
            }
        }

        // End: an unobservable Running would be a lie.
        self.step = SidecarStep::RunningMeta;
        if let Err(e) = self.meta.transition_running(self.identity) {
            return Err(self.fail(SidecarStep::RunningMeta, e));
        }
        if self.pty.is_some() {
            self.meta.set_pty(PtyMeta::new());
        }
        self.lifecycle.emit(&mut self.meta, false);
        let published = test_fault(SidecarStep::RunningMeta.as_str())
            .map_err(|e| e.to_string())
            .and_then(|()| self.write_meta().map_err(|e| e.to_string()));
        if let Err(e) = published {
            return Err(self.fail(SidecarStep::RunningMeta, e));
        }

        // Recover: readiness is a courtesy to the client (#71).
        self.step = SidecarStep::Readiness;
        self.signal_readiness();
        test_abort_point("after_running");

        if let Some(timeout_s) = self.meta.launch_spec().timeout_s {
            self.timed_out = setup_timeout(
                self.kill_handle.clone(),
                timeout_s,
                Arc::clone(&self.watch_cancel),
            );
        }
        setup_kill_watcher(
            &dir,
            self.kill_handle.clone(),
            self.meta.run_id(),
            Arc::clone(&self.watch_cancel),
        );
        Ok(())
    }

    /// Start a PTY session's effects thread, its recorder (whose stop report
    /// goes through that thread), its single input writer (which owns the
    /// PTY's write half and records applied sizes) and the attach listener on
    /// the socket bound before spawn. Every fallible step comes first, so a
    /// failure leaves nothing running.
    #[cfg(unix)]
    fn start_pty(&mut self, endpoint: AttachEndpoint) -> io::Result<(StartedPty, PtyInput)> {
        test_fault(SidecarStep::AttachBind.as_str())?;
        // Dup the resize fd before the write half is taken.
        let resize = Current::pty_resize_fd(&self.child);
        let writer = self
            .child
            .take_pty_writer()
            .ok_or_else(|| io::Error::other("PTY write half unavailable"))?;

        let AttachEndpoint { socket, cleanup } = endpoint;
        let sink: AttachSink = Arc::new(Mutex::new(None));
        let registry = ConnectionRegistry::default();
        let control = Arc::new(Mutex::new(PtyControl::AgentControl));
        let effects = RunEffects::spawn(
            self.session_dir().to_path_buf(),
            self.lifecycle.with_fresh_writer(),
            Arc::clone(&control),
        );
        let recording = start_pty_recording(
            self.session.path(),
            self.meta.run_id(),
            &self.meta.launch_spec().env,
            recorder_limits(),
            effects.sender(),
        );
        let hooks = SidecarControlHooks {
            effects: effects.sender(),
            registry: registry.clone(),
            attach_sink: Arc::clone(&sink),
        };
        let input = PtyInput {
            writer: crate::pty_input::InputWriter::spawn(
                self.meta.run_id(),
                UnixPtyInput {
                    writer,
                    resize,
                    recorder: Some(recording.thread.recorder()),
                },
                hooks,
            ),
        };
        {
            let (writer, registry, sink) =
                (input.writer.clone(), registry.clone(), Arc::clone(&sink));
            std::thread::spawn(move || {
                run_attach_listener(&socket.listener, &writer, &registry, &sink);
            });
        }
        let started = StartedPty {
            _socket: cleanup,
            sink,
            recording: Some(recording),
            control,
            teardown: PtyTeardown { registry, effects },
        };
        Ok((started, input))
    }

    /// PTY sessions are Unix-only; the PTY spawn itself fails elsewhere, so no
    /// endpoint exists to start.
    #[cfg(not(unix))]
    fn start_pty(&mut self, endpoint: AttachEndpoint) -> io::Result<(StartedPty, PtyInput)> {
        match endpoint {}
    }

    /// Persist meta, serialized with the sidecar's other `meta.json` writers
    /// ([`META_WRITE`]: control flips and recording stops patch it from other
    /// threads) and with the live control owner and the recording's current
    /// state folded in, so this whole-meta write never reverts either. A
    /// recording that already stopped has reported, or will report under this
    /// lock after this write: either way `meta.json` ends up `Stopped`.
    fn write_meta(&mut self) -> Result<(), session::SessionError> {
        let _serialized = lock(&META_WRITE);
        if let Some(PtySide::Started(pty)) = &self.pty {
            self.meta.set_pty_control(lock(&pty.control).clone());
            if let Some(run) = &pty.recording {
                let state = run
                    .thread
                    .recorder()
                    .stopped()
                    .map_or(RecordingState::Recording, |stopped| {
                        stopped_state(stopped, None)
                    });
                self.meta.set_pty_recording(run.meta(state));
            }
        }
        session::write_meta_atomic(&self.session, &self.meta)
    }

    /// Deliver readiness if it is still owed, and persist a lost delivery as a
    /// session warning (see [`super::signal_readiness`]), through
    /// [`Self::write_meta`].
    fn signal_readiness(&mut self) {
        if deliver_ready(&mut self.ready, &self.meta).record(&mut self.meta) {
            let _ = test_fault("ready_rewrite")
                .map_err(|e| e.to_string())
                .and_then(|()| self.write_meta().map_err(|e| e.to_string()));
        }
    }

    /// Close a PTY session's recording at what it holds: after capture has
    /// drained the PTY, or once the child is stopped. Bounded by the
    /// recorder's close timeout.
    fn finish_recording(&mut self) {
        let Some(PtySide::Started(pty)) = &mut self.pty else {
            return;
        };
        if let Some(PtyRecordingRun { thread, dir }) = pty.recording.take() {
            let (state, warning) = finished_recording(thread.finish());
            self.meta.set_pty_recording(PtyRecording {
                dir,
                input_recorded: false,
                state,
            });
            if let Some(warning) = warning {
                self.meta.add_warning(warning);
            }
        }
    }

    fn supervise(&mut self) -> Result<ExitReason, SidecarFailure> {
        // Recover: without output.log the run still has an exit worth
        // recording, but the child's output must still be read, or it blocks
        // on a full pipe. Drain it.
        self.step = SidecarStep::OutputLog;
        let log_path = self.session_dir().join("output.log");
        let opened = test_fault(SidecarStep::OutputLog.as_str())
            .and_then(|()| OpenOptions::new().create(true).append(true).open(&log_path));
        let log: Box<dyn Write + Send> = match opened {
            Ok(file) => Box::new(file),
            Err(e) => {
                self.meta.add_warning(format!(
                    "output not captured: cannot open output.log ({e}); output was drained"
                ));
                // Best effort, so the warning is visible while the run lasts;
                // the terminal write carries it regardless.
                let _ = self.write_meta();
                Box::new(io::sink())
            }
        };
        self.capture_output(log);

        // End: an exit that cannot be observed cannot be classified.
        self.step = SidecarStep::ChildWait;
        let status = match test_fault(SidecarStep::ChildWait.as_str())
            .and_then(|()| Current::child_wait(&mut self.child))
        {
            Ok(status) => status,
            Err(e) => {
                return Err(self.fail(
                    SidecarStep::ChildWait,
                    format!("cannot observe the child's exit: {e}"),
                ));
            }
        };
        Ok(match status.code() {
            Some(0) => ExitReason::ExitedOk,
            Some(code) => ExitReason::ExitedError {
                code: NonZeroI32::new(code).expect("zero is matched above"),
            },
            None => ExitReason::Killed,
        })
    }

    /// Capture stdout/stderr into `log` until the child closes them. Capture
    /// errors never fail supervision: the child's exit status is still
    /// meaningful. A failed `output.log` write becomes a meta warning while the
    /// run is live, once per stream.
    fn capture_output(&mut self, log: Box<dyn Write + Send>) {
        let log = Mutex::new(log);
        let stdout = Current::child_stdout(&mut self.child);
        let stderr = Current::child_stderr(&mut self.child); // None for PTY sessions
        let (attach_sink, recorder) = match &self.pty {
            Some(PtySide::Started(pty)) => (
                Some(Arc::clone(&pty.sink)),
                pty.recording.as_ref().map(|r| r.thread.recorder()),
            ),
            _ => (None, None),
        };
        let recorder = recorder.as_ref();

        let log_ref = &log;
        let (failed, failures) = std::sync::mpsc::channel();
        let panicked = std::thread::scope(|scope| {
            let stdout_failed = failed.clone();
            let stdout_handle = stdout.map(|s| match attach_sink {
                Some(sink) => scope.spawn(move || {
                    capture_stream_with_tee(s, 'O', log_ref, &stdout_failed, &sink, recorder);
                }),
                None => scope.spawn(move || capture_stream(s, 'O', log_ref, &stdout_failed)),
            });
            let stderr_handle =
                stderr.map(|s| scope.spawn(move || capture_stream(s, 'E', log_ref, &failed)));
            // Ends once both capture threads have dropped their senders.
            for warning in failures {
                self.meta.add_warning(warning);
                // Best effort; the terminal write carries the warning regardless.
                let _ = self.write_meta();
            }
            [(stdout_handle, "stdout"), (stderr_handle, "stderr")]
                .into_iter()
                .filter_map(|(handle, name)| handle?.join().is_err().then_some(name))
                .collect::<Vec<_>>()
        });
        for name in panicked {
            self.meta
                .add_warning(format!("log capture: {name} capture thread panicked"));
        }
    }

    fn finish(&mut self, how: ExitReason) -> anyhow::Result<()> {
        self.ended = true;
        self.watch_cancel.store(true, Ordering::Relaxed);
        let dir = self.session_dir().to_path_buf();

        // Capture has drained the PTY: close the recording at what it holds.
        self.finish_recording();
        let how = self.classify(how);
        self.clean_up_control_files();
        // Breadcrumb no longer needed: meta carries the child identity.
        let _ = std::fs::remove_file(dir.join("child_pid"));
        for warning in collect_warnings(&self.stdin_errors) {
            self.meta.add_warning(warning);
        }

        // WAL order: the durable terminal event precedes the terminal meta
        // write, so terminal meta always implies a logged terminal event
        // (spec §3.6).
        if let Err(e) = self.meta.transition_exited(how, EpochTimestamp::now()) {
            self.meta
                .add_warning(format!("terminal transition refused: {e}"));
        }
        test_abort_point("before_terminal_event");
        self.lifecycle.emit(&mut self.meta, true);
        test_abort_point("before_terminal_meta");
        let recorded = test_fault("terminal_meta")
            .map_err(|e| e.to_string())
            .and_then(|()| self.write_meta().map_err(|e| e.to_string()));
        if let Err(e) = &recorded {
            // The child has already exited: nothing to stop. The durable event
            // lets reconciliation heal meta; keep an independent copy too.
            self.lifecycle.salvage_unrecorded(&self.meta, e);
        }

        // Release the lock: the session is available for --replace.
        test_unlock_gate();
        self.lock.take();
        run_on_exit_hooks(&self.meta, &dir, &mut self.lifecycle);
        recorded.map_err(|e| anyhow::anyhow!("terminal meta not written: {e}"))
    }

    /// Final exit reason. Priority: TimedOut > KilledForced > Killed (from
    /// `kill_acted`) > the raw exit.
    fn classify(&self, how: ExitReason) -> ExitReason {
        let dir = self.session_dir();
        if self.timed_out.load(Ordering::Relaxed) {
            ExitReason::TimedOut
        } else if dir.join("kill_forced").exists() {
            ExitReason::KilledForced
        } else if dir.join("kill_acted").exists() {
            // Sidecar-mediated graceful kill (force=false). The child may
            // report ExitedError on Windows (TerminateJobObject after the
            // grace period), but the user requested a kill.
            ExitReason::Killed
        } else {
            how
        }
    }

    /// Remove the run's control files and transports, including a PTY
    /// session's attach socket and its breadcrumb.
    fn clean_up_control_files(&mut self) {
        let dir = self.session_dir();
        for name in ["kill_forced", "kill_acted", "kill_request"] {
            let _ = std::fs::remove_file(dir.join(name));
        }
        Current::remove_stdin_transport(dir);
        // Taking the side drops it here, started or only bound, which removes
        // the socket.
        if let Some(PtySide::Started(pty)) = self.pty.take() {
            end_attach_side(pty, &mut self.meta);
        }
    }

    /// The failure path: stop the child first, then record
    /// `SidecarFailed { step }`, tell a waiting client, release the lock and
    /// run the hooks. Never claims a record it did not make.
    fn end_failed(&mut self, step: SidecarStep, error: &str) {
        if self.ended {
            return;
        }
        self.ended = true;
        self.step = step;
        self.watch_cancel.store(true, Ordering::Relaxed);

        let stopped = terminate_and_reap(&mut self.child, &self.kill_handle, &self.identity);
        self.finish_recording();

        let dir = self.session_dir().to_path_buf();
        self.meta
            .add_warning(format!("sidecar failed at {step}: {error}"));
        for note in &stopped.notes {
            self.meta.add_warning(note.clone());
        }
        self.clean_up_control_files();
        for warning in collect_warnings(&self.stdin_errors) {
            self.meta.add_warning(warning);
        }

        if let Err(e) =
            self.meta
                .transition_sidecar_failed(self.identity, step, EpochTimestamp::now())
        {
            self.meta
                .add_warning(format!("SidecarFailed transition refused: {e}"));
        }
        self.lifecycle.emit(&mut self.meta, true);
        let recorded = test_fault("failure_record")
            .map_err(|e| e.to_string())
            .and_then(|()| self.write_meta().map_err(|e| e.to_string()));

        match &recorded {
            Ok(()) => {
                if stopped.gone {
                    let _ = std::fs::remove_file(dir.join("child_pid"));
                }
                // A client still waiting gets the terminal snapshot.
                self.signal_readiness();
            }
            Err(record_error) => {
                // Loud on every channel still open. The child_pid breadcrumb
                // stays, so a `start` client keeps the directory and later
                // recovery can still find the child.
                let message = format!(
                    "sidecar failed at {step}: {error}; {}; recording the failure also failed: {record_error}",
                    stopped.summary()
                );
                self.lifecycle.salvage_unrecorded(&self.meta, record_error);
                eprintln!("tendr sidecar: {message}");
                if let Some(writer) = self.ready.take() {
                    let _ = Current::write_ready_signal(writer, &format!("ERROR:{message}\n"));
                }
            }
        }

        test_unlock_gate();
        self.lock.take();
        run_on_exit_hooks(&self.meta, &dir, &mut self.lifecycle);
    }
}

impl Drop for RunCore {
    /// Reached only when the run was abandoned: an early return, a `?`, or a
    /// panic unwinding through the guard.
    fn drop(&mut self) {
        if self.ended {
            return;
        }
        let step = self.step;
        let error = if std::thread::panicking() {
            format!("panicked: {}", super::last_panic_message())
        } else {
            "supervision ended without an outcome".to_owned()
        };
        if std::thread::panicking() {
            // A second panic here would abort; end_failed does not panic.
            self.end_failed(step, &error);
        } else {
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| self.end_failed(step, &error)));
        }
    }
}

/// What stopping the child achieved.
struct Stopped {
    /// The child has exited (reaped, or verifiably gone).
    gone: bool,
    /// Warnings for meta: kill errors, or a child that would not die.
    notes: Vec<String>,
}

impl Stopped {
    fn summary(&self) -> &'static str {
        if self.gone {
            "the child was stopped"
        } else {
            "the child may still be running"
        }
    }
}

/// Stop the child through the platform kill path (process group on Unix, Job
/// Object on Windows): graceful first, forced after the grace period. The
/// graceful kill runs on its own thread while this one reaps, because on Unix
/// an unreaped child stays a zombie that the kill's liveness probe still sees.
/// Panic-free: this runs from `Drop` during unwinding, where a second panic
/// would abort before the child is stopped. If no thread can be created, the
/// child is force-killed inline instead.
fn terminate_and_reap(
    child: &mut SupervisedChild,
    handle: &ChildKillHandle,
    identity: &ProcessIdentity,
) -> Stopped {
    let graceful = handle.clone();
    let killer = std::thread::Builder::new()
        .name("tendr-stop-child".to_owned())
        .spawn(move || Current::kill_child(&graceful, false));
    let mut notes = Vec::new();
    let killer = match killer {
        Ok(killer) => Some(killer),
        Err(e) => {
            notes.push(format!(
                "no thread for a graceful stop ({e}); force-killing"
            ));
            if let Err(e) = Current::kill_child(handle, true) {
                notes.push(format!("force-killing the child failed: {e}"));
            }
            None
        }
    };

    let mut gone = wait_gone(child, identity, Duration::from_secs(10));
    if !gone {
        if let Err(e) = Current::kill_child(handle, true) {
            notes.push(format!("force-killing the child failed: {e}"));
        }
        gone = wait_gone(child, identity, Duration::from_secs(2));
    }
    match killer.map(std::thread::JoinHandle::join) {
        None | Some(Ok(Ok(()))) => {}
        Some(Ok(Err(e))) => notes.push(format!("stopping the child failed: {e}")),
        Some(Err(_)) => notes.push("stopping the child panicked".to_owned()),
    }
    if !gone {
        notes.push(format!(
            "child pid {} may still be running: it outlived a forced kill",
            identity.pid
        ));
    }
    Stopped { gone, notes }
}

/// Reap the child, or confirm it is gone, within `timeout`.
fn wait_gone(child: &mut SupervisedChild, identity: &ProcessIdentity, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match Current::child_try_wait(child) {
            Ok(Some(_)) => return true,
            Ok(None) => {}
            // Cannot reap (e.g. wait itself is failing): fall back to probing.
            Err(_) => {
                if matches!(
                    Current::process_status(identity),
                    ProcessStatus::Missing | ProcessStatus::IdentityMismatch
                ) {
                    return true;
                }
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
