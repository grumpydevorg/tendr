//! Run-bound PTY input controller: who may write to a PTY, and what a write
//! acknowledgement means.
//!
//! This module is pure domain logic with no I/O of its own. The sidecar owns one
//! [`InputArbiter`] per run; how it is used is described in
//! `docs/architecture/04-pty-lane.md` ("The PTY and the input writer").
//!
//! # States and transitions
//!
//! | From | Operation | Result |
//! |---|---|---|
//! | `Unowned { e }` | [`InputArbiter::claim`] | `Owned { e + 1 }` and a run-bound handle |
//! | `Owned { e }` | [`InputArbiter::claim`] | [`ControlError::Busy`]; state unchanged |
//! | any `{ e }` | [`InputArbiter::takeover`] | `Owned { e + 1, Human }`; the previous holder, if any, is returned for retirement |
//! | `Owned { e }` | [`InputArbiter::release`] with the current handle | `Unowned { e }` |
//! | any | [`InputArbiter::release`] with any other handle | [`ControlError::Stale`]; state unchanged |
//!
//! The epoch never decreases and every ownership change advances it by exactly
//! one. Advancing past `u64::MAX` is [`ControlError::EpochExhausted`], never a
//! wrap. A handle authorizes only while its run, epoch, and holder all match the
//! current `Owned` state, so a takeover invalidates every earlier handle, and a
//! released handle can never authorize again — a later owner has a larger epoch.
//! A different run's arbiter rejects the handle by run identity.
//!
//! # Serialized authorization and write
//!
//! Authorization and the PTY write happen inside one `&mut self` call,
//! [`InputArbiter::write_step`], and every ownership change also requires
//! `&mut self`. No takeover can therefore land between the check and the write:
//! exclusive access to the arbiter is the serialization point. Callers must not
//! split the two by authorizing with [`InputArbiter::authorize`] and writing
//! elsewhere. The sidecar keeps the arbiter in the single writer that also
//! applies ownership changes; the sink performs one nonblocking write attempt so
//! a child that is not reading input cannot delay a takeover.
//!
//! # Acknowledgements
//!
//! [`InputOutcome::Accepted`] means every byte of the request was written to the
//! PTY master. It does not mean the child consumed them or finished a turn.
//! [`InputOutcome::Incomplete`] reports exactly how many bytes were written
//! (possibly zero) and why the rest were not. Bytes already written cannot be
//! recalled; only the unwritten remainder is dropped.

use std::fmt;
use std::io;

use thiserror::Error;

use super::ids::RunId;

/// Monotonic ownership counter for one run's PTY input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ControllerEpoch(u64);

impl ControllerEpoch {
    /// The epoch of a PTY that has never been owned.
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The following epoch.
    ///
    /// # Errors
    ///
    /// [`ControlError::EpochExhausted`] when `self` is `u64::MAX`.
    pub fn next(self) -> Result<Self, ControlError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(ControlError::EpochExhausted)
    }
}

impl fmt::Display for ControllerEpoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Identifies one controller: a connection or an explicit claim, assigned by the
/// sidecar. Opaque; carries no invariant beyond identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HolderId(u64);

impl HolderId {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Correlates an input request with its outcome. Not a durability guarantee.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(u64);

impl RequestId {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Who holds input ownership. Serialized by variant name (`"Human"`,
/// `"Agent"`), as `pty.input_revoked` events carry it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
pub enum ControllerKind {
    Human,
    Agent,
}

/// Input ownership of one run's PTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerState {
    Unowned {
        epoch: ControllerEpoch,
    },
    Owned {
        epoch: ControllerEpoch,
        holder: HolderId,
        kind: ControllerKind,
    },
}

impl ControllerState {
    #[must_use]
    pub fn epoch(&self) -> ControllerEpoch {
        match *self {
            Self::Unowned { epoch } | Self::Owned { epoch, .. } => epoch,
        }
    }
}

/// Proof of ownership at one epoch, bound to one run, held by one controller
/// of one kind. Only an [`InputArbiter`] creates handles.
///
/// Not `Clone` or `Copy`: its holder borrows it to write and gives it up to end
/// its input, so nothing can use a handle after its own holder ended it. A
/// takeover still invalidates it from outside, which no owned token can
/// express; that is the epoch check every use makes.
#[derive(Debug, PartialEq, Eq)]
pub struct ControllerHandle {
    key: HandleKey,
    kind: ControllerKind,
}

impl ControllerHandle {
    #[must_use]
    pub fn run_id(&self) -> RunId {
        self.key.run_id
    }

    #[must_use]
    pub fn epoch(&self) -> ControllerEpoch {
        self.key.epoch
    }

    #[must_use]
    pub fn holder(&self) -> HolderId {
        self.key.holder
    }

    #[must_use]
    pub fn kind(&self) -> ControllerKind {
        self.kind
    }

    /// The identity this handle authorizes by.
    #[must_use]
    pub fn key(&self) -> HandleKey {
        self.key
    }
}

/// What a [`ControllerHandle`] authorizes by: its run, epoch, and holder.
/// `Copy`, so queued work can carry it, but only obtainable from a handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandleKey {
    run_id: RunId,
    epoch: ControllerEpoch,
    holder: HolderId,
}

impl HandleKey {
    #[must_use]
    pub fn run_id(self) -> RunId {
        self.run_id
    }

    #[must_use]
    pub fn epoch(self) -> ControllerEpoch {
        self.epoch
    }

    #[must_use]
    pub fn holder(self) -> HolderId {
        self.holder
    }
}

impl From<&ControllerHandle> for HandleKey {
    fn from(handle: &ControllerHandle) -> Self {
        handle.key
    }
}

/// Why a handle does not authorize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleReason {
    /// The handle belongs to a different run.
    RunMismatch,
    /// Nobody owns the PTY now.
    Unowned,
    /// Ownership moved to a later epoch.
    EpochMismatch,
    /// Same epoch, different holder (a forged or foreign handle).
    HolderMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ControlError {
    #[error("PTY input is owned by holder {} ({kind:?}) at epoch {epoch}", holder.get())]
    Busy {
        holder: HolderId,
        kind: ControllerKind,
        epoch: ControllerEpoch,
    },
    #[error("controller handle is stale: {reason:?}")]
    Stale { reason: StaleReason },
    #[error("controller epoch exhausted")]
    EpochExhausted,
}

/// Result of a human takeover.
#[derive(Debug, PartialEq, Eq)]
pub struct Takeover {
    /// The new controller's handle.
    pub handle: ControllerHandle,
    /// The superseded controller, which the caller must retire (close its
    /// connection and drop its queued input). `None` if the PTY was unowned.
    pub retired: Option<(HolderId, ControllerKind)>,
}

/// One nonblocking write attempt to the PTY input.
pub trait PtyInputSink {
    /// Write a prefix of `bytes` without blocking.
    ///
    /// # Errors
    ///
    /// `WouldBlock` and `Interrupted` mean "try again"; any other error is
    /// terminal for the request.
    fn try_write(&mut self, bytes: &[u8]) -> io::Result<usize>;
}

/// Why a request was not written in full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncompleteReason {
    /// The handle stopped authorizing before the request finished.
    NotAuthorized(StaleReason),
    /// The caller's deadline expired first.
    DeadlineExceeded,
    /// The sink accepted zero bytes for a non-empty write: the PTY is closed.
    Closed,
    /// The sink failed with a terminal error.
    SinkFailed(io::ErrorKind),
}

/// Final result of an input request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputOutcome {
    /// All `bytes` were written to the PTY master.
    Accepted {
        request_id: RequestId,
        epoch: ControllerEpoch,
        bytes: usize,
    },
    /// `accepted < total` bytes were written; the remainder was dropped.
    Incomplete {
        request_id: RequestId,
        epoch: ControllerEpoch,
        accepted: usize,
        total: usize,
        reason: IncompleteReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum InputError {
    #[error("input request is empty")]
    Empty,
}

/// A queued input request and its write progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingInput {
    request_id: RequestId,
    key: HandleKey,
    kind: ControllerKind,
    bytes: Vec<u8>,
    accepted: usize,
    outcome: Option<InputOutcome>,
}

impl PendingInput {
    /// # Errors
    ///
    /// [`InputError::Empty`] for an empty request.
    pub fn new(
        request_id: RequestId,
        handle: &ControllerHandle,
        bytes: Vec<u8>,
    ) -> Result<Self, InputError> {
        if bytes.is_empty() {
            return Err(InputError::Empty);
        }
        Ok(Self {
            request_id,
            key: handle.key,
            kind: handle.kind,
            bytes,
            accepted: 0,
            outcome: None,
        })
    }

    #[must_use]
    pub fn request_id(&self) -> RequestId {
        self.request_id
    }

    /// The identity of the handle that queued this request.
    #[must_use]
    pub fn key(&self) -> HandleKey {
        self.key
    }

    /// The kind of controller that queued this request.
    #[must_use]
    pub fn kind(&self) -> ControllerKind {
        self.kind
    }

    /// Bytes written so far.
    #[must_use]
    pub fn accepted(&self) -> usize {
        self.accepted
    }

    #[must_use]
    pub fn total(&self) -> usize {
        self.bytes.len()
    }

    /// The final outcome, once decided.
    #[must_use]
    pub fn outcome(&self) -> Option<InputOutcome> {
        self.outcome
    }

    /// Abandon the remainder because the caller's deadline expired. Returns the
    /// already-decided outcome unchanged if the request had finished.
    pub fn expire(&mut self) -> InputOutcome {
        self.finish_incomplete(IncompleteReason::DeadlineExceeded)
    }

    /// Decide the request as incomplete, unless it is already decided.
    fn finish_incomplete(&mut self, reason: IncompleteReason) -> InputOutcome {
        *self.outcome.get_or_insert(InputOutcome::Incomplete {
            request_id: self.request_id,
            epoch: self.key.epoch,
            accepted: self.accepted,
            total: self.bytes.len(),
            reason,
        })
    }

    /// Record `n` newly written bytes, deciding the request once all are written.
    fn record_written(&mut self, n: usize) {
        debug_assert!(self.outcome.is_none(), "write recorded after the outcome");
        debug_assert!(
            n <= self.bytes.len() - self.accepted,
            "caller validates the count"
        );
        self.accepted += n;
        if self.accepted == self.bytes.len() {
            self.outcome = Some(InputOutcome::Accepted {
                request_id: self.request_id,
                epoch: self.key.epoch,
                bytes: self.accepted,
            });
        }
    }
}

/// Progress of one [`InputArbiter::write_step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteStep {
    /// Not finished; call again when the PTY is writable.
    Pending,
    /// Finished with this outcome. Further steps return it again.
    Done(InputOutcome),
}

/// The single owner of one run's PTY input authority.
#[derive(Debug)]
pub struct InputArbiter {
    run_id: RunId,
    state: ControllerState,
}

impl InputArbiter {
    /// A never-owned PTY at [`ControllerEpoch::ZERO`].
    #[must_use]
    pub fn new(run_id: RunId) -> Self {
        Self::resume(
            run_id,
            ControllerState::Unowned {
                epoch: ControllerEpoch::ZERO,
            },
        )
    }

    /// Resume from a persisted state.
    #[must_use]
    pub fn resume(run_id: RunId, state: ControllerState) -> Self {
        Self { run_id, state }
    }

    #[must_use]
    pub fn run_id(&self) -> RunId {
        self.run_id
    }

    #[must_use]
    pub fn state(&self) -> ControllerState {
        self.state
    }

    /// Take ownership of an unowned PTY.
    ///
    /// # Errors
    ///
    /// [`ControlError::Busy`] if owned; [`ControlError::EpochExhausted`].
    pub fn claim(
        &mut self,
        holder: HolderId,
        kind: ControllerKind,
    ) -> Result<ControllerHandle, ControlError> {
        match self.state {
            ControllerState::Owned {
                epoch,
                holder: owner,
                kind: owner_kind,
            } => Err(ControlError::Busy {
                holder: owner,
                kind: owner_kind,
                epoch,
            }),
            ControllerState::Unowned { epoch } => Ok(self.own(epoch.next()?, holder, kind)),
        }
    }

    /// Explicit human takeover: succeeds whether or not the PTY is owned.
    ///
    /// # Errors
    ///
    /// [`ControlError::EpochExhausted`]; the state is then unchanged.
    pub fn takeover(&mut self, holder: HolderId) -> Result<Takeover, ControlError> {
        let next = self.state.epoch().next()?;
        let retired = match self.state {
            ControllerState::Unowned { .. } => None,
            ControllerState::Owned {
                holder: owner,
                kind,
                ..
            } => Some((owner, kind)),
        };
        let handle = self.own(next, holder, ControllerKind::Human);
        Ok(Takeover { handle, retired })
    }

    /// Give up ownership. Only the current handle releases.
    ///
    /// # Errors
    ///
    /// [`ControlError::Stale`] for any other handle; the state is unchanged.
    pub fn release(&mut self, handle: &ControllerHandle) -> Result<(), ControlError> {
        self.authorize(handle)?;
        self.state = ControllerState::Unowned {
            epoch: handle.key.epoch,
        };
        Ok(())
    }

    /// Whether `handle` (a [`ControllerHandle`] or its [`HandleKey`])
    /// authorizes right now. Informational only — see the module docs on
    /// serialization.
    ///
    /// # Errors
    ///
    /// [`ControlError::Stale`] with the reason.
    pub fn authorize(&self, handle: impl Into<HandleKey>) -> Result<(), ControlError> {
        match self.stale_reason(handle.into()) {
            None => Ok(()),
            Some(reason) => Err(ControlError::Stale { reason }),
        }
    }

    /// `None` exactly when `handle` authorizes now.
    fn stale_reason(&self, handle: HandleKey) -> Option<StaleReason> {
        if handle.run_id != self.run_id {
            return Some(StaleReason::RunMismatch);
        }
        match self.state {
            ControllerState::Unowned { .. } => Some(StaleReason::Unowned),
            ControllerState::Owned { epoch, .. } if epoch != handle.epoch => {
                Some(StaleReason::EpochMismatch)
            }
            ControllerState::Owned { holder, .. } if holder != handle.holder => {
                Some(StaleReason::HolderMismatch)
            }
            ControllerState::Owned { .. } => None,
        }
    }

    /// Authorize `pending` and make one write attempt to `sink`, atomically with
    /// respect to ownership changes.
    pub fn write_step<S: PtyInputSink>(
        &mut self,
        pending: &mut PendingInput,
        sink: &mut S,
    ) -> WriteStep {
        if let Some(outcome) = pending.outcome {
            return WriteStep::Done(outcome);
        }
        if let Some(reason) = self.stale_reason(pending.key) {
            return WriteStep::Done(
                pending.finish_incomplete(IncompleteReason::NotAuthorized(reason)),
            );
        }
        let remaining = pending.bytes.len() - pending.accepted;
        match sink.try_write(&pending.bytes[pending.accepted..]) {
            Ok(0) => WriteStep::Done(pending.finish_incomplete(IncompleteReason::Closed)),
            // A count larger than the buffer breaks the `io::Write` contract; fail
            // closed rather than believe it.
            Ok(n) if n > remaining => WriteStep::Done(
                pending.finish_incomplete(IncompleteReason::SinkFailed(io::ErrorKind::InvalidData)),
            ),
            Ok(n) => {
                pending.record_written(n);
                pending.outcome.map_or(WriteStep::Pending, WriteStep::Done)
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                WriteStep::Pending
            }
            Err(e) => {
                WriteStep::Done(pending.finish_incomplete(IncompleteReason::SinkFailed(e.kind())))
            }
        }
    }

    /// Transition to `Owned` at `epoch` and mint its handle.
    fn own(
        &mut self,
        epoch: ControllerEpoch,
        holder: HolderId,
        kind: ControllerKind,
    ) -> ControllerHandle {
        self.state = ControllerState::Owned {
            epoch,
            holder,
            kind,
        };
        ControllerHandle {
            key: HandleKey {
                run_id: self.run_id,
                epoch,
                holder,
            },
            kind,
        }
    }
}
