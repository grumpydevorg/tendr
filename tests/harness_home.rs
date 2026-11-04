//! Dropping a `harness::Home` stops every session in it and waits for their
//! sidecars before deleting it, including when the test panics (#87), so no
//! sidecar writes under the deleted directory afterwards.
mod harness;

use harness::{HANG, Home, Observation, poll_until, tendr, wait_running_ns};

#[test]
fn dropping_a_home_stops_its_sessions_before_deleting_it() {
    let home = Home::new();
    let path = home.path().to_owned();
    let mut sidecars = Vec::new();
    for (namespace, name) in [("default", "a"), ("other", "b")] {
        tendr(&home)
            // Never ends by itself: only the drop's kill can end it.
            .args(["start", name, "--namespace", namespace, "sleep", "3600"])
            .assert()
            .success();
        wait_running_ns(&home, namespace, name);
        let meta = path.join(format!(".tendr/sessions/{namespace}/{name}/meta.json"));
        let meta: serde_json::Value =
            serde_json::from_slice(&std::fs::read(meta).unwrap()).unwrap();
        sidecars.push(meta["sidecar"]["pid"].as_u64().expect("sidecar.pid"));
    }

    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _home = home;
        panic!("the test body fails before its cleanup");
    }));
    assert!(unwound.is_err(), "the closure must have panicked");

    // Each sidecar has let go of its session; it is gone soon after.
    for pid in sidecars {
        poll_until(HANG, std::time::Duration::from_millis(10), || {
            if running(pid) {
                Observation::Pending(format!("sidecar {pid} is running"))
            } else {
                Observation::Ready(())
            }
        })
        .unwrap_or_else(|e| panic!("the drop must stop sidecar {pid}: {e}"));
    }
    assert!(!path.exists(), "nothing recreated {}", path.display());
}

#[cfg(unix)]
fn running(pid: u64) -> bool {
    harness::process_running(pid as i32)
}

#[cfg(windows)]
fn running(pid: u64) -> bool {
    harness::process_alive(pid as u32)
}
