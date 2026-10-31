---
id: sidecar-control-protocol
depends_on: []
---

# Sidecar control protocol

**Status:** unscheduled design. Start only when one of these is true:

- a feature needs portable correlated request/response over a boundary that
  files cannot serve cleanly;
- a required remote control flow cannot be expressed as a one-shot typed Tendr
  CLI frame over SSH stdio
  ([remote frame transport](../active/01_remote-frame-transport.md)).

Do not build it because the number of file-based IPC patterns grows. The trigger
is demand to cross a boundary.

Goal: portable, typed control IPC between the Tendr CLI and the per-session
sidecar. No current plan depends on it.

## The problem

Tendr's control IPC is local:

- `kill_request`: atomic file write; the sidecar polls for it and consumes it.
- pipe `push`: bytes on the session's stdin FIFO.
- `attach` and PTY `push`: framed messages on the session's Unix attach socket.

This works well on one host with a shared filesystem. Files are inspectable,
durable and survive a restart, and they need no connection state.

It does not carry over cleanly to:

- container boundaries without shared mounts;
- Windows named pipes instead of Unix FIFOs and sockets;
- features that need correlated request/response with typed errors.

## Target architecture: three layers

The control protocol does not replace Tendr's existing strengths. It adds a middle layer.

### Layer 1 — Durable truth (unchanged)

`meta.json` and `output.log` remain on disk. They are not IPC; they are crash recovery, observability, reconciliation substrate, and offline inspection. No protocol replaces them.

### Layer 2 — Control IPC (new)

A framed request/response/event protocol over a duplex stream. One protocol, multiple carriers:

- Unix: Unix domain socket (default)
- Windows: named pipe
- SSH: stdio bridge to the same API
- Container boundary: mounted socket or sidecar-local CLI bridge

### Layer 3 — Work IPC (unchanged)

Raw byte streams for stdin payloads, PTY terminal traffic, and the attach relay. These stay as dedicated channels. Do not multiplex control and work traffic onto one stream.

## Control envelope

```rust
#[derive(Debug, Serialize, Deserialize)]
pub enum ControlMsg {
    Request(ControlRequest),
    Response(ControlResponse),
    Event(ControlEvent),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ControlRequest {
    pub request_id: Uuid,
    pub method: Method,
    pub params: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ControlResponse {
    pub request_id: Uuid,
    pub result: ControlResult,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ControlResult {
    Ok { data: serde_json::Value },
    Err { code: String, message: String, details: Option<serde_json::Value> },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ControlEvent {
    pub event_type: String,
    pub data: serde_json::Value,
}
```

### Framing

Length-prefixed JSON over the duplex stream. Not newline-delimited — length-prefix is unambiguous over arbitrary transports (sockets, pipes, stdio). Four bytes big-endian length, then that many bytes of UTF-8 JSON.

### Transport trait

```rust
pub trait ControlTransport: Send {
    fn send(&mut self, msg: &ControlMsg) -> Result<()>;
    fn recv(&mut self) -> Result<ControlMsg>;
}
```

Backed by `UnixStream`, Windows named pipe, or an SSH stdio bridge. The trait is the seam; transport implementations are thin.

## Candidate methods

These are the operations that would migrate from file-based IPC to control RPC if and when the protocol is built.

| Method | Current IPC | Why it's a candidate |
|--------|-------------|---------------------|
| `request_kill` | `kill_request` file | Correlated request/response; currently fire-and-poll |
| `authorize_push` | stdin FIFO (pipe sessions) | Currently one-way |
| `watch_subscribe` | CLI polls `output.log` | Event stream is the natural fit for control RPC |
| `get_status` | CLI reads `meta.json` | Could be live rather than file-poll |
| `exec_begin` | CLI-side FIFO + lock + log scan | **The big one**: correlated exec over any carrier |

### What stays out

| Primitive | Reason |
|-----------|--------|
| `meta.json` | Durable truth, not live IPC. Must survive sidecar death. |
| `output.log` | Append-only audit trail. Must survive sidecar death. |
| PTY raw bytes | Work plane. Dedicated socket with its own framing. |
| stdin payloads | Work plane. Raw bytes after auth header. |
| Attach relay | Work plane. Already has its own socket + message types. |

## What this is not

- **Not a second lifecycle model.** The sidecar still owns lifecycle truth; the protocol is how clients talk to it, not a competing authority.
- **Not a container manager.** Boundary metadata (`--boundary`) describes where sessions run; the control protocol does not manage those environments.
- **Not a durability replacement.** `meta.json` and `output.log` survive sidecar death. RPC connections do not. The protocol augments file-based truth with live control; it does not replace it.
- **Not HTTP or gRPC.** Too much ceremony for sidecar-local control. Length-prefixed JSON over a duplex stream is sufficient.
- **Not a multiplexed stream.** Control, work and terminal traffic stay on separate channels.

## Relationship to other work

- Provenance (Direct vs Inferred) applies equally to file-based and RPC-based writes.
- Boundary metadata describes where sessions run; this protocol describes how clients talk to the sidecar.
- Exec annotations are written to `output.log` by the sidecar, whether or not `exec_begin` moves to this protocol.
- PTY input ownership already goes over the attach socket ([cloud PTY control](../active/00_cloud-pty-control.md)); it does not need this protocol.

## Design constraints

- The sidecar is currently sync-threaded, not async. The control socket listener would be another thread in the sidecar's existing poll loop, not a Tokio runtime. Keep this in mind for the transport implementation.
- The protocol must handle sidecar restart gracefully. Clients connecting to a dead socket should get a clear transport error, not a hang. The file-based `meta.json` remains the crash-recovery path.
- Multiple concurrent clients (e.g., two agents calling `get_status`) must be supported. The listener accepts multiple connections; each request is independently correlated by `request_id`.

## When to revisit

Re-evaluate this design when a start trigger at the top is met, or when the
file-based IPC grows beyond "files + FIFO + attach socket" into something hard
to reason about locally.
