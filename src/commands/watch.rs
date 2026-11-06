use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use tendr::events::{POLL_INTERVAL, merge_key, read_segment_records};
use tendr::log::LogLine;
use tendr::log_files::{LogCursor, LogReader};
use tendr::model::event::Event;
use tendr::model::ids::Namespace;
use tendr::session::{self, SessionRoot};

/// Per-session polling state.
struct SessionWatcher {
    namespace: String,
    session: String,
    run_id: String,
    log_cursor: LogCursor,
    /// Segment file name → offset after last consumed line.
    seg_offsets: BTreeMap<String, u64>,
}

/// Read new lifecycle events in deterministic merge order, advancing offsets.
fn read_new_run_events(
    session_dir: &Path,
    seg_offsets: &mut BTreeMap<String, u64>,
) -> anyhow::Result<Vec<Event>> {
    use anyhow::Context;

    let events_dir = session_dir.join("events");
    let read_dir = std::fs::read_dir(&events_dir)
        .with_context(|| format!("cannot read event history at {}", events_dir.display()))?;
    let mut segments: Vec<PathBuf> = read_dir
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("cannot list event history at {}", events_dir.display()))?;
    segments.retain(|p| p.extension().is_some_and(|x| x == "jsonl"));
    segments.sort();

    let mut events = Vec::new();
    for segment in segments {
        let Some(file_name) = segment.file_name().and_then(|f| f.to_str()) else {
            continue;
        };
        let from = seg_offsets.get(file_name).copied().unwrap_or(0);
        let outcome = read_segment_records(&segment, from)
            .with_context(|| format!("cannot read event history segment {}", segment.display()))?;
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
    Ok(events)
}

/// A replacement can remove or recreate the directory while history is read.
/// Callers reset offsets when discovering a new run; stale reads are not emitted.
fn checked_history(
    session_dir: &session::SessionDir,
    run_id: tendr::model::ids::RunId,
    offsets: &mut BTreeMap<String, u64>,
    require_history: bool,
    read: impl FnOnce(&Path, &mut BTreeMap<String, u64>) -> anyhow::Result<Vec<Event>>,
) -> anyhow::Result<Option<Vec<Event>>> {
    let history = read(session_dir.path(), offsets);
    let metadata = session::read_meta(session_dir);
    checked_history_outcome(session_dir, run_id, require_history, history, metadata)
}

/// Validate the captured read outcomes, not a later filesystem snapshot.
fn checked_history_outcome(
    session_dir: &session::SessionDir,
    run_id: tendr::model::ids::RunId,
    require_history: bool,
    history: anyhow::Result<Vec<Event>>,
    metadata: Result<tendr::model::meta::Meta, session::SessionError>,
) -> anyhow::Result<Option<Vec<Event>>> {
    match metadata {
        Ok(meta) if meta.run_id() != run_id => return Ok(None),
        Ok(_) => {}
        Err(session::SessionError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    }
    let history = history.map_err(|e| anyhow::anyhow!(
        "session {} has unusable event history: {e:#}; use --logs or --annotations to view output without lifecycle events",
        session_dir.name()
    ))?;
    if require_history && history.is_empty() {
        anyhow::bail!(
            "session {} has no usable event history; use --logs or --annotations to view output without lifecycle events",
            session_dir.name()
        );
    }
    Ok(Some(history))
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

    // Existing sessions get the last persisted lifecycle event as a snapshot;
    // sessions discovered later replay their history.
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
            if let Some(watcher) = watchers.get_mut(&key) {
                let replaced = watcher.run_id != run_id_str;
                let mut fresh_offsets = BTreeMap::new();
                let offsets = if replaced {
                    &mut fresh_offsets
                } else {
                    &mut watcher.seg_offsets
                };
                let history = if emit_events {
                    let Some(history) = checked_history(
                        &session_dir,
                        meta.run_id(),
                        offsets,
                        replaced,
                        read_new_run_events,
                    )?
                    else {
                        continue;
                    };
                    history
                } else {
                    Vec::new()
                };
                if replaced {
                    watcher.run_id = run_id_str;
                    watcher.log_cursor = LogCursor::default();
                    watcher.seg_offsets = fresh_offsets;
                }
                for event in &history {
                    if !emit_projected_run_event(&mut stdout, watcher, event) {
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
                    log_cursor: LogCursor::default(),
                    seg_offsets: BTreeMap::new(),
                };

                if emit_events {
                    // Even --from-now requires usable lifecycle history; consume
                    // it to establish offsets before suppressing the snapshot.
                    let Some(history) = checked_history(
                        &session_dir,
                        meta.run_id(),
                        &mut watcher.seg_offsets,
                        true,
                        read_new_run_events,
                    )?
                    else {
                        continue;
                    };
                    if !skip_initial {
                        if first_scan {
                            if !emit_projected_run_event(
                                &mut stdout,
                                &watcher,
                                history.last().unwrap(),
                            ) {
                                return Ok(());
                            }
                        } else {
                            for event in &history {
                                if !emit_projected_run_event(&mut stdout, &watcher, event) {
                                    return Ok(());
                                }
                            }
                        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;
    use tendr::events::{EventDraft, EventWriter};
    use tendr::model::event::Kind;
    use tendr::model::ids::{
        EpochTimestamp, Generation, ProcessIdentity, RunId, SessionName, Source,
    };
    use tendr::model::meta::Meta;
    use tendr::model::spec::LaunchSpec;

    fn fixture(root: &SessionRoot, name: &str) -> (session::SessionDir, Meta) {
        let ns = Namespace::new("default").unwrap();
        let name = SessionName::new(name).unwrap();
        let dir = session::create(root, &ns, &name).unwrap();
        let meta = Meta::new_starting(
            name,
            RunId::new(),
            Generation::first(),
            LaunchSpec::new(vec!["echo".into()]).unwrap(),
            ProcessIdentity {
                pid: NonZeroU32::new(100).unwrap(),
                start_time_ns: 1000,
            },
            EpochTimestamp::now(),
        );
        session::write_meta_atomic(&dir, &meta).unwrap();
        (dir, meta)
    }

    #[test]
    fn replacement_during_history_read_does_not_stop_other_sessions() {
        let temp = tempfile::tempdir().unwrap();
        let root = SessionRoot::new(temp.path().to_owned());
        let (swap, old) = fixture(&root, "swap");
        let (other, other_meta) = fixture(&root, "other");
        EventWriter::new(other.path())
            .append(
                EventDraft {
                    id: None,
                    kind: Kind::new("run.starting").unwrap(),
                    namespace: Namespace::new("default").unwrap(),
                    session: SessionName::new("other").unwrap(),
                    run_id: other_meta.run_id(),
                    generation: None,
                    source: Source::trusted("tendr.sidecar").unwrap(),
                    block_id: None,
                    parent_id: None,
                    data: None,
                    preview: None,
                },
                false,
            )
            .unwrap();

        let mut offsets = BTreeMap::new();
        let result = checked_history(&swap, old.run_id(), &mut offsets, true, |path, offsets| {
            // Deletion unpublishes metadata before recursively removing files.
            // Exercise that partial-removal window while the directory remains.
            std::fs::remove_file(path.join("meta.json")).unwrap();
            read_new_run_events(path, offsets)
        })
        .unwrap();
        assert!(result.is_none());

        // The missing-file result can be handled after replacement republishes.
        let history = read_new_run_events(swap.path(), &mut BTreeMap::new());
        let metadata = session::read_meta(&swap);
        assert!(metadata.is_err());
        std::fs::remove_dir_all(swap.path()).unwrap();
        let (_, replacement) = fixture(&root, "swap");
        assert_ne!(old.run_id(), replacement.run_id());
        let result = checked_history_outcome(&swap, old.run_id(), true, history, metadata).unwrap();
        assert!(result.is_none());
        let history = checked_history(
            &other,
            other_meta.run_id(),
            &mut offsets,
            true,
            read_new_run_events,
        )
        .unwrap()
        .unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].kind.as_str(), "run.starting");
    }

    #[test]
    fn changed_run_during_empty_read_is_retried_but_same_run_is_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let root = SessionRoot::new(temp.path().to_owned());
        let (dir, old) = fixture(&root, "swap");
        let mut offsets = BTreeMap::new();
        let result = checked_history(&dir, old.run_id(), &mut offsets, true, |path, _| {
            std::fs::remove_dir_all(path).unwrap();
            let _ = fixture(&root, "swap");
            Ok(Vec::new())
        })
        .unwrap();
        assert!(result.is_none());

        let current = session::read_meta(&dir).unwrap();
        let error = checked_history(&dir, current.run_id(), &mut offsets, true, |_, _| {
            Ok(Vec::new())
        })
        .unwrap_err();
        assert!(error.to_string().contains("event history"));
    }
}
