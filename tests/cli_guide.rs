//! `tendr guide [TOPIC]` — the self-documenting usage guide.
//!
//! The guide is `docs/guide.md`, embedded in the binary. `tendr guide` prints
//! the whole thing; `tendr guide <topic>` slices out one section via a
//! topic→heading registry. An unknown topic is a usage error (exit 2) that lists
//! the available topics.

mod harness;

use harness::tendr;
use predicates::prelude::*;
use tempfile::TempDir;

#[test]
fn version_reports_package_version() {
    let root = TempDir::new().unwrap();
    let expected = format!("tendr {}\n", env!("CARGO_PKG_VERSION"));

    tendr(&root)
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::eq(expected));
}

#[test]
fn guide_prints_the_whole_guide() {
    let root = TempDir::new().unwrap();

    // The top-level title and a section from the far end both appear only when
    // the entire document is emitted.
    tendr(&root)
        .args(["guide"])
        .assert()
        .success()
        .stdout(predicate::str::contains("# Tendr Guide"))
        .stdout(predicate::str::contains("Reach remote hosts"))
        .stdout(predicate::str::contains("Record where a session runs"));
}

#[test]
fn guide_exec_prints_only_the_exec_section() {
    let root = TempDir::new().unwrap();

    // The exec section carries its own argv rule; it must NOT bleed into the
    // remote or REPL sections that follow it.
    tendr(&root)
        .args(["guide", "exec"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Drive it with"))
        .stdout(predicate::str::contains("takes argv, not a shell snippet"))
        .stdout(predicate::str::contains("exec_truncated"))
        .stdout(predicate::str::contains("Reach remote hosts").not())
        .stdout(predicate::str::contains("Record where a session runs").not());
}

#[test]
fn guide_remote_includes_the_frame_subsection() {
    let root = TempDir::new().unwrap();

    // The remote section runs to the next top-level heading, so its nested
    // frame-from-stdin subsection rides along.
    tendr(&root)
        .args(["guide", "remote"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Reach remote hosts"))
        .stdout(predicate::str::contains("frame-from-stdin"))
        .stdout(predicate::str::contains("default ssh shell"))
        .stdout(predicate::str::contains("Drive it with").not());
}

#[test]
fn guide_python_section_is_relevant() {
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["guide", "python"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Python"))
        .stdout(predicate::str::contains("persist between calls"))
        .stdout(predicate::str::contains("Reach remote hosts").not());
}

#[test]
fn guide_duckdb_section_is_relevant() {
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["guide", "duckdb"])
        .assert()
        .success()
        .stdout(predicate::str::contains("DuckDB"))
        .stdout(predicate::str::contains("Python").not());
}

#[test]
fn guide_install_section_is_relevant() {
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["guide", "install"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Install and build"))
        .stdout(predicate::str::contains("cargo install tendr"))
        .stdout(predicate::str::contains("cargo-zigbuild"));
}

#[test]
fn guide_boundary_section_is_relevant() {
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["guide", "boundary"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--boundary-parent"))
        .stdout(predicate::str::contains("Query the event log").not());
}

#[test]
fn guide_cgroup_section_is_relevant() {
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["guide", "cgroup"])
        .assert()
        .success()
        .stdout(predicate::str::contains("tendr.slice"))
        .stdout(predicate::str::contains("Record where a session runs").not());
}

#[test]
fn guide_herdr_section_is_relevant() {
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["guide", "herdr"])
        .assert()
        .success()
        .stdout(predicate::str::contains("herdr pane read"))
        .stdout(predicate::str::contains("## Exit codes").not());
}

#[test]
fn guide_topic_is_case_insensitive() {
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["guide", "EXEC"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Drive it with"));
}

#[test]
fn guide_unknown_topic_exits_2_and_lists_topics() {
    let root = TempDir::new().unwrap();

    tendr(&root)
        .args(["guide", "bogus"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("bogus"))
        .stderr(predicate::str::contains("exec"))
        .stderr(predicate::str::contains("install"))
        .stderr(predicate::str::contains("remote"))
        .stderr(predicate::str::contains("duckdb"));
}
