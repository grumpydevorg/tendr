//! `tendr start --boundary` / `--boundary-parent` — the boundary descriptor
//! is recorded on the session's LaunchSpec and surfaced by `status`.
//! See docs/guide.md ("Record where a session runs").

mod harness;

use harness::{read_events, tendr, wait_terminal};
use serde_json::Value;
use tempfile::TempDir;

/// The `data` payload of the first event of the given kind, if any.
fn event_data<'a>(events: &'a [Value], kind: &str) -> Option<&'a Value> {
    events
        .iter()
        .find(|e| e["kind"] == kind)
        .map(|e| &e["data"])
}

/// Run `start` with the given extra args after the session name and return the
/// parsed meta JSON it prints to stdout on success.
fn start_ok(root: &TempDir, session: &str, extra: &[&str]) -> Value {
    let mut args = vec!["start", session];
    args.extend_from_slice(extra);
    args.extend_from_slice(&["--", "echo", "hi"]);
    let out = tendr(root).args(&args).assert().success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    serde_json::from_str(&stdout).expect("start prints meta JSON")
}

#[test]
fn start_records_boundary_on_launch_spec() {
    let root = TempDir::new().unwrap();
    let meta = start_ok(&root, "job", &["--boundary", "host:data-box"]);
    let b = &meta["launch_spec"]["boundary"];
    assert_eq!(b["current"]["kind"], "host");
    assert_eq!(b["current"]["label"], "data-box");
    assert_eq!(
        b["current"].get("parents"),
        None,
        "current is a Boundary, not a context"
    );
    assert_eq!(b["parents"].as_array().unwrap().len(), 0);
}

#[test]
fn boundary_parent_builds_ancestry() {
    let root = TempDir::new().unwrap();
    let meta = start_ok(
        &root,
        "dev",
        &[
            "--boundary",
            "container:my-image:latest",
            "--boundary-parent",
            "host:data-box",
        ],
    );
    let b = &meta["launch_spec"]["boundary"];
    assert_eq!(b["current"]["kind"], "container");
    // First colon splits kind from label; the tag colon survives.
    assert_eq!(b["current"]["label"], "my-image:latest");
    assert_eq!(b["parents"][0]["kind"], "host");
    assert_eq!(b["parents"][0]["label"], "data-box");
}

#[test]
fn start_without_boundary_omits_it() {
    let root = TempDir::new().unwrap();
    let meta = start_ok(&root, "plain", &[]);
    assert!(
        meta["launch_spec"].get("boundary").is_none(),
        "no --boundary should leave the field absent"
    );
}

#[test]
fn status_surfaces_boundary() {
    let root = TempDir::new().unwrap();
    start_ok(&root, "job", &["--boundary", "vm:builder-1"]);
    wait_terminal(&root, "job");

    let out = tendr(&root).args(["status", "job"]).assert().success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let meta: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(meta["launch_spec"]["boundary"]["current"]["kind"], "vm");
    assert_eq!(
        meta["launch_spec"]["boundary"]["current"]["label"],
        "builder-1"
    );
}

#[test]
fn invalid_boundary_is_rejected() {
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args(["start", "bad", "--boundary", "host", "--", "echo", "hi"])
        .assert()
        .failure();
}

#[test]
fn boundary_parent_requires_boundary() {
    let root = TempDir::new().unwrap();
    tendr(&root)
        .args([
            "start",
            "bad",
            "--boundary-parent",
            "host:x",
            "--",
            "echo",
            "hi",
        ])
        .assert()
        .failure();
}

// --- history snapshot on lifecycle events ---

#[test]
fn run_starting_and_started_carry_boundary_snapshot() {
    let root = TempDir::new().unwrap();
    start_ok(&root, "job", &["--boundary", "container:img:latest"]);
    wait_terminal(&root, "job");
    let events = read_events(&root, "job");

    for kind in ["run.starting", "run.started"] {
        let data = event_data(&events, kind).unwrap_or_else(|| panic!("{kind} event missing"));
        assert_eq!(
            data["boundary"]["current"]["kind"], "container",
            "{kind} must carry the boundary snapshot: {data}"
        );
        assert_eq!(data["boundary"]["current"]["label"], "img:latest");
    }
}

#[test]
fn terminal_events_do_not_carry_boundary_snapshot() {
    let root = TempDir::new().unwrap();
    start_ok(&root, "job", &["--boundary", "host:data-box"]);
    wait_terminal(&root, "job");
    let events = read_events(&root, "job");

    let terminal = event_data(&events, "run.exited").expect("run.exited event missing");
    assert!(
        terminal.get("boundary").is_none(),
        "terminal events join to the lifecycle snapshot on run_id, they don't re-carry it: {terminal}"
    );
}

#[test]
fn no_declared_boundary_means_no_snapshot() {
    let root = TempDir::new().unwrap();
    start_ok(&root, "plain", &[]);
    wait_terminal(&root, "plain");
    let events = read_events(&root, "plain");

    let starting = event_data(&events, "run.starting").expect("run.starting event missing");
    assert!(
        starting.get("boundary").is_none(),
        "no --boundary should stamp no snapshot: {starting}"
    );
}
