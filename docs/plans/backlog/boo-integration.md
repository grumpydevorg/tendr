---
id: boo-integration
depends_on: []
---

# Boo integration: Tendr supervises a Boo daemon

**Status:** the composition recipe is documented in `docs/guide.md` ("Tendr and
Boo"). One step remains: validate it live against a pinned Boo build.

[coder/boo](https://github.com/coder/boo) is a terminal multiplexer built on
libghostty-vt. An agent can drive a live TUI through it and read the rendered
screen. Tendr supervises processes and gives structured results, logs and
events. This plan composes the two without code in either project.

Screen reads inside Tendr (`tendr pty snapshot`, screen waits, an optional
`tendr-screen` extension) belong to
[cloud PTY control](../active/00_cloud-pty-control.md) slice 4. The reasons are
in [decisions](../../decisions.md).

## Boo facts this recipe relies on

Source-reviewed at Boo v0.6.4 (HEAD `39245a7`), not run live:

- One forked daemon per session, each owning one PTY child and an in-memory VT
  terminal (512 KiB scrollback). One Unix socket per session.
- Agent commands: `send --text/--key/--stdin`, `peek [--scrollback] --json`,
  `wait --text` and `wait --idle` (2 s of no output), `ls --json`.
- `peek --scrollback` silently truncates at the 1 MiB frame cap.
- No exit codes, no durability, no event log, no remote hosts, no Windows.
- Boo is 0.x and changes its CLI often. Pin a version before scripting it.

## Composition recipe

`BOO_FOREGROUND=1` skips Boo's daemon fork (`src/main.zig:338-350`), so the Boo
daemon can run as Tendr's supervised child:

```sh
tendr start claude-tui --namespace agents -- \
  env BOO_FOREGROUND=1 boo new claude -- claude
```

| Tendr owns | Boo owns |
|---|---|
| process lifecycle: `--replace`, `kill`, `wait`, `--timeout` | rendered terminal state |
| exit status, `meta.json`, `output.log` | `send`: keystroke input |
| events (`run.*`, `exec.*`, `hook.*`, `callback.finished`) | `peek`: read the rendered screen |
| `--host`, dependencies, `on_exit` | `wait --text` / `wait --idle` |

Observe the lifecycle through Tendr (session `agents/claude-tui`). Drive and
read the TUI through Boo (Boo session `claude`):

```sh
tendr status claude-tui --namespace agents
tendr wait   claude-tui --namespace agents
tendr events --session agents/claude-tui --follow

boo send claude --text "run tests"
boo wait claude --text "Done"
boo peek claude --json
```

Limitation: Tendr's `output.log` records Boo's own output, not the PTY bytes.
Those flow from the Boo daemon to its socket clients only.

Over SSH, `tendr --host <h> exec -- boo peek … --json` gives quoting-safe
remote screen reads.

## Remaining step

- [ ] Run the recipe end to end against a pinned Boo build: a real TUI session
  under `tendr start`, driven with `boo send`/`wait`/`peek`, then stopped with
  `tendr kill`. Record the Boo version, and fix the guide section if any command
  differs.
