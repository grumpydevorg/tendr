//! Tests for the shared `poll_until` polling primitive. These live in their own
//! integration-test binary on purpose: `harness/mod.rs` is compiled into every
//! integration test, so a `#[test]` placed there would be duplicated dozens of
//! times.

mod harness;

use harness::{HANG, Observation, WaitTimeout, poll_until};
use std::cell::Cell;
use std::sync::mpsc;
use std::time::Duration;

/// An interval no wait in these tests is meant to sleep through: a poller
/// that slept it would still be asleep long after [`HANG`].
const NEVER: Duration = Duration::from_secs(20 * 60);

/// Run one `poll_until` on its own thread and return its result with the
/// number of probes it made. A poller that sleeps an interval it should not
/// is then reported at [`HANG`] rather than hanging the test.
fn run_poll<T: Send + 'static>(
    timeout: Duration,
    interval: Duration,
    mut probe: impl FnMut(u32) -> Observation<T> + Send + 'static,
) -> (Result<T, WaitTimeout>, u32) {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut probes = 0u32;
        let result = poll_until(timeout, interval, || {
            probes += 1;
            probe(probes)
        });
        let _ = tx.send((result, probes));
    });
    rx.recv_timeout(HANG)
        .expect("poll_until slept through an interval it should have skipped")
}

/// The first probe runs immediately — no leading sleep — so an already-true
/// condition returns without waiting an interval. The interval is one no
/// test outlives: a poller that slept it first would not return at all.
#[test]
fn probes_immediately_without_initial_sleep() {
    let (r, probes) = run_poll(NEVER, NEVER, |_| Observation::Ready(42u32));
    assert_eq!(r.unwrap(), 42);
    assert_eq!(probes, 1, "one probe, already ready");
}

/// An already-true condition is reported even when the deadline has already
/// passed: the probe comes before the deadline check.
#[test]
fn probes_before_checking_the_deadline() {
    let (r, probes) = run_poll(Duration::ZERO, NEVER, |_| Observation::Ready(7u32));
    assert_eq!(r.unwrap(), 7);
    assert_eq!(probes, 1);
}

/// A timeout returns `Err(WaitTimeout)` carrying the last `Pending` description
/// and the elapsed time; `Display` surfaces both.
#[test]
fn deadline_trips_and_records_last_state() {
    let n = Cell::new(0u32);
    let err = poll_until::<()>(Duration::from_millis(80), Duration::from_millis(10), || {
        n.set(n.get() + 1);
        Observation::Pending(format!("attempt {}", n.get()))
    })
    .unwrap_err();

    // A lower bound: load only lengthens the wait.
    assert!(
        err.elapsed >= Duration::from_millis(80),
        "elapsed reaches the deadline"
    );
    assert!(
        err.last.starts_with("attempt "),
        "records the last observed state: {}",
        err.last
    );
    assert!(
        err.to_string().contains(&err.last),
        "Display surfaces the last state: {err}"
    );
}

/// The final wait is clamped to the remaining time, so a long interval cannot
/// overshoot a short deadline: with an interval no test outlives, the wait
/// still ends, at its deadline, after probing once more.
#[test]
fn final_sleep_clamped_to_remaining() {
    let (r, probes) = run_poll(Duration::from_millis(100), NEVER, |_| {
        Observation::<()>::Pending("still".into())
    });
    let err = r.unwrap_err();
    assert!(
        err.elapsed >= Duration::from_millis(100),
        "waited out the deadline: {err}"
    );
    assert!(
        probes >= 2,
        "probed again after the clamped sleep: {probes}"
    );
}

/// Returns the `Ready` value after a few `Pending` probes.
#[test]
fn returns_ready_after_pending() {
    let n = Cell::new(0u32);
    let r = poll_until(HANG, Duration::from_millis(5), || {
        n.set(n.get() + 1);
        if n.get() >= 3 {
            Observation::Ready(n.get())
        } else {
            Observation::Pending("waiting".into())
        }
    });
    assert_eq!(r.unwrap(), 3);
}
