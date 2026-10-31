---
id: ready-check
depends_on: []
---

# Report when a long-running child is ready

**Status:** not started. Needs a design decision on flag names and on what
`--after` should wait for.

A server started with `tendr start` is `Running` as soon as it spawns. Nothing
tells the caller when it is ready to use. Agents poll `tendr log` or `curl` in a
loop, each with their own timeout, and a server stuck before it listens looks
the same as a healthy one. `--after` waits for a dependency to exit, so it
cannot start a client once a server is ready, because a server never exits.

## Goal

`start` can be given one ready condition and a deadline:

- a regular expression on the child's output (`output.log` lines);
- a TCP port on the host accepting connections.

The sidecar checks the condition, so it works for detached and remote
(`--host`) sessions alike. When it holds, the sidecar records a `run.ready`
event and a ready time in `meta.json`. When the deadline passes first, the run
is stopped and ends with its own reason, distinct from `--timeout`.

Callers then wait for readiness with `tendr wait`, and a dependent session can
start on it with `--after`.

## Open questions

- Flag names. `watch --ready-file` and `events --ready-file` already use
  "ready" for the watcher's own baseline, not the child's state. Pick names
  that cannot be confused with them.
- Whether `start` blocks until ready, or returns at once and `wait` blocks.
- How `--after` chooses between "exited" and "ready" for a dependency.
- Whether a missed deadline stops the child or only records the failure.
- The interactive-agent-turns plan proposes a way for `events --follow` to
  exit on the first matching event. With a `run.ready` event, that would also
  be a way to wait for readiness. Decide whether `wait` needs its own option.
