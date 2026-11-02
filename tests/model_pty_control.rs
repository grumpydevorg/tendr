//! Contract tests for the run-bound PTY input controller
//! (`tendr::model::pty_control`).

use std::collections::VecDeque;
use std::io;

use proptest::prelude::*;
use tendr::model::ids::RunId;
use tendr::model::pty_control::{
    ControlError, ControllerEpoch, ControllerKind, ControllerState, HolderId, IncompleteReason,
    InputArbiter, InputError, InputOutcome, PendingInput, PtyInputSink, RequestId, StaleReason,
    WriteStep,
};

/// A scripted sink. Each attempt pops one behaviour; an exhausted script accepts
/// everything offered. Every accepted byte count is logged.
#[derive(Default)]
struct ScriptedSink {
    script: VecDeque<Attempt>,
    written: Vec<u8>,
    attempts: usize,
}

#[derive(Debug, Clone, Copy)]
enum Attempt {
    Accept(usize),
    /// Break the `io::Write` contract: report more bytes than were offered.
    OverReport(usize),
    WouldBlock,
    Interrupted,
    Zero,
    Fail(io::ErrorKind),
}

impl ScriptedSink {
    fn with(script: impl IntoIterator<Item = Attempt>) -> Self {
        Self {
            script: script.into_iter().collect(),
            ..Self::default()
        }
    }
}

impl PtyInputSink for ScriptedSink {
    fn try_write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.attempts += 1;
        match self.script.pop_front() {
            None => {
                self.written.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            Some(Attempt::Accept(n)) => {
                let n = n.min(bytes.len());
                self.written.extend_from_slice(&bytes[..n]);
                Ok(n)
            }
            Some(Attempt::OverReport(extra)) => Ok(bytes.len() + extra),
            Some(Attempt::WouldBlock) => Err(io::ErrorKind::WouldBlock.into()),
            Some(Attempt::Interrupted) => Err(io::ErrorKind::Interrupted.into()),
            Some(Attempt::Zero) => Ok(0),
            Some(Attempt::Fail(kind)) => Err(kind.into()),
        }
    }
}

const H1: HolderId = HolderId::new(1);
const H2: HolderId = HolderId::new(2);

fn epoch(n: u64) -> ControllerEpoch {
    ControllerEpoch::new(n)
}

fn run_until_done(
    arbiter: &mut InputArbiter,
    pending: &mut PendingInput,
    sink: &mut ScriptedSink,
) -> InputOutcome {
    for _ in 0..1_000 {
        if let WriteStep::Done(outcome) = arbiter.write_step(pending, sink) {
            return outcome;
        }
    }
    panic!("write did not finish within 1000 steps");
}

// ---------------------------------------------------------------------------
// Epochs and transitions
// ---------------------------------------------------------------------------

#[test]
fn new_arbiter_is_unowned_at_epoch_zero() {
    let arbiter = InputArbiter::new(RunId::new());
    assert_eq!(
        arbiter.state(),
        ControllerState::Unowned {
            epoch: ControllerEpoch::ZERO
        }
    );
}

#[test]
fn epoch_next_advances_by_one_and_refuses_to_wrap() {
    assert_eq!(epoch(0).next(), Ok(epoch(1)));
    assert_eq!(epoch(41).next(), Ok(epoch(42)));
    assert_eq!(epoch(u64::MAX).next(), Err(ControlError::EpochExhausted));
}

#[test]
fn claim_on_unowned_advances_epoch_and_binds_holder() {
    let run = RunId::new();
    let mut arbiter = InputArbiter::new(run);
    let handle = arbiter.claim(H1, ControllerKind::Agent).unwrap();

    assert_eq!(handle.run_id(), run);
    assert_eq!(handle.epoch(), epoch(1));
    assert_eq!(handle.holder(), H1);
    assert_eq!(handle.kind(), ControllerKind::Agent);
    assert_eq!(
        arbiter.state(),
        ControllerState::Owned {
            epoch: epoch(1),
            holder: H1,
            kind: ControllerKind::Agent
        }
    );
    assert_eq!(arbiter.authorize(&handle), Ok(()));
}

#[test]
fn claim_on_owned_is_busy_and_leaves_state_unchanged() {
    let mut arbiter = InputArbiter::new(RunId::new());
    arbiter.claim(H1, ControllerKind::Agent).unwrap();
    let before = arbiter.state();

    assert_eq!(
        arbiter.claim(H2, ControllerKind::Human),
        Err(ControlError::Busy {
            holder: H1,
            kind: ControllerKind::Agent,
            epoch: epoch(1)
        })
    );
    assert_eq!(arbiter.state(), before);
}

#[test]
fn takeover_of_owned_pty_advances_epoch_and_retires_previous_holder() {
    let mut arbiter = InputArbiter::new(RunId::new());
    let agent = arbiter.claim(H1, ControllerKind::Agent).unwrap();

    let takeover = arbiter.takeover(H2).unwrap();

    assert_eq!(takeover.handle.epoch(), epoch(2));
    assert_eq!(takeover.handle.holder(), H2);
    assert_eq!(takeover.handle.kind(), ControllerKind::Human);
    assert_eq!(takeover.retired, Some((H1, ControllerKind::Agent)));
    assert_eq!(
        arbiter.state(),
        ControllerState::Owned {
            epoch: epoch(2),
            holder: H2,
            kind: ControllerKind::Human
        }
    );
    assert_eq!(
        arbiter.authorize(&agent),
        Err(ControlError::Stale {
            reason: StaleReason::EpochMismatch
        })
    );
}

#[test]
fn takeover_of_unowned_pty_retires_nobody() {
    let mut arbiter = InputArbiter::new(RunId::new());
    let takeover = arbiter.takeover(H1).unwrap();
    assert_eq!(takeover.retired, None);
    assert_eq!(takeover.handle.epoch(), epoch(1));
}

#[test]
fn human_takeover_supersedes_an_earlier_human_even_if_still_connected() {
    let mut arbiter = InputArbiter::new(RunId::new());
    let first = arbiter.takeover(H1).unwrap().handle;
    let second = arbiter.takeover(H2).unwrap();

    assert_eq!(second.retired, Some((H1, ControllerKind::Human)));
    assert!(arbiter.authorize(&first).is_err());
    assert_eq!(arbiter.authorize(&second.handle), Ok(()));
}

#[test]
fn takeover_at_exhausted_epoch_fails_without_changing_state() {
    let mut arbiter = InputArbiter::from_state(
        RunId::new(),
        ControllerState::Owned {
            epoch: epoch(u64::MAX),
            holder: H1,
            kind: ControllerKind::Agent,
        },
    );
    let before = arbiter.state();
    assert_eq!(arbiter.takeover(H2), Err(ControlError::EpochExhausted));
    assert_eq!(arbiter.state(), before);
}

#[test]
fn claim_at_exhausted_epoch_fails_without_changing_state() {
    let mut arbiter = InputArbiter::from_state(
        RunId::new(),
        ControllerState::Unowned {
            epoch: epoch(u64::MAX),
        },
    );
    assert_eq!(
        arbiter.claim(H1, ControllerKind::Agent),
        Err(ControlError::EpochExhausted)
    );
    assert_eq!(
        arbiter.state(),
        ControllerState::Unowned {
            epoch: epoch(u64::MAX)
        }
    );
}

// ---------------------------------------------------------------------------
// Resume: a human regains the control it held at an epoch
// ---------------------------------------------------------------------------

const H3: HolderId = HolderId::new(3);

fn stale() -> ControlError {
    ControlError::Stale {
        reason: StaleReason::EpochMismatch,
    }
}

#[test]
fn resume_after_the_holder_left_grants_the_next_epoch() {
    let mut arbiter = InputArbiter::new(RunId::new());
    let first = arbiter.claim(H1, ControllerKind::Human).unwrap();
    arbiter.release(&first).unwrap();

    let resumed = arbiter.resume(H2, epoch(1)).unwrap();

    assert_eq!(resumed.handle.epoch(), epoch(2), "never epoch 1 again");
    assert_eq!(resumed.handle.holder(), H2);
    assert_eq!(resumed.handle.kind(), ControllerKind::Human);
    assert_eq!(resumed.retired, None);
    assert!(arbiter.authorize(&first).is_err());
}

/// The half-open case: the old connection still holds its epoch. The resume
/// supersedes it, and its handle no longer authorizes.
#[test]
fn resume_retires_the_stale_connection_still_holding_its_epoch() {
    let mut arbiter = InputArbiter::new(RunId::new());
    let stale_handle = arbiter.claim(H1, ControllerKind::Human).unwrap();

    let resumed = arbiter.resume(H2, epoch(1)).unwrap();

    assert_eq!(resumed.handle.epoch(), epoch(2));
    assert_eq!(resumed.retired, Some((H1, ControllerKind::Human)));
    assert_eq!(
        arbiter.release(&stale_handle),
        Err(stale()),
        "the old connection's late disconnect releases nothing"
    );
    assert_eq!(arbiter.authorize(&resumed.handle), Ok(()));
}

#[test]
fn resume_after_a_human_took_control_since_is_refused() {
    // Taken over, and the taker still holds it.
    let mut arbiter = InputArbiter::new(RunId::new());
    arbiter.claim(H1, ControllerKind::Human).unwrap();
    arbiter.takeover(H3).unwrap();
    let before = arbiter.state();
    assert_eq!(arbiter.resume(H2, epoch(1)).unwrap_err(), stale());
    assert_eq!(arbiter.state(), before);

    // Attached and detached again in between: it still counts.
    let mut arbiter = InputArbiter::new(RunId::new());
    let first = arbiter.claim(H1, ControllerKind::Human).unwrap();
    arbiter.release(&first).unwrap();
    let other = arbiter.claim(H3, ControllerKind::Human).unwrap();
    arbiter.release(&other).unwrap();
    assert_eq!(arbiter.resume(H2, epoch(1)).unwrap_err(), stale());
    assert_eq!(
        arbiter.state(),
        ControllerState::Unowned { epoch: epoch(2) }
    );
}

#[test]
fn resume_after_a_finished_push_succeeds() {
    let mut arbiter = InputArbiter::new(RunId::new());
    let human = arbiter.claim(H1, ControllerKind::Human).unwrap();
    arbiter.release(&human).unwrap();
    let push = arbiter.claim(H3, ControllerKind::Agent).unwrap();
    arbiter.release(&push).unwrap();

    let resumed = arbiter.resume(H2, epoch(1)).unwrap();
    assert_eq!(resumed.handle.epoch(), epoch(3), "after the push's epoch");
    assert_eq!(resumed.retired, None);
}

#[test]
fn resume_while_a_push_runs_is_busy() {
    let mut arbiter = InputArbiter::new(RunId::new());
    let human = arbiter.claim(H1, ControllerKind::Human).unwrap();
    arbiter.release(&human).unwrap();
    arbiter.claim(H3, ControllerKind::Agent).unwrap();
    let before = arbiter.state();

    assert_eq!(
        arbiter.resume(H2, epoch(1)),
        Err(ControlError::Busy {
            holder: H3,
            kind: ControllerKind::Agent,
            epoch: epoch(2)
        })
    );
    assert_eq!(arbiter.state(), before);
}

#[test]
fn resume_of_an_epoch_no_human_was_granted_is_refused() {
    let mut arbiter = InputArbiter::new(RunId::new());
    assert_eq!(arbiter.resume(H1, epoch(0)).unwrap_err(), stale());
    let push = arbiter.claim(H3, ControllerKind::Agent).unwrap();
    arbiter.release(&push).unwrap();
    assert_eq!(arbiter.resume(H1, epoch(1)).unwrap_err(), stale());
    assert_eq!(arbiter.resume(H1, epoch(7)).unwrap_err(), stale());
}

/// Only one client resumes from an epoch; the one that did can resume
/// again from its own.
#[test]
fn a_resumed_epoch_is_the_only_one_left_to_resume() {
    let mut arbiter = InputArbiter::new(RunId::new());
    arbiter.claim(H1, ControllerKind::Human).unwrap();
    arbiter.resume(H2, epoch(1)).unwrap();
    assert_eq!(arbiter.resume(H3, epoch(1)).unwrap_err(), stale());
    let again = arbiter.resume(H3, epoch(2)).unwrap();
    assert_eq!(again.handle.epoch(), epoch(3));
    assert_eq!(again.retired, Some((H2, ControllerKind::Human)));
}

#[test]
fn resume_at_exhausted_epoch_fails_without_changing_state() {
    let mut arbiter = InputArbiter::from_state(
        RunId::new(),
        ControllerState::Owned {
            epoch: epoch(u64::MAX),
            holder: H1,
            kind: ControllerKind::Human,
        },
    );
    let before = arbiter.state();
    assert_eq!(
        arbiter.resume(H2, epoch(u64::MAX)),
        Err(ControlError::EpochExhausted)
    );
    assert_eq!(arbiter.state(), before);
}

#[test]
fn release_by_current_handle_unowns_without_advancing_epoch() {
    let mut arbiter = InputArbiter::new(RunId::new());
    let handle = arbiter.claim(H1, ControllerKind::Human).unwrap();

    assert_eq!(arbiter.release(&handle), Ok(()));
    assert_eq!(
        arbiter.state(),
        ControllerState::Unowned { epoch: epoch(1) }
    );
    assert_eq!(
        arbiter.authorize(&handle),
        Err(ControlError::Stale {
            reason: StaleReason::Unowned
        })
    );
}

#[test]
fn release_by_superseded_handle_cannot_release_its_successor() {
    let mut arbiter = InputArbiter::new(RunId::new());
    let old = arbiter.takeover(H1).unwrap().handle;
    let new = arbiter.takeover(H2).unwrap().handle;
    let before = arbiter.state();

    assert_eq!(
        arbiter.release(&old),
        Err(ControlError::Stale {
            reason: StaleReason::EpochMismatch
        })
    );
    assert_eq!(arbiter.state(), before);
    assert_eq!(arbiter.authorize(&new), Ok(()));
}

#[test]
fn agent_must_reclaim_after_human_detach_and_old_agent_handle_stays_stale() {
    let mut arbiter = InputArbiter::new(RunId::new());
    let agent = arbiter.claim(H1, ControllerKind::Agent).unwrap();
    let human = arbiter.takeover(H2).unwrap().handle;
    arbiter.release(&human).unwrap();

    // The preempted agent handle is never silently restored.
    assert!(arbiter.authorize(&agent).is_err());

    let reclaimed = arbiter.claim(H1, ControllerKind::Agent).unwrap();
    assert_eq!(reclaimed.epoch(), epoch(3));
    assert!(arbiter.authorize(&agent).is_err());
    assert_eq!(arbiter.authorize(&reclaimed), Ok(()));
}

#[test]
fn handle_from_another_run_is_rejected_by_run_identity() {
    let mut first = InputArbiter::new(RunId::new());
    let mut second = InputArbiter::new(RunId::new());
    let foreign = first.claim(H1, ControllerKind::Human).unwrap();
    second.claim(H1, ControllerKind::Human).unwrap();

    assert_eq!(
        second.authorize(&foreign),
        Err(ControlError::Stale {
            reason: StaleReason::RunMismatch
        })
    );
}

#[test]
fn same_epoch_different_holder_is_rejected() {
    let run = RunId::new();
    let mut a = InputArbiter::new(run);
    let mut b = InputArbiter::new(run);
    let from_a = a.claim(H1, ControllerKind::Human).unwrap();
    b.claim(H2, ControllerKind::Human).unwrap();

    assert_eq!(
        b.authorize(&from_a),
        Err(ControlError::Stale {
            reason: StaleReason::HolderMismatch
        })
    );
}

// ---------------------------------------------------------------------------
// Input requests and acknowledgements
// ---------------------------------------------------------------------------

#[test]
fn empty_input_request_is_rejected() {
    let mut arbiter = InputArbiter::new(RunId::new());
    let handle = arbiter.claim(H1, ControllerKind::Agent).unwrap();
    assert_eq!(
        PendingInput::new(RequestId::new(1), &handle, Vec::new()),
        Err(InputError::Empty)
    );
}

#[test]
fn fully_written_request_is_accepted_with_exact_byte_count() {
    let mut arbiter = InputArbiter::new(RunId::new());
    let handle = arbiter.claim(H1, ControllerKind::Agent).unwrap();
    let mut pending = PendingInput::new(RequestId::new(7), &handle, b"hello".to_vec()).unwrap();
    let mut sink = ScriptedSink::with([Attempt::Accept(2), Attempt::WouldBlock]);

    let outcome = run_until_done(&mut arbiter, &mut pending, &mut sink);

    assert_eq!(
        outcome,
        InputOutcome::Accepted {
            request_id: RequestId::new(7),
            epoch: epoch(1),
            bytes: 5
        }
    );
    assert_eq!(sink.written, b"hello");
    assert_eq!(pending.accepted(), 5);
    assert_eq!(pending.total(), 5);
    assert_eq!(pending.outcome(), Some(outcome));
}

#[test]
fn would_block_and_interrupted_leave_the_request_pending_without_progress() {
    let mut arbiter = InputArbiter::new(RunId::new());
    let handle = arbiter.claim(H1, ControllerKind::Agent).unwrap();
    let mut pending = PendingInput::new(RequestId::new(1), &handle, b"abc".to_vec()).unwrap();
    let mut sink = ScriptedSink::with([Attempt::WouldBlock, Attempt::Interrupted]);

    assert_eq!(
        arbiter.write_step(&mut pending, &mut sink),
        WriteStep::Pending
    );
    assert_eq!(
        arbiter.write_step(&mut pending, &mut sink),
        WriteStep::Pending
    );
    assert_eq!(pending.accepted(), 0);
    assert_eq!(pending.outcome(), None);
}

#[test]
fn takeover_between_chunks_revokes_the_remainder_and_reports_partial_count() {
    let mut arbiter = InputArbiter::new(RunId::new());
    let agent = arbiter.claim(H1, ControllerKind::Agent).unwrap();
    let mut pending = PendingInput::new(RequestId::new(9), &agent, b"abcdef".to_vec()).unwrap();
    let mut sink = ScriptedSink::with([Attempt::Accept(2)]);

    assert_eq!(
        arbiter.write_step(&mut pending, &mut sink),
        WriteStep::Pending
    );
    arbiter.takeover(H2).unwrap();
    let step = arbiter.write_step(&mut pending, &mut sink);

    let expected = InputOutcome::Incomplete {
        request_id: RequestId::new(9),
        epoch: epoch(1),
        accepted: 2,
        total: 6,
        reason: IncompleteReason::NotAuthorized(StaleReason::EpochMismatch),
    };
    assert_eq!(step, WriteStep::Done(expected));
    assert_eq!(
        sink.written, b"ab",
        "no byte from the old epoch after takeover"
    );
    assert_eq!(sink.attempts, 1, "the revoked step never reaches the sink");
}

#[test]
fn queued_request_of_superseded_controller_writes_nothing() {
    let mut arbiter = InputArbiter::new(RunId::new());
    let agent = arbiter.claim(H1, ControllerKind::Agent).unwrap();
    let mut queued = PendingInput::new(RequestId::new(3), &agent, b"rm -rf".to_vec()).unwrap();
    arbiter.takeover(H2).unwrap();
    let mut sink = ScriptedSink::default();

    let outcome = run_until_done(&mut arbiter, &mut queued, &mut sink);

    assert_eq!(
        outcome,
        InputOutcome::Incomplete {
            request_id: RequestId::new(3),
            epoch: epoch(1),
            accepted: 0,
            total: 6,
            reason: IncompleteReason::NotAuthorized(StaleReason::EpochMismatch),
        }
    );
    assert!(sink.written.is_empty());
    assert_eq!(sink.attempts, 0);
}

#[test]
fn deadline_expiry_reports_bytes_written_so_far() {
    let mut arbiter = InputArbiter::new(RunId::new());
    let handle = arbiter.claim(H1, ControllerKind::Agent).unwrap();
    let mut pending = PendingInput::new(RequestId::new(4), &handle, b"abcd".to_vec()).unwrap();
    let mut sink = ScriptedSink::with([Attempt::Accept(1), Attempt::WouldBlock]);
    arbiter.write_step(&mut pending, &mut sink);
    arbiter.write_step(&mut pending, &mut sink);

    let outcome = pending.expire();

    assert_eq!(
        outcome,
        InputOutcome::Incomplete {
            request_id: RequestId::new(4),
            epoch: epoch(1),
            accepted: 1,
            total: 4,
            reason: IncompleteReason::DeadlineExceeded,
        }
    );
    assert_eq!(
        arbiter.write_step(&mut pending, &mut sink),
        WriteStep::Done(outcome),
        "a finished request never writes again"
    );
    assert_eq!(sink.written, b"a");
}

#[test]
fn expire_after_completion_keeps_the_accepted_outcome() {
    let mut arbiter = InputArbiter::new(RunId::new());
    let handle = arbiter.claim(H1, ControllerKind::Agent).unwrap();
    let mut pending = PendingInput::new(RequestId::new(5), &handle, b"ok".to_vec()).unwrap();
    let mut sink = ScriptedSink::default();
    let accepted = run_until_done(&mut arbiter, &mut pending, &mut sink);

    assert_eq!(pending.expire(), accepted);
}

#[test]
fn zero_byte_write_means_closed() {
    let mut arbiter = InputArbiter::new(RunId::new());
    let handle = arbiter.claim(H1, ControllerKind::Agent).unwrap();
    let mut pending = PendingInput::new(RequestId::new(6), &handle, b"xy".to_vec()).unwrap();
    let mut sink = ScriptedSink::with([Attempt::Accept(1), Attempt::Zero]);

    let outcome = run_until_done(&mut arbiter, &mut pending, &mut sink);

    assert_eq!(
        outcome,
        InputOutcome::Incomplete {
            request_id: RequestId::new(6),
            epoch: epoch(1),
            accepted: 1,
            total: 2,
            reason: IncompleteReason::Closed,
        }
    );
}

#[test]
fn sink_reporting_more_bytes_than_offered_fails_closed_without_panicking() {
    let mut arbiter = InputArbiter::new(RunId::new());
    let handle = arbiter.claim(H1, ControllerKind::Agent).unwrap();
    let mut pending = PendingInput::new(RequestId::new(10), &handle, b"abcd".to_vec()).unwrap();
    let mut sink = ScriptedSink::with([Attempt::Accept(1), Attempt::OverReport(5)]);

    let outcome = run_until_done(&mut arbiter, &mut pending, &mut sink);

    assert_eq!(
        outcome,
        InputOutcome::Incomplete {
            request_id: RequestId::new(10),
            epoch: epoch(1),
            accepted: 1,
            total: 4,
            reason: IncompleteReason::SinkFailed(io::ErrorKind::InvalidData),
        }
    );
    assert_eq!(
        pending.accepted(),
        1,
        "an impossible count is never believed"
    );
}

#[test]
fn terminal_sink_error_is_reported_with_its_kind() {
    let mut arbiter = InputArbiter::new(RunId::new());
    let handle = arbiter.claim(H1, ControllerKind::Agent).unwrap();
    let mut pending = PendingInput::new(RequestId::new(8), &handle, b"xy".to_vec()).unwrap();
    let mut sink = ScriptedSink::with([Attempt::Fail(io::ErrorKind::BrokenPipe)]);

    let outcome = run_until_done(&mut arbiter, &mut pending, &mut sink);

    assert_eq!(
        outcome,
        InputOutcome::Incomplete {
            request_id: RequestId::new(8),
            epoch: epoch(1),
            accepted: 0,
            total: 2,
            reason: IncompleteReason::SinkFailed(io::ErrorKind::BrokenPipe),
        }
    );
}

// ---------------------------------------------------------------------------
// Property: arbitrary interleavings never let a stale controller write
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Op {
    Claim(u64, bool),
    Takeover(u64),
    Release(usize),
    /// Resume from the epoch of the handle at this index.
    Resume(u64, usize),
    Submit(usize, usize),
    Step(usize, Attempt),
    Expire(usize),
}

fn attempt_strategy() -> impl Strategy<Value = Attempt> {
    prop_oneof![
        4 => (0usize..8).prop_map(Attempt::Accept),
        2 => Just(Attempt::WouldBlock),
        1 => Just(Attempt::Interrupted),
        1 => Just(Attempt::Zero),
        1 => Just(Attempt::Fail(io::ErrorKind::BrokenPipe)),
        1 => (1usize..4).prop_map(Attempt::OverReport),
    ]
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        2 => (1u64..4, any::<bool>()).prop_map(|(h, human)| Op::Claim(h, human)),
        2 => (1u64..4).prop_map(Op::Takeover),
        1 => any::<usize>().prop_map(Op::Release),
        1 => (1u64..4, any::<usize>()).prop_map(|(h, i)| Op::Resume(h, i)),
        2 => (any::<usize>(), 1usize..12).prop_map(|(h, len)| Op::Submit(h, len)),
        5 => (any::<usize>(), attempt_strategy()).prop_map(|(p, a)| Op::Step(p, a)),
        1 => any::<usize>().prop_map(Op::Expire),
    ]
}

proptest! {
    #[test]
    fn no_interleaving_lets_a_stale_controller_write(ops in proptest::collection::vec(op_strategy(), 1..80)) {
        let mut arbiter = InputArbiter::new(RunId::new());
        let mut handles = Vec::new();
        let mut pendings: Vec<(PendingInput, usize)> = Vec::new();
        let mut last_epoch = arbiter.state().epoch();
        // The model of the resume rule: the last epoch a human was granted.
        let mut last_human = None;

        for op in ops {
            let before = arbiter.state();
            match op {
                Op::Claim(h, human) => {
                    let kind = if human { ControllerKind::Human } else { ControllerKind::Agent };
                    match arbiter.claim(HolderId::new(h), kind) {
                        Ok(handle) => {
                            let was_unowned = matches!(before, ControllerState::Unowned { .. });
                            prop_assert!(was_unowned);
                            prop_assert_eq!(handle.epoch().get(), before.epoch().get() + 1);
                            if human {
                                last_human = Some(handle.epoch());
                            }
                            handles.push(handle);
                        }
                        Err(ControlError::Busy { .. }) => {
                            let was_owned = matches!(before, ControllerState::Owned { .. });
                            prop_assert!(was_owned);
                            prop_assert_eq!(arbiter.state(), before);
                        }
                        Err(other) => prop_assert!(false, "unexpected claim error {other:?}"),
                    }
                }
                Op::Takeover(h) => {
                    let takeover = arbiter.takeover(HolderId::new(h)).unwrap();
                    prop_assert_eq!(takeover.handle.epoch().get(), before.epoch().get() + 1);
                    for old in &handles {
                        prop_assert!(arbiter.authorize(old).is_err());
                    }
                    last_human = Some(takeover.handle.epoch());
                    handles.push(takeover.handle);
                }
                Op::Resume(h, i) => {
                    if handles.is_empty() { continue; }
                    let from = handles[i % handles.len()].epoch();
                    let unclaimed = match before {
                        ControllerState::Unowned { .. } => true,
                        ControllerState::Owned { epoch, kind, .. } => {
                            epoch == from && kind == ControllerKind::Human
                        }
                    };
                    match arbiter.resume(HolderId::new(h), from) {
                        Ok(resumed) => {
                            prop_assert_eq!(last_human, Some(from), "resumed past a later human");
                            prop_assert!(unclaimed, "resumed over another holder");
                            prop_assert_eq!(resumed.handle.epoch().get(), before.epoch().get() + 1);
                            for old in &handles {
                                prop_assert!(arbiter.authorize(old).is_err());
                            }
                            last_human = Some(resumed.handle.epoch());
                            handles.push(resumed.handle);
                        }
                        Err(_) => {
                            prop_assert!(last_human != Some(from) || !unclaimed);
                            prop_assert_eq!(arbiter.state(), before);
                        }
                    }
                }
                Op::Release(i) => {
                    if handles.is_empty() { continue; }
                    let handle = &handles[i % handles.len()];
                    let current = arbiter.authorize(handle).is_ok();
                    let result = arbiter.release(handle);
                    prop_assert_eq!(result.is_ok(), current);
                    if current {
                        prop_assert_eq!(arbiter.state(), ControllerState::Unowned { epoch: before.epoch() });
                    } else {
                        prop_assert_eq!(arbiter.state(), before);
                    }
                }
                Op::Submit(i, len) => {
                    if handles.is_empty() { continue; }
                    let handle = &handles[i % handles.len()];
                    let id = RequestId::new(pendings.len() as u64);
                    pendings.push((PendingInput::new(id, handle, vec![b'x'; len]).unwrap(), 0));
                }
                Op::Step(i, attempt) => {
                    if pendings.is_empty() { continue; }
                    let n = pendings.len();
                    let (pending, written) = &mut pendings[i % n];
                    let mut sink = ScriptedSink::with([attempt]);
                    let authorized_before = arbiter.authorize(pending.key()).is_ok();
                    let finished_before = pending.outcome().is_some();
                    let step = arbiter.write_step(pending, &mut sink);
                    prop_assert_eq!(arbiter.state(), before, "a write step never changes ownership");
                    if !sink.written.is_empty() {
                        prop_assert!(authorized_before, "bytes written by a stale controller");
                        prop_assert!(!finished_before, "bytes written after the request finished");
                    }
                    *written += sink.written.len();
                    prop_assert_eq!(pending.accepted(), *written);
                    if let WriteStep::Done(outcome) = step {
                        match outcome {
                            InputOutcome::Accepted { bytes, epoch, .. } => {
                                prop_assert_eq!(bytes, pending.total());
                                prop_assert_eq!(bytes, *written);
                                prop_assert_eq!(epoch, pending.key().epoch());
                            }
                            InputOutcome::Incomplete { accepted, total, epoch, .. } => {
                                prop_assert!(accepted < total);
                                prop_assert_eq!(accepted, *written);
                                prop_assert_eq!(total, pending.total());
                                prop_assert_eq!(epoch, pending.key().epoch());
                            }
                        }
                    }
                }
                Op::Expire(i) => {
                    if pendings.is_empty() { continue; }
                    let n = pendings.len();
                    let (pending, written) = &mut pendings[i % n];
                    let decided = pending.outcome();
                    let outcome = pending.expire();
                    if let Some(decided) = decided {
                        prop_assert_eq!(outcome, decided);
                    } else {
                        let is_deadline = matches!(
                            outcome,
                            InputOutcome::Incomplete { reason: IncompleteReason::DeadlineExceeded, .. }
                        );
                        prop_assert!(is_deadline);
                    }
                    prop_assert_eq!(pending.accepted(), *written);
                }
            }
            let now = arbiter.state().epoch();
            prop_assert!(now >= last_epoch, "epoch decreased");
            prop_assert!(now.get() <= last_epoch.get() + 1, "epoch skipped");
            last_epoch = now;
        }
    }
}
