# Transport Boundaries

Tendr’s architecture is easier to understand if you separate pure in-process logic from the places where bytes cross a boundary.

```mermaid
flowchart TD
    CLI["CLI process"] -->|anonymous ready pipe| Sidecar["sidecar process"]
    CLI -->|stdin.pipe FIFO / named pipe| Sidecar
    CLI -->|kill_request file| Sidecar
    CLI -->|meta.json read/write| Session["session dir"]
    CLI -->|output.log read| Session

    Sidecar -->|meta.json write| Session
    Sidecar -->|output.log append| Session
    Sidecar -->|child_pid / kill markers / breadcrumbs| Session

    Human["human terminal"] -->|Unix socket + attach protocol| Sidecar

    Remote["remote caller"] -->|ssh -T| RemoteCLI["remote tendr CLI"]
    Remote["remote attach"] -->|ssh -t| RemoteCLI
    RemoteCLI -->|same local boundaries on remote host| Session
    RemoteCLI -->|same local boundaries on remote host| Sidecar
```

Boundary inventory:

- Ready pipe:
  - created by `start`
  - used once for sidecar startup handshake
  - carries `OK:<meta>` or `ERROR:<message>`

- `stdin.pipe`:
  - created when `--stdin` is enabled
  - input transport for `push` to a pipe session (a PTY `push` uses the attach socket)
  - also used by `exec` to inject framed commands into running shell-like sessions

- `kill_request` file:
  - written by `kill`
  - consumed by the sidecar kill watcher
  - validated against current `run_id`

- `output.log`:
  - append-only JSONL from sidecar and wrapper writers
  - queried directly by `log`
  - projected into NDJSON events by `watch`

- PTY attach socket:
  - local Unix socket on the sidecar host
  - breadcrumbed through `a.sock.path`
  - framed with `MSG_DATA`, `MSG_RESIZE`, `MSG_DETACH`

- SSH transport:
  - forwards only an allowlisted subset of commands today
  - does not invent a separate event model or remote session store
  - connect deadline (`src/ssh.rs`): every call except `attach` runs the remote
    as `tendr --hello …`, whose first stderr line is `ssh::HELLO_LINE`. The
    local end reads stderr, drops the hello, and passes the rest through. ssh's
    `ConnectTimeout` ends at key exchange; authentication can then stall (a
    lapsed Tailscale SSH check-mode approval waits about 30 minutes, silently),
    so if the hello has not arrived within `TENDR_SSH_CONNECT_TIMEOUT` seconds
    (default 30) ssh gets SIGTERM, then SIGKILL a second later, and the call
    exits 82. After the hello there is no deadline, so `wait` and `log -f` run
    as long as they need. The marker comes from the remote tendr, not from
    ssh's log, which ControlMaster reuse and OpenSSH versions make unreliable.
    `attach` gets none: its stderr shares the terminal. `0` sends no `--hello`
    and leaves stderr inherited, which is how an older remote is reached.
    Stdout stays inherited while a thread relays stderr, so the order between
    the two streams is not exact; each stream's own order is kept

What stays in-process:

- `Meta` transition methods
- launch-spec hashing and idempotency checks
- watch event formatting once state/log lines have been read
- annotation payload construction before append

Current remote-command scope:

This split follows Theme 5: Separate Control Plane From Work Plane; see [../design-principles.md](../design-principles.md).

- supported over `--host`: `start`, `status`, `list`, `log`, `push`, `kill`, `wait`, `watch`, `attach`, `exec`
- local-only: `run`, `wrap`, `prune`, `query`, `guide`, `skill`, `emit`, `events` (everything not in the remote list above)

How `exec` goes remote: the whole exec request is serialized as one JSON frame and streamed to a remote `tendr exec --frame-from-stdin` over the ssh stdin channel. The shared-filesystem IPC that exec needs — the session's FIFO, `exec.lock`, `output.log` scan, and side-channel result files (`exec-results/<token>.json` for Python REPL) — all runs locally on the *remote* host exactly as it would for a local exec; only the request frame and the JSON result envelope cross the ssh boundary. That is a one-shot request/response, so no second lifecycle protocol is needed. (The earlier design treated `exec` as local-only for the IPC reasons above; the frame transport resolved it — see the remote-exec-host-parity plan.)

`run`, `wrap`, and `prune` stay local-only by design: `--host` on those exits 2 with a pre-filled `ssh host 'tendr <cmd>'` fallback. `wrap` additionally needs the supervised-process environment, so it only makes sense inside a tendr-supervised process on the target host.

## Execution-environment boundaries

Tendr's IPC is process-native and filesystem-local. That means every IPC primitive assumes a shared filesystem + same UID within a single trust domain. When sessions run inside containers, VMs, or across hosts, the question is not "does Tendr work?" — it's "where does the session dir live?"

Five axes define the boundary relationship between Tendr and any execution environment:

| Axis | Tendr's position |
|------|-------------------|
| **Identity** | `ProcessIdentity { pid, start_time_ns }` — process-native, no container/VM ID concept |
| **Lifecycle** | `RunStatus` state machine — process-native; container states like `paused`/`restarting` are out of scope |
| **Authority** | The kernel (process tables, signals) — not a container daemon |
| **Boundaries** | Implicit — whatever process tree is visible to the sidecar |
| **IPC** | Shared filesystem + local sockets — works inside any boundary that satisfies those assumptions |

The practical consequence:

- **Tendr inside a container** is the cleanest pattern for persistent shells/REPLs. Tendr owns the child directly; all IPC works normally.
- **Tendr outside, supervising `docker run ...`** works for foreground container jobs. Tendr supervises the `docker` client process, not the container object. Do not use with detached containers (`docker run -d`) — Tendr would supervise the short-lived client and lose the real workload.
- **Cross-container session sharing** violates *Theme 2: One authority per fact*. Shared volumes for artifacts are fine; shared volumes for multiple Tendr instances mutating the same session state are not.

Tendr does not model containers, VMs, or orchestrators as first-class objects. The `Boundary` metadata field (shipped 2026-07-10) *describes* where a session runs without *managing* that environment. See [../plans/completed/2026-07-10-boundary-metadata.md](../plans/completed/2026-07-10-boundary-metadata.md).
