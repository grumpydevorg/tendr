//! What unit tests share. Integration tests have their own in
//! `tests/harness`, which these rules follow.

use std::time::Duration;

/// How long a unit test waits for something expected to happen before it
/// gives up. Only a hang should reach it: a loaded machine makes waits
/// slower, never wrong, so no wait for an expected event is bounded tighter.
pub(crate) const HANG: Duration = Duration::from_secs(120);
