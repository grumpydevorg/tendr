---
id: polling-consolidation
depends_on: []
---

# Test polling consolidation: one `poll_until` primitive

**Status:** steps 1 and 2 shipped (`080ad25`): `poll_until`, `Observation`,
`WaitTimeout` and the domain wrappers are in `tests/harness/mod.rs`. Step 3
remains: migrate the duplicated loops in the test files to the wrappers. This
work does not block cloud PTY control or remote frame transport.

About 33 polling loops in `tests/` share one shape: probe a condition, return on
success, fail on a deadline, sleep an interval, repeat. Each file derives its own
deadline, interval and panic message. This plan moves them onto one primitive
plus named domain wrappers.

The aim is one deadline policy and useful timeout messages, not a smaller
`grep` count. Today one loop panics with its own message, another times out
silently, and some check the deadline before the first probe while others check
it after. With one primitive, every wait reports what it was waiting for and how
long it waited.

## Invariants (pinned before implementation)

1. **Immediate first probe — no initial sleep.** The condition is often already
   true; a leading sleep only adds latency and hides races.
2. **`Instant`-based deadline**, computed once at entry (never a decremented
   counter, never re-derived per iteration).
3. **Returns `Result<T, WaitTimeout>`** — callers can run cleanup (detach, join,
   kill) *before* asserting, so a timeout never bypasses teardown or leaks a
   process. (The lesson from the PTY resize test.)
4. **`WaitTimeout` records the last observed state** and the elapsed time, so the
   failure message says what the wait was stuck on.
5. **Domain wrappers preserve proof-carrying handles** — `QuiescentTerminal`
   (#54) and `OrphanedRunning` (#62) keep their private constructors and returned
   guards; they are re-expressed *on top of* `poll_until`, not flattened away.
6. **Condvar/mpsc conversion is a separate step from filesystem polling** — see
   "Risk classes" below. The core-primitive migration must not also change any
   synchronization mechanism.
7. **No Tokio, no filesystem watchers, no over-generic "eventually" framework.**
   These are synchronous integration tests; added machinery is pure cost.
8. **The initial refactor is behavior-preserving.** Each migrated call site keeps
   its exact timeout value, interval, and first-probe ordering; verified by the
   same stress passes the campaign relied on.

## Core primitive

```rust
/// One probe result: ready with a value, or not-yet with a cheap description
/// of the current state (retained only for the timeout message).
pub enum Observation<T> {
    Ready(T),
    Pending(String),
}

/// Elapsed time plus the last `Pending` description, for the failure message.
pub struct WaitTimeout {
    pub elapsed: Duration,
    pub last: String,
}

/// Probe immediately, then every `interval` until `Ready` or the `Instant`
/// deadline. Never sleeps before the first probe.
pub fn poll_until<T>(
    timeout: Duration,
    interval: Duration,
    mut probe: impl FnMut() -> Observation<T>,
) -> Result<T, WaitTimeout>;
```

Sketch (this *is* the deduplicated machinery — every wrapper reduces to it):

```rust
let start = Instant::now();
let deadline = start + timeout;                       // (2) Instant-based, once
loop {
    match probe() {                                   // (1) immediate first probe
        Observation::Ready(v) => return Ok(v),
        Observation::Pending(last) => {
            let now = Instant::now();                 // one `now` for both uses
            if now >= deadline {
                return Err(WaitTimeout { elapsed: now - start, last }); // (3)(4)
            }
            // Clamp the final wait so we never sleep past the deadline.
            let remaining = deadline.saturating_duration_since(now);
            thread::sleep(interval.min(remaining));   // the one remaining sleep
        }
    }
}
```

`WaitTimeout` derives `Debug` and implements `Display` with one uniform format
(`"timed out after {elapsed:?}; last: {last}"`), so every wait fails the same way.

### `Pending` description cost — a usage rule, not an API knob

`Pending(String)` is computed each probe but only the last survives (into
`WaitTimeout.last`). It **still allocates every iteration** — that is accepted:
at this test-suite scale the per-probe allocation is small and bounded, and worth
the simpler API and better diagnostics. The one rule is that the description must
be a *summary* (`"status=Starting"`, `"12 records, last kind=run.started"`),
never a full buffer dump — probes that accumulate large buffers (follower output,
PTY reader) summarize rather than clone the buffer every tick. (Alternative
considered and rejected for these tests: `Pending` unit + a lazy `describe`
closure invoked only on timeout — more API surface for a guideline a code review
catches anyway.)

## Domain wrappers (names + proof-carrying returns preserved)

`poll_until` returns `Result`; **each wrapper preserves its call sites' existing
timeout *disposition*, not just their timeout value** — this is what makes the
migration behavior-preserving. Today's loops dispose of a timeout four different
ways, and each must be kept:

- **panic** — the common integration-test case (`wait_running`, `wait_terminal`,
  the proof-carrying waits): `.unwrap_or_else(|e| panic!("{e}"))`.
- **return `false`** — e.g. a Windows process-death check that yields `bool`:
  `.is_ok()`.
- **propagate `Result`** — cleanup-first callers (the PTY resize) that must run
  detach/join/kill *before* asserting: return the `Result` unchanged.
- **best-effort / silent** — deliberate break-on-timeout cleanup or callback
  waits that return silently: `.ok()` / ignore.

So the wrapper table below lists each wrapper's *return type*; the disposition is
whatever its current call sites already do, preserved verbatim. Consolidates
today's duplicated loops:

| Wrapper | Returns | Probe |
|---|---|---|
| `wait_running_ns` / `wait_running` | `()` | meta.json status == `Running` |
| `wait_terminal_ns` / `wait_terminal` | `serde_json::Value` | meta.json status is terminal |
| `wait_terminal_quiescent` | `QuiescentTerminal` (guard held) | terminal **and** lock acquired |
| `wait_orphaned_running` | `OrphanedRunning` (guard held) | lock acquired **and** status Running |
| `wait_event_kind` | `serde_json::Value` | event of kind K present |
| `wait_path_exists` / `wait_ready_file` | `()` | path exists |
| `wait_pid_dead` | `()` | PID no longer alive |
| `wait_child_exit` | `ExitStatus` | `child.try_wait()` is `Some` |
| `wait_lock_released` | `LockGuard` | `try_acquire` succeeds |

The two proof-carrying wrappers stay exactly as shipped in shape — private
constructor, returned guard, fail-fast on the contradictory state — with only the
deadline/interval/sleep body replaced by `poll_until`.

## Risk classes — migrate in this order, do not mix

**Class A — filesystem / process polling (low risk, this slice).** meta.json,
event logs, path existence, PID liveness, `try_wait`, `try_acquire`. No in-process
notifier exists for these, so polling is the honest primitive. Pure "factor the
loop, same numbers" — behavior-preserving, file by file, each stress-verified.

**Class B — in-process producer threads (deferred until demonstrated value).**
`ReadyFollower`'s record buffer and the PTY reader threads currently busy-poll a
shared `Mutex<Vec<_>>`/`String`. These *could* become `Condvar::wait_timeout` (the
reader notifies on each new record) or `mpsc::recv_timeout` — eliminating the
poll. But that **changes the synchronization mechanism to fix nothing currently
broken**: the busy-polls are bounded and green under stress. So Class B is
**deferred** — it is speculative churn that introduces a missed-notification risk
without addressing a known failure. Revisit only if a real problem (latency,
flake, CPU) demonstrates the value; when it does, it lands as its own PR with a
dedicated stress pass, never mixed into Class A.

**Class C — child output readiness (deferred / out of scope).** Portable pipe
read deadlines are a separate platform problem and are not addressed by this
slice. Any such wait stays Class A polling for now; a dedicated effort can
revisit blocking reads with OS read deadlines later.

## Migration plan

1. **Shipped (`080ad25`).** `harness::poll_until`, `Observation` and
   `WaitTimeout`, tested in `tests/poll_until.rs`.
2. **Shipped (`080ad25`).** Domain wrappers re-expressed on `poll_until`.
3. Migrate the duplicated meta / event / path / PID / lock loops across the ~17
   files to the wrappers, file by file, stress-passing each.
4. **Deferred:** Class B (Condvar/mpsc for `ReadyFollower` + PTY readers) is not
   part of this slice and is not scheduled — see Risk classes. After Class A, the
   next work is remote-frame Phase 1 (product architecture), not more harness
   polish.

After step 3, direct `thread::sleep` in `tests/` is concentrated in
`poll_until` (plus a couple of specialized platform loops), with one deadline
policy and uniformly useful timeout diagnostics.

## Non-goals

- No async/Tokio, no `notify`/inotify/FSEvents watchers, no generic reactive
  "eventually" DSL.
- Not a change to any production code — this is test-harness consolidation only.
- Not a change to any test's *observable behavior* — timeouts, intervals, and
  proof semantics are preserved (Class A) or improved without weakening (Class B).

## Verification

- `tests/poll_until.rs`: probe-once-then-poll, deadline records last state, final
  sleep clamped to remaining, `WaitTimeout` `Display` format.
- The existing per-file stress passes, re-run after each file's migration.
- Full `nextest` green after each step; Class B gets an extended stress pass.
