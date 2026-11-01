//! `tendr skill {print, install, path}` — manage the embedded agent skill stub.
//!
//! The stub's single source of truth is `src/embedded/SKILL.md`, embedded at
//! build time. `install` lands it as a Claude Code skill file so an agent picks
//! up the bootstrap rules; the command is idempotent when the on-disk copy
//! already matches and refuses to clobber a user-edited file without `--force`.
//! `path` reports where install would write, with no filesystem side effects.

use std::path::{Path, PathBuf};

use anyhow::Context;

/// The canonical skill stub, embedded from the repo's source of truth.
const SKILL_STUB: &str = include_str!("../embedded/SKILL.md");

/// Install location relative to the chosen root (the project cwd, or `$HOME`
/// with `--global`).
const SKILL_SUBPATH: &str = ".claude/skills/using-tendr/SKILL.md";

/// Print the embedded skill stub to stdout.
pub fn cmd_skill_print() -> anyhow::Result<()> {
    out!("{SKILL_STUB}");
    if !SKILL_STUB.ends_with('\n') {
        outln!();
    }
    Ok(())
}

/// Print where `install` would write, without touching the filesystem.
pub fn cmd_skill_path(global: bool) -> anyhow::Result<()> {
    outln!("{}", skill_path(global)?.display());
    Ok(())
}

/// Install the embedded stub to the skill file. When the on-disk file is already
/// byte-identical to the canonical stub the call is a no-op success ("already
/// installed"). When it exists but differs, the call refuses (exit 2) unless
/// `force` is set. Parent directories are created as needed.
pub fn cmd_skill_install(global: bool, force: bool) -> anyhow::Result<()> {
    let path = skill_path(global)?;

    if path.exists() {
        let existing = std::fs::read(&path)
            .with_context(|| format!("reading existing skill file {}", path.display()))?;
        if existing == SKILL_STUB.as_bytes() {
            outln!("already installed: {}", path.display());
            return Ok(());
        }
        if !force {
            // Never silently overwrite a file a user has edited: exit 2 (usage
            // error) and name the path plus the escape hatch.
            eprintln!(
                "refusing to overwrite modified skill file: {}\n\
                 it differs from the canonical stub — re-run with --force to replace it",
                path.display()
            );
            std::process::exit(2);
        }
    }

    write_stub(&path)?;
    outln!("installed: {}", path.display());
    Ok(())
}

/// Resolve the install path. `global` roots it at the home directory
/// ([`tendr::session::home_dir`], as for the session state); otherwise at the
/// current working directory.
fn skill_path(global: bool) -> anyhow::Result<PathBuf> {
    let root = if global {
        tendr::session::home_dir()?
    } else {
        std::env::current_dir().context("resolving current working directory")?
    };
    Ok(root.join(SKILL_SUBPATH))
}

/// Create the skill file's parent directories and write the canonical stub.
fn write_stub(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating skill directory {}", parent.display()))?;
    }
    std::fs::write(path, SKILL_STUB)
        .with_context(|| format!("writing skill file {}", path.display()))?;
    Ok(())
}
