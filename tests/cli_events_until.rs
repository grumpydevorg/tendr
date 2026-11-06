//! `tendr events --follow` that ends on a match: `--max-count`, `--data` and
//! `--timeout`. A scripted producer uses agent-shaped payloads to exercise
//! arbitrary application events; only the test caller interprets their
//! meaning. The generic event transport must correlate requests, ignore
//! unrelated or stale results, and bound a wait without cancelling the producer.

mod harness;
use harness::Home;

use harness::{DeadlineAssertExt, ReadyFollower, read_events, tendr, test_callback_bin};

/// Start `cc`, a scripted agent that fires one hook per line pushed to it.
fn start_agent(root: &Home) {
    let tendr_bin = assert_cmd::cargo::cargo_bin("tendr");
    tendr(root)
        .args(["start", "cc", "--stdin", "--"])
        .arg(test_callback_bin())
        .arg("hook-agent")
        .arg(tendr_bin)
        .assert_within_deadline()
        .success();
    harness::wait_running(root, "cc");
}

/// Fire hooks in the agent, one `<hook> <json>` per line.
fn fire(root: &Home, hooks: &[(&str, serde_json::Value)]) {
    let lines: String = hooks
        .iter()
        .map(|(hook, data)| format!("{hook} {data}\n"))
        .collect();
    tendr(root)
        .args(["push", "cc"])
        .write_stdin(lines)
        .assert_within_deadline()
        .success();
}

/// Start a bounded event follower before triggering its producer.
fn wait_for(root: &Home, args: &[&str]) -> ReadyFollower {
    let mut all = vec!["--follow", "--from-now", "--session", "default/cc"];
    all.extend_from_slice(args);
    ReadyFollower::spawn(root, "events", &all)
}

/// Two turns in one session, each waited for by its own `prompt_id`: the
/// first ignores another prompt's stop and ends at exactly its Nth match;
/// the second ends on `hook.stop_failure`, which `--kind hook.stop` matches
/// by prefix.
#[test]
fn turn_waits_end_on_their_own_prompt_id() {
    let root = Home::new();
    start_agent(&root);

    let turn1 = wait_for(
        &root,
        &[
            "--kind",
            "hook.",
            "--data",
            "prompt_id=p1",
            "--max-count",
            "2",
        ],
    );
    fire(
        &root,
        &[
            ("user_prompt_submit", serde_json::json!({"prompt_id": "p1"})),
            (
                "stop",
                serde_json::json!({"prompt_id": "p0", "last_assistant_message": "not yours"}),
            ),
            (
                "stop",
                serde_json::json!({"prompt_id": "p1", "last_assistant_message": "one"}),
            ),
            // A third match: the waiter must already have ended at two.
            ("notification", serde_json::json!({"prompt_id": "p1"})),
        ],
    );
    let (code, records) = turn1.wait_exit();
    assert_eq!(code, Some(0), "records: {records:?}");
    let seen: Vec<(&str, &str)> = records
        .iter()
        .map(|r| {
            (
                r["kind"].as_str().unwrap(),
                r["data"]["last_assistant_message"].as_str().unwrap_or(""),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [("hook.user_prompt_submit", ""), ("hook.stop", "one")],
        "exactly the turn's submit and its stop"
    );

    let turn2 = wait_for(
        &root,
        &[
            "--kind",
            "hook.stop",
            "--data",
            "prompt_id=p2",
            "--max-count",
            "1",
        ],
    );
    fire(
        &root,
        &[
            ("user_prompt_submit", serde_json::json!({"prompt_id": "p2"})),
            (
                "stop_failure",
                serde_json::json!({"prompt_id": "p2", "error": "rate_limit"}),
            ),
        ],
    );
    let (code, records) = turn2.wait_exit();
    assert_eq!(code, Some(0), "records: {records:?}");
    assert_eq!(records.len(), 1, "records: {records:?}");
    assert_eq!(records[0]["kind"], "hook.stop_failure");
    assert_eq!(records[0]["data"]["error"], "rate_limit");
}

/// A payload over the 16 KiB inline cap is stored as a preview without its
/// fields, so `--data` cannot match it (the documented rule); the projected
/// payload the recipe emits does.
#[test]
fn spilled_payload_does_not_match_its_field() {
    let root = Home::new();
    start_agent(&root);

    let waiter = wait_for(
        &root,
        &[
            "--kind",
            "hook.stop",
            "--data",
            "prompt_id=p3",
            "--max-count",
            "1",
        ],
    );
    fire(
        &root,
        &[
            (
                "stop",
                serde_json::json!({"prompt_id": "p3", "last_assistant_message": "x".repeat(20_000)}),
            ),
            (
                "stop",
                serde_json::json!({"prompt_id": "p3", "last_assistant_message": "projected"}),
            ),
        ],
    );
    let (code, records) = waiter.wait_exit();
    assert_eq!(code, Some(0), "records: {records:?}");
    assert_eq!(records.len(), 1, "records: {records:?}");
    assert_eq!(records[0]["data"]["last_assistant_message"], "projected");

    // The raw payload was written, and spilled, before the one that matched.
    let stops: Vec<_> = read_events(&root, "cc")
        .into_iter()
        .filter(|e| e["kind"] == "hook.stop")
        .collect();
    assert_eq!(stops.len(), 2, "stops: {stops:?}");
    assert_eq!(stops[0]["truncated"], true, "the first stop spilled");
}

/// An end-of-turn event written before a `--from-now` waiter started is
/// history: it must not end the wait, even with a matching `prompt_id`.
#[test]
fn event_before_from_now_waiter_does_not_match() {
    let root = Home::new();
    start_agent(&root);
    fire(
        &root,
        &[(
            "stop",
            serde_json::json!({"prompt_id": "p1", "last_assistant_message": "stale"}),
        )],
    );
    harness::wait_event_kind(&root, "cc", "hook.stop");

    let waiter = wait_for(
        &root,
        &[
            "--kind",
            "hook.stop",
            "--data",
            "prompt_id=p1",
            "--max-count",
            "1",
        ],
    );
    fire(
        &root,
        &[(
            "stop",
            serde_json::json!({"prompt_id": "p1", "last_assistant_message": "fresh"}),
        )],
    );
    let (code, records) = waiter.wait_exit();
    assert_eq!(code, Some(0), "records: {records:?}");
    assert_eq!(records.len(), 1, "records: {records:?}");
    assert_eq!(records[0]["data"]["last_assistant_message"], "fresh");
}

/// A turn that never ends (an interrupt fires no hook) ends the wait at its
/// deadline with 124 and nothing printed. The agent here never emits, so only
/// the deadline can end the wait; that timer is the behaviour under test.
#[test]
fn wait_with_no_end_exits_124_at_its_deadline() {
    let root = Home::new();
    start_agent(&root);

    let waiter = wait_for(
        &root,
        &[
            "--kind",
            "hook.stop",
            "--data",
            "prompt_id=p1",
            "--max-count",
            "1",
            "--timeout",
            "1s",
        ],
    );
    let (code, records) = waiter.wait_exit();
    assert_eq!(code, Some(124));
    assert!(records.is_empty(), "records: {records:?}");
}
