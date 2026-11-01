use std::path::Path;

use anyhow::Context;
use tendr::model::boundary::BoundaryContext;
use tendr::model::ids::{Namespace, SessionName};
use tendr::model::spec::{ExecTarget, IoMode, LaunchSpec, StdinMode};
use tendr::platform::{Current, Platform};
use tendr::session::{self, SessionRoot};

#[allow(clippy::too_many_arguments)] // launch surface; bundled into StartRequest by the frame-transport work
pub fn cmd_start(
    name: &str,
    cmd: Vec<String>,
    stdin: bool,
    replace: bool,
    timeout: Option<u64>,
    cwd: Option<&std::path::Path>,
    env_vars: &[String],
    unset_env: &[String],
    on_exit: &[String],
    after: &[String],
    any_exit: bool,
    namespace: &Namespace,
    pty: bool,
    exec_target: Option<ExecTarget>,
    boundary: Option<BoundaryContext>,
) -> anyhow::Result<()> {
    let (meta, _session) = launch_session(
        name,
        cmd,
        stdin,
        replace,
        timeout,
        cwd,
        env_vars,
        unset_env,
        on_exit,
        after,
        any_exit,
        namespace,
        pty,
        exec_target,
        boundary,
    )?;

    let json = serde_json::to_string_pretty(&meta)?;
    println!("{json}");

    // Exit non-zero if the run already ended in failure — agents branch on
    // exit code (the same codes as `wait`).
    match meta.status() {
        tendr::model::state::RunStatus::SpawnFailed { .. } => std::process::exit(2),
        tendr::model::state::RunStatus::Exited {
            how: tendr::model::state::ExitReason::SidecarFailed { .. },
            ..
        } => std::process::exit(5),
        _ => {}
    }

    Ok(())
}

/// Launch a session: create directory, spawn sidecar, wait for readiness.
///
/// Returns the Meta snapshot and SessionDir. For new sessions, the Meta comes
/// from the sidecar's ready signal. For idempotent starts (already running with
/// the same spec), it re-reads the existing meta from disk.
///
/// Does not print to stdout or call process::exit — callers decide what to do.
#[allow(clippy::too_many_arguments)] // launch surface; bundled into StartRequest by the frame-transport work
pub(crate) fn launch_session(
    name: &str,
    cmd: Vec<String>,
    stdin: bool,
    replace: bool,
    timeout: Option<u64>,
    cwd: Option<&std::path::Path>,
    env_vars: &[String],
    unset_env: &[String],
    on_exit: &[String],
    after: &[String],
    any_exit: bool,
    namespace: &Namespace,
    pty: bool,
    exec_target: Option<ExecTarget>,
    boundary: Option<BoundaryContext>,
) -> anyhow::Result<(tendr::model::meta::Meta, session::SessionDir)> {
    let session_name = SessionName::new(name)?;
    let root = SessionRoot::default_path()?;

    // Handle --replace before session creation
    let next_generation = if replace {
        handle_replace(&root, namespace, &session_name)?
    } else {
        None
    };

    // Build launch spec
    let mut launch_spec = LaunchSpec::new(cmd)?;
    launch_spec.stdin_mode = if stdin {
        StdinMode::Pipe
    } else {
        StdinMode::None
    };
    launch_spec.timeout_s = timeout;
    launch_spec.namespace = Some(namespace.as_str().to_string());
    launch_spec.cwd = cwd.map(Path::to_path_buf);
    for entry in env_vars {
        let (key, value) = entry
            .split_once('=')
            .with_context(|| format!("invalid --env format: expected KEY=VALUE, got: {entry}"))?;
        anyhow::ensure!(
            !key.is_empty(),
            "invalid --env format: key cannot be empty, got: {entry}"
        );
        launch_spec.env.insert(key.to_string(), value.to_string());
    }
    for name in unset_env {
        anyhow::ensure!(
            !name.is_empty(),
            "invalid --unset-env: name cannot be empty"
        );
        anyhow::ensure!(
            !name.contains('='),
            "invalid --unset-env: name cannot contain '=', got: {name}"
        );
        launch_spec.unset_env.insert(name.clone());
    }
    launch_spec.on_exit = on_exit.to_vec();
    launch_spec.boundary = boundary;
    if pty {
        launch_spec.io_mode = IoMode::Pty;
    }
    launch_spec.exec_target =
        exec_target.unwrap_or_else(|| infer_exec_target(&launch_spec.argv()[0]));

    // Resolve --after: capture each dependency's run_id at bind time
    if !after.is_empty() {
        for dep_name in after {
            let dep_session_name = SessionName::new(dep_name)?;
            let dep_session = session::open(&root, namespace, &dep_session_name)?
                .ok_or_else(|| anyhow::anyhow!("--after: session not found: {dep_name}"))?;
            let dep_meta = session::read_meta(&dep_session)?;
            launch_spec
                .after
                .push(tendr::model::spec::DependencyBinding {
                    session: dep_session_name,
                    run_id: dep_meta.run_id(),
                });
        }
        launch_spec.after_any_exit = any_exit;
    }

    // Create session directory (with idempotent handling)
    let session = match session::create(&root, namespace, &session_name) {
        Ok(s) => s,
        Err(session::SessionError::AlreadyExists(_)) => {
            match try_idempotent_start(&root, namespace, &session_name, &launch_spec)? {
                None => {
                    // Idempotent: already running with same spec.
                    // Re-open and return existing meta.
                    let existing =
                        session::open(&root, namespace, &session_name)?.ok_or_else(|| {
                            anyhow::anyhow!("session vanished during idempotent check")
                        })?;
                    let meta = session::read_meta(&existing)?;
                    return Ok((meta, existing));
                }
                Some(s) => s, // orphan cleaned, session re-created
            }
        }
        Err(e) => return Err(e.into()),
    };

    // Write generation hint for sidecar to pick up
    if let Some(next_gen) = next_generation {
        let _ = std::fs::write(
            session.path().join("generation"),
            next_gen.as_u64().to_string(),
        );
    }

    let meta = spawn_and_wait_ready_inner(&session, &launch_spec)?;
    Ok((meta, session))
}

/// Handle `--replace`: kill any existing session and remove its directory.
/// Returns `Some(next_generation)` when an existing session was replaced,
/// `None` when no session existed or only an orphan was cleaned.
fn handle_replace(
    root: &SessionRoot,
    namespace: &Namespace,
    session_name: &SessionName,
) -> anyhow::Result<Option<tendr::model::ids::Generation>> {
    let session_path = root
        .path()
        .join(namespace.as_str())
        .join(session_name.as_str());
    if session_path.exists() {
        if !session_path.join("meta.json").exists() {
            // Orphan dir -- just clean up, no generation to read
            super::status::cleanup_orphan_dir(&session_path);
        } else if let Some(existing) = session::open(root, namespace, session_name)? {
            let existing_meta = session::read_meta(&existing)?;
            let prev_generation = existing_meta.generation();

            if !existing_meta.status().is_terminal() {
                // Kill the child
                if let Some(child) = existing_meta.status().child() {
                    let _ = Current::kill_orphan(child, true);
                }
            }
            // Wait for the sidecar to write terminal state AND release the lock,
            // also when meta is already terminal: the sidecar writes it just
            // before releasing, and on Windows a directory it still holds open
            // cannot be removed (#91).
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let name = session_name.as_str();
            loop {
                if let Ok(m) = session::read_meta(&existing) {
                    if m.status().is_terminal() && !session::is_locked(&existing).unwrap_or(true) {
                        break; // Terminal + unlocked = safe to remove
                    }
                }
                if std::time::Instant::now() >= deadline {
                    anyhow::bail!(
                        "replace timed out: old sidecar for {name} did not exit within 10s"
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            // Safe to remove -- sidecar has exited or timed out
            std::fs::remove_dir_all(existing.path())?;
            return Ok(Some(prev_generation.next()));
        }
    }
    Ok(None)
}

/// Handle `AlreadyExists` when creating a session.
///
/// Returns:
/// - `Ok(None)` if idempotent return was already printed (caller should `return Ok(())`)
/// - `Ok(Some(session))` if orphan was cleaned and session was re-created
/// - `Err` for conflicts, terminal state, or Starting state
fn try_idempotent_start(
    root: &SessionRoot,
    namespace: &Namespace,
    session_name: &SessionName,
    launch_spec: &LaunchSpec,
) -> anyhow::Result<Option<session::SessionDir>> {
    let session_path = root
        .path()
        .join(namespace.as_str())
        .join(session_name.as_str());
    let name = session_name.as_str();

    // Orphan dir check: no meta.json means sidecar crashed before writing state
    if !session_path.join("meta.json").exists() {
        super::status::cleanup_orphan_dir(&session_path);
        let session = session::create(root, namespace, session_name)?;
        return Ok(Some(session));
    }

    let existing = session::open(root, namespace, session_name)?
        .ok_or_else(|| anyhow::anyhow!("session exists but not openable"))?;
    let existing_meta = session::read_meta(&existing)?;

    if matches!(
        existing_meta.status(),
        tendr::model::state::RunStatus::Running { .. }
    ) {
        // Running -- check spec match for idempotent return
        if existing_meta.launch_spec_hash() == launch_spec.canonical_hash() {
            Ok(None)
        } else {
            anyhow::bail!(
                "session conflict: {name} is running with a different launch spec (use --replace to override)"
            );
        }
    } else if existing_meta.status().is_terminal() {
        anyhow::bail!(
            "session already exists in terminal state: {name} (use --replace to restart)"
        );
    } else if matches!(
        existing_meta.status(),
        tendr::model::state::RunStatus::Starting
    ) {
        // Starting state: sidecar may be in dependency wait or still initializing.
        if session::is_locked(&existing).unwrap_or(false) {
            // Sidecar alive — check spec match for idempotent return
            if existing_meta.launch_spec_hash() == launch_spec.canonical_hash() {
                Ok(None) // Idempotent
            } else {
                anyhow::bail!(
                    "session conflict: {name} is starting with a different launch spec (use --replace to override)"
                );
            }
        } else {
            // Starting + unlocked = orphan
            super::status::cleanup_orphan_dir(&session_path);
            let session = session::create(root, namespace, session_name)?;
            Ok(Some(session))
        }
    } else {
        // Unreachable — all RunStatus variants covered above
        anyhow::bail!("session {name} is in unexpected state")
    }
}

/// Write launch spec, spawn sidecar, wait for readiness, return meta.
fn spawn_and_wait_ready_inner(
    session: &session::SessionDir,
    launch_spec: &LaunchSpec,
) -> anyhow::Result<tendr::model::meta::Meta> {
    // Write launch spec for sidecar to read
    let spec_json = serde_json::to_string_pretty(launch_spec)?;
    std::fs::write(session.path().join("launch_spec.json"), &spec_json)?;

    // Create readiness pipe
    let (read_end, write_end) = Current::ready_channel()?;

    // Spawn detached sidecar
    let tendr_bin = std::env::current_exe()?;
    let sidecar_result = Current::spawn_sidecar(&tendr_bin, session.path(), &write_end);

    // Close write end in parent -- we only read
    drop(write_end);

    if let Err(e) = sidecar_result {
        // Sidecar failed to spawn -- clean up session dir so start is retryable
        let _ = std::fs::remove_dir_all(session.path());
        anyhow::bail!("failed to spawn sidecar: {e}");
    }

    // Block until sidecar signals readiness
    let signal = match Current::read_ready_signal(read_end) {
        Ok(s) => s,
        Err(e) => {
            // Sidecar died before signaling. Only clean up if no child was spawned.
            // If child_pid exists, a child may be alive -- don't delete the evidence.
            if !session.path().join("child_pid").exists() {
                let _ = std::fs::remove_dir_all(session.path());
            }
            anyhow::bail!("sidecar startup failed: {e}");
        }
    };

    if signal.starts_with("ERROR:") {
        let err_msg = signal.trim_start_matches("ERROR:").trim();
        if !session.path().join("child_pid").exists() {
            let _ = std::fs::remove_dir_all(session.path());
        }
        anyhow::bail!("sidecar failed: {err_msg}");
    }

    // Sidecar sends "OK:<json>\n" -- parse the meta snapshot directly from pipe.
    let meta_json = signal
        .strip_prefix("OK:")
        .ok_or_else(|| anyhow::anyhow!("unexpected readiness signal: {signal}"))?
        .trim();

    let meta: tendr::model::meta::Meta = serde_json::from_str(meta_json)?;
    Ok(meta)
}

/// Infer exec target from argv[0] when --exec-target is not specified.
fn infer_exec_target(argv0: &str) -> ExecTarget {
    let name = std::path::Path::new(argv0)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(argv0);
    let lower = name.to_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    match stem {
        "bash" | "sh" | "zsh" | "dash" | "ash" => ExecTarget::PosixShell,
        "pwsh" | "powershell" => ExecTarget::PowerShell,
        "python" | "python3" | "ipython" | "py" => ExecTarget::PythonRepl,
        "duckdb" => ExecTarget::DuckDb,
        _ => ExecTarget::None,
    }
}
