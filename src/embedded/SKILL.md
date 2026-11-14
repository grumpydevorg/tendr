---
name: using-tendr
description: "Use for durable shells, REPLs, Python/IPython/DuckDB/PowerShell sessions, NDJSON processes, long-running commands, remote --host work, logs, watch/wait, and live process handoffs between agents or a human. Not for ordinary one-shot file edits/searches."
---

# Using Tendr

Tendr keeps the process behind a tool call alive.

Before first use:
- `tendr exec` takes argv, not a shell string. Use `-- bash -c '...'` for multi-step shell logic.
- Check `exit_code` / process status, not just stdout text.
- `tendr run <name> -- <command> [args]` streams supervised work and returns its exit code; `start` returns JSON immediately. Session identity and interpreter are explicit.
- Choose `--env-mode clean` to build the sidecar and worker environment from the platform baseline; `--pass-env NAME` opts a set variable in without persisting its value. Default inherit mode is unchanged. Names resolve on the executing host, including with `--host`; this needs frame version 3 remotely. Clean mode is not a sandbox or escape from OS containment; see `tendr guide environment`.
- Use one in-flight `exec` per session.
- A `--host` call that exits 82 with "did not reach the remote tendr" most likely never ran remotely (SSH auth stalled, e.g. a lapsed Tailscale SSH approval). Stop and get a human to approve or fix SSH; do not retry in a loop, and check `status` before re-running a `start`.
- To read what a PTY session showed, use `tendr export <session dir> --format asciicast` (text events, no terminal needed), and use an external player for viewing. Recordings remain until `tendr prune --recordings-older-than`/`--recordings-max` deletes them. `start --record-input` stores every typed or pushed byte in clear, including secrets, which export emits as `i` events.
- With Herdr Core (`hc`), hc hosts the interactive agent terminal, the harness owns task policy, and tendr is the primary supervisor of its work processes. Work launched inside hc can still be swept through inherited lifetime metadata when the terminal program ends, even after detachment; do not assume survival or strip that metadata implicitly. Independently retained work needs a separately established lifetime boundary. See `tendr guide herdr`; wait for application-defined events with `tendr guide events`, not an agent-turn driver in tendr.
- For a server, give `start` a ready condition (`--ready-when-output <regex>` or `--ready-when-port <port>`, with `--ready-within <duration>`) and block on `tendr wait --ready <name>` (0 ready, 43 not ready) instead of polling `log` or `curl`; start its clients with `--after-ready <name>`.
- Check `tendr log <name>` within a minute of starting a long job. Empty? Progress meters that redraw with carriage returns stay unlogged until a newline: see `tendr guide pty` (`--log-delimiter newline-or-cr`, and `pipe_reader` in `tendr status`).

## Hand off a running process

Prefer an existing named PTY session when authorized work needs human input or
continuation by another agent. Read `tendr guide handoff` for the full procedure.

- Give the recipient the host, namespace, session name, current task and next action.
  Process state survives; the agent's conversation and task authorization do not transfer automatically.
- A human runs `tendr attach <name>`, performs the requested step, then holds Control,
  taps `\`, releases both keys and presses `d`. This detaches without stopping the job.
  `Ctrl-C` reaches the running program and may cancel it.
- An attach does not replay old prompts. Arrange a fresh prompt or a harmless
  press-Enter gate, and explain that step before handing over. Never guess that an
  unseen password prompt is safe to type into.
- Wait for `status` to show `AgentControl` before resuming writes. Exit 81 while
  `HumanControl` is a control conflict; coordinate a detach rather than retrying or
  taking the terminal away. Designate one agent writer: `AgentControl` does not identify that agent.
- For secret entry, keep input recording off and use masked input. PTY output still
  goes to `output.log`, even with `--no-record`; capture secret output within the process.
  Verify the application result, release the temporary unlock and retire finished helpers.

For current, version-matched usage, run:

```bash
tendr guide
tendr guide install
tendr guide run
tendr guide handoff
tendr guide environment
tendr guide exec
tendr guide ready
tendr guide remote
tendr guide cgroup
tendr guide herdr
tendr guide events
tendr guide python
tendr guide duckdb
tendr guide powershell
tendr guide ndjson
tendr guide export
tendr guide boundary
tendr guide pty
```
