# Event-log analytics recipes

`tendr query` runs SQL over the event log with the external
[DuckDB](https://duckdb.org) CLI. It registers an `events` view over the
on-disk JSONL files and never writes to them. Use `tendr query` for aggregate
analysis; use `tendr events` to stream or replay events.

You need the `duckdb` CLI on your `PATH`. Tendr is developed against DuckDB 1.x;
`tendr query --version` shows the version it will use.

## CLI

```sh
tendr query "SELECT COUNT(*) FROM events"      # inline SQL
tendr query --file analyses/failure-rate.sql   # SQL from a file
tendr query --namespace default,agents "<SQL>" # only these namespaces (default: all)
tendr query --shell                            # DuckDB shell with `events` registered
tendr query --version                          # DuckDB version in use
```

`tendr query` exits with DuckDB's exit code, so a failed query fails your
script or CI job. It is local-only: it does not accept `--host`.

## The `events` view

Each line of each event log file in scope is one row. The envelope fields
(see [the event protocol](architecture/07-event-protocol.md)) are typed
columns. The payload stays JSON; read it with `->` (JSON) or `->>` (text).

|Column|Type|Notes|
|---|---|---|
|`v`|INT|Envelope version (`1`)|
|`id`|VARCHAR|Event ID (UUIDv7)|
|`ts`|TIMESTAMP|Compare, use `BETWEEN` or cast `::DATE` directly|
|`kind`|VARCHAR|Dotted kind, e.g. `exec.result`, `run.exited`, `hook.post_tool_use`|
|`namespace`, `session`|VARCHAR|Where the event belongs|
|`run_id`|VARCHAR|Run ID (UUIDv7)|
|`gen`|UBIGINT|Generation, when known|
|`writer`, `seq`|VARCHAR, UBIGINT|Writing process and its sequence number|
|`source`|VARCHAR|Emitter, e.g. `tendr.sidecar`, `tendr.exec`, `claude.hook`|
|`block_id`|VARCHAR|Command block, like a trace span ID|
|`parent_id`|VARCHAR|Direct causal parent, like a parent span ID|
|`data`|JSON|Payload|
|`data_ref`|JSON|`{path,bytes,sha256,media_type}` of the full payload when `data` was truncated|

A bad line does not fail the query. A field with an unexpected type becomes
NULL (`TRY_CAST`). An unparseable or partly written line is skipped
(`ignore_errors`) and is not counted.

## Recipes

### 1. Event count by kind

```sql
SELECT kind, COUNT(*) AS n
FROM events
GROUP BY kind
ORDER BY n DESC;
```

### 2. exec failure rate by session, last 7 days

`data->>'exit_code'` is text; cast it to compare numbers.

```sql
SELECT session,
       COUNT(*) FILTER (WHERE (data->>'exit_code')::INT != 0) AS failures,
       COUNT(*)                                               AS total
FROM events
WHERE kind = 'exec.result'
  AND ts > now() - INTERVAL 7 DAY
GROUP BY session
ORDER BY failures DESC;
```

### 3. The 10 longest exec commands today

`exec.started` and `exec.result` share a `block_id`.

```sql
SELECT s.block_id,
       epoch_ms(r.ts) - epoch_ms(s.ts) AS dur_ms,
       s.data->>'command'              AS command
FROM events s
JOIN events r USING (block_id)
WHERE s.kind = 'exec.started'
  AND r.kind = 'exec.result'
  AND s.ts::DATE = current_date
ORDER BY dur_ms DESC
LIMIT 10;
```

### 4. Walk a causal chain to its root

Follow `parent_id` with a recursive CTE.

```sql
WITH RECURSIVE chain AS (
  SELECT * FROM events WHERE id = '<event-id>'
  UNION ALL
  SELECT e.* FROM events e JOIN chain c ON e.id = c.parent_id
)
SELECT id, parent_id, kind, data->>'command' AS command
FROM chain;
```

### 5. Agent tool calls in one session

For Claude Code hook events recorded with
`tendr wrap --event hook.post_tool_use`. `wrap` stores the hook's stdin JSON
under `data.hook_stdin`. Events from `tendr emit` carry whatever payload you
gave them, so adjust the path for those.

```sql
SELECT data->'hook_stdin'->>'tool_name' AS tool,
       COUNT(*)                         AS calls
FROM events
WHERE kind = 'hook.post_tool_use'
  AND session = '<name>'
GROUP BY tool
ORDER BY calls DESC;
```

### 6. Run outcomes by exit reason

A finished run writes one of four kinds: `run.exited` (`ExitedOk`,
`ExitedError`), `run.killed` (`Killed`, `KilledForced`), `run.timed_out`
(`TimedOut`) or `run.sidecar_failed` (`SidecarFailed`). Each holds the outcome
in `data->>'reason'`.

```sql
SELECT data->>'reason' AS reason,
       COUNT(*)         AS runs
FROM events
WHERE kind IN ('run.exited', 'run.killed', 'run.timed_out', 'run.sidecar_failed')
GROUP BY reason
ORDER BY runs DESC;
```

### 7. Runs by boundary

`run.starting` and `run.started` hold a fixed copy of the session's
`--boundary` in `data.boundary`. Group those events by
`data->'boundary'->'current'->>'kind'` or `->>'label'`. To group other events
by boundary, join them to their run's launch event on `run_id`. Do not use
`meta.json` for this: it holds only the current boundary.

```sql
SELECT data->'boundary'->'current'->>'kind'  AS boundary_kind,
       data->'boundary'->'current'->>'label' AS boundary_label,
       COUNT(*)                              AS runs
FROM events
WHERE kind = 'run.started'
GROUP BY boundary_kind, boundary_label
ORDER BY runs DESC;
```

Save a query you run often to a `.sql` file and run it with `--file`.
