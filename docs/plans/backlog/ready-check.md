---
id: ready-check
depends_on: []
---

# Report when a long-running child is ready

**Status:** not started. The open questions below need answers before
implementation.

A server started with `tendr start` is `Running` as soon as it spawns. Nothing
tells the caller when it is ready to use. Agents poll `tendr log` or `curl` in a
loop, each with their own timeout, and a server stuck before it listens looks
the same as a healthy one. `--after` waits for a dependency to exit, so it
cannot start a client once a server is ready, because a server never exits.

## Goal

`start` can be given one ready condition and a deadline:

- a regular expression on the child's output (`output.log` lines);
- a TCP port on the host accepting connections.

The caller supplies the condition; tendr evaluates it and records the result.
Tendr does not decide what "ready" means for any program (design principle
[Mechanism Over Policy](../../design-principles.md#1-mechanism-over-policy)).

The sidecar checks the condition, so it works for detached and remote
(`--host`) sessions alike. It records a `run.ready` event and a ready time in
`meta.json` when the condition holds, or a missed-deadline event when the
deadline passes first.

## Open questions

- Flag names. `watch --ready-file` and `events --ready-file` already use
  "ready" for the watcher's own baseline, not the child's state. Pick names
  that cannot be confused with them.
- Whether a missed deadline stops the child, and with what end reason, or only
  records the miss.
- How callers wait: `start` blocks until ready, `wait` gets an option, or both.
  The [interactive agent turns](interactive-agent-turns.md) plan proposes a way
  for `events --follow` to exit on its first matching event, which would also
  wait for a `run.ready` event.
- How `--after` chooses between "exited" and "ready" for a dependency.
