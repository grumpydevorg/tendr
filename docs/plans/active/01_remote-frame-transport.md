---
id: remote-frame-transport
depends_on: []
---

# Remote frame transport: the attach bridge

**Status:** every `--host` operation travels as a frame on ssh stdin
([remote frame](../../architecture/06-transport-boundaries.md#the-remote-frame)),
and `attach` from a Unix client bridges the attach protocol over it
([attach bridge](../../architecture/06-transport-boundaries.md#the-attach-bridge)).
What is left is resuming viewers, read-only viewers, and moving Windows
clients off `ssh -t`. The cloud PTY workflow needs the first
([VM scripts and results](../../../scripts/pty-vm/README.md)).

Goal: a viewer that loses its connection can resume where it left off, and no
client needs a remote terminal or a rebuilt command line.

Windows-specific work outside the transport (ConPTY, the console frontend,
lifecycle hardening, PowerShell state) belongs to
[windows-parity](../backlog/windows-parity.md).

## Steps

1. **Resumable viewers.** A viewer reconnects with its run ID and the last
   sequence it consumed, and gets the retained contiguous suffix of the
   recording. History that is gone is an error with the available range (as
   `events --from-cursor` exits 44); it never skips forward. Input is never
   resent automatically. Today a viewer gets no sequence numbers
   ([PTY lane](../../architecture/04-pty-lane.md#output-and-viewers)). An
   earlier design for typed PTY operations (send, resize, claim, release,
   observe; reads retryable, bounded writes never retried automatically,
   finite default deadlines) is slice 3 of the cloud PTY plan at `83f83ab`.
2. **Read-only viewers.** Up to eight per run, admitted through a separate
   bounded pool for pending hellos, so a full viewer set never refuses a
   replacement controller. Today one pool of eight holds every connection,
   hellos included
   ([PTY lane](../../architecture/04-pty-lane.md#the-attach-handshake)).
3. **Delete the `ssh -t` path** and the plain-name rule for `attach`, once
   Windows clients can attach: today's `ssh -t` lets a Windows client attach
   to a Unix host with no Windows terminal code, and the bridge needs a local
   Windows console frontend
   ([windows-parity](../backlog/windows-parity.md#no-pty-or-attach)). Windows
   as the attach target needs ConPTY and a named-pipe carrier, also there.

## Acceptance

- On the VM (`scripts/pty-vm`), through the attach bridge from the laptop: a
  stalled viewer cannot delay a takeover (the fake-ssh test
  `host_takeover_while_the_first_client_is_stalled` covers it locally);
  reattach after a dropped connection resumes from the last consumed sequence
  with no gap; a full set of read-only viewers does not block a controller.
