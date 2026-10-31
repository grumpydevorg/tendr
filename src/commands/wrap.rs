use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tendr::events::{self, EventDraft, EventWriter};
use tendr::model::event::{Event, Kind, KindError, Uuid7};
use tendr::model::ids::{Namespace, RunId, SessionName, Source};
use tendr::platform::{Current, Platform};
use tendr::session::{self, SessionDir, SessionRoot};

/// Re-export shared constants for local use.
const MAX_FIELD_BYTES: usize = tendr::annotation::MAX_FIELD_BYTES;

const POLL_INTERVAL: Duration = Duration::from_millis(50);

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn cmd_wrap(
    session: &str,
    namespace: &Namespace,
    source: &Source,
    event: &str,
    cmd: Vec<String>,
) -> anyhow::Result<()> {
    // Argument validation before any side effect (spec §1/§6): a reserved
    // --event is a loud config error (exit 6, child never runs). A
    // grammar-invalid one (legacy dotless names like "pre-tool-use") keeps
    // the pre-slice-3 behavior — A-line only, no stored event, no env
    // chain — because annotations are free-form while stored events are
    // kinds.
    let kind = match Kind::new_user(event) {
        Ok(kind) => Some(kind),
        Err(KindError::ReservedPrefix(prefix)) => {
            eprintln!(
                "tendr wrap: --event '{event}' uses the reserved prefix '{prefix}' \
                 (tendr-owned schema)"
            );
            std::process::exit(6);
        }
        Err(_) => {
            eprintln!(
                "tendr wrap: --event '{event}' is not a valid event kind; \
                 writing annotation only (no stored event)"
            );
            None
        }
    };

    let run_id = std::env::var("TENDR_RUN_ID").map_err(|_| {
        anyhow::anyhow!("TENDR_RUN_ID not set — wrap must run inside a tendr-supervised process")
    })?;

    // Resolve session dir structurally, not from env
    let root = SessionRoot::default_path()?;
    let session_name = SessionName::new(session)?;
    let session_dir = session::open(&root, namespace, &session_name)?
        .ok_or_else(|| anyhow::anyhow!("session not found: {session}"))?;

    // Read all stdin
    let mut stdin_buf = Vec::new();
    io::stdin().read_to_end(&mut stdin_buf)?;

    // Install stop-notification handler
    STOP_REQUESTED.store(false, Ordering::SeqCst);
    install_stop_handler();

    // Spawn child via Platform trait
    if cmd.is_empty() {
        anyhow::bail!("no command specified");
    }

    // Pre-minted identity (spec §2): TENDR_PARENT_EVENT_ID names the event
    // wrap WILL write after the child exits, so hook-spawned emits chain to
    // it; TENDR_BLOCK_ID is wrap's own fresh block.
    let causality = kind
        .is_some()
        .then(|| (Uuid7::new(), Uuid7::new(), events::env_parent_chain()));

    let mut env = BTreeMap::new();
    if let Some((event_id, block_id, _)) = &causality {
        env.insert("TENDR_BLOCK_ID".to_owned(), block_id.to_string());
        env.insert("TENDR_PARENT_EVENT_ID".to_owned(), event_id.to_string());
    }
    let mut child = Current::spawn_child(&cmd, true, None, &BTreeSet::new(), &env)
        .map_err(|e| anyhow::anyhow!("failed to spawn '{}': {e}", cmd[0]))?;
    let kill_handle = Current::child_kill_handle(&child);

    // Pipe buffered stdin to child
    if let Some(mut child_stdin) = Current::child_stdin(&mut child) {
        let _ = child_stdin.write_all(&stdin_buf);
        // Drop closes the pipe — child sees EOF
    }

    // Wait for child, handling stop requests
    let output = wait_with_signal_handling(&mut child, &kill_handle);

    let (stdout_bytes, stderr_bytes, exit_code) = match output {
        Ok(out) => (out.stdout, out.stderr, out.status.code()),
        Err(e) => {
            eprintln!("tendr wrap: child wait failed: {e}");
            (Vec::new(), Vec::new(), None)
        }
    };

    // Replay captured output to caller
    let _ = io::stdout().write_all(&stdout_bytes);
    let _ = io::stderr().write_all(&stderr_bytes);

    // Dual-write (spec §0): the authoritative event first, then the A-line
    // projection linked to it by event_id. Both best-effort — the child's
    // exit code always passes through.
    let stored_event = match (&kind, &causality) {
        (Some(kind), Some((event_id, block_id, parent_id))) => append_wrap_event(
            &session_dir,
            namespace,
            &session_name,
            &run_id,
            kind.clone(),
            source,
            *event_id,
            *block_id,
            *parent_id,
            &stdin_buf,
            &stdout_bytes,
            &stderr_bytes,
            exit_code,
            &cmd,
        ),
        _ => None,
    };

    // Build and write annotation
    let mut payloads = build_annotation_payloads(
        source,
        event,
        &run_id,
        &stdin_buf,
        &stdout_bytes,
        &stderr_bytes,
        exit_code,
        &cmd,
    );
    if let Some((_, block_id, _)) = &causality {
        for payload in &mut payloads {
            payload["block_id"] = serde_json::json!(block_id.to_string());
            if let Some(event) = &stored_event {
                payload["event_id"] = serde_json::json!(event.id.to_string());
            }
        }
    }

    let log_path = session_dir.path().join("output.log");
    let mut wrote = false;
    for payload in payloads {
        match tendr::annotation::write_annotation_line(&log_path, &payload) {
            Ok(true) => {
                wrote = true;
                break;
            }
            Ok(false) => continue,
            Err(e) => {
                eprintln!("tendr wrap: failed to write annotation: {e}");
                wrote = true;
                break;
            }
        }
    }
    if !wrote {
        eprintln!("tendr wrap: annotation too large even after truncation, dropping");
    }

    // Exit with child's exit code
    std::process::exit(exit_code.unwrap_or(1));
}

/// Best-effort append of wrap's hook event with its pre-minted id — the
/// authoritative half of the dual-write (spec §0, data shape = spec
/// example (b)). Failures warn on stderr; the A-line and the child's exit
/// code are unaffected.
#[allow(clippy::too_many_arguments)]
fn append_wrap_event(
    session_dir: &SessionDir,
    namespace: &Namespace,
    session: &SessionName,
    run_id_env: &str,
    kind: Kind,
    source: &Source,
    event_id: Uuid7,
    block_id: Uuid7,
    parent_id: Option<Uuid7>,
    stdin_buf: &[u8],
    stdout_bytes: &[u8],
    stderr_bytes: &[u8],
    exit_code: Option<i32>,
    cmd: &[String],
) -> Option<Event> {
    let run_id_value = serde_json::Value::String(run_id_env.to_owned());
    let Ok(run_id) = serde_json::from_value::<RunId>(run_id_value) else {
        eprintln!("tendr wrap: TENDR_RUN_ID is not a valid run id; skipping stored event");
        return None;
    };
    let generation = std::env::var("TENDR_GENERATION")
        .ok()
        .and_then(|s| s.parse().ok());

    let data = serde_json::json!({
        "hook_stdin": try_parse_json_or_string(stdin_buf),
        "hook_stdout": try_parse_json_or_string(stdout_bytes),
        "hook_stderr": String::from_utf8_lossy(stderr_bytes).into_owned(),
        "hook_exit_code": exit_code,
        "command": cmd,
        "truncated": false,
    });
    let draft = EventDraft {
        id: Some(event_id),
        kind,
        namespace: namespace.clone(),
        session: session.clone(),
        run_id,
        generation,
        source: source.clone(),
        block_id: Some(block_id),
        parent_id,
        data: Some(data),
        preview: None,
    };
    match EventWriter::new(session_dir.path()).append(draft, false) {
        Ok(event) => Some(event),
        Err(e) => {
            eprintln!("tendr wrap: event append failed: {e}");
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_annotation_payloads(
    source: &Source,
    event: &str,
    run_id: &str,
    stdin_buf: &[u8],
    stdout_bytes: &[u8],
    stderr_bytes: &[u8],
    exit_code: Option<i32>,
    cmd: &[String],
) -> Vec<serde_json::Value> {
    let hook_stdin = try_parse_json_or_string(stdin_buf);
    let hook_stdout = try_parse_json_or_string(stdout_bytes);
    let hook_stderr = String::from_utf8_lossy(stderr_bytes).into_owned();

    let full_payload = serde_json::json!({
        "source": source.as_str(),
        "event": event,
        "run_id": run_id,
        "data": {
            "hook_stdin": hook_stdin,
            "hook_stdout": hook_stdout,
            "hook_stderr": hook_stderr,
            "hook_exit_code": exit_code,
            "command": cmd,
            "truncated": false,
        }
    });

    let truncated_stdin = truncate_field(&hook_stdin, MAX_FIELD_BYTES);
    let truncated_stdout = truncate_field(&hook_stdout, MAX_FIELD_BYTES);
    let truncated_stderr = truncate_string(&hook_stderr, MAX_FIELD_BYTES);

    let truncated_payload = serde_json::json!({
        "source": source.as_str(),
        "event": event,
        "run_id": run_id,
        "data": {
            "hook_stdin": truncated_stdin,
            "hook_stdout": truncated_stdout,
            "hook_stderr": truncated_stderr,
            "hook_exit_code": exit_code,
            "command": cmd,
            "truncated": true,
        }
    });

    let minimal_payload = serde_json::json!({
        "source": source.as_str(),
        "event": event,
        "run_id": run_id,
        "data": {
            "hook_stdin": serde_json::Value::Null,
            "hook_stdout": serde_json::Value::Null,
            "hook_stderr": "",
            "hook_exit_code": exit_code,
            "command": cmd,
            "truncated": true,
        }
    });

    vec![full_payload, truncated_payload, minimal_payload]
}

fn try_parse_json_or_string(bytes: &[u8]) -> serde_json::Value {
    if bytes.is_empty() {
        return serde_json::Value::Null;
    }
    let s = String::from_utf8_lossy(bytes);
    serde_json::from_str(&s).unwrap_or_else(|_| serde_json::Value::String(s.into_owned()))
}

fn truncate_field(val: &serde_json::Value, max_bytes: usize) -> serde_json::Value {
    match val {
        serde_json::Value::String(s) => serde_json::Value::String(truncate_string(s, max_bytes)),
        other => {
            let s = serde_json::to_string(other).unwrap_or_default();
            if s.len() <= max_bytes {
                other.clone()
            } else {
                serde_json::Value::String(truncate_string(&s, max_bytes))
            }
        }
    }
}

fn truncate_string(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    // Truncate at char boundary
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

#[cfg(unix)]
fn install_stop_handler() {
    // SAFETY: signal() is async-signal-safe. The handler only sets an AtomicBool.
    // Using raw libc here because rustix does not wrap signal-handler registration.
    unsafe {
        libc::signal(
            libc::SIGTERM,
            stop_signal_handler as *const () as libc::sighandler_t,
        );
    }
}

#[cfg(unix)]
extern "C" fn stop_signal_handler(_: libc::c_int) {
    STOP_REQUESTED.store(true, Ordering::SeqCst);
}

#[cfg(windows)]
fn install_stop_handler() {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

    unsafe extern "system" fn handler(_ctrl_type: u32) -> i32 {
        STOP_REQUESTED.store(true, Ordering::SeqCst);
        1 // TRUE — handled
    }

    // SAFETY: handler is a valid extern "system" function. We pass TRUE (1)
    // to add the handler.
    unsafe { SetConsoleCtrlHandler(Some(handler), 1) };
}

fn wait_with_signal_handling(
    child: &mut <Current as Platform>::SupervisedChild,
    kill_handle: &<Current as Platform>::ChildKillHandle,
) -> io::Result<std::process::Output> {
    // Collect stdout/stderr in threads so we don't deadlock
    let stdout = Current::child_stdout(child);
    let stderr = Current::child_stderr(child);

    let stdout_handle = std::thread::spawn(move || -> Vec<u8> {
        let mut buf = Vec::new();
        if let Some(mut r) = stdout {
            let _ = r.read_to_end(&mut buf);
        }
        buf
    });

    let stderr_handle = std::thread::spawn(move || -> Vec<u8> {
        let mut buf = Vec::new();
        if let Some(mut r) = stderr {
            let _ = r.read_to_end(&mut buf);
        }
        buf
    });

    // Uniform poll loop — Platform::kill_child handles graceful→force escalation
    let mut stop_forwarded = false;
    let status = loop {
        if let Some(status) = Current::child_try_wait(child)? {
            break status;
        }
        if STOP_REQUESTED.load(Ordering::SeqCst) && !stop_forwarded {
            Current::kill_child(kill_handle, false)?;
            stop_forwarded = true;
        }
        std::thread::sleep(POLL_INTERVAL);
    };

    let stdout_bytes = stdout_handle.join().unwrap_or_default();
    let stderr_bytes = stderr_handle.join().unwrap_or_default();

    Ok(std::process::Output {
        status,
        stdout: stdout_bytes,
        stderr: stderr_bytes,
    })
}
