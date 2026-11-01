use std::path::PathBuf;
use std::time::Duration;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use tendr::model::event::EventTimestamp;
use tendr::model::ids::{Namespace, Source};

mod commands;
mod remote_frame;
mod remote_op;

#[derive(Clone, Debug, clap::ValueEnum)]
enum CliExecTarget {
    /// POSIX shell (bash, sh, zsh)
    PosixShell,
    /// PowerShell (pwsh, powershell.exe)
    #[value(name = "powershell")]
    PowerShell,
    /// Python REPL (python3, ipython)
    #[value(name = "python-repl")]
    PythonRepl,
    /// DuckDB SQL
    #[value(name = "duckdb")]
    DuckDb,
    /// Exec not supported
    #[value(name = "none")]
    None,
}

/// The attach client's detach escape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum AttachEscape {
    /// Ctrl-\ then d detaches; Ctrl-\ twice sends one Ctrl-\
    #[value(name = "ctrl-backslash")]
    CtrlBackslash,
    /// No escape: every key goes to the session
    #[value(name = "none")]
    None,
}

impl From<CliExecTarget> for tendr::model::spec::ExecTarget {
    fn from(c: CliExecTarget) -> Self {
        match c {
            CliExecTarget::PosixShell => Self::PosixShell,
            CliExecTarget::PowerShell => Self::PowerShell,
            CliExecTarget::PythonRepl => Self::PythonRepl,
            CliExecTarget::DuckDb => Self::DuckDb,
            CliExecTarget::None => Self::None,
        }
    }
}

#[derive(Parser)]
#[command(
    name = "tendr",
    about, // Cargo.toml's description
    version = env!("CARGO_PKG_VERSION")
)]
struct Cli {
    /// Route command through SSH to a remote host (e.g. user@box).
    ///
    /// Supported remotely: start, status, list, log, push, kill, wait,
    /// watch, attach and exec. The request travels to the remote tendr as
    /// data, never through a shell; the remote tendr must be this version
    /// or newer. Every other command is local-only.
    // allow_hyphen_values so an option-shaped destination (`-oProxyCommand=…`)
    // is captured as the value and rejected by our own validate_destination
    // (clear error, exit 2) rather than mis-parsed by clap as a flag.
    #[arg(long, global = true, allow_hyphen_values = true)]
    host: Option<String>,

    /// Write `tendr::ssh::HELLO_LINE` to stderr before anything else. A local
    /// `--host` call passes it to the remote tendr: its arrival proves ssh got
    /// through authentication, which ends the connect deadline.
    #[arg(long, global = true, hide = true)]
    hello: bool,

    /// Run the command, then write `tendr-exit <code>` (`tendr::ssh::EXIT_TRAILER`)
    /// to stderr as its last line. A local `--host` call passes it to the
    /// remote tendr: a PowerShell login shell reports any failure as ssh exit
    /// 1, so the code travels in-band.
    #[arg(long, hide = true)]
    exit_trailer: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start a supervised run
    Start {
        /// Session name
        name: String,
        /// Namespace for session grouping
        #[arg(long)]
        namespace: Option<String>,
        /// Enable stdin pipe for exec and push (a --pty session takes push without it)
        #[arg(long)]
        stdin: bool,
        /// Interactive pseudo-terminal mode
        #[arg(long)]
        pty: bool,
        /// Exec target protocol (inferred from argv[0] if omitted)
        #[arg(long = "exec-target", value_enum)]
        exec_target: Option<CliExecTarget>,
        /// Replace existing session (kill + restart)
        #[arg(long)]
        replace: bool,
        /// Stop the child, as `kill` does, after N seconds
        #[arg(long)]
        timeout: Option<u64>,
        /// Working directory for the child process
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Environment variable override (KEY=VALUE)
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env_vars: Vec<String>,
        /// Remove NAME from the inherited environment (repeatable)
        #[arg(long = "unset-env", value_name = "NAME")]
        unset_env: Vec<String>,
        /// Command to run after the child exits (repeatable)
        #[arg(long = "on-exit", value_name = "COMMAND")]
        on_exit: Vec<String>,
        /// Wait for session(s) to exit before starting (repeatable)
        #[arg(long = "after", value_name = "SESSION")]
        after: Vec<String>,
        /// Proceed even if dependency exits non-zero
        #[arg(long = "any-exit", requires = "after")]
        any_exit: bool,
        /// Declared execution boundary as KIND:LABEL (host, container, vm, pod)
        #[arg(long, value_name = "KIND:LABEL")]
        boundary: Option<tendr::model::boundary::Boundary>,
        /// Ancestry boundary as KIND:LABEL (repeatable; outermost last)
        #[arg(
            long = "boundary-parent",
            value_name = "KIND:LABEL",
            requires = "boundary"
        )]
        boundary_parent: Vec<tendr::model::boundary::Boundary>,
        /// Command and arguments
        #[arg(trailing_var_arg = true, required = true)]
        cmd: Vec<String>,
    },
    /// Run a script as a supervised session
    Run {
        /// Script file to run
        script: PathBuf,
        /// Interpreter override (default: inferred from extension, +x, or shebang)
        #[arg(long)]
        shell: Option<String>,
        /// Return immediately after start (don't wait for exit)
        #[arg(long, conflicts_with = "foreground")]
        detach: bool,
        /// Force foreground mode (overrides #tendr: detach directive)
        #[arg(long, conflicts_with = "detach")]
        foreground: bool,
        /// Namespace for session grouping
        #[arg(long)]
        namespace: Option<String>,
        /// Enable stdin pipe for push command
        #[arg(long)]
        stdin: bool,
        /// Replace existing session (kill + restart)
        #[arg(long)]
        replace: bool,
        /// Stop the child, as `kill` does, after N seconds
        #[arg(long)]
        timeout: Option<u64>,
        /// Working directory for the child process
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Environment variable override (KEY=VALUE)
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env_vars: Vec<String>,
        /// Remove NAME from the inherited environment (repeatable)
        #[arg(long = "unset-env", value_name = "NAME")]
        unset_env: Vec<String>,
        /// Command to run after the child exits (repeatable)
        #[arg(long = "on-exit", value_name = "COMMAND")]
        on_exit: Vec<String>,
        /// Wait for session(s) to exit before starting (repeatable)
        #[arg(long = "after", value_name = "SESSION")]
        after: Vec<String>,
        /// Proceed even if dependency exits non-zero
        #[arg(long = "any-exit", requires = "after")]
        any_exit: bool,
        /// Arguments to pass to the script
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Send stdin to a running session's child process
    Push {
        /// Session name
        name: String,
        /// Namespace for session grouping
        #[arg(long)]
        namespace: Option<String>,
    },
    /// Show session status
    Status {
        /// Session name
        name: String,
        /// Namespace for session grouping
        #[arg(long)]
        namespace: Option<String>,
    },
    /// Kill a supervised run
    Kill {
        /// Session name
        name: String,
        /// Namespace for session grouping
        #[arg(long)]
        namespace: Option<String>,
        /// Force kill (SIGKILL immediately, no grace period)
        #[arg(short, long)]
        force: bool,
    },
    /// List all sessions
    List {
        /// Namespace to filter (lists all namespaces if omitted)
        #[arg(long)]
        namespace: Option<String>,
    },
    /// Query session output log
    Log {
        /// Session name
        name: String,
        /// Namespace for session grouping
        #[arg(long)]
        namespace: Option<String>,
        /// Show last N lines
        #[arg(short = 'n', long)]
        tail: Option<usize>,
        /// Follow log output (like tail -f)
        #[arg(short, long)]
        follow: bool,
        /// Show lines since TIME (epoch seconds or duration: 30s, 5m, 2h, 1d)
        #[arg(short, long)]
        since: Option<String>,
        /// Output content only, stripping the JSONL envelope
        #[arg(short, long)]
        raw: bool,
    },
    /// Block until session(s) exit and their sidecar releases them
    Wait {
        /// Session name(s)
        #[arg(required = true)]
        names: Vec<String>,
        /// Namespace for session grouping
        #[arg(long)]
        namespace: Option<String>,
        /// Timeout in seconds
        #[arg(short, long)]
        timeout: Option<u64>,
        /// Return when ANY session reaches terminal state (default: wait for ALL)
        #[arg(long)]
        any: bool,
    },
    /// Execute a command in a running shell session
    ///
    /// Takes argv, not a shell snippet. For multi-step commands, use separate
    /// exec calls or wrap explicitly with `bash -c '...'`.
    Exec {
        /// Session name
        #[arg(required_unless_present = "frame_from_stdin")]
        name: Option<String>,
        /// Namespace for session grouping
        #[arg(long)]
        namespace: Option<String>,
        /// Timeout in seconds (client-side only)
        #[arg(long)]
        timeout: Option<u64>,
        /// Read the entire exec request as one JSON frame from stdin
        /// (the frame carries session/namespace/cmd/timeout)
        #[arg(
            long = "frame-from-stdin",
            conflicts_with_all = ["name", "namespace", "timeout", "cmd"]
        )]
        frame_from_stdin: bool,
        /// Command and arguments
        #[arg(trailing_var_arg = true, required_unless_present = "frame_from_stdin")]
        cmd: Vec<String>,
    },
    /// Watch session events as an NDJSON stream
    Watch {
        /// Namespace to filter (watches all namespaces if omitted)
        #[arg(long)]
        namespace: Option<String>,
        /// Emit run lifecycle events only
        #[arg(long)]
        events: bool,
        /// Emit log output events only
        #[arg(long)]
        logs: bool,
        /// Emit annotation events
        #[arg(long)]
        annotations: bool,
        /// Skip initial state snapshot, only emit new events
        #[arg(long = "from-now")]
        from_now: bool,
        /// Atomically create this file once the initial scan/snapshot is
        /// flushed and watch enters steady-state polling
        #[arg(long = "ready-file")]
        ready_file: Option<PathBuf>,
    },
    /// Run a command and record an annotation in the session's event log
    Wrap {
        /// Session name (defaults to TENDR_SESSION env var)
        #[arg(long)]
        session: Option<String>,
        /// Namespace (defaults to TENDR_NAMESPACE env var)
        #[arg(long)]
        namespace: Option<String>,
        /// Annotation source (e.g. "cmux.claude-hook")
        #[arg(long)]
        source: String,
        /// Event name (e.g. "pre-tool-use")
        #[arg(long)]
        event: String,
        /// Command and arguments
        #[arg(trailing_var_arg = true, required = true)]
        cmd: Vec<String>,
    },
    /// Replay session event logs as envelope NDJSON
    Events {
        /// Namespace to filter (all namespaces if omitted)
        #[arg(long)]
        namespace: Option<String>,
        /// Session as <namespace>/<name> or bare <name>; repeatable
        #[arg(long = "session")]
        sessions: Vec<String>,
        /// Kind prefix filter (e.g. "hook."); repeatable
        #[arg(long = "kind")]
        kinds: Vec<String>,
        /// Source prefix filter (e.g. "claude."); repeatable
        #[arg(long = "source")]
        sources: Vec<String>,
        /// After replay, poll for new events (100ms; Ctrl-C to stop)
        #[arg(long)]
        follow: bool,
        /// Skip history of sessions that exist now (later-discovered
        /// sessions still replay from their start)
        #[arg(long = "from-now", group = "warm_start")]
        from_now: bool,
        /// Resume exactly from a cursor token (from a --cursors bookmark)
        #[arg(long = "from-cursor", group = "warm_start")]
        from_cursor: Option<String>,
        /// Replay only events with ts >= this RFC 3339 UTC timestamp
        #[arg(long, group = "warm_start", value_parser = EventTimestamp::parse_flexible)]
        since: Option<EventTimestamp>,
        /// Replay only the last N events by merge order
        #[arg(long, group = "warm_start")]
        last: Option<usize>,
        /// Interleave resumable cursor.bookmark records on stdout
        #[arg(long)]
        cursors: bool,
        /// Merge output.log lines in as derived log.stdout/log.stderr records
        #[arg(long = "include-logs")]
        include_logs: bool,
        /// Exit 65 when unparseable lines were skipped
        #[arg(long)]
        strict: bool,
        /// Atomically create this file once the follower's baseline is
        /// established and initial replay is flushed (requires --follow)
        #[arg(long = "ready-file", requires = "follow")]
        ready_file: Option<PathBuf>,
    },
    /// Query the event log with DuckDB SQL (analytical surface)
    ///
    /// Registers an `events` view over the on-disk JSONL and runs your SQL via
    /// the external `duckdb` CLI. Distinct from `events` (streaming/replay):
    /// `query` is the offline analytical surface. Local-only in v1.
    Query {
        /// Inline SQL to run against the `events` view
        sql: Option<String>,
        /// Read SQL from a file instead of the inline argument
        #[arg(long, conflicts_with = "sql")]
        file: Option<PathBuf>,
        /// Namespaces to scope the view (comma-separated); default = all
        #[arg(long)]
        namespace: Option<String>,
        /// Drop into a DuckDB shell with the `events` view pre-registered
        #[arg(long, conflicts_with_all = ["sql", "file"])]
        shell: bool,
        /// Print the DuckDB version tendr will use, then exit
        #[arg(long = "version", conflicts_with_all = ["sql", "file", "shell", "namespace"])]
        version: bool,
    },
    /// Append an event to a session's event log
    Emit {
        /// Event kind (dotted, e.g. "hook.post_tool_use"; tendr-owned
        /// prefixes like "run." are reserved)
        #[arg(long)]
        kind: String,
        /// Inline JSON object payload
        #[arg(long, group = "data_src")]
        data: Option<String>,
        /// Read JSON object payload from a file
        #[arg(long = "data-file", group = "data_src")]
        data_file: Option<PathBuf>,
        /// Read JSON object payload from stdin
        #[arg(long = "data-stdin", group = "data_src")]
        data_stdin: bool,
        /// Semantic emitter (default: "user.emit"; "tendr.*" is reserved)
        #[arg(long)]
        source: Option<String>,
        /// Target session as <namespace>/<name> or bare <name> (default
        /// namespace); defaults to the supervised-run environment
        #[arg(long)]
        session: Option<String>,
        /// Causal parent event id (UUIDv7)
        #[arg(long)]
        parent: Option<String>,
        /// fdatasync the segment before returning
        #[arg(long)]
        durable: bool,
        /// Exit 0 on every failure (for hooks that must never fail their host)
        #[arg(long = "best-effort")]
        best_effort: bool,
    },
    /// Delete terminal sessions: by name, older than a threshold, or all
    ///
    /// Also removes attach sockets that sidecars killed outright left in
    /// ~/.tendr/sockets (counted as `sockets_removed`): only sockets no session
    /// names, nothing listens on, and older than a minute.
    Prune {
        /// Sessions to delete, by name. They resolve in --namespace, or in
        /// `default`; a name that doesn't exist is reported and fails the command
        #[arg(conflicts_with_all = ["older_than", "all"])]
        names: Vec<String>,
        /// Delete sessions ended more than DURATION ago (e.g. 7d, 24h, 30m)
        #[arg(long, value_parser = parse_duration, conflicts_with = "all")]
        older_than: Option<Duration>,
        /// Delete all terminal sessions regardless of age
        #[arg(long, conflicts_with = "older_than")]
        all: bool,
        /// Namespace to prune. Without names, every namespace if omitted; with
        /// names, the namespace they are in (`default` if omitted)
        #[arg(long)]
        namespace: Option<String>,
        /// Show what would be deleted without deleting
        #[arg(long)]
        dry_run: bool,
    },
    /// Attach to a PTY session's terminal
    Attach {
        /// Session name
        name: String,
        /// Namespace
        #[arg(long)]
        namespace: Option<String>,
        /// Take control even if another client holds it, retiring that client
        /// and dropping its queued input
        #[arg(long)]
        takeover: bool,
        /// Detach escape: `ctrl-backslash` (Ctrl-\ then d detaches; Ctrl-\ twice
        /// sends one Ctrl-\) or `none` (every key goes to the session)
        #[arg(long, value_enum, default_value_t = AttachEscape::CtrlBackslash)]
        escape: AttachEscape,
    },
    /// Print the usage guide (embedded), optionally a single topic
    ///
    /// `tendr guide` prints the whole guide; `tendr guide <topic>` prints one
    /// section. An unknown topic prints the list of topics.
    /// Local-only.
    Guide {
        /// Topic to print (omit for the whole guide)
        topic: Option<String>,
    },
    /// Manage the embedded `using-tendr` agent skill stub
    ///
    /// Print the stub, install it as a Claude Code skill file, or show where
    /// install would write. Local-only.
    Skill {
        #[command(subcommand)]
        action: SkillAction,
    },
    /// Internal: sidecar process (not for direct use)
    #[command(name = "_sidecar", hide = true)]
    Sidecar {
        #[arg(long)]
        session_dir: PathBuf,
    },
    /// Internal: run one `--host` request read from stdin (not for direct use)
    #[command(name = "_remote", hide = true)]
    Remote,
}

/// Subcommands of `tendr skill`.
#[derive(Subcommand)]
enum SkillAction {
    /// Print the embedded skill stub to stdout
    Print,
    /// Write the skill stub to a skill file
    Install {
        /// Install under $HOME/.claude instead of ./.claude (project-local)
        #[arg(long)]
        global: bool,
        /// Overwrite an existing file even if it differs from the stub
        #[arg(long)]
        force: bool,
    },
    /// Print where install would write, without creating anything
    Path {
        /// Report the $HOME/.claude path instead of the project-local one
        #[arg(long)]
        global: bool,
    },
}

impl Commands {
    /// Reconstruct CLI args for local-only commands that can offer a pre-filled
    /// `ssh <host> 'tendr …'` fallback when `--host` is rejected. Reconstructed
    /// from clap-parsed state — never raw argv, which would corrupt child args
    /// after `--`. Returns `None` for commands that keep the generic rejection.
    fn local_fallback_args(&self) -> Option<Vec<String>> {
        match self {
            Commands::Run {
                script,
                shell,
                detach,
                foreground,
                namespace,
                stdin,
                replace,
                timeout,
                cwd,
                env_vars,
                unset_env,
                on_exit,
                after,
                any_exit,
                args: script_args,
            } => {
                let mut args = vec!["run".to_string()];
                if let Some(s) = shell {
                    args.extend(["--shell".to_string(), s.clone()]);
                }
                if *detach {
                    args.push("--detach".to_string());
                }
                if *foreground {
                    args.push("--foreground".to_string());
                }
                if let Some(ns) = namespace {
                    args.extend(["--namespace".to_string(), ns.clone()]);
                }
                if *stdin {
                    args.push("--stdin".to_string());
                }
                if *replace {
                    args.push("--replace".to_string());
                }
                if let Some(t) = timeout {
                    args.extend(["--timeout".to_string(), t.to_string()]);
                }
                if let Some(c) = cwd {
                    args.extend(["--cwd".to_string(), c.display().to_string()]);
                }
                for e in env_vars {
                    args.extend(["--env".to_string(), e.clone()]);
                }
                for u in unset_env {
                    args.extend(["--unset-env".to_string(), u.clone()]);
                }
                for o in on_exit {
                    args.extend(["--on-exit".to_string(), o.clone()]);
                }
                for a in after {
                    args.extend(["--after".to_string(), a.clone()]);
                }
                if *any_exit {
                    args.push("--any-exit".to_string());
                }
                args.push(script.display().to_string());
                // clap consumed the user's first `--` (later ones stay in
                // the captured args) — re-insert exactly one so hyphen
                // script args don't re-parse as tendr flags on paste.
                if !script_args.is_empty() {
                    args.push("--".to_string());
                }
                args.extend(script_args.iter().cloned());
                Some(args)
            }
            Commands::Wrap {
                session,
                namespace,
                source,
                event,
                cmd,
            } => {
                let mut args = vec!["wrap".to_string()];
                if let Some(s) = session {
                    args.extend(["--session".to_string(), s.clone()]);
                }
                if let Some(ns) = namespace {
                    args.extend(["--namespace".to_string(), ns.clone()]);
                }
                args.extend(["--source".to_string(), source.clone()]);
                args.extend(["--event".to_string(), event.clone()]);
                args.push("--".to_string());
                args.extend(cmd.iter().cloned());
                Some(args)
            }
            Commands::Prune {
                names,
                older_than,
                all,
                namespace,
                dry_run,
            } => {
                let mut args = vec!["prune".to_string()];
                args.extend(names.iter().cloned());
                if let Some(d) = older_than {
                    args.extend([
                        "--older-than".to_string(),
                        humantime::format_duration(*d).to_string(),
                    ]);
                }
                if *all {
                    args.push("--all".to_string());
                }
                if let Some(ns) = namespace {
                    args.extend(["--namespace".to_string(), ns.clone()]);
                }
                if *dry_run {
                    args.push("--dry-run".to_string());
                }
                Some(args)
            }
            Commands::Query {
                sql,
                file,
                namespace,
                shell,
                version,
            } => {
                let mut args = vec!["query".to_string()];
                if let Some(f) = file {
                    args.extend(["--file".to_string(), f.display().to_string()]);
                }
                if let Some(ns) = namespace {
                    args.extend(["--namespace".to_string(), ns.clone()]);
                }
                if *shell {
                    args.push("--shell".to_string());
                }
                if *version {
                    args.push("--version".to_string());
                }
                // The positional SQL goes last so a hyphen-led statement is
                // unambiguous when the fallback is pasted.
                if let Some(s) = sql {
                    args.push(s.clone());
                }
                Some(args)
            }
            Commands::Guide { topic } => {
                let mut args = vec!["guide".to_string()];
                if let Some(t) = topic {
                    args.push(t.clone());
                }
                Some(args)
            }
            Commands::Skill { action } => {
                let mut args = vec!["skill".to_string()];
                match action {
                    SkillAction::Print => args.push("print".to_string()),
                    SkillAction::Install { global, force } => {
                        args.push("install".to_string());
                        if *global {
                            args.push("--global".to_string());
                        }
                        if *force {
                            args.push("--force".to_string());
                        }
                    }
                    SkillAction::Path { global } => {
                        args.push("path".to_string());
                        if *global {
                            args.push("--global".to_string());
                        }
                    }
                }
                Some(args)
            }
            _ => None,
        }
    }
}

/// Parse a human-readable duration string (e.g. "7d", "24h", "30m").
fn parse_duration(s: &str) -> Result<Duration, humantime::DurationError> {
    humantime::parse_duration(s)
}

/// Resolve an optional namespace string into a `Namespace`, defaulting to "default".
/// Used by commands that always operate in exactly one namespace.
fn resolve_namespace(namespace: Option<String>) -> anyhow::Result<Namespace> {
    namespace
        .map(|s| Namespace::new(&s))
        .transpose()
        .map(|opt| opt.unwrap_or_else(Namespace::default_namespace))
        .map_err(Into::into)
}

/// Parse an optional namespace string without defaulting.
/// Returns `None` when no namespace was provided — meaning varies by command:
/// "all namespaces" (list/watch/prune) or "defer to directive/default" (run).
fn parse_optional_namespace(namespace: Option<String>) -> anyhow::Result<Option<Namespace>> {
    namespace
        .map(|s| Namespace::new(&s).map_err(anyhow::Error::from))
        .transpose()
}

/// `--exit-trailer`: run this command line as a child, without
/// `--exit-trailer`, with our stdin, stdout and stderr. Then write the
/// child's exit code to stderr as the last line, `tendr-exit <code>`, and
/// exit with it. The child writes the hello, if asked, as the first line:
/// this process writes nothing before it. Running the command in a child
/// covers every way it can end, every `process::exit` and a panic included.
/// This process never reads stdin. A child ended by a signal (Unix) gets no
/// trailer: this exits 128 + the signal.
fn run_with_exit_trailer(matches: &clap::ArgMatches) -> ! {
    use std::io::Write;

    let Some(args) = without_exit_trailer(std::env::args_os().collect(), matches) else {
        eprintln!("error: --exit-trailer must be given as its own word before the command");
        std::process::exit(2);
    };
    let code = match std::env::current_exe()
        .and_then(|exe| std::process::Command::new(exe).args(&args[1..]).status())
    {
        Ok(status) => match status.code() {
            Some(code) => code,
            None => {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    std::process::exit(128 + status.signal().unwrap_or(0));
                }
                #[cfg(not(unix))]
                std::process::exit(1);
            }
        },
        Err(e) => {
            eprintln!("error: cannot run tendr: {e}");
            1
        }
    };
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{}{code}", tendr::ssh::EXIT_TRAILER);
    let _ = stderr.flush();
    std::process::exit(code);
}

/// `argv` without the `--exit-trailer` clap read as the flag, found by its
/// position, so the same word as a child command's argument stays. The flag
/// is accepted only before the subcommand, where clap's positions count from
/// the start of `argv`. `None` when a position clap reports does not hold the
/// plain flag (clap counts `--opt=value` as two positions), so the wrong word
/// is never removed.
fn without_exit_trailer(
    argv: Vec<std::ffi::OsString>,
    matches: &clap::ArgMatches,
) -> Option<Vec<std::ffi::OsString>> {
    let flag: Vec<usize> = matches
        .indices_of("exit_trailer")
        .into_iter()
        .flatten()
        .collect();
    if flag
        .iter()
        .any(|&i| argv.get(i).is_none_or(|arg| arg != "--exit-trailer"))
    {
        return None;
    }
    Some(
        argv.into_iter()
            .enumerate()
            .filter(|(i, _)| !flag.contains(i))
            .map(|(_, arg)| arg)
            .collect(),
    )
}

fn main() {
    let matches = Cli::command().get_matches();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    if cli.exit_trailer {
        run_with_exit_trailer(&matches);
    }
    if cli.hello {
        eprintln!("{}", tendr::ssh::HELLO_LINE);
    }

    // If --host is set, dispatch to SSH transport.
    if let Some(ref host) = cli.host {
        // The destination is a bare positional argument to the local ssh, so an
        // empty or option-shaped value (e.g. `-oProxyCommand=<cmd>`) would run a
        // local command. Reject it at the boundary before any ssh spawn (exit 2).
        if let Err(e) = tendr::ssh::validate_destination(host) {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
        // exec goes remote via the frame transport: the whole request is
        // one JSON frame on the SSH stdin channel; the remote argv is
        // constant, so the payload never traverses a shell.
        if let Commands::Exec {
            name,
            namespace,
            timeout,
            cmd,
            frame_from_stdin,
        } = &cli.command
        {
            // With --frame-from-stdin, local stdin already is the frame:
            // inherit it straight through. Otherwise build the frame
            // from the parsed args.
            let frame = (!frame_from_stdin).then(|| {
                tendr::exec_request::ExecRequestFrame {
                    v: tendr::exec_request::EXEC_FRAME_VERSION,
                    session: name.clone().expect("clap: name required without frame"),
                    namespace: namespace.clone(),
                    cmd: cmd.clone(),
                    timeout: *timeout,
                }
                .to_json()
            });
            match tendr::ssh::exec_ssh_frame(host, frame) {
                Ok(code) => std::process::exit(code),
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(e.exit_code());
                }
            }
        }
        let operation = match remote_op::RemoteOperation::try_from(&cli.command) {
            Ok(operation) => operation,
            Err(local) => {
                // The local-only verbs are a usage error (exit 2) with a
                // pre-filled, copy-pasteable fallback — rejected before any
                // connection or side effect.
                if let Some(args) = cli.command.local_fallback_args() {
                    let mut full = vec!["tendr".to_string()];
                    full.extend(args);
                    let remote_cmd = shell_words::join(&full);
                    // POSIX quoting: the text is for a POSIX shell on both
                    // ends, and is labelled so.
                    eprintln!(
                        "error: '{}' is local-only and does not support --host\n\
                         try (POSIX shell):  ssh {} {}",
                        local.command,
                        shell_words::quote(host),
                        shell_words::quote(&remote_cmd)
                    );
                    std::process::exit(2);
                }
                eprintln!("{local}");
                std::process::exit(1);
            }
        };
        let sent = match operation {
            // attach still needs `ssh -t`, so it cannot take its request on
            // stdin: it sends a command line of plain names instead.
            remote_op::RemoteOperation::Attach(request) => match request.ssh_argv() {
                Ok(args) => tendr::ssh::exec_ssh_attach(host, &args),
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }
            },
            operation => match remote_frame::encode(&operation) {
                Ok(frame) => {
                    let body = matches!(operation, remote_op::RemoteOperation::Push(_));
                    tendr::ssh::exec_ssh_remote(host, frame, body)
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }
            },
        };
        match sent {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(e.exit_code());
            }
        }
    }

    let result = match cli.command {
        Commands::Run {
            script,
            shell,
            detach,
            foreground,
            namespace,
            stdin,
            replace,
            timeout,
            cwd,
            env_vars,
            unset_env,
            on_exit,
            after,
            any_exit,
            args,
        } => parse_optional_namespace(namespace).and_then(|ns| {
            commands::cmd_run(
                &script,
                args,
                shell,
                detach,
                foreground,
                ns.as_ref(),
                timeout,
                stdin,
                replace,
                cwd.as_deref(),
                &env_vars,
                &unset_env,
                &on_exit,
                &after,
                any_exit,
            )
        }),
        Commands::Exec {
            name,
            namespace,
            timeout,
            cmd,
            frame_from_stdin,
        } => {
            if frame_from_stdin {
                commands::cmd_exec_frame_from_stdin()
            } else {
                let name = name.expect("clap: name required without --frame-from-stdin");
                resolve_namespace(namespace)
                    .and_then(|ns| commands::cmd_exec(&name, cmd, timeout, &ns))
            }
        }
        Commands::Events {
            namespace,
            sessions,
            kinds,
            sources,
            follow,
            from_now,
            from_cursor,
            since,
            last,
            cursors,
            include_logs,
            strict,
            ready_file,
        } => commands::cmd_events(commands::EventsOptions {
            namespace,
            sessions,
            kinds,
            sources,
            follow,
            from_now,
            from_cursor,
            since,
            last,
            cursors,
            include_logs,
            strict,
            ready_file,
        }),
        Commands::Query {
            sql,
            file,
            namespace,
            shell,
            version,
        } => commands::cmd_query(commands::QueryOptions {
            sql,
            file,
            namespace,
            shell,
            version,
        }),
        Commands::Emit {
            kind,
            data,
            data_file,
            data_stdin,
            source,
            session,
            parent,
            durable,
            best_effort,
        } => commands::cmd_emit(commands::EmitOptions {
            kind,
            data,
            data_file,
            data_stdin,
            source,
            session,
            parent,
            durable,
            best_effort,
        }),
        Commands::Prune {
            names,
            older_than,
            all,
            namespace,
            dry_run,
        } => {
            if names.is_empty() {
                parse_optional_namespace(namespace)
                    .and_then(|ns| commands::cmd_prune(older_than, all, ns.as_ref(), dry_run))
            } else {
                resolve_namespace(namespace)
                    .and_then(|ns| commands::cmd_prune_named(&names, &ns, dry_run))
            }
        }
        Commands::Wrap {
            session,
            namespace,
            source,
            event,
            cmd,
        } => {
            let session = session
                .or_else(|| std::env::var("TENDR_SESSION").ok())
                .ok_or_else(|| {
                    anyhow::anyhow!("--session required (or set TENDR_SESSION env var)")
                });
            let namespace = namespace.or_else(|| std::env::var("TENDR_NAMESPACE").ok());
            let source = Source::new(&source).map_err(anyhow::Error::from);
            match (session, resolve_namespace(namespace), source) {
                (Ok(s), Ok(ns), Ok(src)) => commands::cmd_wrap(&s, &ns, &src, &event, cmd),
                (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => Err(e),
            }
        }
        Commands::Guide { topic } => commands::cmd_guide(topic.as_deref()),
        Commands::Skill { action } => match action {
            SkillAction::Print => commands::cmd_skill_print(),
            SkillAction::Install { global, force } => commands::cmd_skill_install(global, force),
            SkillAction::Path { global } => commands::cmd_skill_path(global),
        },
        Commands::Sidecar { session_dir } => commands::cmd_sidecar(session_dir),
        Commands::Remote => remote_frame::serve(),
        // The remote-capable commands run through the same typed layer as
        // `--host`. Named, not a catch-all, so a new command must be classified.
        remote @ (Commands::Start { .. }
        | Commands::Status { .. }
        | Commands::List { .. }
        | Commands::Log { .. }
        | Commands::Push { .. }
        | Commands::Kill { .. }
        | Commands::Wait { .. }
        | Commands::Watch { .. }
        | Commands::Attach { .. }) => remote_op::RemoteOperation::try_from(&remote)
            .map_err(anyhow::Error::from)
            .and_then(remote_op::dispatch),
    };

    if let Err(e) = result {
        eprintln!("{e:#}");
        // 1, or a PTY operation's own code (80–85, `tendr::pty_exit`).
        std::process::exit(tendr::pty_exit::exit_code(&e));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wrapper's child gets the command line without the `--exit-trailer`
    /// clap read as the flag, and keeps the same word when it is a child
    /// command's argument. `--hello` stays: the child writes the hello.
    #[test]
    fn the_wrapper_child_runs_the_command_without_the_flag() {
        let cases: &[(&[&str], &[&str])] = &[
            (
                &["tendr", "--hello", "--exit-trailer", "_remote"],
                &["tendr", "--hello", "_remote"],
            ),
            (
                &["tendr", "--exit-trailer", "exec", "--frame-from-stdin"],
                &["tendr", "exec", "--frame-from-stdin"],
            ),
            (
                &["tendr", "--exit-trailer", "status", "s", "--hello"],
                &["tendr", "status", "s", "--hello"],
            ),
            (
                &[
                    "tendr",
                    "--exit-trailer",
                    "start",
                    "s",
                    "--",
                    "prog",
                    "--hello",
                    "--exit-trailer",
                ],
                &[
                    "tendr",
                    "start",
                    "s",
                    "--",
                    "prog",
                    "--hello",
                    "--exit-trailer",
                ],
            ),
        ];
        for (argv, expected) in cases {
            let matches = Cli::command().try_get_matches_from(*argv).unwrap();
            let argv = argv.iter().map(std::ffi::OsString::from).collect();
            let got = without_exit_trailer(argv, &matches);
            assert_eq!(
                got,
                Some(
                    expected
                        .iter()
                        .map(std::ffi::OsString::from)
                        .collect::<Vec<_>>()
                )
            );
        }
        // clap counts `--host=box` as two positions, so its index for the
        // flag no longer names the flag's word: refuse rather than strip
        // the wrong one.
        let argv = ["tendr", "--host=box", "--exit-trailer", "status", "s"];
        let matches = Cli::command().try_get_matches_from(argv).unwrap();
        let argv = argv.iter().map(std::ffi::OsString::from).collect();
        assert_eq!(without_exit_trailer(argv, &matches), None);
        // Only before the subcommand: the child never sees the flag, so it
        // cannot wrap itself again.
        assert!(
            Cli::command()
                .try_get_matches_from(["tendr", "status", "s", "--exit-trailer"])
                .is_err()
        );
    }
}
