use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tendr::events::{POLL_INTERVAL, merge_key, read_segment_records};
use tendr::log::LogLine;
use tendr::log_files::{LogCursor, LogReader};
use tendr::model::event::Event;
use tendr::model::ids::Namespace;
use tendr::model::state::{ExitReason, RunStatus};
use tendr::session::{self, SessionRoot};

/// Per-session polling state.
struct SessionWatcher {
    namespace: String,
    session: String,
    run_id: String,
    last_status: String,
    log_cursor: LogCursor,
    /// When the session has an `events/` dir, the run-event stream is
    /// derived from the event log (spec §5.3) — true timestamps,
    /// un-collapsed transitions, real sources. Sessions without one keep
    /// the legacy meta-diff synthesis. Output shape is frozen either way.
    event_mode: bool,
    /// Event-mode: segment file name → offset after last consumed line.
    seg_offsets: BTreeMap<String, u64>,
}

/// Return epoch seconds with microsecond precision as an f64.
fn now_epoch_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Map a RunStatus to its event name string.
fn run_event_name(status: &RunStatus) -> &'static str {
    match status {
        RunStatus::Starting => "run.starting",
        RunStatus::Running { .. } => "run.started",
        RunStatus::SpawnFailed { .. } => "run.spawn_failed",
        RunStatus::Exited { how, .. } => match how {
            ExitReason::ExitedOk => "run.exited",
            ExitReason::ExitedError { .. } => "run.exited",
            ExitReason::Killed => "run.killed",
            ExitReason::KilledForced => "run.killed",
            ExitReason::TimedOut => "run.timed_out",
            ExitReason::SidecarFailed { .. } => "run.sidecar_failed",
        },
        RunStatus::SidecarLost { .. } => "run.sidecar_lost",
        RunStatus::DependencyFailed { .. } => "run.dependency_failed",
    }
}

/// Build the data object for a run event.
fn run_event_data(status: &RunStatus) -> serde_json::Value {
    match status {
        RunStatus::Starting => serde_json::json!({"status": "Starting"}),
        RunStatus::Running { .. } => serde_json::json!({"status": "Running"}),
        RunStatus::SpawnFailed { .. } => serde_json::json!({"status": "SpawnFailed"}),
        RunStatus::Exited { how, .. } => match how {
            ExitReason::ExitedOk => {
                serde_json::json!({"status": "Exited", "reason": "ExitedOk", "exit_code": 0})
            }
            ExitReason::ExitedError { code } => {
                serde_json::json!({"status": "Exited", "reason": "ExitedError", "exit_code": code.get()})
            }
            ExitReason::Killed => {
                serde_json::json!({"status": "Exited", "reason": "Killed"})
            }
            ExitReason::KilledForced => {
                serde_json::json!({"status": "Exited", "reason": "KilledForced"})
            }
            ExitReason::TimedOut => {
                serde_json::json!({"status": "Exited", "reason": "TimedOut"})
            }
            ExitReason::SidecarFailed { step } => {
                serde_json::json!({"status": "Exited", "reason": "SidecarFailed", "step": step.as_str()})
            }
        },
        RunStatus::SidecarLost { .. } => {
            serde_json::json!({"status": "SidecarLost"})
        }
        RunStatus::DependencyFailed { reason, .. } => {
            use tendr::model::dep_fail::DepFailReason;
            let reason_str = match reason {
                DepFailReason::Failed => "Failed",
                DepFailReason::TimedOut => "TimedOut",
                DepFailReason::Killed => "Killed",
                DepFailReason::KilledForced => "KilledForced",
            };
            serde_json::json!({"status": "DependencyFailed", "reason": reason_str})
        }
    }
}

/// Serialize a status to a string for dedup comparison.
fn status_key(status: &RunStatus) -> String {
    // Serialize the status to JSON for a stable comparison key.
    serde_json::to_string(status).unwrap_or_default()
}

/// Read new `run.*` events from a session's segments, advancing per-segment
/// offsets. New (lexicographically later) segments are picked up from their
/// start. Returned in deterministic merge order (spec §4).
fn read_new_run_events(session_dir: &Path, seg_offsets: &mut BTreeMap<String, u64>) -> Vec<Event> {
    let events_dir = session_dir.join("events");
    let Ok(read_dir) = std::fs::read_dir(&events_dir) else {
        return Vec::new(); // wiped mid-replace — the next generation recreates it
    };
    let mut segments: Vec<PathBuf> = read_dir
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    segments.sort();

    let mut events = Vec::new();
    for segment in segments {
        let Some(file_name) = segment.file_name().and_then(|f| f.to_str()) else {
            continue;
        };
        let from = seg_offsets.get(file_name).copied().unwrap_or(0);
        let Ok(outcome) = read_segment_records(&segment, from) else {
            continue;
        };
        seg_offsets.insert(file_name.to_owned(), outcome.consumed_to);
        events.extend(
            outcome
                .records
                .into_iter()
                .map(|r| r.event)
                .filter(|e| e.kind.as_str().starts_with("run.")),
        );
    }
    events.sort_by_key(merge_key);
    events
}

/// Project one stored lifecycle event onto watch's frozen output shape:
/// f64 ts (the event's occurrence time), kind "run"/name split, the
/// event's real source, and the legacy data shape — the event log's
/// `provenance` field is stripped at projection (spec §5.3).
fn emit_projected_run_event(out: &mut impl Write, watcher: &SessionWatcher, event: &Event) -> bool {
    let ts = event.ts.epoch_micros() as f64 / 1e6;
    let mut data = event.data.clone().unwrap_or_else(|| serde_json::json!({}));
    if let Some(object) = data.as_object_mut() {
        object.remove("provenance");
    }
    emit_event(
        out,
        ts,
        &watcher.namespace,
        &watcher.session,
        &event.run_id.to_string(),
        event.source.as_str(),
        "run",
        event.kind.as_str(),
        data,
    )
}

/// Emit one NDJSON event line to stdout. Returns false if stdout is broken (pipe closed).
#[allow(clippy::too_many_arguments)]
fn emit_event(
    out: &mut impl Write,
    ts: f64,
    namespace: &str,
    session: &str,
    run_id: &str,
    source: &str,
    kind: &str,
    name: &str,
    data: serde_json::Value,
) -> bool {
    let event = serde_json::json!({
        "ts": ts,
        "namespace": namespace,
        "session": session,
        "run_id": run_id,
        "source": source,
        "kind": kind,
        "name": name,
        "data": data,
    });
    let line = serde_json::to_string(&event).expect("JSON serialization cannot fail");
    if writeln!(out, "{line}").is_err() {
        return false;
    }
    if out.flush().is_err() {
        return false;
    }
    true
}

pub fn cmd_watch(
    namespace: Option<&Namespace>,
    events: bool,
    logs: bool,
    annotations: bool,
    from_now: bool,
    ready_file: Option<PathBuf>,
) -> anyhow::Result<()> {
    let root = SessionRoot::default_path()?;

    // If no filter flags specified, emit events + logs (not annotations).
    // Annotations require explicit --annotations.
    let any_filter = events || logs || annotations;
    let emit_events = events || !any_filter;
    let emit_logs = logs || !any_filter;
    let emit_annotations = annotations;

    let mut watchers: HashMap<(String, String), SessionWatcher> = HashMap::new();
    let mut stdout = io::BufWriter::new(io::stdout().lock());

    // Record which sessions exist at watch invocation time.
    // --from-now skips initial state only for these, not for sessions discovered later.
    let initial_sessions: HashSet<(String, String)> = if from_now {
        session::list(&root, namespace)
            .unwrap_or_default()
            .iter()
            .map(|(ns, name)| (ns.as_str().to_owned(), name.as_str().to_owned()))
            .collect()
    } else {
        HashSet::new()
    };

    // Sessions found on the first scan existed at invocation: they get
    // watch's frozen current-state snapshot. Sessions discovered on later
    // scans are news — with an event log, their whole history replays
    // un-collapsed (spec §5.3).
    let mut first_scan = true;

    loop {
        // Discover sessions.
        let sessions = session::list(&root, namespace).unwrap_or_default();

        for (ns, name) in &sessions {
            let key = (ns.as_str().to_owned(), name.as_str().to_owned());

            // Try to open the session and read meta.
            let session_dir = match session::open(&root, ns, name) {
                Ok(Some(dir)) => dir,
                _ => continue,
            };
            let meta = match session::read_meta(&session_dir) {
                Ok(m) => m,
                Err(_) => continue,
            };

            let run_id_str = meta.run_id().to_string();
            let current_status_key = status_key(meta.status());
            let has_event_log = session_dir.path().join("events").is_dir();

            if let Some(watcher) = watchers.get_mut(&key) {
                // Detect run_id change (session replaced) — reset log offset
                if watcher.run_id != run_id_str {
                    watcher.run_id = run_id_str.clone();
                    watcher.log_cursor = LogCursor::default();
                    watcher.last_status = String::new(); // force status re-emit
                }

                // An events dir appearing mid-watch (a legacy session
                // replaced under a slice-2 binary): switch to the event
                // log — it carries the new generation from its start.
                if !watcher.event_mode && has_event_log {
                    watcher.event_mode = true;
                    watcher.seg_offsets = BTreeMap::new();
                }

                if watcher.event_mode {
                    if emit_events {
                        let events =
                            read_new_run_events(session_dir.path(), &mut watcher.seg_offsets);
                        watcher.last_status = current_status_key;
                        for event in &events {
                            if !emit_projected_run_event(&mut stdout, watcher, event) {
                                return Ok(());
                            }
                        }
                    }
                } else if emit_events && watcher.last_status != current_status_key {
                    // Legacy meta-diff synthesis, unchanged.
                    watcher.last_status = current_status_key;
                    let ts = now_epoch_secs();
                    if !emit_event(
                        &mut stdout,
                        ts,
                        &watcher.namespace,
                        &watcher.session,
                        &watcher.run_id,
                        "tendr.sidecar",
                        "run",
                        run_event_name(meta.status()),
                        run_event_data(meta.status()),
                    ) {
                        return Ok(());
                    }
                }
            } else {
                // New session discovered.
                let skip_initial = initial_sessions.contains(&key);

                let mut watcher = SessionWatcher {
                    namespace: ns.as_str().to_owned(),
                    session: name.as_str().to_owned(),
                    run_id: run_id_str,
                    last_status: current_status_key,
                    log_cursor: LogCursor::default(),
                    event_mode: has_event_log,
                    seg_offsets: BTreeMap::new(),
                };

                if watcher.event_mode {
                    // Consume the log up to now either way — offsets must
                    // sit past history so later polls stream only news.
                    let history = read_new_run_events(session_dir.path(), &mut watcher.seg_offsets);
                    if skip_initial || !emit_events {
                        // --from-now (or logs-only): history skipped.
                    } else if first_scan {
                        // Pre-existing session: the frozen snapshot
                        // contract — current state once, from the last
                        // transition (its true timestamp and source).
                        if let Some(event) = history.last() {
                            if !emit_projected_run_event(&mut stdout, &watcher, event) {
                                return Ok(());
                            }
                        } else {
                            // Events dir with no lifecycle events yet:
                            // legacy snapshot from meta.
                            let ts = now_epoch_secs();
                            if !emit_event(
                                &mut stdout,
                                ts,
                                &watcher.namespace,
                                &watcher.session,
                                &watcher.run_id,
                                "tendr.sidecar",
                                "run",
                                run_event_name(meta.status()),
                                run_event_data(meta.status()),
                            ) {
                                return Ok(());
                            }
                        }
                    } else {
                        // Discovered mid-watch: its whole history is news —
                        // every transition, un-collapsed.
                        for event in &history {
                            if !emit_projected_run_event(&mut stdout, &watcher, event) {
                                return Ok(());
                            }
                        }
                    }
                } else if !skip_initial && emit_events {
                    // Legacy: emit initial snapshot of current state.
                    let ts = now_epoch_secs();
                    if !emit_event(
                        &mut stdout,
                        ts,
                        &watcher.namespace,
                        &watcher.session,
                        &watcher.run_id,
                        "tendr.sidecar",
                        "run",
                        run_event_name(meta.status()),
                        run_event_data(meta.status()),
                    ) {
                        return Ok(());
                    }
                }

                watchers.insert(key.clone(), watcher);

                if skip_initial {
                    // Skip existing log content — start at its end.
                    if let Ok(end) = LogCursor::end(session_dir.path()) {
                        watchers.get_mut(&key).unwrap().log_cursor = end;
                    }
                }
            }

            // Read new log lines.
            if emit_logs || emit_annotations {
                let watcher = watchers.get_mut(&key).unwrap();
                let mut reader = LogReader::new(session_dir.path(), watcher.log_cursor);
                let mut raw = Vec::new();
                while let Ok(true) = reader.next_line(&mut raw) {
                    watcher.log_cursor = reader.cursor();
                    let Ok(parsed) = serde_json::from_slice::<LogLine>(&raw) else {
                        continue;
                    };
                    let ts_secs = parsed.ts;
                    match parsed.tag.as_str() {
                        "O" | "E" if emit_logs => {
                            let log_name = if parsed.tag == "O" {
                                "log.stdout"
                            } else {
                                "log.stderr"
                            };
                            if !emit_event(
                                &mut stdout,
                                ts_secs,
                                &watcher.namespace,
                                &watcher.session,
                                &watcher.run_id,
                                "tendr.sidecar",
                                "log",
                                log_name,
                                log_data(&parsed),
                            ) {
                                return Ok(());
                            }
                        }
                        "A" if emit_annotations && !parsed.content.is_null() => {
                            let ann = parsed.content.clone();
                            let source = ann["source"].as_str().unwrap_or("unknown").to_owned();
                            let event_name = ann["event"].as_str().unwrap_or("unknown").to_owned();
                            let name = format!("annotation.{event_name}");
                            if !emit_event(
                                &mut stdout,
                                ts_secs,
                                &watcher.namespace,
                                &watcher.session,
                                &watcher.run_id,
                                &source,
                                "annotation",
                                &name,
                                ann,
                            ) {
                                return Ok(());
                            }
                        }
                        _ => {} // skip if not emitting this type
                    }
                }
            }
        }

        // Prune watchers for sessions that no longer exist on disk
        let current_keys: HashSet<(String, String)> = sessions
            .iter()
            .map(|(ns, name)| (ns.as_str().to_owned(), name.as_str().to_owned()))
            .collect();
        watchers.retain(|key, _| current_keys.contains(key));

        if first_scan {
            // First full scan complete and initial snapshots flushed: publish
            // the out-of-band readiness signal (never on stdout) before
            // entering steady-state polling. Runs exactly once.
            if let Some(ready_path) = &ready_file {
                // A dead consumer stops the watch, same as emit_event's flush
                // handling; only signal readiness while someone is reading.
                if stdout.flush().is_err() {
                    return Ok(());
                }
                tendr::ready_file::create_ready_file(ready_path).map_err(|e| {
                    anyhow::anyhow!("failed to create ready-file {}: {e}", ready_path.display())
                })?;
            }
        }

        first_scan = false;
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// A `log.stdout`/`log.stderr` record's data: its content and, as in `events
/// --include-logs`, its `terminator` when it has one.
fn log_data(line: &LogLine) -> serde_json::Value {
    let mut data = serde_json::json!({"content": line.format_raw()});
    if let Some(terminator) = line.terminator {
        data["terminator"] =
            serde_json::to_value(terminator).expect("Terminator is always serializable");
    }
    data
}
