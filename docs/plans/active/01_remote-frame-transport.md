---
id: remote-frame-transport
depends_on: []
---

# Remote frame transport: the attach bridge

**Status:** every `--host` operation but `attach` travels as a frame on ssh
stdin ([remote frame](../../architecture/06-transport-boundaries.md#the-remote-frame)).
What is left is moving `attach` onto the same channel. The cloud PTY workflow
needs it first ([VM scripts and results](../../../scripts/pty-vm/README.md)).

Goal: `attach --host` runs `ssh -T` to the constant remote entry point, so no
remote terminal and no rebuilt command line is needed, and a viewer that loses
its connection can resume where it left off. Until then `attach` stays on
`ssh -t` with plain names only
([transport boundaries](../../architecture/06-transport-boundaries.md#the-exit-trailer)).

Windows-specific work outside the transport (ConPTY, the console frontend,
lifecycle hardening, PowerShell state) belongs to
[windows-parity](../backlog/windows-parity.md).

## Steps

1. **Unix bridge.** `attach --host` sends an attach request in the remote
   frame, then the hello and typed PTY frames follow on the same stdin; the
   remote tendr connects them to the sidecar's attach socket. Raw mode, resize
   and the detach escape move to the laptop. The remote sidecar keeps
   controller epochs and the recording; the bridge has no lifecycle
   authority. Control frames (takeover, resize, detach) travel on a bounded
   path that never waits behind output, so a stalled viewer cannot delay a
   takeover. An earlier design for typed PTY operations (send, resize, claim,
   release, observe; reads retryable, bounded writes never retried
   automatically, finite default deadlines) is slice 3 of the cloud PTY plan
   at `83f83ab`.
2. **Resumable viewers.** A viewer reconnects with its run ID and the last
   sequence it consumed, and gets the retained contiguous suffix of the
   recording. History that is gone is an error with the available range (as
   `events --from-cursor` exits 44); it never skips forward. Input is never
   resent automatically. Today a viewer gets no sequence numbers
   ([PTY lane](../../architecture/04-pty-lane.md#output-and-viewers)).
3. **Read-only viewers.** Up to eight per run, admitted through a separate
   bounded pool for pending hellos, so a full viewer set never refuses a
   replacement controller. Today one pool of eight holds every connection,
   hellos included
   ([PTY lane](../../architecture/04-pty-lane.md#the-attach-handshake)).
4. **Delete the `ssh -t` path** and the plain-name rule for `attach`, once
   Windows clients can attach: today's `ssh -t` lets a Windows client attach
   to a Unix host with no Windows terminal code, and `ssh -T` needs a local
   Windows console frontend
   ([windows-parity](../backlog/windows-parity.md#no-pty-or-attach)). Windows
   as the attach target needs ConPTY and a named-pipe carrier, also there.

## Acceptance

- `attach --host` sends a constant ssh command line, checked by the same
  constant-argv test as the other operations.
- On the VM (`scripts/pty-vm`): reattach after a dropped connection resumes
  from the last consumed sequence with no gap; a stalled viewer cannot delay a
  takeover; a full set of read-only viewers does not block a controller.
