//! `harness::SessionGuard` stops a session when the test holding it ends,
//! including when the test panics before reaching its own cleanup (#87).
mod harness;

use harness::{SessionGuard, tendr, wait_running, wait_terminal};
use tempfile::TempDir;

#[test]
fn session_guard_kills_the_session_when_the_test_panics() {
    let root = TempDir::new().unwrap();
    tendr(&root)
        // Never ends by itself: only the guard's kill can end it.
        .args(["start", "guarded", "sleep", "3600"])
        .assert()
        .success();
    wait_running(&root, "guarded");

    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _session = SessionGuard::new(&root, "guarded");
        panic!("the test body fails before its cleanup");
    }));
    assert!(unwound.is_err(), "the closure must have panicked");

    let meta = wait_terminal(&root, "guarded");
    assert_eq!(meta["status"], "Exited", "{meta}");
    assert_eq!(
        meta["reason"], "KilledForced",
        "the guard must force-kill the session during unwinding: {meta}"
    );
}
