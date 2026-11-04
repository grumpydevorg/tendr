//! Debug-only lengthening of product timers. An integration test whose
//! scenario needs a timer not to fire on a loaded machine sets the timer's
//! `TENDR_TEST_<NAME>_MS` variable (see "Time" in tests/harness/mod.rs).
//! Release builds read no variable.

use std::time::Duration;

/// `default`, or in a debug build the milliseconds in `var` when it is set.
pub fn or_env(var: &str, default: Duration) -> Duration {
    if cfg!(debug_assertions) {
        if let Some(ms) = std::env::var(var).ok().and_then(|v| v.parse().ok()) {
            return Duration::from_millis(ms);
        }
    }
    default
}
