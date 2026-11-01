use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::time::{Duration, Instant};

use tendr::events::{self, EventDraft, EventWriter};
use tendr::exec_frame;
use tendr::log::LogLine;
use tendr::model::event::{Event, Kind, Uuid7};
use tendr::model::ids::{Namespace, SessionName, Source};
use tendr::model::meta::Meta;
use tendr::model::spec::{ExecTarget, StdinMode};
use tendr::model::state::RunStatus;
use tendr::platform::{Current, Platform};
use tendr::session::{self, SessionDir, SessionRoot};

/// Advisory flock on `session_dir/exec.lock`, non-blocking.
/// Ensures only one exec runs on a session at a time.
#[derive(Debug)]
pub struct ExecLock {
    _file: File,
}

#[cfg(unix)]
impl ExecLock {
    /// Try to acquire the exec lock without waiting. `None` if another exec
    /// holds it.
    pub fn try_acquire(session: &SessionDir) -> std::io::Result<Option<Self>> {
        use std::os::unix::io::AsRawFd;

        let lock_path = session.path().join("exec.lock");
        let file = File::create(&lock_path)?;

        // SAFETY: file is an open File, so as_raw_fd() returns a valid fd.
        // LOCK_EX | LOCK_NB is a valid flock operation (non-blocking exclusive).
        // flock may fail (EWOULDBLOCK) but won't cause UB.
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(err);
        }

        Ok(Some(Self { _file: file }))
    }
}

#[cfg(windows)]
impl ExecLock {
    /// Try to acquire the exec lock without waiting. `None` if another exec
    /// holds it.
    pub fn try_acquire(session: &SessionDir) -> std::io::Result<Option<Self>> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, HANDLE};
        use windows_sys::Win32::Storage::FileSystem::{
            LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
        };

        let lock_path = session.path().join("exec.lock");
        let file = File::create(&lock_path)?;

        let mut overlapped: windows_sys::Win32::System::IO::OVERLAPPED =
            unsafe { std::mem::zeroed() };
        let ret = unsafe {
            LockFileEx(
                file.as_raw_handle() as HANDLE,
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                1,
                0,
                &mut overlapped,
            )
        };
        if ret == 0 {
            let err = std::io::Error::last_os_error();
            // A lock held by another handle fails immediately with this.
            if err.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
                return Ok(None);
            }
            return Err(err);
        }

        Ok(Some(Self { _file: file }))
    }
}

/// The record of an exec whose frame went to the session and has not been
/// seen to finish: `exec.pending.json`, written (temporary file, then rename)
/// just before the frame is sent and removed once its result is read.
///
/// A timed-out exec leaves it behind, as do an error after sending and a
/// client that dies. The next exec on the session checks it under the exec
/// lock: once the frame's completion signal is in the session dir it removes
/// the record and goes on; until then it refuses, naming this exec. Sending
/// earlier would put the new frame on the stdin of a command that may read it
/// as its own input, and its output after the next exec's log offset. NDJSON
/// execs never write one: their replies are matched by id.
#[derive(serde::Serialize, serde::Deserialize)]
struct PendingExec {
    token: String,
    /// When the exec started, RFC 3339 UTC.
    started_at: String,
    /// The command, shortened for messages.
    command: String,
    /// `output.log` length before the frame was sent; the sentinel targets
    /// look for its completion from here.
    log_offset: u64,
}

const PENDING_EXEC_FILE: &str = "exec.pending.json";

/// How much of the command a pending-exec record keeps.
const PENDING_COMMAND_BYTES: usize = 120;

impl PendingExec {
    fn new(cmd: &[String], token: &str, log_offset: u64) -> Self {
        Self {
            token: token.to_owned(),
            started_at: humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string(),
            command: tendr::annotation::truncate_string(
                &shell_words::join(cmd),
                PENDING_COMMAND_BYTES,
            ),
            log_offset,
        }
    }

    fn path(session: &SessionDir) -> std::path::PathBuf {
        session.path().join(PENDING_EXEC_FILE)
    }

    fn load(session: &SessionDir) -> anyhow::Result<Option<Self>> {
        let path = Self::path(session);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(anyhow::anyhow!("cannot read {}: {e}", path.display())),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| anyhow::anyhow!("unreadable {}: {e}", path.display()))
    }

    fn save(&self, session: &SessionDir) -> anyhow::Result<()> {
        let path = Self::path(session);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Remove the record of exec `token`, then its result file if any. The
    /// result file is the side-channel completion signal, so it must outlive
    /// the record: a client that dies between the two leaves no record, not a
    /// record whose signal is gone.
    fn clear(session: &SessionDir, token: &str) {
        let _ = std::fs::remove_file(Self::path(session));
        let _ = std::fs::remove_file(result_file(session, token));
    }

    /// Whether this exec's frame has finished in the session: its result
    /// file exists, or its sentinel and, for a POSIX shell, its stderr end
    /// marker are in `output.log`.
    fn is_finished(&self, session: &SessionDir, target: ExecTarget) -> anyhow::Result<bool> {
        match target {
            ExecTarget::PythonRepl | ExecTarget::PowerShell => {
                Ok(result_file(session, &self.token).exists())
            }
            ExecTarget::PosixShell => self.sentinel_logged(session, true),
            ExecTarget::DuckDb => {
                let finished = self.sentinel_logged(session, false)?;
                if finished {
                    // As in drain_trailing_stderr: the statement's stderr may
                    // be logged just after the sentinel.
                    std::thread::sleep(DUCKDB_STDERR_GRACE);
                }
                Ok(finished)
            }
            ExecTarget::Ndjson | ExecTarget::None => Ok(true),
        }
    }

    fn sentinel_logged(
        &self,
        session: &SessionDir,
        require_err_marker: bool,
    ) -> anyhow::Result<bool> {
        let log_path = session.path().join("output.log");
        let file = match File::open(&log_path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(self.log_offset))?;

        let mut sentinel = false;
        let mut err_marked = !require_err_marker;
        let mut buf: Vec<u8> = Vec::new();
        while !(sentinel && err_marked) {
            buf.clear();
            if reader.read_until(b'\n', &mut buf)? == 0 {
                break;
            }
            // A torn last line does not parse; the next exec reads it whole.
            let Ok(line) = std::str::from_utf8(&buf) else {
                continue;
            };
            let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
            let Ok(parsed) = serde_json::from_str::<LogLine>(trimmed) else {
                continue;
            };
            let Some(content) = parsed.content_text() else {
                continue;
            };
            if parsed.tag == "O" && exec_frame::parse_sentinel(content, &self.token).is_some() {
                sentinel = true;
            } else if require_err_marker
                && exec_frame::parse_err_marker(content, &self.token).is_some()
            {
                err_marked = true;
            }
        }
        if sentinel && !err_marked {
            // As in wait_sentinel_result: the shell's stderr may no longer
            // reach the log. Give stragglers the same grace before the next
            // exec takes its log offset.
            std::thread::sleep(ERR_MARKER_GRACE);
        }
        Ok(sentinel)
    }

    /// `exec <token>, started <time>: <command>`
    fn describe(&self) -> String {
        format!(
            "exec {}, started {}: {}",
            self.token, self.started_at, self.command
        )
    }
}

/// `exec-results/<token>.json`: where a Python REPL or PowerShell frame
/// writes its result.
fn result_file(session: &SessionDir, token: &str) -> std::path::PathBuf {
    session
        .path()
        .join("exec-results")
        .join(format!("{token}.json"))
}

/// `tendr <verb> <session>`, with `--namespace` outside the default one, as
/// the user would type it.
fn session_command(verb: &str, name: &SessionName, namespace: &Namespace) -> String {
    if *namespace == Namespace::default_namespace() {
        format!("tendr {verb} {}", name.as_str())
    } else {
        format!(
            "tendr {verb} --namespace {} {}",
            namespace.as_str(),
            name.as_str()
        )
    }
}

#[derive(serde::Serialize)]
struct ExecResult {
    session: String,
    stdout: String,
    stderr: String,
    exit_code: i32,
    cwd_after: String,
    timed_out: bool,
    truncated: bool,
    /// NDJSON target only: stdout lines seen while waiting that were not the
    /// reply (late replies to a timed-out exec, unsolicited output).
    #[serde(skip_serializing_if = "Option::is_none")]
    unmatched: Option<Vec<String>>,
    /// NDJSON target only: the `id` tendr added to the request, so a caller
    /// can address it later (for example, to cancel it after a timeout).
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
}

#[derive(serde::Deserialize)]
struct SideChannelResult {
    exit_code: i32,
    cwd: String,
    stdout: String,
    stderr: String,
    traceback: Option<String>,
}

/// Execute one exec request read as a JSON frame from stdin
/// (`--frame-from-stdin`; docs/architecture/06-transport-boundaries.md). The
/// frame is the whole request; a malformed or wrong-version frame is a
/// usage error (exit 2) before any side effect. After decoding this is
/// exactly `cmd_exec` — same exec lock, same session semantics.
pub fn cmd_exec_frame_from_stdin() -> anyhow::Result<()> {
    use std::io::Read;

    let mut buf = Vec::new();
    std::io::stdin().read_to_end(&mut buf)?;
    let frame = match tendr::exec_request::ExecRequestFrame::from_json(&buf) {
        Ok(frame) => frame,
        Err(e) => {
            eprintln!("tendr exec: {e}");
            std::process::exit(2);
        }
    };
    let namespace = match frame.namespace {
        Some(ns) => match Namespace::new(&ns) {
            Ok(ns) => ns,
            Err(e) => {
                eprintln!("tendr exec: invalid exec frame: {e}");
                std::process::exit(2);
            }
        },
        None => Namespace::default_namespace(),
    };
    cmd_exec(&frame.session, frame.cmd, frame.timeout, &namespace)
}

pub fn cmd_exec(
    name: &str,
    cmd: Vec<String>,
    timeout: Option<u64>,
    namespace: &Namespace,
) -> anyhow::Result<()> {
    let session_name = SessionName::new(name)?;
    let root = SessionRoot::default_path()?;

    let session = session::open(&root, namespace, &session_name)?
        .ok_or_else(|| anyhow::anyhow!("session not found: {name}"))?;

    let meta = session::read_meta(&session)?;

    if !matches!(meta.status(), RunStatus::Running { .. }) {
        anyhow::bail!("session is not running");
    }

    if meta.launch_spec().io_mode == tendr::model::spec::IoMode::Pty
        && meta.launch_spec().exec_target != tendr::model::spec::ExecTarget::PythonRepl
    {
        anyhow::bail!("exec is not supported on PTY sessions (except python-repl)");
    }

    if meta.launch_spec().exec_target == tendr::model::spec::ExecTarget::DuckDb
        && meta.launch_spec().io_mode == tendr::model::spec::IoMode::Pty
    {
        anyhow::bail!("DuckDB exec requires pipe transport, not PTY");
    }

    if meta.launch_spec().stdin_mode != StdinMode::Pipe {
        anyhow::bail!("session was not started with --stdin");
    }

    // The NDJSON request is checked before any side effect: a bad one is a
    // usage error, like a malformed exec frame.
    let token = exec_frame::generate_token();
    let ndjson_request = if meta.launch_spec().exec_target == tendr::model::spec::ExecTarget::Ndjson
    {
        match exec_frame::ndjson_frame(&cmd, &token) {
            Ok(line) => Some(line),
            Err(e) => {
                eprintln!("tendr exec: {e}");
                std::process::exit(2);
            }
        }
    } else {
        None
    };

    let exec_target = meta.launch_spec().exec_target;
    let Some(_lock) = ExecLock::try_acquire(&session)? else {
        // The record is informational here: it is read without the lock,
        // and the exec holding the lock may not have written it yet.
        let running = PendingExec::load(&session)
            .ok()
            .flatten()
            .map(|p| format!(" ({})", p.describe()))
            .unwrap_or_default();
        anyhow::bail!(
            "another exec is already running on this session{running}; wait for it to \
             return, or start a second session for parallel work"
        );
    };

    if let Some(pending) = PendingExec::load(&session)? {
        if !pending.is_finished(&session, exec_target)? {
            anyhow::bail!(
                "an earlier exec on this session has not finished ({}). It timed out, or its \
                 client stopped, while the command was still running. Nothing was sent: the \
                 command could read this exec as its own input, and its late output would be \
                 mixed into this result. Retry once it finishes (`{} -f` shows its output), \
                 or stop the session with `{}`.",
                pending.describe(),
                session_command("log", &session_name, namespace),
                session_command("kill", &session_name, namespace),
            );
        }
        PendingExec::clear(&session, &pending.token);
    }

    // Validate cmd is non-empty (clap enforces this, but belt-and-suspenders)
    if cmd.is_empty() {
        anyhow::bail!("no command specified");
    }

    // The command block (spec §2): fresh block_id per invocation; parent
    // from exec's own ambient chain, so nested producers chain upward.
    let block_id = Uuid7::new();
    let parent_id = events::env_parent_chain();
    let mut event_writer = EventWriter::new(session.path());

    let mut started_data = serde_json::json!({
        "command": &cmd,
        "exec_target": meta.launch_spec().exec_target,
    });
    if let Some(t) = timeout {
        started_data["timeout_ms"] = serde_json::json!(t.saturating_mul(1000));
    }
    emit_exec_event(
        &mut event_writer,
        &meta,
        namespace,
        "exec.started",
        started_data,
        None,
        block_id,
        parent_id,
    );

    let result = run_exec(
        &session,
        &meta,
        &cmd,
        &token,
        ndjson_request,
        timeout,
        block_id,
    );
    // A timed-out frame may still be running, and after an error it may not
    // be known to have finished: the record stays, so the next exec checks
    // before sending anything.
    if result.as_ref().is_ok_and(|r| !r.timed_out) {
        PendingExec::clear(&session, &token);
    }
    let result = result?;

    // The authoritative record (spec §6): exec.result with the ExecResult
    // fields; oversize output spills with a structured preview that keeps
    // exit_code/cwd_after queryable inline (spec example (d)).
    let result_event = {
        use tendr::annotation;

        let mut result_data = serde_json::json!({
            "exit_code": result.exit_code,
            "cwd_after": &result.cwd_after,
            "timed_out": result.timed_out,
            "stderr": &result.stderr,
            "stdout": &result.stdout,
            "truncated": false,
        });
        let mut result_preview = serde_json::json!({
            "exit_code": result.exit_code,
            "cwd_after": &result.cwd_after,
            "timed_out": result.timed_out,
            "stderr": annotation::truncate_string(&result.stderr, 1024),
            "stdout_preview": annotation::truncate_string(&result.stdout, 2048),
            "truncated": true,
        });
        if let Some(unmatched) = &result.unmatched {
            result_data["unmatched"] = serde_json::json!(unmatched);
            result_preview["unmatched_count"] = serde_json::json!(unmatched.len());
        }
        if let Some(request_id) = &result.request_id {
            result_data["request_id"] = serde_json::json!(request_id);
            result_preview["request_id"] = serde_json::json!(request_id);
        }
        emit_exec_event(
            &mut event_writer,
            &meta,
            namespace,
            "exec.result",
            result_data,
            Some(result_preview),
            block_id,
            parent_id,
        )
    };

    // The output.log A-line projection (bounded by MAX_LINE), linked to the
    // authoritative event by event_id (spec §0). Shape additive-only. Overflow
    // degrades quietly — full → field-truncated → compact breadcrumb — with no
    // stderr warning; see write_exec_annotation.
    {
        let log_path = session.path().join("output.log");
        let event_id = result_event.as_ref().map(|event| event.id.to_string());
        write_exec_annotation(
            &log_path,
            &cmd,
            &result,
            &meta.run_id().to_string(),
            &block_id.to_string(),
            event_id.as_deref(),
            &token,
        )?;
    }

    let json = serde_json::to_string_pretty(&result)?;
    outln!("{json}");

    if result.timed_out {
        if exec_target == ExecTarget::Ndjson {
            eprintln!(
                "exec timed out — request {} may still be running in the session; its reply \
                 will arrive in a later exec's `unmatched` (or `tendr log`)",
                result.request_id.as_deref().unwrap_or_default()
            );
        } else {
            eprintln!(
                "exec timed out — exec {token} may still be running in the session; later \
                 execs are refused until it finishes, or until `{}` stops the session",
                session_command("kill", &session_name, namespace)
            );
        }
        std::process::exit(124);
    }
    if result.exit_code != 0 {
        std::process::exit(result.exit_code);
    }

    Ok(())
}

/// Best-effort append of a tendr-stamped exec event (spec §6). `exec.` is
/// a reserved prefix — this internal call site is its only permitted writer
/// (grammar-only `Kind::new`). Failures warn on stderr and never alter
/// exec's output or exit code.
#[allow(clippy::too_many_arguments)]
fn emit_exec_event(
    writer: &mut EventWriter,
    meta: &Meta,
    namespace: &Namespace,
    kind: &str,
    data: serde_json::Value,
    preview: Option<serde_json::Value>,
    block_id: Uuid7,
    parent_id: Option<Uuid7>,
) -> Option<Event> {
    let draft = EventDraft {
        id: None,
        kind: Kind::new(kind).expect("exec kinds are grammatical"),
        namespace: namespace.clone(),
        session: meta.session().clone(),
        run_id: meta.run_id(),
        generation: Some(meta.generation().as_u64()),
        source: Source::trusted("tendr.exec").expect("tendr.exec is grammatical"),
        block_id: Some(block_id),
        parent_id,
        data: Some(data),
        preview,
    };
    match writer.append(draft, false) {
        Ok(event) => Some(event),
        Err(e) => {
            eprintln!("tendr exec: {kind} event append failed: {e}");
            None
        }
    }
}

fn run_exec(
    session: &SessionDir,
    meta: &tendr::model::meta::Meta,
    cmd: &[String],
    token: &str,
    ndjson_request: Option<String>,
    timeout: Option<u64>,
    block_id: Uuid7,
) -> anyhow::Result<ExecResult> {
    let session_name = meta.session().as_str().to_string();
    let deadline = timeout.map(|t| Instant::now() + Duration::from_secs(t));

    // 1. Capture log cursor (for sentinel path)
    let log_path = session.path().join("output.log");
    let cursor = std::fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0);

    // 2. Frame the command according to the session's exec target.
    enum WaitMode {
        /// Shell-style: sentinel in stdout, results in stdout/stderr log lines.
        Sentinel,
        /// Like Sentinel, but after finding the sentinel, drain trailing stderr
        /// and set exit_code=1 if any stderr was captured. DuckDB's sentinel
        /// hardcodes exit code 0 (SQL has no $?), so errors must be detected
        /// from stderr presence.
        SentinelWithStderrCheck,
        /// Python-style: poll for a result file written atomically by the frame.
        SideChannel,
        /// NDJSON: the stdout line whose `id` is the token is the reply.
        Ndjson,
    }

    let (framed, wait_mode) = match meta.launch_spec().exec_target {
        // PosixShell is the only target that exports TENDR_BLOCK_ID; the
        // REPL and PowerShell frames do not yet, and will when a consumer
        // needs it.
        ExecTarget::PosixShell => (
            exec_frame::unix_frame(cmd, token, &block_id.to_string()),
            WaitMode::Sentinel,
        ),
        ExecTarget::PowerShell => {
            let results_dir = session.path().join("exec-results");
            std::fs::create_dir_all(&results_dir)?;
            let result_path = result_file(session, token);
            let result_path_str = result_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("session path is not valid UTF-8"))?;
            let code = cmd.join("\n");
            (
                exec_frame::powershell_frame(&code, result_path_str),
                WaitMode::SideChannel,
            )
        }
        ExecTarget::PythonRepl => {
            // Ensure exec-results dir exists
            let results_dir = session.path().join("exec-results");
            std::fs::create_dir_all(&results_dir)?;
            let result_path = result_file(session, token);
            let result_path_str = result_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("session path is not valid UTF-8"))?;
            let code = cmd.join("\n");
            (
                exec_frame::python_frame(&code, result_path_str),
                WaitMode::SideChannel,
            )
        }
        ExecTarget::DuckDb => {
            let sql = cmd.join("\n");
            (
                exec_frame::duckdb_frame(&sql, token),
                WaitMode::SentinelWithStderrCheck,
            )
        }
        ExecTarget::Ndjson => (
            ndjson_request
                .ok_or_else(|| anyhow::anyhow!("internal: ndjson request was not built"))?,
            WaitMode::Ndjson,
        ),
        ExecTarget::None => {
            anyhow::bail!(
                "session has no exec target — restart with --exec-target if this is a shell"
            )
        }
    };

    // 3. Send through stdin transport (with retry on ConnectionRefused)
    let mut writer = loop {
        if let Some(dl) = deadline {
            if Instant::now() >= dl {
                anyhow::bail!("timeout connecting to stdin transport");
            }
        }
        match Current::open_stdin_writer(session.path()) {
            Ok(f) => break f,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                let current = session::read_meta(session)?;
                if !matches!(current.status(), RunStatus::Running { .. }) {
                    anyhow::bail!("session exited before exec could connect");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                return Err(anyhow::anyhow!("failed to open stdin pipe: {e}"));
            }
        }
    };
    // Recorded before the frame goes out, so the guard holds even if this
    // client dies while waiting (see PendingExec).
    if !matches!(wait_mode, WaitMode::Ndjson) {
        PendingExec::new(cmd, token, cursor).save(session)?;
    }
    writer.write_all(framed.as_bytes())?;
    writer.flush()?;
    drop(writer);

    // 4. Wait for result
    match wait_mode {
        WaitMode::SideChannel => wait_side_channel_result(session, &session_name, token, deadline),
        WaitMode::Ndjson => wait_ndjson_result(session, &session_name, token, cursor, deadline),
        WaitMode::Sentinel => {
            wait_sentinel_result(session, &session_name, token, cursor, deadline, true)
        }
        WaitMode::SentinelWithStderrCheck => {
            let mut result =
                wait_sentinel_result(session, &session_name, token, cursor, deadline, false)?;

            // DuckDB's sentinel hardcodes exit code 0 (SQL has no $?).
            // Detect errors from stderr. Stderr lines may arrive in the log
            // after the sentinel due to pipe read ordering, so always drain
            // trailing stderr to catch races and partial-success cases
            // (e.g. "SELECT 1; SELECT * FROM bad;").
            let trailing_stderr = drain_trailing_stderr(session, cursor);
            if !trailing_stderr.is_empty() {
                if result.stderr.is_empty() {
                    result.stderr = trailing_stderr;
                } else {
                    result.stderr.push('\n');
                    result.stderr.push_str(&trailing_stderr);
                }
            }
            if !result.stderr.is_empty() {
                result.exit_code = 1;
            }

            Ok(result)
        }
    }
}

/// How long exec keeps reading after the stdout sentinel for the stderr end
/// marker. Only reached when the session shell's stderr no longer reaches the
/// sidecar (e.g. an earlier `push 'exec 2>/dev/null'`).
const ERR_MARKER_GRACE: Duration = Duration::from_secs(1);

/// Scan output.log for the result of one framed exec (PosixShell, DuckDB).
///
/// The sidecar logs stdout and stderr from separate threads, so their lines
/// interleave in no fixed order. With `require_err_marker` (the POSIX frame)
/// the exec is complete only once both the stdout sentinel and the stderr end
/// marker are logged; each pipe is logged in order, so nothing of the
/// command's output can still be in flight (#92). DuckDB's frame has no stderr
/// marker and relies on `drain_trailing_stderr` instead.
fn wait_sentinel_result(
    session: &SessionDir,
    session_name: &str,
    token: &str,
    cursor: u64,
    deadline: Option<Instant>,
    require_err_marker: bool,
) -> anyhow::Result<ExecResult> {
    let log_path = session.path().join("output.log");

    let mut stdout_lines: Vec<String> = Vec::new();
    let mut stderr_lines: Vec<String> = Vec::new();
    // The stdout sentinel's (exit code, cwd), and when it was seen.
    let mut sentinel: Option<(i32, String, Instant)> = None;
    let mut err_marked = !require_err_marker;

    let result = |stdout: &[String], stderr: &[String], done: Option<(i32, String)>| {
        let (exit_code, cwd_after, timed_out) = match done {
            Some((code, cwd)) => (code, cwd, false),
            None => (-1, String::new(), true),
        };
        ExecResult {
            session: session_name.to_string(),
            stdout: stdout.join("\n"),
            stderr: stderr.join("\n"),
            exit_code,
            cwd_after,
            timed_out,
            truncated: false,
            unmatched: None,
            request_id: None,
        }
    };

    // Wait for log file to exist
    while !log_path.exists() {
        if let Some(dl) = deadline {
            if Instant::now() >= dl {
                return Ok(result(&[], &[], None));
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let file = std::fs::File::open(&log_path)?;
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(cursor))?;

    let mut buf = String::new();
    loop {
        if let Some((code, cwd, _)) = &sentinel {
            if err_marked {
                return Ok(result(
                    &stdout_lines,
                    &stderr_lines,
                    Some((*code, cwd.clone())),
                ));
            }
        } else if let Some(dl) = deadline {
            // No sentinel by the deadline: the command is still running.
            if Instant::now() >= dl {
                return Ok(result(&stdout_lines, &stderr_lines, None));
            }
        }

        buf.clear();
        let bytes = reader.read_line(&mut buf)?;
        if bytes == 0 {
            if let Some((code, cwd, seen_at)) = &sentinel {
                if seen_at.elapsed() >= ERR_MARKER_GRACE {
                    eprintln!(
                        "tendr exec: stderr end marker not seen; the session shell's stderr \
                         may be redirected, so stderr may be incomplete"
                    );
                    return Ok(result(
                        &stdout_lines,
                        &stderr_lines,
                        Some((*code, cwd.clone())),
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            // No data available — check session is still running
            let current = session::read_meta(session)?;
            if !matches!(current.status(), RunStatus::Running { .. }) {
                anyhow::bail!("session exited while waiting for exec result");
            }
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }

        let trimmed = buf.trim_end_matches('\n').trim_end_matches('\r');
        let Some(parsed) = serde_json::from_str::<LogLine>(trimmed).ok() else {
            continue;
        };
        let Some(content) = parsed.content_text() else {
            continue; // annotations and other structured lines
        };

        match parsed.tag.as_str() {
            "O" => {
                if let Some((residual, code, cwd)) = exec_frame::parse_sentinel(content, token) {
                    if !residual.is_empty() {
                        stdout_lines.push(residual.to_owned());
                    }
                    sentinel.get_or_insert((code, cwd, Instant::now()));
                } else if let Some(residual) = require_err_marker
                    .then(|| exec_frame::parse_err_marker(content, token))
                    .flatten()
                {
                    // A PTY merges both streams onto stdout.
                    if !residual.is_empty() {
                        stdout_lines.push(residual.to_owned());
                    }
                    err_marked = true;
                } else {
                    stdout_lines.push(content.to_owned());
                }
            }
            "E" => {
                if let Some(residual) = require_err_marker
                    .then(|| exec_frame::parse_err_marker(content, token))
                    .flatten()
                {
                    if !residual.is_empty() {
                        stderr_lines.push(residual.to_owned());
                    }
                    err_marked = true;
                } else {
                    stderr_lines.push(content.to_owned());
                }
            }
            _ => {}
        }
    }
}

/// Poll for a side-channel result file written by the Python frame.
fn wait_side_channel_result(
    session: &SessionDir,
    session_name: &str,
    token: &str,
    deadline: Option<Instant>,
) -> anyhow::Result<ExecResult> {
    let result_path = result_file(session, token);

    loop {
        if let Some(dl) = deadline {
            if Instant::now() >= dl {
                return Ok(ExecResult {
                    session: session_name.to_string(),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: -1,
                    cwd_after: String::new(),
                    timed_out: true,
                    truncated: false,
                    unmatched: None,
                    request_id: None,
                });
            }
        }

        if result_path.exists() {
            let content = std::fs::read_to_string(&result_path)?;
            // The file stays until cmd_exec clears the record (PendingExec::clear).
            let sc: SideChannelResult = serde_json::from_str(&content)
                .map_err(|e| anyhow::anyhow!("invalid side-channel result: {e}"))?;

            let mut stderr = sc.stderr;
            if let Some(tb) = sc.traceback {
                if !stderr.is_empty() {
                    stderr.push('\n');
                }
                stderr.push_str(&tb);
            }

            return Ok(ExecResult {
                session: session_name.to_string(),
                stdout: sc.stdout,
                stderr,
                exit_code: sc.exit_code,
                cwd_after: sc.cwd,
                timed_out: false,
                truncated: false,
                unmatched: None,
                request_id: None,
            });
        }

        // Check session is still alive
        let current = session::read_meta(session)?;
        if !matches!(current.status(), RunStatus::Running { .. }) {
            anyhow::bail!("session exited while waiting for exec result");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// How long NDJSON exec keeps reading after the reply for stderr the child
/// wrote before it: the sidecar logs each pipe from its own thread, so such
/// stderr can be logged just after the reply.
const NDJSON_STDERR_GRACE: Duration = Duration::from_millis(20);

/// Scan output.log for the NDJSON reply to `token`: the first stdout line
/// that is a JSON object whose `id` is the token. Every other stdout line is
/// kept in `unmatched`, so a late reply to a timed-out exec is reported, not
/// taken as this exec's reply. Stderr lines are the result's stderr.
fn wait_ndjson_result(
    session: &SessionDir,
    session_name: &str,
    token: &str,
    cursor: u64,
    deadline: Option<Instant>,
) -> anyhow::Result<ExecResult> {
    let log_path = session.path().join("output.log");
    let mut reply: Option<(String, i32)> = None;
    let mut unmatched: Vec<String> = Vec::new();
    let mut stderr_lines: Vec<String> = Vec::new();

    let result = |reply: Option<(String, i32)>, unmatched: Vec<String>, stderr: &[String]| {
        let (stdout, exit_code, timed_out) = match reply {
            Some((line, code)) => (line, code, false),
            None => (String::new(), -1, true),
        };
        ExecResult {
            session: session_name.to_string(),
            stdout,
            stderr: stderr.join("\n"),
            exit_code,
            cwd_after: String::new(),
            timed_out,
            truncated: false,
            unmatched: Some(unmatched),
            request_id: Some(token.to_owned()),
        }
    };

    while !log_path.exists() {
        if deadline.is_some_and(|dl| Instant::now() >= dl) {
            return Ok(result(None, unmatched, &stderr_lines));
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let mut reader = BufReader::new(std::fs::File::open(&log_path)?);
    reader.seek(SeekFrom::Start(cursor))?;

    let mut grace_ends: Option<Instant> = None;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        // Bytes, not a String: a torn read can end mid-character.
        let bytes = reader.read_until(b'\n', &mut buf)?;
        if bytes > 0 && !buf.ends_with(b"\n") {
            // A line the sidecar is still writing: read it again once whole,
            // since the reply must not be lost to a torn read.
            reader.seek(SeekFrom::Current(-(bytes as i64)))?;
        }
        if bytes == 0 || !buf.ends_with(b"\n") {
            if reply.is_some() {
                let ends = *grace_ends.get_or_insert_with(|| Instant::now() + NDJSON_STDERR_GRACE);
                if Instant::now() >= ends {
                    return Ok(result(reply, unmatched, &stderr_lines));
                }
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            // The deadline is checked only once everything logged so far has
            // been read, so a reply already on disk is never reported late.
            if deadline.is_some_and(|dl| Instant::now() >= dl) {
                return Ok(result(None, unmatched, &stderr_lines));
            }
            let current = session::read_meta(session)?;
            if !matches!(current.status(), RunStatus::Running { .. }) {
                anyhow::bail!("session exited while waiting for exec result");
            }
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }

        let Ok(line) = std::str::from_utf8(&buf) else {
            continue;
        };
        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
        let Some(parsed) = serde_json::from_str::<LogLine>(trimmed).ok() else {
            continue;
        };
        let Some(content) = parsed.content_text() else {
            continue; // annotations and other structured lines
        };
        match parsed.tag.as_str() {
            "O" => match exec_frame::parse_ndjson_reply(content, token) {
                Some(code) if reply.is_none() => reply = Some((content.to_owned(), code)),
                _ => unmatched.push(content.to_owned()),
            },
            "E" => stderr_lines.push(content.to_owned()),
            _ => {}
        }
    }
}

/// How long a DuckDB exec reads past its sentinel for trailing stderr.
const DUCKDB_STDERR_GRACE: Duration = Duration::from_millis(100);

/// After the sentinel has been found, re-scan the log for any 'E' (stderr)
/// lines that arrived after the sentinel.
///
/// The sidecar reads stdout and stderr from separate pipes on separate threads,
/// writing both to output.log with 'O' and 'E' tags. Line ordering between
/// tags is not guaranteed: a child can write to stderr and then stdout nearly
/// simultaneously, and the sidecar may flush the stdout ('O') sentinel line
/// to the log before the stderr ('E') error line. This function accommodates
/// that race by waiting up to DUCKDB_STDERR_GRACE after the sentinel for
/// trailing stderr.
fn drain_trailing_stderr(session: &SessionDir, cursor: u64) -> String {
    let log_path = session.path().join("output.log");
    let Ok(file) = std::fs::File::open(&log_path) else {
        return String::new();
    };
    let mut reader = BufReader::new(file);
    let _ = reader.seek(SeekFrom::Start(cursor));

    let mut stderr_lines: Vec<String> = Vec::new();
    let mut buf = String::new();
    let deadline = Instant::now() + DUCKDB_STDERR_GRACE;
    let mut saw_sentinel = false;

    loop {
        if Instant::now() >= deadline {
            break;
        }
        buf.clear();
        match reader.read_line(&mut buf) {
            Ok(0) => {
                if saw_sentinel {
                    // Already past the sentinel and no more data — give a small
                    // window for stderr to arrive, then stop.
                    std::thread::sleep(Duration::from_millis(20));
                    buf.clear();
                    if reader.read_line(&mut buf).unwrap_or(0) == 0 {
                        break;
                    }
                    // Fall through to parse the line
                } else {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }

        let trimmed = buf.trim_end_matches('\n').trim_end_matches('\r');
        if let Ok(parsed) = serde_json::from_str::<LogLine>(trimmed) {
            match parsed.tag.as_str() {
                "O" if parsed
                    .content_text()
                    .is_some_and(|c| c.contains("__TENDR_EXEC__")) =>
                {
                    saw_sentinel = true;
                }
                "E" if saw_sentinel => {
                    if let Some(content) = parsed.content_text() {
                        stderr_lines.push(content.to_owned());
                    }
                }
                _ => {}
            }
        }
    }

    stderr_lines.join("\n")
}

/// SHA-256 of `bytes` as lowercase hex (same convention as `events::hex`).
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Write the `agent.exec` A-line annotation to `output.log`, degrading
/// gracefully as the payload grows: full record → field-truncated record →
/// compact `exec_truncated` breadcrumb (lengths + digests, no raw bytes).
///
/// Annotation overflow is normal for large-output execs, so it is silent — no
/// stderr warning to grep away. The final
/// breadcrumb is bounded by construction, so an oversized exec always leaves a
/// durable record instead of a silent hole. The exec's own JSON result and
/// exit code are untouched (spec §6).
fn write_exec_annotation(
    log_path: &std::path::Path,
    cmd: &[String],
    result: &ExecResult,
    run_id: &str,
    block_id: &str,
    event_id: Option<&str>,
    token: &str,
) -> std::io::Result<()> {
    use tendr::annotation;

    let hook_stdin = shell_words::join(cmd);
    let sentinel = format!("TENDR_EXEC_{token}");

    let with_event_id = |mut payload: serde_json::Value| {
        if let Some(id) = event_id {
            payload["event_id"] = serde_json::json!(id);
        }
        payload
    };

    let full_payload = |stdout: &str, stderr: &str, truncated: bool| {
        with_event_id(serde_json::json!({
            "source": "agent.exec",
            "event": "exec",
            "run_id": run_id,
            "block_id": block_id,
            "data": {
                "hook_stdin": &hook_stdin,
                "command": cmd,
                "hook_stdout": stdout,
                "hook_stderr": stderr,
                "hook_exit_code": result.exit_code,
                "cwd_after": &result.cwd_after,
                "sentinel": &sentinel,
                "timed_out": result.timed_out,
                "truncated": truncated,
            }
        }))
    };

    // 1. Full record.
    let payload = full_payload(&result.stdout, &result.stderr, result.truncated);
    if annotation::write_annotation_line(log_path, &payload)? {
        return Ok(());
    }

    // 2. Field-truncated record.
    let trunc_stdout = annotation::truncate_string(&result.stdout, annotation::MAX_FIELD_BYTES);
    let trunc_stderr = annotation::truncate_string(&result.stderr, annotation::MAX_FIELD_BYTES);
    let payload = full_payload(&trunc_stdout, &trunc_stderr, true);
    if annotation::write_annotation_line(log_path, &payload)? {
        return Ok(());
    }

    // 3. Compact breadcrumb: durable evidence the exec ran, without the
    //    oversized payload. Digests let a reader who still holds the output
    //    correlate it; lengths explain why the full record was dropped. Every
    //    field except `command` is fixed-size, and `cwd_after` is bounded here.
    let breadcrumb = |include_command: bool| {
        let mut data = serde_json::json!({
            "stdout_len": result.stdout.len(),
            "stderr_len": result.stderr.len(),
            "hook_exit_code": result.exit_code,
            "cwd_after": annotation::truncate_string(&result.cwd_after, 512),
            "sentinel": &sentinel,
            "timed_out": result.timed_out,
            "truncated": true,
            "stdout_sha256": sha256_hex(result.stdout.as_bytes()),
            "stderr_sha256": sha256_hex(result.stderr.as_bytes()),
        });
        if include_command {
            data["command"] = serde_json::json!(cmd);
        }
        with_event_id(serde_json::json!({
            "source": "agent.exec",
            "event": "exec_truncated",
            "run_id": run_id,
            "block_id": block_id,
            "data": data,
        }))
    };

    let payload = breadcrumb(true);
    if annotation::write_annotation_line(log_path, &payload)? {
        return Ok(());
    }

    // 4. Pathological: even the breadcrumb overflows (a giant command line).
    //    Drop the command; the remaining fields are fixed/bounded and always
    //    fit, so the exec is never a silent hole in the log.
    let payload = breadcrumb(false);
    annotation::write_annotation_line(log_path, &payload)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A session dir whose output.log holds exactly `lines` (tag, content).
    fn session_with_log(tmp: &tempfile::TempDir, lines: &[(&str, &str)]) -> SessionDir {
        use tendr::model::ids::{Namespace, SessionName};
        let root = SessionRoot::new(tmp.path().join("sessions"));
        let dir = session::create(
            &root,
            &Namespace::new("default").unwrap(),
            &SessionName::new("s").unwrap(),
        )
        .unwrap();
        let log: String = lines
            .iter()
            .map(|(tag, content)| {
                format!(
                    "{}\n",
                    serde_json::json!({"ts": 1.0, "tag": tag, "content": content})
                )
            })
            .collect();
        std::fs::write(dir.path().join("output.log"), log).unwrap();
        dir
    }

    /// Stderr logged after the stdout sentinel still belongs to the exec: the
    /// two pipes are logged by separate threads (#92). Before the stderr end
    /// marker, exec returned at the sentinel with `stderr == ""`.
    #[test]
    fn stderr_logged_after_the_sentinel_is_kept() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = session_with_log(
            &tmp,
            &[
                ("O", "__TENDR_EXEC__ tok 0 /tmp"),
                ("E", "late"),
                ("E", "__TENDR_EXEC_ERR__ tok"),
            ],
        );
        let result = wait_sentinel_result(&dir, "s", "tok", 0, None, true).unwrap();
        assert_eq!(result.stderr, "late");
        assert_eq!(result.exit_code, 0);
        assert!(!result.timed_out);
    }

    /// Unterminated last lines come back glued to both markers (#95).
    #[test]
    fn glued_residuals_are_kept_as_output() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = session_with_log(
            &tmp,
            &[
                ("E", "y__TENDR_EXEC_ERR__ tok"),
                ("O", "foo__TENDR_EXEC__ tok 3 /tmp"),
            ],
        );
        let result = wait_sentinel_result(&dir, "s", "tok", 0, None, true).unwrap();
        assert_eq!(result.stdout, "foo");
        assert_eq!(result.stderr, "y");
        assert_eq!(result.exit_code, 3);
    }

    fn pending(token: &str, log_offset: u64) -> PendingExec {
        PendingExec::new(&["sleep".to_owned(), "9".to_owned()], token, log_offset)
    }

    /// A shell frame is finished only once its own sentinel and stderr end
    /// marker are logged after its offset; another exec's markers don't count.
    #[test]
    fn shell_frame_finishes_on_its_own_markers() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = session_with_log(
            &tmp,
            &[
                ("O", "__TENDR_EXEC__ old 0 /tmp"),
                ("E", "__TENDR_EXEC_ERR__ old"),
                ("O", "__TENDR_EXEC__ other 0 /tmp"),
                ("E", "__TENDR_EXEC_ERR__ other"),
                ("E", "__TENDR_EXEC_ERR__ tok"),
            ],
        );
        let log_len = std::fs::metadata(dir.path().join("output.log"))
            .unwrap()
            .len();
        assert!(
            !pending("tok", 0)
                .is_finished(&dir, ExecTarget::PosixShell)
                .unwrap()
        );
        assert!(
            !pending("old", log_len)
                .is_finished(&dir, ExecTarget::PosixShell)
                .unwrap()
        );
        assert!(
            pending("old", 0)
                .is_finished(&dir, ExecTarget::PosixShell)
                .unwrap()
        );
        assert!(
            pending("other", 0)
                .is_finished(&dir, ExecTarget::DuckDb)
                .unwrap()
        );
    }

    /// A side-channel frame is finished once its result file exists. Clearing
    /// the record removes that file too.
    #[test]
    fn side_channel_frame_finishes_on_its_result_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = session_with_log(&tmp, &[]);
        let results = dir.path().join("exec-results");
        std::fs::create_dir_all(&results).unwrap();
        let p = pending("tok", 0);
        assert!(!p.is_finished(&dir, ExecTarget::PythonRepl).unwrap());

        std::fs::write(results.join("tok.json"), "{}").unwrap();
        assert!(p.is_finished(&dir, ExecTarget::PowerShell).unwrap());
        // Checking twice gives the same answer: the signal is not consumed.
        assert!(p.is_finished(&dir, ExecTarget::PythonRepl).unwrap());

        p.save(&dir).unwrap();
        PendingExec::clear(&dir, "tok");
        assert!(PendingExec::load(&dir).unwrap().is_none());
        assert!(!results.join("tok.json").exists());
    }

    /// The record round-trips through the session dir, and clearing it twice
    /// is harmless.
    #[test]
    fn pending_record_round_trips() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = session_with_log(&tmp, &[]);
        assert!(PendingExec::load(&dir).unwrap().is_none());
        pending("tok", 7).save(&dir).unwrap();
        let loaded = PendingExec::load(&dir).unwrap().unwrap();
        assert_eq!((loaded.token.as_str(), loaded.log_offset), ("tok", 7));
        assert!(loaded.describe().contains("tok") && loaded.describe().contains("sleep 9"));
        PendingExec::clear(&dir, "tok");
        PendingExec::clear(&dir, "tok");
        assert!(PendingExec::load(&dir).unwrap().is_none());
    }

    /// Independent SHA-256 hex reference (does not go through exec.rs's own helper).
    fn expected_sha256(s: &str) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(s.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn exec_result(stdout: String, stderr: String) -> ExecResult {
        ExecResult {
            session: "shell".to_owned(),
            stdout,
            stderr,
            exit_code: 0,
            cwd_after: "/tmp".to_owned(),
            timed_out: false,
            truncated: false,
            unmatched: None,
            request_id: None,
        }
    }

    /// The single `agent.exec` annotation line written to `log`, parsed.
    fn only_annotation(log: &Path) -> serde_json::Value {
        let content = std::fs::read_to_string(log).unwrap();
        let mut lines = content
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|line| line["tag"] == "A" && line["content"]["source"] == "agent.exec");
        let line = lines.next().expect("an agent.exec annotation line");
        assert!(lines.next().is_none(), "exactly one annotation line");
        line
    }

    /// Every stored line stays within the annotation size cap (incl. newline).
    fn assert_all_lines_fit(log: &Path) {
        let content = std::fs::read_to_string(log).unwrap();
        for line in content.lines() {
            // `lines()` strips the newline; the stored line includes it, so the
            // on-disk length is line.len() + 1 and must not exceed MAX_LINE.
            assert!(
                line.len() < tendr::annotation::MAX_LINE,
                "line of {} bytes exceeds MAX_LINE",
                line.len() + 1
            );
        }
    }

    /// Output so large that even the field-truncated annotation overflows the
    /// cap: the annotation degrades to a compact `exec_truncated` breadcrumb
    /// carrying lengths + digests instead of the raw bytes, and it fits.
    #[test]
    fn oversized_exec_writes_compact_breadcrumb() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("output.log");
        let stdout = "x".repeat(5000);
        let stderr = "y".repeat(5000);
        let result = exec_result(stdout.clone(), stderr.clone());

        write_exec_annotation(
            &log,
            &["cat".to_owned(), "big".to_owned()],
            &result,
            "run-1",
            "block-1",
            Some("evt-1"),
            "TOKEN",
        )
        .unwrap();

        let ann = only_annotation(&log);
        let content = &ann["content"];
        assert_eq!(content["event"], "exec_truncated");
        assert_eq!(content["run_id"], "run-1");
        assert_eq!(content["block_id"], "block-1");
        assert_eq!(content["event_id"], "evt-1");

        let data = &content["data"];
        assert_eq!(data["stdout_len"], 5000);
        assert_eq!(data["stderr_len"], 5000);
        assert_eq!(data["truncated"], true);
        assert_eq!(data["stdout_sha256"], expected_sha256(&stdout));
        assert_eq!(data["stderr_sha256"], expected_sha256(&stderr));
        // The raw payload must not leak back into the breadcrumb.
        assert!(data.get("hook_stdout").is_none());
        assert!(data.get("hook_stderr").is_none());

        assert_all_lines_fit(&log);
    }

    /// Normal-sized output is recorded verbatim in the full `exec` annotation —
    /// the degradation ladder must not touch the common path.
    #[test]
    fn small_exec_writes_full_annotation_untruncated() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("output.log");
        let result = exec_result("hello".to_owned(), String::new());

        write_exec_annotation(
            &log,
            &["echo".to_owned(), "hello".to_owned()],
            &result,
            "run-1",
            "block-1",
            Some("evt-1"),
            "TOKEN",
        )
        .unwrap();

        let ann = only_annotation(&log);
        let content = &ann["content"];
        assert_eq!(content["event"], "exec");
        assert_eq!(content["event_id"], "evt-1");
        let data = &content["data"];
        assert_eq!(data["hook_stdout"], "hello");
        assert_eq!(data["truncated"], false);
        assert_eq!(data["command"], serde_json::json!(["echo", "hello"]));
        assert_all_lines_fit(&log);
    }

    /// One oversized field, the other small: the full record overflows but the
    /// field-truncated retry fits, so we keep the rich `exec` annotation (with
    /// `truncated: true`) rather than dropping to a breadcrumb.
    #[test]
    fn large_stdout_truncates_but_keeps_full_exec_record() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("output.log");
        let result = exec_result("z".repeat(100_000), "small".to_owned());

        write_exec_annotation(
            &log,
            &["big".to_owned()],
            &result,
            "run-1",
            "block-1",
            None,
            "TOKEN",
        )
        .unwrap();

        let ann = only_annotation(&log);
        let content = &ann["content"];
        assert_eq!(content["event"], "exec");
        let data = &content["data"];
        assert_eq!(data["truncated"], true);
        let out = data["hook_stdout"].as_str().unwrap();
        assert!(out.len() <= tendr::annotation::MAX_FIELD_BYTES);
        assert!(out.starts_with("zzz"));
        assert_eq!(data["hook_stderr"], "small");
        assert_all_lines_fit(&log);
    }

    /// Pathological: the command line alone busts the cap. The breadcrumb drops
    /// the command but the exec is still recorded — never a silent hole.
    #[test]
    fn giant_command_still_leaves_a_breadcrumb() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("output.log");
        let result = exec_result("x".repeat(5000), "y".repeat(5000));

        write_exec_annotation(
            &log,
            &["a".repeat(6000)],
            &result,
            "run-1",
            "block-1",
            None,
            "TOKEN",
        )
        .unwrap();

        let ann = only_annotation(&log);
        let content = &ann["content"];
        assert_eq!(content["event"], "exec_truncated");
        let data = &content["data"];
        assert_eq!(data["stdout_len"], 5000);
        assert!(
            data.get("command").is_none(),
            "giant command dropped from breadcrumb"
        );
        assert!(data["stdout_sha256"].is_string());
        assert_all_lines_fit(&log);
    }
}
