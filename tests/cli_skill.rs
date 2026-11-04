//! `tendr skill {print, install, path}` — manage the embedded agent skill stub.
//!
//! The stub's single source of truth is `src/embedded/SKILL.md`, embedded in the
//! binary. `print` emits it, `install` lands it as a Claude Code skill file
//! (project-local by default, `--global` under `$HOME`), and `path` reports where
//! install would write. Install is idempotent and refuses to clobber a
//! user-edited file without `--force`.

mod harness;
use harness::Home;

use harness::tendr;
use predicates::prelude::*;
use tempfile::TempDir;

/// Project-relative skill file that `install` writes by default.
const SKILL_REL: &str = ".claude/skills/using-tendr/SKILL.md";

#[test]
fn skill_print_emits_frontmatter_and_bootstrap_rules() {
    let root = Home::new();

    tendr(&root)
        .args(["skill", "print"])
        .assert()
        .success()
        // frontmatter identity
        .stdout(predicate::str::contains("name: using-tendr"))
        // the three bootstrap rules
        .stdout(predicate::str::contains("takes argv, not a shell string"))
        .stdout(predicate::str::contains("exit_code"))
        .stdout(predicate::str::contains("one in-flight `exec` per session"))
        .stdout(predicate::str::contains("tendr guide install"))
        .stdout(predicate::str::contains("tendr guide powershell"))
        .stdout(predicate::str::contains("tendr guide boundary"));
}

#[test]
fn skill_install_writes_project_local_file() {
    let home = Home::new();
    let project = TempDir::new().unwrap();
    let target = project.path().join(SKILL_REL);

    tendr(&home)
        .current_dir(project.path())
        .args(["skill", "install"])
        .assert()
        .success()
        .stdout(predicate::str::contains(target.display().to_string()));

    let written = std::fs::read_to_string(&target).expect("skill file written");
    assert!(
        written.contains("name: using-tendr"),
        "installed file carries the canonical stub"
    );
}

#[test]
fn skill_install_is_idempotent() {
    let home = Home::new();
    let project = TempDir::new().unwrap();

    // First install writes the file.
    tendr(&home)
        .current_dir(project.path())
        .args(["skill", "install"])
        .assert()
        .success();

    // Second install sees byte-identical content and reports "already installed"
    // without failing.
    tendr(&home)
        .current_dir(project.path())
        .args(["skill", "install"])
        .assert()
        .success()
        .stdout(predicate::str::contains("already installed"));
}

#[test]
fn skill_install_refuses_to_clobber_without_force() {
    let home = Home::new();
    let project = TempDir::new().unwrap();
    let target = project.path().join(SKILL_REL);

    tendr(&home)
        .current_dir(project.path())
        .args(["skill", "install"])
        .assert()
        .success();

    // A user edits the installed file.
    std::fs::write(&target, "# my own edits\n").unwrap();

    // A plain install must not overwrite it: usage error, exit 2, path named.
    tendr(&home)
        .current_dir(project.path())
        .args(["skill", "install"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(target.display().to_string()));

    // The user's edits survive.
    let after = std::fs::read_to_string(&target).unwrap();
    assert_eq!(after, "# my own edits\n", "refusal left the file untouched");

    // --force overwrites unconditionally, restoring the canonical stub.
    tendr(&home)
        .current_dir(project.path())
        .args(["skill", "install", "--force"])
        .assert()
        .success();
    let forced = std::fs::read_to_string(&target).unwrap();
    assert!(
        forced.contains("name: using-tendr"),
        "--force restored the canonical stub"
    );
}

#[test]
fn skill_install_global_writes_under_home() {
    let home = Home::new();
    let project = TempDir::new().unwrap();
    let target = home.path().join(SKILL_REL);

    // cwd is the project dir, but --global must resolve $HOME, not cwd.
    tendr(&home)
        .current_dir(project.path())
        .args(["skill", "install", "--global"])
        .assert()
        .success()
        .stdout(predicate::str::contains(target.display().to_string()));

    assert!(target.exists(), "global install landed under $HOME");
    assert!(
        !project.path().join(SKILL_REL).exists(),
        "global install did not touch the project tree"
    );
}

#[test]
fn skill_path_prints_project_target_without_writing() {
    let home = Home::new();
    let project = TempDir::new().unwrap();
    let target = project.path().join(SKILL_REL);

    tendr(&home)
        .current_dir(project.path())
        .args(["skill", "path"])
        .assert()
        .success()
        .stdout(predicate::str::contains(target.display().to_string()));

    assert!(
        !target.exists(),
        "path is a pure query — it must not create anything"
    );
}

#[test]
fn skill_path_global_prints_home_target() {
    let home = Home::new();
    let project = TempDir::new().unwrap();
    let target = home.path().join(SKILL_REL);

    tendr(&home)
        .current_dir(project.path())
        .args(["skill", "path", "--global"])
        .assert()
        .success()
        .stdout(predicate::str::contains(target.display().to_string()));

    assert!(!target.exists(), "path --global must not create anything");
}
