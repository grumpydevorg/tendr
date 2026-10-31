# Tendr Architecture

These files describe how the code on `main` works. The rules for judging a
change, and the reasons behind hard-to-reverse choices, are in
[../design.md](../design.md).

1. [01-system-context.md](01-system-context.md): the processes, who talks to them, and who owns what.
2. [02-session-storage.md](02-session-storage.md): every file under `~/.tendr/` and how long it lives.
3. [03-run-lifecycle.md](03-run-lifecycle.md): the run state machine, its writers, provenance, and crash recovery.
4. [04-pty-lane.md](04-pty-lane.md): PTY sessions: input control, the attach protocol and socket, and recording.
5. [05-key-flows.md](05-key-flows.md): the `start`, `exec`, `kill`, `attach`, `run` and `wrap` sequences.
6. [06-transport-boundaries.md](06-transport-boundaries.md): pipes, files, sockets, locks, SSH, and execution environments.
7. [07-event-protocol.md](07-event-protocol.md): the event log format, append rules, and the `events` and `emit` commands.
8. [08-pty-recording-format.md](08-pty-recording-format.md): the byte format of PTY recordings.
