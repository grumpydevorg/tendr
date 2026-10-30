---
name: using-tendr
description: Use when an agent needs durable shells, REPLs, Python/IPython/DuckDB/PowerShell sessions, long-running commands, remote --host work, logs, watch/wait, or process state across tool calls. Not for ordinary one-shot file edits/searches.
---

# Using Tendr

Tendr keeps the process behind a tool call alive.

Before first use:
- `tendr exec` takes argv, not a shell string. Use `-- bash -c '...'` for multi-step shell logic.
- Check `exit_code` / process status, not just stdout text.
- Use one in-flight `exec` per session.
- A `--host` call that exits 82 never reached the remote host's tendr (SSH auth stalled, e.g. a lapsed Tailscale SSH approval). Stop and get a human to approve or fix SSH; do not retry in a loop.

For current, version-matched usage, run:

```bash
tendr guide
tendr guide install
tendr guide exec
tendr guide remote
tendr guide cgroup
tendr guide herdr
tendr guide python
tendr guide duckdb
tendr guide powershell
tendr guide boundary
```
