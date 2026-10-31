//! `tendr query` — event-log analytics v1.
//!
//! Points the external `duckdb` CLI at the on-disk JSONL event log: locate the
//! event segments in scope, register an `events` view over them, run the user's
//! SQL. Zero bespoke analytics code — DuckDB is the engine; tendr only locates
//! the segments and projects the envelope columns. Read-only over the shipped
//! log; no new write path.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};

use anyhow::Context;
use tendr::model::ids::Namespace;
use tendr::session::{self, SessionRoot};

/// DuckDB release this slice is developed and tested against. DuckDB's JSON
/// functions (`read_json` `records=false`, `->`/`->>`) are stable across 1.x;
/// surfaced by `tendr query --version`.
const TESTED_DUCKDB: &str = "1.x";

/// Options for `tendr query`, mirroring the clap subcommand.
pub struct QueryOptions {
    /// Inline SQL to run against the `events` view.
    pub sql: Option<String>,
    /// Read SQL from a file instead of the inline argument.
    pub file: Option<PathBuf>,
    /// Comma-separated namespaces to scope the view; `None` = all.
    pub namespace: Option<String>,
    /// Drop into a DuckDB shell with the view pre-registered.
    pub shell: bool,
    /// Print the DuckDB version and exit.
    pub version: bool,
}

/// The `events` view's projected columns: each JSONL line is read as one JSON
/// value (`records=false` → column `j`), then the envelope fields are pulled out
/// with typed casts. `ts` becomes a real TIMESTAMP; `data`/`data_ref` stay JSON
/// so `data->>'exit_code'` works and the open-vocabulary payload is not forced
/// into a wide mostly-NULL table. Two layers of tolerance keep one bad line from
/// killing the query: `TRY_CAST` turns a valid line with an unexpected field
/// value into a NULL in that column (not an error), and the reader itself skips
/// an unparseable or torn line (`ignore_errors` + `WHERE j IS NOT NULL` in
/// `build_view_sql`) so it is neither an error nor a phantom counted row.
/// Envelope contract: docs/architecture/07-event-protocol.md §1.
const VIEW_PROJECTION: &str = "\
    TRY_CAST(j->>'v' AS INT) AS v, \
    j->>'id' AS id, \
    TRY_CAST(j->>'ts' AS TIMESTAMP) AS ts, \
    j->>'kind' AS kind, \
    j->>'namespace' AS namespace, \
    j->>'session' AS session, \
    j->>'run_id' AS run_id, \
    TRY_CAST(j->>'gen' AS UBIGINT) AS gen, \
    j->>'writer' AS writer, \
    TRY_CAST(j->>'seq' AS UBIGINT) AS seq, \
    j->>'source' AS source, \
    j->>'block_id' AS block_id, \
    j->>'parent_id' AS parent_id, \
    j->'data' AS data, \
    j->'data_ref' AS data_ref";

pub fn cmd_query(opts: QueryOptions) -> anyhow::Result<()> {
    if opts.version {
        return report_version();
    }
    if !opts.shell && opts.sql.is_none() && opts.file.is_none() {
        anyhow::bail!(
            "nothing to run: provide SQL as an argument, or --file <path>, --shell, or --version"
        );
    }

    let root = SessionRoot::default_path()?;
    let namespaces = parse_namespaces(opts.namespace.as_deref())?;
    let segments = discover_segments(&root, &namespaces)?;
    let preamble = build_view_sql(&segments);

    if opts.shell {
        run_shell(&preamble)
    } else {
        let sql = resolve_sql(opts.sql, opts.file)?;
        run_query(&preamble, &sql)
    }
}

/// Report the DuckDB CLI version tendr will use, plus the tested-against range.
fn report_version() -> anyhow::Result<()> {
    let out = Command::new("duckdb")
        .arg("--version")
        .output()
        .map_err(|e| duckdb_spawn_error(&e))?;
    let version = String::from_utf8_lossy(&out.stdout);
    println!("DuckDB CLI: {}", version.trim());
    println!("tendr query is developed against DuckDB {TESTED_DUCKDB}");
    Ok(())
}

/// Split a `--namespace a,b,c` spec into namespaces. Empty parts are skipped;
/// `None` (flag omitted) means "all namespaces".
fn parse_namespaces(spec: Option<&str>) -> anyhow::Result<Vec<Namespace>> {
    let Some(spec) = spec else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        out.push(Namespace::new(part)?);
    }
    Ok(out)
}

/// Collect every `events/*.jsonl` segment path in scope, sorted. An empty
/// `namespaces` slice means all namespaces. Reuses `session::list` for the
/// same session discovery `tendr events`/`list` use.
fn discover_segments(root: &SessionRoot, namespaces: &[Namespace]) -> anyhow::Result<Vec<PathBuf>> {
    let sessions = if namespaces.is_empty() {
        session::list(root, None)?
    } else {
        let mut all = Vec::new();
        for ns in namespaces {
            all.extend(session::list(root, Some(ns))?);
        }
        all
    };

    let mut segments = Vec::new();
    for (ns, name) in sessions {
        let events_dir = root
            .path()
            .join(ns.as_str())
            .join(name.as_str())
            .join("events");
        if !events_dir.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&events_dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                segments.push(path);
            }
        }
    }
    segments.sort();
    Ok(segments)
}

/// Build the `CREATE VIEW events` preamble over the discovered segments. With no
/// segments in scope, define an empty view with the same column types so
/// `SELECT COUNT(*)` returns 0 rather than erroring on a no-file glob.
///
/// `ignore_errors=true` lets a torn or corrupt line come back as a NULL row
/// instead of aborting the read; `WHERE j IS NOT NULL` then drops those rows so
/// a bad line neither fails the query nor inflates a count. (Event envelopes are
/// always JSON objects, so a non-NULL `j` is exactly a well-formed event.)
fn build_view_sql(segments: &[PathBuf]) -> String {
    if segments.is_empty() {
        return format!(
            "CREATE VIEW events AS SELECT {VIEW_PROJECTION} \
             FROM (SELECT NULL::JSON AS j) WHERE false;"
        );
    }
    let list = segments
        .iter()
        .map(|p| sql_string_literal(&p.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "CREATE VIEW events AS SELECT {VIEW_PROJECTION} \
         FROM read_json([{list}], format='newline_delimited', records=false, \
         ignore_errors=true) t(j) \
         WHERE j IS NOT NULL;"
    )
}

/// Single-quoted SQL string literal, escaping embedded quotes by doubling.
/// Backslashes are literal in DuckDB single-quoted strings, so Windows paths
/// need no extra handling.
fn sql_string_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Resolve the SQL text from the inline argument or `--file`.
fn resolve_sql(sql: Option<String>, file: Option<PathBuf>) -> anyhow::Result<String> {
    match (sql, file) {
        (Some(s), None) => Ok(s),
        (None, Some(f)) => {
            std::fs::read_to_string(&f).with_context(|| format!("reading SQL file {}", f.display()))
        }
        // clap enforces `--file` conflicts with the positional SQL.
        (Some(_), Some(_)) => anyhow::bail!("provide either inline SQL or --file, not both"),
        (None, None) => anyhow::bail!("no SQL provided"),
    }
}

/// Run the preamble + user SQL through a one-shot `duckdb`, inheriting
/// stdout/stderr so the user sees DuckDB's native output. Propagates DuckDB's
/// exit code so a failed query fails `tendr query`.
fn run_query(preamble: &str, sql: &str) -> anyhow::Result<()> {
    let mut child = Command::new("duckdb")
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| duckdb_spawn_error(&e))?;

    let script = format!("{preamble}\n{sql}\n");
    {
        let mut stdin = child.stdin.take().expect("stdin was piped");
        stdin.write_all(script.as_bytes())?;
        // stdin dropped here → EOF, DuckDB runs then exits.
    }

    propagate_exit(child.wait()?)
}

/// Launch an interactive DuckDB shell with the `events` view pre-registered via
/// `-cmd`. Inherits all stdio for the REPL.
fn run_shell(preamble: &str) -> anyhow::Result<()> {
    let status = Command::new("duckdb")
        .arg("-cmd")
        .arg(preamble)
        .status()
        .map_err(|e| duckdb_spawn_error(&e))?;
    propagate_exit(status)
}

/// Mirror DuckDB's exit status as tendr's own: success → `Ok`, failure → exit
/// with DuckDB's code (the exit contract every other tendr verb follows).
fn propagate_exit(status: ExitStatus) -> anyhow::Result<()> {
    if status.success() {
        Ok(())
    } else {
        std::process::exit(status.code().unwrap_or(1));
    }
}

/// Turn a spawn failure into a clear, actionable error. A missing binary is the
/// common case: name the tool and how to fix it instead of leaking a bare
/// "No such file or directory".
fn duckdb_spawn_error(e: &std::io::Error) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        anyhow::anyhow!(
            "duckdb not found on PATH — `tendr query` requires the DuckDB CLI.\n\
             Install it from https://duckdb.org and ensure `duckdb` is on your PATH."
        )
    } else {
        anyhow::anyhow!("failed to run duckdb: {e}")
    }
}
