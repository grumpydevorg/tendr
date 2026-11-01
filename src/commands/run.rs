use std::path::{Path, PathBuf};
use std::thread;

use anyhow::Context;
use tendr::directive::{self, Directives};
use tendr::log::{LogQuery, follow_log};
use tendr::model::ids::Namespace;
use tendr::model::state::{ExitReason, RunStatus};
use tendr::session;

#[allow(clippy::too_many_arguments)] // launch surface; bundled into a request DTO by the frame-transport work
pub fn cmd_run(
    script: &Path,
    args: Vec<String>,
    shell: Option<String>,
    detach: bool,
    foreground: bool,
    // CLI overrides for directives:
    namespace: Option<&Namespace>,
    timeout: Option<u64>,
    stdin: bool,
    replace: bool,
    cwd: Option<&Path>,
    env_vars: &[String],
    unset_env: &[String],
    on_exit: &[String],
    after: &[String],
    any_exit: bool,
) -> anyhow::Result<()> {
    // --- Validate script file ---
    anyhow::ensure!(script.exists(), "script not found: {}", script.display());
    anyhow::ensure!(script.is_file(), "not a file: {}", script.display());

    // --- Parse directives ---
    let content = std::fs::read_to_string(script)
        .with_context(|| format!("failed to read script: {}", script.display()))?;
    let directives = directive::parse_directives(&content)
        .with_context(|| format!("failed to parse directives in {}", script.display()))?;

    // --- Resolve session name ---
    let session_name = if let Some(ref name) = directives.session {
        name.clone()
    } else {
        directive::derive_session_name(script).map_err(|e| anyhow::anyhow!("{e}"))?
    };

    // --- Resolve shell and build argv ---
    let script_path = std::fs::canonicalize(script).unwrap_or_else(|_| script.to_path_buf());
    let script_str = script_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("script path is not valid UTF-8"))?;

    let cmd = resolve_shell_argv(shell.as_deref(), script_str, &script_path, &content, args)?;

    // --- Merge directives with CLI overrides (CLI wins) ---
    let effective = merge_directives_with_cli(
        &directives,
        namespace,
        timeout,
        stdin,
        replace,
        cwd,
        env_vars,
        unset_env,
        on_exit,
        after,
        any_exit,
    );

    // --foreground overrides #tendr: detach. --detach forces detach.
    // Without either CLI flag, honor the directive.
    let effective_detach = if foreground {
        false
    } else if detach {
        true
    } else {
        directives.detach
    };

    // --- Launch session ---
    let (meta, session) = super::start::launch_session(
        &session_name,
        cmd,
        effective.stdin,
        effective.replace,
        effective.timeout,
        effective.cwd.as_deref(),
        &effective.env_vars,
        unset_env, // CLI-only, no directive equivalent
        &effective.on_exit,
        &effective.after,
        effective.any_exit,
        &effective.namespace,
        false, // pty not supported via `run`
        Some(tendr::model::spec::ExecTarget::None),
        None, // `run` does not expose --boundary
    )?;

    // Check for spawn failure.
    if matches!(meta.status(), RunStatus::SpawnFailed { .. }) {
        eprintln!("error: child failed to spawn");
        std::process::exit(2);
    }

    // --- Detached mode: print JSON and return ---
    if effective_detach {
        let json = serde_json::to_string_pretty(&meta)?;
        outln!("{json}");
        return Ok(());
    }

    // --- Foreground mode: follow log + wait for terminal state ---
    foreground_wait(&session)
}

/// Foreground mode: follow log until terminal, then propagate exit code.
///
/// The follow thread checks meta.json for terminal state on each poll cycle.
/// When it sees terminal state, it drains remaining output and exits. The main
/// thread then reads the final meta.json to extract the exit code.
///
/// This avoids the race where a separate terminal-state detector signals stop
/// before the sidecar has flushed its last log lines.
fn foreground_wait(session: &session::SessionDir) -> anyhow::Result<()> {
    let log_path = session.path().join("output.log");
    let meta_path = session.path().join("meta.json");
    let meta_path_clone = meta_path.clone();

    // The follow thread handles both output streaming and terminal detection.
    // Its should_stop closure reads meta.json directly — same as cmd_log --follow.
    // This means the follow thread exits only after it has drained all output
    // written before the terminal state, avoiding the flush race.
    let follow_handle = thread::spawn(move || {
        let query = LogQuery {
            tail: None,
            since_us: Some(0), // Show all output from the beginning, don't seek to EOF.
            raw: true,
        };
        let should_stop = || {
            std::fs::read_to_string(&meta_path_clone)
                .ok()
                .and_then(|c| serde_json::from_str::<tendr::model::meta::Meta>(&c).ok())
                .map(|m| m.status().is_terminal())
                .unwrap_or(false)
        };
        let mut stdout = std::io::stdout().lock();
        let _ = follow_log(&log_path, &query, &mut stdout, should_stop);
    });

    // Wait for the follow thread to finish (it exits when terminal + no more data).
    let _ = follow_handle.join();

    // Now read the final meta.json for exit code. The follow thread already
    // confirmed terminal state, so this read should succeed.
    let content = std::fs::read_to_string(&meta_path)?;
    let mut meta: tendr::model::meta::Meta = serde_json::from_str(&content)?;

    // Reconciliation: if the sidecar crashed between the follow thread's last
    // check and now (unlikely but possible). Best-effort — heals from the
    // event log or infers SidecarLost (spec §3.6).
    let _ = tendr::reconcile::reconcile_sidecar_gone(session, &mut meta);

    // Propagate exit code.
    match meta.status() {
        RunStatus::Exited { how, .. } => match how {
            ExitReason::ExitedOk => Ok(()),
            ExitReason::ExitedError { code } => std::process::exit(code.get()),
            ExitReason::Killed | ExitReason::KilledForced => std::process::exit(137),
            ExitReason::TimedOut => std::process::exit(124),
            ExitReason::SidecarFailed { .. } => std::process::exit(5),
        },
        RunStatus::SpawnFailed { .. } => std::process::exit(2),
        RunStatus::SidecarLost { .. } => std::process::exit(3),
        RunStatus::DependencyFailed { reason, .. } => {
            use tendr::model::dep_fail::DepFailReason;
            match reason {
                DepFailReason::Failed => std::process::exit(4),
                DepFailReason::TimedOut => std::process::exit(124),
                DepFailReason::Killed | DepFailReason::KilledForced => std::process::exit(137),
            }
        }
        _ => Ok(()),
    }
}

/// Map a file extension to the interpreter argv prefix.
/// Returns None for unmapped extensions.
fn launcher_argv_for_extension(ext: &str) -> Option<Vec<String>> {
    let argv: &[&str] = match ext {
        "sh" => &["bash"],
        "ps1" => &["pwsh", "-File"],
        #[cfg(windows)]
        "bat" | "cmd" => &["cmd", "/c"],
        #[cfg(not(windows))]
        "bat" | "cmd" => return None,
        "py" => {
            if cfg!(windows) {
                &["py", "-3"]
            } else {
                &["python3"]
            }
        }
        "rb" => &["ruby"],
        "js" => &["node"],
        _ => return None,
    };
    Some(argv.iter().map(|s| s.to_string()).collect())
}

/// Resolve the child command argv.
///
/// Precedence: `--shell` > executable > extension mapping > shebang (Unix) > error.
fn resolve_shell_argv(
    shell: Option<&str>,
    script_str: &str,
    script_path: &Path,
    script_content: &str,
    args: Vec<String>,
) -> anyhow::Result<Vec<String>> {
    let mut cmd = Vec::new();

    if let Some(sh) = shell {
        // 1. Explicit --shell override
        cmd.push(sh.to_string());
        cmd.push(script_str.to_string());
    } else if is_executable(script_path) {
        // 2. Directly executable (+x on Unix, .exe on Windows)
        cmd.push(script_str.to_string());
    } else if let Some(launcher) = script_path
        .extension()
        .and_then(|e| e.to_str())
        .and_then(|e| launcher_argv_for_extension(&e.to_ascii_lowercase()))
    {
        // 3. Extension mapping
        cmd.extend(launcher);
        cmd.push(script_str.to_string());
    } else if let Some(interp) = parse_shebang(script_content) {
        // 4. Shebang (Unix only)
        cmd.push(interp);
        cmd.push(script_str.to_string());
    } else {
        // 5. Hard fail
        anyhow::bail!(
            "cannot determine interpreter for '{}'\n  \
             hint: use --shell to specify the interpreter\n  \
             example: tendr run --shell bash {}",
            script_path.display(),
            script_path.display(),
        );
    }

    cmd.extend(args);
    Ok(cmd)
}

/// Extract the interpreter from a shebang line.
/// Handles `#!/path/to/interp` and `#!/usr/bin/env interp`.
/// Returns a single interpreter token — args are not preserved.
#[cfg(unix)]
fn parse_shebang(content: &str) -> Option<String> {
    let first_line = content.lines().next()?;
    let shebang = first_line.strip_prefix("#!")?.trim();
    if shebang.is_empty() {
        return None;
    }
    let parts: Vec<&str> = shebang.split_whitespace().collect();
    if parts.len() >= 2 && parts[0].ends_with("/env") {
        Some(parts[1].to_string())
    } else {
        Some(parts[0].to_string())
    }
}

#[cfg(windows)]
fn parse_shebang(_content: &str) -> Option<String> {
    None
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(windows)]
fn is_executable(path: &Path) -> bool {
    // Only .exe is directly executable. .bat/.cmd use the extension
    // mapping (cmd /c) so they go through launcher_argv_for_extension.
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e == "exe")
        .unwrap_or(false)
}

/// Merged effective options (CLI overrides directives).
struct EffectiveOptions {
    namespace: Namespace,
    timeout: Option<u64>,
    stdin: bool,
    replace: bool,
    cwd: Option<PathBuf>,
    env_vars: Vec<String>,
    on_exit: Vec<String>,
    after: Vec<String>,
    any_exit: bool,
}

#[allow(clippy::too_many_arguments)]
fn merge_directives_with_cli(
    d: &Directives,
    cli_namespace: Option<&Namespace>,
    cli_timeout: Option<u64>,
    cli_stdin: bool,
    cli_replace: bool,
    cli_cwd: Option<&Path>,
    cli_env_vars: &[String],
    cli_unset_env: &[String],
    cli_on_exit: &[String],
    cli_after: &[String],
    cli_any_exit: bool,
) -> EffectiveOptions {
    // CLI namespace (pre-validated in main.rs) overrides directive namespace
    // (pre-validated in parse_directives). Fall back to default if neither set.
    let namespace = if let Some(ns) = cli_namespace {
        ns.clone()
    } else if let Some(ref ns_str) = d.namespace {
        Namespace::new(ns_str).expect("directive namespace was pre-validated")
    } else {
        Namespace::default_namespace()
    };

    let timeout = cli_timeout.or(d.timeout);
    let stdin = cli_stdin || d.stdin_pipe;
    let replace = cli_replace || d.replace;

    let cwd = cli_cwd
        .map(Path::to_path_buf)
        .or_else(|| d.cwd.as_ref().map(PathBuf::from));

    // Directive env entries + CLI env entries. CLI entries come after,
    // so they override in cmd_start's BTreeMap insertion. A CLI --unset-env
    // drops a directive entry of that name; a CLI --env still sets it.
    let mut env_vars: Vec<String> = d
        .env
        .iter()
        .filter(|(key, _)| !cli_unset_env.contains(key))
        .map(|(_, entry)| entry.clone())
        .collect();
    env_vars.extend(cli_env_vars.iter().cloned());

    // on-exit: if CLI specifies any, use CLI only (override). Otherwise use directives.
    let on_exit = if cli_on_exit.is_empty() {
        d.on_exit.clone()
    } else {
        cli_on_exit.to_vec()
    };

    // --after and --any-exit: CLI-only, no directive equivalent
    let after = cli_after.to_vec();
    let any_exit = cli_any_exit;

    EffectiveOptions {
        namespace,
        timeout,
        stdin,
        replace,
        cwd,
        env_vars,
        on_exit,
        after,
        any_exit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_sh() {
        assert_eq!(
            launcher_argv_for_extension("sh"),
            Some(vec!["bash".to_string()])
        );
    }

    #[test]
    fn launcher_ps1() {
        assert_eq!(
            launcher_argv_for_extension("ps1"),
            Some(vec!["pwsh".to_string(), "-File".to_string()])
        );
    }

    #[test]
    fn launcher_py() {
        let argv = launcher_argv_for_extension("py").unwrap();
        if cfg!(windows) {
            assert_eq!(argv, vec!["py", "-3"]);
        } else {
            assert_eq!(argv, vec!["python3"]);
        }
    }

    #[cfg(windows)]
    #[test]
    fn launcher_bat() {
        assert_eq!(
            launcher_argv_for_extension("bat"),
            Some(vec!["cmd".to_string(), "/c".to_string()])
        );
        assert_eq!(
            launcher_argv_for_extension("cmd"),
            Some(vec!["cmd".to_string(), "/c".to_string()])
        );
    }

    #[cfg(unix)]
    #[test]
    fn launcher_bat_not_on_unix() {
        assert!(launcher_argv_for_extension("bat").is_none());
        assert!(launcher_argv_for_extension("cmd").is_none());
    }

    #[test]
    fn launcher_rb() {
        assert_eq!(
            launcher_argv_for_extension("rb"),
            Some(vec!["ruby".to_string()])
        );
    }

    #[test]
    fn launcher_js() {
        assert_eq!(
            launcher_argv_for_extension("js"),
            Some(vec!["node".to_string()])
        );
    }

    #[test]
    fn resolve_extension_case_insensitive() {
        let result = resolve_shell_argv(
            None,
            "/tmp/HELLO.PY",
            Path::new("/tmp/HELLO.PY"),
            "",
            vec![],
        )
        .unwrap();
        assert_eq!(result[0], if cfg!(windows) { "py" } else { "python3" });
    }

    #[test]
    fn launcher_unknown() {
        assert!(launcher_argv_for_extension("xyz").is_none());
        assert!(launcher_argv_for_extension("ts").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn shebang_basic() {
        assert_eq!(
            parse_shebang("#!/bin/bash\necho hi"),
            Some("/bin/bash".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn shebang_env() {
        assert_eq!(
            parse_shebang("#!/usr/bin/env python3\nprint(1)"),
            Some("python3".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn shebang_missing() {
        assert_eq!(parse_shebang("echo hi"), None);
    }

    #[cfg(unix)]
    #[test]
    fn shebang_empty() {
        assert_eq!(parse_shebang("#!\n"), None);
    }

    #[test]
    fn resolve_shell_override() {
        let result = resolve_shell_argv(
            Some("ruby"),
            "/tmp/test.xyz",
            Path::new("/tmp/test.xyz"),
            "",
            vec![],
        )
        .unwrap();
        assert_eq!(result, vec!["ruby", "/tmp/test.xyz"]);
    }

    #[test]
    fn resolve_extension_py() {
        let result = resolve_shell_argv(
            None,
            "/tmp/test.py",
            Path::new("/tmp/test.py"),
            "",
            vec!["arg1".to_string()],
        )
        .unwrap();
        if cfg!(windows) {
            assert_eq!(result[0], "py");
            assert_eq!(result[1], "-3");
            assert_eq!(result[2], "/tmp/test.py");
            assert_eq!(result[3], "arg1");
        } else {
            assert_eq!(result[0], "python3");
            assert_eq!(result[1], "/tmp/test.py");
            assert_eq!(result[2], "arg1");
        }
    }

    #[test]
    fn resolve_unknown_fails() {
        let result = resolve_shell_argv(
            None,
            "/tmp/test.xyz",
            Path::new("/tmp/test.xyz"),
            "no shebang here",
            vec![],
        );
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("cannot determine interpreter"), "got: {err}");
        assert!(err.contains("--shell"), "got: {err}");
    }
}
