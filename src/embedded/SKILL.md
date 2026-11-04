---
name: using-tendr
description: Use when an agent needs durable shells, REPLs, Python/IPython/DuckDB/PowerShell sessions, line-oriented JSON (NDJSON) processes, long-running commands, remote --host work, logs, watch/wait, or process state across tool calls. Not for ordinary one-shot file edits/searches.
---

# Using Tendr

Tendr keeps the process behind a tool call alive.

Before first use:
- `tendr exec` takes argv, not a shell string. Use `-- bash -c '...'` for multi-step shell logic.
- Check `exit_code` / process status, not just stdout text.
- Use one in-flight `exec` per session.
- A `--host` call that exits 82 with "did not reach the remote tendr" most likely never ran remotely (SSH auth stalled, e.g. a lapsed Tailscale SSH approval). Stop and get a human to approve or fix SSH; do not retry in a loop, and check `status` before re-running a `start`.
- To read what a PTY session showed, use `tendr replay --export asciicast <session dir>` (text events, no terminal needed); plain `tendr replay` needs a human at a real terminal. PTY recordings are kept until `tendr prune --recordings-older-than`/`--recordings-max` deletes them. `start --record-input` stores everything typed or pushed in clear, passwords and tokens included, and the export emits it as `i` events: do not use it for sessions that receive secrets.
- To drive an interactive agent (e.g. Claude Code) in a session, wait for its turn with one `tendr events --follow --max-count 1 --timeout <d>` on its hook events, not with `tendr wait` (that waits for the process to exit); see `tendr guide agent-turns`.
- For a server, give `start` a ready condition (`--ready-when-output <regex>` or `--ready-when-port <port>`, with `--ready-within <duration>`) and block on `tendr wait --ready <name>` (0 ready, 43 not ready) instead of polling `log` or `curl`; start its clients with `--after-ready <name>`.

For current, version-matched usage, run:

```bash
tendr guide
tendr guide install
tendr guide exec
tendr guide ready
tendr guide remote
tendr guide cgroup
tendr guide herdr
tendr guide agent-turns
tendr guide python
tendr guide duckdb
tendr guide powershell
tendr guide ndjson
tendr guide replay
tendr guide boundary
```
